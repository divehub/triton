//! Shared primitives for the NGC Rust/WebAssembly emulator.
//!
//! Virtual time is an unsigned count of **nanoseconds**, the resolution of Renode's
//! `TimeInterval` (`TICKS_PER_SECOND = 1_000_000_000`). The Renode-compatible 100 MIPS
//! instruction period is 10 ticks and the synchronization quantum 100 000 ticks. Clocks that do
//! not divide a nanosecond (80 MHz, 32 768 Hz, 120 Hz) are handled by clock entries ([`clock`]),
//! which keep an exact fractional residue like Renode's `ClockEntry`. A u64 covers about 584
//! years of virtual time.
//!
//! On top of the time base this crate provides the CPU-independent machine used by
//! every board (see `DESIGN.md` sections 5 and 7 and `docs/framework.md`):
//!
//! * [`Peripheral`] trait, [`Width`] and per-peripheral [`AccessPolicy`] (Renode's sub-word access
//!   translation), [`SyncRegister`] declarations;
//! * [`MachineCore`]: flash/SRAM, the MMIO map, peripheral slots, signal nets ([`Target`]), the
//!   clock registry and [`EventQueue`], the IRQ-change queue and the log;
//! * [`clock`]: Renode `ClockEntry`/`BaseClockSource` semantics, [`LimitTimer`], [`ManagedThread`],
//!   `ScheduleAction` and the CPU-side [`LocalClock`];
//! * [`Ctx`] and [`View`], what a running peripheral can do to / see of the rest of the machine;
//! * [`ArrayMemory`], [`Json`], and the [`testing::Harness`] for peripheral tests.

pub mod access;
pub mod clock;
pub mod event;
pub mod json;
pub mod log;
pub mod machine;
pub mod memory;
pub mod peripheral;
pub mod testing;

pub use access::{translate_read, translate_write, AccessPolicy, RegisterAccess, Resolution, Translations, Width, Widths};
pub use clock::{
    Advance, ClockEntry, ClockId, ClockRead, Direction, LimitTimer, LimitTimerConfig, LocalClock, ManagedThread, WorkMode,
};
pub use event::{EventId, EventQueue, PendingEvent, PoppedEvent};
pub use json::Json;
pub use log::{LogBuffer, LogEntry, LogLevel, WarnSet};
pub use machine::{
    Ctx, MachineCore, MachineStats, MapError, MappedRegion, View, MAX_EVENTS_PER_DRAIN, MAX_OUTPUT_LINES,
    NOTIFY_IRQ_CHANGED, NOTIFY_STOP_REQUESTED,
};
pub use memory::{ArrayMemory, MemoryLayout, PlainMemory, RegionKind};
pub use peripheral::{PeriphId, Peripheral, SyncRegister, Target};

/// Virtual time in nanoseconds (ticks of 1/`TICKS_PER_SECOND` second).
pub type Time = u64;

pub const TICKS_PER_SECOND: Time = 1_000_000_000;
pub const TICKS_PER_MILLISECOND: Time = TICKS_PER_SECOND / 1_000;
pub const TICKS_PER_MICROSECOND: Time = TICKS_PER_SECOND / 1_000_000;

/// Renode's default `PerformanceInMips` (100) preserved by every existing NGC
/// evidence run: one instruction advances virtual time by 10 ns.
pub const DEFAULT_MIPS: u64 = 100;
pub const TICKS_PER_INSTRUCTION: Time = TICKS_PER_SECOND / (DEFAULT_MIPS * 1_000_000);

/// Renode's default global synchronization quantum (100 virtual microseconds).
pub const QUANTUM: Time = 100 * TICKS_PER_MICROSECOND;

/// Ticks per cycle of a clock, when the clock divides the time base exactly (`None` for 80 MHz,
/// 32 768 Hz, 120 Hz...). Do **not** derive timer periods from this: use clock entries, which keep
/// the exact fractional residue and round limits up to whole nanoseconds like Renode.
pub const fn ticks_per_cycle(hz: u64) -> Option<Time> {
    if hz == 0 || TICKS_PER_SECOND % hz != 0 {
        None
    } else {
        Some(TICKS_PER_SECOND / hz)
    }
}

pub const fn from_micros(us: u64) -> Time {
    us * TICKS_PER_MICROSECOND
}

pub const fn from_millis(ms: u64) -> Time {
    ms * TICKS_PER_MILLISECOND
}

/// Nearest tick to a duration in seconds (negative and NaN inputs map to zero).
pub fn from_secs_f64(seconds: f64) -> Time {
    if seconds.is_nan() || seconds <= 0.0 {
        0
    } else {
        (seconds * TICKS_PER_SECOND as f64).round() as Time
    }
}

pub fn to_secs_f64(time: Time) -> f64 {
    time as f64 / TICKS_PER_SECOND as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_base_is_nanoseconds_like_renode() {
        assert_eq!(TICKS_PER_SECOND, 1_000_000_000);
        assert_eq!(TICKS_PER_MILLISECOND, 1_000_000);
        assert_eq!(TICKS_PER_MICROSECOND, 1_000);
        assert_eq!(TICKS_PER_INSTRUCTION, 10);
        assert_eq!(QUANTUM, 100_000);
        assert_eq!(QUANTUM / TICKS_PER_INSTRUCTION, 10_000, "instructions per quantum");
        assert_eq!(from_micros(25), 25_000);
        assert_eq!(from_millis(50), 50_000_000);
        assert_eq!(from_secs_f64(1.5), 1_500_000_000);
        assert_eq!(to_secs_f64(250_000_000), 0.25);
    }

    #[test]
    fn ticks_per_cycle_is_exact_only() {
        for hz in [1_000_000_000, 100_000_000, 10_000, 1_000, 50, 1] {
            assert!(ticks_per_cycle(hz).is_some(), "{hz} Hz");
        }
        assert_eq!(ticks_per_cycle(100_000_000), Some(10));
        for hz in [80_000_000, 32_768, 32_000_000 / 3, 120, 60, 0] {
            assert_eq!(ticks_per_cycle(hz), None, "{hz} Hz does not divide a nanosecond");
        }
        // The system clocks that do not divide 1 ns are exact through clock entries instead.
        let e = ClockEntry::new(1, 80_000_000, true, Direction::Ascending, WorkMode::Periodic);
        assert_eq!(e.ratio(), (2, 25));
    }
}
