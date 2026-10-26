//! `open kipimo://selftest`: a quick end-to-end check of the panel's
//! open/close behaviour, printed to stderr (run the binary from a terminal to
//! see it). Posts synthetic events to Kipimo itself only.

use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;

use crate::util::{self, ns};
use crate::{panel, ui};

fn report(step: &str, ok: bool) {
    eprintln!("[selftest] {} {step}", if ok { "PASS" } else { "FAIL" });
}

pub fn run() {
    panel::close();
    panel::open();
    report("panel opens", panel::is_open());
    util::main_after(1.5, || {
        report("panel samples while open", panel::ticks() > 0);
        // Escape, as if typed while the panel is key.
        let mtm = util::mtm();
        let win = panel::window_number().unwrap_or(0);
        let esc = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            win,
            None,
            &ns("\u{1b}"),
            &ns("\u{1b}"),
            false,
            53,
        );
        if let Some(e) = esc {
            ui::app(mtm).postEvent_atStart(&e, true);
        }
        util::main_after(0.5, || {
            report("Escape closes the panel", !panel::is_open());
            panel::toggle();
            report("toggle reopens after a pause", panel::is_open());
            panel::toggle();
            report("toggle closes", !panel::is_open());
            panel::toggle();
            report(
                "a click right after closing doesn't reopen",
                !panel::is_open(),
            );
            eprintln!("[selftest] done");
        });
    });
}
