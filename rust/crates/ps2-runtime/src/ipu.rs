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
use std::sync::{Mutex, OnceLock};

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
    /// Quantiser matrices as transmitted (zigzag order), applied by scan position like the
    /// hardware (PCSX2 IPU_MultiISA quant_matrix[i]), also for the alternate scan.
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
        // peek16 pads with zeros past the data the IPU has: a code cut there decodes as
        // another or none. Wait for input then (the hardware stalls), as for any other read.
        if (l == 0 || l as usize > b.avail()) && b.avail() < 16 {
            return Err(NeedData);
        }
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
            ("0000001101", rl(15, 1)), ("0000001000", rl(16, 1)), ("000000011100", rl(3, 3)),
            ("000000010010", rl(4, 3)), ("000000011110", rl(6, 2)), ("000000010101", rl(7, 2)),
            ("000000010001", rl(8, 2)), ("000000011111", rl(17, 1)), ("000000011010", rl(18, 1)),
            ("000000011001", rl(19, 1)), ("000000010111", rl(20, 1)), ("000000010110", rl(21, 1)),
            ("0000000010110", rl(1, 6)), ("0000000010101", rl(1, 7)),
            ("0000000010100", rl(2, 5)), ("0000000010011", rl(3, 4)), ("0000000010010", rl(5, 3)),
            ("0000000010001", rl(9, 2)), ("0000000010000", rl(10, 2)), ("0000000011111", rl(22, 1)),
            ("0000000011110", rl(23, 1)), ("0000000011101", rl(24, 1)), ("0000000011100", rl(25, 1)),
            ("0000000011011", rl(26, 1)),
        ];
        let long: Vec<(&str, i32)> = vec![
            // Shared with B-15 (libmpeg2 / PCSX2 tab2..tab3).
            ("0000000010111", rl(0, 15)), ("0000000011000", rl(0, 14)), ("0000000011001", rl(0, 13)), ("0000000011010", rl(0, 12)), ("000000010000", rl(0, 11)), ("000000010011", rl(0, 10)), ("000000010100", rl(2, 4)), ("000000011000", rl(0, 9)), ("000000011011", rl(1, 5)), ("000000011101", rl(0, 8)),
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

/// PCSX2 IDCT_Block (mpeg2dec integer IDCT, row pass into 16 bits, then columns), on a block
/// in natural order; the result is spatial, natural order.
fn idct(blk: &[i32; 64]) -> [i32; 64] {
    const W1: i32 = 2841;
    const W2: i32 = 2676;
    const W3: i32 = 2408;
    const W5: i32 = 1609;
    const W6: i32 = 1108;
    const W7: i32 = 565;
    // Coefficient order of PCSX2's packed scan within a row / column.
    const P: [usize; 8] = [0, 2, 4, 6, 1, 3, 5, 7];
    fn bf(w0: i32, w1: i32, d0: i32, d1: i32) -> (i32, i32) {
        let tmp = w0 * (d0 + d1);
        (tmp + (w1 - w0) * d1, tmp - (w1 + w0) * d0)
    }
    let mut t = [0i16; 64];
    for r in 0..8 {
        let g = |k: usize| blk[r * 8 + P[k]];
        let (d0, d2) = ((g(0) << 11) + 128, g(2) << 11);
        let (t0, t1) = (d0 + d2, d0 - d2);
        let (t2, t3) = bf(W6, W2, g(3), g(1));
        let a = [t0 + t2, t1 + t3, t1 - t3, t0 - t2];
        let (t0, t1) = bf(W7, W1, g(7), g(4));
        let (t2, t3) = bf(W3, W5, g(5), g(6));
        let (b0, b3) = (t0 + t2, t1 + t3);
        let (t0, t1) = (t0 - t2, t1 - t3);
        let b = [b0, ((t0 + t1) * 181) >> 8, ((t0 - t1) * 181) >> 8, b3];
        for k in 0..4 {
            t[r * 8 + k] = ((a[k] + b[k]) >> 8) as i16;
            t[r * 8 + 7 - k] = ((a[k] - b[k]) >> 8) as i16;
        }
    }
    let mut out = [0i32; 64];
    for x in 0..8 {
        let g = |k: usize| t[P[k] * 8 + x] as i32;
        let (d0, d2) = ((g(0) << 11) + 65536, g(2) << 11);
        let (t0, t1) = (d0 + d2, d0 - d2);
        let (t2, t3) = bf(W6, W2, g(3), g(1));
        let a = [t0 + t2, t1 + t3, t1 - t3, t0 - t2];
        let (t0, t1) = bf(W7, W1, g(7), g(4));
        let (t2, t3) = bf(W3, W5, g(5), g(6));
        let (b0, b3) = (t0 + t2, t1 + t3);
        let (t0, t1) = ((t0 - t2) >> 8, (t1 - t3) >> 8);
        let b = [b0, (t0 + t1) * 181, (t0 - t1) * 181, b3];
        for k in 0..4 {
            out[k * 8 + x] = ((a[k] + b[k]) >> 17) as i16 as i32;
            out[(7 - k) * 8 + x] = ((a[k] - b[k]) >> 17) as i16 as i32;
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
        loop {
            let v = if first { t.b14_first.get(b)? } else { table.get(b)? };
            first = false;
            if v == EOB {
                break;
            }
            // PCSX2 get_intra_block / get_non_intra_block: table codes are scaled by magnitude
            // and the sign applied afterwards (rounds towards 0); escapes are scaled signed and
            // shifted (rounds down).
            let (run, level, neg);
            if v == ESC {
                neg = false;
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
                neg = b.get(1)? == 1;
            }
            i += run as usize;
            if i > 63 {
                break;
            }
            let j = scan[i];
            let w = iq[i] as i32;
            let mut val = if intra {
                if mpeg1 { (level * q * w) >> 3 } else { (level * q * w) >> 4 }
            } else {
                let s = if level >= 0 { 1 } else { -1 };
                if mpeg1 { ((2 * level + s) * q * w) >> 4 } else { ((2 * level + s) * q * w) >> 5 }
            };
            if mpeg1 && val != 0 && val & 1 == 0 {
                val -= val.signum();
            }
            if neg {
                val = -val;
            }
            val = val.clamp(-2048, 2047);
            c[j] = val;
            i += 1;
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
                        let v = if intra { v.clamp(0, 255) } else { v };
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

    /// The IPU's integer YCbCr -> RGB (PCSX2 yuv2rgb_reference, "the precise documented integer
    /// method used by the hardware") and the TH0 / TH1 alpha thresholds (PCSX2 ipu_csc).
    fn rgb(&self, y: i32, cb: i32, cr: i32) -> (u8, u8, u8, u8) {
        let lum = (0x95 * (y - 16).max(0)) >> 6;
        let (cb, cr) = (cb - 128, cr - 128);
        let r = ((lum + ((0xCC * cr) >> 6) + 1) >> 1).clamp(0, 255) as u32;
        let g = ((lum + ((-0x68 * cr) >> 6) + ((-0x32 * cb) >> 6) + 1) >> 1).clamp(0, 255) as u32;
        let bl = ((lum + ((0x102 * cb) >> 6) + 1) >> 1).clamp(0, 255) as u32;
        let a = if r < self.th0 && g < self.th0 && bl < self.th0 {
            0
        } else if r < self.th1 && g < self.th1 && bl < self.th1 {
            0x40
        } else {
            0x80
        };
        if a == 0 { (0, 0, 0, 0) } else { (r as u8, g as u8, bl as u8, a) }
    }

    /// CSC: `count` RAW8 macroblocks (384 bytes each, read raw from the input) to RGB32 / RGB16
    /// (OFM, dithered with DTE: PCSX2 ipu_dither_reference).
    fn csc(&mut self, b: &mut Bits, cmd: u32) -> Result<Vec<u128>, NeedData> {
        const DITHER: [[i32; 4]; 4] = [[-4, 0, -3, 1], [2, -2, 3, -1], [-3, 1, -4, 0], [3, -1, 2, -2]];
        let count = (cmd & 0x7FF) as usize;
        let dte = cmd & (1 << 26) != 0;
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
                for (k, (r, g, bl, a)) in px.into_iter().enumerate() {
                    let d = if dte { DITHER[(k / 16) & 3][k & 3] } else { 0 };
                    let q = |c: u8| ((c as i32 + d).clamp(0, 255) >> 3) as u16;
                    let v = q(r) | (q(g) << 5) | (q(bl) << 10) | (((a == 0x40) as u16) << 15);
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
            trace(cmd, 0, &[], "");
            return Ok(0);
        }
        if op == 9 {
            self.th0 = cmd & 0x1FF;
            self.th1 = (cmd >> 16) & 0x1FF;
            return Ok(0);
        }
        // T5_IPUTRACE: the state a BDEC starts from (modes, DC predictors, matrices, BP).
        let pre = (op == 2 && trace_on()).then(|| {
            let h = |m: &[u8; 64]| m.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
            format!(" ctrl {:08x} dc {:?} iq {:016x} {:016x} bp {}", self.ctrl & 0x07F3_0000, self.dc_pred, h(&self.iq_intra), h(&self.iq_non), self.bp)
        });
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
                // DATA as PCSX2 ipuVDEC: value | code length << 16, the value sign-extended over
                // the whole word (motion codes, DMV); macroblock types of I / P pictures without
                // the length, P / B with MC_FRAME (frame_pred_frame_dct = 1, frame pictures).
                let start = b.pos;
                let tbl = (cmd >> 26) & 3;
                let pct = ((self.ctrl >> 24) & 7).clamp(1, 4);
                let mpeg1 = self.ctrl & (1 << 23) != 0;
                let v = match tbl {
                    0 => t.mbai.get(&mut b)?,
                    1 => t.mbt[pct as usize - 1].get(&mut b)?,
                    2 => t.mc.get(&mut b)?,
                    _ => t.dmv.get(&mut b)?,
                };
                let len = (b.pos - start) as i32;
                let data = if v == i32::MIN || (tbl == 0 && v == 0x22 && !mpeg1) {
                    // Invalid code (or MPEG-1 stuffing in an MPEG-2 stream): an error, nothing used.
                    b.pos = start;
                    0
                } else {
                    match tbl {
                        1 => match pct {
                            1 => v,
                            2 => v | if v & 8 != 0 { 0x80 } else { 0 },
                            3 => v | 0x80 | len << 16,
                            _ => v | len << 16,
                        },
                        // The sign bit is not counted.
                        2 if v != 0 => v | (len - 1) << 16,
                        _ => v | len << 16,
                    }
                };
                self.cmd_data = data as u32;
                self.ctrl = (self.ctrl & !(1 << 14)) | ((data == 0) as u32) << 14;
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
                    for v in m.iter_mut() {
                        *v = b.get(8)? as u8;
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
        if op == 2 || op == 3 {
            trace(cmd, self.cmd_data, &result_out, pre.as_deref().unwrap_or(""));
        }
        self.out.extend(result_out);
        Ok(b_pos / 128)
    }
}

/// `T5_IPUDUMP=<file>`: every qword the decoding commands consume, in order (the stream the
/// game feeds the IPU), for decoding the same data in `tests::mpeg_stream_sync` (T5_MPEG_ES).
/// `T5_IPUCSC=<file>`: the CSC input (the pictures libmpeg reconstructed, RAW8 macroblocks), to
/// compare with that decode (T5_MPEG_RAW).
fn dump_in(v: u128, csc: bool) {
    use std::io::Write;
    type Sink = OnceLock<Option<Mutex<std::io::BufWriter<std::fs::File>>>>;
    static FILES: [Sink; 2] = [OnceLock::new(), OnceLock::new()];
    let Some(f) = FILES[csc as usize].get_or_init(|| {
        let path = std::env::var(if csc { "T5_IPUCSC" } else { "T5_IPUDUMP" }).ok()?;
        Some(Mutex::new(std::io::BufWriter::with_capacity(1 << 16, std::fs::File::create(path).ok()?)))
    }) else { return };
    let _ = f.lock().unwrap().write_all(&v.to_le_bytes());
}

/// `T5_IPUTRACE=<file>`: every BCLR, VDEC (result) and BDEC (hash of its output), one line each,
/// so a game run can be compared with the decode of the same stream in `tests::mpeg_stream_sync`
/// (which writes the same lines through this function when the variable is set).
fn trace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("T5_IPUTRACE").is_some())
}

fn trace(cmd: u32, data: u32, out: &[u128], pre: &str) {
    use std::io::Write;
    static FILE: OnceLock<Option<Mutex<(std::io::BufWriter<std::fs::File>, u32)>>> = OnceLock::new();
    let Some(f) = FILE.get_or_init(|| {
        let path = std::env::var("T5_IPUTRACE").ok()?;
        Some(Mutex::new((std::io::BufWriter::new(std::fs::File::create(path).ok()?), 0)))
    }) else { return };
    let mut g = f.lock().unwrap();
    let line = match cmd >> 28 {
        0 => format!("C {cmd:08x}
"),
        3 => format!("V {cmd:08x} {:04x}
", data & 0xFFFF),
        _ => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in out.iter().flat_map(|q| q.to_le_bytes()) {
                h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
            }
            format!("B {cmd:08x} {h:016x}{pre}
")
        }
    };
    let _ = g.0.write_all(line.as_bytes());
    g.1 += 1;
    // Flushed at every BCLR (picture / stream boundaries) and every 1024 lines.
    if g.1 % 1024 == 0 || cmd >> 28 == 0 {
        let _ = g.0.flush();
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
/// `csc`: CSC input (else the decoded stream), for T5_IPUCSC / T5_IPUDUMP.
fn commit(m: &mut Machine, n: usize, csc: bool) {
    let from_fifo = n.min(m.ipu.fifo.len());
    for v in m.ipu.fifo.drain(..from_fifo) {
        dump_in(v, csc);
    }
    if n > from_fifo {
        if let Some(mut c) = ToCursor::of(m) {
            for _ in from_fifo..n {
                if let Some(v) = c.next(m) {
                    dump_in(v, csc);
                }
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
            // As PCSX2 ipuWrite32: the mode bits (IDP, AS, IVF, QST, MP1, PCT) are written first,
            // then RST resets the IPU keeping them (ctrl.reset: & 0x7F33F00). libmpeg sets the
            // picture's modes in the same write as the reset.
            let wmask = 0x07F3_0000;
            m.ipu.ctrl = (m.ipu.ctrl & !wmask) | (v as u32 & wmask);
            if (m.ipu.ctrl >> 16) & 3 == 3 {
                m.ipu.ctrl &= !(1 << 17); // invalid IDP: 9 bits
            }
            if v & (1 << 30) != 0 {
                let keep = (m.ipu.iq_intra, m.ipu.iq_non, m.ipu.ctrl & 0x07F3_3F00);
                m.ipu = Ipu::default();
                (m.ipu.iq_intra, m.ipu.iq_non, m.ipu.ctrl) = keep;
            }
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
                    commit(m, n, cmd >> 28 == 7);
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

#[cfg(test)]
mod tests {
    use super::Ipu;

    /// The elementary video stream (PES id 0xE0) of the MPEG-2 program stream at `off` in the
    /// disc image.
    fn video_es(iso: &str, off: u64) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(iso).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut d = vec![0u8; 64 << 20];
        let n = f.read(&mut d).unwrap();
        d.truncate(n);
        let mut es = Vec::new();
        let mut p = 0usize;
        while p + 14 < d.len() && d[p..p + 3] == [0, 0, 1] {
            let id = d[p + 3];
            match id {
                0xBA => p += 14 + (d[p + 13] & 7) as usize,
                0xB9 => break,
                _ => {
                    let len = u16::from_be_bytes([d[p + 4], d[p + 5]]) as usize;
                    if id == 0xE0 {
                        let h = d[p + 8] as usize;
                        es.extend_from_slice(&d[p + 9 + h..p + 6 + len]);
                    }
                    p += 6 + len;
                }
            }
        }
        es
    }

    /// One IPU command on the stream from qword `qpos` (the position the IPU's BP continues in).
    /// The input arrives in pieces, as from the game's DMA transfers: 2 qwords, doubled while the
    /// command needs more, so every command also meets the end of the data it has.
    fn run(ipu: &mut Ipu, q: &[u128], qpos: &mut usize, cmd: u32) -> u32 {
        let mut n = 2;
        loop {
            let end = (*qpos + n).min(q.len());
            match ipu.exec(cmd, &q[*qpos..end]) {
                Ok(k) => {
                    *qpos += k;
                    return ipu.cmd_data;
                }
                Err(_) if end < q.len() => n *= 2,
                Err(_) => panic!("stream ran out"),
            }
        }
    }

    /// `n` plain bits: FDEC, then FDEC with FB = n to advance (as libmpeg reads fields).
    fn read(ipu: &mut Ipu, q: &[u128], qpos: &mut usize, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = run(ipu, q, qpos, 4 << 28) >> (32 - n);
        run(ipu, q, qpos, 4 << 28 | n);
        v
    }

    /// A decoded picture, 4:2:0.
    #[derive(Clone)]
    struct Pic {
        w: usize,
        h: usize,
        p: [Vec<u8>; 3],
    }

    impl Pic {
        fn new(w: usize, h: usize) -> Pic {
            Pic { w, h, p: [vec![0; w * h], vec![128; w * h / 4], vec![128; w * h / 4]] }
        }
        /// Half-pel prediction of a `bw` x `bh` block of plane `c` at full-pel (x, y) plus the
        /// half-pel vector (MPEG-2 7.6.4).
        fn pred(&self, c: usize, x: i32, y: i32, mvx: i32, mvy: i32, bw: usize, bh: usize) -> Vec<i32> {
            let (pw, ph) = if c == 0 { (self.w, self.h) } else { (self.w / 2, self.h / 2) };
            let px = |xx: i32, yy: i32| self.p[c][(yy.clamp(0, ph as i32 - 1) as usize) * pw + xx.clamp(0, pw as i32 - 1) as usize] as i32;
            let (ix, iy, hx, hy) = (x + (mvx >> 1), y + (mvy >> 1), mvx & 1, mvy & 1);
            let mut out = Vec::with_capacity(bw * bh);
            for j in 0..bh as i32 {
                for i in 0..bw as i32 {
                    let (a, b, c2, d) = (px(ix + i, iy + j), px(ix + i + 1, iy + j), px(ix + i, iy + j + 1), px(ix + i + 1, iy + j + 1));
                    out.push(match (hx, hy) {
                        (0, 0) => a,
                        (1, 0) => (a + b + 1) >> 1,
                        (0, _) => (a + c2 + 1) >> 1,
                        _ => (a + b + c2 + d + 2) >> 2,
                    });
                }
            }
            out
        }
    }

    /// Default intra quantiser matrix (MPEG-2 6.3.11), natural order.
    const DEFAULT_INTRA: [u8; 64] = [
        8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38, 22, 22, 26, 27, 29, 34, 37, 40,
        22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58, 26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
    ];

    /// The IPU on the game's own MPEG-2 streams, driven with the commands libmpeg issues (VDEC
    /// MBAI / MBT / motion codes, FDEC for plain fields, BDEC per macroblock):
    /// 1. sync: every slice must end exactly at the next start code with its row complete (a
    ///    wrong VLC code, length or sign shifts the stream);
    /// 2. with T5_MPEG_OUT=<dir>, pictures are reconstructed from the BDEC output with textbook
    ///    motion compensation (frame prediction, half-pel) and written as PPM (decode order), to
    ///    tell IPU errors (dequantisation, IDCT, CSC) from errors on the game's side.
    /// Needs the disc image (T5_DISC or the default path).
    #[test]
    fn mpeg_stream_sync() {
        let iso = std::env::var("T5_DISC").unwrap_or_else(|_| r"E:\Decompilation\Game\Tekken 5 (USA).iso".into());
        if !std::path::Path::new(&iso).exists() {
            eprintln!("no disc image, skipped");
            return;
        }
        let out_dir = std::env::var("T5_MPEG_OUT").ok();
        // T5_MPEG_ES=<file>: the stream a game run fed the IPU (T5_IPUDUMP) instead of the disc's
        // own streams; it may hold slices cut by the player's BCLR, so desyncs are counted there.
        let dumped = std::env::var("T5_MPEG_ES").ok();
        // T5_MPEG_MBLOG=<pic,pic,...>: print every macroblock of these pictures (decode order, 1-based).
        let mblog: Option<Vec<u32>> = std::env::var("T5_MPEG_MBLOG").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect());
        let strict = dumped.is_none();
        let sources: Vec<u64> = if strict { vec![0xc595000, 0x5fa8800] } else { vec![0] };
        let mut desyncs = 0;
        for off in sources {
            let mut es = match &dumped {
                Some(p) => std::fs::read(p).unwrap(),
                None => video_es(&iso, off),
            };
            let len = es.len();
            es.resize(len.div_ceil(16) * 16 + 64, 0);
            let q: Vec<u128> = es.chunks(16).map(|c| u128::from_le_bytes(c.try_into().unwrap())).collect();
            let next_start = |from: usize| (from..len.saturating_sub(3)).find(|&i| es[i..i + 3] == [0, 0, 1]);
            let mut ipu = Ipu::default();
            let mut qpos: usize;
            let (mut w, mut h, mut ptype, mut f) = (0usize, 0usize, 0u32, [[1u32; 2]; 2]);
            let (mut pics, mut slices, mut mbs) = (0, 0, 0);
            let (mut fwd, mut bwd, mut cur) = (Pic::new(16, 16), Pic::new(16, 16), Pic::new(16, 16));
            let mut at = 0usize;
            let raw_out = std::env::var("T5_MPEG_RAW").ok();
            if let Some(r) = &raw_out {
                let _ = std::fs::remove_file(r);
                let _ = std::fs::remove_file(format!("{r}.types"));
            }
            let finish = |cur: &Pic, pics: usize, ptype: u32| {
                if let Some(r) = &raw_out {
                    // RAW8 macroblocks in raster order (the layout libmpeg feeds the CSC), and
                    // the picture type per picture (decode order) in <file>.types.
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(r).unwrap();
                    let (mw, mh) = (cur.w / 16, cur.h / 16);
                    let mut mbs = Vec::with_capacity(mw * mh * 384);
                    for my in 0..mh {
                        for mx in 0..mw {
                            for y in 0..16 {
                                mbs.extend_from_slice(&cur.p[0][(my * 16 + y) * cur.w + mx * 16..][..16]);
                            }
                            for c in 1..3 {
                                for y in 0..8 {
                                    mbs.extend_from_slice(&cur.p[c][(my * 8 + y) * (cur.w / 2) + mx * 8..][..8]);
                                }
                            }
                        }
                    }
                    f.write_all(&mbs).unwrap();
                    let mut t = std::fs::OpenOptions::new().create(true).append(true).open(format!("{r}.types")).unwrap();
                    let _ = t.write_all(&[b"?IPB"[ptype as usize & 3]]);
                }
                if let Some(dir) = &out_dir {
                    if pics <= 60 || pics % 100 == 0 {
                        let conv = Ipu::default();
                        let mut ppm = format!("P6 {} {} 255\n", cur.w, cur.h).into_bytes();
                        for y in 0..cur.h {
                            for x in 0..cur.w {
                                let c = (y / 2) * (cur.w / 2) + x / 2;
                                let (r, g, b, _) = conv.rgb(cur.p[0][y * cur.w + x] as i32, cur.p[1][c] as i32, cur.p[2][c] as i32);
                                ppm.extend_from_slice(&[r, g, b]);
                            }
                        }
                        let t = ["?", "I", "P", "B"][ptype as usize];
                        std::fs::write(format!("{dir}/{off:x}_{pics:03}_{t}.ppm"), ppm).unwrap();
                    }
                }
            };
            while let Some(sc) = next_start(at) {
                let code = es[sc + 3];
                qpos = (sc + 4) / 16;
                ipu.bp = ((sc + 4) % 16) as u32 * 8;
                if code <= 0xAF && code != 0 && ptype == 0 {
                    at = sc + 4;
                    continue;
                }
                if (code == 0x00 || code == 0xB3 || code == 0xB7) && pics > 0 && ptype != 0 {
                    // The previous picture is complete.
                    finish(&cur, pics, ptype);
                    if ptype != 3 {
                        fwd = std::mem::replace(&mut bwd, cur.clone());
                    }
                    ptype = 0;
                }
                match code {
                    0xB3 => {
                        let (nw, nh) = (read(&mut ipu, &q, &mut qpos, 12) as usize, read(&mut ipu, &q, &mut qpos, 12) as usize);
                        if !strict && w != 0 {
                            // Later headers may be cut by the dump (the player's BCLR / SETIQ in
                            // between); the game's streams keep their size and matrices.
                            at = sc + 4;
                            continue;
                        }
                        (w, h) = (nw, nh);
                        read(&mut ipu, &q, &mut qpos, 4 + 4 + 18 + 1 + 10 + 1);
                        let mut intra = [0u8; 64];
                        for i in 0..64 {
                            intra[i] = DEFAULT_INTRA[super::ZIGZAG[i]];
                        }
                        if read(&mut ipu, &q, &mut qpos, 1) == 1 {
                            for v in intra.iter_mut() {
                                *v = read(&mut ipu, &q, &mut qpos, 8) as u8;
                            }
                        }
                        ipu.iq_intra = intra;
                        let mut non = [16u8; 64];
                        if read(&mut ipu, &q, &mut qpos, 1) == 1 {
                            for v in non.iter_mut() {
                                *v = read(&mut ipu, &q, &mut qpos, 8) as u8;
                            }
                        }
                        ipu.iq_non = non;
                        if bwd.w != w {
                            (fwd, bwd) = (Pic::new(w, h), Pic::new(w, h));
                        }
                    }
                    0x00 => {
                        read(&mut ipu, &q, &mut qpos, 10);
                        ptype = read(&mut ipu, &q, &mut qpos, 3);
                        pics += 1;
                        cur = Pic::new(w, h);
                    }
                    0xB5 => match read(&mut ipu, &q, &mut qpos, 4) {
                        8 => {
                            for s in 0..2 {
                                for t in 0..2 {
                                    f[s][t] = read(&mut ipu, &q, &mut qpos, 4);
                                }
                            }
                            let idp = read(&mut ipu, &q, &mut qpos, 2);
                            let ps = read(&mut ipu, &q, &mut qpos, 2);
                            read(&mut ipu, &q, &mut qpos, 1);
                            let fpfd = read(&mut ipu, &q, &mut qpos, 1);
                            let cmv = read(&mut ipu, &q, &mut qpos, 1);
                            let qst = read(&mut ipu, &q, &mut qpos, 1);
                            let ivf = read(&mut ipu, &q, &mut qpos, 1);
                            let alt = read(&mut ipu, &q, &mut qpos, 1);
                            assert!(ps == 3 && fpfd == 1 && cmv == 0, "field pictures / prediction / concealment not covered");
                            ipu.ctrl = idp << 16 | alt << 20 | ivf << 21 | qst << 22 | ptype << 24;
                        }
                        3 => panic!("quant matrix extension not covered"),
                        _ => {}
                    },
                    0x01..=0xAF => {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            slices += 1;
                            let mbw = w.div_ceil(16) as u32;
                            let mut qsc = read(&mut ipu, &q, &mut qpos, 5);
                            while read(&mut ipu, &q, &mut qpos, 1) == 1 {
                                read(&mut ipu, &q, &mut qpos, 8);
                            }
                            let mut addr = (code as u32 - 1) * mbw;
                            let mut first = true;
                            let mut dcr = true;
                            // Motion vector predictors [direction][x / y], previous B prediction.
                            let mut pmv = [[0i32; 2]; 2];
                            let mut prev_modes = 0u32;
                            loop {
                                // macroblock_address_increment (escapes / stuffing first).
                                let mut inc = 0;
                                loop {
                                    let d = run(&mut ipu, &q, &mut qpos, 3 << 28);
                                    assert!(d != 0, "MBAI error: pic {pics} slice {code} addr {addr} bit {}", qpos * 128 + ipu.bp as usize);
                                    match d & 0xFFFF {
                                        0x23 => inc += 33,
                                        0x22 => {}
                                        v => {
                                            inc += v;
                                            break;
                                        }
                                    }
                                }
                                if first {
                                    addr += inc - 1;
                                    first = false;
                                } else {
                                    // Skipped macroblocks: P forward with vector 0, B as the last.
                                    for s in 1..inc {
                                        let a = addr + s;
                                        if ptype == 2 {
                                            pmv = [[0; 2]; 2];
                                        }
                                        // P pictures predict from the newest reference.
                                        let f0 = if ptype == 2 { &bwd } else { &fwd };
                                        predict(&mut cur, f0, &bwd, a, mbw, if ptype == 2 { 8 } else { prev_modes }, &pmv, None);
                                    }
                                    addr += inc;
                                    if inc > 1 {
                                        dcr = true;
                                    }
                                }
                                let modes = run(&mut ipu, &q, &mut qpos, 3 << 28 | 1 << 26) & 0x1F;
                            if let Some(want) = &mblog {
                                if want.contains(&(pics as u32)) {
                                    eprintln!("MB pic {pics} type {ptype} addr {addr} (x {} y {}) inc {inc} modes {modes:02x} pmv {pmv:?}", addr % mbw, addr / mbw);
                                }
                            }
                                assert!(modes != 0, "MBT error: pic {pics} type {ptype} slice {code} addr {addr}");
                                if modes & 16 != 0 {
                                    qsc = read(&mut ipu, &q, &mut qpos, 5);
                                }
                                for (s, bit) in [(0usize, 8u32), (1, 4)] {
                                    if modes & bit != 0 {
                                        for t in 0..2 {
                                            let mc = run(&mut ipu, &q, &mut qpos, 3 << 28 | 2 << 26) as i32 as i16 as i32;
                                            let r = f[s][t] as i32 - 1;
                                            let delta = if r == 0 || mc == 0 {
                                                mc
                                            } else {
                                                let res = read(&mut ipu, &q, &mut qpos, r as u32) as i32;
                                                let d = ((mc.abs() - 1) << r) + res + 1;
                                                if mc < 0 { -d } else { d }
                                            };
                                            let range = 32 << r;
                                            let mut v = pmv[s][t] + delta;
                                            if v < -(16 << r) {
                                                v += range;
                                            } else if v > (16 << r) - 1 {
                                                v -= range;
                                            }
                                            pmv[s][t] = v;
                                        }
                                    }
                                }
                                let intra = modes & 1 != 0;
                                if mblog.as_ref().is_some_and(|w| w.is_empty() || w.contains(&(pics as u32))) {
                                    eprintln!("MV {pics} {ptype} {addr} {modes} {} {} {} {}", pmv[0][0], pmv[0][1], pmv[1][0], pmv[1][1]);
                                }
                                if intra || (ptype == 2 && modes & 8 == 0) {
                                    pmv = [[0; 2]; 2];
                                }
                                let mut resid: Option<Vec<i16>> = None;
                                if intra || modes & 2 != 0 {
                                    let cmd = 2 << 28 | (intra as u32) << 27 | ((intra && dcr) as u32) << 26 | qsc << 16;
                                    run(&mut ipu, &q, &mut qpos, cmd);
                                    let v: Vec<i16> = ipu.out.drain(..48).flat_map(|x| (0..8).map(move |k| (x >> (16 * k)) as u16 as i16)).collect();
                                if mblog.as_ref().is_some_and(|w| w.contains(&(pics as u32))) {
                                    eprintln!("BDEC pic {pics} addr {addr} cbp {:02x} Y {:?}", (ipu.ctrl >> 8) & 0x3F, &v[..256]);
                                }
                                    resid = Some(v);
                                }
                                let f0 = if ptype == 2 { &bwd } else { &fwd };
                                if intra {
                                    predict(&mut cur, f0, &bwd, addr, mbw, 0, &pmv, resid.as_deref());
                                } else {
                                    let m = if ptype == 2 { modes | 8 } else { modes };
                                    predict(&mut cur, f0, &bwd, addr, mbw, m, &pmv, resid.as_deref());
                                    prev_modes = m & 12;
                                }
                                dcr = !intra;
                                mbs += 1;
                                // Slice end: only zero bits up to the next start code.
                                let bp = qpos * 128 + ipu.bp as usize;
                                let next = next_start(bp.div_ceil(8)).unwrap_or(len);
                                if (bp..next * 8).all(|b| es[b / 8] >> (7 - b % 8) & 1 == 0) {
                                    assert_eq!(addr % mbw, mbw - 1, "slice ended early: pic {pics} type {ptype} slice {code} addr {addr}");
                                    break;
                                }
                                assert!(addr % mbw < mbw - 1, "slice ran past its row: pic {pics} type {ptype} slice {code} addr {addr} bit {bp}");
                            }
                        }));
                        if let Err(e) = r {
                            if strict {
                                std::panic::resume_unwind(e);
                            }
                            desyncs += 1;
                        }
                    }
                    0xB7 => break,
                    _ => {}
                }
                at = sc + 4;
            }
            // A BCLR line between streams (and the trace flushed), as the game's player issues.
            super::trace(0, 0, &[], "");
            eprintln!("stream {off:#x}: {pics} pictures, {slices} slices, {mbs} macroblocks, {desyncs} slices out of sync");
            assert!(pics > 0 && slices > 0);
        }
        assert!(!strict || desyncs == 0);
    }

    /// Macroblock `addr`: prediction (modes: 8 forward, 4 backward; 0 none = intra) plus the
    /// BDEC output (intra: the pixels, else the residual).
    #[allow(clippy::too_many_arguments)]
    fn predict(cur: &mut Pic, fwd: &Pic, bwd: &Pic, addr: u32, mbw: u32, modes: u32, pmv: &[[i32; 2]; 2], blk: Option<&[i16]>) {
        let (mx, my) = ((addr % mbw) as i32 * 16, (addr / mbw) as i32 * 16);
        for c in 0..3 {
            let (bs, x, y, pw) = if c == 0 { (16, mx, my, cur.w) } else { (8, mx / 2, my / 2, cur.w / 2) };
            let mv = |d: usize| if c == 0 { (pmv[d][0], pmv[d][1]) } else { (pmv[d][0] / 2, pmv[d][1] / 2) };
            let pf = (modes & 8 != 0).then(|| { let (a, b) = mv(0); fwd.pred(c, x, y, a, b, bs, bs) });
            let pb = (modes & 4 != 0).then(|| { let (a, b) = mv(1); bwd.pred(c, x, y, a, b, bs, bs) });
            let base = if c == 0 { 0 } else { 256 + (c - 1) * 64 };
            for j in 0..bs {
                for i in 0..bs {
                    let p = match (&pf, &pb) {
                        (Some(a), Some(b)) => (a[j * bs + i] + b[j * bs + i] + 1) >> 1,
                        (Some(a), None) => a[j * bs + i],
                        (None, Some(b)) => b[j * bs + i],
                        (None, None) => 0,
                    };
                    let r = blk.map_or(0, |v| v[base + j * bs + i] as i32);
                    cur.p[c][(y as usize + j) * pw + x as usize + i] = (p + r).clamp(0, 255) as u8;
                }
            }
        }
    }

    /// The integer IDCT stays within 1 of the exact transform (IEEE 1180 range inputs).
    #[test]
    fn idct_matches_reference() {
        let mut seed = 12345u32;
        let mut rnd = |m: i32| {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 8) % (2 * m as u32 + 1)) as i32 - m
        };
        for _ in 0..2000 {
            let mut blk = [0i32; 64];
            for (k, v) in blk.iter_mut().enumerate() {
                *v = if k < 10 { rnd(256) } else { rnd(5) };
            }
            let got = super::idct(&blk);
            for y in 0..8 {
                for x in 0..8 {
                    let mut s = 0.0f64;
                    for v in 0..8 {
                        for u in 0..8 {
                            let cu = if u == 0 { 0.5f64.sqrt() } else { 1.0 };
                            let cv = if v == 0 { 0.5f64.sqrt() } else { 1.0 };
                            let pi = std::f64::consts::PI;
                            s += cu * cv / 4.0 * blk[v * 8 + u] as f64 * ((2 * x + 1) as f64 * u as f64 * pi / 16.0).cos() * ((2 * y + 1) as f64 * v as f64 * pi / 16.0).cos();
                        }
                    }
                    assert!((got[y * 8 + x] - s.round() as i32).abs() <= 1, "{blk:?}");
                }
            }
        }
    }
}
