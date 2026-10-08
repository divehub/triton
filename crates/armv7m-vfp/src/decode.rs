//! Thumb-2 coprocessor-space decoder for the FPv4-SP instruction set.
//!
//! The Thumb encodings of the floating-point instructions are bit-for-bit the
//! Arm encodings with the condition field replaced by `0b1110`, so the
//! decoder works on `w = hw1 << 16 | hw2` using the familiar A32 bit numbers
//! (`w[27:24]` = `hw1[11:8]`, `w[11:8]` = coprocessor number, ...).
//!
//! Classification:
//! * not a CP10/CP11 coprocessor encoding: [`VfpDecode::NotVfp`] (NOCP);
//! * CP10/CP11 but not implemented by FPv4-SP (double-precision data
//!   processing, Armv8 additions, Advanced SIMD moves, unallocated and
//!   UNPREDICTABLE register combinations): [`VfpDecode::Undefined`].
//!
//! Choices for UNPREDICTABLE encodings (documented, all decode as `Undefined`):
//! base register PC for VSTR/VLDM/VSTM/VPUSH/VPOP, empty or oversized register
//! lists, FLDMX/FSTMX (odd doubleword word count), register-transfer
//! instructions with PC as a core register, `VMOV Sm,Sm1` with `m == 31`,
//! `VMOV Rt,Rt2,...` with `Rt == Rt2`, fixed-point conversions with a negative
//! fraction-bit count, and use of D16..D31 (FPv4-SP only has D0..D15).

use crate::VfpDecode;

/// Operation selector of a decoded instruction.
///
/// Field conventions of [`VfpInsn`] per operation (`d`, `n`, `m` are 8-bit
/// register numbers; S registers are given as the S index 0..31, core
/// registers as 0..15):
///
/// | op | d | n | m | imm | flags |
/// | --- | --- | --- | --- | --- | --- |
/// | `Vldr`/`Vstr` | S | Rn | - | signed byte offset | - |
/// | `VldrD`/`VstrD` | S index of the low word (2*D) | Rn | - | signed byte offset | - |
/// | `Vldm`/`Vstm` | first S | Rn | word count | bytes (4*words) | `F_WBACK`, `F_DB`, `F_DOUBLE` |
/// | `VmovImm` | S | - | - | value | - |
/// | `VmovReg`, `Vabs`, `Vneg`, `Vsqrt` | S | - | S | - | - |
/// | `VmovToS` | S | Rt | - | - | `F_DOUBLE` (D half), `F_SCALAR` |
/// | `VmovFromS` | S | Rt | - | - | `F_DOUBLE`, `F_SCALAR` |
/// | `Vmov2ToS`/`Vmov2FromS` | first S | Rt | Rt2 | - | `F_DOUBLE` |
/// | `Vmrs` | Rt (15 = APSR_nzcv) | - | - | - | - |
/// | `Vmsr` | - | Rt | - | - | - |
/// | `Vadd` .. `Vfnms` | Sd | Sn | Sm | - | - |
/// | `Vcmp` | Sd | - | Sm | - | `F_E`, `F_ZERO` |
/// | `VcvtFromInt` | Sd | - | Sm | - | `F_UNSIGNED` |
/// | `VcvtToInt` | Sd | - | Sm | - | `F_UNSIGNED`, `F_ROUND_ZERO` |
/// | `VcvtFromFixed`/`VcvtToFixed` | S | - | - | fraction bits | `F_UNSIGNED`, `F_SIZE32` |
/// | `Vcvt16To32`/`Vcvt32To16` | Sd | - | Sm | - | `F_TOP` |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Op {
    Vldr,
    Vstr,
    VldrD,
    VstrD,
    Vldm,
    Vstm,
    VmovImm,
    VmovReg,
    VmovToS,
    VmovFromS,
    Vmov2ToS,
    Vmov2FromS,
    Vmrs,
    Vmsr,
    Vadd,
    Vsub,
    Vmul,
    Vnmul,
    Vdiv,
    Vsqrt,
    Vabs,
    Vneg,
    Vmla,
    Vmls,
    Vnmla,
    Vnmls,
    Vfma,
    Vfms,
    Vfnma,
    Vfnms,
    Vcmp,
    VcvtFromInt,
    VcvtToInt,
    VcvtFromFixed,
    VcvtToFixed,
    Vcvt16To32,
    Vcvt32To16,
}

