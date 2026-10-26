//! Startup items (tool D): everything set to start automatically, grouped,
//! with who signed it and what looks wrong (unsigned, ad-hoc, invalid,
//! missing program, orphaned). Your own launch agents can be disabled (the
//! plist goes to the Trash); system ones show the administrator commands to
//! copy; login and background items open their System Settings pane. "Check
//! Apps for Broken Signatures" runs the repair check. Both scans run in a
//! child process (the Security framework keeps tens of MB dirty otherwise).

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kipimo_startup::{
    ActionOutcome, ActionResult, AppCheck, Flag, Inventory, ItemKind, RepairEvent, RepairOptions,
    RepairReport, ScanOptions, Scope, Severity, StartupItem,
};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSButton, NSPasteboard, NSPasteboardTypeString, NSProgressIndicator, NSTextField, NSView,
};
use objc2_foundation::NSRect;

use crate::insights;
use crate::list::{Badge, Icon, List, ListEvent, Row};
use crate::page::{self, Header, MARGIN, Priority, post};
use crate::tools_window::ToolPage;
use crate::ui::{Weight, rect};
use crate::widgets::{self, Targets, Tint};

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
        }
    }
}

enum Msg {
    Scan,
    Item(u64, usize, usize),
    Scanned(u64, Result<Inventory, String>),
    List(ListEvent),
    Repair,
    RepairProgress(u64, usize, usize, Option<String>),
    Repaired(u64, Result<RepairReport, String>),
    Primary,
    Copy,
    Reveal,
}

struct State {
    ticket: u64,
    started: bool,
    cancel: Arc<AtomicBool>,
    repair_cancel: Arc<AtomicBool>,
    header: Header,
    list: List,
    repair_button: Retained<NSButton>,
    repair_bar: Retained<NSProgressIndicator>,
    detail_title: Retained<NSTextField>,
    detail_text: Retained<NSTextField>,
    commands: Retained<NSTextField>,
    primary: Retained<NSButton>,
    copy: Retained<NSButton>,
    reveal: Retained<NSButton>,
    _targets: Targets,
    items: Vec<StartupItem>,
    repair: Option<RepairReport>,
    repairing: bool,
    /// Row key → what it shows.
    keys: Vec<Key>,
    selected: Option<Key>,
    commands_text: String,
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

fn build(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSView> {
    let (w, h) = (frame.size.width, frame.size.height);
    let root = page::root(mtm, frame);
    let mut targets = Targets::default();
    let mut header = Header::new(
        mtm,
        &root,
        w,
        "Startup items",
        "Everything set to start on its own, and who signed it.",
    );
    header.button(mtm, &root, "Scan Again", &mut targets, || {
        post(handle, Msg::Scan)
    });
    let repair_button = header.button(
        mtm,
        &root,
        "Check Apps for Broken Signatures",
        &mut targets,
        || post(handle, Msg::Repair),
    );
    let repair_bar = widgets::bar(mtm, rect(w - MARGIN - 300.0, 56.0, 300.0, 6.0));
    widgets::pin_right(&repair_bar);
    repair_bar.setHidden(true);
    root.addSubview(&repair_bar);
    let top = page::HEADER_H;
    let list = List::new(
        mtm,
        rect(0.0, top, w - DETAIL_W - 1.0, h - top),
        42.0,
        |e| post(handle, Msg::List(e)),
    );
    list.set_empty_text("Looking for startup items…");
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
    // Detail pane.
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
        "Select an item to see where it comes from.",
        12.0,
        Tint::Secondary,
    );
    detail_text.setFrame(rect(0.0, 26.0, dw, 190.0));
    pane.addSubview(&detail_text);
    let primary = widgets::button(mtm, "", &mut targets, || post(handle, Msg::Primary));
    primary.setFrame(rect(-6.0, 222.0, dw + 12.0, 30.0));
    primary.setHidden(true);
    pane.addSubview(&primary);
    let reveal = widgets::button(mtm, "Show in Finder", &mut targets, || {
        post(handle, Msg::Reveal)
    });
    reveal.setFrame(rect(-6.0, 254.0, dw + 12.0, 30.0));
    reveal.setHidden(true);
    pane.addSubview(&reveal);
    let commands = widgets::wrap(mtm, "", 11.0, Tint::Normal);
    commands.setFont(Some(
        &objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(10.5, unsafe {
            objc2_app_kit::NSFontWeightRegular
        }),
    ));
    commands.setSelectable(true);
    commands.setFrame(rect(0.0, 292.0, dw, 150.0));
    pane.addSubview(&commands);
    let copy = widgets::button(mtm, "Copy Commands", &mut targets, || {
        post(handle, Msg::Copy)
    });
    copy.setFrame(rect(-6.0, 446.0, dw + 12.0, 30.0));
    copy.setHidden(true);
    pane.addSubview(&copy);
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            ticket: super::next_generation(),
            started: false,
            cancel: Arc::new(AtomicBool::new(false)),
            repair_cancel: Arc::new(AtomicBool::new(false)),
            header,
            list,
            repair_button,
            repair_bar,
            detail_title,
            detail_text,
            commands,
            primary,
            copy,
            reveal,
            _targets: targets,
            items: Vec::new(),
            repair: None,
            repairing: false,
            keys: Vec::new(),
            selected: None,
            commands_text: String::new(),
        })
    });
    root
}

