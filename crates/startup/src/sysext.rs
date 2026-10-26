//! System extensions (from `sysextd`'s on-disk database) and loaded kernel extensions
//! (from IOKit's `KextManagerCopyLoadedKextInfo`).

use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use objc2_core_foundation::{CFArray, CFDictionary, CFRetained};
use plist::Value;

use crate::btm::percent_decode;
use crate::cf;

/// `sysextd` keeps its state here; world-readable, no root needed.
pub(crate) const DEFAULT_DB: &str = "/Library/SystemExtensions/db.plist";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SystemExtension {
    pub identifier: String,
    pub team_id: Option<String>,
    /// `activated_enabled`, `activated_disabled`, `terminated_waiting_to_uninstall_on_reboot`...
    pub state: String,
    pub categories: Vec<String>,
    pub version: Option<String>,
    /// The copy sysextd runs, under `/Library/SystemExtensions/<uuid>/`.
    pub staged_path: Option<PathBuf>,
    /// Where it came from inside its app.
    pub origin_path: Option<PathBuf>,
    pub app_path: Option<PathBuf>,
    pub app_bundle_id: Option<String>,
}

impl SystemExtension {
    pub fn enabled(&self) -> bool {
        self.state == "activated_enabled"
    }

    /// A readable category, for the name column.
    pub fn category_label(&self) -> Option<&'static str> {
        let c = self.categories.first()?;
        Some(match c.as_str() {
            "com.apple.system_extension.network_extension" => "Network extension",
            "com.apple.system_extension.endpoint_security" => "Endpoint security extension",
            "com.apple.system_extension.driver_extension" => "Driver extension",
            "com.apple.system_extension.cmio" => "Camera extension",
            _ => "System extension",
        })
    }
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded.trim_end_matches('/')))
}

/// Parses sysextd's `db.plist`.
pub(crate) fn parse_db(bytes: &[u8]) -> Result<Vec<SystemExtension>, String> {
    let value =
        Value::from_reader(std::io::Cursor::new(bytes)).map_err(|e| format!("db.plist: {e}"))?;
    let dict = value
        .as_dictionary()
        .ok_or("db.plist is not a dictionary")?;
    let Some(list) = dict.get("extensions").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for ext in list.iter().filter_map(Value::as_dictionary) {
        let s = |k: &str| ext.get(k).and_then(Value::as_string).map(str::to_owned);
        let Some(identifier) = s("identifier") else {
            continue;
        };
        let staged_path = match ext.get("stagedBundleURL") {
            Some(Value::Dictionary(d)) => d
                .get("relative")
                .and_then(Value::as_string)
                .and_then(file_url_to_path),
            Some(Value::String(u)) => file_url_to_path(u),
            _ => None,
        };
        let container = ext.get("container").and_then(Value::as_dictionary);
        let reference = ext
            .get("references")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .and_then(Value::as_dictionary);
        let version = ext
            .get("bundleVersion")
            .and_then(Value::as_dictionary)
            .and_then(|v| {
                v.get("CFBundleShortVersionString")
                    .or_else(|| v.get("CFBundleVersion"))
                    .and_then(Value::as_string)
                    .map(str::to_owned)
            });
        out.push(SystemExtension {
            identifier,
            team_id: s("teamID"),
            state: s("state").unwrap_or_default(),
            categories: ext
                .get("categories")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_string)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            version,
            staged_path,
            origin_path: s("originPath").map(PathBuf::from),
            app_path: container
                .and_then(|c| c.get("bundlePath"))
                .and_then(Value::as_string)
                .map(PathBuf::from),
            app_bundle_id: reference
                .and_then(|r| r.get("appIdentifier"))
                .and_then(Value::as_string)
                .map(str::to_owned),
        });
    }
    Ok(out)
}

