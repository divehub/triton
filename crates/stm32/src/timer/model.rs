// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32_Timer.cs,
// src/Emulator/Main/Peripherals/Timers/LimitTimer.cs and src/Emulator/Main/Time/BaseClockSource.cs
// (MIT License, Copyright (c) Antmicro).

//! State and clock-source emulation of one `Timers.STM32_Timer`.
//!
//! # The five clock entries
//!
//! Renode builds an `STM32_Timer` from five `LimitTimer`s, i.e. five entries of the machine's
//! `BaseClockSource`: the counter (entry 0, [`MAIN`]) and one one-shot compare timer per capture/compare
//! channel (entries 1..=4). Every enabled entry normally owns one clock-source event at its limit, so the
//! handset's TIM2 (about 40 kHz) and TIM15 (about 400 kHz) with their PWM channels would cost millions of
//! events per virtual second for no observable effect.
//!
//! # Lazy evaluation
//!
//! [`Model`] keeps the five entries itself ([`LocalClock`]: Renode's exact `ClockEntry` arithmetic) and
//! reproduces the relevant parts of `BaseClockSource` for them (`Update`, `ExchangeClockEntryWith`, the
//! re-update loop, `alreadyRunHandlers`), including the handler order of one instant. The entries' events are
//! **not** all pushed to the machine. Instead:
//!
//! * every access (register read/write, input edge, `peek`) first **catches up**: all limits up to the
//!   current clock time are processed in order by [`Model::catch_up`], running the same handlers the stock
//!   model would have run, so guest-visible state is identical at every access. Whole periods of a steady
//!   state are skipped in closed form ([`Periodic`], exact because the state at the anchor repeats);
//! * a real machine event (the *alarm*, one registry clock entry owned by the adapter) is armed for the
//!   next limit that the [`Scheduling`] policy keeps as an event: every limit of an enabled entry under the
//!   stock policies, or only the next limit whose handler looks at something outside the timer (it
//!   re-evaluates the interrupt outputs or writes an observed pin) under `Observable`; [`Model::plan`] finds
//!   it, for the latter by running the model forward on a clone.
//!
//! # Scheduling policies and CPU chunk boundaries
//!
//! The machine plans a CPU chunk against the earliest queued event, so an elided event is not free of
//! consequences: it removes a chunk boundary, and the clock-source time that lags the CPU inside a chunk
//! (`ctx.now()`) then differs from Renode's. [`Scheduling`] therefore decides which limits stay real events:
//!
//! * [`Scheduling::Stock`]: all of them, like the stock `STM32_Timer` (every enabled entry is in the queue);
//! * [`Scheduling::NgcArithmeticPwm`] (the default): like the handset's `NGCLazyPwmTimer` with
//!   `ArithmeticUnconnectedPWM` and `LazyUnconnectedPWM` (the runner's default). All limits are real events
//!   until a normal overflow finds the configuration eligible ([`Model::try_suppress`], a port of
//!   `TrySuppress`: periodic edge-aligned counter, `DIER = 0`, PWM outputs only, unconnected pins, integral
//!   nanoseconds per tick, ...); from then on the timer owns no event at all until the next register write,
//!   input edge or reset ([`Model::restore`], a port of `Restore`), which brings the stock events back;
//! * [`Scheduling::Observable`]: only the limits that touch an interrupt output or an observed pin, the
//!   fastest, with chunk boundaries that differ from Renode's.
//!
//! The model's own state is the exact stock state under every policy, so register values, interrupt edges and
//! pin edges are identical; only the set of queued events (and the `RequestReturn` calls of the handlers
//! that really run) differs.

use super::io::{Io, SimIo};
use emu_core::{ClockEntry, Direction, LocalClock, LogLevel, Time, WorkMode};

/// Which limits of the timer's clock entries are real machine events (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scheduling {
    /// Every limit of every enabled entry, as the stock `STM32_Timer`.
    Stock,
    /// The handset's `NGCLazyPwmTimer` in arithmetic mode: stock events until a normal overflow engages the
    /// suppression, none while it is engaged. The right choice for the handset's TIM2 and TIM15.
    #[default]
    NgcArithmeticPwm,
    /// Only the limits whose handlers touch an interrupt output or an observed pin.
    Observable,
}

