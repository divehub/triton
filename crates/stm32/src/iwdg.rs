// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32_IndependentWatchdog.cs
// (MIT License, Copyright (c) Antmicro and Zisis Adamos).

//! `Timers.STM32_IndependentWatchdog`: the independent watchdog of both boards (`frequency: 32000`).
//!
//! * `KEY`: `0x5555` unlocks `PR`/`RLR`/`WINR` (any other write locks again), `0xAAAA` reloads (restarts the
//!   countdown from `RLR`; with a window set, a reload while the counter is still above the window requests a
//!   reset), `0xCCCC` starts the watchdog. The counter is a one-shot descending `LimitTimer` at `32 kHz /
//!   prescaler` (initially divider 4, limit `0xFFF`: a started and never reloaded watchdog expires after
//!   `4095 * 4 / 32000` s = 511.875 ms).
//! * `PR` selects the divider `4 << PR`, capped at 256 (`PR = 6` and `7` both give 256).
//! * Expiry (or a window violation) logs a warning and **requests a machine reset** (`machine.RequestReset()`).
//!
//! # Reset request surface
//!
//! The framework has no machine-level reset request yet, so the request is recorded in the model:
//! [`Iwdg::reset_requested`] / [`Iwdg::take_reset_request`] (the virtual time of the request), plus
//! [`Iwdg::reset_request_count`]. The system polls them after each quantum and acts like for any other reset
//! (Renode runs `Machine.Reset()` at the next synchronized state). // Renode parity: after a request the
//! watchdog itself keeps running until the machine is reset. The recorded time is the request's: the
//! ceil-nanosecond expiry for a timeout, the clock-source time of the write for a window violation (which
//! lags the CPU inside a chunk). `tests/renode_timer` checks against Renode 1.17.0 that the reset it carries
//! out is at the end of the 100 us quantum that contains that time (`ceil(request / quantum) * quantum`), for
//! timeouts and window violations. Only the first request is kept until it is taken (`RequestReset` is
//! idempotent for the system); a machine reset does not clear a request that nobody took.
//!
//! Register access: word only, size 0x400 ([`SIZE`]). Writes to `PR`/`RLR`/`WINR` while locked log a warning
//! but still change the stored (read-back) value, as in Renode.

use emu_core::{
    impl_peripheral_any, AccessPolicy, ClockRead, Ctx, Direction, LimitTimer, LimitTimerConfig, LogLevel, Peripheral, Time,
    View, WorkMode, Width,
};
use std::borrow::Cow;
use std::fmt::Write as _;

pub const SIZE: u32 = 0x400;

pub mod reg {
    pub const KEY: u32 = 0x0;
    pub const PR: u32 = 0x4;
    pub const RLR: u32 = 0x8;
    pub const SR: u32 = 0xC;
    pub const WINR: u32 = 0x10;
}

pub mod key {
    pub const UNLOCK: u32 = 0x5555;
    pub const RELOAD: u32 = 0xAAAA;
    pub const START: u32 = 0xCCCC;
}

const DEFAULT_RELOAD: u32 = 0xFFF;
const DEFAULT_WINDOW: u32 = 0xFFF;
const DEFAULT_PRESCALER_DIVIDER: u64 = 4;
const TIMER: u64 = 1;

/// Bits of a register that are known but not implemented (`WithReservedBits`/`WithTag`).
pub(crate) struct Tag {
    pub name: Cow<'static, str>,
    pub pos: u32,
    pub width: u32,
}

pub(crate) fn tag(name: &'static str, pos: u32, width: u32) -> Tag {
    Tag { name: Cow::Borrowed(name), pos, width }
}

fn field_mask(pos: u32, width: u32) -> u32 {
    if width == 0 {
        0
    } else if width >= 32 {
        u32::MAX
    } else {
        ((1u32 << width) - 1) << pos
    }
}

/// `BitHelper.GetSetBitsPretty`: `"0, 3-5, 7"`.
pub(crate) fn set_bits_pretty(mask: u32) -> String {
    let mut out = String::new();
    let mut bit = 0;
    while bit < 32 {
        if mask & (1 << bit) == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 32 && mask & (1 << (bit + 1)) != 0 {
            bit += 1;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if start == bit {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}-{bit}");
        }
        bit += 1;
    }
    out
}

