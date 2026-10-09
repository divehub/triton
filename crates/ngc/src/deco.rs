//! Decompression state handling: a read-only health report and a labeled storage fixture (DESIGN.md "Decompression
//! state handling").
//!
//! The original TRITON main 5.8 firmware keeps 16 tissue records (N2 and He pressure floats) in RAM and loads them from
//! 32 EEPROM words at start-up. Its initializer (`0x08008308`) resets the tissues when the saved *last decompression
//! date* is four days or more old (or erased) and writes the date, but it only *saves* the tissues on the power-down
//! route (handset CAN `0x149`, or a queue case), never at start-up. A profile that was booted once and then restarted
//! therefore holds a date and an erased tissue block: the next start loads 32 erased words as NaN, keeps them (the
//! elapsed time is under four days) and the no-decompression limit stays at 99 minutes for good. With the oxygen cells
//! uncalibrated in the measured-ppO2 mode the ppO2 is NaN and the limit stays at 99 as well. Both are behaviors of the
//! original firmware, not of this engine.
//!
//! This module adds two things, both for the TRITON main image only (the addresses are in
//! [`crate::firmware::ReleaseAddresses`]; another release reports *unknown* with the reason):
//!
//! * [`health`]: peeks (no side effects) at the RAM words above and reports whether the tissues are finite and whether
//!   the oxygen cells are calibrated. It changes nothing.
//! * [`storage_fixture`]: **an emulator fixture, on by default and switchable**. Before a board is created it looks at
//!   the stored EEPROM image and, if the tissue block is entirely erased while the date record is set, erases the date
//!   record so that the firmware takes its own four-day reset path. It touches no other byte: not the oxygen
//!   calibration, not the tissue words, no RAM.
//!
//! Evidence class: the layout and the firmware behavior were established on this engine with Renode-hooked runs of the
//! unchanged TRITON images (a synthetic reproduction, not a physical observation); the engine's decompression arithmetic
//! equals Renode's bit for bit.

use crate::firmware::{AddressEntry, Release};
use crate::system::{Mode, System, Which};
use emu_core::{Json, Width};

/// Tissue records in main RAM, bytes between two records, and the offsets of the two floats in a record.
pub const TISSUE_RECORDS: u32 = 16;
pub const TISSUE_STRIDE: u32 = 36;
pub const TISSUE_N2_OFFSET: u32 = 24;
pub const TISSUE_HE_OFFSET: u32 = 28;
/// Words of the stored tissue block (an N2 and a He float per record) and its size in the EEPROM.
pub const TISSUE_WORDS: usize = 32;
pub const EEPROM_TISSUE_BYTES: usize = 128;
/// Size of the saved last-decompression date record.
pub const EEPROM_DATE_BYTES: usize = 4;
/// Size of the EEPROM image.
pub const EEPROM_BYTES: usize = 2048;
/// Breathing-mode byte value that takes the ppO2 from the oxygen cells.
pub const MEASURED_PPO2_MODE: u8 = 2;

// ---- health ---------------------------------------------------------------------------------------------------------

/// Whether the tissue pressures in RAM are finite numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TissueHealth {
    Valid,
    Invalid,
    Unknown,
}

impl TissueHealth {
    pub fn name(self) -> &'static str {
        match self {
            TissueHealth::Valid => "valid",
            TissueHealth::Invalid => "invalid",
            TissueHealth::Unknown => "unknown",
        }
    }
}

/// Whether the oxygen cells are calibrated, as far as the measured ppO2 shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OxygenHealth {
    Calibrated,
    Uncalibrated,
    Unknown,
}

impl OxygenHealth {
    pub fn name(self) -> &'static str {
        match self {
            OxygenHealth::Calibrated => "calibrated",
            OxygenHealth::Uncalibrated => "uncalibrated",
            OxygenHealth::Unknown => "unknown",
        }
    }
}

