//! What the device does with what it hears: the alarm thresholds, the heartbeat schedule, the
//! wording of every message.
//!
//! [`App`] is the device's state, and `app_task` the only place it changes. Delivery is none of
//! its business: this task only drops a [`Notification`] in the outbox, so a send that parks for
//! half a minute cannot make it miss a reading.
//!
//! The outbox lives in `telegram.rs` because that is what consumes it, which is why this module
//! names the notifier it does. Move the outbox somewhere neutral if a second one ever appears.

use alloc::{format, string::String};
use core::future::pending;

use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};

use crate::{
    config,
    env_pro::{self, EnvData},
    telegram::{Notification, notify},
    wifi::{self, WifiState},
};

#[embassy_executor::task]
pub async fn app_task() {
    let config = config::config();
    let mut env = env_pro::subscribe();
    let mut wifi = wifi::subscribe();

    log::info!(
        "Alarm below {:.1}C, clears at {:.1}C",
        config.tg_min_c,
        config.tg_clear_c
    );

    let mut app = App::new(config);

    loop {
        let produced =
            match select3(env.changed(), wifi.changed(), sleep_until(app.deadline())).await {
                Either3::First(reading) => app.on_env(reading, Instant::now()),
                Either3::Second(state) => app.on_wifi(state, Instant::now()),
                Either3::Third(()) => app.on_timeout(Instant::now()),
            };

        if let Some(message) = produced {
            notify(message);
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => Timer::at(deadline).await,
        None => pending().await,
    }
}

/// Past this there is no reading — the ENV-Pro samples every few seconds.
const SENSOR_TIMEOUT: Duration = Duration::from_secs(300);

/// How often a heartbeat due with no link looks for one, instead of spending its interval.
const LINK_RETRY: Duration = Duration::from_secs(60);

struct App {
    config: &'static config::Config,
    boot: Instant,
    latest: Option<(EnvData, Instant)>,
    ssid: Option<&'static str>,
    announced: bool,
    cold: bool,
    last_flip: Option<Instant>,
    last_alarm: Instant,
    next_heartbeat: Option<Instant>,
}

impl App {
    fn new(config: &'static config::Config) -> Self {
        let boot = Instant::now();

        Self {
            config,
            boot,
            latest: None,
            ssid: None,
            announced: false,
            cold: false,
            last_flip: None,
            last_alarm: boot,
            next_heartbeat: config.tg_heartbeat.map(|every| boot + every),
        }
    }

    fn on_env(&mut self, env: EnvData, now: Instant) -> Option<Notification> {
        self.latest = Some((env, now));

        let crossed = if self.cold {
            env.temperature > self.config.tg_clear_c
        } else {
            env.temperature < self.config.tg_min_c
        };

        // Only a recovery dwells: delaying an alarm would lose a short excursion, not space it.
        let dwelling = self.cold
            && self
                .config
                .tg_dwell
                .zip(self.last_flip)
                .is_some_and(|(dwell, at)| now < at + dwell);

        if !crossed || dwelling {
            return None;
        }

        self.cold = !self.cold;
        self.last_flip = Some(now);
        self.last_alarm = now;

        let text = if self.cold {
            self.alarm_text(&env)
        } else {
            format!(
                "Recovered: {:.1}C, back above {:.1}C",
                env.temperature, self.config.tg_clear_c
            )
        };
        Some(self.speak(Notification::alarm(text), now))
    }

    /// One line on the first connection after boot, so a reboot loop shows up in the chat
    /// instead of hiding behind the heartbeat interval.
    fn on_wifi(&mut self, state: WifiState, now: Instant) -> Option<Notification> {
        let WifiState::Connected { ssid, .. } = state else {
            // Or the next heartbeat would name a network the device has left.
            self.ssid = None;
            return None;
        };

        self.ssid = Some(ssid);

        if self.announced {
            return None;
        }
        self.announced = true;

        let text = format!("{} started", env!("CARGO_PKG_NAME"));
        Some(self.speak(Notification::info(text), now))
    }

    /// When the next message comes due with no event to prompt it.
    fn deadline(&self) -> Option<Instant> {
        let repeat = match (self.cold, self.config.tg_repeat) {
            (true, Some(every)) => Some(self.last_alarm + every),
            _ => None,
        };

        [self.next_heartbeat, repeat].into_iter().flatten().min()
    }

    fn on_timeout(&mut self, now: Instant) -> Option<Notification> {
        if self.cold
            && let Some(every) = self.config.tg_repeat
            && now >= self.last_alarm + every
        {
            self.last_alarm = now;

            // No elapsed span: a loud message waits for the link and would read as current.
            let text = match self.reading(now) {
                Some(env) => self.alarm_text(&env),
                None => format!(
                    "ALARM: still below the {:.1}C threshold, and the sensor has gone quiet",
                    self.config.tg_min_c
                ),
            };
            return Some(self.speak(Notification::alarm(text), now));
        }

        if self.next_heartbeat.is_some_and(|due| now >= due) {
            // Proof of life only counts when it lands, and nothing lands without a link.
            if self.ssid.is_none() {
                self.next_heartbeat = Some(now + LINK_RETRY);
                return None;
            }

            let message = match self.reading(now) {
                Some(env) => Notification::perishable(self.heartbeat_text(&env, now)),
                // No reading means no alarm can fire, which is worth waking someone for.
                None => Notification::alarm(format!(
                    "ALARM: no sensor reading for over {} minutes",
                    SENSOR_TIMEOUT.as_secs() / 60
                )),
            };
            return Some(self.speak(message, now));
        }

        None
    }

    /// Every message is proof of life, so the heartbeat is due again from here.
    fn speak(&mut self, message: Notification, now: Instant) -> Notification {
        if let Some(every) = self.config.tg_heartbeat {
            self.next_heartbeat = Some(now + every);
        }
        message
    }

    fn reading(&self, now: Instant) -> Option<EnvData> {
        self.latest
            .filter(|(_, at)| now < *at + SENSOR_TIMEOUT)
            .map(|(env, _)| env)
    }

    fn alarm_text(&self, env: &EnvData) -> String {
        format!(
            "ALARM: {:.1}C is below the {:.1}C threshold ({:.0}%RH, {:.0}hPa)",
            env.temperature, self.config.tg_min_c, env.humidity, env.pressure
        )
    }

    fn heartbeat_text(&self, env: &EnvData, now: Instant) -> String {
        let wifi = self.ssid.map(|ssid| format!(", wifi {ssid}"));

        format!(
            "alive - {:.1}C, {:.0}%RH, {:.0}hPa, up {}{}",
            env.temperature,
            env.humidity,
            env.pressure,
            hhmm(now - self.boot),
            wifi.unwrap_or_default()
        )
    }
}

fn hhmm(span: Duration) -> String {
    let secs = span.as_secs();
    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
}
