//! App lifecycle, the menu-bar item and its attention colour, and command routing.

use std::cell::{OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSColor, NSEventMask, NSEventModifierFlags, NSEventType, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use kipimo_sys::MacModel;

use crate::attention::Reason;
use crate::commands::Command;
use crate::i18n::tr;
use crate::recommend::{self, Facts, Recommendation};
use crate::util::{self, ns};
use crate::{background, panel, settings, settings_window, tools_window, ui};

/// A gauge with its needle low: the app icon's motif.
const MENU_SYMBOL: &str = "gauge.with.dots.needle.33percent";

thread_local! {
    static ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    static MODEL: OnceCell<MacModel> = const { OnceCell::new() };
    /// The icon state last drawn, so unchanged states don't redraw the menu bar.
    static SHOWN: RefCell<Option<(bool, String)>> = const { RefCell::new(None) };
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("Kipimo must start on the main thread");
    let app = ui::app(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    ui::set_app_delegate(mtm);
    app.run();
}

/// Called from `applicationDidFinishLaunching:`.
pub fn launched(mtm: MainThreadMarker) {
    install_main_menu(mtm);
    if crate::tools_process::is_child() {
        crate::tools_process::child_launched();
        return;
    }
    build_status_item(mtm);
    crate::single::listen();
    crate::notify::install();
    crate::pages::register();
    crate::insights::register_sources();
    crate::actions::register_custom("leftovers", crate::review_window::show);
    background::start();
    util::main_after(3.0, crate::panel::warm_up);
    util::log(format!(
        "launched on {}; Full Disk Access: {:?}",
        model().name,
        crate::page::full_disk_access()
    ));
}

/// The Mac model, read once.
pub fn model() -> MacModel {
    MODEL.with(|m| m.get_or_init(kipimo_sys::model::mac_model).clone())
}

/// A main menu the user never sees (Kipimo has no menu bar of its own), so
/// ⌘W, ⌘Q, ⌘, and the edit shortcuts work in Kipimo's windows.
fn install_main_menu(mtm: MainThreadMarker) {
    let item = |title: &str, action: objc2::runtime::Sel, key: &str, mods: NSEventModifierFlags| {
        let i = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                Some(action),
                &ns(key),
            )
        };
        i.setKeyEquivalentModifierMask(mods);
        i
    };
    let cmd = NSEventModifierFlags::Command;
    let main = NSMenu::new(mtm);
    let appm = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Kipimo"));
    let settings_item = item("Settings…", sel!(command:), ",", cmd);
    unsafe { settings_item.setTarget(Some(&ui::controller(mtm))) };
    settings_item.setTag(Command::OpenSettings.tag());
    appm.addItem(&settings_item);
    appm.addItem(&item("Quit Kipimo", sel!(terminate:), "q", cmd));
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    edit.addItem(&item("Cut", sel!(cut:), "x", cmd));
    edit.addItem(&item("Copy", sel!(copy:), "c", cmd));
    edit.addItem(&item("Paste", sel!(paste:), "v", cmd));
    edit.addItem(&item("Select All", sel!(selectAll:), "a", cmd));
    let window = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Window"));
    window.addItem(&item("Close", sel!(performClose:), "w", cmd));
    for (title, sub) in [("Kipimo", appm), ("Edit", edit), ("Window", window)] {
        let top = item(
            title,
            sel!(submenuAction:),
            "",
            NSEventModifierFlags::empty(),
        );
        top.setSubmenu(Some(&sub));
        main.addItem(&top);
    }
    ui::app(mtm).setMainMenu(Some(&main));
}

fn build_status_item(mtm: MainThreadMarker) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let target = ui::controller(mtm);
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(statusClicked:)));
        }
        button.sendActionOn(NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown);
    }
    ITEM.with(|i| *i.borrow_mut() = Some(item));
    set_icon(false, "Kipimo");
}

pub fn status_button(mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
    ITEM.with(|i| i.borrow().as_ref().and_then(|i| i.button(mtm)))
}

pub fn set_highlight(on: bool) {
    if let Some(b) = status_button(util::mtm()) {
        b.highlight(on);
    }
}