pub(crate) fn read_db(path: &Path) -> Result<Vec<SystemExtension>, String> {
    match std::fs::read(path) {
        Ok(bytes) => parse_db(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// A loaded kernel extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kext {
    pub bundle_id: String,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub started: Option<bool>,
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// IOKit/kext/KextManager.h: info for loaded kexts (all of them with NULL arguments).
    fn KextManagerCopyLoadedKextInfo(
        kext_identifiers: Option<&CFArray>,
        info_keys: Option<&CFArray>,
    ) -> Option<NonNull<CFDictionary>>;
}

/// Loaded kernel extensions that aren't Apple's (`com.apple.*`).
pub(crate) fn loaded_third_party_kexts() -> Vec<Kext> {
    // SAFETY: NULL arguments mean "all kexts, all keys"; returns a +1 dictionary or NULL.
    let Some(dict) = (unsafe { KextManagerCopyLoadedKextInfo(None, None) }) else {
        return Vec::new();
    };
    // SAFETY: +1 reference handed to us.
    let dict: CFRetained<CFDictionary> = unsafe { CFRetained::from_raw(dict) };
    let mut out = Vec::new();
    for (key, value) in cf::dict_entries(&dict) {
        let Some(bundle_id) = cf::as_string(&key) else {
            continue;
        };
        if bundle_id.starts_with("com.apple.") || bundle_id == "__kernel__" {
            continue;
        }
        let Ok(info) = value.downcast::<CFDictionary>() else {
            continue;
        };
        out.push(Kext {
            path: cf::dict_get_str(&info, "OSBundlePath").map(PathBuf::from),
            version: cf::dict_get_str(&info, "CFBundleVersion"),
            started: cf::dict_get_bool(&info, "OSBundleStarted"),
            bundle_id,
        });
    }
    out.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>extensions</key><array>
  <dict>
    <key>identifier</key><string>com.example.Camera</string>
    <key>teamID</key><string>EXAMPLE123</string>
    <key>state</key><string>activated_enabled</string>
    <key>categories</key><array><string>com.apple.system_extension.cmio</string></array>
    <key>bundleVersion</key><dict><key>CFBundleShortVersionString</key><string>1.7</string></dict>
    <key>container</key><dict><key>bundlePath</key><string>/Applications/Example Cam.app</string></dict>
    <key>originPath</key><string>/Applications/Example Cam.app/Contents/Library/SystemExtensions/com.example.Camera.systemextension</string>
    <key>references</key><array><dict><key>appIdentifier</key><string>com.example.cam</string></dict></array>
    <key>stagedBundleURL</key><dict><key>relative</key><string>file:///Library/SystemExtensions/0000/com.example.Camera.systemextension/</string></dict>
  </dict>
  <dict>
    <key>identifier</key><string>com.example.Filter</string>
    <key>state</key><string>terminated_waiting_to_uninstall_on_reboot</string>
    <key>categories</key><array><string>com.apple.system_extension.network_extension</string></array>
  </dict>
  <dict><key>state</key><string>no identifier, skipped</string></dict>
</array>
<key>version</key><integer>1</integer>
</dict></plist>"#;

    #[test]
    fn parses_sysext_db() {
        let exts = parse_db(DB).unwrap();
        assert_eq!(exts.len(), 2);
        let cam = &exts[0];
        assert!(cam.enabled());
        assert_eq!(cam.category_label(), Some("Camera extension"));
        assert_eq!(cam.version.as_deref(), Some("1.7"));
        assert_eq!(cam.app_bundle_id.as_deref(), Some("com.example.cam"));
        assert_eq!(
            cam.app_path,
            Some(PathBuf::from("/Applications/Example Cam.app"))
        );
        assert_eq!(
            cam.staged_path,
            Some(PathBuf::from(
                "/Library/SystemExtensions/0000/com.example.Camera.systemextension"
            ))
        );
        assert!(!exts[1].enabled());
        assert_eq!(exts[1].category_label(), Some("Network extension"));
        assert!(parse_db(b"not a plist").is_err());
        assert!(
            parse_db(br#"<plist version="1.0"><dict/></plist>"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn kext_query_runs() {
        // Read-only IOKit query; most Apple silicon Macs have no third-party kexts loaded.
        for kext in loaded_third_party_kexts() {
            assert!(!kext.bundle_id.starts_with("com.apple."));
        }
    }
}
