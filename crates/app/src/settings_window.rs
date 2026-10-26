//! The Settings window: General, Alerts and Weekly summary tabs, built like
//! Photon's (stock controls in a native tabbed window, a 150 pt right-aligned
//! label column). Every control carries `Command::Setting(id)` and writes
//! straight to settings on change.

use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSBackingStoreType, NSButton, NSColor, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSDatePicker, NSDatePickerElementFlags, NSDatePickerStyle,
    NSPopUpButton, NSTabView, NSTabViewItem, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSCalendar, NSCalendarOptions, NSCalendarUnit, NSDate, NSNotification, NSObjectProtocol, NSURL,
};

use crate::commands::Command;
use crate::settings::{
    self, BLUETOOTH_PCT_CHOICES, DISK_GB_CHOICES, REFRESH_CHOICES, TRASH_GB_CHOICES,
};
use crate::ui::{self, Weight, rect};
use crate::util::{self, ns};
use crate::{app, notify};

// Control ids.
const LOGIN: u16 = 1;
const TINT: u16 = 2;
const REFRESH: u16 = 3;
const FULL_DISK: u16 = 4;
const DISK: u16 = 10;
const DISK_GB: u16 = 11;
const TRASH: u16 = 12;
const TRASH_GB: u16 = 13;
const BLUETOOTH: u16 = 14;
const BLUETOOTH_PCT: u16 = 15;
const DRIVES: u16 = 16;
const LEFTOVERS: u16 = 17;
const TEST_ALERT: u16 = 18;
const WEEKLY: u16 = 30;
const WEEKDAY: u16 = 31;
const TIME: u16 = 32;
const SUMMARY_NOW: u16 = 33;

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

define_class!(
    // SAFETY: NSObject subclass acting as the window delegate; no Drop impl.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoSettingsDelegate"]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            util::main_async(closed);
        }
    }
);

struct State {
    window: Retained<NSWindow>,
    _delegate: Retained<WindowDelegate>,
    controls: HashMap<u16, Retained<NSView>>,
    login_note: Retained<NSTextField>,
    access_note: Retained<NSTextField>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Top-down form layout inside a flipped container (Photon's rhythm).
struct Form {
    view: Retained<NSView>,
    y: f64,
    width: f64,
    controls: Vec<(u16, Retained<NSView>)>,
}

const LABEL_W: f64 = 150.0;
const X: f64 = 24.0;
const CX: f64 = X + LABEL_W + 10.0;

impl Form {
    fn new(mtm: MainThreadMarker, width: f64, height: f64) -> Form {
        Form {
            view: ui::flipped(mtm, rect(0.0, 0.0, width, height)),
            y: 18.0,
            width,
            controls: Vec::new(),
        }
    }

    fn label(&self, text: &str, y: f64, h: f64) {
        let l = NSTextField::labelWithString(&ns(text), util::mtm());
        l.setAlignment(NSTextAlignment::Right);
        l.setFrame(rect(X, y + (h - 17.0) / 2.0, LABEL_W, 17.0));
        self.view.addSubview(&l);
    }

    fn section(&mut self, title: &str) {
        if self.y > 20.0 {
            self.y += 10.0;
        }
        let l = ui::label(
            util::mtm(),
            title,
            13.0,
            Weight::Semibold,
            &NSColor::labelColor(),
        );
        l.setFrame(rect(X, self.y, self.width - 2.0 * X, 18.0));
        self.view.addSubview(&l);
        self.y += 26.0;
    }

    /// A labelled row; `w` is the control width.
    fn row(&mut self, label: &str, id: u16, control: Retained<NSView>, w: f64, h: f64) {
        self.label(label, self.y, h);
        control.setFrame(rect(CX, self.y, w, h));
        self.view.addSubview(&control);
        self.controls.push((id, control));
        self.y += h + 8.0;
    }

    /// A control in the control column, with an optional second control beside it.
    fn plain(
        &mut self,
        id: u16,
        control: Retained<NSView>,
        w: f64,
        h: f64,
        beside: Option<(u16, Retained<NSView>, f64)>,
    ) {
        control.setFrame(rect(CX, self.y, w, h));
        self.view.addSubview(&control);
        if id != 0 {
            self.controls.push((id, control));
        }
        if let Some((bid, b, bw)) = beside {
            b.setFrame(rect(CX + w + 6.0, self.y - 3.0, bw, 25.0));
            self.view.addSubview(&b);
            self.controls.push((bid, b));
        }
        self.y += h + 8.0;
    }

    fn note(&mut self, text: &str) -> Retained<NSTextField> {
        let l = NSTextField::wrappingLabelWithString(&ns(text), util::mtm());
        l.setFont(Some(&ui::font(11.0, Weight::Regular)));
        l.setTextColor(Some(&NSColor::secondaryLabelColor()));
        l.setFrame(rect(CX, self.y - 4.0, self.width - CX - X, 30.0));
        self.view.addSubview(&l);
        self.y += 30.0;
        l
    }

    fn button(&mut self, title: &str, id: u16) {
        let b = ui::text_button(util::mtm(), title, Command::Setting(id));
        let w = b.fittingSize().width.max(120.0);
        b.setFrame(rect(CX - 6.0, self.y - 2.0, w + 12.0, 28.0));
        self.view.addSubview(&b);
        self.y += 32.0;
    }
}

fn target_action(c: &NSControl, id: u16) {
    let t = ui::controller(util::mtm());
    unsafe {
        c.setTarget(Some(&t));
        c.setAction(Some(sel!(command:)));
    }
    c.setTag(Command::Setting(id).tag());
}

fn checkbox(title: &str, id: u16, on: bool) -> Retained<NSView> {
    let t = ui::controller(util::mtm());
    let b = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &ns(title),
            Some(&t),
            Some(sel!(command:)),
            util::mtm(),
        )
    };
    b.setTag(Command::Setting(id).tag());
    b.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    Retained::into_super(Retained::into_super(b))
}

