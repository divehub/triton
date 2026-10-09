//! Firmware identity, verification and flash-image construction for the supported releases of the paired
//! **main 5.8 / handset 65.3** images: `TRITON-5.8-65.3` (`DESIGN.md` section 3) and
//! `NEPTUN-5.8-65.3` (DESIGN 15.3e).
//!
//! Identity is by SHA-256 of the SREC file as supplied. A recognized file is then verified
//! against the facts recorded for it (record counts, payload size, segment layout, binary span
//! hash, vector table). The span is the contiguous byte range from the lowest to the highest
//! data address with holes filled with `0xFF` - exactly the file Renode loaded with
//! `LoadBinary` at `0x08004000` into a zero-initialized flash.
//!
//! # Releases
//!
//! A [`Release`] groups the two images that belong together (main + handset) with the
//! **firmware-specific addresses** the engine reads for its state document and fixtures
//! ([`ReleaseAddresses`]): application RAM variables and one code address. They are properties of an
//! image, not of the hardware model; every address of a release other than TRITON is either proven to be
//! the equivalent of the TRITON one (byte matching, see the `basis` texts) or reported as unavailable with
//! a reason, never silently taken over from TRITON. A session needs both images from the same release.

use crate::sha256;
use crate::srec::{self, Srec, SrecError};
use emu_core::{Json, MemoryLayout};
use std::fmt;

/// Which board a firmware image belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Main,
    Handset,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Handset => "handset",
        }
    }

    /// The facts of the TRITON image of this role (the default release; see [`Release::expected`]).
    pub fn expected(self) -> &'static Expected {
        TRITON.expected(self)
    }

    /// `main 5.8` / `handset 65.3`.
    pub fn name_with_version(self) -> &'static str {
        match self {
            Role::Main => "main 5.8",
            Role::Handset => "handset 65.3",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Facts recorded for a known image (DESIGN.md section 3).
#[derive(Debug)]
pub struct Expected {
    pub role: Role,
    pub label: &'static str,
    pub file_name: &'static str,
    pub srec_sha256: &'static str,
    pub s3_records: u32,
    /// Payload bytes in the S3 records (span minus the 4-byte hole).
    pub data_bytes: u64,
    pub span_start: u32,
    pub span_end: u32,
    pub bin_sha256: &'static str,
    /// Value of the first vector table word.
    pub initial_sp: u32,
    /// Reset handler vector entry (Thumb bit set); also the S7 start address.
    pub reset_vector: u32,
}

pub const MAIN: Expected = Expected {
    role: Role::Main,
    label: "main 5.8",
    file_name: "ngc_main_5.8_TRITON.srec",
    srec_sha256: "838cb050fa572dddca3153f43a1768db0a0665db4cde0567749fb7be8d18d6ea",
    s3_records: 11_671,
    data_bytes: 186_676,
    span_start: 0x0800_4000,
    span_end: 0x0803_1938,
    bin_sha256: "76af4ba51029afa93e788fa11bca74bad5dea7be7b14b0f8ee59baea5bfd014d",
    initial_sp: 0x2001_8000,
    reset_vector: 0x0802_13B9,
};

pub const HANDSET: Expected = Expected {
    role: Role::Handset,
    label: "handset 65.3",
    file_name: "ngc_handset_65.3_TRITON.srec",
    srec_sha256: "71a9af68de1d23d4f845784bcbf8ccf72dcd9888587e0da0125ff41c74ea1e03",
    s3_records: 44_152,
    data_bytes: 706_376,
    span_start: 0x0800_4000,
    span_end: 0x080B_074C,
    bin_sha256: "f9a85fb016081dae1e7e3c7e3007637889557df7b0e3a8142b574c21f91b9d57",
    initial_sp: 0x2001_8000,
    reset_vector: 0x0800_8411,
};

/// NEPTUN main 5.8 (facts derived from the supplied SREC: 20 059 S3 records, span `0x08004000..0x08052584`).
pub const NEPTUN_MAIN: Expected = Expected {
    role: Role::Main,
    label: "main 5.8",
    file_name: "ngc_main_5.8_NEPTUN.srec",
    srec_sha256: "e462bc7345d6ded69124b97b87de9a68884f44b8e716fbbf4fe3839ff8da8c89",
    s3_records: 20_059,
    data_bytes: 320_896,
    span_start: 0x0800_4000,
    span_end: 0x0805_2584,
    bin_sha256: "ba59fbc0977adc7910dc7e032afdaf35d5ec70cf267bff83178b309a67297ebb",
    initial_sp: 0x2001_8000,
    reset_vector: 0x0803_90B9,
};

/// NEPTUN handset 65.3 (44 197 S3 records, span `0x08004000..0x080B0A14`).
pub const NEPTUN_HANDSET: Expected = Expected {
    role: Role::Handset,
    label: "handset 65.3",
    file_name: "ngc_handset_65.3_NEPTUN.srec",
    srec_sha256: "f91adcf461fa0e06ef40ab3f757dd66754180b9711956542736efb0d4be8162e",
    s3_records: 44_197,
    data_bytes: 707_088,
    span_start: 0x0800_4000,
    span_end: 0x080B_0A14,
    bin_sha256: "6adc5760495dee487ed58e74c7ffc7bc7c71a4291f8de3e98744d073e5b89f42",
    initial_sp: 0x2001_8000,
    reset_vector: 0x0800_8445,
};

/// All images the engine knows, TRITON first.
pub const KNOWN: [&Expected; 4] = [&MAIN, &HANDSET, &NEPTUN_MAIN, &NEPTUN_HANDSET];

/// An address that depends on the firmware image rather than on the hardware model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressEntry {
    /// The address in this release and how it is known (`basis`).
    Known { address: u32, basis: &'static str },
    /// No equivalent address was proven for this release; the field built on it is reported as `null` with `reason`.
    Unavailable { reason: &'static str },
}

impl AddressEntry {
    pub const fn known(address: u32, basis: &'static str) -> Self {
        AddressEntry::Known { address, basis }
    }

    pub const fn unavailable(reason: &'static str) -> Self {
        AddressEntry::Unavailable { reason }
    }

    pub fn address(&self) -> Option<u32> {
        match self {
            AddressEntry::Known { address, .. } => Some(*address),
            AddressEntry::Unavailable { .. } => None,
        }
    }

    /// Why the address is unavailable.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            AddressEntry::Known { .. } => None,
            AddressEntry::Unavailable { reason } => Some(reason),
        }
    }

    pub fn basis(&self) -> Option<&'static str> {
        match self {
            AddressEntry::Known { basis, .. } => Some(basis),
            AddressEntry::Unavailable { .. } => None,
        }
    }

    /// JSON of the state document: `{"address": n, "basis": "..."}` or `{"address": null, "reason": "..."}`.
    pub fn to_json(&self) -> Json {
        match self {
            AddressEntry::Known { address, basis } => Json::object().with("address", u64::from(*address)).with("basis", *basis),
            AddressEntry::Unavailable { reason } => Json::object().with("address", Json::Null).with("reason", *reason),
        }
    }
}

