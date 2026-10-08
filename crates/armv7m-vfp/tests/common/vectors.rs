//! Hand-checked test vectors (expected values worked out from the Arm
//! pseudocode, independent of the implementation). The same table is run
//! against the library (`ieee_vectors.rs`) and, on aarch64, against the
//! host hardware (`ieee_hw_oracle.rs`), so a mistake in a vector cannot hide.
#![allow(dead_code)]

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{ieee, soft};

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Sqrt,
    /// `a` = addend, `b` = op1, `c` = op2: a + b*c fused.
    Fma,
    /// Compare `a` with `b`; result is NZCV in bits 31:28.
    Cmp { e: bool },
    CmpZero { e: bool },
    /// float -> 32-bit integer: VCVT (round toward zero) or VCVTR (FPSCR rounding).
    ToInt { unsigned: bool, round_zero: bool },
    FromInt { unsigned: bool },
    ToFixed { size: u32, fbits: u32, unsigned: bool },
    FromFixed { size: u32, fbits: u32, unsigned: bool },
    ToHalf,
    FromHalf,
}

#[derive(Clone, Copy, Debug)]
pub struct V {
    pub name: &'static str,
    pub op: Op,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    /// FPSCR mode bits (RMode, FZ, DN, AHP) on entry.
    pub mode: u32,
    pub expect: u32,
    /// Expected cumulative flags (IOC, DZC, OFC, UFC, IXC, IDC).
    pub flags: u32,
}

pub const RN: u32 = 0;
pub const RP: u32 = 1 << 22;
pub const RM: u32 = 2 << 22;
pub const RZ: u32 = 3 << 22;

pub const ONE: u32 = 0x3F80_0000;
pub const TWO: u32 = 0x4000_0000;
pub const THREE: u32 = 0x4040_0000;
pub const INF: u32 = 0x7F80_0000;
pub const NINF: u32 = 0xFF80_0000;
pub const MAXF: u32 = 0x7F7F_FFFF;
pub const NMAXF: u32 = 0xFF7F_FFFF;
pub const MINN: u32 = 0x0080_0000;
pub const QNAN: u32 = 0x7FC0_0000;
pub const SNAN: u32 = 0x7F80_0001;
pub const NZERO: u32 = 0x8000_0000;

macro_rules! v {
    ($name:expr, $op:expr, $a:expr, $b:expr, $c:expr, $mode:expr, $expect:expr, $flags:expr) => {
        V { name: $name, op: $op, a: $a, b: $b, c: $c, mode: $mode, expect: $expect, flags: $flags }
    };
}


