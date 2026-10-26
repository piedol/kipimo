//! Compact in-memory result tree. One fixed-size record per folder in chunked arenas (no huge
//! reallocations while a scan grows the tree), names in a shared byte arena, and per folder only
//! its subfolders plus its N largest files. Every other file is folded into its folder's totals.

use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::model::{
    Entry, EntryKind, FileEntry, FolderInfo, NodeId, NodeState, SkipReason, SkippedFolder, Totals,
};

pub(crate) const NONE: u32 = u32::MAX;

const STATE_PENDING: u8 = 0;
const STATE_SCANNED: u8 = 1;
const STATE_SKIPPED: u8 = 2;
const FLAG_REMOVED: u8 = 1;

/// One folder. 48 bytes; a folder's own (direct) totals are derived as its subtree totals minus
/// its live children's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Node {
    name_off: u32,
    name_len: u16,
    state: u8,
    flags: u8,
    parent: u32,
    children_start: u32,
    children_len: u32,
    files_start: u32,
    files_len: u16,
    /// Folders in this subtree (itself included) not read yet.
    pending: u32,
    /// Folders below it.
    folders: u32,
    /// Files in the whole subtree.
    files: u32,
    /// Allocated bytes of the whole subtree.
    size: u64,
}

const _: () = assert!(std::mem::size_of::<Node>() == 48);

impl Node {
    fn new(name: (u32, u16), parent: u32, state: u8) -> Node {
        Node {
            name_off: name.0,
            name_len: name.1,
            state,
            flags: 0,
            parent,
            children_start: 0,
            children_len: 0,
            files_start: 0,
            files_len: 0,
            pending: u32::from(state == STATE_PENDING),
            folders: 0,
            files: 0,
            size: 0,
        }
    }

    fn removed(&self) -> bool {
        self.flags & FLAG_REMOVED != 0
    }
}

fn add_u32(value: u32, delta: u64) -> u32 {
    u32::try_from(u64::from(value) + delta).unwrap_or(u32::MAX)
}

/// One remembered large file. 16 bytes.
#[derive(Clone, Copy, Debug)]
struct ListedFile {
    name_off: u32,
    name_len: u16,
    removed: bool,
    size: u64,
}

const CHUNK_BITS: u32 = 13;
const CHUNK: usize = 1 << CHUNK_BITS;

/// Append-only vector stored in fixed chunks so growth never copies existing records.
struct ChunkVec<T> {
    chunks: Vec<Vec<T>>,
    len: u32,
}

impl<T> ChunkVec<T> {
    fn new() -> Self {
        ChunkVec {
            chunks: Vec::new(),
            len: 0,
        }
    }

    fn push(&mut self, value: T) -> u32 {
        if self.chunks.last().is_none_or(|chunk| chunk.len() == CHUNK) {
            self.chunks.push(Vec::with_capacity(CHUNK));
        }
        if let Some(chunk) = self.chunks.last_mut() {
            chunk.push(value);
        }
        self.len += 1;
        self.len - 1
    }

    fn get(&self, index: u32) -> Option<&T> {
        self.chunks
            .get((index >> CHUNK_BITS) as usize)?
            .get(index as usize & (CHUNK - 1))
    }

    fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        self.chunks
            .get_mut((index >> CHUNK_BITS) as usize)?
            .get_mut(index as usize & (CHUNK - 1))
    }

    fn len(&self) -> u32 {
        self.len
    }

    fn reserved_bytes(&self) -> usize {
        self.chunks.len() * CHUNK * std::mem::size_of::<T>()
    }
}

const NAME_CHUNK: usize = 1 << 20;

/// Byte arena for names.
struct Names {
    chunks: Vec<Vec<u8>>,
}

