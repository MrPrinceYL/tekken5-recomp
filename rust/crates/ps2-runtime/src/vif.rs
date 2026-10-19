//! VIF0 / VIF1: the vector unit interfaces. They decode VIF codes from DMA channels 0 / 1 (or
//! their FIFOs) and load VU micro programs (MPG), unpack vertex data into VU data memory
//! (UNPACK with write cycles, masks and offset / difference modes), start micro programs
//! (MSCAL / MSCALF / MSCNT; VIF1 double-buffers its data area with BASE / OFST / TOPS) and pass
//! GIF packets to PATH2 (DIRECT / DIRECTHL, VIF1 only).
//!
//! Commands and their data may span transfers, so incoming words are queued and a command runs
//! once all its data has arrived. Micro programs run to completion when started, so FLUSH /
//! FLUSHE / FLUSHA never wait. A code with the interrupt bit stops the VIF (STAT.INT) and raises
//! the VIF interrupt until the program clears it (FBRST.STC).

use crate::{diag, Machine, INTC_VIF0};

pub struct Vif {
    pub n: usize,
    pub stat: u32,
    pub err: u32,
    pub mark: u32,
    pub cl: u32,
    pub wl: u32,
    pub mode: u32,
    pub num: u32,
    pub mask: u32,
    pub code: u32,
    pub itops: u32,
    pub itop: u32,
    pub base: u32,
    pub ofst: u32,
    pub tops: u32,
    pub top: u32,
    pub dbf: bool,
    pub row: [u32; 4],
    pub col: [u32; 4],
    pub mskpath3: bool,
    /// Stopped by an interrupt bit until FBRST.STC.
    pub stopped: bool,
    queue: Vec<u32>,
    /// `process` is running (data fed meanwhile is picked up by the running loop).
    busy: bool,
}

impl Vif {
    pub fn new(n: usize) -> Vif {
        Vif {
            n,
            stat: 0,
            err: 0,
            mark: 0,
            cl: 1,
            wl: 1,
            mode: 0,
            num: 0,
            mask: 0,
            code: 0,
            itops: 0,
            itop: 0,
            base: 0,
            ofst: 0,
            tops: 0,
            top: 0,
            dbf: false,
            row: [0; 4],
            col: [0; 4],
            mskpath3: false,
            stopped: false,
            queue: Vec::new(),
            busy: false,
        }
    }

    pub fn stalled(&self) -> bool {
        self.stopped
    }

    pub fn read(&self, a: u32) -> u32 {
        match a & 0x3F0 {
            0x000 => self.stat | ((self.dbf as u32) << 7) | ((self.stopped as u32) << 11),
            0x020 => self.err,
            0x030 => self.mark,
            0x040 => self.cl | (self.wl << 8),
            0x050 => self.mode,
            0x060 => self.num,
            0x070 => self.mask,
            0x080 => self.code,
            0x090 => self.itops,
            0x0A0 => self.base,
            0x0B0 => self.ofst,
            0x0C0 => self.tops,
            0x0D0 => self.itop,
            0x0E0 => self.top,
            0x100 => self.row[0],
            0x110 => self.row[1],
            0x120 => self.row[2],
            0x130 => self.row[3],
            0x140 => self.col[0],
            0x150 => self.col[1],
            0x160 => self.col[2],
            0x170 => self.col[3],
            _ => 0,
        }
    }
}

pub fn reg_write(m: &mut Machine, n: usize, a: u32, v: u32) {
    let vif = &mut m.vif[n];
    match a & 0x3F0 {
        0x010 => {
            // FBRST: RST, FBK, STP, STC
            if v & 1 != 0 {
                *vif = Vif::new(n);
            }
            if v & 8 != 0 {
                vif.stopped = false;
                vif.stat &= !(0x400 | 0x800);
                crate::dmac::kick(m);
                process(m, n);
            }
        }
        0x020 => vif.err = v,
        0x030 => {
            vif.mark = v & 0xFFFF;
            vif.stat &= !0x40;
        }
        0x100..=0x170 => {
            let i = ((a & 0x3F0) - 0x100) as usize / 16;
            if i < 4 {
                vif.row[i] = v;
            } else {
                vif.col[i - 4] = v;
            }
        }
        _ => {}
    }
}

