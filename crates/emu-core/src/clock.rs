// Ported from Renode 1.17.0 src/Emulator/Main/Time/ClockEntry.cs, src/Emulator/Main/Time/BaseClockSource.cs,
// src/Emulator/Main/Peripherals/Timers/LimitTimer.cs and src/Emulator/Main/Core/Machine.cs
// (ObtainManagedThread, ScheduleAction) (MIT License, Copyright (c) Antmicro).

//! Renode clock entries: the arithmetic of `ClockEntry`/`BaseClockSource` and the Renode-shaped
//! wrappers `LimitTimer`, `ManagedThread` and (through [`crate::Ctx::schedule_action`]) `ScheduleAction`.
//!
//! * [`ClockEntry`] is a plain `Copy` value: `Value`, an exact-fraction residuum, `Period`, `Frequency`,
//!   direction, work mode, enabled. [`ClockEntry::advance`] is Renode's update handler (limit-reached
//!   rule, overshoot discarded, ceil-nanosecond time to the limit).
//! * [`LocalClock`] is an entry plus the time of its last update, for owners outside the machine registry
//!   (the CPU's SysTick and DWT).
//! * The machine keeps a registry of entries in creation order (`MachineCore`, `Ctx::clock_*`); each
//!   enabled entry owns one queue event at its limit time, which the owning peripheral receives in
//!   `on_event(token, ..)`.
//! * [`LimitTimer`] and [`ManagedThread`] wrap the registry operations exactly like the Renode classes
//!   (setter by setter, including which setters ask the CPU to return).
//!
//! See `docs/framework.md` section 5 for usage and `docs/renode-semantics.md` sections 3-4 for the
//! reference behaviour.

use crate::event::EventId;
use crate::machine::Ctx;
use crate::peripheral::PeriphId;
use crate::{Time, TICKS_PER_SECOND};

/// Safety valve for loops that process limits at one instant (a zero-period periodic entry).
const MAX_LIMITS_PER_RUN: u32 = 10_000_000;

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Non-negative exact fraction `num/den` (`den >= 1`), always reduced; zero is `0/1` like Renode's `Fraction.Zero`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frac {
    num: u64,
    den: u64,
}

impl Frac {
    const ZERO: Frac = Frac { num: 0, den: 1 };

    fn reduced(num: u128, den: u128) -> Frac {
        if num == 0 || den == 0 {
            return Frac::ZERO;
        }
        let g = gcd(num, den);
        let clamp = |v: u128| u64::try_from(v).unwrap_or(u64::MAX);
        Frac { num: clamp(num / g), den: clamp(den / g) }
    }
}

/// Counting direction (Renode `Direction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// `Value` counts up from 0; the limit is reached when `Value >= Period`.
    Ascending,
    /// `Value` counts down from `Period`; the limit is reached when the elapsed whole ticks `>= Value`.
    Descending,
}

/// What an entry does when it reaches its limit (Renode `WorkMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkMode {
    /// Re-arms: `Value` restarts at 0 (ascending) or `Period` (descending).
    Periodic,
    /// Disables itself at its first limit.
    OneShot,
}

/// Result of [`ClockEntry::advance`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    /// The limit was reached during this update (the entry's handler must run).
    pub reached: bool,
    /// Nanoseconds from now to the next limit, rounded **up**; `u64::MAX` when the entry will never
    /// reach it (disabled, or a zero frequency).
    pub to_limit: u64,
}

/// A Renode `ClockEntry` without its handler: pure state plus the update rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockEntry {
    value: u64,
    residuum: Frac,
    period: u64,
    frequency: u64,
    step: u64,
    /// `step * frequency / 1e9`, reduced: entry ticks per nanosecond.
    ratio: Frac,
    enabled: bool,
    direction: Direction,
    mode: WorkMode,
}

fn ratio_of(step: u64, frequency: u64) -> Frac {
    Frac::reduced(u128::from(step) * u128::from(frequency), u128::from(TICKS_PER_SECOND))
}

impl ClockEntry {
    /// Renode's `new ClockEntry(period, frequency, handler, owner, name, enabled, direction, workMode)`:
    /// `Value` starts at 0 (ascending) or `period` (descending), the residuum at zero.
    pub fn new(period: u64, frequency: u64, enabled: bool, direction: Direction, mode: WorkMode) -> Self {
        let step = 1;
        ClockEntry {
            value: if direction == Direction::Ascending { 0 } else { period },
            residuum: Frac::ZERO,
            period,
            frequency,
            step,
            ratio: ratio_of(step, frequency),
            enabled,
            direction,
            mode,
        }
    }

    // ---- Renode `ClockEntry.With(...)` -----------------------------------------------------------

    /// `With(period: p)`: `Value` and residuum are kept (even when `Value` is now above the limit).
    pub fn with_period(mut self, period: u64) -> Self {
        self.period = period;
        self
    }

    /// `With(frequency: f)`: the **residuum is cleared**, `Value` is kept.
    pub fn with_frequency(mut self, frequency: u64) -> Self {
        self.frequency = frequency;
        self.ratio = ratio_of(self.step, frequency);
        self.residuum = Frac::ZERO;
        self
    }

