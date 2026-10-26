//! Thin wrappers over the macOS system calls the scanner needs: `getattrlistbulk` (one call reads
//! the name, type, flags, inode, link count and allocated size of many directory entries at once,
//! with no per-file `lstat`), `openat`/`fstat`/`fstatfs`, the mount table (`getfsstat`), the APFS
//! firmlink list, and the I/O policy that stops a scan from downloading iCloud placeholders.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

// Constants that the libc crate does not carry (values from the macOS SDK headers).
/// `<sys/attr.h>`: per-entry error code, placed right after `ATTR_CMN_RETURNED_ATTRS`.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// `<sys/stat.h>`: the object is a dataless placeholder (iCloud "Optimise Mac Storage").
pub(crate) const SF_DATALESS: u32 = 0x4000_0000;
/// `<sys/stat.h>`: a macOS data vault; only processes with a private Apple entitlement may read
/// it, Full Disk Access or not.
pub(crate) const UF_DATAVAULT: u32 = 0x0000_0080;
/// `<sys/stat.h>` extended flags (`ATTR_CMNEXT_EXT_FLAGS`): the file may share blocks with an
/// APFS clone.
const EF_MAY_SHARE_BLOCKS: u64 = 0x1;
/// `<sys/vnode.h>` `enum vtype`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
/// `<sys/resource.h>`.
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
const IOPOL_SCOPE_THREAD: c_int = 1;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

unsafe extern "C" {
    fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
}

/// Makes this thread fail with `EDEADLK` instead of downloading a dataless (cloud-only) file or
/// folder when it touches one, so sizing a folder never pulls data from iCloud.
pub(crate) fn disable_dataless_materialization() {
    // SAFETY: plain syscall wrapper with constant arguments.
    unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_THREAD,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        );
    }
}

fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// An open directory file descriptor, closed on drop.
pub(crate) struct Dir {
    fd: c_int,
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor.
        unsafe { libc::close(self.fd) };
    }
}

fn openat_dir(dirfd: c_int, name: &CStr) -> io::Result<Dir> {
    // O_NOFOLLOW: never follow a symbolic link, even if one replaced the folder after we listed it.
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    loop {
        // SAFETY: `name` is a valid C string; the result is checked.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
        if fd >= 0 {
            return Ok(Dir { fd });
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

/// Information about an open directory itself.
pub(crate) struct DirStat {
    pub dev: u64,
    pub ino: u64,
    /// Allocated bytes of the directory object itself (0 on APFS, a few KB on HFS+).
    pub alloc: u64,
}

impl Dir {
    /// Opens a directory without following a final symbolic link. Paths longer than `PATH_MAX`
    /// (deep nesting) are opened one component at a time with `openat`.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() < libc::PATH_MAX as usize - 1 {
            return openat_dir(libc::AT_FDCWD, &cstring(bytes)?);
        }
        let mut current: Option<Dir> = None;
        for component in path.components() {
            let name: &OsStr = match component {
                Component::RootDir => OsStr::new("/"),
                Component::Normal(name) => name,
                Component::CurDir => continue,
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
            };
            let base = current.as_ref().map_or(libc::AT_FDCWD, |dir| dir.fd);
            current = Some(openat_dir(base, &cstring(name.as_bytes())?)?);
        }
        current.ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))
    }

    pub fn stat(&self) -> io::Result<DirStat> {
        // SAFETY: zeroed `stat` is a valid out-parameter; the result is checked.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.fd, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(DirStat {
            dev: st.st_dev as u64,
            ino: st.st_ino,
            alloc: (st.st_blocks.max(0) as u64) * 512,
        })
    }

    /// The volume this directory lives on.
    pub fn volume(&self) -> Option<Volume> {
        // SAFETY: zeroed `statfs` is a valid out-parameter; the result is checked.
        let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(self.fd, &mut sf) } != 0 {
            return None;
        }
        Some(Volume::from_statfs(&sf))
    }
}

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// One directory entry as returned by `getattrlistbulk`.
pub(crate) struct BulkEntry<'a> {
    pub name: &'a [u8],
    pub kind: Kind,
    /// `st_flags` (`SF_FIRMLINK`, `SF_DATALESS`, ...).
    pub flags: u32,
    pub file_id: u64,
    pub link_count: u32,
    /// Allocated bytes on disk (all forks), the same figure `st_blocks * 512` gives.
    pub alloc: u64,
    pub mount_point: bool,
    /// Non-zero when the kernel could not read this entry's attributes.
    pub error: u32,
    /// Only filled when clone information was requested: whether the file may share blocks with
    /// an APFS clone, its clone family id, and the bytes it alone owns (freed if it is deleted).
    pub may_share_blocks: bool,
    pub clone_id: u64,
    pub private_size: Option<u64>,
}

