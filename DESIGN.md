# NGC Rust/WebAssembly emulator — design and work plan

> **About this document.** It is the design and work log of the emulator as written during development by a planner and
> implementers working in work packages (CPU, FPU, BOARD, PER, SYS, WEB, RUST, ...). It is kept for its technical content:
> contracts, the timing model, decisions and the validation plan. In this repository file paths are relative to the
> repository root (during development the emulator lived in a subdirectory of a larger private repository, and earlier
> text names paths with that prefix). References to `emulation/models/*.cs`, `emulation/run_emulator.py`,
> `emulation/viewer.html`, `emulation/*.repl` and similar name files of the separate Renode-based analysis workspace (not public),
> which earlier text also called the `main` branch. Commit hashes of that workspace and local paths were removed.

Planner-owned document. Implementers: read sections 1–3 fully, then the sections for your work package (section 12; since 2026-10-08 section 15 takes precedence). If a contract here is wrong or insufficient, finish what you can and describe the needed change in your final report; do not silently diverge.

## 1. Goal and acceptance

Run the unchanged **main 5.8** and **handset 65.3** firmware, loaded from the two original SREC files, in a browser via WebAssembly, connected over CAN, with the features of the existing Renode viewer (`emulation/run_emulator.py`, `emulation/viewer.html`) except `--stock-pwm`.

- **Performance gate (hard):** sustained emulation in the browser must be **faster than 0.9× real time** (target ≥ 1.0×) for steady state, menu navigation and sensor changes; boot speed is reported. Both CPUs execute 100 000 000 instructions per virtual second in the reference (no WFI in steady state), so the engine must deliver that work, using exact optimizations (section 9) rather than changing the timing model. If measurements show this cannot be exceeded after optimization, the project stops.
- **Functional parity target:** the pinned Renode 1.17.0 platform in `emulation/handset.repl`, `emulation/main.repl`, `emulation/*.resc` and `emulation/models/*.cs`. This engine is a *new functional model*: Renode evidence does not transfer automatically. Every state/capture it produces carries `"engine": "ngc-wasm/<version>"`.
- **Final step:** the browser app asks the user for the two SREC files and validates them before booting.

## 2. Ground rules for implementers

1. Work only inside your checkout of this repository. **Never modify the separate Renode-based analysis workspace (not public)** — other workers use it. Reading its files (the Renode installation, recorded evidence such as `emulation/runtime/**`) was allowed.
2. **No git commands that change state** (commit, add, stash, checkout, reset, rebase, clean) for implementers in the original process: the planner handled git. Read-only `git status`/`git diff` was fine.
3. Build with the repository's cargo wrapper: `./cargo <args>`. It uses a toolchain in `tools/rust` when one is provisioned (it sets RUSTUP_HOME/CARGO_HOME there) and otherwise the cargo on PATH (`rust-toolchain.toml` pins Rust 1.98.1 with the wasm32 target). **No crates.io dependencies**, no global installs, no network package downloads. Python helpers: standard library only. Node v22 is used for the WebAssembly tests.
4. The agents' sandbox rejected complex shell commands. **One simple command per call**: no heredocs, no `cd … &&` chains, no `export VAR=$PWD`, no command substitution of paths into env vars. Files were created and edited with editor tools, using absolute paths.
5. Use a private cargo target directory per work package so concurrent agents don't block or break each other: `./cargo test -p <crate> --target-dir target/<wp-id>` (relative to the repository root, which the wrapper `cd`s into). Build only the crates you need (`-p`).
6. Edit only files you own (section 12). Pre-created module files are yours to fill. Public contracts defined by the planner (marked "Contract") may be extended, not renamed. Other agents build crates that depend on yours at the same time: **keep your crate compiling at every save point** (write new code in new files and add the `mod` line last; never leave a public signature half-edited). If a dependency crate owned by someone else fails to compile, wait a minute and retry rather than editing it.
7. Firmware (SREC/BIN) is never committed: it stays local and gitignored. Don't print or store secrets. *(Since 2026-10-08 this repository no longer produces Renode reference data; see section 15.)*
8. *(Historical, superseded by section 15: the pinned sources and harness were removed from this repository; the Renode emulator lives in the separate Renode-based analysis workspace (not public), the upstream sources at the URLs below.)* **Renode is the behavioral reference.** Port from the pinned Renode 1.17.0 sources (infrastructure commit `066a7f13c052215632d469c995c89aea37c573b1`, MIT). Put an attribution header on ported files: `// Ported from Renode 1.17.0 <path> (MIT License, Copyright (c) Antmicro).` When the ARM/ST manuals and Renode differ on firmware-visible behavior, follow Renode and mark it `// Renode parity: …`. Pinned sources (they were kept in the analysis workspace): `emulation/hardware-reference/*.cs`, `emulation/upstream/*.cs`, `emulation/references/*.cs`; others are at `https://raw.githubusercontent.com/renode/renode-infrastructure/066a7f13c052215632d469c995c89aea37c573b1/src/Emulator/...` (canonical copies were saved under `reference/renode-src/` while the harness existed).
9. Evidence discipline (AGENTS.md): never claim physical-device accuracy; distinguish model behavior from physical observation; keep fixture assumptions explicit.
10. Performance matters on every hot path (CPU loop, memory access, MMIO dispatch, event queue). Avoid allocation, `dyn` calls and hashing in per-instruction paths.

## 3. Inputs and boot facts

| | main 5.8 | handset 65.3 |
| --- | --- | --- |
| SREC | `firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec` | `firmware/TRITON-5.8-65.3/ngc_handset_65.3_TRITON.srec` |
| SREC SHA-256 | `838cb050fa572dddca3153f43a1768db0a0665db4cde0567749fb7be8d18d6ea` | `71a9af68de1d23d4f845784bcbf8ccf72dcd9888587e0da0125ff41c74ea1e03` |
| Records | S0 1, S3 11 671, S7 1 | S0 1, S3 44 152, S7 1 |
| Binary span | `0x08004000..0x08031938` (186 680 B) | `0x08004000..0x080b074c` (706 380 B) |
| Binary SHA-256 | `76af4ba51029afa93e788fa11bca74bad5dea7be7b14b0f8ee59baea5bfd014d` | `f9a85fb016081dae1e7e3c7e3007637889557df7b0e3a8142b574c21f91b9d57` |
| Reset PC / SP | `0x080213b8` / `0x20018000` | `0x08008410` / `0x20018000` |

