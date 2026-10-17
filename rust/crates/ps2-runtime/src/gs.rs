//! The Graphics Synthesizer: 4 MiB of local memory with its swizzled page / block / column
//! layouts, the general registers that GIF packets write, primitive assembly (drawing is in
//! `raster`), host <-> local and local -> local transfers, the privileged registers (display,
//! CSR, IMR) and the CRT output (PCRTC: two read circuits merged with PMODE, at each vertical
//! blank, into the frame the front end shows).
//!
//! Threading (PCSX2's MTGS): the EE side keeps the privileged registers and the GIF's register
//! events (SIGNAL / FINISH / LABEL) in [`GsFront`]; everything that touches local memory (general
//! registers, transfers, drawing, the display read-out at each vertical blank) runs in order on
//! the GS thread, fed in batches. A local -> host transfer waits for the GS thread; the EE side
//! stays at most two fields ahead of it.

use crate::raster;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed as R;

pub const VRAM: usize = 4 << 20;

// Pixel storage modes.
pub const PSMCT32: u32 = 0x00;
pub const PSMCT24: u32 = 0x01;
pub const PSMCT16: u32 = 0x02;
pub const PSMCT16S: u32 = 0x0A;
pub const PSMT8: u32 = 0x13;
pub const PSMT4: u32 = 0x14;
pub const PSMT8H: u32 = 0x1B;
pub const PSMT4HL: u32 = 0x24;
pub const PSMT4HH: u32 = 0x2C;
pub const PSMZ32: u32 = 0x30;
pub const PSMZ24: u32 = 0x31;
pub const PSMZ16: u32 = 0x32;
pub const PSMZ16S: u32 = 0x3A;

pub const BLOCK32: [[u32; 8]; 4] = [
    [0, 1, 4, 5, 16, 17, 20, 21],
    [2, 3, 6, 7, 18, 19, 22, 23],
    [8, 9, 12, 13, 24, 25, 28, 29],
    [10, 11, 14, 15, 26, 27, 30, 31],
];
pub const COLUMN32: [[u32; 8]; 8] = [
    [0, 1, 4, 5, 8, 9, 12, 13],
    [2, 3, 6, 7, 10, 11, 14, 15],
    [16, 17, 20, 21, 24, 25, 28, 29],
    [18, 19, 22, 23, 26, 27, 30, 31],
    [32, 33, 36, 37, 40, 41, 44, 45],
    [34, 35, 38, 39, 42, 43, 46, 47],
    [48, 49, 52, 53, 56, 57, 60, 61],
    [50, 51, 54, 55, 58, 59, 62, 63],
];
pub const BLOCK16: [[u32; 4]; 8] = [
    [0, 2, 8, 10],
    [1, 3, 9, 11],
    [4, 6, 12, 14],
    [5, 7, 13, 15],
    [16, 18, 24, 26],
    [17, 19, 25, 27],
    [20, 22, 28, 30],
    [21, 23, 29, 31],
];
pub const BLOCK16S: [[u32; 4]; 8] = [
    [0, 2, 16, 18],
    [1, 3, 17, 19],
    [8, 10, 24, 26],
    [9, 11, 25, 27],
    [4, 6, 20, 22],
    [5, 7, 21, 23],
    [12, 14, 28, 30],
    [13, 15, 29, 31],
];
const COLUMN16: [[u32; 16]; 8] = [
    [0, 2, 8, 10, 16, 18, 24, 26, 1, 3, 9, 11, 17, 19, 25, 27],
    [4, 6, 12, 14, 20, 22, 28, 30, 5, 7, 13, 15, 21, 23, 29, 31],
    [32, 34, 40, 42, 48, 50, 56, 58, 33, 35, 41, 43, 49, 51, 57, 59],
    [36, 38, 44, 46, 52, 54, 60, 62, 37, 39, 45, 47, 53, 55, 61, 63],
    [64, 66, 72, 74, 80, 82, 88, 90, 65, 67, 73, 75, 81, 83, 89, 91],
    [68, 70, 76, 78, 84, 86, 92, 94, 69, 71, 77, 79, 85, 87, 93, 95],
    [96, 98, 104, 106, 112, 114, 120, 122, 97, 99, 105, 107, 113, 115, 121, 123],
    [100, 102, 108, 110, 116, 118, 124, 126, 101, 103, 109, 111, 117, 119, 125, 127],
];
const BLOCK8: [[u32; 8]; 4] = BLOCK32;
const COLUMN8: [[u32; 16]; 16] = [
    [0, 4, 16, 20, 32, 36, 48, 52, 2, 6, 18, 22, 34, 38, 50, 54],
    [8, 12, 24, 28, 40, 44, 56, 60, 10, 14, 26, 30, 42, 46, 58, 62],
    [33, 37, 49, 53, 1, 5, 17, 21, 35, 39, 51, 55, 3, 7, 19, 23],
    [41, 45, 57, 61, 9, 13, 25, 29, 43, 47, 59, 63, 11, 15, 27, 31],
    [96, 100, 112, 116, 64, 68, 80, 84, 98, 102, 114, 118, 66, 70, 82, 86],
    [104, 108, 120, 124, 72, 76, 88, 92, 106, 110, 122, 126, 74, 78, 90, 94],
    [65, 69, 81, 85, 97, 101, 113, 117, 67, 71, 83, 87, 99, 103, 115, 119],
    [73, 77, 89, 93, 105, 109, 121, 125, 75, 79, 91, 95, 107, 111, 123, 127],
    [128, 132, 144, 148, 160, 164, 176, 180, 130, 134, 146, 150, 162, 166, 178, 182],
    [136, 140, 152, 156, 168, 172, 184, 188, 138, 142, 154, 158, 170, 174, 186, 190],
    [161, 165, 177, 181, 129, 133, 145, 149, 163, 167, 179, 183, 131, 135, 147, 151],
    [169, 173, 185, 189, 137, 141, 153, 157, 171, 175, 187, 191, 139, 143, 155, 159],
    [224, 228, 240, 244, 192, 196, 208, 212, 226, 230, 242, 246, 194, 198, 210, 214],
    [232, 236, 248, 252, 200, 204, 216, 220, 234, 238, 250, 254, 202, 206, 218, 222],
    [193, 197, 209, 213, 225, 229, 241, 245, 195, 199, 211, 215, 227, 231, 243, 247],
    [201, 205, 217, 221, 233, 237, 249, 253, 203, 207, 219, 223, 235, 239, 251, 255],
];
const BLOCK4: [[u32; 4]; 8] = BLOCK16;
const COLUMN4: [[u32; 32]; 16] = [
    [0, 8, 32, 40, 64, 72, 96, 104, 2, 10, 34, 42, 66, 74, 98, 106, 4, 12, 36, 44, 68, 76, 100, 108, 6, 14, 38, 46, 70, 78, 102, 110],
    [16, 24, 48, 56, 80, 88, 112, 120, 18, 26, 50, 58, 82, 90, 114, 122, 20, 28, 52, 60, 84, 92, 116, 124, 22, 30, 54, 62, 86, 94, 118, 126],
    [65, 73, 97, 105, 1, 9, 33, 41, 67, 75, 99, 107, 3, 11, 35, 43, 69, 77, 101, 109, 5, 13, 37, 45, 71, 79, 103, 111, 7, 15, 39, 47],
    [81, 89, 113, 121, 17, 25, 49, 57, 83, 91, 115, 123, 19, 27, 51, 59, 85, 93, 117, 125, 21, 29, 53, 61, 87, 95, 119, 127, 23, 31, 55, 63],
    [192, 200, 224, 232, 128, 136, 160, 168, 194, 202, 226, 234, 130, 138, 162, 170, 196, 204, 228, 236, 132, 140, 164, 172, 198, 206, 230, 238, 134, 142, 166, 174],
    [208, 216, 240, 248, 144, 152, 176, 184, 210, 218, 242, 250, 146, 154, 178, 186, 212, 220, 244, 252, 148, 156, 180, 188, 214, 222, 246, 254, 150, 158, 182, 190],
    [129, 137, 161, 169, 193, 201, 225, 233, 131, 139, 163, 171, 195, 203, 227, 235, 133, 141, 165, 173, 197, 205, 229, 237, 135, 143, 167, 175, 199, 207, 231, 239],
    [145, 153, 177, 185, 209, 217, 241, 249, 147, 155, 179, 187, 211, 219, 243, 251, 149, 157, 181, 189, 213, 221, 245, 253, 151, 159, 183, 191, 215, 223, 247, 255],
    [256, 264, 288, 296, 320, 328, 352, 360, 258, 266, 290, 298, 322, 330, 354, 362, 260, 268, 292, 300, 324, 332, 356, 364, 262, 270, 294, 302, 326, 334, 358, 366],
    [272, 280, 304, 312, 336, 344, 368, 376, 274, 282, 306, 314, 338, 346, 370, 378, 276, 284, 308, 316, 340, 348, 372, 380, 278, 286, 310, 318, 342, 350, 374, 382],
    [321, 329, 353, 361, 257, 265, 289, 297, 323, 331, 355, 363, 259, 267, 291, 299, 325, 333, 357, 365, 261, 269, 293, 301, 327, 335, 359, 367, 263, 271, 295, 303],
    [337, 345, 369, 377, 273, 281, 305, 313, 339, 347, 371, 379, 275, 283, 307, 315, 341, 349, 373, 381, 277, 285, 309, 317, 343, 351, 375, 383, 279, 287, 311, 319],
    [448, 456, 480, 488, 384, 392, 416, 424, 450, 458, 482, 490, 386, 394, 418, 426, 452, 460, 484, 492, 388, 396, 420, 428, 454, 462, 486, 494, 390, 398, 422, 430],
    [464, 472, 496, 504, 400, 408, 432, 440, 466, 474, 498, 506, 402, 410, 434, 442, 468, 476, 500, 508, 404, 412, 436, 444, 470, 478, 502, 510, 406, 414, 438, 446],
    [385, 393, 417, 425, 449, 457, 481, 489, 387, 395, 419, 427, 451, 459, 483, 491, 389, 397, 421, 429, 453, 461, 485, 493, 391, 399, 423, 431, 455, 463, 487, 495],
    [401, 409, 433, 441, 465, 473, 497, 505, 403, 411, 435, 443, 467, 475, 499, 507, 405, 413, 437, 445, 469, 477, 501, 509, 407, 415, 439, 447, 471, 479, 503, 511],
];

