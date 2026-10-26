//! Small Core Foundation helpers shared by the Security, ServiceManagement, IOKit and
//! LaunchServices readers. Everything here is read-only conversion from CF values to Rust.

use std::path::PathBuf;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFError, CFNumber, CFRetained, CFString, CFType, CFURL,
};

/// Looks up `key` in an untyped CF dictionary.
pub(crate) fn dict_get(dict: &CFDictionary, key: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: every dictionary we read (signing information, launchd job dictionaries, kext
    // info, error user info) has CFString keys and CF object values, so reinterpreting the
    // generics as <CFString, CFType> is valid.
    let typed: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    typed.get(key)
}

pub(crate) fn dict_get_str(dict: &CFDictionary, key: &str) -> Option<String> {
    dict_get(dict, &CFString::from_str(key)).and_then(|v| as_string(&v))
}

pub(crate) fn dict_get_i64(dict: &CFDictionary, key: &str) -> Option<i64> {
    dict_get(dict, &CFString::from_str(key)).and_then(|v| as_i64(&v))
}

pub(crate) fn dict_get_bool(dict: &CFDictionary, key: &str) -> Option<bool> {
    dict_get(dict, &CFString::from_str(key)).and_then(|v| as_bool(&v))
}

pub(crate) fn dict_get_array(dict: &CFDictionary, key: &CFString) -> Option<CFRetained<CFArray>> {
    dict_get(dict, key).and_then(|v| v.downcast::<CFArray>().ok())
}

/// All keys and values of an untyped dictionary, as CF objects.
pub(crate) fn dict_entries(dict: &CFDictionary) -> Vec<(CFRetained<CFType>, CFRetained<CFType>)> {
    // SAFETY: keys and values of any CF dictionary are CF objects.
    let typed: &CFDictionary<CFType, CFType> = unsafe { dict.cast_unchecked() };
    let (keys, values) = typed.to_vecs();
    keys.into_iter().zip(values).collect()
}

pub(crate) fn array_items(array: &CFArray) -> Vec<CFRetained<CFType>> {
    // SAFETY: elements of any CF array we read are CF objects.
    let typed: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    typed.to_vec()
}

pub(crate) fn as_string(value: &CFType) -> Option<String> {
    value.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub(crate) fn as_i64(value: &CFType) -> Option<i64> {
    if let Some(n) = value.downcast_ref::<CFNumber>() {
        return n.as_i64();
    }
    value
        .downcast_ref::<CFBoolean>()
        .map(|b| i64::from(b.as_bool()))
}

pub(crate) fn as_bool(value: &CFType) -> Option<bool> {
    if let Some(b) = value.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    value
        .downcast_ref::<CFNumber>()
        .and_then(|n| n.as_i64())
        .map(|n| n != 0)
}

pub(crate) fn as_path(value: &CFType) -> Option<PathBuf> {
    if let Some(url) = value.downcast_ref::<CFURL>() {
        return url.to_file_path();
    }
    as_string(value).map(PathBuf::from)
}

pub(crate) fn file_url(path: &std::path::Path) -> Option<CFRetained<CFURL>> {
    if path.is_dir() {
        CFURL::from_directory_path(path)
    } else {
        CFURL::from_file_path(path)
    }
}

/// The localized description of a CFError, falling back to its domain and code.
pub(crate) fn error_message(error: &CFError) -> String {
    error
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|| format!("error {}", error.code()))
}
