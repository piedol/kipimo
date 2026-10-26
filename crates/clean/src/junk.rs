//! Tool A: the cleanup scanner. Finds known junk, sizes it, and moves what the user ticks to the
//! Trash.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::apps;
use crate::env::Env;
use crate::leftovers::{eq_ic, id_or_child};
use crate::native::RunningApp;
use crate::plist;
use crate::trash::{self, Zone};
use crate::types::{ItemResult, Risk, SkipReason};
use crate::walk::{self, SizeTotals, WalkEvent};

/// A junk category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryId {
    AppCaches,
    SystemCaches,
    Logs,
    XcodeDerivedData,
    XcodeArchives,
    XcodeDeviceSupport,
    XcodeSimulators,
    DeveloperCaches,
    MailDownloads,
    IosBackups,
    Trash,
}

/// What the UI does with a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryAction {
    /// Ticked items go to the Trash through [`clean`].
    MoveToTrash,
    /// Report only: emptying the Trash goes through Finder (with Finder's own confirmation).
    EmptyTrashInFinder,
}

/// Static facts about a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CategoryInfo {
    pub id: CategoryId,
    pub title: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    /// Whether the category starts ticked.
    pub default_selected: bool,
    pub action: CategoryAction,
}

impl CategoryId {
    /// Every category, in display order.
    pub const ALL: [CategoryId; 11] = [
        CategoryId::AppCaches,
        CategoryId::SystemCaches,
        CategoryId::Logs,
        CategoryId::XcodeDerivedData,
        CategoryId::XcodeArchives,
        CategoryId::XcodeDeviceSupport,
        CategoryId::XcodeSimulators,
        CategoryId::DeveloperCaches,
        CategoryId::MailDownloads,
        CategoryId::IosBackups,
        CategoryId::Trash,
    ];

    pub fn info(self) -> CategoryInfo {
        use CategoryAction::*;
        let (title, description, risk, default_selected, action) = match self {
            CategoryId::AppCaches => (
                "App caches",
                "Files apps keep in ~/Library/Caches to start faster. Apps rebuild them as needed. Caches of apps that are running are left alone.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::SystemCaches => (
                "macOS caches",
                "Caches of macOS's own services (com.apple.*). macOS rebuilds them, sometimes slowly. Caches that hold sync, sign-in or download state (CloudKit, iCloud, App Store, Photos analysis and similar) are never offered.",
                Risk::Medium,
                false,
                MoveToTrash,
            ),
            CategoryId::Logs => (
                "Logs and crash reports",
                "App logs in ~/Library/Logs and crash reports in DiagnosticReports. Only useful when troubleshooting.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::XcodeDerivedData => (
                "Xcode build data",
                "DerivedData: intermediate build products and indexes. Xcode rebuilds them on the next build. Skipped while Xcode is running.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::XcodeArchives => (
                "Old Xcode archives",
                "Archives older than the age limit. They hold the dSYMs needed to read crash reports from builds you shipped, so they start unticked.",
                Risk::Medium,
                false,
                MoveToTrash,
            ),
            CategoryId::XcodeDeviceSupport => (
                "Old device symbols",
                "iOS, watchOS, tvOS and visionOS DeviceSupport for versions no device has used recently. The newest version per platform is kept; Xcode copies symbols again when a device connects.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::XcodeSimulators => (
                "Unavailable simulators",
                "Simulator devices whose runtime is no longer installed (they can't boot), deleted devices, and simulator caches for runtimes or macOS versions that are gone.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::DeveloperCaches => (
                "Developer caches",
                "Package downloads kept by Homebrew, npm, Yarn, pnpm, Cargo, pip, Gradle, CocoaPods and Go. Tools download them again when needed. Finish running builds first.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::MailDownloads => (
                "Mail downloads",
                "Copies of attachments Mail saved when you opened them. The originals stay in your mailboxes.",
                Risk::Low,
                true,
                MoveToTrash,
            ),
            CategoryId::IosBackups => (
                "Old iPhone and iPad backups",
                "Local device backups older than the age limit. Never ticked: a backup can be the only copy of a device's messages, photos and health data. Tick one only if that device is backed up elsewhere, for example in iCloud.",
                Risk::High,
                false,
                MoveToTrash,
            ),
            CategoryId::Trash => (
                "Trash",
                "What's already in the Trash. Shown for reference; empty it from Finder.",
                Risk::Low,
                false,
                EmptyTrashInFinder,
            ),
        };
        CategoryInfo {
            id: self,
            title,
            description,
            risk,
            default_selected,
            action,
        }
    }
}

