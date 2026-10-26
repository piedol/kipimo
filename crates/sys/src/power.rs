//! The internal battery (IOPowerSources) and live system power draw.

use objc2_core_foundation::{CFDictionary, CFType};
use serde::Serialize;

use crate::cf::{self, Dict};
use crate::iokit::{
    self, IOPSCopyPowerSourcesInfo, IOPSCopyPowerSourcesList, IOPSGetPowerSourceDescription,
};
use crate::smc::Smc;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Battery {
    /// 0–100.
    pub percent: u8,
    pub on_ac: bool,
    pub charging: bool,
    /// Plugged in and full, or held below full by Optimized Battery Charging.
    pub charged: bool,
    /// Minutes left on battery; `None` while macOS is still estimating or when plugged in.
    pub minutes_to_empty: Option<u32>,
    /// Minutes to full while charging; `None` while estimating.
    pub minutes_to_full: Option<u32>,
    /// "Normal", or what macOS asks for ("Service Recommended").
    pub condition: Option<String>,
    pub cycle_count: Option<u32>,
    /// Full-charge capacity as a percentage of the design capacity.
    pub max_capacity_percent: Option<u8>,
}

fn minutes(v: Option<i64>) -> Option<u32> {
    v.filter(|&m| m > 0 && m < 60 * 72).map(|m| m as u32)
}

/// Build a `Battery` from one IOPowerSources description. Pure, so tests can feed it a plist.
pub(crate) fn battery_from(d: &Dict) -> Option<Battery> {
    if d.string("Type").as_deref() != Some("InternalBattery")
        || d.boolean("Is Present") == Some(false)
    {
        return None;
    }
    let cur = d.int("Current Capacity")?;
    let max = d.int("Max Capacity").filter(|&m| m > 0).unwrap_or(100);
    let on_ac = d.string("Power Source State").as_deref() == Some("AC Power");
    let charging = d.boolean("Is Charging").unwrap_or(false);
    Some(Battery {
        percent: (cur * 100 / max).clamp(0, 100) as u8,
        on_ac,
        charging,
        charged: d.boolean("Is Charged").unwrap_or(false) || (on_ac && !charging),
        minutes_to_empty: if on_ac {
            None
        } else {
            minutes(d.int("Time to Empty"))
        },
        minutes_to_full: if charging {
            minutes(d.int("Time to Full Charge"))
        } else {
            None
        },
        // `BatteryHealthCondition` is empty while the battery is fine (System
        // Information then says "Normal"); `BatteryHealth` can read "Check Battery"
        // at the same time, so it isn't used.
        condition: Some(
            d.string("BatteryHealthCondition")
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "Normal".into()),
        ),
        cycle_count: None,
        max_capacity_percent: None,
    })
}

/// Every power-source description in an IOPowerSources blob.
pub(crate) fn descriptions(blob: &CFType) -> Vec<Dict> {
    // SAFETY: `blob` is an IOPS info blob; the list is +1, each description +0.
    let Some(list) = (unsafe { cf::owned::<CFType>(IOPSCopyPowerSourcesList(cf::ptr(blob))) })
    else {
        return Vec::new();
    };
    cf::array(&list)
        .iter()
        .filter_map(|ps| unsafe {
            cf::borrowed::<CFDictionary>(IOPSGetPowerSourceDescription(
                cf::ptr(blob),
                cf::ptr(&**ps),
            ))
        })
        .map(Dict)
        .collect()
}

/// The internal battery, or `None` on a Mac without one.
pub fn battery() -> Option<Battery> {
    // SAFETY: the blob comes back +1.
    let blob = unsafe { cf::owned::<CFType>(IOPSCopyPowerSourcesInfo()) }?;
    let mut b = descriptions(&blob).iter().find_map(battery_from)?;
    if let Some(props) = iokit::service("AppleSmartBattery").and_then(|s| s.properties()) {
        b.cycle_count = props.int("CycleCount").map(|c| c as u32);
        let design = props.int("DesignCapacity").filter(|&d| d > 0);
        let full = props
            .int("NominalChargeCapacity")
            .or_else(|| props.int("AppleRawMaxCapacity"));
        if let (Some(d), Some(f)) = (design, full) {
            b.max_capacity_percent = Some(((f * 100 + d / 2) / d).clamp(0, 100) as u8);
        }
    }
    Some(b)
}

/// Whole-Mac power draw in watts: the SMC's `PSTR` sensor when it answers,
/// otherwise the battery gauge's system-load telemetry.
pub fn system_power_watts(smc: Option<&Smc>) -> Option<f32> {
    if let Some(w) = smc.and_then(Smc::system_power) {
        return Some(w);
    }
    let props = iokit::service("AppleSmartBattery")?.properties()?;
    let mw = props.dict("PowerTelemetryData")?.int("SystemLoad")?;
    let w = mw as f32 / 1000.0;
    (w > 0.0 && w < 2000.0).then_some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(body: &str) -> Dict {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>{body}</dict></plist>"#
        );
        Dict::from_value(&cf::plist(xml.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn battery_on_ac_held_at_80() {
        let d = dict(
            "<key>Type</key><string>InternalBattery</string>
             <key>Is Present</key><true/>
             <key>Current Capacity</key><integer>80</integer>
             <key>Max Capacity</key><integer>100</integer>
             <key>Power Source State</key><string>AC Power</string>
             <key>Is Charging</key><false/>
             <key>Time to Empty</key><integer>0</integer>
             <key>BatteryHealth</key><string>Check Battery</string>
             <key>BatteryHealthCondition</key><string></string>",
        );
        let b = battery_from(&d).unwrap();
        assert_eq!(b.percent, 80);
        assert!(b.on_ac && !b.charging && b.charged);
        assert_eq!(b.minutes_to_empty, None);
        assert_eq!(b.condition.as_deref(), Some("Normal"));
    }

    #[test]
    fn battery_discharging_with_estimate() {
        let d = dict(
            "<key>Type</key><string>InternalBattery</string>
             <key>Current Capacity</key><integer>41</integer>
             <key>Max Capacity</key><integer>100</integer>
             <key>Power Source State</key><string>Battery Power</string>
             <key>Is Charging</key><false/>
             <key>Time to Empty</key><integer>222</integer>
             <key>BatteryHealthCondition</key><string>Service Recommended</string>",
        );
        let b = battery_from(&d).unwrap();
        assert!(!b.on_ac && !b.charged);
        assert_eq!(b.condition.as_deref(), Some("Service Recommended"));
        assert_eq!(b.minutes_to_empty, Some(222));
    }

    #[test]
    fn battery_charging_still_estimating() {
        let d = dict(
            "<key>Type</key><string>InternalBattery</string>
             <key>Current Capacity</key><integer>20</integer>
             <key>Power Source State</key><string>AC Power</string>
             <key>Is Charging</key><true/>
             <key>Time to Full Charge</key><integer>-1</integer>",
        );
        let b = battery_from(&d).unwrap();
        assert!(b.charging && !b.charged);
        assert_eq!(b.minutes_to_full, None);
    }

    #[test]
    fn ups_is_not_the_battery() {
        let d = dict(
            "<key>Type</key><string>UPS</string><key>Current Capacity</key><integer>50</integer>",
        );
        assert!(battery_from(&d).is_none());
    }
}
