//! Scanner tests on synthetic folders in a temporary directory.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{DiskImage, TempTree, deep_nest, missing_put_back, reference_allocated};
use kipimo_space::{
    EntryKind, NodeState, ScanOptions, SkipReason, SpaceMap, TargetKind, TrashOutcome, TrashSkip,
    scan_targets,
};

fn scan(root: &std::path::Path, options: ScanOptions) -> SpaceMap {
    let map = SpaceMap::new(root, options).unwrap();
    let summary = map.scan(&AtomicBool::new(false), |_| {}).unwrap();
    assert!(!summary.cancelled);
    map
}

#[test]
fn totals_match_lstat_reference_and_listing_is_sorted() {
    let tree = TempTree::new("totals");
    tree.file("a/big.bin", 3_000_000);
    tree.file("a/small.txt", 10);
    tree.file("a/b/medium.bin", 800_000);
    tree.file("c/one.bin", 1_200_000);
    for i in 0..30 {
        tree.file(&format!("c/many/{i}.txt"), 100 + i);
    }
    tree.dir("empty");

    let map = scan(&tree.root, ScanOptions::default());
    let (bytes, files, folders) = reference_allocated(&tree.root);
    let totals = map.totals();
    assert!(totals.complete);
    assert_eq!(totals.bytes, bytes);
    assert_eq!(totals.files, files);
    assert_eq!(totals.folders, folders);

    let rows = map.children(map.root());
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a", "c", "empty"]);
    assert!(rows.windows(2).all(|w| w[0].size >= w[1].size));
    let fractions: f64 = rows.iter().map(|r| r.fraction_of_parent).sum();
    assert!((fractions - 1.0).abs() < 1e-9);

    // Folder "a": its big file, subfolder "b", and one row for the small file.
    let a = rows[0].node.unwrap();
    let a_rows = map.children(a);
    assert_eq!(a_rows[0].kind, EntryKind::File);
    assert_eq!(a_rows[0].name, "big.bin");
    assert_eq!(
        a_rows[0].path.as_deref(),
        Some(tree.path("a/big.bin").as_path())
    );
    assert_eq!(a_rows[1].kind, EntryKind::Folder);
    let other = a_rows.last().unwrap();
    assert_eq!(other.kind, EntryKind::OtherFiles);
    assert_eq!(other.files, 1);

    // Path, parent, breadcrumb and find agree.
    assert_eq!(map.path(a).unwrap(), tree.path("a"));
    assert_eq!(map.parent(a), Some(map.root()));
    let b = map.find(tree.path("a/b")).unwrap();
    assert_eq!(map.ancestors(b), vec![map.root(), a, b]);
    let fraction = map.fraction_of_parent(b).unwrap();
    assert!(fraction > 0.0 && fraction < 1.0);

    // Largest files anywhere under the root.
    let top = map.largest_files(map.root(), 3);
    let top_names: Vec<&str> = top.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(top_names, ["big.bin", "one.bin", "medium.bin"]);
    assert_eq!(top[2].path, tree.path("a/b/medium.bin"));

    // Results serialize for the UI.
    let json = serde_json::to_string(&rows).unwrap();
    assert!(json.contains("\"Folder\""));
}

#[test]
fn listed_files_are_bounded_per_folder() {
    let tree = TempTree::new("bounded");
    for i in 0..50 {
        tree.file(&format!("lots/{i:02}.bin"), 300_000 + i * 4096);
    }
    let options = ScanOptions {
        files_per_folder: 5,
        min_listed_file_size: 1,
        ..ScanOptions::default()
    };
    let map = scan(&tree.root, options);
    let lots = map.find(tree.path("lots")).unwrap();
    let rows = map.children(lots);
    let files: Vec<_> = rows.iter().filter(|r| r.kind == EntryKind::File).collect();
    assert_eq!(files.len(), 5);
    assert_eq!(files[0].name, "49.bin");
    let other = rows
        .iter()
        .find(|r| r.kind == EntryKind::OtherFiles)
        .unwrap();
    assert_eq!(other.files, 45);
    let (_, listed, _) = map.memory_stats();
    assert_eq!(listed, 5);
    let sum: u64 = rows.iter().map(|r| r.size).sum();
    assert_eq!(sum, map.folder(lots).unwrap().size);
}