/// Reads whole directories with `getattrlistbulk`, reusing one buffer.
pub(crate) struct BulkReader {
    buf: Vec<u64>,
}

const BULK_BUFFER_BYTES: usize = 128 * 1024;

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Parses one packed attribute group. Attributes come in bit order inside each group (common,
/// directory, file, then the extended common ones requested through `forkattr`), 4-byte
/// aligned, and only the ones flagged in the returned set are present.
fn parse_entry(b: &[u8]) -> Option<BulkEntry<'_>> {
    let mut p = 4; // skip the group length
    let common = read_u32(b, p)?;
    let dirattr = read_u32(b, p + 8)?;
    let fileattr = read_u32(b, p + 12)?;
    let extended = read_u32(b, p + 16)?;
    p += std::mem::size_of::<libc::attribute_set_t>();
    let mut entry = BulkEntry {
        name: &[],
        kind: Kind::Other,
        flags: 0,
        file_id: 0,
        link_count: 1,
        alloc: 0,
        mount_point: false,
        error: 0,
        may_share_blocks: false,
        clone_id: 0,
        private_size: None,
    };
    if common & ATTR_CMN_ERROR != 0 {
        entry.error = read_u32(b, p)?;
        p += 4;
    }
    if common & libc::ATTR_CMN_NAME != 0 {
        let offset = read_u32(b, p)? as i32;
        let length = read_u32(b, p + 4)? as usize;
        let start = usize::try_from(p as isize + offset as isize).ok()?;
        let raw = b.get(start..start + length)?;
        // The length includes the terminating NUL.
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        entry.name = &raw[..end];
        p += 8;
    }
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        entry.kind = match read_u32(b, p)? {
            VDIR => Kind::Dir,
            VREG => Kind::File,
            VLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        p += 4;
    }
    if common & libc::ATTR_CMN_FLAGS != 0 {
        entry.flags = read_u32(b, p)?;
        p += 4;
    }
    if common & libc::ATTR_CMN_FILEID != 0 {
        entry.file_id = read_u64(b, p)?;
        p += 8;
    }
    if dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        entry.mount_point = read_u32(b, p)? & libc::DIR_MNTSTATUS_MNTPOINT != 0;
        p += 4;
    }
    if fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
        entry.link_count = read_u32(b, p)?;
        p += 4;
    }
    if fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        entry.alloc = read_u64(b, p)?;
        p += 8;
    }
    if extended & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
        entry.private_size = Some(read_u64(b, p)?);
        p += 8;
    }
    if extended & libc::ATTR_CMNEXT_CLONEID != 0 {
        entry.clone_id = read_u64(b, p)?;
        p += 8;
    }
    if extended & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
        entry.may_share_blocks = read_u64(b, p)? & EF_MAY_SHARE_BLOCKS != 0;
    }
    Some(entry)
}

impl BulkReader {
    pub fn new() -> Self {
        BulkReader {
            buf: vec![0; BULK_BUFFER_BYTES / 8],
        }
    }