/// `Vldm`/`Vstm`: base register writeback.
pub(crate) const F_WBACK: u8 = 1 << 0;
/// `Vldm`/`Vstm`: decrement before (else increment after).
pub(crate) const F_DB: u8 = 1 << 1;
/// Register list / transfer is made of doubleword registers (disassembly only).
pub(crate) const F_DOUBLE: u8 = 1 << 2;
/// Scalar form `VMOV.32 Dd[x], Rt` (disassembly only).
pub(crate) const F_SCALAR: u8 = 1 << 3;
/// `Vcmp`: VCMPE (signal on quiet NaN).
pub(crate) const F_E: u8 = 1 << 0;
/// `Vcmp`: compare with zero.
pub(crate) const F_ZERO: u8 = 1 << 1;
/// Conversions: unsigned integer / fixed-point type.
pub(crate) const F_UNSIGNED: u8 = 1 << 0;
/// `VcvtToInt`: round toward zero (VCVT) rather than FPSCR rounding (VCVTR).
pub(crate) const F_ROUND_ZERO: u8 = 1 << 1;
/// Fixed-point conversions: 32-bit (else 16-bit) fixed-point type.
pub(crate) const F_SIZE32: u8 = 1 << 1;
/// Half-precision conversions: top half (VCVTT) rather than bottom (VCVTB).
pub(crate) const F_TOP: u8 = 1 << 0;

/// A decoded FPv4-SP instruction.
///
/// The representation is opaque to users of the crate; it is `Copy` and
/// 16 bytes so the CPU can keep it inside its predecoded instruction cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VfpInsn {
    pub(crate) imm: u32,
    /// `hw1 << 16 | hw2`, kept for disassembly and debugging.
    pub(crate) raw: u32,
    pub(crate) op: Op,
    pub(crate) d: u8,
    pub(crate) n: u8,
    pub(crate) m: u8,
    pub(crate) flags: u8,
}

const _: () = assert!(core::mem::size_of::<VfpInsn>() <= 16);

impl VfpInsn {
    /// The original encoding as `(hw1, hw2)`.
    pub fn encoding(&self) -> (u16, u16) {
        ((self.raw >> 16) as u16, self.raw as u16)
    }

    #[inline(always)]
    fn make(raw: u32, op: Op, d: u32, n: u32, m: u32, imm: u32, flags: u8) -> VfpDecode {
        VfpDecode::Insn(VfpInsn {
            imm,
            raw,
            op,
            d: d as u8,
            n: n as u8,
            m: m as u8,
            flags,
        })
    }
}

/// `VFPExpandImm` for a single-precision VMOV (immediate).
pub fn expand_imm_f32(imm8: u32) -> u32 {
    let sign = (imm8 >> 7) & 1;
    let b6 = (imm8 >> 6) & 1;
    let exp = ((b6 ^ 1) << 7) | (if b6 == 1 { 0x1F << 2 } else { 0 }) | ((imm8 >> 4) & 3);
    (sign << 31) | (exp << 23) | ((imm8 & 0xF) << 19)
}

#[inline(always)]
fn bit(w: u32, n: u32) -> u32 {
    (w >> n) & 1
}

