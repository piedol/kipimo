//! Running the checks: grouping apps by feed, a small worker pool, caching, streaming results.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cache::{self, Cache};
use crate::finding::{Env, Failure, Finding, evaluate};
use crate::net::{Http, HttpConfig};
use crate::types::{
    AppUpdate, ElectronFeed, InstalledApp, Progress, SelfUpdater, Status, UpdateReport,
    UpdateSource, UpdaterHint,
};
use crate::{appcast, appstore, electron, system, vscode};

/// How to run a check.
#[derive(Clone, Debug)]
pub struct CheckOptions {
    /// Ignore cached results and read every feed again.
    pub force: bool,
    /// Answer only from the cache, whatever its age, and never go online. Apps with no cached
    /// answer come back as `CheckFailed` ("not checked yet"). Overrides `force`.
    pub offline: bool,
    /// JSON cache file (e.g. `~/Library/Caches/<Kipimo's bundle id>/updates.json`).
    /// `None` disables caching.
    pub cache_path: Option<PathBuf>,
    /// A feed is read again only when its cached result is older than this. Default 24 h.
    pub max_cache_age: Duration,
    /// Sparkle channels to accept besides the default one (e.g. "beta"). Default: none, so
    /// beta releases are ignored.
    pub sparkle_channels: Vec<String>,
    /// App Store region (ISO code). Default: the Mac's region.
    pub store_country: Option<String>,
    /// Override this Mac's macOS version ("26.6.2"); for tests.
    pub macos_version: Option<String>,
    /// Override this Mac's Darwin version ("25.6.0"); for tests.
    pub darwin_version: Option<String>,
    /// Requests in flight at once. Default 4.
    pub concurrency: usize,
    /// Whole-request timeout. Default 20 s.
    pub timeout: Duration,
    pub connect_timeout: Duration,
    /// Minimum gap between requests to the same host. Default 1 s.
    pub per_host_interval: Duration,
    /// Use the proxy from System Settings. Default true.
    pub use_system_proxy: bool,
    /// Largest feed accepted. Default 8 MiB.
    pub max_response_bytes: u64,
    /// The iTunes lookup endpoint; overridable for tests.
    pub itunes_lookup_url: String,
    /// github.com; overridable for tests.
    pub github_url: String,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            force: false,
            offline: false,
            cache_path: None,
            max_cache_age: Duration::from_secs(24 * 3600),
            sparkle_channels: Vec::new(),
            store_country: None,
            macos_version: None,
            darwin_version: None,
            concurrency: 4,
            timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(8),
            per_host_interval: Duration::from_secs(1),
            use_system_proxy: true,
            max_response_bytes: 8 * 1024 * 1024,
            itunes_lookup_url: "https://itunes.apple.com/lookup".into(),
            github_url: "https://github.com".into(),
        }
    }
}

/// The cache file Kipimo uses by default: `~/Library/Caches/top.piedol.kipimo/updates.json`.
pub fn default_cache_path() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join("Library/Caches/top.piedol.kipimo/updates.json"))
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

enum Job {
    AppStore {
        ids: Vec<String>,
        keys: Vec<String>,
        ios_apps: bool,
    },
    Sparkle {
        url: String,
        key: String,
    },
    Electron {
        feed: ElectronFeed,
        key: String,
    },
    VsCode {
        url: String,
        quality: String,
        key: String,
    },
}

struct Ctx {
    http: Http,
    env: Env,
    stop: AtomicBool,
    itunes: String,
    github: String,
}

type JobResult = Vec<(String, Result<Finding, Failure>)>;

fn electron_key(feed: &ElectronFeed) -> String {
    match feed {
        ElectronFeed::GitHub {
            host,
            owner,
            repo,
            channel,
        } => {
            format!(
                "electron|github|{}|{owner}|{repo}|{channel}",
                host.as_deref().unwrap_or("github.com")
            )
        }
        ElectronFeed::Generic { url, channel } => format!("electron|generic|{url}|{channel}"),
    }
}

