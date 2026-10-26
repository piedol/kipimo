//! Chip power (CPU, GPU, Neural Engine) from IOReport's "Energy Model"
//! channels, sampled the way macmon does. IOReport is a private library, so it
//! is loaded at run time: if it or any symbol is missing, `PowerSampler::new`
//! returns `None` and the panel hides the value.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use serde::Serialize;

use crate::cf::{self, Dict};

type Ref = *const c_void;
type CopyChannelsFn = unsafe extern "C" fn(Ref, Ref, u64, u64, u64) -> Ref;
type SubscribeFn = unsafe extern "C" fn(Ref, Ref, *mut Ref, u64, Ref) -> Ref;
type SamplesFn = unsafe extern "C" fn(Ref, Ref, Ref) -> Ref;
type ChannelStrFn = unsafe extern "C" fn(Ref) -> Ref;
type IntegerFn = unsafe extern "C" fn(Ref, i32) -> i64;

struct Lib {
    copy_channels_in_group: CopyChannelsFn,
    create_subscription: SubscribeFn,
    create_samples: SamplesFn,
    create_samples_delta: SamplesFn,
    channel_get_group: ChannelStrFn,
    channel_get_name: ChannelStrFn,
    channel_get_unit: ChannelStrFn,
    simple_get_integer: IntegerFn,
}

unsafe extern "C" {
    fn CFDictionaryCreateMutableCopy(alloc: Ref, capacity: isize, dict: Ref) -> Ref;
    fn CFDictionarySetValue(dict: Ref, key: Ref, value: Ref);
    fn CFArrayCreateMutable(alloc: Ref, capacity: isize, callbacks: Ref) -> Ref;
    fn CFArrayAppendValue(array: Ref, value: Ref);
    static kCFTypeArrayCallBacks: c_void;
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        let path = c"/usr/lib/libIOReport.dylib";
        // SAFETY: dlopen/dlsym with valid C strings; the handle is kept for the process lifetime.
        unsafe {
            let h = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if h.is_null() {
                return None;
            }
            macro_rules! load {
                ($name:literal, $ty:ty) => {{
                    let p = libc::dlsym(h, $name.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $ty>(p)
                }};
            }
            Some(Lib {
                copy_channels_in_group: load!(c"IOReportCopyChannelsInGroup", CopyChannelsFn),
                create_subscription: load!(c"IOReportCreateSubscription", SubscribeFn),
                create_samples: load!(c"IOReportCreateSamples", SamplesFn),
                create_samples_delta: load!(c"IOReportCreateSamplesDelta", SamplesFn),
                channel_get_group: load!(c"IOReportChannelGetGroup", ChannelStrFn),
                channel_get_name: load!(c"IOReportChannelGetChannelName", ChannelStrFn),
                channel_get_unit: load!(c"IOReportChannelGetUnitLabel", ChannelStrFn),
                simple_get_integer: load!(c"IOReportSimpleGetIntegerValue", IntegerFn),
            })
        }
    })
    .as_ref()
}

/// Power in watts over the last sampling interval.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct ChipPower {
    pub cpu_watts: f32,
    pub gpu_watts: f32,
    pub ane_watts: f32,
}

impl ChipPower {
    pub fn total(&self) -> f32 {
        self.cpu_watts + self.gpu_watts + self.ane_watts
    }
}

/// Energy in joules from an IOReport value and its unit label.
pub(crate) fn joules(value: i64, unit: &str) -> Option<f64> {
    let scale = match unit.trim() {
        "J" => 1.0,
        "mJ" => 1e-3,
        "uJ" | "µJ" => 1e-6,
        "nJ" => 1e-9,
        _ => return None,
    };
    Some(value as f64 * scale)
}

/// Which bucket an Energy Model channel belongs to (names differ between chips:
/// "CPU Energy", "DIE_0_CPU Energy", "GPU Energy", "ANE0"…).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Bucket {
    Cpu,
    Gpu,
    Ane,
}

pub(crate) fn bucket(channel: &str) -> Option<Bucket> {
    if channel.ends_with("CPU Energy") {
        Some(Bucket::Cpu)
    } else if channel == "GPU Energy" || channel.ends_with("_GPU Energy") {
        Some(Bucket::Gpu)
    } else if channel.starts_with("ANE") {
        Some(Bucket::Ane)
    } else {
        None
    }
}

