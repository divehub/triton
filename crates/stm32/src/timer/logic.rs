// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32_Timer.cs
// (MIT License, Copyright (c) Antmicro).

//! Behavior of `Timers.STM32_Timer` on top of the clock entries of [`Model`]: the `LimitReached` handlers of
//! the counter and of the compare timers, interrupt outputs, compare-timer scheduling (`UpdateTimer`), input
//! capture (`OnGPIO`) and the slave modes. Every function mirrors the C# member it is named after; deliberate
//! quirks of Renode are kept and marked `// Renode parity`.

use super::io::Io;
use super::model::{
    Model, Scheduling, BREAK_LINE, CAPTURE_COMPARE_LINE, COMMUTATION_LINE, IRQ_LINE, MAIN, PIN_LINE_BASE, TRIGGER_LINE, UPDATE_LINE,
};
use super::regs::reg;
use emu_core::{LogLevel, WorkMode, TICKS_PER_SECOND};

/// `OutputCompareMode`.
pub(crate) mod ocm {
    pub const SET_ACTIVE_ON_MATCH: u32 = 1;
    pub const SET_INACTIVE_ON_MATCH: u32 = 2;
    pub const TOGGLE_ON_MATCH: u32 = 3;
    pub const FORCE_INACTIVE: u32 = 4;
    pub const FORCE_ACTIVE: u32 = 5;
    pub const PWM_MODE1: u32 = 6;
    pub const PWM_MODE2: u32 = 7;
}

/// `CaptureCompareSelection`.
pub(crate) mod ccs {
    pub const OUTPUT: u8 = 0;
    pub const INPUT_TI_SAME: u8 = 1;
    pub const INPUT_TI_CROSS: u8 = 2;
    pub const INPUT_TRC: u32 = 3;
}

/// `SlaveModeSelection`.
pub(crate) mod sms {
    pub const ENCODER1: u8 = 1;
    pub const ENCODER2: u8 = 2;
    pub const ENCODER3: u8 = 3;
    pub const RESET: u8 = 4;
    pub const GATED: u8 = 5;
    pub const TRIGGER: u8 = 6;
    pub const EXTERNAL_CLOCK1: u32 = 7;
}

/// `TriggerSelection`.
const TS_TIMER_INPUT1: u8 = 5;
const TS_TIMER_INPUT2: u8 = 6;

/// `InputCaptureEdge` (`(CCxP << 1) | CCxNP`).
const EDGE_RISING: u8 = 0;
const EDGE_FALLING: u8 = 2;
const EDGE_BOTH: u8 = 3;

/// The pseudo input that resets the timer (`ResetPin`).
pub const RESET_INPUT: u32 = 0xFF;

impl Model {
    // ---- mode predicates ---------------------------------------------------------------------------

    pub(crate) fn is_encoder_mode(&self) -> bool {
        matches!(self.st.sms, sms::ENCODER1 | sms::ENCODER2 | sms::ENCODER3)
    }

    pub(crate) fn is_trigger_mode(&self) -> bool {
        self.st.sms == sms::TRIGGER
    }

    pub(crate) fn is_output_mode(&self, i: usize) -> bool {
        self.st.ch[i].mode == ccs::OUTPUT
    }

    pub(crate) fn is_input_mode(&self, i: usize) -> bool {
        self.st.ch[i].mode != ccs::OUTPUT
    }

    /// `channel.CompareMode.Value`: the field object lives in a specific variant of the channel pair's CCMR
    /// (see `regs.rs`).
    pub(crate) fn compare_mode(&self, i: usize) -> u32 {
        let (pair, half) = (i / 2, i % 2);
        let variant = if half == 0 { 1 } else { 2 };
        (self.st.ccmr[pair][variant] >> (half * 8 + 4)) & 7
    }

    /// `channel.Prescaler.Value` (the field object lives in the both-input variant).
    pub(crate) fn input_prescaler(&self, i: usize) -> u32 {
        let (pair, half) = (i / 2, i % 2);
        (self.st.ccmr[pair][3] >> (half * 8 + 2)) & 3
    }

