//! Running the game's own IOP modules (recompiled, see tools/gen_iop.py) on an HLE IOP kernel.
//!
//! - Loading: an IRX image the EE hands to the loader is matched by SHA-1 against the
//!   recompiled modules, relocated to the module's fixed address (the same relocation the
//!   generator saw) and started (`_start(argc, argv)` on a loader thread).
//! - Linking: import stubs are compiled with an HLE hook. A stub of a library another loaded
//!   module exports (libsd, modhsyn, nuFile, sdrdrv) is forwarded to that module's function;
//!   the kernel libraries are served by `iop::hle`.
//! - Threads: IOP threads are host coroutines like the EE's; the IOP runs inside the EE's
//!   service points until every IOP thread waits (or one used up its time slice, so a polling
//!   loop cannot stall the EE). Lower priority numbers run first.
//! - Interrupts: hardware timer handlers (timrman), alarms and registered interrupt handlers
//!   run on the IOP context with the interrupted registers saved.

use crate::{diag, Machine};
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use iop_core::{Ctx, GPR_A0, GPR_GP, GPR_RA, GPR_SP, GPR_V0, REG_BYTES};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering::Relaxed;

/// A recompiled IOP module (generated table, tools/gen_iop.py).
pub struct IopModule {
    pub name: &'static str,
    pub sha1: &'static str,
    pub base: u32,
    pub size: u32,
    pub entry: u32,
    pub gp: u32,
    pub funcs: &'static [(u32, u32, fn(&mut Ctx, u32))],
    pub imports: &'static [(u32, &'static str, u32)],
    pub exports: &'static [(&'static str, u32, u32)],
}

/// IOP clock (Hz) for its timers and the system clock of thbase.
pub const IOP_HZ: u64 = 36_864_000;
const HOST_STACK: usize = 16 << 20;
/// Time slice of an IOP thread inside an EE service point (EE cycles).
const SLICE: u64 = crate::EE_HZ / 10_000;
/// Host time the IOP may use per EE service point (the IOP is a separate CPU: a polling IOP
/// thread must not starve the EE).
const IOP_SHARE: std::time::Duration = std::time::Duration::from_micros(150);
const IRQ_STACK: u32 = 0x0001_D000;

type Co = Coroutine<(), (), (), DefaultStack>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Wait {
    None,
    Sleep,
    Sema(u32),
    Event { id: u32, bits: u32, mode: u32, res: u32 },
    Delay(u64),
    Rpc(u32),
    Vpl(u32),
}

pub struct IThread {
    pub id: u32,
    pub ready: bool,
    pub dormant: bool,
    pub prio: u32,
    pub init_prio: u32,
    pub entry: u32,
    pub stack: u32,
    pub stack_size: u32,
    pub gp: u32,
    pub attr: u32,
    pub option: u32,
    pub wait: Wait,
    pub wakeups: u32,
    pub arg: u32,
    pub args: Option<(u32, u32)>,
    seq: u64,
    saved: Option<Box<[u8; REG_BYTES]>>,
    pub(crate) co: Option<Co>,
    yielder: *const Yielder<(), ()>,
    pub wake_result: i32,
}

pub struct Sema {
    pub count: i32,
    pub max: i32,
    pub attr: u32,
    pub option: u32,
    pub waiters: VecDeque<u32>,
}

pub struct EventFlag {
    pub bits: u32,
    pub attr: u32,
    pub option: u32,
}

pub struct Pool {
    pub base: u32,
    pub size: u32,
    pub block: u32,
    pub used: Vec<(u32, u32)>,
}

#[derive(Clone, Copy)]
pub struct HwTimer {
    pub id: u32,
    pub source: u32,
    pub prescale: u32,
    pub compare: u32,
    pub handler: u32,
    pub arg: u32,
    pub gp: u32,
    pub running: bool,
    pub start: u64,
    pub next: u64,
}

#[derive(Clone, Copy)]
pub struct Alarm {
    pub id: u32,
    pub due: u64,
    pub handler: u32,
    pub arg: u32,
    pub gp: u32,
}

