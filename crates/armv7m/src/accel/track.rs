//! Dependency tracking along one recorded execution path.
//!
//! A routine call is memoized only if its outcome is a function of declared inputs (the "key": argument
//! registers, the S registers carrying float arguments, the FPSCR control fields). This module proves that
//! for one concrete path. Every register starts with a *descriptor*:
//!
//! * `Const` - the value is a function of the key (key registers at entry, and everything computed from
//!   `Const` values only: arithmetic results, flash literals and tables read at `Const` addresses);
//! * `Entry(id)` - the value is a verbatim copy of register `id` as it was at entry (the callee-saved
//!   registers and the return address, pushed and popped; also registers the path never touched).
//!
//! `Entry` values may only be copied (`mov`, `push`/`pop`, `vmov`, `vpush`/`vpop`, stores to the routine's
//! own frame and loads back from it). Using one in any other way (as an operand, an address, a branch
//! condition input) makes the path **unsafe**: the outcome could then depend on the caller's state, and no
//! memo entry is created. Flags are tracked per bit (N Z C V) the same way: reading a flag the path has not
//! produced itself is unsafe.
//!
//! Memory is limited to (a) reads of flash at `Const` addresses (immutable), (b) the routine's own frame
//! below the entry SP: stores by `push`/`vpush`/`str [sp, ...]`, and loads of words the path itself stored.
//! Anything else (RAM reads or writes elsewhere, MMIO, reads of the caller's frame) is unsafe.
//!
//! Because the path was *executed* by the interpreter, and every operand of every instruction on it is
//! determined by the key, re-running the call with the same key and any other caller state executes the
//! same instructions on the same values: induction over the path. The registers, flags, frame words and
//! the instruction count at the end are therefore `Const` (functions of the key) or `Entry` copies, which
//! is exactly what a memo entry stores.

use crate::alu::{SHIFT_LSL, SHIFT_RRX};
use crate::op::*;
use crate::vfp;

/// Bytes below the entry SP a routine may use for its frame.
pub(super) const MAX_FRAME: i32 = 512;

/// Descriptor ids: core registers 0..=15, S registers 16..=47.
pub(super) const ID_S0: u8 = 16;

/// Register descriptor (see the module documentation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Desc(u8);

impl Desc {
    pub const CONST: Desc = Desc(0xF0);

    pub fn entry(id: u8) -> Desc {
        debug_assert!(id < 48);
        Desc(id)
    }

    pub fn is_const(self) -> bool {
        self == Desc::CONST
    }

    pub fn entry_id(self) -> Option<u8> {
        (self.0 < 48).then_some(self.0)
    }
}

/// State before the instruction executes.
#[derive(Clone, Copy)]
pub(super) struct Pre {
    pub r: [u32; 16],
    pub s: [u32; 32],
    pub apsr: u32,
    pub itstate: u8,
}

/// One word of the routine's frame.
#[derive(Clone, Copy, Debug)]
pub(super) struct Word {
    /// Offset from the entry SP (negative, multiple of 4).
    pub off: i32,
    pub desc: Desc,
    pub value: u32,
}

/// What the end of a path looks like to the recorder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PathState {
    Running,
    /// The routine returned to its caller (a branch to the entry LR from the entry LR value).
    Returned,
    Unsafe(&'static str),
}

pub(super) struct Tracker {
    fp_allowed: bool,
    code_lo: u32,
    code_hi: u32,
    entry_ret: u32,
    sp0: u32,
    pub core: [Desc; 16],
    pub s: [Desc; 32],
    /// NZCV bits (N = 8 .. V = 1) produced by the path.
    pub flags_const: u8,
    pub fpscr_nzcv_const: bool,
    pub fpscr_nzcv_written: bool,
    pub uses_fp: bool,
    sp_off: i32,
    pub frame: Vec<Word>,
    /// Every store in program order.
    pub stores: Vec<Word>,
    pub min_off: i32,
    pub state: PathState,
}