/// `PeripheralRegister.LogUnhandledWrites` for non-silent tags: written bits that belong to no field and overlap
/// a tag are reported as a warning (once per offset and bit set).
pub(crate) fn warn_tags(ctx: &mut Ctx<'_>, offset: u32, value: u32, defined: u32, tags: &[Tag]) {
    let unhandled = value & !defined;
    if unhandled == 0 {
        return;
    }
    let mut names = String::new();
    for t in tags.iter().filter(|t| field_mask(t.pos, t.width) & unhandled != 0) {
        if !names.is_empty() {
            names.push_str(", ");
        }
        let _ = write!(names, "{} (0x{:X})", t.name, (value & field_mask(t.pos, t.width)) >> t.pos);
    }
    if names.is_empty() {
        return;
    }
    let key = (u64::from(unhandled) << 16) | u64::from(offset & 0xFFFF);
    ctx.log_once(
        LogLevel::Warning,
        key,
        format_args!(
            "Unhandled write to offset 0x{offset:X}. Unhandled bits: [{}] when writing value 0x{value:X}. Tags: {names}.",
            set_bits_pretty(unhandled)
        ),
    );
}

pub struct Iwdg {
    name: String,
    timer: LimitTimer,
    window_option: bool,
    default_prescaler: u32,
    // Register storage (the fields without a value provider).
    prescaler: u32,
    reload_register: u32,
    window_register: u32,
    // Model state.
    unlocked: bool,
    reload_value: u32,
    window: u32,
    window_enabled: bool,
    reset_request: Option<Time>,
    reset_requests: u64,
}

impl Iwdg {
    /// `new STM32_IndependentWatchdog(machine, frequency, windowOption = true, defaultPrescaler = 0)`.
    pub fn new(name: impl Into<String>, frequency: u64, window_option: bool, default_prescaler: u32) -> Self {
        let cfg = LimitTimerConfig {
            limit: u64::from(DEFAULT_RELOAD),
            direction: Direction::Descending,
            enabled: false,
            mode: WorkMode::OneShot,
            event_enabled: true,
            auto_update: true,
            divider: DEFAULT_PRESCALER_DIVIDER,
            ..LimitTimerConfig::new(frequency)
        };
        Self {
            name: name.into(),
            timer: LimitTimer::new(cfg, TIMER),
            window_option,
            default_prescaler,
            prescaler: default_prescaler & 7,
            reload_register: DEFAULT_RELOAD,
            window_register: DEFAULT_WINDOW,
            unlocked: false,
            reload_value: DEFAULT_RELOAD,
            window: DEFAULT_WINDOW,
            window_enabled: false,
            reset_request: None,
            reset_requests: 0,
        }
    }

    /// The NGC platforms' watchdog: `frequency: 32000`, window option on, default prescaler 0.
    pub fn ngc(name: impl Into<String>) -> Self {
        Self::new(name, 32_000, true, 0)
    }

    /// True while a machine reset requested by the watchdog (expiry or window violation) has not been taken.
    pub fn reset_requested(&self) -> bool {
        self.reset_request.is_some()
    }

    /// The virtual time of the pending reset request, clearing it (the system calls this when it acts).
    pub fn take_reset_request(&mut self) -> Option<Time> {
        self.reset_request.take()
    }

    /// Reset requests since construction (not cleared by `Reset`).
    pub fn reset_request_count(&self) -> u64 {
        self.reset_requests
    }

    /// Whether the countdown is running.
    pub fn running(&self, clock: &dyn ClockRead) -> bool {
        self.timer.enabled(clock)
    }

    /// The counter value (`watchdogTimer.Value`) now, without side effects.
    pub fn counter(&self, clock: &dyn ClockRead) -> u64 {
        self.timer.value(clock)
    }

    fn request_reset(&mut self, ctx: &mut Ctx<'_>) {
        self.reset_requests += 1;
        // Several requests before the system acts collapse into the first one's time (RequestReset is idempotent).
        self.reset_request.get_or_insert(ctx.now());
    }

    fn do_reload(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.set_limit(ctx, u64::from(self.reload_value));
    }

    fn offset_label(offset: u32) -> String {
        const NAMES: [(u32, &str); 5] = [(0x0, "Key"), (0x4, "Prescaler"), (0x8, "Reload"), (0xC, "Status"), (0x10, "Window")];
        let text = match NAMES.iter().rev().find(|(o, _)| *o <= offset) {
            Some((o, name)) if *o == offset => (*name).to_string(),
            Some((o, name)) => format!("{name}+0x{:x}", offset - o),
            None => "unknown".to_string(),
        };
        format!(" ({text})")
    }

