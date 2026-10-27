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

use crate::engine_text;
use crate::i18n::{tr, trf, trn};
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
        &tr("Updates"),
        &tr("Newer versions from app feeds and the App Store. Nothing gets installed."),
    );
    let check_button = header.button(mtm, &root, &tr("Check Now"), &mut targets, || {
        post(handle, Msg::Check(true))
    });
    let list = List::new(mtm, page::body(w, h, 0.0), 42.0, |e| {
        post(handle, Msg::List(e))
    });
    list.set_empty_text(&tr("Checking for updates…"));
    list.add_to(&root);
    let mut footer = Footer::new(mtm, &root, w, h);
    let updater = footer.button(mtm, &root, &tr("Open App Store"), &mut targets, || {
        post(handle, Msg::Open(Link::Updater))
    });
    let download = footer.button(mtm, &root, &tr("Download Page"), &mut targets, || {
        post(handle, Msg::Open(Link::Download))
    });
    let notes = footer.button(mtm, &root, &tr("Release Notes"), &mut targets, || {
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
                st.header.busy(
                    true,
                    &trf(
                        "Checked {done} of {total}",
                        &[
                            ("done", &crate::format::count(done as u64)),
                            ("total", &crate::format::count(total as u64)),
                        ],
                    ),
                );
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
                            tr("from cache")
                        } else {
                            trf(
                                "{seconds} s",
                                &[(
                                    "seconds",
                                    &crate::format::decimal(rep.duration_ms as f64 / 1000.0, 1),
                                )],
                            )
                        };
                        st.header.busy(
                            false,
                            &[
                                trn(rep.apps.len() as u64, "{n} app", "{n} apps"),
                                trn(c.update_available as u64, "{n} update", "{n} updates"),
                                when,
                            ]
                            .join(" · "),
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
        st.header.busy(true, &tr("Finding apps…"));
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

/// The group an app is listed in: its order, and its header's title.
fn group(u: &AppUpdate) -> usize {
    match &u.status {
        Status::UpdateAvailable => 0,
        Status::RequiresNewerMacos { .. } => 1,
        Status::UpToDate => 2,
        Status::CheckFailed { .. } => 3,
        Status::UnknownSource => 4,
    }
}

fn group_title(g: usize) -> String {
    match g {
        0 => tr("Updates available"),
        1 => tr("Needs a newer macOS"),
        2 => tr("Up to date"),
        3 => tr("Couldn't check"),
        _ => tr("No update feed Kipimo can read"),
    }
}

fn refresh() {
    with(|st| {
        let mut order: Vec<usize> = (0..st.apps.len()).collect();
        order.sort_by_key(|&i| (group(&st.apps[i]), st.apps[i].app.name.to_lowercase()));
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut last = usize::MAX;
        for i in order {
            let u = &st.apps[i];
            let g = group(u);
            if g != last {
                let n = st.apps.iter().filter(|a| group(a) == g).count();
                let mut h = Row::header(keys.len(), group_title(g));
                h.value = crate::format::count(n as u64);
                rows.push(h);
                keys.push(None);
                last = g;
            }
            let mut r = Row::new(keys.len(), &u.app.name);
            r.icon = Some(Icon::File(u.app.path.clone()));
            let installed = u.app.display_version().unwrap_or("?").to_string();
            let source = engine_text::source_label(u.app.source.label());
            r.subtitle = match (&u.status, u.latest_version()) {
                (Status::UpdateAvailable, Some(l)) => format!("{installed} → {l} · {source}"),
                (Status::RequiresNewerMacos { minimum }, Some(l)) => trf(
                    "{installed} → {latest} needs macOS {minimum}",
                    &[
                        ("installed", &installed),
                        ("latest", l),
                        ("minimum", minimum),
                    ],
                ),
                (Status::CheckFailed { reason }, _) => {
                    format!("{installed} · {}", engine_text::text(reason))
                }
                (Status::UnknownSource, _) => match &u.hint {
                    Some(h) => format!("{installed} · {}", engine_text::text(&h.text)),
                    None => installed.clone(),
                },
                _ => format!("{installed} · {source}"),
            };
            match &u.status {
                Status::UpdateAvailable => {
                    r.badges.push(Badge::new(tr("Update"), Tint::Blue));
                    if u.critical {
                        r.badges.push(Badge::new(tr("Critical"), Tint::Red));
                    }
                }
                Status::RequiresNewerMacos { .. } => r
                    .badges
                    .push(Badge::new(tr("Newer macOS"), Tint::Secondary)),
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
                u.hint
                    .as_ref()
                    .map(|h| engine_text::text(&h.text))
                    .unwrap_or_default(),
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
        st.download.setTitle(&crate::util::ns(&if store {
            tr("Open in App Store")
        } else {
            tr("Download Page")
        }));
        st.updater.setTitle(&crate::util::ns(&if upd
            .as_deref()
            .is_some_and(|u| u.starts_with("macappstore:"))
        {
            tr("App Store Updates")
        } else {
            tr("Open Updater")
        }));
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
