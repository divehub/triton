//! The EEPROM factory-init fixture (DESIGN.md section 18, `docs/eeprom.md`).
//!
//! A fresh emulator profile starts with a fully erased main EEPROM (2048 bytes of `0xFF`). The original firmware's first-boot
//! default routine (`0x08009fea`, gated on the validity marker `0xa3` at offset 254) fills most records, but not all: a handful
//! were added by later firmware versions (or are skipped by a setter's "write only if different from RAM" shortcut) and
//! stay erased. Read back, those erased records are NaN or out of range: the oxygen-toxicity dose base makes the handset print
//! `?a?%` for the delta vital capacity, the toxicity-model setting is `0xFF` so the main never sends CAN `0x225`, the
//! no-fly time is 4 294 967 295 s, and the 32 tissue words (written only on the power-down route) load as NaN on the next start.
//!
//! **An emulator fixture, on by default and switchable** (`SessionConfig::eeprom_factory_init`, `--no-eeprom-factory-init`,
//! `eepromFactoryInit`): before every board creation, each *inventoried* record that is still entirely `0xFF` receives the value
//! below; a brand-new profile starts from an all-`0xFF` image and gets the whole list. Nothing else is touched: not a byte that
//! already holds data, not the oxygen or ADC calibration, not a setting the default routine initializes (so the routine runs
//! exactly as before: its only gate is the marker at offset 254, which is not in the list), no RAM and no ppO2.
//!
//! The values are **firmware-derived, not the manufacturer's factory image** (that image is unknown): each is what the
//! firmware's own code writes or implies (a migration or clear path, a save routine run from its zero state, a getter's
//! floor, or the RAM the firmware's own reset computes at a first boot). `docs/eeprom.md` lists every record the first boot
//! leaves erased, with the evidence, and the ones this fixture deliberately leaves erased. The serial number is the exception:
//! EEPROM offset 0 is synthetic emulator storage and the value 1 is the page's default, not evidence of anything.
//!
//! The record layout (logical ID to physical offset and size) is the table of the main image; both releases carry the same
//! 568-byte table (SHA-256 [`RECORD_TABLE_SHA256`], at `0x080306f2` in TRITON and `0x08050e38` in NEPTUN). The fixture checks
//! that hash against the loaded image before it writes anything.
//!
//! Order with the other fixtures: this one runs first, then [`crate::deco::storage_fixture`] on the resulting image. With the
//! tissue block filled the date-erase repair no longer applies (its condition is an entirely erased block); it stays for
//! profiles of this fixture's switched-off runs.

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
    /// Logical record ID(s) of the main image's table.
    id: &'static str,
    name: &'static str,
    /// Physical EEPROM offset.
    offset: usize,
    value: Value,
    /// The value in words.
    shown: &'static str,
    /// Why this value, with the original addresses (see `docs/eeprom.md`).
    reason: &'static str,
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

    fn range(&self) -> String {
        format!("0x{:03x}..0x{:03x}", self.offset, self.offset + self.len() - 1)
    }
}