/// The firmware-specific addresses of a release (see the module documentation).
#[derive(Clone, Copy, Debug)]
pub struct ReleaseAddresses {
    /// Handset RAM byte: the display orientation; 2 keeps the Up/Down pin masks, any other value swaps them.
    pub handset_orientation: AddressEntry,
    /// Handset code address of the interrupts-disabled error loop; the system stops with an error when the PC is there.
    pub handset_error_loop: AddressEntry,
    /// Main RAM byte `mainBatteryReady`.
    pub main_battery_ready: AddressEntry,
    /// Main application variables read by the benchmark and the scenarios.
    pub main_wake_cause: AddressEntry,
    pub main_screen_mode: AddressEntry,
    pub main_mode: AddressEntry,
    pub main_hal_tick: AddressEntry,
    pub main_pressure: AddressEntry,
    pub main_temperature: AddressEntry,
    /// FreeRTOS current-TCB pointers.
    pub main_current_tcb: AddressEntry,
    pub handset_current_tcb: AddressEntry,
    /// Decompression state (see [`crate::deco`]). Main RAM address of the first of the 16 tissue records (36 bytes each,
    /// the N2 float at +24 and the He float at +28 of every record).
    pub main_deco_tissues: AddressEntry,
    /// Main RAM byte: the breathing mode; [`crate::deco::MEASURED_PPO2_MODE`] (2) takes the ppO2 from the oxygen cells.
    pub main_breathing_mode: AddressEntry,
    /// Main RAM float: the ppO2 the decompression code uses (NaN while the oxygen cells are not calibrated, in mode 2).
    pub main_ppo2: AddressEntry,
    /// Main RAM: the three cached oxygen-cell flag bytes (EEPROM record IDs `0x2f..=0x31`): bit 0 enables the cell, bits 2-3 are
    /// its calibration state (0 uncalibrated, 2 fresh, 1 aged). Read only while the ppO2 has not been computed yet.
    pub main_cell_flags: AddressEntry,
    /// Main **EEPROM** physical offset of the 128-byte stored tissue block (logical record IDs `0x6a..=0x89`, 32 words).
    pub eeprom_tissue_block: AddressEntry,
    /// Main **EEPROM** physical offset of the 4-byte last-decompression date record (logical record ID `0x8a`).
    pub eeprom_deco_date: AddressEntry,
    /// **Flash** address of the main image's EEPROM record table (0x8e entries of offset u16 and size u16, indexed by logical record
    /// ID): the layout the factory-init fixture ([`crate::eeprom_init`]) writes by; it checks the table's SHA-256 against the image.
    pub eeprom_record_table: AddressEntry,
}

impl ReleaseAddresses {
    /// `(name, entry)` of every address, in a stable order.
    pub fn entries(&self) -> [(&'static str, &AddressEntry); 18] {
        [
            ("handsetOrientation", &self.handset_orientation),
            ("handsetErrorLoopPC", &self.handset_error_loop),
            ("mainBatteryReady", &self.main_battery_ready),
            ("mainWakeCause", &self.main_wake_cause),
            ("mainScreenMode", &self.main_screen_mode),
            ("mainMode", &self.main_mode),
            ("mainHalTick", &self.main_hal_tick),
            ("mainPressure", &self.main_pressure),
            ("mainTemperature", &self.main_temperature),
            ("mainCurrentTcb", &self.main_current_tcb),
            ("handsetCurrentTcb", &self.handset_current_tcb),
            ("mainDecoTissues", &self.main_deco_tissues),
            ("mainBreathingMode", &self.main_breathing_mode),
            ("mainPpO2", &self.main_ppo2),
            ("mainCellFlags", &self.main_cell_flags),
            ("eepromTissueBlock", &self.eeprom_tissue_block),
            ("eepromDecoDate", &self.eeprom_deco_date),
            ("eepromRecordTable", &self.eeprom_record_table),
        ]
    }

    pub fn to_json(&self) -> Json {
        let mut json = Json::object();
        for (name, entry) in self.entries() {
            json.insert(name, entry.to_json());
        }
        json
    }
}

/// A supported firmware release: the main and the handset image that belong together.
#[derive(Debug)]
pub struct Release {
    /// Stable identifier, also the name of the local firmware directory (`TRITON-5.8-65.3`).
    pub id: &'static str,
    pub label: &'static str,
    pub main: &'static Expected,
    pub handset: &'static Expected,
    pub addresses: ReleaseAddresses,
    /// `Some(reason)` when the cold-boot fixture (zero wake flags, observed standby request) is refused for this release.
    pub cold_boot_refusal: Option<&'static str>,
}

impl Release {
    pub fn expected(&self, role: Role) -> &'static Expected {
        match role {
            Role::Main => self.main,
            Role::Handset => self.handset,
        }
    }

    /// `{"id": "...", "label": "..."}` (the `release` member of the inspect and state documents).
    pub fn to_json(&self) -> Json {
        Json::object().with("id", self.id).with("label", self.label)
    }

    pub fn by_id(id: &str) -> Option<&'static Release> {
        RELEASES.iter().copied().find(|release| release.id == id)
    }
}

