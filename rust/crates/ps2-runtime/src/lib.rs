//! The PlayStation 2 behind `ee_core::Host`: the hardware the recompiled game talks to (INTC,
//! DMAC, timers, GIF, GS, VIF0/1, VU0/1 micro mode, IPU, SIF), a high-level emulation of the EE
//! kernel (threads, semaphores, interrupt handlers, alarms, program loading) and of the IOP
//! (the RPC services the game's IOP modules provide: files, CD/DVD, controllers, memory cards,
//! sound).
//!
//! Everything here runs on the guest thread; the Bevy front end only touches [`Shared`] (finished
//! frames, audio samples, controller state).
//!
//! Time: the guest runs as fast as the host allows. Device time is host time converted to EE
//! cycles (294.912 MHz); a ticker thread raises `ee_core::ATTENTION` twice a millisecond and the
//! generated code calls [`service`] at the next function entry or loop back-edge, which advances
//! the devices to the current time and takes pending interrupts if the CPU has them enabled.
//!
//! Guest threads (EE kernel threads) are host coroutines (see `kernel`): one runs at a time, the
//! kernel switches between them when a thread blocks or a higher-priority one becomes ready.

pub mod bus;
pub mod diag;
pub mod disc;
pub mod dmac;
pub mod gif;
pub mod gpu;
pub mod gs;
pub mod iop;
pub mod ipu;
pub mod kernel;
pub mod mtvu;
pub mod pad;
pub mod raster;
pub mod sif;
pub mod spu2;
pub mod timers;
pub mod vif;
pub mod vu;
pub mod vuc;
pub mod gscap;

pub use ee_core::{Ctx, Host, BLOCK_MISSES, CALLS, FN_ENTRIES, LAST_FN, UNKNOWN_TARGETS};
use ee_core::ATTENTION;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

/// EE core clock (Hz) and the bus clock that drives the timers.
pub const EE_HZ: u64 = 294_912_000;
pub const BUS_HZ: u64 = EE_HZ / 2;
/// NTSC: 59.94 fields per second, 262.5 lines per field.
pub const CYCLES_PER_FIELD: u64 = EE_HZ * 1001 / 60_000;
pub const CYCLES_PER_LINE: u64 = CYCLES_PER_FIELD * 2 / 525;
/// Vertical blank lasts 22.5 lines (VBLANK_START .. VBLANK_END).
pub const VBLANK_CYCLES: u64 = CYCLES_PER_LINE * 45 / 2;

/// INTC causes (I_STAT / I_MASK bits).
pub const INTC_GS: u32 = 0;
pub const INTC_SBUS: u32 = 1;
pub const INTC_VBLANK_START: u32 = 2;
pub const INTC_VBLANK_END: u32 = 3;
pub const INTC_VIF0: u32 = 4;
pub const INTC_VIF1: u32 = 5;
pub const INTC_VU0: u32 = 6;
pub const INTC_VU1: u32 = 7;
pub const INTC_IPU: u32 = 8;
pub const INTC_TIM0: u32 = 9;

/// Vertical blanks and displayed frames so far.
pub static VBLANKS: AtomicU64 = AtomicU64::new(0);
pub static FRAMES: AtomicU64 = AtomicU64::new(0);

/// Device time skipped while nothing was running (EE cycles, see `kernel::idle`).
pub static WARP: AtomicU64 = AtomicU64::new(0);
/// Fast-forward key held in the window.
pub static WARP_KEY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Skip idle time now? `WARP_KEY` held, or `T5_WARP`: `1` = always, else device-second ranges
/// `a-b,c-` (e.g. `0-70` = up to second 70, then real time).
pub fn warp_on(now: u64) -> bool {
    static RANGES: std::sync::OnceLock<Vec<(f64, f64)>> = std::sync::OnceLock::new();
    let r = RANGES.get_or_init(|| {
        let v = std::env::var("T5_WARP").unwrap_or_default();
        if v.trim() == "1" {
            return vec![(0.0, f64::MAX)];
        }
        v.split(',')
            .filter_map(|p| p.split_once('-'))
            .map(|(a, b)| (a.trim().parse().unwrap_or(0.0), b.trim().parse().unwrap_or(f64::MAX)))
            .collect()
    });
    let s = now as f64 / EE_HZ as f64;
    WARP_KEY.load(Relaxed) || r.iter().any(|&(a, b)| s >= a && s < b)
}

/// Device seconds since boot (the clock of `--press` and the timed diagnostics).
pub fn secs() -> f64 {
    machine_opt().map(|m| m.now() as f64 / EE_HZ as f64).unwrap_or(0.0)
}

