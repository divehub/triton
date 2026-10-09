# Firmware releases

The engine runs the original **main 5.8 / handset 65.3** images of two releases, selected by the SHA-256 of the SREC
files (or of their reconstructed binary span when the text differs, for example in line endings). The table lives in
`crates/ngc/src/firmware.rs` (`RELEASES`, `TRITON`, `NEPTUN`); both images of a session must come from the same release
(`firmware::common_release`), otherwise creation fails with `Mixed firmware releases: the main image is ... but the handset
image is ... Supply both images of the same release.` Nothing of the images is stored in the repository.

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
| `handsetOrientation` (RAM byte) | `0x20000740` | `0x20000740` (proven) | Up/Down pin mask swap of `up` / `down` |
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

The five RAM entries come from Renode hooks on the unchanged TRITON main image (start-up initializer `0x08008308`, the NDL routine around `0x08008550`/`0x0800857e`): every tissue word, the breathing-mode byte and the ppO2 were read at those points, including the NaN case; the cell-flag cache `0x200023f4..=0x200023f6` was read on this engine (`0x01` x3 on a fresh profile, `0x09` x3 after the firmware's air calibration, `0x01` x3 after a cold boot). The two EEPROM entries were checked against the original record table of the main image: it holds one entry (offset u16, size u16) per record ID at `0x080306f2 + 4 * ID`, and IDs `0x6a..=0x89` are 32 words at physical `0x0ff..=0x17e`, ID `0x8a` is 4 bytes at `0x17f` (the fixture reads only these two ranges). They are consumed by `crates/ngc/src/deco.rs` and reported in the state as `firmware.addresses`.

NEPTUN: **unavailable**. The decompression code of its main image is part of the different, frame-pointer build that has no instruction-identical counterpart (the reason of `NEPTUN_MAIN_BUILD`), so `decoHealth` reports `unknown` with that reason and the EEPROM consistency fixture is skipped with it. A proof would need `testdata/tools/release_match.py` windows around the initializer, which was not attempted.

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
  instruction-identical counterpart, so those fields are reported unavailable. `crates/ngc/tests/session_parity.rs`
  (`the_neptun_address_table_is_proven_against_the_images`) re-checks the proven entries against the local images.

## Cold boot

The cold-boot fixture (zero `PWR.SR1` / `RCC.CSR` wake flags, observed standby request) is characterized for TRITON only: the
TRITON main requests standby after about 1.5 virtual seconds. The NEPTUN main kept running for 40 virtual seconds without a
standby request (it boots normally), so there is no observed-standby route. `cold` (action and `bootMode: "cold"`) is
refused for NEPTUN with `Release::cold_boot_refusal`; Restart and Wake work.

## Speed

The idle-loop fast-forward recognizes loops of straight, call-free code. The NEPTUN main image (an unoptimized build) keeps its
FreeRTOS idle task in a loop that calls `prvCheckTasksWaitingTermination` every round, and a HAL busy-wait with timeout
polls the UART5 status register (`UART_WaitOnFlagUntilTimeout`, `0x0803dbc4`, reads `0x4000501c`, always `0x204000c0`); a
PC histogram of the steady state (3-9 virtual s) shows these two loops at about 98 % of the instructions, neither involves a
solenoid or GPIO. Main fast-forward skips 0 % (the handset 94 %), so NEPTUN runs at about 2.4x real time natively with
fast-forward (TRITON 33x, 1.4x without). This is deferred; the idle loop is exact either way (fast-forward on/off digests are
identical for both releases).
