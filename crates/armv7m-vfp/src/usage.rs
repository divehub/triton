//! Register and memory usage of decoded instructions.
//!
//! The core's exact routine acceleration (`armv7m::accel`) proves, per recorded code path, which inputs
//! a routine depends on. For that it needs to know, for every VFP instruction, which S registers and
//! core registers it reads, which it writes and how it touches memory and the FPSCR. This module is a
//! pure function of the decoded instruction; `tests/usage.rs` checks it against [`crate::execute`]
//! (registers outside the write sets never change, registers outside the read sets never influence
//! a result).

use crate::decode::*;
use crate::VfpInsn;

/// The memory access of a load/store instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemUse {
    pub store: bool,
    /// Base register (0..=15). 15 means the literal base `Align(PC, 4)`.
    pub base: u8,
    /// `VLDM`/`VSTM` (`VPUSH`/`VPOP`): a block transfer. Otherwise `VLDR`/`VSTR` (one or two words at `base + offset`).
    pub block: bool,
    /// Signed byte offset added to the base (`VLDR`/`VSTR`); for `VLDM`/`VSTM` the total transfer size in bytes.
    pub offset: u32,
    /// `VLDM`/`VSTM`: the transfer starts at `base - offset` (decrement before).
    pub decrement_before: bool,
    /// Number of 32-bit words transferred (S registers `first .. first + words`, modulo 32).
    pub words: u8,
    /// First S register of the transfer.
    pub first_s: u8,
    /// The base register is written back (`VLDM`/`VSTM` with `!`; `base - offset` or `base + offset`).
    pub writeback: bool,
}

/// A register named by a verbatim copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Core(u8),
    S(u8),
}

/// What a decoded instruction reads and writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Register-to-register bit copies `(from, to)` the instruction performs (`VMOV` between S registers and
    /// between core and S registers): the destination takes the source's 32 bits unchanged.
    pub moves: [Option<(Loc, Loc)>; 2],
    /// S registers whose value the instruction uses (accumulator destinations and stored registers included).
    pub reads_s: u32,
    /// S registers the instruction writes.
    pub writes_s: u32,
    /// Core registers read (address bases, transferred values). The literal base is not a register.
    pub reads_core: u16,
    /// Core registers written (transfer destinations, base writeback).
    pub writes_core: u16,
    /// Reads the FPSCR control fields (rounding mode, flush-to-zero, default NaN, alternative half precision).
    pub reads_fpscr_ctrl: bool,
    /// Reads the whole FPSCR (`VMRS`).
    pub reads_fpscr_all: bool,
    /// Writes FPSCR N, Z, C, V (`VCMP`).
    pub writes_fpscr_nzcv: bool,
    /// Replaces the FPSCR (`VMSR`).
    pub writes_fpscr_all: bool,
    /// May set cumulative exception flags (IOC, DZC, OFC, UFC, IXC, IDC).
    pub raises_flags: bool,
    /// Writes the APSR N, Z, C, V flags (`VMRS APSR_nzcv, FPSCR`).
    pub writes_apsr_nzcv: bool,
    pub mem: Option<MemUse>,
}