/// The inventory of records this fixture fills (the records the first boot leaves erased and that the firmware's own code gives a
/// value; `docs/eeprom.md` explains each, and the ones left erased).
const RECORDS: [Record; 8] = [
    Record {
        id: "0x01",
        name: "serial number",
        offset: 0x000,
        value: Value::Bytes(&[1, 0, 0, 0]),
        shown: "1",
        reason: "Synthetic: EEPROM offset 0 is emulator storage and never a physical unit's number; 1 is the page's default and inside the nine-digit range. Erased it reads 4294967295 (shown as SN -00000001 on the handset's System info page). Loader 0x0800a2c8, getter 0x0800a324.",
    },
    Record {
        id: "0x2b",
        name: "oxygen-toxicity model",
        offset: 0x0AF,
        value: Value::Bytes(&[0]),
        shown: "0 (OTU/UPTD)",
        reason: "Erased (0xff) the main sends no CAN 0x225 (producer 0x08013c1a, 0x080159dc) and the handset keeps placeholder CNS/OTU values. 0 is what the setting's CAN setter (0x0800afa0) maps everything but 1 to, what the default routine writes to its five sibling settings (IDs 0x26..0x2a at 0x0800a134..0x0800a154) and the handset's fallback (OTU/UPTD).",
    },
    Record {
        id: "0x67",
        name: "oxygen-toxicity dose base (x squared)",
        offset: 0x0B8,
        value: Value::Bytes(&[0, 0, 0, 0]),
        shown: "0.0",
        reason: "Loaded raw by 0x0800a500; the only writer is 0x0800a618, called by the save routine 0x0801ab7c with x squared. Erased it is NaN, so the handset shows the delta vital capacity as ?a?% (CAN 0x226 is NaN). 0.0 is what the save routine writes from its zero-initialized RAM state.",
    },
    Record {
        id: "0x68",
        name: "oxygen-toxicity ESOT minutes (u16)",
        offset: 0x0BC,
        value: Value::Bytes(&[0, 0]),
        shown: "0",
        reason: "Loaded raw by 0x0800a500, written by 0x0800a660 from the live ESOT value (0x0801a79c). Erased it is 65535 minutes, which the recovery routine 0x0801a948 decays only slowly and which becomes the live ESOT value on entering dive mode. 0 is the save routine's value from a zero state.",
    },
    Record {
        id: "0x69",
        name: "last ppO2 times 100 (u8)",
        offset: 0x0BE,
        value: Value::Bytes(&[110]),
        shown: "110 (1.10 bar)",
        reason: "Loaded by 0x0800a500 (divided by 100), written by 0x0800a688. The save routine 0x0801ab7c stores max(average ppO2, 1.1) times 100 and the recovery routine 0x0801a948 applies the same 1.1 floor, so a fresh state is 110. Erased it is 255 (2.55 bar), the fastest recovery rate.",
    },
    Record {
        id: "0x6a..0x89",
        name: "stored tissue block (32 words)",
        offset: 0x0FF,
        value: Value::Tissues,
        shown: "16 x (N2 0.750737 = 0x3f40304d, He 0.0)",
        reason: "The surface-equilibrium values the firmware's own reset (0x08007584, run by the initializer 0x08008308 at a first boot) leaves in RAM: 0.79 x (1.013 - 0.0627) bar N2 in every tissue, no He (captured from RAM on TRITON; NEPTUN's own reset and power-down save write the same words). The firmware saves the block only on power-down (0x080081b4), so after a first boot it stays erased and the next start loads 32 NaN words.",
    },
    Record {
        id: "0x8b",
        name: "no-fly time (s)",
        offset: 0x183,
        value: Value::Bytes(&[0, 0, 0, 0]),
        shown: "0",
        reason: "Loaded by 0x08008a38, saved by 0x08008b18 case 3 at power-down. The firmware's own clear path stores 0 (0x08008bc8, and the expiry branch of 0x08008a38). Erased it is 4294967295 s minus the elapsed time, shown as NoFlyTime 80515 on the surface information page.",
    },
    Record {
        id: "0x8d",
        name: "date record of the no-fly time",
        offset: 0x190,
        value: Value::Bytes(&[0, 0, 0, 0]),
        shown: "0",
        reason: "Read by 0x08008a38, 0x08008b18 and the getters 0x08008c10 and 0x08008c4c, which read 0 as no date; written by 0x08008b18 case 2. The firmware's own migration from marker 0xa2 to 0xa3 writes 0 here (0x08009fc6..0x08009fd2); the fresh-profile default routine 0x08009fea omits the record. Erased it is an invalid calendar (no difference was seen in the runs here).",
    },
];

/// The bytes of the main image's record table for `release` (None when the release has no proven table or the span is too short).
pub fn record_table<'a>(release: &Release, main: &'a Firmware) -> Option<&'a [u8]> {
    let address = release.addresses.eeprom_record_table.address()?;
    let offset = address.checked_sub(main.span_base)? as usize;
    main.span.get(offset..offset.checked_add(RECORD_TABLE_BYTES)?)
}

