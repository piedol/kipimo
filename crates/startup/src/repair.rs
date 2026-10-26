//! Repair check (SCOPE R6.3): which apps' code signatures no longer validate, for example
//! after a cleaner thinned their universal binaries or deleted language files inside the
//! bundle. Read-only: it only reads bundles and reports, it never modifies an app.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::codesign::{Checker, Depth};
use crate::inventory::home_dir;
use crate::model::{Signature, SignatureStatus, SignerKind};
use crate::owner;

/// What to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairOptions {
    /// Folders whose `.app` bundles are checked (and one level of subfolders, such as
    /// `/Applications/Utilities`).
    pub folders: Vec<PathBuf>,
    /// Parallel checks. Signature validation is mostly hashing, so a few threads help on
    /// large bundles; it is capped at 8.
    pub threads: usize,
}

impl Default for RepairOptions {
    fn default() -> Self {
        RepairOptions {
            folders: vec![
                PathBuf::from("/Applications"),
                home_dir().join("Applications"),
            ],
            threads: std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(2),
        }
    }
}

/// The result for one app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppCheck {
    pub path: PathBuf,
    pub name: String,
    pub bundle_id: Option<String>,
    pub signature: Signature,
    pub elapsed_ms: u64,
}

impl AppCheck {
    /// The app's signature no longer validates: reinstall it.
    pub fn is_broken(&self) -> bool {
        self.signature.status == SignatureStatus::Invalid
    }

