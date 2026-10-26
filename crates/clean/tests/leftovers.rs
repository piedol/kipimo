//! The leftover finder on a synthetic home and app folder.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use common::*;
use kipimo_clean::{
    AppIndex, AppTarget, Confidence, Env, ItemOutcome, LeftoverKind, LeftoverReport, SkipReason,
    find_leftovers, find_leftovers_for_bundle, remove_leftovers,
};

fn env(f: &Fixture, a: Acme) -> Env {
    let mut env = f.env();
    env.signing = Arc::new(a.signing);
    env
}

fn rel(f: &Fixture, p: &Path) -> String {
    p.strip_prefix(&f.home)
        .map(|r| format!("~/{}", r.display()))
        .or_else(|_| p.strip_prefix(&f.sys).map(|r| format!("/{}", r.display())))
        .unwrap()
}

fn offered(f: &Fixture, r: &LeftoverReport) -> Vec<(String, Confidence)> {
    let mut v: Vec<(String, Confidence)> = r
        .leftovers
        .iter()
        .map(|l| (rel(f, &l.path), l.confidence))
        .collect();
    v.sort();
    v
}

fn kept(f: &Fixture, r: &LeftoverReport) -> Vec<(String, SkipReason)> {
    let mut v: Vec<(String, SkipReason)> = r
        .kept
        .iter()
        .map(|k| (rel(f, &k.path), k.reason.clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn run(env: &Env, bundle: &Path) -> LeftoverReport {
    let cancel = AtomicBool::new(false);
    let target = AppTarget::from_bundle(env, bundle).unwrap();
    let index = AppIndex::build(env, &cancel);
    find_leftovers(env, &target, &index, &cancel)
}

#[test]
fn finds_every_location_conservatively() {
    let f = Fixture::new("finder");
    let a = acme(&f);
    let bundle = a.target.clone();
    let env = env(&f, a);
    let r = run(&env, &bundle);

    use Confidence::*;
    let expected: Vec<(String, Confidence)> = vec![
        ("/Library/Application Support/Target Pro", Medium),
        ("/Library/LaunchDaemons/com.acme.target.privhelper.plist", High),
        ("/Library/PrivilegedHelperTools/com.acme.target.privhelper", High),
        ("~/Library/Application Support/Target Pro", Medium),
        ("~/Library/Application Support/Target Pro_Helper", Low),
        ("~/Library/Application Support/com.acme.target", High),
        ("~/Library/Application Support/com.acme.target.share", High),
        ("~/Library/Caches/com.acme.target", High),
        ("~/Library/Caches/com.acme.target.ShipIt", High),
        ("~/Library/Containers/ACME123456.com.acme.target.helper", High),
        ("~/Library/Containers/com.acme.target.share", High),
        ("~/Library/Cookies/com.acme.target.binarycookies", High),
        ("~/Library/Group Containers/ACME123456.com.acme.target", High),
        ("~/Library/HTTPStorages/com.acme.target", High),
        ("~/Library/HTTPStorages/com.acme.target.binarycookies", High),
        ("~/Library/LaunchAgents/com.acme.target.updater.plist", High),
        ("~/Library/LaunchAgents/com.random.job.plist", High),
        ("~/Library/LaunchAgents/com.zzz.assoc.plist", High),
        ("~/Library/Logs/Target Pro", Medium),
        ("~/Library/Logs/com.acme.target", High),
        ("~/Library/Preferences/ByHost/com.acme.target.0123ABCD-0123-4567-89AB-0123456789AB.plist", High),
        ("~/Library/Preferences/com.acme.target.plist", High),
        ("~/Library/Preferences/com.acme.target.share.plist", High),
        ("~/Library/Saved Application State/com.acme.target.savedState", High),
        ("~/Library/WebKit/com.acme.target", High),
    ]
    .into_iter()
    .map(|(p, c)| (p.to_owned(), c))
    .collect();
    assert_eq!(offered(&f, &r), expected);

    assert_eq!(
        kept(&f, &r),
        vec![
            (
                "~/Library/Application Support/Target Pro Max".to_owned(),
                SkipReason::Shared {
                    app: "Target Pro Max".into()
                }
            ),
            (
                "~/Library/Group Containers/ACME123456.shared".to_owned(),
                SkipReason::Shared {
                    app: "Sibling".into()
                }
            ),
            (
                "~/Library/Group Containers/group.com.acme.common".to_owned(),
                SkipReason::Shared {
                    app: "Sibling".into()
                }
            ),
            (
                "~/Library/LaunchAgents/com.acme.target.bridge.plist".to_owned(),
                SkipReason::Shared {
                    app: "Other".into()
                }
            ),
        ]
    );

    // Kinds, admin flags and default ticks.
    let get = |p: &str| r.leftovers.iter().find(|l| rel(&f, &l.path) == p).unwrap();
    assert_eq!(
        get("~/Library/Group Containers/ACME123456.com.acme.target").kind,
        LeftoverKind::GroupContainer
    );
    assert_eq!(get("~/Library/Preferences/ByHost/com.acme.target.0123ABCD-0123-4567-89AB-0123456789AB.plist").kind, LeftoverKind::ByHostPreferences);
    let helper = get("/Library/PrivilegedHelperTools/com.acme.target.privhelper");
    assert!(
        helper.needs_admin
            && !helper.default_selected
            && helper.kind == LeftoverKind::PrivilegedHelper
    );
    assert!(
        !get("~/Library/Application Support/Target Pro_Helper").default_selected,
        "low confidence starts unticked"
    );
    // Data folders are offered unticked; caches, preferences and saved state start ticked.
    for data in [
        "~/Library/Application Support/Target Pro",
        "~/Library/Group Containers/ACME123456.com.acme.target",
    ] {
        assert!(!get(data).default_selected, "{data} starts unticked");
    }
    for p in [
        "~/Library/Caches/com.acme.target",
        "~/Library/Preferences/ByHost/com.acme.target.0123ABCD-0123-4567-89AB-0123456789AB.plist",
    ] {
        assert!(get(p).default_selected, "{p} starts ticked");
    }
    for l in &r.leftovers {
        if kipimo_clean::holds_user_data(l.kind) {
            assert!(!l.default_selected, "{}", l.path.display());
        }
    }
    assert!(get("~/Library/Caches/com.acme.target").bytes >= 20_000);
    // Admin-only items aren't in the movable total.
    let movable: u64 = r
        .leftovers
        .iter()
        .filter(|l| !l.needs_admin)
        .map(|l| l.bytes)
        .sum();
    assert_eq!(r.total_bytes, movable);
    assert!(
        r.total_bytes < 500_000,
        "the symlinked folder was not followed"
    );
    assert!(r.other_copy.is_none() && !r.target_running && !r.cancelled);
    // The stock Electron helper ID is not treated as the app's.
    assert!(
        !r.target
            .related_ids
            .iter()
            .any(|i| i == "com.github.Electron.helper")
    );
    assert!(
        serde_json::to_string(&r)
            .unwrap()
            .contains("\"confidence\":\"high\"")
    );
}

#[test]
fn another_copy_keeps_everything() {
    let f = Fixture::new("copy");
    let a = acme(&f);
    let bundle = a.target.clone();
    f.app(
        f.h("Applications/Target.app"),
        "com.acme.target",
        "Target Pro",
        "2.0",
    );
    let env = env(&f, a);
    let r = run(&env, &bundle);
    assert!(r.leftovers.is_empty());
    assert_eq!(r.other_copy, Some(f.h("Applications/Target.app")));
    assert!(r.kept.len() >= 20);
}

#[test]
fn an_app_already_in_the_trash() {
    let f = Fixture::new("intrash");
    let a = acme(&f);
    let in_trash = f.h(".Trash/Target.app");
    fs::create_dir_all(f.h(".Trash")).unwrap();
    fs::rename(&a.target, &in_trash).unwrap();
    let mut signing = a.signing;
    let s = signing.0.remove(&a.target).unwrap();
    signing.0.insert(in_trash.clone(), s);
    let mut env = f.env();
    env.signing = Arc::new(signing);
    let cancel = AtomicBool::new(false);
    let r = find_leftovers_for_bundle(&env, &in_trash, &cancel).unwrap();
    let paths: Vec<String> = r.leftovers.iter().map(|l| rel(&f, &l.path)).collect();
    assert!(paths.contains(&"~/Library/Group Containers/ACME123456.com.acme.target".to_owned()));
    assert!(paths.contains(&"~/Library/LaunchAgents/com.acme.target.updater.plist".to_owned()));
    assert!(r.other_copy.is_none());
}

#[test]
fn a_gone_app_by_bundle_id() {
    let f = Fixture::new("gone");
    let a = acme(&f);
    fs::remove_dir_all(&a.target).unwrap();
    let env = env(&f, a);
    let cancel = AtomicBool::new(false);
    let index = AppIndex::build(&env, &cancel);
    let r = find_leftovers(
        &env,
        &AppTarget::from_bundle_id("com.acme.target", None),
        &index,
        &cancel,
    );
    assert!(
        r.leftovers.iter().all(|l| l.confidence == Confidence::High),
        "no names, so only ID matches"
    );
    let paths: Vec<String> = r.leftovers.iter().map(|l| rel(&f, &l.path)).collect();
    assert!(
        paths.contains(&"~/Library/Group Containers/ACME123456.com.acme.target".to_owned()),
        "team-prefixed group by ID"
    );
    assert!(paths.contains(&"~/Library/Caches/com.acme.target.ShipIt".to_owned()));
    assert!(!paths.iter().any(|p| p.contains("Target Pro")));
}

#[test]
fn remove_leftovers_rechecks_every_item() {
    let f = Fixture::new("remove");
    let a = acme(&f);
    let bundle = a.target.clone();
    let env = env(&f, a);
    let r = run(&env, &bundle);
    let pick = |p: &str| {
        r.leftovers
            .iter()
            .find(|l| rel(&f, &l.path) == p)
            .unwrap()
            .clone()
    };
    let cache = pick("~/Library/Caches/com.acme.target");
    let helper = pick("/Library/PrivilegedHelperTools/com.acme.target.privhelper");
    let mut shared = cache.clone();
    shared.path = f.h("Library/Group Containers/ACME123456.shared");
    let mut forged = cache.clone();
    forged.path = f.h("Documents/important");
    f.file(&forged.path, 10);
    let mut other_apps = cache.clone();
    other_apps.path = f.h("Library/Application Support/com.acme.sibling");

    let target = AppTarget::from_bundle(&env, &bundle).unwrap();
    let cancel = AtomicBool::new(false);
    let results = remove_leftovers(
        &env,
        &target,
        &[
            cache.clone(),
            helper,
            shared,
            forged.clone(),
            other_apps.clone(),
        ],
        &mut |_, _| {},
        &cancel,
    );
    assert!(matches!(results[0].outcome, ItemOutcome::Moved { .. }));
    assert_eq!(
        results[1].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::NeedsAdmin
        }
    );
    assert_eq!(
        results[2].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::Shared {
                app: "Sibling".into()
            }
        }
    );
    assert_eq!(
        results[3].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert_eq!(
        results[4].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert_eq!(f.trash.moved(), vec![cache.path.clone()]);
    assert!(forged.path.exists() && other_apps.path.exists());
    assert!(
        f.s("Library/PrivilegedHelperTools/com.acme.target.privhelper")
            .exists()
    );
    assert!(f.h("Library/Group Containers/ACME123456.shared/x").exists());

    // While the app runs (say it was moved to the Trash but not quit), nothing is moved.
    let mut running_env = env.clone();
    running_env.running = running(&f.h(".Trash/Target.app"), "com.acme.target", "Target Pro");
    let prefs = pick("~/Library/Preferences/com.acme.target.plist");
    let results = remove_leftovers(
        &running_env,
        &target,
        std::slice::from_ref(&prefs),
        &mut |_, _| {},
        &cancel,
    );
    assert_eq!(
        results[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::InUse {
                app: "Target Pro".into()
            }
        }
    );
    assert!(prefs.path.exists());
}

#[test]
fn group_containers_are_shared_by_entitlement_or_developer() {
    let f = Fixture::new("groups");
    let a = acme(&f);
    let bundle = a.target.clone();
    // Without Sibling installed, the developer-wide groups become the target's.
    fs::remove_dir_all(&a.sibling).unwrap();
    let env = env(&f, a);
    let r = run(&env, &bundle);
    let paths: Vec<String> = r.leftovers.iter().map(|l| rel(&f, &l.path)).collect();
    assert!(paths.contains(&"~/Library/Group Containers/ACME123456.shared".to_owned()));
    assert!(paths.contains(&"~/Library/Group Containers/group.com.acme.common".to_owned()));
    assert!(!paths.iter().any(|p| p.contains("OTHER12345")));
}

#[test]
fn paths_never_leave_the_library_folders() {
    let f = Fixture::new("bounds");
    let a = acme(&f);
    let bundle = a.target.clone();
    let env = env(&f, a);
    let r = run(&env, &bundle);
    let roots: Vec<PathBuf> = vec![f.h("Library"), f.s("Library")];
    for l in &r.leftovers {
        assert!(
            roots.iter().any(|root| l.path.starts_with(root)),
            "{}",
            l.path.display()
        );
        assert!(
            !fs::symlink_metadata(&l.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn a_symlinked_library_folder_is_not_followed() {
    let f = Fixture::new("linkedroot");
    let a = acme(&f);
    let bundle = a.target.clone();
    fs::remove_dir_all(f.h("Library/WebKit")).unwrap();
    let elsewhere = f.root.join("elsewhere/WebKit");
    f.file(elsewhere.join("com.acme.target/data"), 40_000);
    std::os::unix::fs::symlink(&elsewhere, f.h("Library/WebKit")).unwrap();
    let env = env(&f, a);
    let r = run(&env, &bundle);
    assert!(!r.leftovers.iter().any(|l| l.kind == LeftoverKind::WebKit));
    assert!(!r.kept.iter().any(|k| k.kind == LeftoverKind::WebKit));
}
