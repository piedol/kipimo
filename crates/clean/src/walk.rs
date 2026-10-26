//! Parallel, cancellable size walker.
//!
//! Sizes a list of roots in one pass with a small thread pool. Sizes are *allocated* bytes (what
//! the files really take on disk), read in bulk with `getattrlistbulk(2)`; a `readdir` + `lstat`
//! fallback covers file systems that don't support it. Symbolic links are never followed, mount
//! points are never crossed, and a file with several hard links is counted once for the whole
//! pass. Directories are processed depth first from a shared queue, so memory stays bounded by the
//! tree depth rather than its size.

use std::collections::{HashSet, VecDeque};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Totals for one measured root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SizeTotals {
    /// Allocated bytes.
    pub bytes: u64,
    /// Regular files and links counted.
    pub files: u64,
    /// Folders that couldn't be read (permission denied and similar).
    pub unreadable: u64,
    /// Part of `bytes` in files with more than one hard link.
    pub linked: u64,
}

/// Events streamed while measuring.
pub(crate) enum WalkEvent {
    /// Running totals across every root, sent about every 100 ms.
    Tick { bytes: u64, files: u64 },
    /// One root finished (its totals are final).
    Done { index: usize, totals: SizeTotals },
}

/// The result of [`measure`].
pub(crate) struct WalkOutcome {
    pub totals: Vec<SizeTotals>,
    pub cancelled: bool,
    /// The hard-link table hit its cap, so later hard links may be counted more than once.
    pub hardlink_cap_hit: bool,
}

/// At most this many hard-linked inodes are remembered per pass (8 bytes each, about 4 MB).
const HARDLINK_CAP: usize = 500_000;
const SHARDS: usize = 16;
const BULK_BUF: usize = 64 * 1024;
const TICK: Duration = Duration::from_millis(100);
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

struct Job {
    item: usize,
    path: PathBuf,
    dev: u64,
}

struct Queue {
    jobs: VecDeque<Job>,
    active: usize,
    finished: bool,
}

struct ItemState {
    bytes: AtomicU64,
    linked: AtomicU64,
    files: AtomicU64,
    unreadable: AtomicU64,
    pending: AtomicUsize,
}

impl ItemState {
    fn totals(&self) -> SizeTotals {
        SizeTotals {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            unreadable: self.unreadable.load(Ordering::Relaxed),
            linked: self.linked.load(Ordering::Relaxed),
        }
    }
}

struct Shared<'a> {
    queue: Mutex<Queue>,
    cv: Condvar,
    items: Vec<ItemState>,
    links: Vec<Mutex<HashSet<u64>>>,
    link_count: AtomicUsize,
    cap_hit: AtomicBool,
    bytes: AtomicU64,
    files: AtomicU64,
    cancel: &'a AtomicBool,
}

