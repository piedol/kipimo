//! What needs attention, with hysteresis so a value hovering at a threshold
//! doesn't flip the menu-bar icon (or fire an alert) on every check.
//!
//! [`Gate`] tracks one threshold and reports crossings, so an alert goes out
//! once per crossing. [`Watch`] holds every gate and turns the latest readings
//! into the list of [`Reason`]s the icon and the panel header show.

use kipimo_sys::{PressureLevel, ThermalState};

use crate::i18n::{tr, trf};

/// One threshold with a recovery margin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gate {
    /// Fires when the value goes below `threshold` (true) or above it (false).
    pub below: bool,
    pub active: bool,
}

/// A crossing reported by [`Gate::update`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Entered,
    Left,
}

impl Gate {
    pub const fn below() -> Gate {
        Gate {
            below: true,
            active: false,
        }
    }
    pub const fn above() -> Gate {
        Gate {
            below: false,
            active: false,
        }
    }

    /// Feed a new value. The gate opens when the value crosses `threshold` and
    /// closes only once it is back past `threshold ± margin`.
    pub fn update(&mut self, value: f64, threshold: f64, margin: f64) -> Option<Edge> {
        let (enter, leave) = if self.below {
            (value < threshold, value >= threshold + margin)
        } else {
            (value > threshold, value <= threshold - margin)
        };
        if !self.active && enter {
            self.active = true;
            Some(Edge::Entered)
        } else if self.active && leave {
            self.active = false;
            Some(Edge::Left)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    LowDisk { free: u64 },
    MemoryPressure(PressureLevel),
    Hot(ThermalState),
    LowBluetooth { name: String, percent: u8 },
    Recommendation { title: String },
}

impl Reason {
    pub fn text(&self) -> String {
        match self {
            Reason::LowDisk { free } => trf(
                "Startup disk has {size} left",
                &[("size", &crate::format::bytes(*free))],
            ),
            Reason::MemoryPressure(PressureLevel::Critical) => tr("Memory pressure is critical"),
            Reason::MemoryPressure(_) => tr("Memory pressure is high"),
            Reason::Hot(ThermalState::Critical) => tr("Your Mac is very hot"),
            Reason::Hot(_) => tr("Your Mac is hot and slowing down to cool off"),
            Reason::LowBluetooth { name, percent } => trf(
                "{device} battery is at {percent}%",
                &[("device", name), ("percent", &percent.to_string())],
            ),
            Reason::Recommendation { title } => title.clone(),
        }
    }
}

/// Thresholds the gates use, from Settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub disk_bytes: u64,
    pub trash_bytes: u64,
    pub bluetooth_pct: u8,
}

impl Limits {
    pub fn from(s: &crate::settings::Settings) -> Limits {
        Limits {
            disk_bytes: s.disk_bytes(),
            trash_bytes: s.trash_bytes(),
            bluetooth_pct: s.bluetooth_pct,
        }
    }
    /// Disk alerts clear 10% (at least 2 GB) above the threshold.
    pub fn disk_margin(&self) -> f64 {
        (self.disk_bytes as f64 * 0.1).max(2e9)
    }
    pub fn trash_margin(&self) -> f64 {
        self.trash_bytes as f64 * 0.1
    }
}

/// Crossings worth an alert, from one [`Watch::update`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Crossings {
    pub disk_low: Option<u64>,
    pub trash_big: Option<u64>,
    pub bluetooth_low: Vec<(String, u8)>,
}

/// The latest readings the background check feeds in. `None` = not read this time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Readings {
    pub boot_free: Option<u64>,
    pub trash_bytes: Option<u64>,
    pub bluetooth: Option<Vec<(String, u8)>>,
}

/// Every gate, plus the latest event-driven states.
#[derive(Clone, Debug, PartialEq)]
pub struct Watch {
    disk: Gate,
    trash: Gate,
    bluetooth: Vec<(String, Gate)>,
    pub boot_free: Option<u64>,
    pub low_devices: Vec<(String, u8)>,
    pub memory: PressureLevel,
    pub thermal: ThermalState,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            disk: Gate::below(),
            trash: Gate::above(),
            bluetooth: Vec::new(),
            boot_free: None,
            low_devices: Vec::new(),
            memory: PressureLevel::Normal,
            thermal: ThermalState::Nominal,
        }
    }
}

