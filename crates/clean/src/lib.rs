//! Tools A and B: the junk cleanup scanner, the app uninstaller, and the leftover finder used when
//! an app is moved to the Trash. Everything removed goes to the Trash, never deleted permanently.
//!
//! No UI here. Every call is synchronous: run it on a background thread. Long calls stream
//! events through a callback (called on the calling thread) and stop early when the
//! `cancel: &AtomicBool` flag is set, returning what they have so far. All results derive
//! `serde::Serialize`; sizes are allocated bytes, paths are `PathBuf`, dates are Unix seconds.
//!
//! # Environment
//!
//! Everything runs against an [`Env`]: the home folder, the system root and providers for the
//! live system (running apps, the Trash, code signatures, Spotlight).
//!
//! - [`Env::current_user()`] — the real Mac (home from the password database, `/`, libproc,
//!   `NSFileManager` Trash, embedded code signatures, Spotlight). Use this in the app.
//! - [`Env::new(home, system_root)`](Env::new) — a synthetic environment with inert providers:
//!   nothing running, **no Trash** (every move is refused), no signatures, no Spotlight. Tests set
//!   their own [`Trasher`], [`RunningApps`], [`SigningReader`] or [`LastUsedReader`].
//!
//! # A. Clean up
//!
//! ```ignore
//! let report = kipimo_clean::scan_junk(&env, &ScanOptions::default(), &mut |event| match event {
//!     ScanEvent::Started { categories } => {}        // show the rows
//!     ScanEvent::Progress { bytes, files } => {}     // running total, ~10 per second
//!     ScanEvent::Category { report } => {}           // one category finished (any order)
//! }, &cancel);
//! // The user ticks items (start from `item.default_selected`), then:
//! let done = kipimo_clean::clean(&env, &ticked_items, &ScanOptions::default(), &mut |p| {}, &cancel);
//! ```
//!
//! [`scan_junk`] returns a [`JunkReport`]: one [`CategoryReport`] per [`CategoryId`] with its
//! [`Risk`], whether it starts ticked, its [`JunkItem`]s (largest first) and the
//! [`SkippedEntry`]s it saw but won't offer (with a [`SkipReason`]: in use by a running app,
//! essential to macOS, recent, a symlink...). `JunkReport::selected_bytes` is what a cleanup with
//! the default ticks frees (the weekly summary's number). `root_unreadable` on a category means
//! Kipimo needs Full Disk Access to see it.
//!
//! | Category | Risk | Ticked | Where |
//! |---|---|---|---|
//! | `app_caches` | low | yes | `~/Library/Caches/*` (not `com.apple.*`), minus running apps' caches |
//! | `system_caches` | medium | no | `~/Library/Caches/com.apple.*`, minus an essential list (CloudKit, iCloud, App Store, Photos analysis...) |
//! | `logs` | low | yes | `~/Library/Logs/*`, each crash report in `DiagnosticReports` |
//! | `xcode_derived_data` | low | yes | `DerivedData/*` (skipped while Xcode runs) |
//! | `xcode_archives` | medium | no | `.xcarchive`s older than `archive_age_days` (dSYMs!) |
//! | `xcode_device_support` | low | yes | `* DeviceSupport/*` except the newest and those used within `device_support_keep_days` |
//! | `xcode_simulators` | low | yes | devices whose runtime isn't installed or that are deleted (CoreSimulator metadata, no `xcrun`), stale dyld caches |
//! | `developer_caches` | low | yes (pnpm store no) | Homebrew, npm, Yarn, pnpm, Cargo, pip, Gradle, CocoaPods, Go |
//! | `mail_downloads` | low | yes | Mail's attachment copies |
//! | `ios_backups` | high | no | MobileSync backups older than `backup_age_days`; may be the only copy |
//! | `trash` | — | report only | `~/.Trash`; `action == EmptyTrashInFinder`, [`clean`] refuses it |
//!
//! [`clean`] takes the ticked [`JunkItem`]s and returns a [`CleanReport`] with one [`ItemResult`]
//! per item: [`ItemOutcome::Moved`], `Skipped { reason }` or `Failed { error }`. Each item is
//! re-checked against a fresh discovery first, so anything that isn't a current scan result, or
//! belongs to an app that started since the scan, is skipped.
//!
//! # B. Uninstall
//!
//! ```ignore
//! let apps = kipimo_clean::list_apps(&env, &mut |e| { /* AppsEvent::Found / Sized / Progress */ }, &cancel);
//! let target = AppTarget::from_bundle(&env, &app.path).unwrap();
//! let index = AppIndex::build(&env, &cancel);
//! let found = kipimo_clean::find_leftovers(&env, &target, &index, &cancel);
//! // app.protected.is_some(): show why and offer nothing. app.running: offer "Quit first".
//! let ticked: Vec<Leftover> = found.leftovers.iter().filter(|l| l.default_selected).cloned().collect();
//! let r = kipimo_clean::uninstall(&env, &app.path, app.size_bytes.unwrap_or(0), &ticked, &mut |p| {}, &cancel);
//! // r.status: Done | QuitFirst | Refused | NotAnApp | AppNotMoved | Cancelled
//! ```
//!
//! [`list_apps`] covers `/Applications` and `~/Applications` plus one level of subfolders, with
//! name, bundle ID, version, size, Spotlight last-used date, team ID, `running`, and
//! `protected` ([`Protection::ShipsWithMacOS`] for platform binaries, `/System` and the sealed
//! volume — Safari, for one — or [`Protection::Kipimo`]). [`uninstall`] refuses protected and
//! running apps, moves the app first and touches leftovers only if that worked. An app owned by
//! root (installed by a `.pkg`) usually needs an administrator: it's reported as
//! `SkipReason::NeedsAdmin`, never escalated.
//!
//! # Leftover finder (B and the "app moved to the Trash" alert)
//!
//! [`find_leftovers`] (or [`find_leftovers_for_bundle`] for a bundle sitting in `~/.Trash`, or
//! [`AppTarget::from_bundle_id`] for an app that's already gone) returns a [`LeftoverReport`]:
//! [`Leftover`]s with [`LeftoverKind`], size, [`Confidence`], a reason, `needs_admin` for
//! `/Library` items (reported only) and `default_selected`; `kept` lists matches another
//! installed app still uses (a shared App Group, a launch agent that starts another app, a
//! folder named after another app's ID). `other_copy` is set when the same app is installed
//! elsewhere (then everything is kept). [`remove_leftovers`] moves the user's picks to the Trash
//! after re-checking them.
//!
//! Locations: `~/Library/{Application Support, Application Scripts, Caches, Containers, Group
//! Containers, Preferences (+ ByHost), Saved Application State, HTTPStorages, WebKit, Logs,
//! Cookies, LaunchAgents}`, and `/Library/{Application Support, Caches, Preferences, Logs,
//! LaunchAgents, LaunchDaemons, PrivilegedHelperTools}`.
//!
//! # Safety rules
//!
//! - Moves go through [`Trasher`]; the real one is `NSFileManager trashItemAtURL:`, which keeps
//!   Finder's "Put Back" working (verified: it writes Finder's `ptbL`/`ptbN` records).
//! - Nothing outside the home folder is moved, except app bundles in the app folders. Paths are
//!   resolved first; symlinks are never followed during scans and never offered.
//! - Items that need an administrator are reported, never escalated.
//! - Destructive calls re-check every selected item against a fresh scan or finder run; anything
//!   else is returned as `Skipped { NotACandidate }` and left alone.
//!
//! # Resource use
//!
//! Size walks use `getattrlistbulk` on `Env::threads` workers (2–8) with a shared depth-first
//! queue; a file with several hard links counts once per call (up to 500,000 linked inodes, then
//! `hardlink_cap_hit`). Code signatures are read from each executable's embedded signature rather
//! than through the Security framework, which keeps about 15 MB of caches resident after reading
//! a few dozen apps. Nothing runs between calls.

