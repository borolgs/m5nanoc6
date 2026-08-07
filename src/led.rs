//! On-board LEDs of the NanoC6: the blue status LED on GPIO7, a plain push-pull output, and
//! one WS2812 RGB LED on GPIO20 clocked out by the RMT peripheral, its supply gated by GPIO19.
//!
//! Both belong to [`led_task`], which plays the [`LedCmd`]s anyone drops in through [`send`].
//! Timed and finite-count effects restore the last steady value on their own, so callers can
//! fire and forget.
//!
//! `esp-hal-smartled` still pins `esp-hal ~1.0`, so the bit encoding lives here instead.

use core::future::pending;

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    Async,
    gpio::{Level, Output},
    rmt::{Channel, Error, PulseCode, Tx},
    time::Rate,
};

/// A failed Wi-Fi sweep fires three commands back to back; this is room for a few of those.
const DEPTH: usize = 8;

static COMMANDS: channel::Channel<CriticalSectionRawMutex, LedCmd, DEPTH> = channel::Channel::new();

/// Queue a command for [`led_task`].
///
/// Never blocks: the task awaits `Timer`s while playing a pattern, so it is a slow drainer by
/// design, and no producer should wait on an LED. A dropped command is cosmetic.
pub fn send(cmd: LedCmd) {
    if COMMANDS.try_send(cmd).is_err() {
        log::warn!("LED queue full, dropping {cmd:?}");
    }
}

/// What an LED should show, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedCmd {
    /// The blue status LED on GPIO7.
    Status(bool, Pattern),
    /// The WS2812 RGB LED on GPIO20.
    Rgb(Rgb, Pattern),
}

impl LedCmd {
    pub const fn status(on: bool) -> Self {
        Self::Status(on, Pattern::Solid)
    }

    pub const fn status_for(on: bool, duration: Duration) -> Self {
        Self::Status(on, Pattern::For(duration))
    }

    pub const fn status_blink(count: u16) -> Self {
        Self::Status(true, Pattern::blink(count))
    }

    /// Blink the status LED until the next command for it.
    pub const fn status_blink_forever() -> Self {
        Self::Status(true, Pattern::blink_forever())
    }

    pub const fn rgb(color: Rgb) -> Self {
        Self::Rgb(color, Pattern::Solid)
    }

    pub const fn rgb_for(color: Rgb, duration: Duration) -> Self {
        Self::Rgb(color, Pattern::For(duration))
    }

    pub const fn blink(color: Rgb, count: u16) -> Self {
        Self::Rgb(color, Pattern::blink(count))
    }

    pub const fn blink_forever(color: Rgb) -> Self {
        Self::Rgb(color, Pattern::blink_forever())
    }
}

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// The palette below is deliberately dim — the WS2812 at full scale is blinding.
    pub const LEVEL: u8 = 32;

    pub const OFF: Self = Self::new(0, 0, 0);
    pub const RED: Self = Self::new(Self::LEVEL, 0, 0);
    pub const GREEN: Self = Self::new(0, Self::LEVEL, 0);
    pub const BLUE: Self = Self::new(0, 0, Self::LEVEL);
    pub const YELLOW: Self = Self::new(Self::LEVEL, Self::LEVEL, 0);
    pub const WHITE: Self = Self::new(Self::LEVEL, Self::LEVEL, Self::LEVEL);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The same hue re-scaled so that a channel at [`Rgb::LEVEL`] ends up at `level`.
    ///
    /// Channels brighter than [`Rgb::LEVEL`] saturate at full scale rather than wrapping,
    /// so the hue can only wash out, never flip.
    pub const fn scaled(self, level: u8) -> Self {
        const fn scale(c: u8, level: u8) -> u8 {
            let scaled = (c as u32 * level as u32) / Rgb::LEVEL as u32;
            if scaled > u8::MAX as u32 {
                u8::MAX
            } else {
                scaled as u8
            }
        }

        Self::new(
            scale(self.r, level),
            scale(self.g, level),
            scale(self.b, level),
        )
    }
}

