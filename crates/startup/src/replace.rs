//! Download and Replace: swap an app whose signature broke for a fresh copy from its
//! developer, after checking that the copy is intact and comes from the same developer.
//!
//! Why not re-sign the broken app instead? Re-signing replaces the developer's signature
//! with an ad-hoc one made on this Mac. That drops the developer's identity, which breaks
//! entitlements that need it (iCloud, push, keychain access groups), keychain items the app
//! stored under its old identity, notarization trust and the app's own updater (Sparkle and
//! Squirrel check that an update is signed like the running copy). It would also mean
//! changing the contents of an app bundle, which Kipimo never does. A fresh copy fixes the
//! cause; re-signing would only hide it.
//!
//! The steps, each of which leaves everything as it was if it fails:
//!
//! 1. [`replaceable`]: the installed copy claims a Team ID (so there is something to match)
//!    and its folder is writable without an administrator.
//! 2. The caller downloads the archive into a [`WorkDir`] on the same volume as the app.
//! 3. [`replace_from_zip`] checks every name in the archive (nothing may land outside the
//!    work folder), unpacks it with `ditto`, finds the app with the same bundle identifier,
//!    and validates its signature like `codesign --verify --deep --strict`. The new copy
//!    must carry a valid Developer ID signature from the team the old copy claims.
//! 4. Only then does the old copy go to the Trash (Put Back restores it) and the new one
//!    take its place. If that last rename fails, the old copy is moved back.
//!
//! The new copy isn't marked as downloaded from the internet (no quarantine attribute), like
//! an update installed by the app's own updater: Sparkle and Squirrel also install without
//! quarantine after checking the team, as done here. With quarantine, macOS would run the app
//! from a translocated, read-only copy until it was moved again in Finder, which stops those
//! updaters from working.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask, NSURL};

use crate::codesign::{Checker, Depth};
use crate::model::{Signature, SignatureStatus, SignerKind};
use crate::owner;
use crate::trash::{SystemTrash, TrashFailure, Trasher};

/// What Kipimo knows about the installed copy before replacing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCopy {
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    /// The Team ID its signature claims (read without validation, since the signature is
    /// broken). A replacement must be signed by this team.
    pub team_id: String,
}

/// Why a replacement didn't happen (or didn't finish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceError {
    /// Kipimo can't replace this copy (no Team ID to match, needs an administrator, ...).
    NotReplaceable(String),
    /// The download isn't a usable archive of this app.
    BadDownload(String),
    /// The new copy failed a signature check. Nothing was changed.
    Untrusted(String),
    /// macOS refused to move the old copy to the Trash, usually App Management protection.
    /// Nothing was changed.
    Blocked(String),
    /// Moving the files failed. `old_in_trash` is set when the old copy could not be put back
    /// and is still in the Trash.
    Failed {
        error: String,
        old_in_trash: Option<PathBuf>,
    },
}

impl ReplaceError {
    /// A sentence for the UI.
    pub fn message(&self) -> String {
        match self {
            ReplaceError::NotReplaceable(m)
            | ReplaceError::BadDownload(m)
            | ReplaceError::Untrusted(m)
            | ReplaceError::Blocked(m) => m.clone(),
            ReplaceError::Failed {
                error,
                old_in_trash: Some(p),
            } => format!(
                "{error}. The old copy is in the Trash at {}; Put Back in Finder restores it.",
                p.display()
            ),
            ReplaceError::Failed { error, .. } => error.clone(),
        }
    }
}

/// The result of a replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// Where the new copy now is (the old copy's path).
    pub app: PathBuf,
    /// Where the old copy went in the Trash, when macOS reported it.
    pub old_in_trash: Option<PathBuf>,
    /// The new copy's version, "3.7.6 (4120)".
    pub version: Option<String>,
}

/// Whether Kipimo could replace `app` with a fresh copy: its signature claims a Team ID and
/// both the app and its folder are writable by this user. Reads only the signature's own
/// claims (no validation of the bundle's files), so it is quick.
pub fn replaceable(app: &Path) -> Result<InstalledCopy, ReplaceError> {
    let signature = Checker::new().check(app, Depth::QUICK);
    installed_copy(app, &signature)
}

