//! Acting on startup items the user selected. Nothing here runs with elevated rights:
//! user LaunchAgents are unloaded and trashed directly; system-wide items come back as the
//! exact commands the owner can run in Terminal; login items and extensions point to the
//! System Settings pane that manages them.

use std::path::{Path, PathBuf};

use objc2_core_foundation::{CFRetained, CFURL};
use serde::{Deserialize, Serialize};

use crate::inventory::{Roots, home_dir};
use crate::launchd;
use crate::launchd_state;
use crate::model::{ItemKind, Scope, StartupItem};
use crate::trash::{SystemTrash, Trasher};

/// System Settings > General > Login Items & Extensions (macOS 13+).
pub const LOGIN_ITEMS_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

/// One shell command for the owner to run in Terminal, with what it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCommand {
    pub command: String,
    pub explanation: String,
}

/// What happened to one selected item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionOutcome {
    /// Unloaded from launchd (when it was loaded) and its plist moved to the Trash.
    Disabled {
        unloaded: bool,
        /// Where the plist is now (inside the Trash), when the system reported it.
        trashed_to: Option<PathBuf>,
    },
    /// System-wide: needs an administrator. Kipimo doesn't escalate; run one of these sets in
    /// Terminal.
    NeedsAdmin {
        /// Stop it now and keep it from starting again, without moving any file (reversible).
        disable: Vec<AdminCommand>,
        /// Stop it now and move its files to the Trash.
        remove: Vec<AdminCommand>,
    },
    /// Managed by macOS in System Settings: open this pane.
    OpenSettings { url: String, hint: String },
    /// Not acted on, and why.
    Skipped { reason: String },
    /// Tried and failed.
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub id: String,
    pub name: String,
    pub outcome: ActionOutcome,
}

/// launchd operations on the user's own domain, behind a trait for tests.
pub(crate) trait UserLaunchd {
    fn loaded(&self, label: &str) -> bool;
    fn remove(&self, label: &str) -> Result<(), String>;
}

struct SystemLaunchd;

impl UserLaunchd for SystemLaunchd {
    fn loaded(&self, label: &str) -> bool {
        launchd_state::user_job_loaded(label)
    }
    fn remove(&self, label: &str) -> Result<(), String> {
        launchd_state::remove_user_job(label)
    }
}

/// Disables the selected items, one result per item, in order. Only the items passed in are
/// considered; each is re-checked against the disk first.
///
/// - User LaunchAgent (`~/Library/LaunchAgents`): removed from launchd for this session
///   (`SMJobRemove`), then its plist goes to the Trash (Put Back works).
/// - System LaunchAgent / LaunchDaemon (`/Library/...`): [`ActionOutcome::NeedsAdmin`] with
///   the commands to run.
/// - Login items, background items and system extensions:
///   [`ActionOutcome::OpenSettings`].
/// - Kernel extensions: skipped (remove them with their app's uninstaller).
pub fn disable_items(items: &[StartupItem]) -> Vec<ActionResult> {
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    disable_with(
        items,
        &Roots::system(),
        uid,
        &home_dir().join(".Trash"),
        &SystemLaunchd,
        &SystemTrash,
    )
}

pub(crate) fn disable_with(
    items: &[StartupItem],
    roots: &Roots,
    uid: u32,
    user_trash: &Path,
    launchd: &dyn UserLaunchd,
    trasher: &dyn Trasher,
) -> Vec<ActionResult> {
    items
        .iter()
        .map(|item| ActionResult {
            id: item.id.clone(),
            name: item.name.clone(),
            outcome: disable_one(item, roots, uid, user_trash, launchd, trasher),
        })
        .collect()
}