/// Entry index of the counter; compare channel `i` is entry `1 + i`.
pub(crate) const MAIN: usize = 0;
pub(crate) const N_ENTRIES: usize = 5;
/// Safety valve for a single catch-up: instants processed before the rest is deferred to the next call.
const MAX_INSTANTS_PER_CALL: u32 = 5_000_000;
/// Limits reached again at the same instant before the entries involved are declared runaway.
const MAX_PER_INSTANT: u32 = 10_000;
/// Upper bound of instants the planner simulates before it falls back to a conservative wake-up.
const PLAN_CAP: u32 = 128;

/// Output lines: the interrupt outputs of the Renode class, then the channel pins.
pub const IRQ_LINE: u32 = 0;
pub const BREAK_LINE: u32 = 1;
pub const UPDATE_LINE: u32 = 2;
pub const TRIGGER_LINE: u32 = 3;
pub const COMMUTATION_LINE: u32 = 4;
pub const CAPTURE_COMPARE_LINE: u32 = 5;
/// First channel pin line (`Connections[i]` is line `PIN_LINE_BASE + i`).
pub const PIN_LINE_BASE: u32 = 8;

/// Static configuration (the `.repl` parameters).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cfg {
    /// `frequency`: the clock input of the prescaler in Hz.
    pub frequency: u64,
    /// `initialLimit`: reset value of ARR and width of the counter (`floor(log2(initialLimit)) + 1` bits).
    pub initial_limit: u32,
    pub bits: u32,
}

impl Cfg {
    pub(crate) fn mask(&self) -> u32 {
        if self.bits >= 32 {
            u32::MAX
        } else {
            (1u32 << self.bits) - 1
        }
    }

    fn main_entry(&self) -> ClockEntry {
        ClockEntry::new(u64::from(self.initial_limit), self.frequency, false, Direction::Ascending, WorkMode::Periodic)
    }

    fn cc_entry(&self) -> ClockEntry {
        ClockEntry::new(u64::from(self.initial_limit), self.frequency, false, Direction::Ascending, WorkMode::OneShot)
    }

    fn entry(&self, i: usize) -> ClockEntry {
        if i == MAIN {
            self.main_entry()
        } else {
            self.cc_entry()
        }
    }
}

/// One capture/compare channel (`CaptureCompareChannel` of the C# class).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Chan {
    /// `CaptureCompareSelection`: 0 output, 1 TI same, 2 TI cross, 3 TRC (never stored: unsupported).
    pub mode: u8,
    /// `InterruptEnable` (CCxIE).
    pub ie: bool,
    /// `InterruptFlag` (CCxIF).
    pub iflag: bool,
    /// `OutputEnable` (CCxE).
    pub oe: bool,
    /// `Polarity` (CCxP), stored in CCER.
    pub polarity: bool,
    /// `ComplementaryPolarity` (CCxNP), stored in CCER.
    pub comp_polarity: bool,
    /// `OvercaptureFlag` (CCxOF), stored in SR.
    pub overcapture: bool,
    /// `CapturedValue`.
    pub captured: u32,
    pub edge_counter: u32,
    /// `Signal`: the muxed input after TIx selection.
    pub signal: bool,
}

