//! The tools window ("Open Kipimo"): five tabs, Clean up, Uninstall, Space,
//! Startup items and Updates. Each tab shows a [`ToolPage`]; until the
//! integration step registers real pages, a placeholder says what's coming.
//!
//! ## Adding a tool page (integration step)
//!
//! ```ignore
//! struct CleanPage { /* scan state, cancel flag, views */ }
//! impl ToolPage for CleanPage {
//!     fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> { … }
//!     fn shown(&mut self) { /* start a scan on a background queue */ }
//!     fn closed(&mut self) { /* set the cancel flag, drop results */ }
//! }
//! tools_window::register(Tab::Clean, || Box::new(CleanPage::new()));
//! ```
//! Register at launch (before the window first opens). Pages are created when
//! the window opens and dropped when it closes, so a closed window holds nothing.
//! Run engine calls off the main thread and wrap them in `catch_unwind`.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSImageView, NSTabView,
    NSTabViewDelegate, NSTabViewItem, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSRect, NSSize};

use crate::i18n::tr;
use crate::ui::{self, Weight, rect};
use crate::util::{self, ns};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Clean,
    Uninstall,
    Space,
    Startup,
    Updates,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Clean,
        Tab::Uninstall,
        Tab::Space,
        Tab::Startup,
        Tab::Updates,
    ];

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    pub fn key(self) -> &'static str {
        match self {
            Tab::Clean => "clean",
            Tab::Uninstall => "uninstall",
            Tab::Space => "space",
            Tab::Startup => "startup",
            Tab::Updates => "updates",
        }
    }

    pub fn from_key(s: &str) -> Option<Tab> {
        Tab::ALL.iter().copied().find(|t| t.key() == s)
    }

    pub fn title(self) -> String {
        match self {
            Tab::Clean => tr("Clean up"),
            Tab::Uninstall => tr("Uninstall"),
            Tab::Space => tr("Space"),
            Tab::Startup => tr("Startup items"),
            Tab::Updates => tr("Updates"),
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Tab::Clean => "sparkles",
            Tab::Uninstall => "trash",
            Tab::Space => "chart.bar.doc.horizontal",
            Tab::Startup => "power",
            Tab::Updates => "arrow.down.circle",
        }
    }

    fn blurb(self) -> String {
        match self {
            Tab::Clean => tr(
                "Finds caches, logs, developer leftovers and other junk, shows how much each category takes, and moves what you tick to the Trash.",
            ),
            Tab::Uninstall => tr(
                "Moves an app and the files it left around your Library to the Trash in one step.",
            ),
            Tab::Space => tr(
                "Sizes your folders quickly so you can see what fills the disk, drill down, reveal items in Finder or move them to the Trash.",
            ),
            Tab::Startup => tr(
                "Lists everything set to start automatically, with who signed it. Unsigned or unknown items stand out.",
            ),
            Tab::Updates => tr(
                "Checks each app's own update feed and the App Store for newer versions, with links to get them.",
            ),
        }
    }
}

/// One tab's content.
pub trait ToolPage {
    /// Build the page's view. It is sized to `frame` and should resize with the window.
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView>;
    /// The tab became visible (also when the window opens on this tab).
    fn shown(&mut self) {}
    /// The window is closing: stop work and drop results.
    fn closed(&mut self) {}
}

type Factory = fn() -> Box<dyn ToolPage>;

/// The page shown until a tool registers its own.
struct Placeholder(Tab);

impl ToolPage for Placeholder {
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
        let root = NSView::initWithFrame(NSView::alloc(mtm), frame);
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let (w, h) = (420.0, 200.0);
        let col = ui::flipped(
            mtm,
            rect(
                (frame.size.width - w) / 2.0,
                (frame.size.height - h) / 2.0,
                w,
                h,
            ),
        );
        col.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        let icon = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            rect((w - 48.0) / 2.0, 0.0, 48.0, 48.0),
        );
        icon.setImage(ui::symbol(self.0.symbol(), 34.0).as_deref());
        icon.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        col.addSubview(&icon);
        let title = ui::label(
            mtm,
            &self.0.title(),
            17.0,
            Weight::Semibold,
            &NSColor::labelColor(),
        );
        title.setAlignment(NSTextAlignment::Center);
        title.setFrame(rect(0.0, 60.0, w, 22.0));
        col.addSubview(&title);
        let blurb = NSTextField::wrappingLabelWithString(&ns(&self.0.blurb()), mtm);
        blurb.setFont(Some(&ui::font(13.0, Weight::Regular)));
        blurb.setTextColor(Some(&NSColor::secondaryLabelColor()));
        blurb.setAlignment(NSTextAlignment::Center);
        blurb.setFrame(rect(0.0, 90.0, w, 54.0));
        col.addSubview(&blurb);
        let note = ui::label(
            mtm,
            &tr("Arrives with the next build."),
            11.0,
            Weight::Regular,
            &NSColor::tertiaryLabelColor(),
        );
        note.setAlignment(NSTextAlignment::Center);
        note.setFrame(rect(0.0, 156.0, w, 15.0));
        col.addSubview(&note);
        root.addSubview(&col);
        root
    }
}

define_class!(
    // SAFETY: NSObject subclass acting as window and tab-view delegate; no Drop impl.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoToolsDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, n: &NSNotification) {
            if let Some(w) = n.object().and_then(|o| o.downcast::<NSWindow>().ok()) {
                ui::release_window_contents(&w);
            }
            util::main_async(closed);
        }
    }

    unsafe impl NSTabViewDelegate for Delegate {
        #[unsafe(method(tabView:didSelectTabViewItem:))]
        fn did_select(&self, tabs: &NSTabView, item: Option<&NSTabViewItem>) {
            let i = item
                .map(|it| tabs.indexOfTabViewItem(it))
                .unwrap_or(0)
                .max(0) as usize;
            if let Some(&t) = Tab::ALL.get(i) {
                util::main_async(move || page_shown(t));
            }
        }
    }
);

