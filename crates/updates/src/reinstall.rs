//! Where to get a good copy of an app, for Startup items' Reinstall: its App Store page, the
//! download its update feed points at, or its developer's site. Offline; the caller supplies
//! the update check's answer for the app, if it has one.

use std::ffi::c_void;
use std::path::Path;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSNumber, NSString};
use serde::{Deserialize, Serialize};

use crate::types::{AppUpdate, ElectronFeed, InstalledApp, Status, UpdateSource};

/// How to reinstall an app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// The App Store. `url` is the app's page (`macappstore://`), `None` when it isn't known.
    AppStore { url: Option<String> },
    /// The developer's own download, from the app's update feed. `zip` is true when the link
    /// is a zip archive of the app (what Download and Replace can use).
    Download {
        url: String,
        version: Option<String>,
        /// Where the link came from: "Sparkle", "electron-updater", "VS Code updates".
        source: String,
        zip: bool,
    },
    /// The developer's website.
    Website { url: String },
    /// Nothing known: show the app in Finder and say to download a fresh copy.
    Finder,
}

impl Route {
    /// The zip archive Download and Replace can use.
    pub fn zip_url(&self) -> Option<&str> {
        match self {
            Route::Download { url, zip: true, .. } => Some(url),
            _ => None,
        }
    }
}

/// The App Store page of an item id.
pub fn store_url(id: u64) -> String {
    format!("macappstore://apps.apple.com/app/id{id}?mt=12")
}

/// Works out the route for `app` from the update check's answer for it (`update`) and its App
/// Store item id (see [`store_id`]).
pub fn route(app: &InstalledApp, update: Option<&AppUpdate>, store_id: Option<u64>) -> Route {
    let latest = update
        .filter(|u| matches!(u.status, Status::UpToDate | Status::UpdateAvailable))
        .and_then(|u| u.latest.as_ref());
    if let UpdateSource::AppStore { .. } = app.source {
        let looked_up = latest
            .and_then(|l| l.download_url.clone())
            .filter(|u| u.starts_with("macappstore:"));
        return Route::AppStore {
            url: store_id.map(store_url).or(looked_up),
        };
    }
    if let Some(l) = latest {
        let link = l
            .download_url
            .clone()
            .or_else(|| l.release_notes_url.clone());
        if let Some(url) = link.filter(|u| crate::url::http_scheme(u).is_some()) {
            return Route::Download {
                zip: l.download_url.as_deref() == Some(url.as_str()) && is_zip(&url),
                url,
                version: l.display_version().map(str::to_string),
                source: app.source.label().to_string(),
            };
        }
    }
    // No download this Mac can use (the check failed, or the newest release needs a newer
    // macOS): a page about the release is still the developer's site.
    let notes = update.and_then(|u| {
        u.latest
            .as_ref()
            .or(u.newer_requires_macos.as_ref())
            .and_then(|l| l.release_notes_url.clone())
    });
    match notes
        .filter(|u| crate::url::http_scheme(u).is_some())
        .or_else(|| developer_site(app))
    {
        Some(url) => Route::Website { url },
        None => Route::Finder,
    }
}

/// The link's path ends in `.zip`.
pub fn is_zip(link: &str) -> bool {
    let path = link.split(['?', '#']).next().unwrap_or(link);
    path.to_ascii_lowercase().ends_with(".zip")
}

/// The developer's site from what the app ships: its GitHub repository (electron-updater or
/// a Sparkle feed on GitHub), Visual Studio Code's `downloadUrl`, or the home page of the
/// server its Sparkle or generic feed lives on (not when that is a storage service).
pub fn developer_site(app: &InstalledApp) -> Option<String> {
    let github = |host: Option<&str>, owner: &str, repo: &str| {
        format!(
            "https://{}/{owner}/{repo}/releases/latest",
            host.unwrap_or("github.com")
        )
    };
    let feed = match &app.source {
        UpdateSource::ElectronUpdater {
            feed: ElectronFeed::GitHub {
                host, owner, repo, ..
            },
        } => return Some(github(host.as_deref(), owner, repo)),
        UpdateSource::ElectronUpdater {
            feed: ElectronFeed::Generic { url, .. },
        } => url.clone(),
        UpdateSource::Sparkle { feed_url, .. } => feed_url.clone(),
        UpdateSource::VsCode { .. } => {
            let product = app.path.join("Contents/Resources/app/product.json");
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(product).ok()?).ok()?;
            return json
                .get("downloadUrl")?
                .as_str()
                .filter(|u| crate::url::http_scheme(u) == Some("https"))
                .map(str::to_string);
        }
        UpdateSource::AppStore { .. } | UpdateSource::Unknown { .. } => return None,
    };
    let scheme = crate::url::http_scheme(&feed)?;
    let host = crate::url::host(&feed)?;
    let path: Vec<&str> = feed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty())
        .collect();
    if host == "raw.githubusercontent.com" || host == "github.com" {
        return match path.as_slice() {
            [owner, repo, ..] => Some(github(None, owner, repo)),
            _ => None,
        };
    }
    const STORAGE: &[&str] = &[
        "amazonaws.com",
        "cloudfront.net",
        "googleapis.com",
        "core.windows.net",
        "azureedge.net",
        "digitaloceanspaces.com",
        "backblazeb2.com",
        "r2.dev",
        "r2.cloudflarestorage.com",
        "githubusercontent.com",
    ];
    if STORAGE
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
    {
        return None;
    }
    Some(format!(
        "{scheme}://{}/",
        crate::url::authority(&feed).unwrap_or(host)
    ))
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn MDItemCreate(allocator: *const c_void, path: *const c_void) -> *const c_void;
    fn MDItemCopyAttribute(item: *const c_void, name: *const c_void) -> *const c_void;
}

