//! R5900 multimedia instructions (MMI, opcode 0x1C) whose p-code Ghidra leaves opaque
//! (CALLOTHER). The generator emits `mmi::exec(c, WORD)` with the constant instruction word, so
//! the decode below folds away after inlining.
//!
//! Lanes: a 128-bit GPR is bytes 0..15 little endian; word lane i is bits 32i..32i+31, halfword
//! lane i bits 16i.., byte lane i bits 8i... HI and LO are 128 bits each (HI0/HI1, LO0/LO1).

use crate::{Ctx, REG_HI, REG_LO, REG_SA};

#[inline(always)] fn w(v: u128, i: usize) -> u32 { (v >> (32 * i)) as u32 }
#[inline(always)] fn h(v: u128, i: usize) -> u16 { (v >> (16 * i)) as u16 }
#[inline(always)] fn bt(v: u128, i: usize) -> u8 { (v >> (8 * i)) as u8 }
#[inline(always)] fn d(v: u128, i: usize) -> u64 { (v >> (64 * i)) as u64 }
#[inline(always)] fn from_w(l: [u32; 4]) -> u128 { l.iter().enumerate().fold(0, |a, (i, x)| a | (*x as u128) << (32 * i)) }
#[inline(always)] fn from_h(l: [u16; 8]) -> u128 { l.iter().enumerate().fold(0, |a, (i, x)| a | (*x as u128) << (16 * i)) }
#[inline(always)] fn from_b(l: [u8; 16]) -> u128 { l.iter().enumerate().fold(0, |a, (i, x)| a | (*x as u128) << (8 * i)) }
#[inline(always)] fn from_d(lo: u64, hi: u64) -> u128 { lo as u128 | (hi as u128) << 64 }

#[inline(always)] fn mapw(a: u128, c: u128, f: impl Fn(u32, u32) -> u32) -> u128 { from_w(std::array::from_fn(|i| f(w(a, i), w(c, i)))) }
#[inline(always)] fn maph(a: u128, c: u128, f: impl Fn(u16, u16) -> u16) -> u128 { from_h(std::array::from_fn(|i| f(h(a, i), h(c, i)))) }
#[inline(always)] fn mapb(a: u128, c: u128, f: impl Fn(u8, u8) -> u8) -> u128 { from_b(std::array::from_fn(|i| f(bt(a, i), bt(c, i)))) }

#[inline(always)] fn sat_w(v: i64) -> u32 { v.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32 }
#[inline(always)] fn sat_h(v: i32) -> u16 { v.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16 }
#[inline(always)] fn sat_b(v: i32) -> u8 { v.clamp(i8::MIN as i32, i8::MAX as i32) as i8 as u8 }
#[inline(always)] fn sat_uw(v: i64) -> u32 { v.clamp(0, u32::MAX as i64) as u32 }
#[inline(always)] fn sat_uh(v: i32) -> u16 { v.clamp(0, u16::MAX as i32) as u16 }
#[inline(always)] fn sat_ub(v: i32) -> u8 { v.clamp(0, u8::MAX as i32) as u8 }

#[inline(always)] fn hi(c: &Ctx) -> u128 { c.r128(REG_HI) }
#[inline(always)] fn lo(c: &Ctx) -> u128 { c.r128(REG_LO) }
#[inline(always)] fn set_hi(c: &mut Ctx, v: u128) { c.wr128(REG_HI, v) }
#[inline(always)] fn set_lo(c: &mut Ctx, v: u128) { c.wr128(REG_LO, v) }