struct State {
    window: Retained<NSWindow>,
    tabs: Retained<NSTabView>,
    _delegate: Retained<Delegate>,
    pages: Vec<(Tab, Box<dyn ToolPage>)>,
}

thread_local! {
    static FACTORIES: RefCell<Vec<(Tab, Factory)>> = const { RefCell::new(Vec::new()) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Use `factory` for a tab from now on (replaces the placeholder).
#[allow(dead_code)] // Extension point for the integration step.
pub fn register(tab: Tab, factory: Factory) {
    FACTORIES.with(|f| {
        let mut f = f.borrow_mut();
        f.retain(|(t, _)| *t != tab);
        f.push((tab, factory));
    });
}

fn make_page(tab: Tab) -> Box<dyn ToolPage> {
    FACTORIES
        .with(|f| f.borrow().iter().find(|(t, _)| *t == tab).map(|(_, f)| f()))
        .unwrap_or_else(|| Box::new(Placeholder(tab)))
}

/// Open the window on a tab. From the menu-bar app this goes to the tools
/// process (see `tools_process`); in the tools process it opens here.
pub fn show(tab: Tab) {
    show_path(tab.key());
}

/// `clean`, `space/Applications`…: a tab key with an optional argument.
pub fn show_path(path: &str) {
    if crate::tools_process::is_child() {
        let (tab, rest) = path.split_once('/').unwrap_or((path, ""));
        let tab = Tab::from_key(tab).unwrap_or(Tab::Clean);
        show_here(tab);
        // Development builds: `tools/space/<path>` scans that folder (through the same Full
        // Disk Access check as a picked folder).
        #[cfg(dev)]
        if tab == Tab::Space && !rest.is_empty() {
            crate::pages::space::scan_folder(std::path::PathBuf::from(format!("/{rest}")));
        }
        #[cfg(not(dev))]
        let _ = rest;
    } else {
        crate::tools_process::send(&format!("tab {path}"));
    }
}

pub fn is_open() -> bool {
    STATE.with(|s| s.borrow().is_some())
}

/// Bring this (tools) process to the front. Activation is cooperative since
/// macOS 14: Kipimo yields it to us, so ask again shortly after in case the
/// request came first.
pub fn activate() {
    fn ask() {
        let app = ui::app(util::mtm());
        app.activate();
        // A child process got no user input of its own, so the cooperative request
        // alone isn't always granted; the older call still is.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    ask();
    for delay in [0.3, 1.0] {
        util::main_after(delay, ask);
    }
}

/// Open the window on a tab here (or bring it forward and switch).
pub fn show_here(tab: Tab) {
    let mtm = util::mtm();
    if let Some((w, tabs)) = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| (s.window.clone(), s.tabs.clone()))
    }) {
        tabs.selectTabViewItemAtIndex(tab.index() as isize);
        activate();
        w.makeKeyAndOrderFront(None);
        return;
    }
    let (w, h) = (820.0, 560.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns("Kipimo"));
    window.setContentMinSize(NSSize::new(680.0, 440.0));
    let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(0.0, 0.0, w, h));
    tabs.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let content = rect(0.0, 0.0, w - 20.0, h - 44.0);
    let mut pages = Vec::new();
    for t in Tab::ALL {
        let mut page = make_page(t);
        let view = page.view(mtm, content);
        let item = unsafe {
            NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), Some(&ns(t.key())))
        };
        item.setLabel(&ns(&t.title()));
        item.setView(Some(&view));
        tabs.addTabViewItem(&item);
        pages.push((t, page));
    }
    let this = Delegate::alloc(mtm).set_ivars(());
    let delegate: Retained<Delegate> = unsafe { msg_send![super(this), init] };
    window.setContentView(Some(&tabs));
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    tabs.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.setFrameAutosaveName(&ns("KipimoTools"));
    if !window.setFrameUsingName(&ns("KipimoTools")) {
        window.center();
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            window: window.clone(),
            tabs: tabs.clone(),
            _delegate: delegate,
            pages,
        })
    });
    tabs.selectTabViewItemAtIndex(tab.index() as isize);
    page_shown(tab);
    activate();
    window.makeKeyAndOrderFront(None);
}

fn page_shown(tab: Tab) {
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut()
            && let Some((_, p)) = s.pages.iter_mut().find(|(t, _)| *t == tab)
        {
            p.shown();
        }
    });
}

/// Close the window if it is open (in the menu-bar app: quit the tools process).
#[cfg(dev)]
pub fn close() {
    if !crate::tools_process::is_child() {
        crate::tools_process::close();
        return;
    }
    close_here();
}

/// Close this process's tools and review windows.
#[cfg(dev)]
pub fn close_here() {
    if let Some(w) = STATE.with(|s| s.borrow().as_ref().map(|s| s.window.clone())) {
        w.close();
    }
    crate::review_window::close();
}

fn closed() {
    let st = STATE.with(|s| s.borrow_mut().take());
    if let Some(mut st) = st {
        for (_, p) in st.pages.iter_mut() {
            p.closed();
        }
        drop(st);
    }
    util::release_memory_soon();
    crate::tools_process::window_closed();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_round_trip() {
        for t in Tab::ALL {
            assert_eq!(Tab::from_key(t.key()), Some(t));
            assert_eq!(Tab::ALL[t.index()], t);
        }
        assert_eq!(
            Tab::ALL.map(|t| t.title()),
            ["Clean up", "Uninstall", "Space", "Startup items", "Updates"]
        );
        assert_eq!(Tab::from_key("nope"), None);
    }
}
