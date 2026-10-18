//! padman (RPC 0x80000100 / 0x80000101) for libpad 3.0 (SLUS 0x0010B900..0x0010CE68): commands
//! in a 0x80-byte buffer (word 0 command, +4 port, +8 slot, +0x10 argument; the result word
//! depends on the command, see the end of `call`), and per open port two 0x80-byte `pad_data` records in EE memory that padman
//! rewrites every frame, alternating, with a growing frame count (libpad reads the newer one).
//!
//! pad_data: +0x00 data[32] (the controller's reply: 0x00, mode id, buttons (active low),
//! sticks, pressures), +0x30 actuator data, +0x50 mode table, +0x58 frame, +0x60 data length,
//! +0x64 modeConfig, +0x65 modeCurId, +0x66 model, +0x67 buttonDataReady, +0x68 nrOfModes,
//! +0x69 modeCurOffs, +0x6A nrOfActuators, +0x6B numActComb, +0x6D mode, +0x6E lock,
//! +0x70 state (6 = stable), +0x71 request state (0 = complete), +0x72 current task (1).
//! The controller is a DualShock 2: digital at power-on, analog / pressure when the game asks.

use super::{le, Reply};
use crate::{diag, pad, Machine};
use std::sync::atomic::Ordering::Relaxed;

#[derive(Default, Clone, Copy)]
pub struct Port {
    pub open: bool,
    pub buf: u32,
    pub frame: u32,
    /// 0x41 digital, 0x73 analog, 0x79 analog with pressure.
    pub mode_id: u8,
    pub lock: u8,
    pub pressure: bool,
}

#[derive(Default)]
pub struct Pads {
    pub ports: [Port; 2],
    pub last_vblank: u64,
}

pub fn call(m: &mut Machine, sid: u32, d: &[u8], recv: usize) -> Reply {
    let mut r = d.to_vec();
    r.resize(recv.max(0x80), 0);
    let set = |r: &mut Vec<u8>, o: usize, v: u32| r[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let cmd = le(d, 0);
    let port = (le(d, 4) & 1) as usize;
    let mut result = 1u32;
    if sid == 0x8000_0100 {
        match cmd {
            0x01 => {
                let p = &mut m.iop.pad.ports[port];
                p.open = true;
                p.buf = le(d, 0x10);
                p.frame = 0;
                if p.mode_id == 0 {
                    p.mode_id = 0x41;
                }
                diag::log(format!("pad: port {port} open, buffer {:#010x}", p.buf));
                set(&mut r, 0x14, 0x0001_8000 + 0x100 * port as u32);
            }
            0x06 => {
                // SetMainMode(mode, lock): 0 digital, 1 analog.
                let p = &mut m.iop.pad.ports[port];
                p.mode_id = if le(d, 0x0C) == 1 { 0x73 } else { 0x41 };
                p.lock = le(d, 0x10) as u8;
            }
            0x09 => result = 0x3FFFF,
            0x0A => {
                // SetButtonInfo: pressure mode on / off.
                let p = &mut m.iop.pad.ports[port];
                p.pressure = le(d, 0x0C) != 0;
                if p.mode_id != 0x41 {
                    p.mode_id = if p.pressure { 0x79 } else { 0x73 };
                }
            }
            0x0C => result = 2,
            0x0D => result = 1,
            0x0E => m.iop.pad.ports[port].open = false,
            0x0F | 0x10 => {}
            0x12 => result = 0x400,
            _ => diag::once(format!("pad: command {cmd:#x}")),
        }
    }
    // Where libpad reads each command's result (main 0x10BD58.., devil 0x2615A0..).
    let at = match cmd {
        0x06 | 0x08 => 0x14,
        0x0A => 0x10,
        0x0B => 0x1C,
        0x14 => 0x08,
        _ => 0x0C,
    };
    set(&mut r, at, result);
    Reply::Now(r)
}

/// Every vertical blank: write each open port's next pad_data record.
pub fn update(m: &mut Machine) {
    let vb = crate::VBLANKS.load(Relaxed);
    if vb == m.iop.pad.last_vblank {
        return;
    }
    m.iop.pad.last_vblank = vb;
    for n in 0..2 {
        let p = m.iop.pad.ports[n];
        if !p.open {
            continue;
        }
        let frame = p.frame + 1;
        m.iop.pad.ports[n].frame = frame;
        let shared = &m.shared.pad;
        let blocked = n == 0 && crate::pad::BLOCK.load(Relaxed);
        let buttons = if blocked { 0 } else { shared.buttons[n].load(Relaxed) as u32 };
        let sticks = if blocked { 0 } else { shared.sticks[n].load(Relaxed) };
        let connected = shared.connected[n].load(Relaxed);
        let mut rec = [0u8; 0x80];
        let mode = p.mode_id;
        let mut data = [0u8; 32];
        data[0] = 0;
        data[1] = mode;
        let raw = !buttons & 0xFFFF;
        data[2] = raw as u8;
        data[3] = (raw >> 8) as u8;
        let st = if sticks == 0 { 0x8080_8080 } else { sticks };
        data[4..8].copy_from_slice(&st.to_le_bytes());
        // Pressures: right, left, up, down, triangle, circle, cross, square, L1, R1, L2, R2.
        let order = [pad::RIGHT, pad::LEFT, pad::UP, pad::DOWN, pad::TRIANGLE, pad::CIRCLE, pad::CROSS, pad::SQUARE, pad::L1, pad::R1, pad::L2, pad::R2];
        for (i, b) in order.iter().enumerate() {
            data[8 + i] = if buttons & *b as u32 != 0 { 0xFF } else { 0 };
        }
        let len = match mode { 0x41 => 4, 0x73 => 8, _ => 20 };
        rec[..32].copy_from_slice(&data);
        // Actuators: two (small: on/off, large: 0..255).
        rec[0x30..0x38].copy_from_slice(&[0, 1, 1, 1, 1, 1, 0, 1]);
        rec[0x50..0x54].copy_from_slice(&[4, 0, 7, 0]);
        rec[0x58..0x5C].copy_from_slice(&frame.to_le_bytes());
        rec[0x60..0x64].copy_from_slice(&(len as u32).to_le_bytes());
        // modeConfig: padman writes 2 when the controller answers the config-mode query
        // (DualShock 2), 1 after ten failed tries. libpad's InfoMode(CUREXID) returns 0 for 1, and
        // the game (0x239658) then never asks for analog mode, so the sticks stay at 0x80.
        rec[0x64] = 2;
        rec[0x65] = mode;
        rec[0x66] = 3; // model: DualShock 2
        rec[0x67] = 1;
        rec[0x68] = 2;
        rec[0x69] = if mode == 0x41 { 0 } else { 1 };
        rec[0x6A] = 2;
        rec[0x6B] = 1;
        rec[0x6D] = if mode == 0x41 { 0 } else { 1 };
        rec[0x6E] = p.lock;
        rec[0x70] = if connected { 6 } else { 0 };
        rec[0x71] = 0;
        rec[0x72] = 1; // currentTask: updating (libpad's InfoMode / InfoAct require it)
        let addr = p.buf + 0x80 * (frame & 1);
        for (i, b) in rec.iter().enumerate() {
            crate::bus::ram_write(m, addr + i as u32, 1, *b as u64);
        }
    }
}
