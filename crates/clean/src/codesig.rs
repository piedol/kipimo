//! Reads what a bundle's code signature declares (team ID, platform flag, App Group
//! entitlements) straight from the main executable's embedded signature.
//!
//! Why not `SecStaticCodeCreateWithPath` + `SecCodeCopySigningInformation`? They work, but the
//! Security framework parses each signature's certificate chain and keeps caches: reading ~50 apps
//! left about 11–20 MB of memory resident in the process for good, which a 15 MB menu-bar app can't
//! afford. The embedded signature is a stable, documented format (xnu `cs_blobs.h`), and nothing
//! here needs validation: like `SecCodeCopySigningInformation` without a validity check, this only
//! reports what the signature says. It reads a few kilobytes per app.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::native::{self, SigningInfo};
use crate::plist;

const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSMAGIC_EMBEDDED_ENTITLEMENTS: u32 = 0xfade_7171;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_ENTITLEMENTS: u32 = 5;
const MAX_LOAD_COMMANDS: u64 = 1 << 20;
const MAX_SIGNATURE: u64 = 8 << 20;

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(((be32(b, at)? as u64) << 32) | be32(b, at + 4)? as u64)
}

fn read_at(f: &mut File, offset: u64, len: u64) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.take(len).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 == len).then_some(buf)
}

/// The main executable of a bundle.
fn main_executable(bundle: &Path) -> Option<PathBuf> {
    let name = native::read_info_plist(bundle)?
        .str("CFBundleExecutable")?
        .to_owned();
    ["Contents/MacOS", "", "WrappedBundle"]
        .iter()
        .map(|d| bundle.join(d).join(&name))
        .find(|p| p.is_file())
}

/// Signing information for a bundle, or `None` if it's unsigned or unreadable.
pub(crate) fn read_bundle(bundle: &Path) -> Option<SigningInfo> {
    let exe = main_executable(bundle)?;
    read_executable(&exe).or_else(|| {
        // A bundle whose main executable is a script keeps its signature in files.
        let dir = bundle.join("Contents/_CodeSignature");
        let cd = fs::read(dir.join("CodeDirectory")).ok()?;
        let mut slots = vec![(CSSLOT_CODEDIRECTORY, cd)];
        if let Ok(ent) = fs::read(dir.join("CodeEntitlements")) {
            slots.push((CSSLOT_ENTITLEMENTS, ent));
        }
        parse_superblob(&superblob(&slots))
    })
}

/// Signing information of a Mach-O file (thin or universal; the arm64 slice is preferred), or
/// of a script or other file signed with its signature in extended attributes.
pub(crate) fn read_executable(path: &Path) -> Option<SigningInfo> {
    let mut f = File::open(path).ok()?;
    let head = read_at(&mut f, 0, 8)?;
    let magic = be32(&head, 0)?;
    if magic != FAT_MAGIC
        && magic != FAT_MAGIC_64
        && !matches!(le32(&head, 0)?, MH_MAGIC | MH_MAGIC_64)
    {
        return read_xattr_signature(path);
    }
    let slice = match magic {
        FAT_MAGIC | FAT_MAGIC_64 => {
            let wide = be32(&head, 0)? == FAT_MAGIC_64;
            let n = be32(&head, 4)?.min(32) as u64;
            let entry = if wide { 32 } else { 20 };
            let table = read_at(&mut f, 8, n * entry)?;
            let mut slices = Vec::new();
            for i in 0..n as usize {
                let at = i * entry as usize;
                let cpu = be32(&table, at)?;
                let offset = if wide {
                    be64(&table, at + 8)?
                } else {
                    be32(&table, at + 8)? as u64
                };
                slices.push((cpu, offset));
            }
            slices
                .iter()
                .find(|(c, _)| *c == CPU_TYPE_ARM64)
                .or_else(|| slices.iter().find(|(c, _)| *c == CPU_TYPE_X86_64))
                .or(slices.first())?
                .1
        }
        _ => 0,
    };
    let header = read_at(&mut f, slice, 32)?;
    let header_len = match le32(&header, 0)? {
        MH_MAGIC_64 => 32,
        MH_MAGIC => 28,
        _ => return None,
    };
    let ncmds = le32(&header, 16)? as usize;
    let sizeofcmds = (le32(&header, 20)? as u64).min(MAX_LOAD_COMMANDS);
    let cmds = read_at(&mut f, slice + header_len, sizeofcmds)?;
    let mut at = 0usize;
    let mut sig = None;
    for _ in 0..ncmds {
        let cmd = le32(&cmds, at)?;
        let size = le32(&cmds, at + 4)? as usize;
        if cmd == LC_CODE_SIGNATURE {
            sig = Some((le32(&cmds, at + 8)? as u64, le32(&cmds, at + 12)? as u64));
            break;
        }
        if size < 8 {
            return None;
        }
        at += size;
    }
    let (dataoff, datasize) = sig?;
    let blob = read_at(&mut f, slice + dataoff, datasize.min(MAX_SIGNATURE))?;
    parse_superblob(&blob)
}

/// `codesign` stores the signature of a lone non-Mach-O file (a script) in extended
/// attributes: `com.apple.cs.CodeDirectory` and `com.apple.cs.CodeEntitlements` hold the same
/// blobs a SuperBlob would.
fn read_xattr_signature(path: &Path) -> Option<SigningInfo> {
    let cd = xattr(path, "com.apple.cs.CodeDirectory")?;
    let mut slots = vec![(CSSLOT_CODEDIRECTORY, cd)];
    if let Some(ent) = xattr(path, "com.apple.cs.CodeEntitlements") {
        slots.push((CSSLOT_ENTITLEMENTS, ent));
    }
    parse_superblob(&superblob(&slots))
}

