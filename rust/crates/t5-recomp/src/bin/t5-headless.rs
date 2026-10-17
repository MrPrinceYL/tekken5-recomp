//! Headless runner for automated checks: boots the game without a window or input, writes the
//! displayed frame as a PNG every second and stops after the given time.
//!
//! Usage: t5-headless [seconds] [out_dir] [--press BUTTON@SECOND ...]
//! (BUTTON: START, CROSS, CIRCLE, UP, DOWN, LEFT, RIGHT; held for half a second.)
//! Seconds are device time: `T5_WARP=1` (or ranges, `ps2_runtime::warp_on`) skips idle time.

use std::sync::atomic::Ordering::Relaxed;

fn main() {
    // T5_NOCOUNT=1: no event counters (measure as in normal play).
    ps2_runtime::diag::COUNTING.store(std::env::var_os("T5_NOCOUNT").is_none(), std::sync::atomic::Ordering::Relaxed);
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let out = args.get(2).cloned().unwrap_or_else(|| "headless".into());
    let _ = std::fs::create_dir_all(&out);
    let shared = t5_recomp::boot();
    std::thread::Builder::new()
        .name("ps2-ee".into())
        .stack_size(512 << 20)
        .spawn(|| {
            #[cfg(windows)]
            t5_recomp::profile::mark_guest();
            t5_recomp::run()
        })
        .expect("spawn guest thread");
    t5_recomp::script::init(&args);
    let pad = shared.pad.clone();
    std::thread::spawn(move || loop {
        pad.buttons[0].store(t5_recomp::script::held(), std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    // T5_PROFILE=<start>: sample the guest thread from that second to the end (Windows only).
    #[cfg(windows)]
    let prof = std::env::var("T5_PROFILE").ok().and_then(|v| v.parse::<u64>().ok()).map(|start| {
        let span = secs.saturating_sub(start).max(1);
        std::thread::spawn(move || {
            wait_secs(start as f64);
            t5_recomp::profile::run(span)
        })
    });
    // T5_BURST=<second>: also save every displayed frame for one second from then on (to see
    // the frame-to-frame output the front end gets).
    if let Some(start) = std::env::var("T5_BURST").ok().and_then(|v| v.parse::<u64>().ok()) {
        let frames = shared.frames.clone();
        let out = out.clone();
        std::thread::spawn(move || {
            wait_secs(start as f64);
            let mut seen = 0;
            let mut buf = ps2_runtime::gs::Frame::default();
            let end = start as f64 + 1.0;
            let mut i = 0;
            let mut kept = Vec::new();
            while ps2_runtime::secs() < end {
                let n = frames.take_if_new(seen, &mut buf);
                if n != seen {
                    seen = n;
                    kept.push((n, buf.width, buf.height, buf.rgba.clone()));
                    i += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            for (k, (n, w, h, px)) in kept.iter().enumerate() {
                let _ = t5_recomp::write_png(&format!("{out}/burst_{k:03}_{n}.png"), *w, *h, px);
            }
            eprintln!("[headless] burst: {i} frames");
        });
    }
    for s in 1..=secs {
        wait_secs(s as f64);
        // T5_RAMSNAP=<s1>,<s2>,...: EE RAM dumps at those seconds (finding game variables).
        if let Ok(v) = std::env::var("T5_RAMSNAP") {
            if v.split(',').any(|x| x.trim().parse::<u64>().ok() == Some(s)) {
                let ram = &ps2_runtime::machine().ram;
                let _ = std::fs::write(format!("{out}/ram_{s:03}.bin"), &ram[..32 << 20]);
                eprintln!("[headless] ram snapshot {s}s at vblank {}", ps2_runtime::VBLANKS.load(Relaxed));
            }
        }
        let f = shared.frames.frame.lock().unwrap();
        if f.width > 0 && f.rgba.len() == (f.width * f.height * 4) as usize {
            let _ = t5_recomp::write_png(&format!("{out}/frame_{s:03}.png"), f.width, f.height, &f.rgba);
        }
        eprintln!(
            "[headless] {s}s: vblanks {} frames {} ({}x{}) fn {} unknown {}",
            ps2_runtime::VBLANKS.load(Relaxed),
            ps2_runtime::FRAMES.load(Relaxed),
            f.width,
            f.height,
            ps2_runtime::FN_ENTRIES.load(Relaxed),
            ps2_runtime::UNKNOWN_TARGETS.load(Relaxed)
        );
    }
    #[cfg(windows)]
    if let Some(p) = prof {
        if let Ok(h) = p.join() {
            t5_recomp::profile::report(h);
        }
    }
    std::process::exit(0);
}

/// Sleep until the device clock reaches `t` seconds (all times here are device time, so a run
/// under `T5_WARP` does the same at the same game moments, only sooner).
fn wait_secs(t: f64) {
    while ps2_runtime::secs() < t {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
