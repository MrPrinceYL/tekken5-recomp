//! The IPU: the MPEG-2 decoder the game's libmpeg drives for its movies.
//!
//! Commands (IPU_CMD bits 28..31): BCLR (0), IDEC (1), BDEC (2), VDEC (3), FDEC (4), SETIQ (5),
//! SETVQ (6), CSC (7), PACK (8), SETTH (9). The bitstream comes from DMA channel 4 (IPU_TO)
//! through the 8-qword input FIFO; results (BDEC: 384 RAW16 values per macroblock, CSC: RGB32 /
//! RGB16 pixels) go to the output FIFO, which DMA channel 3 (IPU_FROM) empties.
//!
//! The decoding follows ISO/IEC 13818-2 (VLC tables B-1, B-2..B-4, B-9..B-15, inverse
//! quantisation with mismatch control, IDCT), the colour conversion the EE manual gives for the
//! IPU (ITU-R BT.601 studio range). A command that runs out of input waits until DMA brings
//! more and is retried from its start (the stream is only consumed when a command completes).

use crate::{bus, diag, dmac, Machine};
use std::collections::VecDeque;
use std::sync::OnceLock;

pub struct Ipu {
    pub ctrl: u32,
    pub cmd_data: u32,
    pub busy: bool,
    /// The command waiting for input.
    pub pending: Option<u32>,
    /// Input FIFO: the 2-qword bit buffer (FP) and the 8-qword FIFO (IFC). IPU_TO DMA fills it
    /// only while there is room; the rest of the transfer stays in memory (MADR / QWC / TADR), as
    /// on the hardware (PCSX2 IPUdma.cpp), so sceIpuStopDMA / RestartDMA see the true state.
    pub fifo: VecDeque<u128>,
    /// IPU_TO in chain mode: the last tag (end / refe / IRQ with TIE) was read.
    pub to_end: bool,
    /// Reused input view buffer.
    scratch: Vec<u128>,
    /// Bit position in the front qword.
    pub bp: u32,
    pub out: VecDeque<u128>,
    pub iq_intra: [u8; 64],
    pub iq_non: [u8; 64],
    pub vqclut: [u16; 16],
    pub th0: u32,
    pub th1: u32,
    pub dc_pred: [i32; 3],
    /// IPU_FROM waiting for output.
    pub from_waiting: bool,
}

impl Default for Ipu {
    fn default() -> Ipu {
        Ipu {
            ctrl: 0,
            cmd_data: 0,
            busy: false,
            pending: None,
            fifo: VecDeque::new(),
            to_end: false,
            scratch: Vec::new(),
            bp: 0,
            out: VecDeque::new(),
            iq_intra: [16; 64],
            iq_non: [16; 64],
            vqclut: [0; 16],
            th0: 0,
            th1: 0,
            dc_pred: [128; 3],
            from_waiting: false,
        }
    }
}

// ---- the bitstream view ----

struct Bits<'a> {
    q: &'a [u128],
    pos: usize,
}

#[derive(Debug)]
struct NeedData;

