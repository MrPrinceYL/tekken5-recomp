//! The stable ABI between generated code and the PlayStation 2 runtime.
//!
//! The generated crates (`t5-<variant>-NN`) depend only on this crate. It holds the Emotion Engine
//! CPU context (the R5900 register file: 128-bit GPRs, HI/LO, SA, the FPU, COP0 and the VU0
//! registers that COP2 macro instructions use, in a compact layout), p-code operator semantics,
//! the inline RAM / scratchpad fast paths and a table of function pointers ([`Host`]) through
//! which everything else (I/O registers, the kernel, interrupts, dispatch) reaches `ps2-runtime`.
//! Changing the runtime never rebuilds the generated crates; only changing this crate does.

pub mod fpu;
pub mod mmi;
pub mod ops;
pub mod vu0;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// Main RAM: 32 MiB at physical 0 (seen at 0x00000000, 0x20000000 uncached, 0x30000000
/// uncached-accelerated, 0x80000000 KSEG0 and 0xA0000000 KSEG1).
pub const RAM_SIZE: usize = 0x0200_0000;
/// The 16 KiB scratchpad RAM at 0x70000000.
pub const SCRATCH_BASE: u32 = 0x7000_0000;
pub const SCRATCH_SIZE: usize = 0x4000;

/// Register file layout (bytes). `tools/gen_rust.py` maps Ghidra's R5900 register offsets onto
/// it (REG_BLOCKS there). GPR n is 16 bytes at 16*n (r0 reads as zero: the generator never
/// writes it).
pub const REG_HI: usize = 0x210; // HI (64), HI1 (64): the 128-bit HI
pub const REG_LO: usize = 0x220; // LO, LO1
pub const REG_SA: usize = 0x238; // shift amount (qfsrv)
pub const REG_FPR: usize = 0x240; // f0..f31, 4 bytes each, in Ghidra's pair order (see fpr())
pub const REG_FCR: usize = 0x2C0; // fir fccr fexr fenr fcsr (FCR0 / FCR31 = fir / fcsr)
pub const REG_FACC: usize = 0x2D4; // FPU accumulator (madd.s, ...)
pub const REG_COP0: usize = 0x2E0; // COP0 registers 0..31
pub const REG_VI: usize = 0x390; // VU0 vi0..vi15, then control registers vc0..vc15
pub const REG_VUCR: usize = 0x410; // Ghidra's vucr block (unused by the generator)
pub const REG_VF: usize = 0x500; // VU0 vf0..vf31, 16 bytes each: x, y, z, w at +0, +4, +8, +12
pub const REG_VACC: usize = 0x700; // VU0 ACC (x, y, z, w)
pub const REG_BYTES: usize = 0x800;

/// VU0 control registers (cfc2 / ctc2 numbers 16..31) inside REG_VI.
pub const VI_STATUS: usize = 16;
pub const VI_MAC: usize = 17;
pub const VI_CLIP: usize = 18;
pub const VI_R: usize = 20;
pub const VI_I: usize = 21;
pub const VI_Q: usize = 22;
pub const VI_P: usize = 23;
pub const VI_TPC: usize = 26;
pub const VI_CMSAR0: usize = 27;
pub const VI_FBRST: usize = 28;
pub const VI_VPU_STAT: usize = 29;
pub const VI_CMSAR1: usize = 31;

pub const GPR_V0: usize = 2;
pub const GPR_V1: usize = 3;
pub const GPR_A0: usize = 4;
pub const GPR_T0: usize = 8;
pub const GPR_GP: usize = 28;
pub const GPR_SP: usize = 29;
pub const GPR_FP: usize = 30;
pub const GPR_RA: usize = 31;

