//! # kipimo-startup: tool D (Startup items) and the repair check (SCOPE E6-D, R6.3)
//!
//! UI-free engine. Every function is synchronous; call it from a background thread. Long
//! operations take a cancel flag and a callback that streams results. All results are plain
//! serde types (paths are `PathBuf`).
//!
//! ## Inventory
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use kipimo_startup::*;
//!
//! let cancel = AtomicBool::new(false);
//! let inventory = scan(&ScanOptions::default(), &cancel, |progress, item| {
//!     // progress.done / progress.total; `item` is finished (signature checked).
//! });
//! for item in &inventory.items {
//!     if item.needs_attention() { /* highlight: item.flags */ }
//! }
//! ```
//!
//! - [`scan`] → [`Inventory`] of [`StartupItem`]s: user and system LaunchAgents, LaunchDaemons,
//!   login items and app background items (Background Task Management), system extensions,
//!   loaded third-party kernel extensions. Each item has its [`ItemKind`], [`Scope`]
//!   (user / system), label, program, arguments, the script it runs (if any), owning app
//!   ([`OwnerApp`], `installed: false` when the app is gone), [`ItemState`] (enabled, loaded,
//!   pid, launchd override, "Allow in the Background" status) and [`Signature`].
//! - [`Flag`]s mark what to highlight: unsigned, ad-hoc, invalid signature, untrusted
//!   certificate, program/script/app missing (orphans: [`StartupItem::is_orphaned`]),
//!   malformed plist, runs a script. [`Flag::severity`] and [`Flag::describe`] help the UI.
//! - A typical scan of this Mac: 22 items in about 1 s.
//!
//! ## Actions (only on items the user selected)
//!
//! - [`disable_items`]`(&[StartupItem]) -> Vec<`[`ActionResult`]`>`, one result per item:
//!   - user LaunchAgent → [`ActionOutcome::Disabled`]: removed from launchd for this session,
//!     then its plist moved to the Trash (Finder's Put Back works);
//!   - `/Library/LaunchAgents` and `/Library/LaunchDaemons` → [`ActionOutcome::NeedsAdmin`]
//!     with the exact Terminal commands (a reversible `disable` set and a `remove` set that
//!     moves the plist and privileged helper into the user's Trash). Kipimo never escalates;
//!   - login items, background items, system extensions → [`ActionOutcome::OpenSettings`]
//!     with [`LOGIN_ITEMS_SETTINGS_URL`] and a hint; [`open_settings`] opens it;
//!   - kernel extensions → skipped with a reason.
//!
//! ## Repair check
//!
//! - [`repair_check`]`(&`[`RepairOptions`]`, &cancel, |`[`RepairEvent`]`| ..) -> `[`RepairReport`]:
//!   validates every app in `/Applications` and `~/Applications` like
//!   `codesign --verify --deep --strict` (through `SecStaticCode`, offline) and reports
//!   `broken` apps (signature no longer validates) with [`AppCheck::reason`] and
//!   [`signer_summary`], plus `unsigned` ones and `not_checked` ones (for example a seal only
//!   root can read). In-process cancel takes effect between apps; the isolated variant
//!   cancels at once. On this Mac: 58 apps in 22-30 s with 4 threads.
//! - Note: deleting whole `.lproj` folders doesn't break a signature (Apple's seal rules mark
//!   localizations optional), and thinning keeps each remaining slice's own signature, so
//!   the check reports only seals that really fail, as `codesign --verify` would.
//! - Each [`AppCheck`] has the app's `version` and its signature's `cdhash` (the code directory
//!   hash, read without validation), so a caller can tell one build from the next (Kipimo's
//!   Ignore keys on them).
//!
//! ## Replacing a broken app (Download and Replace)
//!
//! Kipimo never re-signs an app: an ad-hoc signature made on this Mac would replace the
//! developer's, which breaks entitlements, keychain access, notarization trust and the app's
//! own updates, and it would mean changing an app bundle. It swaps in the developer's copy:
//!
//! - [`replaceable`]`(&app) -> Result<`[`InstalledCopy`]`, `[`ReplaceError`]`>`: the copy's
//!   signature claims a Team ID and the app and its folder are writable without an
//!   administrator. Quick (the signature's claims and the main executable only).
//! - [`WorkDir::for_app`]: a scratch folder on the app's volume (`NSItemReplacementDirectory`),
//!   removed with its contents when dropped. Download the archive into it.
//! - [`replace_from_zip`]`(&app, &zip, &work) -> Result<`[`Replaced`]`, `[`ReplaceError`]`>`:
//!   refuses archive names that would land outside the work folder (absolute, `..`, or
//!   through a symlink in the archive), unpacks with `/usr/bin/ditto -x -k`, finds the app with
//!   the same bundle identifier, validates it like `codesign --verify --deep --strict` and
//!   requires a Developer ID signature from the old copy's team ([`check_replacement`]). Only
//!   then does the old copy go to the Trash (Put Back works) and the new one take its place;
//!   if that rename fails, the old copy is moved back. Hold the app's Put Back guard around it.
//!   `ReplaceError::Blocked` means macOS refused to move the old copy (App Management).
//!
//! ## Memory: run heavy work in a child process
//!
//! The Security framework leaves tens of MB of malloc memory dirty after validating big
//! bundles (about 65-85 MB after Xcode; about 17 MB after a scan), which would break the
//! app's 15 MB idle budget. [`scan_isolated`] and [`repair_check_isolated`] run the same work
//! in a child process (the app's own executable started with [`WORKER_ARG`]) and stream the
//! same events back; the parent stays under 2 MB and cancel kills the child at once. The app's
//! `main` must start with:
//!
//! ```no_run
//! if std::env::args().nth(1).as_deref() == Some(kipimo_startup::WORKER_ARG) {
//!     std::process::exit(kipimo_startup::worker_main());
//! }
//! ```
//!
//! ## Where the data comes from (all without root)
//!
//! - launchd plists: read from the folders (XML, binary or ASCII), never executed.
//! - Loaded/running: `SMCopyAllJobDictionaries` (ServiceManagement; deprecated but still the
//!   only public launchd query). Overrides: launchd's `disabled.<uid>.plist`.
//! - "Allow in the Background": `SMAppService.statusForLegacyURL:` for plists; the Background
//!   Task Management database (`BackgroundItems-v<N>.btm`, a keyed archive that needs Full
//!   Disk Access to read) for login items, app background items, developer names and owning
//!   apps. When that file can't be read or decoded and the user asks for it (it needs an
//!   administrator password): `/usr/bin/sfltool dumpbtm`.
//! - System extensions: sysextd's `/Library/SystemExtensions/db.plist`. Kernel extensions:
//!   IOKit `KextManagerCopyLoadedKextInfo`.
//! - Owning app: `AssociatedBundleIdentifiers` through LaunchServices, the enclosing `.app`,
//!   or the BTM parent record.
//! - Code signing: `SecStaticCode` validity, `SecCodeCopySigningInformation`, and
//!   requirement checks (`anchor apple`, Developer ID, Mac App Store, `notarized`).
//!
//! [`codesign::Checker`] is public for other tools that need a signature check.

