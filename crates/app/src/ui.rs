//! The single Objective-C controller AppKit calls back into (app delegate,
//! control target, timers, notification delegate), plus small view helpers in
//! the Photon style: SF Symbols, the system font, white chrome on dark HUD material.

use std::cell::{OnceCell, RefCell};
use std::sync::OnceLock;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBezierPath, NSButton, NSColor, NSFont,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold, NSImage,
    NSImageSymbolConfiguration, NSLineBreakMode, NSTextField, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSURL,
};
use objc2_user_notifications::{
    UNNotification, UNNotificationPresentationOptions, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use crate::commands::Command;
use crate::util::ns;

/// When quitting was first requested; the Put Back wait is capped from then.
static QUIT_REQUESTED: OnceLock<Instant> = OnceLock::new();

/// Quit once no Trash job is running and Put Back records had time to land.
fn finish_terminating() {
    let wait = crate::trash_guard::quit_wait(*QUIT_REQUESTED.get_or_init(Instant::now));
    if wait.is_zero() {
        app(crate::util::mtm()).replyToApplicationShouldTerminate(true);
    } else {
        crate::util::run_loop_after(wait.as_secs_f64(), finish_terminating);
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoController"]
    pub struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            crate::app::launched(self.mtm());
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, app: &NSApplication) -> NSApplicationTerminateReply {
            // Finder's Put Back records for recent Trash moves are still being
            // written by this process: hide the windows, quit a little later.
            let wait = crate::trash_guard::quit_wait(*QUIT_REQUESTED.get_or_init(Instant::now));
            crate::util::log(format!("quit requested; Put Back hold {:.1} s", wait.as_secs_f64()));
            if wait.is_zero() {
                return NSApplicationTerminateReply::TerminateNow;
            }
            for w in app.windows().iter() {
                w.orderOut(None);
            }
            // Out of the way, so a second Quit (which AppKit would carry out at once) can't
            // reach us while we wait.
            app.hide(None);
            // A run-loop timer, not the main queue: `terminate:` may have been called from a
            // main-queue block, and its wait for this reply can't run that queue again.
            crate::util::run_loop_after(wait.as_secs_f64(), finish_terminating);
            NSApplicationTerminateReply::TerminateLater
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            for url in urls.iter() {
                if let Some(host) = url.host() {
                    let path = url.path().map(|p| p.to_string()).unwrap_or_default();
                    crate::app::open_url(&host.to_string(), path.trim_start_matches('/'));
                }
            }
        }
    }

    unsafe impl UNUserNotificationCenterDelegate for Controller {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // Show banners even while one of Kipimo's windows is in front.
            handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, handler: &block2::DynBlock<dyn Fn()>) {
            crate::notify::responded(response);
            handler.call(());
        }
    }

    impl Controller {
        #[unsafe(method(command:))]
        fn command(&self, sender: Option<&AnyObject>) {
            let tag: isize = match sender {
                Some(s) => unsafe { msg_send![s, tag] },
                None => 0,
            };
            if let Some(cmd) = Command::from_tag(tag) {
                crate::commands::run(cmd);
            }
        }

        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            crate::app::status_clicked();
        }

        #[unsafe(method(panelTick:))]
        fn panel_tick(&self, _timer: Option<&AnyObject>) {
            crate::panel::tick();
        }

        #[unsafe(method(backgroundTick:))]
        fn background_tick(&self, _timer: Option<&AnyObject>) {
            crate::background::tick();
        }

        #[unsafe(method(anotherInstanceLaunched:))]
        fn another_instance(&self, _n: Option<&NSNotification>) {
            crate::util::log("another Kipimo started: showing the panel");
            crate::panel::open();
        }

        #[unsafe(method(thermalStateChanged:))]
        fn thermal_changed(&self, _n: Option<&NSNotification>) {
            // Posted on an arbitrary thread.
            crate::util::main_async(crate::background::thermal_changed);
        }

        #[unsafe(method(didWake:))]
        fn did_wake(&self, _n: Option<&NSNotification>) {
            crate::background::tick();
        }
    }
);

thread_local! {
    static CONTROLLER: OnceCell<Retained<Controller>> = const { OnceCell::new() };
}

/// The single controller instance (target of every control and timer).
pub fn controller(mtm: MainThreadMarker) -> Retained<Controller> {
    CONTROLLER.with(|c| {
        c.get_or_init(|| {
            let this = Controller::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        })
        .clone()
    })
}

