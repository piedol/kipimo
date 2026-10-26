//! The leftover finder: what an app leaves in `~/Library` (and `/Library`, reported only).
//!
//! Matching is deliberately narrow. A folder or file counts only when its name *is* one of the
//! app's bundle IDs (or that ID followed by `.something`, the developer's own namespace), a known
//! wrapper of it (`TEAMID.<id>`, `group.<id>`, `<id>.plist`, `<id>.savedState`,
//! `<id>.binarycookies`, ByHost `<id>.<host>.plist`), an App Group the app is entitled to, a
//! launchd job that names or launches the app, or — in Application Support, Caches and Logs only —
//! exactly the app's name. There are no substring matches. Anything another installed app still
//! uses is moved to `kept` instead, with the reason.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::apps::{AppIndex, AppTarget, IndexedApp, vendor};
use crate::env::Env;
use crate::plist;
use crate::trash::{self, Zone};
use crate::types::{ItemResult, SkipReason};
use crate::walk;

/// Where a leftover lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverKind {
    ApplicationSupport,
    ApplicationScripts,
    Caches,
    Container,
    GroupContainer,
    Preferences,
    ByHostPreferences,
    SavedState,
    HttpStorage,
    WebKit,
    Logs,
    Cookies,
    LaunchAgent,
    LaunchDaemon,
    PrivilegedHelper,
}

/// Kinds where an app keeps the user's own data (documents, libraries, databases, settings
/// beyond preferences): offered, but never ticked by default. Caches, logs, preferences, saved
/// window state and web storage are rebuilt or reset harmlessly, so they start ticked.
pub fn holds_user_data(kind: LeftoverKind) -> bool {
    matches!(
        kind,
        LeftoverKind::ApplicationSupport | LeftoverKind::Container | LeftoverKind::GroupContainer
    )
}

/// How sure the finder is that an item belongs to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Named like the app with a suffix (`<Name>_Helper`). Never ticked by default.
    Low,
    /// Named exactly like the app (not its bundle ID).
    Medium,
    /// Named by the app's bundle ID, an App Group it's entitled to, or a launchd job for it.
    High,
}

/// One leftover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leftover {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    pub confidence: Confidence,
    /// Why it matched, in plain words.
    pub reason: String,
    pub bytes: u64,
    /// In `/Library`: reported only, an administrator has to remove it.
    pub needs_admin: bool,
    /// Ticked in the review window by default (Medium or High, and not needing admin).
    pub default_selected: bool,
}

/// Something that matched but is kept because another app uses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptItem {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    pub reason: SkipReason,
}

/// What [`find_leftovers`] found.
#[derive(Debug, Clone, Serialize)]
pub struct LeftoverReport {
    pub target: AppTarget,
    pub leftovers: Vec<Leftover>,
    pub kept: Vec<KeptItem>,
    /// Total of the leftovers in the home folder (what Kipimo can move).
    pub total_bytes: u64,
    /// Another copy of the same app is installed here, so everything is kept.
    pub other_copy: Option<PathBuf>,
    /// The app (or a copy of it) is running: quit it before removing leftovers.
    pub target_running: bool,
    pub cancelled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Bundle ID or exact app name.
    Folder,
    /// Bundle ID, optionally team-prefixed.
    IdOnly,
    Prefs,
    ByHost,
    SavedState,
    Cookies,
    HttpStorage,
    GroupContainers,
    Launchd,
    Helper,
}

struct Location {
    rel: &'static str,
    kind: LeftoverKind,
    system: bool,
    style: Style,
}