fn set_icon(attention: bool, tip: &str) {
    let Some(button) = status_button(util::mtm()) else {
        return;
    };
    let img = if attention {
        // Orange is Kipimo's "needs attention", as in Photon's notices.
        ui::tinted_symbol(MENU_SYMBOL, 0.0, &NSColor::systemOrangeColor())
    } else {
        ui::symbol(MENU_SYMBOL, 0.0).inspect(|i| i.setTemplate(true))
    };
    button.setImage(img.as_deref());
    button.setToolTip(Some(&ns(tip)));
}

/// What the recommendation sources can see right now.
pub fn facts() -> Facts {
    let w = background::watch();
    let s = settings::get();
    Facts {
        boot_free: w.boot_free,
        disk_low: w.disk_low(),
        trash_bytes: background::trash_bytes(),
        trash_over: w.trash_over(),
        trash_unreadable: background::trash_denied(),
        limits: Some(crate::attention::Limits::from(&s)),
    }
}

pub fn recommendation() -> Option<Recommendation> {
    recommend::current(&facts())
}

pub fn disk_low() -> bool {
    facts().disk_low
}

/// Everything that needs attention, pending recommendation included.
pub fn reasons() -> Vec<Reason> {
    let rec = recommendation().filter(|r| r.nag);
    background::watch().reasons(rec.as_ref().map(|r| r.title.as_str()))
}

/// Recolour the menu-bar icon after any state change.
pub fn refresh_attention() {
    let reasons = reasons();
    let attention = !reasons.is_empty() && settings::get().tint_icon;
    let tip = if reasons.is_empty() {
        tr("Kipimo · your Mac is in good shape")
    } else {
        format!(
            "Kipimo · {}",
            reasons
                .iter()
                .map(|r| r.text())
                .collect::<Vec<_>>()
                .join(" · ")
        )
    };
    let changed = SHOWN.with(|s| {
        let mut s = s.borrow_mut();
        let new = Some((attention, tip.clone()));
        let changed = *s != new;
        *s = new;
        changed
    });
    if changed {
        set_icon(attention, &tip);
    }
}

/// Left click toggles the panel; right click (or ⌃-click) shows a short menu.
pub fn status_clicked() {
    let mtm = util::mtm();
    let right = ui::app(mtm)
        .currentEvent()
        .map(|e| {
            e.r#type() == NSEventType::RightMouseDown
                || e.modifierFlags().contains(NSEventModifierFlags::Control)
        })
        .unwrap_or(false);
    if !right {
        panel::toggle();
        return;
    }
    panel::close();
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let item = |title: &str, cmd: Command| {
        let i = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                Some(sel!(command:)),
                &NSString::new(),
            )
        };
        unsafe { i.setTarget(Some(&ui::controller(mtm))) };
        i.setTag(cmd.tag());
        i
    };
    menu.addItem(&item(
        &tr("Open Kipimo"),
        Command::OpenTools(tools_window::Tab::Clean),
    ));
    menu.addItem(&item(&tr("Settings…"), Command::OpenSettings));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(&tr("Quit Kipimo"), Command::Quit));
    ITEM.with(|i| {
        if let Some(i) = i.borrow().as_ref() {
            #[allow(deprecated)]
            i.popUpStatusItemMenu(&menu);
        }
    });
}

pub fn login_item_enabled() -> bool {
    unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled }
}

/// Register or unregister Kipimo as a login item.
pub fn set_login_item(on: bool) -> Result<(), String> {
    let service = unsafe { SMAppService::mainAppService() };
    let r = if on {
        unsafe { service.registerAndReturnError() }
    } else {
        unsafe { service.unregisterAndReturnError() }
    };
    r.map_err(|e| e.localizedDescription().to_string())
}

pub fn toggle_login_item() {
    if let Err(e) = set_login_item(!login_item_enabled()) {
        util::log(format!("launch at login: {e}"));
        settings_window::show_error(&tr("Couldn’t change Launch at Login"), &e);
    }
    settings_window::refresh();
}

