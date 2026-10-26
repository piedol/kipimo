//! Network upload and download rates from the kernel's 64-bit interface
//! counters (`sysctl NET_RT_IFLIST2`).

use std::time::Instant;

use serde::Serialize;

const RTM_IFINFO2: u8 = 0x12;
const IFF_UP: i32 = 0x1;
const IFF_LOOPBACK: i32 = 0x8;
const IFT_ETHER: u8 = 0x06;
const IFT_CELLULAR: u8 = 0xff;
/// `sizeof(struct if_msghdr2)`: a 32-byte header plus `struct if_data64` (128 bytes).
const MSGHDR2_LEN: usize = 160;
const DATA_OFFSET: usize = 32;

/// One interface's counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IfCounters {
    pub name: String,
    pub index: u16,
    pub flags: i32,
    pub if_type: u8,
    pub ibytes: u64,
    pub obytes: u64,
}

impl IfCounters {
    /// Physical traffic only: Wi-Fi, Ethernet and cellular. Loopback, VPN
    /// tunnels and bridges would count the same bytes twice; AWDL and
    /// low-latency WLAN are AirDrop's private links.
    fn counts(&self) -> bool {
        self.flags & IFF_UP != 0
            && self.flags & IFF_LOOPBACK == 0
            && matches!(self.if_type, IFT_ETHER | IFT_CELLULAR)
            && !["awdl", "llw", "anpi", "bridge", "ap"]
                .iter()
                .any(|p| self.name.starts_with(p))
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([b[o], b[o + 1]])
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_ne_bytes(b[o..o + 8].try_into().unwrap())
}

/// Parse the `NET_RT_IFLIST2` routing-message buffer. Each `RTM_IFINFO2`
/// message is an `if_msghdr2` followed by the interface's `sockaddr_dl`.
pub(crate) fn parse_iflist2(buf: &[u8]) -> Vec<IfCounters> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 4 <= buf.len() {
        let len = u16_at(buf, off) as usize;
        if len == 0 || off + len > buf.len() {
            break;
        }
        let msg = &buf[off..off + len];
        if msg[3] == RTM_IFINFO2 && len >= MSGHDR2_LEN {
            let data = &msg[DATA_OFFSET..];
            // sockaddr_dl: len, family, index(2), type, nlen, alen, slen, data[…]
            let sdl = &msg[MSGHDR2_LEN..];
            let name = if sdl.len() >= 8 {
                let nlen = sdl[5] as usize;
                String::from_utf8_lossy(&sdl[8..(8 + nlen).min(sdl.len())]).into_owned()
            } else {
                String::new()
            };
            out.push(IfCounters {
                name,
                flags: i32_at(msg, 8),
                index: u16_at(msg, 12),
                if_type: data[0],
                ibytes: u64_at(data, 64),
                obytes: u64_at(data, 72),
            });
        }
        off += len;
    }
    out
}

fn read_iflist2() -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len: usize = 0;
    // SAFETY: a null buffer asks for the size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            6,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; len + 2048];
    let mut len = buf.len();
    // SAFETY: the buffer holds `len` bytes.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            6,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// Bytes received and sent since boot on physical interfaces.
