//! High-level emulation of the IOP: the SIF command / RPC layer (what IOP sifcmd and sifrpc do)
//! and the RPC services of the IOP modules the game talks to.
//!
//! The IOP CPU itself is not run. EE commands arrive in IOP RAM (CMD_BUF) through
//! `sceSifSetDma`; replies go back as SIF packets (`sif::send`). Services are matched by RPC
//! server id; an unknown id still binds (a dummy server whose calls return zeros) and is logged,
//! so a missing service shows up in the trace instead of hanging the game.
//!
//! Protocol (from the game's own libkernl, SLUS 0x00112378 / 0x00112530 / 0x00112A28 /
//! 0x001130B0):
//!   command header (16 bytes): u8 psize, u24 dsize, u32 dest, u32 cid, u32 opt
//!   0x80000000 CHANGE_SADDR / 0x80000002 INIT (opt 0: payload +0x10 = EE packet buffer; opt 1:
//!   RPC init, answered with SET_SREG(0, 1)), 0x80000003 RESET (IOP reboot),
//!   0x80000009 BIND {+0x1C client, +0x20 sid}, 0x8000000A CALL {+0x1C client, +0x20 fno,
//!   +0x24 send size, +0x28 receive, +0x2C receive size, +0x30 rmode, +0x34 server; the send data
//!   is already in the server's buffer}, 0x8000000C GETOTHERDATA,
//!   0x80000008 END (to the EE) {+0x10 rec_id, +0x14 pkt_addr, +0x18 rpc_id, +0x1C client,
//!   +0x20 cid, +0x24 server, +0x28 buff, +0x2C cbuff}.

pub mod cdvd;
pub mod exec;
pub mod hle;
pub mod hw;
pub mod iopfile;
pub mod mc;
pub mod fileio;
pub mod misc;
pub mod modules;
pub mod pad;

pub use exec::IopModule;

use crate::{diag, sif, Machine};
use std::collections::HashMap;

/// The IOP's SIF command buffer (SMCOM): EE commands are DMA'd here.
pub const CMD_BUF: u32 = 0x0001_F000;
pub const IOP_RAM: usize = 0x20_0000;
const SERVER_BASE: u32 = 0x0001_E000;
/// Receive buffers of the emulated RPC servers (16 × 8 KiB at the top of IOP RAM; the largest
/// send seen is 4 KiB). Below it is the IOP heap (`exec::HEAP_END`).
const BUFF_BASE: u32 = 0x001E_0000;
const BUFF_SIZE: u32 = 0x2000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    FileIo,
    IopHeap,
    LoadFile,
    CdInit,
    CdS,
    CdN,
    CdSearch,
    CdDiskReady,
    CdDiskReady2,
    Pad,
    PadExt,
    Mc,
    Sdr,
    Unknown,
}

pub struct Server {
    pub sid: u32,
    pub kind: Kind,
    pub addr: u32,
    pub buff: u32,
}

/// Work the IOP finishes later (a reply after a CD access, a service's own timing).
pub struct Job {
    pub at: u64,
    pub writes: Vec<(u32, Vec<u8>)>,
    pub packet: Vec<u8>,
}

pub struct Iop {
    pub ram: Box<[u8]>,
    pub servers: HashMap<u32, Server>,
    pub fileio: fileio::FileIo,
    pub cd: cdvd::Cd,
    pub misc: misc::Misc,
    pub pad: pad::Pads,
    pub mc: mc::Mc,
    pub scratch: Box<[u8]>,
    pub exec: exec::IopExec,
    pub reboot_at: Option<u64>,
    /// IOP memory the EE allocated over RPC (freed by an IOP reboot).
    pub ee_heap: Vec<u32>,
    pub modules: Vec<String>,
}

impl Iop {
    pub fn new() -> Iop {
        let mut ram = vec![0u8; IOP_RAM].into_boxed_slice();
        let mut scratch = vec![0u8; 0x400].into_boxed_slice();
        let exec = exec::IopExec::new(&[], ram.as_mut_ptr(), scratch.as_mut_ptr());
        Iop {
            scratch,
            exec,
            ram,
            servers: HashMap::new(),
            fileio: fileio::FileIo::default(),
            cd: cdvd::Cd::default(),
            misc: misc::Misc::default(),
            pad: pad::Pads::default(),
            mc: mc::Mc::default(),
            reboot_at: None,
            ee_heap: Vec::new(),
            modules: Vec::new(),
        }
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) {
        let a = addr as usize & (IOP_RAM - 1);
        let n = data.len().min(IOP_RAM - a);
        self.ram[a..a + n].copy_from_slice(&data[..n]);
    }