    /// `With(step: s)`: the ratio changes, `Value` and residuum are kept.
    pub fn with_step(mut self, step: u64) -> Self {
        self.step = step;
        self.ratio = ratio_of(step, self.frequency);
        self
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// `With(value: v)`: the residuum is kept.
    pub fn with_value(mut self, value: u64) -> Self {
        self.value = value;
        self
    }

    /// `With(direction: d)`: `Value` is kept.
    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    pub fn with_mode(mut self, mode: WorkMode) -> Self {
        self.mode = mode;
        self
    }

    // ---- state -----------------------------------------------------------------------------------

    pub fn value(&self) -> u64 {
        self.value
    }

    pub fn period(&self) -> u64 {
        self.period
    }

    pub fn frequency(&self) -> u64 {
        self.frequency
    }

    pub fn step(&self) -> u64 {
        self.step
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn mode(&self) -> WorkMode {
        self.mode
    }

    /// The not-yet-accounted fraction of an entry tick as `(numerator, denominator)`.
    pub fn residuum(&self) -> (u64, u64) {
        (self.residuum.num, self.residuum.den)
    }

    /// Entry ticks per nanosecond as a reduced fraction `(numerator, denominator)`.
    pub fn ratio(&self) -> (u64, u64) {
        (self.ratio.num, self.ratio.den)
    }

    /// `elapsed_ns * Ratio + residuum` split into whole entry ticks and the remaining fraction.
    fn entry_ticks(&self, ns: u64) -> (u128, Frac) {
        let (rn, rd) = (u128::from(self.ratio.num), u128::from(self.ratio.den));
        let (xn, xd) = (u128::from(self.residuum.num), u128::from(self.residuum.den));
        let den = rd / gcd(rd, xd) * xd;
        let num = u128::from(ns)
            .saturating_mul(rn)
            .saturating_mul(den / rd)
            .saturating_add(xn.saturating_mul(den / xd));
        (num / den, Frac::reduced(num % den, den))
    }

    /// Nanoseconds from the entry's current state to its next limit, rounded up (`u64::MAX`: never).
    /// This is the `emulatorTicksToLimit` of Renode's update handlers; `0` when the entry is already at
    /// its limit (the next update reaches it).
    pub fn ns_to_limit(&self) -> u64 {
        if !self.enabled || self.ratio.num == 0 {
            return u64::MAX;
        }
        let ticks = match self.direction {
            Direction::Descending => self.value,
            Direction::Ascending => self.period.saturating_sub(self.value),
        };
        let (rn, rd) = (u128::from(self.ratio.num), u128::from(self.ratio.den));
        let (xn, xd) = (u128::from(self.residuum.num), u128::from(self.residuum.den));
        // (ticks - residuum) / ratio = ((ticks * xd - xn) * rd) / (xd * rn)
        let remaining = u128::from(ticks).saturating_mul(xd).saturating_sub(xn);
        let num = remaining.saturating_mul(rd);
        let den = xd.saturating_mul(rn);
        let whole = num / den;
        let ceil = if num % den != 0 { whole + 1 } else { whole };
        if ceil >= u128::from(u64::MAX) {
            u64::MAX
        } else {
            ceil as u64
        }
    }

    /// Renode's update handler (`HandleDirection{Ascending,Descending}PositiveRatio`) for `ns` elapsed
    /// nanoseconds. A disabled entry does not change. On a limit the overshoot is discarded: `Value`
    /// restarts at 0 (ascending) / `Period` (descending) with a zero residuum, and a one-shot entry
    /// disables itself.
    pub fn advance(&mut self, ns: u64) -> Advance {
        if !self.enabled {
            return Advance { reached: false, to_limit: u64::MAX };
        }
        let (integer, fraction) = self.entry_ticks(ns);
        let reached = match self.direction {
            Direction::Descending => {
                let reached = integer >= u128::from(self.value);
                self.residuum = fraction;
                if reached {
                    self.value = self.period;
                    self.residuum = Frac::ZERO;
                } else {
                    self.value -= integer as u64;
                }
                reached
            }
            Direction::Ascending => {
                self.value = u128::from(self.value).saturating_add(integer).min(u128::from(u64::MAX)) as u64;
                self.residuum = fraction;
                let reached = self.value >= self.period;
                if reached {
                    self.value = 0;
                    self.residuum = Frac::ZERO;
                }
                reached
            }
        };
        if reached && self.mode == WorkMode::OneShot {
            self.enabled = false;
        }
        Advance { reached, to_limit: self.ns_to_limit() }
    }
}

/// A clock entry together with the time of its last update, for owners that keep their own timers
/// outside the machine registry (the CPU's SysTick and DWT cycle counter).
///
/// The value can be read for any time; limits are processed by [`LocalClock::run_until`], which visits
/// every limit at **its own time** (the ceil-nanosecond time), so the overshoot is dropped and the next
/// period starts from that rounded nanosecond exactly as in the machine registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalClock {
    entry: ClockEntry,
    last_update: Time,
}

impl LocalClock {
    /// `AddClockEntry` at `now` (the entry has not been checked for a zero-time limit yet; `exchange`
    /// and `run_until` do).
    pub fn new(entry: ClockEntry, now: Time) -> Self {
        Self { entry, last_update: now }
    }

    /// The entry as of the last update.
    pub fn entry(&self) -> &ClockEntry {
        &self.entry
    }

    pub fn last_update(&self) -> Time {
        self.last_update
    }

    /// The entry advanced to `now` without processing limits (`GetClockEntry`): readable at any time.
    pub fn entry_at(&self, now: Time) -> ClockEntry {
        let mut entry = self.entry;
        if now > self.last_update {
            entry.advance(now - self.last_update);
        }
        entry
    }

