# EEPROM records a first boot leaves erased, and the factory image

A fresh emulator profile would start with a fully erased main EEPROM: 2048 bytes of `0xFF`. The original firmware's first-boot default
routine (`0x08009fea`, gated on the validity marker `0xa3` at offset 254, logical record `0x63`) fills most records, but not all. An
erased record reads back as `0xFF` bytes: NaN for a float, 65535 or 4294967295 for an integer. For a handful of records the firmware
computes with that value and the result is visible: the handset prints the delta vital capacity as `?a?%`, the surface information
page shows a no-fly time of 80 515, the System info page shows the serial number as `-00000001`, and after a restart the tissues load as
NaN. The **factory image** (DESIGN.md section 18) gives these records the value the firmware's own code implies.

**When it applies.** Once, when a *new* EEPROM is created: the profile has no `eeprom.bin`, or the stored image is entirely erased (all
2048 bytes `0xFF`). The engine then writes the factory image, which is 2048 bytes of `0xFF` with the eight records below written into
it, before the board loads it, and the image is saved with the profile. **An existing ("dirty") EEPROM is never touched**, even when
some of the records below are still erased in it; a restart, a cold boot, a wake, a serial change and a reopened profile all find the
saved image and leave it alone. There is no option: no page control, no session-create key, no CLI flag. The state says whether this
session created its EEPROM from the image: `eepromFactoryInit: {applied, reason}`. (An internal `SessionConfig` field switches it off
for the Renode-recorded scenarios and the dive benchmark, which compare an erased first boot byte for byte; users cannot reach it. The
web benchmark tool uses the hook `blankEeprom` for the same reason.)

Everything here is an **emulator fixture with firmware-derived values, not the manufacturer's factory image** (that image is not known),
established on this engine by static reading of the unchanged images and by synthetic runs. Nothing is a physical-device observation.
Evidence labels: **static** (read from the instructions of the original image), **dynamic** (observed in a run of the engine on the
unchanged firmware), **fixture** (a choice of this emulator, named as such).

## What is filled

Eight inventoried records (tissues counted as one block). Logical IDs and physical ranges come from the main image's record table
(see [Method](#method)). Addresses are original code addresses of the TRITON main 5.8 image.

| Logical ID | Physical | Size and type | Factory value | Read by | An erased value causes | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| `0x01` serial number | `0x000..0x003` | u32 | **1** | loader `0x0800a2c8`, getter `0x0800a324` | 4294967295; the handset's System info page shows `SN # -00000001` | **fixture**: synthetic storage (never a physical unit's number); 1 is the page's default and inside the nine-digit range |
| `0x2b` toxicity model | `0x0af` | u8 | **0** (OTU/UPTD) | settings loader `0x08009814`, getter `0x0801acc0` via `0x08009c34` index 5 | the main sends no CAN `0x225` (producers `0x08013c1a`, `0x080159dc`) and the handset keeps placeholder values (`CNS:005%`, `OTU:00123` on the surface page) | **static**: the CAN setter `0x0800afa0` maps every value but 1 to 0; the default routine writes 0 to the five sibling settings (IDs `0x26..0x2a`, calls `0x0800a134..0x0800a154`) and skips index 5; the handset's fallback is OTU/UPTD |
| `0x67` dose base (x squared) | `0x0b8..0x0bb` | float32 | **0.0** | loader `0x0800a500`; consumer `0x0801a948` | NaN: the delta vital capacity is NaN (CAN `0x226` is `FFFFFFFF`, handset text `nan`, LCD `ΔvC?a?%`) | **static**: its only writer `0x0800a618` is called by the save routine `0x0801ab7c` with x squared, which is 0 from a fresh state; **dynamic**: with 0.0 the value is finite and rises |
| `0x68` ESOT minutes | `0x0bc..0x0bd` | u16 | **0** | loader `0x0800a500`; writer `0x0800a660` (from `0x0801a79c`) | 65535 minutes, decayed only slowly by `0x0801a948`; becomes the live ESOT value on entering dive mode (65 402 min seen) | **static**: the save routine stores the live ESOT value, 0 from a fresh state |
| `0x69` last ppO2 x 100 | `0x0be` | u8 | **110** (1.10 bar) | loader `0x0800a500` (divided by 100); writer `0x0800a688` | 255 (2.55 bar): the fastest recovery rate of the decay in `0x0801a948` | **static**: the save routine stores max(average ppO2, 1.1) x 100 (compare helper `0x0801ec70` is "a < b") and the recovery routine floors the same value at 1.1 |
| `0x6a..0x89` tissue block | `0x0ff..0x17e` | 32 x float32, (N2, He) per tissue | **16 x (N2 0.750737 = `0x3f40304d`, He 0.0)** | loader `0x08007fe8`; saved by `0x080081b4` only on power-down | 32 NaN words load at the next start and the no-decompression limit stays at 99 | **dynamic**: the values the firmware's own reset (`0x08007584`, run by the initializer `0x08008308`) leaves in RAM at a first boot (TRITON; NEPTUN's own reset and save write the same words); the order N2 then He per record is checked by a test |
| `0x8b` no-fly time | `0x183..0x186` | u32, seconds | **0** | loader `0x08008a38`; saved by `0x08008b18` case 3 | 4294967295 s minus the elapsed time (about 2.9 billion s): `NoFlyTime:80515` on the surface page (CAN `0x229`) | **static**: the firmware's own clear path stores 0 (`0x08008b18` case 4 at `0x08008bc8`, called at start of the ADC task `0x0801f8c2`; the expiry branch of `0x08008a38`) |
| `0x8d` date next to the no-fly time | `0x190..0x193` | u32, packed calendar | **0** | `0x08008a38` (into RAM `0x20002374`), `0x08008b18` (written by case 2 at `0x08008ba6`), the getters `0x08008c10` and `0x08008c4c` | an invalid calendar (month 15, day 31) where the getters expect a date or 0 (`cbz`); no visible difference was seen (CAN `0x235` is 0 either way) | **static**: the firmware's own migration from marker `0xa2` to `0xa3` writes 0 (`0x08009fc6..0x08009fd2`) and its getters read 0 as "no date"; the fresh-profile routine omits the record |

