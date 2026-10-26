//! Read-only measurement: scans each path given and prints time, totals, throughput and memory.
//!
//! cargo run --release -p kipimo-space --example measure -- [--threads N] [--files N]
//!     [--min BYTES] [--cross] [--top N] PATH...

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kipimo_space::{EntryKind, ScanOptions, SkipReason, SpaceMap};

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
}

fn main() {
    let mut options = ScanOptions::default();
    let mut top = 8usize;
    let mut paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => options.threads = args.next().unwrap().parse().unwrap(),
            "--files" => options.files_per_folder = args.next().unwrap().parse().unwrap(),
            "--min" => options.min_listed_file_size = args.next().unwrap().parse().unwrap(),
            "--top" => top = args.next().unwrap().parse().unwrap(),
            "--cross" => options.cross_volumes = true,
            "--clones-once" => options.count_clones_once = true,
            _ => paths.push(arg),
        }
    }
    for path in paths {
        let map = SpaceMap::new(&path, options.clone()).expect("root");
        let cancel = AtomicBool::new(false);
        let mut last = Instant::now();
        let summary = map
            .scan(&cancel, |p| {
                if last.elapsed().as_secs_f64() >= 2.0 || p.finished {
                    last = Instant::now();
                    eprintln!(
                        "  {:>6.1}s {} {} files {} folders",
                        p.elapsed_ms as f64 / 1000.0,
                        gib(p.bytes),
                        p.files,
                        p.folders
                    );
                }
            })
            .expect("scan");
        let (nodes, listed, tree_bytes) = map.memory_stats();
        println!("== {}", summary.root.display());
        println!(
            "time {:.2}s, {} threads, {:.0} items/s",
            summary.elapsed_ms as f64 / 1000.0,
            summary.threads,
            summary.items_per_second
        );
        println!(
            "total {} ({} bytes = {} KiB), {} files, {} folders, complete {}",
            gib(summary.totals.bytes),
            summary.totals.bytes,
            summary.totals.bytes / 1024,
            summary.totals.files,
            summary.totals.folders,
            summary.totals.complete
        );
        println!(
            "tree: {nodes} folder records, {listed} listed files, ~{:.1} MiB reserved",
            tree_bytes as f64 / (1 << 20) as f64
        );
        let mut reasons: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for skipped in map.skipped() {
            let key = match &skipped.reason {
                SkipReason::OtherVolume { fs_type, .. } => format!("OtherVolume({fs_type})"),
                SkipReason::SameFolderAs { .. } => "SameFolderAs".to_string(),
                SkipReason::Error { code, .. } => format!("Error({code})"),
                other => format!("{other:?}"),
            };
            let size = map.folder(skipped.node).map_or(0, |f| f.size);
            let entry = reasons.entry(key).or_default();
            entry.0 += 1;
            entry.1 += size;
        }
        println!("skipped: {reasons:?}");
        for skipped in map.skipped().iter().filter(|s| {
            matches!(
                s.reason,
                SkipReason::SameFolderAs { .. } | SkipReason::OtherVolume { .. }
            )
        }) {
            println!(
                "  skipped {} : {}",
                skipped.path.display(),
                skipped.reason.message()
            );
        }
        for entry in map.children(map.root()).iter().take(top) {
            let label = match entry.kind {
                EntryKind::OtherFiles => format!("({} other files)", entry.files),
                _ => entry.name.clone(),
            };
            println!(
                "  {:>10}  {:5.1}%  {label}",
                gib(entry.size),
                entry.fraction_of_parent * 100.0
            );
        }
        for file in map.largest_files(map.root(), 5) {
            println!("  big file {} {}", gib(file.size), file.path.display());
        }
    }
}
