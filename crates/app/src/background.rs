//! What Kipimo watches while the panel is closed: memory pressure (dispatch
//! source), the thermal state, volume mounts and wake (notifications), plus a
//! slow 60 s check of disk space, the Trash and Bluetooth batteries. These feed
//! the menu-bar icon's colour and the alerts.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::sel;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSNotificationCenter, NSTimer};

use kipimo_sys::{BtDevice, PressureWatcher, VolumeKind};

use crate::actions::Action;
use crate::attention::{Crossings, Limits, Readings, Watch};
use crate::notify::{self, Alert, Kind};
use crate::tools_window::Tab;
use crate::util;
use crate::{app, format, settings, ui};

const CHECK_SECS: f64 = 60.0;

/// The Trash's size, re-measured only when the folder changes.
struct TrashCache {
    modified: Option<SystemTime>,
    bytes: Option<u64>,
}

static TRASH: Mutex<TrashCache> = Mutex::new(TrashCache {
    modified: None,
    bytes: None,
});
static TRASH_DENIED: AtomicBool = AtomicBool::new(false);

/// Measure the Trash (any thread). Cheap when nothing changed since last time.
pub fn measure_trash() -> Option<u64> {
    let modified = kipimo_sys::disk::trash_modified();
    {
        let c = TRASH.lock().unwrap();
        if c.bytes.is_some() && modified.is_some() && c.modified == modified {
            return c.bytes;
        }
    }
    let r = kipimo_sys::disk::trash_size();
    TRASH_DENIED.store(
        r == Err(kipimo_sys::TrashError::NoAccess),
        Ordering::Relaxed,
    );
    let bytes = r.ok().map(|t| t.bytes);
    let mut c = TRASH.lock().unwrap();
    c.modified = modified;
    c.bytes = bytes;
    bytes
}

/// macOS refused to let Kipimo read the Trash last time it tried.
pub fn trash_denied() -> bool {
    TRASH_DENIED.load(Ordering::Relaxed)
}

/// The last measured Trash size.
pub fn trash_bytes() -> Option<u64> {
    TRASH.lock().unwrap().bytes
}

struct Background {
    _pressure: PressureWatcher,
    _timer: Retained<NSTimer>,
    /// External drives seen mounted, by path, so "ejected" can name them.
    drives: Vec<(String, String)>,
}

thread_local! {
    static WATCH: RefCell<Watch> = RefCell::new(Watch::default());
    static BG: RefCell<Option<Background>> = const { RefCell::new(None) };
}

/// The current attention state (main thread).
pub fn watch() -> Watch {
    WATCH.with(|w| w.borrow().clone())
}

/// Start every watcher. Called once at launch.
pub fn start() {
    let mtm = util::mtm();
    WATCH.with(|w| {
        let mut w = w.borrow_mut();
        w.memory =
            kipimo_sys::memory::pressure_level().unwrap_or(kipimo_sys::PressureLevel::Normal);
        w.thermal = kipimo_sys::thermal::thermal_state();
    });
    let pressure = PressureWatcher::start(DispatchQueue::main(), |level| {
        WATCH.with(|w| w.borrow_mut().memory = level);
        app::refresh_attention();
    });

    let target = ui::controller(mtm);
    // SAFETY: the controller lives for the whole process; selectors take one NSNotification.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &target,
            sel!(thermalStateChanged:),
            Some(objc2_foundation::NSProcessInfoThermalStateDidChangeNotification),
            None,
        );
        let ws = NSWorkspace::sharedWorkspace().notificationCenter();
        for name in [
            objc2_app_kit::NSWorkspaceDidMountNotification,
            objc2_app_kit::NSWorkspaceDidUnmountNotification,
        ] {
            ws.addObserver_selector_name_object(&target, sel!(volumesChanged:), Some(name), None);
        }
        ws.addObserver_selector_name_object(
            &target,
            sel!(didWake:),
            Some(objc2_app_kit::NSWorkspaceDidWakeNotification),
            None,
        );
    }
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            CHECK_SECS,
            &target,
            sel!(backgroundTick:),
            None,
            true,
        )
    };
    // Let macOS batch this wake-up with others.
    timer.setTolerance(15.0);
    let drives = kipimo_sys::disk::volumes()
        .into_iter()
        .filter(|v| v.kind == VolumeKind::External)
        .map(|v| (v.path.display().to_string(), v.name))
        .collect();
    BG.with(|b| {
        *b.borrow_mut() = Some(Background {
            _pressure: pressure,
            _timer: timer,
            drives,
        })
    });
    util::main_after(2.0, tick);
}

