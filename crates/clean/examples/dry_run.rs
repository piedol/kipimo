//! Read-only dry run against this Mac: prints category totals, app counts and leftover sizes,
//! never file names, and never moves anything (the Trash provider is replaced by one that
//! refuses every move).
//!
//! `cargo run --release -p kipimo-clean --example dry_run [-- <bundle id>…]`

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kipimo_clean::{
    AppIndex, AppTarget, AppsEvent, Env, NoTrash, ScanEvent, ScanOptions, SigningReader,
};

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// (current, peak) physical footprint in MB: what Activity Monitor calls "Memory".
fn footprint() -> (f64, f64) {
    // SAFETY: proc_pid_rusage into a zeroed rusage_info_v4.
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as i32,
            libc::RUSAGE_INFO_V4,
            (&mut ri as *mut libc::rusage_info_v4).cast(),
        )
    };
    if rc != 0 {
        return (0.0, 0.0);
    }
    (
        ri.ri_phys_footprint as f64 / 1e6,
        ri.ri_lifetime_max_phys_footprint as f64 / 1e6,
    )
}

fn mem(label: &str) {
    let (now, peak) = footprint();
    println!("[{label}] footprint now {now:.1} MB, peak {peak:.1} MB");
}

fn main() {
    mem("start");
    let mut env = Env::current_user().expect("home folder");
    env.trash = Arc::new(NoTrash);
    let cancel = AtomicBool::new(false);

    println!("== Clean up (threads: {})", env.threads);
    let t = Instant::now();
    let mut ticks = 0;
    let report = kipimo_clean::scan_junk(
        &env,
        &ScanOptions::default(),
        &mut |e| {
            if let ScanEvent::Progress { .. } = e {
                ticks += 1;
            }
        },
        &cancel,
    );
    for c in &report.categories {
        println!(
            "{:<22} {:>10}  items {:>5}  ticked {:>10}  skipped {:>4}{}{}",
            format!("{:?}", c.info.id),
            human(c.total_bytes),
            c.items.len(),
            human(c.selected_bytes),
            c.skipped.len(),
            if c.unreadable > 0 {
                format!("  unreadable dirs {}", c.unreadable)
            } else {
                String::new()
            },
            if c.root_unreadable {
                "  (needs Full Disk Access)"
            } else {
                ""
            },
        );
    }
    let files: u64 = report
        .categories
        .iter()
        .flat_map(|c| c.items.iter())
        .map(|i| i.files)
        .sum();
    println!(
        "total {}  ticked by default {}  files {}  in {:.2} s ({} progress events, hard-link cap hit: {})",
        human(report.total_bytes),
        human(report.selected_bytes),
        files,
        t.elapsed().as_secs_f64(),
        ticks,
        report.hardlink_cap_hit
    );
    mem("after scan");

    println!("\n== Installed apps");
    let t = Instant::now();
    let mut found = 0;
    let apps = kipimo_clean::list_apps(
        &env,
        &mut |e| {
            if let AppsEvent::Found { .. } = e {
                found += 1;
            }
        },
        &cancel,
    );
    let listed_at = t.elapsed();
    let total: u64 = apps.iter().filter_map(|a| a.size_bytes).sum();
    println!(
        "{} apps, {} total, {} protected, {} running, {} with a last-used date, {} with a team ID; {:.2} s",
        apps.len(),
        human(total),
        apps.iter().filter(|a| a.protected.is_some()).count(),
        apps.iter().filter(|a| a.running).count(),
        apps.iter().filter(|a| a.last_used_unix.is_some()).count(),
        apps.iter().filter(|a| a.team_id.is_some()).count(),
        listed_at.as_secs_f64(),
    );
    assert_eq!(found, apps.len());
    mem("after app list");
    // Read-only check that platform (ships-with-macOS) detection works on this Mac.
    let platform = kipimo_clean::SystemSigning
        .read(std::path::Path::new("/System/Applications/Calculator.app"))
        .is_some_and(|s| s.platform);
    println!("platform signature detected on a built-in app: {platform}");

    println!("\n== Leftover finder");
    let t = Instant::now();
    let index = AppIndex::build(&env, &cancel);
    println!(
        "index: {} apps in {:.2} s",
        index.apps.len(),
        t.elapsed().as_secs_f64()
    );
    // Apps to look up, by bundle ID: `-- com.example.App com.example.Gone`.
    let mut targets: Vec<(String, AppTarget)> = Vec::new();
    for id in std::env::args().skip(1) {
        let installed = apps
            .iter()
            .find(|a| a.bundle_id.as_deref() == Some(id.as_str()));
        let state = if installed.is_some() {
            "installed"
        } else {
            "not installed"
        };
        let target = installed
            .and_then(|a| AppTarget::from_bundle(&env, &a.path))
            .unwrap_or_else(|| AppTarget::from_bundle_id(&id, None));
        targets.push((format!("{id} ({state})"), target));
    }
    for (label, target) in targets {
        let t = Instant::now();
        let r = kipimo_clean::find_leftovers(&env, &target, &index, &cancel);
        let mut by_kind: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
        for l in &r.leftovers {
            let e = by_kind
                .entry(format!(
                    "{:?}{}",
                    l.kind,
                    if l.needs_admin { " (admin)" } else { "" }
                ))
                .or_default();
            e.0 += 1;
            e.1 += l.bytes;
        }
        println!(
            "{label}: {} leftovers, {} movable, {} kept as shared, {:.2} s",
            r.leftovers.len(),
            human(r.total_bytes),
            r.kept.len(),
            t.elapsed().as_secs_f64()
        );
        for (kind, (n, b)) in by_kind {
            println!("    {kind:<28} {n:>3}  {}", human(b));
        }
    }
    // Every installed app, to check the finder never claims another app's items.
    let t = Instant::now();
    let (mut n, mut bytes, mut kept) = (0usize, 0u64, 0usize);
    for app in &apps {
        if let Some(target) = AppTarget::from_bundle(&env, &app.path) {
            let r = kipimo_clean::find_leftovers(&env, &target, &index, &cancel);
            n += r.leftovers.len();
            bytes += r.total_bytes;
            kept += r.kept.len();
        }
    }
    println!(
        "all {} apps: {} leftovers, {} movable, {} kept as shared; {:.2} s",
        apps.len(),
        n,
        human(bytes),
        kept,
        t.elapsed().as_secs_f64()
    );
    mem("after leftovers for every app");
}