impl Shared<'_> {
    /// Adds one file. Returns false if it was already counted through another hard link.
    fn count_file(&self, item: usize, dev: u64, ino: u64, nlink: u64, alloc: u64) {
        if nlink > 1 && !self.first_link(dev, ino) {
            return;
        }
        let it = &self.items[item];
        it.bytes.fetch_add(alloc, Ordering::Relaxed);
        if nlink > 1 {
            it.linked.fetch_add(alloc, Ordering::Relaxed);
        }
        it.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(alloc, Ordering::Relaxed);
        self.files.fetch_add(1, Ordering::Relaxed);
    }

    fn first_link(&self, dev: u64, ino: u64) -> bool {
        let key = ino ^ (dev << 48) ^ (dev >> 16);
        let shard = &self.links[(key as usize ^ (key >> 32) as usize) % SHARDS];
        let mut set = shard.lock().unwrap_or_else(|e| e.into_inner());
        if set.contains(&key) {
            return false;
        }
        if self.link_count.load(Ordering::Relaxed) >= HARDLINK_CAP {
            self.cap_hit.store(true, Ordering::Relaxed);
            return true;
        }
        set.insert(key);
        self.link_count.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/// Measures `roots` in parallel. `on_event` runs on the calling thread.
pub(crate) fn measure(
    roots: &[PathBuf],
    threads: usize,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(WalkEvent),
) -> WalkOutcome {
    let items: Vec<ItemState> = roots
        .iter()
        .map(|_| ItemState {
            bytes: AtomicU64::new(0),
            linked: AtomicU64::new(0),
            files: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
            pending: AtomicUsize::new(0),
        })
        .collect();
    let shared = Shared {
        queue: Mutex::new(Queue {
            jobs: VecDeque::new(),
            active: 0,
            finished: false,
        }),
        cv: Condvar::new(),
        items,
        links: (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
        link_count: AtomicUsize::new(0),
        cap_hit: AtomicBool::new(false),
        bytes: AtomicU64::new(0),
        files: AtomicU64::new(0),
        cancel,
    };

    // Seed the queue. Files and missing roots are finished right away.
    let mut immediate = Vec::new();
    {
        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        for (index, root) in roots.iter().enumerate() {
            match std::fs::symlink_metadata(root) {
                Ok(meta) if meta.file_type().is_dir() => {
                    shared.items[index].pending.store(1, Ordering::Relaxed);
                    q.jobs.push_back(Job {
                        item: index,
                        path: root.clone(),
                        dev: meta.dev(),
                    });
                }
                Ok(meta) => {
                    shared.count_file(
                        index,
                        meta.dev(),
                        meta.ino(),
                        meta.nlink(),
                        meta.blocks() * 512,
                    );
                    immediate.push(index);
                }
                Err(e) => {
                    if is_permission(&e) {
                        shared.items[index]
                            .unreadable
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    immediate.push(index);
                }
            }
        }
    }
    for index in immediate {
        on_event(WalkEvent::Done {
            index,
            totals: shared.items[index].totals(),
        });
    }

    let has_jobs = !shared
        .queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .jobs
        .is_empty();
    let threads = threads.clamp(1, 16);
    let (tx, rx) = mpsc::channel::<usize>();
    std::thread::scope(|scope| {
        if has_jobs {
            for _ in 0..threads {
                let tx = tx.clone();
                let shared = &shared;
                scope.spawn(move || worker(shared, &tx));
            }
        }
        drop(tx);
        let mut last_tick = Instant::now();
        loop {
            match rx.recv_timeout(TICK) {
                Ok(index) => on_event(WalkEvent::Done {
                    index,
                    totals: shared.items[index].totals(),
                }),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                on_event(WalkEvent::Tick {
                    bytes: shared.bytes.load(Ordering::Relaxed),
                    files: shared.files.load(Ordering::Relaxed),
                });
            }
        }
    });

    WalkOutcome {
        totals: shared.items.iter().map(ItemState::totals).collect(),
        cancelled: cancel.load(Ordering::Relaxed),
        hardlink_cap_hit: shared.cap_hit.load(Ordering::Relaxed),
    }
}

fn worker(shared: &Shared<'_>, done: &mpsc::Sender<usize>) {
    let mut buf = vec![0u8; BULK_BUF];
    let mut subdirs: Vec<Job> = Vec::new();
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if shared.cancel.load(Ordering::Relaxed) {
                    q.finished = true;
                    q.jobs.clear();
                    shared.cv.notify_all();
                    return;
                }
                if let Some(job) = q.jobs.pop_front() {
                    q.active += 1;
                    break job;
                }
                if q.finished || q.active == 0 {
                    q.finished = true;
                    shared.cv.notify_all();
                    return;
                }
                q = shared.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };

        subdirs.clear();
        read_dir_into(shared, &job, &mut buf, &mut subdirs);

        let item = job.item;
        let state = &shared.items[item];
        state.pending.fetch_add(subdirs.len(), Ordering::AcqRel);
        let finished_item = state.pending.fetch_sub(1, Ordering::AcqRel) == 1;
        {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            // Depth first: the newest folders go to the front.
            for sub in subdirs.drain(..).rev() {
                q.jobs.push_front(sub);
            }
            q.active -= 1;
            if !q.jobs.is_empty() || q.active == 0 {
                shared.cv.notify_all();
            }
        }
        if finished_item {
            let _ = done.send(item);
        }
    }
}

fn is_permission(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
}

fn read_dir_into(shared: &Shared<'_>, job: &Job, buf: &mut [u8], subdirs: &mut Vec<Job>) {
    let Ok(cpath) = CString::new(job.path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: plain open(2) on a NUL-terminated path; the fd is closed below.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if is_permission(&err) {
            shared.items[job.item]
                .unreadable
                .fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    let fd = Fd(fd);
    // SAFETY: fstat on an open fd into a zeroed stat buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.0, &mut st) } != 0 || st.st_dev as u64 != job.dev {
        // Unreadable, or a mount point: never cross into another volume.
        return;
    }
    if !read_bulk(shared, job, &fd, buf, subdirs) {
        read_fallback(shared, job, subdirs);
    }
}

struct Fd(libc::c_int);
impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: we own this descriptor.
        unsafe { libc::close(self.0) };
    }
}

