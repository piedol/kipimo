//! Moving user-selected items to the Trash, with the safety rules, and updating the tree after.
//!
//! Method: `NSFileManager trashItemAtURL:resultingItemURL:error:`. Checked on macOS 26.6 on a
//! mounted disk image: it records Finder's "Put Back" location (`ptbL`/`ptbN` entries in the
//! Trash folder's `.DS_Store`) exactly like Finder's own Move to Trash, so Put Back works. It
//! needs no Apple Events permission to control Finder, and it never deletes anything.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;

use objc2::rc::Retained;
use objc2_foundation::{NSFileManager, NSURL};

use crate::ffi;
use crate::map::SpaceMap;
use crate::model::{TrashOutcome, TrashResult, TrashSkip};
use crate::scan::{read_tree, write_tree};

/// Moves one item to the Trash, returning where it ended up.
pub(crate) type TrashFn<'a> = dyn FnMut(&Path) -> Result<Option<PathBuf>, String> + 'a;

/// The real mover: `NSFileManager` (keeps Finder's "Put Back").
pub(crate) fn trash_with_file_manager(path: &Path) -> Result<Option<PathBuf>, String> {
    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::from_file_path(path).ok_or_else(|| "not a valid file path".to_string())?;
        let manager = NSFileManager::defaultManager();
        let mut resulting: Option<Retained<NSURL>> = None;
        manager
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|err| err.localizedDescription().to_string())?;
        Ok(resulting.and_then(|url| url.to_file_path()))
    })
}

/// Bundles and packages whose insides are never touched: apps and their plug-ins (Kipimo never
/// modifies apps) and document packages that break if a piece is removed.
const SEALED_PACKAGES: &[&str] = &[
    "app",
    "appex",
    "framework",
    "bundle",
    "plugin",
    "kext",
    "xpc",
    "systemextension",
    "dext",
    "prefpane",
    "saver",
    "qlgenerator",
    "mdimporter",
    "component",
    "vst",
    "vst3",
    "aaxplugin",
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "logicx",
    "band",
    "sparsebundle",
    "backupbundle",
];

/// Folders of the startup disk's `/Library` that belong to macOS or to startup items (tool D
/// handles those), protected with everything inside.
const PROTECTED_LIBRARY: &[&str] = &[
    "Apple",
    "CoreServices",
    "DriverExtensions",
    "Extensions",
    "Filesystems",
    "Frameworks",
    "Keychains",
    "LaunchAgents",
    "LaunchDaemons",
    "OSAnalytics",
    "Preferences",
    "PrivilegedHelperTools",
    "Receipts",
    "Sandbox",
    "Security",
    "StagedExtensions",
    "SystemExtensions",
    "SystemMigration",
    "Updates",
];

/// Folders of a home folder that are protected themselves (their contents are not).
const HOME_FOLDERS: &[&str] = &[
    "Applications",
    "Desktop",
    "Documents",
    "Downloads",
    "Library",
    "Movies",
    "Music",
    "Pictures",
    "Public",
    "Sites",
];

fn names(path: &Path) -> Vec<&OsStr> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

fn is(name: &OsStr, expected: &str) -> bool {
    name.as_bytes() == expected.as_bytes()
}

fn extension_is_sealed(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| {
            SEALED_PACKAGES
                .iter()
                .any(|sealed| sealed.eq_ignore_ascii_case(ext))
        })
}

/// Rules inside a home folder; `rel` is the path below the home folder.
fn home_rule(rel: &[&OsStr]) -> Option<TrashSkip> {
    match rel {
        [] => Some(TrashSkip::ProtectedLocation),
        [name] if HOME_FOLDERS.iter().any(|f| is(name, f)) => Some(TrashSkip::ProtectedLocation),
        // ~/Library/<anything> itself (Caches, Containers, Preferences, ...).
        [library, _] if is(library, "Library") => Some(TrashSkip::ProtectedLocation),
        // The root folder of each cloud storage provider.
        [library, cloud, _] if is(library, "Library") && is(cloud, "CloudStorage") => {
            Some(TrashSkip::ProtectedLocation)
        }
        [library, special, ..]
            if is(library, "Library")
                && ["Keychains", "LaunchAgents"].iter().any(|s| is(special, s)) =>
        {
            Some(TrashSkip::ProtectedLocation)
        }
        _ => None,
    }
}

