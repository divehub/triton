//! Thumb / Thumb-2 (ARMv7E-M) instruction decoder.
//!
//! Follows the ARMv7-M Architecture Reference Manual encoding tables
//! (A5.2 16-bit, A5.3 32-bit). Unsupported or UNPREDICTABLE encodings that a
//! compiler never emits decode to `Kind::Undefined`.

use crate::alu::{self, SHIFT_ASR, SHIFT_LSL, SHIFT_LSR, SHIFT_ROR};
use crate::op::*;

/// True when `hw1` is the first halfword of a 32-bit instruction.
#[inline(always)]
pub fn is_32bit(hw1: u16) -> bool {
    (hw1 >> 11) >= 0b11101
}

/// Decodes the instruction at address `pc` whose halfwords are `hw1`/`hw2`
/// (`hw2` is ignored for 16-bit instructions).
pub fn decode(pc: u32, hw1: u16, hw2: u16) -> Op {
    if is_32bit(hw1) {
        decode32(pc, hw1, hw2)
    } else {
        decode16(pc, hw1)
    }
}

#[inline]
fn undef(len: u8, raw: u32) -> Op {
    Op::new(Kind::Undefined, len, raw)
}

#[inline]
fn sign_ext(v: u32, bits: u32) -> u32 {
    alu::sign_extend(v, bits)
}

#[inline]
fn align4(v: u32) -> u32 {
    v & !3
}

// Builder helpers --------------------------------------------------------------

#[inline]
fn op_rd_rn_imm(kind: Kind, len: u8, raw: u32, rd: u32, rn: u32, imm: u32, flags: u8) -> Op {
    let mut o = Op::new(kind, len, raw);
    o.rd = rd as u8;
    o.rn = rn as u8;
    o.imm = imm;
    o.flags = flags;
    o
}

#[inline]
fn op_rd_rn_rm(kind: Kind, len: u8, raw: u32, rd: u32, rn: u32, rm: u32, flags: u8) -> Op {
    let mut o = Op::new(kind, len, raw);
    o.rd = rd as u8;
    o.rn = rn as u8;
    o.rm = rm as u8;
    o.flags = flags;
    o
}

/// 16-bit encodings -------------------------------------------------------------

