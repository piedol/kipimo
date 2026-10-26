//! A list for the tools pages: a view-based NSTableView whose rows are plain
//! Rust values. A row can carry a checkbox (with a mixed state), a disclosure
//! chevron, an SF Symbol or a file's Finder icon, a title and subtitle,
//! coloured badges ("Unsigned", "Running"), a fraction bar and a right-aligned
//! value (usually a size). The table only builds views for visible rows, so a
//! few thousand rows stay cheap.
//!
//! Clicks come back as [`ListEvent`]s carrying the row's `key`, which the page
//! maps to its own data.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBorderType, NSButton, NSControlStateValueMixed, NSControlStateValueOff,
    NSControlStateValueOn, NSControlTextEditingDelegate, NSImageView, NSScrollView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewStyle, NSTextField, NSView, NSWorkspace,
};
use objc2_foundation::{NSIndexSet, NSNotification, NSObject, NSObjectProtocol, NSRect, NSSize};

use crate::ui::{self, Weight, rect};
use crate::util::ns;
use crate::widgets::{self, Tint};

#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    Symbol(&'static str, Tint),
    /// The Finder icon of a file or bundle.
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub text: String,
    pub tint: Tint,
}

impl Badge {
    pub fn new(text: impl Into<String>, tint: Tint) -> Badge {
        Badge {
            text: text.into(),
            tint,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    On,
    Off,
    Mixed,
}

impl Check {
    pub fn from(on: bool) -> Check {
        if on { Check::On } else { Check::Off }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// The page's own id for this row.
    pub key: usize,
    pub indent: u8,
    /// A section header: bold, not selectable.
    pub header: bool,
    pub check: Option<Check>,
    pub check_enabled: bool,
    /// `Some(expanded)` shows a chevron.
    pub disclosure: Option<bool>,
    /// Leave room for a chevron so rows line up with ones that have one.
    pub disclosure_space: bool,
    pub icon: Option<Icon>,
    pub title: String,
    pub subtitle: String,
    pub badges: Vec<Badge>,
    pub bar: Option<f64>,
    pub value: String,
    pub value_tint: Tint,
    pub dim: bool,
}

impl Row {
    pub fn new(key: usize, title: impl Into<String>) -> Row {
        Row {
            key,
            title: title.into(),
            check_enabled: true,
            ..Row::default()
        }
    }

    pub fn header(key: usize, title: impl Into<String>) -> Row {
        Row {
            header: true,
            ..Row::new(key, title)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListEvent {
    Toggle(usize),
    Disclose(usize),
    Select(Option<usize>),
    DoubleClick(usize),
}

type Handler = Rc<dyn Fn(ListEvent)>;

pub struct SourceIvars {
    rows: RefCell<Vec<Row>>,
    handler: RefCell<Option<Handler>>,
    row_height: Cell<f64>,
}

define_class!(
    // SAFETY: NSObject subclass acting as the table's data source and delegate.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoListSource"]
    #[ivars = SourceIvars]
    struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSTableViewDataSource for Source {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _t: &NSTableView) -> isize {
            self.ivars().rows.borrow().len() as isize
        }
    }

    unsafe impl NSControlTextEditingDelegate for Source {}

    unsafe impl NSTableViewDelegate for Source {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(&self, table: &NSTableView, _col: Option<&NSTableColumn>, row: isize) -> Option<Retained<NSView>> {
            let r = self.ivars().rows.borrow().get(row as usize).cloned();
            // The column's width: the inset style keeps it narrower than the table.
            let w = table.tableColumns().firstObject().map(|c| c.width()).unwrap_or(400.0).max(200.0);
            r.map(|r| cell(self, &r, row, w, self.height(&r)))
        }

        #[unsafe(method(tableView:heightOfRow:))]
        fn height_of_row(&self, _t: &NSTableView, row: isize) -> f64 {
            let rows = self.ivars().rows.borrow();
            rows.get(row as usize).map(|r| self.height(r)).unwrap_or(self.ivars().row_height.get())
        }

        #[unsafe(method(tableView:shouldSelectRow:))]
        fn should_select(&self, _t: &NSTableView, row: isize) -> bool {
            self.ivars().rows.borrow().get(row as usize).is_some_and(|r| !r.header)
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, n: &NSNotification) {
            let Some(table) = n.object().and_then(|o| o.downcast::<NSTableView>().ok()) else { return };
            let i = table.selectedRow();
            let key = (i >= 0).then(|| self.ivars().rows.borrow().get(i as usize).map(|r| r.key)).flatten();
            self.emit(ListEvent::Select(key));
        }
    }

    impl Source {
        #[unsafe(method(toggle:))]
        fn toggle(&self, sender: Option<&AnyObject>) {
            if let Some(k) = self.key_of(sender) {
                self.emit(ListEvent::Toggle(k));
            }
        }

        #[unsafe(method(disclose:))]
        fn disclose(&self, sender: Option<&AnyObject>) {
            if let Some(k) = self.key_of(sender) {
                self.emit(ListEvent::Disclose(k));
            }
        }

        #[unsafe(method(doubleClick:))]
        fn double_click(&self, sender: Option<&AnyObject>) {
            let Some(table) = sender.and_then(|s| s.downcast_ref::<NSTableView>()) else { return };
            let i = table.clickedRow();
            let key = (i >= 0).then(|| self.ivars().rows.borrow().get(i as usize).map(|r| r.key)).flatten();
            if let Some(k) = key {
                self.emit(ListEvent::DoubleClick(k));
            }
        }
    }
);

impl Source {
    fn height(&self, r: &Row) -> f64 {
        if r.header {
            26.0
        } else {
            self.ivars().row_height.get()
        }
    }

    fn key_of(&self, sender: Option<&AnyObject>) -> Option<usize> {
        let tag: isize = unsafe { msg_send![sender?, tag] };
        self.ivars().rows.borrow().get(tag as usize).map(|r| r.key)
    }

    fn emit(&self, e: ListEvent) {
        // Clone the handler out so it may replace the rows while it runs.
        let h = self.ivars().handler.borrow().clone();
        if let Some(h) = h {
            h(e);
        }
    }
}

fn cell(source: &Source, r: &Row, index: isize, w: f64, h: f64) -> Retained<NSView> {
    let mtm = MainThreadMarker::from(source);
    let v = ui::flipped(mtm, rect(0.0, 0.0, w, h));
    if r.header {
        let t = widgets::text(mtm, &r.title, 12.0, Weight::Semibold, Tint::Secondary);
        t.setFrame(rect(6.0, h - 20.0, w - 120.0, 16.0));
        widgets::stretch_width(&t);
        v.addSubview(&t);
        if !r.value.is_empty() {
            let val = widgets::text(mtm, &r.value, 11.0, Weight::Digits, Tint::Secondary);
            widgets::right(&val);
            val.setFrame(rect(w - 106.0, h - 20.0, 100.0, 16.0));
            widgets::pin_right(&val);
            v.addSubview(&val);
        }
        return v;
    }
    let mut x = 4.0 + r.indent as f64 * 20.0;
    if let Some(open) = r.disclosure {
        // SAFETY: `source` lives as long as the table; the selector takes a sender.
        let b = unsafe {
            NSButton::buttonWithImage_target_action(
                &ui::symbol(
                    if open {
                        "chevron.down"
                    } else {
                        "chevron.right"
                    },
                    10.0,
                )
                .expect("SF Symbols"),
                Some(source),
                Some(sel!(disclose:)),
                mtm,
            )
        };
        b.setBordered(false);
        b.setTag(index);
        b.setContentTintColor(Some(&Tint::Secondary.color()));
        b.setFrame(rect(x, (h - 18.0) / 2.0, 18.0, 18.0));
        v.addSubview(&b);
    }
    if r.disclosure.is_some() || r.disclosure_space {
        x += 20.0;
    }
    if let Some(c) = r.check.filter(|_| !r.check_enabled) {
        // A choice the user can't change: no checkbox (an empty enabled box and a disabled
        // one look alike, especially in an inactive window). Included anyway shows a check
        // mark; left out shows nothing, and the row says why.
        if c != Check::Off
            && let Some(img) = ui::symbol("checkmark", 11.0)
        {
            let iv = NSImageView::initWithFrame(
                NSImageView::alloc(mtm),
                rect(x + 2.0, (h - 16.0) / 2.0, 16.0, 16.0),
            );
            iv.setImage(Some(&img));
            iv.setContentTintColor(Some(&Tint::Secondary.color()));
            v.addSubview(&iv);
        }
        x += 24.0;
    } else if let Some(c) = r.check {
        // SAFETY: as above.
        let b = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns(""),
                Some(source),
                Some(sel!(toggle:)),
                mtm,
            )
        };
        b.setAllowsMixedState(c == Check::Mixed);
        b.setState(match c {
            Check::On => NSControlStateValueOn,
            Check::Off => NSControlStateValueOff,
            Check::Mixed => NSControlStateValueMixed,
        });
        b.setTag(index);
        b.setFrame(rect(x, (h - 18.0) / 2.0, 20.0, 18.0));
        v.addSubview(&b);
        x += 24.0;
    }
    if let Some(icon) = &r.icon {
        let s = if h >= 36.0 { 24.0 } else { 18.0 };
        let iv = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(x, (h - s) / 2.0, s, s));
        match icon {
            Icon::Symbol(name, tint) => {
                iv.setImage(ui::symbol(name, s * 0.62).as_deref());
                iv.setContentTintColor(Some(&tint.color()));
            }
            Icon::File(path) => {
                let img = NSWorkspace::sharedWorkspace().iconForFile(&ns(&path.to_string_lossy()));
                img.setSize(NSSize::new(s, s));
                iv.setImage(Some(&img));
            }
        }
        v.addSubview(&iv);
        x += s + 8.0;
    }
    // Right side, right to left: value, bar, badges.
    let mut rx = w - 8.0;
    if !r.value.is_empty() {
        let val = widgets::text(
            mtm,
            &r.value,
            12.0,
            Weight::Digits,
            if r.dim { Tint::Tertiary } else { r.value_tint },
        );
        widgets::right(&val);
        let vw = (ui::text_width(&val) + 6.0).max(52.0);
        val.setFrame(rect(rx - vw, (h - 16.0) / 2.0, vw, 16.0));
        widgets::pin_right(&val);
        v.addSubview(&val);
        rx -= vw + 10.0;
    }
    if let Some(f) = r.bar {
        let bar = ui::Fill::meter(
            mtm,
            rect(rx - 72.0, (h - 6.0) / 2.0, 72.0, 6.0),
            &objc2_app_kit::NSColor::secondaryLabelColor(),
            &objc2_app_kit::NSColor::quaternaryLabelColor(),
            f,
        );
        widgets::pin_right(&bar);
        v.addSubview(&bar);
        rx -= 82.0;
    }
    let title_tint = if r.dim { Tint::Secondary } else { Tint::Normal };
    let title = widgets::text(mtm, &r.title, 13.0, Weight::Regular, title_tint);
    // Reverse-DNS labels (com.example.agent) keep both ends when they don't fit.
    if r.title.contains('.') && !r.title.contains(' ') {
        title.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
    }
    let sub: Option<Retained<NSTextField>> = (!r.subtitle.is_empty()).then(|| {
        widgets::text(
            mtm,
            &r.subtitle,
            11.0,
            Weight::Regular,
            if r.dim {
                Tint::Tertiary
            } else {
                Tint::Secondary
            },
        )
    });
    // Badges go on whichever line has room for them, so neither the name nor the details
    // are cut short when one of them could fit beside them.
    let badge_texts: Vec<(Retained<NSTextField>, f64, Tint)> = r
        .badges
        .iter()
        .map(|b| {
            let t = widgets::text(mtm, &b.text, 10.5, Weight::Medium, b.tint);
            let tw = ui::text_width(&t) + 12.0;
            (t, tw, b.tint)
        })
        .collect();
    let badges_w: f64 = badge_texts.iter().map(|(_, w, _)| w + 6.0).sum();
    let avail = rx - x - 4.0;
    let fits = |t: &NSTextField| ui::text_width(t) + 4.0 + badges_w <= avail;
    // 0: centred in a one-line row, 1: the title line, 2: the subtitle line.
    let line = match &sub {
        None => 0,
        Some(_) if badge_texts.is_empty() || fits(&title) => 1,
        Some(s) if fits(s) => 2,
        Some(s) => {
            if ui::text_width(s) <= ui::text_width(&title) {
                2
            } else {
                1
            }
        }
    };
    let badge_y = match line {
        0 => (h - 18.0) / 2.0,
        1 => h / 2.0 - 18.0,
        _ => h / 2.0,
    };
    let mut bx = rx;
    for (t, tw, tint) in badge_texts.iter().rev() {
        let frame = rect(bx - tw, badge_y, *tw, 18.0);
        let bg = ui::fill(mtm, frame, &tint.color().colorWithAlphaComponent(0.14), 5.0);
        widgets::pin_right(&bg);
        v.addSubview(&bg);
        t.setFrame(rect(
            frame.origin.x + 6.0,
            frame.origin.y + 2.0,
            tw - 12.0,
            14.0,
        ));
        widgets::pin_right(t);
        v.addSubview(t);
        bx -= tw + 6.0;
    }
    let full = avail.max(40.0);
    let short = (avail - badges_w).max(40.0);
    let (title_w, sub_w) = match line {
        0 => (short, full),
        1 => (short, full),
        _ => (full, short),
    };
    match &sub {
        Some(s) => {
            title.setFrame(rect(x, h / 2.0 - 17.0, title_w, 17.0));
            s.setFrame(rect(x, h / 2.0 + 1.0, sub_w, 15.0));
            s.setLineBreakMode(
                if r.subtitle.starts_with('/') || r.subtitle.starts_with('~') {
                    objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle
                } else {
                    objc2_app_kit::NSLineBreakMode::ByTruncatingTail
                },
            );
            widgets::stretch_width(s);
            v.addSubview(s);
        }
        None => title.setFrame(rect(x, (h - 17.0) / 2.0, title_w, 17.0)),
    }
    widgets::stretch_width(&title);
    v.addSubview(&title);
    v
}

/// The list's views and data.
pub struct List {
    pub scroll: Retained<NSScrollView>,
    pub table: Retained<NSTableView>,
    source: Retained<Source>,
    empty: Retained<NSTextField>,
    empty_msg: RefCell<String>,
}

impl List {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        row_height: f64,
        on_event: impl Fn(ListEvent) + 'static,
    ) -> List {
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setBorderType(NSBorderType::NoBorder);
        widgets::stretch(&scroll);
        let table = NSTableView::initWithFrame(
            NSTableView::alloc(mtm),
            rect(0.0, 0.0, frame.size.width, frame.size.height),
        );
        let col = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ns("main"));
        col.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        col.setWidth(frame.size.width - 40.0);
        table.addTableColumn(&col);
        table.setHeaderView(None);
        table.setStyle(NSTableViewStyle::Inset);
        table.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::FirstColumnOnlyAutoresizingStyle,
        );
        table.setRowHeight(row_height);
        let source = {
            let this = Source::alloc(mtm).set_ivars(SourceIvars {
                rows: RefCell::new(Vec::new()),
                handler: RefCell::new(Some(Rc::new(on_event))),
                row_height: Cell::new(row_height),
            });
            let s: Retained<Source> = unsafe { msg_send![super(this), init] };
            s
        };
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
            table.setTarget(Some(&source));
            table.setDoubleAction(Some(sel!(doubleClick:)));
        }
        scroll.setDocumentView(Some(&table));
        table.sizeLastColumnToFit();
        let empty = widgets::text(mtm, "", 13.0, Weight::Regular, Tint::Secondary);
        empty.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        empty.setFrame(rect(
            20.0,
            frame.size.height / 2.0 - 30.0,
            frame.size.width - 40.0,
            18.0,
        ));
        empty.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        empty.setHidden(true);
        List {
            scroll,
            table,
            source,
            empty,
            empty_msg: RefCell::new(String::new()),
        }
    }

    /// Add the list (and its empty-state label) to a container.
    pub fn add_to(&self, parent: &NSView) {
        parent.addSubview(&self.scroll);
        parent.addSubview(&self.empty);
    }

    /// Hide the list entirely (an empty table still draws its background).
    pub fn set_hidden(&self, hidden: bool) {
        self.scroll.setHidden(hidden);
    }

    /// Replace every row, keeping the selection on the same key when it's still there.
    pub fn set_rows(&self, rows: Vec<Row>) {
        let selected = self.selected();
        self.empty
            .setHidden(!rows.is_empty() || self.empty_msg.borrow().is_empty());
        *self.source.ivars().rows.borrow_mut() = rows;
        self.table.reloadData();
        if let Some(k) = selected {
            self.select_key(k);
        }
    }

    pub fn len(&self) -> usize {
        self.source.ivars().rows.borrow().len()
    }

    pub fn selected(&self) -> Option<usize> {
        let i = self.table.selectedRow();
        (i >= 0)
            .then(|| {
                self.source
                    .ivars()
                    .rows
                    .borrow()
                    .get(i as usize)
                    .map(|r| r.key)
            })
            .flatten()
    }

    pub fn select_key(&self, key: usize) {
        let i = self
            .source
            .ivars()
            .rows
            .borrow()
            .iter()
            .position(|r| r.key == key);
        if let Some(i) = i {
            self.table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(i), false);
        }
    }

    /// The text shown while there are no rows ("" shows nothing).
    pub fn set_empty_text(&self, s: &str) {
        *self.empty_msg.borrow_mut() = s.to_string();
        self.empty.setStringValue(&ns(s));
        self.empty.setHidden(s.is_empty() || self.len() > 0);
    }

    /// Drop the closure (and whatever it captured) before the views go.
    pub fn detach(&self) {
        self.source.ivars().handler.borrow_mut().take();
        unsafe {
            self.table.setDataSource(None);
            self.table.setDelegate(None);
            self.table.setTarget(None);
        }
    }
}

impl Drop for List {
    fn drop(&mut self) {
        self.detach();
    }
}
