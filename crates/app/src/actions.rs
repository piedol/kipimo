//! Things a button, a recommendation or a notification click can do. Each has
//! a string key so it can ride in a notification's `userInfo`.
//!
//! The integration step adds its own with [`Action::Custom`] and
//! [`register_custom`]: e.g. `Action::Custom("leftovers:com.zoom.us".into())`
//! with a handler registered under `"leftovers"`.

use std::cell::RefCell;
use std::path::PathBuf;

use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSURL;

use crate::tools_window::Tab;
use crate::util::{self, ns};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    OpenPanel,
    OpenTools(Tab),
    OpenSettings,
    /// Open the Trash in Finder (emptying it stays Finder's job, with its own confirmation).
    OpenTrash,
    Reveal(PathBuf),
    OpenUrl(String),
    /// `name` or `name:argument`, routed to a handler from [`register_custom`].
    Custom(String),
}

impl Action {
    pub fn key(&self) -> String {
        match self {
            Action::OpenPanel => "panel".into(),
            Action::OpenTools(t) => format!("tools:{}", t.key()),
            Action::OpenSettings => "settings".into(),
            Action::OpenTrash => "trash".into(),
            Action::Reveal(p) => format!("reveal:{}", p.display()),
            Action::OpenUrl(u) => format!("url:{u}"),
            Action::Custom(c) => format!("custom:{c}"),
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        let (head, rest) = s.split_once(':').unwrap_or((s, ""));
        Some(match head {
            "panel" => Action::OpenPanel,
            "tools" => Action::OpenTools(Tab::from_key(rest)?),
            "settings" => Action::OpenSettings,
            "trash" => Action::OpenTrash,
            "reveal" if !rest.is_empty() => Action::Reveal(PathBuf::from(rest)),
            "url" if !rest.is_empty() => Action::OpenUrl(rest.to_string()),
            "custom" if !rest.is_empty() => Action::Custom(rest.to_string()),
            _ => return None,
        })
    }
}

type Handler = Box<dyn Fn(&str)>;

thread_local! {
    static CUSTOM: RefCell<Vec<(String, Handler)>> = const { RefCell::new(Vec::new()) };
}

/// Handle `Action::Custom("name…")`. The handler gets the text after `name:` (or "").
#[allow(dead_code)] // Extension point for the integration step.
pub fn register_custom(name: &str, handler: impl Fn(&str) + 'static) {
    CUSTOM.with(|c| c.borrow_mut().push((name.to_string(), Box::new(handler))));
}

/// Perform an action (main thread).
pub fn run(a: Action) {
    util::log(format!("action {}", a.key()));
    match a {
        Action::OpenPanel => crate::panel::open(),
        Action::OpenTools(t) => {
            crate::panel::close();
            crate::tools_window::show(t);
        }
        Action::OpenSettings => {
            crate::panel::close();
            crate::settings_window::show();
        }
        Action::OpenTrash => {
            crate::panel::close();
            open_path(&kipimo_sys::disk::trash_dir());
        }
        Action::Reveal(p) => {
            crate::panel::close();
            let url = NSURL::fileURLWithPath(&ns(&p.to_string_lossy()));
            let urls = objc2_foundation::NSArray::from_retained_slice(&[url]);
            NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
        }
        Action::OpenUrl(u) => {
            if let Some(url) = NSURL::URLWithString(&ns(&u)) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }
        Action::Custom(c) => {
            let (name, arg) = c.split_once(':').unwrap_or((&c, ""));
            let handled = CUSTOM.with(|h| {
                let h = h.borrow();
                h.iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, f)| f(arg))
                    .is_some()
            });
            if !handled {
                util::log(format!("no handler for custom action {name}"));
            }
        }
    }
}

fn open_path(p: &std::path::Path) {
    let url = NSURL::fileURLWithPath(&ns(&p.to_string_lossy()));
    NSWorkspace::sharedWorkspace().openURL(&url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let all = [
            Action::OpenPanel,
            Action::OpenTools(Tab::Clean),
            Action::OpenTools(Tab::Updates),
            Action::OpenSettings,
            Action::OpenTrash,
            Action::Reveal(PathBuf::from("/Users/x/Library/Caches/com.zoom.us")),
            Action::OpenUrl("https://example.com/a:b".into()),
            Action::Custom("leftovers:com.zoom.us".into()),
        ];
        for a in all {
            assert_eq!(Action::parse(&a.key()), Some(a.clone()), "{a:?}");
        }
        assert_eq!(Action::parse("tools:nope"), None);
        assert_eq!(Action::parse("reveal:"), None);
        assert_eq!(Action::parse("bogus"), None);
    }
}