/// One cleanable item (a folder or file moved as a whole).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JunkItem {
    pub category: CategoryId,
    pub path: PathBuf,
    /// A short display name.
    pub label: String,
    /// Allocated bytes.
    pub bytes: u64,
    pub files: u64,
    pub default_selected: bool,
    pub note: Option<String>,
    pub modified_unix: Option<i64>,
    /// Folders inside it that couldn't be read (its size is a lower bound).
    pub unreadable: u64,
}

/// Something the scan saw but doesn't offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedEntry {
    pub path: PathBuf,
    pub label: String,
    pub reason: SkipReason,
    /// Measured only where it helps (an app's cache that's in use, a recent backup).
    pub bytes: Option<u64>,
}

/// One category's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryReport {
    #[serde(flatten)]
    pub info: CategoryInfo,
    /// Largest first.
    pub items: Vec<JunkItem>,
    pub skipped: Vec<SkippedEntry>,
    pub total_bytes: u64,
    /// The total of the items ticked by default.
    pub selected_bytes: u64,
    /// Folders that couldn't be read, across all items.
    pub unreadable: u64,
    /// The category's folder itself couldn't be read: Kipimo likely needs Full Disk Access.
    pub root_unreadable: bool,
    /// False if the scan was cancelled before this category finished.
    pub complete: bool,
}

/// The result of [`scan_junk`].
#[derive(Debug, Clone, Serialize)]
pub struct JunkReport {
    /// In display order.
    pub categories: Vec<CategoryReport>,
    /// The total of every cleanable item (the Trash excluded).
    pub total_bytes: u64,
    /// The total of the items ticked by default (what the weekly summary reports).
    pub selected_bytes: u64,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    /// More hard-linked files than the dedupe table holds; sizes may count some twice.
    pub hardlink_cap_hit: bool,
}

/// Scan settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanOptions {
    /// Categories to scan (all by default).
    pub categories: Vec<CategoryId>,
    /// Xcode archives older than this are offered.
    pub archive_age_days: u64,
    /// Device symbols touched within this many days are kept.
    pub device_support_keep_days: u64,
    /// Device backups older than this are offered.
    pub backup_age_days: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            categories: CategoryId::ALL.to_vec(),
            archive_age_days: 180,
            device_support_keep_days: 90,
            backup_age_days: 90,
        }
    }
}

/// Streamed by [`scan_junk`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ScanEvent {
    /// The categories about to be scanned.
    Started { categories: Vec<CategoryInfo> },
    /// Running totals, about every 100 ms.
    Progress { bytes: u64, files: u64 },
    /// A category is fully measured (they finish in any order).
    Category { report: CategoryReport },
}

/// Streamed by [`clean`].
#[derive(Debug, Clone, Serialize)]
pub struct CleanProgress {
    pub done: usize,
    pub total: usize,
    pub current: Option<PathBuf>,
}

/// The result of [`clean`].
#[derive(Debug, Clone, Serialize)]
pub struct CleanReport {
    pub results: Vec<ItemResult>,
    pub moved_bytes: u64,
    pub moved_count: usize,
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------------------------
// Discovery

struct Found {
    path: PathBuf,
    label: String,
    default_selected: bool,
    note: Option<String>,
    modified: Option<i64>,
}

struct Skip {
    found: Found,
    reason: SkipReason,
    measure: bool,
}

#[derive(Default)]
struct Discovery {
    items: Vec<Found>,
    skipped: Vec<Skip>,
    root_unreadable: bool,
}

impl Discovery {
    fn item(
        &mut self,
        path: PathBuf,
        label: impl Into<String>,
        default_selected: bool,
    ) -> &mut Found {
        let modified = mtime(&path);
        self.items.push(Found {
            path,
            label: label.into(),
            default_selected,
            note: None,
            modified,
        });
        self.items.last_mut().expect("just pushed")
    }

