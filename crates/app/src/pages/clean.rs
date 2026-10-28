//! Clean up (tool A): junk categories with size, risk and default tick; expand
//! a category to its items; "Clean" moves the ticked items to the Trash and
//! lists what happened to each. The Trash is reported only: "Empty Trash…"
//! opens it in Finder, which asks before erasing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kipimo_clean::{
    CategoryAction, CategoryId, CategoryInfo, CategoryReport, CleanProgress, CleanReport, Env,
    JunkItem, JunkReport, Risk, ScanEvent, ScanOptions,
};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSView};
use objc2_foundation::NSRect;

use crate::engine_text;
use crate::i18n::{tr, trf, trn};
use crate::i18n_text::fill;
use crate::list::{Badge, Check, Icon, List, ListEvent, Row};
use crate::page::{self, Footer, Header, MARGIN, Priority, post};
use crate::tools_window::ToolPage;
use crate::ui::rect;
use crate::widgets::{Banner, Targets, Tint};
use crate::{format, insights};

pub struct Page;

impl ToolPage for Page {
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
        build(mtm, frame)
    }

    fn shown(&mut self) {
        let start = STATE.with(|s| s.borrow().as_ref().is_some_and(|s| !s.started));
        if start {
            post(handle, Msg::Scan);
        }
    }

    fn closed(&mut self) {
        if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel.store(true, Ordering::Relaxed);
        }
    }
}

enum Msg {
    Scan,
    Event(u64, ScanEvent),
    Scanned(u64, Result<JunkReport, String>),
    List(ListEvent),
    Clean,
    Cleaning(u64, CleanProgress),
    Cleaned(u64, Result<CleanReport, String>),
    EmptyTrash,
    FullDiskAccess,
}

#[derive(PartialEq)]
enum Phase {
    Idle,
    Scanning,
    Cleaning,
    Results,
}

struct State {
    ticket: u64,
    started: bool,
    phase: Phase,
    cancel: Arc<AtomicBool>,
    header: Header,
    footer: Footer,
    list: List,
    banner: Banner,
    scan_button: Retained<NSButton>,
    clean_button: Retained<NSButton>,
    trash_button: Retained<NSButton>,
    _targets: Targets,
    body: NSRect,
    infos: Vec<CategoryInfo>,
    reports: HashMap<CategoryId, CategoryReport>,
    expanded: HashSet<CategoryId>,
    ticked: HashSet<PathBuf>,
    /// Not scanned: reading them would make macOS ask for access to another app's data.
    excluded: Vec<CategoryId>,
    results: Option<CleanReport>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const ITEM: usize = 1;
const SKIP: usize = 1 << 19;

fn cat_key(ci: usize) -> usize {
    ci << 20
}

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let mut header = Header::new(
        mtm,
        &root,
        w,
        &tr("Clean up"),
        &tr("Caches, logs and developer leftovers. Everything you clean goes to the Trash."),
    );
    let scan_button = header.button(mtm, &root, &tr("Scan Again"), &mut targets, || {
        post(handle, Msg::Scan)
    });
    let fda = tr("Open Full Disk Access…");
    let banner = Banner::new(
        mtm,
        rect(MARGIN, page::HEADER_H, w - 2.0 * MARGIN, 44.0),
        "lock.shield",
        Tint::Orange,
        "",
        &mut targets,
        vec![(&fda, Box::new(|| post(handle, Msg::FullDiskAccess)))],
    );
    crate::widgets::stretch_width(&banner.view);
    banner.view.setHidden(true);
    root.addSubview(&banner.view);
    let body = page::body(w, h, 0.0);
    let list = List::new(mtm, body, 40.0, |e| post(handle, Msg::List(e)));
    list.set_empty_text(&tr("Scanning…"));
    list.add_to(&root);
    let mut footer = Footer::new(mtm, &root, w, h);
    let clean_button = footer.button(mtm, &root, &tr("Clean"), &mut targets, || {
        post(handle, Msg::Clean)
    });
    let trash_button = footer.button(mtm, &root, &tr("Empty Trash…"), &mut targets, || {
        post(handle, Msg::EmptyTrash)
    });
    trash_button.setEnabled(false);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            started: false,
            phase: Phase::Idle,
            cancel: Arc::new(AtomicBool::new(false)),
            header,
            footer,
            list,
            banner,
            scan_button,
            clean_button,
            trash_button,
            _targets: targets,
            body,
            infos: Vec::new(),
            reports: HashMap::new(),
            expanded: HashSet::new(),
            ticked: HashSet::new(),
            excluded: Vec::new(),
            results: None,
        })
    });
    refresh();
    root
}

