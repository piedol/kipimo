//! Kipimo: a small native menu-bar app that keeps an eye on your Mac (disk,
//! memory, battery and power, CPU and temperature, network, Bluetooth
//! batteries, XProtect) and hosts its cleanup tools.

mod actions;
mod app;
mod attention;
mod background;
mod commands;
mod format;
mod notify;
mod panel;
mod recommend;
mod selftest;
mod settings;
mod settings_window;
mod single;
mod tools_window;
mod ui;
mod util;
mod weekly;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("Kipimo is built for Apple silicon Macs only (aarch64-apple-darwin).");

fn main() {
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
            if !single::acquire() {
                single::signal_running();
                return;
            }
            app::run();
        }
    }
}
