//! Public result types. Everything is plain data (serde-serializable) so the UI can show it,
//! log it, or keep it between a scan and an action.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What kind of startup item this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKind {
    /// A launchd agent plist (`~/Library/LaunchAgents` or `/Library/LaunchAgents`).
    LaunchAgent,
    /// A launchd daemon plist (`/Library/LaunchDaemons`), runs as root.
    LaunchDaemon,
    /// An app or helper that opens at login ("Open at Login" in System Settings).
    LoginItem,
    /// A background agent or daemon an app registered from inside its own bundle
    /// (`SMAppService`), shown under "Allow in the Background".
    BackgroundItem,
    /// A system extension (network filter, camera, driver extension, endpoint security).
    SystemExtension,
    /// A loaded third-party kernel extension.
    KernelExtension,
}

/// Who the item applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// Only the current user; Kipimo can disable it without a password.
    User,
    /// Every user or the whole system; changing it needs an administrator.
    System,
}

/// The app an item belongs to, when it can be worked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerApp {
    /// Display name (the bundle's file name without `.app`, or the BTM record's name).
    pub name: String,
    pub bundle_id: Option<String>,
    /// Where the app is (or was) installed.
    pub path: Option<PathBuf>,
    /// False when the app is no longer installed: the item is a leftover.
    pub installed: bool,
}

/// What "Allow in the Background" (Background Task Management) says about an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackgroundStatus {
    /// Allowed to run.
    Allowed,
    /// Turned off by the user in System Settings > General > Login Items & Extensions.
    TurnedOff,
    /// Not registered with Background Task Management.
    NotRegistered,
    /// Background Task Management has no record of it.
    NotFound,
    /// Could not be determined (old macOS, or not applicable to this kind of item).
    Unknown,
}

/// Whether an item will start, and whether it is running now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    /// Will it start automatically? Combines the plist's `Disabled` key, launchd's own
    /// enable/disable overrides, Background Task Management and system-extension state.
    /// `None` when it can't be determined.
    pub enabled: Option<bool>,
    /// Is it currently known to launchd (loaded) in its domain? `None` when not applicable.
    pub loaded: Option<bool>,
    /// The process id when it is running right now.
    pub pid: Option<i32>,
    /// The plist sets `Disabled = true`.
    pub disabled_key: bool,
    /// launchd's override database: `Some(true)` = disabled with `launchctl disable`,
    /// `Some(false)` = explicitly enabled, `None` = no override.
    pub launchd_override_disabled: Option<bool>,
    pub background: BackgroundStatus,
    /// Raw state text where the source has one (for example a system extension's
    /// `activated_enabled`).
    pub detail: Option<String>,
}

impl Default for ItemState {
    fn default() -> Self {
        ItemState {
            enabled: None,
            loaded: None,
            pid: None,
            disabled_key: false,
            launchd_override_disabled: None,
            background: BackgroundStatus::Unknown,
            detail: None,
        }
    }
}

/// Who signed a piece of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignerKind {
    /// Signed by Apple as part of macOS or an Apple app (`anchor apple`).
    Apple,
    /// Signed by Apple for the Mac App Store.
    AppStore,
    /// Signed by a developer with an Apple-issued Developer ID certificate.
    DeveloperId,
    /// Signed with another Apple-issued certificate (for example "Apple Development").
    AppleIssued,
    /// Signed with a certificate that doesn't chain to Apple (self-signed or another CA).
    OtherCertificate,
    /// Ad-hoc signature: no identity at all (common for locally built tools).
    AdHoc,
    /// No signature.
    Unsigned,
    /// Could not be read (missing file, not code).
    Unknown,
}

impl SignerKind {
    /// Apple's own code (macOS, Apple apps).
    pub fn is_apple(self) -> bool {
        matches!(self, SignerKind::Apple)
    }
}

/// The outcome of validating a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureStatus {
    /// The signature is intact and matches the code on disk.
    Valid,
    /// Signed, but the signature no longer matches (files changed, removed or added).
    Invalid,
    /// Not signed at all.
    Unsigned,
    /// The path is missing or isn't code, so nothing could be checked.
    NotChecked,
}

/// Why a signature check failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureProblem {
    /// The Security framework's `OSStatus` (for example -67054 errSecCSBadResource).
    pub code: i32,
    /// Security framework's description, for example "a sealed resource is missing or invalid".
    pub message: String,
    /// Counts of sealed files reported missing, modified, or added since signing.
    pub missing: usize,
    pub altered: usize,
    pub added: usize,
    /// Up to a few of those files, to show the owner what changed.
    pub examples: Vec<PathBuf>,
}

