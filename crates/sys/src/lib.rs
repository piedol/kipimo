//! # kipimo-sys: the panel's system readers
//!
//! Plain Rust readers for CPU, memory, disks and Trash, network, battery and
//! power, chip temperature, Bluetooth device batteries, XProtect and the Mac
//! model. No UI code. Every reader is cheap enough to call on a 1–2 s timer
//! while the panel is open; call them off the main thread.
//!
//! Readers that need two samples (CPU, processes, network, chip power) are
//! small structs that keep the previous sample. Create them when the panel
//! opens and drop them when it closes, so nothing is held while idle.
//!
//! ## API
//!
//! | Module | What | Call |
//! |---|---|---|
//! | [`cpu`] | Total and per-core load | `CpuSampler::new()`, `.sample() -> Option<CpuLoad>` |
//! | [`cpu`] | Busiest processes | `ProcessSampler::new()`, `.top(3) -> Vec<ProcessCpu>` |
//! | [`memory`] | Pressure %, used / app / wired / compressed / cached / swap | `memory() -> Option<Memory>`, `pressure_level()` |
//! | [`memory`] | Pressure changes, event-driven | `PressureWatcher::start(&queue, \|level\| …)` (stops on drop) |
//! | [`disk`] | Volumes with free and purgeable space | `volumes()`, `boot_volume()`, `boot_free_bytes()` |
//! | [`disk`] | Trash size (needs Full Disk Access) | `trash_size() -> Result<TrashSize, TrashError>`, `trash_modified()`, `tree_size(path, cancel)` |
//! | [`net`] | Upload and download rates | `NetSampler::new()`, `.sample() -> Option<NetRates>` |
//! | [`power`] | Internal battery | `battery() -> Option<Battery>` |
//! | [`power`] | Whole-Mac power draw (W) | `system_power_watts(Option<&Smc>)` |
//! | [`ioreport`] | CPU / GPU / ANE power (W), private IOReport | `PowerSampler::new() -> Option<_>`, `.sample() -> Option<ChipPower>` |
//! | [`smc`] | Chip temperature (°C), SMC keys | `Smc::open() -> Option<Smc>`, `.temperatures() -> Option<Temperatures>` |
//! | [`thermal`] | macOS thermal state | `thermal_state() -> ThermalState` |
//! | [`bluetooth`] | Device batteries (AirPods left/right/case, Magic devices) | `devices() -> Vec<BtDevice>` |
//! | [`xprotect`] | Malware-definitions version and date | `status() -> XProtect` |
//! | [`model`] | "MacBook Pro (16-inch, M5 Max)", chip, memory | `mac_model() -> MacModel` |
//!
//! [`Monitor`] bundles the stateful samplers, and [`snapshot`] reads
//! everything once (used by `cargo run -p kipimo-sys --example snapshot`).
//!
//! ## Private interfaces
//!
//! IOReport (chip power), the SMC key interface (temperature, `PSTR` system
//! power) and `IOPSCopyPowerSourcesByType` (accessory batteries) are not public
//! API. IOReport and `IOPSCopyPowerSourcesByType` are looked up at run time;
//! every one of them returns `None` / empty instead of failing, and the panel
//! hides what it can't read.
//!
//! All result types derive `serde::Serialize`; sizes are bytes, rates are
//! bytes per second, power is watts, temperature is °C.

use std::path::PathBuf;

mod cf;
mod iokit;

pub mod bluetooth;
pub mod cpu;
pub mod disk;
pub mod ioreport;
pub mod memory;
pub mod model;
pub mod net;
pub mod power;
pub mod smc;
pub mod thermal;
pub mod xprotect;

use serde::Serialize;

pub use bluetooth::{BatteryPart, BtDevice, PartKind};
pub use cpu::{CpuLoad, CpuSampler, ProcessCpu, ProcessSampler};
pub use disk::{TrashError, TrashSize, Volume, VolumeKind};
pub use ioreport::{ChipPower, PowerSampler};
pub use memory::{Memory, PressureLevel, PressureWatcher};
pub use model::MacModel;
pub use net::{NetRates, NetSampler};
pub use power::Battery;
pub use smc::{Smc, Temperatures};
pub use thermal::ThermalState;
pub use xprotect::XProtect;

