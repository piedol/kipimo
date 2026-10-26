//! The panel's "Today's recommendation": one suggestion, picked from pluggable
//! sources. A pending recommendation also colours the menu-bar icon.
//!
//! Built in: a low startup disk and a big Trash. The integration step adds
//! tool-driven sources ("Leftovers from Zoom found", "4 app updates") with
//! [`register`]; a source keeps its own state (e.g. the last scan result) and
//! answers from it, quickly, on the main thread.

use std::cell::RefCell;

use crate::actions::Action;
use crate::attention::Limits;
use crate::format;
use crate::tools_window::Tab;

/// What sources can look at.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub boot_free: Option<u64>,
    /// The startup disk is below the alert threshold (with hysteresis).
    pub disk_low: bool,
    pub trash_bytes: Option<u64>,
    /// The Trash is above the threshold (with hysteresis).
    pub trash_over: bool,
    /// macOS won't let Kipimo read the Trash (no Full Disk Access yet).
    pub trash_unreadable: bool,
    pub limits: Option<Limits>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    /// Stable per kind of suggestion ("trash", "disk", "leftovers:com.zoom.us"),
    /// so "Later" can snooze it until tomorrow.
    pub id: String,
    pub title: String,
    pub detail: String,
    /// Button label and what it does.
    pub button: Option<(String, Action)>,
    /// Higher wins when several sources have something.
    pub priority: i32,
    /// Colours the menu-bar icon while pending. Setup hints don't.
    pub nag: bool,
}

pub trait Source {
    fn recommend(&self, facts: &Facts) -> Option<Recommendation>;
}

/// The startup disk is nearly full.
pub struct DiskSpace;

impl Source for DiskSpace {
    fn recommend(&self, f: &Facts) -> Option<Recommendation> {
        let free = f.boot_free.filter(|_| f.disk_low)?;
        Some(Recommendation {
            id: "disk".into(),
            title: format!("Only {} left on your startup disk", format::bytes(free)),
            detail: "Clean up caches, logs and other junk to make room.".into(),
            button: Some(("Clean up".into(), Action::OpenTools(Tab::Clean))),
            priority: 100,
            nag: true,
        })
    }
}

/// The Trash is big.
pub struct Trash;

impl Source for Trash {
    fn recommend(&self, f: &Facts) -> Option<Recommendation> {
        let bytes = f.trash_bytes.filter(|_| f.trash_over)?;
        Some(Recommendation {
            id: "trash".into(),
            title: format!("Trash is {}", format::bytes(bytes)),
            detail: "Empty it in Finder to get that space back.".into(),
            button: Some(("Open Trash".into(), Action::OpenTrash)),
            priority: 50,
            nag: true,
        })
    }
}

/// Kipimo can't read the Trash until it has Full Disk Access.
pub struct TrashAccess;

impl Source for TrashAccess {
    fn recommend(&self, f: &Facts) -> Option<Recommendation> {
        f.trash_unreadable.then(|| Recommendation {
            id: "trash-access".into(),
            title: "Let Kipimo see the Trash".into(),
            detail: "Allow Full Disk Access to measure it and find leftovers.".into(),
            button: Some((
                "Allow…".into(),
                Action::OpenUrl(FULL_DISK_ACCESS_URL.into()),
            )),
            priority: 5,
            nag: false,
        })
    }
}

/// System Settings › Privacy & Security › Full Disk Access.
pub const FULL_DISK_ACCESS_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

thread_local! {
    static SOURCES: RefCell<Vec<Box<dyn Source>>> = RefCell::new(vec![Box::new(DiskSpace), Box::new(Trash), Box::new(TrashAccess)]);
}

/// Add a source (main thread).
#[allow(dead_code)] // Extension point for the integration step.
pub fn register(source: Box<dyn Source>) {
    SOURCES.with(|s| s.borrow_mut().push(source));
}

