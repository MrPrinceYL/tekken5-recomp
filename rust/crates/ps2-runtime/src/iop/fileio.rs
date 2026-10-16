//! The IOP file I/O service (RPC 0x80000001) as this libkernl version uses it (SLUS 0x00113FF0
//! init, 0x001144F0 open, 0x00114780 close, 0x00114B38 read, 0x001148F8 lseek, completion
//! handler 0x00113AB8 for SIF command 0x80000011).
//!
//! Every call sends { +0 EE semaphore, +4 result address, +8 result size, +0xC fd / flags,
//! +0x10.. arguments } and receives a nonzero word when the IOP accepted it. When the operation
//! is done the IOP writes a record into the EE's record buffer (given by the init call, fn 0xFF):
//! { sema, op, result address, result size, result bytes (+0x10) ..., read: head / tail byte
//! counts and addresses (+0x14..+0x20) and bytes (+0x24, +0x64) } and sends command 0x80000011;
//! the EE handler copies the result and signals the semaphore. Read data goes straight to the EE
//! buffer (no cache to work around here, so the head / tail counts are 0).
//!
//! Devices: cdrom0: (the disc), rom0: (ROMVER).

use super::{le, Reply};
use crate::{diag, sif, Machine};
use std::collections::HashMap;

/// DVD transfer rate for timing reads (4x DVD-ROM).
pub const DVD_BYTES_PER_SEC: u64 = 5_400_000;

/// Disc reads this many times faster than the console (settings; 1 = original).
pub static FAST_LOAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// EE cycles a read of `bytes` takes.
pub fn read_cycles(bytes: u64) -> u64 {
    let f = FAST_LOAD.load(std::sync::atomic::Ordering::Relaxed).max(1) as u64;
    bytes * crate::EE_HZ / (DVD_BYTES_PER_SEC * f)
}

pub enum Backing {
    Disc { lba: u32, size: u32 },
    Mem(Vec<u8>),
}

pub struct File {
    pub path: String,
    pub backing: Backing,
    pub pos: u64,
}

#[derive(Default)]
pub struct FileIo {
    pub files: HashMap<i32, File>,
    pub next_fd: i32,
    pub rec_buf: u32,
    /// The drive is busy until then (reads queue behind each other).
    pub busy_until: u64,
}

fn size_of(f: &File) -> u64 {
    match &f.backing {
        Backing::Disc { size, .. } => *size as u64,
        Backing::Mem(v) => v.len() as u64,
    }
}

