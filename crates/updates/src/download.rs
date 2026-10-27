//! Downloading one file to disk, for Reinstall's Download and Replace. Only ever started by
//! the user, for an app's own download link from its update feed.
//!
//! Same network rules as the update check (rustls, the system proxy for each hop, a generic
//! `User-Agent`, no cookies), but the body streams to a file instead of memory, with a stall
//! timeout instead of a whole-request one, so a large app can take as long as it needs.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::url;

const MAX_REDIRECTS: usize = 8;

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    /// Largest file accepted. Default 4 GB.
    pub max_bytes: u64,
    pub connect_timeout: Duration,
    /// Give up when no data arrives for this long. Default 30 s.
    pub stall_timeout: Duration,
    /// Use the proxy from System Settings. Default true.
    pub use_system_proxy: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        DownloadOptions {
            max_bytes: 4_000_000_000,
            connect_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_secs(30),
            use_system_proxy: true,
        }
    }
}

/// Downloads `start` (http or https, following redirects) to the file `dest`, which must not
/// exist yet. `progress` gets the bytes received so far and the total when the server says.
/// Stops with an error when `cancel` is set. A partial file is removed on failure.
pub fn download(
    start: &str,
    dest: &Path,
    opts: &DownloadOptions,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let mut created = false;
    let r = fetch(start, dest, opts, cancel, &mut progress, &mut created);
    if r.is_err() && created {
        let _ = std::fs::remove_file(dest);
    }
    r
}

fn fetch(
    start: &str,
    dest: &Path,
    opts: &DownloadOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
    created: &mut bool,
) -> Result<(), String> {
    let mut current = start.trim().to_string();
    for _ in 0..=MAX_REDIRECTS {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let scheme = url::http_scheme(&current)
            .ok_or_else(|| format!("Kipimo only downloads http and https links ({current})"))?;
        let proxy = if opts.use_system_proxy {
            let key = format!(
                "{scheme}://{}",
                url::authority(&current).unwrap_or_default()
            );
            crate::proxy::system_proxy_for(&key)
        } else {
            None
        };
        let proxy = match proxy {
            Some(p) => Some(ureq::Proxy::new(&p).map_err(|_| format!("invalid system proxy {p}"))?),
            None => None,
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(opts.connect_timeout))
            .timeout_recv_response(Some(opts.stall_timeout))
            .timeout_per_read(Some(opts.stall_timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(concat!("Kipimo/", env!("CARGO_PKG_VERSION")))
            .proxy(proxy)
            .build()
            .into();
        log::debug!("download GET {current}");
        let mut resp = agent
            .get(&current)
            .call()
            .map_err(|e| format!("couldn't reach {}: {e}", host_of(&current)))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("a redirect without a location (HTTP {status})"))?;
            current = url::resolve(&current, location).ok_or_else(|| {
                format!("a redirect to an address Kipimo won't fetch ({location})")
            })?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("{} answered with HTTP {status}", host_of(&current)));
        }
        let total = resp.body().content_length();
        if total.is_some_and(|t| t > opts.max_bytes) {
            return Err("the download is larger than Kipimo accepts".into());
        }
        let mut reader = resp.body_mut().with_config().limit(opts.max_bytes).reader();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .map_err(|e| format!("couldn't create the download file: {e}"))?;
        *created = true;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("the download stopped: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| format!("couldn't save the download: {e}"))?;
            done += n as u64;
            progress(done, total);
        }
        if total.is_some_and(|t| t != done) {
            return Err("the download ended early".into());
        }
        file.sync_all()
            .map_err(|e| format!("couldn't save the download: {e}"))?;
        return Ok(());
    }
    Err("the download link redirects too many times".into())
}

fn host_of(u: &str) -> String {
    url::host(u).unwrap_or_else(|| "the server".into())
}
