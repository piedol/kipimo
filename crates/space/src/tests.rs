//! Move-to-Trash logic on synthetic folders, with a stand-in mover that renames items into a
//! temporary folder: these tests never touch the real Trash.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::{EntryKind, ScanOptions, SpaceMap, TrashOutcome, TrashSkip};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Temp {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = base.join(format!(
            "kipimo-space-unit-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Temp(path)
    }

    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![1u8; len]).unwrap();
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Renames items into `bin` (a stand-in for the Trash) and records what it was asked to move.
fn fake_trash<'a>(
    bin: &'a Path,
    calls: &'a mut Vec<PathBuf>,
) -> impl FnMut(&Path) -> Result<Option<PathBuf>, String> + 'a {
    move |path| {
        calls.push(path.to_path_buf());
        let target = bin.join(path.file_name().unwrap());
        std::fs::rename(path, &target).map_err(|e| e.to_string())?;
        Ok(Some(target))
    }
}

fn scanned(root: &Path) -> SpaceMap {
    let map = SpaceMap::new(root, ScanOptions::default()).unwrap();
    map.scan(&AtomicBool::new(false), |_| {}).unwrap();
    map
}

#[test]
fn moves_selected_items_and_updates_totals_without_rescan() {
    let root = Temp::new("move");
    let outside = Temp::new("move-bin");
    let big = root.file("downloads/big.bin", 3_000_000);
    root.file("downloads/keep.bin", 400_000);
    root.file("old/a.bin", 700_000);
    root.file("old/deeper/b.bin", 600_000);
    let map = scanned(&root.0);
    let before = map.totals();
    let old_size = map
        .folder(map.find(root.0.join("old")).unwrap())
        .unwrap()
        .size;
    let big_size = map
        .children(map.find(root.0.join("downloads")).unwrap())
        .into_iter()
        .find(|e| e.name == "big.bin")
        .unwrap()
        .size;

    let mut calls = Vec::new();
    let results = map.move_to_trash_with(
        &[big.clone(), root.0.join("old")],
        &mut fake_trash(&outside.0, &mut calls),
    );
    // Shallowest first (so an enclosing folder goes before items inside it); results keep the
    // caller's order.
    assert_eq!(calls, vec![root.0.join("old"), big.clone()]);
    assert_eq!(
        results[0].outcome,
        TrashOutcome::Moved {
            trashed_to: Some(outside.0.join("big.bin")),
            bytes: big_size,
            files: 1,
        }
    );
    assert!(matches!(
        results[1].outcome,
        TrashOutcome::Moved { bytes, files: 2, .. } if bytes == old_size
    ));

    let after = map.totals();
    assert_eq!(after.bytes, before.bytes - big_size - old_size);
    assert_eq!(after.files, before.files - 3);
    assert_eq!(after.folders, before.folders - 2);
    let names: Vec<String> = map
        .children(map.root())
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["downloads"]);
    let downloads = map.find(root.0.join("downloads")).unwrap();
    assert!(map.children(downloads).iter().all(|e| e.name != "big.bin"));
    assert!(
        map.largest_files(map.root(), 10)
            .iter()
            .all(|f| f.path != big)
    );
    let rows = map.children(map.root());
    let sum: u64 = rows.iter().map(|r| r.size).sum();
    assert_eq!(sum, after.bytes);
}

#[test]
fn refuses_everything_it_should() {
    let root = Temp::new("refuse");
    let outside = Temp::new("refuse-outside");
    let bin = Temp::new("refuse-bin");
    let stranger = outside.file("not-mine.bin", 10_000);
    let in_bundle = root.file("Tool.app/Contents/Resources/data.bin", 500_000);
    let folder = root.0.join("pair");
    let inner = root.file("pair/inner.bin", 300_000);
    let map = scanned(&root.0);
    // Created after the scan: not part of its results.
    let late = root.file("late/new.bin", 1_000);

    let mut calls = Vec::new();
    let selected = vec![
        stranger.clone(),
        root.0.clone(),
        PathBuf::from("relative/path"),
        root.0.join("pair/../pair/inner.bin"),
        root.0.join("missing.bin"),
        in_bundle.clone(),
        late.clone(),
        inner.clone(),
        folder.clone(),
        folder.clone(),
    ];
    let results = map.move_to_trash_with(&selected, &mut fake_trash(&bin.0, &mut calls));
    let outcomes: Vec<&TrashOutcome> = results.iter().map(|r| &r.outcome).collect();
    assert_eq!(
        outcomes[0],
        &TrashOutcome::Skipped(TrashSkip::OutsideScannedRoot)
    );
    assert_eq!(outcomes[1], &TrashOutcome::Skipped(TrashSkip::ScanRoot));
    assert_eq!(outcomes[2], &TrashOutcome::Skipped(TrashSkip::InvalidPath));
    assert_eq!(outcomes[3], &TrashOutcome::Skipped(TrashSkip::InvalidPath));
    assert_eq!(outcomes[4], &TrashOutcome::Skipped(TrashSkip::NotFound));
    assert_eq!(outcomes[5], &TrashOutcome::Skipped(TrashSkip::InsideBundle));
    assert_eq!(outcomes[6], &TrashOutcome::Skipped(TrashSkip::NotInScan));
    assert_eq!(
        outcomes[7],
        &TrashOutcome::Skipped(TrashSkip::InsideAnotherSelectedItem)
    );
    assert!(matches!(outcomes[8], TrashOutcome::Moved { .. }));
    assert_eq!(
        outcomes[9],
        &TrashOutcome::Skipped(TrashSkip::InsideAnotherSelectedItem)
    );
    // Only the one allowed item reached the mover.
    assert_eq!(calls, vec![folder]);
    assert!(stranger.exists() && in_bundle.exists() && late.exists());
}

