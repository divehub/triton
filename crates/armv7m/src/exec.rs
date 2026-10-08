//! Instruction execution: one dense `match` over the predecoded `Kind`.
//!
//! Conventions: `r[15]` already holds the address of the next instruction
//! when a handler runs (the run loop advanced it); `pc` is the address of the
//! current instruction. Handlers that fault restore `r[15] = pc` through
//! `usage_fault`. Memory accesses pass the retire count (instructions
//! completed before this one) to the bus.

use crate::alu::{self, shift_c, SHIFT_LSL};
use crate::cpu::*;
use crate::nvic::*;
use crate::op::*;
use crate::{vfp, CpuBus};

impl Cpu {
    #[inline(always)]
    pub(crate) fn carry(&self) -> bool {
        self.apsr & 0x2000_0000 != 0
    }

    #[inline(always)]
    fn set_nz(&mut self, v: u32) {
        self.apsr = (self.apsr & 0x3FFF_FFFF) | (v & 0x8000_0000) | (((v == 0) as u32) << 30);
    }

    #[inline(always)]
    fn set_nzc(&mut self, v: u32, c: bool) {
        self.apsr = (self.apsr & 0x1FFF_FFFF) | (v & 0x8000_0000) | (((v == 0) as u32) << 30) | ((c as u32) << 29);
    }

    #[inline(always)]
    fn set_nzcv(&mut self, v: u32, c: bool, o: bool) {
        self.apsr = (self.apsr & 0x0FFF_FFFF) | (v & 0x8000_0000) | (((v == 0) as u32) << 30) | ((c as u32) << 29) | ((o as u32) << 28);
    }

    #[inline(always)]
    fn set_q(&mut self) {
        self.apsr |= 1 << 27;
    }

    /// Non-inlined entry used by the slow paths (IT blocks, tracing, loop verification).
    #[inline(never)]
    pub(crate) fn exec_slow<B: CpuBus>(&mut self, bus: &mut B, op: &Op, pc: u32) {
        self.exec(bus, op.kind, op, pc)
    }