/// The best suggestion from `sources`, skipping one snoozed for `today`.
pub fn pick_from(
    sources: &[Box<dyn Source>],
    facts: &Facts,
    snoozed: Option<&(String, i64)>,
    today: i64,
) -> Option<Recommendation> {
    sources
        .iter()
        .filter_map(|s| s.recommend(facts))
        .filter(|r| snoozed.is_none_or(|(id, day)| !(id == &r.id && *day == today)))
        .max_by_key(|r| r.priority)
}

/// The current recommendation from every registered source.
pub fn current(facts: &Facts) -> Option<Recommendation> {
    let s = crate::settings::get();
    SOURCES.with(|src| pick_from(&src.borrow(), facts, s.snoozed.as_ref(), today()))
}

/// Put a recommendation off until tomorrow.
pub fn snooze(id: &str) {
    let day = today();
    crate::settings::update(|s| s.snoozed = Some((id.to_string(), day)));
}

/// Local day number (days since 1970-01-01 in local time).
pub fn today() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (now + crate::weekly::local_offset(now)).div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtins() -> Vec<Box<dyn Source>> {
        vec![Box::new(DiskSpace), Box::new(Trash), Box::new(TrashAccess)]
    }

    #[test]
    fn nothing_when_all_is_well() {
        let f = Facts {
            boot_free: Some(400_000_000_000),
            trash_bytes: Some(1_000),
            ..Facts::default()
        };
        assert_eq!(pick_from(&builtins(), &f, None, 0), None);
    }

    #[test]
    fn trash_over_threshold() {
        let f = Facts {
            trash_bytes: Some(3_100_000_000),
            trash_over: true,
            ..Facts::default()
        };
        let r = pick_from(&builtins(), &f, None, 0).unwrap();
        assert_eq!(r.title, "Trash is 3.1 GB");
        assert_eq!(r.button, Some(("Open Trash".into(), Action::OpenTrash)));
    }

    #[test]
    fn low_disk_wins_over_trash() {
        let f = Facts {
            boot_free: Some(14_000_000_000),
            disk_low: true,
            trash_bytes: Some(3_100_000_000),
            trash_over: true,
            ..Facts::default()
        };
        let r = pick_from(&builtins(), &f, None, 0).unwrap();
        assert_eq!(r.id, "disk");
        assert_eq!(r.title, "Only 14 GB left on your startup disk");
    }

    #[test]
    fn access_hint_is_quiet_and_last() {
        let f = Facts {
            trash_unreadable: true,
            ..Facts::default()
        };
        let r = pick_from(&builtins(), &f, None, 0).unwrap();
        assert_eq!(r.id, "trash-access");
        assert!(!r.nag);
        let f = Facts {
            trash_unreadable: true,
            boot_free: Some(1),
            disk_low: true,
            ..Facts::default()
        };
        assert_eq!(pick_from(&builtins(), &f, None, 0).unwrap().id, "disk");
    }

    #[test]
    fn snooze_lasts_one_day() {
        let f = Facts {
            trash_bytes: Some(3_100_000_000),
            trash_over: true,
            ..Facts::default()
        };
        let snoozed = ("trash".to_string(), 20_000);
        assert_eq!(pick_from(&builtins(), &f, Some(&snoozed), 20_000), None);
        assert!(pick_from(&builtins(), &f, Some(&snoozed), 20_001).is_some());
    }

    #[test]
    fn plugged_in_source() {
        struct Updates(u32);
        impl Source for Updates {
            fn recommend(&self, _: &Facts) -> Option<Recommendation> {
                (self.0 > 0).then(|| Recommendation {
                    id: "updates".into(),
                    title: format!("{} app updates", self.0),
                    detail: String::new(),
                    button: Some(("Review".into(), Action::OpenTools(Tab::Updates))),
                    priority: 60,
                    nag: true,
                })
            }
        }
        let mut s = builtins();
        s.push(Box::new(Updates(4)));
        let f = Facts {
            trash_bytes: Some(3_100_000_000),
            trash_over: true,
            ..Facts::default()
        };
        assert_eq!(pick_from(&s, &f, None, 0).unwrap().title, "4 app updates");
    }
}