/// Reads one folder with `getattrlistbulk`. Returns false if the file system doesn't support it.
fn read_bulk(
    shared: &Shared<'_>,
    job: &Job,
    fd: &Fd,
    buf: &mut [u8],
    subdirs: &mut Vec<Job>,
) -> bool {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    let mut first = true;
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return true;
        }
        // SAFETY: the attribute list and buffer are valid for the duration of the call.
        let count = unsafe {
            libc::getattrlistbulk(
                fd.0,
                (&mut attrs as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
            )
        };
        if count < 0 {
            let err = std::io::Error::last_os_error();
            if first && matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL)) {
                return false;
            }
            if is_permission(&err) {
                shared.items[job.item]
                    .unreadable
                    .fetch_add(1, Ordering::Relaxed);
            }
            return true;
        }
        if count == 0 {
            return true;
        }
        first = false;
        let mut offset = 0usize;
        for _ in 0..count {
            let Some(entry) = buf.get(offset..) else {
                break;
            };
            let len = read_u32(entry, 0) as usize;
            if len == 0 || len > entry.len() {
                break;
            }
            parse_entry(shared, job, &entry[..len], subdirs);
            offset += len;
        }
    }
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map(|s| u32::from_ne_bytes([s[0], s[1], s[2], s[3]]))
        .unwrap_or(0)
}

fn read_i32(b: &[u8], at: usize) -> i32 {
    read_u32(b, at) as i32
}

fn read_u64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map(|s| u64::from_ne_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .unwrap_or(0)
}

