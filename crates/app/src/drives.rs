//! External drives mounting and ejecting, from DiskArbitration. Disk images,
//! network shares, internal disks and system volumes are ignored. Callbacks
//! arrive on the main queue.

use std::ffi::c_void;
use std::path::PathBuf;

use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFString, CFType, CFURL};

use crate::util;

type Ref = *const c_void;
type Changed = extern "C" fn(Ref, Ref, *mut c_void);

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(alloc: Ref) -> Ref;
    fn DASessionSetDispatchQueue(session: Ref, queue: Ref);
    fn DARegisterDiskDescriptionChangedCallback(
        session: Ref,
        matching: Ref,
        watch: Ref,
        callback: Changed,
        context: *mut c_void,
    );
    fn DADiskCopyDescription(disk: Ref) -> Ref;
}

/// What a callback needs to know about a volume.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Drive {
    pub name: String,
    pub path: Option<PathBuf>,
    pub internal: Option<bool>,
    pub network: bool,
    pub model: Option<String>,
    pub protocol: Option<String>,
    /// The BSD name ("disk4s1"), the same before and after unmounting.
    pub bsd: String,
}

impl Drive {
    /// A drive someone plugged in: not internal, not a disk image, not a network
    /// share, and mounted in /Volumes (system volumes live elsewhere).
    pub fn is_external(&self) -> bool {
        self.internal == Some(false)
            && !self.network
            && self.model.as_deref() != Some("Disk Image")
            && self.protocol.as_deref() != Some("Virtual Interface")
    }

    pub fn mounted_in_volumes(&self) -> bool {
        self.path
            .as_ref()
            .is_some_and(|p| p.starts_with("/Volumes/"))
    }
}

fn string(d: &CFDictionary, key: &str) -> Option<String> {
    let k = CFString::from_str(key);
    // SAFETY: a valid dictionary and key; the value is +0.
    let v = unsafe { d.value((&*k as *const CFString).cast()) };
    let v = std::ptr::NonNull::new(v as *mut CFType)?;
    // SAFETY: a live CF object owned by the dictionary.
    unsafe { v.as_ref() }
        .downcast_ref::<CFString>()
        .map(|s| s.to_string())
}

fn boolean(d: &CFDictionary, key: &str) -> Option<bool> {
    let k = CFString::from_str(key);
    // SAFETY: as above.
    let v = unsafe { d.value((&*k as *const CFString).cast()) };
    let v = std::ptr::NonNull::new(v as *mut CFType)?;
    unsafe { v.as_ref() }
        .downcast_ref::<objc2_core_foundation::CFBoolean>()
        .map(|b| b.as_bool())
}

fn path(d: &CFDictionary) -> Option<PathBuf> {
    let k = CFString::from_str("DAVolumePath");
    // SAFETY: as above.
    let v = unsafe { d.value((&*k as *const CFString).cast()) };
    let v = std::ptr::NonNull::new(v as *mut CFType)?;
    let url = unsafe { v.as_ref() }.downcast_ref::<CFURL>()?;
    let mut buf = [0u8; 1024];
    // SAFETY: the buffer holds 1024 bytes.
    let ok = unsafe { url.file_system_representation(true, buf.as_mut_ptr(), buf.len() as isize) };
    if !ok {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(PathBuf::from(
        String::from_utf8_lossy(&buf[..end]).into_owned(),
    ))
}

fn describe(disk: Ref) -> Option<Drive> {
    // SAFETY: a live DADisk; the description comes back +1.
    let d: CFRetained<CFDictionary> = unsafe { crate_cf_owned(DADiskCopyDescription(disk))? };
    Some(Drive {
        name: string(&d, "DAVolumeName").unwrap_or_else(|| crate::i18n::tr("Drive")),
        path: path(&d),
        internal: boolean(&d, "DADeviceInternal"),
        network: boolean(&d, "DAVolumeNetwork").unwrap_or(false),
        model: string(&d, "DADeviceModel"),
        protocol: string(&d, "DADeviceProtocol"),
        bsd: string(&d, "DAMediaBSDName").unwrap_or_default(),
    })
}

unsafe fn crate_cf_owned<T: objc2_core_foundation::Type>(p: Ref) -> Option<CFRetained<T>> {
    std::ptr::NonNull::new(p as *mut T).map(|p| unsafe { CFRetained::from_raw(p) })
}

extern "C" fn changed(disk: Ref, _keys: Ref, _ctx: *mut c_void) {
    if let Some(d) = describe(disk) {
        crate::background::drive_changed(d);
    }
}

/// Keeps the DiskArbitration session alive.
pub struct DriveWatcher {
    _session: CFRetained<CFType>,
}

/// Start listening. Callbacks run on the main queue.
pub fn start() -> Option<DriveWatcher> {
    // SAFETY: plain DiskArbitration calls with valid arguments; the session is +1.
    unsafe {
        let session: CFRetained<CFType> = crate_cf_owned(DASessionCreate(std::ptr::null()))?;
        let key = CFString::from_str("DAVolumePath");
        let watch: CFRetained<CFArray<CFString>> = CFArray::from_retained_objects(&[key]);
        DARegisterDiskDescriptionChangedCallback(
            (&*session as *const CFType).cast(),
            std::ptr::null(),
            (&*watch as *const CFArray<CFString>).cast(),
            changed,
            std::ptr::null_mut(),
        );
        let main = dispatch2::DispatchQueue::main();
        DASessionSetDispatchQueue(
            (&*session as *const CFType).cast(),
            (main as *const dispatch2::DispatchQueue).cast(),
        );
        util::log("watching for external drives");
        Some(DriveWatcher { _session: session })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(internal: bool, model: &str, protocol: &str, network: bool, path: &str) -> Drive {
        Drive {
            name: "X".into(),
            path: (!path.is_empty()).then(|| PathBuf::from(path)),
            internal: Some(internal),
            network,
            model: Some(model.into()),
            protocol: Some(protocol.into()),
            bsd: "disk9s1".into(),
        }
    }

    #[test]
    fn only_plugged_in_drives_count() {
        let usb = drive(false, "Extreme SSD", "USB", false, "/Volumes/Extreme SSD");
        assert!(usb.is_external() && usb.mounted_in_volumes());
        assert!(
            !drive(
                false,
                "Disk Image",
                "Virtual Interface",
                false,
                "/Volumes/Installer"
            )
            .is_external()
        );
        assert!(!drive(true, "APPLE SSD", "Apple Fabric", false, "/").is_external());
        assert!(!drive(false, "", "", true, "/Volumes/share").is_external());
        // An external drive's helper volume mounted outside /Volumes isn't announced.
        assert!(
            !drive(
                false,
                "Extreme SSD",
                "USB",
                false,
                "/System/Volumes/Update/mnt1"
            )
            .mounted_in_volumes()
        );
        // Unmounted: no path any more.
        assert!(!drive(false, "Extreme SSD", "USB", false, "").mounted_in_volumes());
    }
}
