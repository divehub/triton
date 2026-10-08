// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32_Timer.cs and
// src/Emulator/Main/Peripherals/Timers/LimitTimer.cs (MIT License, Copyright (c) Antmicro).

//! `Timers.STM32_Timer`: the general-purpose / basic timers of both boards (handset TIM2, TIM3, TIM6, TIM15;
//! main TIM4, TIM6, TIM7), event driven and lazily evaluated.
//!
//! Parameters (`.repl`): `frequency` (the prescaler input, 80 MHz) and `initialLimit` (0xFFFF or 0xFFFFFFFF,
//! which also fixes the counter width). Region size 0x400.
//!
//! # Behaviour (all of it Renode's, see `docs/renode-semantics.md` sections 3.5 and 11)
//!
//! * the counter is a `LimitTimer`: **period = ARR ticks** (it wraps at ARR, not ARR + 1), the entry runs at
//!   `frequency / (PSC + 1)` (integer division), a limit is reported at the **ceil-nanosecond** time and the
//!   next period restarts from that rounded nanosecond with the overshoot dropped;
//! * `CEN` takes effect only while `ARR > 0`; `UG` sets the counter to 0 (or ARR when counting down) but keeps
//!   the fractional-tick residuum; ARR is applied at the next update event when preloaded (`CR1.ARPE`);
//! * one one-shot compare timer per channel drives PWM/compare outputs and the CCxIF flags;
//! * `CNT` reads, input-capture latches and `SMCR` writes in trigger/encoder mode sync the CPU time
//!   (`sync_registers` / `ctx.sync_time()`), everything else sees the lagging clock time;
//! * unimplemented bits (`CR1.CKD`, `OCxPE`, `BDTR.MOE`, DMA enables, ...) are tags: writing 1s logs the Renode
//!   warning, reads give 0.
//!
//! # Performance and CPU chunk boundaries: [`Scheduling`]
//!
//! Renode's stock model owns a clock-source event for every update and every compare match, which made the
//! handset's TIM2 (about 40 kHz) and TIM15 (about 400 kHz) with `DIER = 0` and unconnected outputs 17 times
//! slower. Here the state is derived from time on every access and the machine event that stands for the
//! timer's limits (one clock entry per timer, the *alarm*) is armed according to the [`Scheduling`] policy;
//! whatever is not an event is replayed exactly when something looks at the timer, with whole steady-state
//! periods skipped in closed form. The design and its exactness argument are in `timer/model.rs`.
//!
//! The events matter beyond their handlers: the CPU plans each chunk against the event queue, and the clock
//! time that lags the CPU inside a chunk (what `ctx.now()` returns to every access) restarts at chunk
//! boundaries. Register values and output edges do not depend on the policy, but the guest-visible timing of
//! the other peripherals does (a DWT counter enabled 6 us after the PWM timer started differs by hundreds of
//! cycles). Therefore the default, [`Scheduling::NgcArithmeticPwm`], is a port of the handset's
//! `NGCLazyPwmTimer` in arithmetic mode (the runner's default for TIM2 and TIM15): every limit is an event
//! until a normal overflow finds the configuration eligible (`TrySuppress`), then the timer owns no event until
//! the next register write, input edge or reset (`Restore`). [`Scheduling::Stock`] (every limit, for the
//! plain `STM32_Timer` instances: the main board's TIM4/TIM6/TIM7, the handset's TIM3/TIM6) and
//! [`Scheduling::Observable`] (only the limits that touch an interrupt output or an observed pin, the fastest;
//! chunk boundaries differ from Renode's) are selectable with [`Stm32Timer::with_scheduling`].
//! `ngc/tests/micro_pwm` pins the clock-source effect down against Renode.
//!
//! Configurations that always schedule events: an enabled update interrupt (one event per period, e.g. the
//! TIM6 HAL tick), enabled capture/compare interrupts (one per compare match), observed pins (one per edge),
//! and under `Observable` anything the planner cannot prove periodic (a bounded wake-up every 128 instants).
//!
//! # Lines
//!
//! Outputs: [`IRQ_LINE`] 0 (the `.repl`'s `-> nvic@n`), 1 break, 2 update, 3 trigger, 4 commutation, 5 capture
//! compare, and the channel pins [`PIN_LINE_BASE`]` + i` (Renode's `Connections[i]`). Inputs: 0..=3 are the
//! channel inputs TI1..TI4 (`buttons.PE3 -> timer3@0`), [`RESET_INPUT`] (0xFF) resets the timer.

