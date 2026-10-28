//! Startup items (tool D): everything set to start automatically, grouped,
//! with who signed it and what looks wrong (unsigned, ad-hoc, invalid,
//! missing program, orphaned). Your own launch agents can be disabled (the
//! plist goes to the Trash); system ones show the administrator commands to
//! copy; login and background items open their System Settings pane. "Check
//! Apps for Broken Signatures" runs the repair check. Both scans run in a
//! child process (the Security framework keeps tens of MB dirty otherwise).
//!
//! Flagged items and apps from the check can be ignored (see [`crate::ignore`]).
//! An app with a broken signature can be reinstalled: Reinstall… opens its App
//! Store page, its developer's download or site; Download and Replace… swaps
//! it for a verified copy from its update feed (see
//! [`kipimo_startup::replace_from_zip`]). Kipimo never re-signs an app.
//!
//! Login and background items come from the Background Task Management database, which
//! macOS lets Kipimo read only with Full Disk Access. Without it a banner says so; its Use
//! Password… scans once more with `sfltool dumpbtm`, which asks for an administrator password.
//! A scan never runs sfltool on its own.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kipimo_startup::{
    ActionOutcome, ActionResult, AppCheck, BtmSource, Flag, InstalledCopy, Inventory, ItemKind,
    RepairEvent, RepairOptions, RepairReport, ReplaceError, Replaced, ScanOptions, Scope, Severity,
    StartupItem,
};
use kipimo_updates::{Route, UpdateSource};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSButton, NSPasteboard, NSPasteboardTypeString, NSProgressIndicator, NSTextField, NSView,
};
use objc2_foundation::NSRect;

use crate::engine_text;
use crate::i18n::{tr, trf, trn};
use crate::i18n_text::fill;
use crate::ignore;
use crate::insights;
use crate::list::{Badge, Icon, List, ListEvent, Row};
use crate::page::{self, Header, MARGIN, Priority, post};
use crate::review::Helper;
use crate::tools_window::ToolPage;
use crate::ui::{Weight, rect};
use crate::widgets::{self, Banner, Targets, Tint};

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
        // Cancelling kills the child process at once.
        if let Some(st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel.store(true, Ordering::Relaxed);
            st.repair_cancel.store(true, Ordering::Relaxed);
            st.replace_cancel.store(true, Ordering::Relaxed);
        }
    }
}

enum Msg {
    Scan,
    /// Scan with `sfltool dumpbtm` for the login and background items (asks for a password).
    ScanWithPassword,
    FullDiskAccess,
    Item(u64, usize, usize),
    Scanned(u64, Result<Inventory, String>),
    List(ListEvent),
    Repair,
    RepairProgress(u64, usize, usize, Option<String>),
    Repaired(u64, Result<RepairReport, String>),
    Primary,
    Disabled(Vec<ActionResult>),
    Copy,
    Reveal,
    Ignore,
    Reinstall,
    /// Where to get a good copy of an app; `act` opens it.
    Routed(Box<RouteInfo>, bool),
    Replace,
    ReplaceProgress(PathBuf, String, Option<f64>),
    Replaced(PathBuf, Result<Replaced, ReplaceError>),
}

/// How to reinstall one app, as far as Kipimo could tell.
struct RouteInfo {
    path: PathBuf,
    route: Route,
    /// The installed copy, when Download and Replace could swap it; else why not.
    copy: Result<InstalledCopy, String>,
    /// The update feed was read (or the cache had its answer). False when only an offline
    /// look found nothing yet: Reinstall… then asks the feed first.
    checked: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Last {
    Scan,
    Repair,
}

struct State {
    ticket: u64,
    started: bool,
    cancel: Arc<AtomicBool>,
    repair_cancel: Arc<AtomicBool>,
    replace_cancel: Arc<AtomicBool>,
    header: Header,
    list: List,
    repair_button: Retained<NSButton>,
    repair_bar: Retained<NSProgressIndicator>,
    /// Shown when the login and background items couldn't be read; the list, the separator
    /// and the detail pane move down under it.
    banner: Banner,
    banner_shown: bool,
    sep: Retained<NSView>,
    pane: Retained<NSView>,
    /// Where the last finished scan got its login and background items.
    btm: Option<BtmSource>,
    detail_w: f64,
    detail_title: Retained<NSTextField>,
    detail_text: Retained<NSTextField>,
    note: Retained<NSTextField>,
    commands: Retained<NSTextField>,
    primary: Retained<NSButton>,
    reinstall: Retained<NSButton>,
    replace: Retained<NSButton>,
    reveal: Retained<NSButton>,
    ignore: Retained<NSButton>,
    copy: Retained<NSButton>,
    _targets: Targets,
    items: Vec<StartupItem>,
    scan_ms: Option<u64>,
    repair: Option<RepairReport>,
    scanning: bool,
    repairing: bool,
    last: Last,
    /// The last scan or check failed: shown instead of the counts.
    error: Option<String>,
    /// Row key → what it shows.
    keys: Vec<Key>,
    selected: Option<Key>,
    commands_text: String,
    /// The reinstall route of the selected broken app.
    route: Option<RouteInfo>,
    /// A Download and Replace in progress (the app's path).
    replacing: Option<PathBuf>,
    /// What to say under the buttons for an app (after Reinstall… or a replacement).
    notes: Vec<(PathBuf, String)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Item(usize),
    Broken(usize),
    Unsigned(usize),
    Header,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const DETAIL_W: f64 = 300.0;
/// Room for three lines of text, and the gap under the banner.
const BANNER_H: f64 = 60.0;
const BANNER_SPACE: f64 = BANNER_H + 8.0;

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let mut header = Header::new(
        mtm,
        &root,
        w,
        &tr("Startup items"),
        &tr("Everything set to start on its own, and who signed it."),
    );
    header.button(mtm, &root, &tr("Scan Again"), &mut targets, || {
        post(handle, Msg::Scan)
    });
    let repair_button = header.button(
        mtm,
        &root,
        &tr("Check Apps for Broken Signatures"),
        &mut targets,
        || post(handle, Msg::Repair),
    );
    let repair_bar = widgets::bar(mtm, rect(w - MARGIN - 300.0, 56.0, 300.0, 6.0));
    widgets::pin_right(&repair_bar);
    repair_bar.setHidden(true);
    root.addSubview(&repair_bar);
    let banner = Banner::new(
        mtm,
        rect(MARGIN, page::HEADER_H, w - 2.0 * MARGIN, BANNER_H),
        "lock.shield",
        Tint::Orange,
        "",
        &mut targets,
        vec![
            (
                &tr("Use Password…"),
                Box::new(|| post(handle, Msg::ScanWithPassword)),
            ),
            (
                &tr("Open Full Disk Access…"),
                Box::new(|| post(handle, Msg::FullDiskAccess)),
            ),
        ],
    );
    widgets::stretch_width(&banner.view);
    banner.view.setHidden(true);
    root.addSubview(&banner.view);
    let top = page::HEADER_H;
    let list = List::new(
        mtm,
        rect(0.0, top, w - DETAIL_W - 1.0, h - top),
        42.0,
        |e| post(handle, Msg::List(e)),
    );
    list.set_empty_text(&tr("Looking for startup items…"));
    list.add_to(&root);
    let sep = crate::ui::fill(
        mtm,
        rect(w - DETAIL_W - 1.0, top, 1.0, h - top),
        &objc2_app_kit::NSColor::separatorColor(),
        0.0,
    );
    sep.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
    );
    root.addSubview(&sep);
    // Detail pane. Its controls are stacked by `layout` under the text.
    let dx = w - DETAIL_W + 16.0;
    let dw = DETAIL_W - 32.0;
    let pane = crate::ui::flipped(mtm, rect(dx, top + 12.0, dw, h - top - 24.0));
    pane.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
    );
    root.addSubview(&pane);
    let detail_title = widgets::text(mtm, "", 14.0, Weight::Semibold, Tint::Normal);
    detail_title.setFrame(rect(0.0, 0.0, dw, 20.0));
    pane.addSubview(&detail_title);
    let detail_text = widgets::wrap(
        mtm,
        &tr("Select an item to see where it comes from."),
        12.0,
        Tint::Secondary,
    );
    detail_text.setFrame(rect(0.0, 26.0, dw, 190.0));
    pane.addSubview(&detail_text);
    let mut button = |title: &str, msg: fn() -> Msg| {
        let b = widgets::button(mtm, title, &mut targets, move || post(handle, msg()));
        b.setHidden(true);
        pane.addSubview(&b);
        b
    };
    let primary = button("", || Msg::Primary);
    let reinstall = button(&tr("Reinstall…"), || Msg::Reinstall);
    let replace = button(&tr("Download and Replace…"), || Msg::Replace);
    let reveal = button(&tr("Show in Finder"), || Msg::Reveal);
    let ignore = button(&tr("Ignore"), || Msg::Ignore);
    let note = widgets::wrap(mtm, "", 11.5, Tint::Secondary);
    note.setSelectable(true);
    pane.addSubview(&note);
    let commands = widgets::wrap(mtm, "", 11.0, Tint::Normal);
    commands.setFont(Some(
        &objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(10.5, unsafe {
            objc2_app_kit::NSFontWeightRegular
        }),
    ));
    commands.setSelectable(true);
    pane.addSubview(&commands);
    let copy = button(&tr("Copy Commands"), || Msg::Copy);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            started: false,
            cancel: Arc::new(AtomicBool::new(false)),
            repair_cancel: Arc::new(AtomicBool::new(false)),
            replace_cancel: Arc::new(AtomicBool::new(false)),
            header,
            list,
            repair_button,
            repair_bar,
            banner,
            banner_shown: false,
            sep: Retained::into_super(sep),
            pane,
            btm: None,
            detail_w: dw,
            detail_title,
            detail_text,
            note,
            commands,
            primary,
            reinstall,
            replace,
            reveal,
            ignore,
            copy,
            _targets: targets,
            items: Vec::new(),
            scan_ms: None,
            repair: None,
            scanning: false,
            repairing: false,
            last: Last::Scan,
            error: None,
            keys: Vec::new(),
            selected: None,
            commands_text: String::new(),
            route: None,
            replacing: None,
            notes: Vec::new(),
        })
    });
    root
}

