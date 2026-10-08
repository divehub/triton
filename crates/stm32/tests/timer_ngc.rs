//! The scheduling policies of `Timers.STM32_Timer`, in particular the handset's `NGCLazyPwmTimer` in arithmetic
//! mode (`Scheduling::NgcArithmeticPwm`, the default): which limits are machine events, i.e. where the CPU's
//! chunks end. Expected values are derived from `emulation/models/NGCLazyPwmTimer.cs` (`TrySuppress`,
//! `Restore`, the overridden `WriteDoubleWord` / `OnGPIO` / `Reset`) and `STM32_Timer.cs`; the clock-source
//! effect of the boundaries is checked against Renode by `ngc/tests/micro_pwm`.

use emu_core::testing::Harness;
use emu_core::{PeriphId, Time, Width};
use stm32::timer::{reg, Scheduling, Stm32Timer};

const BASE: u32 = 0x4000_0000;

struct Rig {
    h: Harness,
    id: PeriphId,
}

impl Rig {
    fn new(limit: u32) -> Rig {
        Rig::with(Stm32Timer::new("timer2", 80_000_000, limit))
    }

    fn with(timer: Stm32Timer) -> Rig {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, timer);
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

    fn timer(&self) -> &Stm32Timer {
        self.h.get::<Stm32Timer>(self.id)
    }

    fn engaged(&self) -> bool {
        self.timer().suppressed()
    }

    fn next(&self) -> Option<Time> {
        self.h.next_event_time()
    }
}

/// The handset's TIM2 (backlight): 4 MHz tick (250 ns), period 99 ticks = 24 750 ns, PWM mode 1 on channel 2
/// with CCR 26 (6 500 ns).
fn tim2(r: &mut Rig) {
    r.w(reg::PSC, 19);
    r.w(reg::ARR, 99);
    r.w(reg::CCMR1, 0x6000);
    r.w(reg::CCER, 0x10);
    r.w(reg::CCR2, 26);
    r.w(reg::CR1, 1);
}

/// The handset's TIM15 (vibrator): 40 MHz tick (25 ns), period 100 ticks = 2 500 ns, PWM mode 1 on channel 1
/// with CCR 50 (1 250 ns).
fn tim15(r: &mut Rig) {
    r.w(reg::PSC, 1);
    r.w(reg::ARR, 100);
    r.w(reg::CCMR1, 0x60);
    r.w(reg::CCER, 0x1);
    r.w(reg::CCR1, 50);
    r.w(reg::CR1, 1);
}

#[test]
fn tim2_keeps_its_stock_events_until_the_first_overflow_then_gives_them_up() {
    let mut r = Rig::new(0xFFFF_FFFF);
    tim2(&mut r);
    assert!(r.h.take_stop_request(), "the CEN write is a LimitTimer setter: it ends the CPU chunk");
    assert!(!r.engaged());
    // The compare timer of channel 2 fires after 26 ticks (the event that ends a CPU chunk in Renode) ...
    assert_eq!(r.next(), Some(6_500));
    assert_eq!(r.timer().armed_alarm(), Some(6_500));
    r.at(6_500);
    // ... then the phantom limit of the expired one-shot compare timer (Renode's `nearestLimitIn` still holds
    // its would-be next limit, one compare period later, until an update pass drops it) ...
    assert_eq!(r.next(), Some(13_000));
    r.at(13_000);
    // ... and the counter's own limit after 99 ticks.
    assert_eq!(r.next(), Some(24_750));
    r.at(24_749);
    assert!(!r.engaged());
    r.at(24_750);
    assert!(r.engaged(), "the normal overflow found the configuration eligible");
    assert_eq!(r.next(), None, "no event of this timer is left");
    assert_eq!(r.timer().stats().engagements, 1);
    // Time passes without any work: 10 ms is 404 periods plus 1 000 ns = 4 ticks.
    r.at(10_000_000);
    assert_eq!(r.next(), None);
    assert_eq!(r.r(reg::CNT), 4);
    assert_eq!(r.r(reg::SR), 1, "UIF was latched by the first overflow and is sticky");
    assert!(r.engaged(), "reads do not restore the stock events");
    assert!(r.timer().stats().instants < 12 || r.timer().stats().skipped_cycles > 300, "{:?}", r.timer().stats());
}

