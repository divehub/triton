// Ported from emulation/models/NGCAdc.cs of the analysis workspace.

//! `NGCAdc`: the handset's functional STM32L4 ADC1/common-register approximation
//! (`adc @ 0x50040000`, `sampleValue: 400`, a synthetic board-ID input; the physical revision is
//! unverified). Calibration and enable complete immediately; a conversion finishes at the **next read of
//! `ISR`**. Samples and conversion timing are synthetic: this model does not reproduce analog electronics,
//! acquisition timing, DMA, injected channels, ADC2/ADC3, interrupt delivery or calibration accuracy.
//!
//! Registers (STM32L4 names, offsets as used by the firmware) are plain storage except:
//!
//! * `ISR` (`0x00`): write-one-to-clear. A read completes a pending conversion first (`ConversionCount`++,
//!   `ISR |= EOC | EOS` (`0xC`), `CR.ADSTART` (bit 2) cleared);
//! * `CR` (`0x08`): `ADEN` (bit 0) is set by writing 1 (writing 0 never disables), which also raises
//!   `ISR.ADRDY` (bit 0); `ADDIS` (bit 1, never stored) disables (`ADEN`, `ADRDY` cleared, pending
//!   conversion dropped); `ADCAL` (bit 31) increments `CalibrationCount` and clears itself at once; `ADSTP`
//!   (bit 4) clears `ADSTART`/`ADSTP` and drops the pending conversion; `ADSTART` (bit 2) with `ADEN` set
//!   starts a conversion; HAL starts conversions with `ADSTART=1, ADEN=0` in the written value, relying on
//!   `ADEN` being sticky;
//! * `DR` (`0x40`): returns `SampleValue & 0xFFF` (always the *current* `SampleValue`), clears `EOC`/`EOS`
//!   and, when `CFGR.CONT` (bit 13) and `ADEN` are set, starts the next conversion.
//!
//! Accesses: 16-bit and 32-bit only (`IWordPeripheral` + `IDoubleWordPeripheral`, no translations): a byte
//! access is logged and ignored by the bus. A 16-bit read is a full 32-bit read (with its side effects) of
//! the aligned word, shifted by `offset & 2`; a 16-bit write merges into the **stored** word and performs
//! a 32-bit write.

use super::clock_control::WordStore;
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, Translations, View, Width, Widths};

/// `Size`: the register window.
pub use super::clock_control::SIZE;

/// Register offsets.
pub mod reg {
    pub const ISR: u32 = 0x00;
    pub const CR: u32 = 0x08;
    pub const CFGR: u32 = 0x0C;
    pub const DR: u32 = 0x40;
}

/// `ADC_ISR` bits used by the model.
const ISR_ADRDY: u32 = 1;
const ISR_EOC_EOS: u32 = 0xC;
/// `ADC_CR` bits used by the model.
const CR_ADEN: u32 = 1;
const CR_ADDIS: u32 = 2;
const CR_ADSTART: u32 = 4;
const CR_ADSTP: u32 = 0x10;
const CR_ADCAL: u32 = 0x8000_0000;
/// `ADC_CFGR.CONT`.
const CFGR_CONT: u32 = 0x2000;
/// ADC1 deep-power-down reset state of `CR`.
const CR_RESET_VALUE: u32 = 0x2000_0000;
/// 12-bit data.
const DATA_MASK: u32 = 0xFFF;

/// `Analog.NGCAdc`.
pub struct NgcAdc {
    name: String,
    /// `SampleValue` (settable at runtime, e.g. the runner's `adc SampleValue 400`).
    sample_value: u32,
    /// `ConversionCount`.
    conversions: u64,
    /// `CalibrationCount`.
    calibrations: u64,
    conversion_pending: bool,
    values: WordStore,
}

impl NgcAdc {
    /// `NGCAdc(sampleValue)`; the handset platform passes 400.
    pub fn new(name: impl Into<String>, sample_value: u32) -> Self {
        let mut adc = Self {
            name: name.into(),
            sample_value,
            conversions: 0,
            calibrations: 0,
            conversion_pending: false,
            values: WordStore::new(),
        };
        adc.reset_state();
        adc
    }

    fn reset_state(&mut self) {
        self.values.clear();
        self.values.set(reg::CR, CR_RESET_VALUE);
        self.conversions = 0;
        self.calibrations = 0;
        self.conversion_pending = false;
    }

