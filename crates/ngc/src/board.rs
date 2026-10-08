//! One board: a Cortex-M4F core plus a `MachineCore`, and the run loop that interleaves them.
//!
//! # Run loop (DESIGN.md section 5, `docs/framework.md` section 13)
//!
//! Renode's `CpuThreadBodyInner` for one machine. Rounds follow the global quantum grid
//! (`QUANTUM`, aligned to time 0); inside a round `Board::run_until(target)` repeats:
//!
//! 1. apply queued IRQ changes to the CPU, fire the events that are already due at the clock time;
//! 2. `limit = min(round end, target)`; a halted or locked-up core only lets time pass
//!    (`cpu.advance_idle`, then the machine clock advances and fires its events);
//! 3. otherwise plan a chunk of `max(1, floor((nearest limit - clock time) / ticks_per_instruction))`
//!    instructions, at most the instructions left in the round (Renode `InstructionsToNearestLimit`;
//!    the nearest limit is the earlier of the next queued event and the CPU's internal deadline);
//! 4. `cpu.run(bus, now, now + chunk * tpi)`: whole instructions, returning early at the end of the
//!    current translation block when a peripheral asked for it (`ctx.request_return()`), or on
//!    sleep, halt, lockup, reset request;
//! 5. the machine clock catches up with the CPU (`MachineCore::advance_clock`): events fire at
//!    their **own** times and `Ctx::now()` is that time inside their handlers; a core that went to
//!    sleep (WFI, counted as an executed instruction) then skips to the nearest limit or the end of
//!    the round, whichever is first.
//!
//! Inside a chunk, MMIO side effects see the lagging clock time (chunk start or last sync), except
//! for registers that declare `Peripheral::sync_registers` and models that call `ctx.sync_time()`.
//!
//! The CPU is reached through the [`CpuCore`] trait so the loop can be unit-tested with a scripted
//! core; `Board` defaults to `armv7m::Cpu`.

use crate::bus::BusView;
use crate::memory::{self, LAYOUT};
use armv7m::{Cpu, CpuBus, CpuConfig, ExitReason, RunExit};
use emu_core::{
    Ctx, EventId, MachineCore, MapError, MemoryLayout, PeriphId, Peripheral, Target, Time, Width, NOTIFY_IRQ_CHANGED,
    NOTIFY_STOP_REQUESTED, QUANTUM,
};

/// What the board needs from a CPU core. Implemented for [`armv7m::Cpu`].
pub trait CpuCore {
    /// See `armv7m::Cpu::run`. The board passes `until = now + n * ticks_per_instruction` with `n >= 1`
    /// and expects `RunExit::now = now + executed * ticks_per_instruction`; the chunk may end earlier at
    /// the end of the current translation block when the bus raised `BUS_STOP_REQUESTED`.
    fn run<B: CpuBus>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit;
    /// Time passes without instructions (halted, locked up, sleeping): processes the core's own timers
    /// (SysTick) in `(now, until]`.
    fn advance_idle(&mut self, now: Time, until: Time);
    /// Absolute time of the core's next internal limit (SysTick), if any.
    fn next_internal_deadline(&self) -> Option<Time>;
    fn set_irq_line(&mut self, irq: u32, level: bool);
    /// Monotonic executed-instruction count.
    fn instructions(&self) -> u64;
    fn set_halted(&mut self, halted: bool);
    fn is_halted(&self) -> bool;
    fn is_sleeping(&self) -> bool;
    /// Side-effect-free read of a private-peripheral-bus register.
    fn ppb_peek32(&self, addr: u32, now: Time) -> Option<u32>;
    /// Drops decoded instructions after flash contents changed. Flash is writable memory in the
    /// stock platform (stores, DMA and `load` all take effect), so the board calls this before
    /// the next slice whenever flash was written; the machine also ends the running slice right
    /// after a flash store. Cores without a predecode cache can keep the default.
    fn invalidate_code_cache(&mut self) {}
    /// True once after the firmware requested a system reset (AIRCR.SYSRESETREQ); that slice
    /// ended with `ExitReason::StopRequested`. The board reports it and stops `run_until`.
    fn take_reset_request(&mut self) -> bool {
        false
    }
}

impl CpuCore for Cpu {
    #[inline]
    fn run<B: CpuBus>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit {
        Cpu::run(self, bus, now, until)
    }

    fn advance_idle(&mut self, now: Time, until: Time) {
        Cpu::advance_idle(self, now, until)
    }

    fn next_internal_deadline(&self) -> Option<Time> {
        Cpu::next_internal_deadline(self)
    }

    fn set_irq_line(&mut self, irq: u32, level: bool) {
        Cpu::set_irq_line(self, irq, level)
    }

    fn instructions(&self) -> u64 {
        Cpu::instructions(self)
    }

    fn set_halted(&mut self, halted: bool) {
        Cpu::set_halted(self, halted)
    }

    fn is_halted(&self) -> bool {
        Cpu::is_halted(self)
    }

    fn is_sleeping(&self) -> bool {
        Cpu::is_sleeping(self)
    }

    fn ppb_peek32(&self, addr: u32, now: Time) -> Option<u32> {
        Cpu::ppb_peek32(self, addr, now)
    }

    fn invalidate_code_cache(&mut self) {
        Cpu::invalidate_code_cache(self)
    }

    fn take_reset_request(&mut self) -> bool {
        Cpu::take_reset_request(self)
    }
}

/// A core that never executes anything: always halted, interrupt lines are only recorded.
/// `Board<NullCpu>` (see [`Board::headless`]) lets tests assemble a board, run its peripherals'
/// events, exercise register access through `bus_read/bus_write` and check interrupt wiring
/// without a real Cortex-M core.
#[derive(Debug, Default)]
pub struct NullCpu {
    levels: Vec<bool>,
    changes: Vec<(u32, bool)>,
}

impl NullCpu {
    /// Level of external interrupt line `irq` as last reported to the core.
    pub fn irq_level(&self, irq: u32) -> bool {
        self.levels.get(irq as usize).copied().unwrap_or(false)
    }

    /// Every `set_irq_line` call since the last `clear_irq_changes`, in order.
    pub fn irq_changes(&self) -> &[(u32, bool)] {
        &self.changes
    }

    pub fn clear_irq_changes(&mut self) {
        self.changes.clear();
    }
}

impl CpuCore for NullCpu {
    fn run<B: CpuBus>(&mut self, _bus: &mut B, now: Time, _until: Time) -> RunExit {
        RunExit { now, executed: 0, reason: ExitReason::Halted }
    }

    fn advance_idle(&mut self, _now: Time, _until: Time) {}

    fn next_internal_deadline(&self) -> Option<Time> {
        None
    }

    fn set_irq_line(&mut self, irq: u32, level: bool) {
        if self.levels.len() <= irq as usize {
            self.levels.resize(irq as usize + 1, false);
        }
        self.levels[irq as usize] = level;
        self.changes.push((irq, level));
    }

    fn instructions(&self) -> u64 {
        0
    }

    fn set_halted(&mut self, _halted: bool) {}

    fn is_halted(&self) -> bool {
        true
    }

    fn is_sleeping(&self) -> bool {
        false
    }

    fn ppb_peek32(&self, _addr: u32, _now: Time) -> Option<u32> {
        None
    }
}

/// Static configuration of a board.
#[derive(Clone, Debug)]
pub struct BoardConfig {
    /// Board name for logs (`"ngc-main"`, `"ngc-handset"`).
    pub name: String,
    pub cpu: CpuConfig,
}

impl BoardConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), cpu: CpuConfig::default() }
    }
}

