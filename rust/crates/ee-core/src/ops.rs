//! P-code operator semantics. Values up to 8 bytes are carried in `u64`, 16-byte values (the
//! R5900's 128-bit GPRs, lq / sq) in `u128` (the `w_` functions); `s` is the operand size in
//! bytes.
//!
//! Division follows the R5900 (like the R3000A): division by zero does not trap; `div` gives
//! lo = -1 (dividend >= 0) or +1 (dividend < 0) and hi = dividend; `divu` gives lo = all ones
//! and hi = dividend. MIN / -1 gives lo = MIN, hi = 0.
//!
//! Floating point (4-byte values) follows the EE FPU, see [`crate::fpu`].

pub use crate::fpu::*;

#[inline(always)] pub fn mask(v: u64, s: usize) -> u64 { if s >= 8 { v } else { v & ((1u64 << (8 * s)) - 1) } }
#[inline(always)] pub fn sext(v: u64, s: usize) -> i64 { if s >= 8 { v as i64 } else { let sh = 64 - 8 * s; ((v << sh) as i64) >> sh } }
#[inline(always)] fn b(x: bool) -> u64 { x as u64 }

#[inline(always)] pub fn int_add(a: u64, c: u64, s: usize) -> u64 { mask(a.wrapping_add(c), s) }
#[inline(always)] pub fn int_sub(a: u64, c: u64, s: usize) -> u64 { mask(a.wrapping_sub(c), s) }
#[inline(always)] pub fn int_mult(a: u64, c: u64, s: usize) -> u64 { mask(a.wrapping_mul(c), s) }
#[inline] pub fn int_div(a: u64, c: u64, s: usize) -> u64 { if mask(c, s) == 0 { mask(u64::MAX, s) } else { mask(mask(a, s) / mask(c, s), s) } }
#[inline] pub fn int_rem(a: u64, c: u64, s: usize) -> u64 { if mask(c, s) == 0 { mask(a, s) } else { mask(mask(a, s) % mask(c, s), s) } }
#[inline] pub fn int_sdiv(a: u64, c: u64, s: usize) -> u64 { let d = sext(c, s); if d == 0 { mask(if sext(a, s) >= 0 { u64::MAX } else { 1 }, s) } else { mask(sext(a, s).wrapping_div(d) as u64, s) } }
#[inline] pub fn int_srem(a: u64, c: u64, s: usize) -> u64 { let d = sext(c, s); if d == 0 { mask(a, s) } else { mask(sext(a, s).wrapping_rem(d) as u64, s) } }
#[inline(always)] pub fn int_and(a: u64, c: u64, _: usize) -> u64 { a & c }
#[inline(always)] pub fn int_or(a: u64, c: u64, _: usize) -> u64 { a | c }
#[inline(always)] pub fn int_xor(a: u64, c: u64, _: usize) -> u64 { a ^ c }
#[inline(always)] pub fn int_negate(a: u64, s: usize) -> u64 { mask(!a, s) }
#[inline(always)] pub fn int_2comp(a: u64, s: usize) -> u64 { mask(a.wrapping_neg(), s) }
#[inline(always)] pub fn int_left(a: u64, c: u64, s: usize) -> u64 { if c >= 64 { 0 } else { mask(a << c, s) } }
#[inline(always)] pub fn int_right(a: u64, c: u64, s: usize) -> u64 { if c >= 64 { 0 } else { mask(a, s) >> c } }
#[inline(always)] pub fn int_sright(a: u64, c: u64, s: usize) -> u64 { mask((sext(a, s) >> c.min(63)) as u64, s) }

