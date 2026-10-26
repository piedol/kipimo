//! The whole check against a local HTTP server: App Store lookups, Sparkle and
//! electron-updater feeds, redirects, failures, caching, rate limiting, timeouts, cancel.

mod common;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use kipimo_updates::{
    AppUpdate, CheckOptions, DiscoverOptions, Status, UpdateReport, check_updates, discover_apps,
};

fn options(server: &TestServer, cache: Option<&Path>) -> CheckOptions {
    CheckOptions {
        cache_path: cache.map(Path::to_path_buf),
        store_country: Some("GB".into()),
        macos_version: Some("26.6.2".into()),
        darwin_version: Some("25.6.0".into()),
        per_host_interval: Duration::ZERO,
        timeout: Duration::from_secs(5),
        use_system_proxy: false,
        itunes_lookup_url: server.url("/lookup"),
        github_url: server.base.clone(),
        ..CheckOptions::default()
    }
}

fn find<'a>(r: &'a UpdateReport, name: &str) -> &'a AppUpdate {
    r.apps
        .iter()
        .find(|a| a.app.name == name)
        .unwrap_or_else(|| panic!("{name} missing from report"))
}

fn run(dir: &Path, opts: &CheckOptions) -> (UpdateReport, Vec<(usize, usize, String)>) {
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![dir.to_path_buf()],
        home: None,
    });
    let mut seen = Vec::new();
    let cancel = AtomicBool::new(false);
    let report = check_updates(
        &apps,
        opts,
        |p| seen.push((p.done, p.total, p.update.app.name.clone())),
        &cancel,
    );
    (report, seen)
}

const GH_YML: &str = "version: 3.7.21
files:
  - url: Thing-3.7.21-universal.zip
    size: 1
  - url: Thing-3.7.21-universal.dmg
    size: 1
path: Thing-3.7.21-universal.zip
releaseDate: '2026-09-25T21:28:37.326Z'
";

const GENERIC_YML: &str = "version: 8.32.0
files:
  - url: chat-mac-x64-8.32.0.zip
    size: 1
  - url: chat-mac-arm64-8.32.0.zip
    size: 1
path: chat-mac-x64-8.32.0.zip
vendor:
  minOSVersion: 22.1.0
releaseDate: '2026-10-20T17:43:00.437Z'
";

fn setup(server: &TestServer, dir: &Path) {
    server.ok("/lookup", fixture("itunes_lookup.json"));
    server.ok("/appcast.xml", fixture("appcast_multi.xml"));
    server.redirect("/moved.xml", 301, "/appcast.xml");
    server.status("/missing.xml", 404);
    server.ok("/html.xml", fixture("not_an_appcast.html"));
    server.ok("/chat/latest-mac.yml", GENERIC_YML);
    server.redirect(
        "/acme/thing/releases/latest/download/latest-mac.yml",
        302,
        "/acme/thing/releases/download/v3.7.21/latest-mac.yml",
    );
    server.ok(
        "/acme/thing/releases/download/v3.7.21/latest-mac.yml",
        GH_YML,
    );

    for (name, id, v) in [
        ("Store App", "com.example.storeapp", "2.4.0"),
        ("Current", "com.example.current", "1.0"),
        ("Needs New OS", "com.example.NeedsNewOS", "4.0"),
        ("Gone", "com.example.gone", "1.0"),
    ] {
        let app = app_with(dir, name, id, v, v, &[]);
        add_mas_receipt(&app);
    }
    let feed = server.url("/appcast.xml");
    app_with(
        dir,
        "Sparkly",
        "com.example.sparkly",
        "2.1",
        "210",
        &[("SUFeedURL", &feed)],
    );
    app_with(
        dir,
        "Sparkly Twin",
        "com.example.twin",
        "2.3",
        "230",
        &[("SUFeedURL", &feed)],
    );
    app_with(
        dir,
        "Moved",
        "com.example.moved",
        "2.3",
        "230",
        &[("SUFeedURL", &server.url("/moved.xml"))],
    );
    app_with(
        dir,
        "Missing Feed",
        "com.example.missing",
        "1.0",
        "1",
        &[("SUFeedURL", &server.url("/missing.xml"))],
    );
    app_with(
        dir,
        "Html Feed",
        "com.example.html",
        "1.0",
        "1",
        &[("SUFeedURL", &server.url("/html.xml"))],
    );
    let chat = app_with(dir, "Chat", "com.example.chat", "8.31.2", "8.31.2", &[]);
    add_file(
        &chat,
        "Contents/Resources/app-update.yml",
        &format!("provider: generic\nurl: {}\n", server.url("/chat")),
    );
    let thing = app_with(dir, "Thing", "com.example.thing", "3.7.21", "3.7.21", &[]);
    add_file(
        &thing,
        "Contents/Resources/app-update.yml",
        "owner: acme\nrepo: thing\nprovider: github\n",
    );
    let squirrel = app_with(dir, "Mystery", "com.example.mystery", "1.0", "1", &[]);
    add_framework(&squirrel, "Squirrel");
}

