//! Exact software IEEE-754 binary32 / binary16 arithmetic with Arm VFP semantics.
//!
//! This module is the reference implementation of every FPv4-SP data operation.
//! It follows the Arm architecture pseudocode (`FPUnpack`, `FPRound`,
//! `FPProcessNaNs`, `FPProcessNaNs3`, `FPAdd`, `FPSub`, `FPMul`, `FPDiv`,
//! `FPSqrt`, `FPMulAdd`, `FPCompare`, `FPToFixed`, `FixedToFP`,
//! `FPHalfToSingle`, `FPSingleToHalf`) using integer arithmetic only: the host
//! floating-point unit, the host rounding mode and the host NaN payload
//! behaviour are never consulted, so results are identical on every target
//! (including WebAssembly).
//!
//! All functions take and return raw bit patterns and update the cumulative
//! exception flags (and nothing else) inside the `fpscr` they are given. The
//! FPSCR fields that influence the result are `RMode` (23:22), `FZ` (24),
//! `DN` (25) and `AHP` (26).
//!
//! Notable architectural details reproduced here:
//! * underflow is detected before rounding (a tiny inexact result sets UFC);
//! * `FZ` flushes denormal inputs (setting IDC) and results whose unrounded
//!   exponent is below the minimum normal exponent (setting UFC, not IXC);
//! * overflow sets OFC and IXC;
//! * NaN propagation: first signalling NaN operand (quietened), else first
//!   quiet NaN, else the default NaN `0x7FC00000`; `DN` forces the default NaN.

use crate::fpscr::*;

/// Sign bit of a binary32 value.
pub const SIGN: u32 = 0x8000_0000;
/// The default NaN (positive, quiet, zero payload).
pub const DEFAULT_NAN: u32 = 0x7FC0_0000;
/// The quiet bit of a binary32 NaN.
pub const QUIET_BIT: u32 = 0x0040_0000;
/// +infinity.
pub const INFINITY: u32 = 0x7F80_0000;
/// Largest finite magnitude.
pub const MAX_NORMAL: u32 = 0x7F7F_FFFF;

/// Classification of an unpacked operand (`FPType`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Zero,
    Num,
    Inf,
    QNan,
    SNan,
}

/// An unpacked binary32 operand. For `Num`, the value is
/// `sig * 2^(exp - 23)` with `sig` in `[2^23, 2^24)` (subnormals are
/// normalised), i.e. `exp` is the unbiased exponent of the leading bit.
#[derive(Clone, Copy, Debug)]
struct Unp {
    kind: Kind,
    /// 0 or `SIGN`.
    sign: u32,
    exp: i32,
    sig: u32,
    /// The original encoding (used for NaN propagation).
    bits: u32,
}

impl Unp {
    #[inline(always)]
    fn is_nan(&self) -> bool {
        matches!(self.kind, Kind::QNan | Kind::SNan)
    }
}

/// `FPUnpack` for binary32 (including the `FZ` input flush and IDC).
#[inline]
fn unpack(bits: u32, fpscr: &mut u32) -> Unp {
    let sign = bits & SIGN;
    let e = (bits >> 23) & 0xFF;
    let f = bits & 0x007F_FFFF;
    if e == 0xFF {
        let kind = if f == 0 {
            Kind::Inf
        } else if f & QUIET_BIT != 0 {
            Kind::QNan
        } else {
            Kind::SNan
        };
        Unp { kind, sign, exp: 0, sig: 0, bits }
    } else if e == 0 {
        if f == 0 {
            Unp { kind: Kind::Zero, sign, exp: 0, sig: 0, bits }
        } else if *fpscr & FZ != 0 {
            *fpscr |= IDC;
            Unp { kind: Kind::Zero, sign, exp: 0, sig: 0, bits }
        } else {
            let sh = f.leading_zeros() - 8;
            Unp { kind: Kind::Num, sign, exp: -126 - sh as i32, sig: f << sh, bits }
        }
    } else {
        Unp { kind: Kind::Num, sign, exp: e as i32 - 127, sig: f | 0x0080_0000, bits }
    }
}

/// Right shift that ORs every bit shifted out into bit 0 ("jamming").
#[inline(always)]
fn shr_jam64(x: u64, n: u32) -> u64 {
    if n == 0 {
        x
    } else if n < 64 {
        (x >> n) | (((x << (64 - n)) != 0) as u64)
    } else {
        (x != 0) as u64
    }
}