const LOCATIONS: &[Location] = &[
    Location {
        rel: "Application Support",
        kind: LeftoverKind::ApplicationSupport,
        system: false,
        style: Style::Folder,
    },
    Location {
        rel: "Application Scripts",
        kind: LeftoverKind::ApplicationScripts,
        system: false,
        style: Style::IdOnly,
    },
    Location {
        rel: "Caches",
        kind: LeftoverKind::Caches,
        system: false,
        style: Style::Folder,
    },
    Location {
        rel: "Containers",
        kind: LeftoverKind::Container,
        system: false,
        style: Style::IdOnly,
    },
    Location {
        rel: "Group Containers",
        kind: LeftoverKind::GroupContainer,
        system: false,
        style: Style::GroupContainers,
    },
    Location {
        rel: "Preferences",
        kind: LeftoverKind::Preferences,
        system: false,
        style: Style::Prefs,
    },
    Location {
        rel: "Preferences/ByHost",
        kind: LeftoverKind::ByHostPreferences,
        system: false,
        style: Style::ByHost,
    },
    Location {
        rel: "Saved Application State",
        kind: LeftoverKind::SavedState,
        system: false,
        style: Style::SavedState,
    },
    Location {
        rel: "HTTPStorages",
        kind: LeftoverKind::HttpStorage,
        system: false,
        style: Style::HttpStorage,
    },
    Location {
        rel: "WebKit",
        kind: LeftoverKind::WebKit,
        system: false,
        style: Style::IdOnly,
    },
    Location {
        rel: "Logs",
        kind: LeftoverKind::Logs,
        system: false,
        style: Style::Folder,
    },
    Location {
        rel: "Cookies",
        kind: LeftoverKind::Cookies,
        system: false,
        style: Style::Cookies,
    },
    Location {
        rel: "LaunchAgents",
        kind: LeftoverKind::LaunchAgent,
        system: false,
        style: Style::Launchd,
    },
    Location {
        rel: "Application Support",
        kind: LeftoverKind::ApplicationSupport,
        system: true,
        style: Style::Folder,
    },
    Location {
        rel: "Caches",
        kind: LeftoverKind::Caches,
        system: true,
        style: Style::Folder,
    },
    Location {
        rel: "Preferences",
        kind: LeftoverKind::Preferences,
        system: true,
        style: Style::Prefs,
    },
    Location {
        rel: "Logs",
        kind: LeftoverKind::Logs,
        system: true,
        style: Style::Folder,
    },
    Location {
        rel: "LaunchAgents",
        kind: LeftoverKind::LaunchAgent,
        system: true,
        style: Style::Launchd,
    },
    Location {
        rel: "LaunchDaemons",
        kind: LeftoverKind::LaunchDaemon,
        system: true,
        style: Style::Launchd,
    },
    Location {
        rel: "PrivilegedHelperTools",
        kind: LeftoverKind::PrivilegedHelper,
        system: true,
        style: Style::Helper,
    },
];

/// Names too generic to match on their own.
const GENERIC_NAMES: &[&str] = &[
    "agent",
    "app",
    "apple",
    "cache",
    "caches",
    "common",
    "data",
    "default",
    "google",
    "helper",
    "launcher",
    "logs",
    "microsoft",
    "service",
    "settings",
    "setup",
    "shared",
    "temp",
    "update",
    "updater",
];

// ---------------------------------------------------------------------------------------------
// Matching helpers

pub(crate) fn eq_ic(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// `name` is `id` or `id.<more>`.
pub(crate) fn id_or_child(name: &str, id: &str) -> Option<bool> {
    if eq_ic(name, id) {
        return Some(true);
    }
    let (n, i) = (name.as_bytes(), id.as_bytes());
    (n.len() > i.len() + 1 && n[..i.len()].eq_ignore_ascii_case(i) && n[i.len()] == b'.')
        .then_some(false)
}

/// The most specific of `ids` that `name` is, or is a child of. Returns (id, exact).
fn best_id<'a>(name: &str, ids: &[&'a str]) -> Option<(&'a str, bool)> {
    let mut best: Option<(&str, bool)> = None;
    for id in ids {
        if let Some(exact) = id_or_child(name, id) {
            match best {
                Some((b, bexact))
                    if (bexact && !exact) || (bexact == exact && b.len() >= id.len()) => {}
                _ => best = Some((id, exact)),
            }
        }
    }
    best
}

/// `ABCDE12345.rest` → `rest` (a 10-character uppercase team ID prefix).
fn strip_team(name: &str) -> Option<&str> {
    let (team, rest) = name.split_once('.')?;
    (team.len() == 10
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && !rest.is_empty())
    .then_some(rest)
}

fn team_of(name: &str) -> Option<&str> {
    name.split_once('.')
        .map(|(t, _)| t)
        .filter(|_| strip_team(name).is_some())
}

/// `<id>.<host UUID or 12 hex digits>` → `<id>`.
fn strip_host(core: &str) -> &str {
    if let Some((head, tail)) = core.rsplit_once('.') {
        let uuid = tail.len() == 36 && tail.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        let hex = tail.len() == 12 && tail.bytes().all(|b| b.is_ascii_hexdigit());
        if uuid || hex {
            return head;
        }
    }
    core
}