    fn skip(&mut self, path: PathBuf, label: impl Into<String>, reason: SkipReason, measure: bool) {
        let modified = mtime(&path);
        self.skipped.push(Skip {
            found: Found {
                path,
                label: label.into(),
                default_selected: false,
                note: None,
                modified,
            },
            reason,
            measure,
        });
    }
}

/// Developer caches: (label, path under home, ticked by default, note).
const DEV_CACHES: &[(&str, &str, bool, Option<&str>)] = &[
    ("Homebrew downloads", "Library/Caches/Homebrew", true, None),
    ("npm cache", ".npm/_cacache", true, None),
    ("Yarn cache", "Library/Caches/Yarn", true, None),
    ("Yarn (Berry) cache", ".yarn/berry/cache", true, None),
    (
        "pnpm store",
        "Library/pnpm/store",
        false,
        Some(
            "Projects hard-link their packages from this store, so moving it frees little until they reinstall.",
        ),
    ),
    ("pnpm metadata cache", "Library/Caches/pnpm", true, None),
    (
        "Cargo registry downloads",
        ".cargo/registry/cache",
        true,
        None,
    ),
    ("Cargo registry sources", ".cargo/registry/src", true, None),
    ("Cargo git checkouts", ".cargo/git/checkouts", true, None),
    ("pip cache", "Library/Caches/pip", true, None),
    ("Gradle caches", ".gradle/caches", true, None),
    ("CocoaPods cache", "Library/Caches/CocoaPods", true, None),
    ("Go build cache", "Library/Caches/go-build", true, None),
];

/// Apple-owned cache folders that aren't named `com.apple.*`.
const APPLE_CACHE_NAMES: &[&str] = &[
    "CloudKit",
    "FamilyCircle",
    "GameKit",
    "GeoServices",
    "TemporaryItems",
];

/// macOS caches that are never offered, by exact name or `prefix*`. Reasons, by group:
/// - sync and account state: deleting can force a full re-sync or sign-out (CloudKit, iCloud
///   Drive, Photos, Music library, Family Sharing, Calendar/Contacts/Reminders accounts, Home,
///   Wallet, Find My, Apple Account and AuthKit);
/// - work in flight: background downloads and App Store installs (nsurlsessiond, appstoreagent,
///   storekitagent, managed app distribution);
/// - expensive to rebuild: on-device analysis and compiled models (mediaanalysisd,
///   photoanalysisd, e5rt Neural Engine caches), Spotlight, Siri Suggestions learning
///   (duetexpertd, proactive), Launch Services;
/// - security and bookkeeping macOS manages itself (trustd, security, containermanagerd,
///   cache_delete, ad privacy, Screen Time);
/// - `TemporaryItems`, where apps stage files mid-save.
const ESSENTIAL_CACHES: &[&str] = &[
    "CloudKit",
    "FamilyCircle",
    "TemporaryItems",
    "com.apple.bird",
    "com.apple.cloudd",
    "com.apple.cloudphotod",
    "com.apple.CloudPhotosConfiguration",
    "com.apple.icloud*",
    "com.apple.iCloud*",
    "com.apple.findmy*",
    "com.apple.akd",
    "com.apple.appleaccountd",
    "com.apple.amsaccountsd",
    "com.apple.AuthenticationServices*",
    "com.apple.iTunesCloud",
    "com.apple.itunescloudd",
    "com.apple.dataaccess*",
    "com.apple.remindd",
    "com.apple.HomeKit",
    "com.apple.homed",
    "com.apple.passd",
    "com.apple.nsurlsessiond",
    "com.apple.appstore*",
    "com.apple.storekitagent",
    "com.apple.managedappdistribution*",
    "com.apple.commerce*",
    "com.apple.mediaanalysisd",
    "com.apple.photoanalysisd",
    "com.apple.Photos*",
    "com.apple.e5rt*",
    "com.apple.Spotlight",
    "com.apple.duetexpertd",
    "com.apple.proactive*",
    "com.apple.LaunchServices*",
    "com.apple.trustd",
    "com.apple.security*",
    "com.apple.containermanagerd",
    "com.apple.cache_delete",
    "com.apple.ap.adprivacyd",
    "com.apple.ScreenTimeAgent",
];

/// Known cache folder names of popular apps that don't follow the bundle-ID convention.
const APP_CACHE_ALIASES: &[(&str, &[&str])] = &[
    ("com.google.Chrome", &["Google"]),
    ("com.google.Chrome.canary", &["Google"]),
    ("org.mozilla.firefox", &["Firefox", "Mozilla"]),
    ("com.microsoft.edgemac", &["Microsoft Edge"]),
    ("com.brave.Browser", &["BraveSoftware"]),
    ("com.vivaldi.Vivaldi", &["Vivaldi"]),
];

fn wildcard(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            name.len() >= prefix.len()
                && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
        None => eq_ic(pattern, name),
    }
}

fn is_apple_cache(name: &str) -> bool {
    wildcard("com.apple.*", name) || APPLE_CACHE_NAMES.iter().any(|n| eq_ic(n, name))
}

fn is_essential_cache(name: &str) -> bool {
    ESSENTIAL_CACHES.iter().any(|p| wildcard(p, name))
}

/// The running app that owns a folder named `name`, if any.
fn running_owner<'a>(name: &str, running: &'a [RunningApp]) -> Option<&'a RunningApp> {
    running.iter().find(|r| {
        r.bundle_id
            .as_deref()
            .is_some_and(|b| id_or_child(name, b).is_some())
            || r.name.as_deref().is_some_and(|n| eq_ic(n, name))
            || r.executable.as_deref().is_some_and(|n| eq_ic(n, name))
            || r.bundle_id.as_deref().is_some_and(|b| {
                APP_CACHE_ALIASES
                    .iter()
                    .any(|(id, dirs)| eq_ic(id, b) && dirs.iter().any(|d| eq_ic(d, name)))
            })
    })
}

