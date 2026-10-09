//! The EEPROM factory image (DESIGN.md section 18, `docs/eeprom.md`).
//!
//! A fresh emulator profile starts with a fully erased main EEPROM (2048 bytes of `0xFF`). The original firmware's first-boot
//! default routine (`0x08009fea`, gated on the validity marker `0xa3` at offset 254) fills most records, but not all: a handful
//! were added by later firmware versions (or are skipped by a setter's "write only if different from RAM" shortcut) and
//! stay erased. Read back, those erased records are NaN or out of range: the oxygen-toxicity dose base makes the handset print
//! `?a?%` for the delta vital capacity, the toxicity-model setting is `0xFF` so the main never sends CAN `0x225`, the
//! no-fly time is 4 294 967 295 s, and the 32 tissue words (written only on the power-down route) load as NaN on the next start.
//!
//! **An emulator fixture, applied once and without an option.** When a *new* EEPROM is created, the engine writes the factory image
//! below into it before the board loads it, and the image is saved with the profile. A new EEPROM is a profile without
//! `eeprom.bin` or a stored image that is entirely erased (all 2048 bytes `0xFF`). Any other stored image is an existing ("dirty")
//! EEPROM and is **never touched**, even when some inventoried records in it are still erased. There is no user option; the
//! internal [`SessionConfig::eeprom_factory_init`](crate::session::SessionConfig::eeprom_factory_init) exists only so that the
//! recorded scenarios and the dive benchmark ([`crate::scenario`]), which compare an erased first boot byte for byte with the Renode
//! recordings, can keep a blank EEPROM.
//!
//! The factory image is 2048 bytes of `0xFF` with the *inventoried* records below written into it. Nothing else is written: not the
//! oxygen or ADC calibration, not a setting the default routine initializes (so the routine runs exactly as before: its only gate is
//! the marker at offset 254, which is not in the list), no RAM and no ppO2.
//!
//! The values are **firmware-derived, not the manufacturer's factory image** (that image is unknown): each is what the
//! firmware's own code writes or implies (a migration or clear path, a save routine run from its zero state, a getter's
//! floor, or the RAM the firmware's own reset computes at a first boot). `docs/eeprom.md` lists every record the first boot
//! leaves erased, with the evidence, and the ones this image deliberately leaves erased. The serial number is the exception:
//! EEPROM offset 0 is synthetic emulator storage and the value 1 is the page's default, not evidence of anything.
//!
//! The record layout (logical ID to physical offset and size) is the table of the main image; both releases carry the same
//! 568-byte table (SHA-256 [`RECORD_TABLE_SHA256`], at `0x080306f2` in TRITON and `0x08050e38` in NEPTUN). The engine checks
//! that hash against the loaded image before it writes anything.
//!
//! Existing profiles are not repaired: a profile saved before the tissue block was written keeps blank tissues, which the read-only
//! `decoHealth` report ([`crate::deco`]) shows as invalid; resetting the profile creates an initialized EEPROM.

use crate::deco::EEPROM_BYTES;
use crate::firmware::{Firmware, Release};
use emu_core::Json;

/// Bytes of the record table of the main image: 0x8e entries (IDs 0 to 0x8d) of 4 bytes.
pub const RECORD_TABLE_BYTES: usize = 0x8E * 4;
/// SHA-256 of that table in both releases (the layout facts are metadata, not code; the table itself is not stored here).
pub const RECORD_TABLE_SHA256: &str = "a69c0b84bdffa90dee14b26daa4eb35c7578b0923247a4ffe9b2c98fef244672";
/// The surface-equilibrium N2 pressure of a tissue (float32 `0x3f40304d`, 0.750737 bar): 0.79 x (1.013 - 0.0627) bar, the value the
/// firmware's own reset routine (`0x08007584`) leaves in RAM at a first boot (its latched surface pressure is 1.013 bar for the
/// emulated 1013.25 mbar sensor reading). All 16 tissues get it; He is 0.
pub const SURFACE_TISSUE_N2: u32 = 0x3F40_304D;