/// A pending RPC request for a server the modules registered.
#[derive(Clone)]
pub struct Req {
    pub sd: u32,
    pub fno: u32,
    pub size: u32,
    pub recv: u32,
    pub recv_size: u32,
    pub rmode: u32,
    pub pkt: Vec<u8>,
}

pub struct RpcServer {
    pub sd: u32,
    pub sid: u32,
    pub func: u32,
    pub buff: u32,
    pub cfunc: u32,
    pub cbuff: u32,
    pub qd: u32,
    pub gp: u32,
}

pub struct IopExec {
    pub ctx: Box<Ctx>,
    pub table: &'static [IopModule],
    pub loaded: Vec<&'static IopModule>,
    funcs: Vec<(u32, u32, fn(&mut Ctx, u32))>,
    pub stubs: HashMap<u32, (&'static str, u32)>,
    pub exports: HashMap<(String, u32), u32>,
    pub threads: Vec<Option<IThread>>,
    pub current: usize,
    pub in_thread: bool,
    pub in_irq: bool,
    seq: u64,
    pub slice_end: u64,
    pub semas: HashMap<u32, Sema>,
    pub events: HashMap<u32, EventFlag>,
    pub pools: HashMap<u32, Pool>,
    pub next_obj: u32,
    pub heap: Vec<(u32, u32)>,
    pub intr: HashMap<u32, (u32, u32, u32)>,
    pub intr_enabled: u64,
    pub cpu_ie: bool,
    pub timers: Vec<HwTimer>,
    pub alarms: Vec<Alarm>,
    pub servers: HashMap<u32, RpcServer>,
    pub queues: HashMap<u32, (u32, VecDeque<Req>)>,
    pub current_req: HashMap<u32, Req>,
    pub cmd_handlers: HashMap<u32, (u32, u32, u32)>,
    pub running: bool,
    pub dma: [super::hw::DmaCh; 2],
    pub pending_irq: u64,
    pub files: HashMap<i32, super::fileio::File>,
    pub next_fd: i32,
    pub cd_busy_until: u64,
    /// IOP RPC clients whose call to the EE has not ended yet.
    pub rpc_busy: std::collections::HashSet<u32>,
}

pub const HEAP_END: u32 = 0x001E_0000;

impl IopExec {
    pub fn new(table: &'static [IopModule], ram: *mut u8, scratch: *mut u8) -> IopExec {
        IopExec {
            ctx: Box::new(Ctx::new(ram, scratch, &IOP_HOST)),
            table,
            loaded: Vec::new(),
            funcs: Vec::new(),
            stubs: HashMap::new(),
            exports: HashMap::new(),
            threads: (0..128).map(|_| None).collect(),
            current: 0,
            in_thread: false,
            in_irq: false,
            seq: 1,
            slice_end: 0,
            semas: HashMap::new(),
            events: HashMap::new(),
            pools: HashMap::new(),
            next_obj: 1,
            heap: Vec::new(),
            intr: HashMap::new(),
            intr_enabled: 0,
            cpu_ie: true,
            timers: Vec::new(),
            alarms: Vec::new(),
            servers: HashMap::new(),
            queues: HashMap::new(),
            current_req: HashMap::new(),
            cmd_handlers: HashMap::new(),
            running: false,
            dma: Default::default(),
            pending_irq: 0,
            files: HashMap::new(),
            next_fd: 0,
            cd_busy_until: 0,
            rpc_busy: Default::default(),
        }
    }

    // ---- memory ----

    /// Start of the IOP heap: right after the recompiled modules' slots (64 KiB each, see
    /// tools/iop_modules.py), as on the IOP where free memory begins after the loaded modules.
    /// Modules of the upper area (0x100000.., the Arcade History titles') count only once
    /// loaded, so the main game's heap does not depend on them.
    fn heap_start(&self) -> u32 {
        let end = |m: &IopModule| (m.base + m.size + 0xFFFF) & !0xFFFF;
        let lower = self.table.iter().filter(|m| m.base < 0x0010_0000).map(end).max().unwrap_or(0);
        let loaded = self.loaded.iter().map(|m| end(m)).max().unwrap_or(0);
        lower.max(loaded).max(0x0002_0000)
    }

