//! Code-signing checks through the Security framework (`SecStaticCode`), not the `codesign`
//! tool. Validation runs offline: since macOS 11.3 static validation only uses the network
//! when `kSecCSAllowNetworkAccess` is passed, and Kipimo never passes it.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFArray, CFDictionary, CFError, CFRetained, CFString, CFType, CFURL};
use objc2_security::{
    SecCSFlags, SecCertificate, SecCode, SecCopyErrorMessageString, SecRequirement, SecStaticCode,
    errSecCSUnsigned, kSecCFErrorResourceAdded, kSecCFErrorResourceAltered,
    kSecCFErrorResourceMissing, kSecCodeInfoCertificates, kSecCodeInfoFlags,
    kSecCodeInfoIdentifier, kSecCodeInfoTeamIdentifier, kSecCodeInfoUnique,
};

use crate::cf;
use crate::model::{Signature, SignatureProblem, SignatureStatus, SignerKind};

pub(crate) type OSStatus = i32;

// SecStaticCodeCheckValidity flags (SecStaticCode.h).
const CHECK_ALL_ARCHITECTURES: u32 = 1 << 0;
const DO_NOT_VALIDATE_EXECUTABLE: u32 = 1 << 1;
const DO_NOT_VALIDATE_RESOURCES: u32 = 1 << 2;
const BASIC_VALIDATE_ONLY: u32 = DO_NOT_VALIDATE_EXECUTABLE | DO_NOT_VALIDATE_RESOURCES;
const CHECK_NESTED_CODE: u32 = 1 << 3;
const STRICT_VALIDATE: u32 = 1 << 4;
// SecCodeCopySigningInformation flags (SecCode.h).
const SIGNING_INFORMATION: u32 = 1 << 1;
// kSecCodeSignatureAdhoc (CSCommon.h).
const SIGNATURE_ADHOC: i64 = 0x0002;

/// errSecCSReqFailed: the code is validly signed but doesn't satisfy the requirement.
const ERR_REQ_FAILED: OSStatus = -67050;

/// How thoroughly to validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depth {
    /// Hash every sealed resource file of a bundle (needed to notice deleted, added or
    /// rewritten files; whole `.lproj` folders are optional in Apple's seal rules).
    pub resources: bool,
    /// Also validate frameworks, helpers and plug-ins nested inside a bundle
    /// (`codesign --deep`).
    pub nested: bool,
    /// Strict structural checks (`codesign --strict`).
    pub strict: bool,
}

impl Depth {
    /// What the repair check uses: the same as `codesign --verify --deep --strict`, across all
    /// architectures.
    pub const FULL: Depth = Depth {
        resources: true,
        nested: true,
        strict: true,
    };
    /// The signature's own claims and the main executable only (no resources, nothing nested):
    /// enough to read who signed it, quickly.
    pub const QUICK: Depth = Depth {
        resources: false,
        nested: false,
        strict: false,
    };
    /// What the startup inventory uses: the program's own signature and sealed resources.
    pub const STANDARD: Depth = Depth {
        resources: true,
        nested: false,
        strict: true,
    };

    fn flags(self) -> u32 {
        let mut flags = CHECK_ALL_ARCHITECTURES;
        if !self.resources {
            flags |= DO_NOT_VALIDATE_RESOURCES;
        }
        if self.nested {
            flags |= CHECK_NESTED_CODE;
        }
        if self.strict {
            flags |= STRICT_VALIDATE;
        }
        flags
    }
}

/// Compiled code requirements used to classify signers. Build one per thread and reuse it.
pub struct Checker {
    apple: CFRetained<SecRequirement>,
    app_store: CFRetained<SecRequirement>,
    developer_id: CFRetained<SecRequirement>,
    apple_generic: CFRetained<SecRequirement>,
    notarized: Option<CFRetained<SecRequirement>>,
}

fn requirement(text: &str) -> Option<CFRetained<SecRequirement>> {
    let text = CFString::from_str(text);
    let mut out: *mut SecRequirement = ptr::null_mut();
    // SAFETY: `out` is a valid out-pointer; on success it holds a +1 reference.
    let status = unsafe {
        SecRequirement::create_with_string(&text, SecCSFlags(0), NonNull::from(&mut out))
    };
    if status != 0 {
        return None;
    }
    NonNull::new(out).map(|p| unsafe { CFRetained::from_raw(p) })
}

