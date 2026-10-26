//! Facts about this Mac: macOS and Darwin versions, CPU, region.

use std::ffi::CString;

fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: a null buffer asks for the size; the name is a valid C string.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buf has room for len bytes, as the kernel just told us.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    let s = String::from_utf8_lossy(&buf)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn sysctl_i32(name: &str) -> Option<i32> {
    let cname = CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    // SAFETY: value is an i32-sized buffer and len says so.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

/// macOS version, e.g. "26.6.2". Uses the real version even for old-SDK compatibility modes.
pub fn macos_version() -> String {
    sysctl_string("kern.osproductversion").unwrap_or_else(|| "0".into())
}

/// Darwin kernel version, e.g. "25.6.0".
pub fn darwin_version() -> String {
    sysctl_string("kern.osrelease").unwrap_or_else(|| "0".into())
}

/// True on Apple silicon, also when this process runs under Rosetta.
pub fn is_arm64() -> bool {
    sysctl_i32("hw.optional.arm64") == Some(1) || cfg!(target_arch = "aarch64")
}

/// The Mac's region as a lower-case ISO country code ("gb"), from the user's locale
/// (System Settings › General › Language & Region). Falls back to "us".
pub fn region() -> String {
    use objc2_foundation::NSLocale;
    let code = objc2::rc::autoreleasepool(|_| {
        let locale = NSLocale::currentLocale();
        if objc2::available!(macos = 13.0) {
            locale.regionCode().map(|c| c.to_string())
        } else {
            #[allow(deprecated)]
            locale.countryCode().map(|c| c.to_string())
        }
    });
    match code {
        Some(c) if c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()) => {
            c.to_ascii_lowercase()
        }
        _ => "us".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions() {
        let v = macos_version();
        assert!(
            v.split('.').next().unwrap().parse::<u32>().unwrap() >= 11,
            "{v}"
        );
        let d = darwin_version();
        assert!(
            d.split('.').next().unwrap().parse::<u32>().unwrap() >= 20,
            "{d}"
        );
        assert_eq!(region().len(), 2);
    }
}
