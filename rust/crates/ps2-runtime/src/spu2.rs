//! SPU2 (the IOP's sound processor): 2 MiB of sound RAM, two cores of 24 ADPCM voices each with
//! ADSR envelopes, pitch modulation and noise, the auto-DMA PCM input (streamed music), core 0
//! mixed into core 1, output at 48 kHz in step with device time ([`update`]) to the front end.
//!
//! Registers (per core at 0x000 / 0x400 of 0x1F900000): voice parameters 0x000..0x17F (16 bytes
//! per voice: VOLL, VOLR, PITCH, ADSR1, ADSR2, ENVX, VOLXL, VOLXR), PMON / NON / VMIX*
//! 0x180..0x197 (two words each: voices 0..15, 16..23), MMIX 0x198, ATTR 0x19A, IRQA 0x19C,
//! KON 0x1A0, KOFF 0x1A4, TSA 0x1A8, DATA 0x1AC (manual transfer port), ADMAS 0x1B0, voice
//! addresses 0x1C0..0x2DF (12 bytes per voice: SSA, LSAX, NAX, each high / low), ESA / EEA
//! 0x2E0 / 0x33C, ENDX 0x340, STAT 0x344; MVOL / EVOL / AVOL / BVOL / MVOLX from 0x760 + 0x28 *
//! core; SPDIF / IRQ info 0x7C0... Addresses count 16-bit halfwords. Transfers finish at once, so
//! STAT never shows busy.

use crate::{Machine, EE_HZ};
use std::collections::VecDeque;
use std::sync::Mutex;

pub const SAMPLE_RATE: u32 = 48_000;
const RAM: usize = 2 << 20;
const POS: [i32; 5] = [0, 60, 115, 98, 122];
const NEG: [i32; 5] = [0, 0, -52, -55, -60];

/// Interleaved stereo samples for the front end's audio output.
pub struct AudioShared {
    pub samples: Mutex<VecDeque<i16>>,
}

impl AudioShared {
    pub fn new() -> AudioShared {
        AudioShared { samples: Mutex::new(VecDeque::new()) }
    }
}

impl Default for AudioShared {
    fn default() -> Self {
        AudioShared::new()
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
enum Phase {
    #[default]
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, Default)]
struct Voice {
    /// Next block address (bytes) and loop address (bytes).
    addr: u32,
    loop_addr: u32,
    loop_set: bool,
    counter: u32,
    samples: [i16; 28],
    prev: [i32; 3],
    hist: [i32; 2],
    decoded: bool,
    flags: u8,
    phase: Phase,
    level: i32,
    env_wait: u32,
    out: i32,
}

pub struct Spu2 {
    pub ram: Vec<u8>,
    pub regs: Vec<u16>,
    v: [[Voice; 24]; 2],
    endx: [u32; 2],
    noise: [i32; 2],
    noise_count: [i32; 2],
    /// Auto-DMA PCM input per core: stereo samples waiting to be played.
    pub stream: [VecDeque<(i16, i16)>; 2],
    generated: u64,
    start: Option<u64>,
    pub irq: bool,
    /// Output sample counter (PCSX2 `Cycles`): the reverb alternates L / R on it and walks its
    /// work area with it.
    cycles: u32,
    rvb: [Reverb; 2],
    /// Voice volumes (L, R) and master volumes (L, R) per core.
    vvol: [[[VolSlide; 2]; 24]; 2],
    mvol: [[VolSlide; 2]; 2],
}

/// Reverb resampling state per core (PCSX2 `RevbDownBuf` / `RevbUpBuf`): the effect runs at
/// 24 kHz, one channel per output sample, through 39-tap half-band filters.
#[derive(Clone, Copy)]
struct Reverb {
    down: [[i16; 128]; 2],
    up: [[i16; 128]; 2],
    pos: usize,
}

impl Default for Reverb {
    fn default() -> Self {
        Reverb { down: [[0; 128]; 2], up: [[0; 128]; 2], pos: 0 }
    }
}

const NUM_TAPS: usize = 39;
const FILTER_DOWN: [i32; NUM_TAPS] = [
    -1, 0, 2, 0, -10, 0, 35, 0, -103, 0, 266, 0, -616, 0, 1332, 0, -2960, 0, 10246, 16384, 10246, 0, -2960, 0, 1332, 0, -616, 0,
    266, 0, -103, 0, 35, 0, -10, 0, 2, 0, -1,
];

fn clamp_mix(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

/// The voice interpolation table (PCSX2 `interpTable`, the hardware's 4-point Gaussian
/// filter; generator by Near / nocash).
fn interp_table() -> &'static [[i32; 4]; 256] {
    static T: std::sync::OnceLock<[[i32; 4]; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        use std::f64::consts::PI;
        let mut table = [0f64; 512];
        for n in 0..512 {
            let k = 0.5 + n as f64;
            let s = (PI * k * 2.048 / 1024.0).sin();
            let t = ((PI * k * 2.000 / 1023.0).cos() - 1.0) * 0.50;
            let u = ((PI * k * 4.000 / 1023.0).cos() - 1.0) * 0.08;
            table[511 - n] = s * (t + u + 1.0) / k;
        }
        let sum: f64 = table.iter().sum();
        let scale = (0x7f80 * 128) as f64 / sum;
        for n in table.iter_mut() {
            *n *= scale;
        }
        let mut r = [[0i32; 4]; 256];
        for phase in 0..256 {
            let ps = table[phase] + table[phase + 256] + table[511 - phase] + table[255 - phase];
            let diff = (ps - 0x7f80 as f64) / 4.0;
            r[255 - phase] = [
                (table[phase] - diff).round() as i16 as i32,
                (table[phase + 256] - diff).round() as i16 as i32,
                (table[511 - phase] - diff).round() as i16 as i32,
                (table[255 - phase] - diff).round() as i16 as i32,
            ];
        }
        r
    })
}

