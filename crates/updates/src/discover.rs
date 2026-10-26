//! Finding installed apps and working out where each one gets its updates.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use plist::Value;

use crate::types::{InstalledApp, SelfUpdater, UpdateSource};

/// Where to look for apps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoverOptions {
    /// Folders to scan, plus one level of subfolders ("/Applications/Utilities").
    /// Default: `/Applications` and `~/Applications`.
    pub roots: Vec<PathBuf>,
    /// Home folder whose `Library/Preferences` (and app containers) may hold a Sparkle feed
    /// override. `None` skips reading preferences. Default: the user's home.
    pub home: Option<PathBuf>,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        let home = std::env::home_dir();
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(h) = &home {
            roots.push(h.join("Applications"));
        }
        DiscoverOptions { roots, home }
    }
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| !is_hidden(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

/// Lists the apps in the configured folders, sorted by name. Reads only `Info.plist`, a few
/// marker files inside each bundle, `app-update.yml`, and (for Sparkle apps) the app's
/// preferences file. Takes a few milliseconds per app.
pub fn discover_apps(opts: &DiscoverOptions) -> Vec<InstalledApp> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    let mut add = |path: PathBuf, apps: &mut Vec<InstalledApp>| {
        let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key)
            && let Some(app) = read_app(&path, opts.home.as_deref())
        {
            apps.push(app);
        }
    };
    for root in &opts.roots {
        for entry in sorted_children(root) {
            if !entry.is_dir() {
                continue;
            }
            if is_app_bundle(&entry) {
                add(entry, &mut apps);
            } else {
                // One level of subfolders: "Utilities", "Adobe Photoshop 2026", "Setapp".
                for inner in sorted_children(&entry) {
                    if is_app_bundle(&inner) && inner.is_dir() {
                        add(inner, &mut apps);
                    }
                }
            }
        }
    }
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    apps
}

fn plist_string(dict: &plist::Dictionary, key: &str) -> Option<String> {
    let s = match dict.get(key)? {
        Value::String(s) => s.trim().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        _ => return None,
    };
    if s.is_empty() { None } else { Some(s) }
}

fn read_dict(path: &Path) -> Option<plist::Dictionary> {
    Value::from_file(path).ok()?.into_dictionary()
}

/// A Sparkle feed address from the app's preferences. Sparkle reads `SUFeedURL` from user
/// defaults before `Info.plist`. Read straight from the plist file rather than through
/// CFPreferences, which would keep every app's preferences cached in Kipimo's memory.
fn feed_from_defaults(home: &Path, bundle_id: &str) -> Option<String> {
    let file = format!("{bundle_id}.plist");
    let candidates = [
        home.join("Library/Preferences").join(&file),
        home.join("Library/Containers")
            .join(bundle_id)
            .join("Data/Library/Preferences")
            .join(&file),
    ];
    candidates
        .iter()
        .filter_map(|p| read_dict(p))
        .find_map(|d| plist_string(&d, "SUFeedURL"))
        .filter(|u| crate::url::http_scheme(u).is_some())
}

/// Reads one app bundle. Returns `None` for things that aren't readable app bundles.
pub fn read_app(path: &Path, home: Option<&Path>) -> Option<InstalledApp> {
    if !path.is_dir() {
        return None;
    }
    let name = path.file_stem()?.to_string_lossy().into_owned();

    // iPhone/iPad apps on Apple silicon: "X.app/Wrapper/X.app" with "WrappedBundle" → it.
    let wrapped = path.join("WrappedBundle");
    if wrapped.exists() || path.join("Wrapper").is_dir() {
        let inner = if wrapped.exists() {
            wrapped
        } else {
            sorted_children(&path.join("Wrapper"))
                .into_iter()
                .find(|p| is_app_bundle(p))?
        };
        let info = read_dict(&inner.join("Info.plist")).unwrap_or_default();
        let from_store =
            path.join("Wrapper/iTunesMetadata.plist").exists() || inner.join("SC_Info").is_dir();
        return Some(InstalledApp {
            name,
            path: path.to_path_buf(),
            bundle_id: plist_string(&info, "CFBundleIdentifier"),
            version: plist_string(&info, "CFBundleShortVersionString"),
            build: plist_string(&info, "CFBundleVersion"),
            minimum_system_version: plist_string(&info, "LSMinimumSystemVersion"),
            source: if from_store {
                UpdateSource::AppStore { ios_app: true }
            } else {
                UpdateSource::Unknown { updater: None }
            },
        });
    }

    let contents = path.join("Contents");
    let info = read_dict(&contents.join("Info.plist")).unwrap_or_default();
    let bundle_id = plist_string(&info, "CFBundleIdentifier");
    let source = detect_source(path, &contents, &info, bundle_id.as_deref(), home);
    Some(InstalledApp {
        name,
        path: path.to_path_buf(),
        bundle_id,
        version: plist_string(&info, "CFBundleShortVersionString"),
        build: plist_string(&info, "CFBundleVersion"),
        minimum_system_version: plist_string(&info, "LSMinimumSystemVersion"),
        source,
    })
}

