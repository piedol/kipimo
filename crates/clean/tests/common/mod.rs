//! A synthetic Mac in a temp folder: `<root>/home` (the user's home), `<root>/sys` (the system
//! root, with `/Applications` and `/Library`) and `<root>/trash` (where the fake Trash puts
//! things). Nothing here touches the real home folder, the real Trash or the real system.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kipimo_clean::{
    Env, RunningApp, SigningInfo, SigningReader, StaticRunningApps, TrashError, Trasher,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub root: PathBuf,
    pub home: PathBuf,
    pub sys: PathBuf,
    pub trash: Arc<FolderTrash>,
}

impl Fixture {
    pub fn new(name: &str) -> Fixture {
        Fixture::in_dir(&std::env::temp_dir(), name)
    }

    /// A fixture below `base` (e.g. on a throwaway disk image).
    pub fn in_dir(base: &Path, name: &str) -> Fixture {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = base.join(format!("kipimo-clean-{name}-{}-{n}", std::process::id()));
        if root.exists() {
            unlock(&root);
            fs::remove_dir_all(&root).unwrap();
        }
        let home = root.join("home");
        let sys = root.join("sys");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(sys.join("Applications")).unwrap();
        fs::create_dir_all(sys.join("Library")).unwrap();
        let trash = Arc::new(FolderTrash::new(root.join("trash")));
        // Resolve /var → /private/var so paths compare equal to what the crate canonicalizes.
        let root = fs::canonicalize(&root).unwrap();
        Fixture {
            home: root.join("home"),
            sys: root.join("sys"),
            trash,
            root,
        }
    }

    /// An environment on this fixture with the fake Trash and nothing running.
    pub fn env(&self) -> Env {
        let mut env = Env::new(&self.home, &self.sys);
        env.trash = self.trash.clone();
        env.threads = 4;
        env
    }

    pub fn h(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    pub fn s(&self, rel: &str) -> PathBuf {
        self.sys.join(rel)
    }

    /// Writes a file of `len` bytes (creating parent folders).
    pub fn file(&self, path: impl AsRef<Path>, len: usize) -> PathBuf {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0x5a; len]).unwrap();
        path
    }

    pub fn dir(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A minimal app bundle with an Info.plist.
    pub fn app(
        &self,
        path: impl AsRef<Path>,
        bundle_id: &str,
        name: &str,
        version: &str,
    ) -> PathBuf {
        let path = path.as_ref().to_path_buf();
        plist(
            &path.join("Contents/Info.plist"),
            &[
                ("CFBundleIdentifier", V::S(bundle_id)),
                ("CFBundleName", V::S(name)),
                ("CFBundleShortVersionString", V::S(version)),
                ("CFBundleExecutable", V::S(name)),
            ],
        );
        self.file(path.join("Contents/MacOS").join(name), 50_000);
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unlock(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Gives every folder back its permissions so the fixture can be removed.
pub fn unlock(root: &Path) {
    let Ok(meta) = fs::symlink_metadata(root) else {
        return;
    };
    if meta.is_dir() {
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o755));
        if let Ok(rd) = fs::read_dir(root) {
            for e in rd.flatten() {
                unlock(&e.path());
            }
        }
    }
}

pub fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Allocated size of one file.
pub fn alloc(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

pub fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(days * 86_400)
}

/// Sets a path's modification time (works for folders too).
pub fn set_mtime(path: &Path, t: SystemTime) {
    use std::os::unix::ffi::OsStrExt;
    let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs() as libc::time_t;
    let tv = [
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
    ];
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::utimes(c.as_ptr(), tv.as_ptr()) }, 0);
}

