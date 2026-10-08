// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/I2C/STM32F7_I2C.cs (MIT License, Copyright (c) Antmicro).
// The target trait mirrors Renode 1.17.0 src/Emulator/Main/Peripherals/I2C/II2CPeripheral.cs; target registration and
// lookup mirror `SimpleContainer<II2CPeripheral>` (an integer-keyed dictionary, `TryGetByAddress`).

//! Renode `I2C.STM32F7_I2C` (STM32F7/L4-style I2C controller, master side) and the I2C target trait.
//!
//! The controller is a faithful port of the Renode model, **not** of the ST reference manual, so that
//! firmware sees exactly what it sees under Renode:
//!
//! * A transfer is *synchronous with the register access* that causes it. Writing CR2 with START calls
//!   the target immediately (a read transfer fetches all `NBYTES` right away, a write transfer waits for
//!   `NBYTES` bytes in TXDR and hands them to the target as one chunk), the flags (TXIS, RXNE, TC, TCR,
//!   STOPF) change in the same access and the event interrupt line is recomputed at once. There are no
//!   events and no bus timing; ISR/NBYTES polling loops of the HAL succeed immediately.
//! * A multi-byte transfer reaches the target as *chunks*: with RELOAD set (HAL: the memory-address
//!   byte of `HAL_I2C_Mem_Write`, or 255-byte pieces) the controller calls [`I2cTarget::write`] once per
//!   `NBYTES` bytes and raises TCR instead of stopping; software then writes CR2 again (no START) to
//!   continue ("extend"). `FinishTransmission` is called only at AUTOEND completion or CR2.STOP.
//! * `NACKF` is a tag in the Renode class (`TODO: implement NACKF`): it always reads 0 and no error
//!   interrupt is ever raised. A START to an address no target answers only logs a warning
//!   (`Unknown slave at address N.`); no flag changes, so a polling HAL times out. This is Renode
//!   parity, deliberately not an ST-manual NACK.
//! * Register-level quirks are kept: write-1-to-clear fields in ICR never count as a "change", so
//!   clearing STOPF/ADDR through ICR does not recompute the interrupt line until the next register
//!   write that does; TIMINGR (and the other tagged bits) store nothing and read 0; writing tagged
//!   bits logs Renode's `Unhandled write ... Tags: ...` warning.
//!
//! Bus interface: `AccessPolicy::WORD_ONLY` plus Renode's `[AllowedTranslations(ByteToDoubleWord)]`;
//! byte accesses are read-modify-write through the 32-bit register (the machine does that), halfword
//! accesses are "not supported". Output lines: [`EVENT_INTERRUPT`], [`ERROR_INTERRUPT`] (never raised)
//! and [`DMA_RECEIVE`] (the Renode `DmaReceive` GPIO, unconnected on the NGC boards).
//!
//! # Targets
//!
//! [`I2cTarget`] mirrors Renode `II2CPeripheral` (`Write(byte[])`, `Read(int)`, `FinishTransmission()`).
//! Targets are attached with [`Stm32F7I2c::attach`] at an integer address, the equivalent of
//! `.repl` `eeprom0: Memory.NGCEepromBank @ i2c1 0x50` (Renode `SimpleContainer.Register`): the 7-bit
//! address of a 7-bit transfer, or the raw 10-bit `SADD` when `ADD10` is set. A target that needs time
//! (the MS5837 conversion delay, Renode `machine.ScheduleAction`) schedules through [`I2cCtx`]
//! ([`I2cCtx::schedule_action`]); the events are delivered to [`I2cTarget::on_event`] by the controller.
//! [`testing`] has the HAL polling flows (`HAL_I2C_Master_Transmit/Receive`, `HAL_I2C_Mem_Write/Read`) as
//! register sequences for tests of targets and boards.
//!
//! # Verification
//!
//! `crates/ngc/tests/renode_i2c_eeprom` replays more than 12 000 register operations (scripted HAL flows,
//! interrupt/DMA/slave-mode cases and seeded random traffic) recorded from the unmodified Renode 1.17.0
//! `STM32F7_I2C` and `NGCEeprom` models and requires identical register reads, ISR/CR2/interrupt lines after
//! every step, target calls, EEPROM images and log messages.

use emu_core::{
    emu_error, emu_info, emu_warn, impl_peripheral_any, AccessPolicy, ClockId, Ctx, EventId, LogLevel, Peripheral, Time,
    Translations, View, Width,
};
use std::any::Any;
use std::collections::VecDeque;
use std::fmt;

/// Size of the register window (`IKnownSize.Size`).
pub const I2C_SIZE: u32 = 0x400;

/// Output line 0: `EventInterrupt` (the `.repl` line `EventInterrupt -> nvic@31`).
pub const EVENT_INTERRUPT: u32 = 0;
/// Output line 1: `ErrorInterrupt` (`-> nvic@32`). The Renode model never raises it; connect it anyway
/// so the NVIC input gets its (low) initial level.
pub const ERROR_INTERRUPT: u32 = 1;
/// Output line 2: `DmaReceive` (RXDMAEN request, high while the receive queue is not empty).
pub const DMA_RECEIVE: u32 = 2;

/// Register offsets and bit masks (offsets as in the Renode `Registers` enum, ST names).
pub mod regs {
    pub const CR1: u32 = 0x00;
    pub const CR2: u32 = 0x04;
    pub const OAR1: u32 = 0x08;
    pub const OAR2: u32 = 0x0C;
    pub const TIMINGR: u32 = 0x10;
    /// In the Renode `Registers` enum but not implemented: accesses log `Unhandled ...` and read 0.
    pub const TIMEOUTR: u32 = 0x14;
    pub const ISR: u32 = 0x18;
    pub const ICR: u32 = 0x1C;
    /// In the Renode `Registers` enum but not implemented.
    pub const PECR: u32 = 0x20;
    pub const RXDR: u32 = 0x24;
    pub const TXDR: u32 = 0x28;

    pub const CR1_PE: u32 = 1 << 0;
    pub const CR1_TXIE: u32 = 1 << 1;
    pub const CR1_RXIE: u32 = 1 << 2;
    pub const CR1_ADDRIE: u32 = 1 << 3;
    pub const CR1_NACKIE: u32 = 1 << 4;
    pub const CR1_STOPIE: u32 = 1 << 5;
    pub const CR1_TCIE: u32 = 1 << 6;
    /// Bit 7 (ERRIE) is a stored but otherwise ignored bit in the Renode class.
    pub const CR1_ERRIE: u32 = 1 << 7;
    pub const CR1_RXDMAEN: u32 = 1 << 15;
    pub const CR1_NOSTRETCH: u32 = 1 << 17;

    pub const CR2_SADD: u32 = 0x3FF;
    pub const CR2_RD_WRN: u32 = 1 << 10;
    pub const CR2_ADD10: u32 = 1 << 11;
    pub const CR2_START: u32 = 1 << 13;
    pub const CR2_STOP: u32 = 1 << 14;
    /// Tag only: stored nowhere, logs `Unhandled write` when set.
    pub const CR2_NACK: u32 = 1 << 15;
    pub const CR2_NBYTES_SHIFT: u32 = 16;
    pub const CR2_NBYTES: u32 = 0xFF << CR2_NBYTES_SHIFT;
    pub const CR2_RELOAD: u32 = 1 << 24;
    pub const CR2_AUTOEND: u32 = 1 << 25;

    pub const OAR1_OA1EN: u32 = 1 << 15;
    pub const OAR2_OA2EN: u32 = 1 << 15;

    pub const ISR_TXE: u32 = 1 << 0;
    pub const ISR_TXIS: u32 = 1 << 1;
    pub const ISR_RXNE: u32 = 1 << 2;
    pub const ISR_ADDR: u32 = 1 << 3;
    /// Tag only: always reads 0 in the Renode class.
    pub const ISR_NACKF: u32 = 1 << 4;
    pub const ISR_STOPF: u32 = 1 << 5;
    pub const ISR_TC: u32 = 1 << 6;
    pub const ISR_TCR: u32 = 1 << 7;
    /// Tag only: always reads 0 in the Renode class.
    pub const ISR_BUSY: u32 = 1 << 15;
    pub const ISR_DIR: u32 = 1 << 16;

    pub const ICR_ADDRCF: u32 = 1 << 3;
    pub const ICR_STOPCF: u32 = 1 << 5;
}

use regs::*;

// ---- targets -------------------------------------------------------------------------------

