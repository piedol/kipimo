//! The real Trash on a throwaway disk image: every item `clean`, `uninstall` and
//! `remove_leftovers` move must get Finder's Put Back record, including the ones `NSFileManager`
//! writes seconds after the move. Nothing here touches the user's Trash or home folder.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::*;
use kipimo_clean::{
    AppIndex, AppTarget, CategoryId, ItemOutcome, ItemResult, ScanOptions, SystemTrash, clean,
    find_leftovers, remove_leftovers, scan_junk, uninstall,
};

fn trashed(results: &[ItemResult], image: &DiskImage) -> Vec<PathBuf> {
    results
        .iter()
        .map(|r| match &r.outcome {
            ItemOutcome::Moved {
                trash_path: Some(p),
            } => {
                assert!(image.holds(p), "{} left the disk image", p.display());
                p.clone()
            }
            other => panic!("{}: {other:?}", r.path.display()),
        })
        .collect()
}

#[test]
fn every_moved_item_gets_its_put_back_record() {
    let host = Fixture::new("dmg-host");
    let Some(image) = DiskImage::attach(
        &host.root.join("image.dmg"),
        &host.root.join("mnt"),
        "KipimoCleanTrash",
    ) else {
        eprintln!("hdiutil unavailable; skipping the disk image Put Back test");
        return;
    };
    let f = Fixture::in_dir(&image.mount, "putback");
    assert!(image.holds(&f.home));
    let mut env = f.env();
    env.trash = Arc::new(SystemTrash);
    let cancel = AtomicBool::new(false);

    // Clean up: five app caches in one go.
    for i in 0..5 {
        f.file(
            f.h(&format!("Library/Caches/com.example.app{i}/data.bin")),
            100_000,
        );
    }
    let opts = ScanOptions::default();
    let report = scan_junk(&env, &opts, &mut |_| {}, &cancel);
    let caches = report
        .categories
        .iter()
        .find(|c| c.info.id == CategoryId::AppCaches)
        .unwrap();
    assert_eq!(caches.items.len(), 5);
    let done = clean(&env, &caches.items, &opts, &mut |_| {}, &cancel);
    let mut moved = trashed(&done.results, &image);

    // Uninstall: the app and its leftovers.
    let app = f.app(
        f.s("Applications/Widget.app"),
        "com.example.widget",
        "Widget",
        "1.0",
    );
    f.file(f.h("Library/Application Support/Widget/db.sqlite"), 20_000);
    f.file(f.h("Library/Caches/com.example.widget/cache.db"), 20_000);
    f.file(f.h("Library/Preferences/com.example.widget.plist"), 500);
    let index = AppIndex::build(&env, &cancel);
    let target = AppTarget::from_bundle(&env, &app).unwrap();
    let found = find_leftovers(&env, &target, &index, &cancel);
    assert!(found.leftovers.len() >= 3, "{:?}", found.leftovers);
    let gone = uninstall(&env, &app, 50_000, &found.leftovers, &mut |_| {}, &cancel);
    moved.extend(trashed(std::slice::from_ref(&gone.app), &image));
    moved.extend(trashed(&gone.leftovers, &image));

    // Leftover review after an app was dragged to the Trash.
    let other = f.app(
        f.s("Applications/Gadget.app"),
        "com.example.gadget",
        "Gadget",
        "1.0",
    );
    f.file(f.h("Library/Caches/com.example.gadget/c.bin"), 20_000);
    f.file(
        f.h("Library/Saved Application State/com.example.gadget.savedState/w.data"),
        500,
    );
    let target = AppTarget::from_bundle(&env, &other).unwrap();
    let index = AppIndex::build(&env, &cancel);
    let found = find_leftovers(&env, &target, &index, &cancel);
    assert!(found.leftovers.len() >= 2, "{:?}", found.leftovers);
    let removed = remove_leftovers(&env, &target, &found.leftovers, &mut |_, _| {}, &cancel);
    moved.extend(trashed(&removed, &image));

    assert!(moved.len() >= 11, "{moved:?}");
    let missing = missing_put_back(&moved, Duration::from_secs(15));
    assert!(missing.is_empty(), "no Put Back record for {missing:?}");
    drop(f);
    drop(image);
}
