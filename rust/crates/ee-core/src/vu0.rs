//! Vector units: the register file, the upper-instruction arithmetic (shared by VU0 macro mode
//! and the micro-mode interpreter in `ps2-runtime`) and COP2 macro instructions.
//!
//! VU floats follow the same rules as the EE FPU (no denormals / infinities / NaNs, round
//! toward zero, clamp on overflow), see [`crate::fpu`].
//!
//! The generator emits `vu0::exec(c, WORD)` for every COP2 instruction whose p-code Ghidra
//! leaves opaque; `_lqc2` / `_sqc2` stay p-code (a 16-byte LOAD / STORE and a copy).

use crate::fpu::{f64_to_ps2, ps2_to_f64};
use crate::{Ctx, REG_VACC, REG_VF, REG_VI, VI_CLIP, VI_I, VI_MAC, VI_P, VI_Q, VI_R, VI_STATUS};

/// A vector unit's registers. For VU0 this overlays `Ctx::regs` from REG_VI (macro mode and
/// micro mode share it); VU1 has its own.
#[repr(C)]
pub struct VuRegs {
    /// vi0..vi15 (16-bit integer registers, vi0 = 0), then the control registers 16..31
    /// (Status, MAC, Clip, -, R, I, Q, P, -, -, TPC, CMSAR0, FBRST, VPU-STAT, -, CMSAR1).
    pub vi: [u32; 32],
    _pad: [u8; REG_VF - REG_VI - 128],
    /// vf0..vf31 as x, y, z, w (f32 bit patterns); vf0 = (0, 0, 0, 1).
    pub vf: [[u32; 4]; 32],
    pub acc: [u32; 4],
}

const _: () = assert!(REG_VACC - REG_VI == 128 + (REG_VF - REG_VI - 128) + 512);

impl VuRegs {
    pub fn new() -> Box<VuRegs> {
        let mut r: Box<VuRegs> = Box::new(unsafe { std::mem::zeroed() });
        r.vf[0] = [0, 0, 0, 0x3F80_0000];
        r
    }
}

/// VU0's registers inside the EE context.
#[inline(always)]
pub fn regs(c: &mut Ctx) -> &mut VuRegs {
    unsafe { &mut *(c.regs.as_mut_ptr().add(REG_VI) as *mut VuRegs) }
}

/// The dest mask field (bits 21..24) as a per-component bool array in x, y, z, w order.
#[inline(always)]
pub fn dest(word: u32) -> [bool; 4] {
    [word >> 24 & 1 != 0, word >> 23 & 1 != 0, word >> 22 & 1 != 0, word >> 21 & 1 != 0]
}

#[inline(always)] fn f(x: u32) -> f64 { ps2_to_f64(x) }
#[inline(always)] fn r(x: f64) -> u32 { f64_to_ps2(x) }

/// Write `vals` to the masked fields of vf[fd] (or ACC when `fd` is None) and update the MAC
/// and Status flags like an FMAC operation.
pub fn write_fmac(v: &mut VuRegs, fd: Option<usize>, d: [bool; 4], vals: [f64; 4]) {
    fmac_lanes::<true>(v, fd, d, |k| vals[k]);
}

/// The FMAC write-back: compute only the lanes the dest mask selects (`lane(k)` gives the exact
/// f64 result of lane k), round each to an EE single, set MAC and Status.
/// With `FLAGS` false the MAC and Status flags are left alone (micro code that never reads them).
#[inline(always)]
fn fmac_lanes<const FLAGS: bool>(v: &mut VuRegs, fd: Option<usize>, d: [bool; 4], lane: impl Fn(usize) -> f64) {
    let mut mac = 0u32;
    let mut out = match fd {
        Some(n) => v.vf[n],
        None => v.acc,
    };
    for i in 0..4 {
        if !d[i] {
            continue;
        }
        let bit = 3 - i; // x is the high bit of each nibble
        let x = lane(i);
        let bits = r(x);
        out[i] = bits;
        if !FLAGS {
            continue;
        }
        if bits & 0x7FFF_FFFF == 0 {
            mac |= 1 << bit; // zero
            let e = (x.to_bits() >> 52) & 0x7FF;
            if x != 0.0 && e != 0 {
                mac |= 1 << (bit + 8); // underflow (flushed)
            }
        }
        if bits & 0x8000_0000 != 0 {
            mac |= 1 << (bit + 4);
        }
        if bits & 0x7FFF_FFFF == 0x7FFF_FFFF && (x.abs() > PS2_MAX) {
            mac |= 1 << (bit + 12); // overflow (clamped)
        }
    }
    match fd {
        Some(0) => {}
        Some(n) => v.vf[n] = out,
        None => v.acc = out,
    }
    if FLAGS {
        set_mac(v, mac);
    }
}

