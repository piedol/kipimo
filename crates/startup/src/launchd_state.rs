//! Live launchd and Background Task Management state, read through ServiceManagement.
//!
//! `SMCopyAllJobDictionaries` / `SMJobCopyDictionary` / `SMJobRemove` are deprecated but are
//! still the only public C API into launchd's job table (the replacement Apple promised in
//! libxpc never shipped publicly); they work without root for reading both domains and for
//! removing jobs from the caller's own (gui) domain. `SMAppService.statusForLegacyURL:`
//! (macOS 13+) gives the "Allow in the Background" state of a legacy plist.

#![allow(deprecated)]

use std::collections::HashMap;
use std::path::Path;
use std::ptr;

use objc2_core_foundation::{CFDictionary, CFError, CFRetained, CFString};
use objc2_foundation::NSURL;
use objc2_service_management::{
    SMAppService, SMAppServiceStatus, SMCopyAllJobDictionaries, SMJobCopyDictionary, SMJobRemove,
    kSMDomainSystemLaunchd, kSMDomainUserLaunchd,
};

use crate::cf;
use crate::model::BackgroundStatus;

/// A job as launchd reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub pid: Option<i32>,
}

fn domain(system: bool) -> Option<&'static CFString> {
    // SAFETY: reading framework-exported constant pointers.
    unsafe {
        if system {
            kSMDomainSystemLaunchd
        } else {
            kSMDomainUserLaunchd
        }
    }
}

/// Every job loaded in the user's (gui) domain or the system domain, keyed by label.
pub(crate) fn jobs(system: bool) -> HashMap<String, Job> {
    let mut map = HashMap::new();
    let Some(domain) = domain(system) else {
        return map;
    };
    // SAFETY: valid domain constant; returns a +1 array or NULL.
    let Some(array) = (unsafe { SMCopyAllJobDictionaries(Some(domain)) }) else {
        return map;
    };
    for item in cf::array_items(&array) {
        let Ok(dict) = item.downcast::<CFDictionary>() else {
            continue;
        };
        let Some(label) = cf::dict_get_str(&dict, "Label") else {
            continue;
        };
        let pid = cf::dict_get_i64(&dict, "PID")
            .and_then(|p| i32::try_from(p).ok())
            .filter(|p| *p > 0);
        map.insert(label, Job { pid });
    }
    map
}

/// Is `label` loaded in the user's domain right now?
pub(crate) fn user_job_loaded(label: &str) -> bool {
    let Some(domain) = domain(false) else {
        return false;
    };
    let label = CFString::from_str(label);
    // SAFETY: valid arguments; returns a +1 dictionary or NULL.
    unsafe { SMJobCopyDictionary(Some(domain), Some(&label)) }.is_some()
}

/// Removes `label` from the user's launchd domain (stops it for this session; equivalent to
/// `launchctl remove`). Blocks until the process has exited.
pub(crate) fn remove_user_job(label: &str) -> Result<(), String> {
    let Some(domain) = domain(false) else {
        return Err("launchd user domain unavailable".into());
    };
    let label = CFString::from_str(label);
    let mut error: *mut CFError = ptr::null_mut();
    // SAFETY: valid arguments; a NULL AuthorizationRef is allowed for the user domain; `error`
    // is a valid out-pointer that receives a +1 CFError on failure.
    let ok = unsafe {
        SMJobRemove(
            Some(domain),
            Some(&label),
            ptr::null_mut(),
            true,
            &mut error,
        )
    };
    let error = ptr::NonNull::new(error).map(|p| unsafe { CFRetained::from_raw(p) });
    if ok {
        Ok(())
    } else {
        Err(error
            .map(|e| cf::error_message(&e))
            .unwrap_or_else(|| "launchd refused to remove the job".into()))
    }
}

/// "Allow in the Background" state of a legacy launchd plist.
pub(crate) fn legacy_status(plist: &Path) -> BackgroundStatus {
    let Some(url) = NSURL::from_file_path(plist) else {
        return BackgroundStatus::Unknown;
    };
    // SAFETY: plain class method call with a valid NSURL (macOS 13+, which Kipimo requires).
    let status = unsafe { SMAppService::statusForLegacyURL(&url) };
    background_from_sm(status)
}

pub(crate) fn background_from_sm(status: SMAppServiceStatus) -> BackgroundStatus {
    match status {
        SMAppServiceStatus::Enabled => BackgroundStatus::Allowed,
        SMAppServiceStatus::RequiresApproval => BackgroundStatus::TurnedOff,
        SMAppServiceStatus::NotRegistered => BackgroundStatus::NotRegistered,
        SMAppServiceStatus::NotFound => BackgroundStatus::NotFound,
        _ => BackgroundStatus::Unknown,
    }
}

/// launchd's persistent enable/disable overrides (`launchctl disable/enable`):
/// `/private/var/db/com.apple.xpc.launchd/disabled.<uid>.plist` for the user's gui domain and
/// `disabled.plist` for the system domain. World-readable. `true` = disabled.
pub(crate) fn read_overrides(path: &Path) -> HashMap<String, bool> {
    let Ok(value) = plist::Value::from_file(path) else {
        return HashMap::new();
    };
    value
        .as_dictionary()
        .map(|d| {
            d.iter()
                .filter_map(|(k, v)| v.as_boolean().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_fixture() {
        let dir = crate::testutil::TempDir::new("overrides");
        let path = dir.path().join("disabled.501.plist");
        std::fs::write(
            &path,
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>com.example.off</key><true/>
<key>com.example.on</key><false/>
<key>com.example.weird</key><string>x</string>
</dict></plist>"#,
        )
        .unwrap();
        let map = read_overrides(&path);
        assert_eq!(map.get("com.example.off"), Some(&true));
        assert_eq!(map.get("com.example.on"), Some(&false));
        assert_eq!(map.len(), 2);
        assert!(read_overrides(&dir.path().join("missing.plist")).is_empty());
    }

    #[test]
    fn job_tables_are_readable() {
        // Read-only: both domains list jobs without root.
        assert!(!jobs(false).is_empty());
        assert!(!jobs(true).is_empty());
        assert!(!user_job_loaded("top.piedol.kipimo.test.never-loaded"));
    }
}
