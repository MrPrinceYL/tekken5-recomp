//! Diagnostics: a log (stderr and `t5_trace.log`), counters of unusual events, and a periodic
//! summary of what the machine is doing.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static COUNTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
/// Host time (us) by activity: [EE threads, IOP, idle sleep, interrupt handlers].
pub static TIME: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

static ONCE: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

/// Open the trace file (next to the executable's working directory).
pub fn init(path: &str) {
    if let Ok(f) = std::fs::File::create(path) {
        *LOG.lock().unwrap() = Some(f);
    }
}

pub fn log(s: String) {
    let ms = crate::machine_opt().map(|m| m.start.elapsed().as_millis()).unwrap_or(0);
    let line = format!("[{:>8}] {s}", ms);
    eprintln!("{line}");
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
    }
}

/// Log a message only the first time it is seen.
pub fn once(s: String) {
    let mut g = ONCE.lock().unwrap();
    let set = g.get_or_insert_with(Default::default);
    if set.insert(s.clone()) {
        drop(g);
        log(s);
    }
}

/// Count an event (shown in the periodic summary).
/// Whether event counters are kept (the headless runner prints them; normal play does not, so
/// the counters cost nothing there).
pub static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn counting() -> bool {
    COUNTING.load(Relaxed) || enabled("T5_PRIMSTATS") || enabled("T5_FLUSHSTATS")
}

/// `T5_TIMELINE=1`: time-stamped pipeline events (EE / MTVU worker / GS thread) in the log.
#[inline]
pub fn tl(ev: &str) {
    if enabled("T5_TIMELINE") {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let us = T0.get_or_init(std::time::Instant::now).elapsed().as_micros();
        log(format!("TL {us} {ev}"));
    }
}

/// Add `n` to an event counter.
pub fn add(what: &str, n: u64) {
    if counting() {
        *COUNTS.lock().unwrap().entry(what.to_string()).or_insert(0) += n;
    }
}

pub fn count(what: &str) {
    if counting() {
        *COUNTS.lock().unwrap().entry(what.to_string()).or_insert(0) += 1;
    }
}

/// `count` with a name that is only built when counting.
#[inline]
pub fn count_with(what: impl FnOnce() -> String) {
    if counting() {
        count(&what());
    }
}