    // ---- outputs -----------------------------------------------------------------------------------

    /// `Connections[i].Set(level)` (edge only). Only observed pins reach the machine at event time; the
    /// level of the others is pushed when an access ends (see `Stm32Timer`).
    pub(crate) fn set_pin(&mut self, io: &mut dyn Io, i: usize, level: bool) {
        if self.observed & (1 << i) != 0 {
            io.mark_event();
        }
        if self.st.pins[i] == level {
            return;
        }
        self.st.pins[i] = level;
        if self.observed & (1 << i) != 0 {
            io.set_output(PIN_LINE_BASE + i as u32, level);
        } else {
            self.pin_flush |= 1 << i;
        }
    }

    fn set_irq_output(&mut self, io: &mut dyn Io, index: usize, level: bool) {
        if self.st.irq[index] == level {
            return;
        }
        self.st.irq[index] = level;
        let line = match index {
            0 => IRQ_LINE,
            1 => BREAK_LINE,
            2 => UPDATE_LINE,
            3 => TRIGGER_LINE,
            4 => COMMUTATION_LINE,
            _ => CAPTURE_COMPARE_LINE,
        };
        io.set_output(line, level);
    }

    /// `UpdateInterrupts()`.
    pub(crate) fn update_interrupts(&mut self, io: &mut dyn Io) {
        io.mark_event();
        let mut cc_irq = false;
        for c in &self.st.ch {
            cc_irq |= c.iflag & c.ie;
        }
        let update_irq = self.st.update_flag & self.st.uie;
        let trigger_irq = self.st.tif & self.st.tie;
        self.set_irq_output(io, 0, cc_irq || update_irq || trigger_irq);
        self.set_irq_output(io, 1, false);
        self.set_irq_output(io, 2, update_irq);
        self.set_irq_output(io, 3, trigger_irq);
        self.set_irq_output(io, 4, false);
        self.set_irq_output(io, 5, cc_irq);
    }

    // ---- LimitReached handlers ---------------------------------------------------------------------

    /// The `LimitReached` event of entry `i` (called by the clock source with the entry already reset).
    pub(crate) fn on_limit(&mut self, io: &mut dyn Io, i: usize) {
        if i == MAIN {
            self.on_main_limit(io);
            // `LimitReached += TrySuppress` of the NGC subclass runs after the base handler.
            if self.sched == Scheduling::NgcArithmeticPwm {
                self.try_suppress(io);
            }
        } else {
            self.on_cc_limit(io, i - 1);
        }
    }