#[test]
fn sparse_files_count_allocated_bytes_not_length() {
    let tree = TempTree::new("sparse");
    let path = tree.path("sparse.img");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(4 << 30).unwrap();
    use std::os::unix::fs::FileExt;
    file.write_at(b"end", (4 << 30) - 3).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 4 << 30);

    let map = scan(&tree.root, ScanOptions::default());
    let (bytes, _, _) = reference_allocated(&tree.root);
    assert_eq!(map.totals().bytes, bytes);
    assert!(
        map.totals().bytes < 1 << 20,
        "a 4 GiB sparse file uses a few KB"
    );
}

#[test]
fn hard_links_count_once() {
    let tree = TempTree::new("hardlinks");
    let original = tree.file("one/data.bin", 2_000_000);
    tree.dir("two");
    tree.dir("three");
    std::fs::hard_link(&original, tree.path("two/link.bin")).unwrap();
    std::fs::hard_link(&original, tree.path("three/link.bin")).unwrap();
    std::fs::hard_link(&original, tree.path("one/same-folder-link.bin")).unwrap();
    let blocks = std::fs::metadata(&original).unwrap().blocks() * 512;

    let map = scan(&tree.root, ScanOptions::default());
    let totals = map.totals();
    assert_eq!(totals.files, 4);
    let (bytes, _, _) = reference_allocated(&tree.root);
    assert_eq!(totals.bytes, bytes);
    assert!(
        totals.bytes < blocks * 2,
        "the data is counted once, not four times"
    );
}

#[test]
fn symlink_loops_and_links_are_not_followed() {
    let tree = TempTree::new("symlinks");
    let outside = TempTree::new("symlinks-outside");
    outside.file("huge.bin", 5_000_000);
    tree.file("real/file.bin", 50_000);
    std::os::unix::fs::symlink(&tree.root, tree.path("real/loop-to-root")).unwrap();
    std::os::unix::fs::symlink(".", tree.path("real/loop-to-self")).unwrap();
    std::os::unix::fs::symlink("../real", tree.path("real/loop-to-parent")).unwrap();
    std::os::unix::fs::symlink(outside.path("huge.bin"), tree.path("link-to-huge")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/target", tree.path("dangling")).unwrap();

    let map = scan(&tree.root, ScanOptions::default());
    let totals = map.totals();
    assert!(totals.complete);
    assert_eq!(totals.folders, 1);
    // Six symbolic links and files, each counted as a small item of its own.
    assert_eq!(totals.files, 6);
    assert!(
        totals.bytes < 1_000_000,
        "the 5 MB target outside is not counted"
    );
    let (bytes, _, _) = reference_allocated(&tree.root);
    assert_eq!(totals.bytes, bytes);
}

#[test]
fn deep_nesting_beyond_path_max() {
    let tree = TempTree::new("deep");
    // 120 levels of 11 bytes: the deepest path is well over PATH_MAX (1024 bytes).
    deep_nest(&tree.root, "nested-dir", 120, 9_000);
    let options = ScanOptions {
        min_listed_file_size: 1,
        ..ScanOptions::default()
    };
    let map = scan(&tree.root, options);
    let totals = map.totals();
    assert!(totals.complete);
    assert_eq!(totals.folders, 120);
    assert_eq!(totals.files, 1);
    assert!(map.skipped().is_empty(), "{:?}", map.skipped());
    let top = map.largest_files(map.root(), 1);
    assert!(top[0].path.as_os_str().len() > 1024);
    assert!(totals.bytes >= 9_000);
}

#[test]
fn unreadable_folders_are_skipped_and_reported() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores permission bits
    }
    use std::os::unix::fs::PermissionsExt;
    let tree = TempTree::new("denied");
    tree.file("open/a.bin", 100_000);
    tree.file("locked/secret.bin", 900_000);
    let locked = tree.path("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let map = scan(&tree.root, ScanOptions::default());
    let skipped = map.skipped();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].path, locked);
    assert_eq!(skipped[0].reason, SkipReason::PermissionDenied);
    let totals = map.totals();
    assert!(totals.complete);
    assert_eq!(totals.skipped_folders, 1);
    assert!(totals.bytes < 900_000);
    let row = map
        .children(map.root())
        .into_iter()
        .find(|r| r.name == "locked")
        .unwrap();
    assert_eq!(row.state, NodeState::Skipped(SkipReason::PermissionDenied));
    assert!(row.complete);
}