/// The largest EE single (0x7FFFFFFF) as f64.
const PS2_MAX: f64 = 6.805646932770577e38;

#[inline]
fn set_mac(v: &mut VuRegs, mac: u32) {
    v.vi[VI_MAC] = mac;
    let mut st = v.vi[VI_STATUS] & 0xFF0; // keep sticky bits, D and I
    st &= !0x0F;
    if mac & 0x000F != 0 { st |= 1; }
    if mac & 0x00F0 != 0 { st |= 2; }
    if mac & 0x0F00 != 0 { st |= 4; }
    if mac & 0xF000 != 0 { st |= 8; }
    st |= (st & 0xF) << 6;
    v.vi[VI_STATUS] = st | (v.vi[VI_STATUS] & 0x30);
}

/// One upper instruction (the low 26 bits of a macro-mode COP2 word or a micro-mode upper
/// word share the encoding). `q`, `i` are the Q and I register values to use.
#[inline(always)]
pub fn upper(v: &mut VuRegs, word: u32) {
    upper_ex::<true>(v, word)
}

/// `upper` with or without the MAC / Status flag updates (`FLAGS`).
#[inline(always)]
pub fn upper_ex<const FLAGS: bool>(v: &mut VuRegs, word: u32) {
    let d = dest(word);
    let ft = (word >> 16 & 31) as usize;
    let fs = (word >> 11 & 31) as usize;
    let fd = (word >> 6 & 31) as usize;
    let bc = (word & 3) as usize;
    // Raw operands; lanes are converted only where the dest mask needs them.
    let s = v.vf[fs];
    let t = v.vf[ft];
    let acc = v.acc;
    let q = v.vi[VI_Q];
    let ii = v.vi[VI_I];
    let tb = t[bc];
    // Products are rounded to EE singles before accumulation (as the FMAC does).
    let rr = |x: f64| f(r(x));
    let funct = word & 0x3F;
    if funct < 0x3C {
        let dst = Some(fd);
        match funct {
            0x00..=0x03 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) + f(tb)),
            0x04..=0x07 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) - f(tb)),
            0x08..=0x0B => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) + rr(f(s[k]) * f(tb))),
            0x0C..=0x0F => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) - rr(f(s[k]) * f(tb))),
            0x10..=0x13 => minmax(v, fd, d, fs, [tb; 4], true),
            0x14..=0x17 => minmax(v, fd, d, fs, [tb; 4], false),
            0x18..=0x1B => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) * f(tb)),
            0x1C => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) * f(q)),
            0x1D => minmax(v, fd, d, fs, [ii; 4], true),
            0x1E => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) * f(ii)),
            0x1F => minmax(v, fd, d, fs, [ii; 4], false),
            0x20 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) + f(q)),
            0x21 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) + rr(f(s[k]) * f(q))),
            0x22 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) + f(ii)),
            0x23 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) + rr(f(s[k]) * f(ii))),
            0x24 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) - f(q)),
            0x25 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) - rr(f(s[k]) * f(q))),
            0x26 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) - f(ii)),
            0x27 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) - rr(f(s[k]) * f(ii))),
            0x28 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) + f(t[k])),
            0x29 => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) + rr(f(s[k]) * f(t[k]))),
            0x2A => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) * f(t[k])),
            0x2B => minmax(v, fd, d, fs, t, true),
            0x2C => fmac_lanes::<FLAGS>(v, dst, d, |k| f(s[k]) - f(t[k])),
            0x2D => fmac_lanes::<FLAGS>(v, dst, d, |k| f(acc[k]) - rr(f(s[k]) * f(t[k]))),
            0x2E => {
                // OPMSUB: fd.xyz = ACC.xyz - fs.yzx * ft.zxy
                let (sy, sz, sx) = (1, 2, 0);
                fmac_lanes::<FLAGS>(v, dst, [true, true, true, false], |k| {
                    let p = match k {
                        0 => f(s[sy]) * f(t[2]),
                        1 => f(s[sz]) * f(t[0]),
                        _ => f(s[sx]) * f(t[1]),
                    };
                    f(acc[k]) - rr(p)
                });
            }
            0x2F => minmax(v, fd, d, fs, t, false),
            _ => {}
        }
        return;
    }
    let op = (funct & 3) | (word >> 6 & 31) << 2;
    match op {
        0x00..=0x03 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) + f(tb)),
        0x04..=0x07 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) - f(tb)),
        0x08..=0x0B => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) + rr(f(s[k]) * f(tb))),
        0x0C..=0x0F => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) - rr(f(s[k]) * f(tb))),
        0x10..=0x13 => {
            // ITOF0/4/12/15
            let sc = [1.0, 16.0, 4096.0, 32768.0][(op & 3) as usize];
            for k in 0..4 {
                if d[k] && ft != 0 {
                    v.vf[ft][k] = r(s[k] as i32 as f64 / sc);
                }
            }
        }
        0x14..=0x17 => {
            // FTOI0/4/12/15: truncate, saturate
            let sc = [1.0, 16.0, 4096.0, 32768.0][(op & 3) as usize];
            for k in 0..4 {
                if d[k] && ft != 0 {
                    v.vf[ft][k] = (f(s[k]) * sc).clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32;
                }
            }
        }
        0x18..=0x1B => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) * f(tb)),
        0x1C => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) * f(q)),
        0x1D => {
            // ABS
            for k in 0..4 {
                if d[k] && ft != 0 {
                    v.vf[ft][k] = s[k] & 0x7FFF_FFFF;
                }
            }
        }
        0x1E => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) * f(ii)),
        0x1F => {
            // CLIPw.xyz: compare fs.xyz with |ft.w|
            let wv = f(t[3]).abs();
            let mut flags = 0u32;
            for k in 0..3 {
                let x = f(s[k]);
                if x > wv {
                    flags |= 1 << (2 * k);
                }
                if x < -wv {
                    flags |= 1 << (2 * k + 1);
                }
            }
            v.vi[VI_CLIP] = ((v.vi[VI_CLIP] << 6) | flags) & 0xFF_FFFF;
        }
        0x20 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) + f(q)),
        0x21 => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) + rr(f(s[k]) * f(q))),
        0x22 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) + f(ii)),
        0x23 => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) + rr(f(s[k]) * f(ii))),
        0x24 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) - f(q)),
        0x25 => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) - rr(f(s[k]) * f(q))),
        0x26 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) - f(ii)),
        0x27 => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) - rr(f(s[k]) * f(ii))),
        0x28 => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) + f(t[k])),
        0x29 => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) + rr(f(s[k]) * f(t[k]))),
        0x2A => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) * f(t[k])),
        0x2C => fmac_lanes::<FLAGS>(v, None, d, |k| f(s[k]) - f(t[k])),
        0x2D => fmac_lanes::<FLAGS>(v, None, d, |k| f(acc[k]) - rr(f(s[k]) * f(t[k]))),
        0x2E => {
            // OPMULA: ACC.xyz = fs.yzx * ft.zxy
            fmac_lanes::<FLAGS>(v, None, [true, true, true, false], |k| match k {
                0 => f(s[1]) * f(t[2]),
                1 => f(s[2]) * f(t[0]),
                _ => f(s[0]) * f(t[1]),
            });
        }
        _ => {} // NOP
    }
}

