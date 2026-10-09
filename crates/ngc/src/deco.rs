//! Decompression state handling: the read-only `decoHealth` report (DESIGN.md "Decompression state handling").
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
//! A new EEPROM no longer has the first problem: the factory image ([`crate::eeprom_init`]) fills the stored tissue block with
//! the surface values the firmware's own reset computes. An older profile whose stored tissues are blank still loads NaN; the engine
//! does not repair an existing EEPROM, it only reports it, and a profile reset creates an initialized one.
//!
//! [`health`] peeks (no side effects) at the RAM words above and reports whether the tissues are finite and whether the oxygen
//! cells are calibrated. It changes nothing. It is available for the TRITON main image only (the addresses are in
//! [`crate::firmware::ReleaseAddresses`]; another release reports *unknown* with the reason).
//!
//! Evidence class: the layout and the firmware behavior were established on this engine with Renode-hooked runs of the
//! unchanged TRITON images (a synthetic reproduction, not a physical observation); the engine's decompression arithmetic
//! equals Renode's bit for bit.

use crate::firmware::AddressEntry;
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