The tissue values are 0.79 x (1.013 - 0.0627) bar. The firmware latches 1.013 bar as its surface pressure for the engine's 1013.25 mbar
sensor reading (it reads 1013.2 mbar), so the block is the surface equilibrium at the standard atmosphere, whatever surface-pressure
setting the page uses later.

The firmware's own save routine writes more than this fixture reads from it: records `0x64..0x66` (written 0 by the firmware itself at the
first boot, through the reset path of `0x0801a948`) are left to it.

## What is left erased, and why

The image fills only what it can justify. These records stay entirely `0xFF` after a first boot from the factory image (a test asserts
that this list is exactly the erased set).

| Logical ID | Physical | Size | Why it stays erased |
| --- | --- | --- | --- |
| `0x24`, `0x25` | `0x031`, `0x032` | u8 each | The battery types of B1 and B2. `0xFF` means "not chosen": the default routine writes `0xFF` itself (`0x08009bf8` at `0x0800a120..0x0800a12c`, which a cold boot also does), the voltage task tests for `0xFF` (`0x0801fd88`), and the handset's battery wizard writes the choice. Filling them would skip the wizard. |
| `0x0c` | `0x016` | u8 | Index 4 of a five-entry setting family (IDs `0x08..0x0c`, setter `0x08009584`); the default routine writes indices 0 to 3 and skips 4. Every consumer tests `== 1` and takes the other branch otherwise (`0x08013f10`, `0x08015818`, `0x08019d8c`, `0x0801a1d0`), so `0xFF` behaves like 0; the raw byte is forwarded to the handset (CAN `0x126`, the settings report `0x08015208`). No firmware default is known. |
| `0x13` | `0x097` | u8 | Index 4 of a five-entry family (IDs `0x10, 0x8c, 0x11, 0x12, 0x13`, setter `0x08009680`); the default routine skips it. The main only forwards it (CAN `0x7b` from `0x0801314c`) and stores what the handset sends back (`0x080131c8`). No default is known. |
| `0x4d` | `0x090..0x091` | u16 | Calibration set (IDs `0x3e..0x4d`, loader `0x08008f94`). The setter's index 9 branch loads record `0x4b` instead of `0x4d` (`0x080091f2` jumps to `0x080091ee`, a defect), so no code path can ever write it. Inert: the validity routine `0x08005664` returns 0 whenever record `0x4b` is 0, whatever `0x4d` holds. Calibration bytes are never part of the image. |
| `0x50`, `0x54..0x57` | `0x188`, `0x189..0x18c` | u8 each | Written by CAN-driven setters (`0x0800afc4`, `0x0800b0e0`, `0x0800b108`, `0x0800b130`, `0x0800b14c`) but **never read back**: no code of the image loads these IDs. Dead data in this firmware. (`0x4e`, `0x4f`, `0x51..0x53` are never read either; the default routine fills them.) |
| `0x5d` | `0x18f` | u8 | Loaded by `0x08009d84` into RAM `0x20002455` and overwritten with 0 right after (`0x08009df4`); nothing writes it. |
| `0x5f` | `0x09d` | u8 | The default routine calls its setter with 0 (`0x0800a2b0`), but the setter skips the write when RAM already equals the value (RAM is 0), so the first boot's RAM holds 0 and later boots load `0xFF`. The only consumers forward the byte to the handset (CAN `0x183`). Inert in the main; handset effect not analyzed. |
| `0x60`, `0x61` | `0x09e`, `0x09f` | u8 each | Fixed-setpoint ppO2 values for breathing mode 3 (`0x08008154`); the default mode is 2. The setter `0x08009f14` accepts only 90 to 160. No routine gives them a default, so `0xFF` (2.55 bar in mode 3) is shown to the handset (CAN `0x204`) until the user sets them. No firmware-implied value exists to use. |