fn popup(items: &[String], selected: usize, id: u16) -> Retained<NSView> {
    let p = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(util::mtm()),
        rect(0.0, 0.0, 100.0, 25.0),
        false,
    );
    for i in items {
        p.addItemWithTitle(&ns(i));
    }
    p.selectItemAtIndex(selected.min(items.len().saturating_sub(1)) as isize);
    target_action(&p, id);
    Retained::into_super(Retained::into_super(Retained::into_super(p)))
}

fn pos<T: PartialEq>(all: &[T], v: T) -> usize {
    all.iter().position(|x| *x == v).unwrap_or(0)
}

fn time_picker(minute: u16) -> Retained<NSView> {
    let mtm = util::mtm();
    let p = NSDatePicker::initWithFrame(NSDatePicker::alloc(mtm), rect(0.0, 0.0, 90.0, 24.0));
    p.setDatePickerStyle(NSDatePickerStyle::TextFieldAndStepper);
    p.setDatePickerElements(NSDatePickerElementFlags::HourMinute);
    p.setBezeled(true);
    p.setDateValue(&date_at(minute));
    target_action(&p, TIME);
    Retained::into_super(Retained::into_super(p))
}

fn date_at(minute: u16) -> Retained<NSDate> {
    let cal = NSCalendar::currentCalendar();
    cal.dateBySettingHour_minute_second_ofDate_options(
        (minute / 60) as isize,
        (minute % 60) as isize,
        0,
        &NSDate::now(),
        NSCalendarOptions::empty(),
    )
    .unwrap_or_else(NSDate::now)
}

fn minute_of(date: &NSDate) -> u16 {
    let cal = NSCalendar::currentCalendar();
    let h = cal.component_fromDate(NSCalendarUnit::Hour, date);
    let m = cal.component_fromDate(NSCalendarUnit::Minute, date);
    (h * 60 + m).clamp(0, 24 * 60 - 1) as u16
}

fn trash_access_text() -> &'static str {
    match kipimo_sys::disk::trash_size() {
        Err(kipimo_sys::TrashError::NoAccess) => {
            "Kipimo can't read the Trash yet. Allow Full Disk Access so it can measure the Trash and find leftovers."
        }
        _ => "Kipimo can read the Trash. Full Disk Access is only needed if this changes.",
    }
}