/// DMA data for VIF `n`.
pub fn feed(m: &mut Machine, n: usize, data: &[u128]) {
    let q = &mut m.vif[n].queue;
    for d in data {
        q.push(*d as u32);
        q.push((*d >> 32) as u32);
        q.push((*d >> 64) as u32);
        q.push((*d >> 96) as u32);
    }
    process(m, n);
}

/// Words outside qwords (a DMA tag's upper half with TTE).
pub fn feed_words(m: &mut Machine, n: usize, words: &[u32]) {
    m.vif[n].queue.extend_from_slice(words);
    process(m, n);
}

pub fn fifo_write(m: &mut Machine, n: usize, v: u128) {
    feed(m, n, &[v]);
}

/// Words of data a command needs after its code word (None: unknown command).
fn data_words(vif: &Vif, code: u32) -> Option<usize> {
    let cmd = (code >> 24) & 0x7F;
    let num = (code >> 16) & 0xFF;
    let imm = code & 0xFFFF;
    Some(match cmd {
        0x20 => 1,
        0x30 | 0x31 => 4,
        0x4A => 2 * if num == 0 { 256 } else { num as usize },
        0x50 | 0x51 => 4 * if imm == 0 { 65536 } else { imm as usize },
        0x60..=0x7F => {
            let vn = (cmd >> 2) & 3;
            let vl = cmd & 3;
            let n = if num == 0 { 256 } else { num as usize };
            let bits = if vl == 3 { 16 } else { (32 >> vl) * (vn as usize + 1) };
            // Filling write (WL > CL) consumes data only for the CL positions of each cycle.
            let wl = if vif.wl == 0 { 256 } else { vif.wl as usize };
            let elems = if wl > vif.cl as usize {
                let cycles = n / wl;
                let rem = n % wl;
                cycles * vif.cl as usize + rem.min(vif.cl as usize)
            } else {
                n
            };
            (elems * bits).div_ceil(32)
        }
        0x00..=0x07 | 0x10 | 0x11 | 0x13 | 0x14 | 0x15 | 0x17 => 0,
        _ => return None,
    })
}

/// Run every complete command in the queue.
fn process(m: &mut Machine, n: usize) {
    if m.vif[n].busy {
        return;
    }
    m.vif[n].busy = true;
    // The queue is taken out while its commands run, so each command gets its data as a slice
    // of it (no copy per command); words fed meanwhile are appended afterwards.
    let queue = std::mem::take(&mut m.vif[n].queue);
    let mut pos = 0usize;
    loop {
        let vif = &m.vif[n];
        if vif.stopped || pos >= queue.len() {
            break;
        }
        let code = queue[pos];
        let Some(need) = data_words(vif, code) else {
            diag::once(format!("VIF{n}: unknown code {code:#010x}"));
            pos += 1;
            continue;
        };
        // MPG and DIRECT data start on a 64-bit / 128-bit boundary of the stream; the program
        // pads with NOPs, so the word position is already aligned.
        if pos + 1 + need > queue.len() {
            break;
        }
        let data = &queue[pos + 1..pos + 1 + need];
        pos += 1 + need;
        command(m, n, code, data);
        if code & 0x8000_0000 != 0 {
            // Interrupt bit: stop after this command and raise the VIF interrupt.
            let vif = &mut m.vif[n];
            vif.stopped = true;
            vif.stat |= 0x800 | 0x400;
            if crate::diag::enabled("T5_PRIMSTATS") {
                diag::count_with(|| format!("vif{n}_irq_stop"));
            }
            m.raise(INTC_VIF0 + n as u32);
        }
    }
    let mut queue = queue;
    let vif = &mut m.vif[n];
    let added = std::mem::take(&mut vif.queue);
    queue.drain(..pos.min(queue.len()));
    queue.extend_from_slice(&added);
    vif.queue = queue;
    vif.busy = false;
}

