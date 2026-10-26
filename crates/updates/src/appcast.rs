//! Sparkle appcasts: parsing the RSS feed and choosing the release Sparkle would offer.

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};

use crate::finding::{Candidate, Critical, Env, Finding};
use crate::types::Release;
use crate::version::{compare_sparkle, is_comparable};

/// One `<item>` with the fields Kipimo needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AppcastItem {
    pub title: Option<String>,
    /// `sparkle:version` (element, enclosure attribute, or Sparkle's file-name fallback).
    pub version: Option<String>,
    pub short_version: Option<String>,
    pub minimum_system_version: Option<String>,
    pub maximum_system_version: Option<String>,
    pub channel: Option<String>,
    pub release_notes_link: Option<String>,
    pub full_release_notes_link: Option<String>,
    pub link: Option<String>,
    pub enclosure_url: Option<String>,
    /// `sparkle:os` on the enclosure ("macos" or "windows").
    pub os: Option<String>,
    pub critical: Option<Critical>,
    pub informational: bool,
    pub hardware_requirements: Vec<String>,
    pub pub_date: Option<String>,
}

/// Elements directly inside `<item>` whose text we keep.
fn is_text_field(name: &str) -> bool {
    matches!(
        name,
        "title"
            | "link"
            | "pubDate"
            | "sparkle:version"
            | "sparkle:shortVersionString"
            | "sparkle:minimumSystemVersion"
            | "sparkle:maximumSystemVersion"
            | "sparkle:channel"
            | "sparkle:releaseNotesLink"
            | "sparkle:fullReleaseNotesLink"
            | "sparkle:hardwareRequirements"
    )
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .map(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|_| a.value.trim().to_string())
        })
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Release-notes links can come in several languages; keep the English or unlabelled one.
fn lang_rank(lang: Option<&str>) -> u8 {
    match lang {
        None => 0,
        Some(l) if l.eq_ignore_ascii_case("en") || l.to_ascii_lowercase().starts_with("en-") => 1,
        Some(_) => 2,
    }
}

