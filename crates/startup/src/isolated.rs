//! Running a scan or the repair check in a short-lived child process.
//!
//! Why: the Security framework's validation of very large bundles keeps tens of megabytes of
//! malloc memory dirty after it finishes (measured on this Mac: about 65-85 MB retained after
//! checking Xcode, about 17 MB after a startup-items scan, never returned without memory
//! pressure). Running the work in a child that exits gives all of it back, keeps the menu-bar
//! app at its idle budget, and makes cancel instant (the child is killed).
//!
//! The child is the app's own executable started with [`WORKER_ARG`]; the app's `main` calls
//! [`worker_main`] when it sees that argument. Job and events travel as JSON lines.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::inventory::{ScanOptions, scan};
use crate::model::{Inventory, ScanProgress, StartupItem};
use crate::repair::{AppCheck, RepairEvent, RepairOptions, RepairReport, repair_check};

/// First argument that puts the executable into worker mode.
pub const WORKER_ARG: &str = "--kipimo-startup-worker";

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Job {
    Scan(ScanOptions),
    Repair(RepairOptions),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Wire {
    ScanItem {
        progress: ScanProgress,
        item: Box<StartupItem>,
    },
    ScanDone(Box<Inventory>),
    RepairFound {
        total: usize,
    },
    RepairStarted {
        path: PathBuf,
        done: usize,
        total: usize,
    },
    RepairFinished {
        check: Box<AppCheck>,
        done: usize,
        total: usize,
    },
    RepairDone(Box<RepairReport>),
    Error(String),
}

/// Worker entry point: reads one job from stdin, streams events to stdout, returns the exit
/// code. Call it from `main` when `std::env::args().nth(1) == Some(WORKER_ARG)`.
pub fn worker_main() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    worker_loop(stdin.lock(), stdout.lock())
}

fn send(out: &mut impl Write, msg: &Wire) -> bool {
    serde_json::to_writer(&mut *out, msg).is_ok()
        && out.write_all(b"\n").is_ok()
        && out.flush().is_ok()
}

pub(crate) fn worker_loop(mut input: impl Read, mut out: impl Write) -> i32 {
    let mut text = String::new();
    if input.read_to_string(&mut text).is_err() {
        return 2;
    }
    let job: Job = match serde_json::from_str(&text) {
        Ok(job) => job,
        Err(e) => {
            send(&mut out, &Wire::Error(format!("bad job: {e}")));
            return 2;
        }
    };
    // The parent cancels by killing this process.
    let cancel = AtomicBool::new(false);
    match job {
        Job::Scan(options) => {
            let inventory = scan(&options, &cancel, |progress, item| {
                send(
                    &mut out,
                    &Wire::ScanItem {
                        progress,
                        item: Box::new(item.clone()),
                    },
                );
            });
            send(&mut out, &Wire::ScanDone(Box::new(inventory)));
        }
        Job::Repair(options) => {
            let report = repair_check(&options, &cancel, |event| {
                let msg = match event {
                    RepairEvent::Found { total } => Wire::RepairFound { total },
                    RepairEvent::Started { path, done, total } => Wire::RepairStarted {
                        path: path.to_path_buf(),
                        done,
                        total,
                    },
                    RepairEvent::Finished { check, done, total } => Wire::RepairFinished {
                        check: Box::new(check.clone()),
                        done,
                        total,
                    },
                };
                send(&mut out, &msg);
            });
            send(&mut out, &Wire::RepairDone(Box::new(report)));
        }
    }
    0
}