    /// Calls `each` for every entry of `dir` (never `.` or `..`). Stops early, returning
    /// `Ok(false)`, when `stop` returns true between batches. Entries already delivered stay
    /// delivered if a later batch fails. `clone_info` also asks for APFS clone details.
    pub fn read_all(
        &mut self,
        dir: &Dir,
        clone_info: bool,
        mut stop: impl FnMut() -> bool,
        mut each: impl FnMut(&BulkEntry<'_>),
    ) -> io::Result<bool> {
        let mut list = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | ATTR_CMN_ERROR
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_FLAGS
                | libc::ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: libc::ATTR_DIR_MOUNTSTATUS,
            fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
            forkattr: if clone_info {
                libc::ATTR_CMNEXT_PRIVATESIZE
                    | libc::ATTR_CMNEXT_CLONEID
                    | libc::ATTR_CMNEXT_EXT_FLAGS
            } else {
                0
            },
        };
        let options = if clone_info {
            u64::from(libc::FSOPT_ATTR_CMN_EXTENDED)
        } else {
            0
        };
        let size = self.buf.len() * 8;
        loop {
            // SAFETY: `list` and `buf` are valid for the sizes given; the result is checked.
            let count = unsafe {
                libc::getattrlistbulk(
                    dir.fd,
                    (&mut list as *mut libc::attrlist).cast::<c_void>(),
                    self.buf.as_mut_ptr().cast::<c_void>(),
                    size,
                    options,
                )
            };
            if count < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            if count == 0 {
                return Ok(true);
            }
            // SAFETY: the buffer is `size` initialised bytes (a zeroed Vec<u64>).
            let bytes = unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), size) };
            let mut offset = 0usize;
            for _ in 0..count {
                let Some(length) = read_u32(bytes, offset).map(|l| l as usize) else {
                    break;
                };
                if length == 0 || offset + length > bytes.len() {
                    break;
                }
                if let Some(entry) = parse_entry(&bytes[offset..offset + length]) {
                    each(&entry);
                }
                offset += length;
            }
            if stop() {
                return Ok(false);
            }
        }
    }
}

/// A mounted volume, from the mount table or `fstatfs`.
#[derive(Clone, Debug)]
pub(crate) struct Volume {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub local: bool,
    pub browsable: bool,
    pub read_only: bool,
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

fn c_chars_to_bytes(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect()
}

impl Volume {
    fn from_statfs(sf: &libc::statfs) -> Volume {
        let bsize = u64::from(sf.f_bsize);
        let flags = sf.f_flags as c_int;
        Volume {
            mount_point: PathBuf::from(OsStr::from_bytes(&c_chars_to_bytes(&sf.f_mntonname))),
            fs_type: String::from_utf8_lossy(&c_chars_to_bytes(&sf.f_fstypename)).into_owned(),
            local: flags & libc::MNT_LOCAL != 0,
            browsable: flags & libc::MNT_DONTBROWSE == 0,
            read_only: flags & libc::MNT_RDONLY != 0,
            total: sf.f_blocks.saturating_mul(bsize),
            used: sf.f_blocks.saturating_sub(sf.f_bfree).saturating_mul(bsize),
            available: sf.f_bavail.saturating_mul(bsize),
        }
    }

    /// File systems the scanner never enters: device nodes and automounter triggers (entering an
    /// autofs mount can block on the network).
    pub fn never_enter(&self) -> bool {
        matches!(self.fs_type.as_str(), "devfs" | "autofs" | "fdesc")
    }
}

/// The mount table, read without waiting on unresponsive network volumes (`MNT_NOWAIT`).
pub(crate) fn mounts() -> Vec<Volume> {
    // SAFETY: a null buffer asks for the count; the second call fills our own buffer.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Vec::new();
    }
    let capacity = count as usize + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(capacity);
    let bytes = (capacity * std::mem::size_of::<libc::statfs>()) as c_int;
    // SAFETY: `buf` has room for `capacity` entries; we only read back what the call wrote.
    let filled = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if filled <= 0 {
        return Vec::new();
    }
    unsafe { buf.set_len((filled as usize).min(capacity)) };
    buf.iter().map(Volume::from_statfs).collect()
}

/// Mount table indexed by mount point.
pub(crate) fn mounts_by_path() -> HashMap<PathBuf, Volume> {
    mounts()
        .into_iter()
        .map(|volume| (volume.mount_point.clone(), volume))
        .collect()
}

/// `lstat` returning device, inode, allocated bytes and link count.
pub(crate) struct ItemStat {
    pub is_dir: bool,
    pub alloc: u64,
    pub link_count: u64,
}

