//! Mac App Store versions through Apple's public iTunes lookup API.

use std::collections::HashMap;

use serde::Deserialize;

use crate::finding::{Candidate, Env, Finding};
use crate::types::Release;
use crate::version::compare_lenient;

/// Bundle IDs per lookup request; the API takes a comma-separated list.
pub(crate) const BATCH: usize = 40;

/// The lookup URL for a batch of bundle IDs. `ios_apps` asks for iPhone/iPad apps (which run
/// on Apple silicon) instead of Mac apps; the API needs to be told which store to search.
pub(crate) fn lookup_url(base: &str, ids: &[String], country: &str, ios_apps: bool) -> String {
    let ids: Vec<String> = ids.iter().map(|id| crate::url::encode_query(id)).collect();
    let entity = if ios_apps {
        "software"
    } else {
        "desktopSoftware"
    };
    format!(
        "{base}?bundleId={}&country={}&entity={entity}",
        ids.join(","),
        crate::url::encode_query(country)
    )
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    results: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    bundle_id: Option<String>,
    kind: Option<String>,
    version: Option<String>,
    minimum_os_version: Option<String>,
    track_id: Option<u64>,
    track_view_url: Option<String>,
    current_version_release_date: Option<String>,
}

fn release(it: &Item, country: &str, ios_apps: bool) -> Release {
    let page = match it.track_id {
        Some(id) => Some(format!("https://apps.apple.com/{country}/app/id{id}")),
        // Drop the lookup API's tracking parameter ("uo=4").
        None => it
            .track_view_url
            .as_deref()
            .map(|u| u.split("&uo=").next().unwrap_or(u).to_string()),
    };
    let store = it
        .track_id
        .map(|id| format!("macappstore://apps.apple.com/app/id{id}?mt=12"))
        .or_else(|| page.clone());
    Release {
        version: it.version.clone().filter(|v| !v.trim().is_empty()),
        build: None,
        // For iPhone/iPad apps this is an iOS version, which says nothing about macOS.
        minimum_system_version: if ios_apps {
            None
        } else {
            it.minimum_os_version.clone()
        },
        release_notes_url: page,
        download_url: store,
        published: it.current_version_release_date.clone(),
    }
}

/// Parses a lookup response. Returns one result per requested bundle ID (keyed lower case);
/// IDs the store doesn't know get an error.
pub(crate) fn parse_lookup(
    json: &str,
    ids: &[String],
    env: &Env,
    ios_apps: bool,
) -> Result<HashMap<String, Result<Finding, String>>, String> {
    let resp: Response = serde_json::from_str(json)
        .map_err(|e| format!("the App Store sent an unexpected answer ({e})"))?;
    let want_kind = if ios_apps { "software" } else { "mac-software" };
    let mut out = HashMap::new();
    for id in ids {
        let matches: Vec<&Item> = resp
            .results
            .iter()
            .filter(|r| {
                r.bundle_id
                    .as_deref()
                    .is_some_and(|b| b.eq_ignore_ascii_case(id))
            })
            .collect();
        let item = matches
            .iter()
            .find(|r| r.kind.as_deref() == Some(want_kind))
            .or(matches.first());
        let result = match item {
            None => Err(format!(
                "not found in the App Store ({} region)",
                env.country.to_ascii_uppercase()
            )),
            Some(it) if it.version.as_deref().is_none_or(|v| v.trim().is_empty()) => {
                Err("the App Store doesn't list a version".to_string())
            }
            Some(it) => {
                let rel = release(it, &env.country, ios_apps);
                let compatible = rel
                    .minimum_system_version
                    .as_deref()
                    .is_none_or(|min| compare_lenient(&env.macos, min) != std::cmp::Ordering::Less);
                Ok(if compatible {
                    Finding {
                        latest: Some(Candidate::new(rel)),
                        newest_incompatible: None,
                    }
                } else {
                    Finding {
                        latest: None,
                        newest_incompatible: Some(Candidate::new(rel)),
                    }
                })
            }
        };
        out.insert(id.to_ascii_lowercase(), result);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            macos: "26.6.2".into(),
            darwin: "25.6.0".into(),
            arm64: true,
            channels: vec![],
            country: "gb".into(),
        }
    }

    #[test]
    fn url() {
        let ids = vec![
            "com.wireguard.macos".to_string(),
            "com.apple.dt.Xcode".to_string(),
        ];
        assert_eq!(
            lookup_url("https://itunes.apple.com/lookup", &ids, "gb", false),
            "https://itunes.apple.com/lookup?bundleId=com.wireguard.macos,com.apple.dt.Xcode&country=gb&entity=desktopSoftware"
        );
    }

    #[test]
    fn lookup_fixture() {
        let json = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/itunes_lookup.json"
        ))
        .unwrap();
        let ids: Vec<String> = [
            "com.example.storeapp",
            "com.example.current",
            "com.example.needsnewos",
            "com.example.gone",
        ]
        .map(String::from)
        .into();
        let map = parse_lookup(&json, &ids, &env(), false).unwrap();
        assert_eq!(map.len(), 4);

        // The Mac entry wins over the iPhone app with the same bundle ID.
        let f = map["com.example.storeapp"].as_ref().unwrap();
        let r = &f.latest.as_ref().unwrap().release;
        assert_eq!(r.version.as_deref(), Some("2.4.1"));
        assert_eq!(r.minimum_system_version.as_deref(), Some("13.0"));
        assert_eq!(
            r.download_url.as_deref(),
            Some("macappstore://apps.apple.com/app/id111111?mt=12")
        );
        assert_eq!(
            r.release_notes_url.as_deref(),
            Some("https://apps.apple.com/gb/app/id111111")
        );
        assert_eq!(r.published.as_deref(), Some("2026-10-01T12:00:00Z"));

        // Bundle IDs match case-insensitively; a too-new minimum macOS is flagged.
        let f = map["com.example.needsnewos"].as_ref().unwrap();
        assert!(f.latest.is_none());
        assert_eq!(
            f.newest_incompatible
                .as_ref()
                .unwrap()
                .release
                .minimum_system_version
                .as_deref(),
            Some("99.0")
        );

        assert_eq!(
            map["com.example.gone"],
            Err("not found in the App Store (GB region)".into())
        );
    }

    #[test]
    fn lookup_garbage() {
        let ids = vec!["a".to_string()];
        assert!(parse_lookup("<html>", &ids, &env(), false).is_err());
        let map = parse_lookup("{\"resultCount\":0,\"results\":[]}", &ids, &env(), false).unwrap();
        assert!(map["a"].is_err());
        let map = parse_lookup(
            "{\"results\":[{\"bundleId\":\"a\",\"version\":\"\"}]}",
            &ids,
            &env(),
            false,
        )
        .unwrap();
        assert!(map["a"].is_err());
    }
}