/// How long an LED holds the value it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// Hold until the next command. This becomes the LED's steady state.
    Solid,
    /// Hold for this long, then restore the steady state.
    For(Duration),
    /// Alternate between the value and off. `count: None` blinks until the next command;
    /// `Some(n)` plays `n` on-phases and then restores the steady state.
    Blink {
        on: Duration,
        off: Duration,
        count: Option<u16>,
    },
}

impl Pattern {
    pub const DEFAULT_ON: Duration = Duration::from_millis(150);
    pub const DEFAULT_OFF: Duration = Duration::from_millis(150);

    /// Blink `count` times at the default rate, then restore the steady state.
    /// `count: 0` is a no-op: the steady state is kept as-is.
    pub const fn blink(count: u16) -> Self {
        Self::Blink {
            on: Self::DEFAULT_ON,
            off: Self::DEFAULT_OFF,
            count: Some(count),
        }
    }

    /// Blink at the default rate until the next command for that LED.
    pub const fn blink_forever() -> Self {
        Self::Blink {
            on: Self::DEFAULT_ON,
            off: Self::DEFAULT_OFF,
            count: None,
        }
    }
}

/// RMT source clock the timings below assume: one tick = 12.5 ns.
pub const RMT_FREQUENCY: Rate = Rate::from_mhz(80);

// WS2812 bit timings, in RMT ticks.
const T0H: u16 = 32; // 0.40 µs
const T0L: u16 = 68; // 0.85 µs
const T1H: u16 = 64; // 0.80 µs
const T1L: u16 = 36; // 0.45 µs

const BITS: usize = 24;

/// The low period that latches a frame, in RMT ticks (2 × 300 µs). Sent inside the frame, so
/// that two `set` calls in quick succession cannot merge into one chain update.
const RESET: u16 = 24_000;

/// How long the WS2812 needs after its supply is switched on, before the first frame.
const POWER_UP: Duration = Duration::from_millis(1);

const RGB_RETRY: Duration = Duration::from_millis(20);

pub struct RgbLed {
    channel: Channel<'static, Async, Tx>,
    _power: Output<'static>,
    codes: [PulseCode; BITS + 2],
}

impl RgbLed {
    /// `power` must already be high — the LED needs a millisecond or so to come up.
    pub fn new(channel: Channel<'static, Async, Tx>, power: Output<'static>) -> Self {
        // Only the data codes are rewritten per frame; the reset and end marker stay put.
        let mut codes = [PulseCode::end_marker(); BITS + 2];
        codes[BITS] = PulseCode::new(Level::Low, RESET, Level::Low, RESET);

        Self {
            channel,
            _power: power,
            codes,
        }
    }

    pub async fn set(&mut self, color: Rgb) -> Result<(), Error> {
        // The WS2812 takes the channels in G, R, B order, most significant bit first.
        let grb = u32::from_be_bytes([0, color.g, color.r, color.b]);

        for (bit, code) in self.codes[..BITS].iter_mut().enumerate() {
            *code = if grb & (1 << (BITS - 1 - bit)) != 0 {
                PulseCode::new(Level::High, T1H, Level::Low, T1L)
            } else {
                PulseCode::new(Level::High, T0H, Level::Low, T0L)
            };
        }

        self.channel.transmit(&self.codes).await
    }
}

/// What an LED can be driven to: `bool` for the blue one, [`Rgb`] for the WS2812.
trait LedValue: Copy + PartialEq {
    const OFF: Self;
}

impl LedValue for bool {
    const OFF: Self = false;
}

impl LedValue for Rgb {
    const OFF: Self = Rgb::OFF;
}

/// One LED's playback state: what it shows now, and what it falls back to.
struct Animation<T> {
    /// The steady value, restored when a timed or finite-count pattern ends.
    base: T,
    /// What the running pattern shows during its on-phase.
    value: T,
    pattern: Pattern,
    on_phase: bool,
    /// On-phases left to play, `None` for an endless blink.
    left: Option<u16>,
    deadline: Option<Instant>,
    out: T,
    dirty: bool,
}