    /// `SampleValue` as configured (not masked).
    pub fn sample_value(&self) -> u32 {
        self.sample_value
    }

    /// Changes the synthetic sample (`adc SampleValue <n>`); `DR` returns its low 12 bits.
    pub fn set_sample_value(&mut self, value: u32) {
        self.sample_value = value;
    }

    /// `ConversionCount`: conversions completed by an `ISR` read since reset.
    pub fn conversion_count(&self) -> u64 {
        self.conversions
    }

    /// `CalibrationCount`: `ADCAL` requests since reset.
    pub fn calibration_count(&self) -> u64 {
        self.calibrations
    }

    /// A conversion has been started and will complete at the next `ISR` read.
    pub fn conversion_pending(&self) -> bool {
        self.conversion_pending
    }

    /// The stored word at `offset` (`Get(offset)`).
    pub fn stored(&self, offset: u32) -> u32 {
        self.values.get(offset)
    }

    /// One-line state (the `summary` text; the C# model has no `Summary` property).
    pub fn describe(&self) -> String {
        format!(
            "{}: sample={} conversions={} calibrations={} pending={} ISR=0x{:08X} CR=0x{:08X} CFGR=0x{:08X}",
            self.name,
            self.sample_value,
            self.conversions,
            self.calibrations,
            u8::from(self.conversion_pending),
            self.values.get(reg::ISR),
            self.values.get(reg::CR),
            self.values.get(reg::CFGR)
        )
    }

    /// `ReadDoubleWord`, including its side effects.
    fn read_dword(&mut self, offset: u32) -> u32 {
        if offset == reg::ISR && self.conversion_pending {
            // The conversion finishes now.
            self.conversion_pending = false;
            self.conversions += 1;
            self.values.set(reg::DR, self.sample_value & DATA_MASK);
            self.values.set(reg::ISR, self.values.get(reg::ISR) | ISR_EOC_EOS);
            self.values.set(reg::CR, self.values.get(reg::CR) & !CR_ADSTART);
        }
        let value = self.values.get(offset);
        if offset == reg::DR {
            // Reading DR consumes the synthetic regular conversion.
            self.values.set(reg::ISR, self.values.get(reg::ISR) & !ISR_EOC_EOS);
            if self.values.get(reg::CFGR) & CFGR_CONT != 0 && self.values.get(reg::CR) & CR_ADEN != 0 {
                self.conversion_pending = true;
            }
            return self.sample_value & DATA_MASK;
        }
        value
    }

    /// `WriteDoubleWord`.
    fn write_dword(&mut self, offset: u32, value: u32) {
        if offset == reg::ISR {
            // ADC_ISR uses write-one-to-clear status flags.
            self.values.set(offset, self.values.get(offset) & !value);
            return;
        }
        if offset == reg::CR {
            // ADEN is set by writing one; writing zero does not disable an enabled ADC. HAL starts a
            // conversion with CR.ADSTART=1 and ADEN=0 in the write value, relying on this behaviour.
            let mut control = value | (self.values.get(reg::CR) & CR_ADEN);
            if value & CR_ADCAL != 0 {
                self.calibrations += 1;
                control &= !CR_ADCAL;
            }
            if value & CR_ADDIS != 0 {
                control &= !(CR_ADEN | CR_ADDIS);
                self.values.set(reg::ISR, self.values.get(reg::ISR) & !ISR_ADRDY);
                self.conversion_pending = false;
            } else if value & CR_ADEN != 0 {
                self.values.set(reg::ISR, self.values.get(reg::ISR) | ISR_ADRDY);
            }
            if value & CR_ADSTP != 0 {
                control &= !(CR_ADSTART | CR_ADSTP);
                self.conversion_pending = false;
            } else if value & CR_ADSTART != 0 && control & CR_ADEN != 0 {
                self.conversion_pending = true;
            }
            self.values.set(offset, control & !CR_ADDIS);
            return;
        }
        self.values.set(offset, value);
    }

    /// `ReadWord`.
    fn read_word(&mut self, offset: u32) -> u32 {
        (self.read_dword(offset & !3) >> ((offset & 2) * 8)) & 0xFFFF
    }

