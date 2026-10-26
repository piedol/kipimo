//! Read-only repair check of /Applications and ~/Applications on this Mac.
//! `cargo run --release -p kipimo-startup --example repair_check [threads] [cancel-after-secs]`
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kipimo_startup::{RepairEvent, RepairOptions, repair_check, signer_summary};

/// This process's physical footprint and lifetime peak, in MB (what Activity Monitor shows).
fn footprint_mb() -> (f64, f64) {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    };
    if rc != 0 {
        return (0.0, 0.0);
    }
    let mb = |b: u64| b as f64 / 1_048_576.0;
    (
        mb(info.ri_phys_footprint),
        mb(info.ri_lifetime_max_phys_footprint),
    )
}

fn main() {
    let cancel = AtomicBool::new(false);
    let mut options = RepairOptions::default();
    if let Some(n) = std::env::args().nth(1).and_then(|a| a.parse().ok()) {
        options.threads = n;
    }
    let cancel_after = std::env::args().nth(2).and_then(|a| a.parse::<u64>().ok());
    let before = footprint_mb();
    let started = Instant::now();
    let mut cancelled_at = None;
    let mut slowest: Vec<(u64, String)> = Vec::new();
    let report = std::thread::scope(|scope| {
        if let Some(secs) = cancel_after {
            let cancel = &cancel;
            let cancelled_at = &mut cancelled_at;
            scope.spawn(move || {
                std::thread::sleep(Duration::from_secs(secs));
                cancel.store(true, Ordering::Relaxed);
                *cancelled_at = Some(Instant::now());
            });
        }
        repair_check(&options, &cancel, |event| match event {
            RepairEvent::Found { total } => {
                eprintln!("checking {total} apps with {} threads", options.threads)
            }
            RepairEvent::Finished { check, done, total } => {
                slowest.push((check.elapsed_ms, check.name.clone()));
                eprintln!(
                    "[{done}/{total}] {:>6} ms {:?} {}",
                    check.elapsed_ms, check.signature.status, check.name
                );
            }
            RepairEvent::Started { .. } => {}
        })
    });
    if let Some(at) = cancelled_at {
        println!(
            "cancelled after {:.1} s; returned {:.0} ms after the cancel (cancelled = {})",
            at.duration_since(started).as_secs_f64(),
            at.elapsed().as_secs_f64() * 1000.0,
            report.cancelled
        );
    }
    println!("\nBroken ({}):", report.broken.len());
    for app in &report.broken {
        println!(
            "  {} ({}) — {} — signed by {}",
            app.name,
            app.path.display(),
            app.reason(),
            signer_summary(&app.signature)
        );
        for ex in app
            .signature
            .problem
            .iter()
            .flat_map(|p| p.examples.iter())
            .take(3)
        {
            println!("      {}", ex.display());
        }
    }
    println!(
        "Unsigned ({}): {:?}",
        report.unsigned.len(),
        report.unsigned.iter().map(|a| &a.name).collect::<Vec<_>>()
    );
    println!(
        "Not checked ({}): {:?}",
        report.not_checked.len(),
        report
            .not_checked
            .iter()
            .map(|a| (&a.name, a.reason()))
            .collect::<Vec<_>>()
    );
    slowest.sort();
    slowest.reverse();
    println!("Slowest: {:?}", &slowest[..slowest.len().min(5)]);
    let after = footprint_mb();
    println!(
        "Checked {} apps ({} valid) in {:.1} s with {} threads; footprint {:.1} MB before, {:.1} MB after, peak {:.1} MB",
        report.checked,
        report.valid,
        report.elapsed_ms as f64 / 1000.0,
        options.threads,
        before.0,
        after.0,
        after.1
    );
}