/// Everything except the clock entries that decides the model's future behavior.
///
/// Two equal values (together with equal entries) at two instants mean identical futures shifted in time:
/// that is what the planner's cycle detection relies on, so it must hold **all** such state and nothing
/// that only counts (statistics live in [`Model`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct St {
    pub auto_reload: u32,
    pub enable_requested: bool,
    pub repetitions_left: u32,
    /// `updateInterruptFlag` (UIF).
    pub update_flag: bool,
    // CR1 (stored fields)
    pub udis: bool,
    pub urs: bool,
    pub apre: bool,
    pub cms: u8,
    // CR2
    pub ti1s: bool,
    // SMCR
    pub sms: u8,
    pub ts: u8,
    // DIER
    pub uie: bool,
    pub tie: bool,
    // SR
    pub tif: bool,
    // RCR
    pub rcr: u8,
    pub ch: [Chan; 4],
    /// Underlying values of the four variants of CCMR1/CCMR2 (see `regs.rs`).
    pub ccmr: [[u32; 4]; 2],
    /// `Connections[i].IsSet`: the level of each channel pin.
    pub pins: [bool; 4],
    /// Levels of the interrupt outputs (`IRQ`, break, update, trigger, commutation, capture/compare).
    pub irq: [bool; 6],
    /// `NGCLazyPwmTimer.suppressed` (arithmetic mode): the stock events are elided. Only ever true under
    /// [`Scheduling::NgcArithmeticPwm`].
    pub engaged: bool,
    /// `NGCLazyPwmTimer.externalInput`: an input edge arrived; the suppression is refused until a reset.
    pub external_input: bool,
}

impl St {
    pub(crate) fn new(cfg: &Cfg) -> St {
        St {
            auto_reload: cfg.initial_limit,
            enable_requested: false,
            repetitions_left: 0,
            update_flag: false,
            udis: false,
            urs: false,
            apre: false,
            cms: 0,
            ti1s: false,
            sms: 0,
            ts: 0,
            uie: false,
            tie: false,
            tif: false,
            rcr: 0,
            ch: [Chan::default(); 4],
            ccmr: [[0; 4]; 2],
            pins: [false; 4],
            irq: [false; 6],
            engaged: false,
            external_input: false,
        }
    }
}

/// A steady state found by the planner: from `anchor` on, the state at every main-limit instant
/// `anchor + k * period` is the same and no externally visible output ever changes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Periodic {
    pub anchor: Time,
    pub period: Time,
}

/// Counters for tests and diagnostics (not part of the model state).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerStats {
    /// Limit instants processed (replayed or real).
    pub instants: u64,
    /// Whole periods skipped in closed form.
    pub skipped_cycles: u64,
    /// Planner runs.
    pub plans: u64,
    /// Instants simulated by the planner.
    pub plan_instants: u64,
    /// Alarm events delivered by the machine.
    pub alarms: u64,
    /// Times the NGC suppression engaged (`NGCLazyPwmTimer.SuppressionCount`).
    pub engagements: u64,
}

/// The state of one entry's neighborhood that the planner compares between instants.
#[derive(Clone, PartialEq, Eq)]
struct Signature {
    entries: [ClockEntry; N_ENTRIES],
    st: St,
}

/// Renode `BaseClockSource` state for the five entries.
#[derive(Clone)]
pub(crate) struct Clk {
    pub e: [LocalClock; N_ENTRIES],
    /// `LimitTimer.Divider` of each entry.
    pub div: [u64; N_ENTRIES],
    /// Absolute time of each enabled entry's next limit (the ceil-ns time), `None` when disabled.
    due: [Option<Time>; N_ENTRIES],
    /// `updateAlreadyInProgress`.
    in_update: bool,
    /// `reupdateNeeded`.
    reupdate: bool,
    /// `alreadyRunHandlers` (bit per entry) of the current top-level update.
    already_run: u8,
    /// The phantom limit of Renode's `nearestLimitIn`: the last update pass computed an entry's time to its limit
    /// *after* that one-shot entry had disabled itself on reaching it, so the CPU is still told to stop one period
    /// later. It lasts until the next update pass (any setter, any limit). Absolute time. Only kept while the
    /// timer's limits are stock events ([`Model::stock_events`]).
    pub phantom: Option<Time>,
}

/// `BaseClockSource`'s `emulatorTicksToLimit` of an entry in its current state, rounded up to whole
/// nanoseconds, **without looking at `Enabled`** (the update handlers compute it after a one-shot entry has
/// disabled itself). `u64::MAX` when it never gets there.
fn ns_to_limit_ignoring_enabled(entry: &ClockEntry) -> u64 {
    entry.ns_to_limit_ignoring_enabled()
}

