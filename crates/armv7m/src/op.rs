//! Predecoded instruction representation.
//!
//! One `Op` describes one Thumb/Thumb-2 instruction at a fixed address; branch
//! targets and PC-relative literal addresses are pre-resolved to absolute
//! values. The struct is `Copy` and exactly 16 bytes so that the predecode
//! cache stays compact and dispatch is a single dense `match` on `kind`.
//!
//! Field conventions per kind (unlisted fields are zero):
//!
//! * data-processing immediate (`*Imm`): `rd`, `rn`, `imm` = expanded
//!   immediate, `flags` = `FL_S` (set flags), `FL_IT` (S only outside IT),
//!   `FL_IMMC` (logical ops: carry-out is `imm >> 31`, else carry unchanged).
//! * data-processing register (`*Reg`): `rd`, `rn`, `rm`, `ra` = shift type
//!   (`alu::SHIFT_*`), `x` = shift amount, flags as above.
//! * immediate shifts (`LslImm`..`Rrx`): `rd`, `rm`, `x` = amount.
//! * register shifts (`LslReg`..`RorReg`): `rd`, `rn` (value), `rm` (amount).
//! * loads/stores with immediate offset: `rd` = Rt, `rn`, `imm` = signed offset
//!   (wrapping `u32`), flags `FL_IDX` (apply offset before the access) and
//!   `FL_WB` (write the updated base back).
//! * loads/stores with register offset: `rd` = Rt, `rn`, `rm`, `x` = LSL amount.
//! * literal loads: `rd` = Rt, `imm` = absolute address.
//! * `Ldm`/`Stm`: `rn`, `imm` = register list, `FL_WB`, `FL_DB`; `Push`/`Pop`:
//!   `imm` = register list.
//! * branches: `imm` = absolute target (`Bcc`: `x` = condition).

