// Signal semantics (change-only delivery, connect pushes the current level) follow Renode 1.17.0
// src/Emulator/Main/Core/GPIO.cs; unmapped-access behaviour follows SystemBus.ReportNonExistingRead/Write;
// the clock registry follows src/Emulator/Main/Time/BaseClockSource.cs (MIT License, Copyright (c) Antmicro).

//! The CPU-independent machine: plain memories, peripheral slots, the MMIO map, signal
//! nets, the clock registry and event queue, the IRQ-change queue, logging, and the `Ctx` handed to
//! peripherals.
//!
//! # Execution model
//!
//! Peripherals live in `Vec<Option<Box<dyn Peripheral>>>` slots. Every entry into
//! peripheral code (`read`, `write`, `on_event`, `on_input`, `reset`, typed `with_peripheral`)
//! takes the peripheral out of its slot, so the `Ctx` it receives can reach the rest of the
//! machine without aliasing it. A bus access that targets a peripheral that is currently
//! running (it accessed itself through `Ctx::mem_*`, or two peripherals call each other) is a
//! logged error that reads 0. Events that become due for a running peripheral are deferred and
//! delivered when its call returns.
//!
//! # Time
//!
//! The machine has its own **clock time** (Renode's clock-source time). It moves only through
//! [`MachineCore::advance_clock`] (the board calls it at the end of every CPU chunk and for idle
//! jumps) and through syncs: a declared sync register, [`Ctx::sync_time`] and
//! [`Ctx::schedule_action`] advance it to the exact time of the CPU access in progress first.
//! `advance_clock` fires the queued events in time order, setting the clock to each event's own time
//! before its handler runs. Clock events (limits of clock entries) of the same time are processed as
//! one batch: all entries are updated first, then the handlers run in entry creation order.
//!
//! # Signals
//!
//! `Ctx::set_output` records the new level and, when it changed, queues the deliveries:
//! `Target::Irq` pushes `(irq, level)` to the IRQ-change queue immediately, `Target::Input`
//! queues an `on_input` call. Queued input deliveries run when the producing peripheral call
//! returns - before control goes back to the CPU or the event loop - depth first and in
//! connection order. This differs from Renode's `GPIO.Set`, which re-enters the receiver in the
//! middle of the sender's method; it is what allows a receiver (for instance a DMA controller
//! reacting to an ADC request) to read back the sender's registers. See `docs/framework.md`.

use crate::access::{translate_read, translate_write, AccessPolicy, RegisterAccess, Width};
use crate::clock::{ClockEntry, ClockId, ClockRead, ClockRegistry, Direction, LocalClock, WorkMode};
use crate::event::{EventId, EventQueue, PendingEvent};
use crate::log::{LogBuffer, LogEntry, LogLevel, WarnSet};
use crate::memory::{MemoryLayout, PlainMemory};
use crate::peripheral::{PeriphId, Peripheral, SyncRegister, Target};
use crate::{Time, TICKS_PER_SECOND};
use std::fmt;

/// `MachineCore::take_notifications` bit: IRQ levels changed, drain the IRQ-change queue.
/// Equal to `armv7m::BUS_IRQ_CHANGED`.
pub const NOTIFY_IRQ_CHANGED: u32 = 1 << 0;
/// `MachineCore::take_notifications` bit: the running CPU chunk must end at the end of the current
/// translation block (Renode `RequestReturn`). Equal to `armv7m::BUS_STOP_REQUESTED`.
pub const NOTIFY_STOP_REQUESTED: u32 = 1 << 1;
/// Output lines per peripheral.
pub const MAX_OUTPUT_LINES: u32 = 64;
/// Safety valve for `advance_clock`: a handler that keeps scheduling itself at a time that is
/// already due would otherwise hang the host.
pub const MAX_EVENTS_PER_DRAIN: u32 = 10_000_000;

/// Warn-once source id for messages raised by the framework itself.
const MACHINE_SOURCE: u32 = u32::MAX;

// ---- MMIO map ---------------------------------------------------------

const GRANULE_SHIFT: u32 = 8;
const WINDOW_SHIFT: u32 = 20;
const WINDOWS: usize = 1 << (32 - WINDOW_SHIFT);
const GRANULES_PER_WINDOW: usize = 1 << (WINDOW_SHIFT - GRANULE_SHIFT);

#[derive(Clone, Copy)]
struct MmioRegion {
    base: u32,
    size: u32,
    periph: PeriphId,
    policy: AccessPolicy,
    /// This region's sync registers are `sync_lists[sync_start..sync_start + sync_len]`.
    sync_start: u32,
    sync_len: u32,
}

/// Two-level table: 1 MiB windows (allocated on demand) of 256-byte granules holding a
/// 1-based region index (0 = unmapped). Lookup is two dependent loads and a bounds compare.
struct MmioTable {
    windows: Box<[Option<Box<[u16; GRANULES_PER_WINDOW]>>; WINDOWS]>,
}

impl MmioTable {
    fn new() -> Self {
        let windows: Vec<Option<Box<[u16; GRANULES_PER_WINDOW]>>> = (0..WINDOWS).map(|_| None).collect();
        let windows: Box<[Option<Box<[u16; GRANULES_PER_WINDOW]>>; WINDOWS]> =
            windows.into_boxed_slice().try_into().expect("window table has WINDOWS entries");
        Self { windows }
    }

    #[inline]
    fn find(&self, regions: &[MmioRegion], addr: u32) -> Option<MmioRegion> {
        let window = self.windows[(addr >> WINDOW_SHIFT) as usize].as_deref()?;
        let index = window[((addr >> GRANULE_SHIFT) as usize) & (GRANULES_PER_WINDOW - 1)];
        if index == 0 {
            return None;
        }
        let region = regions[index as usize - 1];
        if addr.wrapping_sub(region.base) < region.size {
            Some(region)
        } else {
            None
        }
    }

    fn get(&self, granule: u32) -> u16 {
        match &self.windows[(granule >> (WINDOW_SHIFT - GRANULE_SHIFT)) as usize] {
            Some(window) => window[granule as usize & (GRANULES_PER_WINDOW - 1)],
            None => 0,
        }
    }

    fn set(&mut self, granule: u32, index: u16) {
        let window = self.windows[(granule >> (WINDOW_SHIFT - GRANULE_SHIFT)) as usize]
            .get_or_insert_with(|| Box::new([0u16; GRANULES_PER_WINDOW]));
        window[granule as usize & (GRANULES_PER_WINDOW - 1)] = index;
    }
}

/// Why `MachineCore::map` or `connect` rejected a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    UnknownPeripheral(PeriphId),
    /// The peripheral was running or already taken out when `map` queried its access policy.
    PeripheralBusy(PeriphId),
    ZeroSize,
    /// Region bases must be 256-byte aligned (the MMIO table granule).
    Misaligned(u32),
    /// The region does not fit in the 32-bit address space.
    OutOfAddressSpace,
    /// The region intersects flash or SRAM.
    OverlapsMemory { base: u32, size: u32 },
    /// The region intersects an earlier region.
    Overlap { base: u32, size: u32, with: String },
    TooManyRegions,
    /// `connect`: output line number is not below `MAX_OUTPUT_LINES`.
    LineOutOfRange(u32),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::UnknownPeripheral(id) => write!(f, "unknown peripheral {id}"),
            MapError::PeripheralBusy(id) => write!(f, "peripheral {id} is currently running"),
            MapError::ZeroSize => write!(f, "region size is zero"),
            MapError::Misaligned(base) => write!(f, "region base 0x{base:08x} is not 256-byte aligned"),
            MapError::OutOfAddressSpace => write!(f, "region exceeds the 32-bit address space"),
            MapError::OverlapsMemory { base, size } => {
                write!(f, "region 0x{base:08x}+0x{size:x} overlaps flash or SRAM")
            }
            MapError::Overlap { base, size, with } => {
                write!(f, "region 0x{base:08x}+0x{size:x} overlaps the region of '{with}'")
            }
            MapError::TooManyRegions => write!(f, "too many MMIO regions"),
            MapError::LineOutOfRange(line) => write!(f, "output line {line} is out of range (max {MAX_OUTPUT_LINES})"),
        }
    }
}