#[test]
fn unicode_and_odd_names_round_trip() {
    let tree = TempTree::new("unicode");
    let names = [
        "café-nfc",
        "cafe\u{301}-nfd",
        "日本語のフォルダ",
        "😀 emoji",
        "tab\there",
        "new\nline",
        " leading and trailing ",
        "Ωmega–dash—quote\"'",
    ];
    for (i, name) in names.iter().enumerate() {
        tree.file(&format!("{name}/{name}.bin"), 300_000 + i * 10_000);
    }
    let long = "x".repeat(255);
    tree.file(&format!("long/{long}"), 400_000);

    let map = scan(&tree.root, ScanOptions::default());
    for row in map.children(map.root()) {
        let path: PathBuf = row.path.clone().unwrap();
        assert!(std::fs::symlink_metadata(&path).is_ok(), "{path:?} exists");
        let node = row.node.unwrap();
        assert_eq!(map.find(&path), Some(node));
        for child in map.children(node) {
            if let Some(file) = child.path {
                assert!(std::fs::symlink_metadata(&file).is_ok(), "{file:?} exists");
            }
        }
    }
    assert!(
        map.children(map.root())
            .iter()
            .any(|r| r.name == "😀 emoji")
    );
    assert_eq!(map.largest_files(map.root(), 20).len(), names.len() + 1);
}

#[test]
fn progress_streams_and_partial_results_are_queryable() {
    let tree = TempTree::new("progress");
    for i in 0..40 {
        for j in 0..40 {
            tree.file(&format!("d{i}/e{j}/f.bin"), 5_000);
        }
    }
    let options = ScanOptions {
        progress_interval_ms: 10,
        threads: 2,
        ..ScanOptions::default()
    };
    let map = SpaceMap::new(&tree.root, options).unwrap();
    let cancel = AtomicBool::new(false);
    let mut updates = Vec::new();
    let reader = map.clone();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Query while the scan runs: never blocks for long, never panics.
            while !done.load(Ordering::Relaxed) {
                let _ = reader.children(reader.root());
                let _ = reader.totals();
                let _ = reader.largest_files(reader.root(), 5);
            }
        });
        map.scan(&cancel, |p| updates.push(p.clone())).unwrap();
        done.store(true, Ordering::Relaxed);
    });
    let last = updates.last().unwrap();
    assert!(last.finished);
    assert!(updates.windows(2).all(|w| w[0].bytes <= w[1].bytes));
    assert_eq!(last.files, 1600);
    assert_eq!(map.totals().files, 1600);
    assert_eq!(map.totals().folders, 40 + 1600);
}

#[test]
fn cancellation_keeps_partial_results() {
    let tree = TempTree::new("cancel");
    for i in 0..20 {
        tree.file(&format!("d{i}/f.bin"), 5_000);
    }
    let map = SpaceMap::new(&tree.root, ScanOptions::default()).unwrap();
    let cancelled = AtomicBool::new(true);
    let summary = map.scan(&cancelled, |_| {}).unwrap();
    assert!(summary.cancelled);
    assert!(!map.totals().complete);
    assert_eq!(map.folder(map.root()).unwrap().state, NodeState::Pending);

    // A cancelled map can be scanned again from scratch.
    let summary = map.scan(&AtomicBool::new(false), |_| {}).unwrap();
    assert!(!summary.cancelled);
    assert!(map.totals().complete);
    assert_eq!(map.totals().files, 20);
}

