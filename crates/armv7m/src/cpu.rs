//! The Cortex-M4F core: state, public API and the run loop.
//!
//! # Execution and timing model (Renode 1.17.0 / tlib parity, DESIGN.md section 5)
//!
//! `Cpu::run` is one Renode *chunk* (`ExecuteInstructions`): it executes
//! `max(1, floor((until - now) / ticks_per_instruction))` instructions unless something ends it
//! earlier. Chunk ends, interrupt arbitration and the machine clock follow tlib:
//!
//! * pending exceptions are taken only at **translation-block boundaries** (chunk start, after an
//!   instruction that ends a tlib translation block - see [`Kind::ends_tb`] and
//!   [`tb_page_end`] - and at the chunk end); exception entry and return cost no instructions and
//!   a return re-arbitrates before the next instruction (tail-chaining);
//! * an asserted IRQ line (rising edge of the NVIC output), a board stop request, a SysTick/DWT
//!   register write (`LimitTimer` setters call `RequestReturn`), SLEEPONEXIT and a system reset
//!   request end the chunk at the end of the current translation block (after taking the
//!   exception, if any): `ExitReason::StopRequested`;
//! * `WFI`/`WFE` (and the branch-to-self `B .`, which tlib executes as WFI) end the chunk with
//!   `ExitReason::Sleeping` after counting as one executed instruction; the core is woken at the
//!   start of a later chunk when the NVIC reports a pending exception even if PRIMASK masks it
//!   (Renode's `MaskedInterruptPresent` quirk);
//! * the machine clock (SysTick, DWT) advances only when progress is reported: at the start of
//!   every `run` / `advance_idle` (the end of the previous chunk) and at explicit time syncs
//!   (SysTick `CVR` / DWT `CYCCNT` reads, `CpuBus::sync_time`). Register writes made in the middle
//!   of a chunk therefore take effect at the chunk start (Renode's clock-source lag).
//!
//! # Code structure
//!
//! The outer loop of `run` handles translation-block boundaries (`boundary`) and the slow paths
//! (loop verification, finishing a block after an event, resuming an IT block in a new chunk).
//! `fast_loop` is the tight inner loop: fetch a predecoded `Op`, dispatch, count; it only returns
//! when the chunk budget is exhausted or an event asked for the outer loop (`kick`). IT blocks run
//! inside it (a second mode of the loop, entered after the `IT` instruction, see `Cpu::set_fast_it`).

use crate::decode;
use crate::fastfwd::FastFwd;
use crate::nvic::{Nvic, SyncFault, EXC_NMI, EXC_SYSTICK, EXC_USAGEFAULT};
use crate::op::*;
use crate::timers::{Dwt, Effects, SysTick};
use crate::trace::Trace;
use crate::{vfp, CpuBus, CpuConfig, ExitReason, RunExit, BUS_IRQ_CHANGED, BUS_STOP_REQUESTED};
use emu_core::Time;

// CFSR bits (UFSR in bits 31:16).
pub const CFSR_UNDEFINSTR: u32 = 1 << 16;
pub const CFSR_INVSTATE: u32 = 1 << 17;
pub const CFSR_INVPC: u32 = 1 << 18;
pub const CFSR_NOCP: u32 = 1 << 19;
pub const CFSR_UNALIGNED: u32 = 1 << 24;
pub const CFSR_DIVBYZERO: u32 = 1 << 25;

pub const CCR_UNALIGN_TRP: u32 = 1 << 3;
pub const CCR_DIV_0_TRP: u32 = 1 << 4;
pub const CCR_STKALIGN: u32 = 1 << 9;

pub const CONTROL_NPRIV: u32 = 1;
pub const CONTROL_SPSEL: u32 = 2;
pub const CONTROL_FPCA: u32 = 4;

pub const FPCCR_LSPACT: u32 = 1 << 0;
pub const FPCCR_CLRONRET: u32 = 1 << 28;
pub const FPCCR_LSPEN: u32 = 1 << 30;
pub const FPCCR_ASPEN: u32 = 1 << 31;
/// Readiness bits tlib keeps in its "common" FPCCR slot: HFRDY, BFRDY, SFRDY, MONRDY.
const FPCCR_COMMON_READY: u32 = (1 << 4) | (1 << 6) | (1 << 7) | (1 << 8);
/// Readiness bits tlib keeps in the (non-secure) banked slot: MMRDY, UFRDY.
const FPCCR_BANKED_READY: u32 = (1 << 5) | (1 << 10);
/// What a read of the non-secure FPCCR returns from the common slot (SFRDY is not readable
/// without TrustZone), see `fpccr_read` in tlib's `cpu.h`.
const FPCCR_READ_COMMON: u32 = (1 << 4) | (1 << 6) | (1 << 8) | FPCCR_LSPEN | FPCCR_CLRONRET;
/// Bits of the non-secure slot a read returns: all but RES0 [25:11] and the TrustZone-only
/// S, TS, CLRONRETS and LSPENS bits.
const FPCCR_READ_NS: u32 = 0xD000_07FB;
/// Bits of the non-secure slot a write stores (LSPEN and CLRONRET go to the common slot).
const FPCCR_WRITE_NS: u32 = 0x83FF_FFFB;
/// APSR.Z.
pub const APSR_Z: u32 = 1 << 30;

/// tlib `ARM_M_EXC_RETURN_MIN`: program counter values from here up are EXC_RETURN magic values.
pub const EXC_RETURN_MIN: u32 = 0xFFFF_FF80;
/// tlib `ARM_M_FNC_RETURN_MIN`: interrupts are not taken while the PC is in the magic range.
pub const FNC_RETURN_MIN: u32 = 0xFEFF_FF00;
/// tlib `ARMV7M_LOCKUP_PC`: the PC of a locked-up core.
pub const LOCKUP_PC: u32 = 0xEFFF_FFFE;

/// A translation block that tlib cut short the first time it translated its start address (the
/// chunk budget ran out inside the block). tlib keeps such a block in its cache and reuses it for
/// every later lookup that has room for it - a longer block is never generated for that address
/// again - so the block stays split. `inner` is the instruction at the block start (the cache
/// slot holds a `Kind::CutHead` wrapper), `cuts` the known cut lengths in instructions, ascending.
#[derive(Clone, Debug)]
pub(crate) struct CutEntry {
    pub inner: Op,
    /// Index of the cache slot holding the wrapper.
    pub idx: usize,
    pub cuts: Vec<u32>,
}

impl CutEntry {
    /// tlib picks, among the blocks it has for an address, the largest one that fits the
    /// remaining chunk budget `m`.
    fn largest_cut_le(&self, m: u64) -> Option<u64> {
        self.cuts.iter().rev().map(|&l| l as u64).find(|&l| l <= m)
    }

    fn add_cut(&mut self, len: u32) {
        if let Err(pos) = self.cuts.binary_search(&len) {
            self.cuts.insert(pos, len);
        }
    }
}

/// The translation block the core is inside, when the cut model needs to know how it started:
/// the first instruction was translated for the first time (`entry == NO_CUT_ENTRY`) or has a
/// `CutEntry`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CutCtx {
    pub tb_icount: u64,
    pub pc: u32,
    pub entry: u32,
}

pub(crate) const NO_CUT_ENTRY: u32 = u32::MAX;

impl CutCtx {
    const NONE: CutCtx = CutCtx { tb_icount: u64::MAX, pc: 0, entry: NO_CUT_ENTRY };
}

/// System control block state outside the NVIC.
#[derive(Clone, Debug)]
pub(crate) struct Scb {
    pub vtor: u32,
    pub scr: u32,
    pub ccr: u32,
    pub cfsr: u32,
    pub mmfar: u32,
    pub bfar: u32,
    pub cpacr: u32,
    /// tlib `v7m.fpccr[NS]`: the stored software-writable bits (ASPEN, [10:0]) plus LSPACT and the
    /// MMRDY / UFRDY snapshot of the last lazy allocation.
    pub fpccr_ns: u32,
    /// tlib `v7m.fpccr[COMMON]`: LSPEN, CLRONRET and the HFRDY / BFRDY / SFRDY / MONRDY snapshot.
    pub fpccr_common: u32,
    pub fpcar: u32,
    pub fpdscr: u32,
    pub demcr: u32,
    pub mpu_ctrl: u32,
    pub mpu_rnr: u32,
    pub mpu_rbar: [u32; 8],
    pub mpu_rasr: [u32; 8],
}