#[inline(always)] pub fn int_equal(a: u64, c: u64, _: usize) -> u64 { b(a == c) }
#[inline(always)] pub fn int_notequal(a: u64, c: u64, _: usize) -> u64 { b(a != c) }
#[inline(always)] pub fn int_less(a: u64, c: u64, s: usize) -> u64 { b(mask(a, s) < mask(c, s)) }
#[inline(always)] pub fn int_lessequal(a: u64, c: u64, s: usize) -> u64 { b(mask(a, s) <= mask(c, s)) }
#[inline(always)] pub fn int_sless(a: u64, c: u64, s: usize) -> u64 { b(sext(a, s) < sext(c, s)) }
#[inline(always)] pub fn int_slessequal(a: u64, c: u64, s: usize) -> u64 { b(sext(a, s) <= sext(c, s)) }
#[inline] pub fn int_carry(a: u64, c: u64, s: usize) -> u64 { b(mask(a, s) > mask(!c, s)) }
#[inline] pub fn int_scarry(a: u64, c: u64, s: usize) -> u64 { let r = int_add(a, c, s); let t = 8 * s - 1; b(((a ^ r) & (c ^ r)) >> t & 1 == 1) }
#[inline] pub fn int_sborrow(a: u64, c: u64, s: usize) -> u64 { let r = int_sub(a, c, s); let t = 8 * s - 1; b(((a ^ c) & (a ^ r)) >> t & 1 == 1) }
#[inline(always)] pub fn int_zext(a: u64, s: usize) -> u64 { mask(a, s) }
#[inline(always)] pub fn int_sext(a: u64, s: usize) -> u64 { sext(a, s) as u64 }

#[inline(always)] pub fn bool_negate(a: u64, _: usize) -> u64 { b(a & 1 == 0) }
#[inline(always)] pub fn bool_and(a: u64, c: u64, _: usize) -> u64 { a & c & 1 }
#[inline(always)] pub fn bool_or(a: u64, c: u64, _: usize) -> u64 { (a | c) & 1 }
#[inline(always)] pub fn bool_xor(a: u64, c: u64, _: usize) -> u64 { (a ^ c) & 1 }

#[inline] pub fn popcount(a: u64, s: usize) -> u64 { mask(a, s).count_ones() as u64 }
#[inline] pub fn lzcount(a: u64, s: usize) -> u64 { (mask(a, s).leading_zeros() as u64).saturating_sub(64 - 8 * s as u64) }
/// SUBPIECE: drop `c` low bytes, truncate to output size.
#[inline(always)] pub fn subpiece(a: u64, c: u64, out: usize) -> u64 { mask(if c >= 8 { 0 } else { a >> (8 * c) }, out) }
/// PIECE: `hi` concatenated above `lo` (lo has size `ls`).
#[inline(always)] pub fn piece(hi: u64, lo: u64, ls: usize) -> u64 { if ls >= 8 { lo } else { (hi << (8 * ls)) | mask(lo, ls) } }

// ---- 128-bit (any operand or the result wider than 8 bytes) ----

#[inline(always)] pub fn wmask(v: u128, s: usize) -> u128 { if s >= 16 { v } else { v & ((1u128 << (8 * s)) - 1) } }
#[inline(always)] pub fn wsext(v: u128, s: usize) -> i128 { if s >= 16 { v as i128 } else { let sh = 128 - 8 * s; ((v << sh) as i128) >> sh } }
#[inline(always)] fn wb(x: bool) -> u128 { x as u128 }