pub fn app(mtm: MainThreadMarker) -> Retained<NSApplication> {
    NSApplication::sharedApplication(mtm)
}

pub fn set_app_delegate(mtm: MainThreadMarker) {
    let c = controller(mtm);
    app(mtm).setDelegate(Some(ProtocolObject::from_ref(&*c)));
}

// ── View helpers ───────────────────────────────────────────────────────────

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// An SF Symbol at a point size and weight (0 = default size).
pub fn symbol(name: &str, points: f64) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)?;
    if points > 0.0 {
        let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(points, unsafe {
            NSFontWeightMedium
        });
        return img.imageWithSymbolConfiguration(&cfg).or(Some(img));
    }
    Some(img)
}

/// An SF Symbol drawn in one colour (not a template), for state tints in the menu bar.
pub fn tinted_symbol(name: &str, points: f64, color: &NSColor) -> Option<Retained<NSImage>> {
    let img = symbol(name, points)?;
    let colors = NSArray::from_retained_slice(&[color.retain()]);
    let cfg = NSImageSymbolConfiguration::configurationWithPaletteColors(&colors);
    let img = img.imageWithSymbolConfiguration(&cfg).unwrap_or(img);
    img.setTemplate(false);
    Some(img)
}

/// White at an opacity: the HUD chrome colours (95% text, 60% secondary…).
pub fn white(alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithWhite_alpha(1.0, alpha)
}

/// A one-line label.
pub fn label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    weight: Weight,
    color: &NSColor,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(text), mtm);
    l.setFont(Some(&font(size, weight)));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l
}

#[derive(Clone, Copy)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    /// Medium with tabular digits, for live numbers (the Steady Digits Rule).
    Digits,
    /// Semibold with tabular digits.
    BigDigits,
}

pub fn font(size: f64, w: Weight) -> Retained<NSFont> {
    // SAFETY: AppKit's font-weight constants.
    unsafe {
        match w {
            Weight::Regular => NSFont::systemFontOfSize_weight(size, NSFontWeightRegular),
            Weight::Medium => NSFont::systemFontOfSize_weight(size, NSFontWeightMedium),
            Weight::Semibold => NSFont::systemFontOfSize_weight(size, NSFontWeightSemibold),
            Weight::Digits => {
                NSFont::monospacedDigitSystemFontOfSize_weight(size, NSFontWeightMedium)
            }
            Weight::BigDigits => {
                NSFont::monospacedDigitSystemFontOfSize_weight(size, NSFontWeightSemibold)
            }
        }
    }
}

/// What a [`Fill`] view draws.
pub enum Paint {
    /// A rounded rectangle in one colour.
    Solid,
    /// A horizontal meter: track, then `fraction` of the width in the colour.
    Meter(f64),
    /// One vertical meter per value (0–1), side by side, `gap` apart.
    Columns(Vec<f64>, f64),
}

pub struct FillIvars {
    color: RefCell<Retained<NSColor>>,
    track: Retained<NSColor>,
    radius: f64,
    paint: RefCell<Paint>,
}

define_class!(
    // SAFETY: NSView subclass that draws its own content; no Drop impl beyond the ivars.
    // Cheaper than NSBox (no content view, no KVO), which matters because the
    // panel's allocations decide how much memory stays dirty after it closes.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoFill"]
    #[ivars = FillIvars]
    pub struct Fill;

    impl Fill {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let iv = self.ivars();
            let r = iv.radius;
            let round = |rect: NSRect, color: &NSColor| {
                color.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, r, r).fill();
            };
            let color = iv.color.borrow();
            match &*iv.paint.borrow() {
                Paint::Solid => round(b, &color),
                Paint::Meter(f) => {
                    round(b, &iv.track);
                    let f = *f;
                    let w = (b.size.width * f.clamp(0.0, 1.0)).max(if f > 0.0 { b.size.height } else { 0.0 });
                    if w > 0.0 {
                        round(rect(0.0, 0.0, w, b.size.height), &color);
                    }
                }
                Paint::Columns(values, gap) => {
                    let n = values.len().max(1) as f64;
                    let w = ((b.size.width - gap * (n - 1.0)) / n).max(1.0);
                    for (i, v) in values.iter().enumerate() {
                        let x = i as f64 * (w + gap);
                        round(rect(x, 0.0, w, b.size.height), &iv.track);
                        let h = (b.size.height * v.clamp(0.0, 1.0)).round();
                        if h > 0.0 {
                            round(rect(x, b.size.height - h, w, h), &color);
                        }
                    }
                }
            }
        }
    }
);

