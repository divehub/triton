// SysTick: ported from Renode 1.17.0 src/Emulator/Cores/Arm-M/NVIC.cs (class SysTick).
// DWT cycle counter: ported from Renode 1.17.0 src/Emulator/Cores/Arm-M/DWT.cs.
// Both are Renode `LimitTimer`s; their clock entries are `emu_core::clock` (ported from
// Renode ClockEntry.cs / BaseClockSource.cs / LimitTimer.cs).
// (MIT License, Copyright (c) Antmicro.)
//
//! Core-internal timers with Renode `ClockEntry` semantics.
//!
//! SysTick and the DWT cycle counter are `LimitTimer`s on the machine clock source. They are
//! `emu_core::clock::LocalClock`s: an entry counting `elapsed_ns * Ratio + ValueResiduum` timer
//! cycles with an exact fraction, whose limits fire at the ceil-nanosecond tick, restarting from
//! that tick with the overshoot discarded. A timer only knows the clock time up to which it has
//! been advanced (`at`): Renode's clock source advances only when the CPU reports progress, so
//! register writes made in the middle of a chunk take effect at the chunk start (or the last
//! explicit time sync), see `Cpu::advance_clock`.
//!
//! Every setter reports in [`Effects`] whether Renode's `LimitTimer` setter ran (it calls
//! `RequestReturn()`, which ends the CPU chunk at the end of the current translation block) and
//! whether the SysTick exception has to be pended (a limit reached by the zero-time update that
//! follows an entry change, for example enabling a counter whose value is 0).

use emu_core::clock::{ClockEntry, Direction, LocalClock, WorkMode};
use emu_core::Time;
use std::cell::Cell;

const SYSTICK_MAX: u32 = 0x00FF_FFFF;

/// What a register operation did to a timer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// A `LimitTimer` setter ran (`RequestReturn`).
    pub touched: bool,
    /// The SysTick exception must be pended (limit reached with TICKINT set).
    pub pend: bool,
}

fn systick_entry(hz: u64) -> ClockEntry {
    ClockEntry::new(SYSTICK_MAX as u64, hz, false, Direction::Descending, WorkMode::Periodic)
}

/// SysTick (SYST_CSR/RVR/CVR) with Renode's semantics: the counter is reloaded with RELOAD when
/// it reaches zero, so the period is RELOAD cycles; writing CVR loads RELOAD (not zero); RELOAD = 0
/// stops the counter.
#[derive(Clone, Debug)]
pub struct SysTick {
    clock: LocalClock,
    /// CSR.ENABLE as written (Renode `systickEnabled`); the entry itself may be stopped
    /// (RELOAD = 0, deep sleep).
    pub enabled: bool,
    pub tickint: bool,
    pub countflag: bool,
    reload: u32,
    hz: u64,
    /// `clock.next_limit()` as last computed; every change of the clock entry or of its update time clears it
    /// (all of them go through `clock_mut`). The deadline is asked for several times per chunk.
    deadline_memo: Cell<Option<Option<Time>>>,
}

impl SysTick {
    pub fn new(hz: u64) -> Self {
        SysTick {
            clock: LocalClock::new(systick_entry(hz), 0),
            enabled: false,
            tickint: false,
            countflag: false,
            reload: 0,
            hz,
            deadline_memo: Cell::new(None),
        }
    }

    /// The clock for a change: forgets the memoized deadline.
    #[inline]
    fn clock_mut(&mut self) -> &mut LocalClock {
        self.deadline_memo.set(None);
        &mut self.clock
    }

    /// Core reset: a fresh stopped entry at clock time `at`.
    pub fn reset(&mut self, at: Time) {
        self.clock = LocalClock::new(systick_entry(self.hz), at);
        self.deadline_memo.set(None);
        self.enabled = false;
        self.tickint = false;
        self.countflag = false;
        self.reload = 0;
    }

    pub fn reload(&self) -> u32 {
        self.reload
    }

    /// The underlying counter is running (it can be stopped while `enabled`: RELOAD = 0, deep sleep).
    #[cfg(test)]
    pub fn is_counting(&self) -> bool {
        self.clock.entry().enabled()
    }

    /// Current counter value as of the last clock advance (CVR read value).
    pub fn value(&self) -> u32 {
        self.clock.entry().value() as u32
    }

    /// Clock time up to which the timer has been advanced.
    pub fn at(&self) -> Time {
        self.clock.last_update()
    }

