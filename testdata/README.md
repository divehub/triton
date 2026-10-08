# Test data

Regression data of the engine. Nothing here is firmware: the original SREC files are never part of this repository
(you supply them in `firmware/<release>/` or below `NGC_FIRMWARE_DIR`, both ignored by Git), and tests that need them
skip when they are absent. The Renode recordings were made with Renode 1.17.0 on 2026-10-07/08, with a harness and
generators that belong to the separate Renode-based analysis workspace (not public) and are not part of this
repository, so these files are fixtures, not something to regenerate.

## In this directory

- `renode-micro-vectors.json` (was `reference/micro/vectors.json`): ten tiny Thumb programs run on a minimal
  single-CPU Renode platform (Cortex-M4, SysTick, TIM6, DWT). Each entry holds the program image (hex), labels, the
  recorded instruction index of every interrupt entry and further timing facts. Used by
  `crates/armv7m/tests/micro.rs`, `crates/ngc/tests/micro_timer.rs` and `crates/emu-core/tests/clock.rs`; the
  semantics are explained in `docs/renode-semantics.md` section 15.
- `renode-runner-rtc-state.json`: the `rtc-state.json` written by the Renode runner of the analysis workspace during
  the clock/storage probe of 2026-10-07 (`emulation/runtime/clock-storage/20261007T122012979393Z/` there). The
  persistence test requires that this engine reads and rewrites it byte for byte.
- `instruction-starts-main.txt`, `instruction-starts-handset.txt`: one bit per halfword of the TRITON main 5.8 and
  handset 65.3 images (span from `0x08004000`), set where a Ghidra analysis (exported in the analysis workspace) listed an instruction;
  57 950 and 130 737 starts. They contain no instruction bytes and no mnemonics; `crates/armv7m/tests/static_decode.rs`
  decodes the bytes at those addresses from the local SREC. `tools/instruction_starts.py` shows how they were derived
  from the Ghidra listing (standard library only; the listing itself is not part of this repository).

## Tools

- `tools/instruction_starts.py`: derives the instruction-start data above from a Ghidra listing.
- `tools/release_match.py`: matches TRITON code against another firmware release (`sites`, `range`, `align`, `bytes`);
  how the NEPTUN address table was proven, see `docs/releases.md`. It needs `ngc-cli` and the local SRECs.

## Next to the tests (recorded Renode output compiled into the test binaries)

- `crates/stm32/tests/renode_golden.rs`, `crates/ngc/tests/renode_golden.rs`: register-level sequences (GPIO, EXTI,
  CRC, RNG, RCC, handset ADC) driven on the unmodified `handset.repl` platform without firmware.
- `crates/stm32/tests/renode_timer/golden.txt`: transcripts of the stock `STM32_Timer`, `STM32F4_RTC` and
  `STM32_IndependentWatchdog` (register values, output edges with nanosecond stamps, machine resets).
- `crates/ngc/tests/renode_per_e/golden.rs`: LCD, main ADC (with and without DMA), MS5837 and QSPI/NOR operation
  transcripts with every register read, summary text, edge log and backing-file hash.
- `crates/ngc/tests/renode_i2c_eeprom/golden.rs`: I2C controller and EEPROM bank flows and random traffic.
- `crates/ngc/tests/micro_pwm/vector.json`: chunk boundaries of the handset PWM timer events (DWT cycle counter
  samples at 19 delays).

The scenario suite (`ngc-cli scenario`, `crates/ngc/src/scenario/`) embeds the values that the Renode runner of the
analysis workspace recorded as constants with their provenance (`recorded by the Renode runner in the analysis
workspace, <path>`; the paths name files of that workspace, which is not public). Every recording was made with the
runner's 1500 mV batteries, while a fresh profile of this engine starts at 4100 mV: the scenarios pin 1500 mV
(`scenario::recorded_inputs`, reported as `platformOptions.batteryMv`), and so do the other tests that compare with a
recorded value.
