//! What Kipimo has learned from its tools: reclaimable junk, waiting updates,
//! suspicious startup items and apps that left files behind. The tools pages
//! report here as they finish; a daily check fills it in the background. It
//! feeds the panel's recommendations and the weekly summary.
//!
//! The background check runs in a child process (this executable with
//! [`WORKER_ARG`]) at background priority: a junk scan, the update check
//! (feeds cached for a day) and a startup-items scan. Whatever memory those
//! need goes away with the child, so the menu-bar process stays at its idle
//! footprint.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kipimo_clean::{CategoryId, Env, LeftoverReport, ScanOptions};
use kipimo_startup::{Flag, StartupItem};
use serde::{Deserialize, Serialize};

use crate::actions::Action;
use crate::recommend::{self, Facts, Recommendation, Source};
use crate::tools_window::Tab;
use crate::{format, page, util};

/// First argument that makes the executable run the background check and exit.
pub const WORKER_ARG: &str = "--kipimo-insights";
const LINE_PREFIX: &str = "KIPIMO-INSIGHTS ";
/// Junk above this becomes a recommendation.
pub const JUNK_WORTH_IT: u64 = 3_000_000_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// What a cleanup with the default ticks frees.
    pub junk_selected: Option<u64>,
    pub junk_total: Option<u64>,
    /// Apps with an update this Mac can install.
    pub updates: Option<Vec<String>>,
    /// Startup items that are unsigned, invalidly signed or orphaned (not Apple's).
    pub suspicious: Option<Vec<String>>,
}

impl Summary {
    /// Keep what `other` doesn't know.
    pub fn merge(&mut self, other: Summary) {
        if other.junk_selected.is_some() {
            self.junk_selected = other.junk_selected;
            self.junk_total = other.junk_total;
        }
        if other.updates.is_some() {
            self.updates = other.updates;
        }
        if other.suspicious.is_some() {
            self.suspicious = other.suspicious;
        }
    }
}

/// An app moved to the Trash left files behind.
#[derive(Clone, Debug)]
pub struct Leftovers {
    pub bundle_id: String,
    pub name: String,
    pub report: LeftoverReport,
}

thread_local! {
    static SUMMARY: RefCell<Summary> = RefCell::new(Summary::default());
    static LEFTOVERS: RefCell<Vec<Leftovers>> = const { RefCell::new(Vec::new()) };
}

static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn summary() -> Summary {
    SUMMARY.with(|s| s.borrow().clone())
}

fn update(f: impl FnOnce(&mut Summary)) {
    SUMMARY.with(|s| f(&mut s.borrow_mut()));
    crate::app::refresh_attention();
}

pub fn junk_scanned(selected: u64, total: u64) {
    update(|s| {
        s.junk_selected = Some(selected);
        s.junk_total = Some(total);
    });
}

pub fn updates_checked(names: Vec<String>) {
    update(|s| s.updates = Some(names));
}

/// Not Apple's, and unsigned, invalidly signed, untrusted or orphaned.
pub fn suspicious(items: &[StartupItem]) -> Vec<String> {
    items
        .iter()
        .filter(|i| !i.is_apple())
        .filter(|i| {
            i.is_orphaned()
                || i.flags.iter().any(|f| {
                    matches!(
                        f,
                        Flag::Unsigned | Flag::InvalidSignature | Flag::UntrustedCertificate
                    )
                })
        })
        .map(|i| i.name.clone())
        .collect()
}

pub fn startup_scanned(items: &[StartupItem]) {
    let names = suspicious(items);
    update(|s| s.suspicious = Some(names));
}

pub fn leftovers_found(l: Leftovers) {
    LEFTOVERS.with(|v| {
        let mut v = v.borrow_mut();
        v.retain(|x| x.bundle_id != l.bundle_id);
        v.push(l);
    });
    crate::app::refresh_attention();
}

/// The pending leftovers of an app (they stay listed until reviewed).
pub fn leftovers(bundle_id: &str) -> Option<Leftovers> {
    LEFTOVERS.with(|v| {
        v.borrow()
            .iter()
            .find(|x| x.bundle_id == bundle_id)
            .cloned()
    })
}

pub fn leftovers_reviewed(bundle_id: &str) {
    LEFTOVERS.with(|v| v.borrow_mut().retain(|x| x.bundle_id != bundle_id));
    crate::app::refresh_attention();
}

// ── Recommendation sources ─────────────────────────────────────────────────

struct Junk;
impl Source for Junk {
    fn recommend(&self, _: &Facts) -> Option<Recommendation> {
        let bytes = summary().junk_selected.filter(|b| *b >= JUNK_WORTH_IT)?;
        Some(Recommendation {
            id: "junk".into(),
            title: format!("Clean up {} of junk", format::bytes(bytes)),
            detail: "Caches, logs and developer leftovers that rebuild on their own.".into(),
            button: Some(("Clean up".into(), Action::OpenTools(Tab::Clean))),
            priority: 70,
            nag: true,
        })
    }
}

struct Updates;
impl Source for Updates {
    fn recommend(&self, _: &Facts) -> Option<Recommendation> {
        let names = summary().updates.filter(|n| !n.is_empty())?;
        let title = match names.len() {
            1 => format!("{} has an update", names[0]),
            n => format!("{n} app updates are waiting"),
        };
        Some(Recommendation {
            id: "updates".into(),
            title,
            detail: names.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            button: Some(("Review".into(), Action::OpenTools(Tab::Updates))),
            priority: 40,
            // Updates are almost always waiting somewhere: don't keep the icon orange for them.
            nag: false,
        })
    }
}

