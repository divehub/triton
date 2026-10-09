//! `NGCHandsetButtons` driving the handset's TIM3 capture inputs (`timer3@0`, `timer3@2`): the 100 us stimulus grid,
//! press/pulse/confirm timing, busy errors, the summary text, and the capture values the guest reads back (an accepted
//! 250-count pulse and a rejected 100-count pulse at PSC 65535), in the two modes of the model.
//!
//! * the **gated** Renode model (`Buttons::gated`, a plain `Stm32Timer`): readiness, the not-ready refusal, the idle-high edges after the
//!   capture configuration (most tests below, through `Rig::new()`);
//! * the default **pull-up** model (`Buttons::new` with `Stm32Timer::with_external_pull_ups`, DESIGN.md 20.3): the pins are high from
//!   reset, a press is accepted before the firmware configures anything and nothing is captured at the configuration
//!   (`Rig::pull_up()`, the tests named `pull_up_*`).

use emu_core::testing::{Harness, Probe};
use emu_core::{PeriphId, Time, TICKS_PER_MICROSECOND, TICKS_PER_MILLISECOND};
use ngc::models::buttons::{Buttons, ButtonsError, PE3_LINE, PE5_LINE};
use stm32::timer::{reg, Stm32Timer, IRQ_LINE};

const US: Time = TICKS_PER_MICROSECOND;
const MS: Time = TICKS_PER_MILLISECOND;
const TIM3: u32 = 0x4000_0400;
const BUTTONS: u32 = 0x6100_0200;
const TIM3_IRQ: u32 = 29;

struct Rig {
    h: Harness,
    timer: PeriphId,
    buttons: PeriphId,
    pe3: Probe,
    pe5: Probe,
}

impl Rig {
    /// The gated Renode model with a plain timer.
    fn new() -> Rig {
        Rig::build(Stm32Timer::new("timer3", 80_000_000, 0xFFFF), Buttons::gated("buttons", TIM3))
    }

    /// The default: the pins rest high from reset, the timer remembers the level of a pin it sees before its capture is configured.
    fn pull_up() -> Rig {
        Rig::build(Stm32Timer::new("timer3", 80_000_000, 0xFFFF).with_external_pull_ups(), Buttons::new("buttons"))
    }

    fn build(timer: Stm32Timer, buttons: Buttons) -> Rig {
        let mut h = Harness::new();
        let timer = h.add_mapped(TIM3, 0x400, timer);
        h.connect_irq(timer, IRQ_LINE, TIM3_IRQ);
        let buttons = h.add_mapped(BUTTONS, 0x100, buttons);
        let pe3 = h.probe(buttons, PE3_LINE);
        let pe5 = h.probe(buttons, PE5_LINE);
        h.connect_input(buttons, PE3_LINE, timer, 0);
        h.connect_input(buttons, PE5_LINE, timer, 2);
        h.clear_irq_changes();
        Rig { h, timer, buttons, pe3, pe5 }
    }

    fn tim(&mut self, offset: u32, value: u32) {
        self.h.write32(TIM3 + offset, value);
    }

    fn tim_read(&mut self, offset: u32) -> u32 {
        self.h.read32(TIM3 + offset)
    }

    /// The firmware's TIM3 setup: 80 MHz / 65536, channels 1/2 and 3/4 capturing TI1 / TI3 on rising and
    /// falling edges, all four capture interrupts enabled.
    fn configure_capture(&mut self) {
        self.tim(reg::PSC, 65_535);
        self.tim(reg::ARR, 0xFFFF);
        self.tim(reg::CCMR1, 0x0201);
        self.tim(reg::CCMR2, 0x0201);
        self.tim(reg::CCER, 1 | 1 << 4 | 1 << 5 | 1 << 8 | 1 << 12 | 1 << 13);
        self.tim(reg::DIER, 0x1E);
        self.tim(reg::CR1, 1);
    }

    fn buttons(&self) -> &Buttons {
        self.h.get::<Buttons>(self.buttons)
    }

    fn press(&mut self, mask: u32) -> Result<(), ButtonsError> {
        self.h.with::<Buttons, _>(self.buttons, |b, _| b.press(mask))
    }

    fn pulse(&mut self, mask: u32, us: u32) -> Result<(), ButtonsError> {
        self.h.with::<Buttons, _>(self.buttons, |b, _| b.pulse(mask, us))
    }

    fn confirm(&mut self) -> Result<(), ButtonsError> {
        self.h.with::<Buttons, _>(self.buttons, |b, _| b.confirm())
    }

