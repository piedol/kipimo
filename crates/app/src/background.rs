//! What Kipimo watches while the panel is closed: memory pressure (dispatch
//! source), the thermal state and wake (notifications), external drives
//! (DiskArbitration), apps arriving in the Trash (FSEvents), plus a slow 60 s
//! check of disk space, the Trash and Bluetooth batteries, and a daily
//! background check (in a child process). These feed the menu-bar icon's
//! colour, the alerts and the recommendations.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::sel;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSNotificationCenter, NSTimer};

use kipimo_sys::{BtDevice, PressureWatcher};

use crate::actions::Action;
use crate::attention::{Crossings, Limits, Readings, Watch};
use crate::i18n::{tr, trf, trn};
use crate::i18n_text::fill;
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
    _drives: Option<crate::drives::DriveWatcher>,
    _trash: Option<crate::trash_watch::TrashWatcher>,
    /// External drives seen mounted, by BSD name, so each is announced once.
    mounted: Vec<String>,
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
    let trash = crate::trash_watch::TrashWatcher::start(&kipimo_sys::disk::trash_dir(), |path| {
        util::main_async(move || app_trashed(path));
    });
    BG.with(|b| {
        *b.borrow_mut() = Some(Background {
            _pressure: pressure,
            _timer: timer,
            _drives: crate::drives::start(),
            _trash: trash,
            mounted: Vec::new(),
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
            crate::insights::maybe_refresh();
            crate::insights::revalidate_leftovers();
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
            title: tr("Your startup disk is almost full"),
            body: trf(
                "{size} left. Clean up junk files to make room.",
                &[("size", &format::bytes(free))],
            ),
            kind: Kind::Disk,
            action: Some(Action::OpenTools(Tab::Clean)),
        });
    }
    if let Some(bytes) = c.trash_big.filter(|_| s.alert_trash) {
        notify::post(Alert {
            id: "trash-big".into(),
            title: trf("The Trash is {size}", &[("size", &format::bytes(bytes))]),
            body: tr("Empty it in Finder to get that space back."),
            kind: Kind::Trash,
            action: Some(Action::OpenTrash),
        });
    }
    if s.alert_bluetooth {
        for (name, pct) in &c.bluetooth_low {
            notify::post(Alert {
                id: format!("bluetooth-low:{name}"),
                title: trf("{device} battery is low", &[("device", name)]),
                body: trf("{percent}% left.", &[("percent", &pct.to_string())]),
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

/// DiskArbitration saw a volume mount or unmount. External drives get an
/// alert; disk images, network shares and system volumes don't.
pub fn drive_changed(d: crate::drives::Drive) {
    if !d.is_external() {
        return;
    }
    let s = settings::get();
    match &d.path {
        Some(path) if d.mounted_in_volumes() => {
            let new = BG.with(|b| {
                let mut b = b.borrow_mut();
                let Some(b) = b.as_mut() else { return false };
                if b.mounted.contains(&d.bsd) {
                    return false;
                }
                b.mounted.push(d.bsd.clone());
                true
            });
            if new && s.alert_drives {
                let (free, total) = volume_space(path);
                notify::post(Alert {
                    id: format!("drive:{}", d.bsd),
                    title: trf("{drive} connected", &[("drive", &d.name)]),
                    body: trf(
                        "{free} free of {total}.",
                        &[
                            ("free", &format::bytes(free)),
                            ("total", &format::bytes(total)),
                        ],
                    ),
                    kind: Kind::Drive,
                    action: Some(Action::Reveal(path.clone())),
                });
            }
        }
        Some(_) => {}
        None => {
            BG.with(|b| {
                if let Some(b) = b.borrow_mut().as_mut() {
                    b.mounted.retain(|x| x != &d.bsd);
                }
            });
            if s.alert_drives {
                notify::post(Alert {
                    id: format!("drive:{}", d.bsd),
                    title: trf("{drive} ejected", &[("drive", &d.name)]),
                    body: tr("You can unplug it."),
                    kind: Kind::Drive,
                    action: None,
                });
            }
        }
    }
    tick();
}

fn volume_space(path: &std::path::Path) -> (u64, u64) {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return (0, 0);
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid path and buffer.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    (
        st.f_bavail * st.f_bsize as u64,
        st.f_blocks * st.f_bsize as u64,
    )
}

/// An app bundle arrived in the Trash: after a short wait (updaters swap copies through the
/// Trash), find what it left behind in a child process and offer a review.
pub fn app_trashed(path: std::path::PathBuf) {
    if !settings::get().alert_leftovers {
        return;
    }
    util::log(format!("app moved to the Trash: {}", path.display()));
    let wait = crate::leftover_check::SETTLE.as_secs_f64();
    util::main_after(wait, move || {
        // Put back (or emptied) meanwhile: nothing to say.
        if !path.exists() {
            return;
        }
        crate::page::spawn(
            crate::page::Priority::Background,
            {
                let path = path.clone();
                move || crate::leftover_check::check(&path)
            },
            move |r| {
                let Ok(Some(found)) = r else { return };
                if found.count == 0 || found.other_copy {
                    return;
                }
                leftovers_found(found, path);
            },
        );
    });
}

fn leftovers_found(found: crate::leftover_check::Found, bundle_path: std::path::PathBuf) {
    let crate::leftover_check::Found {
        bundle_id: id,
        name,
        count: n,
        bytes,
        ..
    } = found;
    crate::insights::leftovers_found(crate::insights::Leftovers {
        bundle_id: id.clone(),
        name: name.clone(),
        count: n,
        bytes,
        bundle_path,
    });
    notify::post(Alert {
        id: format!("leftovers:{id}"),
        title: fill(
            &trn(
                n as u64,
                "{app} left {n} file behind",
                "{app} left {n} files behind",
            ),
            &[("app", &name)],
        ),
        body: trf(
            "{size}. Review them before they go to the Trash.",
            &[("size", &format::bytes(bytes))],
        ),
        kind: Kind::Leftovers,
        action: Some(Action::Custom(format!("leftovers:{id}"))),
    });
}
