//! `Timers.STM32_Timer` beyond the plain counter: output-compare pulse shapes, input capture, the slave modes, the
//! repetition counter, run-time pin observation, side-effect-free peeks and the guards for configurations on which
//! the stock model hangs. Expected values are derived from `STM32_Timer.cs` (not from the Rust code); the
//! transcripts replayed by `tests/renode_timer` check the same behaviour against the real Renode 1.17.0.

use emu_core::testing::Harness;
use emu_core::{PeriphId, Time, Width, TICKS_PER_MICROSECOND};
use stm32::timer::{reg, Stm32Timer, IRQ_LINE, PIN_LINE_BASE, TRIGGER_LINE};

const US: Time = TICKS_PER_MICROSECOND;
const BASE: u32 = 0x4000_0400;
const NVIC: u32 = 29;

struct Rig {
    h: Harness,
    id: PeriphId,
}

impl Rig {
    fn new(limit: u32, observed: u8) -> Rig {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Stm32Timer::new("timer", 80_000_000, limit).with_observed_pins(observed));
        h.connect_irq(id, IRQ_LINE, NVIC);
        h.clear_irq_changes();
        Rig { h, id }
    }

    fn w(&mut self, offset: u32, value: u32) {
        self.h.write32(BASE + offset, value);
    }

    fn r(&mut self, offset: u32) -> u32 {
        self.h.read32(BASE + offset)
    }

    fn at(&mut self, t: Time) {
        self.h.advance_to(t);
    }

    fn input(&mut self, line: u32, level: bool) {
        self.h.set_input(self.id, line, level);
    }

    fn timer(&self) -> &Stm32Timer {
        self.h.get::<Stm32Timer>(self.id)
    }

    fn summary(&self) -> String {
        self.h.core().summaries().into_iter().find(|(n, _)| n == "timer").expect("timer summary").1
    }
}

/// `Blink()` is `Set(); Unset();`: two edges at the same instant (none for the one that changes nothing).
fn pulse(t: Time, first: bool) -> Vec<(Time, bool)> {
    vec![(t, first), (t, !first)]
}

#[test]
fn output_compare_modes_pulse_toggle_or_force_the_pin() {
    // PSC 79 (1 MHz), ARR 100, CCR1 30: the compare timer fires 30 us after every update (0, 100, 200 us ...).
    let cases: Vec<(u32, &str, Vec<(Time, bool)>)> = vec![
        (0, "frozen: no effect", vec![]),
        (1, "set active on match: a high pulse", [pulse(30 * US, true), pulse(130 * US, true), pulse(230 * US, true)].concat()),
        // Writing the mode already drives the pin high; every match is a low pulse.
        (2, "set inactive on match: a low pulse", [vec![(0, true)], pulse(30 * US, false), pulse(130 * US, false), pulse(230 * US, false)].concat()),
        (3, "toggle on match", vec![(30 * US, true), (130 * US, false), (230 * US, true)]),
        (4, "force inactive: low at the mode write, nothing at the matches", vec![]),
        (5, "force active: high at the mode write, nothing at the matches", vec![(0, true)]),
    ];
    for (mode, what, want) in cases {
        let mut r = Rig::new(0xFFFF, 0b0001);
        let pin = r.h.probe(r.id, PIN_LINE_BASE);
        r.w(reg::PSC, 79);
        r.w(reg::ARR, 100);
        r.w(reg::CCR1, 30);
        r.w(reg::CCMR1, mode << 4);
        r.w(reg::CCER, 1);
        r.w(reg::CR1, 1);
        r.at(250 * US);
        assert_eq!(r.h.probe_changes(pin), want, "OC1M = {mode}: {what}");
    }
}

#[test]
fn disabling_the_channel_output_drives_the_pin_low() {
    let mut r = Rig::new(0xFFFF, 0b0001);
    let pin = r.h.probe(r.id, PIN_LINE_BASE);
    r.w(reg::CCMR1, 5 << 4); // force active: high
    r.w(reg::CCER, 1);
    r.at(10 * US);
    r.w(reg::CCER, 0); // WriteCaptureCompareOutputEnable(false) unsets the connection
    assert_eq!(r.h.probe_changes(pin), vec![(0, true), (10 * US, false)]);
}

