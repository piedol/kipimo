//! Installed apps: listing, protection rules, and the index the leftover finder checks against.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::env::Env;
use crate::native::{self, RunningApp};
use crate::walk::{self, WalkEvent};

/// The parts of a bundle's Info.plist this crate uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleInfo {
    pub bundle_id: Option<String>,
    /// Display name, falling back to the bundle name, then the file name.
    pub name: String,
    pub bundle_name: Option<String>,
    pub version: Option<String>,
    pub executable: Option<String>,
}

impl BundleInfo {
    pub fn read(bundle: &Path) -> Option<BundleInfo> {
        let p = native::read_info_plist(bundle)?;
        let stem = bundle
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let display = p.str("CFBundleDisplayName").map(str::to_owned);
        let bundle_name = p.str("CFBundleName").map(str::to_owned);
        Some(BundleInfo {
            bundle_id: p
                .str("CFBundleIdentifier")
                .map(str::to_owned)
                .filter(|s| !s.is_empty()),
            name: display
                .clone()
                .or(bundle_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or(stem),
            bundle_name,
            version: p
                .str("CFBundleShortVersionString")
                .or_else(|| p.str("CFBundleVersion"))
                .map(str::to_owned),
            executable: p.str("CFBundleExecutable").map(str::to_owned),
        })
    }
}

/// Why an app can't be uninstalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    /// Part of macOS (a platform binary, on the sealed system volume or under `/System`).
    #[serde(rename = "ships_with_macos")]
    ShipsWithMacOS,
    /// Kipimo itself.
    Kipimo,
}

/// An installed app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledApp {
    pub path: PathBuf,
    pub name: String,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    /// Allocated size of the bundle; `None` until measured.
    pub size_bytes: Option<u64>,
    /// Spotlight's last used date (Unix seconds).
    pub last_used_unix: Option<i64>,
    pub team_id: Option<String>,
    /// Set when the app must not be uninstalled.
    pub protected: Option<Protection>,
    /// A process is running from this bundle: the UI should offer "Quit first".
    pub running: bool,
}

/// Streamed by [`list_apps`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AppsEvent {
    /// An app was found (size not measured yet).
    Found { app: InstalledApp },
    /// An app's size is known.
    Sized { path: PathBuf, bytes: u64 },
    /// Running byte total while sizing.
    Progress { bytes: u64, files: u64 },
}

/// Every `.app` directly in the app folders or one subfolder down. Symlinks are skipped.
pub(crate) fn app_bundles(env: &Env) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in &env.app_dirs {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let path = entry.path();
            if is_app(&path) {
                out.push(path);
            } else if let Ok(sub) = fs::read_dir(&path) {
                let mut subs: Vec<_> = sub.flatten().collect();
                subs.sort_by_key(|e| e.file_name());
                for s in subs {
                    if s.file_type().is_ok_and(|t| t.is_dir()) && is_app(&s.path()) {
                        out.push(s.path());
                    }
                }
            }
        }
    }
    out
}

fn is_app(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "app")
}

/// The protection rule for one bundle.
pub(crate) fn protection(
    env: &Env,
    path: &Path,
    bundle_id: Option<&str>,
    platform: bool,
) -> Option<Protection> {
    if bundle_id.is_some() && bundle_id == env.own_bundle_id.as_deref() {
        return Some(Protection::Kipimo);
    }
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let system = fs::canonicalize(env.system_root.join("System"))
        .unwrap_or_else(|_| env.system_root.join("System"));
    if platform || real.starts_with(&system) || native::on_read_only_volume(&real) {
        return Some(Protection::ShipsWithMacOS);
    }
    None
}

/// A process is running from this bundle (compared after resolving symlinks).
pub(crate) fn is_running(app_path: &Path, running: &[RunningApp]) -> bool {
    let real = fs::canonicalize(app_path).unwrap_or_else(|_| app_path.to_path_buf());
    running.iter().any(|r| {
        r.bundle_path == app_path
            || r.bundle_path == real
            || fs::canonicalize(&r.bundle_path).is_ok_and(|p| p == real)
    })
}