pub fn decode16(pc: u32, hw: u16) -> Op {
    let raw = hw as u32;
    let h = hw as u32;
    let pcv = pc.wrapping_add(4); // value of PC as read by the instruction
    const SIT: u8 = FL_S | FL_IT;
    match h >> 12 {
        0x0 | 0x1 => {
            // shift (immediate), add, subtract, move, compare
            match (h >> 11) & 3 {
                0 | 1 | 2 => {
                    // LSL/LSR/ASR immediate (hw[12:11] = 00, 01, 10)
                    let imm5 = (h >> 6) & 31;
                    let rm = (h >> 3) & 7;
                    let rd = h & 7;
                    let ty = (h >> 11) & 3;
                    match ty {
                        0 => {
                            if imm5 == 0 {
                                op_rd_rn_rm(Kind::MovReg, 2, raw, rd, 0, rm, SIT)
                            } else {
                                let mut o = op_rd_rn_rm(Kind::LslImm, 2, raw, rd, 0, rm, SIT);
                                o.x = imm5 as u8;
                                o
                            }
                        }
                        1 => {
                            let mut o = op_rd_rn_rm(Kind::LsrImm, 2, raw, rd, 0, rm, SIT);
                            o.x = if imm5 == 0 { 32 } else { imm5 as u8 };
                            o
                        }
                        _ => {
                            let mut o = op_rd_rn_rm(Kind::AsrImm, 2, raw, rd, 0, rm, SIT);
                            o.x = if imm5 == 0 { 32 } else { imm5 as u8 };
                            o
                        }
                    }
                }
                _ => {
                    // 00011: add/subtract register or 3-bit immediate
                    let rd = h & 7;
                    let rn = (h >> 3) & 7;
                    let m = (h >> 6) & 7;
                    match (h >> 9) & 3 {
                        0 => op_rd_rn_rm(Kind::AddReg, 2, raw, rd, rn, m, SIT),
                        1 => op_rd_rn_rm(Kind::SubReg, 2, raw, rd, rn, m, SIT),
                        2 => op_rd_rn_imm(Kind::AddImm, 2, raw, rd, rn, m, SIT),
                        _ => op_rd_rn_imm(Kind::SubImm, 2, raw, rd, rn, m, SIT),
                    }
                }
            }
        }
        0x2 | 0x3 => {
            let rd = (h >> 8) & 7;
            let imm8 = h & 0xFF;
            match (h >> 11) & 3 {
                0 => op_rd_rn_imm(Kind::MovImm, 2, raw, rd, 0, imm8, SIT),
                1 => op_rd_rn_imm(Kind::CmpImm, 2, raw, 0, rd, imm8, FL_S),
                2 => op_rd_rn_imm(Kind::AddImm, 2, raw, rd, rd, imm8, SIT),
                _ => op_rd_rn_imm(Kind::SubImm, 2, raw, rd, rd, imm8, SIT),
            }
        }
        0x4 => {
            if h & 0x0800 != 0 {
                // LDR (literal)
                let rt = (h >> 8) & 7;
                let addr = align4(pcv).wrapping_add((h & 0xFF) << 2);
                let mut o = Op::new(Kind::LdrLit, 2, raw);
                o.rd = rt as u8;
                o.imm = addr;
                return o;
            }
            if h & 0x0400 == 0 {
                // data processing
                let op = (h >> 6) & 0xF;
                let rm = (h >> 3) & 7;
                let rdn = h & 7;
                let regop = |k: Kind, flags: u8| op_rd_rn_rm(k, 2, raw, rdn, rdn, rm, flags);
                return match op {
                    0 => regop(Kind::AndReg, SIT),
                    1 => regop(Kind::EorReg, SIT),
                    2 => regop(Kind::LslReg, SIT),
                    3 => regop(Kind::LsrReg, SIT),
                    4 => regop(Kind::AsrReg, SIT),
                    5 => regop(Kind::AdcReg, SIT),
                    6 => regop(Kind::SbcReg, SIT),
                    7 => regop(Kind::RorReg, SIT),
                    8 => op_rd_rn_rm(Kind::TstReg, 2, raw, 0, rdn, rm, FL_S),
                    9 => op_rd_rn_imm(Kind::RsbImm, 2, raw, rdn, rm, 0, SIT),
                    10 => op_rd_rn_rm(Kind::CmpReg, 2, raw, 0, rdn, rm, FL_S),
                    11 => op_rd_rn_rm(Kind::CmnReg, 2, raw, 0, rdn, rm, FL_S),
                    12 => regop(Kind::OrrReg, SIT),
                    13 => op_rd_rn_rm(Kind::Mul, 2, raw, rdn, rm, rdn, SIT),
                    14 => regop(Kind::BicReg, SIT),
                    _ => op_rd_rn_rm(Kind::MvnReg, 2, raw, rdn, 0, rm, SIT),
                };
            }
            // special data instructions and branch and exchange
            let rm = (h >> 3) & 15;
            let rdn = (h & 7) | ((h >> 4) & 8);
            match (h >> 8) & 3 {
                0 => {
                    // ADD (register) T2, no flags
                    if rdn == 15 {
                        if rm == 15 {
                            return undef(2, raw);
                        }
                        return op_rd_rn_rm(Kind::AddToPc, 2, raw, 0, 0, rm, 0);
                    }
                    let fl = if rdn == 13 { FL_SPMASK } else { 0 };
                    if rm == 15 {
                        return op_rd_rn_imm(Kind::AddImm, 2, raw, rdn, rdn, pcv, fl);
                    }
                    op_rd_rn_rm(Kind::AddReg, 2, raw, rdn, rdn, rm, fl)
                }
                1 => {
                    // CMP (register) T2
                    if rdn == 15 {
                        return undef(2, raw);
                    }
                    if rm == 15 {
                        return op_rd_rn_imm(Kind::CmpImm, 2, raw, 0, rdn, pcv, FL_S);
                    }
                    op_rd_rn_rm(Kind::CmpReg, 2, raw, 0, rdn, rm, FL_S)
                }
                2 => {
                    // MOV (register) T1, no flags
                    let rd = rdn;
                    if rd == 15 {
                        if rm == 15 {
                            return undef(2, raw);
                        }
                        return op_rd_rn_rm(Kind::MovToPc, 2, raw, 0, 0, rm, 0);
                    }
                    if rm == 15 {
                        return op_rd_rn_imm(Kind::MovImm, 2, raw, rd, 0, pcv, 0);
                    }
                    let fl = if rd == 13 { FL_SPMASK } else { 0 };
                    op_rd_rn_rm(Kind::MovReg, 2, raw, rd, 0, rm, fl)
                }
                _ => {
                    // BX / BLX (register)
                    let mut o = Op::new(if h & 0x80 != 0 { Kind::Blx } else { Kind::Bx }, 2, raw);
                    o.rm = rm as u8;
                    o
                }
            }
        }
        0x5 => {
            // load/store (register offset)
            let rm = (h >> 6) & 7;
            let rn = (h >> 3) & 7;
            let rt = h & 7;
            let kind = match (h >> 9) & 7 {
                0 => Kind::StrReg,
                1 => Kind::StrhReg,
                2 => Kind::StrbReg,
                3 => Kind::LdrsbReg,
                4 => Kind::LdrReg,
                5 => Kind::LdrhReg,
                6 => Kind::LdrbReg,
                _ => Kind::LdrshReg,
            };
            op_rd_rn_rm(kind, 2, raw, rt, rn, rm, 0)
        }
        0x6 | 0x7 => {
            // LDR/STR/LDRB/STRB (immediate offset)
            let imm5 = (h >> 6) & 31;
            let rn = (h >> 3) & 7;
            let rt = h & 7;
            let load = h & 0x0800 != 0;
            let byte = h & 0x1000 != 0;
            let (kind, imm) = match (load, byte) {
                (false, false) => (Kind::StrImm, imm5 << 2),
                (true, false) => (Kind::LdrImm, imm5 << 2),
                (false, true) => (Kind::StrbImm, imm5),
                (true, true) => (Kind::LdrbImm, imm5),
            };
            op_rd_rn_imm(kind, 2, raw, rt, rn, imm, FL_IDX)
        }
        0x8 => {
            // STRH/LDRH (immediate)
            let imm = ((h >> 6) & 31) << 1;
            let rn = (h >> 3) & 7;
            let rt = h & 7;
            let kind = if h & 0x0800 != 0 { Kind::LdrhImm } else { Kind::StrhImm };
            op_rd_rn_imm(kind, 2, raw, rt, rn, imm, FL_IDX)
        }
        0x9 => {
            // STR/LDR (SP-relative)
            let rt = (h >> 8) & 7;
            let imm = (h & 0xFF) << 2;
            let kind = if h & 0x0800 != 0 { Kind::LdrImm } else { Kind::StrImm };
            op_rd_rn_imm(kind, 2, raw, rt, 13, imm, FL_IDX)
        }
        0xA => {
            let rd = (h >> 8) & 7;
            let imm = (h & 0xFF) << 2;
            if h & 0x0800 == 0 {
                // ADR: ADD Rd, PC, #imm8*4
                op_rd_rn_imm(Kind::MovImm, 2, raw, rd, 0, align4(pcv).wrapping_add(imm), 0)
            } else {
                op_rd_rn_imm(Kind::AddImm, 2, raw, rd, 13, imm, 0)
            }
        }
        0xB => decode16_misc(pc, hw),
        0xC => {
            let rn = (h >> 8) & 7;
            let list = h & 0xFF;
            let mut o = Op::new(if h & 0x0800 != 0 { Kind::Ldm } else { Kind::Stm }, 2, raw);
            o.rn = rn as u8;
            o.imm = list;
            // STM always writes back; LDM only when Rn is not in the list.
            if h & 0x0800 == 0 || (list >> rn) & 1 == 0 {
                o.flags = FL_WB;
            }
            o
        }
        0xD => {
            let cond = (h >> 8) & 0xF;
            match cond {
                0xE => undef(2, raw),
                0xF => {
                    let mut o = Op::new(Kind::Svc, 2, raw);
                    o.imm = h & 0xFF;
                    o
                }
                _ => {
                    let target = pcv.wrapping_add(sign_ext(h & 0xFF, 8) << 1);
                    let mut o = Op::new(Kind::Bcc, 2, raw);
                    o.x = cond as u8;
                    o.imm = target;
                    o
                }
            }
        }
        0xE => {
            // 11100: unconditional branch T2 (11101.. is a 32-bit prefix)
            if h & 0x0800 != 0 {
                return undef(2, raw);
            }
            let target = pcv.wrapping_add(sign_ext(h & 0x7FF, 11) << 1);
            // Renode parity: tlib translates the branch-to-self `B .` (0xE7FE) as a WFI that
            // retries the same instruction (`Kind::BSelf`).
            let mut o = Op::new(if h == 0xE7FE { Kind::BSelf } else { Kind::B }, 2, raw);
            o.imm = target;
            o
        }
        _ => undef(2, raw),
    }
}