impl Default for Spu2 {
    fn default() -> Self {
        Spu2::new()
    }
}

/// A volume register with its sweep (PCSX2 V_VolumeSlide): bit 15 clear = fixed volume
/// (value << 1), set = slide by Step / Shift, linear or exponential, up or down.
#[derive(Clone, Copy, Default)]
struct VolSlide {
    reg: u16,
    counter: u32,
    value: i32,
}

impl VolSlide {
    fn set(&mut self, v: u16) {
        self.reg = v;
        if v & 0x8000 == 0 {
            self.value = ((v << 1) as i16) as i32;
        }
    }

    fn update(&mut self) {
        let r = self.reg;
        if r & 0x8000 == 0 {
            return;
        }
        let (step, shift) = ((r & 3) as i32, ((r >> 2) & 0x1F) as i32);
        let (phase, decr, exp) = (r & 0x1000 != 0, r & 0x2000 != 0, r & 0x4000 != 0);
        let mut step_size = 7 - step;
        if decr {
            step_size = !step_size;
        }
        let mut counter_inc = 0x8000u32 >> (shift - 11).max(0);
        let mut level_inc = step_size << (11 - shift).max(0);
        if exp {
            if !decr && self.value > 0x6000 {
                counter_inc >>= 2;
            }
            if decr {
                level_inc = ((level_inc * self.value) >> 15) as i16 as i32;
            }
        }
        if step != 3 && shift != 0x1F {
            counter_inc = counter_inc.max(1);
        }
        self.counter += counter_inc;
        if !(exp && decr) && phase {
            level_inc = -level_inc;
        }
        if self.counter >= 0x8000 {
            self.counter = 0;
            let (lo, hi) = if !decr {
                (-32768, 32767)
            } else if exp {
                (0, 32767)
            } else if phase {
                (-32768, 0)
            } else {
                (0, 32767)
            };
            self.value = (self.value + level_inc).clamp(lo, hi);
        }
    }
}

impl Spu2 {
    pub fn new() -> Spu2 {
        let mut s = Spu2 {
            ram: vec![0; RAM],
            regs: vec![0; 0x400],
            v: [[Voice::default(); 24]; 2],
            endx: [0; 2],
            noise: [1; 2],
            noise_count: [0; 2],
            stream: Default::default(),
            generated: 0,
            start: None,
            irq: false,
            cycles: 0,
            rvb: [Reverb::default(); 2],
            vvol: Default::default(),
            mvol: Default::default(),
        };
        // Power-on values (PCSX2 V_Core::Init): effect areas at the top of RAM, MMIX all on.
        for c in 0..2 {
            s.set_reg(c, 0x2E0, if c == 1 { 0xF } else { 0xE });
            s.set_reg(c, 0x2E2, 0xFFF8);
            s.set_reg(c, 0x33C, if c == 1 { 0xF } else { 0xE });
            s.set_reg(c, 0x198, if c == 1 { 0xFFC } else { 0xFF0 });
        }
        s
    }

    /// A 20-bit halfword address register pair (high word first).
    fn waddr(&self, core: usize, r: u32) -> u32 {
        ((self.reg(core, r) as u32) << 16 | self.reg(core, r + 2) as u32) & 0xF_FFFF
    }

