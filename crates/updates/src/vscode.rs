//! Visual Studio Code's update service.
//!
//! VS Code asks `<updateUrl>/api/update/<platform>/<quality>/<commit>`; Kipimo asks for
//! `.../latest` instead, which answers with the newest build without sending the installed
//! commit.

use serde::Deserialize;

use crate::finding::{Candidate, Finding};
use crate::types::Release;

pub(crate) fn latest_url(update_url: &str, platform: &str, quality: &str) -> String {
    format!(
        "{}/api/update/{platform}/{quality}/latest",
        update_url.trim_end_matches('/')
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    url: Option<String>,
    name: Option<String>,
    product_version: Option<String>,
    timestamp: Option<u64>,
}

/// "2026-10-26T16:53:17Z" from Unix milliseconds.
fn iso_date(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub(crate) fn parse(json: &str, quality: &str) -> Result<Finding, String> {
    let a: Answer = serde_json::from_str(json)
        .map_err(|e| format!("the update service sent an unexpected answer ({e})"))?;
    let version = a
        .product_version
        .or(a.name)
        .filter(|v| !v.trim().is_empty())
        .ok_or("the update service doesn't give a version")?;
    // Stable releases have a notes page per minor version ("updates/v1_143").
    let notes = if quality == "stable" {
        let mut parts = version.split('.');
        match (parts.next(), parts.next()) {
            (Some(major), Some(minor)) => Some(format!(
                "https://code.visualstudio.com/updates/v{major}_{minor}"
            )),
            _ => None,
        }
    } else {
        None
    };
    let release = Release {
        version: Some(version),
        build: None,
        minimum_system_version: None,
        release_notes_url: notes,
        download_url: a.url.filter(|u| crate::url::http_scheme(u).is_some()),
        published: a.timestamp.map(iso_date),
    };
    Ok(Finding {
        latest: Some(Candidate::new(release)),
        newest_incompatible: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_answer() {
        let json = r#"{"url":"https://vscode.download.example/stable/e64/VSCode-darwin-arm64.zip","name":"1.143.1","version":"e64","productVersion":"1.143.1","hash":"7317","timestamp":1792690529437,"supportsFastUpdate":true}"#;
        let f = parse(json, "stable").unwrap();
        let r = f.latest.unwrap().release;
        assert_eq!(r.version.as_deref(), Some("1.143.1"));
        assert_eq!(
            r.release_notes_url.as_deref(),
            Some("https://code.visualstudio.com/updates/v1_143")
        );
        assert_eq!(
            r.download_url.as_deref(),
            Some("https://vscode.download.example/stable/e64/VSCode-darwin-arm64.zip")
        );
        assert_eq!(r.published.as_deref(), Some("2026-10-22T17:35:29Z"));
        assert!(parse("{}", "stable").is_err());
        assert!(parse("<html>", "stable").is_err());
        assert_eq!(
            latest_url(
                "https://update.code.visualstudio.com/",
                "darwin-arm64",
                "stable"
            ),
            "https://update.code.visualstudio.com/api/update/darwin-arm64/stable/latest"
        );
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_date(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso_date(1_792_690_529_437), "2026-10-22T17:35:29Z");
    }
}
