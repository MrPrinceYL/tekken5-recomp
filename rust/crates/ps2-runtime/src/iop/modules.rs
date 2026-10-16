//! IOP modules (IRX) the game loads: identification and, for now, bookkeeping. Sony driver
//! modules whose services are emulated at the RPC level (padman, sio2man, mcman, mcserv) need
//! nothing more; Namco's own modules are run from their recompiled code (see CLAUDE.md, work in
//! progress).

use crate::{diag, Machine};

/// The module name in an IRX image's .iopmod section (in IOP RAM at `addr`).
pub fn irx_name(m: &Machine, addr: u32) -> Option<String> {
    let img = m.iop.read(addr, 0x40000);
    if &img[..4] != b"\x7fELF" {
        return None;
    }
    let le32 = |o: usize| u32::from_le_bytes(img[o..o + 4].try_into().unwrap()) as usize;
    let le16 = |o: usize| u16::from_le_bytes([img[o], img[o + 1]]) as usize;
    let (shoff, shnum) = (le32(32), le16(48));
    for i in 0..shnum {
        let s = shoff + 40 * i;
        if s + 40 > img.len() {
            break;
        }
        if le32(s + 4) == 0x7000_0080 {
            let off = le32(s + 16);
            let name = &img[off + 0x1A..];
            let end = name.iter().position(|&b| b == 0).unwrap_or(0);
            return Some(String::from_utf8_lossy(&name[..end]).to_string());
        }
    }
    None
}

/// sceSifLoadModuleBuffer: returns the module id.
pub fn load_buffer(m: &mut Machine, addr: u32) -> i32 {
    let name = irx_name(m, addr).unwrap_or_else(|| "?".into());
    m.iop.modules.push(name.clone());
    let _ = super::exec::load(m, addr, &name);
    let id = m.iop.modules.len() as i32 + 1;
    diag::log(format!("IOP: load module {name} from {addr:#010x} -> id {id}"));
    id
}
