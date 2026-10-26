//! Updates (tool F): each app's installed and latest version from its own
//! update feed or the App Store, with release notes, download and App Store
//! links and a hint when the app's own updater is the way to install. Kipimo
//! never downloads or installs anything. Feeds are cached for a day; "Check
//! Now" reads them again.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kipimo_updates::{AppUpdate, CheckOptions, DiscoverOptions, Status, UpdateReport};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSView};
use objc2_foundation::NSRect;

use crate::insights;
use crate::list::{Badge, Icon, List, ListEvent, Row};
use crate::page::{self, Footer, Header, Priority, post};
use crate::tools_window::ToolPage;
use crate::widgets::{self, Targets, Tint};

pub struct Page;

impl ToolPage for Page {
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
        build(mtm, frame)
    }

    fn shown(&mut self) {
        let start = STATE.with(|s| s.borrow().as_ref().is_some_and(|s| !s.started));
        if start {
            post(handle, Msg::Check(false));
        }
    }

    fn closed(&mut self) {
        if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel.store(true, Ordering::Relaxed);
        }
    }
}

enum Msg {
    Check(bool),
    Progress(u64, usize, usize, Box<AppUpdate>),
    Checked(u64, Result<UpdateReport, String>),
    List(ListEvent),
    Open(Link),
}

#[derive(Clone, Copy)]
enum Link {
    Notes,
    Download,
    Updater,
}

struct State {
    ticket: u64,
    started: bool,
    checking: bool,
    cancel: Arc<AtomicBool>,
    header: Header,
    footer: Footer,
    list: List,
    check_button: Retained<NSButton>,
    notes: Retained<NSButton>,
    download: Retained<NSButton>,
    updater: Retained<NSButton>,
    _targets: Targets,
    apps: Vec<AppUpdate>,
    keys: Vec<Option<usize>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let mut header = Header::new(
        mtm,
        &root,
        w,
        "Updates",
        "Newer versions from app feeds and the App Store. Nothing gets installed.",
    );
    let check_button = header.button(mtm, &root, "Check Now", &mut targets, || {
        post(handle, Msg::Check(true))
    });
    let list = List::new(mtm, page::body(w, h, 0.0), 42.0, |e| {
        post(handle, Msg::List(e))
    });
    list.set_empty_text("Checking for updates…");
    list.add_to(&root);
    let mut footer = Footer::new(mtm, &root, w, h);
    let updater = footer.button(mtm, &root, "Open App Store", &mut targets, || {
        post(handle, Msg::Open(Link::Updater))
    });
    let download = footer.button(mtm, &root, "Download Page", &mut targets, || {
        post(handle, Msg::Open(Link::Download))
    });
    let notes = footer.button(mtm, &root, "Release Notes", &mut targets, || {
        post(handle, Msg::Open(Link::Notes))
    });
    for b in [&updater, &download, &notes] {
        b.setEnabled(false);
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            started: false,
            checking: false,
            cancel: Arc::new(AtomicBool::new(false)),
            header,
            footer,
            list,
            check_button,
            notes,
            download,
            updater,
            _targets: targets,
            apps: Vec::new(),
            keys: Vec::new(),
        })
    });
    root
}