fn command(m: &mut Machine, n: usize, code: u32, data: &[u32]) {
    let cmd = (code >> 24) & 0x7F;
    let num = (code >> 16) & 0xFF;
    let imm = code & 0xFFFF;
    m.vif[n].code = code;
    match cmd {
        0x00 => {}
        0x01 => {
            m.vif[n].cl = imm & 0xFF;
            m.vif[n].wl = imm >> 8;
        }
        0x02 => {
            let v = &mut m.vif[n];
            v.ofst = imm & 0x3FF;
            v.dbf = false;
            v.tops = v.base;
        }
        0x03 => m.vif[n].base = imm & 0x3FF,
        0x04 => m.vif[n].itops = imm & 0x3FF,
        0x05 => m.vif[n].mode = imm & 3,
        0x06 => {
            let was = m.vif[1].mskpath3;
            m.vif[1].mskpath3 = imm & 0x8000 != 0;
            if crate::diag::enabled("T5_PRIMSTATS") {
                diag::count_with(|| format!("mskpath3_{}", imm & 0x8000 != 0));
            }
            if was && !m.vif[1].mskpath3 {
                crate::dmac::kick(m);
            }
        }
        0x07 => {
            m.vif[n].mark = imm;
            m.vif[n].stat |= 0x40;
        }
        0x10 | 0x11 | 0x13 => {}
        0x14 | 0x15 => mscal(m, n, Some(imm)),
        0x17 => mscal(m, n, None),
        0x20 => m.vif[n].mask = data[0],
        0x30 => m.vif[n].row.copy_from_slice(&data[..4]),
        0x31 => m.vif[n].col.copy_from_slice(&data[..4]),
        0x4A => {
            // MPG: num instructions (64-bit) to micro memory at imm * 8.
            if n == 0 { m.vu0.code_gen += 1 } else { m.vu1.code_gen += 1 }
            let code_mem = if n == 0 { &mut m.vu0.code } else { &mut m.vu1.code };
            let len = code_mem.len();
            let mut a = (imm as usize * 8) % len;
            for w in data {
                code_mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
                a = (a + 4) % len;
            }
        }
        0x50 | 0x51 => {
            if n == 1 {
                let q: Vec<u128> = data.chunks(4).map(|c| c[0] as u128 | (c[1] as u128) << 32 | (c[2] as u128) << 64 | (c[3] as u128) << 96).collect();
                crate::gif::path2(m, &q);
            }
        }
        0x60..=0x7F => unpack(m, n, cmd, num, imm, data),
        _ => {}
    }
}

/// MSCAL / MSCALF (start at `imm`) / MSCNT (continue): VIF1 double buffering, then run the VU.
fn mscal(m: &mut Machine, n: usize, imm: Option<u32>) {
    if n == 1 {
        let v = &mut m.vif[1];
        v.top = v.tops;
        v.itop = v.itops;
        v.dbf = !v.dbf;
        v.tops = if v.dbf { v.base + v.ofst } else { v.base };
    } else {
        let v = &mut m.vif[0];
        v.itop = v.itops;
    }
    let vu = if n == 0 { &mut m.vu0 } else { &mut m.vu1 };
    let pc = match imm {
        Some(i) => i,
        None => vu.tpc,
    };
    crate::vu::run(m, n, pc);
}

