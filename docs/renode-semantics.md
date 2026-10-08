# Renode 1.17.0 behavioural semantics for the NGC Rust engine

Work package REF. This document records what the pinned Renode 1.17.0 platform (`emulation/handset.repl`, `emulation/main.repl`, `emulation/models/*.cs` of the analysis workspace) actually does, with source citations, so that the Rust engine can reproduce it, or deviate from it knowingly. It supplements `DESIGN.md` (sections 5-7 are checked against it in section 13).

> **Note (2026-10-08).** The `renode-src/...` citations below refer to the upstream Renode 1.17.0 sources (renode-infrastructure commit `066a7f13c052215632d469c995c89aea37c573b1`, tlib `167decf9129758829762e582939128c0694e90d6`); the pinned copies and the REF harness (`reference/*.py`, `reference/micro/*`, `reference/README.md`, the measurement data under `reference/data/`) are not part of this repository (the data directory was never versioned). Numbers measured there are quoted in the text. What stays as regression data is committed under `testdata/` and `crates/*/tests/**` (`testdata/README.md`). Paths like `emulation/handset.repl` name files of the separate Renode-based analysis workspace (not public).

**Evidence classes.** Every non-trivial statement carries one of:

- **[S]** derived from reading the pinned source (`reference/renode-src/`, byte-identical copies of the upstream files; see the note above);
- **[M]** measured in a Renode run of the analysis workspace (data was in `reference/data/`, ignored by git and not available; the harness is not part of this repository);
- **[S+M]** both.

Anything not marked was not verified. Never read this document as a statement about the physical device.

**Citation shorthand.** `M/` = `reference/renode-src/src/Emulator/Main/`, `P/` = `.../src/Emulator/Peripherals/Peripherals/`, `C/` = `.../src/Emulator/Cores/`, `tlib/` = `reference/renode-src/tlib/` (LGPL-2.1, **reference only**; do not copy code from it). `File.cs:123` is a line number in the pinned file. Pinned: renode-infrastructure `066a7f13c052215632d469c995c89aea37c573b1` (Renode v1.17.0, build `1.17.0+20260906gitf1dd1b4af`, .NET 8.0.21), tlib `167decf9129758829762e582939128c0694e90d6` (the submodule revision recorded in that tree). The pinned copies that lived in `emulation/{hardware-reference,upstream,references}/` on `main` were byte-identical to the downloads (checked by hash).

---

## 0. Findings that sharpen or contradict DESIGN.md sections 5-7

Short list; details and recommended action in the numbered sections. "Contradicts" means the Rust engine as designed will differ from Renode and the differential tests must expect it.

| # | Finding | DESIGN ref | Verdict |
| --- | --- | --- | --- |
| 1 | Renode's time tick is **1 ns** (`TicksPerNanosecond = 1`). 100 MIPS gives exactly 10 ns/instruction; 80 MHz gives 12.5 ns/cycle, which is *not* an integer number of Renode ticks. Timer events happen at whole-nanosecond times (rounded **up**), restart counting from that rounded time and discard the sub-tick overshoot. Effective period of a `LimitTimer` = `ceil(period_ticks * 1e9 / f)` ns. | s5 "exact integer arithmetic ... 3.84e10" | Rust at 3.84e10 is exact where Renode rounds. Per event the difference is below 1 ns; it accumulates (e.g. SysTick `RELOAD=0x1387F`: Renode 999 988 ns, exact 999 987.5 ns). **[S+M]** micro-benchmarks: the ceil-ns model reproduces every ISR entry index of SysTick (40 events), TIM6 PSC0/ARR1001 (479 events) and PSC2/ARR1001 (426 events); the exact-rational model fails after 8, 6 and 8 events. Section 3.4, 15. |
| 2 | An event at time T is processed after the instruction whose *end* time is >= T (`floor(n/10)` instructions, then 1 more if fewer than 10 ns remain). | s5 "rounding up like InstructionsToNearestLimit" | Confirmed [S+M] (every timer/SysTick ISR entry index in the micro-benchmarks equals `ceil(event_ns / 10)`). Section 5.2. |
| 3 | **Peripheral writes see "clock-source time", not instruction-exact time.** The machine clock source is advanced only when the CPU reports progress (end of a chunk). A chunk ends at the nearest timer limit, at the end of the 100 us quantum, or at the end of the translation block (TB) in which a timer-class register was written (`RequestReturn`). Only a few reads call `SyncTime()` (STM32_Timer CNT, DWT CYCCNT, SysTick CVR). Everything else, including the *start* of a timer, is credited from the chunk start. | s5 "MMIO time: ... exact time" | **Contradicts for writes.** [S+M] measured in the boot (after TIM6 reconfiguration the next tick came ~104 instructions early) and in a controlled micro-benchmark: after enabling TIM6 the first tick arrived at exactly `chunk_start + 99 900` instructions in all 8 trials, i.e. up to **8 915 instructions (89 us) early** versus an exact-time model (chunk start = last quantum boundary or end of the translation block after the previous timer register write). Sections 5.4, 15. |
| 4 | IRQs raised synchronously by an MMIO write are taken at the **end of the current translation block** (tlib block header check), not at the next instruction. TBs end at any PC-changing instruction, WFI/WFE/SVC, page crossing, the instruction budget, 0x7FF instructions. | s5 note on tlib TB end | Confirmed; [M] boot example: UG write at trace index 93 353, `bx lr` at 93 354, ISR entry at 93 355; micro-benchmark: after an `ICSR.PENDSVSET` store followed by k NOPs and a taken branch the PendSV ISR is entered exactly k+2 instructions after the store (k = 0, 1, 2, 3, 6, 12, 25); with PRIMASK set, the pended exception is taken right after `cpsie i`. This is the **first expected divergence** of a PC trace comparison. Sections 5.5, 15. |
| 5 | Dual-machine mode: both machines share one master time source, 100 us quantum, CPUs run in parallel threads. Each machine's clock source advances only to the **minimum** progress of all CPUs [S], so the faster CPU's timers are processed late and non-deterministically (bounded by about a quantum; inferred from the source, consistent with the measured run-to-run differences but not separately measured). Cross-machine events (CAN) are queued with the sender's stamp and run in the sync phase at the end of the quantum, ordered by (stamp, id). CAN stamps are the sender CPU handle's last-reported time (chunk-lagged). | s5 "System" | Quantum/ordering model confirmed; the engine's fixed main-then-handset order is deterministic and lies inside Renode's nondeterminism envelope. Measured envelope: CAN stamps differ up to 79.6 us between two runs, idle-loop PCs differ from ~1.5 s on, LCD/CAN payload order/EEPROM/NOR identical. Section 6. |
| 6 | SysTick period is `RELOAD` ticks (not RELOAD+1) after the first expiry; the first period after enabling starts from the value last written to CVR (or 0xFFFFFF after reset). `COUNTFLAG` is cleared by *any* CSR read, including the monitor's `sysbus ReadDoubleWord`. | s6 "copy Renode's reload/period semantics exactly" | Section 9.5. |
| 7 | STM32 timers wrap at `ARR` ticks (period ARR, not ARR+1); PSC maps to `Divider = PSC+1`. `UG` sets `Value = 0` but keeps the fractional-tick residuum (`ClockEntry.With(value:)` does not clear `ValueResiduum`). Confirmed by main HAL tick 4504 at 4.5 s (ARR 999, PSC 79: 999 us ticks). | s8 table | Section 3.5. |
| 8 | Unmapped addresses: Warning `Read<Width> from non existing peripheral at 0x...` returns 0; writes ignored; no bus fault. | s6, s7 | Confirmed. Observed unmapped accesses during boot are listed in section 12. |
| 9 | Sub-word and unaligned accesses are translated **per peripheral** by `[AllowedTranslations]`; narrow writes to a wider peripheral are **read-modify-write that performs a real device read first**; unsupported widths log a warning and read 0 / ignore the write. Unaligned MMIO: reads are two aligned same-width reads merged; writes are byte writes from the high address down ([S+M], micro `unaligned-mmio`). Table of every peripheral type in the two `.repl` files in section 7.4, verified with 46 bus accesses ([M], `access_probe.py`). | s7 "document them"; s6 "call the bus with the original width and address" | New detail; the unaligned-MMIO rule **contradicts** the s6 wording. |
| 10 | GPIO: `Set(value)` is edge-only (no call when the level is unchanged), notifies receivers in connection order, then hooks; `Connect` pushes the current level immediately. NVIC external lines: pending on a rising edge, `Running` while high, `ICPR` cannot clear pending while the line is high, exception return re-pends while the line is still high. | s5, s6 | Confirmed; section 8, 9.2. |
| 11 | The **first TIM6 ISR in the handset boot** happens at instruction 93 355, entered because of a `UG` event (not a period); then periodic ticks every 99 900 instructions (999 us). | s11 trace plan | Useful anchor for differential tests. Section 12. |
| 12 | `WFI` counts as one executed instruction; idle time is skipped until the nearest clock limit (a SysTick tick wakes exactly at its ceil-ns event: 20 ticks in 20 ms, 7 instructions per tick for `wfi`+5-instruction ISR+`b`). A **masked pending** exception (PRIMASK=1) makes WFI wake, but only **once per time slice**: 2 instructions (`b`, `wfi`) per 100 us instead of a busy loop. | s5 `advance_idle` | NGC firmware never executes WFI in steady state; model WFI as sleep-until-next-event and document the PRIMASK case. Section 5.6, 15. |
| 13 | **Handset release loses one quantum.** The runner's `cpu IsHalted false` (at the 1.05 s poll) enables the CPU's time handle only at the next unlatch; the handset executes its first instruction at 1.0501 s. Measured handset `ExecutedInstructions`: 44 990 000 at 1.5 s, 94 990 000 at 2.0 s, 194 990 000 at 3.0 s, 344 990 000 at 4.5 s, 444 990 000 at 5.5 s (main: exactly 100 000 000 x t). | s5 "fixture polling", s10 "handset held halted until the 50 ms poll releases it" | **Contradicts** unless the Rust system delays the released CPU by one quantum (100 us). Section 5.6. |
| 14 | A level-held peripheral line re-enters its ISR back-to-back: tim6 UIF left set gave 5 consecutive ISR entries 7 instructions apart with **zero** thread instructions between them (exception return re-arbitrates before executing the thread). | s6 | Confirmed [M]. Section 9.2, 15. |

---

## 1. Platform facts used by the reference runs