pub fn call(m: &mut Machine, fno: u32, d: &[u8], recv_size: usize) -> Reply {
    if fno == 0xFF {
        m.iop.fileio.rec_buf = le(d, 0);
        diag::log(format!("fileio init: record buffer {:#010x}", m.iop.fileio.rec_buf));
        // Word 0: the module's version tag, which libkernl checks against its own ("3000",
        // SLUS 0x001141F8); word 1: 2 would select one record slot per request.
        let mut r = vec![0u8; recv_size.max(8)];
        r[0..4].copy_from_slice(b"3000");
        return Reply::Now(r);
    }
    let sema = le(d, 0);
    let res_ptr = le(d, 4);
    let res_size = le(d, 8);
    let now = m.now();
    let mut at = now;
    let mut extra: Vec<(u32, Vec<u8>)> = Vec::new();
    let result: i32 = match fno {
        0 => {
            let flags = le(d, 0xC);
            let name_end = d[0x14..].iter().position(|&b| b == 0).map(|p| 0x14 + p).unwrap_or(d.len());
            let name = String::from_utf8_lossy(&d[0x14..name_end]).to_string();
            open(m, &name, flags)
        }
        1 => {
            let fd = le(d, 0xC) as i32;
            if m.iop.fileio.files.remove(&fd).is_some() { 0 } else { -9 }
        }
        2 => {
            let fd = le(d, 0xC) as i32;
            let buf = le(d, 0x10);
            let size = le(d, 0x14) as usize;
            match read(m, fd, size) {
                Some((data, disc_bytes)) => {
                    let n = data.len() as i32;
                    if disc_bytes > 0 && std::env::var_os("T5_FAST_DISC").is_none() {
                        let start = m.iop.fileio.busy_until.max(now);
                        at = start + read_cycles(disc_bytes);
                        m.iop.fileio.busy_until = at;
                    }
                    extra.push((buf, data));
                    n
                }
                None => -9,
            }
        }
        3 => le(d, 0x14) as i32,
        4 => {
            let fd = le(d, 0xC) as i32;
            let off = le(d, 0x10) as i32 as i64;
            let whence = le(d, 0x14);
            match m.iop.fileio.files.get_mut(&fd) {
                Some(f) => {
                    let base = match whence { 1 => f.pos as i64, 2 => size_of(f) as i64, _ => 0 };
                    f.pos = (base + off).max(0) as u64;
                    f.pos as i32
                }
                None => -9,
            }
        }
        _ => {
            diag::once(format!("fileio: unimplemented fn {fno:#x}"));
            -1
        }
    };
    // Completion record + command 0x80000011.
    let rec = m.iop.fileio.rec_buf;
    let mut r = vec![0u8; 0x40];
    r[0..4].copy_from_slice(&sema.to_le_bytes());
    r[4..8].copy_from_slice(&fno.to_le_bytes());
    r[8..12].copy_from_slice(&res_ptr.to_le_bytes());
    r[12..16].copy_from_slice(&res_size.min(0x400).to_le_bytes());
    r[16..20].copy_from_slice(&result.to_le_bytes());
    extra.push((rec, r));
    let pkt = super::packet(0x8000_0011, 0x10, &[(0xC, 0)]);
    sif::send(m, at, extra, pkt);
    // Accepted.
    let mut acc = vec![0u8; recv_size.max(4)];
    acc[0..4].copy_from_slice(&1u32.to_le_bytes());
    Reply::Now(acc)
}

fn open(m: &mut Machine, name: &str, flags: u32) -> i32 {
    let lower = name.to_ascii_lowercase();
    let backing = if lower.starts_with("cdrom") {
        m.disc.as_ref().and_then(|d| d.find(name)).map(|(lba, size)| Backing::Disc { lba, size })
    } else if lower.starts_with("rom0:romver") {
        Some(Backing::Mem(b"0220AC20060905\0".to_vec()))
    } else {
        None
    };
    let Some(backing) = backing else {
        diag::log(format!("fileio: open {name} (flags {flags:#x}) failed"));
        return -2;
    };
    let fio = &mut m.iop.fileio;
    fio.next_fd += 1;
    let fd = fio.next_fd;
    diag::log(format!("fileio: open {name} -> fd {fd}"));
    fio.files.insert(fd, File { path: name.to_string(), backing, pos: 0 });
    fd
}

/// Read up to `size` bytes; also returns how many came from the disc (for timing).
fn read(m: &mut Machine, fd: i32, size: usize) -> Option<(Vec<u8>, u64)> {
    let f = m.iop.fileio.files.get_mut(&fd)?;
    let total = size_of(f);
    let n = (total.saturating_sub(f.pos)).min(size as u64) as usize;
    let mut out = vec![0u8; n];
    let mut disc_bytes = 0;
    match &f.backing {
        Backing::Disc { lba, .. } => {
            let off = *lba as u64 * crate::disc::SECTOR + f.pos;
            if let Some(d) = m.disc.as_mut() {
                let _ = d.read_into(off, &mut out);
            }
            disc_bytes = n as u64;
        }
        Backing::Mem(v) => out.copy_from_slice(&v[f.pos as usize..f.pos as usize + n]),
    }
    let f = m.iop.fileio.files.get_mut(&fd)?;
    f.pos += n as u64;
    Some((out, disc_bytes))
}