impl<'a> Bits<'a> {
    fn avail(&self) -> usize {
        (self.q.len() * 128).saturating_sub(self.pos)
    }
    /// Stream byte `k` (bytes in memory order, each read MSB first), 0 past the end.
    #[inline(always)]
    fn byte(&self, k: usize) -> u64 {
        match self.q.get(k >> 4) {
            Some(q) => ((q >> (8 * (k & 15))) as u8) as u64,
            None => 0,
        }
    }
    /// The 64 bits from bit `pos` on, MSB first (zero-padded past the end).
    #[inline(always)]
    fn window(&self, pos: usize) -> u64 {
        let b = pos >> 3;
        let sh = (pos & 7) as u32;
        let mut v = 0u64;
        for k in 0..8 {
            v = (v << 8) | self.byte(b + k);
        }
        if sh != 0 {
            v = (v << sh) | (self.byte(b + 8) >> (8 - sh));
        }
        v
    }
    #[inline(always)]
    fn peek(&self, n: usize) -> Result<u32, NeedData> {
        if self.avail() < n {
            return Err(NeedData);
        }
        if n == 0 {
            return Ok(0);
        }
        Ok((self.window(self.pos) >> (64 - n)) as u32)
    }
    #[inline(always)]
    fn get(&mut self, n: usize) -> Result<u32, NeedData> {
        let v = self.peek(n)?;
        self.pos += n;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<(), NeedData> {
        if self.avail() < n {
            return Err(NeedData);
        }
        self.pos += n;
        Ok(())
    }
    /// Peek up to 16 bits (zero-padded past the end of the data).
    #[inline(always)]
    fn peek16(&self) -> u32 {
        (self.window(self.pos) >> 48) as u32
    }
}

// ---- VLC tables ----

/// A VLC table: for each 16-bit prefix, (value, code length), length 0 = invalid.
struct Vlc {
    t: Vec<(i32, u8)>,
}

impl Vlc {
    fn new(codes: &[(&str, i32)]) -> Vlc {
        let mut t = vec![(0i32, 0u8); 1 << 16];
        for &(code, val) in codes {
            let bits: String = code.chars().filter(|c| *c == '0' || *c == '1').collect();
            let len = bits.len();
            let v = u32::from_str_radix(&bits, 2).unwrap();
            let base = (v << (16 - len)) as usize;
            for i in 0..(1usize << (16 - len)) {
                t[base + i] = (val, len as u8);
            }
        }
        Vlc { t }
    }
    fn get(&self, b: &mut Bits) -> Result<i32, NeedData> {
        let (v, l) = self.t[b.peek16() as usize];
        if l == 0 {
            return Ok(i32::MIN);
        }
        b.skip(l as usize)?;
        Ok(v)
    }
}

struct Tables {
    mbai: Vlc,
    mbt: [Vlc; 4],
    mc: Vlc,
    dmv: Vlc,
    cbp: Vlc,
    dc_l: Vlc,
    dc_c: Vlc,
    /// DCT coefficients: value = run << 16 | level, -1 = EOB, -2 = escape.
    b14: Vlc,
    b14_first: Vlc,
    b15: Vlc,
}

const EOB: i32 = -1;
const ESC: i32 = -2;

fn rl(run: i32, level: i32) -> i32 {
    run << 16 | level
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mbai: Vec<(&str, i32)> = vec![
            ("1", 1), ("011", 2), ("010", 3), ("0011", 4), ("0010", 5), ("00011", 6), ("00010", 7),
            ("0000111", 8), ("0000110", 9), ("00001011", 10), ("00001010", 11), ("00001001", 12),
            ("00001000", 13), ("00000111", 14), ("00000110", 15), ("0000010111", 16), ("0000010110", 17),
            ("0000010101", 18), ("0000010100", 19), ("0000010011", 20), ("0000010010", 21),
            ("00000100011", 22), ("00000100010", 23), ("00000100001", 24), ("00000100000", 25),
            ("00000011111", 26), ("00000011110", 27), ("00000011101", 28), ("00000011100", 29),
            ("00000011011", 30), ("00000011010", 31), ("00000011001", 32), ("00000011000", 33),
            ("00000001111", 0x22), ("00000001000", 0x23),
        ];
        let mbt_i: Vec<(&str, i32)> = vec![("1", 0x01), ("01", 0x11)];
        let mbt_p: Vec<(&str, i32)> = vec![
            ("1", 0x0A), ("01", 0x02), ("001", 0x08), ("00011", 0x01), ("00010", 0x1A), ("00001", 0x12), ("000001", 0x11),
        ];
        let mbt_b: Vec<(&str, i32)> = vec![
            ("10", 0x0C), ("11", 0x0E), ("010", 0x04), ("011", 0x06), ("0010", 0x08), ("0011", 0x0A),
            ("00011", 0x01), ("00010", 0x1E), ("000011", 0x1A), ("000010", 0x16), ("000001", 0x11),
        ];
        let mbt_d: Vec<(&str, i32)> = vec![("1", 0x01)];
        let mcodes = [
            "1", "01", "001", "0001", "000011", "0000101", "0000100", "0000011", "000001011", "000001010",
            "000001001", "0000010001", "0000010000", "0000001111", "0000001110", "0000001101", "0000001100",
        ];
        let mut mc: Vec<(String, i32)> = vec![(mcodes[0].to_string(), 0)];
        for (i, c) in mcodes.iter().enumerate().skip(1) {
            mc.push((format!("{c}0"), i as i32));
            mc.push((format!("{c}1"), -(i as i32)));
        }
        let mc_ref: Vec<(&str, i32)> = mc.iter().map(|(a, b)| (a.as_str(), *b)).collect();
        let dmv: Vec<(&str, i32)> = vec![("0", 0), ("10", 1), ("11", -1)];
        let cbp: Vec<(&str, i32)> = vec![
            ("111", 60), ("1101", 4), ("1100", 8), ("1011", 16), ("1010", 32), ("10011", 12), ("10010", 48),
            ("10001", 20), ("10000", 40), ("01111", 28), ("01110", 44), ("01101", 52), ("01100", 56),
            ("01011", 1), ("01010", 61), ("01001", 2), ("01000", 62), ("001111", 24), ("001110", 36),
            ("001101", 3), ("001100", 63), ("0010111", 5), ("0010110", 9), ("0010101", 17), ("0010100", 33),
            ("0010011", 6), ("0010010", 10), ("0010001", 18), ("0010000", 34), ("00011111", 7), ("00011110", 11),
            ("00011101", 19), ("00011100", 35), ("00011011", 13), ("00011010", 49), ("00011001", 21),
            ("00011000", 41), ("00010111", 14), ("00010110", 50), ("00010101", 22), ("00010100", 42),
            ("00010011", 15), ("00010010", 51), ("00010001", 23), ("00010000", 43), ("00001111", 25),
            ("00001110", 37), ("00001101", 26), ("00001100", 38), ("00001011", 29), ("00001010", 45),
            ("00001001", 53), ("00001000", 57), ("00000111", 30), ("00000110", 46), ("00000101", 54),
            ("00000100", 58), ("000000111", 31), ("000000110", 47), ("000000101", 55), ("000000100", 59),
            ("000000011", 27), ("000000010", 39), ("000000001", 0),
        ];
        let dc_l: Vec<(&str, i32)> = vec![
            ("100", 0), ("00", 1), ("01", 2), ("101", 3), ("110", 4), ("1110", 5), ("11110", 6), ("111110", 7),
            ("1111110", 8), ("11111110", 9), ("111111110", 10), ("111111111", 11),
        ];
        let dc_c: Vec<(&str, i32)> = vec![
            ("00", 0), ("01", 1), ("10", 2), ("110", 3), ("1110", 4), ("11110", 5), ("111110", 6), ("1111110", 7),
            ("11111110", 8), ("111111110", 9), ("1111111110", 10), ("1111111111", 11),
        ];
        // Table B-14 (without the sign bit), long codes shared with B-15.
        let b14_common: Vec<(&str, i32)> = vec![
            ("011", rl(1, 1)), ("0100", rl(0, 2)), ("0101", rl(2, 1)), ("00101", rl(0, 3)), ("00111", rl(3, 1)),
            ("00110", rl(4, 1)), ("000110", rl(1, 2)), ("000111", rl(5, 1)), ("000101", rl(6, 1)),
            ("000100", rl(7, 1)), ("0000110", rl(0, 4)), ("0000100", rl(2, 2)), ("0000111", rl(8, 1)),
            ("0000101", rl(9, 1)), ("000001", ESC), ("00100110", rl(0, 5)), ("00100001", rl(0, 6)),
            ("00100101", rl(1, 3)), ("00100100", rl(3, 2)), ("00100111", rl(10, 1)), ("00100011", rl(11, 1)),
            ("00100010", rl(12, 1)), ("00100000", rl(13, 1)), ("0000001010", rl(0, 7)), ("0000001100", rl(1, 4)),
            ("0000001011", rl(2, 3)), ("0000001111", rl(4, 2)), ("0000001001", rl(5, 2)), ("0000001110", rl(14, 1)),
            ("0000001101", rl(15, 1)), ("0000001000", rl(16, 1)), ("000000011101", rl(0, 8)),
            ("000000011000", rl(0, 9)), ("000000010011", rl(0, 10)), ("000000010000", rl(0, 11)),
            ("000000011011", rl(1, 5)), ("000000010100", rl(2, 4)), ("000000011100", rl(3, 3)),
            ("000000010010", rl(4, 3)), ("000000011110", rl(6, 2)), ("000000010101", rl(7, 2)),
            ("000000010001", rl(8, 2)), ("000000011111", rl(17, 1)), ("000000011010", rl(18, 1)),
            ("000000011001", rl(19, 1)), ("000000010111", rl(20, 1)), ("000000010110", rl(21, 1)),
            ("0000000011010", rl(0, 12)), ("0000000011001", rl(0, 13)), ("0000000011000", rl(0, 14)),
            ("0000000010111", rl(0, 15)), ("0000000010110", rl(1, 6)), ("0000000010101", rl(1, 7)),
            ("0000000010100", rl(2, 5)), ("0000000010011", rl(3, 4)), ("0000000010010", rl(5, 3)),
            ("0000000010001", rl(9, 2)), ("0000000010000", rl(10, 2)), ("0000000011111", rl(22, 1)),
            ("0000000011110", rl(23, 1)), ("0000000011101", rl(24, 1)), ("0000000011100", rl(25, 1)),
            ("0000000011011", rl(26, 1)),
        ];
        let long: Vec<(&str, i32)> = vec![
            ("00000000011111", rl(0, 16)), ("00000000011110", rl(0, 17)), ("00000000011101", rl(0, 18)),
            ("00000000011100", rl(0, 19)), ("00000000011011", rl(0, 20)), ("00000000011010", rl(0, 21)),
            ("00000000011001", rl(0, 22)), ("00000000011000", rl(0, 23)), ("00000000010111", rl(0, 24)),
            ("00000000010110", rl(0, 25)), ("00000000010101", rl(0, 26)), ("00000000010100", rl(0, 27)),
            ("00000000010011", rl(0, 28)), ("00000000010010", rl(0, 29)), ("00000000010001", rl(0, 30)),
            ("00000000010000", rl(0, 31)), ("000000000011000", rl(0, 32)), ("000000000010111", rl(0, 33)),
            ("000000000010110", rl(0, 34)), ("000000000010101", rl(0, 35)), ("000000000010100", rl(0, 36)),
            ("000000000010011", rl(0, 37)), ("000000000010010", rl(0, 38)), ("000000000010001", rl(0, 39)),
            ("000000000010000", rl(0, 40)), ("000000000011111", rl(1, 8)), ("000000000011110", rl(1, 9)),
            ("000000000011101", rl(1, 10)), ("000000000011100", rl(1, 11)), ("000000000011011", rl(1, 12)),
            ("000000000011010", rl(1, 13)), ("000000000011001", rl(1, 14)), ("0000000000010011", rl(1, 15)),
            ("0000000000010010", rl(1, 16)), ("0000000000010001", rl(1, 17)), ("0000000000010000", rl(1, 18)),
            ("0000000000010100", rl(6, 3)), ("0000000000011010", rl(11, 2)), ("0000000000011001", rl(12, 2)),
            ("0000000000011000", rl(13, 2)), ("0000000000010111", rl(14, 2)), ("0000000000010110", rl(15, 2)),
            ("0000000000010101", rl(16, 2)), ("0000000000011111", rl(27, 1)), ("0000000000011110", rl(28, 1)),
            ("0000000000011101", rl(29, 1)), ("0000000000011100", rl(30, 1)), ("0000000000011011", rl(31, 1)),
        ];
        let mut b14: Vec<(&str, i32)> = vec![("10", EOB), ("11", rl(0, 1))];
        b14.extend(b14_common.iter().copied());
        b14.extend(long.iter().copied());
        let mut b14_first: Vec<(&str, i32)> = vec![("1", rl(0, 1))];
        b14_first.extend(b14_common.iter().copied());
        b14_first.extend(long.iter().copied());
        let mut b15: Vec<(&str, i32)> = vec![
            ("0110", EOB), ("10", rl(0, 1)), ("010", rl(1, 1)), ("110", rl(0, 2)), ("00101", rl(2, 1)),
            ("0111", rl(0, 3)), ("00111", rl(3, 1)), ("000110", rl(4, 1)), ("00110", rl(1, 2)), ("000111", rl(5, 1)),
            ("0000110", rl(6, 1)), ("0000100", rl(7, 1)), ("11100", rl(0, 4)), ("0000111", rl(2, 2)),
            ("0000101", rl(8, 1)), ("1111000", rl(9, 1)), ("000001", ESC), ("11101", rl(0, 5)), ("000101", rl(0, 6)),
            ("1111001", rl(1, 3)), ("00100110", rl(3, 2)), ("1111010", rl(10, 1)), ("00100001", rl(11, 1)),
            ("00100101", rl(12, 1)), ("00100100", rl(13, 1)), ("000100", rl(0, 7)), ("00100111", rl(1, 4)),
            ("11111100", rl(2, 3)), ("11111101", rl(4, 2)), ("000000100", rl(5, 2)), ("000000101", rl(14, 1)),
            ("000000111", rl(15, 1)), ("0000001101", rl(16, 1)), ("1111011", rl(0, 8)), ("1111100", rl(0, 9)),
            ("00100011", rl(0, 10)), ("00100010", rl(0, 11)), ("00100000", rl(1, 5)), ("0000001100", rl(2, 4)),
            ("000000011100", rl(3, 3)), ("000000010010", rl(4, 3)), ("000000011110", rl(6, 2)),
            ("000000010101", rl(7, 2)), ("000000010001", rl(8, 2)), ("000000011111", rl(17, 1)),
            ("000000011010", rl(18, 1)), ("000000011001", rl(19, 1)), ("000000010111", rl(20, 1)),
            ("000000010110", rl(21, 1)), ("11111010", rl(0, 12)), ("11111011", rl(0, 13)), ("11111110", rl(0, 14)),
            ("11111111", rl(0, 15)), ("0000000010110", rl(1, 6)), ("0000000010101", rl(1, 7)),
            ("0000000010100", rl(2, 5)), ("0000000010011", rl(3, 4)), ("0000000010010", rl(5, 3)),
            ("0000000010001", rl(9, 2)), ("0000000010000", rl(10, 2)), ("0000000011111", rl(22, 1)),
            ("0000000011110", rl(23, 1)), ("0000000011101", rl(24, 1)), ("0000000011100", rl(25, 1)),
            ("0000000011011", rl(26, 1)),
        ];
        b15.extend(long.iter().copied());
        Tables {
            mbai: Vlc::new(&mbai),
            mbt: [Vlc::new(&mbt_i), Vlc::new(&mbt_p), Vlc::new(&mbt_b), Vlc::new(&mbt_d)],
            mc: Vlc::new(&mc_ref),
            dmv: Vlc::new(&dmv),
            cbp: Vlc::new(&cbp),
            dc_l: Vlc::new(&dc_l),
            dc_c: Vlc::new(&dc_c),
            b14: Vlc::new(&b14),
            b14_first: Vlc::new(&b14_first),
            b15: Vlc::new(&b15),
        }
    })
}

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55,
    62, 63,
];
const ALTERNATE: [usize; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19, 27, 34, 42, 50, 58, 35, 43,
    51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53, 61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47,
    55, 63,
];
const NONLINEAR_Q: [i32; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 104, 112,
];