/// The `decoHealth` member of the state document.
#[derive(Clone, Debug, PartialEq)]
pub struct DecoHealth {
    pub tissues: TissueHealth,
    pub oxygen: OxygenHealth,
    /// Why, in words and numbers (see [`health`]).
    pub details: Json,
}

impl DecoHealth {
    /// `{"tissues": "valid"|"invalid"|"unknown", "oxygen": "calibrated"|"uncalibrated"|"unknown", "details": {...}}`.
    pub fn to_json(&self) -> Json {
        Json::object().with("tissues", self.tissues.name()).with("oxygen", self.oxygen.name()).with("details", self.details.clone())
    }
}

fn unknown_reason(entry: &AddressEntry) -> Option<&'static str> {
    entry.reason()
}

/// Reads the decompression state of the running main application with side-effect-free peeks.
///
/// * **tissues**: all 32 words (N2 and He of the 16 records) finite means *valid*; any NaN or infinity means *invalid*.
///   Words that are all exactly zero mean the firmware has not initialized the state yet (RAM is zero until then):
///   *unknown*.
/// * **oxygen**: only in the measured-ppO2 mode (breathing-mode byte 2). A non-finite ppO2 means *uncalibrated* (the
///   original firmware computes NaN from cells without a valid calibration); a finite non-zero ppO2 means *calibrated*.
///   A ppO2 of zero means the firmware has not evaluated the cells yet (a surface start of an uncalibrated profile reads
///   zero until a dive starts): then the cached cell flags decide, *uncalibrated* when every enabled cell has calibration
///   state 0, else *unknown*. Any other mode is *unknown*.
/// * A handset-only run, or a release whose addresses are not proven (NEPTUN), reports *unknown* with the reason.
pub fn health(system: &System) -> DecoHealth {
    let addresses = &system.release().addresses;
    let (tissues_text, tissues, tissue_detail) = tissue_health(system, &addresses.main_deco_tissues);
    let (oxygen_text, oxygen, oxygen_detail) = oxygen_health(system, &addresses.main_breathing_mode, &addresses.main_ppo2, &addresses.main_cell_flags);
    let mut details = Json::object().with("tissues", tissues_text).with("oxygen", oxygen_text);
    for (key, value) in tissue_detail.into_iter().chain(oxygen_detail) {
        details.insert(key, value);
    }
    DecoHealth { tissues, oxygen, details }
}