pub fn totals() -> Option<(u64, u64)> {
    let list = parse_iflist2(&read_iflist2()?);
    Some(
        list.iter()
            .filter(|c| c.counts())
            .fold((0, 0), |(i, o), c| (i + c.ibytes, o + c.obytes)),
    )
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct NetRates {
    pub down_bytes_per_sec: f64,
    pub up_bytes_per_sec: f64,
}

/// Rates between two counter snapshots. A counter that went backwards (an
/// interface went away) counts as zero for that interval.
pub(crate) fn rates(prev: (u64, u64), cur: (u64, u64), secs: f64) -> NetRates {
    if secs <= 0.0 {
        return NetRates::default();
    }
    NetRates {
        down_bytes_per_sec: cur.0.saturating_sub(prev.0) as f64 / secs,
        up_bytes_per_sec: cur.1.saturating_sub(prev.1) as f64 / secs,
    }
}

/// Keeps the previous counters so each `sample` reports rates since the last one.
pub struct NetSampler {
    prev: Option<((u64, u64), Instant)>,
}

impl Default for NetSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl NetSampler {
    pub fn new() -> Self {
        NetSampler {
            prev: totals().map(|t| (t, Instant::now())),
        }
    }

    /// Rates since the previous call (or since `new`).
    pub fn sample(&mut self) -> Option<NetRates> {
        let cur = totals()?;
        let now = Instant::now();
        let r = self
            .prev
            .map(|(p, at)| rates(p, cur, now.duration_since(at).as_secs_f64()));
        self.prev = Some((cur, now));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one RTM_IFINFO2 message the way the kernel lays it out.
    fn message(name: &str, flags: i32, if_type: u8, ibytes: u64, obytes: u64) -> Vec<u8> {
        let sdl_len = 8 + name.len().max(12);
        let len = MSGHDR2_LEN + sdl_len;
        let mut m = vec![0u8; len];
        m[0..2].copy_from_slice(&(len as u16).to_ne_bytes());
        m[2] = 5; // RTM_VERSION
        m[3] = RTM_IFINFO2;
        m[8..12].copy_from_slice(&flags.to_ne_bytes());
        m[12..14].copy_from_slice(&7u16.to_ne_bytes());
        m[DATA_OFFSET] = if_type;
        m[DATA_OFFSET + 64..DATA_OFFSET + 72].copy_from_slice(&ibytes.to_ne_bytes());
        m[DATA_OFFSET + 72..DATA_OFFSET + 80].copy_from_slice(&obytes.to_ne_bytes());
        let sdl = &mut m[MSGHDR2_LEN..];
        sdl[0] = sdl_len as u8;
        sdl[1] = libc::AF_LINK as u8;
        sdl[5] = name.len() as u8;
        sdl[8..8 + name.len()].copy_from_slice(name.as_bytes());
        m
    }

    #[test]
    fn layout_matches_libc() {
        assert_eq!(std::mem::size_of::<libc::if_msghdr2>(), MSGHDR2_LEN);
        assert_eq!(
            std::mem::offset_of!(libc::if_msghdr2, ifm_data),
            DATA_OFFSET
        );
        assert_eq!(std::mem::offset_of!(libc::if_data64, ifi_ibytes), 64);
        assert_eq!(std::mem::offset_of!(libc::if_data64, ifi_obytes), 72);
    }

    #[test]
    fn parses_messages_and_skips_others() {
        let mut buf = message("en0", IFF_UP, IFT_ETHER, 1_000, 200);
        // An RTM_NEWMADDR2 message in between is skipped.
        let mut other = vec![0u8; 20];
        other[0..2].copy_from_slice(&20u16.to_ne_bytes());
        other[3] = 0x13;
        buf.extend(other);
        buf.extend(message("lo0", IFF_UP | IFF_LOOPBACK, 0x18, 5_000, 5_000));
        buf.extend(message("utun3", IFF_UP, 0x01, 900, 90));
        buf.extend(message("awdl0", IFF_UP, IFT_ETHER, 77, 77));
        buf.extend(message("en7", 0, IFT_ETHER, 10, 10));
        let list = parse_iflist2(&buf);
        assert_eq!(list.len(), 5);
        assert_eq!(
            list[0],
            IfCounters {
                name: "en0".into(),
                index: 7,
                flags: IFF_UP,
                if_type: IFT_ETHER,
                ibytes: 1_000,
                obytes: 200
            }
        );
        let counted: Vec<_> = list
            .iter()
            .filter(|c| c.counts())
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(counted, vec!["en0"]);
    }

    #[test]
    fn truncated_buffer_is_safe() {
        let buf = message("en0", IFF_UP, IFT_ETHER, 1, 1);
        assert!(parse_iflist2(&buf[..buf.len() - 3]).is_empty());
        assert!(parse_iflist2(&[]).is_empty());
    }

    #[test]
    fn rates_per_second() {
        let r = rates((1_000, 500), (4_000, 1_500), 2.0);
        assert_eq!(
            r,
            NetRates {
                down_bytes_per_sec: 1_500.0,
                up_bytes_per_sec: 500.0
            }
        );
        assert_eq!(rates((10, 10), (5, 5), 1.0), NetRates::default());
    }

    #[test]
    fn live_totals() {
        assert!(totals().is_some());
    }
}