/// Instruction kinds. `Undecoded` must stay zero: the predecode cache is
/// initialised with zeroed slots.
///
/// The kinds from [`Kind::B`] to the end of the enum are exactly the
/// instructions after which Renode's translator (tlib) ends the current
/// translation block (`is_jmp != DISAS_NEXT`: every branch / PC write whether
/// taken or not, `wfi`/`wfe`, `svc`, `msr`, `cps`, barriers, undefined and
/// faulting encodings); keep them contiguous at the end (see [`Kind::ends_tb`]).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Undecoded = 0,
    /// Predecode-cache wrapper (never produced by the decoder) for the first instruction of a
    /// translation block that tlib cut short when it first translated it. The slot keeps every field
    /// of the instruction except `kind` and `raw`: `raw` indexes the core's `cut_entries`, which hold
    /// the instruction (and its kind) and the known cut lengths. Kinds up to and including this one
    /// are handled by the cold path of the hot loop, which executes the instruction from the slot.
    CutHead,
    /// Decoder-internal marker for coprocessor-space encodings (`imm` =
    /// `hw1 << 16 | hw2`); resolved by the cache fill and never executed.
    Coproc,
    /// VFP instruction; `imm` indexes the core's VFP instruction table.
    Vfp,

    // --- hints and system (translation block continues) ---------------------
    Nop,
    Sev,
    /// `rd`, `imm` = SYSm.
    Mrs,
    Clrex,

    // --- data processing, immediate ---------------------------------------
    MovImm,
    MvnImm,
    Movw,
    Movt,
    AndImm,
    BicImm,
    OrrImm,
    OrnImm,
    EorImm,
    TstImm,
    TeqImm,
    AddImm,
    AdcImm,
    SubImm,
    SbcImm,
    RsbImm,
    CmpImm,
    CmnImm,

    // --- data processing, register ----------------------------------------
    MovReg,
    MvnReg,
    AndReg,
    BicReg,
    OrrReg,
    OrnReg,
    EorReg,
    TstReg,
    TeqReg,
    AddReg,
    AdcReg,
    SubReg,
    SbcReg,
    RsbReg,
    CmpReg,
    CmnReg,
    LslImm,
    LsrImm,
    AsrImm,
    RorImm,
    Rrx,
    LslReg,
    LsrReg,
    AsrReg,
    RorReg,

    // --- multiply / divide -------------------------------------------------
    Mul,
    Mla,
    Mls,
    /// Long multiplies: `rd` = RdLo, `ra` = RdHi, `rn`, `rm`.
    Umull,
    Smull,
    Umlal,
    Smlal,
    Umaal,
    Sdiv,
    Udiv,

    // --- extend, bit manipulation, saturation ------------------------------
    /// `rd`, `rm`, `x` = rotation; the `*a*` forms add `rn`.
    Sxtb,
    Sxth,
    Uxtb,
    Uxth,
    Sxtab,
    Sxtah,
    Uxtab,
    Uxtah,
    Sxtb16,
    Uxtb16,
    Sxtab16,
    Uxtab16,
    Rev,
    Rev16,
    Revsh,
    Rbit,
    Clz,
    /// `rd`, `rn`, `x` = lsb, `ra` = msb (`Bfc`: `rn` unused).
    Bfi,
    Bfc,
    /// `rd`, `rn`, `x` = lsb, `ra` = width - 1.
    Ubfx,
    Sbfx,
    /// `rd`, `rn`, `imm` = saturate-to, `ra` = shift type, `x` = shift amount.
    Ssat,
    Usat,
    Ssat16,
    Usat16,
    /// `rd`, `rm` (first operand), `rn` (second operand).
    Qadd,
    Qsub,
    Qdadd,
    Qdsub,
    /// `rd`, `rn`, `rm`, `ra` = shift type, `x` = amount.
    Pkhbt,
    Pkhtb,
    /// `rd`, `rn`, `rm`, `x` = operation index (see `alu::parallel_addsub`).
    Parallel,
    Usad8,
    Usada8,
    Sel,

    // --- DSP multiplies ----------------------------------------------------
    /// `rd`, `rn`, `rm`, `ra`, `x` = variant bits (see exec).
    Smulxy,
    Smlaxy,
    Smulwy,
    Smlawy,
    Smlalxy,
    Smuad,
    Smusd,
    Smlad,
    Smlsd,
    Smlald,
    Smlsld,
    Smmul,
    Smmla,
    Smmls,

    // --- loads and stores --------------------------------------------------
    LdrImm,
    LdrbImm,
    LdrhImm,
    LdrsbImm,
    LdrshImm,
    StrImm,
    StrbImm,
    StrhImm,
    LdrReg,
    LdrbReg,
    LdrhReg,
    LdrsbReg,
    LdrshReg,
    StrReg,
    StrbReg,
    StrhReg,
    LdrLit,
    LdrbLit,
    LdrhLit,
    LdrsbLit,
    LdrshLit,
    /// `rd` = Rt, `ra` = Rt2, `rn`, `imm`, `FL_IDX`/`FL_WB`; literal form has
    /// `rn = 15` and `imm` = absolute address.
    LdrdImm,
    StrdImm,
    Ldm,
    Stm,
    Push,
    Pop,
    /// `Ldrex*`: `rd` = Rt, `rn`, `imm` = offset. `Strex*`: `rd` = Rd (status),
    /// `ra` = Rt, `rn`, `imm` = offset.
    Ldrex,
    Ldrexb,
    Ldrexh,
    Strex,
    Strexb,
    Strexh,
    /// `imm` = ITSTATE value (firstcond << 4 | mask). Does not end its translation block, but the hot loop
    /// must leave its plain mode after it: it sits directly in front of the block-ending kinds so that one
    /// comparison (`kind >= It`, [`FIRST_SPECIAL`]) separates the instructions that need a closer look from
    /// the plain ones. (Moving it renumbered the kinds in between, so the values of `Cpu::exactness_digest`,
    /// which hashes `kind as u8`, differ from those of builds before the move; they are comparable within one build
    /// and with a build whose kinds are numbered alike.)
    It,

    // --- translation-block ending kinds (contiguous; see the enum docs) ---------
    // branches
    B,
    Bcc,
    Bl,
    Bx,
    Blx,
    Cbz,
    Cbnz,
    /// `rn`, `rm`, `imm` = address of the instruction + 4 (value of PC).
    Tbb,
    Tbh,
    /// `MOV PC, Rm` (ALUWritePC, no interworking).
    MovToPc,
    /// `ADD PC, Rm`: `PC = (address + 4) + Rm`.
    AddToPc,
    /// Word load whose destination is PC (`LDR PC, ...`): `x` = 0 immediate,
    /// 1 register (`rm`, shift in `ra`), 2 literal (`imm` = absolute address).
    LdrToPc,
    /// `LDM` / `POP` whose register list contains PC (same fields as `Ldm` / `Pop`).
    LdmPc,
    PopPc,
    // system
    /// `imm` = SVC number.
    Svc,
    /// `imm` = BKPT immediate.
    Bkpt,
    Wfi,
    Wfe,
    /// `x` bits: 0 = F, 1 = I, 4 = disable (CPSID) instead of enable.
    Cps,
    /// `rn`, `imm` = SYSm, `x` = mask bits (hw2[11:10]).
    Msr,
    /// `DMB` / `DSB` / `ISB`: no effect on this core, but tlib ends the translation block.
    Barrier,
    /// The 16-bit `B .` (0xE7FE): tlib translates it as a WFI that retries the same instruction.
    BSelf,
    /// `VMSR FPSCR, Rt` (tlib ends the translation block after FPSCR writes); executed like `Vfp`.
    VfpEnd,
    /// Permanently undefined / unsupported encoding: UsageFault UNDEFINSTR.
    Undefined,
    /// Coprocessor instruction without a usable coprocessor: UsageFault NOCP.
    Nocp,
    /// Predecode-cache wrapper (never produced by the decoder) for a non-branching instruction that
    /// is the last one of a 1 KiB page: `imm` indexes the core's `page_ops` table that holds the real
    /// instruction. tlib ends the translation block after it ([`tb_page_end`]); wrapping it lets the
    /// hot loop learn that from the kind alone.
    PageEnd,
}