#[inline(always)]
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Decodes a 32-bit Thumb coprocessor-space encoding (`hw1` is the first halfword).
pub fn decode(hw1: u16, hw2: u16) -> VfpDecode {
    // Coprocessor space: hw1 = 111x 11xx xxxx xxxx.
    if hw1 & 0xEC00 != 0xEC00 {
        return VfpDecode::NotVfp;
    }
    // hw1[9:8] == 0b11 (0xEFxx / 0xFFxx) is not a coprocessor instruction at all.
    if hw1 & 0x0300 == 0x0300 {
        return VfpDecode::Undefined;
    }
    let coproc = (hw2 >> 8) & 0xF;
    if coproc & 0xE != 0xA {
        return VfpDecode::NotVfp;
    }
    // Unconditional (cond = 0b1111) encodings with CP10/CP11 are Armv8 additions
    // (VSEL, VMAXNM, VRINT*, VCVTA..) that FPv4-SP does not have.
    if hw1 & 0xF000 != 0xE000 {
        return VfpDecode::Undefined;
    }
    let w = ((hw1 as u32) << 16) | hw2 as u32;
    let dp = bit(w, 8) != 0; // sz: CP11 (doubleword registers)
    if bits(w, 27, 25) == 0b110 {
        load_store(w, dp)
    } else if bit(w, 4) == 0 {
        if dp {
            // Double-precision data processing is not implemented on FPv4-SP.
            VfpDecode::Undefined
        } else {
            data_processing(w)
        }
    } else {
        register_transfer(w, dp)
    }
}

/// Extension register load/store and 64-bit transfers (`w[27:25] == 0b110`).
fn load_store(w: u32, dp: bool) -> VfpDecode {
    let p = bit(w, 24);
    let u = bit(w, 23);
    let dbit = bit(w, 22);
    let wb = bit(w, 21);
    let l = bit(w, 20);
    let rn = bits(w, 19, 16);
    let vd = bits(w, 15, 12);
    let imm8 = bits(w, 7, 0);

    if p == 0 && u == 0 && wb == 0 {
        // MCRR/MRRC with CP10/CP11: VMOV between two core registers and
        // two single-precision registers or one doubleword register.
        if dbit == 0 || bits(w, 7, 6) != 0 || bit(w, 4) == 0 {
            return VfpDecode::Undefined;
        }
        let rt = vd;
        let rt2 = rn;
        let mbit = bit(w, 5);
        let vm = bits(w, 3, 0);
        if rt == 15 || rt2 == 15 || (l == 1 && rt == rt2) {
            return VfpDecode::Undefined;
        }
        let (sidx, flags) = if dp {
            let dm = (mbit << 4) | vm;
            if dm > 15 {
                return VfpDecode::Undefined;
            }
            (dm * 2, F_DOUBLE)
        } else {
            let sm = (vm << 1) | mbit;
            if sm == 31 {
                return VfpDecode::Undefined;
            }
            (sm, 0)
        };
        let op = if l == 0 { Op::Vmov2ToS } else { Op::Vmov2FromS };
        return VfpInsn::make(w, op, sidx, rt, rt2, 0, flags);
    }

    if p == 1 && wb == 0 {
        // VLDR / VSTR.
        let imm = imm8 << 2;
        let off = if u == 1 { imm } else { imm.wrapping_neg() };
        if rn == 15 && l == 0 {
            // VSTR with PC base is UNPREDICTABLE.
            return VfpDecode::Undefined;
        }
        return if dp {
            let d = (dbit << 4) | vd;
            if d > 15 {
                return VfpDecode::Undefined;
            }
            let op = if l == 1 { Op::VldrD } else { Op::VstrD };
            VfpInsn::make(w, op, d * 2, rn, 0, off, 0)
        } else {
            let d = (vd << 1) | dbit;
            let op = if l == 1 { Op::Vldr } else { Op::Vstr };
            VfpInsn::make(w, op, d, rn, 0, off, 0)
        };
    }

    if p == u && wb == 1 {
        return VfpDecode::Undefined;
    }
    // VLDM / VSTM (P:U:W = 0:1:x increment after, 1:0:1 decrement before),
    // including VPUSH / VPOP when the base is SP.
    if rn == 15 {
        return VfpDecode::Undefined;
    }
    let (first, words, flags) = if dp {
        if imm8 & 1 != 0 {
            // FLDMX / FSTMX (deprecated).
            return VfpDecode::Undefined;
        }
        let regs = imm8 >> 1;
        let d = (dbit << 4) | vd;
        if regs == 0 || regs > 16 || d + regs > 16 {
            return VfpDecode::Undefined;
        }
        (d * 2, regs * 2, F_DOUBLE)
    } else {
        let regs = imm8;
        let d = (vd << 1) | dbit;
        if regs == 0 || d + regs > 32 {
            return VfpDecode::Undefined;
        }
        (d, regs, 0)
    };
    let flags = flags | if wb == 1 { F_WBACK } else { 0 } | if p == 1 { F_DB } else { 0 };
    let op = if l == 1 { Op::Vldm } else { Op::Vstm };
    VfpInsn::make(w, op, first, rn, words, words * 4, flags)
}

