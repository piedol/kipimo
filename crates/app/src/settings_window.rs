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
use crate::i18n::tr;
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
const UPDATES: u16 = 5;
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

/// Sunday first, as the weekly summary numbers them (1 = Sunday).
fn weekdays() -> Vec<String> {
    vec![
        tr("Sunday"),
        tr("Monday"),
        tr("Tuesday"),
        tr("Wednesday"),
        tr("Thursday"),
        tr("Friday"),
        tr("Saturday"),
    ]
}

define_class!(
    // SAFETY: NSObject subclass acting as the window delegate; no Drop impl.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoSettingsDelegate"]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, n: &NSNotification) {
            if let Some(w) = n.object().and_then(|o| o.downcast::<NSWindow>().ok()) {
                ui::release_window_contents(&w);
            }
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

    /// One alert: label, switch, and the limit popup in a fixed column.
    fn alert_row(
        &mut self,
        label: &str,
        id: u16,
        check: Retained<NSView>,
        limit: Option<(u16, Retained<NSView>)>,
    ) {
        const SWITCH_W: f64 = 110.0;
        self.label(label, self.y, 25.0);
        let w = if limit.is_some() {
            SWITCH_W
        } else {
            self.width - CX - X
        };
        check.setFrame(rect(CX, self.y + 2.0, w, 20.0));
        self.view.addSubview(&check);
        self.controls.push((id, check));
        if let Some((pid, p)) = limit {
            p.setFrame(rect(CX + SWITCH_W + 4.0, self.y, 100.0, 25.0));
            self.view.addSubview(&p);
            self.controls.push((pid, p));
        }
        self.y += 25.0 + 8.0;
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
        let w = ui::text_width(&b).max(120.0);
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

fn trash_access_text() -> String {
    match kipimo_sys::disk::trash_size() {
        Err(kipimo_sys::TrashError::NoAccess) => tr(
            "Kipimo can't read the Trash yet. Allow Full Disk Access so it can measure the Trash and find leftovers.",
        ),
        _ => tr("Kipimo can read the Trash. Full Disk Access is only needed if this changes."),
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
    let (w, h) = (580.0, 514.0);
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
    window.setTitle(&ns(&tr("Kipimo Settings")));
    let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(0.0, 0.0, w, h));
    let (iw, ih) = (w - 20.0, h - 40.0);
    let mut controls: HashMap<u16, Retained<NSView>> = HashMap::new();

    // General.
    let mut g = Form::new(mtm, iw, ih);
    g.section(&tr("Startup"));
    g.plain(
        LOGIN,
        checkbox(
            &tr("Launch Kipimo at login"),
            LOGIN,
            app::login_item_enabled(),
        ),
        300.0,
        20.0,
        None,
    );
    let login_note = g.note("");
    g.section(&tr("Updates"));
    g.plain(
        UPDATES,
        checkbox(
            &tr("Check for updates automatically"),
            UPDATES,
            s.check_updates,
        ),
        330.0,
        20.0,
        None,
    );
    g.note(&tr(
        "Once a day, Kipimo asks GitHub whether there's a newer version.",
    ));
    g.section(&tr("Menu bar"));
    g.plain(
        TINT,
        checkbox(
            &tr("Colour the icon when something needs attention"),
            TINT,
            s.tint_icon,
        ),
        330.0,
        20.0,
        None,
    );
    let refresh_labels = vec![tr("Every second"), tr("Every 2 seconds")];
    g.row(
        &tr("Panel updates:"),
        REFRESH,
        popup(
            &refresh_labels,
            pos(&REFRESH_CHOICES, s.refresh_secs),
            REFRESH,
        ),
        160.0,
        25.0,
    );
    g.note(&tr(
        "Only while the panel is open. When it's closed, Kipimo checks once a minute.",
    ));
    g.section(&tr("Access"));
    let access_note = g.note(&trash_access_text());
    g.button(&tr("Open Full Disk Access…"), FULL_DISK);

    // Alerts.
    let mut a = Form::new(mtm, iw, ih);
    a.section(&tr("Tell me when"));
    let gb = |v: &[u32]| {
        v.iter()
            .map(|g| format!("{g} {}", tr("GB")))
            .collect::<Vec<_>>()
    };
    let pct = BLUETOOTH_PCT_CHOICES
        .iter()
        .map(|p| format!("{p}%"))
        .collect::<Vec<_>>();
    // One grid: a right-aligned label, the switch, and (where there is one) the limit,
    // every limit in the same column.
    a.alert_row(
        &tr("Startup disk:"),
        DISK,
        checkbox(&tr("Below"), DISK, s.alert_disk),
        Some((
            DISK_GB,
            popup(
                &gb(&DISK_GB_CHOICES),
                pos(&DISK_GB_CHOICES, s.disk_gb),
                DISK_GB,
            ),
        )),
    );
    a.alert_row(
        &tr("Trash:"),
        TRASH,
        checkbox(&tr("Larger than"), TRASH, s.alert_trash),
        Some((
            TRASH_GB,
            popup(
                &gb(&TRASH_GB_CHOICES),
                pos(&TRASH_GB_CHOICES, s.trash_gb),
                TRASH_GB,
            ),
        )),
    );
    a.alert_row(
        &tr("Bluetooth battery:"),
        BLUETOOTH,
        checkbox(&tr("Below"), BLUETOOTH, s.alert_bluetooth),
        Some((
            BLUETOOTH_PCT,
            popup(
                &pct,
                pos(&BLUETOOTH_PCT_CHOICES, s.bluetooth_pct),
                BLUETOOTH_PCT,
            ),
        )),
    );
    a.alert_row(
        &tr("External drives:"),
        DRIVES,
        checkbox(
            &tr("When one is connected or ejected"),
            DRIVES,
            s.alert_drives,
        ),
        None,
    );
    a.alert_row(
        &tr("Leftovers:"),
        LEFTOVERS,
        checkbox(
            &tr("When an app moved to the Trash leaves files behind"),
            LEFTOVERS,
            s.alert_leftovers,
        ),
        None,
    );
    a.note(&tr(
        "Each alert comes once, when a value crosses its limit. macOS asks before Kipimo's first notification.",
    ));
    a.y += 6.0;
    a.button(&tr("Send a Test Notification"), TEST_ALERT);

    // Weekly summary.
    let mut wk = Form::new(mtm, iw, ih);
    wk.section(&tr("Weekly summary"));
    wk.plain(
        WEEKLY,
        checkbox(&tr("Send a weekly summary"), WEEKLY, s.weekly),
        300.0,
        20.0,
        None,
    );
    let days = weekdays();
    wk.row(
        &tr("Day:"),
        WEEKDAY,
        popup(&days, (s.weekly_weekday.clamp(1, 7) - 1) as usize, WEEKDAY),
        140.0,
        25.0,
    );
    wk.row(&tr("Time:"), TIME, time_picker(s.weekly_minute), 90.0, 24.0);
    wk.note(&tr(
        "One notification with how much space a cleanup would free and how many app updates are waiting. Clicking it opens Kipimo.",
    ));
    wk.y += 6.0;
    wk.button(&tr("Send One Now"), SUMMARY_NOW);

    for (id, title, form) in [
        ("General", tr("General"), g),
        ("Alerts", tr("Alerts"), a),
        ("Weekly Summary", tr("Weekly Summary"), wk),
    ] {
        let item =
            unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), Some(&ns(id))) };
        item.setLabel(&ns(&title));
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

#[cfg(dev)]
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

/// Call `f` with the Settings window, for screenshot runs.
#[cfg(dev)]
pub fn with_window_for_shots(f: impl FnOnce(&NSWindow)) {
    STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            f(&st.window)
        }
    })
}

/// Close the window if it is open.
#[cfg(dev)]
pub fn close() {
    if let Some(w) = STATE.with(|s| s.borrow().as_ref().map(|s| s.window.clone())) {
        w.close();
    }
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
            st.login_note.setStringValue(&ns(&if login {
                tr("Kipimo starts with your Mac, quietly in the menu bar.")
            } else {
                tr("Off: start Kipimo yourself after you log in.")
            }));
            st.access_note.setStringValue(&ns(&trash_access_text()));
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
        UPDATES => {
            let on = is_on(id);
            settings::update(|s| s.check_updates = on);
            crate::update_ui::setting_changed();
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
            title: tr("Kipimo notifications work"),
            body: tr(
                "Alerts for disk space, the Trash, drives and Bluetooth batteries will look like this.",
            ),
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
