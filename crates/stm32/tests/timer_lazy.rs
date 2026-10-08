//! The lazy timer against the same model scheduling every limit as a real event (`set_naive_events`), which is
//! how the stock Renode model behaves: identical register reads, interrupt edges and observed pin edges for the
//! real firmware configurations and for randomized register scripts. Also checks that the performance
//! configurations (handset TIM2/TIM15) schedule no events at all and stay cheap over long intervals.

use emu_core::testing::{Harness, IrqChange, Probe};
use emu_core::{PeriphId, Time, TICKS_PER_MICROSECOND, TICKS_PER_MILLISECOND, TICKS_PER_SECOND};
use stm32::timer::{reg, Stm32Timer, IRQ_LINE, PIN_LINE_BASE};

const US: Time = TICKS_PER_MICROSECOND;
const MS: Time = TICKS_PER_MILLISECOND;
const BASE: u32 = 0x4000_0000;
const NVIC: u32 = 28;

struct Rig {
    h: Harness,
    id: PeriphId,
    probes: Vec<Probe>,
}

impl Rig {
    fn new(limit: u32, naive: bool, observed: u8) -> Rig {
        let mut h = Harness::new();
        let mut timer = Stm32Timer::new("timer", 80_000_000, limit).with_observed_pins(observed);
        timer.set_naive_events(naive);
        let id = h.add_mapped(BASE, 0x400, timer);
        h.connect_irq(id, IRQ_LINE, NVIC);
        let probes = (0..4).filter(|i| observed & (1 << i) != 0).map(|i| h.probe(id, PIN_LINE_BASE + i)).collect();
        h.clear_irq_changes();
        Rig { h, id, probes }
    }

    fn w(&mut self, offset: u32, value: u32) {
        self.h.write32(BASE + offset, value);
    }

    fn r(&mut self, offset: u32) -> u32 {
        self.h.read32(BASE + offset)
    }

    fn timer(&self) -> &Stm32Timer {
        self.h.get::<Stm32Timer>(self.id)
    }
}

/// The handset's TIM2 configuration (backlight, PWM mode 1 on channel 2): PSC 19, ARR 99, CCMR1 0x6000,
/// CCER 0x10, CCR2 26 (setting 70).
fn configure_tim2(rig: &mut Rig) {
    rig.w(reg::PSC, 19);
    rig.w(reg::ARR, 99);
    rig.w(reg::CCMR1, 0x6000);
    rig.w(reg::CCER, 0x10);
    rig.w(reg::CCR2, 26);
    rig.w(reg::CR1, 1);
}

/// The handset's TIM15 configuration (vibrator, PWM mode 1 on channel 1): PSC 1, ARR 100, CCMR1 0x60, CCER 1.
fn configure_tim15(rig: &mut Rig, ccr: u32) {
    rig.w(reg::PSC, 1);
    rig.w(reg::ARR, 100);
    rig.w(reg::CCMR1, 0x60);
    rig.w(reg::CCER, 1);
    rig.w(reg::CCR1, ccr);
    rig.w(reg::CR1, 1);
}