    /// Waits for the readiness tick and consumes the two zero-width idle-high capture edges.
    fn make_ready(&mut self) {
        self.configure_capture();
        self.h.advance_to(100 * US);
        assert!(self.buttons().ready());
        for ccr in [reg::CCR1, reg::CCR3] {
            self.tim_read(ccr);
        }
    }
}

/// `CNT` at `t` for a timer enabled at time 0: 1220 Hz (80 MHz / 65536 by integer division) from the clock entry.
fn cnt_at(t: Time) -> u32 {
    (u128::from(t) * 61 / 50_000_000) as u32
}

#[test]
fn not_ready_until_tim3_is_configured_for_capture() {
    let mut rig = Rig::new();
    assert_eq!(rig.press(1), Err(ButtonsError::NotReady));
    rig.h.advance_to(5 * MS);
    assert!(!rig.buttons().ready());
    assert_eq!(rig.h.read32(BUTTONS), 0);
    // CEN alone is not enough, the mapping must be the capture one for both pairs.
    rig.tim(reg::CCMR1, 0x0201);
    rig.tim(reg::CR1, 1);
    rig.h.advance_to(6 * MS);
    assert!(!rig.buttons().ready());
    rig.configure_capture();
    // The next 100 us tick establishes idle-high.
    let t = rig.h.now();
    rig.h.advance_to(t + 100 * US);
    assert!(rig.buttons().ready());
    assert_eq!(rig.h.read32(BUTTONS), 1);
    assert_eq!(rig.h.probe_changes(rig.pe3), vec![(6 * MS + 100 * US, true)]);
    assert_eq!(rig.h.probe_changes(rig.pe5), vec![(6 * MS + 100 * US, true)]);
}

#[test]
fn idle_high_produces_zero_width_capture_edges() {
    let mut rig = Rig::new();
    rig.configure_capture();
    rig.h.advance_to(100 * US);
    // Rising edges on TI1 and TI3 capture CNT = 0 into CCR1 and CCR3; the interrupt line follows the flags.
    assert!(rig.h.irq_level(TIM3_IRQ));
    assert_eq!(rig.h.irq_changes().len(), 1);
    assert_eq!(rig.tim_read(reg::SR) & 0x1E, 0b01010, "CC1IF (bit 1) and CC3IF (bit 3)");
    assert_eq!(rig.tim_read(reg::CCR1), 0);
    assert_eq!(rig.tim_read(reg::CCR3), 0);
    assert!(!rig.h.irq_level(TIM3_IRQ), "reading the capture registers cleared the flags");
}

#[test]
fn press_produces_a_204_8_ms_low_pulse_that_the_timer_measures() {
    let mut rig = Rig::new();
    rig.make_ready();
    // Start on a 100 us tick after some idle time.
    rig.h.advance_to(10 * MS);
    rig.press(1).unwrap();
    assert!(rig.buttons().busy());
    assert_eq!(rig.press(2), Err(ButtonsError::Busy));
    rig.h.advance_to(10 * MS + 100 * US);
    // The first tick after queueing starts the gesture: PE3 falls on the 10.1 ms grid point.
    let fall = 10 * MS + 100 * US;
    rig.h.advance_to(fall + 204_800 * US - 1);
    assert_eq!(rig.buttons().active_mask(), 1);
    rig.h.advance_to(fall + 204_800 * US);
    assert_eq!(rig.buttons().active_mask(), 0);
    assert_eq!(rig.buttons().pulse_count(), 1);
    assert_eq!(rig.buttons().release_count(), 1);
    assert_eq!(rig.h.probe_changes(rig.pe3).iter().skip(1).copied().collect::<Vec<_>>(), vec![(fall, false), (fall + 204_800 * US, true)]);
    assert_eq!(rig.h.probe_changes(rig.pe5).len(), 1, "PE5 only had the idle-high edge");
    // CH2 captured the falling edge, CH1 the rising edge: the pulse measures 250 (or 249) counts of 1220 Hz.
    let falling = rig.tim_read(reg::CCR2);
    let rising = rig.tim_read(reg::CCR1);
    assert_eq!(falling, cnt_at(fall));
    assert_eq!(rising, cnt_at(fall + 204_800 * US));
    let width = rising - falling;
    assert!((249..=250).contains(&width), "{width}");
    assert!(width >= 150 && width <= 700, "accepted by the firmware's 150..700 count window");
    let ccer = rig.tim_read(reg::CCER);
    assert_eq!(ccer & 0x33, 0x31);
    assert!(rig.h.warnings().is_empty(), "{:?}", rig.h.warnings());
    let _ = rig.timer;
}

