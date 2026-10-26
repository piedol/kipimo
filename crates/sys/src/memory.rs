//! Memory: pressure, and used / app / wired / compressed / cached / swap the way
//! Activity Monitor counts them. `PressureWatcher` wraps the memory-pressure
//! dispatch source, so the app hears about pressure changes without polling.

use std::ffi::{CString, c_void};

use block2::RcBlock;
use dispatch2::{
    DispatchObject, DispatchQueue, DispatchRetained, DispatchSource, dispatch_source_type_t,
};
use serde::Serialize;

use crate::iokit::kern_return_t;

const HOST_VM_INFO64: i32 = 4;

/// `struct vm_statistics64` from <mach/vm_statistics.h>.
#[repr(C, align(8))]
#[derive(Default, Clone, Copy)]
pub(crate) struct VmStats {
    pub free_count: u32,
    pub active_count: u32,
    pub inactive_count: u32,
    pub wire_count: u32,
    pub zero_fill_count: u64,
    pub reactivations: u64,
    pub pageins: u64,
    pub pageouts: u64,
    pub faults: u64,
    pub cow_faults: u64,
    pub lookups: u64,
    pub hits: u64,
    pub purges: u64,
    pub purgeable_count: u32,
    pub speculative_count: u32,
    pub decompressions: u64,
    pub compressions: u64,
    pub swapins: u64,
    pub swapouts: u64,
    pub compressor_page_count: u32,
    pub throttled_count: u32,
    pub external_page_count: u32,
    pub internal_page_count: u32,
    pub total_uncompressed_pages_in_compressor: u64,
}

/// `struct xsw_usage` from <sys/sysctl.h>.
#[repr(C)]
#[derive(Default)]
struct SwapUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: i32,
}

unsafe extern "C" {
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> kern_return_t;
}

/// macOS's own three-step assessment (`kern.memorystatus_vm_pressure_level`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum PressureLevel {
    Normal,
    Warning,
    Critical,
}

impl PressureLevel {
    pub(crate) fn from_kernel(v: i32) -> PressureLevel {
        match v {
            4 => PressureLevel::Critical,
            2 => PressureLevel::Warning,
            _ => PressureLevel::Normal,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Memory {
    pub total: u64,
    /// App + wired + compressed (Activity Monitor's "Memory Used").
    pub used: u64,
    pub app: u64,
    pub wired: u64,
    pub compressed: u64,
    /// File cache macOS can drop at once (Activity Monitor's "Cached Files").
    pub cached: u64,
    pub free: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// 0–100: how hard macOS is working to find memory (100 minus the kernel's
    /// free-memory level, the number `memory_pressure` prints).
    pub pressure_percent: u8,
    pub level: Option<PressureLevel>,
}

pub(crate) fn sysctl_raw(name: &str, out: *mut c_void, size: usize) -> Option<usize> {
    let c = CString::new(name).ok()?;
    let mut len = size;
    // SAFETY: `out` holds `size` bytes.
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), out, &mut len, std::ptr::null_mut(), 0) };
    (r == 0).then_some(len)
}

pub(crate) fn sysctl_i32(name: &str) -> Option<i32> {
    let mut v: i32 = 0;
    sysctl_raw(name, (&mut v as *mut i32).cast(), 4).map(|_| v)
}

pub(crate) fn sysctl_u64(name: &str) -> Option<u64> {
    let mut v: u64 = 0;
    match sysctl_raw(name, (&mut v as *mut u64).cast(), 8)? {
        4 => Some(v & 0xffff_ffff),
        _ => Some(v),
    }
}

pub(crate) fn sysctl_string(name: &str) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = sysctl_raw(name, buf.as_mut_ptr().cast(), buf.len())?;
    let s = &buf[..n];
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    Some(String::from_utf8_lossy(&s[..end]).trim().to_string())
}

fn vm_stats() -> Option<VmStats> {
    let mut s = VmStats::default();
    let mut count = (std::mem::size_of::<VmStats>() / std::mem::size_of::<i32>()) as u32;
    // SAFETY: the buffer is a vm_statistics64 and `count` is its size in integers.
    let kr = unsafe {
        host_statistics64(
            crate::cpu::host(),
            HOST_VM_INFO64,
            (&mut s as *mut VmStats).cast(),
            &mut count,
        )
    };
    (kr == 0).then_some(s)
}

fn page_size() -> u64 {
    // SAFETY: no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as u64 } else { 16384 }
}