/// Byte address of a 32-bit pixel (PSMCT32 / 24 / Z32 / 24, T8H / T4HL / T4HH). `z`: Z layout.
#[inline(always)]
pub fn addr32(bp: u32, bw: u32, x: u32, y: u32, z: bool) -> usize {
    let page = (x >> 6) + (y >> 5) * bw.max(1);
    let mut block = BLOCK32[((y >> 3) & 3) as usize][((x >> 3) & 7) as usize];
    if z {
        block ^= 0x18;
    }
    let word = COLUMN32[(y & 7) as usize][(x & 7) as usize];
    (((bp << 6) + (page << 11) + (block << 6) + word) as usize * 4) & (VRAM - 1)
}

/// Byte address of a 16-bit pixel (PSMCT16 / 16S / Z16 / Z16S).
#[inline(always)]
pub fn addr16(bp: u32, bw: u32, x: u32, y: u32, psm: u32) -> usize {
    let page = (x >> 6) + (y >> 6) * bw.max(1);
    let (by, bx) = (((y >> 3) & 7) as usize, ((x >> 4) & 3) as usize);
    let mut block = if psm & 8 != 0 { BLOCK16S[by][bx] } else { BLOCK16[by][bx] };
    if psm & 0x30 == 0x30 {
        block ^= 0x18;
    }
    let half = COLUMN16[(y & 7) as usize][(x & 15) as usize];
    (((bp << 7) + (page << 12) + (block << 7) + half) as usize * 2) & (VRAM - 1)
}

/// Byte address of an 8-bit pixel (PSMT8).
#[inline(always)]
pub fn addr8(bp: u32, bw: u32, x: u32, y: u32) -> usize {
    let page = (x >> 7) + (y >> 6) * (bw >> 1).max(1);
    let block = BLOCK8[((y >> 4) & 3) as usize][((x >> 4) & 7) as usize];
    let byte = COLUMN8[(y & 15) as usize][(x & 15) as usize];
    ((bp << 8) + (page << 13) + (block << 8) + byte) as usize & (VRAM - 1)
}

/// Nibble address of a 4-bit pixel (PSMT4).
#[inline(always)]
pub fn addr4(bp: u32, bw: u32, x: u32, y: u32) -> usize {
    let page = (x >> 7) + (y >> 7) * (bw >> 1).max(1);
    let block = BLOCK4[((y >> 4) & 7) as usize][((x >> 5) & 3) as usize];
    let nib = COLUMN4[(y & 15) as usize][(x & 31) as usize];
    ((bp << 9) + (page << 14) + (block << 9) + nib) as usize & (2 * VRAM - 1)
}

/// A CLUT as a raster job fills it.
pub struct ClutBuf(pub std::cell::UnsafeCell<[u32; 256]>);
unsafe impl Sync for ClutBuf {}
unsafe impl Send for ClutBuf {}

impl ClutBuf {
    pub fn get(&self) -> &[u32; 256] {
        unsafe { &*self.0.get() }
    }
    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut [u32; 256] {
        unsafe { &mut *self.0.get() }
    }
}

/// A CLUT load raster worker 0 does: `out` = `base` (the CLUT before; None: `out` holds it
/// already) with the load's entries from local memory.
pub struct ClutJob {
    pub t: u64,
    pub texclut: u64,
    pub base: Option<std::sync::Arc<ClutBuf>>,
    pub out: std::sync::Arc<ClutBuf>,
}

/// A host -> local transfer chunk a raster worker stores (`Gs::run_upload`).
pub struct Upload {
    pub dbp: u32,
    pub dbw: u32,
    pub dpsm: u32,
    pub dsax: u32,
    pub dsay: u32,
    pub rrw: u32,
    pub rrh: u32,
    /// Where this chunk starts.
    pub x: u32,
    pub y: u32,
    pub data: Vec<u8>,
}

/// The finished picture for the front end (RGBA8, `width` x `height`).
#[derive(Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub serial: u64,
}

#[derive(Default)]
pub struct FrameShared {
    pub frame: Mutex<Frame>,
    /// Displayed frames and the game's own frame changes (the display buffer switched).
    pub frames: std::sync::atomic::AtomicU64,
    pub flips: std::sync::atomic::AtomicU64,
}