/// Single-precision data-processing instructions (`w[27:24] == 0b1110`, bit 4 clear, CP10).
fn data_processing(w: u32) -> VfpDecode {
    let d = (bits(w, 15, 12) << 1) | bit(w, 22);
    let n = (bits(w, 19, 16) << 1) | bit(w, 7);
    let m = (bits(w, 3, 0) << 1) | bit(w, 5);
    let op6 = bit(w, 6);
    let three = |op: Op| VfpInsn::make(w, op, d, n, m, 0, 0);
    match (bit(w, 23), bits(w, 21, 20)) {
        (0, 0) => three(if op6 == 0 { Op::Vmla } else { Op::Vmls }),
        (0, 1) => three(if op6 == 0 { Op::Vnmls } else { Op::Vnmla }),
        (0, 2) => three(if op6 == 0 { Op::Vmul } else { Op::Vnmul }),
        (0, 3) => three(if op6 == 0 { Op::Vadd } else { Op::Vsub }),
        (1, 0) => {
            if op6 == 0 {
                three(Op::Vdiv)
            } else {
                VfpDecode::Undefined
            }
        }
        (1, 1) => three(if op6 == 0 { Op::Vfnms } else { Op::Vfnma }),
        (1, 2) => three(if op6 == 0 { Op::Vfma } else { Op::Vfms }),
        _ => other_data_processing(w, d, m),
    }
}