    fn mem(&self, w: u32) -> i32 {
        let a = (w as usize * 2) & (RAM - 1);
        i16::from_le_bytes([self.ram[a], self.ram[a + 1]]) as i32
    }

    fn set_mem(&mut self, w: u32, v: i32) {
        let a = (w as usize * 2) & (RAM - 1);
        self.ram[a..a + 2].copy_from_slice(&(clamp_mix(v) as i16).to_le_bytes());
    }

    /// One sample of a core's reverb (PCSX2 V_Core::DoReverb): `input` = wet mix.
    fn reverb(&mut self, core: usize, input: (i32, i32)) -> (i32, i32) {
        let esa = self.waddr(core, 0x2E0);
        let eea = (self.reg(core, 0x33C) as u32 & 0xF) << 16 | 0xFFFF;
        if esa >= eea {
            return (0, 0);
        }
        let pos = self.rvb[core].pos;
        {
            let (l, r) = (clamp_mix(input.0) as i16, clamp_mix(input.1) as i16);
            let rv = &mut self.rvb[core];
            rv.down[0][pos] = l;
            rv.down[1][pos] = r;
            rv.down[0][pos | 64] = l;
            rv.down[1][pos | 64] = r;
        }
        let r = self.cycles & 1 != 0;
        let span = eea - esa + 1;
        let cyc = self.cycles >> 1;
        let idx = |off: u32| (cyc.wrapping_add(off) % span + esa) & 0xF_FFFF;
        let sel = |s: &Self, l: u32, rr: u32| if r { s.waddr(core, rr) } else { s.waddr(core, l) };
        let same_src = idx(sel(self, 0x304, 0x308));
        let same_dst_off = sel(self, 0x2EC, 0x2F0);
        let same_dst = idx(same_dst_off);
        let same_prv = idx(same_dst_off.wrapping_sub(1));
        // DIFF sources are crossed (R reads DIFF_L_SRC), as in PCSX2.
        let diff_src = idx(sel(self, 0x328, 0x324));
        let diff_dst_off = sel(self, 0x30C, 0x310);
        let diff_dst = idx(diff_dst_off);
        let diff_prv = idx(diff_dst_off.wrapping_sub(1));
        let comb = [
            idx(sel(self, 0x2F4, 0x2F8)),
            idx(sel(self, 0x2FC, 0x300)),
            idx(sel(self, 0x314, 0x318)),
            idx(sel(self, 0x31C, 0x320)),
        ];
        let apf1_dst_off = sel(self, 0x32C, 0x330);
        let apf2_dst_off = sel(self, 0x334, 0x338);
        let apf1_src = idx(apf1_dst_off.wrapping_sub(self.waddr(core, 0x2E4)));
        let apf1_dst = idx(apf1_dst_off);
        let apf2_src = idx(apf2_dst_off.wrapping_sub(self.waddr(core, 0x2E8)));
        let apf2_dst = idx(apf2_dst_off);
        let base = 0x760 + 0x28 * core as u32;
        let v = |s: &Self, o: u32| s.regs[((base + o) / 2) as usize] as i16 as i32;
        let (iir, wall, apf1v, apf2v) = (v(self, 0x14), v(self, 0x1E), v(self, 0x20), v(self, 0x22));
        let combv = [v(self, 0x16), v(self, 0x18), v(self, 0x1A), v(self, 0x1C)];
        let in_coef = if r { v(self, 0x26) } else { v(self, 0x24) };
        let mul = |x: i32, y: i32| (x * y) >> 15;
        // Downsample the channel of this cycle.
        let down = {
            let rv = &self.rvb[core];
            let i0 = (pos + 64 - NUM_TAPS) & 63;
            let mut acc = 0i32;
            for (k, c) in FILTER_DOWN.iter().enumerate() {
                acc += rv.down[r as usize][i0 + k] as i32 * c;
            }
            clamp_mix(acc >> 15)
        };
        let inp = mul(in_coef, down);
        let same = mul(iir, inp + mul(wall, self.mem(same_src)) - self.mem(same_prv)) + self.mem(same_prv);
        let diff = mul(iir, inp + mul(wall, self.mem(diff_src)) - self.mem(diff_prv)) + self.mem(diff_prv);
        let mut out = 0;
        for k in 0..4 {
            out += mul(combv[k], self.mem(comb[k]));
        }
        let apf1 = out - mul(apf1v, self.mem(apf1_src));
        out = self.mem(apf1_src) + mul(apf1v, apf1);
        let apf2 = out - mul(apf2v, self.mem(apf2_src));
        out = self.mem(apf2_src) + mul(apf2v, apf2);
        if self.reg(core, 0x19A) & 0x80 != 0 {
            self.set_mem(same_dst, same);
            self.set_mem(diff_dst, diff);
            self.set_mem(apf1_dst, apf1);
            self.set_mem(apf2_dst, apf2);
        }
        let out = clamp_mix(out) as i16;
        let rv = &mut self.rvb[core];
        let (ri, li) = (r as usize, (!r) as usize);
        rv.up[ri][pos] = out;
        rv.up[li][pos] = 0;
        rv.up[ri][pos | 64] = out;
        rv.up[li][pos | 64] = 0;
        rv.pos = (pos + 1) & 63;
        // Upsample both channels (coefficients doubled: every other sample is zero).
        let i0 = (rv.pos + 64 - NUM_TAPS) & 63;
        let (mut l, mut rr) = (0i32, 0i32);
        for (k, c) in FILTER_DOWN.iter().enumerate() {
            let c = (c * 2).clamp(-32768, 32767);
            l += rv.up[0][i0 + k] as i32 * c;
            rr += rv.up[1][i0 + k] as i32 * c;
        }
        (clamp_mix(l >> 15), clamp_mix(rr >> 15))
    }

