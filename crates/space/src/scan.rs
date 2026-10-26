//! The parallel scanner. Worker threads take folders from a shared stack (depth first, so the
//! queue stays small), read each with one `getattrlistbulk` loop, and commit the result to the
//! shared tree under a short write lock. The calling thread reports progress and watches the
//! cancellation flag.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::ffi::{self, BulkReader, Dir, Kind, SF_DATALESS, UF_DATAVAULT, Volume};
use crate::model::{ScanOptions, ScanProgress, SkipReason};
use crate::tree::{ChildDir, FolderRead, Tree};

/// Safety net against pathological nesting (the scanner never follows symbolic links, so a
/// loop cannot form, but a hostile tree could still be absurdly deep).
const MAX_DEPTH: u32 = 4096;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write_tree(tree: &RwLock<Tree>) -> std::sync::RwLockWriteGuard<'_, Tree> {
    tree.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn read_tree(tree: &RwLock<Tree>) -> std::sync::RwLockReadGuard<'_, Tree> {
    tree.read().unwrap_or_else(PoisonError::into_inner)
}

/// A folder waiting to be read. Its path is rebuilt from the tree when it is taken, so a wide
/// folder with 200,000 subfolders queues 12-byte jobs instead of 200,000 path strings.
struct Job {
    node: u32,
    depth: u32,
    mount_point: bool,
    data_vault: bool,
}

struct Queue {
    jobs: Vec<Job>,
    active: usize,
    done: bool,
}

/// Volumes and devices the scan may enter.
struct Devices {
    allowed: RwLock<HashSet<u64>>,
    mounts: HashMap<PathBuf, Volume>,
    /// `(device, inode)` of firmlinked folders inside the root, mapped to their firmlink path.
    firmlinks: HashMap<(u64, u64), PathBuf>,
}

struct Ctx<'a> {
    tree: &'a RwLock<Tree>,
    options: &'a ScanOptions,
    cancel: &'a AtomicBool,
    queue: Mutex<Queue>,
    work: Condvar,
    finished: Condvar,
    devices: Devices,
    /// Hard-linked files already counted: inodes by device.
    hard_links: Mutex<HashMap<u64, HashSet<u64>>>,
    /// With `count_clones_once`: APFS clone families already counted, by device.
    clones: Mutex<HashMap<u64, HashSet<u64>>>,
    /// Whether Kipimo has Full Disk Access, and the home folder: looked up on the first privacy
    /// denial only.
    privacy: OnceLock<(Option<bool>, Option<PathBuf>)>,
    bytes: AtomicU64,
    files: AtomicU64,
    folders: AtomicU64,
    skipped: AtomicU64,
    current: AtomicU32,
}

/// Per-worker scratch space, reused across folders.
#[derive(Default)]
struct Scratch {
    names: Vec<u8>,
    dirs: Vec<ChildDir>,
    /// Min-heap of the folder's largest files: (size, name offset, name length).
    top: BinaryHeap<std::cmp::Reverse<(u64, usize, usize)>>,
}

impl Scratch {
    fn clear(&mut self) {
        // Do not keep megabytes around after one enormous folder.
        if self.names.capacity() > 4 << 20 {
            self.names = Vec::new();
        }
        if self.dirs.capacity() > 1 << 16 {
            self.dirs = Vec::new();
        }
        self.names.clear();
        self.dirs.clear();
        self.top.clear();
    }

    fn push_name(&mut self, name: &[u8]) -> (usize, usize) {
        let offset = self.names.len();
        self.names.extend_from_slice(name);
        (offset, name.len())
    }

    fn offer_file(&mut self, name: &[u8], size: u64, limit: usize) {
        if self.top.len() < limit {
            let (offset, len) = self.push_name(name);
            self.top.push(std::cmp::Reverse((size, offset, len)));
        } else if self.top.peek().is_some_and(|smallest| size > smallest.0.0) {
            self.top.pop();
            let (offset, len) = self.push_name(name);
            self.top.push(std::cmp::Reverse((size, offset, len)));
        }
    }
}

fn reason_for(err: &io::Error, partial: bool) -> SkipReason {
    if ffi::is_privacy_denial(err) {
        // Refined by `Ctx::privacy_reason` where the path is known.
        SkipReason::NeedsFullDiskAccess
    } else if err.raw_os_error() == Some(libc::EACCES) && !partial {
        SkipReason::PermissionDenied
    } else if ffi::is_dataless_error(err) {
        SkipReason::NotDownloaded
    } else {
        SkipReason::Error {
            code: err.raw_os_error().unwrap_or(0),
            message: err.to_string(),
            partial,
        }
    }
}

