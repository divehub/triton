//! Debug disassembler for decoded FPv4-SP instructions.
//!
//! The text follows the layout used by the Ghidra listings of the separate
//! analysis workspace (lower case, no space
//! after commas, immediates in hex, `vmov.f32 s1,0x40400000` for expanded
//! floating-point immediates), without any IT/condition suffix because the
//! decoder does not know the condition.

use crate::decode::*;
use crate::{decode, VfpDecode, VfpInsn};

fn core_reg(r: u32) -> String {
    match r {
        13 => "sp".to_string(),
        14 => "lr".to_string(),
        15 => "pc".to_string(),
        _ => format!("r{r}"),
    }
}

fn mem_operand(rn: u8, off: u32) -> String {
    let off = off as i32;
    let rn = core_reg(rn as u32);
    match off {
        0 => format!("[{rn}]"),
        o if o > 0 => format!("[{rn},#0x{o:x}]"),
        o => format!("[{rn},#-0x{:x}]", -(o as i64)),
    }
}

fn reg_list(first_s: u32, words: u32, double: bool) -> String {
    let mut out = String::from("{");
    if double {
        for i in 0..words / 2 {
            if i != 0 {
                out.push(',');
            }
            out.push_str(&format!("d{}", first_s / 2 + i));
        }
    } else {
        for i in 0..words {
            if i != 0 {
                out.push(',');
            }
            out.push_str(&format!("s{}", first_s + i));
        }
    }
    out.push('}');
    out
}

impl VfpInsn {
    /// Disassembles the instruction in the Ghidra-compatible layout.
    pub fn disassemble(&self) -> String {
        let (d, n, m) = (self.d as u32, self.n as u32, self.m as u32);
        let f = self.flags;
        let three = |name: &str| format!("{name}.f32 s{d},s{n},s{m}");
        let two = |name: &str| format!("{name}.f32 s{d},s{m}");
        match self.op {
            Op::Vldr => format!("vldr.32 s{d},{}", mem_operand(self.n, self.imm)),
            Op::Vstr => format!("vstr.32 s{d},{}", mem_operand(self.n, self.imm)),
            Op::VldrD => format!("vldr.64 d{},{}", d / 2, mem_operand(self.n, self.imm)),
            Op::VstrD => format!("vstr.64 d{},{}", d / 2, mem_operand(self.n, self.imm)),
            Op::Vldm | Op::Vstm => {
                let load = self.op == Op::Vldm;
                let wback = f & F_WBACK != 0;
                let db = f & F_DB != 0;
                let list = reg_list(d, m, f & F_DOUBLE != 0);
                if n == 13 && wback && load && !db {
                    format!("vpop {list}")
                } else if n == 13 && wback && !load && db {
                    format!("vpush {list}")
                } else {
                    format!(
                        "{}{} {}{},{list}",
                        if load { "vldm" } else { "vstm" },
                        if db { "db" } else { "ia" },
                        core_reg(n),
                        if wback { "!" } else { "" }
                    )
                }
            }
            Op::VmovImm => format!("vmov.f32 s{d},0x{:08x}", self.imm),
            Op::VmovReg => two("vmov"),
            Op::VmovToS => {
                if f & F_SCALAR != 0 {
                    format!("vmov.32 d{}[{}],{}", d / 2, d & 1, core_reg(n))
                } else {
                    format!("vmov s{d},{}", core_reg(n))
                }
            }
            Op::VmovFromS => {
                if f & F_SCALAR != 0 {
                    format!("vmov.32 {},d{}[{}]", core_reg(n), d / 2, d & 1)
                } else {
                    format!("vmov {},s{d}", core_reg(n))
                }
            }
            Op::Vmov2ToS => {
                if f & F_DOUBLE != 0 {
                    format!("vmov d{},{},{}", d / 2, core_reg(n), core_reg(m))
                } else {
                    format!("vmov s{d},s{},{},{}", d + 1, core_reg(n), core_reg(m))
                }
            }
            Op::Vmov2FromS => {
                if f & F_DOUBLE != 0 {
                    format!("vmov {},{},d{}", core_reg(n), core_reg(m), d / 2)
                } else {
                    format!("vmov {},{},s{d},s{}", core_reg(n), core_reg(m), d + 1)
                }
            }
            Op::Vmrs => {
                if d == 15 {
                    "vmrs apsr,fpscr".to_string()
                } else {
                    format!("vmrs {},fpscr", core_reg(d))
                }
            }
            Op::Vmsr => format!("vmsr fpscr,{}", core_reg(n)),
            Op::Vadd => three("vadd"),
            Op::Vsub => three("vsub"),
            Op::Vmul => three("vmul"),
            Op::Vnmul => three("vnmul"),
            Op::Vdiv => three("vdiv"),
            Op::Vsqrt => two("vsqrt"),
            Op::Vabs => two("vabs"),
            Op::Vneg => two("vneg"),
            Op::Vmla => three("vmla"),
            Op::Vmls => three("vmls"),
            Op::Vnmla => three("vnmla"),
            Op::Vnmls => three("vnmls"),
            Op::Vfma => three("vfma"),
            Op::Vfms => three("vfms"),
            Op::Vfnma => three("vfnma"),
            Op::Vfnms => three("vfnms"),
            Op::Vcmp => {
                let name = if f & F_E != 0 { "vcmpe" } else { "vcmp" };
                if f & F_ZERO != 0 {
                    format!("{name}.f32 s{d},#0")
                } else {
                    two(name)
                }
            }
            Op::VcvtFromInt => {
                format!("vcvt.f32.{} s{d},s{m}", if f & F_UNSIGNED != 0 { "u32" } else { "s32" })
            }
            Op::VcvtToInt => format!(
                "{}.{}.f32 s{d},s{m}",
                if f & F_ROUND_ZERO != 0 { "vcvt" } else { "vcvtr" },
                if f & F_UNSIGNED != 0 { "u32" } else { "s32" }
            ),
            Op::VcvtFromFixed | Op::VcvtToFixed => {
                let ty = match (f & F_UNSIGNED != 0, f & F_SIZE32 != 0) {
                    (false, false) => "s16",
                    (true, false) => "u16",
                    (false, true) => "s32",
                    (true, true) => "u32",
                };
                if self.op == Op::VcvtFromFixed {
                    format!("vcvt.f32.{ty} s{d},s{d},#0x{:x}", self.imm)
                } else {
                    format!("vcvt.{ty}.f32 s{d},s{d},#0x{:x}", self.imm)
                }
            }
            Op::Vcvt16To32 => format!(
                "{}.f32.f16 s{d},s{m}",
                if f & F_TOP != 0 { "vcvtt" } else { "vcvtb" }
            ),
            Op::Vcvt32To16 => format!(
                "{}.f16.f32 s{d},s{m}",
                if f & F_TOP != 0 { "vcvtt" } else { "vcvtb" }
            ),
        }
    }
}

/// Disassembles a Thumb coprocessor-space encoding; `"undefined"` for
/// CP10/CP11 encodings FPv4-SP does not implement and `"(not vfp)"` for
/// other coprocessors.
pub fn disassemble(hw1: u16, hw2: u16) -> String {
    match decode(hw1, hw2) {
        VfpDecode::Insn(i) => i.disassemble(),
        VfpDecode::Undefined => "undefined".to_string(),
        VfpDecode::NotVfp => "(not vfp)".to_string(),
    }
}
