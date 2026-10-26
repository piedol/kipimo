//! Background Task Management (BTM): the records behind System Settings > General >
//! Login Items & Extensions ("Open at Login" and "Allow in the Background").
//!
//! Source, most native first:
//! 1. The on-disk database `/private/var/db/com.apple.backgroundtaskmanagement/
//!    BackgroundItems-v<N>.btm`. It is world-readable (no root needed) and is an
//!    `NSKeyedArchiver` archive of private classes (`Storage`, `ItemRecord`), so it is decoded
//!    here by walking the archive's object table by key name. That keeps working across the
//!    format's version bumps as long as the field names stay.
//! 2. If the database can't be read or decoded, `/usr/bin/sfltool dumpbtm` (a macOS built-in,
//!    the only other way to read these records without root; Apple ships no public API to
//!    enumerate other apps' login items). Its text output is parsed line by line.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use plist::{Dictionary, Value};

pub(crate) const DEFAULT_DIR: &str = "/private/var/db/com.apple.backgroundtaskmanagement";

// Item types (sfltool prints these names).
pub(crate) const TYPE_USER_ITEM: u64 = 0x1;
pub(crate) const TYPE_APP: u64 = 0x2;
pub(crate) const TYPE_LOGIN_ITEM: u64 = 0x4;
pub(crate) const TYPE_AGENT: u64 = 0x8;
pub(crate) const TYPE_DAEMON: u64 = 0x10;
// 0x20 developer (a grouping record), 0x40 Spotlight importer, 0x80 dock tile plug-in,
// 0x800 Quick Look extension: not startup items.
pub(crate) const TYPE_LEGACY: u64 = 0x10000;
// Disposition bits: "[enabled, allowed, notified]".
pub(crate) const DISP_ENABLED: u64 = 0x1;
pub(crate) const DISP_ALLOWED: u64 = 0x2;

/// One BTM item record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Record {
    /// `<type>.<label or bundle id>` (for example `8.com.google.GoogleUpdater.wake`), or the
    /// developer name for developer group records.
    pub identifier: String,
    pub name: Option<String>,
    pub developer_name: Option<String>,
    pub team_id: Option<String>,
    pub item_type: u64,
    pub disposition: u64,
    pub flags: u64,
    /// As stored: an absolute `file://` URL, or a path relative to the container app
    /// (`Contents/Library/LoginItems/Helper.app`).
    pub url: Option<String>,
    pub executable_path: Option<String>,
    pub bundle_id: Option<String>,
    /// Identifier of the parent record (an app or a developer group).
    pub container: Option<String>,
    pub associated_bundle_ids: Vec<String>,
    pub program_arguments: Vec<String>,
}

impl Record {
    pub fn is_legacy(&self) -> bool {
        self.item_type & TYPE_LEGACY != 0
    }

    pub fn base_type(&self) -> u64 {
        self.item_type & !TYPE_LEGACY
    }

    pub fn enabled(&self) -> bool {
        self.disposition & DISP_ENABLED != 0
    }

    pub fn allowed(&self) -> bool {
        self.disposition & DISP_ALLOWED != 0
    }

    /// The label or bundle id after the `<type>.` prefix.
    pub fn short_identifier(&self) -> &str {
        match self.identifier.split_once('.') {
            Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) => rest,
            _ => &self.identifier,
        }
    }

    /// The record's file URL as an absolute path, when it is absolute.
    pub fn absolute_path(&self) -> Option<PathBuf> {
        let url = self.url.as_deref()?;
        let rest = url.strip_prefix("file://")?;
        if !rest.starts_with('/') {
            return None;
        }
        let decoded = percent_decode(rest);
        let trimmed = decoded.trim_end_matches('/');
        Some(PathBuf::from(if trimmed.is_empty() {
            "/"
        } else {
            trimmed
        }))
    }

    /// The URL as a path relative to the container app, when it is relative.
    pub fn relative_path(&self) -> Option<PathBuf> {
        let url = self.url.as_deref()?;
        if url.starts_with("file://") || url.starts_with('/') || url.is_empty() {
            return None;
        }
        Some(PathBuf::from(percent_decode(url).trim_end_matches('/')))
    }
}