    pub fn value_at(&self, now: Time) -> u64 {
        self.entry_at(now).value()
    }

    /// Absolute time of the next limit (feeds a core's `next_internal_deadline`), `None` if there is none.
    pub fn next_limit(&self) -> Option<Time> {
        match self.entry.ns_to_limit() {
            u64::MAX => None,
            to_limit => match self.last_update.saturating_add(to_limit) {
                Time::MAX => None,
                time => Some(time),
            },
        }
    }

    /// Accounts the time from the last update to `now` in one step (no limit splitting): Renode's `Update`
    /// of this entry. An update with no elapsed time still reaches a limit the entry is already at
    /// (zero-period entries, state set to the limit). Prefer [`LocalClock::run_until`] when limits may lie
    /// in between.
    pub fn advance_to(&mut self, now: Time) -> Advance {
        let advance = self.entry.advance(now.saturating_sub(self.last_update));
        self.last_update = self.last_update.max(now);
        advance
    }

    /// Renode `BaseClockSource.Advance`: processes every limit up to and including `now` in order, each
    /// at its own time, calling `on_limit(limit_time)`, then accounts the remaining time up to `now`.
    /// Returns the number of limits. A limit exactly at `now` is processed.
    pub fn run_until(&mut self, now: Time, mut on_limit: impl FnMut(Time)) -> u32 {
        let mut count = 0;
        while let Some(limit) = self.next_limit() {
            if limit > now || count >= MAX_LIMITS_PER_RUN {
                break;
            }
            let advance = self.advance_to(limit.max(self.last_update));
            if !advance.reached {
                break; // cannot happen with ceil limits; never spin
            }
            count += 1;
            on_limit(limit);
        }
        if now > self.last_update {
            self.advance_to(now);
        }
        count
    }

    /// Renode `ExchangeClockEntryWith`: accounts the time up to `now`, applies `change`, then performs the
    /// zero-time update, so a state that is already at its limit (a `Value` write to the limit, a smaller
    /// period, a zero-delay one-shot) reaches it immediately. Returns `true` if the limit was reached by
    /// this call, in which case the entry state has already been reset and the handler is due now.
    pub fn exchange(&mut self, now: Time, change: impl FnOnce(ClockEntry) -> ClockEntry) -> bool {
        let mut reached = self.advance_to(now).reached;
        self.entry = change(self.entry);
        reached |= self.entry.advance(0).reached;
        reached
    }

    /// `exchange` with a replacement entry (`ExchangeClockEntryWith(handler, x => entry)`).
    pub fn replace(&mut self, now: Time, entry: ClockEntry) -> bool {
        self.exchange(now, |_| entry)
    }

    /// Zero-time update of a freshly added entry (the second `UpdateLimits()` of `AddClockEntry`).
    pub(crate) fn zero_update(&mut self) -> bool {
        self.entry.advance(0).reached
    }
}

/// Read access to clock entries at the current clock time. Implemented by [`Ctx`] and
/// [`crate::View`], so timer getters work in `read` (with a `Ctx`) and in `peek`/`summary` (with a `View`).
pub trait ClockRead {
    /// The machine's clock-source time.
    fn clock_now(&self) -> Time;

    /// Snapshot of the entry advanced to `clock_now()` (Renode `GetClockEntry`): the value is exact at
    /// any clock time. A stale or never-created id yields a disabled zero entry.
    fn clock_entry(&self, id: ClockId) -> ClockEntry;
}

/// Handle to a registry entry. Cheap to copy; `ClockId::NONE` never matches an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClockId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl ClockId {
    pub const NONE: ClockId = ClockId { index: u32::MAX, generation: 0 };

    pub fn is_none(self) -> bool {
        self == ClockId::NONE
    }
}

impl Default for ClockId {
    fn default() -> Self {
        ClockId::NONE
    }
}

// ---- registry data (operations live in `MachineCore`) ---------------------------------------------

pub(crate) struct ClockSlot {
    pub(crate) clock: LocalClock,
    pub(crate) owner: PeriphId,
    pub(crate) token: u64,
    /// Creation index: orders simultaneous limits.
    pub(crate) order: u64,
    /// `Some(scheduling time)` for `schedule_action` entries (removed after their handler ran).
    pub(crate) action_origin: Option<Time>,
    /// The queued limit event, or `EventId::NONE`.
    pub(crate) event: EventId,
    pub(crate) generation: u32,
    pub(crate) alive: bool,
}

#[derive(Default)]
pub(crate) struct ClockRegistry {
    pub(crate) slots: Vec<ClockSlot>,
    free: Vec<u32>,
    next_order: u64,
}

impl ClockRegistry {
    pub(crate) fn add(&mut self, clock: LocalClock, owner: PeriphId, token: u64, action_origin: Option<Time>) -> (u32, ClockId) {
        let order = self.next_order;
        self.next_order += 1;
        match self.free.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                let generation = slot.generation;
                *slot = ClockSlot { clock, owner, token, order, action_origin, event: EventId::NONE, generation, alive: true };
                (index, ClockId { index, generation })
            }
            None => {
                self.slots.push(ClockSlot { clock, owner, token, order, action_origin, event: EventId::NONE, generation: 0, alive: true });
                let index = (self.slots.len() - 1) as u32;
                (index, ClockId { index, generation: 0 })
            }
        }
    }

    pub(crate) fn get(&self, id: ClockId) -> Option<&ClockSlot> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.alive && slot.generation == id.generation).then_some(slot)
    }

    pub(crate) fn index_of(&self, id: ClockId) -> Option<u32> {
        self.get(id).map(|_| id.index)
    }

    pub(crate) fn id_of(&self, index: u32) -> ClockId {
        ClockId { index, generation: self.slots[index as usize].generation }
    }

    pub(crate) fn release(&mut self, index: u32) {
        let slot = &mut self.slots[index as usize];
        slot.alive = false;
        slot.event = EventId::NONE;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(index);
    }

    pub(crate) fn live(&self) -> usize {
        self.slots.iter().filter(|s| s.alive).count()
    }
}

