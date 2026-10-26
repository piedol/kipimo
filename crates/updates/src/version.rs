//! Version comparison.
//!
//! [`compare_sparkle`] reproduces Sparkle's `SUStandardVersionComparator`, so Kipimo agrees
//! with what a Sparkle app's own updater would offer. [`compare_lenient`] is the fallback for
//! display versions (App Store, electron-updater) where Sparkle's rules give wrong answers:
//! it treats "1.2" and "1.2.0" as equal and ranks "1.0-beta" below "1.0".

use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Number,
    Text,
    Separator,
}

/// Sparkle classifies characters with `NSCharacterSet`: decimal digits are numbers, `.`,
/// whitespace and punctuation are separators, and everything else (letters, but also symbols
/// such as `+` or `~`) is text.
fn kind(c: char) -> Kind {
    if c.is_ascii_digit() {
        Kind::Number
    } else if c.is_whitespace() || is_punctuation(c) {
        Kind::Separator
    } else {
        Kind::Text
    }
}

/// Unicode punctuation (general category P*), which is what `punctuationCharacterSet` holds.
/// ASCII symbols such as `+ $ < = > ^ | ~` and the backtick are category S*, so not included.
fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '!' | '"'
            | '#'
            | '%'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | ','
            | '-'
            | '.'
            | '/'
            | ':'
            | ';'
            | '?'
            | '@'
            | '['
            | '\\'
            | ']'
            | '_'
            | '{'
            | '}'
            | '\u{a1}'
            | '\u{a7}'
            | '\u{ab}'
            | '\u{b6}'
            | '\u{b7}'
            | '\u{bb}'
            | '\u{bf}'
            | '\u{2010}'..='\u{2027}'
            | '\u{2030}'..='\u{2043}'
            | '\u{2045}'..='\u{2051}'
            | '\u{2053}'..='\u{205e}'
            | '\u{3001}'..='\u{3003}'
            | '\u{3008}'..='\u{3011}'
    )
}

/// Splits like Sparkle: runs of digits, runs of text, and every separator on its own.
fn split(v: &str) -> Vec<(Kind, &str)> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut prev: Option<Kind> = None;
    for (i, c) in v.char_indices() {
        let k = kind(c);
        if let Some(p) = prev
            && (p != k || p == Kind::Separator)
        {
            parts.push((p, &v[start..i]));
            start = i;
        }
        prev = Some(k);
    }
    if let Some(p) = prev {
        parts.push((p, &v[start..]));
    }
    parts
}

