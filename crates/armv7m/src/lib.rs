//! Cortex-M4F core: ARMv7E-M Thumb/Thumb-2 + DSP, FPv4-SP, exception model,
//! system control space (NVIC, SCB, SysTick, MPU registers, FPU control) and DWT.
//!
//! Contract owner: planning (`DESIGN.md`, section "CPU core").
//! Implementation owner: work package CPU. Floating point lives in the
//! `armv7m-vfp` crate (work package FPU), re-exported here as `vfp`.
//! Keep the public items below source-compatible; extend rather than rename.
//!
//! Timing follows Renode 1.17.0 single-machine behaviour (see `docs/renode-semantics.md` and
//! the `cpu` module documentation): 1 ns time base, chunked execution, interrupt arbitration at
//! tlib translation-block boundaries, SysTick / DWT as Renode clock entries.
//!
//! Overview of the implementation:
//!  * [`decode`] turns Thumb / Thumb-2 encodings into compact predecoded [`op::Op`]s
//!    (cached per halfword address over the bus' `code_region`); [`op::Kind::ends_tb`]
//!    classifies the instructions after which tlib ends a translation block;
//!  * `exec` is the dense `match` that executes them; `cpu` holds the state, the
//!    public API and the run loop; `exception` implements exception entry/return;
//!    `nvic`, `scs` and `timers` model the system control space (Renode NVIC.cs
//!    semantics) and the DWT cycle counter; `fastfwd` is the exact idle-loop
//!    fast-forward; `trace` the opt-in instruction trace.

pub use armv7m_vfp as vfp;

use emu_core::Time;

pub mod accel;
pub mod alu;
pub mod decode;
pub mod disasm;
pub mod op;

mod cpu;
mod exception;
mod exec;
mod fastfwd;
mod mem;
mod nvic;
mod scs;
mod timers;
mod trace;

pub use accel::{RoutineAccelMode, RoutineAccelStats, RoutineStats};
pub use cpu::{Cpu, FastForwardStats, RegSnapshot};
pub use trace::TraceEntry;

/// `CpuBus::take_notifications` bit: external IRQ levels changed; call `drain_irq_changes`.
pub const BUS_IRQ_CHANGED: u32 = 1 << 0;
/// `CpuBus::take_notifications` bit: the board needs `Cpu::run` to return at the end of the
/// current translation block (Renode `RequestReturn`: every `LimitTimer` setter, `ScheduleAction`).
/// The instruction that raised it and the rest of its translation block (up to the next branch,
/// `wfi`, `svc`, `msr`, `cps`, barrier, page boundary or the chunk end) still execute.
pub const BUS_STOP_REQUESTED: u32 = 1 << 1;

/// Memory interface used for every access outside the private peripheral bus
/// (0xE000_0000..=0xE00F_FFFF is handled inside the core). Implemented by the
/// board and monomorphized into `Cpu::run`, so keep implementations `#[inline]`.
///
/// `icount` is the core's monotonic executed-instruction count the board turns into the time of a
/// synchronised MMIO access. Precisely: it is the number of instructions retired before the
/// *translation block* of the accessing instruction started - what tlib's
/// `tlib_get_executed_instructions` reports to Renode's `SyncTime()`, because tlib adds a block to
/// the executed count only when the next block starts. The access therefore happens at
/// `slice_start_time + (icount - slice_start_icount) * ticks_per_instruction`, which is up to the
/// block length earlier than the accessing instruction itself, and every access of one block sees
/// the same time. (Renode peripherals do not see this time but the machine clock time, i.e. the
/// slice start, unless they call `SyncTime()`.) The blocks are the ones of Renode's translator:
/// they end after every branch / PC write, `wfi`, `svc`, `msr`, `cps`, barrier and at 1 KiB page
/// ends, a chunk starts a new block, and the block in which a chunk's budget ran out when its start
/// address was translated for the first time stays cut at that length (see `Cpu::run`).
pub trait CpuBus {
    fn read8(&mut self, addr: u32, icount: u64) -> u8;
    fn read16(&mut self, addr: u32, icount: u64) -> u16;
    fn read32(&mut self, addr: u32, icount: u64) -> u32;
    fn write8(&mut self, addr: u32, value: u8, icount: u64);
    fn write16(&mut self, addr: u32, value: u16, icount: u64);
    fn write32(&mut self, addr: u32, value: u32, icount: u64);