#[test]
fn failures_leave_the_tree_untouched() {
    let root = Temp::new("fail");
    let file = root.file("x/a.bin", 200_000);
    let map = scanned(&root.0);
    let before = map.totals();
    let results = map.move_to_trash_with(std::slice::from_ref(&file), &mut |_: &Path| {
        Err("not allowed".to_string())
    });
    assert_eq!(
        results[0].outcome,
        TrashOutcome::Failed {
            error: "not allowed".to_string()
        }
    );
    assert_eq!(map.totals(), before);
    assert!(file.exists());
}

#[test]
fn refuses_while_a_scan_runs() {
    let root = Temp::new("running");
    let file = root.file("a.bin", 1_000);
    let map = scanned(&root.0);
    map.shared.scanning.store(true, Ordering::Release);
    let mut called = false;
    let results = map.move_to_trash_with(std::slice::from_ref(&file), &mut |_: &Path| {
        called = true;
        Ok(None)
    });
    assert_eq!(
        results[0].outcome,
        TrashOutcome::Skipped(TrashSkip::ScanRunning)
    );
    assert!(!called);
    map.shared.scanning.store(false, Ordering::Release);
}

#[test]
fn items_moved_into_a_trash_folder_inside_the_root_still_count_there() {
    // Scanning the home folder includes ~/.Trash: moving an item there does not free space
    // until the Trash is emptied, and the totals say so.
    let root = Temp::new("inner-trash");
    let file = root.file("docs/report.bin", 1_000_000);
    root.file("bin-folder/placeholder.txt", 10);
    let map = scanned(&root.0);
    let before = map.totals();
    let bin = root.0.join("bin-folder");
    let mut calls = Vec::new();
    let results = map.move_to_trash_with(&[file], &mut fake_trash(&bin, &mut calls));
    assert!(matches!(results[0].outcome, TrashOutcome::Moved { .. }));
    let after = map.totals();
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.files, before.files);
    let bin_node = map.find(&bin).unwrap();
    let rows = map.children(bin_node);
    let other = rows
        .iter()
        .find(|r| r.kind == EntryKind::OtherFiles)
        .unwrap();
    assert!(other.size >= 1_000_000);
}

/// Run by the test below as the "program inside an app": this test binary, copied into a
/// bundle (it is validly ad-hoc signed, unlike a copy of a system binary, which macOS freezes).
#[test]
#[ignore]
fn sleeper_for_the_running_check() {
    if std::env::var_os("KIPIMO_TEST_SLEEPER").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

#[test]
fn refuses_a_folder_with_a_running_program_inside() {
    let root = Temp::new("running");
    let bin = Temp::new("running-bin");
    let exe = root.0.join("Sleepy.app/Contents/MacOS/sleepy");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let mut child = std::process::Command::new(&exe)
        .args([
            "--exact",
            "tests::sleeper_for_the_running_check",
            "--ignored",
            "-q",
        ])
        .env("KIPIMO_TEST_SLEEPER", "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    root.file("idle/data.bin", 10_000);
    let map = scanned(&root.0);
    let mut calls = Vec::new();
    let selected = vec![root.0.join("Sleepy.app"), root.0.join("idle")];
    let results = map.move_to_trash_with(&selected, &mut fake_trash(&bin.0, &mut calls));
    // It ends on its own (no signal, so no crash report anywhere).
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_eq!(
        results[0].outcome,
        TrashOutcome::Skipped(TrashSkip::Running)
    );
    assert!(matches!(results[1].outcome, TrashOutcome::Moved { .. }));
    assert!(root.0.join("Sleepy.app").exists());
}