mod io;
mod logic;
mod model;
mod regs;

pub use logic::RESET_INPUT;
pub use model::{Scheduling, TimerStats, BREAK_LINE, CAPTURE_COMPARE_LINE, COMMUTATION_LINE, IRQ_LINE, PIN_LINE_BASE, TRIGGER_LINE, UPDATE_LINE};
pub use regs::reg;

use emu_core::{
    impl_peripheral_any, AccessPolicy, ClockEntry, ClockId, Ctx, Direction, Peripheral, SyncRegister, Time, Translations, View,
    WorkMode, Width, TICKS_PER_SECOND,
};
use io::CtxIo;
use model::{Cfg, Model, MAIN};

/// Size of the register window.
pub const SIZE: u32 = 0x400;

/// Event token of the alarm.
const ALARM: u64 = 1;

/// The inputs a PWM duty indicator needs for one channel (`NGCBoardTelemetry` reads the same registers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PwmChannel {
    /// `CR1.CEN`.
    pub counter_enabled: bool,
    /// `CCER.CCxE`.
    pub output_enabled: bool,
    /// `CCMRx.OCxM` (6 = PWM mode 1, 7 = PWM mode 2).
    pub mode: u32,
    /// The channel is in output mode (`CCxS == 0`).
    pub output_mode: bool,
    /// `CCER.CCxP`.
    pub polarity_inverted: bool,
    /// ARR, the modelled period in timer ticks.
    pub period: u32,
    /// CCR (the compare value; in capture mode the captured value, not consumed).
    pub compare: u32,
}

pub struct Stm32Timer {
    name: String,
    model: Model,
    alarm: ClockId,
    /// The time the alarm is armed for.
    armed: Option<Time>,
}

impl Stm32Timer {
    /// `new STM32_Timer(machine, frequency, initialLimit)`. Panics like the Renode constructor throws when
    /// `initial_limit` is zero.
    pub fn new(name: impl Into<String>, frequency: u64, initial_limit: u32) -> Self {
        assert!(initial_limit > 0, "initialLimit has to be greater than zero");
        assert!(frequency > 0, "Frequency must be greater than 0");
        let bits = 32 - initial_limit.leading_zeros();
        let cfg = Cfg { frequency, initial_limit, bits };
        Self { name: name.into(), model: Model::new(cfg, 0, 0), alarm: ClockId::NONE, armed: None }
    }

    /// Declares which channel pins have a receiver (bit `i` = channel `i + 1`): their edges are delivered at
    /// the time they happen, which needs one real event per edge. Pins that nobody observes are evaluated
    /// lazily. Call it before the timer is added to a machine, or use [`Stm32Timer::observe_pin`].
    pub fn with_observed_pins(mut self, mask: u8) -> Self {
        self.model.observed = mask & 0xF;
        self
    }

    /// Changes the observed state of a channel pin at run time (re-plans the alarm).
    pub fn observe_pin(&mut self, ctx: &mut Ctx<'_>, channel: usize, observed: bool) {
        assert!(channel < 4, "the timer has four channels");
        let bit = 1u8 << channel;
        {
            let mut io = CtxIo { ctx };
            self.model.begin(&mut io);
        }
        if observed {
            self.model.observed |= bit;
        } else {
            self.model.observed &= !bit;
        }
        // A topology change brings the stock events back (NGC: connect outputs only with the suppression off).
        {
            let mut io = CtxIo { ctx };
            self.model.restore(&mut io);
        }
        self.model.periodic = None;
        self.model.pin_flush |= bit;
        self.model.dirty = true;
        self.finish(ctx);
    }

    /// Chooses which limits are real machine events (see [`Scheduling`]). The default,
    /// [`Scheduling::NgcArithmeticPwm`], is what the handset's TIM2 and TIM15 are in Renode (`NGCLazyPwmTimer`
    /// with its arithmetic mode on, the runner's default); the other timers of both boards are the plain
    /// `STM32_Timer`, i.e. [`Scheduling::Stock`]. Register values and output edges are identical under every
    /// policy; the policy decides the CPU's chunk boundaries and with them the clock-source time seen by
    /// accesses. Call it before the timer is added to a machine.
    pub fn with_scheduling(mut self, scheduling: Scheduling) -> Self {
        self.model.sched = scheduling;
        self
    }

    /// The scheduling policy.
    pub fn scheduling(&self) -> Scheduling {
        self.model.sched
    }