fn idct_basis() -> &'static [[f64; 8]; 8] {
    static B: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    B.get_or_init(|| {
        let mut b = [[0f64; 8]; 8];
        for (x, row) in b.iter_mut().enumerate() {
            for (u, v) in row.iter_mut().enumerate() {
                let c = if u == 0 { (0.5f64).sqrt() } else { 1.0 };
                *v = c * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos() / 2.0;
            }
        }
        b
    })
}

/// Separable double-precision 8x8 inverse DCT (IEEE 1180 accurate), rounded.
fn idct(blk: &[i32; 64]) -> [i32; 64] {
    let b = idct_basis();
    let mut tmp = [0f64; 64];
    for v in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for u in 0..8 {
                s += b[x][u] * blk[v * 8 + u] as f64;
            }
            tmp[v * 8 + x] = s;
        }
    }
    let mut out = [0i32; 64];
    for x in 0..8 {
        for y in 0..8 {
            let mut s = 0.0;
            for v in 0..8 {
                s += b[y][v] * tmp[v * 8 + x];
            }
            out[y * 8 + x] = s.round() as i32;
        }
    }
    out
}

impl Ipu {
    fn idp(&self) -> u32 {
        (self.ctrl >> 16) & 3
    }

    /// Decode one block's coefficients (natural order, dequantised).
    fn block(&mut self, b: &mut Bits, intra: bool, cc: usize, qsc: u32) -> Result<[i32; 64], NeedData> {
        let t = tables();
        let mpeg1 = self.ctrl & (1 << 23) != 0;
        let scan = if self.ctrl & (1 << 20) != 0 { &ALTERNATE } else { &ZIGZAG };
        let q = if self.ctrl & (1 << 22) != 0 { NONLINEAR_Q[(qsc & 31) as usize] } else { 2 * (qsc & 31) as i32 };
        let q = if mpeg1 { (qsc & 31) as i32 } else { q };
        let mut c = [0i32; 64];
        let mut i: usize;
        if intra {
            let size = if cc == 0 { t.dc_l.get(b)? } else { t.dc_c.get(b)? };
            let diff = if size <= 0 {
                0
            } else {
                let d = b.get(size as usize)? as i32;
                if d < (1 << (size - 1)) { d - (1 << size) + 1 } else { d }
            };
            self.dc_pred[cc] += diff;
            c[0] = self.dc_pred[cc] << (3 - self.idp().min(3));
            i = 1;
        } else {
            i = 0;
        }
        let table = if intra && self.ctrl & (1 << 21) != 0 { &t.b15 } else { &t.b14 };
        let iq = if intra { self.iq_intra } else { self.iq_non };
        let mut first = !intra;
        let mut sum = c[0];
        loop {
            let v = if first { t.b14_first.get(b)? } else { table.get(b)? };
            first = false;
            if v == EOB {
                break;
            }
            let (run, mut level);
            if v == ESC {
                run = b.get(6)? as i32;
                if mpeg1 {
                    let l = b.get(8)? as i32;
                    level = match l {
                        0 => b.get(8)? as i32,
                        128 => b.get(8)? as i32 - 256,
                        _ => (l as i8) as i32,
                    };
                } else {
                    level = ((b.get(12)? as i32) << 20) >> 20;
                }
            } else if v == i32::MIN {
                // An invalid code: stop this block (the stream is broken).
                break;
            } else {
                run = v >> 16;
                level = v & 0xFFFF;
                if b.get(1)? == 1 {
                    level = -level;
                }
            }
            i += run as usize;
            if i > 63 {
                break;
            }
            let j = scan[i];
            let w = iq[j] as i32;
            let mut val = if intra {
                if mpeg1 { (level * q * w) >> 3 } else { (level * q * w) >> 4 }
            } else {
                let s = if level > 0 { 1 } else { -1 };
                if mpeg1 { ((2 * level + s) * q * w) >> 4 } else { ((2 * level + s) * q * w) / 32 }
            };
            if mpeg1 && val != 0 && val & 1 == 0 {
                val -= val.signum();
            }
            val = val.clamp(-2048, 2047);
            c[j] = val;
            sum += val;
            i += 1;
        }
        if !mpeg1 && sum & 1 == 0 {
            c[63] ^= 1;
        }
        Ok(c)
    }

