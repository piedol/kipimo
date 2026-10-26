//! Read-only dry run of the startup-items scan on this Mac. Prints one line per item.
//! `cargo run --release -p kipimo-startup --example startup_items [--json]`
use std::sync::atomic::AtomicBool;

use kipimo_startup::{ScanOptions, scan};

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let cancel = AtomicBool::new(false);
    let inv = scan(&ScanOptions::default(), &cancel, |_, _| {});
    if json {
        println!("{}", serde_json::to_string_pretty(&inv).unwrap());
        return;
    }
    for item in &inv.items {
        let sig = item.signature.as_ref();
        println!(
            "{:<16} {:<6} {:<45} enabled={:<5} loaded={:<5} pid={:<6} signer={:<12} {:<60} owner={:<22} flags={:?}",
            format!("{:?}", item.kind),
            format!("{:?}", item.scope),
            item.label,
            item.state
                .enabled
                .map(|b| b.to_string())
                .unwrap_or("-".into()),
            item.state
                .loaded
                .map(|b| b.to_string())
                .unwrap_or("-".into()),
            item.state.pid.map(|p| p.to_string()).unwrap_or("-".into()),
            sig.map(|s| format!("{:?}", s.kind)).unwrap_or("-".into()),
            sig.map(|s| format!("{:?} {}", s.status, s.signer.clone().unwrap_or_default()))
                .unwrap_or_default(),
            item.owner
                .as_ref()
                .map(|o| format!("{}{}", o.name, if o.installed { "" } else { " (gone)" }))
                .unwrap_or("-".into()),
            item.flags,
        );
    }
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    };
    println!(
        "\n{} items in {} ms; BTM source: {:?}; warnings: {:?}; footprint {:.1} MB, peak {:.1} MB",
        inv.items.len(),
        inv.elapsed_ms,
        inv.btm_source,
        inv.warnings,
        info.ri_phys_footprint as f64 / 1048576.0,
        info.ri_lifetime_max_phys_footprint as f64 / 1048576.0
    );
}