/// The whole model: configuration, clock source, state.
#[derive(Clone)]
pub(crate) struct Model {
    pub cfg: Cfg,
    /// The model's current time: the clock-source time of the access or event being processed.
    pub t: Time,
    pub clk: Clk,
    pub st: St,
    /// Bit `i`: channel pin `i` is observed (a receiver is connected): its level changes are real events.
    pub observed: u8,
    /// Request CPU returns from the handlers (`LimitTimer` setters do in Renode); off while replaying
    /// elided events.
    pub emit_return: bool,
    pub periodic: Option<Periodic>,
    /// Which limits are real machine events.
    pub sched: Scheduling,
    /// A read changed state that the planner has not seen.
    pub dirty: bool,
    /// Unobserved pins whose level changed since the machine was last told (bit per channel).
    pub pin_flush: u8,
    pub stats: TimerStats,
    /// The time of the last instant in which the counter reached its limit.
    pub main_instant: Option<Time>,
}

impl Model {
    /// The state a freshly constructed (and reset) Renode timer is in, at clock time `now`.
    pub(crate) fn new(cfg: Cfg, observed: u8, now: Time) -> Model {
        let e = std::array::from_fn(|i| LocalClock::new(cfg.entry(i), now));
        Model {
            cfg,
            t: now,
            clk: Clk { e, div: [1; N_ENTRIES], due: [None; N_ENTRIES], in_update: false, reupdate: false, already_run: 0, phantom: None },
            st: St::new(&cfg),
            observed,
            emit_return: false,
            periodic: None,
            sched: Scheduling::default(),
            dirty: false,
            pin_flush: 0,
            stats: TimerStats::default(),
            main_instant: None,
        }
    }

    // ---- entry access ---------------------------------------------------------------------------

    /// `GetClockEntry` at `now`: the entry advanced to `now` (pure).
    #[inline]
    pub(crate) fn entry_at(&self, i: usize, now: Time) -> ClockEntry {
        self.clk.e[i].entry_at(now)
    }

    /// The entry at the model's current time.
    #[inline]
    pub(crate) fn entry(&self, i: usize) -> ClockEntry {
        self.clk.e[i].entry_at(self.t)
    }

    fn refresh_due(&mut self, i: usize) {
        self.clk.due[i] = if self.clk.e[i].entry().enabled() { self.clk.e[i].next_limit() } else { None };
    }

    /// Time of the earliest pending limit, including the phantom one (see [`Clk::phantom`]): the earliest
    /// time at which Renode's `nearestLimitIn` would stop the CPU.
    #[inline]
    pub(crate) fn next_event_time(&self) -> Option<Time> {
        let mut best: Option<Time> = self.clk.phantom;
        for due in self.clk.due.iter().flatten() {
            best = Some(best.map_or(*due, |b| b.min(*due)));
        }
        best
    }

    #[inline]
    pub(crate) fn rr(&mut self, io: &mut dyn Io) {
        if self.emit_return {
            io.request_return();
        }
    }

    /// True while every limit of the timer's enabled entries is a real event of the machine (stock scheduling,
    /// or the NGC policy outside the engaged state): the limits then cannot be replayed late.
    pub(crate) fn stock_events(&self) -> bool {
        match self.sched {
            Scheduling::Stock => true,
            Scheduling::NgcArithmeticPwm => !self.st.engaged,
            Scheduling::Observable => false,
        }
    }

    // ---- BaseClockSource -------------------------------------------------------------------------

