//! Shared scaffolding for the tools pages: a header (title, explanation,
//! status, spinner, buttons on the right), a footer (summary text and the
//! page's main action), confirmation alerts, and a background-job runner.
//!
//! Pages keep their state in a thread-local on the main thread. Controls and
//! background jobs never touch it directly: they post a message with
//! [`post`], which runs the page's handler on a later turn of the main run
//! loop. That keeps handlers from re-entering each other (a table reload can
//! fire a selection change while the state is borrowed).

use std::panic::{AssertUnwindSafe, catch_unwind};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle, NSButton,
    NSProgressIndicator, NSTextField, NSView,
};
use objc2_foundation::NSRect;

use crate::i18n::{tr, trf};
use crate::ui::{self, Weight, rect};
use crate::util::{self, ns};
use crate::widgets::{self, Targets, Tint};

pub const MARGIN: f64 = 20.0;
pub const HEADER_H: f64 = 64.0;
pub const FOOTER_H: f64 = 52.0;

/// Run `handler(msg)` on the main thread, on a later turn of the run loop.
pub fn post<M: Send + 'static>(handler: fn(M), msg: M) {
    util::main_async(move || handler(msg));
}

/// How eagerly a job runs.
#[derive(Clone, Copy)]
pub enum Priority {
    /// The user is waiting (a scan they started).
    User,
    /// Nobody is waiting (weekly summary, daily checks).
    Background,
}

/// Run `work` on a background queue with a panic guard; `done` gets its
/// result (or the panic message) on the main thread. Engine bugs must not take
/// the menu-bar app down.
pub fn spawn<R: Send + 'static>(
    priority: Priority,
    work: impl FnOnce() -> R + Send + 'static,
    done: impl FnOnce(Result<R, String>) + Send + 'static,
) {
    let qos = match priority {
        Priority::User => dispatch2::DispatchQoS::UserInitiated,
        Priority::Background => dispatch2::DispatchQoS::Background,
    };
    dispatch2::DispatchQueue::global_queue(dispatch2::GlobalQueueIdentifier::QualityOfService(qos))
        .exec_async(move || {
            let r = catch_unwind(AssertUnwindSafe(work)).map_err(|p| {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| tr("unknown error"));
                util::log(format!("job panicked: {msg}"));
                trf(
                    "Something went wrong inside Kipimo ({error}).",
                    &[("error", &msg)],
                )
            });
            util::main_async(move || done(r));
        });
}

/// A flipped page root that follows the window's size.
pub fn root(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let v = ui::flipped(mtm, rect(0.0, 0.0, frame.size.width, frame.size.height));
    widgets::stretch(&v);
    v
}

pub struct Header {
    _title: Retained<NSTextField>,
    _subtitle: Retained<NSTextField>,
    pub status: Retained<NSTextField>,
    pub spinner: Retained<NSProgressIndicator>,
    /// Where the next header button goes (its right edge).
    next_x: f64,
}

impl Header {
    pub fn new(
        mtm: MainThreadMarker,
        root: &NSView,
        w: f64,
        title: &str,
        subtitle: &str,
    ) -> Header {
        let t = widgets::text(mtm, title, 17.0, Weight::Semibold, Tint::Normal);
        t.setFrame(rect(MARGIN, 14.0, w - 2.0 * MARGIN, 22.0));
        widgets::stretch_width(&t);
        root.addSubview(&t);
        let s = widgets::text(mtm, subtitle, 12.0, Weight::Regular, Tint::Secondary);
        s.setFrame(rect(MARGIN, 38.0, w - 2.0 * MARGIN - 300.0, 16.0));
        widgets::stretch_width(&s);
        root.addSubview(&s);
        let status = widgets::text(mtm, "", 11.0, Weight::Digits, Tint::Secondary);
        widgets::right(&status);
        status.setFrame(rect(w - MARGIN - 300.0, 40.0, 300.0, 15.0));
        widgets::pin_right(&status);
        root.addSubview(&status);
        let spinner = widgets::spinner(mtm, rect(w - MARGIN - 318.0, 39.0, 16.0, 16.0));
        widgets::pin_right(&spinner);
        root.addSubview(&spinner);
        Header {
            _title: t,
            _subtitle: s,
            status,
            spinner,
            next_x: w - MARGIN + 6.0,
        }
    }

    /// A button at the header's right, laid out right to left.
    pub fn button(
        &mut self,
        mtm: MainThreadMarker,
        root: &NSView,
        title: &str,
        targets: &mut Targets,
        f: impl Fn() + 'static,
    ) -> Retained<NSButton> {
        let b = widgets::button(mtm, title, targets, f);
        let bw = ui::text_width(&b) + 28.0;
        self.next_x -= bw;
        b.setFrame(rect(self.next_x, 10.0, bw, 30.0));
        widgets::pin_right(&b);
        root.addSubview(&b);
        self.next_x -= 4.0;
        b
    }