/// Parses one `getattrlistbulk` record. Layout: length, returned attribute set, then the
/// requested attributes in bit order (`ATTR_CMN_ERROR` comes first when present), common before
/// file attributes.
fn parse_entry(shared: &Shared<'_>, job: &Job, e: &[u8], subdirs: &mut Vec<Job>) {
    let returned_common = read_u32(e, 4);
    let returned_file = read_u32(e, 4 + 12);
    let mut at = 4 + std::mem::size_of::<libc::attribute_set_t>();
    if returned_common & ATTR_CMN_ERROR != 0 {
        let error = read_u32(e, at);
        at += 4;
        if error != 0 {
            if matches!(error as i32, libc::EACCES | libc::EPERM) {
                shared.items[job.item]
                    .unreadable
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    }
    let mut name: &[u8] = &[];
    if returned_common & libc::ATTR_CMN_NAME != 0 {
        let off = read_i32(e, at);
        let len = read_u32(e, at + 4) as usize;
        let start = (at as i64 + off as i64) as usize;
        if let Some(raw) = e.get(start..start + len) {
            // The length includes the trailing NUL.
            name = raw.strip_suffix(&[0]).unwrap_or(raw);
        }
        at += 8;
    }
    let mut objtype = 0;
    if returned_common & libc::ATTR_CMN_OBJTYPE != 0 {
        objtype = read_u32(e, at);
        at += 4;
    }
    let mut ino = 0;
    if returned_common & libc::ATTR_CMN_FILEID != 0 {
        ino = read_u64(e, at);
        at += 8;
    }
    if name.is_empty() || name == b"." || name == b".." {
        return;
    }
    match objtype {
        VDIR => subdirs.push(Job {
            item: job.item,
            path: job.path.join(OsStr::from_bytes(name)),
            dev: job.dev,
        }),
        VREG | VLNK => {
            let mut nlink = 1u64;
            if returned_file & libc::ATTR_FILE_LINKCOUNT != 0 {
                nlink = read_u32(e, at) as u64;
                at += 4;
            }
            let mut alloc = 0u64;
            if returned_file & libc::ATTR_FILE_ALLOCSIZE != 0 {
                alloc = read_u64(e, at);
            }
            shared.count_file(job.item, job.dev, ino, nlink, alloc);
        }
        _ => {}
    }
}

/// `readdir` + `lstat` for file systems without bulk attributes.
fn read_fallback(shared: &Shared<'_>, job: &Job, subdirs: &mut Vec<Job>) {
    let rd = match std::fs::read_dir(&job.path) {
        Ok(rd) => rd,
        Err(e) => {
            if is_permission(&e) {
                shared.items[job.item]
                    .unreadable
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    };
    for entry in rd.flatten() {
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_dir() {
            if meta.dev() == job.dev {
                subdirs.push(Job {
                    item: job.item,
                    path,
                    dev: job.dev,
                });
            }
        } else if meta.file_type().is_file() || meta.file_type().is_symlink() {
            shared.count_file(
                job.item,
                meta.dev(),
                meta.ino(),
                meta.nlink(),
                meta.blocks() * 512,
            );
        }
    }
}

#[cfg(test)]
fn measure_one(path: &std::path::Path, threads: usize, cancel: &AtomicBool) -> SizeTotals {
    let out = measure(&[path.to_path_buf()], threads, cancel, &mut |_| {});
    out.totals.first().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kipimo-walk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn alloc(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    #[test]
    fn counts_allocated_bytes_and_files() {
        let dir = tmp("basic");
        fs::create_dir_all(dir.join("a/b/c")).unwrap();
        fs::write(dir.join("a/one"), vec![1u8; 10_000]).unwrap();
        fs::write(dir.join("a/b/two"), vec![2u8; 100_000]).unwrap();
        fs::write(dir.join("a/b/c/three"), b"x").unwrap();
        let expected = alloc(&dir.join("a/one"))
            + alloc(&dir.join("a/b/two"))
            + alloc(&dir.join("a/b/c/three"));
        let cancel = AtomicBool::new(false);
        let t = measure_one(&dir, 4, &cancel);
        assert_eq!(t.files, 3);
        assert_eq!(t.bytes, expected);
        assert!(t.bytes >= 110_001);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hard_links_count_once_across_roots() {
        let dir = tmp("hardlink");
        fs::create_dir_all(dir.join("x")).unwrap();
        fs::create_dir_all(dir.join("y")).unwrap();
        fs::write(dir.join("x/big"), vec![7u8; 200_000]).unwrap();
        fs::hard_link(dir.join("x/big"), dir.join("y/big-link")).unwrap();
        fs::hard_link(dir.join("x/big"), dir.join("x/big-link2")).unwrap();
        let one = alloc(&dir.join("x/big"));
        let cancel = AtomicBool::new(false);
        let out = measure(&[dir.join("x"), dir.join("y")], 4, &cancel, &mut |_| {});
        assert_eq!(out.totals[0].bytes + out.totals[1].bytes, one);
        assert_eq!(out.totals[0].files + out.totals[1].files, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinks_are_not_followed() {
        let dir = tmp("symlink");
        let outside = tmp("symlink-outside");
        fs::write(outside.join("huge"), vec![0u8; 500_000]).unwrap();
        fs::create_dir_all(dir.join("root")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("root/link-to-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("huge"), dir.join("root/link-to-file")).unwrap();
        let cancel = AtomicBool::new(false);
        let t = measure_one(&dir.join("root"), 2, &cancel);
        assert!(t.bytes < 500_000, "followed a symlink: {} bytes", t.bytes);
        // A symlink root is measured as the link itself.
        let t2 = measure_one(&dir.join("root/link-to-dir"), 2, &cancel);
        assert!(t2.bytes < 500_000);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn unreadable_folders_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("perm");
        fs::create_dir_all(dir.join("locked/inner")).unwrap();
        fs::write(dir.join("locked/inner/f"), vec![1u8; 5000]).unwrap();
        fs::write(dir.join("open"), vec![1u8; 5000]).unwrap();
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let cancel = AtomicBool::new(false);
        let t = measure_one(&dir, 2, &cancel);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(t.files, 1);
        assert_eq!(t.unreadable, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bulk_attributes_are_used_on_apfs() {
        let dir = tmp("bulk");
        fs::write(dir.join("f"), vec![1u8; 4096]).unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        let cancel = AtomicBool::new(false);
        let shared = Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                active: 0,
                finished: false,
            }),
            cv: Condvar::new(),
            items: vec![ItemState {
                bytes: AtomicU64::new(0),
                linked: AtomicU64::new(0),
                files: AtomicU64::new(0),
                unreadable: AtomicU64::new(0),
                pending: AtomicUsize::new(0),
            }],
            links: (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
            link_count: AtomicUsize::new(0),
            cap_hit: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
            cancel: &cancel,
        };
        let path = CString::new(dir.as_os_str().as_bytes()).unwrap();
        let fd = Fd(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) });
        let job = Job {
            item: 0,
            path: dir.clone(),
            dev: fs::metadata(&dir).unwrap().dev(),
        };
        let mut buf = vec![0u8; BULK_BUF];
        let mut subdirs = Vec::new();
        assert!(
            read_bulk(&shared, &job, &fd, &mut buf, &mut subdirs),
            "getattrlistbulk unsupported"
        );
        assert_eq!(subdirs.len(), 1);
        assert_eq!(shared.items[0].files.load(Ordering::Relaxed), 1);
        assert_eq!(
            shared.items[0].bytes.load(Ordering::Relaxed),
            alloc(&dir.join("f"))
        );
        drop(fd);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_stops_early() {
        let dir = tmp("cancel");
        for i in 0..50 {
            fs::create_dir_all(dir.join(format!("d{i}"))).unwrap();
            fs::write(dir.join(format!("d{i}/f")), b"abc").unwrap();
        }
        let cancel = AtomicBool::new(true);
        let out = measure(std::slice::from_ref(&dir), 2, &cancel, &mut |_| {});
        assert!(out.cancelled);
        assert!(out.totals[0].files < 50);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn done_events_fire_once_per_root() {
        let dir = tmp("events");
        for i in 0..5 {
            fs::create_dir_all(dir.join(format!("r{i}/sub"))).unwrap();
            fs::write(dir.join(format!("r{i}/sub/f")), b"abc").unwrap();
        }
        fs::write(dir.join("file-root"), b"abc").unwrap();
        let mut roots: Vec<PathBuf> = (0..5).map(|i| dir.join(format!("r{i}"))).collect();
        roots.push(dir.join("file-root"));
        roots.push(dir.join("missing"));
        let cancel = AtomicBool::new(false);
        let mut done = vec![0; roots.len()];
        let out = measure(&roots, 3, &cancel, &mut |e| {
            if let WalkEvent::Done { index, totals } = e {
                done[index] += 1;
                assert_eq!(totals.files, if index < 6 { 1 } else { 0 });
            }
        });
        assert_eq!(done, vec![1; roots.len()]);
        assert!(!out.cancelled);
        fs::remove_dir_all(&dir).unwrap();
    }
}