use std::path::PathBuf;

mod actions;
mod btm;
mod cf;
pub mod codesign;
mod inventory;
mod isolated;
mod launchd;
mod launchd_state;
mod model;
mod owner;
mod repair;
mod replace;
mod sysext;
#[cfg(test)]
mod testutil;
mod trash;

pub use actions::{
    ActionOutcome, ActionResult, AdminCommand, AdminError, LOGIN_ITEMS_SETTINGS_URL, disable_items,
    open_settings, remove_leftover_entries, run_with_password, stop_user_job, unload_user_agent,
    user_job_of_pid,
};
pub use inventory::{Roots, ScanOptions, scan};
pub use isolated::{WORKER_ARG, repair_check_isolated, scan_isolated, worker_main};
pub use model::*;
pub use repair::{
    AppCheck, RepairEvent, RepairOptions, RepairReport, find_apps, repair_check, signer_summary,
};
pub use replace::{
    InstalledCopy, ReplaceError, Replaced, WorkDir, check_replacement, replace_from_zip,
    replaceable,
};

/// Development builds only: `KIPIMO_FAKE_HOME` / `KIPIMO_FAKE_ROOT` stand in for the home
/// folder and `/`, so the app can be exercised on a synthetic Mac (a throwaway disk image).
/// Release builds ignore them.
pub fn fake_home() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_HOME")
}

/// See [`fake_home`].
pub fn fake_root() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_ROOT")
}

fn dev_var(name: &str) -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    }
}
