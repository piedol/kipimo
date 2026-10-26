//! A small JSON cache of feed findings, so each feed is read at most once a day.
//!
//! Entries are keyed by feed (not by app) and store what the feed said, not a status, so an
//! app updated since the last check is re-evaluated without another request.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::finding::Finding;

const FORMAT: u32 = 2;
/// Entries older than this are dropped when saving.
const KEEP_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub checked_at: u64,
    /// macOS version the finding was computed for (compatibility depends on it).
    pub macos: String,
    /// What the feed said, or a definite failure (not found, unreadable feed).
    pub outcome: Result<Finding, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Cache {
    format: u32,
    entries: HashMap<String, Entry>,
}

impl Cache {
    /// Loads the cache; a missing, unreadable or old-format file gives an empty cache.
    pub fn load(path: &Path) -> Cache {
        let Ok(bytes) = fs::read(path) else {
            return Cache::default();
        };
        match serde_json::from_slice::<Cache>(&bytes) {
            Ok(c) if c.format == FORMAT => c,
            _ => Cache::default(),
        }
    }

    pub fn fresh(&self, key: &str, now: u64, max_age_secs: u64, macos: &str) -> Option<&Entry> {
        self.entries.get(key).filter(|e| {
            e.macos == macos && e.checked_at <= now && now - e.checked_at < max_age_secs
        })
    }

    pub fn put(&mut self, key: String, entry: Entry) {
        self.entries.insert(key, entry);
    }

    /// Writes atomically (temporary file, then rename), creating the folder if needed.
    pub fn save(&mut self, path: &Path, now: u64) -> std::io::Result<()> {
        self.format = FORMAT;
        self.entries
            .retain(|_, e| now.saturating_sub(e.checked_at) < KEEP_SECS);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Candidate;
    use crate::types::Release;

    fn entry(checked_at: u64) -> Entry {
        let release = Release {
            version: Some("1.0".into()),
            ..Release::default()
        };
        let finding = Finding {
            latest: Some(Candidate::new(release)),
            newest_incompatible: None,
        };
        Entry {
            checked_at,
            macos: "26.6.2".into(),
            outcome: Ok(finding),
        }
    }

    #[test]
    fn round_trip_freshness_and_pruning() {
        let dir = std::env::temp_dir().join(format!("kipimo-updates-cache-{}", std::process::id()));
        let path = dir.join("sub/updates.json");
        let now = 10_000_000;
        let mut c = Cache::default();
        c.put("fresh".into(), entry(now - 60));
        c.put("old".into(), entry(now - 2 * 24 * 3600));
        c.put("ancient".into(), entry(now - 40 * 24 * 3600));
        c.put(
            "failed".into(),
            Entry {
                outcome: Err("not found".into()),
                ..entry(now - 60)
            },
        );
        c.save(&path, now).unwrap();

        let c = Cache::load(&path);
        assert_eq!(c.entries.len(), 3, "the 40-day-old entry is pruned");
        let day = 24 * 3600;
        assert!(c.fresh("fresh", now, day, "26.6.2").is_some());
        assert!(
            c.fresh("fresh", now, 30, "26.6.2").is_none(),
            "older than max age"
        );
        assert!(
            c.fresh("fresh", now, day, "27.0").is_none(),
            "another macOS version"
        );
        assert!(c.fresh("old", now, day, "26.6.2").is_none());
        assert_eq!(
            c.fresh("failed", now, day, "26.6.2").unwrap().outcome,
            Err("not found".into())
        );
        // A clock that went backwards doesn't make entries fresh forever.
        assert!(c.fresh("fresh", now - 3600, day, "26.6.2").is_none());

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(Cache::load(&path).entries.is_empty());
        std::fs::write(&path, br#"{"format":1,"entries":{}}"#).unwrap();
        assert!(Cache::load(&path).entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