/// `kipimo://panel`, `kipimo://close-panel`, `kipimo://settings[/<tab index>]`, `kipimo://tools/<tab>`,
/// `kipimo://summary` (send the weekly summary now).
/// Release builds answer only `panel`, `settings` and `tools[/<tab>]`: any web page can open
/// these URLs, so they may show Kipimo's windows but never start anything or pass a path.
/// The rest (`demo`, `selftest`, `summary`, `close-*`, folder paths) exist only in
/// development builds (`cfg(dev)`, see build.rs).
fn allowed(host: &str, path: &str) -> bool {
    allowed_in(host, path, cfg!(dev))
}

fn allowed_in(host: &str, path: &str, dev: bool) -> bool {
    match host {
        "panel" | "settings" if path.is_empty() => true,
        "tools" if path.is_empty() || tools_window::Tab::from_key(path).is_some() => true,
        _ => dev,
    }
}

pub fn open_url(host: &str, path: &str) {
    if !allowed(host, path) {
        util::log(format!("ignored URL kipimo://{host}/{path}"));
        return;
    }
    // LaunchServices may hand a URL to the tools process (same bundle): handle the
    // tools commands there and leave the rest to the menu-bar app.
    if crate::tools_process::is_child() {
        match host {
            "tools" => tools_window::show_path(if path.is_empty() { "clean" } else { path }),
            #[cfg(dev)]
            "demo" => crate::pages::demo(path),
            #[cfg(dev)]
            "close-windows" => tools_window::close_here(),
            _ => {}
        }
        return;
    }
    match host {
        "panel" => panel::open(),
        "settings" => {
            settings_window::show();
            #[cfg(dev)]
            if let Ok(i) = path.parse::<isize>() {
                settings_window::select_tab(i);
            }
        }
        // Development builds also take `kipimo://tools/space/Applications` (scans /Applications).
        "tools" => tools_window::show_path(if path.is_empty() { "clean" } else { path }),
        #[cfg(dev)]
        "close-panel" => panel::close(),
        #[cfg(dev)]
        "close-windows" => {
            panel::close();
            settings_window::close();
            tools_window::close();
            crate::review_window::close();
        }
        #[cfg(dev)]
        "summary" => crate::weekly::send(),
        #[cfg(dev)]
        "selftest" => crate::selftest::run(),
        #[cfg(dev)]
        "demo" => crate::tools_process::send(&format!("demo {path}")),
        _ => util::log(format!("unknown URL kipimo://{host}/{path}")),
    }
}

pub fn handle(cmd: Command) {
    let mtm = util::mtm();
    match cmd {
        Command::TogglePanel => panel::toggle(),
        Command::OpenTools(t) => crate::actions::run(crate::actions::Action::OpenTools(t)),
        Command::OpenSettings => crate::actions::run(crate::actions::Action::OpenSettings),
        Command::OpenTrash => crate::actions::run(crate::actions::Action::OpenTrash),
        Command::Recommendation => panel::recommendation_clicked(),
        Command::SnoozeRecommendation => panel::snooze_recommendation(),
        Command::GearMenu => panel::gear_menu(),
        Command::ToggleLogin => toggle_login_item(),
        Command::Quit => ui::app(mtm).terminate(None),
        Command::Setting(id) => settings_window::changed(id),
    }
}

#[cfg(test)]
mod tests {
    use super::allowed_in;

    #[test]
    fn release_urls_only_open_windows() {
        for (host, path) in [
            ("panel", ""),
            ("settings", ""),
            ("tools", ""),
            ("tools", "clean"),
            ("tools", "space"),
            ("tools", "updates"),
        ] {
            assert!(allowed_in(host, path, false), "{host}/{path}");
        }
        for (host, path) in [
            ("tools", "space/Users/me"),
            ("tools", "space/"),
            ("tools", "nope"),
            ("settings", "2"),
            ("panel", "x"),
            ("summary", ""),
            ("selftest", ""),
            ("demo", "clean/0"),
            ("demo", "review/Applications/Foo.app"),
            ("close-windows", ""),
            ("close-panel", ""),
        ] {
            assert!(!allowed_in(host, path, false), "{host}/{path}");
            assert!(allowed_in(host, path, true), "{host}/{path} in development");
        }
    }
}
