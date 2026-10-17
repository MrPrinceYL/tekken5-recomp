//! Headless runner for automated checks: boots the game without a window or input, writes the
//! displayed frame as a PNG every second and stops after the given time.
//!
//! Usage: t5-headless [seconds] [out_dir] [--press BUTTON@SECOND ...]
//! (BUTTON: START, CROSS, CIRCLE, UP, DOWN, LEFT, RIGHT; held for half a second.)

use std::sync::atomic::Ordering::Relaxed;

fn main() {
    // T5_NOCOUNT=1: no event counters (measure as in normal play).
    ps2_runtime::diag::COUNTING.store(std::env::var_os("T5_NOCOUNT").is_none(), std::sync::atomic::Ordering::Relaxed);
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let out = args.get(2).cloned().unwrap_or_else(|| "headless".into());
    let _ = std::fs::create_dir_all(&out);
    let mut presses: Vec<(u16, f64)> = Vec::new();
    for w in args.windows(2).filter(|w| w[0] == "--press") {
        if let Some((b, t)) = w[1].split_once('@') {
            let bit = match b {
                "START" => ps2_runtime::pad::START,
                "CROSS" => ps2_runtime::pad::CROSS,
                "CIRCLE" => ps2_runtime::pad::CIRCLE,
                "UP" => ps2_runtime::pad::UP,
                "DOWN" => ps2_runtime::pad::DOWN,
                "LEFT" => ps2_runtime::pad::LEFT,
                "RIGHT" => ps2_runtime::pad::RIGHT,
                _ => 0,
            };
            presses.push((bit, t.parse().unwrap_or(0.0)));
        }
    }
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
    let t0 = std::time::Instant::now();
    let pad = shared.pad.clone();
    std::thread::spawn(move || loop {
        let t = t0.elapsed().as_secs_f64();
        // T5_HOLD=<seconds>: how long a press is held (default 0.5; menus repeat when held long).
        let hold: f64 = std::env::var("T5_HOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5);
        let held = presses.iter().filter(|(_, at)| t >= *at && t < at + hold).fold(0u16, |a, (b, _)| a | b);
        pad.buttons[0].store(held, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    // T5_PROFILE=<start>: sample the guest thread from that second to the end (Windows only).
    #[cfg(windows)]
    let prof = std::env::var("T5_PROFILE").ok().and_then(|v| v.parse::<u64>().ok()).map(|start| {
        let span = secs.saturating_sub(start).max(1);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(start));
            t5_recomp::profile::run(span)
        })
    });
    // T5_BURST=<second>: also save every displayed frame for one second from then on (to see
    // the frame-to-frame output the front end gets).
    if let Some(start) = std::env::var("T5_BURST").ok().and_then(|v| v.parse::<u64>().ok()) {
        let frames = shared.frames.clone();
        let out = out.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(start));
            let mut seen = 0;
            let mut buf = ps2_runtime::gs::Frame::default();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut i = 0;
            let mut kept = Vec::new();
            while std::time::Instant::now() < end {
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
        std::thread::sleep(std::time::Duration::from_secs(1));
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
