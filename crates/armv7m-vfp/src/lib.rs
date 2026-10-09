//! FPv4-SP instruction decode/execute (Cortex-M4F floating point).
//! Re-exported by the core as `armv7m::vfp`.
//!
//! Contract owner: planning (`DESIGN.md`, section "CPU core").
//! Implementation owner: work package FPU. The core calls `decode` for every
//! coprocessor-space encoding and `execute` after performing CPACR/NOCP checks,
//! lazy FP state preservation and CONTROL.FPCA updates itself.
//!
//! Layout of the crate:
//! * [`soft`]  - exact integer-only reference implementation of every IEEE
//!   operation with Arm VFP semantics (all rounding modes, FZ, DN, AHP, NaN
//!   propagation, cumulative flags); never touches the host FPU.
//! * [`ieee`]  - the operations `execute` uses: identical to `soft`, with
//!   native `f32`/`f64` fast paths where they are provably equivalent.
//! * [`fpscr`] - FPSCR bit constants (including the Cortex-M4F write mask).
//! * [`disassemble`] - debug disassembler (Ghidra-style text).
//! * [`selftest`] - deterministic fast-path/exact-core agreement check with a
//!   reproducible checksum (the same on native and WebAssembly builds).
//!
//! Architectural choices (see also `decode.rs`):
//! * Semantics are the Arm ARM pseudocode (FPUnpack/FPRound/FPProcessNaNs/...):
//!   underflow is detected before rounding, `FZ` flushes inputs (IDC) and
//!   results (UFC), overflow sets OFC and IXC, NaN propagation is first
//!   signaling NaN else first quiet NaN else the default NaN, `DN` forces the
//!   default NaN. The results are cross-checked bit-for-bit (NaN payloads and
//!   all cumulative flags) against the AArch64 hardware FPU, which implements
//!   the same pseudocode, in every rounding mode / FZ / DN combination.
//! * Writable FPSCR bits are N,Z,C,V, AHP, DN, FZ, RMode and the cumulative
//!   flags ([`fpscr::WRITE_MASK`]); the M-profile unit never traps.
//! * UNPREDICTABLE encodings (PC as base/core register in stores and list
//!   transfers, empty/oversized register lists, FLDMX/FSTMX, non-zero SBZ
//!   bits, D16-D31) decode as [`VfpDecode::Undefined`].
//! * D-register loads/stores/moves (VLDR/VSTR/VLDM/VSTM/VPUSH/VPOP/VMOV) are
//!   supported; double-precision data processing is UNDEFINED on FPv4-SP.

mod decode;
mod disasm;
mod exec;
pub mod fpscr;
pub mod ieee;
pub mod selftest;
pub mod soft;
mod usage;

pub use decode::{decode, expand_imm_f32, VfpInsn};
pub use disasm::disassemble;
pub use exec::execute;
pub use usage::{Loc, MemUse, Usage};

/// S0..S31 as raw IEEE-754 bit patterns plus FPSCR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FpRegs {
    pub s: [u32; 32],
    pub fpscr: u32,
}

/// Memory access failure reported by the host; the core converts it into the
/// architectural fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfpFault {
    /// Unaligned word access (VLDR/VSTR/VLDM/VSTM/VPUSH/VPOP require word alignment).
    Unaligned(u32),
}

/// Core services used while executing a VFP instruction.
pub trait VfpHost {
    /// R0..R14 (R13 is the active SP). R15 is never requested; see `literal_base`.
    fn reg(&self, n: u32) -> u32;
    fn set_reg(&mut self, n: u32, value: u32);
    /// Replaces APSR N,Z,C,V with bits 31:28 of `nzcv` (VMRS APSR_nzcv, FPSCR).
    fn set_apsr_nzcv(&mut self, nzcv: u32);
    fn fp(&mut self) -> &mut FpRegs;
    /// Word load/store through the core's data path (performs MMIO side effects).
    fn load32(&mut self, addr: u32) -> u32;
    fn store32(&mut self, addr: u32, value: u32);
    /// Align(PC, 4) as seen by the current instruction (PC = instruction address + 4).
    fn literal_base(&self) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfpDecode {
    /// Not a CP10/CP11 encoding: the core raises a NOCP UsageFault.
    NotVfp,
    /// CP10/CP11 encoding that is UNDEFINED on FPv4-SP (e.g. double-precision
    /// arithmetic): the core raises an UNDEFINSTR UsageFault.
    Undefined,
    Insn(VfpInsn),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfpExec {
    Ok,
    Undefined,
    Fault(VfpFault),
}
