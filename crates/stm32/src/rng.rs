// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Miscellaneous/STM32_RNG.cs
// (MIT License, Copyright (c) Antmicro).

//! `Miscellaneous.STM32_RNG` (`series: STM32Series.F7`; handset `rng @ 0x50060800`, `IRQ -> nvic@80`).
//!
//! * `CR`: `RNGEN` (bit 2) and `IE` (bit 3); the interrupt output (**line 0**) is `RNGEN && IE`, updated by
//!   the change callbacks of the two flags only;
//! * `SR`: `DRDY` (bit 0) simply mirrors `RNGEN` (a number is always ready); the error flags are tags;
//! * `DR`: a new random number on every read while `RNGEN` is set, 0 otherwise;
//! * L5 only: `CONDRST` (a stored flag) and `HTCR` (health test control with the `0x17590ABC` magic warning).
//!
//! # Determinism
//!
//! Renode draws `DR` from `Emulation.RandomGenerator`, a `System.Random` per thread seeded from a process-wide
//! random base seed, so its values differ on every run. This engine must be deterministic, so the values
//! come from a SplitMix64 generator with a **fixed default seed** ([`Rng::DEFAULT_SEED`], changeable with
//! [`Rng::with_seed`] / [`Rng::set_seed`]). The values therefore differ from any Renode run and from any
//! physical device; only their distribution properties match: like `System.Random.Next()` they are
//! non-negative 31-bit integers (bit 31 is always clear, `0x7FFFFFFF` is never returned).
//!
//! // Renode parity: `Reset()` only resets the registers, it does not re-evaluate the interrupt output, so
//! an asserted `IRQ` line stays asserted across a reset until `RNGEN`/`IE` change again.

use crate::crc::Stm32Series;
use crate::gpio::regfw::{Hooks, Mode, Model, RegisterBuilder, RegisterFile};
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, View, Width};

/// `IKnownSize.Size`.
pub const SIZE: u32 = 0x400;

/// Register offsets.
pub mod reg {
    pub const CR: u32 = 0x00;
    pub const SR: u32 = 0x04;
    pub const DR: u32 = 0x08;
    /// L5 only.
    pub const HTCR: u32 = 0x10;
}

/// The interrupt output line.
pub const IRQ_LINE: u32 = 0;

const R_CR: usize = 0;
const R_SR: usize = 1;
const R_DR: usize = 2;
const R_HTCR: usize = 3;

/// Field indices inside `CR`.
const F_RNGEN: usize = 0;
const F_IE: usize = 1;

/// `HealthTestControlMagic`.
const HEALTH_TEST_CONTROL_MAGIC: u32 = 0x1759_0ABC;

/// Warn-once key of the health-test-control warning.
const KEY_MAGIC: u64 = 1 << 40;

/// SplitMix64 (public domain); small, fast, well distributed and trivially reproducible.
struct Prng {
    state: u64,
}

impl Prng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Like `System.Random.Next()`: an integer in `0..0x7FFFFFFF`.
    fn next_i31(&mut self) -> u32 {
        loop {
            let value = (self.next_u64() >> 33) as u32;
            if value != 0x7FFF_FFFF {
                return value;
            }
        }
    }
}

struct RngDev {
    series: Stm32Series,
    prng: Prng,
    /// Number of values handed out by `DR` reads (diagnostics).
    draws: u64,
}

impl RngDev {
    /// `Update`: `IRQ.Set(enable.Value && interruptEnable.Value)`.
    fn update(regs: &RegisterFile, ctx: &mut Ctx<'_>) {
        ctx.set_output(IRQ_LINE, regs.field(R_CR, F_RNGEN) != 0 && regs.field(R_CR, F_IE) != 0);
    }
}

impl Model for RngDev {
    fn provide(&mut self, regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, _field: usize, current: u32) -> u32 {
        match reg {
            // DRDY: always ready while the generator is enabled.
            R_SR => regs.field(R_CR, F_RNGEN),
            R_DR => {
                if regs.field(R_CR, F_RNGEN) != 0 {
                    self.draws += 1;
                    self.prng.next_i31()
                } else {
                    0
                }
            }
            _ => current,
        }
    }

    fn field_changed(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, _field: usize, _old: u32, _new: u32) {
        if reg == R_CR {
            Self::update(regs, ctx);
        }
    }

    fn field_written(&mut self, _regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, _field: usize, old: u32, written: u32) {
        if reg == R_HTCR && old != HEALTH_TEST_CONTROL_MAGIC && written != HEALTH_TEST_CONTROL_MAGIC {
            ctx.warn_once(
                KEY_MAGIC | u64::from(written),
                format_args!("Magic value 0x{HEALTH_TEST_CONTROL_MAGIC:X} not written before 0x{written:X}"),
            );
        }
    }
}