fn name_variants(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let n = n.trim();
        for v in [
            n.to_owned(),
            n.replace(' ', ""),
            n.replace(' ', "_"),
            n.replace(' ', "-"),
        ] {
            if v.chars().count() >= 4
                && !GENERIC_NAMES.iter().any(|g| eq_ic(g, &v))
                && !out.iter().any(|o| eq_ic(o, &v))
            {
                out.push(v);
            }
        }
    }
    out
}

fn exact_name(entry: &str, variants: &[String]) -> Option<(String, bool)> {
    variants
        .iter()
        .find(|v| eq_ic(entry, v))
        .map(|v| (v.clone(), true))
}

/// Exact name (`Some(true)`) or name plus a separator and more (`Some(false)`).
fn name_hit(entry: &str, variants: &[String]) -> Option<(String, bool)> {
    let mut prefix_hit = None;
    for v in variants {
        if eq_ic(entry, v) {
            return Some((v.clone(), true));
        }
        let (e, vb) = (entry.as_bytes(), v.as_bytes());
        if v.chars().count() >= 5
            && e.len() > vb.len() + 1
            && e[..vb.len()].eq_ignore_ascii_case(vb)
            && matches!(e[vb.len()], b' ' | b'_' | b'-' | b'.')
            && prefix_hit.is_none()
        {
            prefix_hit = Some((v.clone(), false));
        }
    }
    prefix_hit
}

fn other_ids(o: &IndexedApp) -> Vec<&str> {
    o.bundle_id
        .iter()
        .map(String::as_str)
        .chain(o.related_ids.iter().map(String::as_str))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The finder

struct Candidate {
    path: PathBuf,
    kind: LeftoverKind,
    system: bool,
    confidence: Confidence,
    reason: String,
    kept_by: Option<String>,
}

struct Ctx<'a> {
    target: &'a AppTarget,
    ids: Vec<&'a str>,
    /// Every name of the app, for exact matches.
    variants: Vec<String>,
    /// Only the primary (display) name, for `<Name>_suffix` matches.
    prefix_variants: Vec<String>,
    others: Vec<&'a IndexedApp>,
}