/// Runtime services, installed once by `ps2-runtime`.
pub struct Host {
    /// Loads/stores outside RAM and the scratchpad (I/O registers, VU memory, GS, BIOS ROM).
    pub io_read: fn(&mut Ctx, u32, usize) -> u128,
    pub io_write: fn(&mut Ctx, u32, usize, u128),
    /// Run due device events and deliver pending interrupts (called when [`ATTENTION`] is set).
    pub service: fn(&mut Ctx),
    /// HLE handler for a hooked function; true if it handled the call.
    pub hle: fn(&mut Ctx, u32) -> bool,
    /// Run the recompiled function containing an address; false if there is none.
    pub dispatch: fn(&mut Ctx, u32) -> bool,
    /// `syscall` (number in v1), `break`.
    pub syscall: fn(&mut Ctx, u32),
    pub brk: fn(&mut Ctx, u32),
    /// COP0 register access (Status, Count, Compare, ...), `ei` / `di`.
    pub cop0_read: fn(&mut Ctx, u32) -> u32,
    pub cop0_write: fn(&mut Ctx, u32, u32),
    pub ei: fn(&mut Ctx, bool),
    /// VU0 micro mode (`vcallms` / `vcallmsr`, ctc2 to CMSAR0 / FBRST) and its busy state
    /// (`bc2f` / `bc2t`, cfc2 of VPU-STAT).
    pub vu0_call: fn(&mut Ctx, u32),
    pub vu0_busy: fn(&mut Ctx) -> bool,
    /// Unhandled CALLOTHER intrinsics (counted, logged once).
    pub intrinsic: fn(&mut Ctx, &'static str, u32),
    pub log: fn(String),
}

pub static FN_ENTRIES: AtomicU64 = AtomicU64::new(0);
pub static CALLS: AtomicU64 = AtomicU64::new(0);
/// Entries into a function at a non-leader address (see `Ctx::block_miss`).
pub static BLOCK_MISSES: AtomicU64 = AtomicU64::new(0);
pub static UNKNOWN_TARGETS: AtomicU64 = AtomicU64::new(0);
pub static LAST_FN: AtomicU32 = AtomicU32::new(0);

/// Function coverage (diagnostics, `T5_COVER`): one bit per word address of EE RAM.
pub static COVER_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static COVER: std::sync::OnceLock<Vec<AtomicU32>> = std::sync::OnceLock::new();

fn cover_map() -> &'static Vec<AtomicU32> {
    COVER.get_or_init(|| (0..(0x200_0000 >> 7)).map(|_| AtomicU32::new(0)).collect())
}

#[cold]
fn cover_mark(addr: u32) {
    let w = ((addr & 0x1FF_FFFF) >> 2) as usize;
    cover_map()[w >> 5].fetch_or(1 << (w & 31), Relaxed);
}

/// The function addresses entered since the last call (and clear them).
pub fn cover_take() -> Vec<u32> {
    let mut out = Vec::new();
    for (i, w) in cover_map().iter().enumerate() {
        let mut b = w.swap(0, Relaxed);
        while b != 0 {
            let k = b.trailing_zeros();
            out.push(((i as u32) << 5 | k) << 2);
            b &= b - 1;
        }
    }
    out
}
/// Address of the last loop back-edge taken (diagnostics: where a busy loop spins).
pub static LAST_LOOP: AtomicU32 = AtomicU32::new(0);
/// Set by the runtime whenever device time has advanced or an interrupt is waiting.
pub static ATTENTION: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn bump(c: &AtomicU64) {
    c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
}

#[inline(always)]
pub fn get_le(buf: &[u8], off: usize, size: usize) -> u64 {
    match size {
        1 => buf[off] as u64,
        2 => u16::from_le_bytes([buf[off], buf[off + 1]]) as u64,
        4 => u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as u64,
        8 => u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()),
        _ => {
            let mut v = 0u64;
            for i in 0..size.min(8) {
                v |= (buf[off + i] as u64) << (8 * i);
            }
            v
        }
    }
}
#[inline(always)]
pub fn put_le(buf: &mut [u8], off: usize, size: usize, v: u64) {
    match size {
        1 => buf[off] = v as u8,
        2 => buf[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes()),
        4 => buf[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes()),
        8 => buf[off..off + 8].copy_from_slice(&v.to_le_bytes()),
        _ => {
            for i in 0..size.min(8) {
                buf[off + i] = (v >> (8 * i)) as u8;
            }
        }
    }
}
#[inline(always)]
pub fn get128(buf: &[u8], off: usize) -> u128 {
    u128::from_le_bytes(buf[off..off + 16].try_into().unwrap())
}
#[inline(always)]
pub fn put128(buf: &mut [u8], off: usize, v: u128) {
    buf[off..off + 16].copy_from_slice(&v.to_le_bytes());
}

