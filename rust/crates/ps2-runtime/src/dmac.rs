//! The EE DMA controller (DMAC): ten channels, normal / chain / interleave transfers.
//!
//! A transfer runs to completion as soon as the program starts it (CHCR.STR = 1): the
//! destination devices (VIF, GIF, IPU, scratchpad) consume the data at once, so the guest sees
//! the transfer finished at its next look at the channel. Completion sets the channel's D_STAT
//! CIS bit; an enabled one (CIM) is an INT1 the kernel delivers to the AddDmacHandler handlers.
//! The GIF channel waits while VIF1 masks PATH3 (MSKPATH3).

use crate::{bus, diag, Machine};

pub const VIF0: usize = 0;
pub const VIF1: usize = 1;
pub const GIF: usize = 2;
pub const IPU_FROM: usize = 3;
pub const IPU_TO: usize = 4;
pub const SIF0: usize = 5;
pub const SIF1: usize = 6;
pub const SIF2: usize = 7;
pub const SPR_FROM: usize = 8;
pub const SPR_TO: usize = 9;

const BASES: [u32; 10] = [0x8000, 0x9000, 0xA000, 0xB000, 0xB400, 0xC000, 0xC400, 0xC800, 0xD000, 0xD400];

#[derive(Default, Clone, Copy)]
pub struct Channel {
    pub chcr: u32,
    pub madr: u32,
    pub qwc: u32,
    pub tadr: u32,
    pub asr: [u32; 2],
    pub sadr: u32,
    /// Started while it could not run (PATH3 masked): run when possible.
    pub waiting: bool,
}

#[derive(Default)]
pub struct Dmac {
    pub ch: [Channel; 10],
    pub ctrl: u32,
    pub stat: u32,
    pub pcr: u32,
    pub sqwc: u32,
    pub rbsr: u32,
    pub rbor: u32,
    pub stadr: u32,
    pub enable: u32,
}

pub fn reset(m: &mut Machine) {
    m.dmac = Dmac::default();
    m.dmac.enable = 0x1201;
}

/// ResetEE(flags): bit 0 DMAC, 1 VU1, 2 VIF1, 3 GIF, 4 VU0, 5 VIF0, 6 IPU.
pub fn reset_ee(m: &mut Machine, flags: u32) {
    if flags & 1 != 0 {
        reset(m);
    }
    if flags & 4 != 0 {
        if let Some(t) = m.mtvu.as_ref() {
            t.sync();
        }
        m.vif[1] = crate::vif::Vif::new(1);
    }
    if flags & 0x20 != 0 {
        m.vif[0] = crate::vif::Vif::new(0);
    }
    if flags & 8 != 0 {
        m.gif = crate::gif::Gif::default();
    }
}

fn chan_of(a: u32) -> Option<usize> {
    let base = a & 0xFF00;
    BASES.iter().position(|&b| b == base)
}

pub fn read(m: &mut Machine, a: u32) -> u32 {
    let lo = a & 0xFFFF;
    if lo >= 0xE000 {
        return match lo {
            0xE000 => m.dmac.ctrl,
            0xE010 => m.dmac.stat,
            0xE020 => m.dmac.pcr,
            0xE030 => m.dmac.sqwc,
            0xE040 => m.dmac.rbsr,
            0xE050 => m.dmac.rbor,
            0xE060 => m.dmac.stadr,
            _ => 0,
        };
    }
    let Some(n) = chan_of(a) else { return 0 };
    let c = &m.dmac.ch[n];
    match lo & 0xFF {
        0x00 => c.chcr,
        0x10 => c.madr,
        0x20 => c.qwc,
        0x30 => c.tadr,
        0x40 => c.asr[0],
        0x50 => c.asr[1],
        0x80 => c.sadr,
        _ => 0,
    }
}

