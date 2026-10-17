//! The GIF: unpacks GIF packets (tag + data in PACKED, REGLIST or IMAGE format) from its three
//! paths (PATH1: VU1 XGKICK, PATH2: VIF1 DIRECT/DIRECTHL, PATH3: DMA channel 2 / the GIF FIFO)
//! into GS register writes. A packet may span several transfers, so each path keeps its own
//! unpacking state.

use crate::{gs::GsFront, Machine};

#[derive(Default, Clone, Copy)]
pub struct PathState {
    /// Data qwords (or register writes) left in the current GIFtag's loop.
    pub nloop: u32,
    pub nreg: u32,
    pub regs: u64,
    pub flg: u32,
    pub eop: bool,
    /// Register index within the current loop (PACKED / REGLIST).
    pub reg: u32,
    pub active: bool,
    /// Q from the last ST in PACKED mode (used by RGBAQ).
    pub q: u32,
}

#[derive(Default)]
pub struct Gif {
    pub ctrl: u32,
    pub mode: u32,
    pub stat: u32,
    pub paths: [PathState; 3],
    pub tag: [u32; 4],
}

impl Gif {
    pub fn read(&self, a: u32) -> u32 {
        match a & 0xFF {
            0x00 => self.ctrl,
            0x10 => self.mode,
            0x20 => self.stat,
            0x40 => self.tag[0],
            0x50 => self.tag[1],
            0x60 => self.tag[2],
            0x70 => self.tag[3],
            _ => 0,
        }
    }

    pub fn write(&mut self, a: u32, v: u32) {
        match a & 0xFF {
            0x00 => {
                self.ctrl = v & 8;
                if v & 1 != 0 {
                    // RST
                    self.paths = Default::default();
                }
            }
            0x10 => self.mode = v & 5,
            _ => {}
        }
    }

    /// PATH3 is masked by VIF1 (MSKPATH3) or GIF_MODE.M3R.
    pub fn path3_masked(&self) -> bool {
        self.mode & 1 != 0 || crate::machine().vif[1].mskpath3
    }
}