    /// BDEC: one macroblock into 384 RAW16 values.
    fn bdec(&mut self, b: &mut Bits, cmd: u32) -> Result<Vec<i16>, NeedData> {
        let fb = (cmd & 0x3F) as usize;
        let qsc = (cmd >> 16) & 31;
        let dt = cmd & (1 << 25) != 0;
        let dcr = cmd & (1 << 26) != 0;
        let intra = cmd & (1 << 27) != 0;
        b.skip(fb)?;
        let saved_pred = self.dc_pred;
        if dcr {
            let r = 128 << self.idp();
            self.dc_pred = [r, r, r];
        }
        let cbp = if intra { 0x3F } else { tables().cbp.get(b)?.max(0) as u32 };
        let mut out = vec![0i16; 384];
        let res = (|| -> Result<(), NeedData> {
            for blk in 0..6 {
                if cbp & (0x20 >> blk) == 0 {
                    continue;
                }
                let cc = if blk < 4 { 0 } else { blk - 3 };
                let coefs = self.block(b, intra, cc, qsc)?;
                let pix = idct(&coefs);
                for y in 0..8 {
                    for x in 0..8 {
                        let v = pix[y * 8 + x];
                        let v = if intra { v.clamp(0, 255) } else { v.clamp(-256, 255) };
                        let (o, stride) = if blk < 4 {
                            let bx = (blk & 1) * 8;
                            let row = if dt { (blk >> 1) + 2 * y } else { (blk >> 1) * 8 + y };
                            (row * 16 + bx + x, 0)
                        } else {
                            (256 + (blk - 4) * 64 + y * 8 + x, 0)
                        };
                        let _ = stride;
                        out[o] = v as i16;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            self.dc_pred = saved_pred;
            return Err(e);
        }
        self.ctrl = (self.ctrl & !(0x3F << 8)) | (cbp << 8);
        Ok(out)
    }

    fn rgb(&self, y: i32, cb: i32, cr: i32) -> (u8, u8, u8, u8) {
        let yy = 1.164 * (y - 16) as f64;
        let (cb, cr) = ((cb - 128) as f64, (cr - 128) as f64);
        let r = (yy + 1.596 * cr).round().clamp(0.0, 255.0) as u32;
        let g = (yy - 0.813 * cr - 0.391 * cb).round().clamp(0.0, 255.0) as u32;
        let bl = (yy + 2.018 * cb).round().clamp(0.0, 255.0) as u32;
        let a = if r < self.th0 && g < self.th0 && bl < self.th0 {
            0
        } else if r < self.th1 && g < self.th1 && bl < self.th1 {
            0x40
        } else {
            0x80
        };
        if a == 0 { (0, 0, 0, 0) } else { (r as u8, g as u8, bl as u8, a) }
    }

    /// CSC: `count` RAW8 macroblocks (384 bytes each, read raw from the input) to RGB32 / RGB16.
    fn csc(&mut self, b: &mut Bits, cmd: u32) -> Result<Vec<u128>, NeedData> {
        let count = (cmd & 0x7FF) as usize;
        let rgb16 = cmd & (1 << 27) != 0;
        let mut out = Vec::new();
        for _ in 0..count {
            let mut raw = [0u8; 384];
            for v in raw.iter_mut() {
                *v = b.get(8)? as u8;
            }
            let mut px = Vec::with_capacity(256);
            for y in 0..16 {
                for x in 0..16 {
                    let c = (y / 2) * 8 + x / 2;
                    px.push(self.rgb(raw[y * 16 + x] as i32, raw[256 + c] as i32, raw[320 + c] as i32));
                }
            }
            if rgb16 {
                let mut bytes = Vec::with_capacity(512);
                for (r, g, bl, a) in px {
                    let v = (r as u16 >> 3) | ((g as u16 >> 3) << 5) | ((bl as u16 >> 3) << 10) | (((a != 0) as u16) << 15);
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                out.extend(bytes.chunks(16).map(|c| u128::from_le_bytes(c.try_into().unwrap())));
            } else {
                let bytes: Vec<u8> = px.iter().flat_map(|&(r, g, bl, a)| [r, g, bl, a]).collect();
                out.extend(bytes.chunks(16).map(|c| u128::from_le_bytes(c.try_into().unwrap())));
            }
        }
        Ok(out)
    }

    /// Try to run command `cmd` on the input `q` (FIFO followed by the DMA data still in memory);
    /// Ok(qwords consumed), Err if it needs more input. Sets BP; the caller commits the input.
    fn exec(&mut self, cmd: u32, q: &[u128]) -> Result<usize, NeedData> {
        let op = cmd >> 28;
        let t = tables();
        if op == 0 {
            // BCLR: clear the input FIFO (the rest of the DMA transfer stays), set BP.
            self.fifo.clear();
            self.bp = cmd & 0x7F;
            return Ok(0);
        }
        if op == 9 {
            self.th0 = cmd & 0x1FF;
            self.th1 = (cmd >> 16) & 0x1FF;
            return Ok(0);
        }
        // Run on a view of the stream; the caller commits the consumed bits.
        let mut b = Bits { q: &q, pos: self.bp as usize };
        let mut result_out: Vec<u128> = Vec::new();
        match op {
            2 => {
                let raw = self.bdec(&mut b, cmd)?;
                let bytes: Vec<u8> = raw.iter().flat_map(|v| v.to_le_bytes()).collect();
                result_out = bytes.chunks(16).map(|c| u128::from_le_bytes(c.try_into().unwrap())).collect();
            }
            3 => {
                b.skip((cmd & 0x3F) as usize)?;
                let start = b.pos;
                let v = match (cmd >> 26) & 3 {
                    0 => t.mbai.get(&mut b)?,
                    1 => {
                        let pct = ((self.ctrl >> 24) & 7).clamp(1, 4) as usize;
                        t.mbt[pct - 1].get(&mut b)?
                    }
                    2 => t.mc.get(&mut b)?,
                    _ => t.dmv.get(&mut b)?,
                };
                let len = (b.pos - start) as u32;
                self.cmd_data = (v as u32 & 0xFFFF) | (len << 16);
            }
            4 => {
                b.skip((cmd & 0x3F) as usize)?;
                self.cmd_data = b.peek(32)?;
                if crate::diag::enabled("T5_IPULOG") {
                    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 300 {
                        diag::log(format!("FDEC {cmd:#010x} bp {} -> {:#010x} (fifo {})", self.bp, self.cmd_data, self.fifo.len()));
                    }
                }
            }
            5 | 6 => {
                b.skip((cmd & 0x3F) as usize)?;
                if op == 5 {
                    let mut m = [0u8; 64];
                    for i in 0..64 {
                        m[ZIGZAG[i]] = b.get(8)? as u8;
                    }
                    if cmd & (1 << 27) != 0 { self.iq_non = m } else { self.iq_intra = m }
                } else {
                    for i in 0..16 {
                        let lo = b.get(8)?;
                        let hi = b.get(8)?;
                        self.vqclut[i] = (lo | hi << 8) as u16;
                    }
                }
            }
            7 => {
                result_out = self.csc(&mut b, cmd)?;
            }
            1 | 8 => {
                diag::once(format!("IPU: command {op} not implemented ({cmd:#010x})"));
                return Ok(0);
            }
            _ => return Ok(0),
        }
        let b_pos = b.pos;
        drop(b);
        self.bp = (b_pos % 128) as u32;
        self.out.extend(result_out);
        Ok(b_pos / 128)
    }
}

/// FIFO capacity: 8 qwords plus the 2-qword bit buffer.
const FIFO_QW: usize = 10;

/// A walk over the IPU_TO channel's remaining data (normal or source chain mode).
#[derive(Clone, Copy)]
struct ToCursor {
    madr: u32,
    qwc: u32,
    tadr: u32,
    chain: bool,
    tie: bool,
    end: bool,
}

impl ToCursor {
    /// The running transfer, if any.
    fn of(m: &Machine) -> Option<ToCursor> {
        let ch = &m.dmac.ch[dmac::IPU_TO];
        if ch.chcr & 0x100 == 0 {
            return None;
        }
        Some(ToCursor { madr: ch.madr, qwc: ch.qwc, tadr: ch.tadr, chain: (ch.chcr >> 2) & 3 == 1, tie: ch.chcr & 0x80 != 0, end: m.ipu.to_end })
    }

    fn next(&mut self, m: &mut Machine) -> Option<u128> {
        loop {
            if self.qwc > 0 {
                let v = bus::qword(m, self.madr, self.madr & 0x8000_0000 != 0);
                self.madr += 16;
                self.qwc -= 1;
                return Some(v);
            }
            if !self.chain || self.end {
                return None;
            }
            let tag = bus::qword(m, self.tadr, self.tadr & 0x8000_0000 != 0) as u64;
            let qwc = (tag & 0xFFFF) as u32;
            let addr = ((tag >> 32) as u32) & 0xFFFF_FFF0;
            match (tag >> 28) & 7 {
                0 => {
                    // refe
                    self.madr = addr;
                    self.tadr += 16;
                    self.end = true;
                }
                2 => {
                    // next
                    self.madr = self.tadr + 16;
                    self.tadr = addr;
                }
                3 | 4 => {
                    // ref / refs
                    self.madr = addr;
                    self.tadr += 16;
                }
                7 => {
                    // end
                    self.madr = self.tadr + 16;
                    self.tadr = self.madr + 16 * qwc;
                    self.end = true;
                }
                id => {
                    // cnt (call / ret are not used with IPU_TO; treated as cnt)
                    if id != 1 {
                        diag::once(format!("IPU_TO: chain tag id {id} treated as cnt"));
                    }
                    self.madr = self.tadr + 16;
                    self.tadr = self.madr + 16 * qwc;
                }
            }
            if self.tie && tag >> 31 & 1 != 0 {
                self.end = true;
            }
            self.qwc = qwc;
        }
    }

    fn store(&self, m: &mut Machine) {
        let ch = &mut m.dmac.ch[dmac::IPU_TO];
        ch.madr = self.madr;
        ch.qwc = self.qwc;
        ch.tadr = self.tadr;
        m.ipu.to_end = self.end;
    }

    fn done(&self) -> bool {
        self.qwc == 0 && (!self.chain || self.end)
    }
}

/// Let IPU_TO fill the FIFO; the transfer finishes when all its data has entered it.
fn refill(m: &mut Machine) {
    let Some(mut c) = ToCursor::of(m) else { return };
    while m.ipu.fifo.len() < FIFO_QW {
        match c.next(m) {
            Some(v) => m.ipu.fifo.push_back(v),
            None => break,
        }
    }
    c.store(m);
    if c.done() {
        dmac::finish(m, dmac::IPU_TO);
    }
}

/// The input stream: the FIFO followed by up to `look` qwords of the transfer still in memory.
fn stream(m: &mut Machine, look: usize) -> Vec<u128> {
    let mut q = Vec::new();
    stream_into(m, look, &mut q);
    q
}

/// `stream` into a reused buffer.
fn stream_into(m: &mut Machine, look: usize, q: &mut Vec<u128>) {
    q.clear();
    q.extend(m.ipu.fifo.iter().copied());
    if let Some(mut c) = ToCursor::of(m) {
        for _ in 0..look {
            match c.next(m) {
                Some(v) => q.push(v),
                None => break,
            }
        }
    }
}

/// Consume `n` qwords of input: from the FIFO first, then from the transfer.
fn commit(m: &mut Machine, n: usize) {
    let from_fifo = n.min(m.ipu.fifo.len());
    m.ipu.fifo.drain(..from_fifo);
    if n > from_fifo {
        if let Some(mut c) = ToCursor::of(m) {
            for _ in from_fifo..n {
                c.next(m);
            }
            c.store(m);
        }
    }
    refill(m);
}

/// CTRL.SCD: a start code follows at the next byte boundary.
fn update_scd(m: &mut Machine) {
    let q = stream(m, 4);
    let byte_pos = (m.ipu.bp as usize).div_ceil(8) * 8;
    let b = Bits { q: &q, pos: byte_pos };
    let scd = matches!(b.peek(24), Ok(1));
    m.ipu.ctrl = (m.ipu.ctrl & !(1 << 15)) | ((scd as u32) << 15);
}

pub fn read(m: &mut Machine, a: u32) -> u64 {
    let top = if a & 0xF0 == 0x30 { Some(stream(m, 4)) } else { None };
    let ipu = &m.ipu;
    match a & 0xF0 {
        0x00 => ipu.cmd_data as u64 | ((ipu.pending.is_some() as u64) << 63),
        0x10 => {
            let ifc = ipu.fifo.len().saturating_sub(2).min(8) as u32;
            let ofc = ipu.out.len().min(8) as u32;
            (ipu.ctrl & !0xFF & !(1 << 31)) as u64 | ifc as u64 | (ofc << 4) as u64 | ((ipu.pending.is_some() as u64) << 31)
        }
        0x20 => {
            let fp = ipu.fifo.len().min(2) as u32;
            let ifc = ipu.fifo.len().saturating_sub(2).min(8) as u32;
            (ipu.bp | ifc << 8 | fp << 16) as u64
        }
        0x30 => {
            let q = top.unwrap();
            let b = Bits { q: &q, pos: ipu.bp as usize };
            match b.peek(32) {
                Ok(v) => v as u64,
                Err(_) => 1 << 63,
            }
        }
        _ => 0,
    }
}

pub fn write(m: &mut Machine, a: u32, v: u64) {
    match a & 0xF0 {
        0x00 => {
            let cmd = v as u32;
            diag::count_with(|| format!("ipu_op{}_fn{:x}", cmd >> 28, ee_core::LAST_FN.load(std::sync::atomic::Ordering::Relaxed)));
            m.ipu.pending = Some(cmd);
            run_pending(m);
        }
        0x10 => {
            if v & (1 << 30) != 0 {
                let keep = (m.ipu.iq_intra, m.ipu.iq_non);
                m.ipu = Ipu::default();
                m.ipu.iq_intra = keep.0;
                m.ipu.iq_non = keep.1;
                return;
            }
            let wmask = (3 << 16) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23) | (7 << 24);
            m.ipu.ctrl = (m.ipu.ctrl & !wmask) | (v as u32 & wmask);
        }
        _ => {}
    }
}

fn run_pending(m: &mut Machine) {
    refill(m);
    if let Some(cmd) = m.ipu.pending {
        // Look ahead into the transfer as far as the command is likely to need (CSC: 24 qwords
        // per macroblock; a BDEC macroblock rarely exceeds 64), more if that is not enough.
        let mut look = match cmd >> 28 {
            7 => (cmd & 0x7FF) as usize * 24 + 4,
            2 | 1 => 64,
            _ => 8,
        };
        let mut q = std::mem::take(&mut m.ipu.scratch);
        loop {
            stream_into(m, look, &mut q);
            match m.ipu.exec(cmd, &q) {
                Ok(n) => {
                    m.ipu.pending = None;
                    commit(m, n);
                    update_scd(m);
                    break;
                }
                Err(NeedData) => {
                    if q.len() < m.ipu.fifo.len() + look || look >= 1 << 17 {
                        break; // waits for more input
                    }
                    look *= 4;
                }
            }
        }
        m.ipu.scratch = q;
    }
    // Output for a waiting IPU_FROM transfer.
    if m.ipu.from_waiting {
        dma_from(m);
    }
}

pub fn fifo_read(m: &mut Machine, _a: u32) -> u128 {
    m.ipu.out.pop_front().unwrap_or(0)
}

pub fn fifo_write(m: &mut Machine, v: u128) {
    m.ipu.fifo.push_back(v);
    run_pending(m);
}

/// IPU_TO started: data flows into the FIFO as there is room.
pub fn dma_start(m: &mut Machine) {
    m.ipu.to_end = false;
    if crate::diag::enabled("T5_IPULOG") {
        let ch = &m.dmac.ch[dmac::IPU_TO];
        diag::log(format!("IPU_TO start chcr {:#x} madr {:#x} qwc {} tadr {:#x}", ch.chcr, ch.madr, ch.qwc, ch.tadr));
    }
    run_pending(m);
}

/// IPU_FROM: move output qwords to memory; finishes when QWC is done, else waits for output.
pub fn dma_from(m: &mut Machine) {
    let ch = dmac::IPU_FROM;
    if m.dmac.ch[ch].chcr & 0x100 == 0 {
        m.ipu.from_waiting = false;
        return;
    }
    while m.dmac.ch[ch].qwc > 0 {
        let Some(q) = m.ipu.out.pop_front() else { break };
        let madr = m.dmac.ch[ch].madr;
        bus::set_qword(m, madr, madr & 0x8000_0000 != 0, q);
        m.dmac.ch[ch].madr = madr + 16;
        m.dmac.ch[ch].qwc -= 1;
    }
    if m.dmac.ch[ch].qwc == 0 {
        m.ipu.from_waiting = false;
        dmac::finish(m, ch);
    } else {
        m.ipu.from_waiting = true;
    }
}
