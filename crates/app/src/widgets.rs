//! Building blocks for the tools window and review windows: buttons and popups
//! whose actions are Rust closures, labels in the system's semantic colours,
//! spinners, banners. Stock AppKit controls throughout (Photon's rule: never
//! custom-draw what AppKit provides).
//!
//! AppKit holds control targets weakly, so every closure target is returned to
//! the caller, who keeps it alive (usually in a [`Targets`] list on the page).

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSButton, NSColor, NSControl, NSControlSize,
    NSImageView, NSLineBreakMode, NSPopUpButton, NSProgressIndicator, NSProgressIndicatorStyle,
    NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSObject, NSRect};

use crate::ui::{self, Weight, rect};
use crate::util::ns;

pub struct TargetIvars {
    action: RefCell<Option<Box<dyn Fn()>>>,
}

define_class!(
    // SAFETY: NSObject subclass whose only state is the Rust closure it calls.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoClosureTarget"]
    #[ivars = TargetIvars]
    pub struct Target;

    impl Target {
        #[unsafe(method(fire:))]
        fn fire(&self, _sender: Option<&AnyObject>) {
            // Stay alive while the closure runs, even if it drops the page that keeps us.
            let _keep = self.retain();
            // Take the closure out while it runs, so it may rebuild the view that owns it.
            let f = self.ivars().action.borrow_mut().take();
            if let Some(f) = f {
                f();
                let mut slot = self.ivars().action.borrow_mut();
                if slot.is_none() {
                    *slot = Some(f);
                }
            }
        }
    }
);

impl Target {
    pub fn new(mtm: MainThreadMarker, f: impl Fn() + 'static) -> Retained<Target> {
        let this = Target::alloc(mtm).set_ivars(TargetIvars {
            action: RefCell::new(Some(Box::new(f))),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Point a control at this target.
    pub fn attach(&self, c: &NSControl) {
        unsafe {
            c.setTarget(Some(self));
            c.setAction(Some(sel!(fire:)));
        }
    }
}

/// Keeps closure targets alive for as long as the page that owns them.
#[derive(Default)]
pub struct Targets(Vec<Retained<Target>>);

impl Targets {
    pub fn keep(&mut self, t: Retained<Target>) {
        self.0.push(t);
    }

    /// Wire `c` to `f` and keep the target.
    pub fn on(&mut self, c: &NSControl, f: impl Fn() + 'static) {
        let t = Target::new(MainThreadMarker::from(c), f);
        t.attach(c);
        self.keep(t);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tint {
    #[default]
    Normal,
    Secondary,
    Tertiary,
    Green,
    Orange,
    Red,
    Blue,
}

impl Tint {
    pub fn color(self) -> Retained<NSColor> {
        match self {
            Tint::Normal => NSColor::labelColor(),
            Tint::Secondary => NSColor::secondaryLabelColor(),
            Tint::Tertiary => NSColor::tertiaryLabelColor(),
            Tint::Green => NSColor::systemGreenColor(),
            Tint::Orange => NSColor::systemOrangeColor(),
            Tint::Red => NSColor::systemRedColor(),
            Tint::Blue => NSColor::systemBlueColor(),
        }
    }
}

/// A one-line label in a semantic colour.
pub fn text(
    mtm: MainThreadMarker,
    s: &str,
    size: f64,
    weight: Weight,
    tint: Tint,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(s), mtm);
    l.setFont(Some(&ui::font(size, weight)));
    l.setTextColor(Some(&tint.color()));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l
}

/// A wrapping label.
pub fn wrap(mtm: MainThreadMarker, s: &str, size: f64, tint: Tint) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&ns(s), mtm);
    l.setFont(Some(&ui::font(size, Weight::Regular)));
    l.setTextColor(Some(&tint.color()));
    l
}

pub fn set_text(l: &NSTextField, s: &str) {
    l.setStringValue(&ns(s));
}

/// A push button running `f`.
pub fn button(
    mtm: MainThreadMarker,
    title: &str,
    targets: &mut Targets,
    f: impl Fn() + 'static,
) -> Retained<NSButton> {
    // SAFETY: no target or action yet; `Targets::on` sets both.
    let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), None, None, mtm) };
    b.setBezelStyle(NSBezelStyle::Push);
    targets.on(&b, f);
    b
}

/// A small push button (in rows of controls and banners).
pub fn small_button(
    mtm: MainThreadMarker,
    title: &str,
    targets: &mut Targets,
    f: impl Fn() + 'static,
) -> Retained<NSButton> {
    let b = button(mtm, title, targets, f);
    b.setControlSize(NSControlSize::Small);
    b.setFont(Some(&ui::font(11.0, Weight::Regular)));
    b
}

