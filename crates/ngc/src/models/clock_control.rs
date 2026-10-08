// Ported from emulation/models/NGCClockControl.cs of the analysis workspace.

//! `NGCClockControl`: the minimal STM32L4 RCC shim recovered from the handset's HAL start-up
//! (`rcc @ 0x40021000` on both boards). It is an explicit functional approximation, not an oscillator or
//! timing model: every register is plain storage keyed by its exact offset, except that the "ready"
//! status bits are derived from the "on" request bits **on every read** (the stored value is not changed
//! by a read):
//!
//! | offset | register (STM32L4 names, inferred) | derived bits |
//! | --- | --- | --- |
//! | `0x00` | `CR` | bit 1 = bit 0 (MSI), bit 10 = bit 8 (HSI16), bit 17 = bit 16 (HSE), bit 25 = bit 24 (PLL), bit 27 = bit 26 (PLLSAI1), bit 29 = bit 28 (PLLSAI2) |
//! | `0x08` | `CFGR` | `SWS[3:2]` = `SW[1:0]` (the system clock switch completes immediately) |
//! | `0x90`, `0x94`, `0x98` | `BDCR`, `CSR`, `CRRCR` | bit 1 = bit 0 (LSE, LSI, HSI48 ready) |
//!
//! `CR` resets to `0x63`. Only 32-bit accesses are accepted (`IDoubleWordPeripheral`, no translations);
//! byte and halfword accesses are logged and ignored by the bus. Peripheral clock frequencies are
//! configured in the platform, not derived from these registers.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, View, Width};
use std::collections::BTreeMap;

/// `Size`: the register window.
pub const SIZE: u32 = 0x400;

/// Reset value of `CR` (`values[0] = 0x63`).
pub const CR_RESET_VALUE: u32 = 0x63;

/// Register offsets with derived bits.
pub mod reg {
    pub const CR: u32 = 0x00;
    pub const CFGR: u32 = 0x08;
    pub const BDCR: u32 = 0x90;
    pub const CSR: u32 = 0x94;
    pub const CRRCR: u32 = 0x98;
}

/// A `Dictionary<long, uint>` keyed by register offset (absent keys read 0): aligned offsets of the
/// 0x400-byte window are kept in a flat array, any other key (an unaligned offset reached through a
/// monitor-style access) in a sorted map, so the semantics are exactly those of the dictionary without
/// hashing on the hot path.
pub(crate) struct WordStore {
    words: Box<[u32; (SIZE / 4) as usize]>,
    other: BTreeMap<u32, u32>,
}

impl WordStore {
    pub(crate) fn new() -> Self {
        Self { words: Box::new([0; (SIZE / 4) as usize]), other: BTreeMap::new() }
    }

    pub(crate) fn clear(&mut self) {
        self.words.fill(0);
        self.other.clear();
    }

    fn slot(offset: u32) -> Option<usize> {
        (offset & 3 == 0 && offset < SIZE).then_some((offset / 4) as usize)
    }

    /// `values.TryGetValue(offset, out v)` with 0 for a missing key.
    #[inline]
    pub(crate) fn get(&self, offset: u32) -> u32 {
        match Self::slot(offset) {
            Some(slot) => self.words[slot],
            None => self.other.get(&offset).copied().unwrap_or(0),
        }
    }

    /// `values[offset] = value`.
    #[inline]
    pub(crate) fn set(&mut self, offset: u32, value: u32) {
        match Self::slot(offset) {
            Some(slot) => self.words[slot] = value,
            None => {
                self.other.insert(offset, value);
            }
        }
    }
}

/// `Ready(v, on, ready)`: copies bit `on` into bit `ready`.
const fn ready(v: u32, on: u32, ready: u32) -> u32 {
    (v & !(1 << ready)) | (((v >> on) & 1) << ready)
}

/// `Miscellaneous.NGCClockControl`.
pub struct NgcClockControl {
    name: String,
    values: WordStore,
}

impl NgcClockControl {
    pub fn new(name: impl Into<String>) -> Self {
        let mut rcc = Self { name: name.into(), values: WordStore::new() };
        rcc.reset_state();
        rcc
    }

    fn reset_state(&mut self) {
        self.values.clear();
        self.values.set(reg::CR, CR_RESET_VALUE);
    }

    /// The stored (written) value of a register, without the derived ready bits.
    pub fn stored(&self, offset: u32) -> u32 {
        self.values.get(offset)
    }