    pub fn read(&self, addr: u32, n: usize) -> Vec<u8> {
        let a = addr as usize & (IOP_RAM - 1);
        let n = n.min(IOP_RAM - a);
        self.ram[a..a + n].to_vec()
    }

    pub fn r32(&self, addr: u32) -> u32 {
        let a = addr as usize & (IOP_RAM - 4);
        u32::from_le_bytes(self.ram[a..a + 4].try_into().unwrap())
    }

    /// A C string from IOP RAM.
    pub fn cstr(&self, addr: u32, max: usize) -> String {
        let b = self.read(addr, max);
        let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).to_string()
    }
}

impl Default for Iop {
    fn default() -> Self {
        Iop::new()
    }
}

fn kind_of(sid: u32) -> Kind {
    match sid {
        0x8000_0001 => Kind::FileIo,
        0x8000_0003 => Kind::IopHeap,
        0x8000_0006 => Kind::LoadFile,
        0x8000_0592 => Kind::CdInit,
        0x8000_0593 => Kind::CdS,
        0x8000_0595 => Kind::CdN,
        0x8000_0597 => Kind::CdSearch,
        0x8000_059A => Kind::CdDiskReady,
        0x8000_059C => Kind::CdDiskReady2,
        0x8000_0100 => Kind::Pad,
        0x8000_0101 => Kind::PadExt,
        0x8000_0400 => Kind::Mc,
        0x8000_0701 => Kind::Sdr,
        _ => Kind::Unknown,
    }
}

fn le(v: &[u8], o: usize) -> u32 {
    if o + 4 > v.len() {
        return 0;
    }
    u32::from_le_bytes(v[o..o + 4].try_into().unwrap())
}

fn put(v: &mut Vec<u8>, o: usize, x: u32) {
    if v.len() < o + 4 {
        v.resize(o + 4, 0);
    }
    v[o..o + 4].copy_from_slice(&x.to_le_bytes());
}

/// A SIF command packet (16-byte header + payload) for the EE.
pub fn packet(cid: u32, size: usize, fields: &[(usize, u32)]) -> Vec<u8> {
    let mut p = vec![0u8; size];
    p[0] = size as u8;
    put(&mut p, 8, cid);
    for &(o, v) in fields {
        put(&mut p, o, v);
    }
    p
}