    /// AllocSysMemory: 0 = lowest fit, 1 = highest fit, 2 = at `at`.
    pub fn alloc(&mut self, mode: u32, size: u32, at: u32) -> u32 {
        let heap_start = self.heap_start();
        let size = (size + 255) & !255;
        if size == 0 {
            return 0;
        }
        self.heap.sort();
        let fits = |s: u32, heap: &Vec<(u32, u32)>| heap.iter().all(|&(a, n)| s + size <= a || s >= a + n);
        let a = if mode == 2 {
            let a = at & !255;
            if a < heap_start || a + size > HEAP_END || !fits(a, &self.heap) {
                return 0;
            }
            a
        } else {
            let mut cands = vec![heap_start];
            cands.extend(self.heap.iter().map(|&(a, n)| (a + n + 255) & !255));
            if mode == 1 {
                cands.extend(self.heap.iter().map(|&(a, _)| a.saturating_sub(size) & !255));
                cands.push((HEAP_END - size) & !255);
                cands.sort();
                cands.reverse();
            }
            match cands.into_iter().find(|&s| s >= heap_start && s + size <= HEAP_END && fits(s, &self.heap)) {
                Some(s) => s,
                None => return 0,
            }
        };
        self.heap.push((a, size));
        a
    }

    pub fn free(&mut self, a: u32) -> bool {
        let n = self.heap.len();
        self.heap.retain(|&(x, _)| x != a);
        self.heap.len() != n
    }

    pub fn max_free(&mut self) -> u32 {
        let heap_start = self.heap_start();
        self.heap.sort();
        let mut best = 0;
        let mut cur = heap_start;
        for &(a, n) in &self.heap {
            best = best.max(a.saturating_sub(cur));
            cur = cur.max(a + n);
        }
        best.max(HEAP_END.saturating_sub(cur))
    }

    pub fn total_free(&self) -> u32 {
        let heap_start = self.heap_start();
        (HEAP_END - heap_start) - self.heap.iter().map(|x| x.1).sum::<u32>()
    }

    fn lookup(&self, addr: u32) -> Option<fn(&mut Ctx, u32)> {
        let i = self.funcs.partition_point(|(s, _, _)| *s <= addr);
        (i > 0).then(|| self.funcs[i - 1]).filter(|&(_, e, _)| addr < e).map(|(_, _, f)| f)
    }

    // ---- threads ----

    pub fn cur(&mut self) -> Option<&mut IThread> {
        let c = self.current;
        self.threads.get_mut(c).and_then(|t| t.as_mut())
    }

    pub fn make_ready(&mut self, tid: u32) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(t) = self.threads.get_mut(tid as usize).and_then(|t| t.as_mut()) {
            t.ready = true;
            t.wait = Wait::None;
            t.seq = seq;
        }
    }

    fn pick(&self) -> Option<usize> {
        self.threads
            .iter()
            .flatten()
            .filter(|t| t.ready && !t.dormant)
            .min_by_key(|t| (t.prio, t.seq))
            .map(|t| t.id as usize)
    }

    pub fn new_thread(&mut self, entry: u32, prio: u32, stack_size: u32, attr: u32, option: u32, gp: u32) -> i32 {
        let Some(id) = (1..self.threads.len()).find(|&i| self.threads[i].is_none()) else { return -1 };
        let stack = self.alloc(1, stack_size.max(0x400), 0);
        if stack == 0 {
            return -400; // KE_NO_MEMORY
        }
        self.threads[id] = Some(IThread {
            id: id as u32,
            ready: false,
            dormant: true,
            prio,
            init_prio: prio,
            entry,
            stack,
            stack_size,
            gp,
            attr,
            option,
            wait: Wait::None,
            wakeups: 0,
            arg: 0,
            args: None,
            seq: 0,
            saved: None,
            co: None,
            yielder: std::ptr::null(),
            wake_result: 0,
        });
        id as i32
    }