fn unpack(m: &mut Machine, n: usize, cmd: u32, num: u32, imm: u32, data: &[u32]) {
    let vn = (cmd >> 2) & 3;
    let vl = cmd & 3;
    let masked = cmd & 0x10 != 0;
    let usn = imm & 0x4000 != 0;
    let flg = imm & 0x8000 != 0;
    let vif = &m.vif[n];
    // WL = 0 means 256 (PCSX2 nVifUnpack); V4-5 ignores MODE.
    let (cl, wl, mode, mask_reg, row0, col) = (vif.cl, if vif.wl == 0 { 256 } else { vif.wl }, if vl == 3 && vn == 3 { 0 } else { vif.mode }, vif.mask, vif.row, vif.col);
    let fill = cl < wl;
    let qmask = if n == 0 { 0xFF } else { 0x3FF };
    let mut addr = (imm & 0x3FF) + if flg && n == 1 { vif.tops } else { 0 };
    let count = if num == 0 { 256 } else { num };
    let mut row = row0;
    // Bit reader over the data words.
    let mut bitpos = 0usize;
    let read_at = |pos: usize, bits: usize| -> u32 {
        let w = pos / 32;
        let sh = pos % 32;
        let lo = *data.get(w).unwrap_or(&0) as u64 | (*data.get(w + 1).unwrap_or(&0) as u64) << 32;
        let v = (lo >> sh) as u32;
        if bits == 32 { v } else { v & ((1u32 << bits) - 1) }
    };
    let ext = |v: u32, bits: usize| -> u32 {
        if bits == 32 || usn { v } else { ((v << (32 - bits)) as i32 >> (32 - bits)) as u32 }
    };
    let vu_data: &mut [u8] = if n == 0 { &mut m.vu0.data[..] } else { &mut m.vu1.data[..] };
    let mut cycle = 0u32;
    let mut written = 0u32;
    while written < count {
        let in_cycle = cycle % wl;
        // Filling write: the positions past CL write the next element without consuming it
        // (PCSX2 _nVifUnpackLoop).
        let consume = !fill || in_cycle < cl;
        // The element (x, y, z, w) from the data (V3 and V4 share the V4 logic: V3's w is the
        // next word, PCSX2 UNPACK_V4).
        let elem: [u32; 4] = if vl == 3 {
            let v = read_at(bitpos, 16);
            [(v & 0x1F) << 3, ((v >> 5) & 0x1F) << 3, ((v >> 10) & 0x1F) << 3, ((v >> 15) & 1) << 7]
        } else {
            let bits = 32 >> vl;
            let c = |k: usize| ext(read_at(bitpos + k * bits, bits), bits);
            match vn {
                0 => [c(0); 4],
                1 => [c(0), c(1), c(0), c(1)],
                _ => [c(0), c(1), c(2), c(3)],
            }
        };
        if consume {
            bitpos += if vl == 3 { 16 } else { (32 >> vl) * (vn as usize + 1) };
        }
        let have = [true; 4];
        let o = ((addr & qmask) as usize) * 16;
        let ri = in_cycle.min(3) as usize;
        for c in 0..4 {
            let msel = if masked { (mask_reg >> (ri * 8 + c * 2)) & 3 } else { 0 };
            let cur = u32::from_le_bytes(vu_data[o + 4 * c..o + 4 * c + 4].try_into().unwrap());
            let v = match msel {
                0 => {
                    if !have[c] {
                        cur
                    } else {
                        match mode {
                            1 => elem[c].wrapping_add(row[c]),
                            2 => {
                                row[c] = row[c].wrapping_add(elem[c]);
                                row[c]
                            }
                            3 => {
                                row[c] = elem[c];
                                row[c]
                            }
                            _ => elem[c],
                        }
                    }
                }
                1 => row[c],
                2 => col[ri],
                _ => cur,
            };
            vu_data[o + 4 * c..o + 4 * c + 4].copy_from_slice(&v.to_le_bytes());
        }
        written += 1;
        cycle += 1;
        // Skipping write (CL > WL) jumps over the unwritten qwords of each cycle.
        if !fill && wl < cl && cycle % wl == 0 {
            addr += cl - wl + 1;
        } else {
            addr += 1;
        }
    }
    m.vif[n].row = row;
    m.vif[n].num = 0;
}
