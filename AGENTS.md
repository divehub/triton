# Contributor and agent instructions

Work from the repository root. This repository holds the NGC Rust/WebAssembly emulator and nothing else: the Rust workspace in `crates/`, the browser app in `web/`, the publishing sources in `deploy/` and `.github/`, documentation in `docs/`, recorded regression data in `testdata/` and third-party notices in `licenses/`. The Renode-based analysis workspace the engine was validated against is separate and not public; comments that cite its files (`emulation/models/*.cs`, `emulation/run_emulator.py`, `emulation/runtime/...`) are provenance, not paths of this repository.

## Read first

1. `README.md` for commands, layout and limits; `DESIGN.md` for the contracts (timing model in sections 5–7, session API in section 14, repository split in section 15) and its work log.
2. `web/README.md` for the browser app and `deploy/README.md` for publishing (GitHub Pages site, Vercel firmware proxy).
3. `docs/releases.md` before touching release tables or firmware-specific addresses; `docs/renode-semantics.md` and `docs/framework.md` before changing timing, the peripheral framework or a model; `testdata/README.md` before touching recorded data.

The paired inputs are **main 5.8 / handset 65.3**, in two releases (TRITON, NEPTUN). Keep the roles distinct: central Main PCB versus arm-worn display/control PCB. Their application vectors start at `0x08004000`; the handset reset PC is `0x08008410` (TRITON), initial SP `0x20018000`. The supplied images omit the manufacturer bootloader. The handset MCU family matches STM32L4 Cortex-M4F; the exact part and panel remain unverified.

## Build and test

- `./cargo <args>` is the only way to run cargo (it works from any directory, runs from the repository root, uses `tools/rust` if present and otherwise the `cargo` on `PATH`). `rust-toolchain.toml` pins Rust 1.98.1 with `wasm32-unknown-unknown`.
- `./cargo test --workspace --release` (all crates), `node web/test-ui.mjs`, `node deploy/test-proxy.mjs`, `python3 deploy/test_build_site.py`, `python3 deploy/test_build_proxy.py` need no firmware. With firmware (`firmware/<release>/` or `NGC_FIRMWARE_DIR`) the suite also runs the real-image tests and `node web/test-node.mjs`; the slow tier adds `ngc-cli scenario all`.
- **Run every test and build command in the foreground, with a timeout. Never start background watchers or shells** (they linger after the tests finish).
- **Quick loop per area** (with firmware, about 36 s for all of Rust; the README "Tests" section has the table and the times):
  - CPU and FPU: `./cargo test -p armv7m -p armv7m-vfp --release`.
  - Peripherals and framework (golden replays of recorded Renode transcripts): `./cargo test -p emu-core -p stm32 --release`.
  - Session, actions, persistence, fixtures, outputs, serial, ABI, CLI: `./cargo test -p ngc -p ngc-wasm -p ngc-cli --release`; one file: `./cargo test -p ngc --release --test <name>`.
  - Page logic: `node web/test-ui.mjs`; against the real module (needs firmware): `python3 web/build.py` then `node web/test-node.mjs`.
  - Deploy: `node deploy/test-proxy.mjs`, `python3 deploy/test_build_site.py`, `python3 deploy/test_build_proxy.py`.
  - **Slow tier** (before a push that touches timing, the CPU, the acceleration or the session): `./cargo test --workspace --release -- --ignored`, `ngc-cli scenario all --main <abs path> --handset <abs path> --out <fresh dir>`, `ngc-cli bench --dive --verify-routine-accel`, `node web/test-node.mjs --slow`. Expensive end-to-end tests are `#[ignore = "slow: ..."]`; do not add a test that takes more than a few seconds to the quick loop.
  - NEPTUN is optional and has no tests; keep its code paths and release table.
- Use a private cargo target directory when several people or agents build at once: `./cargo test -p <crate> --release --target-dir target/<name>`.
- Keep every crate compiling at every save point; write new code in new files and add the `mod` line last.
- Measure performance before and after a change to a hot path (CPU loop, memory access, MMIO dispatch, event queue): `ngc-cli bench` natively and `web/bench-node.mjs` in V8. Read the performance sections of `DESIGN.md` (9, 13) before proposing a change.
- Use American English spelling in UI text, docs, comments and messages.

## Repository content

The repository versions authored code, tests, recorded test data, documentation and notices. It must never contain:

- **Firmware**: no SREC / S19 / BIN / HEX / ELF / APK / AAB file, no extracted or reconstructed image, no listing or dump that carries instruction bytes, and no content starting with `S0` or holding S-record lines. `.gitignore` ignores these and `/firmware/`; never force-add them. `deploy/build_site.py` and `deploy/build_proxy.py` refuse anything firmware-like and tests check it.
- Authenticated server responses, passwords, tokens or API keys, or local absolute paths.
- Generated output (`target/`, `web/pkg/`) and locally provisioned tools (`tools/rust`).

The recorded test data (`testdata/`, `crates/*/tests/**`) contains register-level transcripts, vectors, instruction-start bitmaps (positions only) and expected values. Keep it as it is; do not regenerate it unless its provenance is documented in `testdata/README.md`.

## Evidence discipline