fn decode16_misc(pc: u32, hw: u16) -> Op {
    let raw = hw as u32;
    let h = hw as u32;
    let pcv = pc.wrapping_add(4);
    // CBZ / CBNZ: 1011 x0x1 imm5 Rn
    if h & 0xF500 == 0xB100 {
        let imm = (((h >> 9) & 1) << 6) | (((h >> 3) & 31) << 1);
        let mut o = Op::new(if h & 0x0800 != 0 { Kind::Cbnz } else { Kind::Cbz }, 2, raw);
        o.rn = (h & 7) as u8;
        o.imm = pcv.wrapping_add(imm);
        return o;
    }
    match (h >> 8) & 0xF {
        0x0 => {
            let imm = (h & 0x7F) << 2;
            let kind = if h & 0x80 != 0 { Kind::SubImm } else { Kind::AddImm };
            op_rd_rn_imm(kind, 2, raw, 13, 13, imm, FL_SPMASK)
        }
        0x2 => {
            let kind = match (h >> 6) & 3 {
                0 => Kind::Sxth,
                1 => Kind::Sxtb,
                2 => Kind::Uxth,
                _ => Kind::Uxtb,
            };
            op_rd_rn_rm(kind, 2, raw, h & 7, 0, (h >> 3) & 7, 0)
        }
        0x4 | 0x5 => {
            let mut o = Op::new(Kind::Push, 2, raw);
            o.imm = (h & 0xFF) | (((h >> 8) & 1) << 14);
            o
        }
        0x6 => {
            if h & 0xFFE8 == 0xB660 {
                let mut o = Op::new(Kind::Cps, 2, raw);
                // x: bit0 = F, bit1 = I, bit4 = disable
                o.x = (((h >> 4) & 1) << 4 | ((h >> 1) & 1) << 1 | (h & 1)) as u8;
                o
            } else {
                undef(2, raw)
            }
        }
        0xA => {
            let kind = match (h >> 6) & 3 {
                0 => Kind::Rev,
                1 => Kind::Rev16,
                3 => Kind::Revsh,
                _ => return undef(2, raw),
            };
            op_rd_rn_rm(kind, 2, raw, h & 7, 0, (h >> 3) & 7, 0)
        }
        0xC | 0xD => {
            let mut o = Op::new(if h & 0x100 != 0 { Kind::PopPc } else { Kind::Pop }, 2, raw);
            o.imm = (h & 0xFF) | (((h >> 8) & 1) << 15);
            o
        }
        0xE => {
            let mut o = Op::new(Kind::Bkpt, 2, raw);
            o.imm = h & 0xFF;
            o
        }
        0xF => {
            let mask = h & 0xF;
            let first = (h >> 4) & 0xF;
            if mask != 0 {
                let mut o = Op::new(Kind::It, 2, raw);
                o.imm = (first << 4) | mask;
                o
            } else {
                match first {
                    2 => Op::new(Kind::Wfe, 2, raw),
                    3 => Op::new(Kind::Wfi, 2, raw),
                    4 => Op::new(Kind::Sev, 2, raw),
                    _ => Op::new(Kind::Nop, 2, raw), // NOP, YIELD and reserved hints
                }
            }
        }
        _ => undef(2, raw),
    }
}

/// 32-bit encodings -------------------------------------------------------------

pub fn decode32(pc: u32, hw1: u16, hw2: u16) -> Op {
    let h1 = hw1 as u32;
    let h2 = hw2 as u32;
    let raw = (h1 << 16) | h2;
    match (h1 >> 11) & 3 {
        1 => decode32_op1_01(pc, h1, h2, raw),
        2 => decode32_op1_10(pc, h1, h2, raw),
        _ => decode32_op1_11(pc, h1, h2, raw),
    }
}

fn coproc(raw: u32) -> Op {
    let mut o = Op::new(Kind::Coproc, 4, raw);
    o.imm = raw;
    o
}

/// Common data-processing operation decoder shared by the modified-immediate
/// and shifted-register forms. `opc` is the 4-bit operation field.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DpOp {
    And,
    Bic,
    Orr,
    Orn,
    Eor,
    Add,
    Adc,
    Sbc,
    Sub,
    Rsb,
    // pseudo operations
    Tst,
    Teq,
    Cmn,
    Cmp,
    Mov,
    Mvn,
}

fn dp_opcode(opc: u32, rn: u32, rd: u32, s: bool) -> Option<DpOp> {
    Some(match opc {
        0b0000 => {
            if rd == 15 && s {
                DpOp::Tst
            } else {
                DpOp::And
            }
        }
        0b0001 => DpOp::Bic,
        0b0010 => {
            if rn == 15 {
                DpOp::Mov
            } else {
                DpOp::Orr
            }
        }
        0b0011 => {
            if rn == 15 {
                DpOp::Mvn
            } else {
                DpOp::Orn
            }
        }
        0b0100 => {
            if rd == 15 && s {
                DpOp::Teq
            } else {
                DpOp::Eor
            }
        }
        0b1000 => {
            if rd == 15 && s {
                DpOp::Cmn
            } else {
                DpOp::Add
            }
        }
        0b1010 => DpOp::Adc,
        0b1011 => DpOp::Sbc,
        0b1101 => {
            if rd == 15 && s {
                DpOp::Cmp
            } else {
                DpOp::Sub
            }
        }
        0b1110 => DpOp::Rsb,
        _ => return None,
    })
}

fn decode32_op1_01(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let op2 = (h1 >> 4) & 0x7F;
    if op2 & 0x64 == 0x00 {
        return decode_ldm_stm(h1, h2, raw);
    }
    if op2 & 0x64 == 0x04 {
        return decode_dual_excl(pc, h1, h2, raw);
    }
    if op2 & 0x60 == 0x20 {
        return decode_dp_shifted(h1, h2, raw);
    }
    coproc(raw)
}

fn decode_ldm_stm(h1: u32, h2: u32, raw: u32) -> Op {
    let rn = h1 & 0xF;
    let w = (h1 >> 5) & 1 != 0;
    let l = (h1 >> 4) & 1 != 0;
    let list = h2 & 0xFFFF;
    match (h1 >> 7) & 3 {
        1 => {
            // IA
            let pc_in_list = list & 0x8000 != 0;
            if l && w && rn == 13 {
                let mut o = Op::new(if pc_in_list { Kind::PopPc } else { Kind::Pop }, 4, raw);
                o.imm = list;
                return o;
            }
            let mut o = Op::new(if !l { Kind::Stm } else if pc_in_list { Kind::LdmPc } else { Kind::Ldm }, 4, raw);
            o.rn = rn as u8;
            o.imm = list;
            if w {
                o.flags |= FL_WB;
            }
            o
        }
        2 => {
            // DB
            if !l && w && rn == 13 {
                let mut o = Op::new(Kind::Push, 4, raw);
                o.imm = list;
                return o;
            }
            let mut o = Op::new(if !l { Kind::Stm } else if list & 0x8000 != 0 { Kind::LdmPc } else { Kind::Ldm }, 4, raw);
            o.rn = rn as u8;
            o.imm = list;
            o.flags |= FL_DB;
            if w {
                o.flags |= FL_WB;
            }
            o
        }
        _ => undef(4, raw), // SRS / RFE do not exist on M profile
    }
}

fn decode_dual_excl(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let rn = h1 & 0xF;
    let p = (h1 >> 8) & 1;
    let u = (h1 >> 7) & 1;
    let w = (h1 >> 5) & 1;
    let l = (h1 >> 4) & 1;
    let rt = (h2 >> 12) & 0xF;
    let imm8 = h2 & 0xFF;
    let op1d = (h1 >> 7) & 3;
    let op2d = (h1 >> 4) & 3;
    if op1d == 0 && op2d == 0 {
        // STREX Rd, Rt, [Rn, #imm8*4]
        let mut o = Op::new(Kind::Strex, 4, raw);
        o.rd = ((h2 >> 8) & 0xF) as u8;
        o.ra = rt as u8;
        o.rn = rn as u8;
        o.imm = imm8 << 2;
        return o;
    }
    if op1d == 0 && op2d == 1 {
        let mut o = Op::new(Kind::Ldrex, 4, raw);
        o.rd = rt as u8;
        o.rn = rn as u8;
        o.imm = imm8 << 2;
        return o;
    }
    if op1d == 1 && op2d == 0 {
        let op3 = (h2 >> 4) & 0xF;
        let kind = match op3 {
            4 => Kind::Strexb,
            5 => Kind::Strexh,
            _ => return undef(4, raw),
        };
        let mut o = Op::new(kind, 4, raw);
        o.rd = (h2 & 0xF) as u8;
        o.ra = rt as u8;
        o.rn = rn as u8;
        return o;
    }
    if op1d == 1 && op2d == 1 {
        let op3 = (h2 >> 4) & 0xF;
        let pcv = pc.wrapping_add(4);
        return match op3 {
            0 | 1 => {
                let mut o = Op::new(if op3 == 0 { Kind::Tbb } else { Kind::Tbh }, 4, raw);
                o.rn = rn as u8;
                o.rm = (h2 & 0xF) as u8;
                o.imm = pcv;
                o
            }
            4 | 5 => {
                let mut o = Op::new(if op3 == 4 { Kind::Ldrexb } else { Kind::Ldrexh }, 4, raw);
                o.rd = rt as u8;
                o.rn = rn as u8;
                o
            }
            _ => undef(4, raw),
        };
    }
    // LDRD / STRD (immediate or literal)
    if p == 0 && w == 0 {
        return undef(4, raw);
    }
    let rt2 = (h2 >> 8) & 0xF;
    let off = imm8 << 2;
    let mut o = Op::new(if l == 1 { Kind::LdrdImm } else { Kind::StrdImm }, 4, raw);
    o.rd = rt as u8;
    o.ra = rt2 as u8;
    o.rn = rn as u8;
    if rn == 15 {
        if l == 0 {
            return undef(4, raw);
        }
        let base = align4(pc.wrapping_add(4));
        o.imm = if u == 1 { base.wrapping_add(off) } else { base.wrapping_sub(off) };
        o.flags = FL_IDX;
        return o;
    }
    o.imm = if u == 1 { off } else { off.wrapping_neg() };
    if p == 1 {
        o.flags |= FL_IDX;
    }
    if w == 1 {
        o.flags |= FL_WB;
    }
    o
}

