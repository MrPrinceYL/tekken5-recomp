//! The IOP kernel libraries the recompiled modules import, served at the import stub
//! (`jr ra; addiu zero,zero,n`, compiled with an HLE hook by tools/gen_iop.py). A stub of a
//! library another loaded module exports is forwarded to that module's function.
//!
//! Arguments: a0..a3, then the stack (sp + 0x10 ...), result in v0 (O32).

use super::exec::{block, maybe_preempt, switch_out, timer_cycles, Alarm, EventFlag, HwTimer, Pool, Req, RpcServer, Sema, Wait, IOP_HZ};
use crate::{diag, sif, Machine};
use iop_core::{Ctx, GPR_A0, GPR_GP, GPR_SP, GPR_V0};

const KE_OK: i32 = 0;
const KE_ERROR: i32 = -1;
const KE_UNKNOWN_THID: i32 = -407;
const KE_UNKNOWN_SEMID: i32 = -408;
const KE_UNKNOWN_EVFID: i32 = -409;
const KE_SEMA_ZERO: i32 = -419;
const KE_EVF_COND: i32 = -421;
const KE_CPUDI: i32 = -102;

fn arg(m: &Machine, c: &Ctx, n: usize) -> u32 {
    if n < 4 {
        c.gpr(GPR_A0 + n)
    } else {
        m.iop.r32(c.gpr(GPR_SP).wrapping_add(0x10 + 4 * (n as u32 - 4)))
    }
}

fn w32(m: &mut Machine, a: u32, v: u32) {
    m.iop.write(a, &v.to_le_bytes());
}

fn r32(m: &Machine, a: u32) -> u32 {
    m.iop.r32(a & 0x1F_FFFC)
}

/// Run an import stub: true (handled; v0 set).
pub fn call(c: &mut Ctx, addr: u32) -> bool {
    let m = crate::machine();
    let Some(&(lib, idx)) = m.iop.exec.stubs.get(&addr) else { return false };
    if let Some(&target) = m.iop.exec.exports.get(&(lib.to_string(), idx)) {
        c.call(target);
        return true;
    }
    diag::count_with(|| format!("iop {lib}#{idx}"));
    let r = kernel(m, c, lib, idx);
    if let Some(v) = r {
        c.set_gpr(GPR_V0, v as u32);
    }
    true
}

fn now(m: &Machine) -> u64 {
    m.now()
}