/// Runs `job` in `exe WORKER_ARG`, calling `on_msg` for each event until it returns true
/// (the final event). Returns `Ok(false)` when cancelled (the child is killed).
fn run_child(
    exe: &Path,
    job: &Job,
    cancel: &AtomicBool,
    on_msg: &mut dyn FnMut(Wire) -> bool,
) -> Result<bool, String> {
    let mut child = Command::new(exe)
        .arg(WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't start worker: {e}"))?;
    {
        let mut stdin = child.stdin.take().ok_or("worker has no stdin")?;
        let job = serde_json::to_vec(job).map_err(|e| e.to_string())?;
        stdin
            .write_all(&job)
            .map_err(|e| format!("can't send job: {e}"))?;
        // Dropping stdin closes it, which starts the worker.
    }
    let stdout = child.stdout.take().ok_or("worker has no stdout")?;
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut finished = false;
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            cancelled = true;
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let msg: Wire =
                    serde_json::from_str(&line).map_err(|e| format!("bad worker output: {e}"))?;
                if let Wire::Error(e) = msg {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
                if on_msg(msg) {
                    finished = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.wait();
    let _ = reader.join();
    if cancelled {
        return Ok(false);
    }
    if !finished {
        return Err("worker exited before finishing".into());
    }
    Ok(true)
}

/// [`scan`] in a child process (`exe` is normally `std::env::current_exe()`). On cancel,
/// returns the items received so far with `cancelled = true`.
pub fn scan_isolated<F>(
    exe: &Path,
    options: &ScanOptions,
    cancel: &AtomicBool,
    mut on_item: F,
) -> Result<Inventory, String>
where
    F: FnMut(ScanProgress, &StartupItem),
{
    let mut items = Vec::new();
    let mut done: Option<Inventory> = None;
    let started = std::time::Instant::now();
    let completed = run_child(
        exe,
        &Job::Scan(options.clone()),
        cancel,
        &mut |msg| match msg {
            Wire::ScanItem { progress, item } => {
                on_item(progress, &item);
                items.push(*item);
                false
            }
            Wire::ScanDone(inv) => {
                done = Some(*inv);
                true
            }
            _ => false,
        },
    )?;
    match (completed, done) {
        (true, Some(inv)) => Ok(inv),
        _ => Ok(Inventory {
            items,
            btm_source: crate::model::BtmSource::Unavailable("cancelled".into()),
            warnings: Vec::new(),
            cancelled: true,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }),
    }
}

/// [`repair_check`] in a child process. On cancel, returns what finished so far with
/// `cancelled = true`.
pub fn repair_check_isolated<F>(
    exe: &Path,
    options: &RepairOptions,
    cancel: &AtomicBool,
    mut on_event: F,
) -> Result<RepairReport, String>
where
    F: FnMut(RepairEvent<'_>),
{
    let started = std::time::Instant::now();
    let mut partial = RepairReport {
        checked: 0,
        broken: Vec::new(),
        unsigned: Vec::new(),
        valid: 0,
        not_checked: Vec::new(),
        cancelled: true,
        elapsed_ms: 0,
    };
    let mut done: Option<RepairReport> = None;
    let completed = run_child(
        exe,
        &Job::Repair(options.clone()),
        cancel,
        &mut |msg| match msg {
            Wire::RepairFound { total } => {
                on_event(RepairEvent::Found { total });
                false
            }
            Wire::RepairStarted { path, done, total } => {
                on_event(RepairEvent::Started {
                    path: &path,
                    done,
                    total,
                });
                false
            }
            Wire::RepairFinished { check, done, total } => {
                on_event(RepairEvent::Finished {
                    check: &check,
                    done,
                    total,
                });
                partial.checked += 1;
                match check.signature.status {
                    crate::model::SignatureStatus::Valid => partial.valid += 1,
                    crate::model::SignatureStatus::Invalid => partial.broken.push(*check),
                    crate::model::SignatureStatus::Unsigned => partial.unsigned.push(*check),
                    crate::model::SignatureStatus::NotChecked => partial.not_checked.push(*check),
                }
                false
            }
            Wire::RepairDone(report) => {
                done = Some(*report);
                true
            }
            _ => false,
        },
    )?;
    match (completed, done) {
        (true, Some(report)) => Ok(report),
        _ => {
            partial.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(partial)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Roots;
    use crate::testutil::TempDir;

    #[test]
    fn worker_protocol_round_trip() {
        let tmp = TempDir::new("worker");
        let roots = Roots::synthetic(tmp.path());
        std::fs::create_dir_all(&roots.user_agents).unwrap();
        std::fs::write(
            roots.user_agents.join("com.example.w.plist"),
            r#"<plist version="1.0"><dict><key>Label</key><string>com.example.w</string><key>Program</key><string>/bin/ls</string></dict></plist>"#,
        )
        .unwrap();
        let job = Job::Scan(ScanOptions {
            roots,
            live_state: false,
            kernel_extensions: false,
            sfltool_fallback: false,
            btm_user_uuid: None,
        });
        let mut out = Vec::new();
        let code = worker_loop(serde_json::to_vec(&job).unwrap().as_slice(), &mut out);
        assert_eq!(code, 0);
        let lines: Vec<Wire> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[0], Wire::ScanItem { item, .. } if item.label == "com.example.w"));
        assert!(matches!(&lines[1], Wire::ScanDone(inv) if inv.items.len() == 1));

        let mut out = Vec::new();
        assert_eq!(worker_loop(&b"{not json"[..], &mut out), 2);
        assert!(String::from_utf8(out).unwrap().contains("bad job"));
    }

    #[test]
    fn repair_job_round_trip() {
        let tmp = TempDir::new("worker-repair");
        let job = Job::Repair(RepairOptions {
            folders: vec![tmp.path().to_path_buf()],
            threads: 1,
        });
        let mut out = Vec::new();
        assert_eq!(
            worker_loop(serde_json::to_vec(&job).unwrap().as_slice(), &mut out),
            0
        );
        let text = String::from_utf8(out).unwrap();
        let last: Wire = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert!(matches!(last, Wire::RepairDone(r) if r.checked == 0 && !r.cancelled));
    }
}