/// The App Store item id of an installed App Store app: `itemId` in an iPhone or iPad app's
/// `iTunesMetadata.plist`, else Spotlight's `kMDItemAppStoreAdamID`, which macOS records
/// for every app the App Store installs. Offline.
pub fn store_id(app: &Path) -> Option<u64> {
    if let Ok(v) = plist::Value::from_file(app.join("Wrapper/iTunesMetadata.plist"))
        && let Some(id) = v
            .as_dictionary()
            .and_then(|d| d.get("itemId"))
            .and_then(|v| v.as_unsigned_integer())
            .filter(|id| *id > 0)
    {
        return Some(id);
    }
    autoreleasepool(|_| {
        let path = NSString::from_str(app.to_str()?);
        let key = NSString::from_str("kMDItemAppStoreAdamID");
        // SAFETY: NSString is toll-free bridged with CFString; the item is released below.
        let item = unsafe { MDItemCreate(std::ptr::null(), Retained::as_ptr(&path).cast()) };
        if item.is_null() {
            return None;
        }
        // SAFETY: a valid MDItem and attribute name; the value is +1 (or null).
        let value = unsafe { MDItemCopyAttribute(item, Retained::as_ptr(&key).cast()) };
        unsafe { CFRelease(item) };
        if value.is_null() {
            return None;
        }
        // SAFETY: CF objects are Objective-C objects; we take over the +1.
        let obj: Retained<AnyObject> = unsafe { Retained::from_raw(value as *mut AnyObject) }?;
        let n = obj.downcast_ref::<NSNumber>()?.longLongValue();
        u64::try_from(n).ok().filter(|n| *n > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Release;
    use std::path::PathBuf;

    fn app(source: UpdateSource) -> InstalledApp {
        InstalledApp {
            name: "Thing".into(),
            path: PathBuf::from("/nonexistent/Thing.app"),
            bundle_id: Some("com.example.thing".into()),
            version: Some("1.0".into()),
            build: Some("100".into()),
            minimum_system_version: None,
            source,
        }
    }

    fn update(app: &InstalledApp, status: Status, release: Option<Release>) -> AppUpdate {
        AppUpdate {
            app: app.clone(),
            status,
            latest: release,
            newer_requires_macos: None,
            critical: false,
            hint: None,
            checked_at: 1,
            from_cache: true,
        }
    }

    fn sparkle(feed: &str) -> UpdateSource {
        UpdateSource::Sparkle {
            feed_url: feed.into(),
            from_defaults: false,
        }
    }

    #[test]
    fn app_store_apps_go_to_their_page() {
        let a = app(UpdateSource::AppStore { ios_app: false });
        assert_eq!(
            route(&a, None, Some(497799835)),
            Route::AppStore {
                url: Some("macappstore://apps.apple.com/app/id497799835?mt=12".into())
            }
        );
        let looked_up = Release {
            download_url: Some("macappstore://apps.apple.com/app/id1?mt=12".into()),
            ..Release::default()
        };
        let u = update(&a, Status::UpToDate, Some(looked_up));
        assert_eq!(
            route(&a, Some(&u), None),
            Route::AppStore {
                url: Some("macappstore://apps.apple.com/app/id1?mt=12".into())
            }
        );
        assert_eq!(route(&a, None, None), Route::AppStore { url: None });
    }

    #[test]
    fn feed_downloads_and_zips() {
        let a = app(sparkle("https://www.example.com/appcast.xml"));
        let rel = |dl: &str| Release {
            version: Some("2.0".into()),
            download_url: Some(dl.into()),
            release_notes_url: Some("https://www.example.com/notes".into()),
            ..Release::default()
        };
        let u = update(
            &a,
            Status::UpdateAvailable,
            Some(rel("https://cdn.example.com/Thing-2.0.zip?x=1")),
        );
        let r = route(&a, Some(&u), None);
        assert_eq!(
            r.zip_url(),
            Some("https://cdn.example.com/Thing-2.0.zip?x=1")
        );
        assert_eq!(
            r,
            Route::Download {
                url: "https://cdn.example.com/Thing-2.0.zip?x=1".into(),
                version: Some("2.0".into()),
                source: "Sparkle".into(),
                zip: true
            }
        );
        let u = update(
            &a,
            Status::UpToDate,
            Some(rel("https://cdn.example.com/Thing-2.0.dmg")),
        );
        let r = route(&a, Some(&u), None);
        assert_eq!(r.zip_url(), None);
        assert!(matches!(r, Route::Download { zip: false, .. }));
    }

    #[test]
    fn no_usable_download_falls_back_to_the_site() {
        let a = app(sparkle("https://www.example.com/updates/appcast.xml"));
        // The newest release needs a newer macOS: its notes page.
        let mut u = update(
            &a,
            Status::RequiresNewerMacos {
                minimum: "99.0".into(),
            },
            None,
        );
        u.newer_requires_macos = Some(Release {
            release_notes_url: Some("https://www.example.com/notes/3".into()),
            download_url: Some("https://www.example.com/Thing-3.zip".into()),
            ..Release::default()
        });
        assert_eq!(
            route(&a, Some(&u), None),
            Route::Website {
                url: "https://www.example.com/notes/3".into()
            }
        );
        // The check failed: the feed's server.
        let u = update(&a, Status::CheckFailed { reason: "x".into() }, None);
        assert_eq!(
            route(&a, Some(&u), None),
            Route::Website {
                url: "https://www.example.com/".into()
            }
        );
        // Nothing at all.
        let a = app(UpdateSource::Unknown { updater: None });
        assert_eq!(route(&a, None, None), Route::Finder);
    }

    #[test]
    fn developer_sites() {
        let gh = app(UpdateSource::ElectronUpdater {
            feed: ElectronFeed::GitHub {
                host: None,
                owner: "acme".into(),
                repo: "thing".into(),
                channel: "latest".into(),
            },
        });
        assert_eq!(
            developer_site(&gh).as_deref(),
            Some("https://github.com/acme/thing/releases/latest")
        );
        assert_eq!(
            developer_site(&app(sparkle(
                "https://raw.githubusercontent.com/acme/thing/main/appcast.xml"
            )))
            .as_deref(),
            Some("https://github.com/acme/thing/releases/latest")
        );
        assert_eq!(
            developer_site(&app(sparkle("https://bucket.s3.amazonaws.com/appcast.xml"))),
            None
        );
        assert_eq!(
            developer_site(&app(sparkle("http://example.org:8080/a/appcast.xml"))).as_deref(),
            Some("http://example.org:8080/")
        );
        let generic = app(UpdateSource::ElectronUpdater {
            feed: ElectronFeed::Generic {
                url: "https://downloads.example.net/chat".into(),
                channel: "latest".into(),
            },
        });
        assert_eq!(
            developer_site(&generic).as_deref(),
            Some("https://downloads.example.net/")
        );
        assert_eq!(
            developer_site(&app(UpdateSource::Unknown { updater: None })),
            None
        );
    }

    #[test]
    fn zip_links() {
        assert!(is_zip("https://x/Thing.zip"));
        assert!(is_zip("https://x/Thing.ZIP#frag"));
        assert!(is_zip("https://x/Thing.zip?sig=1"));
        assert!(!is_zip("https://x/Thing.dmg"));
        assert!(!is_zip("https://x/zip?file=Thing.dmg"));
    }

    #[test]
    fn store_id_of_a_folder_without_one() {
        let dir = std::env::temp_dir().join(format!("kipimo-store-id-{}", std::process::id()));
        let app = dir.join("Plain.app");
        std::fs::create_dir_all(app.join("Wrapper")).unwrap();
        assert_eq!(store_id(&app), None);
        let mut d = plist::Dictionary::new();
        d.insert("itemId".into(), plist::Value::Integer(1_234_567.into()));
        plist::Value::Dictionary(d)
            .to_file_xml(app.join("Wrapper/iTunesMetadata.plist"))
            .unwrap();
        assert_eq!(store_id(&app), Some(1_234_567));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
