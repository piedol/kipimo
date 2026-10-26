//! Moving files to the Trash.
//!
//! Uses `-[NSFileManager trashItemAtURL:resultingItemURL:error:]` (Foundation). Checked on
//! macOS 26.6: it records the original location in the Trash's `.DS_Store` (`ptbL`/`ptbN`
//! records, the same ones Finder writes), so Finder's "Put Back" works for items Kipimo
//! trashes. It moves the item at the URL itself, so a symlinked plist moves as a link and its
//! target (which may live inside an app bundle) is never touched.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSURL};

/// Moves one path to the Trash and returns where it landed.
pub(crate) trait Trasher {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String>;
}

/// The real Trash, through NSFileManager.
pub(crate) struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String> {
        // `symlink_metadata` so a dangling symlink can still be trashed.
        if std::fs::symlink_metadata(path).is_err() {
            return Err(format!("{} no longer exists", path.display()));
        }
        let url = NSURL::from_file_path(path).ok_or("path can't be expressed as a file URL")?;
        let manager = NSFileManager::defaultManager();
        let mut resulting = None;
        manager
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|e| e.localizedDescription().to_string())?;
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
        fn trash(&self, path: &Path) -> Result<Option<PathBuf>, String> {
            let dest = self.dir.join(path.file_name().ok_or("no file name")?);
            std::fs::rename(path, &dest).map_err(|e| e.to_string())?;
            self.moved.borrow_mut().push(path.to_path_buf());
            Ok(Some(dest))
        }
    }
}