impl std::error::Error for MapError {}

/// A mapped region as reported by `MachineCore::mapped_regions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedRegion {
    pub name: String,
    pub periph: PeriphId,
    pub base: u32,
    pub size: u32,
}

// ---- machine ----------------------------------------------------------

struct Slot {
    periph: Option<Box<dyn Peripheral>>,
    name: String,
    /// Current level of each output line (bit n = line n).
    out_levels: u64,
    /// Fan-out of each output line, indexed by line number.
    nets: Vec<Vec<Target>>,
}

#[derive(Clone, Copy)]
struct PendingInput {
    dst: PeriphId,
    line: u32,
    level: bool,
}

/// An `on_event` call that is due: collected by `advance_clock`, or deferred because the owner is running.
#[derive(Clone, Copy)]
struct Handler {
    owner: PeriphId,
    token: u64,
    scheduled: Time,
    /// `schedule_action` entry to remove once the handler ran.
    remove: Option<ClockId>,
}

/// Cheap counters for diagnostics and benchmarks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachineStats {
    pub mmio_reads: u64,
    pub mmio_writes: u64,
    pub unmapped_reads: u64,
    pub unmapped_writes: u64,
    pub events_fired: u64,
    pub irq_changes: u64,
    /// Clock advances caused by a sync register or `Ctx::sync_time` inside a CPU chunk.
    pub syncs: u64,
}

pub struct MachineCore {
    /// Flash and SRAM.
    pub mem: PlainMemory,
    slots: Vec<Slot>,
    regions: Vec<MmioRegion>,
    sync_lists: Vec<SyncRegister>,
    table: MmioTable,
    /// Event queue; peripherals use it through `Ctx`, boards through `peek_time`/`advance_clock`.
    pub events: EventQueue,
    clocks: ClockRegistry,
    irq_queue: Vec<(u32, bool)>,
    /// Clock time of each entry of `irq_queue`.
    irq_times: Vec<Time>,
    pending: Vec<PendingInput>,
    notifications: u32,
    /// The machine's clock-source time (see the module documentation).
    clock_time: Time,
    /// Exact time of the CPU access in progress (`cpu_read`/`cpu_write`), if any.
    cpu_exact: Option<Time>,
    /// Handlers due for peripherals that were running when they became due, and zero-time limits.
    deferred: Vec<Handler>,
    flushing: bool,
    /// `advance_clock` is running (nested advances are no-ops).
    advancing: bool,
    /// Clock entries whose handler ran at the instant being processed (Renode `alreadyRunHandlers`).
    already_run: Vec<ClockId>,
    batch: Vec<Handler>,
    popped: Vec<u32>,
    flash_epoch_seen: u32,
    flash_dirty: bool,
    pub log: LogBuffer,
    warned: WarnSet,
    warn_overflow_noticed: bool,
    pub stats: MachineStats,
}

impl MachineCore {
    pub fn new(layout: MemoryLayout) -> Self {
        Self {
            mem: PlainMemory::new(layout),
            slots: Vec::new(),
            regions: Vec::new(),
            sync_lists: Vec::new(),
            table: MmioTable::new(),
            events: EventQueue::with_capacity(64),
            clocks: ClockRegistry::default(),
            irq_queue: Vec::with_capacity(32),
            irq_times: Vec::with_capacity(32),
            pending: Vec::with_capacity(16),
            notifications: 0,
            clock_time: 0,
            cpu_exact: None,
            deferred: Vec::new(),
            flushing: false,
            advancing: false,
            already_run: Vec::new(),
            batch: Vec::new(),
            popped: Vec::new(),
            flash_epoch_seen: 0,
            flash_dirty: false,
            log: LogBuffer::new(),
            warned: WarnSet::new(),
            warn_overflow_noticed: false,
            stats: MachineStats::default(),
        }
    }

    /// Records a write that reached flash (CPU store, DMA, poke, load). Flash is writable
    /// memory as in the stock platform, so anything that caches decoded flash must be told:
    /// the flag is exposed through `take_flash_dirty`, and a stop request ends the running CPU
    /// chunk right after the writing access so no stale decode executes.
    #[inline]
    fn note_memory_write(&mut self) {
        let epoch = self.mem.flash_epoch();
        if epoch != self.flash_epoch_seen {
            self.flash_epoch_seen = epoch;
            self.flash_dirty = true;
            self.notifications |= NOTIFY_STOP_REQUESTED;
        }
    }

    /// True once if flash changed since the last call (the board then invalidates the CPU's
    /// predecode cache).
    pub fn take_flash_dirty(&mut self) -> bool {
        std::mem::take(&mut self.flash_dirty)
    }

    /// Copies `bytes` into flash or SRAM (Renode `sysbus LoadBinary`).
    pub fn load_memory(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        let result = self.mem.load(addr, bytes);
        self.note_memory_write();
        result
    }

    // ---- assembly -------------------------------------------------------

    /// Registers a peripheral (not mapped on the bus yet) and calls its `attach` hook. The id is
    /// stable for the machine's life; the order of `add_peripheral` calls is the creation order of
    /// the clock entries the peripherals create in `attach`.
    pub fn add_peripheral(&mut self, peripheral: Box<dyn Peripheral>) -> PeriphId {
        let id = PeriphId(self.slots.len() as u32);
        let name = peripheral.name().to_string();
        self.slots.push(Slot { periph: Some(peripheral), name, out_levels: 0, nets: Vec::new() });
        self.enter(id, |p, ctx| p.attach(ctx));
        id
    }

    /// Maps `[base, base + size)` to the peripheral. Reads and writes arrive with offsets relative
    /// to `base`. A peripheral may be mapped more than once.
    pub fn map(&mut self, id: PeriphId, base: u32, size: u32) -> Result<(), MapError> {
        let slot = self.slots.get(id.index()).ok_or(MapError::UnknownPeripheral(id))?;
        let periph = slot.periph.as_ref().ok_or(MapError::PeripheralBusy(id))?;
        let policy = periph.access_policy();
        let syncs = periph.sync_registers();
        if size == 0 {
            return Err(MapError::ZeroSize);
        }
        if base & ((1 << GRANULE_SHIFT) - 1) != 0 {
            return Err(MapError::Misaligned(base));
        }
        let end = u64::from(base) + u64::from(size);
        if end > 1 << 32 {
            return Err(MapError::OutOfAddressSpace);
        }
        if self.mem.layout.overlaps(base, size) {
            return Err(MapError::OverlapsMemory { base, size });
        }
        if self.regions.len() >= usize::from(u16::MAX) {
            return Err(MapError::TooManyRegions);
        }
        let first = base >> GRANULE_SHIFT;
        let last = ((end - 1) as u32) >> GRANULE_SHIFT;
        for granule in first..=last {
            let owner = self.table.get(granule);
            if owner != 0 {
                let other = self.regions[usize::from(owner) - 1];
                return Err(MapError::Overlap {
                    base,
                    size,
                    with: self.slots[other.periph.index()].name.clone(),
                });
            }
        }
        let sync_start = self.sync_lists.len() as u32;
        self.sync_lists.extend_from_slice(&syncs);
        self.regions.push(MmioRegion { base, size, periph: id, policy, sync_start, sync_len: syncs.len() as u32 });
        let index = self.regions.len() as u16;
        for granule in first..=last {
            self.table.set(granule, index);
        }
        Ok(())
    }