/// A command from the EE arrived in CMD_BUF.
pub fn command(m: &mut Machine, pkt: &[u8]) {
    let cid = le(pkt, 8);
    let opt = le(pkt, 12);
    diag::count_with(|| format!("sifcmd_in_{cid:#x}"));
    if cid == 0x8000_0009 {
        diag::count_with(|| format!("bind_in_{:#x}", le(pkt, 0x20)));
    }
    let now = m.now();
    match cid {
        0x8000_0000 | 0x8000_0002 => {
            if cid == 0x8000_0002 && opt == 1 {
                // RPC init: the IOP's sifrpc answers with SET_SREG(0, 1).
                let p = packet(0x8000_0001, 0x18, &[(0x10, 0), (0x14, 1)]);
                sif::send(m, now, vec![], p);
            } else {
                m.sif.ee_pktbuf = le(pkt, 0x10);
                diag::log(format!("SIF: EE command buffer {:#010x}", m.sif.ee_pktbuf));
            }
        }
        0x8000_0001 => {}
        0x8000_0008 => {
            // END from the EE: an IOP client's bind / call finished.
            let client = le(pkt, 0x1C);
            let what = le(pkt, 0x20);
            if what == 0x8000_0009 {
                m.iop.write(client + 36, &le(pkt, 0x24).to_le_bytes());
                m.iop.write(client + 20, &le(pkt, 0x28).to_le_bytes());
                m.iop.write(client + 24, &le(pkt, 0x2C).to_le_bytes());
                diag::log(format!("IOP: bound EE server for client {client:#x}: server {:#x}", le(pkt, 0x24)));
            }
            m.iop.exec.rpc_busy.remove(&client);
            let x = &mut m.iop.exec;
            let waiting: Vec<u32> = x.threads.iter().flatten().filter(|t| t.wait == exec::Wait::Rpc(client)).map(|t| t.id).collect();
            for t in waiting {
                x.make_ready(t);
            }
            iop_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        0x8000_0003 => {
            let n = le(pkt, 0x10) as usize;
            let args = String::from_utf8_lossy(&pkt[0x18..(0x18 + n).min(pkt.len())]).trim_end_matches('\0').to_string();
            diag::log(format!("IOP reboot: {args}"));
            m.iop.modules.clear();
            for a in std::mem::take(&mut m.iop.ee_heap) {
                m.iop.exec.free(a);
            }
            // mcman restarts: the next GetInfo reports a new card again.
            m.iop.mc.seen = [false; 2];
            m.sif.queue.clear();
            m.sif.ee_pktbuf = 0;
            m.iop.reboot_at = Some(now + crate::EE_HZ / 10);
        }
        0x8000_0009 => {
            let sid = le(pkt, 0x20);
            let (addr, buff) = match m.iop.exec.servers.get(&sid) {
                // A server of a recompiled module (its sd structure and buffer).
                Some(s) => (s.sd, s.buff),
                // Served by a recompiled module that has not registered it yet: not bound
                // (server 0), the EE retries.
                None if kind_of(sid) == Kind::Unknown && !m.iop.exec.table.is_empty() => {
                    diag::once(format!("IOP: bind {sid:#010x}: no server yet"));
                    (0, 0)
                }
                None => server_for(m, sid),
            };
            let p = end_packet(pkt, 0x8000_0009, addr, buff);
            sif::send(m, now, vec![], p);
        }
        0x8000_000A => rpc_call(m, pkt),
        0x8000_000C => {
            // GETOTHERDATA: copy IOP memory to the EE, then END.
            let src = le(pkt, 0x20);
            let dst = le(pkt, 0x24);
            let size = le(pkt, 0x28) as usize;
            let data = m.iop.read(src, size);
            let p = end_packet(pkt, 0x8000_000C, 0, 0);
            sif::send(m, now, vec![(dst, data)], p);
        }
        _ => diag::once(format!("IOP: unhandled SIF command {cid:#x}")),
    }
}

/// The IOP address of the server structure and buffer for `sid` (created on first bind). IOP RAM layout: HLE kernel 0..0x20000 (command buffer 0x1F000, server structures 0x1E000), modules 0x20000..0x100000, heap 0x100000..0x1C0000, HLE server buffers 0x1C0000.. (16 KiB each).
fn server_for(m: &mut Machine, sid: u32) -> (u32, u32) {
    let n = m.iop.servers.len() as u32;
    assert!(BUFF_BASE + BUFF_SIZE * (n + 1) <= IOP_RAM as u32, "IOP: too many emulated RPC servers");
    let s = m.iop.servers.entry(sid).or_insert_with(|| {
        let kind = kind_of(sid);
        diag::log(format!("IOP: bind RPC server {sid:#010x} ({kind:?})"));
        Server { sid, kind, addr: SERVER_BASE + 0x40 * n, buff: BUFF_BASE + BUFF_SIZE * n }
    });
    (s.addr, s.buff)
}

/// Free the EE's RPC packet at `pkt_addr` (a call without END).
pub fn free_ee_packet(m: &mut Machine, pkt_addr: u32) {
    if pkt_addr == 0 {
        return;
    }
    let rec = crate::bus::ram_read(m, pkt_addr + 0x10, 4) & !1;
    crate::bus::ram_write(m, pkt_addr + 0x10, 4, rec);
    crate::bus::ram_write(m, pkt_addr + 0x18, 4, 0);
}

/// The END packet a module's RPC server sends when a call finished.
pub fn end_reply(req: &[u8], server: u32, buff: u32) -> Vec<u8> {
    end_packet(req, 0x8000_000A, server, buff)
}

fn end_packet(req: &[u8], cid: u32, server: u32, buff: u32) -> Vec<u8> {
    packet(0x8000_0008, 0x40, &[
        (0x10, le(req, 0x10)),
        (0x14, le(req, 0x14)),
        (0x18, le(req, 0x18)),
        (0x1C, le(req, 0x1C)),
        (0x20, cid),
        (0x24, server),
        (0x28, buff),
        (0x2C, 0),
    ])
}

/// What a service does with a call.
pub enum Reply {
    /// Data for the EE's receive buffer now, END now.
    Now(Vec<u8>),
    /// Data and END at a later time (a CD read).
    Later(u64, Vec<u8>, Vec<(u32, Vec<u8>)>),
}

fn rpc_call(m: &mut Machine, pkt: &[u8]) {
    let fno = le(pkt, 0x20);
    let send_size = le(pkt, 0x24) as usize;
    let recv = le(pkt, 0x28);
    let recv_size = le(pkt, 0x2C) as usize;
    let rmode = le(pkt, 0x30);
    let server = le(pkt, 0x34);
    diag::once(format!("IOP: first call to server struct {server:#x} fn {fno:#x} (send {send_size}, recv {recv_size}, rmode {rmode})"));
    if let Some(sid) = m.iop.exec.servers.values().find(|s| s.sd == server).map(|s| s.sid) {
        diag::count_with(|| format!("rpc {sid:#x}/{fno:#x}"));
        diag::count_with(|| format!("call_{sid:#x}_rmode{rmode}"));
        hle::rpc_request(m, sid, fno, send_size as u32, recv, recv_size as u32, rmode, pkt);
        return;
    }
    let Some(s) = m.iop.servers.values().find(|s| s.addr == server) else {
        diag::once(format!("IOP: call to unknown server {server:#x}"));
        return;
    };
    let (kind, buff, sid) = (s.kind, s.buff, s.sid);
    let data = m.iop.read(buff, send_size);
    diag::count_with(|| format!("rpc {sid:#x}/{fno:#x}"));
    let reply = match kind {
        Kind::FileIo => fileio::call(m, fno, &data, recv_size),
        Kind::CdInit | Kind::CdS | Kind::CdN | Kind::CdSearch | Kind::CdDiskReady | Kind::CdDiskReady2 => {
            cdvd::call(m, kind, fno, &data, recv_size)
        }
        Kind::IopHeap | Kind::LoadFile => misc::heap_call(m, kind, fno, &data, recv_size),
        Kind::Pad | Kind::PadExt => pad::call(m, sid, &data, recv_size),
        Kind::Mc => mc::call(m, fno, &data, recv_size),
        Kind::Sdr => misc::sdr_call(m, fno, &data, recv_size),
        Kind::Unknown => {
            diag::once(format!("IOP: call to unimplemented server {sid:#010x} fn {fno:#x} send {send_size} recv {recv_size}"));
            diag::count_with(|| format!("rpc {sid:#x}"));
            Reply::Now(vec![0; recv_size])
        }
    };
    let now = m.now();
    let (at, mut out, mut writes) = match reply {
        Reply::Now(d) => (now, d, vec![]),
        Reply::Later(at, d, w) => (at, d, w),
    };
    out.resize(recv_size, 0);
    if recv != 0 && recv_size > 0 {
        writes.insert(0, (recv, out));
    }
    diag::count_with(|| format!("call_{sid:#x}_rmode{rmode}"));
    if rmode != 0 {
        let p = end_packet(pkt, 0x8000_000A, server, buff);
        sif::send(m, at, writes, p);
    } else {
        // No END requested: the data still arrives, and the EE's packet is freed (rec_id bit 0,
        // rpc_id), which is what sceSifCheckStatRpc waits for.
        for (a, d) in writes {
            for (i, b) in d.iter().enumerate() {
                crate::bus::ram_write(m, a + i as u32, 1, *b as u64);
            }
        }
        free_ee_packet(m, le(pkt, 0x14));
    }
}

/// IOP-side timing: the end of a reboot, service jobs.
pub fn update(m: &mut Machine, now: u64) {
    if let Some(t) = m.iop.reboot_at {
        if now >= t {
            m.iop.reboot_at = None;
            m.sif.smcom = CMD_BUF;
            m.sif.smflg |= sif::SIF_STAT_SIFINIT | sif::SIF_STAT_CMDINIT | sif::SIF_STAT_BOOTEND;
            diag::log("IOP reboot done".into());
        }
    }
    misc::update(m, now);
    pad::update(m);
    hw::update(m, now);
    exec::run(m);
}

pub fn next_event(m: &Machine) -> u64 {
    m.iop.reboot_at.unwrap_or(u64::MAX).min(misc::next_event(m)).min(exec::next_event(m))
}
