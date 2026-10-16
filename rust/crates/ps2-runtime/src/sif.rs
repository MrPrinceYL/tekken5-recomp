//! The SIF (subsystem interface) between the EE and the IOP, EE side.
//!
//! - Registers: MSCOM / SMCOM (mailboxes), MSFLG / SMFLG (flags: the EE sets MSFLG bits and
//!   clears SMFLG bits, the IOP the other way round), and the EE kernel's private SIF registers
//!   (0x80000000 + n, kept by the kernel for libkernl).
//! - EE -> IOP: `sceSifSetDma` (kernel) copies blocks into IOP RAM; a block written to the IOP's
//!   command buffer (SMCOM) is a SIF command for the IOP (`iop::command`).
//! - IOP -> EE: packets (and the data that precedes them) are queued and delivered through DMA
//!   channel 5 (SIF0) whenever the EE has armed it (`sceSifSetDChain`): the data is written to EE
//!   RAM, the packet to the EE's command buffer (given in its INIT command), and the channel
//!   finishes, raising the DMAC interrupt the EE's libkernl handles.

use crate::{diag, dmac, Machine};
use std::collections::VecDeque;

pub const SIF_STAT_SIFINIT: u32 = 0x10000;
pub const SIF_STAT_CMDINIT: u32 = 0x20000;
pub const SIF_STAT_BOOTEND: u32 = 0x40000;

/// One IOP -> EE delivery: data blocks for EE RAM, then a command packet.
pub struct Delivery {
    pub at: u64,
    pub writes: Vec<(u32, Vec<u8>)>,
    pub packet: Vec<u8>,
}

pub struct Sif {
    pub mscom: u32,
    pub smcom: u32,
    pub msflg: u32,
    pub smflg: u32,
    pub kregs: [u32; 32],
    pub armed: bool,
    pub queue: VecDeque<Delivery>,
    /// The EE's command packet buffer (from its INIT / CHANGE_SADDR command).
    pub ee_pktbuf: u32,
    pub next_id: u32,
}

impl Default for Sif {
    fn default() -> Sif {
        Sif {
            mscom: 0,
            smcom: crate::iop::CMD_BUF,
            msflg: 0,
            smflg: SIF_STAT_SIFINIT | SIF_STAT_CMDINIT | SIF_STAT_BOOTEND,
            kregs: [0; 32],
            armed: false,
            queue: VecDeque::new(),
            ee_pktbuf: 0,
            next_id: 1,
        }
    }
}

pub fn get_reg(m: &mut Machine, reg: u32) -> u32 {
    if reg == 4 && m.sif.smflg == 0 {
        diag::once(format!("SMFLG read as 0 (msflg {:#x}, smcom {:#x}, iop reboot pending {:?}, modules {:?})", m.sif.msflg, m.sif.smcom, m.iop.reboot_at, m.iop.modules));
    }
    let s = &m.sif;
    match reg {
        1 => s.mscom,
        2 => s.smcom,
        3 => s.msflg,
        4 => s.smflg,
        r if r >= 0x8000_0000 => s.kregs[(r & 31) as usize],
        _ => 0,
    }
}

pub fn set_reg(m: &mut Machine, reg: u32, v: u32) -> u32 {
    let s = &mut m.sif;
    match reg {
        1 => s.mscom = v,
        2 => s.smcom = v,
        3 => s.msflg |= v,
        4 => s.smflg &= !v,
        r if r >= 0x8000_0000 => s.kregs[(r & 31) as usize] = v,
        _ => {}
    }
    0
}

pub fn hw_read(m: &mut Machine, a: u32) -> u32 {
    match a & 0xFF {
        0x00 => m.sif.mscom,
        0x10 => m.sif.smcom,
        0x20 => m.sif.msflg,
        0x30 => m.sif.smflg,
        _ => 0,
    }
}