/// One record the fixture filled at the last board creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilledRecord {
    /// Logical record ID (`0x67`) or ID range of a block (`0x6a..0x89`).
    pub id: &'static str,
    pub name: &'static str,
    /// Physical range, `0x0b8..0x0bb`.
    pub range: String,
    /// The value, in words.
    pub value: &'static str,
    /// Why this value (firmware evidence).
    pub reason: &'static str,
}

/// What the fixture did at the last board creation (`eepromFactoryInit` of the state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactoryInit {
    /// The switch: `SessionConfig::eeprom_factory_init`.
    pub enabled: bool,
    /// At least one record was filled at the last board creation.
    pub applied: bool,
    /// Why it was or was not applied.
    pub reason: String,
    /// The records filled at the last board creation.
    pub records: Vec<FilledRecord>,
}

impl FactoryInit {
    /// Before any board exists: nothing was applied yet.
    pub fn idle(enabled: bool) -> Self {
        Self { enabled, applied: false, reason: "No board has been created yet.".to_string(), records: Vec::new() }
    }

    fn skipped(enabled: bool, reason: impl Into<String>) -> Self {
        Self { enabled, applied: false, reason: reason.into(), records: Vec::new() }
    }

    /// The stored tissue block was filled at the last board creation (the date-erase repair of [`crate::deco`] then has nothing to do).
    pub fn filled_tissues(&self) -> bool {
        self.records.iter().any(|record| record.id.starts_with("0x6a"))
    }

    /// `{"enabled", "applied", "reason", "records": [{"id", "name", "range", "value", "reason"}], "note"}`.
    pub fn to_json(&self) -> Json {
        let records = self.records.iter().map(|record| {
            Json::object().with("id", record.id).with("name", record.name).with("range", record.range.as_str()).with("value", record.value).with("reason", record.reason)
        });
        Json::object()
            .with("enabled", self.enabled)
            .with("applied", self.applied)
            .with("reason", self.reason.as_str())
            .with("records", Json::from_items(records))
            .with("note", NOTE)
    }
}

const NOTE: &str = "Emulator fixture with firmware-derived values, not the manufacturer's factory image: before every board creation each inventoried EEPROM record that is still entirely erased receives the value its firmware code implies (docs/eeprom.md); nothing else is written.";

/// The fixture. `image` is the stored EEPROM image that is about to be loaded into the board (`None`: a brand-new profile, which
/// then starts as 2048 bytes of `0xFF`); `table` is the main image's record table (see [`record_table`]). With the fixture on, in a
/// dual run, for a release whose record table is proven and hashes to [`RECORD_TABLE_SHA256`], every inventoried record whose
/// bytes are all `0xFF` gets its value. A skipped fixture leaves `image` exactly as it was.
pub fn apply(enabled: bool, dual: bool, release: &Release, table: Option<&[u8]>, image: &mut Option<Vec<u8>>) -> FactoryInit {
    if !enabled {
        return FactoryInit::skipped(false, "Switched off (eepromFactoryInit: false, or --no-eeprom-factory-init).");
    }
    if !dual {
        return FactoryInit::skipped(true, "Not applicable: a handset-only run has no main board and no EEPROM.");
    }
    if let Some(existing) = image.as_ref() {
        if existing.len() != EEPROM_BYTES {
            return FactoryInit::skipped(true, format!("Skipped: the EEPROM image has {} bytes, not {EEPROM_BYTES}.", existing.len()));
        }
    }
    let entry = &release.addresses.eeprom_record_table;
    let Some(address) = entry.address() else {
        let reason = entry.reason().unwrap_or("no record table");
        return FactoryInit::skipped(true, format!("Skipped for {}: the EEPROM record layout is not proven for this release ({reason}).", release.id));
    };
    match table {
        Some(bytes) if crate::sha256::digest_hex(bytes) == RECORD_TABLE_SHA256 => {}
        Some(_) => {
            return FactoryInit::skipped(true, format!("Skipped for {}: the record table at 0x{address:08x} of the loaded image is not the verified table (SHA-256 mismatch).", release.id));
        }
        None => return FactoryInit::skipped(true, format!("Skipped for {}: the record table at 0x{address:08x} is not inside the loaded image.", release.id)),
    }
    let fresh = image.is_none();
    let records = fill(image.get_or_insert_with(|| vec![0xFF; EEPROM_BYTES]));
    if records.is_empty() {
        return FactoryInit::skipped(true, "Not needed: every inventoried record already holds a value.");
    }
    let reason = if fresh {
        format!("A new profile: the EEPROM started erased and {} inventoried records were filled; the firmware's own first-boot defaults (0x08009fea) still run for everything else (emulator fixture).", records.len())
    } else {
        format!("{} inventoried records were still entirely erased and were filled; no other byte was touched (emulator fixture).", records.len())
    };
    FactoryInit { enabled: true, applied: true, reason, records }
}

