//! The tools window's pages, one per engine. Each registers with
//! [`crate::tools_window::register`] at launch, is built when the window opens
//! and is dropped (scans cancelled, results freed) when it closes.

pub mod clean;
pub mod space;
pub mod startup;
pub mod uninstall;
pub mod updates;

use std::sync::atomic::{AtomicU64, Ordering};

use kipimo_clean::{ItemOutcome, ItemResult, SkipReason};

use crate::list::{Badge, Icon, Row};
use crate::tools_window::{self, Tab};
use crate::widgets::Tint;
use crate::{format, page};

/// Register every page. Called once at launch.
pub fn register() {
    tools_window::register(Tab::Clean, || Box::new(clean::Page));
    tools_window::register(Tab::Uninstall, || Box::new(uninstall::Page));
    tools_window::register(Tab::Space, || Box::new(space::Page));
    tools_window::register(Tab::Startup, || Box::new(startup::Page));
    tools_window::register(Tab::Updates, || Box::new(updates::Page));
}

/// `kipimo://demo/<page>/<arg>`: drive a page the way a click would, for
/// screenshots and checks without touching other apps. Read-only actions only.
pub fn demo(path: &str) {
    let (page, arg) = path.split_once('/').unwrap_or((path, ""));
    match page {
        "clean" => clean::demo(arg),
        "uninstall" => uninstall::demo(arg),
        "space" => space::demo(arg),
        "startup" => startup::demo(arg),
        "updates" => updates::demo(arg),
        // `review/Applications/Audacity.app`: the leftovers review for an installed app (read-only until confirmed).
        "review" => crate::review_window::show_bundle(std::path::PathBuf::from(format!("/{arg}"))),
        _ => {}
    }
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// A fresh number for a page instance or a job, so results that arrive after
/// the page closed (or after a newer scan started) are dropped.
pub fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Why an engine left something alone, in a few words.
pub fn skip_text(r: &SkipReason) -> String {
    match r {
        SkipReason::InUse { app } => format!("In use by {app}"),
        SkipReason::Essential => "macOS relies on it".into(),
        SkipReason::KeepsData => "Holds more than a cache".into(),
        SkipReason::Own => "Kipimo's own".into(),
        SkipReason::Recent => "Recent, kept".into(),
        SkipReason::Newest => "Newest copy, kept".into(),
        SkipReason::Shared { app } => format!("Also used by {app}"),
        SkipReason::Symlink => "A link, never followed".into(),
        SkipReason::OutsideHome => "Outside your home folder".into(),
        SkipReason::NeedsAdmin => "Needs an administrator".into(),
        SkipReason::ReadOnly => "Read-only".into(),
        SkipReason::NotACandidate => "Not offered by the scan any more".into(),
        SkipReason::Missing => "Already gone".into(),
        SkipReason::ReportOnly => "Shown for information".into(),
        SkipReason::AppNotRemoved => "Kept because the app stayed".into(),
        SkipReason::Cancelled => "Cancelled".into(),
    }
}

/// One result row for a destructive action's report.
pub fn result_row(key: usize, r: &ItemResult) -> Row {
    let name = r
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| r.path.display().to_string());
    let mut row = Row::new(key, name);
    row.subtitle = page::tilde(&r.path);
    row.value = format::bytes(r.bytes);
    match &r.outcome {
        ItemOutcome::Moved { .. } => {
            row.icon = Some(Icon::Symbol("checkmark.circle.fill", Tint::Green));
            row.badges.push(Badge::new("In the Trash", Tint::Green));
        }
        ItemOutcome::Skipped { reason } => {
            row.icon = Some(Icon::Symbol("minus.circle", Tint::Secondary));
            row.badges
                .push(Badge::new(skip_text(reason), Tint::Secondary));
            row.dim = true;
        }
        ItemOutcome::Failed { error } => {
            row.icon = Some(Icon::Symbol("xmark.octagon.fill", Tint::Red));
            row.badges.push(Badge::new("Failed", Tint::Red));
            row.subtitle = format!("{} · {error}", row.subtitle);
        }
    }
    row
}

/// Result rows grouped under headers: moved, left in place, failed.
pub fn result_rows(results: &[ItemResult], first_key: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    type Pick = fn(&ItemOutcome) -> bool;
    let groups: [(&str, Pick); 3] = [
        ("Moved to the Trash", |o| {
            matches!(o, ItemOutcome::Moved { .. })
        }),
        ("Left in place", |o| {
            matches!(o, ItemOutcome::Skipped { .. })
        }),
        ("Couldn't move", |o| matches!(o, ItemOutcome::Failed { .. })),
    ];
    let mut key = first_key;
    for (title, pick) in groups {
        let items: Vec<&ItemResult> = results.iter().filter(|r| pick(&r.outcome)).collect();
        if items.is_empty() {
            continue;
        }
        let mut h = Row::header(key, title);
        h.value = format::bytes(items.iter().map(|r| r.bytes).sum());
        rows.push(h);
        key += 1;
        for r in items {
            rows.push(result_row(key, r));
            key += 1;
        }
    }
    rows
}