pub fn vectors() -> Vec<V> {
    use Op::*;
    let mut t: Vec<V> = Vec::new();
    // ---- addition / subtraction: rounding modes -----------------------------------
    // 1 + 2^-24 is exactly half way between 1.0 and 1+2^-23.
    t.push(v!("1+2^-24 RN tie to even", Add, ONE, 0x3380_0000, 0, RN, 0x3F80_0000, IXC));
    t.push(v!("1+2^-24 RP", Add, ONE, 0x3380_0000, 0, RP, 0x3F80_0001, IXC));
    t.push(v!("1+2^-24 RM", Add, ONE, 0x3380_0000, 0, RM, 0x3F80_0000, IXC));
    t.push(v!("1+2^-24 RZ", Add, ONE, 0x3380_0000, 0, RZ, 0x3F80_0000, IXC));
    t.push(v!("-1-2^-24 RN", Add, 0xBF80_0000, 0xB380_0000, 0, RN, 0xBF80_0000, IXC));
    t.push(v!("-1-2^-24 RP", Add, 0xBF80_0000, 0xB380_0000, 0, RP, 0xBF80_0000, IXC));
    t.push(v!("-1-2^-24 RM", Add, 0xBF80_0000, 0xB380_0000, 0, RM, 0xBF80_0001, IXC));
    t.push(v!("-1-2^-24 RZ", Add, 0xBF80_0000, 0xB380_0000, 0, RZ, 0xBF80_0000, IXC));
    t.push(v!("1+0.75ulp RN", Add, ONE, 0x33C0_0000, 0, RN, 0x3F80_0001, IXC));
    t.push(v!("1+0.75ulp RZ", Add, ONE, 0x33C0_0000, 0, RZ, 0x3F80_0000, IXC));
    t.push(v!("odd + half ulp ties to even", Add, 0x3F80_0001, 0x3380_0000, 0, RN, 0x3F80_0002, IXC));
    t.push(v!("exact sum", Add, ONE, TWO, 0, RN, THREE, 0));
    t.push(v!("sub exact", Sub, THREE, ONE, 0, RN, TWO, 0));
    // exact zero results: sign depends on the rounding mode
    t.push(v!("1-1 RN", Sub, ONE, ONE, 0, RN, 0, 0));
    t.push(v!("1-1 RM", Sub, ONE, ONE, 0, RM, NZERO, 0));
    t.push(v!("1-1 RP", Sub, ONE, ONE, 0, RP, 0, 0));
    t.push(v!("1+(-1) RM", Add, ONE, 0xBF80_0000, 0, RM, NZERO, 0));
    t.push(v!("+0 + -0 RN", Add, 0, NZERO, 0, RN, 0, 0));
    t.push(v!("+0 + -0 RM", Add, 0, NZERO, 0, RM, NZERO, 0));
    t.push(v!("-0 + -0 RN", Add, NZERO, NZERO, 0, RN, NZERO, 0));
    t.push(v!("-0 + -0 RP", Add, NZERO, NZERO, 0, RP, NZERO, 0));
    t.push(v!("+0 - +0 RN", Sub, 0, 0, 0, RN, 0, 0));
    t.push(v!("+0 - +0 RM", Sub, 0, 0, 0, RM, NZERO, 0));
    t.push(v!("+0 - -0 RM", Sub, 0, NZERO, 0, RM, 0, 0));
    t.push(v!("-0 - +0 RN", Sub, NZERO, 0, 0, RN, NZERO, 0));
    // overflow
    t.push(v!("max+max RN", Add, MAXF, MAXF, 0, RN, INF, OFC | IXC));
    t.push(v!("max+max RP", Add, MAXF, MAXF, 0, RP, INF, OFC | IXC));
    t.push(v!("max+max RM", Add, MAXF, MAXF, 0, RM, MAXF, OFC | IXC));
    t.push(v!("max+max RZ", Add, MAXF, MAXF, 0, RZ, MAXF, OFC | IXC));
    t.push(v!("-max-max RN", Add, NMAXF, NMAXF, 0, RN, NINF, OFC | IXC));
    t.push(v!("-max-max RP", Add, NMAXF, NMAXF, 0, RP, NMAXF, OFC | IXC));
    t.push(v!("-max-max RM", Add, NMAXF, NMAXF, 0, RM, NINF, OFC | IXC));
    t.push(v!("-max-max RZ", Add, NMAXF, NMAXF, 0, RZ, NMAXF, OFC | IXC));
    // subnormals are exact for addition
    t.push(v!("minsub+minsub", Add, 1, 1, 0, RN, 2, 0));
    t.push(v!("minnormal - minsub", Sub, MINN, 1, 0, RN, 0x007F_FFFF, 0));
    // infinities and NaNs
    t.push(v!("inf-inf", Sub, INF, INF, 0, RN, QNAN, IOC));
    t.push(v!("inf+-inf", Add, INF, NINF, 0, RN, QNAN, IOC));
    t.push(v!("inf+inf", Add, INF, INF, 0, RN, INF, 0));
    t.push(v!("inf+1", Add, INF, ONE, 0, RN, INF, 0));
    t.push(v!("1-inf", Sub, ONE, INF, 0, RN, NINF, 0));
    t.push(v!("qnan first operand", Add, 0x7FC0_0001, 0x7FC0_0002, 0, RN, 0x7FC0_0001, 0));
    t.push(v!("qnan second operand", Add, ONE, 0x7FC0_0002, 0, RN, 0x7FC0_0002, 0));
    t.push(v!("snan beats qnan (second)", Add, 0x7FC0_0001, 0x7F80_0002, 0, RN, 0x7FC0_0002, IOC));
    t.push(v!("snan first wins", Add, 0x7F80_0001, 0x7F80_0002, 0, RN, 0x7FC0_0001, IOC));
    t.push(v!("negative qnan keeps sign", Add, 0xFFC0_0001, ONE, 0, RN, 0xFFC0_0001, 0));
    t.push(v!("sub keeps nan sign", Sub, ONE, 0xFFC0_0007, 0, RN, 0xFFC0_0007, 0));
    t.push(v!("DN qnan", Add, 0x7FC0_1234, ONE, 0, DN, QNAN, 0));
    t.push(v!("DN snan", Add, 0x7F80_0001, ONE, 0, DN, QNAN, IOC));
    t.push(v!("no DN snan quiets", Add, 0x7F80_0001, ONE, 0, RN, 0x7FC0_0001, IOC));
    // flush to zero
    t.push(v!("FZ denormal input", Add, 1, ONE, 0, FZ, ONE, IDC));
    t.push(v!("FZ denormal both", Add, 1, 2, 0, FZ, 0, IDC));
    t.push(v!("no FZ denormal sum", Add, 1, 2, 0, RN, 3, 0));
    t.push(v!("FZ keeps zero sign", Add, 0x8000_0001, 0x8000_0000, 0, FZ, NZERO, IDC));
    // ---- multiplication -----------------------------------------------------------
    t.push(v!("2*3", Mul, TWO, THREE, 0, RN, 0x40C0_0000, 0));
    t.push(v!("inf*0", Mul, INF, 0, 0, RN, QNAN, IOC));
    t.push(v!("0*inf", Mul, 0, NINF, 0, RN, QNAN, IOC));
    t.push(v!("inf*-2", Mul, INF, 0xC000_0000, 0, RN, NINF, 0));
    t.push(v!("0*-3", Mul, 0, 0xC040_0000, 0, RN, NZERO, 0));
    t.push(v!("max*2 RN", Mul, MAXF, TWO, 0, RN, INF, OFC | IXC));
    t.push(v!("max*2 RZ", Mul, MAXF, TWO, 0, RZ, MAXF, OFC | IXC));
    t.push(v!("-max*2 RM", Mul, NMAXF, TWO, 0, RM, NINF, OFC | IXC));
    t.push(v!("-max*2 RP", Mul, NMAXF, TWO, 0, RP, NMAXF, OFC | IXC));
    t.push(v!("2^-100*2^-30 exact subnormal", Mul, 0x0D80_0000, 0x3080_0000, 0, RN, 0x0008_0000, 0));
    t.push(v!("tiny inexact RN", Mul, 0x0D80_0001, 0x3080_0000, 0, RN, 0x0008_0000, UFC | IXC));
    t.push(v!("tiny inexact RP", Mul, 0x0D80_0001, 0x3080_0000, 0, RP, 0x0008_0001, UFC | IXC));
    t.push(v!("FZ output flush", Mul, 0x0D80_0000, 0x3080_0000, 0, FZ, 0, UFC));
    t.push(v!("FZ output flush keeps sign", Mul, 0x8D80_0000, 0x3080_0000, 0, FZ, NZERO, UFC));
    t.push(v!("FZ input flush", Mul, 1, TWO, 0, FZ, 0, IDC));
    t.push(v!("FZ min normal*0.5 flushes", Mul, MINN, 0x3F00_0000, 0, FZ, 0, UFC));
    t.push(v!("min normal*0.5 exact subnormal", Mul, MINN, 0x3F00_0000, 0, RN, 0x0040_0000, 0));
    // (1 - 2^-24) * 2^-126 is exactly half way between the largest subnormal and the
    // minimum normal: it rounds (RN, ties to even) to the minimum normal, but tininess is
    // detected before rounding, so UFC is set; with FZ the unrounded exponent flushes it.
    t.push(v!("underflow before rounding RN", Mul, 0x3F7F_FFFF, MINN, 0, RN, 0x0080_0000, UFC | IXC));
    t.push(v!("underflow before rounding RZ", Mul, 0x3F7F_FFFF, MINN, 0, RZ, 0x007F_FFFF, UFC | IXC));
    t.push(v!("underflow before rounding RP", Mul, 0x3F7F_FFFF, MINN, 0, RP, 0x0080_0000, UFC | IXC));
    t.push(v!("underflow before rounding FZ", Mul, 0x3F7F_FFFF, MINN, 0, FZ, 0, UFC));
    // ---- division -----------------------------------------------------------------
    t.push(v!("1/3 RN", Div, ONE, THREE, 0, RN, 0x3EAA_AAAB, IXC));
    t.push(v!("1/3 RZ", Div, ONE, THREE, 0, RZ, 0x3EAA_AAAA, IXC));
    t.push(v!("1/3 RM", Div, ONE, THREE, 0, RM, 0x3EAA_AAAA, IXC));
    t.push(v!("1/3 RP", Div, ONE, THREE, 0, RP, 0x3EAA_AAAB, IXC));
    t.push(v!("6/3", Div, 0x40C0_0000, THREE, 0, RN, TWO, 0));
    t.push(v!("1/0", Div, ONE, 0, 0, RN, INF, DZC));
    t.push(v!("-1/0", Div, 0xBF80_0000, 0, 0, RN, NINF, DZC));
    t.push(v!("1/-0", Div, ONE, NZERO, 0, RN, NINF, DZC));
    t.push(v!("0/0", Div, 0, 0, 0, RN, QNAN, IOC));
    t.push(v!("inf/inf", Div, INF, INF, 0, RN, QNAN, IOC));
    t.push(v!("inf/0", Div, INF, 0, 0, RN, INF, 0));
    t.push(v!("1/inf", Div, ONE, INF, 0, RN, 0, 0));
    t.push(v!("-1/inf", Div, 0xBF80_0000, INF, 0, RN, NZERO, 0));
    t.push(v!("0/-5", Div, 0, 0xC0A0_0000, 0, RN, NZERO, 0));
    t.push(v!("qnan/1", Div, 0x7FC0_00AA, ONE, 0, RN, 0x7FC0_00AA, 0));
    t.push(v!("1/snan", Div, ONE, 0x7F80_0003, 0, RN, 0x7FC0_0003, IOC));
    t.push(v!("max/0.5 RN", Div, MAXF, 0x3F00_0000, 0, RN, INF, OFC | IXC));
    t.push(v!("max/0.5 RZ", Div, MAXF, 0x3F00_0000, 0, RZ, MAXF, OFC | IXC));
    t.push(v!("min normal/2 exact subnormal", Div, MINN, TWO, 0, RN, 0x0040_0000, 0));
    t.push(v!("FZ min normal/2 flushes", Div, MINN, TWO, 0, FZ, 0, UFC));
    t.push(v!("1/2^127 exact subnormal", Div, ONE, 0x7F00_0000, 0, RN, 0x0040_0000, 0));
    // ---- square root --------------------------------------------------------------
    t.push(v!("sqrt 4", Sqrt, 0x4080_0000, 0, 0, RN, TWO, 0));
    t.push(v!("sqrt 2 RN", Sqrt, TWO, 0, 0, RN, 0x3FB5_04F3, IXC));
    t.push(v!("sqrt 2 RZ", Sqrt, TWO, 0, 0, RZ, 0x3FB5_04F3, IXC));
    t.push(v!("sqrt 2 RM", Sqrt, TWO, 0, 0, RM, 0x3FB5_04F3, IXC));
    t.push(v!("sqrt 2 RP", Sqrt, TWO, 0, 0, RP, 0x3FB5_04F4, IXC));
    t.push(v!("sqrt -1", Sqrt, 0xBF80_0000, 0, 0, RN, QNAN, IOC));
    t.push(v!("sqrt -0", Sqrt, NZERO, 0, 0, RN, NZERO, 0));
    t.push(v!("sqrt +0", Sqrt, 0, 0, 0, RN, 0, 0));
    t.push(v!("sqrt inf", Sqrt, INF, 0, 0, RN, INF, 0));
    t.push(v!("sqrt -inf", Sqrt, NINF, 0, 0, RN, QNAN, IOC));
    t.push(v!("sqrt qnan", Sqrt, 0x7FC0_0042, 0, 0, RN, 0x7FC0_0042, 0));
    t.push(v!("sqrt snan", Sqrt, 0x7F80_0001, 0, 0, RN, 0x7FC0_0001, IOC));
    t.push(v!("sqrt snan DN", Sqrt, 0x7F80_0001, 0, 0, DN, QNAN, IOC));
    t.push(v!("sqrt -denormal FZ", Sqrt, 0x8000_0001, 0, 0, FZ, NZERO, IDC));
    // ---- fused multiply-add: addend + op1 * op2 ------------------------------------
    t.push(v!("fma 1+2*3", Fma, ONE, TWO, THREE, RN, 0x40E0_0000, 0));
    t.push(v!("fma exact residual", Fma, 0xBF80_0800, 0x3F80_0400, 0x3F80_0400, RN, 0x3280_0000, 0));
    t.push(v!("fma inf*0 + qnan", Fma, 0x7FC0_0055, INF, 0, RN, QNAN, IOC));
    t.push(v!("fma 0*inf + qnan", Fma, 0x7FC0_0055, 0, INF, RN, QNAN, IOC));
    t.push(v!("fma inf*0 + snan", Fma, 0x7F80_0055, INF, 0, RN, 0x7FC0_0055, IOC));
    t.push(v!("fma inf*0 + 1", Fma, ONE, INF, 0, RN, QNAN, IOC));
    t.push(v!("fma snan addend", Fma, 0x7F80_0001, ONE, ONE, RN, 0x7FC0_0001, IOC));
    t.push(v!("fma qnan addend, snan op1", Fma, 0x7FC0_0011, 0x7F80_0022, ONE, RN, 0x7FC0_0022, IOC));
    t.push(v!("fma qnan op1 beats qnan op2", Fma, ONE, 0x7FC0_0033, 0x7FC0_0044, RN, 0x7FC0_0033, 0));
    t.push(v!("fma qnan addend first", Fma, 0x7FC0_0011, 0x7FC0_0033, ONE, RN, 0x7FC0_0011, 0));
    t.push(v!("fma inf + -inf", Fma, INF, NINF, ONE, RN, QNAN, IOC));
    t.push(v!("fma 1 + 0*5", Fma, ONE, 0, 0x40A0_0000, RN, ONE, 0));
    t.push(v!("fma +0 + -0*1 RN", Fma, 0, NZERO, ONE, RN, 0, 0));
    t.push(v!("fma +0 + -0*1 RM", Fma, 0, NZERO, ONE, RM, NZERO, 0));
    t.push(v!("fma -0 + -0*1", Fma, NZERO, NZERO, ONE, RN, NZERO, 0));
    t.push(v!("fma overflow", Fma, MAXF, MAXF, TWO, RN, INF, OFC | IXC));
    t.push(v!("fma overflow RZ", Fma, MAXF, MAXF, TWO, RZ, MAXF, OFC | IXC));
    t.push(v!("fma exact subnormal product", Fma, 0, 0x0D80_0000, 0x3080_0000, RN, 0x0008_0000, 0));
    t.push(v!("fma cancels exactly RN", Fma, 0xC080_0000, TWO, TWO, RN, 0, 0));
    t.push(v!("fma cancels exactly RM", Fma, 0xC080_0000, TWO, TWO, RM, NZERO, 0));
    // (1+2^-12)(1-2^-12) + (-1) = -2^-24, exactly
    t.push(v!("fma exact cancellation to -2^-24", Fma, 0xBF80_0000, 0x3F80_0800, 0x3F7F_F000, RN, 0xB380_0000, 0));
    t.push(v!("fma FZ denormal addend", Fma, 1, ONE, ONE, FZ, ONE, IDC));
    // ---- comparison (result = NZCV) -----------------------------------------------
    t.push(v!("cmp less", Cmp { e: false }, ONE, TWO, 0, RN, 0x8000_0000, 0));
    t.push(v!("cmp greater", Cmp { e: false }, TWO, ONE, 0, RN, 0x2000_0000, 0));
    t.push(v!("cmp equal", Cmp { e: false }, ONE, ONE, 0, RN, 0x6000_0000, 0));
    t.push(v!("cmp +0 -0", Cmp { e: false }, 0, NZERO, 0, RN, 0x6000_0000, 0));
    t.push(v!("cmp -inf < inf", Cmp { e: false }, NINF, INF, 0, RN, 0x8000_0000, 0));
    t.push(v!("cmp inf = inf", Cmp { e: false }, INF, INF, 0, RN, 0x6000_0000, 0));
    t.push(v!("cmp -1 < 1", Cmp { e: true }, 0xBF80_0000, ONE, 0, RN, 0x8000_0000, 0));
    t.push(v!("cmp qnan vcmp", Cmp { e: false }, QNAN, ONE, 0, RN, 0x3000_0000, 0));
    t.push(v!("cmp qnan vcmpe", Cmp { e: true }, QNAN, ONE, 0, RN, 0x3000_0000, IOC));
    t.push(v!("cmp snan vcmp", Cmp { e: false }, ONE, SNAN, 0, RN, 0x3000_0000, IOC));
    t.push(v!("cmp zero vs 0", CmpZero { e: false }, 0, 0, 0, RN, 0x6000_0000, 0));
    t.push(v!("cmp -1 vs 0", CmpZero { e: true }, 0xBF80_0000, 0, 0, RN, 0x8000_0000, 0));
    t.push(v!("cmp denormal vs 0", CmpZero { e: false }, 1, 0, 0, RN, 0x2000_0000, 0));
    t.push(v!("cmp FZ denormal vs 0", CmpZero { e: false }, 1, 0, 0, FZ, 0x6000_0000, IDC));
    t.push(v!("cmp FZ denormals equal", Cmp { e: false }, 1, 0x8000_0003, 0, FZ, 0x6000_0000, IDC));
    // ---- float -> 32-bit integer ----------------------------------------------------
    let vcvt_s = ToInt { unsigned: false, round_zero: true };
    let vcvt_u = ToInt { unsigned: true, round_zero: true };
    let vcvtr_s = ToInt { unsigned: false, round_zero: false };
    let vcvtr_u = ToInt { unsigned: true, round_zero: false };
    t.push(v!("3.7 -> s32", vcvt_s, 0x406C_CCCD, 0, 0, RN, 3, IXC));
    t.push(v!("-3.7 -> s32", vcvt_s, 0xC06C_CCCD, 0, 0, RN, 0xFFFF_FFFD, IXC));
    t.push(v!("3.7 -> s32 ignores RMode", vcvt_s, 0x406C_CCCD, 0, 0, RP, 3, IXC));
    t.push(v!("2^31 -> s32 saturates", vcvt_s, 0x4F00_0000, 0, 0, RN, 0x7FFF_FFFF, IOC));
    t.push(v!("-2^31 -> s32 exact", vcvt_s, 0xCF00_0000, 0, 0, RN, 0x8000_0000, 0));
    t.push(v!("-2^31-256 -> s32 saturates", vcvt_s, 0xCF00_0001, 0, 0, RN, 0x8000_0000, IOC));
    t.push(v!("2^31-128 -> s32 exact", vcvt_s, 0x4EFF_FFFF, 0, 0, RN, 0x7FFF_FF80, 0));
    t.push(v!("2^32 -> u32 saturates", vcvt_u, 0x4F80_0000, 0, 0, RN, 0xFFFF_FFFF, IOC));
    t.push(v!("2^32-256 -> u32 exact", vcvt_u, 0x4F7F_FFFF, 0, 0, RN, 0xFFFF_FF00, 0));
    t.push(v!("-1 -> u32", vcvt_u, 0xBF80_0000, 0, 0, RN, 0, IOC));
    t.push(v!("-0.5 -> u32 inexact", vcvt_u, 0xBF00_0000, 0, 0, RN, 0, IXC));
    t.push(v!("0.5 -> u32", vcvt_u, 0x3F00_0000, 0, 0, RN, 0, IXC));
    t.push(v!("-0 -> s32", vcvt_s, NZERO, 0, 0, RN, 0, 0));
    t.push(v!("qnan -> s32", vcvt_s, QNAN, 0, 0, RN, 0, IOC));
    t.push(v!("snan -> u32", vcvt_u, SNAN, 0, 0, RN, 0, IOC));
    t.push(v!("+inf -> s32", vcvt_s, INF, 0, 0, RN, 0x7FFF_FFFF, IOC));
    t.push(v!("-inf -> s32", vcvt_s, NINF, 0, 0, RN, 0x8000_0000, IOC));
    t.push(v!("+inf -> u32", vcvt_u, INF, 0, 0, RN, 0xFFFF_FFFF, IOC));
    t.push(v!("-inf -> u32", vcvt_u, NINF, 0, 0, RN, 0, IOC));
    t.push(v!("denormal -> s32", vcvt_s, 1, 0, 0, RN, 0, IXC));
    t.push(v!("FZ denormal -> s32", vcvt_s, 1, 0, 0, FZ, 0, IDC));
    t.push(v!("vcvtr 2.5 RN", vcvtr_s, 0x4020_0000, 0, 0, RN, 2, IXC));
    t.push(v!("vcvtr 2.5 RP", vcvtr_s, 0x4020_0000, 0, 0, RP, 3, IXC));
    t.push(v!("vcvtr 2.5 RM", vcvtr_s, 0x4020_0000, 0, 0, RM, 2, IXC));
    t.push(v!("vcvtr 2.5 RZ", vcvtr_s, 0x4020_0000, 0, 0, RZ, 2, IXC));
    t.push(v!("vcvtr 3.5 RN", vcvtr_s, 0x4060_0000, 0, 0, RN, 4, IXC));
    t.push(v!("vcvtr -2.5 RN", vcvtr_s, 0xC020_0000, 0, 0, RN, 0xFFFF_FFFE, IXC));
    t.push(v!("vcvtr -2.5 RP", vcvtr_s, 0xC020_0000, 0, 0, RP, 0xFFFF_FFFE, IXC));
    t.push(v!("vcvtr -2.5 RM", vcvtr_s, 0xC020_0000, 0, 0, RM, 0xFFFF_FFFD, IXC));
    t.push(v!("vcvtr -2.5 RZ", vcvtr_s, 0xC020_0000, 0, 0, RZ, 0xFFFF_FFFE, IXC));
    t.push(v!("vcvtr 2.0 exact", vcvtr_s, TWO, 0, 0, RP, 2, 0));
    t.push(v!("vcvtr u 0.5 RP", vcvtr_u, 0x3F00_0000, 0, 0, RP, 1, IXC));
    t.push(v!("vcvtr u -0.5 RP", vcvtr_u, 0xBF00_0000, 0, 0, RP, 0, IXC));
    t.push(v!("vcvtr u -0.5 RM", vcvtr_u, 0xBF00_0000, 0, 0, RM, 0, IOC));
    t.push(v!("vcvtr s 0.5 RN tie", vcvtr_s, 0x3F00_0000, 0, 0, RN, 0, IXC));
    t.push(v!("vcvtr s 1.5 RN tie", vcvtr_s, 0x3FC0_0000, 0, 0, RN, 2, IXC));
    t.push(v!("vcvtr 2^31 RM saturates", vcvtr_s, 0x4F00_0000, 0, 0, RM, 0x7FFF_FFFF, IOC));
    // ---- 32-bit integer -> float ----------------------------------------------------
    let from_s = FromInt { unsigned: false };
    let from_u = FromInt { unsigned: true };
    t.push(v!("16777217 RN", from_s, 0x0100_0001, 0, 0, RN, 0x4B80_0000, IXC));
    t.push(v!("16777217 RP", from_s, 0x0100_0001, 0, 0, RP, 0x4B80_0001, IXC));
    t.push(v!("16777217 RM", from_s, 0x0100_0001, 0, 0, RM, 0x4B80_0000, IXC));
    t.push(v!("16777217 RZ", from_s, 0x0100_0001, 0, 0, RZ, 0x4B80_0000, IXC));
    t.push(v!("-16777217 RN", from_s, 0xFEFF_FFFF, 0, 0, RN, 0xCB80_0000, IXC));
    t.push(v!("-16777217 RM", from_s, 0xFEFF_FFFF, 0, 0, RM, 0xCB80_0001, IXC));
    t.push(v!("-16777217 RP", from_s, 0xFEFF_FFFF, 0, 0, RP, 0xCB80_0000, IXC));
    t.push(v!("0xFFFFFFFF signed", from_s, 0xFFFF_FFFF, 0, 0, RN, 0xBF80_0000, 0));
    t.push(v!("0xFFFFFFFF unsigned RN", from_u, 0xFFFF_FFFF, 0, 0, RN, 0x4F80_0000, IXC));
    t.push(v!("0xFFFFFFFF unsigned RZ", from_u, 0xFFFF_FFFF, 0, 0, RZ, 0x4F7F_FFFF, IXC));
    t.push(v!("0 -> +0", from_s, 0, 0, 0, RM, 0, 0));
    t.push(v!("0x80000000 signed", from_s, 0x8000_0000, 0, 0, RN, 0xCF00_0000, 0));
    t.push(v!("0x7FFFFFFF RN", from_s, 0x7FFF_FFFF, 0, 0, RN, 0x4F00_0000, IXC));
    t.push(v!("0x7FFFFFFF RZ", from_s, 0x7FFF_FFFF, 0, 0, RZ, 0x4EFF_FFFF, IXC));
    t.push(v!("1 -> 1.0", from_u, 1, 0, 0, RN, ONE, 0));
    // ---- fixed point ---------------------------------------------------------------
    let to_s32 = |fbits| ToFixed { size: 32, fbits, unsigned: false };
    let to_u32 = |fbits| ToFixed { size: 32, fbits, unsigned: true };
    let to_s16 = |fbits| ToFixed { size: 16, fbits, unsigned: false };
    let to_u16 = |fbits| ToFixed { size: 16, fbits, unsigned: true };
    let from_s32 = |fbits| FromFixed { size: 32, fbits, unsigned: false };
    let from_u32 = |fbits| FromFixed { size: 32, fbits, unsigned: true };
    let from_s16 = |fbits| FromFixed { size: 16, fbits, unsigned: false };
    let from_u16 = |fbits| FromFixed { size: 16, fbits, unsigned: true };
    t.push(v!("1.5 -> s32.8", to_s32(8), 0x3FC0_0000, 0, 0, RN, 384, 0));
    t.push(v!("-1.5 -> s32.8", to_s32(8), 0xBFC0_0000, 0, 0, RN, 0xFFFF_FE80, 0));
    t.push(v!("0.001 -> s32.8", to_s32(8), 0x3A83_126F, 0, 0, RN, 0, IXC));
    t.push(v!("-0.001 -> s32.8", to_s32(8), 0xBA83_126F, 0, 0, RN, 0, IXC));
    t.push(v!("0.5 -> u32.32", to_u32(32), 0x3F00_0000, 0, 0, RN, 0x8000_0000, 0));
    t.push(v!("1.0 -> u32.32 saturates", to_u32(32), ONE, 0, 0, RN, 0xFFFF_FFFF, IOC));
    t.push(v!("-0.5 -> s32.32", to_s32(32), 0xBF00_0000, 0, 0, RN, 0x8000_0000, 0));
    t.push(v!("0.5 -> s32.32 saturates", to_s32(32), 0x3F00_0000, 0, 0, RN, 0x7FFF_FFFF, IOC));
    t.push(v!("200 -> s16.8 saturates", to_s16(8), 0x4348_0000, 0, 0, RN, 0x7FFF, IOC));
    t.push(v!("-200 -> s16.8 saturates", to_s16(8), 0xC348_0000, 0, 0, RN, 0xFFFF_8000, IOC));
    t.push(v!("127.99609375 -> s16.8 exact", to_s16(8), 0x42FF_FE00, 0, 0, RN, 0x7FFF, 0));
    t.push(v!("-1 -> s16.0", to_s16(0), 0xBF80_0000, 0, 0, RN, 0xFFFF_FFFF, 0));
    t.push(v!("-32769 -> s16.0 saturates", to_s16(0), 0xC700_0100, 0, 0, RN, 0xFFFF_8000, IOC));
    t.push(v!("4095.9375 -> u16.4 exact", to_u16(4), 0x457F_FF00, 0, 0, RN, 0xFFFF, 0));
    t.push(v!("4096 -> u16.4 saturates", to_u16(4), 0x4580_0000, 0, 0, RN, 0xFFFF, IOC));
    t.push(v!("-1 -> u16.0", to_u16(0), 0xBF80_0000, 0, 0, RN, 0, IOC));
    t.push(v!("nan -> s16.4", to_s16(4), QNAN, 0, 0, RN, 0, IOC));
    t.push(v!("inf -> u16.4", to_u16(4), INF, 0, 0, RN, 0xFFFF, IOC));
    t.push(v!("fixed to ignores RMode", to_s32(4), 0x3FD9_999A, 0, 0, RP, 27, IXC)); // 1.7*16 = 27.2 truncates
    t.push(v!("s16 0xFFFE .1 -> -1", from_s16(1), 0x1234_FFFE, 0, 0, RN, 0xBF80_0000, 0));
    t.push(v!("u16 0xFFFF .0", from_u16(0), 0xABCD_FFFF, 0, 0, RN, 0x477F_FF00, 0));
    t.push(v!("s16 0x8000 .0", from_s16(0), 0x8000, 0, 0, RN, 0xC700_0000, 0));
    t.push(v!("s32 0x80000000 .31", from_s32(31), 0x8000_0000, 0, 0, RN, 0xBF80_0000, 0));
    t.push(v!("u32 1 .32", from_u32(32), 1, 0, 0, RN, 0x2F80_0000, 0));
    t.push(v!("u32 0xFFFFFFFF .1 RN", from_u32(1), 0xFFFF_FFFF, 0, 0, RN, 0x4F00_0000, IXC));
    t.push(v!("u32 0xFFFFFFFF .1 RZ", from_u32(1), 0xFFFF_FFFF, 0, 0, RZ, 0x4EFF_FFFF, IXC));
    t.push(v!("s32 0 .5", from_s32(5), 0, 0, 0, RM, 0, 0));
    t.push(v!("s32 -3 .1 = -1.5", from_s32(1), 0xFFFF_FFFD, 0, 0, RN, 0xBFC0_0000, 0));
    // ---- half precision ------------------------------------------------------------
    t.push(v!("1.0 -> f16", ToHalf, ONE, 0, 0, RN, 0x3C00, 0));
    t.push(v!("-2.0 -> f16", ToHalf, 0xC000_0000, 0, 0, RN, 0xC000, 0));
    t.push(v!("65504 -> f16", ToHalf, 0x477F_E000, 0, 0, RN, 0x7BFF, 0));
    t.push(v!("65520 -> f16 RN overflows", ToHalf, 0x477F_F000, 0, 0, RN, 0x7C00, OFC | IXC));
    // Rounding toward zero yields the largest finite value: that is inexact, not an overflow.
    t.push(v!("65520 -> f16 RZ is not an overflow", ToHalf, 0x477F_F000, 0, 0, RZ, 0x7BFF, IXC));
    t.push(v!("65520 -> f16 RP", ToHalf, 0x477F_F000, 0, 0, RP, 0x7C00, OFC | IXC));
    t.push(v!("65520 -> f16 RM is not an overflow", ToHalf, 0x477F_F000, 0, 0, RM, 0x7BFF, IXC));
    t.push(v!("65536 -> f16 RZ overflows", ToHalf, 0x4780_0000, 0, 0, RZ, 0x7BFF, OFC | IXC));
    t.push(v!("65536 -> f16 RN overflows", ToHalf, 0x4780_0000, 0, 0, RN, 0x7C00, OFC | IXC));
    t.push(v!("-65536 -> f16 RM", ToHalf, 0xC780_0000, 0, 0, RM, 0xFC00, OFC | IXC));
    t.push(v!("-65536 -> f16 RP", ToHalf, 0xC780_0000, 0, 0, RP, 0xFBFF, OFC | IXC));
    t.push(v!("2^-24 -> f16 min subnormal", ToHalf, 0x3380_0000, 0, 0, RN, 0x0001, 0));
    t.push(v!("2^-25 -> f16 RN tie to even", ToHalf, 0x3300_0000, 0, 0, RN, 0x0000, UFC | IXC));
    t.push(v!("2^-25 -> f16 RP", ToHalf, 0x3300_0000, 0, 0, RP, 0x0001, UFC | IXC));
    t.push(v!("2^-14 -> f16 min normal", ToHalf, 0x3880_0000, 0, 0, RN, 0x0400, 0));
    t.push(v!("just below 2^-14 -> f16", ToHalf, 0x387F_FFFF, 0, 0, RN, 0x0400, UFC | IXC));
    t.push(v!("qnan -> f16", ToHalf, QNAN, 0, 0, RN, 0x7E00, 0));
    t.push(v!("qnan payload -> f16", ToHalf, 0x7FC0_2000, 0, 0, RN, 0x7E01, 0));
    t.push(v!("snan -> f16", ToHalf, SNAN, 0, 0, RN, 0x7E00, IOC));
    t.push(v!("qnan -> f16 DN", ToHalf, 0x7FC0_2000, 0, 0, DN, 0x7E00, 0));
    t.push(v!("-inf -> f16", ToHalf, NINF, 0, 0, RN, 0xFC00, 0));
    t.push(v!("FZ denormal -> f16", ToHalf, 1, 0, 0, FZ, 0, IDC));
    t.push(v!("1.0 -> ahp", ToHalf, ONE, 0, 0, AHP, 0x3C00, 0));
    t.push(v!("65536 -> ahp exact", ToHalf, 0x4780_0000, 0, 0, AHP, 0x7C00, 0));
    t.push(v!("131008 -> ahp max", ToHalf, 0x47FF_E000, 0, 0, AHP, 0x7FFF, 0));
    t.push(v!("1e6 -> ahp saturates", ToHalf, 0x4974_2400, 0, 0, AHP, 0x7FFF, IOC));
    t.push(v!("-1e6 -> ahp saturates", ToHalf, 0xC974_2400, 0, 0, AHP, 0xFFFF, IOC));
    t.push(v!("nan -> ahp", ToHalf, QNAN, 0, 0, AHP, 0, IOC));
    t.push(v!("inf -> ahp", ToHalf, INF, 0, 0, AHP, 0x7FFF, IOC));
    t.push(v!("-inf -> ahp", ToHalf, NINF, 0, 0, AHP, 0xFFFF, IOC));
    t.push(v!("f16 1.0", FromHalf, 0x3C00, 0, 0, RN, ONE, 0));
    t.push(v!("f16 -2.0", FromHalf, 0xC000, 0, 0, RN, 0xC000_0000, 0));
    t.push(v!("f16 min subnormal", FromHalf, 0x0001, 0, 0, RN, 0x3380_0000, 0));
    t.push(v!("f16 max subnormal", FromHalf, 0x03FF, 0, 0, RN, 0x387F_C000, 0));
    t.push(v!("f16 subnormal with FZ stays exact", FromHalf, 0x0001, 0, 0, FZ, 0x3380_0000, 0));
    t.push(v!("f16 -0", FromHalf, 0x8000, 0, 0, RN, NZERO, 0));
    t.push(v!("f16 inf", FromHalf, 0x7C00, 0, 0, RN, INF, 0));
    t.push(v!("f16 snan", FromHalf, 0x7C01, 0, 0, RN, 0x7FC0_2000, IOC));
    t.push(v!("f16 qnan", FromHalf, 0x7E00, 0, 0, RN, QNAN, 0));
    t.push(v!("f16 qnan payload", FromHalf, 0x7E55, 0, 0, RN, 0x7FCA_A000, 0));
    t.push(v!("f16 snan DN", FromHalf, 0x7C01, 0, 0, DN, QNAN, IOC));
    t.push(v!("f16 ahp 0x7C00 = 65536", FromHalf, 0x7C00, 0, 0, AHP, 0x4780_0000, 0));
    t.push(v!("f16 ahp 0x7FFF = 131008", FromHalf, 0x7FFF, 0, 0, AHP, 0x47FF_E000, 0));
    t.push(v!("f16 ahp 0xFFFF", FromHalf, 0xFFFF, 0, 0, AHP, 0xC7FF_E000, 0));
    t
}