/// A popup button running `f` with the selected index.
pub fn popup(
    mtm: MainThreadMarker,
    items: &[String],
    selected: usize,
    targets: &mut Targets,
    f: impl Fn(usize) + 'static,
) -> Retained<NSPopUpButton> {
    let p = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(0.0, 0.0, 120.0, 25.0),
        false,
    );
    for i in items {
        p.addItemWithTitle(&ns(i));
    }
    p.selectItemAtIndex(selected.min(items.len().saturating_sub(1)) as isize);
    let me = p.clone();
    targets.on(&p, move || f(me.indexOfSelectedItem().max(0) as usize));
    p
}

/// A small spinning progress indicator, hidden while stopped.
pub fn spinner(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSProgressIndicator> {
    let s = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), frame);
    s.setStyle(NSProgressIndicatorStyle::Spinning);
    s.setControlSize(NSControlSize::Small);
    s.setDisplayedWhenStopped(false);
    s
}

pub fn spin(s: &NSProgressIndicator, on: bool) {
    if on {
        unsafe { s.startAnimation(None) };
    } else {
        unsafe { s.stopAnimation(None) };
    }
}

/// A determinate bar (repair check, cleaning).
pub fn bar(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSProgressIndicator> {
    let s = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), frame);
    s.setStyle(NSProgressIndicatorStyle::Bar);
    s.setIndeterminate(false);
    s.setMinValue(0.0);
    s.setMaxValue(1.0);
    s.setControlSize(NSControlSize::Small);
    s
}

pub fn symbol_view(
    mtm: MainThreadMarker,
    frame: NSRect,
    symbol: &str,
    points: f64,
    tint: Tint,
) -> Retained<NSImageView> {
    let v = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
    v.setImage(ui::symbol(symbol, points).as_deref());
    v.setContentTintColor(Some(&tint.color()));
    v
}

/// Autoresizing shorthands for views in a flipped container.
pub fn stretch(v: &NSView) {
    v.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
}

pub fn stretch_width(v: &NSView) {
    v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
}

/// Stays at the right edge.
pub fn pin_right(v: &NSView) {
    v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
}

/// Stays at the bottom-right corner of a flipped container.
pub fn pin_bottom_right(v: &NSView) {
    v.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
}

/// Right-aligned label.
pub fn right(l: &NSTextField) {
    l.setAlignment(NSTextAlignment::Right);
}

/// A banner's button: title, where to keep its target, and what it does.
pub type BannerButton<'a> = (&'a str, &'a mut Targets, Box<dyn Fn()>);

/// A rounded strip for hints like "needs Full Disk Access": symbol, text and an optional button.
pub struct Banner {
    pub view: Retained<NSView>,
    pub text: Retained<NSTextField>,
    pub _button: Option<Retained<NSButton>>,
}

impl Banner {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        symbol: &str,
        tint: Tint,
        message: &str,
        button: Option<BannerButton<'_>>,
    ) -> Banner {
        let (w, h) = (frame.size.width, frame.size.height);
        let view = ui::flipped(mtm, frame);
        let bg = ui::fill(
            mtm,
            rect(0.0, 0.0, w, h),
            &tint.color().colorWithAlphaComponent(0.12),
            8.0,
        );
        stretch(&bg);
        view.addSubview(&bg);
        view.addSubview(&symbol_view(
            mtm,
            rect(10.0, (h - 18.0) / 2.0, 18.0, 18.0),
            symbol,
            13.0,
            tint,
        ));
        // The button comes first, so the text keeps clear of it at whatever width its
        // (translated) title needs.
        let button = button.map(|(title, targets, f)| {
            let b = small_button(mtm, title, targets, f);
            let bwid = ui::text_width(&b) + 24.0;
            b.setFrame(rect(w - bwid - 8.0, (h - 24.0) / 2.0, bwid, 24.0));
            pin_right(&b);
            b
        });
        let reserve = button.as_ref().map_or(0.0, |b| b.frame().size.width + 16.0);
        let text = wrap(mtm, message, 12.0, Tint::Normal);
        text.setFrame(rect(36.0, (h - 32.0) / 2.0, w - 46.0 - reserve, 32.0));
        stretch_width(&text);
        view.addSubview(&text);
        if let Some(b) = &button {
            view.addSubview(b);
        }
        Banner {
            view,
            text,
            _button: button,
        }
    }
}
