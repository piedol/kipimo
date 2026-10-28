//! Kipimo's side of the updater (the shared part is in `update/`): the menu item in the gear
//! and status menus, the daily check and its setting, the notification, the update window,
//! and closing the tools process before the install replaces the app.

use std::cell::{Cell, RefCell};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAlert, NSBorderType, NSButton, NSFont, NSFontAttributeName, NSMenuItem,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSScrollView, NSTextAlignment,
    NSTextTab, NSTextView, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSRange, NSSize, NSString, NSURL};

use crate::actions::Action;
use crate::commands::Command;
use crate::i18n::{tr, trf};
use crate::notify::{self, Alert, Kind};
use crate::ui::{self, rect};
use crate::update::notes::{Line, Style};
use crate::update::{self, Check, Release, Step};
use crate::util::{self, ns};
use crate::{settings, tools_process, trash_guard};

pub static CONFIG: update::Config = update::Config {
    repo: "piedol/kipimo",
    asset: "Kipimo-macOS-arm64.zip",
    name: "Kipimo",
    bundle_id: "top.piedol.kipimo",
    team_id: "D9JNQPYFBM",
    env_prefix: "KIPIMO",
    fallback_version: env!("CARGO_PKG_VERSION"),
    log: |m| util::log(m),
};

/// What the menu item offers.
enum State {
    Idle,
    /// A check chosen from the menu is running.
    Checking,
    Available(Release),
    Installing(Release, Step),
}

// The update window's buttons (`Command::UpdateButton`).
const INSTALL: u8 = 1;
const LATER: u8 = 2;
const OK: u8 = 3;

/// The notification's id and its `Action::Custom` name.
const NOTIFICATION: &str = "update";

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State::Idle) };
    /// The update window or a message, while one is open.
    static ALERT: RefCell<Option<Retained<NSAlert>>> = const { RefCell::new(None) };
    /// The version last announced in a notification, so each is announced once per run.
    static NOTIFIED: RefCell<Option<update::Version>> = const { RefCell::new(None) };
    /// KIPIMO_UPDATE_TEST=install: unattended, so results go to the log only.
    static TESTING: Cell<bool> = const { Cell::new(false) };
}

fn current() -> update::Version {
    update::current_version(&CONFIG)
}

fn set_state(s: State) {
    STATE.with(|st| *st.borrow_mut() = s);
}

fn available() -> Option<Release> {
    STATE.with(|st| match &*st.borrow() {
        State::Available(r) | State::Installing(r, _) => Some(r.clone()),
        _ => None,
    })
}

/// `KIPIMO_UPDATE_TEST=install`, for testing the whole install: open the tools window (so the
/// install has to close it), check now and install anything newer without asking. `true` for
/// such a run, which starts nothing else: no watchers, alerts or notifications.
pub fn install_test() -> bool {
    if std::env::var("KIPIMO_UPDATE_TEST").as_deref() != Ok("install") {
        return false;
    }
    update::launched(&CONFIG);
    TESTING.with(|t| t.set(true));
    util::log("update test: opening the tools window, checking now, installing without asking");
    crate::tools_window::show(crate::tools_window::Tab::Clean);
    update::check(&CONFIG, |result| match result {
        Ok(Check::Available(r)) => {
            set_state(State::Available(r.clone()));
            install(r);
        }
        Ok(Check::UpToDate) => util::log("update test: nothing newer"),
        Err(e) => util::log(format!("update test: {e}")),
    });
    true
}

/// Called from `app::launched`.
pub fn launched() {
    if let Some(v) = update::launched(&CONFIG) {
        util::log(format!("updated to Kipimo {v}"));
    }
    crate::actions::register_custom(NOTIFICATION, |_| notification_clicked());
    start_schedule();
}

fn start_schedule() {
    update::schedule::start(|| settings::get().check_updates, last_check, auto_check);
}

/// The Settings checkbox changed.
pub fn setting_changed() {
    start_schedule();
}