    /// `add_peripheral` + `map`.
    pub fn add_mapped(&mut self, base: u32, size: u32, peripheral: Box<dyn Peripheral>) -> Result<PeriphId, MapError> {
        let id = self.add_peripheral(peripheral);
        self.map(id, base, size)?;
        Ok(id)
    }

    /// Connects output `line` of `src` to `target`. Mirrors `.repl` `->` lines; one line can have
    /// many targets (`|`). Duplicate connections are ignored. As in Renode's `GPIO.Connect`, the
    /// line's current level is pushed to the new target immediately (even when low).
    pub fn connect(&mut self, src: PeriphId, line: u32, target: Target) -> Result<(), MapError> {
        if line >= MAX_OUTPUT_LINES {
            return Err(MapError::LineOutOfRange(line));
        }
        if let Target::Input(dst, _) = target {
            if dst.index() >= self.slots.len() {
                return Err(MapError::UnknownPeripheral(dst));
            }
        }
        let slot = self.slots.get_mut(src.index()).ok_or(MapError::UnknownPeripheral(src))?;
        if slot.nets.len() <= line as usize {
            slot.nets.resize_with(line as usize + 1, Vec::new);
        }
        let net = &mut slot.nets[line as usize];
        if net.contains(&target) {
            return Ok(());
        }
        net.push(target);
        let level = slot.out_levels & (1u64 << line) != 0;
        match target {
            Target::Irq(irq) => self.push_irq_change(irq, level),
            Target::Input(dst, dst_line) => {
                let mark = self.pending.len();
                self.pending.push(PendingInput { dst, line: dst_line, level });
                self.process_pending(mark);
                self.settle();
            }
        }
        Ok(())
    }

    /// `connect(src, line, Target::Irq(irq))`.
    pub fn connect_irq(&mut self, src: PeriphId, line: u32, irq: u32) -> Result<(), MapError> {
        self.connect(src, line, Target::Irq(irq))
    }

    /// `connect(src, line, Target::Input(dst, dst_line))`.
    pub fn connect_input(&mut self, src: PeriphId, line: u32, dst: PeriphId, dst_line: u32) -> Result<(), MapError> {
        self.connect(src, line, Target::Input(dst, dst_line))
    }

    /// Removes every connection of an output line.
    pub fn disconnect(&mut self, src: PeriphId, line: u32) {
        if let Some(net) = self.slots.get_mut(src.index()).and_then(|s| s.nets.get_mut(line as usize)) {
            net.clear();
        }
    }

    // ---- lookup ---------------------------------------------------------

    pub fn peripheral_count(&self) -> usize {
        self.slots.len()
    }

    pub fn find(&self, name: &str) -> Option<PeriphId> {
        self.slots.iter().position(|s| s.name == name).map(|i| PeriphId(i as u32))
    }

    pub fn name_of(&self, id: PeriphId) -> &str {
        self.slots.get(id.index()).map_or("?", |s| s.name.as_str())
    }

    /// Typed shared access to a peripheral that is not currently running.
    pub fn get<T: Peripheral>(&self, id: PeriphId) -> Option<&T> {
        self.slots.get(id.index())?.periph.as_ref()?.as_any().downcast_ref::<T>()
    }

    pub fn get_mut<T: Peripheral>(&mut self, id: PeriphId) -> Option<&mut T> {
        self.slots.get_mut(id.index())?.periph.as_mut()?.as_any_mut().downcast_mut::<T>()
    }