    fn reg(&self, core: usize, r: u32) -> u16 {
        self.regs[((0x400 * core as u32 + r) / 2) as usize]
    }

    fn set_reg(&mut self, core: usize, r: u32, v: u16) {
        self.regs[((0x400 * core as u32 + r) / 2) as usize] = v;
    }

    fn pair(&self, core: usize, r: u32) -> u32 {
        (self.reg(core, r) as u32) | ((self.reg(core, r + 2) as u32) << 16)
    }

    fn addr24(&self, core: usize, r: u32) -> u32 {
        (((self.reg(core, r) as u32) << 16 | self.reg(core, r + 2) as u32) & 0xF_FFFF) * 2
    }

    pub fn read16(&mut self, off: u32) -> u16 {
        let o = off & 0x7FE;
        if o >= 0x760 {
            return self.regs[(o / 2) as usize];
        }
        let core = (o >> 10) as usize;
        let r = o & 0x3FF;
        match r {
            0x000..=0x17F if r & 0xF == 0xA => self.v[core][(r / 16) as usize].level as u16,
            0x1C0..=0x2DF => {
                let n = ((r - 0x1C0) / 12) as usize;
                match (r - 0x1C0) % 12 {
                    8 => ((self.v[core][n].addr / 2) >> 16) as u16,
                    10 => (self.v[core][n].addr / 2) as u16,
                    _ => self.regs[(o / 2) as usize],
                }
            }
            0x340 => self.endx[core] as u16,
            0x342 => (self.endx[core] >> 16) as u16,
            0x344 => 0x0080,
            0x1AC => {
                let tsa = self.tsa(core);
                self.set_tsa(core, tsa + 1);
                let a = (tsa as usize * 2) & (RAM - 1);
                u16::from_le_bytes([self.ram[a], self.ram[a + 1]])
            }
            _ => self.regs[(o / 2) as usize],
        }
    }

    pub fn write16(&mut self, off: u32, v: u16) {
        let o = off & 0x7FE;
        if o & 0x3FF == 0x19A && o < 0x760 && v & 0x40 == 0 {
            self.regs[0x7C2 / 2] &= !(4 << (o >> 10));
        }
        self.regs[(o / 2) as usize] = v;
        if (0x760..0x760 + 0x50).contains(&o) && (o - 0x760) % 0x28 < 4 {
            self.mvol[((o - 0x760) / 0x28) as usize][((o - 0x760) % 0x28 / 2) as usize].set(v);
        }
        if o < 0x760 && o & 0x3FF < 0x180 && o & 0xF < 4 {
            self.vvol[(o >> 10) as usize][((o & 0x3FF) / 16) as usize][((o & 0xF) / 2) as usize].set(v);
        }
        if o >= 0x760 {
            return;
        }
        let core = (o >> 10) as usize;
        let r = o & 0x3FF;
        match r {
            0x000..=0x17F if r & 0xF == 0xA => {
                if crate::diag::enabled("T5_SPULOG") {
                    crate::diag::log(format!("spu {} ENVX c{core} v{} = {v:#x}", self.cycles, r / 16));
                }
                self.v[core][(r / 16) as usize].level = (v & 0x7FFF) as i32
            }
            0x1C0..=0x2DF => {
                let n = ((r - 0x1C0) / 12) as usize;
                if (r - 0x1C0) % 12 == 6 {
                    self.v[core][n].loop_addr = self.addr24(core, 0x1C0 + 12 * n as u32 + 4);
                    self.v[core][n].loop_set = true;
                }
            }
            0x1A0 | 0x1A2 => {
                let base = if r == 0x1A0 { 0 } else { 16 };
                for i in 0..16 {
                    if v & (1 << i) != 0 && base + i < 24 {
                        self.key_on(core, base + i);
                    }
                }
            }
            0x1A4 | 0x1A6 => {
                let base = if r == 0x1A4 { 0 } else { 16 };
                for i in 0..16 {
                    if v & (1 << i) != 0 && base + i < 24 {
                        if crate::diag::enabled("T5_SPULOG") {
                            crate::diag::log(format!("spu {} KOFF c{core} v{} level {}", self.cycles, base + i, self.v[core][base + i].level));
                        }
                        let vo = &mut self.v[core][base + i];
                        if vo.phase != Phase::Off {
                            vo.phase = Phase::Release;
                            vo.env_wait = 0;
                        }
                    }
                }
            }
            0x340 | 0x342 => self.endx[core] = 0,
            0x1AC => {
                let tsa = self.tsa(core);
                let a = (tsa as usize * 2) & (RAM - 1);
                self.ram[a..a + 2].copy_from_slice(&v.to_le_bytes());
                self.set_tsa(core, tsa + 1);
            }
            0x19A if v & 0x8000 != 0 => {}
            _ => {}
        }
    }