fn last_check() -> Option<SystemTime> {
    let secs = settings::get().last_update_check;
    (secs > 0).then(|| UNIX_EPOCH + Duration::from_secs(secs))
}

fn auto_check() {
    if STATE.with(|s| matches!(&*s.borrow(), State::Checking | State::Installing(..))) {
        return;
    }
    update::check(&CONFIG, |result| checked(result, false));
}

/// Check for Updates… in a menu.
fn check_now() {
    set_state(State::Checking);
    update::check(&CONFIG, |result| checked(result, true));
}

fn checked(result: Result<Check, update::Error>, manual: bool) {
    if result.is_ok() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        settings::update(|s| s.last_update_check = now);
    }
    if update::installing() {
        return;
    }
    match result {
        Ok(Check::Available(r)) => {
            set_state(State::Available(r.clone()));
            if manual {
                show_update(&r);
            } else {
                announce(&r);
            }
        }
        Ok(Check::UpToDate) => {
            set_state(State::Idle);
            if manual {
                message(
                    &tr("Kipimo is up to date"),
                    &trf(
                        "Kipimo {version} is the newest version.",
                        &[("version", &current().to_string())],
                    ),
                );
            }
        }
        Err(e) => {
            if STATE.with(|s| matches!(&*s.borrow(), State::Checking)) {
                set_state(State::Idle);
            }
            if manual {
                message(&tr("Couldn't check for updates"), &error_text(&e));
            }
        }
    }
}

/// The core's errors are English (they go to the log too); this is what the user reads.
fn error_text(e: &update::Error) -> String {
    use update::Error;
    match e {
        // macOS's own description, already in Kipimo's language.
        Error::Network(why) => trf("Couldn't reach GitHub. {reason}", &[("reason", why)]),
        Error::Http(403 | 429) => {
            tr("GitHub is limiting requests from this network. Try again in an hour.")
        }
        Error::Http(404) => tr("GitHub has no published release to compare with."),
        Error::Http(code) => trf(
            "GitHub answered with HTTP status {code}. Try again later.",
            &[("code", &code.to_string())],
        ),
        Error::Release(_) => tr("GitHub's answer didn't include a release Kipimo can install."),
        Error::Verify(_) => tr("The download failed its checksum, signature or version check."),
        Error::NotWritable(_) => {
            not_writable_text(&NSBundle::mainBundle().bundlePath().to_string())
        }
        Error::NotBundled(_) => {
            tr("Kipimo isn't running from its app bundle, so it can't replace itself.")
        }
        Error::Replace(_) => tr("The new version couldn't be put in place."),
        Error::Relaunch(_) => tr("Kipimo couldn't reopen. Quit Kipimo and open it again."),
    }
}

/// Why Kipimo at `bundle` can't replace itself.
fn not_writable_text(bundle: &str) -> String {
    if bundle.contains("/AppTranslocation/") {
        return tr(
            "macOS is running Kipimo from a read-only copy, so it can't replace itself. Move Kipimo into the Applications folder and open it from there.",
        );
    }
    let folder = std::path::Path::new(bundle)
        .parent()
        .map_or_else(|| bundle.to_string(), |p| p.display().to_string());
    trf(
        "Kipimo can't replace itself because your account can't make changes in {folder}.",
        &[("folder", &folder)],
    )
}

// ── Menu ───────────────────────────────────────────────────────────────────

/// The item for the gear and status menus: Check for Updates…, Update to Kipimo X.Y.Z…, or
/// the install under way. Their menus don't enable items by themselves.
pub fn menu_item(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let (title, subtitle, enabled) = menu_text();
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &ns(&title),
            Some(sel!(command:)),
            &NSString::new(),
        )
    };
    unsafe { item.setTarget(Some(&ui::controller(mtm))) };
    item.setTag(Command::Update.tag());
    if let Some(s) = subtitle {
        item.setSubtitle(Some(&ns(&s)));
    }
    item.setEnabled(enabled);
    item
}