#[test]
fn end_to_end_statuses() {
    let server = TestServer::start();
    let tmp = TempDir::new("e2e");
    let dir = tmp.path().join("Applications");
    fs::create_dir_all(&dir).unwrap();
    setup(&server, &dir);

    let (report, seen) = run(&dir, &options(&server, None));
    assert!(!report.cancelled);
    assert_eq!(report.apps.len(), 12);
    assert_eq!(report.store_country, "gb");
    assert_eq!(report.macos_version, "26.6.2");

    // Progress: once per app, counting up to the total.
    assert_eq!(seen.len(), 12);
    assert!(
        seen.iter()
            .enumerate()
            .all(|(i, (done, total, _))| *done == i + 1 && *total == 12)
    );

    let store = find(&report, "Store App");
    assert_eq!(store.status, Status::UpdateAvailable);
    assert_eq!(store.latest_version(), Some("2.4.1"));
    let latest = store.latest.as_ref().unwrap();
    assert_eq!(
        latest.download_url.as_deref(),
        Some("macappstore://apps.apple.com/app/id111111?mt=12")
    );
    assert_eq!(
        store.hint.as_ref().unwrap().open_url.as_deref(),
        Some("macappstore://showUpdatesPage")
    );
    assert_eq!(find(&report, "Current").status, Status::UpToDate);
    assert_eq!(
        find(&report, "Needs New OS").status,
        Status::RequiresNewerMacos {
            minimum: "99.0".into()
        }
    );
    assert_eq!(
        find(&report, "Gone").status,
        Status::CheckFailed {
            reason: "not found in the App Store (GB region)".into()
        }
    );

    let sparkly = find(&report, "Sparkly");
    assert_eq!(sparkly.status, Status::UpdateAvailable);
    assert_eq!(sparkly.latest_version(), Some("2.3"));
    assert_eq!(
        sparkly
            .latest
            .as_ref()
            .unwrap()
            .release_notes_url
            .as_deref(),
        Some("https://example.test/notes/2.3.html")
    );
    assert_eq!(
        sparkly.latest.as_ref().unwrap().download_url.as_deref(),
        Some("https://example.test/dl/Example-2.3.zip")
    );
    assert_eq!(
        sparkly
            .newer_requires_macos
            .as_ref()
            .unwrap()
            .version
            .as_deref(),
        Some("3.0")
    );
    assert!(
        sparkly
            .hint
            .as_ref()
            .unwrap()
            .text
            .contains("Check for Updates")
    );
    assert_eq!(
        find(&report, "Sparkly Twin").status,
        Status::RequiresNewerMacos {
            minimum: "99.0".into()
        }
    );
    assert_eq!(
        find(&report, "Moved").status,
        Status::RequiresNewerMacos {
            minimum: "99.0".into()
        }
    );
    match &find(&report, "Missing Feed").status {
        Status::CheckFailed { reason } => assert!(reason.contains("404"), "{reason}"),
        s => panic!("{s:?}"),
    }
    assert!(matches!(
        find(&report, "Html Feed").status,
        Status::CheckFailed { .. }
    ));

    // The download matching this Mac's architecture, never the other one.
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let chat = find(&report, "Chat");
    assert_eq!(chat.status, Status::UpdateAvailable);
    assert_eq!(chat.latest_version(), Some("8.32.0"));
    let chat_latest = chat.latest.as_ref().unwrap();
    assert_eq!(
        chat_latest.download_url.as_deref(),
        Some(
            server
                .url(&format!("/chat/chat-mac-{arch}-8.32.0.zip"))
                .as_str()
        )
    );
    assert_eq!(chat_latest.minimum_system_version.as_deref(), Some("13.0"));

    let thing = find(&report, "Thing");
    assert_eq!(thing.status, Status::UpToDate);
    let thing_latest = thing.latest.as_ref().unwrap();
    assert_eq!(
        thing_latest.release_notes_url.as_deref(),
        Some(server.url("/acme/thing/releases/tag/v3.7.21").as_str())
    );
    assert_eq!(
        thing_latest.download_url.as_deref(),
        Some(
            server
                .url("/acme/thing/releases/download/v3.7.21/Thing-3.7.21-universal.dmg")
                .as_str()
        )
    );

    let mystery = find(&report, "Mystery");
    assert_eq!(mystery.status, Status::UnknownSource);
    assert!(mystery.hint.is_some());

    // Updates sort first.
    assert_eq!(report.apps[0].status, Status::UpdateAvailable);
    assert_eq!(report.counts().update_available, 3);
    assert_eq!(report.updates().count(), 3);

    // One lookup for all four App Store apps, one request per shared feed.
    let lookups = server.hits_for("/lookup");
    assert_eq!(lookups.len(), 1);
    let q = &lookups[0].target;
    assert!(
        q.contains("country=gb") && q.contains("entity=desktopSoftware"),
        "{q}"
    );
    assert!(
        q.contains("com.example.storeapp") && q.contains("com.example.gone"),
        "{q}"
    );
    assert_eq!(
        server.hits_for("/appcast.xml").len(),
        2,
        "direct + via redirect"
    );
    assert_eq!(report.requests, server.hits().len());

    // Nothing identifying: a generic user agent, no cookies, no referrer.
    for hit in server.hits() {
        assert!(
            hit.header("user-agent")
                .is_some_and(|ua| ua.starts_with("Kipimo/")),
            "{hit:?}"
        );
        assert!(
            hit.header("cookie").is_none() && hit.header("referer").is_none(),
            "{hit:?}"
        );
    }
}

