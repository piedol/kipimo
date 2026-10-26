//! Time each reader (release build): `cargo run -p kipimo-sys --release --example timing`.
use std::time::{Duration, Instant};

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let v = f();
    eprintln!("{label:<22} {:>8.2?}", t.elapsed());
    v
}

fn main() {
    let mut cpu = time("cpu new", kipimo_sys::CpuSampler::new);
    let mut procs = time("procs new", kipimo_sys::ProcessSampler::new);
    let mut net = time("net new", kipimo_sys::NetSampler::new);
    let mut power = time("ioreport new", kipimo_sys::PowerSampler::new);
    let smc = time("smc open", kipimo_sys::Smc::open);
    time("smc sensors (1st)", || {
        smc.as_ref().and_then(|s| s.temperatures())
    });
    std::thread::sleep(Duration::from_secs(1));
    for _ in 0..2 {
        time("cpu", || cpu.sample());
        time("procs top3", || procs.top(3));
        time("memory", kipimo_sys::memory::memory);
        time("net", || net.sample());
        time("battery", kipimo_sys::power::battery);
        time("system power", || {
            kipimo_sys::power::system_power_watts(smc.as_ref())
        });
        time("ioreport", || power.as_mut().and_then(|p| p.sample()));
        time("temperatures", || {
            smc.as_ref().and_then(|s| s.temperatures())
        });
        time("thermal", kipimo_sys::thermal::thermal_state);
        time("volumes", kipimo_sys::disk::volumes);
        time("boot free", kipimo_sys::disk::boot_free_bytes);
        let _ = time("trash", kipimo_sys::disk::trash_size);
        time("bluetooth", kipimo_sys::bluetooth::devices);
        time("xprotect", kipimo_sys::xprotect::status);
        time("model", kipimo_sys::model::mac_model);
        eprintln!("--");
        std::thread::sleep(Duration::from_millis(1500));
    }
}
