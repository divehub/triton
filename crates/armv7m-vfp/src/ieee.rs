//! FPv4-SP data operations as used by `execute`.
//!
//! Every function here is bit-for-bit and flag-for-flag identical to its
//! counterpart in [`crate::soft`] (the exact reference implementation). Where
//! the operands and the FPSCR mode make it provable, a native `f32`/`f64`
//! fast path is taken instead:
//!
//! * the FPSCR selects round-to-nearest-even with flush-to-zero off, and
//! * every operand is a finite number (so no NaN payload, signaling NaN or
//!   invalid-operation behavior is involved).
//!
//! WebAssembly's `f32.add/sub/mul/div/sqrt`, `f64.mul/add`, `f64.trunc` and
//! conversions are correctly rounded round-to-nearest-even operations, so the
//! results are deterministic on every target. Exception flags are recovered
//! exactly:
//!
//! * add/sub: the Knuth TwoSum error term is exactly representable, so the
//!   result is inexact iff it is non-zero (no flag can be lost to rounding);
//! * mul: the exact product of two binary32 values fits in a binary64, so
//!   inexactness and tininess (checked before rounding, as the Arm
//!   architecture requires) are exact comparisons in binary64;
//! * div/sqrt: inexactness is `quotient * divisor != dividend` (resp.
//!   `root * root != operand`), evaluated exactly in binary64;
//! * fma: the product is exact in binary64; the binary64 sum is used only
//!   when TwoSum shows it is exact too, otherwise the exact software path
//!   is taken (WebAssembly has no scalar fused multiply-add).
//!
//! The fast paths are cross-checked against [`crate::soft`] and against the
//! host's hardware floating-point unit by the integration tests.

use crate::fpscr::*;
use crate::soft;
use crate::soft::SIGN;

/// Smallest positive normal binary32 value (2^-126), as binary64.
const MIN_NORMAL: f64 = f32::MIN_POSITIVE as f64;

#[inline(always)]
fn is_finite_bits(x: u32) -> bool {
    x & 0x7F80_0000 != 0x7F80_0000
}

/// True when the native paths are valid for the current mode.
#[inline(always)]
fn fast(fpscr: u32) -> bool {
    fpscr & FAST_MODE_MASK == 0
}

/// Sum of two finite binary32 values in RNE with exact flags.
#[inline(always)]
fn fast_add(x: f32, y: f32, fpscr: &mut u32) -> u32 {
    let s = x + y;
    if s.is_finite() {
        let bb = s - x;
        let err = (x - (s - bb)) + (y - bb);
        if err != 0.0 {
            *fpscr |= IXC;
        }
    } else {
        *fpscr |= OFC | IXC;
    }
    s.to_bits()
}

/// `FPAdd`.
#[inline]
pub fn add(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    if fast(*fpscr) && is_finite_bits(a) && is_finite_bits(b) {
        return fast_add(f32::from_bits(a), f32::from_bits(b), fpscr);
    }
    soft::add(a, b, fpscr)
}

/// `FPSub`.
#[inline]
pub fn sub(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    if fast(*fpscr) && is_finite_bits(a) && is_finite_bits(b) {
        return fast_add(f32::from_bits(a), -f32::from_bits(b), fpscr);
    }
    soft::sub(a, b, fpscr)
}

/// `FPMul`.
#[inline]
pub fn mul(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    if fast(*fpscr) && is_finite_bits(a) && is_finite_bits(b) {
        let p = f32::from_bits(a) as f64 * f32::from_bits(b) as f64; // exact
        let r = p as f32;
        if r.is_infinite() {
            *fpscr |= OFC | IXC;
        } else if r as f64 != p {
            *fpscr |= if p.abs() < MIN_NORMAL { IXC | UFC } else { IXC };
        }
        return r.to_bits();
    }
    soft::mul(a, b, fpscr)
}

/// `FPDiv`.
#[inline]
pub fn div(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    if fast(*fpscr) && is_finite_bits(a) && is_finite_bits(b) && b & 0x7FFF_FFFF != 0 {
        let x = f32::from_bits(a);
        let y = f32::from_bits(b);
        let r = x / y;
        if r.is_infinite() {
            *fpscr |= OFC | IXC;
        } else {
            let (xd, yd) = (x as f64, y as f64);
            if r as f64 * yd != xd {
                // Tiny before rounding iff |x / y| < 2^-126.
                *fpscr |= if xd.abs() < yd.abs() * MIN_NORMAL { IXC | UFC } else { IXC };
            }
        }
        return r.to_bits();
    }
    soft::div(a, b, fpscr)
}

/// `FPSqrt`.
#[inline]
pub fn sqrt(a: u32, fpscr: &mut u32) -> u32 {
    // Finite and either positive or a zero (so the operation cannot be invalid).
    if fast(*fpscr) && is_finite_bits(a) && (a & SIGN == 0 || a & 0x7FFF_FFFF == 0) {
        let x = f32::from_bits(a);
        let r = x.sqrt();
        let rd = r as f64;
        if rd * rd != x as f64 {
            *fpscr |= IXC;
        }
        return r.to_bits();
    }
    soft::sqrt(a, fpscr)
}

/// `FPMulAdd(addend, op1, op2)`: `addend + op1 * op2`, fused.
#[inline]
pub fn fma(addend: u32, op1: u32, op2: u32, fpscr: &mut u32) -> u32 {
    if fast(*fpscr) && is_finite_bits(addend) && is_finite_bits(op1) && is_finite_bits(op2) {
        let p = f32::from_bits(op1) as f64 * f32::from_bits(op2) as f64; // exact
        let z = f32::from_bits(addend) as f64;
        let s = p + z;
        let bb = s - p;
        let err = (p - (s - bb)) + (z - bb);
        if err == 0.0 {
            // The binary64 sum is exact, so one rounding to binary32 is correct.
            let r = s as f32;
            if r.is_infinite() {
                *fpscr |= OFC | IXC;
            } else if r as f64 != s {
                *fpscr |= if s.abs() < MIN_NORMAL { IXC | UFC } else { IXC };
            }
            return r.to_bits();
        }
    }
    soft::fma(addend, op1, op2, fpscr)
}