enum Value {
    Bytes(&'static [u8]),
    /// 16 records of (N2, He) float32 words: the stored tissue block.
    Tissues,
}

struct Record {
    /// Logical record ID(s) of the main image's table (the key of `docs/eeprom.md`; the tests name records by it).
    #[cfg_attr(not(test), allow(dead_code))]
    id: &'static str,
    /// Physical EEPROM offset.
    offset: usize,
    value: Value,
}

impl Record {
    fn len(&self) -> usize {
        match &self.value {
            Value::Bytes(bytes) => bytes.len(),
            Value::Tissues => 128,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match &self.value {
            Value::Bytes(bytes) => bytes.to_vec(),
            Value::Tissues => (0..16).flat_map(|_| SURFACE_TISSUE_N2.to_le_bytes().into_iter().chain(0u32.to_le_bytes())).collect(),
        }
    }
}

/// The inventory of records the factory image fills (the records the first boot leaves erased and that the firmware's own code gives a
/// value). Each entry names the value and the evidence; `docs/eeprom.md` explains them all, and the records left erased.
const RECORDS: [Record; 8] = [
    // Serial number, 1: synthetic (EEPROM offset 0 is emulator storage and never a physical unit's number; 1 is the page's default and
    // inside the nine-digit range). Erased it reads 4294967295 (SN -00000001 on the System info page). Loader 0x0800a2c8, getter 0x0800a324.
    Record { id: "0x01", offset: 0x000, value: Value::Bytes(&[1, 0, 0, 0]) },
    // Oxygen-toxicity model, 0 (OTU/UPTD): what the CAN setter (0x0800afa0) maps everything but 1 to, what the default routine writes to
    // its five sibling settings (0x0800a134..0x0800a154) and the handset's fallback. Erased (0xff) the main sends no CAN 0x225.
    Record { id: "0x2b", offset: 0x0AF, value: Value::Bytes(&[0]) },
    // Dose base (x squared), 0.0: loaded raw by 0x0800a500, written only by 0x0800a618 (save routine 0x0801ab7c) from a zero state.
    // Erased it is NaN and the handset shows the delta vital capacity as ?a?% (CAN 0x226 is NaN).
    Record { id: "0x67", offset: 0x0B8, value: Value::Bytes(&[0, 0, 0, 0]) },
    // ESOT minutes (u16), 0: the save routine's value from a zero state (0x0800a660). Erased it is 65535 minutes and becomes the live
    // ESOT value on entering dive mode.
    Record { id: "0x68", offset: 0x0BC, value: Value::Bytes(&[0, 0]) },
    // Last ppO2 times 100, 110: the save routine 0x0801ab7c stores max(average ppO2, 1.1) x 100 and the recovery routine 0x0801a948 applies
    // the same 1.1 floor. Erased it is 255 (2.55 bar), the fastest recovery rate.
    Record { id: "0x69", offset: 0x0BE, value: Value::Bytes(&[110]) },
    // Stored tissue block, 16 x (N2 0.750737 = 0x3f40304d, He 0): the surface-equilibrium values the firmware's own reset (0x08007584, run
    // by the initializer 0x08008308 at a first boot) leaves in RAM. The firmware saves the block only on power-down (0x080081b4), so after
    // a first boot it stays erased and the next start loads 32 NaN words.
    Record { id: "0x6a..0x89", offset: 0x0FF, value: Value::Tissues },
    // No-fly time (s), 0: the firmware's own clear path stores 0 (0x08008bc8, and the expiry branch of 0x08008a38). Erased it is
    // 4294967295 s minus the elapsed time, shown as NoFlyTime 80515 on the surface information page.
    Record { id: "0x8b", offset: 0x183, value: Value::Bytes(&[0, 0, 0, 0]) },
    // Date record of the no-fly time, 0: the firmware's migration from marker 0xa2 to 0xa3 writes 0 (0x08009fc6..0x08009fd2) and its getters
    // (0x08008c10, 0x08008c4c) read 0 as no date; the fresh-profile default routine 0x08009fea omits the record.
    Record { id: "0x8d", offset: 0x190, value: Value::Bytes(&[0, 0, 0, 0]) },
];

/// The bytes of the main image's record table for `release` (None when the release has no proven table or the span is too short).
pub fn record_table<'a>(release: &Release, main: &'a Firmware) -> Option<&'a [u8]> {
    let address = release.addresses.eeprom_record_table.address()?;
    let offset = address.checked_sub(main.span_base)? as usize;
    main.span.get(offset..offset.checked_add(RECORD_TABLE_BYTES)?)
}

