//! Result types shared by every tool.

use std::path::PathBuf;

use serde::Serialize;

/// How risky it is to clean something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Rebuilt or re-downloaded automatically; nothing of yours is lost.
    Low,
    /// Safe, but rebuilding is slow or you might want some of it.
    Medium,
    /// May be the only copy of something you care about.
    High,
}

/// Why an item was left alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkipReason {
    /// The app that owns it is running (quit it first).
    InUse { app: String },
    /// macOS relies on it (sync state, sign-in, in-flight downloads) or rebuilds it slowly.
    Essential,
    /// It's new enough to keep (a recent backup, archive or device symbols).
    Recent,
    /// Kept so the newest copy stays available (for example the newest device symbols).
    Newest,
    /// Another installed app still uses it.
    Shared { app: String },
    /// A symbolic link; never followed.
    Symlink,
    /// Lives outside the user's home (or outside the allowed app folders).
    OutsideHome,
    /// Not writable by this user; an administrator has to remove it.
    NeedsAdmin,
    /// Yours but read-only (a folder without write permission can't be moved).
    ReadOnly,
    /// Not something this tool offered (not found by the scan or the leftover finder).
    NotACandidate,
    /// Already gone.
    Missing,
    /// Shown for information only (the Trash, which Finder empties).
    ReportOnly,
    /// The app itself couldn't be moved, so its leftovers were kept.
    AppNotRemoved,
    /// Cancelled before this item was reached.
    Cancelled,
}

/// What happened to one item in a destructive action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ItemOutcome {
    /// Moved to the Trash (Finder's "Put Back" works).
    Moved { trash_path: Option<PathBuf> },
    /// Left in place on purpose.
    Skipped { reason: SkipReason },
    /// The move was attempted and failed.
    Failed { error: String },
}

/// One item's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResult {
    pub path: PathBuf,
    /// The size measured by the scan (what moving it frees, roughly).
    pub bytes: u64,
    pub outcome: ItemOutcome,
}

impl ItemResult {
    pub fn moved(&self) -> bool {
        matches!(self.outcome, ItemOutcome::Moved { .. })
    }

    pub(crate) fn skipped(path: PathBuf, bytes: u64, reason: SkipReason) -> ItemResult {
        ItemResult {
            path,
            bytes,
            outcome: ItemOutcome::Skipped { reason },
        }
    }
}
