//! Moving files to the Trash.
//!
//! Uses `-[NSFileManager trashItemAtURL:resultingItemURL:error:]` (Foundation). Checked on
//! macOS 26.6: it records the original location in the Trash's `.DS_Store` (`ptbL`/`ptbN`
//! records, the same ones Finder writes), so Finder's "Put Back" works for items Kipimo
//! trashes. It moves the item at the URL itself, so a symlinked plist moves as a link and its
//! target (which may live inside an app bundle) is never touched.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSURL};

/// Why a move to the Trash failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrashFailure {
    /// macOS refused: no write permission, or (for an app bundle) App Management.
    Denied(String),
    Other(String),
}

impl std::fmt::Display for TrashFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashFailure::Denied(m) | TrashFailure::Other(m) => f.write_str(m),
        }
    }
}

/// Moves one path to the Trash and returns where it landed.
pub(crate) trait Trasher {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashFailure>;
}

/// The real Trash, through NSFileManager.
pub(crate) struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashFailure> {
        // `symlink_metadata` so a dangling symlink can still be trashed.
        if std::fs::symlink_metadata(path).is_err() {
            return Err(TrashFailure::Other(format!(
                "{} no longer exists",
                path.display()
            )));
        }
        let url = NSURL::from_file_path(path)
            .ok_or_else(|| TrashFailure::Other("path can't be expressed as a file URL".into()))?;
        let manager = NSFileManager::defaultManager();
        let mut resulting = None;
        manager
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|e| {
                let message = e.localizedDescription().to_string();
                // NSFileWriteNoPermissionError = 513, NSFileReadNoPermissionError = 257,
                // POSIX EACCES/EPERM.
                match e.code() {
                    513 | 257 => TrashFailure::Denied(message),
                    c if c == libc::EACCES as isize || c == libc::EPERM as isize => {
                        TrashFailure::Denied(message)
                    }
                    _ => TrashFailure::Other(message),
                }
            })?;
        Ok(resulting.and_then(|u| u.to_file_path()))
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;

    /// Moves items into a folder inside a test's temp dir instead of the real Trash.
    pub(crate) struct FolderTrash {
        pub dir: PathBuf,
        pub moved: RefCell<Vec<PathBuf>>,
    }

    impl FolderTrash {
        pub fn new(dir: PathBuf) -> FolderTrash {
            std::fs::create_dir_all(&dir).unwrap();
            FolderTrash {
                dir,
                moved: RefCell::new(Vec::new()),
            }
        }
    }

    impl Trasher for FolderTrash {
        fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashFailure> {
            let name = path
                .file_name()
                .ok_or_else(|| TrashFailure::Other("no file name".into()))?;
            let dest = self.dir.join(name);
            std::fs::rename(path, &dest).map_err(|e| TrashFailure::Other(e.to_string()))?;
            self.moved.borrow_mut().push(path.to_path_buf());
            Ok(Some(dest))
        }
    }
}
