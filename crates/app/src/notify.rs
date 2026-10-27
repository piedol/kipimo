//! Notifications through UNUserNotificationCenter.
//!
//! `post(Alert)` shows one. Kipimo asks for permission the first time it has
//! something to say, not at launch. A click (or the notification's button)
//! runs the alert's [`Action`]. Alerts with the same `id` replace each other.
//!
//! For the integration step: build an [`Alert`] with a [`Kind`] (which picks
//! the button) and an `action`, e.g. leftovers found for Zoom:
//!
//! ```ignore
//! notify::post(Alert {
//!     id: "leftovers:us.zoom.xos".into(),
//!     title: "Zoom left 1.2 GB behind".into(),
//!     body: "Review its leftovers and move them to the Trash.".into(),
//!     kind: Kind::Leftovers,
//!     action: Some(Action::Custom("leftovers:us.zoom.xos".into())),
//! });
//! ```
//! A new kind of alert with its own button is `Kind::Custom { category, button }`.

use std::cell::RefCell;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSError, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
    UNNotificationAction, UNNotificationActionOptions, UNNotificationCategory,
    UNNotificationCategoryOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNUserNotificationCenter,
};

use crate::actions::Action;
use crate::i18n::tr;
use crate::util::{self, ns};

/// Which button a notification gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Disk,
    Trash,
    Bluetooth,
    Drive,
    Leftovers,
    Weekly,
    /// A new version of Kipimo.
    Update,
    #[allow(dead_code)] // Extension point for the integration step.
    Custom {
        category: &'static str,
        button: &'static str,
    },
}

impl Kind {
    const BUILTIN: [Kind; 7] = [
        Kind::Disk,
        Kind::Trash,
        Kind::Bluetooth,
        Kind::Drive,
        Kind::Leftovers,
        Kind::Weekly,
        Kind::Update,
    ];

    pub fn category(self) -> &'static str {
        match self {
            Kind::Disk => "disk",
            Kind::Trash => "trash",
            Kind::Bluetooth => "bluetooth",
            Kind::Drive => "drive",
            Kind::Leftovers => "leftovers",
            Kind::Weekly => "weekly",
            Kind::Update => "update",
            Kind::Custom { category, .. } => category,
        }
    }

    /// The button title, or `None` for a notification you just read. A custom kind's
    /// `button` is shown as it is.
    pub fn button(self) -> Option<String> {
        match self {
            Kind::Disk => Some(tr("Clean Up")),
            Kind::Trash => Some(tr("Open Trash")),
            Kind::Leftovers => Some(tr("Review")),
            Kind::Weekly => Some(tr("Open Kipimo")),
            Kind::Update => Some(tr("Update…")),
            Kind::Bluetooth | Kind::Drive => None,
            Kind::Custom { button, .. } => Some(button.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    /// Replaces an earlier notification with the same id.
    pub id: String,
    pub title: String,
    pub body: String,
    pub kind: Kind,
    /// What a click or the button does.
    pub action: Option<Action>,
}

/// The id of the button every category uses; it runs the alert's action.
const BUTTON_ID: &str = "open";
const ACTION_KEY: &str = "action";

thread_local! {
    static CUSTOM_KINDS: RefCell<Vec<Kind>> = const { RefCell::new(Vec::new()) };
}

/// UserNotifications only works inside an app bundle.
pub fn available() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
        && NSBundle::mainBundle()
            .bundlePath()
            .to_string()
            .ends_with(".app")
}

fn center() -> Option<Retained<UNUserNotificationCenter>> {
    available().then(UNUserNotificationCenter::currentNotificationCenter)
}

fn category(k: Kind) -> Retained<UNNotificationCategory> {
    let actions: Vec<Retained<UNNotificationAction>> = k
        .button()
        .map(|b| {
            UNNotificationAction::actionWithIdentifier_title_options(
                &ns(BUTTON_ID),
                &ns(&b),
                UNNotificationActionOptions::Foreground,
            )
        })
        .into_iter()
        .collect();
    let actions = NSArray::from_retained_slice(&actions);
    let intents: Retained<NSArray<NSString>> = NSArray::new();
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &ns(k.category()),
        &actions,
        &intents,
        UNNotificationCategoryOptions::empty(),
    )
}

