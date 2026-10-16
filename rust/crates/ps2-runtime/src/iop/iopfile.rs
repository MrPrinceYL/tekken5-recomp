//! cdvdman and ioman for the recompiled IOP modules (NU_File_Manager reads the disc with them).
//! Reads complete into IOP RAM at once; the drive stays busy for the time a 4x DVD drive needs,
//! and sceCdSync(0) / blocking reads wait for it.

use super::exec::{block, Wait};
use super::fileio::{Backing, File};
use crate::{diag, Machine};
use iop_core::{Ctx, GPR_A0, GPR_SP};

fn arg(m: &Machine, c: &Ctx, n: usize) -> u32 {
    if n < 4 { c.gpr(GPR_A0 + n) } else { m.iop.r32(c.gpr(GPR_SP).wrapping_add(0x10 + 4 * (n as u32 - 4))) }
}

fn busy_for(m: &mut Machine, bytes: u64) -> u64 {
    let now = m.now();
    let start = m.iop.exec.cd_busy_until.max(now);
    let end = if std::env::var_os("T5_FAST_DISC").is_some() { now } else { start + super::fileio::read_cycles(bytes) };
    m.iop.exec.cd_busy_until = end;
    end
}

pub fn cdvd(m: &mut Machine, c: &mut Ctx, idx: u32) -> i32 {
    let a = |n: usize| arg(crate::machine(), c, n);
    match idx {
        6 => {
            // sceCdRead(lsn, sectors, buf, mode)
            let (lsn, n, buf) = (a(0), a(1), a(2));
            let mut data = vec![0u8; n as usize * 2048];
            if let Some(d) = m.disc.as_mut() {
                let _ = d.read_into(lsn as u64 * 2048, &mut data);
            }
            m.iop.write(buf, &data);
            busy_for(m, data.len() as u64);
            diag::count("iop cd sectors");
            1
        }
        7 => 1,
        8 => 0,
        10 => {
            // sceCdSearchFile(fp, name)
            let (fp, name) = (a(0), a(1));
            let name = m.iop.cstr(name, 256);
            match m.disc.as_ref().and_then(|d| d.find(&name)) {
                Some((lba, size)) => {
                    let mut e = vec![0u8; 0x24];
                    e[0..4].copy_from_slice(&lba.to_le_bytes());
                    e[4..8].copy_from_slice(&size.to_le_bytes());
                    for (i, b) in name.rsplit(['\\', '/']).next().unwrap_or("").bytes().take(15).enumerate() {
                        e[8 + i] = b;
                    }
                    m.iop.write(fp, &e);
                    1
                }
                None => {
                    diag::log(format!("IOP cdvd: search {name}: not found"));
                    0
                }
            }
        }
        11 => {
            // sceCdSync(mode): 0 waits, 1 polls (1 = busy).
            let now = m.now();
            let until = m.iop.exec.cd_busy_until;
            if until <= now {
                0
            } else if a(0) == 0 {
                block(m, Wait::Delay(until));
                0
            } else {
                1
            }
        }
        13 => 2,
        28 => if m.iop.exec.cd_busy_until > m.now() { 0x06 } else { 0x0A },
        _ => {
            diag::once(format!("IOP cdvdman #{idx}"));
            0
        }
    }
}

pub fn ioman(m: &mut Machine, c: &mut Ctx, idx: u32) -> i32 {
    let a = |n: usize| arg(crate::machine(), c, n);
    match idx {
        4 => {
            let name = m.iop.cstr(a(0), 256);
            let backing = if name.to_ascii_lowercase().starts_with("cdrom") {
                m.disc.as_ref().and_then(|d| d.find(&name)).map(|(lba, size)| Backing::Disc { lba, size })
            } else {
                None
            };
            match backing {
                Some(b) => {
                    let x = &mut m.iop.exec;
                    x.next_fd += 1;
                    let fd = x.next_fd;
                    x.files.insert(fd, File { path: name.clone(), backing: b, pos: 0 });
                    diag::log(format!("IOP open {name} -> {fd}"));
                    fd
                }
                None => {
                    diag::log(format!("IOP open {name}: not found"));
                    -2
                }
            }
        }
        5 => if m.iop.exec.files.remove(&(a(0) as i32)).is_some() { 0 } else { -9 },
        6 => {
            let (fd, buf, n) = (a(0) as i32, a(1), a(2) as u64);
            let Some(f) = m.iop.exec.files.get_mut(&fd) else { return -9 };
            let Backing::Disc { lba, size } = f.backing else { return -9 };
            let n = n.min((size as u64).saturating_sub(f.pos));
            let off = lba as u64 * 2048 + f.pos;
            f.pos += n;
            let mut data = vec![0u8; n as usize];
            if let Some(d) = m.disc.as_mut() {
                let _ = d.read_into(off, &mut data);
            }
            m.iop.write(buf, &data);
            let until = busy_for(m, n);
            block(m, Wait::Delay(until));
            n as i32
        }
        8 => {
            let (fd, off, whence) = (a(0) as i32, a(1) as i32 as i64, a(2));
            let Some(f) = m.iop.exec.files.get_mut(&fd) else { return -9 };
            let size = match f.backing { Backing::Disc { size, .. } => size as i64, Backing::Mem(ref v) => v.len() as i64 };
            let base = match whence { 1 => f.pos as i64, 2 => size, _ => 0 };
            f.pos = (base + off).max(0) as u64;
            f.pos as i32
        }
        _ => {
            diag::once(format!("IOP ioman #{idx}"));
            -1
        }
    }
}