    pub fn busy(&self, on: bool, status: &str) {
        widgets::spin(&self.spinner, on);
        widgets::set_text(&self.status, status);
    }
}

pub struct Footer {
    pub text: Retained<NSTextField>,
    next_x: f64,
    y: f64,
}

impl Footer {
    pub fn new(mtm: MainThreadMarker, root: &NSView, w: f64, h: f64) -> Footer {
        let y = h - FOOTER_H + 14.0;
        let text = widgets::text(mtm, "", 12.0, Weight::Regular, Tint::Secondary);
        text.setFrame(rect(MARGIN, y + 6.0, w - 2.0 * MARGIN - 360.0, 16.0));
        text.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        root.addSubview(&text);
        Footer {
            text,
            next_x: w - MARGIN + 6.0,
            y,
        }
    }

    /// A button at the footer's right, laid out right to left.
    pub fn button(
        &mut self,
        mtm: MainThreadMarker,
        root: &NSView,
        title: &str,
        targets: &mut Targets,
        f: impl Fn() + 'static,
    ) -> Retained<NSButton> {
        let b = widgets::button(mtm, title, targets, f);
        let bw = (ui::text_width(&b) + 28.0).max(96.0);
        self.next_x -= bw;
        b.setFrame(rect(self.next_x, self.y, bw, 30.0));
        widgets::pin_bottom_right(&b);
        root.addSubview(&b);
        self.next_x -= 4.0;
        // The summary text ends where the buttons start.
        let mut f = self.text.frame();
        f.size.width = (self.next_x - f.origin.x - 8.0).max(60.0);
        self.text.setFrame(f);
        b
    }
}

/// The frame between header (plus `extra` for banners or bars) and footer.
pub fn body(w: f64, h: f64, extra: f64) -> NSRect {
    rect(
        0.0,
        HEADER_H + extra,
        w,
        (h - HEADER_H - extra - FOOTER_H).max(100.0),
    )
}

/// Ask before something destructive. Cancel is the default button (Return), so a second
/// Return can't move anything by accident; `true` only when `action` itself is clicked.
pub fn confirm(title: &str, detail: &str, action: &str) -> bool {
    let mtm = util::mtm();
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(detail));
    a.setAlertStyle(NSAlertStyle::Warning);
    let cancel = a.addButtonWithTitle(&ns(&tr("Cancel")));
    cancel.setKeyEquivalent(&ns("\r"));
    let go = a.addButtonWithTitle(&ns(action));
    go.setKeyEquivalent(&ns(""));
    go.setHasDestructiveAction(true);
    a.runModal() == NSAlertSecondButtonReturn
}

/// Offer a harmless next step (like opening System Settings). `true` for `action`, the default.
pub fn ask(title: &str, detail: &str, action: &str) -> bool {
    let mtm = util::mtm();
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(detail));
    a.addButtonWithTitle(&ns(action));
    a.addButtonWithTitle(&ns(&tr("Not Now")));
    a.runModal() == NSAlertFirstButtonReturn
}

/// Tell the user something that needs no decision.
pub fn inform(title: &str, detail: &str) {
    let mtm = util::mtm();
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(detail));
    a.runModal();
}

/// "~/Library/Caches/…" for display.
pub fn tilde(p: &std::path::Path) -> String {
    let home = kipimo_clean::fake_home()
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// "3 days ago" from Unix seconds.
pub fn ago(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::format::days_ago(now.saturating_sub(unix).max(0) as u64)
}

/// Whether Kipimo can read privacy-protected folders. `None` = can't tell.
pub fn full_disk_access() -> Option<bool> {
    kipimo_space::has_full_disk_access()
}

/// Open System Settings › Privacy & Security › Full Disk Access.
/// System Settings › Privacy & Security › App Management (needed to move some signed apps).
pub const APP_MANAGEMENT_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AppBundles";

pub fn open_app_management() {
    crate::actions::run(crate::actions::Action::OpenUrl(APP_MANAGEMENT_URL.into()));
}

pub fn open_full_disk_access() {
    crate::actions::run(crate::actions::Action::OpenUrl(
        crate::recommend::FULL_DISK_ACCESS_URL.into(),
    ));
}

/// Show items in Finder.
pub fn reveal(paths: &[std::path::PathBuf]) {
    let urls: Vec<_> = paths
        .iter()
        .map(|p| objc2_foundation::NSURL::fileURLWithPath(&ns(&p.to_string_lossy())))
        .collect();
    if urls.is_empty() {
        return;
    }
    let arr = objc2_foundation::NSArray::from_retained_slice(&urls);
    objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&arr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let home = kipimo_clean::fake_home()
            .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap()));
        assert_eq!(tilde(&home.join("Library/Caches")), "~/Library/Caches");
        assert_eq!(
            tilde(std::path::Path::new("/Applications")),
            "/Applications"
        );
    }
}