#[inline(always)]
fn shr_jam128(x: u128, n: u32) -> u128 {
    if n == 0 {
        x
    } else if n < 128 {
        (x >> n) | (((x << (128 - n)) != 0) as u128)
    } else {
        (x != 0) as u128
    }
}

/// Sign of an exact zero sum/difference: negative only when rounding toward -infinity.
#[inline(always)]
fn zero_sum(fpscr: u32) -> u32 {
    if rmode(fpscr) == RMODE_RM {
        SIGN
    } else {
        0
    }
}

#[cold]
#[inline(never)]
fn overflow(sign: u32, rm: u32, fpscr: &mut u32) -> u32 {
    *fpscr |= OFC | IXC;
    let to_inf = match rm {
        RMODE_RN => true,
        RMODE_RP => sign == 0,
        RMODE_RM => sign != 0,
        _ => false,
    };
    sign | if to_inf { INFINITY } else { MAX_NORMAL }
}

/// `FPRound` for binary32 of a non-zero finite value
/// `sig * 2^(exp - 62)` where `sig` has bit 62 set (so `exp` is the unbiased
/// exponent of the leading bit) and bit 0 may carry a sticky ("jam") bit.
/// The rounding mode comes from `fpscr`.
#[inline]
fn round_pack(sign: u32, exp: i32, sig: u64, fpscr: &mut u32) -> u32 {
    debug_assert!(sig >> 62 == 1);
    let rm = rmode(*fpscr);
    let tiny = exp < -126;
    let mut sig = sig;
    if tiny {
        if *fpscr & FZ != 0 {
            // Flush-to-zero decided on the unrounded exponent; never sets IXC.
            *fpscr |= UFC;
            return sign;
        }
        sig = shr_jam64(sig, (-126 - exp) as u32);
    } else if exp > 127 {
        return overflow(sign, rm, fpscr);
    }
    // Bits 62..39 hold the 24-bit significand, bits 38..0 the guard/sticky bits.
    let rem = sig & 0x7F_FFFF_FFFF;
    let mut m = (sig >> 39) as u32;
    if rem != 0 {
        *fpscr |= if tiny { IXC | UFC } else { IXC };
        let up = match rm {
            RMODE_RN => rem > 0x40_0000_0000 || (rem == 0x40_0000_0000 && m & 1 != 0),
            RMODE_RP => sign == 0,
            RMODE_RM => sign != 0,
            _ => false,
        };
        m += up as u32;
    }
    if tiny {
        // Exponent field 0; m == 2^23 after rounding up encodes the minimum normal.
        return sign | m;
    }
    // (biased_exponent - 1) << 23 plus the significand including its hidden bit.
    let bits = (((exp + 126) as u32) << 23).wrapping_add(m);
    if bits >= INFINITY {
        return overflow(sign, rm, fpscr);
    }
    sign | bits
}

/// Normalises `m * 2^e` (non-zero `m`, optional sticky bit in bit 0) and rounds it.
#[inline]
fn norm_round_pack(sign: u32, m: u64, e: i32, fpscr: &mut u32) -> u32 {
    debug_assert!(m != 0);
    let lz = m.leading_zeros();
    let (sig, exp) = if lz == 0 {
        (shr_jam64(m, 1), e + 63)
    } else {
        (m << (lz - 1), e + 63 - lz as i32)
    };
    round_pack(sign, exp, sig, fpscr)
}

/// Like [`norm_round_pack`] for a 128-bit magnitude.
#[inline]
fn norm_round_pack128(sign: u32, m: u128, e: i32, fpscr: &mut u32) -> u32 {
    debug_assert!(m != 0);
    let p = 127 - m.leading_zeros();
    let sig = if p > 62 { shr_jam128(m, p - 62) as u64 } else { (m as u64) << (62 - p) };
    round_pack(sign, e + p as i32, sig, fpscr)
}

#[inline(always)]
fn invalid(fpscr: &mut u32) -> u32 {
    *fpscr |= IOC;
    DEFAULT_NAN
}