    pub fn tsa(&self, core: usize) -> u32 {
        (self.reg(core, 0x1A8) as u32) << 16 | self.reg(core, 0x1AA) as u32
    }

    pub fn set_tsa(&mut self, core: usize, v: u32) {
        let v = v & 0xF_FFFF;
        self.set_reg(core, 0x1A8, (v >> 16) as u16);
        self.set_reg(core, 0x1AA, v as u16);
    }

    pub fn admas(&self, core: usize) -> bool {
        self.reg(core, 0x1B0) & (1 << core) != 0
    }

    pub fn dma_write(&mut self, core: usize, data: &[u8]) {
        let tsa = self.tsa(core) as usize * 2;
        self.check_irq_range(tsa & (RAM - 1), data.len());
        if crate::diag::enabled("T5_SPULOG") {
            let nz = data.iter().filter(|b| **b != 0).count();
            crate::diag::log(format!("spu {} DMA c{core} to {tsa:#x} len {:#x} nonzero {nz}", self.cycles, data.len()));
        }
        for (i, b) in data.iter().enumerate() {
            self.ram[(tsa + i) & (RAM - 1)] = *b;
        }
        self.set_tsa(core, ((tsa + data.len()) / 2) as u32);
    }

    pub fn dma_read(&mut self, core: usize, bytes: usize) -> Vec<u8> {
        let tsa = self.tsa(core) as usize * 2;
        self.check_irq_range(tsa & (RAM - 1), bytes);
        let d = (0..bytes).map(|i| self.ram[(tsa + i) & (RAM - 1)]).collect();
        self.set_tsa(core, ((tsa + bytes) / 2) as u32);
        d
    }

    /// Auto-DMA PCM: blocks of 0x200 bytes left channel, 0x200 bytes right channel.
    pub fn stream_in(&mut self, core: usize, data: &[u8]) {
        for blk in data.chunks(0x400) {
            if blk.len() < 0x400 {
                break;
            }
            for i in 0..256 {
                let l = i16::from_le_bytes([blk[2 * i], blk[2 * i + 1]]);
                let r = i16::from_le_bytes([blk[0x200 + 2 * i], blk[0x201 + 2 * i]]);
                self.stream[core].push_back((l, r));
            }
        }
        let q = &mut self.stream[core];
        while q.len() > 48_000 {
            q.pop_front();
        }
    }

    fn key_on(&mut self, core: usize, n: usize) {
        let ssa = self.addr24(core, 0x1C0 + 12 * n as u32);
        if crate::diag::enabled("T5_SPULOG") {
            let r = |o: u32| self.reg(core, 16 * n as u32 + o);
            crate::diag::log(format!("spu {} KON c{core} v{n} ssa {ssa:#x} pitch {:#x} adsr {:#06x} {:#06x} vol {:#06x} {:#06x} lsa {:#x} hdr {:02x}{:02x}", self.cycles, r(4), r(6), r(8), r(0), r(2), self.addr24(core, 0x1C4 + 12 * n as u32), self.ram[ssa as usize & (RAM - 1)], self.ram[(ssa as usize + 1) & (RAM - 1)]));
        }
        let v = &mut self.v[core][n];
        v.addr = ssa;
        v.loop_addr = ssa;
        v.loop_set = false;
        v.counter = 0;
        v.decoded = false;
        v.hist = [0; 2];
        v.prev = [0; 3];
        v.phase = Phase::Attack;
        v.level = 0;
        v.env_wait = 0;
        self.endx[core] &= !(1 << n);
    }

