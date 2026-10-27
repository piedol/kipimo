//! The window that opens from the "X left N files" alert: the same review as
//! the Uninstall page, for an app that is already in the Trash. Its files go to
//! the Trash only after the user ticks and confirms them. It opens in the
//! tools process (see `tools_process`), which re-runs the leftover finder on
//! the bundle so nothing has to be passed across.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kipimo_clean::{AppIndex, AppTarget, Env, ItemResult, Leftover, LeftoverReport};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSSize};

use crate::format;
use crate::i18n::{tr, trf, trn};
use crate::i18n_text::fill;
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
    source: Source,
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
    match insights::leftovers(bundle_id) {
        // Still in the Trash: review it from its bundle.
        Some(l) if in_trash(&l.bundle_path) => {
            crate::tools_process::send(&format!("review {}", l.bundle_path.display()))
        }
        // The Trash was emptied, or the app was put back: go by its bundle ID (a put-back app
        // is installed again, which the review notices).
        Some(l) => crate::tools_process::send(&format!("review-id {}\t{}", l.bundle_id, l.name)),
        None => page::inform(
            &tr("Nothing to review"),
            &tr(
                "Kipimo no longer has that app's leftovers on file. Find them again in Kipimo › Uninstall.",
            ),
        ),
    }
}

/// In the tools process: the review for an app that is no longer in the Trash, known by its
/// bundle ID (`<id>\t<name>`).
pub fn show_id(arg: &str) {
    let (id, name) = arg.split_once('\t').unwrap_or((arg, arg));
    let (id, name) = (id.to_string(), name.to_string());
    open_window(Source::Gone {
        bundle_id: id,
        name,
    });
}

/// Whether `bundle` is an app inside a Trash folder: the user's (`~/.Trash`) or a volume's
/// (`<volume>/.Trashes/<uid>`). The review exists for apps the user already threw away.
pub fn in_trash(bundle: &std::path::Path) -> bool {
    let Ok(real) = std::fs::canonicalize(bundle) else {
        return false;
    };
    if real.extension().is_none_or(|e| e != "app") {
        return false;
    }
    let home_trash = std::fs::canonicalize(kipimo_sys::disk::trash_dir());
    if home_trash.is_ok_and(|t| real.starts_with(t) && real != std::path::Path::new("/")) {
        return true;
    }
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() }.to_string();
    let parts: Vec<_> = real
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == ".Trashes" && w[1] == uid.as_str())
}

/// What a review is about: an app bundle in a Trash folder, or an app that is gone.
#[derive(Clone, PartialEq, Eq)]
enum Source {
    Bundle(PathBuf),
    Gone { bundle_id: String, name: String },
}

impl Source {
    fn name(&self) -> String {
        match self {
            Source::Bundle(b) => b
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Source::Gone { name, .. } => name.clone(),
        }
    }
}

/// In the tools process: find the leftovers of `bundle` and show them. Refuses anything that
/// isn't an app in a Trash folder.
pub fn show_bundle(bundle: PathBuf) {
    if !in_trash(&bundle) {
        util::log(format!(
            "not reviewing {}: not in a Trash folder",
            bundle.display()
        ));
        crate::tools_process::window_closed();
        return;
    }
    open_window(Source::Bundle(bundle));
}

fn open_window(source: Source) {
    let mtm = util::mtm();
    if let Some(w) = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .filter(|s| s.source == source)
            .map(|s| s.window.clone())
    }) {
        crate::tools_window::activate();
        w.makeKeyAndOrderFront(None);
        return;
    }
    close();
    let name = source.name();
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
    window.setTitle(&ns(&trf("Leftovers from {app}", &[("app", &name)])));
    window.setContentMinSize(NSSize::new(560.0, 420.0));
    // The app is in the Trash: nothing to stop.
    let review = Review::new(
        mtm,
        rect(0.0, 0.0, w, h),
        |picked| page::post(remove, picked),
        |_| {},
    );
    review.loading(None, &name);
    window.setContentView(Some(&review.view));
    let this = Delegate::alloc(mtm).set_ivars(());
    let delegate: Retained<Delegate> = unsafe { msg_send![super(this), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.center();
    let cancel = Arc::new(AtomicBool::new(false));
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            window: window.clone(),
            _delegate: delegate,
            review,
            source: source.clone(),
            cancel: cancel.clone(),
        })
    });
    crate::tools_window::activate();
    window.makeKeyAndOrderFront(None);
    let job = source.clone();
    page::spawn(
        Priority::User,
        move || find(&job, &cancel),
        move |r| page::post(found, (source, r.and_then(|r| r))),
    );
}