#[test]
fn a_100_count_pulse_is_rejected_by_the_window() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.h.advance_to(2 * MS);
    // 100 counts of 1220 Hz = 81.97 ms; request 82 000 us (a multiple of the 100 us clock).
    rig.pulse(1, 82_000).unwrap();
    rig.h.advance_to(2 * MS + 100 * US + 82_000 * US);
    let falling = rig.tim_read(reg::CCR2);
    let rising = rig.tim_read(reg::CCR1);
    let width = rising - falling;
    assert!((100..=101).contains(&width), "{width}");
    assert!(width < 150, "below the firmware's accepted window");
}

#[test]
fn the_phase_decides_between_249_and_250_counts() {
    // Start late in a count so that 204.8 ms (249.856 counts of 1220 Hz) crosses 249 or 250 boundaries.
    let mut widths = std::collections::BTreeSet::new();
    for offset_us in (0..820).step_by(100) {
        let mut rig = Rig::new();
        rig.make_ready();
        rig.h.advance_to(10 * MS + offset_us * US);
        rig.press(1).unwrap();
        rig.h.advance_to(10 * MS + offset_us * US + 100 * US + 204_800 * US);
        let falling = rig.tim_read(reg::CCR2);
        let rising = rig.tim_read(reg::CCR1);
        let fall_time = 10 * MS + offset_us * US + 100 * US;
        assert_eq!(rising - falling, cnt_at(fall_time + 204_800 * US) - cnt_at(fall_time));
        widths.insert(rising - falling);
    }
    assert!(widths.iter().all(|w| (249..=250).contains(w)), "{widths:?}");
}

#[test]
fn pe5_press_uses_the_second_capture_pair() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.h.advance_to(MS);
    rig.press(2).unwrap();
    rig.h.advance_to(MS + 100 * US + 204_800 * US);
    let falling = rig.tim_read(reg::CCR4);
    let rising = rig.tim_read(reg::CCR3);
    assert_eq!(rising - falling, cnt_at(MS + 100 * US + 204_800 * US) - cnt_at(MS + 100 * US));
    assert_eq!(rig.h.probe_changes(rig.pe3).len(), 1, "PE3 stayed high");
}

#[test]
fn simultaneous_press_drives_both_pins_on_the_same_tick() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.h.advance_to(MS);
    rig.press(3).unwrap();
    rig.h.advance_to(MS + 100 * US);
    assert_eq!(rig.buttons().active_mask(), 3);
    rig.h.advance_to(MS + 100 * US + 204_800 * US);
    let t3 = rig.h.probe_changes(rig.pe3);
    let t5 = rig.h.probe_changes(rig.pe5);
    assert_eq!(&t3[1..], &[(MS + 100 * US, false), (MS + 100 * US + 204_800 * US, true)]);
    assert_eq!(&t5[1..], &t3[1..]);
    assert_eq!(rig.buttons().pulse_count(), 1, "one gesture, not one per pin");
}

#[test]
fn confirm_staggers_pe5_by_50_ms_and_keeps_each_pin_low_for_204_8_ms() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.h.advance_to(MS);
    rig.confirm().unwrap();
    assert!(rig.buttons().summary_text().contains("gesture=staggered-confirm"));
    assert_eq!(rig.press(1), Err(ButtonsError::Busy));
    let start = MS + 100 * US;
    rig.h.advance_to(start + 50 * MS + 204_800 * US + 10 * MS);
    let t3 = rig.h.probe_changes(rig.pe3);
    let t5 = rig.h.probe_changes(rig.pe5);
    // 500 ticks between the falling edges, 2048 ticks low each (the second pin's width starts when it starts).
    assert_eq!(&t3[1..], &[(start, false), (start + 204_800 * US, true)]);
    assert_eq!(&t5[1..], &[(start + 50 * MS, false), (start + 50 * MS + 204_800 * US, true)]);
    assert_eq!(rig.buttons().pulse_count(), 1);
    assert_eq!(rig.buttons().release_count(), 1);
    assert!(!rig.buttons().busy());
}