#[test]
fn input_capture_latches_the_counter_flags_and_overcaptures() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79); // 1 us per tick
    r.w(reg::CCMR1, 0x01); // CC1S = TI1
    r.w(reg::CCER, 0x01); // CC1E, rising edge
    r.w(reg::DIER, 0x02); // CC1IE
    r.w(reg::CR1, 1);
    // A rising edge at 1234 us latches CNT and raises CC1IF and the interrupt.
    r.at(1234 * US);
    r.input(0, true);
    assert_eq!(r.r(reg::SR), 0x2);
    assert!(r.h.irq_level(NVIC));
    // A peek never clears the flag; a read of CCR1 does.
    assert_eq!(r.h.peek(BASE + reg::CCR1, Width::Word), Some(1234));
    assert_eq!(r.r(reg::SR), 0x2);
    assert_eq!(r.r(reg::CCR1), 1234);
    assert_eq!(r.r(reg::SR), 0, "reading CCR1 clears CC1IF");
    assert!(!r.h.irq_level(NVIC));
    // CCR1 is read-only while the channel is an input.
    r.w(reg::CCR1, 7);
    assert_eq!(r.r(reg::CCR1), 1234);
    // The falling edge is not a capture edge.
    r.at(1500 * US);
    r.input(0, false);
    assert_eq!(r.r(reg::SR), 0);
    // A second rising edge while CC1IF is still set also sets the overcapture flag (SR bit 9).
    r.at(2000 * US);
    r.input(0, true);
    r.at(2100 * US);
    r.input(0, false);
    r.at(2200 * US);
    r.input(0, true);
    assert_eq!(r.r(reg::SR), 0x2 | 1 << 9);
    assert_eq!(r.r(reg::CCR1), 2200, "the latest capture wins");
    r.w(reg::SR, !(1 << 9));
    assert_eq!(r.r(reg::SR), 0, "CC1IF went with the CCR1 read, CC1OF with the write of zero");
}

#[test]
fn input_capture_on_both_edges_and_with_a_prescaler() {
    // Both edges: CC1P and CC1NP set.
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::CCMR1, 0x01);
    r.w(reg::CCER, 0x0B);
    r.w(reg::CR1, 1);
    r.at(100 * US);
    r.input(0, true);
    assert_eq!(r.r(reg::CCR1), 100);
    r.at(250 * US);
    r.input(0, false);
    assert_eq!(r.r(reg::SR) & 0x2, 0x2, "the falling edge is captured too");
    assert_eq!(r.r(reg::CCR1), 250);
    // IC1PSC = 1: every second edge is captured. Renode parity: CCMR1 exists in four layouts (output/input for each
    // half), selected by the channel directions *before* the write, and the prescaler field the capture logic reads
    // is the one of the last layout built, (input, input). So the prescaler only takes effect when it is written
    // while both channels of the pair are already inputs.
    let sequences: [(&str, &[u32], Time); 3] = [
        ("one write that also switches CC1 to input", &[0x01 | 1 << 2], 100),
        ("CC1 input, CC2 still an output", &[0x01, 0x01 | 1 << 2], 100),
        ("both channels inputs first", &[0x0101, 0x0101 | 1 << 2], 300),
    ];
    for (what, writes, captured_at) in sequences {
        let mut r = Rig::new(0xFFFF, 0);
        r.w(reg::PSC, 79);
        for &value in writes {
            r.w(reg::CCMR1, value);
        }
        r.w(reg::CCER, 0x01);
        r.w(reg::CR1, 1);
        let mut first_capture = None;
        for (t, level) in [(100, true), (200, false), (300, true)] {
            r.at(t * US);
            r.input(0, level);
            if first_capture.is_none() && r.r(reg::SR) & 0x2 != 0 {
                first_capture = Some(t);
            }
        }
        assert_eq!(first_capture, Some(captured_at), "{what}");
    }
}

#[test]
fn ti1s_makes_channel_one_follow_the_xor_of_the_first_three_inputs() {
    for ti1s in [false, true] {
        let mut r = Rig::new(0xFFFF, 0);
        r.w(reg::PSC, 79);
        r.w(reg::CCMR1, 0x0101); // CC1S = TI1, CC2S = TI2
        r.w(reg::CCER, 0x11);
        r.w(reg::CR2, if ti1s { 0x80 } else { 0 });
        r.w(reg::CR1, 1);
        r.at(500 * US);
        r.input(1, true); // TI2 rises
        let flags = r.r(reg::SR) & 0x1E;
        assert_eq!(flags, if ti1s { 0x6 } else { 0x4 }, "TI1S = {ti1s}");
    }
}

#[test]
fn slave_reset_mode_zeroes_the_counter_and_raises_only_the_trigger_line() {
    let mut r = Rig::new(0xFFFF, 0);
    let trigger = r.h.probe(r.id, TRIGGER_LINE);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 1000);
    r.w(reg::CCMR1, 0x01);
    r.w(reg::CCER, 0x01);
    r.w(reg::SMCR, 4 | 5 << 4); // reset mode, trigger TI1
    r.w(reg::DIER, 1 << 6); // TIE
    r.w(reg::CR1, 1);
    r.at(300 * US);
    r.input(0, true);
    r.at(450 * US);
    assert_eq!(r.r(reg::CNT), 150, "reset at 300 us");
    assert_eq!(r.r(reg::SR), 0x42, "TIF and the capture flag of the same edge");
    assert_eq!(r.h.probe_changes(trigger), vec![(300 * US, true)]);
    // Renode parity: HandleResetMode only sets the TriggerInterrupt output; IRQ is recomputed at the next register
    // access that calls UpdateInterrupts (a write of SR here).
    assert!(!r.h.irq_level(NVIC));
    r.w(reg::SR, 0xFFFF_FFFF);
    assert!(r.h.irq_level(NVIC));
}