/// All records of one user (keyed by the user's UUID).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UserRecords {
    pub user_uuid: String,
    pub records: Vec<Record>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Database {
    pub users: Vec<UserRecords>,
}

impl Database {
    pub fn user(&self, uuid: &str) -> Option<&UserRecords> {
        self.users
            .iter()
            .find(|u| u.user_uuid.eq_ignore_ascii_case(uuid))
    }

    /// Finds a record by identifier, preferring the given user's records.
    pub fn find(&self, identifier: &str, prefer_user: Option<&str>) -> Option<&Record> {
        let preferred = prefer_user
            .and_then(|u| self.user(u))
            .and_then(|u| u.records.iter().find(|r| r.identifier == identifier));
        preferred.or_else(|| {
            self.users
                .iter()
                .flat_map(|u| u.records.iter())
                .find(|r| r.identifier == identifier)
        })
    }

    /// Legacy agent/daemon records keyed by their plist path.
    pub fn legacy_by_plist(&self, prefer_user: Option<&str>) -> HashMap<PathBuf, Record> {
        let mut map = HashMap::new();
        // Insert other users first so the preferred user's records win.
        let mut users: Vec<&UserRecords> = self.users.iter().collect();
        users.sort_by_key(|u| prefer_user.is_some_and(|p| u.user_uuid.eq_ignore_ascii_case(p)));
        for user in users {
            for record in user.records.iter().filter(|r| r.is_legacy()) {
                if let Some(path) = record.absolute_path() {
                    map.insert(path, record.clone());
                }
            }
        }
        map
    }

    /// The absolute path of a record, resolving container-relative URLs through the parent.
    pub fn resolve_path(&self, record: &Record, prefer_user: Option<&str>) -> Option<PathBuf> {
        if let Some(path) = record.absolute_path() {
            return Some(path);
        }
        let relative = record.relative_path()?;
        let parent = self.find(record.container.as_deref()?, prefer_user)?;
        let base = self.resolve_path_depth(parent, prefer_user, 4)?;
        Some(base.join(relative))
    }

    fn resolve_path_depth(
        &self,
        record: &Record,
        prefer_user: Option<&str>,
        depth: u8,
    ) -> Option<PathBuf> {
        if let Some(path) = record.absolute_path() {
            return Some(path);
        }
        if depth == 0 {
            return None;
        }
        let relative = record.relative_path()?;
        let parent = self.find(record.container.as_deref()?, prefer_user)?;
        Some(
            self.resolve_path_depth(parent, prefer_user, depth - 1)?
                .join(relative),
        )
    }
}

/// The newest `BackgroundItems-v<N>.btm` in `dir`.
pub(crate) fn database_path(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let version: u32 = name
                .strip_prefix("BackgroundItems-v")?
                .strip_suffix(".btm")?
                .parse()
                .ok()?;
            Some((version, e.path()))
        })
        .max_by_key(|(v, _)| *v)
        .map(|(_, p)| p)
}

// ---------------------------------------------------------------------------------------------
// NSKeyedArchiver decoding
// ---------------------------------------------------------------------------------------------

struct Archive<'a> {
    objects: &'a [Value],
}