    /// `BaseClockSource.Update(time)` at the model's time: every enabled entry is brought up to date, the
    /// entries that reached their limit are collected (unless their handler already ran in this top-level
    /// update) and their handlers run in entry order.
    fn update(&mut self, io: &mut dyn Io) {
        if self.clk.in_update {
            return;
        }
        self.clk.in_update = true;
        let mut notify = [0usize; N_ENTRIES];
        let mut n = 0;
        // `nearestLimitIn` is recomputed from scratch by every pass.
        let mut phantom: Option<Time> = None;
        let track_phantom = self.stock_events();
        for i in 0..N_ENTRIES {
            if !self.clk.e[i].entry().enabled() {
                continue;
            }
            if self.clk.e[i].advance_to_reached(self.t) {
                self.refresh_due(i);
                if track_phantom && !self.clk.e[i].entry().enabled() {
                    // Renode parity: the update handler computes the time to the limit after a one-shot entry
                    // disabled itself, and that value still lowers `nearestLimitIn`: the CPU stops one period
                    // later for an entry that no longer runs, unless a later pass (a setter) drops it.
                    let ns = ns_to_limit_ignoring_enabled(self.clk.e[i].entry());
                    if ns != u64::MAX {
                        let at = self.t.saturating_add(ns);
                        phantom = Some(phantom.map_or(at, |p| p.min(at)));
                    }
                }
                if self.clk.already_run & (1 << i) == 0 {
                    notify[n] = i;
                    n += 1;
                }
            }
        }
        self.clk.phantom = phantom;
        for &i in &notify[..n] {
            self.on_limit(io, i);
            self.clk.already_run |= 1 << i;
        }
        self.clk.in_update = false;
    }

    /// `UpdateLimits()` = `AdvanceInner(0, immediately: true)`: a nested call (from a handler) only requests
    /// a re-update, which the outermost call performs after its handlers finished.
    fn update_limits(&mut self, io: &mut dyn Io) {
        if self.clk.in_update {
            self.clk.reupdate = true;
            return;
        }
        self.clk.already_run = 0;
        self.update(io);
        while self.clk.reupdate {
            self.clk.reupdate = false;
            self.update(io);
        }
    }

    /// `ExchangeClockEntryWith(handler, visitor)`: flush, apply, flush (a state at its limit fires now).
    pub(crate) fn exchange(&mut self, io: &mut dyn Io, i: usize, change: impl FnOnce(ClockEntry) -> ClockEntry) {
        self.update_limits(io);
        let entry = *self.clk.e[i].entry();
        self.clk.e[i] = LocalClock::new(change(entry), self.t);
        self.refresh_due(i);
        self.update_limits(io);
    }

    /// Processes the limit instant `t` (`Advance` stopping at the nearest limit).
    fn process_instant(&mut self, io: &mut dyn Io, t: Time) {
        self.t = self.t.max(t);
        self.stats.instants += 1;
        self.main_instant = None;
        self.update_limits(io);
    }

    /// Processes every limit up to and including `target` in time order, then moves the model's time to
    /// `target`. Handlers of instants at or after `real_from` run as real events (they request CPU returns);
    /// earlier ones are replays of elided events.
    pub(crate) fn catch_up(&mut self, io: &mut dyn Io, target: Time, real_from: Time) {
        let mut guard = 0u32;
        let mut same_instant = 0u32;
        let mut last_instant = Time::MAX;
        while let Some(due) = self.next_event_time() {
            if due > target {
                break;
            }
            guard += 1;
            if guard > MAX_INSTANTS_PER_CALL {
                io.log_once(LogLevel::Error, 0xCA7C_0001, format_args!("too many timer events in one catch-up; deferring the rest"));
                break;
            }
            let t = due.max(self.t);
            if t == last_instant {
                same_instant += 1;
                if same_instant >= MAX_PER_INSTANT {
                    self.stop_runaway_entries(io, t);
                    same_instant = 0;
                    continue;
                }
            } else {
                last_instant = t;
                same_instant = 0;
            }
            let saved = self.emit_return;
            // A limit the stock model would have as an event of its own runs its handler with the setters'
            // `RequestReturn` calls (a no-op outside CPU accesses, but honored when a `SyncTime` processes it).
            self.emit_return = due >= real_from || self.stock_events();
            self.process_instant(io, t);
            self.emit_return = saved;
            if let (Some(p), Some(m)) = (self.periodic, self.main_instant) {
                if m == t {
                    self.skip_cycles(p, t, target);
                }
            }
        }
        if target > self.t {
            self.t = target;
        }
    }