fn installed_copy(app: &Path, signature: &Signature) -> Result<InstalledCopy, ReplaceError> {
    let is_app = app.extension().is_some_and(|e| e == "app");
    // A symlink would be trashed as a link while the app it points at stays: refuse it.
    let real_dir = std::fs::symlink_metadata(app).is_ok_and(|m| m.is_dir());
    if !is_app || !real_dir {
        return Err(ReplaceError::NotReplaceable(
            "it isn't an app bundle any more".into(),
        ));
    }
    let Some(team_id) = signature.team_id.clone().filter(|t| !t.is_empty()) else {
        return Err(ReplaceError::NotReplaceable(
            "its signature names no developer team, so a download can't be matched to it".into(),
        ));
    };
    let parent = app.parent().unwrap_or(Path::new("/"));
    // Moving a folder needs write access to its parent and to the folder itself (".." changes).
    if !writable(parent) || !writable(app) {
        return Err(ReplaceError::NotReplaceable(
            "it was installed by an administrator, so replacing it needs one".into(),
        ));
    }
    let (bundle_id, _) = owner::bundle_info(app);
    Ok(InstalledCopy {
        path: app.to_path_buf(),
        bundle_id,
        team_id,
    })
}

fn writable(path: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Checks a downloaded copy against the installed one: an intact signature (deep, strict,
/// every architecture), a Developer ID certificate, the same Team ID and the same bundle
/// identifier.
pub fn check_replacement(
    old: &InstalledCopy,
    new: &Signature,
    new_bundle_id: Option<&str>,
) -> Result<(), String> {
    if new.status != SignatureStatus::Valid {
        let why = new
            .problem
            .as_ref()
            .map(|p| format!(" ({})", p.message))
            .unwrap_or_default();
        return Err(format!(
            "the downloaded copy's signature doesn't check out{why}"
        ));
    }
    if new.kind != SignerKind::DeveloperId {
        return Err("the downloaded copy isn't signed with a Developer ID".into());
    }
    match new.team_id.as_deref() {
        Some(t) if t == old.team_id => {}
        Some(t) => {
            return Err(format!(
                "the downloaded copy is signed by team {t}, not {} like the installed one",
                old.team_id
            ));
        }
        None => return Err("the downloaded copy's signature names no team".into()),
    }
    if let Some(id) = old.bundle_id.as_deref()
        && new_bundle_id != Some(id)
    {
        return Err(format!(
            "the download holds a different app ({})",
            new_bundle_id.unwrap_or("no bundle identifier")
        ));
    }
    Ok(())
}

/// A scratch folder for one download, on the same volume as the app it replaces (so the new
/// copy can be renamed into place). Removed with everything in it when dropped: it only ever
/// holds the archive and what was unpacked from it, never anything of the user's.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    /// A fresh folder from `NSItemReplacementDirectory`, which Foundation places on the
    /// volume of `app` for exactly this kind of swap.
    pub fn for_app(app: &Path) -> Result<WorkDir, String> {
        let url = NSURL::from_file_path(app).ok_or("the app's path isn't a file URL")?;
        let fm = NSFileManager::defaultManager();
        let dir = fm
            .URLForDirectory_inDomain_appropriateForURL_create_error(
                NSSearchPathDirectory::ItemReplacementDirectory,
                NSSearchPathDomainMask::UserDomainMask,
                Some(&url),
                true,
            )
            .map_err(|e| e.localizedDescription().to_string())?;
        let path = dir
            .to_file_path()
            .ok_or("the download folder has no path")?;
        Ok(WorkDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Unpacks `zip` (inside `work`), checks the app in it and swaps it for `old`. See the module
/// documentation for the steps. Call [`replaceable`] first; this checks again anyway.
pub fn replace_from_zip(old: &Path, zip: &Path, work: &WorkDir) -> Result<Replaced, ReplaceError> {
    let checker = Checker::new();
    let check = |p: &Path, d: Depth| checker.check(p, d);
    replace_with(old, zip, work.path(), &check, &SystemTrash, &|a, b| {
        std::fs::rename(a, b)
    })
}

type Check<'a> = dyn Fn(&Path, Depth) -> Signature + 'a;
type Rename<'a> = dyn Fn(&Path, &Path) -> std::io::Result<()> + 'a;

pub(crate) fn replace_with(
    old: &Path,
    zip: &Path,
    work: &Path,
    check: &Check<'_>,
    trash: &dyn Trasher,
    rename: &Rename<'_>,
) -> Result<Replaced, ReplaceError> {
    let installed = installed_copy(old, &check(old, Depth::QUICK))?;
    zip_names_are_safe(zip).map_err(ReplaceError::BadDownload)?;
    let unpacked = work.join("unpacked");
    unpack(zip, &unpacked).map_err(ReplaceError::BadDownload)?;
    let new = find_app(&unpacked, installed.bundle_id.as_deref())?;
    let signature = check(&new, Depth::FULL);
    let (new_id, _) = owner::bundle_info(&new);
    check_replacement(&installed, &signature, new_id.as_deref())
        .map_err(ReplaceError::Untrusted)?;
    let parent = old.parent().unwrap_or(Path::new("/"));
    let same_volume = match (std::fs::metadata(&new), std::fs::metadata(parent)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    };
    if !same_volume {
        return Err(ReplaceError::Failed {
            error: "the download landed on a different disk from the app".into(),
            old_in_trash: None,
        });
    }
    let version = owner::bundle_version(&new);
    let old_in_trash = swap(old, &new, trash, rename)?;
    Ok(Replaced {
        app: old.to_path_buf(),
        old_in_trash,
        version,
    })
}

/// Old copy to the Trash, new copy into its place; puts the old copy back if the second step
/// fails.
fn swap(
    old: &Path,
    new: &Path,
    trash: &dyn Trasher,
    rename: &Rename<'_>,
) -> Result<Option<PathBuf>, ReplaceError> {
    let trashed = match trash.trash(old) {
        Ok(p) => p,
        Err(TrashFailure::Denied(m)) => {
            return Err(ReplaceError::Blocked(format!(
                "macOS didn't let Kipimo move the old copy to the Trash ({m})"
            )));
        }
        Err(TrashFailure::Other(m)) => {
            return Err(ReplaceError::Failed {
                error: format!("couldn't move the old copy to the Trash: {m}"),
                old_in_trash: None,
            });
        }
    };
    if let Err(e) = rename(new, old) {
        let error = format!("couldn't put the new copy in place: {e}");
        let restored = trashed.as_deref().is_some_and(|t| rename(t, old).is_ok());
        return Err(ReplaceError::Failed {
            error,
            old_in_trash: if restored { None } else { trashed },
        });
    }
    Ok(trashed)
}

/// Unpacks with `/usr/bin/ditto -x -k`. ditto is macOS's own archiver (Finder's Compress and
/// Archive Utility use the same code) and restores the symlinks, extended attributes and
/// resource forks an app's signature covers; a generic unzip loses some of them and would
/// break the very signature checked next. No framework API unpacks a zip archive, so this
/// built-in command is the last resort. It runs without a shell, on paths Kipimo made.
fn unpack(zip: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let out = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip)
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't unpack the download: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "the download couldn't be unpacked{}",
            if err.is_empty() {
                String::new()
            } else {
                format!(" ({err})")
            }
        ));
    }
    Ok(())
}