/// A device on the I2C bus (Renode `II2CPeripheral`). Object safe; the controller owns its targets.
///
/// Call protocol (see the module documentation): a master write transaction arrives as one or more
/// [`write`](I2cTarget::write) chunks (the first chunk of a `HAL_I2C_Mem_Write` is just the address
/// byte), a master read as [`read`](I2cTarget::read) calls (one per START/RELOAD piece, the count is
/// `NBYTES`), and a completed transfer (AUTOEND or STOP) ends with
/// [`finish_transmission`](I2cTarget::finish_transmission). A zero-length `write` is the
/// address-only readiness probe that the controller ACKs.
pub trait I2cTarget: 'static {
    /// Instance name (the `.repl` name, `eeprom0`, `pressure1`); prefixes messages logged through [`I2cCtx`].
    fn name(&self) -> &str;

    /// `II2CPeripheral.Write(byte[] data)`: one chunk of a master write.
    fn write(&mut self, data: &[u8], ctx: &mut I2cCtx<'_, '_>);

    /// `II2CPeripheral.Read(int count)`: `count` bytes for the master. Like the C# `byte[]` it may return
    /// fewer bytes (the controller queues whatever it gets; RXNE stays low for missing bytes).
    fn read(&mut self, count: usize, ctx: &mut I2cCtx<'_, '_>) -> Vec<u8>;

    /// `II2CPeripheral.FinishTransmission()`: STOP of a transfer that addressed this target.
    fn finish_transmission(&mut self, ctx: &mut I2cCtx<'_, '_>);

    /// `IPeripheral.Reset()`: the machine reset (called from the controller's `reset`, after its own).
    fn reset(&mut self, _ctx: &mut I2cCtx<'_, '_>) {}

    /// An event the target scheduled through [`I2cCtx::schedule_at`], [`I2cCtx::schedule_in`] or
    /// [`I2cCtx::schedule_action`] fires. `scheduled` is the event time, or for an action the
    /// **scheduling time** (see `Peripheral::on_event`).
    fn on_event(&mut self, _token: u64, _scheduled: Time, _ctx: &mut I2cCtx<'_, '_>) {}

    /// One-line state (the C# `Summary`, where the model has one).
    fn summary(&self) -> String {
        String::new()
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl dyn I2cTarget {
    /// The concrete target, if it is a `T`.
    pub fn downcast_ref<T: I2cTarget>(&self) -> Option<&T> {
        self.as_any().downcast_ref::<T>()
    }

    pub fn downcast_mut<T: I2cTarget>(&mut self) -> Option<&mut T> {
        self.as_any_mut().downcast_mut::<T>()
    }
}

/// Implements `as_any`/`as_any_mut` inside an `impl I2cTarget for T` block.
#[macro_export]
macro_rules! impl_i2c_target_any {
    () => {
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
    };
}

/// Largest user token a target can schedule (the top 8 bits of the machine token carry the target slot).
pub const MAX_TARGET_TOKEN: u64 = (1 << 56) - 1;

/// Most targets one controller can hold (the slot is 8 bits wide in event tokens).
pub const MAX_TARGETS: usize = 255;

/// What a target can do to the machine while the controller calls it: read the clock, schedule events
/// that come back to [`I2cTarget::on_event`], and log. The clock is the machine's clock time (it lags
/// the accessing instruction inside a CPU chunk, `docs/framework.md` section 2.2); Renode targets use
/// `machine.ScheduleAction`, which first syncs to the exact instruction time: [`schedule_action`].
///
/// [`schedule_action`]: I2cCtx::schedule_action
pub struct I2cCtx<'c, 'a> {
    ctx: &'c mut Ctx<'a>,
    slot: u64,
    name: &'c str,
}

impl<'c, 'a> I2cCtx<'c, 'a> {
    /// The machine's clock time (`Ctx::now`).
    pub fn now(&self) -> Time {
        self.ctx.now()
    }

    /// `cpu.SyncTime()`: advances the clock to the exact time of the CPU access in progress and returns it
    /// (`Ctx::sync_time`; a no-op outside a CPU access).
    pub fn sync_time(&mut self) -> Time {
        self.ctx.sync_time()
    }

    /// `cpu.RequestReturn()` (`Ctx::request_return`).
    pub fn request_return(&mut self) {
        self.ctx.request_return();
    }

    /// Renode `machine.ScheduleAction(delay, action)` for this target (`Ctx::schedule_action`): syncs the
    /// clock to the exact CPU time, fires [`I2cTarget::on_event`]`(token, scheduling time)` `delay` ns
    /// later and asks the CPU to return. `token` must not exceed [`MAX_TARGET_TOKEN`].
    pub fn schedule_action(&mut self, delay: Time, token: u64) -> ClockId {
        debug_assert!(token <= MAX_TARGET_TOKEN, "I2C target event token exceeds 56 bits");
        self.ctx.schedule_action(delay, (self.slot << 56) | (token & MAX_TARGET_TOKEN))
    }

    /// Removes a pending action (`Ctx::clock_remove`); `false` if it already ran.
    pub fn cancel_action(&mut self, id: ClockId) -> bool {
        self.ctx.clock_remove(id)
    }

    /// Schedules an ordinary event `on_event(token, time)` of this target at absolute virtual `time`
    /// (no sync, no return request). `token` must not exceed [`MAX_TARGET_TOKEN`].
    pub fn schedule_at(&mut self, time: Time, token: u64) -> EventId {
        debug_assert!(token <= MAX_TARGET_TOKEN, "I2C target event token exceeds 56 bits");
        self.ctx.schedule_at(time, (self.slot << 56) | (token & MAX_TARGET_TOKEN))
    }

    /// Schedules an ordinary event relative to [`now`](I2cCtx::now).
    pub fn schedule_in(&mut self, delay: Time, token: u64) -> EventId {
        let time = self.ctx.now().saturating_add(delay);
        self.schedule_at(time, token)
    }

    pub fn cancel(&mut self, id: EventId) -> bool {
        self.ctx.cancel(id)
    }

    pub fn is_scheduled(&self, id: EventId) -> bool {
        self.ctx.is_scheduled(id)
    }

    /// Logs `"<target name>: <message>"` (formatted only when `level` is enabled).
    pub fn logf(&mut self, level: LogLevel, args: fmt::Arguments<'_>) {
        if self.ctx.log_enabled(level) {
            let name = self.name;
            self.ctx.logf(level, format_args!("{name}: {args}"));
        }
    }

    /// Like `Ctx::warn_once`, per target.
    pub fn warn_once(&mut self, key: u64, args: fmt::Arguments<'_>) {
        if self.ctx.log_enabled(LogLevel::Warning) {
            let name = self.name;
            let key = key ^ (self.slot + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            self.ctx.warn_once(key, format_args!("{name}: {args}"));
        }
    }

    /// The underlying machine context for everything else. Do not schedule events through it: they
    /// would reach the controller's own `on_event` without the routing prefix.
    pub fn ctx(&mut self) -> &mut Ctx<'a> {
        self.ctx
    }
}

/// Why [`Stm32F7I2c::attach`] refused a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum I2cError {
    /// Renode `RegistrationException`: "The specified registration point is already in use."
    AddressInUse(u32),
    /// More than [`MAX_TARGETS`] targets.
    TooManyTargets,
}

impl fmt::Display for I2cError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            I2cError::AddressInUse(address) => {
                write!(f, "the specified registration point (I2C address 0x{address:X}) is already in use")
            }
            I2cError::TooManyTargets => write!(f, "too many I2C targets (at most {MAX_TARGETS})"),
        }
    }
}

impl std::error::Error for I2cError {}

// ---- register layout (Renode field/tag definitions) -------------------------------------------

/// An unimplemented field (`WithTag`/`WithReservedBits`): stores nothing, only named in the
/// `Unhandled write` warning.
struct Tag {
    name: &'static str,
    pos: u32,
    width: u32,
}

const fn tag(name: &'static str, pos: u32, width: u32) -> Tag {
    Tag { name, pos, width }
}

impl Tag {
    fn mask(&self) -> u32 {
        (((1u64 << self.width) - 1) << self.pos) as u32
    }
}

/// Which bits of a register are fields and how they are written (Renode `FieldMode`).
struct Layout {
    /// `FieldMode.Read | FieldMode.Write`: stored, writable.
    rw: u32,
    /// `FieldMode.Read`: stored (set by the model), writes ignored.
    ro: u32,
    /// `FieldMode.WriteOneToClear`.
    w1c: u32,
    tags: &'static [Tag],
}

impl Layout {
    fn defined(&self) -> u32 {
        self.rw | self.ro | self.w1c
    }
}

const CR1_TAGS: &[Tag] = &[
    tag("DNF", 8, 4),
    tag("ANFOFF", 12, 1),
    tag("RESERVED", 13, 1),
    tag("TXDMAEN", 14, 1),
    tag("SBC", 16, 1),
    tag("WUPEN", 18, 1),
    tag("GCEN", 19, 1),
    tag("SMBHEN", 20, 1),
    tag("SMBDEN", 21, 1),
    tag("ALERTEN", 22, 1),
    tag("PECEN", 23, 1),
    tag("RESERVED", 24, 8),
];
const CR2_TAGS: &[Tag] = &[tag("HEAD10R", 12, 1), tag("NACK", 15, 1), tag("PECBYTE", 26, 1), tag("RESERVED", 27, 5)];
const OAR1_TAGS: &[Tag] = &[tag("RESERVED", 11, 4), tag("RESERVED", 16, 16)];
const OAR2_TAGS: &[Tag] = &[tag("RESERVED", 0, 1), tag("RESERVED", 11, 4), tag("RESERVED", 16, 16)];
const TIMINGR_TAGS: &[Tag] = &[
    tag("SCLL", 0, 8),
    tag("SCLH", 8, 8),
    tag("SDADEL", 16, 4),
    tag("SCLDEL", 20, 4),
    tag("RESERVED", 24, 4),
    tag("PRESC", 28, 4),
];
const ISR_TAGS: &[Tag] = &[
    tag("NACKF", 4, 1),
    tag("BERR", 8, 1),
    tag("ARLO", 9, 1),
    tag("OVR", 10, 1),
    tag("PECERR", 11, 1),
    tag("TIMEOUT", 12, 1),
    tag("ALERT", 13, 1),
    tag("RESERVED", 14, 1),
    tag("BUSY", 15, 1),
    tag("ADDCODE", 17, 7),
    tag("RESERVED", 24, 8),
];
const ICR_TAGS: &[Tag] = &[
    tag("RESERVED", 0, 3),
    tag("NACKCF", 4, 1),
    tag("RESERVED", 6, 2),
    tag("BERRCF", 8, 1),
    tag("ARLOCF", 9, 1),
    tag("OVRCF", 10, 1),
    tag("PECCF", 11, 1),
    tag("TIMOUTCF", 12, 1),
    tag("ALERTCF", 13, 1),
    tag("RESERVED", 14, 18),
];
// The Renode class reserves bits 9..31 of RXDR/TXDR (bit 8 is covered by nothing).
const DATA_TAGS: &[Tag] = &[tag("RESERVED", 9, 23)];

const CR1_LAYOUT: Layout = Layout {
    rw: CR1_PE | CR1_TXIE | CR1_RXIE | CR1_ADDRIE | CR1_NACKIE | CR1_STOPIE | CR1_TCIE | CR1_ERRIE | CR1_RXDMAEN | CR1_NOSTRETCH,
    ro: 0,
    w1c: 0,
    tags: CR1_TAGS,
};
const CR2_LAYOUT: Layout = Layout {
    rw: CR2_SADD | CR2_RD_WRN | CR2_ADD10 | CR2_START | CR2_STOP | CR2_NBYTES | CR2_RELOAD | CR2_AUTOEND,
    ro: 0,
    w1c: 0,
    tags: CR2_TAGS,
};
const OAR1_LAYOUT: Layout = Layout { rw: 0x3FF | (1 << 10) | OAR1_OA1EN, ro: 0, w1c: 0, tags: OAR1_TAGS };
const OAR2_LAYOUT: Layout = Layout { rw: (0x7F << 1) | (0x7 << 8) | OAR2_OA2EN, ro: 0, w1c: 0, tags: OAR2_TAGS };
const TIMINGR_LAYOUT: Layout = Layout { rw: 0, ro: 0, w1c: 0, tags: TIMINGR_TAGS };
const ISR_LAYOUT: Layout = Layout {
    rw: ISR_TXE | ISR_TXIS,
    ro: ISR_RXNE | ISR_ADDR | ISR_STOPF | ISR_TC | ISR_TCR | ISR_DIR,
    w1c: 0,
    tags: ISR_TAGS,
};
const ICR_LAYOUT: Layout = Layout { rw: 0, ro: 0, w1c: ICR_ADDRCF | ICR_STOPCF, tags: ICR_TAGS };
const RXDR_LAYOUT: Layout = Layout { rw: 0, ro: 0xFF, w1c: 0, tags: DATA_TAGS };
const TXDR_LAYOUT: Layout = Layout { rw: 0xFF, ro: 0, w1c: 0, tags: DATA_TAGS };

fn layout_for(offset: u32) -> Option<&'static Layout> {
    Some(match offset {
        CR1 => &CR1_LAYOUT,
        CR2 => &CR2_LAYOUT,
        OAR1 => &OAR1_LAYOUT,
        OAR2 => &OAR2_LAYOUT,
        TIMINGR => &TIMINGR_LAYOUT,
        ISR => &ISR_LAYOUT,
        ICR => &ICR_LAYOUT,
        RXDR => &RXDR_LAYOUT,
        TXDR => &TXDR_LAYOUT,
        _ => return None,
    })
}

