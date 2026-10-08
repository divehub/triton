//! Mini assembler written from the Arm ARM encoding diagrams (A32 layout with
//! cond = 1110; the Thumb halfwords are `w >> 16` and `w & 0xFFFF`). It is
//! deliberately independent of the decoder's tables.
#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Mini assembler (A32 diagrams with cond = 1110; Thumb hw1 = w >> 16)
// ---------------------------------------------------------------------------

pub type Enc = (u16, u16);

pub fn t(w: u32) -> Enc {
    ((w >> 16) as u16, w as u16)
}

/// Single register field helpers: Vx = r >> 1, low bit = r & 1.
pub fn s_d(r: u32) -> u32 {
    ((r >> 1) << 12) | ((r & 1) << 22)
}
pub fn s_n(r: u32) -> u32 {
    ((r >> 1) << 16) | ((r & 1) << 7)
}
pub fn s_m(r: u32) -> u32 {
    (r >> 1) | ((r & 1) << 5)
}

pub fn dp3(base: u32, d: u32, n: u32, m: u32) -> Enc {
    t(base | s_d(d) | s_n(n) | s_m(m))
}
pub fn vadd(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE30_0A00, d, n, m) }
pub fn vsub(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE30_0A40, d, n, m) }
pub fn vmul(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE20_0A00, d, n, m) }
pub fn vnmul(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE20_0A40, d, n, m) }
pub fn vdiv(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE80_0A00, d, n, m) }
pub fn vmla(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE00_0A00, d, n, m) }
pub fn vmls(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE00_0A40, d, n, m) }
pub fn vnmls(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE10_0A00, d, n, m) }
pub fn vnmla(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE10_0A40, d, n, m) }
pub fn vfnms(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE90_0A00, d, n, m) }
pub fn vfnma(d: u32, n: u32, m: u32) -> Enc { dp3(0xEE90_0A40, d, n, m) }
pub fn vfma(d: u32, n: u32, m: u32) -> Enc { dp3(0xEEA0_0A00, d, n, m) }
pub fn vfms(d: u32, n: u32, m: u32) -> Enc { dp3(0xEEA0_0A40, d, n, m) }

pub fn dp2(base: u32, d: u32, m: u32) -> Enc {
    t(base | s_d(d) | s_m(m))
}
pub fn vmov_reg(d: u32, m: u32) -> Enc { dp2(0xEEB0_0A40, d, m) }
pub fn vabs(d: u32, m: u32) -> Enc { dp2(0xEEB0_0AC0, d, m) }
pub fn vneg(d: u32, m: u32) -> Enc { dp2(0xEEB1_0A40, d, m) }
pub fn vsqrt(d: u32, m: u32) -> Enc { dp2(0xEEB1_0AC0, d, m) }
pub fn vcmp(d: u32, m: u32, e: bool) -> Enc { dp2(0xEEB4_0A40 | ((e as u32) << 7), d, m) }
pub fn vcmp0(d: u32, e: bool) -> Enc { dp2(0xEEB5_0A40 | ((e as u32) << 7), d, 0) }
pub fn vcvt_f32_s32(d: u32, m: u32) -> Enc { dp2(0xEEB8_0AC0, d, m) }
pub fn vcvt_f32_u32(d: u32, m: u32) -> Enc { dp2(0xEEB8_0A40, d, m) }
pub fn vcvt_s32_f32(d: u32, m: u32) -> Enc { dp2(0xEEBD_0AC0, d, m) }
pub fn vcvt_u32_f32(d: u32, m: u32) -> Enc { dp2(0xEEBC_0AC0, d, m) }
pub fn vcvtr_s32_f32(d: u32, m: u32) -> Enc { dp2(0xEEBD_0A40, d, m) }
pub fn vcvtr_u32_f32(d: u32, m: u32) -> Enc { dp2(0xEEBC_0A40, d, m) }
pub fn vcvtb_f32_f16(d: u32, m: u32) -> Enc { dp2(0xEEB2_0A40, d, m) }
pub fn vcvtt_f32_f16(d: u32, m: u32) -> Enc { dp2(0xEEB2_0AC0, d, m) }
pub fn vcvtb_f16_f32(d: u32, m: u32) -> Enc { dp2(0xEEB3_0A40, d, m) }
pub fn vcvtt_f16_f32(d: u32, m: u32) -> Enc { dp2(0xEEB3_0AC0, d, m) }

pub fn vmov_imm(d: u32, imm8: u32) -> Enc {
    t(0xEEB0_0A00 | s_d(d) | ((imm8 >> 4) << 16) | (imm8 & 0xF))
}

/// Fixed-point conversions: `to_fixed` selects float -> fixed.
pub fn vcvt_fixed(d: u32, to_fixed: bool, unsigned: bool, size32: bool, fbits: u32) -> Enc {
    let size = if size32 { 32 } else { 16 };
    let imm = size - fbits;
    let base = 0xEEBA_0A40
        | ((to_fixed as u32) << 18)
        | ((unsigned as u32) << 16)
        | ((size32 as u32) << 7)
        | ((imm & 1) << 5)
        | (imm >> 1);
    t(base | s_d(d)) // Sd occupies Vd:D only
}