pub fn show() {
    let mtm = util::mtm();
    if let Some(w) = STATE.with(|s| s.borrow().as_ref().map(|s| s.window.clone())) {
        ui::app(mtm).activate();
        w.makeKeyAndOrderFront(None);
        return;
    }
    let s = settings::get();
    let (w, h) = (580.0, 420.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns("Kipimo Settings"));
    let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(0.0, 0.0, w, h));
    let (iw, ih) = (w - 20.0, h - 40.0);
    let mut controls: HashMap<u16, Retained<NSView>> = HashMap::new();

    // General.
    let mut g = Form::new(mtm, iw, ih);
    g.section("Startup");
    g.plain(
        LOGIN,
        checkbox("Launch Kipimo at login", LOGIN, app::login_item_enabled()),
        300.0,
        20.0,
        None,
    );
    let login_note = g.note("");
    g.section("Menu bar");
    g.plain(
        TINT,
        checkbox(
            "Colour the icon when something needs attention",
            TINT,
            s.tint_icon,
        ),
        330.0,
        20.0,
        None,
    );
    let refresh_labels = vec!["Every second".to_string(), "Every 2 seconds".to_string()];
    g.row(
        "Panel updates:",
        REFRESH,
        popup(
            &refresh_labels,
            pos(&REFRESH_CHOICES, s.refresh_secs),
            REFRESH,
        ),
        160.0,
        25.0,
    );
    g.note("Only while the panel is open. When it's closed, Kipimo checks once a minute.");
    g.section("Access");
    let access_note = g.note(trash_access_text());
    g.button("Open Full Disk Access…", FULL_DISK);

    // Alerts.
    let mut a = Form::new(mtm, iw, ih);
    a.section("Tell me when");
    let gb = |v: &[u32]| v.iter().map(|g| format!("{g} GB")).collect::<Vec<_>>();
    a.plain(
        DISK,
        checkbox("The startup disk has less than", DISK, s.alert_disk),
        220.0,
        20.0,
        Some((
            DISK_GB,
            popup(
                &gb(&DISK_GB_CHOICES),
                pos(&DISK_GB_CHOICES, s.disk_gb),
                DISK_GB,
            ),
            90.0,
        )),
    );
    a.plain(
        TRASH,
        checkbox("The Trash is larger than", TRASH, s.alert_trash),
        220.0,
        20.0,
        Some((
            TRASH_GB,
            popup(
                &gb(&TRASH_GB_CHOICES),
                pos(&TRASH_GB_CHOICES, s.trash_gb),
                TRASH_GB,
            ),
            90.0,
        )),
    );
    let pct = BLUETOOTH_PCT_CHOICES
        .iter()
        .map(|p| format!("{p}%"))
        .collect::<Vec<_>>();
    a.plain(
        BLUETOOTH,
        checkbox(
            "A Bluetooth device's battery is below",
            BLUETOOTH,
            s.alert_bluetooth,
        ),
        250.0,
        20.0,
        Some((
            BLUETOOTH_PCT,
            popup(
                &pct,
                pos(&BLUETOOTH_PCT_CHOICES, s.bluetooth_pct),
                BLUETOOTH_PCT,
            ),
            70.0,
        )),
    );
    a.plain(
        DRIVES,
        checkbox(
            "An external drive is connected or ejected",
            DRIVES,
            s.alert_drives,
        ),
        330.0,
        20.0,
        None,
    );
    a.plain(
        LEFTOVERS,
        checkbox(
            "An app moved to the Trash leaves files behind",
            LEFTOVERS,
            s.alert_leftovers,
        ),
        330.0,
        20.0,
        None,
    );
    a.note("Each alert comes once, when a value crosses its limit. macOS asks before Kipimo's first notification.");
    a.y += 6.0;
    a.button("Send a Test Notification", TEST_ALERT);

    // Weekly summary.
    let mut wk = Form::new(mtm, iw, ih);
    wk.section("Weekly summary");
    wk.plain(
        WEEKLY,
        checkbox("Send a weekly summary", WEEKLY, s.weekly),
        300.0,
        20.0,
        None,
    );
    let days = WEEKDAYS.iter().map(|d| d.to_string()).collect::<Vec<_>>();
    wk.row(
        "Day:",
        WEEKDAY,
        popup(&days, (s.weekly_weekday.clamp(1, 7) - 1) as usize, WEEKDAY),
        140.0,
        25.0,
    );
    wk.row("Time:", TIME, time_picker(s.weekly_minute), 90.0, 24.0);
    wk.note("One notification with how much space a cleanup would free and how many app updates are waiting. Clicking it opens Kipimo.");
    wk.y += 6.0;
    wk.button("Send One Now", SUMMARY_NOW);

    for (title, form) in [("General", g), ("Alerts", a), ("Weekly Summary", wk)] {
        let item =
            unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), Some(&ns(title))) };
        item.setLabel(&ns(title));
        item.setView(Some(&form.view));
        tabs.addTabViewItem(&item);
        controls.extend(form.controls);
    }
    window.setContentView(Some(&tabs));
    let this = WindowDelegate::alloc(mtm).set_ivars(());
    let delegate: Retained<WindowDelegate> = unsafe { msg_send![super(this), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.center();
    STATE.with(|st| {
        *st.borrow_mut() = Some(State {
            window: window.clone(),
            _delegate: delegate,
            controls,
            login_note,
            access_note,
        })
    });
    refresh();
    ui::app(mtm).activate();
    window.makeKeyAndOrderFront(None);
}

pub fn select_tab(i: isize) {
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref()
            && let Some(v) = s.window.contentView()
            && let Ok(tabs) = v.downcast::<NSTabView>()
        {
            tabs.selectTabViewItemAtIndex(i);
        }
    });
}

fn closed() {
    STATE.with(|s| s.borrow_mut().take());
    ui::app(util::mtm()).deactivate();
    util::release_memory_soon();
}

