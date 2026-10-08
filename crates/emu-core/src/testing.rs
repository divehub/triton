//! Test harness for peripheral authors (and for anything else that wants a bare machine).
//!
//! `Harness` owns a `MachineCore` with the standard memory layout and no CPU. You map a
//! peripheral, perform bus reads and writes at the machine's clock time, advance time (firing
//! events in order), and observe output lines, IRQ changes and the event queue:
//!
//! ```
//! use emu_core::testing::Harness;
//! use emu_core::{ArrayMemory, Width};
//!
//! let mut h = Harness::new();
//! h.add_mapped(0x4000_7000, 0x400, ArrayMemory::new("pwr", 0x400));
//! h.write(0x4000_7010, Width::Word, 0x104);
//! assert_eq!(h.read(0x4000_7010, Width::Word), 0x104);
//! ```
//!
//! `read`/`write` are host accesses at the clock time. `cpu_read`/`cpu_write` behave like accesses
//! made by a CPU in the middle of a chunk: they carry the exact instruction time, the clock stays
//! where it is (so non-sync registers see the lagging clock time) unless the register is declared
//! with `Peripheral::sync_registers` or the model calls `ctx.sync_time()`. `end_chunk(t)` is the
//! board's `advance_clock` at the end of a chunk.
//!
//! Semantics match `Board::run_until` with an infinitely fast CPU: `advance_to` advances the
//! clock event by event, so `ctx.now()` equals each event's own time inside handlers.

use crate::access::Width;
use crate::machine::{Ctx, MachineCore, NOTIFY_IRQ_CHANGED, NOTIFY_STOP_REQUESTED};
use crate::memory::MemoryLayout;
use crate::peripheral::{PeriphId, Peripheral};
use crate::{LogLevel, Time};
use std::any::Any;

/// An IRQ level change observed by the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqChange {
    pub time: Time,
    pub irq: u32,
    pub level: bool,
}

/// One level delivered to a probe. Connecting a probe records the current level of the line as
/// its first entry (Renode `GPIO.Connect` semantics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeEvent {
    pub time: Time,
    pub level: bool,
}

/// Handle returned by [`Harness::probe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe(PeriphId);

struct ProbePeripheral {
    name: String,
    events: Vec<ProbeEvent>,
}