#[test]
fn handset_pwm_timers_schedule_no_real_events() {
    let mut tim2 = Rig::new(0xFFFF_FFFF, false, 0);
    configure_tim2(&mut tim2);
    assert_eq!(tim2.timer().armed_alarm(), None);
    assert_eq!(tim2.h.next_event_time(), None, "TIM2 at 40 kHz: nothing in the event queue");
    let mut tim15 = Rig::new(0xFFFF, false, 0);
    configure_tim15(&mut tim15, 50);
    assert_eq!(tim15.h.next_event_time(), None, "TIM15 at 400 kHz: nothing in the event queue");
    // Ten virtual seconds: 400 000 / 4 000 000 periods, a handful of replayed instants.
    for rig in [&mut tim2, &mut tim15] {
        rig.h.advance_to(10 * TICKS_PER_SECOND);
        assert_eq!(rig.h.next_event_time(), None);
        assert_eq!(rig.timer().stats().alarms, 0);
        assert!(rig.h.irq_changes().is_empty());
    }
    // CNT is reconstructed from time on access. TIM15: 10 s is exactly 4 000 000 periods of 2 500 ns. TIM2: the
    // period is 24 750 ns, 10 s is 404 040 periods plus 10 000 ns = 40 ticks of the 4 MHz clock.
    assert_eq!(tim2.r(reg::CNT), 40);
    assert_eq!(tim15.r(reg::CNT), 0);
    let stats = tim15.timer().stats();
    assert!(stats.instants < 64, "{stats:?}: the steady state is skipped in closed form");
    assert!(stats.skipped_cycles > 3_000_000, "{stats:?}");
    // UIF is sticky after the first update, exactly as in the stock model.
    assert_eq!(tim15.r(reg::SR), 1);
    // 10 s + 7.9 us is 3 periods and 400 ns into the 4th: 400 ns is 16 ticks of the 40 MHz clock.
    tim15.h.advance_to(10 * TICKS_PER_SECOND + 8 * US);
    assert_eq!(tim15.r(reg::CNT), 20, "8 us = 3 periods (7.5 us) + 500 ns = 20 ticks");
}

#[test]
fn pwm_registers_and_cnt_match_the_naive_model_over_time() {
    for (name, limit, config) in [
        ("tim2", 0xFFFF_FFFFu32, configure_tim2 as fn(&mut Rig)),
        ("tim15", 0xFFFF, (|rig: &mut Rig| configure_tim15(rig, 33)) as fn(&mut Rig)),
    ] {
        let mut lazy = Rig::new(limit, false, 0);
        let mut naive = Rig::new(limit, true, 0);
        config(&mut lazy);
        config(&mut naive);
        for step in 0..60u64 {
            let t = 1 + step * 7_919 + step * step * 13;
            lazy.h.advance_to(t * 100);
            naive.h.advance_to(t * 100);
            for offset in [reg::CNT, reg::SR, reg::CR1, reg::ARR, reg::PSC, reg::CCR1, reg::CCR2, reg::CCMR1, reg::CCER] {
                assert_eq!(lazy.r(offset), naive.r(offset), "{name} step {step} register 0x{offset:02X}");
            }
        }
        assert!(naive.timer().stats().instants > 1000, "the naive model processed every limit");
        assert!(lazy.timer().stats().instants < naive.timer().stats().instants / 10);
    }
}

#[test]
fn register_writes_during_pwm_are_exact() {
    // Writes at arbitrary times between periods, compared with the naive model.
    let mut lazy = Rig::new(0xFFFF, false, 0b0001);
    let mut naive = Rig::new(0xFFFF, true, 0b0001);
    configure_tim15(&mut lazy, 50);
    configure_tim15(&mut naive, 50);
    let script: &[(Time, u32, u32)] = &[
        (12_345, reg::CCR1, 20),
        (98_765, reg::SR, !1),
        (250_001, reg::ARR, 80),
        (251_333, reg::CNT, 30),
        (400_000, reg::PSC, 3),
        (400_001, reg::EGR, 1),
        (777_777, reg::CCER, 0),
        (900_000, reg::CCER, 1),
        (1_234_567, reg::CR1, 0x81),
    ];
    for &(t, offset, value) in script {
        lazy.h.advance_to(t);
        naive.h.advance_to(t);
        lazy.w(offset, value);
        naive.w(offset, value);
        for probe in [reg::CNT, reg::SR, reg::ARR, reg::CCR1] {
            assert_eq!(lazy.r(probe), naive.r(probe), "t={t} after writing 0x{offset:02X}: register 0x{probe:02X}");
        }
    }
    assert_eq!(lazy.h.probe_changes(lazy.probes[0]), naive.h.probe_changes(naive.probes[0]));
}