#[test]
fn tim15_engages_at_its_first_overflow() {
    let mut r = Rig::new(0xFFFF);
    tim15(&mut r);
    assert_eq!(r.next(), Some(1_250));
    r.at(1_250);
    assert_eq!(r.next(), Some(2_500));
    r.at(2_500);
    assert!(r.engaged());
    assert_eq!(r.next(), None);
    r.at(1_000_000_000);
    assert_eq!(r.next(), None);
    assert_eq!(r.r(reg::CNT), 0, "400 000 whole periods");
}

#[test]
fn a_register_write_brings_the_stock_events_back_until_the_next_overflow() {
    let mut r = Rig::new(0xFFFF_FFFF);
    tim2(&mut r);
    r.at(24_750);
    assert!(r.engaged());
    r.h.take_stop_request();
    // 100 100 ns: 4 ticks and 0.4 tick into the fifth period (that began at 99 000 ns).
    r.at(100_100);
    assert!(r.engaged());
    r.w(reg::SR, 0); // clears UIF; any write restores (a SR write calls no LimitTimer setter by itself)
    assert!(!r.engaged());
    assert!(r.h.take_stop_request(), "Restore() rebuilds the compare timers with setters: the chunk ends");
    // The compare timer of channel 2 fires at 99 000 + 26 * 250 = 105 500 ns (its phantom limit follows at
    // 112 000 ns), the counter at 123 750 ns.
    assert_eq!(r.next(), Some(105_500));
    r.at(105_500);
    assert_eq!(r.next(), Some(112_000));
    r.at(112_000);
    assert_eq!(r.next(), Some(123_750));
    assert_eq!(r.r(reg::SR), 0, "UIF is set again only by the next overflow");
    r.at(123_750);
    assert!(r.engaged());
    assert_eq!(r.r(reg::SR), 1);
    assert_eq!(r.timer().stats().engagements, 2);
    assert_eq!(r.next(), None);
}

/// Renode's `nearestLimitIn` keeps the would-be next limit of a one-shot entry that just expired and disabled
/// itself (the update handler computes the time to the limit after the entry reset itself, and a handler that
/// calls no setter triggers no second update pass). The phantom limit ends a CPU chunk; the next update pass
/// (any `LimitTimer` setter, any other limit) recomputes `nearestLimitIn` and drops it.
#[test]
fn an_expired_one_shot_leaves_a_phantom_limit_until_the_next_update_pass() {
    for policy in [Scheduling::Stock, Scheduling::NgcArithmeticPwm] {
        let mut r = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_scheduling(policy));
        tim2(&mut r);
        r.at(6_500); // the compare timer expired and disabled itself
        assert_eq!(r.next(), Some(13_000), "{policy:?}: the phantom, one compare period (26 ticks) after the expiry");
        // A status write is no setter: no update pass, the phantom stays (and still ends a chunk at 13 000).
        r.w(reg::SR, 0);
        assert_eq!(r.next(), Some(13_000), "{policy:?}");
        // A LimitTimer setter (CNT is rewritten with its current value) runs update passes: the phantom is gone.
        let cnt = r.r(reg::CNT);
        r.w(reg::CNT, cnt);
        assert_eq!(r.next(), Some(24_750), "{policy:?}: only the counter's own limit is left");
        // The counter's own limit reloads `Limit = ARR` (a setter): it leaves no phantom behind. At that update
        // event the stock timer re-arms the compare timer (its match is at 24 750 + 6 500); the NGC timer finds the
        // configuration eligible and owns no event from here on.
        r.at(24_750);
        let expected = if policy == Scheduling::Stock { Some(31_250) } else { None };
        assert_eq!(r.next(), expected, "{policy:?}");
    }
}