impl Watch {
    /// Feed readings; returns the crossings that should become alerts.
    pub fn update(&mut self, r: &Readings, l: &Limits) -> Crossings {
        let mut c = Crossings::default();
        if let Some(free) = r.boot_free {
            self.boot_free = Some(free);
            if self
                .disk
                .update(free as f64, l.disk_bytes as f64, l.disk_margin())
                == Some(Edge::Entered)
            {
                c.disk_low = Some(free);
            }
        }
        if let Some(t) = r.trash_bytes
            && self
                .trash
                .update(t as f64, l.trash_bytes as f64, l.trash_margin())
                == Some(Edge::Entered)
        {
            c.trash_big = Some(t);
        }
        if let Some(devices) = &r.bluetooth {
            // Gates outlive disconnects: AirPods drop off every time they go in their case,
            // and coming back still low must not alert again. A gate re-arms only once that
            // device reports a charge past the margin. A few dozen are kept at most.
            const KEEP: usize = 32;
            if self.bluetooth.len() > KEEP {
                let connected = |n: &String| devices.iter().any(|(d, _)| d == n);
                let mut extra = self.bluetooth.len() - KEEP;
                self.bluetooth.retain(|(n, _)| {
                    let drop = extra > 0 && !connected(n);
                    extra -= usize::from(drop);
                    !drop
                });
            }
            self.low_devices.clear();
            for (name, pct) in devices {
                let gate = match self.bluetooth.iter_mut().find(|(n, _)| n == name) {
                    Some((_, g)) => g,
                    None => {
                        self.bluetooth.push((name.clone(), Gate::below()));
                        &mut self.bluetooth.last_mut().unwrap().1
                    }
                };
                if gate.update(*pct as f64, l.bluetooth_pct as f64, 5.0) == Some(Edge::Entered) {
                    c.bluetooth_low.push((name.clone(), *pct));
                }
                if gate.active {
                    self.low_devices.push((name.clone(), *pct));
                }
            }
        }
        c
    }

    pub fn trash_over(&self) -> bool {
        self.trash.active
    }

    pub fn disk_low(&self) -> bool {
        self.disk.active
    }

