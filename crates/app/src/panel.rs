//! The menu-bar panel: a borderless HUD panel anchored under the status item.
//! It samples every 1–2 s only while open; closing it stops the timer and
//! drops every sampler and view, so nothing is held while idle.
//!
//! Layout (360 pt wide): header, Protection, then Disk | Memory, Battery | CPU,
//! Network | Devices, Today's recommendation, and the footer.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezelStyle, NSButton, NSColor, NSControlSize, NSEvent, NSEventMask,
    NSImageView, NSMenu, NSMenuItem, NSPanel, NSScreen, NSStatusWindowLevel, NSTextAlignment,
    NSTextField, NSView, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSPoint, NSRect, NSTimer};

use kipimo_sys::{BtDevice, Live, Monitor, PressureLevel, ThermalState, Volume};

use crate::commands::Command;
use crate::recommend::Recommendation;
use crate::ui::{self, Weight, rect, white};
use crate::util::{self, ns};
use crate::{app, background, format, settings};

const W: f64 = 360.0;
const PAD: f64 = 12.0;
const GAP: f64 = 8.0;
const HALF: f64 = (W - 2.0 * PAD - GAP) / 2.0;
const CARD_H: f64 = 102.0;
const PROTECTION_H: f64 = 50.0;
const REC_H: f64 = 66.0;
const HEADER_H: f64 = 46.0;
const FOOTER_H: f64 = 28.0;
const H: f64 = PAD
    + HEADER_H
    + GAP
    + PROTECTION_H
    + GAP
    + 3.0 * (CARD_H + GAP)
    + REC_H
    + 10.0
    + FOOTER_H
    + PAD;
const KEY_ESCAPE: u16 = 53;
const UNIT_Y: f64 = 39.0;
const BAR_Y: f64 = 62.0;

define_class!(
    // SAFETY: NSPanel subclass that only allows itself to become key (borderless
    // windows can't by default), so Escape reaches it.
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoPanel"]
    struct KeyPanel;

    impl KeyPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
    }
);

define_class!(
    // SAFETY: NSObject subclass acting as the panel's delegate; no Drop impl.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoPanelDelegate"]
    struct PanelDelegate;

    unsafe impl NSObjectProtocol for PanelDelegate {}

    unsafe impl NSWindowDelegate for PanelDelegate {
        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _n: &NSNotification) {
            if !MENU_OPEN.with(|m| m.get()) {
                util::log("panel: resigned key");
                util::main_async(close);
            }
        }
    }
);

/// Orange when something needs attention, otherwise white at `alpha`.
fn warn_or(warn: bool, alpha: f64) -> Retained<NSColor> {
    if warn {
        NSColor::systemOrangeColor()
    } else {
        white(alpha)
    }
}

// ── Cards ──────────────────────────────────────────────────────────────────

/// A horizontal meter (disk used, memory pressure, charge).
struct Bar(Retained<ui::Fill>);

impl Bar {
    fn new(mtm: MainThreadMarker, parent: &NSView, frame: NSRect) -> Bar {
        let f = ui::Fill::new(
            mtm,
            frame,
            &white(0.85),
            frame.size.height / 2.0,
            ui::Paint::Meter(0.0),
        );
        parent.addSubview(&f);
        Bar(f)
    }

    fn set(&self, fraction: f64, color: &NSColor) {
        self.0.set(color, ui::Paint::Meter(fraction));
    }
}

struct Card {
    root: Retained<NSView>,
    icon: Retained<NSImageView>,
    title: Retained<NSTextField>,
    accessory: Retained<NSTextField>,
    value: Retained<NSTextField>,
    unit: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    width: f64,
}

