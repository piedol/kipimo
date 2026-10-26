//! Print a live snapshot of every reader as JSON, plus how long each sample took.
//!
//!     cargo run -p kipimo-sys --example snapshot            # one snapshot after 1 s
//!     cargo run -p kipimo-sys --example snapshot -- --channels   # also list IOReport energy channels

use std::time::{Duration, Instant};

fn main() {
    let t = Instant::now();
    let mut m = kipimo_sys::Monitor::new();
    let setup = t.elapsed();
    std::thread::sleep(Duration::from_secs(1));
    let t = Instant::now();
    let snap = kipimo_sys::snapshot(&mut m);
    let full = t.elapsed();
    let t = Instant::now();
    let _ = m.sample();
    let live = t.elapsed();
    println!("{}", serde_json::to_string_pretty(&snap).unwrap());
    eprintln!("monitor setup {setup:?}, full snapshot {full:?}, live sample {live:?}");
    if std::env::args().any(|a| a == "--channels") {
        if let Some(p) = m.power_sampler() {
            for c in p.channel_names() {
                eprintln!("  {c}");
            }
        } else {
            eprintln!("IOReport is not available");
        }
    }
}
