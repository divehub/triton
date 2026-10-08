# Board and peripheral framework

Guide for authors of peripheral models (`crates/stm32`, `crates/ngc/src/models`) and for code that
assembles boards. The framework lives in `crates/emu-core` (CPU independent, usable from unit tests
without a CPU) and `crates/ngc/src/{board,bus,memory}.rs` (the per-board run loop and the CPU bus).
Design context: `DESIGN.md` sections 5 and 7 (timing is normative there), Renode semantics in
`docs/renode-semantics.md` (sections 2-6 and 15 for time). Renode was the behavioural reference
(fidelity to Renode is no longer a gate for new features, `DESIGN.md` 15.1, but existing behaviour must
not change); where this framework differs from it, the difference is listed in section 15.

> **Note (2026-10-08).** `renode-src/...` citations in this document and in
> `docs/renode-semantics.md` refer to the upstream Renode 1.17.0 sources (renode-infrastructure commit
> `066a7f13c052215632d469c995c89aea37c573b1`, readable on GitHub). The pinned copies and the `reference/`
> harness with its data are not part of this repository; the recorded regression data that remains is
> described in `testdata/README.md`. Paths such as `emulation/models/*.cs` name files of the separate Renode-based analysis workspace (not public).

**Status of this document: it is the settled API contract for the Renode-faithful timing model
(decision of 2026-10-08).** Section 18 lists what the CPU work package has to provide to match.

Contents: 1 model overview, 2 time, 3 writing a peripheral, 4 sub-word access, 5 clock entries and
timers, 6 sync registers and `request_return`, 7 ordinary events, 8 signals, 9 bus access from
models, 10 logging, 11 debugger access, 12 assembling a board, 13 the run loop, 14 testing,
15 differences from Renode, 16 porting checklist, 17 complete example, 18 contract with the CPU.

## 1. Model overview

```
Board<C: CpuCore>                      one per MCU (main, handset)
 |- cpu: armv7m::Cpu                   Cortex-M4F; handles 0xE000_0000..=0xE00F_FFFF itself (NVIC, SCB, SysTick, DWT)
 |- core: MachineCore                  everything outside the CPU
 |   |- mem: PlainMemory               flash 1 MiB @0x08000000, SRAM1 96 KiB @0x20000000, SRAM2 32 KiB @0x10000000
 |   |- peripherals                    Vec<Option<Box<dyn Peripheral>>>  (taken out of their slot while they run)
 |   |- MMIO map                       address -> (peripheral, region base, access policy, sync registers)
 |   |- nets                           (peripheral, output line) -> [Target::Irq(n) | Target::Input(peripheral, line)]
 |   |- clock: ClockRegistry           Renode ClockEntry/BaseClockSource port, entries in creation order
 |   |- events: EventQueue             one queue: clock-entry limit events (ordered by creation index) and ordinary events
 |   |- clock_time                     the machine's clock-source time (lags the CPU inside a chunk)
 |   |- IRQ-change queue               (irq, level) in the order they happened, drained into the CPU
 |   `- log                            bounded ring + warn-once
 `- run_until(t)                       Renode's round/chunk loop: CPU chunks, clock advance, events, IRQ delivery
```

A peripheral is a Rust struct implementing `emu_core::Peripheral`. It never touches the CPU: it
reads and writes registers when the bus asks, owns **clock entries** (timers, managed threads,
scheduled actions) and ordinary events for its future work, drives output lines (interrupts, GPIO)
and, for DMA-like models, reads and writes the bus through `Ctx`.

Everything is single threaded and deterministic. There are no locks, no wall-clock reads and no
hash-ordered iteration in model code; a run is a pure function of the firmware, the configuration
and the time-stamped inputs.

## 2. Time

### 2.1 Units

`emu_core::Time` is a `u64` count of **nanoseconds** (Renode `TimeInterval` ticks).

```rust
pub const TICKS_PER_SECOND: Time = 1_000_000_000;
pub const TICKS_PER_MILLISECOND: Time = 1_000_000;
pub const TICKS_PER_MICROSECOND: Time = 1_000;
pub const TICKS_PER_INSTRUCTION: Time = 10;   // 100 MIPS, Renode default PerformanceInMips
pub const QUANTUM: Time = 100_000;            // 100 us, Renode default global quantum
```

`from_micros`, `from_millis`, `from_secs_f64`, `to_secs_f64` convert. `ticks_per_cycle(hz)` is
`Some(1e9 / hz)` only when that is an exact integer (`None` for 80 MHz, 32 768 Hz, 120 Hz...): **do
not use it for timing**. Clocks that do not divide 1 ns are modelled with clock entries (section 5),
which keep the exact fractional residue like Renode does. Do all arithmetic in integers.

### 2.2 Two times: clock time and CPU time

Renode does not advance a machine's clock source instruction by instruction. The CPU runs a *chunk*
of instructions and reports its progress afterwards; only then does the clock source advance and
timer events fire. Inside a chunk, peripherals see the **clock-source time**, which is the time at
the start of the chunk (or at the last explicit sync), not the time of the instruction that is
accessing them. This lag is observable by firmware and the framework reproduces it:

* `MachineCore::clock_time()` is the clock-source time. `Board::now()` is the CPU time. They are
  equal between chunks and differ inside one.
* **`ctx.now()` is the clock time**, in every context:

  | Context | `ctx.now()` |
  | --- | --- |
  | CPU register access (`read`/`write`) | clock time: chunk start, or the exact instruction time after a sync (section 6) |
  | `on_event` of a clock entry or an ordinary event | the event's own time (the ceil-ns limit time of the entry) |
  | `on_input`, `reset`, `attach`, `with_peripheral`, `set_input`, host bus access | clock time of the cause (between chunks: the exact board time) |
  | event that had to be deferred because its owner was running | the clock time when it is delivered (`scheduled` still holds the event time) |

* The machine advances clock time **only** at the end of a chunk (`MachineCore::advance_clock`)
  and at explicit syncs (declared sync registers, `ctx.sync_time()`, `schedule_action`). When it
  advances it fires every event on the way in time order, setting `ctx.now()` to each event's
  own time before its handler runs.
* **Exact instruction time is available only through a sync.** A register that Renode reads after
  `cpu.SyncTime()` (timer `CNT`) is declared with `sync_registers()`; code that Renode syncs
  conditionally calls `ctx.sync_time()`. Everything else deliberately sees the lagged time, e.g. a
  status flag set by a timer event inside the current chunk is not visible to a polling loop until
  the chunk ends. Do not "fix" this lag; it is part of the reference behaviour.

### 2.3 Prefer time-derived state

Derive state from time instead of simulating ticks: a clock entry computes its value at any clock
time (`LimitTimer::value(ctx)`), and only observable edges (limit reached, compare match, capture,
DMA request) produce events. Per-tick events would make real-time emulation of two 100 MIPS boards
impossible.

## 3. Writing a peripheral

```rust
pub trait Peripheral: 'static {
    fn name(&self) -> &str;
    fn attach(&mut self, ctx: &mut Ctx<'_>) {}           // once, from add_peripheral: create clock entries here
    fn reset(&mut self, ctx: &mut Ctx<'_>) {}
    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32;
    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>);
    fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {}
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {}
    fn access_policy(&self) -> AccessPolicy { AccessPolicy::EXACT }
    fn sync_registers(&self) -> Vec<SyncRegister> { Vec::new() }
    fn peek(&self, offset: u32, width: Width, view: &View<'_>) -> Option<u32> { None }
    fn poke(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) -> bool { false }
    fn summary(&self, view: &View<'_>) -> String { String::new() }
    fn as_any(&self) -> &dyn Any;          // use impl_peripheral_any!()
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
```