    /// Read-only code memory (flash) containing `addr`, as `(base, bytes)`.
    /// The predecode cache is built over these bytes. `None` means the core
    /// must fetch with `fetch16` and must not cache (e.g. code in SRAM).
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])>;

    /// Instruction fetch for code outside `code_region` (no side effects).
    fn fetch16(&mut self, addr: u32) -> u16;

    /// True for ordinary RAM/flash whose contents change only through stores
    /// by this core or through board events (DMA), never as a side effect of
    /// reads or of time passing. Used by exact idle-loop fast-forward.
    fn is_plain_memory(&self, addr: u32) -> bool;

    /// Returns and clears `BUS_*` notification bits raised by MMIO side effects
    /// since the previous call. Must be cheap (plain field swap).
    fn take_notifications(&mut self) -> u32;

    /// Delivers accumulated external interrupt line changes as `(irq, level)`
    /// in the order they happened.
    fn drain_irq_changes(&mut self, sink: &mut dyn FnMut(u32, bool));

    /// Renode `cpu.SyncTime()`: called before the core reads SysTick `CVR` or DWT `CYCCNT`.
    /// The board should advance its clock time (firing every event due up to then) to
    /// `slice_start + (icount - slice_start_icount) * ticks_per_instruction` with the same
    /// `icount` convention as the data accesses (the start of the translation block of the
    /// reading instruction). The core advances its own timers to the same instant and polls
    /// `take_notifications` afterwards. The default does nothing (the core's own timers are
    /// still exact).
    #[inline]
    fn sync_time(&mut self, _icount: u64) {}
}

/// Static configuration of one core instance.
#[derive(Clone, Copy, Debug)]
pub struct CpuConfig {
    /// Virtual ticks per executed instruction (`emu_core::TICKS_PER_INSTRUCTION`; 10 = 100 MIPS
    /// with the 1 ns time base).
    pub ticks_per_instruction: Time,
    /// SysTick clock (Renode `systickFrequency`, 80 MHz on both boards).
    pub systick_hz: u64,
    /// DWT CYCCNT clock (Renode DWT `frequency`, 80 MHz).
    pub dwt_hz: u64,
    /// Number of external interrupt lines wired to the NVIC.
    pub num_irqs: u32,
    /// Implemented priority bits (Renode NVIC `priorityMask`, 0xF0).
    pub priority_mask: u8,
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            ticks_per_instruction: emu_core::TICKS_PER_INSTRUCTION,
            systick_hz: 80_000_000,
            dwt_hz: 80_000_000,
            num_irqs: 96,
            priority_mask: 0xF0,
        }
    }
}

/// Why `Cpu::run` returned. Every reason except the last two ends the chunk at a
/// translation-block boundary after the pending exception (if any) has been taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The chunk's instruction budget was executed (`max(1, floor((until - now) / tpi))`).
    /// `RunExit::now` can be earlier than `until` (a remainder below one instruction is run as a
    /// separate one-instruction chunk, like Renode) or later (the last instruction crossed it).
    Deadline,
    /// `WFI` / `WFE` / `B .` executed, or the core still sleeps at chunk start. The board skips
    /// time to the nearest clock limit or the end of the 100 us round (Renode WFI handling) and
    /// calls `run` again: the wake-up check happens at its start (a pending exception wakes the
    /// core even when PRIMASK masks it).
    Sleeping,
    /// Held by `set_halted(true)` (Renode `cpu IsHalted true` fixture).
    Halted,
    /// The chunk ended early at the end of a translation block: a board `BUS_STOP_REQUESTED`, an
    /// asserted NVIC output (IRQ line rising edge), a SysTick / DWT register write, SLEEPONEXIT,
    /// or a firmware system reset request (then `Cpu::take_reset_request` is true).
    StopRequested,
    /// Unrecoverable core state (lockup, PC = 0xEFFF_FFFE); see `Cpu::lockup_reason`. Only an
    /// NMI leaves it. The board treats it like `Sleeping`.
    Lockup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunExit {
    /// Core virtual time after the last executed instruction.
    pub now: Time,
    /// Instructions executed during this call.
    pub executed: u64,
    pub reason: ExitReason,
}
