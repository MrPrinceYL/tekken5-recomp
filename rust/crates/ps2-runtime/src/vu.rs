//! VU0 / VU1 micro mode: an interpreter for micro programs (loaded by VIF MPG or written to
//! micro memory by the EE). The upper half of each instruction pair uses the shared FMAC code in
//! `ee_core::vu0::upper`; the lower half is decoded here.
//!
//! Timing model: PCSX2's VU interpreter (VU1microInterp.cpp, VUops.cpp), ported. Each pair takes
//! one cycle plus stalls: an instruction reading a VF field an FMAC operation still computes
//! (4 cycles), an FDIV / EFU operation while the unit is busy, a branch reading a VI register
//! ILW / ILWR still loads. The MAC / status / clip flags an operation produces become visible to
//! the flag instructions when its FMAC pipeline entry completes; Q and P when FDIV / EFU finish.
//! Both halves read the registers as they were before the pair (when both write the same VF
//! register, or both the clip flag, the upper result wins). A conditional branch right after an
//! integer operation that wrote one of its registers reads the old value (VI backup). A program
//! runs to completion (E bit) when started; the pipelines are flushed at its end.

use crate::{diag, Machine};
use ee_core::vu0::{self, upper_ex};
pub use ee_core::vu0::VuRegs;
use ee_core::{Ctx, VI_CLIP, VI_I, VI_MAC, VI_P, VI_Q, VI_R, VI_STATUS, VI_TPC};

/// Executes one instruction pair at `run.ctl.pc`; true when the program has ended.
pub type PairFn = fn(&mut Run) -> bool;

/// A compiled pair: with MAC / Status flags, and without (micro code that never reads them,
/// PCSX2 microVU's flag elimination).
pub type PairFns = (u64, PairFn, PairFn);

/// Pairs compiled ahead of time (crate t5-vu, generated from sheets/vu_microcode.csv by
/// tools/gen_vu.py), sorted by `upper << 32 | lower`.
static PAIR_TABLE: std::sync::OnceLock<&'static [PairFns]> = std::sync::OnceLock::new();

/// Install the compiled pair table (at start-up).
pub fn set_pair_table(t: &'static [PairFns]) {
    let _ = PAIR_TABLE.set(t);
}

/// Translated blocks (crate t5-vu, from sheets/vu_blocks.csv): (key, block). A block runs the
/// straight-line pairs from its start to its branch delay slot / E-bit slot.
static BLOCK_TABLE: std::sync::OnceLock<&'static [(u64, PairFn)]> = std::sync::OnceLock::new();

/// Install the translated block table (at start-up).
pub fn set_block_tables(parts: &[&'static [(u64, PairFn)]]) {
    let mut v: Vec<(u64, PairFn)> = parts.iter().flat_map(|p| p.iter().copied()).collect();
    v.sort_unstable_by_key(|e| e.0);
    let _ = BLOCK_TABLE.set(Box::leak(v.into_boxed_slice()));
}

/// The straight-line block starting at `pc`: its pair count (through the branch delay slot or
/// the slot after an E bit), or None if it runs off micro memory or exceeds 256 pairs.
pub fn block_len(code: &[u8], pc: usize) -> Option<usize> {
    let n = code.len() / 8;
    let mut j = pc;
    while j < n && j - pc < 256 {
        let o = j * 8;
        let lo = u32::from_le_bytes(code[o..o + 4].try_into().unwrap());
        let up = u32::from_le_bytes(code[o + 4..o + 8].try_into().unwrap());
        let ibit = up & 0x8000_0000 != 0;
        if up & 0x4000_0000 != 0 || (!ibit && matches!(lo >> 25, 0x20 | 0x21 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2C..=0x2F)) {
            return (j + 1 < n).then_some(j + 2 - pc);
        }
        j += 1;
    }
    None
}

/// A block's key: FNV-1a over the VU number and its instruction words, not its address: the game
/// loads the same micro code at different places (Devil Within packs its overlays behind the
/// resident program), the translated block takes its pc at run time (tools/gen_vu.py computes
/// the same key).
pub fn block_key(code: &[u8], n: usize, pc: usize, len: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |w: u32| {
        for b in w.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    };
    mix(n as u32);
    for w in code[pc * 8..(pc + len) * 8].chunks_exact(4) {
        mix(u32::from_le_bytes(w.try_into().unwrap()));
    }
    h
}

fn resolve_block(code: &[u8], n: usize, pc: usize) -> Option<PairFn> {
    let t = BLOCK_TABLE.get()?;
    let len = block_len(code, pc)?;
    let key = block_key(code, n, pc, len);
    t.binary_search_by_key(&key, |e| e.0).ok().map(|i| t[i].1)
}

fn resolve(lower: u32, upper: u32, flags: bool) -> PairFn {
    let key = (upper as u64) << 32 | lower as u64;
    if let Some(t) = PAIR_TABLE.get() {
        if let Ok(i) = t.binary_search_by_key(&key, |e| e.0) {
            return if flags { t[i].1 } else { t[i].2 };
        }
    }
    diag::count("vu pair not compiled");
    if flags { pair_dyn } else { pair_dyn_nf }
}

/// Whether the pair reads what the pipelines deliver (the VI copies of MAC / Status / Clip, Q,
/// P) or starts a new FDIV / EFU operation: only then must completed entries be applied first.
/// Applying them later, in order, at the next such pair gives the same values there.
#[inline(always)]
fn needs_retire(lower: u32, upper: u32) -> bool {
    let funct = upper & 0x3F;
    let upper_q = if funct < 0x3C {
        matches!(funct, 0x1C | 0x20 | 0x21 | 0x24 | 0x25)
    } else {
        matches!((funct & 3) | ((upper >> 6) & 31) << 2, 0x1C | 0x20 | 0x21 | 0x24 | 0x25)
    };
    if upper_q {
        return true;
    }
    if upper & 0x8000_0000 != 0 {
        return false;
    }
    match lower >> 25 {
        0x10 | 0x12 | 0x13 | 0x14 | 0x16 | 0x17 | 0x18 | 0x1A | 0x1B | 0x1C => true,
        0x40 if lower & 0x3F >= 0x3C => {
            let op = (lower & 3) | ((lower >> 6) & 31) << 2;
            matches!(op, 0x38..=0x3A | 0x64 | 0x70..=0x7E)
        }
        _ => false,
    }
}

/// Which flags the loaded micro code reads: Status (FSEQ / FSAND / FSOR) and MAC (FMEQ /
/// FMAND / FMOR) readers anywhere in micro memory.
fn flag_readers(code: &[u8]) -> (bool, bool) {
    let mut st = false;
    let mut mac = false;
    for c in code.chunks_exact(8) {
        let lower = u32::from_le_bytes(c[0..4].try_into().unwrap());
        let upper = u32::from_le_bytes(c[4..8].try_into().unwrap());
        if upper & 0x8000_0000 == 0 {
            match lower >> 25 {
                0x14 | 0x16 | 0x17 => st = true,
                0x18 | 0x1A | 0x1B => mac = true,
                _ => {}
            }
        }
    }
    (st, mac)
}

/// Whether the upper instruction computes new MAC flags (the FMAC arithmetic; not MAX / MINI,
/// ITOF / FTOI, ABS, CLIP, NOP).
fn upper_sets_mac(w: u32) -> bool {
    let funct = w & 0x3F;
    if funct < 0x3C {
        return !matches!(funct, 0x10..=0x17 | 0x1D | 0x1F | 0x2B | 0x2F | 0x30..=0x3B);
    }
    let op = (funct & 3) | ((w >> 6) & 31) << 2;
    matches!(op, 0x00..=0x0F | 0x18..=0x1C | 0x1E | 0x20..=0x2A | 0x2C..=0x2E)
}

/// Whether the MAC flags the pair at `pc` computes can be read (PCSX2 microVU flag elimination,
/// per instruction). A MAC reader at pair j sees the newest FMAC entry completed by then; once a
/// later MAC-setting pair c has had 4 pairs (at least 4 cycles) to complete, nothing after sees
/// `pc`'s flags. So they are dead if no MAC reader lies in (pc, c + 3] within the straight-line
/// code; reaching a block end (branch delay slot, E bit) first counts as live.
fn mac_live(code: &[u8], pc: usize) -> bool {
    let n = code.len() / 8;
    let pair = |j: usize| {
        let o = j * 8;
        (u32::from_le_bytes(code[o..o + 4].try_into().unwrap()), u32::from_le_bytes(code[o + 4..o + 8].try_into().unwrap()))
    };
    if !upper_sets_mac(pair(pc).1) {
        return false;
    }
    let mut c: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut j = pc + 1;
    loop {
        if j >= n {
            return true;
        }
        let (lo, up) = pair(j);
        let ibit = up & 0x8000_0000 != 0;
        if !ibit && matches!(lo >> 25, 0x18 | 0x1A | 0x1B) && c.is_none_or(|c| j <= c + 3) {
            return true;
        }
        if let Some(c) = c {
            if j >= c + 3 {
                return false;
            }
        }
        if c.is_none() && upper_sets_mac(up) {
            c = Some(j);
        }
        if end == Some(j) {
            return true;
        }
        if end.is_none() && (up & 0x4000_0000 != 0 || (!ibit && matches!(lo >> 25, 0x20 | 0x21 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2C..=0x2F))) {
            end = Some(j + 1);
        }
        j += 1;
    }
}

pub struct Vu {
    pub n: usize,
    /// Bumped by every write to micro memory (MPG, EE stores): invalidates `dec`.
    pub code_gen: u64,
    dec_gen: u64,
    dec: Vec<Option<PairFn>>,
    /// The loaded micro code's flag readers (Status, MAC), see `flag_readers`.
    readers: (bool, bool),
    pub code: Box<[u8]>,
    pub data: Box<[u8]>,
    /// VU1's registers (VU0 uses the EE context's COP2 registers).
    pub regs: Box<VuRegs>,
    pub tpc: u32,
    pub runs: u64,
}

impl Vu {
    pub fn new(n: usize) -> Vu {
        let size = if n == 0 { 0x1000 } else { 0x4000 };
        Vu { n, code_gen: 1, dec_gen: 0, dec: Vec::new(), readers: (true, true), code: vec![0u8; size].into_boxed_slice(), data: vec![0u8; size].into_boxed_slice(), regs: VuRegs::new(), tpc: 0, runs: 0 }
    }
}

/// COP2 macro-mode requests: vcallms / vcallmsr (an address in bytes), and ctc2 writes to FBRST
/// / CMSAR1 (0x80000000 | reg << 16 | value).
pub fn vu0_call(c: &mut Ctx, arg: u32) {
    let m = crate::machine();
    if arg & 0x8000_0000 != 0 {
        let reg = (arg >> 16) & 0x1F;
        let v = arg & 0xFFFF;
        match reg {
            31 => match m.mtvu.as_mut() {
                Some(t) => t.send(crate::mtvu::Cmd::VuStart(v)),
                None => run(m, 1, v),
            },
            28 => {
                if v & 0x200 != 0 {
                    match m.mtvu.as_mut() {
                        Some(t) => t.send(crate::mtvu::Cmd::VuReset),
                        None => m.vu1.regs = VuRegs::new(),
                    }
                }
            }
            _ => {}
        }
        return;
    }
    let _ = c;
    run(m, 0, arg / 8);
}

#[derive(Clone, Copy)]
enum Pending {
    Vf(usize, [bool; 4], [u32; 4]),
    Vi(usize, u32),
    None,
}

/// Register-use summary of one instruction (PCSX2's _VURegsNum).
#[derive(Clone, Copy, Default)]
struct Info {
    pipe: u8,
    /// VF register written by the FMAC pipeline and its fields (x = 8 .. w = 1).
    vfw: usize,
    vfw_m: u32,
    /// VF registers read (0 = none) and their fields.
    r0: usize,
    r0_m: u32,
    r1: usize,
    r1_m: u32,
    /// Integer / control registers written (bits 0..15 = vi0..vi15, plus the W_* bits).
    viw: u32,
    /// Integer registers read (bits 0..15).
    vir: u32,
    /// FDIV / EFU / IALU latency.
    cycles: u64,
}

const P_NONE: u8 = 0;
const P_FMAC: u8 = 1;
const P_FDIV: u8 = 2;
const P_EFU: u8 = 3;
const P_IALU: u8 = 4;
const P_BRANCH: u8 = 5;

const W_STATUS: u32 = 1 << 16;
const W_CLIP: u32 = 1 << 18;
const W_Q: u32 = 1 << 22;
const W_P: u32 = 1 << 23;

/// A small FIFO without allocation (the pipelines hold at most four entries).
#[derive(Clone, Copy, Default)]
struct Ring<T: Copy + Default> {
    a: [T; 8],
    head: usize,
    len: usize,
}

impl<T: Copy + Default> Ring<T> {
    #[inline(always)]
    fn push_back(&mut self, v: T) {
        if self.len == 8 {
            self.pop_front();
        }
        self.a[(self.head + self.len) & 7] = v;
        self.len += 1;
    }
    #[inline(always)]
    fn front(&self) -> Option<&T> {
        (self.len > 0).then(|| &self.a[self.head])
    }
    #[inline(always)]
    fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.a[self.head];
        self.head = (self.head + 1) & 7;
        self.len -= 1;
        Some(v)
    }
    #[inline(always)]
    fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len).map(move |i| &self.a[(self.head + i) & 7])
    }
    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Dynamic pipeline values at the entry of a statically translated block (`Pipe::s_in`).