impl Fill {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        color: &NSColor,
        radius: f64,
        paint: Paint,
    ) -> Retained<Fill> {
        let this = Fill::alloc(mtm).set_ivars(FillIvars {
            color: RefCell::new(color.retain()),
            track: white(0.12),
            radius,
            paint: RefCell::new(paint),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// A meter with its own track colour (system colours in normal windows).
    pub fn meter(
        mtm: MainThreadMarker,
        frame: NSRect,
        fill: &NSColor,
        track: &NSColor,
        fraction: f64,
    ) -> Retained<Fill> {
        let this = Fill::alloc(mtm).set_ivars(FillIvars {
            color: RefCell::new(fill.retain()),
            track: track.retain(),
            radius: frame.size.height / 2.0,
            paint: RefCell::new(Paint::Meter(fraction)),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set(&self, color: &NSColor, paint: Paint) {
        *self.ivars().color.borrow_mut() = color.retain();
        *self.ivars().paint.borrow_mut() = paint;
        self.setNeedsDisplay(true);
    }
}

/// A plain filled rounded rectangle (card backgrounds).
pub fn fill(mtm: MainThreadMarker, frame: NSRect, color: &NSColor, radius: f64) -> Retained<Fill> {
    Fill::new(mtm, frame, color, radius, Paint::Solid)
}

/// A control's natural width, measured through its cell. (`fittingSize` would
/// spin up the window's Auto Layout engine with a constraint per view.)
pub fn text_width(c: &objc2_app_kit::NSControl) -> f64 {
    c.cell().map(|c| c.cellSize().width).unwrap_or(0.0)
}

/// Borderless symbol button that sends `command:`.
pub fn icon_button(
    mtm: MainThreadMarker,
    symbol_name: &str,
    tooltip: &str,
    cmd: Command,
) -> Retained<NSButton> {
    let image = symbol(symbol_name, 15.0)
        .or_else(|| symbol("questionmark", 15.0))
        .expect("SF Symbols");
    let target = controller(mtm);
    let b = unsafe {
        NSButton::buttonWithImage_target_action(&image, Some(&target), Some(sel!(command:)), mtm)
    };
    b.setBordered(false);
    b.setTag(cmd.tag());
    b.setToolTip(Some(&ns(tooltip)));
    b.setContentTintColor(Some(&white(0.92)));
    b
}

/// Push button that sends `command:`.
pub fn text_button(mtm: MainThreadMarker, title: &str, cmd: Command) -> Retained<NSButton> {
    let target = controller(mtm);
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &ns(title),
            Some(&target),
            Some(sel!(command:)),
            mtm,
        )
    };
    b.setTag(cmd.tag());
    b
}

/// The dark HUD surface every floating Kipimo panel sits on.
pub fn hud_surface(
    mtm: MainThreadMarker,
    frame: NSRect,
    radius: f64,
) -> Retained<NSVisualEffectView> {
    let v = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
    v.setMaterial(NSVisualEffectMaterial::HUDWindow);
    v.setState(NSVisualEffectState::Active);
    v.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
    dark(&v);
    v
}

/// Empty a window that is about to go away, while it is still on screen
/// (made invisible first), and commit that. A hidden window never commits
/// again, so otherwise the render server keeps its last frame's layer
/// surfaces, charged to Kipimo, for as long as AppKit keeps the window object
/// (it remembers the last key window). Call before ordering out or closing.
pub fn release_window_contents(w: &objc2_app_kit::NSWindow) {
    w.setAlphaValue(0.0);
    w.makeFirstResponder(None);
    w.setInitialFirstResponder(None);
    w.setContentView(None);
    w.displayIfNeeded();
    objc2_quartz_core::CATransaction::flush();
}

/// Force dark appearance on a view tree.
pub fn dark(view: &NSView) {
    if let Some(a) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
        view.setAppearance(Some(&a));
    }
}

define_class!(
    // SAFETY: NSView subclass that only flips its coordinate system.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoFlippedView"]
    pub struct Flipped;

    impl Flipped {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

/// A top-down container: (0, 0) is the top-left corner.
pub fn flipped(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let this = Flipped::alloc(mtm).set_ivars(());
    let v: Retained<Flipped> = unsafe { msg_send![super(this), initWithFrame: frame] };
    Retained::into_super(v)
}