#[inline(always)] pub fn w_int_add(a: u128, c: u128, s: usize) -> u128 { wmask(a.wrapping_add(c), s) }
#[inline(always)] pub fn w_int_sub(a: u128, c: u128, s: usize) -> u128 { wmask(a.wrapping_sub(c), s) }
#[inline(always)] pub fn w_int_mult(a: u128, c: u128, s: usize) -> u128 { wmask(a.wrapping_mul(c), s) }
#[inline] pub fn w_int_div(a: u128, c: u128, s: usize) -> u128 { if wmask(c, s) == 0 { wmask(u128::MAX, s) } else { wmask(a, s) / wmask(c, s) } }
#[inline] pub fn w_int_rem(a: u128, c: u128, s: usize) -> u128 { if wmask(c, s) == 0 { wmask(a, s) } else { wmask(a, s) % wmask(c, s) } }
#[inline] pub fn w_int_sdiv(a: u128, c: u128, s: usize) -> u128 { let d = wsext(c, s); if d == 0 { wmask(if wsext(a, s) >= 0 { u128::MAX } else { 1 }, s) } else { wmask(wsext(a, s).wrapping_div(d) as u128, s) } }
#[inline] pub fn w_int_srem(a: u128, c: u128, s: usize) -> u128 { let d = wsext(c, s); if d == 0 { wmask(a, s) } else { wmask(wsext(a, s).wrapping_rem(d) as u128, s) } }
#[inline(always)] pub fn w_int_and(a: u128, c: u128, _: usize) -> u128 { a & c }
#[inline(always)] pub fn w_int_or(a: u128, c: u128, _: usize) -> u128 { a | c }
#[inline(always)] pub fn w_int_xor(a: u128, c: u128, _: usize) -> u128 { a ^ c }
#[inline(always)] pub fn w_int_negate(a: u128, s: usize) -> u128 { wmask(!a, s) }
#[inline(always)] pub fn w_int_2comp(a: u128, s: usize) -> u128 { wmask(a.wrapping_neg(), s) }
#[inline(always)] pub fn w_int_left(a: u128, c: u128, s: usize) -> u128 { if c >= 128 { 0 } else { wmask(a << c, s) } }
#[inline(always)] pub fn w_int_right(a: u128, c: u128, s: usize) -> u128 { if c >= 128 { 0 } else { wmask(a, s) >> c } }
#[inline(always)] pub fn w_int_sright(a: u128, c: u128, s: usize) -> u128 { wmask((wsext(a, s) >> c.min(127)) as u128, s) }
#[inline(always)] pub fn w_int_equal(a: u128, c: u128, _: usize) -> u128 { wb(a == c) }
#[inline(always)] pub fn w_int_notequal(a: u128, c: u128, _: usize) -> u128 { wb(a != c) }
#[inline(always)] pub fn w_int_less(a: u128, c: u128, s: usize) -> u128 { wb(wmask(a, s) < wmask(c, s)) }
#[inline(always)] pub fn w_int_lessequal(a: u128, c: u128, s: usize) -> u128 { wb(wmask(a, s) <= wmask(c, s)) }
#[inline(always)] pub fn w_int_sless(a: u128, c: u128, s: usize) -> u128 { wb(wsext(a, s) < wsext(c, s)) }
#[inline(always)] pub fn w_int_slessequal(a: u128, c: u128, s: usize) -> u128 { wb(wsext(a, s) <= wsext(c, s)) }
#[inline] pub fn w_int_carry(a: u128, c: u128, s: usize) -> u128 { wb(wmask(a, s) > wmask(!c, s)) }
#[inline] pub fn w_int_scarry(a: u128, c: u128, s: usize) -> u128 { let r = w_int_add(a, c, s); let t = 8 * s - 1; wb(((a ^ r) & (c ^ r)) >> t & 1 == 1) }
#[inline] pub fn w_int_sborrow(a: u128, c: u128, s: usize) -> u128 { let r = w_int_sub(a, c, s); let t = 8 * s - 1; wb(((a ^ c) & (a ^ r)) >> t & 1 == 1) }
#[inline(always)] pub fn w_int_zext(a: u128, s: usize) -> u128 { wmask(a, s) }
#[inline(always)] pub fn w_int_sext(a: u128, s: usize) -> u128 { wsext(a, s) as u128 }
#[inline(always)] pub fn w_bool_negate(a: u128, _: usize) -> u128 { wb(a & 1 == 0) }
#[inline(always)] pub fn w_bool_and(a: u128, c: u128, _: usize) -> u128 { a & c & 1 }
#[inline(always)] pub fn w_bool_or(a: u128, c: u128, _: usize) -> u128 { (a | c) & 1 }
#[inline(always)] pub fn w_bool_xor(a: u128, c: u128, _: usize) -> u128 { (a ^ c) & 1 }
#[inline] pub fn w_popcount(a: u128, s: usize) -> u128 { wmask(a, s).count_ones() as u128 }
#[inline] pub fn w_lzcount(a: u128, s: usize) -> u128 { (wmask(a, s).leading_zeros() as u128).saturating_sub(128 - 8 * s as u128) }
#[inline(always)] pub fn w_subpiece(a: u128, c: u128, out: usize) -> u128 { wmask(if c >= 16 { 0 } else { a >> (8 * c) }, out) }
#[inline(always)] pub fn w_piece(hi: u128, lo: u128, ls: usize) -> u128 { if ls >= 16 { lo } else { (hi << (8 * ls)) | wmask(lo, ls) } }