/// `kipimo://demo/startup/<row>` selects a row; `repair` runs the signature check.
#[cfg(dev)]
pub fn demo(arg: &str) {
    if arg == "repair" {
        post(handle, Msg::Repair);
    } else if arg == "flagged" {
        with(|st| {
            let k = st
                .keys
                .iter()
                .position(|k| matches!(k, Key::Item(i) if st.items[*i].needs_attention()));
            if let Some(k) = k {
                st.list.select_key(k);
            }
        });
    } else if let Ok(k) = arg.parse::<usize>() {
        with(|st| st.list.select_key(k));
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

/// A Download and Replace is under way (an update of Kipimo waits for it). While a handler
/// is using the page's state, this says yes, to be safe.
pub fn replacing() -> bool {
    STATE.with(|s| {
        s.try_borrow().map_or(true, |st| {
            st.as_ref().is_some_and(|st| st.replacing.is_some())
        })
    })
}

fn exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

fn ignored_list() -> Vec<String> {
    crate::settings::get().ignored
}

fn handle(msg: Msg) {
    match msg {
        Msg::Scan => scan(false),
        Msg::ScanWithPassword => scan(true),
        Msg::FullDiskAccess => page::open_full_disk_access(),
        Msg::Item(ticket, done, total) => {
            with(|st| {
                if st.ticket == ticket {
                    st.header.busy(
                        true,
                        &trf(
                            "Checking {done} of {total}…",
                            &[
                                ("done", &crate::format::count(done as u64)),
                                ("total", &crate::format::count(total as u64)),
                            ],
                        ),
                    );
                }
            });
        }
        Msg::Scanned(ticket, r) => {
            let done = with(|st| {
                if st.ticket != ticket {
                    return None;
                }
                st.scanning = false;
                match r {
                    Ok(inv) => {
                        insights::startup_scanned(&inv.items);
                        st.btm = (!inv.cancelled).then(|| inv.btm_source.clone());
                        st.scan_ms = Some(inv.elapsed_ms);
                        st.last = Last::Scan;
                        st.error = None;
                        st.items = inv.items;
                        (!inv.cancelled).then(|| st.items.clone())
                    }
                    Err(e) => {
                        st.error = Some(engine_text::text(&e));
                        None
                    }
                }
            })
            .flatten();
            // A complete scan: forget ignored problems that are gone or have changed.
            if let Some(items) = done {
                let mut list = ignored_list();
                if ignore::prune_items(&mut list, &items) {
                    ignore::change(|l| *l = list);
                }
            }
            refresh();
        }
        Msg::List(ListEvent::Select(k)) => {
            with(|st| st.selected = k.and_then(|k| st.keys.get(k).copied()));
            selection_changed();
        }
        Msg::List(_) => {}
        Msg::Repair => repair(),
        Msg::RepairProgress(ticket, done, total, name) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.repair_bar.setHidden(false);
                st.repair_bar.setDoubleValue(if total > 0 {
                    done as f64 / total as f64
                } else {
                    0.0
                });
                let (done, total) = (
                    crate::format::count(done as u64),
                    crate::format::count(total as u64),
                );
                st.header.busy(
                    true,
                    &match name {
                        Some(app) => trf(
                            "Checking signatures {done} of {total} · {app}",
                            &[("done", &done), ("total", &total), ("app", &app)],
                        ),
                        None => trf(
                            "Checking signatures {done} of {total}",
                            &[("done", &done), ("total", &total)],
                        ),
                    },
                );
            });
        }
        Msg::Repaired(ticket, r) => {
            let done = with(|st| {
                if st.ticket != ticket {
                    return None;
                }
                st.repairing = false;
                st.repair_bar.setHidden(true);
                st.repair_button.setEnabled(true);
                match r {
                    Ok(rep) => {
                        st.last = Last::Repair;
                        st.error = None;
                        let complete = !rep.cancelled;
                        st.repair = Some(rep);
                        complete.then_some(())
                    }
                    Err(e) => {
                        st.error = Some(engine_text::text(&e));
                        None
                    }
                }
            })
            .flatten();
            if done.is_some() {
                let apps = with(|st| {
                    st.repair
                        .as_ref()
                        .map(|r| r.broken.iter().chain(&r.unsigned).cloned().collect())
                        .unwrap_or_default()
                })
                .unwrap_or_else(Vec::<AppCheck>::new);
                let mut list = ignored_list();
                if ignore::prune_apps(&mut list, &apps.iter().collect::<Vec<_>>()) {
                    ignore::change(|l| *l = list);
                }
            }
            refresh();
        }
        Msg::Primary => primary(),
        Msg::Disabled(results) => {
            show_outcome(&results);
            scan(false);
        }
        Msg::Copy => {
            if let Some(text) = with(|st| st.commands_text.clone()) {
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                pb.setString_forType(&crate::util::ns(&text), unsafe { NSPasteboardTypeString });
                with(|st| st.copy.setTitle(&crate::util::ns(&tr("Copied"))));
            }
        }
        Msg::Reveal => {
            if let Some(p) = with(selected_path).flatten() {
                page::reveal(&[p]);
            }
        }
        Msg::Ignore => toggle_ignore(),
        Msg::Reinstall => reinstall(),
        Msg::Routed(info, act) => routed(*info, act),
        Msg::Replace => replace(),
        Msg::ReplaceProgress(path, text, fraction) => {
            with(|st| {
                if st.replacing.as_deref() != Some(path.as_path()) {
                    return;
                }
                st.header.busy(true, &text);
                match fraction {
                    Some(f) => {
                        st.repair_bar.setHidden(false);
                        st.repair_bar.setDoubleValue(f);
                    }
                    None => st.repair_bar.setHidden(true),
                }
            });
        }
        Msg::Replaced(path, r) => replaced(path, r),
    }
}

