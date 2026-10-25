//! The battery, from UPower's DisplayDevice (all batteries as one) over the system bus.
//! `watch` runs as a task of its own, so neither a slow bus nor a missing UPower can hold
//! up the login screen: until it says otherwise, no battery is shown.

use std::collections::HashMap;

use anyhow::{Context, bail};
use futures_lite::StreamExt;
use tokio::sync::mpsc;
use tracing::info;
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedValue;

const UPOWER: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE: InterfaceName =
    InterfaceName::from_static_str_unchecked("org.freedesktop.UPower.Device");

// UPower's device states.
const CHARGING: u32 = 1;
const FULLY_CHARGED: u32 = 4;
const PENDING_CHARGE: u32 = 5;

/// What the battery indicator shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    /// 0 to 100.
    pub percent: u8,
    /// On mains power (charging, or charged): the bolt.
    pub charging: bool,
}

/// The battery UPower describes, if it is `present`, at `percentage` and in `state`.
pub fn battery(present: bool, percentage: f64, state: u32) -> Option<Battery> {
    present.then(|| Battery {
        percent: percentage.clamp(0.0, 100.0).round() as u8,
        charging: matches!(state, CHARGING | FULLY_CHARGED | PENDING_CHARGE),
    })
}

/// Send the battery now, and again each time UPower says it changed, or starts or stops
/// (`None` while it is not running); until the greeter is gone, or with no system bus.
pub async fn watch(sender: mpsc::Sender<Option<Battery>>) {
    if let Err(e) = follow(&sender).await {
        info!("no battery from UPower: {e:#}");
        log(None);
        // The receiver is gone only if the greeter is.
        let _ = sender.send(None).await;
    }
}

async fn follow(sender: &mpsc::Sender<Option<Battery>>) -> anyhow::Result<()> {
    let bus = zbus::Connection::system()
        .await
        .context("connecting to the system bus")?;
    let properties = fdo::PropertiesProxy::builder(&bus)
        .destination(UPOWER)?
        .path(DISPLAY_DEVICE)?
        .build()
        .await?;
    // Listening before the first read, so that no change is missed between them.
    let mut changes = properties.receive_properties_changed().await?;
    let dbus = fdo::DBusProxy::new(&bus).await?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, UPOWER)])
        .await?;
    loop {
        let battery = match properties.get_all(DEVICE).await {
            Ok(all) => shown(&all),
            Err(e) => {
                info!("no battery from UPower: {e}");
                None
            }
        };
        log(battery);
        if sender.send(battery).await.is_err() {
            return Ok(());
        }
        tokio::select! {
            change = changes.next() => if change.is_none() { bail!("no more PropertiesChanged") },
            owner = owners.next() => if owner.is_none() { bail!("no more NameOwnerChanged") },
        }
    }
}

fn log(battery: Option<Battery>) {
    match battery {
        Some(Battery {
            percent,
            charging: true,
        }) => info!("battery: {percent}% charging"),
        Some(Battery { percent, .. }) => info!("battery: {percent}%"),
        None => info!("battery: none"),
    }
}

/// The battery in the DisplayDevice's properties `all`; a property missing is no battery.
fn shown(all: &HashMap<String, OwnedValue>) -> Option<Battery> {
    let get = |name: &str| all.get(name).map(|value| value.try_clone());
    let present = bool::try_from(get("IsPresent")?.ok()?).ok()?;
    let percentage = f64::try_from(get("Percentage")?.ok()?).ok()?;
    let state = u32::try_from(get("State")?.ok()?).ok()?;
    battery(present, percentage, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upowers_states_are_a_level_and_whether_on_mains() {
        assert_eq!(battery(false, 50.0, CHARGING), None);
        let shown = |percentage, state| battery(true, percentage, state).unwrap();
        assert_eq!(
            shown(49.6, 2),
            Battery {
                percent: 50,
                charging: false
            }
        );
        for state in [CHARGING, FULLY_CHARGED, PENDING_CHARGE] {
            assert!(shown(100.0, state).charging, "{state}");
        }
        // Unknown, discharging, empty, pending discharge.
        for state in [0, 2, 3, 6] {
            assert!(!shown(10.0, state).charging, "{state}");
        }
        assert_eq!(shown(120.0, 2).percent, 100);
        assert_eq!(shown(-1.0, 2).percent, 0);
    }

    #[test]
    fn the_battery_is_read_from_the_properties_and_none_without_one() {
        let all = |properties: &[(&str, zbus::zvariant::Value)]| -> HashMap<String, OwnedValue> {
            let owned = properties.iter().map(|(name, value)| {
                (
                    name.to_string(),
                    OwnedValue::try_from(value.clone()).unwrap(),
                )
            });
            owned.collect()
        };
        let present = [
            ("IsPresent", true.into()),
            ("Percentage", 42.0.into()),
            ("State", CHARGING.into()),
        ];
        let want = Battery {
            percent: 42,
            charging: true,
        };
        assert_eq!(shown(&all(&present)), Some(want));
        // A property missing, or of another type.
        assert_eq!(shown(&all(&present[..2])), None);
        assert_eq!(shown(&all(&[("IsPresent", 1u32.into())])), None);
    }
}