mod apps;
mod codesig;
mod env;
mod junk;
mod leftovers;
mod native;
mod plist;
mod trash;
mod types;
mod uninstall;
mod walk;

pub use apps::{AppIndex, AppTarget, AppsEvent, IndexedApp, InstalledApp, Protection, list_apps};
pub use env::{
    Env, LastUsedReader, NoLastUsed, NoSigning, NoTrash, RunningApps, SigningReader,
    SpotlightLastUsed, StaticProcesses, StaticRunningApps, SystemRunningApps, SystemSigning,
    SystemTrash, Trasher,
};
pub use junk::{
    BIG_ITEM, CategoryAction, CategoryId, CategoryInfo, CategoryReport, CleanProgress, CleanReport,
    JunkItem, JunkReport, ScanEvent, ScanOptions, SkippedEntry, clean, scan_junk,
};
pub use leftovers::{
    Confidence, KeptItem, Leftover, LeftoverKind, LeftoverReport, find_leftovers,
    find_leftovers_for_bundle, remove_leftovers,
};
pub use native::{Process, RunningApp, SigningInfo, TrashError};
pub use types::{ItemOutcome, ItemResult, Risk, SkipReason};
pub use uninstall::{UninstallProgress, UninstallReport, UninstallStatus, uninstall};