impl<T: LedValue> Animation<T> {
    const fn new() -> Self {
        Self {
            base: T::OFF,
            value: T::OFF,
            pattern: Pattern::Solid,
            on_phase: false,
            left: None,
            deadline: None,
            out: T::OFF,
            // Hardware state is unknown until the first flush.
            dirty: true,
        }
    }

    /// Start showing `value` with `pattern`, replacing whatever was running.
    fn apply(&mut self, value: T, pattern: Pattern, now: Instant) {
        self.value = value;
        self.pattern = pattern;

        match pattern {
            Pattern::Solid => {
                self.base = value;
                self.deadline = None;
            }
            Pattern::For(duration) => self.deadline = Some(now + duration),
            // Nothing to play — leave the steady value alone.
            Pattern::Blink { count: Some(0), .. } => {
                self.restore();
                return;
            }
            Pattern::Blink { on, count, .. } => {
                self.on_phase = true;
                self.left = count;
                self.deadline = Some(now + on);
            }
        }

        self.show(value);
    }

    /// Advance to the next phase if the current one has expired.
    fn poll(&mut self, now: Instant) {
        if !self.deadline.is_some_and(|deadline| deadline <= now) {
            return;
        }

        match self.pattern {
            // Solid never sets a deadline.
            Pattern::Solid | Pattern::For(_) => self.restore(),
            Pattern::Blink { off, .. } if self.on_phase => {
                self.on_phase = false;
                self.deadline = Some(now + off);
                self.show(T::OFF);
            }
            Pattern::Blink { on, .. } => {
                // The off-phase that just ended completes one blink.
                self.left = self.left.map(|left| left.saturating_sub(1));
                if self.left == Some(0) {
                    self.restore();
                } else {
                    self.on_phase = true;
                    self.deadline = Some(now + on);
                    self.show(self.value);
                }
            }
        }
    }

    fn restore(&mut self) {
        self.pattern = Pattern::Solid;
        self.deadline = None;
        self.show(self.base);
    }

    fn show(&mut self, value: T) {
        if self.out != value {
            self.out = value;
            self.dirty = true;
        }
    }

    /// The value to write out, or `None` if the hardware is already showing it.
    fn take_output(&mut self) -> Option<T> {
        self.dirty.then(|| {
            self.dirty = false;
            self.out
        })
    }

    /// Put back the value a failed write consumed — a `Solid` one would otherwise stay wrong
    /// until some later command happened to change it.
    fn retry_output(&mut self) {
        self.dirty = true;
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => Timer::at(deadline).await,
        None => pending().await,
    }
}

/// Owns both on-board LEDs and plays the effects requested through [`send`].
#[embassy_executor::task]
pub async fn led_task(mut status: Output<'static>, mut rgb: RgbLed) {
    let mut status_anim = Animation::<bool>::new();
    let mut rgb_anim = Animation::<Rgb>::new();
    let mut rgb_retry: Option<Instant> = None;

    // The supply was switched on in `main`; give the WS2812 time to come up.
    Timer::after(POWER_UP).await;

    loop {
        if let Some(on) = status_anim.take_output() {
            status.set_level(if on { Level::High } else { Level::Low });
        }
        if let Some(color) = rgb_anim.take_output() {
            rgb_retry = match rgb.set(color).await {
                Ok(()) => None,
                Err(e) => {
                    log::warn!("RGB LED update failed, retrying: {e:?}");
                    rgb_anim.retry_output();
                    Some(Instant::now() + RGB_RETRY)
                }
            };
        }

        let deadline = [status_anim.deadline, rgb_anim.deadline, rgb_retry]
            .into_iter()
            .flatten()
            .min();

        match select(COMMANDS.receive(), wait_until(deadline)).await {
            Either::First(cmd) => {
                let now = Instant::now();
                match cmd {
                    LedCmd::Status(on, pattern) => status_anim.apply(on, pattern, now),
                    LedCmd::Rgb(color, pattern) => rgb_anim.apply(color, pattern, now),
                }
            }
            Either::Second(()) => {
                let now = Instant::now();
                status_anim.poll(now);
                rgb_anim.poll(now);
            }
        }
    }
}
