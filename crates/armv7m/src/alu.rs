//! Pure integer arithmetic helpers shared by the decoder and the executor
//! (ARMv7-M ARM pseudocode: AddWithCarry, Shift_C, SignedSatQ, parallel
//! add/subtract, ...). Everything here is side-effect free and unit tested.

/// Shift types as encoded in Thumb-2 data-processing instructions.
pub const SHIFT_LSL: u8 = 0;
pub const SHIFT_LSR: u8 = 1;
pub const SHIFT_ASR: u8 = 2;
pub const SHIFT_ROR: u8 = 3;
/// Pseudo type used for `RRX` (rotate right with extend, amount 1).
pub const SHIFT_RRX: u8 = 4;

/// `AddWithCarry(x, y, carry_in)` returning `(result, carry_out, overflow)`.
#[inline(always)]
pub fn add_with_carry(x: u32, y: u32, carry_in: bool) -> (u32, bool, bool) {
    let (s1, c1) = x.overflowing_add(y);
    let (s2, c2) = s1.overflowing_add(carry_in as u32);
    let overflow = ((x ^ s2) & (y ^ s2)) >> 31 != 0;
    (s2, c1 | c2, overflow)
}

/// `LSL_C` for a shift amount `n > 0`.
#[inline(always)]
pub fn lsl_c(x: u32, n: u32) -> (u32, bool) {
    debug_assert!(n > 0);
    if n < 32 {
        (x << n, (x >> (32 - n)) & 1 != 0)
    } else if n == 32 {
        (0, x & 1 != 0)
    } else {
        (0, false)
    }
}

/// `LSR_C` for a shift amount `n > 0`.
#[inline(always)]
pub fn lsr_c(x: u32, n: u32) -> (u32, bool) {
    debug_assert!(n > 0);
    if n < 32 {
        (x >> n, (x >> (n - 1)) & 1 != 0)
    } else if n == 32 {
        (0, x >> 31 != 0)
    } else {
        (0, false)
    }
}

/// `ASR_C` for a shift amount `n > 0`.
#[inline(always)]
pub fn asr_c(x: u32, n: u32) -> (u32, bool) {
    debug_assert!(n > 0);
    if n < 32 {
        (((x as i32) >> n) as u32, (x >> (n - 1)) & 1 != 0)
    } else {
        let s = ((x as i32) >> 31) as u32;
        (s, s & 1 != 0)
    }
}

/// `ROR_C` for a shift amount `n > 0` (any amount; the rotation is modulo 32).
#[inline(always)]
pub fn ror_c(x: u32, n: u32) -> (u32, bool) {
    debug_assert!(n > 0);
    let r = x.rotate_right(n & 31);
    (r, r >> 31 != 0)
}

/// `RRX_C`.
#[inline(always)]
pub fn rrx_c(x: u32, carry_in: bool) -> (u32, bool) {
    (((carry_in as u32) << 31) | (x >> 1), x & 1 != 0)
}

/// `Shift_C(value, type, amount, carry_in)` where `amount` may be zero (the
/// value and carry are then returned unchanged, as for register-controlled
/// shifts by zero). `ty` is one of the `SHIFT_*` constants.
#[inline(always)]
pub fn shift_c(value: u32, ty: u8, amount: u32, carry_in: bool) -> (u32, bool) {
    if ty == SHIFT_RRX {
        return rrx_c(value, carry_in);
    }
    if amount == 0 {
        return (value, carry_in);
    }
    match ty {
        SHIFT_LSL => lsl_c(value, amount),
        SHIFT_LSR => lsr_c(value, amount),
        SHIFT_ASR => asr_c(value, amount),
        _ => ror_c(value, amount),
    }
}

/// `DecodeImmShift`: converts the 2-bit type and 5-bit immediate of a
/// shifted-register operand into `(shift type, amount)`; type `ROR` with a zero
/// immediate becomes `RRX`.
#[inline(always)]
pub fn decode_imm_shift(ty: u32, imm5: u32) -> (u8, u32) {
    match ty & 3 {
        0 => (SHIFT_LSL, imm5),
        1 => (SHIFT_LSR, if imm5 == 0 { 32 } else { imm5 }),
        2 => (SHIFT_ASR, if imm5 == 0 { 32 } else { imm5 }),
        _ => {
            if imm5 == 0 {
                (SHIFT_RRX, 1)
            } else {
                (SHIFT_ROR, imm5)
            }
        }
    }
}