// ---- randomized differential test -------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Clone, Debug)]
enum Op {
    Write(u32, u32),
    Read(u32),
    Peek(u32),
    Advance(Time),
    Input(u32, bool),
}

struct Gen {
    rng: Rng,
    arr: u64,
    psc: u64,
    ccmr_in: [bool; 2],
}

impl Gen {
    fn value_for(&mut self, offset: u32) -> u32 {
        let rng = &mut self.rng;
        match offset {
            reg::CR1 => {
                let mut v = 0;
                if rng.chance(75) {
                    v |= 1;
                }
                if rng.chance(8) {
                    v |= 2;
                }
                if rng.chance(10) {
                    v |= 4;
                }
                if rng.chance(12) {
                    v |= 8;
                }
                if rng.chance(15) {
                    v |= 0x10;
                }
                if rng.chance(5) {
                    v |= (1 + rng.below(3) as u32) << 5;
                }
                if rng.chance(30) {
                    v |= 0x80;
                }
                if rng.chance(5) {
                    v |= 0x100;
                }
                v
            }
            reg::CR2 => (rng.below(2) as u32) << 7,
            reg::SMCR => {
                let sms = if rng.chance(80) { 0 } else { rng.below(8) as u32 };
                sms | (rng.below(8) as u32) << 4
            }
            reg::DIER => {
                let mut v = 0;
                for bit in [0u32, 1, 2, 3, 4, 6] {
                    if rng.chance(35) {
                        v |= 1 << bit;
                    }
                }
                v
            }
            reg::SR => match rng.below(4) {
                0 => 0,
                1 => 0xFFFF_FFFF,
                2 => !(1u32 << rng.below(13)),
                _ => rng.below(0x2000) as u32,
            },
            reg::EGR => 1,
            reg::CCMR1 | reg::CCMR2 => {
                let mut v = 0;
                for half in 0..2 {
                    let cc = if rng.chance(70) { 0 } else { 1 + rng.below(2) as u32 };
                    v |= cc << (half * 8);
                    if cc == 0 {
                        v |= (rng.below(8) as u32) << (half * 8 + 4);
                        if rng.chance(5) {
                            v |= 8 << (half * 8);
                        }
                    } else {
                        v |= (rng.below(4) as u32) << (half * 8 + 2);
                    }
                }
                v
            }
            reg::CCER => {
                let mut v = 0;
                for i in 0..4 {
                    if rng.chance(60) {
                        v |= 1 << (4 * i);
                    }
                    if rng.chance(30) {
                        v |= 2 << (4 * i);
                    }
                    if rng.chance(20) {
                        v |= 8 << (4 * i);
                    }
                }
                v
            }
            reg::CNT => rng.below(self.arr + 2) as u32,
            reg::PSC => {
                let psc = if rng.chance(70) { rng.below(6) } else { rng.below(80) };
                self.psc = psc;
                psc as u32
            }
            reg::ARR => {
                let arr = if rng.chance(10) { 0 } else { 1 + rng.below(300) };
                self.arr = arr;
                arr as u32
            }
            reg::RCR => rng.below(4) as u32,
            reg::CCR1 | reg::CCR2 | reg::CCR3 | reg::CCR4 => rng.below(self.arr + 6) as u32,
            reg::BDTR => rng.below(0x1_0000) as u32,
            _ => unreachable!(),
        }
    }