fn cache_key(app: &InstalledApp, env: &Env) -> Result<Option<String>, String> {
    Ok(match &app.source {
        UpdateSource::AppStore { ios_app } => {
            let id = app
                .bundle_id
                .as_deref()
                .ok_or("the app has no bundle identifier")?;
            Some(format!(
                "appstore|{}|{}|{}",
                env.country,
                if *ios_app { "ios" } else { "mac" },
                id.to_ascii_lowercase()
            ))
        }
        UpdateSource::Sparkle { feed_url, .. } => {
            Some(format!("sparkle|{feed_url}|{}", env.channels.join(",")))
        }
        UpdateSource::ElectronUpdater { feed } => Some(electron_key(feed)),
        UpdateSource::VsCode {
            update_url,
            quality,
            platform,
        } => Some(format!("vscode|{update_url}|{platform}|{quality}")),
        UpdateSource::Unknown { .. } => None,
    })
}

fn run_job(job: &Job, ctx: &Ctx) -> JobResult {
    match job {
        Job::AppStore {
            ids,
            keys,
            ios_apps,
        } => {
            let url = appstore::lookup_url(&ctx.itunes, ids, &ctx.env.country, *ios_apps);
            let parsed = ctx
                .http
                .get(&url, &ctx.stop)
                .map_err(|e| Failure {
                    reason: e.reason("the App Store"),
                    transient: e.is_transient(),
                })
                .and_then(|f| {
                    // A garbled answer from Apple is usually a hiccup: retry next time.
                    appstore::parse_lookup(&f.body, ids, &ctx.env, *ios_apps).map_err(|reason| {
                        Failure {
                            reason,
                            transient: true,
                        }
                    })
                });
            match parsed {
                Err(e) => keys.iter().map(|k| (k.clone(), Err(e.clone()))).collect(),
                Ok(mut map) => ids
                    .iter()
                    .zip(keys)
                    .map(|(id, k)| {
                        let r = map
                            .remove(&id.to_ascii_lowercase())
                            .unwrap_or_else(|| Err("not in the App Store's answer".into()))
                            .map_err(Failure::definite);
                        (k.clone(), r)
                    })
                    .collect(),
            }
        }
        Job::Sparkle { url, key } => {
            let r = ctx
                .http
                .get(url, &ctx.stop)
                .map_err(feed_failure)
                .and_then(|f| {
                    let items = appcast::parse(&f.body)?;
                    if items.is_empty() {
                        return Err(Failure::definite("the update feed lists no releases"));
                    }
                    Ok(appcast::select(&items, &ctx.env, &f.url))
                });
            vec![(key.clone(), r)]
        }
        Job::VsCode { url, quality, key } => {
            let r = ctx
                .http
                .get(url, &ctx.stop)
                .map_err(feed_failure)
                .and_then(|f| Ok(vscode::parse(&f.body, quality)?));
            vec![(key.clone(), r)]
        }
        Job::Electron { feed, key } => {
            let url = electron::feed_url(feed, &ctx.github);
            let r = ctx
                .http
                .get(&url, &ctx.stop)
                .map_err(feed_failure)
                .and_then(|f| {
                    // A private repository redirects to GitHub's sign-in or SSO page.
                    let to_sign_in = f.chain.iter().skip(1).any(|u| {
                        let path = u.split('?').next().unwrap_or(u);
                        path.ends_with("/login") || path.ends_with("/sso")
                    });
                    if matches!(feed, ElectronFeed::GitHub { .. }) && to_sign_in {
                        return Err(Failure::definite(
                            "the GitHub repository is private or needs a sign-in",
                        ));
                    }
                    let release_page = match feed {
                        ElectronFeed::GitHub {
                            host, owner, repo, ..
                        } => electron::github_tag(&f.chain).map(|tag| {
                            let base = match host {
                                Some(h) => format!("https://{h}"),
                                None => ctx.github.trim_end_matches('/').to_string(),
                            };
                            format!("{base}/{owner}/{repo}/releases/tag/{tag}")
                        }),
                        ElectronFeed::Generic { .. } => None,
                    };
                    Ok(electron::parse_latest(
                        &f.body,
                        &f.url,
                        release_page,
                        &ctx.env,
                    )?)
                });
            vec![(key.clone(), r)]
        }
    }
}

