//! The System Management Controller, read the way macmon does: chip
//! temperatures from the `Tp`/`Te` (CPU) and `Tg` (GPU) sensor keys, and total
//! system power from `PSTR`. The SMC's key interface is undocumented, so every
//! read is optional and the panel hides a value it can't get.

use std::sync::OnceLock;

use serde::Serialize;

use crate::iokit::{
    self, IOConnectCallStructMethod, IOServiceClose, IOServiceOpen, io_object_t, task_self,
};

const KERNEL_INDEX_SMC: u32 = 2;
const CMD_READ_BYTES: u8 = 5;
const CMD_READ_INDEX: u8 = 8;
const CMD_READ_KEYINFO: u8 = 9;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyInfo {
    size: u32,
    ty: u32,
    attr: u8,
}

/// `SMCKeyData_t`, the 80-byte struct the AppleSMC user client exchanges.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyData {
    key: u32,
    vers: Vers,
    plimit: PLimit,
    info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

pub(crate) fn fourcc(s: &str) -> u32 {
    let b = s.as_bytes();
    if b.len() != 4 {
        return 0;
    }
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn fourcc_str(k: u32) -> String {
    String::from_utf8_lossy(&k.to_be_bytes()).into_owned()
}

/// Decode an SMC value to a number by its type code.
pub(crate) fn decode(ty: &str, bytes: &[u8]) -> Option<f64> {
    let be16 = || bytes.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    match ty {
        // Apple silicon stores floats little-endian.
        "flt " => bytes
            .get(..4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        "sp78" => be16().map(|v| v as i16 as f64 / 256.0),
        "fpe2" => be16().map(|v| v as f64 / 4.0),
        "ui8 " => bytes.first().map(|&v| v as f64),
        "ui16" => be16().map(|v| v as f64),
        "ui32" => bytes
            .get(..4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64),
        _ => None,
    }
}

/// Each SMC read is a round trip to the SMC firmware (about 0.15 ms on an M5
/// Max), and big chips have 100+ sensors, so only an even spread of them is read.
const MAX_SENSORS: usize = 8;

/// At most `n` items, evenly spread over the list.
pub(crate) fn spread<T: Copy>(v: Vec<T>, n: usize) -> Vec<T> {
    if v.len() <= n || n == 0 {
        return if n == 0 { Vec::new() } else { v };
    }
    (0..n).map(|i| v[i * v.len() / n]).collect()
}

/// Sensor keys with their type info.
type Sensors = Vec<(u32, KeyInfo)>;

/// An open connection to the SMC.
pub struct Smc {
    conn: io_object_t,
}

// SAFETY: an IOKit connection handle is a plain port name usable from any thread.
unsafe impl Send for Smc {}
unsafe impl Sync for Smc {}

impl Drop for Smc {
    fn drop(&mut self) {
        // SAFETY: we own the connection.
        unsafe { IOServiceClose(self.conn) };
    }
}

/// Chip temperatures in °C.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct Temperatures {
    /// Average over the CPU sensors (what macmon reports as CPU temperature).
    pub cpu_avg: f32,
    pub cpu_max: f32,
    pub gpu_avg: Option<f32>,
}

impl Smc {
    pub fn open() -> Option<Smc> {
        let svc = iokit::service("AppleSMC")?;
        let mut conn: io_object_t = 0;
        // SAFETY: valid service and out pointer.
        let kr = unsafe { IOServiceOpen(svc.0, task_self(), 0, &mut conn) };
        (kr == 0 && conn != 0).then_some(Smc { conn })
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut size = std::mem::size_of::<KeyData>();
        // SAFETY: both buffers are KeyData-sized.
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                (input as *const KeyData).cast(),
                std::mem::size_of::<KeyData>(),
                (&mut out as *mut KeyData).cast(),
                &mut size,
            )
        };
        (kr == 0 && out.result == 0).then_some(out)
    }

    fn key_info(&self, key: u32) -> Option<KeyInfo> {
        self.call(&KeyData {
            key,
            data8: CMD_READ_KEYINFO,
            ..KeyData::default()
        })
        .map(|o| o.info)
    }

    fn read_with(&self, key: u32, info: KeyInfo) -> Option<f64> {
        let out = self.call(&KeyData {
            key,
            info,
            data8: CMD_READ_BYTES,
            ..KeyData::default()
        })?;
        let len = (info.size as usize).min(32);
        decode(&fourcc_str(info.ty), &out.bytes[..len])
    }

    /// Read a key as a number, e.g. `PSTR`.
    pub fn read(&self, key: &str) -> Option<f64> {
        let k = fourcc(key);
        self.read_with(k, self.key_info(k)?)
    }

    fn key_at(&self, index: u32) -> Option<u32> {
        self.call(&KeyData {
            data8: CMD_READ_INDEX,
            data32: index,
            ..KeyData::default()
        })
        .map(|o| o.key)
    }

    /// Temperature sensor keys (CPU, GPU), found once by walking the key list.
    fn sensors(&self) -> &'static (Sensors, Sensors) {
        static SENSORS: OnceLock<(Sensors, Sensors)> = OnceLock::new();
        SENSORS.get_or_init(|| {
            let (mut cpu, mut gpu) = (Vec::new(), Vec::new());
            let count = self.read("#KEY").unwrap_or(0.0) as u32;
            for i in 0..count.min(10_000) {
                let Some(k) = self.key_at(i) else { continue };
                let name = fourcc_str(k);
                let is_cpu = name.starts_with("Tp") || name.starts_with("Te");
                let is_gpu = name.starts_with("Tg");
                if !(is_cpu || is_gpu) {
                    continue;
                }
                let Some(info) = self.key_info(k) else {
                    continue;
                };
                if fourcc_str(info.ty) != "flt " {
                    continue;
                }
                if is_cpu {
                    cpu.push((k, info))
                } else {
                    gpu.push((k, info))
                }
            }
            (spread(cpu, MAX_SENSORS), spread(gpu, MAX_SENSORS / 2))
        })
    }

    /// Current chip temperatures, or `None` if this Mac exposes no sensors this way.
    pub fn temperatures(&self) -> Option<Temperatures> {
        let (cpu, gpu) = self.sensors();
        let read = |keys: &[(u32, KeyInfo)]| -> Vec<f32> {
            keys.iter()
                .filter_map(|&(k, i)| self.read_with(k, i))
                .map(|v| v as f32)
                .filter(|v| *v > 0.0 && *v < 150.0)
                .collect()
        };
        let c = read(cpu);
        if c.is_empty() {
            return None;
        }
        let g = read(gpu);
        Some(Temperatures {
            cpu_avg: c.iter().sum::<f32>() / c.len() as f32,
            cpu_max: c.iter().copied().fold(f32::MIN, f32::max),
            gpu_avg: (!g.is_empty()).then(|| g.iter().sum::<f32>() / g.len() as f32),
        })
    }

    /// Total system power in watts (`PSTR`).
    pub fn system_power(&self) -> Option<f32> {
        static INFO: OnceLock<Option<KeyInfo>> = OnceLock::new();
        let k = fourcc("PSTR");
        let info = (*INFO.get_or_init(|| self.key_info(k)))?;
        self.read_with(k, info)
            .map(|v| v as f32)
            .filter(|v| *v > 0.0 && *v < 2000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_is_80_bytes() {
        assert_eq!(std::mem::size_of::<KeyData>(), 80);
    }

    #[test]
    fn fourcc_round_trip() {
        assert_eq!(fourcc("PSTR"), 0x5053_5452);
        assert_eq!(fourcc_str(fourcc("Tp0C")), "Tp0C");
        assert_eq!(fourcc("bad"), 0);
    }

    #[test]
    fn spreads_sensors_evenly() {
        assert_eq!(spread((0..23).collect(), 4), vec![0, 5, 11, 17]);
        assert_eq!(spread(vec![1, 2], 12), vec![1, 2]);
        assert!(spread(vec![1, 2], 0).is_empty());
    }

    #[test]
    fn decodes_types() {
        assert_eq!(decode("flt ", &45.5f32.to_le_bytes()), Some(45.5));
        // sp78: 0x2D80 = 45.5
        assert_eq!(decode("sp78", &[0x2d, 0x80]), Some(45.5));
        assert_eq!(decode("sp78", &[0xff, 0x00]), Some(-1.0));
        assert_eq!(decode("fpe2", &[0x00, 0x0a]), Some(2.5));
        assert_eq!(decode("ui32", &[0, 0, 0x0e, 0x55]), Some(3669.0));
        assert_eq!(decode("ui8 ", &[7]), Some(7.0));
        assert_eq!(decode("flag", &[1]), None);
        assert_eq!(decode("flt ", &[1, 2]), None);
    }
}