/// The factory image: 2048 bytes of `0xFF` with the inventoried records written into it (nothing else). The firmware's own first-boot
/// defaults fill everything else when it runs.
pub fn factory_image() -> Vec<u8> {
    let mut image = vec![0xFF; EEPROM_BYTES];
    for record in &RECORDS {
        image[record.offset..record.offset + record.len()].copy_from_slice(&record.bytes());
    }
    image
}

/// Whether the stored EEPROM counts as new: there is none, or it is entirely erased (every byte `0xFF`).
pub fn is_new(image: Option<&[u8]>) -> bool {
    image.map_or(true, |bytes| bytes.iter().all(|&byte| byte == 0xFF))
}

/// What the factory image did for this session (`eepromFactoryInit` of the state): whether the session created its EEPROM from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactoryInit {
    /// This session created the EEPROM from the factory image.
    pub applied: bool,
    /// Why it was or was not applied.
    pub reason: String,
}

impl FactoryInit {
    fn skipped(reason: impl Into<String>) -> Self {
        Self { applied: false, reason: reason.into() }
    }

    /// `{"applied", "reason"}`.
    pub fn to_json(&self) -> Json {
        Json::object().with("applied", self.applied).with("reason", self.reason.as_str())
    }
}

/// The fixture. `image` is the stored EEPROM image that is about to be loaded into the board (`None`: a brand-new profile); `table` is
/// the main image's record table (see [`record_table`]). With the internal switch on, in a dual run, for a release whose record table is
/// proven and hashes to [`RECORD_TABLE_SHA256`], a new EEPROM (none stored, or every stored byte `0xFF`) becomes the factory image. An
/// existing EEPROM, whatever it holds, and every skipped case leave `image` exactly as it was.
pub fn apply(enabled: bool, dual: bool, release: &Release, table: Option<&[u8]>, image: &mut Option<Vec<u8>>) -> FactoryInit {
    if !enabled {
        return FactoryInit::skipped("Not applied: switched off by the internal configuration of a recorded scenario.");
    }
    if !dual {
        return FactoryInit::skipped("Not applicable: a handset-only run has no main board and no EEPROM.");
    }
    if let Some(existing) = image.as_ref() {
        if existing.len() != EEPROM_BYTES {
            return FactoryInit::skipped(format!("Skipped: the EEPROM image has {} bytes, not {EEPROM_BYTES}.", existing.len()));
        }
        if !is_new(Some(existing)) {
            return FactoryInit::skipped("Not applied: the profile already holds an EEPROM, which the factory image never touches.");
        }
    }
    let entry = &release.addresses.eeprom_record_table;
    let Some(address) = entry.address() else {
        let reason = entry.reason().unwrap_or("no record table");
        return FactoryInit::skipped(format!("Skipped for {}: the EEPROM record layout is not proven for this release ({reason}).", release.id));
    };
    match table {
        Some(bytes) if crate::sha256::digest_hex(bytes) == RECORD_TABLE_SHA256 => {}
        Some(_) => {
            return FactoryInit::skipped(format!("Skipped for {}: the record table at 0x{address:08x} of the loaded image is not the verified table (SHA-256 mismatch).", release.id));
        }
        None => return FactoryInit::skipped(format!("Skipped for {}: the record table at 0x{address:08x} is not inside the loaded image.", release.id)),
    }
    let fresh = image.is_none();
    *image = Some(factory_image());
    let reason = if fresh {
        "This session created the EEPROM from the factory image (an emulator fixture with firmware-derived values for the records the firmware's first-boot defaults never write); the firmware's own first-boot defaults (0x08009fea) still run for everything else."
    } else {
        "The stored EEPROM was entirely erased (2048 bytes of 0xFF), so it counted as new and became the factory image (an emulator fixture with firmware-derived values); the firmware's own first-boot defaults (0x08009fea) still run for everything else."
    };
    FactoryInit { applied: true, reason: reason.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::{AddressEntry, ReleaseAddresses, HANDSET, MAIN, NEPTUN, TRITON};

    /// A table that is not the verified one: enough for the refusals; the real hash is checked against the images in
    /// `crates/ngc/tests/eeprom_init.rs`.
    fn fake_table() -> Vec<u8> {
        vec![0; RECORD_TABLE_BYTES]
    }

    #[test]
    fn the_inventory_stays_inside_the_image_without_overlaps_and_off_the_marker() {
        let mut taken = vec![false; EEPROM_BYTES];
        for record in &RECORDS {
            for offset in record.offset..record.offset + record.len() {
                assert!(!taken[offset], "{} overlaps another record at 0x{offset:03x}", record.id);
                taken[offset] = true;
            }
            assert!(record.offset + record.len() <= EEPROM_BYTES);
            // The default routine's only gate is the validity marker (ID 0x63, offset 254): never part of the inventory.
            assert!(!(record.offset..record.offset + record.len()).contains(&254), "{} covers the marker", record.id);
        }
        assert_eq!(RECORDS.iter().map(Record::len).sum::<usize>(), 4 + 1 + 4 + 2 + 1 + 128 + 4 + 4);
    }

    #[test]
    fn the_factory_image_is_the_inventory_on_erased_bytes() {
        let image = factory_image();
        assert_eq!(image.len(), EEPROM_BYTES);
        // Serial 1, model 0, base 0.0, ESOT 0, last ppO2 110, no-fly 0 and its date 0.
        assert_eq!(&image[0..4], &[1, 0, 0, 0]);
        assert_eq!(image[0x0AF], 0);
        assert_eq!(&image[0x0B8..0x0BF], &[0, 0, 0, 0, 0, 0, 110]);
        assert_eq!(&image[0x183..0x187], &[0; 4]);
        assert_eq!(&image[0x190..0x194], &[0; 4]);
        // The tissue block: (N2 = the surface equilibrium of the original reset, He = 0) per tissue.
        let tissues = RECORDS.iter().find(|record| matches!(record.value, Value::Tissues)).expect("tissues");
        assert_eq!((tissues.id, tissues.offset, tissues.len()), ("0x6a..0x89", 0x0FF, 128));
        for pair in image[0x0FF..0x17F].chunks(8) {
            let n2 = f32::from_le_bytes(pair[0..4].try_into().unwrap());
            assert_eq!(n2.to_bits(), SURFACE_TISSUE_N2);
            assert!((f64::from(n2) - 0.79 * (1.013 - 0.0627)).abs() < 1e-6, "{n2}");
            assert_eq!(&pair[4..8], &[0, 0, 0, 0], "no helium");
        }
        // Everything outside the inventory is still erased: the firmware's own first-boot defaults have all of it to do.
        for (offset, &byte) in image.iter().enumerate() {
            if !RECORDS.iter().any(|record| (record.offset..record.offset + record.len()).contains(&offset)) {
                assert_eq!(byte, 0xFF, "0x{offset:03x}");
            }
        }
        assert!(!is_new(Some(&image)), "the factory image is not itself entirely erased");
    }

    #[test]
    fn only_a_missing_or_entirely_erased_image_is_new() {
        assert!(is_new(None));
        assert!(is_new(Some(&vec![0xFF; EEPROM_BYTES])));
        for offset in [0, 254, EEPROM_BYTES - 1] {
            let mut dirty = vec![0xFF; EEPROM_BYTES];
            dirty[offset] = 0xFE;
            assert!(!is_new(Some(&dirty)), "one stored byte at {offset} makes the EEPROM existing");
        }
        assert!(!is_new(Some(&vec![0x00; EEPROM_BYTES])));
    }

    #[test]
    fn the_gates_refuse_with_a_reason_and_leave_the_image_as_it_was() {
        let table = fake_table();
        // Switched off (the recorded scenarios), a handset-only run.
        let mut image = None;
        let off = apply(false, true, &TRITON, Some(&table), &mut image);
        assert!(!off.applied && off.reason.starts_with("Not applied: switched off") && image.is_none(), "{off:?}");
        let handset = apply(true, false, &TRITON, Some(&table), &mut image);
        assert!(!handset.applied && handset.reason.starts_with("Not applicable") && image.is_none(), "{handset:?}");
        // A table that is not the verified one, and none at all.
        let wrong = apply(true, true, &TRITON, Some(&table), &mut image);
        assert!(!wrong.applied && wrong.reason.contains("TRITON-5.8-65.3") && wrong.reason.contains("not the verified table") && image.is_none(), "{wrong:?}");
        let missing = apply(true, true, &NEPTUN, None, &mut image);
        assert!(!missing.applied && missing.reason.contains("not inside the loaded image") && image.is_none(), "{missing:?}");
        // A release without a proven table.
        let unproven = Release {
            id: "TEST-0.0",
            label: "test",
            main: &MAIN,
            handset: &HANDSET,
            addresses: ReleaseAddresses { eeprom_record_table: AddressEntry::unavailable("not proven for the test"), ..TRITON.addresses },
            cold_boot_refusal: None,
        };
        let skipped = apply(true, true, &unproven, Some(&table), &mut image);
        assert!(!skipped.applied && skipped.reason.contains("TEST-0.0") && skipped.reason.contains("not proven for the test") && image.is_none(), "{skipped:?}");
        // A wrong image size is left to the loader's own refusal, untouched.
        let mut short = Some(vec![0xFF; 100]);
        let size = apply(true, true, &TRITON, Some(&table), &mut short);
        assert!(!size.applied && size.reason.contains("100 bytes") && short.as_deref() == Some(&[0xFF; 100][..]), "{size:?}");
        let json = wrong.to_json();
        assert_eq!(json.get("applied"), Some(&Json::Bool(false)));
        assert_eq!(json.len(), 2, "the report is just {{applied, reason}}");
    }

    #[test]
    fn an_existing_eeprom_is_never_touched_and_a_new_or_erased_one_becomes_the_factory_image() {
        // The record table check needs the real image; the real hash is accepted by the integration tests, so here the table is the
        // only thing the unit test cannot supply: an existing EEPROM is refused before the table is looked at.
        let table = fake_table();
        let mut dirty = vec![0xFF; EEPROM_BYTES];
        dirty[2047] = 0x00; // one stored byte: every inventoried record is still erased, and it stays that way
        let before = dirty.clone();
        let mut image = Some(dirty);
        let report = apply(true, true, &TRITON, Some(&table), &mut image);
        assert!(!report.applied && report.reason.starts_with("Not applied: the profile already holds an EEPROM"), "{report:?}");
        assert_eq!(image.as_deref(), Some(&before[..]));
        // The same for a profile in which some inventoried records are erased and others hold data.
        let mut partial = factory_image();
        partial[0..4].fill(0xFF);
        let before = partial.clone();
        let mut image = Some(partial);
        assert!(!apply(true, true, &TRITON, Some(&table), &mut image).applied);
        assert_eq!(image.as_deref(), Some(&before[..]), "an erased serial in an existing EEPROM stays erased");
    }
}
