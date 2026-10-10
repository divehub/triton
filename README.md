# NGC Rust/WebAssembly emulator

A browser-hosted emulator of the paired **main 5.8 / handset 65.3** NGC electronics. It loads the two original SREC files of a supported release (**TRITON**, or **NEPTUN** with the limits below) and runs both unchanged firmware images on a Rust Cortex-M4F core. The STM32 peripheral models are ported from the Renode 1.17.0 platform. Both boards run in real time inside a Web Worker. Every state and capture carries an `ngc-wasm/<version>` engine label; Renode evidence does not transfer to it automatically. Nothing here claims physical-device accuracy.

A hosted copy runs at **<https://triton.divehub.ai>**.

> **The firmware is not included and never will be.** You need the two original S-record files of one release (`ngc_main_5.8_TRITON.srec` and `ngc_handset_65.3_TRITON.srec`, or the NEPTUN pair). The page verifies them in your browser (SHA-256 and content) and never uploads, bundles or serves them. This repository contains no firmware bytes, and `.gitignore` refuses the usual firmware file types.

| Topic | Document |
| --- | --- |
| Design, contracts, validation plan and work log | [DESIGN.md](DESIGN.md) |
| Browser app | [web/README.md](web/README.md) |
| Publishing: GitHub Pages site and Vercel firmware proxy | [deploy/README.md](deploy/README.md) |
| Supported releases and per-release addresses | [docs/releases.md](docs/releases.md) |
| EEPROM records a first boot leaves erased, and the factory image | [docs/eeprom.md](docs/eeprom.md) |
| Renode behavior the engine reproduces | [docs/renode-semantics.md](docs/renode-semantics.md) |
| Peripheral framework | [docs/framework.md](docs/framework.md) |
| Recorded test data | [testdata/README.md](testdata/README.md) |
| Toolchain provisioning | [tools/README.md](tools/README.md) |
| Third-party notices | [licenses/README.md](licenses/README.md) |
| Contributor and agent rules | [AGENTS.md](AGENTS.md) |

## Run in a browser