impl Card {
    fn new(
        mtm: MainThreadMarker,
        parent: &NSView,
        frame: NSRect,
        symbol: &str,
        title: &str,
    ) -> Card {
        let w = frame.size.width;
        let h = frame.size.height;
        let root = ui::flipped(mtm, frame);
        root.addSubview(&ui::fill(mtm, rect(0.0, 0.0, w, h), &white(0.07), 10.0));
        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(PAD, 10.0, 18.0, 18.0));
        icon.setContentTintColor(Some(&white(0.6)));
        root.addSubview(&icon);
        let title_l = ui::label(mtm, title, 12.0, Weight::Semibold, &white(0.85));
        title_l.setFrame(rect(PAD + 22.0, 10.0, w - PAD * 2.0 - 22.0, 17.0));
        root.addSubview(&title_l);
        let accessory = ui::label(mtm, "", 11.0, Weight::Digits, &white(0.6));
        accessory.setAlignment(NSTextAlignment::Right);
        accessory.setFrame(rect(w - PAD - 90.0, 11.0, 90.0, 15.0));
        root.addSubview(&accessory);
        let value = ui::label(mtm, "–", 22.0, Weight::BigDigits, &white(0.95));
        value.setFrame(rect(PAD, 29.0, w - PAD * 2.0, 28.0));
        root.addSubview(&value);
        let unit = ui::label(mtm, "", 12.0, Weight::Regular, &white(0.6));
        unit.setFrame(rect(PAD, UNIT_Y, 80.0, 16.0));
        root.addSubview(&unit);
        let detail = ui::label(mtm, "", 11.0, Weight::Regular, &white(0.6));
        detail.setFrame(rect(PAD, h - PAD - 13.0, w - PAD * 2.0, 15.0));
        root.addSubview(&detail);
        parent.addSubview(&root);
        let c = Card {
            root,
            icon,
            title: title_l,
            accessory,
            value,
            unit,
            detail,
            width: w,
        };
        c.set_symbol(symbol);
        c
    }

    fn set_symbol(&self, name: &str) {
        self.icon.setImage(ui::symbol(name, 13.0).as_deref());
    }

    /// The big number and its unit, laid out side by side.
    fn set_value(&self, value: &str, unit: &str) {
        self.value.setStringValue(&ns(value));
        let vw = ui::text_width(&self.value);
        self.unit.setStringValue(&ns(unit));
        self.unit.setFrame(rect(
            PAD + vw + 3.0,
            UNIT_Y,
            (self.width - PAD * 2.0 - vw - 3.0).max(0.0),
            16.0,
        ));
    }

    fn set_detail(&self, text: &str) {
        self.detail.setStringValue(&ns(text));
    }

    fn set_tip(&self, tip: &str) {
        let t = ns(tip);
        self.root
            .setToolTip(if tip.is_empty() { None } else { Some(&t) });
    }
}

struct Views {
    window: Retained<KeyPanel>,
    _delegate: Retained<PanelDelegate>,
    /// Every card lives under this view, so detaching it frees them all.
    root: Retained<NSView>,
    status_icon: Retained<NSImageView>,
    status_title: Retained<NSTextField>,
    status_sub: Retained<NSTextField>,
    protection: Card,
    disk: Card,
    disk_bar: Bar,
    memory: Card,
    memory_bar: Bar,
    battery: Card,
    battery_bar: Bar,
    cpu: Card,
    cores: Retained<ui::Fill>,
    cores_gap: f64,
    network: Card,
    up_value: Retained<NSTextField>,
    up_unit: Retained<NSTextField>,
    devices: Card,
    device_rows: Vec<(Retained<NSTextField>, Retained<NSTextField>)>,
    rec: Card,
    rec_button: Retained<NSButton>,
    rec_later: Retained<NSButton>,
    gear: Retained<NSButton>,
}

/// Values that change slowly, refreshed every few ticks.
#[derive(Clone, Default)]
struct Slow {
    boot: Option<Volume>,
    xprotect: Option<kipimo_sys::XProtect>,
}

struct State {
    views: Views,
    timer: Option<Retained<NSTimer>>,
    monitors: Vec<Retained<AnyObject>>,
    sampler: Arc<Mutex<Option<Monitor>>>,
    busy: Arc<AtomicBool>,
    ticks: u64,
    slow: Slow,
    recommendation: Option<Recommendation>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LAST_CLOSE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    /// The last readings, shown the moment the panel opens while fresh ones are taken.
    static LAST_LIVE: RefCell<Option<(Live, Vec<BtDevice>)>> = const { RefCell::new(None) };
}

/// Take one reading shortly after launch, so even the first panel isn't blank.
pub fn warm_up() {
    util::background(|| {
        let mut m = Monitor::new();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let live = m.sample();
        drop(m);
        let devices = kipimo_sys::bluetooth::devices();
        util::main_async(move || {
            LAST_LIVE.with(|l| {
                let mut l = l.borrow_mut();
                if l.is_none() {
                    *l = Some((live, devices));
                }
            });
            util::release_memory_soon();
        });
    });
}

/// Samples rendered since the panel opened (for the self-test).
#[cfg(dev)]
pub fn ticks() -> u64 {
    STATE.with(|s| s.borrow().as_ref().map(|s| s.ticks).unwrap_or(0))
}

/// The panel's window number.
#[cfg(dev)]
pub fn window_number() -> Option<isize> {
    STATE.with(|s| s.borrow().as_ref().map(|s| s.views.window.windowNumber()))
}

pub fn is_open() -> bool {
    STATE.with(|s| s.borrow().is_some())
}

/// Open, or close if open (the status item's click).
pub fn toggle() {
    if is_open() {
        close();
    } else if LAST_CLOSE
        .with(|l| l.get())
        .is_none_or(|t| t.elapsed().as_secs_f64() > 0.3)
    {
        // A click on the icon can first close the panel by taking key focus;
        // don't reopen it from the same click.
        open();
    }
}

fn status_anchor(mtm: MainThreadMarker) -> Option<NSRect> {
    let button = app::status_button(mtm)?;
    let win = button.window()?;
    Some(win.frame())
}

