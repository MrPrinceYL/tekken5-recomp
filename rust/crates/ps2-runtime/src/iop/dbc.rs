//! Dbc_Manager (dbcman, RPC 0x80001300) with the DualShock 2 driver behind it, for libdbc 3.1 /
//! libpad2 in the Arcade History programs (tekken1 0x152CC0..0x155E10). Read from libdbc/libpad2:
//!
//! - 0x80001300: commands in a 0x90-byte buffer, the RPC function number is the command.
//!   0x80001363 GetModVersion (word 0, the library wants 0x31x), 0x80001304 SetWorkAddr (word 1 =
//!   EE work area), 0x80001305 GetIopDmabuff, 0x80001301 CreateSocket (word 0 type | 1, word 2
//!   port, word 3 slot, +0x28 / +0x2C the two DMA records; socket number back at +0x24),
//!   0x80001303 socket info (word 0 socket, result word 1, < 0 = not connected), 0x8000131A
//!   device command (word 0 socket, word 1 command, +8 length, +0xC data, result +0x8C).
//! - 0x8000131C, 0x8000131E/F (SendData2 per channel): bound by sceDbcInit, answered as is.
//! - Work area: one word per socket, 1 = connected (sceDbcGetConnection).
//! - Per socket two 0x80-byte records in EE memory, rewritten every frame alternately; libpad2
//!   reads the one with the larger frame number: +0x00 state (1 = stable), +0x02 data length,
//!   +0x03 profile length, +0x04 valid, +0x1C data then profile, +0x7C frame number.
//!   Data: the buttons in profile order, active low: byte 0 select, start, up, right, down,
//!   left, L2, R2; byte 1 L1, R1, triangle, circle, cross, square (tekken1 0x1393C0).
use super::{le, Reply};
use crate::{diag, pad, Machine};
use std::sync::atomic::Ordering::Relaxed;

pub const SERVERS: [u32; 4] = [0x8000_1300, 0x8000_131C, 0x8000_131E, 0x8000_131F];

#[derive(Default, Clone, Copy)]
pub struct Socket {
    pub port: usize,
    pub buf: u32,
}

#[derive(Default)]
pub struct Dbc {
    pub work: u32,
    pub sockets: Vec<Socket>,
    pub frame: u32,
    pub last_vblank: u64,
}

const STABLE: u8 = 1;

pub fn call(m: &mut Machine, sid: u32, fno: u32, d: &[u8], recv: usize) -> Reply {
    let mut r = d.to_vec();
    r.resize(recv.max(0x90), 0);
    let set = |r: &mut Vec<u8>, o: usize, v: u32| r[o..o + 4].copy_from_slice(&v.to_le_bytes());
    if sid != 0x8000_1300 {
        return Reply::Now(r);
    }
    match fno {
        0x8000_1363 => set(&mut r, 0, 0x0310),
        0x8000_1304 => {
            m.iop.dbc.work = le(d, 4);
            set(&mut r, 0, 1);
        }
        0x8000_1305 => {}
        0x8000_1301 => {
            let s = Socket { port: (le(d, 8) & 1) as usize, buf: le(d, 0x28) };
            diag::log(format!("dbc: socket {} port {} records {:#010x}", m.iop.dbc.sockets.len(), s.port, s.buf));
            m.iop.dbc.sockets.push(s);
            set(&mut r, 0x24, m.iop.dbc.sockets.len() as u32 - 1);
        }
        0x8000_1303 => {
            let n = le(d, 0) as usize;
            let ok = m.iop.dbc.sockets.get(n).is_some_and(|s| connected(m, s.port));
            set(&mut r, 4, if ok { n as u32 } else { (-12i32) as u32 });
        }
        0x8000_131A => {
            // Device query: the state is all the library asks for.
            let n = le(d, 0) as usize;
            let st = m.iop.dbc.sockets.get(n).is_some_and(|s| connected(m, s.port));
            set(&mut r, 8, 1);
            r[0xC] = if st { STABLE } else { 0 };
            set(&mut r, 0x8C, if st { 1 } else { (-1i32) as u32 });
        }
        _ => diag::once(format!("dbc: command {fno:#x}")),
    }
    Reply::Now(r)
}

fn connected(m: &Machine, port: usize) -> bool {
    m.shared.pad.connected[port].load(Relaxed)
}

/// Every vertical blank: the connection table and each socket's next record.
pub fn update(m: &mut Machine) {
    let vb = crate::VBLANKS.load(Relaxed);
    if vb == m.iop.dbc.last_vblank || m.iop.dbc.sockets.is_empty() {
        return;
    }
    m.iop.dbc.last_vblank = vb;
    m.iop.dbc.frame += 1;
    let frame = m.iop.dbc.frame;
    let work = m.iop.dbc.work;
    for (n, s) in m.iop.dbc.sockets.clone().into_iter().enumerate() {
        let on = connected(m, s.port);
        if work != 0 {
            crate::bus::ram_write(m, work + 4 * n as u32, 4, on as u64);
        }
        let buttons = m.shared.pad.buttons[s.port].load(Relaxed) as u32;
        let order = [
            [pad::SELECT, pad::START, pad::UP, pad::RIGHT, pad::DOWN, pad::LEFT, pad::L2, pad::R2],
            [pad::L1, pad::R1, pad::TRIANGLE, pad::CIRCLE, pad::CROSS, pad::SQUARE, 0, 0],
        ];
        let mut data = [0xFFu8; 2];
        for (i, bits) in order.iter().enumerate() {
            for (k, b) in bits.iter().enumerate() {
                if *b != 0 && buttons & *b as u32 != 0 {
                    data[i] &= !(1 << k);
                }
            }
        }
        let mut rec = [0u8; 0x80];
        rec[0] = if on { STABLE } else { 0 };
        rec[2] = data.len() as u8;
        rec[3] = 5;
        rec[4] = on as u8;
        rec[0x1C..0x1E].copy_from_slice(&data);
        // Profile: 14 digital buttons (bits 0..13).
        rec[0x1E..0x23].copy_from_slice(&[0xFF, 0x3F, 0, 0, 0]);
        rec[0x7C..0x80].copy_from_slice(&frame.to_le_bytes());
        let addr = s.buf + 0x80 * (frame & 1);
        for (i, b) in rec.iter().enumerate() {
            crate::bus::ram_write(m, addr + i as u32, 1, *b as u64);
        }
    }
}