fn kernel(m: &mut Machine, c: &mut Ctx, lib: &str, idx: u32) -> Option<i32> {
    let a = |n: usize| arg(crate::machine(), c, n);
    let x = &mut m.iop.exec;
    Some(match (lib, idx) {
        // ---- sysmem / loadcore ----
        ("sysmem", 4) => x.alloc(a(0), a(1), a(2)) as i32,
        ("sysmem", 5) => if x.free(a(0)) { 0 } else { KE_ERROR },
        ("sysmem", 7) => x.max_free() as i32,
        ("sysmem", 8) => x.total_free() as i32,
        ("sysmem", 14) => {
            let s = format_c(m, c, 0);
            diag::log(format!("IOP Kprintf: {}", s.trim_end()));
            0
        }
        ("loadcore", 4) | ("loadcore", 5) => 0,
        ("loadcore", 6) | ("loadcore", 7) => 0,

        // ---- intrman ----
        ("intrman", 4) => {
            // RegisterIntrHandler(irq, mode, handler, arg)
            x.intr.insert(a(0), (a(2), a(3), c.gpr(GPR_GP)));
            0
        }
        ("intrman", 5) => {
            x.intr.remove(&a(0));
            0
        }
        ("intrman", 6) => {
            x.intr_enabled |= 1u64 << (a(0) & 63);
            0
        }
        ("intrman", 7) => {
            let old = x.intr_enabled >> (a(0) & 63) & 1;
            x.intr_enabled &= !(1u64 << (a(0) & 63));
            if a(1) != 0 {
                w32(m, a(1), old as u32);
            }
            0
        }
        ("intrman", 17) => {
            let was = x.cpu_ie;
            x.cpu_ie = false;
            if a(0) != 0 {
                w32(m, a(0), was as u32);
            }
            if was { KE_OK } else { KE_CPUDI }
        }
        ("intrman", 18) => {
            x.cpu_ie = a(0) != 0;
            0
        }
        ("intrman", 23) => x.in_irq as i32,

        // ---- threads ----
        ("thbase", 4) => {
            let p = a(0);
            let (attr, option, entry, stack, prio) = (r32(m, p), r32(m, p + 4), r32(m, p + 8), r32(m, p + 12), r32(m, p + 16));
            let gp = c.gpr(GPR_GP);
            m.iop.exec.new_thread(entry, prio, stack, attr, option, gp)
        }
        ("thbase", 5) => {
            let t = a(0) as usize;
            match x.threads.get(t).and_then(|t| t.as_ref()) {
                Some(th) if th.dormant => {
                    let stack = th.stack;
                    x.threads[t] = None;
                    x.free(stack);
                    0
                }
                Some(_) => -414,
                None => KE_UNKNOWN_THID,
            }
        }
        ("thbase", 6) => {
            let r = x.start_thread(a(0), a(1), None);
            maybe_preempt(m);
            r
        }
        ("thbase", 8) | ("thbase", 9) => {
            if let Some(t) = x.cur() {
                t.dormant = true;
                t.ready = false;
            }
            unsafe { iop_core::ABORT = true };
            return None;
        }
        ("thbase", 10) | ("thbase", 11) => {
            let t = a(0) as usize;
            if t == x.current {
                return Some(-405);
            }
            match x.threads.get_mut(t).and_then(|t| t.as_mut()) {
                Some(th) => {
                    th.dormant = true;
                    th.ready = false;
                    th.wait = Wait::None;
                    th.co = None;
                    for s in x.semas.values_mut() {
                        s.waiters.retain(|&w| w as usize != t);
                    }
                    0
                }
                None => KE_UNKNOWN_THID,
            }
        }
        ("thbase", 14) | ("thbase", 15) => {
            let t = if a(0) == 0 { x.current } else { a(0) as usize };
            match x.threads.get_mut(t).and_then(|t| t.as_mut()) {
                Some(th) => {
                    th.prio = a(1);
                    maybe_preempt(m);
                    0
                }
                None => KE_UNKNOWN_THID,
            }
        }
        ("thbase", 16) | ("thbase", 17) => {
            if let Some(t) = x.cur() {
                if t.prio == a(0) || a(0) == 0 {
                    t.ready = true;
                }
            }
            let cur = x.current as u32;
            x.make_ready(cur);
            if lib == "thbase" && idx == 16 {
                switch_out(m);
            }
            0
        }
        ("thbase", 20) => x.current as i32,
        ("thbase", 22) | ("thbase", 23) => {
            let t = if a(0) == 0 { x.current } else { a(0) as usize };
            let Some(th) = x.threads.get(t).and_then(|t| t.as_ref()) else { return Some(KE_UNKNOWN_THID) };
            let status = if th.dormant { 0x10 } else if t == x.current { 1 } else if th.ready { 2 } else { 4 };
            let wt = match th.wait { Wait::Sleep => 1, Wait::Delay(_) => 2, Wait::Sema(_) => 3, Wait::Event { .. } => 4, Wait::Vpl(_) => 6, _ => 0 };
            let wid = match th.wait { Wait::Sema(s) | Wait::Rpc(s) | Wait::Vpl(s) => s, Wait::Event { id, .. } => id, _ => 0 };
            let vals = [th.attr, th.option, status, th.entry, th.stack, th.stack_size, th.gp, th.init_prio, th.prio, wt, wid, th.wakeups];
            let info = a(1);
            for (i, v) in vals.iter().enumerate() {
                w32(m, info + 4 * i as u32, *v);
            }
            0
        }
        ("thbase", 24) => {
            if let Some(t) = x.cur() {
                if t.wakeups > 0 {
                    t.wakeups -= 1;
                    return Some(0);
                }
            }
            block(m, Wait::Sleep);
            0
        }
        ("thbase", 25) | ("thbase", 26) => {
            let t = a(0) as usize;
            match x.threads.get_mut(t).and_then(|t| t.as_mut()) {
                Some(th) => {
                    if th.wait == Wait::Sleep {
                        th.wake_result = 0;
                        x.make_ready(t as u32);
                        if idx == 25 {
                            maybe_preempt(m);
                        }
                    } else {
                        th.wakeups += 1;
                    }
                    0
                }
                None => KE_UNKNOWN_THID,
            }
        }
        ("thbase", 27) | ("thbase", 28) => {
            let t = if a(0) == 0 { x.current } else { a(0) as usize };
            match x.threads.get_mut(t).and_then(|t| t.as_mut()) {
                Some(th) => {
                    let n = th.wakeups;
                    th.wakeups = 0;
                    n as i32
                }
                None => KE_UNKNOWN_THID,
            }
        }
        ("thbase", 33) => {
            let until = now(m) + a(0) as u64 * crate::EE_HZ / 1_000_000;
            block(m, Wait::Delay(until));
            0
        }
        ("thbase", 34) => {
            let t = now(m) as u128 * IOP_HZ as u128 / crate::EE_HZ as u128;
            w32(m, a(0), t as u32);
            w32(m, a(0) + 4, (t >> 32) as u32);
            0
        }
        ("thbase", 39) => {
            let t = a(0) as u64 * IOP_HZ / 1_000_000;
            w32(m, a(1), t as u32);
            w32(m, a(1) + 4, (t >> 32) as u32);
            0
        }
        ("thbase", 40) => {
            let t = r32(m, a(0)) as u64 | (r32(m, a(0) + 4) as u64) << 32;
            let us = t * 1_000_000 / IOP_HZ;
            w32(m, a(1), (us / 1_000_000) as u32);
            w32(m, a(2), (us % 1_000_000) as u32);
            0
        }
        ("thbase", 45) => {
            // ReferThreadRunStatus: like ReferThreadStatus, shorter record.
            let t = if a(0) == 0 { x.current } else { a(0) as usize };
            let Some(th) = x.threads.get(t).and_then(|t| t.as_ref()) else { return Some(KE_UNKNOWN_THID) };
            let status = if th.dormant { 0x10 } else if t == x.current { 1 } else if th.ready { 2 } else { 4 };
            let vals = [status, th.prio, 0, 0, 0];
            let info = a(1);
            for (i, v) in vals.iter().enumerate() {
                w32(m, info + 4 * i as u32, *v);
            }
            0
        }
        ("thbase", 47) => {
            // GetThreadmanIdList(type, buf, size, count*): threads (type 1) only.
            let ids: Vec<u32> = x.threads.iter().flatten().map(|t| t.id).collect();
            let (buf, size, cnt) = (a(1), a(2), a(3));
            for (i, id) in ids.iter().take(size as usize).enumerate() {
                w32(m, buf + 4 * i as u32, *id);
            }
            if cnt != 0 {
                w32(m, cnt, ids.len() as u32);
            }
            ids.len().min(size as usize) as i32
        }

        // ---- semaphores ----
        ("thsemap", 4) => {
            let p = a(0);
            let (attr, option, init, max) = (r32(m, p), r32(m, p + 4), r32(m, p + 8) as i32, r32(m, p + 12) as i32);
            let x = &mut m.iop.exec;
            let id = x.next_obj;
            x.next_obj += 1;
            x.semas.insert(id, Sema { count: init, max, attr, option, waiters: Default::default() });
            id as i32
        }
        ("thsemap", 5) => {
            match x.semas.remove(&a(0)) {
                Some(s) => {
                    for w in s.waiters {
                        if let Some(t) = x.threads.get_mut(w as usize).and_then(|t| t.as_mut()) {
                            t.wake_result = -425;
                        }
                        x.make_ready(w);
                    }
                    0
                }
                None => KE_UNKNOWN_SEMID,
            }
        }
        ("thsemap", 6) | ("thsemap", 7) => {
            let id = a(0);
            let Some(s) = x.semas.get_mut(&id) else { return Some(KE_UNKNOWN_SEMID) };
            if let Some(w) = s.waiters.pop_front() {
                if let Some(t) = x.threads.get_mut(w as usize).and_then(|t| t.as_mut()) {
                    t.wake_result = 0;
                }
                x.make_ready(w);
                if idx == 6 {
                    maybe_preempt(m);
                }
            } else if s.count < s.max || s.max <= 0 {
                s.count += 1;
            }
            0
        }
        ("thsemap", 8) => {
            let id = a(0);
            let cur = x.current as u32;
            let Some(s) = x.semas.get_mut(&id) else { return Some(KE_UNKNOWN_SEMID) };
            if s.count > 0 {
                s.count -= 1;
                0
            } else {
                s.waiters.push_back(cur);
                block(m, Wait::Sema(id))
            }
        }
        ("thsemap", 9) | ("thsemap", 10) => {
            let Some(s) = x.semas.get_mut(&a(0)) else { return Some(KE_UNKNOWN_SEMID) };
            if s.count > 0 {
                s.count -= 1;
                0
            } else {
                KE_SEMA_ZERO
            }
        }
        ("thsemap", 11) | ("thsemap", 12) => {
            let Some(s) = x.semas.get(&a(0)) else { return Some(KE_UNKNOWN_SEMID) };
            let vals = [s.attr, s.option, 0, s.max as u32, s.count as u32, s.waiters.len() as u32];
            let info = a(1);
            for (i, v) in vals.iter().enumerate() {
                w32(m, info + 4 * i as u32, *v);
            }
            0
        }

        // ---- event flags ----
        ("thevent", 4) => {
            let p = a(0);
            let (attr, option, bits) = (r32(m, p), r32(m, p + 4), r32(m, p + 8));
            let x = &mut m.iop.exec;
            let id = x.next_obj;
            x.next_obj += 1;
            x.events.insert(id, EventFlag { bits, attr, option });
            id as i32
        }
        ("thevent", 5) => if x.events.remove(&a(0)).is_some() { 0 } else { KE_UNKNOWN_EVFID },
        ("thevent", 6) | ("thevent", 7) => {
            let id = a(0);
            let Some(e) = x.events.get_mut(&id) else { return Some(KE_UNKNOWN_EVFID) };
            e.bits |= a(1);
            wake_events(m, id);
            if idx == 6 {
                maybe_preempt(m);
            }
            0
        }
        ("thevent", 8) | ("thevent", 9) => {
            let Some(e) = x.events.get_mut(&a(0)) else { return Some(KE_UNKNOWN_EVFID) };
            e.bits &= a(1);
            0
        }
        ("thevent", 10) | ("thevent", 11) | ("thevent", 12) => {
            let (id, bits, mode, res) = (a(0), a(1), a(2), a(3));
            let Some(e) = x.events.get_mut(&id) else { return Some(KE_UNKNOWN_EVFID) };
            if event_met(e.bits, bits, mode) {
                let cur = e.bits;
                if mode & 0x10 != 0 {
                    e.bits = 0;
                }
                if res != 0 {
                    w32(m, res, cur);
                }
                0
            } else if idx != 10 {
                KE_EVF_COND
            } else {
                block(m, Wait::Event { id, bits, mode, res })
            }
        }

        // ---- memory pools ----
        ("thfpool", 4) => {
            let p = a(0);
            let (bsize, blocks) = (r32(m, p + 8), r32(m, p + 12));
            let x = &mut m.iop.exec;
            let base = x.alloc(0, bsize * blocks, 0);
            let id = x.next_obj;
            x.next_obj += 1;
            x.pools.insert(id, Pool { base, size: bsize * blocks, block: bsize, used: Vec::new() });
            id as i32
        }
        ("thfpool", 5) | ("thvpool", 5) => {
            if let Some(p) = x.pools.remove(&a(0)) {
                x.free(p.base);
            }
            0
        }
        ("thvpool", 4) => {
            let p = a(0);
            let size = r32(m, p + 8);
            let x = &mut m.iop.exec;
            let base = x.alloc(0, size, 0);
            let id = x.next_obj;
            x.next_obj += 1;
            x.pools.insert(id, Pool { base, size, block: 0, used: Vec::new() });
            id as i32
        }
        ("thvpool", 6) | ("thvpool", 7) | ("thvpool", 8) => {
            let Some(p) = x.pools.get_mut(&a(0)) else { return Some(KE_ERROR) };
            let size = (a(1) + 15) & !15;
            p.used.sort();
            let mut at = p.base;
            for &(u, n) in &p.used {
                if at + size <= u {
                    break;
                }
                at = at.max(u + n);
            }
            if at + size > p.base + p.size {
                return Some(-428); // KE_NO_MEMORY in the pool
            }
            p.used.push((at, size));
            at as i32
        }
        ("thvpool", 9) => {
            let Some(p) = x.pools.get_mut(&a(0)) else { return Some(KE_ERROR) };
            p.used.retain(|&(u, _)| u != a(1));
            0
        }

        // ---- hardware timers ----
        ("timrman", 4) => {
            let id = 0x100 + x.timers.len() as u32 * 2;
            x.timers.push(HwTimer { id, source: a(0), prescale: a(2).max(1), compare: 0, handler: 0, arg: 0, gp: 0, running: false, start: 0, next: u64::MAX });
            id as i32
        }
        ("timrman", 6) => {
            x.timers.retain(|t| t.id != a(0));
            0
        }
        ("timrman", 10) => {
            let n = now(m);
            let x = &mut m.iop.exec;
            let Some(t) = x.timers.iter().find(|t| t.id == a(0)) else { return Some(0) };
            if !t.running {
                return Some(0);
            }
            let hz = if t.source == 1 { IOP_HZ / t.prescale as u64 } else { 15_734 };
            ((n - t.start) as u128 * hz as u128 / crate::EE_HZ as u128) as i32
        }
        ("timrman", 20) => {
            let gp = c.gpr(GPR_GP);
            let Some(t) = x.timers.iter_mut().find(|t| t.id == a(0)) else { return Some(KE_ERROR) };
            t.compare = a(1);
            t.handler = a(2);
            t.arg = a(3);
            t.gp = gp;
            0
        }
        ("timrman", 22) => {
            let Some(t) = x.timers.iter_mut().find(|t| t.id == a(0)) else { return Some(KE_ERROR) };
            t.source = a(1);
            t.prescale = a(3).max(1);
            0
        }
        ("timrman", 23) => {
            let n = now(m);
            let x = &mut m.iop.exec;
            let Some(t) = x.timers.iter_mut().find(|t| t.id == a(0)) else { return Some(KE_ERROR) };
            t.running = true;
            t.start = n;
            t.next = n + timer_cycles(t, t.compare.max(1));
            iop_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
            0
        }
        ("timrman", 24) => {
            if let Some(t) = x.timers.iter_mut().find(|t| t.id == a(0)) {
                t.running = false;
            }
            0
        }

        // ---- SIF ----
        ("sifman", 5) => 0,
        ("sifman", 29) => 1,
        ("sifman", 7) | ("sifman", 32) => {
            // sceSifSetDma(transfers, count) to EE memory; SetDmaIntr calls its function after.
            let (list, n) = (a(0), a(1));
            for i in 0..n.min(32) {
                let t = list + 16 * i;
                let (src, dst, size) = (r32(m, t), r32(m, t + 4), r32(m, t + 8));
                let data = m.iop.read(src, size as usize);
                for (j, b) in data.iter().enumerate() {
                    crate::bus::ram_write(m, dst + j as u32, 1, *b as u64);
                }
            }
            if idx == 32 && a(2) != 0 {
                let (f, farg) = (a(2), a(3));
                c.call_guest(f, &[farg]);
            }
            1
        }
        ("sifman", 8) => -1,
        ("sifcmd", 14) => 0,
        ("sifcmd", 17) => {
            // sceSifRegisterRpc(sd, sid, func, buff, cfunc, cbuff, qd)
            let (sd, sid, func, buff, cfunc, cbuff, qd) = (a(0), a(1), a(2), a(3), a(4), a(5), a(6));
            for (o, v) in [(0, sid), (4, func), (8, buff), (16, cfunc), (20, cbuff), (64, qd)] {
                w32(m, sd + o, v);
            }
            let gp = c.gpr(GPR_GP);
            diag::log(format!("IOP: register RPC server {sid:#010x} (sd {sd:#x}, func {func:#x}, queue {qd:#x})"));
            m.iop.exec.servers.insert(sid, RpcServer { sd, sid, func, buff, cfunc, cbuff, qd, gp });
            0
        }
        ("sifcmd", 19) => {
            // sceSifSetRpcQueue(qd, thread)
            w32(m, a(0), a(1));
            m.iop.exec.queues.insert(a(0), (a(1), Default::default()));
            0
        }
        ("sifcmd", 22) => {
            rpc_loop(m, c, a(0));
            return None;
        }
        ("sifcmd", 20) => {
            let qd = a(0);
            match x.queues.get_mut(&qd).and_then(|q| q.1.pop_front()) {
                Some(r) => {
                    let sd = r.sd;
                    x.current_req.insert(sd, r);
                    sd as i32
                }
                None => 0,
            }
        }
        ("sifcmd", 21) => {
            exec_request(m, c, a(0));
            0
        }
        ("sifcmd", 23) => {
            // sceSifGetOtherData(rd, src (EE), dest (IOP), size, mode)
            let (src, dst, size) = (a(1), a(2), a(3));
            let mut buf = vec![0u8; size as usize];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = crate::bus::ram_read(m, src + i as u32, 1) as u8;
            }
            m.iop.write(dst, &buf);
            0
        }
        ("sifcmd", 24) => {
            x.servers.retain(|_, s| s.sd != a(0));
            0
        }
        ("sifcmd", 25) => {
            x.queues.remove(&a(0));
            0
        }
        ("sifcmd", 18) => m.iop.exec.rpc_busy.contains(&a(0)) as i32,
        ("sifcmd", 15) => {
            // sceSifBindRpc(client, sid, mode): an RPC server of the EE program.
            let (client, sid, mode) = (a(0), a(1), a(2));
            w32(m, client + 36, 0);
            w32(m, client + 16, 0x8000_0009);
            let p = super::packet(0x8000_0009, 0x40, &[(0x1C, client), (0x20, sid)]);
            let now = m.now();
            sif::send(m, now, vec![], p);
            if mode & 1 == 0 {
                block(m, Wait::Rpc(client));
            }
            0
        }
        ("sifcmd", 16) => {
            // sceSifCallRpc(client, fno, mode, send, ssize, recv, rsize, end_func, end_param)
            let (client, fno, mode, send, ssize, recv, rsize, ef, ep) = (a(0), a(1), a(2), a(3), a(4), a(5), a(6), a(7), a(8));
            let (server, buff) = (r32(m, client + 36), r32(m, client + 20));
            w32(m, client + 16, 0x8000_000A);
            w32(m, client + 28, ef);
            w32(m, client + 32, ep);
            let data = m.iop.read(send, ssize as usize);
            let p = super::packet(0x8000_000A, 0x40, &[(0x1C, client), (0x20, fno), (0x24, ssize), (0x28, recv), (0x2C, rsize), (0x30, 1), (0x34, server)]);
            let now = m.now();
            let writes = if ssize > 0 && buff != 0 { vec![(buff, data)] } else { vec![] };
            sif::send(m, now, writes, p);
            m.iop.exec.rpc_busy.insert(client);
            if mode & 1 == 0 {
                block(m, Wait::Rpc(client));
                // The end function runs in the caller's thread here (it is not interrupt-safe code
                // anyway in the modules that use it).
                if ef != 0 {
                    c.call_guest(ef, &[ep]);
                }
            }
            0
        }

        // ---- C library ----
        ("stdio", 4) => {
            let s = format_c(m, c, 0);
            diag::log(format!("IOP: {}", s.trim_end()));
            s.len() as i32
        }
        ("sysclib", 12) | ("sysclib", 13) => {
            let d = m.iop.read(a(1), a(2) as usize);
            m.iop.write(a(0), &d);
            a(0) as i32
        }
        ("sysclib", 14) => {
            m.iop.write(a(0), &vec![a(1) as u8; a(2) as usize]);
            a(0) as i32
        }
        ("sysclib", 17) => {
            m.iop.write(a(0), &vec![0; a(1) as usize]);
            0
        }
        ("sysclib", 27) => m.iop.cstr(a(0), 0x10000).len() as i32,
        ("sysclib", 22) | ("sysclib", 29) => {
            let n = if idx == 29 { a(2) as usize } else { usize::MAX };
            let (s1, s2) = (cbytes(m, a(0)), cbytes(m, a(1)));
            let mut r = 0i32;
            for i in 0..n {
                let (x1, x2) = (*s1.get(i).unwrap_or(&0), *s2.get(i).unwrap_or(&0));
                if x1 != x2 || x1 == 0 {
                    r = x1 as i32 - x2 as i32;
                    break;
                }
            }
            r
        }
        ("sysclib", 23) | ("sysclib", 30) => {
            let mut s = cbytes(m, a(1));
            s.push(0);
            if idx == 30 {
                s.resize(a(2) as usize, 0);
            }
            m.iop.write(a(0), &s);
            a(0) as i32
        }
        ("sysclib", 20) => {
            let mut s = cbytes(m, a(1));
            s.push(0);
            let l = cbytes(m, a(0)).len() as u32;
            m.iop.write(a(0) + l, &s);
            a(0) as i32
        }
        ("sysclib", 21) => {
            let s = cbytes(m, a(0));
            let ch = a(1) as u8;
            match s.iter().position(|&b| b == ch) {
                Some(p) => (a(0) + p as u32) as i32,
                None => if ch == 0 { (a(0) + s.len() as u32) as i32 } else { 0 },
            }
        }
        ("sysclib", 6) => (a(0) as u8).to_ascii_uppercase() as i32,
        ("sysclib", 7) => (a(0) as u8).to_ascii_lowercase() as i32,
        ("sysclib", 8) => {
            let ch = a(0) as u8;
            let mut f = 0;
            if ch.is_ascii_uppercase() { f |= 1; }
            if ch.is_ascii_lowercase() { f |= 2; }
            if ch.is_ascii_digit() { f |= 4; }
            if ch == b' ' || (9..=13).contains(&ch) { f |= 8; }
            if ch.is_ascii_punctuation() { f |= 0x10; }
            if ch.is_ascii_control() { f |= 0x20; }
            if ch.is_ascii_hexdigit() { f |= 0x40; }
            f
        }
        ("sysclib", 36) => {
            let s = String::from_utf8_lossy(&cbytes(m, a(0))).to_string();
            let t = s.trim_start();
            let base = if a(2) == 0 { if t.starts_with("0x") || t.starts_with("0X") { 16 } else { 10 } } else { a(2) };
            let digits: String = t.trim_start_matches("0x").trim_start_matches("0X").chars().take_while(|ch| ch.is_digit(base) || *ch == '-').collect();
            if a(1) != 0 {
                let used = s.len() - t.len() + digits.len();
                w32(m, a(1), a(0) + used as u32);
            }
            i64::from_str_radix(&digits, base).unwrap_or(0) as i32
        }
        ("sysclib", 19) => {
            let s = format_c(m, c, 1);
            let mut b = s.into_bytes();
            let n = b.len();
            b.push(0);
            m.iop.write(a(0), &b);
            n as i32
        }

        // ---- files (cdvdman, ioman) ----
        ("cdvdman", _) => return Some(super::iopfile::cdvd(m, c, idx)),
        ("ioman", _) => return Some(super::iopfile::ioman(m, c, idx)),

        _ => {
            diag::once(format!("IOP: unimplemented import {lib} #{idx} (a0 {:#x} a1 {:#x})", a(0), a(1)));
            0
        }
    })
}