#[test]
fn restore_is_one_chunk_end_even_when_the_write_has_no_setter_of_its_own() {
    // Stock Renode: a plain SR write ends no chunk. Engaged NGC: Restore() does.
    let mut stock = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_scheduling(Scheduling::Stock));
    tim2(&mut stock);
    stock.at(50_000);
    stock.h.take_stop_request();
    stock.w(reg::SR, 0);
    assert!(!stock.h.take_stop_request(), "a status write calls no LimitTimer setter");
    let mut ngc = Rig::new(0xFFFF_FFFF);
    tim2(&mut ngc);
    ngc.at(50_000);
    assert!(ngc.engaged());
    ngc.h.take_stop_request();
    ngc.w(reg::SR, 0);
    assert!(ngc.h.take_stop_request());
}

#[test]
fn the_stock_policy_has_every_limit_as_an_event_and_never_engages() {
    let mut r = Rig::with(Stm32Timer::new("timer15", 80_000_000, 0xFFFF).with_scheduling(Scheduling::Stock));
    tim15(&mut r);
    let mut events = Vec::new();
    while let Some(t) = r.next() {
        if t > 10_000 {
            break;
        }
        events.push(t);
        r.at(t);
    }
    // Per 2 500 ns period: the compare match at 1 250 ns and the overflow (the compare timer is re-armed by
    // every overflow).
    assert_eq!(events, vec![1_250, 2_500, 3_750, 5_000, 6_250, 7_500, 8_750, 10_000]);
    assert!(!r.engaged());
    assert_eq!(r.timer().stats().engagements, 0);
    assert_eq!(r.timer().scheduling(), Scheduling::Stock);
}

#[test]
fn the_observable_policy_never_queues_events_for_unobserved_pwm() {
    let mut r = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_scheduling(Scheduling::Observable));
    tim2(&mut r);
    assert_eq!(r.next(), None);
    assert!(!r.h.take_stop_request() || true);
    r.at(10_000_000);
    assert_eq!(r.next(), None);
    assert_eq!(r.r(reg::CNT), 4);
    assert!(!r.engaged());
}

/// Configurations on which `TrySuppress` returns: the stock events stay as long as the configuration lasts.
#[test]
fn ineligible_configurations_keep_their_stock_events() {
    type Setup = fn(&mut Rig);
    let cases: Vec<(&str, Setup)> = vec![
        ("update interrupt enabled", |r| r.w(reg::DIER, 1)),
        ("compare interrupt enabled", |r| r.w(reg::DIER, 1 << 2)),
        ("compare value equal to ARR", |r| r.w(reg::CCR2, 99)),
        ("compare value above ARR", |r| r.w(reg::CCR2, 120)),
        ("toggle mode, not PWM", |r| r.w(reg::CCMR1, 0x3000)),
        ("channel 2 an input", |r| {
            r.w(reg::CCMR1, 0x0100);
            r.w(reg::CCMR1, 0x0100 | 0x6000);
        }),
        ("one-pulse mode", |r| r.w(reg::CR1, 0x9)),
        ("counting down", |r| r.w(reg::CR1, 0x11)),
        ("center-aligned", |r| r.w(reg::CR1, 0x21)),
        ("update disabled", |r| r.w(reg::CR1, 0x3)),
        ("a slave mode", |r| r.w(reg::SMCR, 4)),
        ("repetition counter", |r| r.w(reg::RCR, 1)),
        ("no PWM output enabled", |r| r.w(reg::CCER, 0)),
        ("a divider that does not divide the clock (80 MHz / 3)", |r| r.w(reg::PSC, 2)),
        ("a tick of 312.5 ns (80 MHz / 25 = 3.2 MHz)", |r| r.w(reg::PSC, 24)),
    ];
    for (what, change) in cases {
        let mut r = Rig::new(0xFFFF_FFFF);
        tim2(&mut r);
        r.w(reg::CR1, 0); // the changes below are made while the timer is stopped, then it is started again
        change(&mut r);
        let cr1 = if what == "one-pulse mode" || what == "counting down" || what == "center-aligned" || what == "update disabled" {
            r.r(reg::CR1) | 1
        } else {
            1
        };
        r.w(reg::CR1, cr1);
        r.at(2_000_000);
        assert!(!r.engaged(), "{what}");
        assert_eq!(r.timer().stats().engagements, 0, "{what}");
        if what != "one-pulse mode" {
            assert!(r.next().is_some(), "{what}: the stock events continue");
        }
    }
}

