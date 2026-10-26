//! Synthetic folders for tests: everything lives in a fresh temporary directory that is removed
//! afterwards. Nothing here touches the real home folder.
#![allow(dead_code)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TempTree {
    pub root: PathBuf,
}

impl TempTree {
    pub fn new(label: &str) -> TempTree {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "kipimo-space-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        TempTree { root }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn dir(&self, rel: &str) -> PathBuf {
        let path = self.path(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Writes `len` bytes of real (non-sparse) data.
    pub fn file(&self, rel: &str, len: usize) -> PathBuf {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, vec![0x5au8; len]).unwrap();
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Restore permissions a test may have removed, then clean up the synthetic folder.
        restore_permissions(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn restore_permissions(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.is_dir()
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                restore_permissions(&entry.path());
            }
        }
    }
}

/// Allocated bytes on disk of everything under `path`, computed the slow way: `lstat` every
/// entry, `st_blocks * 512`, hard links once. The reference the scanner must match.
pub fn reference_allocated(path: &Path) -> (u64, u64, u64) {
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0;
    let mut files = 0;
    let mut folders = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        bytes += std::fs::symlink_metadata(&dir).unwrap().blocks() * 512;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                folders += 1;
                stack.push(entry.path());
            } else {
                files += 1;
                if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                    bytes += meta.blocks() * 512;
                }
            }
        }
    }
    (bytes, files, folders)
}

/// Creates `depth` nested folders called `name` below `base` with `mkdirat`/`openat`, so the
/// total path can exceed `PATH_MAX`, and writes `leaf.bin` (`leaf_len` bytes) at the bottom.
pub fn deep_nest(base: &Path, name: &str, depth: usize, leaf_len: usize) {
    let c_name = CString::new(name).unwrap();
    let c_base = CString::new(base.as_os_str().as_bytes()).unwrap();
    unsafe {
        let mut fd = libc::open(c_base.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(fd >= 0);
        for _ in 0..depth {
            assert_eq!(libc::mkdirat(fd, c_name.as_ptr(), 0o755), 0);
            let next = libc::openat(fd, c_name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            assert!(next >= 0);
            libc::close(fd);
            fd = next;
        }
        let leaf = CString::new("leaf.bin").unwrap();
        let file = libc::openat(fd, leaf.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
        assert!(file >= 0);
        let data = vec![7u8; leaf_len];
        assert_eq!(
            libc::write(file, data.as_ptr().cast(), data.len()),
            data.len() as isize
        );
        libc::close(file);
        libc::close(fd);
    }
}

/// A small APFS disk image attached (hidden from Finder) at a folder of our choosing, detached
/// on drop. `None` if `hdiutil` is unavailable here.
pub struct DiskImage {
    pub mount: PathBuf,
}

impl DiskImage {
    pub fn attach(image: &Path, mount: &Path, name: &str) -> Option<DiskImage> {
        std::fs::create_dir_all(mount).ok()?;
        let created = Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-quiet", "-size", "16m", "-fs", "APFS", "-layout", "NONE",
            ])
            .args(["-volname", name])
            .arg(image)
            .status()
            .ok()?
            .success();
        if !created {
            return None;
        }
        let attached = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noverify", "-noautoopen"])
            .arg("-mountpoint")
            .arg(mount)
            .arg(image)
            .status()
            .ok()?
            .success();
        if !attached {
            return None;
        }
        Some(DiskImage {
            mount: std::fs::canonicalize(mount).ok()?,
        })
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .status();
    }
}

/// Waits (up to `timeout`) until Finder's Put Back record of every trashed path is in its Trash
/// folder's `.DS_Store`, and returns the names still missing. `NSFileManager` writes the first
/// record at once and batches the rest seconds later, from inside this process.
pub fn missing_put_back(trashed: &[PathBuf], timeout: std::time::Duration) -> Vec<String> {
    let start = std::time::Instant::now();
    loop {
        let missing: Vec<String> = trashed
            .iter()
            .filter(|p| !has_put_back(p))
            .map(|p| p.display().to_string())
            .collect();
        if missing.is_empty() || start.elapsed() >= timeout {
            return missing;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `.DS_Store` records are keyed by the UTF-16BE file name; `ptbL` is the Put Back location.
fn has_put_back(trashed: &Path) -> bool {
    let (Some(dir), Some(name)) = (trashed.parent(), trashed.file_name()) else {
        return false;
    };
    let Ok(store) = std::fs::read(dir.join(".DS_Store")) else {
        return false;
    };
    let mut key: Vec<u8> = name
        .to_string_lossy()
        .encode_utf16()
        .flat_map(|u| u.to_be_bytes())
        .collect();
    key.extend_from_slice(b"ptbL");
    store.windows(key.len()).any(|w| w == key.as_slice())
}