fn register_categories(center: &UNUserNotificationCenter) {
    let mut kinds: Vec<Kind> = Kind::BUILTIN.to_vec();
    CUSTOM_KINDS.with(|c| kinds.extend(c.borrow().iter().copied()));
    let cats: Vec<_> = kinds.into_iter().map(category).collect();
    center.setNotificationCategories(&NSSet::from_retained_slice(&cats));
}

/// Set up the delegate and button categories. No permission prompt.
pub fn install() {
    let Some(c) = center() else {
        util::log("notifications need the app bundle; skipped");
        return;
    };
    let d = crate::ui::controller(util::mtm());
    c.setDelegate(Some(ProtocolObject::from_ref(&*d)));
    register_categories(&c);
}

fn request_for(a: &Alert) -> Retained<UNNotificationRequest> {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&ns(&a.title));
    content.setBody(&ns(&a.body));
    content.setCategoryIdentifier(&ns(a.kind.category()));
    if let Some(action) = &a.action {
        let key = ns(ACTION_KEY);
        let value = ns(&action.key());
        let info = NSDictionary::from_slices(&[&*key], &[&*value]);
        // SAFETY: an NSDictionary of NSStrings is a valid userInfo.
        unsafe { content.setUserInfo(&Retained::cast_unchecked::<NSDictionary>(info)) };
    }
    UNNotificationRequest::requestWithIdentifier_content_trigger(&ns(&a.id), &content, None)
}

/// Show a notification, asking for permission first if macOS hasn't been asked yet.
pub fn post(a: Alert) {
    util::log(format!("notify {}: {}", a.title, a.body));
    let Some(c) = center() else { return };
    if let Kind::Custom { .. } = a.kind {
        let new = CUSTOM_KINDS.with(|k| {
            let mut k = k.borrow_mut();
            let new = !k.contains(&a.kind);
            if new {
                k.push(a.kind);
            }
            new
        });
        if new {
            register_categories(&c);
        }
    }
    let request = request_for(&a);
    let center = c.clone();
    let block = RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
        // SAFETY: the system passes a valid settings object.
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let add = {
            let center = center.clone();
            let request = request.clone();
            move || center.addNotificationRequest_withCompletionHandler(&request, None)
        };
        if status == UNAuthorizationStatus::NotDetermined {
            let ask = RcBlock::new(move |granted: Bool, _e: *mut NSError| {
                if granted.as_bool() {
                    add();
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &ask,
            );
        } else if status == UNAuthorizationStatus::Authorized
            || status == UNAuthorizationStatus::Provisional
        {
            add();
        }
    });
    c.getNotificationSettingsWithCompletionHandler(&block);
}

/// The delegate got a click or a button press.
pub fn responded(response: &UNNotificationResponse) {
    let id = response.actionIdentifier().to_string();
    let info = response.notification().request().content().userInfo();
    let key = ns(ACTION_KEY);
    let key: &AnyObject = &key;
    let action = info
        .objectForKey(key)
        .and_then(|v| v.downcast::<NSString>().ok())
        .and_then(|s| Action::parse(&s.to_string()));
    // Default click or our button: run the action. A dismissal does nothing.
    let dismissed = id == "com.apple.UNNotificationDismissActionIdentifier";
    if let Some(a) = action.filter(|_| !dismissed) {
        util::main_async(move || crate::actions::run(a));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_unique() {
        let mut ids: Vec<_> = Kind::BUILTIN.iter().map(|k| k.category()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), Kind::BUILTIN.len());
        assert_eq!(Kind::Trash.button().as_deref(), Some("Open Trash"));
        assert_eq!(
            Kind::Custom {
                category: "x",
                button: "Do It"
            }
            .button()
            .as_deref(),
            Some("Do It")
        );
    }
}