fn selected_path(st: &mut State) -> Option<PathBuf> {
    match st.selected {
        Some(Key::Item(i)) => st
            .items
            .get(i)
            .and_then(|it| it.plist_path.clone().or(it.program.clone())),
        Some(Key::Broken(i)) | Some(Key::Unsigned(i)) => selected_app(st, i).map(|a| a.path),
        _ => None,
    }
}

/// The app behind a Broken/Unsigned row.
fn selected_app(st: &State, i: usize) -> Option<AppCheck> {
    let rep = st.repair.as_ref()?;
    match st.selected {
        Some(Key::Broken(_)) => rep.broken.get(i).cloned(),
        Some(Key::Unsigned(_)) => rep.unsigned.get(i).cloned(),
        _ => None,
    }
}

/// Scans for startup items. With `password`, `sfltool dumpbtm` reads the login and background
/// items if the database can't be (it asks for an administrator password).
fn scan(password: bool) {
    let options = ScanOptions {
        sfltool_fallback: password,
        ..ScanOptions::default()
    };
    let Some((ticket, cancel)) = with(|st| {
        st.cancel.store(true, Ordering::Relaxed);
        st.cancel = Arc::new(AtomicBool::new(false));
        st.ticket = super::next_generation();
        st.started = true;
        st.scanning = true;
        st.repairing = false;
        st.header.busy(true, &tr("Looking for startup items…"));
        (st.ticket, st.cancel.clone())
    }) else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            kipimo_startup::scan_isolated(&exe()?, &options, &cancel, |p, _| {
                post(handle, Msg::Item(ticket, p.done, p.total));
            })
        },
        move |r| post(handle, Msg::Scanned(ticket, r.and_then(|r| r))),
    );
}

fn repair() {
    let Some((ticket, cancel)) = with(|st| {
        if st.repairing {
            st.repair_cancel.store(true, Ordering::Relaxed);
            return None;
        }
        if st.replacing.is_some() {
            return None;
        }
        st.repair_cancel = Arc::new(AtomicBool::new(false));
        st.repairing = true;
        st.repair_bar.setDoubleValue(0.0);
        st.repair_bar.setHidden(false);
        st.header.busy(true, &tr("Finding apps…"));
        Some((st.ticket, st.repair_cancel.clone()))
    })
    .flatten() else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            kipimo_startup::repair_check_isolated(
                &exe()?,
                &RepairOptions::default(),
                &cancel,
                |e| match e {
                    RepairEvent::Found { total } => {
                        post(handle, Msg::RepairProgress(ticket, 0, total, None))
                    }
                    RepairEvent::Started { .. } => {}
                    RepairEvent::Finished { check, done, total } => post(
                        handle,
                        Msg::RepairProgress(ticket, done, total, Some(check.name.clone())),
                    ),
                },
            )
        },
        move |r| post(handle, Msg::Repaired(ticket, r.and_then(|r| r))),
    );
}

/// The group an item is listed in, in display order.
fn section(item: &StartupItem) -> usize {
    match (item.kind, item.scope) {
        (ItemKind::LoginItem, _) => 0,
        (ItemKind::BackgroundItem, _) => 1,
        (ItemKind::LaunchAgent, Scope::User) => 2,
        (ItemKind::LaunchAgent, Scope::System) => 3,
        (ItemKind::LaunchDaemon, _) => 4,
        (ItemKind::SystemExtension, _) => 5,
        (ItemKind::KernelExtension, _) => 6,
    }
}

fn section_title(section: usize) -> String {
    match section {
        0 => tr("Open at login"),
        1 => tr("Allowed in the background"),
        2 => tr("Your launch agents"),
        3 => tr("Launch agents for all users"),
        4 => tr("Launch daemons"),
        5 => tr("System extensions"),
        _ => tr("Kernel extensions"),
    }
}

fn symbol(item: &StartupItem) -> &'static str {
    match item.kind {
        ItemKind::LoginItem => "person.crop.circle",
        ItemKind::BackgroundItem => "rectangle.stack",
        ItemKind::LaunchAgent => "gearshape",
        ItemKind::LaunchDaemon => "gearshape.2",
        ItemKind::SystemExtension => "puzzlepiece.extension",
        ItemKind::KernelExtension => "cpu",
    }
}

fn flag_badge(f: Flag, quiet: bool) -> Badge {
    let text = match f {
        Flag::Unsigned => tr("Unsigned"),
        Flag::AdHocSigned => tr("Ad-hoc signed"),
        Flag::InvalidSignature => tr("Invalid signature"),
        Flag::UntrustedCertificate => tr("Untrusted certificate"),
        Flag::ProgramMissing => tr("Program missing"),
        Flag::OwnerAppMissing => tr("Orphaned"),
        Flag::RunsScript => tr("Runs a script"),
        Flag::ScriptMissing => tr("Script missing"),
        Flag::MalformedPlist => tr("Broken plist"),
    };
    let tint = match f.severity() {
        _ if quiet => Tint::Secondary,
        Severity::Info => Tint::Secondary,
        _ if matches!(f, Flag::InvalidSignature | Flag::UntrustedCertificate) => Tint::Red,
        _ => Tint::Orange,
    };
    Badge::new(text, tint)
}

