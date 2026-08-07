//! Everything the build bakes in: tunables from `.cargo/config.toml`, secrets from `.env`.
//!
//! `build.rs` forwards every line of `.env` to the compiler, so both arrive as `option_env!`.
//! Nothing else in the crate reads the environment — the rest of it goes through [`config()`].

use core::str::FromStr;

use embassy_sync::once_lock::OnceLock;
use embassy_time::Duration;

/// The whole configuration, parsed once on first use.
pub fn config() -> &'static Config {
    CONFIG.get_or_init(Config::from_env)
}

pub struct Config {
    /// `ssid1,pass1;ssid2,pass2` — read through [`Config::wifi_networks`].
    wifi_creds: &'static str,
    /// `123456789:AAE…` from @BotFather. Empty when the notifier is not set up.
    pub telegram_token: &'static str,
    /// The one chat every message goes to. Negative for groups.
    pub telegram_chat_id: &'static str,
    /// Alarm below this, in °C.
    pub tg_min_c: f32,
    /// Hysteresis: the alarm clears above this, so a temperature on the threshold can't chatter.
    pub tg_clear_c: f32,
    /// How long a recovery has to hold before it clears the alarm. `None` clears at once.
    pub tg_dwell: Option<Duration>,
    /// Between repeats of a standing alarm.
    pub tg_repeat: Option<Duration>,
    /// Between silent heartbeats.
    pub tg_heartbeat: Option<Duration>,
}

impl Config {
    /// `ssid1,pass1;ssid2,pass2` in order; an entry without a comma is open, both halves trimmed.
    pub fn wifi_networks(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        self.wifi_creds
            .split(';')
            .map(|entry| match entry.split_once(',') {
                Some((ssid, password)) => (ssid.trim(), password.trim()),
                None => (entry.trim(), ""),
            })
            .filter(|(ssid, _)| !ssid.is_empty())
    }

    fn from_env() -> Self {
        let tg_min_c = number(option_env!("TG_MIN_C"), 10.0);

        Self {
            wifi_creds: option_env!("WIFI_CREDS").unwrap_or(""),
            telegram_token: option_env!("TELEGRAM_TOKEN").unwrap_or("").trim(),
            telegram_chat_id: option_env!("TELEGRAM_CHAT_ID").unwrap_or("").trim(),
            tg_min_c,
            tg_clear_c: tg_min_c + number(option_env!("TG_HYST_C"), 1.0),
            tg_dwell: minutes(option_env!("TG_DWELL_MIN"), 5),
            tg_repeat: minutes(option_env!("TG_REPEAT_MIN"), 60),
            tg_heartbeat: minutes(option_env!("TG_HEARTBEAT_MIN"), 360),
        }
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn number<T: FromStr>(value: Option<&str>, default: T) -> T {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

/// `0` disables the interval rather than turning it into a busy loop.
fn minutes(value: Option<&str>, default: u64) -> Option<Duration> {
    let minutes = number(value, default);
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}