/// Plist values for [`plist`].
pub enum V<'a> {
    S(&'a str),
    B(bool),
    I(i64),
    /// ISO 8601, e.g. `2020-01-01T00:00:00Z`.
    D(String),
    A(Vec<&'a str>),
    Dict(Vec<(&'a str, V<'a>)>),
    Arr(Vec<V<'a>>),
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render(v: &V<'_>, out: &mut String) {
    match v {
        V::S(s) => out.push_str(&format!("<string>{}</string>", esc(s))),
        V::B(b) => out.push_str(if *b { "<true/>" } else { "<false/>" }),
        V::I(i) => out.push_str(&format!("<integer>{i}</integer>")),
        V::D(d) => out.push_str(&format!("<date>{d}</date>")),
        V::A(a) => {
            out.push_str("<array>");
            for s in a {
                out.push_str(&format!("<string>{}</string>", esc(s)));
            }
            out.push_str("</array>");
        }
        V::Arr(a) => {
            out.push_str("<array>");
            for v in a {
                render(v, out);
            }
            out.push_str("</array>");
        }
        V::Dict(d) => {
            out.push_str("<dict>");
            for (k, v) in d {
                out.push_str(&format!("<key>{}</key>", esc(k)));
                render(v, out);
            }
            out.push_str("</dict>");
        }
    }
}

/// Writes an XML property list with a top-level dictionary.
pub fn plist(path: &Path, entries: &[(&str, V<'_>)]) {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>",
    );
    for (k, v) in entries {
        body.push_str(&format!("<key>{}</key>", esc(k)));
        render(v, &mut body);
    }
    body.push_str("</dict></plist>\n");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// ISO 8601 for a time.
pub fn iso(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// A Trash that moves items into a folder of the fixture and records every call.
pub struct FolderTrash {
    pub dir: PathBuf,
    pub moved: Mutex<Vec<PathBuf>>,
}

impl FolderTrash {
    pub fn new(dir: PathBuf) -> FolderTrash {
        fs::create_dir_all(&dir).unwrap();
        FolderTrash {
            dir,
            moved: Mutex::new(Vec::new()),
        }
    }

    pub fn moved(&self) -> Vec<PathBuf> {
        self.moved.lock().unwrap().clone()
    }
}

impl Trasher for FolderTrash {
    fn trash(&self, path: &Path) -> Result<Option<PathBuf>, TrashError> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dest = self.dir.join(format!(
            "{n}-{}",
            path.file_name().unwrap().to_string_lossy()
        ));
        match fs::rename(path, &dest) {
            Ok(()) => {
                self.moved.lock().unwrap().push(path.to_path_buf());
                Ok(Some(dest))
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(TrashError::PermissionDenied(e.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TrashError::NotFound),
            Err(e) => Err(TrashError::Other(e.to_string())),
        }
    }
}

/// Signing information by bundle path.
#[derive(Default)]
pub struct FakeSigning(pub HashMap<PathBuf, SigningInfo>);

impl SigningReader for FakeSigning {
    fn read(&self, bundle: &Path) -> Option<SigningInfo> {
        self.0.get(bundle).cloned()
    }
}

pub fn signing(team: Option<&str>, platform: bool, groups: &[&str]) -> SigningInfo {
    SigningInfo {
        team_id: team.map(str::to_owned),
        platform,
        app_groups: groups.iter().map(|g| g.to_string()).collect(),
    }
}

pub fn running(bundle_path: &Path, bundle_id: &str, name: &str) -> Arc<StaticRunningApps> {
    Arc::new(StaticRunningApps(vec![RunningApp {
        bundle_path: bundle_path.to_path_buf(),
        bundle_id: Some(bundle_id.to_owned()),
        name: Some(name.to_owned()),
        executable: Some(name.to_owned()),
    }]))
}

/// A small "installed apps" world for the leftover finder and the uninstaller:
/// - `Target.app` (`com.acme.target`, "Target Pro", team ACME123456) with an extension, a login
///   item, a privileged helper and a stock Electron helper (a shared ID that must be ignored);
/// - `Sibling.app` from the same team, entitled to the shared group `ACME123456.shared`;
/// - `Other.app` (`com.other.tool`) that a LaunchAgent named after Target actually launches;
/// - `Target Pro Max.app` from another developer, whose name starts with Target's.
pub struct Acme {
    pub target: PathBuf,
    pub sibling: PathBuf,
    pub other: PathBuf,
    pub max: PathBuf,
    pub signing: FakeSigning,
}

pub fn acme(f: &Fixture) -> Acme {
    let target = f.app(
        f.s("Applications/Target.app"),
        "com.acme.target",
        "Target Pro",
        "2.1",
    );
    f.app(
        target.join("Contents/PlugIns/Share.appex"),
        "com.acme.target.share",
        "Share",
        "1",
    );
    f.app(
        target.join("Contents/Library/LoginItems/Helper.app"),
        "com.acme.target.helper",
        "Helper",
        "1",
    );
    f.app(
        target.join("Contents/Frameworks/Electron Helper.app"),
        "com.github.Electron.helper",
        "Electron Helper",
        "1",
    );
    f.file(
        target.join("Contents/Library/LaunchServices/com.acme.target.privhelper"),
        1_000,
    );
    let sibling = f.app(
        f.s("Applications/Sibling.app"),
        "com.acme.sibling",
        "Sibling",
        "1.0",
    );
    let other = f.app(
        f.s("Applications/Other.app"),
        "com.other.tool",
        "Other",
        "3.0",
    );
    let max = f.app(
        f.s("Applications/Target Pro Max.app"),
        "net.max.app",
        "Target Pro Max",
        "1.0",
    );

    let mut signing = FakeSigning::default();
    signing.0.insert(
        target.clone(),
        crate::common::signing(
            Some("ACME123456"),
            false,
            &[
                "ACME123456.com.acme.target",
                "ACME123456.shared",
                "group.com.acme.common",
            ],
        ),
    );
    signing.0.insert(
        sibling.clone(),
        crate::common::signing(Some("ACME123456"), false, &["ACME123456.shared"]),
    );
    signing.0.insert(
        other.clone(),
        crate::common::signing(Some("OTHER12345"), false, &[]),
    );
    signing.0.insert(
        max.clone(),
        crate::common::signing(Some("MAX1234567"), false, &[]),
    );

    let lib = |rel: &str| f.h(&format!("Library/{rel}"));
    for rel in [
        "Application Support/com.acme.target/db.sqlite",
        "Application Support/Target Pro/settings.json",
        "Application Support/Target Pro_Helper/state",
        "Application Support/Target Pro Max/state",
        "Application Support/Targetted/state",
        "Application Support/com.acme.targetx/state",
        "Application Support/com.acme.target.share/x",
        "Application Support/com.acme.sibling/x",
        "Caches/com.acme.target/cache.db",
        "Caches/com.acme.target.ShipIt/update.zip",
        "Caches/com.github.Electron.helper/x",
        "Containers/com.acme.target.share/Data/x",
        "Containers/ACME123456.com.acme.target.helper/Data/x",
        "Group Containers/ACME123456.com.acme.target/x",
        "Group Containers/ACME123456.shared/x",
        "Group Containers/group.com.acme.common/x",
        "Group Containers/OTHER12345.com.other.tool/x",
        "Saved Application State/com.acme.target.savedState/data.data",
        "HTTPStorages/com.acme.target/httpstorages.sqlite",
        "WebKit/com.acme.target/WebsiteData/x",
        "Logs/Target Pro/app.log",
        "Logs/com.acme.target/app.log",
    ] {
        f.file(lib(rel), 20_000);
    }
    for rel in [
        "Preferences/com.acme.target.plist",
        "Preferences/com.acme.target.share.plist",
        "Preferences/com.acme.targetx.plist",
        "Preferences/ByHost/com.acme.target.0123ABCD-0123-4567-89AB-0123456789AB.plist",
        "HTTPStorages/com.acme.target.binarycookies",
        "Cookies/com.acme.target.binarycookies",
    ] {
        f.file(lib(rel), 4_000);
    }
    let exe = |app: &Path, name: &str| app.join("Contents/MacOS").join(name).display().to_string();
    let target_exe = exe(&target, "Target Pro");
    let other_exe = exe(&other, "Other");
    plist(
        &lib("LaunchAgents/com.acme.target.updater.plist"),
        &[
            ("Label", V::S("com.acme.target.updater")),
            ("Program", V::S(&target_exe)),
        ],
    );
    plist(
        &lib("LaunchAgents/com.acme.target.bridge.plist"),
        &[
            ("Label", V::S("com.acme.target.bridge")),
            (
                "ProgramArguments",
                V::A(vec![other_exe.as_str(), "--bridge"]),
            ),
        ],
    );
    plist(
        &lib("LaunchAgents/com.random.job.plist"),
        &[
            ("Label", V::S("com.random.job")),
            ("Program", V::S(&target_exe)),
        ],
    );
    plist(
        &lib("LaunchAgents/com.zzz.assoc.plist"),
        &[
            ("Label", V::S("com.zzz.assoc")),
            ("Program", V::S("/usr/bin/true")),
            ("AssociatedBundleIdentifiers", V::A(vec!["com.acme.target"])),
        ],
    );
    plist(
        &lib("LaunchAgents/com.unrelated.plist"),
        &[
            ("Label", V::S("com.unrelated")),
            ("Program", V::S("/usr/bin/true")),
        ],
    );
    // A symlinked leftover pointing out of the home is never offered.
    let outside = f.root.join("outside-support");
    f.file(outside.join("big"), 500_000);
    std::os::unix::fs::symlink(&outside, lib("Application Support/com.acme.target.link")).unwrap();
    // System leftovers (reported, never moved).
    plist(
        &f.s("Library/LaunchDaemons/com.acme.target.privhelper.plist"),
        &[("Label", V::S("com.acme.target.privhelper"))],
    );
    f.file(
        f.s("Library/PrivilegedHelperTools/com.acme.target.privhelper"),
        30_000,
    );
    f.file(f.s("Library/Application Support/Target Pro/license"), 1_000);
    Acme {
        target,
        sibling,
        other,
        max,
        signing,
    }
}

/// A throwaway APFS disk image, detached on drop. Moving files on it to the Trash uses that
/// volume's own `.Trashes`, never the user's Trash.
pub struct DiskImage {
    pub mount: PathBuf,
}

impl DiskImage {
    pub fn attach(image: &Path, mount: &Path, name: &str) -> Option<DiskImage> {
        fs::create_dir_all(mount).ok()?;
        let created = Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-quiet", "-size", "16m", "-fs", "APFS", "-layout", "NONE",
            ])
            .args(["-volname", name])
            .arg(image)
            .status()
            .ok()?
            .success();
        let attached = created
            && Command::new("/usr/bin/hdiutil")
                .args(["attach", "-quiet", "-nobrowse", "-noverify", "-noautoopen"])
                .arg("-mountpoint")
                .arg(mount)
                .arg(image)
                .status()
                .ok()?
                .success();
        attached.then(|| DiskImage {
            mount: fs::canonicalize(mount).unwrap(),
        })
    }

    /// True when `path` is on this image (a separate volume), checked by device number.
    pub fn holds(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
        path.starts_with(&self.mount) && dev(&self.mount) != dev(&std::env::temp_dir())
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
/// folder's `.DS_Store`, and returns the ones still missing. `NSFileManager` writes the first
/// record at once and batches the rest seconds later, from inside this process.
pub fn missing_put_back(trashed: &[PathBuf], timeout: Duration) -> Vec<String> {
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `.DS_Store` records are keyed by the UTF-16BE file name; `ptbL` is the Put Back location.
fn has_put_back(trashed: &Path) -> bool {
    let (Some(dir), Some(name)) = (trashed.parent(), trashed.file_name()) else {
        return false;
    };
    let Ok(store) = fs::read(dir.join(".DS_Store")) else {
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