/// Lists installed apps in `/Applications` and `~/Applications` (one level of subfolders), then
/// measures their sizes in parallel. Streams [`AppsEvent`]s; returns the full list (sizes filled
/// in unless cancelled).
pub fn list_apps(
    env: &Env,
    on_event: &mut dyn FnMut(AppsEvent),
    cancel: &AtomicBool,
) -> Vec<InstalledApp> {
    let running = env.running.snapshot();
    let mut apps = Vec::new();
    for path in app_bundles(env) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(info) = BundleInfo::read(&path) else {
            continue;
        };
        let signing = env.signing.read(&path);
        let app = InstalledApp {
            name: info.name.clone(),
            protected: protection(
                env,
                &path,
                info.bundle_id.as_deref(),
                signing.as_ref().is_some_and(|s| s.platform),
            ),
            running: is_running(&path, &running),
            bundle_id: info.bundle_id,
            version: info.version,
            size_bytes: None,
            last_used_unix: env.last_used.last_used(&path),
            team_id: signing.and_then(|s| s.team_id),
            path,
        };
        on_event(AppsEvent::Found { app: app.clone() });
        apps.push(app);
    }
    let roots: Vec<PathBuf> = apps.iter().map(|a| a.path.clone()).collect();
    let out = walk::measure(&roots, env.threads, cancel, &mut |e| match e {
        WalkEvent::Tick { bytes, files } => on_event(AppsEvent::Progress { bytes, files }),
        WalkEvent::Done { index, totals } => on_event(AppsEvent::Sized {
            path: roots[index].clone(),
            bytes: totals.bytes,
        }),
    });
    if !out.cancelled {
        for (app, t) in apps.iter_mut().zip(out.totals) {
            app.size_bytes = Some(t.bytes);
        }
    }
    apps
}

/// Who an app is, for the leftover finder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppTarget {
    pub bundle_id: String,
    /// Names to match folder names against (display name, bundle name, file name, executable).
    pub names: Vec<String>,
    pub team_id: Option<String>,
    /// App Groups from the app's entitlements.
    pub app_groups: Vec<String>,
    /// Bundle IDs of helpers, extensions and login items inside the bundle.
    pub related_ids: Vec<String>,
    /// The bundle, if it still exists (in `/Applications` or in the Trash).
    pub bundle_path: Option<PathBuf>,
}

impl AppTarget {
    /// Reads a bundle (installed, or already in the Trash). `None` without a bundle ID.
    pub fn from_bundle(env: &Env, bundle: &Path) -> Option<AppTarget> {
        Self::read(env, bundle, true)
    }

    fn read(env: &Env, bundle: &Path, with_signing: bool) -> Option<AppTarget> {
        let info = BundleInfo::read(bundle)?;
        let bundle_id = info.bundle_id.clone()?;
        let signing = if with_signing {
            env.signing.read(bundle).unwrap_or_default()
        } else {
            Default::default()
        };
        let mut names = Vec::new();
        for n in [
            Some(info.name.clone()),
            info.bundle_name.clone(),
            bundle.file_stem().map(|s| s.to_string_lossy().into_owned()),
            info.executable.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !n.is_empty() && !names.iter().any(|m: &String| m.eq_ignore_ascii_case(&n)) {
                names.push(n);
            }
        }
        Some(AppTarget {
            related_ids: embedded_ids(bundle, &bundle_id),
            bundle_id,
            names,
            team_id: signing.team_id,
            app_groups: signing.app_groups,
            bundle_path: Some(bundle.to_path_buf()),
        })
    }

    /// An app that's no longer installed, known only by its bundle ID (and maybe its name).
    pub fn from_bundle_id(bundle_id: &str, name: Option<&str>) -> AppTarget {
        AppTarget {
            bundle_id: bundle_id.to_owned(),
            names: name.map(|n| vec![n.to_owned()]).unwrap_or_default(),
            team_id: None,
            app_groups: Vec::new(),
            related_ids: Vec::new(),
            bundle_path: None,
        }
    }
}