/// (title, subtitle, enabled).
fn menu_text() -> (String, Option<String>, bool) {
    STATE.with(|st| match &*st.borrow() {
        State::Idle => (tr("Check for Updates…"), None, true),
        State::Checking => (tr("Checking for Updates…"), None, false),
        State::Available(r) => (
            trf(
                "Update to Kipimo {version}…",
                &[("version", &r.version.to_string())],
            ),
            Some(trf(
                "You have {version}",
                &[("version", &current().to_string())],
            )),
            true,
        ),
        State::Installing(r, step) => (
            trf(
                "Installing Kipimo {version}…",
                &[("version", &r.version.to_string())],
            ),
            Some(step_text(*step, r.size, tools_process::running())),
            false,
        ),
    })
}

fn step_text(step: Step, size: u64, tools_open: bool) -> String {
    match step {
        Step::Downloading { received, total } => {
            let total = if total > 0 { total } else { size };
            match (received * 100).checked_div(total) {
                Some(percent) => trf(
                    "Downloading, {percent}%",
                    &[("percent", &percent.min(100).to_string())],
                ),
                None => tr("Downloading"),
            }
        }
        Step::Verifying => tr("Checking the download"),
        Step::Waiting if tools_open => tr("Installs when the Kipimo window finishes"),
        Step::Waiting => tr("Installs in a moment"),
        Step::Replacing => tr("Replacing the app"),
    }
}

/// The menu item was chosen.
pub fn menu_chosen() {
    match available() {
        Some(r) if !update::installing() => show_update(&r),
        Some(_) => {}
        None if STATE.with(|s| matches!(&*s.borrow(), State::Idle)) => check_now(),
        None => {}
    }
}

// ── Installing ─────────────────────────────────────────────────────────────

fn install(r: Release) {
    set_state(State::Installing(
        r.clone(),
        Step::Downloading {
            received: 0,
            total: 0,
        },
    ));
    let hooks = update::Hooks {
        is_idle: ready_to_quit,
        progress,
        done: installed,
    };
    update::install(&CONFIG, r, hooks);
}

/// Kipimo can quit for the install: nothing here is on its way to the Trash, and the tools
/// process has quit. It's asked to as soon as it has nothing in progress.
fn ready_to_quit() -> bool {
    tools_process::close_when_idle() && trash_guard::remaining().is_zero()
}

fn progress(step: Step) {
    STATE.with(|st| {
        if let State::Installing(_, s) = &mut *st.borrow_mut() {
            *s = step;
        }
    });
}

fn installed(result: Result<(), update::Error>) {
    let Some(r) = available() else { return };
    let version = r.version.to_string();
    match result {
        Ok(()) => ui::app(util::mtm()).terminate(None),
        Err(_) if TESTING.with(Cell::get) => set_state(State::Available(r)),
        Err(e @ update::Error::NotWritable(_)) => {
            set_state(State::Available(r.clone()));
            if let Some(url) = NSURL::URLWithString(&ns(&r.page)) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
            message(
                &trf(
                    "Download Kipimo {version} from GitHub",
                    &[("version", &version)],
                ),
                &trf(
                    "{reason} The release page is open in your browser.",
                    &[("reason", &error_text(&e))],
                ),
            );
        }
        Err(e @ update::Error::Relaunch(_)) => {
            set_state(State::Idle);
            message(
                &trf("Kipimo {version} is installed", &[("version", &version)]),
                &error_text(&e),
            );
        }
        Err(e) => {
            set_state(State::Available(r));
            message(
                &trf(
                    "Kipimo {version} wasn't installed",
                    &[("version", &version)],
                ),
                &trf(
                    "{reason} Kipimo {version} is still installed.",
                    &[
                        ("reason", &error_text(&e)),
                        ("version", &current().to_string()),
                    ],
                ),
            );
        }
    }
}

// ── Windows ────────────────────────────────────────────────────────────────

