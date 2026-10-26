//! Thin wrappers over the macOS APIs this crate needs: Spotlight
//! metadata (last used date), libproc (running apps), Foundation (Trash) and libc.

use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDate, NSFileManager, NSString, NSURL};

use crate::plist::{self, Plist};

type CFTypeRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn MDItemCreate(allocator: CFTypeRef, path: CFTypeRef) -> CFTypeRef;
    fn MDItemCopyAttribute(item: CFTypeRef, name: CFTypeRef) -> CFTypeRef;
    static kMDItemLastUsedDate: CFTypeRef;
}

/// What a bundle's code signature declares (read from the executable, not validated).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SigningInfo {
    /// The developer team ID, if signed with a Developer ID or App Store certificate.
    pub team_id: Option<String>,
    /// True for platform binaries: code that ships as part of macOS.
    pub platform: bool,
    /// App Group identifiers from the `com.apple.security.application-groups` entitlement.
    pub app_groups: Vec<String>,
}

/// Spotlight's `kMDItemLastUsedDate` for a path, as Unix seconds.
pub(crate) fn last_used(path: &Path) -> Option<i64> {
    autoreleasepool(|_| {
        let s = NSString::from_str(path.to_str()?);
        // SAFETY: NSString is toll-free bridged with CFString; the item is released below.
        let item = unsafe { MDItemCreate(std::ptr::null(), Retained::as_ptr(&s).cast()) };
        if item.is_null() {
            return None;
        }
        // SAFETY: valid MDItem and attribute name; the value is +1 (or null).
        let value = unsafe { MDItemCopyAttribute(item, kMDItemLastUsedDate) };
        unsafe { CFRelease(item) };
        if value.is_null() {
            return None;
        }
        // SAFETY: CF objects are Objective-C objects; we take over the +1.
        let obj: Retained<AnyObject> = unsafe { Retained::from_raw(value as *mut AnyObject) }?;
        obj.downcast_ref::<NSDate>()
            .map(|d| d.timeIntervalSince1970() as i64)
    })
}

/// Why a move to the Trash failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashError {
    /// The item or its folder isn't writable by this user (an administrator must remove it).
    PermissionDenied(String),
    /// The item is gone.
    NotFound,
    Other(String),
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::PermissionDenied(m) => write!(f, "permission denied: {m}"),
            TrashError::NotFound => write!(f, "not found"),
            TrashError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Moves an item to the Trash with `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`.
///
/// This is the call Apple's own `/usr/bin/trash` (CarbonCore) uses, and on current macOS it
/// writes Finder's "Put Back" records (`ptbL`/`ptbN` in the Trash folder's `.DS_Store`), so
/// "Put Back" keeps working. Checked on macOS 26.6 against a scratch disk image.
pub(crate) fn trash_item(path: &Path) -> Result<Option<PathBuf>, TrashError> {
    autoreleasepool(|_| {
        let url =
            NSURL::from_file_path(path).ok_or_else(|| TrashError::Other("bad path".into()))?;
        let fm = NSFileManager::defaultManager();
        let mut out: Option<Retained<NSURL>> = None;
        match fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut out)) {
            Ok(()) => Ok(out.and_then(|u| u.to_file_path())),
            Err(err) => {
                let code = err.code();
                let message = err.localizedDescription().to_string();
                // NSFileNoSuchFileError = 4, NSFileWriteNoPermissionError = 513,
                // NSFileReadNoPermissionError = 257, POSIX EACCES/EPERM.
                match code {
                    4 => Err(TrashError::NotFound),
                    513 | 257 => Err(TrashError::PermissionDenied(message)),
                    c if c == libc::EACCES as isize || c == libc::EPERM as isize => {
                        Err(TrashError::PermissionDenied(message))
                    }
                    _ => Err(TrashError::Other(format!("{message} (code {code})"))),
                }
            }
        }
    })
}

/// A process running from an app bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunningApp {
    /// The outermost `.app` bundle the process runs from.
    pub bundle_path: PathBuf,
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub executable: Option<String>,
}

/// Lists the app bundles that have a running process, through `proc_listallpids` and
/// `proc_pidpath` (libproc). Helper apps nested in a bundle count as their outer app.
pub(crate) fn running_apps() -> Vec<RunningApp> {
    // SAFETY: a null buffer asks for the number of pids.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::c_int; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    // SAFETY: the buffer holds `bytes` bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        return Vec::new();
    }
    pids.truncate(n as usize);
    let mut bundles: Vec<PathBuf> = Vec::new();
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        // SAFETY: buffer of PROC_PIDPATHINFO_MAXSIZE bytes.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            continue;
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(&buf[..len as usize]));
        if let Some(bundle) = outermost_app(path)
            && !bundles.contains(&bundle)
        {
            bundles.push(bundle);
        }
    }
    bundles
        .into_iter()
        .map(|bundle_path| {
            let info = crate::apps::BundleInfo::read(&bundle_path);
            RunningApp {
                bundle_id: info.as_ref().and_then(|i| i.bundle_id.clone()),
                name: info.as_ref().map(|i| i.name.clone()),
                executable: info.as_ref().and_then(|i| i.executable.clone()),
                bundle_path,
            }
        })
        .collect()
}

/// `/Applications/Foo.app/Contents/Frameworks/Bar.app/Contents/MacOS/Bar` → `/Applications/Foo.app`.
pub(crate) fn outermost_app(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        out.push(comp);
        if comp.as_os_str().as_bytes().ends_with(b".app") {
            return Some(out);
        }
    }
    None
}

/// The current user's home folder from the password database (not `$HOME`).
pub(crate) fn user_home() -> Option<PathBuf> {
    // SAFETY: getpwuid_r with a caller-owned buffer.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; 4096];
        let rc = libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        let dir = CStr::from_ptr(pwd.pw_dir);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
    }
}

/// `kern.osversion`, the macOS build (for example `25G83`).
pub(crate) fn os_build() -> Option<String> {
    let name = CString::new("kern.osversion").ok()?;
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: sysctlbyname into a fixed buffer.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf[..len]).ok()?;
    Some(s.to_string_lossy().into_owned())
}

/// True if the path lives on a read-only volume (the sealed system volume, a mounted image).
pub(crate) fn on_read_only_volume(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: statfs into a zeroed buffer.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_flags & libc::MNT_RDONLY as u32 != 0
}

/// `access(path, W_OK)`: can this user modify the item (or add/remove entries in the folder)?
pub(crate) fn writable(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: plain access(2).
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Reads `Contents/Info.plist`-style metadata helpers used by several modules.
pub(crate) fn read_info_plist(bundle: &Path) -> Option<Plist> {
    plist::read(&bundle.join("Contents/Info.plist"))
        .or_else(|| plist::read(&bundle.join("Info.plist")))
        .or_else(|| plist::read(&bundle.join("WrappedBundle/Info.plist")))
}
