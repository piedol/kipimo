//! Test helpers: a temporary folder, synthetic app bundles and a tiny local HTTP server.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plist::Value;

/// A folder under the system temp dir, removed when dropped. Never the real home folder.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kipimo-updates-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates `<dir>/<name>.app` with an XML Info.plist holding `info` (string values).
pub fn make_app(dir: &Path, name: &str, info: &[(&str, &str)]) -> PathBuf {
    let app = dir.join(format!("{name}.app"));
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    let mut d = plist::Dictionary::new();
    for (k, v) in info {
        d.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    Value::Dictionary(d)
        .to_file_xml(app.join("Contents/Info.plist"))
        .unwrap();
    app
}

/// A basic app with an ID and versions.
pub fn app_with(
    dir: &Path,
    name: &str,
    id: &str,
    version: &str,
    build: &str,
    extra: &[(&str, &str)],
) -> PathBuf {
    let mut info = vec![
        ("CFBundleIdentifier", id),
        ("CFBundleShortVersionString", version),
        ("CFBundleVersion", build),
    ];
    info.extend_from_slice(extra);
    make_app(dir, name, &info)
}

pub fn add_mas_receipt(app: &Path) {
    let dir = app.join("Contents/_MASReceipt");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("receipt"), b"not a real receipt").unwrap();
}

pub fn add_file(app: &Path, rel: &str, contents: &str) {
    let p = app.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

pub fn add_framework(app: &Path, name: &str) {
    fs::create_dir_all(
        app.join("Contents/Frameworks")
            .join(format!("{name}.framework")),
    )
    .unwrap();
}

pub fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[derive(Clone)]
pub struct Route {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay: Duration,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub at: Instant,
    /// Path and query, e.g. "/lookup?bundleId=a".
    pub target: String,
    /// Header names lower case.
    pub headers: Vec<(String, String)>,
}

impl Hit {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A minimal HTTP/1.1 server on 127.0.0.1 that answers from a route table and records
/// every request. Each connection gets one response and is closed.
pub struct TestServer {
    pub base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl TestServer {
    pub fn start() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let (r, h) = (Arc::clone(&routes), Arc::clone(&hits));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (r, h) = (Arc::clone(&r), Arc::clone(&h));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut headers = Vec::new();
                    loop {
                        let mut l = String::new();
                        if reader.read_line(&mut l).is_err() || l == "\r\n" || l.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.trim_end().split_once(':') {
                            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                        }
                    }
                    h.lock().unwrap().push(Hit {
                        at: Instant::now(),
                        target: target.clone(),
                        headers,
                    });
                    let route = {
                        let routes = r.lock().unwrap();
                        let path = target.split('?').next().unwrap_or("/");
                        routes.get(&target).or_else(|| routes.get(path)).cloned()
                    };
                    let route = route.unwrap_or(Route {
                        status: 404,
                        headers: vec![],
                        body: b"not found".to_vec(),
                        delay: Duration::ZERO,
                    });
                    std::thread::sleep(route.delay);
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        route.status,
                        route.body.len()
                    );
                    for (k, v) in &route.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&route.body);
                    let _ = stream.flush();
                });
            }
        });
        TestServer { base, routes, hits }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub fn ok(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.route(
            path,
            Route {
                status: 200,
                headers: vec![],
                body: body.into(),
                delay: Duration::ZERO,
            },
        );
    }

    pub fn slow(&self, path: &str, body: impl Into<Vec<u8>>, delay: Duration) {
        self.route(
            path,
            Route {
                status: 200,
                headers: vec![],
                body: body.into(),
                delay,
            },
        );
    }

    pub fn redirect(&self, path: &str, status: u16, location: &str) {
        self.route(
            path,
            Route {
                status,
                headers: vec![("Location".into(), location.into())],
                body: vec![],
                delay: Duration::ZERO,
            },
        );
    }

    pub fn status(&self, path: &str, status: u16) {
        self.route(
            path,
            Route {
                status,
                headers: vec![],
                body: b"nope".to_vec(),
                delay: Duration::ZERO,
            },
        );
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    pub fn hits_for(&self, prefix: &str) -> Vec<Hit> {
        self.hits()
            .into_iter()
            .filter(|h| h.target.starts_with(prefix))
            .collect()
    }
}
