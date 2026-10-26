//! Uninstall (tool B): every app with its size and when it was last used.
//! Selecting one finds its leftovers and shows the review: the app and its
//! files with sizes, items over 1 GB unticked. Running apps must be quit first;
//! apps installed by a `.pkg` belong to the system, so the user drags them to
//! the Trash in Finder (which asks for a password) and Kipimo cleans up after.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kipimo_clean::{
    AppIndex, AppTarget, AppsEvent, Env, InstalledApp, ItemOutcome, Leftover, LeftoverReport,
    SkipReason, UninstallReport, UninstallStatus,
};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::NSView;
use objc2_foundation::NSRect;

use crate::format;
use crate::list::{Badge, Icon, List, ListEvent, Row};
use crate::page::{self, Header, Priority, post};
use crate::review::{Review, ReviewApp};
use crate::tools_window::ToolPage;
use crate::ui::rect;
use crate::widgets::{self, Targets, Tint};

pub struct Page;

impl ToolPage for Page {
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
        build(mtm, frame)
    }

    fn shown(&mut self) {
        let start = STATE.with(|s| s.borrow().as_ref().is_some_and(|s| !s.started));
        if start {
            post(handle, Msg::Load);
        }
    }

    fn closed(&mut self) {
        if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel.store(true, Ordering::Relaxed);
            st.find_cancel.store(true, Ordering::Relaxed);
        }
    }
}

enum Msg {
    Load,
    Apps(u64, AppsEvent),
    Listed(u64, Result<(Vec<InstalledApp>, Arc<AppIndex>), String>),
    List(ListEvent),
    Sort(usize),
    Found(u64, PathBuf, Result<LeftoverReport, String>),
    Move(Vec<Leftover>),
    Uninstalled(u64, Result<UninstallReport, String>),
}

struct State {
    ticket: u64,
    started: bool,
    cancel: Arc<AtomicBool>,
    find_cancel: Arc<AtomicBool>,
    header: Header,
    list: List,
    review: Review,
    _targets: Targets,
    apps: Vec<InstalledApp>,
    admin: HashSet<PathBuf>,
    sizes: HashMap<PathBuf, u64>,
    index: Option<Arc<AppIndex>>,
    sort: usize,
    selected: Option<PathBuf>,
    working: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const LEFT_W: f64 = 320.0;
const SORTS: [&str; 3] = ["Name", "Size", "Last used"];

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let header = Header::new(
        mtm,
        &root,
        w,
        "Uninstall",
        "Remove an app and the files it left in your Library.",
    );
    let sort = widgets::popup(mtm, &SORTS.map(String::from), 0, &mut targets, |i| {
        post(handle, Msg::Sort(i))
    });
    sort.setFrame(rect(w - page::MARGIN - 130.0, 12.0, 130.0, 25.0));
    widgets::pin_right(&sort);
    root.addSubview(&sort);
    let top = page::HEADER_H;
    let list = List::new(mtm, rect(0.0, top, LEFT_W, h - top), 44.0, |e| {
        post(handle, Msg::List(e))
    });
    list.scroll
        .setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);
    list.set_empty_text("Finding apps…");
    list.add_to(&root);
    let sep = crate::ui::fill(
        mtm,
        rect(LEFT_W, top, 1.0, h - top),
        &objc2_app_kit::NSColor::separatorColor(),
        0.0,
    );
    sep.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);
    root.addSubview(&sep);
    let review = Review::new(
        mtm,
        rect(LEFT_W + 1.0, top, w - LEFT_W - 1.0, h - top),
        |picked| post(handle, Msg::Move(picked)),
    );
    root.addSubview(&review.view);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            started: false,
            cancel: Arc::new(AtomicBool::new(false)),
            find_cancel: Arc::new(AtomicBool::new(false)),
            header,
            list,
            review,
            _targets: targets,
            apps: Vec::new(),
            admin: HashSet::new(),
            sizes: HashMap::new(),
            index: None,
            sort: 0,
            selected: None,
            working: false,
        })
    });
    root
}