/// The TRITON release: the images all earlier evidence and the scenario suite refer to. The addresses are the
/// ones the Renode runner of the analysis workspace and the static analysis of these images established.
pub static TRITON: Release = Release {
    id: "TRITON-5.8-65.3",
    label: "TRITON main 5.8 / handset 65.3",
    main: &MAIN,
    handset: &HANDSET,
    addresses: ReleaseAddresses {
        handset_orientation: AddressEntry::known(0x2000_0740, "TRITON static analysis (runner INPUT mask swap); the reference for the NEPTUN mapping"),
        handset_error_loop: AddressEntry::known(0x0800_598E, "TRITON static analysis: interrupts-disabled error loop of the handset"),
        main_battery_ready: AddressEntry::known(0x2000_42A1, "TRITON static analysis (runner mainBatteryReady)"),
        main_wake_cause: AddressEntry::known(0x2000_4388, "TRITON static analysis (wake cause classification)"),
        main_screen_mode: AddressEntry::known(0x2000_438D, "TRITON static analysis"),
        main_mode: AddressEntry::known(0x2000_24B2, "TRITON static analysis"),
        main_hal_tick: AddressEntry::known(0x2000_4A6C, "TRITON static analysis (HAL tick of the main TIM6)"),
        main_pressure: AddressEntry::known(0x2000_4378, "TRITON static analysis"),
        main_temperature: AddressEntry::known(0x2000_4360, "TRITON static analysis"),
        main_current_tcb: AddressEntry::known(0x2000_5708, "FreeRTOS pxCurrentTCB, PendSV literal of the main image"),
        handset_current_tcb: AddressEntry::known(0x2000_13FC, "FreeRTOS pxCurrentTCB, PendSV literal of the handset image"),
        main_deco_tissues: AddressEntry::known(
            0x2000_1E94,
            "Renode hooks on the start-up initializer 0x08008308 and the NDL routine (0x08008550, 0x0800857e): 16 records of 36 bytes, N2 float at +24 and He float at +28; the initializer loads them from the EEPROM block, and every NaN word of the loaded block was seen at this address",
        ),
        main_breathing_mode: AddressEntry::known(0x2000_2457, "Renode hooks (same routines): byte 2 = ppO2 measured by the oxygen cells; the settings default is 2"),
        main_ppo2: AddressEntry::known(
            0x2000_421C,
            "Renode hooks (same routines): the ppO2 the NDL routine reads (the median cell ppO2); NaN (0x7fc00000) with uncalibrated cells in mode 2, finite after the air calibration",
        ),
        main_cell_flags: AddressEntry::known(
            0x2000_23F4,
            "cache of the EEPROM cell flags (records 0x2f..=0x31) filled by the settings loader 0x080091fc; observed on this engine: 0x01 x3 on a fresh profile, 0x09 x3 after the firmware's air calibration, 0x01 x3 again after a cold boot",
        ),
        eeprom_tissue_block: AddressEntry::known(
            0x0FF,
            "EEPROM record table of the main image (entries of offset u16 and size u16 at 0x080306f2 + 4 * record ID): IDs 0x6a..=0x89 are 32 words at physical 0x0ff..=0x17e; the block is written only by the power-down route",
        ),
        eeprom_deco_date: AddressEntry::known(
            0x17F,
            "EEPROM record table of the main image: ID 0x8a is 4 bytes at physical 0x17f (packed RTC calendar of the last decompression; 0xffffffff when erased); erasing it makes the initializer (0x08008308) take its >= 4 day reset path",
        ),
        eeprom_record_table: AddressEntry::known(
            0x0803_06F2,
            "entry of ID 0 of the 568-byte table (IDs 0..=0x8d, offset u16 and size u16 each) that the EEPROM read/write wrappers 0x08010344 and 0x08010430 and the table walker 0x08010494 load; SHA-256 a69c0b84...4672",
        ),
    },
    cold_boot_refusal: None,
};

/// Why the NEPTUN main application variables cannot be taken over from TRITON: the NEPTUN main image (320 900 bytes
/// against 186 680) is a different build of the application: its functions use frame-pointer code (`push {r7, lr}`,
/// `add r7, sp, #0`) where the TRITON ones are optimized, so none of the TRITON functions that touch these variables has
/// an instruction-identical counterpart. Only hand-written code (the FreeRTOS PendSV handler) survived byte for byte.
const NEPTUN_MAIN_BUILD: &str = "no byte-identical counterpart in the NEPTUN main image (a different, frame-pointer build of the application); equivalent not proven";

/// The NEPTUN release. Every address is either proven to be the equivalent of the TRITON one by matching code bytes
/// (the `basis` texts name the sites) or unavailable with a reason.
pub static NEPTUN: Release = Release {
    id: "NEPTUN-5.8-65.3",
    label: "NEPTUN main 5.8 / handset 65.3",
    main: &NEPTUN_MAIN,
    handset: &NEPTUN_HANDSET,
    addresses: ReleaseAddresses {
        handset_orientation: AddressEntry::known(
            0x2000_0740,
            "key sampler of the handset: the 26/27 instructions around the two TRITON loads (0x08045fb2, 0x08045fc2) occur exactly once each in NEPTUN (0x08046276, 0x08046286), loading the same literal",
        ),
        handset_error_loop: AddressEntry::known(
            0x0800_598E,
            "HAL error handler `cpsid i; b .`: the 80 instructions 0x08005940..0x080059fe are identical in both handset images at the same addresses (only BL targets differ), including the literal pool",
        ),
        main_battery_ready: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_wake_cause: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_screen_mode: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_mode: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_hal_tick: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_pressure: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_temperature: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_current_tcb: AddressEntry::known(
            0x2000_53A8,
            "FreeRTOS PendSV handler (vector 14): TRITON 0x08028a40 and NEPTUN 0x080470d0 are the same 27 instructions (only the literal and the BL target differ); the literal is pxCurrentTCB (TRITON 0x20005708)",
        ),
        handset_current_tcb: AddressEntry::known(
            0x2000_13FC,
            "FreeRTOS kernel of the handset: 13 of the 15 TRITON access sites have exactly one identical 26..36 instruction window in NEPTUN and load the same literal",
        ),
        main_deco_tissues: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_breathing_mode: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_ppo2: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        main_cell_flags: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        eeprom_tissue_block: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        eeprom_deco_date: AddressEntry::unavailable(NEPTUN_MAIN_BUILD),
        eeprom_record_table: AddressEntry::known(
            0x0805_0E38,
            "the TRITON table (568 bytes, SHA-256 a69c0b84...4672) occurs byte for byte in the NEPTUN main image, exactly once, and three literal-pool words of its accessor code point at it (0x0801ad40, 0x0801adec, 0x0801b124), like the three sites of TRITON's wrappers; the record layout (ID to offset and size) is therefore the same. The factory values were also checked on NEPTUN's own first boot (docs/eeprom.md)",
        ),
    },
    cold_boot_refusal: Some(
        "The cold-boot fixture is characterized for TRITON-5.8-65.3 only: with zero PWR.SR1/RCC.CSR wake flags the TRITON main requests standby after about 1.5 virtual seconds, but the NEPTUN main kept running for 40 virtual seconds without a standby request, so the fixture's observed-standby route does not exist for this release. Use Restart or Wake instead.",
    ),
};

/// The releases this engine supports, TRITON first (the default).
pub const RELEASES: [&Release; 2] = [&TRITON, &NEPTUN];

/// The release and role an `Expected` entry belongs to.
fn release_of(expected: &Expected) -> &'static Release {
    RELEASES.iter().copied().find(|release| release.expected(expected.role).srec_sha256 == expected.srec_sha256).expect("every known image belongs to a release")
}

/// Both images keep the application vector table at the start of the span.
pub const APPLICATION_BASE: u32 = 0x0800_4000;
/// 16 system exceptions + 83 external interrupts.
pub const VECTOR_ENTRIES: usize = 99;
pub const VECTOR_TABLE_BYTES: u32 = 0x18C;
/// The supplied images omit the manufacturer bootloader and have a 4-byte hole after the table.
pub const SEGMENT_SPLIT: (u32, u32) = (0x0800_418C, 0x0800_4190);