// ---- LimitTimer ------------------------------------------------------------------------------------

/// Construction parameters of a [`LimitTimer`], with Renode's constructor defaults:
/// `limit = u64::MAX`, `Descending`, disabled, `Periodic`, event disabled, `auto_update` false, divider 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitTimerConfig {
    /// Input frequency in Hz (before the divider).
    pub frequency: u64,
    pub limit: u64,
    pub direction: Direction,
    pub enabled: bool,
    pub mode: WorkMode,
    pub event_enabled: bool,
    pub auto_update: bool,
    pub divider: u64,
}

impl LimitTimerConfig {
    pub const fn new(frequency: u64) -> Self {
        Self {
            frequency,
            limit: u64::MAX,
            direction: Direction::Descending,
            enabled: false,
            mode: WorkMode::Periodic,
            event_enabled: false,
            auto_update: false,
            divider: 1,
        }
    }
}

/// Port of Renode's `LimitTimer`: one clock entry plus the `rawInterrupt` / `eventEnabled` flags. The owner
/// forwards the entry's event token to [`LimitTimer::on_limit_reached`] and runs its own `LimitReached`
/// handler when that returns `true`. See `docs/framework.md` section 5.2.
#[derive(Clone, Debug)]
pub struct LimitTimer {
    cfg: LimitTimerConfig,
    token: u64,
    id: ClockId,
    frequency: u64,
    divider: u64,
    event_enabled: bool,
    auto_update: bool,
    raw_interrupt: bool,
}

impl LimitTimer {
    /// Panics (Renode: `ConstructionException`) if `limit`, `frequency` or `divider` is zero. `token` is
    /// the `on_event` token of the entry's limit event.
    pub fn new(cfg: LimitTimerConfig, token: u64) -> Self {
        assert!(cfg.limit > 0, "Limit must be greater than 0");
        assert!(cfg.divider > 0, "Divider must be greater than 0");
        assert!(cfg.frequency > 0, "Frequency must be greater than 0");
        Self {
            cfg,
            token,
            id: ClockId::NONE,
            frequency: cfg.frequency,
            divider: cfg.divider,
            event_enabled: cfg.event_enabled,
            auto_update: cfg.auto_update,
            raw_interrupt: false,
        }
    }

    fn initial_entry(&self) -> ClockEntry {
        ClockEntry::new(self.cfg.limit, self.cfg.frequency / self.cfg.divider, self.cfg.enabled, self.cfg.direction, self.cfg.mode)
    }

    /// Creates the registry entry (the constructor's `InternalReset` in Renode). Call it from
    /// `Peripheral::attach`: entry creation order decides same-instant handler order.
    pub fn attach(&mut self, ctx: &mut Ctx<'_>) {
        if self.id.is_none() {
            let entry = self.initial_entry();
            self.id = ctx.clock_add(entry, self.token);
        }
    }

