//! Tool A on a synthetic home folder: every category, running-app exclusion, essential caches,
//! symlinks, hard links, permission errors, and `clean()`'s checks.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use common::*;
use kipimo_clean::{
    CategoryAction, CategoryId, CategoryReport, Env, ItemOutcome, JunkItem, JunkReport, ScanEvent,
    ScanOptions, SkipReason, clean, scan_junk,
};

fn scan(env: &Env) -> JunkReport {
    let cancel = AtomicBool::new(false);
    scan_junk(env, &ScanOptions::default(), &mut |_| {}, &cancel)
}

fn cat(r: &JunkReport, id: CategoryId) -> &CategoryReport {
    r.categories.iter().find(|c| c.info.id == id).unwrap()
}

fn labels(c: &CategoryReport) -> Vec<&str> {
    let mut v: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
    v.sort();
    v
}

fn skipped(c: &CategoryReport, label: &str) -> SkipReason {
    c.skipped
        .iter()
        .find(|s| s.label == label)
        .unwrap_or_else(|| panic!("{label} not skipped"))
        .reason
        .clone()
}

/// Builds a home with something in every category.
fn populate(f: &Fixture) -> PathBuf {
    // App caches, including a running app's and a symlink pointing out of the home.
    f.file(f.h("Library/Caches/com.example.editor/blob"), 300_000);
    f.file(
        f.h("Library/Caches/com.example.editor/nested/deeper/more"),
        20_000,
    );
    f.file(f.h("Library/Caches/com.example.running/data"), 40_000);
    f.file(
        f.h("Library/Caches/com.example.running.ShipIt/update"),
        10_000,
    );
    f.file(f.h("Library/Caches/Google/Chrome/Default/cache"), 30_000);
    f.dir(f.h("Library/Caches/empty-folder"));
    let outside = f.root.join("outside");
    f.file(outside.join("huge"), 2_000_000);
    std::os::unix::fs::symlink(&outside, f.h("Library/Caches/linked-out")).unwrap();
    // macOS caches: one offered (unticked), two essential.
    f.file(f.h("Library/Caches/com.apple.helpd/index"), 12_000);
    f.file(f.h("Library/Caches/GeoServices/tiles"), 8_000);
    f.file(f.h("Library/Caches/CloudKit/state"), 9_000);
    f.file(f.h("Library/Caches/com.apple.bird/state"), 9_000);
    f.file(
        f.h("Library/Caches/com.apple.nsurlsessiond/download"),
        9_000,
    );
    // Logs and crash reports.
    f.file(f.h("Library/Logs/SomeApp/app.log"), 5_000);
    f.file(
        f.h("Library/Logs/DiagnosticReports/SomeApp-2024-01-01-000000.ips"),
        3_000,
    );
    f.file(
        f.h("Library/Logs/DiagnosticReports/Other-2024-02-01-000000.ips"),
        3_000,
    );
    // Xcode.
    f.file(
        f.h("Library/Developer/Xcode/DerivedData/App-abc/Build/obj.o"),
        70_000,
    );
    let old = f.h("Library/Developer/Xcode/Archives/2020-01-01/Old App.xcarchive");
    plist(
        &old.join("Info.plist"),
        &[("CreationDate", V::D(iso(days_ago(400))))],
    );
    f.file(old.join("dSYMs/App.dSYM"), 60_000);
    let new = f.h("Library/Developer/Xcode/Archives/2026-01-01/New App.xcarchive");
    plist(
        &new.join("Info.plist"),
        &[("CreationDate", V::D(iso(days_ago(3))))],
    );
    f.file(new.join("dSYMs/App.dSYM"), 60_000);
    let ds = f.h("Library/Developer/Xcode/iOS DeviceSupport");
    for (name, age) in [
        ("16.0 (20A362)", 300),
        ("17.0 (21A329)", 200),
        ("17.4 (21E219)", 10),
        ("18.0 (22A3354)", 400),
    ] {
        let p = f.file(ds.join(name).join("Symbols/dyld"), 25_000);
        set_mtime(&p, days_ago(age));
        set_mtime(p.parent().unwrap(), days_ago(age));
        set_mtime(&ds.join(name), days_ago(age));
    }
    // Simulators: one runtime installed (iOS 18).
    plist(
        &f.s("Library/Developer/CoreSimulator/Images/images.plist"),
        &[(
            "images",
            V::Arr(vec![V::Dict(vec![(
                "runtimeInfo",
                V::Dict(vec![(
                    "bundleIdentifier",
                    V::S("com.apple.CoreSimulator.SimRuntime.iOS-18-0"),
                )]),
            )])]),
        )],
    );
    for (udid, runtime, deleted) in [
        ("AAAA", "com.apple.CoreSimulator.SimRuntime.iOS-18-0", false),
        ("BBBB", "com.apple.CoreSimulator.SimRuntime.iOS-16-0", false),
        ("CCCC", "com.apple.CoreSimulator.SimRuntime.iOS-18-0", true),
    ] {
        let d = f.h("Library/Developer/CoreSimulator/Devices").join(udid);
        plist(
            &d.join("device.plist"),
            &[
                ("name", V::S("iPhone")),
                ("runtime", V::S(runtime)),
                ("isDeleted", V::B(deleted)),
            ],
        );
        f.file(d.join("data/file"), 15_000);
    }
    f.file(f.h("Library/Developer/CoreSimulator/Caches/dyld/24A100/com.apple.CoreSimulator.SimRuntime.iOS-17-0.21A1/x"), 11_000);
    f.file(f.h("Library/Developer/CoreSimulator/Caches/dyld/25A100/com.apple.CoreSimulator.SimRuntime.iOS-16-0.20A1/x"), 11_000);
    f.file(f.h("Library/Developer/CoreSimulator/Caches/dyld/25A100/com.apple.CoreSimulator.SimRuntime.iOS-18-0.22A1/x"), 11_000);
    // Developer caches.
    for rel in [
        "Library/Caches/Homebrew/downloads/pkg.tar.gz",
        ".npm/_cacache/index",
        "Library/Caches/Yarn/v6/pkg",
        "Library/pnpm/store/v3/files",
        ".cargo/registry/cache/index/crate.crate",
        ".cargo/registry/src/index/crate/lib.rs",
        ".cargo/git/checkouts/repo/abc/file",
        "Library/Caches/pip/http/x",
        ".gradle/caches/modules/x.jar",
        "Library/Caches/CocoaPods/Pods/x",
        "Library/Caches/go-build/aa/bb",
    ] {
        f.file(f.h(rel), 21_000);
    }
    // Mail downloads.
    f.file(
        f.h("Library/Containers/com.apple.mail/Data/Library/Mail Downloads/1234/report.pdf"),
        33_000,
    );
    // Device backups: one old, one recent.
    let b = f.h("Library/Application Support/MobileSync/Backup");
    plist(
        &b.join("old-device/Info.plist"),
        &[
            ("Device Name", V::S("Old Phone")),
            ("Last Backup Date", V::D(iso(days_ago(500)))),
        ],
    );
    f.file(b.join("old-device/00/blob"), 90_000);
    plist(
        &b.join("new-device/Info.plist"),
        &[
            ("Device Name", V::S("New Phone")),
            ("Last Backup Date", V::D(iso(days_ago(2)))),
        ],
    );
    f.file(b.join("new-device/00/blob"), 90_000);
    // Trash.
    f.file(f.h(".Trash/old.zip"), 44_000);
    outside
}