impl Ctx<'_> {
    /// Another app whose own ID is `core`, or a more specific owner of it than `matched`.
    fn id_owner(&self, core: &str, matched: &str) -> Option<String> {
        for o in &self.others {
            for oid in other_ids(o) {
                if let Some(exact) = id_or_child(core, oid)
                    && (exact || oid.len() > matched.len())
                {
                    return Some(o.name.clone());
                }
            }
        }
        None
    }

    fn name_owner(&self, entry: &str, matched: &str, exact: bool) -> Option<String> {
        for o in &self.others {
            let vs = name_variants(&o.names);
            if let Some((v, oexact)) = name_hit(entry, &vs)
                && (oexact || (!exact && v.len() > matched.len()) || eq_ic(&v, matched))
            {
                return Some(o.name.clone());
            }
        }
        None
    }

    fn match_id(&self, core: &str) -> Option<(Confidence, String, Option<String>)> {
        let (id, exact) = best_id(core, &self.ids)?;
        let reason = if exact {
            format!("named after {id}")
        } else {
            format!("in the {id} namespace")
        };
        Some((Confidence::High, reason, self.id_owner(core, id)))
    }

    fn match_entry(
        &self,
        loc: &Location,
        dir: &Path,
        name: &str,
    ) -> Option<(Confidence, String, Option<String>)> {
        match loc.style {
            Style::Folder => self.match_id(name).or_else(|| {
                let (v, exact) = exact_name(name, &self.variants)
                    .or_else(|| name_hit(name, &self.prefix_variants))?;
                let owner = self.name_owner(name, &v, exact);
                Some(if exact {
                    (
                        Confidence::Medium,
                        format!("named like the app ({v})"),
                        owner,
                    )
                } else {
                    (
                        Confidence::Low,
                        format!("starts with the app's name ({v})"),
                        owner,
                    )
                })
            }),
            Style::IdOnly => self
                .match_id(name)
                .or_else(|| self.match_id(strip_team(name)?)),
            Style::Prefs => self.match_id(name.strip_suffix(".plist")?),
            Style::ByHost => self.match_id(strip_host(name.strip_suffix(".plist")?)),
            Style::SavedState => self.match_id(name.strip_suffix(".savedState")?),
            Style::Cookies => self.match_id(name.strip_suffix(".binarycookies")?),
            Style::HttpStorage => {
                self.match_id(name.strip_suffix(".binarycookies").unwrap_or(name))
            }
            Style::Helper => self.match_id(name),
            Style::GroupContainers => self.match_group(name),
            Style::Launchd => self.match_launchd(&dir.join(name), name),
        }
    }

    fn match_group(&self, name: &str) -> Option<(Confidence, String, Option<String>)> {
        let entitled = self.target.app_groups.iter().any(|g| eq_ic(g, name));
        let core = strip_team(name)
            .or_else(|| name.strip_prefix("group."))
            .unwrap_or(name);
        let by_id = best_id(core, &self.ids);
        if !entitled && by_id.is_none() {
            return None;
        }
        // Another installed app entitled to the same group still uses it.
        let mut owner = self
            .others
            .iter()
            .find(|o| o.app_groups.iter().any(|g| eq_ic(g, name)))
            .map(|o| o.name.clone());
        if owner.is_none()
            && let Some((id, _)) = by_id
        {
            owner = self.id_owner(core, id);
        }
        if owner.is_none() && by_id.is_none() {
            // Only the entitlement ties it to this app: if another app from the same developer is
            // installed, it may share the group even if we couldn't read its entitlements.
            let team = team_of(name)
                .map(str::to_owned)
                .or(self.target.team_id.clone());
            let target_vendor = vendor(&self.target.bundle_id);
            owner = self
                .others
                .iter()
                .find(|o| {
                    (team.is_some() && o.team_id == team)
                        || o.bundle_id
                            .as_deref()
                            .is_some_and(|b| vendor(b) == target_vendor)
                })
                .map(|o| o.name.clone());
        }
        let reason = if entitled {
            "an App Group the app is entitled to".to_owned()
        } else {
            format!(
                "App Group named after {}",
                by_id.map(|(i, _)| i).unwrap_or_default()
            )
        };
        Some((Confidence::High, reason, owner))
    }

    fn match_launchd(
        &self,
        path: &Path,
        name: &str,
    ) -> Option<(Confidence, String, Option<String>)> {
        let stem = name.strip_suffix(".plist")?;
        let p = plist::read(path);
        let label = p
            .as_ref()
            .and_then(|p| p.str("Label"))
            .unwrap_or(stem)
            .to_owned();
        let program = p.as_ref().and_then(|p| {
            p.str("Program").map(str::to_owned).or_else(|| {
                p.get("ProgramArguments")
                    .and_then(|a| a.as_array()?.first()?.as_str().map(str::to_owned))
            })
        });
        let associated: Vec<String> = p
            .as_ref()
            .and_then(|p| p.get("AssociatedBundleIdentifiers"))
            .map(|a| a.strings().into_iter().map(str::to_owned).collect())
            .unwrap_or_default();

        // Which installed app does the job launch?
        let program_owner = program.as_deref().and_then(|prog| {
            self.others
                .iter()
                .find(|o| Path::new(prog).starts_with(&o.path))
                .map(|o| o.name.clone())
        });
        let in_bundle = program
            .as_deref()
            .zip(self.target.bundle_path.as_deref())
            .is_some_and(|(prog, b)| Path::new(prog).starts_with(b));

        let (reason, matched) = if let Some((id, _)) = best_id(&label, &self.ids) {
            (format!("launchd job {label}"), Some((label.clone(), id)))
        } else if let Some((id, _)) = best_id(stem, &self.ids) {
            (
                format!("launchd job file named after {id}"),
                Some((stem.to_owned(), id)),
            )
        } else if associated
            .iter()
            .any(|a| self.ids.iter().any(|i| eq_ic(a, i)))
        {
            ("launchd job associated with the app".to_owned(), None)
        } else if in_bundle {
            ("launchd job that starts the app".to_owned(), None)
        } else {
            return None;
        };
        let owner =
            program_owner.or_else(|| matched.and_then(|(core, id)| self.id_owner(&core, id)));
        Some((Confidence::High, reason, owner))
    }
}

fn stays_inside(dir: &Path, base: &Path) -> bool {
    let is_link = fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink());
    match (fs::canonicalize(dir), fs::canonicalize(base)) {
        (Ok(d), Ok(b)) => !is_link && d.starts_with(b),
        _ => false,
    }
}