    /// `NGCLazyPwmTimer.TrySuppress()` in arithmetic mode (`LazyUnconnectedPWM` and `ArithmeticUnconnectedPWM`
    /// both on): after a normal overflow of a plain PWM timer the periodic stock events are given up. Every
    /// early `return` of the C# is an unmet condition here. Register conditions use the values the stock
    /// register reads return (`base.ReadDoubleWord`), including their layout quirks.
    ///
    /// Engaging changes no timer state in this model (it keeps the exact stock state lazily); it only means
    /// that no event of this timer exists until [`Model::restore`].
    fn try_suppress(&mut self, io: &mut dyn Io) {
        if self.st.engaged || self.st.external_input || self.observed != 0 {
            return;
        }
        let now = self.t;
        let read = |m: &Self, offset: u32| m.reg_value(offset, now).unwrap_or(0);
        // Ascending, edge-aligned, periodic, update-enabled normal timer.
        let control = read(self, reg::CR1);
        if control & 0x7B != 1
            || read(self, reg::SMCR) != 0
            || read(self, reg::DIER) != 0
            || read(self, reg::RCR) != 0
            || read(self, reg::SR) & 1 == 0
            || !self.lt_enabled(MAIN)
        {
            return;
        }
        // A latent repetition count must drain through stock overflows first.
        if self.st.repetitions_left != 0 {
            return;
        }
        let frequency = self.cfg.frequency;
        let divider = self.clk.div[MAIN];
        if frequency % divider != 0 {
            return;
        }
        let effective = frequency / divider;
        // Exact integral emulator ticks per timer cycle: parent and compare phases coincide at each overflow.
        if effective == 0 || TICKS_PER_SECOND % effective != 0 {
            return;
        }
        if (1..=4).any(|k| self.clk.div[k] != divider) {
            return;
        }
        let mode1 = read(self, reg::CCMR1);
        let mode2 = read(self, reg::CCMR2);
        if mode1 & 0x303 != 0 || mode2 & 0x303 != 0 {
            return;
        }
        let output_enable = read(self, reg::CCER);
        let parent = self.entry(MAIN);
        let parent_phase = parent.residuum();
        let mut any_pwm = false;
        for i in 0..4 {
            if output_enable & (1 << (i * 4)) == 0 {
                continue;
            }
            let mode = ((if i < 2 { mode1 } else { mode2 }) >> ((i % 2) * 8 + 4)) & 7;
            if mode != ocm::PWM_MODE1 && mode != ocm::PWM_MODE2 {
                return;
            }
            // Boundary/outside comparisons and unusual residual phases keep the stock ordering.
            if self.lt_limit(1 + i) >= parent.period() {
                return;
            }
            if self.lt_enabled(1 + i) && self.entry(1 + i).residuum() != parent_phase {
                return;
            }
            any_pwm = true;
        }
        if !any_pwm {
            return;
        }
        // Arithmetic mode: the parent's fractional phase must be a whole number of nanoseconds.
        let cycle_ticks = u128::from(TICKS_PER_SECOND / effective);
        let (phase_num, phase_den) = parent_phase;
        if (u128::from(phase_num) * cycle_ticks) % u128::from(phase_den) != 0 {
            return;
        }
        // The C# disables the compare timers (`Enabled = false`: a `RequestReturn` each) and parks the counter's
        // clock entry.
        self.rr(io);
        self.st.engaged = true;
        self.stats.engagements += 1;
        // Disabling the compare timers (and parking the counter's entry) runs update passes: no phantom limit
        // survives, and none is created while the events are elided.
        self.clk.phantom = None;
    }

    /// `NGCLazyPwmTimer.Restore()`: any register write, input edge or reset gives the stock events back. The
    /// C# rebuilds the compare timers with `UpdateCaptureCompareTimers` (setters that request a CPU return);
    /// the state itself needs no repair here because this model never stopped being the exact stock state.
    pub(crate) fn restore(&mut self, io: &mut dyn Io) {
        if !self.st.engaged {
            return;
        }
        self.st.engaged = false;
        self.rr(io);
    }

    /// `LimitReached += delegate { ... }` of the constructor: the update event.
    fn on_main_limit(&mut self, io: &mut dyn Io) {
        self.main_instant = Some(self.t);
        if self.st.udis {
            return;
        }
        if self.entry(MAIN).mode() == WorkMode::OneShot {
            self.st.enable_requested = false;
        }
        // The buffered ARR takes effect at the update event.
        let reload = u64::from(self.st.auto_reload);
        self.lt_set_limit(io, MAIN, reload);
        io.log(LogLevel::Noisy, format_args!("IRQ pending"));
        self.st.update_flag = true;
        for i in 0..4 {
            self.update_timer(io, i);
            if !self.lt_enabled(1 + i) || !self.is_output_mode(i) {
                continue;
            }
            match self.compare_mode(i) {
                ocm::PWM_MODE1 => self.set_pin(io, i, true),
                ocm::PWM_MODE2 => self.set_pin(io, i, false),
                _ => {}
            }
        }
        if self.st.uie && self.st.repetitions_left == 0 {
            // Central-aligned modes 1 and 2 raise the interrupt only on overflow/underflow, half as often.
            let unbalanced = self.st.cms == 1 || self.st.cms == 2;
            self.st.repetitions_left = 1 + u32::from(self.st.rcr) * if unbalanced { 2 } else { 1 };
            self.update_interrupts(io);
        }
        if self.st.repetitions_left > 0 {
            self.st.repetitions_left -= 1;
        }
    }

