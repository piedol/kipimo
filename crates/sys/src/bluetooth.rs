//! Battery levels of connected Bluetooth devices.
//!
//! Two native sources, merged by name:
//! - IOPowerSources' accessory list (`IOPSCopyPowerSourcesByType`, what
//!   `pmset -g accps` shows). AirPods appear here with separate left, right
//!   and case parts. The function is exported by IOKit but not in the public
//!   headers, so it is looked up at run time and skipped if missing.
//! - The IORegistry's `AppleDeviceManagementHIDEventService` entries, which
//!   carry `BatteryPercent` for Magic Keyboard, Mouse and Trackpad.

use std::ffi::c_void;
use std::sync::OnceLock;

use objc2_core_foundation::CFType;
use serde::Serialize;

use crate::cf::{self, Dict};
use crate::iokit;
use crate::power::descriptions;

/// `kIOPSSourceForAccessories` in IOKit's private power-source types
/// (0 all, 1 internal, 2 UPS, 3 internal and UPS, 4 accessories; checked on macOS 26).
const SOURCE_FOR_ACCESSORIES: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PartKind {
    /// A single-battery device (keyboard, mouse, headphones).
    Main,
    Left,
    Right,
    Case,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BatteryPart {
    pub kind: PartKind,
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BtDevice {
    pub name: String,
    /// "Headset", "Keyboard", "Mouse", "Trackpad", "Gamepad"… (best effort).
    pub category: String,
    pub parts: Vec<BatteryPart>,
}

impl BtDevice {
    /// The lowest battery among the parts that aren't charging (for low-battery alerts).
    pub fn lowest(&self) -> Option<u8> {
        self.parts
            .iter()
            .filter(|p| !p.charging)
            .map(|p| p.percent)
            .min()
    }
}

fn part_kind(id: Option<&str>) -> PartKind {
    match id {
        Some("Left") => PartKind::Left,
        Some("Right") => PartKind::Right,
        Some("Case") => PartKind::Case,
        _ => PartKind::Main,
    }
}

fn part_from(d: &Dict) -> Option<BatteryPart> {
    let cur = d.int("Current Capacity")?;
    let max = d.int("Max Capacity").filter(|&m| m > 0).unwrap_or(100);
    Some(BatteryPart {
        kind: part_kind(d.string("Part Identifier").as_deref()),
        percent: (cur * 100 / max).clamp(0, 100) as u8,
        charging: d.boolean("Is Charging").unwrap_or(false),
    })
}

/// One accessory power source. AirPods list their buds and case under "Combined Parts".
pub(crate) fn device_from_power_source(d: &Dict) -> Option<BtDevice> {
    if d.string("Type").as_deref() != Some("Accessory Source")
        || d.boolean("Is Present") == Some(false)
    {
        return None;
    }
    let name = d.string("Name")?;
    let mut parts: Vec<BatteryPart> = d
        .array("Combined Parts")
        .iter()
        .filter_map(|p| Dict::from_value(p))
        .filter_map(|p| part_from(&p))
        .collect();
    if parts.is_empty() {
        let mut p = part_from(d)?;
        if p.kind != PartKind::Left && p.kind != PartKind::Right && p.kind != PartKind::Case {
            p.kind = PartKind::Main;
        }
        parts.push(p);
    }
    let order = |k: PartKind| match k {
        PartKind::Main => 0,
        PartKind::Left => 1,
        PartKind::Right => 2,
        PartKind::Case => 3,
    };
    parts.sort_by_key(|p| order(p.kind));
    Some(BtDevice {
        name,
        category: d.string("Accessory Category").unwrap_or_default(),
        parts,
    })
}

fn category_from_name(name: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.contains("keyboard") {
        "Keyboard"
    } else if n.contains("mouse") {
        "Mouse"
    } else if n.contains("trackpad") {
        "Trackpad"
    } else {
        ""
    }
}

/// A HID registry entry with a `BatteryPercent` (Magic Keyboard, Mouse, Trackpad).
pub(crate) fn device_from_hid(d: &Dict) -> Option<BtDevice> {
    let percent = d.int("BatteryPercent")?;
    let name = d.string("Product")?;
    // These entries carry no documented charging flag, so a HID device always
    // reads as "not charging"; IOPowerSources (merged first) knows better when it has the device.
    let charging = false;
    Some(BtDevice {
        category: category_from_name(&name).to_string(),
        name,
        parts: vec![BatteryPart {
            kind: PartKind::Main,
            percent: percent.clamp(0, 100) as u8,
            charging,
        }],
    })
}

type CopyByType = unsafe extern "C" fn(i32) -> *const c_void;

fn copy_by_type() -> Option<CopyByType> {
    static F: OnceLock<Option<CopyByType>> = OnceLock::new();
    *F.get_or_init(|| {
        // SAFETY: IOKit is linked; RTLD_DEFAULT searches loaded images.
        let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"IOPSCopyPowerSourcesByType".as_ptr()) };
        // SAFETY: the symbol has this signature in every macOS release that exports it.
        (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, CopyByType>(p) })
    })
}