    /// What a read of `offset` returns: the stored value with the derived bits applied.
    pub fn register(&self, offset: u32) -> u32 {
        let mut v = self.values.get(offset);
        if offset == reg::CR {
            v = ready(v, 0, 1);
            v = ready(v, 8, 10);
            v = ready(v, 16, 17);
            v = ready(v, 24, 25);
            v = ready(v, 26, 27);
            v = ready(v, 28, 29);
        }
        if offset == reg::CFGR {
            v = (v & !0xC) | ((v & 3) << 2);
        }
        if offset == reg::BDCR || offset == reg::CSR || offset == reg::CRRCR {
            v = ready(v, 0, 1);
        }
        v
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        format!(
            "{}: CR=0x{:08X} CFGR=0x{:08X} BDCR=0x{:08X} CSR=0x{:08X}",
            self.name,
            self.register(reg::CR),
            self.register(reg::CFGR),
            self.register(reg::BDCR),
            self.register(reg::CSR)
        )
    }
}

impl Peripheral for NgcClockControl {
    fn name(&self) -> &str {
        &self.name
    }

    fn reset(&mut self, _ctx: &mut Ctx<'_>) {
        self.reset_state();
    }

    fn read(&mut self, offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        self.register(offset)
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, _ctx: &mut Ctx<'_>) {
        self.values.set(offset, value);
    }

    // IDoubleWordPeripheral only, no [AllowedTranslations].
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    /// Reads have no side effects, so the peek is the read.
    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        Some(self.register(offset))
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
    use emu_core::PeriphId;

    const BASE: u32 = 0x4002_1000;