impl AppcastItem {
    fn set_text(&mut self, name: &str, text: String, lang_ranks: &mut [u8; 2], lang: Option<&str>) {
        let Some(text) = non_empty(text) else { return };
        match name {
            "title" => self.title = Some(text),
            "link" => self.link = Some(text),
            "pubDate" => self.pub_date = Some(text),
            "sparkle:version" => self.version = Some(text),
            "sparkle:shortVersionString" => self.short_version = Some(text),
            "sparkle:minimumSystemVersion" => self.minimum_system_version = Some(text),
            "sparkle:maximumSystemVersion" => self.maximum_system_version = Some(text),
            "sparkle:channel" => self.channel = Some(text),
            "sparkle:releaseNotesLink" | "sparkle:fullReleaseNotesLink" => {
                let slot = usize::from(name == "sparkle:fullReleaseNotesLink");
                let rank = lang_rank(lang);
                let field = if slot == 0 {
                    &mut self.release_notes_link
                } else {
                    &mut self.full_release_notes_link
                };
                if field.is_none() || rank < lang_ranks[slot] {
                    *field = Some(text);
                    lang_ranks[slot] = rank;
                }
            }
            "sparkle:hardwareRequirements" => {
                self.hardware_requirements = text
                    .split([',', ' '])
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }

    fn read_enclosure(&mut self, e: &BytesStart<'_>) {
        self.enclosure_url = attr(e, "url").and_then(non_empty);
        if let Some(v) = attr(e, "sparkle:version").and_then(non_empty) {
            self.version.get_or_insert(v);
        }
        if let Some(v) = attr(e, "sparkle:shortVersionString").and_then(non_empty) {
            self.short_version.get_or_insert(v);
        }
        self.os = attr(e, "sparkle:os").and_then(non_empty);
    }

    fn finish(&mut self) {
        // Sparkle's legacy fallback: "MyApp_1.2.zip" gives version "1.2".
        if self.version.is_none()
            && let Some(url) = &self.enclosure_url
        {
            let file = url.split(['?', '#']).next().unwrap_or(url);
            if let Some((_, last)) = file.rsplit_once('_') {
                let stem = last.rsplit_once('.').map_or(last, |(s, _)| s);
                if !stem.is_empty() && !stem.contains('/') && is_comparable(stem) {
                    self.version = Some(stem.to_string());
                }
            }
        }
    }
}

#[derive(Default)]
struct Parser {
    items: Vec<AppcastItem>,
    saw_rss: bool,
    /// Element names from the root to the current element.
    stack: Vec<String>,
    item: Option<AppcastItem>,
    item_depth: usize,
    lang_ranks: [u8; 2],
    /// The text field being read: (name, xml:lang, text so far).
    field: Option<(String, Option<String>, String)>,
}

impl Parser {
    fn open(&mut self, e: &BytesStart<'_>, empty: bool) {
        let name = e.name().as_ref().to_string();
        if name == "rss" || name == "channel" {
            self.saw_rss = true;
        }
        if let Some(it) = self.item.as_mut() {
            let direct_child = self.stack.len() == self.item_depth + 1;
            if direct_child && name == "enclosure" {
                it.read_enclosure(e);
            } else if direct_child && is_text_field(&name) && !empty {
                self.field = Some((name.clone(), attr(e, "xml:lang"), String::new()));
            } else if name == "sparkle:criticalUpdate" {
                // Sparkle 2 puts it in the item; Sparkle 1 inside <sparkle:tags>.
                it.critical = Some(match attr(e, "sparkle:version").and_then(non_empty) {
                    Some(v) => Critical::Below(v),
                    None => Critical::All,
                });
            } else if direct_child && name == "sparkle:informationalUpdate" {
                it.informational = true;
            }
        } else if name == "item" && !empty {
            self.item = Some(AppcastItem::default());
            self.item_depth = self.stack.len();
            self.lang_ranks = [u8::MAX; 2];
        }
        if !empty {
            self.stack.push(name);
        }
    }

    fn close(&mut self, name: &str) {
        if let Some((fname, lang, text)) = self.field.take() {
            if fname == name {
                if let Some(it) = self.item.as_mut() {
                    it.set_text(&fname, text, &mut self.lang_ranks, lang.as_deref());
                }
            } else {
                self.field = Some((fname, lang, text));
            }
        }
        self.stack.pop();
        if name == "item"
            && self.stack.len() == self.item_depth
            && let Some(mut it) = self.item.take()
        {
            it.finish();
            self.items.push(it);
        }
    }

    fn text(&mut self, s: &str) {
        if let Some((_, _, text)) = self.field.as_mut() {
            text.push_str(s);
        }
    }
}

/// Parses an appcast. Malformed XML after at least one complete item still yields the items
/// read so far (feeds list newest first, and Sparkle itself is lenient); otherwise it's an error.
pub(crate) fn parse(xml: &str) -> Result<Vec<AppcastItem>, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut p = Parser::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => p.open(&e, false),
            Ok(Event::Empty(e)) => p.open(&e, true),
            Ok(Event::End(e)) => p.close(e.name().as_ref()),
            Ok(Event::Text(t)) => p.text(&t.xml_content(XmlVersion::Implicit1_0)),
            Ok(Event::CData(t)) => p.text(&t.into_inner()),
            Ok(Event::GeneralRef(r)) => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    p.text(c.encode_utf8(&mut [0; 4]));
                } else if let Some(s) =
                    resolve_predefined_entity(&r.xml_content(XmlVersion::Implicit1_0))
                {
                    p.text(s);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                if p.items.is_empty() {
                    return Err(format!("the update feed isn't valid XML ({e})"));
                }
                log::warn!(
                    "appcast: XML error after {} items, using those: {e}",
                    p.items.len()
                );
                break;
            }
        }
    }
    if p.items.is_empty() && !p.saw_rss {
        return Err("the update feed isn't an appcast".into());
    }
    Ok(p.items)
}

fn os_at_least(os: &str, min: &str) -> bool {
    compare_sparkle(os, min) != std::cmp::Ordering::Less
}

/// The version Sparkle orders items by.
fn sort_key(it: &AppcastItem) -> Option<&str> {
    it.version.as_deref().or(it.short_version.as_deref())
}

fn to_candidate(it: &AppcastItem, feed_url: &str) -> Candidate {
    let resolve = |u: &Option<String>| u.as_deref().and_then(|u| crate::url::resolve(feed_url, u));
    let notes = resolve(&it.release_notes_link)
        .or_else(|| resolve(&it.full_release_notes_link))
        .or_else(|| resolve(&it.link));
    let download = if it.informational {
        resolve(&it.link)
    } else {
        resolve(&it.enclosure_url)
    };
    Candidate {
        release: Release {
            version: it.short_version.clone().or_else(|| it.version.clone()),
            build: it.version.clone(),
            minimum_system_version: it.minimum_system_version.clone(),
            release_notes_url: notes,
            download_url: download,
            published: it.pub_date.clone(),
        },
        critical: it.critical.clone(),
    }
}