/// `kipimo://demo/updates/<row>`: select a row, as a click would.
#[cfg(dev)]
pub fn demo(arg: &str) {
    if let Ok(k) = arg.parse::<usize>() {
        with(|st| st.list.select_key(k));
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

/// Check options shared with the background check: cached for a day unless forced.
pub fn options(force: bool) -> CheckOptions {
    CheckOptions {
        force,
        cache_path: kipimo_updates::default_cache_path(),
        ..CheckOptions::default()
    }
}

/// Discover options. Without Full Disk Access, skip reading other apps' preferences
/// inside their containers (macOS would ask about "data from other apps").
pub fn discover_options() -> DiscoverOptions {
    let mut d = DiscoverOptions::default();
    if page::full_disk_access() == Some(false) {
        d.home = None;
    }
    d
}

fn handle(msg: Msg) {
    match msg {
        Msg::Check(force) => check(force),
        Msg::Progress(ticket, done, total, update) => {
            let changed = with(|st| {
                if st.ticket != ticket {
                    return false;
                }
                st.header.busy(true, &format!("Checked {done} of {total}"));
                st.apps.retain(|a| a.app.path != update.app.path);
                st.apps.push(*update);
                // Refresh the list every few results while checking.
                done % 5 == 0 || done == total
            })
            .unwrap_or(false);
            if changed {
                refresh();
            }
        }
        Msg::Checked(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.checking = false;
                st.check_button.setEnabled(true);
                match r {
                    Ok(rep) => {
                        let c = rep.counts();
                        // Nothing was fetched: say where the answers came from, not "0.0 s".
                        let when = if rep.requests == 0 {
                            "from cache".to_string()
                        } else {
                            format!("{:.1} s", rep.duration_ms as f64 / 1000.0)
                        };
                        st.header.busy(
                            false,
                            &format!(
                                "{} apps · {} update{} · {when}",
                                rep.apps.len(),
                                c.update_available,
                                if c.update_available == 1 { "" } else { "s" },
                            ),
                        );
                        insights::updates_checked(
                            rep.updates().map(|u| u.app.name.clone()).collect(),
                        );
                        st.apps = rep.apps;
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh();
        }
        Msg::List(ListEvent::Select(_)) => selection_changed(),
        Msg::List(ListEvent::DoubleClick(_)) => open(Link::Download),
        Msg::List(_) => {}
        Msg::Open(l) => open(l),
    }
}

fn check(force: bool) {
    let Some((ticket, cancel)) = with(|st| {
        if st.checking {
            return None;
        }
        st.cancel.store(true, Ordering::Relaxed);
        st.cancel = Arc::new(AtomicBool::new(false));
        st.ticket = super::next_generation();
        st.started = true;
        st.checking = true;
        st.check_button.setEnabled(false);
        st.header.busy(true, "Finding apps…");
        Some((st.ticket, st.cancel.clone()))
    })
    .flatten() else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            kipimo_updates::scan(
                &discover_options(),
                &options(force),
                |p| {
                    post(
                        handle,
                        Msg::Progress(ticket, p.done, p.total, Box::new(p.update.clone())),
                    )
                },
                &cancel,
            )
        },
        move |r| post(handle, Msg::Checked(ticket, r)),
    );
}

/// "2026-10-22" from ISO 8601 ("2026-10-22T09:00:00Z") or RFC 2822 ("Thu, 22 Oct 2026 09:00:00 +0000").
pub fn short_date(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        return s[..10].to_string();
    }
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let words: Vec<&str> = s.split([' ', ',']).filter(|w| !w.is_empty()).collect();
    for w in words.windows(3) {
        let (d, m, y) = (w[0], w[1].to_lowercase(), w[2]);
        if let (Ok(d), Some(mi), Ok(y)) = (
            d.parse::<u32>(),
            MONTHS.iter().position(|x| m.starts_with(x)),
            y.parse::<u32>(),
        ) {
            return format!("{y:04}-{:02}-{d:02}", mi + 1);
        }
    }
    s.chars().take(10).collect()
}

fn group(u: &AppUpdate) -> (usize, &'static str) {
    match &u.status {
        Status::UpdateAvailable => (0, "Updates available"),
        Status::RequiresNewerMacos { .. } => (1, "Needs a newer macOS"),
        Status::UpToDate => (2, "Up to date"),
        Status::CheckFailed { .. } => (3, "Couldn't check"),
        Status::UnknownSource => (4, "No update feed Kipimo can read"),
    }
}

fn refresh() {
    with(|st| {
        let mut order: Vec<usize> = (0..st.apps.len()).collect();
        order.sort_by_key(|&i| (group(&st.apps[i]).0, st.apps[i].app.name.to_lowercase()));
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut last = usize::MAX;
        for i in order {
            let u = &st.apps[i];
            let (g, title) = group(u);
            if g != last {
                let n = st.apps.iter().filter(|a| group(a).0 == g).count();
                let mut h = Row::header(keys.len(), title);
                h.value = n.to_string();
                rows.push(h);
                keys.push(None);
                last = g;
            }
            let mut r = Row::new(keys.len(), &u.app.name);
            r.icon = Some(Icon::File(u.app.path.clone()));
            let installed = u.app.display_version().unwrap_or("?").to_string();
            r.subtitle = match (&u.status, u.latest_version()) {
                (Status::UpdateAvailable, Some(l)) => {
                    format!("{installed} → {l} · {}", u.app.source.label())
                }
                (Status::RequiresNewerMacos { minimum }, Some(l)) => {
                    format!("{installed} → {l} needs macOS {minimum}")
                }
                (Status::CheckFailed { reason }, _) => format!("{installed} · {reason}"),
                (Status::UnknownSource, _) => match &u.hint {
                    Some(h) => format!("{installed} · {}", h.text),
                    None => installed.clone(),
                },
                _ => format!("{installed} · {}", u.app.source.label()),
            };
            match &u.status {
                Status::UpdateAvailable => {
                    r.badges.push(Badge::new("Update", Tint::Blue));
                    if u.critical {
                        r.badges.push(Badge::new("Critical", Tint::Red));
                    }
                }
                Status::RequiresNewerMacos { .. } => {
                    r.badges.push(Badge::new("Newer macOS", Tint::Secondary))
                }
                Status::CheckFailed { .. } => r.dim = true,
                Status::UnknownSource => r.dim = true,
                Status::UpToDate => {}
            }
            r.value = u
                .latest
                .as_ref()
                .and_then(|l| l.published.as_deref())
                .map(short_date)
                .unwrap_or_default();
            r.value_tint = Tint::Secondary;
            rows.push(r);
            keys.push(Some(i));
        }
        st.keys = keys;
        st.list.set_rows(rows);
    });
    selection_changed();
}

fn selected(st: &State) -> Option<&AppUpdate> {
    let k = st.list.selected()?;
    st.apps.get((*st.keys.get(k)?)?)
}

fn selection_changed() {
    with(|st| {
        let (notes, dl, upd, hint) = match selected(st) {
            Some(u) => (
                u.latest.as_ref().and_then(|l| l.release_notes_url.clone()),
                u.latest.as_ref().and_then(|l| l.download_url.clone()),
                u.hint.as_ref().and_then(|h| h.open_url.clone()),
                u.hint.as_ref().map(|h| h.text.clone()).unwrap_or_default(),
            ),
            None => (None, None, None, String::new()),
        };
        st.notes.setEnabled(notes.is_some());
        st.download.setEnabled(dl.is_some());
        st.updater.setEnabled(upd.is_some());
        let store = dl.as_deref().is_some_and(|d| d.starts_with("macappstore:"))
            || upd
                .as_deref()
                .is_some_and(|d| d.starts_with("macappstore:"));
        st.download.setTitle(&crate::util::ns(if store {
            "Open in App Store"
        } else {
            "Download Page"
        }));
        st.updater.setTitle(&crate::util::ns(
            if upd
                .as_deref()
                .is_some_and(|u| u.starts_with("macappstore:"))
            {
                "App Store Updates"
            } else {
                "Open Updater"
            },
        ));
        widgets::set_text(&st.footer.text, &hint);
    });
}

fn open(l: Link) {
    let url = with(|st| {
        let u = selected(st)?;
        match l {
            Link::Notes => u.latest.as_ref()?.release_notes_url.clone(),
            Link::Download => u.latest.as_ref()?.download_url.clone(),
            Link::Updater => u.hint.as_ref()?.open_url.clone(),
        }
    })
    .flatten();
    if let Some(url) = url {
        crate::actions::run(crate::actions::Action::OpenUrl(url));
    }
}

#[cfg(test)]
mod tests {
    use super::short_date;

    #[test]
    fn dates() {
        assert_eq!(short_date("2026-10-22T09:00:00Z"), "2026-10-22");
        assert_eq!(short_date("Thu, 22 Oct 2026 09:41:00 +0000"), "2026-10-22");
        assert_eq!(short_date("Mon, 5 Jan 2026 1:00:00 GMT"), "2026-01-05");
        assert_eq!(short_date("someday"), "someday");
    }
}