/// MAX / MINI compare EE singles as signed-magnitude integers (no flags).
#[inline(always)]
fn minmax(v: &mut VuRegs, fd: usize, d: [bool; 4], fs: usize, t: [u32; 4], max: bool) {
    if fd == 0 {
        return;
    }
    let key = |x: u32| -> i64 { if x & 0x8000_0000 != 0 { -((x & 0x7FFF_FFFF) as i64) - 1 } else { x as i64 } };
    for k in 0..4 {
        if d[k] {
            let a = v.vf[fs][k];
            let pick = if (key(a) >= key(t[k])) == max { a } else { t[k] };
            v.vf[fd][k] = pick;
        }
    }
}

/// The FDIV unit: DIV / SQRT / RSQRT into Q (status D and I flags).
#[inline(always)]
pub fn fdiv(v: &mut VuRegs, op: u32, fs: usize, fsf: usize, ft: usize, ftf: usize) -> u32 {
    let a = v.vf[fs][fsf];
    let b = v.vf[ft][ftf];
    let mut st = v.vi[VI_STATUS] & !0x30;
    let res = match op {
        0 => {
            if b & 0x7FFF_FFFF == 0 {
                if a & 0x7FFF_FFFF == 0 { st |= 0x10 | 0x400; } else { st |= 0x20 | 0x800; }
                ((a ^ b) & 0x8000_0000) | 0x7FFF_FFFF
            } else {
                r(f(a) / f(b))
            }
        }
        1 => {
            if b & 0x8000_0000 != 0 && b & 0x7FFF_FFFF != 0 { st |= 0x10 | 0x400; }
            r(f(b).abs().sqrt())
        }
        _ => {
            if b & 0x7FFF_FFFF == 0 {
                st |= 0x20 | 0x800;
                (a & 0x8000_0000) | 0x7FFF_FFFF
            } else {
                if b & 0x8000_0000 != 0 { st |= 0x10 | 0x400; }
                r(f(a) / f(b).abs().sqrt())
            }
        }
    };
    v.vi[VI_STATUS] = st;
    res
}