    fn op(&mut self) -> Op {
        const WRITABLE: [u32; 17] = [
            reg::CR1, reg::CR1, reg::CR2, reg::SMCR, reg::DIER, reg::DIER, reg::SR, reg::EGR, reg::CCMR1, reg::CCMR2,
            reg::CCER, reg::CNT, reg::PSC, reg::ARR, reg::RCR, reg::CCR1, reg::BDTR,
        ];
        const READABLE: [u32; 12] = [
            reg::CR1, reg::DIER, reg::SR, reg::CCMR1, reg::CCMR2, reg::CCER, reg::CNT, reg::PSC, reg::ARR, reg::CCR1,
            reg::CCR2, reg::CCR4,
        ];
        let _ = self.ccmr_in;
        match self.rng.below(100) {
            0..=34 => {
                let mut offset = WRITABLE[self.rng.below(WRITABLE.len() as u64) as usize];
                if offset == reg::CCR1 {
                    offset += 4 * self.rng.below(4) as u32;
                }
                Op::Write(offset, self.value_for(offset))
            }
            35..=54 => Op::Read(READABLE[self.rng.below(READABLE.len() as u64) as usize]),
            55..=62 => Op::Peek(READABLE[self.rng.below(READABLE.len() as u64) as usize]),
            63..=66 => Op::Input(self.rng.below(4) as u32, self.rng.chance(50)),
            _ => {
                // Bound the work of the naive model: at most ~150 periods per step.
                let period_ns = (self.arr.max(1) * (self.psc + 1) * 25 / 2).max(25);
                let bound = (period_ns * 150).min(3 * MS);
                let ns = match self.rng.below(10) {
                    0..=2 => 1 + self.rng.below(2_000),
                    3..=7 => 1 + self.rng.below(bound),
                    _ => 1 + self.rng.below(bound / 4 + 1),
                };
                Op::Advance(ns)
            }
        }
    }
}

fn run_script(seed: u64, ops: usize, observed: u8, limit: u32) -> Coverage {
    if std::env::var_os("TIMER_TEST_VERBOSE").is_some() {
        eprintln!("script seed {seed}");
    }
    let mut gen = Gen { rng: Rng(seed ^ 0x9E37_79B9_7F4A_7C15), arr: 0xFFFF, psc: 0, ccmr_in: [false; 2] };
    let mut lazy = Rig::new(limit, false, observed);
    let mut naive = Rig::new(limit, true, observed);
    let mut log: Vec<Op> = Vec::new();
    for step in 0..ops {
        let op = gen.op();
        if std::env::var_os("TIMER_TEST_TRACE").is_some() {
            eprintln!("step {step}: {op:?} (lazy time {})", lazy.h.now());
        }
        log.push(op.clone());
        let context = || format!("seed {seed} step {step} op {op:?}\nscript so far: {log:?}");
        match op {
            Op::Write(offset, value) => {
                lazy.w(offset, value);
                naive.w(offset, value);
            }
            Op::Read(offset) => {
                let (a, b) = (lazy.r(offset), naive.r(offset));
                assert_eq!(a, b, "read 0x{offset:02X}: lazy 0x{a:X} naive 0x{b:X}\n{}", context());
            }
            Op::Peek(offset) => {
                let a = lazy.h.peek(BASE + offset, emu_core::Width::Word);
                let b = naive.h.peek(BASE + offset, emu_core::Width::Word);
                assert_eq!(a, b, "peek 0x{offset:02X}\n{}", context());
            }
            Op::Advance(ns) => {
                let t = lazy.h.now() + ns;
                lazy.h.advance_to(t);
                naive.h.advance_to(t);
            }
            Op::Input(line, level) => {
                lazy.h.set_input(lazy.id, line, level);
                naive.h.set_input(naive.id, line, level);
            }
        }
        assert_eq!(lazy.h.irq_changes(), naive.h.irq_changes(), "interrupt edges differ\n{}", context());
        for (a, b) in lazy.probes.iter().zip(naive.probes.iter()) {
            assert_eq!(lazy.h.probe_changes(*a), naive.h.probe_changes(*b), "pin edges differ\n{}", context());
        }
    }
    // Final state: every register.
    for offset in (0..=0x44).step_by(4) {
        if offset == reg::EGR {
            continue;
        }
        let (a, b) = (lazy.r(offset), naive.r(offset));
        assert_eq!(a, b, "final register 0x{offset:02X} (seed {seed})");
    }
    Coverage {
        irq_edges: lazy.h.irq_changes().len() as u64,
        pin_edges: lazy.probes.iter().map(|p| lazy.h.probe_changes(*p).len() as u64).sum(),
        naive_instants: naive.timer().stats().instants,
        lazy_instants: lazy.timer().stats().instants,
        lazy_alarms: lazy.timer().stats().alarms,
        skipped_cycles: lazy.timer().stats().skipped_cycles,
        warnings: lazy.h.warnings().len() as u64,
    }
}