impl Names {
    fn push(&mut self, name: &[u8]) -> (u32, u16) {
        let name = &name[..name.len().min(u16::MAX as usize)];
        if self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.len() + name.len() > NAME_CHUNK)
        {
            self.chunks.push(Vec::with_capacity(NAME_CHUNK));
        }
        let index = self.chunks.len() - 1;
        let chunk = &mut self.chunks[index];
        let offset = index * NAME_CHUNK + chunk.len();
        chunk.extend_from_slice(name);
        (offset as u32, name.len() as u16)
    }

    fn get(&self, offset: u32, len: u16) -> &[u8] {
        let chunk = offset as usize / NAME_CHUNK;
        let start = offset as usize % NAME_CHUNK;
        self.chunks
            .get(chunk)
            .and_then(|c| c.get(start..start + len as usize))
            .unwrap_or(&[])
    }

    fn reserved_bytes(&self) -> usize {
        self.chunks.len() * NAME_CHUNK
    }
}

/// A subfolder found while reading a folder; its name is `names[name_off..name_off + name_len]`
/// of the [`FolderRead`]. 8 bytes, so a folder with 200,000 subfolders stays cheap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildDir {
    pub name_off: u32,
    pub name_len: u16,
    /// A cloud-only placeholder: recorded as skipped, never read.
    pub dataless: bool,
    pub mount_point: bool,
    /// Flagged as a macOS data vault (`UF_DATAVAULT`).
    pub data_vault: bool,
}

/// What a worker learned from reading one folder.
pub(crate) struct FolderRead<'a> {
    pub own_size: u64,
    pub own_files: u32,
    pub names: &'a [u8],
    pub children: &'a [ChildDir],
    /// The largest files, any order.
    pub files: Vec<(&'a [u8], u64)>,
    /// Set when reading stopped partway; the folder is then marked skipped with this reason
    /// but what was read still counts.
    pub partial_error: Option<SkipReason>,
}

/// What `remove_*` took out of the tree.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Removed {
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
}

pub(crate) struct Tree {
    root_path: PathBuf,
    nodes: ChunkVec<Node>,
    files: ChunkVec<ListedFile>,
    names: Names,
    skipped: HashMap<u32, SkipReason>,
}

fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl Tree {
    pub fn new(root_path: PathBuf) -> Tree {
        let mut names = Names { chunks: Vec::new() };
        let root_name = root_path
            .file_name()
            .map(|n| n.as_bytes().to_vec())
            .unwrap_or_else(|| root_path.as_os_str().as_bytes().to_vec());
        let name = names.push(&root_name);
        let mut nodes = ChunkVec::new();
        nodes.push(Node::new(name, NONE, STATE_PENDING));
        Tree {
            root_path,
            nodes,
            files: ChunkVec::new(),
            names,
            skipped: HashMap::new(),
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// A node that exists and has not been moved to the Trash (nor has any ancestor).
    fn live(&self, id: u32) -> Option<&Node> {
        let node = self.node(id)?;
        let mut cursor = Some(node);
        while let Some(n) = cursor {
            if n.removed() {
                return None;
            }
            cursor = if n.parent == NONE {
                None
            } else {
                self.node(n.parent)
            };
        }
        Some(node)
    }

    fn name_of(&self, node: &Node) -> &[u8] {
        self.names.get(node.name_off, node.name_len)
    }

    /// Applies a change to `id` and every ancestor.
    fn propagate(&mut self, mut id: u32, mut apply: impl FnMut(&mut Node)) {
        while id != NONE {
            let Some(node) = self.nodes.get_mut(id) else {
                return;
            };
            apply(node);
            id = node.parent;
        }
    }

    /// Records a folder's contents and returns the node id of its first new child (children
    /// get consecutive ids in the order given).
    pub fn commit(&mut self, id: u32, read: FolderRead<'_>) -> u32 {
        let children_start = self.nodes.len();
        let mut pending_children: i64 = 0;
        let child_count = read.children.len() as u32;
        for child in read.children {
            let start = child.name_off as usize;
            let name_bytes = read
                .names
                .get(start..start + child.name_len as usize)
                .unwrap_or_default();
            let name = self.names.push(name_bytes);
            let state = if child.dataless {
                STATE_SKIPPED
            } else {
                pending_children += 1;
                STATE_PENDING
            };
            let child_id = self.nodes.push(Node::new(name, id, state));
            if child.dataless {
                self.skipped.insert(child_id, SkipReason::NotDownloaded);
            }
        }
        let files_start = self.files.len();
        let mut files = read.files;
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        files.truncate(u16::MAX as usize);
        for (name, size) in &files {
            let (name_off, name_len) = self.names.push(name);
            self.files.push(ListedFile {
                name_off,
                name_len,
                removed: false,
                size: *size,
            });
        }
        let state = if let Some(reason) = read.partial_error {
            self.skipped.insert(id, reason);
            STATE_SKIPPED
        } else {
            STATE_SCANNED
        };
        if let Some(node) = self.nodes.get_mut(id) {
            node.state = state;
            node.children_start = children_start;
            node.children_len = child_count;
            node.files_start = files_start;
            node.files_len = files.len() as u16;
        }
        let (size, file_count) = (read.own_size, u64::from(read.own_files));
        let pending_delta = pending_children - 1;
        self.propagate(id, |n| {
            n.size += size;
            n.files = add_u32(n.files, file_count);
            n.folders = add_u32(n.folders, u64::from(child_count));
            n.pending = (i64::from(n.pending) + pending_delta).max(0) as u32;
        });
        children_start
    }

    /// Marks a pending folder as skipped.
    pub fn skip(&mut self, id: u32, reason: SkipReason) {
        let Some(node) = self.nodes.get_mut(id) else {
            return;
        };
        if node.state != STATE_PENDING {
            return;
        }
        node.state = STATE_SKIPPED;
        self.skipped.insert(id, reason);
        self.propagate(id, |n| n.pending = n.pending.saturating_sub(1));
    }

    fn state_of(&self, id: u32, node: &Node) -> NodeState {
        match node.state {
            STATE_PENDING => NodeState::Pending,
            STATE_SCANNED => NodeState::Scanned,
            _ => NodeState::Skipped(self.skipped.get(&id).cloned().unwrap_or(SkipReason::Error {
                code: 0,
                message: String::new(),
                partial: false,
            })),
        }
    }

    /// Allocated bytes and file count directly inside a folder (not in its subfolders).
    fn own(&self, node: &Node) -> (u64, u64) {
        let (mut size, mut files) = (node.size, u64::from(node.files));
        for child_id in node.children_start..node.children_start + node.children_len {
            if let Some(child) = self.node(child_id)
                && !child.removed()
            {
                size = size.saturating_sub(child.size);
                files = files.saturating_sub(u64::from(child.files));
            }
        }
        (size, files)
    }

    /// Full path of a node (without checking whether it was removed).
    fn path_unchecked(&self, id: u32) -> PathBuf {
        let mut names: Vec<&[u8]> = Vec::new();
        let mut cursor = id;
        while let Some(node) = self.node(cursor) {
            if node.parent == NONE {
                break;
            }
            names.push(self.name_of(node));
            cursor = node.parent;
        }
        let mut path = self.root_path.clone();
        for name in names.iter().rev() {
            path.push(OsStr::from_bytes(name));
        }
        path
    }

    /// Path of a node for the scanner (the node may not have been read yet).
    pub fn scan_path(&self, id: u32) -> PathBuf {
        self.path_unchecked(id)
    }

    pub fn path(&self, id: u32) -> Option<PathBuf> {
        self.live(id).map(|_| self.path_unchecked(id))
    }

    fn fraction(part: u64, whole: u64) -> f64 {
        if whole == 0 {
            0.0
        } else {
            (part as f64 / whole as f64).clamp(0.0, 1.0)
        }
    }

    pub fn info(&self, id: u32) -> Option<FolderInfo> {
        let node = self.live(id)?;
        let parent_size = self.node(node.parent).map(|p| p.size);
        let (own_size, own_files) = self.own(node);
        Some(FolderInfo {
            node: NodeId(id),
            parent: (node.parent != NONE).then_some(NodeId(node.parent)),
            name: display(self.name_of(node)),
            path: self.path_unchecked(id),
            size: node.size,
            own_size,
            files: u64::from(node.files),
            own_files,
            folders: u64::from(node.folders),
            fraction_of_parent: parent_size.map_or(1.0, |p| Self::fraction(node.size, p)),
            state: self.state_of(id, node),
            complete: node.pending == 0,
        })
    }

    pub fn totals(&self) -> Totals {
        let root = self
            .node(0)
            .copied()
            .unwrap_or(Node::new((0, 0), NONE, STATE_PENDING));
        Totals {
            bytes: root.size,
            files: u64::from(root.files),
            folders: u64::from(root.folders),
            skipped_folders: self.skipped.len() as u64,
            complete: root.pending == 0,
        }
    }

    pub fn children(&self, id: u32) -> Vec<Entry> {
        let Some(node) = self.live(id).copied() else {
            return Vec::new();
        };
        let base = self.path_unchecked(id);
        let mut entries =
            Vec::with_capacity(node.children_len as usize + node.files_len as usize + 1);
        for child_id in node.children_start..node.children_start + node.children_len {
            let Some(child) = self.node(child_id) else {
                continue;
            };
            if child.removed() {
                continue;
            }
            let name = self.name_of(child);
            entries.push(Entry {
                kind: EntryKind::Folder,
                node: Some(NodeId(child_id)),
                name: display(name),
                path: Some(base.join(OsStr::from_bytes(name))),
                size: child.size,
                files: u64::from(child.files),
                folders: u64::from(child.folders),
                fraction_of_parent: Self::fraction(child.size, node.size),
                state: self.state_of(child_id, child),
                complete: child.pending == 0,
            });
        }
        let mut listed_bytes = 0u64;
        let mut listed_count = 0u64;
        for file_id in node.files_start..node.files_start + u32::from(node.files_len) {
            let Some(file) = self.files.get(file_id) else {
                continue;
            };
            if file.removed {
                continue;
            }
            listed_bytes += file.size;
            listed_count += 1;
            let name = self.names.get(file.name_off, file.name_len);
            entries.push(Entry {
                kind: EntryKind::File,
                node: None,
                name: display(name),
                path: Some(base.join(OsStr::from_bytes(name))),
                size: file.size,
                files: 1,
                folders: 0,
                fraction_of_parent: Self::fraction(file.size, node.size),
                state: NodeState::Scanned,
                complete: true,
            });
        }
        let (own_size, own_files) = self.own(&node);
        let other_count = own_files.saturating_sub(listed_count);
        let other_bytes = own_size.saturating_sub(listed_bytes);
        if other_count > 0 || other_bytes > 0 {
            entries.push(Entry {
                kind: EntryKind::OtherFiles,
                node: None,
                name: String::new(),
                path: None,
                size: other_bytes,
                files: other_count,
                folders: 0,
                fraction_of_parent: Self::fraction(other_bytes, node.size),
                state: NodeState::Scanned,
                complete: true,
            });
        }
        entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        entries
    }

    /// The live child of `parent` called `name`.
    fn child_named(&self, parent: u32, name: &[u8]) -> Option<u32> {
        let node = self.node(parent)?;
        (node.children_start..node.children_start + node.children_len).find(|&child_id| {
            self.node(child_id)
                .is_some_and(|child| !child.removed() && self.name_of(child) == name)
        })
    }

    /// The folder node at `path`, if it is part of this tree and not removed.
    pub fn find(&self, path: &Path) -> Option<u32> {
        let relative = path.strip_prefix(&self.root_path).ok()?;
        let mut id = 0;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return None;
            };
            id = self.child_named(id, name.as_bytes())?;
        }
        self.live(id).map(|_| id)
    }

    /// Whether a folder's contents were read (fully, or partly before an error).
    pub fn is_scanned(&self, id: u32) -> bool {
        self.live(id).is_some_and(|n| match n.state {
            STATE_SCANNED => true,
            STATE_SKIPPED => matches!(
                self.skipped.get(&id),
                Some(SkipReason::Error { partial: true, .. })
            ),
            _ => false,
        })
    }

    pub fn largest_files(&self, id: u32, limit: usize) -> Vec<FileEntry> {
        if limit == 0 || self.live(id).is_none() {
            return Vec::new();
        }
        // Min-heap of the best `limit` (size, file index, folder) seen so far.
        let mut best: BinaryHeap<std::cmp::Reverse<(u64, u32, u32)>> = BinaryHeap::new();
        let mut stack = vec![id];
        while let Some(folder) = stack.pop() {
            let Some(node) = self.node(folder) else {
                continue;
            };
            if node.removed() {
                continue;
            }
            for file_id in node.files_start..node.files_start + u32::from(node.files_len) {
                let Some(file) = self.files.get(file_id) else {
                    continue;
                };
                if file.removed {
                    continue;
                }
                if best.len() < limit {
                    best.push(std::cmp::Reverse((file.size, file_id, folder)));
                } else if best.peek().is_some_and(|w| file.size > w.0.0) {
                    best.pop();
                    best.push(std::cmp::Reverse((file.size, file_id, folder)));
                }
            }
            stack.extend(node.children_start..node.children_start + node.children_len);
        }
        let mut found: Vec<(u64, u32, u32)> = best.into_iter().map(|r| r.0).collect();
        found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        found
            .into_iter()
            .filter_map(|(size, file_id, folder)| {
                let file = self.files.get(file_id)?;
                let name = self.names.get(file.name_off, file.name_len);
                Some(FileEntry {
                    name: display(name),
                    path: self.path_unchecked(folder).join(OsStr::from_bytes(name)),
                    size,
                    folder: NodeId(folder),
                })
            })
            .collect()
    }

    pub fn skipped(&self) -> Vec<SkippedFolder> {
        let mut list: Vec<SkippedFolder> = self
            .skipped
            .iter()
            .filter(|(id, _)| self.live(**id).is_some())
            .map(|(id, reason)| SkippedFolder {
                node: NodeId(*id),
                path: self.path_unchecked(*id),
                reason: reason.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.path.cmp(&b.path));
        list
    }

    pub fn needs_full_disk_access_count(&self) -> u64 {
        self.skipped
            .values()
            .filter(|r| matches!(r, SkipReason::NeedsFullDiskAccess))
            .count() as u64
    }

    /// Takes a folder (and everything under it) out of the tree.
    pub fn remove_folder(&mut self, id: u32) -> Option<Removed> {
        let node = *self.live(id)?;
        if node.parent == NONE {
            return None;
        }
        if let Some(n) = self.nodes.get_mut(id) {
            n.flags |= FLAG_REMOVED;
        }
        let removed = Removed {
            bytes: node.size,
            files: u64::from(node.files),
            folders: u64::from(node.folders) + 1,
        };
        let folders = node.folders.saturating_add(1);
        self.propagate(node.parent, |n| {
            n.size = n.size.saturating_sub(node.size);
            n.files = n.files.saturating_sub(node.files);
            n.folders = n.folders.saturating_sub(folders);
            n.pending = n.pending.saturating_sub(node.pending);
        });
        Some(removed)
    }

    /// Takes one file of folder `parent` out of the tree. `alloc` is its allocated size and
    /// `counts_size` whether the scan credited that size here (false for an extra hard link).
    pub fn remove_file(
        &mut self,
        parent: u32,
        name: &[u8],
        alloc: u64,
        counts_size: bool,
    ) -> Removed {
        let Some(node) = self.live(parent).copied() else {
            return Removed::default();
        };
        let mut bytes = if counts_size { alloc } else { 0 };
        for file_id in node.files_start..node.files_start + u32::from(node.files_len) {
            let matches = self.files.get(file_id).is_some_and(|file| {
                !file.removed && self.names.get(file.name_off, file.name_len) == name
            });
            if let (true, Some(file)) = (matches, self.files.get_mut(file_id)) {
                file.removed = true;
                bytes = file.size;
                break;
            }
        }
        let (own_size, own_files) = self.own(&node);
        let bytes = bytes.min(own_size);
        let files = own_files.min(1);
        self.propagate(parent, |n| {
            n.size = n.size.saturating_sub(bytes);
            n.files = n.files.saturating_sub(files as u32);
        });
        Removed {
            bytes,
            files,
            folders: 0,
        }
    }

    /// Adds moved-in bytes to a scanned folder's own totals (used when the Trash folder itself
    /// is inside the scanned root).
    pub fn add_to_folder(&mut self, id: u32, added: Removed) {
        if !self.is_scanned(id) {
            return;
        }
        self.propagate(id, |n| {
            n.size += added.bytes;
            n.files = add_u32(n.files, added.files);
            n.folders = add_u32(n.folders, added.folders);
        });
    }

    /// Approximate heap memory held by the tree.
    pub fn memory_bytes(&self) -> u64 {
        let skipped = self.skipped.capacity() * (std::mem::size_of::<(u32, SkipReason)>() + 1);
        (self.nodes.reserved_bytes()
            + self.files.reserved_bytes()
            + self.names.reserved_bytes()
            + skipped) as u64
    }

    pub fn node_count(&self) -> u32 {
        self.nodes.len()
    }

    pub fn listed_file_count(&self) -> u32 {
        self.files.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(
        tree: &mut Tree,
        id: u32,
        size: u64,
        files: u32,
        children: &[&str],
        listed: &[(&str, u64)],
    ) -> u32 {
        let mut names = Vec::new();
        let mut dirs = Vec::new();
        for child in children {
            dirs.push(ChildDir {
                name_off: names.len() as u32,
                name_len: child.len() as u16,
                dataless: false,
                mount_point: false,
                data_vault: false,
            });
            names.extend_from_slice(child.as_bytes());
        }
        tree.commit(
            id,
            FolderRead {
                own_size: size,
                own_files: files,
                names: &names,
                children: &dirs,
                files: listed.iter().map(|(n, s)| (n.as_bytes(), *s)).collect(),
                partial_error: None,
            },
        )
    }

    #[test]
    fn totals_propagate_and_pending_counts_settle() {
        let mut tree = Tree::new(PathBuf::from("/r"));
        let first = read(&mut tree, 0, 100, 2, &["a", "b"], &[("big", 80)]);
        assert_eq!(first, 1);
        assert!(!tree.totals().complete);
        read(&mut tree, 1, 50, 1, &[], &[]);
        tree.skip(2, SkipReason::PermissionDenied);
        let totals = tree.totals();
        assert!(totals.complete);
        assert_eq!(totals.bytes, 150);
        assert_eq!(totals.files, 3);
        assert_eq!(totals.folders, 2);
        let children = tree.children(0);
        assert_eq!(children[0].name, "big");
        assert_eq!(children[1].name, "a");
        assert_eq!(children.last().unwrap().kind, EntryKind::Folder);
        let other = children
            .iter()
            .find(|e| e.kind == EntryKind::OtherFiles)
            .unwrap();
        assert_eq!((other.size, other.files), (20, 1));
        assert_eq!(tree.find(Path::new("/r/a")), Some(1));
        assert_eq!(tree.path(2).unwrap(), PathBuf::from("/r/b"));
    }

    #[test]
    fn removing_updates_ancestors() {
        let mut tree = Tree::new(PathBuf::from("/r"));
        read(&mut tree, 0, 10, 1, &["a"], &[]);
        read(&mut tree, 1, 90, 3, &["deep"], &[("x", 60)]);
        read(&mut tree, 2, 5, 1, &[], &[]);
        assert_eq!(tree.totals().bytes, 105);
        let removed = tree.remove_file(1, b"x", 60, true);
        assert_eq!(removed.bytes, 60);
        assert_eq!(tree.totals().bytes, 45);
        let removed = tree.remove_folder(1).unwrap();
        assert_eq!((removed.bytes, removed.files, removed.folders), (35, 3, 2));
        let totals = tree.totals();
        assert_eq!((totals.bytes, totals.files, totals.folders), (10, 1, 0));
        assert!(tree.find(Path::new("/r/a/deep")).is_none());
        assert!(tree.path(2).is_none());
    }
}