fn control<T: objc2::Message>(id: u16) -> Option<Retained<T>> {
    STATE.with(|s| {
        let s = s.borrow();
        let v = s.as_ref()?.controls.get(&id)?.clone();
        // SAFETY: each id is registered with the control type the caller asks for.
        Some(unsafe { Retained::cast_unchecked::<T>(v) })
    })
}

fn is_on(id: u16) -> bool {
    control::<NSButton>(id)
        .map(|b| b.state() == NSControlStateValueOn)
        .unwrap_or(false)
}

fn selected(id: u16) -> usize {
    control::<NSPopUpButton>(id)
        .map(|p| p.indexOfSelectedItem().max(0) as usize)
        .unwrap_or(0)
}

/// Sync enabled states and notes with the current settings.
pub fn refresh() {
    let s = settings::get();
    let login = app::login_item_enabled();
    if let Some(b) = control::<NSButton>(LOGIN) {
        b.setState(if login {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
    STATE.with(|st| {
        if let Some(st) = st.borrow().as_ref() {
            st.login_note.setStringValue(&ns(if login {
                "Kipimo starts with your Mac, quietly in the menu bar."
            } else {
                "Off: start Kipimo yourself after you log in."
            }));
            st.access_note.setStringValue(&ns(trash_access_text()));
        }
    });
    for (id, on) in [
        (DISK_GB, s.alert_disk),
        (TRASH_GB, s.alert_trash),
        (BLUETOOTH_PCT, s.alert_bluetooth),
        (WEEKDAY, s.weekly),
        (TIME, s.weekly),
    ] {
        if let Some(c) = control::<NSControl>(id) {
            c.setEnabled(on);
        }
    }
}

/// A control changed.
pub fn changed(id: u16) {
    match id {
        LOGIN => app::toggle_login_item(),
        TINT => {
            let on = is_on(id);
            settings::update(|s| s.tint_icon = on);
        }
        REFRESH => {
            let v = REFRESH_CHOICES[selected(id).min(REFRESH_CHOICES.len() - 1)];
            settings::update(|s| s.refresh_secs = v);
        }
        FULL_DISK => {
            if let Some(u) = NSURL::URLWithString(&ns(crate::recommend::FULL_DISK_ACCESS_URL)) {
                objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&u);
            }
        }
        DISK => {
            let on = is_on(id);
            settings::update(|s| s.alert_disk = on);
        }
        DISK_GB => {
            let v = DISK_GB_CHOICES[selected(id).min(DISK_GB_CHOICES.len() - 1)];
            settings::update(|s| s.disk_gb = v);
        }
        TRASH => {
            let on = is_on(id);
            settings::update(|s| s.alert_trash = on);
        }
        TRASH_GB => {
            let v = TRASH_GB_CHOICES[selected(id).min(TRASH_GB_CHOICES.len() - 1)];
            settings::update(|s| s.trash_gb = v);
        }
        BLUETOOTH => {
            let on = is_on(id);
            settings::update(|s| s.alert_bluetooth = on);
        }
        BLUETOOTH_PCT => {
            let v = BLUETOOTH_PCT_CHOICES[selected(id).min(BLUETOOTH_PCT_CHOICES.len() - 1)];
            settings::update(|s| s.bluetooth_pct = v);
        }
        DRIVES => {
            let on = is_on(id);
            settings::update(|s| s.alert_drives = on);
        }
        LEFTOVERS => {
            let on = is_on(id);
            settings::update(|s| s.alert_leftovers = on);
        }
        TEST_ALERT => notify::post(notify::Alert {
            id: "test".into(),
            title: "Kipimo notifications work".into(),
            body: "Alerts for disk space, the Trash, drives and Bluetooth batteries will look like this.".into(),
            kind: notify::Kind::Weekly,
            action: Some(crate::actions::Action::OpenPanel),
        }),
        WEEKLY => {
            let on = is_on(id);
            settings::update(|s| s.weekly = on);
        }
        WEEKDAY => {
            let d = selected(id).min(6) as u8 + 1;
            settings::update(|s| s.weekly_weekday = d);
        }
        TIME => {
            if let Some(p) = control::<NSDatePicker>(id) {
                let m = minute_of(&p.dateValue());
                settings::update(|s| s.weekly_minute = m);
            }
        }
        SUMMARY_NOW => crate::weekly::send(),
        _ => {}
    }
    refresh();
    // Thresholds and the tint change what the icon shows.
    crate::background::tick();
}

/// A modal error, for the rare failures the user should see.
pub fn show_error(title: &str, detail: &str) {
    let mtm = util::mtm();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&ns(title));
    alert.setInformativeText(&ns(detail));
    ui::app(mtm).activate();
    alert.runModal();
}
