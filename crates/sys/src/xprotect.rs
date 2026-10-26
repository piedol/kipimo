//! XProtect, macOS's built-in malware protection: is it installed, which
//! definitions version, and when they last changed. Read from the bundles on
//! disk; no network.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use crate::cf::{self, Dict};

/// Where macOS keeps the definitions. Since macOS 15 the XProtect service
/// keeps its own updated copy under /private/var/protected.
pub const DEFINITION_BUNDLES: [&str; 2] = [
    "/private/var/protected/xprotect/XProtect.bundle",
    "/Library/Apple/System/Library/CoreServices/XProtect.bundle",
];
/// XProtect Remediator, the scanner that runs in the background.
pub const REMEDIATOR_APP: &str = "/Library/Apple/System/Library/CoreServices/XProtect.app";

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct XProtect {
    /// The definitions bundle exists. XProtect can't be switched off, so this means "on".
    pub installed: bool,
    /// Definitions version, e.g. "5368".
    pub version: Option<String>,
    /// When the newest definitions were written to disk.
    pub updated: Option<SystemTime>,
    pub remediator_version: Option<String>,
}

/// A bundle's short version and the modification time of its Info.plist.
pub(crate) fn bundle_info(bundle: &Path) -> Option<(String, SystemTime)> {
    let plist = bundle.join("Contents/Info.plist");
    let bytes = std::fs::read(&plist).ok()?;
    let d = Dict::from_value(&*cf::plist(&bytes)?)?;
    let v = d.string("CFBundleShortVersionString")?;
    let t = std::fs::metadata(&plist).and_then(|m| m.modified()).ok()?;
    Some((v, t))
}

/// Numeric-aware version comparison ("5368" > "999", "2.10" > "2.9").
pub(crate) fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// The newest definitions: highest version, then latest write.
pub(crate) fn newest(found: Vec<(String, SystemTime)>) -> Option<(String, SystemTime)> {
    found.into_iter().max_by(|a, b| {
        version_key(&a.0)
            .cmp(&version_key(&b.0))
            .then(a.1.cmp(&b.1))
    })
}

/// XProtect's status from the given bundle locations (tests pass synthetic ones).
pub(crate) fn status_from(bundles: &[PathBuf], remediator: &Path) -> XProtect {
    let found: Vec<_> = bundles.iter().filter_map(|b| bundle_info(b)).collect();
    let installed = !found.is_empty() || bundles.iter().any(|b| b.is_dir());
    let newest = newest(found);
    XProtect {
        installed,
        version: newest.as_ref().map(|n| n.0.clone()),
        updated: newest.map(|n| n.1),
        remediator_version: bundle_info(remediator).map(|r| r.0),
    }
}

pub fn status() -> XProtect {
    let bundles: Vec<PathBuf> = DEFINITION_BUNDLES.iter().map(PathBuf::from).collect();
    status_from(&bundles, Path::new(REMEDIATOR_APP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write_bundle(root: &Path, name: &str, version: &str) -> PathBuf {
        let b = root.join(name);
        std::fs::create_dir_all(b.join("Contents")).unwrap();
        std::fs::write(
            b.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>"#
            ),
        )
        .unwrap();
        b
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_key("5368") > version_key("999"));
        assert!(version_key("2.10") > version_key("2.9"));
    }

    #[test]
    fn newest_prefers_version_then_time() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(
            newest(vec![("5367".into(), t1), ("5368".into(), t0)])
                .unwrap()
                .0,
            "5368"
        );
        assert_eq!(
            newest(vec![("5368".into(), t0), ("5368".into(), t1)])
                .unwrap()
                .1,
            t1
        );
        assert!(newest(vec![]).is_none());
    }

    #[test]
    fn status_from_synthetic_bundles() {
        let root = std::env::temp_dir().join(format!("kipimo-xp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = write_bundle(&root, "A/XProtect.bundle", "5367");
        let b = write_bundle(&root, "B/XProtect.bundle", "5368");
        let r = write_bundle(&root, "XProtect.app", "170");
        let s = status_from(&[a, b, root.join("missing")], &r);
        assert!(s.installed);
        assert_eq!(s.version.as_deref(), Some("5368"));
        assert!(s.updated.is_some());
        assert_eq!(s.remediator_version.as_deref(), Some("170"));
        let none = status_from(&[root.join("nope")], &root.join("nope.app"));
        assert!(!none.installed && none.version.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
