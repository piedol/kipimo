//! Public result types. Everything here is plain data (`Serialize` + `Deserialize`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// An app bundle found in `/Applications` or `~/Applications`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledApp {
    /// Finder name without ".app", e.g. "Visual Studio Code".
    pub name: String,
    pub path: PathBuf,
    /// `CFBundleIdentifier`.
    pub bundle_id: Option<String>,
    /// `CFBundleShortVersionString`, the version people see ("3.7.6").
    pub version: Option<String>,
    /// `CFBundleVersion`, the build ("4120"). Sparkle compares this one.
    pub build: Option<String>,
    /// `LSMinimumSystemVersion` of the installed copy.
    pub minimum_system_version: Option<String>,
    pub source: UpdateSource,
}

impl InstalledApp {
    /// The version to show: the short version, else the build.
    pub fn display_version(&self) -> Option<&str> {
        self.version.as_deref().or(self.build.as_deref())
    }
}

/// Where Kipimo looks for a newer version of an app.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateSource {
    /// Installed from the Mac App Store (has `Contents/_MASReceipt`). `ios_app` is an iPhone or
    /// iPad app running on Apple silicon.
    AppStore { ios_app: bool },
    /// A Sparkle appcast. `from_defaults` is true when the feed address came from the app's
    /// preferences (which Sparkle prefers over `Info.plist`, e.g. a beta feed the user chose).
    Sparkle {
        feed_url: String,
        from_defaults: bool,
    },
    /// electron-updater (`Contents/Resources/app-update.yml`).
    ElectronUpdater { feed: ElectronFeed },
    /// Visual Studio Code's update service (`updateUrl` and `quality` in
    /// `Contents/Resources/app/product.json`). `platform` is "darwin-arm64", "darwin" or
    /// "darwin-universal".
    #[serde(rename = "vscode")]
    VsCode {
        update_url: String,
        quality: String,
        platform: String,
    },
    /// No update source Kipimo can check. `updater` names the app's own updater when one was
    /// recognised, so the UI can say where to look.
    Unknown { updater: Option<SelfUpdater> },
}

impl UpdateSource {
    /// Short label for the UI: "App Store", "Sparkle", "electron-updater", "VS Code updates"
    /// or "None".
    pub fn label(&self) -> &'static str {
        match self {
            UpdateSource::AppStore { .. } => "App Store",
            UpdateSource::Sparkle { .. } => "Sparkle",
            UpdateSource::ElectronUpdater { .. } => "electron-updater",
            UpdateSource::VsCode { .. } => "VS Code updates",
            UpdateSource::Unknown { .. } => "None",
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, UpdateSource::Unknown { .. })
    }
}

/// An electron-updater feed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ElectronFeed {
    /// Releases on GitHub (or GitHub Enterprise when `host` is set).
    #[serde(rename = "github")]
    GitHub {
        host: Option<String>,
        owner: String,
        repo: String,
        channel: String,
    },
    /// A plain web folder holding `<channel>-mac.yml`.
    Generic { url: String, channel: String },
}

/// An app's own updater that Kipimo recognises but can't query.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "updater", rename_all = "snake_case")]
pub enum SelfUpdater {
    /// Sparkle, with the feed address set inside the app's code.
    Sparkle,
    /// electron-updater with a provider Kipimo doesn't read (s3, private GitHub repo, ...).
    ElectronUpdater { provider: String },
    /// Squirrel.Mac (Electron's built-in updater: Slack, Discord, VS Code, ...).
    Squirrel,
    /// Google Software Update (Keystone): Chrome and other Google apps.
    GoogleKeystone,
    /// Mozilla's updater: Firefox, Thunderbird.
    Mozilla,
    /// Microsoft AutoUpdate (Office installed outside the App Store).
    MicrosoftAutoUpdate,
    /// An Apple app that macOS Software Update keeps current (Safari).
    #[serde(rename = "macos")]
    MacOS,
}