    /// `WriteWord`: merges into the stored word, then a 32-bit write.
    fn write_word(&mut self, offset: u32, value: u32) {
        let aligned = offset & !3;
        let shift = (offset & 2) * 8;
        let combined = (self.values.get(aligned) & !(0xFFFFu32 << shift)) | ((value & 0xFFFF) << shift);
        self.write_dword(aligned, combined);
    }

    /// The word a 32-bit read of `offset` returns, without the read's side effects.
    fn peek_dword(&self, offset: u32) -> u32 {
        match offset {
            // A pending conversion would complete (and its flags appear) at this read.
            reg::ISR if self.conversion_pending => self.values.get(reg::ISR) | ISR_EOC_EOS,
            reg::DR => self.sample_value & DATA_MASK,
            _ => self.values.get(offset),
        }
    }
}

impl Peripheral for NgcAdc {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset`: registers, counters and the pending conversion; `SampleValue` is configuration and stays.
    fn reset(&mut self, _ctx: &mut Ctx<'_>) {
        self.reset_state();
    }

    fn read(&mut self, offset: u32, width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        match width {
            Width::Word => self.read_dword(offset),
            Width::Half => self.read_word(offset),
            // Not delivered: the bus rejects byte accesses for this policy.
            Width::Byte => 0,
        }
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, _ctx: &mut Ctx<'_>) {
        match width {
            Width::Word => self.write_dword(offset, value),
            Width::Half => self.write_word(offset, value),
            Width::Byte => {}
        }
    }

    // IDoubleWordPeripheral + IWordPeripheral, no [AllowedTranslations]: byte accesses are not supported.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::new(Widths::HALF | Widths::WORD, Translations::NONE)
    }

    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        match width {
            Width::Word => Some(self.peek_dword(offset)),
            Width::Half => Some((self.peek_dword(offset & !3) >> ((offset & 2) * 8)) & 0xFFFF),
            Width::Byte => None,
        }
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

    const BASE: u32 = 0x5004_0000;
    const ISR: u32 = BASE + reg::ISR;
    const CR: u32 = BASE + reg::CR;
    const CFGR: u32 = BASE + reg::CFGR;
    const DR: u32 = BASE + reg::DR;

    const ADVREGEN: u32 = 1 << 28;
    const DEEPPWD: u32 = 1 << 29;

    fn setup(sample: u32) -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, NgcAdc::new("adc", sample));
        (h, id)
    }

    fn adc(h: &Harness, id: PeriphId) -> &NgcAdc {
        h.get::<NgcAdc>(id)
    }

    /// The HAL sequence: leave deep power down, enable the regulator, calibrate, enable (ADRDY is raised).
    fn enable(h: &mut Harness) {
        let cr = h.read32(CR);
        h.write32(CR, cr & !DEEPPWD);
        h.write32(CR, ADVREGEN);
        h.write32(CR, ADVREGEN | CR_ADCAL);
        let cr = h.read32(CR);
        h.write32(CR, cr | CR_ADEN);
    }

    /// `enable`, then acknowledge ADRDY so that `ISR` reads show only conversion flags.
    fn enable_and_acknowledge(h: &mut Harness) {
        enable(h);
        h.write32(ISR, ISR_ADRDY);
    }

    #[test]
    fn reset_state_is_deep_power_down() {
        let (mut h, id) = setup(400);
        assert_eq!(h.read32(CR), 0x2000_0000);
        assert_eq!(h.read32(ISR), 0);
        assert_eq!(h.read32(CFGR), 0);
        assert_eq!(adc(&h, id).conversion_count(), 0);
        assert_eq!(adc(&h, id).calibration_count(), 0);
        assert!(!adc(&h, id).conversion_pending());
        for offset in [0x04, 0x10, 0x30, 0x300, 0x308, 0x3FC] {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:X}");
        }
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn dr_always_returns_the_current_sample_masked_to_12_bits() {
        let (mut h, id) = setup(400);
        assert_eq!(h.read32(DR), 400, "even before any conversion");
        h.get_mut::<NgcAdc>(id).set_sample_value(4095);
        assert_eq!(h.read32(DR), 4095);
        h.get_mut::<NgcAdc>(id).set_sample_value(0x1FFF);
        assert_eq!(h.read32(DR), 0xFFF);
        assert_eq!(adc(&h, id).sample_value(), 0x1FFF, "the configured value is kept unmasked");
        h.get_mut::<NgcAdc>(id).set_sample_value(0);
        assert_eq!(h.read32(DR), 0);
    }

    #[test]
    fn enable_and_calibration_complete_immediately() {
        let (mut h, id) = setup(400);
        // Leave deep power down (clear DEEPPWD, set ADVREGEN) as HAL does.
        h.write32(CR, ADVREGEN);
        assert_eq!(h.read32(CR), ADVREGEN, "ADVREGEN stored, DEEPPWD gone");
        // ADCAL: counted and cleared at once.
        let cr = h.read32(CR);
        h.write32(CR, cr | CR_ADCAL);
        assert_eq!(h.read32(CR), ADVREGEN, "ADCAL reads back clear immediately");
        assert_eq!(adc(&h, id).calibration_count(), 1);
        // ADEN: sets ISR.ADRDY and stays set.
        assert_eq!(h.read32(ISR) & ISR_ADRDY, 0);
        h.write32(CR, ADVREGEN | CR_ADEN);
        assert_eq!(h.read32(CR), ADVREGEN | CR_ADEN);
        assert_eq!(h.read32(ISR), ISR_ADRDY);
        // ISR is write-one-to-clear.
        h.write32(ISR, ISR_ADRDY);
        assert_eq!(h.read32(ISR), 0);
        // Writing ADEN=0 does not disable an enabled ADC.
        h.write32(CR, ADVREGEN);
        assert_eq!(h.read32(CR), ADVREGEN | CR_ADEN);
        // Another calibration request is counted again.
        h.write32(CR, ADVREGEN | CR_ADCAL);
        assert_eq!(adc(&h, id).calibration_count(), 2);
        assert_eq!(h.read32(CR) & CR_ADCAL, 0);
    }

    #[test]
    fn addis_disables_and_clears_adrdy_and_a_pending_conversion() {
        let (mut h, id) = setup(400);
        enable(&mut h);
        h.write32(CR, ADVREGEN | CR_ADEN | CR_ADSTART);
        assert!(adc(&h, id).conversion_pending());
        h.write32(CR, ADVREGEN | CR_ADDIS);
        assert_eq!(h.read32(CR), ADVREGEN, "ADEN cleared, ADDIS is never stored");
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(h.read32(ISR) & ISR_ADRDY, 0, "ADRDY cleared");
        assert_eq!(adc(&h, id).conversion_count(), 0, "the dropped conversion never completed");
    }

    #[test]
    fn single_conversion_completes_at_the_next_isr_read() {
        let (mut h, id) = setup(1234);
        enable_and_acknowledge(&mut h);
        // HAL: ADSTART with ADEN already set (the written value includes the sticky ADEN as read back).
        // A written value with ADEN set raises ADRDY again (the model does not look at the stored state).
        let cr = h.read32(CR);
        h.write32(CR, cr | CR_ADSTART);
        assert!(adc(&h, id).conversion_pending());
        assert_eq!(h.read32(CR) & CR_ADSTART, CR_ADSTART, "ADSTART stays until the conversion completes");
        // The next ISR read finishes the conversion: EOC | EOS, ADSTART cleared.
        assert_eq!(h.read32(ISR), ISR_ADRDY | ISR_EOC_EOS);
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(adc(&h, id).conversion_count(), 1);
        assert_eq!(h.read32(CR) & CR_ADSTART, 0);
        assert_eq!(adc(&h, id).stored(reg::DR), 1234, "the data register word is latched");
        // Reading DR returns the sample and consumes EOC/EOS; CONT is clear, so no further conversion.
        assert_eq!(h.read32(DR), 1234);
        assert_eq!(h.read32(ISR), ISR_ADRDY);
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(h.read32(ISR), ISR_ADRDY);
        assert_eq!(adc(&h, id).conversion_count(), 1);
    }

    #[test]
    fn hal_style_start_with_aden_clear_in_the_written_value_still_converts() {
        // The model ORs the stored ADEN into the written value: a write of ADSTART alone starts a conversion.
        let (mut h, id) = setup(77);
        enable(&mut h);
        h.write32(CR, CR_ADSTART);
        assert!(adc(&h, id).conversion_pending());
        assert_eq!(h.read32(CR), CR_ADSTART | CR_ADEN, "the written value replaced the other bits, ADEN is sticky");
        assert_eq!(h.read32(ISR) & 0xC, 0xC);
        assert_eq!(h.read32(CR), CR_ADEN, "ADSTART cleared by the completed conversion");
    }

    #[test]
    fn adstart_without_aden_does_not_convert() {
        let (mut h, id) = setup(5);
        h.write32(CR, ADVREGEN | CR_ADSTART);
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(h.read32(ISR), 0);
        assert_eq!(adc(&h, id).conversion_count(), 0);
    }

    #[test]
    fn continuous_mode_restarts_after_each_dr_read() {
        let (mut h, id) = setup(300);
        enable_and_acknowledge(&mut h);
        h.write32(CFGR, CFGR_CONT);
        let cr = h.read32(CR);
        h.write32(CR, cr | CR_ADSTART);
        for round in 1..=4u64 {
            // ADRDY was raised again by the write of ADEN|ADSTART and nobody acknowledged it.
            assert_eq!(h.read32(ISR), ISR_ADRDY | ISR_EOC_EOS, "round {round}");
            assert_eq!(h.read32(DR), 300);
            assert!(adc(&h, id).conversion_pending(), "CONT starts the next conversion after the DR read");
            assert_eq!(adc(&h, id).conversion_count(), round);
        }
        // ADSTP stops it.
        h.write32(CR, CR_ADSTP);
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(h.read32(CR) & (CR_ADSTART | CR_ADSTP), 0);
        assert_eq!(h.read32(ISR), ISR_ADRDY);
        assert_eq!(adc(&h, id).conversion_count(), 4);
    }

    #[test]
    fn dr_read_clears_the_flags_set_by_the_completed_conversion() {
        let (mut h, id) = setup(9);
        enable_and_acknowledge(&mut h);
        h.write32(CR, CR_ADSTART);
        assert_eq!(h.read32(ISR), ISR_EOC_EOS);
        let _ = h.read32(ISR);
        assert_eq!(h.read32(DR), 9);
        assert_eq!(h.read32(ISR), 0);
        assert_eq!(adc(&h, id).conversion_count(), 1);
    }

    #[test]
    fn arbitrary_registers_are_storage_including_the_common_block() {
        let (mut h, id) = setup(400);
        for (offset, value) in [(0x04, 0x11u32), (0x10, 0x22), (0x30, 0x33), (0x34, 0x44), (0x308, 0x0055_0000), (0x3FC, 0x66)] {
            h.write32(BASE + offset, value);
            assert_eq!(h.read32(BASE + offset), value, "offset 0x{offset:X}");
        }
        assert_eq!(adc(&h, id).stored(0x30), 0x33);
        h.write32(CFGR, 0x2001);
        assert_eq!(h.read32(CFGR), 0x2001);
        assert!(h.warnings().is_empty());
    }

    // ---- 16-bit accesses --------------------------------------------------------------------

    #[test]
    fn halfword_reads_shift_the_aligned_word_and_keep_the_side_effects() {
        let (mut h, id) = setup(0x0ABC);
        enable_and_acknowledge(&mut h);
        h.write32(CR, CR_ADSTART);
        assert!(adc(&h, id).conversion_pending());
        // A halfword read of ISR completes the conversion like a word read.
        assert_eq!(h.read16(ISR), ISR_EOC_EOS);
        assert_eq!(adc(&h, id).conversion_count(), 1);
        // DR: low half = sample, high half = 0; both are reads of the whole DR word.
        assert_eq!(h.read16(DR), 0x0ABC);
        assert_eq!(h.read16(DR + 2), 0);
        h.write32(BASE + 0x30, 0xDEAD_BEEF);
        assert_eq!(h.read16(BASE + 0x30), 0xBEEF);
        assert_eq!(h.read16(BASE + 0x32), 0xDEAD);
        // Odd offsets: shift by (offset & 2), aligned to 4.
        assert_eq!(h.read16(BASE + 0x31), 0xBEEF);
        assert_eq!(h.read16(BASE + 0x33), 0xDEAD);
    }

    #[test]
    fn halfword_writes_merge_into_the_stored_word() {
        let (mut h, _id) = setup(400);
        h.write32(BASE + 0x30, 0x1111_2222);
        h.write16(BASE + 0x30, 0xAAAA);
        assert_eq!(h.read32(BASE + 0x30), 0x1111_AAAA);
        h.write16(BASE + 0x32, 0xBBBB);
        assert_eq!(h.read32(BASE + 0x30), 0xBBBB_AAAA);
        // CFGR upper half.
        h.write16(CFGR + 2, 0x0002);
        assert_eq!(h.read32(CFGR), 0x0002_0000);
        // ISR halfword write is write-one-to-clear through the 32-bit write.
        h.write32(CR, ADVREGEN | CR_ADEN);
        assert_eq!(h.read32(ISR), ISR_ADRDY);
        h.write16(ISR, 0x0001);
        assert_eq!(h.read32(ISR), 0);
        // A halfword write to CR goes through the control logic too: ADCAL in the upper half is counted
        // and cleared, and the merged word replaces the whole register (DEEPPWD lived in the upper half).
        let (mut h2, id2) = setup(400);
        h2.write16(CR + 2, 0x8000);
        assert_eq!(adc(&h2, id2).calibration_count(), 1);
        assert_eq!(h2.read32(CR), 0);
    }

    #[test]
    fn byte_accesses_are_not_supported() {
        let (mut h, id) = setup(400);
        h.write8(CR, 0xFF);
        assert_eq!(h.read8(DR), 0);
        assert_eq!(h.read32(CR), 0x2000_0000);
        assert_eq!(adc(&h, id).conversion_count(), 0);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("Attempted Byte write isn't supported by the peripheral"));
        assert!(warnings[1].contains("Attempted Byte read isn't supported by the peripheral"));
    }

    // ---- reset, peek, summary ----------------------------------------------------------------

    #[test]
    fn reset_clears_registers_and_counters_but_keeps_the_sample_value() {
        let (mut h, id) = setup(222);
        enable(&mut h);
        h.write32(CR, CR_ADSTART);
        let _ = h.read32(ISR);
        h.write32(BASE + 0x30, 5);
        h.write32(CR, CR_ADSTART);
        assert!(adc(&h, id).conversion_pending());
        h.core_mut().reset_all();
        assert_eq!(h.read32(CR), 0x2000_0000);
        assert_eq!(h.read32(ISR), 0);
        assert_eq!(h.read32(BASE + 0x30), 0);
        assert_eq!((adc(&h, id).conversion_count(), adc(&h, id).calibration_count()), (0, 0));
        assert!(!adc(&h, id).conversion_pending());
        assert_eq!(adc(&h, id).sample_value(), 222);
        assert_eq!(h.read32(DR), 222);
    }

    #[test]
    fn peek_shows_what_a_read_returns_without_completing_the_conversion() {
        let (mut h, id) = setup(0x0123);
        enable(&mut h);
        h.write32(CR, CR_ADSTART);
        assert_eq!(h.peek(ISR, Width::Word), Some(ISR_ADRDY | ISR_EOC_EOS), "the flags a read would show");
        assert!(adc(&h, id).conversion_pending(), "peek did not complete it");
        assert_eq!(h.peek(DR, Width::Word), Some(0x0123));
        assert_eq!(h.peek(DR, Width::Half), Some(0x0123));
        assert_eq!(h.peek(DR + 2, Width::Half), Some(0));
        assert_eq!(h.peek(DR, Width::Byte), None);
        assert_eq!(h.read32(ISR), ISR_ADRDY | ISR_EOC_EOS);
        assert_eq!(adc(&h, id).conversion_count(), 1);
    }

    #[test]
    fn summary_reports_counters_and_registers() {
        let (mut h, id) = setup(400);
        enable(&mut h);
        assert_eq!(
            adc(&h, id).describe(),
            "adc: sample=400 conversions=0 calibrations=1 pending=0 ISR=0x00000001 CR=0x10000001 CFGR=0x00000000"
        );
        assert!(h.core().summaries().iter().any(|(n, s)| n == "adc" && s.contains("calibrations=1")));
    }

    #[test]
    fn access_policy_is_halfword_and_word_without_translations() {
        let adc = NgcAdc::new("adc", 0);
        let policy = adc.access_policy();
        assert!(policy.native.contains(Width::Half) && policy.native.contains(Width::Word) && !policy.native.contains(Width::Byte));
        assert_eq!(policy.translations, Translations::NONE);
    }
}