/// Picks what Sparkle would offer on this Mac: the highest version among items for macOS, in
/// an allowed channel and within the item's macOS range. Also reports the highest item that
/// only a newer macOS could install.
pub(crate) fn select(items: &[AppcastItem], env: &Env, feed_url: &str) -> Finding {
    let mut best: Option<&AppcastItem> = None;
    let mut best_any: Option<&AppcastItem> = None;
    let newer = |a: &AppcastItem, b: Option<&AppcastItem>| match (sort_key(a), b.and_then(sort_key))
    {
        (Some(_), None) => true,
        (Some(x), Some(y)) => compare_sparkle(x, y) == std::cmp::Ordering::Greater,
        _ => false,
    };
    for it in items {
        if sort_key(it).is_none() || (it.enclosure_url.is_none() && !it.informational) {
            continue;
        }
        if it
            .os
            .as_deref()
            .is_some_and(|os| !os.eq_ignore_ascii_case("macos"))
        {
            continue;
        }
        if let Some(ch) = it.channel.as_deref()
            && !env.channels.iter().any(|c| c.eq_ignore_ascii_case(ch))
        {
            continue;
        }
        if !env.arm64
            && it
                .hardware_requirements
                .iter()
                .any(|h| h.eq_ignore_ascii_case("arm64"))
        {
            continue;
        }
        if it
            .maximum_system_version
            .as_deref()
            .is_some_and(|max| compare_sparkle(&env.macos, max) == std::cmp::Ordering::Greater)
        {
            continue;
        }
        if newer(it, best_any) {
            best_any = Some(it);
        }
        let compatible = it
            .minimum_system_version
            .as_deref()
            .is_none_or(|min| os_at_least(&env.macos, min));
        if compatible && newer(it, best) {
            best = Some(it);
        }
    }
    let newest_incompatible = best_any.filter(|any| newer(any, best));
    Finding {
        latest: best.map(|it| to_candidate(it, feed_url)),
        newest_incompatible: newest_incompatible.map(|it| to_candidate(it, feed_url)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::evaluate;
    use crate::types::{InstalledApp, Status, UpdateSource};

    const FEED: &str = "https://example.test/appcast.xml";

    fn env(macos: &str, channels: &[&str]) -> Env {
        Env {
            macos: macos.into(),
            darwin: "25.6.0".into(),
            arm64: true,
            channels: channels.iter().map(|c| c.to_string()).collect(),
            country: "us".into(),
        }
    }

    fn app(version: &str, build: &str) -> InstalledApp {
        InstalledApp {
            name: "Example".into(),
            path: "/Applications/Example.app".into(),
            bundle_id: Some("com.example.app".into()),
            version: Some(version.into()),
            build: Some(build.into()),
            minimum_system_version: None,
            source: UpdateSource::Sparkle {
                feed_url: FEED.into(),
                from_defaults: false,
            },
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn multi_item_feed() {
        let items = parse(&fixture("appcast_multi.xml")).unwrap();
        assert_eq!(items.len(), 7);
        let first = &items[0];
        assert_eq!(first.version.as_deref(), Some("230"));
        assert_eq!(first.short_version.as_deref(), Some("2.3"));
        // The delta's enclosure must not replace the item's own.
        assert_eq!(
            first.enclosure_url.as_deref(),
            Some("https://example.test/dl/Example-2.3.zip")
        );
        // The unlabelled link wins over the German one, and whitespace is trimmed.
        assert_eq!(
            first.release_notes_link.as_deref(),
            Some("https://example.test/notes/2.3.html")
        );
        assert_eq!(items[1].channel.as_deref(), Some("beta"));
        assert_eq!(items[2].critical, Some(Critical::Below("215".into())));
        assert_eq!(items[3].os.as_deref(), Some("windows"));

        let f = select(&items, &env("26.6.2", &[]), FEED);
        let latest = f.latest.as_ref().unwrap();
        assert_eq!(latest.release.version.as_deref(), Some("2.3"));
        assert_eq!(latest.release.build.as_deref(), Some("230"));
        assert_eq!(
            latest.release.minimum_system_version.as_deref(),
            Some("13.0")
        );
        assert_eq!(
            latest.release.published.as_deref(),
            Some("Mon, 05 Oct 2026 10:00:00 +0000")
        );
        let newer = f.newest_incompatible.as_ref().unwrap();
        assert_eq!(newer.release.version.as_deref(), Some("3.0"));
        assert_eq!(
            newer.release.release_notes_url.as_deref(),
            Some("https://example.test/app/3.0")
        );

        // Opting into the beta channel picks the beta.
        let f = select(&items, &env("26.6.2", &["beta"]), FEED);
        assert_eq!(f.latest.unwrap().release.version.as_deref(), Some("2.10b1"));

        // On macOS 12, 2.3 is out of reach; 2.2 (relative URL) is the best this Mac can run.
        let f = select(&items, &env("12.7", &[]), FEED);
        let latest = f.latest.unwrap();
        assert_eq!(latest.release.version.as_deref(), Some("2.2"));
        assert_eq!(
            latest.release.download_url.as_deref(),
            Some("https://example.test/dl/Example-2.2.zip")
        );
        assert_eq!(
            f.newest_incompatible.unwrap().release.version.as_deref(),
            Some("3.0")
        );
    }

    #[test]
    fn statuses_from_multi_feed() {
        let items = parse(&fixture("appcast_multi.xml")).unwrap();
        let f = select(&items, &env("26.6.2", &[]), FEED);

        let e = evaluate(&app("2.1", "210"), &f);
        assert_eq!(e.status, Status::UpdateAvailable);
        assert_eq!(e.latest.as_ref().unwrap().version.as_deref(), Some("2.3"));
        assert_eq!(
            e.newer_requires_macos.as_ref().unwrap().version.as_deref(),
            Some("3.0")
        );
        assert!(!e.critical, "2.3 isn't critical");

        // Installed is current for this macOS, and 3.0 needs a newer one.
        let e = evaluate(&app("2.3", "230"), &f);
        assert_eq!(
            e.status,
            Status::RequiresNewerMacos {
                minimum: "99.0".into()
            }
        );
        assert_eq!(e.latest.unwrap().version.as_deref(), Some("3.0"));

        // A beta newer than everything is "up to date".
        let e = evaluate(&app("3.1b1", "3100"), &f);
        assert_eq!(e.status, Status::UpToDate);

        // Critical: 2.2 is critical for installs below 215.
        let f12 = select(&items, &env("12.7", &[]), FEED);
        assert!(evaluate(&app("2.0", "200"), &f12).critical);
        assert!(!evaluate(&app("2.1.6", "216"), &f12).critical);
    }

    #[test]
    fn sparkle1_enclosure_only_versions() {
        let items = parse(&fixture("appcast_sparkle1.xml")).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].version.as_deref(), Some("1.10"));
        assert_eq!(items[0].critical, Some(Critical::All));
        assert_eq!(
            items[0].release_notes_link.as_deref(),
            Some("https://legacy.test/notes.html?v=1.10&lang=en")
        );
        // No version anywhere: Sparkle's "Name_1.8.zip" fallback.
        assert_eq!(items[2].version.as_deref(), Some("1.8"));

        let f = select(&items, &env("26.6.2", &[]), FEED);
        // "1.10" beats "1.9" numerically, not as text.
        assert_eq!(
            f.latest.as_ref().unwrap().release.version.as_deref(),
            Some("1.10")
        );
        let e = evaluate(&app("1.9", "1.9"), &f);
        assert_eq!(e.status, Status::UpdateAvailable);
        assert!(e.critical);
    }

    #[test]
    fn informational_update_links_to_page() {
        let items = parse(&fixture("appcast_informational.xml")).unwrap();
        let f = select(&items, &env("26.6.2", &[]), FEED);
        let r = f.latest.unwrap().release;
        assert_eq!(r.download_url.as_deref(), Some("https://info.test/buy-5"));
        assert_eq!(r.version.as_deref(), Some("5.0"));
    }

    #[test]
    fn malformed_feeds() {
        // Broken after one complete item: keep that item.
        let items = parse(&fixture("appcast_truncated.xml")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].short_version.as_deref(), Some("1.4"));
        // Broken before any item: an error.
        assert!(parse(&fixture("appcast_malformed.xml")).is_err());
        // An HTML page (a CDN redirect page, say).
        assert!(parse(&fixture("not_an_appcast.html")).is_err());
        assert!(parse("").is_err());
        assert!(parse("just text").is_err());
        // An empty but valid feed parses to no items.
        assert_eq!(parse("<rss><channel></channel></rss>").unwrap().len(), 0);
    }
}