fn disable_one(
    item: &StartupItem,
    roots: &Roots,
    uid: u32,
    user_trash: &Path,
    launchd: &dyn UserLaunchd,
    trasher: &dyn Trasher,
) -> ActionOutcome {
    match (item.kind, item.scope) {
        (ItemKind::LaunchAgent, Scope::User) => disable_user_agent(item, roots, launchd, trasher),
        (ItemKind::LaunchAgent | ItemKind::LaunchDaemon, Scope::System) => admin_plan(item, uid, user_trash),
        (ItemKind::LaunchDaemon, Scope::User) => ActionOutcome::Skipped {
            reason: "launch daemons are always system-wide".into(),
        },
        (ItemKind::LoginItem, _) => ActionOutcome::OpenSettings {
            url: LOGIN_ITEMS_SETTINGS_URL.into(),
            hint: format!("Remove {} from \"Open at Login\".", item.name),
        },
        (ItemKind::BackgroundItem, _) => ActionOutcome::OpenSettings {
            url: LOGIN_ITEMS_SETTINGS_URL.into(),
            hint: format!(
                "Turn off {} under \"Allow in the Background\".",
                item.owner.as_ref().map(|o| o.name.as_str()).unwrap_or(&item.name)
            ),
        },
        (ItemKind::SystemExtension, _) => ActionOutcome::OpenSettings {
            url: LOGIN_ITEMS_SETTINGS_URL.into(),
            hint: match &item.owner {
                Some(o) => format!(
                    "Turn it off under Extensions, or remove it by moving {} to the Trash (macOS asks to remove its extension).",
                    o.name
                ),
                None => "Turn it off under Extensions.".into(),
            },
        },
        (ItemKind::KernelExtension, _) => ActionOutcome::Skipped {
            reason: "kernel extensions are removed with their app's uninstaller; Kipimo doesn't unload kernel code".into(),
        },
    }
}

fn disable_user_agent(
    item: &StartupItem,
    roots: &Roots,
    launchd: &dyn UserLaunchd,
    trasher: &dyn Trasher,
) -> ActionOutcome {
    let Some(plist) = item.plist_path.as_deref() else {
        return ActionOutcome::Skipped {
            reason: "no plist path".into(),
        };
    };
    // Only ever touch a file directly inside the user's LaunchAgents folder.
    let inside = plist
        .parent()
        .zip(std::fs::canonicalize(&roots.user_agents).ok())
        .and_then(|(parent, agents)| std::fs::canonicalize(parent).ok().map(|p| p == agents))
        .unwrap_or(false);
    if !inside {
        return ActionOutcome::Skipped {
            reason: format!(
                "{} is not in {}",
                plist.display(),
                roots.user_agents.display()
            ),
        };
    }
    if std::fs::symlink_metadata(plist).is_err() {
        return ActionOutcome::Skipped {
            reason: "the plist is already gone".into(),
        };
    }
    // Re-read: if the file now describes a different job, the selection is stale.
    let now = launchd::read_job(plist);
    if now.label.is_some() && now.label.as_deref() != Some(item.label.as_str()) {
        return ActionOutcome::Skipped {
            reason: "the plist changed since the scan; scan again".into(),
        };
    }
    let mut unloaded = false;
    if let Some(label) = now.label.as_deref()
        && launchd.loaded(label)
    {
        if let Err(e) = launchd.remove(label) {
            return ActionOutcome::Failed {
                error: format!("couldn't unload {label}: {e}"),
            };
        }
        unloaded = true;
    }
    match trasher.trash(plist) {
        Ok(trashed_to) => ActionOutcome::Disabled {
            unloaded,
            trashed_to,
        },
        Err(e) => ActionOutcome::Failed {
            error: if unloaded {
                format!("unloaded, but couldn't move the plist to the Trash: {e}")
            } else {
                format!("couldn't move the plist to the Trash: {e}")
            },
        },
    }
}

/// Single-quotes a path for zsh/bash.
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `~/.Trash/<name>`, renamed when that name is already taken (mv -n also refuses to
/// overwrite, so nothing in the Trash is ever replaced).
fn trash_destination(user_trash: &Path, source: &Path) -> String {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "item".into());
    let mut candidate = name.clone();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut n = 2;
    while std::fs::symlink_metadata(user_trash.join(&candidate)).is_ok() && n < 100 {
        candidate = format!("{stem} {n}{ext}");
        n += 1;
    }
    format!("~/.Trash/{}", shell_quote(&candidate))
}