/// One of the game's programs (generated from sheets/variants.csv).
pub struct Program {
    pub name: &'static str,
    pub title: &'static str,
    pub entry: u32,
    pub gp: u32,
    /// The program image's first bytes at its entry point (identifies the program in RAM).
    pub ident: &'static [u8],
    pub parts: &'static [&'static [(u32, u32, fn(&mut Ctx, u32))]],
    pub hle: &'static [(u32, &'static str)],
    pub names: &'static [(u32, &'static str)],
}

/// What the Bevy front end shares with the machine.
#[derive(Clone)]
pub struct Shared {
    pub frames: Arc<gs::FrameShared>,
    pub audio: Arc<spu2::AudioShared>,
    pub pad: Arc<pad::PadShared>,
}

pub struct Machine {
    pub ram: Box<[u8]>,
    pub scratch: Box<[u8]>,
    pub ctx: Box<Ctx>,
    pub start: Instant,
    /// Cycles skipped by a host stall (window drag, breakpoint): not replayed.
    pub skew: u64,
    pub istat: u32,
    pub imask: u32,
    pub dmac: dmac::Dmac,
    pub timers: timers::Timers,
    pub gs: gs::GsFront,
    pub gif: gif::Gif,
    pub vif: [vif::Vif; 2],
    pub vu1: vu::Vu,
    pub vu0: vu::Vu,
    pub ipu: ipu::Ipu,
    /// The VIF1 / VU1 / GIF worker (None: they run on the EE thread).
    pub mtvu: Option<mtvu::Mtvu>,
    pub sif: sif::Sif,
    pub iop: iop::Iop,
    pub spu2: spu2::Spu2,
    pub kernel: kernel::Kernel,
    pub disc: Option<disc::Disc>,
    pub disc_path: String,
    pub shared: Shared,
    pub programs: &'static [Program],
    funcs: Vec<Vec<(u32, u32, fn(&mut Ctx, u32))>>,
    /// Index of the program running now (identified from RAM at boot / ExecPS2).
    pub cur: Option<usize>,
    pub next_vblank: u64,
    pub vblank_end: u64,
    pub in_vblank: bool,
    pub next_summary: u64,
    pub field: u32,
}

static mut MACHINE: *mut Machine = std::ptr::null_mut();

/// The machine. Only the guest thread (and boot, before it starts) may use it.
#[allow(static_mut_refs)]
/// Tekken 5's own frame counter (main program, EE 0x151F90: one per game logic frame; found by
/// RAM snapshots). The true frame rate: displayed pictures and copy passes repeat every field.
pub fn logic_frames(m: &Machine) -> u32 {
    u32::from_le_bytes(m.ram[0x151F90..0x151F94].try_into().unwrap())
}

static LAST_LOGIC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Logic frames since the previous call (the period log).
pub fn logic_frames_delta(m: &Machine) -> u32 {
    let v = logic_frames(m);
    v.wrapping_sub(LAST_LOGIC.swap(v, Relaxed))
}

pub fn machine() -> &'static mut Machine {
    unsafe {
        assert!(!MACHINE.is_null(), "machine not created");
        &mut *MACHINE
    }
}

#[allow(static_mut_refs)]
/// Raise the calling thread's priority (the EE, MTVU and GS threads: the raster pool must not
/// push them off the CPU).
pub fn boost_thread() {
    boost_thread_on(None)
}

/// The physical cores (as logical-processor masks) from Windows, in order.
#[cfg(windows)]
fn core_masks() -> &'static [u64] {
    static M: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        #[repr(C)]
        struct Info {
            mask: usize,
            relationship: u32,
            _rest: [u64; 2],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLogicalProcessorInformation(buf: *mut Info, len: *mut u32) -> i32;
        }
        let mut len: u32 = 0;
        unsafe { GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len) };
        let n = len as usize / std::mem::size_of::<Info>();
        let mut v: Vec<Info> = (0..n).map(|_| Info { mask: 0, relationship: 0, _rest: [0; 2] }).collect();
        if n == 0 || unsafe { GetLogicalProcessorInformation(v.as_mut_ptr(), &mut len) } == 0 {
            return Vec::new();
        }
        // RelationProcessorCore = 0
        let mut cores: Vec<u64> = v.iter().filter(|i| i.relationship == 0).map(|i| i.mask as u64).collect();
        cores.sort();
        cores
    })
}