fn supported(series: Stm32Series) -> bool {
    matches!(series, Stm32Series::F4 | Stm32Series::F7 | Stm32Series::H7 | Stm32Series::L0 | Stm32Series::L5)
}

fn build_registers(series: Stm32Series) -> RegisterFile {
    let mut cr = RegisterBuilder::new(reg::CR)
        .reserved(0, 2)
        // RNGEN, IE: change callbacks re-evaluate the interrupt line.
        .field(2, 1, Mode::READ_WRITE, Hooks::CHANGE)
        .field(3, 1, Mode::READ_WRITE, Hooks::CHANGE);
    if series == Stm32Series::L5 {
        cr = cr
            .reserved(4, 1)
            .tagged_flag("CED", 5)
            .reserved(6, 2)
            .tag("RND_CONFIG3", 8, 4)
            .tagged_flag("NISTC", 12)
            .tag("RND_CONFIG2", 13, 3)
            .tag("CLKDIV", 16, 4)
            .tag("RND_CONFIG1", 20, 6)
            .reserved(26, 4)
            // CONDRST: "Nothing to do for the emulation".
            .field(30, 1, Mode::READ_WRITE, Hooks::NONE)
            .tagged_flag("CONFIGLOCK", 31);
    } else {
        cr = cr.reserved(4, 28);
    }
    let sr = RegisterBuilder::new(reg::SR)
        .field(0, 1, Mode::READ, Hooks::PROVIDER)
        .tag("CECS", 1, 1)
        .tag("SECS", 2, 1)
        .reserved(3, 2)
        .tag("CEIS", 5, 1)
        .tag("SEIS", 6, 1)
        .reserved(7, 25);
    let dr = RegisterBuilder::new(reg::DR).field(0, 32, Mode::READ, Hooks::PROVIDER);
    let mut registers = vec![cr, sr, dr];
    if series == Stm32Series::L5 {
        registers.push(RegisterBuilder::new(reg::HTCR).field(0, 32, Mode::READ_WRITE, Hooks::WRITE));
    }
    RegisterFile::new(registers)
}

/// `Miscellaneous.STM32_RNG`.
pub struct Rng {
    name: String,
    regs: RegisterFile,
    dev: RngDev,
}

impl Rng {
    /// Seed of the generator unless another one is given ("NGC_RNG" in ASCII).
    pub const DEFAULT_SEED: u64 = 0x4E47_435F_524E_4700;

    /// Builds the RNG of `series` (F4, F7, H7, L0 and L5); panics on another series (Renode throws a
    /// `ConstructionException`).
    pub fn new(name: impl Into<String>, series: Stm32Series) -> Self {
        Self::with_seed(name, series, Self::DEFAULT_SEED)
    }

    pub fn with_seed(name: impl Into<String>, series: Stm32Series, seed: u64) -> Self {
        match Self::try_new(name, series, seed) {
            Ok(rng) => rng,
            Err(message) => panic!("cannot construct STM32_RNG: {message}"),
        }
    }

    pub fn try_new(name: impl Into<String>, series: Stm32Series, seed: u64) -> Result<Self, String> {
        if !supported(series) {
            return Err(format!("Unsupported STM32 series value: {series}!"));
        }
        Ok(Self {
            name: name.into(),
            regs: build_registers(series),
            dev: RngDev { series, prng: Prng::new(seed), draws: 0 },
        })
    }

    /// Restarts the number stream from `seed`.
    pub fn set_seed(&mut self, seed: u64) {
        self.dev.prng = Prng::new(seed);
        self.dev.draws = 0;
    }

    pub fn series(&self) -> Stm32Series {
        self.dev.series
    }

    /// `RNGEN`.
    pub fn enabled(&self) -> bool {
        self.regs.field(R_CR, F_RNGEN) != 0
    }

    /// `IE`.
    pub fn interrupt_enabled(&self) -> bool {
        self.regs.field(R_CR, F_IE) != 0
    }

    /// Number of random values returned so far.
    pub fn draws(&self) -> u64 {
        self.dev.draws
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        format!(
            "{}: series={} RNGEN={} IE={} draws={}",
            self.name,
            self.dev.series,
            u8::from(self.enabled()),
            u8::from(self.interrupt_enabled()),
            self.dev.draws
        )
    }