pub fn write(m: &mut Machine, a: u32, v: u32) {
    let lo = a & 0xFFFF;
    if lo >= 0xE000 {
        match lo {
            0xE000 => m.dmac.ctrl = v,
            0xE010 => {
                // CIS bits: write 1 clears; CIM bits: write 1 toggles.
                m.dmac.stat &= !(v & 0x0000_E3FF);
                m.dmac.stat ^= v & 0x63FF_0000;
                ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            0xE020 => m.dmac.pcr = v,
            0xE030 => m.dmac.sqwc = v,
            0xE040 => m.dmac.rbsr = v,
            0xE050 => m.dmac.rbor = v,
            0xE060 => m.dmac.stadr = v,
            _ => {}
        }
        return;
    }
    let Some(n) = chan_of(a) else { return };
    match lo & 0xFF {
        0x00 => {
            let was = m.dmac.ch[n].chcr & 0x100 != 0;
            m.dmac.ch[n].chcr = v;
            if v & 0x100 != 0 && !was {
                start(m, n);
            }
        }
        0x10 => m.dmac.ch[n].madr = v & !0xF,
        0x20 => m.dmac.ch[n].qwc = v & 0xFFFF,
        0x30 => m.dmac.ch[n].tadr = v & !0xF,
        0x40 => m.dmac.ch[n].asr[0] = v & !0xF,
        0x50 => m.dmac.ch[n].asr[1] = v & !0xF,
        0x80 => m.dmac.ch[n].sadr = v & 0x3FF0,
        _ => {}
    }
}

/// INT1 sources: channels with CIS and CIM set.
pub fn pending(m: &Machine) -> u32 {
    m.dmac.stat & (m.dmac.stat >> 16) & 0x3FF
}

pub fn ack(m: &mut Machine, ch: u32) {
    m.dmac.stat &= !(1 << ch);
}

/// _EnableDmac / _DisableDmac: set CIM; returns the old state.
pub fn enable(m: &mut Machine, ch: u32, on: bool) -> u32 {
    let bit = 1 << (16 + (ch & 15));
    let old = (m.dmac.stat & bit != 0) as u32;
    if on {
        m.dmac.stat |= bit;
    } else {
        m.dmac.stat &= !bit;
    }
    if on { old ^ 1 } else { old }
}

/// COP0 condition 0: every channel selected in D_PCR.CPC has finished (CIS set).
pub fn cpcond0(m: &Machine) -> bool {
    let cpc = m.dmac.pcr & 0x3FF;
    (!cpc | m.dmac.stat) & 0x3FF == 0x3FF
}

/// A channel finished: STR off, CIS on (INT1 if enabled).
pub fn finish(m: &mut Machine, n: usize) {
    m.dmac.ch[n].chcr &= !0x100;
    m.dmac.ch[n].qwc = 0;
    m.dmac.stat |= 1 << n;
    if pending(m) != 0 {
        ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn start(m: &mut Machine, n: usize) {
    if n == 1 || n == 2 {
        crate::diag::tl(if n == 1 { "EE dma vif1" } else { "EE dma gif" });
    }
    if crate::diag::enabled("T5_PRIMSTATS") {
        diag::count_with(|| format!("dma_start_ch{n}"));
    }
    if m.dmac.ctrl & 1 == 0 {
        diag::count("dma start while DMAC disabled");
    }
    match n {
        SIF0 => {
            crate::sif::sif0_armed(m);
            return;
        }
        SIF1 | SIF2 => {
            diag::once(format!("direct SIF{} DMA start (chcr {:#x})", n - 5, m.dmac.ch[n].chcr));
            finish(m, n);
            return;
        }
        // MTVU: the worker runs VIF1 (from memory) and GIF transfers, in order.
        VIF1 | GIF if m.mtvu.is_some() && !(n == VIF1 && m.dmac.ch[n].chcr & 1 == 0) => {
            let ch = m.dmac.ch[n];
            m.mtvu.as_mut().unwrap().send(crate::mtvu::Cmd::Dma(n, ch));
            return;
        }
        VIF1 if m.mtvu.is_some() => {
            // VIF1 -> memory reads what the worker produced.
            m.mtvu.as_ref().unwrap().sync();
        }
        GIF if m.gif.path3_masked() => {
            m.dmac.ch[n].waiting = true;
            return;
        }
        _ => {}
    }
    run(m, n);
}

/// Run a started transfer to the end.
pub fn run(m: &mut Machine, n: usize) {
    m.dmac.ch[n].waiting = false;
    let chcr = m.dmac.ch[n].chcr;
    let mode = (chcr >> 2) & 3;
    let to_mem = chcr & 1 == 0;
    match n {
        VIF1 if to_mem && mode == 0 => {
            // VIF1 -> memory (GS local memory download through the VIF1 FIFO).
            let q = m.dmac.ch[n].qwc;
            let mut madr = m.dmac.ch[n].madr;
            for _ in 0..q {
                let v = m.gs.download_qword();
                bus::set_qword(m, madr, false, v);
                madr += 16;
            }
            m.dmac.ch[n].madr = madr;
            finish(m, n);
            return;
        }
        IPU_FROM => {
            crate::ipu::dma_from(m);
            return;
        }
        IPU_TO => {
            crate::ipu::dma_start(m);
            return;
        }
        SPR_FROM => {
            spr_from(m);
            finish(m, n);
            return;
        }
        _ => {}
    }
    if mode == 1 {
        if !chain(m, n) {
            return; // stopped on a VIF stall; resumed by kick()
        }
    } else {
        let q = m.dmac.ch[n].qwc;
        let madr = m.dmac.ch[n].madr;
        let interleave = mode == 2 && n == SPR_TO;
        transfer(m, n, madr, q, interleave);
        m.dmac.ch[n].madr = madr + 16 * q;
    }
    finish(m, n);
}

/// Move `qwc` quadwords from `madr` to channel `n`'s device.
fn transfer(m: &mut Machine, n: usize, madr: u32, qwc: u32, interleave: bool) {
    if qwc == 0 {
        return;
    }
    let spr = madr & 0x8000_0000 != 0;
    let mut data = Vec::with_capacity(qwc as usize);
    if interleave {
        let tqwc = (m.dmac.sqwc >> 16) & 0xFF;
        let sqwc = m.dmac.sqwc & 0xFF;
        let mut a = madr;
        let mut left = qwc;
        while left > 0 {
            for _ in 0..tqwc.max(1).min(left) {
                data.push(bus::qword(m, a, spr));
                a += 16;
                left -= 1;
            }
            a += 16 * sqwc;
        }
    } else {
        for i in 0..qwc {
            data.push(bus::qword(m, madr + 16 * i, spr));
        }
    }
    deliver(m, n, &data);
}

fn deliver(m: &mut Machine, n: usize, data: &[u128]) {
    match n {
        VIF0 | VIF1 => crate::vif::feed(m, n, data),
        GIF => crate::gif::path3(m, data),
        SPR_TO => {
            let mut s = m.dmac.ch[SPR_TO].sadr;
            for q in data {
                bus::set_qword(m, s, true, *q);
                s = (s + 16) & 0x3FF0;
            }
            m.dmac.ch[SPR_TO].sadr = s;
        }
        _ => diag::count_with(|| format!("dma ch{n} data dropped")),
    }
}

/// MTVU worker: run a VIF1 / GIF transfer described by `ch` (a snapshot of the registers at
/// the start). False if a VIF1 stall stopped a chain (call again once it resumes).
pub fn run_job(m: &mut Machine, n: usize, ch: &mut Channel) -> bool {
    if (ch.chcr >> 2) & 3 == 1 {
        return chain_ch(m, n, ch);
    }
    let q = ch.qwc;
    transfer(m, n, ch.madr, q, false);
    ch.madr += 16 * q;
    ch.qwc = 0;
    true
}

/// Source chain mode on the channel's registers (updated as it goes: a nested resume from
/// `kick` continues where this one is).
fn chain(m: &mut Machine, n: usize) -> bool {
    let ch: *mut Channel = &mut m.dmac.ch[n];
    chain_ch(m, n, unsafe { &mut *ch })
}

/// Source chain mode: follow DMAtags from TADR. False if it stopped before the end (VIF1 stall).
pub fn chain_ch(m: &mut Machine, n: usize, ch: &mut Channel) -> bool {
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 1_000_000 {
            diag::log(format!("dma ch{n}: runaway chain at {:#010x}", ch.tadr));
            break;
        }
        let tadr = ch.tadr;
        let spr = tadr & 0x8000_0000 != 0;
        let tag = bus::qword(m, tadr, spr);
        let lo = tag as u64;
        let qwc = (lo & 0xFFFF) as u32;
        let id = ((lo >> 28) & 7) as u32;
        let irq = lo >> 31 & 1 != 0;
        let addr = ((lo >> 32) as u32) & 0xFFFF_FFF0;
        let chcr = ch.chcr;
        ch.chcr = (chcr & 0xFFFF) | ((lo as u32) & 0xFFFF_0000);
        if chcr & 0x40 != 0 && (n == VIF0 || n == VIF1) {
            // TTE: the tag's upper 64 bits go to the VIF as two VIF codes.
            crate::vif::feed_words(m, n, &[(tag >> 64) as u32, (tag >> 96) as u32]);
        }
        let mut end = false;
        match id {
            0 => {
                // refe
                transfer(m, n, addr, qwc, false);
                ch.madr = addr + 16 * qwc;
                ch.tadr = tadr + 16;
                end = true;
            }
            1 => {
                // cnt
                transfer(m, n, tadr + 16, qwc, false);
                ch.madr = tadr + 16 + 16 * qwc;
                ch.tadr = tadr + 16 + 16 * qwc;
            }
            2 => {
                // next
                transfer(m, n, tadr + 16, qwc, false);
                ch.madr = tadr + 16 + 16 * qwc;
                ch.tadr = addr;
            }
            3 | 4 => {
                // ref / refs
                transfer(m, n, addr, qwc, false);
                ch.madr = addr + 16 * qwc;
                ch.tadr = tadr + 16;
            }
            5 => {
                // call
                transfer(m, n, tadr + 16, qwc, false);
                let ret = tadr + 16 + 16 * qwc;
                let asp = (chcr >> 4) & 3;
                if asp < 2 {
                    ch.asr[asp as usize] = ret;
                    ch.chcr = (ch.chcr & !0x30) | ((asp + 1) << 4);
                } else {
                    diag::count("dma call stack overflow");
                }
                ch.tadr = addr;
            }
            6 => {
                // ret
                transfer(m, n, tadr + 16, qwc, false);
                let asp = (ch.chcr >> 4) & 3;
                if asp > 0 {
                    let a = ch.asr[(asp - 1) as usize];
                    ch.chcr = (ch.chcr & !0x30) | ((asp - 1) << 4);
                    ch.tadr = a;
                } else {
                    end = true;
                }
            }
            _ => {
                // end
                transfer(m, n, tadr + 16, qwc, false);
                ch.madr = tadr + 16 + 16 * qwc;
                end = true;
            }
        }
        if end || (irq && chcr & 0x80 != 0) {
            return true;
        }
        if n == VIF1 && m.vif[1].stalled() {
            // VIF1 stopped (an interrupt bit / a stall): resume later.
            return false;
        }
    }
    true
}

/// SPR_FROM: scratchpad -> memory (normal or interleave; chain mode uses destination tags).
fn spr_from(m: &mut Machine) {
    let c = m.dmac.ch[SPR_FROM];
    let mode = (c.chcr >> 2) & 3;
    let mut sadr = c.sadr;
    let mut madr = c.madr;
    let move_q = |m: &mut Machine, madr: &mut u32, sadr: &mut u32, q: u32, skip: u32, take: u32| {
        let mut left = q;
        while left > 0 {
            for _ in 0..take.max(1).min(left) {
                let v = bus::qword(m, *sadr, true);
                bus::set_qword(m, *madr, false, v);
                *madr += 16;
                *sadr = (*sadr + 16) & 0x3FF0;
                left -= 1;
            }
            *madr += 16 * skip;
        }
    };
    match mode {
        1 => {
            // Destination chain: tags come from the scratchpad.
            for _ in 0..4096 {
                let tag = bus::qword(m, sadr, true) as u64;
                sadr = (sadr + 16) & 0x3FF0;
                let qwc = (tag & 0xFFFF) as u32;
                let id = (tag >> 28) & 7;
                madr = ((tag >> 32) as u32) & !0xF;
                move_q(m, &mut madr, &mut sadr, qwc, 0, qwc);
                if id == 7 || (tag >> 31 & 1 != 0 && c.chcr & 0x80 != 0) {
                    break;
                }
            }
        }
        2 => {
            let tq = (m.dmac.sqwc >> 16) & 0xFF;
            let sq = m.dmac.sqwc & 0xFF;
            move_q(m, &mut madr, &mut sadr, c.qwc, sq, tq);
        }
        _ => move_q(m, &mut madr, &mut sadr, c.qwc, 0, c.qwc),
    }
    m.dmac.ch[SPR_FROM].sadr = sadr;
    m.dmac.ch[SPR_FROM].madr = madr;
}

/// Run channels that were waiting (PATH3 unmasked, VIF1 resumed).
pub fn kick(m: &mut Machine) {
    if crate::mtvu::on_worker() {
        // The worker resumes its own VIF1 chain; PATH3 transfers waiting for the mask run now.
        crate::mtvu::path3_unmasked(m);
        return;
    }
    if m.dmac.ch[GIF].waiting && !m.gif.path3_masked() {
        run(m, GIF);
    }
    if m.dmac.ch[VIF1].chcr & 0x100 != 0 && !m.vif[1].stalled() && (m.dmac.ch[VIF1].chcr >> 2) & 3 == 1 {
        // Resume a VIF1 chain that stopped on a VIF stall.
        if chain(m, VIF1) {
            finish(m, VIF1);
        }
    }
}