fn run(v: &V, fast: bool) -> (u32, u32) {
    let mut f = v.mode;
    let r = match v.op {
        Op::Add => if fast { ieee::add(v.a, v.b, &mut f) } else { soft::add(v.a, v.b, &mut f) },
        Op::Sub => if fast { ieee::sub(v.a, v.b, &mut f) } else { soft::sub(v.a, v.b, &mut f) },
        Op::Mul => if fast { ieee::mul(v.a, v.b, &mut f) } else { soft::mul(v.a, v.b, &mut f) },
        Op::Div => if fast { ieee::div(v.a, v.b, &mut f) } else { soft::div(v.a, v.b, &mut f) },
        Op::Sqrt => if fast { ieee::sqrt(v.a, &mut f) } else { soft::sqrt(v.a, &mut f) },
        Op::Fma => if fast { ieee::fma(v.a, v.b, v.c, &mut f) } else { soft::fma(v.a, v.b, v.c, &mut f) },
        Op::Cmp { e } => if fast { ieee::compare(v.a, v.b, e, &mut f) } else { soft::compare(v.a, v.b, e, &mut f) },
        Op::CmpZero { e } => if fast { ieee::compare(v.a, 0, e, &mut f) } else { soft::compare(v.a, 0, e, &mut f) },
        Op::ToInt { unsigned, round_zero } => {
            if fast {
                ieee::to_int(v.a, unsigned, round_zero, &mut f)
            } else {
                let rm = if round_zero { RMODE_RZ } else { rmode(f) };
                soft::to_fixed(v.a, 32, 0, unsigned, rm, &mut f)
            }
        }
        Op::FromInt { unsigned } => {
            if fast { ieee::from_int(v.a, unsigned, &mut f) } else { soft::from_fixed(v.a, 32, 0, unsigned, &mut f) }
        }
        Op::ToFixed { size, fbits, unsigned } => {
            if fast { ieee::to_fixed(v.a, size, fbits, unsigned, &mut f) } else { soft::to_fixed(v.a, size, fbits, unsigned, RMODE_RZ, &mut f) }
        }
        Op::FromFixed { size, fbits, unsigned } => {
            if fast { ieee::from_fixed(v.a, size, fbits, unsigned, &mut f) } else { soft::from_fixed(v.a, size, fbits, unsigned, &mut f) }
        }
        Op::ToHalf => if fast { ieee::f32_to_f16(v.a, &mut f) as u32 } else { soft::f32_to_f16(v.a, &mut f) as u32 },
        Op::FromHalf => if fast { ieee::f16_to_f32(v.a as u16, &mut f) } else { soft::f16_to_f32(v.a as u16, &mut f) },
    };
    (r, f & FLAGS_MASK)
}

