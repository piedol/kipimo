//! A small YAML reader for electron-builder's `app-update.yml` and `latest-mac.yml`.
//!
//! It covers the subset those files use: nested maps, lists (including lists of maps),
//! plain, quoted and block scalars, and comments. Anchors, flow collections and tags are
//! not supported; a flow collection is kept as a plain string.

#[derive(Clone, Debug, PartialEq)]
pub enum Yaml {
    Null,
    Str(String),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// A scalar value, trimmed; `None` for missing, null, empty or non-scalar values.
    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Yaml::Str(s) if !s.trim().is_empty() => Some(s.trim()),
            _ => None,
        }
    }

    pub fn list(&self, key: &str) -> &[Yaml] {
        match self.get(key) {
            Some(Yaml::List(items)) => items,
            _ => &[],
        }
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    indent: usize,
    /// The line without its indentation.
    text: &'a str,
    /// Blank or comment-only: skipped between structural entries.
    skip: bool,
}

pub fn parse(src: &str) -> Yaml {
    let mut lines: Vec<Line<'_>> = src
        .lines()
        .filter(|l| !l.starts_with("---") && !l.starts_with("..."))
        .map(|raw| {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let text = raw.trim_start_matches(' ');
            let indent = raw.len() - text.len();
            let skip = text.trim().is_empty() || text.starts_with('#');
            Line { indent, text, skip }
        })
        .collect();
    let mut i = 0;
    skip_blank(&lines, &mut i);
    if i >= lines.len() {
        return Yaml::Null;
    }
    let indent = lines[i].indent;
    parse_block(&mut lines, &mut i, indent)
}

fn skip_blank(lines: &[Line<'_>], i: &mut usize) {
    while *i < lines.len() && lines[*i].skip {
        *i += 1;
    }
}

fn is_list_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn parse_block<'a>(lines: &mut Vec<Line<'a>>, i: &mut usize, indent: usize) -> Yaml {
    skip_blank(lines, i);
    if *i >= lines.len() {
        return Yaml::Null;
    }
    if is_list_item(lines[*i].text) {
        parse_list(lines, i, indent)
    } else {
        parse_map(lines, i, indent)
    }
}

/// Finds the `key: value` split, ignoring colons inside quoted keys and URLs ("https://").
fn split_key(text: &str) -> Option<(String, &str)> {
    if let Some(q) = text.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let end = text[1..].find(q)? + 1;
        let rest = text[end + 1..].trim_start();
        let value = rest.strip_prefix(':')?;
        if !(value.is_empty() || value.starts_with(' ')) {
            return None;
        }
        return Some((text[1..end].to_string(), value.trim()));
    }
    let mut search = 0;
    while let Some(pos) = text[search..].find(':') {
        let at = search + pos;
        let after = &text[at + 1..];
        if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
            let key = text[..at].trim();
            if key.is_empty() {
                return None;
            }
            return Some((key.to_string(), after.trim()));
        }
        search = at + 1;
    }
    None
}

fn parse_map<'a>(lines: &mut Vec<Line<'a>>, i: &mut usize, indent: usize) -> Yaml {
    let mut entries = Vec::new();
    loop {
        skip_blank(lines, i);
        if *i >= lines.len() {
            break;
        }
        let line = lines[*i];
        if line.indent != indent || is_list_item(line.text) {
            break;
        }
        let Some((key, value)) = split_key(line.text) else {
            // Not a mapping line: stop rather than guess.
            break;
        };
        *i += 1;
        let value = if value.is_empty() || value.starts_with('#') {
            let mut j = *i;
            skip_blank(lines, &mut j);
            match lines.get(j) {
                Some(next) if next.indent > indent => parse_block(lines, i, next.indent),
                // Lists may sit at the same indentation as their key.
                Some(next) if next.indent == indent && is_list_item(next.text) => {
                    parse_list(lines, i, indent)
                }
                _ => Yaml::Null,
            }
        } else {
            scalar(value, lines, i, indent)
        };
        entries.push((key, value));
    }
    Yaml::Map(entries)
}

fn parse_list<'a>(lines: &mut Vec<Line<'a>>, i: &mut usize, indent: usize) -> Yaml {
    let mut items = Vec::new();
    loop {
        skip_blank(lines, i);
        if *i >= lines.len() {
            break;
        }
        let line = lines[*i];
        if line.indent != indent || !is_list_item(line.text) {
            break;
        }
        let rest = line.text[1..].trim_start_matches(' ');
        if rest.is_empty() || rest.starts_with('#') {
            *i += 1;
            let mut j = *i;
            skip_blank(lines, &mut j);
            match lines.get(j) {
                Some(next) if next.indent > indent => {
                    items.push(parse_block(lines, i, next.indent))
                }
                _ => items.push(Yaml::Null),
            }
        } else if is_list_item(rest) || split_key(rest).is_some() {
            // "- key: value": a map (or nested list) whose first line shares the dash. Re-read
            // this line as if the dash were indentation.
            let offset = line.text.len() - rest.len();
            lines[*i] = Line {
                indent: indent + offset,
                text: rest,
                skip: false,
            };
            items.push(parse_block(lines, i, indent + offset));
        } else {
            *i += 1;
            items.push(scalar(rest, lines, i, indent));
        }
    }
    Yaml::List(items)
}