    /// Renode `Reset()` / `InternalReset()`: the initial configuration is restored and the entry is
    /// replaced in place (its creation order is kept).
    pub fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.frequency = self.cfg.frequency;
        self.divider = self.cfg.divider;
        self.event_enabled = self.cfg.event_enabled;
        self.auto_update = self.cfg.auto_update;
        self.raw_interrupt = false;
        let entry = self.initial_entry();
        if self.id.is_none() {
            self.id = ctx.clock_add(entry, self.token);
        } else {
            ctx.clock_replace(self.id, entry);
        }
    }

    pub fn id(&self) -> ClockId {
        self.id
    }

    pub fn token(&self) -> u64 {
        self.token
    }

    // ---- reads ------------------------------------------------------------------------------------

    /// `Value`, exact at the current clock time.
    pub fn value(&self, clock: &dyn ClockRead) -> u64 {
        clock.clock_entry(self.id).value()
    }

    /// `Limit` (the entry's `Period`).
    pub fn limit(&self, clock: &dyn ClockRead) -> u64 {
        clock.clock_entry(self.id).period()
    }

    /// `GetValueAndLimit`.
    pub fn value_and_limit(&self, clock: &dyn ClockRead) -> (u64, u64) {
        let entry = clock.clock_entry(self.id);
        (entry.value(), entry.period())
    }

    pub fn enabled(&self, clock: &dyn ClockRead) -> bool {
        clock.clock_entry(self.id).enabled()
    }

    pub fn direction(&self, clock: &dyn ClockRead) -> Direction {
        clock.clock_entry(self.id).direction()
    }

    pub fn mode(&self, clock: &dyn ClockRead) -> WorkMode {
        clock.clock_entry(self.id).mode()
    }

    /// Input frequency (before the divider).
    pub fn frequency(&self) -> u64 {
        self.frequency
    }

    pub fn divider(&self) -> u64 {
        self.divider
    }

    /// `RawInterrupt`: set by every limit, cleared by `clear_interrupt`.
    pub fn raw_interrupt(&self) -> bool {
        self.raw_interrupt
    }

    pub fn event_enabled(&self) -> bool {
        self.event_enabled
    }

    /// `Interrupt`: `raw_interrupt && event_enabled`.
    pub fn interrupt(&self) -> bool {
        self.raw_interrupt && self.event_enabled
    }

    pub fn auto_update(&self) -> bool {
        self.auto_update
    }

    // ---- writes -----------------------------------------------------------------------------------

    /// `Enabled = v`. Requests a return, like every setter below except `set_mode` and the flag setters.
    pub fn set_enabled(&mut self, ctx: &mut Ctx<'_>, enabled: bool) {
        ctx.clock_exchange(self.id, |e| e.with_enabled(enabled));
        ctx.request_return();
    }

    /// `Value = v`. Renode throws `ArgumentException` when `v` exceeds the *initial* limit; here that is an
    /// error log (once) and the write is ignored. The residuum is kept.
    pub fn set_value(&mut self, ctx: &mut Ctx<'_>, value: u64) {
        if value > self.cfg.limit {
            ctx.error_once(self.token ^ 0x4C54_0001, format_args!("LimitTimer: value {value} cannot be larger than the limit {}", self.cfg.limit));
            return;
        }
        ctx.clock_exchange(self.id, |e| e.with_value(value));
        ctx.request_return();
    }

    /// `Limit = l`. With `auto_update` the value is also reset (0 ascending, `l` descending).
    pub fn set_limit(&mut self, ctx: &mut Ctx<'_>, limit: u64) {
        let auto_update = self.auto_update;
        ctx.clock_exchange(self.id, |e| {
            if auto_update {
                let value = if e.direction() == Direction::Ascending { 0 } else { limit };
                e.with_period(limit).with_value(value)
            } else {
                e.with_period(limit)
            }
        });
        ctx.request_return();
    }

    /// `Frequency = hz`: the entry runs at `hz / divider` (u64 integer division) and its residuum is cleared.
    pub fn set_frequency(&mut self, ctx: &mut Ctx<'_>, frequency: u64) {
        if frequency == 0 {
            ctx.error_once(self.token ^ 0x4C54_0002, format_args!("LimitTimer: frequency must be greater than 0"));
            return;
        }
        self.frequency = frequency;
        let effective = frequency / self.divider;
        ctx.clock_exchange(self.id, |e| e.with_frequency(effective));
        ctx.request_return();
    }

    /// `Divider = d`: a no-op when unchanged; otherwise the entry runs at `frequency / d` and its residuum is cleared.
    pub fn set_divider(&mut self, ctx: &mut Ctx<'_>, divider: u64) {
        if divider == self.divider {
            return;
        }
        if divider == 0 {
            ctx.error_once(self.token ^ 0x4C54_0003, format_args!("LimitTimer: divider must be greater than 0"));
            return;
        }
        self.divider = divider;
        let effective = self.frequency / divider;
        ctx.clock_exchange(self.id, |e| e.with_frequency(effective));
        ctx.request_return();
    }

    pub fn set_direction(&mut self, ctx: &mut Ctx<'_>, direction: Direction) {
        ctx.clock_exchange(self.id, |e| e.with_direction(direction));
        ctx.request_return();
    }

    /// `Mode = m`. Unlike the other setters it does **not** request a return (Renode parity).
    pub fn set_mode(&mut self, ctx: &mut Ctx<'_>, mode: WorkMode) {
        ctx.clock_exchange(self.id, |e| e.with_mode(mode));
    }

    /// `ResetValue()`: 0 (ascending) or the limit (descending); the residuum is kept.
    pub fn reset_value(&mut self, ctx: &mut Ctx<'_>) {
        ctx.clock_exchange(self.id, |e| match e.direction() {
            Direction::Ascending => e.with_value(0),
            Direction::Descending => e.with_value(e.period()),
        });
        ctx.request_return();
    }

    /// `Increment(by)`: `Value = (Value + by) % Limit`, returns `(Value + by) / Limit`.
    pub fn increment(&mut self, ctx: &mut Ctx<'_>, by: u64) -> u64 {
        let limit = u128::from(self.limit(ctx));
        if limit == 0 {
            return 0;
        }
        let total = u128::from(self.value(ctx)) + u128::from(by);
        self.set_value(ctx, (total % limit) as u64);
        (total / limit) as u64
    }

    /// `Decrement(by)`: wraps below zero through the limit, returns how many times it wrapped.
    pub fn decrement(&mut self, ctx: &mut Ctx<'_>, by: u64) -> u64 {
        let limit = u128::from(self.limit(ctx));
        if limit == 0 {
            return 0;
        }
        let value = u128::from(self.value(ctx));
        let by = u128::from(by);
        let wrapped = ((limit - 1).saturating_sub(value) + by) / limit;
        self.set_value(ctx, (value + limit * wrapped).saturating_sub(by) as u64);
        wrapped as u64
    }

    /// `EventEnabled = v` (no clock effect).
    pub fn set_event_enabled(&mut self, enabled: bool) {
        self.event_enabled = enabled;
    }

    /// `AutoUpdate = v` (no clock effect).
    pub fn set_auto_update(&mut self, auto_update: bool) {
        self.auto_update = auto_update;
    }

    /// `ClearInterrupt()`.
    pub fn clear_interrupt(&mut self) {
        self.raw_interrupt = false;
    }

    /// `OnLimitReached()`: call it from `on_event` for this timer's token. Sets `RawInterrupt` and returns
    /// `true` when the event is enabled, i.e. when Renode would invoke `LimitReached`.
    pub fn on_limit_reached(&mut self) -> bool {
        self.raw_interrupt = true;
        self.event_enabled
    }
}