fn decode_dp_shifted(h1: u32, h2: u32, raw: u32) -> Op {
    if h2 & 0x8000 != 0 {
        return undef(4, raw);
    }
    let opc = (h1 >> 5) & 0xF;
    let s = (h1 >> 4) & 1 != 0;
    let rn = h1 & 0xF;
    let rd = (h2 >> 8) & 0xF;
    let rm = h2 & 0xF;
    let imm5 = ((h2 >> 12) & 7) << 2 | ((h2 >> 6) & 3);
    let ty = (h2 >> 4) & 3;
    let (sty, samt) = alu::decode_imm_shift(ty, imm5);
    let sflag = if s { FL_S } else { 0 };

    // PKHBT / PKHTB
    if opc == 0b0110 {
        if s || (h2 & 0x10) != 0 {
            return undef(4, raw);
        }
        let tb = (h2 >> 5) & 1 != 0;
        let (kind, ty2, amt) = if tb {
            // PKHTB: ASR, imm5 == 0 encodes #32
            (Kind::Pkhtb, SHIFT_ASR, if imm5 == 0 { 32 } else { imm5 })
        } else {
            (Kind::Pkhbt, SHIFT_LSL, imm5)
        };
        let mut o = op_rd_rn_rm(kind, 4, raw, rd, rn, rm, 0);
        o.ra = ty2;
        o.x = amt as u8;
        return o;
    }

    let dp = match dp_opcode(opc, rn, rd, s) {
        Some(d) => d,
        None => return undef(4, raw),
    };
    // Destination PC is only valid for the compare/test forms.
    let is_cmp = matches!(dp, DpOp::Tst | DpOp::Teq | DpOp::Cmn | DpOp::Cmp);
    if !is_cmp && rd == 15 {
        return undef(4, raw);
    }
    match dp {
        DpOp::Mov => {
            // MOV (register) / LSL / LSR / ASR / ROR (immediate) / RRX
            let (kind, amt) = match sty {
                SHIFT_LSL => {
                    if samt == 0 {
                        (Kind::MovReg, 0)
                    } else {
                        (Kind::LslImm, samt)
                    }
                }
                SHIFT_LSR => (Kind::LsrImm, samt),
                SHIFT_ASR => (Kind::AsrImm, samt),
                SHIFT_ROR => (Kind::RorImm, samt),
                _ => (Kind::Rrx, 1),
            };
            let mut o = op_rd_rn_rm(kind, 4, raw, rd, 0, rm, sflag);
            o.x = amt as u8;
            if rd == 13 && kind == Kind::MovReg {
                o.flags |= FL_SPMASK;
            }
            o
        }
        DpOp::Mvn => {
            let mut o = op_rd_rn_rm(Kind::MvnReg, 4, raw, rd, 0, rm, sflag);
            o.ra = sty;
            o.x = samt as u8;
            o
        }
        _ => {
            let (kind, fl) = match dp {
                DpOp::And => (Kind::AndReg, sflag),
                DpOp::Bic => (Kind::BicReg, sflag),
                DpOp::Orr => (Kind::OrrReg, sflag),
                DpOp::Orn => (Kind::OrnReg, sflag),
                DpOp::Eor => (Kind::EorReg, sflag),
                DpOp::Add => (Kind::AddReg, sflag),
                DpOp::Adc => (Kind::AdcReg, sflag),
                DpOp::Sbc => (Kind::SbcReg, sflag),
                DpOp::Sub => (Kind::SubReg, sflag),
                DpOp::Rsb => (Kind::RsbReg, sflag),
                DpOp::Tst => (Kind::TstReg, FL_S),
                DpOp::Teq => (Kind::TeqReg, FL_S),
                DpOp::Cmn => (Kind::CmnReg, FL_S),
                _ => (Kind::CmpReg, FL_S),
            };
            let rdv = if is_cmp { 0 } else { rd };
            let mut o = op_rd_rn_rm(kind, 4, raw, rdv, rn, rm, fl);
            o.ra = sty;
            o.x = samt as u8;
            if rd == 13 && matches!(kind, Kind::AddReg | Kind::SubReg) {
                o.flags |= FL_SPMASK;
            }
            o
        }
    }
}

fn decode32_op1_10(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    if h2 & 0x8000 == 0 {
        if h1 & 0x0200 == 0 {
            decode_dp_modified_imm(pc, h1, h2, raw)
        } else {
            decode_dp_plain_imm(pc, h1, h2, raw)
        }
    } else {
        decode_branch_misc(pc, h1, h2, raw)
    }
}

fn decode_dp_modified_imm(_pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let opc = (h1 >> 5) & 0xF;
    let s = (h1 >> 4) & 1 != 0;
    let rn = h1 & 0xF;
    let rd = (h2 >> 8) & 0xF;
    let imm12 = (((h1 >> 10) & 1) << 11) | (((h2 >> 12) & 7) << 8) | (h2 & 0xFF);
    let (imm32, _c) = alu::thumb_expand_imm_c(imm12, false);
    let rotated = alu::thumb_imm_is_rotated(imm12);
    let dp = match dp_opcode(opc, rn, rd, s) {
        Some(d) => d,
        None => return undef(4, raw),
    };
    let is_cmp = matches!(dp, DpOp::Tst | DpOp::Teq | DpOp::Cmn | DpOp::Cmp);
    if !is_cmp && rd == 15 {
        return undef(4, raw);
    }
    let sflag = if s { FL_S } else { 0 };
    let immc = if rotated { FL_IMMC } else { 0 };
    let (kind, fl) = match dp {
        DpOp::And => (Kind::AndImm, sflag | immc),
        DpOp::Bic => (Kind::BicImm, sflag | immc),
        DpOp::Orr => (Kind::OrrImm, sflag | immc),
        DpOp::Orn => (Kind::OrnImm, sflag | immc),
        DpOp::Eor => (Kind::EorImm, sflag | immc),
        DpOp::Mov => (Kind::MovImm, sflag | immc),
        DpOp::Mvn => (Kind::MvnImm, sflag | immc),
        DpOp::Tst => (Kind::TstImm, FL_S | immc),
        DpOp::Teq => (Kind::TeqImm, FL_S | immc),
        DpOp::Add => (Kind::AddImm, sflag),
        DpOp::Adc => (Kind::AdcImm, sflag),
        DpOp::Sbc => (Kind::SbcImm, sflag),
        DpOp::Sub => (Kind::SubImm, sflag),
        DpOp::Rsb => (Kind::RsbImm, sflag),
        DpOp::Cmn => (Kind::CmnImm, FL_S),
        DpOp::Cmp => (Kind::CmpImm, FL_S),
    };
    let (rdv, rnv) = match dp {
        DpOp::Mov | DpOp::Mvn => (rd, 0),
        DpOp::Tst | DpOp::Teq | DpOp::Cmn | DpOp::Cmp => (0, rn),
        _ => (rd, rn),
    };
    let mut o = op_rd_rn_imm(kind, 4, raw, rdv, rnv, imm32, fl);
    if rd == 13 && matches!(kind, Kind::AddImm | Kind::SubImm) {
        o.flags |= FL_SPMASK;
    }
    o
}

