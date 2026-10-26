//! [`SpaceMap`]: one scan root, its result tree, and the queries the UI needs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::ffi;
use crate::model::{
    Entry, FileEntry, FolderInfo, NodeId, ScanOptions, ScanProgress, ScanSummary, SkippedFolder,
    SpaceError, Totals,
};
use crate::scan::{self, read_tree, write_tree};
use crate::tree::Tree;

pub(crate) struct Shared {
    pub root: PathBuf,
    pub options: ScanOptions,
    pub tree: RwLock<Tree>,
    pub scanning: AtomicBool,
}

/// A space map of one folder or volume. Cheap to clone (all clones share the same results), and
/// safe to query from the UI thread while `scan` runs on a background thread: queries see the
/// partial results so far.
#[derive(Clone)]
pub struct SpaceMap {
    pub(crate) shared: Arc<Shared>,
}

/// Clears the "scanning" flag even if the scan panics.
struct ScanningGuard<'a>(&'a AtomicBool);

impl Drop for ScanningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SpaceMap {
    /// Prepares a map of `root` (a folder, the home folder, or a volume such as `/`). Resolves
    /// symbolic links in the path; does not scan yet.
    pub fn new(root: impl AsRef<Path>, options: ScanOptions) -> Result<SpaceMap, SpaceError> {
        let given = root.as_ref();
        let root = ffi::real_path(given).map_err(|err| SpaceError::Io {
            path: given.to_path_buf(),
            message: err.to_string(),
        })?;
        match ffi::lstat(&root) {
            Ok(stat) if stat.is_dir => {}
            _ => return Err(SpaceError::NotAFolder(root)),
        }
        Ok(SpaceMap {
            shared: Arc::new(Shared {
                tree: RwLock::new(Tree::new(root.clone())),
                root,
                options,
                scanning: AtomicBool::new(false),
            }),
        })
    }

    /// The scanned root (symbolic links resolved).
    pub fn root_path(&self) -> &Path {
        &self.shared.root
    }

    pub fn options(&self) -> &ScanOptions {
        &self.shared.options
    }

    /// The root folder's node.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// True while `scan` is running.
    pub fn is_scanning(&self) -> bool {
        self.shared.scanning.load(Ordering::Acquire)
    }

    /// Scans (or rescans from scratch) the root. Blocking: call it on a background thread.
    /// `progress` runs on this thread about every `progress_interval_ms` and once more at the end
    /// (`finished: true`). Setting `cancel` stops the scan within a few milliseconds; whatever was
    /// read so far stays queryable and unread folders stay `Pending`. Node ids from an earlier
    /// scan of this map become invalid.
    pub fn scan(
        &self,
        cancel: &AtomicBool,
        mut progress: impl FnMut(&ScanProgress),
    ) -> Result<ScanSummary, SpaceError> {
        if self
            .shared
            .scanning
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SpaceError::Busy);
        }
        let _guard = ScanningGuard(&self.shared.scanning);
        let started = Instant::now();
        *write_tree(&self.shared.tree) = Tree::new(self.shared.root.clone());
        let result = scan::run(
            &self.shared.tree,
            &self.shared.options,
            cancel,
            &mut progress,
        );
        let elapsed = started.elapsed();
        let tree = read_tree(&self.shared.tree);
        let totals = tree.totals();
        let items = totals.files + totals.folders + 1;
        Ok(ScanSummary {
            root: self.shared.root.clone(),
            totals,
            needs_full_disk_access: tree.needs_full_disk_access_count(),
            cancelled: result.cancelled,
            elapsed_ms: elapsed.as_millis() as u64,
            items_per_second: items as f64 / elapsed.as_secs_f64().max(1e-6),
            threads: result.threads,
            tree_bytes: tree.memory_bytes(),
        })
    }

    /// Totals for the whole root so far.
    pub fn totals(&self) -> Totals {
        read_tree(&self.shared.tree).totals()
    }

    /// Details of one folder.
    pub fn folder(&self, node: NodeId) -> Option<FolderInfo> {
        read_tree(&self.shared.tree).info(node.0)
    }

    /// The rows of a folder, largest first: its subfolders, its largest files, and one
    /// `OtherFiles` row for the rest of its files. Each row carries `fraction_of_parent`.
    pub fn children(&self, node: NodeId) -> Vec<Entry> {
        read_tree(&self.shared.tree).children(node.0)
    }

    /// Full path of a folder.
    pub fn path(&self, node: NodeId) -> Option<PathBuf> {
        read_tree(&self.shared.tree).path(node.0)
    }

    /// The parent folder (`None` for the root).
    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.folder(node).and_then(|info| info.parent)
    }

    /// The folders from the root down to `node` (for a breadcrumb), root first.
    pub fn ancestors(&self, node: NodeId) -> Vec<NodeId> {
        let tree = read_tree(&self.shared.tree);
        let mut chain = Vec::new();
        let mut cursor = Some(node);
        while let Some(id) = cursor {
            let Some(info) = tree.info(id.0) else {
                break;
            };
            chain.push(id);
            cursor = info.parent;
        }
        chain.reverse();
        chain
    }

    /// `node`'s size as a fraction (0.0 to 1.0) of its parent's (1.0 for the root).
    pub fn fraction_of_parent(&self, node: NodeId) -> Option<f64> {
        self.folder(node).map(|info| info.fraction_of_parent)
    }

    /// The folder at `path`, if it is part of this scan.
    pub fn find(&self, path: impl AsRef<Path>) -> Option<NodeId> {
        read_tree(&self.shared.tree).find(path.as_ref()).map(NodeId)
    }

    /// The `limit` largest files anywhere under `node`, largest first. Exact for the files the
    /// map remembers: each folder's `files_per_folder` largest files of at least
    /// `min_listed_file_size` bytes.
    pub fn largest_files(&self, node: NodeId, limit: usize) -> Vec<FileEntry> {
        read_tree(&self.shared.tree).largest_files(node.0, limit)
    }

    /// Every folder that was not read, with the reason, sorted by path.
    pub fn skipped(&self) -> Vec<SkippedFolder> {
        read_tree(&self.shared.tree).skipped()
    }

    /// Diagnostics: folders and listed files held in memory, and the approximate bytes used.
    pub fn memory_stats(&self) -> (u32, u32, u64) {
        let tree = read_tree(&self.shared.tree);
        (
            tree.node_count(),
            tree.listed_file_count(),
            tree.memory_bytes(),
        )
    }
}
