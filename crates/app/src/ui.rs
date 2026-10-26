//! The single Objective-C controller AppKit calls back into (app delegate,
//! control target, timers, notification delegate), plus small view helpers in
//! the Photon style: SF Symbols, the system font, white chrome on dark HUD material.

use std::cell::OnceCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationDelegate, NSBox, NSBoxType, NSButton, NSColor, NSFont, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSImage, NSImageSymbolConfiguration,
    NSLineBreakMode, NSTextField, NSTitlePosition, NSView, NSVisualEffectBlendingMode,
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
            crate::panel::open();
        }

        #[unsafe(method(thermalStateChanged:))]
        fn thermal_changed(&self, _n: Option<&NSNotification>) {
            // Posted on an arbitrary thread.
            crate::util::main_async(crate::background::thermal_changed);
        }

        #[unsafe(method(volumesChanged:))]
        fn volumes_changed(&self, n: Option<&NSNotification>) {
            let mounted = n.map(|n| n.name().to_string().contains("DidMount")).unwrap_or(false);
            let path = n
                .and_then(|n| n.userInfo())
                .and_then(|u| u.objectForKey(&*ns("NSDevicePath")))
                .and_then(|p| p.downcast::<objc2_foundation::NSString>().ok())
                .map(|p| p.to_string());
            crate::background::volume_changed(mounted, path);
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

/// A plain filled rounded rectangle (cards, bars). NSBox draws it, so no layer code.
pub fn fill_box(
    mtm: MainThreadMarker,
    frame: NSRect,
    fill: &NSColor,
    radius: f64,
) -> Retained<NSBox> {
    let b = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    b.setBoxType(NSBoxType::Custom);
    b.setTitlePosition(NSTitlePosition::NoTitle);
    b.setBorderWidth(0.0);
    b.setCornerRadius(radius);
    b.setFillColor(fill);
    b.setContentViewMargins(NSSize::new(0.0, 0.0));
    b
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
