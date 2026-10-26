//! The handful of IOKit calls the readers need, declared directly.

use std::ffi::{CString, c_char, c_void};

use objc2_core_foundation::CFDictionary;

use crate::cf::{self, Dict};

#[allow(non_camel_case_types)]
pub(crate) type io_object_t = u32;
#[allow(non_camel_case_types)]
pub(crate) type kern_return_t = i32;

/// `kIOMainPortDefault` (MACH_PORT_NULL).
const MAIN_PORT: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main: u32, matching: *mut c_void) -> io_object_t;
    fn IOServiceGetMatchingServices(
        main: u32,
        matching: *mut c_void,
        it: *mut io_object_t,
    ) -> kern_return_t;
    fn IOIteratorNext(it: io_object_t) -> io_object_t;
    fn IOObjectRelease(o: io_object_t) -> kern_return_t;
    fn IORegistryEntryCreateCFProperties(
        entry: io_object_t,
        props: *mut *mut c_void,
        alloc: *const c_void,
        options: u32,
    ) -> kern_return_t;
    fn IORegistryEntryFromPath(main: u32, path: *const c_char) -> io_object_t;
    fn IORegistryEntryCreateCFProperty(
        entry: io_object_t,
        key: *const c_void,
        alloc: *const c_void,
        options: u32,
    ) -> *const c_void;
    pub(crate) fn IOServiceOpen(
        svc: io_object_t,
        task: u32,
        ty: u32,
        conn: *mut io_object_t,
    ) -> kern_return_t;
    pub(crate) fn IOServiceClose(conn: io_object_t) -> kern_return_t;
    pub(crate) fn IOConnectCallStructMethod(
        conn: io_object_t,
        selector: u32,
        input: *const c_void,
        in_size: usize,
        output: *mut c_void,
        out_size: *mut usize,
    ) -> kern_return_t;

    pub(crate) fn IOPSCopyPowerSourcesInfo() -> *const c_void;
    pub(crate) fn IOPSCopyPowerSourcesList(blob: *const c_void) -> *const c_void;
    pub(crate) fn IOPSGetPowerSourceDescription(
        blob: *const c_void,
        ps: *const c_void,
    ) -> *const c_void;
}

unsafe extern "C" {
    static mach_task_self_: u32;
}

pub(crate) fn task_self() -> u32 {
    // SAFETY: a process-wide constant set up by libSystem.
    unsafe { mach_task_self_ }
}

/// An IOKit object released on drop.
pub(crate) struct IoObject(pub io_object_t);

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: we own this reference.
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

impl IoObject {
    /// One property of a registry entry. Much cheaper than [`properties`](Self::properties)
    /// for entries with big property tables, such as the battery gauge.
    pub fn property(
        &self,
        key: &str,
    ) -> Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFType>> {
        let k = objc2_core_foundation::CFString::from_str(key);
        // SAFETY: valid entry and key; the value comes back +1 or null.
        unsafe {
            cf::owned(IORegistryEntryCreateCFProperty(
                self.0,
                cf::ptr(&*k),
                std::ptr::null(),
                0,
            ))
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.property(key).and_then(|v| cf::int(&v))
    }

    /// All properties of a registry entry.
    pub fn properties(&self) -> Option<Dict> {
        let mut out: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid entry and out pointer; the dictionary comes back +1.
        let kr =
            unsafe { IORegistryEntryCreateCFProperties(self.0, &mut out, std::ptr::null(), 0) };
        if kr != 0 {
            return None;
        }
        // SAFETY: a +1 CFMutableDictionary on success.
        unsafe { cf::owned::<CFDictionary>(out) }.map(Dict)
    }
}

/// The first service of an IOKit class.
pub(crate) fn service(class: &str) -> Option<IoObject> {
    let name = CString::new(class).ok()?;
    // SAFETY: the matching dictionary is consumed by the call.
    let s = unsafe { IOServiceGetMatchingService(MAIN_PORT, IOServiceMatching(name.as_ptr())) };
    (s != 0).then_some(IoObject(s))
}

/// Every service of an IOKit class.
pub(crate) fn services(class: &str) -> Vec<IoObject> {
    let Ok(name) = CString::new(class) else {
        return Vec::new();
    };
    let mut it: io_object_t = 0;
    // SAFETY: the matching dictionary is consumed by the call.
    let kr = unsafe {
        IOServiceGetMatchingServices(MAIN_PORT, IOServiceMatching(name.as_ptr()), &mut it)
    };
    if kr != 0 || it == 0 {
        return Vec::new();
    }
    let it = IoObject(it);
    let mut out = Vec::new();
    loop {
        // SAFETY: a valid iterator; each entry comes back +1.
        let o = unsafe { IOIteratorNext(it.0) };
        if o == 0 {
            break;
        }
        out.push(IoObject(o));
    }
    out
}

/// A registry entry by path, such as `IODeviceTree:/product`.
pub(crate) fn entry(path: &str) -> Option<IoObject> {
    let p = CString::new(path).ok()?;
    // SAFETY: a valid C string.
    let e = unsafe { IORegistryEntryFromPath(MAIN_PORT, p.as_ptr()) };
    (e != 0).then_some(IoObject(e))
}
