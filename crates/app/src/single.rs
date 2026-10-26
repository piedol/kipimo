//! One Kipimo at a time. The first instance holds an exclusive lock on a file
//! in the user's temporary folder; a second launch finds it taken, asks the
//! running one (by distributed notification) to open its panel, and quits.

use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use objc2::sel;
use objc2_foundation::NSDistributedNotificationCenter;

use crate::util::{self, ns};

const SHOW: &str = "top.piedol.kipimo.show-panel";

static LOCK: OnceLock<std::fs::File> = OnceLock::new();

fn lock_path() -> std::path::PathBuf {
    std::env::temp_dir().join("top.piedol.kipimo.lock")
}

/// Take the instance lock. `false` if another Kipimo holds it.
pub fn acquire() -> bool {
    let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path())
    else {
        // No temp folder: don't block launching over it.
        return true;
    };
    // SAFETY: a valid descriptor; the lock lives as long as the file stays open.
    let ok = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if ok {
        let _ = LOCK.set(f);
    }
    ok
}

/// Ask the running instance to show itself.
pub fn signal_running() {
    let c = NSDistributedNotificationCenter::defaultCenter();
    // SAFETY: no object or userInfo.
    unsafe {
        c.postNotificationName_object_userInfo_deliverImmediately(&ns(SHOW), None, None, true)
    };
}

/// Listen for later launches (running instance).
pub fn listen() {
    let target = crate::ui::controller(util::mtm());
    let c = NSDistributedNotificationCenter::defaultCenter();
    // SAFETY: the controller lives for the whole process and the selector takes one NSNotification.
    unsafe {
        c.addObserver_selector_name_object(
            &target,
            sel!(anotherInstanceLaunched:),
            Some(&ns(SHOW)),
            None,
        )
    };
}
