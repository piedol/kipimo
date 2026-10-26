//! Which Mac this is: "MacBook Pro (16-inch, M5 Max)", "Mac17,6", "Apple M5 Max".

use serde::Serialize;

use crate::iokit;
use crate::memory::{sysctl_string, sysctl_u64};

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct MacModel {
    /// Marketing name, e.g. "MacBook Pro (16-inch, M5 Max)". Falls back to the identifier.
    pub name: String,
    /// Model identifier, e.g. "Mac17,6".
    pub identifier: String,
    /// e.g. "Apple M5 Max".
    pub chip: String,
    pub memory_bytes: u64,
}

/// A device-tree string property: bytes with a trailing NUL.
pub(crate) fn tree_string(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn mac_model() -> MacModel {
    let identifier = sysctl_string("hw.model").unwrap_or_default();
    // Apple silicon publishes the marketing name in the device tree.
    let name = iokit::entry("IODeviceTree:/product")
        .and_then(|e| e.properties())
        .and_then(|p| p.data("product-name"))
        .and_then(|d| tree_string(&d))
        .unwrap_or_else(|| {
            if identifier.is_empty() {
                "Mac".into()
            } else {
                identifier.clone()
            }
        });
    MacModel {
        name,
        identifier,
        chip: sysctl_string("machdep.cpu.brand_string").unwrap_or_default(),
        memory_bytes: sysctl_u64("hw.memsize").unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_tree_strings() {
        assert_eq!(
            tree_string(b"MacBook Pro (16-inch, M5 Max)\0").as_deref(),
            Some("MacBook Pro (16-inch, M5 Max)")
        );
        assert_eq!(tree_string(b"Mac mini").as_deref(), Some("Mac mini"));
        assert_eq!(tree_string(b"\0"), None);
    }

    #[test]
    fn live_model() {
        let m = mac_model();
        assert!(!m.name.is_empty() && !m.identifier.is_empty());
        assert!(m.memory_bytes > 0);
    }
}