fn xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let n = std::ffi::CString::new(name).ok()?;
    // SAFETY: size query, then a read into a buffer of that size.
    let len = unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    if len <= 0 || len as u64 > MAX_SIGNATURE {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let got = unsafe {
        libc::getxattr(
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            0,
        )
    };
    (got == len).then_some(buf)
}

/// Assembles a SuperBlob from (slot, blob) pairs.
fn superblob(slots: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let header = 12 + slots.len() * 8;
    let mut index = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    for (kind, b) in slots {
        index.extend(kind.to_be_bytes());
        index.extend(((header + data.len()) as u32).to_be_bytes());
        data.extend(b);
    }
    let mut out = CSMAGIC_EMBEDDED_SIGNATURE.to_be_bytes().to_vec();
    out.extend(((header + data.len()) as u32).to_be_bytes());
    out.extend((slots.len() as u32).to_be_bytes());
    out.extend(index);
    out.extend(data);
    out
}

/// Parses an embedded signature SuperBlob.
pub(crate) fn parse_superblob(b: &[u8]) -> Option<SigningInfo> {
    if be32(b, 0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        return None;
    }
    let count = be32(b, 8)?.min(64) as usize;
    let mut info = SigningInfo::default();
    let mut found_cd = false;
    for i in 0..count {
        let kind = be32(b, 12 + i * 8)?;
        let offset = be32(b, 16 + i * 8)? as usize;
        match kind {
            CSSLOT_CODEDIRECTORY if be32(b, offset)? == CSMAGIC_CODEDIRECTORY => {
                found_cd = true;
                let cd = b.get(offset..)?;
                let version = be32(cd, 8)?;
                info.platform = cd.get(38).copied().unwrap_or(0) != 0;
                if version >= 0x20200 {
                    let team_offset = be32(cd, 48)? as usize;
                    if team_offset != 0 {
                        let raw = cd.get(team_offset..)?;
                        let end = raw.iter().position(|&c| c == 0)?;
                        let team = String::from_utf8_lossy(&raw[..end]).into_owned();
                        info.team_id = (!team.is_empty()).then_some(team);
                    }
                }
            }
            CSSLOT_ENTITLEMENTS if be32(b, offset)? == CSMAGIC_EMBEDDED_ENTITLEMENTS => {
                let len = be32(b, offset + 4)? as usize;
                let xml = b.get(offset + 8..offset + len)?;
                if let Some(p) = plist::parse(xml)
                    && let Some(groups) = p.get("com.apple.security.application-groups")
                {
                    info.app_groups = groups.strings().into_iter().map(str::to_owned).collect();
                }
            }
            _ => {}
        }
    }
    found_cd.then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut v = magic.to_be_bytes().to_vec();
        v.extend(((body.len() + 8) as u32).to_be_bytes());
        v.extend(body);
        v
    }

    /// A SuperBlob with a CodeDirectory (team ID, platform byte) and an entitlements blob.
    fn signature(team: Option<&str>, platform: u8, entitlements: Option<&str>) -> Vec<u8> {
        let mut cd = vec![0u8; 52];
        cd[0..4].copy_from_slice(&0x20400u32.to_be_bytes()); // version (after magic+length)
        cd[30] = platform; // offset 38 in the blob = 30 in the body
        if let Some(t) = team {
            cd[40..44].copy_from_slice(&(60u32).to_be_bytes()); // teamOffset (48 in the blob)
            cd.extend(t.as_bytes());
            cd.push(0);
        }
        let mut slots = vec![(CSSLOT_CODEDIRECTORY, blob(CSMAGIC_CODEDIRECTORY, &cd))];
        if let Some(e) = entitlements {
            slots.push((
                CSSLOT_ENTITLEMENTS,
                blob(CSMAGIC_EMBEDDED_ENTITLEMENTS, e.as_bytes()),
            ));
        }
        superblob(&slots)
    }

    #[test]
    fn reads_team_platform_and_groups() {
        let ent = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>com.apple.security.application-groups</key><array><string>ABCDE12345.shared</string></array></dict></plist>"#;
        let info = parse_superblob(&signature(Some("ABCDE12345"), 0, Some(ent))).unwrap();
        assert_eq!(info.team_id.as_deref(), Some("ABCDE12345"));
        assert!(!info.platform);
        assert_eq!(info.app_groups, vec!["ABCDE12345.shared"]);
        let sys = parse_superblob(&signature(None, 26, None)).unwrap();
        assert!(sys.platform && sys.team_id.is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_superblob(b"nope").is_none());
        assert!(parse_superblob(&[0xfa, 0xde, 0x0c, 0xc0, 0, 0, 0, 0, 0, 0, 0, 9]).is_none());
    }

    #[test]
    fn reads_this_test_binary() {
        // Rust test binaries are ad-hoc signed by the linker on Apple silicon: a CodeDirectory
        // with no team ID. Reads only our own executable.
        let exe = std::env::current_exe().unwrap();
        if let Some(info) = read_executable(&exe) {
            assert!(info.team_id.is_none() && !info.platform);
        }
    }
}