fn ignored_badge() -> Badge {
    Badge::new(tr("Ignored"), Tint::Secondary)
}

fn state_text(item: &StartupItem) -> String {
    let s = &item.state;
    if s.pid.is_some() {
        tr("Running")
    } else if s.enabled == Some(false) || s.launchd_override_disabled == Some(true) {
        tr("Off")
    } else if s.loaded == Some(true) {
        tr("Loaded")
    } else {
        String::new()
    }
}

fn app_row(a: &AppCheck, broken: bool, ignored: bool) -> Row {
    let mut r = Row::new(0, &a.name);
    r.icon = Some(Icon::File(a.path.clone()));
    r.subtitle = if broken {
        engine_text::app_check_reason(a)
    } else {
        engine_text::signer(&a.signature)
    };
    if ignored {
        r.badges.push(ignored_badge());
        r.dim = true;
    }
    r.badges.push(match (broken, ignored) {
        (true, false) => Badge::new(tr("Broken signature"), Tint::Red),
        (true, true) => Badge::new(tr("Broken signature"), Tint::Secondary),
        (false, false) => Badge::new(tr("Unsigned"), Tint::Orange),
        (false, true) => Badge::new(tr("Unsigned"), Tint::Secondary),
    });
    r
}

/// The header's summary of the last scan or check, leaving ignored entries out of the counts.
fn status_line(st: &State, ignored: &[String]) -> String {
    if let Some(e) = &st.error {
        return e.clone();
    }
    match (st.last, &st.repair) {
        (Last::Repair, Some(rep)) => {
            let broken = rep
                .broken
                .iter()
                .filter(|a| !ignore::app_ignored(a, ignored))
                .count();
            let unsigned = rep
                .unsigned
                .iter()
                .filter(|a| !ignore::app_ignored(a, ignored))
                .count();
            let skipped = rep.broken.len() + rep.unsigned.len() - broken - unsigned;
            let mut parts = vec![fill(
                &trn(
                    rep.checked as u64,
                    "{n} app checked: {broken} broken, {unsigned} unsigned",
                    "{n} apps checked: {broken} broken, {unsigned} unsigned",
                ),
                &[
                    ("broken", &crate::format::count(broken as u64)),
                    ("unsigned", &crate::format::count(unsigned as u64)),
                ],
            )];
            if skipped > 0 {
                parts.push(trf(
                    "{n} ignored",
                    &[("n", &crate::format::count(skipped as u64))],
                ));
            }
            parts.join(" · ")
        }
        _ => {
            let Some(ms) = st.scan_ms else {
                return String::new();
            };
            let flagged = st.items.iter().filter(|i| i.needs_attention()).count();
            let skipped = st
                .items
                .iter()
                .filter(|i| ignore::item_ignored(i, ignored))
                .count();
            let mut parts = vec![
                trn(st.items.len() as u64, "{n} item", "{n} items"),
                trn(
                    (flagged - skipped) as u64,
                    "{n} needs a look",
                    "{n} need a look",
                ),
            ];
            if skipped > 0 {
                parts.push(trf(
                    "{n} ignored",
                    &[("n", &crate::format::count(skipped as u64))],
                ));
            }
            parts.push(trf(
                "{seconds} s",
                &[("seconds", &crate::format::decimal(ms as f64 / 1000.0, 1))],
            ));
            parts.join(" · ")
        }
    }
}

/// Shows the banner when the last scan couldn't read the login and background items, and
/// moves what's under it out of the way.
fn update_banner(st: &mut State) {
    let message = match &st.btm {
        Some(BtmSource::NoAccess) => Some((
            tr(
                "Login and background items need Full Disk Access. Allow it and scan again, or use your password for this scan.",
            ),
            true,
        )),
        Some(BtmSource::Unavailable(_)) => Some((
            tr(
                "Kipimo couldn't read the login and background items. Your password lets it read them for this scan.",
            ),
            false,
        )),
        _ => None,
    };
    let show = message.is_some();
    if let Some((text, full_disk_access)) = message {
        if let Some(b) = st.banner.buttons.get(1) {
            b.setHidden(!full_disk_access);
        }
        st.banner.set_message(&text);
    }
    if show != st.banner_shown {
        let dy = if show { BANNER_SPACE } else { -BANNER_SPACE };
        let views: [&NSView; 3] = [&st.list.scroll, &st.sep, &st.pane];
        for v in views {
            let mut f = v.frame();
            f.origin.y += dy;
            f.size.height -= dy;
            v.setFrame(f);
        }
        st.banner.view.setHidden(!show);
        st.banner_shown = show;
    }
}

fn refresh() {
    let ignored = ignored_list();
    with(|st| {
        update_banner(st);
        // While a scan, check or download runs, its progress stays in the header.
        if !st.scanning && !st.repairing && st.replacing.is_none() {
            let line = status_line(st, &ignored);
            st.header.busy(false, &line);
        }
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let push = |r: Row, k: Key, rows: &mut Vec<Row>, keys: &mut Vec<Key>| {
            let mut r = r;
            r.key = keys.len();
            rows.push(r);
            keys.push(k);
        };
        if let Some(rep) = &st.repair {
            let counted = |v: &[AppCheck]| {
                v.iter()
                    .filter(|a| !ignore::app_ignored(a, &ignored))
                    .count()
            };
            let mut h = Row::header(
                0,
                fill(
                    &trn(
                        rep.checked as u64,
                        "App signatures: {broken} of {n} app broken",
                        "App signatures: {broken} of {n} apps broken",
                    ),
                    &[("broken", &crate::format::count(counted(&rep.broken) as u64))],
                ),
            );
            h.value = trf(
                "{n} unsigned",
                &[("n", &crate::format::count(counted(&rep.unsigned) as u64))],
            );
            push(h, Key::Header, &mut rows, &mut keys);
            // Ignored apps go last in their group, dimmed.
            for want_ignored in [false, true] {
                for (i, a) in rep.broken.iter().enumerate() {
                    let ig = ignore::app_ignored(a, &ignored);
                    if ig == want_ignored {
                        push(app_row(a, true, ig), Key::Broken(i), &mut rows, &mut keys);
                    }
                }
            }
            for want_ignored in [false, true] {
                for (i, a) in rep.unsigned.iter().enumerate() {
                    let ig = ignore::app_ignored(a, &ignored);
                    if ig == want_ignored {
                        push(
                            app_row(a, false, ig),
                            Key::Unsigned(i),
                            &mut rows,
                            &mut keys,
                        );
                    }
                }
            }
            if rep.broken.is_empty() && rep.unsigned.is_empty() {
                let mut r = Row::new(0, tr("Every app's signature checks out"));
                r.icon = Some(Icon::Symbol("checkmark.seal", Tint::Green));
                push(r, Key::Header, &mut rows, &mut keys);
            }
        }
        let flagged: Vec<bool> = st
            .items
            .iter()
            .map(|i| i.needs_attention() && !ignore::item_ignored(i, &ignored))
            .collect();
        let mut order: Vec<usize> = (0..st.items.len()).collect();
        order.sort_by_key(|&i| {
            (
                section(&st.items[i]),
                !flagged[i],
                st.items[i].is_apple(),
                st.items[i].name.to_lowercase(),
            )
        });
        let mut last = usize::MAX;
        for i in order {
            let it = &st.items[i];
            let ig = ignore::item_ignored(it, &ignored);
            let sec = section(it);
            if sec != last {
                let n = st.items.iter().filter(|x| section(x) == sec).count();
                let mut h = Row::header(0, section_title(sec));
                h.value = crate::format::count(n as u64);
                push(h, Key::Header, &mut rows, &mut keys);
                last = sec;
            }
            let mut r = Row::new(0, &it.name);
            r.icon = Some(Icon::Symbol(
                symbol(it),
                if flagged[i] {
                    Tint::Orange
                } else {
                    Tint::Secondary
                },
            ));
            let who = it
                .developer
                .clone()
                .or_else(|| it.signature.as_ref().and_then(|s| s.signer.clone()))
                .or_else(|| it.owner.as_ref().map(|o| o.name.clone()))
                .unwrap_or_else(|| it.label.clone());
            r.subtitle = match &it.program {
                Some(p) => format!("{who} · {}", page::tilde(p)),
                None => who,
            };
            let mut flags = it.flags.clone();
            if it.is_orphaned()
                && !flags.contains(&Flag::OwnerAppMissing)
                && !flags.contains(&Flag::ProgramMissing)
            {
                flags.push(Flag::OwnerAppMissing);
            }
            if ig {
                r.badges.push(ignored_badge());
            }
            for f in flags {
                r.badges.push(flag_badge(f, ig));
            }
            r.value = state_text(it);
            r.value_tint = Tint::Secondary;
            r.dim = it.is_apple() || ig;
            push(r, Key::Item(i), &mut rows, &mut keys);
        }
        // Keep the selection on the same entry when the rows move (after Ignore).
        let selected = st.selected;
        st.keys = keys;
        st.list.set_rows(rows);
        if let Some(k) = selected.and_then(|s| st.keys.iter().position(|k| *k == s)) {
            st.list.select_key(k);
        }
    });
    selection_changed();
}