/// Merge two device lists, keeping the first list's entry when names match.
pub(crate) fn merge(mut primary: Vec<BtDevice>, secondary: Vec<BtDevice>) -> Vec<BtDevice> {
    for d in secondary {
        if !primary.iter().any(|p| p.name.eq_ignore_ascii_case(&d.name)) {
            primary.push(d);
        }
    }
    primary
}

/// Connected Bluetooth devices that report a battery level.
pub fn devices() -> Vec<BtDevice> {
    let from_ps = copy_by_type()
        // SAFETY: the blob comes back +1.
        .and_then(|f| unsafe { cf::owned::<CFType>(f(SOURCE_FOR_ACCESSORIES)) })
        .map(|blob| {
            descriptions(&blob)
                .iter()
                .filter_map(device_from_power_source)
                .collect()
        })
        .unwrap_or_default();
    let from_hid = iokit::services("AppleDeviceManagementHIDEventService")
        .iter()
        .filter_map(|s| s.properties())
        .filter_map(|p| device_from_hid(&p))
        .collect();
    merge(from_ps, from_hid)
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

    /// Shaped like what this Mac reported for AirPods Pro on 2026-10-26.
    const AIRPODS: &str = "
        <key>Type</key><string>Accessory Source</string>
        <key>Name</key><string>Alex’s AirPods Pro</string>
        <key>Accessory Category</key><string>Headset</string>
        <key>Is Present</key><true/>
        <key>Part Identifier</key><string>Combined</string>
        <key>Current Capacity</key><integer>73</integer>
        <key>Max Capacity</key><integer>100</integer>
        <key>Combined Parts</key><array>
          <dict><key>Part Identifier</key><string>Right</string><key>Current Capacity</key><integer>74</integer>
                <key>Max Capacity</key><integer>100</integer><key>Is Charging</key><false/></dict>
          <dict><key>Part Identifier</key><string>Left</string><key>Current Capacity</key><integer>73</integer>
                <key>Max Capacity</key><integer>100</integer><key>Is Charging</key><false/></dict>
          <dict><key>Part Identifier</key><string>Case</string><key>Current Capacity</key><integer>12</integer>
                <key>Max Capacity</key><integer>100</integer><key>Is Charging</key><true/></dict>
        </array>";

    #[test]
    fn airpods_parts_in_order() {
        let d = device_from_power_source(&dict(AIRPODS)).unwrap();
        assert_eq!(d.name, "Alex’s AirPods Pro");
        assert_eq!(d.category, "Headset");
        let kinds: Vec<_> = d.parts.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![PartKind::Left, PartKind::Right, PartKind::Case]);
        assert_eq!(d.parts[0].percent, 73);
        // The charging case doesn't count as low.
        assert_eq!(d.lowest(), Some(73));
    }

    #[test]
    fn single_part_accessory() {
        let d = device_from_power_source(&dict(
            "<key>Type</key><string>Accessory Source</string><key>Name</key><string>Beats Flex</string>
             <key>Current Capacity</key><integer>15</integer>",
        ))
        .unwrap();
        assert_eq!(
            d.parts,
            vec![BatteryPart {
                kind: PartKind::Main,
                percent: 15,
                charging: false
            }]
        );
        assert_eq!(d.lowest(), Some(15));
    }

    #[test]
    fn internal_battery_is_not_an_accessory() {
        assert!(device_from_power_source(&dict("<key>Type</key><string>InternalBattery</string><key>Name</key><string>x</string><key>Current Capacity</key><integer>1</integer>")).is_none());
    }

    #[test]
    fn hid_keyboard() {
        let d = device_from_hid(&dict(
            "<key>Product</key><string>Magic Keyboard with Touch ID</string><key>BatteryPercent</key><integer>18</integer>
             <key>BatteryStatusFlags</key><integer>0</integer>",
        ))
        .unwrap();
        assert_eq!(d.category, "Keyboard");
        assert_eq!(d.lowest(), Some(18));
        // The built-in keyboard has no BatteryPercent and is left out.
        assert!(
            device_from_hid(&dict(
                "<key>Product</key><string>Apple Internal Keyboard / Trackpad</string>"
            ))
            .is_none()
        );
    }

    #[test]
    fn merge_dedupes_by_name() {
        let a = device_from_power_source(&dict(AIRPODS)).unwrap();
        let k = device_from_hid(&dict("<key>Product</key><string>Magic Mouse</string><key>BatteryPercent</key><integer>50</integer>")).unwrap();
        let dup = BtDevice {
            name: "alex’s airpods pro".into(),
            category: String::new(),
            parts: vec![],
        };
        let m = merge(vec![a.clone()], vec![dup, k.clone()]);
        assert_eq!(m, vec![a, k]);
    }
}