    /// Check the IRQ address against a voice's block read (bytes).
    fn check_irq(&mut self, _core: usize, addr: u32) {
        for c in 0..2 {
            if self.addr24(c, 0x19C) & !15 == addr & !15 {
                self.irq_call(c);
            }
        }
    }

    /// Check the IRQ address against a DMA / port transfer of `len` bytes at byte address `a`.
    fn check_irq_range(&mut self, a: usize, len: usize) {
        for c in 0..2 {
            let irqa = self.addr24(c, 0x19C) as usize;
            if irqa.wrapping_sub(a) & (RAM - 1) < len {
                self.irq_call(c);
            }
        }
    }

    /// PCSX2 SetIrqCall: one interrupt per core until the game acknowledges it by clearing
    /// ATTR's IRQ enable (bit 6), which clears the core's Info bit (0x7C2 bit 2 + core).
    fn irq_call(&mut self, c: usize) {
        if self.reg(c, 0x19A) & 0x40 != 0 && self.regs[0x7C2 / 2] & (4 << c) == 0 {
            self.regs[0x7C2 / 2] |= 4 << c;
            self.irq = true;
        }
    }

    fn decode_block(&mut self, core: usize, n: usize) {
        let addr = self.v[core][n].addr as usize & (RAM - 1) & !15;
        self.check_irq(core, addr as u32);
        let hdr = self.ram[addr];
        let flags = self.ram[addr + 1];
        let shift = (hdr & 0xF).min(12) as i32;
        let filter = ((hdr >> 4) & 7).min(4) as usize;
        let v = &mut self.v[core][n];
        if v.decoded {
            v.prev = [v.samples[25] as i32, v.samples[26] as i32, v.samples[27] as i32];
        }
        if flags & 4 != 0 && !v.loop_set {
            v.loop_addr = addr as u32;
        }
        for i in 0..28 {
            let b = self.ram[addr + 2 + i / 2];
            let nib = if i & 1 == 0 { b & 0xF } else { b >> 4 };
            let raw = (((nib as i32) << 28) >> 16) >> shift;
            let s = (raw + ((v.hist[0] * POS[filter] + v.hist[1] * NEG[filter] + 32) >> 6)).clamp(-32768, 32767);
            v.hist = [s, v.hist[0]];
            v.samples[i] = s as i16;
        }
        v.flags = flags;
        v.decoded = true;
    }

    fn sample_at(&self, core: usize, n: usize, i: i32) -> i32 {
        let v = &self.v[core][n];
        if i >= 0 { v.samples[i as usize] as i32 } else { v.prev[(3 + i) as usize] }
    }

    fn envelope(&mut self, core: usize, n: usize) {
        let a1 = self.reg(core, 16 * n as u32 + 6);
        let a2 = self.reg(core, 16 * n as u32 + 8);
        let v = &mut self.v[core][n];
        let (lo, hi) = (a1, a2);
        let (exp, dec, shift, step): (bool, bool, i32, i32) = match v.phase {
            Phase::Off => return,
            Phase::Attack => (lo & 0x8000 != 0, false, ((lo >> 10) & 0x1F) as i32, 7 - ((lo >> 8) & 3) as i32),
            Phase::Decay => (true, true, ((lo >> 4) & 0xF) as i32, -8),
            Phase::Sustain => {
                let d = hi & 0x4000 != 0;
                let st = ((hi >> 6) & 3) as i32;
                (hi & 0x8000 != 0, d, ((hi >> 8) & 0x1F) as i32, if d { -8 + st } else { 7 - st })
            }
            Phase::Release => (hi & 0x20 != 0, true, (hi & 0x1F) as i32, -8),
        };
        if v.env_wait > 0 {
            v.env_wait -= 1;
        } else {
            let mut cycles = 1u32 << (shift - 11).max(0);
            let mut s = step << (11 - shift).max(0);
            if exp && !dec && v.level > 0x6000 {
                cycles *= 4;
            }
            if exp && dec {
                s = s * v.level / 0x8000;
            }
            v.env_wait = cycles - 1;
            v.level = (v.level + s).clamp(0, 0x7FFF);
        }
        match v.phase {
            Phase::Attack if v.level >= 0x7FFF => {
                v.phase = Phase::Decay;
                v.env_wait = 0;
            }
            Phase::Decay => {
                let sustain = (((lo & 0xF) as i32) + 1) * 0x800;
                if v.level <= sustain {
                    v.phase = Phase::Sustain;
                    v.env_wait = 0;
                }
            }
            Phase::Release if v.level <= 0 => {
                v.phase = Phase::Off;
                v.level = 0;
            }
            _ => {}
        }
    }

