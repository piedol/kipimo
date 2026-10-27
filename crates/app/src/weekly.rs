//! The weekly summary: one notification at the chosen day and time (default
//! Sunday 10:00). The background check calls [`check`] about once a minute; a
//! summary missed while the Mac slept goes out at the next check.
//!
//! Content comes from line providers. Built in: the Trash and free space. The
//! integration step adds "A cleanup would free 12 GB" and "4 app updates are
//! waiting" with [`add_line`]. Providers run on a background queue.

use std::sync::Mutex;

use crate::actions::Action;
use crate::i18n::{tr, trf, trn};
use crate::notify;
use crate::tools_window::Tab;
use crate::{format, settings, util};

/// Seconds east of UTC for local time at `t` (Unix seconds).
pub fn local_offset(t: i64) -> i64 {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

/// The most recent scheduled moment at or before `now` (Unix seconds), for a
/// weekday (1 = Sunday … 7 = Saturday) and minute of the day, in a zone
/// `offset` seconds east of UTC.
pub fn last_slot(now: i64, weekday: u8, minute: u16, offset: i64) -> i64 {
    let local = now + offset;
    let day = local.div_euclid(86_400);
    // 1970-01-01 was a Thursday (weekday 5).
    let wd = (day + 4).rem_euclid(7) + 1;
    let back = (wd - weekday as i64).rem_euclid(7);
    let mut slot = (day - back) * 86_400 + minute as i64 * 60;
    if slot > local {
        slot -= 7 * 86_400;
    }
    slot - offset
}

/// Whether a summary is due. A first run never sends one for a slot that
/// passed before Kipimo was set up.
pub fn due(now: i64, last: Option<i64>, weekday: u8, minute: u16, offset: i64) -> bool {
    let Some(last) = last else { return false };
    last < last_slot(now, weekday, minute, offset)
}

type Line = Box<dyn Fn() -> Option<String> + Send>;

static LINES: Mutex<Vec<Line>> = Mutex::new(Vec::new());

/// Add a line provider. Runs on a background queue when a summary goes out;
/// return `None` to say nothing.
#[allow(dead_code)] // Extension point for the integration step.
pub fn add_line(f: impl Fn() -> Option<String> + Send + 'static) {
    LINES.lock().unwrap().push(Box::new(f));
}

fn builtin_lines() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(t) = kipimo_sys::disk::trash_size()
        && t.bytes >= 100_000_000
    {
        v.push(trf(
            "The Trash holds {size}.",
            &[("size", &format::bytes(t.bytes))],
        ));
    }
    if let Some(free) = kipimo_sys::disk::boot_free_bytes() {
        v.push(trf(
            "{size} free on your startup disk.",
            &[("size", &format::bytes(free))],
        ));
    }
    v
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Called from the background check (main thread).
pub fn check() {
    let s = settings::get();
    let t = now();
    if s.weekly_last.is_none() {
        settings::update(|s| s.weekly_last = Some(t));
        return;
    }
    if !s.weekly
        || !due(
            t,
            s.weekly_last,
            s.weekly_weekday,
            s.weekly_minute,
            local_offset(t),
        )
    {
        return;
    }
    settings::update(|s| s.weekly_last = Some(t));
    send();
}

/// Gather the lines and post the summary.
pub fn send() {
    // A fresh junk scan and update check, in a background child process.
    crate::insights::refresh(|s| {
        let insights = summary_lines(&s);
        util::background(move || {
            let mut lines = insights;
            lines.extend(LINES.lock().unwrap().iter().filter_map(|f| f()));
            lines.extend(builtin_lines());
            let body = if lines.is_empty() {
                tr("Nothing needs doing this week.")
            } else {
                lines.join(" ")
            };
            util::main_async(move || {
                notify::post(notify::Alert {
                    id: "weekly-summary".into(),
                    title: tr("Your weekly Mac summary"),
                    body,
                    kind: notify::Kind::Weekly,
                    action: Some(Action::OpenTools(Tab::Clean)),
                });
            });
        });
    });
}

/// "A cleanup would free 12 GB." and "4 app updates are waiting."
pub fn summary_lines(s: &crate::insights::Summary) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(b) = s.junk_selected.filter(|b| *b >= 100_000_000) {
        v.push(trf(
            "A cleanup would free {size}.",
            &[("size", &format::bytes(b))],
        ));
    }
    match s.updates.as_ref().map(|u| u.len()) {
        Some(0) | None => {}
        Some(n) => v.push(trn(
            n as u64,
            "{n} app update is waiting.",
            "{n} app updates are waiting.",
        )),
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-25 is a Sunday. 10:00 UTC that day:
    const SUN_10_UTC: i64 = 1_792_922_400;

    #[test]
    fn epoch_weekday() {
        // 2026-10-25 00:00 UTC is a Sunday.
        assert_eq!(last_slot(SUN_10_UTC, 1, 0, 0), SUN_10_UTC - 10 * 3600);
    }

    #[test]
    fn slot_is_the_latest_past_sunday_ten() {
        assert_eq!(last_slot(SUN_10_UTC, 1, 600, 0), SUN_10_UTC);
        assert_eq!(last_slot(SUN_10_UTC + 60, 1, 600, 0), SUN_10_UTC);
        // One minute before: last week's.
        assert_eq!(
            last_slot(SUN_10_UTC - 60, 1, 600, 0),
            SUN_10_UTC - 7 * 86_400
        );
        // Wednesday after: still that Sunday.
        assert_eq!(last_slot(SUN_10_UTC + 3 * 86_400, 1, 600, 0), SUN_10_UTC);
    }

    #[test]
    fn slot_in_local_time() {
        // New York in October is UTC−4: Sunday 10:00 local is 14:00 UTC.
        let off = -4 * 3600;
        assert_eq!(
            last_slot(SUN_10_UTC + 5 * 3600, 1, 600, off),
            SUN_10_UTC + 4 * 3600
        );
    }

    #[test]
    fn due_once_per_week() {
        let last = SUN_10_UTC - 86_400; // Saturday
        assert!(!due(SUN_10_UTC - 60, Some(last), 1, 600, 0));
        assert!(due(SUN_10_UTC + 30, Some(last), 1, 600, 0));
        // Sent at 10:00:30: not again until next week.
        assert!(!due(SUN_10_UTC + 3600, Some(SUN_10_UTC + 30), 1, 600, 0));
        assert!(due(
            SUN_10_UTC + 7 * 86_400,
            Some(SUN_10_UTC + 30),
            1,
            600,
            0
        ));
        // Asleep on Sunday, awake Monday: goes out Monday.
        assert!(due(SUN_10_UTC + 86_400, Some(last), 1, 600, 0));
        // Never on a first run.
        assert!(!due(SUN_10_UTC + 30, None, 1, 600, 0));
    }
}
