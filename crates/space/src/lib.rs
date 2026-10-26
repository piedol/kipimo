//! Tool C: the space map. Sizes folders quickly so the UI can drill down by size, reveal items in
//! Finder (the UI's job: `NSWorkspace activateFileViewerSelectingURLs:` with an entry's `path`)
//! and move selected items to the Trash.
//!
//! # Public API
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use kipimo_space::{ScanOptions, SpaceMap, scan_targets};
//!
//! // 1. Pick a root: the home folder, the startup disk ("/") or another volume.
//! let target = &scan_targets()[0];
//! let map = SpaceMap::new(&target.path, ScanOptions::default())?;
//!
//! // 2. Scan on a background thread. `map` is cheap to clone and the UI may query it while the
//! //    scan runs (partial results). Set `cancel` to stop.
//! let cancel = AtomicBool::new(false);
//! let summary = map.scan(&cancel, |progress| {
//!     // progress.bytes, .files, .folders, .current_path, .elapsed_ms (about 10 times a second)
//! })?;
//!
//! // 3. Query.
//! let rows = map.children(map.root()); // largest first: folders, big files, "other files"
//! let top = map.largest_files(map.root(), 50);
//! let unreadable = map.skipped(); // with reasons, e.g. SkipReason::NeedsFullDiskAccess
//!
//! // 4. Act on what the user selected (moves to the Trash; "Put Back" works).
//! let results = map.move_to_trash(&[rows[0].path.clone().unwrap()]);
//! # Ok::<(), kipimo_space::SpaceError>(())
//! ```
//!
//! - [`scan_targets`] lists suggested roots ([`ScanTarget`]: home, startup disk, volumes, with
//!   size and free space). [`has_full_disk_access`] tells whether the UI should ask for Full Disk
//!   Access first (without it, protected folders are skipped and reported, never fatal).
//! - [`SpaceMap::new`]`(root, `[`ScanOptions`]`)` prepares a map; [`SpaceMap::scan`]`(&cancel,
//!   progress)` blocks until done or cancelled and returns a [`ScanSummary`]. Calling it again
//!   rescans from scratch (node ids from the previous scan become invalid).
//! - Queries (all cheap, all safe during a scan): [`SpaceMap::root`], [`SpaceMap::totals`]
//!   ([`Totals`]), [`SpaceMap::folder`] ([`FolderInfo`]), [`SpaceMap::children`] (rows of
//!   [`Entry`], sorted by size, each with `fraction_of_parent`), [`SpaceMap::path`],
//!   [`SpaceMap::parent`], [`SpaceMap::ancestors`] (breadcrumb), [`SpaceMap::find`] (path to
//!   node), [`SpaceMap::fraction_of_parent`], [`SpaceMap::largest_files`] ([`FileEntry`]) and
//!   [`SpaceMap::skipped`] ([`SkippedFolder`]).
//! - [`SpaceMap::move_to_trash`]`(&selected_paths)` returns one [`TrashResult`] per path
//!   ([`TrashOutcome`]: moved / skipped with a [`TrashSkip`] reason / failed with the error) and
//!   updates the totals in place, without a rescan.
//!
//! All result types derive `serde::Serialize`. Sizes are allocated bytes on disk (`st_blocks *
//! 512`, what `du` and Finder's "on disk" report), with each hard-linked file counted once.
//!
//! # How it scans
//!
//! - Worker threads (one per core, at most 8: APFS stops scaling there) read whole folders
//!   with `getattrlistbulk`: one system call returns name, type, flags, inode, link count and
//!   allocated size for hundreds of entries, so files never need their own `lstat`.
//! - Memory stays bounded by the number of folders, not files: the map keeps one 48-byte
//!   record per folder plus, per folder, its `files_per_folder` largest files of at least
//!   `min_listed_file_size`. All other files are only counted into their folder's totals and
//!   shown as one "other files" row. Measured: about 60 MB peak for a home folder of 5.4
//!   million files in 740,000 folders.
//! - It never follows symbolic links, so link loops are harmless, and stays on the root's
//!   volume unless `cross_volumes` is set. Other mounts, including network shares and
//!   automounts, are listed as skipped ([`SkipReason::OtherVolume`]) without being touched.
//! - The startup disk is one APFS volume group: the read-only System volume plus the Data
//!   volume, which also appears at `/System/Volumes/Data`. Firmlinks (`/Users`,
//!   `/Applications`, `/Library`, ... from `/usr/share/firmlinks`) are shown at their usual
//!   paths, and their duplicate view under `/System/Volumes/Data` is skipped
//!   ([`SkipReason::SameFolderAs`]), so nothing is counted twice.
//! - Unreadable folders are skipped and reported, never fatal: privacy-protected ones (TCC)
//!   that Full Disk Access would open as [`SkipReason::NeedsFullDiskAccess`], folders macOS
//!   protects from every app (System Integrity Protection, data vaults) as
//!   [`SkipReason::ProtectedByMacOS`], ordinary permissions as [`SkipReason::PermissionDenied`].
//!   Cloud-only iCloud folders are never downloaded ([`SkipReason::NotDownloaded`]).
//! - APFS clones: by default each clone counts its full allocated size, as `du` and Finder
//!   do, so a disk with many clones can total more than its used space (on the development Mac,
//!   `/` totals about 7% more). `ScanOptions::count_clones_once` counts shared blocks once.
//!
//! # Trash rules
//!
//! [`SpaceMap::move_to_trash`] only acts on the exact paths given, and only on items the scan
//! showed (a folder of the map, or a file in a folder that was read). It refuses: the scan root
//! itself and anything outside it; volume roots and mount points; macOS system locations
//! (`/System`, `/usr` except `/usr/local`, `/bin`, `/sbin`, `/private` except temporary folders,
//! system parts of `/Library` such as `LaunchDaemons` or `Extensions`, hidden volume folders,
//! read-only volumes); structural folders themselves (every top-level folder of the startup
//! disk, `/Users/<name>`, `~/Library` and each folder directly in it, `~/Documents`, ...);
//! anything inside an app, plug-in or other bundle or package (Kipimo never modifies apps);
//! and items already in the Trash. It uses `NSFileManager trashItemAtURL:`, which keeps
//! Finder's "Put Back". Nothing is ever deleted permanently.

use std::path::PathBuf;

mod ffi;
mod map;
mod model;
mod scan;
mod targets;
mod trash;
mod tree;

pub use map::SpaceMap;
pub use model::{
    Entry, EntryKind, FileEntry, FolderInfo, NodeId, NodeState, ScanOptions, ScanProgress,
    ScanSummary, ScanTarget, SkipReason, SkippedFolder, SpaceError, TargetKind, Totals,
    TrashOutcome, TrashResult, TrashSkip,
};
pub use targets::{has_full_disk_access, scan_targets};

#[cfg(test)]
mod tests;

// The UI shares a map between its main thread and a background scan thread.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<SpaceMap>();
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
