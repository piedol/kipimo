//! The startup-items scan: gathers launchd plists, Background Task Management records,
//! system extensions and kernel extensions, then checks each program's signature.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::btm::{self, Database, Record};
use crate::codesign::{Checker, Depth};
use crate::launchd::{self, JobPlist};
use crate::launchd_state::{self, Job};
use crate::model::*;
use crate::owner;
use crate::sysext::{self, Kext, SystemExtension};

/// Where the scan looks. [`Roots::system`] is the real Mac; tests point these at temp folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roots {
    /// `~/Library/LaunchAgents`
    pub user_agents: PathBuf,
    /// `/Library/LaunchAgents`
    pub system_agents: PathBuf,
    /// `/Library/LaunchDaemons`
    pub system_daemons: PathBuf,
    /// Folder holding `BackgroundItems-v<N>.btm`; `None` skips Background Task Management.
    pub btm_dir: Option<PathBuf>,
    /// sysextd's `db.plist`; `None` skips system extensions.
    pub sysext_db: Option<PathBuf>,
    /// launchd's override folder (`disabled.<uid>.plist`, `disabled.plist`); `None` skips.
    pub launchd_overrides: Option<PathBuf>,
}

impl Roots {
    /// The real locations on this Mac.
    pub fn system() -> Roots {
        Roots {
            user_agents: home_dir().join("Library/LaunchAgents"),
            system_agents: PathBuf::from("/Library/LaunchAgents"),
            system_daemons: PathBuf::from("/Library/LaunchDaemons"),
            btm_dir: Some(PathBuf::from(btm::DEFAULT_DIR)),
            sysext_db: Some(PathBuf::from(sysext::DEFAULT_DB)),
            launchd_overrides: Some(PathBuf::from("/private/var/db/com.apple.xpc.launchd")),
        }
    }

    /// Only launchd folders under `base` (`base/user`, `base/agents`, `base/daemons`), nothing
    /// else. For tests and dry runs.
    pub fn synthetic(base: &Path) -> Roots {
        Roots {
            user_agents: base.join("user"),
            system_agents: base.join("agents"),
            system_daemons: base.join("daemons"),
            btm_dir: None,
            sysext_db: None,
            launchd_overrides: None,
        }
    }
}

/// The user's home folder (`$HOME`, else the password database).
pub(crate) fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    // SAFETY: getpwuid returns a pointer into static storage or NULL.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_dir.is_null() {
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
            return PathBuf::from(dir.to_string_lossy().into_owned());
        }
    }
    PathBuf::from("/var/empty")
}

/// What to scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub roots: Roots,
    /// Ask launchd which jobs are loaded/running and Background Task Management whether each
    /// legacy plist is allowed (read-only). Off for deterministic tests.
    pub live_state: bool,
    /// Query IOKit for loaded third-party kernel extensions.
    pub kernel_extensions: bool,
    /// When the BTM database can't be read, fall back to `/usr/bin/sfltool dumpbtm`.
    pub sfltool_fallback: bool,
    /// Whose BTM records count as "this user's" (uppercase UUID). `None` = the current user.
    pub btm_user_uuid: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            roots: Roots::system(),
            live_state: true,
            kernel_extensions: true,
            sfltool_fallback: true,
            btm_user_uuid: None,
        }
    }
}

/// Live, read-only system queries, behind a trait so the merge logic is testable.
pub(crate) trait Live {
    fn jobs(&self, system: bool) -> HashMap<String, Job>;
    fn legacy_status(&self, plist: &Path) -> BackgroundStatus;
    fn apps_for_bundle_id(&self, id: &str) -> Vec<PathBuf>;
    fn kexts(&self) -> Vec<Kext>;
    fn user_uuid(&self) -> Option<String>;
    fn uid(&self) -> u32;
    fn sfltool(&self) -> Result<String, String>;
}

pub(crate) struct SystemLive {
    pub state: bool,
    pub kexts: bool,
}

impl Live for SystemLive {
    fn jobs(&self, system: bool) -> HashMap<String, Job> {
        if self.state {
            launchd_state::jobs(system)
        } else {
            HashMap::new()
        }
    }
    fn legacy_status(&self, plist: &Path) -> BackgroundStatus {
        if self.state {
            launchd_state::legacy_status(plist)
        } else {
            BackgroundStatus::Unknown
        }
    }
    fn apps_for_bundle_id(&self, id: &str) -> Vec<PathBuf> {
        owner::apps_for_bundle_id(id)
    }
    fn kexts(&self) -> Vec<Kext> {
        if self.kexts {
            sysext::loaded_third_party_kexts()
        } else {
            Vec::new()
        }
    }
    fn user_uuid(&self) -> Option<String> {
        btm::current_user_uuid()
    }
    fn uid(&self) -> u32 {
        // SAFETY: getuid never fails.
        unsafe { libc::getuid() }
    }
    fn sfltool(&self) -> Result<String, String> {
        btm::run_sfltool(Duration::from_secs(15))
    }
}