    /// Whether the NGC suppression is engaged right now: the timer owns no machine event until the next
    /// register write, input edge or reset (`NGCLazyPwmTimer.ArithmeticSuppressed`).
    pub fn suppressed(&self) -> bool {
        self.model.st.engaged
    }

    /// Test hook: `true` schedules every limit as a real event like the stock Renode model
    /// ([`Scheduling::Stock`]), `false` only the observable ones ([`Scheduling::Observable`]).
    #[doc(hidden)]
    pub fn set_naive_events(&mut self, naive: bool) {
        self.model.sched = if naive { Scheduling::Stock } else { Scheduling::Observable };
    }

    /// Counters of the lazy machinery (tests, diagnostics).
    pub fn stats(&self) -> TimerStats {
        self.model.stats
    }

    /// The time the alarm is armed for, if any (the next externally visible limit).
    pub fn armed_alarm(&self) -> Option<Time> {
        self.armed
    }

    /// Side-effect-free value of the register at `offset` at clock time `now` (model events up to `now` are
    /// replayed on a copy).
    pub fn register_at(&self, offset: u32, now: Time) -> Option<u32> {
        self.model.peek_register(offset, now)
    }

    /// `CNT` at `now`, without side effects.
    pub fn counter_at(&self, now: Time) -> u32 {
        self.model.peek_register(reg::CNT, now).unwrap_or(0)
    }

    /// Level of channel pin `channel` (Renode `Connections[channel].IsSet`) at `now`.
    pub fn pin_level_at(&self, channel: usize, now: Time) -> bool {
        assert!(channel < 4, "the timer has four channels");
        match self.model.replayed(now) {
            Some(copy) => copy.st.pins[channel],
            None => self.model.st.pins[channel],
        }
    }

    /// Level of the interrupt output `IRQ` at `now`.
    pub fn irq_level_at(&self, now: Time) -> bool {
        match self.model.replayed(now) {
            Some(copy) => copy.st.irq[0],
            None => self.model.st.irq[0],
        }
    }

    /// The registers a PWM duty indicator reads for `channel` (0-based), at `now`, without side effects.
    pub fn pwm_channel(&self, channel: usize, now: Time) -> PwmChannel {
        assert!(channel < 4, "the timer has four channels");
        let word = |offset: u32| self.model.peek_register(offset, now).unwrap_or(0);
        let ccmr = word(if channel < 2 { reg::CCMR1 } else { reg::CCMR2 });
        let shift = (channel % 2) as u32 * 8;
        let ccer = word(reg::CCER);
        let output_mode = (ccmr >> shift) & 3 == 0;
        PwmChannel {
            counter_enabled: word(reg::CR1) & 1 != 0,
            output_enabled: ccer & (1 << (4 * channel)) != 0,
            mode: (ccmr >> (shift + 4)) & 7,
            output_mode,
            polarity_inverted: ccer & (2 << (4 * channel)) != 0,
            period: word(reg::ARR),
            compare: if output_mode { word(reg::CCR1 + 4 * channel as u32) } else { 0 },
        }
    }

    /// Pushes the levels of unobserved pins to the machine and re-plans the alarm when needed.
    fn finish(&mut self, ctx: &mut Ctx<'_>) {
        let flush = std::mem::take(&mut self.model.pin_flush);
        if flush != 0 {
            for i in 0..4 {
                if flush & (1 << i) != 0 {
                    ctx.set_output(PIN_LINE_BASE + i as u32, self.model.st.pins[i]);
                }
            }
        }
        if self.model.dirty {
            self.model.dirty = false;
            self.replan(ctx);
        }
    }

    /// Plans the next real event and arms (or disarms) the alarm for it.
    fn replan(&mut self, ctx: &mut Ctx<'_>) {
        let mut guard = 0;
        loop {
            let want = self.model.plan();
            let now = ctx.now();
            if let Some(t) = want {
                if t <= now && guard < 8 {
                    // An event is already due: run it as a real one now.
                    guard += 1;
                    let mut io = CtxIo { ctx };
                    self.model.catch_up(&mut io, now, t);
                    continue;
                }
            }
            self.arm(ctx, want);
            break;
        }
    }

