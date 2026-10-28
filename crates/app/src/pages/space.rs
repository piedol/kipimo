//! Space (tool C): pick a root, scan it with live progress, and drill down
//! through folders sorted by size, each with a bar showing its share of the
//! folder above. Reveal in Finder or move a selection to the Trash. Before
//! scanning a home folder or a disk without Full Disk Access, Kipimo explains
//! that macOS would ask about each protected folder, and waits for the user.
//! The map is dropped when the page closes.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kipimo_space::{
    Entry, EntryKind, FileEntry, NodeId, NodeState, ScanOptions, ScanProgress, ScanSummary,
    ScanTarget, SpaceMap, TargetKind, TrashOutcome, TrashResult,
};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSButton, NSPopUpButton, NSSegmentedControl, NSView};
use objc2_foundation::NSRect;

use crate::engine_text;
use crate::format;
use crate::i18n::{tr, trf, trn};
use crate::i18n_text::fill;
use crate::list::{Icon, List, ListEvent, Row};
use crate::page::{self, Footer, Header, MARGIN, Priority, post};
use crate::tools_window::ToolPage;
use crate::ui::{Weight, rect};
use crate::widgets::{self, Banner, Targets, Tint};

pub struct Page;

impl ToolPage for Page {
    fn view(&mut self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
        build(mtm, frame)
    }

    fn closed(&mut self) {
        // Dropping the state drops the map (the scan thread lets go of its copy once cancelled).
        if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel.store(true, Ordering::Relaxed);
        }
    }
}

enum Msg {
    Scan,
    PickTarget(usize),
    Progress(u64, ScanProgress),
    Scanned(u64, Result<ScanSummary, String>),
    List(ListEvent),
    Crumb(u32),
    Mode(usize),
    Reveal,
    Trash,
    Trashed(u64, Vec<TrashResult>),
    FullDiskAccess,
    #[cfg(dev)]
    Folder(PathBuf),
}

/// What a row stands for.
#[derive(Clone)]
enum Item {
    Folder(NodeId, PathBuf, u64),
    File(PathBuf, u64),
    Other,
}

struct State {
    ticket: u64,
    cancel: Arc<AtomicBool>,
    header: Header,
    footer: Footer,
    picker: Retained<NSPopUpButton>,
    scan_button: Retained<NSButton>,
    trash_button: Retained<NSButton>,
    reveal_button: Retained<NSButton>,
    mode: Retained<NSSegmentedControl>,
    banner: Banner,
    crumbs: Retained<NSView>,
    crumb_targets: Targets,
    list: List,
    _targets: Targets,
    body: NSRect,
    width: f64,
    choices: Vec<ScanTarget>,
    custom: Option<PathBuf>,
    choice: usize,
    map: Option<SpaceMap>,
    current: Option<NodeId>,
    scanning: bool,
    accepted_prompts: bool,
    largest: bool,
    items: Vec<Item>,
    last_refresh: Instant,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const CRUMBS_H: f64 = 30.0;

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let mut header = Header::new(
        mtm,
        &root,
        w,
        &tr("Space"),
        &tr("See what fills your disk, folder by folder, largest first."),
    );
    let scan_titles = [tr("Scan"), tr("Scan Again"), tr("Scan Anyway"), tr("Stop")];
    let scan_button = header.button_for_titles(mtm, &root, &scan_titles, &mut targets, || {
        post(handle, Msg::Scan)
    });
    let choices = kipimo_space::scan_targets();
    let names: Vec<String> = choices
        .iter()
        .map(|t| match t.kind {
            TargetKind::Home => trf("Home folder ({name})", &[("name", &t.name)]),
            _ => trf(
                "{name} ({size} used)",
                &[("name", &t.name), ("size", &format::bytes(t.used_bytes))],
            ),
        })
        .collect();
    let picker = widgets::popup(mtm, &names, 0, &mut targets, |i| {
        post(handle, Msg::PickTarget(i))
    });
    // Left of the Scan button, whose width depends on the language.
    picker.setFrame(rect(header.left_edge() - 250.0, 12.0, 250.0, 25.0));
    widgets::pin_right(&picker);
    root.addSubview(&picker);
    header
        .status
        .setFrame(rect(w - MARGIN - 420.0, 40.0, 420.0, 15.0));

