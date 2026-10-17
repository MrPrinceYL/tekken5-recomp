//! The EE's memory map outside RAM and the scratchpad: hardware registers, FIFOs, VU memory,
//! the GS privileged registers and the BIOS ROM.

use crate::{diag, machine, Machine};
use ee_core::{ram_off, Ctx, SCRATCH_BASE, SCRATCH_SIZE};

/// Read guest memory (RAM or scratchpad) from runtime code.
pub fn ram_read(m: &Machine, addr: u32, size: usize) -> u64 {
    if let Some(o) = ram_off(addr) {
        let mut v = 0u64;
        for i in 0..size {
            v |= (m.ram[(o + i) & (ee_core::RAM_SIZE - 1)] as u64) << (8 * i);
        }
        return v;
    }
    let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
    if s < SCRATCH_SIZE {
        let mut v = 0u64;
        for i in 0..size {
            v |= (m.scratch[(s + i) & (SCRATCH_SIZE - 1)] as u64) << (8 * i);
        }
        return v;
    }
    0
}

pub fn ram_write(m: &mut Machine, addr: u32, size: usize, v: u64) {
    if let Some(o) = ram_off(addr) {
        for i in 0..size {
            m.ram[(o + i) & (ee_core::RAM_SIZE - 1)] = (v >> (8 * i)) as u8;
        }
        return;
    }
    let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
    if s < SCRATCH_SIZE {
        for i in 0..size {
            m.scratch[(s + i) & (SCRATCH_SIZE - 1)] = (v >> (8 * i)) as u8;
        }
    }
}

/// A quadword of guest memory for DMA (RAM or, with `spr`, the scratchpad).
#[inline]
pub fn qword(m: &Machine, addr: u32, spr: bool) -> u128 {
    if spr {
        let o = (addr as usize) & (SCRATCH_SIZE - 1) & !15;
        return u128::from_le_bytes(m.scratch[o..o + 16].try_into().unwrap());
    }
    let o = (addr as usize) & (ee_core::RAM_SIZE - 1) & !15;
    u128::from_le_bytes(m.ram[o..o + 16].try_into().unwrap())
}

#[inline]
pub fn set_qword(m: &mut Machine, addr: u32, spr: bool, v: u128) {
    if spr {
        let o = (addr as usize) & (SCRATCH_SIZE - 1) & !15;
        m.scratch[o..o + 16].copy_from_slice(&v.to_le_bytes());
        return;
    }
    let o = (addr as usize) & (ee_core::RAM_SIZE - 1) & !15;
    m.ram[o..o + 16].copy_from_slice(&v.to_le_bytes());
}

/// Physical address of a hardware access (KSEG1 / KSEG0 mirrors removed).
#[inline]
fn phys(addr: u32) -> u32 {
    if addr >= 0x8000_0000 && addr < 0xC000_0000 { addr & 0x1FFF_FFFF } else { addr }
}

/// T5_T1LOG=1: each Timer 1 COUNT read -> t1log.txt: vblank, the game's frame load in % (as
/// FUN_00337a58 - DAT_00154e80 over 147456000 / 60 bus clocks, see bugs.md B14), the innermost
/// calls, the render time of the last frame and the raw timer state.
fn t1log(m: &Machine, c: &Ctx, count: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    let mode = m.timers.t[1].mode;
    let wraps = ram_read(m, 0x440A60, 4) as u32 + (mode >> 11 & 1);
    let shift = if mode & 3 == 0 { 0 } else { 2 << (mode & 3) };
    let val = (((wraps as u64) << 16 | count as u64) << shift) as u32;
    let el = val.wrapping_sub(ram_read(m, 0x154E80, 4) as u32) as i32;
    let calls: Vec<String> = (0..4u32).map(|i| format!("{:x}", c.trail[(c.depth.wrapping_sub(i) & 63) as usize])).collect();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("t1log.txt") {
        use std::io::Write;
        // Render time of the last frame (0x440480 + 0x238, written by the DMA-end handler 0x31d708).
        let render = ram_read(m, 0x4406B8, 4) as u32;
        let _ = writeln!(f, "{} {:.2} {} render {:.2} | count {count:#x} mode {mode:#x} wraps {wraps} comp {:#x}", crate::VBLANKS.load(Relaxed), el as f64 / 2_457_600.0 * 100.0, calls.join(","), render as f64 / 2_457_600.0 * 100.0, m.timers.t[1].comp);
    }
}