impl Peripheral for ProbePeripheral {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_input(&mut self, _line: u32, level: bool, ctx: &mut Ctx<'_>) {
        self.events.push(ProbeEvent { time: ctx.now(), level });
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct Harness {
    core: MachineCore,
    irq_log: Vec<IrqChange>,
    irq_levels: Vec<bool>,
    probes: usize,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_layout(MemoryLayout::STM32L4_1M)
    }

    pub fn with_layout(layout: MemoryLayout) -> Self {
        Self { core: MachineCore::new(layout), irq_log: Vec::new(), irq_levels: Vec::new(), probes: 0 }
    }

    /// Direct access to the machine for anything the harness does not wrap.
    pub fn core(&self) -> &MachineCore {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut MachineCore {
        &mut self.core
    }

    /// The machine's clock time (what `ctx.now()` reports for host accesses made through the harness).
    pub fn now(&self) -> Time {
        self.core.clock_time()
    }

    // ---- assembly ---------------------------------------------------------

    /// Adds a peripheral without mapping it (input-only or output-only models).
    pub fn add<P: Peripheral>(&mut self, peripheral: P) -> PeriphId {
        self.core.add_peripheral(Box::new(peripheral))
    }

    /// Adds and maps a peripheral. Panics on a mapping error (tests want loud failures).
    pub fn add_mapped<P: Peripheral>(&mut self, base: u32, size: u32, peripheral: P) -> PeriphId {
        match self.core.add_mapped(base, size, Box::new(peripheral)) {
            Ok(id) => id,
            Err(e) => panic!("cannot map peripheral at 0x{base:08x}+0x{size:x}: {e}"),
        }
    }

    pub fn connect_irq(&mut self, src: PeriphId, line: u32, irq: u32) {
        self.core.connect_irq(src, line, irq).expect("connect_irq");
        self.sync_irqs();
    }

    pub fn connect_input(&mut self, src: PeriphId, line: u32, dst: PeriphId, dst_line: u32) {
        self.core.connect_input(src, line, dst, dst_line).expect("connect_input");
        self.sync_irqs();
    }

    /// Adds a recording receiver on `src`'s output `line`.
    pub fn probe(&mut self, src: PeriphId, line: u32) -> Probe {
        self.probes += 1;
        let probe = ProbePeripheral { name: format!("probe{}", self.probes), events: Vec::new() };
        let id = self.add(probe);
        self.core.connect_input(src, line, id, 0).expect("probe connect");
        Probe(id)
    }

    /// Levels received by a probe, oldest first (the first entry is the level at connect time).
    pub fn probe_events(&self, probe: Probe) -> Vec<ProbeEvent> {
        self.core.get::<ProbePeripheral>(probe.0).map(|p| p.events.clone()).unwrap_or_default()
    }

    /// Levels received after the connect-time push, as `(time, level)`.
    pub fn probe_changes(&self, probe: Probe) -> Vec<(Time, bool)> {
        self.probe_events(probe).into_iter().skip(1).map(|e| (e.time, e.level)).collect()
    }

    // ---- bus --------------------------------------------------------------

    /// Host bus read at the clock time (side effects included, never syncs).
    pub fn read(&mut self, addr: u32, width: Width) -> u32 {
        let value = self.core.bus_read(addr, width);
        self.sync_irqs();
        value
    }

    /// Host bus write at the clock time.
    pub fn write(&mut self, addr: u32, width: Width, value: u32) {
        self.core.bus_write(addr, width, value);
        self.sync_irqs();
    }

    /// CPU data read made by an instruction at exact time `exact` inside a chunk: the clock is advanced to
    /// `exact` only for declared sync registers (or `ctx.sync_time()`), otherwise the model sees the lagging
    /// clock time. Unaligned MMIO accesses are split like the real bus does.
    pub fn cpu_read(&mut self, addr: u32, width: Width, exact: Time) -> u32 {
        let value = self.core.cpu_read(addr, width, exact);
        self.sync_irqs();
        value
    }

    /// CPU data write at exact time `exact`; see `cpu_read`.
    pub fn cpu_write(&mut self, addr: u32, width: Width, value: u32, exact: Time) {
        self.core.cpu_write(addr, width, value, exact);
        self.sync_irqs();
    }

    pub fn cpu_read8(&mut self, addr: u32, exact: Time) -> u32 {
        self.cpu_read(addr, Width::Byte, exact)
    }

    pub fn cpu_read16(&mut self, addr: u32, exact: Time) -> u32 {
        self.cpu_read(addr, Width::Half, exact)
    }

    pub fn cpu_read32(&mut self, addr: u32, exact: Time) -> u32 {
        self.cpu_read(addr, Width::Word, exact)
    }

    pub fn cpu_write8(&mut self, addr: u32, value: u32, exact: Time) {
        self.cpu_write(addr, Width::Byte, value, exact);
    }

    pub fn cpu_write16(&mut self, addr: u32, value: u32, exact: Time) {
        self.cpu_write(addr, Width::Half, value, exact);
    }

    pub fn cpu_write32(&mut self, addr: u32, value: u32, exact: Time) {
        self.cpu_write(addr, Width::Word, value, exact);
    }

    /// The board's `advance_clock` at the end of a CPU chunk (identical to `advance_to`).
    pub fn end_chunk(&mut self, time: Time) -> usize {
        self.advance_to(time)
    }

    pub fn read8(&mut self, addr: u32) -> u32 {
        self.read(addr, Width::Byte)
    }

    pub fn read16(&mut self, addr: u32) -> u32 {
        self.read(addr, Width::Half)
    }

    pub fn read32(&mut self, addr: u32) -> u32 {
        self.read(addr, Width::Word)
    }

    pub fn write8(&mut self, addr: u32, value: u32) {
        self.write(addr, Width::Byte, value);
    }

    pub fn write16(&mut self, addr: u32, value: u32) {
        self.write(addr, Width::Half, value);
    }

    pub fn write32(&mut self, addr: u32, value: u32) {
        self.write(addr, Width::Word, value);
    }

    /// Side-effect-free read (`Peripheral::peek` or plain memory) at the clock time.
    pub fn peek(&self, addr: u32, width: Width) -> Option<u32> {
        self.core.peek(addr, width)
    }

    // ---- time -------------------------------------------------------------

    /// Time of the earliest queued event.
    pub fn next_event_time(&self) -> Option<Time> {
        self.core.events.peek_time()
    }

    /// `(time, peripheral, token)` of every queued event in firing order.
    pub fn pending_events(&self) -> Vec<(Time, PeriphId, u64)> {
        self.core.pending_events().into_iter().map(|e| (e.time, e.periph, e.token)).collect()
    }

    /// Advances the clock to `target` (the board's `advance_clock`), firing every event due on the way in
    /// order. Inside a handler `ctx.now()` equals the event's own time (or the current time for events
    /// that are already late). IRQ changes are time-stamped with the time of the event that caused them.
    /// Returns the number of events fired.
    pub fn advance_to(&mut self, target: Time) -> usize {
        let now = self.core.clock_time();
        assert!(target >= now, "time cannot go backwards ({target} < {now})");
        let mut fired = 0usize;
        while let Some(next) = self.core.events.peek_time() {
            if next > target {
                break;
            }
            fired += self.core.advance_clock(next.max(self.core.clock_time())) as usize;
            self.sync_irqs();
        }
        self.core.advance_clock(target);
        self.sync_irqs();
        fired
    }

    pub fn advance_by(&mut self, delta: Time) -> usize {
        self.advance_to(self.core.clock_time() + delta)
    }

    /// Jumps to the next queued event and fires it (and everything else due then). Returns its time.
    pub fn run_next_event(&mut self) -> Option<Time> {
        let next = self.core.events.peek_time()?;
        self.advance_to(next.max(self.core.clock_time()));
        Some(next)
    }

    // ---- observation ------------------------------------------------------

    /// Current level of an output line of a peripheral (whether or not anything is connected to it).
    pub fn output(&self, id: PeriphId, line: u32) -> bool {
        self.core.output_level(id, line)
    }

    /// Level of NVIC input `irq` as last reported by a connected output.
    pub fn irq_level(&self, irq: u32) -> bool {
        self.irq_levels.get(irq as usize).copied().unwrap_or(false)
    }

    /// Every IRQ change since the last `clear_irq_changes`, in order (including the connect-time pushes).
    pub fn irq_changes(&self) -> &[IrqChange] {
        &self.irq_log
    }

    pub fn clear_irq_changes(&mut self) {
        self.irq_log.clear();
    }

    /// True once if a `ctx.request_return()` (`LimitTimer` setters, `schedule_action`, a flash write) was
    /// raised since the last call.
    pub fn take_stop_request(&mut self) -> bool {
        let bits = self.core.notifications();
        self.core.clear_notifications(NOTIFY_STOP_REQUESTED);
        bits & NOTIFY_STOP_REQUESTED != 0
    }

    /// Messages logged at `Warning` or above since the harness was created (or last drained).
    pub fn warnings(&self) -> Vec<String> {
        self.core.log.entries().filter(|e| e.level >= LogLevel::Warning).map(|e| e.message.clone()).collect()
    }

    pub fn drain_log(&mut self) -> Vec<crate::LogEntry> {
        self.core.log.drain()
    }

    // ---- peripherals --------------------------------------------------------

    /// Typed access to a mapped peripheral. Panics if the type is wrong.
    pub fn get<T: Peripheral>(&self, id: PeriphId) -> &T {
        self.core.get::<T>(id).expect("peripheral of that type")
    }

    pub fn get_mut<T: Peripheral>(&mut self, id: PeriphId) -> &mut T {
        self.core.get_mut::<T>(id).expect("peripheral of that type")
    }

    /// Runs `f` with the peripheral and a `Ctx` at harness time (typed model methods such as
    /// `receive_frame(ctx, ..)` or `press(ctx, ..)`).
    pub fn with<T: Peripheral, R>(&mut self, id: PeriphId, f: impl FnOnce(&mut T, &mut Ctx<'_>) -> R) -> R {
        let result = self.core.with_peripheral::<T, R>(id, f).expect("peripheral of that type");
        self.sync_irqs();
        result
    }

    /// Drives input `line` of `dst` (Renode `OnGPIO`).
    pub fn set_input(&mut self, dst: PeriphId, line: u32, level: bool) {
        self.core.set_input(dst, line, level);
        self.sync_irqs();
    }

    /// Drives output `line` of `src` as if the peripheral had set it.
    pub fn drive_output(&mut self, src: PeriphId, line: u32, level: bool) {
        self.core.drive_output(src, line, level);
        self.sync_irqs();
    }

    fn sync_irqs(&mut self) {
        let log = &mut self.irq_log;
        let levels = &mut self.irq_levels;
        self.core.drain_irq_changes_timed(&mut |time, irq, level| {
            log.push(IrqChange { time, irq, level });
            if levels.len() <= irq as usize {
                levels.resize(irq as usize + 1, false);
            }
            levels[irq as usize] = level;
        });
        self.core.clear_notifications(NOTIFY_IRQ_CHANGED);
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