fn env_with_running(f: &Fixture) -> Env {
    let mut env = f.env();
    env.os_build = Some("25A100".into());
    env.running = running(
        &f.h("Applications/Running.app"),
        "com.example.running",
        "Running",
    );
    env
}

#[test]
fn every_category_is_found() {
    let f = Fixture::new("every");
    populate(&f);
    let env = env_with_running(&f);
    let mut events = (0, 0, 0);
    let cancel = AtomicBool::new(false);
    let r = scan_junk(
        &env,
        &ScanOptions::default(),
        &mut |e| match e {
            ScanEvent::Started { categories } => events.0 = categories.len(),
            ScanEvent::Progress { .. } => events.1 += 1,
            ScanEvent::Category { report } => {
                assert!(report.complete);
                events.2 += 1;
            }
        },
        &cancel,
    );
    assert_eq!(events.0, CategoryId::ALL.len());
    assert_eq!(
        events.2,
        CategoryId::ALL.len(),
        "one Category event per category"
    );
    assert!(!r.cancelled);

    let app = cat(&r, CategoryId::AppCaches);
    assert_eq!(labels(app), vec!["Google", "com.example.editor"]);
    assert!(app.items.iter().all(|i| i.default_selected));
    assert_eq!(app.items[0].label, "com.example.editor", "largest first");
    assert_eq!(app.items[0].files, 2);
    assert_eq!(
        skipped(app, "com.example.running"),
        SkipReason::InUse {
            app: "Running".into()
        }
    );
    assert_eq!(
        skipped(app, "com.example.running.ShipIt"),
        SkipReason::InUse {
            app: "Running".into()
        }
    );
    assert_eq!(skipped(app, "linked-out"), SkipReason::Symlink);
    let in_use = app
        .skipped
        .iter()
        .find(|s| s.label == "com.example.running")
        .unwrap();
    assert!(
        in_use.bytes.unwrap() >= 40_000,
        "in-use caches are measured so the UI can say what quitting frees"
    );

    let sys = cat(&r, CategoryId::SystemCaches);
    assert!(!sys.info.default_selected);
    assert_eq!(labels(sys), vec!["GeoServices", "com.apple.helpd"]);
    assert!(sys.items.iter().all(|i| !i.default_selected));
    assert_eq!(sys.selected_bytes, 0);
    for essential in ["CloudKit", "com.apple.bird", "com.apple.nsurlsessiond"] {
        assert_eq!(skipped(sys, essential), SkipReason::Essential);
    }

    let logs = cat(&r, CategoryId::Logs);
    assert_eq!(
        labels(logs),
        vec![
            "Crash report: Other-2024-02-01-000000.ips",
            "Crash report: SomeApp-2024-01-01-000000.ips",
            "SomeApp"
        ]
    );

    assert_eq!(
        labels(cat(&r, CategoryId::XcodeDerivedData)),
        vec!["App-abc"]
    );

    let arch = cat(&r, CategoryId::XcodeArchives);
    assert_eq!(labels(arch), vec!["Old App"]);
    assert!(!arch.items[0].default_selected);
    assert_eq!(skipped(arch, "New App"), SkipReason::Recent);

    let ds = cat(&r, CategoryId::XcodeDeviceSupport);
    assert_eq!(labels(ds), vec!["iOS 16.0 (20A362)", "iOS 17.0 (21A329)"]);
    assert_eq!(
        skipped(ds, "iOS 18.0 (22A3354)"),
        SkipReason::Newest,
        "the newest version is kept even if old"
    );
    assert_eq!(skipped(ds, "iOS 17.4 (21E219)"), SkipReason::Recent);

    let sims = cat(&r, CategoryId::XcodeSimulators);
    let mut sim_paths: Vec<String> = sims
        .items
        .iter()
        .map(|i| i.path.strip_prefix(&f.home).unwrap().display().to_string())
        .collect();
    sim_paths.sort();
    assert_eq!(
        sim_paths,
        vec![
            "Library/Developer/CoreSimulator/Caches/dyld/24A100",
            "Library/Developer/CoreSimulator/Caches/dyld/25A100/com.apple.CoreSimulator.SimRuntime.iOS-16-0.20A1",
            "Library/Developer/CoreSimulator/Devices/BBBB",
            "Library/Developer/CoreSimulator/Devices/CCCC",
        ]
    );

    let dev = cat(&r, CategoryId::DeveloperCaches);
    assert_eq!(dev.items.len(), 11);
    let pnpm = dev.items.iter().find(|i| i.label == "pnpm store").unwrap();
    assert!(!pnpm.default_selected && pnpm.note.is_some());
    assert!(
        dev.items
            .iter()
            .filter(|i| i.label != "pnpm store")
            .all(|i| i.default_selected)
    );
    // Developer caches under ~/Library/Caches are not listed twice.
    assert!(!labels(app).contains(&"Homebrew") && !labels(app).contains(&"pip"));

    assert_eq!(cat(&r, CategoryId::MailDownloads).items.len(), 1);

    let backups = cat(&r, CategoryId::IosBackups);
    assert_eq!(labels(backups), vec!["Old Phone backup"]);
    assert!(!backups.items[0].default_selected);
    assert_eq!(skipped(backups, "New Phone backup"), SkipReason::Recent);

    let trash = cat(&r, CategoryId::Trash);
    assert_eq!(trash.info.action, CategoryAction::EmptyTrashInFinder);
    assert_eq!(trash.items.len(), 1);
    assert!(trash.total_bytes >= 44_000);
    assert_eq!(trash.selected_bytes, 0);

    // Totals: the Trash isn't cleanable; unticked categories aren't in selected_bytes.
    let sum: u64 = r
        .categories
        .iter()
        .filter(|c| c.info.id != CategoryId::Trash)
        .map(|c| c.total_bytes)
        .sum();
    assert_eq!(r.total_bytes, sum);
    assert!(r.selected_bytes < r.total_bytes);
    // The symlinked folder outside the home was never measured.
    assert!(r.total_bytes < 2_000_000);

    // Results serialize for the UI.
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"id\":\"app_caches\"") && json.contains("\"risk\":\"high\""));
}