fn cfstr(p: Ref) -> String {
    // SAFETY: IOReport's getters return +0 CFStrings or null.
    unsafe { cf::borrowed::<CFString>(p) }
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A live IOReport subscription to the Energy Model group.
pub struct PowerSampler {
    lib: &'static Lib,
    sub: CFRetained<CFType>,
    channels: CFRetained<CFType>,
    prev: Option<(CFRetained<CFType>, Instant)>,
}

// SAFETY: the subscription and samples are CF objects only touched through `&mut self`.
unsafe impl Send for PowerSampler {}

impl PowerSampler {
    /// Subscribe and take a first sample. `None` if IOReport isn't available.
    pub fn new() -> Option<PowerSampler> {
        let lib = lib()?;
        let group = CFString::from_str("Energy Model");
        // SAFETY: IOReport calls with valid CF arguments; results are +1 or null.
        unsafe {
            let chans: CFRetained<CFType> = cf::owned((lib.copy_channels_in_group)(
                cf::ptr(&*group),
                std::ptr::null(),
                0,
                0,
                0,
            ))?;
            let channels: CFRetained<CFType> = cf::owned(CFDictionaryCreateMutableCopy(
                std::ptr::null(),
                0,
                cf::ptr(&*chans),
            ))?;
            // Keep only the channels we add up. The group has hundreds (per core,
            // per SRAM bank…) and sampling cost grows with the count.
            let all = Dict::from_value(&channels)
                .map(|d| d.array("IOReportChannels"))
                .unwrap_or_default();
            let wanted: Vec<_> = all
                .iter()
                .filter(|c| bucket(&cfstr((lib.channel_get_name)(cf::ptr(&***c)))).is_some())
                .collect();
            if !wanted.is_empty() {
                let arr: CFRetained<CFType> = cf::owned(CFArrayCreateMutable(
                    std::ptr::null(),
                    0,
                    (&raw const kCFTypeArrayCallBacks).cast(),
                ))?;
                for c in &wanted {
                    CFArrayAppendValue(cf::ptr(&*arr), cf::ptr(&***c));
                }
                let key = CFString::from_str("IOReportChannels");
                CFDictionarySetValue(cf::ptr(&*channels), cf::ptr(&*key), cf::ptr(&*arr));
            }
            let mut subbed: Ref = std::ptr::null();
            let sub: CFRetained<CFType> = cf::owned((lib.create_subscription)(
                std::ptr::null(),
                cf::ptr(&*channels),
                &mut subbed,
                0,
                std::ptr::null(),
            ))?;
            // The "subscribed channels" out-parameter is +1 too.
            drop(cf::owned::<CFType>(subbed));
            let mut s = PowerSampler {
                lib,
                sub,
                channels,
                prev: None,
            };
            s.prev = s.take().map(|v| (v, Instant::now()));
            s.prev.as_ref()?;
            Some(s)
        }
    }

    fn take(&self) -> Option<CFRetained<CFType>> {
        // SAFETY: valid subscription and channel dictionary; the sample is +1.
        unsafe {
            cf::owned((self.lib.create_samples)(
                cf::ptr(&*self.sub),
                cf::ptr(&*self.channels),
                std::ptr::null(),
            ))
        }
    }

    /// Average power since the previous call (or since `new`).
    pub fn sample(&mut self) -> Option<ChipPower> {
        let cur = self.take()?;
        let now = Instant::now();
        let (prev, at) = self.prev.replace((cur.clone(), now))?;
        let secs = now.duration_since(at).as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        // SAFETY: two samples from the same subscription; the delta is +1.
        let delta: CFRetained<CFDictionary> = unsafe {
            cf::owned((self.lib.create_samples_delta)(
                cf::ptr(&*prev),
                cf::ptr(&*cur),
                std::ptr::null(),
            ))
        }?;
        let mut p = ChipPower::default();
        for item in Dict(delta).array("IOReportChannels") {
            let ch = cf::ptr(&*item);
            // SAFETY: each element is an IOReport channel dictionary.
            let (group, name, unit, value) = unsafe {
                (
                    cfstr((self.lib.channel_get_group)(ch)),
                    cfstr((self.lib.channel_get_name)(ch)),
                    cfstr((self.lib.channel_get_unit)(ch)),
                    (self.lib.simple_get_integer)(ch, 0),
                )
            };
            if group != "Energy Model" {
                continue;
            }
            let (Some(b), Some(j)) = (bucket(&name), joules(value, &unit)) else {
                continue;
            };
            let w = (j / secs) as f32;
            match b {
                Bucket::Cpu => p.cpu_watts += w,
                Bucket::Gpu => p.gpu_watts += w,
                Bucket::Ane => p.ane_watts += w,
            }
        }
        Some(p)
    }

    /// Every Energy Model channel name with its unit, for checking a new chip.
    pub fn channel_names(&self) -> Vec<String> {
        let Some(s) = self.take() else {
            return Vec::new();
        };
        let Some(d) = Dict::from_value(&s) else {
            return Vec::new();
        };
        d.array("IOReportChannels")
            .iter()
            .map(|item| {
                let ch = cf::ptr(&**item);
                // SAFETY: each element is an IOReport channel dictionary.
                unsafe {
                    format!(
                        "{} [{}]",
                        cfstr((self.lib.channel_get_name)(ch)),
                        cfstr((self.lib.channel_get_unit)(ch))
                    )
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_units() {
        assert_eq!(joules(1500, "mJ"), Some(1.5));
        assert_eq!(joules(2_000_000, "uJ"), Some(2.0));
        assert_eq!(joules(3_000_000_000, "nJ"), Some(3.0));
        assert_eq!(joules(1, "W"), None);
    }

    #[test]
    fn channel_buckets() {
        assert_eq!(bucket("CPU Energy"), Some(Bucket::Cpu));
        assert_eq!(bucket("DIE_1_CPU Energy"), Some(Bucket::Cpu));
        assert_eq!(bucket("GPU Energy"), Some(Bucket::Gpu));
        assert_eq!(bucket("ANE0"), Some(Bucket::Ane));
        assert_eq!(bucket("EACC_CPU0"), None);
        assert_eq!(bucket("GPU0"), None);
    }
}