* **Registers.** `offset` is relative to the base the peripheral was mapped at and is passed
  unmodified, including odd alignments. `width` is `Width::Byte | Half | Word`. `read` returns the
  value in the low `width` bits (the machine masks the rest). `write` receives `value` in the low
  `width` bits. Keep register storage as `u32` fields, not byte arrays, unless the hardware is
  byte oriented (the CRC and the EEPROM are).
* **`attach`.** Called exactly once from `MachineCore::add_peripheral` (so before the peripheral is
  mapped or connected), in registration order. Create clock entries here (`LimitTimer::attach`,
  `ManagedThread::attach`, `ctx.clock_add`). The **creation order of clock entries is the order of
  `attach` calls**, and it decides in which order handlers run when several entries expire at the
  same nanosecond (section 5.5): add peripherals to a board in the order Renode constructs them
  (`.repl` order) and create a peripheral's own entries in the order its C# class does (an
  `STM32_Timer` is its own `LimitTimer` first, then channels 1..4). Do not touch other peripherals
  from `attach`.
* **Unimplemented registers.** Follow the Renode class: return 0 and ignore writes, and log once
  with `ctx.warn_once(key, format_args!(...))` so a firmware polling it does not flood the log.
* **Read side effects** (clear-on-read flags, FIFO pops) belong in `read` only. Provide `peek` for
  the same registers *without* the side effect. `peek` gets a read-only [`View`] of the machine:
  `view.now()` is the clock time and `view` implements `ClockRead`, so time-dependent registers
  (`timer.value(view)`) are computed for it. Snapshots, the HUD telemetry and the fixtures use
  `Board::peek`/`Ctx::mem_peek`, never `read`.
* **`poke`** is the write counterpart for fixtures and snapshot restore: no side effects beyond
  setting the state; it receives a `Ctx` so clock entries can be patched.