/// What a batch of scripts exercised (the test must not pass vacuously).
#[derive(Default, Debug)]
struct Coverage {
    irq_edges: u64,
    pin_edges: u64,
    naive_instants: u64,
    lazy_instants: u64,
    lazy_alarms: u64,
    skipped_cycles: u64,
    warnings: u64,
}

impl Coverage {
    fn add(&mut self, other: Coverage) {
        self.irq_edges += other.irq_edges;
        self.pin_edges += other.pin_edges;
        self.naive_instants += other.naive_instants;
        self.lazy_instants += other.lazy_instants;
        self.lazy_alarms += other.lazy_alarms;
        self.skipped_cycles += other.skipped_cycles;
        self.warnings += other.warnings;
    }
}

fn run_batch(seeds: std::ops::Range<u64>, observed: u8, limit: u32) -> Coverage {
    let mut total = Coverage::default();
    for seed in seeds {
        total.add(run_script(seed, 160, observed, limit));
    }
    total
}

/// `TIMER_TEST_SEED=25 TIMER_TEST_TRACE=1 cargo test ... one_script -- --nocapture --ignored`
#[test]
#[ignore]
fn one_script() {
    let seed: u64 = std::env::var("TIMER_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let observed: u8 = std::env::var("TIMER_TEST_OBSERVED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    run_script(seed, 160, observed, 0xFFFF);
}

#[test]
fn random_scripts_16_bit_timer_unobserved_pins() {
    let c = run_batch(1..121, 0, 0xFFFF);
    eprintln!("{c:?}");
    assert!(c.irq_edges > 300 && c.naive_instants > 20_000, "{c:?}");
    assert!(c.lazy_instants < c.naive_instants, "{c:?}");
}

#[test]
fn random_scripts_16_bit_timer_observed_pins() {
    let c = run_batch(1000..1100, 0xF, 0xFFFF);
    eprintln!("{c:?}");
    assert!(c.irq_edges > 300 && c.pin_edges > 300 && c.naive_instants > 20_000, "{c:?}");
}

#[test]
fn random_scripts_32_bit_timer_mixed_pins() {
    let c = run_batch(5000..5060, 0b0101, 0xFFFF_FFFF);
    eprintln!("{c:?}");
    assert!(c.irq_edges > 100 && c.naive_instants > 10_000, "{c:?}");
}

#[test]
fn irq_edges_are_exact_when_interrupts_are_enabled() {
    // The HAL tick: UIE with a handler that clears UIF 3 us after every interrupt.
    for naive in [false, true] {
        let mut rig = Rig::new(0xFFFF, naive, 0);
        rig.w(reg::ARR, 999);
        rig.w(reg::PSC, 79);
        rig.w(reg::DIER, 1);
        rig.w(reg::CR1, 1);
        let mut edges: Vec<IrqChange> = Vec::new();
        for k in 1..=20u64 {
            rig.h.advance_to(k * 999 * US + 3 * US);
            rig.w(reg::SR, !1);
        }
        edges.extend_from_slice(rig.h.irq_changes());
        assert_eq!(edges.len(), 40);
        for (i, edge) in edges.iter().enumerate() {
            let k = (i / 2 + 1) as u64;
            let want = if i % 2 == 0 {
                IrqChange { time: k * 999 * US, irq: NVIC, level: true }
            } else {
                IrqChange { time: k * 999 * US + 3 * US, irq: NVIC, level: false }
            };
            assert_eq!(*edge, want, "edge {i} (naive={naive})");
        }
        let stats = rig.timer().stats();
        assert_eq!(stats.alarms, 20, "one real event per tick (naive={naive})");
    }
}