/// `FPProcessNaN`.
#[inline]
fn process_nan(u: &Unp, fpscr: &mut u32) -> u32 {
    let mut r = u.bits;
    if u.kind == Kind::SNan {
        r |= QUIET_BIT;
        *fpscr |= IOC;
    }
    if *fpscr & DN != 0 {
        r = DEFAULT_NAN;
    }
    r
}

/// `FPProcessNaNs`: returns the result when either operand is a NaN.
#[inline]
fn nans2(a: &Unp, b: &Unp, fpscr: &mut u32) -> Option<u32> {
    if !(a.is_nan() || b.is_nan()) {
        return None;
    }
    Some(if a.kind == Kind::SNan {
        process_nan(a, fpscr)
    } else if b.kind == Kind::SNan {
        process_nan(b, fpscr)
    } else if a.kind == Kind::QNan {
        process_nan(a, fpscr)
    } else {
        process_nan(b, fpscr)
    })
}

/// `FPProcessNaNs3`.
#[inline]
fn nans3(a: &Unp, b: &Unp, c: &Unp, fpscr: &mut u32) -> Option<u32> {
    if !(a.is_nan() || b.is_nan() || c.is_nan()) {
        return None;
    }
    Some(if a.kind == Kind::SNan {
        process_nan(a, fpscr)
    } else if b.kind == Kind::SNan {
        process_nan(b, fpscr)
    } else if c.kind == Kind::SNan {
        process_nan(c, fpscr)
    } else if a.kind == Kind::QNan {
        process_nan(a, fpscr)
    } else if b.kind == Kind::QNan {
        process_nan(b, fpscr)
    } else {
        process_nan(c, fpscr)
    })
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

/// `FPAdd(a, b)`.
#[inline(never)]
pub fn add(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    add_sub(a, b, false, fpscr)
}

/// `FPSub(a, b)`.
#[inline(never)]
pub fn sub(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    add_sub(a, b, true, fpscr)
}

fn add_sub(a: u32, b: u32, negate_b: bool, fpscr: &mut u32) -> u32 {
    let ua = unpack(a, fpscr);
    let mut ub = unpack(b, fpscr);
    if let Some(r) = nans2(&ua, &ub, fpscr) {
        return r;
    }
    if negate_b {
        ub.sign ^= SIGN;
    }
    match (ua.kind, ub.kind) {
        (Kind::Inf, Kind::Inf) => {
            if ua.sign != ub.sign {
                invalid(fpscr)
            } else {
                ua.sign | INFINITY
            }
        }
        (Kind::Inf, _) => ua.sign | INFINITY,
        (_, Kind::Inf) => ub.sign | INFINITY,
        (Kind::Zero, Kind::Zero) => {
            if ua.sign == ub.sign {
                ua.sign
            } else {
                zero_sum(*fpscr)
            }
        }
        // The other operand is exactly representable: FPRound returns it unchanged.
        (Kind::Zero, _) => ub.sign | (b & 0x7FFF_FFFF),
        (_, Kind::Zero) => ua.sign | (a & 0x7FFF_FFFF),
        _ => add_nums(&ua, &ub, fpscr),
    }
}

/// Exact sum of two non-zero finite operands, rounded once.
fn add_nums(ua: &Unp, ub: &Unp, fpscr: &mut u32) -> u32 {
    let (big, small) = if (ua.exp, ua.sig) >= (ub.exp, ub.sig) { (ua, ub) } else { (ub, ua) };
    let diff = (big.exp - small.exp) as u32;
    // Leading bit at bit 62; 39 guard bits below the 24-bit significand.
    let mb = (big.sig as u64) << 39;
    let ms = shr_jam64((small.sig as u64) << 39, diff);
    if big.sign == small.sign {
        norm_round_pack(big.sign, mb + ms, big.exp - 62, fpscr)
    } else {
        let d = mb - ms;
        if d == 0 {
            zero_sum(*fpscr)
        } else {
            norm_round_pack(big.sign, d, big.exp - 62, fpscr)
        }
    }
}

/// `FPMul(a, b)`.
#[inline(never)]
pub fn mul(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    let ua = unpack(a, fpscr);
    let ub = unpack(b, fpscr);
    if let Some(r) = nans2(&ua, &ub, fpscr) {
        return r;
    }
    let sign = ua.sign ^ ub.sign;
    match (ua.kind, ub.kind) {
        (Kind::Inf, Kind::Zero) | (Kind::Zero, Kind::Inf) => invalid(fpscr),
        (Kind::Inf, _) | (_, Kind::Inf) => sign | INFINITY,
        (Kind::Zero, _) | (_, Kind::Zero) => sign,
        _ => mul_nums(&ua, &ub, fpscr),
    }
}

#[inline]
fn mul_nums(ua: &Unp, ub: &Unp, fpscr: &mut u32) -> u32 {
    let p = (ua.sig as u64) * (ub.sig as u64);
    norm_round_pack(ua.sign ^ ub.sign, p, ua.exp + ub.exp - 46, fpscr)
}

/// `FPDiv(a, b)`.
#[inline(never)]
pub fn div(a: u32, b: u32, fpscr: &mut u32) -> u32 {
    let ua = unpack(a, fpscr);
    let ub = unpack(b, fpscr);
    if let Some(r) = nans2(&ua, &ub, fpscr) {
        return r;
    }
    let sign = ua.sign ^ ub.sign;
    match (ua.kind, ub.kind) {
        (Kind::Inf, Kind::Inf) | (Kind::Zero, Kind::Zero) => invalid(fpscr),
        (Kind::Inf, _) => sign | INFINITY,
        (_, Kind::Zero) => {
            *fpscr |= DZC;
            sign | INFINITY
        }
        (Kind::Zero, _) | (_, Kind::Inf) => sign,
        _ => {
            let num = (ua.sig as u64) << 40;
            let den = ub.sig as u64;
            let q = num / den;
            let r = num % den;
            norm_round_pack(sign, q | (r != 0) as u64, ua.exp - ub.exp - 40, fpscr)
        }
    }
}

/// Floor of the square root of `n` (`n < 2^62`).
fn isqrt(n: u64) -> u64 {
    // The f64 estimate is within 1 of the answer for n < 2^62; fix it up exactly.
    let mut r = (n as f64).sqrt() as u64;
    while r * r > n {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

/// `FPSqrt(a)`.
#[inline(never)]
pub fn sqrt(a: u32, fpscr: &mut u32) -> u32 {
    let ua = unpack(a, fpscr);
    match ua.kind {
        Kind::QNan | Kind::SNan => process_nan(&ua, fpscr),
        Kind::Zero => ua.sign,
        Kind::Inf if ua.sign == 0 => INFINITY,
        _ if ua.sign != 0 => invalid(fpscr),
        _ => {
            // value = sig * 2^e0; scale the radicand so that (e0 - k) is even and
            // the root keeps at least 30 significant bits.
            let e0 = ua.exp - 23;
            let k: u32 = if (e0 - 38) & 1 != 0 { 37 } else { 38 };
            let rad = (ua.sig as u64) << k;
            let r = isqrt(rad);
            let sticky = (r * r != rad) as u64;
            norm_round_pack(0, r | sticky, (e0 - k as i32) / 2, fpscr)
        }
    }
}

/// `FPMulAdd(addend, op1, op2)`: `addend + op1 * op2` with a single rounding.
#[inline(never)]
pub fn fma(addend: u32, op1: u32, op2: u32, fpscr: &mut u32) -> u32 {
    let ua = unpack(addend, fpscr);
    let u1 = unpack(op1, fpscr);
    let u2 = unpack(op2, fpscr);
    let prod_invalid = (u1.kind == Kind::Inf && u2.kind == Kind::Zero)
        || (u1.kind == Kind::Zero && u2.kind == Kind::Inf);
    if let Some(r) = nans3(&ua, &u1, &u2, fpscr) {
        if ua.kind == Kind::QNan && prod_invalid {
            return invalid(fpscr);
        }
        return r;
    }
    let psign = u1.sign ^ u2.sign;
    let inf_a = ua.kind == Kind::Inf;
    let inf_p = u1.kind == Kind::Inf || u2.kind == Kind::Inf;
    let zero_a = ua.kind == Kind::Zero;
    let zero_p = u1.kind == Kind::Zero || u2.kind == Kind::Zero;
    if prod_invalid || (inf_a && inf_p && ua.sign != psign) {
        return invalid(fpscr);
    }
    if (inf_a && ua.sign == 0) || (inf_p && psign == 0) {
        return INFINITY;
    }
    if inf_a || inf_p {
        return SIGN | INFINITY;
    }
    if zero_a && zero_p {
        return if ua.sign == psign { ua.sign } else { zero_sum(*fpscr) };
    }
    if zero_p {
        // Exact non-zero addend: FPRound returns it unchanged.
        return addend;
    }
    // Exact product p * 2^(exp1 + exp2 - 46), p < 2^48.
    let p = (u1.sig as u64) * (u2.sig as u64);
    if zero_a {
        return norm_round_pack(psign, p, u1.exp + u2.exp - 46, fpscr);
    }
    // Both terms are non-zero: add them exactly in a 128-bit window with the
    // leading bit of the larger term at bit 120, then round once.
    let pbit = 63 - p.leading_zeros();
    let pexp = u1.exp + u2.exp - 46 + pbit as i32;
    let pm = (p as u128) << (120 - pbit);
    let am = (ua.sig as u128) << (120 - 23);
    let (bm, bexp, bsign, sm, sexp, ssign) = if (ua.exp, am) >= (pexp, pm) {
        (am, ua.exp, ua.sign, pm, pexp, psign)
    } else {
        (pm, pexp, psign, am, ua.exp, ua.sign)
    };
    let sm = shr_jam128(sm, (bexp - sexp) as u32);
    if bsign == ssign {
        norm_round_pack128(bsign, bm + sm, bexp - 120, fpscr)
    } else {
        let d = bm - sm;
        if d == 0 {
            zero_sum(*fpscr)
        } else {
            norm_round_pack128(bsign, d, bexp - 120, fpscr)
        }
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// `FPCompare(a, b, quiet_nan_exc)`: returns the NZCV result in bits 31:28
/// (unordered `0011`, equal `0110`, less `1000`, greater `0010`).
#[inline(never)]
pub fn compare(a: u32, b: u32, quiet_nan_exc: bool, fpscr: &mut u32) -> u32 {
    let ua = unpack(a, fpscr);
    let ub = unpack(b, fpscr);
    if ua.is_nan() || ub.is_nan() {
        if ua.kind == Kind::SNan || ub.kind == Kind::SNan || quiet_nan_exc {
            *fpscr |= IOC;
        }
        return 0x3000_0000;
    }
    #[inline(always)]
    fn key(u: &Unp) -> i64 {
        let mag = match u.kind {
            Kind::Zero => 0,
            _ => (u.bits & 0x7FFF_FFFF) as i64,
        };
        if u.sign != 0 {
            -mag
        } else {
            mag
        }
    }
    let (ka, kb) = (key(&ua), key(&ub));
    if ka == kb {
        0x6000_0000
    } else if ka < kb {
        0x8000_0000
    } else {
        0x2000_0000
    }
}

// ---------------------------------------------------------------------------
// Conversions with integers / fixed point
// ---------------------------------------------------------------------------

/// Fraction class used while rounding a float to an integer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frac {
    Zero,
    Below,
    Half,
    Above,
}

/// `FPToFixed`: converts to a `size`-bit (16 or 32) signed or unsigned
/// fixed-point value with `fbits` fraction bits, rounding with `round`
/// (`RMODE_*`), saturating with IOC. The result is sign- (signed) or
/// zero- (unsigned) extended to 32 bits, as VCVT leaves it in the register.
#[inline(never)]
pub fn to_fixed(
    a: u32,
    size: u32,
    fbits: u32,
    unsigned: bool,
    round: u32,
    fpscr: &mut u32,
) -> u32 {
    debug_assert!(size == 16 || size == 32);
    let (max_pos, max_neg_mag) = if unsigned {
        ((1u64 << size) - 1, 0u64)
    } else {
        ((1u64 << (size - 1)) - 1, 1u64 << (size - 1))
    };
    let sat = |neg: bool| -> u32 {
        if unsigned {
            if neg {
                0
            } else {
                max_pos as u32
            }
        } else if neg {
            (max_neg_mag as i64).wrapping_neg() as u32
        } else {
            max_pos as u32
        }
    };
    let u = unpack(a, fpscr);
    let neg = u.sign != 0;
    match u.kind {
        Kind::QNan | Kind::SNan => {
            *fpscr |= IOC;
            return 0;
        }
        Kind::Zero => return 0,
        Kind::Inf => {
            *fpscr |= IOC;
            return sat(neg);
        }
        Kind::Num => {}
    }
    let sig = u.sig as u64;
    let sh = u.exp - 23 + fbits as i32;
    let (mut mag, frac) = if sh >= 0 {
        if sh >= 40 {
            *fpscr |= IOC;
            return sat(neg);
        }
        (sig << sh, Frac::Zero)
    } else {
        let r = (-sh) as u32;
        if r > 31 {
            (0, Frac::Below)
        } else {
            let rem = sig & ((1u64 << r) - 1);
            let half = 1u64 << (r - 1);
            let frac = if rem == 0 {
                Frac::Zero
            } else if rem < half {
                Frac::Below
            } else if rem == half {
                Frac::Half
            } else {
                Frac::Above
            };
            (sig >> r, frac)
        }
    };
    let inexact = frac != Frac::Zero;
    let up = match round {
        RMODE_RN => frac == Frac::Above || (frac == Frac::Half && mag & 1 != 0),
        RMODE_RP => inexact && !neg,
        RMODE_RM => inexact && neg,
        _ => false,
    };
    mag += up as u64;
    let overflow = if unsigned {
        (neg && mag != 0) || mag > max_pos
    } else if neg {
        mag > max_neg_mag
    } else {
        mag > max_pos
    };
    if overflow {
        *fpscr |= IOC;
        return sat(neg);
    }
    if inexact {
        *fpscr |= IXC;
    }
    if neg {
        (mag as i64).wrapping_neg() as u32
    } else {
        mag as u32
    }
}

/// `FixedToFP`: converts the low `size` bits (16 or 32) of `x`, interpreted
/// as signed or unsigned fixed point with `fbits` fraction bits, to binary32
/// using the rounding mode in `fpscr`.
#[inline(never)]
pub fn from_fixed(x: u32, size: u32, fbits: u32, unsigned: bool, fpscr: &mut u32) -> u32 {
    debug_assert!(size == 16 || size == 32);
    let (sign, mag) = if unsigned {
        (0, if size == 16 { (x & 0xFFFF) as u64 } else { x as u64 })
    } else {
        let v = if size == 16 { x as u16 as i16 as i64 } else { x as i32 as i64 };
        (if v < 0 { SIGN } else { 0 }, v.unsigned_abs())
    };
    if mag == 0 {
        return 0;
    }
    norm_round_pack(sign, mag, -(fbits as i32), fpscr)
}

// ---------------------------------------------------------------------------
// Half precision
// ---------------------------------------------------------------------------

/// `FPHalfToSingle` (IEEE half precision, or the alternative format when
/// `FPSCR.AHP` is set). Exact; only NaN inputs raise IOC.
#[inline(never)]
pub fn f16_to_f32(h: u16, fpscr: &mut u32) -> u32 {
    let h = h as u32;
    let sign = (h & 0x8000) << 16;
    let e = (h >> 10) & 0x1F;
    let f = h & 0x3FF;
    if e == 0x1F && *fpscr & AHP == 0 {
        if f == 0 {
            return sign | INFINITY;
        }
        if f & 0x200 == 0 {
            *fpscr |= IOC;
        }
        if *fpscr & DN != 0 {
            return DEFAULT_NAN;
        }
        return sign | 0x7FC0_0000 | ((f & 0x1FF) << 13);
    }
    if e == 0 {
        if f == 0 {
            return sign;
        }
        // Subnormal half: f * 2^-24, normalised.
        let sh = f.leading_zeros() - 21;
        let m = (f << sh) & 0x3FF;
        return sign | ((127 - 14 - sh) << 23) | (m << 13);
    }
    sign | ((e + 112) << 23) | (f << 13)
}

/// `FPRound` of `sig * 2^(exp - 62)` to binary16 (`sig` has bit 62 set).
fn round_pack_f16(sign16: u32, exp: i32, sig: u64, fpscr: &mut u32) -> u32 {
    let rm = rmode(*fpscr);
    let ahp = *fpscr & AHP != 0;
    let tiny = exp < -14;
    let mut sig = sig;
    if tiny {
        sig = shr_jam64(sig, (-14 - exp) as u32);
    } else if exp > 17 {
        return half_overflow(sign16, rm, ahp, fpscr);
    }
    // Bits 62..52 hold the 11-bit significand, bits 51..0 the guard/sticky bits.
    let rem = sig & 0xF_FFFF_FFFF_FFFF;
    let inexact = rem != 0;
    let mut m = (sig >> 52) as u32;
    if inexact {
        let half = 1u64 << 51;
        let up = match rm {
            RMODE_RN => rem > half || (rem == half && m & 1 != 0),
            RMODE_RP => sign16 == 0,
            RMODE_RM => sign16 != 0,
            _ => false,
        };
        m += up as u32;
    }
    if tiny {
        if inexact {
            *fpscr |= IXC | UFC;
        }
        return sign16 | m;
    }
    let bits = (((exp + 14) as u32) << 10).wrapping_add(m);
    let limit = if ahp { 32 << 10 } else { 31 << 10 };
    if bits >= limit {
        // Sets the overflow flags itself (AHP: IOC only, never inexact).
        return half_overflow(sign16, rm, ahp, fpscr);
    }
    if inexact {
        *fpscr |= IXC;
    }
    sign16 | bits
}

#[cold]
fn half_overflow(sign16: u32, rm: u32, ahp: bool, fpscr: &mut u32) -> u32 {
    if ahp {
        // Alternative format has no infinity: saturate, IOC, and no inexact.
        *fpscr |= IOC;
        return sign16 | 0x7FFF;
    }
    *fpscr |= OFC | IXC;
    let to_inf = match rm {
        RMODE_RN => true,
        RMODE_RP => sign16 == 0,
        RMODE_RM => sign16 != 0,
        _ => false,
    };
    sign16 | if to_inf { 0x7C00 } else { 0x7BFF }
}

/// `FPSingleToHalf` (IEEE half precision, or the alternative format when
/// `FPSCR.AHP` is set), using the rounding mode in `fpscr`.
#[inline(never)]
pub fn f32_to_f16(a: u32, fpscr: &mut u32) -> u16 {
    let u = unpack(a, fpscr);
    let ahp = *fpscr & AHP != 0;
    let sign16 = (u.sign >> 16) & 0x8000;
    let r = match u.kind {
        Kind::QNan | Kind::SNan => {
            let r = if ahp {
                sign16
            } else if *fpscr & DN != 0 {
                0x7E00
            } else {
                sign16 | 0x7C00 | 0x0200 | ((a >> 13) & 0x1FF)
            };
            if u.kind == Kind::SNan || ahp {
                *fpscr |= IOC;
            }
            r
        }
        Kind::Inf => {
            if ahp {
                *fpscr |= IOC;
                sign16 | 0x7FFF
            } else {
                sign16 | 0x7C00
            }
        }
        Kind::Zero => sign16,
        Kind::Num => {
            let sig = (u.sig as u64) << 39;
            round_pack_f16(sign16, u.exp, sig, fpscr)
        }
    };
    r as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jam_shifts() {
        assert_eq!(shr_jam64(0b1000, 3), 1);
        assert_eq!(shr_jam64(0b1001, 3), 0b1 | 1);
        assert_eq!(shr_jam64(0b1001, 0), 0b1001);
        assert_eq!(shr_jam64(5, 64), 1);
        assert_eq!(shr_jam64(0, 200), 0);
        assert_eq!(shr_jam128(1 << 100, 100), 1);
        assert_eq!(shr_jam128((1 << 100) | 1, 100), 1);
        assert_eq!(shr_jam128(3, 128), 1);
    }

    #[test]
    fn integer_sqrt() {
        for n in [0u64, 1, 2, 3, 4, 15, 16, 17, (1 << 62) - 1, 1 << 60, 999_999_999_999] {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "isqrt({n})");
        }
    }

    #[test]
    fn simple_arithmetic() {
        let mut f = 0;
        assert_eq!(add(0x3F80_0000, 0x3F80_0000, &mut f), 0x4000_0000);
        assert_eq!(f, 0);
        assert_eq!(mul(0x4000_0000, 0x4040_0000, &mut f), 0x40C0_0000);
        assert_eq!(div(0x3F80_0000, 0x4040_0000, &mut f), 0x3EAA_AAAB);
        assert_eq!(f, IXC);
        let mut f = 0;
        assert_eq!(sqrt(0x4080_0000, &mut f), 0x4000_0000);
        assert_eq!(f, 0);
    }
}
