// Ported from emulation/models/NGCHandsetButtons.cs.

//! `NGCHandsetButtons`: the physical-pin stimulus for the handset's TIM3 capture inputs.
//!
//! Synthetic low pulses on `PE3` and `PE5` pass through the real timer capture (`timer3@0` / `timer3@2`), its
//! interrupt (IRQ 29) and the firmware callbacks; nothing in application RAM or in the firmware is touched. The
//! stimulus runs on a 10 kHz managed thread (period 100 000 ns), so every edge lies on a 100 us grid.
//!
//! * The thread waits until TIM3 is configured for capture (`CR1.CEN`, `CCMR1`/`CCMR2` selecting the input
//!   mapping `0x201` for both channel pairs); only then it sets both pins high (idle-high) and the model is
//!   *ready*. Before that the timer ignores GPIO inputs in output mode.
//! * [`Buttons::press`] queues a `204 800 us` low pulse (exactly 250 counts of the ideal 80 MHz / 65536 clock;
//!   Renode's integer prescaler division makes it 249 or 250 counts of the model's 1220 Hz), [`Buttons::pulse`] an
//!   arbitrary one, [`Buttons::confirm`] the staggered two-key gesture: `PE3` low, `PE5` low 50 ms later
//!   (500 ticks), each held for 204.8 ms. The stagger is a functional fixture so that the firmware sees two
//!   separate key events inside its 250-tick combination window, not a measured switch skew.
//! * One gesture at a time; the pulse and release counters count completed gestures, not pins.
//!
//! Registers (word only, size 0x100): offset 0 = `ready | activeMask << 8`, 4 = pulse count, 8 = release
//! count; everything else reads 0 (diagnostic only, the firmware never reads them).
//!
//! Lines: output 0 is `PE3`, output 1 `PE5` (wire them to `gpioE@3 | timer3@0` and `gpioE@5 | timer3@2`).

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, ManagedThread, Peripheral, Time, View, Width};
use std::fmt;

pub const SIZE: u32 = 0x100;
pub const PE3_LINE: u32 = 0;
pub const PE5_LINE: u32 = 1;
/// The stimulus clock (`ObtainManagedThread(Tick, 10000)`).
pub const TICK_HZ: u64 = 10_000;
/// Default low width: 204 800 us.
pub const DEFAULT_PULSE_US: u32 = 204_800;
/// Distance between the two falling edges of a confirm.
pub const CONFIRM_STAGGER_US: u32 = 50_000;

const TICK: u64 = 1;
// TIM3 registers read by the readiness check.
const TIM_CR1: u32 = 0x00;
const TIM_CCMR1: u32 = 0x18;
const TIM_CCMR2: u32 = 0x1C;

/// The errors Renode raises as exceptions (`InvalidOperationException`, `ArgumentOutOfRangeException`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ButtonsError {
    /// "TIM3 capture inputs are not ready".
    NotReady,
    /// "A button pulse is already in progress".
    Busy,
    /// `mask` is 0 or above 3.
    MaskOutOfRange,
    /// `durationMicroseconds` is 0 or above 2 000 000.
    DurationOutOfRange,
}

impl fmt::Display for ButtonsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ButtonsError::NotReady => f.write_str("TIM3 capture inputs are not ready"),
            ButtonsError::Busy => f.write_str("A button pulse is already in progress"),
            ButtonsError::MaskOutOfRange => f.write_str("Specified argument was out of the range of valid values. (Parameter 'mask')"),
            ButtonsError::DurationOutOfRange => {
                f.write_str("Specified argument was out of the range of valid values. (Parameter 'durationMicroseconds')")
            }
        }
    }
}

impl std::error::Error for ButtonsError {}

pub struct Buttons {
    name: String,
    /// Base address of the TIM3 register window (`timer: timer3`).
    timer_base: u32,
    tick: ManagedThread,
    initialized: bool,
    gesture_started: bool,
    staggered_confirm: bool,
    pending_mask: u32,
    active_mask: u32,
    duration_ticks: u32,
    delayed_start_ticks: u32,
    remaining_pe3_ticks: u32,
    remaining_pe5_ticks: u32,
    pulse_count: u64,
    release_count: u64,
    pe3: bool,
    pe5: bool,
}