/// The app in the unpacked archive with `bundle_id` (or the only app, when the old copy has
/// no bundle identifier). Looks at the top level and one folder down.
fn find_app(dir: &Path, bundle_id: Option<&str>) -> Result<PathBuf, ReplaceError> {
    let is_app = |p: &Path| {
        p.extension().is_some_and(|e| e == "app")
            && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
    };
    let children = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .map(|r| r.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    };
    let mut apps: Vec<PathBuf> = children(dir).into_iter().filter(|p| is_app(p)).collect();
    if apps.is_empty() {
        for sub in children(dir) {
            let skip = sub.file_name().is_some_and(|n| n == "__MACOSX");
            if !skip && std::fs::symlink_metadata(&sub).is_ok_and(|m| m.is_dir()) {
                apps.extend(children(&sub).into_iter().filter(|p| is_app(p)));
            }
        }
    }
    let pick = match bundle_id {
        Some(id) => apps
            .into_iter()
            .find(|a| owner::bundle_info(a).0.as_deref() == Some(id)),
        None if apps.len() == 1 => apps.pop(),
        None => None,
    };
    pick.ok_or_else(|| ReplaceError::BadDownload("the download doesn't hold this app".into()))
}

/// Reads the archive's central directory and refuses names that could land outside the
/// folder it is unpacked into: absolute paths, `..`, and entries written through a symlink
/// stored earlier in the same archive. Handles Zip64 archives.
pub(crate) fn zip_names_are_safe(zip: &Path) -> Result<(), String> {
    let names = zip_entries(zip)?;
    let links: std::collections::HashSet<&str> = names
        .iter()
        .filter(|e| e.symlink)
        .map(|e| e.name.trim_end_matches('/'))
        .collect();
    for e in &names {
        let name = e.name.as_str();
        if name.starts_with('/') || name.split('/').any(|c| c == "..") || name.contains('\0') {
            return Err(format!(
                "the download has a file that would land outside its folder ({name})"
            ));
        }
        let trimmed = name.trim_end_matches('/');
        let mut prefix = String::new();
        let parts: Vec<&str> = trimmed.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if links.contains(prefix.as_str()) {
                return Err(format!(
                    "the download writes through a link inside itself ({name})"
                ));
            }
        }
    }
    if names.is_empty() {
        return Err("the download is an empty archive".into());
    }
    Ok(())
}

