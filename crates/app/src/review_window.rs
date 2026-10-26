//! The window that opens from the "X left N files" alert: the same review as
//! the Uninstall page, for an app that is already in the Trash. Its files go to
//! the Trash only after the user ticks and confirms them. It opens in the
//! tools process (see `tools_process`), which re-runs the leftover finder on
//! the bundle so nothing has to be passed across.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kipimo_clean::{Env, ItemResult, Leftover, LeftoverReport};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSSize};

use crate::format;
use crate::insights;
use crate::page::{self, Priority};
use crate::review::Review;
use crate::ui::{self, rect};
use crate::util::{self, ns};

define_class!(
    // SAFETY: NSObject subclass acting as the window delegate; no Drop impl.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "KipimoReviewWindowDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, n: &NSNotification) {
            if let Some(w) = n.object().and_then(|o| o.downcast::<NSWindow>().ok()) {
                ui::release_window_contents(&w);
            }
            util::main_async(|| {
                if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
                    st.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                util::release_memory_soon();
                crate::tools_process::window_closed();
            });
        }
    }
);

struct State {
    window: Retained<NSWindow>,
    _delegate: Retained<Delegate>,
    review: Review,
    bundle: PathBuf,
    cancel: Arc<AtomicBool>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    STATE.with(|s| s.borrow().is_some())
}

/// From the alert or the recommendation (`Action::Custom("leftovers:<bundle id>")`):
/// open the review in the tools process.
pub fn show(bundle_id: &str) {
    let path = insights::leftovers(bundle_id).and_then(|l| l.report.target.bundle_path.clone());
    match path {
        Some(p) => crate::tools_process::send(&format!("review {}", p.display())),
        None => page::inform(
            "Nothing to review",
            "Kipimo no longer has that app's leftovers on file. Find them again in Kipimo › Uninstall.",
        ),
    }
}

/// In the tools process: find the leftovers of `bundle` and show them.
pub fn show_bundle(bundle: PathBuf) {
    let mtm = util::mtm();
    if let Some(w) = STATE.with(|s| s.borrow().as_ref().filter(|s| s.bundle == bundle).map(|s| s.window.clone())) {
        crate::tools_window::activate();
        w.makeKeyAndOrderFront(None);
        return;
    }
    close();
    let name = bundle.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (w, h) = (700.0, 560.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns(&format!("Leftovers from {name}")));
    window.setContentMinSize(NSSize::new(560.0, 420.0));
    let review = Review::new(mtm, rect(0.0, 0.0, w, h), |picked| page::post(remove, picked));
    review.loading(None, &name);
    window.setContentView(Some(&review.view));
    let this = Delegate::alloc(mtm).set_ivars(());
    let delegate: Retained<Delegate> = unsafe { msg_send![super(this), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.center();
    let cancel = Arc::new(AtomicBool::new(false));
    STATE.with(|s| {
        *s.borrow_mut() = Some(State { window: window.clone(), _delegate: delegate, review, bundle: bundle.clone(), cancel: cancel.clone() })
    });
    crate::tools_window::activate();
    window.makeKeyAndOrderFront(None);
    let b = bundle.clone();
    page::spawn(
        Priority::User,
        move || {
            let env = Env::current_user().map_err(|e| e.to_string())?;
            kipimo_clean::find_leftovers_for_bundle(&env, &b, &cancel).ok_or_else(|| "That app has no bundle identifier.".to_string())
        },
        move |r| page::post(found, (bundle, r.and_then(|r| r))),
    );
}

fn found((bundle, r): (PathBuf, Result<LeftoverReport, String>)) {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref().filter(|s| s.bundle == bundle) else { return };
        match r {
            Ok(report) if report.leftovers.is_empty() => s.review.clear("It didn't leave anything behind."),
            Ok(report) => s.review.show(None, report),
            Err(e) => s.review.clear(&e),
        }
    });
}

pub fn close() {
    if let Some(w) = STATE.with(|s| s.borrow().as_ref().map(|s| s.window.clone())) {
        w.close();
    }
}

fn remove(picked: Vec<Leftover>) {
    let Some((report, cancel)) = STATE.with(|s| s.borrow().as_ref().and_then(|s| Some((s.review.report()?, s.cancel.clone())))) else {
        return;
    };
    let bytes: u64 = picked.iter().map(|l| l.bytes).sum();
    if !page::confirm(
        &format!("Move {} items to the Trash?", picked.len()),
        &format!("{} in all. You can put them back from the Trash in Finder.", format::bytes(bytes)),
        "Move to Trash",
    ) {
        return;
    }
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            s.review.busy("Moving to the Trash…");
        }
    });
    let id = report.target.bundle_id.clone();
    page::spawn(
        Priority::User,
        move || {
            let env = Env::current_user().map_err(|e| e.to_string())?;
            Ok::<_, String>(kipimo_clean::remove_leftovers(&env, &report.target, &picked, &mut |_, _| {}, &cancel))
        },
        move |r| page::post(removed, (id, r.and_then(|r| r))),
    );
}

fn removed((bundle_id, r): (String, Result<Vec<ItemResult>, String>)) {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref() else { return };
        match r {
            Ok(results) => {
                let n = results.iter().filter(|r| r.moved()).count();
                s.review.results(None, results, &format!("{n} items are in the Trash. Put Back in Finder restores them."));
            }
            Err(e) => s.review.clear(&e),
        }
    });
    insights::leftovers_reviewed(&bundle_id);
}