pub struct SIn {
    pub cycle: u64,
    pub mac: u32,
    pub status: u32,
    pub clip: u32,
    /// FMAC entries in order: (mac, status, clip).
    pub fm: [(u32, u32, u32); 8],
    /// FDIV result (Q, status), EFU result (P), VI backup value.
    pub fdiv: (u32, u32),
    pub efu: u32,
    pub vib: u32,
}

/// An FMAC pipeline entry: the registers it writes and the flags it carries.
#[derive(Clone, Copy, Default)]
struct Fmac {
    start: u64,
    flagreg: u32,
    mac: u32,
    status: u32,
    clip: u32,
}

pub struct Pipe {
    cycle: u64,
    fmac: Ring<Fmac>,
    /// FDIV in flight: (start, cycles, Q, status).
    fdiv: Option<(u64, u64, u32, u32)>,
    /// EFU in flight: (start, cycles, P).
    efu: Option<(u64, u64, u32)>,
    /// IALU loads in flight: (start, cycles, written VI mask).
    ialu: Ring<(u64, u64, u32)>,
    /// Flags as the FMAC units compute them (the VI copies change when entries complete).
    mac: u32,
    status: u32,
    clip: u32,
    /// FSSET / FCSET results, applied after the upper half (an upper CLIP wins over FCSET).
    post_status: Option<u32>,
    post_clip: Option<u32>,
    /// VI backup: (cycles left, register, value before the write).
    vib: (u64, usize, u32),
    /// Per VF register and field (x, y, z, w): the cycle its last FMAC write completes.
    ready: [[u64; 4]; 32],
    /// The last FMAC write events (completion cycle, register, fields): everything still in
    /// flight is among them (at most two writes per pair, done within four cycles). The entry
    /// shape of a statically translated block is read from these instead of `ready`.
    wq: Ring<(u64, u8, u8)>,
    /// The entry shape key of the next block, when a statically translated block left the
    /// pipeline in canonical form and knew it (`key_ok`); anything else clears `key_ok`.
    pub key: u64,
    pub key_ok: bool,
}

impl Pipe {
    /// PCSX2 _vuFMACTestStall: wait for the in-flight FMAC results the read fields need. `ready`
    /// holds, per register and field, the cycle its last FMAC write completes, so this is the
    /// same as scanning the pipeline for unfinished writers of those fields.
    #[inline(always)]
    fn fmac_stall(&mut self, reg: usize, mask: u32) {
        if reg == 0 || mask == 0 {
            return;
        }
        let rd = &self.ready[reg];
        for k in 0..4 {
            if mask & (8 >> k) != 0 && rd[k] > self.cycle {
                self.cycle = rd[k];
            }
        }
    }

    /// Record an FMAC write of `reg`'s `mask` fields started this cycle.
    #[inline(always)]
    fn fmac_write(&mut self, reg: usize, mask: u32) {
        if reg == 0 {
            return;
        }
        let done = self.cycle + 4;
        for k in 0..4 {
            if mask & (8 >> k) != 0 {
                self.ready[reg][k] = done;
            }
        }
        if mask != 0 {
            self.wq.push_back((done, reg as u8, mask as u8));
        }
    }