fn feed_failure(e: crate::net::FetchError) -> Failure {
    Failure {
        reason: e.reason("the update feed"),
        transient: e.is_transient(),
    }
}

fn job_keys(job: &Job) -> Vec<String> {
    match job {
        Job::AppStore { keys, .. } => keys.clone(),
        Job::Sparkle { key, .. } | Job::Electron { key, .. } | Job::VsCode { key, .. } => {
            vec![key.clone()]
        }
    }
}

fn hint(app: &InstalledApp, status: &Status) -> Option<UpdaterHint> {
    let name = &app.name;
    let text = |t: String| {
        Some(UpdaterHint {
            text: t,
            open_url: None,
        })
    };
    let software_update = || UpdaterHint {
        text: "Update macOS in System Settings › General › Software Update.".into(),
        open_url: Some(
            "x-apple.systempreferences:com.apple.Software-Update-Settings.extension".into(),
        ),
    };
    let actionable = matches!(status, Status::UpdateAvailable | Status::CheckFailed { .. });
    match (&app.source, status) {
        (_, Status::RequiresNewerMacos { .. }) => Some(software_update()),
        (UpdateSource::AppStore { .. }, _) if actionable => Some(UpdaterHint {
            text: "Update it from the App Store's Updates page.".into(),
            open_url: Some("macappstore://showUpdatesPage".into()),
        }),
        (UpdateSource::Sparkle { .. }, _) if actionable => text(format!(
            "{name} can install it itself: choose Check for Updates… in its app menu."
        )),
        (UpdateSource::VsCode { .. }, _) if actionable => text(format!(
            "{name} can install it itself: choose {name} › Check for Updates…, or restart it."
        )),
        (UpdateSource::ElectronUpdater { .. }, _) if actionable => text(format!(
            "{name} installs updates itself, usually when you quit and reopen it."
        )),
        (UpdateSource::Unknown { updater: Some(u) }, _) => match u {
            SelfUpdater::Sparkle => text(format!(
                "{name} checks for updates itself: choose Check for Updates… in its app menu."
            )),
            SelfUpdater::ElectronUpdater { .. } | SelfUpdater::Squirrel => text(format!(
                "{name} updates itself, usually when you quit and reopen it, or from its menu."
            )),
            SelfUpdater::GoogleKeystone => text(format!(
                "{name} updates itself: open About {name} in its app menu to check."
            )),
            SelfUpdater::Mozilla => text(format!(
                "{name} updates itself: choose {name} › About {name} to check."
            )),
            SelfUpdater::MicrosoftAutoUpdate => text(format!(
                "Choose Help › Check for Updates in {name} (Microsoft AutoUpdate)."
            )),
            SelfUpdater::MacOS => Some(UpdaterHint {
                text: format!(
                    "{name} is updated with macOS in System Settings › General › Software Update."
                ),
                ..software_update()
            }),
        },
        _ => None,
    }
}

fn make_update(
    app: &InstalledApp,
    result: Result<&Finding, &str>,
    checked_at: u64,
    from_cache: bool,
) -> AppUpdate {
    let (status, latest, newer, critical) = match result {
        Ok(f) => {
            let e = evaluate(app, f);
            (e.status, e.latest, e.newer_requires_macos, e.critical)
        }
        Err(reason) => (
            Status::CheckFailed {
                reason: reason.to_string(),
            },
            None,
            None,
            false,
        ),
    };
    AppUpdate {
        hint: hint(app, &status),
        app: app.clone(),
        status,
        latest,
        newer_requires_macos: newer,
        critical,
        checked_at,
        from_cache,
    }
}

fn status_rank(s: &Status) -> u8 {
    match s {
        Status::UpdateAvailable => 0,
        Status::RequiresNewerMacos { .. } => 1,
        Status::UpToDate => 2,
        Status::CheckFailed { .. } => 3,
        Status::UnknownSource => 4,
    }
}

