//! Execution of decoded FPv4-SP instructions.

use crate::decode::*;
use crate::fpscr::*;
use crate::ieee;
use crate::soft::SIGN;
use crate::{VfpExec, VfpFault, VfpHost, VfpInsn};

#[inline(always)]
fn base<H: VfpHost>(host: &H, rn: u8) -> u32 {
    if rn == 15 {
        host.literal_base()
    } else {
        host.reg(rn as u32)
    }
}

/// Executes a decoded instruction. Condition checks (IT blocks) are the core's job.
///
/// Kept out of line on purpose: it is monomorphized per host in the caller's
/// crate and keeping this body out of the interpreter loop keeps that loop small.
/// Never returns [`VfpExec::Undefined`] (the variant is reserved by the contract);
/// the only failure is [`VfpFault::Unaligned`], reported before any state changes.
#[inline(never)]
pub fn execute<H: VfpHost>(insn: &VfpInsn, host: &mut H) -> VfpExec {
    let d = (insn.d & 31) as usize;
    let n = (insn.n & 31) as usize;
    let m = (insn.m & 31) as usize;
    match insn.op {
        // ---- loads and stores -------------------------------------------------
        Op::Vldr => {
            let addr = base(host, insn.n).wrapping_add(insn.imm);
            if addr & 3 != 0 {
                return VfpExec::Fault(VfpFault::Unaligned(addr));
            }
            let v = host.load32(addr);
            host.fp().s[d] = v;
        }
        Op::Vstr => {
            let addr = base(host, insn.n).wrapping_add(insn.imm);
            if addr & 3 != 0 {
                return VfpExec::Fault(VfpFault::Unaligned(addr));
            }
            let v = host.fp().s[d];
            host.store32(addr, v);
        }
        Op::VldrD => {
            let addr = base(host, insn.n).wrapping_add(insn.imm);
            if addr & 3 != 0 {
                return VfpExec::Fault(VfpFault::Unaligned(addr));
            }
            let lo = host.load32(addr);
            let hi = host.load32(addr.wrapping_add(4));
            let fp = host.fp();
            fp.s[d] = lo;
            fp.s[(d + 1) & 31] = hi;
        }
        Op::VstrD => {
            let addr = base(host, insn.n).wrapping_add(insn.imm);
            if addr & 3 != 0 {
                return VfpExec::Fault(VfpFault::Unaligned(addr));
            }
            let (lo, hi) = {
                let fp = host.fp();
                (fp.s[d], fp.s[(d + 1) & 31])
            };
            host.store32(addr, lo);
            host.store32(addr.wrapping_add(4), hi);
        }
        Op::Vldm | Op::Vstm => {
            let rn = insn.n as u32;
            let b = host.reg(rn);
            let (start, new_base) = if insn.flags & F_DB != 0 {
                let s = b.wrapping_sub(insn.imm);
                (s, s)
            } else {
                (b, b.wrapping_add(insn.imm))
            };
            if start & 3 != 0 {
                return VfpExec::Fault(VfpFault::Unaligned(start));
            }
            let words = insn.m as u32;
            if insn.op == Op::Vldm {
                for i in 0..words {
                    let v = host.load32(start.wrapping_add(i * 4));
                    host.fp().s[(d + i as usize) & 31] = v;
                }
            } else {
                for i in 0..words {
                    let v = host.fp().s[(d + i as usize) & 31];
                    host.store32(start.wrapping_add(i * 4), v);
                }
            }
            if insn.flags & F_WBACK != 0 {
                host.set_reg(rn, new_base);
            }
        }

        // ---- moves ------------------------------------------------------------
        Op::VmovImm => host.fp().s[d] = insn.imm,
        Op::VmovReg => {
            let fp = host.fp();
            fp.s[d] = fp.s[m];
        }
        Op::VmovToS => {
            let v = host.reg(insn.n as u32);
            host.fp().s[d] = v;
        }
        Op::VmovFromS => {
            let v = host.fp().s[d];
            host.set_reg(insn.n as u32, v);
        }
        Op::Vmov2ToS => {
            let a = host.reg(insn.n as u32);
            let b = host.reg(insn.m as u32);
            let fp = host.fp();
            fp.s[d] = a;
            fp.s[(d + 1) & 31] = b;
        }
        Op::Vmov2FromS => {
            let (a, b) = {
                let fp = host.fp();
                (fp.s[d], fp.s[(d + 1) & 31])
            };
            host.set_reg(insn.n as u32, a);
            host.set_reg(insn.m as u32, b);
        }
        Op::Vmrs => {
            let v = host.fp().fpscr;
            if insn.d == 15 {
                host.set_apsr_nzcv(v);
            } else {
                host.set_reg(insn.d as u32, v);
            }
        }
        Op::Vmsr => {
            let v = host.reg(insn.n as u32);
            host.fp().fpscr = v & WRITE_MASK;
        }

        // ---- arithmetic -------------------------------------------------------
        Op::Vadd => {
            let fp = host.fp();
            fp.s[d] = ieee::add(fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vsub => {
            let fp = host.fp();
            fp.s[d] = ieee::sub(fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vmul => {
            let fp = host.fp();
            fp.s[d] = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vnmul => {
            let fp = host.fp();
            fp.s[d] = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr) ^ SIGN;
        }
        Op::Vdiv => {
            let fp = host.fp();
            fp.s[d] = ieee::div(fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vsqrt => {
            let fp = host.fp();
            fp.s[d] = ieee::sqrt(fp.s[m], &mut fp.fpscr);
        }
        Op::Vabs => {
            let fp = host.fp();
            fp.s[d] = fp.s[m] & !SIGN;
        }
        Op::Vneg => {
            let fp = host.fp();
            fp.s[d] = fp.s[m] ^ SIGN;
        }
        // Separately rounded multiply-accumulate: the product is rounded, then the sum.
        Op::Vmla => {
            let fp = host.fp();
            let p = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr);
            fp.s[d] = ieee::add(fp.s[d], p, &mut fp.fpscr);
        }
        Op::Vmls => {
            let fp = host.fp();
            let p = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr);
            fp.s[d] = ieee::add(fp.s[d], p ^ SIGN, &mut fp.fpscr);
        }
        Op::Vnmla => {
            let fp = host.fp();
            let p = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr);
            fp.s[d] = ieee::add(fp.s[d] ^ SIGN, p ^ SIGN, &mut fp.fpscr);
        }
        Op::Vnmls => {
            let fp = host.fp();
            let p = ieee::mul(fp.s[n], fp.s[m], &mut fp.fpscr);
            fp.s[d] = ieee::add(fp.s[d] ^ SIGN, p, &mut fp.fpscr);
        }
        // Fused multiply-accumulate (single rounding).
        Op::Vfma => {
            let fp = host.fp();
            fp.s[d] = ieee::fma(fp.s[d], fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vfms => {
            let fp = host.fp();
            fp.s[d] = ieee::fma(fp.s[d], fp.s[n] ^ SIGN, fp.s[m], &mut fp.fpscr);
        }
        Op::Vfnma => {
            let fp = host.fp();
            fp.s[d] = ieee::fma(fp.s[d] ^ SIGN, fp.s[n] ^ SIGN, fp.s[m], &mut fp.fpscr);
        }
        Op::Vfnms => {
            let fp = host.fp();
            fp.s[d] = ieee::fma(fp.s[d] ^ SIGN, fp.s[n], fp.s[m], &mut fp.fpscr);
        }
        Op::Vcmp => {
            let fp = host.fp();
            let rhs = if insn.flags & F_ZERO != 0 { 0 } else { fp.s[m] };
            let nzcv = ieee::compare(fp.s[d], rhs, insn.flags & F_E != 0, &mut fp.fpscr);
            fp.fpscr = (fp.fpscr & 0x0FFF_FFFF) | nzcv;
        }

        // ---- conversions ------------------------------------------------------
        Op::VcvtFromInt => {
            let fp = host.fp();
            fp.s[d] = ieee::from_int(fp.s[m], insn.flags & F_UNSIGNED != 0, &mut fp.fpscr);
        }
        Op::VcvtToInt => {
            let fp = host.fp();
            fp.s[d] = ieee::to_int(
                fp.s[m],
                insn.flags & F_UNSIGNED != 0,
                insn.flags & F_ROUND_ZERO != 0,
                &mut fp.fpscr,
            );
        }
        Op::VcvtFromFixed => {
            let fp = host.fp();
            let size = if insn.flags & F_SIZE32 != 0 { 32 } else { 16 };
            fp.s[d] = ieee::from_fixed(fp.s[d], size, insn.imm, insn.flags & F_UNSIGNED != 0, &mut fp.fpscr);
        }
        Op::VcvtToFixed => {
            let fp = host.fp();
            let size = if insn.flags & F_SIZE32 != 0 { 32 } else { 16 };
            fp.s[d] = ieee::to_fixed(fp.s[d], size, insn.imm, insn.flags & F_UNSIGNED != 0, &mut fp.fpscr);
        }
        Op::Vcvt16To32 => {
            let fp = host.fp();
            let src = fp.s[m];
            let h = if insn.flags & F_TOP != 0 { (src >> 16) as u16 } else { src as u16 };
            fp.s[d] = ieee::f16_to_f32(h, &mut fp.fpscr);
        }
        Op::Vcvt32To16 => {
            let fp = host.fp();
            let h = ieee::f32_to_f16(fp.s[m], &mut fp.fpscr) as u32;
            fp.s[d] = if insn.flags & F_TOP != 0 {
                (fp.s[d] & 0xFFFF) | (h << 16)
            } else {
                (fp.s[d] & 0xFFFF_0000) | h
            };
        }
    }
    VfpExec::Ok
}