    /// Runs `f` on the typed peripheral with a `Ctx`, so model methods that schedule events,
    /// drive outputs or touch the bus (UI inputs, CAN frame delivery, fixtures) work outside the
    /// CPU. Queued signal deliveries and due events run before this returns. `None` if the id is
    /// unknown, the type differs or the peripheral is running.
    pub fn with_peripheral<T: Peripheral, R>(
        &mut self,
        id: PeriphId,
        f: impl FnOnce(&mut T, &mut Ctx<'_>) -> R,
    ) -> Option<R> {
        let result = self.enter(id, |p, ctx| p.as_any_mut().downcast_mut::<T>().map(|t| f(t, ctx))).flatten();
        self.settle();
        result
    }

    /// Like `with_peripheral` for the trait object.
    pub fn with_dyn<R>(&mut self, id: PeriphId, f: impl FnOnce(&mut dyn Peripheral, &mut Ctx<'_>) -> R) -> Option<R> {
        let result = self.enter(id, f);
        self.settle();
        result
    }

    /// Every MMIO region in mapping order.
    pub fn mapped_regions(&self) -> Vec<MappedRegion> {
        self.regions
            .iter()
            .map(|r| MappedRegion { name: self.name_of(r.periph).to_string(), periph: r.periph, base: r.base, size: r.size })
            .collect()
    }

    /// `(name, summary)` of every peripheral that is not running.
    pub fn summaries(&self) -> Vec<(String, String)> {
        let view = View { core: self };
        self.slots
            .iter()
            .map(|s| (s.name.clone(), s.periph.as_ref().map(|p| p.summary(&view)).unwrap_or_default()))
            .collect()
    }

    // ---- time, notifications, IRQ queue ------------------------------------

    /// The machine's clock-source time: what `Ctx::now()` reports outside events.
    #[inline]
    pub fn clock_time(&self) -> Time {
        self.clock_time
    }

    /// Alias of [`MachineCore::clock_time`].
    #[inline]
    pub fn now(&self) -> Time {
        self.clock_time
    }

    /// Exact time of the CPU access in progress (`cpu_read`/`cpu_write`), if any.
    #[inline]
    pub fn cpu_access_time(&self) -> Option<Time> {
        self.cpu_exact
    }

    /// Time of the earliest queued event (clock limit or ordinary event).
    #[inline]
    pub fn next_event_time(&self) -> Option<Time> {
        self.events.peek_time()
    }

    /// Advances the clock to `to` (Renode `BaseClockSource.Advance`), firing every event with a time
    /// `<= to` in order: limits of clock entries first (all entries due at one instant are updated before
    /// any handler runs; handlers run in entry creation order), then ordinary events. `Ctx::now()` is
    /// each event's own time during its handler; handlers may schedule events that are processed in the
    /// same call. A nested call (a handler calling `Ctx::sync_time`) is a no-op. Returns the number of
    /// events processed.
    pub fn advance_clock(&mut self, to: Time) -> u32 {
        if self.advancing {
            return 0;
        }
        self.advancing = true;
        let mut fired = 0u32;
        let mut instant = Time::MAX;
        while let Some((head_time, _)) = self.events.peek_head() {
            if head_time > to {
                break;
            }
            let time = head_time.max(self.clock_time);
            if time != instant {
                instant = time;
                self.already_run.clear();
            }
            self.clock_time = time;
            fired += self.fire_head(time);
            if fired >= MAX_EVENTS_PER_DRAIN {
                self.log_machine(
                    LogLevel::Error,
                    format_args!(
                        "more than {MAX_EVENTS_PER_DRAIN} events due at one instant; dropping the events due now (runaway periodic event or zero-period clock entry?)"
                    ),
                );
                self.drop_due_events(time);
                break;
            }
        }
        if to > self.clock_time {
            self.clock_time = to;
        }
        self.advancing = false;
        self.already_run.clear();
        self.stats.events_fired += u64::from(fired);
        fired
    }

    /// Fires the head of the queue at `time` (an ordinary event) or the whole batch of clock events due
    /// by `time`. Returns how many events it consumed.
    fn fire_head(&mut self, time: Time) -> u32 {
        let Some((_, is_clock)) = self.events.peek_head() else { return 0 };
        if !is_clock {
            let Some(event) = self.events.pop_due_ex(time) else { return 0 };
            self.deliver(Handler { owner: event.periph, token: event.token, scheduled: event.time, remove: None });
            return 1;
        }
        // Phase 1: every clock event due now leaves the queue (creation order).
        let mut popped = std::mem::take(&mut self.popped);
        while let Some((event_time, true)) = self.events.peek_head() {
            if event_time > time {
                break;
            }
            if let Some(event) = self.events.pop_due_ex(time) {
                if let Some(index) = event.clock {
                    popped.push(index);
                }
            }
        }
        // Phase 2: all entries are updated before any handler runs (BaseClockSource.Update).
        let mut batch = std::mem::take(&mut self.batch);
        for &index in &popped {
            let slot = &mut self.clocks.slots[index as usize];
            slot.event = EventId::NONE;
            let reached = slot.clock.advance_to_reached(time);
            let id = ClockId { index, generation: slot.generation };
            if !reached {
                continue; // cannot happen with ceil limits; the entry is re-armed below
            }
            let handler = Handler {
                owner: slot.owner,
                token: slot.token,
                scheduled: slot.action_origin.unwrap_or(time),
                remove: slot.action_origin.map(|_| id),
            };
            if self.already_run.contains(&id) {
                if handler.remove.is_some() {
                    self.clock_remove(id);
                }
                continue; // Renode parity: a handler that already ran in this advance is not run again
            }
            self.already_run.push(id);
            batch.push(handler);
        }
        // Phase 3: next limits (a zero-period entry fires again in the next batch of this instant).
        for &index in &popped {
            if self.clocks.slots[index as usize].alive {
                self.clock_reschedule(index);
            }
        }
        let consumed = popped.len() as u32;
        popped.clear();
        self.popped = popped;
        // Phase 4: handlers in creation order, even those of entries a previous handler removed.
        for handler in batch.drain(..) {
            self.deliver(handler);
        }
        self.batch = batch;
        consumed
    }

    /// Error path of the runaway guard: removes every queued event due at `time` so the loop cannot restart
    /// (a clock entry that loses its event stays registered; reconfiguring it re-arms it).
    fn drop_due_events(&mut self, time: Time) {
        while let Some(event) = self.events.pop_due_ex(time) {
            if let Some(index) = event.clock {
                self.clocks.slots[index as usize].event = EventId::NONE;
            }
        }
    }

    /// Runs a due handler now, or defers it while its owner is running.
    fn deliver(&mut self, handler: Handler) {
        match self.slots.get(handler.owner.index()) {
            None => self.log_machine(LogLevel::Error, format_args!("event for {} could not be delivered (missing peripheral)", handler.owner)),
            Some(slot) if slot.periph.is_none() => self.deferred.push(handler),
            Some(_) => self.run_handler(handler),
        }
    }

    fn run_handler(&mut self, handler: Handler) {
        self.enter(handler.owner, |p, ctx| p.on_event(handler.token, handler.scheduled, ctx));
        if let Some(id) = handler.remove {
            self.clock_remove(id);
        }
    }

    /// Delivers deferred handlers whose owners are no longer running, oldest first.
    fn flush_deferred(&mut self) {
        if self.flushing {
            return;
        }
        self.flushing = true;
        let mut i = 0;
        while i < self.deferred.len() {
            let handler = self.deferred[i];
            if self.slots[handler.owner.index()].periph.is_some() {
                self.deferred.remove(i);
                self.run_handler(handler);
                i = 0;
            } else {
                i += 1;
            }
        }
        self.flushing = false;
    }

    /// End of a host-level entry point or CPU access: delivers deferred handlers and fires events that are
    /// already due at the current clock time (zero-time limits, `schedule_at(<= now)`).
    #[inline]
    fn settle(&mut self) {
        if !self.deferred.is_empty() {
            self.flush_deferred();
        }
        if let Some((time, _)) = self.events.peek_head() {
            if time <= self.clock_time && !self.advancing {
                self.advance_clock(self.clock_time);
            }
        }
    }

    /// Advances the clock to the exact time of the CPU access in progress (no-op otherwise).
    fn sync_to_cpu(&mut self) {
        if let Some(exact) = self.cpu_exact {
            if exact > self.clock_time && !self.advancing {
                self.stats.syncs += 1;
                self.advance_clock(exact);
            }
        }
    }

    pub fn pending_events(&self) -> Vec<PendingEvent> {
        self.events.pending()
    }

    /// Calls `reset` on every peripheral in registration order.
    pub fn reset_all(&mut self) {
        for index in 0..self.slots.len() {
            self.enter(PeriphId(index as u32), |p, ctx| p.reset(ctx));
        }
        self.settle();
    }

    /// Returns and clears the `NOTIFY_*` bits raised since the last call. The core polls this
    /// after every bus access, so the common "nothing raised" case is a load and a compare.
    #[inline(always)]
    pub fn take_notifications(&mut self) -> u32 {
        let bits = self.notifications;
        if bits != 0 {
            self.notifications = 0;
        }
        bits
    }

    #[inline]
    pub fn notifications(&self) -> u32 {
        self.notifications
    }

    pub fn clear_notifications(&mut self, mask: u32) {
        self.notifications &= !mask;
    }

    /// Delivers the accumulated `(irq, level)` changes in order and empties the queue.
    #[inline]
    pub fn drain_irq_changes(&mut self, sink: &mut dyn FnMut(u32, bool)) {
        for &(irq, level) in &self.irq_queue {
            sink(irq, level);
        }
        self.irq_queue.clear();
        self.irq_times.clear();
    }

    /// Like `drain_irq_changes`, also reporting the clock time at which each change happened.
    pub fn drain_irq_changes_timed(&mut self, sink: &mut dyn FnMut(Time, u32, bool)) {
        for (&time, &(irq, level)) in self.irq_times.iter().zip(&self.irq_queue) {
            sink(time, irq, level);
        }
        self.irq_queue.clear();
        self.irq_times.clear();
    }

    /// Queued `(irq, level)` changes not yet drained.
    pub fn irq_changes(&self) -> &[(u32, bool)] {
        &self.irq_queue
    }

    /// Queues an IRQ level change as if a peripheral output had produced it (fixtures, tests).
    pub fn push_irq_change(&mut self, irq: u32, level: bool) {
        self.irq_queue.push((irq, level));
        self.irq_times.push(self.clock_time);
        self.notifications |= NOTIFY_IRQ_CHANGED;
    }

    // ---- clock registry ------------------------------------------------------

    /// `AddClockEntry`: the entry starts counting at the current clock time.
    fn clock_add(&mut self, owner: PeriphId, entry: ClockEntry, token: u64, action_origin: Option<Time>) -> ClockId {
        let mut clock = LocalClock::new(entry, self.clock_time);
        let reached = clock.zero_update();
        let (index, id) = self.clocks.add(clock, owner, token, action_origin);
        self.clock_reschedule(index);
        if reached {
            self.queue_zero_time_handler(index);
        }
        id
    }

    /// `ExchangeClockEntryWith`: accounts the elapsed time, applies `change`, re-arms the entry's event.
    fn clock_exchange(&mut self, id: ClockId, change: impl FnOnce(ClockEntry) -> ClockEntry) {
        let Some(index) = self.clocks.index_of(id) else {
            self.log_machine_once(MACHINE_SOURCE, 0xC10C_0001, LogLevel::Error, format_args!("clock entry used before attach or after removal"));
            return;
        };
        let now = self.clock_time;
        let reached = self.clocks.slots[index as usize].clock.exchange(now, change);
        self.clock_reschedule(index);
        if reached {
            self.queue_zero_time_handler(index);
        }
    }

    fn clock_remove(&mut self, id: ClockId) -> bool {
        let Some(index) = self.clocks.index_of(id) else { return false };
        let event = self.clocks.slots[index as usize].event;
        self.events.cancel(event);
        self.clocks.release(index);
        true
    }

    /// Cancels the entry's queued event and schedules the next limit, if any.
    fn clock_reschedule(&mut self, index: u32) {
        let slot = &mut self.clocks.slots[index as usize];
        self.events.cancel(slot.event);
        slot.event = match slot.clock.next_limit() {
            Some(time) => self.events.schedule_clock(time, slot.order, slot.owner, slot.token, index),
            None => EventId::NONE,
        };
    }

    /// A reconfiguration put the entry at its limit: its handler is due now. It is delivered as soon as the
    /// running peripheral returns, unless it already ran at this instant (Renode `alreadyRunHandlers`).
    fn queue_zero_time_handler(&mut self, index: u32) {
        let id = self.clocks.id_of(index);
        let slot = &self.clocks.slots[index as usize];
        let handler = Handler {
            owner: slot.owner,
            token: slot.token,
            scheduled: slot.action_origin.unwrap_or(self.clock_time),
            remove: slot.action_origin.map(|_| id),
        };
        if self.advancing && self.already_run.contains(&id) {
            if handler.remove.is_some() {
                self.clock_remove(id);
            }
            return;
        }
        self.deferred.push(handler);
    }

    fn clock_snapshot(&self, id: ClockId) -> ClockEntry {
        match self.clocks.get(id) {
            Some(slot) => slot.clock.entry_at(self.clock_time),
            None => {
                debug_assert!(id.is_none(), "stale clock id");
                ClockEntry::new(0, 1, false, Direction::Ascending, WorkMode::Periodic)
            }
        }
    }

    /// Number of live clock entries (diagnostics, tests).
    pub fn clock_entry_count(&self) -> usize {
        self.clocks.live()
    }

    // ---- signals --------------------------------------------------------

    /// Current level of an output line.
    pub fn output_level(&self, id: PeriphId, line: u32) -> bool {
        line < MAX_OUTPUT_LINES && self.slots.get(id.index()).is_some_and(|s| s.out_levels & (1u64 << line) != 0)
    }

    /// Drives an input line of `dst` (Renode `peripheral OnGPIO line level` from a script or an
    /// external stimulus). Delivered immediately, at the current clock time.
    pub fn set_input(&mut self, dst: PeriphId, line: u32, level: bool) {
        if self.enter(dst, |p, ctx| p.on_input(line, level, ctx)).is_none() {
            self.log_machine(LogLevel::Error, format_args!("input {line} could not be delivered to {dst}"));
        }
        self.settle();
    }

    /// Drives an *output* line of `src` as if the peripheral had set it, delivering the change to
    /// the connected targets (used for fixtures such as pins held by the environment).
    pub fn drive_output(&mut self, src: PeriphId, line: u32, level: bool) {
        if src.index() >= self.slots.len() {
            self.log_machine(LogLevel::Error, format_args!("drive_output: unknown peripheral {src}"));
            return;
        }
        let mark = self.pending.len();
        self.set_output_line(src, line, level);
        self.process_pending(mark);
        self.settle();
    }

    fn set_output_line(&mut self, id: PeriphId, line: u32, level: bool) {
        if line >= MAX_OUTPUT_LINES {
            self.log_machine_once(id.0, u64::from(line) | (1 << 40), LogLevel::Error, format_args!("output line {line} is out of range (max {MAX_OUTPUT_LINES})"));
            return;
        }
        let bit = 1u64 << line;
        let slot = &mut self.slots[id.index()];
        if (slot.out_levels & bit != 0) == level {
            return;
        }
        slot.out_levels ^= bit;
        let Some(targets) = slot.nets.get(line as usize) else { return };
        for &target in targets {
            match target {
                Target::Irq(irq) => {
                    self.irq_queue.push((irq, level));
                    self.irq_times.push(self.clock_time);
                    self.notifications |= NOTIFY_IRQ_CHANGED;
                    self.stats.irq_changes += 1;
                }
                Target::Input(dst, dst_line) => self.pending.push(PendingInput { dst, line: dst_line, level }),
            }
        }
    }

    /// Delivers the queued input changes produced since `mark`, depth first. Items whose
    /// destination is running further up the call stack are kept for that call's own
    /// post-processing.
    fn process_pending(&mut self, mark: usize) {
        let mut keep = mark;
        let mut i = mark;
        while i < self.pending.len() {
            let item = self.pending[i];
            i += 1;
            match self.slots.get(item.dst.index()) {
                Some(slot) if slot.periph.is_some() => {
                    self.enter(item.dst, |p, ctx| p.on_input(item.line, item.level, ctx));
                }
                Some(_) => {
                    self.pending[keep] = item;
                    keep += 1;
                }
                None => {}
            }
        }
        self.pending.truncate(keep);
    }

    /// Takes the peripheral out of its slot, runs `f`, puts it back, delivers the signal
    /// changes `f` produced and the handlers that became due for it meanwhile. `None` if the id is
    /// unknown or the peripheral is already running.
    fn enter<R>(&mut self, id: PeriphId, f: impl FnOnce(&mut dyn Peripheral, &mut Ctx<'_>) -> R) -> Option<R> {
        let mut peripheral = self.slots.get_mut(id.index())?.periph.take()?;
        let mark = self.pending.len();
        let result = {
            let mut ctx = Ctx { core: &mut *self, id };
            f(&mut *peripheral, &mut ctx)
        };
        self.slots[id.index()].periph = Some(peripheral);
        if self.pending.len() > mark {
            self.process_pending(mark);
        }
        if !self.deferred.is_empty() {
            self.flush_deferred();
        }
        Some(result)
    }

    // ---- bus ------------------------------------------------------------

    /// Host/monitor system-bus read at the current clock time with MMIO side effects (Renode
    /// `sysbus ReadDoubleWord`). Plain memory is served directly; accesses straddling plain memory
    /// and anything else are split into bytes; unmapped addresses read 0 (warned once per address).
    /// Events that are due at the clock time run before this returns.
    pub fn bus_read(&mut self, addr: u32, width: Width) -> u32 {
        let value = self.bus_read_inner(addr, width);
        self.settle();
        value
    }

    /// Host/monitor system-bus write; see `bus_read`.
    pub fn bus_write(&mut self, addr: u32, width: Width, value: u32) {
        self.bus_write_inner(addr, width, value);
        self.settle();
    }

    #[inline]
    fn bus_read_inner(&mut self, addr: u32, width: Width) -> u32 {
        // MMIO first: the table and the plain memories are disjoint, and MMIO is what reaches
        // here from the CPU's slow path.
        if let Some(region) = self.table.find(&self.regions, addr) {
            self.stats.mmio_reads += 1;
            return self.region_read(region, addr - region.base, width);
        }
        self.memory_read(addr, width)
    }

    #[inline]
    fn bus_write_inner(&mut self, addr: u32, width: Width, value: u32) {
        if let Some(region) = self.table.find(&self.regions, addr) {
            self.stats.mmio_writes += 1;
            self.region_write(region, addr - region.base, width, value & width.mask());
            return;
        }
        self.memory_write(addr, width, value);
    }

    /// Plain memory, straddling accesses and unmapped addresses (everything `bus_read` does not
    /// find in the MMIO table).
    #[inline(never)]
    fn memory_read(&mut self, addr: u32, width: Width) -> u32 {
        if let Some(value) = self.mem.read(addr, width) {
            return value;
        }
        if width != Width::Byte && (self.mem.contains(addr) || self.mem.contains(addr.wrapping_add(width.bytes() - 1))) {
            // Splits like Renode's translation library: byte by byte, lowest address first.
            let mut value = 0u32;
            for i in 0..width.bytes() {
                value |= self.bus_read_inner(addr.wrapping_add(i), Width::Byte) << (8 * i);
            }
            return value;
        }
        self.unmapped_read(addr, width)
    }

    #[inline(never)]
    fn memory_write(&mut self, addr: u32, width: Width, value: u32) {
        if self.mem.write(addr, width, value) {
            self.note_memory_write();
            return;
        }
        if width != Width::Byte && (self.mem.contains(addr) || self.mem.contains(addr.wrapping_add(width.bytes() - 1))) {
            for i in 0..width.bytes() {
                self.bus_write_inner(addr.wrapping_add(i), Width::Byte, (value >> (8 * i)) & 0xFF);
            }
            return;
        }
        self.unmapped_write(addr, width, value);
    }

    /// CPU data read that missed the board's inline plain-memory paths. `exact` is the exact time of
    /// the accessing instruction: it is what a declared sync register (or `Ctx::sync_time`) advances the
    /// clock to; everything else sees the lagged clock time. Identical to `bus_read` except for
    /// **unaligned accesses outside plain memory**, which the stock platform's translation library never
    /// sends to the bus as one access (Renode tlib `softmmu_template.h`, documented in
    /// `docs/renode-semantics.md` 7.5): a load becomes two aligned loads of the same width merged by
    /// shift (both with MMIO side effects), a store becomes single-byte stores from the highest address
    /// down. Unaligned accesses to flash/SRAM are plain host accesses. After the access, events that
    /// became due at the clock time (zero-time limits) run before this returns.
    pub fn cpu_read(&mut self, addr: u32, width: Width, exact: Time) -> u32 {
        self.cpu_exact = Some(exact);
        let bytes = width.bytes();
        let value = if bytes > 1 && addr & (bytes - 1) != 0 && !self.mem.contains(addr) {
            // Renode parity: tlib splits unaligned MMIO loads into two aligned loads.
            let shift = (addr & (bytes - 1)) * 8;
            let aligned = addr & !(bytes - 1);
            let low = self.bus_read_inner(aligned, width);
            let high = self.bus_read_inner(aligned.wrapping_add(bytes), width);
            let merged = u64::from(low) | (u64::from(high) << (8 * bytes));
            ((merged >> shift) as u32) & width.mask()
        } else {
            self.bus_read_inner(addr, width)
        };
        self.cpu_exact = None;
        self.settle();
        value
    }

    /// CPU data write counterpart of `cpu_read`.
    pub fn cpu_write(&mut self, addr: u32, width: Width, value: u32, exact: Time) {
        self.cpu_exact = Some(exact);
        let bytes = width.bytes();
        if bytes > 1 && addr & (bytes - 1) != 0 && !self.mem.contains(addr) {
            // Renode parity: tlib splits unaligned MMIO stores into byte stores, highest first.
            for i in (0..bytes).rev() {
                self.bus_write_inner(addr.wrapping_add(i), Width::Byte, (value >> (8 * i)) & 0xFF);
            }
        } else {
            self.bus_write_inner(addr, width, value);
        }
        self.cpu_exact = None;
        self.settle();
    }

    #[cold]
    fn unmapped_read(&mut self, addr: u32, width: Width) -> u32 {
        self.stats.unmapped_reads += 1;
        self.log_machine_once(
            MACHINE_SOURCE,
            u64::from(addr),
            LogLevel::Warning,
            format_args!("Read{} from non existing peripheral at 0x{addr:X}, returning 0", width.renode_name()),
        );
        0
    }

    #[cold]
    fn unmapped_write(&mut self, addr: u32, width: Width, value: u32) {
        self.stats.unmapped_writes += 1;
        self.log_machine_once(
            MACHINE_SOURCE,
            u64::from(addr) | (1 << 40),
            LogLevel::Warning,
            format_args!("Write{} to non existing peripheral at 0x{addr:X}, value 0x{:X}", width.renode_name(), value & width.mask()),
        );
    }

    /// True if a CPU access of `width` at `offset` of `region` overlaps one of its declared sync registers.
    #[inline]
    fn sync_hit(&self, region: &MmioRegion, offset: u32, width: Width, write: bool) -> bool {
        let start = region.sync_start as usize;
        self.sync_lists[start..start + region.sync_len as usize].iter().any(|r| r.matches(offset, width, write))
    }

    #[inline]
    fn region_read(&mut self, region: MmioRegion, offset: u32, width: Width) -> u32 {
        if region.sync_len != 0 && self.cpu_exact.is_some() && self.sync_hit(&region, offset, width, false) {
            self.sync_to_cpu();
        }
        if region.policy.native.contains(width) {
            return self.call_read(region.periph, offset, width);
        }
        let id = region.periph;
        let result = translate_read(region.policy, &mut PeriphPort { core: self, id }, offset, width);
        match result {
            Some(value) => value,
            None => {
                self.warn_not_translated(id, offset, width, None);
                0
            }
        }
    }

    #[inline]
    fn region_write(&mut self, region: MmioRegion, offset: u32, width: Width, value: u32) {
        if region.sync_len != 0 && self.cpu_exact.is_some() && self.sync_hit(&region, offset, width, true) {
            self.sync_to_cpu();
        }
        if region.policy.native.contains(width) {
            self.call_write(region.periph, offset, width, value);
            return;
        }
        let id = region.periph;
        if !translate_write(region.policy, &mut PeriphPort { core: self, id }, offset, width, value) {
            self.warn_not_translated(id, offset, width, Some(value));
        }
    }

    #[inline]
    fn call_read(&mut self, id: PeriphId, offset: u32, width: Width) -> u32 {
        match self.enter(id, |p, ctx| p.read(offset, width, ctx)) {
            Some(value) => value & width.mask(),
            None => {
                self.reentrant_access(id, offset, false);
                0
            }
        }
    }

    #[inline]
    fn call_write(&mut self, id: PeriphId, offset: u32, width: Width, value: u32) {
        if self.enter(id, |p, ctx| p.write(offset, width, value, ctx)).is_none() {
            self.reentrant_access(id, offset, true);
        }
    }

    #[cold]
    fn reentrant_access(&mut self, id: PeriphId, offset: u32, write: bool) {
        let name = self.name_of(id).to_string();
        self.log_machine_once(
            id.0,
            u64::from(offset) | (u64::from(write) << 40) | (1 << 41),
            LogLevel::Error,
            format_args!("re-entrant bus {} of '{name}' at offset 0x{offset:X} while it is running; ignored", if write { "write" } else { "read" }),
        );
    }

    #[cold]
    fn warn_not_translated(&mut self, id: PeriphId, offset: u32, width: Width, value: Option<u32>) {
        let name = self.name_of(id).to_string();
        let key = u64::from(offset) | (u64::from(width.bytes()) << 32) | (u64::from(value.is_some()) << 40) | (1 << 42);
        match value {
            None => self.log_machine_once(
                id.0,
                key,
                LogLevel::Warning,
                format_args!("{name}: Attempted {} read isn't supported by the peripheral. Offset 0x{offset:X}.", width.renode_name()),
            ),
            Some(value) => self.log_machine_once(
                id.0,
                key,
                LogLevel::Warning,
                format_args!(
                    "{name}: Attempted {} write isn't supported by the peripheral. Offset 0x{offset:X}, value 0x{value:X}.",
                    width.renode_name()
                ),
            ),
        }
    }

    // ---- debugger access ------------------------------------------------

    /// Side-effect-free read of plain memory or of a peripheral register that offers a
    /// `Peripheral::peek` (translated like a bus access when the width is not native).
    /// `None` for unmapped addresses, running peripherals and registers without a peek.
    /// Time-dependent registers are computed for the current clock time.
    pub fn peek(&self, addr: u32, width: Width) -> Option<u32> {
        if let Some(value) = self.mem.read(addr, width) {
            return Some(value);
        }
        let region = self.table.find(&self.regions, addr)?;
        let peripheral = self.slots[region.periph.index()].periph.as_deref()?;
        let mut port = PeekPort { peripheral, view: View { core: self }, failed: false };
        let value = translate_read(region.policy, &mut port, addr - region.base, width)?;
        if port.failed {
            None
        } else {
            Some(value)
        }
    }

    /// Side-effect-free write: plain memory, or a native-width `Peripheral::poke`.
    pub fn poke(&mut self, addr: u32, width: Width, value: u32) -> bool {
        if self.mem.write(addr, width, value) {
            self.note_memory_write();
            return true;
        }
        let Some(region) = self.table.find(&self.regions, addr) else { return false };
        if !region.policy.native.contains(width) {
            return false;
        }
        let offset = addr - region.base;
        let done = self.enter(region.periph, |p, ctx| p.poke(offset, width, value, ctx)).unwrap_or(false);
        self.settle();
        done
    }

    // ---- logging --------------------------------------------------------

    /// Records a log entry attributed to `source` (a peripheral) or to the machine, stamped with the
    /// current clock time.
    pub fn log_entry(&mut self, level: LogLevel, source: Option<PeriphId>, args: fmt::Arguments<'_>) {
        if !self.log.enabled(level) {
            return;
        }
        let source = match source {
            Some(id) => self.name_of(id).to_string(),
            None => "machine".to_string(),
        };
        self.log.push(LogEntry { time: self.clock_time, level, source, message: args.to_string() });
    }

    fn log_machine(&mut self, level: LogLevel, args: fmt::Arguments<'_>) {
        self.log_entry(level, None, args);
    }

    /// Logs once per `(source, key)`; `source == MACHINE_SOURCE` attributes the entry to the machine.
    fn log_machine_once(&mut self, source: u32, key: u64, level: LogLevel, args: fmt::Arguments<'_>) {
        if !self.log.enabled(level) {
            return;
        }
        if self.first_time(source, key) {
            let origin = if source == MACHINE_SOURCE { None } else { Some(PeriphId(source)) };
            self.log_entry(level, origin, args);
        }
    }

    /// True the first time a `(source, key)` pair is seen. The memory of seen pairs is bounded
    /// (`WarnSet::DEFAULT_LIMIT`); when it overflows a single notice says so and later new pairs
    /// are treated as seen.
    fn first_time(&mut self, source: u32, key: u64) -> bool {
        if self.warned.insert(source, key) {
            return true;
        }
        if self.warned.suppressed() > 0 && !self.warn_overflow_noticed {
            self.warn_overflow_noticed = true;
            let limit = WarnSet::DEFAULT_LIMIT;
            self.log_machine(
                LogLevel::Warning,
                format_args!("warn-once table is full ({limit} distinct warnings); further first-time warnings are suppressed"),
            );
        }
        false
    }
}

/// `RegisterAccess` view of one peripheral for the translation helpers.
struct PeriphPort<'a> {
    core: &'a mut MachineCore,
    id: PeriphId,
}

impl RegisterAccess for PeriphPort<'_> {
    fn read(&mut self, offset: u32, width: Width) -> u32 {
        self.core.call_read(self.id, offset, width)
    }

    fn write(&mut self, offset: u32, width: Width, value: u32) {
        self.core.call_write(self.id, offset, width, value);
    }
}

/// Read-only `RegisterAccess` that maps to `Peripheral::peek`.
struct PeekPort<'a> {
    peripheral: &'a dyn Peripheral,
    view: View<'a>,
    failed: bool,
}