* **Reset.** `reset` is called by `MachineCore::reset_all` and by models themselves (a GPIO reset
  line). Reset your clock entries (`LimitTimer::reset(ctx)` replaces the entry in place, keeping
  its creation order, exactly like Renode's `InternalReset`), cancel ordinary events and restore
  outputs. Boards are normally recreated instead of reset (Restart/Cold/Wake), so reset paths
  matter mostly for in-model resets such as the LCD's `PB4` input.
* **State must be restorable from outside.** Persisted state (EEPROM, NOR, RTC checkpoint) goes
  through explicit typed methods that the system calls via `with_peripheral`, not through the bus.
* **`impl_peripheral_any!()`** inside the `impl Peripheral` block provides `as_any/as_any_mut`
  so `board.get::<T>(id)` and `with_peripheral::<T, _>` can reach the concrete type.
* **Re-entrancy.** While a method of a peripheral runs, the peripheral is out of its slot. Bus
  accesses to it from its own code (or from a peer that it called) are logged errors that read 0.
  Events that become due for a running peripheral are *deferred* and delivered when it returns
  (section 5.6).

## 4. Sub-word access and `AccessPolicy`

The Cortex-M issues byte, halfword and word accesses. Renode does not hand them all to a
peripheral: a class implements some of `IBytePeripheral`, `IWordPeripheral` (16 bit) and
`IDoubleWordPeripheral` (32 bit) and may carry `[AllowedTranslations(...)]`. The bus then

1. passes an access of an implemented width straight to the peripheral, offset unmodified;
2. otherwise uses the first *allowed* translation (priority below);
3. otherwise logs "Attempted <width> read/write isn't supported by the peripheral", reads 0 and
   drops the write. The peripheral is not called.

This is reproduced by the machine from `Peripheral::access_policy()`, which is queried once when
the peripheral is mapped:

```rust
AccessPolicy::EXACT                       // every width reaches read/write (default; ArrayMemory, CRC)
AccessPolicy::WORD_ONLY                   // IDoubleWordPeripheral only, nothing translated
AccessPolicy::WORD_ONLY.with_translations(Translations::HALF_TO_WORD)   // STM32_GPIOPort
AccessPolicy::new(Widths::BYTE | Widths::WORD, Translations::NONE)      // native widths + allowed translations
```

Translations (ARM names; the Renode `AllowedTranslation` spelling is in the doc comments):
`BYTE_TO_HALF`, `BYTE_TO_WORD`, `HALF_TO_BYTE`, `HALF_TO_WORD`, `WORD_TO_BYTE`, `WORD_TO_HALF`.
Priority per width: byte -> 32-bit then 16-bit; halfword -> 32-bit then byte; word -> halfword then
byte. A translation needs its target width to be native.

Semantics ported from Renode `ReadWriteExtensions`:

* byte/halfword **read** via a wider register: read the aligned wider value, shift by
  `(offset & mask) * 8`, truncate;
* byte/halfword **write** via a wider register: **read-modify-write** (the read has the
  register's normal side effects), writing the aligned value back;
* a halfword at offset 3 of a 32-bit register reads only the top byte and writes only the
  low byte of the value (no second word is touched) - Renode's shift arithmetic, kept on purpose;
* word via halfwords/bytes and halfword via bytes: consecutive narrower accesses, lowest address first.

`access.rs` documents the details and `translate_read/translate_write` can be reused by models that
need the same arithmetic internally. `docs/renode-semantics.md` 7.3-7.4 lists the policy of every
peripheral in the two `.repl` files (section 16 repeats it as a porting table).

**Unaligned CPU accesses.** The stock platform's translation library never sends an unaligned
access outside plain memory to the bus as one access (`docs/renode-semantics.md` 7.5). The CPU bus
reproduces that (`MachineCore::cpu_read/cpu_write`): an unaligned load becomes two aligned loads of the
same width merged by shift (both have their side effects), an unaligned store becomes single-byte
stores from the highest address down - and those byte stores are then subject to the access policy
above (a word-only peripheral drops them with the "not supported" warning, as in Renode). Unaligned
accesses to flash/SRAM are plain. Accesses issued through `ctx.mem_*` and `Board::bus_*` (DMA,
monitor) are *not* split: like `sysbus.ReadDoubleWord` they reach the peripheral with the offset as given.

## 5. Clock entries and timers

Every Renode timer is a `ClockEntry` registered with the machine's `BaseClockSource`. `emu_core::clock`
ports both faithfully (`docs/renode-semantics.md` sections 3-4). Use the Renode-shaped wrappers
(`LimitTimer`, `ManagedThread`, `schedule_action`); drop to `ClockEntry` only for classes that call
`AddClockEntry` themselves.

### 5.1 `ClockEntry` semantics

An entry counts **entry ticks** at `frequency` Hz, from `Value` towards `Period` (the limit):

| field | meaning |
| --- | --- |
| `value: u64` | current count; ascending starts at 0, descending at `period` |
| residuum | exact fractional entry ticks not yet accounted (kept as an exact fraction, never rounded) |
| `period: u64` | the limit |
| `frequency: u64` | entry ticks per second (already divided by any prescaler) |
| `Ratio` | `frequency / 1e9` reduced: entry ticks per nanosecond |
| `Direction` | `Ascending` reaches the limit when `value >= period`; `Descending` when `floor(elapsed) >= value` |
| `WorkMode` | `Periodic` re-arms after each limit, `OneShot` disables itself |
| `enabled` | disabled entries do not count and own no event |

Rules, all ported verbatim:

* Elapsed time `ns` adds `ns * Ratio + residuum` entry ticks; the integer part moves `Value`, the
  fraction becomes the new residuum.
* **The limit is reported at the ceil-ns time**: an enabled entry owns one pending event at
  `now + ceil(time to limit)`. When it fires, the overshoot (the fraction of a nanosecond past the
  true limit) is **discarded**: `Value` restarts at 0 (ascending) / `period` (descending) with a zero
  residuum from that rounded nanosecond. Periodic entries therefore fire every `ceil(period * 1e9 / f)`
  ns, not every `period * 1e9 / f`, with no memory of the dropped fraction.
* `Value` writes keep the residuum; **frequency (and divider) changes clear it**; period, direction,
  mode and enable changes keep it. Setting `Value` below/above the limit takes effect immediately:
  if the new state is already at its limit the limit is reached "now" (the handler is delivered
  right after the access, section 5.6).
* The value can be read at **any clock time** without events (`entry` snapshots are advanced to the
  current clock time); a disabled entry keeps its value.
* Reconfiguration (`clock_exchange`) first accounts the elapsed time, applies the change, reschedules
  the entry's event, and performs the zero-time limit check described above - Renode's
  `ExchangeClockEntryWith`.

Periods you can check against (`testdata/renode-micro-vectors.json`, renode-semantics 3.4):

| entry | first-limit time |
| --- | --- |
| STM32 timer, `ARR = 999`, 1 MHz, ascending | 999 000 ns |
| managed thread at 10 kHz (buttons) | 100 000 ns |
| managed thread at 1 kHz / 120 Hz | 1 000 000 ns / 8 333 334 ns |
| SysTick `RELOAD = 79 999` at 80 MHz, descending | 999 988 ns |
| TIM6 `ARR = 1001`, `PSC = 0`, 80 MHz, ascending | 12 513 ns |
| TIM6 `ARR = 1001`, `PSC = 2` (26 666 666 Hz after integer division), ascending | 37 538 ns |

### 5.2 `LimitTimer`

Port of Renode `LimitTimer` (the base of every STM32 timer, RTC prescalers, the IWDG...). It owns one
clock entry and the `rawInterrupt`/`eventEnabled` flags; **it does not own the owner's callback**:
the owner forwards the entry's event token and decides what the Renode `LimitReached` handler does.

```rust
let cfg = LimitTimerConfig {            // Renode defaults: limit u64::MAX, Descending, disabled, Periodic,
    limit: 0xFFFF,                      // event disabled, auto_update false, divider 1
    direction: Direction::Ascending,
    event_enabled: true,
    ..LimitTimerConfig::new(80_000_000) // frequency in Hz (required)
};
let mut timer = LimitTimer::new(cfg, TIMER_TOKEN);      // in the peripheral's constructor
fn attach(&mut self, ctx: &mut Ctx<'_>) { self.timer.attach(ctx); }          // creates the entry
fn reset(&mut self, ctx: &mut Ctx<'_>) { self.timer.reset(ctx); }           // InternalReset: entry replaced in place

// reads: any ClockRead (Ctx or View), no side effects, exact at the current clock time
timer.value(ctx); timer.limit(ctx); timer.enabled(ctx); timer.direction(ctx); timer.mode(ctx);
timer.value_and_limit(ctx);   timer.frequency(); timer.divider();
timer.raw_interrupt(); timer.interrupt() /* raw && event_enabled */; timer.event_enabled();

// writes (each reconfigures the entry; all except set_mode and the flag setters call ctx.request_return())
timer.set_enabled(ctx, true);  timer.set_value(ctx, v);    timer.set_limit(ctx, l);
timer.set_frequency(ctx, hz);  timer.set_divider(ctx, d);  timer.set_direction(ctx, dir);
timer.set_mode(ctx, WorkMode::OneShot);  timer.reset_value(ctx);
timer.set_event_enabled(true); timer.set_auto_update(true); timer.clear_interrupt();
timer.increment(ctx, by) -> overflows;  timer.decrement(ctx, by) -> overflows;

// in on_event:
if token == TIMER_TOKEN {
    if timer.on_limit_reached() {       // sets raw_interrupt; true when event_enabled (Renode raises LimitReached)
        /* what your C# class does in the LimitReached handler: set UIF, UpdateInterrupts(), ... */
    }
}
```

Renode quirks that are kept (`// Renode parity` in the source):

* The entry runs at `frequency / divider` using **u64 integer division** (80 MHz / 3 = 26 666 666 Hz).
* `set_limit` with `auto_update` also resets `Value` (0 ascending, the new limit descending); without
  it `Value` is kept even if it now exceeds the limit (the entry then reaches its limit immediately).
* `set_value` rejects a value above the *initial* limit (Renode throws `ArgumentException`; here an
  error is logged once and the write is ignored).
* The limit must be at least 1, the frequency and divider at least 1 (construction panics otherwise,
  `set_frequency`/`set_divider` log an error and ignore 0).
* `set_mode` and the flag setters do not request a return; every other setter does.
* A one-shot timer disables itself at its limit; its value is `0` (ascending) / `period` (descending).

### 5.3 `ManagedThread` (`machine.ObtainManagedThread`)

A periodic ascending entry with period 1 at `frequency` (or `period` ns at 1e9 Hz), **created disabled**.

```rust
let mut tick = ManagedThread::new(10_000, TICK);                 // 10 kHz: fires every 100 000 ns
let mut once = ManagedThread::with_period(250 * US, SENSOR);     // Renode ObtainManagedThread(action, TimeInterval)
tick.attach(ctx);
tick.start(ctx);        // enabled; keeps Value and residuum: first firing ceil(1e9/f) ns later (no phase reset)
tick.stop(ctx);         // disabled; keeps Value and residuum, so a later start resumes the partial period
tick.restart(ctx);      // enabled and Value = 0 (ascending); residuum kept
tick.set_frequency(ctx, hz);   // clears the residuum
tick.set_period(ctx, ns);      // period ns at 1e9 Hz
tick.start_delayed(ctx, delay, START_TOKEN);  // schedule_action: on START_TOKEN call tick.start(ctx) and run the body once
tick.frequency(ctx); tick.period(ctx); tick.enabled(ctx); tick.dispose(ctx);
// in on_event: if token == TICK { /* the thread body */ }     (a stop condition is just an `if` before the body)
```

`start`, `stop`, `restart` do **not** request a return (as in Renode): a thread started in the middle
of a chunk whose first firing falls inside that chunk is handled at the chunk's end.

### 5.4 `schedule_action`

```rust
let id = ctx.schedule_action(delay, TOKEN);   // machine.ScheduleAction(delay, action)
// later: on_event(TOKEN, scheduled, ctx)
```

* It first calls `ctx.sync_time()` (the delay is measured from the exact instruction time when called
  from a CPU access, and the action's origin is that time), creates a one-shot entry of `delay` ns at
  1e9 Hz, removes it after the action ran, and calls `ctx.request_return()`.
* `on_event`'s `scheduled` is the **scheduling time** (what Renode passes to the callback);
  `ctx.now()` is the firing time `scheduled + delay`.
* `delay == 0` fires immediately after the current access (section 5.6).

### 5.5 Ordering rules

* Entries that expire at the same nanosecond are all **updated first**, then their handlers run in
  **creation order** (`attach` order, see section 3). A handler that reads another entry that expires
  in the same instant therefore sees the post-limit state of that entry.
* A handler that reconfigures an entry that already ran in the same instant, such that it would reach its
  limit again at zero time, does not run again (Renode's `alreadyRunHandlers`); the state change
  still happens.
* Clock events sort before ordinary events at the same time; ordinary events keep scheduling order.
* Removing an entry from a handler does not cancel handlers of the same instant that were already
  collected (Renode parity).

### 5.6 Zero-time limits, deferral and re-entrancy

Renode runs a handler *inside* the setter that causes it (a `Value` write to the limit calls
`OnLimitReached` re-entrantly). The framework cannot re-enter the running peripheral, so the entry state is
updated immediately (`Value` reset, one-shot disabled, flags as for any limit) and **the handler is delivered
as soon as the peripheral's method returns** (before the CPU executes another instruction, before
the next event). The same rule covers events that become due for a peripheral that is running (for
example when `ctx.sync_time()` inside `read` fires the owner's own limit event) and
`schedule_at(<= now)`. The only observable difference to Renode is the order *within* one register
write: statements after the setter run before the handler.

### 5.7 Using entries directly

```rust
let id: ClockId = ctx.clock_add(ClockEntry::new(period, frequency_hz, enabled, Direction::Ascending, WorkMode::Periodic), TOKEN);
let e: ClockEntry = ctx.clock_entry(id);                  // snapshot at the current clock time
ctx.clock_exchange(id, |e| e.with_period(100).with_value(0));   // Renode ExchangeClockEntryWith
ctx.clock_replace(id, new_entry);                         // replace in place (keeps creation order)
ctx.clock_remove(id);
```

`ClockEntry` is a plain value type (`Copy`) with Renode's `With(...)` family (`with_period`,
`with_frequency` (clears the residuum), `with_enabled`, `with_value`, `with_direction`, `with_mode`) and the pure
functions `advance(ns) -> Advance { reached, to_limit }` (the hot callers use `advance_reached(ns) -> bool`, which
skips the time to the next limit) and `ns_to_limit()`, so it is unit-testable without a machine. The arithmetic is
exact `u64` whenever no intermediate value overflows (always, for the clocks in use) and falls back to a 128-bit
reference otherwise; `tests::u64_paths_equal_the_u128_reference` proves the two equal over millions of random and edge
cases. On `wasm32` a checked 64-bit multiplication is a library call, so operands below 2^32 skip it.

### 5.8 `LocalClock`: entries owned by the CPU

SysTick and the DWT cycle counter live inside the CPU crate, not in the registry. They use the same
arithmetic through `emu_core::clock::LocalClock`, a `ClockEntry` plus the time of its last update:

```rust
let mut c = LocalClock::new(ClockEntry::new(reload, 80_000_000, true, Direction::Descending, WorkMode::Periodic), now);
c.value_at(now);                     // value at any time without events
c.next_limit();                      // Option<Time>: absolute ceil-ns time of the next limit (feeds next_internal_deadline)
c.run_until(now, |t| { /* limit reached at t (ceil-ns): set COUNTFLAG, pend SysTick */ });   // Advance(): splits at limits
c.exchange(now, |e| e.with_value(v));   // ExchangeClockEntryWith; returns true if the new state is already at its limit
```

`run_until` processes every limit at its own time, so overshoot is discarded and the next period starts
at the rounded nanosecond exactly as in the registry. Call it before `exchange`/reads that must see the
processed state.

## 6. Sync registers and `request_return`

### 6.1 Declaring a sync

Renode's `STM32_Timer` calls `cpu.SyncTime()` in the `CNT` read callback; the machine reproduces this
declaratively:

```rust
fn sync_registers(&self) -> Vec<SyncRegister> {
    vec![SyncRegister::read(CNT)]         // also ::write(off), ::read_write(off); 4-byte registers
}
```

The list is returned as a `Vec` (a `&[SyncRegister::read(CNT)]` literal cannot be promoted to `'static`
because the constructors are function calls) and is read when the peripheral is mapped, like `access_policy`.
For an access of any width that overlaps a declared register, a **CPU access** first advances clock time to the
synced time `chunk_start + (icount - chunk_start_icount) * ticks_per_instruction`, where the core passes the
instruction count at the **start of the current translation block** (Renode's `SyncTime` reports
`TlibGetExecutedInstructions()`, which tlib advances only when a block completes; renode-semantics §5.4) (firing every event
on the way, each with its own time), then dispatches the access with `ctx.now()` equal to that time. The check
is made once per CPU access, before any sub-word translation, and also applies to accesses that another
peripheral makes through `ctx.mem_*` while the CPU access is in progress (a DMA transfer started by a register
write): like Renode's `sysbus.TryGetCurrentCPU`, "a CPU access is in progress" is a property of the whole call
chain. Host accesses (`Board::bus_read/bus_write`, `Harness::read/write`) happen outside any CPU access and
never sync.

### 6.2 Conditional syncs

When Renode syncs only in some cases (the `SMS` write in trigger/encoder mode, the input-capture latch):

```rust
let now = ctx.sync_time();   // advance clock time to the exact instruction time of the CPU access in progress
```

`sync_time` returns the (new) clock time. Outside a CPU access (event handlers, host calls) it is a
no-op returning `ctx.now()` - the clock is already at the event time. Events that become due for the
calling peripheral during the sync are deferred until it returns (section 5.6).

### 6.3 `request_return`

```rust
ctx.request_return();   // Renode RequestReturn: end the current CPU chunk at the end of the current translation block
```

Sets `NOTIFY_STOP_REQUESTED`; the core returns from `run` at the end of the current TB and the board
plans the next chunk with the new nearest event. **`LimitTimer` setters (except the mode and flag
setters) and `schedule_action` call it for you.** Raise it yourself when a change must be noticed
before the CPU's planned chunk end and Renode requests a return there. A request raised outside a
chunk is ignored. `ctx.request_cpu_stop()` is a deprecated alias.

## 7. Ordinary events

For behaviour that has no Renode clock entry behind it (frames delivered by the host or the other board,
fixtures). Renode-derived timers use section 5.

```rust
let id: EventId = ctx.schedule_at(time, token);   // absolute
let id = ctx.schedule_in(delay, token);           // relative to ctx.now() = the *clock* time (lags inside a chunk)
ctx.cancel(id);                                    // false if already fired/cancelled/NONE
ctx.replace_event(&mut slot, time, token);         // cancel *slot, schedule, store the new id
fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>)
```

* Events fire in `(time, class, order)` order: clock events first (by entry creation order), then
  ordinary events in scheduling order. The queue is an indexed binary heap with O(log n) schedule,
  pop and cancel and no allocation in steady state.
* `scheduled` is the event time; `ctx.now()` equals it (clock time is set to each event's own time),
  except for deferred deliveries and events scheduled in the past (they fire at the current clock time).
* **They do not request a return.** An event scheduled from a CPU access fires at the end of the
  chunk the access belongs to, unless it is due at or before the current clock time (it then fires
  right after the access). Call `ctx.request_return()` (or use `schedule_action`, which does it and
  syncs) when it must not wait.
* `tokens` are yours. Cancel your events in `reset`.
* Safety valve: more than `MAX_EVENTS_PER_DRAIN` events processed at one instant (a zero-period
  self-rescheduling entry or event; Renode would hang) log an error and **drop every event that is due at that
  instant**, so the host loop always terminates. A clock entry that lost its event stays registered; reconfiguring
  it re-arms it.

## 8. Signals: outputs, inputs and interrupts

A peripheral owns up to 64 numbered **output lines** (`ctx.set_output(line, level)`), the
equivalent of its Renode `GPIO` properties or `INumberedGPIOOutput.Connections`. Lines are wired
by the assembler, mirroring `.repl` `->` lines (one source, many targets, like `|`):

```rust
core.connect_irq(src, line, 29);                 // exti.0 -> nvic@29
core.connect_input(src, line, dst, dst_line);    // lcd.TE -> gpioD@3
```

* `Target::Irq(n)` pushes `(n, level)` to the IRQ-change queue and the CPU applies it before its
  next instruction (it arbitrates only at the next translation-block boundary). Level changes are
  queued in order, so `set_output(l, true); set_output(l, false)` is a pulse the NVIC sees as a pending
  edge. The queue is applied by the CPU itself during a chunk and by the board between chunks.
* `Target::Input(dst, n)` calls `dst.on_input(n, level, ctx)`.
* `set_output` notifies **only on a level change** (Renode `GPIO.Set`). The current level is kept per
  line, also when nothing is connected.
* `connect` pushes the line's *current* level to the new target immediately, even when low
  (Renode `GPIO.Connect`). Connecting the same target twice is ignored. Models that must ignore
  this initialization push can wait for their own armed state.
* Inputs from the outside: `board.set_input(dst, line, level)` (Renode `peripheral OnGPIO n level`) and
  `board.drive_output(src, line, level)` (a pin held by the environment).

### Delivery timing (differs from Renode)

Renode's `GPIO.Set` calls the receiver *inside* the sender's method. Here `set_output` only records
the change; the `on_input` calls (and everything they cause) run **when the sender's current call
returns** - before control goes back to the CPU or to the event loop, depth first, in connection
order. Within a handler, outputs are therefore observed by receivers after the handler finished.
For the usual pattern (drive the line last) this is indistinguishable. It is what makes this possible:

```
ADC event fires -> sets DMA request line -> (ADC returns) -> DMA.on_input reads the ADC's data
register through the bus -> ADC.read runs (the ADC is back in its slot) -> clears EOC
```

With immediate delivery the DMA would hit the ADC while its `&mut self` is still borrowed. If the
rest of your handler depends on the receiver's reaction (Renode ordering: request pulse, then the
DMA read clears EOC, *then* the IRQ level is recomputed), split the handler: do the first part,
`set_output` the pulse, `ctx.schedule_at(ctx.now(), CONTINUE)` and finish in `on_event`. Events due at
the current time run right after the queued deliveries and before the CPU resumes, so the observable
order matches Renode. `NGCMainADC` (DMA request) is the case to handle this way.

A delivery to a peripheral that is itself running further up the call chain (it wrote to a peer that
answered on a line wired back to it) is kept and delivered when that peripheral's call returns.

## 9. Bus access from models (DMA, telemetry)

```rust
ctx.mem_read(addr, Width::Word) -> u32        // system-bus read with MMIO side effects (sysbus.ReadDoubleWord)
ctx.mem_write(addr, Width::Word, value)
ctx.mem_read_bytes(addr, &mut buf)            // sysbus.ReadBytes: memcpy for flash/SRAM, bytewise otherwise
ctx.mem_write_bytes(addr, &data)
ctx.is_plain_memory(addr)                     // WhatIsAt(addr) is MappedMemory
ctx.mem_peek(addr, Width::Word) -> Option<u32>  // no side effects (needs the target's peek)
```

These go through the same MMIO table, access policies and unmapped rules as CPU accesses, at
`ctx.now()` (clock time). They do not sync by themselves, but a declared sync register still advances the clock when
the whole call chain was started by a CPU access (section 6.1). Accessing the running peripheral itself, or one that is
further up the call chain, is an error: it is logged once and reads 0 / drops the write. Writes into
flash take effect (the stock platform maps flash as writable `MappedMemory`) and end the running
CPU chunk so the core can drop its predecoded instructions.

## 10. Logging

```rust
emu_warn!(ctx, "unexpected value 0x{:X}", value);       // emu_error! emu_info! emu_debug! emu_noisy!
ctx.logf(LogLevel::Debug, format_args!("..."));         // nothing is formatted unless enabled
ctx.warn_once(key, format_args!("..."));                // at most once per (peripheral, key)
```

Levels follow Renode (`Noisy < Debug < Info < Warning < Error`); entries below
`core.log.threshold()` (default `Info`) are dropped without formatting. The log is a bounded ring
(`LogBuffer`, 4096 entries) with per-level counters; entries carry the clock time of the call.
Unmapped accesses, unsupported widths and re-entrancy errors are logged by the machine, once per
address/key.

## 11. Debugger access

* `Peripheral::peek` / `poke` / `summary`: side-effect-free register view for snapshots;
  time-dependent registers are computed for `view.now()` (the clock time). Implement `peek` for every
  register a snapshot, the HUD telemetry (`NGCBoardTelemetry`) or a fixture reads (timer `CCR`/`ARR`/`CR1`,
  GPIO `MODER`, `AFR`, `ODR`, `IDR`, PWR `CR1`, ...). `Board::peek(addr, width)` also reaches the core's
  PPB registers (`SCB.SCR` at `0xE000_ED10`) through `Cpu::ppb_peek32`.
* `Board::bus_read/bus_write`: monitor-style access *with* side effects at the board's time
  (`sysbus ReadDoubleWord/WriteDoubleWord` in the `.resc` scripts, e.g. the CAN `MCR = 0x10000` write).
* `Board::memory_slice(addr, len)`: raw flash/SRAM bytes for dumps.
* `core.summaries()` lists the `summary()` of every peripheral (the Renode `Summary` property).

## 12. Assembling a board

```rust
let mut board = Board::new(BoardConfig::new("ngc-handset"));       // armv7m::Cpu + NGC memory layout
let gpio_a = board.add_mapped(0x4800_0000, 0x400, Box::new(Gpio::new("gpioA", ...)))?;   // region base must be 256-byte aligned
board.connect_irq(exti, 3, 9)?;                                    // exti output 3 -> nvic@9
board.connect_input(lcd, 0 /*TE*/, gpio_d, 3)?;
board.load(0x0800_0000, &firmware.flash_image())?;                 // or Firmware::install_into
board.cpu.reset(); board.cpu.set_vtor(fw.vtor()); board.cpu.set_sp(fw.initial_sp()); board.cpu.set_pc(fw.reset_pc());
```

* **Order of `add_peripheral` is the clock-entry creation order** (section 3): follow the `.repl`
  order of the original platform.
* Regions may be any size, must not overlap each other or flash/SRAM, and may be mapped more than
  once per peripheral. The lookup is a two-level table (1 MiB windows of 256-byte granules).
* Unmapped addresses read 0 and ignore writes (Renode), warning once per address.
* `0xE000_0000..=0xE00F_FFFF` never reaches the machine (the core handles NVIC/SCB/SysTick/DWT).
* Typed access to a model: `board.get::<T>(id)`, `board.get_mut::<T>(id)`, and for methods that
  need a `Ctx` (they schedule events, drive outputs or touch the bus):
  `board.with_peripheral::<T, _>(id, |model, ctx| model.press_button(ctx, mask))`. Queued
  signal and IRQ changes are delivered before it returns.
* Assembly never calls into the CPU: the connect-time level pushes to `Irq` targets are queued and
  reach the core at the start of the next `run_until`. `Board::headless(name)` builds a board around
  `NullCpu` (always halted, records interrupt lines) for tests that only assemble, wire and poke a
  board (`bus_read/bus_write`, `with_peripheral`, `run_until` firing events).
* Flash is written like any memory. After a store, DMA write, `load` or `poke` into flash the board
  calls `CpuCore::invalidate_code_cache` before the next chunk (a flash store also ends the running
  chunk right away), so decoded instructions never go stale.
* AIRCR.SYSRESETREQ (`Cpu::take_reset_request`) makes `run_until` return early with
  `RunReport::reset_requested`; the board does nothing else. The system decides (Restart).

## 13. The run loop (`Board::run_until`)

Renode's `CpuThreadBodyInner` with the global quantum, reproduced for one machine:

```
round_end = (now / QUANTUM + 1) * QUANTUM                      // quantum grid aligned to emulation start
loop until now >= target:
    apply queued IRQ changes to the CPU; fire events already due (zero-time leftovers)
    limit = min(round_end, target)
    if the CPU is halted or locked up:   cpu.advance_idle(now, limit); advance_clock(limit); continue
    left    = ceil((limit - now) / 10)                        // instructions left in this round
    nearest = min(next queued event time, cpu.next_internal_deadline())
    chunk   = max(1, floor((nearest - clock_time) / 10)).min(left)    // Renode InstructionsToNearestLimit
    exit = cpu.run(bus, now, now + chunk * 10)                // stops early at the end of the TB on request_return,
                                                              // on WFI sleep, halt, lockup, reset request
    now = exit.now;  advance_clock(now)                       // events fire at their own times; irqs applied
    if exit.reason == Sleeping:                               // WFI counted as an executed instruction
        skip = min(max(1, floor((nearest' - clock_time) / 10)), instructions left in the round)
        cpu.advance_idle(now, now + skip * 10); now += skip * 10; advance_clock(now)
```

* **A chunk ends at a whole number of instructions *before* the nearest limit when the limit is not
  a multiple of 10 ns** (floor, minimum one instruction); the remainder becomes a one-instruction chunk.
  The event then fires during the `advance_clock` that follows, at its own time, and the CPU arbitrates the
  resulting interrupt at the next chunk start - the first instruction boundary at or after the event.
  (DESIGN.md section 5 words this as "rounded up"; the boundary at which the event takes effect is the
  same, Renode's chunk partition is the floor one.)
* While a chunk runs, the bus passes `icount` to the machine; MMIO side effects see the **lagged
  clock time** except for sync registers (section 6). The synced time is
  `chunk_start + (icount - chunk_start_icount) * 10`; the core passes the count at the start of the
  current translation block (tlib counts completed blocks only), not the count before the accessing
  instruction.
* A halted core (the handset before the 50 ms PE3 release) does not run but its clock, peripherals
  and events do. After `ExitReason::Lockup` the core stays stopped (`board.locked_up()`) and time advances.
* The `Sleeping` skip jumps to the next event in one step, so an idle board costs one iteration per event.
  Like Renode's WFI branch it does **not** look at interrupts raised by the events that fired in the
  `advance_clock` right before it: an IRQ raised by the event that ends the chunk containing the WFI is only
  taken after the skip (up to the end of the 100 us round). The same branch is what makes a masked pending
  interrupt wake the core once per round (renode-semantics `wfi-primask`).
* The quantum grid is the reason `run_until(target)` with an unaligned `target` still keeps later rounds
  aligned. The dual system calls `run_until` with quantum boundaries.
* Hot paths: flash and SRAM1 accesses are inline range checks; other plain memory, MMIO and unmapped
  addresses take one out-of-line call (one `dyn` call for MMIO). A CPU MMIO access additionally stores
  the exact time and tests the region's sync list.
* The core is reached through the small `CpuCore` trait, implemented for `armv7m::Cpu`; the loop is
  unit-tested with a scripted core (`board.rs` tests).

## 14. Testing models

`emu_core::testing::Harness` is a bare machine (no CPU) for model tests:

```rust
let mut h = Harness::new();
let id = h.add_mapped(0x4000_1000, 0x400, MyModel::new());     // or h.add(model) when it has no registers
h.connect_irq(id, 0, 25);              // line 0 -> nvic@25
h.clear_irq_changes();                 // drop the connect-time level push
let p = h.probe(id, 1);                // records line 1 (first entry = level at connect time)
h.write32(0x4000_1000, 1);             // host bus access at clock time (also read32/16/8, write16/8, read/write(width))
h.advance_to(25 * TICKS_PER_MICROSECOND);   // clock advance: events fire in order; ctx.now() == each event's time
assert_eq!(h.irq_changes(), [IrqChange { time, irq: 25, level: true }]);
assert!(h.irq_level(25)); h.next_event_time(); h.pending_events(); h.probe_changes(p);
h.get::<MyModel>(id); h.with::<MyModel, _>(id, |m, ctx| m.receive_frame(ctx, frame));
h.set_input(id, 3, true); h.peek(addr, Width::Word); h.warnings(); h.take_stop_request();

// CPU-style accesses with the chunk lag:
h.cpu_write32(addr, value, exact_time);    // exact instruction time; clock time stays where it is (sync registers advance it)
h.cpu_read32(addr, exact_time);
h.end_chunk(time);                         // the board's advance_clock at the end of the chunk (== advance_to)
```

* `h.read/write` are *host* accesses at the harness (clock) time: no lag. Use `cpu_read/cpu_write` to
  test sync registers, `ctx.sync_time()` and chunk-lag behaviour. `exact_time` must not decrease between
  CPU accesses of one chunk, and `end_chunk(t)` needs `t >= ` the last exact time.
* `request_return` shows up as `take_stop_request()`.
* `IrqChange.time` is the clock time at which the change happened (the event's own time), also when one
  `advance_to` fires several events (`MachineCore::drain_irq_changes_timed`).
* `core_mut()` exposes the `MachineCore` (`events`, `log`, `advance_clock`, `bus_read`, `clock_entry_count`).
* Board-level behaviour (assembly, `with_peripheral`, `run_until` with events and interrupt wiring) can be
  tested without a core through `Board::headless` (`NullCpu`), or with an executing core by loading a few
  hand-assembled Thumb instructions (see `real_core_takes_an_interrupt_raised_by_a_peripheral_event` in
  `crates/ngc/src/board.rs`).
* Test checklist for a model: reset values, every register's read/write at the widths the access policy
  allows, a not-allowed width, timing (exact event times with ceil-ns rounding, the first firing after
  start/enable, behaviour after reconfiguration, same-instant ordering), CPU-lag behaviour of declared and
  undeclared registers, interrupt line transitions (including no change when the level is unchanged), DMA
  requests, `peek` equals `read` without side effects, `summary` text.

## 15. Differences from Renode

| Topic | Renode | here |
| --- | --- | --- |
| Timers | `ClockEntry` in `BaseClockSource` | same arithmetic (`emu_core::clock`), but each enabled entry owns one queue event at its ceil-ns limit instead of lazy accounting; results are identical because accounting is exact |
| Handler inside a setter (zero-time limit) | runs re-entrantly in the setter | entry state updated at once, handler delivered when the peripheral's method returns (section 5.6) |
| `GPIO.Set` delivery | immediate, re-entrant, inside the sender | deferred until the sender's call returns, depth first (section 8) |
| IRQ taken | at the end of the current translation block | same: the core arbitrates at TB boundaries (CPU work package) |
| Access to the running peripheral | allowed (monitor re-entrancy) | logged error, reads 0 |
| Unmapped access log | every access | once per address |
| Threads | peripheral threads (`ObtainManagedThread`) and locks | single thread; managed threads are clock entries |
| Flash | `MappedMemory`, stores take effect | same; additionally ends the CPU chunk and invalidates predecode |
| Sub-word access | `AllowedTranslations` + NotTranslated warning | same semantics through `AccessPolicy` |
| Unaligned MMIO from the CPU | tlib splits (two aligned loads / byte stores high to low) | same (`cpu_read`/`cpu_write`); DMA and monitor accesses are not split, as in Renode |
| MMIO time | clock-source time lags the CPU except for `SyncTime()` callers | same: lagged clock time, `sync_registers()` / `ctx.sync_time()` for the sync points |
| Time unit | 1 ns ticks, events at `ceil` ns | same |
| One-shot entry after firing | its stale time-to-limit can still shorten the next chunk until the next update | not reproduced (the next chunk boundary can differ, no event time does) |
| Limits of 2^32 ns (4.29 s) or more | the time to the limit is rounded down once and the entry is updated again 1 ns later | rounded up directly (same limit time) |
| Zero-period periodic entry | `Advance` loops forever | error log, the events due at that instant are dropped |
| Several machines | clock sources advance to the minimum progress of all CPUs (non-deterministic) | the system steps boards in a fixed order (DESIGN.md section 5) |

## 16. Porting checklist

1. Read the C# class: which `I*Peripheral` interfaces, `[AllowedTranslations]`, registers
   (`DoubleWordRegisterCollection`), `GPIO` properties, timers (`LimitTimer`, `ObtainManagedThread`,
   `ScheduleAction`, `AddClockEntry`), `cpu.SyncTime()` calls, `IKnownSize.Size`, `Reset()`, `Summary`.
2. Set `access_policy()` from the interfaces. From the pinned sources (verify when porting):

| Renode class | Interfaces | `[AllowedTranslations]` | `access_policy()` |
| --- | --- | --- | --- |
| `STM32_GPIOPort` | IDoubleWord | WordToDoubleWord | `WORD_ONLY` + `HALF_TO_WORD` |
| `STM32_Timer` (also `NGCLazyPwmTimer`) | IDoubleWord | ByteToDoubleWord, WordToDoubleWord | `WORD_ONLY` + `BYTE_TO_WORD \| HALF_TO_WORD` |
| `STM32F7_USART` | IDoubleWord | ByteToDoubleWord, WordToDoubleWord | `WORD_ONLY` + `BYTE_TO_WORD \| HALF_TO_WORD` |
| `STM32F7_I2C` | IDoubleWord | ByteToDoubleWord | `WORD_ONLY` + `BYTE_TO_WORD` |
| `STMCAN`, `STM32F4_EXTI`, `STM32F4_RTC`, `STM32_IndependentWatchdog`, `STM32LDMA`, `STM32_RNG` | IDoubleWord | none | `WORD_ONLY` |
| `STM32_CRC` | IByte, IWord, IDoubleWord | none | `EXACT` |
| `Memory.ArrayMemory` | all widths | none | `EXACT` (`emu_core::ArrayMemory`) |
| `NGCParallelLCD`, `NGCAdc`, `NGCMainADC` | IWord, IDoubleWord | none | native `HALF \| WORD` |
| `NGCQuadSPI` | IByte, IDoubleWord | none | native `BYTE \| WORD` |
| `NGCEepromStore` | IByte | none | native `BYTE` |
| `NGCClockControl`, `NGCHandsetButtons`, `NGCBoardTelemetry` | IDoubleWord | none | `WORD_ONLY` |

3. Map Renode concepts:

| Renode | Framework |
| --- | --- |
| `IGPIOReceiver.OnGPIO(n, v)` | `on_input(line, level, ctx)` |
| `GPIO.Set(v)` / `Connections[i].Set(v)` | `ctx.set_output(line, v)` |
| `.repl` `a -> b@n`, `[0-3] -> nvic@[19-22]` | `connect_input`, `connect_irq` per line |
| `machine.LocalTimeSource.ElapsedVirtualTime` / `ClockSource.CurrentValue` | `ctx.now()` (clock time) |
| `new LimitTimer(machine.ClockSource, f, owner, name, limit, direction, enabled, workMode, eventEnabled, autoUpdate, divider)` | `LimitTimer::new(LimitTimerConfig { .. }, TOKEN)` + `attach`/`reset` |
| `timer.Value`, `.Limit`, `.Enabled`, `.Frequency`, `.Divider`, `.Direction`, `.Mode`, `.EventEnabled`, `.ResetValue()` | `timer.value(ctx)`, `timer.set_value(ctx, v)`, ... (section 5.2) |
| `timer.LimitReached += handler` | `if timer.on_limit_reached() { handler }` in `on_event` |
| `machine.ObtainManagedThread(f, hz)` / `(f, TimeInterval)` | `ManagedThread::new(hz, TOKEN)` / `with_period(ns, TOKEN)` |
| `machine.ScheduleAction(delay, f)` | `ctx.schedule_action(delay, TOKEN)` |
| `machine.ClockSource.AddClockEntry/ExchangeClockEntryWith/GetClockEntry/TryRemoveClockEntry` | `ctx.clock_add/clock_exchange/clock_entry/clock_remove` |
| `cpu.SyncTime()` | `sync_registers()` (unconditional, per register) or `ctx.sync_time()` |
| `cpu.RequestReturn()` | `ctx.request_return()` |
| `sysbus.ReadDoubleWord/WriteDoubleWord`, `DmaEngine` | `ctx.mem_read/mem_write`, `mem_read_bytes/mem_write_bytes` |
| `IKnownSize.Size` | size passed to `map` / `add_mapped` |
| `lock(sync)` | nothing (single threaded) |
| `Reset()`, `Summary` | `reset(ctx)`, `summary(view)` |
| `this.Log(LogLevel.X, ...)` | `emu_*!` macros, `ctx.logf` |

4. Put `// Ported from Renode 1.17.0 <path> (MIT License, Copyright (c) Antmicro).` at the top and
   `// Renode parity: ...` on every deliberate quirk.
5. Write the unit tests (section 14) before wiring the model into a board.

## 17. Complete example

A 1 MHz auto-reload timer with an interrupt line, an optional DMA store, a word-only register file and a
`CNT`-style register that Renode would read after `SyncTime()`. It is compiled and run by
`crates/emu-core/tests/framework_example.rs`; keep both in sync.

```rust
use emu_core::clock::{Direction, LimitTimer, LimitTimerConfig};
use emu_core::testing::{Harness, IrqChange};
use emu_core::*;

const US: Time = TICKS_PER_MICROSECOND;

// Register offsets.
const CTRL: u32 = 0x00; // bit 0 EN, bit 1 IRQ_EN
const LOAD: u32 = 0x04; // counts per period (Renode STM32_Timer style: limit = LOAD)
const COUNT: u32 = 0x08; // read-only counter; Renode would call cpu.SyncTime() in its read callback
const STATUS: u32 = 0x0C; // bit 0 UIF (period elapsed), write 1 to clear
const DMA_ADDR: u32 = 0x10; // if non-zero, every period stores the period count there

const CTRL_EN: u32 = 1;
const CTRL_IRQ_EN: u32 = 2;
const IRQ_LINE: u32 = 0; // output line 0 = interrupt request
const TIMER: u64 = 1; // clock-entry token

pub struct CountdownTimer {
    timer: LimitTimer,
    ctrl: u32,
    status: u32,
    dma_addr: u32,
    periods: u32,
}

impl CountdownTimer {
    pub fn new() -> Self {
        let cfg = LimitTimerConfig {
            limit: 0xFFFF_FFFF,
            direction: Direction::Ascending,
            event_enabled: true,
            ..LimitTimerConfig::new(1_000_000)
        };
        Self { timer: LimitTimer::new(cfg, TIMER), ctrl: 0, status: 0, dma_addr: 0, periods: 0 }
    }

    fn register(&self, offset: u32, clock: &dyn ClockRead) -> Option<u32> {
        match offset {
            CTRL => Some(self.ctrl),
            LOAD => Some(self.timer.limit(clock) as u32),
            COUNT => Some(self.timer.value(clock) as u32),
            STATUS => Some(self.status),
            DMA_ADDR => Some(self.dma_addr),
            _ => None,
        }
    }

    fn update_irq(&self, ctx: &mut Ctx<'_>) {
        let level = self.status & 1 != 0 && self.ctrl & CTRL_IRQ_EN != 0;
        ctx.set_output(IRQ_LINE, level); // delivered to the targets only if the level changed
    }
}

impl Peripheral for CountdownTimer {
    fn name(&self) -> &str {
        "countdown"
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.attach(ctx); // clock-entry creation order = registration order
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.reset(ctx);
        self.ctrl = 0;
        self.status = 0;
        self.update_irq(ctx);
    }

    // A Renode-style 32-bit-only device: byte and halfword accesses are turned into aligned
    // word accesses by the bus (read-modify-write for writes), like [AllowedTranslations].
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD)
    }

    // The CPU reads COUNT after cpu.SyncTime() in the original: advance the clock to the exact
    // instruction time first.
    fn sync_registers(&self) -> Vec<SyncRegister> {
        vec![SyncRegister::read(COUNT)]
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.register(offset, ctx) {
            Some(value) => value,
            None => {
                ctx.warn_once(u64::from(offset), format_args!("read from unimplemented register 0x{offset:X}"));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match offset {
            CTRL => {
                self.ctrl = value & (CTRL_EN | CTRL_IRQ_EN);
                // LimitTimer setters request a return from the CPU chunk, like Renode's.
                self.timer.set_enabled(ctx, self.ctrl & CTRL_EN != 0);
                self.update_irq(ctx);
            }
            LOAD => self.timer.set_limit(ctx, u64::from(value.max(1))),
            STATUS => {
                self.status &= !value;
                self.update_irq(ctx);
            }
            DMA_ADDR => self.dma_addr = value,
            _ => ctx.warn_once(u64::from(offset) | 1 << 32, format_args!("write to unimplemented register 0x{offset:X}")),
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        // `ctx.now()` is the entry's own limit time; the entry re-arms itself (periodic).
        if token == TIMER && self.timer.on_limit_reached() {
            self.periods += 1;
            self.status |= 1;
            if self.dma_addr != 0 {
                ctx.mem_write(self.dma_addr, Width::Word, self.periods);
            }
            self.update_irq(ctx);
        }
    }

    fn peek(&self, offset: u32, _width: Width, view: &View<'_>) -> Option<u32> {
        self.register(offset, view)
    }

    fn summary(&self, view: &View<'_>) -> String {
        format!(
            "countdown timer: enabled={}, limit={}, value={}, periods={}",
            self.timer.enabled(view),
            self.timer.limit(view),
            self.timer.value(view),
            self.periods
        )
    }

    impl_peripheral_any!();
}

const BASE: u32 = 0x4000_1000;

#[test]
fn countdown_timer_example() {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, CountdownTimer::new());
    h.connect_irq(id, IRQ_LINE, 25);
    h.clear_irq_changes(); // drop the connect-time level push

    // 10 us period (10 counts of 1 MHz), interrupt enabled, DMA target in SRAM.
    h.write32(BASE + LOAD, 10);
    h.write32(BASE + DMA_ADDR, 0x2000_0100);
    h.write32(BASE + CTRL, CTRL_EN | CTRL_IRQ_EN);
    assert_eq!(h.next_event_time(), Some(10 * US));
    assert!(h.take_stop_request(), "LimitTimer setters ask the CPU to return, like Renode's");

    h.advance_to(25 * US);
    assert_eq!(h.get::<CountdownTimer>(id).periods, 2, "limits at exactly 10 us and 20 us");
    assert_eq!(h.read32(0x2000_0100), 2, "the second period wrote its count through the bus");
    assert_eq!(h.irq_changes(), [IrqChange { time: 10 * US, irq: 25, level: true }]);
    assert!(h.irq_level(25));
    assert_eq!(h.next_event_time(), Some(30 * US));

    // COUNT is computed from clock time: 25 us is 5 counts into the third period.
    assert_eq!(h.read32(BASE + COUNT), 5);
    assert_eq!(h.peek(BASE + COUNT, Width::Word), Some(5), "peek has no side effects");

    // The bus turns sub-word accesses into word accesses for this policy.
    assert_eq!(h.read8(BASE + LOAD), 10);
    h.write8(BASE + LOAD + 1, 0x01); // read-modify-write: LOAD = 0x10A
    assert_eq!(h.read32(BASE + LOAD), 0x10A);
    h.write32(BASE + LOAD, 10);

    // Acknowledge the interrupt: the line drops.
    h.write32(BASE + STATUS, 1);
    assert!(!h.irq_level(25));
    assert_eq!(h.irq_changes().last(), Some(&IrqChange { time: 25 * US, irq: 25, level: false }));

    // Chunk lag: the CPU runs a chunk from 25 us to 125 us. The 30 us limit has not been processed, so a
    // read of the undeclared STATUS register at 31 us still sees UIF clear; reading the declared COUNT
    // register first advances the clock to 31 us (the 30 us limit fires) and sees the new period.
    assert_eq!(h.cpu_read32(BASE + STATUS, 31 * US), 0, "clock time lags: the 30 us event has not fired yet");
    assert_eq!(h.cpu_read32(BASE + COUNT, 31 * US), 1, "synced to 31 us: 1 count into the period that began at 30 us");
    assert_eq!(h.cpu_read32(BASE + STATUS, 31 * US), 1, "the limit event ran during the sync");
    h.end_chunk(125 * US);

    // Disabling stops the entry: no event is pending, the value is kept.
    h.write32(BASE + CTRL, 0);
    assert_eq!(h.next_event_time(), None);
    let stopped = h.read32(BASE + COUNT);
    h.advance_to(200 * US);
    assert_eq!(h.read32(BASE + COUNT), stopped);

    // Unimplemented registers warn once and read 0.
    assert_eq!(h.read32(BASE + 0x20), 0);
    assert_eq!(h.read32(BASE + 0x20), 0);
    assert_eq!(h.warnings().len(), 1);
    assert!(h.core().summaries().iter().any(|(n, s)| n == "countdown" && s.contains("periods=")));
}
```

## 18. Contract with the CPU (`crates/armv7m`)

What the board relies on, and what changes for the CPU work package with this time model:

1. **Constants.** `Time` is ns, `TICKS_PER_INSTRUCTION = 10`, `QUANTUM = 100_000`. `ticks_per_cycle(80_000_000)` is
   `None`: SysTick and DWT must not derive their period from it. A test that hard-codes `480` ticks per 80 MHz cycle
   is obsolete (80 MHz is 12.5 ns).
2. **SysTick and DWT CYCCNT use `emu_core::clock::{ClockEntry, LocalClock}`** (section 5.8): a descending periodic entry
   at `systick_hz` with Renode's reload/first-period rules, ceil-ns limit events, discarded overshoot. SysTick is a
   core-internal deadline: `next_internal_deadline()` returns the **absolute** time (`LocalClock::next_limit`) of the next
   limit, or `None`.
3. **`run(bus, now, until)`.** The board always passes `until = now + n * ticks_per_instruction` (n >= 1). Execute exactly
   `n` instructions unless the chunk ends earlier; `RunExit.now = now + executed * ticks_per_instruction` (never beyond
   `until` by more than the instruction in flight; WFI counts as an executed instruction). The chunk end is also a TB end.
4. **Stop requests.** `BUS_STOP_REQUESTED` (from `ctx.request_return()`, flash writes) means *return at the end of the
   current translation block*, not after the current instruction; report `ExitReason::StopRequested` with the
   instructions executed so far.
5. **Interrupt arbitration at TB boundaries only** (DESIGN.md section 5): IRQ-change notifications are applied to the NVIC
   immediately (`drain_irq_changes`), but pending exceptions are taken only at the start of a TB.
6. **Synced time inside a chunk.** The bus receives the instruction count at the start of the current translation
   block (tlib's executed-instruction counter advances only when a block completes). SysTick `CVR` reads and DWT
   `CYCCNT` reads (Renode `SyncTime()` callers) are computed for
   `chunk_start + (icount - chunk_start_icount) * ticks_per_instruction` with that block-start count, not for a lagged time.
7. **`advance_idle(now, until)`** (halted, sleeping, locked up) must process SysTick limits in `(now, until]` through
   `LocalClock::run_until`, in order, with the same effects as during `run` (COUNTFLAG, pend, wake from sleep).
8. **WFI.** `run` returns `Sleeping` after executing the WFI instruction (counted as executed); the board then skips to the
   nearest limit through `advance_idle`. A sleeping core woken by an interrupt wakes lazily at the next `run` call.