/// Finds an app's leftovers. `index` lists the installed apps (see [`AppIndex::build`]); the
/// target's own bundle is ignored in it. Sizes are measured in parallel.
pub fn find_leftovers(
    env: &Env,
    target: &AppTarget,
    index: &AppIndex,
    cancel: &AtomicBool,
) -> LeftoverReport {
    find(env, target, index, cancel, true)
}

/// Builds the index and finds the leftovers of an app bundle (installed or already in the
/// Trash). `None` if the bundle has no bundle ID.
pub fn find_leftovers_for_bundle(
    env: &Env,
    bundle: &Path,
    cancel: &AtomicBool,
) -> Option<LeftoverReport> {
    let target = AppTarget::from_bundle(env, bundle)?;
    let index = AppIndex::build(env, cancel);
    Some(find_leftovers(env, &target, &index, cancel))
}

pub(crate) fn find(
    env: &Env,
    target: &AppTarget,
    index: &AppIndex,
    cancel: &AtomicBool,
    measure: bool,
) -> LeftoverReport {
    let mut ids: Vec<&str> = vec![target.bundle_id.as_str()];
    ids.extend(target.related_ids.iter().map(String::as_str));
    let others = index.others(target.bundle_path.as_deref());
    let other_copy = others
        .iter()
        .find(|o| {
            o.bundle_id
                .as_deref()
                .is_some_and(|b| eq_ic(b, &target.bundle_id))
        })
        .map(|o| (o.path.clone(), o.name.clone()));
    let ctx = Ctx {
        target,
        ids,
        variants: name_variants(&target.names),
        prefix_variants: name_variants(&target.names[..target.names.len().min(1)]),
        others,
    };

    let mut candidates: Vec<Candidate> = Vec::new();
    for loc in LOCATIONS {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let dir = if loc.system {
            env.system_library(loc.rel)
        } else {
            env.library(loc.rel)
        };
        // A location that's a symlink (or resolves outside its root) is never followed.
        let base = if loc.system {
            &env.system_root
        } else {
            &env.home
        };
        if !stays_inside(&dir, base) {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((confidence, reason, owner)) = ctx.match_entry(loc, &dir, &name) else {
                continue;
            };
            // A symlinked entry is never followed or offered.
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            let kept_by = match (&other_copy, owner) {
                (Some((_, copy_name)), _) => Some(copy_name.clone()),
                (None, owner) => owner,
            };
            if is_link {
                continue;
            }
            candidates.push(Candidate {
                path: dir.join(&name),
                kind: loc.kind,
                system: loc.system,
                confidence,
                reason,
                kept_by,
            });
        }
    }

    let (keep, offer): (Vec<Candidate>, Vec<Candidate>) =
        candidates.into_iter().partition(|c| c.kept_by.is_some());
    let paths: Vec<PathBuf> = offer.iter().map(|c| c.path.clone()).collect();
    let mut sizes = vec![0u64; paths.len()];
    let mut cancelled = cancel.load(Ordering::Relaxed);
    if measure && !paths.is_empty() {
        let out = walk::measure(&paths, env.threads, cancel, &mut |_| {});
        cancelled |= out.cancelled;
        sizes = out.totals.iter().map(|t| t.bytes).collect();
    }
    let leftovers: Vec<Leftover> = offer
        .into_iter()
        .zip(sizes)
        .map(|(c, bytes)| Leftover {
            default_selected: !c.system
                && c.confidence >= Confidence::Medium
                && !holds_user_data(c.kind),
            needs_admin: c.system,
            path: c.path,
            kind: c.kind,
            confidence: c.confidence,
            reason: c.reason,
            bytes,
        })
        .collect();
    let kept = keep
        .into_iter()
        .map(|c| KeptItem {
            path: c.path,
            kind: c.kind,
            reason: SkipReason::Shared {
                app: c.kept_by.unwrap_or_default(),
            },
        })
        .collect();
    let running = env.running.snapshot();
    LeftoverReport {
        total_bytes: leftovers
            .iter()
            .filter(|l| !l.needs_admin)
            .map(|l| l.bytes)
            .sum(),
        target_running: running.iter().any(|r| {
            r.bundle_id
                .as_deref()
                .is_some_and(|b| eq_ic(b, &target.bundle_id))
        }),
        target: target.clone(),
        leftovers,
        kept,
        other_copy: other_copy.map(|(p, _)| p),
        cancelled,
    }
}

