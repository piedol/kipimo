//! Read-only update check of this Mac's apps; prints a summary. Nothing is downloaded.
//!
//! cargo run -p kipimo-updates --example check_mac --release -- [--cache <file>] [--force] [--json]

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kipimo_updates::{CheckOptions, DiscoverOptions, Status, check_updates, discover_apps};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cache_path = args
        .iter()
        .position(|a| a == "--cache")
        .and_then(|i| args.get(i + 1))
        .map(Into::into);
    let opts = CheckOptions {
        cache_path,
        force: args.iter().any(|a| a == "--force"),
        ..CheckOptions::default()
    };

    let t = Instant::now();
    let apps = discover_apps(&DiscoverOptions::default());
    let discover_ms = t.elapsed().as_millis();
    let known = apps.iter().filter(|a| a.source.is_known()).count();

    let cancel = AtomicBool::new(false);
    let report = check_updates(&apps, &opts, |_| {}, &cancel);
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return;
    }
    for a in &report.apps {
        let installed = a.app.display_version().unwrap_or("?");
        let latest = a.latest_version().unwrap_or("-");
        let status = match &a.status {
            Status::UpToDate => "up to date".to_string(),
            Status::UpdateAvailable => {
                format!("UPDATE{}", if a.critical { " (critical)" } else { "" })
            }
            Status::RequiresNewerMacos { minimum } => format!("requires macOS {minimum}"),
            Status::UnknownSource => "no known update source".to_string(),
            Status::CheckFailed { reason } => format!("check failed: {reason}"),
        };
        println!(
            "{:<28} {:<16} {:<18} {:<16} {status}",
            a.app.name,
            a.app.source.label(),
            installed,
            latest
        );
    }
    let c = report.counts();
    println!(
        "\n{} apps, {known} with a known source; {} updates, {} need a newer macOS, {} up to date, {} failed, {} unknown",
        apps.len(),
        c.update_available,
        c.requires_newer_macos,
        c.up_to_date,
        c.check_failed,
        c.unknown_source
    );
    println!(
        "discovery {discover_ms} ms, check {} ms, {} requests, region {}, macOS {}",
        report.duration_ms, report.requests, report.store_country, report.macos_version
    );
}