/// Power draw right now. Either value may be missing on a given Mac or macOS release.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct PowerDraw {
    /// The whole Mac (SMC `PSTR`, or the battery gauge's telemetry).
    pub system_watts: Option<f32>,
    /// CPU + GPU + Neural Engine from IOReport.
    pub chip: Option<ChipPower>,
}

/// The values that change from second to second.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Live {
    pub cpu: Option<CpuLoad>,
    pub top_processes: Vec<ProcessCpu>,
    pub memory: Option<Memory>,
    pub network: Option<NetRates>,
    pub battery: Option<Battery>,
    pub power: PowerDraw,
    pub temperature: Option<Temperatures>,
    pub thermal: Option<ThermalState>,
}

/// The stateful samplers the panel keeps while it is open.
pub struct Monitor {
    cpu: CpuSampler,
    procs: ProcessSampler,
    net: NetSampler,
    power: Option<PowerSampler>,
    smc: Option<Smc>,
    samples: u64,
    last_chip: Option<ChipPower>,
}

/// IOReport sampling is the costliest read (about 6 ms on an M5 Max), so chip
/// power is re-read every third `sample` and averaged over that longer span.
const CHIP_POWER_EVERY: u64 = 3;

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    /// Take the first samples now; the first `sample` reports the interval since.
    pub fn new() -> Monitor {
        Monitor {
            cpu: CpuSampler::new(),
            procs: ProcessSampler::new(),
            net: NetSampler::new(),
            power: PowerSampler::new(),
            smc: Smc::open(),
            samples: 0,
            last_chip: None,
        }
    }

    /// Everything in [`Live`]. Takes a few milliseconds (chip power is
    /// refreshed every third call).
    pub fn sample(&mut self) -> Live {
        self.samples += 1;
        if (self.samples % CHIP_POWER_EVERY == 1 || self.last_chip.is_none())
            && let Some(p) = self.power.as_mut().and_then(PowerSampler::sample)
        {
            self.last_chip = Some(p);
        }
        Live {
            cpu: self.cpu.sample(),
            top_processes: self.procs.top(3),
            memory: memory::memory(),
            network: self.net.sample(),
            battery: power::battery(),
            power: PowerDraw {
                system_watts: power::system_power_watts(self.smc.as_ref()),
                chip: self.last_chip,
            },
            temperature: self.smc.as_ref().and_then(Smc::temperatures),
            thermal: Some(thermal::thermal_state()),
        }
    }

    /// The IOReport sampler, if this Mac has one (for listing channels).
    pub fn power_sampler(&self) -> Option<&PowerSampler> {
        self.power.as_ref()
    }
}

/// Everything at once.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Snapshot {
    pub model: MacModel,
    pub xprotect: XProtect,
    pub volumes: Vec<Volume>,
    pub trash: Result<TrashSize, TrashError>,
    pub bluetooth: Vec<BtDevice>,
    pub live: Live,
}

/// Read every value once, using `monitor` for the interval-based ones.
pub fn snapshot(monitor: &mut Monitor) -> Snapshot {
    Snapshot {
        model: model::mac_model(),
        xprotect: xprotect::status(),
        volumes: disk::volumes(),
        trash: disk::trash_size(),
        bluetooth: bluetooth::devices(),
        live: monitor.sample(),
    }
}

/// Development builds only: `KIPIMO_FAKE_HOME` / `KIPIMO_FAKE_ROOT` stand in for the home
/// folder and `/`, so the app can be exercised on a synthetic Mac (a throwaway disk image).
/// Release builds ignore them.
pub fn fake_home() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_HOME")
}

/// See [`fake_home`].
pub fn fake_root() -> Option<PathBuf> {
    dev_var("KIPIMO_FAKE_ROOT")
}

fn dev_var(name: &str) -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    }
}