fn decode_dp_plain_imm(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let rn = h1 & 0xF;
    let rd = (h2 >> 8) & 0xF;
    let imm3 = (h2 >> 12) & 7;
    let imm2 = (h2 >> 6) & 3;
    let imm8 = h2 & 0xFF;
    let i = (h1 >> 10) & 1;
    let pcv = pc.wrapping_add(4);
    match (h1 >> 4) & 0x1F {
        0b00000 | 0b01010 => {
            // ADDW / SUBW (imm12); Rn == PC -> ADR
            let imm12 = (i << 11) | (imm3 << 8) | imm8;
            let sub = (h1 >> 4) & 0x1F == 0b01010;
            if rd == 15 {
                return undef(4, raw);
            }
            if rn == 15 {
                let base = align4(pcv);
                let v = if sub { base.wrapping_sub(imm12) } else { base.wrapping_add(imm12) };
                return op_rd_rn_imm(Kind::MovImm, 4, raw, rd, 0, v, 0);
            }
            let fl = if rd == 13 { FL_SPMASK } else { 0 };
            op_rd_rn_imm(if sub { Kind::SubImm } else { Kind::AddImm }, 4, raw, rd, rn, imm12, fl)
        }
        0b00100 | 0b01100 => {
            // MOVW / MOVT
            let imm16 = ((h1 & 0xF) << 12) | (i << 11) | (imm3 << 8) | imm8;
            if rd == 15 || rd == 13 {
                return undef(4, raw);
            }
            if (h1 >> 4) & 0x1F == 0b00100 {
                op_rd_rn_imm(Kind::Movw, 4, raw, rd, 0, imm16, 0)
            } else {
                op_rd_rn_imm(Kind::Movt, 4, raw, rd, 0, imm16 << 16, 0)
            }
        }
        0b10000 | 0b10010 | 0b11000 | 0b11010 => {
            // SSAT/SSAT16/USAT/USAT16
            let unsigned = (h1 >> 4) & 0x08 != 0;
            let sh = (h1 >> 5) & 1;
            let imm5 = (imm3 << 2) | imm2;
            if rd == 13 || rd == 15 || rn == 13 || rn == 15 {
                return undef(4, raw);
            }
            if sh == 1 && imm5 == 0 {
                let sat = if unsigned { h2 & 0xF } else { (h2 & 0xF) + 1 };
                let mut o = op_rd_rn_imm(if unsigned { Kind::Usat16 } else { Kind::Ssat16 }, 4, raw, rd, rn, sat, 0);
                o.x = 0;
                return o;
            }
            let sat = if unsigned { h2 & 0x1F } else { (h2 & 0x1F) + 1 };
            let (sty, samt) = alu::decode_imm_shift(if sh == 1 { 2 } else { 0 }, imm5);
            let mut o = op_rd_rn_imm(if unsigned { Kind::Usat } else { Kind::Ssat }, 4, raw, rd, rn, sat, 0);
            o.ra = sty;
            o.x = samt as u8;
            o
        }
        0b10100 | 0b11100 => {
            // SBFX / UBFX
            if rd == 13 || rd == 15 || rn == 13 || rn == 15 {
                return undef(4, raw);
            }
            let lsb = (imm3 << 2) | imm2;
            let widthm1 = h2 & 0x1F;
            if lsb + widthm1 > 31 {
                return undef(4, raw);
            }
            let kind = if (h1 >> 4) & 0x1F == 0b10100 { Kind::Sbfx } else { Kind::Ubfx };
            let mut o = op_rd_rn_imm(kind, 4, raw, rd, rn, 0, 0);
            o.x = lsb as u8;
            o.ra = widthm1 as u8;
            o
        }
        0b10110 => {
            // BFI / BFC
            let lsb = (imm3 << 2) | imm2;
            let msb = h2 & 0x1F;
            if rd == 13 || rd == 15 || msb < lsb {
                return undef(4, raw);
            }
            let kind = if rn == 15 { Kind::Bfc } else { Kind::Bfi };
            let mut o = op_rd_rn_imm(kind, 4, raw, rd, if rn == 15 { 0 } else { rn }, 0, 0);
            o.x = lsb as u8;
            o.ra = msb as u8;
            o
        }
        _ => undef(4, raw),
    }
}

fn decode_branch_misc(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let pcv = pc.wrapping_add(4);
    let op1 = (h2 >> 12) & 7;
    let s = (h1 >> 10) & 1;
    let j1 = (h2 >> 13) & 1;
    let j2 = (h2 >> 11) & 1;
    if op1 & 5 == 0 {
        // conditional branch T3 or miscellaneous control
        if (h1 >> 7) & 7 != 7 {
            let cond = (h1 >> 6) & 0xF;
            let imm = (s << 20) | (j2 << 19) | (j1 << 18) | ((h1 & 0x3F) << 12) | ((h2 & 0x7FF) << 1);
            let mut o = Op::new(Kind::Bcc, 4, raw);
            o.x = cond as u8;
            o.imm = pcv.wrapping_add(sign_ext(imm, 21));
            return o;
        }
        return match (h1 >> 4) & 0x7F {
            0b0111000 => {
                // MSR
                if h2 & 0xF300 != 0x8000 {
                    return undef(4, raw);
                }
                let mut o = Op::new(Kind::Msr, 4, raw);
                o.rn = (h1 & 0xF) as u8;
                o.imm = h2 & 0xFF;
                o.x = ((h2 >> 10) & 3) as u8;
                o
            }
            0b0111010 => {
                if h1 & 0xF != 0xF || h2 & 0xFF00 != 0x8000 {
                    return undef(4, raw);
                }
                match h2 & 0xFF {
                    2 => Op::new(Kind::Wfe, 4, raw),
                    3 => Op::new(Kind::Wfi, 4, raw),
                    4 => Op::new(Kind::Sev, 4, raw),
                    _ => Op::new(Kind::Nop, 4, raw),
                }
            }
            0b0111011 => {
                if h1 & 0xF != 0xF || h2 & 0xFF00 != 0x8F00 {
                    return undef(4, raw);
                }
                match (h2 >> 4) & 0xF {
                    2 => Op::new(Kind::Clrex, 4, raw),
                    4 | 5 | 6 => Op::new(Kind::Barrier, 4, raw), // DSB, DMB, ISB (end tlib's translation block)
                    _ => undef(4, raw),
                }
            }
            0b0111110 | 0b0111111 => {
                if h1 & 0xF != 0xF || h2 & 0xF000 != 0x8000 {
                    return undef(4, raw);
                }
                let mut o = Op::new(Kind::Mrs, 4, raw);
                o.rd = ((h2 >> 8) & 0xF) as u8;
                o.imm = h2 & 0xFF;
                o
            }
            _ => undef(4, raw), // includes UDF
        };
    }
    if op1 & 5 == 1 {
        // B T4
        let i1 = (!(j1 ^ s)) & 1;
        let i2 = (!(j2 ^ s)) & 1;
        let imm = (s << 24) | (i1 << 23) | (i2 << 22) | ((h1 & 0x3FF) << 12) | ((h2 & 0x7FF) << 1);
        let mut o = Op::new(Kind::B, 4, raw);
        o.imm = pcv.wrapping_add(sign_ext(imm, 25));
        return o;
    }
    if op1 & 5 == 5 {
        // BL
        let i1 = (!(j1 ^ s)) & 1;
        let i2 = (!(j2 ^ s)) & 1;
        let imm = (s << 24) | (i1 << 23) | (i2 << 22) | ((h1 & 0x3FF) << 12) | ((h2 & 0x7FF) << 1);
        let mut o = Op::new(Kind::Bl, 4, raw);
        o.imm = pcv.wrapping_add(sign_ext(imm, 25));
        return o;
    }
    undef(4, raw) // BLX (immediate) is ARM-state only
}

