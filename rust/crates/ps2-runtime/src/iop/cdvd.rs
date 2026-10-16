//! The IOP's CD/DVD services (cdvdfsv): init 0x80000592, S-commands 0x80000593, N-commands
//! 0x80000595, search file 0x80000597, disk ready 0x8000059A / 0x8000059C (libcdvd in the SLUS,
//! 0x0011B9D0..0x0011CE38).
//!
//! The game reads its files through the file I/O service; libcdvd is used for the disc type,
//! the media mode, readiness and file search. N-command reads (sceCdRead) are supported for the
//! other programs.

use super::{le, Kind, Reply};
use crate::{diag, Machine};

#[derive(Default)]
pub struct Cd {
    pub mode: u32,
}

/// SCECdPS2DVD
const DISK_TYPE: u32 = 0x14;

pub fn call(m: &mut Machine, kind: Kind, fno: u32, d: &[u8], recv: usize) -> Reply {
    let mut r = vec![0u8; recv.max(4)];
    let mut set = |o: usize, v: u32| {
        if o + 4 <= r.len() {
            r[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    };
    match kind {
        Kind::CdInit => set(0, 1),
        Kind::CdDiskReady | Kind::CdDiskReady2 => set(0, 2), // SCECdComplete
        Kind::CdS => match fno {
            0x03 => set(0, DISK_TYPE),
            0x22 => {
                m.iop.cd.mode = le(d, 0);
                set(0, 1)
            }
            0x01 => {
                // ReadClock: result, then BCD second, minute, hour, pad, day, month, year.
                set(0, 1);
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|x| x.as_secs()).unwrap_or(0);
                let bcd = |x: u64| ((x / 10) << 4 | (x % 10)) as u8;
                let (s, mi, h) = (t % 60, t / 60 % 60, t / 3600 % 24);
                let days = t / 86400;
                let (y, mo, dd) = civil(days as i64);
                let clock = [bcd(s), bcd(mi), bcd(h), 0, bcd(dd as u64), bcd(mo as u64), bcd((y % 100) as u64), 0];
                if r.len() >= 12 {
                    r[4..12].copy_from_slice(&clock);
                }
            }
            _ => {
                diag::once(format!("cdvd: S-command {fno:#x} (answered 1)"));
                set(0, 1)
            }
        },
        Kind::CdSearch => {
            // { sceCdlFILE (0x24): lsn, size, name[16], date[8], flag } name[256] @0x24,
            // dest @0x124. The IOP writes the found entry to dest.
            let end = d[0x24..].iter().position(|&b| b == 0).map(|p| 0x24 + p).unwrap_or(d.len().min(0x124));
            let name = String::from_utf8_lossy(&d[0x24..end]).to_string();
            let dest = le(d, 0x124);
            match m.disc.as_ref().and_then(|disc| disc.find(&name)) {
                Some((lba, size)) => {
                    let mut e = vec![0u8; 0x24];
                    e[0..4].copy_from_slice(&lba.to_le_bytes());
                    e[4..8].copy_from_slice(&size.to_le_bytes());
                    let base = name.rsplit(['\\', '/']).next().unwrap_or("");
                    for (i, b) in base.bytes().take(15).enumerate() {
                        e[8 + i] = b;
                    }
                    for (i, b) in e.iter().enumerate() {
                        crate::bus::ram_write(m, dest + i as u32, 1, *b as u64);
                    }
                    diag::log(format!("cdvd: search {name} -> lsn {lba} size {size}"));
                    set(0, 1)
                }
                None => {
                    diag::log(format!("cdvd: search {name}: not found"));
                    set(0, 0)
                }
            }
        }
        Kind::CdN => {
            match fno {
                0x01 => {
                    // sceCdRead { lsn, sectors, EE buffer, mode }: data and END when done.
                    let (lsn, n, buf) = (le(d, 0), le(d, 4), le(d, 8));
                    let bytes = n as usize * 2048;
                    let mut data = vec![0u8; bytes];
                    if let Some(disc) = m.disc.as_mut() {
                        let _ = disc.read_into(lsn as u64 * 2048, &mut data);
                    }
                    let now = m.now();
                    let start = m.iop.fileio.busy_until.max(now);
                    let at = start + super::fileio::read_cycles(bytes as u64);
                    m.iop.fileio.busy_until = at;
                    set(0, 1);
                    return Reply::Later(at, r, vec![(buf, data)]);
                }
                _ => {
                    diag::once(format!("cdvd: N-command {fno:#x} (answered 1)"));
                    set(0, 1)
                }
            }
        }
        _ => {}
    }
    Reply::Now(r)
}

/// Days since 1970-01-01 -> (year, month, day).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