impl Scb {
    /// `fpccr_read(env, false)` of tlib (Cortex-M4: no TrustZone, so the non-secure view is the
    /// only one): the readiness bits are whatever the last lazy allocation stored, software-written
    /// bits read back as written.
    pub fn fpccr(&self) -> u32 {
        (self.fpccr_ns & FPCCR_READ_NS) | (self.fpccr_common & FPCCR_READ_COMMON)
    }

    /// `fpccr_write(env, value, false)` of tlib.
    pub fn set_fpccr(&mut self, value: u32) {
        // LSPENS and CLRONRETS are always clear, so LSPEN and CLRONRET are writable.
        let own = FPCCR_LSPEN | FPCCR_CLRONRET;
        self.fpccr_common = (self.fpccr_common & !own) | (value & own);
        self.fpccr_ns = value & FPCCR_WRITE_NS;
    }

    pub fn fpccr_lspact(&self) -> bool {
        self.fpccr_ns & FPCCR_LSPACT != 0
    }

    pub fn fpccr_aspen(&self) -> bool {
        self.fpccr_ns & FPCCR_ASPEN != 0
    }

    pub fn fpccr_lspen(&self) -> bool {
        self.fpccr_common & FPCCR_LSPEN != 0
    }

    pub fn set_fpccr_lspact(&mut self, active: bool) {
        self.fpccr_ns = (self.fpccr_ns & !FPCCR_LSPACT) | active as u32;
    }

    /// tlib `fpccr_update` (lazy state preservation armed at exception entry): LSPACT is set, the
    /// USER and THREAD bits are rewritten and the readiness snapshot taken. Renode evaluates USER
    /// and THREAD after the NVIC acknowledged the new exception (`v7m.exception` is already
    /// non-zero), so both always end up clear - the interrupted mode is never recorded.
    pub fn fpccr_lazy_allocated(&mut self, ready: u32) {
        let user_thread = (1 << 1) | (1 << 3);
        self.fpccr_ns = (self.fpccr_ns | FPCCR_LSPACT) & !user_thread;
        self.fpccr_common = (self.fpccr_common & !FPCCR_COMMON_READY) | (ready & FPCCR_COMMON_READY);
        self.fpccr_ns = (self.fpccr_ns & !FPCCR_BANKED_READY) | (ready & FPCCR_BANKED_READY);
    }

    fn new() -> Self {
        Scb {
            vtor: 0,
            scr: 0,
            ccr: 0,
            cfsr: 0,
            mmfar: 0,
            bfar: 0,
            cpacr: 0,
            fpccr_ns: FPCCR_ASPEN,
            fpccr_common: FPCCR_LSPEN,
            fpcar: 0,
            fpdscr: 0,
            demcr: 0,
            mpu_ctrl: 0,
            mpu_rnr: 0,
            mpu_rbar: [0; 8],
            mpu_rasr: [0; 8],
        }
    }
}

/// Complete architectural register snapshot (debugger view).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegSnapshot {
    /// R0..R15 (R13 = active SP, R15 = address of the next instruction).
    pub r: [u32; 16],
    pub xpsr: u32,
    pub msp: u32,
    pub psp: u32,
    pub primask: bool,
    pub faultmask: bool,
    pub basepri: u8,
    pub control: u32,
    pub ipsr: u32,
    pub itstate: u8,
    pub icount: u64,
}

/// Statistics of the exact idle-loop fast-forward.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FastForwardStats {
    /// Loops verified as fixed points.
    pub loops: u64,
    /// Instructions skipped (accounted without executing).
    pub skipped_instructions: u64,
    /// Verification attempts that did not reach a fixed point.
    pub failed_verifications: u64,
}

pub struct Cpu {
    // ---- architectural state ------------------------------------------------
    /// R0..R15; `r[15]` is the address of the next instruction to execute and
    /// `r[13]` the active stack pointer.
    pub(crate) r: [u32; 16],
    /// APSR flags: N Z C V Q in bits 31..27.
    pub(crate) apsr: u32,
    /// GE[3:0] in bits 3:0.
    pub(crate) ge: u32,
    pub(crate) itstate: u8,
    /// EPSR.T (always 1 for correct code; 0 raises INVSTATE on execution).
    pub(crate) thumb: bool,
    pub(crate) ipsr: u32,
    pub(crate) control: u32,
    /// The inactive stack pointer (PSP while MSP is active and vice versa).
    pub(crate) sp_other: u32,
    pub(crate) use_psp: bool,
    pub(crate) exclusive: Option<u32>,
    pub(crate) fp: vfp::FpRegs,

    // ---- system -----------------------------------------------------------------
    pub(crate) cfg: CpuConfig,
    pub(crate) nvic: Nvic,
    pub(crate) systick: SysTick,
    pub(crate) dwt: Dwt,
    pub(crate) scb: Scb,

    // ---- run state ------------------------------------------------------------------
    pub(crate) icount: u64,
    /// Inner loop bound; zero forces the inner loop to return to the outer loop.
    pub(crate) limit: u64,
    /// Retire count at which the current chunk ends.
    pub(crate) budget_end: u64,
    /// Virtual time at the start of the current chunk and the retire count there.
    pub(crate) slice_time: Time,
    pub(crate) slice_icount: u64,
    /// Retire count at the start of the current translation block. This is what tlib's
    /// `tlib_get_executed_instructions` reports to Renode's `SyncTime()` (a block is added to the
    /// executed-instruction count only when the next block starts), so it is the `icount` the bus
    /// sees for every access and the instant a SysTick `CVR` / DWT `CYCCNT` read observes.
    pub(crate) tb_icount: u64,
    /// Machine clock time: SysTick and DWT have been advanced up to here.
    pub(crate) clock_time: Time,
    /// tlib `env->wfi` / `env->wfe`: the core sleeps until `has_work` says otherwise.
    pub(crate) wfi: bool,
    pub(crate) wfe: bool,
    /// tlib `exception_index == EXCP_WFI`: the chunk ends at the next translation-block boundary.
    pub(crate) wfi_exit: bool,
    /// tlib `was_not_working` (drives the WFI state-change hook of the NVIC).
    pub(crate) was_not_working: bool,
    /// Renode NVIC `InSleep` / `InDeepSleep` outputs (set by the WFI state-change hook).
    pub(crate) in_sleep: bool,
    pub(crate) in_deep_sleep: bool,
    /// How many times the core went to sleep with `SCR.SLEEPDEEP` set (the same hook as `in_deep_sleep`). A pure observation counter:
    /// it never feeds back into execution and survives `reset` (it only grows), so a host can ask at its own boundaries whether
    /// a deep sleep happened since it looked last (DESIGN.md 20.2: standby of a custom build).
    pub(crate) deep_sleep_entries: u64,
    /// tlib `sleep_on_exception_exit` (SCR.SLEEPONEXIT).
    pub(crate) sleep_on_exit: bool,
    pub(crate) halted: bool,
    /// tlib `exit_request`: end the chunk at the next translation-block boundary.
    pub(crate) exit_pending: bool,
    /// The current instruction aborted its translation block (fault, lockup).
    pub(crate) force_tb_end: bool,
    /// The event register (SEV, exception return, SEVONPEND).
    pub(crate) event_flag: bool,
    pub(crate) reset_requested: bool,
    pub(crate) lockup: Option<String>,
    /// The current instruction raised a fault (ITSTATE must not advance).
    pub(crate) insn_faulted: bool,
    /// The current instruction replaced ITSTATE (exception entry / return).
    pub(crate) it_changed: bool,
    /// EXC_RETURN of an invalid exception return whose UsageFault re-uses the existing frame.
    pub(crate) pending_tailchain: Option<u32>,
    /// First undefined / unimplemented encodings executed: `(address, raw encoding)`.
    pub(crate) undef_log: Vec<(u32, u32)>,

    // ---- predecode cache -----------------------------------------------------------
    /// One slot per halfword of the cached region plus two permanently `Undecoded` guard slots
    /// (`cache_span / 2 + 2` entries; `fast_loop` runs off the end into a guard slot).
    pub(crate) cache: Vec<Op>,
    pub(crate) cache_base: u32,
    /// Size of the cached region in bytes (0 = no cache).
    pub(crate) cache_span: u32,
    pub(crate) vfp_table: Vec<vfp::VfpInsn>,
    pub(crate) vfp_scratch: Option<vfp::VfpInsn>,
    /// The instructions behind `Kind::PageEnd` cache slots.
    pub(crate) page_ops: Vec<Op>,
    /// Blocks cut short at their first translation (see [`CutEntry`]), behind `Kind::CutHead` slots.
    pub(crate) cut_entries: Vec<CutEntry>,
    /// How the current translation block started, for the cut model.
    pub(crate) cut_ctx: CutCtx,
    /// Retire count at which the current block ends because of a known cut, valid while
    /// `cut_tb == tb_icount`.
    pub(crate) cut_icount: u64,
    pub(crate) cut_tb: u64,