/// One release from a feed or the App Store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Display version ("3.7.6").
    pub version: Option<String>,
    /// Build version, when the feed has one (Sparkle's `sparkle:version`).
    pub build: Option<String>,
    /// Minimum macOS version ("13.0") this release needs, if the feed says.
    pub minimum_system_version: Option<String>,
    /// Release notes page.
    pub release_notes_url: Option<String>,
    /// Direct download (Sparkle enclosure, electron file) or the App Store page
    /// (`macappstore://` link). Kipimo never downloads it; the UI may open it.
    pub download_url: Option<String>,
    /// Publication date exactly as the feed gives it (RFC 822 for Sparkle, ISO 8601 otherwise).
    pub published: Option<String>,
}

impl Release {
    /// The version to show: the display version, else the build.
    pub fn display_version(&self) -> Option<&str> {
        self.version.as_deref().or(self.build.as_deref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    UpToDate,
    UpdateAvailable,
    /// A newer version exists but needs a newer macOS (`minimum`, e.g. "27.0").
    RequiresNewerMacos {
        minimum: String,
    },
    /// No update source Kipimo can check (see [`UpdateSource::Unknown`]).
    UnknownSource,
    /// The check didn't work; `reason` is a short sentence for the UI.
    CheckFailed {
        reason: String,
    },
}

/// How to update through the app's own mechanism.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdaterHint {
    /// "Open iTerm and choose Check for Updates…"
    pub text: String,
    /// A URL that opens the right place (the App Store's Updates page). When `None`, opening
    /// the app itself (`InstalledApp::path`) is the right action.
    pub open_url: Option<String>,
}

/// The check result for one app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUpdate {
    pub app: InstalledApp,
    pub status: Status,
    /// For `UpdateAvailable` / `UpToDate`: the newest release this Mac can run.
    /// For `RequiresNewerMacos`: the newer release that needs a newer macOS.
    pub latest: Option<Release>,
    /// With `UpdateAvailable`: an even newer release exists but needs a newer macOS.
    pub newer_requires_macos: Option<Release>,
    /// The feed marks the update as critical (Sparkle `sparkle:criticalUpdate`).
    pub critical: bool,
    pub hint: Option<UpdaterHint>,
    /// When the feed was read (Unix seconds); for cached results, the original check time.
    /// 0 when no check ran (unknown source).
    pub checked_at: u64,
    pub from_cache: bool,
}

impl AppUpdate {
    pub fn latest_version(&self) -> Option<&str> {
        self.latest.as_ref().and_then(Release::display_version)
    }
}

/// Counts by status, for badges and the weekly summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub up_to_date: usize,
    pub update_available: usize,
    pub requires_newer_macos: usize,
    pub unknown_source: usize,
    pub check_failed: usize,
}

/// Everything one check run produced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReport {
    /// Sorted: updates first, then by name.
    pub apps: Vec<AppUpdate>,
    /// Start of the run (Unix seconds).
    pub started_at: u64,
    pub duration_ms: u64,
    /// True when the run was cancelled; `apps` then holds only what finished.
    pub cancelled: bool,
    /// The App Store region used, lower case ("gb").
    pub store_country: String,
    /// This Mac's macOS version ("26.6.2").
    pub macos_version: String,
    /// Number of network requests made (0 when everything came from the cache).
    pub requests: usize,
}

impl UpdateReport {
    pub fn counts(&self) -> StatusCounts {
        let mut c = StatusCounts::default();
        for a in &self.apps {
            match a.status {
                Status::UpToDate => c.up_to_date += 1,
                Status::UpdateAvailable => c.update_available += 1,
                Status::RequiresNewerMacos { .. } => c.requires_newer_macos += 1,
                Status::UnknownSource => c.unknown_source += 1,
                Status::CheckFailed { .. } => c.check_failed += 1,
            }
        }
        c
    }

    /// The apps with an update this Mac can install.
    pub fn updates(&self) -> impl Iterator<Item = &AppUpdate> {
        self.apps
            .iter()
            .filter(|a| a.status == Status::UpdateAvailable)
    }
}

/// Streamed to the progress callback after each app's result is known.
#[derive(Clone, Copy, Debug)]
pub struct Progress<'a> {
    /// Apps with a result so far, including this one.
    pub done: usize,
    pub total: usize,
    pub update: &'a AppUpdate,
}