/// Bundle IDs of nested bundles (extensions, helpers, login items, XPC services).
///
/// Bundles in `Frameworks` and `XPCServices` can carry a shared ID (a stock Electron helper, a
/// framework's XPC service), so those count only inside the app's own vendor namespace
/// (`com.example.*`). Extensions, login items, privileged helpers and system extensions are always
/// the app's own.
fn embedded_ids(bundle: &Path, main: &str) -> Vec<String> {
    const PLACES: &[(&str, bool)] = &[
        ("Contents/PlugIns", true),
        ("Contents/Extensions", true),
        ("Contents/Library/LoginItems", true),
        ("Contents/Library/LaunchServices", true),
        ("Contents/Library/SystemExtensions", true),
        ("Contents/Helpers", true),
        ("Contents/XPCServices", false),
        ("Contents/Frameworks", false),
    ];
    let main_vendor = vendor(main);
    let mut out: Vec<String> = Vec::new();
    for (place, own) in PLACES {
        let Ok(rd) = fs::read_dir(bundle.join(place)) else {
            continue;
        };
        for entry in rd.flatten().take(64) {
            let p = entry.path();
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let id = if matches!(ext.as_str(), "app" | "appex" | "xpc" | "systemextension") {
                BundleInfo::read(&p).and_then(|i| i.bundle_id)
            } else if *place == "Contents/Library/LaunchServices"
                && entry.file_type().is_ok_and(|t| t.is_file())
            {
                // Privileged helper tools are named by their bundle ID.
                Some(entry.file_name().to_string_lossy().into_owned())
            } else {
                None
            };
            if let Some(id) = id
                && id.contains('.')
                && (*own || vendor(&id) == main_vendor)
                && !id.eq_ignore_ascii_case(main)
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&id))
            {
                out.push(id);
            }
        }
    }
    out
}

/// The vendor namespace of a bundle ID: its first two components, lowercased (`com.example`).
pub(crate) fn vendor(id: &str) -> String {
    id.split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase()
}

/// One app in the [`AppIndex`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedApp {
    pub path: PathBuf,
    pub name: String,
    pub bundle_id: Option<String>,
    pub names: Vec<String>,
    pub team_id: Option<String>,
    pub app_groups: Vec<String>,
    pub related_ids: Vec<String>,
}

/// Every installed app (plus macOS's own apps and anything running), used to keep leftovers that
/// another app still uses. Build it once per finder run; it reads each app's Info.plist and code
/// signature.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AppIndex {
    pub apps: Vec<IndexedApp>,
}

impl AppIndex {
    pub fn build(env: &Env, cancel: &AtomicBool) -> AppIndex {
        // (path, read the code signature). macOS's own apps only need their IDs and names.
        let mut paths: Vec<(PathBuf, bool)> =
            app_bundles(env).into_iter().map(|p| (p, true)).collect();
        let system_apps = env.system_root.join("System/Applications");
        for dir in [system_apps.clone(), system_apps.join("Utilities")] {
            if let Ok(rd) = fs::read_dir(&dir) {
                paths.extend(
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| is_app(p))
                        .map(|p| (p, false)),
                );
            }
        }
        for r in env.running.snapshot() {
            if !paths.iter().any(|(p, _)| *p == r.bundle_path)
                && !r.bundle_path.starts_with(&system_apps)
            {
                paths.push((r.bundle_path, true));
            }
        }
        let mut apps = Vec::new();
        for (path, with_signing) in paths {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(target) = AppTarget::read(env, &path, with_signing) else {
                continue;
            };
            apps.push(IndexedApp {
                name: target.names.first().cloned().unwrap_or_default(),
                bundle_id: Some(target.bundle_id.clone()),
                names: target.names,
                team_id: target.team_id,
                app_groups: target.app_groups,
                related_ids: target.related_ids,
                path,
            });
        }
        AppIndex { apps }
    }

    /// The apps other than the one at `exclude` (compared after resolving symlinks).
    pub(crate) fn others<'a>(&'a self, exclude: Option<&Path>) -> Vec<&'a IndexedApp> {
        let ex = exclude.map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
        self.apps
            .iter()
            .filter(|a| match &ex {
                Some(ex) => {
                    a.path != *ex && fs::canonicalize(&a.path).map(|p| p != *ex).unwrap_or(true)
                }
                None => true,
            })
            .collect()
    }
}