Bytes outside every record (physical `0x09b`, `0x0bf..0x0fd`, `0x194..0x7ff`: 1708 bytes) have no logical ID and stay erased.

The records the firmware populates later in normal use are not erased for long and are not part of the inventory: the battery types
(`0x24`, `0x25`, wizard), the oxygen calibration (`0x2c..0x34`), the calibration age and counters (`0x39`, `0x3a`, `0x3b`), the calendar
backup (`0x23`), the dates `0x62` and `0x8a`, the version records (`0x02..0x05`), and at power-down the tissue block and `0x8b`.

The calendar backup (`0x23`, physical `0x2d`, valid with the marker `0xa3` at physical 254) is the one record related to the clock of a new
profile (DESIGN.md section 23). The factory image leaves it erased, and the marker is not written until the firmware's first-boot defaults
run, so at the first board creation of a new profile the legacy EEPROM date seed (`eeprom_seed`) finds nothing and both calendars start at
the RTC's 2020-01-01 default. Where an existing EEPROM does hold the marker and a real packed date but there is no RTC checkpoint, that seed
still sets the main board's calendar (original releases only, DESIGN.md section 20.1); the handset starts at the default.

## Method

1. **Layout.** The main image holds one table entry (offset u16, size u16) per logical ID at `0x080306f2 + 4 x ID`, IDs 0 to `0x8d` (568
   bytes). The wrappers `0x08010344` (read) and `0x08010430` (write) check the ID, the pointer and the size against it, and are the only code
   that loads it (with the table walker `0x08010494`). Both releases carry the same table: SHA-256
   `a69c0b84bdffa90dee14b26daa4eb35c7578b0923247a4ffe9b2c98fef244672`, at `0x080306f2` (TRITON) and `0x08050e38` (NEPTUN).
2. **What a first boot leaves erased.** A fresh profile was booted without the image (the internal switch) and the 2048 bytes mapped to
   records: 53 of the 141 records stay entirely erased after ten virtual seconds (the 21 above plus the 32 tissue words). The same boot
   from the factory image differs from it in exactly the inventoried records, byte for byte: every byte the firmware writes (its defaults
   and its dates) is the same.
3. **Readers and writers.** Every call of the two wrappers in the image was classified by record ID (immediate `movs r0, #ID` before a call
   or tail call); IDs `0x4e..0x57` have writers only, `0x4d` and `0x5d` have no effective writer. Consumers were followed from the getters.
4. **Effects.** First-use flows on a fresh profile (Li-Ion battery wizard, air calibration through the firmware's CAN protocol, a 20 m dive,
   the dive pages, a restart, the surface information page) with each candidate record filled alone and together (a study of the
   earlier fill-the-erased-records fixture, which this image replaced), comparing RAM, CAN frames and LCD frames.
5. **Original bytes.** The consequential branches were re-read with the engine's own decoder on the unchanged image (`ngc-cli disasm`):
   the default routine and the migration (`0x08009f98`), the setter defect at `0x080091f2`, the no-fly cases (jump table of `0x08008b18`).
   Ghidra names and C-like output were treated as reconstruction.

## The default routine still runs unchanged

The image never writes the validity marker (offset 254) and no inventoried record is read by the default routine as an "initialized"
flag: the routine reads only the marker (at `0x08009ffc`) and, through its loaders (`0x0800a2c8`, `0x08009814`), serial and settings records
whose RAM it then overwrites with its own setters. Of the filled records only the serial (loaded, then set to 0 in RAM by `0x0800a330`,
which writes the old EEPROM value back, so the stored 1 stays) and `0x2b` (never set by the routine) are loaded by it. A test compares a
first boot from the image with a blank one: every byte the firmware writes is identical.