#[test]
fn slave_gated_mode_counts_only_while_the_input_is_high() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 1000);
    r.w(reg::CCMR1, 0x01);
    r.w(reg::CCER, 0x01);
    r.w(reg::SMCR, 5 | 5 << 4); // gated, TI1
    r.w(reg::CR1, 1);
    // Until the first edge the counter runs (CEN alone enables it).
    r.at(100 * US);
    r.input(0, true);
    r.at(300 * US);
    r.input(0, false); // gate closes: the counter freezes at 300
    r.at(400 * US);
    assert_eq!(r.r(reg::CNT), 300);
    r.at(500 * US);
    r.input(0, true);
    r.at(600 * US);
    assert_eq!(r.r(reg::CNT), 400, "300 plus the 100 us since the gate reopened");
}

#[test]
fn slave_trigger_mode_starts_the_counter_on_the_edge() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 1000);
    r.w(reg::CCMR1, 0x01);
    r.w(reg::CCER, 0x01);
    r.w(reg::SMCR, 6 | 5 << 4); // trigger mode, TI1
    r.w(reg::CR1, 1); // CEN does not start it in trigger mode
    r.at(200 * US);
    assert_eq!(r.r(reg::CNT), 0);
    r.input(0, true);
    r.at(500 * US);
    assert_eq!(r.r(reg::CNT), 300);
}

#[test]
fn external_clock_mode_1_is_refused_with_a_warning() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::SMCR, 7);
    assert!(r.h.warnings().iter().any(|m| m.contains("External Clock mode 1 is not supported")), "{:?}", r.h.warnings());
    assert_eq!(r.r(reg::SMCR) & 7, 7, "the field still stores the value");
}

#[test]
fn encoder_mode_3_counts_the_quadrature_edges() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::CCMR1, 0x0101);
    r.w(reg::CCER, 0x11);
    r.w(reg::SMCR, 3);
    r.w(reg::CR1, 1); // encoder mode: CEN does not run the timer, the edges move the counter
    r.w(reg::CNT, 10);
    // Forward sequence from (0, 0): TI1 up, TI2 up, TI1 down, TI2 down, one count per edge.
    for (i, (line, level)) in [(0, true), (1, true), (0, false), (1, false)].into_iter().enumerate() {
        r.input(line, level);
        assert_eq!(r.r(reg::CNT), 11 + i as u32, "edge {i}");
    }
    r.h.advance_to(10_000 * US);
    assert_eq!(r.r(reg::CNT), 14, "time does not move an encoder-mode counter");
}

#[test]
fn repetition_counter_delays_the_update_interrupt_line() {
    // Edge-aligned: the line is recomputed at the first update and then every RCR + 1 updates; in between UIF is
    // set again but nothing re-evaluates the interrupt (Renode parity).
    for (cr1, rcr) in [(0x01u32, 2u32), (0x21, 1)] {
        let mut r = Rig::new(0xFFFF, 0);
        let irq = r.h.probe(r.id, IRQ_LINE);
        r.w(reg::PSC, 79);
        r.w(reg::ARR, 10); // an update every 10 us
        r.w(reg::RCR, rcr);
        r.w(reg::DIER, 1);
        r.w(reg::CR1, cr1); // the second case is center-aligned mode 1: repetitions count twice
        r.at(15 * US);
        r.w(reg::SR, !1);
        r.at(25 * US);
        assert_eq!(r.r(reg::SR), 1, "UIF was set again by the update at 20 us");
        assert!(!r.h.irq_level(NVIC));
        r.at(45 * US);
        assert_eq!(r.h.probe_changes(irq), vec![(10 * US, true), (15 * US, false), (40 * US, true)], "CR1 = 0x{cr1:X}");
    }
}