    /// A periodic entry with a zero period reaches its limit again at the very instant it just fired, forever:
    /// Renode's `BaseClockSource.Advance` spins on it. Here the offending entries are disabled and an error
    /// is logged, so a guest that sets ARR to 0 behind a preload cannot hang the host.
    /// // Renode parity (deviation): the stock model never returns from such a state.
    fn stop_runaway_entries(&mut self, io: &mut dyn Io, t: Time) {
        io.log_once(
            LogLevel::Error,
            0xCA7C_0003,
            format_args!("a timer entry reaches its limit again without time passing (zero period); disabling it"),
        );
        for i in 0..N_ENTRIES {
            if self.clk.due[i].is_some_and(|due| due <= t) {
                let entry = *self.clk.e[i].entry();
                self.clk.e[i] = LocalClock::new(entry.with_enabled(false), self.t.max(t));
                self.refresh_due(i);
            }
        }
    }

    /// Closed-form skip of whole steady-state periods after the main-limit instant `t` (see [`Periodic`]).
    fn skip_cycles(&mut self, p: Periodic, t: Time, target: Time) {
        if p.period == 0 || t < p.anchor || (t - p.anchor) % p.period != 0 {
            return;
        }
        let k = target.saturating_sub(t) / p.period;
        if k == 0 {
            return;
        }
        let shift = k * p.period;
        for i in 0..N_ENTRIES {
            let clock = self.clk.e[i];
            if clock.entry().enabled() {
                self.clk.e[i] = LocalClock::new(*clock.entry(), clock.last_update() + shift);
            }
            self.refresh_due(i);
        }
        self.t = t + shift;
        self.stats.skipped_cycles += k;
        self.periodic = Some(Periodic { anchor: t + shift, period: p.period });
    }

    /// `cpu.SyncTime()` followed by the catch-up to the new clock time.
    pub(crate) fn sync(&mut self, io: &mut dyn Io) {
        let t = io.sync_time();
        self.catch_up(io, t, Time::MAX);
    }

    /// Start of every access: bring the model up to the access' clock time.
    #[inline]
    pub(crate) fn begin(&mut self, io: &mut dyn Io) {
        let now = io.now();
        self.catch_up(io, now, Time::MAX);
    }

    // ---- LimitTimer setters (each one is an ExchangeClockEntryWith) -------------------------------

    /// `LimitTimer.Enabled = value`.
    pub(crate) fn lt_set_enabled(&mut self, io: &mut dyn Io, i: usize, value: bool) {
        self.exchange(io, i, |e| e.with_enabled(value));
        self.rr(io);
    }

    /// `LimitTimer.Value = value` (Renode throws when it exceeds the initial limit; here: error log, ignored).
    pub(crate) fn lt_set_value(&mut self, io: &mut dyn Io, i: usize, value: u64) {
        if value > u64::from(self.cfg.initial_limit) {
            io.log_once(
                LogLevel::Error,
                0xCA7C_0002,
                format_args!("LimitTimer: value {value} cannot be larger than the limit {}", self.cfg.initial_limit),
            );
            return;
        }
        self.exchange(io, i, |e| e.with_value(value));
        self.rr(io);
    }

    /// `LimitTimer.Limit = limit` (`AutoUpdate` is false for every timer of the class).
    pub(crate) fn lt_set_limit(&mut self, io: &mut dyn Io, i: usize, limit: u64) {
        self.exchange(io, i, |e| e.with_period(limit));
        self.rr(io);
    }

    /// `LimitTimer.Divider = divider`: unchanged values are ignored (the residuum is kept); a change clears it.
    pub(crate) fn lt_set_divider(&mut self, io: &mut dyn Io, i: usize, divider: u64) {
        if divider == self.clk.div[i] {
            return;
        }
        self.clk.div[i] = divider;
        let frequency = self.cfg.frequency / divider;
        self.exchange(io, i, |e| e.with_frequency(frequency));
        self.rr(io);
    }