/// Main RAM offset of an EE address, if it is RAM.
#[inline(always)]
pub fn ram_off(addr: u32) -> Option<usize> {
    if addr < RAM_SIZE as u32 {
        return Some(addr as usize);
    }
    let phys = match addr >> 28 {
        2 | 3 => addr & 0x0FFF_FFFF,
        8..=0xB => addr & 0x1FFF_FFFF,
        _ => return None,
    };
    (phys < RAM_SIZE as u32).then_some(phys as usize)
}

/// One guest CPU (the EE core).
///
/// The register file lives inline: generated functions take `&mut Ctx`, which tells LLVM nothing
/// else aliases it, so guest registers can stay in host registers across guest memory accesses.
#[repr(C, align(16))]
pub struct Ctx {
    pub regs: [u8; REG_BYTES],
    /// Main RAM (RAM_SIZE bytes) and the scratchpad, owned by the runtime's machine.
    pub ram: *mut u8,
    pub scratch: *mut u8,
    pub host: &'static Host,
    pub depth: u32,
    /// Set by [`Ctx::call_orig`]: the next entry to this address skips its HLE handler.
    pub bypass: u32,
    /// Targets of the innermost dispatched calls (ring indexed by depth), for diagnostics.
    pub trail: [u32; 64],
    /// Pending tail jump: a jump out of the running function (`j`/`b` to another function,
    /// `jr` through a jump table, falling off its end). The function returns and the nearest
    /// call site continues there ([`Ctx::call`], [`Ctx::settle`]), so jumps consume no host
    /// stack, as on the CPU.
    pub next: u32,
    /// Set when the running guest thread must stop (program switch, thread exit): every
    /// generated function returns at once and the dispatcher unwinds to the runtime.
    pub abort: bool,
}

// SAFETY: guest code runs on one host thread at a time (guest threads are coroutines).
unsafe impl Send for Ctx {}

impl Ctx {
    pub fn new(ram: *mut u8, scratch: *mut u8, host: &'static Host) -> Self {
        Ctx { regs: [0; REG_BYTES], ram, scratch, host, depth: 0, bypass: 0, trail: [0; 64], next: 0, abort: false }
    }