    /// Register value without side effects (`None` for offsets without a register).
    fn register(&self, offset: u32) -> Option<u32> {
        match offset {
            reg::KEY => Some(0), // write-only
            reg::PR => Some(self.prescaler),
            reg::RLR => Some(self.reload_register),
            reg::SR => Some(0), // PVU, RVU and WVU read false
            reg::WINR if self.window_option => Some(self.window_register),
            _ => None,
        }
    }
}

impl Peripheral for Iwdg {
    fn name(&self) -> &str {
        &self.name
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.attach(ctx);
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        // BasicDoubleWordPeripheral.Reset(): the registers return to their reset values.
        self.prescaler = self.default_prescaler & 7;
        self.reload_register = DEFAULT_RELOAD;
        self.window_register = DEFAULT_WINDOW;
        self.timer.reset(ctx);
        self.unlocked = false;
        self.reload_value = DEFAULT_RELOAD;
        self.window = DEFAULT_WINDOW;
        self.window_enabled = false;
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.register(offset) {
            Some(value) => value,
            None => {
                ctx.warn_once(
                    0x4EAD_0000 | u64::from(offset),
                    format_args!("Unhandled read from offset 0x{offset:X}{}.", Self::offset_label(offset)),
                );
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match offset {
            reg::KEY => {
                let key = value & 0xFFFF;
                self.unlocked = false;
                match key {
                    key::RELOAD => {
                        if self.window_enabled && self.timer.value(ctx) > u64::from(self.window) {
                            ctx.logf(LogLevel::Warning, format_args!("Watchdog reloaded outside of window, triggering reset!"));
                            self.request_reset(ctx);
                        } else {
                            self.do_reload(ctx);
                        }
                    }
                    key::START => self.timer.set_enabled(ctx, true),
                    key::UNLOCK => self.unlocked = true,
                    _ => {}
                }
                warn_tags(ctx, offset, value, 0xFFFF, &[tag("RESERVED", 16, 16)]);
            }
            reg::PR => {
                self.prescaler = value & 7;
                if self.unlocked {
                    let divider = (1u64 << (2 + u64::from(value & 7))).min(256);
                    self.timer.set_divider(ctx, divider);
                } else {
                    ctx.logf(LogLevel::Warning, format_args!("Trying to change watchdog prescaler value without unlocking it"));
                }
                warn_tags(ctx, offset, value, 0x7, &[tag("RESERVED", 3, 29)]);
            }
            reg::RLR => {
                self.reload_register = value & 0xFFF;
                if self.unlocked {
                    self.reload_value = value & 0xFFF;
                } else {
                    ctx.logf(LogLevel::Warning, format_args!("Trying to change watchdog reload value without unlocking it"));
                }
                warn_tags(ctx, offset, value, 0xFFF, &[tag("RESERVED", 12, 20)]);
            }
            reg::SR => warn_tags(ctx, offset, value, 0x7, &[tag("RESERVED", 3, 29)]),
            reg::WINR if self.window_option => {
                self.window_register = value & 0xFFF;
                if self.unlocked {
                    self.window_enabled = true;
                    self.window = value & 0xFFF;
                    self.do_reload(ctx);
                } else {
                    ctx.logf(LogLevel::Warning, format_args!("Trying to change watchdog window without unlocking it"));
                }
                warn_tags(ctx, offset, value, 0xFFF, &[tag("RESERVED", 12, 20)]);
            }
            _ => ctx.warn_once(
                0x4EAD_8000 | u64::from(offset),
                format_args!("Unhandled write to offset 0x{offset:X}{}, value 0x{value:X}.", Self::offset_label(offset)),
            ),
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        if token == TIMER && self.timer.on_limit_reached() {
            ctx.logf(LogLevel::Warning, format_args!("Watchdog reset triggered!"));
            self.request_reset(ctx);
        }
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        self.register(offset)
    }

    fn summary(&self, view: &View<'_>) -> String {
        format!(
            "{}: running={} counter={} divider={} unlocked={} reload={} window={}{} reset requested={}",
            self.name,
            self.timer.enabled(view),
            self.timer.value(view),
            self.timer.divider(),
            self.unlocked,
            self.reload_value,
            self.window,
            if self.window_enabled { " (enabled)" } else { "" },
            self.reset_request.is_some()
        )
    }

    impl_peripheral_any!();
}
