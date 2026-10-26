//! macOS's thermal state (nominal, fair, serious, critical): the public,
//! stable signal for "this Mac is hot". The app listens for
//! `NSProcessInfoThermalStateDidChangeNotification`; this reads the current value.

use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ThermalState {
    Nominal,
    Fair,
    /// macOS is throttling to cool down.
    Serious,
    Critical,
}

pub fn thermal_state() -> ThermalState {
    match NSProcessInfo::processInfo().thermalState() {
        NSProcessInfoThermalState::Fair => ThermalState::Fair,
        NSProcessInfoThermalState::Serious => ThermalState::Serious,
        NSProcessInfoThermalState::Critical => ThermalState::Critical,
        _ => ThermalState::Nominal,
    }
}
