//! The environment every tool runs against: which home folder, which system root, and the
//! providers for anything that touches the live system (running apps, the Trash, code signatures,
//! Spotlight). Tests build an [`Env`] on a temp folder with fakes; only
//! [`Env::current_user`] points at the real Mac.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::native::{self, RunningApp, SigningInfo, TrashError};

/// Lists running apps. Mock it in tests.
pub trait RunningApps: Send + Sync {
    fn snapshot(&self) -> Vec<RunningApp>;
}

/// Moves one item to the Trash and returns where it landed (if known).
pub trait Trasher: Send + Sync {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashError>;
}

/// Reads a bundle's code signature.
pub trait SigningReader: Send + Sync {
    fn read(&self, bundle: &Path) -> Option<SigningInfo>;
}

/// Reads when an app was last used (Unix seconds).
pub trait LastUsedReader: Send + Sync {
    fn last_used(&self, bundle: &Path) -> Option<i64>;
}

/// The live process list (libproc).
pub struct SystemRunningApps;
impl RunningApps for SystemRunningApps {
    fn snapshot(&self) -> Vec<RunningApp> {
        native::running_apps()
    }
}

/// A fixed list of running apps (for tests, or "nothing is running").
#[derive(Default, Clone)]
pub struct StaticRunningApps(pub Vec<RunningApp>);
impl RunningApps for StaticRunningApps {
    fn snapshot(&self) -> Vec<RunningApp> {
        self.0.clone()
    }
}

/// The real Trash, through `NSFileManager trashItemAtURL:` (keeps Finder's "Put Back").
pub struct SystemTrash;
impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashError> {
        native::trash_item(path)
    }
}

/// Refuses every move. The default for [`Env::new`], so a test can't reach the real Trash by
/// accident.
pub struct NoTrash;
impl Trasher for NoTrash {
    fn trash(&self, _path: &Path) -> Result<Option<PathBuf>, TrashError> {
        Err(TrashError::Other(
            "no Trash configured for this environment".into(),
        ))
    }
}

/// Code signatures read from each bundle's main executable (see the `codesig` module for why
/// this doesn't go through the Security framework).
pub struct SystemSigning;
impl SigningReader for SystemSigning {
    fn read(&self, bundle: &Path) -> Option<SigningInfo> {
        crate::codesig::read_bundle(bundle)
    }
}

/// No signing information (unsigned everything).
pub struct NoSigning;
impl SigningReader for NoSigning {
    fn read(&self, _bundle: &Path) -> Option<SigningInfo> {
        None
    }
}

/// Spotlight's `kMDItemLastUsedDate`.
pub struct SpotlightLastUsed;
impl LastUsedReader for SpotlightLastUsed {
    fn last_used(&self, bundle: &Path) -> Option<i64> {
        native::last_used(bundle)
    }
}

/// No last-used dates.
pub struct NoLastUsed;
impl LastUsedReader for NoLastUsed {
    fn last_used(&self, _bundle: &Path) -> Option<i64> {
        None
    }
}

/// Where and how the tools run.
#[derive(Clone)]
pub struct Env {
    /// The user's home folder. Every cleanup and leftover path lives under it.
    pub home: PathBuf,
    /// `/` on a real Mac. Used for `/Library` (reported, never touched), `/System` and CoreSimulator
    /// runtime metadata.
    pub system_root: PathBuf,
    /// Folders that hold installed apps (scanned one level deep).
    pub app_dirs: Vec<PathBuf>,
    /// The macOS build (`kern.osversion`), used to spot stale simulator caches.
    pub os_build: Option<String>,
    /// "Now" for age rules.
    pub now: SystemTime,
    /// Worker threads for size walks.
    pub threads: usize,
    /// Kipimo's own bundle ID: never offered for uninstall.
    pub own_bundle_id: Option<String>,
    pub running: Arc<dyn RunningApps>,
    pub trash: Arc<dyn Trasher>,
    pub signing: Arc<dyn SigningReader>,
    pub last_used: Arc<dyn LastUsedReader>,
}

impl Env {
    /// An environment rooted at `home` and `system_root` with inert providers: nothing running,
    /// no Trash (every move is refused), no signatures, no Spotlight. Swap providers as needed.
    pub fn new(home: impl Into<PathBuf>, system_root: impl Into<PathBuf>) -> Env {
        let home = home.into();
        let system_root = system_root.into();
        Env {
            app_dirs: vec![system_root.join("Applications"), home.join("Applications")],
            home,
            system_root,
            os_build: None,
            now: SystemTime::now(),
            threads: default_threads(),
            own_bundle_id: None,
            running: Arc::new(StaticRunningApps::default()),
            trash: Arc::new(NoTrash),
            signing: Arc::new(NoSigning),
            last_used: Arc::new(NoLastUsed),
        }
    }

    /// The real Mac for the current user: home from the password database, `/` as system root,
    /// live process list, the real Trash, embedded code signatures and Spotlight.
    pub fn current_user() -> std::io::Result<Env> {
        let home = native::user_home()
            .ok_or_else(|| std::io::Error::other("couldn't find the user's home folder"))?;
        let mut env = Env::new(home, "/");
        env.os_build = native::os_build();
        env.own_bundle_id = own_bundle_id();
        env.running = Arc::new(SystemRunningApps);
        env.trash = Arc::new(SystemTrash);
        env.signing = Arc::new(SystemSigning);
        env.last_used = Arc::new(SpotlightLastUsed);
        Ok(env)
    }

    /// `~/Library/<rel>`.
    pub fn library(&self, rel: &str) -> PathBuf {
        self.home.join("Library").join(rel)
    }

    /// `/Library/<rel>` under the system root.
    pub fn system_library(&self, rel: &str) -> PathBuf {
        self.system_root.join("Library").join(rel)
    }
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}

fn own_bundle_id() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bundle = native::outermost_app(&exe)?;
    crate::apps::BundleInfo::read(&bundle)?.bundle_id
}