    /// Everything that needs attention now. `recommendation` is the pending one, if any.
    pub fn reasons(&self, recommendation: Option<&str>) -> Vec<Reason> {
        let mut v = Vec::new();
        if self.disk.active
            && let Some(free) = self.boot_free
        {
            v.push(Reason::LowDisk { free });
        }
        if self.memory >= PressureLevel::Warning {
            v.push(Reason::MemoryPressure(self.memory));
        }
        if self.thermal >= ThermalState::Serious {
            v.push(Reason::Hot(self.thermal));
        }
        for (name, percent) in &self.low_devices {
            v.push(Reason::LowBluetooth {
                name: name.clone(),
                percent: *percent,
            });
        }
        if let Some(t) = recommendation {
            v.push(Reason::Recommendation {
                title: t.to_string(),
            });
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Limits = Limits {
        disk_bytes: 20_000_000_000,
        trash_bytes: 2_000_000_000,
        bluetooth_pct: 20,
    };

    #[test]
    fn gate_has_hysteresis() {
        let mut g = Gate::below();
        assert_eq!(g.update(25.0, 20.0, 2.0), None);
        assert_eq!(g.update(19.0, 20.0, 2.0), Some(Edge::Entered));
        // Hovering around the threshold doesn't flap.
        assert_eq!(g.update(20.5, 20.0, 2.0), None);
        assert_eq!(g.update(19.5, 20.0, 2.0), None);
        assert!(g.active);
        assert_eq!(g.update(22.0, 20.0, 2.0), Some(Edge::Left));
        assert_eq!(g.update(19.0, 20.0, 2.0), Some(Edge::Entered));
    }

    #[test]
    fn gate_above() {
        let mut g = Gate::above();
        assert_eq!(g.update(2.5, 2.0, 0.2), Some(Edge::Entered));
        assert_eq!(g.update(1.9, 2.0, 0.2), None);
        assert_eq!(g.update(1.8, 2.0, 0.2), Some(Edge::Left));
    }

    #[test]
    fn disk_alert_once_per_crossing() {
        let mut w = Watch::default();
        let r = |free| Readings {
            boot_free: Some(free),
            ..Readings::default()
        };
        assert_eq!(w.update(&r(30_000_000_000), &L).disk_low, None);
        assert_eq!(
            w.update(&r(19_000_000_000), &L).disk_low,
            Some(19_000_000_000)
        );
        assert_eq!(w.update(&r(18_000_000_000), &L).disk_low, None);
        assert_eq!(
            w.update(&r(21_000_000_000), &L).disk_low,
            None,
            "within the margin"
        );
        assert_eq!(
            w.reasons(None),
            vec![Reason::LowDisk {
                free: 21_000_000_000
            }]
        );
        w.update(&r(23_000_000_000), &L);
        assert!(w.reasons(None).is_empty());
        assert!(w.update(&r(10_000_000_000), &L).disk_low.is_some());
    }

    #[test]
    fn trash_alert_once_per_crossing() {
        let mut w = Watch::default();
        let r = |t| Readings {
            trash_bytes: Some(t),
            ..Readings::default()
        };
        assert_eq!(
            w.update(&r(3_100_000_000), &L).trash_big,
            Some(3_100_000_000)
        );
        assert!(w.trash_over());
        assert_eq!(w.update(&r(3_500_000_000), &L).trash_big, None);
        w.update(&r(0), &L);
        assert!(!w.trash_over());
    }

    #[test]
    fn bluetooth_per_device() {
        let mut w = Watch::default();
        let r = |v: Vec<(&str, u8)>| Readings {
            bluetooth: Some(v.into_iter().map(|(n, p)| (n.to_string(), p)).collect()),
            ..Readings::default()
        };
        let c = w.update(&r(vec![("AirPods", 19), ("Mouse", 80)]), &L);
        assert_eq!(c.bluetooth_low, vec![("AirPods".to_string(), 19)]);
        assert_eq!(
            w.reasons(None),
            vec![Reason::LowBluetooth {
                name: "AirPods".into(),
                percent: 19
            }]
        );
        assert!(
            w.update(&r(vec![("AirPods", 18), ("Mouse", 80)]), &L)
                .bluetooth_low
                .is_empty()
        );
        // Charged past the margin: clears.
        w.update(&r(vec![("AirPods", 30)]), &L);
        assert!(w.reasons(None).is_empty());
        // Goes low, disconnects (in its case), comes back still low: no second alert.
        assert_eq!(
            w.update(&r(vec![("AirPods", 15)]), &L).bluetooth_low.len(),
            1
        );
        w.update(&r(vec![]), &L);
        assert!(
            w.reasons(None).is_empty(),
            "disconnected devices aren't shown"
        );
        assert!(
            w.update(&r(vec![("AirPods", 14)]), &L)
                .bluetooth_low
                .is_empty()
        );
        assert_eq!(w.reasons(None).len(), 1, "still shown as low");
        // Charged in the case, then low again later: alerts again.
        w.update(&r(vec![]), &L);
        w.update(&r(vec![("AirPods", 100)]), &L);
        assert_eq!(
            w.update(&r(vec![("AirPods", 12)]), &L).bluetooth_low.len(),
            1
        );
        // A different device still alerts on its own.
        assert_eq!(w.update(&r(vec![("Mouse", 10)]), &L).bluetooth_low.len(), 1);
    }

    #[test]
    fn event_driven_states_and_recommendation() {
        let w = Watch {
            memory: PressureLevel::Warning,
            thermal: ThermalState::Serious,
            ..Watch::default()
        };
        let r = w.reasons(Some("Trash is 3.1 GB"));
        assert_eq!(r.len(), 3);
        assert_eq!(
            r[2],
            Reason::Recommendation {
                title: "Trash is 3.1 GB".into()
            }
        );
        let calm = Watch {
            memory: PressureLevel::Normal,
            thermal: ThermalState::Fair,
            ..Watch::default()
        };
        assert!(calm.reasons(None).is_empty());
    }
}