/// The admin commands for a system-wide agent or daemon.
pub(crate) fn admin_plan(item: &StartupItem, uid: u32, user_trash: &Path) -> ActionOutcome {
    let Some(plist) = item.plist_path.as_deref() else {
        return ActionOutcome::Skipped {
            reason: "no plist path".into(),
        };
    };
    let plist_q = shell_quote(&plist.to_string_lossy());
    let has_label = !item.flags.contains(&crate::model::Flag::MalformedPlist);
    let (domain, sudo) = match item.kind {
        ItemKind::LaunchDaemon => ("system".to_string(), "sudo "),
        _ => (format!("gui/{uid}"), ""),
    };
    let target = if has_label {
        format!("{domain}/{}", shell_quote(&item.label))
    } else {
        format!("{domain} {plist_q}")
    };
    let stop = AdminCommand {
        command: format!("{sudo}launchctl bootout {target}"),
        explanation: match item.kind {
            ItemKind::LaunchDaemon => "Stops it now.".into(),
            _ => "Stops it now in your session (no password needed).".into(),
        },
    };
    let mut disable = vec![stop.clone()];
    if has_label {
        let service = format!("{domain}/{}", shell_quote(&item.label));
        disable.push(AdminCommand {
            command: format!("{sudo}launchctl disable {service}"),
            explanation: format!(
                "Keeps it from starting again; the file stays where it is. Undo with: {sudo}launchctl enable {service}"
            ),
        });
    }
    let mut remove = vec![
        stop,
        AdminCommand {
            command: format!(
                "sudo mv -n {plist_q} {}",
                trash_destination(user_trash, plist)
            ),
            explanation: "Moves the plist to your Trash (drag it back to restore).".into(),
        },
    ];
    if item.kind == ItemKind::LaunchDaemon
        && let Some(program) = item.program.as_deref()
        && program.starts_with("/Library/PrivilegedHelperTools/")
        && std::fs::symlink_metadata(program).is_ok()
    {
        remove.push(AdminCommand {
            command: format!(
                "sudo mv -n {} {}",
                shell_quote(&program.to_string_lossy()),
                trash_destination(user_trash, program)
            ),
            explanation: "Moves the privileged helper it runs to your Trash.".into(),
        });
    }
    ActionOutcome::NeedsAdmin { disable, remove }
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    /// LaunchServices (LSOpen.h): opens a URL with its handler.
    fn LSOpenCFURLRef(url: &CFURL, out_launched: *mut *const CFURL) -> i32;
}