#[test]
fn cache_reuse_reevaluate_and_force() {
    let server = TestServer::start();
    let tmp = TempDir::new("cache");
    let dir = tmp.path().join("Applications");
    fs::create_dir_all(&dir).unwrap();
    setup(&server, &dir);
    server.status("/flaky.xml", 503);
    app_with(
        &dir,
        "Flaky",
        "com.example.flaky",
        "1.0",
        "1",
        &[("SUFeedURL", &server.url("/flaky.xml"))],
    );
    let cache = tmp.path().join("Caches/updates.json");
    let opts = options(&server, Some(&cache));

    let (first, _) = run(&dir, &opts);
    let first_hits = server.hits().len();
    assert!(first.requests > 0 && cache.exists());

    // Within a day: no network except to retry the server error; same answers, from the cache.
    let (second, _) = run(&dir, &opts);
    assert_eq!(second.requests, 1);
    assert_eq!(server.hits().len(), first_hits + 1);
    assert_eq!(server.hits_for("/flaky.xml").len(), 2);
    assert_eq!(find(&second, "Sparkly").status, Status::UpdateAvailable);
    assert!(find(&second, "Sparkly").from_cache);
    assert_eq!(
        find(&second, "Sparkly").checked_at,
        find(&first, "Sparkly").checked_at
    );
    // Definite failures (404, not in the store) are cached like answers; server errors aren't.
    assert!(find(&second, "Missing Feed").from_cache);
    assert_eq!(
        find(&second, "Missing Feed").status,
        find(&first, "Missing Feed").status
    );
    assert!(find(&second, "Gone").from_cache);
    assert!(!find(&second, "Flaky").from_cache);

    // The user updated the app: re-evaluated from the cache, still no request.
    app_with(
        &dir,
        "Sparkly",
        "com.example.sparkly",
        "2.3",
        "230",
        &[("SUFeedURL", &server.url("/appcast.xml"))],
    );
    let before = server.hits_for("/appcast.xml").len();
    let (third, _) = run(&dir, &opts);
    assert_eq!(
        find(&third, "Sparkly").status,
        Status::RequiresNewerMacos {
            minimum: "99.0".into()
        }
    );
    assert_eq!(server.hits_for("/appcast.xml").len(), before);

    // Forced: everything is read again.
    let (forced, _) = run(
        &dir,
        &CheckOptions {
            force: true,
            ..opts.clone()
        },
    );
    assert!(forced.apps.iter().all(|a| !a.from_cache));
    assert_eq!(server.hits_for("/lookup").len(), 2);

    // A different macOS version invalidates findings (compatibility depends on it).
    let (other_os, _) = run(
        &dir,
        &CheckOptions {
            macos_version: Some("99.1".into()),
            ..opts.clone()
        },
    );
    assert!(!find(&other_os, "Sparkly").from_cache);
    assert_eq!(find(&other_os, "Sparkly").latest_version(), Some("3.0"));
}