Both images have segments `0x08004000..0x0800418c` and `0x08004190..end`; the 4-byte gap is filled with `0xFF` in the binary span. Renode loads the span with `LoadBinary` into a zero-initialized 1 MiB flash at `0x08000000`, so bytes outside the span are `0x00`. The 0x18C-byte vector table holds 99 entries (16 system + 83 IRQs). Boot (from `emulation/main.resc` / `handset.resc`): load span, write CAN MCR `0x40006400 = 0x00010000` through the CAN model before execution, VTOR `0x08004000`, SP `0x20018000`, PC as above (the vector table's reset entries are `0x080213b9` / `0x08008411`).

Both firmwares use FreeRTOS with the same 7-instruction idle loop (handset `0x0800d086..0x0800d092`, main `0x080276a2..0x080276ae`), ~2 100–2 200 VFP instructions each (incl. VDIV, VFMA/VFMS, VCVT), DSP instructions, LDREX/STREX, and no WFI in steady state.

## 4. Architecture

Cargo workspace at the repository root (edition 2021, no external crates):

| Crate | Role |
| --- | --- |
| `emu-core` | Time base (`Time` in ns, as Renode), Renode-exact clock entries, event queue, `Peripheral` trait, `Ctx`, signal routing, minimal JSON, logging. No CPU dependency. |
| `armv7m-vfp` | FPv4-SP decode/execute (re-exported as `armv7m::vfp`). Contract in `src/lib.rs`. Separate crate so the FPU and CPU work packages build independently. |
| `armv7m` | Cortex-M4F core, SCS (NVIC/SCB/SysTick/MPU regs/FPU ctrl), DWT. Contract in `src/lib.rs`. |
| `stm32` | STM32 peripheral models ported from Renode. |
| `ngc` | Board framework (memory map, MMIO dispatch, `CpuBus` impl, run loop), SREC/firmware identity, NGC models, handset/main assembly, dual `System`, runner fixtures, state JSON, persistence formats. |
| `ngc-cli` | Native runner: info/extract, run, bench, trace, scenario validation. |
| `ngc-wasm` | `extern "C"` WebAssembly API used by `web/` (browser worker) and `web/bench-node.mjs`. |

The browser app (`web/`): static HTML/JS/CSS served by a stdlib Python server; a dedicated Web Worker owns the WebAssembly instance and paces it against wall time; the page renders the LCD into a canvas at up to 60 Hz and implements the viewer controls. Profile storage uses OPFS/IndexedDB with explicit import/export.

## 5. Time model and scheduling (contract, Renode-faithful)

**Decision 2026-10-08** (after `docs/renode-semantics.md` §0/§13/§15): reproduce Renode 1.17.0 *single-machine* timing exactly, so a handset-only run can match Renode's instruction trace and any divergence identifies a bug, and dual runs fall inside Renode's own run-to-run envelope. This supersedes the earlier exact-rational/next-instruction design.

- **Time unit = 1 ns** (`emu_core::Time`, `TICKS_PER_SECOND = 1_000_000_000`, as Renode `TimeInterval`). 100 MIPS → `TICKS_PER_INSTRUCTION = 10`; `QUANTUM = 100_000`.
- **Clock entries** (`emu_core::clock`): every Renode `ClockEntry`-based timer — `LimitTimer`, STM32 timers and their channels, SysTick, DWT CYCCNT, RTC/IWDG prescalers, `ObtainManagedThread`, `ScheduleAction`, LCD TE — reproduces `ClockEntry`/`BaseClockSource` exactly (renode-semantics §3–4): `Value` + exact-fraction `ValueResiduum`, `Ratio = Step*Frequency/1e9`, ascending/descending, periodic/one-shot, event at the **ceil-ns** time, restart from that rounded ns with the overshoot discarded, residuum kept on `Value` writes and cleared on frequency changes, ManagedThread first firing `ceil(1e9/f)` after `Start()` keeping value/residuum, `ScheduleAction` callbacks receive the scheduling time. Entries that expire at the same instant are all updated first, then their handlers run in **entry creation order**.
- **Clock-source time vs CPU time** (renode-semantics §5.4): a machine's clock time advances only when its CPU reports progress — at the end of every `Cpu::run` chunk and at explicit syncs. MMIO side effects observe the clock time (`ctx.now()` = start of the current chunk or the last sync), not the instruction-exact time. Accesses that Renode performs after `cpu.SyncTime()` (STM32_Timer CNT read, SMS trigger/encoder writes, input-capture latch, DWT CYCCNT read, SysTick CVR read, `ScheduleAction`) are declared by the peripheral's access policy; the machine first advances clock time to the exact instruction time (firing due events) and then dispatches the access.
- **Chunks**: `Board::run_until(t)` runs the CPU from `now` until `until = min(next clock event, quantum end, t)`: the CPU executes the smallest number of instructions whose end time is ≥ `until` (an event at T takes effect before the first instruction starting at or after T; `floor((until-now)/10)` instructions plus one if a remainder is left, at least 1). A chunk also ends early at the **end of the current translation block** when a peripheral requests a return (`ctx.request_return()`, raised by every Renode `LimitTimer` setter and by `ScheduleAction`; surfaced to the core as `BUS_STOP_REQUESTED`), or on WFI sleep/halt/lockup. Afterwards clock time = CPU time and due events fire; `on_event` handlers run with `ctx.now()` = their event time.
- **Interrupt arbitration at translation-block boundaries** (tlib, renode-semantics §5.5): pending exceptions are taken only at TB boundaries — the core models tlib's TB partition: a TB starts at a chunk start or after the previous TB end and ends after any instruction tlib marks as a jump (every branch/PC write whether taken or not, `bx`/`blx`/`cbz`/`pop {pc}`/`ldm` with PC, exception return, WFI/WFE/SVC, CPS and the other tlib `is_jmp` cases — derive the complete list from `reference/renode-src/tlib/arch/arm/translate.c`), at the tlib page boundary, after 0x7FF instructions, and at the chunk end. Exception entry/return cost 0 instructions; return re-arbitrates before executing a thread instruction (tail-chaining). Validate with the REF micro vectors (`testdata/renode-micro-vectors.json`: `pendsv-tb-end`, `level-irq-reentry`, `systick-reload79999`, `wfi-*`, `tim6-*`, `timer-enable-lag`).
- **WFI** counts as one executed instruction; sleeping skips to the next clock event; Renode's masked-pending quirk (wake once per 100 µs slice when PRIMASK masks the pending IRQ) is reproduced.
- **IRQ lines**: peripheral IRQ outputs change NVIC inputs through the machine's IRQ-change queue (`BUS_IRQ_CHANGED`); the core applies them before the next instruction but arbitrates only at the next TB boundary.
- **Dual system (`System`)**: one thread, deterministic. Quanta of 100 µs from emulation start: main board to the boundary, then handset board to the boundary. Each board's clock time advances with its own CPU (Renode advances both to the minimum progress, which is nondeterministic; our order is a member of its measured envelope). CAN frames are stamped with the sender's clock time at the transmit write (Renode stamps with the chunk-lagged sender time) and delivered to the other board **at the quantum boundary** in (stamp, board order). UI/automation inputs are applied at quantum boundaries with their virtual timestamp recorded.
- **Fixture polling** (runner parity): at every multiple of 50 virtual ms, the system (a) releases the handset CPU once main GPIOE ODR bit 3 (`0x48001014 & 8`) is set — the released CPU starts executing **one quantum later** (Renode `DeferredEnabled`: first handset instruction at 1.0501 s, `ExecutedInstructions` 44 990 000 at 1.5 s); the handset machine's peripherals/clock keep running while its CPU is held — (`simultaneous_start` bypasses the gate), and (b) detects standby: main PWR CR1 (`0x40007000`) `& 7 == 3` and SCB.SCR (`0xE000ED10`) bit 2 → halt both CPUs, `standby = true`, running = false. Also stop with an error when handset PC is `0x0800598e` (interrupts-disabled error loop). Reference: `run_emulator.py` `run_interval`.

## 6. CPU core (work packages CPU and FPU)

Contract: `crates/armv7m/src/lib.rs` (`CpuBus`, `CpuConfig`, `Cpu` API, `RunExit`) and `crates/armv7m-vfp/src/lib.rs` (`armv7m::vfp`).

**ISA**: complete ARMv7E-M Thumb/Thumb-2: data processing (all shifts/rotations, modified immediates, flags incl. carry-out rules), multiply/divide (MUL/MLA/MLS, UMULL/SMULL/UMLAL/SMLAL/UMAAL, SDIV/UDIV with CCR.DIV_0_TRP), DSP (SMULxy/SMLAxy/SMLALxy/SMULWy/SMLAWy, SMMUL/SMMLA/SMMLS(R), SMUAD/SMUSD/SMLAD/SMLSD(X)/SMLALD/SMLSLD, QADD/QSUB/QDADD/QDSUB, SSAT/USAT(16), parallel add/sub incl. GE flags and SEL, USAD8/USADA8, PKHBT/PKHTB, extends with add), bitfield (BFI/BFC/UBFX/SBFX), CLZ/RBIT/REV/REV16/REVSH, loads/stores of all forms (imm/reg/literal/pre/post/unprivileged, LDRD/STRD, LDM/STM/PUSH/POP incl. PC loads with interworking), LDREX/STREX(B/H)/CLREX with a local exclusive monitor, TBB/TBH, CBZ/CBNZ, IT (correct ITSTATE advance, conditional execution of 16/32-bit ops inside IT, flag-setting rules inside IT), B/BL/BX/BLX, MRS/MSR (APSR/IPSR/EPSR/xPSR/MSP/PSP/PRIMASK/BASEPRI/BASEPRI_MAX/FAULTMASK/CONTROL), CPS, SVC, BKPT, WFI/WFE/SEV/YIELD/NOP/DMB/DSB/ISB, UDF. Coprocessor encodings go to `vfp::decode` (`NotVfp` → NOCP UsageFault).

**Exceptions** (ARMv7-M ARM B1.5): reset state, exception entry with 8-word or extended 26-word frames, 8-byte alignment (CCR.STKALIGN) with xPSR bit 9, EXC_RETURN handling (0xFFFFFFE1/E9/ED/F1/F9/FD), FPU lazy state preservation (FPCCR.ASPEN/LSPEN/LSPACT, FPCAR, CONTROL.FPCA) exactly as architected, tail-chaining and preemption by priority with AIRCR.PRIGROUP, BASEPRI/PRIMASK/FAULTMASK masking, SVCall/PendSV/SysTick/NMI/HardFault/MemManage/BusFault/UsageFault with escalation and CFSR/HFSR/MMFAR/BFAR, VECTKEY reset requests (report to board), lockup → `ExitReason::Lockup`.

**SCS** (`0xE000E000..0xE000EFFF`) handled inside the core with Renode NVIC.cs semantics (port it: `src/Emulator/Peripherals/Peripherals/IRQControllers/NVIC.cs`): ICTR, SysTick (CSR/RVR/CVR/CALIB at `systick_hz`; copy Renode's reload/period semantics exactly), NVIC ISER/ICER/ISPR/ICPR/IABR/IPR with `priority_mask`, STIR, SCB (CPUID, ICSR incl. PENDSVSET/PENDSTSET/VECTPENDING/RETTOBASE, VTOR, AIRCR, SCR, CCR, SHPR1-3, SHCSR, CFSR, HFSR, MMFAR, BFAR, CPACR), MPU registers (store/readback; MPU disabled unless firmware enables it — then report), FPCCR/FPCAR/FPDSCR. External IRQ line semantics (level vs pulse, re-pending while the line stays high) follow Renode NVIC.cs. DWT at `0xE0001000` per Renode `DWT.cs` (CYCCNT from virtual time at `dwt_hz`). Every other PPB address reads 0 / ignores writes (Renode has no model there); log once per address.

**Bus behavior**: unaligned LDR/STR/LDRH/STRH are permitted unless CCR.UNALIGN_TRP; the core calls the bus with the original width and address, and the board reproduces tlib's split for unaligned MMIO (loads = two aligned same-width reads merged; stores = byte writes from the highest address down; renode-semantics §7.5) while plain memory handles any alignment. LDM/STM/LDRD/STRD/LDREX/STREX require alignment (UsageFault UNALIGNED). Bus faults are never raised for unmapped addresses (Renode returns 0 and ignores writes).

**Timing** (section 5 is normative): one instruction = `ticks_per_instruction` (10 ns), including IT-skipped instructions; exception entry/return cost zero instructions. `run` stops at `until` rounded up to a whole instruction, or at the end of the current TB on `BUS_STOP_REQUESTED`. Pending exceptions are arbitrated only at TB boundaries (tlib TB partition). SysTick and DWT use `emu_core::clock` (Renode `ClockEntry` semantics: SysTick period = RELOAD ticks after the first expiry, first period from the last CVR write or 0xFFFFFF, ceil-ns events, COUNTFLAG cleared by any CSR read; renode-semantics §9.5); SysTick is a core-internal deadline included in run bounding and `next_internal_deadline`; SysTick CVR and DWT CYCCNT reads sync to the exact instruction time.

**Performance design**: predecode cache over `code_region` bytes (flash) — decode each halfword address once into a compact `Copy` op (≤ 16 bytes), dispatch with a dense `match`. Keep NZCV in registers/fields cheaply; check a single "attention" flag per instruction for pending exceptions/IRQ changes/stop; poll `take_notifications` after MMIO-capable accesses. Target: ≥ 250 M instructions/s native release on representative integer code.

**Exact idle-loop fast-forward (required, switchable)**: when a backward branch closes a short loop (≤ 32 instructions, target within 128 bytes), classify the loop body from decoded ops; only ALU/compare/move, loads, branches, IT and barrier ops are allowed (no stores, no exclusive ops, no system-register writes, no VFP, no SVC/WFI/BKPT). At the loop head, snapshot R0–R14, APSR (NZCVQ, GE) and require ITSTATE = 0; execute one full iteration normally while checking every load address with `is_plain_memory`; if the next arrival at the head reproduces the identical snapshot and no exception was pending or taken, the loop is a fixed point: skip `k = remaining_budget / iteration_length` whole iterations by adding `k * iteration_length` to the instruction count/time, then continue normally. Exact because no event can occur before `until` and no other agent writes plain memory before then. `Cpu::set_idle_fast_forward(bool)` must exist; A/B runs with it on/off must produce identical state.

**Tracing**: an opt-in PC/instruction trace (bounded buffer or callback) with zero cost when disabled, for differential comparison with Renode traces.

**FPU** (`crates/armv7m-vfp`): all FPv4-SP encodings (VLDR/VSTR S/D, VLDM/VSTM/VPUSH/VPOP, VMOV all forms incl. VFPExpandImm and core↔S/D pairs, VMRS/VMSR FPSCR and APSR_nzcv, VADD/VSUB/VMUL/VNMUL/VDIV/VSQRT/VABS/VNEG, VMLA/VMLS/VNMLA/VNMLS (separately rounded), VFMA/VFMS/VFNMA/VFNMS (fused), VCMP/VCMPE incl. with zero, VCVT f32↔s32/u32 (round-to-zero and VCVTR FPSCR rounding), fixed-point VCVT, VCVTB/VCVTT half precision). Double-precision arithmetic is UNDEFINED on FPv4-SP. Results must be bit-exact ARM: correctly rounded per FPSCR.RMode (all four modes), FZ and DN honored, ARM NaN propagation (SNaN first, quieting, default NaN `0x7FC00000`), saturating conversions with IOC, and FPSCR cumulative flags (IOC/DZC/OFC/UFC/IXC/IDC). Never rely on host NaN payload behavior. WebAssembly has no scalar fused multiply-add: implement FMA exactly (e.g. f64 product + error-free transformation, or integer/softfloat) and test against native `f32::mul_add` (exact on aarch64) over millions of random and edge-case inputs.

## 7. Board framework (work package BOARD)

Contract to be authored by BOARD (in `emu-core` and `crates/ngc/src/board.rs`), following this outline:

- `Peripheral` trait (object-safe): `name`, `reset`, `read(offset, Width, &mut Ctx) -> u32`, `write(offset, Width, value, &mut Ctx)`, `on_event(token: u64, scheduled: Time, &mut Ctx)`, `on_input(line, level, &mut Ctx)`, `summary() -> String`, `as_any/as_any_mut`. `Width` = 1/2/4 bytes.
- `Ctx`: `now()`, `schedule_at(time, token) -> EventId`, `cancel(EventId)`, `set_output(line, level)` (Renode GPIO semantics: deliver only on level change, synchronously before control returns to the CPU; `connect` pushes the current level immediately), `mem_read/mem_write(addr, Width)` (system-bus access with MMIO side effects, for DMA), `request_cpu_stop()`, logging/`warn_once`.
- Peripherals live in `Vec<Option<Box<dyn Peripheral>>>`; while one runs it is taken out of its slot, so `Ctx` can reach the rest of the board (other peripherals, memory). Re-entrant access to the running peripheral is a logged error.
- Signal nets: `connect((src, line), Target::Irq(n) | Target::Input(dst, line))`, one source to many targets, mirroring `.repl` `->` lines and `|` fan-out.
- MMIO dispatch: fast table lookup by address → `(PeriphId, base)`; plain memory (flash, SRAM1 `0x20000000` 96 KiB, SRAM2 `0x10000000` 32 KiB) handled inline in the `CpuBus` impl. `ArrayMemory` regions (PWR `0x40007000`, FLASH `0x40022000`, FMC `0xa0000000`, SYSCFG `0x40010000`, each per `.repl` size) read back what was written. Unmapped: read 0, ignore write, log once per address (Renode parity). Sub-word/unaligned access translation follows Renode's sysbus rules (document them in `docs/renode-semantics.md`).
- Run loop, IRQ-change queue, stop requests and exact MMIO time per section 5. Debug `peek/poke` of memory and peripherals without side effects for state snapshots.
- Firmware: SREC parser (S0/S1/S2/S3/S5/S7/S8/S9, checksum validation, overlap/contiguity checks), SHA-256 (own implementation, tested with standard vectors), identity check against section 3 hashes (SREC and reconstructed span), span reconstruction with `0xFF` gap fill, flash image builder.
- `ngc-cli info --main <srec> --handset <srec>` and `ngc-cli extract-bin --main … --handset … --out-dir …` (span `.bin` files identical to Renode's inputs).

## 8. Peripheral models (work packages PER1–PER4)

Port the exact Renode behavior used by these boards. Each model gets unit tests (register behavior, timing, IRQ/DMA) and a `summary()` matching the information in the C# `Summary` where one exists.

| File | Model | Source |
| --- | --- | --- |
| `stm32/src/gpio.rs` | `GPIOPort.STM32_GPIOPort` (numberOfAFs 16, modeResetValue param) | renode-infrastructure `.../GPIOPort/STM32_GPIOPort.cs` |
| `stm32/src/exti.rs` | `IRQControllers.STM32F4_EXTI` (numberOfOutputLines 24) | `.../IRQControllers/STM32F4_EXTI.cs` (+ base) |
| `stm32/src/combined_input.rs` | `Miscellaneous.CombinedInput` | `.../Miscellaneous/CombinedInput.cs` |
| `stm32/src/timer.rs` | `Timers.STM32_Timer` incl. input capture, PWM/compare, one-pulse, update/UIF semantics (wrap at ARR as in Renode), CNT computed from time; no per-period events unless an IRQ/DMA/capture/observable output needs them (generalized arithmetic mode — section 9) | `emulation/upstream/STM32_Timer.cs` + `LimitTimer` |
| `stm32/src/usart.rs` | `UART.STM32F7_USART` + passive TX capture hook | `emulation/hardware-reference/renode-1.17-STM32F7_USART.cs` |
| `stm32/src/can.rs` | `CAN.STMCAN` (bxCAN, filters, mailboxes, FIFOs, 4 IRQs, `FrameSent`/`OnFrameReceived`) | `emulation/hardware-reference/renode-1.17-STMCAN.cs` |
| `stm32/src/i2c.rs` | `I2C.STM32F7_I2C` + I2C target trait | `emulation/hardware-reference/renode-1.17-STM32F7_I2C.cs` |
| `stm32/src/rtc.rs` | `Timers.STM32F4_RTC` (wakeupTimerFrequency param) | `emulation/hardware-reference/renode-1.17-STM32F4_RTC.cs` |
| `stm32/src/iwdg.rs` | `Timers.STM32_IndependentWatchdog` (32 kHz) | `.../Timers/STM32_IndependentWatchdog.cs` |
| `stm32/src/dma.rs` | `DMA.STM32LDMA` (7 channels, request inputs, IRQs) | `.../DMA/STM32LDMA.cs` |
| `stm32/src/crc.rs` | `CRC.STM32_CRC` (series F0, configurablePoly) | `emulation/hardware-reference/renode-1.17-STM32_CRC.cs` |
| `stm32/src/rng.rs` | `Miscellaneous.STM32_RNG` (series F7) | `emulation/hardware-reference/renode-1.17-STM32_RNG.cs` |
| `ngc/src/models/clock_control.rs` | `NGCClockControl` | `emulation/models/NGCClockControl.cs` |
| `ngc/src/models/lcd.rs` | `NGCParallelLCD` (240×320 GRAM, MADCTL, TE 120 half-periods/s, RGB565, PPM export identical bytes) | `emulation/models/NGCParallelLCD.cs` + `references/renode-1.17-AutoRepaintingVideo.cs` |
| `ngc/src/models/adc_handset.rs` | `NGCAdc` | `emulation/models/NGCAdc.cs` |
| `ngc/src/models/adc_main.rs` | `NGCMainADC` (incl. deterministic noise hash) | `emulation/models/NGCMainADC.cs` |
| `ngc/src/models/buttons.rs` | `NGCHandsetButtons` (100 µs stimulus clock, Press/Pulse/Confirm stagger) | `emulation/models/NGCHandsetButtons.cs` |
| `ngc/src/models/eeprom.rs` | `NGCEepromStore`/`NGCEepromBank` | `emulation/models/NGCEeprom.cs` |
| `ngc/src/models/ms5837.rs` | `NGCMS5837` | `emulation/models/NGCMS5837.cs` |
| `ngc/src/models/qspi.rs` | `NGCQuadSPI` (sparse `nor.ngc` format byte-compatible) | `emulation/models/NGCQuadSPI.cs` |
| `ngc/src/models/telemetry.rs` | `NGCBoardTelemetry` outputs JSON | `emulation/models/NGCBoardTelemetry.cs` |
| `ngc/src/models/uart_capture.rs` | `NGCUartCapture` (16 KiB tails, totals, last TX time) | `emulation/models/NGCBoardTelemetry.cs` |
| `ngc/src/models/can_link.rs` | `NGCCANLink` (connected, DropId, trace TSV format, null-payload normalization) | `emulation/models/NGCCANLink.cs` |

Renode `ObtainManagedThread(f)` / `ScheduleAction` / `LimitTimer` become self-rescheduling events at exact periods; replicate their start/phase semantics from the Renode time sources.

## 9. Performance plan

1. Interpreter with predecode cache, inlined plain-memory paths, monomorphized bus.
2. Exact idle-loop fast-forward (section 6) — the decisive lever: steady-state PCs of both boards sit in the FreeRTOS idle loop.
3. Event-driven timers: compute CNT/flags from time on access; schedule events only for enabled IRQs, DMA requests, capture inputs or explicitly observed outputs (this generalizes the Renode runner's arithmetic PWM mode; no stock per-period mode is provided).
4. Cheap quantum boundaries (no allocation, no thread handoff).
5. Measure natively (`ngc-cli bench`) and in V8 (`web/bench-node.mjs`), then in the browser. Benchmark shape mirrors `emulation/performance/benchmark_system.py`: boot to 4.5 s, three 1 s steady intervals, a button-redraw interval.
6. Only if still short: WebAssembly block compilation of hot code (decided by the planner after measurement).

## 10. System features (work package SYS) — runner parity

Board assembly exactly per `handset.repl`/`main.repl` (addresses, sizes, parameters, connections). `System` (dual or handset-only) with:

- Boot modes: handset-wake (default: restore RTC checkpoint, then main RTC.BKP1R `0x40002854 = 0x32f0`, PWR.SR1 `0x40007010 = 0x104`, RCC.CSR `0x40021094 = 0`), cold (checkpoint, zero wake flags), `simultaneous_start`; handset held halted until the 50 ms PE3 poll releases it.
- Inputs (ranges/defaults from `run_emulator.py` `INPUT_DEFAULTS`/`INPUT_RANGES`) applied through model methods; buttons Up/Down (orientation byte `0x20000740` swaps masks unless it equals 2) and Confirm (staggered); CAN connect/DropId; serial fixture (EEPROM byte 254 must be `0xa3`; write u32 at offset 0; restart); LED color labels; Restart / Cold / Wake (recreate boards; keep EEPROM, NOR and RTC checkpoint); pause/resume/step (2×50 ms)/advance (≤ 20 s).
- State snapshot JSON with the runner's fields (`virtualTime`, `pc`, `mainPC`, `lcdSummary`, `buttonSummary`, `canSummary`, `mainBatteryReady` from `0x200042a1`, `inputs`, `adcSummary`, `storageSummary`, `flashSummary`, `serialNumber`, `handsetPowered`, `hardwareOutputs`, `uartConsole`, `frameReady`, `standby`, `bootMode`, `rtcPersistence`, `error`, …) plus `engine`, `realtimeFactor`.
- Persistence formats byte-compatible with the Renode runner: `eeprom.bin`, `nor.ngc`, `rtc-state.json` (`emulation/rtc_persistence.py`, versioned, strict validation, legacy EEPROM seed), `inputs.json`, `led-colors.json`.
- Evidence capture: `state.json`, `lcd.png`, `can-trace.tsv`.

## 11. Validation plan

- **Reference data (work package REF)**: fresh Renode 1.17.0 runs from the same SRECs, using the Renode installation of the analysis workspace read-only and unique ports (≥ 18900): (a) handset-only boot PC trace for the first ~20 M instructions plus periodic register snapshots; (b) dual handset-wake run with checkpoints at 1.0, 2.0, 3.0, 4.5 s (CPU registers, instruction counts, SRAM dumps, LCD PPM, CAN trace, fault registers, HAL tick, TCB pointers, UART tails) and a 1 s steady interval to 5.5 s. Existing evidence of the analysis workspace: `emulation/main-boot/dual-handset-wake/result.json`, `emulation/runtime/performance/arithmetic-sustained-20261007/result.json` (LCD SHA-256 `62c3a30e…` at 4.75–7.5 s, 50 CAN frames by 4.5 s, HAL tick 4504, handset release 1.05 s, zero CFSR/HFSR).
- **Differential checks**: trace prefix equality until the first explained divergence; checkpoint equality of functional state (CAN payload/ID order, LCD bytes, HAL tick, mode, readiness, TCB pointers, fault registers). Timing-level differences within Renode's own run-to-run variation (CAN timestamps up to ~138 µs, idle PCs) are acceptable and must be reported, not hidden.
- **Scenario suite** (`ngc-cli scenario …`): dual wake to B1 prompt; button capture (accepted 250-count pulse, rejected 100-count pulse); diluent menu confirm; clock/storage retention across Restart/cold/Wake/reopen; outputs/UART; CAN loss; idle fast-forward on/off identity.
- **Performance**: section 9 measurements recorded as JSON with configuration, host, build hash.

## 12. Work packages and file ownership

| WP | Scope | Owns |
| --- | --- | --- |
| Planner | Design, workspace, integration decisions, git | `DESIGN.md`, `{Cargo.toml,Cargo.lock,rust-toolchain.toml,cargo,README.md}`, `.gitignore`, `tools/README.md` |
| CPU | Integer core, exceptions, SCS, DWT, predecode, idle fast-forward, tracing | `crates/armv7m/**` |
| FPU | FPv4-SP decode/execute, IEEE helpers | `crates/armv7m-vfp/**` |
| REF | Renode semantics doc, pinned source copies, reference harness + data | `reference/**` (removed from this repository), `docs/renode-semantics.md`, generated `firmware.bin` span files (ignored) |
| BOARD | Framework, firmware loading, CLI skeleton | `crates/emu-core/**`, `crates/ngc/src/{lib.rs,board.rs,bus.rs,memory.rs,srec.rs,sha256.rs,firmware.rs}`, `crates/ngc-cli/**` (until SYS) |
| PER1–PER4 | Peripheral models (assigned per file at dispatch) | listed files in section 8 |
| SYS | Board assembly, System, fixtures, state, persistence, CLI run/bench/scenarios | `crates/ngc/src/{handset.rs,main_board.rs,system.rs,fixtures.rs,state.rs,persistence.rs}`, `crates/ngc-cli/**` |
| WEB | WebAssembly API, browser app, Node bench | `crates/ngc-wasm/**`, `web/**` |

Status and decisions are tracked in section 13.

## 13. Status log

- 2026-10-07: workspace, toolchain (`tools/rust`, Rust 1.98.1 + wasm32), contracts for time base, `CpuBus`/`Cpu`, VFP interface. Wave 1 dispatched: CPU, FPU, REF, BOARD.
- 2026-10-08: BOARD done (framework, `AccessPolicy` = Renode `AllowedTranslations`, deferred-but-synchronous signal delivery, firmware identity, CLI info/extract-bin). REF semantics findings adopted: section 5 rewritten to Renode-faithful timing (ns time base, ClockEntry rounding, clock-source lag, TB-boundary interrupts, one-quantum handset release delay). BOARD resumed for the time-base/clock/lag changes; CPU informed of TB semantics. Wave 2a dispatched (untimed models): PER-A (gpio, exti, combined_input, crc, rng, clock_control, adc_handset), PER-B (usart, uart_capture, can, can_link, dma), PER-C (i2c + target trait, eeprom). Wave 2b (timed models) waits for `emu_core::clock`: PER-D (timer, iwdg, rtc, buttons, telemetry), PER-E (lcd, adc_main, ms5837, qspi).
- API rate limit interrupted CPU, BOARD, PER-A/B/C mid-task; all resumed from their transcripts. PER-C done: `Stm32F7I2c` + `I2cTarget` trait, EEPROM store/banks; a Renode replay harness (`crates/ngc/tests/renode_i2c_eeprom`) shows 41 scenarios / 14 331 operations identical to Renode's models. Renode parity kept: no NACKF for absent addresses (unimplemented tag in Renode). PER-D dispatched (timer, iwdg, rtc, buttons, telemetry); PER-E follows when concurrency allows.
- PER-A done: GPIO (incl. AF input lines), EXTI, CombinedInput, CRC, RNG (SplitMix64, fixed seed; values differ from Renode), NGC clock control, handset ADC; plus `stm32/src/gpio/regfw.rs`, a port of Renode's register framework (crate-private for now; candidate to move into emu-core). Golden replays recorded from private Renode runs (GPIO/EXTI 140+, CRC 760, RNG 47, RCC 60, ADC 67 steps) match byte for byte incl. log text and NVIC levels. PER-E dispatched (lcd, adc_main, ms5837, qspi).
- BOARD (timing rework) done: ns time base, `emu_core::clock` (ClockEntry/LocalClock/LimitTimer/ManagedThread/schedule_action; vector tests for SysTick reload79999 and TIM6 psc0/psc2 plus a randomized per-ns oracle), clock-source lag with `sync_registers()`, `request_return`, Renode chunk planning `max(1, floor(d/10))` with a 1-instruction chunk across non-aligned events, WFI skip, halted cores keep advancing their clock. MMIO ≈ 7.6 ns/access; idle-loop benchmark 566× real time. Known deviations: zero-time limits fire after the setter returns (not re-entrantly); Renode's stale one-shot chunk shortening not reproduced (no event time differs). Open for SYS: one-quantum handset release delay, CAN stamps/delivery order.
- PER-B done: USART (TX hook at TDR write, ISR reset 0x200000C0), bxCAN (Renode MCR handshake, first-pair-only mask filters, 14-bit EXID compare, …), DMA (8 channels/7 decoded), UART capture (snapshot JSON byte-identical incl. `1E-06` formatting), CAN link (`pump` in (stamp, main-before-handset, transmit order); replays both dual-wake CAN traces byte for byte). Fuzzed differential check against real Renode (~370 scenarios) matched. SYS-CORE dispatched (board assembly, System, fixtures, CLI run/bench/compare-reference, minimal wasm bench + `web/bench-node.mjs`); the state/persistence/actions package and the browser app follow.
- CPU done: 129 tests; Renode micro vectors systick-reload79999, pendsv-tb-end, wfi-systick, wfi-primask, wfi-systick-cyccnt, level-irq-reentry match; static decode of all Ghidra-listed instructions (handset 130 737, main 57 950); flat-bus handset run equals Renode's PC trace for 93 355 instructions (first divergence = TIM6 IRQ needing the timer model). Native release: 327 M instr/s idle loop (fast-forward off), 269 M mixed, 467 M NOP; fast-forward skips 99.9 % of idle instructions with verification. TB partition incl. 1 KiB page rule; tlib TB-cache history (cut-TB persistence) not modeled (matters only for straight runs ≳ 500 instructions). Added `CpuBus::sync_time(icount)` (implemented by SYS-CORE in `bus.rs`).
- PER-E done: LCD (2.1–2.4 ns/pixel in-model, 12–16 ns through bus dispatch; RGBA framebuffer + `frame_version`; PPM byte-identical — all 24 reference `lcd.ppm` images incl. 4.5 s `62c3a30e…` reproduced), main ADC (tick split into two same-instant events), MS5837 (`schedule_action` conversion timing, ±1 ns boundaries match), QSPI (`nor.ngc` round-trips both dual-wake files byte-identically). Renode replay: 70 scenarios / 17 002 operations identical (+~183 000 in local random sweeps). Note: Renode's monitor passes runner doubles through f32 — apply the same rounding to inputs for bit parity.
- SYS-CORE done: boards assembled from the `.repl` files in declaration order, deterministic dual `System` with fixtures, CLI `run`/`bench`/`compare-reference`/`disasm`, minimal wasm API + `web/bench-node.mjs`. Workspace: 849 tests pass. **Handset-only: PC trace identical to Renode over all 20 M reference instructions** (incl. the TIM3 button ISR), 200/200 1 ms snapshots and 74/74 register points match. **Dual wake 0.5–5.5 s: instruction counts, LCD hash/summary, application variables, CAN IDs/payloads/order, UART text, final EEPROM/NOR SHA-256 all equal**; main SRAM byte-identical to Renode until 746 ms. Explained remaining deviations: (1) Renode `SyncTime` sees the time at the start of the current TB (fix sent to CPU); (2) TIM2/TIM15 event elision moves one chunk boundary before Renode's arithmetic mode engages (fix sent to PER-D); (3) three register bits (FPCCR b3, AIRCR b13, reset xPSR.Z; fix sent to CPU); (4) after the 1.05 s handset join, Renode's own thread race (CAN stamps ±84.8 µs vs Renode's run-to-run ±79.6 µs).
- **Performance gate PASSED** (2026-10-08, Apple M1 Max): Node v22 (V8) WebAssembly with exact idle fast-forward: boot to 4.5 s **9.3×** real time, steady state **17.2–19.2×**, menu redraw with real TIM3 button presses **15.4×**; even with fast-forward off: 1.20× / 1.09× / 1.07×. Native: 15.6–40× (fast-forward on), 1.65–1.84× (off). Native and wasm state digests are identical, and fast-forward on/off digests are identical. Next: FEATURES (session API, actions, state, persistence, capture, resets, scenarios) and WEB (browser app) in parallel against section 14. Planner re-measurement (Node v22, 10 ms slices like the browser worker, 4 agents compiling concurrently): fast-forward on — boot 9.52×, steady 18.05/19.45/18.52×, menu redraw 15.84×; fast-forward off — boot 1.23×, steady 1.10×, menu 1.07×; exactly 100 000 000 instructions per board per virtual second.

- PER-D done: STM32 timer with scheduling policies — `NgcArithmeticPwm` (default; faithful port of Renode's `NGCLazyPwmTimer` arithmetic mode incl. its phantom-limit chunk splits; for handset TIM2/TIM15 only), `Stock` (every limit an event; required for all plain `STM32_Timer` instances: main TIM4/6/7, handset TIM3/6 — FEATURES applies it in fixtures), `Observable` (elide everything provable). Handset DWT_CYCCNT and its stored sample now equal Renode at 1/2/3/3.9 s. RTC (checkpoint API mirroring `rtc_persistence.py`), IWDG (`take_reset_request`; Renode resets at the end of the containing quantum), buttons, telemetry. Renode golden replays: 163 scenarios (7 018 reads, 40 750 edges) identical, plus 810 soak scenarios. Known residual: a 2-cycle CYCCNT difference in one `micro_pwm` probe because the phantom limit is per timer rather than global `nearestLimitIn` (would need emu-core registry support).

- WEB done: hand-written wasm ABI over `Session`, browser app (`web/`: entry screen verifying both SRECs by SHA-256 and content, worker pacing in 10 ms virtual slices with a 250 ms catch-up cap, frames on version change ≤ 60 fps, state 5 Hz, OPFS profile, evidence/profile zips, parity with `viewer.html` plus speed selector), `serve.py` (127.0.0.1 only, Host allowlist; opt-in `--dev-firmware DIR` serving exactly the two SREC names for automated checks), `build.py`, 17 Node tests. Planner smoke test in the in-app Chromium: both SRECs verified (9/9 checks), boot reaches the B1 battery-type screen, a Down press moves the selection through TIM3 capture, measured 9.87 virtual s per 10.00 wall s at 1× pacing with reported engine capacity 9.4× (two agents compiling concurrently). Background tabs are throttled by the browser (capacity 4–5.6×, flagged in the UI).

- CPU follow-up done: bus accesses, `sync_time`, SysTick CVR and DWT CYCCNT use the instruction count at the start of the current translation block (tlib only counts completed blocks), AIRCR bit 13, reset xPSR 0x41000000, tlib-style FPCCR storage, and Renode's persisted cut-block case (`CutHead`). Results: **main SRAM1/SRAM2 byte-identical to Renode main-only runs at all 32 marks from 100 ms to 1 370 ms; handset-only fully identical** (20 M-instruction trace, 200 snapshots, 74 points, checkpoints incl. DWT_CYCCNT/SRAM/GPIO); dual-wake identical at 0.5/1/1.05 s, afterwards mostly inside Renode's run-to-run envelope (CAN race). Open: dual-wake handset DWT_CYCCNT constant +3 763 cycles from 1.5 s (handset-only is exact) — re-check after FEATURES switches plain timers to `Stock`. Throughput: idle loop 314, mixed 261.6 M instr/s native. Docs to align: framework.md §2.2/§13/§18, `BusView`/`access_trace` docs (block-start count), renode-semantics §5.4.

- FEATURES done: `Session` (all 15 runner actions with the runner's messages, state JSON, f32-rounded inputs, Renode-style machine reset at the end of the containing quantum, RTC checkpoint/legacy seed, byte-compatible profile files, PNG/zip-ready captures), plain STM32 timers switched to `Stock` (dual-wake now identical at 0.5/1/1.05 s), `ngc-cli scenario` with 10 scenarios (dual wake, button capture, battery setup, diluent menu, clock/storage, outputs/UART, CAN loss, cold/wake, machine reset, fast-forward identity) compared against recorded Renode evidence. Reset default restarts the application (Renode locks up without the bootloader; `ResetStart::RenodeLiteral` reproduces it).
- 2026-10-08 planner verification: `cargo test --workspace --release` 927 passed / 0 failed / 12 ignored; `web/test-node.mjs` 17/17; in-app Chromium end to end — both SRECs verified, B1 and B2 battery selection over CAN, dive screen, sensor input (O2 cell 1 = 12.5 mV) shown by the handset, Restart keeps EEPROM/inputs, page reload restores the OPFS profile, main UART4 diagnostics in the console, pacing 1:1 in six controlled boot measurements and 1.00× reported with 5–9× engine capacity. Two early uncontrolled readings suggested the virtual clock had run ahead of wall time after boot; not reproduced. Out-of-scope edit to `emulation/viewer.html` (an agent copied the analysis workspace's newer serial limit) reverted; test-data files added to the `.gitignore` allowlist.
- 2026-10-08: first commit of the emulator. Split decided (section 15): this repository keeps only the WebAssembly emulator; Renode material removed (it lives in the analysis workspace). Dispatched RUST (trim, then engine parity) and WEB (main-viewer UI parity, releases).
- RUST done:
  - **Trim:** `compare-reference`, the tests that read `reference/data`, and the Renode-side golden generators are removed.
    - Recorded data now lives in `testdata/` and `crates/*/tests/**`. The Ghidra decode coverage is kept as instruction-start bitmaps: no bytes or mnemonics, and 57 950 + 130 737 starts still decode.
    - Scenarios embed their Renode-runner expectations with provenance.
  - **15.3 a–e implemented:**
    - Output histories in the analysis workspace's exact `ActivityJson`, sampled at quantum boundaries, with no change to the guest-state digest.
    - `outputHistoryEpoch` is `<historyNonce>-<generation>`; the generation also advances on a machine reset.
    - The I2C idle-high fixture is on by default. Older Renode comparisons are binding only with it off, and the scenarios run both ways.
    - The serial number accepts 9 digits.
    - TRITON and NEPTUN form a release table (see `docs/releases.md`). NEPTUN main diagnostics are reported unavailable: it is a different build with no instruction-identical counterpart. Cold boot is refused on NEPTUN, and the scenarios are TRITON-only.
  - **NEPTUN speed:** the main board's fast-forward skips about 0%, because steady state is spent in the FreeRTOS idle task with a call plus a UART5 status poll. The user deferred NEPTUN speed; a call-aware fast-forward prototype that reached about 96% skip is set aside, outside the repository.
  - 945 tests pass and 12 are ignored; native and wasm digests are identical, and fast-forward on/off identity holds for both releases.
- WEB done:
  - Basic view with immediate simulated conditions, and an Advanced panel (closed by default).
  - Replay pulses using the analysis workspace's rules.
  - Release-aware entry screen, with mixed pairs refused and a separate OPFS profile per release (TRITON keeps `ngc-wasm/profile/`).
  - New modules `sensors.js`, `conditions.js`, `replay.js`, `keys.js`, `releases.js`; `test-ui.mjs` (76 tests, 49 ported from the analysis workspace) and `test-node.mjs` (22 tests).
- Planner check:
  - Workspace tests 945/0/12 and all 10 TRITON scenarios pass.
  - Node TRITON speed: boot 8.9×, steady 16.7–18.2×, menu 15.4×.
  - In Chromium, the basic view boots to B1 at 1.00×. A fresh boot did not show the LCD until much later, although the engine had drawn it by about 2.7 s; after Restart it worked. Sent back to WEB.
- WEB fix:
  - **Causes:** the worker sent no frames to a hidden page (the in-app Chromium pane always reports `hidden`), and the first frame was dropped because it arrived before `show()`.
  - **Fix:** hidden pages now get at most one frame per second, the early frame is kept, and the note updates immediately. Regression tests cover both causes.
  - `bench-node.mjs` treats an unproven `mainBatteryReady` (NEPTUN) as unknown.
  - **Planner re-check:** a fresh TRITON boot shows B1 at 2.0 virtual s; `test-ui.mjs` 77/77, `test-node.mjs` 23/23.
  - Recorded as the repository-split slice. Next: dive-mode performance (user target 10×, at least 4×; measured about 0.9×). PERF-PROFILE is measuring.
- URL loading:
  - Neither the Wayback Machine nor `api.multi3s.com` sends CORS headers. The user rejected mirroring the firmware (no redistribution) and chose a Vercel proxy.
  - **VERCEL-PROXY done:** `deploy/api/firmware.mjs`. It accepts only an exact allowlist (`api.multi3s.com/static/<name>.srec` and its Wayback `<ts>id_` form) and re-checks every redirect. Limits: 20 s, 4 MiB, the body must start with `S0`. Responses are `no-store`; nothing is logged or stored.
  - **CORS:** `https://triton.divehub.ai`, plus localhost and 127.0.0.1 on any port; same-origin requests are allowed without `Origin`.
  - **Page:** "Load from URLs" feeds the same verification path as dropped files. CSP `connect-src` adds loopback; the published page adds the configured proxy origin (see the publishing entry at the end of this section).
  - **`deploy/build_site.py`** assembled an allowlisted upload directory (then `target/vercel-site/`, 23 files, refusing anything firmware-like); the publishing entry at the end of this section describes the current split.
  - **End to end:** with the local `dev-proxy.mjs`, the user's two Wayback URLs verified as TRITON (SHA-256) and booted to B1.
  - **Tests:** proxy 26/26, build_site 16/16, `test-ui.mjs` 94/94, `test-node.mjs` 23/23.
  - **Not deployed at that time;** see the publishing entry at the end of this section.
- PERF-PROFILE done (valid-tissue dive at 20 m, Node):
  - **Speed:** 2.85× on average (native 4.2×). Bursts of main-board compute (about 1.4 s every 4 s) run at 1.1× in Node; quiet periods at 13–15×.
  - **Where burst time goes:** 75% of executed main instructions are a libgcc-shaped soft-double divide (`0x0800486c`, ~540 instructions per call, 40k calls/s, only 197 distinct operand pairs). Then `expf` (10%), f2d/d2f (4.7%) and the tissue update.
  - **Ruled out:** fast-forward extensions gain ≤2% in a dive; LCD/MMIO ≤3%.
  - **NaN-tissue dive:** 13× in Node, because the deco code short-circuits.
  - **Plan:** exact routine acceleration, then interpreter work (to be written up as a later section).
- Publishing (2026-10-08): the emulator moved into its own public repository, with the layout flattened to the repository root (`crates/`, `web/`, `deploy/`, `docs/`, `testdata/`, `licenses/`). Firmware is never committed; tests that need it skip when neither `firmware/<release>/` nor `NGC_FIRMWARE_DIR` has it.
  - **Site:** GitHub Pages (workflow build, `.github/workflows/pages.yml`) serves the static page at `https://triton.divehub.ai`. The workflow runs the tests that need no firmware and `deploy/build_site.py`, which assembles an allowlisted `target/pages-site/`.
  - **Proxy:** the Vercel function `deploy/api/firmware.mjs` is hosted on its own address (`deploy/build_proxy.py` assembles the proxy-only upload directory). The site learns that address at build time from the repository variable `FIRMWARE_PROXY_URL` (a generated `config.js`; its origin is added to `connect-src` of the published `index.html`). Without it "Load from URLs" is unavailable, and the page never falls back to a same-origin `/api/firmware`.
  - **Fix found while checking the published page in Chromium:** `Element.append(null)` prints "null", so every verified firmware slot showed a stray `null` line and the session information did the same in handset-only mode. The null children are now filtered out, the fake DOM of the tests converts non-nodes to text like a browser does, and a regression test covers both places (96 page tests).
  - **CSP:** GitHub Pages cannot send response headers, so the page policy is the `<meta>` tag only; the worker script has no policy of its own there (the header form of `web/serve.py` applies locally).
- WEBFIX (2026-10-08): UART console freeze, remember by default, 4.1 V batteries.
  - **UART console:**
    - **Cause (proved at the DOM level):** `renderUart` replaced the text of all five `<option>`s of `#uart-channel` and re-assigned its `value` and `disabled` on every state update. Measured with real firmware, 365 option-text mutations in 15 s (every option, 5 Hz). A browser rebuilds or closes the dropdown of a select whose children change while it is open, even for identical text.
    - **Not observed:** the freeze itself. The in-app browser draws the native dropdown outside the page and ignores key events for it, so the report "disappears and reappears, then the whole page freezes" is explained by this cause but was not reproduced end to end. No layout shift, long task or slow render was seen (heartbeat 20 ms, render about 1.7 ms before and after).
    - **Fix:** the select is written only where the channel list differs (options by id, texts in place, selection kept by channel id, `value` and `disabled` only on change). `setText` in `dom.js` writes a text only when it differs; the console text, its status line and the LED color selects follow the same rule. The console shows at most 65 536 characters.
    - **After:** 0 mutations on the select, the same engine state at the same virtual time (HUD1 activations identical), replay flashes unchanged.
    - **Tests:** six page tests in `test-ui.mjs`, five of which fail on the old code.
  - **Remember the files:** `#remember` starts checked; unchecking, Forget and the disabled state without OPFS are tested.
  - **Batteries:**
    - **Default:** a fresh profile starts with both batteries at 4100 mV (`fixtures::DEFAULT_BATTERY_MV`); saved profiles keep their `inputs.json`. The scenarios pin 1500 mV through `scenario::recorded_inputs` (`platformOptions.batteryMv`, plus a check in `dual-wake`), as do the NEPTUN B1-frame test and the `test-node.mjs` B1-hash test, because every Renode recording used 1500 mV.
    - **Firmware reaction (synthetic reproduction):**
      - The B1 prompt is pixel-identical at 4100 and 1500 mV (LCD hash `62c3a30e…`); only the UART line differs ("Main voltage: 4099 mV").
      - After the wizard the voltage must fit the chosen type: Alkaline at 4100 mV makes the next start show "Change battery" and the main board stands by at 8.1 s with the handset unpowered. Alkaline at 1500 mV and Li-Ion 3.7V-18650 at 4100 mV start normally. This is what made the `diluent-menu` and `can-loss` scenarios fail without the pin. Regression test: `crates/ngc/tests/battery_default.rs`.
- DECO-FIX (2026-10-08): decompression state handling (section 17).
  - **Diagnosis (behavior of the original firmware, not of the engine; the engine's decompression arithmetic equals Renode's bit for bit):** the no-decompression limit stayed at 99 for two reasons. (1) On a later start of a profile that was booted once, the main application loads 32 erased tissue words as NaN and keeps them, because it saves the tissues only on power-down while it saves the decompression date at start-up. (2) With uncalibrated oxygen cells in the measured-ppO2 mode the ppO2 is NaN. Related: a cold boot clears the cell calibration flags, and a depth left in `inputs.json` makes the next start begin under water (the firmware takes its first pressure as the surface).
  - **Done:**
    - `decoHealth` in the state (read-only report), with the addresses in the per-release table (`docs/releases.md`).
    - The pre-boot EEPROM consistency fixture `decoStorageFixture` (default on), which erases the saved date record when the stored tissue block was never saved.
    - The start-at-the-surface fixture `startAtSurface` (default on): every board creation starts at depth 0, a new session also with the default oxygen cells.
    - Page warnings with the next step, a cold-boot hint, two start options, and a surface-pressure setting of the basic view that is remembered and sent with Restart, Cold, Wake and a serial change.
    - Switches in `SessionConfig`, the session-create JSON and `ngc-cli run`; the scenario suite pins both fixtures off (`scenario::recorded_config`), like the battery pin.
  - **Results (this engine, fresh profile, air calibration through the firmware's CAN protocol, 35 m, Restart):** with the fixture on the tissues are finite and the raw NDL is 5 min 10 s after the descent; with it off 32 of 32 tissue words are NaN and the NDL stays 99 for 30 s at depth (`decoHealth.tissues` is `invalid`). Synthetic reproductions, not physical observations.
  - **Tests:** `crates/ngc/tests/deco_fixtures.rs` (5, real firmware), unit tests in `deco.rs` and `surface_start.rs`, 8 page tests and one real-engine runtime test in `web/`.
- UI-POLISH (2026-10-09): compact firmware cards and American spelling (user decisions).
  - **Cards:** a verified firmware card is one line (check mark, release and board, file name and size, Remove); the SHA-256, the source address and the check list sit in a closed `<details>` in the card, a failed check stays visible, and a waiting card is two short lines. A card is rebuilt only when its file changes, so an opened `<details>` and the focus survive option changes. On a 375 x 812 phone with both files verified the cards went from 195 px to 92 px each and the Boot button from 1126 px to 851 px from the top of the page.
  - **Spelling:** UI text, Markdown, comments and engine messages use American English (`AGENTS.md`). The checkbox verb "tick" became "check". Left as they are: the `depthMetres` key of the `dive` scenario report, Renode identifiers quoted in comments (`UnhandledAccessBehaviour`, `CancellationToken`), `aria-labelledby`, third-party license texts and the recorded test data.

## 14. Session API (contract for FEATURES and WEB)

`crates/ngc/src/session.rs` (FEATURES) is the single host-facing API used by `ngc-cli` and `ngc-wasm`; hosts never reach into boards directly. Shape (extend, don't rename):

```rust
pub struct Profile {            // byte-compatible with the Renode runner's profile files
    pub eeprom: Option<Vec<u8>>,     // eeprom.bin (2048 bytes)
    pub nor: Option<Vec<u8>>,        // nor.ngc (NGCQuadSPI backing format)
    pub rtc_state: Option<String>,   // rtc-state.json (emulation/rtc_persistence.py format)
    pub inputs: Option<String>,      // inputs.json
    pub led_colors: Option<String>,  // led-colors.json
}
pub struct SessionConfig { pub mode: Mode, pub boot_mode: BootMode, pub simultaneous_start: bool,
                           pub idle_fast_forward: bool, pub adc_sample: u32, pub start_paused: bool }
pub struct Capture { pub state_json: String, pub lcd_png: Vec<u8>, pub can_trace_tsv: String, pub name: String }
impl Session {
    pub fn new(config: SessionConfig, main: Option<&Firmware>, handset: &Firmware, profile: Profile) -> Result<Session, String>;
    pub fn run_for(&mut self, virtual_seconds: f64) -> RunOutcome;  // does nothing while paused/standby/error
    pub fn running(&self) -> bool;
    pub fn action(&mut self, request_json: &str) -> Result<String, String>; // runner POST /api/action schema → state JSON
    pub fn state_json(&self) -> String;          // runner snapshot fields + "engine", timing, idle-skip stats
    pub fn frame(&mut self) -> FrameView<'_>;    // { width, height, rgba: &[u8], version: u64 }
    pub fn take_profile_changes(&mut self) -> Option<Profile>; // dirty storage since last call
    pub fn export_profile(&mut self) -> Profile; // full profile incl. a fresh RTC checkpoint
    pub fn capture(&mut self) -> Capture;        // state.json + lcd.png + can-trace.tsv
    pub fn shutdown(self) -> Profile;            // runner close semantics (RTC checkpoint saved)
}
```

Actions mirror `run_emulator.py` exactly (names, payload fields, validation messages): `pause`, `resume`, `step` (2 × 50 ms), `advance` (`seconds` ≤ 20), `reset`, `cold`, `wake`, `up`, `down`, `confirm`, `can` (`connected`, `dropId`), `inputs` (`inputs` object; ranges/defaults from `INPUT_DEFAULTS`/`INPUT_RANGES`; values rounded through f32 like Renode's monitor), `led-colors` (`colors`), `serial` (`serialNumber`), `capture`. Pacing (wall clock) belongs to the host. Additions of section 17: `reset`, `cold`, `wake` and `serial` accept an optional `surfacePressureMbar` (100 to 30000, validated before anything is shut down); `SessionConfig` and the session-create JSON gain `decoStorageFixture`, `startAtSurface` (both default true) and `surfacePressureMbar`; the state gains `decoHealth`, `decoStorageFixture` and `startAtSurface`.
- REF done: 242 pinned Renode/tlib sources (`reference/renode-src`, MANIFEST hashes; tlib is LGPL reference only), firmware.bin ×2 verified, `docs/renode-semantics.md`, reference data (`reference/data/`: 20 M-instruction handset PC trace + 1 ms snapshots + checkpoints 1/2/3/3.9 s; dual-wake run1/run2 checkpoints 0.5–5.5 s; micro vectors). Renode run-to-run envelope (dual): identical through 1.05 s; afterwards idle PC/R3 and ≤373 SRAM1 bytes differ, CAN stamps ±79.6 µs, all payloads/order/LCD/storage identical.
- FPU done: `armv7m-vfp` decodes all 4 348 Ghidra-listed VFP instructions of both images (+42 Ghidra missed), exact soft-float reference plus proven native fast paths, bit-exact against the AArch64 FPU in all 32 RMode/FZ/DN combinations (millions of cases per op), wasm self-test checksum identical to native; 60 tests pass. Integration notes for CPU: `Undefined` → UNDEFINSTR, `NotVfp` → NOCP, `execute` never returns `Undefined`, FPSCR loads use `vfp::fpscr::WRITE_MASK` (0xF7C0009F).

## 15. Repository split and viewer parity (2026-10-08, takes precedence over sections 1–12)

### 15.1 Repository rules

- This repository is the **WebAssembly emulator**, the base for a playable CCR operating experience. During development it was the WebAssembly branch of a private repository, where its content lived under `emulation/wasm/`; paths here are relative to the repository root. A separate Renode-based analysis workspace (not public) is used to debug the firmware. The two never merge; changes were ported by reading that workspace's files.
- Removed here (they remain in the analysis workspace): the Renode runner/viewer/models/probes/evidence, the firmware/app static analysis, `reference/**` (pinned Renode/tlib sources, Renode harness scripts, `compare-reference` data). **Nothing may read a `reference/data/` directory any more.**
- Kept: recorded Renode golden data committed under `crates/*/tests/**` and `testdata/renode-micro-vectors.json` (moved from `reference/micro/vectors.json`). They are regression fixtures now; they no longer need a Renode installation.
- Comments naming `emulation/models/*.cs`, `emulation/run_emulator.py`, `emulation/viewer.html` etc. refer to the analysis workspace; leave them. Ported-file attribution headers stay (MIT notice in `licenses/`).
- Fidelity to Renode is no longer a gate for new features, but existing behavior must not change unless a contract below says so. **New observation features must not perturb guest execution** (no new clock entries, limit timers or chunk splits in a board's clock); sample at the system's quantum boundaries instead.
- Local firmware: `firmware/TRITON-5.8-65.3/*.srec` and `firmware/NEPTUN-5.8-65.3/ngc_{main_5.8,handset_65.3}_NEPTUN.srec` (both ignored and never committed; you supply them; read-only).

### 15.2 Work package RUST, part 1: trim (do this first)

Make every crate, test, scenario and CLI command self-contained inside the repository, with all tests passing:

1. Delete `ngc-cli compare-reference` (`cmd_compare.rs`, `compare_state.rs`, `compare_dual.rs`, `compare_handset.rs`, the reference-data default in `common.rs`) and its help/README mentions. `scenario --out` must no longer default under `reference/`; default to `target/scenarios` (ignored).
2. Delete tests and test blocks that read `reference/data` (e.g. `armv7m/tests/renode_trace.rs`, `ngc/tests/handset_cyccnt.rs`, the data-dependent parts of `ngc/tests/renode_per_e/main.rs`, `can_link.rs`/`eeprom.rs`/`firmware.rs` unit tests that open `reference/data`). Keep tests that use committed data. Point `armv7m/tests/micro.rs`, `ngc/tests/micro_timer.rs` and `emu-core/tests/clock.rs` at `testdata/renode-micro-vectors.json`.
3. Scenarios that compare with recorded runner evidence under `emulation/main-boot/**` or `reference/data/**` (deleted or unreadable): embed the expected values they need as constants with a provenance comment (`recorded by the Renode runner in the analysis workspace, <path>`), so `ngc-cli scenario all` passes with no files outside the repository except the SRECs.
4. Delete the Renode-side generators and harness peripherals of the golden data (`tests/**/gen_golden.py`, `gen_vector.py`, `Diff*.cs`). Add `testdata/README.md` (one short paragraph per data set: what it is, recorded from Renode 1.17.0 on 2026-10-07/08, the generators are not part of this repository).
5. Doc comments that cite `reference/...` paths: rewrite as provenance in the past tense or drop. `docs/renode-semantics.md` and `docs/framework.md`: add a header note that `renode-src/...` citations refer to Renode 1.17.0 upstream (infrastructure `066a7f13c052215632d469c995c89aea37c573b1`), and fix references to removed files.
6. `grep -rn "reference/" crates --include='*.rs'` must find no path that is read at build or test time.

### 15.3 Work package RUST, part 2: engine contract for the main-viewer parity

Port these changes of the analysis workspace (its Renode models, scripts and runner: `emulation/models/NGCBoardTelemetry.cs`, `emulation/main.resc`, `emulation/handset.resc`, `emulation/run_emulator.py`, and its `emulation/README.md`):

**a. Output activity histories** in `hardwareOutputs[]`, JSON exactly as the analysis workspace's `NGCBoardTelemetry.ActivityJson`:
- Each entry gains `"activity"` and `"pwmActivity"` (both `null` when not applicable). Main HUD 1–3: `activity` = change-only samples of the TIM4 channel command (`source: "sampled-pwm-command"`, sampling period 0.05 s). Handset vibrator: `activity` = exact PB15 enable-level changes (`source: "gpio-enable-command"`, period 0, `level` set, `active`/`dutyPercent` null), `pwmActivity` = change-only samples of the gated TIM15 CH1 command every 0.02 s (`source: "sampled-gated-pwm-command"`). Backlight: both `null`.
- Activity object: `{source, clock: "virtual-time", capacity: 32, eventCount, transitionCount, activationCount, lastOnVirtualTime, lastOffVirtualTime, samplingPeriodSeconds, timestampUncertaintySeconds, truncated, events: [{sequence, virtualTime, kind: "initial-sample"|"change", active, dutyPercent, level, command}]}`; `command` = `{timerEnabled, channelEnabled, polarityInverted, outputMode, pwmMode, ccr, arr, gpioMode, alternateFunction}` or `null`. Counting rules as in `OutputActivity.Record` (initial samples are not transitions/activations; the queue keeps the newest 32).
- Sampling uses the same register reads as the existing duty computation, at system quantum boundaries every 20/50 virtual ms, with no side effects on the guest. Histories reset with the board (restart, cold, wake, machine reset).
- Top-level state field `"outputHistoryEpoch"`: a string that changes on session creation and on every board recreation. Use `"<historyNonce>-<generation>"`, where `historyNonce` (u64, default 0) comes from `SessionConfig` and the session-create JSON (`historyNonce`; the browser passes a random value).
- Fast-forward on/off and native/wasm results must stay identical, including the histories.

**b. HUD color defaults**: `main-hud-1` unknown, `main-hud-2` white, `main-hud-3` red; saved `led-colors.json` choices override.

**c. I2C idle-high fixture on by default** (main board PB6/PB7/PB10/PB11 driven high before guest execution, as `main.resc` now does). `SessionConfig.i2c_idle_high` (default true; session-create JSON `i2cIdleHigh`), CLI `--no-i2c-idle-high`; the state names the fixture. Update tests and scenario expectations that depended on the old default (or pin them to `false` when they reproduce older Renode recordings).

**d. Serial number** accepts 0–999 999 999 (9 digits); clear error message.

**e. Firmware releases** (the analysis workspace runs any supplied pair; this repository supports known releases):
- Extend `firmware.rs` with a release table: `TRITON-5.8-65.3` (existing facts) and `NEPTUN-5.8-65.3` (SREC SHA-256 main `e462bc7345d6ded69124b97b87de9a68884f44b8e716fbbf4fe3839ff8da8c89`, handset `f91adcf461fa0e06ef40ab3f757dd66754180b9711956542736efb0d4be8162e`; reset PC main `0x080390b8`, handset `0x08008444`; SP `0x20018000`; derive and record the remaining facts from the local files). Boot uses each image's own vectors.
- `ngc_firmware_inspect` JSON gains `"release": {"id", "label"}` or `null`; `ngc_set_firmware` accepts either release per role. Session creation fails with a clear message when main and handset come from different releases. The state gains `"firmware": {"release": {id, label}, "main": {srecSha256, binSha256, stack, resetPC}, "handset": {…}}`.
- Inventory every firmware-specific address the engine or state uses (diagnostic RAM fields, the cold-boot standby PC `0x0800598e`, LCD/mode summaries, …) into a per-release table. For NEPTUN, find equivalents by matching the TRITON code/literal bytes; where no equivalent is proven, report the field as unavailable (`null` plus a reason), never a TRITON-derived value. Cold boot on NEPTUN must either detect standby correctly or be refused with a clear message.
- Smoke check: NEPTUN dual boot for 10.5 virtual s reaches the B1 battery-selection screen (look at the LCD PNG), releases the handset near 1.05 s, has zero CFSR/HFSR and forwards CAN frames.

### 15.4 Work package WEB: browser UI parity with the analysis workspace's viewer

Port the user-facing changes of the analysis workspace's `emulation/viewer.html` and their behavior rules (its `emulation/README.md` "Controls" and "Hardware outputs", `AGENTS.md` and `README.md`) into `web/`, keeping this app's module structure, worker pacing and existing features:
- **Basic view** (the default, player-facing): status strip with Vibrator, Red LED (HUD 3) and White LED (HUD 2) above the LCD; LCD; handset buttons (also taps on the LCD: upper third Up, middle third Confirm, lower third Down); execution (Pause/Resume and Restart boards, speed, virtual time; there is no Step button, Advance N s is under Advanced); **Simulated conditions**: oxygen base 0–100 mV + three signed cell offsets, pressure from surface mbar + depth 0–110 m + water type (Fresh 1000 / Salt 1025 / EN13319 1020 kg/m³; `mbar = surface + density × 9.80665 × depth / 100 + offset`) + two signed offsets, temperature base −4–40 °C + two signed offsets; each **Sensor variations** section collapsed. **Sensor numbering:** the page numbers the two MS5837 sensors as the firmware does, the reverse of the engine's input keys (which stay as they are for `inputs.json` and the CLI scripts): sensor 1 (P1, T1, "Sensor 1 offset", "Pressure sensor 1") is the device on I2C2 (`pressure2Mbar`, `temperature2C`), sensor 2 the device on I2C1 (`pressure1Mbar`, `temperature1C`); `web/sensors.js` holds the mapping, and the Advanced raw fields name both ("Pressure sensor 1 (I2C2, absolute mbar)") with one note. Basic edits apply immediately; invalid sums or incomplete numbers show **Not applied** and send nothing (never clip; raw ranges oxygen 0–250 mV, pressure 100–30 000 mbar, temperature −20–85 °C); actions are serialized and adjacent pending basic updates coalesce to the newest; stale replies/polls never overwrite newer edits; status **Applying… / Inputs applied / Raw edits pending**. Bases/offsets are derived from the current inputs on load (no separate persistence).
- **Advanced · raw inputs, outputs and diagnostics** (closed initially): raw sensor fields with **Apply inputs** (drafts until applied; basic edits populate them), CAN controls, serial (9 digits), storage, raw hardware outputs with Drive/Replay labels, histories and HUD color selectors, backlight, UART console, execution/model details, plus this app's extras (Advance N s, background policy, real-time factor, profile and evidence, loaded firmware/session).
- **Replay pulses** (on by default) exactly per the analysis workspace: current On stays steady; when the current command is Off, newly observed activations missed between polls queue wall-clock flashes, 150 ms on + 100 ms gap, at most 12 per output; initial load/reconnect is a baseline; a new `outputHistoryEpoch`, missing history or a sequence gap clears queues. Use the 15.3a JSON.
- **Releases**: the entry screen accepts a TRITON or NEPTUN pair (shows the release, rejects mixed pairs with a clear message), titles show the release, and each release has its own OPFS profile (TRITON keeps its existing location). Pass a random `historyNonce` and `i2cIdleHigh: true` at session creation; expose the I2C fixture as a start option.
- Tests: port the cases of the analysis workspace's `emulation/test_output_replay.js`, `test_output_replay_ui.js`, `test_sensor_controls.js` and `test_scenario_ui.js` to Node tests here (no new dependencies); keep `test-node.mjs` green.

### 15.5 Ownership for this phase

| WP | Owns |
| --- | --- |
| Planner | `DESIGN.md`, `README.md`, `licenses/**`, root files, `.gitignore`, `.gitattributes`, git |
| RUST | `crates/**`, `testdata/**`, `docs/**`, `Cargo.toml` members if a crate changes |
| WEB | `web/**` |

RUST builds with `--target-dir target/rust`, WEB with `target/web` (the `build.py` default). Until RUST lands 15.3, WEB develops against fixture JSON shaped like 15.3a.

## 16. Dive-mode performance (2026-10-08)

**Goal (user):** in dive mode (valid tissues, 20–30 m), 10× real time in the browser, at least 4×. That should hold during the main board's deco bursts too, not only on average. At the start of this phase the average in Node was 2.85× and bursts ran at 1.1× (section 13, PERF-PROFILE). The committed dive benchmark (16.3) reproduces the valid-tissue dive; the original profiling data stayed outside the repository.

### 16.1 Exactness (contract for every optimization in this phase)

An optimization may change only host speed. With it on and with it off, the following must be identical at every quantum boundary: all core registers (scratch registers included), xPSR (flags, IT state, exception number), FPSCR and the VFP registers, all memory (stack below SP included), the instruction count, DWT_CYCCNT/SysTick, event and IRQ timing, and the translation-block/predecode state that can change later block partitioning (Renode cut-block persistence). The proof is required, not optional:
- **Fingerprint identity**, on vs off, at checkpoints over the committed dive benchmark (16.3) and the existing scenarios.
- **Fast-forward identity**, on vs off, with the optimization on.
- **Native/wasm identity.**
- A **shadow-verification mode**: run both ways and compare. It is used in the tests, like `bench --verify-idle-ff`.
- **Randomized differential tests** for every routine shortcut.
- A **switch** for each optimization: SessionConfig, CLI flag and a session-create JSON key, on by default.

### 16.2 Routine acceleration (work package PERF-HLE)

Hot leaf routines of the firmware's runtime library are executed without per-instruction interpretation, exactly. Targets, by share of executed main-board instructions in a dive:
- the soft-double divide at `0x0800486c` (75%; ~540 instructions per call; operands repeat, 197 distinct pairs in 399k calls);
- `expf` and its wrapper (10%; `0x0802cc7c`, `0x0802bc90`, `0x0802be20`, `0x08004b4c`);
- f2d/d2f (4.7%; `0x08004568`, `0x08004c08`).

Rules:
- **Identify routines by their code bytes** (a hash of the routine body plus literal pools), never by TRITON addresses alone. Record the matched entry addresses per image and report whether NEPTUN contains the same code.
- **Apply a shortcut only when** the remaining instruction budget of the current CPU chunk (until the next event or deadline) is at least the routine's exact instruction count for this input. Then no interrupt, event, chunk cut or translation-block cut can fall inside the routine. Otherwise interpret normally.
- **Reproduce everything:** outputs and clobbered scratch registers, flags, IT state, stack writes (pushed values and their addresses), the instruction count (including IT-skipped instructions, which count in this engine) and the return PC. A memoized result, keyed by every input the routine reads, is acceptable if the key is complete. Exact native implementations with a path-accurate instruction count are acceptable if validated against the interpreter. Choose per routine.
- **Memory:** routines that read RAM other than their own stack frame, touch MMIO, or depend on state outside the key are not eligible.

### 16.3 Dive benchmark (PERF-HLE)

Add a committed, deterministic dive benchmark that builds its valid-tissue profile from scratch only through firmware routes: battery wizard, air calibration through the menu, a short NaN dive that saves a decompression date, a +5-day main RTC checkpoint fixture, restart and recalibration, then dives at 20 m and 30 m. Expose it as `ngc-cli bench --dive` (or a scenario) with burst and average speed and fingerprints at checkpoints. Add the same to `web/bench-node.mjs` (`--dive`, using the browser ABI). It is the gate for 16.1 and for the speed targets.

### 16.4 Interpreter and bookkeeping (work package PERF-INTERP)

- **IT blocks in the fast loop.** About 22% of executed main instructions are IT bodies; they cost about 14 ns on the slow path vs 4.4 ns.
- **Chunk bookkeeping** in `run_board_to`.
- **Clock arithmetic:** u128 division in `ClockEntry` replaced by exact u64 arithmetic where provable.
- **No string formatting on timer register writes.**
- **Other measured hot-path costs.** Every change must be exact (16.1); measure before and after on the dive scripts.

### 16.5 Ownership

| WP | Owns |
| --- | --- |
| PERF-HLE | **Owns:** new `crates/armv7m/src/accel/**`; one clearly delimited hook in `crates/armv7m/src/cpu.rs` and its config in `armv7m/src/lib.rs`; `crates/ngc/src/scenario/dive*.rs` plus its registration; config plumbing in `crates/ngc/src/{session.rs,system.rs}` (switch only); `crates/ngc-cli/**`; `crates/ngc-wasm/**`; `web/bench-node.mjs`. |
| PERF-INTERP | **Owns:** `crates/armv7m/src/{exec.rs,op.rs,decode.rs,alu.rs}` and the rest of `cpu.rs`; `crates/emu-core/src/clock*`; `crates/stm32/src/timer/**`; `crates/ngc/src/{board.rs,bus.rs}`; chunk bookkeeping in `system.rs`. |

Both edit `cpu.rs` and `system.rs`. Re-read before every edit, keep edits small and local, and keep every crate compiling at each save. Target dirs: `target/perf-hle` and `target/perf-interp`.

### 16.6 Results (2026-10-08)

- **Routine acceleration (PERF-HLE).** A record/replay memo of whole calls to the soft-double divide, `expf` and its helpers, `unorddf2`, `f2d` and `d2f`, identified by SHA-256 of body plus literal pool (`crates/armv7m/src/accel/`). An entry is created only from a call the interpreter executed, keyed by r0–r3, s0–s1 and the FPSCR control bits; a dependency tracker proves that every output is a function of the key or a copy of an entry register, and that only key-determined flash and the routine's own frame are read. A hit replays only at a block boundary with no pending exception and when the call fits in the chunk budget. NEPTUN's main image has the same divide and conversion routines (same hashes) and its own `expf`.
- **Interpreter (PERF-INTERP).** IT blocks run in the fast loop (a second loop mode); exact u64 clock arithmetic (proven against the u128 reference), a memoized SysTick deadline, a lazily advanced DWT counter, cheaper idle-loop verification, no string formatting on timer register writes.
- **Exactness.** `bench --dive --verify-routine-accel` is identical at all 25 checkpoints; native and wasm are identical at all 25; the shadow mode compared 3.8 M replayed calls with 0 mismatches; scenarios and `bench --verify-idle-ff` are unchanged.
- **Speed, valid-tissue dive** (Apple M1 Max):

| | Node, before this phase | Node, after | Native, after |
| --- | --- | --- | --- |
| 20 m average | 2.85× | 11.8× | 17.1–17.4× |
| 20 m deco bursts | 1.1× | 5.6× | 7.5–7.6× |
| 30 m average | | 13.6× | 18.7–20.9× |
| Surface, idle fast-forward on | 17× | 24.5× | 42.8× |

The remaining time is interpretation of the deco code, limited by host branch mispredictions; further gains need fewer executed guest instructions (more accelerated routines or a block compiler).

## 17. Decompression state handling

Everything in 17.1 is behavior of the original TRITON main 5.8 firmware, reproduced by this engine; the engine's decompression arithmetic equals Renode's bit for bit. Everything in 17.2 and 17.3 is a labeled emulator fixture or a read-only report. All of it is a synthetic reproduction on a functional model, not a physical observation.

### 17.1 What the firmware does

- **Tissues.** The main application keeps 16 tissue records (36 bytes each; an N2 float at +24 and a He float at +28) in RAM and loads them from 32 EEPROM words (physical `0x0ff..=0x17e`, record IDs `0x6a..=0x89`) in its start-up initializer (`0x08008308`). The initializer compares the saved last-decompression date (record `0x8a`, physical `0x17f`, four bytes, a packed RTC calendar) with the RTC (elapsed-time compare at `0x08008334`/`0x0800833a`); at four days (345 600 s) or more, or with an erased date, it sets a reset flag and calls the reset routine (`0x08007584`), which puts the tissues in equilibrium with the pressure it reads at that moment, and it writes the date. It saves the tissues only on the power-down route (handset CAN `0x149`, or queue case 2), never at start-up.
- **Consequence 1.** A profile that was booted once holds a date and an erased tissue block (all `0xFF`). The next start loads 32 NaN words, finds the elapsed time under four days and keeps them: the no-decompression limit (raw NDL, RAM `0x20002108`) stays 99 at any depth.
- **Consequence 2.** In the measured-ppO2 mode (breathing-mode byte `0x20002457` = 2) the ppO2 (`0x2000421c`) is NaN while the oxygen cells have no calibration, with the same effect. On this engine the ppO2 of an uncalibrated profile stays 0 at the surface and becomes NaN once a dive starts; the cached cell flags (`0x200023f4..=0x200023f6`) read `0x01` until a calibration (`0x09`) and are rewritten to `0x01` by a cold boot (wake cause 0).
- **Consequence 3.** The pressure read at start-up is taken as the surface pressure, so a profile whose `inputs.json` holds a depth starts under water with a wrong surface and tissues.
- **Observation (cause not analyzed).** After a calibration with injected CAN commands (the firmware's own protocol, but not driven by the handset's menu) the decompression code's gas record (`0x200020d4..`, entry 6 stays 0.0) was not updated: the NDL stayed 99 for 120 virtual seconds at 35 m in that session and fell within seconds after a Restart. Whether the handset's menu route, which also sends the handset's own follow-up traffic, avoids this was not checked here. The tests therefore dive after the Restart.

### 17.2 `decoHealth` (read-only)

The state document gains `decoHealth`: `{tissues: "valid"|"invalid"|"unknown", oxygen: "calibrated"|"uncalibrated"|"unknown", details}`, from side-effect-free peeks (`crates/ngc/src/deco.rs`).

- **tissues:** all 32 words finite is `valid`; any NaN or infinity is `invalid`; all zero (RAM before the firmware initialized it) is `unknown`.
- **oxygen:** only in mode 2. A non-finite ppO2 is `uncalibrated`, a finite non-zero one `calibrated`. A ppO2 of zero (not computed yet) is decided by the cached cell flags: `uncalibrated` when every enabled cell has calibration state 0 (flags bits 2-3), else `unknown`.
- **Unavailable:** a handset-only run and a release whose addresses are not proven (NEPTUN: the main image is a different build, no byte-identical counterpart) report `unknown` with the reason in `details`; the addresses are in the per-release table (`docs/releases.md`), never taken from TRITON.
- The page shows a warning only for proven invalid tissues, naming the next step: "Decompression state invalid: restart the boards to let the firmware reset it." (with the repair fixture off: close the session, check it and boot again). Uncalibrated oxygen is not shown as a warning (user decision, 2026-10-09); the report still carries it. The Cold boot button and the cold-boot start option carry a hint that the firmware clears the oxygen calibration.

### 17.3 The two fixtures (both on by default, both switchable)

- **Pre-boot EEPROM consistency (`decoStorageFixture`).** Before every board creation (session start, Restart, Cold, Wake, serial change, reopening a profile): if the stored tissue block is entirely `0xFF` and the 4-byte date record is not, the date record is set to `0xFF` in the EEPROM image before it is loaded, and the firmware takes its own four-day reset path. No other byte is touched: not the oxygen calibration, not the tissue words, no RAM, no ppO2. The repaired image becomes the profile (it is flagged for saving). TRITON only; NEPTUN reports why it is skipped. The state names it: `decoStorageFixture: {enabled, applied, reason, previousDateRecord}` (also in captures). Switches: `SessionConfig::deco_storage_fixture`, `ngc-cli run --no-deco-storage-fixture`, session-create `decoStorageFixture`.
- **Start at the surface (`startAtSurface`).** At every board creation both pressure inputs are set to the surface pressure plus each sensor's offset (its reading minus the mean of the two, which is what the basic view derives as the offset), so the depth is 0. A **new session** (`Session::new`: a boot, a profile import or a profile reset) also resets the three oxygen cells to their defaults (10 mV); Restart, Cold, Wake and serial keep them, because a calibration made with them stays meaningful. Nothing else changes. `inputs.json` keeps its format byte for byte: the surface pressure is a session setting (`SessionConfig::surface_pressure_mbar`, session-create `surfacePressureMbar`, default 1013.25 mbar, and the optional `surfacePressureMbar` of `reset`, `cold`, `wake` and `serial`), and the page keeps its own surface-pressure setting in the browser's local storage. The state names it: `startAtSurface: {enabled, surfacePressureMbar, applied, oxygenReset, changedInputs, note}`. Switches: `SessionConfig::start_at_surface`, `ngc-cli run --no-start-at-surface`, session-create `startAtSurface`.
- **Scenarios.** The Renode recordings were made without either fixture (a stored image is compared byte for byte, a reopened profile keeps its inputs), so the scenario suite pins both off through `scenario::recorded_config`, and `platformOptions` says so.
- **Tests.** `crates/ngc/tests/deco_fixtures.rs` (real firmware, calibration through the firmware's CAN protocol): first boot, calibration, a dive, Restart, then NDL below 99 with the fixture on and the NaN state with it off; the fixture writes only the date record; every board creation starts at the surface and a new session resets the cells; the health report follows a calibration and a cold boot; NEPTUN reports unknown. `web/test-ui.mjs` and `web/test-node.mjs` cover the warnings, the start options, the surface setting and the form following the engine after a Restart and a new session.
