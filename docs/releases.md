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
TRITON.**

| entry | TRITON | NEPTUN | used for |
| --- | --- | --- | --- |
| `handsetOrientation` (RAM byte) | `0x20000740` | `0x20000740` (proven) | Up/Down pin mask swap of `up` / `down` |
| `handsetErrorLoopPC` (code) | `0x0800598e` | `0x0800598e` (proven) | error stop when the handset PC is in the interrupts-disabled error loop |
| `mainBatteryReady` (RAM byte) | `0x200042a1` | unavailable | state `mainBatteryReady` |
| `mainWakeCause`, `mainScreenMode`, `mainMode`, `mainHalTick`, `mainPressure`, `mainTemperature` | `0x20004388`, `0x2000438d`, `0x200024b2`, `0x20004a6c`, `0x20004378`, `0x20004360` | unavailable | `mainApplication` of the status JSON, benchmark, scenarios |
| `mainCurrentTcb` (FreeRTOS `pxCurrentTCB`) | `0x20005708` | `0x200053a8` (proven) | diagnostics |
| `handsetCurrentTcb` | `0x200013fc` | `0x200013fc` (proven) | diagnostics |

Scenario-only addresses (battery wizard offsets, key sampler, screen ids, `0x08005b18`) are TRITON-specific; the scenario
suite refuses other releases.

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

The idle-loop fast-forward recognises loops of straight, call-free code. The NEPTUN main image (an unoptimized build) keeps its
FreeRTOS idle task in a loop that calls `prvCheckTasksWaitingTermination` every round, and a HAL busy-wait with timeout
polls the UART5 status register (`UART_WaitOnFlagUntilTimeout`, `0x0803dbc4`, reads `0x4000501c`, always `0x204000c0`); a
PC histogram of the steady state (3-9 virtual s) shows these two loops at about 98 % of the instructions, neither involves a
solenoid or GPIO. Main fast-forward skips 0 % (the handset 94 %), so NEPTUN runs at about 2.4x real time natively with
fast-forward (TRITON 33x, 1.4x without). This is deferred; the idle loop is exact either way (fast-forward on/off digests are
identical for both releases).