fn frame_under_icon(mtm: MainThreadMarker) -> NSRect {
    let anchor = status_anchor(mtm);
    let screen = NSScreen::mainScreen(mtm)
        .map(|s| s.visibleFrame())
        .unwrap_or(rect(0.0, 0.0, 1440.0, 900.0));
    let (mid_x, top) = match anchor {
        Some(a) => (a.origin.x + a.size.width / 2.0, a.origin.y - 6.0),
        None => (
            screen.origin.x + screen.size.width - W / 2.0 - 8.0,
            screen.origin.y + screen.size.height - 6.0,
        ),
    };
    let min_x = screen.origin.x + 8.0;
    let max_x = screen.origin.x + screen.size.width - W - 8.0;
    let x = (mid_x - W / 2.0).clamp(min_x, max_x.max(min_x));
    rect(x, top - H, W, H)
}

fn build(mtm: MainThreadMarker) -> Views {
    let frame = frame_under_icon(mtm);
    let window: Retained<KeyPanel> = unsafe {
        msg_send![
            KeyPanel::alloc(mtm),
            initWithContentRect: frame,
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(NSStatusWindowLevel);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(true);
    window.setHidesOnDeactivate(false);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Transient,
    );
    let surface = ui::hud_surface(mtm, rect(0.0, 0.0, W, H), 12.0);
    let root = ui::flipped(mtm, rect(0.0, 0.0, W, H));
    // A dark wash under the cards keeps them legible over bright windows.
    root.addSubview(&ui::fill(
        mtm,
        rect(0.0, 0.0, W, H),
        &NSColor::colorWithWhite_alpha(0.0, 0.4),
        0.0,
    ));
    surface.addSubview(&root);

    // Header: overall status and the Mac.
    let mut y = PAD;
    let status_icon =
        NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(PAD, y + 6.0, 24.0, 24.0));
    root.addSubview(&status_icon);
    let status_title = ui::label(mtm, "Mac Health", 15.0, Weight::Semibold, &white(0.95));
    status_title.setFrame(rect(PAD + 32.0, y + 2.0, W - PAD * 2.0 - 32.0, 20.0));
    root.addSubview(&status_title);
    let status_sub = ui::label(mtm, &app::model().name, 11.0, Weight::Regular, &white(0.6));
    status_sub.setFrame(rect(PAD + 32.0, y + 23.0, W - PAD * 2.0 - 32.0, 15.0));
    root.addSubview(&status_sub);
    y += HEADER_H + GAP;

    let protection = Card::new(
        mtm,
        &root,
        rect(PAD, y, W - PAD * 2.0, PROTECTION_H),
        "checkmark.shield",
        "Protection",
    );
    protection.value.setHidden(true);
    protection.unit.setHidden(true);
    y += PROTECTION_H + GAP;

    let left = PAD;
    let right = PAD + HALF + GAP;
    let bar_frame = |w: f64| rect(PAD, BAR_Y, w - PAD * 2.0, 4.0);

    let disk = Card::new(
        mtm,
        &root,
        rect(left, y, HALF, CARD_H),
        "internaldrive",
        "Macintosh HD",
    );
    let disk_bar = Bar::new(mtm, &disk.root, bar_frame(HALF));
    let clean = ui::text_button(
        mtm,
        "Clean up",
        Command::OpenTools(crate::tools_window::Tab::Clean),
    );
    clean.setControlSize(NSControlSize::Small);
    clean.setBezelStyle(NSBezelStyle::Push);
    clean.setFont(Some(&ui::font(11.0, Weight::Regular)));
    clean.setFrame(rect(
        HALF - PAD - 66.0 + 5.0,
        CARD_H - PAD - 19.0,
        66.0,
        24.0,
    ));
    disk.accessory.setHidden(true);
    disk.detail.setFrame(rect(
        PAD,
        CARD_H - PAD - 13.0,
        HALF - PAD * 2.0 - 66.0,
        15.0,
    ));
    disk.root.addSubview(&clean);

    let memory = Card::new(
        mtm,
        &root,
        rect(right, y, HALF, CARD_H),
        "memorychip",
        "Memory",
    );
    let memory_bar = Bar::new(mtm, &memory.root, bar_frame(HALF));
    y += CARD_H + GAP;

    let battery = Card::new(
        mtm,
        &root,
        rect(left, y, HALF, CARD_H),
        "battery.75percent",
        "Battery",
    );
    let battery_bar = Bar::new(mtm, &battery.root, bar_frame(HALF));

    let cpu = Card::new(mtm, &root, rect(right, y, HALF, CARD_H), "cpu", "CPU");
    let n = kipimo_sys::cpu::core_count().clamp(1, 64);
    let gap = if n > 24 { 1.0 } else { 2.0 };
    let cores = ui::Fill::new(
        mtm,
        rect(PAD, BAR_Y - 3.0, HALF - PAD * 2.0, 13.0),
        &white(0.8),
        1.0,
        ui::Paint::Columns(vec![0.0; n], gap),
    );
    cpu.root.addSubview(&cores);
    y += CARD_H + GAP;

    let network = Card::new(
        mtm,
        &root,
        rect(left, y, HALF, CARD_H),
        "arrow.up.arrow.down",
        "Network",
    );
    network
        .value
        .setFont(Some(&ui::font(17.0, Weight::BigDigits)));
    network
        .value
        .setFrame(rect(PAD, 33.0, HALF - PAD * 2.0, 22.0));
    let up_value = ui::label(mtm, "–", 17.0, Weight::BigDigits, &white(0.95));
    up_value.setFrame(rect(PAD, 61.0, HALF - PAD * 2.0, 22.0));
    network.root.addSubview(&up_value);
    let up_unit = ui::label(mtm, "", 12.0, Weight::Regular, &white(0.6));
    network.root.addSubview(&up_unit);
    network.detail.setHidden(true);

    let devices = Card::new(
        mtm,
        &root,
        rect(right, y, HALF, CARD_H),
        "headphones",
        "Devices",
    );
    devices.value.setHidden(true);
    devices.unit.setHidden(true);
    devices
        .detail
        .setFrame(rect(PAD, 34.0, HALF - PAD * 2.0, 30.0));
    devices.detail.setUsesSingleLineMode(false);
    devices
        .detail
        .setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
    let mut device_rows = Vec::new();
    for i in 0..2 {
        let top = 32.0 + i as f64 * 31.0;
        let name = ui::label(mtm, "", 12.0, Weight::Medium, &white(0.9));
        name.setFrame(rect(PAD, top, HALF - PAD * 2.0, 16.0));
        let parts = ui::label(mtm, "", 11.0, Weight::Digits, &white(0.6));
        parts.setFrame(rect(PAD, top + 15.0, HALF - PAD * 2.0, 14.0));
        devices.root.addSubview(&name);
        devices.root.addSubview(&parts);
        device_rows.push((name, parts));
    }
    y += CARD_H + GAP;

    let rec = Card::new(
        mtm,
        &root,
        rect(PAD, y, W - PAD * 2.0, REC_H),
        "lightbulb",
        "Today’s recommendation",
    );
    rec.value.setFont(Some(&ui::font(13.0, Weight::Medium)));
    rec.value
        .setFrame(rect(PAD, 30.0, W - PAD * 4.0 - 100.0, 17.0));
    rec.unit.setHidden(true);
    rec.detail
        .setFrame(rect(PAD, 46.0, W - PAD * 4.0 - 100.0, 14.0));
    let rec_button = ui::text_button(mtm, "", Command::Recommendation);
    rec_button.setControlSize(NSControlSize::Small);
    rec_button.setFont(Some(&ui::font(11.0, Weight::Regular)));
    rec_button.setFrame(rect(W - PAD * 3.0 - 96.0 + 4.0, 30.0, 96.0, 24.0));
    rec.root.addSubview(&rec_button);
    let rec_later = ui::text_button(mtm, "Later", Command::SnoozeRecommendation);
    rec_later.setBordered(false);
    rec_later.setFont(Some(&ui::font(11.0, Weight::Regular)));
    rec_later.setContentTintColor(Some(&white(0.6)));
    rec_later.setFrame(rect(W - PAD * 3.0 - 40.0, 9.0, 40.0, 17.0));
    rec_later.setToolTip(Some(&ns("Hide this suggestion until tomorrow")));
    rec.accessory.setHidden(true);
    rec.root.addSubview(&rec_later);
    y += REC_H + 10.0;

    // Footer.
    let open = ui::text_button(
        mtm,
        "Open Kipimo",
        Command::OpenTools(crate::tools_window::Tab::Clean),
    );
    open.setFrame(rect(PAD - 4.0, y - 1.0, 124.0, 30.0));
    root.addSubview(&open);
    let gear = ui::icon_button(mtm, "gearshape", "Settings and Quit", Command::GearMenu);
    gear.setFrame(rect(W - PAD - 26.0, y + 2.0, 26.0, 24.0));
    root.addSubview(&gear);

    window.setContentView(Some(&surface));
    let this = PanelDelegate::alloc(mtm).set_ivars(());
    let delegate: Retained<PanelDelegate> = unsafe { msg_send![super(this), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    Views {
        window,
        _delegate: delegate,
        root,
        status_icon,
        status_title,
        status_sub,
        protection,
        disk,
        disk_bar,
        memory,
        memory_bar,
        battery,
        battery_bar,
        cpu,
        cores,
        cores_gap: gap,
        network,
        up_value,
        up_unit,
        devices,
        device_rows,
        rec,
        rec_button,
        rec_later,
        gear,
    }
}

/// Open the panel under the menu-bar icon.
pub fn open() {
    util::log("panel: open");
    let mtm = util::mtm();
    if is_open() {
        return;
    }
    let views = build(mtm);
    let sampler: Arc<Mutex<Option<Monitor>>> = Arc::new(Mutex::new(None));
    {
        let s = sampler.clone();
        // IOReport and SMC setup take ~80 ms; keep them off the main thread.
        util::background(move || {
            let m = Monitor::new();
            *s.lock().unwrap() = Some(m);
        });
    }

    // Escape closes; a click anywhere outside Kipimo closes.
    let mut monitors = Vec::new();
    let esc = RcBlock::new(|e: NonNull<NSEvent>| -> *mut NSEvent {
        let ev = unsafe { e.as_ref() };
        if ev.keyCode() == KEY_ESCAPE && is_open() {
            util::log("panel: Escape");
            util::main_async(close);
            return std::ptr::null_mut();
        }
        e.as_ptr()
    });
    if let Some(m) =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &esc) }
    {
        monitors.push(m);
    }
    let outside = RcBlock::new(|_e: NonNull<NSEvent>| {
        util::log("panel: click outside");
        util::main_async(close);
    });
    if let Some(m) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
        NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown,
        &outside,
    ) {
        monitors.push(m);
    }

    let s = settings::get();
    let target = ui::controller(mtm);
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            s.refresh_secs,
            &target,
            sel!(panelTick:),
            None,
            true,
        )
    };
    timer.setTolerance(0.05);
    // CPU and network rates need two samples; the cached readings cover the wait.
    timer.setFireDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.3));

    views.window.makeKeyAndOrderFront(None);
    app::set_highlight(true);
    STATE.with(|st| {
        *st.borrow_mut() = Some(State {
            views,
            timer: Some(timer),
            monitors,
            sampler,
            busy: Arc::new(AtomicBool::new(false)),
            ticks: 0,
            slow: Slow {
                boot: None,
                xprotect: Some(kipimo_sys::xprotect::status()),
            },
            recommendation: None,
        })
    });
    // Show what's already known right away.
    render_static();
    if let Some((live, devices)) = LAST_LIVE.with(|l| l.borrow().clone()) {
        render_live(&live);
        render_devices(&devices);
    }
    refresh_slow();
    crate::insights::revalidate_leftovers();
}