impl Buttons {
    /// `buttons: GPIOPort.NGCHandsetButtons @ sysbus 0x61000200 { timer: timer3 }`; `timer_base` is where
    /// the TIM3 model is mapped (`0x40000400` on the handset).
    pub fn new(name: impl Into<String>, timer_base: u32) -> Self {
        Self {
            name: name.into(),
            timer_base,
            tick: ManagedThread::new(TICK_HZ, TICK),
            initialized: false,
            gesture_started: false,
            staggered_confirm: false,
            pending_mask: 0,
            active_mask: 0,
            duration_ticks: 0,
            delayed_start_ticks: 0,
            remaining_pe3_ticks: 0,
            remaining_pe5_ticks: 0,
            pulse_count: 0,
            release_count: 0,
            pe3: false,
            pe5: false,
        }
    }

    /// TIM3 capture inputs are configured and the pins have been set idle-high.
    pub fn ready(&self) -> bool {
        self.initialized
    }

    pub fn pulse_count(&self) -> u64 {
        self.pulse_count
    }

    pub fn release_count(&self) -> u64 {
        self.release_count
    }

    pub fn active_mask(&self) -> u32 {
        self.active_mask
    }

    pub fn pending_mask(&self) -> u32 {
        self.pending_mask
    }

    /// True while a gesture is queued or running.
    pub fn busy(&self) -> bool {
        self.active_mask != 0 || self.pending_mask != 0
    }

    pub fn confirm_stagger_microseconds(&self) -> u32 {
        CONFIRM_STAGGER_US
    }

    /// `Press(mask)`: mask 1 = physical `PE3`, 2 = `PE5`, 3 = both exactly simultaneous.
    pub fn press(&mut self, mask: u32) -> Result<(), ButtonsError> {
        self.pulse(mask, DEFAULT_PULSE_US)
    }

    /// `Confirm()`: `PE3` low, `PE5` low 50 ms later, each for 204.8 ms.
    pub fn confirm(&mut self) -> Result<(), ButtonsError> {
        self.queue_gesture(3, DEFAULT_PULSE_US, CONFIRM_STAGGER_US)
    }

    /// `Pulse(mask, durationMicroseconds)`; the width is rounded up to the 100 us stimulus clock.
    pub fn pulse(&mut self, mask: u32, duration_microseconds: u32) -> Result<(), ButtonsError> {
        if mask == 0 || mask > 3 {
            return Err(ButtonsError::MaskOutOfRange);
        }
        if duration_microseconds == 0 || duration_microseconds > 2_000_000 {
            return Err(ButtonsError::DurationOutOfRange);
        }
        self.queue_gesture(mask, duration_microseconds, 0)
    }

    fn queue_gesture(&mut self, mask: u32, duration_microseconds: u32, stagger_microseconds: u32) -> Result<(), ButtonsError> {
        if !self.initialized {
            return Err(ButtonsError::NotReady);
        }
        if self.active_mask != 0 || self.pending_mask != 0 {
            return Err(ButtonsError::Busy);
        }
        self.pending_mask = mask;
        self.duration_ticks = (duration_microseconds + 99) / 100;
        self.delayed_start_ticks = (stagger_microseconds + 99) / 100;
        self.staggered_confirm = stagger_microseconds != 0;
        self.gesture_started = false;
        Ok(())
    }

    /// `StartPins(mask)`.
    fn start_pins(&mut self, ctx: &mut Ctx<'_>, mask: u32) {
        self.active_mask |= mask;
        self.pending_mask &= !mask;
        if mask & 1 != 0 {
            self.remaining_pe3_ticks = self.duration_ticks;
            self.set_pin(ctx, PE3_LINE, false);
        }
        if mask & 2 != 0 {
            self.remaining_pe5_ticks = self.duration_ticks;
            self.set_pin(ctx, PE5_LINE, false);
        }
    }

    fn set_pin(&mut self, ctx: &mut Ctx<'_>, line: u32, level: bool) {
        if line == PE3_LINE {
            self.pe3 = level;
        } else {
            self.pe5 = level;
        }
        ctx.set_output(line, level);
    }

