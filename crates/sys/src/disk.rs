//! Mounted volumes with their free space (including what macOS can purge), and
//! the size of the Trash.

use std::collections::HashSet;
use std::ffi::c_void;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFDictionary, CFRetained, CFType};
use objc2_foundation::{
    NSArray, NSFileManager, NSNumber, NSString, NSURL, NSURLResourceKey,
    NSURLVolumeAvailableCapacityForImportantUsageKey, NSURLVolumeAvailableCapacityKey,
    NSURLVolumeIsBrowsableKey, NSURLVolumeIsEjectableKey, NSURLVolumeIsInternalKey,
    NSURLVolumeIsLocalKey, NSURLVolumeIsRemovableKey, NSURLVolumeIsRootFileSystemKey,
    NSURLVolumeLocalizedFormatDescriptionKey, NSURLVolumeNameKey, NSURLVolumeTotalCapacityKey,
    NSVolumeEnumerationOptions,
};
use serde::Serialize;

use crate::cf::{self, Dict};

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(alloc: *const c_void) -> *const c_void;
    fn DADiskCreateFromVolumePath(
        alloc: *const c_void,
        session: *const c_void,
        path: *const c_void,
    ) -> *const c_void;
    fn DADiskCopyDescription(disk: *const c_void) -> *const c_void;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum VolumeKind {
    /// The startup disk or another internal volume.
    Internal,
    /// A USB, Thunderbolt or SD-card drive.
    External,
    /// A mounted disk image (.dmg).
    DiskImage,
    /// A network share.
    Network,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Volume {
    pub name: String,
    pub path: PathBuf,
    pub kind: VolumeKind,
    /// "APFS", "ExFAT", …
    pub format: String,
    pub total: u64,
    /// Free right now.
    pub available: u64,
    /// Free for things you save, counting space macOS frees on demand (purgeable
    /// caches, local snapshots). This is the number Finder and System Settings show.
    pub available_important: u64,
    /// `available_important − available`: what macOS can purge.
    pub purgeable: u64,
    pub is_root: bool,
    pub is_ejectable: bool,
    pub is_removable: bool,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available_important)
    }
}

/// The resource keys read for each volume.
struct Keys {
    name: &'static NSURLResourceKey,
    total: &'static NSURLResourceKey,
    available: &'static NSURLResourceKey,
    important: &'static NSURLResourceKey,
    internal: &'static NSURLResourceKey,
    local: &'static NSURLResourceKey,
    removable: &'static NSURLResourceKey,
    ejectable: &'static NSURLResourceKey,
    root: &'static NSURLResourceKey,
    browsable: &'static NSURLResourceKey,
    format: &'static NSURLResourceKey,
}

impl Keys {
    fn get() -> Keys {
        // SAFETY: Foundation's resource-key constants are valid NSStrings for the process lifetime.
        unsafe {
            Keys {
                name: NSURLVolumeNameKey,
                total: NSURLVolumeTotalCapacityKey,
                available: NSURLVolumeAvailableCapacityKey,
                important: NSURLVolumeAvailableCapacityForImportantUsageKey,
                internal: NSURLVolumeIsInternalKey,
                local: NSURLVolumeIsLocalKey,
                removable: NSURLVolumeIsRemovableKey,
                ejectable: NSURLVolumeIsEjectableKey,
                root: NSURLVolumeIsRootFileSystemKey,
                browsable: NSURLVolumeIsBrowsableKey,
                format: NSURLVolumeLocalizedFormatDescriptionKey,
            }
        }
    }

    fn array(&self) -> Retained<NSArray<NSURLResourceKey>> {
        NSArray::from_slice(&[
            self.name,
            self.total,
            self.available,
            self.important,
            self.internal,
            self.local,
            self.removable,
            self.ejectable,
            self.root,
            self.browsable,
            self.format,
        ])
    }
}

struct Values(Retained<objc2_foundation::NSDictionary<NSURLResourceKey, AnyObject>>);

impl Values {
    fn get(&self, key: &NSURLResourceKey) -> Option<Retained<AnyObject>> {
        self.0.objectForKey(key)
    }
    fn u64(&self, key: &NSURLResourceKey) -> Option<u64> {
        self.get(key)?
            .downcast::<NSNumber>()
            .ok()
            .map(|n| n.longLongValue().max(0) as u64)
    }
    fn bool(&self, key: &NSURLResourceKey) -> Option<bool> {
        self.get(key)?
            .downcast::<NSNumber>()
            .ok()
            .map(|n| n.boolValue())
    }
    fn string(&self, key: &NSURLResourceKey) -> Option<String> {
        self.get(key)?
            .downcast::<NSString>()
            .ok()
            .map(|s| s.to_string())
    }
}