#[test]
fn hard_links_are_counted_once() {
    let f = Fixture::new("hardlink");
    let a = f.file(f.h("Library/Caches/com.example.one/big"), 400_000);
    f.dir(f.h("Library/Logs/OneApp"));
    fs::hard_link(&a, f.h("Library/Logs/OneApp/big-link")).unwrap();
    fs::hard_link(&a, f.h("Library/Caches/com.example.one/big-link")).unwrap();
    let r = scan(&f.env());
    let caches = cat(&r, CategoryId::AppCaches).total_bytes;
    let logs = cat(&r, CategoryId::Logs).total_bytes;
    assert_eq!(caches + logs, alloc(&a), "three links, one file");
}

#[test]
fn permission_errors_are_reported() {
    let f = Fixture::new("perm");
    f.file(f.h("Library/Caches/com.example.locked/open"), 10_000);
    let locked = f.dir(f.h("Library/Caches/com.example.locked/private"));
    f.file(locked.join("secret"), 10_000);
    chmod(&locked, 0o000);
    let backups = f.dir(f.h("Library/Application Support/MobileSync/Backup"));
    chmod(&backups, 0o000);
    let r = scan(&f.env());
    let item = cat(&r, CategoryId::AppCaches)
        .items
        .iter()
        .find(|i| i.label == "com.example.locked")
        .unwrap()
        .clone();
    assert_eq!(item.unreadable, 1);
    assert_eq!(item.files, 1);
    assert!(
        cat(&r, CategoryId::IosBackups).root_unreadable,
        "an unreadable root means Full Disk Access is needed"
    );
    chmod(&locked, 0o755);
    chmod(&backups, 0o755);
}