    pub fn start_thread(&mut self, tid: u32, arg: u32, args: Option<(u32, u32)>) -> i32 {
        let Some(t) = self.threads.get_mut(tid as usize).and_then(|t| t.as_mut()) else { return -406 };
        if !t.dormant {
            return -414; // KE_NOT_DORMANT
        }
        t.dormant = false;
        t.co = None;
        t.arg = arg;
        t.args = args;
        t.prio = t.init_prio;
        t.wakeups = 0;
        let mut regs = Box::new([0u8; REG_BYTES]);
        let w = |r: &mut [u8; REG_BYTES], n: usize, v: u32| r[4 * n..4 * n + 4].copy_from_slice(&v.to_le_bytes());
        match args {
            Some((argc, argv)) => {
                w(&mut regs, GPR_A0, argc);
                w(&mut regs, GPR_A0 + 1, argv);
            }
            None => w(&mut regs, GPR_A0, arg),
        }
        w(&mut regs, GPR_GP, t.gp);
        w(&mut regs, GPR_SP, (t.stack + t.stack_size - 0x10) & !0xF);
        t.saved = Some(regs);
        self.make_ready(tid);
        tid as i32
    }
}

// ---------------------------------------------------------------------------------------------
// The IOP scheduler (called from the EE's service points)

fn exec(m: &mut Machine) -> &mut IopExec {
    &mut m.iop.exec
}