/// Activity Monitor's breakdown from raw page counts.
pub(crate) fn breakdown(s: &VmStats, page: u64, total: u64) -> Memory {
    let pages = |n: u32| n as u64 * page;
    let app = pages(s.internal_page_count.saturating_sub(s.purgeable_count));
    let wired = pages(s.wire_count);
    let compressed = pages(s.compressor_page_count);
    let cached = pages(s.external_page_count) + pages(s.purgeable_count);
    let used = app + wired + compressed;
    Memory {
        total,
        used,
        app,
        wired,
        compressed,
        cached,
        free: total.saturating_sub(used + cached),
        ..Memory::default()
    }
}

/// Current kernel pressure level.
pub fn pressure_level() -> Option<PressureLevel> {
    sysctl_i32("kern.memorystatus_vm_pressure_level").map(PressureLevel::from_kernel)
}

/// A memory snapshot. `None` only if the kernel refuses the statistics call.
pub fn memory() -> Option<Memory> {
    let total = sysctl_u64("hw.memsize").unwrap_or(0);
    let mut m = breakdown(&vm_stats()?, page_size(), total);
    let mut swap = SwapUsage::default();
    if sysctl_raw(
        "vm.swapusage",
        (&mut swap as *mut SwapUsage).cast(),
        std::mem::size_of::<SwapUsage>(),
    )
    .is_some()
    {
        m.swap_used = swap.used;
        m.swap_total = swap.total;
    }
    if let Some(free_level) = sysctl_i32("kern.memorystatus_level") {
        m.pressure_percent = (100 - free_level.clamp(0, 100)) as u8;
    }
    m.level = pressure_level();
    Some(m)
}

/// Calls back whenever macOS's memory-pressure level changes (normal, warning,
/// critical), from the memory-pressure dispatch source. Stops when dropped.
pub struct PressureWatcher {
    source: DispatchRetained<DispatchSource>,
}

// SAFETY: dispatch sources are thread-safe objects.
unsafe impl Send for PressureWatcher {}

impl PressureWatcher {
    /// Start watching. `on_change` runs on `queue` with the new level.
    pub fn start(
        queue: &DispatchQueue,
        on_change: impl Fn(PressureLevel) + 'static,
    ) -> PressureWatcher {
        let mask = dispatch2::dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_NORMAL
            .0
            | dispatch2::dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_WARN.0
            | dispatch2::dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_CRITICAL.0;
        // SAFETY: the memory-pressure source type takes no handle and these mask bits.
        let source = unsafe {
            let ty: dispatch_source_type_t =
                (&raw const dispatch2::_dispatch_source_type_memorypressure).cast_mut();
            DispatchSource::new(ty, 0, mask as usize, Some(queue))
        };
        let block = RcBlock::new(move || {
            on_change(pressure_level().unwrap_or(PressureLevel::Normal));
        });
        // SAFETY: dispatch copies the block, so it may be dropped after this call.
        unsafe { source.set_event_handler_with_block(RcBlock::as_ptr(&block)) };
        source.resume();
        PressureWatcher { source }
    }
}

impl Drop for PressureWatcher {
    fn drop(&mut self) {
        self.source.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_matches_kernel_size() {
        // HOST_VM_INFO64_COUNT is 38 integers.
        assert_eq!(std::mem::size_of::<VmStats>(), 152);
        assert_eq!(std::mem::size_of::<SwapUsage>(), 32);
    }

    #[test]
    fn breakdown_like_activity_monitor() {
        let s = VmStats {
            internal_page_count: 1000,
            purgeable_count: 100,
            wire_count: 200,
            compressor_page_count: 50,
            external_page_count: 300,
            ..VmStats::default()
        };
        let m = breakdown(&s, 16384, 4000 * 16384);
        assert_eq!(m.app, 900 * 16384);
        assert_eq!(m.wired, 200 * 16384);
        assert_eq!(m.compressed, 50 * 16384);
        assert_eq!(m.used, 1150 * 16384);
        assert_eq!(m.cached, 400 * 16384);
        assert_eq!(m.free, (4000 - 1150 - 400) * 16384);
    }

    #[test]
    fn levels_from_kernel_values() {
        assert_eq!(PressureLevel::from_kernel(1), PressureLevel::Normal);
        assert_eq!(PressureLevel::from_kernel(2), PressureLevel::Warning);
        assert_eq!(PressureLevel::from_kernel(4), PressureLevel::Critical);
        assert!(PressureLevel::Critical > PressureLevel::Warning);
    }

    #[test]
    fn live_memory() {
        let m = memory().expect("host_statistics64 works");
        assert!(m.total > 0 && m.used > 0 && m.used <= m.total);
        assert!(m.pressure_percent <= 100);
        assert!(m.level.is_some());
    }

    #[test]
    fn watcher_starts_and_stops() {
        let q = DispatchQueue::new("kipimo.test.pressure", None);
        let w = PressureWatcher::start(&q, |_| {});
        drop(w);
    }
}
