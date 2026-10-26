//! Property lists, read through Foundation's `NSPropertyListSerialization` (XML and binary).

use std::collections::BTreeMap;
use std::path::Path;

use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDictionary, NSNumber, NSPropertyListMutabilityOptions,
    NSPropertyListSerialization, NSString,
};

/// A parsed property list value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plist {
    String(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    /// Seconds since 1970.
    Date(f64),
    /// Data blobs are not kept; only their length.
    Data(usize),
    Array(Vec<Plist>),
    Dict(BTreeMap<String, Plist>),
}

/// Property lists larger than this are ignored (Info.plist and launchd plists are tiny).
const MAX_BYTES: u64 = 8 << 20;
const MAX_DEPTH: usize = 24;

impl Plist {
    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(d) => d.get(key),
            _ => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Plist::as_str)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Plist::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Plist]> {
        match self {
            Plist::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Plist::Bool(b) => Some(*b),
            Plist::Integer(i) => Some(*i != 0),
            _ => None,
        }
    }

    pub fn as_date(&self) -> Option<f64> {
        match self {
            Plist::Date(d) => Some(*d),
            _ => None,
        }
    }

    /// A string, or every string in an array (launchd keys accept both).
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Plist::String(s) => vec![s.as_str()],
            Plist::Array(a) => a.iter().filter_map(Plist::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

/// Reads a property list file. Symbolic links are not followed.
pub(crate) fn read(path: &Path) -> Option<Plist> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    parse(&bytes)
}

pub(crate) fn parse(bytes: &[u8]) -> Option<Plist> {
    autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        // SAFETY: a null format pointer is allowed.
        let obj = unsafe {
            NSPropertyListSerialization::propertyListWithData_options_format_error(
                &data,
                NSPropertyListMutabilityOptions::Immutable,
                std::ptr::null_mut(),
            )
        }
        .ok()?;
        convert(&obj, 0)
    })
}

/// Converts a Foundation property list object (or a toll-free bridged CF one) to [`Plist`].
pub(crate) fn convert(obj: &AnyObject, depth: usize) -> Option<Plist> {
    if depth > MAX_DEPTH {
        return None;
    }
    if let Some(s) = obj.downcast_ref::<NSString>() {
        return Some(Plist::String(s.to_string()));
    }
    if let Some(n) = obj.downcast_ref::<NSNumber>() {
        let class = obj.class().name().to_string_lossy().into_owned();
        if class.contains("Boolean") {
            return Some(Plist::Bool(n.as_bool()));
        }
        let enc = n.encoding();
        return Some(match enc {
            objc2::Encoding::Float | objc2::Encoding::Double => Plist::Real(n.as_f64()),
            _ => Plist::Integer(n.as_i64()),
        });
    }
    if let Some(d) = obj.downcast_ref::<NSDate>() {
        return Some(Plist::Date(d.timeIntervalSince1970()));
    }
    if let Some(d) = obj.downcast_ref::<NSData>() {
        return Some(Plist::Data(d.len()));
    }
    if let Some(a) = obj.downcast_ref::<NSArray<AnyObject>>() {
        return Some(Plist::Array(
            a.iter().filter_map(|v| convert(&v, depth + 1)).collect(),
        ));
    }
    if let Some(d) = obj.downcast_ref::<NSDictionary<AnyObject, AnyObject>>() {
        let (keys, values) = d.to_vecs();
        let mut map = BTreeMap::new();
        for (k, v) in keys.iter().zip(values.iter()) {
            let Some(k) = k.downcast_ref::<NSString>() else {
                continue;
            };
            if let Some(v) = convert(v, depth + 1) {
                map.insert(k.to_string(), v);
            }
        }
        return Some(Plist::Dict(map));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xml() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.App</string>
<key>Count</key><integer>3</integer>
<key>On</key><true/>
<key>When</key><date>2024-01-02T03:04:05Z</date>
<key>List</key><array><string>a</string><string>b</string></array>
</dict></plist>"#;
        let p = parse(xml).unwrap();
        assert_eq!(p.str("CFBundleIdentifier"), Some("com.example.App"));
        assert_eq!(p.get("Count"), Some(&Plist::Integer(3)));
        assert_eq!(p.get("On").and_then(Plist::as_bool), Some(true));
        assert_eq!(
            p.get("When").and_then(Plist::as_date),
            Some(1_704_164_645.0)
        );
        assert_eq!(p.get("List").unwrap().strings(), vec!["a", "b"]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"not a plist").is_none());
    }
}