/// `kipimo://demo/startup/<row>` selects a row; `repair` runs the signature check.
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

fn exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

fn handle(msg: Msg) {
    match msg {
        Msg::Scan => scan(),
        Msg::Item(ticket, done, total) => {
            with(|st| {
                if st.ticket == ticket {
                    st.header
                        .busy(true, &format!("Checking {done} of {total}…"));
                }
            });
        }
        Msg::Scanned(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                match r {
                    Ok(inv) => {
                        let flagged = inv.items.iter().filter(|i| i.needs_attention()).count();
                        st.header.busy(
                            false,
                            &format!(
                                "{} items · {} need a look · {:.1} s",
                                inv.items.len(),
                                flagged,
                                inv.elapsed_ms as f64 / 1000.0
                            ),
                        );
                        insights::startup_scanned(&inv.items);
                        st.items = inv.items;
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh();
        }
        Msg::List(ListEvent::Select(k)) => {
            with(|st| st.selected = k.and_then(|k| st.keys.get(k).copied()));
            detail();
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
                st.header.busy(
                    true,
                    &format!(
                        "Checking signatures {done} of {total}{}",
                        name.map(|n| format!(" · {n}")).unwrap_or_default()
                    ),
                );
            });
        }
        Msg::Repaired(ticket, r) => {
            with(|st| {
                if st.ticket != ticket {
                    return;
                }
                st.repairing = false;
                st.repair_bar.setHidden(true);
                st.repair_button.setEnabled(true);
                match r {
                    Ok(rep) => {
                        st.header.busy(
                            false,
                            &format!(
                                "{} apps checked: {} broken, {} unsigned",
                                rep.checked,
                                rep.broken.len(),
                                rep.unsigned.len()
                            ),
                        );
                        st.repair = Some(rep);
                    }
                    Err(e) => st.header.busy(false, &e),
                }
            });
            refresh();
        }
        Msg::Primary => primary(),
        Msg::Copy => {
            if let Some(text) = with(|st| st.commands_text.clone()) {
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                pb.setString_forType(&crate::util::ns(&text), unsafe { NSPasteboardTypeString });
                with(|st| st.copy.setTitle(&crate::util::ns("Copied")));
            }
        }
        Msg::Reveal => {
            let p = with(|st| match st.selected {
                Some(Key::Item(i)) => st
                    .items
                    .get(i)
                    .and_then(|it| it.plist_path.clone().or(it.program.clone())),
                Some(Key::Broken(i)) => st
                    .repair
                    .as_ref()
                    .and_then(|r| r.broken.get(i))
                    .map(|a| a.path.clone()),
                Some(Key::Unsigned(i)) => st
                    .repair
                    .as_ref()
                    .and_then(|r| r.unsigned.get(i))
                    .map(|a| a.path.clone()),
                _ => None,
            })
            .flatten();
            if let Some(p) = p {
                page::reveal(&[p]);
            }
        }
    }
}