// ---- ManagedThread -----------------------------------------------------------------------------------

/// Port of `machine.ObtainManagedThread(action, frequency | period)`: a periodic ascending entry (period 1
/// at `frequency`, or `period` ns at 1e9 Hz) created **disabled**. The owner runs the thread body when
/// `on_event` receives the thread's token. See `docs/framework.md` section 5.3.
#[derive(Clone, Debug)]
pub struct ManagedThread {
    id: ClockId,
    token: u64,
    period: u64,
    frequency: u64,
}

impl ManagedThread {
    /// A thread that fires `frequency` times per second (`ObtainManagedThread(action, hz)`); panics on 0.
    pub fn new(frequency: u64, token: u64) -> Self {
        assert!(frequency > 0, "Frequency must be higher than zero");
        Self { id: ClockId::NONE, token, period: 1, frequency }
    }

    /// A thread with a fixed period in nanoseconds (`ObtainManagedThread(action, TimeInterval)`).
    pub fn with_period(period: Time, token: u64) -> Self {
        Self { id: ClockId::NONE, token, period, frequency: TICKS_PER_SECOND }
    }

    /// Creates the (disabled) entry; call it from `Peripheral::attach`.
    pub fn attach(&mut self, ctx: &mut Ctx<'_>) {
        if self.id.is_none() {
            let entry = ClockEntry::new(self.period, self.frequency, false, Direction::Ascending, WorkMode::Periodic);
            self.id = ctx.clock_add(entry, self.token);
        }
    }

    pub fn id(&self) -> ClockId {
        self.id
    }

    pub fn token(&self) -> u64 {
        self.token
    }