struct ZipEntry {
    name: String,
    symlink: bool,
}

fn zip_entries(zip: &Path) -> Result<Vec<ZipEntry>, String> {
    use std::io::{Read, Seek, SeekFrom};
    const NOT_ZIP: &str = "the download isn't a zip archive";
    let mut f = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len < 22 {
        return Err(NOT_ZIP.into());
    }
    let mut read_at = |offset: u64, n: u64| -> Result<Vec<u8>, String> {
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        (&mut f)
            .take(n)
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        if buf.len() as u64 == n {
            Ok(buf)
        } else {
            Err(NOT_ZIP.to_string())
        }
    };
    let le16 = |b: &[u8], at: usize| b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let le32 = |b: &[u8], at: usize| {
        b.get(at..at + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let le64 = |b: &[u8], at: usize| {
        b.get(at..at + 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    };
    // End of central directory: in the last 22 bytes plus at most a 64 KB comment.
    let tail_len = len.min(22 + 65_535);
    let tail = read_at(len - tail_len, tail_len)?;
    let eocd = (0..=tail.len() - 22)
        .rev()
        .find(|&i| le32(&tail, i) == Some(0x0605_4b50))
        .ok_or(NOT_ZIP)?;
    let mut entries = le16(&tail, eocd + 10).ok_or(NOT_ZIP)? as u64;
    let mut cd_size = le32(&tail, eocd + 12).ok_or(NOT_ZIP)? as u64;
    let mut cd_offset = le32(&tail, eocd + 16).ok_or(NOT_ZIP)? as u64;
    if entries == 0xffff || cd_size == 0xffff_ffff || cd_offset == 0xffff_ffff {
        // Zip64: a locator just before the end record points at the Zip64 end record.
        let loc_at = (len - tail_len + eocd as u64)
            .checked_sub(20)
            .ok_or(NOT_ZIP)?;
        let loc = read_at(loc_at, 20)?;
        if le32(&loc, 0) != Some(0x0706_4b50) {
            return Err(NOT_ZIP.into());
        }
        let rec = read_at(le64(&loc, 8).ok_or(NOT_ZIP)?, 56)?;
        if le32(&rec, 0) != Some(0x0606_4b50) {
            return Err(NOT_ZIP.into());
        }
        entries = le64(&rec, 32).ok_or(NOT_ZIP)?;
        cd_size = le64(&rec, 40).ok_or(NOT_ZIP)?;
        cd_offset = le64(&rec, 48).ok_or(NOT_ZIP)?;
    }
    const MAX_DIRECTORY: u64 = 64 << 20;
    if cd_size > MAX_DIRECTORY || cd_offset.saturating_add(cd_size) > len {
        return Err(NOT_ZIP.into());
    }
    let cd = read_at(cd_offset, cd_size)?;
    let mut out = Vec::new();
    let mut at = 0usize;
    for _ in 0..entries {
        if le32(&cd, at) != Some(0x0201_4b50) {
            return Err(NOT_ZIP.into());
        }
        let made_by = le16(&cd, at + 4).ok_or(NOT_ZIP)?;
        let name_len = le16(&cd, at + 28).ok_or(NOT_ZIP)? as usize;
        let extra_len = le16(&cd, at + 30).ok_or(NOT_ZIP)? as usize;
        let comment_len = le16(&cd, at + 32).ok_or(NOT_ZIP)? as usize;
        let external = le32(&cd, at + 38).ok_or(NOT_ZIP)?;
        let name = cd.get(at + 46..at + 46 + name_len).ok_or(NOT_ZIP)?;
        // Unix hosts keep the file mode in the high half of the external attributes.
        let mode = external >> 16;
        let symlink = made_by >> 8 == 3 && mode & 0o170_000 == 0o120_000;
        out.push(ZipEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            symlink,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, adhoc_app, compiler};
    use crate::trash::fake::FolderTrash;

    /// A stored (uncompressed) zip with the given entries; `link` entries are symlinks.
    fn write_zip(path: &Path, entries: &[(&str, &[u8], bool)]) {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, link) in entries {
            let offset = out.len() as u32;
            let crc = crc32(data);
            let mut local = Vec::new();
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&local);
            out.extend_from_slice(data);
            let mode: u32 = if *link { 0o120_777 } else { 0o100_644 };
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 3, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&(mode << 16).to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        std::fs::write(path, out).unwrap();
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// `ditto -c -k --keepParent`: how developers (and Xcode) make an app's zip.
    fn zip_app(app: &Path, zip: &Path) {
        let ok = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(app)
            .arg(zip)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ditto -c failed");
    }

    /// The real signature check, with a Developer ID team stamped on (the test bundles are
    /// ad-hoc signed, and no test may use a real signing identity). Validity stays real.
    fn as_team<'a>(
        checker: &'a Checker,
        team: &'a dyn Fn(&Path) -> &'static str,
    ) -> impl Fn(&Path, Depth) -> Signature + 'a {
        move |p, d| {
            let mut s = checker.check(p, d);
            if s.kind == SignerKind::AdHoc {
                s.kind = SignerKind::DeveloperId;
                s.team_id = Some(team(p).to_string());
            }
            s
        }
    }

    fn mv(a: &Path, b: &Path) -> std::io::Result<()> {
        std::fs::rename(a, b)
    }

    struct Fixture {
        _tmp: TempDir,
        old: PathBuf,
        zip: PathBuf,
        work: PathBuf,
        trash: FolderTrash,
    }

    /// An installed "Thing.app" whose signature broke, and a zip of a fresh, intact copy.
    fn fixture(cc: &Path, tag: &str, new_is_broken: bool) -> Fixture {
        let tmp = TempDir::new(tag);
        let apps = tmp.path().join("Applications");
        let build = tmp.path().join("build");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        let old = adhoc_app(cc, &apps, "Thing", true);
        std::fs::remove_file(old.join("Contents/Resources/data.json")).unwrap();
        let new = adhoc_app(cc, &build, "Thing", true);
        std::fs::write(new.join("Contents/Resources/fresh.txt"), b"new").unwrap();
        crate::testutil::codesign(&["--force", "--sign", "-"], &new);
        if new_is_broken {
            std::fs::remove_file(new.join("Contents/Resources/data.json")).unwrap();
        }
        let zip = tmp.path().join("Thing.zip");
        zip_app(&new, &zip);
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let trash = FolderTrash::new(tmp.path().join("Trash"));
        Fixture {
            old,
            zip,
            work,
            trash,
            _tmp: tmp,
        }
    }

    #[test]
    fn replaces_a_broken_app_with_a_verified_copy() {
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let f = fixture(&cc, "replace-ok", false);
        let checker = Checker::new();
        let team = |_: &Path| "TEAM123456";
        let check = as_team(&checker, &team);
        assert!(!checker.check(&f.old, Depth::FULL).is_valid());
        let r = replace_with(&f.old, &f.zip, &f.work, &check, &f.trash, &mv).unwrap();
        assert_eq!(r.app, f.old);
        // The new copy is in place and intact; the old one is in the Trash.
        assert!(checker.check(&f.old, Depth::FULL).is_valid());
        assert!(f.old.join("Contents/Resources/fresh.txt").exists());
        let trashed = r.old_in_trash.unwrap();
        assert!(trashed.join("Contents/MacOS/Thing").exists());
        assert!(!trashed.join("Contents/Resources/data.json").exists());
        assert_eq!(
            f.trash.moved.borrow().as_slice(),
            std::slice::from_ref(&f.old)
        );
    }

    #[test]
    fn a_copy_that_fails_a_check_changes_nothing() {
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let checker = Checker::new();

        // The download's own signature is broken.
        let f = fixture(&cc, "replace-broken-new", true);
        let team = |_: &Path| "TEAM123456";
        let err = replace_with(
            &f.old,
            &f.zip,
            &f.work,
            &as_team(&checker, &team),
            &f.trash,
            &mv,
        )
        .unwrap_err();
        assert!(matches!(err, ReplaceError::Untrusted(_)), "{err:?}");
        assert!(f.trash.moved.borrow().is_empty());
        assert!(f.old.join("Contents/MacOS/Thing").exists());

        // Signed by another team.
        let f = fixture(&cc, "replace-other-team", false);
        let old = f.old.clone();
        let team = move |p: &Path| {
            if p.starts_with(&old) {
                "TEAM123456"
            } else {
                "OTHER98765"
            }
        };
        let err = replace_with(
            &f.old,
            &f.zip,
            &f.work,
            &as_team(&checker, &team),
            &f.trash,
            &mv,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ReplaceError::Untrusted(
                "the downloaded copy is signed by team OTHER98765, not TEAM123456 like the installed one".into()
            )
        );
        assert!(f.trash.moved.borrow().is_empty());

        // Ad-hoc signed (the real classification): no team, so nothing to match.
        let real = |p: &Path, d: Depth| checker.check(p, d);
        let err = replace_with(&f.old, &f.zip, &f.work, &real, &f.trash, &mv).unwrap_err();
        assert!(matches!(err, ReplaceError::NotReplaceable(_)), "{err:?}");
        assert!(f.trash.moved.borrow().is_empty());
        assert!(f.old.exists());
    }

    #[test]
    fn a_download_of_another_app_changes_nothing() {
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let f = fixture(&cc, "replace-other-app", false);
        let other_dir = f.work.parent().unwrap().join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = adhoc_app(&cc, &other_dir, "Other", true);
        let zip = other_dir.join("Other.zip");
        zip_app(&other, &zip);
        let checker = Checker::new();
        let team = |_: &Path| "TEAM123456";
        let err = replace_with(
            &f.old,
            &zip,
            &f.work,
            &as_team(&checker, &team),
            &f.trash,
            &mv,
        )
        .unwrap_err();
        assert!(matches!(err, ReplaceError::BadDownload(_)), "{err:?}");
        // Not a zip at all.
        let junk = other_dir.join("junk.zip");
        std::fs::write(&junk, b"<html>not found</html>").unwrap();
        let err = replace_with(
            &f.old,
            &junk,
            &f.work,
            &as_team(&checker, &team),
            &f.trash,
            &mv,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ReplaceError::BadDownload("the download isn't a zip archive".into())
        );
        assert!(f.trash.moved.borrow().is_empty());
    }

    #[test]
    fn blocked_trash_and_failed_rename_leave_the_old_copy() {
        let Some(cc) = compiler() else {
            eprintln!("skipping: no C compiler");
            return;
        };
        struct Refuse;
        impl Trasher for Refuse {
            fn trash(&self, _: &Path) -> Result<Option<PathBuf>, TrashFailure> {
                Err(TrashFailure::Denied("not permitted".into()))
            }
        }
        let checker = Checker::new();
        let team = |_: &Path| "TEAM123456";
        let f = fixture(&cc, "replace-blocked", false);
        let err = replace_with(
            &f.old,
            &f.zip,
            &f.work,
            &as_team(&checker, &team),
            &Refuse,
            &mv,
        )
        .unwrap_err();
        assert!(matches!(err, ReplaceError::Blocked(_)), "{err:?}");
        assert!(f.old.join("Contents/MacOS/Thing").exists());

        // The new copy can't be renamed into place: the old one comes back from the Trash.
        let f = fixture(&cc, "replace-rollback", false);
        let calls = std::cell::Cell::new(0);
        let flaky = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(std::io::Error::other("disk full"))
            } else {
                std::fs::rename(a, b)
            }
        };
        let err = replace_with(
            &f.old,
            &f.zip,
            &f.work,
            &as_team(&checker, &team),
            &f.trash,
            &flaky,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ReplaceError::Failed {
                error: "couldn't put the new copy in place: disk full".into(),
                old_in_trash: None
            }
        );
        assert!(f.old.join("Contents/MacOS/Thing").exists());
        assert!(
            !f.old.join("Contents/Resources/fresh.txt").exists(),
            "still the old copy"
        );
    }

    #[test]
    fn archive_names_must_stay_inside() {
        let tmp = TempDir::new("zip-names");
        let p = tmp.path().join("a.zip");
        write_zip(
            &p,
            &[
                ("Thing.app/Contents/Info.plist", b"x", false),
                ("Thing.app/Contents/Frameworks/Current", b"A", true),
            ],
        );
        assert_eq!(zip_names_are_safe(&p), Ok(()));
        for (name, link) in [
            ("../evil", false),
            ("/etc/evil", false),
            ("a/../../x", false),
        ] {
            write_zip(&p, &[(name, b"x", link)]);
            assert!(zip_names_are_safe(&p).is_err(), "{name}");
        }
        // A link stored first, then a file written through it.
        write_zip(
            &p,
            &[
                ("Thing.app/out", b"/Applications", true),
                ("Thing.app/out/Other.app/x", b"x", false),
            ],
        );
        assert_eq!(
            zip_names_are_safe(&p),
            Err(
                "the download writes through a link inside itself (Thing.app/out/Other.app/x)"
                    .into()
            )
        );
        write_zip(&p, &[]);
        assert!(zip_names_are_safe(&p).is_err());
        std::fs::write(&p, b"PK no").unwrap();
        assert!(zip_names_are_safe(&p).is_err());
    }

    #[test]
    fn work_dir_is_beside_the_app_and_goes_away() {
        let tmp = TempDir::new("workdir");
        let app = tmp.path().join("Thing.app");
        std::fs::create_dir_all(&app).unwrap();
        let work = WorkDir::for_app(&app).unwrap();
        let dir = work.path().to_path_buf();
        assert!(dir.is_dir());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().dev(),
            std::fs::metadata(tmp.path()).unwrap().dev()
        );
        std::fs::write(dir.join("download.zip"), b"x").unwrap();
        drop(work);
        assert!(!dir.exists());
    }

    #[test]
    fn the_check_itself() {
        let old = InstalledCopy {
            path: "/Applications/Thing.app".into(),
            bundle_id: Some("com.example.thing".into()),
            team_id: "TEAM123456".into(),
        };
        let good = Signature {
            path: "/tmp/Thing.app".into(),
            status: SignatureStatus::Valid,
            kind: SignerKind::DeveloperId,
            signer: Some("Developer ID Application: Example (TEAM123456)".into()),
            team_id: Some("TEAM123456".into()),
            identifier: Some("com.example.thing".into()),
            cdhash: Some("ab".into()),
            notarized: Some(true),
            problem: None,
        };
        assert_eq!(
            check_replacement(&old, &good, Some("com.example.thing")),
            Ok(())
        );
        let adhoc = Signature {
            kind: SignerKind::AdHoc,
            team_id: None,
            ..good.clone()
        };
        assert!(check_replacement(&old, &adhoc, Some("com.example.thing")).is_err());
        let invalid = Signature {
            status: SignatureStatus::Invalid,
            ..good.clone()
        };
        assert!(check_replacement(&old, &invalid, Some("com.example.thing")).is_err());
        let dev = Signature {
            kind: SignerKind::AppleIssued,
            ..good.clone()
        };
        assert!(check_replacement(&old, &dev, Some("com.example.thing")).is_err());
        assert!(check_replacement(&old, &good, Some("com.example.other")).is_err());
        assert!(check_replacement(&old, &good, None).is_err());
    }
}
