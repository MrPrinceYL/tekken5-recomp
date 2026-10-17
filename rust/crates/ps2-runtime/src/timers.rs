//! The four EE timers (T0..T3 at 0x10000000 + 0x800 * n): COUNT, MODE, COMP, HOLD.
//!
//! Counters are evaluated lazily from device time. MODE: CLKS (bus clock, /16, /256, HBLANK),
//! CUE (count enable), ZRET (clear on compare), CMPE / OVFE (interrupt on compare / overflow),
//! EQUF / OVFF (flags, write 1 to clear). Gating is not used by the game and is ignored.

use crate::{Machine, BUS_HZ, CYCLES_PER_LINE, EE_HZ, INTC_TIM0};

#[derive(Default, Clone, Copy)]
pub struct Timer {
    pub mode: u32,
    pub comp: u32,
    pub hold: u32,
    /// Counter value at `base_time`.
    pub base: u32,
    pub base_time: u64,
    /// Next compare / overflow event (EE cycles), u64::MAX if none, and which it is.
    pub next: u64,
    pub next_is_comp: bool,
}

#[derive(Default)]
pub struct Timers {
    pub t: [Timer; 4],
}

impl Timer {
    /// EE cycles per count.
    fn period(&self) -> u64 {
        match self.mode & 3 {
            0 => EE_HZ / BUS_HZ,
            1 => 16 * EE_HZ / BUS_HZ,
            2 => 256 * EE_HZ / BUS_HZ,
            _ => CYCLES_PER_LINE,
        }
    }

    fn count(&self, now: u64) -> u32 {
        if self.mode & 0x80 == 0 {
            return self.base;
        }
        let ticks = now.saturating_sub(self.base_time) / self.period();
        let v = self.base as u64 + ticks;
        if self.mode & 0x40 != 0 && self.comp != 0 {
            (v % (self.comp as u64 + 1)) as u32
        } else {
            (v & 0xFFFF) as u32
        }
    }

    fn schedule(&mut self, now: u64) {
        self.next = u64::MAX;
        if self.mode & 0x80 == 0 || self.mode & 0x300 == 0 {
            return;
        }
        let c = self.count(now) as u64;
        let comp = self.comp as u64;
        // Ticks until the count equals COMP, and until it wraps past 0xFFFF.
        let to_comp = if comp > c { comp - c } else { comp + 0x10000 - c };
        let to_ovf = 0x10000 - c;
        let zret = self.mode & 0x40 != 0;
        let comp_on = self.mode & 0x100 != 0 || zret;
        let ovf_on = self.mode & 0x200 != 0;
        let (target, is_comp) = match (comp_on, ovf_on) {
            (true, true) => if to_comp <= to_ovf || zret { (to_comp, true) } else { (to_ovf, false) },
            (true, false) => (to_comp, true),
            (false, true) => (to_ovf, false),
            _ => return,
        };
        self.next = now + target.max(1) * self.period();
        self.next_is_comp = is_comp;
    }
}

impl Timers {
    pub fn read(&mut self, a: u32, now: u64) -> u32 {
        let n = ((a >> 11) & 3) as usize;
        let t = &self.t[n];
        match a & 0x7F0 {
            0x00 => t.count(now),
            0x10 => t.mode,
            0x20 => t.comp,
            0x30 => t.hold,
            _ => 0,
        }
    }

    pub fn write(&mut self, a: u32, v: u32, now: u64) {
        let n = ((a >> 11) & 3) as usize;
        let t = &mut self.t[n];
        match a & 0x7F0 {
            0x00 => {
                t.base = v & 0xFFFF;
                t.base_time = now;
            }
            0x10 => {
                let c = t.count(now);
                t.base = c;
                t.base_time = now;
                let flags = t.mode & 0xC00 & !(v & 0xC00);
                t.mode = (v & 0x3FF) | flags;
            }
            0x20 => t.comp = v & 0xFFFF,
            0x30 => t.hold = v & 0xFFFF,
            _ => {}
        }
        t.schedule(now);
        ee_core::ATTENTION.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn vblank(&mut self, _now: u64) {}
}

pub fn update(m: &mut Machine, now: u64) {
    // Every event that is due, in order: one handled late must not hide the next (Timer 1 has
    // compare and overflow on; a wrap after a late compare was lost and the game's clock, which
    // counts the wraps, fell back 2^16 counts - bugs.md B14).
    for n in 0..4 {
        while m.timers.t[n].next <= now {
            let t = &mut m.timers.t[n];
            // The event happened at t.next: the count was COMP (compare) or wrapped to 0.
            let at = t.next;
            let period = t.period();
            let hit_comp = t.next_is_comp;
            // COMP = 0 (without ZRET) is reached by the wrap: that is the overflow as well, and
            // `schedule` picks only one of the two (Timer 1 of the game: both interrupts on, COMP 0;
            // without OVFF its handler never counted a wrap - bugs.md B14).
            let wrap_too = hit_comp && t.comp == 0 && t.mode & 0x40 == 0;
            let raise = if hit_comp { t.mode & 0x100 != 0 } else { t.mode & 0x200 != 0 } || (wrap_too && t.mode & 0x200 != 0);
            if hit_comp {
                if t.mode & 0x100 != 0 {
                    t.mode |= 0x400;
                }
                if wrap_too && t.mode & 0x200 != 0 {
                    t.mode |= 0x800;
                }
                // ZRET: back to 0 on the tick after COMP.
                t.base = if t.mode & 0x40 != 0 { 0 } else { t.comp };
                t.base_time = if t.mode & 0x40 != 0 { at + period } else { at };
            } else {
                t.mode |= 0x800;
                t.base = 0;
                t.base_time = at;
            }
            if t.base_time > now {
                t.base_time = now;
            }
            // From the event, not from now (see above): base_time is when the count was base.
            t.schedule(t.base_time);
            if raise {
                m.raise(INTC_TIM0 + n as u32);
            }
        }
    }
}

pub fn next_event(m: &Machine) -> u64 {
    m.timers.t.iter().map(|t| t.next).min().unwrap_or(u64::MAX)
}