/// R register LFSR step (VRNEXT).
pub fn rnext(v: &mut VuRegs) {
    let x = v.vi[VI_R] & 0x7F_FFFF;
    let bit = ((x >> 4) ^ (x >> 22)) & 1;
    v.vi[VI_R] = 0x3F80_0000 | ((x << 1 | bit) & 0x7F_FFFF);
}

/// VU0 data memory as the EE sees it (micro / macro lower instructions address it in qwords).
const VU0_DATA: u32 = 0x1100_4000;

#[inline(always)]
fn vi16(v: &VuRegs, n: usize) -> u32 { if n == 0 { 0 } else { v.vi[n] & 0xFFFF } }

/// Execute the COP2 instruction `word` (macro mode, or a COP2 register move / branch helper).
#[inline(always)]
pub fn exec(c: &mut Ctx, word: u32) {
    let op = word >> 26;
    if op != 0x12 {
        c.intrinsic("cop2", word);
        return;
    }
    if word & (1 << 25) == 0 {
        moves(c, word);
        return;
    }
    let funct = word & 0x3F;
    if funct < 0x30 || (funct >= 0x3C && (word >> 6 & 31) << 2 | (word & 3) < 0x30) {
        upper(regs(c), word);
        return;
    }
    lower_macro(c, word);
}

fn moves(c: &mut Ctx, word: u32) {
    let rt = (word >> 16 & 31) as usize;
    let rd = (word >> 11 & 31) as usize;
    match word >> 21 & 31 {
        1 => {
            // QMFC2
            let v = regs(c).vf[rd];
            let q = v.iter().enumerate().fold(0u128, |a, (k, x)| a | (*x as u128) << (32 * k));
            c.set_gpr128(rt, q);
        }
        2 => {
            // CFC2
            let v = cfc2(c, rd);
            c.set_gpr32(rt, v);
        }
        5 => {
            // QMTC2
            if rd != 0 {
                let q = c.gpr128(rt);
                regs(c).vf[rd] = std::array::from_fn(|k| (q >> (32 * k)) as u32);
            }
        }
        6 => {
            // CTC2
            let v = c.gpr32(rt);
            ctc2(c, rd, v);
        }
        _ => c.intrinsic("cop2 move", word),
    }
}