    /// One 48 kHz sample of a core: (dry L, dry R).
    fn core_tick(&mut self, core: usize, ext: (i32, i32)) -> (i32, i32) {
        let pmon = self.pair(core, 0x180);
        let non = self.pair(core, 0x184);
        let vmixl = self.pair(core, 0x188);
        let vmixel = self.pair(core, 0x18C);
        let vmixr = self.pair(core, 0x190);
        let vmixer = self.pair(core, 0x194);
        let attr = self.reg(core, 0x19A);
        // Noise generator (ATTR bits 8..13: clock).
        let nshift = ((attr >> 10) & 0xF) as i32;
        let nstep = ((attr >> 8) & 3) as i32 + 4;
        self.noise_count[core] += nstep;
        let period = 0x20000 >> nshift;
        if self.noise_count[core] >= period {
            self.noise_count[core] -= period;
            let l = self.noise[core];
            let bit = ((l >> 15) ^ (l >> 12) ^ (l >> 11) ^ (l >> 10) ^ 1) & 1;
            self.noise[core] = ((l << 1) | bit) & 0xFFFF;
        }
        let (mut l, mut r) = (0i32, 0i32);
        let (mut wl, mut wr) = (0i32, 0i32);
        let mut prev_out = 0;
        for n in 0..24 {
            self.vvol[core][n][0].update();
            self.vvol[core][n][1].update();
            if self.v[core][n].phase == Phase::Off {
                self.v[core][n].out = 0;
                prev_out = 0;
                continue;
            }
            if !self.v[core][n].decoded {
                self.decode_block(core, n);
            }
            let raw = if non & (1 << n) != 0 {
                self.noise[core] as i16 as i32
            } else {
                let i = (self.v[core][n].counter >> 12) as i32;
                let g = &interp_table()[((self.v[core][n].counter >> 4) & 0xFF) as usize];
                let mut s = 0;
                for k in 0..4 {
                    s += (g[k] * self.sample_at(core, n, i - 3 + k as i32)) >> 15;
                }
                s
            };
            self.envelope(core, n);
            let pitch = self.reg(core, 16 * n as u32 + 4);
            let (vl, vr) = (self.vvol[core][n][0].value, self.vvol[core][n][1].value);
            let v = &mut self.v[core][n];
            let out = (raw * v.level) >> 15;
            v.out = out;
            let (ol, or) = ((out * vl) >> 15, (out * vr) >> 15);
            if vmixl & (1 << n) != 0 {
                l += ol;
            }
            if vmixr & (1 << n) != 0 {
                r += or;
            }
            if vmixel & (1 << n) != 0 {
                wl += ol;
            }
            if vmixer & (1 << n) != 0 {
                wr += or;
            }
            let mut step = pitch.min(0x3FFF) as i32;
            if n > 0 && pmon & (1 << n) != 0 {
                step = (step * (prev_out + 0x8000)) >> 15;
            }
            prev_out = out;
            v.counter += step.clamp(0, 0x3FFF) as u32;
            if v.counter >= 28 << 12 {
                v.counter -= 28 << 12;
                let flags = v.flags;
                if flags & 1 != 0 {
                    if crate::diag::enabled("T5_SPULOG") {
                        crate::diag::log(format!("spu {} END c{core} v{n} at {:#x} flags {flags:#x} loop {:#x}", self.cycles, v.addr, v.loop_addr));
                    }
                    v.addr = v.loop_addr;
                    if flags & 2 == 0 {
                        v.phase = Phase::Release;
                        v.level = 0;
                    }
                    self.endx[core] |= 1 << n;
                } else {
                    v.addr = (v.addr + 16) & (RAM as u32 - 1);
                }
                self.decode_block(core, n);
            }
        }
        let (l, r) = (clamp_mix(l), clamp_mix(r));
        let (wl, wr) = (clamp_mix(wl), clamp_mix(wr));
        let base = 0x760 + 0x28 * core as u32;
        let vreg = |s: &Self, o: u32| s.regs[((base + o) / 2) as usize] as i16 as i32;
        // Auto-DMA PCM input at BVOL (PCSX2 InpVol).
        let mut inp = (0, 0);
        if self.admas(core) || !self.stream[core].is_empty() {
            let (sl, sr) = self.stream[core].pop_front().unwrap_or((0, 0));
            inp = ((sl as i32 * vreg(self, 0xC)) >> 15, (sr as i32 * vreg(self, 0xE)) >> 15);
        }
        let mmix = self.reg(core, 0x198);
        let g = |bit: u32, v: i32| if mmix as u32 & bit != 0 { v } else { 0 };
        let dry = (
            g(0x080, inp.0) + g(0x800, l) + g(0x008, ext.0),
            g(0x040, inp.1) + g(0x400, r) + g(0x004, ext.1),
        );
        let wet = (
            g(0x020, inp.0) + g(0x200, wl) + g(0x002, ext.0),
            g(0x010, inp.1) + g(0x100, wr) + g(0x001, ext.1),
        );
        let rv = self.reverb(core, wet);
        // Effect return at EVOL.
        (dry.0 + ((rv.0 * vreg(self, 0x4)) >> 15), dry.1 + ((rv.1 * vreg(self, 0x6)) >> 15))
    }