/// First translation-block ending kind.
pub const FIRST_TB_END: u8 = Kind::B as u8;

/// First kind the hot loop treats specially after executing it: [`Kind::It`] (the loop switches to IT block
/// mode) and every translation-block ending kind (the loop looks the next instruction up again).
pub const FIRST_SPECIAL: u8 = Kind::It as u8;

const _: () = assert!(FIRST_SPECIAL + 1 == FIRST_TB_END, "It must be the last kind before the block-ending ones");

impl Kind {
    /// True when Renode's translator ends the translation block after this instruction
    /// (independent of the page-boundary rule, see [`tb_page_end`]).
    #[inline(always)]
    pub fn ends_tb(self) -> bool {
        self as u8 >= FIRST_TB_END
    }
}

/// tlib's 1 KiB page rule: a translation block ends after the instruction whose end address
/// reaches or crosses the end of the page in which the block started. For sequential flow this
/// is equivalent to "the instruction touches a page boundary", independent of the block start.
#[inline(always)]
pub fn tb_page_end(pc: u32, len: u8) -> bool {
    (pc ^ pc.wrapping_add(len as u32)) >> 10 != 0
}

pub const FL_S: u8 = 0x01;
pub const FL_IT: u8 = 0x02;
pub const FL_IMMC: u8 = 0x04;
pub const FL_WB: u8 = 0x08;
pub const FL_IDX: u8 = 0x10;
pub const FL_DB: u8 = 0x20;
/// The operation writes SP and must keep SP<1:0> clear.
pub const FL_SPMASK: u8 = 0x40;
/// Branch instruction that must not trigger idle-loop analysis (set by the fast-forward
/// machinery after a loop was rejected).
pub const FL_NOFF: u8 = 0x80;

/// A predecoded instruction. See the module documentation for the field usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Op {
    pub kind: Kind,
    pub flags: u8,
    pub rd: u8,
    pub rn: u8,
    pub rm: u8,
    pub ra: u8,
    /// Instruction length in bytes (2 or 4).
    pub len: u8,
    pub x: u8,
    pub imm: u32,
    /// Raw encoding (`hw1 << 16 | hw2`, or the single halfword for 16-bit ops).
    pub raw: u32,
}

impl Op {
    pub const UNDECODED: Op = Op { kind: Kind::Undecoded, flags: 0, rd: 0, rn: 0, rm: 0, ra: 0, len: 2, x: 0, imm: 0, raw: 0 };

    #[inline]
    pub const fn new(kind: Kind, len: u8, raw: u32) -> Op {
        Op { kind, flags: 0, rd: 0, rn: 0, rm: 0, ra: 0, len, x: 0, imm: 0, raw }
    }

    pub fn is_32bit(&self) -> bool {
        self.len == 4
    }
}

const _: () = assert!(core::mem::size_of::<Op>() == 16);

/// Condition code evaluation table: bit `nzcv` of `COND_MASK[cond]` tells
/// whether the condition holds for the flag nibble N:Z:C:V (N = bit 3).
pub const COND_MASK: [u16; 16] = {
    let mut t = [0u16; 16];
    let mut cond = 0;
    while cond < 16 {
        let mut m = 0u16;
        let mut f = 0u32;
        while f < 16 {
            let n = (f >> 3) & 1 != 0;
            let z = (f >> 2) & 1 != 0;
            let c = (f >> 1) & 1 != 0;
            let v = f & 1 != 0;
            let pass = match cond >> 1 {
                0 => z,
                1 => c,
                2 => n,
                3 => v,
                4 => c && !z,
                5 => n == v,
                6 => n == v && !z,
                _ => true,
            };
            let pass = if cond & 1 == 1 && cond != 15 { !pass } else { pass };
            if pass {
                m |= 1 << f;
            }
            f += 1;
        }
        t[cond] = m;
        cond += 1;
    }
    t
};