/// Identifies a known image: by the SHA-256 of the SREC file as supplied, or - when the file text
/// differs only in formatting such as line endings - by the SHA-256 of its reconstructed binary
/// span (the actual firmware content).
pub fn identify(srec_bytes: &[u8]) -> Option<Role> {
    identify_release(srec_bytes).map(|(_, role)| role)
}

/// [`identify`] with the release the image belongs to.
pub fn identify_release(srec_bytes: &[u8]) -> Option<(&'static Release, Role)> {
    let hash = sha256::digest_hex(srec_bytes);
    if let Some(e) = KNOWN.iter().find(|e| e.srec_sha256 == hash) {
        return Some((release_of(e), e.role));
    }
    let report = inspect(srec_bytes);
    report.release.zip(report.identified)
}

/// The release a main and a handset image have in common, or the refusal text of a mixed pair (DESIGN 15.3e: the
/// engine never combines images of different releases, because every release has its own addresses and behavior).
pub fn common_release(main: &Firmware, handset: &Firmware) -> Result<&'static Release, String> {
    if main.role != Role::Main {
        return Err(format!("the main slot holds the {} image of {}", main.role, main.release.id));
    }
    if handset.role != Role::Handset {
        return Err(format!("the handset slot holds the {} image of {}", handset.role, handset.release.id));
    }
    if main.release.id != handset.release.id {
        return Err(format!(
            "Mixed firmware releases: the main image is {} ({}) but the handset image is {} ({}). Supply both images of the same release.",
            main.release.id,
            main.release.main.file_name,
            handset.release.id,
            handset.release.handset.file_name
        ));
    }
    Ok(main.release)
}

/// One verification result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Decoded vector table words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorInfo {
    pub initial_sp: u32,
    /// Reset vector word as stored (Thumb bit set).
    pub reset_vector: u32,
    pub entries: Vec<u32>,
}

impl VectorInfo {
    /// Reset handler address with the Thumb bit cleared (what `Cpu::set_pc` takes).
    pub fn reset_pc(&self) -> u32 {
        self.reset_vector & !1
    }
}

/// Everything learned about an SREC file, whether or not it is a known image.
#[derive(Clone, Debug)]
pub struct Report {
    pub srec_bytes: usize,
    pub srec_sha256: String,
    pub identified: Option<Role>,
    /// The release of an identified image.
    pub release: Option<&'static Release>,
    pub parse_error: Option<String>,
    pub header: String,
    pub record_counts: [u32; 10],
    /// Segment `(start, end)` pairs, end exclusive.
    pub segments: Vec<(u32, u32)>,
    /// S7/S8/S9 start address.
    pub entry: Option<u32>,
    /// Binary span `(start, end)`, end exclusive.
    pub span: Option<(u32, u32)>,
    pub bin_sha256: Option<String>,
    pub vectors: Option<VectorInfo>,
    pub checks: Vec<Check>,
}

impl Report {
    /// True when the file parsed, was identified and every check passed.
    pub fn ok(&self) -> bool {
        self.parse_error.is_none() && self.identified.is_some() && self.checks.iter().all(|c| c.ok)
    }

    pub fn failed_checks(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }

    /// JSON for the browser file-validation step and `ngc-cli info --json`.
    pub fn to_json(&self) -> Json {
        let mut counts = Json::object();
        for (digit, &count) in self.record_counts.iter().enumerate() {
            if count > 0 {
                counts.insert(format!("S{digit}"), count);
            }
        }
        let segments = Json::from_items(self.segments.iter().map(|&(start, end)| {
            Json::object().with("start", u64::from(start)).with("end", u64::from(end))
        }));
        let checks = Json::from_items(self.checks.iter().map(|c| {
            Json::object().with("name", c.name).with("ok", c.ok).with("detail", c.detail.as_str())
        }));
        let mut json = Json::object()
            .with("ok", self.ok())
            .with("role", self.identified.map(|r| r.name()))
            .with("release", self.release.map(Release::to_json))
            .with("srecBytes", self.srec_bytes)
            .with("srecSha256", self.srec_sha256.as_str())
            .with("header", self.header.as_str())
            .with("recordCounts", counts)
            .with("segments", segments)
            .with("entry", self.entry.map(u64::from))
            .with("binSha256", self.bin_sha256.as_deref())
            .with("checks", checks)
            .with("error", self.parse_error.as_deref());
        if let Some((start, end)) = self.span {
            json.insert("span", Json::object().with("start", u64::from(start)).with("end", u64::from(end)));
        } else {
            json.insert("span", Json::Null);
        }
        if let Some(v) = &self.vectors {
            json.insert("initialSp", u64::from(v.initial_sp));
            json.insert("resetPc", u64::from(v.reset_pc()));
        }
        json
    }
}