/// Rules for the startup disk, on the path as seen from `/` (a path under
/// `/System/Volumes/Data` is checked as the same path under `/`).
fn startup_rule(parts: &[&OsStr]) -> Option<TrashSkip> {
    let protected = Some(TrashSkip::ProtectedLocation);
    let Some(first) = parts.first() else {
        return protected;
    };
    // Every top-level item of the startup disk (/Applications, /Library, /Users, /opt, ...).
    if parts.len() == 1 || first.as_bytes().starts_with(b".") {
        return protected;
    }
    let second = parts[1];
    match first.to_str().unwrap_or_default() {
        "System"
        | "bin"
        | "sbin"
        | "dev"
        | "etc"
        | "var"
        | "cores"
        | "Network"
        | "AppleInternal"
        | "Volumes"
        | "home"
        | "net"
        | "MobileSoftwareUpdate"
        | "macOS Install Data" => protected,
        "usr" if !(is(second, "local") && parts.len() >= 3) => protected,
        "private" => {
            let tmp = is(second, "tmp") && parts.len() >= 3;
            // /private/var/folders/<xx>/<id>/<C|T|0>/<item>: per-user temporary and cache files.
            let user_temp = is(second, "var") && is(parts[2], "folders") && parts.len() >= 7;
            if tmp || user_temp { None } else { protected }
        }
        "Library" => {
            if parts.len() == 2 || PROTECTED_LIBRARY.iter().any(|p| is(second, p)) {
                protected
            } else {
                None
            }
        }
        "Applications" if is(second, "Utilities") && parts.len() == 2 => protected,
        "Users" => {
            if parts.len() == 2 {
                protected
            } else {
                home_rule(&parts[2..])
            }
        }
        _ => None,
    }
}

/// Why `path` must not be moved to the Trash, or `None` if it may be. `path` is absolute and
/// normalised.
pub(crate) fn protection(
    path: &Path,
    home: Option<&Path>,
    mounts: &HashMap<PathBuf, bool>,
) -> Option<TrashSkip> {
    if mounts.contains_key(path) || path == Path::new("/") {
        return Some(TrashSkip::ProtectedLocation);
    }
    let parts = names(path);
    if parts
        .iter()
        .any(|name| is(name, ".Trash") || is(name, ".Trashes"))
    {
        return Some(TrashSkip::AlreadyInTrash);
    }
    if parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|name| extension_is_sealed(name))
    {
        return Some(TrashSkip::InsideBundle);
    }
    // Paths inside other volumes only get the generic rules above plus a few of their own;
    // everything else is checked as part of the startup disk (System and Data volumes).
    let on_other_volume = mounts
        .iter()
        .filter(|(m, _)| m.as_path() != Path::new("/") && path.starts_with(m))
        .max_by_key(|(m, _)| m.as_os_str().len())
        .filter(|(m, _)| !m.starts_with("/System/Volumes/Data"));
    if let Some((mount, &read_only)) = on_other_volume {
        // Read-only volumes (also sealed cryptexes) and the hidden system volumes (VM, Preboot,
        // Update, ...).
        if read_only || mount.starts_with("/System/Volumes") {
            return Some(TrashSkip::ProtectedLocation);
        }
        let rel = names(path.strip_prefix(mount).unwrap_or(path));
        if rel
            .first()
            .is_some_and(|n| n.as_bytes().starts_with(b".") || is(n, "Backups.backupdb"))
        {
            return Some(TrashSkip::ProtectedLocation);
        }
        return None;
    }
    if let Some(home) = home
        && let Ok(rel) = path.strip_prefix(home)
        && let Some(skip) = home_rule(&names(rel))
    {
        return Some(skip);
    }
    let data = Path::new("/System/Volumes/Data");
    if let Ok(rel) = path.strip_prefix(data) {
        return startup_rule(&names(rel));
    }
    startup_rule(&parts)
}

/// An absolute path with `.` removed, or `None` if it is relative or contains `..`.
fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => out.push(name),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

impl SpaceMap {
    /// Moves the items the user selected to the Trash (Finder's "Put Back" keeps working) and
    /// updates the totals without a rescan. Returns one result per item, in order. Only acts on
    /// these exact paths, and refuses anything outside the scanned root, the root itself,
    /// macOS system locations and structural folders (`~/Library`, `/Applications`, volume
    /// roots, ...), anything inside an app or other bundle, items already in the Trash, and
    /// items that are not part of the scan results. Refuses everything while a scan runs.
    pub fn move_to_trash(&self, selected: &[PathBuf]) -> Vec<TrashResult> {
        self.move_to_trash_with(selected, &mut trash_with_file_manager)
    }