fn app_label(r: &RunningApp) -> String {
    r.name
        .clone()
        .or_else(|| r.bundle_id.clone())
        .unwrap_or_else(|| "an app".into())
}

fn is_running_id(running: &[RunningApp], id: &str) -> Option<String> {
    running
        .iter()
        .find(|r| r.bundle_id.as_deref().is_some_and(|b| eq_ic(b, id)))
        .map(app_label)
}

fn to_unix(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn mtime(path: &Path) -> Option<i64> {
    fs::symlink_metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(to_unix)
}

fn now_unix(env: &Env) -> i64 {
    to_unix(env.now).unwrap_or(0)
}

fn older_than(env: &Env, modified: Option<i64>, days: u64) -> bool {
    modified.is_some_and(|m| now_unix(env) - m > (days as i64) * 86_400)
}

struct Child {
    name: String,
    path: PathBuf,
    is_dir: bool,
    is_link: bool,
}

/// The entries of a folder, sorted by name. `Err` if it can't be read for lack of permission.
fn children(dir: &Path) -> Result<Vec<Child>, bool> {
    let rd = fs::read_dir(dir)
        .map_err(|e| matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)))?;
    let mut out: Vec<Child> = rd
        .flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            Some(Child {
                name: e.file_name().to_string_lossy().into_owned(),
                path: e.path(),
                is_dir: ft.is_dir(),
                is_link: ft.is_symlink(),
            })
        })
        .filter(|c| c.name != ".DS_Store" && c.name != ".localized")
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A category root is usable if it's a real folder whose resolved path stays in the home.
fn root_in_home(env: &Env, root: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(root) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    match (fs::canonicalize(root), fs::canonicalize(&env.home)) {
        (Ok(r), Ok(h)) => r.starts_with(h),
        _ => false,
    }
}

/// Lists the children of `root` as items through `each`, handling unreadable roots and symlinks.
fn for_children(
    env: &Env,
    d: &mut Discovery,
    root: &Path,
    mut each: impl FnMut(&mut Discovery, Child),
) {
    if fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        d.skip(root.to_path_buf(), label, SkipReason::Symlink, false);
        return;
    }
    if !root_in_home(env, root) {
        if fs::symlink_metadata(root).is_ok() {
            let label = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            d.skip(root.to_path_buf(), label, SkipReason::OutsideHome, false);
        }
        return;
    }
    match children(root) {
        Ok(list) => {
            for c in list {
                if c.is_link {
                    d.skip(c.path, c.name, SkipReason::Symlink, false);
                } else {
                    each(d, c);
                }
            }
        }
        Err(permission) => d.root_unreadable |= permission,
    }
}