#[test]
fn roots_that_leave_the_home_are_refused() {
    let f = Fixture::new("escape");
    let elsewhere = f.root.join("external-cargo");
    f.file(elsewhere.join("registry/cache/index/crate.crate"), 50_000);
    std::os::unix::fs::symlink(&elsewhere, f.h(".cargo")).unwrap();
    let r = scan(&f.env());
    let dev = cat(&r, CategoryId::DeveloperCaches);
    assert!(dev.items.is_empty());
    assert_eq!(dev.skipped[0].reason, SkipReason::OutsideHome);
}

#[test]
fn clean_moves_only_valid_selected_items() {
    let f = Fixture::new("clean");
    let outside = populate(&f);
    let env = env_with_running(&f);
    let r = scan(&env);
    let opts = ScanOptions::default();
    let app = cat(&r, CategoryId::AppCaches);
    let editor = app
        .items
        .iter()
        .find(|i| i.label == "com.example.editor")
        .unwrap()
        .clone();
    let google = app
        .items
        .iter()
        .find(|i| i.label == "Google")
        .unwrap()
        .clone();
    let trash_item = cat(&r, CategoryId::Trash).items[0].clone();
    let derived = cat(&r, CategoryId::XcodeDerivedData).items[0].clone();

    // A forged item pointing outside the home, one at a protected folder, one for an essential
    // cache and one for the running app's cache: none may move.
    let forged = |category, path: PathBuf| JunkItem {
        category,
        path,
        ..editor.clone()
    };
    let selected = vec![
        editor.clone(),
        trash_item.clone(),
        forged(CategoryId::AppCaches, outside.join("huge")),
        forged(CategoryId::AppCaches, f.h("Library/Caches")),
        forged(
            CategoryId::AppCaches,
            f.h("Library/Caches/../Caches/com.example.editor"),
        ),
        forged(CategoryId::SystemCaches, f.h("Library/Caches/CloudKit")),
        forged(
            CategoryId::AppCaches,
            f.h("Library/Caches/com.example.running"),
        ),
        forged(CategoryId::AppCaches, f.h("Library/Caches/linked-out")),
    ];
    // Xcode starts after the scan: its build data must now be skipped.
    let mut env2 = env.clone();
    env2.running = std::sync::Arc::new(kipimo_clean::StaticRunningApps(vec![
        kipimo_clean::RunningApp {
            bundle_path: f.s("Applications/Xcode.app"),
            bundle_id: Some("com.apple.dt.Xcode".into()),
            name: Some("Xcode".into()),
            executable: Some("Xcode".into()),
        },
    ]));
    let mut ticks = 0;
    let cancel = AtomicBool::new(false);
    let done = clean(&env, &selected, &opts, &mut |_| ticks += 1, &cancel);
    let xcode = clean(
        &env2,
        std::slice::from_ref(&derived),
        &opts,
        &mut |_| {},
        &cancel,
    );

    let outcome = |i: usize| done.results[i].outcome.clone();
    assert!(matches!(
        outcome(0),
        ItemOutcome::Moved {
            trash_path: Some(_)
        }
    ));
    assert_eq!(
        outcome(1),
        ItemOutcome::Skipped {
            reason: SkipReason::ReportOnly
        }
    );
    assert_eq!(
        outcome(2),
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert_eq!(
        outcome(3),
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert_eq!(
        outcome(4),
        ItemOutcome::Skipped {
            reason: SkipReason::NotACandidate
        }
    );
    assert_eq!(
        outcome(5),
        ItemOutcome::Skipped {
            reason: SkipReason::Essential
        }
    );
    assert_eq!(
        outcome(6),
        ItemOutcome::Skipped {
            reason: SkipReason::InUse {
                app: "Running".into()
            }
        }
    );
    assert_eq!(
        outcome(7),
        ItemOutcome::Skipped {
            reason: SkipReason::Symlink
        }
    );
    assert_eq!(
        xcode.results[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::InUse {
                app: "Xcode".into()
            }
        }
    );
    assert!(ticks >= selected.len());

    assert_eq!(done.moved_count, 1);
    assert_eq!(done.moved_bytes, editor.bytes);
    assert_eq!(f.trash.moved(), vec![editor.path.clone()]);
    assert!(!editor.path.exists());
    assert!(google.path.exists(), "unselected items stay");
    assert!(outside.join("huge").exists());
    assert!(f.h("Library/Caches/CloudKit/state").exists());
    assert!(f.h(".Trash/old.zip").exists());
    assert!(derived.path.exists());

    // A second clean of the same item: it's gone now.
    let again = clean(
        &env,
        std::slice::from_ref(&editor),
        &opts,
        &mut |_| {},
        &cancel,
    );
    assert_eq!(
        again.results[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::Missing
        }
    );
}

#[test]
fn read_only_and_root_owned_items_are_reported_not_forced() {
    let f = Fixture::new("readonly");
    let ro = f.dir(f.h("Library/Caches/com.example.readonly"));
    f.file(ro.join("x"), 5_000);
    let r = scan(&f.env());
    let item = cat(&r, CategoryId::AppCaches).items[0].clone();
    chmod(&ro, 0o555);
    let cancel = AtomicBool::new(false);
    let done = clean(
        &f.env(),
        &[item],
        &ScanOptions::default(),
        &mut |_| {},
        &cancel,
    );
    chmod(&ro, 0o755);
    assert_eq!(
        done.results[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::ReadOnly
        }
    );
    assert!(ro.exists());
    assert!(f.trash.moved().is_empty());
}

#[test]
fn the_default_environment_never_moves_anything() {
    let f = Fixture::new("notrash");
    f.file(f.h("Library/Caches/com.example.a/x"), 5_000);
    let env = Env::new(&f.home, &f.sys);
    let r = scan(&env);
    let item = cat(&r, CategoryId::AppCaches).items[0].clone();
    let cancel = AtomicBool::new(false);
    let done = clean(
        &env,
        std::slice::from_ref(&item),
        &ScanOptions::default(),
        &mut |_| {},
        &cancel,
    );
    assert!(matches!(
        done.results[0].outcome,
        ItemOutcome::Failed { .. }
    ));
    assert!(item.path.exists());
}

#[test]
fn cancel_returns_partial_results() {
    let f = Fixture::new("cancel");
    populate(&f);
    let cancel = AtomicBool::new(true);
    let r = scan_junk(&f.env(), &ScanOptions::default(), &mut |_| {}, &cancel);
    assert!(r.cancelled);
    assert_eq!(r.categories.len(), CategoryId::ALL.len());
    let item = cat(&r, CategoryId::AppCaches).items[0].clone();
    let done = clean(
        &f.env(),
        std::slice::from_ref(&item),
        &ScanOptions::default(),
        &mut |_| {},
        &cancel,
    );
    assert_eq!(
        done.results[0].outcome,
        ItemOutcome::Skipped {
            reason: SkipReason::Cancelled
        }
    );
    assert!(done.cancelled && item.path.exists());
}

#[test]
fn unknown_runtimes_mean_no_unavailable_devices() {
    let f = Fixture::new("noruntimes");
    let d = f.h("Library/Developer/CoreSimulator/Devices/DDDD");
    plist(
        &d.join("device.plist"),
        &[
            ("name", V::S("iPad")),
            (
                "runtime",
                V::S("com.apple.CoreSimulator.SimRuntime.iOS-17-0"),
            ),
        ],
    );
    f.file(d.join("data/f"), 5_000);
    let r = scan(&f.env());
    assert!(
        cat(&r, CategoryId::XcodeSimulators).items.is_empty(),
        "without runtime metadata nothing is called unavailable"
    );
}

#[test]
fn a_subset_of_categories_can_be_scanned() {
    let f = Fixture::new("subset");
    populate(&f);
    let opts = ScanOptions {
        categories: vec![CategoryId::Trash, CategoryId::Logs],
        ..ScanOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let r = scan_junk(&f.env(), &opts, &mut |_| {}, &cancel);
    let ids: Vec<CategoryId> = r.categories.iter().map(|c| c.info.id).collect();
    assert_eq!(
        ids,
        vec![CategoryId::Logs, CategoryId::Trash],
        "display order"
    );
}