fn cbytes(m: &Machine, a: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..0x10000u32 {
        let b = m.iop.ram[((a + i) & 0x1F_FFFF) as usize];
        if b == 0 {
            break;
        }
        v.push(b);
    }
    v
}

/// A printf-style format: the format string is argument `first`, values follow.
fn format_c(m: &Machine, c: &Ctx, first: usize) -> String {
    let fmt = cbytes(m, arg(m, c, first));
    let mut out = String::new();
    let mut n = first + 1;
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        i += 1;
        if ch != b'%' {
            out.push(ch as char);
            continue;
        }
        let mut spec = String::from("%");
        while i < fmt.len() && b"-+ #0123456789.lhz".contains(&fmt[i]) {
            spec.push(fmt[i] as char);
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        let width: usize = spec.trim_start_matches('%').trim_start_matches(['-', '0']).split('.').next().unwrap_or("").trim_end_matches(['l', 'h', 'z']).parse().unwrap_or(0);
        let zero = spec.starts_with("%0");
        let v = arg(m, c, n);
        let s = match conv {
            b'%' => "%".to_string(),
            b'd' | b'i' => { n += 1; (v as i32).to_string() }
            b'u' => { n += 1; v.to_string() }
            b'x' => { n += 1; format!("{v:x}") }
            b'X' => { n += 1; format!("{v:X}") }
            b'p' => { n += 1; format!("{v:08x}") }
            b'c' => { n += 1; ((v as u8) as char).to_string() }
            b's' => { n += 1; String::from_utf8_lossy(&cbytes(m, v)).to_string() }
            _ => { n += 1; String::new() }
        };
        let pad = width.saturating_sub(s.len());
        if spec.contains('-') {
            out += &s;
            out += &" ".repeat(pad);
        } else {
            out += &(if zero { "0" } else { " " }).repeat(pad);
            out += &s;
        }
    }
    out
}