fn discover(env: &Env, id: CategoryId, opts: &ScanOptions, running: &[RunningApp]) -> Discovery {
    let mut d = Discovery::default();
    match id {
        CategoryId::AppCaches | CategoryId::SystemCaches => {
            let apple = id == CategoryId::SystemCaches;
            let claimed: HashSet<String> = DEV_CACHES
                .iter()
                .filter_map(|(_, rel, _, _)| rel.strip_prefix("Library/Caches/"))
                .map(str::to_ascii_lowercase)
                .collect();
            for_children(env, &mut d, &env.library("Caches"), |d, c| {
                if claimed.contains(&c.name.to_ascii_lowercase())
                    || is_apple_cache(&c.name) != apple
                {
                    return;
                }
                if apple && is_essential_cache(&c.name) {
                    d.skip(c.path, c.name, SkipReason::Essential, false);
                } else if let Some(owner) = running_owner(&c.name, running) {
                    d.skip(
                        c.path,
                        c.name,
                        SkipReason::InUse {
                            app: app_label(owner),
                        },
                        true,
                    );
                } else {
                    d.item(c.path, c.name, !apple);
                }
            });
        }
        CategoryId::Logs => {
            for_children(env, &mut d, &env.library("Logs"), |d, c| {
                if c.name == "DiagnosticReports" && c.is_dir {
                    for_children(env, d, &c.path, |d, r| {
                        d.item(r.path, format!("Crash report: {}", r.name), true);
                    });
                } else if let Some(owner) = running_owner(&c.name, running) {
                    d.skip(
                        c.path,
                        c.name,
                        SkipReason::InUse {
                            app: app_label(owner),
                        },
                        true,
                    );
                } else {
                    d.item(c.path, c.name, true);
                }
            });
        }
        CategoryId::XcodeDerivedData => {
            let xcode = is_running_id(running, "com.apple.dt.Xcode");
            for_children(
                env,
                &mut d,
                &env.library("Developer/Xcode/DerivedData"),
                |d, c| match &xcode {
                    Some(app) => {
                        d.skip(c.path, c.name, SkipReason::InUse { app: app.clone() }, true)
                    }
                    None => {
                        d.item(c.path, c.name, true);
                    }
                },
            );
        }
        CategoryId::XcodeArchives => {
            for_children(
                env,
                &mut d,
                &env.library("Developer/Xcode/Archives"),
                |d, day| {
                    if !day.is_dir {
                        return;
                    }
                    for_children(env, d, &day.path, |d, a| {
                        if !a.name.ends_with(".xcarchive") {
                            return;
                        }
                        let created = plist::read(&a.path.join("Info.plist"))
                            .and_then(|p| p.get("CreationDate").and_then(|v| v.as_date()))
                            .map(|t| t as i64)
                            .or_else(|| mtime(&a.path));
                        let label = a.name.trim_end_matches(".xcarchive").to_owned();
                        if older_than(env, created, opts.archive_age_days) {
                            d.item(a.path, label, true).modified = created;
                        } else {
                            d.skip(a.path, label, SkipReason::Recent, false);
                        }
                    });
                },
            );
        }
        CategoryId::XcodeDeviceSupport => device_support(env, &mut d, opts),
        CategoryId::XcodeSimulators => simulators(env, &mut d),
        CategoryId::DeveloperCaches => {
            for (label, rel, selected, note) in DEV_CACHES {
                let path = env.home.join(rel);
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    d.skip(path, *label, SkipReason::Symlink, false);
                } else if !meta.is_dir() || !root_in_home(env, &path) {
                    d.skip(path, *label, SkipReason::OutsideHome, false);
                } else {
                    d.item(path, *label, *selected).note = note.map(str::to_owned);
                }
            }
        }
        CategoryId::MailDownloads => {
            for root in [
                env.library("Containers/com.apple.mail/Data/Library/Mail Downloads"),
                env.library("Mail Downloads"),
            ] {
                if fs::symlink_metadata(&root).is_ok() {
                    for_children(env, &mut d, &root, |d, c| {
                        d.item(c.path, c.name, true);
                    });
                }
            }
        }
        CategoryId::IosBackups => {
            for_children(
                env,
                &mut d,
                &env.library("Application Support/MobileSync/Backup"),
                |d, c| {
                    if !c.is_dir {
                        return;
                    }
                    let info = plist::read(&c.path.join("Info.plist"));
                    let label = info
                        .as_ref()
                        .and_then(|p| p.str("Device Name").or_else(|| p.str("Display Name")))
                        .map(|n| format!("{n} backup"))
                        .unwrap_or_else(|| "Device backup".to_owned());
                    let last = info
                        .as_ref()
                        .and_then(|p| p.get("Last Backup Date").and_then(|v| v.as_date()))
                        .map(|t| t as i64)
                        .or_else(|| mtime(&c.path));
                    if older_than(env, last, opts.backup_age_days) {
                        d.item(c.path, label, false).modified = last;
                    } else {
                        d.skip(c.path, label, SkipReason::Recent, true);
                    }
                },
            );
        }
        CategoryId::Trash => {
            let root = env.home.join(".Trash");
            match children(&root) {
                Ok(list) => {
                    let n = list.len();
                    d.item(
                        root,
                        format!("{n} item{} in the Trash", if n == 1 { "" } else { "s" }),
                        false,
                    );
                }
                Err(permission) => d.root_unreadable |= permission,
            }
        }
    }
    d
}

fn parse_version(name: &str) -> Vec<u32> {
    let head = name.split(" (").next().unwrap_or(name);
    head.split_whitespace()
        .rev()
        .find_map(|tok| {
            let parts: Option<Vec<u32>> = tok.split('.').map(|p| p.parse().ok()).collect();
            parts.filter(|p| !p.is_empty())
        })
        .unwrap_or_default()
}

fn device_support(env: &Env, d: &mut Discovery, opts: &ScanOptions) {
    let xcode = env.library("Developer/Xcode");
    let Ok(platforms) = children(&xcode) else {
        return;
    };
    for platform in platforms
        .into_iter()
        .filter(|p| p.is_dir && !p.is_link && p.name.ends_with(" DeviceSupport"))
    {
        let mut entries: Vec<(Child, Vec<u32>, Option<i64>)> = Vec::new();
        for_children(env, d, &platform.path, |_, c| {
            if c.is_dir {
                let touched = [
                    mtime(&c.path),
                    mtime(&c.path.join("Info.plist")),
                    mtime(&c.path.join(".finalized")),
                ]
                .into_iter()
                .flatten()
                .max();
                let v = parse_version(&c.name);
                entries.push((c, v, touched));
            }
        });
        let newest = entries.iter().map(|(_, v, _)| v.clone()).max();
        let os = platform.name.trim_end_matches(" DeviceSupport").to_owned();
        for (c, v, touched) in entries {
            let label = format!("{os} {}", c.name);
            if Some(&v) == newest.as_ref() {
                d.skip(c.path, label, SkipReason::Newest, false);
            } else if !older_than(env, touched, opts.device_support_keep_days) {
                d.skip(c.path, label, SkipReason::Recent, false);
            } else {
                d.item(c.path, label, true).modified = touched;
            }
        }
    }
}

