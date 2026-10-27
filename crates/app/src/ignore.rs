//! Ignoring flagged startup items and apps from the signature check.
//!
//! An ignored entry stays in Startup items (dimmed, with an "Ignored" badge) but no longer
//! counts toward "need a look", and stops colouring the menu-bar icon, changing Mac Health
//! or producing a recommendation.
//!
//! An entry is ignored for what is wrong with it now, not for good. Each key names the entry
//! and the problem, and includes the code's identity, so a different problem, or a program
//! that was reinstalled or updated, counts again:
//!
//! - a startup item has one key per warning flag:
//!   `["item", <id>, <program>, <flag>, <detail>]`, where the detail is the program's code
//!   directory hash for signature flags, the script for script flags and the app for
//!   "app missing". The item is ignored while all of its warning flags are;
//! - an app from the signature check: `["app", <path>, "broken" | "unsigned", <cdhash>,
//!   <version>]`.
//!
//! Keys are JSON arrays, so no path or label can run into the next field. The list lives in
//! Settings (`ignoredIssues`).

use kipimo_startup::{AppCheck, Flag, Severity, SignatureStatus, StartupItem};

fn key(parts: &[&str]) -> String {
    serde_json::to_string(parts).unwrap_or_default()
}

fn flag_name(f: Flag) -> &'static str {
    match f {
        Flag::Unsigned => "unsigned",
        Flag::AdHocSigned => "ad-hoc",
        Flag::InvalidSignature => "invalid-signature",
        Flag::UntrustedCertificate => "untrusted-certificate",
        Flag::ProgramMissing => "program-missing",
        Flag::OwnerAppMissing => "app-missing",
        Flag::RunsScript => "runs-script",
        Flag::ScriptMissing => "script-missing",
        Flag::MalformedPlist => "malformed-plist",
    }
}

