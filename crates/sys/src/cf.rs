//! Small CoreFoundation helpers: typed reads out of the property dictionaries
//! that IOKit, IOPowerSources and IOReport hand back.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFIndex, CFNumber, CFPropertyListCreateWithData,
    CFRetained, CFString, CFType, Type,
};

/// Take ownership of a +1 CF object returned through a raw pointer.
///
/// # Safety
/// `p` must be null or a CF object the caller owns (a "Create" or "Copy" result).
pub(crate) unsafe fn owned<T: objc2_core_foundation::Type>(
    p: *const c_void,
) -> Option<CFRetained<T>> {
    NonNull::new(p as *mut T).map(|p| unsafe { CFRetained::from_raw(p) })
}

/// Retain a +0 CF object returned through a raw pointer ("Get" results).
///
/// # Safety
/// `p` must be null or a valid CF object of type `T`.
pub(crate) unsafe fn borrowed<T: objc2_core_foundation::Type>(
    p: *const c_void,
) -> Option<CFRetained<T>> {
    NonNull::new(p as *mut T).map(|p| unsafe { CFRetained::retain(p) })
}

/// The raw pointer behind a CF reference, for passing to C functions.
pub(crate) fn ptr<T: objc2_core_foundation::Type>(v: &T) -> *const c_void {
    (v as *const T).cast()
}

/// A typed view of a `CFDictionary` with `CFString` keys.
#[derive(Clone)]
pub(crate) struct Dict(pub CFRetained<CFDictionary>);

impl Dict {
    /// Wrap a dictionary-typed CF value, if it is one.
    pub fn from_value(v: &CFType) -> Option<Dict> {
        v.downcast_ref::<CFDictionary>().map(|d| Dict(d.retain()))
    }

    pub fn get(&self, key: &str) -> Option<CFRetained<CFType>> {
        let k = CFString::from_str(key);
        // SAFETY: `k` is a valid CFString; the result is +0 and retained right away.
        unsafe { borrowed(self.0.value(ptr(&*k))) }
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| string(&v))
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| int(&v))
    }

    pub fn boolean(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| boolean(&v))
    }

    pub fn data(&self, key: &str) -> Option<Vec<u8>> {
        self.get(key)
            .and_then(|v| v.downcast_ref::<CFData>().map(|d| d.to_vec()))
    }

    pub fn dict(&self, key: &str) -> Option<Dict> {
        self.get(key).and_then(|v| Dict::from_value(&v))
    }

    pub fn array(&self, key: &str) -> Vec<CFRetained<CFType>> {
        self.get(key).map(|v| array(&v)).unwrap_or_default()
    }
}

pub(crate) fn string(v: &CFType) -> Option<String> {
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub(crate) fn int(v: &CFType) -> Option<i64> {
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        return n.as_i64().or_else(|| n.as_f64().map(|f| f as i64));
    }
    v.downcast_ref::<CFBoolean>().map(|b| b.as_bool() as i64)
}

pub(crate) fn boolean(v: &CFType) -> Option<bool> {
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    v.downcast_ref::<CFNumber>()
        .and_then(|n| n.as_i64())
        .map(|n| n != 0)
}

/// The elements of a `CFArray` value (empty if it isn't one).
pub(crate) fn array(v: &CFType) -> Vec<CFRetained<CFType>> {
    let Some(a) = v.downcast_ref::<CFArray>() else {
        return Vec::new();
    };
    array_items(a)
}

pub(crate) fn array_items(a: &CFArray) -> Vec<CFRetained<CFType>> {
    (0..a.count())
        // SAFETY: the index is in bounds; the element is +0 and retained right away.
        .filter_map(|i: CFIndex| unsafe { borrowed::<CFType>(a.value_at_index(i)) })
        .collect()
}

/// Parse a property list (XML or binary) from bytes.
pub(crate) fn plist(bytes: &[u8]) -> Option<CFRetained<CFType>> {
    let data = CFData::from_bytes(bytes);
    // SAFETY: valid data; null format and error pointers are allowed.
    let pl = unsafe {
        CFPropertyListCreateWithData(
            None,
            Some(&data),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }?;
    // SAFETY: a property list is always a CF object.
    Some(unsafe { CFRetained::cast_unchecked::<CFType>(pl) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_xml_plist() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleShortVersionString</key><string>5368</string>
  <key>Count</key><integer>42</integer>
  <key>Ratio</key><real>0.5</real>
  <key>On</key><true/>
  <key>Parts</key><array><dict><key>Name</key><string>Left</string></dict></array>
</dict></plist>"#;
        let v = plist(xml).expect("parses");
        let d = Dict::from_value(&v).expect("dict");
        assert_eq!(
            d.string("CFBundleShortVersionString").as_deref(),
            Some("5368")
        );
        assert_eq!(d.int("Count"), Some(42));
        assert_eq!(d.int("Ratio"), Some(0));
        assert_eq!(d.boolean("On"), Some(true));
        assert_eq!(d.int("On"), Some(1));
        assert_eq!(d.string("Missing"), None);
        let parts = d.array("Parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(
            Dict::from_value(&parts[0])
                .and_then(|p| p.string("Name"))
                .as_deref(),
            Some("Left")
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(plist(b"not a plist").is_none());
    }
}