/// `FPCompare`: NZCV in bits 31:28.
#[inline]
pub fn compare(a: u32, b: u32, quiet_nan_exc: bool, fpscr: &mut u32) -> u32 {
    #[inline(always)]
    fn key(x: u32) -> i32 {
        let m = (x & 0x7FFF_FFFF) as i32;
        if x & SIGN != 0 {
            -m
        } else {
            m
        }
    }
    if *fpscr & FZ == 0 && a & 0x7FFF_FFFF <= 0x7F80_0000 && b & 0x7FFF_FFFF <= 0x7F80_0000 {
        let (ka, kb) = (key(a), key(b));
        return if ka == kb {
            0x6000_0000
        } else if ka < kb {
            0x8000_0000
        } else {
            0x2000_0000
        };
    }
    soft::compare(a, b, quiet_nan_exc, fpscr)
}

/// VCVT.F32.S32 / VCVT.F32.U32 (FPSCR rounding mode).
#[inline]
pub fn from_int(x: u32, unsigned: bool, fpscr: &mut u32) -> u32 {
    if *fpscr & RMODE_MASK == 0 {
        let (r, v) = if unsigned {
            (x as f32, x as f64)
        } else {
            (x as i32 as f32, x as i32 as f64)
        };
        if r as f64 != v {
            *fpscr |= IXC;
        }
        return r.to_bits();
    }
    soft::from_fixed(x, 32, 0, unsigned, fpscr)
}

/// Fixed-point to binary32 (`VCVT.F32.{S,U}{16,32}` with `fbits` fraction bits).
#[inline]
pub fn from_fixed(x: u32, size: u32, fbits: u32, unsigned: bool, fpscr: &mut u32) -> u32 {
    if *fpscr & RMODE_MASK == 0 {
        let v = if unsigned {
            if size == 16 {
                (x & 0xFFFF) as f64
            } else {
                x as f64
            }
        } else if size == 16 {
            x as u16 as i16 as f64
        } else {
            x as i32 as f64
        };
        // Scaling by a power of two is exact; |v| / 2^fbits >= 2^-32 is normal.
        let v = v * (1.0 / (1u64 << fbits) as f64);
        let r = v as f32;
        if r as f64 != v {
            *fpscr |= IXC;
        }
        return r.to_bits();
    }
    soft::from_fixed(x, size, fbits, unsigned, fpscr)
}

/// VCVT.{S32,U32}.F32 (round toward zero) and, with `round`, VCVTR.
#[inline]
pub fn to_int(a: u32, unsigned: bool, round_zero: bool, fpscr: &mut u32) -> u32 {
    let rm = if round_zero { RMODE_RZ } else { rmode(*fpscr) };
    if rm == RMODE_RZ && *fpscr & FZ == 0 {
        let x = f32::from_bits(a);
        if x.is_nan() {
            *fpscr |= IOC;
            return 0;
        }
        if unsigned {
            if x >= 4_294_967_296.0 {
                *fpscr |= IOC;
                return u32::MAX;
            }
            if x <= -1.0 {
                *fpscr |= IOC;
                return 0;
            }
            let r = x as u32;
            if r as f32 != x {
                *fpscr |= IXC;
            }
            return r;
        }
        if x >= 2_147_483_648.0 {
            *fpscr |= IOC;
            return 0x7FFF_FFFF;
        }
        if x < -2_147_483_648.0 {
            *fpscr |= IOC;
            return 0x8000_0000;
        }
        let r = x as i32;
        if r as f32 != x {
            *fpscr |= IXC;
        }
        return r as u32;
    }
    soft::to_fixed(a, 32, 0, unsigned, rm, fpscr)
}

/// Binary32 to fixed point (`VCVT.{S,U}{16,32}.F32`, always round toward zero).
#[inline]
pub fn to_fixed(a: u32, size: u32, fbits: u32, unsigned: bool, fpscr: &mut u32) -> u32 {
    if *fpscr & FZ == 0 {
        // Scaling a binary32 by 2^fbits (<= 2^32) is exact in binary64, as is the truncation.
        let x = f32::from_bits(a) as f64;
        let v = x * f64::from_bits((1023 + fbits as u64) << 52);
        if v.is_nan() {
            *fpscr |= IOC;
            return 0;
        }
        let t = v.trunc();
        let (lo, hi) = if unsigned {
            (0.0, ((1u64 << size) - 1) as f64)
        } else {
            (-((1u64 << (size - 1)) as f64), ((1u64 << (size - 1)) - 1) as f64)
        };
        if t > hi {
            *fpscr |= IOC;
            return hi as u64 as u32;
        }
        if t < lo {
            *fpscr |= IOC;
            return lo as i64 as u32;
        }
        if t != v {
            *fpscr |= IXC;
        }
        return t as i64 as u32;
    }
    soft::to_fixed(a, size, fbits, unsigned, RMODE_RZ, fpscr)
}

/// `FPHalfToSingle`.
#[inline]
pub fn f16_to_f32(h: u16, fpscr: &mut u32) -> u32 {
    soft::f16_to_f32(h, fpscr)
}

/// `FPSingleToHalf`.
#[inline]
pub fn f32_to_f16(a: u32, fpscr: &mut u32) -> u16 {
    soft::f32_to_f16(a, fpscr)
}