- This is a **functional model, not a claim of physical accuracy**. Distinguish a proved branch/state defect, a conditional ordering hypothesis, a synthetic reproduction and a physical observation in every report. Every state and capture carries the `ngc-wasm/<version>` engine label; evidence from the Renode setup does not transfer to it automatically.
- Check consequential claims against original bytes and Thumb instructions, including literal pools, vtables and tail calls. Treat Ghidra function names and C-like output as reconstruction. Infinite RTOS waits, task loops or shared fields alone do not prove races.
- Fixture assumptions are explicit and stay explicit: handset ADC sample 400 gives the inferred board code `0x0201`; main analog reference 2500 mV, battery divider 1.68, oxygen gain 10, pressure PROM coefficients, the 60 Hz display TE and fixed clocks; the main I2C idle-high lines (PB6/PB7/PB10/PB11, on by default); the synthetic serial number (EEPROM offset 0 is synthetic storage and never identifies a physical unit); the start at the surface (on by default, switchable, named in the state: both pressure inputs at the surface pressure plus the sensor offsets at every board creation, a new session also with default oxygen cells). The original firmware loads NaN tissues from a restarted profile that never saved them and keeps the no-decompression limit at 99; `decoHealth` reports that state read-only, and the page tells the user to reset the profile (there is no repair of an existing EEPROM). The EEPROM factory image (always on, **no option**, named in the state as `eepromFactoryInit: {applied, reason}`; `docs/eeprom.md`): when a **new** EEPROM is created (no `eeprom.bin`, or an entirely erased image) it is written once and saved with the profile, giving the records that the firmware's first-boot defaults never write (serial 1, oxygen-toxicity model and dose, the tissue block, the no-fly records) values derived from the firmware's own code, never the manufacturer's factory image; an existing EEPROM is never touched. The scenario suite and the dive benchmark pin the image and the start at the surface off (`scenario::recorded_config`, through the internal `SessionConfig::eeprom_factory_init`).
- The page's Confirm button uses two overlapping 204.8 ms pulses staggered by 50 virtual ms, so the guest receives separate key events inside its 250-tick combination window. This offset is a functional fixture, not measured switch skew. If a menu confirmation fails, do not patch the guest key handler or inject RAM events to work around it.
- Oxygen calibration goes through the documented firmware protocol; never set application calibration flags or ppO2 RAM directly.
- Boot and power: the default dual boot restores saved RTC domains, then overrides main `RTC.BKP1R` with `0x32f0`, `PWR.SR1` with `0x104` and `RCC.CSR` with `0` to represent a handset wake from standby; the inferred main PE3 supply enable is polled every 50 virtual ms to release the handset CPU. Cold mode keeps the RTC calendar and backup words but supplies zero wake flags. Full electrical power, standby and reset behavior and CAN wire timing are not modeled; PWR, FLASH and FMC are simplified. Preserve these boundaries when interpreting concurrency or cold-boot symptoms.
- Profiles: EEPROM, sparse NOR and the per-board RTC checkpoint survive Restart, cold boot, Wake and reopening; RAM is recreated. The calendar advances in virtual time only (paused or closed means frozen). An invalid checkpoint stops startup and is never silently erased or replaced.

## Engine invariants

- Timing follows `DESIGN.md` section 5. Do not silently disable guest-visible timer counters or flags, slow ADC acquisition, lower the CPU MIPS or coarsen the synchronization quantum to make a test pass or run faster.
- Optimizations must be exact: the idle-loop fast-forward on and off, and native and WebAssembly builds, must produce identical state digests.
- New observation features (histories, summaries, tracing) must not perturb guest execution: no new clock entries, limit timers or chunk splits in a board's clock; sample at the system's quantum boundaries instead.
- Ported files keep their attribution header (`// Ported from Renode 1.17.0 <path> (MIT License, Copyright (c) Antmicro).`), and `licenses/` carries the notice. Do not copy tlib (LGPL) code.

## Dependencies and tools

- No crates.io dependencies. Python helpers use only the standard library; Node code uses only Node 24 built-ins. Announce any new tool or dependency in the pull request, with its source, version and hash; prefer existing tools.
- The web app has no build step beyond `web/build.py`, loads no external script, style or font, and has no inline script or style. Keep the Content-Security-Policy of `web/index.html` identical to `web/serve.py`'s (`deploy/build_site.py` fails otherwise).
- GitHub Actions: `.github/workflows/pages.yml` uses only GitHub's own actions. Do not add third-party actions without a reason and a pinned version.

## Publishing rules

- Pushes to `main` deploy the site to GitHub Pages. Run the tests listed above first.
- A new page module must be added to `SITE_FILES` in `deploy/build_site.py` on purpose; the assembly refuses unlisted modules and anything firmware-like.
- The firmware proxy's upstream allowlist (`deploy/api/firmware.mjs`) and the page-side copy (`web/firmware-url.js`) must agree; `deploy/test-proxy.mjs` checks every case. Do not widen the allowlist, the CORS origins or the size and time limits without updating the tests and `deploy/README.md`. The proxy stores, caches and logs nothing and must not be turned into a mirror of the firmware.
- The proxy address is a build-time setting (`FIRMWARE_PROXY_URL`, a repository variable). The page never assumes a same-origin `/api/firmware`.
- Never print or save tokens or passwords in logs, source, reports or provenance.

## Device scope

Static analysis and local emulation do not authorize flashing or changing a physical device. This project never flashes firmware, switches a device into a programming mode or applies a generated patch to hardware. Keep original firmware bytes unchanged; store any proposed modification as a separately identified artifact outside the firmware.