#[test]
fn per_host_rate_limit_spaces_requests() {
    let server = TestServer::start();
    let tmp = TempDir::new("rate");
    let dir = tmp.path().to_path_buf();
    for i in 0..4 {
        let path = format!("/feed{i}.xml");
        server.ok(&path, fixture("appcast_sparkle1.xml"));
        app_with(
            &dir,
            &format!("App {i}"),
            &format!("com.example.rate{i}"),
            "1.9",
            "1.9",
            &[("SUFeedURL", &server.url(&path))],
        );
    }
    let interval = Duration::from_millis(250);
    let opts = CheckOptions {
        per_host_interval: interval,
        concurrency: 4,
        ..options(&server, None)
    };
    let (report, _) = run(&dir, &opts);
    assert!(
        report
            .apps
            .iter()
            .all(|a| a.status == Status::UpdateAvailable),
        "{:#?}",
        report.apps
    );
    let mut times: Vec<Instant> = server.hits().iter().map(|h| h.at).collect();
    times.sort();
    assert_eq!(times.len(), 4);
    for w in times.windows(2) {
        let gap = w[1] - w[0];
        assert!(
            gap >= interval - Duration::from_millis(20),
            "requests only {gap:?} apart"
        );
    }
}

#[test]
fn timeouts_size_cap_and_bad_redirects() {
    let server = TestServer::start();
    let tmp = TempDir::new("timeouts");
    let dir = tmp.path().to_path_buf();
    server.slow(
        "/slow.xml",
        fixture("appcast_sparkle1.xml"),
        Duration::from_secs(3),
    );
    server.ok("/big.xml", "x".repeat(50_000));
    server.redirect("/loop.xml", 302, "/loop.xml");
    server.redirect("/ftp.xml", 302, "ftp://example.test/feed.xml");
    for (name, path) in [
        ("Slow", "/slow.xml"),
        ("Big", "/big.xml"),
        ("Loop", "/loop.xml"),
        ("Ftp", "/ftp.xml"),
    ] {
        app_with(
            &dir,
            name,
            &format!("com.example.{name}"),
            "1.0",
            "1",
            &[("SUFeedURL", &server.url(path))],
        );
    }
    let opts = CheckOptions {
        timeout: Duration::from_millis(600),
        max_response_bytes: 10_000,
        ..options(&server, None)
    };
    let started = Instant::now();
    let (report, _) = run(&dir, &opts);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    let reason = |name: &str| match &find(&report, name).status {
        Status::CheckFailed { reason } => reason.clone(),
        s => panic!("{name}: {s:?}"),
    };
    assert!(reason("Slow").contains("timed out"), "{}", reason("Slow"));
    assert!(reason("Big").contains("large"), "{}", reason("Big"));
    assert!(reason("Loop").contains("redirects"), "{}", reason("Loop"));
    assert!(reason("Ftp").contains("won't fetch"), "{}", reason("Ftp"));
}

