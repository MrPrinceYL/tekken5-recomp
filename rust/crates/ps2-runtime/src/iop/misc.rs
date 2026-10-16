//! Smaller IOP services: the IOP heap / module loader, controllers (padman), memory cards
//! (mcserv) and sound (sdrdrv).

use super::{le, Kind, Reply};
use crate::{diag, Machine};

#[derive(Default)]
pub struct Misc {
    pub next_event: u64,
}

fn word(recv: usize, v: u32) -> Vec<u8> {
    let mut r = vec![0u8; recv.max(4)];
    r[0..4].copy_from_slice(&v.to_le_bytes());
    r
}

/// IOP heap (0x80000003) and module loader (0x80000006).
pub fn heap_call(m: &mut Machine, kind: Kind, fno: u32, d: &[u8], recv: usize) -> Reply {
    match (kind, fno) {
        (Kind::IopHeap, 1) => {
            // sceSifAllocIopHeap(size): AllocSysMemory(lowest fit) on the IOP (the same heap the
            // recompiled modules allocate from), 0 if it does not fit.
            let a = m.iop.exec.alloc(0, le(d, 0), 0);
            diag::log(format!("IOP heap: alloc {:#x} -> {a:#x} (max free {:#x})", le(d, 0), m.iop.exec.max_free()));
            if a != 0 {
                m.iop.ee_heap.push(a);
            }
            Reply::Now(word(recv, a))
        }
        (Kind::IopHeap, 2) => {
            // sceSifFreeIopHeap(addr): 0, or -1 if it was not allocated.
            let a = le(d, 0);
            m.iop.ee_heap.retain(|&x| x != a);
            diag::log(format!("IOP heap: free {a:#x}"));
            let ok = m.iop.exec.free(a);
            Reply::Now(word(recv, if ok { 0 } else { u32::MAX }))
        }
        (Kind::IopHeap, 4) => {
            // AllocSysMemory(type, size, address): 0 lowest, 1 highest, 2 at the address.
            let a = m.iop.exec.alloc(le(d, 4), le(d, 0), le(d, 8));
            diag::log(format!("IOP heap: AllocSysMemory({}, {:#x}, {:#x}) -> {a:#x}", le(d, 4), le(d, 0), le(d, 8)));
            if a != 0 {
                m.iop.ee_heap.push(a);
            }
            Reply::Now(word(recv, a))
        }
        (Kind::IopHeap, 5) => {
            let f = m.iop.exec.max_free();
            diag::log(format!("IOP heap: QueryMaxFreeMemSize -> {f:#x}"));
            Reply::Now(word(recv, f))
        }
        (Kind::IopHeap, 6) => Reply::Now(word(recv, m.iop.exec.total_free())),
        (Kind::LoadFile, 0xFF) => {
            // Version tag checked by libkernl (SLUS 0x00117A68).
            let mut r = vec![0u8; recv.max(4)];
            r[0..4].copy_from_slice(b"3000");
            Reply::Now(r)
        }
        (Kind::LoadFile, 6) => {
            // sceSifLoadModuleBuffer { +0 IOP address of the IRX image, +4 argument length,
            // +0x104 arguments }; reply { module id, module result }.
            let addr = le(d, 0);
            let id = super::modules::load_buffer(m, addr);
            let mut r = vec![0u8; recv.max(8)];
            r[0..4].copy_from_slice(&id.to_le_bytes());
            Reply::Now(r)
        }
        (Kind::IopHeap, 3) | (Kind::LoadFile, _) => {
            let path = String::from_utf8_lossy(&d[..d.iter().position(|&b| b == 0).unwrap_or(d.len()).min(256)]).to_string();
            diag::log(format!("IOP: {kind:?} fn {fno} ({path})"));
            m.iop.modules.push(path);
            Reply::Now(word(recv, 0))
        }
        _ => {
            diag::once(format!("IOP: {kind:?} fn {fno:#x}"));
            Reply::Now(word(recv, 0))
        }
    }
}

pub fn sdr_call(_m: &mut Machine, fno: u32, _d: &[u8], recv: usize) -> Reply {
    diag::count("sdr calls");
    diag::once(format!("sdr: fn {fno:#x}"));
    Reply::Now(word(recv, 0))
}

pub fn update(_m: &mut Machine, _now: u64) {}

pub fn next_event(m: &Machine) -> u64 {
    if m.iop.misc.next_event == 0 { u64::MAX } else { m.iop.misc.next_event }
}