fn decode32_op1_11(pc: u32, h1: u32, h2: u32, raw: u32) -> Op {
    let op2 = (h1 >> 4) & 0x7F;
    if op2 & 0x40 != 0 {
        return coproc(raw);
    }
    if op2 & 0x60 == 0 {
        // loads and stores
        if op2 & 0x71 == 0x00 {
            return decode_store_single(h1, h2, raw);
        }
        return match op2 & 0x07 {
            0x1 => decode_load_single(pc, h1, h2, raw, Size::Byte),
            0x3 => decode_load_single(pc, h1, h2, raw, Size::Half),
            0x5 => decode_load_single(pc, h1, h2, raw, Size::Word),
            _ => undef(4, raw),
        };
    }
    if op2 & 0x70 == 0x20 {
        return decode_dp_register(h1, h2, raw);
    }
    if op2 & 0x78 == 0x30 {
        return decode_multiply(h1, h2, raw);
    }
    decode_long_multiply(h1, h2, raw)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    Byte,
    Half,
    Word,
}

fn decode_store_single(h1: u32, h2: u32, raw: u32) -> Op {
    let rn = h1 & 0xF;
    let rt = (h2 >> 12) & 0xF;
    if rn == 15 {
        return undef(4, raw);
    }
    let size_op = (h1 >> 5) & 7;
    let (size, imm12) = match size_op {
        0 => (Size::Byte, false),
        1 => (Size::Half, false),
        2 => (Size::Word, false),
        4 => (Size::Byte, true),
        5 => (Size::Half, true),
        6 => (Size::Word, true),
        _ => return undef(4, raw),
    };
    let kinds = |s: Size| match s {
        Size::Byte => (Kind::StrbImm, Kind::StrbReg),
        Size::Half => (Kind::StrhImm, Kind::StrhReg),
        Size::Word => (Kind::StrImm, Kind::StrReg),
    };
    let (kimm, kreg) = kinds(size);
    if imm12 {
        return op_rd_rn_imm(kimm, 4, raw, rt, rn, h2 & 0xFFF, FL_IDX);
    }
    if h2 & 0x0800 != 0 {
        let p = (h2 >> 10) & 1;
        let u = (h2 >> 9) & 1;
        let w = (h2 >> 8) & 1;
        let imm8 = h2 & 0xFF;
        if p == 0 && w == 0 {
            return undef(4, raw);
        }
        // P=1, U=1, W=0 is the unprivileged variant (STRT/STRHT/STRBT): same access here.
        let imm = if u == 1 { imm8 } else { imm8.wrapping_neg() };
        let mut fl = 0;
        if p == 1 {
            fl |= FL_IDX;
        }
        if w == 1 {
            fl |= FL_WB;
        }
        return op_rd_rn_imm(kimm, 4, raw, rt, rn, imm, fl);
    }
    if h2 & 0x0FC0 == 0 {
        let mut o = op_rd_rn_rm(kreg, 4, raw, rt, rn, h2 & 0xF, 0);
        o.x = ((h2 >> 4) & 3) as u8;
        return o;
    }
    undef(4, raw)
}

fn decode_load_single(pc: u32, h1: u32, h2: u32, raw: u32, size: Size) -> Op {
    let rn = h1 & 0xF;
    let rt = (h2 >> 12) & 0xF;
    let signed = (h1 >> 8) & 1 != 0;
    let u_imm12 = (h1 >> 7) & 1 != 0;
    if size == Size::Word && signed {
        return undef(4, raw);
    }
    let (kimm, kreg, klit) = match (size, signed) {
        (Size::Byte, false) => (Kind::LdrbImm, Kind::LdrbReg, Kind::LdrbLit),
        (Size::Byte, true) => (Kind::LdrsbImm, Kind::LdrsbReg, Kind::LdrsbLit),
        (Size::Half, false) => (Kind::LdrhImm, Kind::LdrhReg, Kind::LdrhLit),
        (Size::Half, true) => (Kind::LdrshImm, Kind::LdrshReg, Kind::LdrshLit),
        (Size::Word, _) => (Kind::LdrImm, Kind::LdrReg, Kind::LdrLit),
    };
    // Loads into PC: only the word form (LDR) is a branch; halfword/byte loads
    // with Rt == 15 are memory hints (PLD/PLI/unallocated) and execute as NOPs.
    let hint = rt == 15 && size != Size::Word;
    let nop = || Op::new(Kind::Nop, 4, raw);

    if rn == 15 {
        // literal
        let imm12 = h2 & 0xFFF;
        let base = align4(pc.wrapping_add(4));
        let addr = if u_imm12 { base.wrapping_add(imm12) } else { base.wrapping_sub(imm12) };
        if hint {
            return nop();
        }
        if rt == 15 {
            let mut o = Op::new(Kind::LdrToPc, 4, raw);
            o.x = 2;
            o.imm = addr;
            return o;
        }
        let mut o = Op::new(klit, 4, raw);
        o.rd = rt as u8;
        o.imm = addr;
        return o;
    }
    if u_imm12 {
        // imm12 form
        if hint {
            return nop();
        }
        let imm12 = h2 & 0xFFF;
        if rt == 15 {
            return ldr_to_pc_imm(rn, imm12, FL_IDX, raw);
        }
        return op_rd_rn_imm(kimm, 4, raw, rt, rn, imm12, FL_IDX);
    }
    if h2 & 0x0800 != 0 {
        // imm8 forms
        let p = (h2 >> 10) & 1;
        let u = (h2 >> 9) & 1;
        let w = (h2 >> 8) & 1;
        let imm8 = h2 & 0xFF;
        if p == 0 && w == 0 {
            return undef(4, raw);
        }
        if hint {
            return nop();
        }
        let imm = if u == 1 { imm8 } else { imm8.wrapping_neg() };
        let mut fl = 0;
        if p == 1 {
            fl |= FL_IDX;
        }
        if w == 1 {
            fl |= FL_WB;
        }
        if rt == 15 {
            return ldr_to_pc_imm(rn, imm, fl, raw);
        }
        return op_rd_rn_imm(kimm, 4, raw, rt, rn, imm, fl);
    }
    if h2 & 0x0FC0 == 0 {
        if hint {
            return nop();
        }
        let rm = h2 & 0xF;
        let imm2 = (h2 >> 4) & 3;
        if rt == 15 {
            let mut o = Op::new(Kind::LdrToPc, 4, raw);
            o.rn = rn as u8;
            o.rm = rm as u8;
            o.ra = SHIFT_LSL;
            o.x = 1 | ((imm2 as u8) << 4);
            return o;
        }
        let mut o = op_rd_rn_rm(kreg, 4, raw, rt, rn, rm, 0);
        o.x = imm2 as u8;
        return o;
    }
    undef(4, raw)
}