/// `kipimo://demo/uninstall/<app name>`: select an app, as a click would (screenshots, checks).
pub fn demo(name: &str) {
    with(|st| {
        if let Some(i) = st
            .apps
            .iter()
            .position(|a| a.name.eq_ignore_ascii_case(name))
        {
            st.list.select_key(i);
        }
    });
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn handle(msg: Msg) {
    match msg {
        Msg::Load => load(),
        Msg::Apps(ticket, e) => {
            let changed = with(|st| {
                if st.ticket != ticket {
                    return false;
                }
                match e {
                    AppsEvent::Found { app } => {
                        if ReviewApp::admin_owned(&app.path) {
                            st.admin.insert(app.path.clone());
                        }
                        st.apps.push(app);
                        true
                    }
                    AppsEvent::Sized { path, bytes } => {
                        st.sizes.insert(path, bytes);
                        true
                    }
                    AppsEvent::Progress { bytes, .. } => {
                        st.header
                            .busy(true, &format!("Measuring apps… {}", format::bytes(bytes)));
                        false
                    }
                }
            })
            .unwrap_or(false);
            if changed {
                refresh_list();
            }
        }
        Msg::Listed(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                match r {
                    Ok((apps, index)) => {
                        let total: u64 = apps.iter().filter_map(|a| a.size_bytes).sum();
                        for a in &apps {
                            if let Some(b) = a.size_bytes {
                                st.sizes.insert(a.path.clone(), b);
                            }
                        }
                        st.apps = apps;
                        st.index = Some(index);
                        st.header.busy(
                            false,
                            &format!("{} apps · {}", st.apps.len(), format::bytes(total)),
                        );
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh_list();
            // An app picked before the index was ready.
            let pending = with(|st| st.selected.clone()).flatten();
            if let Some(p) = pending {
                review(p);
            }
        }
        Msg::List(ListEvent::Select(Some(k))) => {
            // Reloading the list while apps stream in re-selects the same row; ignore that.
            let p = with(|st| {
                st.apps
                    .get(k)
                    .map(|a| a.path.clone())
                    .filter(|p| st.selected.as_ref() != Some(p))
            })
            .flatten();
            if let Some(p) = p {
                review(p);
            }
        }
        Msg::List(_) => {}
        Msg::Sort(i) => {
            with(|st| st.sort = i);
            refresh_list();
        }
        Msg::Found(ticket, path, r) => {
            with(|st| {
                if st.ticket != ticket || st.selected.as_ref() != Some(&path) {
                    return;
                }
                let app = review_app(st, &path);
                match r {
                    Ok(report) => st.review.show(app, report),
                    Err(e) => st.review.clear(&e),
                }
            });
        }
        Msg::Move(picked) => uninstall(picked),
        Msg::Uninstalled(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.working = false;
                let name = st.review.app().map(|a| a.name).unwrap_or_default();
                match r {
                    Ok(report) => {
                        let note = match &report.status {
                            UninstallStatus::Done => format!(
                                "{name} and {} of its files are in the Trash. Put Back in Finder restores them.",
                                report.leftovers.iter().filter(|r| r.moved()).count()
                            ),
                            UninstallStatus::QuitFirst => {
                                format!("Quit {name} first, then try again.")
                            }
                            UninstallStatus::Refused { .. } => {
                                format!("{name} is part of macOS and stays.")
                            }
                            UninstallStatus::NotAnApp => {
                                "That isn't an app bundle any more.".into()
                            }
                            UninstallStatus::AppNotMoved if blocked_by_app_management(&report) => {
                                format!(
                                    "macOS blocked moving {name}, so its files stayed too. Allow Kipimo in System Settings › Privacy & Security › App Management, then try again."
                                )
                            }
                            UninstallStatus::AppNotMoved => {
                                format!("{name} couldn't be moved, so its files stayed too.")
                            }
                            UninstallStatus::Cancelled => "Cancelled.".into(),
                        };
                        if report.status == UninstallStatus::Done {
                            let p = report.app.path.clone();
                            st.apps.retain(|a| a.path != p);
                        }
                        if blocked_by_app_management(&report) {
                            let name = name.clone();
                            // After this state update, outside `with`.
                            crate::util::main_async(move || {
                                if page::ask(
                                    &format!("macOS blocked moving {name}"),
                                    "Moving some signed apps needs App Management permission. Turn on Kipimo in System Settings › Privacy & Security › App Management, then try again.",
                                    "Open Settings",
                                ) {
                                    page::open_app_management();
                                }
                            });
                        }
                        st.review.results(Some(report.app), report.leftovers, &note);
                    }
                    Err(e) => st.review.clear(&e),
                }
            });
            refresh_list();
        }
    }
}

fn blocked_by_app_management(r: &UninstallReport) -> bool {
    r.app.outcome
        == (ItemOutcome::Skipped {
            reason: SkipReason::AppManagement,
        })
}

fn review_app(st: &State, path: &PathBuf) -> Option<ReviewApp> {
    let a = st.apps.iter().find(|a| &a.path == path)?;
    Some(ReviewApp {
        path: a.path.clone(),
        name: a.name.clone(),
        bytes: st.sizes.get(path).copied().or(a.size_bytes).unwrap_or(0),
        running: a.running,
        protected: a.protected,
        admin_owned: st.admin.contains(path),
    })
}

fn load() {
    let Some((ticket, cancel)) = with(|st| {
        st.started = true;
        st.header.busy(true, "Finding apps…");
        (st.ticket, st.cancel.clone())
    }) else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            let env = Env::current_user().map_err(|e| e.to_string())?;
            let apps =
                kipimo_clean::list_apps(&env, &mut |e| post(handle, Msg::Apps(ticket, e)), &cancel);
            let index = Arc::new(AppIndex::build(&env, &cancel));
            Ok::<_, String>((apps, index))
        },
        move |r| post(handle, Msg::Listed(ticket, r.and_then(|r| r))),
    );
}