/// Close the panel and drop everything it held.
pub fn close() {
    if is_open() {
        util::log("panel: close");
    }
    let Some(st) = STATE.with(|s| s.borrow_mut().take()) else {
        return;
    };
    if let Some(t) = &st.timer {
        t.invalidate();
    }
    for m in &st.monitors {
        unsafe { NSEvent::removeMonitor(m) };
    }
    let w = &st.views.window;
    st.views.root.removeFromSuperview();
    ui::release_window_contents(w);
    w.orderOut(None);
    w.setDelegate(None);
    // Ordering out alone also keeps the window in AppKit's window list.
    w.close();
    app::set_highlight(false);
    LAST_CLOSE.with(|l| l.set(Some(Instant::now())));
    drop(st);
    util::release_memory_soon();
}

/// Timer tick: sample on a background queue, then render on the main thread.
pub fn tick() {
    let Some((sampler, busy, n)) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = s.as_mut()?;
        s.ticks += 1;
        Some((s.sampler.clone(), s.busy.clone(), s.ticks))
    }) else {
        return;
    };
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    // Disk free space and the Trash change slowly and cost more to read: every ~10 s.
    if n % 5 == 0 {
        refresh_slow();
    }
    util::background(move || {
        let live = sampler.lock().unwrap().as_mut().map(Monitor::sample);
        let devices = kipimo_sys::bluetooth::devices();
        util::main_async(move || {
            busy.store(false, Ordering::Release);
            render_devices(&devices);
            background::feed_devices(&devices);
            if let Some(live) = live {
                render_live(&live);
                LAST_LIVE.with(|l| *l.borrow_mut() = Some((live, devices)));
            }
        });
    });
}