/// `ThumbExpandImm_C(imm12, carry_in)`: returns `(imm32, carry_out)`. The
/// carry-out equals `carry_in` unless the immediate is a rotated 8-bit value.
pub fn thumb_expand_imm_c(imm12: u32, carry_in: bool) -> (u32, bool) {
    if imm12 & 0xC00 == 0 {
        let imm8 = imm12 & 0xFF;
        let v = match (imm12 >> 8) & 3 {
            0 => imm8,
            1 => (imm8 << 16) | imm8,
            2 => (imm8 << 24) | (imm8 << 8),
            _ => imm8 * 0x0101_0101,
        };
        (v, carry_in)
    } else {
        let unrotated = 0x80 | (imm12 & 0x7F);
        let rot = (imm12 >> 7) & 31;
        let v = unrotated.rotate_right(rot);
        (v, v >> 31 != 0)
    }
}

/// True when `imm12` expands through the rotated form (carry-out defined by the value).
#[inline]
pub fn thumb_imm_is_rotated(imm12: u32) -> bool {
    imm12 & 0xC00 != 0
}

/// `SignedSatQ(i, n)` for `1 <= n <= 32`: `(saturated value, saturated?)`.
#[inline]
pub fn signed_sat_q(i: i64, n: u32) -> (i32, bool) {
    debug_assert!((1..=32).contains(&n));
    let max = (1i64 << (n - 1)) - 1;
    let min = -(1i64 << (n - 1));
    if i > max {
        (max as i32, true)
    } else if i < min {
        (min as i32, true)
    } else {
        (i as i32, false)
    }
}

/// `UnsignedSatQ(i, n)` for `0 <= n <= 31`: `(saturated value, saturated?)`.
#[inline]
pub fn unsigned_sat_q(i: i64, n: u32) -> (u32, bool) {
    debug_assert!(n <= 32);
    let max = if n >= 32 { u32::MAX as i64 } else { (1i64 << n) - 1 };
    if i > max {
        (max as u32, true)
    } else if i < 0 {
        (0, true)
    } else {
        (i as u32, false)
    }
}

/// Saturating signed 32-bit add/sub used by QADD/QSUB/QDADD/QDSUB:
/// `(result, saturated)`.
#[inline]
pub fn sat_add_i32(a: i32, b: i32) -> (i32, bool) {
    signed_sat_q(a as i64 + b as i64, 32)
}

#[inline]
pub fn sat_sub_i32(a: i32, b: i32) -> (i32, bool) {
    signed_sat_q(a as i64 - b as i64, 32)
}

/// Sign-extends the low `bits` bits of `v`.
#[inline(always)]
pub fn sign_extend(v: u32, bits: u32) -> u32 {
    let sh = 32 - bits;
    (((v << sh) as i32) >> sh) as u32
}

// ---------------------------------------------------------------------------
// Parallel add/subtract (A7.7: SADD16 ... UHSUB8). Operation index layout:
//   idx = prefix * 6 + op, prefix: 0 S, 1 Q, 2 SH, 3 U, 4 UQ, 5 UH
//   op: 0 ADD16, 1 ASX, 2 SAX, 3 SUB16, 4 ADD8, 5 SUB8
// ---------------------------------------------------------------------------

pub const PAR_S: u8 = 0;
pub const PAR_Q: u8 = 1;
pub const PAR_SH: u8 = 2;
pub const PAR_U: u8 = 3;
pub const PAR_UQ: u8 = 4;
pub const PAR_UH: u8 = 5;

pub const PAR_ADD16: u8 = 0;
pub const PAR_ASX: u8 = 1;
pub const PAR_SAX: u8 = 2;
pub const PAR_SUB16: u8 = 3;
pub const PAR_ADD8: u8 = 4;
pub const PAR_SUB8: u8 = 5;

#[inline]
fn finish16(prefix: u8, v: i32, ge_set: &mut u32, lane: u32, unsigned: bool, is_add: bool) -> u16 {
    // `v` is the exact (17-bit signed/unsigned) sum or difference.
    match prefix {
        PAR_S => {
            if v >= 0 {
                *ge_set |= 3 << (2 * lane);
            }
            v as u16
        }
        PAR_U => {
            let ge = if is_add { v >= 0x1_0000 } else { v >= 0 };
            if ge {
                *ge_set |= 3 << (2 * lane);
            }
            v as u16
        }
        PAR_Q => signed_sat_q(v as i64, 16).0 as u16,
        PAR_UQ => {
            let _ = unsigned;
            unsigned_sat_q(v as i64, 16).0 as u16
        }
        PAR_SH => (v >> 1) as u16,
        _ => (v >> 1) as u16, // UH: v is non-negative or an unsigned difference (arithmetic shift of the exact value)
    }
}

