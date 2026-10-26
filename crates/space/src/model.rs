//! The plain result types the UI reads. Sizes are allocated bytes on disk (what `du` reports:
//! `st_blocks * 512`, each hard-linked file counted once), never the logical file length.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Identifies a folder in a [`crate::SpaceMap`]. Valid until the next `scan()` of that map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

/// How to scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanOptions {
    /// Worker threads; 0 picks automatically (the number of cores, at most 8). At most 64.
    pub threads: usize,
    /// How many of each folder's largest files are remembered by name. Every other file is only
    /// counted, which keeps memory bounded on disks with millions of files.
    pub files_per_folder: usize,
    /// Files smaller than this (allocated bytes) are counted but never listed by name.
    pub min_listed_file_size: u64,
    /// Enter other mounted volumes (external disks, disk images, network shares) found inside the
    /// root. Off by default: a scan stays on the root's volume. The startup disk's System and
    /// Data volumes count as one volume either way.
    pub cross_volumes: bool,
    /// APFS clones (Finder's Duplicate, `cp -c`, package managers that clone their caches)
    /// share blocks. Off (the default): every clone counts its full allocated size, as `du` and
    /// Finder report, so totals can exceed the disk's used space. On: the first file of each
    /// clone family counts fully and the others only the bytes they alone own, which is much
    /// closer to real usage; which copy gets the full size then depends on scan order.
    pub count_clones_once: bool,
    /// How often the progress callback runs, in milliseconds.
    pub progress_interval_ms: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            threads: 0,
            files_per_folder: 12,
            min_listed_file_size: 256 * 1024,
            cross_volumes: false,
            count_clones_once: false,
            progress_interval_ms: 100,
        }
    }
}

/// Why a folder was not read. Its size counts as 0 and it is listed with this reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// macOS privacy protection (TCC) refused access: grant Kipimo Full Disk Access in
    /// System Settings > Privacy & Security to include it.
    NeedsFullDiskAccess,
    /// macOS itself protects this folder (System Integrity Protection, data vaults under
    /// `/private/var/db`, `/Library/Caches/com.apple.*`, ...). No app can read it, with or
    /// without Full Disk Access, so the UI should not suggest granting it.
    ProtectedByMacOS,
    /// Unix permissions refused access (usually a folder owned by root or another user).
    PermissionDenied,
    /// A different mounted volume (not entered unless `cross_volumes` is set). `volume_used` is
    /// that whole volume's used space, for information only; it is not added to any total.
    OtherVolume {
        fs_type: String,
        volume_used: Option<u64>,
    },
    /// The same folder is already shown at `path` (APFS firmlink: for example
    /// `/System/Volumes/Data/Users` is `/Users`), so it is not counted twice.
    SameFolderAs { path: PathBuf },
    /// A cloud-only placeholder (iCloud "Optimise Mac Storage"); reading it would download it.
    NotDownloaded,
    /// Nested deeper than the scanner's safety limit.
    TooDeep,
    /// Any other error. `partial` is true when some entries were read before it happened; those
    /// are still counted.
    Error {
        code: i32,
        message: String,
        partial: bool,
    },
}

impl SkipReason {
    /// A short sentence for the UI.
    pub fn message(&self) -> String {
        match self {
            SkipReason::NeedsFullDiskAccess => "Needs Full Disk Access".to_string(),
            SkipReason::ProtectedByMacOS => "Protected by macOS".to_string(),
            SkipReason::PermissionDenied => "No permission to read this folder".to_string(),
            SkipReason::OtherVolume { fs_type, .. } => format!("Another volume ({fs_type})"),
            SkipReason::SameFolderAs { path } => {
                format!("Same folder as {}, counted there", path.display())
            }
            SkipReason::NotDownloaded => "In iCloud, not downloaded".to_string(),
            SkipReason::TooDeep => "Nested too deeply".to_string(),
            SkipReason::Error {
                message, partial, ..
            } => {
                if *partial {
                    format!("Stopped partway: {message}")
                } else {
                    format!("Could not read: {message}")
                }
            }
        }
    }
}

/// Where a folder is in the scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    /// Not read yet (the scan is still running, or was cancelled before reaching it).
    Pending,
    /// Read. Folders below it may still be pending; see `complete`.
    Scanned,
    /// Not read, for this reason.
    Skipped(SkipReason),
}

/// What a row in a folder listing is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// A subfolder; drill down with its `node`.
    Folder,
    /// One of the folder's largest files.
    File,
    /// Every remaining file of the folder, aggregated ("1,204 smaller files").
    OtherFiles,
}

/// One row of [`crate::SpaceMap::children`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    /// For folders: the node to drill into.
    pub node: Option<NodeId>,
    /// Display name (lossy UTF-8). Empty for `OtherFiles`.
    pub name: String,
    /// Full path, for "Reveal in Finder" and "Move to Trash". `None` for `OtherFiles`.
    pub path: Option<PathBuf>,
    /// Allocated bytes.
    pub size: u64,
    /// Files inside (folders), 1 (a file), or how many files the `OtherFiles` row stands for.
    pub files: u64,
    /// Subfolders inside (folders only).
    pub folders: u64,
    /// `size` as a fraction (0.0 to 1.0) of the parent folder's size.
    pub fraction_of_parent: f64,
    /// Folders: pending, scanned or skipped. Files and `OtherFiles`: always `Scanned`.
    pub state: NodeState,
    /// True when this row's size is final (everything under it has been read or skipped).
    pub complete: bool,
}

