//! Suggested scan roots for the picker, and the Full Disk Access check.

use std::path::{Path, PathBuf};

use crate::ffi;
use crate::model::{ScanTarget, TargetKind};

/// The places the UI can offer to scan: the home folder, the startup disk (`/`, which covers its
/// System and Data volumes once), and every other visible mounted volume (external disks, disk
/// images, network shares). Reads only the mount table: fast and never blocks on the network.
pub fn scan_targets() -> Vec<ScanTarget> {
    let mounts = ffi::mounts();
    let volume_of = |path: &Path| {
        mounts
            .iter()
            .filter(|v| path.starts_with(&v.mount_point))
            .max_by_key(|v| v.mount_point.as_os_str().len())
            .cloned()
    };
    let mut targets = Vec::new();
    if let Some(home) = ffi::home_dir().and_then(|h| ffi::real_path(&h).ok()) {
        let volume = volume_of(&home);
        targets.push(ScanTarget {
            kind: TargetKind::Home,
            name: home
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| home.display().to_string()),
            total_bytes: volume.as_ref().map_or(0, |v| v.total),
            available_bytes: volume.as_ref().map_or(0, |v| v.available),
            used_bytes: volume.as_ref().map_or(0, |v| v.used),
            local: volume.as_ref().is_none_or(|v| v.local),
            path: home,
        });
    }
    for volume in &mounts {
        let is_root = volume.mount_point == Path::new("/");
        if !is_root && (!volume.browsable || volume.never_enter()) {
            continue;
        }
        let name = ffi::volume_name(&volume.mount_point).unwrap_or_else(|| {
            volume
                .mount_point
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| volume.mount_point.display().to_string())
        });
        targets.push(ScanTarget {
            kind: if is_root {
                TargetKind::StartupDisk
            } else {
                TargetKind::Volume
            },
            name,
            path: volume.mount_point.clone(),
            total_bytes: volume.total,
            available_bytes: volume.available,
            // APFS volumes share their container: for the startup disk report what the whole
            // disk uses (System, Data, VM, Preboot, ...), like Finder.
            used_bytes: if is_root {
                volume.total.saturating_sub(volume.available)
            } else {
                volume.used
            },
            local: volume.local,
        });
    }
    targets
}

/// Whether Kipimo has Full Disk Access: `Some(true)`/`Some(false)`, or `None` if it cannot tell.
/// Opens (and immediately closes, without reading) the per-user privacy database, which macOS
/// only lets apps with Full Disk Access open. This never shows a permission prompt.
pub fn has_full_disk_access() -> Option<bool> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = ffi::home_dir() {
        candidates.push(home.join("Library/Application Support/com.apple.TCC/TCC.db"));
    }
    candidates.push(PathBuf::from(
        "/Library/Application Support/com.apple.TCC/TCC.db",
    ));
    for candidate in candidates {
        match std::fs::File::open(&candidate) {
            Ok(_) => return Some(true),
            Err(err) if ffi::is_privacy_denial(&err) => return Some(false),
            Err(_) => continue,
        }
    }
    None
}