struct LeftoverFiles;
impl Source for LeftoverFiles {
    fn recommend(&self, _: &Facts) -> Option<Recommendation> {
        let l = LEFTOVERS.with(|v| v.borrow().last().cloned())?;
        Some(Recommendation {
            id: format!("leftovers:{}", l.bundle_id),
            title: format!("Leftovers from {} found", l.name),
            detail: format!(
                "{} files, {}",
                l.report.leftovers.len(),
                format::bytes(l.report.total_bytes)
            ),
            button: Some((
                "Review".into(),
                Action::Custom(format!("leftovers:{}", l.bundle_id)),
            )),
            priority: 80,
            nag: true,
        })
    }
}

struct Startup;
impl Source for Startup {
    fn recommend(&self, _: &Facts) -> Option<Recommendation> {
        let names = summary().suspicious.filter(|n| !n.is_empty())?;
        Some(Recommendation {
            id: "startup".into(),
            title: match names.len() {
                1 => format!("{} starts on its own and isn't signed properly", names[0]),
                n => format!("{n} startup items need a look"),
            },
            detail: names.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            button: Some(("Review".into(), Action::OpenTools(Tab::Startup))),
            priority: 60,
            nag: true,
        })
    }
}

pub fn register_sources() {
    recommend::register(Box::new(LeftoverFiles));
    recommend::register(Box::new(Junk));
    recommend::register(Box::new(Startup));
    recommend::register(Box::new(Updates));
}

// ── The background check (child process) ───────────────────────────────────

/// Child side: run the checks at background priority and print the summary.
pub fn worker_main() -> i32 {
    // SAFETY: lowers this process's own CPU and I/O priority.
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) };
    // If Kipimo quits, its end of the pipe closes: stop at once.
    std::thread::spawn(|| {
        let mut buf = [0u8; 64];
        while matches!(std::io::stdin().read(&mut buf), Ok(n) if n > 0) {}
        std::process::exit(0);
    });
    let cancel = AtomicBool::new(false);
    let fda = page::full_disk_access();
    let mut s = Summary::default();
    if let Ok(env) = Env::current_user() {
        let mut opts = ScanOptions::default();
        if fda == Some(false) {
            opts.categories.retain(|c| *c != CategoryId::MailDownloads);
        }
        let r = kipimo_clean::scan_junk(&env, &opts, &mut |_| {}, &cancel);
        s.junk_selected = Some(r.selected_bytes);
        s.junk_total = Some(r.total_bytes);
    }
    let report = kipimo_updates::scan(
        &crate::pages::updates::discover_options(),
        &crate::pages::updates::options(false),
        |_| {},
        &cancel,
    );
    s.updates = Some(report.updates().map(|u| u.app.name.clone()).collect());
    let inv = kipimo_startup::scan(&kipimo_startup::ScanOptions::default(), &cancel, |_, _| {});
    s.suspicious = Some(suspicious(&inv.items));
    println!(
        "{LINE_PREFIX}{}",
        serde_json::to_string(&s).unwrap_or_default()
    );
    0
}

/// Parent side: run the check in a child and hand the summary to `done` on the main thread.
pub fn refresh(done: impl FnOnce(Summary) + Send + 'static) {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    page::spawn(
        page::Priority::Background,
        || -> Result<Summary, String> {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let started = Instant::now();
            let mut child = Command::new(exe)
                .arg(WORKER_ARG)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            let out = child.stdout.take().ok_or("no output")?;
            let mut summary = None;
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(json) = line.strip_prefix(LINE_PREFIX) {
                    summary = serde_json::from_str::<Summary>(json).ok();
                }
            }
            let _ = child.wait();
            util::log(format!(
                "background check took {:.1} s",
                started.elapsed().as_secs_f64()
            ));
            summary.ok_or_else(|| "the background check stopped early".into())
        },
        move |r| {
            RUNNING.store(false, Ordering::Release);
            match r {
                Ok(Ok(s)) => {
                    update(|cur| cur.merge(s.clone()));
                    done(s);
                }
                Ok(Err(e)) | Err(e) => {
                    util::log(format!("background check: {e}"));
                    done(summary());
                }
            }
        },
    );
}

thread_local! {
    static LAST: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static LAUNCHED: Instant = Instant::now();
}

/// Called from the minute check: once a day, starting ten minutes after launch.
pub fn maybe_refresh() {
    let up = LAUNCHED.with(|l| l.elapsed());
    let due = LAST.with(|l| {
        l.get()
            .is_none_or(|t| t.elapsed() > Duration::from_secs(24 * 3600))
    });
    if up > Duration::from_secs(600) && due {
        LAST.with(|l| l.set(Some(Instant::now())));
        refresh(|_| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_known_values() {
        let mut a = Summary {
            junk_selected: Some(5),
            junk_total: Some(9),
            updates: Some(vec!["Zoom".into()]),
            suspicious: None,
        };
        a.merge(Summary {
            suspicious: Some(vec!["helper".into()]),
            ..Summary::default()
        });
        assert_eq!(a.junk_selected, Some(5));
        assert_eq!(a.updates.as_deref(), Some(&["Zoom".to_string()][..]));
        assert_eq!(a.suspicious.as_deref(), Some(&["helper".to_string()][..]));
    }

    #[test]
    fn summary_round_trips_as_json() {
        let s = Summary {
            junk_selected: Some(12_000_000_000),
            junk_total: Some(14_000_000_000),
            updates: Some(vec![]),
            suspicious: Some(vec!["x".into()]),
        };
        let line = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Summary>(&line).unwrap(), s);
    }
}