/// Writes the value of every inventoried record whose bytes are all `0xFF` and names what it wrote. Pure: no gate, no other byte.
fn fill(image: &mut [u8]) -> Vec<FilledRecord> {
    let mut filled = Vec::new();
    for record in &RECORDS {
        let range = record.offset..record.offset + record.len();
        if image[range.clone()].iter().all(|&byte| byte == 0xFF) {
            image[range].copy_from_slice(&record.bytes());
            filled.push(FilledRecord { id: record.id, name: record.name, range: record.range(), value: record.shown, reason: record.reason });
        }
    }
    filled
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
                assert!(!taken[offset], "{} overlaps another record at 0x{offset:03x}", record.name);
                taken[offset] = true;
            }
            assert!(record.offset + record.len() <= EEPROM_BYTES);
            // The default routine's only gate is the validity marker (ID 0x63, offset 254): never part of the inventory.
            assert!(!(record.offset..record.offset + record.len()).contains(&254), "{} covers the marker", record.name);
            assert!(!record.reason.is_empty() && !record.shown.is_empty() && !record.id.is_empty());
        }
        assert_eq!(RECORDS.iter().map(Record::len).sum::<usize>(), 4 + 1 + 4 + 2 + 1 + 128 + 4 + 4);
    }

    #[test]
    fn the_tissue_value_is_the_surface_equilibrium_of_the_original_reset() {
        let tissues = RECORDS.iter().find(|record| matches!(record.value, Value::Tissues)).expect("tissues");
        let bytes = tissues.bytes();
        assert_eq!(bytes.len(), 128);
        for pair in bytes.chunks(8) {
            let n2 = f32::from_le_bytes(pair[0..4].try_into().unwrap());
            assert_eq!(n2.to_bits(), 0x3F40_304D);
            assert!((f64::from(n2) - 0.79 * (1.013 - 0.0627)).abs() < 1e-6, "{n2}");
            assert_eq!(&pair[4..8], &[0, 0, 0, 0], "no helium");
        }
        assert_eq!(tissues.range(), "0x0ff..0x17e");
    }

    #[test]
    fn an_erased_image_gets_exactly_the_inventory() {
        let mut image = vec![0xFF; EEPROM_BYTES];
        let filled = fill(&mut image);
        assert_eq!(filled.iter().map(|record| record.id).collect::<Vec<_>>(), ["0x01", "0x2b", "0x67", "0x68", "0x69", "0x6a..0x89", "0x8b", "0x8d"]);
        // Serial 1, model 0, base 0.0, ESOT 0, last ppO2 110, the tissue block, no-fly 0 and its date 0.
        assert_eq!(&image[0..4], &[1, 0, 0, 0]);
        assert_eq!(image[0x0AF], 0);
        assert_eq!(&image[0x0B8..0x0BF], &[0, 0, 0, 0, 0, 0, 110]);
        assert_eq!(&image[0x0FF..0x107], &[0x4D, 0x30, 0x40, 0x3F, 0, 0, 0, 0]);
        assert_eq!(&image[0x183..0x187], &[0; 4]);
        assert_eq!(&image[0x190..0x194], &[0; 4]);
        // Everything outside the inventory is still erased: the firmware's own first-boot defaults have all of it to do.
        for (offset, &byte) in image.iter().enumerate() {
            if !RECORDS.iter().any(|record| (record.offset..record.offset + record.len()).contains(&offset)) {
                assert_eq!(byte, 0xFF, "0x{offset:03x}");
            }
        }
        // A second pass finds nothing erased to fill.
        assert!(fill(&mut image).is_empty());
    }

    #[test]
    fn a_record_with_any_stored_byte_is_left_alone_and_the_tissue_block_counts_as_one_record() {
        let mut image = vec![0xFF; EEPROM_BYTES];
        image[0] = 7; // a serial the user set
        image[0x0B8..0x0BC].copy_from_slice(&f32::NAN.to_le_bytes()); // a stored NaN is data, not erasure
        image[0x0AF] = 1; // the toxicity model chosen on the handset
        image[0x10A] = 0x00; // one stored tissue byte
        image[0x38..0x41].fill(0x09); // calibration bytes: not in the inventory
        let before = image.clone();
        let filled = fill(&mut image);
        assert_eq!(filled.iter().map(|record| record.id).collect::<Vec<_>>(), ["0x68", "0x69", "0x8b", "0x8d"], "{filled:?}");
        assert_eq!(&image[0..4], &before[0..4]);
        assert_eq!(&image[0x0AF..0x0B0], &before[0x0AF..0x0B0]);
        assert_eq!(&image[0x0B8..0x0BC], &before[0x0B8..0x0BC]);
        assert_eq!(&image[0x0FF..0x17F], &before[0x0FF..0x17F], "a partly stored tissue block is never completed");
        assert_eq!(&image[0x38..0x41], &before[0x38..0x41]);
        let changed: Vec<usize> = (0..EEPROM_BYTES).filter(|&i| image[i] != before[i]).collect();
        assert!(changed.iter().all(|&i| (0x0BC..0x0BF).contains(&i) || (0x183..0x187).contains(&i) || (0x190..0x194).contains(&i)), "{changed:?}");
        // A profile that holds every value is untouched.
        let mut full = vec![0x00; EEPROM_BYTES];
        let kept = full.clone();
        assert!(fill(&mut full).is_empty() && full == kept);
    }

    #[test]
    fn the_gates_refuse_with_a_reason_and_leave_the_image_as_it_was() {
        let table = fake_table();
        // Switched off, a handset-only run.
        let mut image = None;
        let off = apply(false, true, &TRITON, Some(&table), &mut image);
        assert!(!off.enabled && !off.applied && off.reason.starts_with("Switched off") && image.is_none(), "{off:?}");
        let handset = apply(true, false, &TRITON, Some(&table), &mut image);
        assert!(handset.enabled && !handset.applied && handset.reason.starts_with("Not applicable") && image.is_none(), "{handset:?}");
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
        assert_eq!(json.get("records").and_then(Json::as_array).map(<[Json]>::len), Some(0));
        assert!(json.get("note").and_then(Json::as_str).unwrap().starts_with("Emulator fixture"));
    }

    #[test]
    fn the_report_names_each_filled_record() {
        let mut image = vec![0xFF; EEPROM_BYTES];
        let records = fill(&mut image);
        let report = FactoryInit { enabled: true, applied: true, reason: "x".to_string(), records };
        assert!(report.filled_tissues());
        let json = report.to_json();
        let list = json.get("records").and_then(Json::as_array).expect("records");
        assert_eq!(list.len(), 8);
        let base = &list[2];
        assert_eq!(base.get("id").and_then(Json::as_str), Some("0x67"));
        assert_eq!(base.get("range").and_then(Json::as_str), Some("0x0b8..0x0bb"));
        assert_eq!(base.get("value").and_then(Json::as_str), Some("0.0"));
        assert!(base.get("reason").and_then(Json::as_str).unwrap().contains("0x0800a618"));
        assert_eq!(list[5].get("range").and_then(Json::as_str), Some("0x0ff..0x17e"));
        assert!(!FactoryInit::idle(true).filled_tissues());
    }
}