fn review(path: PathBuf) {
    let Some((ticket, cancel, index, app)) = with(|st| {
        if st.working {
            return None;
        }
        st.find_cancel.store(true, Ordering::Relaxed);
        st.find_cancel = Arc::new(AtomicBool::new(false));
        st.selected = Some(path.clone());
        let app = review_app(st, &path);
        let name = app.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        st.review.loading(app.clone(), &name);
        // The index (every installed app, to know what's shared) arrives with the list.
        let index = st.index.clone()?;
        Some((st.ticket, st.find_cancel.clone(), index, app))
    })
    .flatten() else {
        return;
    };
    if app.as_ref().is_some_and(|a| a.protected.is_some()) {
        with(|st| st.review.clear("Part of macOS: it can't be uninstalled."));
        return;
    }
    let p = path.clone();
    page::spawn(
        Priority::User,
        move || {
            let env = Env::current_user().map_err(|e| e.to_string())?;
            let target = AppTarget::from_bundle(&env, &p).ok_or(
                "This app has no bundle identifier, so Kipimo can't tell which files are its own.",
            )?;
            Ok::<_, String>(kipimo_clean::find_leftovers(&env, &target, &index, &cancel))
        },
        move |r| post(handle, Msg::Found(ticket, path, r.and_then(|r| r))),
    );
}

fn uninstall(picked: Vec<Leftover>) {
    let Some((ticket, app, cancel)) = with(|st| {
        let app = st.review.app()?;
        Some((st.ticket, app, st.cancel.clone()))
    })
    .flatten() else {
        return;
    };
    let bytes: u64 = app.bytes + picked.iter().map(|l| l.bytes).sum::<u64>();
    let ok = page::confirm(
        &format!(
            "Move {} and {} of its files to the Trash?",
            app.name,
            picked.len()
        ),
        &format!(
            "{} in all. You can put them back from the Trash in Finder.",
            format::bytes(bytes)
        ),
        "Move to Trash",
    );
    if !ok {
        return;
    }
    with(|st| {
        st.working = true;
        st.review.busy("Moving to the Trash…");
    });
    page::spawn(
        Priority::User,
        move || {
            let _moving = crate::trash_guard::moving();
            let env = Env::current_user().map_err(|e| e.to_string())?;
            Ok::<_, String>(kipimo_clean::uninstall(
                &env,
                &app.path,
                app.bytes,
                &picked,
                &mut |_| {},
                &cancel,
            ))
        },
        move |r| post(handle, Msg::Uninstalled(ticket, r.and_then(|r| r))),
    );
}

fn refresh_list() {
    with(|st| {
        let mut order: Vec<usize> = (0..st.apps.len()).collect();
        let size = |i: &usize| st.sizes.get(&st.apps[*i].path).copied().unwrap_or(0);
        match st.sort {
            1 => order.sort_by_key(|i| std::cmp::Reverse(size(i))),
            2 => order.sort_by_key(|i| std::cmp::Reverse(st.apps[*i].last_used_unix.unwrap_or(0))),
            _ => order.sort_by_key(|i| st.apps[*i].name.to_lowercase()),
        }
        let rows = order
            .into_iter()
            .map(|i| {
                let a = &st.apps[i];
                let mut r = Row::new(i, &a.name);
                r.icon = Some(Icon::File(a.path.clone()));
                let mut sub = Vec::new();
                if let Some(v) = &a.version {
                    sub.push(v.clone());
                }
                sub.push(match a.last_used_unix {
                    Some(t) => format!("used {}", page::ago(t)),
                    None => "never opened".into(),
                });
                r.subtitle = sub.join(" · ");
                r.value = st
                    .sizes
                    .get(&a.path)
                    .map(|b| format::bytes(*b))
                    .unwrap_or_else(|| "…".into());
                if a.running {
                    r.badges.push(Badge::new("Running", Tint::Orange));
                }
                if a.protected.is_some() {
                    r.badges.push(Badge::new("macOS", Tint::Secondary));
                    r.dim = true;
                } else if st.admin.contains(&a.path) {
                    r.badges.push(Badge::new("Admin", Tint::Secondary));
                }
                r
            })
            .collect();
        st.list.set_rows(rows);
    });
}
