//! HTTP GETs for update checks: ureq with rustls, short timeouts, a size cap, manual
//! redirects (so each hop is rate limited and GitHub release tags can be read from them), a
//! per-host rate limit and the macOS system proxy.
//!
//! Requests carry only what an ordinary update check sends: a generic `User-Agent`
//! (`Kipimo/<version>`), `Accept`, and `Accept-Encoding: gzip`. No cookies, no device or
//! system profile, no query parameters beyond the feed's own.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::url;

const MAX_REDIRECTS: usize = 6;

#[derive(Clone, Debug)]
pub(crate) struct HttpConfig {
    /// Whole-request limit (connect, send, receive).
    pub timeout: Duration,
    pub connect_timeout: Duration,
    /// Minimum gap between two requests to the same host.
    pub per_host_interval: Duration,
    pub use_system_proxy: bool,
    /// Largest response body accepted, in bytes.
    pub max_body: u64,
    pub user_agent: String,
}

#[derive(Debug)]
pub(crate) struct Fetched {
    /// The URL that finally answered.
    pub url: String,
    /// Every URL requested, in order (the first is the original, the last is `url`).
    pub chain: Vec<String>,
    pub body: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FetchError {
    Cancelled,
    Status(u16),
    Network(String),
    TooLarge,
    TooManyRedirects,
    BadUrl(String),
}

impl FetchError {
    /// Worth retrying soon (network trouble, server errors, rate limiting), as opposed to a
    /// definite answer (not found, bad address) that is cached like a success.
    pub fn is_transient(&self) -> bool {
        match self {
            FetchError::Cancelled | FetchError::Network(_) => true,
            FetchError::Status(code) => *code >= 500 || matches!(code, 408 | 425 | 429),
            FetchError::TooLarge | FetchError::TooManyRedirects | FetchError::BadUrl(_) => false,
        }
    }

    /// A short sentence for the UI.
    pub fn reason(&self, what: &str) -> String {
        match self {
            FetchError::Cancelled => "cancelled".into(),
            FetchError::Status(404 | 410) => format!("{what} wasn't found (HTTP 404)"),
            FetchError::Status(429) => format!("{what} asked Kipimo to slow down (HTTP 429)"),
            FetchError::Status(code) => format!("{what} answered with HTTP {code}"),
            FetchError::Network(e) => format!("couldn't reach {what}: {e}"),
            FetchError::TooLarge => format!("{what} is unexpectedly large"),
            FetchError::TooManyRedirects => format!("{what} redirects too many times"),
            FetchError::BadUrl(u) => format!("{what} has an address Kipimo won't fetch ({u})"),
        }
    }
}

/// Spaces requests to the same host at least `interval` apart.
struct HostLimiter {
    interval: Duration,
    next: Mutex<HashMap<String, Instant>>,
}

impl HostLimiter {
    fn wait(&self, host: &str, cancel: &AtomicBool) -> Result<(), FetchError> {
        let start = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let slot = next.entry(host.to_string()).or_insert(now);
            let start = (*slot).max(now);
            *slot = start + self.interval;
            start
        };
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(FetchError::Cancelled);
            }
            let now = Instant::now();
            if now >= start {
                return Ok(());
            }
            std::thread::sleep((start - now).min(Duration::from_millis(50)));
        }
    }
}

pub(crate) struct Http {
    cfg: HttpConfig,
    /// One agent per proxy (None = direct); agents pool their connections.
    agents: Mutex<HashMap<Option<String>, ureq::Agent>>,
    /// Proxy decision per "scheme://authority".
    proxies: Mutex<HashMap<String, Option<String>>>,
    limiter: HostLimiter,
    requests: AtomicUsize,
}

fn network_error(e: ureq::Error) -> FetchError {
    match e {
        ureq::Error::StatusCode(code) => FetchError::Status(code),
        ureq::Error::BodyExceedsLimit(_) => FetchError::TooLarge,
        ureq::Error::TooManyRedirects => FetchError::TooManyRedirects,
        ureq::Error::Timeout(_) => FetchError::Network("timed out".into()),
        ureq::Error::HostNotFound => FetchError::Network("host not found".into()),
        ureq::Error::ConnectionFailed => FetchError::Network("connection failed".into()),
        ureq::Error::Io(io) => FetchError::Network(io.kind().to_string().replace('_', " ")),
        other => FetchError::Network(other.to_string()),
    }
}

impl Http {
    pub fn new(cfg: HttpConfig) -> Self {
        let limiter = HostLimiter {
            interval: cfg.per_host_interval,
            next: Mutex::new(HashMap::new()),
        };
        Http {
            cfg,
            agents: Mutex::new(HashMap::new()),
            proxies: Mutex::new(HashMap::new()),
            limiter,
            requests: AtomicUsize::new(0),
        }
    }

    /// Number of requests sent so far.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    fn proxy_for(&self, target: &str) -> Option<String> {
        if !self.cfg.use_system_proxy {
            return None;
        }
        let key = format!(
            "{}://{}",
            url::http_scheme(target).unwrap_or("https"),
            url::authority(target).unwrap_or_default()
        );
        let mut map = self.proxies.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key.clone())
            .or_insert_with(|| crate::proxy::system_proxy_for(&key))
            .clone()
    }

    fn agent_for(&self, target: &str) -> Result<ureq::Agent, FetchError> {
        let proxy_uri = self.proxy_for(target);
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = agents.get(&proxy_uri) {
            return Ok(a.clone());
        }
        let proxy = match &proxy_uri {
            Some(p) => Some(
                ureq::Proxy::new(p)
                    .map_err(|_| FetchError::Network(format!("invalid system proxy {p}")))?,
            ),
            None => None,
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(self.cfg.timeout))
            .timeout_connect(Some(self.cfg.connect_timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(self.cfg.user_agent.as_str())
            .proxy(proxy)
            .max_idle_connections(4)
            .max_idle_connections_per_host(1)
            .max_idle_age(Duration::from_secs(15))
            .build()
            .into();
        agents.insert(proxy_uri, agent.clone());
        Ok(agent)
    }

    /// GET `start`, following redirects. Only http(s) URLs are fetched.
    pub fn get(&self, start: &str, cancel: &AtomicBool) -> Result<Fetched, FetchError> {
        let mut current = start.trim().to_string();
        let mut chain = Vec::new();
        for _ in 0..=MAX_REDIRECTS {
            if url::http_scheme(&current).is_none() {
                return Err(FetchError::BadUrl(current));
            }
            let host = url::host(&current).ok_or_else(|| FetchError::BadUrl(current.clone()))?;
            chain.push(current.clone());
            self.limiter.wait(&host, cancel)?;
            let agent = self.agent_for(&current)?;
            self.requests.fetch_add(1, Ordering::Relaxed);
            log::debug!("GET {current}");
            let mut resp = agent
                .get(&current)
                .header(
                    "Accept",
                    "application/rss+xml, application/xml, application/json, text/yaml, */*;q=0.1",
                )
                .call()
                .map_err(network_error)?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) && status != 304 {
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                current = match location {
                    None => {
                        return Err(FetchError::Network(format!(
                            "redirect without a location (HTTP {status})"
                        )));
                    }
                    Some(l) => url::resolve(&current, &l).ok_or(FetchError::BadUrl(l))?,
                };
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(FetchError::Status(status));
            }
            let body = resp
                .body_mut()
                .with_config()
                .limit(self.cfg.max_body)
                .read_to_vec()
                .map_err(network_error)?;
            return Ok(Fetched {
                url: current,
                chain,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Err(FetchError::TooManyRedirects)
    }
}