/// Checks the given apps for updates. Network work happens on up to `concurrency` worker
/// threads; `progress` is called on the calling thread as each app's result is known.
/// Setting `cancel` stops within about 100 ms: requests already in flight finish in the
/// background (bounded by the timeout) and their results are dropped.
pub fn check_updates(
    apps: &[InstalledApp],
    opts: &CheckOptions,
    mut progress: impl FnMut(Progress<'_>),
    cancel: &AtomicBool,
) -> UpdateReport {
    let started = Instant::now();
    let started_at = now_secs();
    let env = Env {
        macos: opts
            .macos_version
            .clone()
            .unwrap_or_else(system::macos_version),
        darwin: opts
            .darwin_version
            .clone()
            .unwrap_or_else(system::darwin_version),
        arm64: system::is_arm64(),
        channels: opts
            .sparkle_channels
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect(),
        country: opts
            .store_country
            .clone()
            .map(|c| c.to_ascii_lowercase())
            .unwrap_or_else(system::region),
    };
    // Loaded even when forced, so entries for feeds not checked this time survive the save.
    let mut cache = opts
        .cache_path
        .as_deref()
        .map(Cache::load)
        .unwrap_or_default();
    let max_age = if opts.offline {
        u64::MAX
    } else {
        opts.max_cache_age.as_secs()
    };
    let total = apps.len();
    let mut results: Vec<Option<AppUpdate>> = vec![None; total];
    let mut done = 0;
    let mut emit = |idx: usize, update: AppUpdate, results: &mut Vec<Option<AppUpdate>>| {
        done += 1;
        progress(Progress {
            done,
            total,
            update: &update,
        });
        results[idx] = Some(update);
    };

    // Answer what we can without the network; group the rest by feed.
    let mut waiting: HashMap<String, Vec<usize>> = HashMap::new();
    let mut jobs: VecDeque<Job> = VecDeque::new();
    let mut store_batches: [Vec<(String, String)>; 2] = [Vec::new(), Vec::new()];
    for (idx, app) in apps.iter().enumerate() {
        let key = match cache_key(app, &env) {
            Err(reason) => {
                emit(
                    idx,
                    make_update(app, Err(&reason), started_at, false),
                    &mut results,
                );
                continue;
            }
            Ok(None) => {
                let status = Status::UnknownSource;
                let update = AppUpdate {
                    hint: hint(app, &status),
                    app: app.clone(),
                    status,
                    latest: None,
                    newer_requires_macos: None,
                    critical: false,
                    checked_at: 0,
                    from_cache: false,
                };
                emit(idx, update, &mut results);
                continue;
            }
            Ok(Some(k)) => k,
        };
        if (!opts.force || opts.offline)
            && let Some(entry) = cache.fresh(&key, started_at, max_age, &env.macos)
        {
            let update = make_update(
                app,
                entry.outcome.as_ref().map_err(String::as_str),
                entry.checked_at,
                true,
            );
            emit(idx, update, &mut results);
            continue;
        }
        let first = !waiting.contains_key(&key);
        waiting.entry(key.clone()).or_default().push(idx);
        if !first {
            continue;
        }
        match &app.source {
            UpdateSource::AppStore { ios_app } => {
                let id = app.bundle_id.clone().unwrap_or_default();
                store_batches[usize::from(*ios_app)].push((id, key));
            }
            UpdateSource::Sparkle { feed_url, .. } => jobs.push_back(Job::Sparkle {
                url: feed_url.clone(),
                key,
            }),
            UpdateSource::ElectronUpdater { feed } => jobs.push_back(Job::Electron {
                feed: feed.clone(),
                key,
            }),
            UpdateSource::VsCode {
                update_url,
                quality,
                platform,
            } => jobs.push_back(Job::VsCode {
                url: vscode::latest_url(update_url, platform, quality),
                quality: quality.clone(),
                key,
            }),
            UpdateSource::Unknown { .. } => {}
        }
    }
    if opts.offline {
        let mut left: Vec<usize> = waiting.drain().flat_map(|(_, v)| v).collect();
        left.sort_unstable();
        for idx in left {
            let update = make_update(&apps[idx], Err("not checked yet"), 0, false);
            emit(idx, update, &mut results);
        }
        store_batches = [Vec::new(), Vec::new()];
        jobs.clear();
    }
    for (ios, batch) in store_batches.iter().enumerate() {
        for chunk in batch.chunks(appstore::BATCH) {
            // The App Store goes first: one request answers many apps.
            jobs.push_front(Job::AppStore {
                ids: chunk.iter().map(|(id, _)| id.clone()).collect(),
                keys: chunk.iter().map(|(_, k)| k.clone()).collect(),
                ios_apps: ios == 1,
            });
        }
    }

    let mut cancelled = false;
    let mut requests = 0;
    let needs_network = !jobs.is_empty();
    if needs_network && !cancel.load(Ordering::Relaxed) {
        let ctx = Arc::new(Ctx {
            http: Http::new(HttpConfig {
                timeout: opts.timeout,
                connect_timeout: opts.connect_timeout,
                per_host_interval: opts.per_host_interval,
                use_system_proxy: opts.use_system_proxy,
                max_body: opts.max_response_bytes,
                user_agent: concat!("Kipimo/", env!("CARGO_PKG_VERSION")).to_string(),
            }),
            env: env.clone(),
            stop: AtomicBool::new(false),
            itunes: opts.itunes_lookup_url.clone(),
            github: opts.github_url.clone(),
        });
        let workers = opts.concurrency.clamp(1, 16).min(jobs.len());
        let queue = Arc::new(Mutex::new(jobs));
        let (tx, rx) = mpsc::channel::<(u64, JobResult)>();
        for n in 0..workers {
            let (ctx, queue, tx) = (Arc::clone(&ctx), Arc::clone(&queue), tx.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("kipimo-updates-{n}"))
                .spawn(move || {
                    loop {
                        if ctx.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Some(job) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
                        else {
                            break;
                        };
                        // A parser bug in one feed must not take the other checks down.
                        let out =
                            std::panic::catch_unwind(AssertUnwindSafe(|| run_job(&job, &ctx)))
                                .unwrap_or_else(|_| {
                                    log::error!("updates: a check panicked");
                                    let failure = Failure {
                                        reason: "the check failed unexpectedly".into(),
                                        transient: true,
                                    };
                                    job_keys(&job)
                                        .into_iter()
                                        .map(|k| (k, Err(failure.clone())))
                                        .collect()
                                });
                        if tx.send((now_secs(), out)).is_err() {
                            break;
                        }
                    }
                });
            if let Err(e) = spawned {
                log::error!("updates: couldn't start a worker: {e}");
            }
        }
        drop(tx);
        loop {
            if cancel.load(Ordering::Relaxed) {
                ctx.stop.store(true, Ordering::Relaxed);
                cancelled = true;
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((checked_at, out)) => {
                    for (key, result) in out {
                        // Findings and definite failures are cached; network trouble isn't.
                        let cacheable = result.as_ref().map_or_else(|f| !f.transient, |_| true);
                        let outcome = result.map_err(|f| f.reason);
                        if cacheable {
                            let entry = cache::Entry {
                                checked_at,
                                macos: env.macos.clone(),
                                outcome: outcome.clone(),
                            };
                            cache.put(key.clone(), entry);
                        }
                        for idx in waiting.remove(&key).unwrap_or_default() {
                            let update = make_update(
                                &apps[idx],
                                outcome.as_ref().map_err(String::as_str),
                                checked_at,
                                false,
                            );
                            emit(idx, update, &mut results);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        requests = ctx.http.requests();
    } else if needs_network {
        cancelled = true;
    }

    if !cancelled {
        // Anything still waiting lost its worker (it shouldn't happen).
        let mut left: Vec<usize> = waiting.into_values().flatten().collect();
        left.sort_unstable();
        for idx in left {
            let update = make_update(
                &apps[idx],
                Err("the check didn't finish"),
                now_secs(),
                false,
            );
            emit(idx, update, &mut results);
        }
    }
    if let Some(path) = opts.cache_path.as_ref().filter(|_| !opts.offline)
        && let Err(e) = cache.save(path, now_secs())
    {
        log::warn!(
            "updates: couldn't save the cache at {}: {e}",
            path.display()
        );
    }

    let mut apps_out: Vec<AppUpdate> = results.into_iter().flatten().collect();
    apps_out.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| a.app.name.to_lowercase().cmp(&b.app.name.to_lowercase()))
    });
    UpdateReport {
        apps: apps_out,
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        cancelled,
        store_country: env.country,
        macos_version: env.macos,
        requests,
    }
}