/// Compares two digit strings by value without overflowing (Sparkle uses `longLongValue`,
/// which saturates; for every realistic version the result is the same).
fn compare_digits(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Sparkle's `SUStandardVersionComparator`: `Greater` means `a` is newer than `b`.
///
/// "1.10" > "1.9", "1.0" > "1.0b1" (text after a shared prefix marks a pre-release),
/// "1.0.1" > "1.0", and a number beats text or a separator in the same position.
pub fn compare_sparkle(a: &str, b: &str) -> Ordering {
    let pa = split(a);
    let pb = split(b);
    for (&(ka, sa), &(kb, sb)) in pa.iter().zip(pb.iter()) {
        if ka == kb {
            let ord = match ka {
                Kind::Number => compare_digits(sa, sb),
                Kind::Text => sa.cmp(sb),
                Kind::Separator => Ordering::Equal,
            };
            if ord != Ordering::Equal {
                return ord;
            }
        } else if ka != Kind::Text && kb == Kind::Text {
            return Ordering::Greater;
        } else if ka == Kind::Text && kb != Kind::Text {
            return Ordering::Less;
        } else if ka == Kind::Number {
            // A number against a separator: the separator is invalid there.
            return Ordering::Greater;
        } else {
            return Ordering::Less;
        }
    }
    if pa.len() == pb.len() {
        return Ordering::Equal;
    }
    // One runs longer: the extra part decides. Text means a pre-release (shorter wins);
    // a number or separator means a further component (longer wins).
    let (extra, longer_is_a) = if pa.len() > pb.len() {
        (pa[pb.len()], true)
    } else {
        (pb[pa.len()], false)
    };
    let longer_wins = extra.0 != Kind::Text;
    match (longer_wins, longer_is_a) {
        (true, true) | (false, false) => Ordering::Greater,
        _ => Ordering::Less,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    Number(&'a str),
    Text(String),
}

/// Pre-release tags in the order they usually progress. Unknown tags sort among themselves
/// alphabetically, after the known ones.
fn tag_rank(t: &str) -> Option<u8> {
    Some(match t {
        "dev" | "snapshot" | "nightly" | "canary" => 0,
        "a" | "alpha" => 1,
        "b" | "beta" => 2,
        "pre" | "preview" => 3,
        "c" | "rc" => 4,
        _ => return None,
    })
}

fn compare_text(a: &str, b: &str) -> Ordering {
    match (tag_rank(a), tag_rank(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

fn tokens(v: &str) -> Vec<Token<'_>> {
    split(v)
        .into_iter()
        .filter_map(|(k, s)| match k {
            Kind::Number => Some(Token::Number(s)),
            Kind::Text => Some(Token::Text(s.to_ascii_lowercase())),
            Kind::Separator => None,
        })
        .collect()
}

fn compare_tokens(a: &[Token<'_>], b: &[Token<'_>]) -> Ordering {
    for (ta, tb) in a.iter().zip(b.iter()) {
        let ord = match (ta, tb) {
            (Token::Number(x), Token::Number(y)) => compare_digits(x, y),
            (Token::Text(x), Token::Text(y)) => compare_text(x, y),
            // A further release component beats a pre-release tag.
            (Token::Number(_), Token::Text(_)) => Ordering::Greater,
            (Token::Text(_), Token::Number(_)) => Ordering::Less,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    let (rest, sign) = if a.len() > b.len() {
        (&a[b.len()..], Ordering::Greater)
    } else if b.len() > a.len() {
        (&b[a.len()..], Ordering::Less)
    } else {
        return Ordering::Equal;
    };
    match rest.first() {
        // "1.0-beta" vs "1.0": the longer one is a pre-release of the shorter.
        Some(Token::Text(_)) => sign.reverse(),
        // "1.2.0" vs "1.2": trailing zeros don't make a version newer.
        _ if rest
            .iter()
            .all(|t| matches!(t, Token::Number(n) if n.trim_start_matches('0').is_empty())) =>
        {
            Ordering::Equal
        }
        _ => sign,
    }
}

/// Splits "7.1.9 (88375)" into the version proper and a trailing build note.
fn main_and_note(v: &str) -> (&str, &str) {
    let v = v.trim();
    let v = v
        .strip_prefix(['v', 'V'])
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(v);
    // Semver build metadata never affects precedence.
    let v = v.split('+').next().unwrap_or(v);
    match v.find(|c: char| c.is_whitespace() || c == '(') {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    }
}

/// A forgiving comparison for display versions: ignores a leading "v", semver build metadata
/// ("+abc") and trailing zero components, ranks pre-release tags (dev < alpha < beta <
/// preview < rc < release), and only uses a trailing "(build)" note to break ties.
pub fn compare_lenient(a: &str, b: &str) -> Ordering {
    let (ma, na) = main_and_note(a);
    let (mb, nb) = main_and_note(b);
    compare_tokens(&tokens(ma), &tokens(mb)).then_with(|| {
        if na.is_empty() || nb.is_empty() {
            Ordering::Equal
        } else {
            compare_tokens(&tokens(na), &tokens(nb))
        }
    })
}

/// True when the version contains at least one digit, so comparing it means something.
pub(crate) fn is_comparable(v: &str) -> bool {
    v.chars().any(|c| c.is_ascii_digit())
}

/// Whether `candidate` is a newer Sparkle build than `installed`. Sparkle's rules decide,
/// except that "1.2.0" is not offered over "1.2" (Sparkle would; it's never a real update).
pub fn sparkle_is_newer(candidate: &str, installed: &str) -> bool {
    compare_sparkle(candidate, installed) == Ordering::Greater
        && compare_lenient(candidate, installed) != Ordering::Equal
}

/// Whether a display version is newer, by the lenient rules.
pub fn lenient_is_newer(candidate: &str, installed: &str) -> bool {
    compare_lenient(candidate, installed) == Ordering::Greater
}

/// macOS version for a Darwin kernel version, for display ("22.1.0" → "13.0").
/// Darwin 20–24 are macOS 11–15 and Darwin 25 is macOS 26. For macOS 11–13 the Darwin minor
/// version runs one ahead (Darwin 22.1 is macOS 13.0); from macOS 14 on they match.
pub fn macos_for_darwin(darwin: &str) -> Option<String> {
    let mut it = darwin.trim().split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some(match major {
        4..=19 => format!("10.{}", major - 4),
        20..=22 => format!("{}.{}", major - 9, minor.saturating_sub(1)),
        23 | 24 => format!("{}.{minor}", major - 9),
        _ if major >= 25 => format!("{}.{minor}", major + 1),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    #[test]
    fn sparkle_table() {
        // (a, b, expected compare(a, b))
        let table = [
            ("1.0", "1.0", Equal),
            ("1.10", "1.9", Greater),
            ("1.9", "1.10", Less),
            ("1.0.1", "1.0", Greater),
            ("1.0", "1.0.1", Less),
            ("1.0", "1.0b1", Greater),
            ("1.0b1", "1.0", Less),
            ("1.0b2", "1.0b1", Greater),
            ("1.0b10", "1.0b9", Greater),
            ("1.0a1", "1.0b1", Less),
            ("1.0rc1", "1.0b5", Greater),
            ("1.0b1", "1.0.1", Less),
            ("2.0", "1.99.99", Greater),
            ("123", "122", Greater),
            ("4120", "999", Greater),
            ("0010", "9", Greater),
            ("1.0 (100)", "1.0 (99)", Greater),
            ("3.6.11", "3.7.6", Less),
            ("191", "190", Greater),
            ("1.0", "1.0.0", Less), // Sparkle's own (strict) answer
            (
                "99999999999999999999999",
                "99999999999999999999998",
                Greater,
            ),
            ("", "", Equal),
            ("1", "", Greater),
            ("", "1", Less),
            ("1.0-beta", "1.0", Greater), // a separator part counts as "more", as in Sparkle
        ];
        for (a, b, want) in table {
            assert_eq!(compare_sparkle(a, b), want, "compare_sparkle({a:?}, {b:?})");
            assert_eq!(
                compare_sparkle(b, a),
                want.reverse(),
                "compare_sparkle({b:?}, {a:?})"
            );
        }
    }

    #[test]
    fn sparkle_split_matches_sparkle() {
        let parts: Vec<_> = split("1.0b2 (3)").into_iter().map(|p| p.1).collect();
        assert_eq!(parts, ["1", ".", "0", "b", "2", " ", "(", "3", ")"]);
        // '+' is a symbol, so text, not a separator.
        let parts: Vec<_> = split("1+2").into_iter().map(|p| p.0).collect();
        assert_eq!(parts, [Kind::Number, Kind::Text, Kind::Number]);
    }

    #[test]
    fn lenient_table() {
        let table = [
            ("1.10", "1.9", Greater),
            ("1.2", "1.2.0", Equal),
            ("1.2.0.0", "1.2", Equal),
            ("16.114.1", "16.114", Greater),
            ("1.0.0-beta.2", "1.0.0", Less),
            ("1.0.0-beta.2", "1.0.0-beta.10", Less),
            ("1.0.0-alpha", "1.0.0-beta", Less),
            ("1.0.0-rc.1", "1.0.0-beta.9", Greater),
            ("1.0b1", "1.0", Less),
            ("1.0.1", "1.0-beta", Greater),
            ("v2.3.4", "2.3.4", Equal),
            ("2.3.4+build.7", "2.3.4", Equal),
            ("7.1.10 (12000)", "7.1.9 (88375)", Greater),
            ("6.1.1 (400)", "6.1 (500)", Greater),
            ("6.1 (501)", "6.1 (500)", Greater),
            ("6.1", "6.1 (500)", Equal),
            ("8.32.0", "8.32.0", Equal),
            ("2026.10", "2026.9", Greater),
            ("0.13.6-local4", "0.13.6", Less),
            ("1.0.0-beta", "1.0.0-Beta", Equal),
        ];
        for (a, b, want) in table {
            assert_eq!(compare_lenient(a, b), want, "compare_lenient({a:?}, {b:?})");
            assert_eq!(
                compare_lenient(b, a),
                want.reverse(),
                "compare_lenient({b:?}, {a:?})"
            );
        }
    }

    #[test]
    fn sparkle_newer_guards_trailing_zero() {
        assert!(!sparkle_is_newer("1.2.0", "1.2"));
        assert!(sparkle_is_newer("1.2.1", "1.2"));
        assert!(sparkle_is_newer("1.10", "1.9"));
        assert!(!sparkle_is_newer("1.0b1", "1.0"));
        assert!(!sparkle_is_newer("1.0", "1.0"));
    }

    #[test]
    fn darwin_to_macos() {
        assert_eq!(macos_for_darwin("22.1.0").as_deref(), Some("13.0"));
        assert_eq!(macos_for_darwin("23.0.0").as_deref(), Some("14.0"));
        assert_eq!(macos_for_darwin("24.4.0").as_deref(), Some("15.4"));
        assert_eq!(macos_for_darwin("25.6.0").as_deref(), Some("26.6"));
        assert_eq!(macos_for_darwin("19.6.0").as_deref(), Some("10.15"));
        assert_eq!(macos_for_darwin("junk"), None);
    }
}