/// Execute the MMI instruction `word`.
#[inline(always)]
pub fn exec(c: &mut Ctx, word: u32) {
    let rs = ((word >> 21) & 31) as usize;
    let rt = ((word >> 16) & 31) as usize;
    let rd = ((word >> 11) & 31) as usize;
    let sa = (word >> 6) & 31;
    let a = c.gpr128(rs);
    let b = c.gpr128(rt);
    let r: Option<u128> = match word & 0x3F {
        0x04 => {
            // PLZCW: leading bits equal to the sign, minus one, per word of the low doubleword.
            let f = |x: u32| { let x = if (x as i32) < 0 { !x } else { x }; x.leading_zeros().saturating_sub(1) };
            Some(from_d(f(w(a, 0)) as u64 | (f(w(a, 1)) as u64) << 32, d(c.gpr128(rd), 1)))
        }
        0x08 => mmi0(sa, a, b, c),
        0x09 => mmi2(sa, a, b, rd, c),
        0x28 => mmi1(sa, a, b, c),
        0x29 => mmi3(sa, a, b, rd, c),
        0x30 => Some(pmfhl(sa, c)),
        0x31 => { pmthl(sa, a, c); None }
        0x34 => Some(maph(b, 0, |x, _| x << (sa & 15))),
        0x36 => Some(maph(b, 0, |x, _| x >> (sa & 15))),
        0x37 => Some(maph(b, 0, |x, _| ((x as i16) >> (sa & 15)) as u16)),
        0x3C => Some(mapw(b, 0, |x, _| x << sa)),
        0x3E => Some(mapw(b, 0, |x, _| x >> sa)),
        0x3F => Some(mapw(b, 0, |x, _| ((x as i32) >> sa) as u32)),
        _ => { c.intrinsic("mmi", word); None }
    };
    if let Some(v) = r {
        c.set_gpr128(rd, v);
    }
}

fn mmi0(op: u32, a: u128, b: u128, c: &mut Ctx) -> Option<u128> {
    Some(match op {
        0 => mapw(a, b, |x, y| x.wrapping_add(y)),
        1 => mapw(a, b, |x, y| x.wrapping_sub(y)),
        2 => mapw(a, b, |x, y| if (x as i32) > (y as i32) { !0 } else { 0 }),
        3 => mapw(a, b, |x, y| (x as i32).max(y as i32) as u32),
        4 => maph(a, b, |x, y| x.wrapping_add(y)),
        5 => maph(a, b, |x, y| x.wrapping_sub(y)),
        6 => maph(a, b, |x, y| if (x as i16) > (y as i16) { !0 } else { 0 }),
        7 => maph(a, b, |x, y| (x as i16).max(y as i16) as u16),
        8 => mapb(a, b, |x, y| x.wrapping_add(y)),
        9 => mapb(a, b, |x, y| x.wrapping_sub(y)),
        10 => mapb(a, b, |x, y| if (x as i8) > (y as i8) { !0 } else { 0 }),
        16 => mapw(a, b, |x, y| sat_w(x as i32 as i64 + y as i32 as i64)),
        17 => mapw(a, b, |x, y| sat_w(x as i32 as i64 - y as i32 as i64)),
        18 => from_w([w(b, 0), w(a, 0), w(b, 1), w(a, 1)]), // PEXTLW
        19 => from_w([w(b, 0), w(b, 2), w(a, 0), w(a, 2)]), // PPACW
        20 => maph(a, b, |x, y| sat_h(x as i16 as i32 + y as i16 as i32)),
        21 => maph(a, b, |x, y| sat_h(x as i16 as i32 - y as i16 as i32)),
        22 => from_h([h(b, 0), h(a, 0), h(b, 1), h(a, 1), h(b, 2), h(a, 2), h(b, 3), h(a, 3)]), // PEXTLH
        23 => from_h([h(b, 0), h(b, 2), h(b, 4), h(b, 6), h(a, 0), h(a, 2), h(a, 4), h(a, 6)]), // PPACH
        24 => mapb(a, b, |x, y| sat_b(x as i8 as i32 + y as i8 as i32)),
        25 => mapb(a, b, |x, y| sat_b(x as i8 as i32 - y as i8 as i32)),
        26 => from_b(std::array::from_fn(|i| if i % 2 == 0 { bt(b, i / 2) } else { bt(a, i / 2) })), // PEXTLB
        27 => from_b(std::array::from_fn(|i| if i < 8 { bt(b, 2 * i) } else { bt(a, 2 * (i - 8)) })), // PPACB
        30 => {
            // PEXT5: 1-5-5-5 halfwords to 8-8-8-8 words
            from_w(std::array::from_fn(|i| {
                let x = w(b, i);
                (x & 0x1F) << 3 | (x >> 5 & 0x1F) << 11 | (x >> 10 & 0x1F) << 19 | (x >> 15 & 1) << 31
            }))
        }
        31 => {
            // PPAC5: 8-8-8-8 words to 1-5-5-5 halfwords
            from_w(std::array::from_fn(|i| {
                let x = w(b, i);
                (x >> 3 & 0x1F) | (x >> 11 & 0x1F) << 5 | (x >> 19 & 0x1F) << 10 | (x >> 31) << 15
            }))
        }
        _ => { c.intrinsic("mmi0", op); return None; }
    })
}