/// The selection changed (or what it shows did): update the pane, and for a broken app look
/// up, offline, where a good copy could come from.
fn selection_changed() {
    let lookup = with(|st| {
        let path = match st.selected {
            Some(Key::Broken(i)) => selected_app(st, i).map(|a| a.path),
            _ => None,
        };
        if st.route.as_ref().map(|r| &r.path) != path.as_ref() {
            st.route = None;
        }
        path.filter(|_| st.route.is_none())
    })
    .flatten();
    detail();
    if let Some(path) = lookup {
        find_route(path, false, false);
    }
}

/// Works out the reinstall route in the background. `online` lets the update check ask the
/// app's feed (when its cached answer is more than a day old); `act` opens the route after.
fn find_route(path: PathBuf, online: bool, act: bool) {
    page::spawn(
        Priority::User,
        move || {
            let cancel = AtomicBool::new(false);
            let home = crate::pages::updates::discover_options().home;
            let app = kipimo_updates::read_app(&path, home.as_deref());
            let store_id = kipimo_updates::store_id(&path);
            let (route, checked) = match &app {
                Some(app) => {
                    let store_known =
                        matches!(app.source, UpdateSource::AppStore { .. }) && store_id.is_some();
                    let update = if app.source.is_known() && !store_known {
                        let opts = kipimo_updates::CheckOptions {
                            offline: !online,
                            ..crate::pages::updates::options(false)
                        };
                        kipimo_updates::check_updates(
                            std::slice::from_ref(app),
                            &opts,
                            |_| {},
                            &cancel,
                        )
                        .apps
                        .into_iter()
                        .next()
                    } else {
                        None
                    };
                    let checked =
                        online || update.is_none() || update.as_ref().is_some_and(|u| u.from_cache);
                    (
                        kipimo_updates::reinstall_route(app, update.as_ref(), store_id),
                        checked,
                    )
                }
                None => (Route::Finder, true),
            };
            // Only worth knowing when there is a zip to swap in.
            let copy = if route.zip_url().is_some() {
                kipimo_startup::replaceable(&path).map_err(|e| engine_text::replace_error(&e))
            } else {
                Err(String::new())
            };
            Box::new(RouteInfo {
                path,
                route,
                copy,
                checked,
            })
        },
        move |r| {
            if let Ok(info) = r {
                post(handle, Msg::Routed(info, act));
            }
        },
    );
}

fn routed(info: RouteInfo, act: bool) {
    let path = info.path.clone();
    let wanted = with(|st| {
        let still = matches!(st.selected, Some(Key::Broken(i)) if selected_app(st, i).is_some_and(|a| a.path == path));
        if still || act {
            st.route = Some(info);
        }
        still
    })
    .unwrap_or(false);
    if act {
        open_route(&path);
    }
    if wanted {
        detail();
    }
}

fn app_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn host(url: &str) -> String {
    url.split_once("://")
        .map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or(r))
        .unwrap_or(url)
        .to_string()
}

fn set_note(path: &Path, text: String) {
    with(|st| {
        st.notes.retain(|(p, _)| p != path);
        st.notes.push((path.to_path_buf(), text));
    });
}

/// Reinstall…: open the route, after asking the feed when only the offline answer is known.
fn reinstall() {
    let Some((path, ready)) = with(|st| {
        let Some(Key::Broken(i)) = st.selected else {
            return None;
        };
        let path = selected_app(st, i)?.path;
        let ready = st
            .route
            .as_ref()
            .is_some_and(|r| r.path == path && r.checked);
        Some((path, ready))
    })
    .flatten() else {
        return;
    };
    if ready {
        open_route(&path);
    } else {
        set_note(&path, tr("Looking up where to get it…"));
        detail();
        find_route(path, true, true);
    }
}

fn open_route(path: &Path) {
    let Some(route) = with(|st| {
        st.route
            .as_ref()
            .filter(|r| r.path == path)
            .map(|r| r.route.clone())
    })
    .flatten() else {
        return;
    };
    let name = app_name(path);
    let (open, note) = match &route {
        Route::AppStore { url: Some(u) } => (
            Some(u.clone()),
            trf(
                "Opened {app} in the App Store. If it shows Open rather than Get, move this copy to the Trash first, then download it again there.",
                &[("app", &name)],
            ),
        ),
        Route::AppStore { url: None } => (
            Some("file:///System/Applications/App%20Store.app/".into()),
            trf(
                "Opened the App Store. {app} is under your account's purchased apps; move this copy to the Trash first, then download it again there.",
                &[("app", &name)],
            ),
        ),
        Route::Download { url, source, .. } => (
            Some(url.clone()),
            trf(
                "Opened {app}'s download from {host} (found through its {source} feed). Replace this copy with the new one.",
                &[
                    ("app", &name),
                    ("host", &host(url)),
                    ("source", &engine_text::source_label(source)),
                ],
            ),
        ),
        Route::Website { url } => (
            Some(url.clone()),
            trf(
                "Opened {host}. Download a fresh copy of {app} from its developer and replace this one.",
                &[("host", &host(url)), ("app", &name)],
            ),
        ),
        Route::Finder => (
            None,
            trf(
                "Kipimo doesn't know where {app} comes from. Download a fresh copy from its developer and replace this one.",
                &[("app", &name)],
            ),
        ),
    };
    match open {
        Some(u) => crate::actions::run(crate::actions::Action::OpenUrl(u)),
        None => page::reveal(&[path.to_path_buf()]),
    }
    set_note(path, note);
    detail();
}