/// Renode `BitHelper.GetSetBitsPretty`: `2-3, 5-7, 10`, or `(none)`.
fn pretty_set_bits(mask: u32) -> String {
    if mask == 0 {
        return "(none)".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut bit = 0u32;
    while bit < 32 {
        if mask & (1 << bit) == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 32 && mask & (1 << (bit + 1)) != 0 {
            bit += 1;
        }
        parts.push(if bit == start { start.to_string() } else { format!("{start}-{bit}") });
        bit += 1;
    }
    parts.join(", ")
}

/// Renode's tag warning for the written bits no field handles, for example
/// `Unhandled write to offset 0x4. Unhandled bits: [15] when writing value 0x2008000. Tags: NACK (0x1).`
/// `None` when no tag covers an unhandled bit (the Renode framework then logs nothing).
fn unhandled_write_message(offset: u32, unhandled: u32, written: u32, tags: &[Tag]) -> Option<String> {
    let mut listed = String::new();
    for tag in tags.iter().filter(|t| unhandled & t.mask() != 0) {
        if !listed.is_empty() {
            listed.push_str(", ");
        }
        let value = (written & tag.mask()) >> tag.pos;
        listed.push_str(&format!("{} (0x{:X})", tag.name, value));
    }
    if listed.is_empty() {
        return None;
    }
    Some(format!(
        "Unhandled write to offset 0x{offset:X}. Unhandled bits: [{}] when writing value 0x{written:X}. Tags: {listed}.",
        pretty_set_bits(unhandled)
    ))
}

/// Warn-once key for messages that include a register value.
fn warn_key(kind: u64, offset: u32, value: u32) -> u64 {
    (kind << 56) | (u64::from(offset) << 32) | u64::from(value)
}

// ---- controller ---------------------------------------------------------------------------

struct TargetSlot {
    address: u32,
    name: String,
    target: Box<dyn I2cTarget>,
}

/// Renode `STM32F7_I2C`: register model, transfer state machine and the target bus.
pub struct Stm32F7I2c {
    name: String,
    targets: Vec<TargetSlot>,

    // Register storage (`PeripheralRegister.UnderlyingValue`); only field bits are ever stored.
    cr1: u32,
    cr2: u32,
    oar1: u32,
    oar2: u32,
    /// TIMINGR has only tags in the Renode class: nothing is ever stored.
    timingr: u32,
    isr: u32,
    icr: u32,
    rxdr: u32,
    txdr: u32,

    // Fields of the Renode class.
    tx_data: VecDeque<u8>,
    rx_data: VecDeque<u8>,
    /// Slot of `currentSlave`.
    current_slave: Option<usize>,
    current_slave_address: u32,
    transfer_outgoing: bool,
    transmit_interrupt_status: bool,
    master_mode: bool,
}

impl Stm32F7I2c {
    /// `new STM32F7_I2C(machine)`: registers at their reset values, interrupt lines low, no targets.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            targets: Vec::new(),
            cr1: 0,
            cr2: 0,
            oar1: 0,
            oar2: 0,
            timingr: 0,
            isr: ISR_TXE, // ISR reset value 1
            icr: 0,
            rxdr: 0,
            txdr: 0,
            tx_data: VecDeque::new(),
            rx_data: VecDeque::new(),
            current_slave: None,
            current_slave_address: 0,
            transfer_outgoing: false,
            transmit_interrupt_status: false,
            master_mode: false,
        }
    }

    // ---- target bus ----

    /// Registers `target` at `address` (Renode `SimpleContainer.Register` with a `NumberRegistrationPoint<int>`):
    /// the 7-bit device address (`0x50` for an `@ i2c1 0x50` line). Fails if the address is taken.
    pub fn attach(&mut self, address: u32, target: Box<dyn I2cTarget>) -> Result<(), I2cError> {
        if self.targets.iter().any(|slot| slot.address == address) {
            return Err(I2cError::AddressInUse(address));
        }
        if self.targets.len() >= MAX_TARGETS {
            return Err(I2cError::TooManyTargets);
        }
        let name = target.name().to_string();
        self.targets.push(TargetSlot { address, name, target });
        Ok(())
    }

    /// Addresses of the attached targets in attach order.
    pub fn target_addresses(&self) -> Vec<u32> {
        self.targets.iter().map(|slot| slot.address).collect()
    }

    /// The target at `address` (Renode `TryGetByAddress`).
    pub fn target_at(&self, address: u32) -> Option<&(dyn I2cTarget + 'static)> {
        self.targets.iter().find(|slot| slot.address == address).map(|slot| slot.target.as_ref())
    }

    pub fn target_at_mut(&mut self, address: u32) -> Option<&mut (dyn I2cTarget + 'static)> {
        self.targets.iter_mut().find(|slot| slot.address == address).map(|slot| slot.target.as_mut())
    }

    /// The concrete target at `address`, if it is a `T` (for typed inputs such as sensor values).
    pub fn target<T: I2cTarget>(&self, address: u32) -> Option<&T> {
        self.target_at(address)?.downcast_ref::<T>()
    }

    pub fn target_mut<T: I2cTarget>(&mut self, address: u32) -> Option<&mut T> {
        self.target_at_mut(address)?.downcast_mut::<T>()
    }

    /// `(address, name, summary)` of every target.
    pub fn target_summaries(&self) -> Vec<(u32, String, String)> {
        self.targets.iter().map(|slot| (slot.address, slot.name.clone(), slot.target.summary())).collect()
    }

    fn find_slot(&self, address: u32) -> Option<usize> {
        self.targets.iter().position(|slot| slot.address == address)
    }

    fn call_target<R>(
        &mut self,
        slot: usize,
        ctx: &mut Ctx<'_>,
        f: impl FnOnce(&mut dyn I2cTarget, &mut I2cCtx<'_, '_>) -> R,
    ) -> R {
        let TargetSlot { name, target, .. } = &mut self.targets[slot];
        let mut target_ctx = I2cCtx { ctx, slot: slot as u64, name: name.as_str() };
        f(target.as_mut(), &mut target_ctx)
    }

    // ---- public properties of the Renode class ----

    /// `RxNotEmpty`.
    pub fn rx_not_empty(&self) -> bool {
        !self.rx_data.is_empty()
    }

    /// `OwnAddress1Enabled`.
    pub fn own_address1_enabled(&self) -> bool {
        self.oar1 & OAR1_OA1EN != 0
    }

    /// `II2CPeripheral.Write(byte[])` of the controller itself (another master writes to it): the slave
    /// enters receiver mode and the bytes queue for RXDR. No interrupt update, as in Renode.
    pub fn slave_write(&mut self, data: &[u8]) {
        // RM0444 Rev 5, p.991/1390 "0: Write transfer, slave enters receiver mode."
        self.transfer_outgoing = false;
        self.rx_data.extend(data.iter().copied());
    }

    /// `II2CPeripheral.Read(int)` of the controller itself (another master reads from it): returns
    /// `count` bytes of what software queued in TXDR, or nothing while fewer bytes are queued.
    pub fn slave_read(&mut self, count: usize, ctx: &mut Ctx<'_>) -> Vec<u8> {
        if self.isr & ISR_ADDR == 0 {
            // Renode parity: the protocol has no start/stop bits, a read begins the transfer.
            self.transfer_outgoing = count > 0;
            if count > 0xFF {
                // `bytesToTransfer.Value = (uint)count` throws ConstructionException in Renode.
                emu_error!(ctx, "Value exceeds the size of the field.");
                return Vec::new();
            }
            self.set_nbytes(count as u32);
            self.isr |= ISR_ADDR;
            self.update(ctx);
        }

        if self.tx_data.len() >= self.nbytes() as usize {
            // STOP condition
            self.isr |= ISR_STOPF;
            self.transmit_interrupt_status = false;
            self.isr &= !ISR_ADDR;
            self.update(ctx);
        } else {
            // TODO (Renode): return partial results
            return Vec::new();
        }

        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            match self.tx_data.pop_front() {
                Some(byte) => result.push(byte),
                None => return Vec::new(),
            }
        }
        result
    }

    /// `II2CPeripheral.FinishTransmission()` of the controller itself: nothing in Renode.
    pub fn slave_finish_transmission(&mut self) {}

    // ---- register access ----

    fn nbytes(&self) -> u32 {
        (self.cr2 & CR2_NBYTES) >> CR2_NBYTES_SHIFT
    }

    fn set_nbytes(&mut self, value: u32) {
        self.cr2 = (self.cr2 & !CR2_NBYTES) | ((value << CR2_NBYTES_SHIFT) & CR2_NBYTES);
    }

    /// ISR as a read returns it: the field value providers (TXE always set, TXIS, RXNE, DIR) merged
    /// into the stored ADDR/STOPF/TC/TCR bits.
    fn isr_value(&self) -> u32 {
        let mut value = self.isr | ISR_TXE;
        value = (value & !ISR_TXIS) | if self.transmit_interrupt_status { ISR_TXIS } else { 0 };
        value = (value & !ISR_RXNE) | if self.rx_data.is_empty() { 0 } else { ISR_RXNE };
        value = (value & !ISR_DIR) | if self.transfer_outgoing { ISR_DIR } else { 0 };
        value
    }

    fn read_register(&mut self, offset: u32, ctx: &mut Ctx<'_>) -> u32 {
        match offset {
            CR1 => self.cr1,
            CR2 => self.cr2,
            OAR1 => self.oar1,
            OAR2 => self.oar2,
            TIMINGR => self.timingr, // all tags: nothing is stored, always 0
            ISR => {
                self.isr = self.isr_value(); // the value providers write into the register
                self.isr
            }
            ICR => self.icr & !ICR_LAYOUT.w1c,
            RXDR => {
                let byte = self.receive_data_read(ctx);
                self.rxdr = (self.rxdr & !0xFF) | (byte & 0xFF);
                self.rxdr
            }
            TXDR => self.txdr,
            _ => {
                ctx.warn_once(warn_key(1, offset, 0), format_args!("Unhandled read from offset 0x{offset:X}."));
                0
            }
        }
    }

    /// Mirrors `PeripheralRegister.WriteInner` followed by the register's callbacks:
    /// field storage by mode, field write callbacks (definition order), register write callbacks,
    /// register change callback (only if a stored field changed), then the unhandled-bits warning.
    fn write_register(&mut self, offset: u32, value: u32, ctx: &mut Ctx<'_>) {
        let Some(layout) = layout_for(offset) else {
            ctx.warn_once(
                warn_key(2, offset, value),
                format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}."),
            );
            return;
        };
        let stored = match offset {
            CR1 => &mut self.cr1,
            CR2 => &mut self.cr2,
            OAR1 => &mut self.oar1,
            OAR2 => &mut self.oar2,
            TIMINGR => &mut self.timingr,
            ISR => &mut self.isr,
            ICR => &mut self.icr,
            RXDR => &mut self.rxdr,
            _ => &mut self.txdr,
        };
        let base = *stored;
        let difference = base ^ value;
        // FieldMode.Write: updated when any bit of the field differs. FieldMode.WriteOneToClear: counts
        // as a change only when the stored bit and the written bit are both 1.
        // Renode parity: the ICR flags have no stored state (their bits are always 0), so an ICR write
        // never counts as a change and its register change callback (`Update()`) never runs.
        let changed = difference & layout.rw != 0 || !difference & value & layout.w1c != 0;
        *stored = ((base & !layout.rw) | (value & layout.rw)) & !(value & layout.w1c);

        // Field write callbacks, in field definition order.
        match offset {
            CR1 => self.peripheral_enabled_write(value & CR1_PE != 0),
            ISR => {
                if value & ISR_TXE != 0 {
                    self.tx_data.clear();
                }
                // TXIS: only effective with NOSTRETCH.
                if self.cr1 & CR1_NOSTRETCH != 0 {
                    self.transmit_interrupt_status = value & ISR_TXIS != 0 && self.cr1 & CR1_TXIE != 0;
                }
            }
            ICR => {
                if value & ICR_ADDRCF != 0 {
                    self.transmit_interrupt_status = self.transfer_outgoing && self.tx_data.is_empty();
                    self.isr &= !ISR_ADDR;
                }
                if value & ICR_STOPCF != 0 {
                    self.isr &= !ISR_STOPF;
                }
            }
            TXDR => self.handle_transmit_data_write(value & 0xFF, ctx),
            _ => {}
        }

        // Register write callbacks.
        match offset {
            CR2 => self.control2_write(base, ctx),
            OAR1 => {
                let mode = if self.oar1 & (1 << 10) != 0 { "10-bit" } else { "7-bit" };
                let status = if self.oar1 & OAR1_OA1EN != 0 { "enabled" } else { "disabled" };
                emu_info!(ctx, "Slave address 1: 0x{:X}, mode: {}, status: {}", self.oar1 & 0x3FF, mode, status);
            }
            OAR2 => {
                let status = if self.oar2 & OAR2_OA2EN != 0 { "enabled" } else { "disabled" };
                emu_info!(
                    ctx,
                    "Slave address 2: 0x{:X}, mask: 0x{:X}, status: {}",
                    (self.oar2 >> 1) & 0x7F,
                    (self.oar2 >> 8) & 0x7,
                    status
                );
            }
            _ => {}
        }

        // Register change callback (`WithChangeCallback((_, __) => Update())`).
        if changed && matches!(offset, CR1 | CR2 | ISR | ICR) {
            self.update(ctx);
        }

        // Renode parity: bits that no field handles are compared against the *stored* value, so a tagged
        // bit written as 1 is reported on every such write (TIMINGR, CR2.NACK, ...); bits that no tag
        // covers are silent.
        let unhandled = difference & !layout.defined();
        if unhandled != 0 {
            if let Some(message) = unhandled_write_message(offset, unhandled, value, layout.tags) {
                ctx.warn_once(warn_key(3, offset, value), format_args!("{message}"));
            }
        }
    }

    // ---- state machine (the private methods of the Renode class) ----

    /// CR1.PE write callback (`PeripheralEnabledWrite`): clearing PE clears the transfer flags.
    fn peripheral_enabled_write(&mut self, enabled: bool) {
        if enabled {
            return;
        }
        self.isr &= !(ISR_STOPF | ISR_TC | ISR_TCR);
        self.transmit_interrupt_status = false;
    }

    /// CR2 register write callback: START, STOP and NBYTES-extension handling.
    fn control2_write(&mut self, old_value: u32, ctx: &mut Ctx<'_>) {
        let old_start = (old_value >> 13) & 1;
        let old_bytes_to_transfer = (old_value >> CR2_NBYTES_SHIFT) & 0xFF;

        let start = self.cr2 & CR2_START != 0;
        let stop = self.cr2 & CR2_STOP != 0;
        if start && stop {
            emu_warn!(ctx, "Setting START and STOP at the same time, ignoring the transfer");
        } else if start {
            self.start_transfer(ctx);
        } else if stop {
            self.stop_transfer(ctx);
        }

        if self.cr2 & CR2_START == 0
            && self.nbytes() > 0
            && self.master_mode
            && self.isr & ISR_TCR != 0
            && self.current_slave.is_some()
        {
            self.extend_transfer(ctx);
        }

        if old_start == 1 && old_bytes_to_transfer != self.nbytes() {
            emu_error!(ctx, "Changing NBYTES when START is set is not permitted");
        }

        self.cr2 &= !(CR2_START | CR2_STOP);
    }

    fn start_transfer(&mut self, ctx: &mut Ctx<'_>) {
        self.master_mode = true;
        self.isr &= !ISR_TC;

        self.current_slave = None;

        self.rx_data.clear();
        let sadd = self.cr2 & CR2_SADD;
        self.current_slave_address = if self.cr2 & CR2_ADD10 != 0 { sadd } else { (sadd >> 1) & 0x7F };
        let address = self.current_slave_address;
        let Some(slot) = self.find_slot(address) else {
            // Renode parity: no NACKF, no flag change; a polling driver times out.
            ctx.warn_once(warn_key(4, address, 0), format_args!("Unknown slave at address {address}."));
            return;
        };
        self.current_slave = Some(slot);

        if self.cr2 & CR2_RD_WRN != 0 {
            self.transmit_interrupt_status = false;
            let count = self.nbytes() as usize;
            let data = self.call_target(slot, ctx, |target, target_ctx| target.read(count, target_ctx));
            self.rx_data.extend(data);
        } else {
            self.transmit_interrupt_status = true;
            self.write_to_slave(ctx);
        }
        self.update(ctx);
    }

    fn stop_transfer(&mut self, ctx: &mut Ctx<'_>) {
        self.master_mode = false;
        self.isr |= ISR_STOPF;
        if let Some(slot) = self.current_slave {
            self.call_target(slot, ctx, |target, target_ctx| target.finish_transmission(target_ctx));
        }
        self.update(ctx);
    }

    fn extend_transfer(&mut self, ctx: &mut Ctx<'_>) {
        // In case of reads we can fetch data from the peripheral immediately, but in case of writes we
        // have to wait until something is written to TXDATA.
        if self.cr2 & CR2_RD_WRN != 0 {
            if let Some(slot) = self.current_slave {
                let count = self.nbytes() as usize;
                let data = self.call_target(slot, ctx, |target, target_ctx| target.read(count, target_ctx));
                self.rx_data.extend(data);
            }
        }
        self.isr &= !ISR_TCR;
        self.update(ctx);
    }

    /// RXDR value provider (`ReceiveDataRead`).
    fn receive_data_read(&mut self, ctx: &mut Ctx<'_>) -> u32 {
        ctx.set_output(DMA_RECEIVE, false);

        if let Some(value) = self.rx_data.pop_front() {
            if self.rx_data.is_empty() {
                self.set_transfer_complete_flags(ctx); // TC/TCR is set when NBYTES data have been transferred
            }
            let request = self.cr1 & CR1_RXDMAEN != 0 && !self.rx_data.is_empty();
            ctx.set_output(DMA_RECEIVE, request);
            return u32::from(value);
        }
        ctx.warn_once(warn_key(5, 0, 0), format_args!("Receive buffer underflow!"));
        0
    }

    fn handle_transmit_data_write(&mut self, value: u32, ctx: &mut Ctx<'_>) {
        if self.master_mode {
            // MasterTransmitDataWrite
            let Some(_slot) = self.current_slave else {
                let address = self.current_slave_address;
                ctx.warn_once(
                    warn_key(6, address, value),
                    format_args!("Trying to send byte {value} to an unknown slave with address {address}."),
                );
                return;
            };
            self.tx_data.push_back(value as u8);
            self.write_to_slave(ctx);
        } else {
            // SlaveTransmitDataWrite
            self.tx_data.push_back(value as u8);
        }
    }

    fn write_to_slave(&mut self, ctx: &mut Ctx<'_>) {
        if self.tx_data.len() == self.nbytes() as usize {
            let data: Vec<u8> = self.tx_data.drain(..).collect();
            if let Some(slot) = self.current_slave {
                self.call_target(slot, ctx, |target, target_ctx| target.write(&data, target_ctx));
            }
            self.set_transfer_complete_flags(ctx);
        }
    }

    fn set_transfer_complete_flags(&mut self, ctx: &mut Ctx<'_>) {
        let auto_end = self.cr2 & CR2_AUTOEND != 0;
        let reload = self.cr2 & CR2_RELOAD != 0;
        if !auto_end && !reload {
            self.isr |= ISR_TC;
        }
        if auto_end {
            // Renode would throw a NullReferenceException for a slave-mode receive without a master
            // target (the only deliberate deviation of this class); here there is nobody to notify.
            if let Some(slot) = self.current_slave {
                self.call_target(slot, ctx, |target, target_ctx| target.finish_transmission(target_ctx));
            }
            self.isr |= ISR_STOPF;
            self.master_mode = false;
        }
        if reload {
            // Renode parity: with RELOAD only TCR is raised; a pending TXIS stays set (sticky), which the
            // HAL's wait for TXIS after a RELOAD chunk relies on.
            self.isr |= ISR_TCR;
        } else {
            self.transmit_interrupt_status = false; // this is a guess based on a driver (Renode)
        }
        self.update(ctx);
    }

    /// Recomputes the event interrupt and the DMA request line.
    fn update(&mut self, ctx: &mut Ctx<'_>) {
        let value = (self.cr1 & CR1_TCIE != 0 && self.isr & (ISR_TCR | ISR_TC) != 0)
            || (self.cr1 & CR1_TXIE != 0 && self.transmit_interrupt_status)
            || (self.cr1 & CR1_RXIE != 0 && self.cr2 & CR2_RD_WRN != 0 && !self.rx_data.is_empty()) // RXNE is calculated dynamically
            || (self.cr1 & CR1_STOPIE != 0 && self.isr & ISR_STOPF != 0)
            // NACKIE: `(nackReceivedInterruptEnabled.Value && false)` in Renode ("TODO: implement NACKF")
            || (self.cr1 & CR1_ADDRIE != 0 && self.isr & ISR_ADDR != 0);
        ctx.set_output(EVENT_INTERRUPT, value);
        ctx.set_output(DMA_RECEIVE, self.cr1 & CR1_RXDMAEN != 0 && !self.rx_data.is_empty());
    }

    fn peek_register(&self, offset: u32) -> Option<u32> {
        Some(match offset {
            CR1 => self.cr1,
            CR2 => self.cr2,
            OAR1 => self.oar1,
            OAR2 => self.oar2,
            TIMINGR => self.timingr,
            ISR => self.isr_value(),
            ICR => self.icr & !ICR_LAYOUT.w1c,
            RXDR => u32::from(self.rx_data.front().copied().unwrap_or(0)),
            TXDR => self.txdr,
            _ => return None,
        })
    }
}

