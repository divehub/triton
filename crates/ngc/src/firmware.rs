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
//!
//! # Custom (native) builds
//!
//! [`load_custom`] admits an SREC for an explicit role without release identification (DESIGN.md section 20): only
//! structure is validated (S-record syntax, address bounds, vector table, initial SP, Thumb reset vector, entry record),
//! and the image belongs to the pseudo-release [`CUSTOM`], whose every original-firmware address is unavailable. A session
//! takes two custom images or two images of one original release, never a mix.

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
    /// Handset RAM byte: the display orientation (2 swaps the Up/Down meaning of the two pins). Documentation of the image only:
    /// the engine no longer reads it, Up is `PE5` and Down is `PE3` for every firmware (DESIGN.md 20.3).
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
    /// ID): the layout the EEPROM factory image ([`crate::eeprom_init`]) writes by; it checks the table's SHA-256 against the image.
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

    /// The original release with this identifier ([`CUSTOM`] is not one: it is reached through [`load_custom`] only).
    pub fn by_id(id: &str) -> Option<&'static Release> {
        RELEASES.iter().copied().find(|release| release.id == id)
    }

    /// True for the pseudo-release of custom (native) builds.
    pub fn is_custom(&self) -> bool {
        self.id == CUSTOM_ID
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
        handset_orientation: AddressEntry::known(0x2000_0740, "TRITON static analysis (the runner's former Up/Down mask swap, no longer read); the reference for the NEPTUN mapping"),
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

/// Identifier and label of the pseudo-release of custom builds (`firmware.release` of the state document).
pub const CUSTOM_ID: &str = "CUSTOM";
pub const CUSTOM_LABEL: &str = "Custom build";

/// The reason every original-firmware address is unavailable for a custom build.
pub const CUSTOM_UNAVAILABLE: &str = "custom build: original firmware addresses do not apply";

/// Display names of the custom images. A custom image has no recorded facts: only `role`, `label` and `file_name` mean
/// anything (the file name is a placeholder, the engine never sees the real one); the hashes, the span and the vectors are
/// computed from the file that was supplied.
pub static CUSTOM_MAIN: Expected = Expected {
    role: Role::Main,
    label: "Custom main build",
    file_name: "custom-main.srec",
    srec_sha256: "",
    s3_records: 0,
    data_bytes: 0,
    span_start: APPLICATION_BASE,
    span_end: APPLICATION_BASE,
    bin_sha256: "",
    initial_sp: 0,
    reset_vector: 0,
};

pub static CUSTOM_HANDSET: Expected = Expected {
    role: Role::Handset,
    label: "Custom handset build",
    file_name: "custom-handset.srec",
    srec_sha256: "",
    s3_records: 0,
    data_bytes: 0,
    span_start: APPLICATION_BASE,
    span_end: APPLICATION_BASE,
    bin_sha256: "",
    initial_sp: 0,
    reset_vector: 0,
};

/// The pseudo-release of custom (native) builds, DESIGN.md section 20. It is not in [`RELEASES`] (nothing identifies a custom
/// image by hash) and every original-firmware address is unavailable, so the engine reads nothing of the original
/// application RAM: no terminal-handler stop, no battery/mode/screen variables, no decompression health, no EEPROM factory
/// image. Hardware-level behavior (the buttons, fault registers, standby detection from the sleeping core and the PWR mode)
/// does not depend on the release.
pub static CUSTOM: Release = Release {
    id: CUSTOM_ID,
    label: CUSTOM_LABEL,
    main: &CUSTOM_MAIN,
    handset: &CUSTOM_HANDSET,
    addresses: ReleaseAddresses {
        handset_orientation: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        handset_error_loop: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_battery_ready: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_wake_cause: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_screen_mode: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_mode: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_hal_tick: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_pressure: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_temperature: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_current_tcb: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        handset_current_tcb: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_deco_tissues: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_breathing_mode: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_ppo2: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        main_cell_flags: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        eeprom_tissue_block: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        eeprom_deco_date: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
        eeprom_record_table: AddressEntry::unavailable(CUSTOM_UNAVAILABLE),
    },
    // Standby is observed from the hardware state (a sleeping core with SLEEPDEEP and a PWR standby/shutdown mode), so the
    // cold-boot fixture has an observed-standby route for any build.
    cold_boot_refusal: None,
};

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
    if main.is_custom() != handset.is_custom() {
        let describe = |image: &Firmware| if image.is_custom() { "a custom build".to_string() } else { format!("{} ({})", image.release.id, image.release.expected(image.role).file_name) };
        return Err(format!(
            "Mixed firmware: the main image is {} but the handset image is {}. A custom build cannot be combined with an original image; supply two custom builds or two images of the same original release.",
            describe(main),
            describe(handset)
        ));
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
    /// The release of an identified image ([`CUSTOM`] for the structural report of a custom build).
    pub release: Option<&'static Release>,
    /// The report of [`inspect_custom`]: structure only, no identification. `ok` then needs no identified role.
    pub custom: bool,
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
    /// True when the file parsed, was identified (not needed for a custom report) and every check passed.
    pub fn ok(&self) -> bool {
        self.parse_error.is_none() && (self.custom || self.identified.is_some()) && self.checks.iter().all(|c| c.ok)
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
        // Only the report of a custom build carries the marker: the identification report keeps its exact shape.
        if self.custom {
            json.insert("custom", true);
        }
        json
    }

    /// The failed checks of a custom report as one sentence fragment (`name: detail; name: detail`), or `None` when it is ok.
    pub fn refusal(&self) -> Option<String> {
        if self.ok() {
            return None;
        }
        let failed: Vec<String> = self.failed_checks().iter().map(|c| format!("{}: {}", c.name, c.detail)).collect();
        Some(if failed.is_empty() { "the image was not accepted".to_string() } else { failed.join("; ") })
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
    /// A custom build that failed the structural validation of [`load_custom`] (`failed` lists every check that did not hold).
    Custom { role: Role, failed: Vec<Check> },
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
            FirmwareError::Custom { role, failed } => {
                write!(f, "custom {role} build rejected: ")?;
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
    /// The release this image belongs to (identified by the SREC or span hash); [`CUSTOM`] for a custom build.
    pub release: &'static Release,
    /// Address of the first span byte (`0x08004000`).
    pub span_base: u32,
    /// The binary span: lowest to highest data address, holes filled with `0xFF` (`0x00` for a custom build).
    pub span: Vec<u8>,
    pub srec_sha256: [u8; 32],
    pub bin_sha256: [u8; 32],
    pub vectors: VectorInfo,
    pub report: Report,
}

impl Firmware {
    /// True for a custom (native) build loaded through [`load_custom`].
    pub fn is_custom(&self) -> bool {
        self.release.is_custom()
    }

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
        custom: false,
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

// ---- custom (native) builds -------------------------------------------------------------------------------------------

/// End (exclusive) of the flash of the supported 1 MiB devices: a custom image must lie in
/// `APPLICATION_BASE..FLASH_END`.
pub const FLASH_END: u32 = 0x0810_0000;
/// SRAM1 of the supported devices: the initial stack pointer lies in `(SRAM_BASE, SRAM_TOP]`.
pub const SRAM_BASE: u32 = 0x2000_0000;
pub const SRAM_TOP: u32 = 0x2001_8000;
/// Largest SREC text that is considered at all (a full 1 MiB image in short records is a few MiB): bounds the work and the
/// allocation before anything is parsed.
pub const MAX_CUSTOM_SREC_BYTES: usize = 16 << 20;
/// The vector table words the report decodes at most (16 system exceptions + 83 interrupts).
const CUSTOM_VECTOR_ENTRIES: usize = VECTOR_ENTRIES;

/// The structural report of a custom build (`ngc_firmware_inspect_custom`, `ngc-cli info --custom`): the same document as
/// [`inspect`] with `release` always [`CUSTOM`], no role (the slot decides, both native builds share a reset vector) and
/// `custom: true`. Nothing is identified: the checks are the structure alone (see [`load_custom`]).
pub fn inspect_custom(srec_bytes: &[u8]) -> Report {
    inspect_custom_parsed(srec_bytes).0
}

fn inspect_custom_parsed(srec_bytes: &[u8]) -> (Report, Option<Vec<u8>>) {
    let mut report = Report {
        srec_bytes: srec_bytes.len(),
        srec_sha256: sha256::digest_hex(srec_bytes),
        identified: None,
        release: Some(&CUSTOM),
        custom: true,
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
    if srec_bytes.len() > MAX_CUSTOM_SREC_BYTES {
        let detail = format!("the file has {} bytes; at most {MAX_CUSTOM_SREC_BYTES} bytes of S-record text are accepted", srec_bytes.len());
        report.parse_error = Some(detail.clone());
        report.checks.push(check("srec-size", false, detail));
        return (report, None);
    }
    let srec = match srec::parse(srec_bytes) {
        Ok(srec) => srec,
        Err(e) => {
            report.parse_error = Some(e.to_string());
            report.checks.push(check("srec-syntax", false, e.to_string()));
            return (report, None);
        }
    };
    report.checks.push(check("srec-syntax", true, "all records parse; checksums, counts and overlaps are consistent".into()));
    report.header = srec.header_text();
    report.record_counts = srec.record_counts;
    report.segments = srec.segments.iter().map(|s| (s.start, s.end() as u32)).collect();
    report.entry = srec.start_address;

    // Address bounds first: the reconstruction below allocates the span, which the bounds limit to the application flash.
    let bounds = format!("0x{APPLICATION_BASE:08x}..0x{FLASH_END:08x}");
    let bounds_ok = match srec.segments.iter().find(|s| s.start < APPLICATION_BASE || s.end() > u64::from(FLASH_END)) {
        _ if srec.segments.is_empty() => {
            report.checks.push(check("address-bounds", false, "the file carries no data records".into()));
            false
        }
        Some(outside) => {
            report.checks.push(check(
                "address-bounds",
                false,
                format!("data at 0x{:08x}..0x{:08x} lies outside the application flash {bounds}", outside.start, outside.end()),
            ));
            false
        }
        None => {
            let plural = if srec.segments.len() == 1 { "" } else { "s" };
            report.checks.push(check("address-bounds", true, format!("{} data segment{plural} inside the application flash {bounds}", srec.segments.len())));
            true
        }
    };
    let mut binary = None;
    if bounds_ok {
        if let Some((base, bytes)) = srec.to_binary(0x00) {
            let gaps = srec.gaps();
            let gap_bytes: u64 = gaps.iter().map(|&(start, end)| u64::from(end - start)).sum();
            let end = base + bytes.len() as u32;
            report.checks.push(check(
                "span",
                true,
                format!(
                    "0x{base:08x}..0x{end:08x}, {} bytes; {} gap{} ({gap_bytes} bytes) filled with 0x00",
                    bytes.len(),
                    gaps.len(),
                    if gaps.len() == 1 { "" } else { "s" }
                ),
            ));
            report.span = Some((base, end));
            report.bin_sha256 = Some(sha256::digest_hex(&bytes));
            binary = Some(bytes);
        }
    }

    // The vector table: initial SP and reset vector, read from the loaded data at the application base.
    let first = srec.segments.first();
    let table = match first {
        Some(segment) if segment.start == APPLICATION_BASE && segment.data.len() >= 8 => Some(segment),
        Some(segment) if segment.start == APPLICATION_BASE => {
            report.checks.push(check(
                "vector-table",
                false,
                format!("only {} bytes are loaded at 0x{APPLICATION_BASE:08x}; the initial SP and the reset vector need 8", segment.data.len()),
            ));
            None
        }
        Some(segment) => {
            report.checks.push(check(
                "vector-table",
                false,
                format!("no data at 0x{APPLICATION_BASE:08x} (the lowest data address is 0x{:08x}); the vector table must start there", segment.start),
            ));
            None
        }
        None => {
            report.checks.push(check("vector-table", false, format!("no data at 0x{APPLICATION_BASE:08x}; the vector table must start there")));
            None
        }
    };
    let Some(table) = table else { return (report, binary) };
    let entries: Vec<u32> = (0..(table.data.len() / 4).min(CUSTOM_VECTOR_ENTRIES)).map(|i| word_at(&table.data, i)).collect();
    let (initial_sp, reset_vector) = (entries[0], entries[1]);
    report.checks.push(check(
        "vector-table",
        true,
        format!("{} vector words loaded at 0x{APPLICATION_BASE:08x}: initial SP 0x{initial_sp:08x}, reset vector 0x{reset_vector:08x}", entries.len()),
    ));
    report.vectors = Some(VectorInfo { initial_sp, reset_vector, entries });

    let mut sp_problems = Vec::new();
    if initial_sp % 8 != 0 {
        sp_problems.push("not 8-byte aligned".to_string());
    }
    if initial_sp <= SRAM_BASE || initial_sp > SRAM_TOP {
        sp_problems.push(format!("outside the SRAM1 range 0x{SRAM_BASE:08x} < SP <= 0x{SRAM_TOP:08x}"));
    }
    if sp_problems.is_empty() {
        report.checks.push(check("initial-sp", true, format!("0x{initial_sp:08x}: 8-byte aligned, inside 0x{SRAM_BASE:08x} < SP <= 0x{SRAM_TOP:08x}")));
    } else {
        report.checks.push(check("initial-sp", false, format!("initial SP 0x{initial_sp:08x} is {}", sp_problems.join(" and "))));
    }

    let reset_pc = reset_vector & !1;
    let segment_list = fmt_ranges(&report.segments);
    let in_loaded_flash = reset_pc >= APPLICATION_BASE && srec.segments.iter().any(|s| u64::from(reset_pc) >= u64::from(s.start) && u64::from(reset_pc) + 2 <= s.end());
    if reset_vector & 1 == 0 {
        report.checks.push(check(
            "reset-vector",
            false,
            format!("reset vector 0x{reset_vector:08x} has the Thumb bit clear; Cortex-M executes Thumb code only (expected 0x{:08x})", reset_vector | 1),
        ));
    } else if !in_loaded_flash {
        report.checks.push(check(
            "reset-vector",
            false,
            format!("reset vector 0x{reset_vector:08x} points to 0x{reset_pc:08x}, which is in no loaded data segment ({segment_list})"),
        ));
    } else {
        report.checks.push(check("reset-vector", true, format!("0x{reset_vector:08x}: Thumb entry 0x{reset_pc:08x} inside loaded flash")));
    }

    match (srec.start_address, srec.start_record_type) {
        (None, _) => report.checks.push(check("entry-point", true, "no start record (S7/S8/S9); the reset vector is the entry".into())),
        (Some(start), kind) => {
            let name = format!("S{}", kind.unwrap_or(7));
            if (start & !1) == reset_pc {
                let note = if start == reset_vector { "" } else { " (the Thumb bit is ignored in the comparison)" };
                report.checks.push(check("entry-point", true, format!("{name} start address 0x{start:08x} equals the reset vector 0x{reset_vector:08x}{note}")));
            } else {
                report.checks.push(check("entry-point", false, format!("{name} start address 0x{start:08x} differs from the reset vector 0x{reset_vector:08x}")));
            }
        }
    }
    (report, binary)
}

/// Admits an SREC as a **custom** (native) build for an explicit `role`, without release identification (DESIGN.md 20.1). The
/// slot decides the role: the content cannot, because both native builds share a reset vector.
///
/// Structural validation only, every violation reported with its address or value:
///
/// * S-record syntax, checksums, record counts and overlapping data (the parser's), and a file of at most
///   [`MAX_CUSTOM_SREC_BYTES`];
/// * `address-bounds`: at least one data record and every byte inside `0x08004000..0x08100000`, checked before the binary is
///   reconstructed so that the span (and the allocation) is bounded;
/// * `vector-table`: loaded data at `0x08004000`, at least the initial SP and the reset vector (8 bytes);
/// * `initial-sp`: 8-byte aligned and `0x20000000 < SP <= 0x20018000`;
/// * `reset-vector`: the Thumb bit is set and the entry lies in loaded data (not in a gap);
/// * `entry-point`: an S7/S8/S9 start address, when present, equals the reset vector (the Thumb bit is ignored).
///
/// The span runs from the lowest to the highest data address; bytes inside it that no record covers are `0x00` (the native
/// export materializes its gaps; the `0xFF` reconstruction of the original images is unchanged). The result belongs to
/// [`CUSTOM`]: every original-firmware address is unavailable.
pub fn load_custom(srec_bytes: &[u8], role: Role) -> Result<Firmware, FirmwareError> {
    let (report, binary) = inspect_custom_parsed(srec_bytes);
    if !report.ok() {
        let failed = report.failed_checks().into_iter().cloned().collect();
        return Err(FirmwareError::Custom { role, failed });
    }
    let span = binary.expect("an accepted custom image has a span");
    let vectors = report.vectors.clone().expect("an accepted custom image has a vector table");
    Ok(Firmware {
        role,
        release: &CUSTOM,
        span_base: APPLICATION_BASE,
        bin_sha256: sha256::digest(&span),
        srec_sha256: sha256::digest(srec_bytes),
        span,
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

    // ---- custom builds ----

    /// One S-record with a correct checksum.
    fn s_record(kind: u8, address_bytes: usize, address: u32, data: &[u8]) -> String {
        let mut body = vec![(address_bytes + data.len() + 1) as u8];
        for i in (0..address_bytes).rev() {
            body.push((address >> (8 * i)) as u8);
        }
        body.extend_from_slice(data);
        let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        body.push(!sum);
        format!("S{kind}{}", body.iter().map(|b| format!("{b:02X}")).collect::<String>())
    }

    /// A synthetic SREC file: S0, S3 records of at most 16 bytes per segment, an optional termination record `(kind, address)`.
    fn srec_text(segments: &[(u32, Vec<u8>)], entry: Option<(u8, u32)>) -> String {
        let mut lines = vec![s_record(0, 2, 0, b"synthetic.srec")];
        for (address, data) in segments {
            for (i, chunk) in data.chunks(16).enumerate() {
                lines.push(s_record(3, 4, address + 16 * i as u32, chunk));
            }
        }
        if let Some((kind, address)) = entry {
            lines.push(s_record(kind, match kind {
                7 => 4,
                8 => 3,
                _ => 2,
            }, address, &[]));
        }
        lines.join("\r\n") + "\r\n"
    }

    /// 64 vector words: the initial SP, the reset vector and 62 copies of the reset vector.
    fn vectors(sp: u32, reset: u32) -> Vec<u8> {
        let mut words = vec![sp, reset];
        words.extend(std::iter::repeat(reset).take(62));
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// A valid synthetic image: the table, code right behind it (one merged segment `0x08004000..0x08004120`) and 16 bytes of data
    /// behind a gap of `0xE0` bytes at `0x08004200`.
    fn good_segments() -> Vec<(u32, Vec<u8>)> {
        vec![(0x0800_4000, vectors(0x2001_8000, 0x0800_4101)), (0x0800_4100, vec![0x70; 32]), (0x0800_4200, vec![0xAB; 16])]
    }

    fn custom_error(text: &str) -> String {
        load_custom(text.as_bytes(), Role::Main).expect_err("must be refused").to_string()
    }

    #[test]
    fn a_structurally_valid_image_is_admitted_as_a_custom_build_whatever_its_hash() {
        let text = srec_text(&good_segments(), Some((7, 0x0800_4101)));
        let image = load_custom(text.as_bytes(), Role::Handset).expect("valid");
        assert_eq!((image.role, image.release.id, image.is_custom()), (Role::Handset, "CUSTOM", true));
        assert_eq!((image.span_base, image.vtor(), image.initial_sp(), image.reset_pc()), (0x0800_4000, 0x0800_4000, 0x2001_8000, 0x0800_4100));
        // The span runs to the last data byte; the bytes no record covers are 0x00 (and the hole of the original images stays 0xFF).
        assert_eq!(image.span.len(), 0x210);
        assert!(image.span[0x120..0x200].iter().all(|&b| b == 0), "the gap is zero filled");
        assert!(image.span[0x200..].iter().all(|&b| b == 0xAB) && image.span[0x100..0x120].iter().all(|&b| b == 0x70));
        assert_eq!(image.bin_sha256, sha256::digest(&image.span));
        assert_eq!(image.srec_sha256, sha256::digest(text.as_bytes()));
        assert_eq!(image.vectors.entries.len(), 72, "the words of the first loaded segment (0x120 bytes), at most 99");
        assert_eq!(image.flash_image().len(), 0x10_0000);
        // The report: structure only, no identity, every check ok.
        let report = &image.report;
        assert!(report.ok() && report.custom && report.identified.is_none());
        assert_eq!(report.checks.iter().map(|c| c.name).collect::<Vec<_>>(), ["srec-syntax", "address-bounds", "span", "vector-table", "initial-sp", "reset-vector", "entry-point"]);
        assert_eq!(report.segments, [(0x0800_4000, 0x0800_4120), (0x0800_4200, 0x0800_4210)]);
        assert!(report.checks[2].detail.contains("1 gap (224 bytes) filled with 0x00"), "{}", report.checks[2].detail);
        let json = report.to_json();
        assert_eq!(json.get("ok").and_then(Json::as_bool), Some(true));
        assert_eq!(json.get("custom").and_then(Json::as_bool), Some(true));
        assert!(json.get("role").unwrap().is_null());
        assert_eq!(json.get("release").unwrap().get("id").and_then(Json::as_str), Some("CUSTOM"));
        assert_eq!(json.get("release").unwrap().get("label").and_then(Json::as_str), Some("Custom build"));
        assert_eq!(json.get("binSha256").and_then(Json::as_str), Some(sha256::to_hex(&image.bin_sha256).as_str()));
        assert_eq!(json.get("srecSha256").and_then(Json::as_str), Some(sha256::to_hex(&image.srec_sha256).as_str()));
        assert_eq!((json.get("initialSp").and_then(Json::as_u64), json.get("resetPc").and_then(Json::as_u64), json.get("entry").and_then(Json::as_u64)), (Some(0x2001_8000), Some(0x0800_4100), Some(0x0800_4101)));
        assert_eq!(json.get("span").unwrap().get("end").and_then(Json::as_u64), Some(0x0800_4210));
        // The same bytes in the other slot: only the role differs (both native builds share a reset vector).
        let main = load_custom(text.as_bytes(), Role::Main).unwrap();
        assert_eq!((main.role, main.span.clone(), main.bin_sha256), (Role::Main, image.span.clone(), image.bin_sha256));
        // The original path knows nothing of it: no identity, no release.
        assert!(matches!(load(text.as_bytes(), None), Err(FirmwareError::Unknown { .. })));
        assert_eq!(identify(text.as_bytes()), None);
        assert!(!inspect(text.as_bytes()).ok() && inspect(text.as_bytes()).release.is_none() && !inspect(text.as_bytes()).custom);
        assert!(inspect(text.as_bytes()).to_json().get("custom").is_none(), "the identification report keeps its exact shape");
    }

    #[test]
    fn the_custom_release_has_every_original_address_unavailable_and_is_not_an_original_release() {
        assert!(CUSTOM.is_custom() && !TRITON.is_custom() && !NEPTUN.is_custom());
        assert!(RELEASES.iter().all(|r| r.id != CUSTOM.id) && Release::by_id("CUSTOM").is_none());
        let entries = CUSTOM.addresses.entries();
        assert_eq!(entries.len(), 18);
        for (name, entry) in entries {
            assert_eq!(entry.address(), None, "{name}");
            assert_eq!(entry.reason(), Some("custom build: original firmware addresses do not apply"), "{name}");
            assert_eq!(entry.to_json().get("address"), Some(&Json::Null));
        }
        assert!(CUSTOM.cold_boot_refusal.is_none(), "standby is observed from the hardware state");
        assert_eq!(CUSTOM.to_json().get("label").and_then(Json::as_str), Some("Custom build"));
        assert_eq!((CUSTOM.expected(Role::Main).role, CUSTOM.expected(Role::Handset).role), (Role::Main, Role::Handset));
    }

    #[test]
    fn data_outside_the_application_flash_or_no_data_is_refused_before_the_binary_is_built() {
        // Below the application base (the bootloader area), and straddling the end of the 1 MiB flash.
        let below = srec_text(&[(0x0800_0000, vectors(0x2001_8000, 0x0800_0101))], None);
        assert_eq!(
            custom_error(&below),
            "custom main build rejected: address-bounds: data at 0x08000000..0x08000100 lies outside the application flash 0x08004000..0x08100000; \
             vector-table: no data at 0x08004000 (the lowest data address is 0x08000000); the vector table must start there"
        );
        let mut segments = good_segments();
        segments.push((0x080F_FFF8, vec![1; 16]));
        let above = srec_text(&segments, None);
        assert!(custom_error(&above).contains("address-bounds: data at 0x080ffff8..0x08100008 lies outside the application flash"), "{}", custom_error(&above));
        // The last byte of flash is fine.
        segments.pop();
        segments.push((0x080F_FFF0, vec![1; 16]));
        let report = inspect_custom(srec_text(&segments, None).as_bytes());
        assert!(report.ok(), "{:?}", report.failed_checks());
        assert_eq!(report.span, Some((0x0800_4000, 0x0810_0000)), "a span of just under 1 MiB is built");
        // A file without data (a header and a start record) and RAM / other-bank addresses.
        let empty = [s_record(0, 2, 0, b"x"), s_record(7, 4, 0x0800_4001, &[])].join("\n");
        assert_eq!(custom_error(&empty), "custom main build rejected: address-bounds: the file carries no data records; vector-table: no data at 0x08004000; the vector table must start there");
        let ram = srec_text(&[(0x2000_0000, vec![0; 16])], None);
        assert!(custom_error(&ram).contains("0x20000000..0x20000010 lies outside"), "{}", custom_error(&ram));
        let report = inspect_custom(ram.as_bytes());
        assert!(report.span.is_none() && report.bin_sha256.is_none(), "no reconstruction after a bounds failure");
    }

    #[test]
    fn the_vector_table_must_start_at_the_application_base_with_the_sp_and_the_reset_vector() {
        let late = srec_text(&[(0x0800_4100, vectors(0x2001_8000, 0x0800_4101))], None);
        assert_eq!(custom_error(&late), "custom main build rejected: vector-table: no data at 0x08004000 (the lowest data address is 0x08004100); the vector table must start there");
        let short = srec_text(&[(0x0800_4000, vec![0, 0, 1, 0x20]), (0x0800_4100, vec![0; 16])], None);
        assert_eq!(custom_error(&short), "custom main build rejected: vector-table: only 4 bytes are loaded at 0x08004000; the initial SP and the reset vector need 8");
        // Eight bytes are enough; the table is as long as the data that was loaded.
        let minimal = srec_text(&[(0x0800_4000, [0x2001_8000u32.to_le_bytes(), 0x0800_4009u32.to_le_bytes()].concat()), (0x0800_4008, vec![0x70; 8])], None);
        let image = load_custom(minimal.as_bytes(), Role::Main).unwrap();
        assert_eq!((image.vectors.entries.len(), image.reset_pc()), (4, 0x0800_4008));
    }

    #[test]
    fn the_initial_sp_must_be_aligned_and_inside_sram1() {
        let sp_error = |sp: u32| custom_error(&srec_text(&good_segments_with(sp, 0x0800_4101), None));
        assert_eq!(sp_error(0x2001_7FFC), "custom main build rejected: initial-sp: initial SP 0x20017ffc is not 8-byte aligned");
        assert_eq!(sp_error(0x2001_8008), "custom main build rejected: initial-sp: initial SP 0x20018008 is outside the SRAM1 range 0x20000000 < SP <= 0x20018000");
        assert_eq!(sp_error(0x2000_0000), "custom main build rejected: initial-sp: initial SP 0x20000000 is outside the SRAM1 range 0x20000000 < SP <= 0x20018000");
        assert_eq!(sp_error(0), "custom main build rejected: initial-sp: initial SP 0x00000000 is outside the SRAM1 range 0x20000000 < SP <= 0x20018000");
        assert!(sp_error(0x2001_8004).contains("is not 8-byte aligned and outside the SRAM1 range"), "both problems are named");
        // The bounds themselves, and the CCM-free low end, are accepted.
        for sp in [0x2001_8000u32, 0x2000_0008, 0x2000_8000] {
            assert!(load_custom(srec_text(&good_segments_with(sp, 0x0800_4101), None).as_bytes(), Role::Main).is_ok(), "{sp:#x}");
        }
    }

    #[test]
    fn the_reset_vector_must_be_thumb_code_inside_loaded_flash() {
        let reset_error = |reset: u32| custom_error(&srec_text(&good_segments_with(0x2001_8000, reset), None));
        assert_eq!(reset_error(0x0800_4100), "custom main build rejected: reset-vector: reset vector 0x08004100 has the Thumb bit clear; Cortex-M executes Thumb code only (expected 0x08004101)");
        // Into the gap (zero filled in the span, but not loaded), beyond the data, and below the application base.
        let gap = reset_error(0x0800_4301);
        assert!(gap.contains("reset vector 0x08004301 points to 0x08004300, which is in no loaded data segment (0x08004000..0x08004120 + 0x08004200..0x08004210)"), "{gap}");
        assert!(reset_error(0x0800_5001).contains("points to 0x08005000, which is in no loaded data segment"));
        assert!(reset_error(0x0800_3001).contains("points to 0x08003000, which is in no loaded data segment"));
        assert!(reset_error(0x0000_0001).contains("in no loaded data segment"));
        // The last halfword of a segment is a valid entry (a Thumb instruction is 2 bytes), the first byte behind it is not; the
        // vector table itself is loaded flash too.
        let ok = |reset: u32| load_custom(srec_text(&good_segments_with(0x2001_8000, reset), None).as_bytes(), Role::Main).is_ok();
        assert!(ok(0x0800_411F) && !ok(0x0800_4121) && ok(0x0800_420F) && !ok(0x0800_4211) && ok(0x0800_4001));
    }

    /// `good_segments` with another initial SP and reset vector in the table.
    fn good_segments_with(sp: u32, reset: u32) -> Vec<(u32, Vec<u8>)> {
        let mut segments = good_segments();
        segments[0].1 = vectors(sp, reset);
        segments
    }

    #[test]
    fn a_start_record_must_equal_the_reset_vector() {
        let with_entry = |entry: Option<(u8, u32)>| srec_text(&good_segments(), entry);
        // Absent: the reset vector is the entry. Equal: ok; equal without the Thumb bit: ok and said so.
        assert!(load_custom(with_entry(None).as_bytes(), Role::Main).unwrap().report.checks.last().unwrap().detail.contains("no start record"));
        let exact = load_custom(with_entry(Some((7, 0x0800_4101))).as_bytes(), Role::Main).unwrap();
        assert_eq!(exact.report.checks.last().unwrap().detail, "S7 start address 0x08004101 equals the reset vector 0x08004101");
        let cleared = load_custom(with_entry(Some((7, 0x0800_4100))).as_bytes(), Role::Main).unwrap();
        assert!(cleared.report.checks.last().unwrap().detail.contains("(the Thumb bit is ignored in the comparison)"));
        // Different: refused, whatever the record type.
        assert_eq!(
            custom_error(&with_entry(Some((7, 0x0800_4201)))),
            "custom main build rejected: entry-point: S7 start address 0x08004201 differs from the reset vector 0x08004101"
        );
        assert!(custom_error(&with_entry(Some((8, 0x0004_1011)))).contains("S8 start address 0x00041011 differs"));
        let s9 = custom_error(&with_entry(Some((9, 0x4101))));
        assert!(s9.contains("S9 start address 0x00004101 differs from the reset vector 0x08004101"), "{s9}");
    }

    #[test]
    fn syntax_checksum_count_and_overlap_errors_name_the_line_and_the_address() {
        let good = srec_text(&good_segments(), Some((7, 0x0800_4101)));
        // A broken checksum (flip the last digit of the second record).
        let mut lines: Vec<String> = good.lines().map(str::to_string).collect();
        let last = lines[1].len() - 1;
        let digit = if lines[1].ends_with('0') { '1' } else { '0' };
        lines[1].replace_range(last.., &digit.to_string());
        let error = custom_error(&lines.join("\n"));
        assert!(error.starts_with("custom main build rejected: srec-syntax: line 2: checksum mismatch (expected 0x"), "{error}");
        // Overlapping data records.
        let overlap = srec_text(&[(0x0800_4000, vectors(0x2001_8000, 0x0800_4101)), (0x0800_4080, vec![0x70; 32])], None);
        assert_eq!(custom_error(&overlap), "custom main build rejected: srec-syntax: data records overlap at 0x08004080");
        // A wrong record count, garbage, an empty file.
        let counted = [s_record(0, 2, 0, b"x"), s_record(3, 4, 0x0800_4000, &[0; 8]), s_record(5, 2, 7, &[])].join("\n");
        assert!(custom_error(&counted).contains("record count 7 does not match the 1 data records seen"), "{}", custom_error(&counted));
        assert!(custom_error("not an srec").contains("srec-syntax: line 1: not an S-record"));
        assert!(custom_error("").contains("address-bounds: the file carries no data records"));
        // S1 and S2 records are parsed but their 16/24-bit addresses are below the application flash.
        let s1 = [s_record(0, 2, 0, b"x"), s_record(1, 2, 0x4000, &[0; 8]), s_record(9, 2, 0, &[])].join("\n");
        assert!(custom_error(&s1).contains("address-bounds: data at 0x00004000..0x00004008 lies outside"));
        // The text is bounded before it is parsed.
        let huge = vec![b'\n'; MAX_CUSTOM_SREC_BYTES + 1];
        let error = load_custom(&huge, Role::Handset).expect_err("too large").to_string();
        assert!(error.starts_with("custom handset build rejected: srec-size: the file has 16777217 bytes"), "{error}");
        let report = inspect_custom(&huge);
        assert!(!report.ok() && report.parse_error.is_some() && report.segments.is_empty());
        assert!(report.to_json().get("error").and_then(Json::as_str).is_some());
    }

    #[test]
    fn a_custom_and_an_original_image_never_make_a_pair() {
        let text = srec_text(&good_segments(), Some((7, 0x0800_4101)));
        let (custom_main, custom_handset) = (load_custom(text.as_bytes(), Role::Main).unwrap(), load_custom(text.as_bytes(), Role::Handset).unwrap());
        assert_eq!(common_release(&custom_main, &custom_handset).map(|r| r.id), Ok("CUSTOM"));
        // A swapped slot is named like an original one.
        assert_eq!(common_release(&custom_handset, &custom_handset).unwrap_err(), "the main slot holds the handset image of CUSTOM");
        let Some(bytes) = read(Role::Handset) else { return };
        let original = load(&bytes, Some(Role::Handset)).unwrap();
        let error = common_release(&custom_main, &original).unwrap_err();
        assert_eq!(
            error,
            "Mixed firmware: the main image is a custom build but the handset image is TRITON-5.8-65.3 (ngc_handset_65.3_TRITON.srec). A custom build cannot be combined with an original image; supply two custom builds or two images of the same original release."
        );
        let original_main = load(&require!(Role::Main), Some(Role::Main)).unwrap();
        let error = common_release(&original_main, &custom_handset).unwrap_err();
        assert!(error.starts_with("Mixed firmware: the main image is TRITON-5.8-65.3 (ngc_main_5.8_TRITON.srec) but the handset image is a custom build."), "{error}");
    }

    #[test]
    fn the_original_images_are_valid_custom_images_with_a_zero_filled_hole_and_still_identify_as_before() {
        for role in [Role::Main, Role::Handset] {
            let bytes = require!(role);
            let original = load(&bytes, Some(role)).unwrap();
            let custom = load_custom(&bytes, role).expect("structurally valid");
            assert!(custom.is_custom() && !original.is_custom());
            assert_eq!((custom.initial_sp(), custom.reset_pc(), custom.span.len()), (original.initial_sp(), original.reset_pc(), original.span.len()));
            // They differ in the four bytes of the hole behind the vector table, 0xFF against 0x00, and so do the binary hashes.
            let differing: Vec<usize> = (0..original.span.len()).filter(|&i| original.span[i] != custom.span[i]).collect();
            assert_eq!(differing, [0x18C, 0x18D, 0x18E, 0x18F]);
            assert_ne!(custom.bin_sha256, original.bin_sha256);
            assert_eq!(custom.srec_sha256, original.srec_sha256);
            assert_eq!(custom.report.checks.iter().map(|c| c.name).collect::<Vec<_>>(), ["srec-syntax", "address-bounds", "span", "vector-table", "initial-sp", "reset-vector", "entry-point"]);
            assert_eq!(custom.vectors.entries.len(), 99);
            // The original path is untouched by the existence of the other one.
            assert_eq!(identify(&bytes), Some(role));
            let report = inspect(&bytes);
            assert!(report.ok() && !report.custom && report.release.map(|r| r.id) == Some("TRITON-5.8-65.3"));
        }
    }
}