#[test]
fn argument_validation_and_busy_state() {
    let mut rig = Rig::new();
    rig.make_ready();
    assert_eq!(rig.press(0), Err(ButtonsError::MaskOutOfRange));
    assert_eq!(rig.press(4), Err(ButtonsError::MaskOutOfRange));
    assert_eq!(rig.pulse(1, 0), Err(ButtonsError::DurationOutOfRange));
    assert_eq!(rig.pulse(1, 2_000_001), Err(ButtonsError::DurationOutOfRange));
    assert_eq!(ButtonsError::Busy.to_string(), "A button pulse is already in progress");
    assert_eq!(ButtonsError::NotReady.to_string(), "TIM3 capture inputs are not ready");
    assert!(ButtonsError::MaskOutOfRange.to_string().contains("(Parameter 'mask')"));
    // The shortest pulse is one 100 us tick (rounded up from 1 us).
    rig.pulse(1, 1).unwrap();
    rig.h.advance_to(rig.h.now() + 400 * US);
    assert!(!rig.buttons().busy());
    assert_eq!(rig.buttons().pulse_count(), 1);
    rig.pulse(2, 100).unwrap();
    assert_eq!(rig.pulse(1, 100), Err(ButtonsError::Busy));
}

#[test]
fn summary_has_the_csharp_format() {
    let mut rig = Rig::new();
    assert_eq!(
        rig.buttons().summary_text(),
        "ready=False; activeMask=0; pendingMask=0; pulses=0; releases=0; PE3=False; PE5=False; gesture=idle; \
         confirmStaggerUs=50000; delayedStartTicks=0; remainingPE3Ticks=0; remainingPE5Ticks=0"
    );
    rig.make_ready();
    assert!(rig.buttons().summary_text().starts_with("ready=True; activeMask=0; pendingMask=0; pulses=0; releases=0; PE3=True; PE5=True; gesture=idle;"));
    rig.confirm().unwrap();
    assert!(rig.buttons().summary_text().contains("pendingMask=3; pulses=0; releases=0; PE3=True; PE5=True; gesture=staggered-confirm; confirmStaggerUs=50000; delayedStartTicks=500;"));
    rig.h.advance_to(rig.h.now() + 100 * US);
    let text = rig.buttons().summary_text();
    assert!(text.contains("activeMask=1; pendingMask=2; pulses=1; releases=0; PE3=False; PE5=True; gesture=staggered-confirm"), "{text}");
    assert!(text.contains("delayedStartTicks=500; remainingPE3Ticks=2048; remainingPE5Ticks=0"), "{text}");
    let summaries = rig.h.core().summaries();
    assert!(summaries.iter().any(|(n, s)| n == "buttons" && s == &text));
}

#[test]
fn reset_returns_the_pins_low_and_needs_a_new_readiness() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.h.core_mut().reset_all();
    assert!(!rig.buttons().ready());
    assert!(rig.h.output(rig.buttons, PE3_LINE) == false);
    assert_eq!(rig.press(1), Err(ButtonsError::NotReady));
    // The thread keeps running: TIM3 was reset too, so it is not configured any more.
    rig.h.advance_to(rig.h.now() + MS);
    assert!(!rig.buttons().ready());
}

#[test]
fn registers_report_state() {
    let mut rig = Rig::new();
    rig.make_ready();
    rig.press(1).unwrap();
    rig.h.advance_to(rig.h.now() + 300 * US);
    assert_eq!(rig.h.read32(BUTTONS), 1 | 1 << 8);
    assert_eq!(rig.h.read32(BUTTONS + 4), 1);
    assert_eq!(rig.h.read32(BUTTONS + 8), 0);
    assert_eq!(rig.h.read32(BUTTONS + 0x20), 0);
    rig.h.write32(BUTTONS, 0xFFFF_FFFF);
    assert_eq!(rig.h.read32(BUTTONS), 1 | 1 << 8);
}

// ---- the default pull-up model (DESIGN.md 20.3) ------------------------------------------------------------------------

#[test]
fn pull_up_pins_rest_high_from_the_attach_and_a_press_needs_no_configuration() {
    let mut rig = Rig::pull_up();
    assert!(rig.h.output(rig.buttons, PE3_LINE) && rig.h.output(rig.buttons, PE5_LINE), "an external pull-up holds both pins high");
    assert!(rig.buttons().ready());
    assert_eq!(rig.h.read32(BUTTONS), 1);
    assert!(rig.buttons().summary_text().starts_with("ready=True; activeMask=0; pendingMask=0; pulses=0; releases=0; PE3=True; PE5=True; gesture=idle;"));
    // The firmware has configured nothing: the press is accepted anyway (no not-ready refusal), the timer ignores it, no interrupt.
    rig.h.advance_to(3 * MS);
    rig.press(1).unwrap();
    assert_eq!(rig.press(2), Err(ButtonsError::Busy));
    rig.h.advance_to(300 * MS);
    let fall = 3 * MS + 100 * US;
    assert_eq!(rig.h.probe_changes(rig.pe3), vec![(fall, false), (fall + 204_800 * US, true)]);
    assert!(rig.h.probe_changes(rig.pe5).is_empty());
    assert_eq!((rig.buttons().pulse_count(), rig.buttons().release_count()), (1, 1));
    assert!(rig.h.irq_changes().is_empty());
}