/// The keys for what is wrong with `item` now: one per warning flag (none when it has none).
pub fn item_keys(item: &StartupItem) -> Vec<String> {
    let program = item
        .program
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cdhash = item
        .signature
        .as_ref()
        .and_then(|s| s.cdhash.clone())
        .unwrap_or_default();
    let script = item
        .script
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let owner = item
        .owner
        .as_ref()
        .and_then(|o| {
            o.path
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned())
                .or_else(|| o.bundle_id.clone())
        })
        .unwrap_or_default();
    let mut keys: Vec<String> = item
        .flags
        .iter()
        .filter(|f| f.severity() == Severity::Warning)
        .map(|&f| {
            let detail = match f {
                Flag::Unsigned
                | Flag::AdHocSigned
                | Flag::InvalidSignature
                | Flag::UntrustedCertificate => cdhash.as_str(),
                Flag::ScriptMissing | Flag::RunsScript => script.as_str(),
                Flag::OwnerAppMissing => owner.as_str(),
                Flag::ProgramMissing | Flag::MalformedPlist => "",
            };
            key(&["item", &item.id, &program, flag_name(f), detail])
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The key for an app from the signature check.
pub fn app_key(a: &AppCheck) -> String {
    let status = match a.signature.status {
        SignatureStatus::Invalid => "broken",
        SignatureStatus::Unsigned => "unsigned",
        SignatureStatus::Valid => "valid",
        SignatureStatus::NotChecked => "not-checked",
    };
    key(&[
        "app",
        &a.path.to_string_lossy(),
        status,
        a.signature.cdhash.as_deref().unwrap_or(""),
        a.version.as_deref().unwrap_or(""),
    ])
}

/// Every key is in `ignored` (and there is at least one).
pub fn covered(keys: &[String], ignored: &[String]) -> bool {
    !keys.is_empty() && keys.iter().all(|k| ignored.contains(k))
}

/// The item is flagged and every one of its problems is ignored.
pub fn item_ignored(item: &StartupItem, ignored: &[String]) -> bool {
    item.needs_attention() && covered(&item_keys(item), ignored)
}

pub fn app_ignored(a: &AppCheck, ignored: &[String]) -> bool {
    ignored.contains(&app_key(a))
}

/// Change the ignore list: save it, and tell the menu-bar app (when this is the tools process)
/// or recolour the icon (when it is the menu-bar app).
pub fn change(f: impl FnOnce(&mut Vec<String>)) {
    let mut list = crate::settings::get().ignored;
    f(&mut list);
    crate::settings::set_ignored(list.clone());
    if crate::tools_process::is_child() {
        crate::tools_process::report_ignored(&list);
    } else {
        crate::app::refresh_attention();
    }
}

/// The item's id, from one of its keys.
fn item_id_of(key: &str) -> Option<String> {
    let parts: Vec<String> = serde_json::from_str(key).ok()?;
    (parts.first().map(String::as_str) == Some("item"))
        .then(|| parts.get(1).cloned())
        .flatten()
}

fn app_path_of(key: &str) -> Option<String> {
    let parts: Vec<String> = serde_json::from_str(key).ok()?;
    (parts.first().map(String::as_str) == Some("app"))
        .then(|| parts.get(1).cloned())
        .flatten()
}

/// Ignore what is wrong with `item` now (replacing what was ignored for it before).
pub fn ignore_item(list: &mut Vec<String>, item: &StartupItem) {
    unignore_item(list, item);
    list.extend(item_keys(item));
}

pub fn unignore_item(list: &mut Vec<String>, item: &StartupItem) {
    list.retain(|k| item_id_of(k).as_deref() != Some(item.id.as_str()));
}

pub fn ignore_app(list: &mut Vec<String>, a: &AppCheck) {
    unignore_app(list, a);
    list.push(app_key(a));
}

pub fn unignore_app(list: &mut Vec<String>, a: &AppCheck) {
    let path = a.path.to_string_lossy();
    list.retain(|k| app_path_of(k).as_deref() != Some(&*path));
}

/// After a complete scan: drop item keys that no longer match any item's problems (fixed,
/// changed or gone), so the list doesn't grow. Returns whether anything was dropped.
pub fn prune_items(list: &mut Vec<String>, items: &[StartupItem]) -> bool {
    let current: std::collections::HashSet<String> = items.iter().flat_map(item_keys).collect();
    let before = list.len();
    list.retain(|k| item_id_of(k).is_none() || current.contains(k));
    list.len() != before
}

/// After a complete signature check: the same for app keys.
pub fn prune_apps(list: &mut Vec<String>, apps: &[&AppCheck]) -> bool {
    let current: std::collections::HashSet<String> = apps.iter().map(|a| app_key(a)).collect();
    let before = list.len();
    list.retain(|k| app_path_of(k).is_none() || current.contains(k));
    list.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use kipimo_startup::{ItemKind, ItemState, Scope, Signature, SignerKind};
    use std::path::PathBuf;

    fn sig(status: SignatureStatus, kind: SignerKind, cdhash: Option<&str>) -> Signature {
        Signature {
            path: "/opt/tool".into(),
            status,
            kind,
            signer: None,
            team_id: None,
            identifier: None,
            cdhash: cdhash.map(str::to_string),
            notarized: None,
            problem: None,
        }
    }

    fn item(flags: Vec<Flag>, cdhash: Option<&str>) -> StartupItem {
        StartupItem {
            id: "launchd:/Users/me/Library/LaunchAgents/com.example.tool.plist".into(),
            kind: ItemKind::LaunchAgent,
            scope: Scope::User,
            label: "com.example.tool".into(),
            name: "tool".into(),
            plist_path: Some("/Users/me/Library/LaunchAgents/com.example.tool.plist".into()),
            program: Some("/opt/tool".into()),
            arguments: vec![],
            script: None,
            owner: None,
            developer: None,
            state: ItemState::default(),
            signature: Some(sig(SignatureStatus::Valid, SignerKind::AdHoc, cdhash)),
            flags,
            btm_identifier: None,
        }
    }

    fn app(path: &str, status: SignatureStatus, cdhash: &str, version: &str) -> AppCheck {
        AppCheck {
            path: PathBuf::from(path),
            name: "Thing".into(),
            bundle_id: Some("com.example.thing".into()),
            version: Some(version.into()),
            signature: sig(status, SignerKind::DeveloperId, Some(cdhash)),
            elapsed_ms: 1,
        }
    }

    #[test]
    fn an_ignored_item_comes_back_when_its_problem_changes() {
        let mut list = Vec::new();
        let adhoc = item(vec![Flag::AdHocSigned, Flag::RunsScript], Some("aa11"));
        assert!(!item_ignored(&adhoc, &list));
        ignore_item(&mut list, &adhoc);
        // One key: info-level flags (runs a script) don't count, so they aren't keyed.
        assert_eq!(list.len(), 1);
        assert!(item_ignored(&adhoc, &list));

        // Rebuilt or reinstalled: a new cdhash.
        let rebuilt = item(vec![Flag::AdHocSigned, Flag::RunsScript], Some("bb22"));
        assert!(!item_ignored(&rebuilt, &list));
        // A second problem on top of the ignored one.
        let worse = item(vec![Flag::AdHocSigned, Flag::ProgramMissing], Some("aa11"));
        assert!(!item_ignored(&worse, &list));
        // Points at another program now.
        let mut moved = adhoc.clone();
        moved.program = Some("/opt/other".into());
        assert!(!item_ignored(&moved, &list));
        // Nothing wrong at all: not "ignored", just fine.
        assert!(!item_ignored(
            &item(vec![Flag::RunsScript], Some("aa11")),
            &list
        ));

        // Ignoring it again replaces the old keys; Stop Ignoring removes them.
        ignore_item(&mut list, &worse);
        assert_eq!(list.len(), 2);
        assert!(item_ignored(&worse, &list));
        assert!(item_ignored(&adhoc, &list), "a subset of what is ignored");
        unignore_item(&mut list, &worse);
        assert!(list.is_empty());
    }

    #[test]
    fn an_ignored_app_comes_back_after_an_update_or_reinstall() {
        let mut list = vec!["something else".to_string()];
        let broken = app(
            "/Applications/Thing.app",
            SignatureStatus::Invalid,
            "c0de",
            "1.0 (10)",
        );
        ignore_app(&mut list, &broken);
        assert!(app_ignored(&broken, &list));
        let updated = app(
            "/Applications/Thing.app",
            SignatureStatus::Invalid,
            "c0de",
            "1.1 (11)",
        );
        assert!(!app_ignored(&updated, &list));
        let rebuilt = app(
            "/Applications/Thing.app",
            SignatureStatus::Invalid,
            "f00d",
            "1.0 (10)",
        );
        assert!(!app_ignored(&rebuilt, &list));
        let elsewhere = app(
            "/Users/me/Applications/Thing.app",
            SignatureStatus::Invalid,
            "c0de",
            "1.0 (10)",
        );
        assert!(!app_ignored(&elsewhere, &list));
        unignore_app(&mut list, &broken);
        assert_eq!(list, vec!["something else".to_string()]);
    }

    #[test]
    fn keys_keep_paths_apart() {
        // A path containing the separator a plain string key would use.
        let a = app("/Applications/A|B.app", SignatureStatus::Invalid, "1", "1");
        let b = app("/Applications/A", SignatureStatus::Invalid, "1", "1");
        let mut list = Vec::new();
        ignore_app(&mut list, &a);
        ignore_app(&mut list, &b);
        unignore_app(&mut list, &b);
        assert!(app_ignored(&a, &list));
        assert!(!app_ignored(&b, &list));
    }

    #[test]
    fn pruning_drops_only_stale_keys() {
        let adhoc = item(vec![Flag::AdHocSigned], Some("aa11"));
        let broken = app(
            "/Applications/Thing.app",
            SignatureStatus::Invalid,
            "c0de",
            "1.0",
        );
        let mut list = Vec::new();
        ignore_item(&mut list, &adhoc);
        ignore_app(&mut list, &broken);
        assert!(!prune_items(&mut list, std::slice::from_ref(&adhoc)));
        assert!(!prune_apps(&mut list, &[&broken]));
        assert_eq!(list.len(), 2);
        // The item was fixed (no flags): its key goes; the app's stays.
        assert!(prune_items(&mut list, &[item(vec![], Some("aa11"))]));
        assert_eq!(list, vec![app_key(&broken)]);
        assert!(prune_apps(&mut list, &[]));
        assert!(list.is_empty());
    }
}