fn refresh_slow() {
    util::background(|| {
        let boot = kipimo_sys::disk::boot_volume();
        let trash = background::measure_trash();
        util::main_async(move || {
            if let Some(b) = &boot {
                background::feed_disk(b.available_important, trash);
            } else {
                background::feed_disk_trash_only(trash);
            }
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.slow.boot = boot;
                }
            });
            render_static();
        });
    });
}

fn with_views(f: impl FnOnce(&Views, &Slow)) {
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            f(&s.views, &s.slow);
        }
    });
}

/// Header, Protection, Disk and the recommendation.
fn render_static() {
    let reasons = app::reasons();
    let rec = app::recommendation();
    with_views(|v, slow| {
        // Header.
        let (sym, color, title) = if reasons.is_empty() {
            (
                "checkmark.circle.fill",
                NSColor::systemGreenColor(),
                "Mac Health: Good",
            )
        } else {
            (
                "exclamationmark.triangle.fill",
                NSColor::systemOrangeColor(),
                "Mac Health: Needs attention",
            )
        };
        v.status_icon.setImage(ui::symbol(sym, 18.0).as_deref());
        v.status_icon.setContentTintColor(Some(&color));
        v.status_title.setStringValue(&ns(title));
        let m = app::model();
        let sub = if reasons.is_empty() {
            format!("{} · {} memory", m.name, format::memory(m.memory_bytes))
        } else {
            reasons
                .iter()
                .map(|r| r.text())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        v.status_sub.setStringValue(&ns(&sub));
        v.status_sub.setToolTip(Some(&ns(&format!(
            "{} ({}, {})",
            m.name, m.identifier, m.chip
        ))));

        // Protection.
        if let Some(x) = &slow.xprotect {
            if x.installed {
                v.protection.set_symbol("checkmark.shield.fill");
                v.protection
                    .icon
                    .setContentTintColor(Some(&NSColor::systemGreenColor()));
                let mut parts = vec!["XProtect is on".to_string()];
                if let Some(ver) = &x.version {
                    parts.push(format!("definitions {ver}"));
                }
                if let Some(t) = x.updated.and_then(|t| t.elapsed().ok()) {
                    parts.push(format!("updated {}", format::days_ago(t.as_secs())));
                }
                v.protection.set_detail(&parts.join(" · "));
                let rem = x
                    .remediator_version
                    .as_deref()
                    .map(|r| format!("XProtect Remediator {r}. "))
                    .unwrap_or_default();
                v.protection.set_tip(&format!(
                    "macOS's built-in malware protection. {rem}It checks apps when they open and scans in the background."
                ));
            } else {
                v.protection.set_symbol("exclamationmark.shield");
                v.protection
                    .icon
                    .setContentTintColor(Some(&NSColor::systemOrangeColor()));
                v.protection
                    .set_detail("XProtect's definitions weren't found on this Mac");
            }
        }

        // Disk.
        if let Some(b) = &slow.boot {
            v.disk.title.setStringValue(&ns(&b.name));
            let (num, unit) = format::bytes_parts(b.available_important);
            v.disk.set_value(&num, &format!("{unit} available"));
            v.disk.set_detail(&format!("of {}", format::bytes(b.total)));
            let low = app::disk_low();
            let used = if b.total > 0 {
                b.used() as f64 / b.total as f64
            } else {
                0.0
            };
            v.disk_bar.set(used, &warn_or(low, 0.85));
            let trash = background::trash_bytes()
                .map(|t| format!("\nTrash: {}", format::bytes(t)))
                .unwrap_or_default();
            v.disk.set_tip(&format!(
                "{} available, including {} macOS can purge when it needs room\n{} used of {}{trash}",
                format::bytes(b.available_important),
                format::bytes(b.purgeable),
                format::bytes(b.used()),
                format::bytes(b.total)
            ));
        }

        // Recommendation.
        match &rec {
            Some(r) => {
                v.rec
                    .set_symbol(if r.nag { "lightbulb.max" } else { "lightbulb" });
                v.rec.icon.setContentTintColor(Some(&warn_or(r.nag, 0.6)));
                v.rec.value.setStringValue(&ns(&r.title));
                v.rec.set_detail(&r.detail);
                match &r.button {
                    Some((label, _)) => {
                        v.rec_button.setTitle(&ns(label));
                        v.rec_button.setHidden(false);
                    }
                    None => v.rec_button.setHidden(true),
                }
                v.rec_later.setHidden(false);
            }
            None => {
                v.rec.set_symbol("lightbulb");
                v.rec.icon.setContentTintColor(Some(&white(0.6)));
                v.rec.value.setStringValue(&ns("Nothing needs doing"));
                v.rec.set_detail("Disk space and the Trash look fine.");
                v.rec_button.setHidden(true);
                v.rec_later.setHidden(true);
            }
        }
    });
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.recommendation = rec;
        }
    });
}