fn event_met(cur: u32, bits: u32, mode: u32) -> bool {
    if mode & 1 != 0 { cur & bits != 0 } else { cur & bits == bits }
}

fn wake_events(m: &mut Machine, id: u32) {
    let x = &mut m.iop.exec;
    let waiting: Vec<(u32, u32, u32, u32)> = x
        .threads
        .iter()
        .flatten()
        .filter_map(|t| match t.wait {
            Wait::Event { id: e, bits, mode, res } if e == id => Some((t.id, bits, mode, res)),
            _ => None,
        })
        .collect();
    for (tid, bits, mode, res) in waiting {
        let x = &mut m.iop.exec;
        let Some(e) = x.events.get_mut(&id) else { return };
        if !event_met(e.bits, bits, mode) {
            continue;
        }
        let cur = e.bits;
        if mode & 0x10 != 0 {
            e.bits = 0;
        }
        if let Some(t) = x.threads.get_mut(tid as usize).and_then(|t| t.as_mut()) {
            t.wake_result = 0;
        }
        x.make_ready(tid);
        if res != 0 {
            w32(m, res, cur);
        }
    }
}

/// Set an event flag from the runtime (interrupt handlers of emulated hardware).
pub fn set_event(m: &mut Machine, id: u32, bits: u32) {
    if let Some(e) = m.iop.exec.events.get_mut(&id) {
        e.bits |= bits;
        wake_events(m, id);
    }
}