pub fn vmov_to_s(sn: u32, rt: u32) -> Enc { t(0xEE00_0A10 | s_n(sn) | (rt << 12)) }
pub fn vmov_from_s(rt: u32, sn: u32) -> Enc { t(0xEE10_0A10 | s_n(sn) | (rt << 12)) }
pub fn vmov2_to_s(sm: u32, rt: u32, rt2: u32) -> Enc {
    t(0xEC40_0A10 | (rt2 << 16) | (rt << 12) | s_m(sm))
}
pub fn vmov2_from_s(rt: u32, rt2: u32, sm: u32) -> Enc {
    t(0xEC50_0A10 | (rt2 << 16) | (rt << 12) | s_m(sm))
}
pub fn vmov2_to_d(dm: u32, rt: u32, rt2: u32) -> Enc {
    t(0xEC40_0B10 | (rt2 << 16) | (rt << 12) | ((dm >> 4) << 5) | (dm & 0xF))
}
pub fn vmov2_from_d(rt: u32, rt2: u32, dm: u32) -> Enc {
    t(0xEC50_0B10 | (rt2 << 16) | (rt << 12) | ((dm >> 4) << 5) | (dm & 0xF))
}
pub fn vmov32_to_scalar(dd: u32, x: u32, rt: u32) -> Enc {
    t(0xEE00_0B10 | (x << 21) | ((dd & 0xF) << 16) | ((dd >> 4) << 7) | (rt << 12))
}
pub fn vmov32_from_scalar(rt: u32, dn: u32, x: u32) -> Enc {
    t(0xEE10_0B10 | (x << 21) | ((dn & 0xF) << 16) | ((dn >> 4) << 7) | (rt << 12))
}
pub fn vmrs(rt: u32) -> Enc { t(0xEEF1_0A10 | (rt << 12)) }
pub fn vmsr(rt: u32) -> Enc { t(0xEEE1_0A10 | (rt << 12)) }

pub fn vldr(d: u32, rn: u32, off: i32) -> Enc {
    let u = (off >= 0) as u32;
    t(0xED10_0A00 | (u << 23) | s_d(d) | (rn << 16) | (off.unsigned_abs() >> 2))
}
pub fn vstr(d: u32, rn: u32, off: i32) -> Enc {
    let u = (off >= 0) as u32;
    t(0xED00_0A00 | (u << 23) | s_d(d) | (rn << 16) | (off.unsigned_abs() >> 2))
}
pub fn vldr_d(dd: u32, rn: u32, off: i32) -> Enc {
    let u = (off >= 0) as u32;
    t(0xED10_0B00 | (u << 23) | ((dd >> 4) << 22) | ((dd & 0xF) << 12) | (rn << 16) | (off.unsigned_abs() >> 2))
}
pub fn vstr_d(dd: u32, rn: u32, off: i32) -> Enc {
    let u = (off >= 0) as u32;
    t(0xED00_0B00 | (u << 23) | ((dd >> 4) << 22) | ((dd & 0xF) << 12) | (rn << 16) | (off.unsigned_abs() >> 2))
}
/// VLDM/VSTM on single registers: `mode` is IA (false) or DB (true).
pub fn vldm(s: u32, count: u32, rn: u32, db: bool, wback: bool) -> Enc {
    let (p, u) = if db { (1, 0) } else { (0, 1) };
    t(0xEC10_0A00 | (p << 24) | (u << 23) | ((wback as u32) << 21) | s_d(s) | (rn << 16) | count)
}
pub fn vstm(s: u32, count: u32, rn: u32, db: bool, wback: bool) -> Enc {
    let (p, u) = if db { (1, 0) } else { (0, 1) };
    t(0xEC00_0A00 | (p << 24) | (u << 23) | ((wback as u32) << 21) | s_d(s) | (rn << 16) | count)
}
pub fn vldm_d(dd: u32, count: u32, rn: u32, db: bool, wback: bool) -> Enc {
    let (p, u) = if db { (1, 0) } else { (0, 1) };
    t(0xEC10_0B00
        | (p << 24)
        | (u << 23)
        | ((wback as u32) << 21)
        | ((dd >> 4) << 22)
        | ((dd & 0xF) << 12)
        | (rn << 16)
        | (count * 2))
}
pub fn vstm_d(dd: u32, count: u32, rn: u32, db: bool, wback: bool) -> Enc {
    let (p, u) = if db { (1, 0) } else { (0, 1) };
    t(0xEC00_0B00
        | (p << 24)
        | (u << 23)
        | ((wback as u32) << 21)
        | ((dd >> 4) << 22)
        | ((dd & 0xF) << 12)
        | (rn << 16)
        | (count * 2))
}
pub fn vpush_d(dd: u32, count: u32) -> Enc { vstm_d(dd, count, 13, true, true) }
pub fn vpop_d(dd: u32, count: u32) -> Enc { vldm_d(dd, count, 13, false, true) }