    /// Absolute time of the next expiry (`ceil` tick), if the counter is running.
    pub fn deadline(&self) -> Option<Time> {
        if let Some(memo) = self.deadline_memo.get() {
            return memo;
        }
        let deadline = self.clock.next_limit();
        self.deadline_memo.set(Some(deadline));
        deadline
    }

    /// The `LimitReached` handler of the NVIC SysTick class. Returns whether the exception is pended.
    fn limit_reached(&mut self, at: Time) -> bool {
        self.countflag = true;
        let pend = self.tickint;
        if self.reload == 0 {
            // "If the timer is running and the reload value is 0, this has the effect of
            // disabling the counter on the expiration."
            self.clock_mut().exchange(at, |e| e.with_enabled(false));
        } else {
            let reload = self.reload as u64;
            self.clock_mut().exchange(at, |e| e.with_value(reload));
        }
        pend
    }

    /// Advances the clock to `t` (`BaseClockSource.Advance`: split at every limit so each event
    /// fires at its own tick). Returns true when an expiry requested the SysTick exception
    /// (TICKINT set at the time of the expiry). COUNTFLAG is set by every expiry.
    pub fn advance_to(&mut self, t: Time) -> bool {
        let mut pend = false;
        while let Some(limit) = self.deadline() {
            if limit > t {
                break;
            }
            let from = limit.max(self.clock.last_update());
            if !self.clock_mut().advance_to_reached(from) {
                break;
            }
            pend |= self.limit_reached(limit);
        }
        if t > self.clock.last_update() {
            self.clock_mut().advance_to_reached(t);
        }
        pend
    }

    /// `LimitTimer` entry change at the current clock time, followed by Renode's zero-time update.
    fn exchange(&mut self, change: impl FnOnce(ClockEntry) -> ClockEntry) -> bool {
        let at = self.clock.last_update();
        let reached = self.clock_mut().exchange(at, change);
        reached && self.limit_reached(at)
    }

    /// CSR.ENABLE write (`SysTick.Enabled` setter).
    pub fn set_enable(&mut self, v: bool) -> Effects {
        if self.enabled == v {
            return Effects::default();
        }
        self.enabled = v;
        if v && self.reload == 0 {
            // Enabled but "won't be started as long as the reload value is zero".
            return Effects::default();
        }
        let pend = self.exchange(|e| e.with_enabled(v));
        Effects { touched: true, pend }
    }

    /// RVR write (`SysTick.Reload` setter).
    pub fn set_reload(&mut self, v: u32) -> Effects {
        let v = v & SYSTICK_MAX;
        let mut fx = Effects::default();
        if self.enabled && self.reload == 0 && v != 0 && !self.clock.entry().enabled() {
            // Resume the counter blocked by RELOAD == 0.
            fx.pend |= self.exchange(|e| e.with_value(v as u64));
            fx.pend |= self.exchange(|e| e.with_enabled(true));
            fx.touched = true;
        }
        self.reload = v;
        fx
    }

    /// CVR write (`UpdateSystickValue`): loads RELOAD (the residuum is kept) and clears COUNTFLAG.
    /// Always touches the `LimitTimer`.
    pub fn write_value(&mut self) -> Effects {
        let mut fx = Effects { touched: true, pend: false };
        if self.reload == 0 {
            fx.pend |= self.exchange(|e| e.with_enabled(false));
        }
        let reload = self.reload as u64;
        fx.pend |= self.exchange(|e| e.with_value(reload));
        self.countflag = false;
        fx
    }

    /// CALIB register value (TENMS, SKEW, NOREF).
    pub fn calib(&self) -> u32 {
        let tenms = SYSTICK_MAX & (self.hz / 100) as u32;
        let skew = self.hz % 100 != 0;
        0x8000_0000 | ((skew as u32) << 30) | tenms
    }
}

fn dwt_entry(hz: u64) -> ClockEntry {
    ClockEntry::new(u64::MAX, hz, false, Direction::Ascending, WorkMode::Periodic)
}

/// DWT cycle counter (CYCCNT): a 64-bit ascending `LimitTimer` of limit `u64::MAX` whose low 32
/// bits are visible; it never raises an event.
#[derive(Clone, Debug)]
pub struct Dwt {
    clock: LocalClock,
    hz: u64,
}