    // ---- options ------------------------------------------------------------------------
    /// Renode parity: tlib aligns the exception frame regardless of CCR.STKALIGN.
    pub(crate) stkalign_always: bool,
    /// Renode parity: NVIC `FilterCcrDiv0Write` blocks CCR.DIV_0_TRP writes.
    pub(crate) filter_ccr_div0: bool,
    /// Renode parity: `HaltSystickOnDeepSleep`.
    pub(crate) halt_systick_on_deep_sleep: bool,
    /// IT blocks run inside `fast_loop` (default). Off: the `IT` instruction leaves the loop and the
    /// block is stepped by `step_insn`; the guest-visible result is identical (host speed only).
    pub(crate) fast_it: bool,
    pub(crate) ff: FastFwd,
    /// ROUTINE-ACCEL (PERF-HLE, DESIGN.md 16.2): exact memoization of known runtime-library routines.
    pub(crate) accel: crate::accel::Accel,
    pub(crate) trace: Trace,
    warnings: Vec<String>,
    warned: Vec<u32>,
}

impl Cpu {
    pub fn new(config: CpuConfig) -> Self {
        let mut cpu = Cpu {
            r: [0; 16],
            apsr: 0,
            ge: 0,
            itstate: 0,
            thumb: true,
            ipsr: 0,
            control: 0,
            sp_other: 0,
            use_psp: false,
            exclusive: None,
            fp: vfp::FpRegs::default(),
            cfg: config,
            nvic: Nvic::new(config.num_irqs, config.priority_mask),
            systick: SysTick::new(config.systick_hz),
            dwt: Dwt::new(config.dwt_hz),
            scb: Scb::new(),
            icount: 0,
            limit: 0,
            budget_end: 0,
            slice_time: 0,
            slice_icount: 0,
            tb_icount: 0,
            clock_time: 0,
            wfi: false,
            wfe: false,
            wfi_exit: false,
            was_not_working: false,
            in_sleep: false,
            in_deep_sleep: false,
            deep_sleep_entries: 0,
            sleep_on_exit: false,
            halted: false,
            exit_pending: false,
            force_tb_end: false,
            event_flag: false,
            reset_requested: false,
            lockup: None,
            insn_faulted: false,
            it_changed: false,
            pending_tailchain: None,
            undef_log: Vec::new(),
            cache: Vec::new(),
            cache_base: 0,
            cache_span: 0,
            vfp_table: Vec::new(),
            vfp_scratch: None,
            page_ops: Vec::new(),
            cut_entries: Vec::new(),
            cut_ctx: CutCtx::NONE,
            cut_icount: 0,
            cut_tb: u64::MAX,
            stkalign_always: true,
            filter_ccr_div0: true,
            halt_systick_on_deep_sleep: true,
            fast_it: true,
            ff: FastFwd::new(),
            accel: crate::accel::Accel::new(),
            trace: Trace::new(),
            warnings: Vec::new(),
            warned: Vec::new(),
        };
        cpu.reset();
        cpu
    }

    /// Core + system control space reset. Does not load SP/PC; the board sets
    /// VTOR/SP/PC explicitly like the Renode .resc scripts.
    pub fn reset(&mut self) {
        self.r = [0; 16];
        // tlib's `cpu_reset` zeroes its flag cache, which stores the Z flag inverted (`ZF == 0`
        // means Z set): the core leaves reset with APSR.Z = 1 (xPSR 0x41000000 with T) until the
        // first flag-setting instruction.
        self.apsr = APSR_Z;
        self.ge = 0;
        self.itstate = 0;
        self.thumb = true;
        self.ipsr = 0;
        self.control = 0;
        self.sp_other = 0;
        self.use_psp = false;
        self.exclusive = None;
        self.fp = vfp::FpRegs::default();
        self.nvic.reset();
        self.systick.reset(self.clock_time);
        self.dwt.reset(self.clock_time);
        self.scb = Scb::new();
        self.limit = 0;
        self.wfi = false;
        self.wfe = false;
        self.wfi_exit = false;
        self.was_not_working = false;
        self.in_sleep = false;
        self.in_deep_sleep = false;
        self.sleep_on_exit = false;
        self.exit_pending = false;
        self.force_tb_end = false;
        self.event_flag = false;
        self.reset_requested = false;
        self.lockup = None;
        self.insn_faulted = false;
        self.it_changed = false;
        self.pending_tailchain = None;
        self.tb_icount = self.icount;
        // tlib's reset flushes the translation block cache.
        self.clear_cut_history();
        self.ff.reset();
    }

    pub fn set_vtor(&mut self, vtor: u32) {
        self.scb.vtor = vtor & 0xFFFF_FF80;
    }

    /// Sets MSP (and SP when MSP is active).
    pub fn set_sp(&mut self, sp: u32) {
        let sp = sp & !3;
        if self.use_psp {
            self.sp_other = sp;
        } else {
            self.r[13] = sp;
        }
    }

    /// Sets the PC (bit 0 is ignored; execution is Thumb).
    pub fn set_pc(&mut self, pc: u32) {
        self.r[15] = pc & !1;
        self.thumb = true;
        self.kick();
    }

    pub fn pc(&self) -> u32 {
        self.r[15]
    }

    /// R0..R15 as the debugger sees them.
    pub fn reg(&self, n: usize) -> u32 {
        self.r[n & 15]
    }

    /// Debugger register write (R13 writes the active SP; bits [1:0] are cleared).
    pub fn set_reg(&mut self, n: usize, value: u32) {
        match n & 15 {
            13 => self.r[13] = value & !3,
            15 => self.set_pc(value),
            i => self.r[i] = value,
        }
    }

    /// Total executed instructions since construction (monotonic).
    pub fn instructions(&self) -> u64 {
        self.icount
    }

    // ---- program status -----------------------------------------------------------------

    /// Composes xPSR (APSR flags, GE, ITSTATE, T, IPSR).
    pub fn xpsr(&self) -> u32 {
        let it = self.itstate as u32;
        (self.apsr & 0xF800_0000) | ((self.ge & 0xF) << 16) | ((it & 3) << 25) | ((it >> 2) << 10) | ((self.thumb as u32) << 24) | (self.ipsr & 0x1FF)
    }

    pub fn apsr(&self) -> u32 {
        self.apsr & 0xF800_0000 | ((self.ge & 0xF) << 16)
    }

    /// Debugger write of the APSR (N Z C V Q in bits 31:27, GE in bits 19:16).
    pub fn set_apsr(&mut self, v: u32) {
        self.apsr = v & 0xF800_0000;
        self.ge = (v >> 16) & 0xF;
    }

    pub fn ipsr(&self) -> u32 {
        self.ipsr
    }

    pub fn itstate(&self) -> u8 {
        self.itstate
    }

    pub fn control(&self) -> u32 {
        self.control
    }

    /// Debugger write of CONTROL (nPRIV, SPSEL, FPCA).
    pub fn set_control(&mut self, v: u32) {
        self.control = v & 7;
        self.update_sp_selection();
        self.kick();
    }

    pub fn msp(&self) -> u32 {
        if self.use_psp {
            self.sp_other
        } else {
            self.r[13]
        }
    }

    pub fn psp(&self) -> u32 {
        if self.use_psp {
            self.r[13]
        } else {
            self.sp_other
        }
    }

    pub fn primask(&self) -> bool {
        self.nvic.primask
    }

    pub fn faultmask(&self) -> bool {
        self.nvic.faultmask
    }

    pub fn basepri(&self) -> u8 {
        self.nvic.basepri_raw
    }

    pub fn snapshot(&self) -> RegSnapshot {
        RegSnapshot {
            r: self.r,
            xpsr: self.xpsr(),
            msp: self.msp(),
            psp: self.psp(),
            primask: self.nvic.primask,
            faultmask: self.nvic.faultmask,
            basepri: self.nvic.basepri_raw,
            control: self.control,
            ipsr: self.ipsr,
            itstate: self.itstate,
            icount: self.icount,
        }
    }