/// DiskArbitration's description of the disk behind a mounted volume.
fn disk_description(session: *const c_void, url: &NSURL) -> Option<Dict> {
    if session.is_null() {
        return None;
    }
    let url_ptr: *const NSURL = url;
    // SAFETY: NSURL is toll-free bridged with CFURL; results are +1 or null.
    let disk = unsafe {
        cf::owned::<CFType>(DADiskCreateFromVolumePath(
            std::ptr::null(),
            session,
            url_ptr.cast(),
        ))
    }?;
    // SAFETY: `disk` is a valid DADisk; the description is +1 or null.
    unsafe { cf::owned::<CFDictionary>(DADiskCopyDescription(cf::ptr(&*disk))) }.map(Dict)
}

/// Classify a volume. Pure, so it can be tested without real disks.
pub(crate) fn classify(
    local: bool,
    internal: Option<bool>,
    device_model: Option<&str>,
    device_protocol: Option<&str>,
) -> VolumeKind {
    if !local {
        return VolumeKind::Network;
    }
    if device_model == Some("Disk Image") || device_protocol == Some("Virtual Interface") {
        return VolumeKind::DiskImage;
    }
    match internal {
        Some(false) => VolumeKind::External,
        _ => VolumeKind::Internal,
    }
}

/// Every user-visible mounted volume (hidden system volumes such as Preboot and VM are skipped).
pub fn volumes() -> Vec<Volume> {
    let fm = NSFileManager::defaultManager();
    let k = Keys::get();
    let keys = k.array();
    let Some(urls) = fm.mountedVolumeURLsIncludingResourceValuesForKeys_options(
        Some(&keys),
        NSVolumeEnumerationOptions::SkipHiddenVolumes,
    ) else {
        return Vec::new();
    };
    // SAFETY: a null allocator means the default one; the session is +1.
    let session: Option<CFRetained<CFType>> =
        unsafe { cf::owned(DASessionCreate(std::ptr::null())) };
    let session_ptr = session.as_deref().map(cf::ptr).unwrap_or(std::ptr::null());
    let mut out = Vec::new();
    for url in urls.iter() {
        let Ok(v) = url.resourceValuesForKeys_error(&keys) else {
            continue;
        };
        let v = Values(v);
        if v.bool(k.browsable) == Some(false) {
            continue;
        }
        let Some(path) = url.path().map(|p| PathBuf::from(p.to_string())) else {
            continue;
        };
        let local = v.bool(k.local).unwrap_or(true);
        let desc = if local {
            disk_description(session_ptr, &url)
        } else {
            None
        };
        let model = desc.as_ref().and_then(|d| d.string("DADeviceModel"));
        let protocol = desc.as_ref().and_then(|d| d.string("DADeviceProtocol"));
        let internal = v
            .bool(k.internal)
            .or_else(|| desc.as_ref().and_then(|d| d.boolean("DADeviceInternal")));
        let available = v.u64(k.available).unwrap_or(0);
        let available_important = v
            .u64(k.important)
            .filter(|&n| n > 0)
            .unwrap_or(available)
            .max(available);
        out.push(Volume {
            name: v
                .string(k.name)
                .unwrap_or_else(|| path.display().to_string()),
            kind: classify(local, internal, model.as_deref(), protocol.as_deref()),
            format: v.string(k.format).unwrap_or_default(),
            total: v.u64(k.total).unwrap_or(0),
            available,
            available_important,
            purgeable: available_important - available,
            is_root: v.bool(k.root).unwrap_or(path == Path::new("/")),
            is_ejectable: v.bool(k.ejectable).unwrap_or(false),
            is_removable: v.bool(k.removable).unwrap_or(false),
            path,
        });
    }
    out
}

/// The startup volume ("Macintosh HD").
pub fn boot_volume() -> Option<Volume> {
    volumes().into_iter().find(|v| v.is_root)
}

/// Free space on the startup volume (important-usage capacity) without
/// enumerating every volume. Cheap enough for a background check.
pub fn boot_free_bytes() -> Option<u64> {
    let k = Keys::get();
    let url = NSURL::fileURLWithPath(&NSString::from_str("/"));
    let v = Values(
        url.resourceValuesForKeys_error(&NSArray::from_slice(&[k.important, k.available]))
            .ok()?,
    );
    v.u64(k.important)
        .filter(|&n| n > 0)
        .or_else(|| v.u64(k.available))
}

// ── Trash ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct TrashSize {
    /// Space the items take on disk.
    pub bytes: u64,
    /// Items at the top level, as Finder counts them.
    pub items: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrashError {
    /// macOS refused access: Kipimo needs Full Disk Access to read the Trash.
    NoAccess,
    /// Some other I/O error.
    Unreadable,
}