impl RegisterAccess for PeekPort<'_> {
    fn read(&mut self, offset: u32, width: Width) -> u32 {
        match self.peripheral.peek(offset, width, &self.view) {
            Some(value) => value,
            None => {
                self.failed = true;
                0
            }
        }
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32) {
        self.failed = true;
    }
}

/// Read-only view of the machine for `Peripheral::peek` and `summary`: the clock time and clock entry reads
/// (it implements [`ClockRead`]), plus side-effect-free memory reads.
pub struct View<'a> {
    core: &'a MachineCore,
}

impl<'a> View<'a> {
    /// The machine's clock time.
    pub fn now(&self) -> Time {
        self.core.clock_time
    }

    /// Side-effect-free read of another peripheral's register or of plain memory.
    pub fn mem_peek(&self, addr: u32, width: Width) -> Option<u32> {
        self.core.peek(addr, width)
    }
}

impl ClockRead for View<'_> {
    fn clock_now(&self) -> Time {
        self.core.clock_time
    }

    fn clock_entry(&self, id: ClockId) -> ClockEntry {
        self.core.clock_snapshot(id)
    }
}

/// What a peripheral can do to the rest of the machine while one of its methods runs.
pub struct Ctx<'a> {
    core: &'a mut MachineCore,
    id: PeriphId,
}