/// See [`Ctx::refine`].
fn classify_denial(
    has_access: Option<bool>,
    data_vault: bool,
    path: &Path,
    home: Option<&Path>,
) -> SkipReason {
    let user_data = path.starts_with("/Users")
        || path.starts_with("/Volumes")
        || home.is_some_and(|home| path.starts_with(home))
        || path == Path::new("/Library/Application Support/com.apple.TCC");
    if has_access == Some(true) || data_vault || !user_data {
        SkipReason::ProtectedByMacOS
    } else {
        SkipReason::NeedsFullDiskAccess
    }
}

impl Ctx<'_> {
    fn next_job(&self) -> Option<Job> {
        let mut queue = lock(&self.queue);
        loop {
            if queue.done {
                return None;
            }
            if self.cancel.load(Ordering::Relaxed) {
                queue.done = true;
                self.work.notify_all();
                self.finished.notify_all();
                return None;
            }
            if let Some(job) = queue.jobs.pop() {
                queue.active += 1;
                return Some(job);
            }
            if queue.active == 0 {
                queue.done = true;
                self.work.notify_all();
                self.finished.notify_all();
                return None;
            }
            queue = self
                .work
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn finish_job(&self, new_jobs: Vec<Job>) {
        let mut queue = lock(&self.queue);
        let added = new_jobs.len();
        queue.jobs.extend(new_jobs);
        queue.active -= 1;
        if queue.active == 0 && queue.jobs.is_empty() {
            queue.done = true;
            self.work.notify_all();
            self.finished.notify_all();
        } else if added > 1 {
            self.work.notify_all();
        } else if added == 1 {
            self.work.notify_one();
        }
    }

    fn stop(&self) {
        let mut queue = lock(&self.queue);
        queue.done = true;
        self.work.notify_all();
        self.finished.notify_all();
    }

    fn skip(&self, node: u32, reason: SkipReason) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
        write_tree(self.tree).skip(node, reason);
    }

    fn device_allowed(&self, dev: u64) -> bool {
        self.devices
            .allowed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&dev)
    }

    fn other_volume(&self, path: &Path, dir: Option<&Dir>) -> SkipReason {
        let volume = self
            .devices
            .mounts
            .get(path)
            .cloned()
            .or_else(|| dir.and_then(Dir::volume));
        SkipReason::OtherVolume {
            fs_type: volume
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |v| v.fs_type.clone()),
            volume_used: volume.map(|v| v.used),
        }
    }

    /// Tells a privacy denial (`EPERM`) that Full Disk Access would lift apart from one that
    /// nothing lifts. With Full Disk Access granted, every remaining denial is macOS's own
    /// protection. Without it, privacy-protected data lives in home folders and on other volumes
    /// (Mail, Messages, Safari, other apps' containers, removable and network volumes), while
    /// denials elsewhere (`/private/var/db`, `/System`, `/Library/Caches/com.apple.*`) come from
    /// System Integrity Protection and data vaults.
    fn refine(&self, reason: SkipReason, path: &Path, job: &Job) -> SkipReason {
        if reason != SkipReason::NeedsFullDiskAccess {
            return reason;
        }
        let (has_access, home) = self.privacy.get_or_init(|| {
            (
                crate::has_full_disk_access(),
                ffi::home_dir().and_then(|h| ffi::real_path(&h).ok()),
            )
        });
        classify_denial(*has_access, job.data_vault, path, home.as_deref())
    }

    /// Whether this hard-linked inode is seen for the first time (and so should be counted).
    fn first_link(&self, dev: u64, ino: u64) -> bool {
        lock(&self.hard_links).entry(dev).or_default().insert(ino)
    }

    /// Whether this APFS clone family is seen for the first time.
    fn first_clone(&self, dev: u64, clone_id: u64) -> bool {
        lock(&self.clones).entry(dev).or_default().insert(clone_id)
    }

    /// Reads one folder and returns the subfolders to read next.
    fn process(&self, reader: &mut BulkReader, scratch: &mut Scratch, job: Job) -> Vec<Job> {
        self.current.store(job.node, Ordering::Relaxed);
        let path = read_tree(self.tree).scan_path(job.node);
        if job.depth > MAX_DEPTH {
            self.skip(job.node, SkipReason::TooDeep);
            return Vec::new();
        }
        if job.mount_point {
            // Decide from the mount table before touching the mount itself: opening an
            // unresponsive network share or an automount trigger can block for a long time.
            if let Some(volume) = self.devices.mounts.get(&path) {
                let enterable =
                    !volume.never_enter() && (volume.local || self.options.cross_volumes);
                if !enterable {
                    self.skip(job.node, self.other_volume(&path, None));
                    return Vec::new();
                }
            }
        }
        let dir = match Dir::open(&path) {
            Ok(dir) => dir,
            Err(err) => {
                self.skip(job.node, self.refine(reason_for(&err, false), &path, &job));
                return Vec::new();
            }
        };
        let stat = match dir.stat() {
            Ok(stat) => stat,
            Err(err) => {
                self.skip(job.node, self.refine(reason_for(&err, false), &path, &job));
                return Vec::new();
            }
        };
        if !self.device_allowed(stat.dev) {
            let volume = self
                .devices
                .mounts
                .get(&path)
                .cloned()
                .or_else(|| dir.volume());
            let enterable =
                self.options.cross_volumes && volume.as_ref().is_none_or(|v| !v.never_enter());
            if !enterable {
                self.skip(job.node, self.other_volume(&path, Some(&dir)));
                return Vec::new();
            }
            self.devices
                .allowed
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(stat.dev);
        }
        if let Some(shown_at) = self.devices.firmlinks.get(&(stat.dev, stat.ino))
            && *shown_at != path
        {
            self.skip(
                job.node,
                SkipReason::SameFolderAs {
                    path: shown_at.clone(),
                },
            );
            return Vec::new();
        }

        scratch.clear();
        let limit = self.options.files_per_folder;
        let min_listed = self.options.min_listed_file_size.max(1);
        let mut own_size = stat.alloc;
        let mut own_files: u32 = 0;
        let mut read_any = false;
        let clones_once = self.options.count_clones_once;
        let result = reader.read_all(
            &dir,
            clones_once,
            || self.cancel.load(Ordering::Relaxed),
            |entry| {
                read_any = true;
                if entry.error != 0 {
                    return;
                }
                if entry.kind == Kind::Dir {
                    let (offset, len) = scratch.push_name(entry.name);
                    scratch.dirs.push(ChildDir {
                        name_off: offset as u32,
                        name_len: len as u16,
                        dataless: entry.flags & SF_DATALESS != 0,
                        mount_point: entry.mount_point,
                        data_vault: entry.flags & UF_DATAVAULT != 0,
                    });
                    return;
                }
                own_files = own_files.saturating_add(1);
                let mut counted = if entry.link_count > 1 && entry.kind != Kind::Symlink {
                    if self.first_link(stat.dev, entry.file_id) {
                        entry.alloc
                    } else {
                        0
                    }
                } else {
                    entry.alloc
                };
                // Later members of an APFS clone family count only the bytes they own.
                if clones_once
                    && counted > 0
                    && entry.may_share_blocks
                    && entry.clone_id != 0
                    && !self.first_clone(stat.dev, entry.clone_id)
                {
                    counted = entry.private_size.unwrap_or(counted).min(counted);
                }
                own_size += counted;
                if limit > 0 && counted >= min_listed {
                    scratch.offer_file(entry.name, counted, limit);
                }
            },
        );
        let partial_error = match result {
            Ok(true) => None,
            // Cancelled partway through this folder: leave it pending.
            Ok(false) => return Vec::new(),
            Err(err) if !read_any => {
                self.skip(job.node, self.refine(reason_for(&err, false), &path, &job));
                return Vec::new();
            }
            Err(err) => Some(self.refine(reason_for(&err, true), &path, &job)),
        };
        if partial_error.is_some() {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }

        let names = &scratch.names;
        let children = &scratch.dirs;
        let dataless_count = children.iter().filter(|c| c.dataless).count() as u64;
        let files = scratch
            .top
            .iter()
            .map(|r| (&names[r.0.1..r.0.1 + r.0.2], r.0.0))
            .collect();
        let child_count = children.len();
        let first_child = write_tree(self.tree).commit(
            job.node,
            FolderRead {
                own_size,
                own_files,
                names,
                children,
                files,
                partial_error,
            },
        );
        self.bytes.fetch_add(own_size, Ordering::Relaxed);
        self.files
            .fetch_add(u64::from(own_files), Ordering::Relaxed);
        self.folders
            .fetch_add(child_count as u64, Ordering::Relaxed);
        self.skipped.fetch_add(dataless_count, Ordering::Relaxed);

        children
            .iter()
            .enumerate()
            .filter(|(_, child)| !child.dataless)
            .map(|(i, child)| Job {
                node: first_child + i as u32,
                depth: job.depth + 1,
                mount_point: child.mount_point,
                data_vault: child.data_vault,
            })
            .collect()
    }

    fn worker(&self) {
        ffi::disable_dataless_materialization();
        let mut reader = BulkReader::new();
        let mut scratch = Scratch::default();
        while let Some(job) = self.next_job() {
            let node = job.node;
            // A bug while reading one folder must not hang the whole scan: record it and go on.
            let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.process(&mut reader, &mut scratch, job)
            }))
            .unwrap_or_else(|_| {
                self.skip(
                    node,
                    SkipReason::Error {
                        code: 0,
                        message: "internal error".to_string(),
                        partial: false,
                    },
                );
                Vec::new()
            });
            self.finish_job(next);
        }
    }

    fn progress(&self, started: Instant, finished: bool) -> ScanProgress {
        let current_path = if finished {
            None
        } else {
            read_tree(self.tree).path(self.current.load(Ordering::Relaxed))
        };
        ScanProgress {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            folders: self.folders.load(Ordering::Relaxed),
            skipped_folders: self.skipped.load(Ordering::Relaxed),
            current_path,
            elapsed_ms: started.elapsed().as_millis() as u64,
            finished,
        }
    }
}