    /// The thread body (`Tick`).
    fn run_tick(&mut self, ctx: &mut Ctx<'_>) {
        if !self.initialized {
            let word = |ctx: &Ctx<'_>, offset: u32| ctx.mem_peek(self.timer_base + offset, Width::Word).unwrap_or(0);
            let (cr1, ccmr1, ccmr2) = (word(ctx, TIM_CR1), word(ctx, TIM_CCMR1), word(ctx, TIM_CCMR2));
            if cr1 & 1 == 0 || ccmr1 & 0x303 != 0x201 || ccmr2 & 0x303 != 0x201 {
                return;
            }
            // Establish idle-high after the capture configuration.
            self.set_pin(ctx, PE3_LINE, true);
            self.set_pin(ctx, PE5_LINE, true);
            self.initialized = true;
            return;
        }
        if !self.gesture_started && self.pending_mask != 0 {
            let first = if self.staggered_confirm { 1 } else { self.pending_mask };
            self.start_pins(ctx, first);
            self.gesture_started = true;
            self.pulse_count += 1;
            return;
        }
        if !self.gesture_started {
            return;
        }
        if self.active_mask & 1 != 0 {
            self.remaining_pe3_ticks = self.remaining_pe3_ticks.wrapping_sub(1);
            if self.remaining_pe3_ticks == 0 {
                self.set_pin(ctx, PE3_LINE, true);
                self.active_mask &= !1;
            }
        }
        if self.active_mask & 2 != 0 {
            self.remaining_pe5_ticks = self.remaining_pe5_ticks.wrapping_sub(1);
            if self.remaining_pe5_ticks == 0 {
                self.set_pin(ctx, PE5_LINE, true);
                self.active_mask &= !2;
            }
        }
        // Start the second pin after counting the delay from the first falling edge; its width starts here.
        if self.delayed_start_ticks != 0 {
            self.delayed_start_ticks -= 1;
            if self.delayed_start_ticks == 0 {
                let mask = self.pending_mask;
                self.start_pins(ctx, mask);
            }
        }
        if self.active_mask == 0 && self.pending_mask == 0 {
            self.gesture_started = false;
            self.release_count += 1;
        }
    }

    /// The `Summary` property of the C# model, byte for byte (C# prints booleans as `True`/`False`).
    pub fn summary_text(&self) -> String {
        let flag = |b: bool| if b { "True" } else { "False" };
        format!(
            "ready={}; activeMask={}; pendingMask={}; pulses={}; releases={}; PE3={}; PE5={}; gesture={}; confirmStaggerUs={}; \
             delayedStartTicks={}; remainingPE3Ticks={}; remainingPE5Ticks={}",
            flag(self.initialized),
            self.active_mask,
            self.pending_mask,
            self.pulse_count,
            self.release_count,
            flag(self.pe3),
            flag(self.pe5),
            if self.gesture_started || self.pending_mask != 0 {
                if self.staggered_confirm {
                    "staggered-confirm"
                } else {
                    "pulse"
                }
            } else {
                "idle"
            },
            CONFIRM_STAGGER_US,
            self.delayed_start_ticks,
            self.remaining_pe3_ticks,
            self.remaining_pe5_ticks
        )
    }
}

impl Peripheral for Buttons {
    fn name(&self) -> &str {
        &self.name
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        // `machine.ObtainManagedThread(Tick, 10000)` followed by `thread.Start()` in the constructor.
        self.tick.attach(ctx);
        self.tick.start(ctx);
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.initialized = false;
        self.gesture_started = false;
        self.staggered_confirm = false;
        self.pending_mask = 0;
        self.active_mask = 0;
        self.duration_ticks = 0;
        self.delayed_start_ticks = 0;
        self.remaining_pe3_ticks = 0;
        self.remaining_pe5_ticks = 0;
        self.pulse_count = 0;
        self.release_count = 0;
        self.set_pin(ctx, PE3_LINE, false);
        self.set_pin(ctx, PE5_LINE, false);
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn read(&mut self, offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        match offset {
            0 => u32::from(self.initialized) | self.active_mask << 8,
            4 => self.pulse_count as u32,
            8 => self.release_count as u32,
            _ => 0,
        }
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        if token == TICK {
            self.run_tick(ctx);
        }
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        Some(match offset {
            0 => u32::from(self.initialized) | self.active_mask << 8,
            4 => self.pulse_count as u32,
            8 => self.release_count as u32,
            _ => 0,
        })
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.summary_text()
    }

    impl_peripheral_any!();
}