- CPU: `CPU.CortexM`, `cpuType "cortex-m4"`, `PerformanceInMips = 100` (default, `BaseCPU.cs:360`), global quantum 100 us (`TimeSourceBase.cs:814`). Both fixed in every run; the runner never changes them. [S+M] (`machine ElapsedVirtualTime` prints `Quantum: 00:00:00.000100000`.)
- Time domain mode: `EmulationMode.SynchronizedIO` (first enum member, the default) (`M/Core/Emulation.cs:778`). [S]
- Dual machines have no local time source; both use `Emulation.MasterTimeSource` (`M/Core/Emulation.cs:176-186`). [S] CPUs register as sinks (`M/Core/Machine.cs:1845`).
- `emulation SetGlobalAdvanceImmediately true` (runner default) only removes host-time sleeping (`BaseCPU.cs:812-852`). It does **not** change virtual-time behaviour. [S]
- Boot fixture: `sysbus WriteDoubleWord 0x40006400 0x10000` (CAN MCR), `VectorTableOffset 0x08004000`, `SP 0x20018000`, `PC` = reset PC (monitor "Patching PC ... for Thumb mode" warning is normal). Initial xPSR reads `0x41000000` (T and Z set) and R0-R14 read 0. [M]
- Renode prints elapsed virtual time with nanosecond resolution (`00:00:01.050000000`). The runner's `virtual_seconds()` parses exactly that.

---

## 2. Time base

- `TimeInterval` is a `ulong` of **nanoseconds** (`M/Time/TimeInterval.cs:20,287-292`: `TicksPerNanosecond = 1`, `TicksPerSecond = 1_000_000_000`). `ToString` prints `hh:mm:ss.nnnnnnnnn` (`:241`). [S]
- CPU cycles to time (`TimeInterval.cs:134-146`): `residuumModulus = mips / gcd(mips, 1000)`; `cyclesResiduum = cycles % residuumModulus`; `ticks = (cycles - residuum) * 1000 / mips`. At 100 MIPS the modulus is 1, so **1 instruction = 10 ns exactly**, residuum always 0. [S]
- Time to cycles (`:260-277`): `nanoseconds * mips / 1000` (integer floor), `ticksResiduum = ticks % 1 = 0`. At 100 MIPS: `floor(ns / 10)`. [S]
- `Fraction` (`M/Utilities/Fraction.cs`) is an exact rational over `ulong` numerator/denominator, reduced after each operation (no overflow checking). `ClockEntry.Ratio = Fraction(Step * Frequency, 1e9)`; for 80 MHz this is 2/25 (entry ticks per ns); for 32 768 Hz it is 4096/125 000 000. [S]

**Recommendation.** Keep 3.84e10 ticks/s, but give every Renode-ClockEntry-equivalent event the Renode rounding rule if bit-exact event timing is wanted (3.4). Otherwise accept sub-ns-per-event drift and say so in test tolerances.

---

## 3. ClockEntry, BaseClockSource, LimitTimer

All Renode timers (`LimitTimer`, `ManagedThread`, `ScheduleAction`, SysTick, DWT cycle counter, STM32 timers and their capture/compare channels, RTC and watchdog timers) are `ClockEntry` structs inside **one `BaseClockSource` per machine** (`Machine.cs:47`).

### 3.1 ClockEntry (`M/Time/ClockEntry.cs`)

Fields: `Value` (ulong, entry ticks), `ValueResiduum` (Fraction, fractional entry ticks), `Period` (limit), `Frequency` (Hz), `Step` (default 1), `Direction` (Ascending/Descending), `WorkMode` (Periodic/OneShot), `Enabled`, `Handler`, `Ratio`. Constructor (`:14-30`): initial `Value = Ascending ? 0 : Period`, `ValueResiduum = 0`, `Ratio = Step*Frequency/1e9`.

`With(...)` (`:32-48`) builds a new entry; `Value` is replaced only when passed; **`ValueResiduum` is kept unless `frequency` is passed** (`:47`). So writing `Value` (e.g. STM32 `CNT` write, `UG`) leaves the fractional-tick residuum in place; changing `Frequency` (prescaler, `Divider`) zeroes it. [S]

### 3.2 BaseClockSource algorithm (`M/Time/BaseClockSource.cs`)

State: `clockEntries` (list, **insertion order**), per-entry `unaccountedTimes`, `elapsed` (time advanced but not yet applied), `nearestLimitIn` (time until the first enabled entry reaches its limit; `TimeInterval.Maximal` if none).

- `Advance(time)` (`:201-217`): if `time > nearestLimitIn`, loop `thisTurn = min(nearestLimitIn, left)` calling `AdvanceInner(thisTurn)` until `left == 0`; otherwise one `AdvanceInner(time)`. So one Advance covering several events fires them in time order at their exact ceil-ns times. [S]
- `AdvanceInner(time, immediately)` (`:333-379`): `elapsed += time; totalElapsed += time`. If `nearestLimitIn > time && !immediately` just `nearestLimitIn -= time` (entries are **not touched**; time is integrated lazily). Otherwise `Update(elapsed)` then `elapsed = 0`. Re-entrancy: while an update is running a nested update only sets `reupdateNeeded`; after the handlers finish, `Update(0)` is repeated until the flag stays clear. [S]
- `Update(time)` (`:386-425`): `nearestLimitIn = Maximal`; for each **enabled** entry in list order call its direction handler with `time + unaccountedTimes[i]` (which also lowers `nearestLimitIn`); entries whose handler returned "reached" (and not already run in this top-level update) are queued; **after the whole loop** the queued handlers run in list order. So when several entries expire at the same Advance step, all entry values are already updated and handlers run in creation order. [S]
- Descending handler (`:280-305`): `entryTicks = ticks_ns * Ratio + ValueResiduum` (exact fraction); `reached = entryTicks.Integer >= Value`; `ValueResiduum = entryTicks.Fractional`; if reached: `Value = Period`, `ValueResiduum = 0`, one-shot disables the entry; else `Value -= entryTicks.Integer`. Time to limit: `ceil((Value - ValueResiduum) / Ratio)` ns (the `+1` when the fraction is non-zero, `:299-301`). [S]
- Ascending handler (`:307-331`): `Value += entryTicks.Integer; ValueResiduum = Fractional`; reached iff `Value >= Period`, then `Value = 0`, `ValueResiduum = 0` (**overshoot discarded**). Same ceil rule. A single update fires the handler **once** even if more than one period elapsed (this cannot normally happen because Advance splits at limits). [S]
- `GetClockEntry(handler)` (`:90-148`): returns an enabled entry after applying `elapsed + unaccountedTimes[i]` to it (may call the handler immediately when it reaches its limit); other entries get `elapsed` added to their `unaccountedTimes`; if one of them now exceeds `nearestLimitIn` a full `UpdateLimits()` runs. Used by every `LimitTimer.Value`/`Enabled`/`Limit` getter. [S]
- `ExchangeClockEntryWith(handler, visitor, factory)` (`:59-88`): `UpdateLimits()` (= `AdvanceInner(0, immediately: true)`), apply visitor, `UpdateLimits()` again. So any reconfiguration first flushes the time **already reported to the clock source**, then applies the change, then recomputes `nearestLimitIn`. Time executed by the CPU but not yet reported is **not** included (see 5.4). [S]
- `AddClockEntry` rejects a second entry with the same handler.

### 3.3 LimitTimer (`M/Peripherals/Timers/LimitTimer.cs`)

A thin wrapper: one entry keyed by its own `OnLimitReached` delegate; constructor parameters `limit` (default `ulong.MaxValue`), `direction` (default Descending), `enabled`, `workMode`, `eventEnabled`, `autoUpdate`, `divider`. Effective entry frequency = `Frequency / Divider`.

- `OnLimitReached` (`:247-262`): sets `rawInterrupt = true`; if `eventEnabled` it raises `LimitReached` (all subscribers in subscription order, inside `lock(irqSync)`).
- Setters `Enabled`, `Value`, `Limit`, `Frequency`, `Divider`, `Direction`, `ResetValue` all call `RequestReturnOnCurrentCpu()` (`:316-321`), i.e. `TlibSetReturnRequest()` on the CPU executing the write: the CPU leaves translated code at the end of the current TB (5.4). `Mode` setter does not. [S]
- `Limit` setter with `AutoUpdate` also resets `Value` to 0 (Ascending) or the new limit (Descending) (`:149-170`). `Value` setter rejects `value > initialLimit`.
- `Increment/Decrement` helpers are not used by the NGC platform.

### 3.4 Effective period and event time (consequence of 3.2)

Starting from `Value = 0`, `ValueResiduum = 0` (Ascending), the entry reaches `Period` after the smallest integer `n` ns with `floor(n * f / 1e9) >= Period`, i.e. **`n = ceil(Period * 1e9 / f)`**. After the event the entry restarts from 0 at that integer nanosecond. Hence a free-running periodic timer has period exactly `ceil(P * 1e9 / f)` ns every cycle (no long-term fractional correction).

| Timer | P | f | Renode period | exact period | note |
| --- | --- | --- | --- | --- | --- |
| STM32 TIM6 (handset, main HAL tick) | ARR=999 | 80 MHz / (PSC+1=80) = 1 MHz | 999 000 ns = 99 900 instructions | same | [M] ISR entries 99 900 instructions apart |
| SysTick | RELOAD=0x1387F=79 999 | 80 MHz | 999 988 ns (99 998.8 instructions) | 999 987.5 ns | [M] micro `systick-reload79999`: 40 ISR entries spaced 99 999/99 998 instructions, the shorter spacing exactly every 5th event |
| TIM6 PSC=0, ARR=1001 | 1001 | 80 MHz | 12 513 ns | 12 512.5 ns | [M] micro: 479 entries, spacing 1 251 (335x) / 1 252 (143x), all matching `ceil((t0 + k*12513)/10)` |
| TIM6 PSC=2, ARR=1001 | 1001 | 26.67 MHz | 37 538 ns | 37 537.5 ns | [M] micro: 426 entries, spacing 3 753 (85x) / 3 754 (340x), all matching the ceil model |
| NGCHandsetButtons stimulus thread | period 1 | 10 kHz | 100 000 ns | same | exact |
| 32 768 Hz RTC prescalers | n/a | n/a | model-specific | | see STM32F4_RTC.cs |

[M] The DWT `CYCCNT` samples taken in 12 consecutive SysTick ISRs (micro `wfi-systick-cyccnt`) advance by exactly 79 999 cycles per tick (11 of 11 deltas), consistent with 999 988 ns per tick on an 80 MHz counter whose fractional residuum is carried (it never reaches a limit).

DWT's cycle counter is a `LimitTimer` at the DWT `frequency` (80 MHz), Ascending, limit `ulong.MaxValue`: never reaches its limit; value = floor(ticks since enable); reading it truncates to 32 bits. [S]

### 3.5 STM32_Timer on top of LimitTimer (facts that matter for porting)

Source is pinned at `P/Timers/STM32_Timer.cs` (identical to `emulation/upstream/STM32_Timer.cs`).