impl Peripheral for Stm32F7I2c {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset()`: registers to their reset values, queues cleared, interrupt lines low. As in Renode,
    /// `transmitInterruptStatus`, `currentSlave` and the DMA line are left alone. The targets are reset
    /// afterwards, in attach order (the machine resets every registered child).
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.cr1 = 0;
        self.cr2 = 0;
        self.oar1 = 0;
        self.oar2 = 0;
        self.timingr = 0;
        self.isr = ISR_TXE;
        self.icr = 0;
        self.rxdr = 0;
        self.txdr = 0;
        self.tx_data = VecDeque::new();
        self.rx_data = VecDeque::new();
        self.current_slave_address = 0;
        self.transfer_outgoing = false;
        ctx.set_output(EVENT_INTERRUPT, false);
        ctx.set_output(ERROR_INTERRUPT, false);
        self.master_mode = false;
        for slot in 0..self.targets.len() {
            self.call_target(slot, ctx, |target, target_ctx| target.reset(target_ctx));
        }
    }

    // `[AllowedTranslations(AllowedTranslation.ByteToDoubleWord)]`, `IDoubleWordPeripheral` only.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD)
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        self.read_register(offset, ctx)
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.write_register(offset, value, ctx);
    }

    /// Events scheduled by targets through [`I2cCtx`]; the controller schedules none of its own.
    fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
        let slot = (token >> 56) as usize;
        if slot < self.targets.len() {
            let user_token = token & MAX_TARGET_TOKEN;
            self.call_target(slot, ctx, |target, target_ctx| target.on_event(user_token, scheduled, target_ctx));
        } else {
            emu_error!(ctx, "event token 0x{token:X} does not belong to any target");
        }
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        self.peek_register(offset)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        let addresses: Vec<String> = self.targets.iter().map(|slot| format!("0x{:X}", slot.address)).collect();
        format!(
            "STM32F7 I2C {}: targets=[{}]; master={}; CR1=0x{:X} CR2=0x{:X} ISR=0x{:X}; txQueue={}; rxQueue={}; current={}",
            self.name,
            addresses.join(", "),
            self.master_mode,
            self.cr1,
            self.cr2,
            self.isr_value(),
            self.tx_data.len(),
            self.rx_data.len(),
            match self.current_slave {
                Some(slot) => format!("0x{:X}", self.targets[slot].address),
                None => "none".to_string(),
            }
        )
    }

    impl_peripheral_any!();
}

/// Driver-level helpers for tests of I2C targets and boards: the polling transfer flows of the STM32
/// HAL (`stm32l4xx_hal_i2c.c`: `I2C_TransferConfig`, `HAL_I2C_Master_Transmit/Receive`,
/// `HAL_I2C_Mem_Write/Read` with an 8-bit memory address) expressed as register accesses through an
/// [`emu_core::testing::Harness`]. They assert, like the HAL's wait loops, that the flags it polls are
/// already set (the Renode model completes every step synchronously) and panic with the ISR value
/// when they are not. `base` is the controller's mapped base address; `dev` is the HAL device address
/// (the 7-bit address shifted left by one).
pub mod testing {
    use super::regs::*;
    use emu_core::testing::Harness;

    /// `I2C_GENERATE_START_READ`.
    pub const GENERATE_START_READ: u32 = 0x8000_0000 | CR2_START | CR2_RD_WRN;
    /// `I2C_GENERATE_START_WRITE`.
    pub const GENERATE_START_WRITE: u32 = 0x8000_0000 | CR2_START;
    /// `I2C_GENERATE_STOP`.
    pub const GENERATE_STOP: u32 = 0x8000_0000 | CR2_STOP;
    /// `I2C_NO_STARTSTOP`.
    pub const NO_STARTSTOP: u32 = 0;
    /// `I2C_RELOAD_MODE`.
    pub const RELOAD_MODE: u32 = CR2_RELOAD;
    /// `I2C_AUTOEND_MODE`.
    pub const AUTOEND_MODE: u32 = CR2_AUTOEND;
    /// `I2C_SOFTEND_MODE`.
    pub const SOFTEND_MODE: u32 = 0;

    const CR2_HEAD10R: u32 = 1 << 12;
    const CR2_PECBYTE: u32 = 1 << 26;
    /// `MAX_NBYTE_SIZE` of the HAL.
    const MAX_NBYTE_SIZE: usize = 255;

    /// Asserts that `flag` is set in ISR (the HAL's `__HAL_I2C_GET_FLAG(...) != RESET` wait).
    pub fn assert_flag(h: &mut Harness, base: u32, flag: u32) {
        let isr = h.read32(base + ISR);
        assert!(isr & flag != 0, "ISR flag 0x{flag:X} should be set, ISR = 0x{isr:X}");
    }

    /// `I2C_TransferConfig`: read-modify-write of CR2 (`mode` is RELOAD/AUTOEND/SOFTEND, `request`
    /// one of the `GENERATE_*`/`NO_STARTSTOP` values).
    pub fn transfer_config(h: &mut Harness, base: u32, dev: u32, size: u32, mode: u32, request: u32) {
        let tmp = ((dev & CR2_SADD) | ((size << CR2_NBYTES_SHIFT) & CR2_NBYTES) | mode | request) & !0x8000_0000;
        let mask = CR2_SADD
            | CR2_NBYTES
            | CR2_RELOAD
            | CR2_AUTOEND
            | (CR2_RD_WRN & (request >> (31 - 10)))
            | CR2_START
            | CR2_STOP
            | CR2_PECBYTE;
        let cr2 = h.read32(base + CR2);
        h.write32(base + CR2, (cr2 & !mask) | tmp);
    }

    /// `I2C_RESET_CR2`.
    pub fn reset_cr2(h: &mut Harness, base: u32) {
        let cr2 = h.read32(base + CR2);
        h.write32(base + CR2, cr2 & !(CR2_SADD | CR2_HEAD10R | CR2_NBYTES | CR2_RELOAD | CR2_RD_WRN));
    }

    /// The tail of every AUTOEND transfer: wait for STOPF, `__HAL_I2C_CLEAR_FLAG(STOPF)`, `I2C_RESET_CR2`.
    pub fn finish_transfer(h: &mut Harness, base: u32) {
        assert_flag(h, base, ISR_STOPF);
        h.write32(base + ICR, ICR_STOPCF);
        reset_cr2(h, base);
    }

    /// `HAL_I2C_Master_Transmit` (polling). An empty `data` is the zero-length address probe.
    pub fn master_transmit(h: &mut Harness, base: u32, dev: u32, data: &[u8]) {
        let mut count = data.len();
        let mut size;
        if count > MAX_NBYTE_SIZE {
            size = MAX_NBYTE_SIZE;
            transfer_config(h, base, dev, size as u32, RELOAD_MODE, GENERATE_START_WRITE);
        } else {
            size = count;
            transfer_config(h, base, dev, size as u32, AUTOEND_MODE, GENERATE_START_WRITE);
        }
        let mut index = 0;
        while count > 0 {
            assert_flag(h, base, ISR_TXIS);
            h.write32(base + TXDR, u32::from(data[index]));
            index += 1;
            count -= 1;
            size -= 1;
            if count != 0 && size == 0 {
                assert_flag(h, base, ISR_TCR);
                size = count.min(MAX_NBYTE_SIZE);
                let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
                transfer_config(h, base, dev, size as u32, mode, NO_STARTSTOP);
            }
        }
        finish_transfer(h, base);
    }

    /// `HAL_I2C_Master_Receive` (polling).
    pub fn master_receive(h: &mut Harness, base: u32, dev: u32, len: usize) -> Vec<u8> {
        let mut count = len;
        let mut size;
        if count > MAX_NBYTE_SIZE {
            size = MAX_NBYTE_SIZE;
            transfer_config(h, base, dev, size as u32, RELOAD_MODE, GENERATE_START_READ);
        } else {
            size = count;
            transfer_config(h, base, dev, size as u32, AUTOEND_MODE, GENERATE_START_READ);
        }
        let mut out = Vec::with_capacity(len);
        while count > 0 {
            assert_flag(h, base, ISR_RXNE);
            out.push(h.read32(base + RXDR) as u8);
            count -= 1;
            size -= 1;
            if count != 0 && size == 0 {
                assert_flag(h, base, ISR_TCR);
                size = count.min(MAX_NBYTE_SIZE);
                let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
                transfer_config(h, base, dev, size as u32, mode, NO_STARTSTOP);
            }
        }
        finish_transfer(h, base);
        out
    }

    /// `HAL_I2C_Mem_Write` (polling, `I2C_MEMADD_SIZE_8BIT`, `data` not empty): the memory-address byte
    /// goes out in a RELOAD chunk (`I2C_RequestMemoryWrite`), the data in AUTOEND/RELOAD chunks of at
    /// most 255 bytes.
    pub fn mem_write(h: &mut Harness, base: u32, dev: u32, mem: u32, data: &[u8]) {
        assert!(!data.is_empty(), "HAL_I2C_Mem_Write rejects a zero size");
        transfer_config(h, base, dev, 1, RELOAD_MODE, GENERATE_START_WRITE);
        assert_flag(h, base, ISR_TXIS);
        h.write32(base + TXDR, mem);
        assert_flag(h, base, ISR_TCR);

        let mut count = data.len();
        let mut size = count.min(MAX_NBYTE_SIZE);
        let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
        transfer_config(h, base, dev, size as u32, mode, NO_STARTSTOP);
        let mut index = 0;
        while count > 0 {
            assert_flag(h, base, ISR_TXIS);
            h.write32(base + TXDR, u32::from(data[index]));
            index += 1;
            count -= 1;
            size -= 1;
            if count != 0 && size == 0 {
                assert_flag(h, base, ISR_TCR);
                size = count.min(MAX_NBYTE_SIZE);
                let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
                transfer_config(h, base, dev, size as u32, mode, NO_STARTSTOP);
            }
        }
        finish_transfer(h, base);
    }

    /// `HAL_I2C_Mem_Read` (polling, `I2C_MEMADD_SIZE_8BIT`, `len` > 0): a SOFTEND write of the memory
    /// address (`I2C_RequestMemoryRead`, waits for TC), then a repeated-START read in AUTOEND/RELOAD pieces.
    pub fn mem_read(h: &mut Harness, base: u32, dev: u32, mem: u32, len: usize) -> Vec<u8> {
        assert!(len > 0, "HAL_I2C_Mem_Read rejects a zero size");
        transfer_config(h, base, dev, 1, SOFTEND_MODE, GENERATE_START_WRITE);
        assert_flag(h, base, ISR_TXIS);
        h.write32(base + TXDR, mem);
        assert_flag(h, base, ISR_TC);

        let mut count = len;
        let mut size = count.min(MAX_NBYTE_SIZE);
        let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
        transfer_config(h, base, dev, size as u32, mode, GENERATE_START_READ);
        let mut out = Vec::with_capacity(len);
        while count > 0 {
            assert_flag(h, base, ISR_RXNE);
            out.push(h.read32(base + RXDR) as u8);
            count -= 1;
            size -= 1;
            if count != 0 && size == 0 {
                assert_flag(h, base, ISR_TCR);
                size = count.min(MAX_NBYTE_SIZE);
                let mode = if count > MAX_NBYTE_SIZE { RELOAD_MODE } else { AUTOEND_MODE };
                transfer_config(h, base, dev, size as u32, mode, NO_STARTSTOP);
            }
        }
        finish_transfer(h, base);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{
        self, AUTOEND_MODE, GENERATE_START_READ, GENERATE_START_WRITE, NO_STARTSTOP, RELOAD_MODE, SOFTEND_MODE,
    };
    use super::*;
    use emu_core::testing::{Harness, IrqChange};
    use emu_core::{from_micros, PeriphId};

    const BASE: u32 = 0x4000_5400;
    const EVENT_NVIC: u32 = 31;
    const ERROR_NVIC: u32 = 32;
    /// HAL-style device address: the 7-bit address shifted left by one.
    const DEV50: u32 = 0x50 << 1;

    /// Scriptable target: records every call, returns an incrementing byte stream for reads and can
    /// schedule an event when it is written `0xEE`.
    #[derive(Default)]
    struct Mock {
        name: String,
        writes: Vec<Vec<u8>>,
        read_requests: Vec<usize>,
        next: u8,
        finishes: usize,
        resets: usize,
        events: Vec<(u64, Time)>,
    }

    impl Mock {
        fn named(name: &str) -> Self {
            Self { name: name.to_string(), ..Default::default() }
        }
    }

    impl I2cTarget for Mock {
        fn name(&self) -> &str {
            &self.name
        }

        fn write(&mut self, data: &[u8], ctx: &mut I2cCtx<'_, '_>) {
            self.writes.push(data.to_vec());
            if data.first() == Some(&0xEE) {
                ctx.schedule_in(from_micros(10), 7);
            }
            if data.first() == Some(&0xEF) {
                ctx.schedule_action(from_micros(20), 9);
            }
        }

        fn read(&mut self, count: usize, _ctx: &mut I2cCtx<'_, '_>) -> Vec<u8> {
            self.read_requests.push(count);
            (0..count)
                .map(|_| {
                    let value = self.next;
                    self.next = self.next.wrapping_add(1);
                    value
                })
                .collect()
        }

        fn finish_transmission(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
            self.finishes += 1;
        }

        fn reset(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
            self.resets += 1;
        }

        fn on_event(&mut self, token: u64, scheduled: Time, _ctx: &mut I2cCtx<'_, '_>) {
            self.events.push((token, scheduled));
        }

        impl_i2c_target_any!();
    }

    /// Controller `i2c1` with both interrupt lines wired like `main.repl` (nvic 31/32), no targets.
    fn rig() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, I2C_SIZE, Stm32F7I2c::new("i2c1"));
        h.connect_irq(id, EVENT_INTERRUPT, EVENT_NVIC);
        h.connect_irq(id, ERROR_INTERRUPT, ERROR_NVIC);
        h.clear_irq_changes();
        (h, id)
    }

    /// `rig()` with a [`Mock`] at each of the given 7-bit addresses.
    fn rig_with_mocks(addresses: &[u32]) -> (Harness, PeriphId) {
        let (mut h, id) = rig();
        for &address in addresses {
            h.get_mut::<Stm32F7I2c>(id).attach(address, Box::new(Mock::named(&format!("mock{address:x}")))).unwrap();
        }
        (h, id)
    }

    fn mock(h: &Harness, id: PeriphId, address: u32) -> &Mock {
        h.get::<Stm32F7I2c>(id).target::<Mock>(address).expect("mock target")
    }

    fn info_messages(h: &Harness) -> Vec<String> {
        h.core().log.entries().filter(|e| e.level == LogLevel::Info).map(|e| e.message.clone()).collect()
    }

    fn isr(h: &mut Harness) -> u32 {
        h.read32(BASE + ISR)
    }

    fn assert_flag(h: &mut Harness, flag: u32) {
        let value = isr(h);
        assert!(value & flag != 0, "ISR flag 0x{flag:X} should be set, ISR = 0x{value:X}");
    }

    fn assert_no_flag(h: &mut Harness, flag: u32) {
        let value = isr(h);
        assert!(value & flag == 0, "ISR flag 0x{flag:X} should be clear, ISR = 0x{value:X}");
    }

    fn transfer_config(h: &mut Harness, dev: u32, size: u32, mode: u32, request: u32) {
        testing::transfer_config(h, BASE, dev, size, mode, request);
    }

    fn reset_cr2(h: &mut Harness) {
        testing::reset_cr2(h, BASE);
    }

    fn hal_master_transmit(h: &mut Harness, dev: u32, data: &[u8]) {
        testing::master_transmit(h, BASE, dev, data);
    }

    fn hal_master_receive(h: &mut Harness, dev: u32, len: usize) -> Vec<u8> {
        testing::master_receive(h, BASE, dev, len)
    }

    fn hal_mem_write(h: &mut Harness, dev: u32, mem: u32, data: &[u8]) {
        testing::mem_write(h, BASE, dev, mem, data);
    }

    fn hal_mem_read(h: &mut Harness, dev: u32, mem: u32, len: usize) -> Vec<u8> {
        testing::mem_read(h, BASE, dev, mem, len)
    }

    #[test]
    fn reset_values_and_field_masks() {
        let (mut h, id) = rig();
        for offset in [CR1, CR2, OAR1, OAR2, TIMINGR, ICR, TXDR] {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:X}");
        }
        assert_eq!(h.read32(BASE + ISR), 1, "ISR resets to TXE");
        assert!(h.get::<Stm32F7I2c>(id).target_addresses().is_empty());
        assert!(!h.irq_level(EVENT_NVIC) && !h.irq_level(ERROR_NVIC));

        // Only field bits are stored; tags and reserved bits read back 0.
        h.write32(BASE + CR1, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE + CR1), 0x0002_80FF);
        h.write32(BASE + CR1, 0);
        h.write32(BASE + CR2, 0x03FF_0FFF); // no START/STOP, so no transfer logic
        assert_eq!(h.read32(BASE + CR2), 0x03FF_0FFF);
        h.write32(BASE + CR2, 0xFFFF_FFFF & !(CR2_START | CR2_STOP));
        assert_eq!(h.read32(BASE + CR2), 0x03FF_0FFF, "HEAD10R, NACK, PECBYTE and reserved bits are not stored");
        h.write32(BASE + OAR1, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE + OAR1), 0x87FF);
        h.write32(BASE + OAR2, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE + OAR2), 0x87FE);
        h.write32(BASE + TIMINGR, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE + TIMINGR), 0, "TIMINGR has only tags in the Renode class");
        h.write32(BASE + TXDR, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE + TXDR), 0xFF);
        assert!(!h.irq_level(EVENT_NVIC), "no enable bit is set");
    }

    #[test]
    fn warnings_match_the_renode_reference_log() {
        // Main board boot of a Renode 1.17.0 dual-wake run recorded on 2026-10-08 (its `renode.log`).
        let (mut h, _id) = rig();
        h.write32(BASE + TIMINGR, 0x1090_9CEC);
        h.write32(BASE + OAR1, 0);
        h.write32(BASE + OAR1, OAR1_OA1EN);
        h.write32(BASE + CR2, 0x0200_8000);
        h.write32(BASE + OAR2, 0);
        h.write32(BASE + OAR2, 0);
        assert_eq!(
            h.warnings(),
            [
                "Unhandled write to offset 0x10. Unhandled bits: [2-3, 5-7, 10-12, 15, 20, 23, 28] when writing value 0x10909CEC. Tags: SCLL (0xEC), SCLH (0x9C), SCLDEL (0x9), PRESC (0x1).",
                "Unhandled write to offset 0x4. Unhandled bits: [15] when writing value 0x2008000. Tags: NACK (0x1).",
            ]
        );
        assert_eq!(
            info_messages(&h),
            [
                "Slave address 1: 0x0, mode: 7-bit, status: disabled",
                "Slave address 1: 0x0, mode: 7-bit, status: enabled",
                "Slave address 2: 0x0, mask: 0x0, status: disabled",
                "Slave address 2: 0x0, mask: 0x0, status: disabled",
            ]
        );
        // Handset board boot.
        let (mut h, _id) = rig();
        h.write32(BASE + TIMINGR, 0x0070_2991);
        assert_eq!(
            h.warnings(),
            ["Unhandled write to offset 0x10. Unhandled bits: [0, 4, 7-8, 11, 13, 20-22] when writing value 0x702991. Tags: SCLL (0x91), SCLH (0x29), SCLDEL (0x7)."]
        );
        // The same value written again changes nothing and is reported once.
        h.write32(BASE + TIMINGR, 0x0070_2991);
        assert_eq!(h.warnings().len(), 1);
    }

    #[test]
    fn tags_and_undefined_offsets() {
        let (mut h, _id) = rig();
        h.write32(BASE + CR1, 0x4001); // PE + TXDMAEN (tag)
        assert_eq!(h.read32(BASE + CR1), 1);
        assert_eq!(
            h.warnings(),
            ["Unhandled write to offset 0x0. Unhandled bits: [14] when writing value 0x4001. Tags: TXDMAEN (0x1)."]
        );
        // Bits no field and no tag covers produce no warning (RXDR bit 8, ISR bit 14 is RESERVED though).
        h.write32(BASE + RXDR, 0x100);
        assert_eq!(h.warnings().len(), 1);
        h.write32(BASE + RXDR, 0x200);
        assert_eq!(h.warnings().len(), 2);
        assert_eq!(h.warnings()[1], "Unhandled write to offset 0x24. Unhandled bits: [9] when writing value 0x200. Tags: RESERVED (0x1).");

        // TIMEOUTR and PECR exist in the Renode enum but have no register.
        assert_eq!(h.read32(BASE + TIMEOUTR), 0);
        h.write32(BASE + TIMEOUTR, 0x1234);
        assert_eq!(h.read32(BASE + PECR), 0);
        assert_eq!(h.read32(BASE + 0x2C), 0);
        let warnings = h.warnings();
        assert_eq!(warnings[2], "Unhandled read from offset 0x14.");
        assert_eq!(warnings[3], "Unhandled write to offset 0x14, value 0x1234.");
        assert_eq!(warnings[4], "Unhandled read from offset 0x20.");
        assert_eq!(warnings[5], "Unhandled read from offset 0x2C.");
        // Repeats are logged once.
        h.read32(BASE + TIMEOUTR);
        assert_eq!(h.warnings().len(), 6);
    }

    #[test]
    fn access_widths_follow_the_renode_policy() {
        let (mut h, id) = rig();
        // Byte accesses are served through the 32-bit register (read-modify-write for writes).
        h.write8(BASE + CR2 + 2, 3); // NBYTES = 3
        assert_eq!(h.read32(BASE + CR2), 3 << 16);
        assert_eq!(h.read8(BASE + ISR), 1);
        assert_eq!(h.read8(BASE + CR2 + 2), 3);
        // Halfword accesses are "not supported": the controller is not touched.
        assert_eq!(h.read16(BASE + ISR), 0);
        h.write16(BASE + CR1, 1);
        assert_eq!(h.read32(BASE + CR1), 0);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("Attempted Word read isn't supported by the peripheral. Offset 0x18."), "{warnings:?}");
        assert!(warnings[1].contains("Attempted Word write isn't supported by the peripheral. Offset 0x0, value 0x1."), "{warnings:?}");
        assert_eq!(h.get::<Stm32F7I2c>(id).access_policy(), AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD));
    }

    #[test]
    fn address_only_probe_is_acknowledged() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        // HAL_I2C_Master_Transmit(hi2c, 0xA0, NULL, 0): START + AUTOEND with NBYTES = 0.
        hal_master_transmit(&mut h, DEV50, &[]);
        let target = mock(&h, id, 0x50);
        assert_eq!(target.writes, [Vec::<u8>::new()], "the target sees one zero-length chunk");
        assert_eq!(target.finishes, 1);
        assert_eq!(isr(&mut h), ISR_TXE, "STOPF was cleared by the HAL, nothing else is pending");
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn memory_write_and_read_use_the_hal_chunk_sequence() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        hal_mem_write(&mut h, DEV50, 0x10, &[1, 2, 3]);
        {
            let target = mock(&h, id, 0x50);
            assert_eq!(target.writes, [vec![0x10], vec![1, 2, 3]], "address chunk first, then the data chunk");
            assert_eq!(target.finishes, 1, "FinishTransmission only at AUTOEND completion");
        }
        let data = hal_mem_read(&mut h, DEV50, 0x10, 4);
        assert_eq!(data, [0, 1, 2, 3]);
        let target = mock(&h, id, 0x50);
        assert_eq!(target.writes, [vec![0x10], vec![1, 2, 3], vec![0x10]]);
        assert_eq!(target.read_requests, [4], "a read transfer fetches all NBYTES at START");
        assert_eq!(target.finishes, 2);
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
        assert_eq!(isr(&mut h), ISR_TXE);
    }

    #[test]
    fn flags_follow_the_renode_transfer_state_machine() {
        let (mut h, _id) = rig_with_mocks(&[0x50]);
        // Memory write, step by step.
        transfer_config(&mut h, DEV50, 1, RELOAD_MODE, GENERATE_START_WRITE);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TXIS, "START of a write: TXIS");
        assert_eq!(h.read32(BASE + CR2) & (CR2_START | CR2_STOP), 0, "START is self-clearing");
        h.write32(BASE + TXDR, 0x20);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TXIS | ISR_TCR, "RELOAD: TCR set and TXIS stays set (Renode)");
        transfer_config(&mut h, DEV50, 2, AUTOEND_MODE, NO_STARTSTOP);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TXIS, "writing CR2 again (extend) clears TCR");
        h.write32(BASE + TXDR, 0xA1);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TXIS, "one byte of two");
        h.write32(BASE + TXDR, 0xA2);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_STOPF, "AUTOEND: STOPF, TXIS drops, no TC");
        h.write32(BASE + ICR, ICR_STOPCF);
        assert_eq!(isr(&mut h), ISR_TXE);

        // SOFTEND write: TC instead of STOPF.
        transfer_config(&mut h, DEV50, 1, SOFTEND_MODE, GENERATE_START_WRITE);
        h.write32(BASE + TXDR, 0x20);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TC);
        // Repeated START (read) clears TC and queues the data: RXNE.
        transfer_config(&mut h, DEV50, 2, AUTOEND_MODE, GENERATE_START_READ);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_RXNE);
        assert_eq!(h.read32(BASE + RXDR), 0);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_RXNE, "one byte left");
        assert_eq!(h.read32(BASE + RXDR), 1);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_STOPF, "last byte read with AUTOEND: STOPF");
        // ADDR, NACKF, BUSY and DIR are never set in master mode.
        assert_eq!(isr(&mut h) & (ISR_ADDR | ISR_NACKF | ISR_BUSY | ISR_DIR), 0);
    }

    #[test]
    fn reload_chain_writes_300_bytes_in_two_chunks() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        let data: Vec<u8> = (0..300).map(|i| (i * 7) as u8).collect();
        hal_mem_write(&mut h, DEV50, 0x00, &data);
        let target = mock(&h, id, 0x50);
        assert_eq!(target.writes.len(), 3);
        assert_eq!(target.writes[0], [0x00]);
        assert_eq!(target.writes[1], data[..255]);
        assert_eq!(target.writes[2], data[255..]);
        assert_eq!(target.finishes, 1);
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    }

    #[test]
    fn reload_chain_reads_300_bytes_in_two_pieces() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        let data = hal_mem_read(&mut h, DEV50, 0x00, 300);
        let expected: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
        assert_eq!(data, expected);
        let target = mock(&h, id, 0x50);
        assert_eq!(target.read_requests, [255, 45], "START reads 255, the extend after TCR reads the rest");
        assert_eq!(target.writes, [vec![0x00]]);
        assert_eq!(target.finishes, 1);
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    }

    #[test]
    fn explicit_stop_ends_a_softend_transfer() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        transfer_config(&mut h, DEV50, 2, SOFTEND_MODE, GENERATE_START_WRITE);
        h.write32(BASE + TXDR, 0xAA);
        h.write32(BASE + TXDR, 0xBB);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TC);
        assert_eq!(mock(&h, id, 0x50).writes, [vec![0xAA, 0xBB]]);
        assert_eq!(mock(&h, id, 0x50).finishes, 0);
        // CR2.STOP: STOPF, FinishTransmission; a later TXDR write is no longer a master write.
        let cr2 = h.read32(BASE + CR2);
        h.write32(BASE + CR2, cr2 | CR2_STOP);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TC | ISR_STOPF, "TC is only cleared by a new START or PE = 0");
        assert_eq!(mock(&h, id, 0x50).finishes, 1);
        h.write32(BASE + TXDR, 0xCC);
        assert_eq!(mock(&h, id, 0x50).writes.len(), 1);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn start_with_stop_is_ignored_with_a_warning() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        h.write32(BASE + CR2, DEV50 | (1 << 16) | CR2_START | CR2_STOP);
        assert_eq!(h.warnings(), ["Setting START and STOP at the same time, ignoring the transfer"]);
        assert_eq!(isr(&mut h), ISR_TXE);
        assert!(mock(&h, id, 0x50).writes.is_empty());
        assert_eq!(h.read32(BASE + CR2), DEV50 | (1 << 16), "START and STOP are cleared again");
    }

    #[test]
    fn unknown_target_only_warns_like_renode() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        h.write32(BASE + CR1, 0x7F); // every interrupt enable
        h.clear_irq_changes();
        // START write to 0x58 (nobody there).
        h.write32(BASE + CR2, (0x58 << 1) | (1 << 16) | CR2_AUTOEND | CR2_START);
        assert_eq!(h.warnings(), ["Unknown slave at address 88."]);
        assert_eq!(isr(&mut h), ISR_TXE, "no TXIS, STOPF or NACKF: Renode does not implement NACKF");
        assert!(!h.irq_level(EVENT_NVIC) && !h.irq_level(ERROR_NVIC));
        h.write32(BASE + TXDR, 0x12);
        assert_eq!(h.warnings()[1], "Trying to send byte 18 to an unknown slave with address 88.");
        assert!(mock(&h, id, 0x50).writes.is_empty());
        // Same for a read: nothing to receive.
        h.write32(BASE + CR2, (0x58 << 1) | (1 << 16) | CR2_AUTOEND | CR2_RD_WRN | CR2_START);
        assert_eq!(isr(&mut h), ISR_TXE);
        assert_eq!(h.read32(BASE + RXDR), 0);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(warnings[2], "Receive buffer underflow!");
        // The failed START also dropped the previous slave: a following CR2 write does not extend.
        assert!(h.irq_changes().is_empty());
        // A known address still works afterwards.
        hal_master_transmit(&mut h, DEV50, &[9]);
        assert_eq!(mock(&h, id, 0x50).writes, [vec![9]]);
    }

    #[test]
    fn event_interrupt_follows_the_enable_bits() {
        let (mut h, _id) = rig_with_mocks(&[0x50]);
        // TXIE: high while TXIS is pending.
        h.write32(BASE + CR1, CR1_PE | CR1_TXIE);
        assert!(!h.irq_level(EVENT_NVIC));
        transfer_config(&mut h, DEV50, 1, SOFTEND_MODE, GENERATE_START_WRITE);
        assert!(h.irq_level(EVENT_NVIC), "TXIS pending");
        h.write32(BASE + TXDR, 0x20);
        assert!(!h.irq_level(EVENT_NVIC), "TXIS dropped after the last byte");
        // TCIE: TC is pending after a SOFTEND write.
        h.write32(BASE + CR1, CR1_PE | CR1_TXIE | CR1_TCIE);
        assert!(h.irq_level(EVENT_NVIC), "TC pending");
        // RXIE: RXNE while a read is in progress; it drops with the last byte read.
        h.write32(BASE + CR1, CR1_PE | CR1_RXIE);
        assert!(!h.irq_level(EVENT_NVIC));
        transfer_config(&mut h, DEV50, 2, AUTOEND_MODE, GENERATE_START_READ);
        assert!(h.irq_level(EVENT_NVIC), "receive data pending");
        h.read32(BASE + RXDR);
        assert!(h.irq_level(EVENT_NVIC), "the line is recomputed only by register changes and transfer events");
        h.read32(BASE + RXDR);
        assert!(!h.irq_level(EVENT_NVIC), "queue empty");
        // The error interrupt is never raised by the Renode model.
        assert!(!h.irq_level(ERROR_NVIC));
        let events: Vec<IrqChange> = h.irq_changes().iter().filter(|c| c.irq == ERROR_NVIC).copied().collect();
        assert!(events.is_empty());
    }

    #[test]
    fn clearing_stopf_through_icr_does_not_recompute_the_line() {
        // Renode quirk: ICR fields are write-one-to-clear without stored state, so a write never counts
        // as a register change and the register's change callback (Update) does not run.
        let (mut h, _id) = rig_with_mocks(&[0x50]);
        h.write32(BASE + CR1, CR1_PE | CR1_STOPIE);
        transfer_config(&mut h, DEV50, 0, AUTOEND_MODE, GENERATE_START_WRITE);
        assert!(h.irq_level(EVENT_NVIC), "STOPF with STOPIE");
        h.write32(BASE + ICR, ICR_STOPCF);
        assert_no_flag(&mut h, ISR_STOPF);
        assert!(h.irq_level(EVENT_NVIC), "still high: nothing recomputed the line");
        h.write32(BASE + CR1, CR1_PE | CR1_STOPIE);
        assert!(h.irq_level(EVENT_NVIC), "an unchanged CR1 write does not recompute either");
        reset_cr2(&mut h); // the HAL's next register write that changes a field
        assert!(!h.irq_level(EVENT_NVIC));
    }

    #[test]
    fn clearing_pe_resets_the_transfer_flags() {
        let (mut h, _id) = rig_with_mocks(&[0x50]);
        h.write32(BASE + CR1, CR1_PE);
        transfer_config(&mut h, DEV50, 1, RELOAD_MODE, GENERATE_START_WRITE);
        h.write32(BASE + TXDR, 0x20);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TXIS | ISR_TCR);
        h.write32(BASE + CR1, 0);
        assert_eq!(isr(&mut h), ISR_TXE, "TCR and TXIS cleared by PE = 0");
        // STOPF and TC too.
        h.write32(BASE + CR1, CR1_PE);
        transfer_config(&mut h, DEV50, 0, AUTOEND_MODE, GENERATE_START_WRITE);
        assert_eq!(isr(&mut h), ISR_TXE | ISR_STOPF);
        h.write32(BASE + CR1, 0);
        assert_eq!(isr(&mut h), ISR_TXE);
    }

    #[test]
    fn receive_underflow_reads_zero_with_a_warning() {
        let (mut h, _id) = rig();
        assert_eq!(h.read32(BASE + RXDR), 0);
        assert_eq!(h.read8(BASE + RXDR), 0);
        assert_eq!(h.warnings(), ["Receive buffer underflow!"]);
    }

    #[test]
    fn dma_receive_line_follows_the_receive_queue() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        let probe = h.probe(id, DMA_RECEIVE);
        h.write32(BASE + CR1, CR1_PE | CR1_RXDMAEN);
        transfer_config(&mut h, DEV50, 3, AUTOEND_MODE, GENERATE_START_READ);
        assert!(h.output(id, DMA_RECEIVE), "request while data is pending");
        h.read32(BASE + RXDR);
        h.read32(BASE + RXDR);
        assert!(h.output(id, DMA_RECEIVE), "one byte left");
        h.read32(BASE + RXDR);
        assert!(!h.output(id, DMA_RECEIVE), "queue empty");
        let levels: Vec<bool> = h.probe_changes(probe).into_iter().map(|(_, level)| level).collect();
        // The line pulses low/high around each read in Renode (Unset, read, Set); only changes are delivered.
        assert_eq!(levels.first(), Some(&true));
        assert_eq!(levels.last(), Some(&false));
    }

    #[test]
    fn ten_bit_addressing_uses_the_full_address_field() {
        let (mut h, id) = rig();
        h.get_mut::<Stm32F7I2c>(id).attach(0x123, Box::new(Mock::named("ten"))).unwrap();
        // ADD10: SADD is the address itself (not shifted).
        h.write32(BASE + CR2, 0x123 | CR2_ADD10 | (1 << 16) | CR2_AUTOEND | CR2_START);
        h.write32(BASE + TXDR, 0x5A);
        assert_eq!(mock(&h, id, 0x123).writes, [vec![0x5A]]);
        // The same SADD without ADD10 is 7-bit address 0x11 (0x123 >> 1 & 0x7F): nobody there.
        h.write32(BASE + ICR, ICR_STOPCF);
        h.write32(BASE + CR2, 0x123 | (1 << 16) | CR2_AUTOEND | CR2_START);
        assert_eq!(h.warnings(), ["Unknown slave at address 17."]);
    }

    #[test]
    fn attach_rejects_duplicates_and_exposes_targets() {
        let (mut h, id) = rig();
        let i2c = h.get_mut::<Stm32F7I2c>(id);
        i2c.attach(0x50, Box::new(Mock::named("a"))).unwrap();
        i2c.attach(0x76, Box::new(Mock::named("b"))).unwrap();
        assert_eq!(i2c.attach(0x50, Box::new(Mock::named("c"))), Err(I2cError::AddressInUse(0x50)));
        assert_eq!(i2c.target_addresses(), [0x50, 0x76]);
        assert_eq!(i2c.target_at(0x76).unwrap().name(), "b");
        assert!(i2c.target_at(0x77).is_none());
        assert!(i2c.target::<Mock>(0x50).is_some());
        i2c.target_mut::<Mock>(0x50).unwrap().next = 42;
        assert_eq!(i2c.target::<Mock>(0x50).unwrap().next, 42);
        let summaries = i2c.target_summaries();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[1].0, 0x76);
        assert!(I2cError::AddressInUse(0x50).to_string().contains("already in use"));
        let summary = h.core().summaries().into_iter().find(|(name, _)| name == "i2c1").unwrap().1;
        assert!(summary.contains("targets=[0x50, 0x76]"), "{summary}");
    }

    #[test]
    fn target_events_are_routed_back_to_the_scheduling_target() {
        let (mut h, id) = rig_with_mocks(&[0x50, 0x51]);
        // Writing 0xEE to the second target makes it schedule an event 10 us later.
        hal_master_transmit(&mut h, 0x51 << 1, &[0xEE]);
        assert_eq!(h.next_event_time(), Some(from_micros(10)));
        let pending = h.pending_events();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, id);
        assert_eq!(pending[0].2, (1u64 << 56) | 7, "slot 1 in the top byte, user token in the rest");
        h.advance_to(from_micros(10));
        assert_eq!(mock(&h, id, 0x51).events, [(7, from_micros(10))]);
        assert!(mock(&h, id, 0x50).events.is_empty());
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn schedule_action_is_routed_with_the_scheduling_time() {
        // Renode `machine.ScheduleAction` as the MS5837 uses it: the callback runs after the delay and
        // receives the time the action was scheduled at.
        let (mut h, id) = rig_with_mocks(&[0x50, 0x51]);
        h.advance_to(from_micros(5));
        hal_master_transmit(&mut h, 0x51 << 1, &[0xEF]);
        assert!(h.take_stop_request(), "ScheduleAction asks the CPU to return");
        assert_eq!(h.next_event_time(), Some(from_micros(25)));
        h.advance_to(from_micros(24));
        assert!(mock(&h, id, 0x51).events.is_empty());
        h.advance_to(from_micros(25));
        assert_eq!(mock(&h, id, 0x51).events, [(9, from_micros(5))], "origin = scheduling time");
        assert!(mock(&h, id, 0x50).events.is_empty());
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn controller_as_slave_receives_and_transmits() {
        let (mut h, id) = rig();
        // Another master writes to the controller: bytes queue for RXDR, DIR = 0.
        h.with::<Stm32F7I2c, _>(id, |i2c, _ctx| i2c.slave_write(&[1, 2, 3]));
        assert!(h.get::<Stm32F7I2c>(id).rx_not_empty());
        assert_eq!(isr(&mut h), ISR_TXE | ISR_RXNE);
        assert_eq!(h.read32(BASE + RXDR), 1);
        assert_eq!(h.read32(BASE + RXDR), 2);
        assert_eq!(h.read32(BASE + RXDR), 3);
        // Draining the queue sets TC (no RELOAD/AUTOEND).
        assert_eq!(isr(&mut h), ISR_TXE | ISR_TC);

        // The master reads from the controller: nothing queued yet, software fills TXDR meanwhile.
        let early = h.with::<Stm32F7I2c, _>(id, |i2c, ctx| i2c.slave_read(2, ctx));
        assert!(early.is_empty());
        assert_eq!(isr(&mut h) & (ISR_ADDR | ISR_DIR), ISR_ADDR | ISR_DIR, "address matched, transmitter mode");
        h.write32(BASE + TXDR, 0x11);
        h.write32(BASE + TXDR, 0x22);
        let data = h.with::<Stm32F7I2c, _>(id, |i2c, ctx| i2c.slave_read(2, ctx));
        assert_eq!(data, [0x11, 0x22]);
        assert_eq!(isr(&mut h) & (ISR_ADDR | ISR_STOPF), ISR_STOPF, "STOP condition detected, ADDR cleared");
        // ISR.TXE written with 1 flushes the transmit queue.
        h.write32(BASE + TXDR, 0x33);
        h.write32(BASE + ISR, ISR_TXE);
        let flushed = h.with::<Stm32F7I2c, _>(id, |i2c, ctx| i2c.slave_read(1, ctx));
        assert!(flushed.is_empty());
        assert!(!h.get::<Stm32F7I2c>(id).own_address1_enabled());
        h.write32(BASE + OAR1, OAR1_OA1EN | 0x30);
        assert!(h.get::<Stm32F7I2c>(id).own_address1_enabled());
    }

    #[test]
    fn nostretch_allows_software_to_set_txis() {
        let (mut h, _id) = rig();
        h.write32(BASE + CR1, CR1_PE | CR1_TXIE);
        h.write32(BASE + ISR, ISR_TXE | ISR_TXIS);
        assert_no_flag(&mut h, ISR_TXIS); // without NOSTRETCH the write is ignored
        h.write32(BASE + CR1, CR1_PE | CR1_TXIE | CR1_NOSTRETCH);
        h.write32(BASE + ISR, ISR_TXE | ISR_TXIS);
        assert_flag(&mut h, ISR_TXIS);
        assert!(h.irq_level(EVENT_NVIC), "TXIE with TXIS");
        // ADDRCF recomputes TXIS from the transfer direction and the transmit queue.
        h.write32(BASE + ICR, ICR_ADDRCF);
        assert_no_flag(&mut h, ISR_TXIS);
    }

    #[test]
    fn peek_matches_read_without_side_effects() {
        let (mut h, id) = rig_with_mocks(&[0x50]);
        transfer_config(&mut h, DEV50, 2, AUTOEND_MODE, GENERATE_START_READ);
        let peeked = h.peek(BASE + ISR, Width::Word);
        assert_eq!(peeked, Some(ISR_TXE | ISR_RXNE));
        assert_eq!(h.peek(BASE + RXDR, Width::Word), Some(0));
        assert_eq!(h.peek(BASE + RXDR, Width::Word), Some(0), "peek does not pop the receive queue");
        assert_eq!(h.get::<Stm32F7I2c>(id).rx_data.len(), 2);
        assert_eq!(h.read32(BASE + RXDR), 0);
        assert_eq!(h.peek(BASE + RXDR, Width::Word), Some(1));
        assert_eq!(h.peek(BASE + CR2, Width::Word), Some(h.read32(BASE + CR2)));
        assert_eq!(h.peek(BASE + TIMEOUTR, Width::Word), None);
        assert_eq!(h.peek(BASE + ISR, Width::Byte), Some(ISR_TXE | ISR_RXNE), "byte peek goes through the word register");
    }

    #[test]
    fn master_transmit_then_receive_like_a_sensor_command() {
        // MS5837 style: HAL_I2C_Master_Transmit(command) then HAL_I2C_Master_Receive(n).
        let (mut h, id) = rig_with_mocks(&[0x76]);
        let dev = 0x76 << 1;
        hal_master_transmit(&mut h, dev, &[0xA2]);
        let data = hal_master_receive(&mut h, dev, 2);
        assert_eq!(data, [0, 1]);
        let target = mock(&h, id, 0x76);
        assert_eq!(target.writes, [vec![0xA2]]);
        assert_eq!(target.read_requests, [2]);
        assert_eq!(target.finishes, 2, "each AUTOEND transfer ends with FinishTransmission");

        // A plain 300-byte transmit is a RELOAD chain of 255 + 45 bytes without a memory address.
        let payload: Vec<u8> = (0..300).map(|i| (i * 3) as u8).collect();
        hal_master_transmit(&mut h, dev, &payload);
        let target = mock(&h, id, 0x76);
        assert_eq!(target.writes.len(), 3);
        assert_eq!(target.writes[1], payload[..255]);
        assert_eq!(target.writes[2], payload[255..]);
        assert_eq!(target.finishes, 3);
        assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    }

    #[test]
    fn reset_restores_registers_lines_and_targets() {
        let (mut h, id) = rig_with_mocks(&[0x50, 0x51]);
        h.write32(BASE + CR1, CR1_PE | CR1_STOPIE);
        transfer_config(&mut h, DEV50, 0, AUTOEND_MODE, GENERATE_START_WRITE);
        assert!(h.irq_level(EVENT_NVIC));
        h.write32(BASE + OAR1, 0x8010);
        h.core_mut().reset_all();
        for change in h.core_mut().irq_changes().to_vec() {
            assert_eq!(change.1, false, "reset only lowers lines");
        }
        assert_eq!(h.read32(BASE + CR1), 0);
        assert_eq!(h.read32(BASE + CR2), 0);
        assert_eq!(h.read32(BASE + OAR1), 0);
        assert_eq!(isr(&mut h), ISR_TXE);
        assert_eq!(mock(&h, id, 0x50).resets, 1);
        assert_eq!(mock(&h, id, 0x51).resets, 1);
        assert!(!h.output(id, EVENT_INTERRUPT));
    }
}