fn scan() {
    let Some((ticket, cancel)) = with(|st| {
        st.cancel.store(true, Ordering::Relaxed);
        st.cancel = Arc::new(AtomicBool::new(false));
        st.ticket = super::next_generation();
        st.started = true;
        st.repairing = false;
        st.header.busy(true, "Looking for startup items…");
        (st.ticket, st.cancel.clone())
    }) else {
        return;
    };
    page::spawn(
        Priority::User,
        move || {
            kipimo_startup::scan_isolated(&exe()?, &ScanOptions::default(), &cancel, |p, _| {
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
        st.repair_cancel = Arc::new(AtomicBool::new(false));
        st.repairing = true;
        st.repair_bar.setDoubleValue(0.0);
        st.repair_bar.setHidden(false);
        st.header.busy(true, "Finding apps…");
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

fn section(item: &StartupItem) -> (usize, &'static str) {
    match (item.kind, item.scope) {
        (ItemKind::LoginItem, _) => (0, "Open at login"),
        (ItemKind::BackgroundItem, _) => (1, "Allowed in the background"),
        (ItemKind::LaunchAgent, Scope::User) => (2, "Your launch agents"),
        (ItemKind::LaunchAgent, Scope::System) => (3, "Launch agents for all users"),
        (ItemKind::LaunchDaemon, _) => (4, "Launch daemons"),
        (ItemKind::SystemExtension, _) => (5, "System extensions"),
        (ItemKind::KernelExtension, _) => (6, "Kernel extensions"),
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

fn flag_badge(f: Flag) -> Badge {
    let text = match f {
        Flag::Unsigned => "Unsigned",
        Flag::AdHocSigned => "Ad-hoc signed",
        Flag::InvalidSignature => "Invalid signature",
        Flag::UntrustedCertificate => "Untrusted certificate",
        Flag::ProgramMissing => "Program missing",
        Flag::OwnerAppMissing => "Orphaned",
        Flag::RunsScript => "Runs a script",
        Flag::ScriptMissing => "Script missing",
        Flag::MalformedPlist => "Broken plist",
    };
    let tint = match f.severity() {
        Severity::Info => Tint::Secondary,
        _ if matches!(f, Flag::InvalidSignature | Flag::UntrustedCertificate) => Tint::Red,
        _ => Tint::Orange,
    };
    Badge::new(text, tint)
}

fn state_text(item: &StartupItem) -> String {
    let s = &item.state;
    if s.pid.is_some() {
        "Running".into()
    } else if s.enabled == Some(false) || s.launchd_override_disabled == Some(true) {
        "Off".into()
    } else if s.loaded == Some(true) {
        "Loaded".into()
    } else {
        String::new()
    }
}

fn app_row(key: usize, a: &AppCheck, broken: bool) -> Row {
    let mut r = Row::new(key, &a.name);
    r.icon = Some(Icon::File(a.path.clone()));
    r.subtitle = if broken {
        a.reason()
    } else {
        kipimo_startup::signer_summary(&a.signature)
    };
    r.badges.push(if broken {
        Badge::new("Broken signature", Tint::Red)
    } else {
        Badge::new("Unsigned", Tint::Orange)
    });
    r
}

fn refresh() {
    with(|st| {
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let push = |r: Row, k: Key, rows: &mut Vec<Row>, keys: &mut Vec<Key>| {
            let mut r = r;
            r.key = keys.len();
            rows.push(r);
            keys.push(k);
        };
        if let Some(rep) = &st.repair {
            let mut h = Row::header(
                0,
                format!(
                    "App signatures: {} of {} apps broken",
                    rep.broken.len(),
                    rep.checked
                ),
            );
            h.value = format!("{} unsigned", rep.unsigned.len());
            push(h, Key::Header, &mut rows, &mut keys);
            for (i, a) in rep.broken.iter().enumerate() {
                push(app_row(0, a, true), Key::Broken(i), &mut rows, &mut keys);
            }
            for (i, a) in rep.unsigned.iter().enumerate() {
                push(app_row(0, a, false), Key::Unsigned(i), &mut rows, &mut keys);
            }
            if rep.broken.is_empty() && rep.unsigned.is_empty() {
                let mut r = Row::new(0, "Every app's signature checks out");
                r.icon = Some(Icon::Symbol("checkmark.seal", Tint::Green));
                push(r, Key::Header, &mut rows, &mut keys);
            }
        }
        let mut order: Vec<usize> = (0..st.items.len()).collect();
        order.sort_by_key(|&i| {
            (
                section(&st.items[i]).0,
                !st.items[i].needs_attention(),
                st.items[i].is_apple(),
                st.items[i].name.to_lowercase(),
            )
        });
        let mut last = usize::MAX;
        for i in order {
            let it = &st.items[i];
            let (sec, title) = section(it);
            if sec != last {
                let n = st.items.iter().filter(|x| section(x).0 == sec).count();
                let mut h = Row::header(0, title);
                h.value = format!("{n}");
                push(h, Key::Header, &mut rows, &mut keys);
                last = sec;
            }
            let mut r = Row::new(0, &it.name);
            r.icon = Some(Icon::Symbol(
                symbol(it),
                if it.needs_attention() {
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
            for f in flags {
                r.badges.push(flag_badge(f));
            }
            r.value = state_text(it);
            r.value_tint = Tint::Secondary;
            r.dim = it.is_apple();
            push(r, Key::Item(i), &mut rows, &mut keys);
        }
        st.keys = keys;
        st.list.set_rows(rows);
    });
    detail();
}

fn detail() {
    with(|st| {
        st.commands_text.clear();
        widgets::set_text(&st.commands, "");
        st.copy.setHidden(true);
        st.copy.setTitle(&crate::util::ns("Copy Commands"));
        st.primary.setHidden(true);
        st.reveal.setHidden(true);
        match st.selected {
            Some(Key::Item(i)) => {
                let Some(it) = st.items.get(i) else { return };
                widgets::set_text(&st.detail_title, &it.name);
                let mut lines = Vec::new();
                lines.push(format!(
                    "{}{}",
                    kind_text(it),
                    if it.is_apple() { " from Apple" } else { "" }
                ));
                if let Some(o) = &it.owner {
                    lines.push(format!(
                        "Belongs to {}{}",
                        o.name,
                        if o.installed {
                            ""
                        } else {
                            " (no longer installed)"
                        }
                    ));
                }
                if let Some(s) = &it.signature {
                    lines.push(format!("Signed: {}", kipimo_startup::signer_summary(s)));
                }
                if let Some(p) = &it.program {
                    lines.push(format!("Runs {}", page::tilde(p)));
                }
                if let Some(p) = &it.plist_path {
                    lines.push(format!("Defined in {}", page::tilde(p)));
                }
                let state = state_text(it);
                if !state.is_empty() {
                    lines.push(format!("Now: {state}"));
                }
                for f in &it.flags {
                    lines.push(format!("• {}", f.describe()));
                }
                widgets::set_text(&st.detail_text, &lines.join("\n"));
                st.reveal
                    .setHidden(it.plist_path.is_none() && it.program.is_none());
                let action = match (it.kind, it.scope) {
                    (ItemKind::LaunchAgent, Scope::User) => Some("Disable and Move to Trash…"),
                    (ItemKind::LaunchAgent | ItemKind::LaunchDaemon, Scope::System) => {
                        Some("Show Administrator Commands")
                    }
                    (
                        ItemKind::LoginItem | ItemKind::BackgroundItem | ItemKind::SystemExtension,
                        _,
                    ) => Some("Open Login Items Settings"),
                    (ItemKind::KernelExtension, _) | (ItemKind::LaunchDaemon, Scope::User) => None,
                };
                if let Some(a) = action {
                    st.primary.setTitle(&crate::util::ns(a));
                    st.primary.setHidden(false);
                }
            }
            Some(Key::Broken(i)) | Some(Key::Unsigned(i)) => {
                let broken = matches!(st.selected, Some(Key::Broken(_)));
                let Some(rep) = &st.repair else { return };
                let Some(a) = (if broken {
                    rep.broken.get(i)
                } else {
                    rep.unsigned.get(i)
                }) else {
                    return;
                };
                widgets::set_text(&st.detail_title, &a.name);
                let text = if broken {
                    format!(
                        "{}\n\nIts code signature no longer matches its files, so macOS may refuse to open it or its updates may fail. Reinstalling it from its developer or the App Store fixes this.",
                        a.reason()
                    )
                } else {
                    format!(
                        "{}\n\nUnsigned apps are allowed but nobody vouches for them.",
                        kipimo_startup::signer_summary(&a.signature)
                    )
                };
                widgets::set_text(&st.detail_text, &text);
                st.reveal.setHidden(false);
            }
            _ => {
                widgets::set_text(&st.detail_title, "");
                widgets::set_text(
                    &st.detail_text,
                    "Select an item to see where it comes from.",
                );
            }
        }
    });
}

fn kind_text(it: &StartupItem) -> &'static str {
    match (it.kind, it.scope) {
        (ItemKind::LoginItem, _) => "Opens when you log in",
        (ItemKind::BackgroundItem, _) => "Runs in the background",
        (ItemKind::LaunchAgent, Scope::User) => "Launch agent for your account",
        (ItemKind::LaunchAgent, Scope::System) => "Launch agent for every user",
        (ItemKind::LaunchDaemon, _) => "Launch daemon (runs as root)",
        (ItemKind::SystemExtension, _) => "System extension",
        (ItemKind::KernelExtension, _) => "Kernel extension",
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
                &format!("Disable {}?", item.name),
                "Kipimo stops it for this session and moves its launch agent file to the Trash, so it won't start again. Put Back in Finder restores it.",
                "Disable",
            ) {
                return;
            }
            let results = kipimo_startup::disable_items(std::slice::from_ref(&item));
            show_outcome(&results);
            post(handle, Msg::Scan);
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
            let mut text = String::from(
                "Run in Terminal (it asks for your password).\n\nTo turn it off (reversible):\n",
            );
            for c in disable {
                text.push_str(&format!("{}\n", c.command));
            }
            text.push_str("\nTo remove it (files go to your Trash):\n");
            for c in remove {
                text.push_str(&format!("{}\n", c.command));
            }
            st.commands_text = text.clone();
            widgets::set_text(&st.commands, &text);
            st.copy.setHidden(false);
        }
        ActionOutcome::Disabled { .. } => widgets::set_text(
            &st.commands,
            &format!("{} is off and its file is in the Trash.", r.name),
        ),
        ActionOutcome::OpenSettings { hint, .. } => widgets::set_text(&st.commands, hint),
        ActionOutcome::Skipped { reason } => widgets::set_text(&st.commands, reason),
        ActionOutcome::Failed { error } => {
            widgets::set_text(&st.commands, &format!("Couldn't disable it: {error}"))
        }
    });
}