impl Checker {
    pub fn new() -> Checker {
        // These strings are Apple's documented requirement language; they always compile.
        Checker {
            apple: requirement("anchor apple").expect("anchor apple"),
            app_store: requirement(
                "anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.9] exists",
            )
            .expect("app store requirement"),
            developer_id: requirement(
                "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
                 and certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
            )
            .expect("developer id requirement"),
            apple_generic: requirement("anchor apple generic").expect("anchor apple generic"),
            // `notarized` exists since macOS 10.14; tolerate its absence anyway.
            notarized: requirement("notarized"),
        }
    }

    /// Checks the code at `path` (a binary, a bundle, or a bundle's main executable).
    ///
    /// One call can take seconds for a very large bundle (Xcode, Chrome) and can't be
    /// interrupted: Apple's only cancel hook (`SecStaticCodeCancelValidation`) is private and
    /// traps unless called from inside its own progress callback, so Kipimo doesn't use it.
    pub fn check(&self, path: &Path, depth: Depth) -> Signature {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let not_checked = |message: String| Signature {
            path: resolved.clone(),
            status: SignatureStatus::NotChecked,
            kind: SignerKind::Unknown,
            signer: None,
            team_id: None,
            identifier: None,
            cdhash: None,
            notarized: None,
            problem: Some(SignatureProblem {
                code: 0,
                message,
                missing: 0,
                altered: 0,
                added: 0,
                examples: Vec::new(),
            }),
        };
        if !resolved.exists() {
            return not_checked("the file does not exist".into());
        }
        let Some(url) = cf::file_url(&resolved) else {
            return not_checked("the path can't be expressed as a file URL".into());
        };
        let code = match static_code(&url) {
            Ok(code) => code,
            Err(status) => {
                let mut sig = not_checked(status_message(status));
                if let Some(p) = sig.problem.as_mut() {
                    p.code = status;
                }
                return sig;
            }
        };

        let info = signing_information(&code);
        let identifier = info
            .as_ref()
            .and_then(|d| get_string(d, unsafe { kSecCodeInfoIdentifier }));
        let team_id = info
            .as_ref()
            .and_then(|d| get_string(d, unsafe { kSecCodeInfoTeamIdentifier }));
        let code_flags = info
            .as_ref()
            .and_then(|d| cf::dict_get(d, unsafe { kSecCodeInfoFlags }))
            .and_then(|v| cf::as_i64(&v))
            .unwrap_or(0);
        let signer = info.as_ref().and_then(|d| leaf_certificate_name(d));
        let cdhash = info
            .as_ref()
            .and_then(|d| cf::dict_get(d, unsafe { kSecCodeInfoUnique }))
            .and_then(|v| cf::as_hex(&v));
        let checked_path = code_path(&code).unwrap_or(resolved);

        let (status, error) = validate(&code, depth.flags(), None);

        let unsigned = status == errSecCSUnsigned;
        let mut sig_status = match status {
            0 => SignatureStatus::Valid,
            s if s == errSecCSUnsigned => SignatureStatus::Unsigned,
            _ => SignatureStatus::Invalid,
        };
        let mut problem =
            (status != 0 && !unsigned).then(|| problem_from(status, error.as_deref()));
        // Some installers leave the seal readable only by root (Python.org's Python Launcher:
        // CodeResources is 0640 root:wheel). That isn't damage; say it couldn't be checked.
        if sig_status == SignatureStatus::Invalid
            && let Some(seal) = unreadable_seal(&checked_path)
            && let Some(p) = problem.as_mut()
        {
            sig_status = SignatureStatus::NotChecked;
            p.message = format!(
                "{} is readable only by its owner ({}), so the signature can't be checked from this account",
                seal.display(),
                file_owner(&seal)
            );
        }

        let kind = if unsigned {
            SignerKind::Unsigned
        } else if code_flags & SIGNATURE_ADHOC != 0 {
            SignerKind::AdHoc
        } else {
            self.classify(
                &code,
                signer.as_deref(),
                sig_status == SignatureStatus::Valid,
            )
        };
        let notarized = match (&self.notarized, kind) {
            (Some(req), SignerKind::DeveloperId) => {
                match validate(&code, BASIC_VALIDATE_ONLY, Some(req)).0 {
                    0 => Some(true),
                    ERR_REQ_FAILED => Some(false),
                    _ => None,
                }
            }
            _ => None,
        };

        Signature {
            path: checked_path,
            status: sig_status,
            kind,
            signer,
            team_id,
            identifier,
            cdhash: if unsigned { None } else { cdhash },
            notarized,
            problem,
        }
    }

