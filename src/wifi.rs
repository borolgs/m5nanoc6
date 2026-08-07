//! Wi-Fi station: sweeps the configured networks, brings up DHCP, and reconnects on its own.
//!
//! The first network that both associates and gets a lease wins.
//!
//! Where the station is is state, so it goes out on a [`Watch`]: readers get the current
//! answer, never a stale one. The sequence is not guaranteed — a `Connecting` immediately
//! followed by a `Failed` may present as only the `Failed` — so read it as a state, and never
//! count retries off it.

use alloc::string::String;
use core::{future::pending, net::Ipv4Addr};

use embassy_net::{Runner, Stack};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    watch::{Receiver, Watch},
};
use embassy_time::{Duration, Timer, with_timeout};
use esp_radio::wifi::{
    AuthenticationMethod, Config, Interface, Ssid, WifiController, WifiError, sta::StationConfig,
};

use crate::{
    config,
    led::{self, LedCmd, Rgb},
};

/// `app_task`, plus one spare.
const RECEIVERS: usize = 2;

static STATE: Watch<CriticalSectionRawMutex, WifiState, RECEIVERS> = Watch::new();

/// Follow the station's state. Panics past `RECEIVERS` — a boot-time failure in the module
/// that added the receiver.
pub fn subscribe() -> Receiver<'static, CriticalSectionRawMutex, WifiState, RECEIVERS> {
    STATE
        .receiver()
        .expect("too many Wi-Fi receivers: raise RECEIVERS")
}

/// Where the Wi-Fi station is in its connect cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiState {
    /// Sweeping the configured networks.
    Connecting,
    Connected {
        ssid: &'static str,
        ip: Ipv4Addr,
    },
    /// Was associated, lost the access point — a new sweep follows.
    Disconnected,
    /// The whole list was exhausted without a lease.
    Failed,
}

/// The configured entry a connect event belongs to, matched the way the driver stores an
/// SSID — as at most 32 bytes, so an over-long entry compares equal to what it truncated to.
fn configured_as(connected: Ssid) -> Option<&'static str> {
    config::config()
        .wifi_networks()
        .map(|(ssid, _)| ssid)
        .find(|&ssid| Ssid::from(ssid) == connected)
}

/// Socket slots the stack can hand out: one for DHCP, one for DNS, one for the Telegram
/// client's TCP connection, and one spare. Both DHCP and DNS go through `sockets.add(..)`
/// like any other socket, so each of them costs a slot.
pub const SOCKETS: usize = 4;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait after an exhausted sweep, and the ceiling it backs off to.
const RETRY_MIN: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// Give the radio — and the IP stack watching its link state — a moment to notice a drop
/// before re-associating, so the next `wait_config_up` can't succeed on the stale lease.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn wifi_task(mut controller: WifiController<'static>, stack: Stack<'static>) {
    let state = STATE.sender();

    if config::config().wifi_networks().next().is_none() {
        log::warn!("No Wi-Fi networks configured — set WIFI_CREDS in .env");
        state.send(WifiState::Failed);
        led::send(LedCmd::rgb(Rgb::RED));
        // Park instead of returning: dropping the controller would deinitialize the radio
        // underneath the interface `net_task` still holds.
        pending::<()>().await;
    }

    let mut backoff = RETRY_MIN;

    loop {
        state.send(WifiState::Connecting);
        led::send(LedCmd::status_blink_forever());

        let Some((ssid, ip)) = connect_any(&mut controller, &stack).await else {
            log::error!(
                "No configured Wi-Fi network reachable, retrying in {}s",
                backoff.as_secs()
            );
            state.send(WifiState::Failed);
            led::send(LedCmd::status(false));
            led::send(LedCmd::rgb(Rgb::RED));
            Timer::after(backoff).await;
            backoff = (backoff * 2).min(RETRY_MAX);
            continue;
        };

        backoff = RETRY_MIN;
        log::info!(
            "Wi-Fi connected: ssid={ssid} ip={ip} rssi={:?} channel={:?}",
            controller.rssi().ok(),
            controller.channel().ok().map(|(channel, _)| channel),
        );

        led::send(LedCmd::rgb(Rgb::OFF));
        led::send(LedCmd::status(false));
        state.send(WifiState::Connected { ssid, ip });

        if let Err(e) = controller.wait_for_disconnect_async().await {
            log::warn!("Waiting for the Wi-Fi disconnect event failed: {e:?}");
        }

        log::warn!("Wi-Fi disconnected from {ssid}, reconnecting");
        state.send(WifiState::Disconnected);
        led::send(LedCmd::status(false));
        Timer::after(RECONNECT_DELAY).await;
    }
}

/// One sweep of the configured networks, returning the one that took us all the way to an IP.
async fn connect_any(
    controller: &mut WifiController<'static>,
    stack: &Stack<'static>,
) -> Option<(&'static str, Ipv4Addr)> {
    for (ssid, password) in config::config().wifi_networks() {
        let mut config = StationConfig::default().with_ssid(ssid);
        config = if password.is_empty() {
            config.with_auth_method(AuthenticationMethod::None)
        } else {
            config.with_password(String::from(password))
        };

        if let Err(e) = controller.set_config(&Config::Station(config)) {
            log::warn!("Wi-Fi config for {ssid} rejected: {e:?}");
            continue;
        }

        log::info!("Connecting to {ssid}");
        // Which network we ended up on, which is not always the one just asked for: a
        // timed-out attempt is abandoned, not cancelled — `disconnect_async` no-ops while
        // the station is still associating, and `set_config` doesn't stop a radio already
        // in station mode. So the event this call wakes up on can belong to an earlier
        // network. Believe the event, not the loop variable.
        let associated = match with_timeout(CONNECT_TIMEOUT, controller.connect_async()).await {
            Ok(Ok(info)) => match configured_as(info.ssid) {
                Some(actual) => {
                    if actual != ssid {
                        log::warn!("Asked for {ssid}, associated with {actual} — keeping it");
                    }
                    Some(actual)
                }
                None => {
                    log::warn!(
                        "Associated with unconfigured network {}, dropping it",
                        info.ssid.as_str()
                    );
                    None
                }
            },
            Ok(Err(e)) => {
                log::warn!("Association with {ssid} failed: {e:?}");
                None
            }
            Err(_) => {
                log::warn!("Association with {ssid} timed out");
                None
            }
        };

        if let Some(actual) = associated {
            match with_timeout(DHCP_TIMEOUT, stack.wait_config_up()).await {
                // `wait_config_up` returns once *either* family is up, so a missing v4
                // config means an IPv6-only lease — not something we can use.
                Ok(()) => match stack.config_v4() {
                    Some(config) => return Some((actual, config.address.address())),
                    None => log::warn!("No IPv4 lease on {actual}"),
                },
                Err(_) => log::warn!("DHCP on {actual} timed out"),
            }
        }

        // Leave the radio idle before reconfiguring it for the next network.
        if let Err(e) = controller.disconnect_async().await
            && !matches!(e, WifiError::NotConnected)
        {
            log::debug!("Disconnect after a failed attempt on {ssid}: {e:?}");
        }
    }

    None
}
