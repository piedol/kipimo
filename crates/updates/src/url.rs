//! Minimal URL helpers: enough to resolve redirects and relative feed links.

/// "https", "http" (lower case) when the URL has one of those schemes.
pub fn http_scheme(url: &str) -> Option<&'static str> {
    let (scheme, _) = url.split_once("://")?;
    if scheme.eq_ignore_ascii_case("https") {
        Some("https")
    } else if scheme.eq_ignore_ascii_case("http") {
        Some("http")
    } else {
        None
    }
}

/// "host" or "host:port" of an http(s) URL, lower case, without user info.
pub fn authority(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..end];
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    if auth.is_empty() {
        None
    } else {
        Some(auth.to_ascii_lowercase())
    }
}

/// The host alone (no port, no IPv6 brackets).
pub fn host(url: &str) -> Option<String> {
    let auth = authority(url)?;
    if let Some(rest) = auth.strip_prefix('[') {
        return rest.split(']').next().map(str::to_string);
    }
    Some(auth.split(':').next().unwrap_or(&auth).to_string())
}

/// Resolves `href` against `base` the way a browser would for the cases feeds use:
/// absolute URLs, scheme-relative ("//host/x"), root-relative ("/x") and relative ("x").
pub fn resolve(base: &str, href: &str) -> Option<String> {
    resolve_raw(base, href.trim()).map(|u| escape_invalid(&u))
}

/// Percent-encodes characters that may not appear in a URL (spaces in file names, non-ASCII),
/// leaving everything else, including existing escapes, alone.
fn escape_invalid(u: &str) -> String {
    let mut out = String::with_capacity(u.len());
    for c in u.chars() {
        if c.is_ascii_graphic()
            && !matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}')
        {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn resolve_raw(base: &str, href: &str) -> Option<String> {
    if href.is_empty() {
        return None;
    }
    if http_scheme(href).is_some() {
        return Some(href.to_string());
    }
    if href.contains("://") {
        // Some other scheme (ftp, file, ...): not something to follow.
        return None;
    }
    let scheme = http_scheme(base)?;
    let auth = base.split_once("://")?.1;
    let auth_end = auth.find(['/', '?', '#']).unwrap_or(auth.len());
    let origin = format!("{scheme}://{}", &auth[..auth_end]);
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    let path = &auth[auth_end..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    if href.starts_with('?') {
        return Some(format!("{origin}{path}{href}"));
    }
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let (href_path, query) = match href.find(['?', '#']) {
        Some(i) => (&href[..i], &href[i..]),
        None => (href, ""),
    };
    let trailing_slash =
        href_path.ends_with('/') || href_path.ends_with("/.") || href_path.ends_with("/..");
    for seg in href_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = format!("{origin}/{}", segments.join("/"));
    if trailing_slash && !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(query);
    Some(out)
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
pub fn encode_query(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Joins a base folder URL and a file name: "https://x/a" + "latest-mac.yml" →
/// "https://x/a/latest-mac.yml" (electron-updater treats the URL as a folder).
pub fn join_folder(base: &str, file: &str) -> String {
    let (base, query) = match base.find('?') {
        Some(i) => (&base[..i], &base[i..]),
        None => (base, ""),
    };
    format!("{}/{}{}", base.trim_end_matches('/'), file, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves() {
        let base = "https://example.com/a/b/feed.xml?x=1";
        assert_eq!(
            resolve(base, "https://other.org/z").as_deref(),
            Some("https://other.org/z")
        );
        assert_eq!(
            resolve(base, "//cdn.org/z").as_deref(),
            Some("https://cdn.org/z")
        );
        assert_eq!(
            resolve(base, "/root.zip").as_deref(),
            Some("https://example.com/root.zip")
        );
        assert_eq!(
            resolve(base, "App_2.zip").as_deref(),
            Some("https://example.com/a/b/App_2.zip")
        );
        assert_eq!(
            resolve(base, "../c/d.zip?q=1").as_deref(),
            Some("https://example.com/a/c/d.zip?q=1")
        );
        assert_eq!(
            resolve("https://example.com", "x.zip").as_deref(),
            Some("https://example.com/x.zip")
        );
        assert_eq!(
            resolve(base, "App 1.2 - Build 7-arm64.dmg").as_deref(),
            Some("https://example.com/a/b/App%201.2%20-%20Build%207-arm64.dmg")
        );
        assert_eq!(
            resolve(base, "caf\u{e9}%20x.zip").as_deref(),
            Some("https://example.com/a/b/caf%C3%A9%20x.zip")
        );
        assert_eq!(resolve(base, "ftp://x/y"), None);
        assert_eq!(resolve(base, ""), None);
    }

    #[test]
    fn hosts() {
        assert_eq!(
            authority("https://User@Example.com:8443/x").as_deref(),
            Some("example.com:8443")
        );
        assert_eq!(
            host("https://Example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(host("http://[::1]:80/").as_deref(), Some("::1"));
        assert_eq!(host("http://127.0.0.1:5000").as_deref(), Some("127.0.0.1"));
        assert_eq!(http_scheme("HTTPS://x"), Some("https"));
        assert_eq!(http_scheme("file:///x"), None);
    }

    #[test]
    fn folders_and_queries() {
        assert_eq!(
            join_folder("https://x/a/", "latest-mac.yml"),
            "https://x/a/latest-mac.yml"
        );
        assert_eq!(
            join_folder("https://x/a?t=1", "latest-mac.yml"),
            "https://x/a/latest-mac.yml?t=1"
        );
        assert_eq!(encode_query("com.a b,c"), "com.a%20b%2Cc");
    }
}