fn battery_symbol(pct: u8, charging: bool) -> String {
    let level = match pct {
        0..=12 => "0",
        13..=37 => "25",
        38..=62 => "50",
        63..=87 => "75",
        _ => "100",
    };
    if charging {
        "battery.100percent.bolt".into()
    } else {
        format!("battery.{level}percent")
    }
}

fn render_live(l: &Live) {
    with_views(|v, _| {
        // Memory.
        if let Some(m) = &l.memory {
            v.memory
                .set_value(&m.pressure_percent.to_string(), "% pressure");
            v.memory.set_detail(&format!(
                "{} of {} used",
                format::memory(m.used),
                format::memory(m.total)
            ));
            let color = match m.level {
                Some(PressureLevel::Critical) => NSColor::systemRedColor(),
                Some(PressureLevel::Warning) => NSColor::systemOrangeColor(),
                _ => white(0.85),
            };
            v.memory_bar.set(m.pressure_percent as f64 / 100.0, &color);
            v.memory.set_tip(&format!(
                "Used {} (apps {}, wired {}, compressed {})\nCached files {}\nSwap {} of {}",
                format::memory(m.used),
                format::memory(m.app),
                format::memory(m.wired),
                format::memory(m.compressed),
                format::memory(m.cached),
                format::memory(m.swap_used),
                format::memory(m.swap_total)
            ));
        }

        // Battery and power.
        let watts = l.power.system_watts;
        let chip = l.power.chip.map(|c| c.total());
        match &l.battery {
            Some(b) => {
                v.battery.title.setStringValue(&ns("Battery"));
                v.battery.set_symbol(&battery_symbol(b.percent, b.charging));
                v.battery.set_value(&b.percent.to_string(), "%");
                let state = if b.charging {
                    match b.minutes_to_full {
                        Some(m) => format!("Charging · {} to full", format::minutes(m)),
                        None => "Charging".into(),
                    }
                } else if b.on_ac {
                    if b.percent < 100 {
                        "On power · not charging".into()
                    } else {
                        "On power · charged".into()
                    }
                } else {
                    match b.minutes_to_empty {
                        Some(m) => format!("{} left", format::minutes(m)),
                        None => "On battery · estimating".into(),
                    }
                };
                v.battery.set_detail(&state);
                v.battery_bar.set(
                    b.percent as f64 / 100.0,
                    &warn_or(b.percent <= 10 && !b.on_ac, 0.85),
                );
                let mut tip = Vec::new();
                if let Some(c) = &b.condition {
                    tip.push(format!("Condition: {c}"));
                }
                if let Some(c) = b.cycle_count {
                    tip.push(format!("{c} charge cycles"));
                }
                if let Some(c) = b.max_capacity_percent {
                    tip.push(format!("Maximum capacity {c}%"));
                }
                if let Some(p) = l.power.chip {
                    tip.push(format!(
                        "Chip {:.1} W (CPU {:.1}, GPU {:.1}, Neural Engine {:.1})",
                        p.total(),
                        p.cpu_watts,
                        p.gpu_watts,
                        p.ane_watts
                    ));
                }
                v.battery.set_tip(&tip.join("\n"));
            }
            None => {
                // A desktop Mac: show the power draw as the main value.
                v.battery.title.setStringValue(&ns("Power"));
                v.battery.set_symbol("powerplug");
                match watts.or(chip) {
                    Some(w) => v.battery.set_value(&format!("{w:.0}"), "W"),
                    None => v.battery.set_value("–", ""),
                }
                v.battery
                    .set_detail(if watts.is_some() { "Whole Mac" } else { "Chip" });
                v.battery_bar.set(0.0, &white(0.85));
            }
        }
        let watts_text = match (watts, chip) {
            (Some(w), _) if l.battery.is_some() => format!("{w:.1} W"),
            (None, Some(c)) if l.battery.is_some() => format!("{c:.1} W chip"),
            _ => String::new(),
        };
        v.battery.accessory.setStringValue(&ns(&watts_text));
        v.battery
            .accessory
            .setToolTip(Some(&ns("Power the whole Mac is drawing right now")));

        // CPU.
        if let Some(c) = &l.cpu {
            v.cpu.set_value(&format::percent(c.total), "% load");
            let values = c.per_core.iter().map(|p| *p as f64 / 100.0).collect();
            v.cores
                .set(&white(0.8), ui::Paint::Columns(values, v.cores_gap));
        }
        let hot = l.thermal.is_some_and(|t| t >= ThermalState::Serious);
        match &l.temperature {
            Some(t) => {
                v.cpu
                    .accessory
                    .setStringValue(&ns(&format!("{:.0} °C", t.cpu_avg)));
                v.cpu.accessory.setTextColor(Some(&warn_or(hot, 0.6)));
                let gpu = t
                    .gpu_avg
                    .map(|g| format!(", GPU {g:.0} °C"))
                    .unwrap_or_default();
                v.cpu.accessory.setToolTip(Some(&ns(&format!(
                    "CPU {:.0} °C average, {:.0} °C hottest{gpu}",
                    t.cpu_avg, t.cpu_max
                ))));
            }
            None => v
                .cpu
                .accessory
                .setStringValue(&ns(if hot { "Hot" } else { "" })),
        }
        match l.top_processes.first() {
            Some(p) => v
                .cpu
                .set_detail(&format!("{} {}%", p.name, format::percent(p.percent))),
            None => v.cpu.set_detail(""),
        }
        let top = l
            .top_processes
            .iter()
            .map(|p| format!("{}  {}%", p.name, format::percent(p.percent)))
            .collect::<Vec<_>>()
            .join("\n");
        let cores = l
            .cpu
            .as_ref()
            .map(|c| {
                format!(
                    "{} cores · user {:.0}%, system {:.0}%",
                    c.per_core.len(),
                    c.user,
                    c.system
                )
            })
            .unwrap_or_default();
        v.cpu.set_tip(&format!(
            "{cores}\n\nBusiest apps (100% = one core):\n{top}"
        ));

        // Network.
        if let Some(n) = &l.network {
            let (d, du) = split_rate(n.down_bytes_per_sec);
            v.network.set_value(&format!("↓ {d}"), &du);
            let (u, uu) = split_rate(n.up_bytes_per_sec);
            v.up_value.setStringValue(&ns(&format!("↑ {u}")));
            let uw = ui::text_width(&v.up_value);
            v.up_unit.setStringValue(&ns(&uu));
            v.up_unit.setFrame(rect(PAD + uw + 3.0, 64.0, 60.0, 16.0));
            // The value row uses a smaller font than other cards; align the unit to it.
            let dw = ui::text_width(&v.network.value);
            v.network
                .unit
                .setFrame(rect(PAD + dw + 3.0, 36.0, 60.0, 16.0));
            v.network
                .set_tip("Download and upload across Wi‑Fi, Ethernet and cellular");
        }
    });
}