/// Evaluates a condition code against `apsr` (N, Z, C, V in bits 31..28).
#[inline(always)]
pub fn cond_holds(cond: u32, apsr: u32) -> bool {
    (COND_MASK[(cond & 15) as usize] >> (apsr >> 28)) & 1 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Op>(), 16);
        assert_eq!(Kind::Undecoded as u8, 0);
        assert!((Kind::PageEnd as u16) < 256);
    }

    #[test]
    fn translation_block_end_kinds() {
        for k in [Kind::B, Kind::Bcc, Kind::Bl, Kind::Bx, Kind::Blx, Kind::Cbz, Kind::Cbnz, Kind::Tbb, Kind::Tbh, Kind::MovToPc, Kind::AddToPc, Kind::LdrToPc, Kind::PopPc, Kind::LdmPc, Kind::Svc, Kind::Bkpt, Kind::Wfi, Kind::Wfe, Kind::Cps, Kind::Msr, Kind::Barrier, Kind::BSelf, Kind::VfpEnd, Kind::Undefined, Kind::Nocp, Kind::PageEnd] {
            assert!(k.ends_tb(), "{k:?}");
        }
        for k in [Kind::Undecoded, Kind::CutHead, Kind::Coproc, Kind::Vfp, Kind::Nop, Kind::Sev, Kind::Mrs, Kind::Clrex, Kind::It, Kind::MovImm, Kind::AddReg, Kind::LdrImm, Kind::StrImm, Kind::Ldm, Kind::Stm, Kind::Push, Kind::Pop, Kind::Ldrex, Kind::Strexh] {
            assert!(!k.ends_tb(), "{k:?}");
        }
    }

    #[test]
    fn page_rule_is_start_independent() {
        // 16-bit instruction ending exactly on a page boundary, 32-bit straddling it, and plain cases.
        assert!(tb_page_end(0x3FE, 2));
        assert!(tb_page_end(0x3FC, 4));
        assert!(tb_page_end(0x3FE, 4));
        assert!(!tb_page_end(0x3FC, 2));
        assert!(!tb_page_end(0x400, 2));
        assert!(!tb_page_end(0x400, 4));
        assert!(tb_page_end(0x0800_03FE, 2));
        assert!(!tb_page_end(0x0800_0400, 4));
    }

    #[test]
    fn condition_codes() {
        let nzcv = |n: u32, z: u32, c: u32, v: u32| (n << 31) | (z << 30) | (c << 29) | (v << 28);
        // EQ/NE
        assert!(cond_holds(0, nzcv(0, 1, 0, 0)));
        assert!(!cond_holds(0, nzcv(0, 0, 0, 0)));
        assert!(cond_holds(1, nzcv(0, 0, 0, 0)));
        // CS/CC
        assert!(cond_holds(2, nzcv(0, 0, 1, 0)));
        assert!(cond_holds(3, nzcv(0, 0, 0, 0)));
        // MI/PL
        assert!(cond_holds(4, nzcv(1, 0, 0, 0)));
        assert!(cond_holds(5, nzcv(0, 0, 0, 0)));
        // VS/VC
        assert!(cond_holds(6, nzcv(0, 0, 0, 1)));
        assert!(cond_holds(7, nzcv(0, 0, 0, 0)));
        // HI: C && !Z ; LS: !C || Z
        assert!(cond_holds(8, nzcv(0, 0, 1, 0)));
        assert!(!cond_holds(8, nzcv(0, 1, 1, 0)));
        assert!(!cond_holds(8, nzcv(0, 0, 0, 0)));
        assert!(cond_holds(9, nzcv(0, 1, 1, 0)));
        assert!(cond_holds(9, nzcv(0, 0, 0, 0)));
        // GE: N == V ; LT: N != V
        assert!(cond_holds(10, nzcv(0, 0, 0, 0)));
        assert!(cond_holds(10, nzcv(1, 0, 0, 1)));
        assert!(cond_holds(11, nzcv(1, 0, 0, 0)));
        assert!(cond_holds(11, nzcv(0, 0, 0, 1)));
        // GT: !Z && N == V ; LE: Z || N != V
        assert!(cond_holds(12, nzcv(0, 0, 0, 0)));
        assert!(!cond_holds(12, nzcv(0, 1, 0, 0)));
        assert!(!cond_holds(12, nzcv(1, 0, 0, 0)));
        assert!(cond_holds(13, nzcv(0, 1, 0, 0)));
        assert!(cond_holds(13, nzcv(1, 0, 0, 0)));
        assert!(!cond_holds(13, nzcv(0, 0, 0, 0)));
        // AL
        assert!(cond_holds(14, 0));
        assert!(cond_holds(14, 0xF000_0000));
        // 0b1111 behaves as always for IT purposes
        assert!(cond_holds(15, 0));
    }
}