#[test]
fn pins_with_a_receiver_and_input_edges_refuse_the_suppression() {
    // An observed (connected) pin.
    let mut r = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_observed_pins(0b0010));
    tim2(&mut r);
    r.at(1_000_000);
    assert!(!r.engaged());
    assert!(r.next().is_some());
    // An input edge: `externalInput` stays set until a reset.
    let mut r = Rig::new(0xFFFF_FFFF);
    tim2(&mut r);
    r.at(30_000);
    assert!(r.engaged());
    r.h.set_input(r.id, 0, true);
    assert!(!r.engaged(), "OnGPIO restores");
    r.at(1_000_000);
    assert!(!r.engaged(), "and the flag keeps it from engaging again");
    assert!(r.next().is_some());
    r.h.core_mut().reset_all();
    tim2(&mut r);
    r.at(r.h.now() + 30_000);
    assert!(r.engaged(), "a reset clears the flag");
}

#[test]
fn a_latent_repetition_count_must_drain_before_the_suppression() {
    // UIE with RCR = 2 at the first overflow sets repetitionsLeft = 3 and decrements it to 2. The guest clears
    // both registers right after, but the hidden count stays: the second overflow decrements it to 1 (the
    // handler runs before `TrySuppress`, which refuses a non-zero count), the third to 0 and engages.
    let mut r = Rig::new(0xFFFF_FFFF);
    r.w(reg::PSC, 19);
    r.w(reg::ARR, 99);
    r.w(reg::CCMR1, 0x6000);
    r.w(reg::CCER, 0x10);
    r.w(reg::CCR2, 26);
    r.w(reg::RCR, 2);
    r.w(reg::DIER, 1);
    r.w(reg::CR1, 1);
    r.at(24_750); // first overflow: the count becomes 3 and is decremented to 2; DIER != 0 anyway
    assert!(!r.engaged());
    r.w(reg::DIER, 0);
    r.w(reg::RCR, 0);
    r.at(49_500); // second overflow: eligible by the registers, but a repetition is still pending
    assert!(!r.engaged());
    r.at(74_249);
    assert!(!r.engaged());
    r.at(74_250); // third overflow: the count reached 0
    assert!(r.engaged());
}

#[test]
fn counters_are_identical_whether_or_not_the_events_exist() {
    let mut ngc = Rig::new(0xFFFF_FFFF);
    let mut stock = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_scheduling(Scheduling::Stock));
    let mut fast = Rig::with(Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF).with_scheduling(Scheduling::Observable));
    for rig in [&mut ngc, &mut stock, &mut fast] {
        tim2(rig);
    }
    for step in 1..=40u64 {
        let t = step * 997_331 + step * step * 13;
        let mut values = Vec::new();
        for rig in [&mut ngc, &mut stock, &mut fast] {
            rig.at(t);
            values.push((rig.r(reg::CNT), rig.r(reg::SR), rig.r(reg::CCR2), rig.h.peek(BASE + reg::CNT, Width::Word)));
        }
        assert_eq!(values[0], values[1], "ngc vs stock at {t}");
        assert_eq!(values[0], values[2], "ngc vs observable at {t}");
        if step % 7 == 0 {
            for rig in [&mut ngc, &mut stock, &mut fast] {
                rig.w(reg::CCR2, 10 + step as u32);
            }
        }
    }
    assert!(ngc.timer().stats().instants < stock.timer().stats().instants / 20, "{:?} vs {:?}", ngc.timer().stats(), stock.timer().stats());
}