fn split_rate(bps: f64) -> (String, String) {
    let s = format::rate(bps);
    match s.split_once(' ') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s, String::new()),
    }
}

fn device_symbol(d: &BtDevice) -> &'static str {
    let n = d.name.to_lowercase();
    match d.category.as_str() {
        _ if n.contains("airpods max") => "airpodsmax",
        _ if n.contains("airpods pro") => "airpodspro",
        _ if n.contains("airpods") => "airpods",
        "Keyboard" => "keyboard",
        "Mouse" => "computermouse",
        "Trackpad" => "rectangle.and.hand.point.up.left",
        "Headset" | "Headphones" => "headphones",
        "Gamepad" => "gamecontroller",
        _ => "dot.radiowaves.left.and.right",
    }
}

fn render_devices(devices: &[BtDevice]) {
    let limit = settings::get().bluetooth_pct;
    with_views(|v, _| {
        v.devices
            .set_symbol(devices.first().map(device_symbol).unwrap_or("headphones"));
        if devices.is_empty() {
            v.devices.detail.setHidden(false);
            v.devices
                .set_detail("No connected devices report a battery");
        } else {
            v.devices.detail.setHidden(true);
        }
        for (i, (name, parts)) in v.device_rows.iter().enumerate() {
            match devices.get(i) {
                Some(d) => {
                    name.setStringValue(&ns(&d.name));
                    parts.setStringValue(&ns(&format::parts(&d.parts)));
                    let low = d.lowest().is_some_and(|p| p < limit);
                    parts.setTextColor(Some(&warn_or(low, 0.6)));
                    name.setHidden(false);
                    parts.setHidden(false);
                }
                None => {
                    name.setHidden(true);
                    parts.setHidden(true);
                }
            }
        }
        let tip = devices
            .iter()
            .map(|d| format!("{}: {}", d.name, format::parts(&d.parts)))
            .collect::<Vec<_>>()
            .join("\n");
        v.devices.set_tip(&tip);
    });
}

