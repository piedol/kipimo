//! The one place anything is moved to the Trash, with the safety checks in front of it.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::env::Env;
use crate::native::{self, TrashError};
use crate::types::{ItemOutcome, ItemResult, SkipReason};

/// Where an item is allowed to live.
#[derive(Clone, Copy)]
pub(crate) enum Zone<'a> {
    /// Under the user's home folder.
    Home,
    /// An app bundle directly in one of these folders, or one subfolder down.
    AppDirs(&'a [PathBuf]),
}

/// Folders under the home that are never moved themselves, whatever a caller passes.
const PROTECTED: &[&str] = &[
    "",
    ".Trash",
    ".cargo",
    ".gradle",
    ".npm",
    "Applications",
    "Desktop",
    "Documents",
    "Downloads",
    "Library",
    "Library/Application Support",
    "Library/Application Support/MobileSync",
    "Library/Application Support/MobileSync/Backup",
    "Library/Application Scripts",
    "Library/Caches",
    "Library/Containers",
    "Library/Cookies",
    "Library/Developer",
    "Library/Developer/CoreSimulator",
    "Library/Developer/CoreSimulator/Devices",
    "Library/Developer/Xcode",
    "Library/Developer/Xcode/Archives",
    "Library/Developer/Xcode/DerivedData",
    "Library/Group Containers",
    "Library/HTTPStorages",
    "Library/LaunchAgents",
    "Library/Logs",
    "Library/Logs/DiagnosticReports",
    "Library/Mail",
    "Library/Preferences",
    "Library/Preferences/ByHost",
    "Library/Saved Application State",
    "Library/WebKit",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];

/// An absolute path with no `.` or `..` components.
pub(crate) fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

/// Checks an item before moving it: an absolute path without `.`/`..`, present, inside the
/// allowed zone once its folder's symlinks are resolved, not a protected folder, and writable.
/// Returns the resolved path, which is what gets moved (so a symlinked parent swapped after the
/// check can't redirect the move).
pub(crate) fn preflight(env: &Env, path: &Path, zone: Zone<'_>) -> Result<PathBuf, SkipReason> {
    if !is_plain_absolute(path) {
        return Err(SkipReason::NotACandidate);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| SkipReason::Missing)?;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(SkipReason::NotACandidate);
    };
    let real_parent = fs::canonicalize(parent).map_err(|_| SkipReason::Missing)?;
    let real = real_parent.join(name);
    let home = fs::canonicalize(&env.home).map_err(|_| SkipReason::OutsideHome)?;
    match zone {
        Zone::Home => {
            if !real.starts_with(&home) {
                return Err(SkipReason::OutsideHome);
            }
            if real.starts_with(home.join(".Trash"))
                || PROTECTED.iter().any(|rel| real == home.join(rel))
            {
                return Err(SkipReason::NotACandidate);
            }
        }
        Zone::AppDirs(dirs) => {
            let ok = dirs
                .iter()
                .filter_map(|d| fs::canonicalize(d).ok())
                .any(|d| real_parent == d || real_parent.parent() == Some(d.as_path()));
            if !ok {
                return Err(SkipReason::OutsideHome);
            }
            if real.extension().is_none_or(|e| e != "app") {
                return Err(SkipReason::NotACandidate);
            }
        }
    }
    let mine = meta.uid() == unsafe { libc::getuid() };
    // Removing an entry needs write access to its folder; moving a folder to a new parent also
    // needs write access to the folder itself (its ".." entry changes).
    if !native::writable(&real_parent) || (meta.is_dir() && !native::writable(&real)) {
        return Err(if mine && native::writable(&real_parent) {
            SkipReason::ReadOnly
        } else {
            SkipReason::NeedsAdmin
        });
    }
    Ok(real)
}

/// Checks and moves one item to the Trash through the environment's [`crate::Trasher`].
pub(crate) fn move_to_trash(env: &Env, path: &Path, bytes: u64, zone: Zone<'_>) -> ItemResult {
    let real = match preflight(env, path, zone) {
        Ok(real) => real,
        Err(reason) => return ItemResult::skipped(path.to_path_buf(), bytes, reason),
    };
    let outcome = match env.trash.trash(&real) {
        Ok(trash_path) => ItemOutcome::Moved { trash_path },
        // We can write to its folder (checked above), so for an app bundle a refusal is
        // macOS's App Management protection of signed apps.
        Err(TrashError::PermissionDenied(_)) if matches!(zone, Zone::AppDirs(_)) => {
            ItemOutcome::Skipped {
                reason: SkipReason::AppManagement,
            }
        }
        Err(TrashError::PermissionDenied(_)) => ItemOutcome::Skipped {
            reason: SkipReason::NeedsAdmin,
        },
        Err(TrashError::NotFound) => ItemOutcome::Skipped {
            reason: SkipReason::Missing,
        },
        Err(TrashError::Other(error)) => ItemOutcome::Failed { error },
    };
    ItemResult {
        path: path.to_path_buf(),
        bytes,
        outcome,
    }
}
