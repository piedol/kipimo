//! Preferences, stored in NSUserDefaults (`top.piedol.kipimo`), loaded once and
//! cached on the main thread. Changed through the Settings window, except the ignore
//! list, which Startup items changes (see [`set_ignored`]).

use std::cell::RefCell;

use objc2_foundation::NSUserDefaults;

use crate::util::ns;

pub const GB: u64 = 1_000_000_000;

/// Choices offered in Settings.
pub const DISK_GB_CHOICES: [u32; 7] = [5, 10, 15, 20, 30, 50, 100];
pub const TRASH_GB_CHOICES: [u32; 6] = [1, 2, 5, 10, 20, 50];
pub const BLUETOOTH_PCT_CHOICES: [u8; 5] = [10, 15, 20, 25, 30];
pub const REFRESH_CHOICES: [f64; 2] = [1.0, 2.0];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Panel refresh interval in seconds while it is open.
    pub refresh_secs: f64,
    /// Colour the menu-bar icon when something needs attention.
    pub tint_icon: bool,

    pub alert_disk: bool,
    pub disk_gb: u32,
    pub alert_trash: bool,
    pub trash_gb: u32,
    pub alert_bluetooth: bool,
    pub bluetooth_pct: u8,
    pub alert_drives: bool,
    pub alert_leftovers: bool,

    pub weekly: bool,
    /// 1 = Sunday … 7 = Saturday (NSCalendar's weekday numbering).
    pub weekly_weekday: u8,
    /// Minutes after midnight.
    pub weekly_minute: u16,
    /// When the last weekly summary went out (Unix seconds).
    pub weekly_last: Option<i64>,

    /// A recommendation put off until tomorrow: its id and the local day number.
    pub snoozed: Option<(String, i64)>,

    /// Problems the user chose to ignore in Startup items (keys from [`crate::ignore`]).
    pub ignored: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            refresh_secs: 2.0,
            tint_icon: true,
            alert_disk: true,
            disk_gb: 20,
            alert_trash: true,
            trash_gb: 2,
            alert_bluetooth: true,
            bluetooth_pct: 20,
            alert_drives: true,
            alert_leftovers: true,
            weekly: true,
            weekly_weekday: 1,
            weekly_minute: 10 * 60,
            weekly_last: None,
            snoozed: None,
            ignored: Vec::new(),
        }
    }
}

impl Settings {
    pub fn disk_bytes(&self) -> u64 {
        self.disk_gb as u64 * GB
    }
    pub fn trash_bytes(&self) -> u64 {
        self.trash_gb as u64 * GB
    }

    /// Clamp values read from disk to what Settings offers.
    pub fn sanitized(mut self) -> Settings {
        let d = Settings::default();
        if !REFRESH_CHOICES.contains(&self.refresh_secs) {
            self.refresh_secs = d.refresh_secs;
        }
        if !DISK_GB_CHOICES.contains(&self.disk_gb) {
            self.disk_gb = d.disk_gb;
        }
        if !TRASH_GB_CHOICES.contains(&self.trash_gb) {
            self.trash_gb = d.trash_gb;
        }
        if !BLUETOOTH_PCT_CHOICES.contains(&self.bluetooth_pct) {
            self.bluetooth_pct = d.bluetooth_pct;
        }
        if !(1..=7).contains(&self.weekly_weekday) {
            self.weekly_weekday = d.weekly_weekday;
        }
        if self.weekly_minute >= 24 * 60 {
            self.weekly_minute = d.weekly_minute;
        }
        self
    }