fn tissue_health(system: &System, entry: &AddressEntry) -> (String, TissueHealth, Vec<(&'static str, Json)>) {
    if system.mode() != Mode::Dual {
        return ("Unknown: a handset-only run has no main board.".to_string(), TissueHealth::Unknown, Vec::new());
    }
    let (Some(base), Some(board)) = (entry.address(), system.board(Which::Main)) else {
        let reason = unknown_reason(entry).unwrap_or("the main board is missing");
        return (format!("Unknown for {}: {reason}.", system.release().id), TissueHealth::Unknown, Vec::new());
    };
    let mut non_finite = 0u64;
    let mut zero = 0usize;
    for record in 0..TISSUE_RECORDS {
        for offset in [TISSUE_N2_OFFSET, TISSUE_HE_OFFSET] {
            let word = board.peek(base + TISSUE_STRIDE * record + offset, Width::Word).unwrap_or(0);
            if !f32::from_bits(word).is_finite() {
                non_finite += 1;
            }
            if word == 0 {
                zero += 1;
            }
        }
    }
    let detail = vec![("tissueWords", Json::from(TISSUE_WORDS)), ("nonFiniteTissueWords", Json::from(non_finite))];
    if non_finite > 0 {
        let text = format!("Invalid: {non_finite} of {TISSUE_WORDS} tissue words are not finite numbers (NaN); the no-decompression limit stays at 99.");
        (text, TissueHealth::Invalid, detail)
    } else if zero == TISSUE_WORDS {
        ("Unknown: the firmware has not initialized the tissue state yet.".to_string(), TissueHealth::Unknown, detail)
    } else {
        ("Valid: all tissue words are finite.".to_string(), TissueHealth::Valid, detail)
    }
}

fn oxygen_health(system: &System, mode_entry: &AddressEntry, ppo2_entry: &AddressEntry, flags_entry: &AddressEntry) -> (String, OxygenHealth, Vec<(&'static str, Json)>) {
    if system.mode() != Mode::Dual {
        return ("Unknown: a handset-only run has no main board.".to_string(), OxygenHealth::Unknown, Vec::new());
    }
    let (Some(mode_address), Some(ppo2_address), Some(board)) = (mode_entry.address(), ppo2_entry.address(), system.board(Which::Main)) else {
        let reason = unknown_reason(mode_entry).or_else(|| unknown_reason(ppo2_entry)).unwrap_or("the main board is missing");
        return (format!("Unknown for {}: {reason}.", system.release().id), OxygenHealth::Unknown, Vec::new());
    };
    let mode = board.peek(mode_address, Width::Byte).unwrap_or(0) as u8;
    let word = board.peek(ppo2_address, Width::Word).unwrap_or(0);
    let value = f32::from_bits(word);
    let mut detail = vec![("breathingMode", Json::from(mode))];
    if mode != MEASURED_PPO2_MODE {
        detail.push(("ppO2", if value.is_finite() { Json::from(value) } else { Json::Null }));
        let text = format!("Unknown: the breathing mode is {mode}, not the measured-ppO2 mode ({MEASURED_PPO2_MODE}), so the cell calibration is not observed.");
        return (text, OxygenHealth::Unknown, detail);
    }
    if !value.is_finite() {
        detail.push(("ppO2", Json::Null));
        detail.push(("ppO2Word", Json::from(format!("0x{word:08x}"))));
        return ("Uncalibrated: the measured ppO2 is NaN, so the no-decompression limit stays at 99.".to_string(), OxygenHealth::Uncalibrated, detail);
    }
    detail.push(("ppO2", Json::from(value)));
    if word != 0 {
        return ("Calibrated: the measured ppO2 is a finite number.".to_string(), OxygenHealth::Calibrated, detail);
    }
    // The ppO2 is zero: the firmware computes it once the oxygen cells are evaluated (on this engine: after a calibration
    // or once a dive starts), so a surface start reads zero whatever the calibration is. The cached cell flags tell whether
    // any enabled cell has a calibration at all.
    let flags: Option<Vec<u8>> = flags_entry.address().map(|address| (0..3).map(|cell| board.peek(address + cell, Width::Byte).unwrap_or(0) as u8).collect());
    let Some(flags) = flags else {
        return ("Unknown: the ppO2 is zero (not computed yet, or the cells read 0 mV).".to_string(), OxygenHealth::Unknown, detail);
    };
    detail.push(("cellFlags", Json::from_items(flags.iter().map(|&flag| u64::from(flag)))));
    let enabled: Vec<u8> = flags.iter().copied().filter(|flag| flag & 1 != 0).collect();
    if !enabled.is_empty() && enabled.iter().all(|flag| (flag >> 2) & 3 == 0) {
        let text = "Uncalibrated: no enabled oxygen cell has a calibration (stored cell flags); the measured ppO2 will be NaN once the cells are evaluated, and the no-decompression limit stays at 99.";
        return (text.to_string(), OxygenHealth::Uncalibrated, detail);
    }
    ("Unknown: the ppO2 is zero (not computed yet, or the cells read 0 mV).".to_string(), OxygenHealth::Unknown, detail)
}

// ---- the storage fixture -------------------------------------------------------------------------------------------------

/// What the pre-boot EEPROM consistency fixture did at the last board creation (`decoStorageFixture` of the state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageFixture {
    /// The switch: `SessionConfig::deco_storage_fixture`.
    pub enabled: bool,
    /// The date record was erased at the last board creation.
    pub applied: bool,
    /// Why it was or was not applied.
    pub reason: String,
    /// The erased date record (a packed RTC calendar), when it was applied.
    pub previous_date: Option<u32>,
}

impl StorageFixture {
    /// Before any board exists: nothing was applied yet.
    pub fn idle(enabled: bool) -> Self {
        Self { enabled, applied: false, reason: "No board has been created yet.".to_string(), previous_date: None }
    }

    fn skipped(enabled: bool, reason: impl Into<String>) -> Self {
        Self { enabled, applied: false, reason: reason.into(), previous_date: None }
    }

    /// `{"enabled", "applied", "reason", "previousDateRecord"}`; the last is the hex of the erased record or null.
    pub fn to_json(&self) -> Json {
        Json::object()
            .with("enabled", self.enabled)
            .with("applied", self.applied)
            .with("reason", self.reason.as_str())
            .with("previousDateRecord", self.previous_date.map(|date| format!("0x{date:08x}")))
    }
}

/// The pre-boot EEPROM consistency fixture (an emulator fixture; see the module documentation).
///
/// `image` is the stored EEPROM image that is about to be loaded into the board (`None`: a fresh profile). If the stored
/// tissue block (physical bytes `0x0ff..=0x17e`) is entirely `0xFF` while the last-decompression date record (`0x17f..=0x182`)
/// is not, the date record is set to `0xFF` in `image`, and the firmware then resets the tissues itself. Nothing else is
/// written. Skipped, with the reason, when switched off, in a handset-only run and for a release whose record layout is not
/// proven.
pub fn storage_fixture(enabled: bool, dual: bool, release: &Release, image: Option<&mut [u8]>) -> StorageFixture {
    if !enabled {
        return StorageFixture::skipped(false, "Switched off (decoStorageFixture: false, or --no-deco-storage-fixture).");
    }
    if !dual {
        return StorageFixture::skipped(true, "Not applicable: a handset-only run has no main board and no EEPROM.");
    }
    let addresses = &release.addresses;
    let (Some(block), Some(date)) = (addresses.eeprom_tissue_block.address(), addresses.eeprom_deco_date.address()) else {
        let reason = addresses.eeprom_tissue_block.reason().or_else(|| addresses.eeprom_deco_date.reason()).unwrap_or("no record layout");
        return StorageFixture::skipped(true, format!("Skipped for {}: the EEPROM record layout is not proven for this release ({reason}).", release.id));
    };
    let (block, date) = (block as usize, date as usize);
    let Some(image) = image else {
        return StorageFixture::skipped(true, "Not needed: there is no saved EEPROM yet (a fresh profile); the firmware resets the tissues itself.");
    };
    if image.len() != EEPROM_BYTES || block + EEPROM_TISSUE_BYTES > image.len() || date + EEPROM_DATE_BYTES > image.len() {
        return StorageFixture::skipped(true, format!("Skipped: the EEPROM image has {} bytes, not {EEPROM_BYTES}.", image.len()));
    }
    if image[block..block + EEPROM_TISSUE_BYTES].iter().any(|&byte| byte != 0xFF) {
        return StorageFixture::skipped(true, "Not needed: the stored tissue block holds saved data.");
    }
    let record = &mut image[date..date + EEPROM_DATE_BYTES];
    if record.iter().all(|&byte| byte == 0xFF) {
        return StorageFixture::skipped(true, "Not needed: the saved decompression date is erased, so the firmware resets the tissues itself.");
    }
    let previous = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
    record.fill(0xFF);
    StorageFixture {
        enabled: true,
        applied: true,
        reason: "The stored tissue block (EEPROM 0x0ff..0x17e) is entirely erased but the saved decompression date (0x17f) is set, so the firmware would load 32 erased words as NaN tissues; \
                 the date record was erased and the firmware takes its own four-day reset path (emulator fixture)."
            .to_string(),
        previous_date: Some(previous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::{NEPTUN, TRITON};

    fn image(tissues: u8, date: [u8; 4]) -> Vec<u8> {
        let mut image = vec![0xFF; EEPROM_BYTES];
        image[0x38..0x3E].copy_from_slice(&[0xE8, 0x03, 0xE8, 0x03, 0xE8, 0x03]);
        image[0x3E..0x41].copy_from_slice(&[0x09, 0x09, 0x09]);
        image[0xFF..0x17F].fill(tissues);
        image[0x17F..0x183].copy_from_slice(&date);
        image
    }

    #[test]
    fn an_erased_tissue_block_with_a_saved_date_loses_only_the_date_record() {
        let mut stored = image(0xFF, [0x54, 0x04, 0x10, 0x50]);
        let before = stored.clone();
        let report = storage_fixture(true, true, &TRITON, Some(&mut stored));
        assert!(report.applied && report.enabled, "{report:?}");
        assert_eq!(report.previous_date, Some(0x5010_0454));
        assert_eq!(&stored[0x17F..0x183], &[0xFF; 4]);
        // Every other byte is untouched, the oxygen calibration included.
        assert_eq!(&stored[..0x17F], &before[..0x17F]);
        assert_eq!(&stored[0x183..], &before[0x183..]);
        assert_eq!(&stored[0x38..0x41], &before[0x38..0x41]);
        assert_eq!(report.to_json().get("previousDateRecord").and_then(Json::as_str), Some("0x50100454"));
    }

    #[test]
    fn the_fixture_is_skipped_with_a_reason_everywhere_else() {
        let reason = |report: &StorageFixture| report.reason.clone();
        // Saved tissues: nothing to repair.
        let mut saved = image(0x3F, [1, 2, 3, 4]);
        let report = storage_fixture(true, true, &TRITON, Some(&mut saved));
        assert!(!report.applied && reason(&report).contains("holds saved data"), "{report:?}");
        assert_eq!(&saved[0x17F..0x183], &[1, 2, 3, 4]);
        // One non-erased tissue byte is enough to keep the date.
        let mut partly = image(0xFF, [1, 2, 3, 4]);
        partly[0x17E] = 0x00;
        assert!(!storage_fixture(true, true, &TRITON, Some(&mut partly)).applied);
        // Erased date: the firmware already takes the reset path.
        let mut erased = image(0xFF, [0xFF; 4]);
        let report = storage_fixture(true, true, &TRITON, Some(&mut erased));
        assert!(!report.applied && reason(&report).contains("date is erased"), "{report:?}");
        // Switched off, handset only, no image, NEPTUN, a wrong image size.
        let mut off = image(0xFF, [1, 2, 3, 4]);
        let report = storage_fixture(false, true, &TRITON, Some(&mut off));
        assert!(!report.enabled && !report.applied && reason(&report).starts_with("Switched off"), "{report:?}");
        assert_eq!(&off[0x17F..0x183], &[1, 2, 3, 4], "switched off: nothing is written");
        assert!(reason(&storage_fixture(true, false, &TRITON, None)).starts_with("Not applicable"));
        assert!(reason(&storage_fixture(true, true, &TRITON, None)).contains("no saved EEPROM"));
        let mut neptun = image(0xFF, [1, 2, 3, 4]);
        let report = storage_fixture(true, true, &NEPTUN, Some(&mut neptun));
        assert!(!report.applied && reason(&report).contains("NEPTUN-5.8-65.3") && reason(&report).contains("not proven"), "{report:?}");
        assert_eq!(&neptun[0x17F..0x183], &[1, 2, 3, 4], "NEPTUN: nothing is written");
        let mut short = vec![0xFF; 100];
        assert!(reason(&storage_fixture(true, true, &TRITON, Some(&mut short))).contains("100 bytes"));
    }
}