    /// `channel.Timer.LimitReached`: a compare match.
    fn on_cc_limit(&mut self, io: &mut dyn Io, i: usize) {
        if !self.is_output_mode(i) {
            return;
        }
        match self.compare_mode(i) {
            ocm::SET_ACTIVE_ON_MATCH => {
                // Blink(): a high pulse.
                self.set_pin(io, i, true);
                self.set_pin(io, i, false);
            }
            ocm::SET_INACTIVE_ON_MATCH => {
                // Unset(); Set(): a low pulse.
                self.set_pin(io, i, false);
                self.set_pin(io, i, true);
            }
            ocm::TOGGLE_ON_MATCH => {
                let level = !self.st.pins[i];
                self.set_pin(io, i, level);
            }
            ocm::PWM_MODE1 => self.set_pin(io, i, false),
            ocm::PWM_MODE2 => self.set_pin(io, i, true),
            _ => {}
        }
        if self.st.ch[i].ie {
            self.st.ch[i].iflag = true;
            io.log(LogLevel::Noisy, format_args!("cctimer{}: Compare IRQ pending", i + 1));
            self.update_interrupts(io);
        }
    }

    // ---- compare timers ----------------------------------------------------------------------------

    /// `CaptureCompareChannel.UpdateTimer()`.
    pub(crate) fn update_timer(&mut self, io: &mut dyn Io, i: usize) {
        let parent = self.entry(MAIN);
        let channel = self.st.ch[i];
        let enable = parent.enabled() && (channel.ie || channel.oe) && parent.value() < self.lt_limit(1 + i);
        self.lt_set_enabled(io, 1 + i, enable);
        if self.lt_enabled(1 + i) {
            let value = self.lt_value(MAIN);
            self.lt_set_value(io, 1 + i, value);
        }
        let direction = self.entry(MAIN).direction();
        self.lt_set_direction(io, 1 + i, direction);
    }

    /// `UpdateCaptureCompareTimers()`.
    pub(crate) fn update_capture_compare_timers(&mut self, io: &mut dyn Io) {
        for i in 0..4 {
            self.update_timer(io, i);
        }
    }

    // ---- register callbacks shared by writes -------------------------------------------------------

    /// `WriteCaptureCompareOutputEnable(i, value)`.
    pub(crate) fn write_cc_output_enable(&mut self, io: &mut dyn Io, i: usize, value: bool) {
        self.st.ch[i].oe = value;
        self.update_timer(io, i);
        if !value {
            self.set_pin(io, i, false);
        }
        io.log(LogLevel::Noisy, format_args!("cctimer{}: Output Enable set to {}", i + 1, value));
    }

    /// `WriteCaptureCompareInterruptEnable(i, value)`.
    pub(crate) fn write_cc_interrupt_enable(&mut self, io: &mut dyn Io, i: usize, value: bool) {
        self.st.ch[i].ie = value;
        self.update_timer(io, i);
        io.log(LogLevel::Noisy, format_args!("cctimer{}: Interrupt Enable set to {}", i + 1, value));
    }

    /// `WriteCaptureCompareSelection(i, value)`.
    pub(crate) fn write_cc_selection(&mut self, io: &mut dyn Io, i: usize, value: u32) {
        if value == ccs::INPUT_TRC {
            io.log_once(LogLevel::Warning, 0x7C00 + i as u64, format_args!("cctimer{i}: Trc mode is not supported"));
            return;
        }
        self.st.ch[i].mode = value as u8;
    }

    /// `WriteOutputCompareMode(i, value)`.
    pub(crate) fn write_output_compare_mode(&mut self, io: &mut dyn Io, i: usize, value: u32) {
        io.log(LogLevel::Noisy, format_args!("cctimer{}: output compare mode set to {}", i + 1, value));
        match value {
            ocm::FORCE_INACTIVE | ocm::SET_ACTIVE_ON_MATCH => self.set_pin(io, i, false),
            ocm::SET_INACTIVE_ON_MATCH | ocm::FORCE_ACTIVE => self.set_pin(io, i, true),
            _ => {}
        }
    }