pub(crate) fn lstat(path: &Path) -> io::Result<ItemStat> {
    let c = cstring(path.as_os_str().as_bytes())?;
    // SAFETY: zeroed `stat` is a valid out-parameter; the result is checked.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ItemStat {
        is_dir: st.st_mode & libc::S_IFMT == libc::S_IFDIR,
        alloc: (st.st_blocks.max(0) as u64) * 512,
        link_count: u64::from(st.st_nlink),
    })
}

/// `stat` (following firmlinks and symlinks) returning `(device, inode)`.
pub(crate) fn dev_ino(path: &Path) -> Option<(u64, u64)> {
    let c = cstring(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: as above.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.st_dev as u64, st.st_ino))
}

/// APFS firmlinks (macOS 10.15+): directories on the read-only system volume such as `/Users` or
/// `/Applications` that are really folders of the Data volume, which is also mounted at
/// `/System/Volumes/Data`. Returns `(device, inode) -> firmlink path` for every firmlink inside
/// `root`, so a scan can show the folder once, at its familiar path, and skip the duplicate view.
pub(crate) fn firmlinks_within(root: &Path) -> HashMap<(u64, u64), PathBuf> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string("/usr/share/firmlinks") else {
        return map;
    };
    for line in text.lines() {
        let Some(source) = line.split('\t').next().map(str::trim) else {
            continue;
        };
        if !source.starts_with('/') {
            continue;
        }
        let source = PathBuf::from(source);
        if !source.starts_with(root) {
            continue;
        }
        if let Some(key) = dev_ino(&source) {
            map.insert(key, source);
        }
    }
    map
}

/// The real path of `path` (symlinks resolved), via `realpath(3)`.
pub(crate) fn real_path(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Volume name of the volume mounted at `mount_point` ("Macintosh HD", "Backup", ...).
pub(crate) fn volume_name(mount_point: &Path) -> Option<String> {
    let c = cstring(mount_point.as_os_str().as_bytes()).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_NAME,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = [0u64; 128];
    // SAFETY: valid pointers and sizes; the result is checked.
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&mut list as *mut libc::attrlist).cast::<c_void>(),
            buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&buf),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: the buffer is fully initialised.
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), std::mem::size_of_val(&buf))
    };
    // Layout: u32 length, then the attrreference_t for the name.
    let p = 4;
    let offset = read_u32(bytes, p)? as i32;
    let length = read_u32(bytes, p + 4)? as usize;
    let start = usize::try_from(p as isize + offset as isize).ok()?;
    let raw = bytes.get(start..start + length)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let name = String::from_utf8_lossy(&raw[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// The user's home folder (from `$HOME`, falling back to the password database).
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    // SAFETY: getpwuid returns a pointer to static storage or null; we copy out immediately.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let dir = CStr::from_ptr((*pw).pw_dir);
        Some(PathBuf::from(OsStr::from_bytes(dir.to_bytes())))
    }
}

/// Whether `errno` means the folder is protected by macOS privacy controls (TCC) rather than
/// ordinary Unix permissions: TCC denials return `EPERM`, permission bits return `EACCES`.
pub(crate) fn is_privacy_denial(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EPERM)
}

/// `EDEADLK` is what a dataless (cloud-only) item returns once materialisation is disabled.
pub(crate) fn is_dataless_error(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EDEADLK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_reader_lists_a_folder() {
        let dir = std::env::temp_dir().join(format!("kipimo-ffi-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), vec![1u8; 10_000]).unwrap();
        let opened = Dir::open(&dir).unwrap();
        let mut reader = BulkReader::new();
        let mut seen = Vec::new();
        reader
            .read_all(
                &opened,
                true,
                || false,
                |e| seen.push((e.name.to_vec(), e.kind, e.alloc)),
            )
            .unwrap();
        seen.sort();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, b"a.txt");
        assert_eq!(seen[0].1, Kind::File);
        assert!(seen[0].2 >= 10_000);
        assert_eq!(seen[1].0, b"sub");
        assert_eq!(seen[1].1, Kind::Dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mount_table_has_root() {
        let mounts = mounts_by_path();
        assert!(mounts.contains_key(Path::new("/")));
    }
}
