//! Reviewing an app's files before they go to the Trash: the app itself (when
//! uninstalling) and its leftovers, with sizes up front. Anything over 1 GB
//! starts unticked, so a big container (OrbStack's 33 GB, a VM image) is never
//! swept up by default. Used by the Uninstall page and by the window that
//! opens from the "app moved to the Trash" alert.

use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use kipimo_clean::{Confidence, ItemResult, Leftover, LeftoverKind, LeftoverReport, Protection};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSTextField, NSView};
use objc2_foundation::NSRect;

use crate::format;
use crate::list::{Badge, Check, Icon, List, ListEvent, Row};
use crate::page::{self, MARGIN};
use crate::ui::{Weight, rect};
use crate::widgets::{self, Targets, Tint};

/// Leftovers above this start unticked.
pub const BIG_ITEM: u64 = kipimo_clean::BIG_ITEM;

#[derive(Clone, Debug)]
pub struct ReviewApp {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub running: bool,
    pub protected: Option<Protection>,
    /// Owned by another user (root, from a `.pkg`): only Finder, with a password, can move it.
    pub admin_owned: bool,
}

impl ReviewApp {
    pub fn admin_owned(path: &Path) -> bool {
        // SAFETY: getuid never fails.
        let me = unsafe { libc::getuid() };
        std::fs::symlink_metadata(path).is_ok_and(|m| m.uid() != me)
    }
}

/// Whether a leftover starts ticked: the engine's default, never over 1 GB, never one that needs an administrator.
pub fn starts_ticked(l: &Leftover) -> bool {
    l.default_selected && l.bytes <= BIG_ITEM && !l.needs_admin
}

pub fn kind_label(k: LeftoverKind) -> &'static str {
    match k {
        LeftoverKind::ApplicationSupport => "Application Support",
        LeftoverKind::ApplicationScripts => "Application Scripts",
        LeftoverKind::Caches => "Cache",
        LeftoverKind::Container => "Container",
        LeftoverKind::GroupContainer => "Group container",
        LeftoverKind::Preferences => "Preferences",
        LeftoverKind::ByHostPreferences => "Preferences (this Mac)",
        LeftoverKind::SavedState => "Saved window state",
        LeftoverKind::HttpStorage => "Web storage",
        LeftoverKind::WebKit => "WebKit data",
        LeftoverKind::Logs => "Logs",
        LeftoverKind::Cookies => "Cookies",
        LeftoverKind::LaunchAgent => "Launch agent",
        LeftoverKind::LaunchDaemon => "Launch daemon",
        LeftoverKind::PrivilegedHelper => "Privileged helper",
    }
}

fn kind_symbol(k: LeftoverKind) -> &'static str {
    match k {
        LeftoverKind::Caches => "shippingbox",
        LeftoverKind::Preferences | LeftoverKind::ByHostPreferences => "slider.horizontal.3",
        LeftoverKind::Logs => "doc.text",
        LeftoverKind::LaunchAgent | LeftoverKind::LaunchDaemon | LeftoverKind::PrivilegedHelper => {
            "power"
        }
        LeftoverKind::Cookies | LeftoverKind::HttpStorage | LeftoverKind::WebKit => "globe",
        LeftoverKind::SavedState => "macwindow",
        _ => "folder",
    }
}

#[derive(Default)]
struct Data {
    app: Option<ReviewApp>,
    report: Option<LeftoverReport>,
    ticked: Vec<bool>,
    results: Option<(Option<ItemResult>, Vec<ItemResult>)>,
    busy: bool,
}

pub struct Review {
    pub view: Retained<NSView>,
    data: Rc<RefCell<Data>>,
    list: Rc<List>,
    title: Retained<NSTextField>,
    size: Retained<NSTextField>,
    summary: Retained<NSTextField>,
    note: Retained<NSTextField>,
    button: Retained<NSButton>,
    reveal: Retained<NSButton>,
    spinner: Retained<objc2_app_kit::NSProgressIndicator>,
    _targets: Targets,
}