impl Dwt {
    pub fn new(hz: u64) -> Self {
        Dwt { clock: LocalClock::new(dwt_entry(hz), 0), hz }
    }

    pub fn reset(&mut self, at: Time) {
        self.clock = LocalClock::new(dwt_entry(self.hz), at);
    }

    pub fn enabled(&self) -> bool {
        self.clock.entry().enabled()
    }

    /// CYCCNT as of the last clock advance.
    pub fn cyccnt(&self) -> u32 {
        self.clock.entry().value() as u32
    }

    pub fn advance_to(&mut self, t: Time) {
        if t > self.clock.last_update() {
            self.clock.advance_to_reached(t);
        }
    }

    /// CTRL.CYCCNTENA write (the register callback runs on every write, changed or not).
    pub fn set_enabled(&mut self, on: bool) {
        let at = self.clock.last_update();
        self.clock.exchange(at, |e| e.with_enabled(on));
    }

    /// CYCCNT write (the residuum is kept).
    pub fn set_cyccnt(&mut self, v: u32) {
        let at = self.clock.last_update();
        self.clock.exchange(at, |e| e.with_value(v as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn systick() -> SysTick {
        SysTick::new(80_000_000)
    }

    #[test]
    fn time_base_is_one_nanosecond() {
        // The Renode-exact rounding below needs the 1 ns time base.
        assert_eq!(emu_core::TICKS_PER_SECOND, 1_000_000_000);
    }

    #[test]
    fn systick_reload_79999_expires_at_ceil_999988_ns_then_every_999988_ns() {
        let mut s = systick();
        s.set_reload(79_999);
        s.write_value();
        s.tickint = true;
        s.set_enable(true);
        assert_eq!(s.deadline(), Some(999_988));
        assert!(!s.advance_to(999_987));
        assert!(s.advance_to(999_988));
        assert!(s.countflag);
        // The entry restarts from the ceil-ns expiry time with a zero residuum.
        assert_eq!(s.deadline(), Some(999_988 * 2));
        let mut t = 999_988;
        for _ in 0..40 {
            t += 999_988;
            assert_eq!(s.deadline(), Some(t));
            assert!(s.advance_to(t));
        }
    }

    #[test]
    fn systick_value_and_residuum_follow_the_exact_fraction() {
        let mut s = systick();
        s.set_reload(79_999);
        s.write_value();
        s.set_enable(true);
        // 140 ns = 11.2 cycles: 11 whole cycles, a fifth of a cycle left over.
        s.advance_to(140);
        assert_eq!(s.value(), 79_999 - 11);
        // The remaining 79 988 - 0.2 cycles take 999 847.5 ns -> 999 848 ns after t = 140.
        assert_eq!(s.deadline(), Some(140 + 999_848));
        // 1 ns more: 0.08 cycles added to the 0.2 residuum -> still 11 whole cycles.
        s.advance_to(141);
        assert_eq!(s.value(), 79_999 - 11);
        s.advance_to(150);
        assert_eq!(s.value(), 79_999 - 12);
    }

    #[test]
    fn systick_cvr_write_loads_reload_and_keeps_the_residuum() {
        let mut s = systick();
        s.set_reload(1000);
        s.write_value();
        s.set_enable(true);
        // 3 ns = 0.24 cycle.
        s.advance_to(3);
        assert_eq!(s.value(), 1000);
        s.write_value();
        assert_eq!(s.value(), 1000);
        // Expiry: (1000 - 0.24) cycles = 999.76 cycles = 12 497 ns after t = 3.
        assert_eq!(s.deadline(), Some(3 + 12_497));
        assert!(!s.countflag);
    }

    #[test]
    fn systick_expiry_without_tickint_only_sets_countflag() {
        let mut s = systick();
        s.set_reload(100);
        s.write_value();
        s.set_enable(true);
        assert!(!s.advance_to(1250));
        assert!(s.countflag);
        assert_eq!(s.deadline(), Some(2500));
    }

    #[test]
    fn systick_several_expiries_in_one_advance() {
        let mut s = systick();
        s.set_reload(100);
        s.tickint = true;
        s.write_value();
        s.set_enable(true);
        // Period 1250 ns exactly: five expiries in 6250 ns.
        assert!(s.advance_to(6250));
        assert_eq!(s.deadline(), Some(7500));
    }

    #[test]
    fn systick_reload_zero_stops_the_counter() {
        let mut s = systick();
        // Enabled with RELOAD == 0: ENABLE reads set but the counter does not run.
        assert!(!s.set_enable(true).touched);
        assert!(s.enabled && !s.is_counting());
        assert_eq!(s.deadline(), None);
        // Writing a non-zero RELOAD resumes the counter with Value = RELOAD.
        s.advance_to(1000);
        assert!(s.set_reload(50).touched);
        assert!(s.is_counting());
        assert_eq!(s.deadline(), Some(1000 + 625));
        // RELOAD cleared while running: the counter stops at its next expiry.
        s.set_reload(0);
        s.advance_to(1000 + 625);
        assert!(!s.is_counting());
        assert!(s.countflag);
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn systick_starts_from_the_reset_value_without_a_cvr_write() {
        let mut s = systick();
        s.set_reload(1000);
        s.set_enable(true);
        // Value = 0xFFFFFF: 16 777 215 cycles = 209 715 187.5 ns -> ceil.
        assert_eq!(s.deadline(), Some(209_715_188));
    }

    #[test]
    fn systick_enabled_with_a_zero_value_expires_immediately() {
        // FreeRTOS order: CSR = 0, CVR = 0 (RELOAD is still 0, so the counter stays at 0), RVR = n,
        // CSR = ENABLE | TICKINT. Renode's zero-time update then reaches the limit at once.
        let mut s = systick();
        s.write_value();
        assert_eq!(s.value(), 0);
        s.set_reload(79_999);
        s.tickint = true;
        let fx = s.set_enable(true);
        assert!(fx.touched && fx.pend, "{fx:?}");
        assert!(s.countflag);
        assert_eq!(s.value(), 79_999, "reloaded by the handler");
        assert_eq!(s.deadline(), Some(999_988));
    }

    #[test]
    fn systick_disable_freezes_the_value() {
        let mut s = systick();
        s.set_reload(1000);
        s.write_value();
        s.set_enable(true);
        s.advance_to(1000);
        assert_eq!(s.value(), 920);
        assert!(s.set_enable(false).touched);
        s.advance_to(5000);
        assert_eq!(s.value(), 920);
        assert!(s.set_enable(true).touched);
        s.advance_to(5000 + 1250);
        // 1250 ns is exactly 100 cycles: 920 - 100.
        assert_eq!(s.value(), 820);
    }

    #[test]
    fn systick_calib() {
        let s = systick();
        assert_eq!(s.calib(), 0x8000_0000 | 800_000);
    }

    #[test]
    fn setters_report_whether_the_limit_timer_was_touched() {
        let mut s = systick();
        assert!(!s.set_enable(false).touched, "no change: the Enabled setter returns early");
        assert!(!s.set_enable(true).touched, "enabled with RELOAD == 0 does not touch the timer");
        assert!(s.write_value().touched, "CVR writes always touch the timer");
        assert!(!s.set_reload(0).touched);
        assert!(s.set_reload(10).touched, "resumes the blocked counter");
    }

    #[test]
    fn dwt_counts_80_mhz_cycles_from_the_clock_time() {
        let mut d = Dwt::new(80_000_000);
        d.advance_to(1000);
        assert_eq!(d.cyccnt(), 0);
        d.set_enabled(true);
        d.advance_to(1_001_000);
        assert_eq!(d.cyccnt(), 80_000);
        // 1 999 990 ns = 159 999.2 cycles; the 0.2 residuum is retained, not rounded.
        d.set_enabled(false);
        d.set_cyccnt(0);
        d.set_enabled(true);
        d.advance_to(1_001_000 + 1_999_990);
        assert_eq!(d.cyccnt(), 159_999);
    }

    #[test]
    fn dwt_wraps_in_32_bits_but_keeps_counting_in_64() {
        let mut d = Dwt::new(80_000_000);
        d.set_enabled(true);
        d.set_cyccnt(0xFFFF_FFFE);
        d.advance_to(125); // 10 cycles
        assert_eq!(d.cyccnt(), 8);
        // A 32-bit write replaces the whole value.
        d.set_cyccnt(5);
        assert_eq!(d.cyccnt(), 5);
    }

    #[test]
    fn dwt_disabled_does_not_count() {
        let mut d = Dwt::new(80_000_000);
        d.advance_to(1_000_000);
        assert_eq!(d.cyccnt(), 0);
        assert!(!d.enabled());
    }
}
