//! Watching the Trash for apps (FSEvents). When an `.app` bundle lands
//! directly in the watched folder, the callback gets its path. Kipimo then
//! runs the leftover finder on it and, if the app left files behind, offers
//! to review them (`background::app_trashed`).
//!
//! Reading `~/.Trash` needs Full Disk Access; without it macOS delivers no
//! events and the watcher stays quiet.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFRetained, CFString};

type StreamRef = *mut c_void;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: *const c_void,
        callback: Callback,
        context: *mut Context,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: *const c_void);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
}

const SINCE_NOW: u64 = u64::MAX;
const FLAG_NO_DEFER: u32 = 0x02;
const FLAG_FILE_EVENTS: u32 = 0x10;
const ITEM_CREATED: u32 = 0x100;
const ITEM_RENAMED: u32 = 0x800;
const ITEM_IS_DIR: u32 = 0x20000;

/// Whether an event says an app bundle appeared directly inside `dir`.
pub(crate) fn is_app_arrival(dir: &Path, path: &Path, flags: u32) -> bool {
    flags & ITEM_IS_DIR != 0
        && flags & (ITEM_CREATED | ITEM_RENAMED) != 0
        && path.parent() == Some(dir)
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

struct Info {
    dir: PathBuf,
    on_app: Box<dyn Fn(PathBuf) + Send + Sync>,
    /// A move fires several events; report each bundle once a minute at most.
    seen: Mutex<Vec<(PathBuf, Instant)>>,
}

extern "C" fn callback(
    _s: StreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: FSEvents passes back our Info and `n` C-string paths and flags.
    let (info, paths, flags) = unsafe {
        (
            &*(info as *const Info),
            std::slice::from_raw_parts(paths as *const *const c_char, n),
            std::slice::from_raw_parts(flags, n),
        )
    };
    for (p, f) in paths.iter().zip(flags) {
        // SAFETY: each path is a NUL-terminated C string owned by FSEvents for this call.
        let path = PathBuf::from(unsafe { CStr::from_ptr(*p) }.to_string_lossy().into_owned());
        if !is_app_arrival(&info.dir, &path, *f) || !path.is_dir() {
            continue;
        }
        let fresh = {
            let mut seen = info.seen.lock().unwrap();
            seen.retain(|(_, t)| t.elapsed() < Duration::from_secs(60));
            let new = !seen.iter().any(|(s, _)| s == &path);
            if new {
                seen.push((path.clone(), Instant::now()));
            }
            new
        };
        if fresh {
            (info.on_app)(path);
        }
    }
}

/// A running watcher; stops when dropped.
pub struct TrashWatcher {
    stream: StreamRef,
    _queue: DispatchRetained<DispatchQueue>,
    info: *mut Info,
}

// SAFETY: the stream is only started and stopped through this handle.
unsafe impl Send for TrashWatcher {}

impl TrashWatcher {
    /// Watch `dir` (normally `~/.Trash`); `on_app` runs on a private queue.
    pub fn start(
        dir: &Path,
        on_app: impl Fn(PathBuf) + Send + Sync + 'static,
    ) -> Option<TrashWatcher> {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let c = CString::new(dir.to_string_lossy().as_bytes()).ok()?;
        let path = CFString::from_str(c.to_str().ok()?);
        let paths: CFRetained<CFArray<CFString>> = CFArray::from_retained_objects(&[path]);
        let info = Box::into_raw(Box::new(Info {
            dir,
            on_app: Box::new(on_app),
            seen: Mutex::new(Vec::new()),
        }));
        let mut ctx = Context {
            version: 0,
            info: info.cast(),
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let queue = DispatchQueue::new("top.piedol.kipimo.trash-watch", None);
        // SAFETY: valid callback, context and CFArray of CFStrings; FSEvents copies the context.
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                callback,
                &mut ctx,
                (&*paths as *const CFArray<CFString>).cast(),
                SINCE_NOW,
                1.0,
                FLAG_FILE_EVENTS | FLAG_NO_DEFER,
            )
        };
        if stream.is_null() {
            // SAFETY: we still own the box.
            drop(unsafe { Box::from_raw(info) });
            return None;
        }
        // SAFETY: a live stream and queue.
        unsafe {
            FSEventStreamSetDispatchQueue(stream, (&*queue as *const DispatchQueue).cast());
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                drop(Box::from_raw(info));
                return None;
            }
        }
        Some(TrashWatcher {
            stream,
            _queue: queue,
            info,
        })
    }
}

impl Drop for TrashWatcher {
    fn drop(&mut self) {
        // SAFETY: stop, invalidate and release once; no callbacks run after invalidate.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.info));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn arrival_filter() {
        let d = Path::new("/Users/x/.Trash");
        let dir_flags = ITEM_IS_DIR | ITEM_RENAMED;
        assert!(is_app_arrival(d, &d.join("Zoom.app"), dir_flags));
        assert!(is_app_arrival(
            d,
            &d.join("Zoom 2.APP"),
            ITEM_IS_DIR | ITEM_CREATED
        ));
        // Inside another folder, not a directory, or just modified: no.
        assert!(!is_app_arrival(d, &d.join("Folder/Zoom.app"), dir_flags));
        assert!(!is_app_arrival(d, &d.join("notes.txt"), ITEM_RENAMED));
        assert!(!is_app_arrival(
            d,
            &d.join("Zoom.app"),
            ITEM_IS_DIR | 0x1000
        ));
    }

    /// A synthetic "Trash" in a temp folder: an app moved in is reported once.
    #[test]
    fn reports_an_app_moved_in() {
        let base = std::env::temp_dir().join(format!("kipimo-trashwatch-{}", std::process::id()));
        let trash = base.join("Trash");
        let outside = base.join("Applications/Fake.app/Contents");
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Info.plist"), b"<plist/>").unwrap();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let w = TrashWatcher::start(&trash, move |p| {
            let _ = tx.lock().unwrap().send(p);
        })
        .expect("stream starts");
        std::thread::sleep(Duration::from_millis(300));
        std::fs::rename(base.join("Applications/Fake.app"), trash.join("Fake.app")).unwrap();
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("event arrives");
        assert_eq!(got.file_name().unwrap(), "Fake.app");
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_err(),
            "reported once"
        );
        drop(w);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