/// Download and Replace…: confirm, then download, verify and swap in the background.
fn replace() {
    let Some((path, url, version, copy)) = with(|st| {
        if st.replacing.is_some() || st.repairing {
            return None;
        }
        let r = st.route.as_ref()?;
        let copy = r.copy.clone().ok()?;
        let (url, version) = match &r.route {
            Route::Download {
                url,
                version,
                zip: true,
                ..
            } => (url.clone(), version.clone()),
            _ => return None,
        };
        Some((r.path.clone(), url, version, copy))
    })
    .flatten() else {
        return;
    };
    let name = app_name(&path);
    let running = Helper::running_in(&path);
    if !running.is_empty() {
        page::inform(
            &trf("Quit {app} first", &[("app", &name)]),
            &trf(
                "{helpers} is running. Kipimo replaces an app only while it isn't.",
                &[("helpers", &Helper::names(&running))],
            ),
        );
        return;
    }
    let (from, team) = (host(&url), copy.team_id.clone());
    let explain = match &version {
        Some(v) => trf(
            "Kipimo downloads {app} {version} from {host}, checks that its signature is intact and that it is signed by the same developer as this copy (team {team}), then moves this copy to the Trash and puts the new one in its place. If a check fails, nothing changes. Put Back in Finder restores the old copy.",
            &[
                ("app", &name),
                ("version", v),
                ("host", &from),
                ("team", &team),
            ],
        ),
        None => trf(
            "Kipimo downloads {app} from {host}, checks that its signature is intact and that it is signed by the same developer as this copy (team {team}), then moves this copy to the Trash and puts the new one in its place. If a check fails, nothing changes. Put Back in Finder restores the old copy.",
            &[("app", &name), ("host", &from), ("team", &team)],
        ),
    };
    if !page::confirm(
        &trf("Replace {app} with a fresh copy?", &[("app", &name)]),
        &explain,
        &tr("Download and Replace"),
    ) {
        return;
    }
    let Some(cancel) = with(|st| {
        st.replace_cancel = Arc::new(AtomicBool::new(false));
        st.replacing = Some(path.clone());
        st.header
            .busy(true, &trf("Downloading {app}…", &[("app", &name)]));
        st.repair_bar.setDoubleValue(0.0);
        st.repair_bar.setHidden(false);
        st.replace_cancel.clone()
    }) else {
        return;
    };
    set_note(&path, trf("Downloading {app}…", &[("app", &name)]));
    detail();
    let p = path.clone();
    page::spawn(
        Priority::User,
        move || download_and_replace(&p, &url, &cancel),
        move |r| {
            let r = r.unwrap_or_else(|e| {
                Err(ReplaceError::Failed {
                    error: e,
                    old_in_trash: None,
                })
            });
            post(handle, Msg::Replaced(path, r));
        },
    );
}

/// The background half of Download and Replace (see `kipimo_startup::replace_from_zip`).
fn download_and_replace(
    path: &Path,
    url: &str,
    cancel: &AtomicBool,
) -> Result<Replaced, ReplaceError> {
    let failed = |error: String| ReplaceError::Failed {
        error,
        old_in_trash: None,
    };
    // On the app's volume, so the new copy can be renamed into place.
    let work = kipimo_startup::WorkDir::for_app(path).map_err(failed)?;
    let zip = work.path().join("download.zip");
    let name = app_name(path);
    let mut last = -1i64;
    kipimo_updates::download(
        url,
        &zip,
        &kipimo_updates::DownloadOptions::default(),
        cancel,
        |done, total| {
            let pct = total.map(|t| (done * 100 / t.max(1)) as i64).unwrap_or(-1);
            if pct != last || total.is_none() && done % (8 << 20) < (256 << 10) {
                last = pct;
                let text = match total {
                    Some(t) => trf(
                        "Downloading {app}… {done} of {total}",
                        &[
                            ("app", &name),
                            ("done", &crate::format::bytes(done)),
                            ("total", &crate::format::bytes(t)),
                        ],
                    ),
                    None => trf(
                        "Downloading {app}… {size}",
                        &[("app", &name), ("size", &crate::format::bytes(done))],
                    ),
                };
                let fraction = total.map(|t| done as f64 / t.max(1) as f64);
                post(
                    handle,
                    Msg::ReplaceProgress(path.to_path_buf(), text, fraction),
                );
            }
        },
    )
    .map_err(|e| {
        failed(trf(
            "The download didn't finish: {error}",
            &[("error", &engine_text::text(&e.to_string()))],
        ))
    })?;
    post(
        handle,
        Msg::ReplaceProgress(
            path.to_path_buf(),
            trf("Checking the new copy of {app}…", &[("app", &name)]),
            None,
        ),
    );
    if is_running(path) {
        return Err(ReplaceError::NotReplaceable(trf(
            "{app} was opened during the download. Quit it and try again.",
            &[("app", &name)],
        )));
    }
    // Keeps the process up for Finder's Put Back record of the old copy.
    let _moving = crate::trash_guard::moving();
    kipimo_startup::replace_from_zip(path, &zip, &work)
}

/// A process runs from inside the bundle (libproc only; safe off the main thread).
fn is_running(bundle: &Path) -> bool {
    use kipimo_clean::RunningApps;
    let real = std::fs::canonicalize(bundle).unwrap_or_else(|_| bundle.to_path_buf());
    kipimo_clean::SystemRunningApps
        .processes()
        .iter()
        .any(|p| p.executable.starts_with(&real))
}

