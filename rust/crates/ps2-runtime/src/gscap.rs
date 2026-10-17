//! GS capture / replay (performance and parity harness for the rasteriser, bin `gs-bench`).
//!
//! `T5_GSCAPTURE=<second>`: at the first vertical blank after that second of the run, the GS
//! thread writes `gscap/state.bin` (local memory, the last value written to every general
//! register, the CLUT) and then records its commands for `FIELDS` fields to `gscap/cmds.bin`.
//! `gs-bench` replays them on a fresh GS: it prints a hash of local memory after every field
//! (two replays with different rasteriser settings must agree) and the time per field.
//!
//! The replayed state is the captured one except for what no register holds (a vertex strip
//! or a transfer in progress at the start), so replays are compared with each other, not with
//! the original run.

use crate::gs::{FrameShared, Gs, GsCmd};
use std::io::{Read, Write};

/// Fields recorded.
const FIELDS: u32 = 120;

/// Registers whose write has an effect beyond the register (vertex kicks, transfers, events):
/// not replayed from the snapshot.
fn side_effect(r: u32) -> bool {
    matches!(r, 0x04 | 0x05 | 0x0C | 0x0D | 0x53 | 0x54 | 0x60 | 0x61 | 0x62)
}

pub struct Capture {
    out: std::io::BufWriter<std::fs::File>,
    fields: u32,
}

/// On the GS thread before each command: start / continue / stop the capture.
pub fn on_cmd(gs: &Gs, cap: &mut Option<Capture>, armed: &mut bool, c: &GsCmd) {
    if !*armed {
        return;
    }
    if cap.is_none() {
        let from: u64 = std::env::var("T5_GSCAPTURE").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
        let started = crate::secs() >= from as f64;
        if !started || !matches!(c, GsCmd::Vsync { .. }) {
            return;
        }
        let _ = std::fs::create_dir_all("gscap");
        let mut st = Vec::with_capacity(gs.mem.len() + 4096);
        st.extend_from_slice(&gs.mem);
        for r in 0..128usize {
            st.push(gs.reg_set[r] as u8);
            st.extend_from_slice(&gs.reg_last[r].to_le_bytes());
        }
        for v in gs.clut.iter() {
            st.extend_from_slice(&v.to_le_bytes());
        }
        st.extend_from_slice(&gs.clut_tex0.to_le_bytes());
        st.extend_from_slice(&gs.clut_hash.to_le_bytes());
        let _ = std::fs::write("gscap/state.bin", &st);
        let f = std::fs::File::create("gscap/cmds.bin").expect("gscap");
        *cap = Some(Capture { out: std::io::BufWriter::new(f), fields: 0 });
        crate::diag::log("GS capture started".into());
    }
    let k = cap.as_mut().unwrap();
    let o = &mut k.out;
    match c {
        GsCmd::Reg(r, v) => {
            let _ = o.write_all(&[1]);
            let _ = o.write_all(&r.to_le_bytes());
            let _ = o.write_all(&v.to_le_bytes());
        }
        GsCmd::Image(d) => {
            let _ = o.write_all(&[2]);
            let _ = o.write_all(&(d.len() as u32).to_le_bytes());
            let _ = o.write_all(d);
        }
        GsCmd::Reset => {
            let _ = o.write_all(&[3]);
        }
        GsCmd::Vsync { pmode, smode2, dispfb, display, bgcolor, field } => {
            let _ = o.write_all(&[4]);
            for v in [*pmode, *smode2, dispfb[0], dispfb[1], display[0], display[1], *bgcolor, *field as u64] {
                let _ = o.write_all(&v.to_le_bytes());
            }
            k.fields += 1;
            if k.fields > FIELDS {
                let _ = o.flush();
                *cap = None;
                *armed = false;
                crate::diag::log("GS capture written".into());
            }
        }
        GsCmd::Download(_) => {
            let _ = o.write_all(&[5]);
        }
    }
}

/// Replay `gscap/` (in `dir`) on a fresh GS: (hash of local memory after each field, wall time
/// per field in ms).
pub fn replay(dir: &str) -> Vec<(u64, f64)> {
    let st = std::fs::read(format!("{dir}/state.bin")).expect("state.bin");
    let mut cmds = Vec::new();
    std::fs::File::open(format!("{dir}/cmds.bin")).expect("cmds.bin").read_to_end(&mut cmds).unwrap();
    let mut gs = Gs::new();
    let n = gs.mem.len();
    gs.mem.copy_from_slice(&st[..n]);
    let mut o = n;
    for r in 0..128u32 {
        let set = st[o] != 0;
        let v = u64::from_le_bytes(st[o + 1..o + 9].try_into().unwrap());
        o += 9;
        if set && !side_effect(r) {
            gs.write_reg(r, v);
        }
    }
    for i in 0..256 {
        gs.clut[i] = u32::from_le_bytes(st[o..o + 4].try_into().unwrap());
        o += 4;
    }
    gs.clut_tex0 = u64::from_le_bytes(st[o..o + 8].try_into().unwrap());
    gs.clut_hash = u64::from_le_bytes(st[o + 8..o + 16].try_into().unwrap());
    gs.verts.clear();
    gs.pending_clut = None;
    gs.enable_gpu();
    let shared = FrameShared::default();
    let done = std::sync::atomic::AtomicU64::new(0);
    let mut out = Vec::new();
    let mut p = 0;
    let rd = |p: &mut usize, k: usize| -> &[u8] {
        let s = &cmds[*p..*p + k];
        *p += k;
        s
    };
    let mut t0 = std::time::Instant::now();
    while p < cmds.len() {
        let tag = cmds[p];
        p += 1;
        let c = match tag {
            1 => {
                let r = u32::from_le_bytes(rd(&mut p, 4).try_into().unwrap());
                let v = u64::from_le_bytes(rd(&mut p, 8).try_into().unwrap());
                GsCmd::Reg(r, v)
            }
            2 => {
                let len = u32::from_le_bytes(rd(&mut p, 4).try_into().unwrap()) as usize;
                GsCmd::Image(rd(&mut p, len).to_vec())
            }
            3 => GsCmd::Reset,
            4 => {
                let v: Vec<u64> = (0..8).map(|_| u64::from_le_bytes(rd(&mut p, 8).try_into().unwrap())).collect();
                GsCmd::Vsync { pmode: v[0], smode2: v[1], dispfb: [v[2], v[3]], display: [v[4], v[5]], bgcolor: v[6], field: v[7] as u32 }
            }
            _ => {
                let (tx, _rx) = std::sync::mpsc::channel();
                GsCmd::Download(tx)
            }
        };
        let vs = matches!(c, GsCmd::Vsync { .. });
        gs.exec_pub(c, &shared, &done);
        if vs {
            crate::raster::flush_sync(&mut gs);
            let dt = t0.elapsed().as_secs_f64() * 1e3;
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for c in gs.mem.chunks_exact(8) {
                h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x100_0000_01b3);
            }
            if std::env::var("GSBENCH_DUMP").ok().and_then(|v| v.parse::<usize>().ok()) == Some(out.len()) {
                let _ = std::fs::write(std::env::var("GSBENCH_DUMPFILE").unwrap_or_else(|_| "vram.bin".into()), &gs.mem[..]);
            }
            out.push((h, dt));
            t0 = std::time::Instant::now();
        }
    }
    out
}