/// Simulator runtimes installed on this Mac, from CoreSimulator's own metadata (no `xcrun`).
/// `None` if none could be found, in which case no device is called unavailable.
fn installed_runtimes(env: &Env) -> Option<HashSet<String>> {
    let mut ids = HashSet::new();
    let cs = env.system_library("Developer/CoreSimulator");
    // Xcode 15+: runtime disk images registered in images.plist.
    if let Some(p) = plist::read(&cs.join("Images/images.plist"))
        && let Some(images) = p.get("images").and_then(|i| i.as_array())
    {
        for image in images {
            if let Some(id) = image
                .get("runtimeInfo")
                .and_then(|r| r.str("bundleIdentifier"))
            {
                ids.insert(id.to_owned());
            }
        }
    }
    let mut runtime_dirs = vec![cs.join("Profiles/Runtimes")];
    if let Ok(vols) = fs::read_dir(cs.join("Volumes")) {
        runtime_dirs.extend(vols.flatten().map(|v| {
            v.path()
                .join("Library/Developer/CoreSimulator/Profiles/Runtimes")
        }));
    }
    // Older Xcodes bundled their runtimes.
    for app in apps::app_bundles(env) {
        if let Ok(platforms) = fs::read_dir(app.join("Contents/Developer/Platforms")) {
            runtime_dirs.extend(platforms.flatten().map(|p| {
                p.path()
                    .join("Library/Developer/CoreSimulator/Profiles/Runtimes")
            }));
        }
    }
    for dir in runtime_dirs {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for rt in rd.flatten() {
            if let Some(id) = plist::read(&rt.path().join("Contents/Info.plist"))
                .and_then(|p| p.str("CFBundleIdentifier").map(str::to_owned))
            {
                ids.insert(id);
            }
        }
    }
    (!ids.is_empty()).then_some(ids)
}

fn short_runtime(id: &str) -> String {
    id.rsplit('.')
        .next()
        .unwrap_or(id)
        .replacen('-', " ", 1)
        .replace('-', ".")
}