/// cfc2: integer and control registers.
pub fn cfc2(c: &mut Ctx, n: usize) -> u32 {
    match n {
        0 => 0,
        1..=15 => regs(c).vi[n] & 0xFFFF,
        29 => {
            // VPU-STAT: VU0 is never busy (micro programs run to completion at once); VU1 busy
            // comes from the runtime.
            let busy = (c.host.vu0_busy)(c);
            regs(c).vi[n] & !1 | busy as u32
        }
        _ => regs(c).vi[n],
    }
}

/// ctc2: integer and control registers (FBRST, CMSAR1 reach the runtime).
pub fn ctc2(c: &mut Ctx, n: usize, v: u32) {
    match n {
        0 => {}
        1..=15 => regs(c).vi[n] = v & 0xFFFF,
        VI_STATUS => { let r = regs(c); r.vi[n] = (r.vi[n] & 0x3F) | (v & 0xFC0); }
        VI_MAC | 29 => {} // read only
        VI_R => regs(c).vi[n] = 0x3F80_0000 | (v & 0x7F_FFFF),
        28 | 31 => {
            // FBRST (reset / force break of VU0, VU1), CMSAR1 (start VU1 at v): the runtime.
            regs(c).vi[n] = v;
            (c.host.vu0_call)(c, 0x8000_0000 | (n as u32) << 16 | (v & 0xFFFF));
        }
        _ => regs(c).vi[n] = v,
    }
}