/// Code-signing facts for a program, bundle or extension, read with the Security framework.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The path that was checked (a bundle when the program is a bundle's main executable).
    pub path: PathBuf,
    pub status: SignatureStatus,
    pub kind: SignerKind,
    /// The leaf certificate's name, for example
    /// "Developer ID Application: Zoom Video Communications, Inc. (BJ4HAAB9B3)".
    pub signer: Option<String>,
    pub team_id: Option<String>,
    /// The signing identifier (usually the bundle id).
    pub identifier: Option<String>,
    /// For Developer ID code: `Some(true)` when a notarization ticket was found (stapled or
    /// in the local cache), `Some(false)` when the check ran and found none. Checked offline,
    /// so `false` can mean "notarized, but never seen online on this Mac". `None` when not
    /// applicable.
    pub notarized: Option<bool>,
    pub problem: Option<SignatureProblem>,
}

impl Signature {
    pub fn is_valid(&self) -> bool {
        self.status == SignatureStatus::Valid
    }
}

/// Something about an item worth pointing out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Flag {
    /// The program it starts has no signature.
    Unsigned,
    /// Ad-hoc signed: no developer identity.
    AdHocSigned,
    /// The signature doesn't validate (the code was modified after signing).
    InvalidSignature,
    /// Signed with a certificate that doesn't chain to Apple.
    UntrustedCertificate,
    /// The program path doesn't exist (or is a broken symlink).
    ProgramMissing,
    /// The app it belongs to is no longer installed.
    OwnerAppMissing,
    /// It runs a script through an interpreter (`/bin/sh`, `python3`, `node`, ...); the script
    /// itself isn't signed.
    RunsScript,
    /// It runs a script that doesn't exist any more.
    ScriptMissing,
    /// The plist couldn't be parsed, or lacks a Label or a program.
    MalformedPlist,
}

/// How much a flag matters, for highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
}

impl Flag {
    pub fn severity(self) -> Severity {
        match self {
            Flag::RunsScript => Severity::Info,
            _ => Severity::Warning,
        }
    }

    /// The item points at something that is gone: a leftover.
    pub fn is_orphan(self) -> bool {
        matches!(
            self,
            Flag::ProgramMissing | Flag::OwnerAppMissing | Flag::ScriptMissing
        )
    }

    /// A short sentence for the UI.
    pub fn describe(self) -> &'static str {
        match self {
            Flag::Unsigned => "Not signed",
            Flag::AdHocSigned => "Ad-hoc signed (no developer identity)",
            Flag::InvalidSignature => "Signature no longer valid",
            Flag::UntrustedCertificate => "Signed with a certificate Apple didn't issue",
            Flag::ProgramMissing => "Program is missing",
            Flag::OwnerAppMissing => "Its app is no longer installed",
            Flag::RunsScript => "Runs a script",
            Flag::ScriptMissing => "Script is missing",
            Flag::MalformedPlist => "Plist is damaged or incomplete",
        }
    }
}

/// One thing set to start automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupItem {
    /// Stable id for this scan's item, used to match a selection back to an item
    /// (for example `launchd:/Users/me/Library/LaunchAgents/com.foo.plist`).
    pub id: String,
    pub kind: ItemKind,
    pub scope: Scope,
    /// launchd label, bundle identifier, or extension identifier.
    pub label: String,
    /// Display name.
    pub name: String,
    /// The launchd plist, for agents and daemons.
    pub plist_path: Option<PathBuf>,
    /// The program it starts, as declared (agents/daemons), or the bundle (login items,
    /// extensions).
    pub program: Option<PathBuf>,
    pub arguments: Vec<String>,
    /// When the program is an interpreter, the script it runs.
    pub script: Option<PathBuf>,
    pub owner: Option<OwnerApp>,
    /// Developer name from Background Task Management, when recorded.
    pub developer: Option<String>,
    pub state: ItemState,
    /// Code signing of the program (or bundle). `None` when there was nothing to check.
    pub signature: Option<Signature>,
    pub flags: Vec<Flag>,
    /// The Background Task Management record identifier, when there is one
    /// (for example `8.com.google.GoogleUpdater.wake`).
    pub btm_identifier: Option<String>,
}

impl StartupItem {
    /// Points at a missing program, script or app.
    pub fn is_orphaned(&self) -> bool {
        self.flags.iter().any(|f| f.is_orphan())
    }

    /// Has any warning-level flag (the UI highlights these).
    pub fn needs_attention(&self) -> bool {
        self.flags.iter().any(|f| f.severity() == Severity::Warning)
    }

    /// Apple's own item.
    pub fn is_apple(&self) -> bool {
        self.signature
            .as_ref()
            .is_some_and(|s| s.kind.is_apple() && s.is_valid())
            && self.script.is_none()
    }
}

/// Where the Background Task Management records came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BtmSource {
    /// Read from the on-disk database (no root needed).
    Database(PathBuf),
    /// The database couldn't be read or parsed, so the records came from
    /// `/usr/bin/sfltool dumpbtm` (last resort).
    SflTool,
    /// Not available; the reason is in the string.
    Unavailable(String),
}

/// The result of a startup-items scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub items: Vec<StartupItem>,
    pub btm_source: BtmSource,
    /// Non-fatal problems (an unreadable folder, a source that failed).
    pub warnings: Vec<String>,
    /// True when the scan stopped early because it was cancelled; `items` holds what was
    /// finished.
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

/// Progress of a scan, passed to the callback after each item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
}
