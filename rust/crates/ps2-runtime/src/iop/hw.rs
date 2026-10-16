//! IOP hardware the recompiled modules touch: the SPU2 registers (0x1F900000..0x1F9007FF,
//! kept in `spu2::Spu2`), IOP DMA channels 4 and 7 (SPU2 cores 0 and 1) and the IOP interrupt
//! lines they raise (36 / 40, delivered to RegisterIntrHandler handlers by `exec`).
//!
//! DMA to the SPU2 (libsd 0x00022358): CHCR 0x01000201 starts a block transfer of BCR words
//! from MADR. With the core's ATTR DMA bits set to 0x20 it is a normal write into SPU2 RAM at
//! TSA, finished after the bus time; with ADMAS set (auto DMA, PCM streaming) the data is played
//! from the core's input buffer and the transfer finishes when it has been consumed (512 bytes
//! per channel every 256 samples at 48 kHz).

use crate::{diag, Machine};
use iop_core::Ctx;

const DMA4: u32 = 0x1F80_10C0;
const DMA7: u32 = 0x1F80_1500;

#[derive(Default, Clone, Copy)]
pub struct DmaCh {
    pub madr: u32,
    pub bcr: u32,
    pub chcr: u32,
    /// When the running transfer finishes (EE cycles).
    pub done_at: u64,
}

fn phys(a: u32) -> u32 {
    a & 0x1FFF_FFFF
}

pub fn io_read(_c: &mut Ctx, addr: u32, size: usize) -> u32 {
    let m = crate::machine();
    let a = phys(addr);
    match a {
        0x1F90_0000..=0x1F90_07FF => {
            let v = m.spu2.read16(a & 0x7FF) as u32;
            if size == 4 { v | (m.spu2.read16((a + 2) & 0x7FF) as u32) << 16 } else { v }
        }
        DMA4..=0x1F80_10CF | DMA7..=0x1F80_150F => {
            let ch = &m.iop.exec.dma[if a < DMA7 { 0 } else { 1 }];
            let w = match a & 0xC {
                0 => ch.madr,
                4 => ch.bcr,
                8 => ch.chcr,
                _ => 0,
            };
            w >> ((a & 3) * 8)
        }
        0x1F80_1070 => m.iop.exec.pending_irq as u32,
        0x1F80_1074 => m.iop.exec.intr_enabled as u32,
        _ => {
            diag::once(format!("IOP read {addr:#010x} ({size})"));
            0
        }
    }
}

pub fn io_write(_c: &mut Ctx, addr: u32, size: usize, v: u32) {
    let m = crate::machine();
    let a = phys(addr);
    match a {
        0x1F90_0000..=0x1F90_07FF => {
            m.spu2.write16(a & 0x7FF, v as u16);
            if size == 4 {
                m.spu2.write16((a + 2) & 0x7FF, (v >> 16) as u16);
            }
        }
        DMA4..=0x1F80_10CF | DMA7..=0x1F80_150F => {
            // libsd writes these registers as 16-bit halves (BCR block size at +4, count at +6).
            let n = if a < DMA7 { 0 } else { 1 };
            let sh = (a & 3) * 8;
            let mask = if size >= 4 { u32::MAX } else { ((1u64 << (size * 8)) - 1) as u32 } << sh;
            let merge = |old: u32| (old & !mask) | ((v << sh) & mask);
            let ch = &mut m.iop.exec.dma[n];
            match a & 0xC {
                0 => ch.madr = merge(ch.madr) & 0x1F_FFFF,
                4 => ch.bcr = merge(ch.bcr),
                8 => {
                    ch.chcr = merge(ch.chcr);
                    if ch.chcr & 0x0100_0000 != 0 && mask & 0x0100_0000 != 0 {
                        dma_start(m, n);
                    }
                }
                _ => {}
            }
        }
        _ => diag::once(format!("IOP write {addr:#010x} = {v:#x} ({size})")),
    }
}

fn dma_start(m: &mut Machine, core: usize) {
    let ch = m.iop.exec.dma[core];
    let words = ((ch.bcr & 0xFFFF) * (ch.bcr >> 16).max(1)) as usize;
    let bytes = words * 4;
    let data = m.iop.read(ch.madr, bytes);
    let now = m.now();
    let to_dev = ch.chcr & 1 != 0;
    let auto = m.spu2.admas(core);
    let dur = if auto {
        // The block lands in the core's input buffer; the transfer finishes when the block
        // before it has been played (the hardware double-buffers 0x200 bytes per channel), so
        // the next one arrives while this one plays.
        let queued = m.spu2.stream[core].len() as u64;
        m.spu2.stream_in(core, &data);
        queued * crate::EE_HZ / 48_000 + 1000
    } else if to_dev {
        m.spu2.dma_write(core, &data);
        bytes as u64 * crate::EE_HZ / 36_000_000 + 1000
    } else {
        let d = m.spu2.dma_read(core, bytes);
        m.iop.write(ch.madr, &d);
        bytes as u64 * crate::EE_HZ / 36_000_000 + 1000
    };
    m.iop.exec.dma[core].done_at = now + dur;
    m.iop.exec.dma[core].madr = ch.madr + bytes as u32;
    diag::count(if auto { "spu2 adma bytes/1k" } else { "spu2 dma" });
}

/// Finish SPU2 DMA transfers that are due: CHCR busy off, interrupt 36 / 40.
pub fn update(m: &mut Machine, now: u64) {
    for core in 0..2 {
        let ch = &mut m.iop.exec.dma[core];
        if ch.chcr & 0x0100_0000 != 0 && ch.done_at <= now {
            ch.chcr &= !0x0100_0000;
            m.iop.exec.pending_irq |= 1u64 << (36 + 4 * core);
            iop_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub fn next_event(m: &Machine) -> u64 {
    m.iop.exec.dma.iter().filter(|c| c.chcr & 0x0100_0000 != 0).map(|c| c.done_at).min().unwrap_or(u64::MAX)
}