/// Run IOP threads until all wait, deliver due IOP interrupts first.
pub fn run(m: &mut Machine) {
    let x = exec(m);
    if x.running || x.loaded.is_empty() {
        return;
    }
    x.running = true;
    let t_iop = std::time::Instant::now();
    let deadline = std::time::Instant::now() + IOP_SHARE;
    loop {
        interrupts(m, None);
        let x = exec(m);
        let Some(tid) = x.pick() else { break };
        run_thread(m, tid);
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    crate::diag::TIME[1].fetch_add(t_iop.elapsed().as_micros() as u64, Relaxed);
    exec(m).running = false;
}

fn run_thread(m: &mut Machine, tid: usize) {
    let now = m.now();
    let x = exec(m);
    x.current = tid;
    x.slice_end = now + SLICE;
    let t = x.threads[tid].as_mut().unwrap();
    if let Some(r) = t.saved.take() {
        x.ctx.regs.copy_from_slice(&r[..]);
        x.ctx.next = 0;
    }
    if t.co.is_none() {
        let entry = t.entry;
        let stack = DefaultStack::new(HOST_STACK).expect("IOP thread stack");
        t.co = Some(Coroutine::with_stack(stack, move |y: &Yielder<(), ()>, ()| {
            let m = crate::machine();
            if let Some(t) = m.iop.exec.threads[tid].as_mut() {
                t.yielder = y as *const _;
            }
            let c: &mut Ctx = unsafe { &mut *(m.iop.exec.ctx.as_mut() as *mut Ctx) };
            c.call(entry);
            let m = crate::machine();
            if let Some(t) = m.iop.exec.threads[tid].as_mut() {
                t.dormant = true;
                t.ready = false;
            }
        }));
    }
    x.in_thread = true;
    let co = x.threads[tid].as_mut().unwrap().co.as_mut().unwrap() as *mut Co;
    let r = unsafe { (*co).resume(()) };
    let x = exec(crate::machine());
    x.in_thread = false;
    unsafe { iop_core::ABORT = false };
    match r {
        CoroutineResult::Yield(()) => {
            if let Some(t) = x.threads[tid].as_mut() {
                t.saved = Some(Box::new(x.ctx.regs));
            }
        }
        CoroutineResult::Return(()) => {
            if let Some(t) = x.threads[tid].as_mut() {
                t.co = None;
                t.saved = None;
            }
        }
    }
    x.current = 0;
}

/// Suspend the running IOP thread (blocked, preempted or ended).
pub fn switch_out(m: &mut Machine) {
    let x = exec(m);
    if !x.in_thread || x.in_irq {
        return;
    }
    let y = x.cur().map(|t| t.yielder).unwrap_or(std::ptr::null());
    if !y.is_null() {
        unsafe { (*y).suspend(()) };
    }
}

/// Block the running IOP thread with `wait`; returns its wake result.
pub fn block(m: &mut Machine, wait: Wait) -> i32 {
    if let Some(t) = exec(m).cur() {
        t.ready = false;
        t.wait = wait;
    }
    switch_out(m);
    exec(crate::machine()).cur().map(|t| t.wake_result).unwrap_or(0)
}

/// After waking threads from a thread: let a higher-priority one run.
pub fn maybe_preempt(m: &mut Machine) {
    let x = exec(m);
    if !x.in_thread || x.in_irq {
        return;
    }
    let Some(cur) = x.threads.get(x.current).and_then(|t| t.as_ref()) else { return };
    let p = cur.prio;
    if x.threads.iter().flatten().any(|t| t.ready && !t.dormant && t.id as usize != x.current && t.prio < p) {
        switch_out(m);
    }
}

/// Wake threads whose delay is over; deliver timer / alarm interrupts. `c`: the running IOP
/// context (its registers are saved around the handlers).
pub fn interrupts(m: &mut Machine, _c: Option<&mut Ctx>) {
    let now = m.now();
    let x = exec(m);
    let due: Vec<u32> = x.threads.iter().flatten().filter(|t| matches!(t.wait, Wait::Delay(d) if d <= now)).map(|t| t.id).collect();
    for t in due {
        x.make_ready(t);
    }
    if x.in_irq || !x.cpu_ie {
        return;
    }
    let mut calls: Vec<(u32, u32, Vec<u32>, Option<usize>)> = Vec::new();
    for (i, t) in x.timers.iter().enumerate() {
        if t.running && t.handler != 0 && t.next <= now {
            calls.push((t.handler, t.gp, vec![t.arg], Some(i)));
        }
    }
    let pend = x.pending_irq & x.intr_enabled;
    for irq in 0..64u32 {
        if pend & (1u64 << irq) != 0 {
            if let Some(&(h, arg, gp)) = x.intr.get(&irq) {
                x.pending_irq &= !(1u64 << irq);
                calls.push((h, gp, vec![arg], None));
            }
        }
    }
    let alarms: Vec<Alarm> = x.alarms.iter().filter(|a| a.due <= now).copied().collect();
    x.alarms.retain(|a| a.due > now);
    for a in alarms {
        calls.push((a.handler, a.gp, vec![a.arg], None));
    }
    if calls.is_empty() {
        return;
    }
    x.in_irq = true;
    let saved = Box::new(x.ctx.regs);
    let saved_next = x.ctx.next;
    for (handler, gp, args, timer) in calls {
        let x = exec(crate::machine());
        let c = x.ctx.as_mut();
        for (i, a) in args.iter().enumerate() {
            c.set_gpr(GPR_A0 + i, *a);
        }
        c.set_gpr(GPR_GP, gp);
        c.set_gpr(GPR_SP, IRQ_STACK);
        c.set_gpr(GPR_RA, 0);
        c.next = 0;
        let c: &mut Ctx = unsafe { &mut *(c as *mut Ctx) };
        c.call(handler);
        let ret = c.gpr(GPR_V0);
        let x = exec(crate::machine());
        if let Some(i) = timer {
            // timrman: the handler returns the next compare value (0 stops the timer).
            let t = &mut x.timers[i];
            if ret == 0 {
                t.running = false;
            } else {
                t.compare = ret;
                t.next += timer_cycles(t, ret).max(1);
                let now = crate::machine().now();
                if t.next <= now {
                    t.next = now + timer_cycles(t, ret).max(1);
                }
            }
        }
    }
    let x = exec(crate::machine());
    x.ctx.regs.copy_from_slice(&saved[..]);
    x.ctx.next = saved_next;
    x.in_irq = false;
}

/// EE cycles for `ticks` of a hardware timer.
pub fn timer_cycles(t: &HwTimer, ticks: u32) -> u64 {
    let hz = if t.source == 1 { IOP_HZ / t.prescale.max(1) as u64 } else { 15_734 };
    ticks as u64 * crate::EE_HZ / hz.max(1)
}

pub fn next_event(m: &Machine) -> u64 {
    let x = &m.iop.exec;
    let mut n = u64::MAX;
    for t in x.threads.iter().flatten() {
        if let Wait::Delay(d) = t.wait {
            n = n.min(d);
        }
        if t.ready && !t.dormant {
            n = 0;
        }
    }
    for t in &x.timers {
        if t.running && t.handler != 0 {
            n = n.min(t.next);
        }
    }
    for a in &x.alarms {
        n = n.min(a.due);
    }
    n.min(super::hw::next_event(m))
}

// ---------------------------------------------------------------------------------------------
// Loading

/// Relocate an IRX image (in IOP RAM at `src`) to `base` (64 KiB aligned) and return the size
/// of the loaded image, or None if it is not an IRX.
fn relocate(m: &mut Machine, src: u32, base: u32, size: u32) -> Option<u32> {
    let img = m.iop.read(src, 0x40000);
    if &img[..4] != b"\x7fELF" {
        return None;
    }
    let u32_ = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u16_ = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let (shoff, shnum) = (u32_(&img, 32) as usize, u16_(&img, 48));
    let mut mem = vec![0u8; size as usize];
    let mut rels = Vec::new();
    for i in 0..shnum {
        let s = shoff + 40 * i;
        let (ty, addr, off, sz) = (u32_(&img, s + 4), u32_(&img, s + 12) as usize, u32_(&img, s + 16) as usize, u32_(&img, s + 20) as usize);
        if ty == 1 && (addr != 0 || i > 0) && sz > 0 && addr + sz <= mem.len() && u32_(&img, s + 8) & 2 != 0 {
            mem[addr..addr + sz].copy_from_slice(&img[off..off + sz]);
        }
        if ty == 9 {
            rels.push((off, sz));
        }
    }
    for (off, sz) in rels {
        for i in (0..sz).step_by(8) {
            let r = u32_(&img, off + i) as usize;
            let t = img[off + i + 4];
            if r + 4 > mem.len() {
                continue;
            }
            let w = u32_(&mem, r);
            let nw = match t {
                2 => w.wrapping_add(base),
                4 => (w & 0xFC00_0000) | ((((w & 0x3FF_FFFF) << 2).wrapping_add(base) >> 2) & 0x3FF_FFFF),
                5 => (w & 0xFFFF_0000) | ((w & 0xFFFF).wrapping_add(base >> 16) & 0xFFFF),
                _ => w,
            };
            mem[r..r + 4].copy_from_slice(&nw.to_le_bytes());
        }
    }
    m.iop.write(base, &mem);
    Some(size)
}

fn sha1_hex(data: &[u8]) -> String {
    // A small SHA-1 (module identification only).
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[4 * i..4 * i + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// The IRX image size in IOP RAM at `addr` (end of section headers or of the last section).
fn irx_size(m: &Machine, addr: u32) -> usize {
    let img = m.iop.read(addr, 0x40000);
    let u32_ = |o: usize| u32::from_le_bytes(img[o..o + 4].try_into().unwrap()) as usize;
    let shoff = u32_(32);
    let shnum = u16::from_le_bytes([img[48], img[49]]) as usize;
    let mut end = shoff + 40 * shnum;
    for i in 0..shnum {
        let s = shoff + 40 * i;
        if s + 40 <= img.len() && u32_(s + 4) != 8 {
            end = end.max(u32_(s + 16) + u32_(s + 20));
        }
    }
    end.min(img.len())
}

/// Load a module the EE passed in IOP RAM: Some(started) if it is one of the recompiled
/// modules, None if not (the caller emulates it).
pub fn load(m: &mut Machine, addr: u32, name: &str) -> Option<i32> {
    let size = irx_size(m, addr);
    let sha = sha1_hex(&m.iop.read(addr, size));
    let table = m.iop.exec.table;
    let Some(md) = table.iter().find(|x| x.sha1 == sha) else {
        diag::log(format!("IOP: module {name} (sha1 {sha}) is not recompiled: emulated"));
        if diag::enabled("T5_DUMPUNK") {
            let _ = std::fs::write(format!("irx_{sha}.bin"), m.iop.read(addr, 0x40000));
        }
        return None;
    };
    if m.iop.exec.loaded.iter().any(|x| x.sha1 == md.sha1) {
        diag::log(format!("IOP: module {name} already resident"));
        return Some(0);
    }
    relocate(m, addr, md.base, md.size)?;
    let x = exec(m);
    x.loaded.push(md);
    x.funcs.extend(md.funcs.iter().copied());
    x.funcs.sort_by_key(|f| f.0);
    for &(stub, lib, idx) in md.imports {
        x.stubs.insert(stub, (lib, idx));
    }
    for &(lib, idx, a) in md.exports {
        x.exports.insert((lib.to_string(), idx), a);
    }
    diag::log(format!("IOP: start module {} at {:#010x}", md.name, md.base));
    // _start(argc = 1, argv = [name]) on a loader thread at a high priority.
    let argv = x.alloc(0, 0x40, 0);
    let tid = x.new_thread(md.entry, 8, 0x1000, 0x0200_0000, 0, md.gp) as u32;
    let mut argb = name.as_bytes().to_vec();
    argb.push(0);
    m.iop.write(argv + 8, &argb);
    m.iop.write(argv, &(argv + 8).to_le_bytes());
    m.iop.write(argv + 4, &0u32.to_le_bytes());
    let x = exec(m);
    x.start_thread(tid, 0, Some((1, argv)));
    // Run it (and whatever it starts) now.
    let was = x.running;
    x.running = false;
    run(m);
    exec(m).running = was;
    Some(0)
}

// ---------------------------------------------------------------------------------------------
// Host table of the IOP CPU

pub static IOP_HOST: iop_core::Host = iop_core::Host {
    io_read: super::hw::io_read,
    io_write: super::hw::io_write,
    service: iop_service,
    hle: super::hle::call,
    dispatch: iop_dispatch,
    syscall: |_, n| diag::once(format!("IOP syscall {n:#x}")),
    brk: |_, n| diag::once(format!("IOP break {n:#x}")),
    mtc0: |_, r, v| {
        if r == 12 {
            crate::machine().iop.exec.cpu_ie = v & 1 != 0;
        }
    },
    mfc0: |_, r| if r == 12 { crate::machine().iop.exec.cpu_ie as u32 | 0x400 } else { 0 },
    intrinsic: |_, name, _| diag::once(format!("IOP intrinsic {name}")),
    log: diag::log,
};

fn iop_dispatch(c: &mut Ctx, addr: u32) -> bool {
    let a = addr & 0x1F_FFFF;
    match crate::machine().iop.exec.lookup(a) {
        Some(f) => {
            f(c, a);
            true
        }
        None => {
            diag::once(format!("IOP: unknown call target {addr:#010x} (from {:#010x})", iop_core::LAST_FN.load(Relaxed)));
            false
        }
    }
}

/// IOP service point (loop back-edges / entries of IOP code): timers, alarms, time slice.
fn iop_service(_c: &mut Ctx) {
    iop_core::ATTENTION.store(false, Relaxed);
    let m = crate::machine();
    interrupts(m, None);
    let now = m.now();
    let x = exec(m);
    if x.in_thread && !x.in_irq && now >= x.slice_end {
        // Used up its slice (a polling loop): let the EE run, continue later.
        switch_out(m);
    }
}

/// Make the IOP's code service its timers while the EE runs (called by the EE ticker path).
pub fn tick() {
    iop_core::ATTENTION.store(true, Relaxed);
}
