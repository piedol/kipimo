//! How numbers read in the panel: sizes, rates, durations, "3 days ago". Units and phrases
//! are translated; decimal and thousands separators follow the region set in System Settings
//! (General › Language & Region), as in other Mac apps.

use std::sync::OnceLock;

use objc2_foundation::NSLocale;

use crate::i18n::{tr, trn};

/// Unit labels, translated once: the language is fixed for the life of the process.
struct Units {
    /// KB, MB, GB, TB, PB.
    size: [String; 5],
    bytes: String,
    min: String,
    h: String,
}

fn units() -> &'static Units {
    static UNITS: OnceLock<Units> = OnceLock::new();
    UNITS.get_or_init(|| Units {
        size: [tr("KB"), tr("MB"), tr("GB"), tr("TB"), tr("PB")],
        bytes: tr("bytes"),
        min: tr("min"),
        h: tr("h"),
    })
}

/// The region's decimal separator: "." or ",".
pub fn decimal_separator() -> String {
    NSLocale::currentLocale().decimalSeparator().to_string()
}

/// `v` with `places` decimals, in the region's notation: "3.1" or "3,1".
pub fn decimal(v: impl Into<f64>, places: usize) -> String {
    let v: f64 = v.into();
    let s = format!("{v:.places$}");
    if places == 0 {
        s
    } else {
        s.replacen('.', &decimal_separator(), 1)
    }
}

/// A count with the region's thousands separator: "1,234", "1.234" or "1 234".
pub fn count(n: u64) -> String {
    let sep = NSLocale::currentLocale().groupingSeparator().to_string();
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push_str(&sep);
        }
        out.push(c);
    }
    out
}

/// Decimal units, as Finder and System Settings show sizes.
pub fn bytes(n: u64) -> String {
    let u = units();
    if n < 1000 {
        return format!("{n} {}", u.bytes);
    }
    let mut v = n as f64 / 1000.0;
    let mut i = 0;
    while v >= 999.5 && i < u.size.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v < 10.0 && i >= 1 {
        format!("{} {}", decimal(v, 1), u.size[i])
    } else {
        format!("{v:.0} {}", u.size[i])
    }
}

/// Memory in binary units, as Activity Monitor shows it (128 GB of RAM, not 137).
pub fn memory(n: u64) -> String {
    let u = units();
    let gib = n as f64 / (1u64 << 30) as f64;
    if gib >= 10.0 {
        format!("{gib:.0} {}", u.size[2])
    } else if gib >= 1.0 {
        format!("{} {}", decimal(gib, 1), u.size[2])
    } else {
        format!("{:.0} {}", n as f64 / (1u64 << 20) as f64, u.size[1])
    }
}

/// Split a size into number and unit for the big value label ("466", "GB").
pub fn bytes_parts(n: u64) -> (String, String) {
    let s = bytes(n);
    match s.split_once(' ') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s, String::new()),
    }
}

/// "1.3 MB/s", "17 KB/s", "0 KB/s".
pub fn rate(bytes_per_sec: f64) -> String {
    let b = bytes_per_sec.max(0.0);
    if b < 1000.0 {
        return format!("{:.0} {}/s", b / 1000.0, units().size[0]);
    }
    let s = bytes(b as u64);
    format!("{s}/s")
}

/// "1 h 20 min", "45 min".
pub fn minutes(m: u32) -> String {
    let u = units();
    match (m / 60, m % 60) {
        (0, m) => format!("{m} {}", u.min),
        (h, 0) => format!("{h} {}", u.h),
        (h, m) => format!("{h} {} {m} {}", u.h, u.min),
    }
}

/// "today", "yesterday", "3 days ago", "2 weeks ago".
pub fn days_ago(secs: u64) -> String {
    let days = secs / 86_400;
    match days {
        0 => tr("today"),
        1 => tr("yesterday"),
        2..=13 => trn(days, "{n} day ago", "{n} days ago"),
        14..=59 => trn(days / 7, "{n} week ago", "{n} weeks ago"),
        _ => trn(days / 30, "{n} month ago", "{n} months ago"),
    }
}

/// Whole percent with the digits kept steady.
pub fn percent(v: f32) -> String {
    format!("{:.0}", v.clamp(0.0, 999.0))
}

/// "L 69% · R 69% · Case 12%".
pub fn parts(parts: &[kipimo_sys::BatteryPart]) -> String {
    use kipimo_sys::PartKind;
    parts
        .iter()
        .map(|p| {
            let label = match p.kind {
                PartKind::Main => String::new(),
                PartKind::Left => format!("{} ", tr("L")),
                PartKind::Right => format!("{} ", tr("R")),
                PartKind::Case => format!("{} ", tr("Case")),
            };
            let bolt = if p.charging { " ⚡︎" } else { "" };
            format!("{label}{}%{bolt}", p.percent)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kipimo_sys::{BatteryPart, PartKind};

    /// The expected text in this Mac's region: "3.1 GB" reads "3,1 GB" where "," is the
    /// decimal separator.
    fn local(s: &str) -> String {
        s.replacen('.', &decimal_separator(), 1)
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(278_528), "279 KB");
        assert_eq!(bytes(3_100_000_000), local("3.1 GB"));
        assert_eq!(bytes(469_348_945_917), "469 GB");
        assert_eq!(bytes(1_995_165_736_960), local("2.0 TB"));
        assert_eq!(bytes(999_600_000), local("1.0 GB"));
        assert_eq!(bytes_parts(469_348_945_917), ("469".into(), "GB".into()));
        assert_eq!(bytes_parts(3_100_000_000), (local("3.1"), "GB".into()));
        assert_eq!(memory(137_438_953_472), "128 GB");
        assert_eq!(memory(2_004_025_344), local("1.9 GB"));
        assert_eq!(memory(300 << 20), "300 MB");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(0.0), "0 KB/s");
        assert_eq!(rate(17_818.0), "18 KB/s");
        assert_eq!(rate(1_323_003.0), local("1.3 MB/s"));
    }

    #[test]
    fn counts() {
        let sep = NSLocale::currentLocale().groupingSeparator().to_string();
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1234), format!("1{sep}234"));
        assert_eq!(count(1_234_567), format!("1{sep}234{sep}567"));
        assert_eq!(decimal(2.4, 0), "2");
        assert_eq!(decimal(1.24, 1), local("1.2"));
    }

    #[test]
    fn durations() {
        assert_eq!(minutes(45), "45 min");
        assert_eq!(minutes(120), "2 h");
        assert_eq!(minutes(80), "1 h 20 min");
        assert_eq!(days_ago(3_600), "today");
        assert_eq!(days_ago(86_400 * 2 + 5), "2 days ago");
        assert_eq!(days_ago(86_400 * 20), "2 weeks ago");
        assert_eq!(days_ago(86_400 * 90), "3 months ago");
    }

    #[test]
    fn battery_parts() {
        let p = [
            BatteryPart {
                kind: PartKind::Left,
                percent: 69,
                charging: false,
            },
            BatteryPart {
                kind: PartKind::Right,
                percent: 70,
                charging: false,
            },
            BatteryPart {
                kind: PartKind::Case,
                percent: 12,
                charging: true,
            },
        ];
        assert_eq!(parts(&p), "L 69% · R 70% · Case 12% ⚡︎");
        assert_eq!(
            parts(&[BatteryPart {
                kind: PartKind::Main,
                percent: 18,
                charging: false
            }]),
            "18%"
        );
    }
}