fn replaced(path: PathBuf, r: Result<Replaced, ReplaceError>) {
    let name = app_name(&path);
    let blocked = matches!(r, Err(ReplaceError::Blocked(_)));
    let note = match &r {
        Ok(done) => match done.version.as_deref() {
            Some(v) => trf(
                "Replaced {app} with a fresh copy (version {version}). The damaged copy is in the Trash; Put Back in Finder restores it.",
                &[("app", &name), ("version", v)],
            ),
            None => trf(
                "Replaced {app} with a fresh copy. The damaged copy is in the Trash; Put Back in Finder restores it.",
                &[("app", &name)],
            ),
        },
        Err(ReplaceError::Blocked(_)) => trf(
            "macOS blocked moving {app}, so nothing changed. Allow Kipimo in System Settings › Privacy & Security › App Management, then try again.",
            &[("app", &name)],
        ),
        Err(e) => trf(
            "{app} wasn't replaced: {reason}",
            &[("app", &name), ("reason", &engine_text::replace_error(e))],
        ),
    };
    let ok = r.is_ok();
    // The window closed meanwhile (the download was cancelled): nothing to show.
    let Some(()) = with(|st| {
        if st.replacing.as_deref() == Some(path.as_path()) {
            st.replacing = None;
        }
        st.repair_bar.setHidden(true);
        if ok {
            // Fixed: it leaves the broken list (the next check re-reads it anyway).
            if let Some(rep) = st.repair.as_mut() {
                rep.broken.retain(|a| a.path != path);
            }
            st.route = None;
            st.selected = None;
        }
    }) else {
        return;
    };
    set_note(&path, note.clone());
    refresh();
    if ok {
        // Startup items that came from the old copy may be flagged no longer.
        scan(false);
        page::inform(&trf("{app} was replaced", &[("app", &name)]), &note);
    } else if blocked {
        if page::ask(
            &trf("macOS blocked moving {app}", &[("app", &name)]),
            &tr(
                "Replacing some signed apps needs App Management permission. Turn on Kipimo in System Settings › Privacy & Security › App Management, then try again.",
            ),
            &tr("Open Settings"),
        ) {
            page::open_app_management();
        }
    } else {
        page::inform(&trf("{app} wasn't replaced", &[("app", &name)]), &note);
    }
}

fn toggle_ignore() {
    enum Target {
        Item(Box<StartupItem>),
        App(Box<AppCheck>),
    }
    let Some(target) = with(|st| match st.selected {
        Some(Key::Item(i)) => st
            .items
            .get(i)
            .cloned()
            .map(|it| Target::Item(Box::new(it))),
        Some(Key::Broken(i)) | Some(Key::Unsigned(i)) => {
            selected_app(st, i).map(|a| Target::App(Box::new(a)))
        }
        _ => None,
    })
    .flatten() else {
        return;
    };
    let ignored = ignored_list();
    match target {
        Target::Item(it) => {
            if ignore::item_ignored(&it, &ignored) {
                ignore::change(|l| ignore::unignore_item(l, &it));
            } else {
                ignore::change(|l| ignore::ignore_item(l, &it));
            }
        }
        Target::App(a) => {
            if ignore::app_ignored(&a, &ignored) {
                ignore::change(|l| ignore::unignore_app(l, &a));
            } else {
                ignore::change(|l| ignore::ignore_app(l, &a));
            }
        }
    }
    refresh();
}

fn detail() {
    let ignored = ignored_list();
    with(|st| {
        st.commands_text.clear();
        widgets::set_text(&st.commands, "");
        widgets::set_text(&st.note, "");
        st.copy.setHidden(true);
        st.copy.setTitle(&crate::util::ns(&tr("Copy Commands")));
        for b in [
            &st.primary,
            &st.reinstall,
            &st.replace,
            &st.reveal,
            &st.ignore,
        ] {
            b.setHidden(true);
            b.setEnabled(true);
        }
        let set_ignore = |b: &NSButton, on: bool| {
            b.setTitle(&crate::util::ns(&if on {
                tr("Stop Ignoring")
            } else {
                tr("Ignore")
            }));
            b.setHidden(false);
        };
        match st.selected {
            Some(Key::Item(i)) => {
                let Some(it) = st.items.get(i) else { return };
                widgets::set_text(&st.detail_title, &it.name);
                let mut lines = Vec::new();
                lines.push(if it.is_apple() {
                    trf("{kind} from Apple", &[("kind", &kind_text(it))])
                } else {
                    kind_text(it)
                });
                if let Some(o) = &it.owner {
                    lines.push(if o.installed {
                        trf("Belongs to {app}", &[("app", &o.name)])
                    } else {
                        trf(
                            "Belongs to {app} (no longer installed)",
                            &[("app", &o.name)],
                        )
                    });
                }
                if let Some(s) = &it.signature {
                    lines.push(trf(
                        "Signed: {signer}",
                        &[("signer", &engine_text::signer(s))],
                    ));
                }
                if let Some(p) = &it.program {
                    lines.push(trf("Runs {path}", &[("path", &page::tilde(p))]));
                }
                if let Some(p) = &it.plist_path {
                    lines.push(trf("Defined in {path}", &[("path", &page::tilde(p))]));
                }
                let state = state_text(it);
                if !state.is_empty() {
                    lines.push(trf("Now: {state}", &[("state", &state)]));
                }
                for f in &it.flags {
                    lines.push(format!("• {}", engine_text::flag(*f)));
                }
                let ig = ignore::item_ignored(it, &ignored);
                if ig {
                    lines.push(String::new());
                    lines.push(tr(
                        "Ignored: it doesn't count as needing a look until something about it changes.",
                    ));
                }
                widgets::set_text(&st.detail_text, &lines.join("\n"));
                st.reveal
                    .setHidden(it.plist_path.is_none() && it.program.is_none());
                let action = match (it.kind, it.scope) {
                    (ItemKind::LaunchAgent, Scope::User) => Some(tr("Disable and Move to Trash…")),
                    (ItemKind::LaunchAgent | ItemKind::LaunchDaemon, Scope::System) => {
                        Some(tr("Show Administrator Commands"))
                    }
                    (
                        ItemKind::LoginItem | ItemKind::BackgroundItem | ItemKind::SystemExtension,
                        _,
                    ) => Some(tr("Open Login Items Settings")),
                    (ItemKind::KernelExtension, _) | (ItemKind::LaunchDaemon, Scope::User) => None,
                };
                if let Some(a) = action {
                    st.primary.setTitle(&crate::util::ns(&a));
                    st.primary.setHidden(false);
                }
                if it.needs_attention() {
                    set_ignore(&st.ignore, ig);
                }
            }
            Some(Key::Broken(i)) | Some(Key::Unsigned(i)) => {
                let broken = matches!(st.selected, Some(Key::Broken(_)));
                let Some(a) = selected_app(st, i) else { return };
                widgets::set_text(&st.detail_title, &a.name);
                let ig = ignore::app_ignored(&a, &ignored);
                let mut text = if broken {
                    format!(
                        "{}\n\n{}",
                        engine_text::app_check_reason(&a),
                        tr(
                            "Its code signature no longer matches its files, so macOS may refuse to open it or its updates may fail. Reinstalling it from its developer or the App Store fixes this. Kipimo doesn't re-sign apps: that would replace the developer's signature with one made on this Mac."
                        )
                    )
                } else {
                    format!(
                        "{}\n\n{}",
                        engine_text::signer(&a.signature),
                        tr("Unsigned apps are allowed but nobody vouches for them.")
                    )
                };
                if let Some(v) = &a.version {
                    text = format!("{}\n{text}", trf("Version {version}", &[("version", v)]));
                }
                if ig {
                    text = format!(
                        "{text}\n\n{}",
                        tr("Ignored: it doesn't count until it is updated or reinstalled.")
                    );
                }
                widgets::set_text(&st.detail_text, &text);
                st.reveal.setHidden(false);
                set_ignore(&st.ignore, ig);
                if broken {
                    st.reinstall.setHidden(false);
                    let route = st.route.as_ref().filter(|r| r.path == a.path);
                    let can_replace =
                        route.is_some_and(|r| r.route.zip_url().is_some() && r.copy.is_ok());
                    st.replace.setHidden(!can_replace);
                    let busy = st.replacing.is_some();
                    st.reinstall.setEnabled(!busy);
                    st.replace.setEnabled(!busy);
                    let hint = route.map(|r| route_hint(&r.route, r.copy.as_ref().err()));
                    let note = st
                        .notes
                        .iter()
                        .find(|(p, _)| *p == a.path)
                        .map(|(_, n)| n.clone())
                        .or(hint)
                        .unwrap_or_default();
                    widgets::set_text(&st.note, &note);
                }
            }
            _ => {
                widgets::set_text(&st.detail_title, "");
                widgets::set_text(
                    &st.detail_text,
                    &tr("Select an item to see where it comes from."),
                );
            }
        }
    });
    layout();
}