#[inline]
fn finish8(prefix: u8, v: i32, ge_set: &mut u32, lane: u32, is_add: bool) -> u8 {
    match prefix {
        PAR_S => {
            if v >= 0 {
                *ge_set |= 1 << lane;
            }
            v as u8
        }
        PAR_U => {
            let ge = if is_add { v >= 0x100 } else { v >= 0 };
            if ge {
                *ge_set |= 1 << lane;
            }
            v as u8
        }
        PAR_Q => signed_sat_q(v as i64, 8).0 as u8,
        PAR_UQ => unsigned_sat_q(v as i64, 8).0 as u8,
        PAR_SH => (v >> 1) as u8,
        _ => (v >> 1) as u8,
    }
}

/// Executes one parallel add/subtract instruction. Returns `(result, new GE
/// bits)` where GE is `Some` only for the S/U (GE-setting) variants.
pub fn parallel_addsub(idx: u8, a: u32, b: u32) -> (u32, Option<u32>) {
    let prefix = idx / 6;
    let op = idx % 6;
    let unsigned = matches!(prefix, PAR_U | PAR_UQ | PAR_UH);
    let sets_ge = matches!(prefix, PAR_S | PAR_U);
    let mut ge = 0u32;
    let h = |x: u32, hi: bool| -> i32 {
        let v = if hi { x >> 16 } else { x & 0xFFFF };
        if unsigned {
            v as i32
        } else {
            v as u16 as i16 as i32
        }
    };
    let by = |x: u32, i: u32| -> i32 {
        let v = (x >> (8 * i)) & 0xFF;
        if unsigned {
            v as i32
        } else {
            v as u8 as i8 as i32
        }
    };
    let result = match op {
        PAR_ADD16 | PAR_SUB16 => {
            let is_add = op == PAR_ADD16;
            let (v0, v1) = if is_add {
                (h(a, false) + h(b, false), h(a, true) + h(b, true))
            } else {
                (h(a, false) - h(b, false), h(a, true) - h(b, true))
            };
            let r0 = finish16(prefix, v0, &mut ge, 0, unsigned, is_add) as u32;
            let r1 = finish16(prefix, v1, &mut ge, 1, unsigned, is_add) as u32;
            r0 | (r1 << 16)
        }
        PAR_ASX => {
            // Rd[15:0] = Rn[15:0] - Rm[31:16]; Rd[31:16] = Rn[31:16] + Rm[15:0]
            let v0 = h(a, false) - h(b, true);
            let v1 = h(a, true) + h(b, false);
            let r0 = finish16(prefix, v0, &mut ge, 0, unsigned, false) as u32;
            let r1 = finish16(prefix, v1, &mut ge, 1, unsigned, true) as u32;
            r0 | (r1 << 16)
        }
        PAR_SAX => {
            // Rd[15:0] = Rn[15:0] + Rm[31:16]; Rd[31:16] = Rn[31:16] - Rm[15:0]
            let v0 = h(a, false) + h(b, true);
            let v1 = h(a, true) - h(b, false);
            let r0 = finish16(prefix, v0, &mut ge, 0, unsigned, true) as u32;
            let r1 = finish16(prefix, v1, &mut ge, 1, unsigned, false) as u32;
            r0 | (r1 << 16)
        }
        PAR_ADD8 | PAR_SUB8 => {
            let is_add = op == PAR_ADD8;
            let mut r = 0u32;
            for i in 0..4 {
                let v = if is_add { by(a, i) + by(b, i) } else { by(a, i) - by(b, i) };
                r |= (finish8(prefix, v, &mut ge, i, is_add) as u32) << (8 * i);
            }
            r
        }
        _ => 0,
    };
    (result, if sets_ge { Some(ge) } else { None })
}

