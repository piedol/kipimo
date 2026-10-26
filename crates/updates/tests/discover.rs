//! Discovery on a synthetic Applications folder and home folder.

mod common;

use std::fs;

use common::*;
use kipimo_updates::{DiscoverOptions, ElectronFeed, SelfUpdater, UpdateSource, discover_apps};
use plist::Value;

fn source_of<'a>(apps: &'a [kipimo_updates::InstalledApp], name: &str) -> &'a UpdateSource {
    &apps
        .iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
        .source
}

#[test]
fn finds_apps_and_their_update_sources() {
    let tmp = TempDir::new("discover");
    let apps_dir = tmp.path().join("Applications");
    let home = tmp.path().join("home");
    let user_apps = home.join("Applications");
    fs::create_dir_all(&apps_dir).unwrap();
    fs::create_dir_all(&user_apps).unwrap();

    let mas = app_with(
        &apps_dir,
        "Store App",
        "com.example.store",
        "2.0",
        "20",
        &[],
    );
    add_mas_receipt(&mas);

    app_with(
        &apps_dir,
        "Sparkly",
        "com.example.sparkly",
        "1.0",
        "100",
        &[("SUFeedURL", " https://feeds.test/appcast.xml ")],
    );

    // The user's defaults override Info.plist (a beta feed chosen in the app's settings).
    let overridden = app_with(
        &apps_dir,
        "Overridden",
        "com.example.overridden",
        "1.0",
        "1",
        &[("SUFeedURL", "https://feeds.test/final.xml")],
    );
    add_framework(&overridden, "Sparkle");
    let prefs = home.join("Library/Preferences");
    fs::create_dir_all(&prefs).unwrap();
    let mut d = plist::Dictionary::new();
    d.insert(
        "SUFeedURL".into(),
        Value::String("https://feeds.test/beta.xml".into()),
    );
    d.insert("SomethingElse".into(), Value::Boolean(true));
    Value::Dictionary(d)
        .to_file_binary(prefs.join("com.example.overridden.plist"))
        .unwrap();

    // A sandboxed app keeps its defaults in its container.
    let contained = app_with(
        &apps_dir,
        "Contained",
        "com.example.contained",
        "1.0",
        "1",
        &[],
    );
    add_framework(&contained, "Sparkle");
    let cprefs = home.join("Library/Containers/com.example.contained/Data/Library/Preferences");
    fs::create_dir_all(&cprefs).unwrap();
    let mut d = plist::Dictionary::new();
    d.insert(
        "SUFeedURL".into(),
        Value::String("https://feeds.test/contained.xml".into()),
    );
    Value::Dictionary(d)
        .to_file_xml(cprefs.join("com.example.contained.plist"))
        .unwrap();

    let sparkle_no_feed = app_with(
        &apps_dir,
        "Sparkle Inside",
        "com.example.inside",
        "1.0",
        "1",
        &[],
    );
    add_framework(&sparkle_no_feed, "Sparkle");

    let gh = app_with(
        &apps_dir,
        "Electron GitHub",
        "com.example.egh",
        "3.0.0",
        "3.0.0",
        &[],
    );
    add_file(
        &gh,
        "Contents/Resources/app-update.yml",
        "owner: acme\nrepo: thing\nprovider: github\nupdaterCacheDirName: x\n",
    );
    let generic = app_with(
        &apps_dir,
        "Electron Generic",
        "com.example.egen",
        "8.0.0",
        "8.0.0",
        &[],
    );
    add_file(
        &generic,
        "Contents/Resources/app-update.yml",
        "provider: generic\nurl: https://updates.test/desktop\n",
    );
    add_framework(&generic, "Squirrel");
    let s3 = app_with(
        &apps_dir,
        "Electron S3",
        "com.example.es3",
        "1.0.0",
        "1.0.0",
        &[],
    );
    add_file(
        &s3,
        "Contents/Resources/app-update.yml",
        "provider: s3\nbucket: b\n",
    );

    let squirrel = app_with(
        &apps_dir,
        "Squirrelly",
        "com.example.squirrel",
        "1.0",
        "1",
        &[],
    );
    add_framework(&squirrel, "Squirrel");
    app_with(
        &apps_dir,
        "Chromish",
        "com.example.chromish",
        "1.0",
        "1",
        &[("KSProductID", "com.example.chromish")],
    );
    let moz = app_with(&apps_dir, "Foxy", "org.example.foxy", "155.0", "15525", &[]);
    add_file(&moz, "Contents/Resources/updater.ini", "[Strings]\n");
    app_with(
        &apps_dir,
        "Word Processor",
        "com.microsoft.Wordish",
        "16.0",
        "16.0",
        &[],
    );
    app_with(&apps_dir, "Plain", "com.example.plain", "1.0", "1", &[]);

    // One level of subfolders is scanned, two are not; hidden and non-app folders are ignored.
    let utilities = apps_dir.join("Utilities");
    fs::create_dir_all(&utilities).unwrap();
    app_with(&utilities, "Sub Tool", "com.example.sub", "1.0", "1", &[]);
    let deep = utilities.join("Deeper");
    fs::create_dir_all(&deep).unwrap();
    app_with(&deep, "Too Deep", "com.example.deep", "1.0", "1", &[]);
    app_with(&apps_dir, ".Hidden", "com.example.hidden", "1.0", "1", &[]);
    fs::create_dir_all(apps_dir.join("Backup.app.old/Contents")).unwrap();

    // Binary Info.plist with a numeric build, and an app without an Info.plist.
    let bin = apps_dir.join("Binary.app/Contents");
    fs::create_dir_all(&bin).unwrap();
    let mut d = plist::Dictionary::new();
    d.insert(
        "CFBundleIdentifier".into(),
        Value::String("com.example.binary".into()),
    );
    d.insert(
        "CFBundleShortVersionString".into(),
        Value::String("4.5".into()),
    );
    d.insert("CFBundleVersion".into(), Value::Integer(450.into()));
    d.insert(
        "LSMinimumSystemVersion".into(),
        Value::String("12.0".into()),
    );
    Value::Dictionary(d)
        .to_file_binary(bin.join("Info.plist"))
        .unwrap();
    fs::create_dir_all(apps_dir.join("Bare.app/Contents/MacOS")).unwrap();

    // An iPhone app wrapped for Apple silicon, from the App Store.
    let ios = apps_dir.join("Phone Game.app");
    let inner = ios.join("Wrapper/PhoneGame.app");
    fs::create_dir_all(&inner).unwrap();
    let mut d = plist::Dictionary::new();
    d.insert(
        "CFBundleIdentifier".into(),
        Value::String("com.example.phonegame".into()),
    );
    d.insert(
        "CFBundleShortVersionString".into(),
        Value::String("3.1".into()),
    );
    Value::Dictionary(d)
        .to_file_xml(inner.join("Info.plist"))
        .unwrap();
    fs::write(ios.join("Wrapper/iTunesMetadata.plist"), b"x").unwrap();
    std::os::unix::fs::symlink("Wrapper/PhoneGame.app", ios.join("WrappedBundle")).unwrap();

    // ~/Applications too.
    app_with(&user_apps, "User App", "com.example.user", "0.1", "1", &[]);

    let apps = discover_apps(&DiscoverOptions {
        roots: vec![apps_dir.clone(), user_apps.clone()],
        home: Some(home.clone()),
    });
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Bare",
            "Binary",
            "Chromish",
            "Contained",
            "Electron Generic",
            "Electron GitHub",
            "Electron S3",
            "Foxy",
            "Overridden",
            "Phone Game",
            "Plain",
            "Sparkle Inside",
            "Sparkly",
            "Squirrelly",
            "Store App",
            "Sub Tool",
            "User App",
            "Word Processor",
        ]
    );

    assert_eq!(
        source_of(&apps, "Store App"),
        &UpdateSource::AppStore { ios_app: false }
    );
    assert_eq!(
        source_of(&apps, "Sparkly"),
        &UpdateSource::Sparkle {
            feed_url: "https://feeds.test/appcast.xml".into(),
            from_defaults: false
        }
    );
    assert_eq!(
        source_of(&apps, "Overridden"),
        &UpdateSource::Sparkle {
            feed_url: "https://feeds.test/beta.xml".into(),
            from_defaults: true
        }
    );
    assert_eq!(
        source_of(&apps, "Contained"),
        &UpdateSource::Sparkle {
            feed_url: "https://feeds.test/contained.xml".into(),
            from_defaults: true
        }
    );
    assert_eq!(
        source_of(&apps, "Sparkle Inside"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::Sparkle)
        }
    );
    assert_eq!(
        source_of(&apps, "Electron GitHub"),
        &UpdateSource::ElectronUpdater {
            feed: ElectronFeed::GitHub {
                host: None,
                owner: "acme".into(),
                repo: "thing".into(),
                channel: "latest".into()
            }
        }
    );
    assert_eq!(
        source_of(&apps, "Electron Generic"),
        &UpdateSource::ElectronUpdater {
            feed: ElectronFeed::Generic {
                url: "https://updates.test/desktop".into(),
                channel: "latest".into()
            }
        }
    );
    assert_eq!(
        source_of(&apps, "Electron S3"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::ElectronUpdater {
                provider: "s3".into()
            })
        }
    );
    assert_eq!(
        source_of(&apps, "Squirrelly"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::Squirrel)
        }
    );
    assert_eq!(
        source_of(&apps, "Chromish"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::GoogleKeystone)
        }
    );
    assert_eq!(
        source_of(&apps, "Foxy"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::Mozilla)
        }
    );
    assert_eq!(
        source_of(&apps, "Word Processor"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::MicrosoftAutoUpdate)
        }
    );
    assert_eq!(
        source_of(&apps, "Plain"),
        &UpdateSource::Unknown { updater: None }
    );
    assert_eq!(
        source_of(&apps, "Phone Game"),
        &UpdateSource::AppStore { ios_app: true }
    );

    let binary = apps.iter().find(|a| a.name == "Binary").unwrap();
    assert_eq!(binary.bundle_id.as_deref(), Some("com.example.binary"));
    assert_eq!(binary.version.as_deref(), Some("4.5"));
    assert_eq!(binary.build.as_deref(), Some("450"));
    assert_eq!(binary.minimum_system_version.as_deref(), Some("12.0"));
    let bare = apps.iter().find(|a| a.name == "Bare").unwrap();
    assert_eq!(
        (bare.bundle_id.as_ref(), bare.version.as_ref()),
        (None, None)
    );
    let phone = apps.iter().find(|a| a.name == "Phone Game").unwrap();
    assert_eq!(phone.bundle_id.as_deref(), Some("com.example.phonegame"));
    assert_eq!(phone.display_version(), Some("3.1"));

    // Without a home folder, preferences aren't read: the Info.plist feed is used.
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![apps_dir],
        home: None,
    });
    assert_eq!(
        source_of(&apps, "Overridden"),
        &UpdateSource::Sparkle {
            feed_url: "https://feeds.test/final.xml".into(),
            from_defaults: false
        }
    );
    assert_eq!(
        source_of(&apps, "Contained"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::Sparkle)
        }
    );
}