/// T5_TBPWATCH: the path feeding the GS right now (path << 16 | XGKICK address) and the start PC
/// of the running VU1 program, for the TEX0 watch in `GsFront::write_reg`.
pub static CUR_PATH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static VU1_START: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn set_path(v: u32) {
    if crate::diag::enabled("T5_TBPWATCH") {
        CUR_PATH.store(v, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Feed PATH3 data (DMA channel 2).
pub fn path3(m: &mut Machine, data: &[u128]) {
    set_path(3 << 16);
    feed(&mut m.gs, &mut m.gif.paths[2], data);
    gs_irq(m);
}

/// The GIF FIFO (direct EE writes).
pub fn fifo_write(m: &mut Machine, v: u128) {
    set_path(4 << 16);
    feed(&mut m.gs, &mut m.gif.paths[2], &[v]);
    gs_irq(m);
}

/// PATH2 (VIF1 DIRECT) data.
pub fn path2(m: &mut Machine, data: &[u128]) {
    set_path(2 << 16);
    feed(&mut m.gs, &mut m.gif.paths[1], data);
    gs_irq(m);
}

/// PATH1 (VU1 XGKICK): packets from VU1 data memory starting at qword `addr`, until EOP.
pub fn path1(m: &mut Machine, addr: u32) {
    if crate::diag::enabled("T5_PRIMSTATS") {
        let o = ((addr & 0x3FF) as usize) * 16;
        let tag = u128::from_le_bytes(m.vu1.data[o..o + 16].try_into().unwrap());
        let lo = tag as u64;
        crate::diag::count_with(|| format!("kick_n{}_flg{}_nreg{}_regs{:x}", lo & 0x7FFF, (lo >> 58) & 3, (lo >> 60) & 15, (tag >> 64) as u64 & 0xFFFF_FFFF));
    }
    set_path(1 << 16 | (addr & 0x3FF));
    path1_run(m, addr);
    gs_irq(m);
}

/// A SIGNAL / FINISH register write raised the GS interrupt.
fn gs_irq(m: &mut Machine) {
    if m.gs.irq {
        m.gs.irq = false;
        m.raise(crate::INTC_GS);
    }
}

fn path1_run(m: &mut Machine, addr: u32) {
    let mut a = addr & 0x3FF;
    let mut st = PathState::default();
    for _ in 0..1024 {
        let o = (a as usize) * 16;
        let q = u128::from_le_bytes(m.vu1.data[o..o + 16].try_into().unwrap());
        feed(&mut m.gs, &mut st, &[q]);
        a = (a + 1) & 0x3FF;
        if !st.active && st.eop {
            break;
        }
    }
}

/// Unpack qwords through a path's state machine.
pub fn feed(gs: &mut GsFront, st: &mut PathState, data: &[u128]) {
    for &q in data {
        if !st.active {
            // A GIFtag.
            let lo = q as u64;
            let hi = (q >> 64) as u64;
            st.nloop = (lo & 0x7FFF) as u32;
            st.eop = lo >> 15 & 1 != 0;
            let pre = lo >> 46 & 1 != 0;
            let prim = (lo >> 47) & 0x7FF;
            st.flg = ((lo >> 58) & 3) as u32;
            st.nreg = ((lo >> 60) & 0xF) as u32;
            if st.nreg == 0 {
                st.nreg = 16;
            }
            st.regs = hi;
            st.reg = 0;
            st.q = 0x3F80_0000;
            if pre && st.flg == 0 {
                gs.write_reg(0x00, prim);
            }
            st.active = st.nloop > 0;
            continue;
        }
        match st.flg {
            0 => {
                packed(gs, st, q);
                st.reg += 1;
                if st.reg >= st.nreg {
                    st.reg = 0;
                    st.nloop -= 1;
                }
            }
            1 => {
                for half in 0..2 {
                    if st.nloop == 0 {
                        break;
                    }
                    let r = ((st.regs >> (4 * st.reg)) & 0xF) as u32;
                    let v = (q >> (64 * half)) as u64;
                    if r != 0xE && r != 0xF {
                        gs.write_reg(r, v);
                    }
                    st.reg += 1;
                    if st.reg >= st.nreg {
                        st.reg = 0;
                        st.nloop -= 1;
                    }
                }
            }
            _ => {
                gs.image_data(&q.to_le_bytes());
                st.nloop -= 1;
            }
        }
        if st.nloop == 0 {
            st.active = false;
        }
    }
}

/// One PACKED-mode register qword.
fn packed(gs: &mut GsFront, st: &mut PathState, q: u128) {
    let r = ((st.regs >> (4 * st.reg)) & 0xF) as u32;
    let w = |i: u32| (q >> (32 * i)) as u32;
    match r {
        0x0 => gs.write_reg(0x00, (q & 0x7FF) as u64),
        0x1 => {
            let v = (w(0) & 0xFF) as u64 | ((w(1) & 0xFF) as u64) << 8 | ((w(2) & 0xFF) as u64) << 16 | ((w(3) & 0xFF) as u64) << 24 | (st.q as u64) << 32;
            gs.write_reg(0x01, v);
        }
        0x2 => {
            st.q = w(2);
            gs.write_reg(0x02, w(0) as u64 | (w(1) as u64) << 32);
        }
        0x3 => gs.write_reg(0x03, (w(0) & 0x3FFF) as u64 | ((w(1) & 0x3FFF) as u64) << 16),
        0x4 => {
            let adc = (q >> 111) & 1 != 0;
            let v = (w(0) & 0xFFFF) as u64 | ((w(1) & 0xFFFF) as u64) << 16 | (((w(2) >> 4) & 0xFF_FFFF) as u64) << 32 | (((w(3) >> 4) & 0xFF) as u64) << 56;
            gs.write_reg(if adc { 0x0C } else { 0x04 }, v);
        }
        0x5 => {
            let adc = (q >> 111) & 1 != 0;
            let v = (w(0) & 0xFFFF) as u64 | ((w(1) & 0xFFFF) as u64) << 16 | (w(2) as u64) << 32;
            gs.write_reg(if adc { 0x0D } else { 0x05 }, v);
        }
        0xA => gs.write_reg(0x0A, (((w(3) >> 4) & 0xFF) as u64) << 56),
        0xE => gs.write_reg(((q >> 64) & 0xFF) as u32, q as u64),
        0xF => {}
        _ => gs.write_reg(r, q as u64),
    }
}