    fn arm(&mut self, ctx: &mut Ctx<'_>, want: Option<Time>) {
        if self.armed == want {
            return;
        }
        // The CPU plans its chunk against the event queue when the chunk starts. With elided events, a write
        // that makes an event due earlier than anything planned must end the chunk, or the interrupt would
        // reach the CPU only at the chunk's end. Under the stock policies every limit is queued anyway, and the
        // only chunk ends are the `RequestReturn` calls the stock setters make (reproduced by the model).
        if let Some(t) = want {
            if self.model.sched == Scheduling::Observable && self.armed.map_or(true, |armed| t < armed) {
                ctx.request_return();
            }
        }
        let entry = match want {
            Some(t) => ClockEntry::new(t.saturating_sub(ctx.now()), TICKS_PER_SECOND, true, Direction::Ascending, WorkMode::OneShot),
            None => ClockEntry::new(1, TICKS_PER_SECOND, false, Direction::Ascending, WorkMode::OneShot),
        };
        ctx.clock_replace(self.alarm, entry);
        self.armed = want;
    }
}

impl Peripheral for Stm32Timer {
    fn name(&self) -> &str {
        &self.name
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.alarm = ctx.clock_add(ClockEntry::new(1, TICKS_PER_SECOND, false, Direction::Ascending, WorkMode::OneShot), ALARM);
        let (cfg, observed, sched) = (self.model.cfg, self.model.observed, self.model.sched);
        self.model = Model::new(cfg, observed, ctx.now());
        self.model.sched = sched;
        self.armed = None;
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        {
            let mut io = CtxIo { ctx };
            self.model.begin(&mut io);
            self.model.periodic = None;
            let saved = self.model.emit_return;
            self.model.emit_return = true;
            self.model.reset(&mut io);
            self.model.emit_return = saved;
        }
        self.finish(ctx);
    }

    fn access_policy(&self) -> AccessPolicy {
        // [AllowedTranslations(ByteToDoubleWord | WordToDoubleWord)] on an IDoubleWordPeripheral.
        AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD)
    }

    fn sync_registers(&self) -> Vec<SyncRegister> {
        // `CNT` is read after `cpu.SyncTime()`.
        vec![SyncRegister::read(reg::CNT)]
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        let value = {
            let mut io = CtxIo { ctx };
            self.model.read(&mut io, offset)
        };
        self.finish(ctx);
        value
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        {
            let mut io = CtxIo { ctx };
            self.model.write(&mut io, offset, value);
        }
        self.finish(ctx);
    }

    fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
        if token != ALARM {
            return;
        }
        self.armed = None;
        self.model.stats.alarms += 1;
        {
            let mut io = CtxIo { ctx };
            // The limits before the alarm's instant were elided; the instant itself is a real event.
            self.model.catch_up(&mut io, scheduled, scheduled);
            let now = io.ctx.now();
            self.model.catch_up(&mut io, now, Time::MAX);
        }
        self.model.dirty = true;
        self.finish(ctx);
    }

    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        {
            let mut io = CtxIo { ctx };
            self.model.begin(&mut io);
            self.model.periodic = None;
            self.model.dirty = true;
            let saved = self.model.emit_return;
            self.model.emit_return = true;
            self.model.on_input(&mut io, line, level);
            self.model.emit_return = saved;
        }
        self.finish(ctx);
    }

    fn peek(&self, offset: u32, _width: Width, view: &View<'_>) -> Option<u32> {
        self.model.peek_register(offset, view.now())
    }

    fn summary(&self, view: &View<'_>) -> String {
        let now = view.now();
        let word = |offset: u32| self.model.peek_register(offset, now).unwrap_or(0);
        let cr1 = word(reg::CR1);
        let sr = word(reg::SR);
        let divider = u64::from(word(reg::PSC)) + 1;
        let main = self.model.replayed(now);
        let model = main.as_ref().unwrap_or(&self.model);
        let entry = model.entry_at(MAIN, now);
        format!(
            "{}: CEN={} OPM={} DIR={} PSC={} ARR={} CNT={} entry={}Hz limit={} DIER=0x{:X} SR=0x{:X} CCER=0x{:X} \
             IRQ={} pins={}{}{}{} alarm={}",
            self.name,
            cr1 & 1,
            (cr1 >> 3) & 1,
            (cr1 >> 4) & 1,
            divider - 1,
            word(reg::ARR),
            word(reg::CNT),
            self.model.cfg.frequency / divider,
            entry.period(),
            word(reg::DIER),
            sr,
            word(reg::CCER),
            u8::from(model.st.irq[0]),
            u8::from(model.st.pins[0]),
            u8::from(model.st.pins[1]),
            u8::from(model.st.pins[2]),
            u8::from(model.st.pins[3]),
            match self.armed {
                Some(t) => format!("{t}ns"),
                None => "none".to_string(),
            },
        )
    }

    impl_peripheral_any!();
}