/// The user's Trash folder.
pub fn trash_dir() -> PathBuf {
    crate::fake_home()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".Trash")
}

/// Trash folders on other mounted volumes (`/Volumes/X/.Trashes/<uid>`), which Finder empties too.
pub fn other_trash_dirs() -> Vec<PathBuf> {
    // SAFETY: no preconditions.
    let uid = unsafe { libc::getuid() };
    let Ok(rd) = std::fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    rd.flatten()
        // The startup volume shows up here as a symlink to "/".
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().join(".Trashes").join(uid.to_string()))
        .filter(|p| p.is_dir())
        .collect()
}

/// Size of the user's Trash, plus the Trash folders on other volumes.
pub fn trash_size() -> Result<TrashSize, TrashError> {
    let mut total = tree_size(&trash_dir(), None).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => TrashError::NoAccess,
        io::ErrorKind::NotFound => TrashError::Unreadable,
        _ => TrashError::Unreadable,
    })?;
    for d in other_trash_dirs() {
        if let Ok(s) = tree_size(&d, None) {
            total.bytes += s.bytes;
            total.items += s.items;
        }
    }
    Ok(total)
}

/// Last-modified time of the Trash folder, which changes whenever something
/// is put in or taken out. Lets a background check skip re-measuring.
pub fn trash_modified() -> Option<std::time::SystemTime> {
    std::fs::metadata(trash_dir())
        .and_then(|m| m.modified())
        .ok()
}

/// Space a folder takes on disk (allocated blocks, hard links counted once,
/// symlinks not followed) and how many entries sit directly inside it.
/// Errors only if `root` itself can't be read; unreadable subfolders are skipped.
pub fn tree_size(root: &Path, cancel: Option<&AtomicBool>) -> io::Result<TrashSize> {
    let mut items = 0u64;
    let mut bytes = 0u64;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![(root.to_path_buf(), true)];
    while let Some((dir, top)) = stack.pop() {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if top => return Err(e),
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            if top {
                items += 1;
            }
            let Ok(md) = entry.metadata() else { continue };
            if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
                continue;
            }
            bytes += md.blocks() * 512;
            if md.is_dir() {
                stack.push((entry.path(), false));
            }
        }
    }
    Ok(TrashSize { bytes, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kipimo-sys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tree_size_counts_top_level_items_and_blocks() {
        let d = temp("tree");
        std::fs::write(d.join("a.txt"), vec![1u8; 10_000]).unwrap();
        std::fs::create_dir_all(d.join("folder/inner")).unwrap();
        std::fs::write(d.join("folder/inner/b.bin"), vec![2u8; 50_000]).unwrap();
        std::os::unix::fs::symlink("/System", d.join("link")).unwrap();
        let s = tree_size(&d, None).unwrap();
        assert_eq!(s.items, 3);
        assert!(s.bytes >= 60_000, "{}", s.bytes);
        // The symlink isn't followed into /System.
        assert!(s.bytes < 10_000_000);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn tree_size_counts_hard_links_once() {
        let d = temp("links");
        std::fs::write(d.join("a"), vec![1u8; 100_000]).unwrap();
        std::fs::hard_link(d.join("a"), d.join("b")).unwrap();
        let s = tree_size(&d, None).unwrap();
        assert_eq!(s.items, 2);
        assert!(s.bytes < 200_000, "{}", s.bytes);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn tree_size_missing_root_is_an_error() {
        assert!(tree_size(Path::new("/nonexistent/kipimo"), None).is_err());
    }

    #[test]
    fn tree_size_stops_when_cancelled() {
        let d = temp("cancel");
        std::fs::write(d.join("a"), b"x").unwrap();
        let flag = AtomicBool::new(true);
        assert_eq!(tree_size(&d, Some(&flag)).unwrap(), TrashSize::default());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn classifies_volumes() {
        assert_eq!(classify(false, None, None, None), VolumeKind::Network);
        assert_eq!(
            classify(true, Some(true), Some("APPLE SSD"), Some("Apple Fabric")),
            VolumeKind::Internal
        );
        assert_eq!(
            classify(
                true,
                Some(false),
                Some("Disk Image"),
                Some("Virtual Interface")
            ),
            VolumeKind::DiskImage
        );
        assert_eq!(
            classify(true, Some(false), Some("Extreme SSD"), Some("USB")),
            VolumeKind::External
        );
    }

    #[test]
    fn live_boot_volume() {
        let v = boot_volume().expect("a startup volume");
        assert!(v.total > 0 && v.available_important >= v.available);
        assert_eq!(v.kind, VolumeKind::Internal);
        assert!(boot_free_bytes().unwrap() > 0);
    }
}