    /// `ClaimCaptureCompareInterrupt(i, value)`.
    pub(crate) fn claim_cc_interrupt(&mut self, i: usize, value: bool) {
        if !value {
            self.st.ch[i].iflag = false;
        }
    }

    // ---- input capture and slave modes (OnGPIO) ----------------------------------------------------

    /// `CaptureCompareChannel.CaptureEdge`.
    fn capture_edge(&self, i: usize) -> u8 {
        (u8::from(self.st.ch[i].polarity) << 1) | u8::from(self.st.ch[i].comp_polarity)
    }

    /// `CaptureCompareChannel.SetInput(value)`.
    fn set_input(&mut self, io: &mut dyn Io, i: usize, value: bool) {
        if self.st.ch[i].signal == value {
            return;
        }
        let edge = self.capture_edge(i);
        let capture_falling = !value && (edge == EDGE_FALLING || edge == EDGE_BOTH);
        let capture_rising = value && (edge == EDGE_RISING || edge == EDGE_BOTH);
        if self.st.ch[i].mode != ccs::OUTPUT && (capture_falling || capture_rising) {
            self.handle_capture(io, i);
        }
        self.update_interrupts(io);
        self.st.ch[i].signal = value;
    }

    /// `CaptureCompareChannel.HandleCapture()`.
    fn handle_capture(&mut self, io: &mut dyn Io, i: usize) {
        let divider = 1u32 << self.input_prescaler(i);
        self.st.ch[i].edge_counter += 1;
        if self.st.ch[i].edge_counter < divider {
            return;
        }
        self.st.ch[i].edge_counter = 0;
        // The latch syncs the CPU time first (a no-op outside CPU accesses).
        self.sync(io);
        self.st.ch[i].captured = self.lt_value(MAIN) as u32;
        if self.st.ch[i].iflag {
            self.st.ch[i].overcapture = true;
        }
        self.st.ch[i].iflag = true;
        self.dirty = true;
    }

    /// `OnGPIO(number, value)`.
    pub(crate) fn on_input(&mut self, io: &mut dyn Io, number: u32, value: bool) {
        // NGCLazyPwmTimer.OnGPIO: `Restore(); externalInput = true;` before the stock handler (whose reset
        // input then clears the flag again through the virtual `Reset`).
        self.restore(io);
        self.st.external_input = true;
        if number == RESET_INPUT {
            if value {
                self.reset(io);
            }
            return;
        }
        if number >= 4 {
            io.log_once(LogLevel::Error, 0x1B00 + u64::from(number), format_args!("input {number} does not exist (the timer has 4 channel inputs)"));
            return;
        }
        let n = number as usize;
        if self.is_output_mode(n) {
            io.log(LogLevel::Noisy, format_args!("Channel #{n} received external input when configured as output"));
            return;
        }
        let old_pin = self.st.pins[n];
        // The channel's pin doubles as its input: the Connection GPIO is set to the received level.
        self.set_pin(io, n, value);
        let pins = self.st.pins;
        let timer_input = [if self.st.ti1s { pins[0] ^ pins[1] ^ pins[2] } else { pins[0] }, pins[1], pins[2], pins[3]];
        for i in 0..4 {
            let mode = self.st.ch[i].mode;
            if mode == ccs::INPUT_TI_SAME || mode == ccs::OUTPUT {
                self.set_input(io, i, timer_input[i]);
            } else if mode == ccs::INPUT_TI_CROSS {
                self.set_input(io, i, timer_input[i ^ 1]);
            }
        }
        if old_pin != value {
            self.handle_modes(io, n, value);
        }
        self.dirty = true;
    }

    /// `HandleModes(tiSource, value)`.
    fn handle_modes(&mut self, io: &mut dyn Io, ti_source: usize, value: bool) {
        if self.is_encoder_mode() {
            self.handle_encoder_mode(io, ti_source, value);
            return;
        }
        if !self.check_trigger_select(ti_source) {
            return;
        }
        match self.st.sms {
            sms::RESET => self.handle_reset_mode(io, ti_source, value),
            sms::GATED => self.handle_gated_mode(io, ti_source, value),
            sms::TRIGGER => self.handle_trigger_mode(io, ti_source, value),
            _ => {}
        }
    }

