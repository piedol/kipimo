//! Keeps a process alive long enough for Finder's "Put Back" records.
//!
//! `-[NSFileManager trashItemAtURL:…]` writes the Put Back record of the first
//! item at once and batches the rest about 2–3 s later, from inside this
//! process. A process that quits sooner loses them: measured on a disk image,
//! quitting right after five moves kept 1 record of 5, waiting 5 s kept all 5.
//! So every job that moves things to the Trash holds a [`Moving`] guard, and
//! quitting (the tools process when its last window closes, either process on
//! Quit) waits until [`remaining`] is zero.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long after the last move a process stays up.
pub const HOLD: Duration = Duration::from_secs(5);

static LAST_MOVE_MS: AtomicU64 = AtomicU64::new(0);
static MOVING: AtomicUsize = AtomicUsize::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Something just went to the Trash (any thread).
pub fn moved() {
    LAST_MOVE_MS.store(now_ms(), Ordering::Release);
}

/// Held for the whole of a job that moves things to the Trash; dropping it
/// counts as the last move.
pub struct Moving(());

pub fn moving() -> Moving {
    MOVING.fetch_add(1, Ordering::AcqRel);
    Moving(())
}

impl Drop for Moving {
    fn drop(&mut self) {
        moved();
        MOVING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How long this process must still stay up (a full hold while a job runs).
pub fn remaining() -> Duration {
    if MOVING.load(Ordering::Acquire) > 0 {
        return HOLD;
    }
    remaining_at(LAST_MOVE_MS.load(Ordering::Acquire), now_ms())
}

pub(crate) fn remaining_at(last: u64, now: u64) -> Duration {
    if last == 0 {
        return Duration::ZERO;
    }
    let until = last + HOLD.as_millis() as u64;
    Duration::from_millis(until.saturating_sub(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_five_seconds_after_the_last_move() {
        assert_eq!(remaining_at(0, 10_000), Duration::ZERO);
        assert_eq!(remaining_at(10_000, 10_000), HOLD);
        assert_eq!(remaining_at(10_000, 13_000), Duration::from_secs(2));
        assert_eq!(remaining_at(10_000, 16_000), Duration::ZERO);
    }

    #[test]
    fn a_running_job_holds_until_it_ends() {
        let job = moving();
        assert_eq!(remaining(), HOLD);
        drop(job);
        let left = remaining();
        assert!(left > Duration::from_secs(4) && left <= HOLD, "{left:?}");
    }
}