With the serial pre-filled, the firmware's own cached serial is 0 on the first boot and 1 from the next start on (the setter's old-value
write-back, a firmware behavior); the state's `serialNumber` reads the EEPROM.

## Existing profiles are not repaired

The image applies only when the EEPROM is created. A profile that an older build saved keeps what it holds: if its tissue block was
never written (the firmware saves the tissues only at power-down, so a profile that was booted and restarted holds a decompression date
but 32 erased tissue words), the firmware loads NaN tissues and keeps them, and the no-decompression limit stays at 99. The engine does
not repair that (an earlier `decoStorageFixture` that erased the saved date record was removed); the read-only `decoHealth` report says
`tissues: invalid`, and the page's warning names the next step: reset the profile (Advanced, Profile and evidence, Reset profile),
which creates a new, correctly initialized EEPROM. The order at a board creation is now: the factory image for a new EEPROM, then the
start at the surface on the sensor inputs.

## Releases

* **TRITON-5.8-65.3**: all of the above.
* **NEPTUN-5.8-65.3**: its main image is a different build of the application, but it holds the same 568-byte record table (one exact
  match, three literal-pool references from its accessor code, `0x0801ad40`, `0x0801adec`, `0x0801b124`). The engine checks the table's
  hash against the loaded image before it writes, and applies the image. Beyond the table, on NEPTUN's own first boot the same records
  stay erased (the only difference is record `0x53`, written on TRITON and not on NEPTUN, which is not in the inventory), its own reset
  and power-down save write the same tissue words (`0x3f40304d`, He 0), the no-fly record becomes about 2.9 billion seconds after the
  save as on TRITON, the System info page shows `SN # -00000001`, and a dive reproduces the NaN delta vital capacity and, with the
  image, the same finite value as TRITON. NEPTUN's decompression RAM addresses stay unavailable, so its checks run through the EEPROM
  and CAN frames. NEPTUN is optional and its tests were dropped from the repository's suite (the code path and the release table stay);
  the observations above were made when they existed.
* **CUSTOM (native builds, DESIGN.md section 20)**: no factory image. The image fills the records of the *original* firmware's first-boot
  gaps, found through the original record table, and a custom build has no such table (`eepromRecordTable` is unavailable: "custom build:
  original firmware addresses do not apply"). A new EEPROM stays entirely erased (2048 bytes of `0xFF`, saved with the profile), and the
  native firmware initializes it itself; `eepromFactoryInit` is `{applied: false, reason: "Skipped for CUSTOM: ..."}`. An existing EEPROM is
  never touched, as for every release. (The Main 5.8 layout is the native build's own contract; nothing here edits it.)
* A release without a proven table is skipped, and the state says why.

## Reproduction

`crates/ngc/tests/eeprom_init.rs` (real TRITON images; skipped without them) holds the checks quoted here: a new profile is initialized
once (the report, the saved image, the first-boot comparison with a blank EEPROM, the erased set after a first boot, a Restart and a
serial change that never apply it again), an entirely erased stored image is a new EEPROM, an existing profile is never touched (several
shapes, and an older profile with blank tissues stays invalid in `decoHealth`), the delta vital capacity (finite and rising, handset
text `0.00`), the Restart that loads finite tissues, the order of the tissue words and the inventory against the image's table. The
contrast with a blank EEPROM (NaN, text `nan`, no CAN `0x225`) is a slow-tier test (`--ignored`). `crates/ngc/src/eeprom_init.rs` has
the unit tests of the gate. `ngc-cli run --data-dir <fresh dir> --seconds 8` boots a fresh profile from the image; `ngc-cli run`
without `--data-dir` uses a bare system with an erased EEPROM.

## Open questions

* **`0x60`, `0x61` (fixed setpoints, mode 3)** and **`0x0c`, `0x13`**: the handset's effect of the raw `0xFF` was not analyzed, and no
  firmware default exists to justify a value.
* **Tissues at another surface pressure**: the block is the equilibrium at 1.013 bar, also when the page's surface-pressure setting is
  different; a restart then loads slightly over- or under-saturated tissues until they equilibrate.
* **Physical units**: whether a physical unit's EEPROM carries these records from production is unknown; nothing here is a claim about it.