    fn setup() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, NgcClockControl::new("rcc"));
        (h, id)
    }

    #[test]
    fn cr_resets_to_0x63_and_everything_else_to_zero() {
        let (mut h, _id) = setup();
        assert_eq!(h.read32(BASE + reg::CR), 0x63);
        for offset in (4..SIZE).step_by(4) {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:X}");
        }
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    }

    #[test]
    fn cr_ready_bits_follow_the_on_bits_on_every_read() {
        let (mut h, id) = setup();
        // Every "on" bit set, every "ready" bit clear.
        let on_bits: u32 = (1 << 0) | (1 << 8) | (1 << 16) | (1 << 24) | (1 << 26) | (1 << 28);
        h.write32(BASE + reg::CR, on_bits);
        assert_eq!(h.read32(BASE + reg::CR), 0x3F03_0503);
        assert_eq!(h.read32(BASE + reg::CR), on_bits | (1 << 1) | (1 << 10) | (1 << 17) | (1 << 25) | (1 << 27) | (1 << 29));
        // The stored value is untouched by reads.
        assert_eq!(h.get::<NgcClockControl>(id).stored(reg::CR), on_bits);
        // A ready bit written as 1 without its "on" bit reads back 0 (the model derives it).
        h.write32(BASE + reg::CR, (1 << 1) | (1 << 10) | (1 << 17) | (1 << 25) | (1 << 27) | (1 << 29));
        assert_eq!(h.read32(BASE + reg::CR), 0);
        // One oscillator at a time.
        for (on, rdy) in [(0, 1), (8, 10), (16, 17), (24, 25), (26, 27), (28, 29)] {
            h.write32(BASE + reg::CR, 1 << on);
            assert_eq!(h.read32(BASE + reg::CR), (1 << on) | (1 << rdy), "bit {on}");
        }
        // Other bits pass through unchanged (e.g. the MSI range, HSE bypass).
        h.write32(BASE + reg::CR, 0x0004_0060);
        assert_eq!(h.read32(BASE + reg::CR), 0x0004_0060);
    }

    #[test]
    fn cfgr_status_bits_mirror_the_switch() {
        let (mut h, id) = setup();
        for (written, expected) in [(0u32, 0u32), (1, 0x5), (2, 0xA), (3, 0xF), (0x0C, 0), (0xFF, 0xFF), (0x1_0002, 0x1_000A), (0x1_00F1, 0x1_00F5)] {
            h.write32(BASE + reg::CFGR, written);
            assert_eq!(h.read32(BASE + reg::CFGR), expected, "CFGR written 0x{written:X}");
        }
        assert_eq!(h.get::<NgcClockControl>(id).stored(reg::CFGR), 0x1_00F1, "reads do not modify the stored value");
    }

    #[test]
    fn lse_lsi_and_hsi48_ready_bits() {
        let (mut h, _id) = setup();
        for offset in [reg::BDCR, reg::CSR, reg::CRRCR] {
            assert_eq!(h.read32(BASE + offset), 0);
            h.write32(BASE + offset, 0x0000_0101);
            assert_eq!(h.read32(BASE + offset), 0x0000_0103, "offset 0x{offset:X}");
            h.write32(BASE + offset, 0x0000_0102);
            assert_eq!(h.read32(BASE + offset), 0x0000_0100, "ready follows the on bit down");
        }
        // The neighbouring registers have no derived bits.
        h.write32(BASE + 0x8C, 1);
        h.write32(BASE + 0x9C, 1);
        assert_eq!(h.read32(BASE + 0x8C), 1);
        assert_eq!(h.read32(BASE + 0x9C), 1);
    }

    #[test]
    fn every_other_offset_is_plain_storage() {
        let (mut h, id) = setup();
        for offset in (0x4..SIZE).step_by(4).filter(|o| ![reg::CFGR, reg::BDCR, reg::CSR, reg::CRRCR].contains(o)) {
            h.write32(BASE + offset, 0xA5A5_0000 | offset);
        }
        for offset in (0x4..SIZE).step_by(4).filter(|o| ![reg::CFGR, reg::BDCR, reg::CSR, reg::CRRCR].contains(o)) {
            assert_eq!(h.read32(BASE + offset), 0xA5A5_0000 | offset, "offset 0x{offset:X}");
        }
        assert_eq!(h.get::<NgcClockControl>(id).stored(0x4C), 0xA5A5_004C);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn reset_clears_the_registers_and_restores_cr() {
        let (mut h, _id) = setup();
        h.write32(BASE + reg::CR, 0x1111_0100);
        h.write32(BASE + 0x58, 0xFFFF_FFFF);
        h.write32(BASE + 0x1, 0x7);
        h.core_mut().reset_all();
        assert_eq!(h.read32(BASE + reg::CR), 0x63);
        assert_eq!(h.read32(BASE + 0x58), 0);
        assert_eq!(h.read(BASE + 0x1, Width::Word), 0, "keys outside the aligned window are cleared too");
    }

    #[test]
    fn unaligned_keys_are_independent_of_the_aligned_register() {
        // A monitor-style access (not split like a CPU access) with an unaligned offset is a different
        // dictionary key in the C# model.
        let (mut h, id) = setup();
        h.write(BASE + 0x1, Width::Word, 0xDEAD_BEEF);
        assert_eq!(h.read(BASE + 0x1, Width::Word), 0xDEAD_BEEF);
        assert_eq!(h.read32(BASE + 0x0), 0x63, "CR is untouched");
        assert_eq!(h.get::<NgcClockControl>(id).stored(1), 0xDEAD_BEEF);
        assert_eq!(h.get::<NgcClockControl>(id).stored(4), 0);
        // Keys 0x91 etc. do not get the derived bits of 0x90.
        h.write(BASE + 0x91, Width::Word, 1);
        assert_eq!(h.read(BASE + 0x91, Width::Word), 1);
    }

    #[test]
    fn matches_the_access_probe_of_the_renode_reference_run() {
        // Access probe recorded from Renode 1.17.0 on 2026-10-08: `sysbus ReadByte 0x40021000` replied 0 with
        // "Attempted Byte read isn't supported by the peripheral. Offset 0x0."; a dword read inside the
        // window at the undefined offset 0x2C replied 0 without any log.
        let (mut h, _id) = setup();
        assert_eq!(h.read8(BASE), 0);
        assert_eq!(h.read32(BASE + 0x2C), 0);
        assert_eq!(h.warnings(), ["rcc: Attempted Byte read isn't supported by the peripheral. Offset 0x0."]);
    }

    #[test]
    fn only_word_accesses_are_accepted() {
        let (mut h, _id) = setup();
        h.write8(BASE + reg::CR, 0xFF);
        h.write16(BASE + reg::CR, 0xFFFF);
        assert_eq!(h.read8(BASE + reg::CR), 0);
        assert_eq!(h.read16(BASE + reg::CR), 0);
        assert_eq!(h.read32(BASE + reg::CR), 0x63, "the dropped writes changed nothing");
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("isn't supported by the peripheral")), "{warnings:?}");
    }

    #[test]
    fn peek_equals_read_and_summary_shows_the_derived_values() {
        let (mut h, id) = setup();
        h.write32(BASE + reg::CR, 1 << 8);
        h.write32(BASE + reg::CFGR, 2);
        for offset in [reg::CR, reg::CFGR, reg::BDCR, 0x58] {
            assert_eq!(h.peek(BASE + offset, Width::Word), Some(h.read32(BASE + offset)));
        }
        let expected = "rcc: CR=0x00000500 CFGR=0x0000000A BDCR=0x00000000 CSR=0x00000000";
        assert_eq!(h.get::<NgcClockControl>(id).describe(), expected);
        assert!(h.core().summaries().iter().any(|(n, s)| n == "rcc" && s == expected));
    }

    #[test]
    fn word_store_matches_dictionary_semantics() {
        let mut store = WordStore::new();
        assert_eq!(store.get(0), 0);
        store.set(0x3FC, 7);
        store.set(0x400, 8);
        store.set(2, 9);
        assert_eq!((store.get(0x3FC), store.get(0x400), store.get(2), store.get(0)), (7, 8, 9, 0));
        store.clear();
        assert_eq!((store.get(0x3FC), store.get(0x400), store.get(2)), (0, 0, 0));
    }
}