    /// One line for the owner, for example "a sealed resource is missing or invalid
    /// (12 files missing, 1 changed)".
    pub fn reason(&self) -> String {
        let Some(p) = &self.signature.problem else {
            return match self.signature.status {
                SignatureStatus::Unsigned => "not signed".into(),
                _ => String::new(),
            };
        };
        let mut parts = Vec::new();
        if p.missing > 0 {
            parts.push(format!("{} missing", plural(p.missing, "file")));
        }
        if p.altered > 0 {
            parts.push(format!("{} changed", plural(p.altered, "file")));
        }
        if p.added > 0 {
            parts.push(format!("{} added", plural(p.added, "file")));
        }
        if parts.is_empty() {
            p.message.clone()
        } else {
            format!("{} ({})", p.message, parts.join(", "))
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Streaming progress of a repair check.
#[derive(Debug, Clone, Copy)]
pub enum RepairEvent<'a> {
    /// The list of apps is known.
    Found { total: usize },
    /// Started checking an app.
    Started {
        path: &'a Path,
        done: usize,
        total: usize,
    },
    /// Finished an app.
    Finished {
        check: &'a AppCheck,
        done: usize,
        total: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    pub checked: usize,
    /// Signature present but no longer valid: reinstall these.
    pub broken: Vec<AppCheck>,
    /// Never signed (not damage, but worth knowing).
    pub unsigned: Vec<AppCheck>,
    /// Apps whose signature validates.
    pub valid: usize,
    /// Paths that couldn't be checked (not code, or a seal only root can read).
    pub not_checked: Vec<AppCheck>,
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

/// The `.app` bundles directly inside each folder, plus those one level down inside plain
/// subfolders (`Utilities`, `Python 3.12`, vendor folders). Symlinked apps are followed and
/// duplicates removed. Sorted by name.
pub fn find_apps(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let is_app = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"));
    let mut push = |p: PathBuf, out: &mut Vec<PathBuf>| {
        let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if key.is_dir() && seen.insert(key) {
            out.push(p);
        }
    };
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };
        let mut level: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        level.sort();
        for path in level {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if is_app(&path) {
                push(path, &mut out);
            } else if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
                && let Ok(sub) = std::fs::read_dir(&path)
            {
                let mut inner: Vec<PathBuf> = sub
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| is_app(p))
                    .collect();
                inner.sort();
                for app in inner {
                    push(app, &mut out);
                }
            }
        }
    }
    out
}

enum Msg {
    Started(usize),
    Finished(Box<AppCheck>),
}

/// Checks every app's signature (`codesign --verify --deep --strict` equivalent, through the
/// Security framework, offline), streaming events. Cancel with `cancel`: no new app is
/// started, the apps already being checked finish (one call can't be interrupted; up to
/// about 16 s for Chrome on this Mac), and the report has `cancelled = true` with everything
/// finished so far. [`crate::repair_check_isolated`] cancels instantly.
pub fn repair_check<F>(options: &RepairOptions, cancel: &AtomicBool, on_event: F) -> RepairReport
where
    F: FnMut(RepairEvent<'_>),
{
    let apps = find_apps(&options.folders);
    check_apps(&apps, options.threads, Depth::FULL, cancel, on_event)
}

pub(crate) fn check_apps<F>(
    apps: &[PathBuf],
    threads: usize,
    depth: Depth,
    cancel: &AtomicBool,
    mut on_event: F,
) -> RepairReport
where
    F: FnMut(RepairEvent<'_>),
{
    let started = Instant::now();
    let total = apps.len();
    on_event(RepairEvent::Found { total });
    let threads = threads.clamp(1, 8).min(total.max(1));
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Msg>();

    let mut report = RepairReport {
        checked: 0,
        broken: Vec::new(),
        unsigned: Vec::new(),
        valid: 0,
        not_checked: Vec::new(),
        cancelled: false,
        elapsed_ms: 0,
    };

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let (next, stop) = (&next, &stop);
            scope.spawn(move || {
                let checker = Checker::new();
                loop {
                    if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(app) = apps.get(i) else {
                        break;
                    };
                    let _ = tx.send(Msg::Started(i));
                    let t = Instant::now();
                    // Finishes the app in hand even after a cancel (see `repair_check`).
                    let signature = checker.check(app, depth);
                    let (bundle_id, name) = owner::bundle_info(app);
                    let check = AppCheck {
                        name: name.unwrap_or_else(|| {
                            app.file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        }),
                        bundle_id,
                        path: app.clone(),
                        signature,
                        elapsed_ms: t.elapsed().as_millis() as u64,
                    };
                    let _ = tx.send(Msg::Finished(Box::new(check)));
                }
            });
        }
        drop(tx);

        let mut cancel_sent = false;
        loop {
            if !cancel_sent && cancel.load(Ordering::Relaxed) {
                cancel_sent = true;
                report.cancelled = true;
                stop.store(true, Ordering::Relaxed);
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Msg::Started(i)) => {
                    if !cancel_sent {
                        on_event(RepairEvent::Started {
                            path: &apps[i],
                            done: report.checked,
                            total,
                        });
                    }
                }
                Ok(Msg::Finished(check)) => {
                    report.checked += 1;
                    on_event(RepairEvent::Finished {
                        check: &check,
                        done: report.checked,
                        total,
                    });
                    match check.signature.status {
                        SignatureStatus::Valid => report.valid += 1,
                        SignatureStatus::Invalid => report.broken.push(*check),
                        SignatureStatus::Unsigned => report.unsigned.push(*check),
                        SignatureStatus::NotChecked => report.not_checked.push(*check),
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report
}

/// A short human summary of who signed an app, for the report table.
pub fn signer_summary(sig: &Signature) -> String {
    match (sig.kind, sig.signer.as_deref(), sig.team_id.as_deref()) {
        (SignerKind::Apple, _, _) => "Apple".into(),
        (SignerKind::AppStore, _, Some(team)) => format!("Mac App Store ({team})"),
        (SignerKind::AppStore, _, None) => "Mac App Store".into(),
        (_, Some(signer), _) => signer.to_string(),
        (SignerKind::AdHoc, None, _) => "ad-hoc".into(),
        (SignerKind::Unsigned, None, _) => "unsigned".into(),
        (_, None, Some(team)) => team.to_string(),
        (_, None, None) => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, adhoc_app};

    #[test]
    fn finds_apps_one_level_deep() {
        let tmp = TempDir::new("find-apps");
        let base = tmp.path();
        for p in [
            "A.app/Contents",
            "Utilities/B.app/Contents",
            "Vendor/Sub/C.app/Contents",
            ".Hidden.app",
            "notes.txt",
        ] {
            if p.ends_with(".txt") {
                std::fs::write(base.join(p), b"x").unwrap();
            } else {
                std::fs::create_dir_all(base.join(p)).unwrap();
            }
        }
        std::os::unix::fs::symlink(base.join("A.app"), base.join("Alias.app")).unwrap();
        let apps = find_apps(&[base.to_path_buf(), base.join("missing")]);
        let names: Vec<String> = apps
            .iter()
            .map(|p| p.strip_prefix(base).unwrap().display().to_string())
            .collect();
        // C.app is two levels down; Alias.app duplicates A.app.
        assert_eq!(names, vec!["A.app", "Utilities/B.app"]);
    }

    #[test]
    fn reports_a_broken_app() {
        let Some(cc) = crate::testutil::compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let tmp = TempDir::new("repair");
        let good = adhoc_app(&cc, tmp.path(), "Good", true);
        // Localizations are optional in Apple's seal rules: deleting a whole .lproj (what
        // "language file" cleaners do) does not break the signature...
        std::fs::remove_dir_all(good.join("Contents/Resources/de.lproj")).unwrap();
        // ...but deleting or rewriting any other sealed file does (thinning a universal
        // binary rewrites it).
        let broken = adhoc_app(&cc, tmp.path(), "Broken", true);
        std::fs::remove_file(broken.join("Contents/Resources/data.json")).unwrap();
        let unsigned = adhoc_app(&cc, tmp.path(), "Plain", false);

        let mut events = Vec::new();
        let cancel = AtomicBool::new(false);
        let report = check_apps(
            &[good, broken.clone(), unsigned],
            2,
            Depth::FULL,
            &cancel,
            |e| {
                events.push(match e {
                    RepairEvent::Found { .. } => "found",
                    RepairEvent::Started { .. } => "started",
                    RepairEvent::Finished { .. } => "finished",
                })
            },
        );
        assert_eq!(report.checked, 3);
        assert_eq!(report.valid, 1);
        assert_eq!(report.unsigned.len(), 1);
        assert_eq!(report.broken.len(), 1);
        let b = &report.broken[0];
        assert_eq!(b.path, broken);
        assert_eq!(b.name, "Broken");
        let problem = b.signature.problem.as_ref().unwrap();
        assert_eq!(problem.missing, 1, "{problem:?}");
        assert!(problem.examples[0].ends_with("Contents/Resources/data.json"));
        assert_eq!(
            b.reason(),
            "a sealed resource is missing or invalid (1 file missing)"
        );
        assert_eq!(signer_summary(&b.signature), "ad-hoc");
        assert_eq!(events.iter().filter(|e| **e == "finished").count(), 3);
        assert_eq!(events[0], "found");
    }

    #[test]
    fn cancel_before_start_checks_nothing() {
        let tmp = TempDir::new("repair-cancel");
        let apps: Vec<PathBuf> = (0..3)
            .map(|i| tmp.path().join(format!("{i}.app")))
            .collect();
        let cancel = AtomicBool::new(true);
        let report = check_apps(&apps, 1, Depth::FULL, &cancel, |_| {});
        assert!(report.cancelled);
        assert_eq!(report.checked, 0);
    }
}
