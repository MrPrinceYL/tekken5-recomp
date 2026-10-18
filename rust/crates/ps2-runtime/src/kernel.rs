//! High-level emulation of the EE kernel (the BIOS part that user programs call with `syscall`).
//!
//! Threads: every guest thread is a host coroutine (corosensei) with its own host stack; the
//! scheduler ([`run`]) resumes the highest-priority ready thread (FIFO within a priority), which
//! runs until it blocks (SleepThread, WaitSema, ...), exits, or a higher-priority thread becomes
//! ready (preemption happens where the kernel would: after a syscall that readies a thread and
//! after interrupt handlers). The single CPU context (`Machine::ctx`) holds the running thread's
//! registers; the scheduler saves / restores them around each resume.
//!
//! Interrupts: INTC (INT0) and DMAC (INT1) causes are delivered at service points while the
//! interrupt enable bit (EIE) is set, by calling the handlers the program registered
//! (AddIntcHandler / AddDmacHandler) on an interrupt stack in kernel memory, with the
//! interrupted registers saved and restored around them. Alarms (SetAlarm, in HSYNC units) and
//! the vertical-blank flag (SetVSyncFlag) are kept by the kernel itself.
//!
//! Program loading: the "BIOS" loads SYSTEM.CNF's BOOT2 ELF; LoadExecPS2 loads an ELF from the
//! disc, ExecPS2 starts code already in RAM. Both end every thread of the old program (the
//! running guest code unwinds through `Ctx::abort`) and start the new program's main thread.

use crate::{diag, machine, Machine, CYCLES_PER_LINE};
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use ee_core::{Ctx, ATTENTION, GPR_A0, GPR_GP, GPR_RA, GPR_SP, GPR_V0, GPR_V1, REG_BYTES};
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;

pub const THS_RUN: u32 = 0x01;
pub const THS_READY: u32 = 0x02;
pub const THS_WAIT: u32 = 0x04;
pub const THS_SUSPEND: u32 = 0x08;
pub const THS_DORMANT: u32 = 0x10;
pub const TSW_SLEEP: u32 = 1;
pub const TSW_SEMA: u32 = 2;

/// Kernel memory used by the HLE kernel: the interrupt handler stack.
const IRQ_STACK: u32 = 0x0007_F000;
/// The syscall table in kernel memory (KSEG0 0x80002000). libkernl finds it by scanning kernel
/// memory for handlers it installed (SLUS 0x00118B50) and reads entries from it; entries of
/// HLE syscalls hold FAKE_HANDLER + 8 * n.
pub const SYSCALL_TABLE: u32 = 0x0000_2000;
pub const FAKE_HANDLER: u32 = 0x8000_3000;
/// Host stack per guest thread (reserved, committed on use).
const HOST_STACK: usize = 64 << 20;
const MAX_THREADS: usize = 256;
const MAX_SEMAS: usize = 256;

type Co = Coroutine<(), (), (), DefaultStack>;

/// A thread's CPU state while it doesn't run.
struct Saved {
    regs: Box<[u8; REG_BYTES]>,
    next: u32,
    depth: u32,
    bypass: u32,
    abort: bool,
}

pub struct Thread {
    pub id: u32,
    pub status: u32,
    pub prio: u32,
    pub init_prio: u32,
    pub entry: u32,
    pub stack: u32,
    pub stack_size: u32,
    pub gp: u32,
    pub attr: u32,
    pub option: u32,
    pub wait_type: u32,
    pub wait_id: u32,
    pub wakeup_count: u32,
    pub suspended: bool,
    pub arg: u32,
    seq: u64,
    saved: Option<Saved>,
    co: Option<Co>,
    yielder: *const Yielder<(), ()>,
    /// The value a blocking syscall returns when the thread is released (WaitSema: sema id or
    /// an error when the semaphore was deleted / the wait released).
    pub wake_result: i32,
}

pub struct Sema {
    pub count: i32,
    pub max: i32,
    pub init: i32,
    pub attr: u32,
    pub option: u32,
    pub waiters: Vec<u32>,
}

#[derive(Clone, Copy)]
pub struct Handler {
    pub id: u32,
    pub cause: u32,
    pub func: u32,
    pub arg: u32,
    pub gp: u32,
    pub enabled: bool,
}

#[derive(Clone, Copy)]
pub struct Alarm {
    pub id: u32,
    pub due: u64,
    pub ticks: u32,
    pub func: u32,
    pub arg: u32,
    pub gp: u32,
}

pub enum Exec {
    /// LoadExecPS2(path, args)
    Load(String, Vec<String>),
    /// ExecPS2(entry, gp, args)
    Start(u32, u32, Vec<String>),
}

pub struct Kernel {
    pub threads: Vec<Option<Thread>>,
    pub current: usize,
    pub semas: Vec<Option<Sema>>,
    pub intc: Vec<Handler>,
    pub dmac: Vec<Handler>,
    pub alarms: Vec<Alarm>,
    next_handler: u32,
    next_alarm: u32,
    seq: u64,
    /// COP0 Status EIE: interrupts enabled.
    pub ie: bool,
    pub in_irq: bool,
    /// True while a guest thread's coroutine runs (false in the scheduler / idle).
    in_thread: bool,
    pub pending_exec: Option<Exec>,
    pub exited: bool,
    pub heap_end: u32,
    pub main_stack: u32,
    pub vsync_flag: u32,
    pub vsync_csr: u32,
    pub syscalls: HashMap<u32, u32>,
    pub args: Vec<String>,
    pub sif_dma_id: u32,
    pub count_base: u64,
    pub osd_config: u32,
    pub osd_config2: u32,
}

impl Kernel {
    pub fn new() -> Kernel {
        Kernel {
            threads: (0..MAX_THREADS).map(|_| None).collect(),
            current: 0,
            semas: (0..MAX_SEMAS).map(|_| None).collect(),
            intc: Vec::new(),
            dmac: Vec::new(),
            alarms: Vec::new(),
            next_handler: 1,
            next_alarm: 1,
            seq: 1,
            ie: true,
            in_irq: false,
            in_thread: false,
            pending_exec: None,
            exited: false,
            heap_end: 0,
            main_stack: 0,
            vsync_flag: 0,
            vsync_csr: 0,
            syscalls: HashMap::new(),
            args: Vec::new(),
            sif_dma_id: 1,
            count_base: 0,
            osd_config: 1 << 16,
            osd_config2: 0,
        }
    }

    pub fn thread_summary(&self) -> String {
        let mut s = String::new();
        for t in self.threads.iter().flatten() {
            let st = match t.status {
                THS_RUN => "run",
                THS_READY => "rdy",
                THS_WAIT => if t.wait_type == TSW_SEMA { "sema" } else { "sleep" },
                THS_DORMANT => "dorm",
                _ => "susp",
            };
            s += &format!("{}:{}{}p{}e{:x}{} ", t.id, st, if t.suspended { "S" } else { "" }, t.prio, t.entry, if t.wait_type == TSW_SEMA && t.status == THS_WAIT { format!("s{}", t.wait_id) } else { String::new() });
        }
        s
    }

    fn cur(&mut self) -> Option<&mut Thread> {
        let c = self.current;
        self.threads.get_mut(c).and_then(|t| t.as_mut())
    }