/// Threads on the critical path get a physical core of their own (no SMT sibling): slot 0 the
/// EE, 1 the MTVU worker, 2 the GS thread; the raster workers run on the other cores
/// (`raster_mask`). T5_AFFINITY: "0" (default) none, "w" only the MTVU worker, "1" all three.
pub fn critical_core(slot: usize) -> Option<u64> {
    #[cfg(windows)]
    {
        let mode = std::env::var("T5_AFFINITY").unwrap_or_else(|_| "0".into());
        let c = core_masks();
        if c.len() < 6 || mode == "0" || (mode == "w" && slot != 1) {
            return None;
        }
        return Some(c[slot]);
    }
    let _ = slot;
    None
}

/// The logical processors left for the raster workers (None: no pinning).
pub fn raster_mask() -> Option<u64> {
    #[cfg(windows)]
    {
        // Mode "w": only the MTVU worker has a core of its own; else EE, worker, GS.
        let reserved: Vec<u64> = (0..3).filter_map(critical_core).collect();
        if reserved.is_empty() {
            return None;
        }
        let all = core_masks().iter().fold(0, |a, m| a | m);
        return Some(reserved.iter().fold(all, |a, m| a & !m));
    }
    #[allow(unreachable_code)]
    None
}

/// Restrict the calling thread to the logical processors in `mask`.
pub fn set_affinity(mask: u64) {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> isize;
            fn SetThreadAffinityMask(h: isize, m: usize) -> usize;
        }
        unsafe { SetThreadAffinityMask(GetCurrentThread(), mask as usize) };
    }
    let _ = mask;
}

/// Raise the calling thread's priority and, for a critical thread, give it its core.
pub fn boost_thread_on(slot: Option<usize>) {
    if let Some(m) = slot.and_then(critical_core) {
        set_affinity(m);
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> isize;
            fn SetThreadPriority(h: isize, p: i32) -> i32;
        }
        // THREAD_PRIORITY_ABOVE_NORMAL
        unsafe { SetThreadPriority(GetCurrentThread(), 1) };
    }
}

/// Called at the start of every vertical blank on the guest thread: the game crate's opt-in
/// switches that set guest data (t5_recomp::devil_debug). Unset: nothing happens.
pub static VBLANK_HOOK: std::sync::OnceLock<fn(&mut Machine)> = std::sync::OnceLock::new();

pub fn machine_opt() -> Option<&'static Machine> {
    unsafe { MACHINE.as_ref() }
}

impl Machine {
    /// Current device time in EE cycles since boot.
    #[inline]
    pub fn now(&self) -> u64 {
        let ns = self.start.elapsed().as_nanos();
        ((ns * EE_HZ as u128 / 1_000_000_000) as u64 + WARP.load(Relaxed)).saturating_sub(self.skew)
    }

    /// Request an INTC interrupt; delivered at the next service point.
    #[inline]
    pub fn raise(&mut self, cause: u32) {
        if mtvu::on_worker() {
            mtvu::RAISED.fetch_or(1 << cause, Relaxed);
            ATTENTION.store(true, Relaxed);
            kernel::wake_ee();
            return;
        }
        self.istat |= 1 << cause;
        ATTENTION.store(true, Relaxed);
    }

    pub fn intc_pending(&self) -> bool {
        self.istat & self.imask != 0
    }

    pub fn program(&self) -> Option<&'static Program> {
        self.cur.map(|i| &self.programs[i])
    }

    /// Name of the function containing `addr` in the running program (diagnostics).
    pub fn function_name(&self, addr: u32) -> Option<&'static str> {
        let p = self.program()?;
        let i = p.names.partition_point(|(a, _)| *a <= addr);
        (i > 0).then(|| p.names[i - 1].1)
    }

    fn lookup_in(&self, prog: usize, addr: u32) -> Option<fn(&mut Ctx, u32)> {
        // Exact, non-overlapping ranges of each function's instructions.
        let funcs = &self.funcs[prog];
        let i = funcs.partition_point(|(s, _, _)| *s <= addr);
        (i > 0).then(|| funcs[i - 1]).filter(|&(_, e, _)| addr < e).map(|(_, _, f)| f)
    }

    /// Recompiled function containing `addr`: the running program first, then any other program
    /// whose image is in RAM (the loader at 0x01E00000 next to the program it unpacks).
    fn lookup(&mut self, addr: u32) -> Option<fn(&mut Ctx, u32)> {
        if let Some(c) = self.cur {
            if let Some(f) = self.lookup_in(c, addr) {
                return Some(f);
            }
        }
        for (i, p) in self.programs.iter().enumerate() {
            if Some(i) == self.cur || !self.in_ram(p) {
                continue;
            }
            if let Some(f) = self.lookup_in(i, addr) {
                return Some(f);
            }
        }
        None
    }

    /// True if RAM holds `p`'s image at its entry point.
    pub fn in_ram(&self, p: &Program) -> bool {
        let e = p.entry as usize & (ee_core::RAM_SIZE - 1);
        e + p.ident.len() <= self.ram.len() && self.ram[e..e + p.ident.len()] == *p.ident
    }

    /// Identify the program that starts at `entry` (after the BIOS / ExecPS2 loaded it).
    pub fn identify(&mut self, entry: u32) {
        let found = self.programs.iter().position(|p| p.entry == entry && self.in_ram(p));
        if found != self.cur {
            diag::log(format!(
                "program at {entry:#010x}: {}",
                found.map(|i| self.programs[i].title).unwrap_or("UNKNOWN (no recompiled code matches RAM)")
            ));
        }
        self.cur = found;
        if diag::enabled("T5_DUMPUNK") {
            let _ = std::fs::write(format!("atexec_{entry:08x}.bin"), &self.ram[0x1E7_3800..0x1E7_3900]);
        }
    }
}

