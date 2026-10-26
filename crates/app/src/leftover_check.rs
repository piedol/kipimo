//! What an app dragged to the Trash left behind, worked out in a short-lived child process.
//!
//! The leftover finder reads every installed app's Info.plist and code signature and walks the
//! Library folders: tens of megabytes of allocations the menu-bar app would keep (malloc rarely
//! returns them), so it runs as `kipimo --kipimo-leftovers <bundle>`, which prints one
//! [`Found`] line and exits.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util;

pub const WORKER_ARG: &str = "--kipimo-leftovers";
const LINE_PREFIX: &str = "KIPIMO-LEFTOVERS ";

/// Waits this long after an app lands in the Trash before looking: an updater (Sparkle and
/// others) moves the old copy to the Trash and puts the new one in place, and by then the new
/// copy counts as "another copy is installed", so no alert.
pub const SETTLE: Duration = Duration::from_secs(30);

/// The worker's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub bundle_id: String,
    pub name: String,
    pub count: usize,
    pub bytes: u64,
    /// Another copy of the app is installed (an update, or a duplicate), so nothing is left over.
    pub other_copy: bool,
}

/// `kipimo --kipimo-leftovers <bundle>`.
pub fn worker_main() -> i32 {
    // SAFETY: lowers this process's own CPU and I/O priority.
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) };
    let Some(bundle) = std::env::args_os().nth(2).map(PathBuf::from) else {
        return 2;
    };
    let Ok(env) = kipimo_clean::Env::current_user() else {
        return 1;
    };
    let cancel = AtomicBool::new(false);
    let Some(report) = kipimo_clean::find_leftovers_for_bundle(&env, &bundle, &cancel) else {
        return 1;
    };
    let id = report.target.bundle_id.clone();
    let found = Found {
        name: report
            .target
            .names
            .first()
            .cloned()
            .unwrap_or_else(|| id.clone()),
        bundle_id: id,
        count: report.leftovers.len(),
        bytes: report.total_bytes,
        other_copy: report.other_copy.is_some(),
    };
    println!(
        "{LINE_PREFIX}{}",
        serde_json::to_string(&found).unwrap_or_default()
    );
    0
}

/// Runs the worker on `bundle` (call off the main thread).
pub fn check(bundle: &Path) -> Option<Found> {
    let exe = std::env::current_exe().ok()?;
    let mut child = Command::new(exe)
        .arg(WORKER_ARG)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| util::log(format!("couldn't start the leftover check: {e}")))
        .ok()?;
    let out = child.stdout.take()?;
    let found = BufReader::new(out)
        .lines()
        .map_while(Result::ok)
        .find_map(|l| serde_json::from_str::<Found>(l.strip_prefix(LINE_PREFIX)?).ok());
    let _ = child.wait();
    found
}