/// Scans everything set to start automatically.
///
/// `on_item` is called after each item is finished (signature checked), with progress; the
/// returned [`Inventory`] holds the same items. Set `cancel` to stop early: the inventory
/// then has `cancelled = true` and the items finished so far.
pub fn scan<F>(options: &ScanOptions, cancel: &AtomicBool, on_item: F) -> Inventory
where
    F: FnMut(ScanProgress, &StartupItem),
{
    let live = SystemLive {
        state: options.live_state,
        kexts: options.kernel_extensions,
    };
    scan_with(options, &live, cancel, on_item)
}

struct Context<'a> {
    live: &'a dyn Live,
    btm: Option<Database>,
    user_uuid: Option<String>,
    legacy: HashMap<PathBuf, Record>,
    user_jobs: HashMap<String, Job>,
    system_jobs: HashMap<String, Job>,
    user_overrides: HashMap<String, bool>,
    system_overrides: HashMap<String, bool>,
}

pub(crate) fn scan_with<F>(
    options: &ScanOptions,
    live: &dyn Live,
    cancel: &AtomicBool,
    mut on_item: F,
) -> Inventory
where
    F: FnMut(ScanProgress, &StartupItem),
{
    let started = Instant::now();
    let mut warnings = Vec::new();
    let roots = &options.roots;

    let (btm_db, btm_source) = load_btm(roots, options.sfltool_fallback, live, &mut warnings);
    let user_uuid = options.btm_user_uuid.clone().or_else(|| live.user_uuid());
    let legacy = btm_db
        .as_ref()
        .map(|db| db.legacy_by_plist(user_uuid.as_deref()))
        .unwrap_or_default();
    let (user_overrides, system_overrides) = match &roots.launchd_overrides {
        Some(dir) => (
            launchd_state::read_overrides(&dir.join(format!("disabled.{}.plist", live.uid()))),
            launchd_state::read_overrides(&dir.join("disabled.plist")),
        ),
        None => Default::default(),
    };
    let ctx = Context {
        live,
        btm: btm_db,
        user_uuid,
        legacy,
        user_jobs: live.jobs(false),
        system_jobs: live.jobs(true),
        user_overrides,
        system_overrides,
    };

    // 1. Gather everything (cheap: file reads and lookups).
    let mut pending: Vec<StartupItem> = Vec::new();
    for (dir, kind, scope) in [
        (&roots.user_agents, ItemKind::LaunchAgent, Scope::User),
        (&roots.system_agents, ItemKind::LaunchAgent, Scope::System),
        (&roots.system_daemons, ItemKind::LaunchDaemon, Scope::System),
    ] {
        match launchd::list_plists(dir) {
            Ok(paths) => {
                for path in paths {
                    pending.push(launchd_item(launchd::read_job(&path), kind, scope, &ctx));
                }
            }
            Err(e) => warnings.push(format!("can't list {}: {e}", dir.display())),
        }
    }
    pending.extend(btm_items(&ctx));
    if let Some(db) = &roots.sysext_db {
        match sysext::read_db(db) {
            Ok(exts) => pending.extend(exts.iter().map(|e| sysext_item(e, &ctx))),
            Err(e) => warnings.push(e),
        }
    }
    pending.extend(live.kexts().iter().map(|k| kext_item(k, &ctx)));

    // 2. Signatures (the slow part), streaming each finished item.
    let checker = Checker::new();
    let mut cache: HashMap<PathBuf, Signature> = HashMap::new();
    let total = pending.len();
    let mut items = Vec::with_capacity(total);
    let mut cancelled = false;
    for mut item in pending {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if let Some(program) = item.program.clone()
            && program.exists()
        {
            let key = std::fs::canonicalize(&program).unwrap_or(program);
            let sig = cache
                .entry(key.clone())
                .or_insert_with(|| checker.check(&key, Depth::STANDARD))
                .clone();
            add_signature_flags(&mut item.flags, &sig);
            item.signature = Some(sig);
        }
        items.push(item);
        on_item(
            ScanProgress {
                done: items.len(),
                total,
            },
            items.last().expect("just pushed"),
        );
    }

    Inventory {
        items,
        btm_source,
        warnings,
        cancelled,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn load_btm(
    roots: &Roots,
    sfltool_fallback: bool,
    live: &dyn Live,
    warnings: &mut Vec<String>,
) -> (Option<Database>, BtmSource) {
    let Some(dir) = &roots.btm_dir else {
        return (None, BtmSource::Unavailable("not requested".into()));
    };
    let from_db = match btm::database_path(dir) {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => btm::parse_database(&bytes).map(|db| (db, path.clone())),
            Err(e) => Err(format!("can't read {}: {e}", path.display())),
        },
        None => Err(format!("no BackgroundItems database in {}", dir.display())),
    };
    let db_error = match from_db {
        Ok((db, path)) => return (Some(db), BtmSource::Database(path)),
        Err(e) => e,
    };
    if !sfltool_fallback {
        warnings.push(format!("Background Task Management: {db_error}"));
        return (None, BtmSource::Unavailable(db_error));
    }
    match live.sfltool().and_then(|text| btm::parse_sfltool(&text)) {
        Ok(db) => {
            warnings.push(format!(
                "Background Task Management database unusable ({db_error}); used sfltool"
            ));
            (Some(db), BtmSource::SflTool)
        }
        Err(e) => {
            let reason = format!("{db_error}; sfltool: {e}");
            warnings.push(format!("Background Task Management: {reason}"));
            (None, BtmSource::Unavailable(reason))
        }
    }
}

fn add_signature_flags(flags: &mut Vec<Flag>, sig: &Signature) {
    let mut add = |f: Flag| {
        if !flags.contains(&f) {
            flags.push(f);
        }
    };
    match sig.status {
        SignatureStatus::Unsigned => add(Flag::Unsigned),
        SignatureStatus::Invalid => add(Flag::InvalidSignature),
        _ => {}
    }
    match sig.kind {
        SignerKind::AdHoc => add(Flag::AdHocSigned),
        SignerKind::OtherCertificate => add(Flag::UntrustedCertificate),
        _ => {}
    }
}

/// Exists, following symlinks (a dangling symlink counts as missing).
fn present(path: &Path) -> bool {
    path.exists()
}

fn owner_for_program(program: Option<&Path>) -> Option<OwnerApp> {
    let program = program?;
    let resolved = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    launchd::enclosing_app(&resolved)
        .or_else(|| launchd::enclosing_app(program))
        .map(|app| owner::owner_from_app_path(&app))
}

fn record_app_path(ctx: &Context, record: &Record) -> Option<PathBuf> {
    ctx.btm
        .as_ref()?
        .resolve_path(record, ctx.user_uuid.as_deref())
}

/// The app record a BTM record hangs off (its container), if that container is an app.
fn container_app<'c>(ctx: &'c Context, record: &Record) -> Option<&'c Record> {
    let db = ctx.btm.as_ref()?;
    let parent = db.find(record.container.as_deref()?, ctx.user_uuid.as_deref())?;
    (parent.base_type() == btm::TYPE_APP).then_some(parent)
}

