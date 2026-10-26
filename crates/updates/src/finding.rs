//! What a feed says (a [`Finding`], cacheable and independent of the installed copy) and how
//! that turns into a [`Status`] for one installed app.

use serde::{Deserialize, Serialize};

use crate::types::{InstalledApp, Release, Status, UpdateSource};
use crate::version::{compare_sparkle, is_comparable, lenient_is_newer, sparkle_is_newer};

/// This Mac, as far as update checks care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Env {
    /// macOS product version, "26.6.2".
    pub macos: String,
    /// Darwin kernel version, "25.6.0" (electron-updater compares against this).
    pub darwin: String,
    pub arm64: bool,
    /// Sparkle channels to accept besides the default one.
    pub channels: Vec<String>,
    /// App Store region, lower case.
    pub country: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Critical {
    All,
    /// Critical only for installed versions below this one.
    Below(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub release: Release,
    #[serde(default)]
    pub critical: Option<Critical>,
}

impl Candidate {
    pub fn new(release: Release) -> Self {
        Candidate {
            release,
            critical: None,
        }
    }
}

/// The result of reading one feed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finding {
    /// The newest release this Mac can run.
    pub latest: Option<Candidate>,
    /// A newer release than `latest` that needs a newer macOS.
    pub newest_incompatible: Option<Candidate>,
}

/// Why a check didn't produce a finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub reason: String,
    /// Network trouble and the like: not cached, retried on the next run.
    pub transient: bool,
}

impl Failure {
    pub fn definite(reason: impl Into<String>) -> Failure {
        Failure {
            reason: reason.into(),
            transient: false,
        }
    }
}

impl From<String> for Failure {
    fn from(reason: String) -> Failure {
        Failure::definite(reason)
    }
}

pub(crate) struct Evaluation {
    pub status: Status,
    pub latest: Option<Release>,
    pub newer_requires_macos: Option<Release>,
    pub critical: bool,
}

fn pick<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.filter(|v| is_comparable(v))
        .or(b.filter(|v| is_comparable(v)))
}

/// Is `rel` newer than what's installed? Sparkle feeds compare builds with Sparkle's rules
/// (what the app's own updater does); everything else compares display versions leniently.
fn is_newer(source: &UpdateSource, rel: &Release, app: &InstalledApp) -> Result<bool, String> {
    let missing = || "the app doesn't declare its version".to_string();
    if matches!(source, UpdateSource::Sparkle { .. })
        && let (Some(b), Some(ib)) = (rel.build.as_deref(), app.build.as_deref())
        && is_comparable(b)
        && is_comparable(ib)
    {
        return Ok(sparkle_is_newer(b, ib));
    }
    let candidate = pick(rel.version.as_deref(), rel.build.as_deref())
        .ok_or("the feed doesn't give a version")?;
    let installed = pick(app.version.as_deref(), app.build.as_deref()).ok_or_else(missing)?;
    Ok(lenient_is_newer(candidate, installed))
}

fn is_critical(c: &Candidate, app: &InstalledApp) -> bool {
    match &c.critical {
        None => false,
        Some(Critical::All) => true,
        Some(Critical::Below(v)) => {
            let installed = app
                .build
                .as_deref()
                .or(app.version.as_deref())
                .unwrap_or("");
            compare_sparkle(installed, v) == std::cmp::Ordering::Less
        }
    }
}

pub(crate) fn evaluate(app: &InstalledApp, finding: &Finding) -> Evaluation {
    let fail = |reason: String| Evaluation {
        status: Status::CheckFailed { reason },
        latest: finding.latest.as_ref().map(|c| c.release.clone()),
        newer_requires_macos: None,
        critical: false,
    };
    let latest_newer = match finding
        .latest
        .as_ref()
        .map(|c| is_newer(&app.source, &c.release, app))
    {
        Some(Err(e)) => return fail(e),
        Some(Ok(n)) => n,
        None => false,
    };
    let incompatible_newer = match finding
        .newest_incompatible
        .as_ref()
        .map(|c| is_newer(&app.source, &c.release, app))
    {
        Some(Err(e)) => return fail(e),
        Some(Ok(n)) => n,
        None => false,
    };
    let incompatible = finding
        .newest_incompatible
        .as_ref()
        .filter(|_| incompatible_newer);
    if latest_newer {
        let c = finding
            .latest
            .as_ref()
            .expect("latest_newer implies latest");
        Evaluation {
            status: Status::UpdateAvailable,
            latest: Some(c.release.clone()),
            newer_requires_macos: incompatible.map(|c| c.release.clone()),
            critical: is_critical(c, app),
        }
    } else if let Some(c) = incompatible {
        Evaluation {
            status: Status::RequiresNewerMacos {
                minimum: c
                    .release
                    .minimum_system_version
                    .clone()
                    .unwrap_or_else(|| "a newer version".into()),
            },
            latest: Some(c.release.clone()),
            newer_requires_macos: None,
            critical: false,
        }
    } else {
        Evaluation {
            status: Status::UpToDate,
            latest: finding.latest.as_ref().map(|c| c.release.clone()),
            newer_requires_macos: None,
            critical: false,
        }
    }
}