/// Runs a vector through the exact software core (`fast = false`) or the
/// fast-path layer (`fast = true`).
pub fn run_impl(v: &V, fast: bool) -> (u32, u32) {
    run(v, fast)
}

/// Runs a vector on the host's hardware FPU (aarch64). `None` for operations
/// the hardware cannot express directly (16-bit fixed point).
#[cfg(target_arch = "aarch64")]
pub fn run_hw(v: &V) -> Option<(u32, u32)> {
    use super::hw;
    let m = v.mode;
    let o = match v.op {
        Op::Add => hw::fadd(v.a, v.b, m),
        Op::Sub => hw::fsub(v.a, v.b, m),
        Op::Mul => hw::fmul(v.a, v.b, m),
        Op::Div => hw::fdiv(v.a, v.b, m),
        Op::Sqrt => hw::fsqrt(v.a, m),
        Op::Fma => hw::fmadd(v.b, v.c, v.a, m),
        Op::Cmp { e } => {
            let (nzcv, fpsr) = if e { hw::fcmpe(v.a, v.b, m) } else { hw::fcmp(v.a, v.b, m) };
            return Some((nzcv, fpsr & FLAGS_MASK));
        }
        Op::CmpZero { e } => {
            let (nzcv, fpsr) = if e { hw::fcmpe_zero(v.a, m) } else { hw::fcmp_zero(v.a, m) };
            return Some((nzcv, fpsr & FLAGS_MASK));
        }
        Op::ToInt { unsigned, round_zero } => {
            let rm = if round_zero { RMODE_RZ } else { rmode(m) };
            match (unsigned, rm) {
                (false, RMODE_RN) => hw::fcvtns(v.a, m),
                (false, RMODE_RP) => hw::fcvtps(v.a, m),
                (false, RMODE_RM) => hw::fcvtms(v.a, m),
                (false, _) => hw::fcvtzs(v.a, m),
                (true, RMODE_RN) => hw::fcvtnu(v.a, m),
                (true, RMODE_RP) => hw::fcvtpu(v.a, m),
                (true, RMODE_RM) => hw::fcvtmu(v.a, m),
                (true, _) => hw::fcvtzu(v.a, m),
            }
        }
        Op::FromInt { unsigned } => if unsigned { hw::ucvtf(v.a, m) } else { hw::scvtf(v.a, m) },
        Op::ToFixed { size: 32, fbits, unsigned } => {
            if fbits == 0 {
                if unsigned { hw::fcvtzu(v.a, m) } else { hw::fcvtzs(v.a, m) }
            } else {
                hw::to_fixed32(v.a, fbits, unsigned, m)
            }
        }
        Op::FromFixed { size: 32, fbits, unsigned } => {
            if fbits == 0 {
                if unsigned { hw::ucvtf(v.a, m) } else { hw::scvtf(v.a, m) }
            } else {
                hw::from_fixed32(v.a, fbits, unsigned, m)
            }
        }
        Op::FromFixed { size: 16, fbits, unsigned } => {
            let x = if unsigned { v.a & 0xFFFF } else { v.a as u16 as i16 as i32 as u32 };
            if fbits == 0 {
                if unsigned { hw::ucvtf(x, m) } else { hw::scvtf(x, m) }
            } else {
                hw::from_fixed32(x, fbits, unsigned, m)
            }
        }
        Op::ToFixed { .. } | Op::FromFixed { .. } => return None,
        Op::ToHalf => hw::fcvt_f32_f16(v.a, m),
        Op::FromHalf => hw::fcvt_f16_f32(v.a as u16, m),
    };
    Some((o.bits, o.fpsr & FLAGS_MASK))
}
