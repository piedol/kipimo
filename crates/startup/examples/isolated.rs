//! How the app runs the heavy checks in a child process, and what that does to memory.
//! `cargo run --release -p kipimo-startup --example isolated [scan|repair] [cancel-after-secs]`
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kipimo_startup::{
    RepairEvent, RepairOptions, ScanOptions, WORKER_ARG, repair_check_isolated, scan_isolated,
    worker_main,
};

fn footprint_mb() -> (f64, f64) {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    };
    let mb = |b: u64| b as f64 / 1_048_576.0;
    (
        mb(info.ri_phys_footprint),
        mb(info.ri_lifetime_max_phys_footprint),
    )
}

fn main() {
    // What the app's main() does first.
    if std::env::args().nth(1).as_deref() == Some(WORKER_ARG) {
        std::process::exit(worker_main());
    }
    let exe = std::env::current_exe().unwrap();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "repair".into());
    let cancel_after = std::env::args().nth(2).and_then(|a| a.parse::<u64>().ok());
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let mut cancelled_at = None;

    std::thread::scope(|scope| {
        if let Some(secs) = cancel_after {
            let (cancel, cancelled_at) = (&cancel, &mut cancelled_at);
            scope.spawn(move || {
                std::thread::sleep(Duration::from_secs(secs));
                cancel.store(true, Ordering::Relaxed);
                *cancelled_at = Some(Instant::now());
            });
        }
        if mode == "scan" {
            let inv = scan_isolated(&exe, &ScanOptions::default(), &cancel, |_, _| {}).unwrap();
            println!("{} items, cancelled = {}", inv.items.len(), inv.cancelled);
        } else {
            let mut done = 0;
            let report = repair_check_isolated(&exe, &RepairOptions::default(), &cancel, |e| {
                if let RepairEvent::Finished { .. } = e {
                    done += 1;
                }
            })
            .unwrap();
            println!(
                "{} apps checked ({done} events), {} broken, cancelled = {}",
                report.checked,
                report.broken.len(),
                report.cancelled
            );
        }
    });
    if let Some(at) = cancelled_at {
        println!(
            "returned {:.0} ms after the cancel",
            at.elapsed().as_secs_f64() * 1000.0
        );
    }
    let (now, peak) = footprint_mb();
    println!(
        "parent: {:.1} s, footprint {now:.1} MB, peak {peak:.1} MB",
        start.elapsed().as_secs_f64()
    );
}
