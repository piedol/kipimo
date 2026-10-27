//! Text from the engine crates, translated for display. The engines (`clean`, `space`,
//! `startup`, `updates`, `sys`) have no UI and no translations, so their text is translated
//! here, where it is shown: enums map to a `tr` literal for each value, and the English
//! sentences the engines return as strings (reasons, hints, errors) are recognised as one of
//! the known sentences, exactly or as a template with `{placeholders}`. Anything else, such as
//! file and app names or messages macOS already wrote in the user's language, passes through.
//!
//! The tests below compare the English here with what the engines produce, so a sentence
//! changed in an engine shows up as a failing test rather than as untranslated text.

use kipimo_clean::CategoryId;
use kipimo_space::{SpaceError, TrashSkip};
use kipimo_startup::{AppCheck, Flag, ReplaceError, Signature, SignatureStatus};

use crate::i18n::{tr, trf, trn};

/// The placeholder values when `s` reads as `template` ("couldn't unload {label}: {error}"),
/// in template order. A placeholder used twice must match the same text both times. Values
/// of `{error}`, `{reason}` and `{problem}` are engine sentences themselves, so they come back
/// translated.
fn fields<'t>(s: &str, template: &'t str) -> Option<Vec<(&'t str, String)>> {
    let open = template.find('{').unwrap_or(template.len());
    let mut rest = s.strip_prefix(&template[..open])?;
    let mut t = &template[open..];
    let mut out: Vec<(&'t str, String)> = Vec::new();
    while !t.is_empty() {
        let close = t.find('}')?;
        let name = &t[1..close];
        t = &t[close + 1..];
        let next = t.find('{').unwrap_or(t.len());
        let literal = &t[..next];
        let value = if next == t.len() {
            // The last placeholder runs up to the closing text.
            let v = rest.strip_suffix(literal)?;
            rest = "";
            v
        } else {
            if literal.is_empty() {
                return None;
            }
            let at = rest.find(literal)?;
            let v = &rest[..at];
            rest = &rest[at + literal.len()..];
            v
        };
        if value.is_empty() {
            return None;
        }
        match out.iter().find(|(n, _)| *n == name) {
            Some((_, seen)) if seen != value => return None,
            Some(_) => {}
            None => {
                let value = match name {
                    "error" | "reason" | "problem" => text(value),
                    _ => value.to_string(),
                };
                out.push((name, value));
            }
        }
        t = &t[next..];
    }
    rest.is_empty().then_some(out)
}

fn pairs<'a>(v: &'a [(&'a str, String)]) -> Vec<(&'a str, &'a str)> {
    v.iter().map(|(n, s)| (*n, s.as_str())).collect()
}

/// `known!(s, tr("…"))` returns the translation when `s` is that sentence;
/// `known!(s, trf("… {name} …"))` when `s` reads as that template, with its values put back.
/// Written as a `tr`/`trf` call so the catalog test finds the literal.
macro_rules! known {
    ($s:expr, tr($key:literal)) => {
        if $s == $key {
            return tr($key);
        }
    };
    ($s:expr, trf($key:literal)) => {
        if let Some(v) = fields($s, $key) {
            return trf($key, &pairs(&v));
        }
    };
}

/// A sentence an engine returned as a string: a reason, a hint, a note or an error.
pub fn text(s: &str) -> String {
    // Clean up: notes on items, and errors.
    known!(
        s,
        tr(
            "Unpacked crate sources that builds and rust-analyzer read. Cargo unpacks them again, which is slow for big projects."
        )
    );
    known!(
        s,
        tr(
            "Projects hard-link their packages from this store, so moving it frees little until they reinstall."
        )
    );
    known!(
        s,
        tr(
            "Browsers Playwright downloaded for tests. They download again on the next test run, several hundred MB each."
        )
    );
    known!(
        s,
        tr("Cypress's test runner app. It downloads again on the next install.")
    );
    known!(
        s,
        tr("Browsers Puppeteer downloaded. They download again when needed.")
    );
    known!(
        s,
        tr("Prisma's database engines. They download again on the next install.")
    );
    known!(
        s,
        tr("Corepack's copies of Yarn and pnpm. They download again when a project needs them.")
    );
    known!(
        s,
        tr(
            "May hold downloaded AI models. Downloading them again is slow, so check before cleaning."
        )
    );
    known!(
        s,
        tr(
            "Holds models compiled for the Neural Engine. The app rebuilds them, slowly, on next use."
        )
    );
    known!(s, tr("deleted device"));
    known!(s, tr("runtime not installed"));
    known!(s, tr("built for an older macOS"));
    known!(s, tr("couldn't find the user's home folder"));
    known!(s, tr("bad path"));
    // Space.
    known!(s, tr("not a valid file path"));
    // Startup items: disabling, and the scan's worker process.
    known!(s, tr("launch daemons are always system-wide"));
    known!(
        s,
        tr(
            "kernel extensions are removed with their app's uninstaller; Kipimo doesn't unload kernel code"
        )
    );
    known!(s, tr("no plist path"));
    known!(s, tr("the plist is already gone"));
    known!(s, tr("the plist changed since the scan; scan again"));
    known!(s, tr("Turn it off under Extensions."));
    known!(s, tr("path can't be expressed as a file URL"));
    known!(s, tr("launchd user domain unavailable"));
    known!(s, tr("launchd refused to remove the job"));
    known!(s, tr("worker has no stdin"));
    known!(s, tr("worker has no stdout"));
    known!(s, tr("worker exited before finishing"));
    known!(s, trf("{plist} is not in {folder}"));
    known!(s, trf("couldn't unload {label}: {error}"));
    known!(
        s,
        trf("unloaded, but couldn't move the plist to the Trash: {error}")
    );
    known!(s, trf("couldn't move the plist to the Trash: {error}"));
    known!(s, trf("Remove {name} from \"Open at Login\"."));
    known!(s, trf("Turn off {name} under \"Allow in the Background\"."));
    known!(
        s,
        trf(
            "Turn it off under Extensions, or remove it by moving {name} to the Trash (macOS asks to remove its extension)."
        )
    );
    known!(s, trf("{path} no longer exists"));
    known!(s, trf("can't start worker: {error}"));
    known!(s, trf("can't send job: {error}"));
    known!(s, trf("bad worker output: {error}"));
    known!(s, trf("bad job: {error}"));
    // Startup items: replacing a damaged app.
    known!(s, tr("it isn't an app bundle any more"));
    known!(
        s,
        tr("its signature names no developer team, so a download can't be matched to it")
    );
    known!(
        s,
        tr("it was installed by an administrator, so replacing it needs one")
    );
    known!(s, tr("the downloaded copy's signature doesn't check out"));
    known!(
        s,
        tr("the downloaded copy isn't signed with a Developer ID")
    );
    known!(s, tr("the downloaded copy's signature names no team"));
    known!(
        s,
        tr("the download holds a different app (no bundle identifier)")
    );
    known!(
        s,
        tr("the download landed on a different disk from the app")
    );
    known!(s, tr("the download doesn't hold this app"));
    known!(s, tr("the download is an empty archive"));
    known!(s, tr("the download isn't a zip archive"));
    known!(s, tr("the download couldn't be unpacked"));
    known!(s, tr("the app's path isn't a file URL"));
    known!(s, tr("the download folder has no path"));
    known!(
        s,
        trf("the downloaded copy's signature doesn't check out ({problem})")
    );
    known!(
        s,
        trf("the downloaded copy is signed by team {team}, not {installed} like the installed one")
    );
    known!(s, trf("the download holds a different app ({id})"));
    known!(
        s,
        trf("macOS didn't let Kipimo move the old copy to the Trash ({error})")
    );
    known!(s, trf("couldn't move the old copy to the Trash: {error}"));
    known!(s, trf("couldn't put the new copy in place: {error}"));
    known!(s, trf("couldn't unpack the download: {error}"));
    known!(s, trf("the download couldn't be unpacked ({error})"));
    known!(
        s,
        trf("the download has a file that would land outside its folder ({name})")
    );
    known!(
        s,
        trf("the download writes through a link inside itself ({name})")
    );
    // Updates: why a check failed.
    known!(s, tr("cancelled"));
    known!(s, tr("the update feed isn't an appcast"));
    known!(s, tr("the update service doesn't give a version"));
    known!(s, tr("the App Store doesn't list a version"));
    known!(
        s,
        tr("the update feed is a web page, not a latest-mac.yml file")
    );
    known!(s, tr("the update feed has no version"));
    known!(s, tr("the app doesn't declare its version"));
    known!(s, tr("the feed doesn't give a version"));
    known!(s, tr("the app has no bundle identifier"));
    known!(s, tr("not in the App Store's answer"));
    known!(s, tr("the update feed lists no releases"));
    known!(s, tr("the GitHub repository is private or needs a sign-in"));
    known!(s, tr("not checked yet"));
    known!(s, tr("the check failed unexpectedly"));
    known!(s, tr("the check didn't finish"));
    known!(s, tr("the App Store wasn't found (HTTP 404)"));
    known!(s, tr("the update feed wasn't found (HTTP 404)"));
    known!(s, tr("the App Store asked Kipimo to slow down (HTTP 429)"));
    known!(
        s,
        tr("the update feed asked Kipimo to slow down (HTTP 429)")
    );
    known!(s, tr("the App Store is unexpectedly large"));
    known!(s, tr("the update feed is unexpectedly large"));
    known!(s, tr("the App Store redirects too many times"));
    known!(s, tr("the update feed redirects too many times"));
    known!(s, trf("the update feed isn't valid XML ({error})"));
    known!(
        s,
        trf("the update service sent an unexpected answer ({error})")
    );
    known!(s, trf("the App Store sent an unexpected answer ({error})"));
    known!(s, trf("not found in the App Store ({region} region)"));
    known!(s, trf("the App Store answered with HTTP {code}"));
    known!(s, trf("the update feed answered with HTTP {code}"));
    known!(s, trf("couldn't reach the App Store: {error}"));
    known!(s, trf("couldn't reach the update feed: {error}"));
    known!(
        s,
        trf("the App Store has an address Kipimo won't fetch ({url})")
    );
    known!(
        s,
        trf("the update feed has an address Kipimo won't fetch ({url})")
    );
    // Updates: how to install with the app's own updater.
    known!(
        s,
        tr("Update macOS in System Settings › General › Software Update.")
    );
    known!(s, tr("Update it from the App Store's Updates page."));
    known!(
        s,
        trf("{name} can install it itself: choose Check for Updates… in its app menu.")
    );
    known!(
        s,
        trf("{name} can install it itself: choose {name} › Check for Updates…, or restart it.")
    );
    known!(
        s,
        trf("{name} installs updates itself, usually when you quit and reopen it.")
    );
    known!(
        s,
        trf("{name} checks for updates itself: choose Check for Updates… in its app menu.")
    );
    known!(
        s,
        trf("{name} updates itself, usually when you quit and reopen it, or from its menu.")
    );
    known!(
        s,
        trf("{name} updates itself: open About {name} in its app menu to check.")
    );
    known!(
        s,
        trf("{name} updates itself: choose {name} › About {name} to check.")
    );
    known!(
        s,
        trf("Choose Help › Check for Updates in {name} (Microsoft AutoUpdate).")
    );
    known!(
        s,
        trf("{name} is updated with macOS in System Settings › General › Software Update.")
    );
    // Updates: downloads (Download and Replace).
    known!(s, tr("the download is larger than Kipimo accepts"));
    known!(s, tr("the download ended early"));
    known!(s, tr("the download link redirects too many times"));
    known!(s, trf("Kipimo only downloads http and https links ({url})"));
    known!(s, trf("invalid system proxy {proxy}"));
    known!(s, trf("couldn't reach the server: {error}"));
    known!(s, trf("couldn't reach {host}: {error}"));
    known!(s, trf("a redirect without a location (HTTP {code})"));
    known!(
        s,
        trf("a redirect to an address Kipimo won't fetch ({url})")
    );
    known!(s, trf("the server answered with HTTP {code}"));
    known!(s, trf("{host} answered with HTTP {code}"));
    known!(s, trf("couldn't create the download file: {error}"));
    known!(s, trf("the download stopped: {error}"));
    known!(s, trf("couldn't save the download: {error}"));
    s.to_string()
}

/// A Clean up category's name.
pub fn category_title(id: CategoryId) -> String {
    match id {
        CategoryId::AppCaches => tr("App caches"),
        CategoryId::SystemCaches => tr("macOS caches"),
        CategoryId::Logs => tr("Logs and crash reports"),
        CategoryId::XcodeDerivedData => tr("Xcode build data"),
        CategoryId::XcodeArchives => tr("Old Xcode archives"),
        CategoryId::XcodeDeviceSupport => tr("Old device symbols"),
        CategoryId::XcodeSimulators => tr("Unavailable simulators"),
        CategoryId::DeveloperCaches => tr("Developer caches"),
        CategoryId::MailDownloads => tr("Mail downloads"),
        CategoryId::IosBackups => tr("Old iPhone and iPad backups"),
        CategoryId::Trash => tr("Trash"),
    }
}

/// The first sentence of a category's description, which its row shows.
pub fn category_summary(id: CategoryId) -> String {
    match id {
        CategoryId::AppCaches => tr("Files apps keep in ~/Library/Caches to start faster."),
        CategoryId::SystemCaches => tr("Caches of macOS's own services (com.apple.*)."),
        CategoryId::Logs => {
            tr("App logs in ~/Library/Logs and crash reports in DiagnosticReports.")
        }
        CategoryId::XcodeDerivedData => tr("DerivedData: intermediate build products and indexes."),
        CategoryId::XcodeArchives => tr("Archives older than the age limit."),
        CategoryId::XcodeDeviceSupport => tr(
            "iOS, watchOS, tvOS and visionOS DeviceSupport for versions no device has used recently.",
        ),
        CategoryId::XcodeSimulators => tr("Simulators that can't boot any more."),
        CategoryId::DeveloperCaches => tr(
            "Package downloads kept by Homebrew, npm, Yarn, pnpm, Cargo, pip, Gradle, CocoaPods and Go.",
        ),
        CategoryId::MailDownloads => tr("Copies of attachments Mail saved when you opened them."),
        CategoryId::IosBackups => tr("Local device backups older than the age limit."),
        CategoryId::Trash => tr("What's already in the Trash."),
    }
}

/// An item's name in a Clean up category. Most are file or folder names and stay as they
/// are; the developer caches have names of their own, and a few kinds of item are described.
pub fn junk_label(category: CategoryId, label: &str) -> String {
    match category {
        CategoryId::DeveloperCaches => {
            known!(label, tr("Homebrew downloads"));
            known!(label, tr("npm cache"));
            known!(label, tr("Yarn cache"));
            known!(label, tr("Yarn (Berry) cache"));
            known!(label, tr("pnpm store"));
            known!(label, tr("pnpm metadata cache"));
            known!(label, tr("Cargo registry downloads"));
            known!(label, tr("Cargo registry sources"));
            known!(label, tr("Cargo git checkouts"));
            known!(label, tr("pip cache"));
            known!(label, tr("Gradle caches"));
            known!(label, tr("CocoaPods cache"));
            known!(label, tr("Go build cache"));
        }
        CategoryId::Logs => known!(label, trf("Crash report: {name}")),
        CategoryId::IosBackups => {
            known!(label, tr("Device backup"));
            known!(label, trf("{device} backup"));
        }
        CategoryId::XcodeSimulators => {
            known!(label, trf("Simulator caches for macOS {build}"));
            known!(label, trf("Simulator caches for {runtime}"));
        }
        _ => {}
    }
    label.to_string()
}

/// What is wrong with a startup item, in a short sentence.
pub fn flag(f: Flag) -> String {
    match f {
        Flag::Unsigned => tr("Not signed"),
        Flag::AdHocSigned => tr("Ad-hoc signed (no developer identity)"),
        Flag::InvalidSignature => tr("Signature no longer valid"),
        Flag::UntrustedCertificate => tr("Signed with a certificate Apple didn't issue"),
        Flag::ProgramMissing => tr("Program is missing"),
        Flag::OwnerAppMissing => tr("Its app is no longer installed"),
        Flag::RunsScript => tr("Runs a script"),
        Flag::ScriptMissing => tr("Script is missing"),
        Flag::MalformedPlist => tr("Plist is damaged or incomplete"),
    }
}

/// Who signed something ([`kipimo_startup::signer_summary`]): a name, or why there is none.
pub fn signer(sig: &Signature) -> String {
    let s = kipimo_startup::signer_summary(sig);
    if sig.signer.is_some() {
        return s;
    }
    match s.as_str() {
        "ad-hoc" => tr("ad-hoc"),
        "unsigned" => tr("unsigned"),
        "unknown" => tr("unknown"),
        _ => s,
    }
}

/// Why an app's signature is broken ([`AppCheck::reason`]): macOS's own description, with
/// how many sealed files changed.
pub fn app_check_reason(a: &AppCheck) -> String {
    let Some(p) = &a.signature.problem else {
        return match a.signature.status {
            SignatureStatus::Unsigned => tr("not signed"),
            _ => String::new(),
        };
    };
    let mut parts = Vec::new();
    if p.missing > 0 {
        parts.push(trn(
            p.missing as u64,
            "{n} file missing",
            "{n} files missing",
        ));
    }
    if p.altered > 0 {
        parts.push(trn(
            p.altered as u64,
            "{n} file changed",
            "{n} files changed",
        ));
    }
    if p.added > 0 {
        parts.push(trn(p.added as u64, "{n} file added", "{n} files added"));
    }
    if parts.is_empty() {
        p.message.clone()
    } else {
        format!("{} ({})", p.message, parts.join(", "))
    }
}

/// Why a Download and Replace didn't happen ([`ReplaceError::message`]).
pub fn replace_error(e: &ReplaceError) -> String {
    match e {
        ReplaceError::NotReplaceable(m)
        | ReplaceError::BadDownload(m)
        | ReplaceError::Untrusted(m)
        | ReplaceError::Blocked(m) => text(m),
        ReplaceError::Failed {
            error,
            old_in_trash: Some(p),
        } => trf(
            "{error}. The old copy is in the Trash at {path}; Put Back in Finder restores it.",
            &[("error", &text(error)), ("path", &p.display().to_string())],
        ),
        ReplaceError::Failed { error, .. } => text(error),
    }
}

/// Why Space didn't read a folder ([`kipimo_space::SkipReason::message`]).
pub fn space_skip(r: &kipimo_space::SkipReason) -> String {
    use kipimo_space::SkipReason;
    match r {
        SkipReason::NeedsFullDiskAccess => tr("Needs Full Disk Access"),
        SkipReason::ProtectedByMacOS => tr("Protected by macOS"),
        SkipReason::PermissionDenied => tr("No permission to read this folder"),
        SkipReason::OtherVolume { fs_type, .. } => {
            trf("Another volume ({type})", &[("type", fs_type.as_str())])
        }
        SkipReason::SameFolderAs { path } => trf(
            "Same folder as {path}, counted there",
            &[("path", &path.display().to_string())],
        ),
        SkipReason::NotDownloaded => tr("In iCloud, not downloaded"),
        SkipReason::TooDeep => tr("Nested too deeply"),
        SkipReason::Error {
            message, partial, ..
        } => {
            if *partial {
                trf("Stopped partway: {error}", &[("error", message.as_str())])
            } else {
                trf("Could not read: {error}", &[("error", message.as_str())])
            }
        }
    }
}

/// Why Space left a selected item where it was ([`TrashSkip::message`]).
pub fn trash_skip(s: &TrashSkip) -> String {
    match s {
        TrashSkip::OutsideScannedRoot => tr("Not inside the scanned folder"),
        TrashSkip::ScanRoot => tr("This is the scanned folder itself"),
        TrashSkip::ProtectedLocation => tr("macOS needs this location"),
        TrashSkip::InsideBundle => tr("Inside an app: Kipimo never modifies apps"),
        TrashSkip::AlreadyInTrash => tr("Already in the Trash"),
        TrashSkip::NotInScan => tr("Not part of this scan"),
        TrashSkip::NotFound => tr("No longer exists"),
        TrashSkip::InsideAnotherSelectedItem => tr("Moved with its enclosing folder"),
        TrashSkip::InvalidPath => tr("Not a valid full path"),
        TrashSkip::ScanRunning => tr("Wait for the scan to finish"),
        TrashSkip::AppData => tr("An app's own data: remove it with the app in Uninstall"),
        TrashSkip::Running => tr("Something inside it is running: quit it first"),
    }
}

/// Why Space couldn't scan a folder ([`SpaceError`]'s `Display`).
pub fn space_error(e: &SpaceError) -> String {
    match e {
        SpaceError::NotAFolder(path) => trf(
            "{path} is not a folder",
            &[("path", &path.display().to_string())],
        ),
        SpaceError::Io { .. } => e.to_string(),
        SpaceError::Busy => tr("a scan or a move to the Trash is running"),
    }
}

/// Where an app's updates come from ([`kipimo_updates::UpdateSource::label`]): product names
/// stay, the rest is translated.
pub fn source_label(label: &str) -> String {
    match label {
        "VS Code updates" => tr("VS Code updates"),
        "None" => tr("None"),
        _ => label.to_string(),
    }
}

/// The battery's health condition as IOKit reports it ("Normal" when it reports none).
pub fn battery_condition(c: &str) -> String {
    match c {
        "Normal" => tr("Normal"),
        "Service Recommended" => tr("Service Recommended"),
        "Check Battery" => tr("Check Battery"),
        "Permanent Battery Failure" => tr("Permanent Battery Failure"),
        _ => c.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn templates() {
        assert_eq!(
            fields(
                "couldn't unload com.x: refused",
                "couldn't unload {label}: {error}"
            ),
            Some(vec![("label", "com.x".into()), ("error", "refused".into())])
        );
        assert_eq!(
            fields("couldn't unload : x", "couldn't unload {label}: {error}"),
            None
        );
        assert_eq!(
            fields("Zoom is not a folder", "{path} is not a folder"),
            Some(vec![("path", "Zoom".into())])
        );
        // A repeated name must repeat the same text.
        let t = "{name} updates itself: choose {name} › About {name} to check.";
        assert!(
            fields(
                "Firefox updates itself: choose Firefox › About Firefox to check.",
                t
            )
            .is_some()
        );
        assert!(
            fields(
                "Firefox updates itself: choose Zoom › About Firefox to check.",
                t
            )
            .is_none()
        );
        // Nested engine sentences are translated (here: kept in English) too.
        assert_eq!(
            text("couldn't move the plist to the Trash: /x/a.plist no longer exists"),
            "couldn't move the plist to the Trash: /x/a.plist no longer exists"
        );
        assert_eq!(text("Something macOS said."), "Something macOS said.");
    }

    #[test]
    fn categories_match_the_engine() {
        for id in CategoryId::ALL {
            let info = id.info();
            assert_eq!(category_title(id), info.title);
            let first = match info.description.find(". ") {
                Some(i) => &info.description[..=i],
                None => info.description,
            };
            assert_eq!(category_summary(id), first, "{id:?}");
        }
    }

    #[test]
    fn startup_text_matches_the_engine() {
        for f in [
            Flag::Unsigned,
            Flag::AdHocSigned,
            Flag::InvalidSignature,
            Flag::UntrustedCertificate,
            Flag::ProgramMissing,
            Flag::OwnerAppMissing,
            Flag::RunsScript,
            Flag::ScriptMissing,
            Flag::MalformedPlist,
        ] {
            assert_eq!(flag(f), f.describe());
        }
        let sig = |status, problem| Signature {
            path: PathBuf::from("/Applications/A.app"),
            status,
            kind: kipimo_startup::SignerKind::Unsigned,
            signer: None,
            team_id: None,
            identifier: None,
            cdhash: None,
            notarized: None,
            problem,
        };
        let check = |signature| AppCheck {
            path: PathBuf::from("/Applications/A.app"),
            name: "A".into(),
            bundle_id: None,
            version: None,
            signature,
            elapsed_ms: 0,
        };
        let problem = |missing, altered, added| kipimo_startup::SignatureProblem {
            code: -67054,
            message: "a sealed resource is missing or invalid".into(),
            missing,
            altered,
            added,
            examples: Vec::new(),
        };
        for a in [
            check(sig(SignatureStatus::Unsigned, None)),
            check(sig(SignatureStatus::Invalid, Some(problem(0, 0, 0)))),
            check(sig(SignatureStatus::Invalid, Some(problem(12, 1, 0)))),
            check(sig(SignatureStatus::Invalid, Some(problem(1, 0, 3)))),
        ] {
            assert_eq!(app_check_reason(&a), a.reason());
            assert_eq!(
                signer(&a.signature),
                kipimo_startup::signer_summary(&a.signature)
            );
        }
        let failed = ReplaceError::Failed {
            error: "couldn't put the new copy in place: disk full".into(),
            old_in_trash: Some(PathBuf::from("/Users/x/.Trash/A.app")),
        };
        assert_eq!(replace_error(&failed), failed.message());
    }

    #[test]
    fn space_text_matches_the_engine() {
        use kipimo_space::SkipReason;
        for r in [
            SkipReason::NeedsFullDiskAccess,
            SkipReason::ProtectedByMacOS,
            SkipReason::PermissionDenied,
            SkipReason::OtherVolume {
                fs_type: "smbfs".into(),
                volume_used: None,
            },
            SkipReason::SameFolderAs {
                path: PathBuf::from("/Users"),
            },
            SkipReason::NotDownloaded,
            SkipReason::TooDeep,
            SkipReason::Error {
                code: 5,
                message: "Input/output error".into(),
                partial: true,
            },
            SkipReason::Error {
                code: 5,
                message: "Input/output error".into(),
                partial: false,
            },
        ] {
            assert_eq!(space_skip(&r), r.message());
        }
        for s in [
            TrashSkip::OutsideScannedRoot,
            TrashSkip::ScanRoot,
            TrashSkip::ProtectedLocation,
            TrashSkip::InsideBundle,
            TrashSkip::AlreadyInTrash,
            TrashSkip::NotInScan,
            TrashSkip::NotFound,
            TrashSkip::InsideAnotherSelectedItem,
            TrashSkip::InvalidPath,
            TrashSkip::ScanRunning,
            TrashSkip::AppData,
            TrashSkip::Running,
        ] {
            assert_eq!(trash_skip(&s), s.message());
        }
        for e in [
            SpaceError::NotAFolder(PathBuf::from("/x")),
            SpaceError::Io {
                path: PathBuf::from("/x"),
                message: "gone".into(),
            },
            SpaceError::Busy,
        ] {
            assert_eq!(space_error(&e), e.to_string());
        }
    }
}