    fn peek_register(&self, offset: u32) -> Option<u32> {
        match offset {
            reg::CR => Some(self.regs.value(R_CR)),
            reg::SR => Some(self.regs.field(R_CR, F_RNGEN)),
            // Reading DR draws a number: there is no side-effect-free view.
            reg::DR => None,
            reg::HTCR if self.dev.series == Stm32Series::L5 => Some(self.regs.value(R_HTCR)),
            _ => Some(0),
        }
    }
}

impl Peripheral for Rng {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset`: registers only; the interrupt output is deliberately left as it is (Renode parity).
    fn reset(&mut self, _ctx: &mut Ctx<'_>) {
        self.regs.reset();
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        self.regs.read(offset, &mut self.dev, ctx)
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.regs.write(offset, value, &mut self.dev, ctx);
    }

    // IDoubleWordPeripheral without [AllowedTranslations].
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        self.peek_register(offset)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.describe()
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{PeriphId, Time};

    const BASE: u32 = 0x5006_0800;
    const CR: u32 = BASE + reg::CR;
    const SR: u32 = BASE + reg::SR;
    const DR: u32 = BASE + reg::DR;
    const HTCR: u32 = BASE + reg::HTCR;

    const RNGEN: u32 = 1 << 2;
    const IE: u32 = 1 << 3;

    fn setup(series: Stm32Series) -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Rng::new("rng", series));
        (h, id)
    }

    fn f7() -> (Harness, PeriphId) {
        setup(Stm32Series::F7)
    }

    fn levels(changes: Vec<(Time, bool)>) -> Vec<bool> {
        changes.into_iter().map(|(_, level)| level).collect()
    }

    // ---- registers --------------------------------------------------------------------------

    #[test]
    fn reset_values_and_enable_flags() {
        let (mut h, id) = f7();
        assert_eq!(h.read32(CR), 0);
        assert_eq!(h.read32(SR), 0);
        assert_eq!(h.read32(DR), 0, "DR reads 0 while disabled");
        assert_eq!(h.read32(DR), 0);
        h.write32(CR, RNGEN);
        assert_eq!(h.read32(CR), RNGEN);
        assert_eq!(h.read32(SR), 1, "DRDY mirrors RNGEN");
        h.write32(CR, RNGEN | IE);
        assert_eq!(h.read32(CR), RNGEN | IE);
        assert!(h.get::<Rng>(id).enabled() && h.get::<Rng>(id).interrupt_enabled());
        h.write32(CR, 0);
        assert_eq!(h.read32(SR), 0);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn reserved_cr_bits_warn_and_are_not_stored() {
        let (mut h, _id) = f7();
        h.write32(CR, 0x0000_000F);
        assert_eq!(h.read32(CR), RNGEN | IE);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x0. Unhandled bits: [0-1] when writing value 0xF. Tags: RESERVED (0x3)."]);
        h.write32(CR, 0xFFFF_FFF0 | RNGEN | IE);
        assert_eq!(h.read32(CR), RNGEN | IE);
        assert_eq!(h.warnings().len(), 2);
        assert_eq!(
            h.warnings()[1],
            "Unhandled write to offset 0x0. Unhandled bits: [4-31] when writing value 0xFFFFFFFC. Tags: RESERVED (0xFFFFFFF)."
        );
    }

    #[test]
    fn sr_writes_log_the_error_flag_tags_like_hal_clear_it() {
        let (mut h, _id) = f7();
        // `__HAL_RNG_CLEAR_IT(hrng, RNG_IT_CEI)` stores ~RNG_IT_CEI.
        h.write32(SR, 0xFFFF_FFDF);
        assert_eq!(h.read32(SR), 0);
        assert_eq!(
            h.warnings(),
            ["Unhandled write to offset 0x4. Unhandled bits: [1-4, 6-31] when writing value 0xFFFFFFDF. Tags: CECS (0x1), SECS (0x1), RESERVED (0x3), SEIS (0x1), RESERVED (0x1FFFFFF)."]
        );
        // Writing 0 is silent.
        h.write32(SR, 0);
        assert_eq!(h.warnings().len(), 1);
    }

    #[test]
    fn dr_is_read_only_and_has_no_unhandled_bits() {
        let (mut h, _id) = f7();
        h.write32(CR, RNGEN);
        h.write32(DR, 0xFFFF_FFFF);
        assert!(h.warnings().is_empty());
        assert_ne!(h.read32(DR), 0xFFFF_FFFF);
    }

    #[test]
    fn health_test_control_exists_only_on_l5() {
        let (mut h, _id) = f7();
        assert_eq!(h.read32(HTCR), 0);
        h.write32(HTCR, 5);
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x10.", "Unhandled write to offset 0x10, value 0x5."]);
    }

    #[test]
    fn l5_has_condrst_tags_and_the_health_test_magic() {
        let (mut h, _id) = setup(Stm32Series::L5);
        // CONDRST (bit 30) is stored, CONFIGLOCK (bit 31) is a tag, the reserved bits are tags.
        h.write32(CR, 0xC000_0000);
        assert_eq!(h.read32(CR), 0x4000_0000);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x0. Unhandled bits: [31] when writing value 0xC0000000. Tags: CONFIGLOCK (0x1)."]);
        h.write32(CR, RNGEN | IE | 0x10);
        assert_eq!(h.read32(CR), RNGEN | IE);
        assert!(h.warnings()[1].contains("RESERVED (0x1)"), "{:?}", h.warnings());
        // Health test control: warn unless the magic is written first.
        h.write32(HTCR, 0x1234);
        assert_eq!(h.warnings().last().unwrap(), "Magic value 0x17590ABC not written before 0x1234");
        let count = h.warnings().len();
        h.write32(HTCR, 0x1759_0ABC);
        h.write32(HTCR, 0x4321);
        assert_eq!(h.warnings().len(), count, "magic then data: no warning");
        h.write32(HTCR, 0x1111);
        assert_eq!(h.warnings().len(), count + 1, "the previous value is not the magic any more");
        assert_eq!(h.read32(HTCR), 0x1111);
    }

    #[test]
    fn unimplemented_offsets_log_and_read_zero() {
        let (mut h, _id) = f7();
        assert_eq!(h.read32(BASE + 0xC), 0);
        assert_eq!(h.read32(BASE + 0x3FC), 0);
        assert_eq!(h.warnings(), ["Unhandled read from offset 0xC.", "Unhandled read from offset 0x3FC."]);
    }

    #[test]
    fn only_word_accesses_are_supported() {
        let (mut h, _id) = f7();
        h.write8(CR, RNGEN);
        h.write16(CR, RNGEN);
        assert_eq!(h.read8(SR), 0);
        assert_eq!(h.read32(CR), 0);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("isn't supported by the peripheral")));
    }

    // ---- interrupt line ----------------------------------------------------------------------

    #[test]
    fn irq_follows_rngen_and_ie() {
        let (mut h, id) = f7();
        let probe = h.probe(id, IRQ_LINE);
        h.connect_irq(id, IRQ_LINE, 80);
        h.clear_irq_changes();
        h.write32(CR, IE);
        assert!(!h.irq_level(80), "interrupt enabled but generator off");
        h.write32(CR, IE | RNGEN);
        assert!(h.irq_level(80));
        h.write32(CR, RNGEN);
        assert!(!h.irq_level(80), "IE cleared");
        h.write32(CR, RNGEN | IE);
        assert!(h.irq_level(80));
        h.write32(CR, IE);
        assert!(!h.irq_level(80), "RNGEN cleared");
        assert_eq!(levels(h.probe_changes(probe)), [true, false, true, false]);
        // Writing the same value again is not a change: no new edge.
        h.write32(CR, IE);
        assert_eq!(levels(h.probe_changes(probe)).len(), 4);
    }

    #[test]
    fn reset_clears_the_registers_but_not_the_interrupt_line() {
        // Renode parity: `Reset()` does not call `Update()`.
        let (mut h, id) = f7();
        h.connect_irq(id, IRQ_LINE, 80);
        h.write32(CR, RNGEN | IE);
        assert!(h.irq_level(80));
        h.core_mut().reset_all();
        assert_eq!(h.read32(CR), 0);
        assert_eq!(h.read32(SR), 0);
        assert!(h.irq_level(80), "the asserted line survives the reset");
        assert!(h.core().output_level(id, IRQ_LINE));
        // The next change of RNGEN/IE re-evaluates it.
        h.write32(CR, RNGEN | IE);
        assert!(h.irq_level(80));
        h.write32(CR, 0);
        assert!(!h.irq_level(80));
    }

    // ---- random numbers ------------------------------------------------------------------------

    fn draw(h: &mut Harness, count: usize) -> Vec<u32> {
        (0..count).map(|_| h.read32(DR)).collect()
    }

    #[test]
    fn numbers_are_deterministic_for_a_seed_and_differ_between_seeds() {
        let (mut a, _ida) = f7();
        let (mut b, _idb) = f7();
        a.write32(CR, RNGEN);
        b.write32(CR, RNGEN);
        let first = draw(&mut a, 32);
        assert_eq!(first, draw(&mut b, 32), "same default seed, same stream");
        assert_ne!(first, draw(&mut a, 32), "the stream advances");

        let mut other = Harness::new();
        other.add_mapped(BASE, 0x400, Rng::with_seed("rng", Stm32Series::F7, 12345));
        other.write32(CR, RNGEN);
        assert_ne!(first, draw(&mut other, 32), "a different seed gives a different stream");
    }

    #[test]
    fn numbers_are_31_bit_and_do_not_repeat_immediately() {
        let (mut h, id) = f7();
        h.write32(CR, RNGEN);
        let values = draw(&mut h, 20_000);
        assert!(values.iter().all(|&v| v < 0x7FFF_FFFF), "like System.Random.Next(): bit 31 clear");
        assert!(values.windows(2).all(|w| w[0] != w[1]));
        assert!(values.iter().any(|&v| v & 0x4000_0000 != 0) && values.iter().any(|&v| v & 1 != 0));
        // Roughly uniform: the mean of the top 31 bits is near the middle.
        let mean = values.iter().map(|&v| u64::from(v)).sum::<u64>() / values.len() as u64;
        assert!((0x3000_0000..0x5000_0000).contains(&mean), "mean 0x{mean:X}");
        assert_eq!(h.get::<Rng>(id).draws(), 20_000);
    }

    #[test]
    fn disabling_stops_the_stream_without_consuming_numbers() {
        let (mut a, _ida) = f7();
        let (mut b, _idb) = f7();
        a.write32(CR, RNGEN);
        b.write32(CR, RNGEN);
        let one = a.read32(DR);
        assert_eq!(one, b.read32(DR));
        a.write32(CR, 0);
        assert_eq!(a.read32(DR), 0);
        assert_eq!(a.read32(DR), 0);
        a.write32(CR, RNGEN);
        assert_eq!(a.read32(DR), b.read32(DR), "disabled reads did not advance the generator");
    }

    #[test]
    fn set_seed_restarts_the_stream() {
        let (mut h, id) = f7();
        h.write32(CR, RNGEN);
        let first = draw(&mut h, 8);
        h.get_mut::<Rng>(id).set_seed(Rng::DEFAULT_SEED);
        assert_eq!(draw(&mut h, 8), first);
        assert_eq!(h.get::<Rng>(id).draws(), 8);
    }

    // ---- misc -----------------------------------------------------------------------------------

    #[test]
    fn peek_has_no_side_effects() {
        let (mut h, id) = f7();
        h.write32(CR, RNGEN | IE);
        assert_eq!(h.peek(CR, Width::Word), Some(RNGEN | IE));
        assert_eq!(h.peek(SR, Width::Word), Some(1));
        assert_eq!(h.peek(DR, Width::Word), None, "reading DR draws a number");
        assert_eq!(h.peek(BASE + 0x10, Width::Word), Some(0));
        assert_eq!(h.get::<Rng>(id).draws(), 0);
        let (mut h2, _id2) = setup(Stm32Series::L5);
        h2.write32(HTCR, 0x1759_0ABC);
        assert_eq!(h2.peek(HTCR, Width::Word), Some(0x1759_0ABC));
    }

    #[test]
    fn summary_and_accessors() {
        let (mut h, id) = f7();
        h.write32(CR, RNGEN);
        let _ = h.read32(DR);
        assert_eq!(h.get::<Rng>(id).describe(), "rng: series=F7 RNGEN=1 IE=0 draws=1");
        assert_eq!(h.get::<Rng>(id).series(), Stm32Series::F7);
        assert!(h.core().summaries().iter().any(|(n, s)| n == "rng" && s.contains("draws=1")));
    }

    #[test]
    fn unsupported_series_are_rejected() {
        for series in [Stm32Series::F0, Stm32Series::F1, Stm32Series::G0, Stm32Series::L1, Stm32Series::WBA] {
            assert!(Rng::try_new("rng", series, 1).is_err(), "{series}");
        }
        for series in [Stm32Series::F4, Stm32Series::F7, Stm32Series::H7, Stm32Series::L0, Stm32Series::L5] {
            assert!(Rng::try_new("rng", series, 1).is_ok(), "{series}");
        }
        assert_eq!(Rng::try_new("rng", Stm32Series::F0, 1).err().unwrap(), "Unsupported STM32 series value: F0!");
    }

    #[test]
    fn access_policy_is_word_only() {
        let rng = Rng::new("rng", Stm32Series::F7);
        let policy = rng.access_policy();
        assert!(policy.native.contains(Width::Word) && !policy.native.contains(Width::Byte) && !policy.native.contains(Width::Half));
        assert_eq!(policy.translations.0, 0);
    }
}