    #[inline(always)]
    pub(crate) fn exec<B: CpuBus>(&mut self, bus: &mut B, kind: Kind, op: &Op, pc: u32) {
        // Operand fields are read where they are used: most kinds need only one or two of them, and
        // extracting all of them before the dispatch costs host instructions on every guest instruction.
        macro_rules! rd {
            () => {
                ((op.rd & 15) as usize)
            };
        }
        macro_rules! rn {
            () => {
                ((op.rn & 15) as usize)
            };
        }
        macro_rules! rm {
            () => {
                ((op.rm & 15) as usize)
            };
        }
        macro_rules! imm {
            () => {
                op.imm
            };
        }
        macro_rules! fl {
            () => {
                op.flags
            };
        }

        // Value of the shifted register operand (value, carry-out).
        macro_rules! shifted {
            () => {{
                let v = self.r[rm!()];
                if op.x == 0 && op.ra == SHIFT_LSL {
                    (v, self.carry())
                } else {
                    shift_c(v, op.ra, op.x as u32, self.carry())
                }
            }};
        }
        // Shifted register operand, value only (arithmetic operations; RRX needs the carry-in).
        macro_rules! shifted_val {
            () => {{
                let v = self.r[rm!()];
                if op.x == 0 && op.ra == SHIFT_LSL {
                    v
                } else {
                    shift_c(v, op.ra, op.x as u32, self.carry()).0
                }
            }};
        }
        macro_rules! logic_imm {
            ($f:expr) => {{
                let a = self.r[rn!()];
                let res: u32 = $f(a, imm!());
                self.r[rd!()] = res;
                if fl!() & FL_S != 0 {
                    let c = if fl!() & FL_IMMC != 0 { imm!() >> 31 != 0 } else { self.carry() };
                    self.set_nzc(res, c);
                }
            }};
        }
        macro_rules! logic_reg {
            ($f:expr) => {{
                let (m, c) = shifted!();
                let res: u32 = $f(self.r[rn!()], m);
                self.r[rd!()] = res;
                if fl!() & FL_S != 0 {
                    self.set_nzc(res, c);
                }
            }};
        }
        // Arithmetic operations: `$a`, `$b`, `$cin` expressions -> result, flags.
        macro_rules! arith {
            ($a:expr, $b:expr, $cin:expr) => {{
                let (res, c, v) = alu::add_with_carry($a, $b, $cin);
                let res = if fl!() & FL_SPMASK != 0 { res & !3 } else { res };
                self.r[rd!()] = res;
                if fl!() & FL_S != 0 {
                    self.set_nzcv(res, c, v);
                }
            }};
        }
        macro_rules! compare {
            ($a:expr, $b:expr, $cin:expr) => {{
                let (res, c, v) = alu::add_with_carry($a, $b, $cin);
                self.set_nzcv(res, c, v);
            }};
        }

        match kind {
            Kind::Undecoded | Kind::Undefined | Kind::Coproc => {
                self.log_undefined(pc, op.raw);
                self.usage_fault(CFSR_UNDEFINSTR, pc)
            }
            Kind::Nocp => self.usage_fault(CFSR_NOCP, pc),
            Kind::CutHead => {
                // Normally replaced by the wrapped instruction's kind in the hot loop.
                let inner = self.cut_entries[op.raw as usize].inner;
                self.exec_slow(bus, &inner, pc);
            }
            Kind::PageEnd => {
                // The last instruction of a 1 KiB page: run the wrapped instruction (the wrapper only
                // carries the TB-end property into the hot loop).
                let inner = self.page_ops[op.imm as usize];
                self.exec_slow(bus, &inner, pc);
            }
            Kind::Vfp | Kind::VfpEnd => self.exec_vfp(bus, op, pc),
            Kind::Nop | Kind::Barrier => {}
            Kind::Wfi => self.do_wfi(false),
            Kind::Wfe => self.do_wfi(true),
            Kind::BSelf => {
                // Renode parity: a branch-to-self is executed as WFI and retried when the core wakes.
                self.r[15] = pc;
                self.do_wfi(false);
            }
            Kind::Sev => self.event_flag = true,
            Kind::Svc => {
                // The return address is the next instruction (already in r15).
                self.raise_sync(EXC_SVCALL);
            }
            Kind::Bkpt => {
                // Renode: BKPT raises the DebugMonitor exception; the return address is the BKPT itself.
                self.r[15] = pc;
                self.insn_faulted = true;
                self.raise_sync(EXC_DEBUGMON);
            }
            Kind::Cps => self.exec_cps(op.x as u32),
            Kind::Mrs => {
                let v = self.mrs_read(imm!());
                self.r[rd!()] = v;
            }
            Kind::Msr => {
                let v = self.r[rn!()];
                self.msr_write(imm!(), op.x as u32, v);
            }
            Kind::Clrex => self.exclusive = None,
            Kind::It => {
                self.itstate = imm!() as u8;
                self.kick();
            }

            // ---- branches ----------------------------------------------------------------
            Kind::B => self.branch_direct(pc, imm!(), fl!()),
            Kind::Bcc => {
                if cond_holds(op.x as u32, self.apsr) {
                    self.branch_direct(pc, imm!(), fl!());
                }
            }
            Kind::Bl => {
                self.r[14] = pc.wrapping_add(4) | 1;
                self.r[15] = imm!();
            }
            Kind::Bx => {
                let t = self.r[rm!()];
                self.bx_write(bus, t);
            }
            Kind::Blx => {
                let t = self.r[rm!()];
                self.r[14] = pc.wrapping_add(2) | 1;
                self.blx_write(t);
            }
            Kind::Cbz => {
                if self.r[rn!()] == 0 {
                    self.branch_direct(pc, imm!(), fl!());
                }
            }
            Kind::Cbnz => {
                if self.r[rn!()] != 0 {
                    self.branch_direct(pc, imm!(), fl!());
                }
            }
            Kind::Tbb | Kind::Tbh => {
                let base = if rn!() == 15 { imm!() } else { self.r[rn!()] };
                let idx = self.r[rm!()];
                let half = if kind == Kind::Tbb {
                    self.ld8(bus, base.wrapping_add(idx))
                } else {
                    self.ld16(bus, base.wrapping_add(idx << 1))
                };
                self.r[15] = imm!().wrapping_add(half << 1);
            }
            Kind::MovToPc => self.r[15] = self.r[rm!()] & !1,
            Kind::AddToPc => self.r[15] = pc.wrapping_add(4).wrapping_add(self.r[rm!()]) & !1,

            // ---- data processing, immediate ---------------------------------------------------
            Kind::MovImm => {
                self.r[rd!()] = imm!();
                if fl!() & FL_S != 0 {
                    let c = if fl!() & FL_IMMC != 0 { imm!() >> 31 != 0 } else { self.carry() };
                    self.set_nzc(imm!(), c);
                }
            }
            Kind::MvnImm => {
                let v = !imm!();
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    let c = if fl!() & FL_IMMC != 0 { imm!() >> 31 != 0 } else { self.carry() };
                    self.set_nzc(v, c);
                }
            }
            Kind::Movw => self.r[rd!()] = imm!(),
            Kind::Movt => self.r[rd!()] = (self.r[rd!()] & 0xFFFF) | imm!(),
            Kind::AndImm => logic_imm!(|a: u32, b: u32| a & b),
            Kind::BicImm => logic_imm!(|a: u32, b: u32| a & !b),
            Kind::OrrImm => logic_imm!(|a: u32, b: u32| a | b),
            Kind::OrnImm => logic_imm!(|a: u32, b: u32| a | !b),
            Kind::EorImm => logic_imm!(|a: u32, b: u32| a ^ b),
            Kind::TstImm => {
                let res = self.r[rn!()] & imm!();
                let c = if fl!() & FL_IMMC != 0 { imm!() >> 31 != 0 } else { self.carry() };
                self.set_nzc(res, c);
            }
            Kind::TeqImm => {
                let res = self.r[rn!()] ^ imm!();
                let c = if fl!() & FL_IMMC != 0 { imm!() >> 31 != 0 } else { self.carry() };
                self.set_nzc(res, c);
            }
            Kind::AddImm => {
                let a = self.r[rn!()];
                if fl!() & (FL_S | FL_SPMASK) == 0 {
                    self.r[rd!()] = a.wrapping_add(imm!());
                } else {
                    arith!(a, imm!(), false);
                }
            }
            Kind::SubImm => {
                let a = self.r[rn!()];
                if fl!() & (FL_S | FL_SPMASK) == 0 {
                    self.r[rd!()] = a.wrapping_sub(imm!());
                } else {
                    arith!(a, !imm!(), true);
                }
            }
            Kind::AdcImm => arith!(self.r[rn!()], imm!(), self.carry()),
            Kind::SbcImm => arith!(self.r[rn!()], !imm!(), self.carry()),
            Kind::RsbImm => arith!(!self.r[rn!()], imm!(), true),
            Kind::CmpImm => compare!(self.r[rn!()], !imm!(), true),
            Kind::CmnImm => compare!(self.r[rn!()], imm!(), false),

            // ---- data processing, register -------------------------------------------------------
            Kind::MovReg => {
                let v = self.r[rm!()];
                if fl!() & (FL_S | FL_SPMASK) == 0 {
                    self.r[rd!()] = v;
                } else {
                    let v = if fl!() & FL_SPMASK != 0 { v & !3 } else { v };
                    self.r[rd!()] = v;
                    if fl!() & FL_S != 0 {
                        self.set_nz(v);
                    }
                }
            }
            Kind::MvnReg => {
                let (m, c) = shifted!();
                let v = !m;
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::AndReg => logic_reg!(|a: u32, b: u32| a & b),
            Kind::BicReg => logic_reg!(|a: u32, b: u32| a & !b),
            Kind::OrrReg => logic_reg!(|a: u32, b: u32| a | b),
            Kind::OrnReg => logic_reg!(|a: u32, b: u32| a | !b),
            Kind::EorReg => logic_reg!(|a: u32, b: u32| a ^ b),
            Kind::TstReg => {
                let (m, c) = shifted!();
                self.set_nzc(self.r[rn!()] & m, c);
            }
            Kind::TeqReg => {
                let (m, c) = shifted!();
                self.set_nzc(self.r[rn!()] ^ m, c);
            }
            Kind::AddReg => {
                let m = shifted_val!();
                let a = self.r[rn!()];
                if fl!() & (FL_S | FL_SPMASK) == 0 {
                    self.r[rd!()] = a.wrapping_add(m);
                } else {
                    arith!(a, m, false);
                }
            }
            Kind::SubReg => {
                let m = shifted_val!();
                let a = self.r[rn!()];
                if fl!() & (FL_S | FL_SPMASK) == 0 {
                    self.r[rd!()] = a.wrapping_sub(m);
                } else {
                    arith!(a, !m, true);
                }
            }
            Kind::AdcReg => {
                let m = shifted_val!();
                arith!(self.r[rn!()], m, self.carry());
            }
            Kind::SbcReg => {
                let m = shifted_val!();
                arith!(self.r[rn!()], !m, self.carry());
            }
            Kind::RsbReg => {
                let m = shifted_val!();
                arith!(!self.r[rn!()], m, true);
            }
            Kind::CmpReg => {
                let m = shifted_val!();
                compare!(self.r[rn!()], !m, true);
            }
            Kind::CmnReg => {
                let m = shifted_val!();
                compare!(self.r[rn!()], m, false);
            }
            Kind::LslImm => {
                let (v, c) = alu::lsl_c(self.r[rm!()], op.x as u32);
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::LsrImm => {
                let (v, c) = alu::lsr_c(self.r[rm!()], op.x as u32);
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::AsrImm => {
                let (v, c) = alu::asr_c(self.r[rm!()], op.x as u32);
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::RorImm => {
                let (v, c) = alu::ror_c(self.r[rm!()], op.x as u32);
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::Rrx => {
                let (v, c) = alu::rrx_c(self.r[rm!()], self.carry());
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }
            Kind::LslReg | Kind::LsrReg | Kind::AsrReg | Kind::RorReg => {
                let ty = match kind {
                    Kind::LslReg => alu::SHIFT_LSL,
                    Kind::LsrReg => alu::SHIFT_LSR,
                    Kind::AsrReg => alu::SHIFT_ASR,
                    _ => alu::SHIFT_ROR,
                };
                let amount = self.r[rm!()] & 0xFF;
                let (v, c) = shift_c(self.r[rn!()], ty, amount, self.carry());
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nzc(v, c);
                }
            }

            // ---- multiply / divide ------------------------------------------------------------------
            Kind::Mul => {
                let v = self.r[rn!()].wrapping_mul(self.r[rm!()]);
                self.r[rd!()] = v;
                if fl!() & FL_S != 0 {
                    self.set_nz(v);
                }
            }
            Kind::Mla => self.r[rd!()] = self.r[rn!()].wrapping_mul(self.r[rm!()]).wrapping_add(self.r[(op.ra & 15) as usize]),
            Kind::Mls => self.r[rd!()] = self.r[(op.ra & 15) as usize].wrapping_sub(self.r[rn!()].wrapping_mul(self.r[rm!()])),
            Kind::Umull | Kind::Smull | Kind::Umlal | Kind::Smlal | Kind::Umaal => {
                let rdhi = (op.ra & 15) as usize;
                let (a, b) = (self.r[rn!()], self.r[rm!()]);
                let acc = ((self.r[rdhi] as u64) << 32) | self.r[rd!()] as u64;
                let res: u64 = match kind {
                    Kind::Umull => (a as u64).wrapping_mul(b as u64),
                    Kind::Smull => (a as i32 as i64).wrapping_mul(b as i32 as i64) as u64,
                    Kind::Umlal => (a as u64).wrapping_mul(b as u64).wrapping_add(acc),
                    Kind::Smlal => (a as i32 as i64).wrapping_mul(b as i32 as i64).wrapping_add(acc as i64) as u64,
                    _ => (a as u64).wrapping_mul(b as u64).wrapping_add(self.r[rdhi] as u64).wrapping_add(self.r[rd!()] as u64),
                };
                self.r[rd!()] = res as u32;
                self.r[rdhi] = (res >> 32) as u32;
            }
            Kind::Sdiv | Kind::Udiv => {
                let (a, b) = (self.r[rn!()], self.r[rm!()]);
                if b == 0 && self.scb.ccr & CCR_DIV_0_TRP != 0 {
                    self.usage_fault(CFSR_DIVBYZERO, pc);
                } else {
                    self.r[rd!()] = if kind == Kind::Sdiv { alu::sdiv(a, b) } else { alu::udiv(a, b) };
                }
            }

            // ---- extend ------------------------------------------------------------------------------------
            Kind::Sxtb => self.r[rd!()] = alu::extend(alu::ExtKind::Sxtb, self.r[rm!()], op.x as u32),
            Kind::Sxth => self.r[rd!()] = alu::extend(alu::ExtKind::Sxth, self.r[rm!()], op.x as u32),
            Kind::Uxtb => self.r[rd!()] = alu::extend(alu::ExtKind::Uxtb, self.r[rm!()], op.x as u32),
            Kind::Uxth => self.r[rd!()] = alu::extend(alu::ExtKind::Uxth, self.r[rm!()], op.x as u32),
            Kind::Sxtab => self.r[rd!()] = self.r[rn!()].wrapping_add(alu::extend(alu::ExtKind::Sxtb, self.r[rm!()], op.x as u32)),
            Kind::Sxtah => self.r[rd!()] = self.r[rn!()].wrapping_add(alu::extend(alu::ExtKind::Sxth, self.r[rm!()], op.x as u32)),
            Kind::Uxtab => self.r[rd!()] = self.r[rn!()].wrapping_add(alu::extend(alu::ExtKind::Uxtb, self.r[rm!()], op.x as u32)),
            Kind::Uxtah => self.r[rd!()] = self.r[rn!()].wrapping_add(alu::extend(alu::ExtKind::Uxth, self.r[rm!()], op.x as u32)),
            Kind::Sxtb16 => self.r[rd!()] = alu::extend_b16(true, self.r[rm!()].rotate_right(op.x as u32)),
            Kind::Uxtb16 => self.r[rd!()] = alu::extend_b16(false, self.r[rm!()].rotate_right(op.x as u32)),
            Kind::Sxtab16 | Kind::Uxtab16 => {
                let e = alu::extend_b16(kind == Kind::Sxtab16, self.r[rm!()].rotate_right(op.x as u32));
                let a = self.r[rn!()];
                let lo = (a & 0xFFFF).wrapping_add(e & 0xFFFF) & 0xFFFF;
                let hi = (a >> 16).wrapping_add(e >> 16) & 0xFFFF;
                self.r[rd!()] = lo | (hi << 16);
            }
            Kind::Rev => self.r[rd!()] = self.r[rm!()].swap_bytes(),
            Kind::Rev16 => self.r[rd!()] = alu::rev16(self.r[rm!()]),
            Kind::Revsh => self.r[rd!()] = alu::revsh(self.r[rm!()]),
            Kind::Rbit => self.r[rd!()] = alu::rbit(self.r[rm!()]),
            Kind::Clz => self.r[rd!()] = self.r[rm!()].leading_zeros(),

            // ---- bit fields, saturation ---------------------------------------------------------------------------
            Kind::Bfi | Kind::Bfc => {
                let lsb = op.x as u32;
                let msb = op.ra as u32;
                let width = msb - lsb + 1;
                let mask = (if width >= 32 { u32::MAX } else { (1u32 << width) - 1 }) << lsb;
                let src = if kind == Kind::Bfc { 0 } else { self.r[rn!()] << lsb };
                self.r[rd!()] = (self.r[rd!()] & !mask) | (src & mask);
            }
            Kind::Ubfx => {
                let w = op.ra as u32 + 1;
                let mask = if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
                self.r[rd!()] = (self.r[rn!()] >> op.x as u32) & mask;
            }
            Kind::Sbfx => {
                let w = op.ra as u32 + 1;
                let v = (self.r[rn!()] >> op.x as u32) & if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
                self.r[rd!()] = alu::sign_extend(v, w);
            }
            Kind::Ssat | Kind::Usat => {
                let (v, _) = shift_c(self.r[rn!()], op.ra, op.x as u32, false);
                let (res, sat) = if kind == Kind::Ssat {
                    let (r, s) = alu::signed_sat_q(v as i32 as i64, imm!());
                    (r as u32, s)
                } else {
                    alu::unsigned_sat_q(v as i32 as i64, imm!())
                };
                self.r[rd!()] = res;
                if sat {
                    self.set_q();
                }
            }
            Kind::Ssat16 | Kind::Usat16 => {
                let v = self.r[rn!()];
                let (lo, hi) = (v as u16 as i16 as i64, (v >> 16) as u16 as i16 as i64);
                let (r0, s0, r1, s1) = if kind == Kind::Ssat16 {
                    let (a, sa) = alu::signed_sat_q(lo, imm!());
                    let (b, sb) = alu::signed_sat_q(hi, imm!());
                    (a as u32 & 0xFFFF, sa, b as u32 & 0xFFFF, sb)
                } else {
                    let (a, sa) = alu::unsigned_sat_q(lo, imm!());
                    let (b, sb) = alu::unsigned_sat_q(hi, imm!());
                    (a & 0xFFFF, sa, b & 0xFFFF, sb)
                };
                self.r[rd!()] = r0 | (r1 << 16);
                if s0 || s1 {
                    self.set_q();
                }
            }
            Kind::Qadd | Kind::Qsub | Kind::Qdadd | Kind::Qdsub => {
                let (a, b) = (self.r[rm!()] as i32, self.r[rn!()] as i32);
                let (res, sat) = match kind {
                    Kind::Qadd => alu::sat_add_i32(a, b),
                    Kind::Qsub => alu::sat_sub_i32(a, b),
                    Kind::Qdadd => {
                        let (d, s1) = alu::sat_add_i32(b, b);
                        let (r, s2) = alu::sat_add_i32(a, d);
                        (r, s1 | s2)
                    }
                    _ => {
                        let (d, s1) = alu::sat_add_i32(b, b);
                        let (r, s2) = alu::sat_sub_i32(a, d);
                        (r, s1 | s2)
                    }
                };
                self.r[rd!()] = res as u32;
                if sat {
                    self.set_q();
                }
            }
            Kind::Pkhbt => {
                let (sh, _) = shift_c(self.r[rm!()], alu::SHIFT_LSL, op.x as u32, false);
                self.r[rd!()] = (sh & 0xFFFF_0000) | (self.r[rn!()] & 0xFFFF);
            }
            Kind::Pkhtb => {
                let (sh, _) = shift_c(self.r[rm!()], alu::SHIFT_ASR, op.x as u32, false);
                self.r[rd!()] = (self.r[rn!()] & 0xFFFF_0000) | (sh & 0xFFFF);
            }
            Kind::Parallel => {
                let (res, ge) = alu::parallel_addsub(op.x, self.r[rn!()], self.r[rm!()]);
                self.r[rd!()] = res;
                if let Some(g) = ge {
                    self.ge = g;
                }
            }
            Kind::Usad8 => self.r[rd!()] = alu::usad8(self.r[rn!()], self.r[rm!()]),
            Kind::Usada8 => self.r[rd!()] = alu::usad8(self.r[rn!()], self.r[rm!()]).wrapping_add(self.r[(op.ra & 15) as usize]),
            Kind::Sel => self.r[rd!()] = alu::sel(self.ge, self.r[rn!()], self.r[rm!()]),

            // ---- DSP multiplies --------------------------------------------------------------------------------------------
            Kind::Smulxy | Kind::Smlaxy => {
                let x = op.x as u32;
                let a = half_s(self.r[rn!()], x >> 1 & 1);
                let b = half_s(self.r[rm!()], x & 1);
                let p = (a * b) as i64;
                if kind == Kind::Smulxy {
                    self.r[rd!()] = p as u32;
                } else {
                    let sum = p + self.r[(op.ra & 15) as usize] as i32 as i64;
                    self.r[rd!()] = sum as u32;
                    if sum != sum as i32 as i64 {
                        self.set_q();
                    }
                }
            }
            Kind::Smulwy | Kind::Smlawy => {
                let b = half_s(self.r[rm!()], op.x as u32 & 1) as i64;
                let prod = self.r[rn!()] as i32 as i64 * b;
                if kind == Kind::Smulwy {
                    self.r[rd!()] = (prod >> 16) as u32;
                } else {
                    let result = prod + ((self.r[(op.ra & 15) as usize] as i32 as i64) << 16);
                    let r = result >> 16;
                    self.r[rd!()] = r as u32;
                    if r != r as i32 as i64 {
                        self.set_q();
                    }
                }
            }
            Kind::Smlalxy => {
                let rdhi = (op.ra & 15) as usize;
                let x = op.x as u32;
                let a = half_s(self.r[rn!()], x >> 1 & 1) as i64;
                let b = half_s(self.r[rm!()], x & 1) as i64;
                let acc = (((self.r[rdhi] as u64) << 32) | self.r[rd!()] as u64) as i64;
                let res = acc.wrapping_add(a * b) as u64;
                self.r[rd!()] = res as u32;
                self.r[rdhi] = (res >> 32) as u32;
            }
            Kind::Smuad | Kind::Smusd | Kind::Smlad | Kind::Smlsd => {
                let a = self.r[rn!()];
                let b = if op.x & 1 != 0 { self.r[rm!()].rotate_right(16) } else { self.r[rm!()] };
                let p1 = (a as u16 as i16 as i64) * (b as u16 as i16 as i64);
                let p2 = ((a >> 16) as u16 as i16 as i64) * ((b >> 16) as u16 as i16 as i64);
                let mut sum = if matches!(kind, Kind::Smuad | Kind::Smlad) { p1 + p2 } else { p1 - p2 };
                if matches!(kind, Kind::Smlad | Kind::Smlsd) {
                    sum += self.r[(op.ra & 15) as usize] as i32 as i64;
                }
                self.r[rd!()] = sum as u32;
                if sum != sum as i32 as i64 {
                    self.set_q();
                }
            }
            Kind::Smlald | Kind::Smlsld => {
                let rdhi = (op.ra & 15) as usize;
                let a = self.r[rn!()];
                let b = if op.x & 1 != 0 { self.r[rm!()].rotate_right(16) } else { self.r[rm!()] };
                let p1 = (a as u16 as i16 as i64) * (b as u16 as i16 as i64);
                let p2 = ((a >> 16) as u16 as i16 as i64) * ((b >> 16) as u16 as i16 as i64);
                let acc = (((self.r[rdhi] as u64) << 32) | self.r[rd!()] as u64) as i64;
                let d = if kind == Kind::Smlald { p1 + p2 } else { p1 - p2 };
                let res = acc.wrapping_add(d) as u64;
                self.r[rd!()] = res as u32;
                self.r[rdhi] = (res >> 32) as u32;
            }
            Kind::Smmul | Kind::Smmla | Kind::Smmls => {
                let prod = self.r[rn!()] as i32 as i64 * self.r[rm!()] as i32 as i64;
                let round = if op.x & 1 != 0 { 0x8000_0000i64 } else { 0 };
                let res = match kind {
                    Kind::Smmul => prod.wrapping_add(round),
                    Kind::Smmla => (((self.r[(op.ra & 15) as usize] as i32 as i64) << 32).wrapping_add(prod)).wrapping_add(round),
                    _ => (((self.r[(op.ra & 15) as usize] as i32 as i64) << 32).wrapping_sub(prod)).wrapping_add(round),
                };
                self.r[rd!()] = (res >> 32) as u32;
            }

            // ---- loads and stores -----------------------------------------------------------------------------------------------
            Kind::LdrImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                if !self.unaligned_ok(addr, 3, pc) {
                    return;
                }
                let v = self.ld32(bus, addr);
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
                self.r[rd!()] = v;
            }
            Kind::LdrbImm | Kind::LdrsbImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                let v = self.ld8(bus, addr);
                let v = if kind == Kind::LdrsbImm { v as u8 as i8 as i32 as u32 } else { v };
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
                self.r[rd!()] = v;
            }
            Kind::LdrhImm | Kind::LdrshImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                if !self.unaligned_ok(addr, 1, pc) {
                    return;
                }
                let v = self.ld16(bus, addr);
                let v = if kind == Kind::LdrshImm { v as u16 as i16 as i32 as u32 } else { v };
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
                self.r[rd!()] = v;
            }
            Kind::StrImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                if !self.unaligned_ok(addr, 3, pc) {
                    return;
                }
                let v = self.r[rd!()];
                self.st32(bus, addr, v);
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
            }
            Kind::StrbImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                let v = self.r[rd!()];
                self.st8(bus, addr, v);
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
            }
            Kind::StrhImm => {
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                if !self.unaligned_ok(addr, 1, pc) {
                    return;
                }
                let v = self.r[rd!()];
                self.st16(bus, addr, v);
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
            }
            Kind::LdrReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                if !self.unaligned_ok(addr, 3, pc) {
                    return;
                }
                self.r[rd!()] = self.ld32(bus, addr);
            }
            Kind::LdrbReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                self.r[rd!()] = self.ld8(bus, addr);
            }
            Kind::LdrsbReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                self.r[rd!()] = self.ld8(bus, addr) as u8 as i8 as i32 as u32;
            }
            Kind::LdrhReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                if !self.unaligned_ok(addr, 1, pc) {
                    return;
                }
                self.r[rd!()] = self.ld16(bus, addr);
            }
            Kind::LdrshReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                if !self.unaligned_ok(addr, 1, pc) {
                    return;
                }
                self.r[rd!()] = self.ld16(bus, addr) as u16 as i16 as i32 as u32;
            }
            Kind::StrReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                if !self.unaligned_ok(addr, 3, pc) {
                    return;
                }
                let v = self.r[rd!()];
                self.st32(bus, addr, v);
            }
            Kind::StrbReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                let v = self.r[rd!()];
                self.st8(bus, addr, v);
            }
            Kind::StrhReg => {
                let addr = self.r[rn!()].wrapping_add(self.r[rm!()] << op.x);
                if !self.unaligned_ok(addr, 1, pc) {
                    return;
                }
                let v = self.r[rd!()];
                self.st16(bus, addr, v);
            }
            Kind::LdrLit => {
                if !self.unaligned_ok(imm!(), 3, pc) {
                    return;
                }
                self.r[rd!()] = self.ld32(bus, imm!());
            }
            Kind::LdrbLit => self.r[rd!()] = self.ld8(bus, imm!()),
            Kind::LdrsbLit => self.r[rd!()] = self.ld8(bus, imm!()) as u8 as i8 as i32 as u32,
            Kind::LdrhLit => {
                if !self.unaligned_ok(imm!(), 1, pc) {
                    return;
                }
                self.r[rd!()] = self.ld16(bus, imm!());
            }
            Kind::LdrshLit => {
                if !self.unaligned_ok(imm!(), 1, pc) {
                    return;
                }
                self.r[rd!()] = self.ld16(bus, imm!()) as u16 as i16 as i32 as u32;
            }
            Kind::LdrToPc => {
                let mut wb: Option<u32> = None;
                let addr = match op.x & 15 {
                    0 => {
                        let base = self.r[rn!()];
                        let off = base.wrapping_add(imm!());
                        if fl!() & FL_WB != 0 {
                            wb = Some(off);
                        }
                        if fl!() & FL_IDX != 0 {
                            off
                        } else {
                            base
                        }
                    }
                    1 => self.r[rn!()].wrapping_add(self.r[rm!()] << (op.x >> 4)),
                    _ => imm!(),
                };
                if !self.unaligned_ok(addr, 3, pc) {
                    return;
                }
                let v = self.ld32(bus, addr);
                if let Some(w) = wb {
                    self.r[rn!()] = w;
                }
                self.bx_write(bus, v);
            }
            Kind::LdrdImm => {
                let rt2 = (op.ra & 15) as usize;
                let (addr, wbv) = if rn!() == 15 {
                    (imm!(), 0)
                } else {
                    let base = self.r[rn!()];
                    let off = base.wrapping_add(imm!());
                    (if fl!() & FL_IDX != 0 { off } else { base }, off)
                };
                if !self.require_aligned(addr, pc) {
                    return;
                }
                let v1 = self.ld32(bus, addr);
                let v2 = self.ld32(bus, addr.wrapping_add(4));
                if fl!() & FL_WB != 0 && rn!() != 15 {
                    self.r[rn!()] = wbv;
                }
                self.r[rd!()] = v1;
                self.r[rt2] = v2;
            }
            Kind::StrdImm => {
                let rt2 = (op.ra & 15) as usize;
                let base = self.r[rn!()];
                let off = base.wrapping_add(imm!());
                let addr = if fl!() & FL_IDX != 0 { off } else { base };
                if !self.require_aligned(addr, pc) {
                    return;
                }
                let (v1, v2) = (self.r[rd!()], self.r[rt2]);
                self.st32(bus, addr, v1);
                self.st32(bus, addr.wrapping_add(4), v2);
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = off;
                }
            }
            Kind::Ldm | Kind::LdmPc => {
                let n = imm!().count_ones();
                let base = self.r[rn!()];
                let (start, wbv) = if fl!() & FL_DB != 0 {
                    let s = base.wrapping_sub(4 * n);
                    (s, s)
                } else {
                    (base, base.wrapping_add(4 * n))
                };
                if !self.require_aligned(start, pc) {
                    return;
                }
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = wbv;
                }
                let mut a = start;
                for i in 0..15 {
                    if imm!() & (1 << i) != 0 {
                        let v = self.ld32(bus, a);
                        self.r[i] = v;
                        a = a.wrapping_add(4);
                    }
                }
                if imm!() & 0x8000 != 0 {
                    let v = self.ld32(bus, a);
                    self.bx_write(bus, v);
                }
            }
            Kind::Stm => {
                let n = imm!().count_ones();
                let base = self.r[rn!()];
                let (start, wbv) = if fl!() & FL_DB != 0 {
                    let s = base.wrapping_sub(4 * n);
                    (s, s)
                } else {
                    (base, base.wrapping_add(4 * n))
                };
                if !self.require_aligned(start, pc) {
                    return;
                }
                let mut a = start;
                for i in 0..15 {
                    if imm!() & (1 << i) != 0 {
                        let v = self.r[i];
                        self.st32(bus, a, v);
                        a = a.wrapping_add(4);
                    }
                }
                if fl!() & FL_WB != 0 {
                    self.r[rn!()] = wbv;
                }
            }
            Kind::Push => {
                let n = imm!().count_ones();
                let start = self.r[13].wrapping_sub(4 * n);
                if !self.require_aligned(start, pc) {
                    return;
                }
                let mut a = start;
                for i in 0..15 {
                    if imm!() & (1 << i) != 0 {
                        let v = self.r[i];
                        self.st32(bus, a, v);
                        a = a.wrapping_add(4);
                    }
                }
                self.r[13] = start;
            }
            Kind::Pop | Kind::PopPc => {
                let n = imm!().count_ones();
                let start = self.r[13];
                if !self.require_aligned(start, pc) {
                    return;
                }
                let mut a = start;
                for i in 0..15 {
                    if imm!() & (1 << i) != 0 {
                        let v = self.ld32(bus, a);
                        self.r[i] = v;
                        a = a.wrapping_add(4);
                    }
                }
                if imm!() & 0x8000 != 0 {
                    let v = self.ld32(bus, a);
                    self.r[13] = start.wrapping_add(4 * n);
                    self.bx_write(bus, v);
                } else if imm!() & (1 << 13) == 0 {
                    self.r[13] = start.wrapping_add(4 * n);
                }
            }
            Kind::Ldrex => {
                let addr = self.r[rn!()].wrapping_add(imm!());
                if !self.require_aligned(addr, pc) {
                    return;
                }
                let v = self.ld32(bus, addr);
                self.exclusive = Some(addr);
                self.r[rd!()] = v;
            }
            Kind::Ldrexb => {
                let addr = self.r[rn!()];
                let v = self.ld8(bus, addr);
                self.exclusive = Some(addr);
                self.r[rd!()] = v;
            }
            Kind::Ldrexh => {
                let addr = self.r[rn!()];
                if addr & 1 != 0 {
                    self.usage_fault(CFSR_UNALIGNED, pc);
                    return;
                }
                let v = self.ld16(bus, addr);
                self.exclusive = Some(addr);
                self.r[rd!()] = v;
            }
            Kind::Strex | Kind::Strexb | Kind::Strexh => {
                let rt = (op.ra & 15) as usize;
                let addr = self.r[rn!()].wrapping_add(if kind == Kind::Strex { imm!() } else { 0 });
                let mask = match kind {
                    Kind::Strex => 3,
                    Kind::Strexh => 1,
                    _ => 0,
                };
                if addr & mask != 0 {
                    self.usage_fault(CFSR_UNALIGNED, pc);
                    return;
                }
                let v = self.r[rt];
                if self.exclusive == Some(addr) {
                    match kind {
                        Kind::Strex => self.st32(bus, addr, v),
                        Kind::Strexh => self.st16(bus, addr, v),
                        _ => self.st8(bus, addr, v),
                    }
                    self.exclusive = None;
                    self.r[rd!()] = 0;
                } else {
                    self.r[rd!()] = 1;
                }
            }
        }
    }

    // ---- branches ---------------------------------------------------------------------------------

    #[inline(always)]
    fn branch_direct(&mut self, pc: u32, target: u32, flags: u8) {
        self.r[15] = target;
        if target < pc && pc.wrapping_sub(target) <= 128 && flags & FL_NOFF == 0 && self.ff.active {
            self.ff_backward_branch(pc, target);
        }
    }

    /// `BXWritePC`: branch with interworking; EXC_RETURN values in Handler mode return from the exception.
    pub(crate) fn bx_write<B: CpuBus>(&mut self, bus: &mut B, value: u32) {
        if self.ipsr != 0 && value >= EXC_RETURN_MIN {
            self.exception_return(bus, value);
            return;
        }
        self.blx_write(value);
    }

    /// `BLXWritePC`: EPSR.T = bit 0; a cleared T bit raises INVSTATE on the next instruction.
    pub(crate) fn blx_write(&mut self, value: u32) {
        let target = value & !1;
        self.r[15] = target;
        if value & 1 == 0 {
            self.thumb = false;
            self.usage_fault(CFSR_INVSTATE, target);
        } else {
            self.thumb = true;
        }
    }

    // ---- WFI / WFE ------------------------------------------------------------------------------------

    /// tlib `helper_wfi` / `helper_wfe` (also used for `B .`): the flag is set unconditionally and
    /// the chunk ends at this translation-block boundary; whether the core really sleeps is
    /// decided by `has_work` at the start of the next chunk. A WFE returns at once when the event
    /// register is set.
    fn do_wfi(&mut self, wfe: bool) {
        if wfe {
            if self.event_flag {
                self.event_flag = false;
                return;
            }
            self.wfe = true;
        } else {
            self.wfi = true;
        }
        self.wfi_exit = true;
        self.kick();
    }

    // ---- CPS / MRS / MSR ---------------------------------------------------------------------------------

    fn exec_cps(&mut self, x: u32) {
        if !self.privileged() {
            return;
        }
        let disable = x & 0x10 != 0;
        if x & 2 != 0 {
            self.nvic.primask = disable;
        }
        if x & 1 != 0 {
            if disable {
                self.nvic.faultmask = true;
            } else if self.nvic.raw_priority() > -1 {
                self.nvic.faultmask = false;
            }
        }
        self.nvic_changed();
    }

    pub(crate) fn mrs_read(&self, sysm: u32) -> u32 {
        let privileged = self.privileged();
        match sysm {
            0..=3 | 5..=7 => {
                let mut v = 0;
                if sysm & 4 == 0 {
                    v |= (self.apsr & 0xF800_0000) | ((self.ge & 0xF) << 16);
                }
                if sysm & 1 != 0 {
                    v |= self.ipsr & 0x1FF;
                }
                if sysm & 2 != 0 {
                    let it = self.itstate as u32;
                    v |= ((self.thumb as u32) << 24) | ((it & 3) << 25) | ((it >> 2) << 10);
                }
                v
            }
            8 => self.msp(),
            9 => {
                if privileged {
                    self.psp()
                } else {
                    0
                }
            }
            16 => {
                if privileged {
                    self.nvic.primask as u32
                } else {
                    0
                }
            }
            17 | 18 => {
                if privileged {
                    self.nvic.basepri_raw as u32
                } else {
                    0
                }
            }
            19 => {
                if privileged {
                    self.nvic.faultmask as u32
                } else {
                    0
                }
            }
            20 => self.control,
            _ => 0,
        }
    }

    pub(crate) fn msr_write(&mut self, sysm: u32, mask: u32, value: u32) {
        let privileged = self.privileged();
        match sysm {
            0..=3 => {
                if mask & 2 != 0 {
                    self.apsr = (self.apsr & 0x07FF_FFFF) | (value & 0xF800_0000);
                }
                if mask & 1 != 0 {
                    self.ge = (value >> 16) & 0xF;
                }
            }
            8 => {
                if privileged {
                    self.set_sp(value);
                }
            }
            9 => {
                if privileged {
                    let v = value & !3;
                    if self.use_psp {
                        self.r[13] = v;
                    } else {
                        self.sp_other = v;
                    }
                }
            }
            16 => {
                if privileged {
                    self.nvic.primask = value & 1 != 0;
                    self.nvic_changed();
                }
            }
            17 => {
                if privileged {
                    self.nvic.set_basepri(value as u8);
                    self.nvic_changed();
                }
            }
            18 => {
                if privileged {
                    let v = (value & 0xFF) as u8;
                    let cur = self.nvic.basepri_raw;
                    if v != 0 && (v < cur || cur == 0) {
                        self.nvic.set_basepri(v);
                        self.nvic_changed();
                    }
                }
            }
            19 => {
                if privileged {
                    self.nvic.faultmask = value & 1 != 0;
                    self.nvic_changed();
                }
            }
            20 => {
                if privileged {
                    let mut c = (self.control & !CONTROL_NPRIV) | (value & CONTROL_NPRIV);
                    if self.ipsr == 0 {
                        c = (c & !CONTROL_SPSEL) | (value & CONTROL_SPSEL);
                    }
                    c = (c & !CONTROL_FPCA) | (value & CONTROL_FPCA);
                    self.control = c;
                    self.update_sp_selection();
                    self.kick();
                }
            }
            _ => {}
        }
    }

    // ---- floating point -------------------------------------------------------------------------------------

    #[inline(never)]
    fn exec_vfp<B: CpuBus>(&mut self, bus: &mut B, op: &Op, pc: u32) {
        let cp = (self.scb.cpacr >> 20) & 3;
        if !(cp == 3 || (cp == 1 && self.privileged())) {
            self.usage_fault(CFSR_NOCP, pc);
            return;
        }
        // Lazy state preservation, then lazy context creation (ExecuteFPCheck).
        if self.scb.fpccr_lspact() {
            self.preserve_fp_state(bus);
        }
        if self.scb.fpccr_aspen() && self.control & CONTROL_FPCA == 0 {
            self.control |= CONTROL_FPCA;
            // New FP context: FPSCR takes the default status (FPDSCR holds AHP/DN/FZ/RMode only).
            self.fp.fpscr = self.scb.fpdscr & vfp::fpscr::WRITE_MASK;
        }
        let insn = if op.imm == u32::MAX {
            match self.vfp_scratch {
                Some(i) => i,
                None => {
                    self.usage_fault(CFSR_UNDEFINSTR, pc);
                    return;
                }
            }
        } else {
            self.vfp_table[op.imm as usize]
        };
        let result = {
            let mut host = FpHost { cpu: self, bus, pc };
            vfp::execute(&insn, &mut host)
        };
        match result {
            vfp::VfpExec::Ok => {}
            vfp::VfpExec::Undefined => self.usage_fault(CFSR_UNDEFINSTR, pc),
            vfp::VfpExec::Fault(vfp::VfpFault::Unaligned(_)) => self.usage_fault(CFSR_UNALIGNED, pc),
        }
    }

    /// Lazy floating-point state preservation: stores S0-S15 and FPSCR at FPCAR.
    pub(crate) fn preserve_fp_state<B: CpuBus>(&mut self, bus: &mut B) {
        let mut a = self.scb.fpcar & !7;
        for i in 0..16 {
            let v = self.fp.s[i];
            self.st32(bus, a, v);
            a = a.wrapping_add(4);
        }
        let fpscr = self.fp.fpscr;
        self.st32(bus, a, fpscr);
        self.scb.set_fpccr_lspact(false);
    }
}

