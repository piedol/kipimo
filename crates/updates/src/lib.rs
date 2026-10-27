//! Tool F: app updates. Finds newer versions from each app's update feed and the App Store.
//!
//! The update check only reports: it never downloads or installs anything. Every network call
//! is a read-only GET of a public feed. The one exception is [`download`], which Startup
//! items' Download and Replace calls when the user asks it to fetch an app's own download.
//!
//! # Public API
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use kipimo_updates::{CheckOptions, DiscoverOptions, default_cache_path, scan};
//!
//! let cancel = AtomicBool::new(false);
//! let opts = CheckOptions { cache_path: default_cache_path(), ..Default::default() };
//! let report = scan(&DiscoverOptions::default(), &opts, |p| {
//!     println!("{}/{} {} {:?}", p.done, p.total, p.update.app.name, p.update.status);
//! }, &cancel);
//! println!("{} updates", report.counts().update_available);
//! ```
//!
//! - [`discover_apps`]`(&DiscoverOptions) -> Vec<InstalledApp>`: lists apps in
//!   `/Applications` and `~/Applications` (plus one level of subfolders) with name, bundle
//!   ID, version, build, `LSMinimumSystemVersion` and the detected [`UpdateSource`]:
//!   App Store (receipt in `Contents/_MASReceipt`), Sparkle (feed from the app's preferences or
//!   `SUFeedURL` in `Info.plist`), electron-updater (`Resources/app-update.yml`, GitHub or
//!   generic provider), Visual Studio Code's update service (`Resources/app/product.json`),
//!   or `Unknown` with the app's own updater named when recognised
//!   ([`SelfUpdater`]: Sparkle without a readable feed, Squirrel, Google Keystone, Mozilla,
//!   Microsoft AutoUpdate, macOS). Fast and offline; call it on a background thread anyway.
//! - [`check_updates`]`(&[InstalledApp], &CheckOptions, progress, &AtomicBool) -> UpdateReport`:
//!   reads the feeds (blocking; call it on a background thread). `progress` gets a
//!   [`Progress`] for every app as its result is known, on the calling thread. Set the flag to
//!   cancel (honoured within ~100 ms; the report then has `cancelled: true`).
//! - [`scan`]: `discover_apps` followed by `check_updates`.
//! - [`default_cache_path`]: `~/Library/Caches/top.piedol.kipimo/updates.json`. With a
//!   `cache_path`, each feed is read at most once per `max_cache_age` (24 h) unless `force`.
//!   `offline` answers from the cache alone, whatever its age, and never goes online.
//!
//! # Reinstalling an app (Startup items › Reinstall…)
//!
//! - [`reinstall_route`]`(&InstalledApp, Option<&AppUpdate>, store_id) -> `[`Route`]: the
//!   App Store page ([`store_id`] reads the item id from Spotlight's
//!   `kMDItemAppStoreAdamID`, offline), the download the app's feed points at (with `zip`
//!   set for a `.zip`), the developer's site ([`developer_site`]: a GitHub repository, VS
//!   Code's `downloadUrl`, the feed's server), or nothing ([`Route::Finder`]).
//! - [`download`]`(url, dest, &DownloadOptions, &cancel, progress)`: streams one file to
//!   disk (redirects followed, system proxy per hop, 4 GB cap, 30 s stall timeout).
//!
//! Each [`AppUpdate`] has the installed app (version, source), a [`Status`] (`UpToDate`,
//! `UpdateAvailable`, `RequiresNewerMacos { minimum }`, `UnknownSource`,
//! `CheckFailed { reason }`), the `latest` [`Release`] (display version, build, minimum
//! macOS, release-notes URL, download or `macappstore://` link, date), `critical`, and an
//! [`UpdaterHint`] (text, plus a URL to open such as the App Store's Updates page) where the
//! app's own updater is the way to install. [`UpdateReport::counts`] gives the badge numbers;
//! [`UpdateReport::updates`] the apps to list.
//!
//! Failures: network trouble (timeouts, HTTP 5xx/429) isn't cached and is retried on the
//! next run; definite answers (HTTP 404, unreadable feed, not in the App Store) are cached
//! like successes, so a broken feed is asked at most once a day.
//!
//! # Behaviour
//!
//! - **App Store:** Apple's iTunes lookup API, batched by bundle ID (40 per request), in the
//!   Mac's region (`NSLocale.currentLocale.regionCode`).
//! - **Sparkle:** RSS appcast; items for other OSes, other channels (betas, unless listed in
//!   `sparkle_channels`) and outside their min/max macOS are skipped; the highest
//!   `sparkle:version` wins, compared with Sparkle's own rules against `CFBundleVersion`.
//!   Critical updates, release-notes links, informational updates and Sparkle 1 feeds
//!   (version only on the enclosure, or in the file name) are handled.
//! - **electron-updater:** `<channel>-mac.yml` from the generic URL or the latest GitHub
//!   release; `minimumSystemVersion` (a Darwin version) is respected. Private repositories
//!   (redirect to GitHub sign-in) are reported as such.
//! - **VS Code:** `<updateUrl>/api/update/<platform>/<quality>/latest` (the installed commit
//!   isn't sent).
//! - **Versions:** [`version::compare_sparkle`] (Sparkle's rules) for Sparkle builds;
//!   [`version::compare_lenient`] for display versions elsewhere ("1.2" = "1.2.0",
//!   "1.0-beta" < "1.0").
//! - A newer release that needs a newer macOS is reported as `RequiresNewerMacos`, or as
//!   `newer_requires_macos` next to an update this Mac can install.
//! - **Network:** ureq + rustls, 20 s timeout, 8 MiB cap, 4 workers, at least 1 s between
//!   requests to one host, macOS system proxy (HTTP/HTTPS/SOCKS; PAC isn't evaluated),
//!   generic `User-Agent: Kipimo/<version>`, no cookies or system profile.

use std::path::PathBuf;

mod appcast;
mod appstore;
mod cache;
mod check;
mod discover;
mod download;
mod electron;
mod finding;
mod net;
mod proxy;
mod reinstall;
mod system;
mod types;
mod url;
pub mod version;
mod vscode;
mod yaml;

use std::sync::atomic::AtomicBool;

pub use check::{CheckOptions, check_updates, default_cache_path};
pub use discover::{DiscoverOptions, discover_apps, read_app};
pub use download::{DownloadOptions, download};
pub use reinstall::{Route, developer_site, is_zip, route as reinstall_route, store_id, store_url};
pub use system::{darwin_version, macos_version, region};
pub use types::{
    AppUpdate, ElectronFeed, InstalledApp, Progress, Release, SelfUpdater, Status, StatusCounts,
    UpdateReport, UpdateSource, UpdaterHint,
};

/// Discovers apps, then checks them. See [`discover_apps`] and [`check_updates`].
pub fn scan(
    discover: &DiscoverOptions,
    check: &CheckOptions,
    progress: impl FnMut(Progress<'_>),
    cancel: &AtomicBool,
) -> UpdateReport {
    let apps = discover_apps(discover);
    check_updates(&apps, check, progress, cancel)
}

/// Development builds only: `KIPIMO_FAKE_HOME` / `KIPIMO_FAKE_ROOT` stand in for the home
/// folder and `/`, so the app can be exercised on a synthetic Mac (a throwaway disk image).
/// Release builds ignore them.
pub fn fake_home() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_HOME")
}

/// See [`fake_home`].
pub fn fake_root() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_ROOT")
}

fn dev_var(name: &str) -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    }
}