#[test]
fn missing_roots_are_fine() {
    let tmp = TempDir::new("discover-missing");
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![tmp.path().join("nope")],
        home: None,
    });
    assert!(apps.is_empty());
}

#[test]
fn symlinked_duplicates_are_listed_once() {
    let tmp = TempDir::new("discover-dupes");
    let a = tmp.path().join("A");
    let b = tmp.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let app = app_with(&a, "Linked", "com.example.linked", "1.0", "1", &[]);
    std::os::unix::fs::symlink(&app, b.join("Linked.app")).unwrap();
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![a, b],
        home: None,
    });
    assert_eq!(apps.len(), 1);
}

#[test]
fn vscode_product_json() {
    let tmp = TempDir::new("discover-vscode");
    let code = app_with(
        tmp.path(),
        "Visual Studio Code",
        "com.microsoft.VSCode",
        "1.143.0",
        "1.143.0",
        &[],
    );
    add_framework(&code, "Squirrel");
    add_file(
        &code,
        "Contents/Resources/app/product.json",
        r#"{"updateUrl":"https://update.code.visualstudio.com","quality":"stable","commit":"abc"}"#,
    );
    // A fork with its own server isn't guessed at: it stays a self-updating Squirrel app.
    let fork = app_with(
        tmp.path(),
        "Forked Code",
        "com.example.fork",
        "1.0",
        "1",
        &[],
    );
    add_framework(&fork, "Squirrel");
    add_file(
        &fork,
        "Contents/Resources/app/product.json",
        r#"{"updateUrl":"https://updates.fork.example","quality":"stable"}"#,
    );
    let apps = discover_apps(&DiscoverOptions {
        roots: vec![tmp.path().to_path_buf()],
        home: None,
    });
    match source_of(&apps, "Visual Studio Code") {
        UpdateSource::VsCode {
            update_url,
            quality,
            platform,
        } => {
            assert_eq!(update_url, "https://update.code.visualstudio.com");
            assert_eq!(quality, "stable");
            assert!(platform.starts_with("darwin"), "{platform}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        source_of(&apps, "Forked Code"),
        &UpdateSource::Unknown {
            updater: Some(SelfUpdater::Squirrel)
        }
    );
}
