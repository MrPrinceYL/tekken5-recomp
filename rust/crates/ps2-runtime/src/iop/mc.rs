//! mcserv (RPC 0x80000400) for libmc 2.x (SLUS 0x0010D030..0x0010E868): the memory card in each
//! port is a folder on the host (`T5_MEMCARD` / `memcard\port<N>`), its PS2 file system mapped
//! onto host files and folders.
//!
//! Requests: a 0x30-byte record { +0 fd, +4 port, +8 slot, +0xC size / flags, +0x10 offset,
//! +0x14 origin / head count, +0x18 EE buffer, +0x1C EE result area, +0x20 head bytes } or a
//! 0x414-byte record { +0 port, +4 slot, +8 flags, +0xC max entries, +0x10 EE table, +0x14 name }.
//! Reply word 0: the result. Some calls fill the result area (GetInfo: +0 type, +4 free
//! clusters, +0x90 formatted; Read: head / tail fix-ups, unused here: data goes straight to the
//! EE buffer) or an EE table (GetDir: 64-byte entries; ChDir: the old directory).

use super::{le, Reply};
use crate::{diag, Machine};
use std::path::PathBuf;

pub struct McFile {
    pub path: PathBuf,
    pub pos: u64,
    pub write: bool,
}

#[derive(Default)]
pub struct Mc {
    pub files: std::collections::HashMap<i32, McFile>,
    pub next_fd: i32,
    pub cwd: [String; 2],
    pub seen: [bool; 2],
}

fn root(port: u32) -> PathBuf {
    let base = std::env::var("T5_MEMCARD").unwrap_or_else(|_| "memcard".into());
    let p = PathBuf::from(base).join(format!("port{}", port & 1));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn cname(d: &[u8], o: usize) -> String {
    let s = &d[o.min(d.len())..];
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).to_string()
}

/// The host path of a card path, relative to the port's current directory.
fn resolve(m: &Machine, port: u32, name: &str) -> PathBuf {
    let cwd = &m.iop.mc.cwd[(port & 1) as usize];
    let full = if name.starts_with('/') { name.to_string() } else { format!("{}/{}", cwd.trim_end_matches('/'), name) };
    let mut parts: Vec<&str> = Vec::new();
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    let mut p = root(port);
    for x in parts {
        p.push(x);
    }
    p
}

fn word(recv: usize, v: i32) -> Vec<u8> {
    let mut r = vec![0u8; recv.max(4)];
    r[0..4].copy_from_slice(&v.to_le_bytes());
    r
}

fn ee_read(m: &Machine, a: u32, n: usize) -> Vec<u8> {
    (0..n).map(|i| crate::bus::ram_read(m, a + i as u32, 1) as u8).collect()
}

/// A 64-byte directory entry (sceMcTblGetDir).
fn dir_entry(name: &str, meta: &std::fs::Metadata) -> Vec<u8> {
    let mut e = vec![0u8; 64];
    let size = if meta.is_dir() { 0 } else { meta.len() as u32 };
    let attr: u16 = if meta.is_dir() { 0x8427 } else { 0x8497 };
    // Dates: PS2 card time (JST): sec, min, hour, day, month, year (u16).
    let t = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0) + 9 * 3600;
    let days = t / 86400;
    let (y, mo, d) = {
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let dd = doy - (153 * mp + 2) / 5 + 1;
        let mm = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + if mm <= 2 { 1 } else { 0 }, mm, dd)
    };
    let date = [0, (t % 60) as u8, (t / 60 % 60) as u8, (t / 3600 % 24) as u8, d as u8, mo as u8, (y & 0xFF) as u8, (y >> 8) as u8];
    e[0..8].copy_from_slice(&date);
    e[8..16].copy_from_slice(&date);
    e[0x10..0x14].copy_from_slice(&size.to_le_bytes());
    e[0x14..0x16].copy_from_slice(&attr.to_le_bytes());
    for (i, b) in name.bytes().take(31).enumerate() {
        e[0x20 + i] = b;
    }
    e
}

