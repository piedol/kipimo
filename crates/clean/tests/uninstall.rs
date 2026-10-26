//! Tool B on a synthetic app folder and home.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use common::*;
use kipimo_clean::{
    AppIndex, AppTarget, AppsEvent, Env, ItemOutcome, LastUsedReader, Leftover, Protection,
    SkipReason, UninstallStatus, find_leftovers, list_apps, uninstall,
};

struct FakeLastUsed;
impl LastUsedReader for FakeLastUsed {
    fn last_used(&self, bundle: &Path) -> Option<i64> {
        bundle.ends_with("Target.app").then_some(1_700_000_000)
    }
}

fn world(f: &Fixture) -> Env {
    let mut a = acme(f);
    let safari = f.app(
        f.s("Applications/Safari.app"),
        "com.apple.Safari",
        "Safari",
        "26.0",
    );
    a.signing.0.insert(safari, signing(None, true, &[]));
    f.app(
        f.s("Applications/Kipimo.app"),
        "top.piedol.kipimo",
        "Kipimo",
        "0.1",
    );
    f.app(
        f.h("Applications/Games/Puzzle.app"),
        "com.games.puzzle",
        "Puzzle",
        "1.2",
    );
    f.app(
        f.h("Applications/Games/Deeper/TooDeep.app"),
        "com.games.deep",
        "TooDeep",
        "1",
    );
    f.dir(f.s("Applications/Not An App"));
    std::os::unix::fs::symlink(
        f.s("Applications/Other.app"),
        f.s("Applications/Linked.app"),
    )
    .unwrap();
    let mut env = f.env();
    env.signing = Arc::new(a.signing);
    env.last_used = Arc::new(FakeLastUsed);
    env.own_bundle_id = Some("top.piedol.kipimo".into());
    env
}

fn leftovers_of(env: &Env, bundle: &Path) -> Vec<Leftover> {
    let cancel = AtomicBool::new(false);
    let target = AppTarget::from_bundle(env, bundle).unwrap();
    let index = AppIndex::build(env, &cancel);
    find_leftovers(env, &target, &index, &cancel).leftovers
}

#[test]
fn lists_apps_with_details() {
    let f = Fixture::new("list");
    let mut env = world(&f);
    env.running = running(&f.s("Applications/Other.app"), "com.other.tool", "Other");
    let cancel = AtomicBool::new(false);
    let (mut found, mut sized) = (0, 0);
    let apps = list_apps(
        &env,
        &mut |e| match e {
            AppsEvent::Found { app } => {
                assert!(app.size_bytes.is_none());
                found += 1
            }
            AppsEvent::Sized { .. } => sized += 1,
            AppsEvent::Progress { .. } => {}
        },
        &cancel,
    );
    let mut names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Kipimo",
            "Other",
            "Puzzle",
            "Safari",
            "Sibling",
            "Target Pro",
            "Target Pro Max"
        ]
    );
    assert_eq!((found, sized), (apps.len(), apps.len()));
    let get = |n: &str| apps.iter().find(|a| a.name == n).unwrap();
    let t = get("Target Pro");
    assert_eq!(t.bundle_id.as_deref(), Some("com.acme.target"));
    assert_eq!(t.version.as_deref(), Some("2.1"));
    assert_eq!(t.team_id.as_deref(), Some("ACME123456"));
    assert_eq!(t.last_used_unix, Some(1_700_000_000));
    assert!(t.size_bytes.unwrap() >= 50_000);
    assert!(t.protected.is_none() && !t.running);
    assert!(get("Other").running);
    assert_eq!(get("Safari").protected, Some(Protection::ShipsWithMacOS));
    assert_eq!(get("Kipimo").protected, Some(Protection::Kipimo));
    assert!(
        serde_json::to_string(&apps)
            .unwrap()
            .contains("\"protected\":\"ships_with_macos\"")
    );
}