- `LimitTimer` with `limit = ARR`, `Divider = PSC + 1`, Ascending, `eventEnabled: true`, `autoUpdate: false` (`:26,62,349-369`). **Period = ARR ticks**. `CEN` write: `Enabled = enableRequested && autoReloadValue > 0` (`:151`).
- `CNT` read calls `cpu.SyncTime()` then returns the entry value (`:322-334`); `CNT` write sets `Value`.
- `UG` (`EGR` bit 0, `:269-297`): `Value = 0` (Ascending) / `autoReloadValue` (Descending), reloads the repetition counter, sets the update flag and pends the interrupt if `UIE` and the update-request-source bit allow, and copies `Value` into each running compare channel timer. Because `ValueResiduum` is kept, the fractional tick accumulated since the last prescaler write is carried into the next period (up to one timer tick shorter).
- Capture channels call `cpu.SyncTime()` before latching `CNT` (`:890-905`), so input capture is instruction-exact (needed by the button stimulus).
- The runner's arithmetic-PWM mode (`NGCLazyPwmTimer`) replaces the periodic events of unconnected TIM2/TIM15 with arithmetic reconstruction; it preserves CNT/SR semantics (`emulation/performance/README.md`).

---

## 4. ManagedThread and ScheduleAction (`M/Core/Machine.cs`)

- `ObtainManagedThread(action, frequencyHz)` (`:131-138`) creates `ClockEntry(period 1, frequency, action, owner, name, enabled: false)` (Ascending, Periodic) inside `ManagedThreadWrappingClockEntry` (`:2190-2260`). The overload taking a `TimeInterval` uses `period.Ticks` at frequency 1e9. [S]
  - `Start()` (`:2209`) only sets `enabled: true`; **`Value` and `ValueResiduum` are kept** (initially 0). The first firing is therefore `ceil(1e9/f)` ns of *clock-source time* after the start; the start itself is applied at clock-source time (lagging the CPU; 5.4).
  - `Stop()` disables, keeping state; `Restart()` enables and sets `Value` to 0 (Ascending) but keeps the residuum; `Frequency` setter calls `With(frequency)` (residuum cleared); `Period` setter sets `period=ticks, frequency=1e9`.
  - The handler runs in whatever thread called `ClockSource.Advance` (a CPU worker thread or the time-source thread) after all entries of that Advance step have been updated.
  - A `stopCondition` callback, when it returns true at a firing, stops the thread instead of running the action.
- `StartDelayed(delay)` (`:2215`) schedules `Start()` plus one immediate `action()` via `ScheduleAction`.
- `ScheduleAction(delay, action)` (`:591-619`): if called from a CPU thread it first `SyncTime()`s (otherwise logs "slight inaccuracy"); `currentTime = ElapsedVirtualTime`; adds a **one-shot** entry `ClockEntry(period = delay.Ticks, frequency = 1e9)` that on firing runs `action(currentTime)` (**the scheduling time, not the firing time**) and removes itself; finally `RequestReturn()` on the CPU. With `delay = 0` the entry fires at the next update. [S]
- Use in the NGC models: `NGCHandsetButtons` (10 kHz stimulus clock), `NGCMainADC` (`1_000_000 / conversionIntervalUs` Hz). No NGC model calls `ScheduleAction`.

**Rust mapping.** Self-rescheduling events work if (a) the first deadline is `start_clock_time + ceil(1e9/f)` ns where `start_clock_time` follows the clock-source-lag rule if bit-exactness matters; (b) a one-shot `ScheduleAction` passes the scheduling time to the callback; (c) all entries that expire in one step update before any handler runs, handlers in creation order.

---

## 5. CPU execution: chunking, time reporting, interrupts

Sources: `P/CPU/BaseCPU.cs`, `P/CPU/TranslationCPU.cs`, `C/Arm-M/CortexM.cs`, `tlib/cpu-exec.c`, `tlib/exports.c`, `tlib/helper.c`, `tlib/arch/arm/{helper.c,translate.c,cpu.h}`.

### 5.1 Granted interval, rounds and chunks

`TimeHandle` grants the CPU an interval (quantum, or the remainder of `RunFor`). `CpuThreadBodyInner` (`BaseCPU.cs:730-910`):

1. `instructionsToExecuteThisRound = interval.ToCPUCycles(mips)` (floor); if `<= executedResiduum` report back unused.
2. `instructionsLeftThisRound = that - executedResiduum` (at 100 MIPS `executedResiduum` is always 0).
3. Loop while not paused/halted and instructions left: `toExecute = min(InstructionsToNearestLimit(), instructionsLeftThisRound)`; optional `skipInstructions` (not used by NGC); `ExecuteInstructions(toExecute)`; `ReportProgress(executed)`.
4. Result handling: `WaitingForInterrupt` -> skip time (5.6); `Interrupted` (return request) or watchpoint -> leave the loop; `Aborted`/MMU fault special.
5. After the loop: if the whole interval was used `ReportBackAndContinue`, otherwise `ReportBackAndBreak(timeLeft)`, which blocks the time grant of all sinks until this CPU has used the rest; the time source then unblocks it and the same interval continues.

A **chunk** is one `ExecuteInstructions` call. [S]

### 5.2 InstructionsToNearestLimit (`BaseCPU.cs:933-950`)

```text
nearest  = machine.ClockSource.NearestLimitIn            // ns, Maximal if no enabled entry
n        = nearest.ToCPUCycles(mips)                     // floor(nearest_ns * 100 / 1000) = floor(ns / 10) at 100 MIPS
if n <= executedResiduum: return 1
n -= executedResiduum
if n != u64::MAX and (nearest == 0 or unusedTicks > 0): n += 1   // unusedTicks is always 0 (1 ns tick)
return n
```

So an event `d` ns ahead is approached by `floor(d/10)` instructions (at least 1). The chunk ends, `ReportProgress` advances the clock by `executed * 10` ns, and `BaseClockSource.Advance` splits at the limit so the handler fires at exactly `now + d` (ceil ns); the *guest* sees its effects before the next instruction. Net effect: **an event at time T takes effect before the first instruction whose start time >= T** (for T a multiple of 10 ns exactly at T; otherwise at the next 10 ns boundary). With no enabled entries the chunk is the rest of the quantum (<= 10 000 instructions). [S]

### 5.3 ReportProgress / ElapsedCycles (`BaseCPU.cs:508-520`)

`ReportProgress(n)` -> `TimeHandle.ReportProgress(FromCPUCycles(n + executedResiduum))` -> `TimeSourceBase.ReportTimeProgress` -> (if this handle held the minimum) `SynchronizeVirtualTime` -> `TimePassed(diff)` -> `Machine.HandleTimeProgress(diff)` -> `clockSource.Advance(diff)` (`Machine.cs:673-679`, skipped only when the **machine** is halted, which is not the same as `cpu IsHalted`). [S]

`cpu ExecutedInstructions` is tlib's total of *executed* instructions (WFI-skipped time is excluded). [S] At 100 MIPS and 50 ms/100 us/1 ms RunFor windows it is exactly `100 000 * ms` (200 consecutive 1 ms snapshots, `reference/data/handset-trace/A-traced/snapshots-1ms.jsonl`). [M]

### 5.4 Clock-source lag: what peripherals see

`SyncTime()` (`TranslationCPU.cs:668-679`) = `ReportProgress(TlibGetExecutedInstructions())`: it reports the instructions executed so far in the *current* chunk — **counting completed translation blocks only**, because tlib advances its executed-instruction counter at block ends; a sync from the k-th instruction of a block therefore sees `10*k` ns less than the instruction-exact time (planner addendum 2026-10-08, confirmed end to end: a TIM7 `CNT` read at main instruction 74 625 145 and the handset `DWT_CYCCNT` sample match Renode only with block-start time; the Rust core passes the block-start count). It is called explicitly, and only, by: STM32_Timer `CNT` read, SMS write in trigger/encoder mode, input-capture latch; DWT `CYCCNT` read; NVIC SysTick `CVR` read; `Machine.ScheduleAction`; `Machine.RealTimeClockDateTime` (grep of the whole pinned tree). Calls from a non-CPU thread (e.g. the monitor reading a register) are ignored with `[ERROR] cpu: Syncing time should be done from CPU thread only. Ignoring the operation` (seen at each checkpoint where the harness reads `DWT_CYCCNT`/`SYST_CVR`; harmless while the machine is paused).

Everything else sees the clock source at the **last report**: the chunk start or the last `SyncTime`. Consequences [S]:

1. A write that enables or reconfigures a clock entry at instruction `k` of a chunk is applied at clock-source time `t_chunk_start`. When the chunk ends, `ReportProgress` credits the entry with the **whole** chunk, including the `k` instructions that preceded the write. A just-started timer runs `k * 10` ns ahead of the write instruction.
2. Every `LimitTimer` setter calls `RequestReturn`, which only sets `exit_request`; the current TB runs to its end (the Renode developers say so in `TranslationCPU.cs:1047`: "TlibSetReturnRequest doesn't finish current translation block"). The write therefore ends the chunk shortly after, and the next chunk starts a few instructions later with an up-to-date clock.
3. CAN transmit timestamps use `TimeHandle.TotalElapsedTime` (5.3) of the sending CPU thread: the **last reported** time, not the instruction time (`BaseCPU.cs:591`, `M/Utilities/TimeDomainExtensions.cs`).
4. The lag is bounded by the chunk length: at most one quantum (10 000 instructions) when no event is near; typically tens to a few thousand instructions.

**[M] Measured example (handset boot, trace `A-traced/pc-trace.u32le`).** TIM6 is reconfigured twice at boot: first `CR1=0`, `ARR=999`, `PSC=3`, `EGR.UG`, `DIER.UIE`, `CR1.CEN` (trace indices 92 376-92 448), then again with `PSC=79` (93 332-93 464). The log (`sysbus LogPeripheralAccess timer6 true`, `reference/data/tim6-boot-log/tim6-access.txt`, produced by `reference/tim6_boot_log.py`) shows these exact writes with their PCs. The UG at index 93 353 resets `Value`; steady-state spacing of TIM6 ISR entries is 99 900 instructions, but the interval between the first (93 355, the UG interrupt) and second entry (193 151) is **99 796**, 104 instructions (1.04 us, about one 1 MHz tick) short. Consistent with: `Value` written while `ValueResiduum` was non-zero and the pre-write part of the chunk credited afterwards. A model that starts counting exactly at the write and clears the residuum would place the second tick near 193 253.

**[M] Controlled experiment** (`micro/` program `timer-enable-lag`, `reference/data/micro/timer-enable-lag/result.json`). TIM6 is configured (ARR=999, PSC=79, UIE; one tick = 999 us = 99 900 instructions), then enabled by a `CR1.CEN` store after D delay-loop iterations that touch no timer register; the ISR clears SR, disables the timer and counts. Eight trials:

| D (iterations) | CEN store at instruction | chunk start (model) | ISR entry | ISR entry - CEN store | exact-time model | early by |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 12 | 0 | 99 900 | 99 888 | 99 900 | 12 |
| 20 | 99 956 | 99 909 | 199 809 | 99 853 | 99 900 | 47 |
| 100 | 200 024 | 200 000 | 299 900 | 99 876 | 99 900 | 24 |
| 400 | 300 716 | 300 000 | 399 900 | 99 184 | 99 900 | 716 |
| 1 500 | 402 915 | 400 000 | 499 900 | 96 985 | 99 900 | 2 915 |
| 3 000 | 505 915 | 500 000 | 599 900 | 93 985 | 99 900 | 5 915 |
| 4 500 | 608 915 | 600 000 | 699 900 | 90 985 | 99 900 | 8 915 |
| 7 000 | 713 915 | 710 000 | 809 900 | 95 985 | 99 900 | 3 915 |

The first tick always arrives at **`chunk_start + 99 900`**, where `chunk_start` is the later of (a) the last quantum boundary (a multiple of 10 000 instructions) and (b) the instruction after the end of the translation block that contained the previous timer-class register write (here the previous ISR's `CR1` store; for D=0 the straight-line configuration code is one TB, so the chunk started at reset). The residual against this model is 0 in all 8 trials. The CEN store itself does not shorten the lag because it only requests the *return* at the end of its TB.

**What to do.** The engine can follow the DESIGN (exact MMIO time) and accept divergences of up to the local chunk length in timer phases after any timer register write (as large as one quantum: 8 915 instructions in the experiment); or emulate Renode's lag exactly by (a) tracking `clock_time` separately from `cpu_time`, (b) defining chunk boundaries as Renode does (nearest limit, quantum, TB end after a timer-class write). (b) needs Renode's TB boundaries (5.5). The first option is recommended; expect the first PC divergence at ~93 354 (5.5) and tick phase differences afterwards.

### 5.5 Interrupts and translation blocks

- `NVIC.IRQ` is a GPIO connected to `cpu@0` (`.repl`). `TranslationCPU.OnGPIO(0, level)` -> `TlibSetIrqWrapped` -> `tlib_set_irq(HARD, level)` (`TranslationCPU.cs:389-407,1634-1655`). The CPU reads the NVIC state through exports (`FindPendingIRQ`, `AcknowledgeIRQ`, `CompleteIRQ`, `OnBASEPRIWrite`, `PendingMaskedIRQ`; `CortexM.cs:970-1030`). [S]
- tlib checks pending interrupts only in the **main loop between TBs** and at each TB header (`tlib/cpu-exec.c:351-412`, `tlib/helper.c:57-86` `prepare_block_for_execution`: returns early when `exit_request` or a pending `exception_index`). A pending HARD interrupt is accepted iff `regs[15] < ARM_M_FNC_RETURN_MIN` (`tlib/arch/arm/translate.c:16646-16690`), so not while PC holds an EXC_RETURN magic value.
- **TB ends** (`translate.c:16565-16630`, `tlib/cpu-exec.c`): after any instruction that sets `is_jmp` (every branch/`bx`/`blx`/`cbz`/`pop {pc}`/PC write, `wfi`, `wfe`, `svc`), page crossing (`(pc - page_base) >= TARGET_PAGE_SIZE`), when `instructions_count_limit - value` is smaller than the block (a shorter TB is retranslated so tlib executes **exactly** the requested count), when `maximum_block_size` (`DefaultMaximumBlockSize = 0x7FF`, `TranslationCPU.cs:2418`) is reached, and for blocks interrupted by `exit_request`.
- A level change raised from an MMIO callback inside a TB therefore takes effect at the **end of that TB**. [M] The `UG` write at TIM6 index 93 353 (`str r3,[r0,#0x14]`, 0x800b290) is followed by `bx lr` (0x800b292, TB end) and the ISR entry (0x8005be0) at index 93 355. **Controlled check** (micro `pendsv-tb-end`): `str r4,[r5]` with `r5 = 0xE000ED04`, `r4 = 1<<28` (PENDSVSET) followed by k NOPs and a taken branch to the next instruction: ISR entry index minus store index = 2, 3, 4, 5, 8, 14, 27 for k = 0, 1, 2, 3, 6, 12, 25 (always k+2: the TB ends at the branch). With `cpsid i; pend; nop x3; cpsie i; nop x5; b`, the PendSV ISR is entered **immediately after `cpsie i`** (index +1; `cpsie` also ends the TB), before the following NOPs. The ISR is 5 instructions (`ldr`, `ldr`, `adds`, `str`, `bx lr`) and exception entry/return add none.
- Exception entry (`do_interrupt_v7m`, `tlib/arch/arm/helper.c:1876`): builds LR from CONTROL/FPCA/mode, calls `tlib_nvic_acknowledge_irq()` (NVIC marks the best pending exception Active, removes Pending, drops `IRQ`). If it returns 0 ("Spurious NVIC IRQ ignored", e.g. BASEPRI raised between pending and entry) nothing happens. Stack frames, FP lazy state etc. follow ARMv7-M/v8-M (see helper.c).
- Exception return: PC >= `ARM_M_EXC_RETURN_MIN` ends the TB; the main loop then runs `do_v7m_exception_exit` -> `tlib_nvic_complete_irq(n)` -> `DeactivateIRQ` (re-pends if the line is still `Running`, 9.2) and **re-runs interrupt arbitration before executing any instruction** (tail-chaining); `cpu-exec.c:378-395`. Entry and return consume zero instructions. [S]
- `WFI`: `cpu_has_work` is true when `tlib_nvic_get_pending_masked_irq() != 0` = `NVIC.MaskedInterruptPresent`, which **ignores PRIMASK** (`NVIC.cs:81-135`). [S]
- `PRIMASK`/`FAULTMASK` writes call `tlib_nvic_find_pending_irq()`; `BASEPRI` writes call `OnBASEPRIWrite` -> `NVIC.BASEPRI_NS` setter (value masked by `priorityMask` 0xF0, `FindPendingInterrupt` re-run) (`helper.c:3885-3930`, `NVIC.cs:933-950`). tlib keeps the unmasked 8-bit value for `MRS BASEPRI` reads (so a write of 0x55 reads back 0x55, while NVIC uses 0x50). [S]

### 5.6 WFI, halt, reset

- `ExecutionResult.WaitingForInterrupt` (WFI or Lockup): `instructionsToSkip = min(InstructionsToNearestLimit(), instructionsLeftThisRound)`; with `AdvanceImmediately` no sleeping; `ReportProgress(instructionsToSkip)` (time passes, instructions are not "executed"). [S]
- `cpu IsHalted true` (the runner's handset hold): the CPU's TimeHandle is disabled, so grants "behave like the whole time was used up" (`TimeHandle.cs:146-180`); `ExecutedInstructions` stays 0; the *machine* keeps advancing its clock source (the machine-level `IsHalted` is a different property), so handset peripherals' timers run while its CPU is held. **Release costs one quantum**: `cpu IsHalted false` only sets `TimeHandle.DeferredEnabled` (`BaseCPU.cs:384-389`), which takes effect at the next unlatch, so the handset executes nothing in the first 100 us quantum after the release. [M] The release was seen at the 1.05 s poll in both reference runs (`handsetReleaseNs = 1050000000`), but the handset's `ExecutedInstructions` is **44 990 000 at 1.5 s, 94 990 000 at 2.0 s, 194 990 000 at 3.0 s, 344 990 000 at 4.5 s, 444 990 000 at 5.5 s** (i.e. `(t - 1.0501 s) * 100 MIPS`; the main board reads exactly `100 000 000 * t`). The handset's first instruction therefore starts at 1.0501 s. The Rust runner must delay the released CPU by one quantum to reproduce this, otherwise every handset-side count is 10 000 higher and every handset timer event 100 us earlier than in the reference.
- **[M] WFI measurements** (micro `wfi-systick`, `wfi-primask`, `wfi-systick-cyccnt`, `reference/data/micro/`): with SysTick (RELOAD 79 999, TICKINT) and `loop: wfi; b loop`, 20 ms produced 20 ISR entries and **148 executed instructions** = 7 setup + 21 `wfi` + 20 x (5-instruction ISR) + 20 `b`; each tick costs 7 instructions (ISR entries 7 apart), i.e. `wfi` is counted as an executed instruction and skipped time is not. The tick wakes the CPU exactly at its ceil-ns event (CYCCNT deltas 79 999 cycles, 3.4). With `cpsid i` first, the pended SysTick never runs the ISR, but WFI still wakes (PRIMASK is ignored for the wake condition): the CPU then executes **2 instructions (`b idle`, `wfi`) per 100 us time slice** (executed count 9 at 0.9 ms, 10, 11, 13, 15, ... in the following 0.1 ms steps; +2 extra at each SysTick event), because after a WFI result the rest of the granted round is skipped (`instructionsToSkip = min(nearest limit, left in round)`). On hardware the CPU would not sleep at all in that case; no NGC firmware path does this.

### 5.7 Instruction counting

- Each executed Thumb instruction (16 or 32 bit) counts 1; IT-skipped instructions are decoded as part of their TB and counted (DESIGN s6; not re-verified here). `ExecutedInstructions` is exactly `100 000 * ms` at every 1 ms boundary of the handset reference. [M]
- `tlib_execute(max_insns)` executes **exactly** `max_insns` instructions unless it returns early (`exit_request`, WFI, exception to C#). `TranslationCPU.cs:741` asserts `executed <= requested`. [S]

---

## 6. Time sources, quantum and cross-machine events

Sources: `M/Time/{TimeSourceBase,MasterTimeSource,HandlesCollection,TimeHandle}.cs`, `M/Core/Machine.cs`, `emulation/models/NGCCANLink.cs`.

- `emulation RunFor "s"` -> `MasterTimeSource.RunFor(period)`: `while (period > 0) { InnerExecute(out elapsed, period); period -= elapsed; }` (`MasterTimeSource.cs:60-72`). Each `InnerExecute` advances `NearestSyncPoint` by `min(remaining, Quantum)` and grants that interval to all handles (`TimeSourceBase.cs:372-460`). The quantum grid is aligned to the start of the *emulation* only if every RunFor length is a multiple of the quantum; all runner calls (50 ms) and all REF harness calls (1 ms, 50 ms) are. [S]
- Parallel execution (default; `ExecuteInSerial` false): all CPU threads run their granted interval concurrently, then `WaitUntilDone` for each; then the **sync phase** (`ExecuteSyncPhase`, `:763-795`): `SyncHook`, then delayed actions whose `when <= now`, ordered by `(When, Id)` (`:1031-1035`), executed on the time-source thread while every CPU is stopped at the barrier. [S]
- `Machine.HandleTimeDomainEvent(handler, arg, stamp)` in `SynchronizedIO` calls `LocalTimeSource.ExecuteInSyncedState(callback, stamp)`: queued with the sender's stamp (a stamp from another time domain is replaced by "now"), run at the first sync point with `now >= stamp` (`Machine.cs:210-232`, `TimeSourceBase.cs:119-126`). NGCCANLink stamps frames with `TimeDomainsManager.GetEffectiveVirtualTimeStamp()` = the sending CPU handle's last reported total time. **Delivery = the end of the quantum in which the frame was transmitted**; the receiving CPU sees the IRQ from its first instruction of the next quantum (the CAN IRQ goes through NVIC like any other, 9.2). [S]
- `STMCAN` raises `FrameSent` synchronously from the register write (no timer; `P/CAN/STMCAN.cs:518-520`), so no bus time is modelled; `OnFrameReceived` writes the FIFO and raises IRQs from the sync-phase thread. [S]
- **Clock-source advance in dual mode** (`TimeSourceBase.cs:655-677`, `HandlesCollection.cs:133-170`): the master's virtual time and, through `TimePassed`, **every machine's clock source** advance only to the *minimum* `TotalElapsedTime` over all handles (`TryGetCommonElapsedTime`). A faster CPU therefore executes chunks computed from a clock source that does not yet include its own recent progress; its timer events fire later (in host time, at the slow CPU's progress report), i.e. at a later *instruction position* of the fast CPU, by up to roughly one quantum. This is the origin of Renode's run-to-run variation. [S]
- **[M] Envelope** (two fresh dual runs, identical configuration, `reference/data/dual-wake-run1-vs-dual-wake-run2/diff.json`): handset release 1.05 s in both; checkpoints at 0.5/1.0/1.05 s fully identical; from 1.5 s main PC/R3 differ (idle loop), SRAM1 differs in 8 (1.5 s) to 373 (4.5 s) bytes (exception frames, counters, buffers), handset SRAM1 15-63 bytes, SRAM2 identical, `executedInstructions`, virtual time, SCS/NVIC/GPIO registers, current-TCB words, LCD bytes, UART tails, ADC/EEPROM/QSPI summaries, HAL tick/mode/readiness, EEPROM/NOR/RTC files identical; all 52 CAN frames identical in source, ID, payload, order; CAN stamp differences up to 79.6 us (mean 11.4 us, 38 of 52 frames non-zero).

**Rust model.** DESIGN s5's fixed order per 100 us quantum (main, then handset), frames delivered at the quantum boundary in (stamp, board order) is a valid deterministic member of the envelope above. Differences to expect against Renode: CAN trace timestamps (Rust: exact transmit time), interrupt arrival phase after timer events, idle-loop PCs.

---

## 7. System bus

Sources: `M/Peripherals/Bus/SystemBus.cs`, `SystemBusGenerated.tt` (the generated `Read/Write{Byte,Word,DoubleWord,QuadWord}` bodies), `M/Core/Extensions/ReadWriteExtensions.cs`, `M/Peripherals/Bus/AllowedTranslation*.cs`, `M/Peripherals/Memory/{MappedMemory,ArrayMemory}.cs`, `tlib/include/softmmu_template.h`.

### 7.1 Dispatch

- `SystemBus.Read<W>(address, context, cpuState)` (generated template `SystemBusGenerated.tt:~50-95`): (1) permission and locked-range checks (not used here); (2) tag override (not used); (3) `TryFindPeripheralAccessMethods(address)`: if no registered range contains the address, `ReportNonExistingRead`; (4) otherwise `lock(accessMethods.Lock) { methods.Read<W>((address - startAddress) + registrationOffset) }`. The peripheral receives an **offset relative to its registration base** and the **access width as issued** (after the translation of 7.3). Writes mirror this. [S]
- **Unmapped** (`SystemBus.cs:2505-2590`): default `UnhandledAccessBehaviour = Report` (first enum member): Warning `"[cpu: 0xPC] Read<Byte|Word|DoubleWord|QuadWord> from non existing peripheral at 0x<addr>."` and return **0**; writes: Warning `"... Write<W> to non existing peripheral at 0x<addr>, value 0x<v>."`, ignored. No bus fault; no exception. The log line is emitted for every access (Renode's logger collapses repeated identical lines). [S+M] (monitor and CPU paths; real firmware examples in section 12.)
- **Sizes/regions.** Every peripheral registers `[address, address + Size)` (`IKnownSize.Size` or the `<base, +size>` form). Addresses inside the range but not defined by the peripheral are the peripheral's problem (register framework: Warning + 0, section 11). `MappedMemory` regions are page-granular and zero-initialised (`MappedMemory.cs`, `Init`), reads/writes outside the declared size log an Error and return 0 / are dropped.
- **Plain memory fast path.** CPU loads/stores to `MappedMemory` (flash, SRAM1, SRAM2) go straight to host memory through the tlib TLB and never reach the bus, so no warnings, no hooks. Flash is ordinary writable RAM from the CPU's view. `sysbus LoadBinary file 0x08004000` writes the file through the bus into the flash `MappedMemory`; bytes outside the image stay 0x00 (`M/Core/Extensions/FileLoaderExtensions.cs`). [S]
- The monitor commands `sysbus ReadByte/ReadWord/ReadDoubleWord/WriteDoubleWord/ReadBytes` use the same bus path with `context = null`: **they have the register side effects of a real read/write** (e.g. clearing NVIC COUNTFLAG, popping a UART FIFO). The REF harness restricts itself to side-effect-free registers (`reference/checkpoint.py`).

### 7.2 ArrayMemory (PWR, FLASH control, FMC, SYSCFG fixtures)

`ArrayMemory` (`M/Peripherals/Memory/ArrayMemory.cs`) implements all four widths natively (`IExecutableIO`), little-endian host order, plain read-back of whatever was written (no masks, no reset values, zero initial). Any access that does not fit entirely inside the array (`offset > size - width`) logs an **Error** `Tried to read/write N byte(s) at offset ... outside the range of the peripheral` and reads 0 / ignores the write. Sub-word accesses are native (no translation needed). The size comes from the `.repl` (`pwr` 0x400 @0x40007000, `flashControl` 0x400 @0x40022000, `fmc` 0x1000 @0xA0000000, `syscfg` 0x400 @0x40010000). [S]

### 7.3 Access-width translation

The bus chooses, **per peripheral and per width**, the methods it will call (`FillAccessMethodsWithDefaultMethods`, `SystemBus.cs:1845-2112`).

1. If the peripheral implements the interface for the requested width (`IBytePeripheral`, `IWordPeripheral`, `IDoubleWordPeripheral`, `IQuadWordPeripheral`) that method is used directly.
2. Otherwise the bus looks for a wider/narrower interface that the peripheral implements **and** that is enabled in its `[AllowedTranslations(...)]` attribute (class attribute, inherited by subclasses). Preference order per missing width:
   - Byte: QuadWord, DoubleWord, Word;
   - Word: QuadWord, DoubleWord, Byte;
   - DoubleWord: QuadWord, Word, Byte;
   - QuadWord: DoubleWord, Word, Byte.
   (Wrapper variants, used only when hook/logging wrappers are installed, win over plain peripheral variants.)
3. If none applies the width is "not translated": `ReadXNotTranslated` logs Warning `"Attempted <Width> read isn't supported by the peripheral. Offset 0x<o>."` and returns **0**; `WriteXNotTranslated` logs `"... write ... Offset 0x<o>, value 0x<v>."` and **drops the write** (`ReadWriteExtensions.cs:1240-1296`). [S]

Translation implementations (`ReadWriteExtensions.cs`, little-endian bus):

| Requested | Peripheral native | Behaviour |
| --- | --- | --- |
| byte read | dword | `(byte)(ReadDoubleWord(addr & ~3) >> ((addr & 3) * 8))` (`:636-643`) |
| halfword read | dword | `(ushort)(ReadDoubleWord(addr & ~3) >> ((addr & 3) * 8))` (`:732-739`); at offset 3 only the top byte survives |
| byte write | dword | **read-modify-write**: `old = ReadDoubleWord(addr & ~3)` (a real read, side effects included), merge the byte at `(addr & 3) * 8`, `WriteDoubleWord(addr & ~3, merged)` (`:659-668`) |
| halfword write | dword | same RMW with a 16-bit mask (`:755-764`) |
| any wider read/write | narrower native | repeated narrow accesses, **ascending addresses**, little-endian composition (`:20-60`: word via two byte reads/writes, etc.) |

[S] Consequences: writes to `NVIC`, `STM32_Timer`, `STM32F7_USART` registers with `STRB`/`STRH` perform a hidden read first; for write-only/clear-on-read registers this changes state. Writes through STM32_GPIOPort `BSRR` halfword stores read `BSRR` (returns 0) first.

### 7.4 Access widths accepted by every peripheral in `handset.repl` and `main.repl`

"Native" = implements the width. "Attr" = `[AllowedTranslations]`. Result columns: OK, RMW (narrow write via dword), or **W0** = Warning `isn't supported by the peripheral`, read 0, write dropped.

| `.repl` name(s) | Class | Native | Attr | byte | halfword | word | Source |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `nvic` @0xE000E000 (size 0x1000) | `IRQControllers.NVIC` | D | Byte->D, Word->D | OK / RMW | OK / RMW | OK | `C/Arm-M/NVIC.cs:29` |
| `dwt` @0xE0001000 (0x1000) | `Miscellaneous.DWT` | D | none | W0 | W0 | OK | `C/Arm-M/DWT.cs:15` |
| `flash`, `sram1`, `sram2` | `Memory.MappedMemory` | B,W,D,Q | n/a | CPU fast path (plain memory) | | | `MappedMemory.cs:36` |
| `pwr`, `flashControl`, `fmc`, `syscfg` | `Memory.ArrayMemory` | B,W,D,Q | n/a | native | native | native | 7.2 |
| `rcc` | `Miscellaneous.NGCClockControl` (own) | D | none | W0 | W0 | OK | `emulation/models/NGCClockControl.cs` |
| `exti` | `IRQControllers.STM32F4_EXTI` | D | none | W0 | W0 | OK | `P/IRQControllers/STM32F4_EXTI.cs:17` |
| `gpioA`..`gpioH` (0x400 each) | `GPIOPort.STM32_GPIOPort` | D | **Word->D only** | W0 | RMW | OK | `P/GPIOPort/STM32_GPIOPort.cs:21` |
| `timer3`, `timer6`; main `timer4`, `timer7` | `Timers.STM32_Timer` | D | Byte->D, Word->D | RMW | RMW | OK | `STM32_Timer.cs:23` |
| handset `timer2`, `timer15` | `Timers.NGCLazyPwmTimer` (own, derives STM32_Timer) | D | inherited + own identical attribute | RMW | RMW | OK | `emulation/models/NGCLazyPwmTimer.cs:14` |
| `usart3`; main `usart1`, `usart2`, `uart4`, `uart5` | `UART.STM32F7_USART` | D | Byte->D, Word->D | RMW | RMW | OK | `STM32F7_USART.cs:18` |
| `can1` (<0x40006400, +0x400>) | `CAN.STMCAN` | D | none | W0 | W0 | OK | `P/CAN/STMCAN.cs:22` |
| `i2c1`, main `i2c2` | `I2C.STM32F7_I2C` | D | **Byte->D only** | RMW | W0 | OK | `STM32F7_I2C.cs:18` |
| `rng` | `Miscellaneous.STM32_RNG` | D | none | W0 | W0 | OK | `STM32_RNG.cs:17` |
| `rtc` | `Timers.STM32F4_RTC` | D | none | W0 | W0 | OK | `STM32F4_RTC.cs:19` |
| `iwdg` | `Timers.STM32_IndependentWatchdog` (Basic dword) | D | none | W0 | W0 | OK | `STM32_IndependentWatchdog.cs:18` |
| `crc` | `CRC.STM32_CRC` | B,W,D | none | native | native | native | `STM32_CRC.cs:20` |
| main `dma1`, `dma2` | `DMA.STM32LDMA` | D | none | W0 | W0 | OK | `STM32LDMA.cs:17` |
| handset `lcd` @0x60000000 (size 0x20004) | `Video.NGCParallelLCD` (own) | W,D | none | W0 | native | OK (low 16 bits) | `emulation/models/NGCParallelLCD.cs` |
| handset `adc`; main `adc` @0x50040000 | `NGCAdc` / `NGCMainADC` (own) | W,D | none | W0 | native | OK | models |
| handset `buttons` @0x61000200, both `outputTelemetry` @0x61000300 | own | D | none | W0 | W0 | OK | models |
| main `qspi` @0xA0001000 | `SPI.NGCQuadSPI` (own) | B,D | none | native | W0 | OK | models |
| main `eepromStore` @0xF0001000 (2048) | `Memory.NGCEepromStore` (own) | B | none | native | W0 | W0 | models |

Evidence: **no `isn't supported by the peripheral` warning occurred in either reference run** (handset-only to 3.9 s, dual to 5.5 s), so the unsupported-width cases did not matter for the boot paths. [M] The table itself was verified with `reference/access_probe.py` (46 monitor bus accesses on `dual.resc`, `reference/data/access-probe/result.json`): gpioA byte read/write, can1 and DWT halfword/byte, i2c1 halfword write, rtc/rcc/lcd byte reads, main eepromStore halfword read and qspi halfword read all produced `Attempted <Width> read|write isn't supported by the peripheral` and read 0; timer6/i2c1/NVIC/SCB byte writes and gpioA/timer6 halfword writes performed the dword read-modify-write (e.g. `WriteByte 0xE000ED22 0x40` then `ReadDoubleWord 0xE000ED20` = `0x00400000`; NVIC `IPR0` byte write 0xFF read back 0xF0 with the priority-mask warning); crc byte accesses and main eepromStore/qspi byte reads were native; an undefined register inside a dword peripheral (gpioA 0x2C, dma1 0xA8) gave `Unhandled read/write from offset ...`; ArrayMemory `ReadDoubleWord 0x400073FE` logged the out-of-range Error and returned 0; unaligned dword reads of plain memory (`0x20000101`) and ArrayMemory (offset 2) return the little-endian bytes at the unaligned offset (`0x00112233`, `0x0000AABB`).

### 7.5 Unaligned and page-crossing accesses (tlib)

For addresses on pages classified as MMIO (everything that is not `MappedMemory`) `softmmu_template.h` (`:231-234,339-348,452-455,514-560`) treats an access whose address is not naturally aligned as:

- **load**: two aligned loads of the **same width** at `addr & ~(size-1)` and that address + `size`, merged by shift (`res = (res1 >> shift) | (res2 << (size*8 - shift))`), i.e. two MMIO reads with side effects;
- **store**: `size` single-**byte** MMIO stores from the highest byte down to the lowest (`for i = size-1 .. 0`).

Naturally aligned MMIO accesses go to the bus as one access of the issued width. Unaligned accesses to `MappedMemory` within a page are plain host accesses; page crossings use the same split. Whether an unaligned access faults is controlled by CCR.UNALIGN_TRP / the `allow_unaligned_accesses` flag of the Cortex-M profile (no unaligned MMIO access occurred in the firmware references). [S] **[M] Controlled check** (micro `unaligned-mmio`, `reference/data/micro/unaligned-mmio/result.json`, TIM6 access log): `str r1,[r0,#0x28]` with `r0 = TIM6+2` (32-bit store at TIM6+0x2A, value 0xAABBCCDD) produced exactly four bus accesses `WriteByte 0x2D = 0xAA`, `0x2C = 0xBB`, `0x2B = 0xCC`, `0x2A = 0xDD` (descending addresses), each translated to a dword read-modify-write by the Byte->D rule (ARR ended 0xAABB, PSC 0: bits 16+ are not implemented); the matching 32-bit load at TIM6+0x2A produced two aligned `ReadUInt32` accesses (0x28 then 0x2C) merged into `0xAABB0000`.

---

## 8. GPIO propagation

Sources: `M/Core/GPIO.cs`, `GPIOGate.cs`, `M/Peripherals/GPIOPort/BaseGPIOPort.cs`, `P/GPIOPort/STM32_GPIOPort.cs`, `P/IRQControllers/{STM32F4_EXTI,STM32_EXTICore}.cs`, `M/Peripherals/Miscellaneous/CombinedInput.cs`.

- `GPIO.Set(bool)` (`GPIO.cs:42-58`): returns immediately when the level is unchanged. Otherwise it stores the level, calls `OnGPIO(number, level)` on every endpoint **in connection order, synchronously on the calling thread**, then invokes the state-changed hooks. `Toggle`, `Unset`, `Blink`-style helpers are `Set` sequences. [S]
- `GPIO.Connect(receiver, n)` (`:68-83`): ignored when that (receiver, n) pair already exists; otherwise appended and the receiver is **immediately** called with the current level (also when it is low). `.repl` fan-out `a -> b@1 | c@2` connects in left-to-right order. [S]
- `GPIOGate` (not used by the NGC platform) ORs several sources into one line. `CombinedInput` (used for EXTI lines 5-9 and 10-15): `inputStates[n] = level; OutputLine.Set(any input high)` (`CombinedInput.cs:31-41`): a level OR, edge-propagating through `Set`. [S]
- **STM32F4_EXTI** (24 lines, `firstDirectLine = 23`): lines 0-22 are *configurable*, line 23 is direct. `OnGPIO(n, level)` (`STM32F4_EXTI.cs:38-52`, `STM32_EXTICore.cs:25-50`): masked lines (IMR bit clear) ignore the input; configurable lines accept only an edge selected by RTSR (rising) / FTSR (falling); on an accepted event the pending bit is set and `Connections[n].Set(true)` (a **latched** high output that stays until `PR` is written with 1; `PR` is write-1-to-clear and unsets the output). Direct lines follow the input level (and clear `PR` when low). `SWIER` write: `Connections[x].Set()` for `value & IMR`. `EMR` is storage only. [S]
- **STM32_GPIOPort** (`STM32_GPIOPort.cs:108-112`): `OnGPIO(n, level)` first runs `BaseGPIOPort.OnGPIO` (updates the input state seen in IDR) and then `Connections[n].Set(level)` (input pins forward to whatever the `.repl` connected, e.g. `gpioC` pin 1 -> `exti@1`). Output pins drive `Connections[n]` from ODR/BSRR according to MODER/AF. Port modes reset to `modeResetValue` (`0xabffffff` for A, `0xfffffebf` for B, 0 otherwise). Register-level details (BSRR/LCKR/AFR/ASCR) are in the pinned source; the porter must read it. Observed unhandled access: offset 0x2C (ASCR on L4) read/write on gpioA/gpioC in both boards: Warning + 0 (section 12). [S+M]
- Platform wiring used (from the `.repl`s): `gpioC:1 -> exti@1` (handset), `gpioA:5 -> exti@5` (main), `lcd.TE -> gpioD@3 | exti@3`, `gpioB:4 -> lcd@0`, `buttons.PE3 -> gpioE@3 | timer3@0`, `buttons.PE5 -> gpioE@5 | timer3@2`, `gpioB:15 -> outputTelemetry@0`, EXTI lines 0-4 -> NVIC 6-10 (external IRQ numbers; NVIC adds 16), 5-9 -> `exti5to9` -> NVIC 23, 10-15 -> `exti10to15` -> NVIC 40.

---

## 9. NVIC and system control space (`C/Arm-M/NVIC.cs`, 142 KB)

`NVIC` is a bus peripheral at 0xE000E000, size 0x1000 (covers all of 0xE000E000-0xE000EFFF: SysTick, NVIC, SCB, MPU, FP control). It accepts byte/halfword accesses through the dword translation of 7.3 (RMW), so `NVIC->IP[n] = byte`, `SCB->SHP[n] = byte` perform a hidden read of the aligned register. Nothing else exists in the private peripheral bus except `DWT` at 0xE0001000: **FPB (0xE0002000) and DBGMCU (0xE0042000) are unmapped** (reads return 0 with a Warning). [S+M]

### 9.1 State

Per exception number (0..IRQCount): flag set `{Enabled, Pending, Active, Running}`, byte priority, plus a `SortedSet<int> pendingIRQs`, a `Stack<int> activeIRQs`, banked BASEPRI and PRIGROUP. Reset (`InitInterrupts`, `NVIC.cs:1780-1802`): exceptions 0-15 are **Enabled**, external IRQs (16+) disabled, all priorities 0. [S]

### 9.2 Interrupt lines: level, pending, running

- `OnGPIO(n, level)` (`NVIC.cs:145-176`; `n += 16`): **high**: set `Running` and `Pending`; **low**: clear `Running` only (pending is *not* cleared). Then `FindPendingInterrupt()`. So a pulse (high then low) leaves the interrupt pending until taken; a level held high keeps `Running` set.
- `ClearPending` (ICPR, ICSR *CLR bits, `:2406-2421`) is a **no-op while `Running`** (line still high; logs "Not clearing pending IRQ ... currently running").
- `AcknowledgeIRQ` (`:476-493`, called by exception entry): picks `FindPendingInterrupt()`, sets `Active`, clears `Pending`, pushes onto `activeIRQs`, drops the CPU `IRQ` line.
- `CompleteIRQ` (`:434-474`, exception return): top-of-stack must match; `DeactivateIRQ` (`:2382-2404`): clear `Active`, pop; **if the line is still `Running`, set `Pending` again** (level re-pend), otherwise if pending stays pending, else inactive.
- `FindPendingInterrupt` (`:81-135`): candidates are exceptions with `Pending & Enabled & !Active` (`IsCandidate`, `:2650`); best = lowest *full* (non-grouped) priority, ties by lower exception number (`DoesAPreemptB`, `:2336-2357`). If something is active, the candidate must have **group priority** strictly lower than the current raw execution priority. `IRQ.Set(groupPriority < ExecutionPriority)` (execution priority includes the PRIMASK/BASEPRI/FAULTMASK boost); `maskedInterruptPresent = groupPriority < ExecutionPriority(ignore PRIMASK)` (the WFI wake condition). Returns null and **deasserts `IRQ`** when nothing qualifies. [S]
- A level-held peripheral line therefore re-enters its ISR repeatedly until the peripheral drops the line; an edge-pending line enters once. [M] micro `level-irq-reentry`: `EGR.UG` sets TIM6 UIF with UIE (line high) and the ISR (7 instructions on non-clearing entries) clears SR only on its 5th entry: ISR entries at trace indices 13, 20, 27, 34, 41, spacing exactly 7, **zero thread instructions between entries**; the first entry is 6 instructions after the UG store (4 NOPs + `b spin` end the TB first), and after the 5th entry (which cleared SR) the thread resumes.

### 9.3 Priorities, grouping, BASEPRI

- Priority registers store `value & priorityMask`; the platform sets `priorityMask: 0xf0` (4 implemented bits). A write with other bits set logs Warning `"Trying to set the priority for interrupt N to 0x.., but it should be maskable with 0xF0"` (seen once per board at boot, interrupt 16 = IRQ 0, written 0xFF) (`NVIC.cs:1804-1880`). Reads return the masked value. [S+M]
- `GetExceptionPriority`: Reset -4, NMI -2, HardFault -1 fixed (-3 for the secure alias, unused), others from the table; group priority = `priority & ~((1 << (PRIGROUP+1)) - 1)` (`:2559-2600`). `BASEPRI` boost = group priority of BASEPRI when nonzero; PRIMASK boost 0; FAULTMASK boost -1; `GetPriorityBoost` (`:2479-2541`); an exception becomes active iff group priority < min(raw active priority, boost).
- `AIRCR.PRIGROUP` is `binaryPointPosition`. TrustZone paths exist but are disabled for Cortex-M4.

### 9.4 Exceptions in tlib

See 5.5 for entry/return, tail-chaining and the TB-end latency. Synchronous faults (UsageFault, BusFault, MemManage, SVC, BKPT, UDF) are pended through `v7m_raise_synchronous_exception` -> `nvic.SetPendingSynchronousFault`, with escalation to HardFault when masked or lower priority (`NVIC.cs:189-300`, `:2261-2335`). **The NGC firmware never faults in either reference run** (CFSR/HFSR read 0 at every checkpoint), so none of this is exercised by the references. [M]

### 9.5 SysTick (`NVIC.cs:2841-2965` class `SysTick`, registers `:1023-1075`)

- Implemented as a `LimitTimer` with **limit = 0xFFFFFF**, `Descending`, `eventEnabled: true`, `AutoUpdate = true` after `Reset`, frequency = `systickFrequency` (80 MHz), initial `Value` = 0xFFFFFF, disabled. `LimitReached` handler: `COUNTFLAG = true`; if TICKINT, pend SysTick (exception 15); if `Reload == 0` disable the timer, else **`systick.Value = Reload`** (so the entry counts `Reload` ticks, then fires again: **period = RELOAD ticks**, not RELOAD+1). [S]
- CSR write: ENABLE -> `systick.Enabled = value` (unless Reload == 0: stays off until RELOAD becomes non-zero, then `Value = Reload`, enabled), TICKINT stored, CLKSOURCE reads 1, **COUNTFLAG is read-to-clear (any CSR read clears it, including monitor reads)**. RVR write stores `Reload` (24 bits). CVR write: `Value = Reload` and `COUNTFLAG = false` (disables first when Reload == 0); CVR read: `cpu.SyncTime()` then the entry `Value` (a remaining-ticks count, not an up-counter). CALIB: TENMS = `(freq / 100) & 0xFFFFFF`, SKEW = `freq % 100 != 0`, NOREF = 1. [S]
- First period after enabling starts from the `Value` at that moment: normally the value just written to CVR (= RELOAD). With the NGC firmware RVR reads 0x1387F (79 999), so the steady period is **999 988 ns** (79 999 cycles = 999 987.5 ns, rounded up by 3.4): 12 ns shorter than the 1 000 000 ns of a RELOAD+1 SysTick, i.e. the tick runs about 12 ppm fast. [S+M] micro `systick-reload79999` (RVR 79 999, CVR write, CSR=7): the first ISR entry is at instruction 99 999 (the enable store is at instruction 6; the entry equals `ceil(999 988 / 10)` because the chunk containing the store started at t = 0), then 39 more entries at spacings of 99 999 (31 times) and 99 998 (8 times, exactly every 5th event), all reproduced by `entry_k = ceil((t0 + k * 999 988) / 10)`; the rational 999 987.5 ns model diverges after 8 events. At the 3.9 s handset checkpoint `SYST_RVR = 0x1387F`, `SYST_CVR = 0x5050`. [M]
- Also: `PENDSTSET`/`PENDSTCLR` in ICSR set/clear the SysTick pending bit through the same `SetPending`/`ClearPending`.

### 9.6 Quirks worth knowing

- `ICSR.RETTOBASE` is computed as `activeIRQs ∩ SystemException enum <= 1` (counts only *system* exception numbers in the active stack), not "exactly one exception active" (`NVIC.cs:1076-1090`). `VECTPENDING` = `FindPendingInterrupt()`; `VECTACTIVE` = top of the active stack (0 if empty). `ISRPENDING` is a tagged (unmodelled) flag.
- WFI wake-up and `PendingMaskedIRQ` use the PRIMASK-ignoring condition (9.2).
- Priority/enable writes re-run `FindPendingInterrupt()` immediately (the `IRQ` level can change inside the write).

---

## 10. DWT (`C/Arm-M/DWT.cs`)

`DWT @ 0xE0001000`, size 0x1000, dword only. `CTRL.CYCCNTENA` (bit 0) enables a `LimitTimer` at the DWT frequency (80 MHz), Ascending, `Value` writable. `CYCCNT` (offset 4): write sets the timer value; read **calls `cpu.SyncTime()` when a CPU context exists** and returns the low 32 bits. All other defined registers are tags (storage-less, read 0, writes warn once as unhandled bits); PID/CID registers return fixed values (PID4 0x04, PID0 0x02, PID1 0xB0, PID2 0x1B, CID0..3 0x0D 0xE0 0x05 0xB1). The firmware enables CYCCNT early (handset: CYCCNT = 286 213 357 at 3.9 s, i.e. enabled at ~0.32 s). [S+M]

---

## 11. Register framework (`M/Core/Structure/Registers/`)

Used by every `Basic*Peripheral`-derived model and by most STM32 models (timers, USART, I2C, RTC, EXTI, GPIO, DMA, CAN partly).

- **Collection lookup** (`RegisterCollection.cs:175-245`): registers are keyed by exact offset. `Read(offset)` with no register: Warning **`Unhandled read from offset 0x<o>.`**, returns 0. `Write` with no register: Warning `Unhandled write to offset 0x<o>, value 0x<v>.`, dropped. Before/after read/write hooks (monitor feature) are unused. [S+M]
- **Read** (`PeripheralRegister.cs:ReadInner`): (1) register-level before-read handlers; (2) for each field in definition order: `UnderlyingValue = field.valueProvider(UnderlyingValue)` (provider results become the stored value); (3) `valueToRead` = stored value with bits of non-readable fields cleared; (4) `ReadToClear` fields clear their bits in the stored value **after** the value to return was captured, `ReadToSet` sets them; (5) field read callbacks, then change callbacks for fields modified by (4), then register-level read/change handlers; returns `valueToRead`. [S]
- **Write** (`WriteInner`, `:588-668`): `difference = stored ^ value`; per field, by write mode:
  - `Write`: update the field bits if any differ; `Set`: OR in 1s; `Toggle`: XOR with 1s; `WriteOneToClear`: clear bits where the value has 1 (and the stored bit was 1); `WriteZeroToClear`: clear bits where the value has 0; `WriteZeroToSet`, `WriteZeroToToggle`, `WriteToClear` (any write clears the field), `WriteToSet`; fields without a write bit (`Read`) are untouched.
  - Then **every field's `writeCallback(oldFieldValue, writtenFieldValue)` runs, in definition order, whether or not the stored bits changed**, then `changeCallback` for changed fields, then register-level write/change handlers.
  - `unhandled = difference & ~definedFieldsMask`: bits not covered by any *field*. Tags (`WithTag`, `WithTaggedFlag`, and **`WithReservedBits`, which is also just a tag named RESERVED**, `PeripheralRegisterExtensions.cs:445`) are not fields, so writing 1s into them is "unhandled": Warning `Unhandled write to offset 0x<o>. Unhandled bits: [<ranges>] when writing value 0x<v>. Tags: <name (0xvalue), ...>.`; tags created with `silent: true` log the same text at Noisy level only (e.g. STM32_Timer `SR` reserved bits, seen at Noisy in `data/tim6-boot-log/renode.log`); a reserved tag with `allowedValue` logs an Error on a different value; `WithIgnoredBits` defines a stored value field named "ignored" to avoid the warning.
- **Wider/narrower collections**: byte/word registers inside a dword peripheral are not a thing here; the bus handles width (7.3). `MultibyteRegister` exists for byte-oriented models (I2C bank, EEPROM) and is not used by the NGC STM32 models.
- **Reset**: `registers.Reset()` restores reset values for soft-resettable registers; `ResetRegister(offset)` on an unknown offset logs a Warning.

**Rust mapping.** Implement a small register helper with the read/write order above; log-once "Unhandled read/write" messages; keep the "write callback runs for every field even if unchanged" rule (several models rely on it, e.g. `UG`, `SR` clear callbacks).

---

## 12. Observed behaviour in the reference boots

All from `reference/data/{handset-trace,dual-wake-run1,dual-wake-run2}/.../renode.log` (grep of `[WARNING]`/`[ERROR]`), identical in both dual runs.

**Unmapped system bus accesses (value 0 returned, no fault):**

| Board | PC | Access | Address | Probable meaning (not verified) |
| --- | --- | --- | --- | --- |
| handset | 0x0805F188 | ReadDoubleWord | 0xE0002000 | FPB_CTRL probe |
| handset | 0x0805F18E | WriteDoubleWord (0) | 0xE0002000 | FPB_CTRL disable |
| handset | 0x0805F1B6 | ReadDoubleWord | 0xE0042000 | DBGMCU_IDCODE |
| handset | 0x0805F1BE/CA/D8 | ReadDoubleWord x3 | 0x5C001000 | unknown peripheral |
| handset | 0x080080D8 | ReadByte | 0x00000008 | null-pointer-like read |
| main | 0x0801C9A8, 0x080041E6 | ReadByte | 0x00000008 | null-pointer-like read |

**Unhandled peripheral register accesses (Warning, read 0 / write ignored):** gpioA/gpioC offset 0x2C (read and write, both boards), `dma1` offset 0xA8 (read x6, write x6; DMA_CSELR), `uart5` offset 0x10 (read, write 0xA), plus `Unhandled write ... Tags:` for unimplemented bit fields: timer2/3/4/15 `CR1.CKD`, `CCMR` preload enables (`OCxPE`), timer15 `BDTR.BKP/MOE`, gpioB `OTYPER` bits, i2c1/i2c2 `TIMINGR` and `CR2.NACK`, usart2 `CR1.IDLEIE`/`CR3.EIE`, uart5 `CR1.MO`/`CR3.IREN`.

**Other warnings:** `Translation cache size 536870912 is larger than maximum allowed 134217728` (Renode config, harmless), `cpu: Patching PC ... for Thumb mode`, `rtc: Shadow registers are not supported` (both boards), `nvic: Trying to set the priority for interrupt 16 to 0xFF ... 0xF0` (both boards). No width-translation warnings, no CPU faults, no `Spurious NVIC IRQ`.

**TIM6 reconfiguration at boot (handset)** from `sysbus LogPeripheralAccess timer6 true` (`data/tim6-boot-log/tim6-access.txt`): `CR1 = 0`, `ARR = 0x3E7`, `PSC = 3`, `EGR = 1`, `DIER = 1`, `CR1 = 1`; later `ARR = 0x3E7`, `PSC = 0x4F`, `EGR = 1` -> "IRQ pending". TIM6 ISR entries then occur every 99 900 instructions (999 us), first ISR at trace index 93 355.

**Reference runs produced** (under `reference/data/` while the harness existed, wall times measured on an Apple-silicon host sharing CPU with other agents; the data is not available, the harness is not part of this repository).

---

## 13. DESIGN.md sections 5-7: deltas and recommended decisions

| Topic | DESIGN.md | Renode (this document) | Recommended action |
| --- | --- | --- | --- |
| Time unit | Exact integer ticks at 3.84e10/s | 1 ns tick; periodic events at ceil-ns, restart from the rounded time, sub-tick overshoot dropped (3.4) | Keep 3.84e10. Offer an optional "Renode ns rounding" mode for `ClockEntry`-style timers if differential traces must match beyond ~1 us of accumulated phase; otherwise document ±1 ns/event drift. |
| Event delivery | Round up to whole instructions | `floor(d/10)` instructions then 1 more if < 10 ns remain = event before the first instruction starting at or after T | Matches. |
| MMIO time | exact `slice_start + icount*384` for all MMIO | Only `SyncTime()` callers (CNT read, DWT CYCCNT, SysTick CVR, input capture, ScheduleAction) see exact time; all other peripheral state changes use clock-source time = last progress report (chunk start); chunk ends: nearest timer limit, quantum end, end of the TB after a timer-class register write | Keep exact time (simpler, more physical) and **expect** timer phase differences vs Renode up to the chunk length after reconfigurations (measured: 104 instructions in the handset boot, up to 8 915 in the micro-benchmark); or implement the lag. Decide before comparing tick phases. |
| Handset release | released at the 50 ms poll that sees GPIOE ODR bit 3 | handset starts one quantum (100 us) after the poll (`DeferredEnabled` applied at unlatch, 5.6); measured `ExecutedInstructions` 44 990 000 at 1.5 s | Delay the first instruction of a released CPU by one quantum, or accept 10 000 more handset instructions at every checkpoint. |
| MMIO-raised IRQ | next instruction boundary | end of the current translation block (TB ends at any branch/WFI/WFE/SVC, page end, budget, 0x7FF) | Next-instruction is acceptable. Expect the first PC-trace divergence near index 93 354 (1-instruction earlier ISR entry). Offer a tracing flag "IRQ at branch boundary" if exact prefix comparison is wanted. |
| Cross-board delivery | at quantum boundary, transmit order | sync phase at the end of the quantum, ordered by (sender CPU last-reported time, task id) | Matches. CAN trace stamps differ (Renode: chunk-lagged sender time). |
| Dual determinism | fixed order, deterministic | Renode is nondeterministic (min-progress clock sources) | Fine; envelope measured (section 6). |
| SysTick | "copy reload/period semantics exactly" | period = RELOAD ticks; COUNTFLAG read-clears on any CSR read; CVR write sets `Value = Reload`; ceil-ns period (9.5) | Port as written. |
| Timer wrap | at ARR | confirmed; UG keeps residuum | Port as written (3.5). |
| Unmapped | read 0, ignore write, log once per address | Warning every access (log collapsing); ArrayMemory out-of-range logs Error | Matches functionally. |
| Sub-word | "document" | 7.3-7.5: per-peripheral rules, RMW reads, unaligned MMIO split | Implement exactly as table 7.4. |
| Unaligned MMIO | s6 "unaligned LDR/STR/LDRH/STRH permitted ... call the bus with the original width and address" | tlib splits before the bus: unaligned MMIO **store = byte writes from the highest address down**, **load = two aligned same-width reads merged** (7.5, measured) | **Contradicts.** Implement the split in the board's MMIO path (`CpuBus` impl); plain memory stays a single access. |
| GPIO | deliver on change, connect pushes level | confirmed (8) | Matches. |
| IRQ semantics | level vs pulse follow NVIC.cs | pending on rising edge, `Running`, ICPR blocked while high, re-pend on completion (9.2) | Port as written. |
| Instruction count | 1 per instruction incl. IT-skipped; entry/return free | confirmed by the measured exact 100 000 per ms; IT counting not separately verified | Verify on first IT-heavy trace segment. |
| DWT | CYCCNT from virtual time | LimitTimer, enabled by CTRL bit 0, SyncTime on read | Matches. |

---

## 14. Not verified here / open items

- SysTick spacing and the TIM6 phase artifacts were derived from source and the 200 ms handset trace; the dual reference has no instruction trace (only checkpoints).
- The exact TB partition of the NGC firmware (which instructions end a Renode TB) was not extracted; it only matters if the engine chooses to emulate TB-end interrupt latency or the clock-source lag.
- IT-skipped instruction counting, exclusive monitors, FPU lazy-stacking behaviour in tlib were not re-verified; see `tlib/arch/arm/helper.c` and the CPU work package.
- STM32 peripheral models (USART, CAN, I2C, DMA, RTC, IWDG, RNG, CRC) were not re-documented here: port them from the upstream sources (the file list with hashes was `reference/README.md` of the harness, which is not part of this repository).
- Physical-device accuracy is not claimed anywhere.


---

## 15. Micro-benchmark test vectors (Renode 1.17.0 reference for the Rust engine)

`reference/micro/` (not part of this repository) held ten tiny Thumb programs (generated by `thumb_asm.py` + `micro_programs.py`, no external assembler), the minimal platform `micro.repl` (Cortex-M4, NVIC `priorityMask 0xf0` with 80 MHz SysTick, 1 MiB flash at 0x08000000, 96 KiB SRAM at 0x20000000, STM32_Timer TIM6 at 0x40001000 / 80 MHz / limit 0xFFFF -> NVIC 54, DWT 80 MHz), the runner `micro_run.py` (PC execution trace + analysis, owned Renode, port >= 18900) and `vectors.json` (images as hex, labels, every expected ISR entry index, trace SHA-256). The vectors are kept as `testdata/renode-micro-vectors.json`. Boot state: VTOR = 0x08000000, SP = 0x20018000, PC = label `reset`, 100 MIPS, 100 us quantum, 1 ms `RunFor` steps. All numbers in this section were produced twice (identical).

| Program | What it pins down | Renode result (instruction indices in the executed-instruction trace) | Exact-time/next-instruction DESIGN model gives |
| --- | --- | --- | --- |
| `systick-reload79999` | SysTick RELOAD semantics and ns rounding | first ISR at 99 999, then spacings 99 999 x31 / 99 998 x8 (every 5th), 40 entries in 40 ms | rational 999 987.5 ns: spacing pattern period 4, diverges after 8 entries |
| `tim6-ceil-arr1001-psc0` / `-psc2` | free-running STM32 timer period ARR, ceil-ns | 479 / 426 entries, all equal to `ceil((t0+k*P)/10)`, P = 12 513 / 37 538 ns | diverges after 6 / 8 entries |
| `timer-enable-lag` | clock-source lag at timer enable | first ISR at `chunk_start + 99 900` for D = 0..7000 (table in 5.4) | `cen_index + 99 900` (up to 8 915 instructions late vs Renode) |
| `pendsv-tb-end` | IRQ pended by MMIO is taken at TB end | store+(k+2) for k = 0,1,2,3,6,12,25; after `cpsie i` +1 | store+1 |
| `level-irq-reentry` | level-held line, tail-chaining | entries at 13, 20, 27, 34, 41 (7 apart, no thread instruction between) | same (ARM-compliant) |
| `wfi-systick` | WFI accounting | 148 instructions, 20 ISR entries in 20 ms, 7 instructions per tick | same if `wfi` counts as 1 instruction |
| `wfi-primask` | WFI wake with masked pending IRQ | 53 instructions in 3 ms (2 per 100 us slice after the first tick), no ISR | busy loop (ARM) |
| `wfi-systick-cyccnt` | exact time at wake | CYCCNT deltas 79 999 (11 of 11) | same |
| `unaligned-mmio` | unaligned 32-bit MMIO store/load split | store -> 4 byte writes 0x2D..0x2A (high to low), load -> aligned dword reads at 0x28 and 0x2C, merged | board-defined (DESIGN s6 says "call the bus with the original width and address") |

Use: build the `micro` board in the Rust engine, load `imageHex`, run, and compare per program; each divergence in the last column is a *known* difference between DESIGN s5 as written and Renode, so the CPU/BOARD packages can decide per item whether to emulate Renode (timer ceil-ns, TB-end IRQ latency, clock-source lag) or accept the difference. The full PC trace of each run (`data/micro/<program>/trace.u32le`) is not kept.

Regenerate (needs a Renode 1.17.0 installation and the harness of the analysis workspace, which is not public): `python3 reference/micro/micro_run.py --name micro` then `python3 reference/micro/export_vectors.py`.
