//! Working out which app an item belongs to: the enclosing `.app` bundle, or a bundle id
//! looked up through LaunchServices (CoreServices).

use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFArray, CFError, CFRetained, CFString};

use crate::cf;
use crate::model::OwnerApp;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    /// LSInfo.h (macOS 10.10+): every registered copy of the app with this bundle id.
    fn LSCopyApplicationURLsForBundleIdentifier(
        bundle_id: &CFString,
        out_error: *mut *mut CFError,
    ) -> Option<NonNull<CFArray>>;
}

/// Installed copies of the app with `bundle_id`, excluding copies in a Trash folder and
/// paths that no longer exist.
pub(crate) fn apps_for_bundle_id(bundle_id: &str) -> Vec<PathBuf> {
    if bundle_id.is_empty() {
        return Vec::new();
    }
    let id = CFString::from_str(bundle_id);
    let mut error: *mut CFError = ptr::null_mut();
    // SAFETY: valid CFString; `error` is a valid out-pointer (+1 CFError on failure).
    let array = unsafe { LSCopyApplicationURLsForBundleIdentifier(&id, &mut error) };
    if let Some(e) = NonNull::new(error) {
        // SAFETY: +1 reference handed to us; dropping releases it.
        drop(unsafe { CFRetained::from_raw(e) });
    }
    let Some(array) = array.map(|a| unsafe { CFRetained::from_raw(a) }) else {
        return Vec::new();
    };
    cf::array_items(&array)
        .iter()
        .filter_map(|v| cf::as_path(v))
        .map(|p| PathBuf::from(p.to_string_lossy().trim_end_matches('/')))
        .filter(|p| p.exists() && !p.components().any(|c| c.as_os_str() == ".Trash"))
        .collect()
}

/// Bundle id and display name from an app's `Contents/Info.plist`.
pub(crate) fn bundle_info(app: &Path) -> (Option<String>, Option<String>) {
    let Ok(value) = plist::Value::from_file(app.join("Contents/Info.plist")) else {
        return (None, None);
    };
    let Some(dict) = value.as_dictionary() else {
        return (None, None);
    };
    let get = |k: &str| dict.get(k).and_then(|v| v.as_string()).map(str::to_owned);
    let name = get("CFBundleDisplayName")
        .or_else(|| get("CFBundleName"))
        .filter(|n| !n.is_empty());
    (get("CFBundleIdentifier"), name)
}

/// "3.7.6 (4120)" from `CFBundleShortVersionString` and `CFBundleVersion` (either alone when
/// only one is set, or they're equal).
pub(crate) fn bundle_version(app: &Path) -> Option<String> {
    let value = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
    let dict = value.as_dictionary()?;
    let get = |k: &str| {
        dict.get(k)
            .and_then(|v| v.as_string())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    match (get("CFBundleShortVersionString"), get("CFBundleVersion")) {
        (Some(v), Some(b)) if v != b => Some(format!("{v} ({b})")),
        (Some(v), _) => Some(v),
        (None, b) => b,
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Owner from an app bundle path (which may no longer exist).
pub(crate) fn owner_from_app_path(app: &Path) -> OwnerApp {
    let installed = app.is_dir();
    let (bundle_id, name) = if installed {
        bundle_info(app)
    } else {
        (None, None)
    };
    OwnerApp {
        name: name.unwrap_or_else(|| file_stem(app)),
        bundle_id,
        path: Some(app.to_path_buf()),
        installed,
    }
}

/// Owner from bundle ids (`AssociatedBundleIdentifiers` or a BTM record), resolved with
/// `lookup` (LaunchServices in production). Not installed when none resolves.
pub(crate) fn owner_from_bundle_ids(
    ids: &[String],
    display_name: Option<&str>,
    lookup: &dyn Fn(&str) -> Vec<PathBuf>,
) -> Option<OwnerApp> {
    let first = ids.iter().find(|id| !id.is_empty())?;
    for id in ids {
        if let Some(path) = lookup(id).into_iter().next() {
            let mut owner = owner_from_app_path(&path);
            owner.bundle_id.get_or_insert_with(|| id.clone());
            return Some(owner);
        }
    }
    Some(OwnerApp {
        name: display_name
            .map(str::to_owned)
            .unwrap_or_else(|| first.clone()),
        bundle_id: Some(first.clone()),
        path: None,
        installed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_plist_and_paths() {
        let dir = crate::testutil::TempDir::new("owner");
        let app = dir.path().join("Example Thing.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            br#"<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.thing</string>
<key>CFBundleName</key><string>Thing</string>
</dict></plist>"#,
        )
        .unwrap();
        let owner = owner_from_app_path(&app);
        assert!(owner.installed);
        assert_eq!(owner.name, "Thing");
        assert_eq!(owner.bundle_id.as_deref(), Some("com.example.thing"));

        let gone = owner_from_app_path(&dir.path().join("Gone.app"));
        assert!(!gone.installed);
        assert_eq!(gone.name, "Gone");
    }

    #[test]
    fn bundle_id_resolution() {
        let lookup = |id: &str| -> Vec<PathBuf> {
            if id == "com.example.present" {
                vec![PathBuf::from("/nonexistent/Present.app")]
            } else {
                Vec::new()
            }
        };
        let owner =
            owner_from_bundle_ids(&["com.example.missing".into()], Some("Missing"), &lookup)
                .unwrap();
        assert!(!owner.installed);
        assert_eq!(owner.name, "Missing");
        let owner = owner_from_bundle_ids(
            &["com.example.missing".into(), "com.example.present".into()],
            None,
            &lookup,
        )
        .unwrap();
        assert_eq!(owner.bundle_id.as_deref(), Some("com.example.present"));
        assert!(owner_from_bundle_ids(&[], None, &lookup).is_none());
    }

    #[test]
    fn launch_services_lookup() {
        // Read-only LaunchServices query: Finder is always registered; a made-up id is not.
        assert!(!apps_for_bundle_id("com.apple.finder").is_empty());
        assert!(apps_for_bundle_id("com.example.kipimo.does-not-exist").is_empty());
    }
}