impl<'a> Ctx<'a> {
    /// The machine's clock time: the event's own time inside `on_event`; inside a CPU access the time of
    /// the chunk start or of the last sync (lagging the accessing instruction, see `sync_time`).
    #[inline]
    pub fn now(&self) -> Time {
        self.core.clock_time
    }

    /// Id of the running peripheral.
    #[inline]
    pub fn id(&self) -> PeriphId {
        self.id
    }

    /// Renode `cpu.SyncTime()`: advances the clock to the exact time of the CPU access in progress, firing
    /// the events on the way (events of this peripheral wait until it returns), and returns the new clock
    /// time. Outside a CPU access it does nothing and returns `now()`.
    pub fn sync_time(&mut self) -> Time {
        self.core.sync_to_cpu();
        self.core.clock_time
    }

    /// Renode `RequestReturn()`: the running CPU chunk ends at the end of the current translation block
    /// and the board plans the next one. `LimitTimer` setters and `schedule_action` call it for you.
    #[inline]
    pub fn request_return(&mut self) {
        self.core.notifications |= NOTIFY_STOP_REQUESTED;
    }

    /// Read-only view of the machine.
    pub fn view(&self) -> View<'_> {
        View { core: &*self.core }
    }

    // ---- ordinary events --------------------------------------------------

    /// Schedules `on_event(token, time, ..)` for this peripheral at absolute virtual `time`. A `time`
    /// that is already due fires right after the current access. Ordinary events do **not** request a
    /// CPU return: an event inside the running chunk fires when the chunk ends (call `request_return`
    /// or use `schedule_action` when it must not wait).
    #[inline]
    pub fn schedule_at(&mut self, time: Time, token: u64) -> EventId {
        self.core.events.schedule(time, self.id, token)
    }

    /// Schedules relative to `now()` (the lagging clock time inside a CPU chunk).
    #[inline]
    pub fn schedule_in(&mut self, delay: Time, token: u64) -> EventId {
        self.schedule_at(self.core.clock_time.saturating_add(delay), token)
    }

    /// Cancels a queued event; `false` if it already fired, was cancelled, or is `NONE`.
    #[inline]
    pub fn cancel(&mut self, id: EventId) -> bool {
        self.core.events.cancel(id)
    }

    #[inline]
    pub fn is_scheduled(&self, id: EventId) -> bool {
        self.core.events.is_pending(id)
    }

    /// Scheduled time of a queued event.
    pub fn event_time(&self, id: EventId) -> Option<Time> {
        self.core.events.time_of(id)
    }

    /// Cancels `*slot` (if queued) and schedules a replacement, storing its id in `*slot`.
    pub fn replace_event(&mut self, slot: &mut EventId, time: Time, token: u64) {
        self.core.events.cancel(*slot);
        *slot = self.schedule_at(time, token);
    }

    // ---- clock entries ----------------------------------------------------

    /// `AddClockEntry`: registers `entry` (counting from the current clock time) for this peripheral;
    /// its limit arrives as `on_event(token, ..)`. Entry creation order decides the order of handlers that
    /// expire together.
    pub fn clock_add(&mut self, entry: ClockEntry, token: u64) -> ClockId {
        self.core.clock_add(self.id, entry, token, None)
    }

    /// `ExchangeClockEntryWith`: accounts the elapsed time, applies `change` and re-arms the event. If the new
    /// state is already at its limit, the limit is reached now: the state is reset at once and the handler
    /// is delivered when this peripheral's method returns.
    pub fn clock_exchange(&mut self, id: ClockId, change: impl FnOnce(ClockEntry) -> ClockEntry) {
        self.core.clock_exchange(id, change);
    }

    /// Replaces the entry in place (keeping its creation order), like Renode's `InternalReset`.
    pub fn clock_replace(&mut self, id: ClockId, entry: ClockEntry) {
        self.core.clock_exchange(id, |_| entry);
    }

    /// `TryRemoveClockEntry`; `false` if the id is stale.
    pub fn clock_remove(&mut self, id: ClockId) -> bool {
        self.core.clock_remove(id)
    }

    /// `machine.ScheduleAction(delay, action)`: syncs the clock to the exact CPU time, registers a one-shot
    /// entry of `delay` ns, asks the CPU to return and removes the entry after `on_event(token, origin, ..)`
    /// ran. `origin` (the second argument) is the scheduling time; `now()` is `origin + delay`.
    pub fn schedule_action(&mut self, delay: Time, token: u64) -> ClockId {
        let origin = self.sync_time();
        let entry = ClockEntry::new(delay, TICKS_PER_SECOND, true, Direction::Ascending, WorkMode::OneShot);
        let id = self.core.clock_add(self.id, entry, token, Some(origin));
        self.request_return();
        id
    }

    // ---- signals ----------------------------------------------------------

    /// Drives output `line`. Targets are notified only when the level changes; input deliveries
    /// run when the current peripheral call returns (see the module documentation).
    #[inline]
    pub fn set_output(&mut self, line: u32, level: bool) {
        self.core.set_output_line(self.id, line, level);
    }

    /// Inverts output `line` (`GPIO.Toggle`).
    pub fn toggle_output(&mut self, line: u32) {
        let level = !self.output(line);
        self.set_output(line, level);
    }

    /// Current level of one of this peripheral's output lines.
    #[inline]
    pub fn output(&self, line: u32) -> bool {
        self.core.output_level(self.id, line)
    }

    // ---- bus (DMA) ----------------------------------------------------------

    /// System-bus read at `now()` with MMIO side effects (DMA source, `sysbus.ReadDoubleWord`). Never syncs.
    pub fn mem_read(&mut self, addr: u32, width: Width) -> u32 {
        self.core.bus_read_inner(addr, width)
    }

    /// System-bus write at `now()` with MMIO side effects.
    pub fn mem_write(&mut self, addr: u32, width: Width, value: u32) {
        self.core.bus_write_inner(addr, width, value);
    }

    /// Bulk read (`sysbus.ReadBytes`): copies plain memory directly, otherwise reads bytewise.
    pub fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) {
        if let Some(source) = self.core.mem.slice(addr, out.len()) {
            out.copy_from_slice(source);
            return;
        }
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = self.core.bus_read_inner(addr.wrapping_add(i as u32), Width::Byte) as u8;
        }
    }

    /// Bulk write (`sysbus.WriteBytes`): copies into plain memory directly, otherwise bytewise.
    pub fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) {
        if let Some(target) = self.core.mem.slice_mut(addr, data.len()) {
            target.copy_from_slice(data);
            self.core.note_memory_write();
            return;
        }
        for (i, &byte) in data.iter().enumerate() {
            self.core.bus_write_inner(addr.wrapping_add(i as u32), Width::Byte, u32::from(byte));
        }
    }

    /// True if `addr` is flash or SRAM (Renode `WhatIsAt(addr)` is `MappedMemory`).
    pub fn is_plain_memory(&self, addr: u32) -> bool {
        self.core.mem.contains(addr)
    }

    /// Side-effect-free read of another peripheral's register or of plain memory.
    pub fn mem_peek(&self, addr: u32, width: Width) -> Option<u32> {
        self.core.peek(addr, width)
    }

    // ---- CPU ----------------------------------------------------------------

    /// Deprecated alias of [`Ctx::request_return`].
    pub fn request_cpu_stop(&mut self) {
        self.request_return();
    }

    // ---- logging ------------------------------------------------------------

    #[inline]
    pub fn log_enabled(&self, level: LogLevel) -> bool {
        self.core.log.enabled(level)
    }

    /// Logs `args` (not formatted unless `level` is enabled).
    pub fn logf(&mut self, level: LogLevel, args: fmt::Arguments<'_>) {
        if self.core.log.enabled(level) {
            let id = self.id;
            self.core.log_entry(level, Some(id), args);
        }
    }

    pub fn log(&mut self, level: LogLevel, message: &str) {
        self.logf(level, format_args!("{message}"));
    }

    /// Logs at most once per `key` for this peripheral (for conditions that can repeat on every
    /// access, e.g. an unimplemented register).
    pub fn log_once(&mut self, level: LogLevel, key: u64, args: fmt::Arguments<'_>) {
        if self.core.log.enabled(level) && self.core.first_time(self.id.0, key) {
            let id = self.id;
            self.core.log_entry(level, Some(id), args);
        }
    }

    pub fn warn_once(&mut self, key: u64, args: fmt::Arguments<'_>) {
        self.log_once(LogLevel::Warning, key, args);
    }

    pub fn error_once(&mut self, key: u64, args: fmt::Arguments<'_>) {
        self.log_once(LogLevel::Error, key, args);
    }
}

