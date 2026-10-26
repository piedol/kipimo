//! Kipimo: a small native menu-bar app that keeps an eye on your Mac (disk,
//! memory, battery and power, CPU and temperature, network, Bluetooth
//! batteries, XProtect) and hosts its cleanup tools.

mod actions;
mod app;
mod attention;
mod background;
mod commands;
mod drives;
mod format;
mod insights;
mod list;
mod notify;
mod page;
mod pages;
mod panel;
mod recommend;
mod review;
mod review_window;
mod selftest;
mod settings;
mod settings_window;
mod single;
mod tools_process;
mod tools_window;
mod trash_watch;
mod ui;
mod util;
mod weekly;
mod widgets;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("Kipimo is built for Apple silicon Macs only (aarch64-apple-darwin).");

fn main() {
    // Child processes this executable starts for heavy work: they do the job and exit.
    match std::env::args().nth(1).as_deref() {
        Some(kipimo_startup::WORKER_ARG) => std::process::exit(kipimo_startup::worker_main()),
        Some(insights::WORKER_ARG) => std::process::exit(insights::worker_main()),
        Some(tools_process::ARG) => {
            tools_process::child_main(std::env::args().nth(2).unwrap_or_default());
            return;
        }
        _ => {}
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // `kipimo --snapshot`: print every reader's values as JSON and exit.
        Some("--snapshot") => {
            let mut m = kipimo_sys::Monitor::new();
            std::thread::sleep(std::time::Duration::from_secs(1));
            let snap = kipimo_sys::snapshot(&mut m);
            println!(
                "{}",
                serde_json::to_string_pretty(&snap).unwrap_or_default()
            );
        }
        // `kipimo --login-item status|on|off`
        Some("--login-item") => {
            let r = match args.get(1).map(String::as_str) {
                Some("on") => app::set_login_item(true),
                Some("off") => app::set_login_item(false),
                _ => Ok(()),
            };
            match r {
                Ok(()) => println!(
                    "Launch at Login: {}",
                    if app::login_item_enabled() {
                        "on"
                    } else {
                        "off"
                    }
                ),
                Err(e) => {
                    eprintln!("Couldn't change Launch at Login: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            ensure_space_efficient_malloc();
            if !single::acquire() {
                single::signal_running();
                return;
            }
            app::run();
        }
    }
}

/// Info.plist's `LSEnvironment` turns on the space-efficient allocator, which
/// lets the memory the panel and windows free go back to macOS (idle footprint
/// about 9 MB at launch and 14 MB after use, against 13 and 23+ without it).
/// If a launch path skipped `LSEnvironment`, set it and re-exec this same
/// binary before anything else runs. Only for the app bundle, not dev runs.
fn ensure_space_efficient_malloc() {
    const VAR: &str = "MallocSpaceEfficient";
    if std::env::var_os(VAR).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if !exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return;
    }
    use std::os::unix::process::CommandExt;
    // SAFETY: single-threaded at this point; `exec` only returns on failure.
    unsafe { std::env::set_var(VAR, "1") };
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    util::log(format!("re-exec for {VAR} failed: {err}"));
}