You need Rust (see [Build](#build)), Python 3 and a current browser.

```sh
python3 web/build.py      # release WebAssembly -> web/pkg/ (ignored by Git)
python3 web/serve.py      # http://127.0.0.1:8770, loopback only
```

Open <http://127.0.0.1:8770> and provide the two SREC files of one release.
- Each release has its own profile in the browser's origin-private storage: EEPROM, NOR log, RTC checkpoint, inputs and LED labels. A profile can be exported or imported as plain files.
- **Load from URLs** can fetch the files from their `api.multi3s.com` or Wayback Machine addresses through the firmware proxy. A page needs a proxy address to offer it (see [Publishing](#publishing)); a local page can use the local development proxy with `?firmware-proxy=http://127.0.0.1:<port>/api/firmware` (`node deploy/dev-proxy.mjs`).
- For automated browser checks, `python3 web/serve.py --dev-firmware firmware/TRITON-5.8-65.3` serves your local files to `http://127.0.0.1:8770/?dev-firmware` (loopback only, off by default).

The page has two views:
- **Basic view.** Shows the Vibrator, Red LED and White LED indicators, the LCD, the handset buttons and the run controls. Its simulated conditions apply immediately:
  - oxygen: a base voltage plus three cell offsets;
  - pressure: depth, water type and surface pressure, plus two sensor offsets;
  - temperature: a base plus two sensor offsets.

  The two pressure/temperature sensors are numbered as the firmware numbers them, which is the reverse of the engine's input names: **sensor 1 is the MS5837 on I2C2** (`pressure2Mbar`, `temperature2C` in `inputs.json`), **sensor 2 the one on I2C1** (`pressure1Mbar`, `temperature1C`). The engine keys and `inputs.json` are unchanged; the page maps them (P1/T1 are sensor 1), and Advanced names the bus on each raw field.
- **Advanced.** Holds the raw inputs (applied with Apply inputs), output command histories, HUD color labels, the UART console and the technical controls.

**Decompression warnings.** The basic view warns, with the next step, when the engine's read-only report shows invalid stored tissues: "Decompression state invalid: this profile's stored tissues are blank. Reset the profile (Advanced → Profile and evidence → Reset profile) to start with an initialized EEPROM." (Uncalibrated oxygen cells are reported in Advanced but not shown as a warning.) Both conditions are behaviors of the original firmware (it loads NaN tissues from a restarted profile that never saved them, and its ppO2 is NaN with uncalibrated cells, which keeps the no-decompression limit at 99), not defects of the engine. A new profile does not have the first one: when the engine creates a new EEPROM (no `eeprom.bin`, or an entirely erased one) it writes a **factory image** once, which gives the records the firmware's first-boot defaults never write (serial 1, the oxygen-toxicity values that otherwise make the handset print `ΔvC?a?%`, the surface-equilibrium tissues, the no-fly time) values derived from the firmware's own code. An existing EEPROM is never touched, and there is no option for it: [DESIGN.md](DESIGN.md) section 18 and [docs/eeprom.md](docs/eeprom.md). A labeled emulator fixture, on by default and switchable under Start options, makes every start begin at the surface (depth 0; a new session also resets the oxygen cells to their defaults); see [DESIGN.md](DESIGN.md) section 17. A cold boot makes the firmware clear the oxygen calibration; the page says so.

**Replay pulses** (on by default) flashes HUD and vibrator activations that happened between status updates. It only animates the display and does not change firmware timing.

**Dive game.** **Start game**, beside Boot emulator, runs the same session as a game (desktop only): you dive in a water scene, the depth and the gas in the loop become the emulated pressure and oxygen-cell inputs, and you operate the handset through its display. Its oxygen-cell voltages are a labeled game fixture (12 mV in air at the surface plus ±1.0 mV per cell, kept per profile). The header's **Reset all** (after a confirmation) erases the dive computer's memory: it closes the session unsaved, clears the release's profile and starts a new game session on the same firmware, with the clock starting again from the local time. Controls, the clock and the limits: [web/README.md](web/README.md#dive-game-start-game-design-21); its gas model has its own checks, `node --test web/game-gas.test.mjs`.

## Firmware

Supply the files yourself; they are read only from where you put them and are never copied or committed.

| Where | Used by |
| --- | --- |
| The browser's file picker, drag and drop, or **Load from URLs** | the page |
| `firmware/<release>/` at the repository root, for example `firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec` and `ngc_handset_65.3_TRITON.srec`, or `firmware/NEPTUN-5.8-65.3/` | `ngc-cli`, the Rust tests, `web/test-node.mjs`, `web/bench-node.mjs` |
| A directory holding the release directories, named by `NGC_FIRMWARE_DIR` (it replaces `firmware/` in the row above) | the same |

`firmware/` is ignored by Git. Without the files, every test that needs them skips with a message and passes.

**Custom firmware builds.** "Use custom firmware builds" on the entry screen (or `ngc-cli run --custom --main <srec> --handset <srec>`) loads any pair of S-record images, such as a native rewrite, for an explicit board each, without release verification. Structural checks still apply: valid records, data inside `0x08004000..0x08100000`, a vector table at `0x08004000`, an 8-byte-aligned initial SP up to `0x20018000`, a Thumb reset vector inside the image and a matching S7 entry. A custom session is labeled "Custom build"; features that rely on original firmware addresses are off and report "unknown", a new EEPROM starts blank, and custom builds share one profile, separate from TRITON and NEPTUN. See [DESIGN.md](DESIGN.md) section 20.

## Build

The workspace has no external crates; it only needs the toolchain pinned in `rust-toolchain.toml` (Rust 1.98.1 with the `wasm32-unknown-unknown` target).

```sh
rustup toolchain install 1.98.1 --profile minimal --target wasm32-unknown-unknown   # once
./cargo build --release                                                              # native tools
python3 web/build.py                                                                 # the WebAssembly module
```

`./cargo` runs cargo from the repository root: with the toolchain in `tools/rust` if you provisioned one there ([tools/README.md](tools/README.md)), otherwise with the `cargo` on your `PATH` (which is what CI does).

## Tests and native tools

The tests have two tiers. The **quick loop** is what you run on every change: with firmware, `./cargo test --workspace --release` takes
about 36 s of test time on an Apple M1 Max (no single test binary above 5 s) after compilation. The **slow tier** holds the expensive
end-to-end checks that must stay available but not run on every iteration (the long dive benchmark identity, the full scenario suite);
those tests are `#[ignore = "slow: ..."]`. Tests that need firmware skip with a message and pass without it (that is what CI runs);
`NGC_FIRMWARE_DIR=/path/to/firmware` points at your release directories. NEPTUN is optional and has no tests. Run every command in the
foreground; none of them needs a background process.

| Area | Quick loop | Time |
| --- | --- | --- |
| CPU and FPU (`armv7m`, `armv7m-vfp`) | `./cargo test -p armv7m -p armv7m-vfp --release` | 15 s |
| Framework and peripherals (`emu-core`, `stm32`; the golden replays of recorded Renode transcripts) | `./cargo test -p emu-core -p stm32 --release` | 6 s |
| Session, actions, persistence, fixtures, outputs, serial, CLI, ABI (`ngc`, `ngc-wasm`, `ngc-cli`) | `./cargo test -p ngc -p ngc-wasm -p ngc-cli --release` | 15 s |
| One test file, for example the EEPROM factory image | `./cargo test -p ngc --release --test eeprom_init` | 5 s |
| Everything in Rust | `./cargo test --workspace --release` | 36 s |
| Page logic, no engine and no firmware | `node web/test-ui.mjs` | 2 s |
| Page logic against the real module, with firmware (build the module first) | `python3 web/build.py` then `node web/test-node.mjs` | 16 s |
| Deploy: proxy, Pages site, proxy assembly | `node deploy/test-proxy.mjs`, `python3 deploy/test_build_site.py`, `python3 deploy/test_build_proxy.py` | under 1 s |

The exactness invariants are in the quick loop, each on a short real-firmware run: the idle-loop fast-forward on and off
(`crates/ngc/tests/session_parity.rs`), the routine acceleration on, off and in shadow mode (`crates/ngc/tests/routine_accel.rs`), and
native against WebAssembly (`web/test-node.mjs` builds `ngc-cli` and compares the state digests of a 3 s boot with `web/bench-node.mjs
--expect`; it skips with a message if `./cargo build --release -p ngc-cli` fails).

**Slow tier**, run before a push that touches timing, the CPU, the acceleration or the session, and once after a large refactoring:

```sh
./cargo test --workspace --release -- --ignored    # about 80 s: dive benchmark identity on/off/shadow, all scenarios (also with the acceleration off), NaN contrast of the factory image, micro-benchmarks
./cargo run -p ngc-cli --release -- scenario all --main <main.srec> --handset <handset.srec> --out <dir>
./cargo run -p ngc-cli --release -- bench --dive --verify-routine-accel   # valid-tissue dive at 20/30 m, acceleration on and off must be identical
node web/test-node.mjs --slow                      # adds the real-time pacing measurement and the replay logic on a real alert dive (about 10 s more)
```

Native tools:

```sh
./cargo run -p ngc-cli --release -- info --main <main.srec> --handset <handset.srec>
./cargo run -p ngc-cli --release -- run --main <main.srec> --handset <handset.srec> --seconds 10 --data-dir <profile dir>
./cargo run -p ngc-cli --release -- run --release NEPTUN-5.8-65.3 --seconds 10.5   # SRECs from firmware/<release>/
./cargo run -p ngc-cli --release -- bench --main <main.srec> --handset <handset.srec>
./cargo run -p ngc-cli --release -- bench --dive --verify-routine-accel   # valid-tissue dive at 20/30 m; SRECs from firmware/<release>/
./cargo run -p ngc-cli --release -- scenario all --main <main.srec> --handset <handset.srec> --out <dir>
node web/bench-node.mjs --slice 0.01               # V8 benchmark of the release module
```

Pass file paths as absolute paths: `./cargo` runs from the repository root.

Further CLI options:
- `--no-i2c-idle-high` (run, bench, scenario) turns off the main I2C idle-high fixture, which is on by default.
- `--no-start-at-surface` (run with `--data-dir`) turns off the start-at-the-surface fixture, which is on by default (see [DESIGN.md](DESIGN.md) section 17). With `--data-dir`, a profile without `eeprom.bin` (or an entirely erased one) gets its EEPROM created from the factory image once, and an existing `eeprom.bin` is never touched; there is no flag for it (section 18; `run` without `--data-dir` uses a bare system with an erased EEPROM). `run` prints the read-only decompression state (`decoHealth`; `--json` has the details).
- `--initial-local-time YYYY-MM-DDTHH:MM:SS` (run with `--data-dir`; the year 2000 to 2099) is the host's local date and time for a **new profile**: a board whose RTC has no saved checkpoint in `rtc-state.json` and no EEPROM date seed starts its calendar from it (24-hour format, correct weekday, provenance `host-local-time`); an existing checkpoint is never changed ([DESIGN.md](DESIGN.md) section 23). The page's session-create JSON key is `initialLocalTime` (`{year, month, day, hour, minute, second}`), sent from the browser's local clock at every session create. The engine never reads a clock; the state names the outcome (`rtcInit: {applied, reason, localTime, boards}`), and the scenarios and the dive benchmark pin it off.
- `--pc-trace N out.u32le --pc-trace-after S` records a steady-state PC window.
- `--no-routine-accel` (run, bench, scenario) turns off the exact acceleration of the firmware's soft-float runtime routines, which is on by default; `--shadow-routine-accel` replays and interprets every accelerated call and compares them. Results are identical either way. The page's session-create JSON key is `routineAccel`.
- `bench --dive` builds a valid-tissue profile through firmware routes and measures dives at 20 and 30 m (`--dive-depths`, `--dive-seconds`, `--dive-png`, `--json`); `--verify-routine-accel` runs it with the acceleration on and off and requires identical checkpoints. `node web/bench-node.mjs --dive` does the same through the browser ABI.

## Publishing

- **The site** is built and deployed by [`.github/workflows/pages.yml`](.github/workflows/pages.yml) to GitHub Pages at <https://triton.divehub.ai> on every push to `main`. It runs the tests that need no firmware, then `deploy/build_site.py` assembles an allowlisted directory (the page, its modules and the engine module; no tests, scripts or firmware).
- **The firmware proxy** (`deploy/api/firmware.mjs`) is a small Vercel Function on **its own address**: Pages cannot run server code and the sources send no CORS headers. It accepts only an exact address allowlist, stores and caches nothing, and answers the page origin `https://triton.divehub.ai` plus localhost. The site learns its address at build time from the repository variable `FIRMWARE_PROXY_URL`; without it, "Load from URLs" is unavailable (the page never falls back to a same-origin `/api/firmware`).

Steps, commands and checks: [deploy/README.md](deploy/README.md).

## Status (2026-10-09)

- **Performance, TRITON** (Apple M1 Max):
  - WebAssembly in Node 24, 10 ms slices: boot about 9×, steady state at the surface 22–25×, menu redraw 15–16× real time with the exact idle-loop fast-forward; about 1.05× with it disabled.
  - Dive with valid tissues (20 m, Node): 11.8× on average and 5.6× during the main board's decompression bursts (natively 17× and 7.5×), with the exact routine acceleration and interpreter fast paths of DESIGN section 16. Before them: 2.85× and 1.1×.
  - In-app Chromium: paced 1.00×.
- **Fidelity against fresh Renode 1.17.0 runs of the TRITON SRECs, with the I2C idle-high fixture off:**
  - Handset-only boot: identical over the full 20 M-instruction PC trace and all snapshots and checkpoints, including DWT_CYCCNT, SRAM and GPIO.
  - Main board: SRAM byte-identical to main-only Renode runs at 32 marks between 0.1 and 1.37 s.
  - Dual wake: identical through 1.05 s. Afterwards instruction counts, LCD bytes, CAN IDs/payloads/order, UART text and storage are equal. SRAM and CAN-stamp differences are of the size Renode shows between its own runs.
  - The peripheral models replay recorded Renode transcripts identically; those recordings remain as regression tests in `testdata/` and `crates/*/tests/`. The Renode setup that produced them lives in a separate analysis workspace that is not public.
  - With the fixture on (the default), start-up ordering differs as intended.
- **NEPTUN** (observed earlier; its tests were dropped when it became optional): a 10.5 s dual boot releases the handset at 1.05 s and shows the B1 battery-selection screen, with no CPU faults and all 62 CAN frames forwarded.
- **Tests (2026-10-09):** the quick loop passes with 992 Rust tests (23 ignored) in about 36 s, 115 page tests in 2 s and 23 app tests in 16 s; the slow tier passes with the 10 TRITON scenarios (with and without the I2C fixture), the dive benchmark identity and the rest of the ignored tests.

## Known differences and limits

- **NEPTUN is a smoke-tested release.**
  - Main-board diagnostics that are proven only for TRITON (battery ready, mode, HAL tick and others) show as unavailable; the decompression report is `unknown` with the reason. NEPTUN has no tests in this repository (code paths and release table stay).
  - Cold boot is refused, because NEPTUN's main board does not request standby on that route.
  - The scenario suite runs only on TRITON.
  - Its main board spends steady state in loops the idle fast-forward does not recognize (the FreeRTOS idle task with a call, and a UART5 status poll). It runs at about 1.5× real time in Node and 2.4× natively. Improving this is deferred.
- **Dual-mode clock progress** follows each board's own CPU, which is deterministic. Renode advances both machines to their minimum progress, which is not. Expect dual-mode differences inside Renode's run-to-run envelope, for example a constant handset DWT_CYCCNT offset after 1.5 s in dual mode only.
- **Resets:** after a firmware-requested or watchdog reset the engine restarts the application at its vector table. Renode locks up, because the manufacturer bootloader is absent; `ResetStart::RenodeLiteral` reproduces that.
- **Fixtures:**
  - The main I2C idle-high lines (PB6/PB7/PB10/PB11) are an explicit fixture, on by default.
  - Output histories record commanded drive (PB15 enable changes exactly; motor commands sampled every 20 virtual ms; HUD commands every 50 virtual ms), not physical edges.
  - The emulated serial number (0–999 999 999) is synthetic; a fresh profile's is 1.
  - **EEPROM factory image** (always on, no option): when a new EEPROM is created (no `eeprom.bin`, or a stored image of 2048 bytes of `0xFF`), the engine writes the factory image once and saves it with the profile: 2048 bytes of `0xFF` with the records the firmware's first-boot defaults never write (serial number, oxygen-toxicity model and dose, the 32 tissue words, the no-fly records) set to the value the firmware's own code implies. The values are firmware-derived, not the manufacturer's factory image. An existing ("dirty") EEPROM is never touched, even when some of those records are still erased in it; an older profile with blank stored tissues still loads NaN, which the page reports with the next step (reset the profile). TRITON and NEPTUN (identical record table). The state says whether the session created its EEPROM from the image (`eepromFactoryInit: {applied, reason}`). The scenarios and the dive benchmark keep a blank EEPROM through an internal field. Details and the records left erased: [docs/eeprom.md](docs/eeprom.md).
  - **Start at the surface** (on by default; `startAtSurface`, `--no-start-at-surface`, a start option): every board creation sets both pressure inputs to the surface pressure plus the sensor offsets (depth 0); a new session, including a profile import or reset, also resets the three oxygen cells to their defaults. `inputs.json` keeps its format; the surface pressure is a session setting that the page remembers in the browser.
  - **Clock of a new profile** (`initialLocalTime`, `--initial-local-time`; the page sends it at every session create; none unless the host supplies one): a board with no saved RTC checkpoint and no EEPROM date seed starts its calendar from the host's local date and time instead of the engine's 2020-01-01 default, as if the device had been set at the factory. An existing checkpoint is never changed; afterwards the calendar advances in virtual time only. The state says what happened (`rtcInit`), `rtc-state.json` records the provenance `host-local-time` (an engine extension of the runner's format), and the scenarios and the dive benchmark pin it off. Reset all in the game clears the profile and starts a new session, so the clock starts again from the local time. [DESIGN.md](DESIGN.md) section 23.
  - The sensor inputs are fixtures. A fresh profile starts with both batteries at **4100 mV** (range 0 to 4200 mV; the Renode runner used 1500 mV, so the scenarios and recorded-evidence tests pin 1500 mV explicitly, and a saved profile keeps its stored values). The firmware compares the voltage with the battery type chosen in its wizard: at 4100 mV the B1 prompt is identical to the one at 1500 mV, but after choosing Alkaline the next start shows "Change battery" and the main board stands by (set about 1500 mV for Alkaline; Li-Ion 3.7V-18650 starts normally at 4100 mV). A synthetic reproduction, not a physical observation.
- **Not provided:** `--stock-pwm` (timers are exact without it) and the older `--demo-peer` synthetic fixture of the Renode setup.
- **Browsers:** background tabs are throttled by the browser, and the page reports when it cannot keep up. Chromium is verified; Safari and Firefox are untested.
- **Provenance paths:** comments and test data cite files such as `emulation/models/NGCAdc.cs` or `emulation/runtime/.../result.json`. They name files of the separate Renode-based analysis workspace, which is not public; the values the tests need are embedded in this repository.

## License

MIT, see [LICENSE](LICENSE). The STM32 peripheral models and parts of the timing framework are Rust ports of Renode sources (MIT, Copyright (c) Antmicro); see [licenses/README.md](licenses/README.md). The license does not cover the original firmware, which is not part of this repository.