impl VfpInsn {
    /// Register, FPSCR and memory usage of this instruction.
    pub fn usage(&self) -> Usage {
        let s = |i: u32| 1u32 << (i & 31);
        let core = |r: u8| 1u16 << (r & 15);
        let (d, n, m) = (u32::from(self.d & 31), u32::from(self.n & 31), u32::from(self.m & 31));
        let mut u = Usage::default();
        match self.op {
            Op::Vldr | Op::Vstr | Op::VldrD | Op::VstrD => {
                let store = matches!(self.op, Op::Vstr | Op::VstrD);
                let words = if matches!(self.op, Op::VldrD | Op::VstrD) { 2 } else { 1 };
                let regs = if words == 2 { s(d) | s(d + 1) } else { s(d) };
                if store {
                    u.reads_s = regs;
                } else {
                    u.writes_s = regs;
                }
                if self.n != 15 {
                    u.reads_core = core(self.n);
                }
                u.mem = Some(MemUse { store, base: self.n, block: false, offset: self.imm, decrement_before: false, words, first_s: self.d, writeback: false });
            }
            Op::Vldm | Op::Vstm => {
                let store = self.op == Op::Vstm;
                let mut regs = 0u32;
                for i in 0..u32::from(self.m) {
                    regs |= s(d + i);
                }
                if store {
                    u.reads_s = regs;
                } else {
                    u.writes_s = regs;
                }
                u.reads_core = core(self.n);
                let writeback = self.flags & F_WBACK != 0;
                if writeback {
                    u.writes_core = core(self.n);
                }
                u.mem = Some(MemUse {
                    store,
                    base: self.n,
                    block: true,
                    offset: self.imm,
                    decrement_before: self.flags & F_DB != 0,
                    words: self.m,
                    first_s: self.d,
                    writeback,
                });
            }
            Op::VmovImm => u.writes_s = s(d),
            Op::VmovReg => {
                u.reads_s = s(m);
                u.writes_s = s(d);
                u.moves[0] = Some((Loc::S(m as u8), Loc::S(d as u8)));
            }
            Op::Vabs | Op::Vneg => {
                u.reads_s = s(m);
                u.writes_s = s(d);
            }
            Op::VmovToS => {
                u.reads_core = core(self.n);
                u.writes_s = s(d);
                u.moves[0] = Some((Loc::Core(self.n & 15), Loc::S(d as u8)));
            }
            Op::VmovFromS => {
                u.reads_s = s(d);
                u.writes_core = core(self.n);
                u.moves[0] = Some((Loc::S(d as u8), Loc::Core(self.n & 15)));
            }
            Op::Vmov2ToS => {
                u.reads_core = core(self.n) | core(self.m);
                u.writes_s = s(d) | s(d + 1);
                u.moves[0] = Some((Loc::Core(self.n & 15), Loc::S(d as u8)));
                u.moves[1] = Some((Loc::Core(self.m & 15), Loc::S(((d + 1) & 31) as u8)));
            }
            Op::Vmov2FromS => {
                u.reads_s = s(d) | s(d + 1);
                u.writes_core = core(self.n) | core(self.m);
                u.moves[0] = Some((Loc::S(d as u8), Loc::Core(self.n & 15)));
                u.moves[1] = Some((Loc::S(((d + 1) & 31) as u8), Loc::Core(self.m & 15)));
            }
            Op::Vmrs => {
                u.reads_fpscr_all = true;
                if self.d == 15 {
                    u.writes_apsr_nzcv = true;
                } else {
                    u.writes_core = core(self.d);
                }
            }
            Op::Vmsr => {
                u.reads_core = core(self.n);
                u.writes_fpscr_all = true;
            }
            Op::Vadd | Op::Vsub | Op::Vmul | Op::Vnmul | Op::Vdiv => {
                u.reads_s = s(n) | s(m);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::Vsqrt => {
                u.reads_s = s(m);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::Vmla | Op::Vmls | Op::Vnmla | Op::Vnmls | Op::Vfma | Op::Vfms | Op::Vfnma | Op::Vfnms => {
                u.reads_s = s(d) | s(n) | s(m);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::Vcmp => {
                u.reads_s = s(d) | if self.flags & F_ZERO != 0 { 0 } else { s(m) };
                u.writes_fpscr_nzcv = true;
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::VcvtFromInt | Op::VcvtToInt | Op::Vcvt16To32 => {
                u.reads_s = s(m);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::VcvtFromFixed | Op::VcvtToFixed => {
                u.reads_s = s(d);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
            Op::Vcvt32To16 => {
                u.reads_s = s(m) | s(d);
                u.writes_s = s(d);
                u.reads_fpscr_ctrl = true;
                u.raises_flags = true;
            }
        }
        u
    }
}