/// The recommendation card's button.
pub fn recommendation_clicked() {
    let a = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|s| s.recommendation.as_ref())
            .and_then(|r| r.button.clone())
    });
    if let Some((_, action)) = a {
        crate::actions::run(action);
    }
}

pub fn snooze_recommendation() {
    let id = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|s| s.recommendation.as_ref())
            .map(|r| r.id.clone())
    });
    if let Some(id) = id {
        crate::recommend::snooze(&id);
        app::refresh_attention();
        render_static();
    }
}

/// The gear: Settings, Launch at Login, Quit.
pub fn gear_menu() {
    let mtm = util::mtm();
    let Some(gear) = STATE.with(|s| s.borrow().as_ref().map(|s| s.views.gear.clone())) else {
        return;
    };
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let item = |title: &str, cmd: Command, key: &str| {
        let i = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                Some(sel!(command:)),
                &ns(key),
            )
        };
        unsafe { i.setTarget(Some(&ui::controller(mtm))) };
        i.setTag(cmd.tag());
        i
    };
    menu.addItem(&item("Settings…", Command::OpenSettings, ","));
    let login = item("Launch at Login", Command::ToggleLogin, "");
    login.setState(if app::login_item_enabled() {
        objc2_app_kit::NSControlStateValueOn
    } else {
        objc2_app_kit::NSControlStateValueOff
    });
    menu.addItem(&login);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item("Quit Kipimo", Command::Quit, "q"));
    MENU_OPEN.with(|m| m.set(true));
    let p = NSPoint::new(0.0, gear.bounds().size.height + 4.0);
    menu.popUpMenuPositioningItem_atLocation_inView(None, p, Some(&gear));
    MENU_OPEN.with(|m| m.set(false));
}
