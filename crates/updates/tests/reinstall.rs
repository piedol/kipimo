//! Reinstall support against a local HTTP server: the cache-only check, the route it gives,
//! and downloading a file.

mod common;

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::*;
use kipimo_updates::{
    CheckOptions, DownloadOptions, Route, Status, check_updates, download, read_app,
    reinstall_route,
};

fn options(server: &TestServer, cache: &Path) -> CheckOptions {
    CheckOptions {
        cache_path: Some(cache.to_path_buf()),
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

#[test]
fn offline_check_and_route() {
    let server = TestServer::start();
    server.ok("/appcast.xml", fixture("appcast_multi.xml"));
    let tmp = TempDir::new("reinstall-offline");
    let app_path = app_with(
        tmp.path(),
        "Sparkly",
        "com.example.sparkly",
        "2.1",
        "210",
        &[("SUFeedURL", &server.url("/appcast.xml"))],
    );
    let app = read_app(&app_path, None).unwrap();
    let cache = tmp.path().join("Caches/updates.json");
    let cancel = AtomicBool::new(false);
    let offline = CheckOptions {
        offline: true,
        ..options(&server, &cache)
    };

    // Nothing cached yet: no request, and no answer.
    let r = check_updates(std::slice::from_ref(&app), &offline, |_| {}, &cancel);
    assert_eq!(r.requests, 0);
    assert!(server.hits().is_empty());
    assert_eq!(
        r.apps[0].status,
        Status::CheckFailed {
            reason: "not checked yet".into()
        }
    );
    assert!(!cache.exists(), "an offline check doesn't write the cache");
    assert!(matches!(
        reinstall_route(&app, Some(&r.apps[0]), None),
        Route::Website { .. }
    ));

    // A normal check fills the cache...
    let r = check_updates(
        std::slice::from_ref(&app),
        &options(&server, &cache),
        |_| {},
        &cancel,
    );
    assert_eq!(r.apps[0].status, Status::UpdateAvailable);
    let hits = server.hits().len();

    // ...which an offline check uses however old it is, without going online.
    let r = check_updates(
        std::slice::from_ref(&app),
        &CheckOptions {
            max_cache_age: Duration::ZERO,
            force: true,
            ..offline
        },
        |_| {},
        &cancel,
    );
    assert_eq!(server.hits().len(), hits);
    assert!(r.apps[0].from_cache);
    assert_eq!(
        reinstall_route(&app, Some(&r.apps[0]), None),
        Route::Download {
            url: "https://example.test/dl/Example-2.3.zip".into(),
            version: Some("2.3".into()),
            source: "Sparkle".into(),
            zip: true
        }
    );
}

#[test]
fn downloads_to_a_file() {
    let server = TestServer::start();
    let body: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    server.ok("/files/Thing.zip", body.clone());
    server.redirect("/latest", 302, "/files/Thing.zip");
    server.status("/gone.zip", 404);
    let tmp = TempDir::new("download");
    let opts = DownloadOptions {
        use_system_proxy: false,
        ..DownloadOptions::default()
    };
    let cancel = AtomicBool::new(false);

    let dest = tmp.path().join("Thing.zip");
    let mut seen = Vec::new();
    download(
        &server.url("/latest"),
        &dest,
        &opts,
        &cancel,
        |done, total| seen.push((done, total)),
    )
    .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), body);
    assert_eq!(
        seen.last(),
        Some(&(body.len() as u64, Some(body.len() as u64)))
    );
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));

    // Never over an existing file.
    assert!(download(&server.url("/latest"), &dest, &opts, &cancel, |_, _| {}).is_err());
    assert_eq!(fs::read(&dest).unwrap(), body);

    let failed = tmp.path().join("gone.zip");
    let err = download(&server.url("/gone.zip"), &failed, &opts, &cancel, |_, _| {}).unwrap_err();
    assert_eq!(err, "127.0.0.1 answered with HTTP 404");
    assert!(!failed.exists());

    let small = DownloadOptions {
        max_bytes: 1000,
        ..opts.clone()
    };
    let big = tmp.path().join("big.zip");
    assert!(download(&server.url("/latest"), &big, &small, &cancel, |_, _| {}).is_err());
    assert!(!big.exists());

    let stop = AtomicBool::new(true);
    let cancelled = tmp.path().join("cancelled.zip");
    assert_eq!(
        download(&server.url("/latest"), &cancelled, &opts, &stop, |_, _| {}),
        Err("cancelled".into())
    );
    assert!(!cancelled.exists());

    assert!(
        download(
            "file:///etc/hosts",
            &tmp.path().join("x"),
            &opts,
            &cancel,
            |_, _| {}
        )
        .is_err()
    );
}