    pub(crate) fn lt_set_direction(&mut self, io: &mut dyn Io, i: usize, direction: Direction) {
        self.exchange(io, i, |e| e.with_direction(direction));
        self.rr(io);
    }

    /// `LimitTimer.Mode = mode`: the one setter that does not request a return.
    pub(crate) fn lt_set_mode(&mut self, io: &mut dyn Io, i: usize, mode: WorkMode) {
        self.exchange(io, i, |e| e.with_mode(mode));
    }

    pub(crate) fn lt_enabled(&self, i: usize) -> bool {
        self.entry(i).enabled()
    }

    pub(crate) fn lt_value(&self, i: usize) -> u64 {
        self.entry(i).value()
    }

    pub(crate) fn lt_limit(&self, i: usize) -> u64 {
        self.entry(i).period()
    }

    /// `LimitTimer.Reset()` (`InternalReset`): the initial entry replaces the old one in place.
    pub(crate) fn lt_reset(&mut self, io: &mut dyn Io, i: usize) {
        self.clk.div[i] = 1;
        let initial = self.cfg.entry(i);
        self.exchange(io, i, |_| initial);
    }

    // ---- planning ---------------------------------------------------------------------------------

    fn signature(&self, t: Time) -> Signature {
        Signature { entries: std::array::from_fn(|i| self.entry_at(i, t)), st: self.st.clone() }
    }

    /// Decides when the machine must wake the model up for an event the rest of the machine can see.
    ///
    /// Runs the model forward on a clone, instant by instant. The first instant whose handlers re-evaluate
    /// the interrupt outputs (an enabled update/compare interrupt) or touch an observed pin
    /// ([`Io::mark_event`]) is returned (the alarm time); the stock model has an event there whether or not
    /// the line is already high, and the CPU's chunk planning needs it in the queue. If the counter repeats
    /// the same state with no such instant in between, the model is periodic: `None` is returned and the
    /// period is remembered for the closed-form skip in [`Model::catch_up`]. If neither happens within
    /// [`PLAN_CAP`] instants the time of the last simulated instant is returned: waking up early is always
    /// exact, it only costs a wake-up. `None` also means "no event will ever happen".
    pub(crate) fn plan(&mut self) -> Option<Time> {
        self.stats.plans += 1;
        self.periodic = None;
        if self.stock_events() {
            // Every enabled entry has its limit in the machine's queue (and ends the CPU chunk there).
            return self.next_event_time();
        }
        let mut sim = self.clone();
        sim.emit_return = false;
        let mut io = SimIo { t: sim.t, visible: false };
        let mut anchor: Option<(Time, Signature)> = None;
        let mut last = sim.t;
        for _ in 0..PLAN_CAP {
            let Some(due) = sim.next_event_time() else { return None };
            let t = due.max(sim.t);
            io.t = t;
            sim.process_instant(&mut io, t);
            self.stats.plan_instants += 1;
            last = t;
            if io.visible {
                return Some(t);
            }
            if sim.main_instant == Some(t) {
                let signature = sim.signature(t);
                match &anchor {
                    None => anchor = Some((t, signature)),
                    Some((anchor_time, first)) if *first == signature => {
                        self.periodic = Some(Periodic { anchor: *anchor_time, period: t - *anchor_time });
                        return None;
                    }
                    Some(_) => {}
                }
            }
        }
        Some(last)
    }

    /// True if a limit is due at or before `now` (a `peek` then needs a replay on a copy).
    pub(crate) fn has_events_until(&self, now: Time) -> bool {
        self.next_event_time().is_some_and(|t| t <= now)
    }

    /// A copy of the model with every limit up to `now` replayed, or `None` when nothing is pending (the
    /// model itself is then already exact at `now`). Used by side-effect-free reads.
    pub(crate) fn replayed(&self, now: Time) -> Option<Model> {
        if !self.has_events_until(now) {
            return None;
        }
        let mut copy = self.clone();
        copy.emit_return = false;
        let mut io = super::io::NullIo { t: now };
        copy.catch_up(&mut io, now, Time::MAX);
        Some(copy)
    }
}
