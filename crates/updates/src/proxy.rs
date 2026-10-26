//! The system proxy for a URL, the way macOS apps get it: `CFNetworkCopySystemProxySettings`
//! (System Settings › Network › Details › Proxies) and `CFNetworkCopyProxiesForURL`, which
//! applies the bypass list ("Bypass proxy settings for these hosts", simple host names).
//!
//! Supported: HTTP and HTTPS ("secure web") proxies and SOCKS. Automatic configuration (PAC
//! files, WPAD) isn't evaluated; those requests go direct.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

#[link(name = "CFNetwork", kind = "framework")]
unsafe extern "C" {
    // Both return +1 references (Copy rule). CFDictionary/CFArray/CFURL are toll-free bridged
    // with their Foundation counterparts.
    fn CFNetworkCopySystemProxySettings() -> *mut NSDictionary<NSString, AnyObject>;
    fn CFNetworkCopyProxiesForURL(
        url: &NSURL,
        proxy_settings: &NSDictionary<NSString, AnyObject>,
    ) -> *mut NSArray<NSDictionary<NSString, AnyObject>>;
    static kCFProxyTypeKey: &'static NSString;
    static kCFProxyHostNameKey: &'static NSString;
    static kCFProxyPortNumberKey: &'static NSString;
    static kCFProxyTypeNone: &'static NSString;
    static kCFProxyTypeHTTP: &'static NSString;
    static kCFProxyTypeHTTPS: &'static NSString;
    static kCFProxyTypeSOCKS: &'static NSString;
}

fn string(d: &NSDictionary<NSString, AnyObject>, key: &NSString) -> Option<String> {
    d.objectForKey(key)?
        .downcast::<NSString>()
        .ok()
        .map(|s| s.to_string())
}

fn number(d: &NSDictionary<NSString, AnyObject>, key: &NSString) -> Option<isize> {
    d.objectForKey(key)?
        .downcast::<NSNumber>()
        .ok()
        .map(|n| n.integerValue())
}

/// The proxy to use for `url` as a ureq proxy URI ("http://host:port", "socks5://host:port"),
/// or `None` for a direct connection.
pub(crate) fn system_proxy_for(url: &str) -> Option<String> {
    objc2::rc::autoreleasepool(|_| {
        let nsurl = NSURL::URLWithString(&NSString::from_str(url))?;
        // SAFETY: plain CFNetwork calls; results are +1 (or null) and adopted by Retained.
        let settings = unsafe { Retained::from_raw(CFNetworkCopySystemProxySettings()) }?;
        let list = unsafe { Retained::from_raw(CFNetworkCopyProxiesForURL(&nsurl, &settings)) }?;
        // SAFETY: reading framework-provided constant strings.
        let (k_type, k_host, k_port) =
            unsafe { (kCFProxyTypeKey, kCFProxyHostNameKey, kCFProxyPortNumberKey) };
        for i in 0..list.count() {
            let entry = list.objectAtIndex(i);
            let Some(kind) = entry
                .objectForKey(k_type)
                .and_then(|t| t.downcast::<NSString>().ok())
            else {
                continue;
            };
            // SAFETY: as above.
            let (none, http, https, socks) = unsafe {
                (
                    kCFProxyTypeNone,
                    kCFProxyTypeHTTP,
                    kCFProxyTypeHTTPS,
                    kCFProxyTypeSOCKS,
                )
            };
            if kind.isEqualToString(none) {
                return None;
            }
            let scheme = if kind.isEqualToString(http) || kind.isEqualToString(https) {
                // Both are CONNECT proxies spoken to over plain HTTP.
                "http"
            } else if kind.isEqualToString(socks) {
                "socks5"
            } else {
                // PAC / auto-discovery: not evaluated, try the next entry.
                log::debug!("proxy: skipping automatic proxy configuration for {url}");
                continue;
            };
            let host = string(&entry, k_host)?;
            let port = number(&entry, k_port).filter(|p| *p > 0 && *p < 65536)?;
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host
            };
            return Some(format!("{scheme}://{host}:{port}"));
        }
        None
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_settings_without_crashing() {
        // Whatever this Mac's proxy settings are, reading them must work.
        let p = super::system_proxy_for("https://itunes.apple.com/lookup");
        assert!(
            p.as_deref()
                .is_none_or(|p| p.starts_with("http://") || p.starts_with("socks5://")),
            "{p:?}"
        );
        assert_eq!(super::system_proxy_for("not a url"), None);
    }
}