const N: u8 = 8;
const Z: u8 = 4;
const C: u8 = 2;
const V: u8 = 1;

/// Flag bits (N Z C V) a condition code reads.
fn cond_flags(cond: u32) -> u8 {
    match (cond & 15) >> 1 {
        0 => Z,
        1 => C,
        2 => N,
        3 => V,
        4 => C | Z,
        5 => N | V,
        6 => Z | N | V,
        _ => 0,
    }
}

fn bit(r: u8) -> u16 {
    1 << (r & 15)
}

impl Tracker {
    /// `core_key`/`s_key`: bit masks of the key registers; `entry_regs` are the registers at entry.
    pub fn new(core_key: u16, s_key: u32, fp_allowed: bool, code: (u32, u32), entry_ret: u32, sp0: u32) -> Self {
        let mut core = [Desc::CONST; 16];
        for (i, d) in core.iter_mut().enumerate() {
            if core_key & (1 << i) == 0 {
                *d = Desc::entry(i as u8);
            }
        }
        let mut s = [Desc::CONST; 32];
        for (i, d) in s.iter_mut().enumerate() {
            if s_key & (1 << i) == 0 {
                *d = Desc::entry(ID_S0 + i as u8);
            }
        }
        Tracker {
            fp_allowed,
            code_lo: code.0,
            code_hi: code.1,
            entry_ret,
            sp0,
            core,
            s,
            flags_const: 0,
            fpscr_nzcv_const: false,
            fpscr_nzcv_written: false,
            uses_fp: false,
            sp_off: 0,
            frame: Vec::new(),
            stores: Vec::new(),
            min_off: 0,
            state: PathState::Running,
        }
    }

    pub fn is_unsafe(&self) -> bool {
        matches!(self.state, PathState::Unsafe(_))
    }

    pub fn mark_unsafe(&mut self, why: &'static str) {
        self.fail(why);
    }

