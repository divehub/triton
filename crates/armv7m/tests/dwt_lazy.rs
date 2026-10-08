//! The DWT cycle counter is advanced lazily (it never raises an event): the machine clock moving on does not touch it,
//! reads and register writes catch it up. This compares the core with an independent exact model of the counter over
//! random sequences of clock advances (chunk ends, long idle jumps), CTRL / CYCCNT writes at the clock time and reads at
//! the clock time or later.

use armv7m::{Cpu, CpuConfig};

const CTRL: u32 = 0xE000_1000;
const CYCCNT: u32 = 0xE000_1004;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.next() >> 11) % n
    }
}

/// 80 MHz counter on the 1 ns time base: 2 / 25 counts per nanosecond, the remainder kept as a fraction of 25.
#[derive(Clone, Copy)]
struct Model {
    value: u64,
    frac: u64,
    enabled: bool,
}

impl Model {
    fn advance(&mut self, ns: u64) {
        if self.enabled {
            let total = u128::from(self.frac) + 2 * u128::from(ns);
            self.value = self.value.wrapping_add((total / 25) as u64);
            self.frac = (total % 25) as u64;
        }
    }
}

#[test]
fn lazily_advanced_dwt_equals_the_exact_model() {
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut cpu = Cpu::new(CpuConfig::default());
        let mut model = Model { value: 0, frac: 0, enabled: false };
        let mut clock = 0u64; // the machine clock: where the model and the core are both "now"
        for step in 0..400 {
            match rng.below(8) {
                // The chunk ends: the clock moves on (a short step, or a long idle jump).
                0..=2 => {
                    let ns = match rng.below(4) {
                        0 => rng.below(1_000),
                        1 => rng.below(200_000),
                        2 => rng.below(50_000_000),
                        _ => rng.below(4_000_000_000),
                    };
                    cpu.advance_idle(clock, clock + ns);
                    model.advance(ns);
                    clock += ns;
                }
                // CTRL write (CYCCNTENA), a poke at the clock time.
                3 => {
                    let on = rng.below(2);
                    cpu.ppb_poke32(CTRL, on as u32, clock);
                    model.enabled = on == 1;
                }
                // CYCCNT write: the fraction of the cycle is kept.
                4 => {
                    let v = rng.next() as u32;
                    cpu.ppb_poke32(CYCCNT, v, clock);
                    model.value = u64::from(v);
                }
                // A poke that moves the clock first.
                5 => {
                    let ns = rng.below(300_000);
                    let on = rng.below(2);
                    cpu.ppb_poke32(CTRL, on as u32, clock + ns);
                    model.advance(ns);
                    clock += ns;
                    model.enabled = on == 1;
                }
                // Reads at the clock time or later (the core's own clock does not move), and at an earlier time.
                _ => {
                    let ahead = match rng.below(3) {
                        0 => 0,
                        1 => rng.below(1_000),
                        _ => rng.below(3_000_000_000),
                    };
                    let mut at_read = model;
                    at_read.advance(ahead);
                    assert_eq!(cpu.ppb_peek32(CYCCNT, clock + ahead), Some(at_read.value as u32), "seed {seed} step {step}: read {ahead} ns ahead of {clock}");
                    if clock > 0 {
                        // A read in the past shows the value at the machine clock (it never went backwards).
                        assert_eq!(cpu.ppb_peek32(CYCCNT, clock / 2), Some(model.value as u32), "seed {seed} step {step}: read in the past");
                    }
                    assert_eq!(cpu.ppb_peek32(CTRL, clock + ahead), Some(u32::from(model.enabled)), "seed {seed} step {step}: CTRL");
                }
            }
        }
        assert_eq!(cpu.ppb_peek32(CYCCNT, clock), Some(model.value as u32), "seed {seed}: final value");
    }
}