    /// Master volume (MVOL, slid once per sample) on a core's clamped output.
    fn master(&mut self, core: usize, o: (i32, i32)) -> (i32, i32) {
        self.mvol[core][0].update();
        self.mvol[core][1].update();
        let (ml, mr) = (self.mvol[core][0].value, self.mvol[core][1].value);
        ((clamp_mix(o.0) * ml) >> 15, (clamp_mix(o.1) * mr) >> 15)
    }

    /// One 48 kHz output sample (PCSX2 spu2Mix): core 0's output enters core 1 as its
    /// external input at core 1's AVOL (ExtVol).
    fn tick(&mut self) -> (i16, i16) {
        let c0 = self.core_tick(0, (0, 0));
        let c0 = self.master(0, c0);
        let base = 0x760 + 0x28;
        let (al, ar) = (self.regs[((base + 8) / 2) as usize] as i16 as i32, self.regs[((base + 10) / 2) as usize] as i16 as i32);
        let ext = ((c0.0 * al) >> 15, (c0.1 * ar) >> 15);
        let c1 = self.core_tick(1, ext);
        let (l, r) = self.master(1, c1);
        self.cycles = self.cycles.wrapping_add(1);
        (clamp_mix(l) as i16, clamp_mix(r) as i16)
    }
}

/// Generate the output samples due by `now` (48 kHz of device time).
pub fn update(m: &mut Machine, now: u64) {
    let start = *m.spu2.start.get_or_insert(now);
    let due = ((now - start) as u128 * SAMPLE_RATE as u128 / EE_HZ as u128) as u64;
    let mut n = due.saturating_sub(m.spu2.generated);
    if n > 4800 {
        m.spu2.generated = due - 4800;
        n = 4800;
    }
    if n == 0 {
        return;
    }
    let mut out = Vec::with_capacity(2 * n as usize);
    for _ in 0..n {
        let (l, r) = m.spu2.tick();
        out.push(l);
        out.push(r);
    }
    m.spu2.generated += n;
    wav_dump(&out);
    if std::mem::take(&mut m.spu2.irq) {
        m.iop.exec.pending_irq |= 1 << 9;
        iop_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let mut q = m.shared.audio.samples.lock().unwrap();
    q.extend(out);
    while q.len() > 2 * 48_000 {
        q.pop_front();
    }
}

/// `T5_WAVDUMP=<file.wav>`: record the SPU2 output (48 kHz stereo 16-bit) while playing.
fn wav_dump(out: &[i16]) {
    use std::io::{Seek, SeekFrom, Write};
    static FILE: std::sync::OnceLock<Option<Mutex<(std::fs::File, u32)>>> = std::sync::OnceLock::new();
    let Some(f) = FILE.get_or_init(|| {
        let path = std::env::var("T5_WAVDUMP").ok()?;
        let mut f = std::fs::File::create(path).ok()?;
        f.write_all(&[0u8; 44]).ok()?;
        Some(Mutex::new((f, 0)))
    }) else { return };
    let mut g = f.lock().unwrap();
    let bytes: Vec<u8> = out.iter().flat_map(|v| v.to_le_bytes()).collect();
    let _ = g.0.write_all(&bytes);
    g.1 += bytes.len() as u32;
    let len = g.1;
    // Keep the header valid so the file plays even if the game is closed abruptly.
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    h.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&len.to_le_bytes());
    let _ = g.0.seek(SeekFrom::Start(0)).and_then(|_| g.0.write_all(&h)).and_then(|_| g.0.seek(SeekFrom::End(0)));
}