fn ldr_to_pc_imm(rn: u32, imm: u32, flags: u8, raw: u32) -> Op {
    let mut o = Op::new(Kind::LdrToPc, 4, raw);
    o.rn = rn as u8;
    o.imm = imm;
    o.flags = flags;
    o.x = 0;
    o
}

fn decode_dp_register(h1: u32, h2: u32, raw: u32) -> Op {
    if h2 & 0xF000 != 0xF000 {
        return undef(4, raw);
    }
    let op1 = (h1 >> 4) & 0xF;
    let op2 = (h2 >> 4) & 0xF;
    let rn = h1 & 0xF;
    let rd = (h2 >> 8) & 0xF;
    let rm = h2 & 0xF;
    if op1 & 8 == 0 {
        if op2 == 0 {
            // register-controlled shifts
            let kind = match (op1 >> 1) & 3 {
                0 => Kind::LslReg,
                1 => Kind::LsrReg,
                2 => Kind::AsrReg,
                _ => Kind::RorReg,
            };
            let fl = if op1 & 1 != 0 { FL_S } else { 0 };
            return op_rd_rn_rm(kind, 4, raw, rd, rn, rm, fl);
        }
        if op2 & 0xC == 0x8 {
            let rot = ((h2 >> 4) & 3) * 8;
            let (plain, add) = match op1 & 7 {
                0 => (Kind::Sxth, Kind::Sxtah),
                1 => (Kind::Uxth, Kind::Uxtah),
                2 => (Kind::Sxtb16, Kind::Sxtab16),
                3 => (Kind::Uxtb16, Kind::Uxtab16),
                4 => (Kind::Sxtb, Kind::Sxtab),
                5 => (Kind::Uxtb, Kind::Uxtab),
                _ => return undef(4, raw),
            };
            let mut o = if rn == 15 { op_rd_rn_rm(plain, 4, raw, rd, 0, rm, 0) } else { op_rd_rn_rm(add, 4, raw, rd, rn, rm, 0) };
            o.x = rot as u8;
            return o;
        }
        return undef(4, raw);
    }
    if op2 & 8 == 0 {
        // parallel addition and subtraction
        let prefix = match (op2 >> 0) & 7 {
            0 => alu::PAR_S,
            1 => alu::PAR_Q,
            2 => alu::PAR_SH,
            4 => alu::PAR_U,
            5 => alu::PAR_UQ,
            6 => alu::PAR_UH,
            _ => return undef(4, raw),
        };
        let opidx = match op1 & 7 {
            0 => alu::PAR_ADD8,
            1 => alu::PAR_ADD16,
            2 => alu::PAR_ASX,
            4 => alu::PAR_SUB8,
            5 => alu::PAR_SUB16,
            6 => alu::PAR_SAX,
            _ => return undef(4, raw),
        };
        let mut o = op_rd_rn_rm(Kind::Parallel, 4, raw, rd, rn, rm, 0);
        o.x = prefix * 6 + opidx;
        return o;
    }
    if op2 & 0xC != 0x8 {
        return undef(4, raw);
    }
    let sub = (op2 & 3) as u8;
    match op1 {
        0b1000 => {
            let kind = match sub {
                0 => Kind::Qadd,
                1 => Kind::Qdadd,
                2 => Kind::Qsub,
                _ => Kind::Qdsub,
            };
            // QADD Rd, Rm, Rn: `rm` (hw2[3:0]) is the first operand, `rn` the second.
            op_rd_rn_rm(kind, 4, raw, rd, rn, rm, 0)
        }
        0b1001 => {
            if rn != rm {
                return undef(4, raw);
            }
            let kind = match sub {
                0 => Kind::Rev,
                1 => Kind::Rev16,
                2 => Kind::Rbit,
                _ => Kind::Revsh,
            };
            op_rd_rn_rm(kind, 4, raw, rd, 0, rm, 0)
        }
        0b1010 => {
            if sub != 0 {
                return undef(4, raw);
            }
            op_rd_rn_rm(Kind::Sel, 4, raw, rd, rn, rm, 0)
        }
        0b1011 => {
            if sub != 0 || rn != rm {
                return undef(4, raw);
            }
            op_rd_rn_rm(Kind::Clz, 4, raw, rd, 0, rm, 0)
        }
        _ => undef(4, raw),
    }
}

fn decode_multiply(h1: u32, h2: u32, raw: u32) -> Op {
    if h2 & 0x00C0 != 0 {
        return undef(4, raw);
    }
    let op1 = (h1 >> 4) & 7;
    let rn = h1 & 0xF;
    let ra = (h2 >> 12) & 0xF;
    let rd = (h2 >> 8) & 0xF;
    let op2 = (h2 >> 4) & 3;
    let rm = h2 & 0xF;
    let mut o = op_rd_rn_rm(Kind::Undefined, 4, raw, rd, rn, rm, 0);
    o.ra = ra as u8;
    let acc = ra != 15;
    o.kind = match op1 {
        0 => match op2 {
            0 => {
                if acc {
                    Kind::Mla
                } else {
                    Kind::Mul
                }
            }
            1 => Kind::Mls,
            _ => Kind::Undefined,
        },
        1 => {
            o.x = op2 as u8; // (N << 1) | M
            if acc {
                Kind::Smlaxy
            } else {
                Kind::Smulxy
            }
        }
        2 => {
            if op2 > 1 {
                Kind::Undefined
            } else {
                o.x = op2 as u8; // X
                if acc {
                    Kind::Smlad
                } else {
                    Kind::Smuad
                }
            }
        }
        3 => {
            if op2 > 1 {
                Kind::Undefined
            } else {
                o.x = op2 as u8; // M (T)
                if acc {
                    Kind::Smlawy
                } else {
                    Kind::Smulwy
                }
            }
        }
        4 => {
            if op2 > 1 {
                Kind::Undefined
            } else {
                o.x = op2 as u8;
                if acc {
                    Kind::Smlsd
                } else {
                    Kind::Smusd
                }
            }
        }
        5 => {
            if op2 > 1 {
                Kind::Undefined
            } else {
                o.x = op2 as u8; // R
                if acc {
                    Kind::Smmla
                } else {
                    Kind::Smmul
                }
            }
        }
        6 => {
            if op2 > 1 {
                Kind::Undefined
            } else {
                o.x = op2 as u8;
                Kind::Smmls
            }
        }
        _ => {
            if op2 != 0 {
                Kind::Undefined
            } else if acc {
                Kind::Usada8
            } else {
                Kind::Usad8
            }
        }
    };
    o
}