    pub fn fp_regs(&self) -> &vfp::FpRegs {
        &self.fp
    }

    pub fn fp_regs_mut(&mut self) -> &mut vfp::FpRegs {
        &mut self.fp
    }

    /// `true` in Handler mode (IPSR != 0).
    #[inline]
    pub(crate) fn handler_mode(&self) -> bool {
        self.ipsr != 0
    }

    #[inline]
    pub(crate) fn privileged(&self) -> bool {
        self.ipsr != 0 || self.control & CONTROL_NPRIV == 0
    }

    // ---- stack pointer banking ------------------------------------------------------------

    /// Selects the active stack pointer (`true` = PSP).
    pub(crate) fn select_sp(&mut self, psp: bool) {
        if psp != self.use_psp {
            core::mem::swap(&mut self.r[13], &mut self.sp_other);
            self.use_psp = psp;
        }
    }

    /// Re-evaluates which SP is active after CONTROL.SPSEL or the mode changed.
    pub(crate) fn update_sp_selection(&mut self) {
        let want_psp = self.ipsr == 0 && self.control & CONTROL_SPSEL != 0;
        self.select_sp(want_psp);
    }

    // ---- options and diagnostics ---------------------------------------------------------------

    /// Enables/disables exact idle-loop fast-forward (DESIGN.md section 6).
    /// Results must be identical either way; only host speed changes.
    pub fn set_idle_fast_forward(&mut self, enabled: bool) {
        self.ff.user_enabled = enabled;
        self.ff.refresh(self.trace.enabled());
    }

    pub fn idle_fast_forward(&self) -> bool {
        self.ff.user_enabled
    }

    pub fn fast_forward_stats(&self) -> FastForwardStats {
        self.ff.stats
    }

    /// Renode parity switch: when `true` (default) the exception frame is always
    /// 8-byte aligned like Renode's tlib; when `false` CCR.STKALIGN decides.
    pub fn set_renode_stkalign_parity(&mut self, always_align: bool) {
        self.stkalign_always = always_align;
    }

    /// Renode parity switch: when `true` (default) writes of CCR.DIV_0_TRP are ignored
    /// (Renode NVIC `FilterCcrDiv0Write`).
    pub fn set_filter_ccr_div0_write(&mut self, filter: bool) {
        self.filter_ccr_div0 = filter;
    }

    /// Renode parity switch (`HaltSystickOnDeepSleep`, default true).
    pub fn set_halt_systick_on_deep_sleep(&mut self, v: bool) {
        self.halt_systick_on_deep_sleep = v;
    }

    /// Speed switch (default on): IT blocks execute inside the hot loop instead of being stepped one
    /// instruction at a time by the outer loop. Results are identical either way; the off setting is
    /// the reference for shadow verification (`tests/fast_it.rs`).
    pub fn set_fast_it(&mut self, enabled: bool) {
        self.fast_it = enabled;
    }

    pub fn fast_it(&self) -> bool {
        self.fast_it
    }

    /// Drops all predecoded instructions (call after flash contents change).
    pub fn invalidate_code_cache(&mut self) {
        for op in self.cache.iter_mut() {
            *op = Op::UNDECODED;
        }
        self.vfp_table.clear();
        self.page_ops.clear();
        self.cut_entries.clear();
        self.cut_ctx = CutCtx::NONE;
        self.ff.reset();
        self.accel_invalidate(); // ROUTINE-ACCEL HOOK: the memo tables describe the old code
    }

    /// Forgets the cut translation blocks (their cache slots get the plain instruction back).
    fn clear_cut_history(&mut self) {
        for e in std::mem::take(&mut self.cut_entries) {
            if let Some(slot) = self.cache.get_mut(e.idx) {
                if slot.kind == Kind::CutHead {
                    *slot = e.inner;
                }
            }
        }
        self.cut_ctx = CutCtx::NONE;
    }

    /// True when the firmware enabled the MPU (permissions are not enforced by this model).
    pub fn mpu_enabled(&self) -> bool {
        self.scb.mpu_ctrl & 1 != 0
    }

    /// Undefined / unimplemented encodings executed so far (first 64 distinct addresses).
    pub fn undefined_log(&self) -> &[(u32, u32)] {
        &self.undef_log
    }

    pub(crate) fn log_undefined(&mut self, pc: u32, raw: u32) {
        if self.undef_log.len() < 64 && !self.undef_log.iter().any(|&(a, _)| a == pc) {
            self.undef_log.push((pc, raw));
        }
    }

    /// Why the core is locked up, if it is.
    pub fn lockup_reason(&self) -> Option<&str> {
        self.lockup.as_deref()
    }

    pub fn is_locked_up(&self) -> bool {
        self.lockup.is_some()
    }

    /// True once after the firmware requested a system reset (AIRCR.SYSRESETREQ);
    /// `run` returned `ExitReason::StopRequested` for it.
    pub fn take_reset_request(&mut self) -> bool {
        core::mem::take(&mut self.reset_requested)
    }

    /// Debug summary of the NVIC / execution state (Renode-style `Summary`).
    pub fn summary(&self) -> String {
        format!(
            "pc=0x{:08x} xpsr=0x{:08x} ipsr={} control=0x{:x} sleeping={} halted={} lockup={:?} clock={} | {}",
            self.r[15],
            self.xpsr(),
            self.ipsr,
            self.control,
            self.is_sleeping(),
            self.halted,
            self.lockup,
            self.clock_time,
            self.nvic.summary()
        )
    }

    /// Drains the diagnostics emitted so far (first occurrence of each condition only).
    pub fn take_warnings(&mut self) -> Vec<String> {
        core::mem::take(&mut self.warnings)
    }

    /// Records a warning once per `key` (typically an address).
    pub(crate) fn warn_once(&mut self, key: u32, msg: impl FnOnce() -> String) {
        if self.warned.contains(&key) || self.warned.len() >= 512 {
            return;
        }
        self.warned.push(key);
        if self.warnings.len() < 512 {
            self.warnings.push(msg());
        }
    }

    // ---- interrupt interface ----------------------------------------------------------------

    /// External interrupt input `irq` (NVIC IRQn numbering, as in `-> nvic@N`).
    pub fn set_irq_line(&mut self, irq: u32, level: bool) {
        self.apply_irq_line(irq, level);
        self.nvic_changed();
    }

    pub(crate) fn apply_irq_line(&mut self, irq: u32, level: bool) {
        if self.nvic.set_irq_line(irq, level) {
            self.systick_wake();
        }
    }

    /// Renode `NVIC.OnGPIO`: an asserted external line with a pending candidate runs
    /// `systick.Enabled |= true`, which also re-enables a SysTick halted by deep sleep.
    fn systick_wake(&mut self) {
        let fx = self.systick.set_enable(true);
        self.apply_timer_effects(fx);
    }

    /// Applies the effects of a SysTick register operation: a touched `LimitTimer` ends the chunk
    /// at the end of the translation block (`RequestReturn`), a limit reached by the zero-time
    /// update pends the SysTick exception.
    pub(crate) fn apply_timer_effects(&mut self, fx: Effects) {
        if fx.touched {
            self.exit_pending = true;
            self.kick();
        }
        if fx.pend {
            self.nvic.set_pending_irq(EXC_SYSTICK);
            self.nvic_changed();
        }
    }

    /// Recomputes the pending-exception state; the NVIC output line may change, in which case the
    /// inner loop is left so that the outer loop can react at the end of the translation block.
    /// A rising edge of the line is tlib's `exit_request`.
    #[inline]
    pub(crate) fn nvic_changed(&mut self) {
        self.nvic.find_pending();
        if self.nvic.irq_rose {
            self.nvic.irq_rose = false;
            self.exit_pending = true;
            self.limit = 0;
        }
        if self.nvic.irq_line {
            self.limit = 0;
        }
        if self.nvic.sev_pending_event {
            self.nvic.sev_pending_event = false;
            self.event_flag = true;
        }
    }

    /// Forces the inner loop to return to the outer loop after the current instruction.
    #[inline(always)]
    pub(crate) fn kick(&mut self) {
        self.limit = 0;
    }