/// Counters accumulated over the board's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoardStats {
    /// `cpu.run` calls.
    pub slices: u64,
    /// Instructions executed through `run_until`.
    pub instructions: u64,
    pub events_fired: u64,
    /// Time jumps over sleeping, halted or locked-up periods.
    pub idle_jumps: u64,
    /// Chunks that ended because of a stop request (`ctx.request_return()`, a flash write).
    pub stop_requests: u64,
    /// Slices where the CPU returned without advancing time (forced progress by the board).
    pub stalls: u64,
    /// `invalidate_code_cache` calls (flash was written since the previous slice).
    pub code_invalidations: u64,
}

/// Result of one `run_until` call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub instructions: u64,
    pub events: u32,
    pub slices: u32,
    /// True if the CPU reported a lockup during this call (it stays locked up until `clear_lockup`).
    pub lockup: bool,
    /// True if the firmware requested a system reset (AIRCR.SYSRESETREQ). `run_until` returned
    /// early, possibly before the target time; the host decides what a reset means (the runner
    /// fixtures treat it like a Restart).
    pub reset_requested: bool,
}

pub struct Board<C: CpuCore = Cpu> {
    /// The core. Public so fixtures can read registers and set the boot state.
    pub cpu: C,
    /// Memories, peripherals, events and signals.
    pub core: MachineCore,
    name: String,
    now: Time,
    ticks_per_instruction: Time,
    locked_up: bool,
    reset_requested: bool,
    stats: BoardStats,
}

impl Board<Cpu> {
    /// A board with a freshly constructed Cortex-M4F core and the NGC memory layout.
    pub fn new(config: BoardConfig) -> Self {
        let tpi = config.cpu.ticks_per_instruction;
        Board::with_cpu(Cpu::new(config.cpu), config.name, tpi)
    }
}

impl Board<NullCpu> {
    /// A board without a CPU (see [`NullCpu`]) at the default 100 MIPS timing.
    pub fn headless(name: impl Into<String>) -> Self {
        Board::with_cpu(NullCpu::default(), name, emu_core::TICKS_PER_INSTRUCTION)
    }
}