/// The slow check: disk, Trash and Bluetooth, read off the main thread.
pub fn tick() {
    util::background(|| {
        let free = kipimo_sys::disk::boot_free_bytes();
        let trash = measure_trash();
        let devices = kipimo_sys::bluetooth::devices();
        util::main_async(move || {
            apply(Readings {
                boot_free: free,
                trash_bytes: trash,
                bluetooth: Some(levels(&devices)),
            });
            crate::weekly::check();
        });
    });
}

fn levels(devices: &[BtDevice]) -> Vec<(String, u8)> {
    devices
        .iter()
        .filter_map(|d| d.lowest().map(|p| (d.name.clone(), p)))
        .collect()
}

/// The panel read the disk and Trash; keep the gates in step.
pub fn feed_disk(free: u64, trash: Option<u64>) {
    apply(Readings {
        boot_free: Some(free),
        trash_bytes: trash,
        bluetooth: None,
    });
}

pub fn feed_disk_trash_only(trash: Option<u64>) {
    apply(Readings {
        boot_free: None,
        trash_bytes: trash,
        bluetooth: None,
    });
}

pub fn feed_devices(devices: &[BtDevice]) {
    apply(Readings {
        boot_free: None,
        trash_bytes: None,
        bluetooth: Some(levels(devices)),
    });
}

fn apply(r: Readings) {
    let s = settings::get();
    let crossings = WATCH.with(|w| w.borrow_mut().update(&r, &Limits::from(&s)));
    alert(&crossings, &s);
    app::refresh_attention();
}

fn alert(c: &Crossings, s: &settings::Settings) {
    if let Some(free) = c.disk_low.filter(|_| s.alert_disk) {
        notify::post(Alert {
            id: "disk-low".into(),
            title: "Your startup disk is almost full".into(),
            body: format!(
                "{} left. Clean up junk files to make room.",
                format::bytes(free)
            ),
            kind: Kind::Disk,
            action: Some(Action::OpenTools(Tab::Clean)),
        });
    }
    if let Some(bytes) = c.trash_big.filter(|_| s.alert_trash) {
        notify::post(Alert {
            id: "trash-big".into(),
            title: format!("The Trash is {}", format::bytes(bytes)),
            body: "Empty it in Finder to get that space back.".into(),
            kind: Kind::Trash,
            action: Some(Action::OpenTrash),
        });
    }
    if s.alert_bluetooth {
        for (name, pct) in &c.bluetooth_low {
            notify::post(Alert {
                id: format!("bluetooth-low:{name}"),
                title: format!("{name} battery is low"),
                body: format!("{pct}% left."),
                kind: Kind::Bluetooth,
                action: None,
            });
        }
    }
}

pub fn thermal_changed() {
    WATCH.with(|w| w.borrow_mut().thermal = kipimo_sys::thermal::thermal_state());
    app::refresh_attention();
}

/// A volume was mounted or unmounted. External drives get an alert; disk
/// images and system volumes don't.
pub fn volume_changed(mounted: bool, path: Option<String>) {
    let s = settings::get();
    let Some(path) = path else { return };
    if mounted {
        util::background(move || {
            let v = kipimo_sys::disk::volumes()
                .into_iter()
                .find(|v| v.path.display().to_string() == path);
            util::main_async(move || {
                let Some(v) = v.filter(|v| v.kind == VolumeKind::External) else {
                    return;
                };
                BG.with(|b| {
                    if let Some(b) = b.borrow_mut().as_mut() {
                        b.drives.retain(|(p, _)| *p != path);
                        b.drives.push((path.clone(), v.name.clone()));
                    }
                });
                if s.alert_drives {
                    notify::post(Alert {
                        id: format!("drive:{path}"),
                        title: format!("{} connected", v.name),
                        body: format!(
                            "{} free of {}.",
                            format::bytes(v.available_important),
                            format::bytes(v.total)
                        ),
                        kind: Kind::Drive,
                        action: Some(Action::Reveal(v.path.clone())),
                    });
                }
            });
        });
    } else {
        let name = BG.with(|b| {
            let mut b = b.borrow_mut();
            let b = b.as_mut()?;
            let i = b.drives.iter().position(|(p, _)| *p == path)?;
            Some(b.drives.remove(i).1)
        });
        if let Some(name) = name.filter(|_| s.alert_drives) {
            notify::post(Alert {
                id: format!("drive:{path}"),
                title: format!("{name} ejected"),
                body: "You can unplug it.".into(),
                kind: Kind::Drive,
                action: None,
            });
        }
    }
    tick();
}