fn detect_source(
    path: &Path,
    contents: &Path,
    info: &plist::Dictionary,
    bundle_id: Option<&str>,
    home: Option<&Path>,
) -> UpdateSource {
    if contents.join("_MASReceipt/receipt").is_file() {
        return UpdateSource::AppStore { ios_app: false };
    }
    let has_sparkle = contents.join("Frameworks/Sparkle.framework").exists();
    let plist_feed =
        plist_string(info, "SUFeedURL").filter(|u| crate::url::http_scheme(u).is_some());
    if has_sparkle || plist_feed.is_some() {
        let defaults_feed = match (home, bundle_id) {
            (Some(h), Some(id)) => feed_from_defaults(h, id),
            _ => None,
        };
        if let Some(feed_url) = defaults_feed {
            return UpdateSource::Sparkle {
                feed_url,
                from_defaults: true,
            };
        }
        if let Some(feed_url) = plist_feed {
            return UpdateSource::Sparkle {
                feed_url,
                from_defaults: false,
            };
        }
    }
    let app_update = contents.join("Resources/app-update.yml");
    if let Ok(src) = fs::read_to_string(&app_update) {
        return match crate::electron::parse_app_update(&src) {
            Ok(feed) => UpdateSource::ElectronUpdater { feed },
            Err(provider) => UpdateSource::Unknown {
                updater: Some(SelfUpdater::ElectronUpdater { provider }),
            },
        };
    }
    if let Some(source) = vscode_source(contents) {
        return source;
    }
    let updater = if has_sparkle {
        Some(SelfUpdater::Sparkle)
    } else if contents.join("Frameworks/Squirrel.framework").exists() {
        Some(SelfUpdater::Squirrel)
    } else if info.contains_key("KSProductID") || info.contains_key("KSUpdateURL") {
        Some(SelfUpdater::GoogleKeystone)
    } else if contents.join("Resources/updater.ini").exists()
        || contents.join("MacOS/updater.app").exists()
    {
        Some(SelfUpdater::Mozilla)
    } else if bundle_id.is_some_and(|id| id.starts_with("com.microsoft.")) {
        Some(SelfUpdater::MicrosoftAutoUpdate)
    } else if fs::canonicalize(path).is_ok_and(|p| p.starts_with("/System")) {
        // Safari and friends live in the system volume (or a cryptex) and update with macOS.
        Some(SelfUpdater::MacOS)
    } else {
        None
    };
    UpdateSource::Unknown { updater }
}

/// Hosts that speak VS Code's update API (`/api/update/<platform>/<quality>/<commit>`).
/// Forks with other servers use other request shapes, so they're not guessed at.
const VSCODE_UPDATE_HOSTS: &[&str] = &["update.code.visualstudio.com"];

/// Visual Studio Code (and Insiders) read their update server from `product.json`.
fn vscode_source(contents: &Path) -> Option<UpdateSource> {
    let path = contents.join("Resources/app/product.json");
    let bytes = fs::read(&path).ok()?;
    let product: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let update_url = product
        .get("updateUrl")?
        .as_str()?
        .trim_end_matches('/')
        .to_string();
    let quality = product.get("quality")?.as_str()?.trim().to_string();
    let host = crate::url::host(&update_url)?;
    if quality.is_empty() || !VSCODE_UPDATE_HOSTS.contains(&host.as_str()) {
        return None;
    }
    let platform = if product
        .get("darwinUniversalAssetId")
        .is_some_and(|v| !v.is_null())
    {
        "darwin-universal"
    } else if crate::system::is_arm64() {
        "darwin-arm64"
    } else {
        "darwin"
    };
    Some(UpdateSource::VsCode {
        update_url,
        quality,
        platform: platform.into(),
    })
}