impl<C: CpuCore> Board<C> {
    /// A board around an existing core (used by tests with scripted cores).
    pub fn with_cpu(cpu: C, name: impl Into<String>, ticks_per_instruction: Time) -> Self {
        assert!(ticks_per_instruction > 0, "ticks per instruction must be positive");
        Self {
            cpu,
            core: MachineCore::new(LAYOUT),
            name: name.into(),
            now: 0,
            ticks_per_instruction,
            locked_up: false,
            reset_requested: false,
            stats: BoardStats::default(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Board virtual time: the CPU time at the last chunk end or idle jump. Between chunks it equals the
    /// machine's clock time (`clock_time()`).
    #[inline]
    pub fn now(&self) -> Time {
        self.now
    }

    /// The machine's clock-source time (what `ctx.now()` reports to peripherals between chunks).
    #[inline]
    pub fn clock_time(&self) -> Time {
        self.core.clock_time()
    }

    pub fn ticks_per_instruction(&self) -> Time {
        self.ticks_per_instruction
    }

    pub fn stats(&self) -> &BoardStats {
        &self.stats
    }

    /// True once the core reported `ExitReason::Lockup`; time keeps advancing, the CPU does not run.
    pub fn locked_up(&self) -> bool {
        self.locked_up
    }

    pub fn clear_lockup(&mut self) {
        self.locked_up = false;
    }

    /// True once the firmware requested a system reset (AIRCR.SYSRESETREQ) and until
    /// `clear_reset_request`. `run_until` stops as soon as it sees the request; the board itself
    /// does nothing else (the runner fixtures decide what a reset means).
    pub fn reset_requested(&self) -> bool {
        self.reset_requested
    }

    pub fn clear_reset_request(&mut self) {
        self.reset_requested = false;
    }

    pub fn layout(&self) -> MemoryLayout {
        LAYOUT
    }

    // ---- assembly --------------------------------------------------------------

    pub fn add_peripheral(&mut self, peripheral: Box<dyn Peripheral>) -> PeriphId {
        self.core.add_peripheral(peripheral)
    }

    /// Adds a peripheral and maps it at `base` (offsets passed to the model are relative to `base`).
    pub fn add_mapped(&mut self, base: u32, size: u32, peripheral: Box<dyn Peripheral>) -> Result<PeriphId, MapError> {
        self.core.add_mapped(base, size, peripheral)
    }

    pub fn map(&mut self, id: PeriphId, base: u32, size: u32) -> Result<(), MapError> {
        self.core.map(id, base, size)
    }

    /// Wires an output line. The connect-time level push to an `Irq` target is queued and reaches
    /// the CPU at the start of the next `run_until` (assembly never calls into the core).
    pub fn connect(&mut self, src: PeriphId, line: u32, target: Target) -> Result<(), MapError> {
        self.core.connect(src, line, target)
    }

    pub fn connect_irq(&mut self, src: PeriphId, line: u32, irq: u32) -> Result<(), MapError> {
        self.connect(src, line, Target::Irq(irq))
    }

    pub fn connect_input(&mut self, src: PeriphId, line: u32, dst: PeriphId, dst_line: u32) -> Result<(), MapError> {
        self.connect(src, line, Target::Input(dst, dst_line))
    }

    pub fn find(&self, name: &str) -> Option<PeriphId> {
        self.core.find(name)
    }

    pub fn get<T: Peripheral>(&self, id: PeriphId) -> Option<&T> {
        self.core.get::<T>(id)
    }

    pub fn get_mut<T: Peripheral>(&mut self, id: PeriphId) -> Option<&mut T> {
        self.core.get_mut::<T>(id)
    }

    /// Copies `bytes` into plain memory (Renode `sysbus LoadBinary`).
    pub fn load(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        self.core.load_memory(addr, bytes)
    }

    // ---- external stimuli (UI, fixtures, the other board) --------------------------------

    /// Runs a model method with a `Ctx` at the board's current clock time, then delivers the signal and
    /// interrupt changes it caused (button presses, ADC input changes, CAN frame delivery...).
    pub fn with_peripheral<T: Peripheral, R>(&mut self, id: PeriphId, f: impl FnOnce(&mut T, &mut Ctx<'_>) -> R) -> Option<R> {
        let result = self.core.with_peripheral::<T, R>(id, f);
        self.finish_external();
        result
    }

    /// Drives a numbered input of a peripheral (Renode `peripheral OnGPIO n level`).
    pub fn set_input(&mut self, dst: PeriphId, line: u32, level: bool) {
        self.core.set_input(dst, line, level);
        self.finish_external();
    }

    /// Drives an output line as if its peripheral had (environment-held pins).
    pub fn drive_output(&mut self, src: PeriphId, line: u32, level: bool) {
        self.core.drive_output(src, line, level);
        self.finish_external();
    }

    /// System-bus read with MMIO side effects at the current clock time (Renode `sysbus ReadDoubleWord`).
    pub fn bus_read(&mut self, addr: u32, width: Width) -> u32 {
        let value = self.core.bus_read(addr, width);
        self.finish_external();
        value
    }

    /// System-bus write at the current clock time (Renode `sysbus WriteDoubleWord`).
    pub fn bus_write(&mut self, addr: u32, width: Width, value: u32) {
        self.core.bus_write(addr, width, value);
        self.finish_external();
    }

    /// Side-effect-free read: plain memory, peripheral `peek`, or the core's PPB registers.
    pub fn peek(&self, addr: u32, width: Width) -> Option<u32> {
        if memory::is_ppb(addr) {
            let aligned = self.cpu.ppb_peek32(addr & !3, self.now)?;
            let shift = (addr & 3) * 8;
            if shift + width.bits() > 32 {
                return None;
            }
            return Some((aligned >> shift) & width.mask());
        }
        self.core.peek(addr, width)
    }

    pub fn peek32(&self, addr: u32) -> Option<u32> {
        self.peek(addr, Width::Word)
    }

    /// Side-effect-free write to plain memory or a native-width `Peripheral::poke`.
    pub fn poke(&mut self, addr: u32, width: Width, value: u32) -> bool {
        let done = self.core.poke(addr, width, value);
        self.finish_external();
        done
    }

    /// Bytes of plain memory (flash or SRAM), for dumps.
    pub fn memory_slice(&self, addr: u32, len: usize) -> Option<&[u8]> {
        self.core.mem.slice(addr, len)
    }

    pub fn set_halted(&mut self, halted: bool) {
        self.cpu.set_halted(halted);
    }

    /// Applies queued interrupt line changes to the CPU.
    pub fn sync_irqs(&mut self) {
        let cpu = &mut self.cpu;
        self.core.drain_irq_changes(&mut |irq, level| cpu.set_irq_line(irq, level));
        self.core.clear_notifications(NOTIFY_IRQ_CHANGED);
    }

    fn finish_external(&mut self) {
        self.sync_irqs();
        // Nothing is running, so a stop request raised outside a slice has no one to stop.
        self.core.clear_notifications(NOTIFY_STOP_REQUESTED);
    }

    // ---- run loop ------------------------------------------------------------------

    /// Advances the board to virtual time `target` (or slightly past it, to the next instruction
    /// boundary), following Renode's round/chunk loop (see the module documentation). Returns immediately
    /// if `now() >= target` after firing the events that are already due.
    pub fn run_until(&mut self, target: Time) -> RunReport {
        let mut report = RunReport::default();
        let tpi = self.ticks_per_instruction;
        loop {
            self.sync_irqs();
            self.fire_due(&mut report);
            if self.now >= target {
                break;
            }
            // The quantum grid is aligned to time 0; a round ends at the next multiple of the quantum.
            let limit = ((self.now / QUANTUM + 1) * QUANTUM).min(target);
            if self.locked_up || self.cpu.is_halted() {
                // A halted core does not run, but the machine's clock and peripherals keep going.
                self.idle_to(limit, &mut report);
                continue;
            }
            // Renode CpuThreadBodyInner: instructions left in the round, bounded by InstructionsToNearestLimit.
            let left = (limit - self.now).div_ceil(tpi);
            let chunk = self.instructions_to_nearest_limit().min(left);
            let until = self.now + chunk * tpi;

            if self.core.take_flash_dirty() {
                self.cpu.invalidate_code_cache();
                self.stats.code_invalidations += 1;
            }
            self.core.clear_notifications(NOTIFY_STOP_REQUESTED);
            let start_icount = self.cpu.instructions();
            let previous = self.now;
            let exit = {
                let mut bus = BusView::new(&mut self.core, self.now, start_icount, tpi);
                self.cpu.run(&mut bus, self.now, until)
            };
            self.stats.slices += 1;
            self.stats.instructions += exit.executed;
            report.slices += 1;
            report.instructions += exit.executed;
            debug_assert!(exit.now >= previous, "the core moved time backwards");
            self.now = exit.now.max(previous);
            // ReportProgress: the machine clock catches up with the CPU; events fire at their own times.
            self.fire_due(&mut report);
            match exit.reason {
                ExitReason::Deadline => {}
                ExitReason::StopRequested => {
                    self.stats.stop_requests += 1;
                    if self.cpu.take_reset_request() {
                        self.reset_requested = true;
                        report.reset_requested = true;
                        break;
                    }
                }
                ExitReason::Sleeping => {
                    // Renode skips `min(InstructionsToNearestLimit, instructions left in the round)` without
                    // looking at interrupts raised by the events that just fired.
                    let left = limit.saturating_sub(self.now).div_ceil(tpi);
                    let skip = self.instructions_to_nearest_limit().min(left);
                    if skip > 0 {
                        self.idle_to(self.now + skip * tpi, &mut report);
                    }
                }
                ExitReason::Halted => self.idle_to(limit, &mut report),
                ExitReason::Lockup => {
                    self.locked_up = true;
                    report.lockup = true;
                }
            }
            if self.now <= previous && !self.locked_up {
                // The core made no progress and did not ask to idle; never spin.
                self.stats.stalls += 1;
                self.idle_to(until, &mut report);
            }
        }
        report
    }

    /// Renode `BaseCPU.InstructionsToNearestLimit`: `floor(time to the nearest limit / ticks per instruction)`,
    /// at least 1 (the limit must be reached or surpassed for its owner to run). Without any limit the
    /// result is unbounded. The nearest limit is the earlier of the next queued event (clock entries and
    /// ordinary events) and the core's internal deadline.
    fn instructions_to_nearest_limit(&self) -> u64 {
        let event = self.core.events.peek_time();
        let deadline = self.cpu.next_internal_deadline();
        let nearest = match (event, deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match nearest {
            Some(time) => (time.saturating_sub(self.core.clock_time()) / self.ticks_per_instruction).max(1),
            None => u64::MAX,
        }
    }

    /// Fires the events that are due at the board time (the machine clock catches up with the CPU).
    fn fire_due(&mut self, report: &mut RunReport) {
        let fired = self.core.advance_clock(self.now);
        if fired > 0 {
            report.events += fired;
            self.stats.events_fired += u64::from(fired);
        }
        self.sync_irqs();
    }

    /// Advances over a period in which no instruction executes: the core's own timers run
    /// (`advance_idle`), then the machine clock advances to `to` and fires its events.
    fn idle_to(&mut self, to: Time, report: &mut RunReport) {
        if self.now < to {
            self.cpu.advance_idle(self.now, to);
            self.now = to;
            self.stats.idle_jumps += 1;
            self.fire_due(report);
        }
    }

    /// Time of the earliest pending event or CPU deadline, for hosts that want to sleep.
    pub fn next_wakeup(&self) -> Option<Time> {
        let event = self.core.events.peek_time();
        let deadline = self.cpu.next_internal_deadline();
        match (event, deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Cancels an event scheduled by a peripheral (fixtures that own events through `Ctx` call
    /// this via `with_peripheral`; exposed for completeness).
    pub fn cancel_event(&mut self, id: EventId) -> bool {
        self.core.events.cancel(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::{impl_peripheral_any, LogLevel, SyncRegister, TICKS_PER_INSTRUCTION};
    use std::cell::RefCell;
    use std::rc::Rc;

    const TPI: Time = TICKS_PER_INSTRUCTION;

    #[derive(Clone, Copy, Debug)]
    enum Op {
        Nop,
        Write32(u32, u32),
        Read32(u32),
        Wfi,
        Fault,
        SysReset,
    }

    /// A scripted core that honours the `Cpu` contract: whole instructions of `TPI` ticks,
    /// `run` stops once its time reaches `until`, MMIO accesses pass the executed-instruction
    /// count, notifications are polled after every access (a stop request ends the chunk right
    /// after the accessing instruction), WFI is an executed instruction that puts the core to sleep.
    struct MockCpu {
        icount: u64,
        halted: bool,
        sleeping: bool,
        deadline: Option<Time>,
        program: Vec<Op>,
        pc: usize,
        run_calls: Vec<(Time, Time)>,
        idle_calls: Vec<(Time, Time)>,
        irq_calls: Vec<(u32, bool, usize)>,
        drained: Vec<(u32, bool)>,
        reads: Vec<u32>,
        invalidations: u32,
        reset_pending: bool,
    }

    impl MockCpu {
        fn new(program: Vec<Op>) -> Self {
            Self {
                icount: 0,
                halted: false,
                sleeping: false,
                deadline: None,
                program,
                pc: 0,
                run_calls: Vec::new(),
                idle_calls: Vec::new(),
                irq_calls: Vec::new(),
                drained: Vec::new(),
                reads: Vec::new(),
                invalidations: 0,
                reset_pending: false,
            }
        }

        /// The core processes its own deadline (SysTick) once time has passed it.
        fn passed(&mut self, time: Time) {
            if self.deadline.is_some_and(|d| d <= time) {
                self.deadline = None;
            }
        }

        fn run_inner<B: CpuBus>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit {
            if self.halted {
                return RunExit { now, executed: 0, reason: ExitReason::Halted };
            }
            if self.sleeping {
                return RunExit { now, executed: 0, reason: ExitReason::Sleeping };
            }
            let mut t = now;
            let mut executed = 0;
            while t < until {
                let op = self.program.get(self.pc).copied().unwrap_or(Op::Nop);
                self.pc += 1;
                let before = self.icount;
                match op {
                    Op::Nop => {}
                    Op::Write32(addr, value) => bus.write32(addr, value, before),
                    Op::Read32(addr) => {
                        let value = bus.read32(addr, before);
                        self.reads.push(value);
                    }
                    Op::Wfi => self.sleeping = true,
                    Op::Fault => {
                        self.icount += 1;
                        return RunExit { now: t + TPI, executed: executed + 1, reason: ExitReason::Lockup };
                    }
                    Op::SysReset => {
                        self.reset_pending = true;
                        self.icount += 1;
                        return RunExit { now: t + TPI, executed: executed + 1, reason: ExitReason::StopRequested };
                    }
                }
                self.icount += 1;
                executed += 1;
                t += TPI;
                let bits = bus.take_notifications();
                if bits & armv7m::BUS_IRQ_CHANGED != 0 {
                    let drained = &mut self.drained;
                    bus.drain_irq_changes(&mut |irq, level| drained.push((irq, level)));
                }
                if bits & armv7m::BUS_STOP_REQUESTED != 0 {
                    return RunExit { now: t, executed, reason: ExitReason::StopRequested };
                }
                if self.sleeping {
                    return RunExit { now: t, executed, reason: ExitReason::Sleeping };
                }
            }
            RunExit { now: t, executed, reason: ExitReason::Deadline }
        }
    }

    impl CpuCore for MockCpu {
        fn run<B: CpuBus>(&mut self, bus: &mut B, now: Time, until: Time) -> RunExit {
            self.run_calls.push((now, until));
            let exit = self.run_inner(bus, now, until);
            self.passed(exit.now);
            exit
        }

        fn advance_idle(&mut self, now: Time, until: Time) {
            self.idle_calls.push((now, until));
            self.passed(until);
        }

        fn next_internal_deadline(&self) -> Option<Time> {
            self.deadline
        }

        fn set_irq_line(&mut self, irq: u32, level: bool) {
            self.irq_calls.push((irq, level, self.run_calls.len()));
            if level {
                self.sleeping = false;
            }
        }

        fn instructions(&self) -> u64 {
            self.icount
        }

        fn set_halted(&mut self, halted: bool) {
            self.halted = halted;
        }

        fn is_halted(&self) -> bool {
            self.halted
        }

        fn is_sleeping(&self) -> bool {
            self.sleeping
        }

        fn ppb_peek32(&self, addr: u32, _now: Time) -> Option<u32> {
            Some(0xAA00_0000 | (addr & 0xFFFF))
        }

        fn invalidate_code_cache(&mut self) {
            self.invalidations += 1;
        }

        fn take_reset_request(&mut self) -> bool {
            std::mem::take(&mut self.reset_pending)
        }
    }

    type Log = Rc<RefCell<Vec<String>>>;

    /// Peripheral used by the loop tests. Register 0x20 is read and written after `cpu.SyncTime()`.
    /// Write value semantics (the value is logged first, with `ctx.now()`):
    /// `0xE000_0000 | delay` schedules an ordinary event `delay` ns after the (lagging) clock time (token 2),
    /// `0xE100_0000 | delay` calls `schedule_action(delay)` (token 4), `0xF000_0000` calls `request_return`,
    /// `0xDEAD_0001` raises output line 0 (-> IRQ 5), `0xDEAD_0000` lowers it.
    /// Events: token 1 reschedules itself every `period` from its scheduled time, token 3 raises line 0.
    struct Device {
        log: Log,
        period: Option<Time>,
        events: Rc<RefCell<Vec<(Time, Time)>>>,
    }

    impl Peripheral for Device {
        fn name(&self) -> &str {
            "device"
        }

        fn sync_registers(&self) -> Vec<SyncRegister> {
            vec![SyncRegister::read_write(0x20)]
        }

        fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
            self.log.borrow_mut().push(format!("read@{offset:x} t={}", ctx.now()));
            0xCAFE_0000 + offset
        }

        fn write(&mut self, _offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push(format!("write={value:x} t={}", ctx.now()));
            match value & 0xFFFF_0000 {
                0xE000_0000 => {
                    ctx.schedule_in(u64::from(value & 0xFFFF), 2);
                }
                0xE100_0000 => {
                    ctx.schedule_action(u64::from(value & 0xFFFF), 4);
                }
                0xF000_0000 => ctx.request_return(),
                _ if value == 0xDEAD_0001 => ctx.set_output(0, true),
                _ if value == 0xDEAD_0000 => ctx.set_output(0, false),
                _ => {}
            }
        }

        fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
            self.events.borrow_mut().push((scheduled, ctx.now()));
            if token == 1 {
                if let Some(period) = self.period {
                    ctx.schedule_at(scheduled + period, 1);
                }
            } else if token == 3 {
                ctx.set_output(0, true);
            }
        }

        impl_peripheral_any!();
    }

    struct Rig {
        board: Board<MockCpu>,
        device: PeriphId,
        log: Log,
        events: Rc<RefCell<Vec<(Time, Time)>>>,
    }

    const DEVICE: u32 = 0x4000_0000;

    fn make_rig(program: Vec<Op>, period: Option<Time>) -> Rig {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut board = Board::with_cpu(MockCpu::new(program), "test", TPI);
        let device = board
            .add_mapped(DEVICE, 0x400, Box::new(Device { log: log.clone(), period, events: events.clone() }))
            .unwrap();
        board.connect_irq(device, 0, 5).unwrap();
        // The connect-time level push is applied lazily; flush it so the tests see only their own changes.
        board.sync_irqs();
        board.cpu.irq_calls.clear();
        Rig { board, device, log, events }
    }

    fn schedule(rig: &mut Rig, time: Time, token: u64) {
        let device = rig.device;
        rig.board.with_peripheral::<Device, _>(device, |_d, ctx| {
            ctx.schedule_at(time, token);
        });
    }

    // ---- chunk planning ------------------------------------------------------------------------------

    #[test]
    fn runs_whole_instructions_up_to_the_target() {
        let mut rig = make_rig(vec![], None);
        let report = rig.board.run_until(1000);
        assert_eq!(rig.board.now(), 1000);
        assert_eq!(report.instructions, 100);
        assert_eq!(rig.board.cpu.run_calls, [(0, 1000)]);
        assert_eq!(rig.board.clock_time(), 1000, "the clock caught up with the CPU");
        // Already at the target: nothing happens.
        let report = rig.board.run_until(1000);
        assert_eq!(report.slices, 0);
        // A target inside an instruction is reached at the next instruction boundary.
        rig.board.run_until(1005);
        assert_eq!(rig.board.now(), 1010);
        assert_eq!(rig.board.stats().instructions, 101);
    }

    #[test]
    fn rounds_follow_the_quantum_grid_and_chunks_never_cross_it() {
        let mut rig = make_rig(vec![], None);
        rig.board.run_until(250_000);
        assert_eq!(rig.board.cpu.run_calls, [(0, 100_000), (100_000, 200_000), (200_000, 250_000)]);
        // Stopping inside a round does not move the grid.
        let mut other = make_rig(vec![], None);
        other.board.run_until(130_000);
        other.board.run_until(250_000);
        assert_eq!(
            other.board.cpu.run_calls,
            [(0, 100_000), (100_000, 130_000), (130_000, 200_000), (200_000, 250_000)]
        );
    }

    #[test]
    fn a_chunk_stops_before_an_event_that_is_not_on_an_instruction_boundary() {
        let mut rig = make_rig(vec![], None);
        schedule(&mut rig, 1005, 2);
        rig.board.run_until(2_000);
        // floor(1005 / 10) = 100 instructions, then a one-instruction chunk crosses the event
        // (Renode InstructionsToNearestLimit; the minimum is one instruction).
        assert_eq!(rig.board.cpu.run_calls, [(0, 1000), (1000, 1010), (1010, 2000)]);
        // The event fired at its own time although the CPU was already at 1010.
        assert_eq!(*rig.events.borrow(), [(1005, 1005)]);
        assert_eq!(rig.board.now(), 2000);
    }

    #[test]
    fn an_event_on_an_instruction_boundary_ends_the_chunk_exactly() {
        let mut rig = make_rig(vec![], None);
        schedule(&mut rig, 1000, 2);
        rig.board.run_until(2_000);
        assert_eq!(rig.board.cpu.run_calls, [(0, 1000), (1000, 2000)]);
        assert_eq!(*rig.events.borrow(), [(1000, 1000)]);
        assert_eq!(rig.board.stats().events_fired, 1);
    }

    #[test]
    fn events_due_at_the_current_time_fire_before_the_cpu_runs() {
        let mut rig = make_rig(vec![], None);
        schedule(&mut rig, 0, 2);
        schedule(&mut rig, 0, 2);
        // They already ran when the calls returned (events due now never wait for the CPU).
        assert_eq!(*rig.events.borrow(), [(0, 0), (0, 0)]);
        rig.board.run_until(TPI);
        assert_eq!(rig.board.cpu.run_calls, [(0, TPI)]);
    }

    #[test]
    fn periodic_events_fire_at_their_own_times_without_drift() {
        let mut rig = make_rig(vec![], Some(1005));
        schedule(&mut rig, 1005, 1);
        rig.board.run_until(10_500);
        let scheduled: Vec<Time> = rig.events.borrow().iter().map(|e| e.0).collect();
        assert_eq!(scheduled, (1..=10).map(|i| i * 1005).collect::<Vec<Time>>());
        assert!(rig.events.borrow().iter().all(|&(s, n)| s == n), "ctx.now() is the event's own time");
    }

    #[test]
    fn chunked_runs_see_the_same_events() {
        let run = |chunks: &[Time]| {
            let mut rig = make_rig(vec![], Some(2500));
            schedule(&mut rig, 2500, 1);
            for &end in chunks {
                rig.board.run_until(end);
            }
            let scheduled: Vec<Time> = rig.events.borrow().iter().map(|e| e.0).collect();
            scheduled
        };
        let one = run(&[100_000]);
        let many = run(&[3_840, 7_680, 11_520, 38_400, 100_000]);
        assert_eq!(one, many);
        assert_eq!(one.len(), 40);
    }

    // ---- clock lag, syncs and return requests ---------------------------------------------------------

    #[test]
    fn mmio_sees_the_lagging_clock_time_except_for_sync_registers() {
        // Instruction 3 writes a plain register, 5 reads the sync register (0x20), 7 reads a plain register.
        let program = vec![
            Op::Nop,
            Op::Nop,
            Op::Nop,
            Op::Write32(DEVICE + 8, 0x77),
            Op::Nop,
            Op::Read32(DEVICE + 0x20),
            Op::Nop,
            Op::Read32(DEVICE + 4),
        ];
        let mut rig = make_rig(program, None);
        rig.board.run_until(10 * TPI);
        assert_eq!(*rig.log.borrow(), ["write=77 t=0", "read@20 t=50", "read@4 t=50"]);
        assert_eq!(rig.board.cpu.reads, [0xCAFE_0020, 0xCAFE_0004]);
        assert_eq!(rig.board.clock_time(), 10 * TPI);
    }

    #[test]
    fn mmio_time_is_relative_to_each_chunk_start() {
        // An event at 25 splits the run into (0,20), (20,30), (30,..): the access after the split sees the
        // clock time of its own chunk start, not time zero.
        let program = vec![Op::Nop; 5].into_iter().chain([Op::Write32(DEVICE, 1)]).collect();
        let mut rig = make_rig(program, None);
        schedule(&mut rig, 25, 2);
        rig.board.run_until(200);
        assert_eq!(rig.board.cpu.run_calls[0], (0, 20));
        assert_eq!(rig.board.cpu.run_calls[1], (20, 30));
        assert_eq!(rig.board.cpu.run_calls[2].0, 30);
        assert_eq!(*rig.log.borrow(), ["write=1 t=30"]);
    }

    #[test]
    fn irq_raised_by_an_mmio_write_reaches_the_core_before_its_next_instruction() {
        let program = vec![Op::Nop, Op::Write32(DEVICE, 0xDEAD_0001), Op::Nop, Op::Write32(DEVICE, 0xDEAD_0000)];
        let mut rig = make_rig(program, None);
        rig.board.run_until(6 * TPI);
        assert_eq!(rig.board.cpu.drained, [(5, true), (5, false)]);
        assert!(rig.board.cpu.irq_calls.is_empty(), "the core drained them itself during the chunk");
    }

    #[test]
    fn irq_raised_by_an_event_is_applied_before_the_core_runs_again() {
        let mut rig = make_rig(vec![], None);
        schedule(&mut rig, 1000, 3); // token 3: raise the output line
        rig.board.run_until(3000);
        // The call is made between the chunk that ended at 1000 and the next one.
        assert_eq!(rig.board.cpu.irq_calls, [(5, true, 1)]);
        assert_eq!(rig.board.cpu.run_calls.len(), 2);
    }

    #[test]
    fn request_return_ends_the_chunk_and_the_board_replans() {
        // Instruction 2 asks the CPU to return; the chunk would otherwise run to the end of the round.
        let program = vec![Op::Nop, Op::Nop, Op::Write32(DEVICE, 0xF000_0000), Op::Nop];
        let mut rig = make_rig(program, None);
        rig.board.run_until(100_000);
        assert_eq!(rig.board.cpu.run_calls, [(0, 100_000), (30, 100_000)]);
        assert_eq!(rig.board.stats().stop_requests, 1);
        assert_eq!(rig.board.now(), 100_000);
    }

    #[test]
    fn ordinary_events_scheduled_inside_a_chunk_wait_for_the_chunk_end() {
        let program = vec![Op::Nop, Op::Nop, Op::Write32(DEVICE, 0xE000_0000 | 500), Op::Nop];
        let mut rig = make_rig(program, None);
        rig.board.run_until(100_000);
        // schedule_in is relative to the lagging clock time (0); nothing asked the CPU to return.
        assert_eq!(rig.board.cpu.run_calls.len(), 1);
        assert_eq!(rig.board.stats().stop_requests, 0);
        assert_eq!(*rig.events.borrow(), [(500, 500)], "fired at its own time once the clock caught up");
    }

    #[test]
    fn schedule_action_syncs_requests_a_return_and_reports_the_scheduling_time() {
        let program = vec![Op::Nop, Op::Nop, Op::Write32(DEVICE, 0xE100_0000 | 500), Op::Nop];
        let mut rig = make_rig(program, None);
        rig.board.run_until(100_000);
        // The write (instruction 2, exact time 20) logs the lagging clock time, then ScheduleAction syncs to 20
        // and schedules the action for 520 and the CPU returns after the instruction; the next chunk ends at 520.
        assert_eq!(*rig.log.borrow(), ["write=e10001f4 t=0"]);
        assert_eq!(rig.board.cpu.run_calls, [(0, 100_000), (30, 520), (520, 100_000)]);
        assert_eq!(*rig.events.borrow(), [(20, 520)], "scheduled = scheduling time, now = firing time");
        assert_eq!(rig.board.stats().stop_requests, 1);
    }

    // ---- sleep, halt, lockup ----------------------------------------------------------------------------

    #[test]
    fn sleeping_core_skips_to_the_next_event() {
        let mut rig = make_rig(vec![Op::Wfi], Some(10_000));
        schedule(&mut rig, 10_000, 1);
        rig.board.run_until(35_000);
        // The board still offers the sleeping core a chunk after every event (it may have been woken lazily),
        // but no instruction executes after the WFI and time jumps exactly to the nearest limit.
        assert_eq!(rig.board.stats().instructions, 1, "{:?}", rig.board.cpu.run_calls);
        assert_eq!(rig.board.cpu.run_calls.len(), 4, "{:?}", rig.board.cpu.run_calls);
        let times: Vec<(Time, Time)> = rig.events.borrow().clone();
        assert_eq!(times, [(10_000, 10_000), (20_000, 20_000), (30_000, 30_000)], "exact times while idle");
        assert_eq!(rig.board.now(), 35_000);
        assert_eq!(rig.board.cpu.idle_calls, [(10, 10_000), (10_000, 20_000), (20_000, 30_000), (30_000, 35_000)]);
    }

    #[test]
    fn irq_wakes_a_sleeping_core() {
        let mut rig = make_rig(vec![Op::Wfi], None);
        schedule(&mut rig, 5_000, 3); // raises IRQ 5 at 5000
        rig.board.run_until(5_000 + 10 * TPI);
        assert!(!rig.board.cpu.sleeping);
        assert_eq!(rig.board.cpu.irq_calls.len(), 1);
        // After the wake-up the core executes again until the target.
        let last = *rig.board.cpu.run_calls.last().unwrap();
        assert_eq!(last, (5_000, 5_000 + 10 * TPI));
        assert_eq!(rig.board.stats().instructions, 11);
    }

    #[test]
    fn wfi_skip_does_not_look_at_interrupts_raised_by_the_event_that_ended_the_chunk() {
        // Renode BaseCPU: after ExecuteInstructions returns WaitingForInterrupt, ReportProgress fires the due
        // events and the WFI branch then skips min(InstructionsToNearestLimit, instructions left in the round)
        // without checking for pending interrupts. Here the event at 10 000 ends the chunk that contains the
        // WFI, raises the line (which wakes the core) and the core still idles to the end of the round.
        let mut program = vec![Op::Nop; 999];
        program.push(Op::Wfi);
        let mut rig = make_rig(program, None);
        schedule(&mut rig, 10_000, 3);
        rig.board.run_until(100_000);
        assert_eq!(rig.board.cpu.run_calls, [(0, 10_000)]);
        assert_eq!(rig.board.cpu.irq_calls, [(5, true, 1)]);
        assert_eq!(rig.board.cpu.idle_calls, [(10_000, 100_000)]);
        assert_eq!(rig.board.stats().instructions, 1000);
    }

    #[test]
    fn halted_core_does_not_run_but_peripherals_do() {
        let mut rig = make_rig(vec![Op::Write32(DEVICE, 1)], Some(1000));
        rig.board.set_halted(true);
        schedule(&mut rig, 1000, 1);
        rig.board.run_until(5_500);
        assert_eq!(rig.events.borrow().len(), 5);
        assert!(rig.board.cpu.run_calls.is_empty(), "no chunks while halted");
        assert!(rig.log.borrow().is_empty());
        assert_eq!(rig.board.now(), 5_500);
        // Releasing the core lets it run from the current time.
        rig.board.set_halted(false);
        rig.board.run_until(5_500 + 2 * TPI);
        assert_eq!(rig.log.borrow().len(), 1);
    }

    #[test]
    fn core_deadline_bounds_chunks_and_idle_jumps() {
        // A running core: the deadline is a limit like any event (floor rule, then a one-instruction chunk).
        let mut rig = make_rig(vec![], None);
        rig.board.cpu.deadline = Some(7_005);
        assert_eq!(rig.board.next_wakeup(), Some(7_005));
        rig.board.run_until(20_000);
        assert_eq!(rig.board.cpu.run_calls, [(0, 7_000), (7_000, 7_010), (7_010, 20_000)]);
        // A halted core: one idle jump per round; the core processes its own deadline inside advance_idle.
        let mut halted = make_rig(vec![], None);
        halted.board.set_halted(true);
        halted.board.cpu.deadline = Some(7_000);
        assert_eq!(halted.board.next_wakeup(), Some(7_000));
        halted.board.run_until(20_000);
        assert_eq!(halted.board.cpu.idle_calls, [(0, 20_000)]);
    }

    #[test]
    fn lockup_stops_the_core_but_not_time() {
        let mut rig = make_rig(vec![Op::Nop, Op::Fault], Some(1000));
        schedule(&mut rig, 1000, 1);
        let report = rig.board.run_until(4_000);
        assert!(report.lockup && rig.board.locked_up());
        assert_eq!(rig.events.borrow().len(), 4, "events keep firing after the lockup");
        assert_eq!(rig.board.now(), 4_000);
        let runs = rig.board.cpu.run_calls.len();
        rig.board.run_until(9_000);
        assert_eq!(rig.board.cpu.run_calls.len(), runs, "a locked-up core is not run again");
        rig.board.clear_lockup();
        assert!(!rig.board.locked_up());
    }

    #[test]
    fn a_core_that_makes_no_progress_cannot_hang_the_board() {
        struct Stuck;
        impl CpuCore for Stuck {
            fn run<B: CpuBus>(&mut self, _bus: &mut B, now: Time, _until: Time) -> RunExit {
                RunExit { now, executed: 0, reason: ExitReason::Deadline }
            }
            fn advance_idle(&mut self, _now: Time, _until: Time) {}
            fn next_internal_deadline(&self) -> Option<Time> {
                None
            }
            fn set_irq_line(&mut self, _irq: u32, _level: bool) {}
            fn instructions(&self) -> u64 {
                0
            }
            fn set_halted(&mut self, _halted: bool) {}
            fn is_halted(&self) -> bool {
                false
            }
            fn is_sleeping(&self) -> bool {
                false
            }
            fn ppb_peek32(&self, _addr: u32, _now: Time) -> Option<u32> {
                None
            }
        }
        let mut board = Board::with_cpu(Stuck, "stuck", TPI);
        board.run_until(10_000);
        assert_eq!(board.now(), 10_000);
        assert_eq!(board.stats().stalls, 1);
    }

    // ---- host-side access ------------------------------------------------------------------------------------

    #[test]
    fn external_stimuli_reach_the_core_and_models() {
        let mut rig = make_rig(vec![], None);
        let device = rig.device;
        rig.board.run_until(10 * TPI);
        // A model method run through the board sees the clock time and its IRQ is applied at once.
        rig.board.with_peripheral::<Device, _>(device, |_d, ctx| ctx.set_output(0, true));
        assert_eq!(rig.board.cpu.irq_calls, [(5, true, 1)]);
        rig.board.drive_output(device, 0, false);
        assert_eq!(rig.board.cpu.irq_calls.last(), Some(&(5, false, 1)));
        // Bus access from the host happens at the board's current time and has side effects.
        assert_eq!(rig.board.bus_read(DEVICE + 0x10, Width::Word), 0xCAFE_0010);
        rig.board.bus_write(DEVICE, Width::Word, 0x42);
        assert_eq!(
            *rig.log.borrow(),
            [format!("read@10 t={}", 10 * TPI), format!("write=42 t={}", 10 * TPI)]
        );
        // A return request raised outside a chunk does not leak into the next one.
        rig.board.bus_write(DEVICE, Width::Word, 0xF000_0000);
        let before = rig.board.stats().stop_requests;
        rig.board.run_until(20 * TPI);
        assert_eq!(rig.board.stats().stop_requests, before);
    }

    #[test]
    fn peek_poke_and_ppb() {
        let mut rig = make_rig(vec![], None);
        assert!(rig.board.poke(0x2000_0000, Width::Word, 0x1234_5678));
        assert_eq!(rig.board.peek32(0x2000_0000), Some(0x1234_5678));
        assert_eq!(rig.board.peek(0x2000_0001, Width::Byte), Some(0x56));
        assert_eq!(rig.board.peek32(DEVICE), None, "no side-effect-free view: peek must not read the register");
        assert!(rig.log.borrow().is_empty());
        // PPB registers come from the core.
        assert_eq!(rig.board.peek32(0xE000_ED10), Some(0xAA00_ED10));
        assert_eq!(rig.board.peek(0xE000_ED12, Width::Half), Some(0xAA00));
        assert_eq!(rig.board.peek(0xE000_ED13, Width::Half), None, "crosses the register");
        assert_eq!(rig.board.memory_slice(0x2000_0000, 4), Some(&[0x78u8, 0x56, 0x34, 0x12][..]));
        assert!(rig.board.load(0x0800_4000, &[1, 2, 3]).is_ok());
        assert_eq!(rig.board.peek(0x0800_4002, Width::Byte), Some(3));
        assert!(rig.board.load(0x4000_0000, &[1]).is_err());
    }

    #[test]
    fn system_reset_request_ends_the_run_and_is_reported() {
        let mut rig = make_rig(vec![Op::Nop, Op::SysReset, Op::Nop], None);
        let report = rig.board.run_until(100 * TPI);
        assert!(report.reset_requested && rig.board.reset_requested());
        assert_eq!(rig.board.now(), 2 * TPI, "returned right after the requesting instruction");
        assert_eq!(report.instructions, 2);
        rig.board.clear_reset_request();
        assert!(!rig.board.reset_requested());
        // The host may simply continue.
        let report = rig.board.run_until(100 * TPI);
        assert!(!report.reset_requested);
        assert_eq!(rig.board.now(), 100 * TPI);
    }

    #[test]
    fn headless_board_assembles_wires_and_runs_peripherals() {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut board = Board::headless("headless");
        let device = board
            .add_mapped(DEVICE, 0x400, Box::new(Device { log: log.clone(), period: Some(1000), events: events.clone() }))
            .unwrap();
        board.connect_irq(device, 0, 7).unwrap();
        // Assembly does not call into the core; the connect-time push arrives with the first run.
        assert!(board.cpu.irq_changes().is_empty());
        board.bus_write(DEVICE, Width::Word, 0xDEAD_0001); // raises output line 0 -> IRQ 7
        assert!(board.cpu.irq_level(7));
        board.bus_write(DEVICE, Width::Word, 0xDEAD_0000);
        assert!(!board.cpu.irq_level(7));
        assert_eq!(board.cpu.irq_changes(), [(7, false), (7, true), (7, false)]);
        // Events fire and time advances without a CPU.
        board.with_peripheral::<Device, _>(device, |_d, ctx| {
            ctx.schedule_at(1000, 1);
        });
        let report = board.run_until(5_500);
        assert_eq!(report.events, 5);
        assert_eq!(report.instructions, 0);
        assert_eq!(board.now(), 5_500);
        assert_eq!(board.clock_time(), 5_500);
        assert_eq!(events.borrow().len(), 5);
        assert_eq!(board.bus_read(DEVICE + 8, Width::Word), 0xCAFE_0008);
        assert!(board.peek32(0xE000_ED10).is_none());
    }

    // ---- flash and unmapped access --------------------------------------------------------------------------

    #[test]
    fn flash_store_ends_the_chunk_and_invalidates_the_code_cache() {
        let program = vec![Op::Nop, Op::Write32(0x0800_4000, 0x1234), Op::Write32(0x2000_0000, 1), Op::Nop];
        let mut rig = make_rig(program, None);
        rig.board.run_until(10 * TPI);
        // The flash store (instruction 1) stopped the chunk right after itself; the board
        // invalidated the predecode cache before resuming. The SRAM store did neither.
        assert_eq!(rig.board.cpu.run_calls, [(0, 10 * TPI), (2 * TPI, 10 * TPI)]);
        assert_eq!(rig.board.cpu.invalidations, 1);
        assert_eq!(rig.board.stats().code_invalidations, 1);
        assert_eq!(rig.board.stats().stop_requests, 1);
        assert_eq!(rig.board.peek32(0x0800_4000), Some(0x1234), "flash stores take effect (Renode MappedMemory)");
        assert_eq!(rig.board.peek32(0x2000_0000), Some(1));
    }

    #[test]
    fn loading_flash_invalidates_once_before_the_next_chunk() {
        let mut rig = make_rig(vec![], None);
        rig.board.load(0x0800_4000, &[1, 2, 3, 4]).unwrap();
        rig.board.run_until(5 * TPI);
        assert_eq!(rig.board.cpu.invalidations, 1);
        rig.board.run_until(10 * TPI);
        assert_eq!(rig.board.cpu.invalidations, 1, "no further invalidation without a flash write");
        // Host writes (monitor-style bus write, poke) count too; SRAM does not.
        rig.board.bus_write(0x0800_5000, Width::Word, 7);
        rig.board.poke(0x0800_5004, Width::Word, 8);
        rig.board.bus_write(0x2000_0010, Width::Word, 9);
        rig.board.run_until(12 * TPI);
        assert_eq!(rig.board.cpu.invalidations, 2);
        // A halted core is not invalidated until it runs again.
        rig.board.set_halted(true);
        rig.board.bus_write(0x0800_5008, Width::Word, 1);
        rig.board.run_until(14 * TPI);
        assert_eq!(rig.board.cpu.invalidations, 2);
        rig.board.set_halted(false);
        rig.board.run_until(16 * TPI);
        assert_eq!(rig.board.cpu.invalidations, 3);
    }

    #[test]
    fn unmapped_access_from_the_core_is_reported_once() {
        let program = vec![Op::Read32(0x5000_0000), Op::Read32(0x5000_0000), Op::Write32(0x5000_0004, 1)];
        let mut rig = make_rig(program, None);
        rig.board.run_until(5 * TPI);
        assert_eq!(rig.board.cpu.reads, [0, 0]);
        assert_eq!(rig.board.core.log.count(LogLevel::Warning), 2);
        assert_eq!(rig.board.core.stats.unmapped_reads, 2);
    }

    // The tests below use the real Cortex-M4F core (`armv7m::Cpu`).

    #[test]
    fn real_core_runs_a_tiny_program_with_a_sync_register_at_exact_time() {
        // 0x08004000: ldr r0, [pc, #8] ; movs r1, #0x55 ; str r1, [r0] ; b 1f ; 1: str r1, [r0] ; b . ;
        //             .word DEVICE + 0x20 (sync register)
        let code: [u8; 16] = [0x02, 0x48, 0x55, 0x21, 0x01, 0x60, 0xFF, 0xE7, 0x01, 0x60, 0xFE, 0xE7, 0x20, 0x00, 0x00, 0x40];
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut board = Board::new(BoardConfig::new("real"));
        board.add_mapped(DEVICE, 0x400, Box::new(Device { log: log.clone(), period: None, events })).unwrap();
        board.load(0x0800_4000, &code).unwrap();
        board.cpu.reset();
        board.cpu.set_vtor(0x0800_4000);
        board.cpu.set_sp(0x2001_8000);
        board.cpu.set_pc(0x0800_4000);
        board.run_until(20 * TPI);
        // Renode's `SyncTime()` (renode-semantics 5.4) reports the instructions of the translation blocks that
        // have been completed: the core passes the retire count at the start of the *block* of the accessing
        // instruction. The first store is the third instruction of the block that starts with the chunk, so the
        // sync register sees t = 0, not the 20 ns at which the store itself starts. The branch ends the block;
        // the second store is the first instruction of the next one (the fifth instruction, 40 ns).
        assert_eq!(*log.borrow(), [format!("write=55 t=0"), format!("write=55 t={}", 4 * TPI)]);
        assert_eq!(board.now(), 20 * TPI);
        assert_eq!(board.cpu.pc(), 0x0800_400A, "spinning on `b .`");
    }

    #[test]
    fn real_core_takes_an_interrupt_raised_by_a_peripheral_event() {
        // Vector table at 0x08004000: SP, reset handler 0x08004100, external IRQ0 handler 0x08004200.
        // reset:  ldr r0,=0xE000E100 ; movs r1,#1 ; str r1,[r0] (ISER0: enable IRQ0) ; b .
        // irq0:   ldr r0,=DEVICE ; ldr r1,=0xDEAD0000 ; str r1,[r0] (lowers the line) ; bx lr
        let mut image = vec![0u8; 0x300];
        let mut put32 = |offset: usize, value: u32| image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        put32(0, 0x2001_8000);
        put32(4, 0x0800_4101);
        put32(16 * 4, 0x0800_4201);
        put32(0x108, 0xE000_E100);
        put32(0x208, DEVICE);
        put32(0x20C, 0xDEAD_0000);
        let mut put16 = |offset: usize, value: u16| image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        for (offset, half) in [(0x100, 0x4801), (0x102, 0x2101), (0x104, 0x6001), (0x106, 0xE7FE)] {
            put16(offset, half);
        }
        for (offset, half) in [(0x200, 0x4801), (0x202, 0x4902), (0x204, 0x6001), (0x206, 0x4770)] {
            put16(offset, half);
        }

        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut board = Board::new(BoardConfig::new("real"));
        let device = board.add_mapped(DEVICE, 0x400, Box::new(Device { log: log.clone(), period: None, events })).unwrap();
        board.connect_irq(device, 0, 0).unwrap();
        board.with_peripheral::<Device, _>(device, |_d, ctx| {
            ctx.schedule_at(emu_core::from_micros(5), 3); // token 3: raise output line 0
        });
        board.load(0x0800_4000, &image).unwrap();
        board.cpu.reset();
        board.cpu.set_vtor(0x0800_4000);
        board.cpu.set_sp(0x2001_8000);
        board.cpu.set_pc(0x0800_4100);
        board.run_until(emu_core::from_micros(20));
        // 5 us is exactly 500 instructions: the chunk ends there, the event fires at that time and the core takes
        // the interrupt when the next chunk starts. The handler's `str` is its third instruction, but the
        // register is a plain one: the model sees the clock time of the chunk start (5 us), not 5 us + 2 instructions.
        assert_eq!(*log.borrow(), [format!("write=dead0000 t={}", emu_core::from_micros(5))]);
        assert!(!board.core.output_level(device, 0), "the handler lowered the line");
        assert_eq!(board.now(), emu_core::from_micros(20));
        assert_eq!(board.cpu.pc(), 0x0800_4106, "back in the spin loop after the handler returned");
    }

    #[test]
    #[ignore = "throughput measurement; run with --ignored --nocapture"]
    fn bench_real_core_idle_loop_with_quanta_and_events() {
        // A three-instruction polling loop (the FreeRTOS idle loop in miniature: load from SRAM,
        // compare, branch back) advanced in 100 us quanta with a 1 kHz peripheral event: the shape
        // of the steady state of each board in the dual system.
        // loop: ldr r0,[r1] ; cmp r0,#0 ; beq loop
        let code: [u8; 6] = [0x08, 0x68, 0x00, 0x28, 0xFC, 0xD0];
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut board = Board::new(BoardConfig::new("idle"));
        let device = board.add_mapped(DEVICE, 0x400, Box::new(Device { log, period: Some(emu_core::from_millis(1)), events: events.clone() })).unwrap();
        board.with_peripheral::<Device, _>(device, |_d, ctx| {
            ctx.schedule_at(emu_core::from_millis(1), 1);
        });
        board.load(0x0800_4000, &code).unwrap();
        board.load(0x0800_4000 + 0x100, &[0; 4]).unwrap();
        board.cpu.reset();
        board.cpu.set_vtor(0x0800_4000);
        board.cpu.set_sp(0x2001_8000);
        board.cpu.set_pc(0x0800_4000);
        board.cpu.set_reg(1, 0x2000_0000);
        let seconds = 2u64;
        let start = std::time::Instant::now();
        let mut t = 0;
        while t < seconds * emu_core::TICKS_PER_SECOND {
            t += emu_core::QUANTUM;
            board.run_until(t);
        }
        let elapsed = start.elapsed();
        let stats = *board.stats();
        println!(
            "idle loop, {} virtual s in {:.1} ms host time ({:.0}x real time): {} instructions, {} chunks, {} events; fast-forward {:?}",
            seconds,
            elapsed.as_secs_f64() * 1e3,
            seconds as f64 / elapsed.as_secs_f64(),
            stats.instructions,
            stats.slices,
            events.borrow().len(),
            board.cpu.fast_forward_stats(),
        );
        assert_eq!(events.borrow().len(), (seconds * 1000) as usize);
        assert_eq!(board.now(), t);
    }

    #[test]
    #[ignore = "smoke/throughput run on the real firmware; run with --ignored --nocapture"]
    fn real_core_smoke_boots_the_main_firmware_with_no_peripherals() {
        let roots = [
            std::env::var_os("NGC_FIRMWARE_DIR").map(std::path::PathBuf::from),
            Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware")),
        ];
        let Some(path) = roots
            .into_iter()
            .flatten()
            .map(|root| root.join("TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec"))
            .find(|p| p.is_file())
        else {
            eprintln!("skipping: main SREC not available locally");
            return;
        };
        let fw = crate::firmware::load(&std::fs::read(path).unwrap(), Some(crate::firmware::Role::Main)).unwrap();
        let mut board = Board::new(BoardConfig::new("main"));
        board.load(memory::FLASH_BASE, &fw.flash_image()).unwrap();
        board.cpu.reset();
        board.cpu.set_vtor(fw.vtor());
        board.cpu.set_sp(fw.initial_sp());
        board.cpu.set_pc(fw.reset_pc());
        let virtual_time = emu_core::from_millis(50);
        let start = std::time::Instant::now();
        let report = board.run_until(virtual_time);
        let elapsed = start.elapsed();
        println!(
            "main firmware, bare machine: {} instructions in {:.1} ms host time = {:.1} MIPS ({} chunks); PC 0x{:08x}; {} unmapped reads, {} unmapped writes",
            report.instructions,
            elapsed.as_secs_f64() * 1e3,
            report.instructions as f64 / elapsed.as_secs_f64() / 1e6,
            report.slices,
            board.cpu.pc(),
            board.core.stats.unmapped_reads,
            board.core.stats.unmapped_writes,
        );
        assert!(board.now() >= virtual_time);
        assert!(report.instructions > 1000 || board.cpu.is_sleeping());
    }
}