impl FrameShared {
    pub fn count(&self) -> u64 {
        self.frames.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn flips(&self) -> u64 {
        self.flips.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Copy the newest frame into `out` if it is newer than `seen`; returns its serial.
    pub fn take_if_new(&self, seen: u64, out: &mut Frame) -> u64 {
        let f = self.frame.lock().unwrap();
        if f.serial != seen {
            out.width = f.width;
            out.height = f.height;
            out.rgba.clear();
            out.rgba.extend_from_slice(&f.rgba);
            out.serial = f.serial;
        }
        f.serial
    }
}

/// A vertex as kicked: 12.4 fixed-point position, z, colour, texture coordinates, fog.
#[derive(Clone, Copy, Default, Debug)]
pub struct Vertex {
    pub x: i32,
    pub y: i32,
    pub z: u32,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub q: f32,
    pub s: f32,
    pub t: f32,
    pub u: u32,
    pub v: u32,
    pub f: u8,
}

/// The drawing context registers (two sets, chosen by PRIM.CTXT).
#[derive(Clone, Copy, Default)]
pub struct Context {
    pub tex0: u64,
    pub clamp: u64,
    pub tex1: u64,
    pub xyoffset: u64,
    pub miptbp1: u64,
    pub miptbp2: u64,
    pub scissor: u64,
    pub alpha: u64,
    pub test: u64,
    pub fba: u64,
    pub frame: u64,
    pub zbuf: u64,
}

pub struct Gs {
    pub mem: Box<[u8]>,
    /// The GPU rasterizer (default, T5_GPU=0 off): owns local memory then; `mem` is read back from it
    /// where the CPU needs it (display read-out, local -> host).
    pub gpu: Option<Box<crate::gpu::GsGpu>>,
    /// The last value written to each general register (gscap snapshots).
    pub reg_last: [u64; 128],
    pub reg_set: [bool; 128],
    pub ctx: [Context; 2],
    pub prim: u64,
    pub prmode: u64,
    pub prmodecont: u64,
    pub rgbaq: u64,
    pub st: u64,
    pub uv: u64,
    pub fog: u64,
    pub texclut: u64,
    pub scanmsk: u64,
    pub texa: u64,
    pub fogcol: u64,
    pub dimx: u64,
    pub dthe: u64,
    pub colclamp: u64,
    pub pabe: u64,
    pub bitbltbuf: u64,
    pub trxpos: u64,
    pub trxreg: u64,
    pub trxdir: u64,
    pub signal: u64,
    pub label: u64,
    /// Queued vertices of the current primitive.
    pub verts: Vec<Vertex>,
    /// Host -> local transfer in progress: next pixel position.
    pub xfer_x: u32,
    pub xfer_y: u32,
    pub xfer_active: bool,
    pub xfer_pending: Vec<u8>,
    /// Local -> host transfer data waiting for the VIF1 FIFO / DMA.
    pub download: std::collections::VecDeque<u128>,
    /// CLUT buffer (256 x 32-bit entries, CSM1 layout resolved at load).
    pub clut: [u32; 256],
    pub clut_tex0: u64,
    /// Hash of `clut` and its format (set by `load_clut`).
    pub clut_hash: u64,
    /// A CLUT load whose source is still being drawn by raster workers: (TEX0, TEXCLUT, source
    /// pages, raster batch to wait for). Done when something needs the CLUT or would change its
    /// source (`realize_clut`), dropped when a later load overwrites the same entries first.
    pub pending_clut: Option<(u64, u64, [u64; 8], u64)>,
    /// The CLUT as a raster job leaves it (loads done in the workers' stream, see
    /// `realize_clut`): `clut` is stale while this is set; `flush_sync` brings it back.
    pub clut_job: Option<std::sync::Arc<ClutBuf>>,
    clut_token: u64,
    /// The raster job that fills `clut_job`.
    pub clut_job_seq: u64,
    pending_mask: [u64; 8],
    // Privileged registers.
    pub pmode: u64,
    pub smode1: u64,
    pub smode2: u64,
    pub srfsh: u64,
    pub synch1: u64,
    pub synch2: u64,
    pub syncv: u64,
    pub dispfb: [u64; 2],
    pub display: [u64; 2],
    pub extbuf: u64,
    pub extdata: u64,
    pub extwrite: u64,
    pub bgcolor: u64,
    pub csr: u64,
    pub imr: u64,
    pub busdir: u64,
    pub siglblid: u64,
    pub crt_interlace: u32,
    pub crt_mode: u32,
    pub crt_ffmd: u32,
    /// Read-out state (frame serial, last picture hash, last display buffer).
    pub readout: std::sync::Arc<Mutex<ReadoutState>>,
    /// The front end's frame and the fields-done counter (read-out as a raster job), set on the
    /// GS thread.
    pub job_ctx: Option<(std::sync::Arc<FrameShared>, std::sync::Arc<std::sync::atomic::AtomicU64>)>,
    pub prims: u64,
    /// Write generation per 8 KiB page of local memory (texture cache invalidation).
    pub page_gen: Vec<std::sync::atomic::AtomicU32>,
    /// The resolved state of the last primitive and the register values it came from (PCSX2
    /// re-evaluates draw state only when it changes): reused while the registers are equal and
    /// no page of its textures was written (`st_pages` / `st_valid`).
    pub(crate) st_cache: Option<([u64; 19], std::sync::Arc<crate::raster::Setup>)>,
    pub st_pages: [u64; 8],
    pub st_valid: std::sync::atomic::AtomicBool,
    /// Pages (of 512) the queued and in-flight primitives read or write as frame / depth
    /// buffer (PCSX2 GSRendererSW m_fzb_pages): local memory accesses that miss them need not
    /// wait for the drawing. Cleared once everything is drawn.
    pub busy_pages: [std::sync::atomic::AtomicU64; 8],
    /// Decoded textures: key -> (generation sum of their pages, image).
    pub tex_cache: std::collections::HashMap<(u64, u64, u64), (u64, std::sync::Arc<crate::raster::TexImg>, u64)>,
    /// Primitives queued for drawing (see raster::flush).
    pub batch: crate::raster::Batch,
    /// A batch drawing in the background: signalled when done (see raster::wait).
    pub pending: Option<std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>>,
    /// The raster worker threads (started at the first parallel batch).
    pub workers: Option<Box<crate::raster::Workers>>,
}

impl Gs {
    pub fn new() -> Gs {
        Gs {
            mem: vec![0u8; VRAM].into_boxed_slice(),
            gpu: None,
            ctx: [Context::default(); 2],
            prim: 0,
            prmode: 0,
            prmodecont: 1,
            rgbaq: 0x3F80_0000_0000_0000,
            st: 0,
            uv: 0,
            fog: 0,
            texclut: 0,
            scanmsk: 0,
            texa: 0,
            fogcol: 0,
            dimx: 0,
            dthe: 0,
            colclamp: 1,
            pabe: 0,
            bitbltbuf: 0,
            trxpos: 0,
            trxreg: 0,
            trxdir: 3,
            signal: 0,
            label: 0,
            verts: Vec::new(),
            reg_last: [0; 128],
            reg_set: [false; 128],
            xfer_x: 0,
            xfer_y: 0,
            xfer_active: false,
            xfer_pending: Vec::new(),
            download: Default::default(),
            clut: [0; 256],
            clut_tex0: 0,
            clut_hash: 0,
            pending_clut: None,
            clut_job: None,
            clut_token: 0,
            clut_job_seq: 0,
            pending_mask: [0; 8],
            pmode: 0,
            smode1: 0,
            smode2: 0,
            srfsh: 0,
            synch1: 0,
            synch2: 0,
            syncv: 0,
            dispfb: [0; 2],
            display: [0; 2],
            extbuf: 0,
            extdata: 0,
            extwrite: 0,
            bgcolor: 0,
            csr: 0x0000_0000_5500_0000 | 8,
            imr: 0xFF00,
            busdir: 0,
            siglblid: 0,
            crt_interlace: 1,
            crt_mode: 2,
            crt_ffmd: 0,
            readout: Default::default(),
            job_ctx: None,
            prims: 0,
            page_gen: (0..VRAM / 8192).map(|_| std::sync::atomic::AtomicU32::new(0)).collect(),
            st_cache: None,
            st_pages: [0; 8],
            st_valid: std::sync::atomic::AtomicBool::new(false),
            busy_pages: Default::default(),
            tex_cache: Default::default(),
            batch: Default::default(),
            pending: None,
            workers: None,
        }
    }

    // ---- local memory pixel access ----

    #[inline(always)]
    pub fn r32(&self, a: usize) -> u32 {
        u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap())
    }
    #[inline(always)]
    pub fn w32(&mut self, a: usize, v: u32) {
        self.mem[a..a + 4].copy_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    pub fn r16(&self, a: usize) -> u16 {
        u16::from_le_bytes([self.mem[a], self.mem[a + 1]])
    }
    #[inline(always)]
    pub fn w16(&mut self, a: usize, v: u16) {
        self.mem[a..a + 2].copy_from_slice(&v.to_le_bytes());
    }

    #[inline(always)]
    /// Read a pixel in any storage mode (raw value: colour bits, index or depth).
    pub fn read_pixel(&self, psm: u32, bp: u32, bw: u32, x: u32, y: u32) -> u32 {
        match psm {
            PSMCT32 | PSMZ32 => self.r32(addr32(bp, bw, x, y, psm == PSMZ32)),
            PSMCT24 | PSMZ24 => self.r32(addr32(bp, bw, x, y, psm == PSMZ24)) & 0xFF_FFFF,
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => self.r16(addr16(bp, bw, x, y, psm)) as u32,
            PSMT8 => self.mem[addr8(bp, bw, x, y)] as u32,
            PSMT4 => {
                let n = addr4(bp, bw, x, y);
                ((self.mem[n >> 1] >> ((n & 1) * 4)) & 15) as u32
            }
            PSMT8H => self.r32(addr32(bp, bw, x, y, false)) >> 24,
            PSMT4HL => (self.r32(addr32(bp, bw, x, y, false)) >> 24) & 15,
            PSMT4HH => self.r32(addr32(bp, bw, x, y, false)) >> 28,
            _ => self.r32(addr32(bp, bw, x, y, false)),
        }
    }

    /// The 8 KiB page holding pixel (x, y).
    #[inline(always)]
    pub fn page_of(psm: u32, bp: u32, bw: u32, x: u32, y: u32) -> usize {
        match psm {
            PSMT4 => addr4(bp, bw, x, y) >> 14,
            PSMT8 => addr8(bp, bw, x, y) >> 13,
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => addr16(bp, bw, x, y, psm) >> 13,
            _ => addr32(bp, bw, x, y, psm & 0x30 == 0x30) >> 13,
        }
    }

    /// Mark the pages of the pixel rectangle [x0, x1) x [y0, y1) written (texture cache).
    /// Call `f` for each page (0..512) a rectangle of a buffer covers (once per page in a row).
    pub fn for_pages(psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32, mut f: impl FnMut(usize) -> bool) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (pw, ph) = match psm {
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (64, 64),
            PSMT8 => (128, 64),
            PSMT4 => (128, 128),
            _ => (64, 32),
        };
        let mut y = y0 - y0 % ph;
        while y < y1 {
            let mut x = x0 - x0 % pw;
            while x < x1 {
                if !f(Self::page_of(psm, bp, bw, x.max(x0) & 2047, y.max(y0) & 2047) & 511) {
                    return;
                }
                x += pw;
            }
            y += ph;
        }
    }

    /// Mark a rectangle's pages as used by queued drawing.
    pub fn mark_busy(&self, psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        Self::for_pages(psm, bp, bw, x0, y0, x1, y1, |p| {
            self.busy_pages[p >> 6].fetch_or(1 << (p & 63), std::sync::atomic::Ordering::Relaxed);
            true
        });
    }

    /// Whether a rectangle touches pages queued drawing uses (PCSX2 InvalidateVideoMem /
    /// InvalidateLocalMem): only then must a transfer or CLUT load wait for it.
    pub fn hits_busy(&self, psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
        let mut hit = false;
        Self::for_pages(psm, bp, bw, x0, y0, x1, y1, |p| {
            hit = self.busy_pages[p >> 6].load(std::sync::atomic::Ordering::Relaxed) & (1 << (p & 63)) != 0;
            !hit
        });
        hit
    }

    /// Wait for queued drawing if a rectangle of local memory is in use by it.
    #[track_caller]
    pub fn sync_rect(&mut self, psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        let mut mask = [0u64; 8];
        Self::for_pages(psm, bp, bw, x0, y0, x1, y1, |p| {
            mask[p >> 6] |= 1 << (p & 63);
            true
        });
        crate::raster::sync_pages(self, &mask);
    }

    pub fn touch_rect(&self, psm: u32, bp: u32, bw: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (pw, ph) = match psm {
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => (64, 64),
            PSMT8 => (128, 64),
            PSMT4 => (128, 128),
            _ => (64, 32),
        };
        let mut seen = [usize::MAX; 8];
        let mut n = 0;
        let mut y = y0 - y0 % ph;
        while y < y1 {
            let mut x = x0 - x0 % pw;
            while x < x1 {
                let page = Self::page_of(psm, bp, bw, x.max(x0) & 2047, y.max(y0) & 2047) & 511;
                if !seen.contains(&page) {
                    seen[n % 8] = page;
                    n += 1;
                    self.page_gen[page].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if self.st_pages[page >> 6] & (1 << (page & 63)) != 0 {
                        self.st_valid.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                x += pw;
            }
            y += ph;
        }
    }

    #[inline(always)]
    /// Write a pixel in any storage mode (`mask`: bits to keep, as FRAME.FBMSK).
    pub fn write_pixel(&mut self, psm: u32, bp: u32, bw: u32, x: u32, y: u32, v: u32) {
        let page = Self::page_of(psm, bp, bw, x, y) & 511;
        self.page_gen[page].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.st_pages[page >> 6] & (1 << (page & 63)) != 0 {
            self.st_valid.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        self.store_pixel(psm, bp, bw, x, y, v)
    }

    #[inline(always)]
    /// Write a pixel without marking its page (the rasteriser marks whole primitives).
    pub fn store_pixel(&mut self, psm: u32, bp: u32, bw: u32, x: u32, y: u32, v: u32) {
        match psm {
            PSMCT32 | PSMZ32 => {
                let a = addr32(bp, bw, x, y, psm == PSMZ32);
                self.w32(a, v)
            }
            PSMCT24 | PSMZ24 => {
                let a = addr32(bp, bw, x, y, psm == PSMZ24);
                let o = self.r32(a);
                self.w32(a, (o & 0xFF00_0000) | (v & 0xFF_FFFF))
            }
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => {
                let a = addr16(bp, bw, x, y, psm);
                self.w16(a, v as u16)
            }
            PSMT8 => {
                let a = addr8(bp, bw, x, y);
                self.mem[a] = v as u8
            }
            PSMT4 => {
                let n = addr4(bp, bw, x, y);
                let b = &mut self.mem[n >> 1];
                let sh = (n & 1) * 4;
                *b = (*b & !(15 << sh)) | (((v & 15) as u8) << sh);
            }
            PSMT8H => {
                let a = addr32(bp, bw, x, y, false);
                let o = self.r32(a);
                self.w32(a, (o & 0x00FF_FFFF) | (v << 24))
            }
            PSMT4HL => {
                let a = addr32(bp, bw, x, y, false);
                let o = self.r32(a);
                self.w32(a, (o & !0x0F00_0000) | ((v & 15) << 24))
            }
            PSMT4HH => {
                let a = addr32(bp, bw, x, y, false);
                let o = self.r32(a);
                self.w32(a, (o & !0xF000_0000) | ((v & 15) << 28))
            }
            _ => {}
        }
    }

    // ---- general registers (GIF PACKED / REGLIST / A+D) ----

    pub fn write_reg(&mut self, reg: u32, v: u64) {
        let r = (reg & 127) as usize;
        self.reg_last[r] = v;
        self.reg_set[r] = true;
        match reg {
            0x00 => {
                self.prim = v;
                self.verts.clear();
            }
            0x01 => self.rgbaq = v,
            0x02 => self.st = v,
            0x03 => self.uv = v,
            // vertex(value, with_fog, kick): XYZF2 / XYZ2 kick drawing, XYZF3 / XYZ3 do not.
            0x04 => self.vertex(v, true, true),
            0x05 => self.vertex(v, false, true),
            0x0C => self.vertex(v, true, false),
            0x0D => self.vertex(v, false, false),
            0x06 | 0x07 => {
                let c = (reg - 6) as usize;
                self.ctx[c].tex0 = v;
                self.tex0_changed(v);
            }
            0x08 | 0x09 => self.ctx[(reg - 8) as usize].clamp = v,
            0x0A => self.fog = v,
            0x14 | 0x15 => self.ctx[(reg - 0x14) as usize].tex1 = v,
            0x16 | 0x17 => {
                // TEX2: TEX0 without the base / width fields.
                let c = (reg - 0x16) as usize;
                let mask = 0xFFFF_FFE0_0000_0000u64 | (0x3F << 20);
                let t = (self.ctx[c].tex0 & !mask) | (v & mask);
                self.ctx[c].tex0 = t;
                self.tex0_changed(t);
            }
            0x18 | 0x19 => self.ctx[(reg - 0x18) as usize].xyoffset = v,
            0x1A => self.prmodecont = v,
            0x1B => self.prmode = v,
            0x1C => self.texclut = v,
            0x22 => self.scanmsk = v,
            0x34 | 0x35 => self.ctx[(reg - 0x34) as usize].miptbp1 = v,
            0x36 | 0x37 => self.ctx[(reg - 0x36) as usize].miptbp2 = v,
            0x3B => self.texa = v,
            0x3D => self.fogcol = v,
            0x3F => {} // TEXFLUSH
            0x40 | 0x41 => self.ctx[(reg - 0x40) as usize].scissor = v,
            0x42 | 0x43 => self.ctx[(reg - 0x42) as usize].alpha = v,
            0x44 => self.dimx = v,
            0x45 => self.dthe = v,
            0x46 => self.colclamp = v,
            0x47 | 0x48 => self.ctx[(reg - 0x47) as usize].test = v,
            0x49 => self.pabe = v,
            0x4A | 0x4B => self.ctx[(reg - 0x4A) as usize].fba = v,
            0x4C | 0x4D => self.ctx[(reg - 0x4C) as usize].frame = v,
            0x4E | 0x4F => self.ctx[(reg - 0x4E) as usize].zbuf = v,
            0x50 => self.bitbltbuf = v,
            0x51 => self.trxpos = v,
            0x52 => self.trxreg = v,
            0x53 => self.start_transfer(v),
            0x54 => self.hwreg(v),
            0x60 => {
                // SIGNAL: ID bits under the mask.
                let mask = (v >> 32) as u32;
                let id = v as u32;
                let old = self.siglblid as u32;
                self.siglblid = (self.siglblid & !0xFFFF_FFFF) | ((old & !mask) | (id & mask)) as u64;
                self.csr |= 1;
            }
            0x61 => self.csr |= 2, // FINISH
            0x62 => {
                let mask = v >> 32;
                let id = v & 0xFFFF_FFFF;
                let old = self.siglblid >> 32;
                self.siglblid = (self.siglblid & 0xFFFF_FFFF) | (((old & !mask) | (id & mask)) << 32);
            }
            _ => {}
        }
    }

    /// The display-list order of vertices: kick a primitive when enough have arrived.
    fn vertex(&mut self, v: u64, with_fog: bool, kick: bool) {
        let (x, y) = ((v & 0xFFFF) as i32, ((v >> 16) & 0xFFFF) as i32);
        let (z, f) = if with_fog { (((v >> 32) & 0xFF_FFFF) as u32, (v >> 56) as u8) } else { ((v >> 32) as u32, (self.fog >> 56) as u8) };
        let q = f32::from_bits((self.rgbaq >> 32) as u32);
        let vert = Vertex {
            x,
            y,
            z,
            r: self.rgbaq as u8,
            g: (self.rgbaq >> 8) as u8,
            b: (self.rgbaq >> 16) as u8,
            a: (self.rgbaq >> 24) as u8,
            q: if q == 0.0 || !q.is_finite() { 1.0 } else { q },
            s: f32::from_bits(self.st as u32),
            t: f32::from_bits((self.st >> 32) as u32),
            u: (self.uv & 0x3FFF) as u32,
            v: ((self.uv >> 16) & 0x3FFF) as u32,
            f,
        };
        self.verts.push(vert);
        let kind = self.prim & 7;
        let need = match kind {
            0 => 1,
            1 | 2 => 2,
            3 | 4 | 5 => 3,
            6 => 2,
            _ => {
                self.verts.clear();
                return;
            }
        };
        if self.verts.len() < need {
            return;
        }
        if kick {
            self.prims += 1;
            raster::draw(self, kind as u32);
        }
        // Keep the vertices strips / fans reuse.
        match kind {
            2 => {
                let last = *self.verts.last().unwrap();
                self.verts.clear();
                self.verts.push(last);
            }
            4 => {
                let n = self.verts.len();
                let (a, b) = (self.verts[n - 2], self.verts[n - 1]);
                self.verts.clear();
                self.verts.push(a);
                self.verts.push(b);
            }
            5 => {
                let n = self.verts.len();
                let (a, b) = (self.verts[0], self.verts[n - 1]);
                self.verts.clear();
                self.verts.push(a);
                self.verts.push(b);
            }
            _ => self.verts.clear(),
        }
    }

    /// The primitive attributes in use: PRIM, or PRMODE when PRMODECONT.AC = 0.
    pub fn attrs(&self) -> u64 {
        if self.prmodecont & 1 != 0 { self.prim } else { (self.prim & 7) | (self.prmode & !7) }
    }

    /// TEX0 write: load the CLUT according to CLD.
    fn tex0_changed(&mut self, t: u64) {
        let psm = ((t >> 20) & 0x3F) as u32;
        let cld = (t >> 61) & 7;
        if !matches!(psm, PSMT8 | PSMT4 | PSMT8H | PSMT4HL | PSMT4HH) {
            return;
        }
        let cbp = ((t >> 37) & 0x3FFF) as u32;
        let load = match cld {
            1 => true,
            2 => true,
            3 => true,
            4 => (self.clut_tex0 >> 37) & 0x3FFF != cbp as u64,
            5 => (self.clut_tex0 >> 37) & 0x3FFF != cbp as u64,
            _ => false,
        };
        if !load {
            return;
        }
        self.clut_tex0 = t;
        self.load_clut(t);
    }

    /// CLUT slots a load writes: all 256 (8-bit), or 16 at CSA (4-bit).
    fn clut_slots(t: u64) -> (usize, usize) {
        let psm = ((t >> 20) & 0x3F) as u32;
        if matches!(psm, PSMT8 | PSMT8H) { (0, 256) } else { ((((t >> 56) & 0x1F) as usize * 16) & 255, 16) }
    }

    /// Draw on the GPU from now on (default; T5_GPU=0: the CPU reference rasterizer), with the
    /// current local memory and CLUT. Without a suitable GPU the CPU draws.
    pub fn enable_gpu(&mut self) {
        if std::env::var("T5_GPU").map_or(true, |v| v != "0") && self.gpu.is_none() {
            crate::raster::flush_sync(self);
            self.gpu = crate::gpu::GsGpu::new(&self.mem, &self.clut).map(Box::new);
        }
    }

    fn load_clut(&mut self, t: u64) {
        let cbp = ((t >> 37) & 0x3FFF) as u32;
        let cpsm = ((t >> 51) & 0xF) as u32;
        if self.gpu.is_some() {
            // In the GPU's stream, after the queued drawing if that writes the palette.
            let cp = if cpsm == 0 { PSMCT32 } else { cpsm };
            let mut mask = [0u64; 8];
            Self::for_pages(cp, cbp, 1, 0, 0, 16, 16, |p| {
                mask[p >> 6] |= 1 << (p & 63);
                true
            });
            if (t >> 55) & 1 != 0 || crate::raster::batch_touches(self, &mask, false) {
                crate::raster::flush(self);
            }
            let tc = self.texclut;
            self.gpu.as_mut().unwrap().clut(t, tc);
            return;
        }
        // An earlier deferred load: dropped if this one overwrites the same entries, else done first.
        if let Some(p) = self.pending_clut {
            let (a, n) = Self::clut_slots(p.0);
            let (b, m) = Self::clut_slots(t);
            if b <= a && a + n <= b + m && (t >> 55) & 1 == 0 {
                self.pending_clut = None;
                crate::diag::count("clut load dropped");
            } else {
                self.realize_clut();
            }
        }
        if (t >> 55) & 1 != 0 {
            crate::raster::flush_sync(self);
        } else {
            let cpsm = if cpsm == 0 { PSMCT32 } else { cpsm };
            // The CLUT lives in the workers' stream: this load goes there too (in order).
            if self.clut_job.is_some() {
                let mut mask = [0u64; 8];
                Self::for_pages(cpsm, cbp, 1, 0, 0, 16, 16, |p| {
                    mask[p >> 6] |= 1 << (p & 63);
                    true
                });
                let blocks = crate::raster::rect_blocks(cpsm, cbp, 1, 0, 0, 16, 16);
                let after = crate::raster::flight_using_blocks(self, &mask, Some(&blocks)).unwrap_or(0);
                self.clut_job_submit(t, self.texclut, mask, after);
                return;
            }
            let mut mask = [0u64; 8];
            Self::for_pages(cpsm, cbp, 1, 0, 0, 16, 16, |p| {
                mask[p >> 6] |= 1 << (p & 63);
                true
            });
            let blocks = crate::raster::rect_blocks(cpsm, cbp, 1, 0, 0, 16, 16);
            if let Some(seq) = crate::raster::flight_using_blocks(self, &mask, Some(&blocks)) {
                if crate::diag::counting() {
                    let up = crate::raster::flight_is_upload(self, seq);
                    crate::diag::count(if up { "clut deferred by upload" } else { "clut deferred by draw" });
                }
                self.pending_clut = Some((t, self.texclut, mask, seq));
                self.pending_mask = mask;
                return;
            }
        }
        self.read_clut(t);
    }

    /// Do a deferred CLUT load now (waits for the drawing it depends on).
    #[track_caller]
    pub fn realize_clut(&mut self) {
        if let Some((t, texclut, _, seq)) = self.pending_clut.take() {
            if crate::diag::counting() {
                let loc = std::panic::Location::caller();
                crate::diag::count_with(|| format!("clutreal_{}:{}", loc.file().rsplit(['/', '\\']).next().unwrap_or(""), loc.line()));
            }
            if crate::raster::safe() {
                crate::raster::wait_seq(self, seq);
                let cur = self.texclut;
                self.texclut = texclut;
                self.read_clut(t);
                self.texclut = cur;
                return;
            }
            let mask = self.pending_mask;
            self.clut_job_submit(t, texclut, mask, seq);
            crate::diag::count("clut load realized");
        }
    }

    /// Hand a CLUT load to raster worker 0 (after batch `after`, on top of the CLUT before it).
    fn clut_job_submit(&mut self, t: u64, texclut: u64, src_pages: [u64; 8], after: u64) {
        let out = std::sync::Arc::new(ClutBuf(std::cell::UnsafeCell::new(self.clut)));
        let base = self.clut_job.clone();
        self.clut_job_seq = crate::raster::submit_clut(self, ClutJob { t, texclut, base, out: out.clone() }, src_pages, after);
        self.clut_job = Some(out);
        self.clut_token += 1;
        self.clut_hash = 0x5EED_0000_0000_0000 ^ self.clut_token;
    }

    /// With every raster job done: the CLUT back on this thread (deferred load included).
    pub fn resolve_clut(&mut self) {
        if let Some(b) = self.clut_job.take() {
            self.clut = *b.get();
            let mut hsh: u64 = 0xcbf2_9ce4_8422_2325;
            for v in &self.clut {
                hsh = (hsh ^ *v as u64).wrapping_mul(0x100_0000_01b3);
            }
            self.clut_hash = hsh ^ ((self.clut_tex0 >> 51) & 0xF);
        }
        if let Some((t, texclut, _, _)) = self.pending_clut.take() {
            let cur = self.texclut;
            self.texclut = texclut;
            self.read_clut(t);
            self.texclut = cur;
        }
    }

    /// Store a CLUT load's entries into `clut` from local memory (as `read_clut`).
    pub fn read_clut_into(&self, t: u64, texclut: u64, clut: &mut [u32; 256]) {
        let psm = ((t >> 20) & 0x3F) as u32;
        let cbp = ((t >> 37) & 0x3FFF) as u32;
        let cpsm = ((t >> 51) & 0xF) as u32;
        let csm = (t >> 55) & 1;
        let csa = ((t >> 56) & 0x1F) as usize;
        let eight = matches!(psm, PSMT8 | PSMT8H);
        let n = if eight { 256 } else { 16 };
        let cpsm = if cpsm == 0 { PSMCT32 } else { cpsm };
        for i in 0..n {
            let (x, y) = if csm == 0 {
                if eight {
                    let col = (i & 7) | ((i & 0x10) >> 1);
                    let row = ((i >> 5) << 1) | ((i >> 3) & 1);
                    (col as u32, row as u32)
                } else {
                    ((i & 7) as u32, (i >> 3) as u32)
                }
            } else {
                let cou = ((texclut >> 6) & 0x3F) as u32 * 16;
                let cov = ((texclut >> 12) & 0x3FF) as u32;
                (cou + i as u32, cov)
            };
            let cbw = if csm == 0 { 1 } else { (texclut & 0x3F) as u32 };
            let c = self.read_pixel(cpsm, cbp, cbw, x, y);
            let slot = if eight { i } else { (csa * 16 + i) & 255 };
            clut[slot] = if cpsm == PSMCT32 || cpsm == PSMCT24 { c } else { c | 0x8000_0000 };
            if cpsm == PSMCT16 || cpsm == PSMCT16S {
                clut[slot] = c & 0xFFFF;
            }
        }
    }

    fn read_clut(&mut self, t: u64) {
        let psm = ((t >> 20) & 0x3F) as u32;
        let cbp = ((t >> 37) & 0x3FFF) as u32;
        let cpsm = ((t >> 51) & 0xF) as u32;
        let csm = (t >> 55) & 1;
        let csa = ((t >> 56) & 0x1F) as usize;
        let eight = matches!(psm, PSMT8 | PSMT8H);
        let n = if eight { 256 } else { 16 };
        let cpsm = if cpsm == 0 { PSMCT32 } else { cpsm };
        for i in 0..n {
            let (x, y) = if csm == 0 {
                if eight {
                    // CSM1, 256 entries: 16x16 with the 8..15 / 16..23 rows swapped.
                    let col = (i & 7) | ((i & 0x10) >> 1);
                    let row = ((i >> 5) << 1) | ((i >> 3) & 1);
                    (col as u32, row as u32)
                } else {
                    ((i & 7) as u32, (i >> 3) as u32)
                }
            } else {
                let cou = ((self.texclut >> 6) & 0x3F) as u32 * 16;
                let cov = ((self.texclut >> 12) & 0x3FF) as u32;
                (cou + i as u32, cov)
            };
            let cbw = if csm == 0 { 1 } else { (self.texclut & 0x3F) as u32 };
            let c = self.read_pixel(cpsm, cbp, cbw, x, y);
            let slot = if eight { i } else { (csa * 16 + i) & 255 };
            self.clut[slot] = if cpsm == PSMCT32 || cpsm == PSMCT24 { c } else { c | 0x8000_0000 };
            if cpsm == PSMCT16 || cpsm == PSMCT16S {
                self.clut[slot] = c & 0xFFFF;
            }
        }
        // The CLUT's hash (texture cache key), once per load instead of per primitive.
        let mut hsh: u64 = 0xcbf2_9ce4_8422_2325;
        for v in &self.clut {
            hsh = (hsh ^ *v as u64).wrapping_mul(0x100_0000_01b3);
        }
        self.clut_hash = hsh ^ ((self.clut_tex0 >> 51) & 0xF);
    }

    // ---- transfers ----

    fn start_transfer(&mut self, v: u64) {
        self.trxdir = v;
        self.xfer_pending.clear();
        let dir = v & 3;
        let ssax = ((self.trxpos) & 0x7FF) as u32;
        let ssay = ((self.trxpos >> 16) & 0x7FF) as u32;
        let dsax = ((self.trxpos >> 32) & 0x7FF) as u32;
        let dsay = ((self.trxpos >> 48) & 0x7FF) as u32;
        match dir {
            0 => {
                self.xfer_active = true;
                self.xfer_x = dsax;
                self.xfer_y = dsay;
            }
            1 => {
                self.local_to_host(ssax, ssay);
            }
            2 => self.local_to_local(ssax, ssay, dsax, dsay),
            _ => self.xfer_active = false,
        }
    }

    /// HWREG: image data for a host -> local transfer (64 bits at a time).
    pub fn hwreg(&mut self, v: u64) {
        self.image_data(&v.to_le_bytes());
    }

    /// Image data (GIF IMAGE mode / HWREG) for the host -> local transfer in progress.
    pub fn image_data(&mut self, data: &[u8]) {
        if !self.xfer_active {
            return;
        }
        let dbp = ((self.bitbltbuf >> 32) & 0x3FFF) as u32;
        let dbw = ((self.bitbltbuf >> 48) & 0x3F) as u32;
        let dpsm = ((self.bitbltbuf >> 56) & 0x3F) as u32;
        let rrw = (self.trxreg & 0xFFF) as u32;
        let rrh = ((self.trxreg >> 32) & 0xFFF) as u32;
        let dsax = ((self.trxpos >> 32) & 0x7FF) as u32;
        let dsay = ((self.trxpos >> 48) & 0x7FF) as u32;
        // Rectangles wrapping at 2048 are rare: wait for everything then. Otherwise a transfer
        // into pages raster batches in flight use becomes a job in their stream (no waiting).
        let mut defer: Option<(u64, [u64; 8])> = None;
        if self.gpu.is_some() {
            // In the GPU's stream, after the queued drawing if that uses the pages.
            let mut mask = [0u64; 8];
            Self::for_pages(dpsm, dbp, dbw, dsax, dsay, (dsax + rrw).min(2048), (dsay + rrh).min(2048), |p| {
                mask[p >> 6] |= 1 << (p & 63);
                true
            });
            if dsax + rrw > 2048 || dsay + rrh > 2048 || crate::raster::batch_touches(self, &mask, true) {
                crate::raster::flush(self);
            }
            defer = Some((0, mask));
        } else if dsax + rrw > 2048 || dsay + rrh > 2048 {
            crate::raster::flush_sync(self);
        } else {
            let mut mask = [0u64; 8];
            Self::for_pages(dpsm, dbp, dbw, dsax, dsay, dsax + rrw, dsay + rrh, |p| {
                mask[p >> 6] |= 1 << (p & 63);
                true
            });
            if self.pending_clut.is_some_and(|p| (0..8).any(|i| p.2[i] & mask[i] != 0)) {
                self.realize_clut();
            }
            let blocks = crate::raster::rect_blocks(dpsm, dbp, dbw, dsax, dsay, dsax + rrw, dsay + rrh);
            match crate::raster::flight_using_blocks(self, &mask, Some(&blocks)) {
                Some(seq) if !crate::diag::enabled("T5_NODEFERUP") && !crate::raster::safe() => {
                    if crate::diag::counting() {
                        let up = crate::raster::flight_is_upload(self, seq);
                        crate::diag::count_with(|| format!("upload deferred {} dbp{dbp:x} {rrw}x{rrh}", if up { "by upload" } else { "by draw" }));
                    }
                    defer = Some((seq, mask))
                }
                Some(_) => self.sync_rect(dpsm, dbp, dbw, dsax, dsay, dsax + rrw, dsay + rrh),
                None => {}
            }
        }
        if rrw == 0 {
            return;
        }
        self.xfer_pending.extend_from_slice(data);
        let bits = match dpsm {
            PSMCT32 | PSMZ32 => 32,
            PSMCT24 | PSMZ24 => 24,
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => 16,
            PSMT8 | PSMT8H => 8,
            _ => 4,
        };
        let mut used = 0usize;
        let pending = std::mem::take(&mut self.xfer_pending);
        let total_bits = pending.len() * 8;
        // The pages of the rows this data reaches change (texture cache), once per call.
        let rows = (total_bits / bits).div_ceil(rrw as usize) as u32 + 1;
        let y0 = self.xfer_y.max(dsay);
        self.touch_rect(dpsm, dbp, dbw, dsax, y0, dsax + rrw, (y0 + rows).min(dsay + rrh));
        let (sx, sy) = (self.xfer_x, self.xfer_y);
        let write = defer.is_none();
        let mut bitpos = 0usize;
        if self.gpu.is_some() {
            // The GPU stores the pixels: only advance the position (as the loop below does).
            let done_px = (self.xfer_y.saturating_sub(dsay) as u64) * rrw as u64 + (self.xfer_x - dsax) as u64;
            let left = (rrh as u64 * rrw as u64).saturating_sub(done_px);
            let n = ((total_bits / bits) as u64).min(left);
            if self.xfer_y >= dsay + rrh {
                self.xfer_active = false;
            }
            let off = (self.xfer_x - dsax) as u64 + n;
            self.xfer_y += (off / rrw as u64) as u32;
            self.xfer_x = dsax + (off % rrw as u64) as u32;
            bitpos = n as usize * bits;
            used = bitpos.div_ceil(8);
        }
        while self.gpu.is_none() && bitpos + bits <= total_bits {
            if self.xfer_y >= dsay + rrh {
                self.xfer_active = false;
                break;
            }
            let byte = bitpos / 8;
            let v = match bits {
                32 => u32::from_le_bytes(pending[byte..byte + 4].try_into().unwrap()),
                24 => pending[byte] as u32 | (pending[byte + 1] as u32) << 8 | (pending[byte + 2] as u32) << 16,
                16 => u16::from_le_bytes([pending[byte], pending[byte + 1]]) as u32,
                8 => pending[byte] as u32,
                _ => ((pending[byte] >> (bitpos & 4)) & 15) as u32,
            };
            let (x, y) = (self.xfer_x & 2047, self.xfer_y & 2047);
            if write {
                self.store_pixel(dpsm, dbp, dbw, x, y, v);
            }
            bitpos += bits;
            used = bitpos.div_ceil(8);
            self.xfer_x += 1;
            if self.xfer_x >= dsax + rrw {
                self.xfer_x = dsax;
                self.xfer_y += 1;
            }
        }
        if let Some((seq, mask)) = defer {
            let up = Upload { dbp, dbw, dpsm, dsax, dsay, rrw, rrh, x: sx, y: sy, data: pending[..bitpos.div_ceil(8)].to_vec() };
            match self.gpu.as_mut() {
                Some(g) => g.upload(&up),
                None => crate::raster::submit_upload(self, up, mask, seq),
            }
        }
        if self.xfer_active && used < pending.len() {
            self.xfer_pending = pending[bitpos / 8..].to_vec();
        }
        if self.xfer_y >= dsay + rrh {
            self.xfer_active = false;
        }
    }

    /// Store a deferred host -> local transfer chunk (on a raster worker, see `image_data`).
    pub fn run_upload(&mut self, u: &Upload) {
        let bits = match u.dpsm {
            PSMCT32 | PSMZ32 => 32,
            PSMCT24 | PSMZ24 => 24,
            PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => 16,
            PSMT8 | PSMT8H => 8,
            _ => 4,
        };
        let (mut x, mut y) = (u.x, u.y);
        let total = u.data.len() * 8;
        let mut bitpos = 0usize;
        while bitpos + bits <= total && y < u.dsay + u.rrh {
            let byte = bitpos / 8;
            let d = &u.data;
            let v = match bits {
                32 => u32::from_le_bytes(d[byte..byte + 4].try_into().unwrap()),
                24 => d[byte] as u32 | (d[byte + 1] as u32) << 8 | (d[byte + 2] as u32) << 16,
                16 => u16::from_le_bytes([d[byte], d[byte + 1]]) as u32,
                8 => d[byte] as u32,
                _ => ((d[byte] >> (bitpos & 4)) & 15) as u32,
            };
            self.store_pixel(u.dpsm, u.dbp, u.dbw, x & 2047, y & 2047, v);
            bitpos += bits;
            x += 1;
            if x >= u.dsax + u.rrw {
                x = u.dsax;
                y += 1;
            }
        }
    }

    fn local_to_host(&mut self, sx: u32, sy: u32) {
        crate::raster::flush_sync(self);
        if let Some(g) = self.gpu.as_mut() {
            g.sync_mem(&mut self.mem);
        }
        let sbp = (self.bitbltbuf & 0x3FFF) as u32;
        let sbw = ((self.bitbltbuf >> 16) & 0x3F) as u32;
        let spsm = ((self.bitbltbuf >> 24) & 0x3F) as u32;
        let rrw = (self.trxreg & 0xFFF) as u32;
        let rrh = ((self.trxreg >> 32) & 0xFFF) as u32;
        let mut bytes = Vec::new();
        let mut nib: Option<u8> = None;
        for y in sy..sy + rrh {
            for x in sx..sx + rrw {
                let v = self.read_pixel(spsm, sbp, sbw, x & 2047, y & 2047);
                match spsm {
                    PSMCT32 | PSMZ32 => bytes.extend_from_slice(&v.to_le_bytes()),
                    PSMCT24 | PSMZ24 => bytes.extend_from_slice(&v.to_le_bytes()[..3]),
                    PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S => bytes.extend_from_slice(&(v as u16).to_le_bytes()),
                    PSMT8 | PSMT8H => bytes.push(v as u8),
                    _ => match nib.take() {
                        Some(lo) => bytes.push(lo | ((v as u8) << 4)),
                        None => nib = Some(v as u8 & 15),
                    },
                }
            }
        }
        if let Some(lo) = nib {
            bytes.push(lo);
        }
        while bytes.len() % 16 != 0 {
            bytes.push(0);
        }
        if std::env::var_os("T5_DLLOG").is_some() {
            let nz = bytes.iter().filter(|&&b| b != 0).count();
            eprintln!("[dl] bp {sbp:#x} bw {sbw} psm {spsm:#x} at {sx},{sy} {rrw}x{rrh} bytes {} nonzero {nz} head {:02x?}", bytes.len(), &bytes[..bytes.len().min(16)]);
        }
        self.download.clear();
        for c in bytes.chunks(16) {
            self.download.push_back(u128::from_le_bytes(c.try_into().unwrap()));
        }
        self.busdir = 1;
    }

    pub fn download_qword(&mut self) -> u128 {
        self.download.pop_front().unwrap_or(0)
    }

    fn local_to_local(&mut self, sx: u32, sy: u32, dx: u32, dy: u32) {
        crate::raster::flush_sync(self);
        if self.gpu.is_some() {
            let sbp = (self.bitbltbuf & 0x3FFF) as u32;
            let sbw = ((self.bitbltbuf >> 16) & 0x3F) as u32;
            let spsm = ((self.bitbltbuf >> 24) & 0x3F) as u32;
            let dbp = ((self.bitbltbuf >> 32) & 0x3FFF) as u32;
            let dbw = ((self.bitbltbuf >> 48) & 0x3F) as u32;
            let dpsm = ((self.bitbltbuf >> 56) & 0x3F) as u32;
            let rrw = (self.trxreg & 0xFFF) as u32;
            let rrh = ((self.trxreg >> 32) & 0xFFF) as u32;
            let dir = ((self.trxpos >> 59) & 3) as u32;
            // Parallel unless the source and destination pages may overlap (then in order).
            // Byte range of the page rows a rectangle touches (all of memory if it wraps).
            let span = |psm: u32, bp: u32, bw: u32, y: u32| -> (u64, u64) {
                if y + rrh > 2048 || sx + rrw > 2048 || dx + rrw > 2048 {
                    return (0, u64::MAX);
                }
                let ph = match psm {
                    PSMCT16 | PSMCT16S | PSMZ16 | PSMZ16S | PSMT8 => 64,
                    PSMT4 => 128,
                    _ => 32,
                };
                let row = (bw.max(1) as u64).div_ceil(if matches!(psm, PSMT8 | PSMT4) { 2 } else { 1 }) * 8192;
                let base = bp as u64 * 256;
                (base + (y / ph) as u64 * row, base + (y + rrh).div_ceil(ph) as u64 * row + 8192)
            };
            let (s0, s1) = span(spsm, sbp, sbw, sy);
            let (d0, d1) = span(dpsm, dbp, dbw, dy);
            let serial = s0 < d1 && d0 < s1;
            self.touch_rect(dpsm, dbp, dbw, dx.min(2047), dy.min(2047), (dx + rrw).min(2048), (dy + rrh).min(2048));
            self.gpu.as_mut().unwrap().copy((sbp, sbw, spsm), (dbp, dbw, dpsm), dir, (sx, sy), (dx, dy), (rrw, rrh), serial);
            return;
        }
        let sbp = (self.bitbltbuf & 0x3FFF) as u32;
        let sbw = ((self.bitbltbuf >> 16) & 0x3F) as u32;
        let spsm = ((self.bitbltbuf >> 24) & 0x3F) as u32;
        let dbp = ((self.bitbltbuf >> 32) & 0x3FFF) as u32;
        let dbw = ((self.bitbltbuf >> 48) & 0x3F) as u32;
        let dpsm = ((self.bitbltbuf >> 56) & 0x3F) as u32;
        let rrw = (self.trxreg & 0xFFF) as u32;
        let rrh = ((self.trxreg >> 32) & 0xFFF) as u32;
        let dir = (self.trxpos >> 59) & 3;
        let ys: Vec<u32> = if dir & 2 != 0 { (0..rrh).rev().collect() } else { (0..rrh).collect() };
        let xs: Vec<u32> = if dir & 1 != 0 { (0..rrw).rev().collect() } else { (0..rrw).collect() };
        for &y in &ys {
            for &x in &xs {
                let v = self.read_pixel(spsm, sbp, sbw, (sx + x) & 2047, (sy + y) & 2047);
                self.write_pixel(dpsm, dbp, dbw, (dx + x) & 2047, (dy + y) & 2047, v);
            }
        }
    }

    /// Vertical blank: read out the display circuits into the front end's frame.
    /// Vertical blank: read out the display circuits into the front end's frame. With raster
    /// workers running this is their job (after the drawing of the displayed pages; it counts
    /// the field done then), else it happens here after everything is drawn.
    /// Returns whether the field was counted done already.
    pub fn vblank_start(&mut self, shared: &FrameShared, _field: u32) -> bool {
        let r = DisplayRegs { pmode: self.pmode, smode2: self.smode2, dispfb: self.dispfb, display: self.display, bgcolor: self.bgcolor };
        if self.gpu.is_some() {
            crate::raster::flush(self);
            // The GS thread: read out on the read-out thread (counts the field done there).
            if let Some((sh, done)) = self.job_ctx.clone() {
                let job = ReadoutJob { regs: r, shared: sh, done, state: self.readout.clone() };
                self.gpu.as_mut().unwrap().readout_async(job);
                return true;
            }
            if let Some(g) = self.gpu.as_mut() {
                g.sync_mem(&mut self.mem);
            }
        }
        if let Some((sh, done)) = self.job_ctx.clone() {
            if self.workers.is_some() && !crate::diag::enabled("T5_SYNCREADOUT") && !crate::raster::safe() {
                let mut mask = [0u64; 8];
                for cidx in 0..2 {
                    if r.pmode & (1 << cidx) == 0 {
                        continue;
                    }
                    let fb = r.dispfb[cidx];
                    let (fbp, fbw, psm) = (((fb & 0x1FF) * 32) as u32, ((fb >> 9) & 0x3F) as u32, ((fb >> 15) & 0x1F) as u32);
                    let (dbx, dby) = (((fb >> 32) & 0x7FF) as u32, ((fb >> 43) & 0x7FF) as u32);
                    let disp = r.display[cidx];
                    let w = (((disp >> 32) & 0xFFF) + 1) as u32;
                    let h = (((disp >> 44) & 0x7FF) + 1) as u32;
                    Self::for_pages(psm, fbp, fbw, dbx, dby, (dbx + w).min(2048), (dby + h).min(2048), |p| {
                        mask[p >> 6] |= 1 << (p & 63);
                        true
                    });
                }
                let after = crate::raster::flight_using(self, &mask).unwrap_or(0);
                let job = ReadoutJob { regs: r, shared: sh, done, state: self.readout.clone() };
                crate::raster::submit_readout(self, job, mask, after);
                return true;
            }
        }
        crate::raster::flush_sync(self);
        let mut st = self.readout.lock().unwrap();
        readout(self, &r, &mut st, shared);
        false
    }
}

impl Default for Gs {
    fn default() -> Self {
        Gs::new()
    }
}

// ---- the GS thread and its EE-side front ----

/// Work for the GS thread, in stream order.
pub enum GsCmd {
    /// A general register write (GIF PACKED / REGLIST / A+D, HWREG).
    Reg(u32, u64),
    /// GIF IMAGE data for the host -> local transfer in progress.
    Image(Vec<u8>),
    /// CSR.RESET.
    Reset,
    /// Vertical blank: read the display out with these privileged registers.
    Vsync { pmode: u64, smode2: u64, dispfb: [u64; 2], display: [u64; 2], bgcolor: u64, field: u32 },
    /// Hand the local -> host transfer's data back.
    Download(std::sync::mpsc::Sender<Vec<u128>>),
}

/// Displayed pictures whose content differed from the one before, since start (the real frame
/// rate; the game re-copies an unchanged frame every field, so field / flip counts are not).
pub static PICTURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Displayed pictures whose content differed from the one before (per log period).
pub static NEW_PICTURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The GS thread's OS thread id (for the sampling profiler).
pub static GS_OS_TID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// One raster pool thread (for `T5_PROFILE_RASTER`).
pub static RASTER_OS_TID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Most commands queued before they go to the GS thread.
const BATCH: usize = 4096;
/// How many fields the EE side may run ahead of the GS thread.
const MAX_AHEAD: u64 = 2;

/// The EE side of the GS: privileged registers, register events, the command queue.
pub struct GsFront {
    pub pmode: u64,
    pub smode1: u64,
    pub smode2: u64,
    pub srfsh: u64,
    pub synch1: u64,
    pub synch2: u64,
    pub syncv: u64,
    pub dispfb: [u64; 2],
    pub display: [u64; 2],
    pub extbuf: u64,
    pub extdata: u64,
    pub extwrite: u64,
    pub bgcolor: u64,
    /// CSR, IMR, SIGLBLID: the EE thread and the MTVU worker (SIGNAL / FINISH / LABEL) share
    /// them.
    pub csr: std::sync::atomic::AtomicU64,
    pub imr: std::sync::atomic::AtomicU64,
    pub busdir: u64,
    pub siglblid: std::sync::atomic::AtomicU64,
    pub crt_interlace: u32,
    pub crt_mode: u32,
    pub crt_ffmd: u32,
    /// A SIGNAL / FINISH event raised the GS interrupt (taken by the GIF paths).
    pub irq: bool,
    /// Local -> host data for VIF1 / the GIF FIFO.
    pub download: std::collections::VecDeque<u128>,
    queue: Vec<GsCmd>,
    tx: std::sync::mpsc::Sender<Vec<GsCmd>>,
    sent_fields: u64,
    done_fields: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl GsFront {
    /// Start the GS thread; frames go to `shared`.
    pub fn new(shared: std::sync::Arc<FrameShared>) -> GsFront {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<GsCmd>>();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done_t = done.clone();
        std::thread::Builder::new()
            .name("gs".into())
            .spawn(move || {
                crate::boost_thread_on(Some(2));
                #[cfg(windows)]
                {
                    #[link(name = "kernel32")]
                    unsafe extern "system" {
                        fn GetCurrentThreadId() -> u32;
                    }
                    GS_OS_TID.store(unsafe { GetCurrentThreadId() }, std::sync::atomic::Ordering::Relaxed);
                }
                let mut gs = Gs::new();
                gs.enable_gpu();
                gs.job_ctx = Some((shared.clone(), done_t.clone()));
                let mut cap: Option<crate::gscap::Capture> = None;
                let mut armed = std::env::var_os("T5_GSCAPTURE").is_some();
                loop {
                    let batch = match rx.try_recv() {
                        Ok(b) => b,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                        Err(_) => {
                            crate::diag::tl("G idle");
                            let Ok(b) = rx.recv() else { break };
                            crate::diag::tl("G busy");
                            b
                        }
                    };
                    for c in batch {
                        if armed {
                            crate::gscap::on_cmd(&gs, &mut cap, &mut armed, &c);
                        }
                        gs.exec(c, &shared, &done_t);
                    }
                }
            })
            .expect("GS thread");
        GsFront {
            pmode: 0,
            smode1: 0,
            smode2: 0,
            srfsh: 0,
            synch1: 0,
            synch2: 0,
            syncv: 0,
            dispfb: [0; 2],
            display: [0; 2],
            extbuf: 0,
            extdata: 0,
            extwrite: 0,
            bgcolor: 0,
            csr: std::sync::atomic::AtomicU64::new(0x0000_0000_5500_0000 | 8),
            imr: std::sync::atomic::AtomicU64::new(0xFF00),
            busdir: 0,
            siglblid: std::sync::atomic::AtomicU64::new(0),
            crt_interlace: 1,
            crt_mode: 2,
            crt_ffmd: 0,
            irq: false,
            download: Default::default(),
            queue: Vec::with_capacity(BATCH),
            tx,
            sent_fields: 0,
            done_fields: done,
        }
    }

    fn push(&mut self, c: GsCmd) {
        self.queue.push(c);
        if self.queue.len() >= BATCH {
            self.flush();
        }
    }

    /// Send the queued commands to the GS thread.
    pub fn flush(&mut self) {
        if !self.queue.is_empty() {
            let batch = std::mem::replace(&mut self.queue, Vec::with_capacity(BATCH));
            let _ = self.tx.send(batch);
        }
    }

    /// A general register write from a GIF path.
    pub fn write_reg(&mut self, reg: u32, v: u64) {
        // T5_TBPWATCH=<tbp>: log which path / VU1 program sets TEX0 to that texture base.
        if reg & !1 == 6 && crate::diag::enabled("T5_TBPWATCH") {
            static TBP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            if v & 0x3FFF == *TBP.get_or_init(|| std::env::var("T5_TBPWATCH").ok().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX)) {
                let p = crate::gif::CUR_PATH.load(R);
                crate::diag::log(format!("[tbp] field {} vblank {} path {} kick {:#x} vu1 {:#x}", self.sent_fields, crate::VBLANKS.load(R), p >> 16, p & 0xFFFF, crate::gif::VU1_START.load(R)));
            }
        }
        match reg {
            0x60 => {
                // SIGNAL: ID bits under the mask; interrupt unless IMR.SIGMSK.
                let mask = (v >> 32) as u32;
                let id = v as u32;
                let sl = self.siglblid.load(R);
                let old = sl as u32;
                self.siglblid.store((sl & !0xFFFF_FFFF) | ((old & !mask) | (id & mask)) as u64, R);
                self.csr.fetch_or(1, R);
                if self.imr.load(R) & 0x100 == 0 {
                    self.irq = true;
                }
            }
            0x61 => {
                crate::diag::tl("FINISH");
                // FINISH: interrupt unless IMR.FINISHMSK.
                self.csr.fetch_or(2, R);
                if self.imr.load(R) & 0x200 == 0 {
                    self.irq = true;
                }
            }
            0x62 => {
                let mask = v >> 32;
                let id = v & 0xFFFF_FFFF;
                let sl = self.siglblid.load(R);
                let old = sl >> 32;
                self.siglblid.store((sl & 0xFFFF_FFFF) | (((old & !mask) | (id & mask)) << 32), R);
            }
            0x53 if v & 3 == 1 => {
                // Local -> host: the data must be there before the EE reads it.
                self.push(GsCmd::Reg(reg, v));
                let (dtx, drx) = std::sync::mpsc::channel();
                self.push(GsCmd::Download(dtx));
                self.flush();
                if let Ok(d) = drx.recv() {
                    self.download.extend(d);
                }
                self.busdir = 1;
            }
            _ => self.push(GsCmd::Reg(reg, v)),
        }
    }

    /// CSR.RESET from the MTVU worker.
    pub fn reset(&mut self) {
        self.push(GsCmd::Reset);
    }

    /// HWREG: image data, 64 bits.
    pub fn hwreg(&mut self, v: u64) {
        self.push(GsCmd::Reg(0x54, v));
    }

    /// GIF IMAGE data.
    pub fn image_data(&mut self, data: &[u8]) {
        if let Some(GsCmd::Image(d)) = self.queue.last_mut() {
            d.extend_from_slice(data);
            return;
        }
        self.push(GsCmd::Image(data.to_vec()));
    }

    pub fn download_qword(&mut self) -> u128 {
        self.download.pop_front().unwrap_or(0)
    }

    pub fn priv_read(&mut self, a: u32) -> u64 {
        match a & 0x1FF0 {
            0x1000 => self.csr_read(),
            0x1010 => self.imr.load(R),
            0x1040 => self.busdir,
            0x1080 => self.siglblid.load(R),
            0x0000 => self.pmode,
            _ => 0,
        }
    }

    pub fn csr_read(&self) -> u64 {
        // FIELD (bit 13) follows the displayed field; FIFO empty (bits 14-15 = 01).
        (self.csr.load(R) & !(0x3000 | 0xC000)) | 0x4000 | ((crate::machine().field as u64) << 13)
    }

    pub fn priv_write(&mut self, a: u32, v: u64) {
        match a & 0x1FF0 {
            0x0000 => self.pmode = v,
            0x0010 => self.smode1 = v,
            0x0020 => self.smode2 = v,
            0x0030 => self.srfsh = v,
            0x0040 => self.synch1 = v,
            0x0050 => self.synch2 = v,
            0x0060 => self.syncv = v,
            0x0070 => self.dispfb[0] = v,
            0x0080 => self.display[0] = v,
            0x0090 => self.dispfb[1] = v,
            0x00A0 => self.display[1] = v,
            0x00B0 => self.extbuf = v,
            0x00C0 => self.extdata = v,
            0x00D0 => self.extwrite = v,
            0x00E0 => self.bgcolor = v,
            0x1000 => {
                // Writing 1 clears SIGNAL / FINISH / HSINT / VSINT / EDWINT; RESET.
                self.csr.fetch_and(!(v & 0x1F), R);
                // RESET: the MTVU worker owns the command queue then (it gets a GsReset).
                if v & 0x200 != 0 && !crate::mtvu::active() {
                    self.push(GsCmd::Reset);
                }
            }
            0x1010 => self.imr.store(v, R),
            0x1040 => self.busdir = v,
            0x1080 => self.siglblid.store(v, R),
            _ => {}
        }
    }

    /// SetGsCrt(interlace, mode, field / frame).
    pub fn set_crt(&mut self, interlace: u32, mode: u32, ffmd: u32) {
        self.crt_interlace = interlace;
        self.crt_mode = mode;
        self.crt_ffmd = ffmd;
        crate::diag::log(format!("SetGsCrt(interlace {interlace}, mode {mode:#x}, ffmd {ffmd})"));
    }

    /// True if the GS raises its VSYNC interrupt (VSINT not masked in IMR).
    pub fn vsync_irq(&mut self) -> bool {
        self.csr.fetch_or(8, R);
        self.imr.load(R) & 0x800 == 0
    }

    /// The privileged registers the display read-out uses.
    pub fn display_regs(&self) -> (u64, u64, [u64; 2], [u64; 2], u64) {
        (self.pmode, self.smode2, self.dispfb, self.display, self.bgcolor)
    }

    /// MTVU worker: a vertical blank with the EE's display registers at that time.
    pub fn vsync_stream(&mut self, pmode: u64, smode2: u64, dispfb: [u64; 2], display: [u64; 2], bgcolor: u64, field: u32) {
        self.push(GsCmd::Vsync { pmode, smode2, dispfb, display, bgcolor, field });
        self.flush();
        self.sent_fields += 1;
        let _wait = crate::mtvu::WaitTimer(std::time::Instant::now(), &crate::mtvu::GSWAIT_US);
        crate::diag::tl("W vsync");
        if self.sent_fields - self.done_fields.load(std::sync::atomic::Ordering::Acquire) > MAX_AHEAD {
            crate::diag::tl("W gswait");
            while self.sent_fields - self.done_fields.load(std::sync::atomic::Ordering::Acquire) > MAX_AHEAD {
                std::thread::yield_now();
            }
            crate::diag::tl("W gsdone");
        }
    }

    /// Vertical blank: the GS thread reads the display out once it has drawn everything before.
    pub fn vblank_start(&mut self, field: u32) {
        self.push(GsCmd::Vsync { pmode: self.pmode, smode2: self.smode2, dispfb: self.dispfb, display: self.display, bgcolor: self.bgcolor, field });
        self.flush();
        self.sent_fields += 1;
        // Spin with yields: a Windows sleep lasts at least a timer tick (about 1 ms or more),
        // enough to miss the next field.
        while self.sent_fields - self.done_fields.load(std::sync::atomic::Ordering::Acquire) > MAX_AHEAD {
            std::thread::yield_now();
        }
    }
}

impl Gs {
    /// Run one command on the GS thread.
    /// `exec` for gs-bench.
    pub fn exec_pub(&mut self, c: GsCmd, shared: &FrameShared, done: &std::sync::atomic::AtomicU64) {
        self.exec(c, shared, done)
    }

    fn exec(&mut self, c: GsCmd, shared: &FrameShared, done: &std::sync::atomic::AtomicU64) {
        match c {
            GsCmd::Reg(r, v) => self.write_reg(r, v),
            GsCmd::Image(d) => {
                let t0 = std::time::Instant::now();
                self.image_data(&d);
                crate::raster::GS_US[3].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            GsCmd::Reset => {
                self.verts.clear();
                self.xfer_active = false;
            }
            GsCmd::Vsync { pmode, smode2, dispfb, display, bgcolor, field } => {
                self.pmode = pmode;
                self.smode2 = smode2;
                self.dispfb = dispfb;
                self.display = display;
                self.bgcolor = bgcolor;
                let t0 = std::time::Instant::now();
                let counted = self.vblank_start(shared, field);
                crate::raster::GS_US[4].fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                if !counted {
                    done.fetch_add(1, std::sync::atomic::Ordering::Release);
                    crate::diag::tl("GS fielddone");
                }
            }
            GsCmd::Download(tx) => {
                let _ = tx.send(self.download.drain(..).collect());
            }
        }
    }
}

/// Display registers at a vertical blank.
#[derive(Clone, Copy)]
pub struct DisplayRegs {
    pub pmode: u64,
    pub smode2: u64,
    pub dispfb: [u64; 2],
    pub display: [u64; 2],
    pub bgcolor: u64,
}

/// What the read-out keeps between fields.
#[derive(Default)]
pub struct ReadoutState {
    pub frame_serial: u64,
    pub last_dispfb: u64,
    last_hash: u64,
}

/// A display read-out raster worker 0 does (`Gs::vblank_start`).
pub struct ReadoutJob {
    pub regs: DisplayRegs,
    pub shared: std::sync::Arc<FrameShared>,
    pub done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub state: std::sync::Arc<Mutex<ReadoutState>>,
}

impl ReadoutJob {
    pub fn run(&self, g: &Gs) {
        let mut st = self.state.lock().unwrap();
        readout(g, &self.regs, &mut st, &self.shared);
        drop(st);
        self.done.fetch_add(1, std::sync::atomic::Ordering::Release);
        crate::diag::tl("GS fielddone");
    }
}

/// Read the display circuits out of local memory into the front end's frame.
pub fn readout(g: &Gs, dr: &DisplayRegs, st: &mut ReadoutState, shared: &FrameShared) {
        let en1 = dr.pmode & 1 != 0;
        let en2 = dr.pmode & 2 != 0;
        let circuits: Vec<usize> = [(en1, 0usize), (en2, 1)].iter().filter(|c| c.0).map(|c| c.1).collect();
        if circuits.is_empty() {
            return;
        }
        // Display size from the first enabled circuit (DISPLAY: DW, DH, MAGH).
        let disp = dr.display[circuits[0]];
        let magh = (((disp >> 23) & 0xF) + 1) as u32;
        let magv = (((disp >> 27) & 3) + 1) as u32;
        let mut w = ((((disp >> 32) & 0xFFF) + 1) as u32 / magh).max(1);
        let mut h = ((((disp >> 44) & 0x7FF) + 1) as u32 / magv).max(1);
        // SMODE2: INT (interlace), FFMD (1 = FRAME mode: each field reads every line of a
        // half-height buffer, shown line-doubled; 0 = FIELD mode: the buffer holds the whole
        // frame and the fields read alternate lines, shown here progressively).
        let ffmd = dr.smode2 & 2 != 0;
        let interlaced = dr.smode2 & 1 != 0;
        let halve = interlaced && ffmd;
        w = w.min(2048);
        h = h.min(2048);
        let mut out = vec![0u8; (w * h * 4) as usize];
        let bg = dr.bgcolor;
        // Rows are independent and only read local memory: all cores.
        use rayon::prelude::*;
        let gs: &Gs = g;
        out.par_chunks_mut((w * 4) as usize).enumerate().for_each(|(y, line)| {
            let y = y as u32;
            for x in 0..w {
                let mut px = [(bg & 0xFF) as u8, ((bg >> 8) & 0xFF) as u8, ((bg >> 16) & 0xFF) as u8, 0xFF];
                // Circuit 2 is the bottom layer, circuit 1 blends over it (MMOD / ALP).
                for &cidx in circuits.iter().rev() {
                    let fb = dr.dispfb[cidx];
                    let fbp = ((fb & 0x1FF) * 32) as u32;
                    let fbw = ((fb >> 9) & 0x3F) as u32;
                    let psm = ((fb >> 15) & 0x1F) as u32;
                    let dbx = ((fb >> 32) & 0x7FF) as u32;
                    let dby = ((fb >> 43) & 0x7FF) as u32;
                    let sy = if halve { dby + y / 2 } else { dby + y };
                    let raw = gs.read_pixel(psm, fbp, fbw, (dbx + x) & 2047, sy & 2047);
                    let (r, g, b, a) = match psm {
                        PSMCT16 | PSMCT16S => {
                            let c = raw;
                            (((c & 31) << 3) as u8, (((c >> 5) & 31) << 3) as u8, (((c >> 10) & 31) << 3) as u8, if c & 0x8000 != 0 { 0x80 } else { 0 })
                        }
                        _ => (raw as u8, (raw >> 8) as u8, (raw >> 16) as u8, (raw >> 24) as u8),
                    };
                    if cidx == 0 && circuits.len() == 2 {
                        // Circuit 1 over circuit 2: alpha from PMODE.ALP or the pixel.
                        let alpha = if dr.pmode & 0x20 != 0 { ((dr.pmode >> 8) & 0xFF) as u32 } else { (a as u32 * 2).min(255) };
                        let mix = |top: u8, bot: u8| ((top as u32 * alpha + bot as u32 * (255 - alpha)) / 255) as u8;
                        px = [mix(r, px[0]), mix(g, px[1]), mix(b, px[2]), 0xFF];
                    } else {
                        px = [r, g, b, 0xFF];
                    }
                }
                let o = (x * 4) as usize;
                line[o..o + 4].copy_from_slice(&px);
            }
        });
        st.frame_serial += 1;
        shared.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // New pictures (the content changed): what the player sees move.
        let mut ph: u64 = 0xcbf2_9ce4_8422_2325;
        for c in out.chunks_exact(4).step_by(61) {
            ph = (ph ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64).wrapping_mul(0x100_0000_01b3);
        }
        if ph != st.last_hash {
            st.last_hash = ph;
            NEW_PICTURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            PICTURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let fb = dr.dispfb[circuits[0]];
        if fb != st.last_dispfb {
            st.last_dispfb = fb;
            shared.flips.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut f = shared.frame.lock().unwrap();
        f.width = w;
        f.height = h;
        f.rgba = out;
        f.serial = st.frame_serial;
        crate::FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
