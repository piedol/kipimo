//! How numbers read in the panel: sizes, rates, durations, "3 days ago".

/// Decimal units, as Finder and System Settings show sizes.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1000.0;
    let mut i = 0;
    while v >= 999.5 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v < 10.0 && i >= 1 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.0} {}", UNITS[i])
    }
}

/// Memory in binary units, as Activity Monitor shows it (128 GB of RAM, not 137).
pub fn memory(n: u64) -> String {
    let gib = n as f64 / (1u64 << 30) as f64;
    if gib >= 10.0 {
        format!("{gib:.0} GB")
    } else if gib >= 1.0 {
        format!("{gib:.1} GB")
    } else {
        format!("{:.0} MB", n as f64 / (1u64 << 20) as f64)
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
        return format!("{:.0} KB/s", b / 1000.0);
    }
    let s = bytes(b as u64);
    format!("{s}/s")
}

/// "1 h 20 min", "45 min".
pub fn minutes(m: u32) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "today", "yesterday", "3 days ago", "2 weeks ago".
pub fn days_ago(secs: u64) -> String {
    let days = secs / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..=13 => format!("{days} days ago"),
        14..=59 => format!("{} weeks ago", days / 7),
        _ => format!("{} months ago", days / 30),
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
                PartKind::Main => "",
                PartKind::Left => "L ",
                PartKind::Right => "R ",
                PartKind::Case => "Case ",
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

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(278_528), "279 KB");
        assert_eq!(bytes(3_100_000_000), "3.1 GB");
        assert_eq!(bytes(469_348_945_917), "469 GB");
        assert_eq!(bytes(1_995_165_736_960), "2.0 TB");
        assert_eq!(bytes(999_600_000), "1.0 GB");
        assert_eq!(bytes_parts(469_348_945_917), ("469".into(), "GB".into()));
        assert_eq!(memory(137_438_953_472), "128 GB");
        assert_eq!(memory(2_004_025_344), "1.9 GB");
        assert_eq!(memory(300 << 20), "300 MB");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(0.0), "0 KB/s");
        assert_eq!(rate(17_818.0), "18 KB/s");
        assert_eq!(rate(1_323_003.0), "1.3 MB/s");
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
