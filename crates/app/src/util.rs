//! Small helpers shared across modules.

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_foundation::NSString;

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("UI work must run on the main thread")
}

/// Run `f` on the main queue soon.
pub fn main_async(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(f);
}

/// Run `f` on the main queue after `secs` seconds.
pub fn main_after(secs: f64, f: impl FnOnce() + Send + 'static) {
    let when = DispatchTime::NOW.time((secs * 1_000_000_000.0) as i64);
    let _ = DispatchQueue::main().after(when, f);
}

/// Run `f` on a background (utility) queue.
pub fn background(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::global_queue(dispatch2::GlobalQueueIdentifier::QualityOfService(
        dispatch2::DispatchQoS::Utility,
    ))
    .exec_async(f);
}

pub fn log(msg: impl AsRef<str>) {
    if cfg!(debug_assertions) || std::env::var_os("KIPIMO_LOG").is_some() {
        eprintln!("[kipimo] {}", msg.as_ref());
    }
}

unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

/// Hand freed memory back to the system after the panel or a window closes,
/// so the idle footprint returns to baseline instead of staying at the peak.
pub fn release_memory_soon() {
    main_after(1.0, || {
        // SAFETY: a null zone means "all zones"; goal 0 means "as much as possible".
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    });
}