/// `opc1 == 1x11`: VMOV immediate/register, VABS, VNEG, VSQRT, VCVT*, VCMP*.
fn other_data_processing(w: u32, d: u32, m: u32) -> VfpDecode {
    let b7 = bit(w, 7);
    if bit(w, 6) == 0 {
        // VMOV (immediate): imm8 = imm4H:imm4L; bits 7 and 5 are SBZ (UNPREDICTABLE
        // when set: decoded as Undefined, like GNU binutils and QEMU do).
        if b7 != 0 || bit(w, 5) != 0 {
            return VfpDecode::Undefined;
        }
        let imm8 = (bits(w, 19, 16) << 4) | bits(w, 3, 0);
        return VfpInsn::make(w, Op::VmovImm, d, 0, 0, expand_imm_f32(imm8), 0);
    }
    let two = |op: Op, flags: u8| VfpInsn::make(w, op, d, 0, m, 0, flags);
    match bits(w, 19, 16) {
        0 => two(if b7 == 0 { Op::VmovReg } else { Op::Vabs }, 0),
        1 => two(if b7 == 0 { Op::Vneg } else { Op::Vsqrt }, 0),
        // VCVTB / VCVTT: half <-> single.
        2 => two(Op::Vcvt16To32, if b7 == 1 { F_TOP } else { 0 }),
        3 => two(Op::Vcvt32To16, if b7 == 1 { F_TOP } else { 0 }),
        // VCMP / VCMPE (register, then with zero: M and Vm are SBZ).
        4 => two(Op::Vcmp, if b7 == 1 { F_E } else { 0 }),
        5 => {
            if bit(w, 5) != 0 || bits(w, 3, 0) != 0 {
                VfpDecode::Undefined
            } else {
                two(Op::Vcmp, F_ZERO | if b7 == 1 { F_E } else { 0 })
            }
        }
        // VRINT*, VCVT between double and single: not in FPv4-SP.
        6 | 7 => VfpDecode::Undefined,
        // VCVT integer -> float (signed when bit 7 is set).
        8 => two(Op::VcvtFromInt, if b7 == 1 { 0 } else { F_UNSIGNED }),
        // VCVT / VCVTR float -> integer.
        12 | 13 => two(
            Op::VcvtToInt,
            (if bit(w, 16) == 0 { F_UNSIGNED } else { 0 }) | if b7 == 1 { F_ROUND_ZERO } else { 0 },
        ),
        // VCVT between floating-point and fixed-point.
        10 | 11 | 14 | 15 => {
            let opc2 = bits(w, 19, 16);
            let size32 = b7 == 1;
            let size = if size32 { 32 } else { 16 };
            let imm = (bits(w, 3, 0) << 1) | bit(w, 5);
            if imm > size {
                return VfpDecode::Undefined;
            }
            let fbits = size - imm;
            let op = if opc2 & 4 == 0 { Op::VcvtFromFixed } else { Op::VcvtToFixed };
            let flags = (if opc2 & 1 == 1 { F_UNSIGNED } else { 0 }) | if size32 { F_SIZE32 } else { 0 };
            VfpInsn::make(w, op, d, 0, 0, fbits, flags)
        }
        _ => VfpDecode::Undefined,
    }
}

/// 8/16/32-bit transfers between core and extension registers and
/// system-register moves (`w[27:24] == 0b1110`, bit 4 set).
fn register_transfer(w: u32, dp: bool) -> VfpDecode {
    let a = bits(w, 23, 21);
    let l = bit(w, 20);
    let rt = bits(w, 15, 12);
    if bits(w, 3, 0) != 0 {
        return VfpDecode::Undefined;
    }
    if !dp {
        if a == 0 {
            // VMOV between a core register and a single-precision register.
            if bits(w, 6, 5) != 0 || rt == 15 {
                return VfpDecode::Undefined;
            }
            let s = (bits(w, 19, 16) << 1) | bit(w, 7);
            let op = if l == 0 { Op::VmovToS } else { Op::VmovFromS };
            return VfpInsn::make(w, op, s, rt, 0, 0, 0);
        }
        if a == 7 {
            // VMRS / VMSR: only FPSCR is accessible on the Cortex-M4F.
            if bits(w, 7, 5) != 0 || bits(w, 19, 16) != 1 {
                return VfpDecode::Undefined;
            }
            return if l == 1 {
                VfpInsn::make(w, Op::Vmrs, rt, 0, 0, 0, 0)
            } else if rt == 15 {
                VfpDecode::Undefined
            } else {
                VfpInsn::make(w, Op::Vmsr, 0, rt, 0, 0, 0)
            };
        }
        return VfpDecode::Undefined;
    }
    // CP11: scalar moves. Only the 32-bit forms exist (no Advanced SIMD):
    // bit 23 clear (no VDUP / unsigned forms), opc1<1> = bit 22 clear, opc2 = 0.
    if a & 4 != 0 || bit(w, 22) != 0 || bits(w, 6, 5) != 0 || rt == 15 {
        return VfpDecode::Undefined;
    }
    let dreg = (bit(w, 7) << 4) | bits(w, 19, 16);
    if dreg > 15 {
        return VfpDecode::Undefined;
    }
    let s = dreg * 2 + bit(w, 21);
    let op = if l == 0 { Op::VmovToS } else { Op::VmovFromS };
    VfpInsn::make(w, op, s, rt, 0, 0, F_DOUBLE | F_SCALAR)
}