    pub fn set_halted(&mut self, halted: bool) {
        self.halted = halted;
        self.kick();
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// True while the core sleeps in WFI / WFE (it wakes at the start of a later chunk).
    pub fn is_sleeping(&self) -> bool {
        self.wfi || self.wfe
    }

    /// How many times the core went to sleep with `SCR.SLEEPDEEP` set since it was created (an observation counter that only grows,
    /// also across `reset`; the NVIC's `InDeepSleep` transition). A host that looks at it at its own boundaries learns whether a
    /// deep sleep started since the last look even if an interrupt has woken the core again.
    pub fn deep_sleep_entries(&self) -> u64 {
        self.deep_sleep_entries
    }

    // ---- time -----------------------------------------------------------------------------------

    /// Exact virtual time at the retire count `ic` within the current run slice.
    #[inline(always)]
    pub(crate) fn time_at(&self, ic: u64) -> Time {
        self.slice_time + (ic - self.slice_icount) * self.cfg.ticks_per_instruction
    }

    /// Machine clock time: the time up to which the core's SysTick and DWT counter have been
    /// advanced (the end of the last chunk, or the last explicit sync).
    pub fn clock_time(&self) -> Time {
        self.clock_time
    }

    /// Advances the machine clock to `to` (`ReportProgress`): SysTick and the DWT counter count,
    /// expiries fire at their own (ceil-rounded) tick.
    pub(crate) fn advance_clock(&mut self, to: Time) {
        if to <= self.clock_time {
            return;
        }
        // (The DWT counter never raises an event and is read as a function of the clock time, so it is not advanced
        // here: reads and writes of its registers catch it up, see `cyccnt_at` and `ppb_write_word`.)
        if self.systick.advance_to(to) {
            self.nvic.set_pending_irq(EXC_SYSTICK);
            self.nvic_changed();
        }
        self.clock_time = to;
    }

    /// Renode `cpu.SyncTime()` for a register read in the middle of a chunk: the machine clock
    /// advances to the start of the current *translation block*. tlib adds a block to the
    /// executed-instruction count that `SyncTime` reports only when the next block starts, so the
    /// k-th instruction of a block sees the time `k * ticks_per_instruction` earlier than its own.
    pub(crate) fn sync_time<B: CpuBus>(&mut self, bus: &mut B) {
        bus.sync_time(self.tb_icount);
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
        let t = self.time_at(self.tb_icount);
        self.advance_clock(t);
    }

    /// Earliest core-internal deadline (SysTick expiry, absolute time), if any. Exact as of the
    /// last `run` / `advance_idle` call.
    pub fn next_internal_deadline(&self) -> Option<Time> {
        self.systick.deadline()
    }

    /// Advances the machine clock across a period with no instruction execution (the core is
    /// sleeping, locked up or halted): SysTick and DWT count, expiries pend their exceptions.
    pub fn advance_idle(&mut self, _now: Time, until: Time) {
        self.advance_clock(until);
    }

    /// Side-effect-free debugger read of a private-peripheral-bus register
    /// (e.g. SCB.SCR at 0xE000_ED10). `None` if `addr` is outside the PPB. Timer registers
    /// show the value at `now` (clock time if later).
    pub fn ppb_peek32(&self, addr: u32, now: Time) -> Option<u32> {
        if addr >> 20 != 0xE00 {
            return None;
        }
        Some(self.ppb_peek_word(addr & !3, now))
    }

    /// Debugger write of a private-peripheral-bus register at virtual time `now`
    /// (same effect as a store by the core, e.g. `ICSR.PENDSVSET`). No-op outside the PPB.
    pub fn ppb_poke32(&mut self, addr: u32, value: u32, now: Time) {
        if addr >> 20 != 0xE00 {
            return;
        }
        self.advance_clock(now);
        self.ppb_write_word(addr & !3, value);
    }

    // ---- predecode cache ------------------------------------------------------------------------

    /// Slow path of `fetch_op`: no cached slot (new region or uncached code).
    #[cold]
    #[inline(never)]
    pub(crate) fn fetch_op_slow<B: CpuBus>(&mut self, bus: &mut B, pc: u32) -> Op {
        self.ensure_cache_region(bus, pc);
        let off = pc.wrapping_sub(self.cache_base);
        if off < self.cache_span {
            return self.fill_slot(bus, pc, (off >> 1) as usize);
        }
        // Code outside any cached region (SRAM, peripherals): decode on every execution.
        let hw1 = bus.fetch16(pc);
        let hw2 = if decode::is_32bit(hw1) { bus.fetch16(pc.wrapping_add(2)) } else { 0 };
        self.finish_decode(pc, hw1, hw2, false)
    }

    /// Makes the predecode cache cover the code region that contains `pc` (a different region
    /// replaces the cache and everything derived from it).
    #[cold]
    #[inline(never)]
    pub(crate) fn ensure_cache_region<B: CpuBus>(&mut self, bus: &mut B, pc: u32) {
        if let Some((base, bytes)) = bus.code_region(pc) {
            let halfwords = bytes.len() / 2;
            if self.cache_span as usize != halfwords * 2 || self.cache_base != base {
                self.cache.clear();
                // Two guard slots behind the last halfword (see `fast_loop`).
                self.cache.resize(halfwords + 2, Op::UNDECODED);
                self.cache_base = base;
                self.cache_span = (halfwords * 2) as u32;
                self.vfp_table.clear();
                self.page_ops.clear();
                self.cut_entries.clear();
                self.cut_ctx = CutCtx::NONE;
                self.ff.reset();
                self.accel_invalidate(); // ROUTINE-ACCEL HOOK
            }
        }
    }

    /// Fills cache slot `idx` for `pc` (cache hit path calls this only on `Undecoded`). An
    /// instruction that cannot be cached (no code region, or a 32-bit encoding whose second
    /// halfword lies behind the end of the region and is fetched from the bus) is returned
    /// decoded but leaves the slot `Undecoded`.
    #[cold]
    #[inline(never)]
    pub(crate) fn fill_slot<B: CpuBus>(&mut self, bus: &mut B, pc: u32, idx: usize) -> Op {
        let (hw1, hw2_in_region) = match bus.code_region(pc) {
            Some((base, bytes)) => {
                let off = pc.wrapping_sub(base) as usize;
                let rd16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
                // `pc` is inside the region (the caller checked), so the first halfword is too.
                let hw1 = if off + 1 < bytes.len() { rd16(off) } else { 0 };
                (hw1, if off + 3 < bytes.len() { Some(rd16(off + 2)) } else { None })
            }
            None => return Op::new(Kind::Undefined, 2, 0),
        };
        let op = if !decode::is_32bit(hw1) {
            self.finish_decode(pc, hw1, 0, true)
        } else {
            match hw2_in_region {
                Some(hw2) => self.finish_decode(pc, hw1, hw2, true),
                None => {
                    let hw2 = bus.fetch16(pc.wrapping_add(2));
                    return self.finish_decode(pc, hw1, hw2, false);
                }
            }
        };
        if !op.kind.ends_tb() && tb_page_end(pc, op.len) {
            // The last instruction of a 1 KiB page ends its translation block like a branch does:
            // the cache holds a wrapper of a TB-ending kind so that the hot loop needs no page test.
            let index = self.page_ops.len() as u32;
            self.page_ops.push(op);
            self.cache[idx] = Op { kind: Kind::PageEnd, imm: index, ..op };
        } else {
            self.cache[idx] = op;
        }
        op
    }

    /// Decodes and resolves coprocessor encodings through the VFP decoder.
    fn finish_decode(&mut self, pc: u32, hw1: u16, hw2: u16, cached: bool) -> Op {
        let mut op = decode::decode(pc, hw1, hw2);
        if op.kind == Kind::Coproc {
            match vfp::decode(hw1, hw2) {
                vfp::VfpDecode::NotVfp => op.kind = Kind::Nocp,
                vfp::VfpDecode::Undefined => op.kind = Kind::Undefined,
                vfp::VfpDecode::Insn(insn) => {
                    // `VMSR FPSCR, Rt` ends tlib's translation block (gen_lookup_tb after the write).
                    op.kind = if hw1 == 0xEEE1 && hw2 & 0x0FFF == 0x0A10 { Kind::VfpEnd } else { Kind::Vfp };
                    if cached {
                        op.imm = self.vfp_table.len() as u32;
                        self.vfp_table.push(insn);
                    } else {
                        op.imm = u32::MAX;
                        self.vfp_scratch = Some(insn);
                    }
                }
            }
        }
        op
    }

    /// Returns the predecoded instruction at `pc`.
    #[inline(always)]
    pub(crate) fn fetch_op<B: CpuBus>(&mut self, bus: &mut B, pc: u32) -> Op {
        let off = pc.wrapping_sub(self.cache_base);
        if off < self.cache_span {
            let idx = (off >> 1) as usize;
            debug_assert!(idx < self.cache.len());
            // SAFETY: `cache_span` is at most twice `cache.len()` and `off < cache_span`.
            let op = unsafe { *self.cache.get_unchecked(idx) };
            if op.kind != Kind::Undecoded {
                if op.kind == Kind::PageEnd {
                    return self.page_ops[op.imm as usize];
                }
                if op.kind == Kind::CutHead {
                    return self.cut_entries[op.raw as usize].inner;
                }
                return op;
            }
            return self.fill_slot(bus, pc, idx);
        }
        self.fetch_op_slow(bus, pc)
    }

    // ---- run loop -----------------------------------------------------------------------------------

    /// One Renode chunk (`ExecuteInstructions`): executes from virtual time `now`
    /// `max(1, floor((until - now) / ticks_per_instruction))` instructions, where `until` is
    /// first reduced to the core's own SysTick deadline when that comes earlier. Chunks of
    /// 100 us quanta, clock events and sleeping are driven by the board, see the module docs.
    ///
    /// The chunk ends early, at the end of the current translation block, with
    /// `StopRequested`, `Sleeping`, `Lockup` or `Halted`. `RunExit::now` is
    /// `now + executed * ticks_per_instruction`; after a `Deadline` exit it can be earlier than
    /// `until` (the remaining sub-instruction time is run as a one-instruction chunk, like
    /// Renode) or later (an event fell inside the last instruction).
    ///
    /// # Translation blocks
    ///
    /// Interrupts are taken, and `CpuBus` accesses are timed (`icount`), at the boundaries of the
    /// translation blocks of Renode's translator (tlib). A block starts with a chunk and after every
    /// instruction that ends one: any branch or PC write (taken or not), `wfi`/`wfe`, `svc`, `msr`,
    /// `cps`, barriers, undefined encodings, and the last instruction of a 1 KiB page. tlib also cuts
    /// a block where a chunk's instruction budget ends, caches the cut block, and never regenerates a
    /// longer one for that start address if the address was translated for the first time under that
    /// budget (any later lookup that has room for the cut block finds it): such a block stays split
    /// at the cut for the rest of the run. The core reproduces this for the first translation of an
    /// address (`CutEntry`); the effects of tlib's chained blocks, of smaller budgets at already
    /// translated addresses, of the block size limit and of flushes other than a reset or
    /// `invalidate_code_cache` are not modeled.
    pub fn run<B: CpuBus>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit {
        if self.trace.enabled() {
            self.run_impl::<B, true>(bus, now, until)
        } else {
            self.run_impl::<B, false>(bus, now, until)
        }
    }

    fn run_impl<B: CpuBus, const TRACE: bool>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit {
        let tpi = self.cfg.ticks_per_instruction;
        let start = self.icount;
        self.slice_time = now;
        self.slice_icount = self.icount;
        // A chunk starts a new translation block.
        self.tb_icount = self.icount;
        // Between chunks: the clock source catches up with the CPU (SysTick expiries, DWT) and
        // the board's queued interrupt line changes reach the NVIC. A stop request or an IRQ edge
        // raised while no instruction was running would only create an empty chunk in Renode.
        self.advance_clock(now);
        let n = bus.take_notifications();
        if n & BUS_IRQ_CHANGED != 0 {
            self.handle_notifications(bus, n & BUS_IRQ_CHANGED);
        }
        self.exit_pending = false;
        self.nvic.irq_rose = false;
        self.accel_prepare(&*bus); // ROUTINE-ACCEL HOOK (per chunk): find the accelerated routines of the code region
        let done = |this: &Self, reason: ExitReason| {
            let executed = this.icount - start;
            RunExit { now: now + executed * tpi, executed, reason }
        };
        if self.halted {
            return done(self, ExitReason::Halted);
        }
        // The core's own limit (the SysTick expiry) bounds the chunk like any other clock entry.
        let mut until = until;
        if let Some(d) = self.systick.deadline() {
            if d > now && d < until {
                until = d;
            }
        }
        if until <= now {
            return done(self, ExitReason::Deadline);
        }
        let budget = ((until - now) / tpi).max(1);
        self.budget_end = self.icount + budget;
        self.wfi_exit = false;
        self.force_tb_end = false;
        if let Some(reason) = self.cpu_exec_entry() {
            return done(self, reason);
        }

        let mut at_boundary = true;
        let reason = loop {
            if self.icount >= self.budget_end {
                // The end of the chunk is a translation-block end (tlib cuts the last block).
                if !at_boundary {
                    self.record_chunk_end_cut();
                }
                at_boundary = true;
            }
            if at_boundary {
                // A translation block starts here (the last block was added to the executed count).
                self.tb_icount = self.icount;
                if let Some(reason) = self.boundary(bus) {
                    break reason;
                }
            } else if self.event_pending() {
                // An event arrived in the middle of a translation block: run it to its end.
                at_boundary = self.step_insn::<B, TRACE>(bus);
                continue;
            }
            // ---- execute ----
            if self.ff.has_pending() {
                self.ff_begin(bus);
            }
            if self.ff.verifying() {
                at_boundary = self.ff_verify_run(bus, at_boundary);
                continue;
            }
            if self.itstate != 0 {
                at_boundary = self.step_insn::<B, TRACE>(bus);
                continue;
            }
            self.limit = self.budget_end;
            if self.cut_tb == self.tb_icount {
                // Re-entered in the middle of a block that a known cut ends.
                self.limit = self.limit.min(self.cut_icount);
            }
            at_boundary = self.fast_loop::<B, TRACE>(bus);
        };
        done(self, reason)
    }

    /// tlib `cpu_exec` prologue: lockup and `cpu_has_work` checks. `Some(reason)` ends the chunk
    /// before any instruction.
    fn cpu_exec_entry(&mut self) -> Option<ExitReason> {
        if self.lockup.is_some() && !(self.nvic.irq_line && self.nvic.pending_exc as usize == EXC_NMI) {
            return Some(ExitReason::Lockup);
        }
        if !self.has_work() {
            if !self.was_not_working {
                self.was_not_working = true;
                self.wfi_state_changed(true);
            }
            return Some(ExitReason::Sleeping);
        }
        if self.was_not_working {
            self.was_not_working = false;
            self.wfi_state_changed(false);
        }
        None
    }

    /// tlib `cpu_has_work`: a WFE sleeper wakes on an event, a WFI sleeper whenever the NVIC
    /// reports a pending exception - PRIMASK does not matter (Renode `MaskedInterruptPresent`).
    fn has_work(&mut self) -> bool {
        if self.wfe && (self.event_flag || (self.nvic.sevonpend && self.nvic.masked_present) || self.nvic.irq_line) {
            self.wfe = false;
        }
        if self.wfi && self.nvic.masked_present {
            self.wfi = false;
        }
        !(self.wfe || self.wfi)
    }

    /// Renode `NVIC.HandleWfiStateChange`: deep sleep halts SysTick (until an external interrupt
    /// asserts).
    fn wfi_state_changed(&mut self, entered: bool) {
        if !entered {
            self.in_sleep = false;
            self.in_deep_sleep = false;
            return;
        }
        if self.scb.scr & 4 != 0 {
            self.in_deep_sleep = true;
            self.deep_sleep_entries += 1;
            if self.halt_systick_on_deep_sleep {
                self.systick.set_enable(false);
            }
        } else {
            self.in_sleep = true;
        }
    }

    /// True when something needs the outer loop at the next translation-block boundary.
    #[inline]
    fn event_pending(&self) -> bool {
        self.nvic.irq_line || self.exit_pending || self.wfi_exit || self.reset_requested || self.lockup.is_some()
    }

    /// The body of tlib's `cpu_exec` loop head, run at every translation-block boundary:
    /// interrupt arbitration, WFI/lockup exits and the chunk limit.
    fn boundary<B: CpuBus>(&mut self, bus: &mut B) -> Option<ExitReason> {
        // process_interrupt()
        if self.nvic.irq_line {
            if self.lockup.is_some() {
                // Only an NMI preempts a locked-up core.
                if self.nvic.pending_exc as usize == EXC_NMI {
                    self.lockup = None;
                    self.nvic.set_lockup(false);
                    self.take_pending_exception(bus);
                    self.wfi_exit = false;
                }
            } else if self.r[15] < FNC_RETURN_MIN || self.pending_tailchain.is_some() {
                // (A derived exception of an invalid exception return is entered directly in
                // tlib, `exception_return_tailchain`, whatever the PC holds.)
                self.take_pending_exception(bus);
                // `exception_index` is overwritten by EXCP_IRQ; the `wfi` flag itself stays set.
                self.wfi_exit = false;
            }
        }
        if self.wfi_exit {
            self.wfi_exit = false;
            return Some(ExitReason::Sleeping);
        }
        if self.lockup.is_some() {
            return Some(ExitReason::Lockup);
        }
        if self.reset_requested {
            self.exit_pending = false;
            return Some(ExitReason::StopRequested);
        }
        if self.icount >= self.budget_end {
            self.exit_pending = false;
            return Some(ExitReason::Deadline);
        }
        if self.exit_pending {
            self.exit_pending = false;
            return Some(ExitReason::StopRequested);
        }
        None
    }

    /// The tight inner loop: runs until the chunk budget is exhausted or `kick` is called.
    /// Returns whether the last executed instruction ended its translation block.
    ///
    /// Straight-line code runs from the predecode cache with the program counter and the cache
    /// slot carried in registers: after an instruction that cannot change the PC (`!ends_tb()`)
    /// the next instruction is `slot + len` with no bounds check (the cache has two guard slots
    /// behind the last halfword that stay `Undecoded`) and no round trip through `self.r[15]`.
    /// Every handler that moves the PC out of sequence either belongs to a TB-ending kind or
    /// kicks the loop (`usage_fault`, `raise_sync`, `enter_lockup`), which the `icount >= limit`
    /// test catches before the stale `pc` is used. A TB-ending instruction re-enters through
    /// `self.r[15]` and the bounds check.
    ///
    /// Translation blocks: the instruction count of the block that starts after a TB-ending
    /// instruction is recorded in `tb_icount` (the value every bus access reports, see there). The
    /// last instruction of a 1 KiB page is cached as a `Kind::PageEnd` wrapper, so `ends_tb()` of the
    /// cached kind is the complete block-end test and the hot path has no page check.
    ///
    /// The running instruction count lives in a register; `self.icount` is written when the loop
    /// is left or hands an instruction to the slow path (nothing in the instruction handlers
    /// reads it: bus accesses use `tb_icount`).
    ///
    /// Measured on the flat-bus benchmark the loop is bound by the number of host instructions
    /// per guest instruction (a NOP costs almost as much as an ADD), so the per-instruction
    /// overhead here is kept to a minimum.
    #[inline(never)]
    fn fast_loop<B: CpuBus, const TRACE: bool>(&mut self, bus: &mut B) -> bool {
        let mut ic = self.icount;
        'lookup: loop {
            let mut pc = self.r[15];
            // ---- ROUTINE-ACCEL HOOK begin (PERF-HLE, DESIGN.md 16.2; `accel/mod.rs`) --------------------
            // At a translation-block start whose address is the entry of a known runtime-library routine,
            // a recorded call is replaced by its exact effect (or recorded). `Done`: instructions were
            // executed; continue exactly as after the last instruction of the call.
            if !TRACE && self.accel.probe[crate::accel::probe_index(pc)] == pc && ic == self.tb_icount {
                self.icount = ic;
                if let crate::accel::Enter::Done { ends } = self.accel_enter(bus, pc) {
                    ic = self.icount;
                    if ic >= self.limit {
                        return ends;
                    }
                    continue 'lookup;
                }
            }
            // ---- ROUTINE-ACCEL HOOK end ----------------------------------------------------------------
            let off = pc.wrapping_sub(self.cache_base);
            if !TRACE && off < self.cache_span {
                let base = self.cache.as_ptr();
                // SAFETY: `cache_span` is twice the number of real slots and `off < cache_span`; the
                // cache has two further guard slots, so `slot` stays inside the allocation for any
                // instruction (at most two halfwords) executed from a real slot. Nothing writes the
                // cache while `op` is alive: `fill_slot` runs before the reference is taken (it
                // does not reallocate) and handlers never touch the cache (rejection and flushes run
                // in the outer loop).
                let mut slot = unsafe { base.add((off >> 1) as usize) };
                // Two loops over the same state: the plain one (ITSTATE == 0) and the IT block one. The plain
                // loop pays nothing for IT blocks: the `IT` instruction sits in front of the block-ending kinds
                // (`FIRST_SPECIAL`), so the test that already follows every instruction also catches it.
                'run: loop {
                    if self.itstate == 0 {
                        loop {
                            let mut kind = unsafe { (*slot).kind };
                            if (kind as u8) <= Kind::CutHead as u8 {
                                // `Undecoded` or `CutHead`: decode, or take the wrapped instruction's kind
                                // (and apply the cut of its block). `Undecoded` back: not cacheable, slow path.
                                let idx = unsafe { slot.offset_from(base) } as usize;
                                kind = self.cold_slot(bus, idx, pc, ic);
                                if kind == Kind::Undecoded {
                                    break 'run;
                                }
                            }
                            // SAFETY: the slot is not modified while the instruction executes.
                            let op: &Op = unsafe { &*slot };
                            let len = op.len as u32;
                            let next = pc.wrapping_add(len);
                            self.r[15] = next;
                            self.exec(bus, kind, op, pc);
                            ic += 1;
                            if ic >= self.limit {
                                self.icount = ic;
                                let forced = core::mem::take(&mut self.force_tb_end);
                                return kind.ends_tb() | forced | self.cut_block_ends(ic);
                            }
                            if kind as u8 >= FIRST_SPECIAL {
                                if kind == Kind::It {
                                    // An IT block starts; it continues behind this instruction.
                                    pc = next;
                                    slot = unsafe { (slot as *const u8).add((len as usize) << 3) as *const Op };
                                    continue 'run;
                                }
                                self.tb_icount = ic;
                                continue 'lookup;
                            }
                            debug_assert_eq!(self.r[15], next, "non-TB-ending instruction {:?} moved the PC without kicking", kind);
                            pc = next;
                            // 16 bytes per halfword slot: `len` (2 or 4 bytes) * 8. Computed in bytes so that the
                            // loop-carried dependency (load of `len`, one add) stays as short as possible.
                            slot = unsafe { (slot as *const u8).add((len as usize) << 3) as *const Op };
                        }
                    } else {
                        // IT block body (ITSTATE != 0): `step_insn`'s rules, executed here. A skipped
                        // instruction dispatches as a `Nop` (it still counts, and still ends its
                        // translation block when its kind does); flag-setting 16-bit instructions that only
                        // set flags outside an IT block run from a copy without `FL_S`.
                        loop {
                            let mut kind = unsafe { (*slot).kind };
                            if (kind as u8) <= Kind::CutHead as u8 {
                                let idx = unsafe { slot.offset_from(base) } as usize;
                                kind = self.cold_slot(bus, idx, pc, ic);
                                if kind == Kind::Undecoded {
                                    break 'run;
                                }
                            }
                            // SAFETY: the slot is not modified while the instruction executes.
                            let op: &Op = unsafe { &*slot };
                            let len = op.len as u32;
                            let next = pc.wrapping_add(len);
                            self.r[15] = next;
                            self.insn_faulted = false;
                            self.it_changed = false;
                            let mut run_kind = kind;
                            let mut run_op = op;
                            let patched;
                            if !cond_holds((self.itstate >> 4) as u32, self.apsr) {
                                run_kind = Kind::Nop;
                            } else if op.flags & FL_IT != 0 {
                                patched = Op { flags: op.flags & !FL_S, ..*op };
                                run_op = &patched;
                            }
                            self.exec(bus, run_kind, run_op, pc);
                            if !self.insn_faulted && !self.it_changed && self.itstate != 0 {
                                self.it_advance();
                            }
                            ic += 1;
                            let ends = kind.ends_tb();
                            if ic >= self.limit {
                                self.icount = ic;
                                let forced = core::mem::take(&mut self.force_tb_end);
                                return ends | forced | self.cut_block_ends(ic);
                            }
                            if ends {
                                self.tb_icount = ic;
                                continue 'lookup;
                            }
                            debug_assert_eq!(self.r[15], next, "non-TB-ending instruction {:?} moved the PC without kicking", kind);
                            pc = next;
                            slot = unsafe { (slot as *const u8).add((len as usize) << 3) as *const Op };
                            if self.itstate == 0 {
                                continue 'run;
                            }
                        }
                    }
                }
            }
            // Uncached code (SRAM, peripherals), a region switch, the guard slot, or tracing: one
            // instruction through `step_insn`, which also knows the IT block rules.
            self.icount = ic;
            let ends = self.step_insn::<B, TRACE>(bus);
            ic = self.icount;
            if ends {
                self.tb_icount = ic;
            }
            if ic >= self.limit {
                return ends;
            }
        }
    }

    /// Cold part of the hot loop's slot handling for an `Undecoded` or `CutHead` slot at `pc`:
    /// returns the kind of the instruction the slot holds (decoding it first when needed; a
    /// `CutHead` wrapper keeps all fields of its instruction), or `Undecoded` when the
    /// instruction has to go through the slow path.
    #[cold]
    #[inline(never)]
    fn cold_slot<B: CpuBus>(&mut self, bus: &mut B, idx: usize, pc: u32, ic: u64) -> Kind {
        if idx >= (self.cache_span >> 1) as usize {
            return Kind::Undecoded;
        }
        self.visit_tb_start(bus, pc, ic);
        if self.cache[idx].kind == Kind::Undecoded {
            self.fill_slot(bus, pc, idx);
            return self.cache[idx].kind;
        }
        let entry = self.cache[idx].raw as usize;
        self.cut_entries[entry].inner.kind
    }

    /// Called before the instruction at `pc` (retire count `ic`) executes. When it starts a
    /// translation block and the predecode slot is `Undecoded` (the first time anything is
    /// translated there) or a `CutHead`, remembers it in `cut_ctx`, and for a `CutHead` ends the
    /// block at the largest known cut that fits the remaining chunk budget (tlib's lookup).
    fn visit_tb_start<B: CpuBus>(&mut self, bus: &mut B, pc: u32, ic: u64) {
        if ic != self.tb_icount {
            return;
        }
        let mut off = pc.wrapping_sub(self.cache_base);
        if off >= self.cache_span {
            // (The very first instruction runs before any predecode region exists.)
            self.ensure_cache_region(bus, pc);
            off = pc.wrapping_sub(self.cache_base);
            if off >= self.cache_span {
                return;
            }
        }
        let idx = (off >> 1) as usize;
        match self.cache[idx].kind {
            Kind::Undecoded => self.cut_ctx = CutCtx { tb_icount: ic, pc, entry: NO_CUT_ENTRY },
            Kind::CutHead => {
                let entry = self.cache[idx].raw;
                self.cut_ctx = CutCtx { tb_icount: ic, pc, entry };
                let budget = self.budget_end.saturating_sub(ic);
                if let Some(l) = self.cut_entries[entry as usize].largest_cut_le(budget) {
                    self.limit = self.limit.min(ic + l);
                    self.cut_icount = ic + l;
                    self.cut_tb = ic;
                }
            }
            _ => {}
        }
    }

    /// True when the instruction that just retired (`ic` instructions executed) ends its block
    /// because of a known cut.
    #[inline(always)]
    fn cut_block_ends(&self, ic: u64) -> bool {
        self.cut_tb == self.tb_icount && self.cut_icount == ic
    }

    /// The chunk budget ended in the middle of a translation block: tlib cut the block there. If
    /// this block's first instruction was translated for the first time (or already has cuts), the
    /// cut block stays in the cache - see [`CutEntry`].
    fn record_chunk_end_cut(&mut self) {
        let ctx = self.cut_ctx;
        if ctx.tb_icount != self.tb_icount {
            return;
        }
        let len = (self.icount - self.tb_icount) as u32;
        if len == 0 {
            return;
        }
        if ctx.entry != NO_CUT_ENTRY {
            self.cut_entries[ctx.entry as usize].add_cut(len);
            return;
        }
        let idx = (ctx.pc.wrapping_sub(self.cache_base) >> 1) as usize;
        let inner = match self.cache.get(idx) {
            Some(op) if op.kind != Kind::Undecoded && op.kind != Kind::CutHead && !op.kind.ends_tb() => *op,
            _ => return,
        };
        let entry = self.cut_entries.len() as u32;
        self.cut_entries.push(CutEntry { inner, idx, cuts: vec![len] });
        self.cache[idx] = Op { kind: Kind::CutHead, raw: entry, ..inner };
        self.cut_ctx.entry = entry;
    }

    /// Executes one instruction with full IT-block handling. Returns whether it ended its
    /// translation block.
    pub(crate) fn step_insn<B: CpuBus, const TRACE: bool>(&mut self, bus: &mut B) -> bool {
        let pc = self.r[15];
        self.visit_tb_start(bus, pc, self.icount);
        let op = self.fetch_op(bus, pc);
        self.step_op::<B, TRACE>(bus, pc, op)
    }

    /// [`Cpu::step_insn`] for an instruction the caller fetched already (the idle-loop verification looks at the
    /// decoded instruction before it runs): the same sequence, without fetching it a second time.
    pub(crate) fn step_fetched<B: CpuBus>(&mut self, bus: &mut B, pc: u32, op: Op) -> bool {
        self.visit_tb_start(bus, pc, self.icount);
        self.step_op::<B, false>(bus, pc, op)
    }

    #[inline(always)]
    fn step_op<B: CpuBus, const TRACE: bool>(&mut self, bus: &mut B, pc: u32, mut op: Op) -> bool {
        if TRACE {
            self.trace_record(pc, &op);
        }
        self.r[15] = pc.wrapping_add(op.len as u32);
        self.insn_faulted = false;
        self.it_changed = false;
        if self.itstate != 0 {
            let cond = (self.itstate >> 4) as u32;
            if cond_holds(cond, self.apsr) {
                if op.flags & FL_IT != 0 {
                    op.flags &= !FL_S;
                }
                self.exec_slow(bus, &op, pc);
            }
            if !self.insn_faulted && !self.it_changed && self.itstate != 0 {
                self.it_advance();
            }
        } else {
            self.exec_slow(bus, &op, pc);
        }
        self.icount += 1;
        let forced = core::mem::take(&mut self.force_tb_end);
        op.kind.ends_tb() | tb_page_end(pc, op.len) | forced | self.cut_block_ends(self.icount)
    }

    /// `ITAdvance()`.
    #[inline]
    pub(crate) fn it_advance(&mut self) {
        if self.itstate & 7 == 0 {
            self.itstate = 0;
        } else {
            self.itstate = (self.itstate & 0xE0) | ((self.itstate << 1) & 0x1F);
        }
    }

    // ---- notifications ------------------------------------------------------------------------------

    #[cold]
    #[inline(never)]
    pub(crate) fn handle_notifications<B: CpuBus>(&mut self, bus: &mut B, n: u32) {
        if n & BUS_IRQ_CHANGED != 0 {
            let mut woke = false;
            {
                let nvic = &mut self.nvic;
                bus.drain_irq_changes(&mut |irq, level| {
                    woke |= nvic.set_irq_line(irq, level);
                });
            }
            if woke {
                self.systick_wake();
            }
            self.nvic_changed();
        }
        if n & BUS_STOP_REQUESTED != 0 {
            // `RequestReturn`: the chunk ends at the end of the current translation block.
            self.exit_pending = true;
            self.kick();
        }
    }

    // ---- fault helpers ----------------------------------------------------------------------------------

    /// Raises a synchronous exception `exc` (configurable fault or SVCall). The faulting
    /// instruction ends its translation block.
    pub(crate) fn raise_sync(&mut self, exc: usize) {
        match self.nvic.set_pending_synchronous_fault(exc) {
            SyncFault::Pending => self.nvic_changed(),
            SyncFault::Lockup => self.enter_lockup(format!("exception {} cannot be taken at the current execution priority", exc)),
        }
        self.force_tb_end = true;
        self.kick();
    }

    /// UsageFault with the given CFSR bit; the stacked return address is `fault_pc`.
    pub(crate) fn usage_fault(&mut self, cfsr_bit: u32, fault_pc: u32) {
        self.scb.cfsr |= cfsr_bit;
        self.r[15] = fault_pc;
        self.insn_faulted = true;
        self.raise_sync(EXC_USAGEFAULT);
    }

    /// Lockup: the PC takes tlib's sentinel value, only an NMI can be taken until the state clears.
    pub(crate) fn enter_lockup(&mut self, reason: String) {
        if self.lockup.is_none() {
            self.lockup = Some(reason);
        }
        self.nvic.set_lockup(true);
        self.r[15] = LOCKUP_PC;
        self.force_tb_end = true;
        self.kick();
    }
}