fn launchd_item(job: JobPlist, kind: ItemKind, scope: Scope, ctx: &Context) -> StartupItem {
    let record = ctx.legacy.get(&job.path);
    let label = job.label.clone().unwrap_or_else(|| {
        job.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    // The developer's own declaration (`AssociatedBundleIdentifiers`, also what System
    // Settings uses to attribute items) wins over the folder the program sits in: an updater
    // living in Application Support is still a leftover once its main app is gone.
    let mut ids = job.associated_bundle_ids.clone();
    for id in record.iter().flat_map(|r| r.associated_bundle_ids.iter()) {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    let enclosing = owner_for_program(job.program.as_deref());
    let mut owner = if ids.is_empty() {
        enclosing
    } else if let Some(app) =
        enclosing.filter(|o| o.bundle_id.as_ref().is_some_and(|b| ids.contains(b)))
    {
        // The program is inside the declared app (LaunchServices may not know every copy).
        Some(app)
    } else {
        owner::owner_from_bundle_ids(&ids, None, &|id| ctx.live.apps_for_bundle_id(id))
    };
    if owner.is_none()
        && let Some(app) = record.and_then(|r| container_app(ctx, r))
        && let Some(path) = record_app_path(ctx, app)
    {
        let mut o = owner::owner_from_app_path(&path);
        if let Some(name) = &app.name {
            o.name = name.clone();
        }
        owner = Some(o);
    }

    // `BundleProgram` is relative to the owning app's bundle.
    let program = job.program.clone().or_else(|| {
        let rel = job.bundle_program.as_deref()?;
        Some(owner.as_ref()?.path.as_ref()?.join(rel))
    });
    let script = program
        .as_deref()
        .and_then(|p| launchd::script_target(p, &job.arguments));

    let developer = record.and_then(|r| {
        r.developer_name.clone().or_else(|| {
            let db = ctx.btm.as_ref()?;
            db.find(r.container.as_deref()?, ctx.user_uuid.as_deref())?
                .developer_name
                .clone()
        })
    });

    let (jobs, overrides) = match kind {
        ItemKind::LaunchDaemon => (&ctx.system_jobs, &ctx.system_overrides),
        _ => (&ctx.user_jobs, &ctx.user_overrides),
    };
    let loaded_job = job.label.as_ref().and_then(|l| jobs.get(l));
    let mut background = ctx.live.legacy_status(&job.path);
    if background == BackgroundStatus::Unknown
        && let Some(r) = record
    {
        background = if r.enabled() && r.allowed() {
            BackgroundStatus::Allowed
        } else {
            BackgroundStatus::TurnedOff
        };
    }
    let override_disabled = job.label.as_ref().and_then(|l| overrides.get(l).copied());
    let enabled = if job.problem.is_some() {
        None
    } else {
        Some(launchd_enabled(job.disabled, override_disabled, background))
    };
    let state = ItemState {
        enabled,
        loaded: (!jobs.is_empty()).then_some(loaded_job.is_some()),
        pid: loaded_job.and_then(|j| j.pid),
        disabled_key: job.disabled,
        launchd_override_disabled: override_disabled,
        background,
        detail: job.problem.clone(),
    };

    let mut flags = Vec::new();
    if job.problem.is_some() {
        flags.push(Flag::MalformedPlist);
    }
    if let Some(p) = &program
        && !present(p)
    {
        flags.push(Flag::ProgramMissing);
    }
    if let Some(s) = &script {
        flags.push(Flag::RunsScript);
        if !present(s) {
            flags.push(Flag::ScriptMissing);
        }
    }
    if owner.as_ref().is_some_and(|o| !o.installed) {
        flags.push(Flag::OwnerAppMissing);
    }

    StartupItem {
        id: format!("launchd:{}", job.path.display()),
        kind,
        scope,
        name: label.clone(),
        label,
        plist_path: Some(job.path.clone()),
        program,
        arguments: job.arguments,
        script,
        owner,
        developer,
        state,
        signature: None,
        flags,
        btm_identifier: record.map(|r| r.identifier.clone()),
    }
}

/// launchd's precedence: an override beats the plist's `Disabled` key, and BTM's
/// "Allow in the Background" switch beats both.
pub(crate) fn launchd_enabled(
    disabled_key: bool,
    override_disabled: Option<bool>,
    background: BackgroundStatus,
) -> bool {
    let mut enabled = !disabled_key;
    if let Some(disabled) = override_disabled {
        enabled = !disabled;
    }
    if background == BackgroundStatus::TurnedOff {
        enabled = false;
    }
    enabled
}

/// Login items and app-registered background items from BTM (legacy plists are covered by
/// the folder scan).
fn btm_items(ctx: &Context) -> Vec<StartupItem> {
    let Some(db) = &ctx.btm else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let me = ctx.user_uuid.as_deref().and_then(|u| db.user(u));
    let mine = me
        .map(|u| u.records.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let daemons = db
        .users
        .iter()
        .flat_map(|u| u.records.iter())
        .filter(|r| !r.is_legacy() && r.base_type() == btm::TYPE_DAEMON);

    for record in mine.into_iter().chain(daemons) {
        if record.is_legacy() || !seen.insert(record.identifier.clone()) {
            continue;
        }
        let kind = match record.base_type() {
            btm::TYPE_APP if record.enabled() => ItemKind::LoginItem,
            btm::TYPE_USER_ITEM => ItemKind::LoginItem,
            btm::TYPE_LOGIN_ITEM | btm::TYPE_AGENT | btm::TYPE_DAEMON => ItemKind::BackgroundItem,
            _ => continue,
        };
        out.push(btm_item(record, kind, ctx));
    }
    out
}

fn btm_item(record: &Record, kind: ItemKind, ctx: &Context) -> StartupItem {
    let base = record.base_type();
    let scope = if base == btm::TYPE_DAEMON {
        Scope::System
    } else {
        Scope::User
    };
    let label = record
        .bundle_id
        .clone()
        .unwrap_or_else(|| record.short_identifier().to_string());
    let url_path = record_app_path(ctx, record);
    let (program, plist_path) = match base {
        btm::TYPE_AGENT | btm::TYPE_DAEMON => (
            record
                .executable_path
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| record.program_arguments.first().map(PathBuf::from)),
            url_path.clone(),
        ),
        _ => (url_path.clone(), None),
    };

    let owner = if kind == ItemKind::LoginItem {
        program.as_deref().map(|p| {
            let mut o = owner::owner_from_app_path(p);
            if let Some(name) = &record.name {
                o.name = name.clone();
            }
            o.bundle_id = o.bundle_id.or_else(|| record.bundle_id.clone());
            o
        })
    } else {
        container_app(ctx, record)
            .and_then(|app| record_app_path(ctx, app).map(|p| (app, p)))
            .map(|(app, p)| {
                let mut o = owner::owner_from_app_path(&p);
                if let Some(name) = &app.name {
                    o.name = name.clone();
                }
                o
            })
            .or_else(|| owner_for_program(program.as_deref()))
    };

    let jobs = if scope == Scope::System {
        &ctx.system_jobs
    } else {
        &ctx.user_jobs
    };
    let short = record.short_identifier();
    let job = jobs
        .get(short)
        .or_else(|| record.bundle_id.as_ref().and_then(|b| jobs.get(b)));
    // A running app shows up in launchd as `application.<bundle id>.<numbers>`.
    let app_pid = (kind == ItemKind::LoginItem)
        .then(|| {
            let prefix = format!("application.{label}.");
            jobs.iter()
                .find(|(l, j)| l.starts_with(&prefix) && j.pid.is_some())
                .and_then(|(_, j)| j.pid)
        })
        .flatten();
    let on = record.enabled() && record.allowed();
    let state = ItemState {
        enabled: Some(on),
        loaded: (!jobs.is_empty() && kind == ItemKind::BackgroundItem).then_some(job.is_some()),
        pid: job.and_then(|j| j.pid).or(app_pid),
        disabled_key: false,
        launchd_override_disabled: None,
        background: if on {
            BackgroundStatus::Allowed
        } else {
            BackgroundStatus::TurnedOff
        },
        detail: Some(format!("BTM disposition 0x{:x}", record.disposition)),
    };

    let mut flags = Vec::new();
    if let Some(p) = &program
        && !present(p)
    {
        flags.push(Flag::ProgramMissing);
    }
    if owner.as_ref().is_some_and(|o| !o.installed) {
        flags.push(Flag::OwnerAppMissing);
    }

    StartupItem {
        id: format!("btm:{}", record.identifier),
        kind,
        scope,
        name: record.name.clone().unwrap_or_else(|| label.clone()),
        label,
        plist_path,
        program,
        arguments: record.program_arguments.clone(),
        script: None,
        owner,
        developer: record.developer_name.clone(),
        state,
        signature: None,
        flags,
        btm_identifier: Some(record.identifier.clone()),
    }
}

fn sysext_item(ext: &SystemExtension, _ctx: &Context) -> StartupItem {
    let program = ext
        .staged_path
        .clone()
        .filter(|p| p.exists())
        .or_else(|| ext.origin_path.clone())
        .or_else(|| ext.staged_path.clone());
    let owner = ext.app_path.as_deref().map(|app| {
        let mut o = owner::owner_from_app_path(app);
        o.bundle_id = o.bundle_id.or_else(|| ext.app_bundle_id.clone());
        o
    });
    let bundle_name = program
        .as_deref()
        .filter(|p| p.is_dir())
        .and_then(|p| owner::bundle_info(p).1);
    let name = match (bundle_name, ext.category_label()) {
        (Some(n), _) => n,
        (None, Some(cat)) => format!("{cat} {}", ext.identifier),
        (None, None) => ext.identifier.clone(),
    };
    let mut flags = Vec::new();
    if program.as_deref().is_none_or(|p| !present(p)) {
        flags.push(Flag::ProgramMissing);
    }
    if owner.as_ref().is_some_and(|o| !o.installed) {
        flags.push(Flag::OwnerAppMissing);
    }
    StartupItem {
        id: format!("sysext:{}", ext.identifier),
        kind: ItemKind::SystemExtension,
        scope: Scope::System,
        label: ext.identifier.clone(),
        name,
        plist_path: None,
        program,
        arguments: Vec::new(),
        script: None,
        owner,
        developer: None,
        state: ItemState {
            enabled: Some(ext.enabled()),
            loaded: Some(ext.state.starts_with("activated")),
            detail: Some(ext.state.clone()),
            ..ItemState::default()
        },
        signature: None,
        flags,
        btm_identifier: None,
    }
}

fn kext_item(kext: &Kext, _ctx: &Context) -> StartupItem {
    let name = kext
        .path
        .as_deref()
        .and_then(|p| owner::bundle_info(p).1)
        .unwrap_or_else(|| kext.bundle_id.clone());
    let mut flags = Vec::new();
    if kext.path.as_deref().is_some_and(|p| !present(p)) {
        flags.push(Flag::ProgramMissing);
    }
    StartupItem {
        id: format!("kext:{}", kext.bundle_id),
        kind: ItemKind::KernelExtension,
        scope: Scope::System,
        label: kext.bundle_id.clone(),
        name,
        plist_path: None,
        program: kext.path.clone(),
        arguments: Vec::new(),
        script: None,
        owner: None,
        developer: None,
        state: ItemState {
            enabled: Some(true),
            loaded: Some(true),
            detail: kext.started.map(|s| {
                if s {
                    "started".into()
                } else {
                    "loaded, not started".into()
                }
            }),
            ..ItemState::default()
        },
        signature: None,
        flags,
        btm_identifier: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Deterministic stand-in for the live system.
    #[derive(Default)]
    pub(crate) struct FakeLive {
        pub user_jobs: HashMap<String, Job>,
        pub system_jobs: HashMap<String, Job>,
        pub legacy: HashMap<PathBuf, BackgroundStatus>,
        pub apps: HashMap<String, PathBuf>,
        pub sfltool: Option<String>,
    }

    impl Live for FakeLive {
        fn jobs(&self, system: bool) -> HashMap<String, Job> {
            if system {
                self.system_jobs.clone()
            } else {
                self.user_jobs.clone()
            }
        }
        fn legacy_status(&self, plist: &Path) -> BackgroundStatus {
            self.legacy
                .get(plist)
                .copied()
                .unwrap_or(BackgroundStatus::Unknown)
        }
        fn apps_for_bundle_id(&self, id: &str) -> Vec<PathBuf> {
            self.apps.get(id).cloned().into_iter().collect()
        }
        fn kexts(&self) -> Vec<Kext> {
            Vec::new()
        }
        fn user_uuid(&self) -> Option<String> {
            Some("11111111-2222-3333-4444-555555555555".into())
        }
        fn uid(&self) -> u32 {
            501
        }
        fn sfltool(&self) -> Result<String, String> {
            self.sfltool
                .clone()
                .ok_or_else(|| "no sfltool in tests".into())
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn agent(label: &str, args: &[&str], extra: &str) -> String {
        let args: String = args
            .iter()
            .map(|a| format!("<string>{a}</string>"))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{args}</array>{extra}</dict></plist>"#
        )
    }

    fn by_label<'a>(inv: &'a Inventory, label: &str) -> &'a StartupItem {
        inv.items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no item {label}"))
    }

    #[test]
    fn enabled_precedence() {
        use BackgroundStatus::*;
        assert!(launchd_enabled(false, None, Allowed));
        assert!(!launchd_enabled(true, None, Allowed));
        assert!(launchd_enabled(true, Some(false), Unknown));
        assert!(!launchd_enabled(false, Some(true), Unknown));
        assert!(!launchd_enabled(false, Some(false), TurnedOff));
    }

    #[test]
    fn merges_launchd_state_btm_and_owner() {
        let tmp = TempDir::new("inventory");
        let base = tmp.path();
        let mut roots = Roots::synthetic(base);
        let overrides = base.join("overrides");
        roots.launchd_overrides = Some(overrides.clone());
        write(
            &overrides.join("disabled.501.plist"),
            r#"<plist version="1.0"><dict><key>com.example.overridden</key><true/></dict></plist>"#,
        );

        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::copy("/usr/bin/true", bin.join("tool")).unwrap();
        let app = base.join("Apps/Example Notes.app");
        write(
            &app.join("Contents/Info.plist"),
            r#"<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.notes</string></dict></plist>"#,
        );
        let tool = bin.join("tool");
        let tool = tool.to_str().unwrap();
        write(
            &roots.user_agents.join("com.example.running.plist"),
            &agent("com.example.running", &[tool], ""),
        );
        write(
            &roots.user_agents.join("com.example.overridden.plist"),
            &agent("com.example.overridden", &[tool], ""),
        );
        write(
            &roots.user_agents.join("com.example.owned.plist"),
            &agent(
                "com.example.owned",
                &[tool],
                "<key>AssociatedBundleIdentifiers</key><array><string>com.example.notes</string></array>",
            ),
        );
        write(
            &roots.user_agents.join("com.example.orphan.plist"),
            &agent(
                "com.example.orphan",
                &[tool],
                "<key>AssociatedBundleIdentifiers</key><string>com.example.uninstalled</string>",
            ),
        );
        write(
            &roots.system_daemons.join("com.example.daemon.plist"),
            &agent("com.example.daemon", &[tool], ""),
        );

        let mut live = FakeLive::default();
        live.user_jobs
            .insert("com.example.running".into(), Job { pid: Some(4242) });
        live.user_jobs
            .insert("com.example.owned".into(), Job { pid: None });
        live.system_jobs
            .insert("com.example.daemon".into(), Job { pid: Some(7) });
        live.legacy.insert(
            roots.user_agents.join("com.example.owned.plist"),
            BackgroundStatus::TurnedOff,
        );
        live.apps.insert("com.example.notes".into(), app.clone());

        let options = ScanOptions {
            roots,
            live_state: true,
            kernel_extensions: false,
            sfltool_fallback: false,
            btm_user_uuid: None,
        };
        let cancel = AtomicBool::new(false);
        let mut seen = 0;
        let inv = scan_with(&options, &live, &cancel, |p, _| {
            seen += 1;
            assert_eq!(p.done, seen);
        });
        assert_eq!(inv.items.len(), 5);
        assert_eq!(seen, 5);
        assert!(!inv.cancelled);

        let running = by_label(&inv, "com.example.running");
        assert_eq!(running.state.loaded, Some(true));
        assert_eq!(running.state.pid, Some(4242));
        assert_eq!(running.state.enabled, Some(true));
        assert_eq!(running.scope, Scope::User);
        // A copy of an Apple binary loses nothing: still Apple-signed and valid.
        let sig = running.signature.as_ref().unwrap();
        assert_eq!(sig.kind, SignerKind::Apple);
        assert!(sig.is_valid());
        assert!(running.flags.is_empty(), "{:?}", running.flags);

        let overridden = by_label(&inv, "com.example.overridden");
        assert_eq!(overridden.state.launchd_override_disabled, Some(true));
        assert_eq!(overridden.state.enabled, Some(false));
        assert_eq!(overridden.state.loaded, Some(false));

        let owned = by_label(&inv, "com.example.owned");
        assert_eq!(
            owned.owner.as_ref().unwrap().path.as_deref(),
            Some(app.as_path())
        );
        assert!(owned.owner.as_ref().unwrap().installed);
        assert_eq!(owned.state.background, BackgroundStatus::TurnedOff);
        assert_eq!(owned.state.enabled, Some(false));

        let orphan = by_label(&inv, "com.example.orphan");
        assert!(orphan.flags.contains(&Flag::OwnerAppMissing));
        assert!(orphan.is_orphaned());

        let daemon = by_label(&inv, "com.example.daemon");
        assert_eq!(daemon.kind, ItemKind::LaunchDaemon);
        assert_eq!(daemon.scope, Scope::System);
        assert_eq!(daemon.state.pid, Some(7));
    }

    #[test]
    fn btm_fixture_items() {
        let tmp = TempDir::new("btm-items");
        let mut roots = Roots::synthetic(tmp.path());
        let btm_dir = tmp.path().join("btm");
        std::fs::create_dir_all(&btm_dir).unwrap();
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/btm-v16-synthetic.btm"
            ),
            btm_dir.join("BackgroundItems-v16.btm"),
        )
        .unwrap();
        roots.btm_dir = Some(btm_dir.clone());
        let options = ScanOptions {
            roots,
            live_state: false,
            kernel_extensions: false,
            sfltool_fallback: false,
            btm_user_uuid: None,
        };
        let live = FakeLive::default();
        let inv = scan_with(&options, &live, &AtomicBool::new(false), |_, _| {});
        assert_eq!(
            inv.btm_source,
            BtmSource::Database(btm_dir.join("BackgroundItems-v16.btm"))
        );

        let ids: Vec<&str> = inv.items.iter().map(|i| i.id.as_str()).collect();
        // The enabled app, the login-item helper, the bundled agent and the bundled daemon...
        assert!(ids.contains(&"btm:2.com.example.notes"), "{ids:?}");
        assert!(ids.contains(&"btm:4.com.example.sync.launcher"));
        assert!(ids.contains(&"btm:8.com.example.notes.agent"));
        // ...but not the parent-only app, the developer group, the Spotlight importer, or the
        // legacy records (those come from the plist folders, which are empty here).
        assert!(!ids.contains(&"btm:2.com.example.sync"));
        assert!(
            !ids.iter()
                .any(|i| i.contains("importer") || i.contains("Example Corp"))
        );
        assert!(
            !ids.iter()
                .any(|i| i.contains("updater") || i.contains("helperd"))
        );

        let notes = inv
            .items
            .iter()
            .find(|i| i.id == "btm:2.com.example.notes")
            .unwrap();
        assert_eq!(notes.kind, ItemKind::LoginItem);
        assert_eq!(
            notes.program,
            Some(PathBuf::from("/Applications/Example Notes.app"))
        );
        assert_eq!(notes.state.enabled, Some(true));
        // Not installed on this Mac: flagged as a leftover login item.
        assert!(notes.flags.contains(&Flag::ProgramMissing));

        let helper = inv
            .items
            .iter()
            .find(|i| i.id == "btm:4.com.example.sync.launcher")
            .unwrap();
        assert_eq!(helper.kind, ItemKind::BackgroundItem);
        assert_eq!(helper.state.enabled, Some(false));
        assert_eq!(
            helper.program,
            Some(PathBuf::from(
                "/Applications/Example Sync.app/Contents/Library/LoginItems/Sync Launcher.app"
            ))
        );
        assert_eq!(helper.owner.as_ref().unwrap().name, "Example Sync");
    }

    #[test]
    fn btm_falls_back_to_sfltool_text() {
        let tmp = TempDir::new("btm-fallback");
        let mut roots = Roots::synthetic(tmp.path());
        let btm_dir = tmp.path().join("btm");
        std::fs::create_dir_all(&btm_dir).unwrap();
        std::fs::write(btm_dir.join("BackgroundItems-v99.btm"), b"garbage").unwrap();
        roots.btm_dir = Some(btm_dir);
        let options = ScanOptions {
            roots,
            live_state: false,
            kernel_extensions: false,
            sfltool_fallback: true,
            btm_user_uuid: None,
        };
        let live = FakeLive {
            sfltool: Some(include_str!("../tests/fixtures/sfltool-dumpbtm-synthetic.txt").into()),
            ..FakeLive::default()
        };
        let inv = scan_with(&options, &live, &AtomicBool::new(false), |_, _| {});
        assert_eq!(inv.btm_source, BtmSource::SflTool);
        assert!(inv.items.iter().any(|i| i.id == "btm:2.com.example.notes"));
        assert!(
            inv.items
                .iter()
                .any(|i| i.id == "btm:8.com.example.notes.agent")
        );
        assert!(inv.warnings.iter().any(|w| w.contains("sfltool")));
    }

    #[test]
    fn legacy_records_enrich_plists() {
        // The fixture's legacy updater record points at /Users/tester/...; give the folder scan
        // a plist at that same path by remapping the record onto a temp plist.
        let tmp = TempDir::new("legacy-enrich");
        let roots = Roots::synthetic(tmp.path());
        let plist = roots.user_agents.join("com.example.updater.plist");
        write(
            &plist,
            &agent("com.example.updater", &["/nonexistent/Updater"], ""),
        );
        let mut db = btm::parse_sfltool(include_str!(
            "../tests/fixtures/sfltool-dumpbtm-synthetic.txt"
        ))
        .unwrap();
        for user in &mut db.users {
            for r in &mut user.records {
                if r.identifier == "8.com.example.updater" {
                    r.url = Some(format!("file://{}", plist.display()));
                    r.disposition = 0xa; // turned off
                }
            }
        }
        let live = FakeLive::default();
        let ctx = Context {
            live: &live,
            legacy: db.legacy_by_plist(Some("11111111-2222-3333-4444-555555555555")),
            btm: Some(db),
            user_uuid: Some("11111111-2222-3333-4444-555555555555".into()),
            user_jobs: HashMap::new(),
            system_jobs: HashMap::new(),
            user_overrides: HashMap::new(),
            system_overrides: HashMap::new(),
        };
        let item = launchd_item(
            launchd::read_job(&plist),
            ItemKind::LaunchAgent,
            Scope::User,
            &ctx,
        );
        assert_eq!(
            item.btm_identifier.as_deref(),
            Some("8.com.example.updater")
        );
        assert_eq!(item.developer.as_deref(), Some("Example Corp"));
        assert_eq!(item.state.background, BackgroundStatus::TurnedOff);
        assert_eq!(item.state.enabled, Some(false));
        assert!(item.flags.contains(&Flag::ProgramMissing));
        // Associated app com.example.notes isn't installed.
        assert!(item.flags.contains(&Flag::OwnerAppMissing));
        assert_eq!(item.state.loaded, None);
    }

    #[test]
    fn cancel_stops_between_items() {
        let tmp = TempDir::new("cancel");
        let roots = Roots::synthetic(tmp.path());
        for i in 0..5 {
            write(
                &roots.user_agents.join(format!("com.example.{i}.plist")),
                &agent(&format!("com.example.{i}"), &["/bin/sh", "-c", "true"], ""),
            );
        }
        let options = ScanOptions {
            roots,
            live_state: false,
            kernel_extensions: false,
            sfltool_fallback: false,
            btm_user_uuid: None,
        };
        let cancel = AtomicBool::new(false);
        let inv = scan_with(&options, &FakeLive::default(), &cancel, |p, _| {
            if p.done == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(inv.cancelled);
        assert_eq!(inv.items.len(), 2);
    }
}