#[test]
fn cancelling_mid_scan_stops_quickly() {
    let tree = TempTree::new("cancel-mid");
    for i in 0..60 {
        for j in 0..30 {
            tree.file(&format!("d{i}/e{j}/f.bin"), 1_000);
        }
    }
    let options = ScanOptions {
        threads: 1,
        progress_interval_ms: 10,
        ..ScanOptions::default()
    };
    let map = SpaceMap::new(&tree.root, options).unwrap();
    let cancel = AtomicBool::new(false);
    let summary = map
        .scan(&cancel, |p| {
            if p.folders > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
    // Either the scan finished before the first progress tick or it was cut short; in both
    // cases the map is consistent and queryable.
    let totals = map.totals();
    assert_eq!(summary.cancelled, !totals.complete);
    let rows = map.children(map.root());
    let sum: u64 = rows.iter().map(|r| r.size).sum();
    assert_eq!(sum, totals.bytes);
}

#[test]
fn rejects_a_file_as_root() {
    let tree = TempTree::new("not-dir");
    let file = tree.file("plain.txt", 10);
    assert!(SpaceMap::new(&file, ScanOptions::default()).is_err());
    assert!(SpaceMap::new(tree.path("missing"), ScanOptions::default()).is_err());
}

#[test]
fn a_second_concurrent_scan_is_refused() {
    let tree = TempTree::new("busy");
    for i in 0..200 {
        tree.file(&format!("d{i}/f.bin"), 1_000);
    }
    let map = SpaceMap::new(&tree.root, ScanOptions::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut refused = None;
    map.scan(&cancel, |_| {
        if refused.is_none() {
            refused = Some(map.scan(&AtomicBool::new(false), |_| {}).is_err());
        }
    })
    .unwrap();
    assert_eq!(refused, Some(true));
    assert!(!map.is_scanning());
}

#[test]
fn startup_disk_is_a_target() {
    let targets = scan_targets();
    let startup = targets
        .iter()
        .find(|t| t.kind == TargetKind::StartupDisk)
        .unwrap();
    assert_eq!(startup.path, PathBuf::from("/"));
    assert!(startup.total_bytes > startup.available_bytes);
}

#[test]
fn mounted_disk_image_is_a_boundary() {
    let tree = TempTree::new("dmg");
    tree.file("local.bin", 200_000);
    let Some(image) = DiskImage::attach(&tree.path("image.dmg"), &tree.path("mnt"), "KipimoScan")
    else {
        eprintln!("hdiutil unavailable; skipping the disk image boundary test");
        return;
    };
    std::fs::write(image.mount.join("inside.bin"), vec![1u8; 3_000_000]).unwrap();

    // By default the mounted volume is listed as skipped and not counted.
    let map = scan(&tree.root, ScanOptions::default());
    let skipped = map.skipped();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0].path, image.mount);
    match &skipped[0].reason {
        SkipReason::OtherVolume {
            fs_type,
            volume_used,
        } => {
            assert_eq!(fs_type, "apfs");
            assert!(volume_used.is_some());
        }
        other => panic!("unexpected {other:?}"),
    }
    let without = map.totals().bytes;

    // With `cross_volumes` it is entered.
    let map = scan(
        &tree.root,
        ScanOptions {
            cross_volumes: true,
            ..ScanOptions::default()
        },
    );
    assert!(map.totals().bytes >= without + 3_000_000);
    let mount_node = map.find(&image.mount).unwrap();
    assert_eq!(map.folder(mount_node).unwrap().state, NodeState::Scanned);
    drop(image);
}

#[test]
fn trash_on_a_disk_image_keeps_put_back_and_updates_totals() {
    // The real NSFileManager path, on a throwaway volume: items go to that volume's own
    // `.Trashes`, never to the user's Trash.
    let tree = TempTree::new("dmg-trash");
    let Some(image) = DiskImage::attach(&tree.path("image.dmg"), &tree.path("mnt"), "KipimoTrash")
    else {
        eprintln!("hdiutil unavailable; skipping the disk image trash test");
        return;
    };
    let folder = image.mount.join("projects/old");
    std::fs::create_dir_all(&folder).unwrap();
    let file = image.mount.join("projects/big.bin");
    std::fs::write(&file, vec![3u8; 2_000_000]).unwrap();
    std::fs::write(folder.join("a.bin"), vec![4u8; 500_000]).unwrap();
    std::fs::write(image.mount.join("keep.bin"), vec![5u8; 300_000]).unwrap();
    let extra: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = image.mount.join(format!("projects/extra {i}.bin"));
            std::fs::write(&p, vec![6u8; 100_000]).unwrap();
            p
        })
        .collect();

    let map = scan(&image.mount, ScanOptions::default());
    let before = map.totals();
    let file_size = std::fs::metadata(&file).unwrap().blocks() * 512;
    let mut picked = vec![file.clone(), folder.clone(), image.mount.clone()];
    picked.extend(extra.iter().cloned());
    let results = map.move_to_trash(&picked);

    let TrashOutcome::Moved {
        trashed_to, bytes, ..
    } = &results[0].outcome
    else {
        panic!("{:?}", results[0]);
    };
    assert_eq!(*bytes, file_size);
    let trashed_to = trashed_to.clone().unwrap();
    assert!(trashed_to.starts_with(image.mount.join(".Trashes")));
    assert!(trashed_to.exists());
    assert!(!file.exists());
    assert!(matches!(results[1].outcome, TrashOutcome::Moved { .. }));
    assert!(!folder.exists());
    assert_eq!(
        results[2].outcome,
        TrashOutcome::Skipped(TrashSkip::ScanRoot)
    );

    let after = map.totals();
    assert!(after.bytes < before.bytes - file_size);
    assert_eq!(after.files, before.files - 5);
    assert!(map.find(&folder).is_none());

    // Finder's Put Back record of every moved item ends up in the volume Trash's .DS_Store
    // (all but the first are written a few seconds later).
    let moved: Vec<PathBuf> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            TrashOutcome::Moved { trashed_to, .. } => trashed_to.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(moved.len(), 5);
    let missing = missing_put_back(&moved, std::time::Duration::from_secs(15));
    assert!(missing.is_empty(), "no Put Back record for {missing:?}");
    drop(image);
}

#[test]
fn apfs_clones_can_be_counted_once() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let tree = TempTree::new("clones");
    let original = tree.file("a/original.bin", 4_000_000);
    tree.dir("b");
    for target in ["a/clone.bin", "b/clone.bin"] {
        let src = CString::new(original.as_os_str().as_bytes()).unwrap();
        let dst = CString::new(tree.path(target).as_os_str().as_bytes()).unwrap();
        if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } != 0 {
            eprintln!("clonefile unsupported here (not APFS); skipping");
            return;
        }
    }
    let one = std::fs::metadata(&original).unwrap().blocks() * 512;

    // Default: like du and Finder, each clone counts in full.
    let map = scan(&tree.root, ScanOptions::default());
    assert!(map.totals().bytes >= 3 * one);
    let (bytes, _, _) = reference_allocated(&tree.root);
    assert_eq!(map.totals().bytes, bytes);

    // Clone-aware: the shared blocks count once.
    let map = scan(
        &tree.root,
        ScanOptions {
            count_clones_once: true,
            ..ScanOptions::default()
        },
    );
    let total = map.totals().bytes;
    assert!(total >= one && total < 2 * one, "{total} vs {one}");
    assert_eq!(map.totals().files, 3);
}