/// The new version, the installed one and the release notes, with Install and Relaunch and Later.
fn show_update(r: &Release) {
    let mtm = util::mtm();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&ns(&trf(
        "Kipimo {version} is available",
        &[("version", &r.version.to_string())],
    )));
    alert.setInformativeText(&ns(&trf(
        "You have Kipimo {version}. Installing quits Kipimo and opens the new version. Your settings carry over.",
        &[("version", &current().to_string())],
    )));
    alert.setAccessoryView(Some(&notes_view(mtm, &r.notes)));
    let install = alert.addButtonWithTitle(&ns(&tr("Install and Relaunch")));
    let later = alert.addButtonWithTitle(&ns(&tr("Later")));
    later.setKeyEquivalent(&ns("\u{1b}"));
    present(alert, &[(install, INSTALL), (later, LATER)]);
}

/// A message with an OK button.
fn message(title: &str, text: &str) {
    let alert = NSAlert::new(util::mtm());
    alert.setMessageText(&ns(title));
    alert.setInformativeText(&ns(text));
    let ok = alert.addButtonWithTitle(&ns(&tr("OK")));
    present(alert, &[(ok, OK)]);
}

/// Show an alert without making it modal, so the panel and menus keep working while it's
/// open. Its buttons send `Command::UpdateButton`.
fn present(alert: Retained<NSAlert>, buttons: &[(Retained<NSButton>, u8)]) {
    close_alert(false);
    let mtm = util::mtm();
    let target = ui::controller(mtm);
    for (b, which) in buttons {
        b.setTag(Command::UpdateButton(*which).tag());
        unsafe {
            b.setTarget(Some(&target));
            b.setAction(Some(sel!(command:)));
        }
    }
    alert.layout();
    let window = alert.window();
    window.center();
    // The answer to a check arrives after the menu has closed, without fresh user input, and
    // macOS 14's cooperative activation then refuses `activate`, leaving the window behind
    // other apps' windows. The older call still brings it forward.
    let app = ui::app(mtm);
    app.activate();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    ALERT.with(|a| *a.borrow_mut() = Some(alert));
}

fn close_alert(deactivate: bool) {
    let Some(alert) = ALERT.with(|a| a.borrow_mut().take()) else {
        return;
    };
    let window = alert.window();
    ui::release_window_contents(&window);
    window.orderOut(None);
    // Hand focus back to the app you were in, unless Settings is still open.
    let app = ui::app(util::mtm());
    let others = app.windows().iter().any(|w| {
        Retained::as_ptr(&w) != Retained::as_ptr(&window)
            && w.isVisible()
            && w.styleMask().contains(NSWindowStyleMask::Titled)
    });
    if deactivate && !others {
        app.deactivate();
    }
    drop(alert);
    util::release_memory_soon();
}

/// A button in the update window or a message.
pub fn button(which: u8) {
    close_alert(true);
    if which == INSTALL
        && let Some(r) = available()
        && !update::installing()
    {
        install(r);
    }
}

/// The release notes in a scrolling text view.
fn notes_view(mtm: MainThreadMarker, markdown: &str) -> Retained<NSScrollView> {
    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setFrame(rect(0.0, 0.0, 440.0, 220.0));
    scroll.setBorderType(NSBorderType::BezelBorder);
    let Some(view) = scroll
        .documentView()
        .and_then(|v| v.downcast::<NSTextView>().ok())
    else {
        return scroll;
    };
    view.setEditable(false);
    view.setTextContainerInset(NSSize::new(6.0, 8.0));
    let lines = update::notes::parse(markdown);
    let (text, styles) = if lines.is_empty() {
        (tr("No release notes."), Vec::new())
    } else {
        layout(&lines)
    };
    view.setString(&ns(&text));
    view.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    let Some(storage) = (unsafe { view.textStorage() }) else {
        return scroll;
    };
    let bullet = NSMutableParagraphStyle::new();
    let tab = unsafe {
        NSTextTab::initWithTextAlignment_location_options(
            NSTextTab::alloc(),
            NSTextAlignment::Left,
            14.0,
            &NSDictionary::new(),
        )
    };
    bullet.setTabStops(Some(&NSArray::from_retained_slice(&[tab])));
    bullet.setHeadIndent(14.0);
    bullet.setParagraphSpacing(3.0);
    let fonts = [
        NSFont::boldSystemFontOfSize(13.0),
        NSFont::boldSystemFontOfSize(12.0),
        NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0),
        NSFont::systemFontOfSize(5.0),
    ];
    for (range, look) in styles {
        let (key, value): (_, &AnyObject) = match look {
            Look::Heading => (unsafe { NSFontAttributeName }, &fonts[0]),
            Look::Bold => (unsafe { NSFontAttributeName }, &fonts[1]),
            Look::Code => (unsafe { NSFontAttributeName }, &fonts[2]),
            Look::Gap => (unsafe { NSFontAttributeName }, &fonts[3]),
            Look::Bullet => (unsafe { NSParagraphStyleAttributeName }, &bullet),
        };
        // SAFETY: fonts and a paragraph style are what these keys take; ranges lie within the text.
        unsafe { storage.addAttribute_value_range(key, value, range) };
    }
    scroll
}