#[test]
fn cancel_stops_quickly() {
    let server = TestServer::start();
    let tmp = TempDir::new("cancel");
    let dir = tmp.path().to_path_buf();
    for i in 0..6 {
        let path = format!("/slow{i}.xml");
        server.slow(
            &path,
            fixture("appcast_sparkle1.xml"),
            Duration::from_secs(4),
        );
        app_with(
            &dir,
            &format!("Slow {i}"),
            &format!("com.example.slow{i}"),
            "1.0",
            "1",
            &[("SUFeedURL", &server.url(&path))],
        );
    }
    app_with(&dir, "Offline", "com.example.offline", "1.0", "1", &[]);
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![dir],
        home: None,
    });
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let report = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Relaxed);
        });
        check_updates(&apps, &options(&server, None), |_| {}, &cancel)
    });
    assert!(report.cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    // Only the app that needed no network has a result.
    assert_eq!(report.apps.len(), 1);
    assert_eq!(report.apps[0].status, Status::UnknownSource);
}

#[test]
fn cancelled_before_start_makes_no_requests() {
    let server = TestServer::start();
    let tmp = TempDir::new("precancel");
    app_with(
        tmp.path(),
        "A",
        "com.example.a",
        "1.0",
        "1",
        &[("SUFeedURL", &server.url("/a.xml"))],
    );
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![tmp.path().to_path_buf()],
        home: None,
    });
    let report = check_updates(
        &apps,
        &options(&server, None),
        |_| {},
        &AtomicBool::new(true),
    );
    assert!(report.cancelled);
    assert!(server.hits().is_empty());
}

#[test]
fn vscode_service_and_private_github() {
    let server = TestServer::start();
    let tmp = TempDir::new("vscode");
    server.ok(
        "/api/update/darwin-arm64/stable/latest",
        r#"{"url":"https://dl.example.test/VSCode-darwin-arm64.zip","name":"1.143.1","productVersion":"1.143.1","timestamp":1792690529437}"#,
    );
    server.redirect(
        "/priv/x/releases/latest/download/latest-mac.yml",
        302,
        "/enterprises/acme/sso?return_to=%2Fpriv%2Fx",
    );
    server.ok(
        "/enterprises/acme/sso",
        "<!DOCTYPE html><html><body>Sign in</body></html>",
    );
    server.ok("/web/latest-mac.yml", "<html>Not here</html>");
    let private = app_with(
        tmp.path(),
        "Private",
        "com.example.private",
        "5.8.0",
        "5.8.0",
        &[],
    );
    add_file(
        &private,
        "Contents/Resources/app-update.yml",
        "owner: priv\nrepo: x\nprovider: github\n",
    );
    let web = app_with(
        tmp.path(),
        "Web Page",
        "com.example.web",
        "1.0.0",
        "1.0.0",
        &[],
    );
    add_file(
        &web,
        "Contents/Resources/app-update.yml",
        &format!("provider: generic\nurl: {}\n", server.url("/web")),
    );
    let mut apps = discover_apps(&DiscoverOptions {
        roots: vec![tmp.path().to_path_buf()],
        home: None,
    });
    apps.push(kipimo_updates::InstalledApp {
        name: "Visual Studio Code".into(),
        path: tmp.path().join("Visual Studio Code.app"),
        bundle_id: Some("com.microsoft.VSCode".into()),
        version: Some("1.143.0".into()),
        build: Some("1.143.0".into()),
        minimum_system_version: None,
        source: kipimo_updates::UpdateSource::VsCode {
            update_url: server.base.clone(),
            quality: "stable".into(),
            platform: "darwin-arm64".into(),
        },
    });
    let report = check_updates(
        &apps,
        &options(&server, None),
        |_| {},
        &AtomicBool::new(false),
    );

    let code = find(&report, "Visual Studio Code");
    assert_eq!(code.status, Status::UpdateAvailable);
    assert_eq!(code.latest_version(), Some("1.143.1"));
    let latest = code.latest.as_ref().unwrap();
    assert_eq!(
        latest.release_notes_url.as_deref(),
        Some("https://code.visualstudio.com/updates/v1_143")
    );
    assert_eq!(
        latest.download_url.as_deref(),
        Some("https://dl.example.test/VSCode-darwin-arm64.zip")
    );

    assert_eq!(
        find(&report, "Private").status,
        Status::CheckFailed {
            reason: "the GitHub repository is private or needs a sign-in".into()
        }
    );
    match &find(&report, "Web Page").status {
        Status::CheckFailed { reason } => assert!(reason.contains("web page"), "{reason}"),
        s => panic!("{s:?}"),
    }
}