impl Review {
    /// `on_move` gets the ticked leftovers when the user confirms.
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        on_move: impl Fn(Vec<Leftover>) + 'static,
    ) -> Review {
        let (w, h) = (frame.size.width, frame.size.height);
        let view = crate::ui::flipped(mtm, frame);
        widgets::stretch(&view);
        let title = widgets::text(mtm, "", 17.0, Weight::Semibold, Tint::Normal);
        title.setFrame(rect(MARGIN, 14.0, w - 2.0 * MARGIN, 22.0));
        widgets::stretch_width(&title);
        view.addSubview(&title);
        let size = widgets::text(mtm, "", 26.0, Weight::BigDigits, Tint::Normal);
        size.setFrame(rect(MARGIN, 38.0, w - 2.0 * MARGIN, 32.0));
        widgets::stretch_width(&size);
        view.addSubview(&size);
        let summary = widgets::wrap(mtm, "", 12.0, Tint::Secondary);
        summary.setFrame(rect(MARGIN, 72.0, w - 2.0 * MARGIN, 32.0));
        widgets::stretch_width(&summary);
        view.addSubview(&summary);
        let spinner = widgets::spinner(mtm, rect(w - MARGIN - 16.0, 18.0, 16.0, 16.0));
        widgets::pin_right(&spinner);
        view.addSubview(&spinner);

        let data = Rc::new(RefCell::new(Data::default()));
        let list_slot: Rc<RefCell<Weak<List>>> = Rc::new(RefCell::new(Weak::new()));
        let (d, slot) = (data.clone(), list_slot.clone());
        let list = Rc::new(List::new(
            mtm,
            rect(0.0, 108.0, w, h - 108.0 - 60.0),
            40.0,
            move |e| {
                let ListEvent::Toggle(k) = e else { return };
                let Ok(mut data) = d.try_borrow_mut() else {
                    return;
                };
                if data.busy || data.results.is_some() || k == 0 {
                    return;
                }
                let i = k - 1;
                let can = data
                    .report
                    .as_ref()
                    .and_then(|r| r.leftovers.get(i))
                    .is_some_and(|l| !l.needs_admin);
                if can && let Some(t) = data.ticked.get_mut(i) {
                    *t = !*t;
                }
                drop(data);
                if let Some(list) = slot.borrow().upgrade() {
                    list.set_rows(rows(&d.borrow()));
                }
            },
        ));
        *list_slot.borrow_mut() = Rc::downgrade(&list);
        list.add_to(&view);

        let mut targets = Targets::default();
        let note = widgets::wrap(mtm, "", 12.0, Tint::Secondary);
        note.setFrame(rect(MARGIN, h - 50.0, w - 2.0 * MARGIN - 320.0, 40.0));
        note.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        view.addSubview(&note);
        let d = data.clone();
        let button = widgets::button(mtm, "Move to Trash", &mut targets, move || {
            let picked = {
                let data = d.borrow();
                let Some(report) = &data.report else { return };
                report
                    .leftovers
                    .iter()
                    .zip(&data.ticked)
                    .filter(|(_, t)| **t)
                    .map(|(l, _)| l.clone())
                    .collect::<Vec<_>>()
            };
            on_move(picked);
        });
        button.setFrame(rect(w - MARGIN - 140.0, h - 44.0, 140.0, 30.0));
        widgets::pin_bottom_right(&button);
        view.addSubview(&button);
        let d = data.clone();
        let reveal = widgets::button(mtm, "Show in Finder", &mut targets, move || {
            if let Some(app) = &d.borrow().app {
                page::reveal(std::slice::from_ref(&app.path));
            }
        });
        reveal.setFrame(rect(w - MARGIN - 290.0, h - 44.0, 146.0, 30.0));
        widgets::pin_bottom_right(&reveal);
        view.addSubview(&reveal);
        let r = Review {
            view,
            data,
            list,
            title,
            size,
            summary,
            note,
            button,
            reveal,
            spinner,
            _targets: targets,
        };
        r.clear("Select an app to see what it installed.");
        r
    }

    /// An empty review with a message.
    pub fn clear(&self, msg: &str) {
        *self.data.borrow_mut() = Data::default();
        widgets::set_text(&self.title, "");
        widgets::set_text(&self.size, "");
        widgets::set_text(&self.summary, "");
        widgets::set_text(&self.note, "");
        self.list.set_empty_text(msg);
        self.list.set_rows(Vec::new());
        self.list.set_hidden(true);
        self.button.setHidden(true);
        self.reveal.setHidden(true);
        widgets::spin(&self.spinner, false);
    }

    /// The finder is looking.
    pub fn loading(&self, app: Option<ReviewApp>, name: &str) {
        self.clear(&format!(
            "Looking for the files {name} left around your Mac…"
        ));
        widgets::set_text(&self.title, name);
        if let Some(a) = &app {
            widgets::set_text(&self.size, &format::bytes(a.bytes));
        }
        self.data.borrow_mut().app = app;
        widgets::spin(&self.spinner, true);
    }

    pub fn show(&self, app: Option<ReviewApp>, report: LeftoverReport) {
        widgets::spin(&self.spinner, false);
        {
            let mut d = self.data.borrow_mut();
            d.ticked = report.leftovers.iter().map(starts_ticked).collect();
            d.app = app;
            d.report = Some(report);
            d.results = None;
            d.busy = false;
        }
        self.list.set_empty_text("");
        self.list.set_hidden(false);
        self.refresh();
    }

    pub fn busy(&self, text: &str) {
        self.data.borrow_mut().busy = true;
        widgets::spin(&self.spinner, true);
        widgets::set_text(&self.note, text);
        self.button.setEnabled(false);
        self.list.set_rows(rows(&self.data.borrow()));
    }

    pub fn results(&self, app: Option<ItemResult>, leftovers: Vec<ItemResult>, note: &str) {
        widgets::spin(&self.spinner, false);
        let freed: u64 = app
            .iter()
            .chain(leftovers.iter())
            .filter(|r| r.moved())
            .map(|r| r.bytes)
            .sum();
        {
            let mut d = self.data.borrow_mut();
            d.busy = false;
            d.results = Some((app, leftovers));
        }
        widgets::set_text(&self.size, &format!("{} freed", format::bytes(freed)));
        widgets::set_text(&self.note, note);
        self.button.setHidden(true);
        self.reveal.setHidden(true);
        self.list.set_hidden(false);
        self.list.set_rows(rows(&self.data.borrow()));
    }

    /// The app this review is about, if any.
    pub fn app(&self) -> Option<ReviewApp> {
        self.data.borrow().app.clone()
    }

    /// The report shown, if any.
    pub fn report(&self) -> Option<LeftoverReport> {
        self.data.borrow().report.clone()
    }

    fn refresh(&self) {
        let d = self.data.borrow();
        let Some(report) = &d.report else { return };
        let picked: Vec<&Leftover> = report
            .leftovers
            .iter()
            .zip(&d.ticked)
            .filter(|(_, t)| **t)
            .map(|(l, _)| l)
            .collect();
        let picked_bytes: u64 = picked.iter().map(|l| l.bytes).sum();
        let big = report
            .leftovers
            .iter()
            .filter(|l| l.bytes > BIG_ITEM)
            .count();
        match &d.app {
            Some(app) => {
                widgets::set_text(&self.title, &app.name);
                widgets::set_text(&self.size, &format::bytes(app.bytes + picked_bytes));
                let mut s = format!(
                    "{} ({}) and {} of its {} files ({}).",
                    app.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    format::bytes(app.bytes),
                    picked.len(),
                    report.leftovers.len(),
                    format::bytes(picked_bytes)
                );
                if big > 0 {
                    s.push_str(" Items over 1 GB start unticked.");
                }
                widgets::set_text(&self.summary, &s);
                self.reveal.setHidden(false);
                let (enabled, note) = if let Some(p) = app.protected {
                    (
                        false,
                        match p {
                            Protection::ShipsWithMacOS => {
                                format!("{} is part of macOS and can't be removed.", app.name)
                            }
                            Protection::Kipimo => "That's Kipimo itself.".to_string(),
                        },
                    )
                } else if app.running {
                    (
                        false,
                        format!("Quit {} first, then select it again.", app.name),
                    )
                } else if app.admin_owned {
                    (
                        false,
                        format!(
                            "{} was installed by an installer and belongs to the system. Drag it to the Trash in Finder (it asks for your password); Kipimo then offers to clean up what it left.",
                            app.name
                        ),
                    )
                } else if report.other_copy.is_some() {
                    (
                        true,
                        "Another copy of this app is installed, so its files stay.".to_string(),
                    )
                } else {
                    (
                        true,
                        "Everything goes to the Trash; Put Back in Finder restores it.".to_string(),
                    )
                };
                self.button.setHidden(false);
                self.button.setEnabled(enabled && !d.busy);
                widgets::set_text(&self.note, &note);
            }
            None => {
                let name = report
                    .target
                    .names
                    .first()
                    .cloned()
                    .unwrap_or_else(|| report.target.bundle_id.clone());
                widgets::set_text(&self.title, &format!("Files left by {name}"));
                widgets::set_text(&self.size, &format::bytes(picked_bytes));
                let mut s = format!(
                    "{} of {} items selected ({} in all).",
                    picked.len(),
                    report.leftovers.len(),
                    format::bytes(report.total_bytes)
                );
                if big > 0 {
                    s.push_str(" Items over 1 GB start unticked.");
                }
                widgets::set_text(&self.summary, &s);
                self.button.setHidden(false);
                self.button.setEnabled(!picked.is_empty() && !d.busy);
                widgets::set_text(
                    &self.note,
                    "They go to the Trash; Put Back in Finder restores them.",
                );
            }
        }
        drop(d);
        self.list.set_rows(rows(&self.data.borrow()));
    }
}