    /// `Start()`: enables the entry keeping `Value` and the residuum, so the first firing is
    /// `ceil(1e9 / f)` ns later (less if a stopped thread resumes a partial period). No return request.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.clock_exchange(self.id, |e| e.with_enabled(true));
    }

    /// `Stop()`: disables the entry, keeping `Value` and the residuum.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.clock_exchange(self.id, |e| e.with_enabled(false));
    }

    /// `Restart()`: enables the entry and resets `Value` to 0 (the residuum is kept).
    pub fn restart(&mut self, ctx: &mut Ctx<'_>) {
        ctx.clock_exchange(self.id, |e| {
            let value = if e.direction() == Direction::Ascending { 0 } else { e.period() };
            e.with_enabled(true).with_value(value)
        });
    }

    /// `StartDelayed(delay)`: a `schedule_action(delay, action_token)`; when `on_event(action_token, ..)`
    /// arrives call `start(ctx)` and then run the thread body once ("the first action runs precisely at
    /// the specified time").
    pub fn start_delayed(&mut self, ctx: &mut Ctx<'_>, delay: Time, action_token: u64) -> ClockId {
        ctx.schedule_action(delay, action_token)
    }

    /// `Frequency = hz`: clears the residuum.
    pub fn set_frequency(&mut self, ctx: &mut Ctx<'_>, frequency: u64) {
        ctx.clock_exchange(self.id, |e| e.with_frequency(frequency));
    }

    /// `Period = ns`: period `ns` at 1e9 Hz (clears the residuum).
    pub fn set_period(&mut self, ctx: &mut Ctx<'_>, period: Time) {
        ctx.clock_exchange(self.id, |e| e.with_period(period).with_frequency(TICKS_PER_SECOND));
    }

    pub fn frequency(&self, clock: &dyn ClockRead) -> u64 {
        clock.clock_entry(self.id).frequency()
    }

    /// `Period`: `entry.period * 1e9 / entry.frequency` nanoseconds.
    pub fn period(&self, clock: &dyn ClockRead) -> Time {
        let entry = clock.clock_entry(self.id);
        if entry.frequency() == 0 {
            return 0;
        }
        let ns = u128::from(entry.period()) * u128::from(TICKS_PER_SECOND) / u128::from(entry.frequency());
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    pub fn enabled(&self, clock: &dyn ClockRead) -> bool {
        clock.clock_entry(self.id).enabled()
    }

    /// `Dispose()`: removes the entry.
    pub fn dispose(&mut self, ctx: &mut Ctx<'_>) {
        ctx.clock_remove(self.id);
        self.id = ClockId::NONE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(period: u64, hz: u64, dir: Direction) -> ClockEntry {
        ClockEntry::new(period, hz, true, dir, WorkMode::Periodic)
    }

    #[test]
    fn ratio_is_reduced_and_zero_is_zero_over_one() {
        assert_eq!(entry(1, 80_000_000, Direction::Ascending).ratio(), (2, 25));
        assert_eq!(entry(1, 1_000_000, Direction::Ascending).ratio(), (1, 1000));
        assert_eq!(entry(1, 120, Direction::Ascending).ratio(), (3, 25_000_000));
        assert_eq!(entry(1, 0, Direction::Ascending).ratio(), (0, 1));
        assert_eq!(entry(1, 2_000_000_000, Direction::Ascending).ratio(), (2, 1));
    }

    #[test]
    fn constructor_starts_ascending_at_zero_and_descending_at_the_period() {
        assert_eq!(entry(77, 1000, Direction::Ascending).value(), 0);
        assert_eq!(entry(77, 1000, Direction::Descending).value(), 77);
        assert_eq!(entry(77, 1000, Direction::Ascending).residuum(), (0, 1));
    }

    #[test]
    fn ascending_time_to_limit_is_rounded_up() {
        // 1001 ticks at 80 MHz = 12512.5 ns -> 12513; 999 ticks at 1 MHz = 999000 ns exactly.
        assert_eq!(entry(1001, 80_000_000, Direction::Ascending).ns_to_limit(), 12_513);
        assert_eq!(entry(999, 1_000_000, Direction::Ascending).ns_to_limit(), 999_000);
        // ManagedThread: period 1 at f.
        assert_eq!(entry(1, 10_000, Direction::Ascending).ns_to_limit(), 100_000);
        assert_eq!(entry(1, 1_000, Direction::Ascending).ns_to_limit(), 1_000_000);
        assert_eq!(entry(1, 120, Direction::Ascending).ns_to_limit(), 8_333_334);
        // 80 MHz / 3 by integer division.
        assert_eq!(entry(1001, 80_000_000 / 3, Direction::Ascending).ns_to_limit(), 37_538);
    }

    #[test]
    fn descending_time_to_limit_is_rounded_up() {
        // SysTick reload 79999 at 80 MHz = 999987.5 ns.
        assert_eq!(entry(79_999, 80_000_000, Direction::Descending).ns_to_limit(), 999_988);
    }

    #[test]
    fn ascending_overshoot_is_discarded_at_the_limit() {
        let mut e = entry(1001, 80_000_000, Direction::Ascending);
        let a = e.advance(12_513);
        assert!(a.reached);
        assert_eq!((e.value(), e.residuum()), (0, (0, 1)), "restarts from the rounded ns with no residuum");
        assert_eq!(a.to_limit, 12_513, "every period is the same ceil value");
    }

    #[test]
    fn residuum_accumulates_exactly_between_updates() {
        // 3 MHz-ish ratio 3/1000: 100 ns = 0.3 tick.
        let mut e = entry(10, 3_000_000, Direction::Ascending);
        assert_eq!(e.ratio(), (3, 1000));
        e.advance(100);
        assert_eq!((e.value(), e.residuum()), (0, (3, 10)));
        e.advance(100);
        assert_eq!((e.value(), e.residuum()), (0, (3, 5)));
        e.advance(100);
        assert_eq!((e.value(), e.residuum()), (0, (9, 10)));
        e.advance(100);
        assert_eq!((e.value(), e.residuum()), (1, (1, 5)), "0.9 + 0.3 = 1.2 ticks");
        // Splitting an interval never changes the result (no limit crossed).
        let mut a = entry(1_000_000, 3_000_000, Direction::Ascending);
        let mut b = a;
        a.advance(1_234_567);
        for part in [1, 10, 100, 1000, 200_000, 1_033_456] {
            b.advance(part);
        }
        assert_eq!((a.value(), a.residuum()), (b.value(), b.residuum()));
    }

    #[test]
    fn descending_reaches_when_whole_ticks_cover_the_value() {
        let mut e = entry(5, 1_000_000_000, Direction::Descending); // 1 tick per ns
        assert!(!e.advance(4).reached);
        assert_eq!(e.value(), 1);
        let a = e.advance(1);
        assert!(a.reached);
        assert_eq!(e.value(), 5, "reloaded with the period");
        assert_eq!(a.to_limit, 5);
        // A descending entry at value 0 reaches at zero elapsed time.
        let mut z = entry(5, 1000, Direction::Descending).with_value(0);
        assert!(z.advance(0).reached);
        assert_eq!(z.value(), 5);
    }

    #[test]
    fn one_shot_disables_itself_and_never_fires_again() {
        let mut e = ClockEntry::new(3, 1_000_000_000, true, Direction::Ascending, WorkMode::OneShot);
        assert!(e.advance(3).reached);
        assert!(!e.enabled());
        assert_eq!(e.ns_to_limit(), u64::MAX);
        assert!(!e.advance(1_000).reached);
        assert_eq!(e.value(), 0);
    }

    #[test]
    fn with_family_keeps_or_clears_the_residuum_like_renode() {
        let mut e = entry(10, 3_000_000, Direction::Ascending);
        e.advance(100);
        assert_eq!(e.residuum(), (3, 10));
        assert_eq!(e.with_value(5).residuum(), (3, 10), "Value writes keep the residuum");
        assert_eq!(e.with_value(5).value(), 5);
        assert_eq!(e.with_period(20).residuum(), (3, 10));
        assert_eq!(e.with_direction(Direction::Descending).value(), 0, "direction change keeps Value");
        assert_eq!(e.with_enabled(false).residuum(), (3, 10));
        assert_eq!(e.with_frequency(3_000_000).residuum(), (0, 1), "frequency changes clear it, even to the same value");
    }

    #[test]
    fn zero_frequency_never_reaches() {
        let mut e = entry(10, 0, Direction::Ascending);
        assert_eq!(e.ns_to_limit(), u64::MAX);
        let a = e.advance(1_000_000);
        assert!(!a.reached);
        assert_eq!(a.to_limit, u64::MAX);
    }

    #[test]
    fn huge_values_do_not_overflow() {
        let mut e = entry(u64::MAX, 1_000_000_000, Direction::Ascending);
        assert_eq!(e.ns_to_limit(), u64::MAX, "beyond the representable time: never");
        e.advance(1_000);
        assert_eq!(e.value(), 1_000);
        e.advance(u64::MAX); // saturates instead of overflowing; the limit is reached
        assert_eq!(e.value(), 0);
        let big = entry(u64::MAX - 1, 1_000_000_000, Direction::Descending);
        assert!(big.ns_to_limit() >= u64::MAX - 1);
    }

    #[test]
    fn local_clock_run_until_visits_each_limit_at_its_own_time() {
        // SysTick-like: descending, reload 79999 at 80 MHz.
        let mut c = LocalClock::new(entry(79_999, 80_000_000, Direction::Descending), 0);
        let mut times = Vec::new();
        let n = c.run_until(5_000_000, |t| times.push(t));
        assert_eq!(n, 5);
        assert_eq!(times, [999_988, 1_999_976, 2_999_964, 3_999_952, 4_999_940]);
        assert_eq!(c.last_update(), 5_000_000);
        assert_eq!(c.next_limit(), Some(4_999_940 + 999_988));
        // The value is readable at any time without events.
        let mid = c.value_at(5_000_000 + 500_000);
        assert!(mid > 0 && mid < 79_999);
    }

    #[test]
    fn local_clock_exchange_applies_the_zero_time_update() {
        let mut c = LocalClock::new(entry(100, 1_000_000, Direction::Ascending), 0);
        assert!(!c.exchange(10_000, |e| e.with_value(5)), "plain value write");
        assert!(c.exchange(10_000, |e| e.with_value(100)), "value at the limit reaches immediately");
        assert_eq!(c.entry().value(), 0, "state already reset");
        assert!(c.exchange(10_000, |e| e.with_period(0)), "a zero period is at its limit");
        let mut one = LocalClock::new(ClockEntry::new(0, 1_000_000_000, true, Direction::Ascending, WorkMode::OneShot), 50);
        assert!(one.zero_update(), "ScheduleAction(0) fires at once");
        assert!(!one.entry().enabled());
    }

    /// Brute-force oracle: Renode's update handler applied after every single nanosecond, with the fraction kept
    /// over the ratio's denominator. The lazily evaluated entry must produce the same limit times.
    fn per_nanosecond_limits(period: u64, hz: u64, dir: Direction, run_ns: u64) -> Vec<u64> {
        let g = gcd(u128::from(hz), u128::from(crate::TICKS_PER_SECOND));
        let (rn, rd) = ((u128::from(hz) / g) as u64, (u128::from(crate::TICKS_PER_SECOND) / g) as u64);
        let mut value = if dir == Direction::Ascending { 0 } else { period };
        let mut residuum = 0u64; // numerator over rd
        let mut limits = Vec::new();
        for t in 1..=run_ns {
            let total = u128::from(residuum) + u128::from(rn);
            let integer = (total / u128::from(rd)) as u64;
            residuum = (total % u128::from(rd)) as u64;
            let reached = match dir {
                Direction::Ascending => {
                    value += integer;
                    value >= period
                }
                Direction::Descending => {
                    if integer >= value {
                        true
                    } else {
                        value -= integer;
                        false
                    }
                }
            };
            if reached {
                limits.push(t);
                value = if dir == Direction::Ascending { 0 } else { period };
                residuum = 0;
            }
        }
        limits
    }

    #[test]
    fn lazy_entries_match_the_per_nanosecond_oracle() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state >> 33
        };
        let mut checked = 0;
        for _ in 0..300 {
            let hz = match next() % 6 {
                0 => 1 + next() % 1_000,
                1 => 1_000 + next() % 1_000_000,
                2 => 32_768,
                3 => 80_000_000 / (1 + next() % 8),
                4 => 1_000_000_000 / (1 + next() % 100),
                _ => 1 + next() % 2_000_000_000,
            };
            let period = 1 + next() % 3_000;
            let dir = if next() % 2 == 0 { Direction::Ascending } else { Direction::Descending };
            let run_ns = 60_000;
            let oracle = per_nanosecond_limits(period, hz, dir, run_ns);
            let mut local = LocalClock::new(entry(period, hz, dir), 0);
            let mut limits = Vec::new();
            local.run_until(run_ns, |t| limits.push(t));
            assert_eq!(limits, oracle, "period {period}, {hz} Hz, {dir:?}");
            // Splitting the run at an arbitrary time (a read in the middle) changes nothing.
            let split = 1 + next() % (run_ns - 1);
            let mut halves = LocalClock::new(entry(period, hz, dir), 0);
            let mut halved = Vec::new();
            halves.run_until(split, |t| halved.push(t));
            let _ = halves.value_at(split + 7); // reads never disturb the state
            halves.run_until(run_ns, |t| halved.push(t));
            assert_eq!(halved, oracle, "split at {split}: period {period}, {hz} Hz, {dir:?}");
            checked += oracle.len();
        }
        assert!(checked > 1_000, "the random cases exercised many limits ({checked})");
    }

    #[test]
    fn local_clock_limit_exactly_at_now_is_processed() {
        let mut c = LocalClock::new(entry(1, 1_000_000, Direction::Ascending), 0);
        let mut times = Vec::new();
        c.run_until(1000, |t| times.push(t));
        assert_eq!(times, [1000]);
    }
}