impl<'a> Archive<'a> {
    /// Follows a UID reference into the object table (other values are returned as is).
    fn deref(&self, value: &'a Value) -> &'a Value {
        match value {
            Value::Uid(uid) => self
                .objects
                .get(uid.get() as usize)
                .unwrap_or(&Value::Boolean(false)),
            other => other,
        }
    }

    fn object(&self, value: &'a Value) -> Option<&'a Dictionary> {
        self.deref(value).as_dictionary()
    }

    fn class_name(&self, object: &'a Dictionary) -> Option<&'a str> {
        let class = self.object(object.get("$class")?)?;
        class.get("$classname")?.as_string()
    }

    fn string(&self, value: &'a Value) -> Option<String> {
        match self.deref(value) {
            Value::String(s) if s == "$null" => None,
            Value::String(s) => Some(s.clone()),
            Value::Dictionary(d) => {
                // NSString / NSMutableString objects.
                d.get("NS.string")
                    .and_then(Value::as_string)
                    .map(str::to_owned)
            }
            _ => None,
        }
    }

    fn int(&self, value: &'a Value) -> Option<u64> {
        match self.deref(value) {
            Value::Integer(i) => i.as_unsigned().or_else(|| i.as_signed().map(|v| v as u64)),
            Value::Boolean(b) => Some(u64::from(*b)),
            Value::Real(r) if *r >= 0.0 => Some(*r as u64),
            _ => None,
        }
    }

    /// Elements of an NSArray / NSSet (or a plain array).
    fn array(&self, value: &'a Value) -> Vec<&'a Value> {
        match self.deref(value) {
            Value::Array(items) => items.iter().map(|v| self.deref(v)).collect(),
            Value::Dictionary(d) => d
                .get("NS.objects")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(|v| self.deref(v)).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn strings(&self, value: &'a Value) -> Vec<String> {
        self.array(value)
            .into_iter()
            .filter_map(|v| self.string(v))
            .collect()
    }

    /// Key/value pairs of an NSDictionary.
    fn dict(&self, value: &'a Value) -> Vec<(&'a Value, &'a Value)> {
        let Some(d) = self.object(value) else {
            return Vec::new();
        };
        let keys = d.get("NS.keys").and_then(Value::as_array);
        let values = d.get("NS.objects").and_then(Value::as_array);
        match (keys, values) {
            (Some(k), Some(v)) => k
                .iter()
                .zip(v.iter())
                .map(|(k, v)| (self.deref(k), self.deref(v)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// An NSURL: `NS.relative` resolved against `NS.base` when there is one.
    fn url(&self, value: &'a Value, depth: u8) -> Option<String> {
        if let Some(s) = self.string(value) {
            return Some(s);
        }
        let object = self.object(value)?;
        let relative = object.get("NS.relative").and_then(|v| self.string(v))?;
        let base = if depth > 0 {
            object.get("NS.base").and_then(|v| self.url(v, depth - 1))
        } else {
            None
        };
        match base {
            // BTM stores container-relative URLs with the base `file:///`; keep them relative
            // so the caller can resolve them against the container app.
            Some(base) if base == "file:///" => Some(relative),
            Some(base) => Some(format!("{}/{}", base.trim_end_matches('/'), relative)),
            None => Some(relative),
        }
    }

    fn record(&self, object: &'a Dictionary) -> Option<Record> {
        let get = |key: &str| object.get(key);
        let identifier = get("identifier").and_then(|v| self.string(v))?;
        Some(Record {
            identifier,
            name: get("name").and_then(|v| self.string(v)),
            developer_name: get("developerName").and_then(|v| self.string(v)),
            team_id: get("teamIdentifier").and_then(|v| self.string(v)),
            item_type: get("type").and_then(|v| self.int(v)).unwrap_or(0),
            disposition: get("disposition").and_then(|v| self.int(v)).unwrap_or(0),
            flags: get("flags").and_then(|v| self.int(v)).unwrap_or(0),
            url: get("url").and_then(|v| self.url(v, 4)),
            executable_path: get("executablePath").and_then(|v| self.string(v)),
            bundle_id: get("bundleIdentifier").and_then(|v| self.string(v)),
            container: get("container").and_then(|v| self.string(v)),
            associated_bundle_ids: get("associatedBundleIdentifiers")
                .map(|v| self.strings(v))
                .unwrap_or_default(),
            program_arguments: get("programArguments")
                .map(|v| self.strings(v))
                .unwrap_or_default(),
        })
    }
}

/// Decodes a `BackgroundItems-v<N>.btm` keyed archive.
pub(crate) fn parse_database(bytes: &[u8]) -> Result<Database, String> {
    let value =
        Value::from_reader(Cursor::new(bytes)).map_err(|e| format!("not a property list: {e}"))?;
    let top = value.as_dictionary().ok_or("not a dictionary")?;
    if top.get("$archiver").and_then(Value::as_string) != Some("NSKeyedArchiver") {
        return Err("not an NSKeyedArchiver archive".into());
    }
    let objects = top
        .get("$objects")
        .and_then(Value::as_array)
        .ok_or("archive has no $objects")?;
    let archive = Archive { objects };
    let roots = top
        .get("$top")
        .and_then(Value::as_dictionary)
        .ok_or("archive has no $top")?;
    // The root is stored under "store" today; accept any root object that has the field.
    let store = roots
        .get("store")
        .into_iter()
        .chain(roots.values())
        .filter_map(|v| archive.object(v))
        .find(|o| o.contains_key("itemsByUserIdentifier"))
        .ok_or("archive has no itemsByUserIdentifier")?;

    let mut users = Vec::new();
    for (key, items) in archive.dict(store.get("itemsByUserIdentifier").expect("checked above")) {
        let Some(user_uuid) = archive.string(key) else {
            continue;
        };
        let records = archive
            .array(items)
            .into_iter()
            .filter_map(|v| v.as_dictionary())
            .filter(|o| {
                archive
                    .class_name(o)
                    .is_none_or(|c| c.contains("ItemRecord"))
            })
            .filter_map(|o| archive.record(o))
            .collect();
        users.push(UserRecords { user_uuid, records });
    }
    if users.is_empty() {
        return Err("archive holds no user records".into());
    }
    Ok(Database { users })
}

// ---------------------------------------------------------------------------------------------
// sfltool dumpbtm (last resort)
// ---------------------------------------------------------------------------------------------

/// Runs `/usr/bin/sfltool dumpbtm`. Built-in macOS tool; only used when the database can't be
/// read, because there is no public API that lists other apps' background items.
pub(crate) fn run_sfltool(timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("/usr/bin/sfltool")
        .arg("dumpbtm")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run sfltool: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut out).map(|_| out)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("sfltool timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("sfltool failed: {e}")),
        }
    }
    let out = reader
        .join()
        .map_err(|_| "sfltool reader panicked".to_string())?
        .map_err(|e| format!("can't read sfltool output: {e}"))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The hex value in a line like `Type: legacy agent (0x10008)`.
fn trailing_hex(value: &str) -> Option<u64> {
    let start = value.rfind("(0x")?;
    let end = value[start..].find(')')? + start;
    u64::from_str_radix(&value[start + 3..end], 16).ok()
}

fn bracket_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parses `sfltool dumpbtm` output.
pub(crate) fn parse_sfltool(text: &str) -> Result<Database, String> {
    let mut db = Database::default();
    let mut current: Option<Record> = None;
    let mut in_embedded = false;

    let flush = |db: &mut Database, current: &mut Option<Record>| {
        if let Some(record) = current.take()
            && !record.identifier.is_empty()
            && let Some(user) = db.users.last_mut()
        {
            user.records.push(record);
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Records for UID") {
            flush(&mut db, &mut current);
            let uuid = rest.rsplit(':').next().unwrap_or("").trim().to_string();
            db.users.push(UserRecords {
                user_uuid: uuid,
                records: Vec::new(),
            });
            in_embedded = false;
            continue;
        }
        // "#3:" starts an item; "#1: 16.com.example" is an embedded identifier.
        if let Some(rest) = line.strip_prefix('#')
            && let Some((num, after)) = rest.split_once(':')
            && num.chars().all(|c| c.is_ascii_digit())
        {
            if after.trim().is_empty() {
                flush(&mut db, &mut current);
                current = Some(Record::default());
                in_embedded = false;
            }
            continue;
        }
        if line.starts_with("Embedded Item Identifiers") {
            in_embedded = true;
            continue;
        }
        let Some(record) = current.as_mut() else {
            continue;
        };
        if in_embedded {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let text = (value != "(null)" && !value.is_empty()).then(|| value.to_string());
        match key.trim() {
            "Name" => record.name = text,
            "Developer Name" => record.developer_name = text,
            "Team Identifier" => record.team_id = text,
            "Type" => record.item_type = trailing_hex(value).unwrap_or(0),
            "Flags" => record.flags = trailing_hex(value).unwrap_or(0),
            "Disposition" => record.disposition = trailing_hex(value).unwrap_or(0),
            "Identifier" => record.identifier = value.to_string(),
            "URL" => record.url = text,
            "Executable Path" => record.executable_path = text,
            "Bundle Identifier" => record.bundle_id = text,
            "Parent Identifier" => record.container = text,
            "Assoc. Bundle IDs" => record.associated_bundle_ids = bracket_list(value),
            _ => {}
        }
    }
    flush(&mut db, &mut current);
    if db.users.iter().all(|u| u.records.is_empty()) {
        return Err("sfltool output holds no records".into());
    }
    Ok(db)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                16,
            )
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

unsafe extern "C" {
    /// membership.h: the user's UUID (its OpenDirectory GeneratedUID), which keys the BTM
    /// records.
    fn mbr_uid_to_uuid(uid: libc::uid_t, uu: *mut u8) -> libc::c_int;
}

/// The current user's UUID in the uppercase form BTM uses.
pub(crate) fn current_user_uuid() -> Option<String> {
    let mut uu = [0u8; 16];
    // SAFETY: `uu` is the 16-byte uuid_t buffer mbr_uid_to_uuid fills.
    let rc = unsafe { mbr_uid_to_uuid(libc::getuid(), uu.as_mut_ptr()) };
    (rc == 0).then(|| format_uuid(&uu))
}

pub(crate) fn format_uuid(uu: &[u8; 16]) -> String {
    let hex: String = uu.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(
            percent_decode("OneDrive%20Launcher.app"),
            "OneDrive Launcher.app"
        );
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%E2%9C%93"), "\u{2713}");
    }

    #[test]
    fn hex_and_lists() {
        assert_eq!(trailing_hex("legacy agent (0x10008)"), Some(0x10008));
        assert_eq!(
            trailing_hex("[enabled, allowed, notified] (0xb)"),
            Some(0xb)
        );
        assert_eq!(trailing_hex("nothing"), None);
        assert_eq!(bracket_list("[ com.a, com.b ]"), vec!["com.a", "com.b"]);
        assert!(bracket_list("[ ]").is_empty());
    }

    #[test]
    fn identifiers_and_paths() {
        let r = Record {
            identifier: "8.com.example.agent".into(),
            url: Some("file:///Users/me/Library/LaunchAgents/com.example.agent.plist".into()),
            item_type: 0x10008,
            disposition: 0xb,
            ..Record::default()
        };
        assert_eq!(r.short_identifier(), "com.example.agent");
        assert!(r.is_legacy() && r.enabled() && r.allowed());
        assert_eq!(r.base_type(), TYPE_AGENT);
        assert_eq!(
            r.absolute_path(),
            Some(PathBuf::from(
                "/Users/me/Library/LaunchAgents/com.example.agent.plist"
            ))
        );
        let dev = Record {
            identifier: "Example Developer".into(),
            ..Record::default()
        };
        assert_eq!(dev.short_identifier(), "Example Developer");
        let app = Record {
            identifier: "2.com.example.app".into(),
            url: Some("file:///Applications/Example%20App.app/".into()),
            ..Record::default()
        };
        assert_eq!(
            app.absolute_path(),
            Some(PathBuf::from("/Applications/Example App.app"))
        );
        let helper = Record {
            identifier: "4.com.example.helper".into(),
            url: Some("Contents/Library/LoginItems/Example%20Helper.app".into()),
            container: Some("2.com.example.app".into()),
            ..Record::default()
        };
        let db = Database {
            users: vec![UserRecords {
                user_uuid: "U".into(),
                records: vec![app, helper.clone()],
            }],
        };
        assert_eq!(
            db.resolve_path(&helper, Some("U")),
            Some(PathBuf::from(
                "/Applications/Example App.app/Contents/Library/LoginItems/Example Helper.app"
            ))
        );
    }

    const DB_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/btm-v16-synthetic.btm");
    const SFL_FIXTURE: &str = include_str!("../tests/fixtures/sfltool-dumpbtm-synthetic.txt");
    const USER: &str = "11111111-2222-3333-4444-555555555555";
    const SYSTEM: &str = "FFFFEEEE-DDDD-CCCC-BBBB-AAAAFFFFFFFE";

    fn check_fixture_db(db: &Database) {
        let user = db.user(USER).expect("user records");
        let get = |id: &str| {
            user.records
                .iter()
                .find(|r| r.identifier == id)
                .unwrap_or_else(|| panic!("{id}"))
        };

        let notes = get("2.com.example.notes");
        assert_eq!(notes.base_type(), TYPE_APP);
        assert!(notes.enabled() && notes.allowed());
        assert_eq!(notes.bundle_id.as_deref(), Some("com.example.notes"));
        assert_eq!(notes.team_id.as_deref(), Some("EXAMPLE123"));
        assert_eq!(
            notes.absolute_path(),
            Some(PathBuf::from("/Applications/Example Notes.app"))
        );

        let launcher = get("4.com.example.sync.launcher");
        assert_eq!(launcher.base_type(), TYPE_LOGIN_ITEM);
        assert!(!launcher.enabled());
        assert_eq!(launcher.container.as_deref(), Some("2.com.example.sync"));
        assert_eq!(
            db.resolve_path(launcher, Some(USER)),
            Some(PathBuf::from(
                "/Applications/Example Sync.app/Contents/Library/LoginItems/Sync Launcher.app"
            ))
        );

        let updater = get("8.com.example.updater");
        assert!(updater.is_legacy());
        assert_eq!(updater.base_type(), TYPE_AGENT);
        assert_eq!(updater.developer_name.as_deref(), Some("Example Corp"));
        assert_eq!(updater.associated_bundle_ids, vec!["com.example.notes"]);
        assert_eq!(
            updater.executable_path.as_deref(),
            Some("/Users/tester/Library/Application Support/Example/Updater")
        );

        let agent = get("8.com.example.notes.agent");
        assert!(!agent.is_legacy());
        assert_eq!(agent.disposition, 0x3);

        let daemon = db
            .user(SYSTEM)
            .unwrap()
            .records
            .iter()
            .find(|r| r.identifier == "16.com.example.helperd")
            .unwrap();
        assert_eq!(daemon.item_type, TYPE_LEGACY | TYPE_DAEMON);
        let legacy = db.legacy_by_plist(Some(USER));
        assert!(legacy.contains_key(Path::new(
            "/Library/LaunchDaemons/com.example.helperd.plist"
        )));
        assert!(legacy.contains_key(Path::new(
            "/Users/tester/Library/LaunchAgents/com.example.updater.plist"
        )));
    }

    #[test]
    fn parses_keyed_archive_fixture() {
        let db = parse_database(DB_FIXTURE).unwrap();
        assert_eq!(db.users.len(), 2);
        assert_eq!(db.user(USER).unwrap().records.len(), 7);
        check_fixture_db(&db);
        // The program arguments only exist in the database form.
        let updater = db.find("8.com.example.updater", Some(USER)).unwrap();
        assert_eq!(updater.program_arguments.len(), 2);
    }

    #[test]
    fn parses_sfltool_fixture() {
        let db = parse_sfltool(SFL_FIXTURE).unwrap();
        assert_eq!(db.users.len(), 2);
        assert_eq!(db.user(USER).unwrap().records.len(), 6);
        check_fixture_db(&db);
    }

    #[test]
    fn rejects_other_data() {
        assert!(parse_database(b"garbage").is_err());
        assert!(parse_database(br#"<plist version="1.0"><dict/></plist>"#).is_err());
        let not_btm =
            br#"<plist version="1.0"><dict><key>$archiver</key><string>NSKeyedArchiver</string>
<key>$top</key><dict/><key>$objects</key><array><string>$null</string></array></dict></plist>"#;
        assert!(
            parse_database(not_btm)
                .unwrap_err()
                .contains("itemsByUserIdentifier")
        );
        assert!(parse_sfltool("").is_err());
        assert!(parse_sfltool("Records for UID 501 : X\n Items:\n").is_err());
    }

    #[test]
    fn newest_database_file() {
        let dir = crate::testutil::TempDir::new("btm-dir");
        for name in [
            "BackgroundItems-v4.btm",
            "BackgroundItems-v16.btm",
            "BackgroundItems-v9.btm",
            "other.btm",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(
            database_path(dir.path()),
            Some(dir.path().join("BackgroundItems-v16.btm"))
        );
        assert_eq!(database_path(&dir.path().join("missing")), None);
    }

    #[test]
    fn uuid_format() {
        let uu = [
            0xFF, 0xFF, 0xEE, 0xEE, 0xDD, 0xDD, 0xCC, 0xCC, 0xBB, 0xBB, 0xAA, 0xAA, 0, 0, 0, 0,
        ];
        assert_eq!(format_uuid(&uu), "FFFFEEEE-DDDD-CCCC-BBBB-AAAA00000000");
        // Reading our own UUID is a read-only directory lookup.
        let me = current_user_uuid().expect("current user has a UUID");
        assert_eq!(me.len(), 36);
    }
}