    pub fn unsafe_reason(&self) -> Option<&'static str> {
        match self.state {
            PathState::Unsafe(r) => Some(r),
            _ => None,
        }
    }

    fn fail(&mut self, why: &'static str) {
        if !self.is_unsafe() {
            self.state = PathState::Unsafe(why);
        }
    }

    // ---- operand helpers -------------------------------------------------------------------------------------

    /// A core register used as data: it must be a function of the key.
    fn use_reg(&mut self, r: u8) -> bool {
        let r = (r & 15) as usize;
        if r == 13 || r == 15 {
            self.fail("SP or PC used as a data operand");
            return false;
        }
        if !self.core[r].is_const() {
            self.fail("reads a caller register");
            return false;
        }
        true
    }

    fn use_regs(&mut self, mask: u16) -> bool {
        (0..15u8).filter(|r| mask & (1 << r) != 0).all(|r| self.use_reg(r))
    }

    fn use_flags(&mut self, mask: u8) -> bool {
        if mask & !self.flags_const != 0 {
            self.fail("reads flags the path did not produce");
            return false;
        }
        true
    }

    fn set_flags(&mut self, mask: u8) {
        self.flags_const |= mask;
    }

    fn write_const(&mut self, r: u8) {
        let r = (r & 15) as usize;
        if r == 13 || r == 15 {
            self.fail("writes SP or PC");
            return;
        }
        self.core[r] = Desc::CONST;
    }

    fn in_flash(&self, addr: u32) -> bool {
        addr >= self.code_lo && addr < self.code_hi
    }

    fn frame_off(&mut self, addr: u32, len: u32) -> Option<i32> {
        let off = addr.wrapping_sub(self.sp0) as i32;
        if off % 4 != 0 || off >= 0 || off < -MAX_FRAME || len == 0 || off.wrapping_add(len as i32) > 0 {
            self.fail("frame access outside the routine's own frame");
            return None;
        }
        Some(off)
    }

    fn frame_load(&mut self, off: i32) -> Option<Desc> {
        match self.frame.iter().find(|w| w.off == off) {
            Some(w) => Some(w.desc),
            None => {
                self.fail("reads a frame word the routine did not store");
                None
            }
        }
    }

    fn frame_store(&mut self, off: i32, desc: Desc, value: u32) {
        let word = Word { off, desc, value };
        match self.frame.iter_mut().find(|w| w.off == off) {
            Some(w) => *w = word,
            None => self.frame.push(word),
        }
        self.stores.push(word);
        self.min_off = self.min_off.min(off);
    }

    /// A word copied into the frame: a register's descriptor (any non-SP register).
    fn store_reg(&mut self, r: u8, pre: &Pre, off: i32) {
        let r = (r & 15) as usize;
        if r == 13 || r == 15 {
            self.fail("stores SP or PC");
            return;
        }
        let desc = self.core[r];
        self.frame_store(off, desc, pre.r[r]);
    }

    fn adjust_sp(&mut self, delta: i32) {
        self.sp_off = self.sp_off.wrapping_add(delta);
        if self.sp_off > 0 || self.sp_off < -MAX_FRAME {
            self.fail("SP leaves the routine's frame");
        }
    }

    // ---- the per-instruction step ----------------------------------------------------------------------------------

    /// Accounts for the instruction at `pre.r[15]` that has just been executed. `post_pc`/`post_sp` are the
    /// register values afterwards. `vfp` is the decoded VFP instruction for `Kind::Vfp`.
    pub fn step(&mut self, pre: &Pre, post_pc: u32, post_sp: u32, op: &Op, vfp: Option<&vfp::VfpInsn>) {
        if self.state != PathState::Running {
            return;
        }
        let in_it = pre.itstate != 0;
        if in_it {
            let cond = u32::from(pre.itstate >> 4);
            if !self.use_flags(cond_flags(cond)) {
                return;
            }
            if !cond_holds(cond, pre.apsr) {
                // Skipped by the IT block: it only counts as an executed instruction.
                self.check_sp(post_sp);
                return;
            }
        }
        // Flag-setting 16-bit forms do not set flags inside an IT block.
        let sflag = op.flags & FL_S != 0 && !(in_it && op.flags & FL_IT != 0);
        let (rd, rn, rm, ra) = (op.rd & 15, op.rn & 15, op.rm & 15, op.ra & 15);
        // Logical operations set N and Z from the result and C from the shifter or the immediate; when the shifter does
        // not produce a carry (no shift, or an immediate that is not a rotated byte) C is left unchanged, which is
        // neither a read nor a write of the flag.
        let shifter_reads_c = op.ra == SHIFT_RRX;
        let shifter_writes_c = !(op.x == 0 && op.ra == SHIFT_LSL);
        let imm_writes_c = op.flags & FL_IMMC != 0;
        match op.kind {
            // ---- no effect ---------------------------------------------------------------------------------------------
            Kind::Nop | Kind::Barrier | Kind::It => {}

            // ---- data processing, immediate ----------------------------------------------------------------------------
            Kind::MovImm | Kind::MvnImm => {
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | if imm_writes_c { C } else { 0 });
                }
            }
            Kind::Movw => self.write_const(rd),
            Kind::Movt => {
                if self.use_reg(rd) {
                    self.write_const(rd);
                }
            }
            Kind::AndImm | Kind::BicImm | Kind::OrrImm | Kind::OrnImm | Kind::EorImm => {
                if !self.use_reg(rn) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | if imm_writes_c { C } else { 0 });
                }
            }
            Kind::TstImm | Kind::TeqImm => {
                if !self.use_reg(rn) {
                    return;
                }
                self.set_flags(N | Z | if imm_writes_c { C } else { 0 });
            }
            Kind::AddImm | Kind::SubImm if rn == 13 => {
                // Only `add sp, sp, #imm` / `sub sp, sp, #imm` (frame allocation).
                if rd != 13 || sflag || op.imm % 4 != 0 || op.imm > 4096 {
                    self.fail("SP used as a data operand");
                    return;
                }
                let delta = if op.kind == Kind::AddImm { op.imm as i32 } else { -(op.imm as i32) };
                self.adjust_sp(delta);
            }
            Kind::AddImm | Kind::SubImm | Kind::RsbImm => {
                if !self.use_reg(rn) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C | V);
                }
            }
            Kind::AdcImm | Kind::SbcImm => {
                if !self.use_reg(rn) || !self.use_flags(C) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C | V);
                }
            }
            Kind::CmpImm | Kind::CmnImm => {
                if !self.use_reg(rn) {
                    return;
                }
                self.set_flags(N | Z | C | V);
            }

            // ---- data processing, register -------------------------------------------------------------------------------
            Kind::MovReg => {
                if rm == 13 || rd == 13 || rm == 15 {
                    self.fail("SP copied");
                    return;
                }
                // A verbatim copy: Entry descriptors travel with the value.
                let d = self.core[rm as usize];
                self.core[rd as usize] = d;
                if sflag {
                    // set_nz: N and Z come from the value, so it must be a function of the key.
                    if !d.is_const() {
                        self.fail("flags set from a caller register");
                        return;
                    }
                    self.set_flags(N | Z);
                }
            }
            Kind::MvnReg | Kind::AndReg | Kind::BicReg | Kind::OrrReg | Kind::OrnReg | Kind::EorReg => {
                let mut reads = bit(rm);
                if !matches!(op.kind, Kind::MvnReg) {
                    reads |= bit(rn);
                }
                if !self.use_regs(reads) || (shifter_reads_c && !self.use_flags(C)) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | if shifter_writes_c { C } else { 0 });
                }
            }
            Kind::TstReg | Kind::TeqReg => {
                if !self.use_regs(bit(rn) | bit(rm)) || (shifter_reads_c && !self.use_flags(C)) {
                    return;
                }
                self.set_flags(N | Z | if shifter_writes_c { C } else { 0 });
            }
            Kind::AddReg | Kind::SubReg | Kind::RsbReg => {
                if !self.use_regs(bit(rn) | bit(rm)) || (op.ra == SHIFT_RRX && !self.use_flags(C)) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C | V);
                }
            }
            Kind::AdcReg | Kind::SbcReg => {
                if !self.use_regs(bit(rn) | bit(rm)) || !self.use_flags(C) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C | V);
                }
            }
            Kind::CmpReg | Kind::CmnReg => {
                if !self.use_regs(bit(rn) | bit(rm)) || (op.ra == SHIFT_RRX && !self.use_flags(C)) {
                    return;
                }
                self.set_flags(N | Z | C | V);
            }
            Kind::LslImm | Kind::LsrImm | Kind::AsrImm | Kind::RorImm => {
                if !self.use_reg(rm) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C);
                }
            }
            Kind::Rrx => {
                if !self.use_reg(rm) || !self.use_flags(C) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | C);
                }
            }
            Kind::LslReg | Kind::LsrReg | Kind::AsrReg | Kind::RorReg => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                // A zero shift amount (a function of the key) leaves the carry flag unchanged.
                let amount_nonzero = pre.r[rm as usize] & 0xFF != 0;
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z | if amount_nonzero { C } else { 0 });
                }
            }

            // ---- multiply, divide, extend, bit operations ------------------------------------------------------------------------
            Kind::Mul => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                self.write_const(rd);
                if sflag {
                    self.set_flags(N | Z);
                }
            }
            Kind::Mla | Kind::Mls => {
                if !self.use_regs(bit(rn) | bit(rm) | bit(ra)) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Umull | Kind::Smull => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                self.write_const(rd);
                self.write_const(ra);
            }
            Kind::Umlal | Kind::Smlal | Kind::Umaal => {
                if !self.use_regs(bit(rn) | bit(rm) | bit(rd) | bit(ra)) {
                    return;
                }
                self.write_const(rd);
                self.write_const(ra);
            }
            Kind::Sdiv | Kind::Udiv => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Sxtb | Kind::Sxth | Kind::Uxtb | Kind::Uxth | Kind::Sxtb16 | Kind::Uxtb16 | Kind::Rev | Kind::Rev16 | Kind::Revsh | Kind::Rbit | Kind::Clz => {
                if !self.use_reg(rm) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Sxtab | Kind::Sxtah | Kind::Uxtab | Kind::Uxtah | Kind::Sxtab16 | Kind::Uxtab16 | Kind::Pkhbt | Kind::Pkhtb | Kind::Usad8 => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Usada8 => {
                if !self.use_regs(bit(rn) | bit(rm) | bit(ra)) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Bfi => {
                if !self.use_regs(bit(rd) | bit(rn)) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Bfc => {
                if !self.use_reg(rd) {
                    return;
                }
                self.write_const(rd);
            }
            Kind::Ubfx | Kind::Sbfx => {
                if !self.use_reg(rn) {
                    return;
                }
                self.write_const(rd);
            }

            // ---- loads --------------------------------------------------------------------------------------------------------------
            Kind::LdrLit | Kind::LdrbLit | Kind::LdrhLit | Kind::LdrsbLit | Kind::LdrshLit => {
                if !self.in_flash(op.imm) {
                    self.fail("literal outside flash");
                    return;
                }
                self.write_const(rd);
            }
            Kind::LdrImm | Kind::LdrbImm | Kind::LdrhImm | Kind::LdrsbImm | Kind::LdrshImm => {
                let writeback = op.flags & FL_WB != 0;
                let base = pre.r[rn as usize];
                let new_base = base.wrapping_add(op.imm);
                let addr = if op.flags & FL_IDX != 0 { new_base } else { base };
                if rn == 13 {
                    // The routine's own frame: whole, word-sized loads only.
                    if op.kind != Kind::LdrImm {
                        self.fail("sub-word frame load");
                        return;
                    }
                    let Some(off) = self.frame_off(addr, 4) else { return };
                    let Some(desc) = self.frame_load(off) else { return };
                    if rd == 13 || rd == 15 {
                        self.fail("loads SP or PC");
                        return;
                    }
                    self.core[rd as usize] = desc;
                    if writeback {
                        self.adjust_sp(op.imm as i32);
                    }
                } else {
                    if !self.use_reg(rn) {
                        return;
                    }
                    if !self.in_flash(addr) {
                        self.fail("load from outside flash");
                        return;
                    }
                    if writeback && rd == rn {
                        self.fail("load with writeback to its own destination");
                        return;
                    }
                    self.write_const(rd);
                }
            }
            Kind::LdrReg | Kind::LdrbReg | Kind::LdrhReg | Kind::LdrsbReg | Kind::LdrshReg => {
                if !self.use_regs(bit(rn) | bit(rm)) {
                    return;
                }
                let addr = pre.r[rn as usize].wrapping_add(pre.r[rm as usize] << op.x);
                if !self.in_flash(addr) {
                    self.fail("load from outside flash");
                    return;
                }
                self.write_const(rd);
            }

            // ---- stores (the routine's own frame only) ---------------------------------------------------------------------------
            Kind::StrImm if rn == 13 => {
                let base = pre.r[13];
                let new_base = base.wrapping_add(op.imm);
                let addr = if op.flags & FL_IDX != 0 { new_base } else { base };
                let Some(off) = self.frame_off(addr, 4) else { return };
                self.store_reg(rd, pre, off);
                if op.flags & FL_WB != 0 {
                    self.adjust_sp(op.imm as i32);
                }
            }
            Kind::Push => {
                let list = op.imm;
                let n = list.count_ones() as i32;
                if list & 0x2000 != 0 {
                    self.fail("push of SP");
                    return;
                }
                let start = self.sp_off - 4 * n;
                let mut off = start;
                for r in 0..15u8 {
                    if list & (1 << r) != 0 {
                        if off < -MAX_FRAME {
                            self.fail("frame too large");
                            return;
                        }
                        self.store_reg(r, pre, off);
                        off += 4;
                    }
                }
                self.sp_off = start;
                self.adjust_sp(0);
            }
            Kind::Pop | Kind::PopPc => {
                let list = op.imm;
                let n = list.count_ones() as i32;
                if list & 0x2000 != 0 {
                    self.fail("pop of SP");
                    return;
                }
                let mut off = self.sp_off;
                let mut last = Desc::CONST;
                for r in 0..16u8 {
                    if list & (1 << r) != 0 {
                        let Some(desc) = self.frame_load(off) else { return };
                        if r < 15 {
                            self.core[r as usize] = desc;
                        } else {
                            last = desc;
                        }
                        off += 4;
                    }
                }
                self.sp_off += 4 * n;
                if self.sp_off > 0 {
                    self.fail("pops beyond the routine's frame");
                    return;
                }
                if op.kind == Kind::PopPc {
                    self.branch_register(last, post_pc);
                }
            }

            // ---- control flow ---------------------------------------------------------------------------------------------------------
            Kind::B => {}
            Kind::Bcc => {
                self.use_flags(cond_flags(u32::from(op.x)));
            }
            Kind::Cbz | Kind::Cbnz => {
                self.use_reg(rn);
            }
            Kind::Bl => self.core[14] = Desc::CONST,
            Kind::Blx => {
                if self.use_reg(rm) {
                    self.core[14] = Desc::CONST;
                }
            }
            Kind::Bx => {
                if rm == 13 || rm == 15 {
                    self.fail("bx sp/pc");
                    return;
                }
                let d = self.core[rm as usize];
                self.branch_register(d, post_pc);
            }

            // ---- floating point ------------------------------------------------------------------------------------------------------
            Kind::Vfp => match vfp {
                Some(insn) => self.step_vfp(pre, insn),
                None => self.fail("undecoded VFP instruction"),
            },

            _ => self.fail("instruction kind not tracked"),
        }
        if !self.is_unsafe() {
            self.check_sp(post_sp);
        }
    }

    /// Debug cross-check: the tracked SP offset must match the real SP.
    fn check_sp(&mut self, post_sp: u32) {
        if post_sp != self.sp0.wrapping_add(self.sp_off as u32) {
            self.fail("tracked SP differs from the real SP");
        }
    }

    /// A register-sourced branch (`bx`, `pop {pc}`): `desc` is the descriptor of the branch target value.
    fn branch_register(&mut self, desc: Desc, post_pc: u32) {
        if desc.is_const() {
            // A nested return or computed jump to a key-determined address.
            if post_pc == self.entry_ret {
                self.fail("internal branch coincides with the return address");
            }
            return;
        }
        if desc.entry_id() == Some(14) {
            if post_pc != self.entry_ret {
                self.fail("return address mismatch");
            } else if self.sp_off != 0 {
                self.fail("returns with an unbalanced stack");
            } else {
                self.state = PathState::Returned;
            }
            return;
        }
        self.fail("branches to a caller-supplied address");
    }

    fn step_vfp(&mut self, pre: &Pre, insn: &vfp::VfpInsn) {
        if !self.fp_allowed {
            self.fail("floating point in an integer routine");
            return;
        }
        self.uses_fp = true;
        let u = insn.usage();
        if u.writes_fpscr_all {
            self.fail("VMSR");
            return;
        }
        if let Some(m) = u.mem {
            self.vfp_memory(pre, &u, m);
            return;
        }
        if u.reads_fpscr_all {
            // Only `vmrs APSR_nzcv, fpscr` of flags the path produced itself.
            if !u.writes_apsr_nzcv || !self.fpscr_nzcv_const {
                self.fail("VMRS of FPSCR bits the path did not produce");
                return;
            }
            self.set_flags(N | Z | C | V);
            return;
        }
        // Verbatim register copies (VMOV): descriptors travel with the value.
        if u.moves[0].is_some() {
            let mut staged = [None; 2];
            for (slot, mv) in staged.iter_mut().zip(u.moves.iter()) {
                if let Some((from, to)) = mv {
                    let desc = match *from {
                        vfp::Loc::Core(r) => {
                            if r == 13 || r == 15 {
                                self.fail("SP or PC transferred to a VFP register");
                                return;
                            }
                            self.core[r as usize]
                        }
                        vfp::Loc::S(i) => self.s[(i & 31) as usize],
                    };
                    *slot = Some((*to, desc));
                }
            }
            for (to, desc) in staged.into_iter().flatten() {
                match to {
                    vfp::Loc::Core(r) => {
                        if r == 13 || r == 15 {
                            self.fail("VFP transfer to SP or PC");
                            return;
                        }
                        self.core[r as usize] = desc;
                    }
                    vfp::Loc::S(i) => self.s[(i & 31) as usize] = desc,
                }
            }
            return;
        }
        for i in 0..32 {
            if u.reads_s & (1 << i) != 0 && !self.s[i].is_const() {
                self.fail("reads a caller S register");
                return;
            }
        }
        for i in 0..15u8 {
            if u.reads_core & (1 << i) != 0 && !self.use_reg(i) {
                return;
            }
        }
        for i in 0..32 {
            if u.writes_s & (1 << i) != 0 {
                self.s[i] = Desc::CONST;
            }
        }
        for r in 0..15u8 {
            if u.writes_core & (1 << r) != 0 {
                self.write_const(r);
            }
        }
        if u.writes_fpscr_nzcv {
            self.fpscr_nzcv_const = true;
            self.fpscr_nzcv_written = true;
        }
    }

    fn vfp_memory(&mut self, pre: &Pre, u: &vfp::Usage, m: vfp::MemUse) {
        let base_value = if m.base == 15 { pre.r[15].wrapping_add(4) & !3 } else { pre.r[(m.base & 15) as usize] };
        let start = if m.block {
            if m.decrement_before {
                base_value.wrapping_sub(m.offset)
            } else {
                base_value
            }
        } else {
            base_value.wrapping_add(m.offset)
        };
        let words = u32::from(m.words);
        if m.base == 13 {
            // The routine's own frame (vpush / vpop / vstr / vldr [sp, #imm]).
            for i in 0..words {
                let sreg = ((u32::from(m.first_s) + i) & 31) as usize;
                let Some(off) = self.frame_off(start.wrapping_add(4 * i), 4) else { return };
                if m.store {
                    let desc = self.s[sreg];
                    self.frame_store(off, desc, pre_s_value(pre, sreg));
                } else {
                    let Some(desc) = self.frame_load(off) else { return };
                    self.s[sreg] = desc;
                }
            }
            if m.writeback {
                let new_base = if m.decrement_before { start } else { base_value.wrapping_add(m.offset) };
                self.sp_off = new_base.wrapping_sub(self.sp0) as i32;
                self.adjust_sp(0);
            }
            return;
        }
        if m.store {
            self.fail("VFP store outside the frame");
            return;
        }
        if m.base != 15 {
            let b = m.base & 15;
            if b == 13 || !self.use_reg(b) {
                return;
            }
        }
        if m.writeback {
            self.fail("VFP load with base writeback");
            return;
        }
        for i in 0..words {
            if !self.in_flash(start.wrapping_add(4 * i)) {
                self.fail("VFP load from outside flash");
                return;
            }
        }
        for i in 0..32 {
            if u.writes_s & (1 << i) != 0 {
                self.s[i] = Desc::CONST;
            }
        }
    }
}

/// The value of an S register before the instruction (needed for the recorded frame contents of `vpush`).
fn pre_s_value(pre: &Pre, sreg: usize) -> u32 {
    pre.s[sreg]
}