pub fn io_read(c: &mut Ctx, addr: u32, size: usize) -> u128 {
    let _ = c;
    let m = machine();
    let a = phys(addr);
    let v: u128 = match a {
        0x1000_0000..=0x1000_1FFF => {
            // A wrap / compare that is due must show in MODE (OVFF / EQUF) together with the
            // count: the game reads COUNT, then MODE.OVFF to add the overflow its handler has
            // not counted yet (FUN_00337a58). Unprocessed, the time jumped back a whole wrap.
            let now = m.now();
            crate::timers::update(m, now);
            let v = m.timers.read(a, now);
            if a == 0x1000_0800 && crate::diag::enabled("T5_T1LOG") {
                t1log(m, c, v);
            }
            v as u128
        }
        0x1000_2000..=0x1000_2FFF => crate::ipu::read(m, a) as u128,
        0x1000_3000..=0x1000_37FF => {
            mtvu_sync(m);
            m.gif.read(a) as u128
        }
        0x1000_3800..=0x1000_3BFF => m.vif[0].read(a) as u128,
        0x1000_3C00..=0x1000_3FFF => {
            if crate::diag::enabled("T5_PRIMSTATS") {
                diag::count_with(|| format!("ee_read_vif1_{:x}", a & 0x3F0));
            }
            mtvu_sync(m);
            m.vif[1].read(a) as u128
        }
        0x1000_7000..=0x1000_701F => crate::ipu::fifo_read(m, a),
        0x1000_4000..=0x1000_6FFF => 0,
        0x1000_8000..=0x1000_EFFF => crate::dmac::read(m, a) as u128,
        0x1000_F000 => {
            crate::kernel::poll_istat(m);
            m.istat as u128
        }
        0x1000_F010 => m.imask as u128,
        0x1000_F200..=0x1000_F2FF => crate::sif::hw_read(m, a) as u128,
        0x1000_F520 => m.dmac.enable as u128,
        0x1000_F590 => m.dmac.enable as u128,
        0x1000_F000..=0x1000_FFFF => {
            diag::count_with(|| format!("io read {a:#010x}"));
            0
        }
        0x1100_0000..=0x1100_FFFF => vu_mem_read(m, a, size),
        0x1200_0000..=0x1200_1FFF => m.gs.priv_read(a) as u128,
        0x1FC0_0000..=0x1FFF_FFFF => 0,
        _ => {
            diag::count("io read unmapped");
            diag::once(format!("read from unmapped page {:#010x} (size {size}) in {:#010x}", addr & !0xFFF, ee_core::LAST_FN.load(std::sync::atomic::Ordering::Relaxed)));
            0
        }
    };
    if size >= 16 { v } else { v & ((1u128 << (8 * size)) - 1) }
}