    #[inline(always)]
    pub fn ram(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ram, RAM_SIZE) }
    }
    #[inline(always)]
    pub fn scratch(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.scratch, SCRATCH_SIZE) }
    }

    // Register space (little endian, like the CPU).
    #[inline(always)] pub fn r(&self, off: usize, size: usize) -> u64 { get_le(&self.regs, off, size) }
    #[inline(always)] pub fn wr(&mut self, off: usize, size: usize, v: u64) { put_le(&mut self.regs, off, size, v) }
    #[inline(always)] pub fn r128(&self, off: usize) -> u128 { get128(&self.regs, off) }
    #[inline(always)] pub fn wr128(&mut self, off: usize, v: u128) { put128(&mut self.regs, off, v) }
    #[inline] pub fn gpr(&self, n: usize) -> u64 { if n == 0 { 0 } else { self.r(16 * n, 8) } }
    #[inline] pub fn gpr32(&self, n: usize) -> u32 { self.gpr(n) as u32 }
    /// Set a GPR's low 64 bits.
    #[inline] pub fn set_gpr(&mut self, n: usize, v: u64) { if n != 0 { self.wr(16 * n, 8, v) } }
    /// Set a GPR to a 32-bit result, sign-extended to 64 bits (as R5900 32-bit operations do).
    #[inline] pub fn set_gpr32(&mut self, n: usize, v: u32) { self.set_gpr(n, v as i32 as i64 as u64) }
    #[inline] pub fn gpr128(&self, n: usize) -> u128 { if n == 0 { 0 } else { self.r128(16 * n) } }
    #[inline] pub fn set_gpr128(&mut self, n: usize, v: u128) { if n != 0 { self.wr128(16 * n, v) } }

    /// Guest load of 1, 2, 4 or 8 bytes. Unaligned accesses (paired lwl/lwr, ldl/ldr) are
    /// performed bytewise.
    #[inline(always)]
    pub fn ld(&mut self, addr: u32, size: usize) -> u64 {
        if let Some(o) = ram_off(addr) {
            if o + size <= RAM_SIZE {
                return get_le(self.ram(), o, size);
            }
        }
        self.ld_slow(addr, size)
    }

    #[inline(never)]
    fn ld_slow(&mut self, addr: u32, size: usize) -> u64 {
        if let Some(o) = ram_off(addr) {
            let mut v = 0;
            for i in 0..size {
                v |= (self.ram()[(o + i) & (RAM_SIZE - 1)] as u64) << (8 * i);
            }
            return v;
        }
        let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
        if s < SCRATCH_SIZE {
            let mut v = 0;
            for i in 0..size {
                v |= (self.scratch()[(s + i) & (SCRATCH_SIZE - 1)] as u64) << (8 * i);
            }
            return v;
        }
        (self.host.io_read)(self, addr, size) as u64
    }

    #[inline(always)]
    pub fn st(&mut self, addr: u32, size: usize, v: u64) {
        if let Some(o) = ram_off(addr) {
            if o + size <= RAM_SIZE {
                return put_le(self.ram(), o, size, v);
            }
        }
        self.st_slow(addr, size, v)
    }

    #[inline(never)]
    fn st_slow(&mut self, addr: u32, size: usize, v: u64) {
        if let Some(o) = ram_off(addr) {
            for i in 0..size {
                self.ram()[(o + i) & (RAM_SIZE - 1)] = (v >> (8 * i)) as u8;
            }
            return;
        }
        let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
        if s < SCRATCH_SIZE {
            for i in 0..size {
                self.scratch()[(s + i) & (SCRATCH_SIZE - 1)] = (v >> (8 * i)) as u8;
            }
            return;
        }
        (self.host.io_write)(self, addr, size, v as u128)
    }

    /// 128-bit load (lq, lqc2). The R5900 ignores the low four address bits; Ghidra's p-code
    /// masks them already, the mask here covers the runtime's own callers.
    #[inline(always)]
    pub fn ld128(&mut self, addr: u32) -> u128 {
        let addr = addr & !15;
        if let Some(o) = ram_off(addr) {
            return get128(self.ram(), o);
        }
        let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
        if s < SCRATCH_SIZE {
            return get128(self.scratch(), s);
        }
        (self.host.io_read)(self, addr, 16)
    }

    #[inline(always)]
    pub fn st128(&mut self, addr: u32, v: u128) {
        let addr = addr & !15;
        if let Some(o) = ram_off(addr) {
            return put128(self.ram(), o, v);
        }
        let s = addr.wrapping_sub(SCRATCH_BASE) as usize;
        if s < SCRATCH_SIZE {
            return put128(self.scratch(), s, v);
        }
        (self.host.io_write)(self, addr, 16, v)
    }

    /// Called at the top of every generated function: bookkeeping, device service and the HLE
    /// check (`hooked` is compiled in from sheets/hle.csv: programs share addresses). Returns true if the function must not run (an HLE handler handled the call, or the
    /// guest thread is being torn down).
    #[inline(always)]
    pub fn enter(&mut self, addr: u32, entry: u32, hooked: bool) -> bool {
        bump(&FN_ENTRIES);
        LAST_FN.store(addr, Relaxed);
        if COVER_ON.load(Relaxed) {
            cover_mark(addr);
        }
        let bypass = self.bypass == addr;
        if bypass {
            self.bypass = 0;
        }
        if ATTENTION.load(Relaxed) {
            (self.host.service)(self);
        }
        if self.abort {
            return true;
        }
        if bypass || entry != addr || !hooked {
            return false;
        }
        (self.host.hle)(self, addr)
    }

    /// Loop back-edge: service devices and deliver pending interrupts, as the CPU would between
    /// any two instructions. Without this, a loop that waits for a flag set by an interrupt
    /// handler never sees the interrupt, since it enters no function. Returns true if the guest
    /// thread is being torn down (the generated code returns).
    #[inline(always)]
    pub fn poll(&mut self) -> bool {
        if ATTENTION.load(Relaxed) {
            (self.host.service)(self);
        }
        self.abort
    }

    /// Loop back-edge at `pc`: like [`Ctx::poll`], recording where the loop is.
    #[inline(always)]
    pub fn poll_at(&mut self, pc: u32) -> bool {
        LAST_LOOP.store(pc, Relaxed);
        self.poll()
    }

    /// A function was entered at an address that isn't one of its block leaders.
    #[cold]
    pub fn block_miss(&mut self, func: u32, pc: u32) {
        BLOCK_MISSES.fetch_add(1, Relaxed);
        (self.host.log)(format!("mid-block entry {pc:#010x} in function {func:#010x} (not a block leader)"));
    }

    /// Jump out of the running function: the caller returns right after this.
    #[inline(always)]
    pub fn tail(&mut self, target: u32) {
        self.next = target;
    }

    /// After a direct call: run the tail jump the callee left pending, if any.
    #[inline(always)]
    pub fn settle(&mut self) {
        if self.next != 0 {
            let t = std::mem::take(&mut self.next);
            self.call(t);
        }
    }

    /// Call through the dispatch table (indirect calls), then follow the tail jumps the callee
    /// leaves pending until code returns.
    pub fn call(&mut self, mut target: u32) {
        loop {
            if self.abort {
                self.next = 0;
                return;
            }
            self.call_one(target);
            if self.next == 0 {
                return;
            }
            target = std::mem::take(&mut self.next);
        }
    }

    fn call_one(&mut self, target: u32) {
        bump(&CALLS);
        if self.depth > 50_000 {
            let chain: Vec<String> = (0..64u32)
                .map(|i| self.trail[(self.depth.wrapping_sub(i) & 63) as usize])
                .map(|t| format!("{t:08x}"))
                .collect();
            (self.host.log)(format!("call depth limit hit at {target:#010x}; innermost calls: {}", chain.join(" ")));
            return;
        }
        self.depth += 1;
        self.trail[(self.depth & 63) as usize] = target;
        let found = (self.host.dispatch)(self, target);
        self.depth -= 1;
        if !found {
            UNKNOWN_TARGETS.fetch_add(1, Relaxed);
            (self.host.log)(format!("unknown call target {target:#010x}"));
        }
    }

    /// From an HLE handler: run the original recompiled function at `addr`.
    pub fn call_orig(&mut self, addr: u32) {
        self.bypass = addr;
        self.call(addr);
        self.bypass = 0;
    }

    /// Call a guest function with up to 8 register arguments (a0..a3, t0..t3, the EE ABI);
    /// returns v0. `ra` is preserved.
    pub fn call_guest(&mut self, addr: u32, args: &[u64]) -> u64 {
        for (i, a) in args.iter().enumerate().take(8) {
            self.set_gpr(GPR_A0 + i, *a);
        }
        let ra = self.gpr(GPR_RA);
        self.call(addr);
        self.set_gpr(GPR_RA, ra);
        self.gpr(GPR_V0)
    }

    #[inline] pub fn syscall(&mut self, code: u32) { (self.host.syscall)(self, code) }
    #[inline] pub fn brk(&mut self, code: u32) { (self.host.brk)(self, code) }
    #[inline] pub fn cop0_read(&mut self, reg: u32) -> u64 { (self.host.cop0_read)(self, reg) as i32 as i64 as u64 }
    #[inline] pub fn cop0_write(&mut self, reg: u32, v: u64) { (self.host.cop0_write)(self, reg, v as u32) }
    #[inline] pub fn ei(&mut self, on: bool) { (self.host.ei)(self, on) }

    /// Other CALLOTHER intrinsics (the instruction word identifies the operation).
    #[cold]
    pub fn intrinsic(&mut self, name: &'static str, word: u32) {
        (self.host.intrinsic)(self, name, word);
    }
}
