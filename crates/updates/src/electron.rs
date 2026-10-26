//! electron-updater: `app-update.yml` inside the app, `latest-mac.yml` on the server.

use crate::finding::{Candidate, Env, Finding};
use crate::types::{ElectronFeed, Release};
use crate::version::{compare_lenient, macos_for_darwin};
use crate::yaml::{self, Yaml};

/// Reads `app-update.yml`. `Err(provider)` names a provider Kipimo can't check.
pub(crate) fn parse_app_update(src: &str) -> Result<ElectronFeed, String> {
    let y = yaml::parse(src);
    let provider = y.str("provider").unwrap_or("").to_ascii_lowercase();
    let channel = y.str("channel").unwrap_or("latest").to_string();
    match provider.as_str() {
        "github" => {
            if y.str("private")
                .is_some_and(|p| p.eq_ignore_ascii_case("true"))
                || y.str("token").is_some()
            {
                return Err("github (private repository)".into());
            }
            let (Some(owner), Some(repo)) = (y.str("owner"), y.str("repo")) else {
                return Err("github (no owner or repo)".into());
            };
            let host = y
                .str("host")
                .filter(|h| !h.eq_ignore_ascii_case("github.com"))
                .map(str::to_string);
            Ok(ElectronFeed::GitHub {
                host,
                owner: owner.to_string(),
                repo: repo.to_string(),
                channel,
            })
        }
        "generic" => match y.str("url") {
            Some(url) if crate::url::http_scheme(url).is_some() => Ok(ElectronFeed::Generic {
                url: url.to_string(),
                channel,
            }),
            _ => Err("generic (no web address)".into()),
        },
        "" => Err("unknown".into()),
        other => Err(other.to_string()),
    }
}

/// The `<channel>-mac.yml` file name electron-updater asks for.
pub(crate) fn channel_file(channel: &str) -> String {
    let ch = if channel.trim().is_empty() {
        "latest"
    } else {
        channel.trim()
    };
    format!("{ch}-mac.yml")
}

/// The first URL to fetch. For GitHub this is the "latest release" download link, which
/// redirects through `/releases/download/<tag>/`, where the tag is read from.
pub(crate) fn feed_url(feed: &ElectronFeed, github_base: &str) -> String {
    match feed {
        ElectronFeed::GitHub {
            host,
            owner,
            repo,
            channel,
        } => {
            let base = match host {
                Some(h) => format!("https://{h}"),
                None => github_base.trim_end_matches('/').to_string(),
            };
            format!(
                "{base}/{owner}/{repo}/releases/latest/download/{}",
                channel_file(channel)
            )
        }
        ElectronFeed::Generic { url, channel } => {
            crate::url::join_folder(url, &channel_file(channel))
        }
    }
}

/// The release tag from a GitHub redirect chain (".../releases/download/v1.2.3/latest-mac.yml").
pub(crate) fn github_tag(urls: &[String]) -> Option<String> {
    urls.iter().find_map(|u| {
        let rest = u.split("/releases/download/").nth(1)?;
        let tag = rest.split('/').next()?;
        if tag.is_empty() || tag == "latest" {
            None
        } else {
            Some(tag.to_string())
        }
    })
}

/// Scores a download for this Mac: a matching architecture or universal build first, then a
/// disk image over a zip. Builds for the other architecture are never chosen.
fn score(name: &str, arm64: bool) -> Option<i32> {
    let n = name.to_ascii_lowercase();
    let is_arm = n.contains("arm64") || n.contains("aarch64");
    let is_intel =
        n.contains("x64") || n.contains("x86_64") || n.contains("intel") || n.contains("amd64");
    let is_universal = n.contains("universal");
    let mut s = 0;
    if is_universal {
        s += 3;
    } else if (arm64 && is_arm) || (!arm64 && is_intel) {
        s += 4;
    } else if is_arm || is_intel {
        return None;
    }
    if n.ends_with(".dmg") {
        s += 3;
    } else if n.ends_with(".zip") {
        s += 1;
    } else if !n.ends_with(".pkg") {
        return None;
    }
    Some(s)
}

fn pick_download(y: &Yaml, arm64: bool) -> Option<String> {
    let mut names: Vec<&str> = y
        .list("files")
        .iter()
        .filter_map(|f| f.str("url"))
        .collect();
    if let Some(p) = y.str("path") {
        names.push(p);
    }
    let best = names
        .iter()
        .filter_map(|n| score(n, arm64).map(|s| (s, *n)))
        .max_by_key(|(s, _)| *s);
    // Fall back to `path` even if its name gives nothing away.
    best.map(|(_, n)| n).or(y.str("path")).map(str::to_string)
}