#[derive(Clone, Copy)]
enum Look {
    Heading,
    Bold,
    Code,
    Gap,
    Bullet,
}

/// The notes as one string, with the ranges (in UTF-16 units) to style.
fn layout(lines: &[Line]) -> (String, Vec<(NSRange, Look)>) {
    let mut text = String::new();
    let mut styles = Vec::new();
    let len = |s: &str| s.encode_utf16().count();
    let mut after_gap = false;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            // A blank line is the newline that ends an empty paragraph: make it short.
            if after_gap {
                styles.push((NSRange::new(len(&text), 1), Look::Gap));
            }
            text.push('\n');
        }
        after_gap = matches!(line, Line::Gap);
        let start = len(&text);
        let spans = match line {
            Line::Gap => continue,
            Line::Bullet(s) => {
                text.push_str("•\t");
                s
            }
            Line::Heading(s) | Line::Text(s) => s,
        };
        for span in spans {
            let at = len(&text);
            text.push_str(&span.text);
            let look = match span.style {
                Style::Plain => continue,
                Style::Bold => Look::Bold,
                Style::Code => Look::Code,
            };
            styles.push((NSRange::new(at, len(&span.text)), look));
        }
        let whole = NSRange::new(start, len(&text) - start);
        match line {
            Line::Heading(_) => styles.push((whole, Look::Heading)),
            Line::Bullet(_) => styles.push((whole, Look::Bullet)),
            _ => {}
        }
    }
    (text, styles)
}

// ── Notification ───────────────────────────────────────────────────────────

/// Announce an update found by the daily check. macOS asks for permission the first time
/// Kipimo posts a notification, if it hasn't asked before.
fn announce(r: &Release) {
    let seen = NOTIFIED.with(|n| n.borrow_mut().replace(r.version.clone()));
    if seen.as_ref() == Some(&r.version) {
        return;
    }
    notify::post(Alert {
        id: NOTIFICATION.into(),
        title: trf(
            "Kipimo {version} is available",
            &[("version", &r.version.to_string())],
        ),
        body: trf(
            "You have {version}. Click to see what's new and install it.",
            &[("version", &current().to_string())],
        ),
        kind: Kind::Update,
        action: Some(Action::Custom(NOTIFICATION.into())),
    });
}

/// A click on the notification or its Update… button. After a relaunch the release isn't
/// known any more, so this checks again, like the menu item.
fn notification_clicked() {
    crate::panel::close();
    menu_chosen();
}

/// Development builds: the update window for a made-up release, for screenshot runs.
#[cfg(dev)]
pub fn show_sample() {
    let Some(version) = update::Version::parse("0.2.2") else {
        return;
    };
    show_update(&Release {
        version,
        notes: "Kipimo 0.2.2 is an example release, shown in screenshots.\n\n## Changes\n\n- **Check for Updates…** in the gear menu looks for a new version.\n- Kipimo checks once a day. Turn this off in Settings › General.\n- `Kipimo-macOS-arm64.zip` is for Apple silicon.".into(),
        page: "https://github.com/piedol/kipimo/releases/latest".into(),
        url: String::new(),
        digest: None,
        size: 0,
    });
}

