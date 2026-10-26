//! CPU load (total and per core, from `host_processor_info`) and the busiest
//! processes (from `proc_pidinfo`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;

use crate::iokit::{kern_return_t, task_self};

const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_host_self() -> u32;
    fn host_processor_info(
        host: u32,
        flavor: i32,
        out_count: *mut u32,
        out_info: *mut *mut i32,
        out_info_count: *mut u32,
    ) -> kern_return_t;
    fn vm_deallocate(task: u32, addr: usize, size: usize) -> kern_return_t;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> kern_return_t;
}

/// The host port, fetched once (each `mach_host_self` call adds a send right).
pub(crate) fn host() -> u32 {
    static HOST: OnceLock<u32> = OnceLock::new();
    // SAFETY: no preconditions.
    *HOST.get_or_init(|| unsafe { mach_host_self() })
}

/// CPU use since the previous sample, in percent of all cores.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct CpuLoad {
    /// 0–100, averaged over every core.
    pub total: f32,
    pub user: f32,
    pub system: f32,
    /// 0–100 for each logical core, in the kernel's order (on Apple silicon the
    /// efficiency cores, if any, come first).
    pub per_core: Vec<f32>,
}

/// Ticks per core: user, system, idle, nice.
pub(crate) type Ticks = [u32; 4];

fn read_ticks() -> Option<Vec<Ticks>> {
    let mut count: u32 = 0;
    let mut info: *mut i32 = std::ptr::null_mut();
    let mut info_count: u32 = 0;
    // SAFETY: valid out pointers; the kernel allocates `info` in our address space.
    let kr = unsafe {
        host_processor_info(
            host(),
            PROCESSOR_CPU_LOAD_INFO,
            &mut count,
            &mut info,
            &mut info_count,
        )
    };
    if kr != 0 || info.is_null() {
        return None;
    }
    // SAFETY: `info_count` integers were written at `info`.
    let raw = unsafe { std::slice::from_raw_parts(info, info_count as usize) };
    let ticks = raw
        .as_chunks::<4>()
        .0
        .iter()
        .take(count as usize)
        .map(|c| [c[0] as u32, c[1] as u32, c[2] as u32, c[3] as u32])
        .collect();
    // SAFETY: give the kernel's buffer back.
    unsafe {
        vm_deallocate(
            task_self(),
            info as usize,
            info_count as usize * std::mem::size_of::<i32>(),
        )
    };
    Some(ticks)
}

/// Load between two tick snapshots. Counters may wrap; the difference is taken modulo 2^32.
pub(crate) fn load_between(prev: &[Ticks], cur: &[Ticks]) -> CpuLoad {
    let mut per_core = Vec::with_capacity(cur.len());
    let (mut user, mut system, mut busy, mut all) = (0u64, 0u64, 0u64, 0u64);
    for (p, c) in prev.iter().zip(cur) {
        let d = |i: usize| c[i].wrapping_sub(p[i]) as u64;
        let u = d(CPU_STATE_USER) + d(CPU_STATE_NICE);
        let s = d(CPU_STATE_SYSTEM);
        let total = u + s + d(CPU_STATE_IDLE);
        per_core.push(if total == 0 {
            0.0
        } else {
            (u + s) as f32 * 100.0 / total as f32
        });
        user += u;
        system += s;
        busy += u + s;
        all += total;
    }
    let pct = |v: u64| {
        if all == 0 {
            0.0
        } else {
            v as f32 * 100.0 / all as f32
        }
    };
    CpuLoad {
        total: pct(busy),
        user: pct(user),
        system: pct(system),
        per_core,
    }
}

/// Keeps the previous tick snapshot so each `sample` reports load since the last one.
pub struct CpuSampler {
    prev: Option<Vec<Ticks>>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    /// Takes a first snapshot now, so the first `sample` covers the time since creation.
    pub fn new() -> Self {
        CpuSampler { prev: read_ticks() }
    }

    /// Load since the previous call (or since `new`). `None` if the kernel call fails.
    pub fn sample(&mut self) -> Option<CpuLoad> {
        let cur = read_ticks()?;
        let load = self
            .prev
            .as_ref()
            .filter(|p| p.len() == cur.len())
            .map(|p| load_between(p, &cur));
        self.prev = Some(cur);
        load
    }
}

/// Number of logical cores.
pub fn core_count() -> usize {
    read_ticks().map(|t| t.len()).unwrap_or(0)
}

// ── Processes ──────────────────────────────────────────────────────────────

/// One process's CPU use since the previous sample. 100% is one full core, as
/// in Activity Monitor, so a busy multithreaded process can exceed 100.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ProcessCpu {
    pub pid: i32,
    pub name: String,
    pub percent: f32,
}

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = MachTimebaseInfo::default();
        // SAFETY: valid out pointer.
        unsafe { mach_timebase_info(&mut info) };
        if info.denom == 0 {
            (1, 1)
        } else {
            (info.numer as u64, info.denom as u64)
        }
    })
}

fn all_pids() -> Vec<i32> {
    // SAFETY: a null buffer asks for the count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
    // SAFETY: the buffer holds `bytes` bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    pids.truncate(n.max(0) as usize);
    pids.retain(|&p| p > 0);
    pids
}

