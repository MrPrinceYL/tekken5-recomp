//! MTVU (PCSX2's multi-threaded VU1): VIF1, VU1 and the GIF paths run on their own thread, as
//! VU1 runs beside the EE on the hardware.
//!
//! The EE thread hands the worker, in program order, everything that feeds the GIF: VIF1 / GIF
//! DMA transfers (a snapshot of the channel registers when started; the worker walks the chain and
//! reads memory as the DMAC would), the VIF1 / GIF FIFOs, VIF1 / GIF register writes, VU1 starts
//! from VU0 (CMSAR1) and each vertical blank (so the display read-out follows the drawing before
//! it). The worker runs the same code the EE thread used to run at those points (`vif::process`,
//! `vu::run`, `gif::path*`, the GS front's register events), so the order of VIF1 / VU1 / GIF work
//! is unchanged; a PATH3 transfer that meets MSKPATH3 waits until the VIF1 stream unmasks it.
//!
//! The worker reports back through `raise` (VIF1 / GS interrupts) and finished transfers, which the
//! EE thread applies at its next service point. The EE thread waits for the worker (`sync`) before
//! it reads VIF1 / GIF registers or VU1 memory; VPU-STAT shows VU1 busy while work is queued. A
//! VIF1 stall (interrupt bit) holds the worker until the program writes FBRST.STC.

use crate::dmac::{Channel, GIF, VIF1};
use crate::Machine;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};

/// Work for the worker, in EE program order.
pub enum Cmd {
    /// A VIF1 (from memory) or GIF transfer started with these registers.
    Dma(usize, Channel),
    Vif1Fifo(u128),
    GifFifo(u128),
    /// VIF1 register write (not FBRST, which goes through `fbrst`).
    VifReg(u32, u32),
    GifReg(u32, u32),
    /// VU1 started by VU0 (CMSAR1), at this instruction.
    VuStart(u32),
    /// VU1 reset (VU0 FBRST).
    VuReset,
    /// GS CSR.RESET.
    GsReset,
    /// Vertical blank: read the display out with these privileged registers.
    Vsync { pmode: u64, smode2: u64, dispfb: [u64; 2], display: [u64; 2], bgcolor: u64, field: u32 },
}

/// Most fields the EE may run ahead of the worker.
const MAX_AHEAD: u64 = 2;

struct Shared {
    /// Commands taken and finished by the worker.
    done: AtomicU64,
    /// The worker holds on a VIF1 stall (its VIF1 state is quiet).
    stalled: AtomicBool,
    /// FBRST writes for VIF1 (delivered ahead of the queue: they release a stall).
    fbrst: Mutex<VecDeque<u32>>,
    /// Finished transfers: (channel, registers).
    finished: Mutex<Vec<(usize, Channel)>>,
    /// A transfer stopped on a VIF1 stall: its registers so far (the EE shows them meanwhile).
    stall_regs: Mutex<Option<(usize, Channel)>>,
    vsyncs_done: AtomicU64,
}

pub struct Mtvu {
    tx: std::sync::mpsc::Sender<Cmd>,
    sent: u64,
    vsyncs_sent: u64,
    shared: Arc<Shared>,
}

/// Time the worker waited on VIF1 stalls / on the GS thread (µs), for the period log.
pub static STALL_US: AtomicU64 = AtomicU64::new(0);
pub static GSWAIT_US: AtomicU64 = AtomicU64::new(0);