    let fda = tr("Open Full Disk Access…");
    let banner = Banner::new(
        mtm,
        rect(MARGIN, page::HEADER_H, w - 2.0 * MARGIN, 52.0),
        "lock.shield",
        Tint::Orange,
        &tr(
            "Without Full Disk Access, macOS asks about each protected folder (Desktop, Documents, Downloads, other apps' data) and the scan waits for your answer. Allow it first, or click Scan Anyway.",
        ),
        &mut targets,
        vec![(&fda, Box::new(|| post(handle, Msg::FullDiskAccess)))],
    );
    widgets::stretch_width(&banner.view);
    banner.view.setHidden(true);
    root.addSubview(&banner.view);

    let crumbs = crate::ui::flipped(
        mtm,
        rect(MARGIN, page::HEADER_H, w - 2.0 * MARGIN - 220.0, CRUMBS_H),
    );
    widgets::stretch_width(&crumbs);
    root.addSubview(&crumbs);
    // SAFETY: no target yet; `Targets::on` sets it below.
    let mode = unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &objc2_foundation::NSArray::from_retained_slice(&[
                crate::util::ns(&tr("Folders")),
                crate::util::ns(&tr("Largest files")),
            ]),
            objc2_app_kit::NSSegmentSwitchTracking::SelectOne,
            None,
            None,
            mtm,
        )
    };
    mode.setSelectedSegment(0);
    mode.setControlSize(objc2_app_kit::NSControlSize::Small);
    mode.setFrame(rect(w - MARGIN - 200.0, page::HEADER_H + 3.0, 200.0, 24.0));
    widgets::pin_right(&mode);
    let m = mode.clone();
    targets.on(&mode, move || {
        post(handle, Msg::Mode(m.selectedSegment().max(0) as usize))
    });
    root.addSubview(&mode);

    let body = page::body(w, h, CRUMBS_H + 4.0);
    let list = List::new(mtm, body, 38.0, |e| post(handle, Msg::List(e)));
    list.set_empty_text(&tr("Pick a place and click Scan."));
    list.add_to(&root);
    let mut footer = Footer::new(mtm, &root, w, h);
    let trash_button = footer.button(mtm, &root, &tr("Move to Trash…"), &mut targets, || {
        post(handle, Msg::Trash)
    });
    let reveal_button = footer.button(mtm, &root, &tr("Show in Finder"), &mut targets, || {
        post(handle, Msg::Reveal)
    });
    trash_button.setEnabled(false);
    reveal_button.setEnabled(false);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            cancel: Arc::new(AtomicBool::new(false)),
            header,
            footer,
            picker,
            scan_button,
            trash_button,
            reveal_button,
            mode,
            banner,
            crumbs,
            crumb_targets: Targets::default(),
            list,
            _targets: targets,
            body,
            width: w,
            choices,
            custom: None,
            choice: 0,
            map: None,
            current: None,
            scanning: false,
            accepted_prompts: false,
            largest: false,
            items: Vec::new(),
            last_refresh: Instant::now(),
        })
    });
    root
}

/// Scan a specific folder (`kipimo://tools/space/<path>`, development builds).
#[cfg(dev)]
pub fn scan_folder(path: PathBuf) {
    post(handle, Msg::Folder(path));
}