/// A scalar value starting on the current line; may consume continuation lines.
fn scalar<'a>(value: &'a str, lines: &mut [Line<'a>], i: &mut usize, indent: usize) -> Yaml {
    if value.starts_with('|') || value.starts_with('>') {
        return Yaml::Str(block_scalar(value.starts_with('>'), lines, i, indent));
    }
    if let Some(q) = value.chars().next().filter(|c| *c == '"' || *c == '\'') {
        // Gather continuation lines until the closing quote (multi-line quoted scalars).
        let mut text = value.to_string();
        while closing_quote(&text, q).is_none()
            && *i < lines.len()
            && (lines[*i].indent > indent || lines[*i].skip)
        {
            text.push(' ');
            text.push_str(lines[*i].text.trim());
            *i += 1;
        }
        return Yaml::Str(unquote(&text, q));
    }
    let plain = match value.find(" #") {
        Some(pos) => &value[..pos],
        None => value,
    }
    .trim();
    if plain == "~" || plain == "null" {
        Yaml::Null
    } else {
        Yaml::Str(plain.to_string())
    }
}

fn block_scalar(folded: bool, lines: &[Line<'_>], i: &mut usize, indent: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut content_indent = None;
    while *i < lines.len() {
        let line = lines[*i];
        let blank = line.text.trim().is_empty();
        if !blank && line.indent <= indent {
            break;
        }
        if blank {
            out.push(String::new());
        } else {
            // Indentation beyond the block's own is part of the content.
            let base = *content_indent.get_or_insert(line.indent);
            let extra = line.indent.saturating_sub(base);
            out.push(format!("{}{}", " ".repeat(extra), line.text));
        }
        *i += 1;
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if folded {
        out.join(" ")
    } else {
        out.join("\n")
    }
}

/// Index of the closing quote, honouring `''` (single) and `\"` (double) escapes.
fn closing_quote(text: &str, q: char) -> Option<usize> {
    let bytes = text.as_bytes();
    let q = q as u8;
    let mut k = 1;
    while k < bytes.len() {
        if q == b'"' && bytes[k] == b'\\' {
            k += 2;
            continue;
        }
        if bytes[k] == q {
            if q == b'\'' && bytes.get(k + 1) == Some(&b'\'') {
                k += 2;
                continue;
            }
            return Some(k);
        }
        k += 1;
    }
    None
}

fn unquote(text: &str, q: char) -> String {
    let end = closing_quote(text, q).unwrap_or(text.len());
    let inner = &text[1..end.max(1)];
    if q == '\'' {
        return inner.replace("''", "'");
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_update_yml() {
        let y = parse(
            "owner: upwork-corp\nrepo: some-repo\nprovider: github\nupdaterCacheDirName: upwork-updater\n",
        );
        assert_eq!(y.str("provider"), Some("github"));
        assert_eq!(y.str("owner"), Some("upwork-corp"));
        assert_eq!(y.str("repo"), Some("some-repo"));
        let y =
            parse("channel: latest\nprovider: generic\nurl: https://download.example.com/abc\n");
        assert_eq!(y.str("url"), Some("https://download.example.com/abc"));
    }

    #[test]
    fn latest_mac_yml() {
        let src = "version: 8.32.0
files:
  - url: signal-desktop-mac-x64-8.32.0.zip
    sha512: Ujd==
    size: 160414520
  - url: signal-desktop-mac-arm64-8.32.0.zip
    size: 153550707
path: signal-desktop-mac-x64-8.32.0.zip
vendor:
  minOSVersion: 22.1.0
releaseDate: '2026-10-20T17:43:00.437Z'
releaseNotes: |
  ## Fixes
  - one: two

  - three
minimumSystemVersion: \"23.0.0\" # trailing comment
";
        let y = parse(src);
        assert_eq!(y.str("version"), Some("8.32.0"));
        let files = y.list("files");
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].str("url"),
            Some("signal-desktop-mac-arm64-8.32.0.zip")
        );
        assert_eq!(files[0].str("size"), Some("160414520"));
        assert_eq!(
            y.get("vendor").and_then(|v| v.str("minOSVersion")),
            Some("22.1.0")
        );
        assert_eq!(y.str("releaseDate"), Some("2026-10-20T17:43:00.437Z"));
        assert_eq!(
            y.str("releaseNotes"),
            Some("## Fixes\n- one: two\n\n- three")
        );
        assert_eq!(y.str("minimumSystemVersion"), Some("23.0.0"));
    }

    #[test]
    fn lists_at_key_indent_and_quotes() {
        let src = "files:\n- url: 'it''s.zip'\n  size: 1\n- 'quoted key': v\n- 'just text'\n- url: \"a\\\"b.dmg\"\nname: x # c\nempty:\nmulti: 'one\n  two'\n";
        let y = parse(src);
        let files = y.list("files");
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].str("url"), Some("it's.zip"));
        assert_eq!(files[1].str("quoted key"), Some("v"));
        assert_eq!(files[2], Yaml::Str("just text".into()));
        assert_eq!(files[3].str("url"), Some("a\"b.dmg"));
        assert_eq!(y.str("name"), Some("x"));
        assert_eq!(y.get("empty"), Some(&Yaml::Null));
        assert_eq!(y.str("multi"), Some("one two"));
    }

    #[test]
    fn garbage_does_not_panic() {
        for src in [
            "",
            "\n\n",
            "- a\n- b",
            ": :",
            "a: [1, 2]",
            "key",
            "  - x: y\n -",
            "\"unterminated: x",
        ] {
            let _ = parse(src);
        }
        assert_eq!(
            parse("- a\n- b"),
            Yaml::List(vec![Yaml::Str("a".into()), Yaml::Str("b".into())])
        );
    }
}