/// Details of one folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderInfo {
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub name: String,
    pub path: PathBuf,
    /// Allocated bytes of everything under it.
    pub size: u64,
    /// Allocated bytes of the files directly inside it.
    pub own_size: u64,
    pub files: u64,
    pub own_files: u64,
    pub folders: u64,
    pub fraction_of_parent: f64,
    pub state: NodeState,
    pub complete: bool,
}

/// A file found by [`crate::SpaceMap::largest_files`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    /// Allocated bytes.
    pub size: u64,
    /// The folder that contains it.
    pub folder: NodeId,
}

/// A folder that was not read, from [`crate::SpaceMap::skipped`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFolder {
    pub node: NodeId,
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// Totals for the whole scan root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Allocated bytes.
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    pub skipped_folders: u64,
    /// True once every folder has been read or skipped.
    pub complete: bool,
}

/// Streaming progress, passed to the `scan` callback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanProgress {
    /// Allocated bytes counted so far.
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    pub skipped_folders: u64,
    /// A folder being read right now.
    pub current_path: Option<PathBuf>,
    pub elapsed_ms: u64,
    /// True on the last callback.
    pub finished: bool,
}

/// What `scan` returns when it ends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanSummary {
    pub root: PathBuf,
    pub totals: Totals,
    /// Folders skipped because Kipimo lacks Full Disk Access: when non-zero, the UI can offer to
    /// open System Settings > Privacy & Security > Full Disk Access.
    pub needs_full_disk_access: u64,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    /// Files plus folders per second.
    pub items_per_second: f64,
    pub threads: usize,
    /// Approximate memory held by the result tree.
    pub tree_bytes: u64,
}

/// Why `move_to_trash` left an item alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrashSkip {
    /// Not inside the scanned folder.
    OutsideScannedRoot,
    /// The scanned folder itself.
    ScanRoot,
    /// A macOS system location, a volume, or a folder macOS needs (like `~/Library` itself).
    ProtectedLocation,
    /// Inside an app or other bundle: Kipimo never modifies apps.
    InsideBundle,
    /// Already in the Trash.
    AlreadyInTrash,
    /// Not part of this scan's results (or already moved).
    NotInScan,
    /// No longer exists.
    NotFound,
    /// Inside another item of the same request, which is moved instead.
    InsideAnotherSelectedItem,
    /// The path is relative or contains `..`.
    InvalidPath,
    /// A scan of this map is running; cancel it or let it finish first.
    ScanRunning,
}

impl TrashSkip {
    /// A short sentence for the UI.
    pub fn message(&self) -> &'static str {
        match self {
            TrashSkip::OutsideScannedRoot => "Not inside the scanned folder",
            TrashSkip::ScanRoot => "This is the scanned folder itself",
            TrashSkip::ProtectedLocation => "macOS needs this location",
            TrashSkip::InsideBundle => "Inside an app: Kipimo never modifies apps",
            TrashSkip::AlreadyInTrash => "Already in the Trash",
            TrashSkip::NotInScan => "Not part of this scan",
            TrashSkip::NotFound => "No longer exists",
            TrashSkip::InsideAnotherSelectedItem => "Moved with its enclosing folder",
            TrashSkip::InvalidPath => "Not a valid full path",
            TrashSkip::ScanRunning => "Wait for the scan to finish",
        }
    }
}

/// What happened to one selected item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrashOutcome {
    /// Moved to the Trash; Finder's "Put Back" works. `trashed_to` is where it is now.
    Moved {
        trashed_to: Option<PathBuf>,
        /// Allocated bytes it took up in the scanned folder.
        bytes: u64,
        files: u64,
    },
    Skipped(TrashSkip),
    Failed {
        error: String,
    },
}

/// Per-item result of [`crate::SpaceMap::move_to_trash`], in the order given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashResult {
    pub path: PathBuf,
    pub outcome: TrashOutcome,
}

/// Kind of a suggested scan root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetKind {
    Home,
    StartupDisk,
    Volume,
}

/// A suggested place to scan, from [`crate::scan_targets`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanTarget {
    pub kind: TargetKind,
    pub name: String,
    pub path: PathBuf,
    /// Volume size and free space (for the home folder: its volume's).
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub local: bool,
}

/// Errors from creating or running a scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpaceError {
    /// The root does not exist or is not a folder.
    NotAFolder(PathBuf),
    /// The root could not be resolved (`realpath` failed).
    Io { path: PathBuf, message: String },
    /// `scan` was called while a scan of the same map, or a move to the Trash, is running.
    Busy,
}

impl std::fmt::Display for SpaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpaceError::NotAFolder(path) => write!(f, "{} is not a folder", path.display()),
            SpaceError::Io { path, message } => write!(f, "{}: {message}", path.display()),
            SpaceError::Busy => write!(f, "a scan or a move to the Trash is running"),
        }
    }
}

impl std::error::Error for SpaceError {}