fn decode_long_multiply(h1: u32, h2: u32, raw: u32) -> Op {
    let op1 = (h1 >> 4) & 7;
    let op2 = (h2 >> 4) & 0xF;
    let rn = h1 & 0xF;
    let rm = h2 & 0xF;
    let rdlo = (h2 >> 12) & 0xF;
    let rdhi = (h2 >> 8) & 0xF;
    let long = |kind: Kind| {
        let mut o = op_rd_rn_rm(kind, 4, raw, rdlo, rn, rm, 0);
        o.ra = rdhi as u8;
        o
    };
    match (op1, op2) {
        (0, 0) => long(Kind::Smull),
        (1, 0xF) => {
            if rdlo != 15 {
                return undef(4, raw);
            }
            op_rd_rn_rm(Kind::Sdiv, 4, raw, rdhi, rn, rm, 0)
        }
        (2, 0) => long(Kind::Umull),
        (3, 0xF) => {
            if rdlo != 15 {
                return undef(4, raw);
            }
            op_rd_rn_rm(Kind::Udiv, 4, raw, rdhi, rn, rm, 0)
        }
        (4, 0) => long(Kind::Smlal),
        (4, 0x8..=0xB) => {
            let mut o = long(Kind::Smlalxy);
            o.x = (op2 & 3) as u8;
            o
        }
        (4, 0xC | 0xD) => {
            let mut o = long(Kind::Smlald);
            o.x = (op2 & 1) as u8;
            o
        }
        (5, 0xC | 0xD) => {
            let mut o = long(Kind::Smlsld);
            o.x = (op2 & 1) as u8;
            o
        }
        (6, 0) => long(Kind::Umlal),
        (6, 6) => long(Kind::Umaal),
        _ => undef(4, raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d16(hw: u16) -> Op {
        decode16(0x0800_0000, hw)
    }
    fn d32(h1: u16, h2: u16) -> Op {
        decode32(0x0800_0000, h1, h2)
    }

    #[test]
    fn sixteen_bit_basics() {
        // movs r0, #1
        let o = d16(0x2001);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::MovImm, 0, 1));
        assert_eq!(o.flags, FL_S | FL_IT);
        // adds r1, r2, r3
        let o = d16(0x18D1);
        assert_eq!((o.kind, o.rd, o.rn, o.rm), (Kind::AddReg, 1, 2, 3));
        // ldr r3, [pc, #8] at 0x08000000 -> literal at align(pc+4)+8
        let o = d16(0x4B02);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::LdrLit, 3, 0x0800_000C));
        // push {r4, lr}
        let o = d16(0xB510);
        assert_eq!((o.kind, o.imm), (Kind::Push, 0x4010));
        // pop {r4, pc}
        let o = d16(0xBD10);
        assert_eq!((o.kind, o.imm), (Kind::PopPc, 0x8010));
        let o = d16(0xBC10);
        assert_eq!((o.kind, o.imm), (Kind::Pop, 0x0010));
        // bx lr
        let o = d16(0x4770);
        assert_eq!((o.kind, o.rm), (Kind::Bx, 14));
        // b . (branch to self): pc+4 + (-2) = pc; tlib runs it as WFI
        let o = d16(0xE7FE);
        assert_eq!((o.kind, o.imm), (Kind::BSelf, 0x0800_0000));
        let o = d16(0xE7FD);
        assert_eq!((o.kind, o.imm), (Kind::B, 0x0800_0000 - 2));
        // beq +0 -> pc+4
        let o = d16(0xD000);
        assert_eq!((o.kind, o.x, o.imm), (Kind::Bcc, 0, 0x0800_0004));
        // svc 5
        let o = d16(0xDF05);
        assert_eq!((o.kind, o.imm), (Kind::Svc, 5));
        // udf
        assert_eq!(d16(0xDE00).kind, Kind::Undefined);
        // it eq / nop / wfi
        let o = d16(0xBF08);
        assert_eq!((o.kind, o.imm), (Kind::It, 0x08));
        assert_eq!(d16(0xBF00).kind, Kind::Nop);
        assert_eq!(d16(0xBF30).kind, Kind::Wfi);
        // cpsid i
        let o = d16(0xB672);
        assert_eq!((o.kind, o.x), (Kind::Cps, 0x12));
        // cbz r0, +2 (imm5 = 1): target = pc + 4 + 2
        let o = d16(0xB108);
        assert_eq!((o.kind, o.rn, o.imm), (Kind::Cbz, 0, 0x0800_0006));
        // cbnz r1, +0x42 (i = 1, imm5 = 1 -> 64 + 2)
        let o = d16(0xBB09);
        assert_eq!((o.kind, o.rn, o.imm), (Kind::Cbnz, 1, 0x0800_0004 + 66));
        // mov r8, r0 (no flags) / mov r0, pc
        let o = d16(0x4680);
        assert_eq!((o.kind, o.rd, o.rm, o.flags), (Kind::MovReg, 8, 0, 0));
        let o = d16(0x4678);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::MovImm, 0, 0x0800_0004));
        // add sp, #16 / sub sp, #16
        let o = d16(0xB004);
        assert_eq!((o.kind, o.rd, o.rn, o.imm), (Kind::AddImm, 13, 13, 16));
        let o = d16(0xB084);
        assert_eq!((o.kind, o.imm), (Kind::SubImm, 16));
    }

    #[test]
    fn thirty_two_bit_basics() {
        // bl: S=0, J1=J2=1 -> I1=I2=0, imm11=2 -> offset 4 -> target = pc + 4 + 4
        let o = d32(0xF000, 0xF802);
        assert_eq!((o.kind, o.imm), (Kind::Bl, 0x0800_0008));
        // movw r0, #0x1234
        let o = d32(0xF241, 0x2034);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::Movw, 0, 0x1234));
        // movt r0, #0x5678
        let o = d32(0xF2C5, 0x6078);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::Movt, 0, 0x5678_0000));
        // ldr.w r0, [r1, #4]
        let o = d32(0xF8D1, 0x0004);
        assert_eq!((o.kind, o.rd, o.rn, o.imm), (Kind::LdrImm, 0, 1, 4));
        // wfi.w / nop.w / dmb / isb
        assert_eq!(d32(0xF3AF, 0x8003).kind, Kind::Wfi);
        assert_eq!(d32(0xF3AF, 0x8000).kind, Kind::Nop);
        assert_eq!(d32(0xF3BF, 0x8F5F).kind, Kind::Barrier);
        assert_eq!(d32(0xF3BF, 0x8F6F).kind, Kind::Barrier);
        assert_eq!(d32(0xF3BF, 0x8F4F).kind, Kind::Barrier);
        // mrs r0, primask
        let o = d32(0xF3EF, 0x8010);
        assert_eq!((o.kind, o.rd, o.imm), (Kind::Mrs, 0, 0x10));
        // msr basepri, r0
        let o = d32(0xF380, 0x8811);
        assert_eq!((o.kind, o.rn, o.imm, o.x), (Kind::Msr, 0, 0x11, 2));
        // udf.w
        assert_eq!(d32(0xF7F0, 0xA000).kind, Kind::Undefined);
    }
}