fn find(source: &Source, cancel: &AtomicBool) -> Result<LeftoverReport, String> {
    let env = Env::current_user().map_err(|e| crate::engine_text::text(&e.to_string()))?;
    match source {
        Source::Bundle(b) => kipimo_clean::find_leftovers_for_bundle(&env, b, cancel)
            .ok_or_else(|| tr("That app has no bundle identifier.")),
        Source::Gone { bundle_id, name } => {
            let index = AppIndex::build(&env, cancel);
            if index
                .apps
                .iter()
                .any(|a| a.bundle_id.as_deref() == Some(bundle_id.as_str()))
            {
                insights::leftovers_reviewed(bundle_id);
                return Err(trf(
                    "{app} is installed again, so its files aren't leftovers.",
                    &[("app", name)],
                ));
            }
            let target = AppTarget::from_bundle_id(bundle_id, Some(name));
            Ok(kipimo_clean::find_leftovers(&env, &target, &index, cancel))
        }
    }
}

fn found((source, r): (Source, Result<LeftoverReport, String>)) {
    STATE.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref().filter(|s| s.source == source) else {
            return;
        };
        match r {
            Ok(report) if report.leftovers.is_empty() => {
                // Whatever was found earlier is gone now: clear the recommendation too.
                insights::leftovers_reviewed(&report.target.bundle_id);
                s.review.clear(&tr("It didn't leave anything behind."))
            }
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
    let Some((report, cancel)) = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|s| Some((s.review.report()?, s.cancel.clone())))
    }) else {
        return;
    };
    let bytes: u64 = picked.iter().map(|l| l.bytes).sum();
    let n = picked.len();
    if !page::confirm(
        &fill(
            &trn(
                n as u64,
                "Move {n} file left by {app} to the Trash?",
                "Move {n} files left by {app} to the Trash?",
            ),
            &[(
                "app",
                report
                    .target
                    .names
                    .first()
                    .unwrap_or(&report.target.bundle_id),
            )],
        ),
        &trf(
            "{size} in all. You can put them back from the Trash in Finder.",
            &[("size", &format::bytes(bytes))],
        ),
        &tr("Move to Trash"),
    ) {
        return;
    }
    STATE.with(|s| {
        if let Some(s) = s.borrow().as_ref() {
            s.review.busy(&tr("Moving to the Trash…"));
        }
    });
    let id = report.target.bundle_id.clone();
    page::spawn(
        Priority::User,
        move || {
            let _moving = crate::trash_guard::moving();
            let env = Env::current_user().map_err(|e| crate::engine_text::text(&e.to_string()))?;
            crate::review::unload_agents(&picked);
            Ok::<_, String>(kipimo_clean::remove_leftovers(
                &env,
                &report.target,
                &picked,
                &mut |_, _| {},
                &cancel,
            ))
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
                s.review.results(
                    None,
                    results,
                    &trn(
                        n as u64,
                        "{n} item is in the Trash. Put Back in Finder restores it.",
                        "{n} items are in the Trash. Put Back in Finder restores them.",
                    ),
                );
            }
            Err(e) => s.review.clear(&e),
        }
    });
    insights::leftovers_reviewed(&bundle_id);
}

#[cfg(test)]
mod tests {
    use super::in_trash;

    #[test]
    fn only_apps_in_a_trash_folder_are_reviewed() {
        let base = std::env::temp_dir().join(format!("kipimo-review-{}", std::process::id()));
        let uid = unsafe { libc::getuid() };
        let in_volume_trash = base.join(format!("Vol/.Trashes/{uid}/Gone.app"));
        let other_user = base.join("Vol/.Trashes/0/Gone.app");
        let installed = base.join("Applications/Kept.app");
        let not_app = base.join(format!("Vol/.Trashes/{uid}/notes.txt"));
        for d in [&in_volume_trash, &other_user, &installed] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&not_app, b"x").unwrap();
        assert!(in_trash(&in_volume_trash));
        assert!(!in_trash(&other_user));
        assert!(!in_trash(&installed));
        assert!(!in_trash(&not_app));
        assert!(!in_trash(&base.join("missing.app")));
        assert!(!in_trash(std::path::Path::new("/Applications/Safari.app")));
        let _ = std::fs::remove_dir_all(&base);
    }
}