fn lower_macro(c: &mut Ctx, word: u32) {
    let d = dest(word);
    let ft = (word >> 16 & 31) as usize;
    let fs = (word >> 11 & 31) as usize;
    let id = (word >> 6 & 31) as usize & 15;
    let it = ft & 15;
    let is = fs & 15;
    let funct = word & 0x3F;
    match funct {
        0x30 => { let r = regs(c); let x = vi16(r, is).wrapping_add(vi16(r, it)); if id != 0 { r.vi[id] = x & 0xFFFF; } }
        0x31 => { let r = regs(c); let x = vi16(r, is).wrapping_sub(vi16(r, it)); if id != 0 { r.vi[id] = x & 0xFFFF; } }
        0x32 => {
            let imm = (((word >> 6 & 31) as i32) << 27 >> 27) as u32;
            let r = regs(c);
            let x = vi16(r, is).wrapping_add(imm);
            if it != 0 { r.vi[it] = x & 0xFFFF; }
        }
        0x34 => { let r = regs(c); let x = vi16(r, is) & vi16(r, it); if id != 0 { r.vi[id] = x; } }
        0x35 => { let r = regs(c); let x = vi16(r, is) | vi16(r, it); if id != 0 { r.vi[id] = x; } }
        0x38 => (c.host.vu0_call)(c, (word >> 6 & 0x7FFF) * 8),
        0x39 => { let a = regs(c).vi[crate::VI_CMSAR0] & 0xFFFF; (c.host.vu0_call)(c, a * 8) }
        0x3C..=0x3F => {
            let op = (funct & 3) | (word >> 6 & 31) << 2;
            let fsf = (word >> 21 & 3) as usize;
            let ftf = (word >> 23 & 3) as usize;
            match op {
                0x30 => {
                    // MOVE
                    if ft != 0 { let r = regs(c); for k in 0..4 { if d[k] { r.vf[ft][k] = r.vf[fs][k]; } } }
                }
                0x31 => {
                    // MR32
                    if ft != 0 { let r = regs(c); let s = r.vf[fs]; let rot = [s[1], s[2], s[3], s[0]]; for k in 0..4 { if d[k] { r.vf[ft][k] = rot[k]; } } }
                }
                0x34 | 0x36 => {
                    // LQI / LQD
                    let mut a = vi16(regs(c), is);
                    if op == 0x36 { a = a.wrapping_sub(1) & 0xFFFF; if is != 0 { regs(c).vi[is] = a; } }
                    let q = c.ld128(VU0_DATA + (a & 0xFF) * 16);
                    let r = regs(c);
                    if ft != 0 { for k in 0..4 { if d[k] { r.vf[ft][k] = (q >> (32 * k)) as u32; } } }
                    if op == 0x34 && is != 0 { r.vi[is] = (a + 1) & 0xFFFF; }
                }
                0x35 | 0x37 => {
                    // SQI / SQD (fs to [it])
                    let mut a = vi16(regs(c), it);
                    if op == 0x37 { a = a.wrapping_sub(1) & 0xFFFF; if it != 0 { regs(c).vi[it] = a; } }
                    let addr = VU0_DATA + (a & 0xFF) * 16;
                    let old = c.ld128(addr);
                    let s = regs(c).vf[fs];
                    let mut q = old;
                    for k in 0..4 { if d[k] { q = q & !(0xFFFF_FFFFu128 << (32 * k)) | (s[k] as u128) << (32 * k); } }
                    c.st128(addr, q);
                    if op == 0x35 && it != 0 { regs(c).vi[it] = (a + 1) & 0xFFFF; }
                }
                0x38..=0x3A => {
                    let r = regs(c);
                    let qv = fdiv(r, op - 0x38, fs, fsf, ft, ftf);
                    r.vi[VI_Q] = qv;
                }
                0x3B => {} // WAITQ
                0x3C => { let r = regs(c); let x = r.vf[fs][fsf] & 0xFFFF; if it != 0 { r.vi[it] = x; } } // MTIR
                0x3D => {
                    // MFIR
                    let r = regs(c);
                    let x = vi16(r, is) as u16 as i16 as i32 as u32;
                    if ft != 0 { for k in 0..4 { if d[k] { r.vf[ft][k] = x; } } }
                }
                0x3E | 0x3F => {
                    // ILWR / ISWR: one field of the qword at [is]
                    let a = vi16(regs(c), is);
                    let addr = VU0_DATA + (a & 0xFF) * 16;
                    let k = (0..4).find(|&k| d[k]).unwrap_or(0);
                    if op == 0x3E {
                        let q = c.ld128(addr);
                        if it != 0 { regs(c).vi[it] = (q >> (32 * k)) as u32 & 0xFFFF; }
                    } else {
                        let val = vi16(regs(c), it);
                        let mut q = c.ld128(addr);
                        for k in 0..4 { if d[k] { q = q & !(0xFFFF_FFFFu128 << (32 * k)) | (val as u128) << (32 * k); } }
                        c.st128(addr, q);
                    }
                }
                0x40 => { let r = regs(c); rnext(r); let x = r.vi[VI_R]; if ft != 0 { for k in 0..4 { if d[k] { r.vf[ft][k] = x; } } } }
                0x41 => { let r = regs(c); let x = r.vi[VI_R]; if ft != 0 { for k in 0..4 { if d[k] { r.vf[ft][k] = x; } } } }
                0x42 => { let r = regs(c); r.vi[VI_R] = 0x3F80_0000 | (r.vf[fs][fsf] & 0x7F_FFFF); }
                0x43 => { let r = regs(c); r.vi[VI_R] = 0x3F80_0000 | ((r.vi[VI_R] ^ r.vf[fs][fsf]) & 0x7F_FFFF); }
                _ => c.intrinsic("cop2 lower", word),
            }
        }
        _ => c.intrinsic("cop2 macro", word),
    }
    let _ = VI_P;
}

#[cfg(test)]
mod tests {
    #[test]
    fn ps2_max_matches_largest_single() {
        assert_eq!(super::PS2_MAX, crate::fpu::ps2_to_f64(0x7FFF_FFFF));
    }
}