fn mmi1(op: u32, a: u128, b: u128, c: &mut Ctx) -> Option<u128> {
    Some(match op {
        1 => mapw(b, 0, |x, _| if x == 0x8000_0000 { 0x7FFF_FFFF } else { (x as i32).unsigned_abs() }),
        2 => mapw(a, b, |x, y| if x == y { !0 } else { 0 }),
        3 => mapw(a, b, |x, y| (x as i32).min(y as i32) as u32),
        4 => from_h(std::array::from_fn(|i| if i % 2 == 0 { h(a, i).wrapping_sub(h(b, i)) } else { h(a, i).wrapping_add(h(b, i)) })), // PADSBH
        5 => maph(b, 0, |x, _| if x == 0x8000 { 0x7FFF } else { (x as i16).unsigned_abs() }),
        6 => maph(a, b, |x, y| if x == y { !0 } else { 0 }),
        7 => maph(a, b, |x, y| (x as i16).min(y as i16) as u16),
        10 => mapb(a, b, |x, y| if x == y { !0 } else { 0 }),
        16 => mapw(a, b, |x, y| sat_uw(x as i64 + y as i64)),
        17 => mapw(a, b, |x, y| sat_uw(x as i64 - y as i64)),
        18 => from_w([w(b, 2), w(a, 2), w(b, 3), w(a, 3)]), // PEXTUW
        20 => maph(a, b, |x, y| sat_uh(x as i32 + y as i32)),
        21 => maph(a, b, |x, y| sat_uh(x as i32 - y as i32)),
        22 => from_h([h(b, 4), h(a, 4), h(b, 5), h(a, 5), h(b, 6), h(a, 6), h(b, 7), h(a, 7)]), // PEXTUH
        24 => mapb(a, b, |x, y| sat_ub(x as i32 + y as i32)),
        25 => mapb(a, b, |x, y| sat_ub(x as i32 - y as i32)),
        26 => from_b(std::array::from_fn(|i| if i % 2 == 0 { bt(b, 8 + i / 2) } else { bt(a, 8 + i / 2) })), // PEXTUB
        27 => {
            // QFSRV: funnel shift {rs, rt} right by SA bits.
            let sa = (c.r(REG_SA, 8) & 0x7F) as u32;
            if sa == 0 { b } else { (b >> sa) | (a << (128 - sa)) }
        }
        _ => { c.intrinsic("mmi1", op); return None; }
    })
}