/// Periodic summary (every few seconds of guest time).
pub fn summary(m: &crate::Machine) {
    if enabled("T5_VUSHAPES") {
        crate::vu::dump_shapes();
    }
    let counts: Vec<String> = COUNTS.lock().unwrap().iter().map(|(k, v)| format!("{k}={v}")).collect();
    let t: Vec<u64> = TIME.iter().map(|x| x.swap(0, Relaxed) / 1000).collect();
    if enabled("T5_COVER") {
        ee_core::COVER_ON.store(true, Relaxed);
        let hit = ee_core::cover_take();
        let line: Vec<String> = hit.iter().map(|a| format!("{a:08X}")).collect();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("cover.txt") {
            use std::io::Write;
            let _ = writeln!(f, "{} {}", m.now() / 294_912, line.join(" "));
        }
    }
    log(format!("GS display: pmode {:#x} dispfb1 {:#x} display1 {:#x} dispfb2 {:#x} display2 {:#x} smode2 {:#x} bg {:#x}", m.gs.pmode, m.gs.dispfb[0], m.gs.display[0], m.gs.dispfb[1], m.gs.display[1], m.gs.smode2, m.gs.bgcolor));
    log(format!("VU pairs in last period: VU0 {} VU1 {} (cycles {} / {}) | XGKICK {} ms ({} kicks)", crate::vu::VU_STEPS[0].swap(0, Relaxed), crate::vu::VU_STEPS[1].swap(0, Relaxed), crate::vu::VU_CYCLES[0].swap(0, Relaxed), crate::vu::VU_CYCLES[1].swap(0, Relaxed), crate::vu::XGKICK_NS.swap(0, Relaxed) / 1_000_000, crate::vu::XGKICKS.swap(0, Relaxed)));
    if let Some(t) = m.mtvu.as_ref() {
        log(format!("{} | VIF1 stopped {} stat {:#x} | DMA ch1 chcr {:#x} ch2 chcr {:#x} | istat {:#x} imask {:#x}", t.status(), m.vif[1].stopped, m.vif[1].stat, m.dmac.ch[1].chcr, m.dmac.ch[2].chcr, m.istat, m.imask));
    }
    {
        use crate::raster::{FLUSHES, GS_US};
        let g: Vec<u64> = GS_US.iter().map(|a| a.swap(0, Relaxed) / 1000).collect();
        let fl: Vec<u64> = FLUSHES.iter().map(|a| a.swap(0, Relaxed)).collect();
        let ku: Vec<u64> = crate::raster::KIND_US.iter().map(|a| a.swap(0, Relaxed) / 1000).collect();
        let kp: Vec<u64> = crate::raster::KIND_PX.iter().map(|a| a.swap(0, Relaxed)).collect();
        log(format!("raster by kind (ms / Mpx): tri {} / {} sprite {} / {} other {}", ku[3] + ku[4] + ku[5], kp[4] / 1_000_000, ku[6], kp[6] / 1_000_000, ku[0] + ku[1] + ku[2]));
        log(format!("GS thread ms: queue {} flush-par {} flush-serial {} image {} readout {} | flushes par {} serial {} prims {} | tile work ms {} tiles {} covered px {}", g[0], g[1], g[2], g[3], g[4], fl[0], fl[1], fl[2], crate::raster::TILE_US.swap(0, Relaxed) / 1000, crate::raster::TILES.swap(0, Relaxed), crate::raster::COVERED.swap(0, Relaxed)));
    }
    log(format!("host ms in last period: EE {} IOP {} idle {} irq {} (VU {} GS draw {}) prims {} bbox pixels {} game frames {} drawn frames {} NEW PICTURES {} GAME FRAMES {} LOGIC FRAMES {} worker busy ms {} (stall wait {} GS wait {})", t[0], t[1], t[2], t[3], t[4], t[5], crate::raster::PRIMS.swap(0, Relaxed), crate::raster::PIXELS.swap(0, Relaxed), m.shared.frames.flips(), crate::raster::GAME_FRAMES.swap(0, Relaxed), crate::gs::NEW_PICTURES.swap(0, Relaxed), crate::raster::RENDERED.swap(0, Relaxed), crate::logic_frames_delta(m), crate::mtvu::BUSY_US.swap(0, Relaxed) / 1000, crate::mtvu::STALL_US.swap(0, Relaxed) / 1000, crate::mtvu::GSWAIT_US.swap(0, Relaxed) / 1000));
    log(format!(
        "vblanks {} frames {} fn {} calls {} unknown {} midblock {} last fn {:#010x} prog {} threads {} sif q{} armed {} buf {:#x} | {}",
        crate::VBLANKS.load(Relaxed),
        crate::FRAMES.load(Relaxed),
        ee_core::FN_ENTRIES.load(Relaxed),
        ee_core::CALLS.load(Relaxed),
        ee_core::UNKNOWN_TARGETS.load(Relaxed),
        ee_core::BLOCK_MISSES.load(Relaxed),
        ee_core::LAST_FN.load(Relaxed),
        m.program().map(|p| p.name).unwrap_or("?"),
        m.kernel.thread_summary(),
        m.sif.queue.len(),
        m.sif.armed,
        m.sif.ee_pktbuf,
        counts.join(" ")
    ));
    let it: Vec<String> = m.iop.exec.threads.iter().flatten().map(|t| format!("{}:{}{}p{}e{:x}{:?}", t.id, if t.dormant { "D" } else if t.ready { "R" } else { "W" }, "", t.prio, t.entry, t.wait)).collect();
    log(format!("IOP threads: {} | cd busy until {} (now {}) | iop last fn {:#x} loop {:#x}", it.join(" "), m.iop.exec.cd_busy_until, m.now(), iop_core::LAST_FN.load(Relaxed), 0));
    let c = &m.ctx;
    let chain: Vec<String> = (0..12u32)
        .map(|i| c.trail[(c.depth.wrapping_sub(i) & 63) as usize])
        .take(c.depth.min(12) as usize)
        .map(|t| format!("{t:08x} {}", m.function_name(t).unwrap_or("?")))
        .collect();
    let names = ["zero","at","v0","v1","a0","a1","a2","a3","t0","t1","t2","t3","t4","t5","t6","t7","s0","s1","s2","s3","s4","s5","s6","s7","t8","t9","k0","k1","gp","sp","fp","ra"];
    let regs: Vec<String> = (1..32).map(|i| format!("{}={:x}", names[i], c.gpr32(i))).collect();
    log(format!("EE regs: {}", regs.join(" ")));
    // T5_DUMP=addr:len[,addr:len]: EE memory words in each summary (debugging).
    if let Ok(spec) = std::env::var("T5_DUMP") {
        for part in spec.split(',') {
            let (a, n) = part.split_once(':').unwrap_or((part, "64"));
            let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).unwrap_or(0);
            let n: u32 = n.parse().unwrap_or(64);
            let words: Vec<String> = (0..n / 4).map(|i| format!("{:08x}", crate::bus::ram_read(m, a + 4 * i, 4) as u32)).collect();
            log(format!("dump {a:#010x}: {}", words.join(" ")));
        }
    }
    let lp = ee_core::LAST_LOOP.load(Relaxed);
    log(format!("EE last loop {lp:#010x} ({}), call chain (innermost first, depth {}): {}", m.function_name(lp).unwrap_or("?"), c.depth, chain.join(" < ")));
}

/// Whether the diagnostic switch `var` is set in the environment. The set `T5_*` names are read
/// once; a lookup is a lock-free scan of that short list (it runs on hot paths).
pub fn enabled(var: &'static str) -> bool {
    static SET: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let set = SET.get_or_init(|| {
        std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            // T5_DISC / T5_MEMCARD / T5_WAVDUMP are settings, not diagnostics.
            .filter(|k| k.starts_with("T5_") && !matches!(k.as_str(), "T5_DISC" | "T5_MEMCARD" | "T5_WAVDUMP"))
            .collect()
    });
    !set.is_empty() && set.iter().any(|k| k == var)
}