impl ClockRead for Ctx<'_> {
    fn clock_now(&self) -> Time {
        self.core.clock_time
    }

    fn clock_entry(&self, id: ClockId) -> ClockEntry {
        self.core.clock_snapshot(id)
    }
}

/// `emu_log!(ctx, level, "format {}", args)`: formats only when `level` is enabled.
#[macro_export]
macro_rules! emu_log {
    ($ctx:expr, $level:expr, $($arg:tt)+) => {
        $ctx.logf($level, ::std::format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! emu_error {
    ($ctx:expr, $($arg:tt)+) => { $crate::emu_log!($ctx, $crate::LogLevel::Error, $($arg)+) };
}

#[macro_export]
macro_rules! emu_warn {
    ($ctx:expr, $($arg:tt)+) => { $crate::emu_log!($ctx, $crate::LogLevel::Warning, $($arg)+) };
}

#[macro_export]
macro_rules! emu_info {
    ($ctx:expr, $($arg:tt)+) => { $crate::emu_log!($ctx, $crate::LogLevel::Info, $($arg)+) };
}

#[macro_export]
macro_rules! emu_debug {
    ($ctx:expr, $($arg:tt)+) => { $crate::emu_log!($ctx, $crate::LogLevel::Debug, $($arg)+) };
}

#[macro_export]
macro_rules! emu_noisy {
    ($ctx:expr, $($arg:tt)+) => { $crate::emu_log!($ctx, $crate::LogLevel::Noisy, $($arg)+) };
}