/// `kipimo://demo/clean/<category index>`: expand a category, as a click would.
#[cfg(dev)]
pub fn demo(arg: &str) {
    // `run`: clean what's ticked without the dialog, to check Put Back end to end. Only on a
    // synthetic Mac (a fake home on a disk image), never on the real one.
    if arg == "run" {
        if kipimo_clean::fake_home().is_some() {
            clean_with(false);
        } else {
            crate::util::log("demo/clean/run needs KIPIMO_FAKE_HOME");
        }
    } else if let Ok(i) = arg.parse::<usize>() {
        post(handle, Msg::List(ListEvent::Disclose(cat_key(i))));
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn handle(msg: Msg) {
    match msg {
        Msg::Scan => scan(),
        Msg::Event(ticket, e) => {
            // Progress only updates the status line; categories rebuild the list.
            let rows_changed = with(|st| {
                if st.ticket != ticket {
                    return false;
                }
                match e {
                    ScanEvent::Started { categories } => {
                        st.infos = categories;
                        true
                    }
                    ScanEvent::Progress { bytes, files } => {
                        st.header.busy(
                            true,
                            &fill(
                                &trn(
                                    files,
                                    "Scanning… {size} in {n} file",
                                    "Scanning… {size} in {n} files",
                                ),
                                &[("size", &format::bytes(bytes))],
                            ),
                        );
                        false
                    }
                    ScanEvent::Category { report } => {
                        for item in report.items.iter().filter(|i| i.default_selected) {
                            st.ticked.insert(item.path.clone());
                        }
                        st.reports.insert(report.info.id, report);
                        true
                    }
                }
            })
            .unwrap_or(false);
            if rows_changed {
                refresh();
            }
        }
        Msg::Scanned(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.phase = Phase::Idle;
                match r {
                    Ok(report) => {
                        st.header.busy(
                            false,
                            &trf(
                                "Found {size} in {seconds} s",
                                &[
                                    ("size", &format::bytes(report.total_bytes)),
                                    (
                                        "seconds",
                                        &format::decimal(report.elapsed_ms as f64 / 1000.0, 1),
                                    ),
                                ],
                            ),
                        );
                        insights::junk_scanned(report.selected_bytes, report.total_bytes);
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh();
        }
        Msg::List(e) => list_event(e),
        Msg::Clean => clean(),
        Msg::Cleaning(ticket, p) => {
            with(|st| {
                if st.ticket == ticket {
                    st.header.busy(
                        true,
                        &trf(
                            "Moving {done} of {total} to the Trash…",
                            &[
                                ("done", &format::count(p.done as u64 + 1)),
                                ("total", &format::count(p.total as u64)),
                            ],
                        ),
                    );
                }
            });
        }
        Msg::Cleaned(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                match r {
                    Ok(report) => {
                        st.header.busy(
                            false,
                            &trf(
                                "Freed {size}",
                                &[("size", &format::bytes(report.moved_bytes))],
                            ),
                        );
                        // What's left, so the recommendation stops offering what just went.
                        let moved: HashSet<&PathBuf> = report
                            .results
                            .iter()
                            .filter(|r| r.moved())
                            .map(|r| &r.path)
                            .collect();
                        let (mut selected, mut total) = (0, 0);
                        for c in st.reports.values() {
                            for i in c.items.iter().filter(|i| !moved.contains(&i.path)) {
                                total += i.bytes;
                                if i.default_selected
                                    && c.info.action == CategoryAction::MoveToTrash
                                {
                                    selected += i.bytes;
                                }
                            }
                        }
                        insights::junk_scanned(selected, total);
                        st.results = Some(report);
                        st.phase = Phase::Results;
                    }
                    Err(e) => {
                        st.header.busy(false, &e);
                        st.phase = Phase::Idle;
                    }
                }
            });
            refresh();
        }
        Msg::EmptyTrash => {
            // Finder empties it, with its own confirmation. Kipimo only opens the window.
            crate::actions::run(crate::actions::Action::OpenTrash);
            page::inform(
                &tr("Empty the Trash in Finder"),
                &tr(
                    "Kipimo opened the Trash. Click Empty at the top right of the window; Finder asks before it erases anything.",
                ),
            );
        }
        Msg::FullDiskAccess => page::open_full_disk_access(),
    }
}

fn scan() {
    let fda = page::full_disk_access();
    let Some((ticket, cancel, opts)) = with(|st| {
        if matches!(st.phase, Phase::Scanning | Phase::Cleaning) {
            return None;
        }
        st.cancel.store(true, Ordering::Relaxed);
        st.cancel = Arc::new(AtomicBool::new(false));
        st.ticket = super::next_generation();
        st.started = true;
        st.phase = Phase::Scanning;
        st.infos.clear();
        st.reports.clear();
        st.ticked.clear();
        st.results = None;
        let mut opts = ScanOptions::default();
        // Mail keeps its downloads in its own container; without Full Disk Access,
        // reading there makes macOS ask about "data from other apps".
        st.excluded = if fda == Some(false) {
            vec![CategoryId::MailDownloads]
        } else {
            Vec::new()
        };
        opts.categories.retain(|c| !st.excluded.contains(c));
        st.header.busy(true, &tr("Scanning…"));
        Some((st.ticket, st.cancel.clone(), opts))
    })
    .flatten() else {
        return;
    };
    refresh();
    page::spawn(
        Priority::User,
        move || {
            let env = Env::current_user().map_err(|e| engine_text::text(&e.to_string()))?;
            Ok::<_, String>(kipimo_clean::scan_junk(
                &env,
                &opts,
                &mut |e| post(handle, Msg::Event(ticket, e)),
                &cancel,
            ))
        },
        move |r| post(handle, Msg::Scanned(ticket, r.and_then(|r| r))),
    );
}

fn selected_items(st: &State) -> Vec<JunkItem> {
    st.reports
        .values()
        .filter(|r| r.info.action == CategoryAction::MoveToTrash)
        .flat_map(|r| r.items.iter())
        .filter(|i| st.ticked.contains(&i.path))
        .cloned()
        .collect()
}

fn clean() {
    clean_with(true);
}

/// `confirm` is false only for the development demo on a synthetic Mac (see `demo`).
fn clean_with(confirm: bool) {
    let Some((items, total)) = with(|st| {
        if st.phase != Phase::Idle {
            return None;
        }
        let items = selected_items(st);
        let total: u64 = items.iter().map(|i| i.bytes).sum();
        (!items.is_empty()).then_some((items, total))
    })
    .flatten() else {
        return;
    };
    let ok = !confirm
        || page::confirm(
            &trn(
                items.len() as u64,
                "Move {n} item to the Trash?",
                "Move {n} items to the Trash?",
            ),
            &trf(
                "They take {size}. You can put any of them back from the Trash in Finder.",
                &[("size", &format::bytes(total))],
            ),
            &tr("Move to Trash"),
        );
    if !ok {
        return;
    }
    let Some((ticket, cancel)) = with(|st| {
        st.phase = Phase::Cleaning;
        st.header.busy(true, &tr("Moving to the Trash…"));
        (st.ticket, st.cancel.clone())
    }) else {
        return;
    };
    refresh();
    page::spawn(
        Priority::User,
        move || {
            let _moving = crate::trash_guard::moving();
            let env = Env::current_user().map_err(|e| engine_text::text(&e.to_string()))?;
            let opts = ScanOptions::default();
            Ok::<_, String>(kipimo_clean::clean(
                &env,
                &items,
                &opts,
                &mut |p| post(handle, Msg::Cleaning(ticket, p)),
                &cancel,
            ))
        },
        move |r| post(handle, Msg::Cleaned(ticket, r.and_then(|r| r))),
    );
}

fn list_event(e: ListEvent) {
    with(|st| {
        if st.phase != Phase::Idle {
            return;
        }
        let (k, toggle) = match e {
            ListEvent::Toggle(k) => (k, true),
            ListEvent::Disclose(k) | ListEvent::DoubleClick(k) => (k, false),
            ListEvent::Select(_) => return,
        };
        let ci = k >> 20;
        let sub = k & ((1 << 20) - 1);
        let Some(id) = st.infos.get(ci).map(|i| i.id) else {
            return;
        };
        let Some(report) = st.reports.get(&id) else {
            return;
        };
        if toggle {
            if sub == 0 {
                let all = report.items.iter().all(|i| st.ticked.contains(&i.path));
                for i in &report.items {
                    if all {
                        st.ticked.remove(&i.path);
                    } else {
                        st.ticked.insert(i.path.clone());
                    }
                }
            } else if let Some(item) = (sub < SKIP).then(|| report.items.get(sub - ITEM)).flatten()
                && !st.ticked.remove(&item.path)
            {
                st.ticked.insert(item.path.clone());
            }
        } else if sub == 0 && !st.expanded.remove(&id) {
            st.expanded.insert(id);
        }
    });
    refresh();
}

fn symbol(id: CategoryId) -> &'static str {
    match id {
        CategoryId::AppCaches => "shippingbox",
        CategoryId::SystemCaches => "gearshape.2",
        CategoryId::Logs => "doc.text",
        CategoryId::XcodeDerivedData => "hammer",
        CategoryId::XcodeArchives => "archivebox",
        CategoryId::XcodeDeviceSupport => "iphone",
        CategoryId::XcodeSimulators => "ipad.and.iphone",
        CategoryId::DeveloperCaches => "terminal",
        CategoryId::MailDownloads => "envelope",
        CategoryId::IosBackups => "externaldrive",
        CategoryId::Trash => "trash",
    }
}

fn risk_badge(r: Risk) -> Badge {
    match r {
        Risk::Low => Badge::new(tr("Low risk"), Tint::Secondary),
        Risk::Medium => Badge::new(tr("Medium risk"), Tint::Orange),
        Risk::High => Badge::new(tr("High risk"), Tint::Red),
    }
}

fn rows(st: &State) -> Vec<Row> {
    let mut out = Vec::new();
    let infos: Vec<CategoryInfo> = if st.infos.is_empty() {
        CategoryId::ALL.iter().map(|c| c.info()).collect()
    } else {
        st.infos.clone()
    };
    for (ci, info) in infos.iter().enumerate() {
        let report = st.reports.get(&info.id);
        let mut row = Row::new(cat_key(ci), engine_text::category_title(info.id));
        row.icon = Some(Icon::Symbol(symbol(info.id), Tint::Secondary));
        row.disclosure_space = true;
        let trash = info.action == CategoryAction::EmptyTrashInFinder;
        if !trash {
            row.badges.push(risk_badge(info.risk));
        }
        if st.excluded.contains(&info.id) {
            row.subtitle = tr("Needs Full Disk Access (Mail keeps these in its own container)");
            row.dim = true;
            row.value = "–".into();
            out.push(row);
            continue;
        }
        let Some(report) = report else {
            row.subtitle = if st.phase == Phase::Scanning {
                tr("Scanning…")
            } else {
                engine_text::category_summary(info.id)
            };
            row.value = "–".into();
            row.dim = st.phase != Phase::Scanning;
            out.push(row);
            continue;
        };
        row.value = format::bytes(report.total_bytes);
        if report.root_unreadable {
            row.subtitle = tr("Kipimo can't read this without Full Disk Access");
            row.dim = true;
            row.value = "–".into();
        } else if trash {
            row.subtitle = if report.total_bytes > 0 {
                tr("Empty it in Finder, which asks before erasing")
            } else {
                tr("Empty")
            };
        } else {
            let mut sub = engine_text::category_summary(info.id);
            if !report.skipped.is_empty() {
                sub = trf(
                    "{n} kept · {summary}",
                    &[
                        ("n", &format::count(report.skipped.len() as u64)),
                        ("summary", &sub),
                    ],
                );
            }
            row.subtitle = sub;
            if report.total_bytes == 0 && report.items.is_empty() {
                row.value = tr("Nothing");
                row.value_tint = Tint::Tertiary;
            }
        }
        let has_children = !report.items.is_empty() || !report.skipped.is_empty();
        let open = st.expanded.contains(&info.id);
        if has_children && !trash {
            row.disclosure = Some(open);
        }
        if !trash {
            let n = report
                .items
                .iter()
                .filter(|i| st.ticked.contains(&i.path))
                .count();
            row.check = Some(if n == 0 {
                Check::Off
            } else if n == report.items.len() {
                Check::On
            } else {
                Check::Mixed
            });
            row.check_enabled = !report.items.is_empty() && st.phase == Phase::Idle;
        }
        out.push(row);
        if !open || trash {
            continue;
        }
        for (ii, item) in report.items.iter().enumerate().take(SKIP - 2) {
            let mut r = Row::new(
                cat_key(ci) | (ii + ITEM),
                engine_text::junk_label(info.id, &item.label),
            );
            r.indent = 1;
            r.disclosure_space = true;
            r.check = Some(Check::from(st.ticked.contains(&item.path)));
            r.check_enabled = st.phase == Phase::Idle;
            r.subtitle = match &item.note {
                Some(n) => format!("{} · {}", page::tilde(&item.path), engine_text::text(n)),
                None => page::tilde(&item.path),
            };
            r.value = format::bytes(item.bytes);
            out.push(r);
        }
        for (si, s) in report.skipped.iter().enumerate() {
            let mut r = Row::new(
                cat_key(ci) | (SKIP + si),
                engine_text::junk_label(info.id, &s.label),
            );
            r.indent = 1;
            r.disclosure_space = true;
            r.dim = true;
            r.subtitle = format!("{} · {}", super::skip_text(&s.reason), page::tilde(&s.path));
            r.value = s.bytes.map(format::bytes).unwrap_or_default();
            out.push(r);
        }
    }
    out
}

fn refresh() {
    with(|st| {
        let unreadable: Vec<String> = st
            .reports
            .values()
            .filter(|r| r.root_unreadable)
            .map(|r| engine_text::category_title(r.info.id))
            .chain(st.excluded.iter().map(|c| engine_text::category_title(*c)))
            .collect();
        if unreadable.is_empty() {
            st.banner.view.setHidden(true);
            st.list.scroll.setFrame(st.body);
        } else {
            st.banner.set_message(&trf(
                "{categories} need Full Disk Access. Allow it in System Settings and scan again.",
                &[("categories", &unreadable.join(", "))],
            ));
            st.banner.view.setHidden(false);
            let mut f = st.body;
            f.origin.y += 52.0;
            f.size.height -= 52.0;
            st.list.scroll.setFrame(f);
        }
        if st.phase == Phase::Results {
            let r = st
                .results
                .as_ref()
                .map(|r| r.results.clone())
                .unwrap_or_default();
            st.list.set_rows(super::result_rows(&r, 1));
            let freed = st.results.as_ref().map(|r| r.moved_bytes).unwrap_or(0);
            crate::widgets::set_text(
                &st.footer.text,
                &trf(
                    "Freed {size}. Put Back in Finder restores any item.",
                    &[("size", &format::bytes(freed))],
                ),
            );
            st.clean_button.setEnabled(false);
        } else {
            st.list.set_rows(rows(st));
            let items = selected_items(st);
            let total: u64 = items.iter().map(|i| i.bytes).sum();
            crate::widgets::set_text(
                &st.footer.text,
                &if items.is_empty() {
                    tr("Nothing selected")
                } else {
                    format!(
                        "{} · {}",
                        trn(
                            items.len() as u64,
                            "{n} item selected",
                            "{n} items selected"
                        ),
                        format::bytes(total)
                    )
                },
            );
            st.clean_button
                .setEnabled(!items.is_empty() && st.phase == Phase::Idle);
        }
        let trash = st
            .reports
            .get(&CategoryId::Trash)
            .map(|r| r.total_bytes)
            .unwrap_or(0);
        st.trash_button
            .setEnabled(trash > 0 && st.phase == Phase::Idle);
        st.scan_button
            .setEnabled(!matches!(st.phase, Phase::Scanning | Phase::Cleaning));
    });
}
