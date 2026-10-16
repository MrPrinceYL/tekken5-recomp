//! EE FPU (COP1) arithmetic: single precision, but not IEEE 754.
//!
//! - There are no denormals, infinities or NaNs: an operand with exponent 0 is (signed) zero, an
//!   operand with exponent 255 is an ordinary number ((1.m) * 2^128).
//! - Results round toward zero; an overflowing result becomes +/- 0x7FFFFFFF (the largest
//!   value), an underflowing one +/- 0.
//!
//! The p-code float operators (`float_*`, size 4) follow these rules: operands are widened to
//! f64 (exact), the operation is done in f64 and the result is truncated back. Size-8 values
//! (none in EE code) are IEEE doubles.

/// An EE single as f64 (exact).
#[inline(always)]
pub fn ps2_to_f64(bits: u32) -> f64 {
    let exp = (bits >> 23) & 0xFF;
    let neg = bits & 0x8000_0000 != 0;
    if exp == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    if exp != 0xFF {
        return f32::from_bits(bits) as f64;
    }
    // Exponent 255: an ordinary number on the EE. Build the f64 directly.
    let mant = (bits & 0x7F_FFFF) as u64;
    let e64 = (255u64 - 127 + 1023) << 52;
    let v = f64::from_bits(e64 | mant << 29);
    if neg { -v } else { v }
}

/// An f64 result as an EE single: round toward zero, clamp overflow to the largest value,
/// flush underflow to zero.
#[inline(always)]
pub fn f64_to_ps2(v: f64) -> u32 {
    let bits = v.to_bits();
    let sign = ((bits >> 32) as u32) & 0x8000_0000;
    let e64 = ((bits >> 52) & 0x7FF) as i32;
    if e64 == 0 {
        return sign;
    }
    if e64 == 0x7FF {
        // An infinity (or NaN) from f64 arithmetic: overflow.
        return sign | 0x7FFF_FFFF;
    }
    let e = e64 - 1023 + 127;
    if e <= 0 {
        return sign;
    }
    if e > 255 {
        return sign | 0x7FFF_FFFF;
    }
    let mant = ((bits >> 29) & 0x7F_FFFF) as u32;
    sign | (e as u32) << 23 | mant
}

#[inline(always)] fn b(x: bool) -> u64 { x as u64 }

#[inline(always)]
pub fn tof(a: u64, s: usize) -> f64 { if s == 4 { ps2_to_f64(a as u32) } else { f64::from_bits(a) } }
#[inline(always)]
pub fn fromf(v: f64, s: usize) -> u64 { if s == 4 { f64_to_ps2(v) as u64 } else { v.to_bits() } }

#[inline] pub fn float_add(a: u64, c: u64, s: usize) -> u64 { fromf(tof(a, s) + tof(c, s), s) }
#[inline] pub fn float_sub(a: u64, c: u64, s: usize) -> u64 { fromf(tof(a, s) - tof(c, s), s) }
#[inline] pub fn float_mult(a: u64, c: u64, s: usize) -> u64 { fromf(tof(a, s) * tof(c, s), s) }
/// Division by zero gives the largest value with the sign of the quotient (EE FPU).
#[inline]
pub fn float_div(a: u64, c: u64, s: usize) -> u64 {
    let (x, y) = (tof(a, s), tof(c, s));
    if y == 0.0 && s == 4 {
        let sign = ((a as u32) ^ (c as u32)) & 0x8000_0000;
        return (sign | 0x7FFF_FFFF) as u64;
    }
    fromf(x / y, s)
}
#[inline] pub fn float_neg(a: u64, s: usize) -> u64 { if s == 4 { a ^ 0x8000_0000 } else { a ^ (1 << 63) } }
#[inline] pub fn float_abs(a: u64, s: usize) -> u64 { if s == 4 { a & 0x7FFF_FFFF } else { a & !(1 << 63) } }
/// sqrt.s uses the magnitude of a negative operand.
#[inline] pub fn float_sqrt(a: u64, s: usize) -> u64 { fromf(tof(a, s).abs().sqrt(), s) }
#[inline] pub fn float_ceil(a: u64, s: usize) -> u64 { fromf(tof(a, s).ceil(), s) }
#[inline] pub fn float_floor(a: u64, s: usize) -> u64 { fromf(tof(a, s).floor(), s) }
#[inline] pub fn float_round(a: u64, s: usize) -> u64 { fromf(tof(a, s).round(), s) }
#[inline] pub fn float_nan(a: u64, s: usize) -> u64 { if s == 4 { 0 } else { b(f64::from_bits(a).is_nan()) } }
#[inline] pub fn float_equal(a: u64, c: u64, s: usize) -> u64 { b(tof(a, s) == tof(c, s)) }
#[inline] pub fn float_notequal(a: u64, c: u64, s: usize) -> u64 { b(tof(a, s) != tof(c, s)) }
#[inline] pub fn float_less(a: u64, c: u64, s: usize) -> u64 { b(tof(a, s) < tof(c, s)) }
#[inline] pub fn float_lessequal(a: u64, c: u64, s: usize) -> u64 { b(tof(a, s) <= tof(c, s)) }
/// FLOAT2FLOAT: input size `s` -> output size `out`.
#[inline] pub fn float2float(a: u64, s: usize, out: usize) -> u64 { fromf(tof(a, s), out) }
#[inline]
pub fn int2float(a: u64, s: usize, out: usize) -> u64 {
    let v = crate::ops::sext(a, s) as f64;
    fromf(v, out)
}
/// cvt.w.s: truncate toward zero, saturating (0x7FFFFFFF / 0x80000000).
#[inline]
pub fn trunc(a: u64, s: usize, out: usize) -> u64 {
    let v = tof(a, s);
    let i = if out <= 4 { v.clamp(i32::MIN as f64, i32::MAX as f64) as i32 as i64 } else { v as i64 };
    crate::ops::mask(i as u64, out)
}

/// The EE single helpers for the runtime (VU0, MMI) with f32 bit patterns.
#[inline] pub fn fadd(a: u32, c: u32) -> u32 { f64_to_ps2(ps2_to_f64(a) + ps2_to_f64(c)) }
#[inline] pub fn fsub(a: u32, c: u32) -> u32 { f64_to_ps2(ps2_to_f64(a) - ps2_to_f64(c)) }
#[inline] pub fn fmul(a: u32, c: u32) -> u32 { f64_to_ps2(ps2_to_f64(a) * ps2_to_f64(c)) }