    fn load() -> Settings {
        let d = NSUserDefaults::standardUserDefaults();
        let has = |k: &str| d.objectForKey(&ns(k)).is_some();
        let b = |k: &str, def: bool| if has(k) { d.boolForKey(&ns(k)) } else { def };
        let i = |k: &str, def: i64| {
            if has(k) {
                d.integerForKey(&ns(k)) as i64
            } else {
                def
            }
        };
        let def = Settings::default();
        let snoozed = d
            .stringForKey(&ns("snoozedRecommendation"))
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .map(|id| (id, i("snoozedDay", 0)));
        Settings {
            refresh_secs: if i("refreshSeconds", 2) == 1 {
                1.0
            } else {
                2.0
            },
            tint_icon: b("tintIcon", def.tint_icon),
            alert_disk: b("alertDisk", def.alert_disk),
            disk_gb: i("diskGB", def.disk_gb as i64) as u32,
            alert_trash: b("alertTrash", def.alert_trash),
            trash_gb: i("trashGB", def.trash_gb as i64) as u32,
            alert_bluetooth: b("alertBluetooth", def.alert_bluetooth),
            bluetooth_pct: i("bluetoothPercent", def.bluetooth_pct as i64).clamp(0, 100) as u8,
            alert_drives: b("alertDrives", def.alert_drives),
            alert_leftovers: b("alertLeftovers", def.alert_leftovers),
            weekly: b("weeklySummary", def.weekly),
            weekly_weekday: i("weeklyWeekday", def.weekly_weekday as i64).clamp(0, 9) as u8,
            weekly_minute: i("weeklyMinute", def.weekly_minute as i64).clamp(0, 9999) as u16,
            weekly_last: has("weeklyLast").then(|| i("weeklyLast", 0)),
            snoozed,
            ignored: d
                .stringArrayForKey(&ns(IGNORED_KEY))
                .map(|a| a.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
        }
        .sanitized()
    }

    /// Writes the ignore list alone. [`Settings::save`] leaves it out: the tools window runs
    /// in another process with its own copy of the settings, and a save from either process
    /// must not put back an older list.
    fn save_ignored(&self) {
        let d = NSUserDefaults::standardUserDefaults();
        if self.ignored.is_empty() {
            d.removeObjectForKey(&ns(IGNORED_KEY));
            return;
        }
        let items: Vec<_> = self.ignored.iter().map(|k| ns(k)).collect();
        let array = objc2_foundation::NSArray::from_retained_slice(&items);
        // SAFETY: an NSArray of NSStrings is a valid property-list value.
        unsafe { d.setObject_forKey(Some(&array), &ns(IGNORED_KEY)) };
    }

    fn save(&self) {
        let d = NSUserDefaults::standardUserDefaults();
        let int = |k: &str, v: i64| d.setInteger_forKey(v as isize, &ns(k));
        let boolean = |k: &str, v: bool| d.setBool_forKey(v, &ns(k));
        int("refreshSeconds", self.refresh_secs as i64);
        boolean("tintIcon", self.tint_icon);
        boolean("alertDisk", self.alert_disk);
        int("diskGB", self.disk_gb as i64);
        boolean("alertTrash", self.alert_trash);
        int("trashGB", self.trash_gb as i64);
        boolean("alertBluetooth", self.alert_bluetooth);
        int("bluetoothPercent", self.bluetooth_pct as i64);
        boolean("alertDrives", self.alert_drives);
        boolean("alertLeftovers", self.alert_leftovers);
        boolean("weeklySummary", self.weekly);
        int("weeklyWeekday", self.weekly_weekday as i64);
        int("weeklyMinute", self.weekly_minute as i64);
        match self.weekly_last {
            Some(t) => int("weeklyLast", t),
            None => d.removeObjectForKey(&ns("weeklyLast")),
        }
        match &self.snoozed {
            Some((id, day)) => {
                // SAFETY: a valid NSString value and key.
                unsafe { d.setObject_forKey(Some(&ns(id)), &ns("snoozedRecommendation")) };
                int("snoozedDay", *day);
            }
            None => d.removeObjectForKey(&ns("snoozedRecommendation")),
        }
    }
}

const IGNORED_KEY: &str = "ignoredIssues";

thread_local! {
    static CURRENT: RefCell<Option<Settings>> = const { RefCell::new(None) };
}

/// A snapshot of the current settings.
pub fn get() -> Settings {
    CURRENT.with(|c| c.borrow_mut().get_or_insert_with(Settings::load).clone())
}

/// Change settings and persist them.
pub fn update(f: impl FnOnce(&mut Settings)) -> Settings {
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let s = c.get_or_insert_with(Settings::load);
        f(s);
        s.save();
        s.clone()
    })
}

/// Replace the ignore list and persist it (only that key).
pub fn set_ignored(list: Vec<String>) {
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let s = c.get_or_insert_with(Settings::load);
        s.ignored = list;
        s.save_ignored();
    });
}

/// Take the ignore list the tools process just saved, without writing it again.
pub fn adopt_ignored(list: Vec<String>) {
    CURRENT.with(|c| {
        c.borrow_mut().get_or_insert_with(Settings::load).ignored = list;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_scope() {
        let s = Settings::default();
        assert_eq!(s.disk_bytes(), 20 * GB);
        assert_eq!(s.trash_bytes(), 2 * GB);
        assert_eq!(s.bluetooth_pct, 20);
        assert!(
            s.alert_disk
                && s.alert_trash
                && s.alert_bluetooth
                && s.alert_drives
                && s.alert_leftovers
        );
        // Weekly summary: Sunday 10:00.
        assert!(s.weekly);
        assert_eq!((s.weekly_weekday, s.weekly_minute), (1, 600));
        assert!(DISK_GB_CHOICES.contains(&s.disk_gb));
        assert!(TRASH_GB_CHOICES.contains(&s.trash_gb));
        assert!(BLUETOOTH_PCT_CHOICES.contains(&s.bluetooth_pct));
    }

    #[test]
    fn sanitize_resets_out_of_range_values() {
        let s = Settings {
            disk_gb: 7,
            trash_gb: 0,
            bluetooth_pct: 99,
            weekly_weekday: 9,
            weekly_minute: 5000,
            refresh_secs: 0.1,
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s, Settings::default());
    }
}