#[derive(Debug)]
pub enum FirmwareError {
    /// The SREC text is malformed.
    Parse(SrecError),
    /// Valid SREC, but not one of the supported images.
    Unknown { srec_sha256: String },
    /// A known image, but not the one expected for this slot.
    WrongRole { expected: Role, found: Role, release: &'static Release },
    /// Recognized by hash but a recorded fact does not hold (should not happen for an intact file).
    Verification { role: Role, release: &'static Release, failed: Vec<Check> },
}

impl fmt::Display for FirmwareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirmwareError::Parse(e) => write!(f, "not a valid S-record file: {e}"),
            FirmwareError::Unknown { srec_sha256 } => {
                write!(f, "unrecognized firmware (SHA-256 {srec_sha256}); expected main 5.8 or handset 65.3 of")?;
                for (i, release) in RELEASES.iter().enumerate() {
                    write!(f, "{} {} ({} / {})", if i == 0 { "" } else { " or" }, release.id, release.main.file_name, release.handset.file_name)?;
                }
                Ok(())
            }
            FirmwareError::WrongRole { expected, found, release } => {
                write!(f, "this is the {found} firmware ({} {}) but the {expected} image is required", release.id, found.name_with_version())
            }
            FirmwareError::Verification { role, release, failed } => {
                write!(f, "{role} firmware ({}) failed verification: ", release.id)?;
                for (i, check) in failed.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{}: {}", check.name, check.detail)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for FirmwareError {}

impl From<SrecError> for FirmwareError {
    fn from(e: SrecError) -> Self {
        FirmwareError::Parse(e)
    }
}

/// A verified firmware image ready to be installed in a board's flash.
#[derive(Clone, Debug)]
pub struct Firmware {
    pub role: Role,
    /// The release this image belongs to (identified by the SREC or span hash).
    pub release: &'static Release,
    /// Address of the first span byte (`0x08004000`).
    pub span_base: u32,
    /// The binary span: lowest to highest data address, holes filled with `0xFF`.
    pub span: Vec<u8>,
    pub srec_sha256: [u8; 32],
    pub bin_sha256: [u8; 32],
    pub vectors: VectorInfo,
    pub report: Report,
}

impl Firmware {
    /// Vector table location for `VTOR`.
    pub fn vtor(&self) -> u32 {
        self.span_base
    }

    pub fn initial_sp(&self) -> u32 {
        self.vectors.initial_sp
    }

    /// Reset handler address (Thumb bit cleared).
    pub fn reset_pc(&self) -> u32 {
        self.vectors.reset_pc()
    }

    /// The span as Renode loads it (`LoadBinary <file> 0x08004000`).
    pub fn bin(&self) -> &[u8] {
        &self.span
    }

    /// Copies the span into `flash` (which starts at the flash base address).
    pub fn install_into(&self, flash: &mut [u8]) -> Result<(), String> {
        let base = MemoryLayout::STM32L4_1M.flash_base;
        let offset = self.span_base.checked_sub(base).ok_or("span starts below the flash base")? as usize;
        let end = offset + self.span.len();
        if end > flash.len() {
            return Err(format!("span of {} bytes at 0x{:08x} does not fit in {} bytes of flash", self.span.len(), self.span_base, flash.len()));
        }
        flash[offset..end].copy_from_slice(&self.span);
        Ok(())
    }

    /// A fresh zero-initialized flash image (1 MiB) with the span installed.
    pub fn flash_image(&self) -> Vec<u8> {
        let mut flash = vec![0u8; MemoryLayout::STM32L4_1M.flash_size as usize];
        self.install_into(&mut flash).expect("verified span fits in flash");
        flash
    }
}

fn check(name: &'static str, ok: bool, detail: String) -> Check {
    Check { name, ok, detail }
}

fn word_at(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([bytes[4 * index], bytes[4 * index + 1], bytes[4 * index + 2], bytes[4 * index + 3]])
}

/// Parses and verifies an SREC file without failing: the report carries every fact that could be
/// established and a list of checks (used by `ngc-cli info` and the browser validation step).
pub fn inspect(srec_bytes: &[u8]) -> Report {
    inspect_parsed(srec_bytes).0
}

fn inspect_parsed(srec_bytes: &[u8]) -> (Report, Option<Srec>, Option<Vec<u8>>) {
    let srec_sha256 = sha256::digest_hex(srec_bytes);
    let by_file = KNOWN.iter().find(|e| e.srec_sha256 == srec_sha256).copied();
    let mut report = Report {
        srec_bytes: srec_bytes.len(),
        srec_sha256,
        identified: by_file.map(|e| e.role),
        release: by_file.map(release_of),
        parse_error: None,
        header: String::new(),
        record_counts: [0; 10],
        segments: Vec::new(),
        entry: None,
        span: None,
        bin_sha256: None,
        vectors: None,
        checks: Vec::new(),
    };
    let srec = match srec::parse(srec_bytes) {
        Ok(srec) => srec,
        Err(e) => {
            report.parse_error = Some(e.to_string());
            report.checks.push(check("srec-syntax", false, e.to_string()));
            return (report, None, None);
        }
    };
    report.checks.push(check("srec-syntax", true, "all records parse; checksums, counts and overlaps are consistent".into()));
    report.header = srec.header_text();
    report.record_counts = srec.record_counts;
    report.segments = srec.segments.iter().map(|s| (s.start, s.end() as u32)).collect();
    report.entry = srec.start_address;
    let binary = srec.to_binary(0xFF);
    if let Some((base, bytes)) = &binary {
        report.span = Some((*base, (u64::from(*base) + bytes.len() as u64) as u32));
        report.bin_sha256 = Some(sha256::digest_hex(bytes));
        if *base == APPLICATION_BASE && bytes.len() >= VECTOR_TABLE_BYTES as usize {
            let entries: Vec<u32> = (0..VECTOR_ENTRIES).map(|i| word_at(bytes, i)).collect();
            report.vectors = Some(VectorInfo { initial_sp: entries[0], reset_vector: entries[1], entries });
        }
    }

    let by_span = report
        .bin_sha256
        .as_deref()
        .and_then(|hash| KNOWN.iter().find(|e| e.bin_sha256 == hash).copied());
    let identified = by_file.or(by_span);
    report.identified = identified.map(|e| e.role);
    report.release = identified.map(release_of);
    let Some(e) = identified else {
        report.checks.insert(
            0,
            check(
                "identity",
                false,
                format!("neither the file SHA-256 {} nor the binary span hash matches a known image", report.srec_sha256),
            ),
        );
        return (report, Some(srec), binary.map(|(_, bytes)| bytes));
    };
    let release = release_of(e);
    let identity_detail = if by_file.is_some() {
        format!("file SHA-256 matches {} {} ({})", release.id, e.label, e.file_name)
    } else {
        format!(
            "binary span matches {} {} ({}); the SREC text differs from the original download (SHA-256 {}), e.g. line endings",
            release.id, e.label, e.file_name, report.srec_sha256
        )
    };
    report.checks.insert(0, check("identity", true, identity_detail));

    // Record layout is a property of the original file text; a re-formatted file with identical
    // content (checked below through the segments, span hash and vectors) stays acceptable.
    if by_file.is_some() {
        let counts = &srec.record_counts;
        let counts_ok = counts[0] == 1
            && counts[3] == e.s3_records
            && counts[7] == 1
            && counts.iter().enumerate().all(|(i, &n)| matches!(i, 0 | 3 | 7) || n == 0);
        report.checks.push(check(
            "record-counts",
            counts_ok,
            format!("S0 {}, S3 {}, S7 {} (expected 1, {}, 1)", counts[0], counts[3], counts[7], e.s3_records),
        ));
    }
    report.checks.push(check(
        "data-bytes",
        srec.data_bytes == e.data_bytes,
        format!("{} payload bytes (expected {})", srec.data_bytes, e.data_bytes),
    ));
    let expected_segments = [(e.span_start, SEGMENT_SPLIT.0), (SEGMENT_SPLIT.1, e.span_end)];
    report.checks.push(check(
        "segments",
        report.segments == expected_segments,
        format!(
            "{} (expected {})",
            fmt_ranges(&report.segments),
            fmt_ranges(&expected_segments)
        ),
    ));
    let span_ok = report.span == Some((e.span_start, e.span_end));
    report.checks.push(check(
        "span",
        span_ok,
        format!(
            "{} (expected 0x{:08x}..0x{:08x}, {} bytes)",
            report.span.map_or("none".to_string(), |(a, b)| format!("0x{a:08x}..0x{b:08x}, {} bytes", b - a)),
            e.span_start,
            e.span_end,
            e.span_end - e.span_start
        ),
    ));
    report.checks.push(check(
        "bin-sha256",
        report.bin_sha256.as_deref() == Some(e.bin_sha256),
        format!("{} (expected {})", report.bin_sha256.as_deref().unwrap_or("none"), e.bin_sha256),
    ));
    match &report.vectors {
        Some(v) => {
            report.checks.push(check(
                "vector-table",
                v.initial_sp == e.initial_sp && v.reset_vector == e.reset_vector,
                format!(
                    "initial SP 0x{:08x}, reset vector 0x{:08x} (expected 0x{:08x}, 0x{:08x})",
                    v.initial_sp, v.reset_vector, e.initial_sp, e.reset_vector
                ),
            ));
        }
        None => report.checks.push(check("vector-table", false, "no vector table at 0x08004000".into())),
    }
    report.checks.push(check(
        "entry-point",
        srec.start_address == Some(e.reset_vector),
        format!(
            "S7 start address {} (expected 0x{:08x})",
            srec.start_address.map_or("none".to_string(), |a| format!("0x{a:08x}")),
            e.reset_vector
        ),
    ));
    (report, Some(srec), binary.map(|(_, bytes)| bytes))
}

fn fmt_ranges(ranges: &[(u32, u32)]) -> String {
    ranges.iter().map(|(a, b)| format!("0x{a:08x}..0x{b:08x}")).collect::<Vec<_>>().join(" + ")
}

/// Parses, identifies and fully verifies an SREC file. With `expect`, a recognized image for the
/// other board is rejected (`WrongRole`), so the browser can tell the user they swapped the files.
pub fn load(srec_bytes: &[u8], expect: Option<Role>) -> Result<Firmware, FirmwareError> {
    let (report, srec, binary) = inspect_parsed(srec_bytes);
    if srec.is_none() {
        // The only way to get here without a parsed file is a syntax error.
        return Err(FirmwareError::Parse(srec::parse(srec_bytes).expect_err("inspect found a parse error")));
    }
    let (Some(role), Some(release)) = (report.identified, report.release) else {
        return Err(FirmwareError::Unknown { srec_sha256: report.srec_sha256 });
    };
    if let Some(expected) = expect {
        if expected != role {
            return Err(FirmwareError::WrongRole { expected, found: role, release });
        }
    }
    if !report.ok() {
        let failed = report.checks.iter().filter(|c| !c.ok).cloned().collect();
        return Err(FirmwareError::Verification { role, release, failed });
    }
    let span = binary.expect("verified image has a span");
    let digest = sha256::digest(&span);
    let srec_sha256 = sha256::digest(srec_bytes);
    let vectors = report.vectors.clone().expect("verified image has a vector table");
    Ok(Firmware {
        role,
        release,
        span_base: report.span.expect("span").0,
        span,
        srec_sha256,
        bin_sha256: digest,
        vectors,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `TRITON-5.8-65.3/<name>` below the directory named by `NGC_FIRMWARE_DIR`, or below the repository's
    /// `firmware/` directory (the SRECs are never committed, so a clone may not have them). `None` skips the
    /// real-image tests.
    pub(crate) fn firmware_path(name: &str) -> Option<PathBuf> {
        let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
        roots.into_iter().flatten().map(|root| root.join("TRITON-5.8-65.3").join(name)).find(|p| p.is_file())
    }

    fn read(role: Role) -> Option<Vec<u8>> {
        let path = firmware_path(role.expected().file_name)?;
        Some(std::fs::read(path).expect("read srec"))
    }

    macro_rules! require {
        ($role:expr) => {
            match read($role) {
                Some(bytes) => bytes,
                None => {
                    eprintln!("skipping: {} SREC not available locally", $role);
                    return;
                }
            }
        };
    }

    #[test]
    fn expected_tables_are_consistent() {
        for e in KNOWN {
            assert_eq!(e.span_end - e.span_start, (e.data_bytes + 4) as u32, "{}", e.label);
            assert_eq!(e.srec_sha256.len(), 64);
            assert_eq!(e.bin_sha256.len(), 64);
            assert_eq!(e.reset_vector & 1, 1);
            assert!(sha256::from_hex(e.srec_sha256).is_some() && sha256::from_hex(e.bin_sha256).is_some());
        }
        assert_eq!(MAIN.span_end - MAIN.span_start, 186_680);
        assert_eq!(HANDSET.span_end - HANDSET.span_start, 706_380);
        assert_eq!(SEGMENT_SPLIT.0 - APPLICATION_BASE, VECTOR_TABLE_BYTES);
        assert_eq!(VECTOR_TABLE_BYTES as usize, VECTOR_ENTRIES * 4);
    }

    #[test]
    fn real_main_image_matches_every_recorded_fact() {
        let bytes = require!(Role::Main);
        assert_eq!(identify(&bytes), Some(Role::Main));
        let fw = load(&bytes, Some(Role::Main)).unwrap();
        assert_eq!(fw.role, Role::Main);
        assert_eq!(fw.span.len(), 186_680);
        assert_eq!(sha256::to_hex(&fw.bin_sha256), MAIN.bin_sha256);
        assert_eq!(sha256::to_hex(&fw.srec_sha256), MAIN.srec_sha256);
        assert_eq!((fw.vtor(), fw.initial_sp(), fw.reset_pc()), (0x0800_4000, 0x2001_8000, 0x0802_13B8));
        assert_eq!(fw.vectors.entries.len(), 99);
        assert_eq!(fw.report.header, "triton_main_controller_eval.srec");
        assert_eq!(fw.report.record_counts[0], 1);
        assert_eq!(fw.report.record_counts[3], 11_671);
        assert_eq!(fw.report.record_counts[7], 1);
        assert!(fw.report.ok(), "{:?}", fw.report.failed_checks());
        assert_eq!(fw.report.checks.len(), 9);
        // The hole after the vector table is 0xFF in the binary span and 0x00 in a fresh flash image.
        assert_eq!(&fw.span[0x18C..0x190], &[0xFF; 4]);
        let flash = fw.flash_image();
        assert_eq!(flash.len(), 0x10_0000);
        assert_eq!(&flash[0x4000..0x4000 + 4], &0x2001_8000u32.to_le_bytes());
        assert_eq!(&flash[0x4000 + 0x18C..0x4000 + 0x190], &[0xFF; 4]);
        assert!(flash[..0x4000].iter().all(|&b| b == 0), "bytes below the span stay zero");
        assert!(flash[0x4000 + fw.span.len()..].iter().all(|&b| b == 0), "bytes above the span stay zero");
    }

    #[test]
    fn real_handset_image_matches_every_recorded_fact() {
        let bytes = require!(Role::Handset);
        assert_eq!(identify(&bytes), Some(Role::Handset));
        let fw = load(&bytes, None).unwrap();
        assert_eq!(fw.role, Role::Handset);
        assert_eq!(fw.span.len(), 706_380);
        assert_eq!(sha256::to_hex(&fw.bin_sha256), HANDSET.bin_sha256);
        assert_eq!((fw.vtor(), fw.initial_sp(), fw.reset_pc()), (0x0800_4000, 0x2001_8000, 0x0800_8410));
        assert_eq!(fw.report.header, "display_eval.srec");
        assert_eq!(fw.report.record_counts[3], 44_152);
        assert!(fw.report.ok(), "{:?}", fw.report.failed_checks());
        assert_eq!(fw.report.segments, [(0x0800_4000, 0x0800_418C), (0x0800_4190, 0x080B_074C)]);
        assert_eq!(fw.flash_image().len(), 0x10_0000);
    }

    #[test]
    fn wrong_slot_and_unknown_files_are_rejected_with_clear_errors() {
        let main = require!(Role::Main);
        let e = load(&main, Some(Role::Handset)).unwrap_err();
        assert!(matches!(e, FirmwareError::WrongRole { expected: Role::Handset, found: Role::Main, .. }));
        assert!(e.to_string().contains("main firmware"), "{e}");
        // A corrupted record (broken checksum) is a parse error, not an unknown image.
        let mut altered = main.clone();
        let newline = altered.iter().position(|&b| b == b'\n').unwrap();
        altered[newline - 1] = if altered[newline - 1] == b'0' { b'1' } else { b'0' };
        assert!(matches!(load(&altered, None), Err(FirmwareError::Parse(_))));
        // Valid SREC but different content.
        let other = b"S0030000FC\nS1050000AABB95\nS9030000FC\n";
        let e = load(other, None).unwrap_err();
        assert!(matches!(e, FirmwareError::Unknown { .. }));
        assert!(e.to_string().contains("unrecognized firmware"));
        let report = inspect(other);
        assert!(!report.ok());
        assert_eq!(report.identified, None);
        assert_eq!(report.segments, [(0, 2)]);
        assert!(!report.checks[0].ok && report.checks[0].name == "identity");
        // Garbage.
        let e = load(b"not an srec", None).unwrap_err();
        assert!(matches!(e, FirmwareError::Parse(_)));
        assert!(!inspect(b"not an srec").ok());
    }

    #[test]
    fn reformatted_srec_is_identified_by_its_content() {
        let bytes = require!(Role::Main);
        let text = String::from_utf8(bytes).unwrap();
        // The downloaded files use CRLF line endings; a copy converted to LF is the same firmware.
        let converted = if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text.replace('\n', "\r\n") };
        assert_ne!(sha256::digest_hex(converted.as_bytes()), MAIN.srec_sha256, "the file bytes differ");
        assert_eq!(identify(converted.as_bytes()), Some(Role::Main));
        let fw = load(converted.as_bytes(), Some(Role::Main)).unwrap();
        assert_eq!(sha256::to_hex(&fw.bin_sha256), MAIN.bin_sha256);
        assert_eq!(fw.report.checks[0].name, "identity");
        assert!(fw.report.checks[0].ok && fw.report.checks[0].detail.contains("differs from the original"), "{:?}", fw.report.checks[0]);
        assert_eq!(fw.report.checks.len(), 8, "record layout is not checked for re-formatted files");
        assert_eq!(sha256::to_hex(&fw.srec_sha256), sha256::digest_hex(converted.as_bytes()));
        // Changed *content* with perfectly valid records is still rejected.
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        let index = lines.len() - 2; // last S3 data record
        let line = lines[index].clone();
        assert!(line.starts_with("S3"));
        let mut payload: Vec<u8> = (2..line.len() - 2).step_by(2).map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap()).collect();
        let last_data = payload.len() - 1;
        payload[last_data] ^= 0x01;
        let sum = payload.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        lines[index] = format!("S3{}{:02X}", payload.iter().map(|b| format!("{b:02X}")).collect::<String>(), !sum);
        let altered = lines.join("\n");
        assert!(matches!(load(altered.as_bytes(), None), Err(FirmwareError::Unknown { .. })));
        assert_eq!(identify(altered.as_bytes()), None);
        let report = inspect(altered.as_bytes());
        assert!(!report.ok() && report.parse_error.is_none() && !report.checks[0].ok);
    }

    #[test]
    fn report_json_shape() {
        let bytes = require!(Role::Handset);
        let json = inspect(&bytes).to_json();
        assert_eq!(json.get("ok").and_then(Json::as_bool), Some(true));
        assert_eq!(json.get("role").and_then(Json::as_str), Some("handset"));
        assert_eq!(json.get("binSha256").and_then(Json::as_str), Some(HANDSET.bin_sha256));
        assert_eq!(json.get("resetPc").and_then(Json::as_u64), Some(0x0800_8410));
        assert_eq!(json.get("recordCounts").unwrap().get("S3").and_then(Json::as_u64), Some(44_152));
        assert_eq!(json.get("checks").unwrap().len(), 9);
        assert_eq!(json.get("span").unwrap().get("end").and_then(Json::as_u64), Some(0x080B_074C));
        let text = json.to_string();
        assert_eq!(Json::parse(&text).unwrap(), json);
        let bad = inspect(b"zzz").to_json();
        assert_eq!(bad.get("ok").and_then(Json::as_bool), Some(false));
        assert!(bad.get("error").and_then(Json::as_str).is_some());
        assert!(bad.get("role").unwrap().is_null());
    }

    #[test]
    fn install_into_validates_bounds() {
        let Some(bytes) = read(Role::Main) else { return };
        let fw = load(&bytes, None).unwrap();
        let mut small = vec![0u8; 0x4000 + 100];
        assert!(fw.install_into(&mut small).is_err());
        let mut flash = vec![0u8; 0x10_0000];
        fw.install_into(&mut flash).unwrap();
        assert_eq!(&flash[0x4000..0x4004], &0x2001_8000u32.to_le_bytes());
    }

    // ---- releases ----

    fn neptun(role: Role) -> Option<Vec<u8>> {
        let name = NEPTUN.expected(role).file_name;
        let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
        roots
            .into_iter()
            .flatten()
            .map(|root| root.join("NEPTUN-5.8-65.3").join(name))
            .find(|p| p.is_file())
            .map(|p| std::fs::read(p).expect("read srec"))
    }

    #[test]
    fn the_release_table_groups_the_images() {
        assert_eq!(RELEASES.iter().map(|r| r.id).collect::<Vec<_>>(), ["TRITON-5.8-65.3", "NEPTUN-5.8-65.3"]);
        assert_eq!(Role::Main.expected().file_name, "ngc_main_5.8_TRITON.srec", "Role::expected is the TRITON default");
        assert_eq!(Release::by_id("NEPTUN-5.8-65.3").map(|r| r.main.file_name), Some("ngc_main_5.8_NEPTUN.srec"));
        assert!(Release::by_id("ATLANTIS").is_none());
        for release in RELEASES {
            assert_eq!(release.expected(Role::Main).role, Role::Main);
            assert_eq!(release.expected(Role::Handset).role, Role::Handset);
            assert_eq!(release_of(release.main).id, release.id);
            assert_eq!(release_of(release.handset).id, release.id);
            assert!(release.main.file_name.contains(release.id.split('-').next().unwrap()), "{}", release.main.file_name);
            let json = release.to_json();
            assert_eq!(json.get("id").and_then(Json::as_str), Some(release.id));
            assert_eq!(json.get("label").and_then(Json::as_str), Some(release.label));
        }
        // NEPTUN facts given by the planner (hashes, reset vectors) and derived from the supplied files.
        assert_eq!(NEPTUN_MAIN.srec_sha256, "e462bc7345d6ded69124b97b87de9a68884f44b8e716fbbf4fe3839ff8da8c89");
        assert_eq!(NEPTUN_HANDSET.srec_sha256, "f91adcf461fa0e06ef40ab3f757dd66754180b9711956542736efb0d4be8162e");
        assert_eq!((NEPTUN_MAIN.reset_vector & !1, NEPTUN_HANDSET.reset_vector & !1), (0x0803_90B8, 0x0800_8444));
        assert_eq!((NEPTUN_MAIN.initial_sp, NEPTUN_HANDSET.initial_sp), (0x2001_8000, 0x2001_8000));
        assert_eq!((NEPTUN_MAIN.span_end - NEPTUN_MAIN.span_start, NEPTUN_HANDSET.span_end - NEPTUN_HANDSET.span_start), (320_900, 707_092));
        // Every image hash belongs to exactly one image.
        for (i, a) in KNOWN.iter().enumerate() {
            for b in &KNOWN[i + 1..] {
                assert!(a.srec_sha256 != b.srec_sha256 && a.bin_sha256 != b.bin_sha256);
            }
        }
    }

    #[test]
    fn address_entries_are_known_with_a_basis_or_unavailable_with_a_reason() {
        for release in RELEASES {
            for (name, entry) in release.addresses.entries() {
                match entry {
                    AddressEntry::Known { address, basis } => assert!(*address != 0 && !basis.is_empty(), "{} {name}", release.id),
                    AddressEntry::Unavailable { reason } => assert!(!reason.is_empty(), "{} {name}", release.id),
                }
            }
            let json = release.addresses.to_json();
            assert_eq!(json.len(), 18);
        }
        // TRITON has every address; the NEPTUN main application variables are unavailable (never a TRITON value).
        assert!(TRITON.addresses.entries().iter().all(|(_, entry)| entry.address().is_some()));
        assert_eq!(TRITON.addresses.main_battery_ready.address(), Some(0x2000_42A1));
        assert!(NEPTUN.addresses.main_battery_ready.address().is_none() && NEPTUN.addresses.main_battery_ready.reason().is_some());
        assert_eq!(NEPTUN.addresses.main_battery_ready.to_json().get("address"), Some(&Json::Null));
        assert_eq!(NEPTUN.addresses.handset_error_loop.to_json().get("address").and_then(Json::as_u64), Some(0x0800_598E));
        assert!(TRITON.cold_boot_refusal.is_none() && NEPTUN.cold_boot_refusal.is_some());
    }

    #[test]
    fn real_neptun_images_match_every_recorded_fact() {
        let (Some(main_bytes), Some(handset_bytes)) = (neptun(Role::Main), neptun(Role::Handset)) else {
            eprintln!("skipping: the NEPTUN SRECs are not available locally");
            return;
        };
        let main = load(&main_bytes, Some(Role::Main)).unwrap();
        let handset = load(&handset_bytes, Some(Role::Handset)).unwrap();
        assert_eq!((main.release.id, handset.release.id), ("NEPTUN-5.8-65.3", "NEPTUN-5.8-65.3"));
        assert_eq!((main.span.len(), handset.span.len()), (320_900, 707_092));
        assert_eq!((main.reset_pc(), handset.reset_pc()), (0x0803_90B8, 0x0800_8444));
        assert_eq!((main.initial_sp(), handset.initial_sp()), (0x2001_8000, 0x2001_8000));
        assert!(main.report.ok() && handset.report.ok(), "{:?} {:?}", main.report.failed_checks(), handset.report.failed_checks());
        assert_eq!((main.report.record_counts[3], handset.report.record_counts[3]), (20_059, 44_197));
        assert_eq!(sha256::to_hex(&main.bin_sha256), NEPTUN_MAIN.bin_sha256);
        assert_eq!(sha256::to_hex(&handset.bin_sha256), NEPTUN_HANDSET.bin_sha256);
        assert_eq!(&main.span[0x18C..0x190], &[0xFF; 4], "the same hole after the vector table as TRITON");
        // The EEPROM record table of the NEPTUN main image is byte for byte the TRITON one (one hash, two addresses).
        let table = crate::eeprom_init::record_table(&NEPTUN, &main).expect("the table is inside the NEPTUN image");
        assert_eq!(sha256::digest_hex(table), crate::eeprom_init::RECORD_TABLE_SHA256);
        // The identity is by file, not by role slot: a NEPTUN main in the handset slot says so.
        let error = load(&main_bytes, Some(Role::Handset)).unwrap_err();
        assert!(matches!(&error, FirmwareError::WrongRole { release, .. } if release.id == "NEPTUN-5.8-65.3"), "{error}");
        assert!(error.to_string().contains("NEPTUN-5.8-65.3"), "{error}");
        // A pair of the same release is accepted, a mixed one is refused with both releases named.
        assert_eq!(common_release(&main, &handset).map(|r| r.id), Ok("NEPTUN-5.8-65.3"));
        if let Some(triton) = read(Role::Handset) {
            let triton = load(&triton, None).unwrap();
            let message = common_release(&main, &triton).unwrap_err();
            assert!(message.contains("NEPTUN-5.8-65.3") && message.contains("TRITON-5.8-65.3") && message.contains("same release"), "{message}");
            assert!(common_release(&main, &main).is_err(), "two mains are not a pair");
        }
        // The unknown-image message lists both releases.
        let text = FirmwareError::Unknown { srec_sha256: "00".into() }.to_string();
        assert!(text.contains("TRITON-5.8-65.3") && text.contains("NEPTUN-5.8-65.3") && text.contains("ngc_main_5.8_NEPTUN.srec"), "{text}");
    }
}