// ---- RPC server side ----

/// A call from the EE to a server a module registered: queue it and wake the queue's thread.
pub fn rpc_request(m: &mut Machine, sid: u32, fno: u32, size: u32, recv: u32, recv_size: u32, rmode: u32, pkt: &[u8]) -> bool {
    let x = &mut m.iop.exec;
    let Some(s) = x.servers.get(&sid) else { return false };
    let (sd, qd) = (s.sd, s.qd);
    let req = Req { sd, fno, size, recv, recv_size, rmode, pkt: pkt.to_vec() };
    let Some(q) = x.queues.get_mut(&qd) else {
        diag::once(format!("IOP: request for {sid:#x}: queue {qd:#x} not set up"));
        return false;
    };
    q.1.push_back(req);
    let tid = q.0;
    // sifcmd wakes the queue's thread (iWakeupThread): a thread that is not sleeping keeps the
    // wakeup for its next SleepThread.
    if let Some(t) = x.threads.get_mut(tid as usize).and_then(|t| t.as_mut()) {
        if matches!(t.wait, Wait::Rpc(_) | Wait::Sleep) {
            t.wake_result = 0;
            x.make_ready(tid);
        } else {
            t.wakeups += 1;
        }
    }
    iop_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
    ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

fn rpc_loop(_m: &mut Machine, c: &mut Ctx, qd: u32) {
    loop {
        let m = crate::machine();
        let req = m.iop.exec.queues.get_mut(&qd).and_then(|q| q.1.pop_front());
        match req {
            Some(r) => {
                let sd = r.sd;
                m.iop.exec.current_req.insert(sd, r);
                exec_request(m, c, sd);
            }
            None => {
                block(m, Wait::Rpc(qd));
            }
        }
        if unsafe { iop_core::ABORT } {
            return;
        }
    }
}

fn exec_request(m: &mut Machine, c: &mut Ctx, sd: u32) {
    let Some(r) = m.iop.exec.current_req.remove(&sd) else { return };
    let Some(s) = m.iop.exec.servers.values().find(|s| s.sd == sd) else { return };
    let (func, buff, gp) = (s.func, s.buff, s.gp);
    w32(m, sd + 36, r.fno);
    let old_gp = c.gpr(GPR_GP);
    c.set_gpr(GPR_GP, gp);
    let res = c.call_guest(func, &[r.fno, buff, r.size]);
    c.set_gpr(GPR_GP, old_gp);
    let m = crate::machine();
    let mut writes = Vec::new();
    if r.recv != 0 && r.recv_size > 0 && res != 0 {
        writes.push((r.recv, m.iop.read(res, r.recv_size as usize)));
    }
    let now = m.now();
    diag::count("module_end_sent");
    if r.rmode != 0 {
        let p = super::end_reply(&r.pkt, sd, buff);
        sif::send(m, now, writes, p);
    } else {
        for (a, d) in writes {
            for (i, b) in d.iter().enumerate() {
                crate::bus::ram_write(m, a + i as u32, 1, *b as u64);
            }
        }
        super::free_ee_packet(m, u32::from_le_bytes(r.pkt[0x14..0x18].try_into().unwrap()));
    }
}

/// An IOP alarm (unused by the game's modules, kept for completeness).
pub fn _alarm(m: &mut Machine, due: u64, handler: u32, arg: u32, gp: u32) {
    let x = &mut m.iop.exec;
    let id = x.next_obj;
    x.next_obj += 1;
    x.alarms.push(Alarm { id, due, handler, arg, gp });
}