    pub(crate) fn move_to_trash_with(
        &self,
        selected: &[PathBuf],
        trash: &mut TrashFn<'_>,
    ) -> Vec<TrashResult> {
        let busy = self
            .shared
            .scanning
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err();
        if busy {
            return selected
                .iter()
                .map(|path| TrashResult {
                    path: path.clone(),
                    outcome: TrashOutcome::Skipped(TrashSkip::ScanRunning),
                })
                .collect();
        }
        struct Release<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.shared.scanning);

        let home = ffi::home_dir().and_then(|h| ffi::real_path(&h).ok());
        let mounts: HashMap<PathBuf, bool> = ffi::mounts_by_path()
            .into_iter()
            .map(|(path, volume)| (path, volume.read_only))
            .collect();
        let normalized: Vec<Option<PathBuf>> = selected.iter().map(|p| normalize(p)).collect();
        // Shallowest first, so an enclosing folder is handled before the items inside it.
        let mut order: Vec<usize> = (0..selected.len()).collect();
        order.sort_by_key(|&i| normalized[i].as_ref().map_or(0, |p| p.components().count()));
        let mut outcomes: Vec<Option<TrashOutcome>> = vec![None; selected.len()];
        let mut moved: HashSet<PathBuf> = HashSet::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for index in order {
            let outcome = match &normalized[index] {
                None => TrashOutcome::Skipped(TrashSkip::InvalidPath),
                Some(path) => {
                    let inside_moved = path.ancestors().skip(1).any(|a| moved.contains(a));
                    if inside_moved || !seen.insert(path.clone()) {
                        TrashOutcome::Skipped(TrashSkip::InsideAnotherSelectedItem)
                    } else {
                        let outcome = self.trash_one(path, home.as_deref(), &mounts, trash);
                        if matches!(outcome, TrashOutcome::Moved { .. }) {
                            moved.insert(path.clone());
                        }
                        outcome
                    }
                }
            };
            outcomes[index] = Some(outcome);
        }
        selected
            .iter()
            .zip(outcomes)
            .map(|(path, outcome)| TrashResult {
                path: path.clone(),
                outcome: outcome.unwrap_or(TrashOutcome::Skipped(TrashSkip::InvalidPath)),
            })
            .collect()
    }

    fn trash_one(
        &self,
        path: &Path,
        home: Option<&Path>,
        mounts: &HashMap<PathBuf, bool>,
        trash: &mut TrashFn<'_>,
    ) -> TrashOutcome {
        let root = &self.shared.root;
        if path == root.as_path() {
            return TrashOutcome::Skipped(TrashSkip::ScanRoot);
        }
        if !path.starts_with(root) {
            return TrashOutcome::Skipped(TrashSkip::OutsideScannedRoot);
        }
        if let Some(skip) = protection(path, home, mounts) {
            return TrashOutcome::Skipped(skip);
        }
        let stat = match ffi::lstat(path) {
            Ok(stat) => stat,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return TrashOutcome::Skipped(TrashSkip::NotFound);
            }
            Err(err) => {
                return TrashOutcome::Failed {
                    error: err.to_string(),
                };
            }
        };
        // Only items the scan showed: a folder node, or a file in a folder that was read.
        let (folder, parent) = {
            let tree = read_tree(&self.shared.tree);
            if stat.is_dir {
                match tree.find(path) {
                    Some(id) => (Some(id), None),
                    None => return TrashOutcome::Skipped(TrashSkip::NotInScan),
                }
            } else {
                match path.parent().and_then(|p| tree.find(p)) {
                    Some(id) if tree.is_scanned(id) => (None, Some(id)),
                    _ => return TrashOutcome::Skipped(TrashSkip::NotInScan),
                }
            }
        };
        let trashed_to = match trash(path) {
            Ok(to) => to,
            Err(error) => return TrashOutcome::Failed { error },
        };
        let mut tree = write_tree(&self.shared.tree);
        let removed = match (folder, parent) {
            (Some(id), _) => tree.remove_folder(id).unwrap_or_default(),
            (None, Some(parent)) => {
                let name = path.file_name().map(OsStr::as_bytes).unwrap_or_default();
                tree.remove_file(parent, name, stat.alloc, stat.link_count <= 1)
            }
            (None, None) => Default::default(),
        };
        // If the Trash folder itself is inside the scanned root (scanning the home folder or the
        // startup disk), the item still takes space there until the Trash is emptied.
        if let Some(destination) = trashed_to.as_deref().and_then(Path::parent)
            && let Some(trash_node) = tree.find(destination)
        {
            tree.add_to_folder(trash_node, removed);
        }
        TrashOutcome::Moved {
            trashed_to,
            bytes: removed.bytes,
            files: removed.files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(path: &str) -> Option<TrashSkip> {
        let mounts: HashMap<PathBuf, bool> = [
            ("/", true),
            ("/System/Volumes/Data", false),
            ("/System/Volumes/VM", false),
            ("/Volumes/Backup", false),
            ("/Volumes/CD", true),
        ]
        .iter()
        .map(|(p, ro)| (PathBuf::from(p), *ro))
        .collect();
        protection(Path::new(path), Some(Path::new("/Users/me")), &mounts)
    }

    #[test]
    fn system_and_structural_locations_are_protected() {
        let protected = [
            "/",
            "/Applications",
            "/Library",
            "/Library/Caches",
            "/Library/LaunchDaemons/com.x.plist",
            "/System/Library/Fonts/x.ttf",
            "/System/Volumes/Data",
            "/System/Volumes/Data/Users",
            "/System/Volumes/Data/Users/me",
            "/System/Volumes/Data/.Spotlight-V100",
            "/usr/bin/true",
            "/usr/local",
            "/private/etc/hosts",
            "/private/var/db/x",
            "/private/var/folders/ab/xyz/T",
            "/Users",
            "/Users/Shared",
            "/Users/me",
            "/Users/me/Documents",
            "/Users/me/Library",
            "/Users/me/Library/Caches",
            "/Users/me/Library/Keychains/login.keychain-db",
            "/Users/me/Library/LaunchAgents/com.x.plist",
            "/Users/me/Library/CloudStorage/Dropbox",
            "/Users/other/Desktop",
            "/Volumes/Backup",
            "/Volumes/Backup/.Spotlight-V100",
            "/Volumes/Backup/Backups.backupdb",
            "/Applications/Utilities",
            "/System/Volumes/VM/swapfile0",
            "/Volumes/CD/track.aiff",
        ];
        for path in protected {
            assert_eq!(
                check(path),
                Some(TrashSkip::ProtectedLocation),
                "{path} should be protected"
            );
        }
    }

    #[test]
    fn bundles_and_trash_are_refused() {
        assert_eq!(
            check("/Applications/Foo.app/Contents/Resources/big.dat"),
            Some(TrashSkip::InsideBundle)
        );
        assert_eq!(
            check("/Users/me/Pictures/Photos Library.photoslibrary/originals"),
            Some(TrashSkip::InsideBundle)
        );
        assert_eq!(
            check("/Users/me/.Trash/old.zip"),
            Some(TrashSkip::AlreadyInTrash)
        );
        assert_eq!(
            check("/Volumes/Backup/.Trashes/501/x"),
            Some(TrashSkip::AlreadyInTrash)
        );
    }

    #[test]
    fn ordinary_items_are_allowed() {
        let allowed = [
            "/Applications/Foo.app",
            "/Library/Application Support/Foo",
            "/Users/me/Downloads/big.dmg",
            "/Users/me/Library/Caches/com.foo",
            "/Users/me/Library/Containers/com.foo",
            "/Users/me/Library/CloudStorage/Dropbox/old",
            "/Users/me/Movies/clip.mov",
            "/System/Volumes/Data/Users/me/Downloads/x.zip",
            "/usr/local/Cellar/foo",
            "/private/tmp/x",
            "/private/var/folders/ab/xyz/T/kipimo-test/x",
            "/opt/homebrew/Cellar/foo",
            "/Volumes/Backup/Projects",
        ];
        for path in allowed {
            assert_eq!(check(path), None, "{path} should be allowed");
        }
    }

    #[test]
    fn normalize_rejects_relative_and_parent_paths() {
        assert_eq!(normalize(Path::new("relative/x")), None);
        assert_eq!(normalize(Path::new("/a/../b")), None);
        assert_eq!(normalize(Path::new("/a/./b/")), Some(PathBuf::from("/a/b")));
    }
}
