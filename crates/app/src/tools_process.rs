//! The tools window lives in its own short-lived process.
//!
//! AppKit keeps several megabytes of state after a window full of tables,
//! icons and symbols closes (about 8 MB here, whatever the page did), which
//! would leave the menu-bar app well over its 15 MB idle budget after one
//! visit. So "Open Kipimo" starts this same executable with [`ARG`]: a child
//! that shows the tools window (and leftover reviews) and quits when its last
//! window closes, giving everything back.
//!
//! Parent → child, on the child's stdin, one command per line:
//! `tab <key>[/<arg>]` (show a page, e.g. `tab space/Applications`) and
//! `review <bundle path>` (review an app's leftovers).
//! Child → parent, on stdout: `KIPIMO-INSIGHTS <json>` (what a page learned,
//! merged into the recommendations), `KIPIMO-REVIEWED <bundle id>` and
//! `KIPIMO-IGNORED <json list>` (Startup items' ignore list changed; the child saved it).
//! The child quits when its stdin closes (Kipimo quit).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::insights::{self, Summary};
use crate::tools_window;
use crate::util;

/// First argument that makes the executable the tools process.
pub const ARG: &str = "--kipimo-tools";
pub const INSIGHTS: &str = "KIPIMO-INSIGHTS ";
const REVIEWED: &str = "KIPIMO-REVIEWED ";
const IGNORED: &str = "KIPIMO-IGNORED ";

static CHILD_MODE: AtomicBool = AtomicBool::new(false);

/// Running as the tools process.
pub fn is_child() -> bool {
    CHILD_MODE.load(Ordering::Relaxed)
}

// ── Parent side ────────────────────────────────────────────────────────────

struct Handle {
    child: Child,
    stdin: ChildStdin,
}

static CHILD: Mutex<Option<Handle>> = Mutex::new(None);

/// Show a page (or review leftovers) in the tools process, starting it if needed.
pub fn send(command: &str) {
    // We just got the user's click, so we may activate; then hand activation to the child.
    crate::ui::app(util::mtm()).activate();
    let mut slot = CHILD.lock().unwrap();
    if let Some(h) = slot.as_mut() {
        let alive = matches!(h.child.try_wait(), Ok(None));
        if alive
            && writeln!(h.stdin, "{command}")
                .and_then(|_| h.stdin.flush())
                .is_ok()
        {
            yield_to(h.child.id(), 40);
            return;
        }
        *slot = None;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let spawned = Command::new(exe)
        .arg(ARG)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            util::log(format!("couldn't start the tools window: {e}"));
            return;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            util::main_async(move || from_child(&line));
        }
        // The child quit: forget it (and reap it).
        let mut slot = CHILD.lock().unwrap();
        if let Some(mut h) = slot.take() {
            let _ = h.child.wait();
        }
    });
    let pid = child.id();
    *slot = Some(Handle { child, stdin });
    drop(slot);
    yield_to(pid, 40);
}

/// Yield activation to the tools process once it has registered (polls for up to 2 s).
fn yield_to(pid: u32, tries: u32) {
    match objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32) {
        Some(app) => crate::ui::app(util::mtm()).yieldActivationToApplication(&app),
        None if tries > 0 => util::main_after(0.05, move || yield_to(pid, tries - 1)),
        None => {}
    }
}

fn from_child(line: &str) {
    if let Some(json) = line.strip_prefix(INSIGHTS) {
        if let Ok(s) = serde_json::from_str::<Summary>(json) {
            insights::merge(s);
        }
    } else if let Some(id) = line.strip_prefix(REVIEWED) {
        insights::leftovers_reviewed(id.trim());
    } else if let Some(json) = line.strip_prefix(IGNORED)
        && let Ok(list) = serde_json::from_str::<Vec<String>>(json)
    {
        crate::settings::adopt_ignored(list);
        crate::app::refresh_attention();
    }
}

/// Close the tools process's windows (`kipimo://close-windows`).
#[cfg(dev)]
pub fn close() {
    if let Some(mut h) = CHILD.lock().unwrap().take() {
        let _ = writeln!(h.stdin, "quit");
        let _ = h.stdin.flush();
        drop(h.stdin);
        std::thread::spawn(move || {
            let _ = h.child.wait();
        });
    }
}

// ── Child side ─────────────────────────────────────────────────────────────

/// Tell the parent what a page learned.
pub fn report_insights(s: &Summary) {
    println!("{INSIGHTS}{}", serde_json::to_string(s).unwrap_or_default());
}

pub fn report_reviewed(bundle_id: &str) {
    println!("{REVIEWED}{bundle_id}");
}

/// Tell the parent the ignore list changed (this process already saved it).
pub fn report_ignored(list: &[String]) {
    println!(
        "{IGNORED}{}",
        serde_json::to_string(list).unwrap_or_default()
    );
}

/// Entry point of the tools process.
pub fn child_main(first: String) {
    CHILD_MODE.store(true, Ordering::Relaxed);
    FIRST.lock().unwrap().replace(first);
    crate::app::run();
}

static FIRST: Mutex<Option<String>> = Mutex::new(None);

/// `applicationDidFinishLaunching:` in the tools process.
pub fn child_launched() {
    crate::pages::register();
    if let Some(cmd) = FIRST.lock().unwrap().take() {
        execute(&cmd);
    }
    std::thread::spawn(|| {
        let mut buf = String::new();
        let mut stdin = BufReader::new(std::io::stdin());
        loop {
            buf.clear();
            match stdin.read_line(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = buf.trim().to_string();
                    util::main_async(move || execute(&line));
                }
            }
        }
        // Kipimo quit or closed us.
        util::main_async(quit);
    });
}

fn execute(cmd: &str) {
    let (verb, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
    match verb {
        "tab" => tools_window::show_path(arg),
        // Like a click in the window: bring it forward (the URL went to the menu-bar app).
        #[cfg(dev)]
        "demo" => {
            crate::pages::demo(arg);
            tools_window::activate();
        }
        #[cfg(dev)]
        "shots" => crate::shots::run_tools(),
        "review" => crate::review_window::show_bundle(PathBuf::from(arg)),
        "review-id" => crate::review_window::show_id(arg),
        "quit" => quit(),
        _ => {}
    }
}

/// Quit once no window is left (called after each window closes).
pub fn window_closed() {
    if !is_child() || windows_open() {
        return;
    }
    // Stay up while Put Back records are being written (see `trash_guard`),
    // and keep going if a page was opened again meanwhile.
    let wait = crate::trash_guard::remaining().as_secs_f64().max(0.1);
    util::main_after(wait, || {
        if !windows_open() {
            quit();
        }
    });
}

fn windows_open() -> bool {
    tools_window::is_open() || crate::review_window::is_open()
}

/// Quit; `applicationShouldTerminate:` holds it for any pending Put Back records.
fn quit() {
    // Once: a second `terminate:` while AppKit waits for our reply ends the process at once,
    // which would lose the Put Back records the wait is for. "quit" and the closed pipe both
    // come here.
    static QUITTING: AtomicBool = AtomicBool::new(false);
    if QUITTING.swap(true, Ordering::AcqRel) {
        return;
    }
    // From a run-loop timer, so AppKit's wait in `terminate:` can still run main-queue work.
    util::run_loop_after(0.1, || crate::ui::app(util::mtm()).terminate(None));
}
