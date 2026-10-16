//! VU1 replay benchmark and parity check.
//!
//!   t5-headless with T5_VUCAPTURE=<second> writes vucap/run_NNNN.bin (VU1 program runs in
//!   sequence, with the state they started from and a hash of what they produced);
//!   `vu-bench [dir] [repeats]` replays them: every run must reproduce its hash (the translated
//!   micro code is bit-exact to the run that was captured), then the time per pair is measured.
use std::sync::atomic::Ordering::Relaxed;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| "vucap".into());
    let repeats: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("capture dir").filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.sort();
    let caps: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    if caps.is_empty() {
        eprintln!("no captures in {dir}");
        return;
    }
    let _shared = t5_recomp::boot();
    let m = ps2_runtime::machine();
    ps2_runtime::vu::BENCH.store(true, Relaxed);

    // Parity: every run must leave the captured hash.
    let mut bad = 0;
    for (i, c) in caps.iter().enumerate() {
        let h = ps2_runtime::vu::replay(m, c, true);
        if h != ps2_runtime::vu::capture_ref(c) {
            if bad < 10 {
                eprintln!("MISMATCH run {i} ({})", files[i].display());
            }
            bad += 1;
        }
    }
    println!("parity: {} of {} runs bit-exact", caps.len() - bad, caps.len());
    if std::env::var_os("VUBENCH_SHAPES").is_some() {
        ps2_runtime::vu::SHAPES_ON.store(true, Relaxed);
        for c in &caps {
            ps2_runtime::vu::replay(m, c, false);
        }
        ps2_runtime::vu::SHAPES_ON.store(false, Relaxed);
        let g = ps2_runtime::vu::SHAPES.lock().unwrap();
        // vu_dumps/shapes.txt: "block-key shape-key shape" per entry shape seen (tools/gen_vu.py
        // --collect merges them into sheets/vu_blocks.csv).
        {
            let _ = std::fs::create_dir_all("vu_dumps");
            let mut out = String::new();
            for ((_, sh), _) in g.as_ref().unwrap() {
                out.push_str(sh);
                out.push('\n');
            }
            let _ = std::fs::write("vu_dumps/shapes.txt", out);
        }
        let mut per: std::collections::HashMap<u32, Vec<(u64, String)>> = Default::default();
        for ((pc, sh), n) in g.as_ref().unwrap() {
            per.entry(*pc).or_default().push((*n, sh.clone()));
        }
        let mut v: Vec<_> = per.into_iter().collect();
        v.sort_by_key(|(_, l)| std::cmp::Reverse(l.iter().map(|x| x.0).sum::<u64>()));
        let total: u64 = v.iter().map(|(_, l)| l.iter().map(|x| x.0).sum::<u64>()).sum();
        let mut cum = 0;
        for (pc, mut l) in v.into_iter().take(25) {
            l.sort_by_key(|x| std::cmp::Reverse(x.0));
            let n: u64 = l.iter().map(|x| x.0).sum();
            cum += n;
            println!("pc {pc:#05x}: {n} entries ({:.0}% cum), {} shapes; top: {}", cum as f64 * 100.0 / total as f64, l.len(), l.iter().take(3).map(|(c, s)| format!("[{c}] {s}")).collect::<Vec<_>>().join(" | "));
        }
        return;
    }

    // VUBENCH_PROFILE=<seconds>: sample this thread while timing (repeats should cover it).
    #[cfg(windows)]
    let prof = std::env::var("VUBENCH_PROFILE").ok().and_then(|v| v.parse::<u64>().ok()).map(|secs| {
        t5_recomp::profile::mark_guest();
        std::thread::spawn(move || t5_recomp::profile::run(secs))
    });
    // Timing (no hashing).
    let s0 = ps2_runtime::vu::VU_STEPS[1].load(Relaxed);
    let t0 = std::time::Instant::now();
    for _ in 0..repeats {
        for c in &caps {
            ps2_runtime::vu::replay(m, c, false);
        }
    }
    let mut dt = t0.elapsed();
    // Subtract the state restore (measured separately, nothing run).
    ps2_runtime::vu::NORUN.store(true, Relaxed);
    let t1 = std::time::Instant::now();
    for _ in 0..repeats {
        for c in &caps {
            ps2_runtime::vu::replay(m, c, false);
        }
    }
    let restore = t1.elapsed();
    ps2_runtime::vu::NORUN.store(false, Relaxed);
    println!("restore overhead {:.1} ms (subtracted)", restore.as_secs_f64() * 1e3);
    dt = dt.saturating_sub(restore);
    let pairs = ps2_runtime::vu::VU_STEPS[1].load(Relaxed) - s0;
    // State restore (32 KB copy per run) is part of the time; it is small next to the runs.
    #[cfg(windows)]
    if let Some(p) = prof {
        if let Ok(h) = p.join() {
            t5_recomp::profile::report(h);
        }
    }
    println!(
        "{} runs x {repeats}: {:.1} ms, {} pairs, {:.2} ns/pair, {:.2} ms per {} runs",
        caps.len(),
        dt.as_secs_f64() * 1e3,
        pairs,
        dt.as_secs_f64() * 1e9 / pairs.max(1) as f64,
        dt.as_secs_f64() * 1e3 / repeats as f64,
        caps.len()
    );
}