    fn make_ready(&mut self, tid: usize) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(t) = self.threads[tid].as_mut() {
            t.status = THS_READY;
            t.wait_type = 0;
            t.seq = seq;
        }
    }

    /// The highest-priority ready (or running) thread.
    fn pick(&self) -> Option<usize> {
        self.threads
            .iter()
            .flatten()
            .filter(|t| (t.status == THS_READY || t.status == THS_RUN) && !t.suspended)
            .min_by_key(|t| (t.prio, t.seq))
            .map(|t| t.id as usize)
    }

    /// True if a ready thread has a higher priority than the running one.
    fn should_preempt(&self) -> bool {
        let Some(cur) = self.threads.get(self.current).and_then(|t| t.as_ref()) else { return false };
        self.threads.iter().flatten().any(|t| t.status == THS_READY && !t.suspended && t.prio < cur.prio)
    }

    fn alloc_thread(&self) -> Option<usize> {
        (1..MAX_THREADS).find(|&i| self.threads[i].is_none())
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Kernel::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Scheduler

/// Load the boot ELF and run the machine forever (the guest thread's body).
/// The guest (EE) thread, woken from `idle` by the MTVU worker.
pub static EE_THREAD: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();

/// Wake the EE thread if it idles (an interrupt or finished transfer from another thread).
pub fn wake_ee() {
    if let Some(t) = EE_THREAD.get() {
        t.unpark();
    }
}

pub fn run() -> ! {
    let _ = EE_THREAD.set(std::thread::current());
    crate::boost_thread_on(Some(0));
    let m = machine();
    let boot = boot_path(m);
    diag::log(format!("boot: {boot}"));
    m.kernel.pending_exec = Some(Exec::Load(boot.clone(), vec![boot]));
    loop {
        let m = machine();
        if m.kernel.exited {
            diag::once("kernel: exited, guest stopped".into());
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        if let Some(e) = m.kernel.pending_exec.take() {
            do_exec(m, e);
            continue;
        }
        match m.kernel.pick() {
            Some(tid) => run_thread(tid),
            None => {
                if std::env::var_os("T5_SYSCALLS").is_some() {
                    diag::count("sched idle");
                }
                idle()
            }
        }
    }
}

fn boot_path(m: &mut Machine) -> String {
    if let Some(d) = m.disc.as_mut() {
        if let Some((lba, size)) = d.find("SYSTEM.CNF") {
            if let Ok(b) = d.read_bytes(lba as u64, size as usize) {
                let s = String::from_utf8_lossy(&b).to_string();
                for line in s.lines() {
                    if let Some(v) = line.split_once('=').filter(|(k, _)| k.trim() == "BOOT2").map(|(_, v)| v.trim()) {
                        return v.to_string();
                    }
                }
            }
        }
    }
    "cdrom0:\\SLUS_210.59;1".to_string()
}

fn run_thread(tid: usize) {
    let m = machine();
    if std::env::var_os("T5_SYSCALLS").is_some() {
        diag::count_with(|| format!("resume t{tid}"));
    }
    let k = &mut m.kernel;
    k.current = tid;
    let first;
    {
        let t = k.threads[tid].as_mut().unwrap();
        t.status = THS_RUN;
        first = t.co.is_none();
        let c = m.ctx.as_mut();
        if let Some(s) = t.saved.take() {
            c.regs.copy_from_slice(&s.regs[..]);
            c.next = s.next;
            c.depth = s.depth;
            c.bypass = s.bypass;
            c.abort = s.abort;
        }
        if first {
            let entry = t.entry;
            let stack = DefaultStack::new(HOST_STACK).expect("thread stack");
            t.co = Some(Coroutine::with_stack(stack, move |y: &Yielder<(), ()>, ()| {
                let m = machine();
                if let Some(t) = m.kernel.threads[tid].as_mut() {
                    t.yielder = y as *const _;
                }
                let c: &mut Ctx = unsafe { &mut *(m.ctx.as_mut() as *mut Ctx) };
                c.call(entry);
                // The thread function returned (or the thread is being torn down).
                thread_returned(tid);
            }));
        }
    }
    k.in_thread = true;
    let co = k.threads[tid].as_mut().unwrap().co.as_mut().unwrap() as *mut Co;
    let t0 = std::time::Instant::now();
    let r = unsafe { (*co).resume(()) };
    diag::TIME[0].fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
    let m = machine();
    m.kernel.in_thread = false;
    match r {
        CoroutineResult::Yield(()) => {
            // Blocked or preempted: keep its registers.
            let c = m.ctx.as_mut();
            if let Some(t) = m.kernel.threads[tid].as_mut() {
                t.saved = Some(Saved { regs: Box::new(c.regs), next: c.next, depth: c.depth, bypass: c.bypass, abort: c.abort });
                if t.status == THS_RUN {
                    t.status = THS_READY;
                }
            }
        }
        CoroutineResult::Return(()) => {
            // Finished: the thread is dormant (or deleted / the program replaced).
            if let Some(t) = m.kernel.threads[tid].as_mut() {
                t.co = None;
                t.saved = None;
            }
            let c = m.ctx.as_mut();
            c.abort = false;
            c.next = 0;
            c.depth = 0;
        }
    }
    m.kernel.current = 0;
}

/// A thread's function returned or the thread was ended (ExitThread / program switch).
fn thread_returned(tid: usize) {
    let m = machine();
    if let Some(t) = m.kernel.threads[tid].as_mut() {
        if t.status != THS_DORMANT {
            t.status = THS_DORMANT;
        }
    }
    if tid == 1 && m.kernel.pending_exec.is_none() && !m.kernel.exited {
        diag::log("main thread returned: program exit".into());
    }
}

/// Suspend the running thread's coroutine (it is blocked or preempted); returns when the
/// scheduler resumes it. Outside a thread (interrupt handlers in the idle loop) it returns at
/// once.
fn switch_out() {
    let m = machine();
    if !m.kernel.in_thread || m.kernel.in_irq {
        return;
    }
    let y = m.kernel.cur().map(|t| t.yielder).unwrap_or(std::ptr::null());
    if y.is_null() {
        return;
    }
    unsafe { (*y).suspend(()) };
}

/// Block the running thread until it is made ready again; returns its wake result.
fn block() -> i32 {
    switch_out();
    let m = machine();
    m.kernel.cur().map(|t| t.wake_result).unwrap_or(0)
}

/// After a syscall that readied threads: let a higher-priority one run.
fn maybe_preempt() {
    let m = machine();
    if m.kernel.in_thread && !m.kernel.in_irq && m.kernel.should_preempt() {
        // Preempted: back to the front of its priority's queue.
        let min = m.kernel.threads.iter().flatten().map(|t| t.seq).min().unwrap_or(0);
        if let Some(t) = m.kernel.cur() {
            t.status = THS_READY;
            t.seq = min.saturating_sub(1);
        }
        switch_out();
    }
}

/// Device time of the next scheduled event (vblank, alarm, IOP, SIF, timer).
fn next_event(m: &Machine) -> u64 {
    let mut next = m.next_vblank.min(if m.in_vblank { m.vblank_end } else { u64::MAX });
    for a in &m.kernel.alarms {
        next = next.min(a.due);
    }
    next.min(crate::iop::next_event(m)).min(crate::sif::next_event(m)).min(crate::timers::next_event(m))
}

/// Warp: jump to the next event instead of waiting for it, but only when the MTVU worker has
/// nothing left (its results would otherwise land late in device time).
fn try_warp(m: &Machine, now: u64, next: u64) -> bool {
    let quiet = m.mtvu.as_ref().map_or(true, |t| t.quiet()) && crate::mtvu::RAISED.load(Relaxed) == 0;
    if next > now && crate::warp_on(now) && quiet {
        crate::WARP.fetch_add(next - now, Relaxed);
        crate::diag::tl("EE warp");
        return true;
    }
    false
}

/// INTC_STAT read: the same value many times in a row is a wait loop (libkernl `VSync` polls
/// it with interrupts off, 0x10F830) - as idle as a parked thread, so warp skips it too.
pub fn poll_istat(m: &Machine) {
    static POLL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let last = POLL.load(Relaxed);
    let n = if last >> 32 == m.istat as u64 { (last & 0xFFFF_FFFF) + 1 } else { 0 };
    POLL.store((m.istat as u64) << 32 | n, Relaxed);
    if n >= 8 {
        let now = m.now();
        if try_warp(m, now, next_event(m)) {
            // The next service point raises what is now due.
            ATTENTION.store(true, Relaxed);
            POLL.store((m.istat as u64) << 32, Relaxed);
        }
    }
}

/// No thread is ready: let time pass until something happens.
fn idle() {
    let m = machine();
    let now = m.now();
    let next = next_event(m);
    if !try_warp(m, now, next) && next > now {
        let ns = ((next - now) as u128 * 1_000_000_000 / crate::EE_HZ as u128) as u64;
        crate::diag::tl("EE idle");
        let t0 = std::time::Instant::now();
        // Woken early by the MTVU worker (`wake_ee`).
        std::thread::park_timeout(std::time::Duration::from_nanos(ns.min(2_000_000)));
        crate::diag::tl("EE wake");
        diag::TIME[2].fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
    }
    let c: &mut Ctx = unsafe { &mut *(m.ctx.as_mut() as *mut Ctx) };
    crate::service(c);
}

/// Replace the running program (LoadExecPS2 / ExecPS2 / boot).
fn do_exec(m: &mut Machine, e: Exec) {
    // Drop every thread of the old program (suspended coroutines unwind their host stacks).
    for t in m.kernel.threads.iter_mut() {
        if let Some(t) = t.as_mut() {
            t.co = None;
        }
    }
    let old_exec_args;
    let (entry, gp, args) = match e {
        Exec::Load(path, args) => match load_elf(m, &path) {
            Some((entry, gp)) => (entry, gp, args),
            None => {
                diag::log(format!("LoadExecPS2: cannot load {path}"));
                m.kernel.exited = true;
                return;
            }
        },
        Exec::Start(entry, gp, args) => (entry, gp, args),
    };
    old_exec_args = args;
    let k = &mut m.kernel;
    *k = Kernel::new();
    k.args = old_exec_args;
    crate::dmac::reset(m);
    // A newer kernel: the alarm timer (T3) has CMPE set, so libkernl's old-kernel alarm patch
    // stays out (SLUS 0x00119140).
    m.timers.t[3].mode = 0x100;
    for n in 0..256u32 {
        crate::bus::ram_write(m, SYSCALL_TABLE + 4 * n, 4, (FAKE_HANDLER + 8 * n) as u64);
    }
    m.ctx.regs.fill(0);
    m.ctx.next = 0;
    m.ctx.depth = 0;
    m.ctx.abort = false;
    m.ctx.bypass = 0;
    ee_core::vu0::regs(&mut m.ctx).vf[0] = [0, 0, 0, 0x3F80_0000];
    m.identify(entry);
    let k = &mut m.kernel;
    k.threads[1] = Some(Thread {
        id: 1,
        status: THS_READY,
        prio: 0,
        init_prio: 0,
        entry,
        stack: 0,
        stack_size: 0,
        gp,
        attr: 0,
        option: 0,
        wait_type: 0,
        wait_id: 0,
        wakeup_count: 0,
        suspended: false,
        arg: 0,
        seq: 0,
        saved: None,
        co: None,
        yielder: std::ptr::null(),
        wake_result: 0,
    });
    let c = m.ctx.as_mut();
    c.set_gpr(GPR_GP, gp as u64);
    c.set_gpr(GPR_SP, 0x01FF_FF00);
    let mut regs = Box::new([0u8; REG_BYTES]);
    regs.copy_from_slice(&c.regs);
    k.threads[1].as_mut().unwrap().saved = Some(Saved { regs, next: 0, depth: 0, bypass: 0, abort: false });
    diag::log(format!("exec: entry {entry:#010x} gp {gp:#010x} args {:?}", k.args));
}

/// Load an ELF from the disc into RAM (the file part of each PT_LOAD segment).
fn load_elf(m: &mut Machine, path: &str) -> Option<(u32, u32)> {
    let d = m.disc.as_mut()?;
    let (lba, size) = d.find(path)?;
    let data = d.read_bytes(lba as u64, size as usize).ok()?;
    if &data[..4] != b"\x7fELF" {
        return None;
    }
    let rd = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    let entry = rd(24);
    let phoff = rd(28) as usize;
    let phnum = u16::from_le_bytes([data[44], data[45]]) as usize;
    for i in 0..phnum {
        let p = phoff + 32 * i;
        if rd(p) != 1 {
            continue;
        }
        let (off, vaddr, filesz, memsz) = (rd(p + 4) as usize, rd(p + 8), rd(p + 16) as usize, rd(p + 20) as usize);
        let base = vaddr as usize & (ee_core::RAM_SIZE - 1);
        let n = filesz.min(data.len().saturating_sub(off));
        // Only the file part: the kernel's loader leaves the rest (BSS) to crt0. (Zeroing it
        // would also wipe sections inside another segment's memory range, like SLUS .y4 in .dmh.)
        let _ = memsz;
        m.ram[base..base + n].copy_from_slice(&data[off..off + n]);
    }
    diag::log(format!("loaded {path}: {size} bytes, entry {entry:#010x}"));
    Some((entry, 0))
}

// ---------------------------------------------------------------------------------------------
// Interrupts

/// Guest functions a game crate asked to run (`request_guest_call`; t5_recomp::devil_debug's
/// opt-in cheat menu): called like an interrupt handler at the next service point.
static GUEST_CALLS: std::sync::Mutex<Vec<(u32, Vec<u64>)>> = std::sync::Mutex::new(Vec::new());

/// Run the guest function `func` (no arguments, the interrupted thread's gp) at the next
/// interrupt service point, in interrupt context like the game's own handlers.
pub fn request_guest_call(func: u32) {
    request_guest_call_args(func, &[]);
}

/// `request_guest_call` with arguments (a0.., in order; calls run in the order requested).
pub fn request_guest_call_args(func: u32, args: &[u64]) {
    if let Ok(mut q) = GUEST_CALLS.lock() {
        q.push((func, args.to_vec()));
    }
    ATTENTION.store(true, Relaxed);
}

/// Guest functions to run in the game's own thread context (they may wait, read files):
/// `run_thread_calls` runs them when the guest calls the function at `THREAD_HOOK` (a function
/// the game calls once per frame from its main loop, set by the game crate).
static THREAD_CALLS: std::sync::Mutex<Vec<(u32, Vec<u64>)>> = std::sync::Mutex::new(Vec::new());
pub static THREAD_HOOK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// An argument of `request_thread_call` that stands for the previous call's return value (v0).
pub const PREV_RESULT: u64 = 0xFFFF_FFFF_FFFF_FFF0;

/// Run `func(args..)` in thread context at the next call of `THREAD_HOOK`.
pub fn request_thread_call(func: u32, args: &[u64]) {
    if let Ok(mut q) = THREAD_CALLS.lock() {
        q.push((func, args.to_vec()));
    }
}

/// From the dispatcher, before the hook function runs: the queued calls, with the caller's
/// registers saved and restored around them.
pub fn run_thread_calls(c: &mut Ctx) {
    let calls: Vec<(u32, Vec<u64>)> = THREAD_CALLS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    if calls.is_empty() {
        return;
    }
    let saved = Saved { regs: Box::new(c.regs), next: c.next, depth: c.depth, bypass: c.bypass, abort: c.abort };
    let mut prev = 0u64;
    for (f, args) in calls {
        c.next = 0;
        c.bypass = 0;
        let args: Vec<u64> = args.iter().map(|&a| if a == PREV_RESULT { prev } else { a }).collect();
        let v0 = c.call_guest(f, &args);
        prev = v0;
        diag::log(format!("thread call {f:#x}({args:x?}) -> {v0:#x}"));
    }
    c.regs.copy_from_slice(&saved.regs[..]);
    c.next = saved.next;
    c.depth = saved.depth;
    c.bypass = saved.bypass;
    c.abort = saved.abort;
}

/// Deliver pending INTC / DMAC interrupts if enabled (called from `service`).
pub fn take_interrupts(c: &mut Ctx) {
    let m = machine();
    if m.kernel.in_irq || !m.kernel.ie {
        return;
    }
    let intc = m.istat & m.imask;
    let dmac = crate::dmac::pending(m);
    let alarms = due_alarms(m);
    let calls: Vec<(u32, Vec<u64>)> = GUEST_CALLS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    if intc == 0 && dmac == 0 && alarms.is_empty() && calls.is_empty() {
        return;
    }
    m.kernel.in_irq = true;
    let t_irq = std::time::Instant::now();
    let saved = Saved { regs: Box::new(c.regs), next: c.next, depth: c.depth, bypass: c.bypass, abort: c.abort };
    c.next = 0;
    c.bypass = 0;
    for cause in 0..15 {
        if intc & (1 << cause) == 0 {
            continue;
        }
        machine().istat &= !(1 << cause);
        let hs: Vec<Handler> = machine().kernel.intc.iter().filter(|h| h.cause == cause && h.enabled).copied().collect();
        for h in hs {
            call_handler(c, h.func, h.gp, &[cause as u64, h.arg as u64]);
        }
    }
    for ch in 0..16 {
        if dmac & (1 << ch) == 0 {
            continue;
        }
        crate::dmac::ack(machine(), ch);
        let hs: Vec<Handler> = machine().kernel.dmac.iter().filter(|h| h.cause == ch && h.enabled).copied().collect();
        for h in hs {
            call_handler(c, h.func, h.gp, &[ch as u64, h.arg as u64]);
        }
    }
    for a in alarms {
        call_handler(c, a.func, a.gp, &[a.id as u64, a.ticks as u64, a.arg as u64]);
    }
    for (f, args) in calls {
        call_handler(c, f, 0, &args);
    }
    c.regs.copy_from_slice(&saved.regs[..]);
    c.next = saved.next;
    c.depth = saved.depth;
    c.bypass = saved.bypass;
    c.abort = saved.abort || c.abort && machine().kernel.pending_exec.is_some();
    diag::TIME[3].fetch_add(t_irq.elapsed().as_micros() as u64, Relaxed);
    let m = machine();
    m.kernel.in_irq = false;
    if m.kernel.pending_exec.is_some() {
        c.abort = true;
        return;
    }
    maybe_preempt();
}

fn call_handler(c: &mut Ctx, func: u32, gp: u32, args: &[u64]) {
    let t0 = std::time::Instant::now();
    let prev = CUR_HANDLER.swap(func, Relaxed);
    call_handler_inner(c, func, gp, args);
    CUR_HANDLER.store(prev, Relaxed);
    let us = t0.elapsed().as_micros() as u64;
    if us > 2000 {
        diag::count_with(|| format!("slow handler {func:#x} (cause {})", args.first().copied().unwrap_or(0)));
        diag::once(format!("handler {func:#x} took {us} us, last loop {:#x}", ee_core::LAST_LOOP.load(Relaxed)));
    }
}

fn call_handler_inner(c: &mut Ctx, func: u32, gp: u32, args: &[u64]) {
    for (i, a) in args.iter().enumerate() {
        c.set_gpr(GPR_A0 + i, *a);
    }
    if gp != 0 {
        c.set_gpr(GPR_GP, gp as u64);
    }
    c.set_gpr(GPR_SP, IRQ_STACK as u64);
    c.set_gpr(GPR_RA, 0);
    c.abort = false;
    c.call(func);
    c.next = 0;
}

fn due_alarms(m: &mut Machine) -> Vec<Alarm> {
    let now = m.now();
    let (due, keep): (Vec<Alarm>, Vec<Alarm>) = m.kernel.alarms.iter().partition(|a| a.due <= now);
    m.kernel.alarms = keep;
    due
}

/// Vertical blank start: the kernel's SetVSyncFlag service.
pub fn vblank(m: &mut Machine) {
    if m.kernel.vsync_flag != 0 {
        let f = m.kernel.vsync_flag;
        crate::bus::ram_write(m, f, 4, 1);
        if m.kernel.vsync_csr != 0 {
            let csr = m.gs.csr_read();
            crate::bus::ram_write(m, m.kernel.vsync_csr, 8, csr);
        }
    }
}

pub fn update(m: &mut Machine, now: u64) {
    if m.kernel.alarms.iter().any(|a| a.due <= now) {
        ATTENTION.store(true, Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Syscalls

fn arg(c: &Ctx, n: usize) -> u32 {
    // EABI: a0..a3, then t0..t3.
    c.gpr32(if n < 4 { GPR_A0 + n } else { 8 + n - 4 })
}

fn ret(c: &mut Ctx, v: i64) {
    c.set_gpr(GPR_V0, v as u64);
}

fn cstr(m: &Machine, addr: u32, max: usize) -> String {
    let mut s = Vec::new();
    for i in 0..max as u32 {
        let b = crate::bus::ram_read(m, addr.wrapping_add(i), 1) as u8;
        if b == 0 {
            break;
        }
        s.push(b);
    }
    String::from_utf8_lossy(&s).to_string()
}

pub fn syscall(c: &mut Ctx, _word: u32) {
    let raw = c.gpr32(GPR_V1) as i32;
    let n = raw.unsigned_abs();
    let m = machine();
    if let Some(&func) = m.kernel.syscalls.get(&n) {
        // A syscall the program installed itself (SetSyscall): run it like the kernel would.
        diag::count_with(|| format!("custom syscall {n:#x}"));
        let ra = c.gpr(GPR_RA);
        c.call(func);
        c.set_gpr(GPR_RA, ra);
        return;
    }
    let a = |i| arg(c, i);
    if std::env::var_os("T5_SYSCALLS").is_some() {
        diag::count_with(|| format!("sc_{raw:#x}_t{}", m.kernel.current));
    }
    match n {
        0x01 => { diag::log(format!("ResetEE({:#x})", a(0))); crate::dmac::reset_ee(m, a(0)); ret(c, 0) }
        0x02 => { m.gs.set_crt(a(0), a(1), a(2)); ret(c, 0) }
        0x04 => {
            diag::log(format!("Exit({})", a(0) as i32));
            m.kernel.exited = true;
            c.abort = true;
        }
        0x06 => {
            let path = cstr(m, a(0), 256);
            let mut args = vec![path.clone()];
            for i in 0..a(1).min(16) {
                let p = crate::bus::ram_read(m, a(2) + 4 * i, 4) as u32;
                args.push(cstr(m, p, 256));
            }
            diag::log(format!("LoadExecPS2({path}, {:?})", &args[1..]));
            m.kernel.pending_exec = Some(Exec::Load(path, args));
            c.abort = true;
        }
        0x07 => {
            let mut args = Vec::new();
            for i in 0..a(2).min(16) {
                let p = crate::bus::ram_read(m, a(3) + 4 * i, 4) as u32;
                args.push(cstr(m, p, 256));
            }
            diag::log(format!("ExecPS2({:#010x}, gp {:#010x}, {:?})", a(0), a(1), args));
            m.kernel.pending_exec = Some(Exec::Start(a(0), a(1), args));
            c.abort = true;
        }
        0x0A | 0x0B | 0x0C => { diag::count_with(|| format!("syscall {n:#x}")); ret(c, 0) }
        0x0D | 0x0E | 0x0F => ret(c, 0),
        0x10 => {
            // AddIntcHandler(cause, handler, next, arg)
            let h = add_handler(m, true, a(0), a(1), a(2) as i32, a(3), c.gpr32(GPR_GP));
            ret(c, h as i64)
        }
        0x11 => { m.kernel.intc.retain(|h| h.id != a(1)); ret(c, 0) }
        0x12 => {
            let h = add_handler(m, false, a(0), a(1), a(2) as i32, a(3), c.gpr32(GPR_GP));
            ret(c, h as i64)
        }
        0x13 => { m.kernel.dmac.retain(|h| h.id != a(1)); ret(c, 0) }
        0x14 | 0x1A => { let old = (m.imask >> a(0)) & 1; m.imask |= 1 << (a(0) & 15); ATTENTION.store(true, Relaxed); ret(c, (old ^ 1) as i64) }
        0x15 | 0x1B => { let old = (m.imask >> a(0)) & 1; m.imask &= !(1 << (a(0) & 15)); ret(c, old as i64) }
        0x16 | 0x1C => { let r = crate::dmac::enable(m, a(0), true); ATTENTION.store(true, Relaxed); ret(c, r as i64) }
        0x17 | 0x1D => { let r = crate::dmac::enable(m, a(0), false); ret(c, r as i64) }
        0x18 | 0x1E => {
            // SetAlarm(ticks, handler, arg)
            let id = m.kernel.next_alarm;
            m.kernel.next_alarm += 1;
            let ticks = a(0) & 0xFFFF;
            let due = m.now() + ticks.max(1) as u64 * CYCLES_PER_LINE;
            m.kernel.alarms.push(Alarm { id, due, ticks, func: a(1), arg: a(2), gp: c.gpr32(GPR_GP) });
            ret(c, id as i64)
        }
        0x19 | 0x1F => {
            let before = m.kernel.alarms.len();
            m.kernel.alarms.retain(|x| x.id != a(0));
            ret(c, if m.kernel.alarms.len() < before { a(0) as i64 } else { -1 })
        }
        0x20 => { let r = create_thread(m, c, a(0)); ret(c, r as i64) }
        0x21 => {
            let t = a(0) as usize;
            let ok = t < MAX_THREADS && m.kernel.threads[t].as_ref().is_some_and(|t| t.status == THS_DORMANT);
            if ok { m.kernel.threads[t] = None; }
            ret(c, if ok { t as i64 } else { -1 })
        }
        0x22 => ret(c, start_thread(m, a(0) as usize, a(1)) as i64),
        0x23 | 0x24 => {
            // ExitThread / ExitDeleteThread: end the running thread.
            let tid = m.kernel.current;
            if let Some(t) = m.kernel.cur() {
                t.status = THS_DORMANT;
            }
            if n == 0x24 {
                // Deleted when its coroutine has finished (scheduler sees no thread).
                diag::count("ExitDeleteThread");
            }
            let _ = tid;
            c.abort = true;
        }
        0x25 | 0x26 => {
            let t = a(0) as usize;
            let ok = terminate(m, t);
            ret(c, if ok { t as i64 } else { -1 })
        }
        0x27 | 0x28 => ret(c, 0),
        0x29 | 0x2A => {
            let t = if a(0) == 0 { m.kernel.current } else { a(0) as usize };
            let r = match m.kernel.threads.get_mut(t).and_then(|x| x.as_mut()) {
                Some(th) => { let old = th.prio; th.prio = a(1) & 127; old as i64 }
                None => -1,
            };
            ret(c, r);
            if n == 0x29 { maybe_preempt_or_yield(); }
        }
        0x2B | 0x2C => {
            // RotateThreadReadyQueue(prio): the first ready thread of that priority (the running
            // one, if it has it) goes to the end of its queue.
            let p = a(0) & 127;
            let k = &mut m.kernel;
            k.seq += 1;
            let seq = k.seq;
            let cur = k.current;
            let cur_has = k.threads.get(cur).and_then(|t| t.as_ref()).is_some_and(|t| t.prio == p && t.status == THS_RUN);
            let head = if cur_has {
                Some(cur)
            } else {
                k.threads.iter().flatten().filter(|t| t.status == THS_READY && t.prio == p && !t.suspended).min_by_key(|t| t.seq).map(|t| t.id as usize)
            };
            if let Some(h) = head {
                if let Some(t) = k.threads[h].as_mut() {
                    t.seq = seq;
                }
            }
            ret(c, p as i64);
            if n == 0x2B && cur_has {
                let other = k.threads.iter().flatten().any(|t| t.status == THS_READY && t.prio == p && !t.suspended && t.id as usize != cur);
                if other {
                    if let Some(t) = k.cur() {
                        t.status = THS_READY;
                    }
                    switch_out();
                }
            }
        }
        0x2D | 0x2E => {
            let t = a(0) as usize;
            let r = release_wait(m, t);
            ret(c, r);
            if n == 0x2D { maybe_preempt(); }
        }
        0x2F => ret(c, m.kernel.current as i64),
        0x30 | 0x31 => ret(c, refer_thread(m, a(0) as usize, a(1))),
        0x32 => {
            // SleepThread
            let cur = m.kernel.current;
            let t = m.kernel.cur().unwrap();
            if t.wakeup_count > 0 {
                t.wakeup_count -= 1;
                ret(c, cur as i64);
            } else {
                t.status = THS_WAIT;
                t.wait_type = TSW_SLEEP;
                t.wake_result = cur as i32;
                let r = block();
                ret(c, r as i64);
            }
        }
        0x33 | 0x34 => {
            let t = a(0) as usize;
            let r = wakeup(m, t);
            ret(c, r);
            if n == 0x33 { maybe_preempt(); }
        }
        0x35 | 0x36 => {
            let t = if a(0) == 0 { m.kernel.current } else { a(0) as usize };
            let r = match m.kernel.threads.get_mut(t).and_then(|x| x.as_mut()) {
                Some(th) => { let w = th.wakeup_count; th.wakeup_count = 0; w as i64 }
                None => -1,
            };
            ret(c, r)
        }
        0x37 | 0x38 => {
            let t = a(0) as usize;
            let r = match m.kernel.threads.get_mut(t).and_then(|x| x.as_mut()) {
                Some(th) => { th.suspended = true; t as i64 }
                None => -1,
            };
            ret(c, r);
            if n == 0x37 && t == m.kernel.current { switch_out(); }
        }
        0x39 | 0x3A => {
            let t = a(0) as usize;
            let r = match m.kernel.threads.get_mut(t).and_then(|x| x.as_mut()) {
                Some(th) => { th.suspended = false; t as i64 }
                None => -1,
            };
            ret(c, r);
            if n == 0x39 { maybe_preempt(); }
        }
        0x3B => ret(c, -1),
        0x3C => { let r = setup_thread(m, c); ret(c, r as i64) }
        0x3D => {
            // SetupHeap(start, size)
            let (start, size) = (a(0), a(1));
            m.kernel.heap_end = if size as i32 == -1 { m.kernel.main_stack } else { start.wrapping_add(size) };
            ret(c, m.kernel.heap_end as i64)
        }
        0x3E => ret(c, m.kernel.heap_end as i64),
        0x40 => ret(c, create_sema(m, a(0)) as i64),
        0x41 => ret(c, delete_sema(m, a(0) as usize)),
        0x42 | 0x43 => {
            let r = signal_sema(m, a(0) as usize);
            ret(c, r);
            if n == 0x42 { maybe_preempt(); }
        }
        0x44 => {
            let s = a(0) as usize;
            let r = wait_sema(m, s);
            ret(c, r)
        }
        0x45 | 0x46 => {
            let s = a(0) as usize;
            let r = match m.kernel.semas.get_mut(s).and_then(|x| x.as_mut()) {
                Some(sm) if sm.count > 0 => { sm.count -= 1; s as i64 }
                Some(_) => -0x1A1,
                None => -0x1A0,
            };
            ret(c, r)
        }
        0x47 | 0x48 => ret(c, refer_sema(m, a(0) as usize, a(1))),
        0x4A => { m.kernel.osd_config = crate::bus::ram_read(m, a(0), 4) as u32; ret(c, 0) }
        0x4B => {
            // GetOsdConfigParam (default: English, 4:3, RGB, no SPDIF); stores like a newer
            // kernel, so libkernl's old-kernel patch stays out (SLUS 0x00118D20).
            let v = m.kernel.osd_config;
            crate::bus::ram_write(m, a(0), 4, v as u64);
            ret(c, 0)
        }
        0x6E => { m.kernel.osd_config2 = crate::bus::ram_read(m, a(0), 4) as u32; ret(c, 0) }
        0x6F => { let v = m.kernel.osd_config2; crate::bus::ram_write(m, a(0), 4, v as u64); ret(c, 0) }
        0x5B => {
            // GetEntryAddress(syscall): the table entry.
            let v = crate::bus::ram_read(m, SYSCALL_TABLE + 4 * (a(0) & 0xFF), 4);
            ret(c, v as u32 as i32 as i64)
        }
        0x54..=0x59 => {
            // libkernl's TLB library (patched in at 0x80075000): Put/Set/Get/Probe TLB entry,
            // ExpandScratchPad. The fixed EE mapping covers the game's memory; log the requests.
            diag::once(format!("TLB syscall {n:#x}({:#x}, {:#x}, {:#x}, {:#x})", a(0), a(1), a(2), a(3)));
            diag::count_with(|| format!("tlb syscall {n:#x}"));
            ret(c, if n == 0x59 { 0 } else { a(0) as i64 })
        }
        0x4C..=0x4F => ret(c, 0),
        0x5C | 0x5D => { set_handler_enabled(m, true, a(0), a(1), n == 0x5C); ret(c, 0) }
        0x5E | 0x5F => { set_handler_enabled(m, false, a(0), a(1), n == 0x5E); ret(c, 0) }
        0x60 | 0x61 | 0x62 => ret(c, 0),
        0x63 | 0x67 => { let v = cop0_read(c, a(0)); ret(c, v as i32 as i64) }
        0x64 | 0x68 => ret(c, 0),
        0x66 | 0x6A => ret(c, 0),
        0x6B => { crate::sif::stop_dma(m); ret(c, 0) }
        0x6C | 0x6D => ret(c, 0),
        0x70 => ret(c, m.gs.imr.load(std::sync::atomic::Ordering::Relaxed) as i64),
        0x71 | 0x72 => { m.gs.imr.store(a(0) as u64, std::sync::atomic::Ordering::Relaxed); ret(c, 0) }
        0x73 => { m.kernel.vsync_flag = a(0); m.kernel.vsync_csr = a(1); ret(c, 0) }
        0x74 => {
            // SetSyscall(number, address)
            let (num, addr) = (a(0) & 0xFF, a(1));
            crate::bus::ram_write(m, SYSCALL_TABLE + 4 * num, 4, addr as u64);
            if addr & 0x1FFF_FFFF < 0x0008_0000 {
                // A handler in kernel memory: libkernl patching the BIOS kernel (code copied
                // there has no recompiled counterpart). The HLE kernel keeps serving it.
                diag::log(format!("SetSyscall({num:#x}, {addr:#010x}): kernel patch, HLE kept"));
                m.kernel.syscalls.remove(&num);
            } else {
                diag::log(format!("SetSyscall({num:#x}, {addr:#010x})"));
                m.kernel.syscalls.insert(num, addr);
            }
            ret(c, 0)
        }
        0x75 => { diag::log(format!("_print: {}", cstr(m, a(0), 256))); ret(c, 0) }
        0x76 => ret(c, crate::sif::dma_stat(m, a(0)) as i64),
        0x77 => { let r = crate::sif::set_dma(m, a(0), a(1)); ret(c, r as i64) }
        0x78 => { crate::sif::set_dchain(m); ret(c, 0) }
        0x79 => {
            diag::log(format!("SifSetReg({:#x}, {:#x}) ra {:#x}", a(0), a(1), c.gpr32(ee_core::GPR_RA)));
            let r = crate::sif::set_reg(m, a(0), a(1));
            ret(c, r as i64)
        }
        0x7A => {
            let r = crate::sif::get_reg(m, a(0));
            if std::env::var_os("T5_SYSCALLS").is_some() {
                diag::count_with(|| format!("getreg_{:#x}={:#x}_ra{:#x}", a(0), r, c.gpr32(ee_core::GPR_RA)));
            }
            ret(c, r as i32 as i64)
        }
        0x7B => { diag::log("ExecOSD".into()); m.kernel.exited = true; c.abort = true; }
        0x7C => ret(c, 0),
        0x7D => ret(c, 0),
        0x7E => ret(c, 0),
        0x7F => ret(c, ee_core::RAM_SIZE as i64),
        0x82 => ret(c, 0),
        _ => {
            diag::once(format!("unhandled syscall {raw:#x} (a0 {:#x})", a(0)));
            diag::count_with(|| format!("syscall {raw:#x}"));
            ret(c, 0)
        }
    }
}

fn maybe_preempt_or_yield() {
    maybe_preempt();
}

fn add_handler(m: &mut Machine, intc: bool, cause: u32, func: u32, next: i32, arg: u32, gp: u32) -> u32 {
    let id = m.kernel.next_handler;
    m.kernel.next_handler += 1;
    let h = Handler { id, cause, func, arg, gp, enabled: true };
    let list = if intc { &mut m.kernel.intc } else { &mut m.kernel.dmac };
    if next == 0 {
        list.insert(0, h);
    } else {
        list.push(h);
    }
    id
}

fn set_handler_enabled(m: &mut Machine, intc: bool, cause: u32, id: u32, on: bool) {
    let list = if intc { &mut m.kernel.intc } else { &mut m.kernel.dmac };
    for h in list.iter_mut().filter(|h| h.cause == cause && h.id == id) {
        h.enabled = on;
    }
}

fn setup_thread(m: &mut Machine, c: &mut Ctx) -> u32 {
    let (gp, stack, size, args, _root) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let base = if stack as i32 == -1 { ee_core::RAM_SIZE as u32 - size } else { stack };
    let top = base.wrapping_add(size);
    m.kernel.main_stack = base;
    if let Some(t) = m.kernel.threads[1].as_mut() {
        t.stack = base;
        t.stack_size = size;
        t.gp = gp;
    }
    // Arguments: { int argc; char *argv[16]; char strings[] }.
    if args != 0 {
        let list: Vec<String> = m.kernel.args.clone();
        let argc = list.len().min(16) as u32;
        crate::bus::ram_write(m, args, 4, argc as u64);
        let mut p = args + 4 + 16 * 4;
        for (i, s) in list.iter().take(16).enumerate() {
            crate::bus::ram_write(m, args + 4 + 4 * i as u32, 4, p as u64);
            for (j, b) in s.bytes().enumerate() {
                crate::bus::ram_write(m, p + j as u32, 1, b as u64);
            }
            crate::bus::ram_write(m, p + s.len() as u32, 1, 0);
            p += s.len() as u32 + 1;
        }
    }
    top - 0x2A0
}

fn create_thread(m: &mut Machine, _c: &mut Ctx, param: u32) -> i32 {
    let rd = |o: u32| crate::bus::ram_read(m, param + o, 4) as u32;
    let (func, stack, size, gp, prio, attr, option) = (rd(4), rd(8), rd(12), rd(16), rd(20), rd(28), rd(32));
    let Some(id) = m.kernel.alloc_thread() else { return -1 };
    m.kernel.threads[id] = Some(Thread {
        id: id as u32,
        status: THS_DORMANT,
        prio: prio & 127,
        init_prio: prio & 127,
        entry: func,
        stack,
        stack_size: size,
        gp,
        attr,
        option,
        wait_type: 0,
        wait_id: 0,
        wakeup_count: 0,
        suspended: false,
        arg: 0,
        seq: 0,
        saved: None,
        co: None,
        yielder: std::ptr::null(),
        wake_result: 0,
    });
    id as i32
}

fn start_thread(m: &mut Machine, tid: usize, argp: u32) -> i32 {
    let Some(t) = m.kernel.threads.get_mut(tid).and_then(|x| x.as_mut()) else { return -1 };
    if t.status != THS_DORMANT {
        return -1;
    }
    t.co = None;
    t.arg = argp;
    t.prio = t.init_prio;
    t.wakeup_count = 0;
    let mut regs = Box::new([0u8; REG_BYTES]);
    {
        let w = |r: &mut [u8; REG_BYTES], n: usize, v: u32| r[16 * n..16 * n + 8].copy_from_slice(&(v as i32 as i64 as u64).to_le_bytes());
        w(&mut regs, GPR_A0, argp);
        w(&mut regs, GPR_GP, t.gp);
        w(&mut regs, GPR_SP, (t.stack.wrapping_add(t.stack_size).wrapping_sub(0x2A0)) & !0xF);
        w(&mut regs, GPR_RA, 0);
        // VU0 vf0 is constant (0, 0, 0, 1).
        let vf0 = ee_core::REG_VF + 12;
        r_set(&mut regs, vf0, 0x3F80_0000);
    }
    t.saved = Some(Saved { regs, next: 0, depth: 0, bypass: 0, abort: false });
    m.kernel.make_ready(tid);
    maybe_preempt();
    tid as i32
}

fn r_set(r: &mut [u8; REG_BYTES], off: usize, v: u32) {
    r[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn terminate(m: &mut Machine, tid: usize) -> bool {
    if tid == m.kernel.current {
        return false;
    }
    let Some(t) = m.kernel.threads.get_mut(tid).and_then(|x| x.as_mut()) else { return false };
    if t.status == THS_WAIT && t.wait_type == TSW_SEMA {
        let s = t.wait_id as usize;
        if let Some(sm) = m.kernel.semas.get_mut(s).and_then(|x| x.as_mut()) {
            sm.waiters.retain(|&w| w as usize != tid);
        }
    }
    let t = m.kernel.threads[tid].as_mut().unwrap();
    t.status = THS_DORMANT;
    t.co = None; // unwinds the suspended coroutine's host stack
    t.saved = None;
    true
}

fn release_wait(m: &mut Machine, tid: usize) -> i64 {
    let Some(t) = m.kernel.threads.get_mut(tid).and_then(|x| x.as_mut()) else { return -1 };
    if t.status != THS_WAIT {
        return -1;
    }
    if t.wait_type == TSW_SEMA {
        let s = t.wait_id as usize;
        if let Some(sm) = m.kernel.semas.get_mut(s).and_then(|x| x.as_mut()) {
            sm.waiters.retain(|&w| w as usize != tid);
        }
    }
    m.kernel.threads[tid].as_mut().unwrap().wake_result = -0x1A2; // KE_RELEASE_WAIT
    m.kernel.make_ready(tid);
    tid as i64
}

/// The interrupt handler running (for the timeline).
static CUR_HANDLER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn wakeup(m: &mut Machine, tid: usize) -> i64 {
    if diag::enabled("T5_TIMELINE") {
        diag::tl(&format!("K wake t{tid} irq{} h{:x}", m.kernel.in_irq as u8, CUR_HANDLER.load(Relaxed)));
    }
    let Some(t) = m.kernel.threads.get_mut(tid).and_then(|x| x.as_mut()) else { return -1 };
    if t.status == THS_WAIT && t.wait_type == TSW_SLEEP {
        t.wake_result = tid as i32;
        m.kernel.make_ready(tid);
    } else {
        t.wakeup_count += 1;
    }
    tid as i64
}

fn refer_thread(m: &mut Machine, tid: usize, info: u32) -> i64 {
    let tid = if tid == 0 { m.kernel.current } else { tid };
    let Some(t) = m.kernel.threads.get(tid).and_then(|x| x.as_ref()) else { return -1 };
    let mut status = t.status;
    if t.suspended {
        status |= THS_SUSPEND;
    }
    let vals = [status, t.entry, t.stack, t.stack_size, t.gp, t.init_prio, t.prio, t.attr, t.option, t.wait_type, t.wait_id, t.wakeup_count];
    if info != 0 {
        for (i, v) in vals.iter().enumerate() {
            crate::bus::ram_write(m, info + 4 * i as u32, 4, *v as u64);
        }
    }
    status as i64
}

fn create_sema(m: &mut Machine, param: u32) -> i32 {
    let rd = |o: u32| crate::bus::ram_read(m, param + o, 4) as u32;
    let (max, init, attr, option) = (rd(4) as i32, rd(8) as i32, rd(16), rd(20));
    let Some(id) = (1..MAX_SEMAS).find(|&i| m.kernel.semas[i].is_none()) else { return -1 };
    m.kernel.semas[id] = Some(Sema { count: init, max, init, attr, option, waiters: Vec::new() });
    id as i32
}

fn delete_sema(m: &mut Machine, s: usize) -> i64 {
    let Some(sm) = m.kernel.semas.get_mut(s).and_then(|x| x.take()) else { return -0x1A0 };
    for w in sm.waiters {
        if let Some(t) = m.kernel.threads.get_mut(w as usize).and_then(|x| x.as_mut()) {
            t.wake_result = -0x1A3; // KE_WAIT_DELETE
        }
        m.kernel.make_ready(w as usize);
    }
    s as i64
}

fn signal_sema(m: &mut Machine, s: usize) -> i64 {
    if diag::enabled("T5_TIMELINE") {
        diag::tl(&format!("K sig s{s} irq{} h{:x}", m.kernel.in_irq as u8, CUR_HANDLER.load(Relaxed)));
    }
    let Some(sm) = m.kernel.semas.get_mut(s).and_then(|x| x.as_mut()) else { return -0x1A0 };
    if !sm.waiters.is_empty() {
        let w = sm.waiters.remove(0) as usize;
        if let Some(t) = m.kernel.threads.get_mut(w).and_then(|x| x.as_mut()) {
            t.wake_result = s as i32;
        }
        m.kernel.make_ready(w);
    } else {
        sm.count += 1;
    }
    s as i64
}

fn wait_sema(m: &mut Machine, s: usize) -> i64 {
    let cur = m.kernel.current;
    let Some(sm) = m.kernel.semas.get_mut(s).and_then(|x| x.as_mut()) else { return -0x1A0 };
    if sm.count > 0 {
        sm.count -= 1;
        return s as i64;
    }
    if !m.kernel.in_thread || m.kernel.in_irq {
        diag::once(format!("WaitSema({s}) outside a thread: would deadlock"));
        return -1;
    }
    sm.waiters.push(cur as u32);
    if diag::enabled("T5_TIMELINE") {
        diag::tl(&format!("K wait s{s} t{cur}"));
    }
    if let Some(t) = m.kernel.cur() {
        t.status = THS_WAIT;
        t.wait_type = TSW_SEMA;
        t.wait_id = s as u32;
        t.wake_result = s as i32;
    }
    block() as i64
}

fn refer_sema(m: &mut Machine, s: usize, info: u32) -> i64 {
    let Some(sm) = m.kernel.semas.get(s).and_then(|x| x.as_ref()) else { return -0x1A0 };
    let vals = [sm.count as u32, sm.max as u32, sm.init as u32, sm.waiters.len() as u32, sm.attr, sm.option];
    if info != 0 {
        for (i, v) in vals.iter().enumerate() {
            crate::bus::ram_write(m, info + 4 * i as u32, 4, *v as u64);
        }
    }
    s as i64
}

// ---------------------------------------------------------------------------------------------
// COP0, break, HLE, unknown calls

pub fn cop0_read(c: &mut Ctx, reg: u32) -> u32 {
    let m = machine();
    match reg {
        9 => (m.now().wrapping_sub(m.kernel.count_base)) as u32,
        12 => {
            // Status: EIE (16), user mode, IE and the INT0 / INT1 masks.
            0x7_0C13 & !(1 << 16) | (m.kernel.ie as u32) << 16
        }
        15 => 0x2E20,
        16 => 0x7244_0402,
        0x100 => crate::dmac::cpcond0(m) as u32,
        _ => {
            let off = ee_core::REG_COP0 + 4 * (reg as usize & 31);
            c.r(off, 4) as u32
        }
    }
}

pub fn cop0_write(c: &mut Ctx, reg: u32, v: u32) {
    let m = machine();
    match reg {
        9 => m.kernel.count_base = m.now().wrapping_sub(v as u64),
        12 => {
            m.kernel.ie = v & (1 << 16) != 0 && v & 1 != 0;
            if m.kernel.ie {
                ATTENTION.store(true, Relaxed);
            }
        }
        _ => {
            let off = ee_core::REG_COP0 + 4 * (reg as usize & 31);
            c.wr(off, 4, v as u64);
        }
    }
}

/// `ei` / `di`.
pub fn ei(_c: &mut Ctx, on: bool) {
    let m = machine();
    m.kernel.ie = on;
    if on {
        ATTENTION.store(true, Relaxed);
    }
}

pub fn brk(_c: &mut Ctx, word: u32) {
    diag::count("break/trap");
    diag::once(format!("break/trap {word:#010x} in {:#010x}", ee_core::LAST_FN.load(Relaxed)));
}

/// HLE handler for a hooked function (sheets/hle.csv); false lets the original run.
pub fn hle(_c: &mut Ctx, addr: u32) -> bool {
    let m = machine();
    let Some(p) = m.program() else { return false };
    let Some(name) = p.hle.iter().find(|(a, _)| *a == addr).map(|(_, n)| *n) else { return false };
    diag::count_with(|| format!("hle {name}"));
    false
}

/// A computed call to an address no recompiled function covers.
pub fn unknown_call(c: &mut Ctx, addr: u32) -> bool {
    let m = machine();
    diag::once(format!(
        "unknown call target {addr:#010x} from {:#010x} ({}) ra {:#010x}",
        ee_core::LAST_FN.load(Relaxed),
        m.function_name(ee_core::LAST_FN.load(Relaxed)).unwrap_or("?"),
        c.gpr32(GPR_RA)
    ));
    // T5_DUMPUNK=1: the 64 KiB at the target (what code the program put there).
    if diag::enabled("T5_DUMPUNK") {
        let base = (addr as usize) & (ee_core::RAM_SIZE - 1);
        let end = (base + 0x10000).min(m.ram.len());
        let _ = std::fs::write(format!("unk_{addr:08x}.bin"), &m.ram[base..end]);
    }
    false
}