#[test]
fn zero_period_entries_are_disabled_instead_of_hanging() {
    // ARR = 0 stops the counter but leaves Limit = 0 (no preload); with the preload and update-disable bits set the
    // next non-zero ARR write enables an entry whose limit stays 0, and its handler returns without reloading
    // (UDIS). The stock clock source then reaches that limit again at the same instant, forever, as soon as time
    // advances (`Advance` loops with a zero step). Here the entry is disabled with an error at the next access.
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::CR1, 1);
    r.w(reg::ARR, 0);
    r.w(reg::CR1, 0x83); // CEN, UDIS, APRE
    r.w(reg::ARR, 84);
    r.at(1000 * US);
    assert_eq!(r.r(reg::CNT), 0);
    assert!(r.h.warnings().iter().any(|m| m.contains("zero period")), "{:?}", r.h.warnings());
    // The same sequence without UDIS is harmless: enabling an entry that is already at its limit runs the handler,
    // which reloads Limit from ARR.
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::CR1, 1);
    r.w(reg::ARR, 0);
    r.w(reg::CR1, 0x81);
    r.w(reg::ARR, 84);
    assert_eq!(r.r(reg::SR), 1, "the update event ran when the entry was enabled");
    r.at(1000 * US);
    assert_eq!(r.r(reg::CNT), 1000 % 84);
    assert!(r.h.warnings().is_empty(), "{:?}", r.h.warnings());
}

#[test]
fn observing_a_pin_at_run_time_delivers_its_edges_from_then_on() {
    // PWM mode 1 on channel 1: high for the first 30 us of every 100 us period.
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::CCR1, 30);
    r.w(reg::CCMR1, 6 << 4);
    r.w(reg::CCER, 1);
    r.w(reg::CR1, 1);
    r.at(1010 * US);
    assert_eq!(r.r(reg::SR), 1);
    assert!(r.h.output(r.id, PIN_LINE_BASE), "inside the high phase of the period that began at 1000 us");
    assert_eq!(r.h.next_event_time(), None, "nothing observes the pin: no event is scheduled");
    r.h.with::<Stm32Timer, _>(r.id, |t, ctx| t.observe_pin(ctx, 0, true));
    let pin = r.h.probe(r.id, PIN_LINE_BASE);
    r.at(1250 * US);
    let want = vec![(1030 * US, false), (1100 * US, true), (1130 * US, false), (1200 * US, true), (1230 * US, false)];
    assert_eq!(r.h.probe_changes(pin), want);
    // And back: unobserved again. The topology change gave the stock events back (the NGC policy restores on it):
    // the compare timer that expired at 1230 us still has its phantom limit at 1260 us, the counter its own at
    // 1300 us; that overflow finds the configuration eligible and the events are given up again.
    r.h.with::<Stm32Timer, _>(r.id, |t, ctx| t.observe_pin(ctx, 0, false));
    assert_eq!(r.h.next_event_time(), Some(1260 * US));
    r.at(1260 * US);
    assert_eq!(r.h.next_event_time(), Some(1300 * US));
    r.at(1310 * US);
    assert!(r.timer().suppressed());
    assert_eq!(r.h.next_event_time(), None);
    // The machine's output level follows lazily on every access: 10 us into the high phase of the 1300 us period.
    assert_eq!(r.r(reg::SR), 1);
    assert!(r.h.output(r.id, PIN_LINE_BASE));
}

#[test]
fn peek_matches_read_and_has_no_side_effects() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::CCR1, 30);
    r.w(reg::CCMR1, 6 << 4);
    r.w(reg::CCER, 1);
    r.w(reg::DIER, 1 << 1 | 1);
    r.w(reg::CR1, 1);
    for step in 1..=5u64 {
        r.at(step * 37 * US + 11);
        for offset in (0..=reg::BDTR).step_by(4) {
            let peeked = r.h.peek(BASE + offset, Width::Word).unwrap_or_else(|| panic!("no peek for 0x{offset:02X}"));
            let read = r.r(offset);
            assert_eq!(peeked, read, "register 0x{offset:02X} at step {step}");
        }
    }
    assert!(r.h.warnings().is_empty(), "{:?}", r.h.warnings());
}

#[test]
fn summary_describes_the_timer() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::CR1, 1);
    r.at(250 * US);
    let text = r.summary();
    for part in ["CEN=1", "PSC=79", "ARR=100", "CNT=50", "entry=1000000Hz"] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
}

#[test]
fn host_reset_restores_the_power_on_state_and_cancels_the_alarm() {
    let mut r = Rig::new(0xFFFF, 0);
    r.w(reg::PSC, 79);
    r.w(reg::ARR, 100);
    r.w(reg::DIER, 1);
    r.w(reg::CR1, 1);
    assert!(r.timer().armed_alarm().is_some());
    r.at(250 * US);
    assert!(r.h.irq_level(NVIC));
    r.h.core_mut().reset_all();
    assert_eq!(r.timer().armed_alarm(), None);
    assert_eq!(r.r(reg::ARR), 0xFFFF);
    assert!(!r.h.irq_level(NVIC), "the interrupt output went low with the reset");
    assert_eq!(r.r(reg::CR1), 0);
    assert_eq!(r.r(reg::SR), 0);
    r.at(5_000 * US);
    assert_eq!(r.r(reg::CNT), 0);
}