    /// Stall for the instruction's inputs (_vuTestUpperStalls / _vuTestLowerStalls).
    #[inline(always)]
    fn stalls(&mut self, i: &Info) {
        match i.pipe {
            P_FMAC => {
                self.fmac_stall(i.r0, i.r0_m);
                self.fmac_stall(i.r1, i.r1_m);
            }
            P_FDIV => {
                self.fmac_stall(i.r0, i.r0_m);
                self.fmac_stall(i.r1, i.r1_m);
                if let Some((st, c, _, _)) = self.fdiv {
                    self.cycle = self.cycle.max(st + c);
                }
            }
            P_EFU => {
                self.fmac_stall(i.r0, i.r0_m);
                self.fmac_stall(i.r1, i.r1_m);
                if let Some(e) = self.efu.as_mut() {
                    // The stall releases a cycle before P is written (PCSX2 _vuTestEFUStalls).
                    e.1 = e.1.saturating_sub(1);
                    self.cycle = self.cycle.max(e.0 + e.1);
                }
            }
            P_BRANCH => {
                for &(st, c, reg) in self.ialu.iter() {
                    if self.cycle - st < c && reg & i.vir != 0 {
                        self.cycle = self.cycle.max(st + c);
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_fmac(r: &mut VuRegs, e: &Fmac) {
        if e.flagreg & W_CLIP != 0 {
            r.vi[VI_CLIP] = e.clip;
        }
        r.vi[VI_STATUS] = if e.flagreg & W_STATUS != 0 {
            (r.vi[VI_STATUS] & 0x30) | (e.status & 0xFC0) | (e.status & 0xF)
        } else {
            (r.vi[VI_STATUS] & 0xFF0) | (e.status & 0xF) | ((e.status & 0xF) << 6)
        };
        r.vi[VI_MAC] = e.mac;
    }

    /// The timing shape relative to the current cycle (what a statically translated block must
    /// know at entry): in-flight FMAC writes (reg, field, cycles left), FMAC entries (age, flag
    /// registers), FDIV / EFU (age, latency), active IALU loads, VI backup (count, register).
    pub fn shape(&self) -> String {
        // Same order and content as `shape_key` (tools/vuc.py parses this).
        let mut out = Vec::new();
        for &(t, reg, mask) in self.wq.iter() {
            if t > self.cycle {
                out.push(format!("w{}:{reg}:{mask}", t - self.cycle));
            }
        }
        for e in self.fmac.iter() {
            out.push(format!("f{}:{:x}", (self.cycle - e.start).min(4), e.flagreg));
        }
        if let Some((st, c, _, _)) = self.fdiv {
            out.push(format!("q{}/{c}", (self.cycle - st).min(c)));
        }
        if let Some((st, c, _)) = self.efu {
            out.push(format!("p{}/{c}", (self.cycle - st).min(c)));
        }
        for &(st, c, reg) in self.ialu.iter() {
            if self.cycle - st < c {
                out.push(format!("i{}/{c}:{reg:x}", self.cycle - st));
            }
        }
        if self.vib.0 > 0 {
            out.push(format!("b{}:{}", self.vib.0, self.vib.1));
        }
        out.join(" ")
    }

    /// Apply the FMAC entries that have completed (as `retire` would at the next pair that needs
    /// them; nothing reads the flags in between, and FDIV / EFU results, applied after the FMAC
    /// entries there, are left for `retire`). Canonical form of the entry state of a block.
    #[inline(always)]
    pub fn canon(&mut self, r: &mut VuRegs) {
        while let Some(e) = self.fmac.front().copied() {
            if self.cycle - e.start < 4 {
                break;
            }
            Self::apply_fmac(r, &e);
            self.fmac.pop_front();
        }
    }

    /// The timing state relative to the current cycle as a key (FNV-1a over a byte encoding
    /// that tools/vuc.py reproduces): in-flight FMAC writes (cycles left, register, fields),
    /// FMAC entries (age, flag registers), FDIV / EFU (age capped at the latency, latency),
    /// active IALU loads (age, latency, registers), VI backup (count, register).
    #[inline(always)]
    pub fn shape_key(&self) -> u64 {
        // One word per element, mixed (tools/vuc.py `shape_key` computes the same).
        let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut m = |x: u64| h = (h ^ x).wrapping_mul(0xFF51_AFD7_ED55_8CCD).rotate_left(23);
        for &(t, reg, mask) in self.wq.iter() {
            if t > self.cycle {
                m(1 << 60 | (t - self.cycle) << 24 | (reg as u64) << 8 | mask as u64);
            }
        }
        for e in self.fmac.iter() {
            m(2 << 60 | (self.cycle - e.start).min(4) << 32 | e.flagreg as u64);
        }
        if let Some((st, c, _, _)) = self.fdiv {
            m(3 << 60 | (self.cycle - st).min(c) << 8 | c);
        }
        if let Some((st, c, _)) = self.efu {
            m(4 << 60 | (self.cycle - st).min(c) << 8 | c);
        }
        for &(st, c, reg) in self.ialu.iter() {
            if self.cycle - st < c {
                m(5 << 60 | (self.cycle - st) << 40 | c << 32 | reg as u64);
            }
        }
        if self.vib.0 > 0 {
            m(6 << 60 | self.vib.0 << 8 | self.vib.1 as u64);
        }
        h
    }

    /// Dynamic values a statically translated block starts from.
    #[inline(always)]
    pub fn s_in(&self) -> SIn {
        let mut fm = [(0u32, 0u32, 0u32); 8];
        for (i, e) in self.fmac.iter().enumerate() {
            fm[i] = (e.mac, e.status, e.clip);
        }
        SIn {
            cycle: self.cycle,
            mac: self.mac,
            status: self.status,
            clip: self.clip,
            fm,
            fdiv: self.fdiv.map(|x| (x.2, x.3)).unwrap_or((0, 0)),
            efu: self.efu.map(|x| x.2).unwrap_or(0),
            vib: self.vib.2,
        }
    }

    /// The state a statically translated block leaves (times relative to `base`, the cycle at
    /// its entry), equivalent to what the pair-by-pair kernel would leave.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub fn s_out(
        &mut self,
        base: u64,
        cycle: i64,
        flags: (u32, u32, u32),
        writes: &[(i64, u8, u8)],
        fm: &[(i64, u32, u32, u32, u32)],
        fdiv: Option<(i64, u64, u32, u32)>,
        efu: Option<(i64, u64, u32)>,
        ialu: &[(i64, u64, u32)],
        vib: (u64, usize, u32),
        key: u64,
    ) {
        self.key = key;
        self.key_ok = true;
        let abs = |t: i64| (base as i64 + t) as u64;
        self.cycle = abs(cycle);
        (self.mac, self.status, self.clip) = flags;
        self.wq.clear();
        for &(t, reg, mask) in writes {
            for k in 0..4 {
                if mask & (8 >> k) != 0 {
                    self.ready[reg as usize][k] = abs(t);
                }
            }
            self.wq.push_back((abs(t), reg, mask));
        }
        self.fmac.clear();
        for &(st, flagreg, mac, status, clip) in fm {
            self.fmac.push_back(Fmac { start: abs(st), flagreg, mac, status, clip });
        }
        self.fdiv = fdiv.map(|(st, c, q, s)| (abs(st), c, q, s));
        self.efu = efu.map(|(st, c, v)| (abs(st), c, v));
        self.ialu.clear();
        for &(st, c, reg) in ialu {
            self.ialu.push_back((abs(st), c, reg));
        }
        self.vib = vib;
    }

    /// Retire what has completed (_vuTestPipes).
    #[inline(always)]
    fn retire(&mut self, r: &mut VuRegs) {
        if self.fmac.len == 0 && self.fdiv.is_none() && self.efu.is_none() && self.ialu.len == 0 {
            return;
        }
        while let Some(e) = self.fmac.front().copied() {
            if self.cycle - e.start < 4 {
                break;
            }
            Self::apply_fmac(r, &e);
            self.fmac.pop_front();
        }
        if let Some((st, c, q, status)) = self.fdiv {
            if self.cycle - st >= c {
                r.vi[VI_Q] = q;
                r.vi[VI_STATUS] = (r.vi[VI_STATUS] & 0xFCF) | (status & 0xC30);
                self.fdiv = None;
            }
        }
        if let Some((st, c, v)) = self.efu {
            if self.cycle - st >= c {
                r.vi[VI_P] = v;
                self.efu = None;
            }
        }
        while let Some(&(st, c, _)) = self.ialu.front() {
            if self.cycle - st < c {
                break;
            }
            self.ialu.pop_front();
        }
    }

    /// Complete everything (end of program, _vuFlushAll).
    fn flush_all(&mut self, r: &mut VuRegs) {
        if let Some((_, _, q, status)) = self.fdiv.take() {
            r.vi[VI_Q] = q;
            r.vi[VI_STATUS] = (r.vi[VI_STATUS] & 0xFCF) | (status & 0xC30);
        }
        if let Some((_, _, v)) = self.efu.take() {
            r.vi[VI_P] = v;
        }
        while let Some(e) = self.fmac.pop_front() {
            Self::apply_fmac(r, &e);
        }
        self.ialu.clear();
        self.vib = (0, 0, 0);
    }

    /// A branch operand: the value before the previous instruction's write (VI backup).
    fn branch_vi(&self, r: &VuRegs, reg: usize) -> u32 {
        if self.vib.0 > 0 && self.vib.1 == reg && reg != 0 {
            self.vib.2 & 0xFFFF
        } else {
            vi16(r, reg)
        }
    }
}

/// Field mask (x = 8 .. w = 1) of the dest bits.
#[inline]
fn xyzw(w: u32) -> u32 {
    (w >> 21) & 0xF
}

/// PCSX2's _vuRegs tables for the upper instructions.
#[inline(always)]
fn upper_info(w: u32) -> Info {
    let fd = ((w >> 6) & 31) as usize;
    let fs = ((w >> 11) & 31) as usize;
    let ft = ((w >> 16) & 31) as usize;
    let m = xyzw(w);
    let bcm = 8 >> (w & 3);
    let fmac = |vfw: usize, r0: usize, r0_m: u32, r1: usize, r1_m: u32, viw: u32| Info { pipe: P_FMAC, vfw, vfw_m: m, r0, r0_m, r1, r1_m, viw, ..Info::default() };
    let funct = w & 0x3F;
    if funct < 0x3C {
        return match funct {
            0x00..=0x1B => fmac(fd, fs, m, ft, bcm, 0),
            0x1C..=0x27 => fmac(fd, fs, m, 0, 0, 0),
            0x28..=0x2D | 0x2F => fmac(fd, fs, m, ft, m, 0),
            0x2E => Info { vfw_m: 0xE, ..fmac(fd, fs, 0xE, ft, 0xE, 0) },
            _ => Info::default(),
        };
    }
    let op = (funct & 3) | ((w >> 6) & 31) << 2;
    match op {
        0x00..=0x0F | 0x18..=0x1B => fmac(0, fs, m, ft, bcm, 0),
        0x10..=0x17 | 0x1D => fmac(ft, fs, m, 0, 0, 0),
        0x1C | 0x1E | 0x20..=0x27 => fmac(0, fs, m, 0, 0, 0),
        0x1F => fmac(0, fs, 0xE, ft, 0x1, W_CLIP),
        0x28..=0x2A | 0x2C | 0x2D => fmac(0, fs, m, ft, m, 0),
        0x2E => fmac(0, fs, 0xE, ft, 0xE, 0),
        _ => Info::default(),
    }
}

/// PCSX2's _vuRegs tables for the lower instructions.
#[inline(always)]
fn lower_info(w: u32) -> Info {
    let ft = ((w >> 16) & 31) as usize;
    let fs = ((w >> 11) & 31) as usize;
    let it = 1u32 << (ft & 15);
    let is = 1u32 << (fs & 15);
    let id = 1u32 << (((w >> 6) & 31) & 15);
    let m = xyzw(w);
    let fsm = 1 << (3 - ((w >> 21) & 3));
    let ftm = 1 << (3 - ((w >> 23) & 3));
    let i = |pipe: u8| Info { pipe, ..Info::default() };
    match w >> 25 {
        0x00 => Info { vfw: ft, vfw_m: m, vir: is, ..i(P_FMAC) },
        0x01 => Info { r0: fs, r0_m: m, vir: it, ..i(P_FMAC) },
        0x04 => Info { viw: it, vir: is, cycles: 4, ..i(P_IALU) },
        0x05 => Info { vir: is | it, ..i(P_IALU) },
        0x08 | 0x09 => Info { viw: it, vir: is, ..i(P_IALU) },
        0x10 | 0x12 | 0x13 => Info { viw: 1 << 1, ..i(P_FMAC) },
        0x11 => Info { viw: W_CLIP, ..i(P_FMAC) },
        0x15 => Info { viw: W_STATUS, ..i(P_FMAC) },
        0x14 | 0x16 | 0x17 | 0x1C => Info { viw: it, ..i(P_FMAC) },
        0x18 | 0x1A | 0x1B => Info { viw: it, vir: is, ..i(P_FMAC) },
        0x20 => i(P_BRANCH),
        0x21 => Info { viw: it, ..i(P_BRANCH) },
        0x24 => Info { vir: is, ..i(P_BRANCH) },
        0x25 => Info { viw: it, vir: is, ..i(P_BRANCH) },
        0x28 | 0x29 => Info { vir: is | it, ..i(P_BRANCH) },
        0x2C..=0x2F => Info { vir: is, ..i(P_BRANCH) },
        0x40 => {
            let funct = w & 0x3F;
            match funct {
                0x30 | 0x31 | 0x34 | 0x35 => return Info { viw: id, vir: is | it, ..i(P_IALU) },
                0x32 => return Info { viw: it, vir: is, ..i(P_IALU) },
                0x3C..=0x3F => {}
                _ => return Info::default(),
            }
            let op = (funct & 3) | ((w >> 6) & 31) << 2;
            match op {
                0x30 => Info { vfw: ft, vfw_m: m, r0: fs, r0_m: m, ..i(if ft == 0 { P_NONE } else { P_FMAC }) },
                0x31 => Info { vfw: ft, vfw_m: m, r0: fs, r0_m: (m >> 1) | ((m << 3) & 8), ..i(P_FMAC) },
                0x34 | 0x36 => Info { vfw: ft, vfw_m: m, viw: is, vir: is, ..i(P_FMAC) },
                0x35 | 0x37 => Info { r0: fs, r0_m: m, viw: it, vir: it, ..i(P_FMAC) },
                0x38 => Info { r0: fs, r0_m: fsm, r1: ft, r1_m: ftm, viw: W_Q, cycles: 7, ..i(P_FDIV) },
                0x39 => Info { r1: ft, r1_m: ftm, viw: W_Q, cycles: 7, ..i(P_FDIV) },
                0x3A => Info { r0: fs, r0_m: fsm, r1: ft, r1_m: ftm, viw: W_Q, cycles: 13, ..i(P_FDIV) },
                0x3B => i(P_FDIV),
                0x3C => Info { r0: fs, r0_m: fsm, viw: it, ..i(P_FMAC) },
                0x3D | 0x40 | 0x41 | 0x64 => Info { vfw: ft, vfw_m: m, vir: is, ..i(P_FMAC) },
                0x3E => Info { viw: it, vir: is, cycles: 4, ..i(P_IALU) },
                0x3F => Info { vir: is | it, ..i(P_IALU) },
                0x42 | 0x43 => Info { r0: fs, r0_m: fsm, ..i(P_FMAC) },
                0x68 | 0x69 => Info { viw: it, ..i(P_IALU) },
                0x70 => Info { r0: fs, r0_m: m, viw: W_P, cycles: 11, ..i(P_EFU) },
                0x71 | 0x72 => Info { r0: fs, r0_m: m, viw: W_P, cycles: 18, ..i(P_EFU) },
                0x73 => Info { r0: fs, r0_m: m, viw: W_P, cycles: 24, ..i(P_EFU) },
                0x74 | 0x75 => Info { r0: fs, r0_m: m, viw: W_P, cycles: 54, ..i(P_EFU) },
                0x76 => Info { r0: fs, r0_m: m, viw: W_P, cycles: 12, ..i(P_EFU) },
                0x78 | 0x7A => Info { r0: fs, r0_m: fsm, viw: W_P, cycles: 12, ..i(P_EFU) },
                0x79 => Info { r0: fs, r0_m: fsm, viw: W_P, cycles: 18, ..i(P_EFU) },
                0x7B => i(P_EFU),
                0x7C => Info { r0: fs, r0_m: fsm, viw: W_P, cycles: 29, ..i(P_EFU) },
                0x7D => Info { r0: fs, r0_m: fsm, viw: W_P, cycles: 54, ..i(P_EFU) },
                0x7E => Info { r0: fs, r0_m: fsm, viw: W_P, cycles: 44, ..i(P_EFU) },
                _ => Info::default(),
            }
        }
        _ => Info::default(),
    }
}

/// The VI register whose old value a following branch sees (PCSX2 _vuBackupVI callers).
#[inline(always)]
fn backup_reg(w: u32) -> Option<usize> {
    let it = ((w >> 16) & 15) as usize;
    let is = ((w >> 11) & 15) as usize;
    let id = ((w >> 6) & 15) as usize;
    match w >> 25 {
        0x08 | 0x09 => (it != 0).then_some(it),
        0x40 => match w & 0x3F {
            0x30 | 0x31 | 0x34 | 0x35 => (id != 0).then_some(id),
            0x32 => (it != 0).then_some(it),
            f if f >= 0x3C => match (f & 3) | ((w >> 6) & 31) << 2 {
                0x3C => Some(it),
                0x34 | 0x36 => Some(is),
                0x35 | 0x37 => Some(it),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Run VU `n` from instruction index `pc` until its E bit.
pub fn run(m: &mut Machine, n: usize, pc: u32) {
    if crate::diag::enabled("T5_VUTRACE") {
        diag::count_with(|| format!("vu{n}_start_{pc:#x}"));
    }
    if n == 1 && crate::diag::enabled("T5_TBPWATCH") {
        crate::gif::VU1_START.store(pc, std::sync::atomic::Ordering::Relaxed);
    }
    let cap = n == 1 && crate::diag::enabled("T5_VUCAPTURE") && capture_start(m, pc);
    // T5_VUSHAPES=1: record block entry shapes (written to vu_dumps/shapes.txt by the summary).
    if n == 1 && crate::diag::enabled("T5_VUSHAPES") {
        SHAPES_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let t0 = std::time::Instant::now();
    run_inner(m, n, pc);
    if cap {
        capture_end(m);
    }
    crate::diag::TIME[4].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// The control flow state of a run: next pair, pending branch target, E-bit delay slot.
pub struct Ctl {
    pub pc: u32,
    pub branch_to: Option<u32>,
    pub end_after: bool,
}

/// One micro program run: the VU's state while its pairs execute.
pub struct Run {
    pub ctl: Ctl,
    mp: *mut Machine,
    n: usize,
    regs: *mut VuRegs,
    vu: *mut Vu,
    p: Pipe,
    icount: u32,
    qmask: u32,
    tracing: bool,
    /// Pairs executed.
    steps: u64,
}

fn run_inner(m: &mut Machine, n: usize, pc: u32) {
    let mp: *mut Machine = m;
    let regs: *mut VuRegs = if n == 0 { vu0::regs(&mut m.ctx) } else { &mut *m.vu1.regs };
    let vup: *mut Vu = if n == 0 { &mut m.vu0 } else { &mut m.vu1 };
    let vu = unsafe { &mut *vup };
    vu.runs += 1;
    let icount = (vu.code.len() / 8) as u32;
    let qmask = (vu.data.len() / 16 - 1) as u32;
    let pc = pc % icount;
    if vu.dec_gen != vu.code_gen || vu.dec.len() != icount as usize {
        vu.dec.clear();
        vu.dec.resize(icount as usize, None);
        vu.dec_gen = vu.code_gen;
        // VU0: the EE can read its flags (CFC2) after a micro program; keep them exact.
        vu.readers = if n == 0 { (true, true) } else { flag_readers(&vu.code) };
        if crate::diag::enabled("T5_VUDUMP") {
            // Every distinct micro memory image, for the VU recompiler (tools/gen_vu.py).
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in vu.code.iter() {
                h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
            }
            let path = format!("vu_dumps/vu{n}_{h:016x}.bin");
            if !std::path::Path::new(&path).exists() {
                let _ = std::fs::create_dir_all("vu_dumps");
                let _ = std::fs::write(&path, &vu.code[..]);
                diag::log(format!("VU{n}: new micro image {h:016x}"));
            }
        }
    }
    let r0 = unsafe { &*regs };
    let p = Pipe {
        cycle: 0,
        fmac: Default::default(),
        fdiv: None,
        efu: None,
        ialu: Default::default(),
        mac: r0.vi[VI_MAC],
        status: r0.vi[VI_STATUS],
        clip: r0.vi[VI_CLIP],
        post_status: None,
        post_clip: None,
        vib: (0, 0, 0),
        ready: [[0; 4]; 32],
        wq: Default::default(),
        key: 0,
        key_ok: false,
    };
    let tracing = n == 1 && trace_this(pc);
    if tracing {
        let _ = std::fs::write("vu1_code.bin", &vu.code[..]);
        let _ = std::fs::write("vu1_data.bin", &vu.data[..]);
        let v = &m.vif[1];
        diag::log(format!("VUT start pc {pc:#x} top {:#x} itop {:#x} tops {:#x} base {:#x} ofst {:#x} dbf {}", v.top, v.itop, v.tops, v.base, v.ofst, v.dbf));
    }
    let mut run = Run { ctl: Ctl { pc, branch_to: None, end_after: false }, mp, n, regs, vu: vup, p, icount, qmask, tracing, steps: 0 };
    let csr = if n == 1 { Some(mxcsr_vu()) } else { None };
    loop {
        if run.steps > 20_000_000 {
            diag::log(format!("VU{n}: program at {:#x} did not end", run.ctl.pc));
            break;
        }
        let pc = run.ctl.pc as usize;
        let f = match vu.dec[pc] {
            Some(f) => f,
            None => {
                let o = pc * 8;
                let lower = u32::from_le_bytes(vu.code[o..o + 4].try_into().unwrap());
                let upper = u32::from_le_bytes(vu.code[o + 4..o + 8].try_into().unwrap());
                // Flags are computed where they can be read: all of them if the micro code reads
                // Status (sticky bits accumulate), else per pair (`mac_live`).
                let flags = vu.readers.0 || (vu.readers.1 && mac_live(&vu.code, pc));
                // A translated block (it carries per-pair flag liveness, which assumes no Status
                // readers), else the single pair.
                let block = if vu.readers.0 { None } else { resolve_block(&vu.code, n, pc) };
                if block.is_none() && crate::diag::enabled("T5_VUDUMP") {
                    log_block_start(&vu.code, n, pc);
                }
                let f = block.unwrap_or_else(|| resolve(lower, upper, flags));
                vu.dec[pc] = Some(f);
                f
            }
        };
        if f(&mut run) {
            break;
        }
    }
    if let Some(old) = csr {
        mxcsr_restore(old);
    }
    let regs = unsafe { &mut *regs };
    run.p.flush_all(regs);
    VU_STEPS[n].fetch_add(run.steps, std::sync::atomic::Ordering::Relaxed);
    VU_CYCLES[n].fetch_add(run.p.cycle, std::sync::atomic::Ordering::Relaxed);
    vu.tpc = run.ctl.pc;
    regs.vi[VI_TPC] = run.ctl.pc;
}

/// Start of a translated block of `n` pairs at the current pc (where `run_inner` resolved it):
/// false when the block cannot run straight (a branch or E bit is still pending from before):
/// the caller then runs one pair (`blk_one`).
#[inline(always)]
pub fn blk_enter(r: &mut Run, n: u64) -> bool {
    let pc = r.ctl.pc;
    if r.ctl.branch_to.is_some() || r.ctl.end_after || r.tracing {
        return false;
    }
    if SHAPES_ON.load(std::sync::atomic::Ordering::Relaxed) {
        let regs = unsafe { &mut *r.regs };
        r.p.canon(regs);
        let vu = unsafe { &*r.vu };
        let key = block_len(&vu.code, pc as usize).map(|len| block_key(&vu.code, r.n, pc as usize, len)).unwrap_or(0);
        let sh = format!("{key:016x} {:016x} {}", r.p.shape_key(), r.p.shape());
        SHAPES.lock().unwrap().get_or_insert_with(Default::default).entry((pc, sh)).and_modify(|c: &mut u64| *c += 1).or_insert(1);
    }
    r.steps += n;
    true
}

/// The parts a block kernel works on, as separate references: the registers, the pipeline and
/// the control state do not alias, so the compiler keeps them in host registers across the
/// block. Data memory and the machine (VIF TOP / ITOP, XGKICK) by pointer.
#[inline(always)]
pub fn blk_parts(r: &mut Run) -> (&mut VuRegs, *mut u8, &mut Pipe, &mut Ctl, *mut Machine) {
    let regs = unsafe { &mut *r.regs };
    let data = unsafe { (*r.vu).data.as_mut_ptr() };
    (regs, data, &mut r.p, &mut r.ctl, r.mp)
}

/// One pair at the current pc, decoded (blocks entered in an unusual state).
#[inline(never)]
pub fn blk_one(r: &mut Run) -> bool {
    r.steps += 1;
    if r.tracing {
        let regs = unsafe { &*r.regs };
        let vi: Vec<String> = (1..16).map(|k| format!("{:x}", regs.vi[k])).collect();
        let vu = unsafe { &*r.vu };
        let o = r.ctl.pc as usize * 8;
        let lower = u32::from_le_bytes(vu.code[o..o + 4].try_into().unwrap());
        let upper = u32::from_le_bytes(vu.code[o + 4..o + 8].try_into().unwrap());
        diag::log(format!("VUT {:04x} L {lower:08x} U {upper:08x} vi {}", r.ctl.pc, vi.join(",")));
    }
    r.steps -= 1;
    pair_dyn(r)
}

/// A pair not compiled ahead of time: decoded each time it runs.
pub fn pair_dyn(r: &mut Run) -> bool {
    let vu = unsafe { &*r.vu };
    let o = r.ctl.pc as usize * 8;
    let lower = u32::from_le_bytes(vu.code[o..o + 4].try_into().unwrap());
    let upper = u32::from_le_bytes(vu.code[o + 4..o + 8].try_into().unwrap());
    pair_body(r, lower, upper, true)
}

/// `pair_dyn` without MAC / Status flags.
pub fn pair_dyn_nf(r: &mut Run) -> bool {
    let vu = unsafe { &*r.vu };
    let o = r.ctl.pc as usize * 8;
    let lower = u32::from_le_bytes(vu.code[o..o + 4].try_into().unwrap());
    let upper = u32::from_le_bytes(vu.code[o + 4..o + 8].try_into().unwrap());
    pair_body(r, lower, upper, false)
}

/// A pair compiled ahead of time: the decoding folds away for the constant words.
#[inline(never)]
pub fn pair_const<const L: u32, const U: u32>(r: &mut Run) -> bool {
    pair_body(r, L, U, true)
}

/// `pair_const` without MAC / Status flags.
#[inline(never)]
pub fn pair_const_nf<const L: u32, const U: u32>(r: &mut Run) -> bool {
    pair_body(r, L, U, false)
}

/// One instruction pair (PCSX2 _vu1Exec): stalls, retire, lower and upper halves, pipeline
/// entries, branch / E-bit delay slots. True when the program has ended.
#[inline(always)]
fn pair_body(r: &mut Run, lower: u32, upw: u32, flags: bool) -> bool {
    r.p.key_ok = false;
    let regs = unsafe { &mut *r.regs };
    let vu = unsafe { &mut *r.vu };
    let n = r.n;
    let pc = r.ctl.pc;
    r.steps += 1;
    let p = &mut r.p;
    p.cycle += 1;
    let before = p.cycle - 1;
    let ibit = upw & 0x8000_0000 != 0;
    let ebit = upw & 0x4000_0000 != 0;
    if r.tracing {
        let vi: Vec<String> = (1..16).map(|k| format!("{:x}", regs.vi[k])).collect();
        diag::log(format!("VUT {pc:04x} L {lower:08x} U {upw:08x} vi {}", vi.join(",")));
    }
    let ui = upper_info(upw);
    p.stalls(&ui);
    let li = if ibit { Info::default() } else { lower_info(lower) };
    if !ibit {
        p.stalls(&li);
    }
    if needs_retire(lower, upw) {
        p.retire(regs);
    }
    if p.vib.0 > 0 {
        p.vib.0 -= (p.cycle - before).min(p.vib.0);
    }
    let mut pending = Pending::None;
    let mut taken: Option<u32> = None;
    let backup = if ibit { None } else { backup_reg(lower) }.map(|x| (x, regs.vi[x]));
    if !ibit && lower != 0x8000_033C {
        let res = lower_op(r.mp, n, regs, vu.data.as_mut_ptr(), p, pc, lower, r.qmask, r.icount);
        pending = res.0;
        taken = res.1;
    }
    // Upper half (reads the registers before this pair's lower writes), on the FMAC flags.
    if flags {
        let vis = (regs.vi[VI_MAC], regs.vi[VI_STATUS], regs.vi[VI_CLIP]);
        regs.vi[VI_MAC] = p.mac;
        regs.vi[VI_STATUS] = p.status;
        regs.vi[VI_CLIP] = p.clip;
        if !(n == 1 && upper_sse_f::<true>(regs, upw & 0x01FF_FFFF)) {
            upper_ex::<true>(regs, upw & 0x01FF_FFFF);
        }
        p.mac = regs.vi[VI_MAC];
        p.status = regs.vi[VI_STATUS];
        p.clip = regs.vi[VI_CLIP];
        regs.vi[VI_MAC] = vis.0;
        regs.vi[VI_STATUS] = vis.1;
        regs.vi[VI_CLIP] = vis.2;
    } else {
        let vis = regs.vi[VI_CLIP];
        regs.vi[VI_CLIP] = p.clip;
        // VU1 programs run with MXCSR set for `upper_sse` (see `run_inner`).
        if !(n == 1 && upper_sse(regs, upw & 0x01FF_FFFF)) {
            if crate::diag::enabled("T5_PRIMSTATS") && n == 1 {
                let w = upw & 0x01FF_FFFF;
                let f = w & 0x3F;
                let op = if f < 0x3C { f } else { 0x100 | (f & 3) | ((w >> 6) & 31) << 2 };
                diag::count_with(|| format!("sse_fallback_{op:x}"));
            }
            upper_ex::<false>(regs, upw & 0x01FF_FFFF);
        }
        p.clip = regs.vi[VI_CLIP];
        regs.vi[VI_CLIP] = vis;
    }
    if ibit {
        regs.vi[VI_I] = lower;
    }
    if let Some(st) = p.post_status.take() {
        if flags {
            p.status = (st & 0xFC0) | (p.status & 0x3F);
        }
    }
    if let Some(c) = p.post_clip.take() {
        if ui.viw & W_CLIP == 0 {
            p.clip = c;
        }
    }
    let upper_fd = (ui.pipe == P_FMAC && ui.vfw != 0).then_some(ui.vfw);
    match pending {
        Pending::Vf(rr, d, v) if rr != 0 => {
            if upper_fd != Some(rr) {
                for k in 0..4 {
                    if d[k] {
                        regs.vf[rr][k] = v[k];
                    }
                }
            }
        }
        Pending::Vi(rr, v) if rr != 0 && rr < 16 => regs.vi[rr] = v & 0xFFFF,
        _ => {}
    }
    regs.vf[0] = [0, 0, 0, 0x3F80_0000];
    regs.vi[0] = 0;
    // Pipeline entries this pair starts.
    if ui.pipe == P_FMAC || li.pipe == P_FMAC {
        let (reg_u, mask_u, fu) = if ui.pipe == P_FMAC { (ui.vfw, ui.vfw_m, ui.viw) } else { (0, 0, 0) };
        let (reg_l, mask_l, fl) = if li.pipe == P_FMAC { (li.vfw, li.vfw_m, li.viw) } else { (0, 0, 0) };
        // Entries of pairs whose MAC flags nobody reads carry nothing new (no Status readers
        // then): only a clip change needs one.
        if flags || (fu | fl) & W_CLIP != 0 {
            if p.fmac.len == 8 {
                // Retire lazily (see `needs_retire`): the oldest of eight is long complete.
                let e = p.fmac.pop_front().unwrap();
                Pipe::apply_fmac(regs, &e);
            }
            p.fmac.push_back(Fmac { start: p.cycle, flagreg: fu | fl, mac: p.mac, status: p.status, clip: p.clip });
        }
        p.fmac_write(reg_u, mask_u);
        p.fmac_write(reg_l, mask_l);
    }
    if li.pipe == P_IALU && li.cycles > 0 {
        p.ialu.push_back((p.cycle, li.cycles, li.viw & 0xFFFF));
    }
    if let Some((reg, old)) = backup {
        if p.vib.0 > 0 && p.vib.1 == reg {
            p.vib.0 = 2;
        } else {
            p.vib = (2, reg, old);
        }
    }
    // Control flow: branch delay slot, E-bit delay slot.
    let next = if pc + 1 == r.icount { 0 } else { pc + 1 };
    if r.ctl.end_after {
        r.ctl.pc = r.ctl.branch_to.take().unwrap_or(next);
        return true;
    }
    r.ctl.pc = r.ctl.branch_to.take().unwrap_or(next);
    if let Some(t) = taken {
        r.ctl.branch_to = Some(t % r.icount);
    }
    if ebit {
        r.ctl.end_after = true;
    }
    false
}

/// `pair_body` for translated blocks (crate t5-vu): the generator resolves per pair what
/// `pair_body` decides at run time. No tracing; steps are counted per block (`blk_enter`); `pc`
/// is the pair's address (block start + offset, at run time: the code is relocatable); only `CTRL` pairs (a branch / E bit and its delay slot) run the
/// control flow; only `VIB` pairs (near a VI-backup writer or a branch, and the first two of a
/// block) keep the VI backup.
#[inline(always)]
pub fn pair_lean<const L: u32, const U: u32, const FLAGS: bool, const CTRL: bool, const VIB: bool, const VU: usize>(
    pc: u32,
    regs: &mut VuRegs,
    data: *mut u8,
    p: &mut Pipe,
    c: &mut Ctl,
    mp: *mut Machine,
) -> bool {
    let (lower, upw, flags) = (L, U, FLAGS);
    let n = VU;
    // Micro memory sizes: VU0 4 KiB code / data, VU1 16 KiB.
    let icount: u32 = if VU == 1 { 0x800 } else { 0x200 };
    let qmask: u32 = if VU == 1 { 0x3FF } else { 0xFF };
    p.cycle += 1;
    let before = p.cycle - 1;
    let ibit = upw & 0x8000_0000 != 0;
    let ebit = upw & 0x4000_0000 != 0;
    let ui = upper_info(upw);
    p.stalls(&ui);
    let li = if ibit { Info::default() } else { lower_info(lower) };
    if !ibit {
        p.stalls(&li);
    }
    if needs_retire(lower, upw) {
        p.retire(regs);
    }
    if VIB && p.vib.0 > 0 {
        p.vib.0 -= (p.cycle - before).min(p.vib.0);
    }
    let mut pending = Pending::None;
    let mut taken: Option<u32> = None;
    let backup = if ibit || !VIB { None } else { backup_reg(lower) }.map(|x| (x, regs.vi[x]));
    if !ibit && lower != 0x8000_033C {
        let res = lower_op(mp, n, regs, data, p, pc, lower, qmask, icount);
        pending = res.0;
        taken = res.1;
    }
    // Upper half (reads the registers before this pair's lower writes), on the FMAC flags.
    if flags {
        let vis = (regs.vi[VI_MAC], regs.vi[VI_STATUS], regs.vi[VI_CLIP]);
        regs.vi[VI_MAC] = p.mac;
        regs.vi[VI_STATUS] = p.status;
        regs.vi[VI_CLIP] = p.clip;
        if !(n == 1 && upper_sse_f::<true>(regs, upw & 0x01FF_FFFF)) {
            upper_ex::<true>(regs, upw & 0x01FF_FFFF);
        }
        p.mac = regs.vi[VI_MAC];
        p.status = regs.vi[VI_STATUS];
        p.clip = regs.vi[VI_CLIP];
        regs.vi[VI_MAC] = vis.0;
        regs.vi[VI_STATUS] = vis.1;
        regs.vi[VI_CLIP] = vis.2;
    } else {
        let vis = regs.vi[VI_CLIP];
        regs.vi[VI_CLIP] = p.clip;
        // VU1 programs run with MXCSR set for `upper_sse` (see `run_inner`).
        if !(n == 1 && upper_sse(regs, upw & 0x01FF_FFFF)) {
            upper_ex::<false>(regs, upw & 0x01FF_FFFF);
        }
        p.clip = regs.vi[VI_CLIP];
        regs.vi[VI_CLIP] = vis;
    }
    if ibit {
        regs.vi[VI_I] = lower;
    }
    if let Some(st) = p.post_status.take() {
        if flags {
            p.status = (st & 0xFC0) | (p.status & 0x3F);
        }
    }
    if let Some(c) = p.post_clip.take() {
        if ui.viw & W_CLIP == 0 {
            p.clip = c;
        }
    }
    let upper_fd = (ui.pipe == P_FMAC && ui.vfw != 0).then_some(ui.vfw);
    match pending {
        Pending::Vf(rr, d, v) if rr != 0 => {
            if upper_fd != Some(rr) {
                for k in 0..4 {
                    if d[k] {
                        regs.vf[rr][k] = v[k];
                    }
                }
            }
        }
        Pending::Vi(rr, v) if rr != 0 && rr < 16 => regs.vi[rr] = v & 0xFFFF,
        _ => {}
    }
    regs.vf[0] = [0, 0, 0, 0x3F80_0000];
    regs.vi[0] = 0;
    // Pipeline entries this pair starts.
    if ui.pipe == P_FMAC || li.pipe == P_FMAC {
        let (reg_u, mask_u, fu) = if ui.pipe == P_FMAC { (ui.vfw, ui.vfw_m, ui.viw) } else { (0, 0, 0) };
        let (reg_l, mask_l, fl) = if li.pipe == P_FMAC { (li.vfw, li.vfw_m, li.viw) } else { (0, 0, 0) };
        // Entries of pairs whose MAC flags nobody reads carry nothing new (no Status readers
        // then): only a clip change needs one.
        if flags || (fu | fl) & W_CLIP != 0 {
            if p.fmac.len == 8 {
                // Retire lazily (see `needs_retire`): the oldest of eight is long complete.
                let e = p.fmac.pop_front().unwrap();
                Pipe::apply_fmac(regs, &e);
            }
            p.fmac.push_back(Fmac { start: p.cycle, flagreg: fu | fl, mac: p.mac, status: p.status, clip: p.clip });
        }
        p.fmac_write(reg_u, mask_u);
        p.fmac_write(reg_l, mask_l);
    }
    if li.pipe == P_IALU && li.cycles > 0 {
        p.ialu.push_back((p.cycle, li.cycles, li.viw & 0xFFFF));
    }
    if let Some((reg, old)) = backup {
        if p.vib.0 > 0 && p.vib.1 == reg {
            p.vib.0 = 2;
        } else {
            p.vib = (2, reg, old);
        }
    }
    if !CTRL {
        return false;
    }
    // Control flow: branch delay slot, E-bit delay slot.
    let next = if pc + 1 == icount { 0 } else { pc + 1 };
    if c.end_after {
        c.pc = c.branch_to.take().unwrap_or(next);
        return true;
    }
    c.pc = c.branch_to.take().unwrap_or(next);
    if let Some(t) = taken {
        c.branch_to = Some(t % icount);
    }
    if ebit {
        c.end_after = true;
    }
    false
}

#[inline]
fn vi16(r: &VuRegs, i: usize) -> u32 {
    if i == 0 { 0 } else { r.vi[i] & 0xFFFF }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn lower_op(mp: *mut Machine, n: usize, r: &mut VuRegs, data: *mut u8, p: &mut Pipe, pc: u32, w: u32, qmask: u32, icount: u32) -> (Pending, Option<u32>) {
    let d = vu0::dest(w);
    let ft = ((w >> 16) & 31) as usize;
    let fs = ((w >> 11) & 31) as usize;
    let it = ft & 15;
    let is = fs & 15;
    let id = ((w >> 6) & 31) as usize & 15;
    let imm11 = ((w & 0x7FF) as i32) << 21 >> 21;
    let imm15 = (w & 0x7FF) | ((w >> 21) & 0xF) << 11;
    let fsf = ((w >> 21) & 3) as usize;
    let ftf = ((w >> 23) & 3) as usize;
    let op = w >> 25;
    // Data memory is (qmask + 1) qwords: the masked offset is always inside it.
    let rdq = |a: u32| -> [u32; 4] {
        let o = ((a & qmask) as usize) * 16;
        unsafe { std::ptr::read_unaligned(data.add(o) as *const [u32; 4]) }
    };
    let wrq = |a: u32, d: [bool; 4], v: [u32; 4]| {
        let o = ((a & qmask) as usize) * 16;
        for k in 0..4 {
            if d[k] {
                unsafe { std::ptr::write_unaligned(data.add(o + 4 * k) as *mut u32, v[k]) };
            }
        }
    };
    let flags = (r.vi[VI_MAC], r.vi[VI_STATUS], r.vi[VI_CLIP]);
    let tgt = |off: i32| ((pc as i32 + 1 + off).rem_euclid(icount as i32)) as u32;
    match op {
        0x00 => {
            let a = (vi16(r, is) as i32 + imm11) as u32;
            (Pending::Vf(ft, d, rdq(a)), None)
        }
        0x01 => {
            let a = (vi16(r, it) as i32 + imm11) as u32;
            wrq(a, d, r.vf[fs]);
            (Pending::None, None)
        }
        0x04 => {
            let a = (vi16(r, is) as i32 + imm11) as u32;
            let q = rdq(a);
            let k = (0..4).find(|&k| d[k]).unwrap_or(0);
            (Pending::Vi(it, q[k] & 0xFFFF), None)
        }
        0x05 => {
            let a = (vi16(r, is) as i32 + imm11) as u32;
            let v = vi16(r, it);
            wrq(a, d, [v; 4]);
            (Pending::None, None)
        }
        0x08 => (Pending::Vi(it, vi16(r, is).wrapping_add(imm15)), None),
        0x09 => (Pending::Vi(it, vi16(r, is).wrapping_sub(imm15)), None),
        0x10 => (Pending::Vi(1, ((flags.2 & 0xFF_FFFF) == (w & 0xFF_FFFF)) as u32), None),
        0x11 => {
            p.post_clip = Some(w & 0xFF_FFFF);
            (Pending::None, None)
        }
        0x12 => (Pending::Vi(1, (flags.2 & w & 0xFF_FFFF != 0) as u32), None),
        0x13 => (Pending::Vi(1, ((flags.2 | w) & 0xFF_FFFF == 0xFF_FFFF) as u32), None),
        0x14 => {
            let imm12 = (w & 0x7FF) | ((w >> 21) & 1) << 11;
            (Pending::Vi(1, ((flags.1 & 0xFFF) == imm12) as u32), None)
        }
        0x15 => {
            let imm12 = (w & 0x7FF) | ((w >> 21) & 1) << 11;
            p.post_status = Some(imm12);
            (Pending::None, None)
        }
        0x16 => {
            let imm12 = (w & 0x7FF) | ((w >> 21) & 1) << 11;
            (Pending::Vi(it, flags.1 & imm12), None)
        }
        0x17 => {
            let imm12 = (w & 0x7FF) | ((w >> 21) & 1) << 11;
            (Pending::Vi(it, (flags.1 | imm12) & 0xFFF), None)
        }
        0x18 => (Pending::Vi(it, ((flags.0 & 0xFFFF) == vi16(r, is)) as u32), None),
        0x1A => (Pending::Vi(it, flags.0 & vi16(r, is)), None),
        0x1B => (Pending::Vi(it, (flags.0 | vi16(r, is)) & 0xFFFF), None),
        0x1C => (Pending::Vi(it, flags.2 & 0xFFF), None),
        0x20 => (Pending::None, Some(tgt(imm11))),
        0x21 => (Pending::Vi(it, (pc + 2) % icount), Some(tgt(imm11))),
        0x24 => (Pending::None, Some(vi16(r, is) % icount)),
        0x25 => (Pending::Vi(it, (pc + 2) % icount), Some(vi16(r, is) % icount)),
        0x28 => (Pending::None, (p.branch_vi(r, it) == p.branch_vi(r, is)).then(|| tgt(imm11))),
        0x29 => (Pending::None, (p.branch_vi(r, it) != p.branch_vi(r, is)).then(|| tgt(imm11))),
        0x2C => (Pending::None, ((p.branch_vi(r, is) as u16 as i16) < 0).then(|| tgt(imm11))),
        0x2D => (Pending::None, ((p.branch_vi(r, is) as u16 as i16) > 0).then(|| tgt(imm11))),
        0x2E => (Pending::None, ((p.branch_vi(r, is) as u16 as i16) <= 0).then(|| tgt(imm11))),
        0x2F => (Pending::None, ((p.branch_vi(r, is) as u16 as i16) >= 0).then(|| tgt(imm11))),
        0x40 => lower_special(mp, n, r, data, p, w, d, ft, fs, it, is, id, fsf, ftf, qmask),
        _ => {
            diag::once(format!("VU{n}: unknown lower op {w:#010x} at {pc:#x}"));
            (Pending::None, None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn lower_special(mp: *mut Machine, n: usize, r: &mut VuRegs, data: *mut u8, p: &mut Pipe, w: u32, d: [bool; 4], ft: usize, fs: usize, it: usize, is: usize, id: usize, fsf: usize, ftf: usize, qmask: u32) -> (Pending, Option<u32>) {
    let funct = w & 0x3F;
    match funct {
        0x30 => return (Pending::Vi(id, vi16(r, is).wrapping_add(vi16(r, it))), None),
        0x31 => return (Pending::Vi(id, vi16(r, is).wrapping_sub(vi16(r, it))), None),
        0x32 => {
            let imm5 = (((w >> 6) & 31) as i32) << 27 >> 27;
            return (Pending::Vi(it, (vi16(r, is) as i32 + imm5) as u32), None);
        }
        0x34 => return (Pending::Vi(id, vi16(r, is) & vi16(r, it)), None),
        0x35 => return (Pending::Vi(id, vi16(r, is) | vi16(r, it)), None),
        0x3C..=0x3F => {}
        _ => {
            diag::once(format!("VU{n}: unknown lower special {w:#010x}"));
            return (Pending::None, None);
        }
    }
    let op = (funct & 3) | ((w >> 6) & 31) << 2;
    // Data memory is (qmask + 1) qwords: the masked offset is always inside it.
    let rdq = |a: u32| -> [u32; 4] {
        let o = ((a & qmask) as usize) * 16;
        unsafe { std::ptr::read_unaligned(data.add(o) as *const [u32; 4]) }
    };
    let wrq = |a: u32, d: [bool; 4], v: [u32; 4]| {
        let o = ((a & qmask) as usize) * 16;
        for k in 0..4 {
            if d[k] {
                unsafe { std::ptr::write_unaligned(data.add(o + 4 * k) as *mut u32, v[k]) };
            }
        }
    };
    let f = ee_core::fpu::ps2_to_f64;
    let tof = ee_core::fpu::f64_to_ps2;
    let efu = |p: &mut Pipe, v: f64, lat: u64| {
        p.efu = Some((p.cycle, lat, tof(v)));
    };
    let s = r.vf[fs];
    match op {
        0x30 => (Pending::Vf(ft, d, s), None),
        0x31 => (Pending::Vf(ft, d, [s[1], s[2], s[3], s[0]]), None),
        0x34 => {
            let a = vi16(r, is);
            let v = rdq(a);
            if is != 0 {
                r.vi[is] = (a + 1) & 0xFFFF;
            }
            (Pending::Vf(ft, d, v), None)
        }
        0x35 => {
            let a = vi16(r, it);
            wrq(a, d, s);
            if it != 0 {
                r.vi[it] = (a + 1) & 0xFFFF;
            }
            (Pending::None, None)
        }
        0x36 => {
            let a = vi16(r, is).wrapping_sub(1) & 0xFFFF;
            if is != 0 {
                r.vi[is] = a;
            }
            (Pending::Vf(ft, d, rdq(a)), None)
        }
        0x37 => {
            let a = vi16(r, it).wrapping_sub(1) & 0xFFFF;
            if it != 0 {
                r.vi[it] = a;
            }
            wrq(a, d, s);
            (Pending::None, None)
        }
        0x38..=0x3A => {
            // The FDIV unit; Q (and the D / I flags) arrive after 7 (DIV, SQRT) or 13 (RSQRT)
            // cycles.
            let (saved_q, saved_st) = (r.vi[VI_Q], r.vi[VI_STATUS]);
            let q = vu0::fdiv(r, op - 0x38, fs, fsf, ft, ftf);
            let st = r.vi[VI_STATUS];
            r.vi[VI_Q] = saved_q;
            r.vi[VI_STATUS] = saved_st;
            p.fdiv = Some((p.cycle, if op == 0x3A { 13 } else { 7 }, q, st));
            (Pending::None, None)
        }
        // WAITQ: the stall happened before the pair.
        0x3B => (Pending::None, None),
        0x3C => (Pending::Vi(it, s[fsf] & 0xFFFF), None),
        0x3D => {
            let x = vi16(r, is) as u16 as i16 as i32 as u32;
            (Pending::Vf(ft, d, [x; 4]), None)
        }
        0x3E => {
            let q = rdq(vi16(r, is));
            let k = (0..4).find(|&k| d[k]).unwrap_or(0);
            (Pending::Vi(it, q[k] & 0xFFFF), None)
        }
        0x3F => {
            let v = vi16(r, it);
            wrq(vi16(r, is), d, [v; 4]);
            (Pending::None, None)
        }
        0x40 => {
            vu0::rnext(r);
            (Pending::Vf(ft, d, [r.vi[VI_R]; 4]), None)
        }
        0x41 => (Pending::Vf(ft, d, [r.vi[VI_R]; 4]), None),
        0x42 => {
            r.vi[VI_R] = 0x3F80_0000 | (s[fsf] & 0x7F_FFFF);
            (Pending::None, None)
        }
        0x43 => {
            r.vi[VI_R] = 0x3F80_0000 | ((r.vi[VI_R] ^ s[fsf]) & 0x7F_FFFF);
            (Pending::None, None)
        }
        0x64 => (Pending::Vf(ft, d, [r.vi[VI_P]; 4]), None),
        0x68 => (Pending::Vi(it, if n == 1 { unsafe { (*mp).vif[1].top } } else { 0 }), None),
        0x69 => (Pending::Vi(it, unsafe { (*mp).vif[n].itop }), None),
        0x6C => {
            if n == 1 {
                xgkick(mp, data, vi16(r, is));
            }
            (Pending::None, None)
        }
        0x70 => { efu(p, f(s[0]) * f(s[0]) + f(s[1]) * f(s[1]) + f(s[2]) * f(s[2]), 11); (Pending::None, None) }
        0x71 => { efu(p, 1.0 / (f(s[0]) * f(s[0]) + f(s[1]) * f(s[1]) + f(s[2]) * f(s[2])), 18); (Pending::None, None) }
        0x72 => { efu(p, (f(s[0]) * f(s[0]) + f(s[1]) * f(s[1]) + f(s[2]) * f(s[2])).sqrt(), 18); (Pending::None, None) }
        0x73 => { efu(p, 1.0 / (f(s[0]) * f(s[0]) + f(s[1]) * f(s[1]) + f(s[2]) * f(s[2])).sqrt(), 24); (Pending::None, None) }
        0x74 => { efu(p, f(s[1]).atan2(f(s[0])), 54); (Pending::None, None) }
        0x75 => { efu(p, f(s[2]).atan2(f(s[0])), 54); (Pending::None, None) }
        0x76 => { efu(p, f(s[0]) + f(s[1]) + f(s[2]) + f(s[3]), 12); (Pending::None, None) }
        0x78 => { efu(p, f(s[fsf]).abs().sqrt(), 12); (Pending::None, None) }
        0x79 => { efu(p, 1.0 / f(s[fsf]).abs().sqrt(), 18); (Pending::None, None) }
        0x7A => { efu(p, 1.0 / f(s[fsf]), 12); (Pending::None, None) }
        // WAITP: the stall happened before the pair.
        0x7B => (Pending::None, None),
        0x7C => { efu(p, f(s[fsf]).sin(), 29); (Pending::None, None) }
        0x7D => { efu(p, f(s[fsf]).atan(), 54); (Pending::None, None) }
        0x7E => { efu(p, (-f(s[fsf])).exp(), 44); (Pending::None, None) }
        _ => {
            diag::once(format!("VU{n}: unknown lower special2 {w:#010x}"));
            (Pending::None, None)
        }
    }
}

/// XGKICK (VU1): PATH1 from data memory at `addr`. PATH1 reads VU1 data memory and the GIF / GS
/// state; nothing the kernels hold by reference (registers, pipeline).
pub fn xgkick(mp: *mut Machine, data: *mut u8, addr: u32) {
    if KICK_HASH.load(std::sync::atomic::Ordering::Relaxed) {
        kick_hash(unsafe { std::slice::from_raw_parts(data, 0x4000) }, addr);
    }
    if !BENCH.load(std::sync::atomic::Ordering::Relaxed) {
        let t0 = std::time::Instant::now();
        crate::gif::path1(unsafe { &mut *mp }, addr);
        XGKICK_NS.fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        XGKICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Time in PATH1 (XGKICK -> GIF -> GS queue) and number of XGKICKs, for the period summary.
pub static XGKICK_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static XGKICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// T5_VUTRACE=<start pc hex>:<nth run>: trace that run of a VU1 program.
fn trace_this(pc: u32) -> bool {
    static SEL: std::sync::OnceLock<Option<(u32, u64)>> = std::sync::OnceLock::new();
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some((sp, nth)) = *SEL.get_or_init(|| {
        let v = std::env::var("T5_VUTRACE").ok()?;
        let (a, b) = v.split_once(':')?;
        Some((u32::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, b.parse().ok()?))
    }) else { return false };
    pc == sp && N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == nth
}

/// Instruction pairs executed / cycles (with stalls) per VU, for the period summary.
pub static VU_STEPS: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
pub static VU_CYCLES: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];


/// T5_VUDUMP: record a block start that has no translation (image hash, pc) for gen_vu.py.
fn log_block_start(code: &[u8], n: usize, pc: usize) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in code.iter() {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    let line = format!("vu{n}_{h:016x} {pc:x}
");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("vu_dumps/starts.txt") {
        let _ = f.write_all(line.as_bytes());
    }
}


// ---- SSE FMAC for VU1 pairs without live flags (PCSX2 microVU style) ----

/// The upper FMAC arithmetic with SSE single precision: the caller has set MXCSR to round toward
/// zero with denormals flushed (as the VU truncates and has no denormals), so a product / sum
/// equals the exact result truncated, like `vu0::upper`. Only for pairs whose MAC / Status flags
/// are dead. Returns false (nothing done) for other instructions and when an operand of a
/// written lane is 2^63 or larger (overflow or the VU's exponent 255 would need the exact path).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn upper_sse(v: &mut VuRegs, word: u32) -> bool {
    upper_sse_f::<false>(v, word)
}

/// `upper_sse`, and with `FLAGS` also the MAC / Status flags (microVU computes them from the SSE
/// result too): written lanes with a nonzero exponent are neither zero, underflowed nor (below
/// 2^63 operands) overflowed, so only their signs set MAC bits; a lane with exponent 0 (zero or
/// underflow) takes the exact path.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn upper_sse_f<const FLAGS: bool>(v: &mut VuRegs, word: u32) -> bool {
    use std::arch::x86_64::*;
    if upper_int(v, word) {
        return true;
    }
    let dmask = (word >> 21) & 0xF; // x = 8 .. w = 1
    // NOP; and without flags, any operation with an empty dest mask writes nothing (CLIP, which
    // sets the clip flag regardless, excepted).
    let special_op = (word & 3) | ((word >> 6) & 31) << 2;
    if word & 0x3F >= 0x3C && special_op == 0x2F {
        return true;
    }
    if dmask == 0 {
        return !FLAGS && !(word & 0x3F >= 0x3C && special_op == 0x1F);
    }
    let ft = ((word >> 16) & 31) as usize;
    let fs = ((word >> 11) & 31) as usize;
    let fd = ((word >> 6) & 31) as usize;
    let bc = (word & 3) as usize;
    let funct = word & 0x3F;
    // (kind, second operand, destination is ACC): kinds 0 add, 1 sub, 2 mul, 3 madd, 4 msub,
    // 5 opmula, 6 opmsub. Second operand: 0 bc, 1 q, 2 i, 3 ft.
    let (kind, src, acc_dst) = if funct < 0x3C {
        match funct {
            0x00..=0x03 => (0, 0, false),
            0x04..=0x07 => (1, 0, false),
            0x08..=0x0B => (3, 0, false),
            0x0C..=0x0F => (4, 0, false),
            0x18..=0x1B => (2, 0, false),
            0x1C => (2, 1, false),
            0x1E => (2, 2, false),
            0x20 => (0, 1, false),
            0x21 => (3, 1, false),
            0x22 => (0, 2, false),
            0x23 => (3, 2, false),
            0x24 => (1, 1, false),
            0x25 => (4, 1, false),
            0x26 => (1, 2, false),
            0x27 => (4, 2, false),
            0x28 => (0, 3, false),
            0x29 => (3, 3, false),
            0x2A => (2, 3, false),
            0x2C => (1, 3, false),
            0x2D => (4, 3, false),
            0x2E => (6, 3, false),
            _ => return false,
        }
    } else {
        match (funct & 3) | ((word >> 6) & 31) << 2 {
            0x00..=0x03 => (0, 0, true),
            0x04..=0x07 => (1, 0, true),
            0x08..=0x0B => (3, 0, true),
            0x0C..=0x0F => (4, 0, true),
            0x18..=0x1B => (2, 0, true),
            0x1C => (2, 1, true),
            0x1E => (2, 2, true),
            0x20 => (0, 1, true),
            0x21 => (3, 1, true),
            0x22 => (0, 2, true),
            0x23 => (3, 2, true),
            0x24 => (1, 1, true),
            0x25 => (4, 1, true),
            0x26 => (1, 2, true),
            0x27 => (4, 2, true),
            0x28 => (0, 3, true),
            0x29 => (3, 3, true),
            0x2A => (2, 3, true),
            0x2C => (1, 3, true),
            0x2D => (4, 3, true),
            0x2E => (5, 3, true),
            _ => return false,
        }
    };
    if !FLAGS && !acc_dst && fd == 0 {
        return true; // writes vf0: nothing (no flags in this mode)
    }
    let s = v.vf[fs];
    let t = match src {
        0 => [v.vf[ft][bc]; 4],
        1 => [v.vi[VI_Q]; 4],
        2 => [v.vi[VI_I]; 4],
        _ => v.vf[ft],
    };
    let acc = v.acc;
    // OPMULA / OPMSUB: fs.yzx * ft.zxy into xyz.
    let (s, t, lanes) = if kind >= 5 { ([s[1], s[2], s[0], 0], [t[2], t[0], t[1], 0], 0xE) } else { (s, t, dmask) };
    let uses_acc = matches!(kind, 3 | 4 | 6);
    // Operands of the written lanes below 2^63 (biased exponent < 0xBE): no overflow, no
    // exponent 255.
    let big = |x: [u32; 4]| (0..4).any(|k| lanes & (8 >> k) != 0 && (x[k] >> 23) & 0xFF >= 0xBE);
    if big(s) || big(t) || (uses_acc && big(acc)) {
        return false;
    }
    unsafe {
        let ld = |x: [u32; 4]| _mm_castsi128_ps(_mm_loadu_si128(x.as_ptr() as *const __m128i));
        let (a, b, c) = (ld(s), ld(t), ld(acc));
        let r = match kind {
            0 => _mm_add_ps(a, b),
            1 => _mm_sub_ps(a, b),
            2 | 5 => _mm_mul_ps(a, b),
            3 => _mm_add_ps(c, _mm_mul_ps(a, b)),
            _ => _mm_sub_ps(c, _mm_mul_ps(a, b)),
        };
        let mut out = [0u32; 4];
        _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, _mm_castps_si128(r));
        if FLAGS {
            let mut mac = 0u32;
            for k in 0..4 {
                if lanes & (8 >> k) != 0 {
                    if out[k] & 0x7F80_0000 == 0 {
                        return false;
                    }
                    if out[k] & 0x8000_0000 != 0 {
                        mac |= 1 << (3 - k + 4);
                    }
                }
            }
            // As ee_core::vu0::set_mac.
            v.vi[VI_MAC] = mac;
            let mut st = v.vi[VI_STATUS] & 0xFF0 & !0x0F;
            if mac & 0x00F0 != 0 {
                st |= 2;
            }
            st |= (st & 0xF) << 6;
            v.vi[VI_STATUS] = st | (v.vi[VI_STATUS] & 0x30);
            if !acc_dst && fd == 0 {
                return true;
            }
        }
        let dst = if acc_dst { &mut v.acc } else { &mut v.vf[fd] };
        for k in 0..4 {
            if lanes & (8 >> k) != 0 {
                dst[k] = out[k];
            }
        }
    }
    true
}

/// The upper operations that set no MAC flags, exactly as ee_core::vu0::upper_ex computes
/// them, without the f64 path (they ran there and were a quarter of all VU1 pairs): MAX / MINI
/// (sign-magnitude integer compare), ABS, ITOF (int -> float: exact up to the round toward zero
/// MXCSR is set to for VU1 programs, then an exact power-of-two scale), FTOI (exact scale and
/// truncation when the result fits in 31 bits; else the exact path), CLIP (single compares,
/// exact unless an operand has exponent 255). False: not one of them, or a case for the exact path.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn upper_int(v: &mut VuRegs, word: u32) -> bool {
    use std::arch::x86_64::*;
    let dmask = (word >> 21) & 0xF;
    let ft = ((word >> 16) & 31) as usize;
    let fs = ((word >> 11) & 31) as usize;
    let fd = ((word >> 6) & 31) as usize;
    let bc = (word & 3) as usize;
    let funct = word & 0x3F;
    if funct < 0x3C {
        let (max, src) = match funct {
            0x10..=0x13 => (true, 0),
            0x14..=0x17 => (false, 0),
            0x1D => (true, 2),
            0x1F => (false, 2),
            0x2B => (true, 3),
            0x2F => (false, 3),
            _ => return false,
        };
        if fd == 0 {
            return true;
        }
        let s = v.vf[fs];
        let t = match src {
            0 => [v.vf[ft][bc]; 4],
            2 => [v.vi[VI_I]; 4],
            _ => v.vf[ft],
        };
        // Sign-magnitude order as two's complement order.
        let key = |x: u32| (x ^ ((((x as i32) >> 31) as u32) & 0x7FFF_FFFF)) as i32;
        for k in 0..4 {
            if dmask & (8 >> k) != 0 {
                v.vf[fd][k] = if (key(s[k]) >= key(t[k])) == max { s[k] } else { t[k] };
            }
        }
        return true;
    }
    let op = (funct & 3) | ((word >> 6) & 31) << 2;
    match op {
        0x1D => {
            // ABS
            if ft != 0 {
                let s = v.vf[fs];
                for k in 0..4 {
                    if dmask & (8 >> k) != 0 {
                        v.vf[ft][k] = s[k] & 0x7FFF_FFFF;
                    }
                }
            }
            true
        }
        0x10..=0x13 => {
            // ITOF0/4/12/15
            if ft != 0 {
                let inv = [1.0f32, 1.0 / 16.0, 1.0 / 4096.0, 1.0 / 32768.0][(op & 3) as usize];
                let s = v.vf[fs];
                let mut out = [0u32; 4];
                unsafe {
                    let x = _mm_cvtepi32_ps(_mm_loadu_si128(s.as_ptr() as *const __m128i));
                    let r = _mm_mul_ps(x, _mm_set1_ps(inv));
                    _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, _mm_castps_si128(r));
                }
                for k in 0..4 {
                    if dmask & (8 >> k) != 0 {
                        v.vf[ft][k] = out[k];
                    }
                }
            }
            true
        }
        0x14..=0x17 => {
            // FTOI0/4/12/15: truncate; results that need saturation go the exact way.
            if ft == 0 {
                return true;
            }
            let sh = [0u32, 4, 12, 15][(op & 3) as usize];
            let s = v.vf[fs];
            for k in 0..4 {
                if dmask & (8 >> k) != 0 && ((s[k] >> 23) & 0xFF) + sh >= 0x9E {
                    return false;
                }
            }
            let mut out = [0u32; 4];
            unsafe {
                let x = _mm_castsi128_ps(_mm_loadu_si128(s.as_ptr() as *const __m128i));
                let r = _mm_cvttps_epi32(_mm_mul_ps(x, _mm_set1_ps((1u32 << sh) as f32)));
                _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, r);
            }
            for k in 0..4 {
                if dmask & (8 >> k) != 0 {
                    v.vf[ft][k] = out[k];
                }
            }
            true
        }
        0x1F => {
            // CLIPw.xyz
            let s = v.vf[fs];
            let w = v.vf[ft][3];
            if (w >> 23) & 0xFF == 0xFF || (0..3).any(|k| (s[k] >> 23) & 0xFF == 0xFF) {
                return false;
            }
            // Exponent 0 is zero (DAZ is set for VU1 programs; written explicitly anyway).
            let fl = |x: u32| if (x >> 23) & 0xFF == 0 { 0.0f32 } else { f32::from_bits(x) };
            let wv = fl(w).abs();
            let mut flags = 0u32;
            for k in 0..3 {
                let x = fl(s[k]);
                if x > wv {
                    flags |= 1 << (2 * k);
                }
                if x < -wv {
                    flags |= 1 << (2 * k + 1);
                }
            }
            v.vi[VI_CLIP] = ((v.vi[VI_CLIP] << 6) | flags) & 0xFF_FFFF;
            true
        }
        _ => false,
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn upper_int(_v: &mut VuRegs, _word: u32) -> bool {
    false
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn upper_sse(_v: &mut VuRegs, _word: u32) -> bool {
    false
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn upper_sse_f<const FLAGS: bool>(_v: &mut VuRegs, _word: u32) -> bool {
    false
}

/// Set MXCSR to round toward zero with flush-to-zero and denormals-are-zero for a VU1 program
/// (the SSE FMAC path); returns the previous value.
#[cfg(target_arch = "x86_64")]
#[allow(deprecated)]
fn mxcsr_vu() -> u32 {
    use std::arch::x86_64::*;
    unsafe {
        let old = _mm_getcsr();
        _mm_setcsr((old & !0x6000) | 0x6000 | 0x8000 | 0x0040);
        old
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(deprecated)]
fn mxcsr_restore(old: u32) {
    unsafe { std::arch::x86_64::_mm_setcsr(old) }
}

#[cfg(not(target_arch = "x86_64"))]
fn mxcsr_vu() -> u32 {
    0
}

#[cfg(not(target_arch = "x86_64"))]
fn mxcsr_restore(_old: u32) {}

// ---- VU1 capture / replay (performance + parity harness, bin `vu-bench`) ----
//
// `T5_VUCAPTURE=<second>`: from that second of the run on, the next CAPTURE_RUNS VU1 program runs
// are written to `vucap/run_NNNN.bin` (start pc, VIF1 TOP / ITOP, micro code, data memory,
// registers) together with a reference hash of what the run produced (every XGKICK: address
// and data memory at that moment; registers and data memory at the end). `vu-bench` replays
// them: it checks the hashes (bit-exact translation) and times the runs.

/// XGKICK only hashes the packet instead of sending it to the GS (`vu-bench`).
/// vu-bench: restore only (measures the replay overhead).
pub static NORUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static BENCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// XGKICKs are folded into `RUN_HASH`.
pub static KICK_HASH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RUN_HASH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const CAPTURE_RUNS: u32 = 3000;
static CAPTURED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fnv64(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn kick_hash(data: &[u8], addr: u32) {
    let h = RUN_HASH.load(std::sync::atomic::Ordering::Relaxed);
    let h = fnv64(fnv64(h, &addr.to_le_bytes()), data);
    RUN_HASH.store(h, std::sync::atomic::Ordering::Relaxed);
}

/// The registers as bytes (VuRegs is repr(C), plain words).
pub fn regs_bytes(r: &VuRegs) -> &[u8] {
    unsafe { std::slice::from_raw_parts(r as *const VuRegs as *const u8, std::mem::size_of::<VuRegs>()) }
}

pub fn regs_bytes_mut(r: &mut VuRegs) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(r as *mut VuRegs as *mut u8, std::mem::size_of::<VuRegs>()) }
}

/// The hash a run leaves: kicks so far, then end registers and data memory.
pub fn end_hash(vu: &Vu) -> u64 {
    let h = RUN_HASH.load(std::sync::atomic::Ordering::Relaxed);
    fnv64(fnv64(h, regs_bytes(&vu.regs)), &vu.data)
}

pub fn reset_run_hash() {
    RUN_HASH.store(0xcbf2_9ce4_8422_2325, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    static CAP_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn capture_start(m: &mut Machine, pc: u32) -> bool {
    let from: u64 = std::env::var("T5_VUCAPTURE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if m.now() / crate::EE_HZ < from || CAPTURED.load(std::sync::atomic::Ordering::Relaxed) >= CAPTURE_RUNS {
        return false;
    }
    let v = &m.vu1;
    let mut b = Vec::with_capacity(16 + v.code.len() + v.data.len() + 1024);
    for w in [pc, m.vif[1].top, m.vif[1].itop, v.code.len() as u32, v.data.len() as u32] {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b.extend_from_slice(&v.code);
    b.extend_from_slice(&v.data);
    b.extend_from_slice(regs_bytes(&v.regs));
    CAP_BUF.with(|c| *c.borrow_mut() = b);
    reset_run_hash();
    KICK_HASH.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

fn capture_end(m: &mut Machine) {
    KICK_HASH.store(false, std::sync::atomic::Ordering::Relaxed);
    let h = end_hash(&m.vu1);
    let i = CAPTURED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    CAP_BUF.with(|c| {
        let mut b = std::mem::take(&mut *c.borrow_mut());
        b.extend_from_slice(&h.to_le_bytes());
        let _ = std::fs::create_dir_all("vucap");
        let _ = std::fs::write(format!("vucap/run_{i:04}.bin"), &b);
    });
}

/// Replay one captured run (vu-bench): restore the state, run, return the end hash.
pub fn replay(m: &mut Machine, cap: &[u8], hash: bool) -> u64 {
    let w = |i: usize| u32::from_le_bytes(cap[4 * i..4 * i + 4].try_into().unwrap());
    let (pc, top, itop, cl, dl) = (w(0), w(1), w(2), w(3) as usize, w(4) as usize);
    let mut o = 20;
    let v = &mut m.vu1;
    if v.code[..] != cap[o..o + cl] {
        v.code.copy_from_slice(&cap[o..o + cl]);
        v.code_gen += 1;
    }
    o += cl;
    v.data.copy_from_slice(&cap[o..o + dl]);
    o += dl;
    let rl = std::mem::size_of::<VuRegs>();
    regs_bytes_mut(&mut v.regs).copy_from_slice(&cap[o..o + rl]);
    m.vif[1].top = top;
    m.vif[1].itop = itop;
    reset_run_hash();
    KICK_HASH.store(hash, std::sync::atomic::Ordering::Relaxed);
    if !NORUN.load(std::sync::atomic::Ordering::Relaxed) {
        run_inner(m, 1, pc);
    }
    KICK_HASH.store(false, std::sync::atomic::Ordering::Relaxed);
    if hash { end_hash(&m.vu1) } else { 0 }
}

/// The reference end hash stored with a capture.
pub fn capture_ref(cap: &[u8]) -> u64 {
    u64::from_le_bytes(cap[cap.len() - 8..].try_into().unwrap())
}

/// T5_VUSHAPES (vu-bench): entry shapes per block start.
pub static SHAPES_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static SHAPES: std::sync::Mutex<Option<std::collections::HashMap<(u32, String), u64>>> = std::sync::Mutex::new(None);

/// Write the recorded entry shapes (T5_VUSHAPES) to vu_dumps/shapes.txt.
pub fn dump_shapes() {
    let g = SHAPES.lock().unwrap();
    let Some(map) = g.as_ref() else { return };
    let mut out = String::new();
    for ((_, sh), _) in map {
        out.push_str(sh);
        out.push('\n');
    }
    let _ = std::fs::create_dir_all("vu_dumps");
    let _ = std::fs::write("vu_dumps/shapes.txt", out);
}