#[inline(always)]
fn half_s(v: u32, high: u32) -> i32 {
    if high != 0 {
        (v >> 16) as u16 as i16 as i32
    } else {
        v as u16 as i16 as i32
    }
}

struct FpHost<'a, B: CpuBus> {
    cpu: &'a mut Cpu,
    bus: &'a mut B,
    pc: u32,
}

impl<'a, B: CpuBus> vfp::VfpHost for FpHost<'a, B> {
    fn reg(&self, n: u32) -> u32 {
        self.cpu.r[(n & 15) as usize]
    }
    fn set_reg(&mut self, n: u32, value: u32) {
        let i = (n & 15) as usize;
        self.cpu.r[i] = if i == 13 { value & !3 } else { value };
    }
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.cpu.apsr = (self.cpu.apsr & 0x0FFF_FFFF) | (nzcv & 0xF000_0000);
    }
    fn fp(&mut self) -> &mut vfp::FpRegs {
        &mut self.cpu.fp
    }
    fn load32(&mut self, addr: u32) -> u32 {
        self.cpu.ld32(&mut *self.bus, addr)
    }
    fn store32(&mut self, addr: u32, value: u32) {
        self.cpu.st32(&mut *self.bus, addr, value)
    }
    fn literal_base(&self) -> u32 {
        self.pc.wrapping_add(4) & !3
    }
}