/// Call `f` with the update window, for screenshot runs. The alert's vibrant background draws
/// white offscreen, so it's drawn in the light appearance, where its text shows.
#[cfg(dev)]
pub fn with_window_for_shots(f: impl FnOnce(&objc2_app_kit::NSWindow)) {
    ALERT.with(|a| {
        if let Some(alert) = a.borrow().as_ref() {
            use objc2_app_kit::{NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua};
            let window = alert.window();
            let light = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua });
            window.setAppearance(light.as_deref());
            window.displayIfNeeded();
            f(&window)
        }
    })
}

/// Close the update window, for screenshot runs.
#[cfg(dev)]
pub fn close() {
    close_alert(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_layout_styles_the_right_text() {
        let lines = update::notes::parse("## New\n\n- **Mic choice.** Pick one.\n- `a`é");
        let (text, styles) = layout(&lines);
        assert_eq!(text, "New\n\n•\tMic choice. Pick one.\n•\taé");
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let slice =
            |r: NSRange| String::from_utf16(&utf16[r.location..r.location + r.length]).unwrap();
        let bold = styles
            .iter()
            .find(|(_, l)| matches!(l, Look::Bold))
            .unwrap()
            .0;
        assert_eq!(slice(bold), "Mic choice.");
        let code = styles
            .iter()
            .find(|(_, l)| matches!(l, Look::Code))
            .unwrap()
            .0;
        assert_eq!(slice(code), "a");
        assert_eq!(
            styles
                .iter()
                .filter(|(_, l)| matches!(l, Look::Bullet))
                .count(),
            2
        );
        assert!(
            styles
                .iter()
                .all(|(r, _)| r.location + r.length <= utf16.len())
        );
        let gap = styles
            .iter()
            .find(|(_, l)| matches!(l, Look::Gap))
            .unwrap()
            .0;
        assert_eq!((gap.location, slice(gap)), (4, "\n".to_string()));
    }

    #[test]
    fn progress_reads_as_text() {
        let down =
            |received, total, size| step_text(Step::Downloading { received, total }, size, false);
        assert_eq!(down(5, 10, 0), "Downloading, 50%");
        assert_eq!(down(5, 0, 20), "Downloading, 25%");
        assert_eq!(down(30, 0, 20), "Downloading, 100%");
        assert_eq!(down(0, 0, 0), "Downloading");
        assert_eq!(
            step_text(Step::Waiting, 0, true),
            "Installs when the Kipimo window finishes"
        );
        assert_eq!(step_text(Step::Waiting, 0, false), "Installs in a moment");
    }

    #[test]
    fn errors_read_as_sentences() {
        use update::Error;
        assert_eq!(
            error_text(&Error::Network(
                "The Internet connection appears to be offline.".into()
            )),
            "Couldn't reach GitHub. The Internet connection appears to be offline."
        );
        assert_eq!(
            error_text(&Error::Http(429)),
            "GitHub is limiting requests from this network. Try again in an hour."
        );
        assert_eq!(
            error_text(&Error::Http(502)),
            "GitHub answered with HTTP status 502. Try again later."
        );
        assert_eq!(
            error_text(&Error::Verify(
                "The download doesn't match the checksum.".into()
            )),
            "The download failed its checksum, signature or version check."
        );
    }

    #[test]
    fn read_only_folders_are_named() {
        assert_eq!(
            not_writable_text("/Volumes/Apps/Kipimo.app"),
            "Kipimo can't replace itself because your account can't make changes in /Volumes/Apps."
        );
        assert!(
            not_writable_text("/private/var/folders/x/T/AppTranslocation/ABC/d/Kipimo.app")
                .starts_with("macOS is running Kipimo from a read-only copy")
        );
    }
}