fn matches(pat: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pat.as_bytes(), name.as_bytes())
}

pub fn call(m: &mut Machine, fno: u32, d: &[u8], recv: usize) -> Reply {
    let now = m.now();
    let mut writes: Vec<(u32, Vec<u8>)> = Vec::new();
    let r: i32 = match fno {
        0xFE => {
            let mut v = word(recv, 0);
            v.resize(12, 0);
            v[4..8].copy_from_slice(&0x20Bu32.to_le_bytes());
            v[8..12].copy_from_slice(&0x20Fu32.to_le_bytes());
            return Reply::Now(v);
        }
        0x01 => {
            // GetInfo(port, slot): type 2 (PS2 card), free clusters, formatted. The first call
            // after power-on reports a new card (-1), later ones the same card (0).
            let port = le(d, 4) & 1;
            let res_area = le(d, 0x1C);
            let mut info = vec![0u8; 0xC0];
            info[0..4].copy_from_slice(&2u32.to_le_bytes());
            info[4..8].copy_from_slice(&7_000u32.to_le_bytes());
            info[0x90..0x94].copy_from_slice(&1u32.to_le_bytes());
            writes.push((res_area, info));
            let first = !m.iop.mc.seen[port as usize];
            m.iop.mc.seen[port as usize] = true;
            if first { -1 } else { 0 }
        }
        0x02 => {
            let (port, flags, name) = (le(d, 0), le(d, 8), cname(d, 0x14));
            let p = resolve(m, port, &name);
            if flags & 0x40 != 0 {
                // Mkdir
                if p.exists() { -4 } else if std::fs::create_dir_all(&p).is_ok() { 0 } else { -5 }
            } else {
                let exists = p.is_file();
                let create = flags & 0x200 != 0;
                if !exists && !create {
                    -4
                } else {
                    if !exists || flags & 0x400 != 0 {
                        let _ = std::fs::write(&p, []);
                    }
                    let mc = &mut m.iop.mc;
                    mc.next_fd += 1;
                    let fd = mc.next_fd;
                    mc.files.insert(fd, McFile { path: p, pos: 0, write: flags & 2 != 0 });
                    fd
                }
            }
        }
        0x03 => if m.iop.mc.files.remove(&(le(d, 0) as i32)).is_some() { 0 } else { -4 },
        0x04 => {
            let (fd, off, origin) = (le(d, 0) as i32, le(d, 0x10) as i32 as i64, le(d, 0x14));
            match m.iop.mc.files.get_mut(&fd) {
                Some(f) => {
                    let size = std::fs::metadata(&f.path).map(|x| x.len()).unwrap_or(0) as i64;
                    let base = match origin { 1 => f.pos as i64, 2 => size, _ => 0 };
                    f.pos = (base + off).max(0) as u64;
                    f.pos as i32
                }
                None => -4,
            }
        }
        0x05 => {
            let (fd, size, buf, res_area) = (le(d, 0) as i32, le(d, 0xC) as usize, le(d, 0x18), le(d, 0x1C));
            match m.iop.mc.files.get_mut(&fd) {
                Some(f) => {
                    let data = std::fs::read(&f.path).unwrap_or_default();
                    let s = (f.pos as usize).min(data.len());
                    let e = (s + size).min(data.len());
                    f.pos = e as u64;
                    writes.push((buf, data[s..e].to_vec()));
                    writes.push((res_area, vec![0u8; 16]));
                    (e - s) as i32
                }
                None => -4,
            }
        }
        0x06 => {
            let (fd, size, head, buf) = (le(d, 0) as i32, le(d, 0xC) as usize, le(d, 0x14) as usize, le(d, 0x18));
            let mut bytes = d[0x20..(0x20 + head).min(d.len())].to_vec();
            bytes.extend(ee_read(m, buf, size));
            match m.iop.mc.files.get_mut(&fd) {
                Some(f) => {
                    let mut data = std::fs::read(&f.path).unwrap_or_default();
                    let s = f.pos as usize;
                    if data.len() < s + bytes.len() {
                        data.resize(s + bytes.len(), 0);
                    }
                    data[s..s + bytes.len()].copy_from_slice(&bytes);
                    f.pos += bytes.len() as u64;
                    if std::fs::write(&f.path, &data).is_ok() { bytes.len() as i32 } else { -5 }
                }
                None => -4,
            }
        }
        0x0A => 0,
        0x0C => {
            let (port, table, name) = (le(d, 0), le(d, 0x10), cname(d, 0x14));
            let old = m.iop.mc.cwd[(port & 1) as usize].clone();
            if table != 0 {
                let mut b = if old.is_empty() { b"/".to_vec() } else { old.clone().into_bytes() };
                b.push(0);
                writes.push((table, b));
            }
            let p = resolve(m, port, &name);
            if p.is_dir() {
                let new = if name.starts_with('/') { name.clone() } else { format!("{}/{}", old.trim_end_matches('/'), name) };
                m.iop.mc.cwd[(port & 1) as usize] = new;
                0
            } else {
                -4
            }
        }
        0x0D => {
            // GetDir(port, slot, name pattern, flags, max, table)
            let (port, max, table, name) = (le(d, 0), le(d, 0xC) as i32, le(d, 0x10), cname(d, 0x14));
            let (dir, pat) = match name.rfind('/') {
                Some(i) => (name[..i.max(1)].to_string(), name[i + 1..].to_string()),
                None => (String::new(), name.clone()),
            };
            let host = resolve(m, port, &dir);
            if !host.is_dir() {
                -4
            } else {
                let mut out = Vec::new();
                if host != root(port) && matches(&pat, ".") {
                    if let Ok(meta) = std::fs::metadata(&host) {
                        out.push(dir_entry(".", &meta));
                        out.push(dir_entry("..", &meta));
                    }
                }
                let mut names: Vec<_> = std::fs::read_dir(&host).into_iter().flatten().flatten().collect();
                names.sort_by_key(|e| e.file_name());
                for e in names {
                    let n = e.file_name().to_string_lossy().to_string();
                    if matches(&pat, &n) {
                        if let Ok(meta) = e.metadata() {
                            out.push(dir_entry(&n, &meta));
                        }
                    }
                }
                let n = out.len().min(max.max(0) as usize);
                writes.push((table, out[..n].concat()));
                n as i32
            }
        }
        0x0E => {
            // SetFileInfo / Rename (flags 0x10: new name in the table entry).
            let (port, flags, table, name) = (le(d, 0), le(d, 8), le(d, 0x10), cname(d, 0x14));
            let p = resolve(m, port, &name);
            if !p.exists() {
                -4
            } else if flags & 0x10 != 0 {
                let tbl = ee_read(m, table, 64);
                let new = cname(&tbl, 0x20);
                if std::fs::rename(&p, p.with_file_name(&new)).is_ok() { 0 } else { -5 }
            } else {
                0
            }
        }
        0x0F => {
            let (port, name) = (le(d, 0), cname(d, 0x14));
            let p = resolve(m, port, &name);
            if p.is_dir() {
                if std::fs::remove_dir(&p).is_ok() { 0 } else { -6 }
            } else if std::fs::remove_file(&p).is_ok() {
                0
            } else {
                -4
            }
        }
        0x10 => 0,
        0x11 => 0,
        0x12 => 0x2000,
        0x14 | 0x15 => 1,
        0x35 => 0,
        _ => {
            diag::once(format!("mc: fn {fno:#x}"));
            0
        }
    };
    diag::count_with(|| format!("mc fn {fno:#x}"));
    Reply::Later(now, word(recv, r), writes)
}