/// Total CPU time of a process in nanoseconds.
fn cpu_time_ns(pid: i32) -> Option<u64> {
    let (numer, denom) = timebase();
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: the buffer is a proc_taskinfo.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if n == size {
        // Mach absolute time units; convert to nanoseconds.
        let ticks = info.pti_total_user + info.pti_total_system;
        return Some((ticks as u128 * numer as u128 / denom as u128) as u64);
    }
    // Other users' processes refuse PROC_PIDTASKINFO; rusage works for more of them.
    let mut ru: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a rusage_info_v2, matching the flavour.
    let r = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            (&mut ru as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
        )
    };
    if r == 0 {
        let ticks = ru.ri_user_time + ru.ri_system_time;
        return Some((ticks as u128 * numer as u128 / denom as u128) as u64);
    }
    None
}

fn pid_path(pid: i32) -> Option<std::path::PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer holds `len` bytes.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n as usize]))
    })
}

/// A readable process name: the executable's file name (not truncated like `p_comm`).
pub fn process_name(pid: i32) -> String {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer holds `len` bytes.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n > 0 {
        let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        if let Some(name) = path.rsplit('/').next().filter(|s| !s.is_empty()) {
            return name.to_string();
        }
    }
    let mut buf = [0u8; 64];
    // SAFETY: the buffer holds 64 bytes.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    } else {
        format!("pid {pid}")
    }
}

/// Rank processes by CPU time used between two snapshots (ns per pid).
pub(crate) fn rank(
    prev: &HashMap<i32, u64>,
    cur: &HashMap<i32, u64>,
    wall_ns: u64,
    n: usize,
) -> Vec<(i32, f32)> {
    if wall_ns == 0 {
        return Vec::new();
    }
    let mut v: Vec<(i32, f32)> = cur
        .iter()
        .filter_map(|(pid, &t)| {
            let p = *prev.get(pid)?;
            let d = t.checked_sub(p)?;
            Some((*pid, (d as f64 * 100.0 / wall_ns as f64) as f32))
        })
        .filter(|(_, pct)| *pct > 0.0)
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

/// Keeps each process's CPU time from the previous sample.
pub struct ProcessSampler {
    prev: HashMap<i32, u64>,
    at: Instant,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        ProcessSampler {
            prev: Self::snapshot(),
            at: Instant::now(),
        }
    }

    fn snapshot() -> HashMap<i32, u64> {
        all_pids()
            .into_iter()
            .filter_map(|p| cpu_time_ns(p).map(|t| (p, t)))
            .collect()
    }

    /// The `n` busiest processes since the previous call, leaving out this app itself (this
    /// process and others started from the same executable, like Kipimo's tools window).
    pub fn top(&mut self, n: usize) -> Vec<ProcessCpu> {
        let cur = Self::snapshot();
        let now = Instant::now();
        let wall = now.duration_since(self.at).as_nanos() as u64;
        let ranked = rank(&self.prev, &cur, wall, n + 4);
        self.prev = cur;
        self.at = now;
        let me = std::process::id() as i32;
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(p).ok());
        ranked
            .into_iter()
            .filter(|(pid, _)| *pid != me && (exe.is_none() || pid_path(*pid) != exe))
            .take(n)
            .map(|(pid, percent)| ProcessCpu {
                pid,
                name: process_name(pid),
                percent,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_ticks() {
        let prev = vec![[100, 50, 850, 0], [0, 0, 1000, 0]];
        let cur = vec![[160, 70, 870, 0], [0, 0, 1100, 0]];
        let l = load_between(&prev, &cur);
        // Core 0: 80 busy of 100. Core 1: idle.
        assert_eq!(l.per_core, vec![80.0, 0.0]);
        assert!((l.total - 40.0).abs() < 1e-4);
        assert!((l.user - 30.0).abs() < 1e-4);
        assert!((l.system - 10.0).abs() < 1e-4);
    }

    #[test]
    fn load_survives_counter_wrap() {
        let prev = vec![[u32::MAX - 9, 0, u32::MAX - 9, 0]];
        let cur = vec![[10, 0, 10, 0]];
        let l = load_between(&prev, &cur);
        assert_eq!(l.per_core, vec![50.0]);
    }

    #[test]
    fn load_with_no_elapsed_ticks_is_zero() {
        let t = vec![[5, 5, 5, 5]];
        assert_eq!(load_between(&t, &t).total, 0.0);
    }

    #[test]
    fn ranks_busiest_first() {
        let prev = HashMap::from([(1, 0), (2, 1_000), (3, 0), (4, 500)]);
        let cur = HashMap::from([
            (1, 500_000_000),
            (2, 1_000),
            (3, 2_000_000_000),
            (4, 250_000_500),
            (5, 9_000_000_000),
        ]);
        // Over one second: pid 3 used two cores, pid 1 half a core, pid 4 a quarter.
        let r = rank(&prev, &cur, 1_000_000_000, 2);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, 3);
        assert!((r[0].1 - 200.0).abs() < 1e-3);
        assert_eq!(r[1].0, 1);
        // New pids (5) and idle ones (2) are left out.
        assert!(
            rank(&prev, &cur, 1_000_000_000, 10)
                .iter()
                .all(|(p, _)| *p != 5 && *p != 2)
        );
    }

    #[test]
    fn live_cpu_sample() {
        let mut s = CpuSampler::new();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let l = s.sample().expect("host_processor_info works");
        assert_eq!(l.per_core.len(), core_count());
        assert!((0.0..=100.0).contains(&l.total));
    }
}