    fn handle_encoder_mode(&mut self, io: &mut dyn Io, ti_source: usize, value: bool) {
        let mode = self.st.sms;
        let current = self.lt_value(MAIN);
        if ti_source == 0 && (mode == sms::ENCODER1 || mode == sms::ENCODER3) {
            let next = if value ^ self.st.pins[1] { current.wrapping_add(1) } else { current.wrapping_sub(1) };
            self.lt_set_value(io, MAIN, next);
        } else if ti_source == 1 && (mode == sms::ENCODER2 || mode == sms::ENCODER3) {
            let next = if value ^ self.st.pins[0] { current.wrapping_sub(1) } else { current.wrapping_add(1) };
            self.lt_set_value(io, MAIN, next);
        }
    }

    fn handle_reset_mode(&mut self, io: &mut dyn Io, ti_source: usize, value: bool) {
        let value = if self.capture_edge(ti_source) == EDGE_FALLING { !value } else { value };
        if !value {
            return;
        }
        self.lt_set_value(io, MAIN, 0);
        if !self.st.tif && self.st.tie {
            self.st.tif = true;
            self.set_irq_output(io, 3, true);
        }
    }

    fn handle_gated_mode(&mut self, io: &mut dyn Io, ti_source: usize, value: bool) {
        let gated_signal = if self.st.ch[ti_source].polarity { !value } else { value };
        let enable = gated_signal && self.st.enable_requested && self.st.auto_reload > 0;
        self.lt_set_enabled(io, MAIN, enable);
    }

    fn handle_trigger_mode(&mut self, io: &mut dyn Io, ti_source: usize, value: bool) {
        let value = if self.capture_edge(ti_source) == EDGE_FALLING { !value } else { value };
        if value && !self.lt_enabled(MAIN) {
            let enable = self.st.enable_requested && self.st.auto_reload > 0;
            self.lt_set_enabled(io, MAIN, enable);
        }
    }

    fn check_trigger_select(&self, ti_source: usize) -> bool {
        (self.st.ts == TS_TIMER_INPUT1 && ti_source == 0) || (self.st.ts == TS_TIMER_INPUT2 && ti_source == 1)
    }

    // ---- Reset -------------------------------------------------------------------------------------

    /// `Reset()`: the counter and compare timers return to their initial entries, the registers to zero, the
    /// reload value to `initialLimit`, the outputs low.
    pub(crate) fn reset(&mut self, io: &mut dyn Io) {
        // NGCLazyPwmTimer.Reset(): Restore(), then the suppression state is cleared.
        self.restore(io);
        self.st.external_input = false;
        // base.Reset(): LimitTimer.InternalReset of the counter.
        self.lt_reset(io, MAIN);
        // registers.Reset(): every stored register field returns to 0 (the capture value survives).
        let st = &mut self.st;
        st.udis = false;
        st.urs = false;
        st.apre = false;
        st.cms = 0;
        st.ti1s = false;
        st.sms = 0;
        st.ts = 0;
        st.uie = false;
        st.tie = false;
        st.tif = false;
        st.rcr = 0;
        st.ccmr = [[0; 4]; 2];
        for c in &mut st.ch {
            c.polarity = false;
            c.comp_polarity = false;
            c.overcapture = false;
        }
        st.auto_reload = self.cfg.initial_limit;
        st.enable_requested = false;
        self.lt_set_limit(io, MAIN, u64::from(self.cfg.initial_limit));
        self.st.repetitions_left = 0;
        self.st.update_flag = false;
        for i in 0..4 {
            self.lt_reset(io, 1 + i);
            let c = &mut self.st.ch[i];
            c.iflag = false;
            c.ie = false;
            c.oe = false;
            c.edge_counter = 0;
            c.mode = ccs::OUTPUT;
            c.signal = false;
            self.set_pin(io, i, false);
        }
        self.update_interrupts(io);
        self.dirty = true;
    }
}