#[test]
fn pull_up_configuring_the_capture_with_the_pins_high_produces_no_edge_and_a_press_is_measured() {
    let mut rig = Rig::pull_up();
    rig.configure_capture();
    rig.h.advance_to(10 * MS);
    assert!(rig.h.irq_changes().is_empty(), "no zero-width capture edge after the configuration");
    assert_eq!(rig.tim_read(reg::SR) & 0x1E, 0);
    rig.press(1).unwrap();
    let fall = 10 * MS + 100 * US;
    rig.h.advance_to(fall + 204_800 * US);
    let falling = rig.tim_read(reg::CCR2);
    let first_rising = rig.tim_read(reg::CCR1);
    assert_eq!(falling, cnt_at(fall));
    assert_eq!(first_rising, cnt_at(fall + 204_800 * US));
    assert!((249..=250).contains(&(first_rising - falling)), "{}", first_rising - falling);
    assert_eq!(rig.tim_read(reg::CCR3), 0, "the other pair never captured");
    // PE5 uses the second pair.
    rig.h.advance_to(300 * MS);
    rig.press(2).unwrap();
    let fall = 300 * MS + 100 * US;
    rig.h.advance_to(fall + 204_800 * US);
    let (falling, rising) = (rig.tim_read(reg::CCR4), rig.tim_read(reg::CCR3));
    assert_eq!((falling, rising), (cnt_at(fall), cnt_at(fall + 204_800 * US)));
    assert_eq!(rig.tim_read(reg::CCR1), first_rising, "PE5 did not touch the first pair");
}

#[test]
fn pull_up_a_press_before_the_configuration_is_invisible_to_the_timer_and_the_next_one_is_captured() {
    let mut rig = Rig::pull_up();
    rig.h.advance_to(MS);
    rig.press(1).unwrap();
    rig.h.advance_to(250 * MS);
    rig.configure_capture();
    rig.h.advance_to(260 * MS);
    assert_eq!(rig.tim_read(reg::SR) & 0x1E, 0, "the earlier press left no trace");
    rig.press(2).unwrap();
    let fall = 260 * MS + 100 * US;
    rig.h.advance_to(fall + 204_800 * US);
    assert!((249..=250).contains(&(rig.tim_read(reg::CCR3) - rig.tim_read(reg::CCR4))));
}

#[test]
fn pull_up_a_reset_pulls_the_pins_up_again_whatever_state_the_gesture_was_in() {
    let mut rig = Rig::pull_up();
    rig.configure_capture();
    rig.h.advance_to(10 * MS);
    rig.confirm().unwrap();
    rig.h.advance_to(10 * MS + 100 * US + 60 * MS);
    assert!(!rig.h.output(rig.buttons, PE3_LINE) && !rig.h.output(rig.buttons, PE5_LINE), "both keys are down");
    rig.h.core_mut().reset_all();
    assert!(rig.h.output(rig.buttons, PE3_LINE) && rig.h.output(rig.buttons, PE5_LINE));
    assert!(!rig.buttons().busy());
    assert_eq!(rig.h.read32(BUTTONS), 1);
    // The timer was reset too (output mode, no flags); the firmware configures it again and the next press is captured.
    rig.h.advance_to(100 * MS);
    rig.configure_capture();
    rig.h.advance_to(110 * MS);
    assert_eq!(rig.tim_read(reg::SR) & 0x1E, 0);
    rig.press(1).unwrap();
    let fall = 110 * MS + 100 * US;
    rig.h.advance_to(fall + 204_800 * US);
    let width = rig.tim_read(reg::CCR1) - rig.tim_read(reg::CCR2);
    assert!((249..=250).contains(&width), "{width}");
}

#[test]
fn pull_up_confirm_staggers_pe5_by_50_ms_exactly_like_the_gated_model() {
    let mut rig = Rig::pull_up();
    rig.configure_capture();
    rig.h.advance_to(MS);
    rig.confirm().unwrap();
    let start = MS + 100 * US;
    rig.h.advance_to(start + 50 * MS + 204_800 * US + 10 * MS);
    assert_eq!(rig.h.probe_changes(rig.pe3), vec![(start, false), (start + 204_800 * US, true)]);
    assert_eq!(rig.h.probe_changes(rig.pe5), vec![(start + 50 * MS, false), (start + 50 * MS + 204_800 * US, true)]);
    assert_eq!((rig.buttons().pulse_count(), rig.buttons().release_count()), (1, 1));
}
