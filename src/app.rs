//! What the device does with what it hears: the alarm thresholds, the heartbeat schedule, the
//! wording of every message.
//!
//! [`App`] is the device's state, and `app_task` the only place it changes. Getting a message
//! delivered belongs to [`crate::telegram`] — nothing here awaits the network, so a send that
//! parks for half a minute cannot make this task miss an event.
//!
//! The heartbeat is what makes silence in the chat mean something: an alarm-only device is
//! indistinguishable from a dead one. Its limit is real — a heartbeat proves liveness only
//! when it arrives, and nothing here can notice this device's own death. See
//! `docs/telegram-bot.md`.

use alloc::{format, string::String};
use core::future::pending;

use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};

use crate::{
    config,
    events::{EnvData, Event, Subscriber, WifiState},
    telegram::{self, Notification},
};

#[embassy_executor::task]
pub async fn app_task(mut events: Subscriber) {
    let config = config::config();

    log::info!(
        "Alarm below {:.1}C, clears at {:.1}C",
        config.tg_min_c,
        config.tg_clear_c
    );

    let mut app = App::new(config);

    loop {
        let produced = match select(events.next_message_pure(), sleep_until(app.deadline())).await {
            Either::First(Event::Env(env)) => app.on_env(env),
            Either::First(Event::Wifi(state)) => app.on_wifi(state),
            Either::First(other) => {
                log::debug!("[app] {other:?}");
                None
            }
            Either::Second(()) => app.on_timeout(Instant::now()),
        };

        if let Some(message) = produced {
            telegram::notify(message);
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => Timer::at(deadline).await,
        None => pending().await,
    }
}

/// Everything the device knows about itself, and what it decides to say about it.
struct App {
    config: &'static config::Config,
    boot: Instant,
    latest: Option<EnvData>,
    ssid: Option<&'static str>,
    cold: bool,
    last_alarm: Instant,
    next_heartbeat: Option<Instant>,
    announced: bool,
}

impl App {
    fn new(config: &'static config::Config) -> Self {
        let boot = Instant::now();

        Self {
            config,
            boot,
            latest: None,
            ssid: None,
            cold: false,
            last_alarm: boot,
            next_heartbeat: config.tg_heartbeat.map(|every| boot + every),
            announced: false,
        }
    }

    fn on_env(&mut self, env: EnvData) -> Option<Notification> {
        self.latest = Some(env);

        if !self.cold && env.temperature < self.config.tg_min_c {
            self.cold = true;
            self.last_alarm = Instant::now();
            return Some(Notification::loud(self.alarm_text(&env)));
        }

        if self.cold && env.temperature > self.config.tg_clear_c {
            self.cold = false;
            return Some(Notification::loud(format!(
                "Recovered: {:.1}C, back above {:.1}C",
                env.temperature, self.config.tg_clear_c
            )));
        }

        None
    }

    /// One line on the first connection after boot, so a reboot loop shows up in the chat
    /// instead of hiding behind the heartbeat interval.
    fn on_wifi(&mut self, state: WifiState) -> Option<Notification> {
        let WifiState::Connected { ssid, .. } = state else {
            return None;
        };

        self.ssid = Some(ssid);

        if self.announced {
            return None;
        }
        self.announced = true;

        Some(Notification::silent(format!(
            "{} started",
            env!("CARGO_PKG_NAME")
        )))
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
            return self
                .latest
                .map(|env| Notification::loud(self.alarm_text(&env)));
        }

        if let Some(every) = self.config.tg_heartbeat
            && self.next_heartbeat.is_some_and(|due| now >= due)
        {
            self.next_heartbeat = Some(now + every);
            return Some(Notification::silent(self.heartbeat_text()).no_retry());
        }

        None
    }

    fn alarm_text(&self, env: &EnvData) -> String {
        format!(
            "ALARM: {:.1}C is below the {:.1}C threshold ({:.0}%RH, {:.0}hPa)",
            env.temperature, self.config.tg_min_c, env.humidity, env.pressure
        )
    }

    fn heartbeat_text(&self) -> String {
        // Worth saying out loud: the device is up but the sensor is not.
        let reading = match self.latest {
            Some(env) => format!(
                "{:.1}C, {:.0}%RH, {:.0}hPa",
                env.temperature, env.humidity, env.pressure
            ),
            None => String::from("no sensor reading"),
        };
        let up = self.boot.elapsed().as_secs();
        let wifi = self.ssid.map(|ssid| format!(", wifi {ssid}"));

        format!(
            "alive - {reading}, up {}h{:02}m{}",
            up / 3600,
            (up % 3600) / 60,
            wifi.unwrap_or_default()
        )
    }
}