fn rows(d: &Data) -> Vec<Row> {
    if let Some((app, leftovers)) = &d.results {
        let all: Vec<ItemResult> = app
            .iter()
            .cloned()
            .chain(leftovers.iter().cloned())
            .collect();
        return crate::pages::result_rows(&all, 1);
    }
    let mut out = Vec::new();
    let Some(report) = &d.report else { return out };
    if let Some(app) = &d.app {
        let mut r = Row::new(0, &app.name);
        r.icon = Some(Icon::File(app.path.clone()));
        r.check = Some(Check::On);
        r.check_enabled = false;
        r.subtitle = page::tilde(&app.path);
        r.value = format::bytes(app.bytes);
        if app.running {
            r.badges.push(Badge::new("Running", Tint::Orange));
        }
        if app.protected.is_some() {
            r.badges.push(Badge::new("Part of macOS", Tint::Secondary));
        } else if app.admin_owned {
            r.badges
                .push(Badge::new("Installed by an installer", Tint::Secondary));
        }
        out.push(r);
    }
    for (i, l) in report.leftovers.iter().enumerate() {
        let name = l
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut r = Row::new(i + 1, name);
        r.icon = Some(Icon::Symbol(kind_symbol(l.kind), Tint::Secondary));
        r.check = Some(Check::from(d.ticked.get(i).copied().unwrap_or(false)));
        r.check_enabled = !l.needs_admin && !d.busy;
        r.subtitle = format!("{} · {}", kind_label(l.kind), page::tilde(&l.path));
        r.value = format::bytes(l.bytes);
        if l.bytes > BIG_ITEM {
            r.value_tint = Tint::Orange;
            r.badges.push(Badge::new("Over 1 GB", Tint::Orange));
        }
        if l.needs_admin {
            r.badges
                .push(Badge::new("Needs an administrator", Tint::Secondary));
        }
        if l.confidence == Confidence::Low {
            r.badges.push(Badge::new("Unsure match", Tint::Orange));
        }
        r.dim = l.needs_admin;
        out.push(r);
    }
    if !report.kept.is_empty() {
        let base = report.leftovers.len() + 10;
        out.push(Row::header(base, "Kept: another app still uses these"));
        for (i, k) in report.kept.iter().enumerate() {
            let name = k
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut r = Row::new(base + 1 + i, name);
            r.icon = Some(Icon::Symbol(kind_symbol(k.kind), Tint::Tertiary));
            r.subtitle = format!(
                "{} · {}",
                crate::pages::skip_text(&k.reason),
                page::tilde(&k.path)
            );
            r.dim = true;
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leftover(bytes: u64, default_selected: bool, needs_admin: bool) -> Leftover {
        Leftover {
            path: PathBuf::from("/Users/x/Library/Group Containers/HUAQ24HBR6.dev.orbstack"),
            kind: LeftoverKind::GroupContainer,
            confidence: Confidence::High,
            reason: "matches the app's team".into(),
            bytes,
            needs_admin,
            default_selected,
        }
    }

    #[test]
    fn big_items_start_unticked() {
        // OrbStack's group container on this Mac: 33.5 GB.
        assert!(!starts_ticked(&leftover(33_500_000_000, true, false)));
        assert!(starts_ticked(&leftover(BIG_ITEM, true, false)));
        assert!(starts_ticked(&leftover(12_000, true, false)));
        assert!(!starts_ticked(&leftover(12_000, false, false)));
        assert!(!starts_ticked(&leftover(12_000, true, true)));
    }
}