pub fn hw_write(m: &mut Machine, a: u32, v: u32) {
    diag::log(format!("SIF hw write {a:#x} = {v:#x} in {:#x}", ee_core::LAST_FN.load(std::sync::atomic::Ordering::Relaxed)));
    match a & 0xFF {
        0x00 => m.sif.mscom = v,
        0x20 => m.sif.msflg |= v,
        0x30 => m.sif.smflg &= !v,
        _ => {}
    }
}

/// sceSifSetDma(transfers, count): each transfer is { src, dest, size, attr }.
pub fn set_dma(m: &mut Machine, list: u32, count: u32) -> u32 {
    let mut commands = Vec::new();
    for i in 0..count.min(32) {
        let t = list + 16 * i;
        let src = crate::bus::ram_read(m, t, 4) as u32;
        let dst = crate::bus::ram_read(m, t + 4, 4) as u32;
        let size = crate::bus::ram_read(m, t + 8, 4) as u32;
        let mut buf = vec![0u8; size as usize];
        for (j, b) in buf.iter_mut().enumerate() {
            *b = crate::bus::ram_read(m, src + j as u32, 1) as u8;
        }
        if dst & 0x1F_FFFF != crate::iop::CMD_BUF && dst < 0x1000 {
            diag::log(format!("SIF DMA to IOP {dst:#x} ({size} bytes): suspicious"));
        }
        diag::count_with(|| format!("sifdma_dst_{:#x}", dst & 0xFFF000));
        if dst & 0x1F_FFFF == crate::iop::CMD_BUF {
            commands.push(buf);
        } else {
            m.iop.write(dst, &buf);
        }
    }
    for c in commands {
        crate::iop::command(m, &c);
    }
    let id = m.sif.next_id;
    m.sif.next_id = m.sif.next_id.wrapping_add(1).max(1);
    id
}

/// sceSifDmaStat: transfers finish at once (negative = done).
pub fn dma_stat(_m: &mut Machine, _id: u32) -> i32 {
    -1
}

pub fn stop_dma(_m: &mut Machine) {}

/// sceSifSetDChain: arm channel 5 for the next IOP packet.
pub fn set_dchain(m: &mut Machine) {
    m.dmac.ch[dmac::SIF0].chcr = 0x184;
    m.sif.armed = true;
    ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The program started channel 5 itself.
pub fn sif0_armed(m: &mut Machine) {
    m.sif.armed = true;
}

/// Queue an IOP -> EE packet (with data written to EE RAM first), deliverable at `at`.
pub fn send(m: &mut Machine, at: u64, writes: Vec<(u32, Vec<u8>)>, packet: Vec<u8>) {
    m.sif.queue.push_back(Delivery { at, writes, packet });
    ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn update(m: &mut Machine, now: u64) {
    if !m.sif.armed || m.sif.ee_pktbuf == 0 {
        return;
    }
    // The earliest-due packet (FIFO among equals): a slow CD read must not hold up the replies
    // of other services queued after it.
    let Some((i, at)) = m.sif.queue.iter().enumerate().map(|(i, d)| (i, d.at)).min_by_key(|&(i, at)| (at, i)) else { return };
    if at > now {
        return;
    }
    let d = m.sif.queue.remove(i).unwrap();
    for (addr, data) in &d.writes {
        for (i, b) in data.iter().enumerate() {
            crate::bus::ram_write(m, addr + i as u32, 1, *b as u64);
        }
    }
    let buf = m.sif.ee_pktbuf;
    for (i, b) in d.packet.iter().enumerate() {
        crate::bus::ram_write(m, buf + i as u32, 1, *b as u64);
    }
    m.sif.armed = false;
    m.dmac.ch[dmac::SIF0].madr = buf + d.packet.len() as u32;
    dmac::finish(m, dmac::SIF0);
    diag::count("sif0 packets");
}

pub fn next_event(m: &Machine) -> u64 {
    if m.sif.armed && m.sif.ee_pktbuf != 0 {
        m.sif.queue.iter().map(|d| d.at).min().unwrap_or(u64::MAX)
    } else {
        u64::MAX
    }
}