/// Create the machine: RAM, devices, the disc image and the shared front-end state.
pub fn create(disc_path: &str, programs: &'static [Program], iop_modules: &'static [iop::IopModule]) -> Shared {
    let shared = Shared {
        frames: Arc::new(gs::FrameShared::default()),
        audio: Arc::new(spu2::AudioShared::new()),
        pad: Arc::new(pad::PadShared::default()),
    };
    let disc = match disc::Disc::open(disc_path) {
        Ok(d) => {
            diag::log(format!("disc: {} ({} sectors, {} files)", disc_path, d.sectors, d.files.len()));
            Some(d)
        }
        Err(e) => {
            diag::log(format!("disc: cannot open {disc_path}: {e}"));
            None
        }
    };
    let funcs = programs
        .iter()
        .map(|p| {
            let mut v: Vec<_> = p.parts.iter().flat_map(|x| x.iter().copied()).collect();
            v.sort_by_key(|f| f.0);
            v
        })
        .collect();
    let mut ram = vec![0u8; ee_core::RAM_SIZE].into_boxed_slice();
    let mut scratch = vec![0u8; ee_core::SCRATCH_SIZE].into_boxed_slice();
    let ctx = Box::new(Ctx::new(ram.as_mut_ptr(), scratch.as_mut_ptr(), &HOST));
    let m = Box::new(Machine {
        ram,
        scratch,
        ctx,
        start: Instant::now(),
        skew: 0,
        istat: 0,
        imask: 0,
        dmac: dmac::Dmac::default(),
        timers: timers::Timers::default(),
        gs: gs::GsFront::new(shared.frames.clone()),
        gif: gif::Gif::default(),
        vif: [vif::Vif::new(0), vif::Vif::new(1)],
        vu1: vu::Vu::new(1),
        vu0: vu::Vu::new(0),
        ipu: ipu::Ipu::default(),
        mtvu: None,
        sif: sif::Sif::default(),
        iop: iop::Iop::new(),
        spu2: spu2::Spu2::new(),
        kernel: kernel::Kernel::new(),
        disc,
        disc_path: disc_path.to_string(),
        shared: shared.clone(),
        programs,
        funcs,
        cur: None,
        next_vblank: CYCLES_PER_FIELD,
        vblank_end: 0,
        in_vblank: false,
        next_summary: EE_HZ * 5,
        field: 0,
    });
    let mut m = m;
    m.iop.exec.table = iop_modules;
    unsafe { MACHINE = Box::into_raw(m) };
    // The raster pool leaves room for the three threads on the critical path (EE, MTVU, GS).
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(cores.saturating_sub(3).max(2))
        .start_handler(|i| {
            #[cfg(windows)]
            if i == 0 {
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn GetCurrentThreadId() -> u32;
                }
                gs::RASTER_OS_TID.store(unsafe { GetCurrentThreadId() }, Relaxed);
            }
            let _ = i;
        })
        .build_global();
    machine().mtvu = mtvu::Mtvu::start();
    shared
}

/// Start the ticker that makes the guest thread service devices regularly.
pub fn start_ticker() {
    std::thread::Builder::new()
        .name("ps2-ticker".into())
        .spawn(|| {
            #[cfg(windows)]
            unsafe {
                windows_time_period();
            }
            loop {
                std::thread::sleep(std::time::Duration::from_micros(500));
                ATTENTION.store(true, Relaxed);
                iop_core::ATTENTION.store(true, Relaxed);
            }
        })
        .expect("spawn ticker");
}