/// One line under the buttons saying what Reinstall… will do.
fn route_hint(route: &Route, no_replace: Option<&String>) -> String {
    match route {
        Route::AppStore { .. } => tr("From the App Store."),
        Route::Download {
            url,
            version,
            source,
            zip,
        } => {
            let (source, from) = (engine_text::source_label(source), host(url));
            let mut s = match version {
                Some(v) => trf(
                    "Its {source} feed offers version {version} from {host}.",
                    &[("source", &source), ("version", v), ("host", &from)],
                ),
                None => trf(
                    "Its {source} feed offers version from {host}.",
                    &[("source", &source), ("host", &from)],
                ),
            };
            if *zip && let Some(why) = no_replace.filter(|w| !w.is_empty()) {
                s = format!(
                    "{s} {}",
                    trf(
                        "Kipimo can't replace this copy itself: {reason}.",
                        &[("reason", why)]
                    )
                );
            }
            s
        }
        Route::Website { url } => trf("Its developer's site: {host}.", &[("host", &host(url))]),
        Route::Finder => String::new(),
    }
}

/// Stacks the pane's visible controls under its text.
fn layout() {
    with(|st| {
        let dw = st.detail_w;
        let mut y = 26.0;
        let fit = |c: &NSTextField, y: f64| {
            let h = crate::ui::text_height(c, dw).ceil();
            c.setFrame(rect(0.0, y, dw, h));
            h
        };
        y += fit(&st.detail_text, y) + 10.0;
        for b in [
            &st.primary,
            &st.reinstall,
            &st.replace,
            &st.reveal,
            &st.ignore,
        ] {
            if !b.isHidden() {
                b.setFrame(rect(-6.0, y, dw + 12.0, 30.0));
                y += 32.0;
            }
        }
        if !st.note.stringValue().to_string().is_empty() {
            y += 4.0;
            y += fit(&st.note, y) + 8.0;
        } else {
            st.note.setFrame(rect(0.0, y, dw, 0.0));
        }
        if !st.commands.stringValue().to_string().is_empty() {
            y += 4.0;
            y += fit(&st.commands, y) + 4.0;
        } else {
            st.commands.setFrame(rect(0.0, y, dw, 0.0));
        }
        st.copy.setFrame(rect(-6.0, y, dw + 12.0, 30.0));
    });
}

fn kind_text(it: &StartupItem) -> String {
    match (it.kind, it.scope) {
        (ItemKind::LoginItem, _) => tr("Opens when you log in"),
        (ItemKind::BackgroundItem, _) => tr("Runs in the background"),
        (ItemKind::LaunchAgent, Scope::User) => tr("Launch agent for your account"),
        (ItemKind::LaunchAgent, Scope::System) => tr("Launch agent for every user"),
        (ItemKind::LaunchDaemon, _) => tr("Launch daemon (runs as root)"),
        (ItemKind::SystemExtension, _) => tr("System extension"),
        (ItemKind::KernelExtension, _) => tr("Kernel extension"),
    }
}

fn primary() {
    let Some(item) = with(|st| match st.selected {
        Some(Key::Item(i)) => st.items.get(i).cloned(),
        _ => None,
    })
    .flatten() else {
        return;
    };
    match (item.kind, item.scope) {
        (ItemKind::LaunchAgent, Scope::User) => {
            if !page::confirm(
                &trf("Disable {item}?", &[("item", &item.name)]),
                &tr(
                    "Kipimo stops it now and moves its launch agent file to the Trash, so it won't start at login. To undo, use Put Back in Finder: it starts again at your next login.",
                ),
                &tr("Disable"),
            ) {
                return;
            }
            with(|st| {
                widgets::set_text(&st.commands, &tr("Disabling…"));
                st.primary.setEnabled(false);
            });
            layout();
            // SMJobRemove can block for a while; keep it off the main thread.
            page::spawn(
                Priority::User,
                move || {
                    let _moving = crate::trash_guard::moving();
                    kipimo_startup::disable_items(std::slice::from_ref(&item))
                },
                move |r| post(handle, Msg::Disabled(r.unwrap_or_default())),
            );
        }
        (ItemKind::LaunchAgent | ItemKind::LaunchDaemon, Scope::System) => {
            // For system-wide items this only works out the commands; nothing is touched.
            let results = kipimo_startup::disable_items(std::slice::from_ref(&item));
            show_outcome(&results);
        }
        _ => {
            let _ = kipimo_startup::open_settings(kipimo_startup::LOGIN_ITEMS_SETTINGS_URL);
        }
    }
}

fn show_outcome(results: &[ActionResult]) {
    let Some(r) = results.first() else { return };
    with(|st| match &r.outcome {
        ActionOutcome::NeedsAdmin { disable, remove } => {
            let mut text = format!(
                "{}\n\n{}\n",
                tr("Run in Terminal (it asks for your password)."),
                tr("To turn it off (reversible):")
            );
            for c in disable {
                text.push_str(&format!("{}\n", c.command));
            }
            text.push_str(&format!(
                "\n{}\n",
                tr("To remove it (files go to your Trash):")
            ));
            for c in remove {
                text.push_str(&format!("{}\n", c.command));
            }
            st.commands_text = text.clone();
            widgets::set_text(&st.commands, &text);
            st.copy.setHidden(false);
        }
        ActionOutcome::Disabled { .. } => widgets::set_text(
            &st.commands,
            &trf(
                "{item} is off and its file is in the Trash. Put Back in Finder restores it; it then starts again at your next login.",
                &[("item", &r.name)],
            ),
        ),
        ActionOutcome::OpenSettings { hint, .. } => {
            widgets::set_text(&st.commands, &engine_text::text(hint))
        }
        ActionOutcome::Skipped { reason } => {
            widgets::set_text(&st.commands, &engine_text::text(reason))
        }
        ActionOutcome::Failed { error } => widgets::set_text(
            &st.commands,
            &trf(
                "Couldn't disable it: {error}",
                &[("error", &engine_text::text(error))],
            ),
        ),
    });
    layout();
}