/// Turns `latest-mac.yml` into a finding. `yml_url` is the file's final URL (relative file
/// names resolve against it); `tag` and `release_page` describe the GitHub release, if any.
pub(crate) fn parse_latest(
    src: &str,
    yml_url: &str,
    release_page: Option<String>,
    env: &Env,
) -> Result<Finding, String> {
    if src.trim_start().starts_with('<') {
        return Err("the update feed is a web page, not a latest-mac.yml file".into());
    }
    let y = yaml::parse(src);
    let version = y
        .str("version")
        .ok_or("the update feed has no version")?
        .trim_start_matches(['v', 'V'])
        .to_string();
    // electron-updater compares `minimumSystemVersion` with os.release(), the Darwin version.
    // Signal publishes the same thing as `vendor.minOSVersion`.
    let min_darwin = y
        .str("minimumSystemVersion")
        .or_else(|| y.get("vendor").and_then(|v| v.str("minOSVersion")))
        .map(str::to_string);
    let minimum_system_version = min_darwin
        .as_deref()
        .map(|d| macos_for_darwin(d).unwrap_or_else(|| d.to_string()));
    let release = Release {
        version: Some(version),
        build: None,
        minimum_system_version,
        release_notes_url: release_page,
        download_url: pick_download(&y, env.arm64).and_then(|f| crate::url::resolve(yml_url, &f)),
        published: y.str("releaseDate").map(str::to_string),
    };
    let compatible = min_darwin
        .as_deref()
        .is_none_or(|min| compare_lenient(&env.darwin, min) != std::cmp::Ordering::Less);
    Ok(if compatible {
        Finding {
            latest: Some(Candidate::new(release)),
            newest_incompatible: None,
        }
    } else {
        Finding {
            latest: None,
            newest_incompatible: Some(Candidate::new(release)),
        }
    })
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
            country: "us".into(),
        }
    }

    #[test]
    fn app_update_providers() {
        assert_eq!(
            parse_app_update("owner: o\nrepo: r\nprovider: github\n"),
            Ok(ElectronFeed::GitHub {
                host: None,
                owner: "o".into(),
                repo: "r".into(),
                channel: "latest".into()
            })
        );
        assert_eq!(
            parse_app_update("provider: generic\nurl: https://u.example/desktop\nchannel: beta\n"),
            Ok(ElectronFeed::Generic {
                url: "https://u.example/desktop".into(),
                channel: "beta".into()
            })
        );
        assert_eq!(
            parse_app_update("provider: s3\nbucket: b\n"),
            Err("s3".into())
        );
        assert!(parse_app_update("provider: github\nowner: o\nrepo: r\nprivate: true\n").is_err());
        assert!(parse_app_update("provider: generic\nurl: file:///x\n").is_err());
    }

    #[test]
    fn urls() {
        let gh = ElectronFeed::GitHub {
            host: None,
            owner: "o".into(),
            repo: "r".into(),
            channel: "latest".into(),
        };
        assert_eq!(
            feed_url(&gh, "https://github.com"),
            "https://github.com/o/r/releases/latest/download/latest-mac.yml"
        );
        let ge = ElectronFeed::Generic {
            url: "https://u.example/d/".into(),
            channel: "beta".into(),
        };
        assert_eq!(feed_url(&ge, "unused"), "https://u.example/d/beta-mac.yml");
        let chain = vec![
            "https://github.com/o/r/releases/latest/download/latest-mac.yml".to_string(),
            "https://github.com/o/r/releases/download/v3.7.21/latest-mac.yml".to_string(),
        ];
        assert_eq!(github_tag(&chain).as_deref(), Some("v3.7.21"));
    }

    #[test]
    fn signal_style_feed() {
        let src = "version: 8.32.0
files:
  - url: signal-desktop-mac-x64-8.32.0.zip
    size: 1
  - url: signal-desktop-mac-arm64-8.32.0.zip
    size: 1
  - url: signal-desktop-mac-universal-8.32.0.dmg
    size: 1
path: signal-desktop-mac-x64-8.32.0.zip
vendor:
  minOSVersion: 22.1.0
releaseDate: '2026-10-20T17:43:00.437Z'
";
        let f = parse_latest(
            src,
            "https://updates.example/desktop/latest-mac.yml",
            None,
            &env(),
        )
        .unwrap();
        let r = f.latest.unwrap().release;
        assert_eq!(r.version.as_deref(), Some("8.32.0"));
        assert_eq!(r.minimum_system_version.as_deref(), Some("13.0"));
        // A disk image people can open beats the arm64 zip; the x64 `path` is never picked.
        assert_eq!(
            r.download_url.as_deref(),
            Some("https://updates.example/desktop/signal-desktop-mac-universal-8.32.0.dmg")
        );
        assert_eq!(r.published.as_deref(), Some("2026-10-20T17:43:00.437Z"));
    }

    #[test]
    fn newer_darwin_required() {
        let src = "version: 2.0.0\npath: App-2.0.0-arm64.zip\nminimumSystemVersion: 30.0.0\n";
        let f = parse_latest(src, "https://x.example/latest-mac.yml", None, &env()).unwrap();
        assert!(f.latest.is_none());
        let r = f.newest_incompatible.unwrap().release;
        assert_eq!(r.minimum_system_version.as_deref(), Some("31.0"));
        assert_eq!(
            r.download_url.as_deref(),
            Some("https://x.example/App-2.0.0-arm64.zip")
        );
    }

    #[test]
    fn missing_version_is_an_error() {
        assert!(
            parse_latest(
                "path: x.zip\n",
                "https://x.example/latest-mac.yml",
                None,
                &env()
            )
            .is_err()
        );
        assert!(
            parse_latest(
                "<html>nope</html>",
                "https://x.example/latest-mac.yml",
                None,
                &env()
            )
            .is_err()
        );
    }
}