pub(crate) fn thread_count(options: &ScanOptions) -> usize {
    if options.threads > 0 {
        return options.threads.min(64);
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        // Measured on an 18-core M-series Mac: APFS directory reads stop scaling at about 8
        // threads; more only add kernel contention.
        .clamp(2, 8)
}

/// Outcome of [`run`].
pub(crate) struct RunResult {
    pub cancelled: bool,
    pub threads: usize,
}

/// Scans `tree`'s root into `tree` (which must be fresh). Blocks until done or cancelled.
pub(crate) fn run(
    tree: &RwLock<Tree>,
    options: &ScanOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ScanProgress),
) -> RunResult {
    let started = Instant::now();
    let root = read_tree(tree).root_path().to_path_buf();
    let threads = thread_count(options);
    let root_dev = ffi::dev_ino(&root).map(|(dev, _)| dev);
    let ctx = Ctx {
        tree,
        options,
        cancel,
        queue: Mutex::new(Queue {
            jobs: vec![Job {
                node: 0,
                depth: 0,
                mount_point: false,
                data_vault: false,
            }],
            active: 0,
            done: false,
        }),
        work: Condvar::new(),
        finished: Condvar::new(),
        devices: Devices {
            // The startup disk's System and Data volumes report the same `st_dev` (they form
            // one APFS volume group), so one entry covers both.
            allowed: RwLock::new(root_dev.into_iter().collect()),
            mounts: ffi::mounts_by_path(),
            firmlinks: ffi::firmlinks_within(&root),
        },
        privacy: OnceLock::new(),
        hard_links: Mutex::new(HashMap::new()),
        clones: Mutex::new(HashMap::new()),
        bytes: AtomicU64::new(0),
        files: AtomicU64::new(0),
        folders: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        current: AtomicU32::new(0),
    };
    let interval = Duration::from_millis(options.progress_interval_ms.max(10));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| ctx.worker());
        }
        loop {
            {
                let queue = lock(&ctx.queue);
                let (queue, _) = ctx
                    .finished
                    .wait_timeout_while(queue, interval, |q| !q.done)
                    .unwrap_or_else(PoisonError::into_inner);
                if queue.done {
                    break;
                }
            }
            if cancel.load(Ordering::Relaxed) {
                ctx.stop();
                break;
            }
            progress(&ctx.progress(started, false));
        }
    });
    let cancelled = cancel.load(Ordering::Relaxed) && !read_tree(tree).totals().complete;
    progress(&ctx.progress(started, true));
    RunResult { cancelled, threads }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_denials_are_classified() {
        let home = Some(Path::new("/Users/me"));
        let mail = Path::new("/Users/me/Library/Mail");
        let vault = Path::new("/private/var/db/Spotlight");
        let usb = Path::new("/Volumes/Stick/photos");
        assert_eq!(
            classify_denial(Some(false), false, mail, home),
            SkipReason::NeedsFullDiskAccess
        );
        assert_eq!(
            classify_denial(None, false, usb, home),
            SkipReason::NeedsFullDiskAccess
        );
        assert_eq!(
            classify_denial(Some(false), false, vault, home),
            SkipReason::ProtectedByMacOS
        );
        assert_eq!(
            classify_denial(Some(false), true, mail, home),
            SkipReason::ProtectedByMacOS
        );
        // With Full Disk Access granted, nothing left is the user's to unlock.
        assert_eq!(
            classify_denial(Some(true), false, mail, home),
            SkipReason::ProtectedByMacOS
        );
    }
}