/// Adds the time since `.0` to `.1` when dropped.
pub struct WaitTimer(pub std::time::Instant, pub &'static AtomicU64);
impl Drop for WaitTimer {
    fn drop(&mut self) {
        self.1.fetch_add(self.0.elapsed().as_micros() as u64, Relaxed);
    }
}

/// The worker's OS thread id (sampling profiler).
pub static OS_TID: AtomicU32 = AtomicU32::new(0);

/// Time the worker spent working (µs), for the period log.
pub static BUSY_US: AtomicU64 = AtomicU64::new(0);

/// INTC causes raised on the worker, merged into I_STAT by the EE thread.
pub static RAISED: AtomicU32 = AtomicU32::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
thread_local! {
    static ON_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The worker's queue and the commands it put aside during a VIF1 stall.
    static RX: std::cell::RefCell<Option<std::sync::mpsc::Receiver<Cmd>>> = const { std::cell::RefCell::new(None) };
    static BACKLOG: std::cell::RefCell<VecDeque<Cmd>> = const { std::cell::RefCell::new(VecDeque::new()) };
}
/// The worker's shared state (for `path3_unmasked`, called from the VIF code).
static SHARED: std::sync::OnceLock<Arc<Shared>> = std::sync::OnceLock::new();
/// PATH3 transfers waiting for MSKPATH3 to clear (worker only).
static DEFERRED: Mutex<Vec<Channel>> = Mutex::new(Vec::new());

/// MTVU is on (T5_MTVU=0 turns it off).
pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

/// The current thread is the worker.
pub fn on_worker() -> bool {
    ON_WORKER.with(|w| w.get())
}

impl Mtvu {
    /// Start the worker, if MTVU is enabled.
    pub fn start() -> Option<Mtvu> {
        if std::env::var("T5_MTVU").map(|v| v == "0").unwrap_or(false) {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared {
            done: AtomicU64::new(0),
            stalled: AtomicBool::new(false),
            fbrst: Mutex::new(VecDeque::new()),
            finished: Mutex::new(Vec::new()),
            stall_regs: Mutex::new(None),
            vsyncs_done: AtomicU64::new(0),
        });
        let sh = shared.clone();
        let _ = SHARED.set(shared.clone());
        std::thread::Builder::new()
            .name("mtvu".into())
            .spawn(move || {
                ON_WORKER.with(|w| w.set(true));
                crate::boost_thread_on(Some(1));
                #[cfg(windows)]
                {
                    #[link(name = "kernel32")]
                    unsafe extern "system" {
                        fn GetCurrentThreadId() -> u32;
                    }
                    OS_TID.store(unsafe { GetCurrentThreadId() }, Relaxed);
                }
                RX.with(|r| *r.borrow_mut() = Some(rx));
                loop {
                    let cmd = match BACKLOG.with(|b| b.borrow_mut().pop_front()) {
                        Some(c) => c,
                        None => match RX.with(|r| r.borrow().as_ref().unwrap().try_recv()) {
                            Ok(c) => c,
                            Err(_) => {
                                crate::diag::tl("W idle");
                                let c = match RX.with(|r| r.borrow().as_ref().unwrap().recv()) {
                                    Ok(c) => c,
                                    Err(_) => break,
                                };
                                crate::diag::tl("W busy");
                                c
                            }
                        },
                    };
                    let m = crate::machine();
                    let t0 = std::time::Instant::now();
                    exec(m, &sh, cmd);
                    BUSY_US.fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
                    sh.done.fetch_add(1, Release);
                }
            })
            .expect("MTVU thread");
        ACTIVE.store(true, Relaxed);
        Some(Mtvu { tx, sent: 0, vsyncs_sent: 0, shared })
    }

    pub fn send(&mut self, c: Cmd) {
        let vsync = matches!(c, Cmd::Vsync { .. });
        self.sent += 1;
        let _ = self.tx.send(c);
        if vsync {
            self.vsyncs_sent += 1;
            // Stay at most MAX_AHEAD fields ahead of the worker (spin: a sleep is too coarse).
            // Not while it holds on a VIF1 stall: only the EE's interrupt handler releases that.
            while self.vsyncs_sent - self.shared.vsyncs_done.load(Acquire) > MAX_AHEAD && !self.shared.stalled.load(Acquire) {
                std::thread::yield_now();
            }
        }
    }

    /// Wait until the worker has done everything queued (or holds on a VIF1 stall).
    pub fn sync(&self) {
        while self.shared.done.load(Acquire) < self.sent && !self.shared.stalled.load(Acquire) {
            std::thread::yield_now();
        }
    }

    /// For the period log.
    pub fn status(&self) -> String {
        format!("MTVU sent {} done {} stalled {} vsyncs {}/{} deferred PATH3 {} finished pending {}",
            self.sent, self.shared.done.load(Acquire), self.shared.stalled.load(Acquire),
            self.shared.vsyncs_done.load(Acquire), self.vsyncs_sent, DEFERRED.lock().unwrap().len(),
            self.shared.finished.lock().unwrap().len())
    }

    /// Work is queued or running (VPU-STAT VU1 busy).
    pub fn busy(&self) -> bool {
        self.shared.done.load(Acquire) < self.sent
    }

    /// Nothing queued, running or waiting to be applied (warp may skip ahead).
    pub fn quiet(&self) -> bool {
        !self.busy() && self.shared.finished.lock().unwrap().is_empty() && self.shared.stall_regs.lock().unwrap().is_none()
    }

    /// VIF1 FBRST (releases a stall).
    pub fn fbrst(&self, v: u32) {
        crate::diag::count_with(|| format!("mtvu_fbrst_sent_{v:x}"));
        self.shared.fbrst.lock().unwrap().push_back(v);
    }

    /// Apply what the worker reported: finished transfers.
    pub fn service(&self, m: &mut Machine) {
        if let Some((n, ch)) = self.shared.stall_regs.lock().unwrap().take() {
            // The chain stopped mid-way: the registers as the DMAC has them there.
            let c = &mut m.dmac.ch[n];
            c.madr = ch.madr;
            c.tadr = ch.tadr;
            c.qwc = ch.qwc;
            c.asr = ch.asr;
            c.chcr = (c.chcr & 0xFFFF) | (ch.chcr & 0xFFFF_0000);
        }
        let done: Vec<(usize, Channel)> = std::mem::take(&mut *self.shared.finished.lock().unwrap());
        for (n, ch) in done {
            let c = &mut m.dmac.ch[n];
            c.madr = ch.madr;
            c.tadr = ch.tadr;
            c.asr = ch.asr;
            c.chcr = (c.chcr & 0xFFFF) | (ch.chcr & 0xFFFF_0000);
            crate::dmac::finish(m, n);
        }
    }
}

/// Apply queued VIF1 FBRST writes on the worker.
fn apply_fbrst(m: &mut Machine, sh: &Shared) {
    loop {
        let v = sh.fbrst.lock().unwrap().pop_front();
        let Some(v) = v else { break };
        crate::diag::count_with(|| format!("mtvu_fbrst_applied_{v:x}"));
        crate::vif::reg_write(m, 1, 0x1000_3C10, v);
    }
}

/// Hold while VIF1 is stopped (until FBRST.STC), then continue its queued codes. `regs`: the
/// transfer in progress, published for the EE meanwhile.
fn wait_stall(m: &mut Machine, sh: &Shared, regs: Option<(usize, Channel)>) {
    if !m.vif[1].stalled() {
        return;
    }
    if regs.is_some() {
        *sh.stall_regs.lock().unwrap() = regs;
    }
    sh.stalled.store(true, Release);
    ee_core::ATTENTION.store(true, Relaxed);
    crate::kernel::wake_ee();
    let t0 = std::time::Instant::now();
    let _wait = WaitTimer(t0, &STALL_US);
    while m.vif[1].stalled() {
        apply_fbrst(m, sh);
        // PATH3 runs on while VIF1 stands (the game's VIF1 handler waits for a GIF transfer to
        // end before it releases the stall); everything else waits its turn.
        while let Some(cmd) = RX.with(|r| r.borrow().as_ref().and_then(|rx| rx.try_recv().ok())) {
            if matches!(cmd, Cmd::Dma(GIF, _) | Cmd::GifFifo(_) | Cmd::GifReg(..)) {
                exec(m, sh, cmd);
                sh.done.fetch_add(1, Release);
            } else {
                BACKLOG.with(|b| b.borrow_mut().push_back(cmd));
            }
        }
        std::thread::yield_now();
    }
    sh.stalled.store(false, Release);
}

fn finished(sh: &Shared, n: usize, ch: Channel) {
    sh.finished.lock().unwrap().push((n, ch));
    ee_core::ATTENTION.store(true, Relaxed);
    crate::kernel::wake_ee();
}

fn exec(m: &mut Machine, sh: &Shared, cmd: Cmd) {
    apply_fbrst(m, sh);
    match cmd {
        Cmd::Dma(n, mut ch) => {
            if n == GIF && m.gif.path3_masked() {
                DEFERRED.lock().unwrap().push(ch);
                return;
            }
            run_dma(m, sh, n, &mut ch);
        }
        Cmd::Vif1Fifo(v) => {
            crate::vif::feed(m, 1, &[v]);
            wait_stall(m, sh, None);
        }
        Cmd::GifFifo(v) => crate::gif::fifo_write(m, v),
        Cmd::VifReg(a, v) => crate::vif::reg_write(m, 1, a, v),
        Cmd::GifReg(a, v) => {
            m.gif.write(a, v);
            path3_unmasked(m);
        }
        Cmd::VuStart(pc) => crate::vu::run(m, 1, pc),
        Cmd::VuReset => m.vu1.regs = ee_core::vu0::VuRegs::new(),
        Cmd::GsReset => m.gs.reset(),
        Cmd::Vsync { pmode, smode2, dispfb, display, bgcolor, field } => {
            m.gs.vsync_stream(pmode, smode2, dispfb, display, bgcolor, field);
            sh.vsyncs_done.fetch_add(1, Release);
        }
    }
}

fn run_dma(m: &mut Machine, sh: &Shared, n: usize, ch: &mut Channel) {
    while !crate::dmac::run_job(m, n, ch) {
        // A VIF1 stall stopped the chain: continue once released.
        wait_stall(m, sh, Some((n, *ch)));
    }
    if n == VIF1 {
        wait_stall(m, sh, Some((n, *ch)));
    }
    finished(sh, n, *ch);
}

/// MSKPATH3 / GIF_MODE changed on the worker: run PATH3 transfers that were waiting.
pub fn path3_unmasked(m: &mut Machine) {
    if m.gif.path3_masked() {
        return;
    }
    let waiting: Vec<Channel> = std::mem::take(&mut *DEFERRED.lock().unwrap());
    if waiting.is_empty() {
        return;
    }
    let sh = SHARED.get().expect("MTVU");
    for mut ch in waiting {
        run_dma(m, sh, GIF, &mut ch);
    }
}