    /// Who signed it, from requirement checks against the signature alone (the CMS blob and
    /// certificate chain), so it also works for code whose resources no longer validate.
    fn classify(&self, code: &SecStaticCode, signer: Option<&str>, valid: bool) -> SignerKind {
        let meets = |req: &SecRequirement| validate(code, BASIC_VALIDATE_ONLY, Some(req)).0 == 0;
        if meets(&self.apple) {
            SignerKind::Apple
        } else if meets(&self.app_store) {
            SignerKind::AppStore
        } else if meets(&self.developer_id) {
            SignerKind::DeveloperId
        } else if meets(&self.apple_generic) {
            SignerKind::AppleIssued
        } else if valid {
            // Intact signature that doesn't chain to Apple: self-signed or another CA, whatever
            // its certificate claims to be called.
            SignerKind::OtherCertificate
        } else {
            // The signature blob itself is damaged: fall back to the certificate name so the
            // owner still sees who it claims to be from (the item is flagged invalid anyway).
            classify_by_name(signer)
        }
    }
}

impl Default for Checker {
    fn default() -> Self {
        Checker::new()
    }
}

/// Best-effort classification from the leaf certificate's name, only used when a damaged
/// signature can't be evaluated against Apple's anchors.
pub(crate) fn classify_by_name(signer: Option<&str>) -> SignerKind {
    match signer {
        None => SignerKind::OtherCertificate,
        Some(name) => {
            if name.starts_with("Developer ID Application") {
                SignerKind::DeveloperId
            } else if name.starts_with("Apple Mac OS Application Signing") {
                SignerKind::AppStore
            } else if name == "Software Signing"
                || name == "macOS Software Signing"
                || name.starts_with("Apple Code Signing")
            {
                SignerKind::Apple
            } else if name.starts_with("Apple Development")
                || name.starts_with("Apple Distribution")
                || name.starts_with("Mac Developer")
                || name.starts_with("3rd Party Mac Developer")
            {
                SignerKind::AppleIssued
            } else {
                SignerKind::OtherCertificate
            }
        }
    }
}

fn static_code(url: &CFURL) -> Result<CFRetained<SecStaticCode>, OSStatus> {
    let mut out: *const SecStaticCode = ptr::null();
    // SAFETY: valid out-pointer; on success it holds a +1 reference.
    let status =
        unsafe { SecStaticCode::create_with_path(url, SecCSFlags(0), NonNull::from(&mut out)) };
    if status != 0 {
        return Err(status);
    }
    NonNull::new(out as *mut SecStaticCode)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(status)
}

fn signing_information(code: &SecStaticCode) -> Option<CFRetained<CFDictionary>> {
    let mut out: *const CFDictionary = ptr::null();
    // SAFETY: valid out-pointer; on success it holds a +1 reference. Works for unsigned code.
    let status = unsafe {
        SecCode::copy_signing_information(
            code,
            SecCSFlags(SIGNING_INFORMATION),
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        return None;
    }
    NonNull::new(out as *mut CFDictionary).map(|p| unsafe { CFRetained::from_raw(p) })
}

fn code_path(code: &SecStaticCode) -> Option<PathBuf> {
    let mut out: *const CFURL = ptr::null();
    // SAFETY: valid out-pointer; on success it holds a +1 reference.
    let status = unsafe { SecCode::copy_path(code, SecCSFlags(0), NonNull::from(&mut out)) };
    if status != 0 {
        return None;
    }
    let url: CFRetained<CFURL> =
        NonNull::new(out as *mut CFURL).map(|p| unsafe { CFRetained::from_raw(p) })?;
    url.to_file_path()
}

/// Runs `SecStaticCodeCheckValidityWithErrors`, returning the status and the error (if any).
fn validate(
    code: &SecStaticCode,
    flags: u32,
    req: Option<&SecRequirement>,
) -> (OSStatus, Option<CFRetained<CFError>>) {
    let mut error: *mut CFError = ptr::null_mut();
    // SAFETY: `error` is a valid out-pointer; on failure it may hold a +1 CFError.
    let status = unsafe { code.check_validity_with_errors(SecCSFlags(flags), req, &mut error) };
    let error = NonNull::new(error).map(|p| unsafe { CFRetained::from_raw(p) });
    (status, error)
}

/// The bundle's `_CodeSignature/CodeResources` when it exists but this user can't read it.
fn unreadable_seal(bundle: &Path) -> Option<PathBuf> {
    let seal = bundle.join("Contents/_CodeSignature/CodeResources");
    match std::fs::File::open(&seal) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(seal),
        _ => None,
    }
}