#[cfg(windows)]
unsafe fn windows_time_period() {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    unsafe { timeBeginPeriod(1) };
}

pub static HOST: Host = Host {
    io_read: bus::io_read,
    io_write: bus::io_write,
    service,
    hle: kernel::hle,
    dispatch,
    syscall: kernel::syscall,
    brk: kernel::brk,
    cop0_read: kernel::cop0_read,
    cop0_write: kernel::cop0_write,
    ei: kernel::ei,
    vu0_call: vu::vu0_call,
    // VPU-STAT VU1 busy: work queued for the MTVU worker.
    vu0_busy: |_| machine().mtvu.as_ref().is_some_and(|t| t.busy()),
    intrinsic,
    log: diag::log,
};

/// CALLOTHER p-code ops without a translation: counted, logged once.
fn intrinsic(c: &mut Ctx, name: &'static str, word: u32) {
    diag::count_with(|| format!("intrinsic {name}"));
    diag::once(format!("intrinsic {name} word {word:#010x} in {:#010x}", ee_core::LAST_FN.load(Relaxed)));
    let _ = c;
}

/// `Host::dispatch`: the recompiled function containing `addr` (any RAM mirror is normalised).
fn dispatch(c: &mut Ctx, addr: u32) -> bool {
    let a = match ee_core::ram_off(addr) {
        Some(o) => o as u32,
        None => addr,
    };
    match machine().lookup(a) {
        Some(f) => {
            if a == kernel::THREAD_HOOK.load(Relaxed) {
                kernel::run_thread_calls(c);
            }
            f(c, a);
            true
        }
        None => kernel::unknown_call(c, a),
    }
}

/// Advance every device to the current time, then take pending interrupts if enabled.
pub fn service(c: &mut Ctx) {
    ATTENTION.store(false, Relaxed);
    let m = machine();
    if diag::enabled("T5_TIMELINE") {
        static LAST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let v = logic_frames(m);
        if LAST.swap(v, Relaxed) != v {
            diag::tl("EE logic");
        }
    }
    // What the MTVU worker reported: interrupts, finished transfers.
    let raised = mtvu::RAISED.swap(0, Relaxed);
    if raised != 0 {
        diag::count_with(|| format!("mtvu_raised_{raised:x}"));
        m.istat |= raised;
    }
    if let Some(t) = m.mtvu.as_ref() {
        let t: *const mtvu::Mtvu = t;
        unsafe { (*t).service(m) };
    }
    let now = m.now();
    if now >= m.next_vblank {
        if now - m.next_vblank > 4 * CYCLES_PER_FIELD {
            // The host stalled: don't replay the missed fields.
            m.skew += now - m.next_vblank;
            m.next_vblank = m.now();
        }
        m.next_vblank += CYCLES_PER_FIELD;
        m.vblank_end = now + VBLANK_CYCLES;
        m.in_vblank = true;
        m.field ^= 1;
        VBLANKS.fetch_add(1, Relaxed);
        diag::tl("EE vblank");
        if let Some(hook) = VBLANK_HOOK.get() {
            hook(m);
        }
        // T5_COVERFIELD=1: the EE functions entered in each field (coverfield.txt).
        if diag::enabled("T5_COVERFIELD") {
            ee_core::COVER_ON.store(true, Relaxed);
            let hit: Vec<String> = ee_core::cover_take().iter().map(|a| format!("{a:X}")).collect();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("coverfield.txt") {
                use std::io::Write;
                let _ = writeln!(f, "{} {}", VBLANKS.load(Relaxed), hit.join(" "));
            }
        }
        if let Some(t) = m.mtvu.as_mut() {
            let (pmode, smode2, dispfb, display, bgcolor) = m.gs.display_regs();
            t.send(mtvu::Cmd::Vsync { pmode, smode2, dispfb, display, bgcolor, field: m.field });
        } else {
            m.gs.vblank_start(m.field);
        }
        m.raise(INTC_VBLANK_START);
        if m.gs.vsync_irq() {
            m.raise(INTC_GS);
        }
        kernel::vblank(m);
        m.timers.vblank(now);
    }
    if m.in_vblank && now >= m.vblank_end {
        m.in_vblank = false;
        m.raise(INTC_VBLANK_END);
    }
    timers::update(m, now);
    sif::update(m, now);
    iop::update(m, now);
    spu2::update(m, now);
    kernel::update(m, now);
    if now >= m.next_summary {
        m.next_summary = now + EE_HZ * 5;
        diag::summary(m);
    }
    kernel::take_interrupts(c);
}