/// `USAD8`: sum of absolute byte differences.
#[inline]
pub fn usad8(a: u32, b: u32) -> u32 {
    let mut s = 0u32;
    for i in 0..4 {
        let x = (a >> (8 * i)) & 0xFF;
        let y = (b >> (8 * i)) & 0xFF;
        s += x.abs_diff(y);
    }
    s
}

/// `SEL`: byte-wise select using the GE flags (`ge` bits 3:0).
#[inline]
pub fn sel(ge: u32, a: u32, b: u32) -> u32 {
    let mut mask = 0u32;
    for i in 0..4 {
        if ge & (1 << i) != 0 {
            mask |= 0xFF << (8 * i);
        }
    }
    (a & mask) | (b & !mask)
}

/// Rotates and extends according to `SXTB/UXTB/SXTH/UXTH` (without add).
#[inline]
pub fn extend(kind: ExtKind, v: u32, rot: u32) -> u32 {
    let r = v.rotate_right(rot & 31);
    match kind {
        ExtKind::Sxtb => r as u8 as i8 as i32 as u32,
        ExtKind::Uxtb => r & 0xFF,
        ExtKind::Sxth => r as u16 as i16 as i32 as u32,
        ExtKind::Uxth => r & 0xFFFF,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtKind {
    Sxtb,
    Uxtb,
    Sxth,
    Uxth,
}

/// `SXTB16`/`UXTB16` on an already rotated value.
#[inline]
pub fn extend_b16(signed: bool, rotated: u32) -> u32 {
    if signed {
        let lo = (rotated as u8 as i8 as i16) as u16 as u32;
        let hi = ((rotated >> 16) as u8 as i8 as i16) as u16 as u32;
        lo | (hi << 16)
    } else {
        (rotated & 0xFF) | ((rotated >> 16) & 0xFF) << 16
    }
}

/// Bit reversal (`RBIT`).
#[inline]
pub fn rbit(x: u32) -> u32 {
    x.reverse_bits()
}

/// `REV16`: byte swap within each halfword.
#[inline]
pub fn rev16(x: u32) -> u32 {
    ((x & 0x00FF_00FF) << 8) | ((x >> 8) & 0x00FF_00FF)
}

/// `REVSH`: byte swap of the low halfword, sign extended.
#[inline]
pub fn revsh(x: u32) -> u32 {
    (((x & 0xFF) << 8) | ((x >> 8) & 0xFF)) as u16 as i16 as i32 as u32
}

/// `SDIV` semantics: division by zero yields zero (no trap handled here),
/// `INT_MIN / -1` yields `INT_MIN`.
#[inline]
pub fn sdiv(a: u32, b: u32) -> u32 {
    if b == 0 {
        0
    } else {
        (a as i32).wrapping_div(b as i32) as u32
    }
}

#[inline]
pub fn udiv(a: u32, b: u32) -> u32 {
    if b == 0 {
        0
    } else {
        a / b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_flags() {
        assert_eq!(add_with_carry(0xFFFF_FFFF, 1, false), (0, true, false));
        assert_eq!(add_with_carry(0x7FFF_FFFF, 1, false), (0x8000_0000, false, true));
        assert_eq!(add_with_carry(0x8000_0000, 0x8000_0000, false), (0, true, true));
        assert_eq!(add_with_carry(0xFFFF_FFFF, 0, true), (0, true, false));
        assert_eq!(add_with_carry(0x7FFF_FFFF, 0, true), (0x8000_0000, false, true));
        // Subtraction x - y == x + !y + 1.
        assert_eq!(add_with_carry(5, !3, true), (2, true, false));
        assert_eq!(add_with_carry(3, !5, true), (0xFFFF_FFFE, false, false));
        assert_eq!(add_with_carry(0x8000_0000, !1, true), (0x7FFF_FFFF, true, true));
    }

    #[test]
    fn shifts() {
        assert_eq!(lsl_c(0x8000_0001, 1), (2, true));
        assert_eq!(lsl_c(1, 31), (0x8000_0000, false));
        assert_eq!(lsl_c(1, 32), (0, true));
        assert_eq!(lsl_c(3, 33), (0, false));
        assert_eq!(lsr_c(0x8000_0001, 1), (0x4000_0000, true));
        assert_eq!(lsr_c(0x8000_0000, 32), (0, true));
        assert_eq!(lsr_c(0xFFFF_FFFF, 33), (0, false));
        assert_eq!(asr_c(0x8000_0000, 1), (0xC000_0000, false));
        assert_eq!(asr_c(0x8000_0001, 31), (0xFFFF_FFFF, false));
        assert_eq!(asr_c(0x8000_0001, 32), (0xFFFF_FFFF, true));
        assert_eq!(asr_c(0x7FFF_FFFF, 40), (0, false));
        assert_eq!(ror_c(0x0000_0001, 1), (0x8000_0000, true));
        assert_eq!(ror_c(0x0000_0003, 32), (3, false)); // rotation modulo 32, carry = bit 31
        assert_eq!(ror_c(0x8000_0000, 32), (0x8000_0000, true));
        assert_eq!(ror_c(0x0000_0010, 36), (1, false));
        assert_eq!(rrx_c(0x0000_0003, true), (0x8000_0001, true));
        assert_eq!(rrx_c(0x0000_0002, false), (1, false));
        // Shift by zero keeps the carry.
        assert_eq!(shift_c(0x1234, SHIFT_LSL, 0, true), (0x1234, true));
        assert_eq!(shift_c(0x1234, SHIFT_ROR, 0, false), (0x1234, false));
    }

    #[test]
    fn imm_shift_decode() {
        assert_eq!(decode_imm_shift(0, 0), (SHIFT_LSL, 0));
        assert_eq!(decode_imm_shift(1, 0), (SHIFT_LSR, 32));
        assert_eq!(decode_imm_shift(2, 0), (SHIFT_ASR, 32));
        assert_eq!(decode_imm_shift(3, 0), (SHIFT_RRX, 1));
        assert_eq!(decode_imm_shift(3, 5), (SHIFT_ROR, 5));
    }

    #[test]
    fn modified_immediates() {
        assert_eq!(thumb_expand_imm_c(0x0AB, false), (0xAB, false));
        assert_eq!(thumb_expand_imm_c(0x1AB, true), (0x00AB_00AB, true));
        assert_eq!(thumb_expand_imm_c(0x2AB, false), (0xAB00_AB00, false));
        assert_eq!(thumb_expand_imm_c(0x3AB, true), (0xABAB_ABAB, true));
        // Rotated form: imm12<11:7> = rotation, unrotated value = 0x80 | imm12<6:0>.
        // 0x47F: rotation = 0x47F >> 7 = 8, unrotated = 0xFF -> 0xFF000000 (bit 31 set -> carry)
        let (v, c) = thumb_expand_imm_c(0x47F, false);
        assert_eq!(v, 0xFF00_0000);
        assert!(c);
        // 0x800: rotation = 16, unrotated = 0x80 -> 0x0080_0000 (bit 31 clear -> carry cleared)
        let (v, c) = thumb_expand_imm_c(0x800, true);
        assert_eq!(v, 0x0080_0000);
        assert!(!c);
    }

    #[test]
    fn saturation() {
        assert_eq!(signed_sat_q(200, 8), (127, true));
        assert_eq!(signed_sat_q(-200, 8), (-128, true));
        assert_eq!(signed_sat_q(5, 8), (5, false));
        assert_eq!(signed_sat_q(i32::MAX as i64 + 1, 32), (i32::MAX, true));
        assert_eq!(signed_sat_q(i32::MIN as i64 - 1, 32), (i32::MIN, true));
        assert_eq!(unsigned_sat_q(300, 8), (255, true));
        assert_eq!(unsigned_sat_q(-1, 8), (0, true));
        assert_eq!(unsigned_sat_q(255, 8), (255, false));
        assert_eq!(unsigned_sat_q(0x1_0000_0000, 32), (u32::MAX, true));
        assert_eq!(unsigned_sat_q(7, 0), (0, true));
        assert_eq!(unsigned_sat_q(0, 0), (0, false));
    }

    #[test]
    fn parallel_signed_add16() {
        // SADD16: 0x7FFF + 1 = 0x8000 (no saturation), GE for non-negative sums.
        let (r, ge) = parallel_addsub(PAR_S * 6 + PAR_ADD16, 0x0001_7FFF, 0xFFFF_0001);
        assert_eq!(r, 0x0000_8000);
        // low lane: 0x7FFF + 1 >= 0 -> GE[1:0]=11 ; high lane: 1 + (-1) = 0 >= 0 -> GE[3:2]=11
        assert_eq!(ge, Some(0xF));
        let (r, ge) = parallel_addsub(PAR_S * 6 + PAR_ADD16, 0x0000_FFFF, 0x0000_0000);
        assert_eq!(r, 0x0000_FFFF);
        assert_eq!(ge, Some(0xC)); // low lane -1 < 0, high lane 0 >= 0
    }

    #[test]
    fn parallel_unsigned_and_sat() {
        // UADD8 carry sets GE.
        let (r, ge) = parallel_addsub(PAR_U * 6 + PAR_ADD8, 0xFF01_80FF, 0x0101_8001);
        // lane0: FF+01=0x100 carry, lane1: 80+80=0x100 carry, lane2: 01+01=2 no carry, lane3: FF+01 carry
        assert_eq!(r, 0x0002_0000);
        assert_eq!(ge, Some(0b1011));
        // UQADD8 saturates at 255.
        let (r, ge) = parallel_addsub(PAR_UQ * 6 + PAR_ADD8, 0xF0F0_F0F0, 0x2020_2020);
        assert_eq!(r, 0xFFFF_FFFF);
        assert_eq!(ge, None);
        // UQSUB16 clamps at 0.
        let (r, _) = parallel_addsub(PAR_UQ * 6 + PAR_SUB16, 0x0005_0010, 0x0006_0005);
        assert_eq!(r, 0x0000_000B);
        // SH variants halve.
        let (r, ge) = parallel_addsub(PAR_SH * 6 + PAR_ADD16, 0x0004_FFFE, 0x0002_0001);
        assert_eq!(r, 0x0003_FFFF); // (4+2)/2=3 ; (-2+1)>>1 = -1
        assert_eq!(ge, None);
        // QADD16 saturates signed.
        let (r, _) = parallel_addsub(PAR_Q * 6 + PAR_ADD16, 0x7FFF_8000, 0x0001_FFFF);
        assert_eq!(r, 0x7FFF_8000);
    }

    #[test]
    fn parallel_asx_sax() {
        // SASX: lo = a.lo - b.hi ; hi = a.hi + b.lo
        let (r, ge) = parallel_addsub(PAR_S * 6 + PAR_ASX, 0x0003_0009, 0x0004_0002);
        assert_eq!(r & 0xFFFF, 5);
        assert_eq!(r >> 16, 5);
        assert_eq!(ge, Some(0xF));
        // SSAX: lo = a.lo + b.hi ; hi = a.hi - b.lo
        let (r, _) = parallel_addsub(PAR_S * 6 + PAR_SAX, 0x0003_0009, 0x0004_0002);
        assert_eq!(r & 0xFFFF, 13);
        assert_eq!(r >> 16, 1);
        // USAX carry on the add lane, no borrow on the sub lane.
        let (r, ge) = parallel_addsub(PAR_U * 6 + PAR_SAX, 0x0010_FFFF, 0x0001_0002);
        assert_eq!(r & 0xFFFF, 0); // 0xFFFF + 1 wraps
        assert_eq!(r >> 16, 0x0E);
        assert_eq!(ge, Some(0xF));
    }

    #[test]
    fn misc() {
        assert_eq!(usad8(0x0A14_1E28, 0x0F0F_0F0F), 5 + 5 + 15 + 25);
        assert_eq!(sel(0b0101, 0xAABB_CCDD, 0x1122_3344), 0x11BB_33DD);
        assert_eq!(extend(ExtKind::Sxtb, 0x1280, 0), 0xFFFF_FF80);
        assert_eq!(extend(ExtKind::Uxth, 0x1234_5678, 16), 0x1234);
        assert_eq!(extend_b16(true, 0x0081_0080), 0xFF81_FF80);
        assert_eq!(extend_b16(false, 0x1281_3480), 0x0081_0080);
        assert_eq!(rbit(1), 0x8000_0000);
        assert_eq!(rev16(0x1122_3344), 0x2211_4433);
        assert_eq!(revsh(0x0000_0180), 0xFFFF_8001);
        assert_eq!(sdiv(0x8000_0000, 0xFFFF_FFFF), 0x8000_0000);
        assert_eq!(sdiv(7, 0), 0);
        assert_eq!(sdiv((-7i32) as u32, 2), (-3i32) as u32);
        assert_eq!(udiv(7, 2), 3);
        assert_eq!(udiv(7, 0), 0);
        assert_eq!(sign_extend(0x80, 8), 0xFFFF_FF80);
        assert_eq!(sign_extend(0x7F, 8), 0x7F);
    }
}