fn simulators(env: &Env, d: &mut Discovery) {
    let runtimes = installed_runtimes(env);
    for root in [
        env.library("Developer/CoreSimulator/Devices"),
        env.library("Developer/XCTestDevices"),
    ] {
        if fs::symlink_metadata(&root).is_err() {
            continue;
        }
        for_children(env, d, &root, |d, c| {
            if !c.is_dir {
                return;
            }
            let Some(p) = plist::read(&c.path.join("device.plist")) else {
                return;
            };
            let name = p.str("name").unwrap_or("Simulator").to_owned();
            let runtime = p.str("runtime").unwrap_or_default().to_owned();
            let deleted = p
                .get("isDeleted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let missing = runtimes
                .as_ref()
                .is_some_and(|r| !runtime.is_empty() && !r.contains(&runtime));
            if deleted || missing {
                let why = if deleted {
                    "deleted device"
                } else {
                    "runtime not installed"
                };
                let item = d.item(
                    c.path,
                    format!("{name} ({})", short_runtime(&runtime)),
                    true,
                );
                item.note = Some(why.to_owned());
            }
        });
    }
    // dyld caches: <macOS build>/<runtime id>.<runtime build>.
    let dyld = env.library("Developer/CoreSimulator/Caches/dyld");
    if fs::symlink_metadata(&dyld).is_ok() {
        for_children(env, d, &dyld, |d, host| {
            if !host.is_dir {
                return;
            }
            if env.os_build.as_deref().is_some_and(|b| b != host.name) {
                d.item(
                    host.path,
                    format!("Simulator caches for macOS {}", host.name),
                    true,
                )
                .note = Some("built for an older macOS".to_owned());
                return;
            }
            let Some(runtimes) = &runtimes else { return };
            for_children(env, d, &host.path, |d, rt| {
                let id = rt
                    .name
                    .rsplit_once('.')
                    .map(|(id, _)| id)
                    .unwrap_or(&rt.name);
                if id.starts_with("com.apple.CoreSimulator.SimRuntime.") && !runtimes.contains(id) {
                    d.item(
                        rt.path,
                        format!("Simulator caches for {}", short_runtime(id)),
                        true,
                    )
                    .note = Some("runtime not installed".to_owned());
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Scan and clean

fn categories_in_order(opts: &ScanOptions) -> Vec<CategoryId> {
    CategoryId::ALL
        .iter()
        .copied()
        .filter(|c| opts.categories.contains(c))
        .collect()
}

enum Slot {
    Item(usize, usize),
    Skipped(usize, usize),
}

fn build_report(
    id: CategoryId,
    d: &Discovery,
    item_sizes: &[Option<SizeTotals>],
    skip_sizes: &[Option<SizeTotals>],
    complete: bool,
) -> CategoryReport {
    let info = id.info();
    let mut items: Vec<JunkItem> = d
        .items
        .iter()
        .zip(item_sizes)
        .filter_map(|(f, t)| {
            let t = t.unwrap_or_default();
            if complete
                && t.bytes == 0
                && t.files == 0
                && t.unreadable == 0
                && id != CategoryId::Trash
            {
                return None;
            }
            Some(JunkItem {
                category: id,
                path: f.path.clone(),
                label: f.label.clone(),
                bytes: t.bytes,
                files: t.files,
                default_selected: info.default_selected && f.default_selected,
                note: f.note.clone(),
                modified_unix: f.modified,
                unreadable: t.unreadable,
            })
        })
        .collect();
    items.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
    let skipped = d
        .skipped
        .iter()
        .zip(skip_sizes)
        .map(|(s, t)| SkippedEntry {
            path: s.found.path.clone(),
            label: s.found.label.clone(),
            reason: s.reason.clone(),
            bytes: t.map(|t| t.bytes),
        })
        .collect();
    let cleanable = info.action == CategoryAction::MoveToTrash;
    CategoryReport {
        total_bytes: items.iter().map(|i| i.bytes).sum(),
        selected_bytes: if cleanable {
            items
                .iter()
                .filter(|i| i.default_selected)
                .map(|i| i.bytes)
                .sum()
        } else {
            0
        },
        unreadable: items.iter().map(|i| i.unreadable).sum(),
        root_unreadable: d.root_unreadable,
        complete,
        info,
        items,
        skipped,
    }
}

/// Scans the junk categories and measures every item in one parallel pass. Streams
/// [`ScanEvent`]s on the calling thread; returns every category in display order.
pub fn scan_junk(
    env: &Env,
    opts: &ScanOptions,
    on_event: &mut dyn FnMut(ScanEvent),
    cancel: &AtomicBool,
) -> JunkReport {
    let start = Instant::now();
    let running = env.running.snapshot();
    let ids = categories_in_order(opts);
    on_event(ScanEvent::Started {
        categories: ids.iter().map(|c| c.info()).collect(),
    });

    let discoveries: Vec<Discovery> = ids
        .iter()
        .map(|id| discover(env, *id, opts, &running))
        .collect();
    let mut roots = Vec::new();
    let mut slots = Vec::new();
    let mut pending = vec![0usize; ids.len()];
    for (ci, d) in discoveries.iter().enumerate() {
        for (i, f) in d.items.iter().enumerate() {
            roots.push(f.path.clone());
            slots.push(Slot::Item(ci, i));
            pending[ci] += 1;
        }
        for (i, s) in d.skipped.iter().enumerate() {
            if s.measure {
                roots.push(s.found.path.clone());
                slots.push(Slot::Skipped(ci, i));
                pending[ci] += 1;
            }
        }
    }
    let mut item_sizes: Vec<Vec<Option<SizeTotals>>> = discoveries
        .iter()
        .map(|d| vec![None; d.items.len()])
        .collect();
    let mut skip_sizes: Vec<Vec<Option<SizeTotals>>> = discoveries
        .iter()
        .map(|d| vec![None; d.skipped.len()])
        .collect();
    let mut reports: Vec<Option<CategoryReport>> = vec![None; ids.len()];

    for (ci, id) in ids.iter().enumerate() {
        if pending[ci] == 0 {
            let r = build_report(
                *id,
                &discoveries[ci],
                &item_sizes[ci],
                &skip_sizes[ci],
                true,
            );
            on_event(ScanEvent::Category { report: r.clone() });
            reports[ci] = Some(r);
        }
    }

    let outcome = walk::measure(&roots, env.threads, cancel, &mut |e| match e {
        WalkEvent::Tick { bytes, files } => on_event(ScanEvent::Progress { bytes, files }),
        WalkEvent::Done { index, totals } => {
            let ci = match slots[index] {
                Slot::Item(ci, i) => {
                    item_sizes[ci][i] = Some(totals);
                    ci
                }
                Slot::Skipped(ci, i) => {
                    skip_sizes[ci][i] = Some(totals);
                    ci
                }
            };
            pending[ci] -= 1;
            if pending[ci] == 0 && !cancel.load(Ordering::Relaxed) {
                let r = build_report(
                    ids[ci],
                    &discoveries[ci],
                    &item_sizes[ci],
                    &skip_sizes[ci],
                    true,
                );
                on_event(ScanEvent::Category { report: r.clone() });
                reports[ci] = Some(r);
            }
        }
    });

    // Categories cut short by a cancel keep whatever was measured.
    let categories: Vec<CategoryReport> = reports
        .into_iter()
        .enumerate()
        .map(|(ci, r)| {
            r.unwrap_or_else(|| {
                build_report(
                    ids[ci],
                    &discoveries[ci],
                    &item_sizes[ci],
                    &skip_sizes[ci],
                    false,
                )
            })
        })
        .collect();
    let cleanable = |c: &&CategoryReport| c.info.action == CategoryAction::MoveToTrash;
    JunkReport {
        total_bytes: categories
            .iter()
            .filter(cleanable)
            .map(|c| c.total_bytes)
            .sum(),
        selected_bytes: categories
            .iter()
            .filter(cleanable)
            .map(|c| c.selected_bytes)
            .sum(),
        categories,
        cancelled: outcome.cancelled,
        elapsed_ms: start.elapsed().as_millis() as u64,
        hardlink_cap_hit: outcome.hardlink_cap_hit,
    }
}

/// Moves the selected items to the Trash. Each item is checked against a fresh discovery of its
/// category (same `opts` as the scan): only paths the scan would offer right now are moved, so an
/// app that started since the scan, or a path that isn't a scan result, is skipped with the
/// reason. Nothing outside `selected` is touched.
pub fn clean(
    env: &Env,
    selected: &[JunkItem],
    opts: &ScanOptions,
    progress: &mut dyn FnMut(CleanProgress),
    cancel: &AtomicBool,
) -> CleanReport {
    let running = env.running.snapshot();
    let mut fresh: HashMap<CategoryId, Discovery> = HashMap::new();
    let mut results = Vec::with_capacity(selected.len());
    let mut cancelled = false;
    for (i, item) in selected.iter().enumerate() {
        progress(CleanProgress {
            done: i,
            total: selected.len(),
            current: Some(item.path.clone()),
        });
        let path = item.path.clone();
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            results.push(ItemResult::skipped(path, item.bytes, SkipReason::Cancelled));
            continue;
        }
        if item.category.info().action != CategoryAction::MoveToTrash {
            results.push(ItemResult::skipped(
                path,
                item.bytes,
                SkipReason::ReportOnly,
            ));
            continue;
        }
        let d = fresh
            .entry(item.category)
            .or_insert_with(|| discover(env, item.category, opts, &running));
        let result = if d.items.iter().any(|f| f.path == path) {
            trash::move_to_trash(env, &path, item.bytes, Zone::Home)
        } else if let Some(s) = d.skipped.iter().find(|s| s.found.path == path) {
            ItemResult::skipped(path, item.bytes, s.reason.clone())
        } else if trash::is_plain_absolute(&path) && fs::symlink_metadata(&path).is_err() {
            ItemResult::skipped(path, item.bytes, SkipReason::Missing)
        } else {
            ItemResult::skipped(path, item.bytes, SkipReason::NotACandidate)
        };
        results.push(result);
    }
    progress(CleanProgress {
        done: selected.len(),
        total: selected.len(),
        current: None,
    });
    let moved: Vec<&ItemResult> = results.iter().filter(|r| r.moved()).collect();
    CleanReport {
        moved_bytes: moved.iter().map(|r| r.bytes).sum(),
        moved_count: moved.len(),
        results,
        cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("17.0 (21A329)"), vec![17, 0]);
        assert_eq!(parse_version("iPhone15,2 17.4.1 (21E236)"), vec![17, 4, 1]);
        assert_eq!(parse_version("weird"), Vec::<u32>::new());
    }

    #[test]
    fn essential_list() {
        assert!(is_essential_cache("CloudKit"));
        assert!(is_essential_cache("com.apple.iCloudHelper"));
        assert!(is_essential_cache("com.apple.e5rt.e5bundlecache"));
        assert!(!is_essential_cache("com.apple.helpd"));
        assert!(
            is_apple_cache("GeoServices")
                && is_apple_cache("com.apple.Safari")
                && !is_apple_cache("com.google.Chrome")
        );
    }

    #[test]
    fn runtime_names() {
        assert_eq!(
            short_runtime("com.apple.CoreSimulator.SimRuntime.iOS-17-0"),
            "iOS 17.0"
        );
        assert_eq!(
            short_runtime("com.apple.CoreSimulator.SimRuntime.watchOS-10-2"),
            "watchOS 10.2"
        );
    }
}