/// Opens a System Settings pane (or any URL) through LaunchServices, for
/// [`ActionOutcome::OpenSettings`]. The UI may use `NSWorkspace` instead; this keeps the
/// engine usable without AppKit.
pub fn open_settings(url: &str) -> Result<(), String> {
    if !url.starts_with("x-apple.systempreferences:") {
        return Err("only System Settings URLs are opened".into());
    }
    let cf_url: CFRetained<CFURL> =
        CFURL::from_string(None, &objc2_core_foundation::CFString::from_str(url), None)
            .ok_or("invalid URL")?;
    // SAFETY: valid CFURL; NULL out-parameter is allowed.
    let status = unsafe { LSOpenCFURLRef(&cf_url, std::ptr::null_mut()) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("LaunchServices error {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::testutil::TempDir;
    use crate::trash::fake::FolderTrash;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeLaunchd {
        loaded: Vec<String>,
        removed: RefCell<Vec<String>>,
        fail: bool,
    }

    impl UserLaunchd for FakeLaunchd {
        fn loaded(&self, label: &str) -> bool {
            self.loaded.iter().any(|l| l == label)
                && !self.removed.borrow().iter().any(|l| l == label)
        }
        fn remove(&self, label: &str) -> Result<(), String> {
            if self.fail {
                return Err("refused".into());
            }
            self.removed.borrow_mut().push(label.into());
            Ok(())
        }
    }

    fn item(kind: ItemKind, scope: Scope, label: &str, plist: Option<PathBuf>) -> StartupItem {
        StartupItem {
            id: format!("test:{label}"),
            kind,
            scope,
            label: label.into(),
            name: label.into(),
            plist_path: plist,
            program: None,
            arguments: Vec::new(),
            script: None,
            owner: None,
            developer: None,
            state: ItemState::default(),
            signature: None,
            flags: Vec::new(),
            btm_identifier: None,
        }
    }

    fn agent_plist(dir: &Path, label: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{label}.plist"));
        std::fs::write(
            &path,
            format!(
                r#"<plist version="1.0"><dict><key>Label</key><string>{label}</string><key>Program</key><string>/bin/true</string></dict></plist>"#
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn user_agent_is_unloaded_then_trashed() {
        let tmp = TempDir::new("disable");
        let roots = Roots::synthetic(tmp.path());
        let plist = agent_plist(&roots.user_agents, "com.example.loaded");
        let idle = agent_plist(&roots.user_agents, "com.example.idle");
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        let launchd = FakeLaunchd {
            loaded: vec!["com.example.loaded".into()],
            ..FakeLaunchd::default()
        };
        let items = vec![
            item(
                ItemKind::LaunchAgent,
                Scope::User,
                "com.example.loaded",
                Some(plist.clone()),
            ),
            item(
                ItemKind::LaunchAgent,
                Scope::User,
                "com.example.idle",
                Some(idle.clone()),
            ),
        ];
        let results = disable_with(
            &items,
            &roots,
            501,
            &tmp.path().join("Trash"),
            &launchd,
            &trash,
        );
        assert_eq!(
            results[0].outcome,
            ActionOutcome::Disabled {
                unloaded: true,
                trashed_to: Some(tmp.path().join("Trash/com.example.loaded.plist"))
            }
        );
        assert!(matches!(
            results[1].outcome,
            ActionOutcome::Disabled {
                unloaded: false,
                ..
            }
        ));
        assert_eq!(
            *launchd.removed.borrow(),
            vec!["com.example.loaded".to_string()]
        );
        assert!(!plist.exists() && !idle.exists());
        // Nothing else was touched.
        assert_eq!(trash.moved.borrow().len(), 2);
    }

    #[test]
    fn refuses_paths_outside_user_agents_and_stale_items() {
        let tmp = TempDir::new("refuse");
        let roots = Roots::synthetic(tmp.path());
        std::fs::create_dir_all(&roots.user_agents).unwrap();
        let elsewhere = agent_plist(&tmp.path().join("elsewhere"), "com.example.elsewhere");
        let changed = agent_plist(&roots.user_agents, "com.example.changed");
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        let launchd = FakeLaunchd::default();
        let items = vec![
            item(
                ItemKind::LaunchAgent,
                Scope::User,
                "com.example.elsewhere",
                Some(elsewhere.clone()),
            ),
            // Scanned as a different label than the file now holds.
            item(
                ItemKind::LaunchAgent,
                Scope::User,
                "com.example.original",
                Some(changed.clone()),
            ),
            item(
                ItemKind::LaunchAgent,
                Scope::User,
                "com.example.gone",
                Some(roots.user_agents.join("com.example.gone.plist")),
            ),
        ];
        let results = disable_with(
            &items,
            &roots,
            501,
            &tmp.path().join("Trash"),
            &launchd,
            &trash,
        );
        for r in &results {
            assert!(matches!(r.outcome, ActionOutcome::Skipped { .. }), "{r:?}");
        }
        assert!(elsewhere.exists() && changed.exists());
        assert!(trash.moved.borrow().is_empty());
    }

    #[test]
    fn unload_failure_keeps_the_plist() {
        let tmp = TempDir::new("unload-fail");
        let roots = Roots::synthetic(tmp.path());
        let plist = agent_plist(&roots.user_agents, "com.example.stuck");
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        let launchd = FakeLaunchd {
            loaded: vec!["com.example.stuck".into()],
            fail: true,
            ..FakeLaunchd::default()
        };
        let items = vec![item(
            ItemKind::LaunchAgent,
            Scope::User,
            "com.example.stuck",
            Some(plist.clone()),
        )];
        let results = disable_with(
            &items,
            &roots,
            501,
            &tmp.path().join("Trash"),
            &launchd,
            &trash,
        );
        assert!(matches!(results[0].outcome, ActionOutcome::Failed { .. }));
        assert!(plist.exists());
    }

    #[test]
    fn symlinked_plist_moves_the_link_only() {
        let tmp = TempDir::new("symlink-plist");
        let roots = Roots::synthetic(tmp.path());
        std::fs::create_dir_all(&roots.user_agents).unwrap();
        let target = agent_plist(
            &tmp.path().join("App.app/Contents/Library"),
            "com.example.linked",
        );
        let link = roots.user_agents.join("com.example.linked.plist");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        let items = vec![item(
            ItemKind::LaunchAgent,
            Scope::User,
            "com.example.linked",
            Some(link.clone()),
        )];
        let results = disable_with(
            &items,
            &roots,
            501,
            &tmp.path().join("Trash"),
            &FakeLaunchd::default(),
            &trash,
        );
        assert!(matches!(results[0].outcome, ActionOutcome::Disabled { .. }));
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(
            target.exists(),
            "the symlink target inside the app must stay"
        );
    }

    #[test]
    fn system_items_get_admin_commands() {
        let tmp = TempDir::new("admin");
        let user_trash = tmp.path().join("Trash");
        std::fs::create_dir_all(&user_trash).unwrap();
        // A same-named file already in the Trash must not be overwritten.
        std::fs::write(user_trash.join("com.example.helperd.plist"), b"old").unwrap();
        let mut daemon = item(
            ItemKind::LaunchDaemon,
            Scope::System,
            "com.example.helperd",
            Some(PathBuf::from(
                "/Library/LaunchDaemons/com.example.helperd.plist",
            )),
        );
        daemon.program = Some(PathBuf::from(
            "/Library/PrivilegedHelperTools/com.example.helperd",
        ));
        let ActionOutcome::NeedsAdmin { disable, remove } = admin_plan(&daemon, 501, &user_trash)
        else {
            panic!("expected admin commands");
        };
        let cmds: Vec<&str> = disable.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(
            cmds,
            vec![
                "sudo launchctl bootout system/com.example.helperd",
                "sudo launchctl disable system/com.example.helperd"
            ]
        );
        assert_eq!(
            remove[1].command,
            "sudo mv -n /Library/LaunchDaemons/com.example.helperd.plist ~/.Trash/'com.example.helperd 2.plist'"
        );
        // The helper isn't on this Mac, so no command for it.
        assert_eq!(remove.len(), 2);

        let agent = item(
            ItemKind::LaunchAgent,
            Scope::System,
            "us.example.updater",
            Some(PathBuf::from(
                "/Library/LaunchAgents/us.example updater.plist",
            )),
        );
        let ActionOutcome::NeedsAdmin { disable, remove } = admin_plan(&agent, 501, &user_trash)
        else {
            panic!("expected admin commands");
        };
        assert_eq!(
            disable[0].command,
            "launchctl bootout gui/501/us.example.updater"
        );
        assert_eq!(
            disable[1].command,
            "launchctl disable gui/501/us.example.updater"
        );
        assert_eq!(
            remove[1].command,
            "sudo mv -n '/Library/LaunchAgents/us.example updater.plist' ~/.Trash/'us.example updater.plist'"
        );
    }

    #[test]
    fn other_kinds() {
        let tmp = TempDir::new("kinds");
        let roots = Roots::synthetic(tmp.path());
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        let items = vec![
            item(ItemKind::LoginItem, Scope::User, "com.example.app", None),
            item(
                ItemKind::BackgroundItem,
                Scope::User,
                "com.example.helper",
                None,
            ),
            item(
                ItemKind::SystemExtension,
                Scope::System,
                "com.example.ext",
                None,
            ),
            item(
                ItemKind::KernelExtension,
                Scope::System,
                "com.example.kext",
                None,
            ),
        ];
        let r = disable_with(
            &items,
            &roots,
            501,
            &tmp.path().join("Trash"),
            &FakeLaunchd::default(),
            &trash,
        );
        assert!(
            matches!(&r[0].outcome, ActionOutcome::OpenSettings { url, .. } if url == LOGIN_ITEMS_SETTINGS_URL)
        );
        assert!(matches!(&r[1].outcome, ActionOutcome::OpenSettings { .. }));
        assert!(matches!(&r[2].outcome, ActionOutcome::OpenSettings { .. }));
        assert!(matches!(&r[3].outcome, ActionOutcome::Skipped { .. }));
        assert!(open_settings("https://example.com").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(
            shell_quote("/Library/LaunchDaemons/a.b-c.plist"),
            "/Library/LaunchDaemons/a.b-c.plist"
        );
        assert_eq!(shell_quote("/A B/it's"), r"'/A B/it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
