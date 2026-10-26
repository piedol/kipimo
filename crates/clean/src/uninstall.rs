//! Tool B: move an app and the leftovers the user picked to the Trash.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::apps::{self, AppIndex, AppTarget, BundleInfo, Protection};
use crate::env::Env;
use crate::leftovers::{self, Leftover};
use crate::trash::{self, Zone};
use crate::types::{ItemResult, SkipReason};

/// How an uninstall ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UninstallStatus {
    /// The app went to the Trash (see each leftover's own result).
    Done,
    /// The app is running. Nothing was moved; ask the user to quit it first.
    QuitFirst,
    /// The app is part of macOS (or is Kipimo). Nothing was moved.
    Refused { protection: Protection },
    /// The path isn't an app bundle in one of the app folders.
    NotAnApp,
    /// Moving the app failed (or needs admin); its leftovers were kept.
    AppNotMoved,
    /// Cancelled before the app was moved.
    Cancelled,
}

/// The result of [`uninstall`].
#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    pub status: UninstallStatus,
    /// The app bundle's own result.
    pub app: ItemResult,
    pub leftovers: Vec<ItemResult>,
}

/// Streamed by [`uninstall`]: `done` of `total` items handled.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UninstallProgress {
    pub done: usize,
    pub total: usize,
}

/// Moves `app_path` and the `selected` leftovers to the Trash.
///
/// Order: check that it's an app bundle in an app folder, not protected and not running; build a
/// fresh leftover list (before the app moves, so the finder can still read it) and check every
/// selected leftover against it; move the app; then move each leftover. If the app can't be
/// moved, no leftover is touched.
pub fn uninstall(
    env: &Env,
    app_path: &Path,
    app_bytes: u64,
    selected: &[Leftover],
    progress: &mut dyn FnMut(UninstallProgress),
    cancel: &AtomicBool,
) -> UninstallReport {
    let total = selected.len() + 1;
    let skip_all = |reason: SkipReason| -> Vec<ItemResult> {
        selected
            .iter()
            .map(|l| ItemResult::skipped(l.path.clone(), l.bytes, reason.clone()))
            .collect()
    };
    let report = |status, app_reason: SkipReason, leftover_reason: SkipReason| UninstallReport {
        status,
        app: ItemResult::skipped(app_path.to_path_buf(), app_bytes, app_reason),
        leftovers: skip_all(leftover_reason),
    };

    let in_app_dir = apps::app_bundles(env).iter().any(|p| p == app_path);
    let info = BundleInfo::read(app_path);
    let (true, Some(info)) = (in_app_dir, info) else {
        return report(
            UninstallStatus::NotAnApp,
            SkipReason::NotACandidate,
            SkipReason::AppNotRemoved,
        );
    };
    let signing = env.signing.read(app_path);
    if let Some(protection) = apps::protection(
        env,
        app_path,
        info.bundle_id.as_deref(),
        signing.as_ref().is_some_and(|s| s.platform),
    ) {
        return report(
            UninstallStatus::Refused { protection },
            SkipReason::Essential,
            SkipReason::AppNotRemoved,
        );
    }
    let running = env.running.snapshot();
    if apps::is_running(app_path, &running) {
        return report(
            UninstallStatus::QuitFirst,
            SkipReason::InUse {
                app: info.name.clone(),
            },
            SkipReason::AppNotRemoved,
        );
    }

    // Validate the leftovers while the bundle is still in place.
    let fresh = AppTarget::from_bundle(env, app_path).map(|target| {
        let index = AppIndex::build(env, cancel);
        leftovers::find(env, &target, &index, cancel, false)
    });
    if cancel.load(Ordering::Relaxed) {
        return report(
            UninstallStatus::Cancelled,
            SkipReason::Cancelled,
            SkipReason::Cancelled,
        );
    }

    progress(UninstallProgress { done: 0, total });
    let app = trash::move_to_trash(env, app_path, app_bytes, Zone::AppDirs(&env.app_dirs));
    if !app.moved() {
        return UninstallReport {
            status: UninstallStatus::AppNotMoved,
            app,
            leftovers: skip_all(SkipReason::AppNotRemoved),
        };
    }
    let leftovers = match fresh {
        Some(fresh) => {
            // `fresh.target_running` still guards the leftovers if a copy runs from elsewhere.
            leftovers::trash_validated(
                env,
                &fresh,
                selected,
                &mut |done, _| {
                    progress(UninstallProgress {
                        done: done + 1,
                        total,
                    })
                },
                cancel,
            )
        }
        // Without a bundle ID there is no leftover list to check against.
        None => skip_all(SkipReason::NotACandidate),
    };
    progress(UninstallProgress { done: total, total });
    UninstallReport {
        status: UninstallStatus::Done,
        app,
        leftovers,
    }
}
