//! Development builds only. Launched with `KIPIMO_SHOTS=<folder>`, Kipimo draws its panel,
//! each Settings tab, the update window (for a made-up release) and each tools page into PNGs
//! in that folder, then quits. Drawing needs no screen recording permission, so a build in
//! `dist/` can run next to the installed Kipimo, in any language:
//!
//! ```text
//! open -n dist/Kipimo.app --env KIPIMO_SHOTS=/tmp/kipimo-shots --env TMPDIR=/tmp/kipimo-dev \
//!   --args -AppleLanguages '(de)'
//! ```
//!
//! The separate `TMPDIR` gives it its own single-instance lock. The tools pages scan this
//! Mac read-only, as they do when opened by hand.

use objc2_app_kit::{NSBitmapImageFileType, NSWindow};

use crate::tools_window::Tab;
use crate::util;

/// The folder the PNGs go to, when this run is a screenshot run.
pub fn folder() -> Option<String> {
    std::env::var("KIPIMO_SHOTS").ok().filter(|d| !d.is_empty())
}

/// Draw `window`, title bar included, into `<folder>/<name>.png`. Vibrancy draws flat, but
/// text and layout are exact.
pub fn draw(window: &NSWindow, name: &str) {
    let Some(dir) = folder() else { return };
    // SAFETY: the content view's superview is the window's frame view, owned by the window.
    let Some(view) = window.contentView().and_then(|v| unsafe { v.superview() }) else {
        return;
    };
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else {
        return;
    };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let props = objc2_foundation::NSDictionary::new();
    // SAFETY: PNG with no properties is a valid request.
    let Some(png) =
        (unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) })
    else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/{name}.png"), png.to_vec());
    util::log(format!("shots: drew {name}"));
}

type Step = (f64, Box<dyn FnOnce() + Send>);

/// Run each step after its delay, one after another.
fn run_steps(mut steps: Vec<Step>) {
    if steps.is_empty() {
        return;
    }
    let (delay, f) = steps.remove(0);
    util::main_after(delay, move || {
        f();
        run_steps(steps);
    });
}

/// The menu-bar process: the panel and Settings, then the tools pages in the tools process.
pub fn run() {
    let mut steps: Vec<Step> = vec![
        (0.5, Box::new(crate::panel::open)),
        (
            3.0,
            Box::new(|| crate::panel::with_window_for_shots(|w| draw(w, "panel"))),
        ),
        (0.1, Box::new(crate::panel::close)),
        (0.2, Box::new(crate::settings_window::show)),
    ];
    for (i, name) in ["settings-general", "settings-alerts", "settings-weekly"]
        .into_iter()
        .enumerate()
    {
        steps.push((
            0.4,
            Box::new(move || crate::settings_window::select_tab(i as isize)),
        ));
        steps.push((
            0.6,
            Box::new(move || crate::settings_window::with_window_for_shots(|w| draw(w, name))),
        ));
    }
    steps.push((0.1, Box::new(crate::settings_window::close)));
    steps.push((0.2, Box::new(crate::update_ui::show_sample)));
    steps.push((
        0.6,
        Box::new(|| crate::update_ui::with_window_for_shots(|w| draw(w, "update"))),
    ));
    steps.push((0.1, Box::new(crate::update_ui::close)));
    steps.push((0.2, Box::new(|| crate::tools_process::send("shots"))));
    // The tools process quits when its pages are drawn; this one follows.
    steps.push((40.0, Box::new(|| std::process::exit(0))));
    run_steps(steps);
}

/// The tools process: each page, given time to scan, then quit both processes.
pub fn run_tools() {
    let mut steps: Vec<Step> = Vec::new();
    for tab in Tab::ALL {
        steps.push((
            0.3,
            Box::new(move || crate::tools_window::show_path(tab.key())),
        ));
        steps.push((
            6.0,
            Box::new(move || {
                crate::tools_window::with_window_for_shots(|w| {
                    draw(w, &format!("tools-{}", tab.key()))
                })
            }),
        ));
    }
    steps.push((
        0.3,
        Box::new(|| {
            util::log("shots: done");
            std::process::exit(0);
        }),
    ));
    run_steps(steps);
}
