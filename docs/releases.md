# Firmware releases

The engine runs the original **main 5.8 / handset 65.3** images of two releases, selected by the SHA-256 of the SREC
files (or of their reconstructed binary span when the text differs, for example in line endings). The table lives in
`crates/ngc/src/firmware.rs` (`RELEASES`, `TRITON`, `NEPTUN`); both images of a session must come from the same release
(`firmware::common_release`), otherwise creation fails with `Mixed firmware releases: the main image is ... but the handset
image is ... Supply both images of the same release.` Nothing of the images is stored in the repository. A third kind of image, the **custom
(native) build**, is admitted without release identification and has no addresses at all: see [Custom builds](#custom-native-builds).

| | TRITON-5.8-65.3 | NEPTUN-5.8-65.3 |
| --- | --- | --- |
| label | `TRITON main 5.8 / handset 65.3` | `NEPTUN main 5.8 / handset 65.3` |
| main SREC | `ngc_main_5.8_TRITON.srec`, SHA-256 `838cb050...d6ea`, 11 671 S3 records | `ngc_main_5.8_NEPTUN.srec`, SHA-256 `e462bc73...8c89`, 20 059 S3 records |
| main span / binary SHA-256 | `0x08004000..0x08031938` (186 680 B), `76af4ba5...fd014d` | `0x08004000..0x08052584` (320 900 B), `ba59fbc0...297ebb` |
| main reset PC / SP | `0x080213b8` / `0x20018000` | `0x080390b8` / `0x20018000` |
| handset SREC | `ngc_handset_65.3_TRITON.srec`, SHA-256 `71a9af68...1e03`, 44 152 S3 records | `ngc_handset_65.3_NEPTUN.srec`, SHA-256 `f91adcf4...162e`, 44 197 S3 records |
| handset span / binary SHA-256 | `0x08004000..0x080b074c` (706 380 B), `f9a85fb0...9b57` | `0x08004000..0x080b0a14` (707 092 B), `6adc5760...9f42` |
| handset reset PC / SP | `0x08008410` / `0x20018000` | `0x08008444` / `0x20018000` |

Both releases have the vector table at `0x08004000`, the 4-byte hole after it and 99 vector entries. A board starts from the
vectors of the image it is given (`firmware.reset_pc()`, `initial_sp()`), not from constants.

## Firmware-specific addresses

Application RAM fields and one code address are properties of an image. `ReleaseAddresses` holds them per release; the
state document publishes the table as `firmware.addresses` (`{address, basis}` or `{address: null, reason}`) and the
fields that depend on an unproven one are `null` with the reason in `unavailable`. **No value is ever taken over from
TRITON.** The decompression entries below are main RAM addresses or physical EEPROM offsets (and one flash address, the record
table); `decoHealth` carries its own reason when one is unavailable.

| entry | TRITON | NEPTUN | used for |
| --- | --- | --- | --- |
| `handsetOrientation` (RAM byte) | `0x20000740` | `0x20000740` (proven) | documentation only: the engine no longer reads it (it swapped the Up/Down pin masks); Up is `PE5` and Down is `PE3` for every firmware, the mapping of the default value 1 (DESIGN.md 20.3) |
| `handsetErrorLoopPC` (code) | `0x0800598e` | `0x0800598e` (proven) | error stop when the handset PC is in the interrupts-disabled error loop |
| `mainBatteryReady` (RAM byte) | `0x200042a1` | unavailable | state `mainBatteryReady` |
| `mainWakeCause`, `mainScreenMode`, `mainMode`, `mainHalTick`, `mainPressure`, `mainTemperature` | `0x20004388`, `0x2000438d`, `0x200024b2`, `0x20004a6c`, `0x20004378`, `0x20004360` | unavailable | `mainApplication` of the status JSON, benchmark, scenarios |
| `mainCurrentTcb` (FreeRTOS `pxCurrentTCB`) | `0x20005708` | `0x200053a8` (proven) | diagnostics |
| `handsetCurrentTcb` | `0x200013fc` | `0x200013fc` (proven) | diagnostics |
| `mainDecoTissues` (RAM, 16 records of 36 bytes; N2 float at +24, He float at +28) | `0x20001e94` | unavailable | `decoHealth.tissues` |
| `mainBreathingMode` (RAM byte; 2 = measured ppO2) | `0x20002457` | unavailable | `decoHealth.oxygen` |
| `mainPpO2` (RAM float) | `0x2000421c` | unavailable | `decoHealth.oxygen` |
| `mainCellFlags` (RAM, 3 bytes, cached cell flags) | `0x200023f4` | unavailable | `decoHealth.oxygen` while the ppO2 is zero |
| `eepromTissueBlock` (EEPROM physical offset, 128 bytes) | `0x0ff` | unavailable | documentation of the stored tissue block (the removed pre-boot repair fixture used it) |
| `eepromDecoDate` (EEPROM physical offset, 4 bytes) | `0x17f` | unavailable | documentation of the saved decompression date record (the removed pre-boot repair fixture used it) |
| `eepromRecordTable` (**flash** address of the 568-byte record table, IDs 0..`0x8d`) | `0x080306f2` | `0x08050e38` (proven) | the layout check of the EEPROM factory image (`docs/eeprom.md`) |

Scenario-only addresses (battery wizard offsets, key sampler, screen ids, `0x08005b18`) are TRITON-specific; the scenario
suite refuses other releases.

### How the TRITON decompression entries were established

The five RAM entries come from Renode hooks on the unchanged TRITON main image (start-up initializer `0x08008308`, the NDL routine around `0x08008550`/`0x0800857e`): every tissue word, the breathing-mode byte and the ppO2 were read at those points, including the NaN case; the cell-flag cache `0x200023f4..=0x200023f6` was read on this engine (`0x01` x3 on a fresh profile, `0x09` x3 after the firmware's air calibration, `0x01` x3 after a cold boot). The two EEPROM entries were checked against the original record table of the main image: it holds one entry (offset u16, size u16) per record ID at `0x080306f2 + 4 * ID`, and IDs `0x6a..=0x89` are 32 words at physical `0x0ff..=0x17e`, ID `0x8a` is 4 bytes at `0x17f` (`decoHealth` reads only these two ranges). They are consumed by `crates/ngc/src/deco.rs` and reported in the state as `firmware.addresses`.

NEPTUN: **unavailable**. The decompression code of its main image is part of the different, frame-pointer build that has no instruction-identical counterpart (the reason of `NEPTUN_MAIN_BUILD`), so `decoHealth` reports `unknown` with that reason. A proof would need `testdata/tools/release_match.py` windows around the initializer, which was not attempted.

### The EEPROM record table (both releases)

The EEPROM layout is a data table, not code, so it can be proven by bytes alone: the 568-byte table (entries of offset u16 and size u16 for logical IDs 0 to `0x8d`, SHA-256 `a69c0b84bdffa90dee14b26daa4eb35c7578b0923247a4ffe9b2c98fef244672`) occurs exactly once in each main image, at `0x080306f2` in TRITON and at `0x08050e38` in NEPTUN, and NEPTUN's accessor code holds three literal-pool references to it (`0x0801ad40`, `0x0801adec`, `0x0801b124`) like TRITON's three (`0x0801039c`, `0x08010488`, `0x08010528`). The record layout (ID to physical offset and size) is therefore the same. The EEPROM factory image (`crates/ngc/src/eeprom_init.rs`) checks the hash against the loaded image before it writes, and a test (`crates/ngc/tests/eeprom_init.rs`) re-checks the TRITON table and the inventoried records against it (NEPTUN has no tests; the table was proven when they existed). That proves the layout only: `eepromTissueBlock` and `eepromDecoDate` above stay unavailable for NEPTUN because the RAM side of the decompression code is not proven; `docs/eeprom.md` lists what was checked on NEPTUN's own first boot.

### How the NEPTUN entries were proven

The NEPTUN **handset** image is 92 % instruction-identical to TRITON's (8-instruction windows after masking relocations), the
NEPTUN **main** image only 33 % because it is a different build of the application (frame-pointer code, `push {r7, lr}` /
`add r7, sp, #0`, 320 900 against 186 680 bytes); hand-written code such as the FreeRTOS PendSV handler is identical.
`testdata/tools/release_match.py` finds the code that accesses a TRITON RAM address, masks what a relocation changes (literal
offsets, `bl` targets, branch displacements) and searches the NEPTUN disassembly (`ngc-cli disasm`) for the same window.

* `handsetOrientation`: the key sampler loads the byte at the TRITON sites `0x08045fb2` and `0x08045fc2`; the 27 and 26
  instructions around them occur exactly once each in NEPTUN (`0x08046276`, `0x08046286`) and load the literal `0x20000740`
  (the third TRITON site has no counterpart).
* `handsetErrorLoopPC`: `cpsid i; b .` (the HAL error handler) at `0x0800598c` in both images; the 80 instructions
  `0x08005940..0x080059fe` are identical at the same addresses apart from `bl` targets, including the literal pool.
* `handsetCurrentTcb`: 13 of the 15 TRITON access sites of `pxCurrentTCB` have exactly one identical 26..36 instruction window in
  NEPTUN and load `0x200013fc`.
* `mainCurrentTcb`: the PendSV handler (vector 14) is the same 27 instructions at TRITON `0x08028a40` and NEPTUN `0x080470d0`;
  its first literal is `pxCurrentTCB`: `0x20005708` becomes `0x200053a8`.
* Everything else in the main image (the battery module: initializer `0x0801af2c`, getter `0x0801ae4c`) has no
  instruction-identical counterpart, so those fields are reported unavailable. The proven entries were re-checked against
  the local images by a test while NEPTUN had tests; NEPTUN is optional and has none now.

## Custom (native) builds

`firmware::load_custom(srec, role)` (ABI `ngc_set_custom_firmware`, report `ngc_firmware_inspect_custom`, CLI `ngc-cli run|info --custom`)
admits an SREC for an explicit role without looking at its hashes: the slot decides, because both native builds share a reset vector.
Only the structure is validated, each violation with its address or value (`FirmwareError::Custom`, the same `checks` list in the report):

| check | requirement |
| --- | --- |
| `srec-syntax` | record syntax, checksums, record counts and no overlapping data; a file of at most 16 MiB of text |
| `address-bounds` | at least one data record, every byte inside `0x08004000..0x08100000` (checked before the binary is reconstructed, so the span and its allocation are bounded) |
| `span` | informational: lowest to highest data address; bytes inside it that no record covers are `0x00` (the original images keep their `0xFF` reconstruction) |
| `vector-table` | loaded data at `0x08004000`, at least the initial SP and the reset vector (8 bytes); the report decodes up to 99 words |
| `initial-sp` | 8-byte aligned, `0x20000000 < SP <= 0x20018000` |
| `reset-vector` | Thumb bit set, and the entry lies inside a loaded data segment (a gap is not loaded code) |
| `entry-point` | an S7/S8/S9 start address, when present, equals the reset vector (the Thumb bit is ignored in the comparison) |

The image belongs to the pseudo-release **`CUSTOM`** (label "Custom build"; not in `RELEASES`, not reachable through `Release::by_id`).
Every address of the table above is `{address: null, reason: "custom build: original firmware addresses do not apply"}`, so by the same
rules that keep NEPTUN honest the engine reads **nothing** of the original application: no terminal-handler stop (`handsetErrorLoopPC`),
no `mainBatteryReady`/mode/screen variables (the state reports `null` and lists them in `unavailable`), `decoHealth` is `unknown`, and the
EEPROM factory image is skipped (`eepromFactoryInit.applied` is false; a new EEPROM stays blank and the firmware initializes it). The exact
routine acceleration keys on code bytes, not on the release, so it applies to a custom image only when its routines are byte-identical to
a recognized one. A session takes two custom images or two images of one original release: `Mixed firmware: the main image is a custom
build but the handset image is TRITON-5.8-65.3 (...)` otherwise, from `ngc_session_create` / `Session::new` / `common_release`.

Runtime behavior that does not depend on the release (all of it also holds for TRITON and NEPTUN unless noted):

* **Faults.** The state carries `faults: {main: {cfsr, hfsr, lockup}, handset: {...}}` (numbers, and the lockup reason or `null`), the health signal
  of a custom build next to the UART console.
* **Buttons.** Up is `PE5` (mask 2), Down is `PE3` (mask 1); the orientation byte is not read. `PE3`/`PE5` rest high from reset through a
  pull-up (a press is accepted whenever no other gesture runs, with no readiness gate on the TIM3 configuration), and TIM3 remembers the
  level of an input it saw before its capture was configured, so the configuration produces no edge (`Stm32Timer::with_external_pull_ups`;
  Renode's timer drops such an input, which is why the Renode button model waited for the configuration and produced two zero-width capture
  edges). The recorded scenarios and the dive benchmark keep the Renode model through `scenario::recorded_config`
  (`SessionConfig::button_pull_up = false`, also selected by the ABI benchmark hook `blankEeprom`), so their results are unchanged.
* **Standby** is detected for a custom build from the hardware state, polled on the 50 virtual ms grid: the main core went to sleep in
  `WFI`/`WFE` with `SCB.SCR.SLEEPDEEP` set (`Cpu::deep_sleep_entries`, an observation counter, so a wake-up by a still-running emulated
  peripheral does not hide it) and `PWR_CR1.LPMS` selects Standby (3) or Shutdown (4). Stop modes (0 to 2) are plain sleeps. The original
  images keep the register-only heuristic of the Renode runner (`LPMS == 3` and `SLEEPDEEP`), whether the core sleeps or not.
* **Wake fixture.** `PWR.SR1 = 0x104` and `RCC.CSR = 0` as before, but not `RTC.BKP1R = 0x32F0`: that marker is the original application's
  own, and a native build keeps the backup words it saved (`rtcPersistence.mainBkp1WakeOverride` is false).
* **Persistence.** The images are part of the system: Restart, Cold, Wake, a machine reset (IWDG, `SYSRESETREQ`) and a reopened profile keep them.

## Cold boot

The cold-boot fixture (zero `PWR.SR1` / `RCC.CSR` wake flags, observed standby request) is characterized for TRITON only: the
TRITON main requests standby after about 1.5 virtual seconds. The NEPTUN main kept running for 40 virtual seconds without a
standby request (it boots normally), so there is no observed-standby route. `cold` (action and `bootMode: "cold"`) is
refused for NEPTUN with `Release::cold_boot_refusal`; Restart and Wake work.

A custom build may boot cold (`CUSTOM` has no refusal): the zero flags are supplied and the standby is observed from the hardware state (see
[Custom builds](#custom-native-builds)), so a native firmware that selects Standby or Shutdown and executes `wfi` stops the run exactly like the
TRITON route. Whether a given native build does is its own behavior; a run without that sleep simply goes on. The TRITON images loaded
through the custom path are the example: the main sets `PWR_CR1 = 0x303` and `SCB.SCR = 4` after its cold start and then idles without
ever sleeping, so only the original path (the register heuristic) reports a standby for them.

## Speed

The idle-loop fast-forward recognizes loops of straight, call-free code. The NEPTUN main image (an unoptimized build) keeps its
FreeRTOS idle task in a loop that calls `prvCheckTasksWaitingTermination` every round, and a HAL busy-wait with timeout
polls the UART5 status register (`UART_WaitOnFlagUntilTimeout`, `0x0803dbc4`, reads `0x4000501c`, always `0x204000c0`); a
PC histogram of the steady state (3-9 virtual s) shows these two loops at about 98 % of the instructions, neither involves a
solenoid or GPIO. Main fast-forward skips 0 % (the handset 94 %), so NEPTUN runs at about 2.4x real time natively with
fast-forward (TRITON 33x, 1.4x without). This is deferred; the idle loop is exact either way (fast-forward on/off digests are
identical for both releases).