fn file_owner(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return "another user".into();
    };
    // SAFETY: getpwuid returns static storage or NULL; copied out immediately.
    unsafe {
        let pw = libc::getpwuid(meta.uid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return format!("uid {}", meta.uid());
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

fn get_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    cf::dict_get(dict, key).and_then(|v| cf::as_string(&v))
}

fn leaf_certificate_name(info: &CFDictionary) -> Option<String> {
    let certs: CFRetained<CFArray> = cf::dict_get_array(info, unsafe { kSecCodeInfoCertificates })?;
    let first: CFRetained<CFType> = cf::array_items(&certs).into_iter().next()?;
    let cert: CFRetained<SecCertificate> = first.downcast::<SecCertificate>().ok()?;
    // SAFETY: `cert` is a valid SecCertificate.
    unsafe { cert.subject_summary() }.map(|s| s.to_string())
}

pub(crate) fn status_message(status: OSStatus) -> String {
    // SAFETY: the reserved argument must be NULL.
    unsafe { SecCopyErrorMessageString(status, ptr::null_mut::<c_void>()) }
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("OSStatus {status}"))
}

/// Builds the problem description from the validation error, including which sealed files
/// were reported missing, modified or added.
fn problem_from(status: OSStatus, error: Option<&CFError>) -> SignatureProblem {
    // The Security framework's text for the status ("a sealed resource is missing or
    // invalid") is clearer than the CFError's generic "The operation couldn't be completed".
    let message = match status_message(status) {
        m if !m.starts_with("OSStatus") => m,
        fallback => error.map(cf::error_message).unwrap_or(fallback),
    };
    let mut problem = SignatureProblem {
        code: status,
        message,
        missing: 0,
        altered: 0,
        added: 0,
        examples: Vec::new(),
    };
    let Some(info) = error.and_then(|e| e.user_info()) else {
        return problem;
    };
    const EXAMPLES: usize = 5;
    let mut collect = |key: &CFString| -> usize {
        let Some(list) = cf::dict_get_array(&info, key) else {
            return 0;
        };
        let items = cf::array_items(&list);
        for item in &items {
            if problem.examples.len() >= EXAMPLES {
                break;
            }
            if let Some(p) = cf::as_path(item) {
                problem.examples.push(p);
            }
        }
        items.len()
    };
    problem.missing = collect(unsafe { kSecCFErrorResourceMissing });
    problem.altered = collect(unsafe { kSecCFErrorResourceAltered });
    problem.added = collect(unsafe { kSecCFErrorResourceAdded });
    problem
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, build_binary, codesign, compiler};

    #[test]
    fn apple_binary() {
        let sig = Checker::new().check(Path::new("/bin/ls"), Depth::FULL);
        assert_eq!(sig.status, SignatureStatus::Valid, "{:?}", sig.problem);
        assert_eq!(sig.kind, SignerKind::Apple);
        assert!(
            sig.signer
                .as_deref()
                .unwrap_or("")
                .contains("Software Signing"),
            "{:?}",
            sig.signer
        );
        assert_eq!(sig.identifier.as_deref(), Some("com.apple.ls"));
        assert_eq!(sig.notarized, None);
        assert!(sig.problem.is_none());
    }

    #[test]
    fn symlink_is_followed() {
        let tmp = TempDir::new("sig-symlink");
        let link = tmp.path().join("ls-link");
        std::os::unix::fs::symlink("/bin/ls", &link).unwrap();
        let sig = Checker::new().check(&link, Depth::STANDARD);
        assert_eq!(sig.kind, SignerKind::Apple);
        assert_eq!(sig.path, PathBuf::from("/bin/ls"));
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nothing"), &dangling).unwrap();
        let sig = Checker::new().check(&dangling, Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::NotChecked);
    }

    #[test]
    fn missing_and_non_code() {
        let tmp = TempDir::new("sig-missing");
        let checker = Checker::new();
        let sig = checker.check(&tmp.path().join("nope"), Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::NotChecked);
        assert_eq!(sig.kind, SignerKind::Unknown);
        let text = tmp.path().join("script.sh");
        std::fs::write(&text, "#!/bin/sh\necho hi\n").unwrap();
        let sig = checker.check(&text, Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::Unsigned);
        assert_eq!(sig.kind, SignerKind::Unsigned);
    }

    #[test]
    fn adhoc_unsigned_and_tampered_binaries() {
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let tmp = TempDir::new("sig-adhoc");
        let checker = Checker::new();

        // The linker ad-hoc signs arm64 binaries; sign again explicitly to be sure on x86_64.
        let adhoc = tmp.path().join("adhoc");
        build_binary(&cc, &adhoc);
        codesign(&["--force", "--sign", "-"], &adhoc);
        let sig = checker.check(&adhoc, Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::Valid, "{:?}", sig.problem);
        assert_eq!(sig.kind, SignerKind::AdHoc);
        assert_eq!(sig.signer, None);
        assert_eq!(sig.team_id, None);
        let cdhash = sig
            .cdhash
            .clone()
            .expect("an ad-hoc signature has a cdhash");
        assert_eq!(cdhash.len(), 40, "{cdhash}");

        let unsigned = tmp.path().join("unsigned");
        build_binary(&cc, &unsigned);
        codesign(&["--remove-signature"], &unsigned);
        let sig = checker.check(&unsigned, Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::Unsigned);
        assert_eq!(sig.kind, SignerKind::Unsigned);
        assert_eq!(sig.cdhash, None);

        // Flip a byte inside a hashed code page.
        let tampered = tmp.path().join("tampered");
        std::fs::copy(&adhoc, &tampered).unwrap();
        let mut bytes = std::fs::read(&tampered).unwrap();
        bytes[0x2000] ^= 0xff;
        std::fs::write(&tampered, &bytes).unwrap();
        let sig = checker.check(&tampered, Depth::STANDARD);
        assert_eq!(sig.status, SignatureStatus::Invalid);
        assert_eq!(sig.kind, SignerKind::AdHoc);
        // The signature still says what the code should be: the same build.
        assert_eq!(sig.cdhash.as_deref(), Some(cdhash.as_str()));
        let problem = sig.problem.unwrap();
        assert_ne!(problem.code, 0);
        assert!(!problem.message.is_empty());
    }

    #[test]
    fn unreadable_seal_is_not_damage() {
        use std::os::unix::fs::PermissionsExt;
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let tmp = TempDir::new("sig-seal-perms");
        let app = crate::testutil::adhoc_app(&cc, tmp.path(), "Locked", true);
        let seal = app.join("Contents/_CodeSignature/CodeResources");
        std::fs::set_permissions(&seal, std::fs::Permissions::from_mode(0o000)).unwrap();
        let sig = Checker::new().check(&app, Depth::FULL);
        std::fs::set_permissions(&seal, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(sig.status, SignatureStatus::NotChecked, "{:?}", sig.problem);
        assert!(
            sig.problem
                .unwrap()
                .message
                .contains("CodeResources is readable only by")
        );
        // Readable again: valid.
        assert!(Checker::new().check(&app, Depth::FULL).is_valid());
    }

    #[test]
    fn names_fallback() {
        assert_eq!(
            classify_by_name(Some("Developer ID Application: X (ABC)")),
            SignerKind::DeveloperId
        );
        assert_eq!(
            classify_by_name(Some("Apple Mac OS Application Signing")),
            SignerKind::AppStore
        );
        assert_eq!(
            classify_by_name(Some("macOS Software Signing")),
            SignerKind::Apple
        );
        assert_eq!(
            classify_by_name(Some("Apple Development: me")),
            SignerKind::AppleIssued
        );
        assert_eq!(
            classify_by_name(Some("My Own CA")),
            SignerKind::OtherCertificate
        );
        assert_eq!(classify_by_name(None), SignerKind::OtherCertificate);
    }
}