/// `kipimo://demo/space/<row>` opens that folder; `largest` shows the largest files.
#[cfg(dev)]
pub fn demo(arg: &str) {
    if arg == "largest" {
        post(handle, Msg::Mode(1));
        with(|st| st.mode.setSelectedSegment(1));
    } else if let Ok(k) = arg.parse::<usize>() {
        post(handle, Msg::List(ListEvent::DoubleClick(k)));
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

fn handle(msg: Msg) {
    match msg {
        Msg::Scan => scan(),
        Msg::PickTarget(i) => {
            with(|st| {
                if i >= st.choices.len() {
                    return;
                }
                st.choice = i;
                st.custom = None;
                st.accepted_prompts = false;
                st.banner.view.setHidden(true);
                st.scan_button.setTitle(&crate::util::ns(&tr("Scan")));
            });
        }
        #[cfg(dev)]
        Msg::Folder(p) => {
            with(|st| {
                let title = format!("Folder: {}", page::tilde(&p));
                st.picker.addItemWithTitle(&crate::util::ns(&title));
                st.picker.selectItemAtIndex(st.picker.numberOfItems() - 1);
                st.custom = Some(p);
            });
            scan();
        }
        Msg::Progress(ticket, p) => {
            let refresh_rows = with(|st| {
                if st.ticket != ticket {
                    return false;
                }
                st.header.busy(
                    true,
                    &fill(
                        &trn(
                            p.files,
                            "Scanning… {size} in {n} file · {path}",
                            "Scanning… {size} in {n} files · {path}",
                        ),
                        &[
                            ("size", &format::bytes(p.bytes)),
                            (
                                "path",
                                &p.current_path
                                    .as_deref()
                                    .map(page::tilde)
                                    .unwrap_or_default(),
                            ),
                        ],
                    ),
                );
                if st.last_refresh.elapsed().as_millis() > 1000 {
                    st.last_refresh = Instant::now();
                    return true;
                }
                false
            })
            .unwrap_or(false);
            if refresh_rows {
                refresh();
            }
        }
        Msg::Scanned(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.scanning = false;
                match r {
                    Ok(s) => {
                        let mut line = fill(
                            &trn(
                                s.totals.files,
                                "{size} in {n} file, {seconds} s",
                                "{size} in {n} files, {seconds} s",
                            ),
                            &[
                                ("size", &format::bytes(s.totals.bytes)),
                                ("seconds", &format::decimal(s.elapsed_ms as f64 / 1000.0, 1)),
                            ],
                        );
                        if s.needs_full_disk_access > 0 {
                            line = format!(
                                "{line} · {}",
                                trn(
                                    s.needs_full_disk_access,
                                    "{n} folder needs Full Disk Access",
                                    "{n} folders need Full Disk Access",
                                )
                            );
                        }
                        st.header.busy(false, &line);
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh();
        }
        Msg::List(e) => match e {
            ListEvent::DoubleClick(k) | ListEvent::Disclose(k) => {
                let node = with(|st| match st.items.get(k) {
                    Some(Item::Folder(n, _, _)) => Some(*n),
                    _ => None,
                })
                .flatten();
                if let Some(n) = node {
                    with(|st| {
                        st.current = Some(n);
                        st.largest = false;
                        st.mode.setSelectedSegment(0);
                    });
                    refresh();
                }
            }
            ListEvent::Select(_) => selection_changed(),
            ListEvent::Toggle(_) => {}
        },
        Msg::Crumb(n) => {
            with(|st| st.current = Some(NodeId(n)));
            refresh();
        }
        Msg::Mode(i) => {
            with(|st| st.largest = i == 1);
            refresh();
        }
        Msg::Reveal => {
            if let Some(p) = selected_path() {
                page::reveal(&[p]);
            }
        }
        Msg::Trash => trash(),
        Msg::Trashed(ticket, results) => {
            let note = results
                .iter()
                .map(|r| match &r.outcome {
                    TrashOutcome::Moved { .. } => trf(
                        "{name} is in the Trash.",
                        &[(
                            "name",
                            &r.path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        )],
                    ),
                    TrashOutcome::Skipped(s) => trf(
                        "Left in place: {reason}",
                        &[("reason", &engine_text::trash_skip(s))],
                    ),
                    TrashOutcome::Failed { error } => trf(
                        "Couldn't move it: {error}",
                        &[("error", &engine_text::text(error))],
                    ),
                })
                .collect::<Vec<_>>()
                .join(" ");
            with(|st| {
                if st.ticket == ticket {
                    widgets::set_text(&st.footer.text, &note);
                }
            });
            refresh();
        }
        Msg::FullDiskAccess => page::open_full_disk_access(),
    }
}

fn scan() {
    let fda = page::full_disk_access();
    let Some((ticket, cancel, root)) = with(|st| {
        if st.scanning {
            st.cancel.store(true, Ordering::Relaxed);
            return None;
        }
        let (root, protected) = match &st.custom {
            // A folder is as protected as the volume it is on (the startup disk or a
            // network share can hold folders macOS asks about).
            Some(p) => {
                let volume = st
                    .choices
                    .iter()
                    .filter(|t| t.kind != TargetKind::Home && p.starts_with(&t.path))
                    .max_by_key(|t| t.path.as_os_str().len());
                let protected =
                    volume.is_none_or(|t| t.kind == TargetKind::StartupDisk || !t.local);
                (p.clone(), protected)
            }
            None => {
                let t = st.choices.get(st.choice)?;
                (
                    t.path.clone(),
                    matches!(t.kind, TargetKind::Home | TargetKind::StartupDisk) || !t.local,
                )
            }
        };
        if protected && fda == Some(false) && !st.accepted_prompts {
            st.banner.view.setHidden(false);
            st.accepted_prompts = true;
            st.scan_button
                .setTitle(&crate::util::ns(&tr("Scan Anyway")));
            relayout(st);
            return None;
        }
        st.banner.view.setHidden(true);
        relayout(st);
        st.cancel = Arc::new(AtomicBool::new(false));
        st.ticket = super::next_generation();
        st.scanning = true;
        st.scan_button.setTitle(&crate::util::ns(&tr("Stop")));
        st.header.busy(true, &tr("Scanning…"));
        Some((st.ticket, st.cancel.clone(), root))
    })
    .flatten() else {
        refresh();
        return;
    };
    let map = match SpaceMap::new(&root, ScanOptions::default()) {
        Ok(m) => m,
        Err(e) => {
            with(|st| {
                st.scanning = false;
                st.header.busy(false, &engine_text::space_error(&e));
            });
            return;
        }
    };
    with(|st| {
        st.current = Some(map.root());
        st.map = Some(map.clone());
    });
    refresh();
    page::spawn(
        Priority::User,
        move || {
            map.scan(&cancel, |p| post(handle, Msg::Progress(ticket, p.clone())))
                .map_err(|e| engine_text::space_error(&e))
        },
        move |r| post(handle, Msg::Scanned(ticket, r.and_then(|r| r))),
    );
}

fn relayout(st: &State) {
    let shift = if st.banner.view.isHidden() { 0.0 } else { 60.0 };
    let mut c = st.crumbs.frame();
    c.origin.y = page::HEADER_H + shift;
    st.crumbs.setFrame(c);
    let mut m = st.mode.frame();
    m.origin.y = page::HEADER_H + shift + 3.0;
    st.mode.setFrame(m);
    let mut f = st.body;
    f.origin.y += shift;
    f.size.height -= shift;
    st.list.scroll.setFrame(f);
}

fn selected_path() -> Option<PathBuf> {
    with(|st| {
        let k = st.list.selected()?;
        match st.items.get(k)? {
            Item::Folder(_, p, _) | Item::File(p, _) => Some(p.clone()),
            Item::Other => None,
        }
    })
    .flatten()
}

fn selection_changed() {
    with(|st| {
        let sel = st.list.selected().and_then(|k| st.items.get(k).cloned());
        let (enabled, text) = match &sel {
            Some(Item::Folder(_, p, b)) | Some(Item::File(p, b)) => {
                (true, format!("{} · {}", page::tilde(p), format::bytes(*b)))
            }
            _ => (false, String::new()),
        };
        st.trash_button.setEnabled(enabled && !st.scanning);
        st.reveal_button.setEnabled(enabled);
        widgets::set_text(&st.footer.text, &text);
    });
}

fn trash() {
    let Some(path) = selected_path() else { return };
    let size = with(|st| {
        st.list.selected().and_then(|k| match st.items.get(k) {
            Some(Item::Folder(_, _, b)) | Some(Item::File(_, b)) => Some(*b),
            _ => None,
        })
    })
    .flatten()
    .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !page::confirm(
        &trf("Move “{name}” to the Trash?", &[("name", &name)]),
        &trf(
            "{size} at {path}. You can put it back from the Trash in Finder.",
            &[
                ("size", &format::bytes(size)),
                ("path", &page::tilde(&path)),
            ],
        ),
        &tr("Move to Trash"),
    ) {
        return;
    }
    let Some((ticket, map)) = with(|st| Some((st.ticket, st.map.clone()?))).flatten() else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            let _moving = crate::trash_guard::moving();
            map.move_to_trash(&[path])
        },
        move |r| post(handle, Msg::Trashed(ticket, r.unwrap_or_default())),
    );
}

fn entry_row(key: usize, e: &Entry) -> (Row, Item) {
    let mut r = Row::new(key, &e.name);
    r.value = format::bytes(e.size);
    r.bar = Some(e.fraction_of_parent);
    let item = match (e.kind, &e.path, e.node) {
        (EntryKind::Folder, Some(p), Some(n)) => {
            r.icon = Some(Icon::Symbol("folder", Tint::Secondary));
            r.disclosure = Some(false);
            r.subtitle = match &e.state {
                NodeState::Skipped(reason) => {
                    r.dim = true;
                    engine_text::space_skip(reason)
                }
                NodeState::Pending => tr("Not read yet"),
                NodeState::Scanned => trf(
                    "{files}, {folders}",
                    &[
                        ("files", &trn(e.files, "{n} file", "{n} files")),
                        ("folders", &trn(e.folders, "{n} folder", "{n} folders")),
                    ],
                ),
            };
            Item::Folder(n, p.clone(), e.size)
        }
        (EntryKind::File, Some(p), _) => {
            r.icon = Some(Icon::File(p.clone()));
            r.disclosure_space = true;
            r.subtitle = tr("File");
            Item::File(p.clone(), e.size)
        }
        _ => {
            r.icon = Some(Icon::Symbol("doc.on.doc", Tint::Tertiary));
            r.disclosure_space = true;
            r.subtitle = trn(e.files, "{n} smaller file", "{n} smaller files");
            r.dim = true;
            Item::Other
        }
    };
    (r, item)
}

fn file_row(key: usize, f: &FileEntry, parent_size: u64) -> (Row, Item) {
    let mut r = Row::new(key, &f.name);
    r.icon = Some(Icon::File(f.path.clone()));
    r.subtitle = f.path.parent().map(page::tilde).unwrap_or_default();
    r.value = format::bytes(f.size);
    r.bar = Some(if parent_size > 0 {
        f.size as f64 / parent_size as f64
    } else {
        0.0
    });
    (r, Item::File(f.path.clone(), f.size))
}

fn refresh() {
    let mtm = crate::util::mtm();
    with(|st| {
        let (Some(map), Some(current)) = (st.map.clone(), st.current) else {
            return;
        };
        let (rows, items): (Vec<Row>, Vec<Item>) = if st.largest {
            let size = map.folder(current).map(|f| f.size).unwrap_or(0);
            map.largest_files(current, 200)
                .iter()
                .enumerate()
                .map(|(i, f)| file_row(i, f, size))
                .unzip()
        } else {
            map.children(current)
                .iter()
                .enumerate()
                .map(|(i, e)| entry_row(i, e))
                .unzip()
        };
        st.items = items;
        st.list.set_rows(rows);
        // Breadcrumb: root › … › current.
        for v in st.crumbs.subviews().iter() {
            v.removeFromSuperview();
        }
        st.crumb_targets = Targets::default();
        // Root first, down to (and including) the current folder.
        let chain = map.ancestors(current);
        let mut x = 0.0;
        let last = chain.len().saturating_sub(1);
        for (i, n) in chain.iter().enumerate() {
            let name = map
                .folder(*n)
                .map(|f| {
                    if f.parent.is_none() {
                        page::tilde(&f.path)
                    } else {
                        f.name
                    }
                })
                .unwrap_or_default();
            if i > 0 {
                let sep = widgets::text(mtm, "›", 12.0, Weight::Regular, Tint::Tertiary);
                sep.setFrame(rect(x, 7.0, 10.0, 16.0));
                st.crumbs.addSubview(&sep);
                x += 12.0;
            }
            let id = n.0;
            let b = widgets::button(mtm, &name, &mut st.crumb_targets, move || {
                post(handle, Msg::Crumb(id))
            });
            b.setBordered(false);
            b.setFont(Some(&crate::ui::font(
                12.0,
                if i == last {
                    Weight::Semibold
                } else {
                    Weight::Regular
                },
            )));
            let bw = crate::ui::text_width(&b) + 4.0;
            b.setFrame(rect(x, 5.0, bw, 20.0));
            st.crumbs.addSubview(&b);
            x += bw + 2.0;
            if x > st.width - 2.0 * MARGIN - 240.0 {
                break;
            }
        }
        st.scan_button.setTitle(&crate::util::ns(&if st.scanning {
            tr("Stop")
        } else {
            tr("Scan Again")
        }));
        st.picker.setEnabled(!st.scanning);
    });
    selection_changed();
}