pub fn io_write(c: &mut Ctx, addr: u32, size: usize, v: u128) {
    let _ = c;
    let m = machine();
    let a = phys(addr);
    match a {
        0x1000_0000..=0x1000_1FFF => {
            let now = m.now();
            crate::timers::update(m, now);
            if a & !0x30 == 0x1000_0800 && diag::enabled("T5_T1LOG") {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("t1log.txt") {
                    use std::io::Write;
                    let _ = writeln!(f, "W {:#x} = {:#x} (count {:#x} mode {:#x})", a, v as u32, m.timers.read(0x1000_0800, now), m.timers.t[1].mode);
                }
            }
            m.timers.write(a, v as u32, now)
        }
        0x1000_2000..=0x1000_2FFF => crate::ipu::write(m, a, v as u64),
        0x1000_3000..=0x1000_37FF => match m.mtvu.as_mut() {
            Some(t) => t.send(crate::mtvu::Cmd::GifReg(a, v as u32)),
            None => m.gif.write(a, v as u32),
        },
        0x1000_3800..=0x1000_3BFF => crate::vif::reg_write(m, 0, a, v as u32),
        0x1000_3C00..=0x1000_3FFF if { if crate::diag::enabled("T5_PRIMSTATS") { diag::count_with(|| format!("ee_write_vif1_{:x}_{:x}", a & 0x3F0, v as u32 & 0xF)); } false } => {}
        0x1000_3C00..=0x1000_3FFF => match m.mtvu.as_mut() {
            Some(t) if a & 0x3F0 == 0x010 => t.fbrst(v as u32),
            Some(t) => t.send(crate::mtvu::Cmd::VifReg(a, v as u32)),
            None => crate::vif::reg_write(m, 1, a, v as u32),
        },
        0x1000_4000..=0x1000_4FFF => crate::vif::fifo_write(m, 0, v),
        0x1000_5000..=0x1000_5FFF => match m.mtvu.as_mut() {
            Some(t) => t.send(crate::mtvu::Cmd::Vif1Fifo(v)),
            None => crate::vif::fifo_write(m, 1, v),
        },
        0x1000_6000..=0x1000_6FFF => match m.mtvu.as_mut() {
            Some(t) => t.send(crate::mtvu::Cmd::GifFifo(v)),
            None => crate::gif::fifo_write(m, v),
        },
        0x1000_7010..=0x1000_701F => crate::ipu::fifo_write(m, v),
        0x1000_7000..=0x1000_700F => {}
        0x1000_8000..=0x1000_EFFF => crate::dmac::write(m, a, v as u32),
        0x1000_F000 => {
            // I_STAT: write 1 to clear.
            m.istat &= !(v as u32);
        }
        0x1000_F010 => {
            // I_MASK: write 1 to toggle.
            m.imask ^= v as u32 & 0x7FFF;
            ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        0x1000_F200..=0x1000_F2FF => crate::sif::hw_write(m, a, v as u32),
        0x1000_F590 => m.dmac.enable = v as u32,
        0x1000_F000..=0x1000_FFFF => diag::count_with(|| format!("io write {a:#010x}")),
        0x1100_0000..=0x1100_FFFF => vu_mem_write(m, a, size, v),
        0x1200_0000..=0x1200_1FFF => {
            m.gs.priv_write(a, v as u64);
            if a & 0x1FF0 == 0x1000 && v & 0x200 != 0 {
                if let Some(t) = m.mtvu.as_mut() {
                    t.send(crate::mtvu::Cmd::GsReset);
                }
            }
        }
        _ => {
            diag::count("io write unmapped");
            diag::once(format!("write to unmapped page {:#010x} (size {size}) in {:#010x}", addr & !0xFFF, ee_core::LAST_FN.load(std::sync::atomic::Ordering::Relaxed)));
        }
    }
}

/// VU0 micro (4 KiB) / data (4 KiB) at 0x11000000 / 0x11004000, VU1 micro (16 KiB) / data
/// (16 KiB) at 0x11008000 / 0x1100C000.
fn vu_slice(m: &mut Machine, a: u32) -> (&mut [u8], usize) {
    let o = (a - 0x1100_0000) as usize;
    match o {
        0x0000..=0x3FFF => (&mut m.vu0.code[..], o & 0xFFF),
        0x4000..=0x7FFF => (&mut m.vu0.data[..], o & 0xFFF),
        0x8000..=0xBFFF => (&mut m.vu1.code[..], o & 0x3FFF),
        _ => (&mut m.vu1.data[..], o & 0x3FFF),
    }
}

/// Wait for the MTVU worker before the EE reads VIF1 / GIF / VU1 state.
fn mtvu_sync(m: &Machine) {
    if let Some(t) = m.mtvu.as_ref() {
        t.sync();
    }
}

fn vu_mem_read(m: &mut Machine, a: u32, size: usize) -> u128 {
    if a >= 0x1100_8000 {
        mtvu_sync(m);
    }
    if crate::diag::enabled("T5_PRIMSTATS") && a >= 0x1100_8000 {
        diag::count("ee_read_vu1_mem");
    }
    let (mem, o) = vu_slice(m, a);
    let mut v = 0u128;
    for i in 0..size.min(16) {
        v |= (mem[(o + i) % mem.len()] as u128) << (8 * i);
    }
    v
}

fn vu_mem_write(m: &mut Machine, a: u32, size: usize, v: u128) {
    if a >= 0x1100_8000 {
        mtvu_sync(m);
    }
    match a - 0x1100_0000 {
        0x0000..=0x3FFF => m.vu0.code_gen += 1,
        0x8000..=0xBFFF => m.vu1.code_gen += 1,
        _ => {}
    }
    let (mem, o) = vu_slice(m, a);
    let n = mem.len();
    for i in 0..size.min(16) {
        mem[(o + i) % n] = (v >> (8 * i)) as u8;
    }
}