/// Moves the selected leftovers to the Trash. Each one is re-checked against a fresh finder run:
/// anything the finder doesn't offer now (or that needs admin, or is kept for another app) is
/// skipped with the reason. Nothing outside `selected` is touched.
pub fn remove_leftovers(
    env: &Env,
    target: &AppTarget,
    selected: &[Leftover],
    progress: &mut dyn FnMut(usize, usize),
    cancel: &AtomicBool,
) -> Vec<ItemResult> {
    let index = AppIndex::build(env, cancel);
    let fresh = find(env, target, &index, cancel, false);
    trash_validated(env, &fresh, selected, progress, cancel)
}

pub(crate) fn trash_validated(
    env: &Env,
    fresh: &LeftoverReport,
    selected: &[Leftover],
    progress: &mut dyn FnMut(usize, usize),
    cancel: &AtomicBool,
) -> Vec<ItemResult> {
    let offered: HashMap<&Path, &Leftover> = fresh
        .leftovers
        .iter()
        .map(|l| (l.path.as_path(), l))
        .collect();
    let kept: HashMap<&Path, &KeptItem> =
        fresh.kept.iter().map(|k| (k.path.as_path(), k)).collect();
    let mut results = Vec::with_capacity(selected.len());
    for (i, item) in selected.iter().enumerate() {
        progress(i, selected.len());
        let path = item.path.clone();
        let result = if cancel.load(Ordering::Relaxed) {
            ItemResult::skipped(path, item.bytes, SkipReason::Cancelled)
        } else if let Some(k) = kept.get(path.as_path()) {
            ItemResult::skipped(path, item.bytes, k.reason.clone())
        } else if let Some(found) = offered.get(path.as_path()) {
            if found.needs_admin {
                ItemResult::skipped(path, item.bytes, SkipReason::NeedsAdmin)
            } else if fresh.target_running {
                ItemResult::skipped(
                    path,
                    item.bytes,
                    SkipReason::InUse {
                        app: fresh
                            .target
                            .names
                            .first()
                            .cloned()
                            .unwrap_or_else(|| fresh.target.bundle_id.clone()),
                    },
                )
            } else {
                trash::move_to_trash(env, &path, item.bytes, Zone::Home)
            }
        } else {
            ItemResult::skipped(path, item.bytes, SkipReason::NotACandidate)
        };
        results.push(result);
    }
    progress(selected.len(), selected.len());
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_matching_is_exact_or_namespaced() {
        assert_eq!(id_or_child("com.foo.App", "com.foo.app"), Some(true));
        assert_eq!(
            id_or_child("com.foo.app.helper", "com.foo.app"),
            Some(false)
        );
        assert_eq!(id_or_child("com.foo.apple", "com.foo.app"), None);
        assert_eq!(id_or_child("com.foo.app.", "com.foo.app"), None);
        assert_eq!(id_or_child("xcom.foo.app", "com.foo.app"), None);
        assert_eq!(
            best_id("com.foo.app.helper", &["com.foo.app", "com.foo.app.helper"]),
            Some(("com.foo.app.helper", true))
        );
    }

    #[test]
    fn wrappers() {
        assert_eq!(
            strip_team("ABCDE12345.com.example.Notes"),
            Some("com.example.Notes")
        );
        assert_eq!(strip_team("group.com.foo"), None);
        assert_eq!(strip_team("com.foo.bar"), None);
        assert_eq!(
            strip_host("com.foo.app.0123ABCD-0123-4567-89AB-0123456789AB"),
            "com.foo.app"
        );
        assert_eq!(strip_host("com.foo.app.001122aabbcc"), "com.foo.app");
        assert_eq!(strip_host("com.foo.app.settings"), "com.foo.app.settings");
    }

    #[test]
    fn names_are_never_substrings() {
        let v = name_variants(&["Clean Up".to_owned(), "App".to_owned()]);
        assert!(
            v.iter().any(|s| s == "Clean Up")
                && v.iter().any(|s| s == "CleanUp")
                && v.iter().any(|s| s == "Clean_Up")
        );
        assert!(
            !v.iter().any(|s| s == "App"),
            "generic or short names are dropped"
        );
        assert_eq!(
            name_hit("clean up", &v),
            Some(("Clean Up".to_owned(), true))
        );
        assert_eq!(
            name_hit("Clean_Up_Helper", &v),
            Some(("Clean_Up".to_owned(), false))
        );
        assert_eq!(name_hit("My Clean Up", &v), None);
        assert_eq!(name_hit("CleanUpper", &v), None);
    }
}