fn mmi2(op: u32, a: u128, b: u128, rd: usize, c: &mut Ctx) -> Option<u128> {
    let _ = rd;
    Some(match op {
        0 | 4 => {
            // PMADDW / PMSUBW: words 0 and 2, 64-bit products accumulated in HI/LO
            let (hv, lv) = (hi(c), lo(c));
            let mut out = [0u64; 2];
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let acc = (d(hv, k) as u32 as u64) << 32 | d(lv, k) as u32 as u64;
                let p = w(a, 2 * k) as i32 as i64 * w(b, 2 * k) as i32 as i64;
                let r = if op == 0 { (acc as i64).wrapping_add(p) } else { (acc as i64).wrapping_sub(p) } as u64;
                out[k] = r;
                nl |= ((r as u32 as i32 as i64 as u64) as u128) << (64 * k);
                nh |= (((r >> 32) as u32 as i32 as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            from_d(out[0], out[1])
        }
        2 => from_d(((w(b, 0) << (w(a, 0) & 31)) as i32 as i64) as u64, ((w(b, 2) << (w(a, 2) & 31)) as i32 as i64) as u64), // PSLLVW
        3 => from_d(((w(b, 0) >> (w(a, 0) & 31)) as i32 as i64) as u64, ((w(b, 2) >> (w(a, 2) & 31)) as i32 as i64) as u64), // PSRLVW
        8 => hi(c),
        9 => lo(c),
        10 => from_h([h(b, 0), h(a, 0), h(b, 1), h(a, 1), h(b, 2), h(a, 2), h(b, 3), h(a, 3)]), // PINTH
        12 => {
            // PMULTW
            let mut out = [0u64; 2];
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let r = (w(a, 2 * k) as i32 as i64 * w(b, 2 * k) as i32 as i64) as u64;
                out[k] = r;
                nl |= ((r as u32 as i32 as i64 as u64) as u128) << (64 * k);
                nh |= (((r >> 32) as u32 as i32 as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            from_d(out[0], out[1])
        }
        13 => {
            // PDIVW
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let (x, y) = (w(a, 2 * k) as i32, w(b, 2 * k) as i32);
                let (q, r) = if y == 0 { (if x >= 0 { -1 } else { 1 }, x) } else if x == i32::MIN && y == -1 { (i32::MIN, 0) } else { (x / y, x % y) };
                nl |= ((q as i64 as u64) as u128) << (64 * k);
                nh |= ((r as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            return None;
        }
        14 => from_d(d(b, 0), d(a, 0)), // PCPYLD
        16 | 20 | 17 | 21 => {
            // PMADDH / PMSUBH / PHMADH / PHMSBH
            let (hv, lv) = (hi(c), lo(c));
            let p: [i32; 8] = std::array::from_fn(|i| h(a, i) as i16 as i32 * h(b, i) as i16 as i32);
            let (mut l, mut hh) = ([0u32; 4], [0u32; 4]);
            let res;
            if op == 16 || op == 20 {
                let sub = op == 20;
                let f = |acc: u32, x: i32| if sub { (acc as i32).wrapping_sub(x) as u32 } else { (acc as i32).wrapping_add(x) as u32 };
                l[0] = f(w(lv, 0), p[0]); l[1] = f(w(lv, 1), p[1]); hh[0] = f(w(hv, 0), p[2]); hh[1] = f(w(hv, 1), p[3]);
                l[2] = f(w(lv, 2), p[4]); l[3] = f(w(lv, 3), p[5]); hh[2] = f(w(hv, 2), p[6]); hh[3] = f(w(hv, 3), p[7]);
                res = from_w([l[0], hh[0], l[2], hh[2]]);
            } else {
                let g = |x: i32, y: i32| if op == 17 { x.wrapping_add(y) } else { x.wrapping_sub(y) } as u32;
                // phmadh: rd/lo/hi words from pairs (p1 + p0), (p3 + p2), ...; phmsbh: p1 - p0
                let r: [u32; 4] = std::array::from_fn(|k| g(p[2 * k + 1], p[2 * k]));
                l = [r[0], 0, r[2], 0];
                hh = [r[1], 0, r[3], 0];
                res = from_w(r);
            }
            set_lo(c, from_w(l));
            set_hi(c, from_w(hh));
            res
        }
        18 => a & b,
        19 => a ^ b,
        26 => from_h([h(b, 2), h(b, 1), h(b, 0), h(b, 3), h(b, 6), h(b, 5), h(b, 4), h(b, 7)]), // PEXEH
        27 => from_h([h(b, 3), h(b, 2), h(b, 1), h(b, 0), h(b, 7), h(b, 6), h(b, 5), h(b, 4)]), // PREVH
        28 => {
            // PMULTH
            let p: [u32; 8] = std::array::from_fn(|i| (h(a, i) as i16 as i32 * h(b, i) as i16 as i32) as u32);
            set_lo(c, from_w([p[0], p[1], p[4], p[5]]));
            set_hi(c, from_w([p[2], p[3], p[6], p[7]]));
            from_w([p[0], p[2], p[4], p[6]])
        }
        29 => {
            // PDIVBW: each word of rs by halfword 0 of rt
            let y = h(b, 0) as i16 as i32;
            let mut q = [0u32; 4];
            let mut r = [0u32; 4];
            for i in 0..4 {
                let x = w(a, i) as i32;
                if y == 0 {
                    q[i] = if x >= 0 { 0xFFFF_FFFF } else { 1 };
                    r[i] = x as u32;
                } else if x == i32::MIN && y == -1 {
                    q[i] = x as u32;
                } else {
                    q[i] = (x / y) as u32;
                    r[i] = (x % y) as i16 as i32 as u32;
                }
            }
            set_lo(c, from_w(q));
            set_hi(c, from_w(r));
            return None;
        }
        30 => from_w([w(b, 2), w(b, 1), w(b, 0), w(b, 3)]), // PEXEW
        31 => from_w([w(b, 1), w(b, 2), w(b, 0), w(b, 3)]), // PROT3W
        _ => { c.intrinsic("mmi2", op); return None; }
    })
}

fn mmi3(op: u32, a: u128, b: u128, rd: usize, c: &mut Ctx) -> Option<u128> {
    let _ = rd;
    Some(match op {
        0 => {
            // PMADDUW
            let (hv, lv) = (hi(c), lo(c));
            let mut out = [0u64; 2];
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let acc = (d(hv, k) as u32 as u64) << 32 | d(lv, k) as u32 as u64;
                let r = acc.wrapping_add(w(a, 2 * k) as u64 * w(b, 2 * k) as u64);
                out[k] = r;
                nl |= ((r as u32 as i32 as i64 as u64) as u128) << (64 * k);
                nh |= (((r >> 32) as u32 as i32 as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            from_d(out[0], out[1])
        }
        3 => from_d((((w(b, 0) as i32) >> (w(a, 0) & 31)) as i64) as u64, (((w(b, 2) as i32) >> (w(a, 2) & 31)) as i64) as u64), // PSRAVW
        8 => { set_hi(c, a); return None; }
        9 => { set_lo(c, a); return None; }
        10 => from_h([h(b, 0), h(a, 0), h(b, 2), h(a, 2), h(b, 4), h(a, 4), h(b, 6), h(a, 6)]), // PINTEH
        12 => {
            // PMULTUW
            let mut out = [0u64; 2];
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let r = w(a, 2 * k) as u64 * w(b, 2 * k) as u64;
                out[k] = r;
                nl |= ((r as u32 as i32 as i64 as u64) as u128) << (64 * k);
                nh |= (((r >> 32) as u32 as i32 as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            from_d(out[0], out[1])
        }
        13 => {
            // PDIVUW
            let (mut nh, mut nl) = (0u128, 0u128);
            for k in 0..2 {
                let (x, y) = (w(a, 2 * k), w(b, 2 * k));
                let (q, r) = if y == 0 { (u32::MAX, x) } else { (x / y, x % y) };
                nl |= ((q as i32 as i64 as u64) as u128) << (64 * k);
                nh |= ((r as i32 as i64 as u64) as u128) << (64 * k);
            }
            set_hi(c, nh);
            set_lo(c, nl);
            return None;
        }
        14 => from_d(d(a, 1), d(b, 1)), // PCPYUD
        18 => a | b,
        19 => !(a | b),
        26 => from_h([h(b, 0), h(b, 2), h(b, 1), h(b, 3), h(b, 4), h(b, 6), h(b, 5), h(b, 7)]), // PEXCH
        27 => from_h([h(b, 0), h(b, 0), h(b, 0), h(b, 0), h(b, 4), h(b, 4), h(b, 4), h(b, 4)]), // PCPYH
        30 => from_w([w(b, 0), w(b, 2), w(b, 1), w(b, 3)]), // PEXCW
        _ => { c.intrinsic("mmi3", op); return None; }
    })
}

fn pmfhl(fmt: u32, c: &mut Ctx) -> u128 {
    let (hv, lv) = (hi(c), lo(c));
    match fmt {
        0 => from_w([w(lv, 0), w(hv, 0), w(lv, 2), w(hv, 2)]), // LW
        1 => from_w([w(lv, 1), w(hv, 1), w(lv, 3), w(hv, 3)]), // UW
        2 => {
            // SLW: saturate the 64-bit {hi, lo} words to 32 bits, sign-extended
            let f = |k: usize| {
                let v = ((w(hv, 2 * k) as u64) << 32 | w(lv, 2 * k) as u64) as i64;
                sat_w(v) as i32 as i64 as u64
            };
            from_d(f(0), f(1))
        }
        3 => from_h([h(lv, 0), h(lv, 2), h(hv, 0), h(hv, 2), h(lv, 4), h(lv, 6), h(hv, 4), h(hv, 6)]), // LH
        4 => {
            // SH: saturate words to halfwords
            let s = |x: u32| sat_h(x as i32);
            from_h([s(w(lv, 0)), s(w(lv, 1)), s(w(hv, 0)), s(w(hv, 1)), s(w(lv, 2)), s(w(lv, 3)), s(w(hv, 2)), s(w(hv, 3))])
        }
        _ => { c.intrinsic("pmfhl", fmt); 0 }
    }
}

fn pmthl(fmt: u32, a: u128, c: &mut Ctx) {
    if fmt == 0 {
        let (hv, lv) = (hi(c), lo(c));
        set_lo(c, from_w([w(a, 0), w(lv, 1), w(a, 2), w(lv, 3)]));
        set_hi(c, from_w([w(a, 1), w(hv, 1), w(a, 3), w(hv, 3)]));
    } else {
        c.intrinsic("pmthl", fmt);
    }
}