#[test]
fn uninstall_moves_app_and_selected_leftovers() {
    let f = Fixture::new("uninstall");
    let env = world(&f);
    let target = f.s("Applications/Target.app");
    let all = leftovers_of(&env, &target);
    let ticked: Vec<Leftover> = all.iter().filter(|l| l.default_selected).cloned().collect();
    let unticked: Vec<Leftover> = all
        .iter()
        .filter(|l| !l.default_selected && !l.needs_admin)
        .cloned()
        .collect();
    let admin: Vec<Leftover> = all.iter().filter(|l| l.needs_admin).cloned().collect();
    assert!(!ticked.is_empty() && !unticked.is_empty() && !admin.is_empty());
    let mut selected = ticked.clone();
    selected.push(admin[0].clone());
    let mut steps = Vec::new();
    let cancel = AtomicBool::new(false);
    let r = uninstall(
        &env,
        &target,
        50_000,
        &selected,
        &mut |p| steps.push(p.done),
        &cancel,
    );
    assert_eq!(r.status, UninstallStatus::Done);
    assert!(matches!(r.app.outcome, ItemOutcome::Moved { .. }));
    assert!(!target.exists());
    for (res, l) in r.leftovers.iter().zip(&selected) {
        if l.needs_admin {
            assert_eq!(
                res.outcome,
                ItemOutcome::Skipped {
                    reason: SkipReason::NeedsAdmin
                }
            );
            assert!(l.path.exists());
        } else {
            assert!(
                matches!(res.outcome, ItemOutcome::Moved { .. }),
                "{:?}",
                res
            );
            assert!(!l.path.exists());
        }
    }
    for l in &unticked {
        assert!(
            l.path.exists(),
            "unticked leftover {} was touched",
            l.path.display()
        );
    }
    assert!(
        f.h("Library/Group Containers/ACME123456.shared/x").exists(),
        "shared group kept"
    );
    assert!(f.s("Applications/Sibling.app").exists() && f.s("Applications/Other.app").exists());
    assert_eq!(f.trash.moved().len(), 1 + ticked.len());
    assert_eq!(steps.last(), Some(&(selected.len() + 1)));
}

#[test]
fn running_apps_must_quit_first() {
    let f = Fixture::new("quitfirst");
    let mut env = world(&f);
    let target = f.s("Applications/Target.app");
    let selected = leftovers_of(&env, &target);
    env.running = running(&target, "com.acme.target", "Target Pro");
    let cancel = AtomicBool::new(false);
    let r = uninstall(&env, &target, 0, &selected, &mut |_| {}, &cancel);
    assert_eq!(r.status, UninstallStatus::QuitFirst);
    assert!(target.exists());
    assert!(f.trash.moved().is_empty());
    assert!(r.leftovers.iter().all(|l| l.outcome
        == ItemOutcome::Skipped {
            reason: SkipReason::AppNotRemoved
        }));
}

#[test]
fn protected_and_foreign_paths_are_refused() {
    let f = Fixture::new("refuse");
    let env = world(&f);
    let cancel = AtomicBool::new(false);
    let r = uninstall(
        &env,
        &f.s("Applications/Safari.app"),
        0,
        &[],
        &mut |_| {},
        &cancel,
    );
    assert_eq!(
        r.status,
        UninstallStatus::Refused {
            protection: Protection::ShipsWithMacOS
        }
    );
    let r = uninstall(
        &env,
        &f.s("Applications/Kipimo.app"),
        0,
        &[],
        &mut |_| {},
        &cancel,
    );
    assert_eq!(
        r.status,
        UninstallStatus::Refused {
            protection: Protection::Kipimo
        }
    );
    // Not in an app folder, too deep, a symlink, not a bundle.
    let stray = f.app(f.h("Downloads/Stray.app"), "com.stray", "Stray", "1");
    for p in [
        stray,
        f.h("Applications/Games/Deeper/TooDeep.app"),
        f.s("Applications/Linked.app"),
        f.s("Applications/Not An App"),
    ] {
        let r = uninstall(&env, &p, 0, &[], &mut |_| {}, &cancel);
        assert_eq!(r.status, UninstallStatus::NotAnApp, "{}", p.display());
        assert!(p.exists());
    }
    assert!(f.trash.moved().is_empty());
}

#[test]
fn an_app_that_cannot_move_keeps_its_leftovers() {
    let f = Fixture::new("stuck");
    let env = world(&f);
    let target = f.s("Applications/Target.app");
    let selected: Vec<Leftover> = leftovers_of(&env, &target)
        .into_iter()
        .filter(|l| l.default_selected)
        .collect();
    // A read-only bundle can't be moved to another folder (like a root-owned app from a .pkg).
    chmod(&target, 0o555);
    let cancel = AtomicBool::new(false);
    let r = uninstall(&env, &target, 0, &selected, &mut |_| {}, &cancel);
    chmod(&target, 0o755);
    assert_eq!(r.status, UninstallStatus::AppNotMoved);
    assert_eq!(
        r.app.outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::ReadOnly
        }
    );
    assert!(selected.iter().all(|l| l.path.exists()));
    assert!(f.trash.moved().is_empty());
}

#[test]
fn leftovers_from_another_app_are_not_accepted() {
    let f = Fixture::new("crossapp");
    let env = world(&f);
    let sibling_stuff = vec![Leftover {
        path: f.h("Library/Application Support/com.acme.sibling"),
        ..leftovers_of(&env, &f.s("Applications/Target.app"))[0].clone()
    }];
    let cancel = AtomicBool::new(false);
    let r = uninstall(
        &env,
        &f.s("Applications/Target.app"),
        0,
        &sibling_stuff,
        &mut |_| {},
        &cancel,
    );
    assert_eq!(r.status, UninstallStatus::Done);
    assert_eq!(
        r.leftovers[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert!(f.h("Library/Application Support/com.acme.sibling").exists());
}
