// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/UART/STM32F7_USART.cs, the UARTBase
// behavior in src/Emulator/Main/Peripherals/UART/UARTBase.cs and the register framework of
// src/Emulator/Main/Core/Structure/Registers/ (MIT License, Copyright (c) Antmicro).

//! STM32 USART/UART as modeled by Renode's `UART.STM32F7_USART` (non low-power mode), with a passive
//! transmit observer hook.
//!
//! What the model does, exactly as Renode does it:
//!
//! * a character written to `TDR` is **transmitted at the moment of the register write** (the observer
//!   hook is called with the byte and `ctx.now()`) if both `UE` and `TE` are set; `TC` is set and the
//!   interrupt line recomputed. Nothing models the wire: no baud-rate timing, no `TXE` delay (`TXE` always
//!   reads 1), no framing, no echo. If `UE` or `TE` is clear the character is dropped with a warning;
//! * the receive side is an unbounded byte queue fed by [`Usart::receive_byte`] (Renode `WriteChar`); the
//!   NGC platform connects nothing to it, but the register behavior (`RXNE`, `RDR` pop, `RXFRQ`, the receiver
//!   timeout and the `ReceiveDmaRequest` output) is ported;
//! * registers are served by a port of Renode's register framework: unhandled offsets log
//!   `Unhandled read/write ...`, writes of ones to unmodeled ("tagged") bits log the framework's
//!   `Unhandled write to offset ... Tags: ...` message, bits that no field covers keep their reset value.
//!
//! Output lines: [`IRQ_LINE`] (0) is Renode's `IRQ`, [`RX_DMA_REQUEST_LINE`] (1) is `ReceiveDmaRequest`.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, LogLevel, Peripheral, Time, Translations, View, Width, TICKS_PER_MICROSECOND};
use std::collections::VecDeque;

/// Output line 0: the interrupt request (`IRQ -> nvic@n` in the `.repl`).
pub const IRQ_LINE: u32 = 0;
/// Output line 1: the receive DMA request (`ReceiveDmaRequest`); not connected on the NGC boards.
pub const RX_DMA_REQUEST_LINE: u32 = 1;

/// Register offsets (RM0351 USART register map; the model has no `GTPR` at 0x10).
pub mod offset {
    pub const CR1: u32 = 0x00;
    pub const CR2: u32 = 0x04;
    pub const CR3: u32 = 0x08;
    pub const BRR: u32 = 0x0C;
    pub const RTOR: u32 = 0x14;
    pub const RQR: u32 = 0x18;
    pub const ISR: u32 = 0x1C;
    pub const ICR: u32 = 0x20;
    pub const RDR: u32 = 0x24;
    pub const TDR: u32 = 0x28;
}

/// Callback receiving every transmitted byte with the virtual time of the `TDR` write
/// (the Renode `UARTBase.CharReceived` event that `NGCUartCapture` subscribes to).
pub type TxHook = Box<dyn FnMut(u8, Time)>;

/// Renode `BufferState` of `IUARTWithBufferState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferState {
    Empty,
    Ready,
    Full,
}

/// Renode `Parity` as reported by `STM32F7_USART.ParityBit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

/// Renode `Bits` as reported by `STM32F7_USART.StopBits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopBits {
    One,
    Half,
    Two,
    OneAndAHalf,
}

// ---- register framework port ------------------------------------------------------------

/// Renode `FieldMode` combinations used by the USART.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `Read`: readable, writes ignored.
    Read,
    /// `Write`: write-only (reads as 0).
    Write,
    /// `Read | Write`.
    ReadWrite,
    /// `Read | WriteOneToClear`.
    ReadWriteOneToClear,
    /// `WriteOneToClear` (not readable).
    WriteOneToClear,
}

impl Mode {
    const fn readable(self) -> bool {
        matches!(self, Mode::Read | Mode::ReadWrite | Mode::ReadWriteOneToClear)
    }
}

/// A register field (`WithFlag` / `WithValueField`).
struct Field {
    pos: u8,
    width: u8,
    mode: Mode,
}

impl Field {
    const fn mask(&self) -> u32 {
        mask_of(self.pos, self.width)
    }
}

/// An unmodeled field (`WithTaggedFlag`, `WithTag`, `WithReservedBits`), kept for the unhandled-write log.
struct Tag {
    name: &'static str,
    pos: u8,
    width: u8,
}

impl Tag {
    const fn mask(&self) -> u32 {
        mask_of(self.pos, self.width)
    }
}

struct RegSpec {
    offset: u32,
    reset: u32,
    fields: &'static [Field],
    /// In definition order (the log lists affected tags in this order).
    tags: &'static [Tag],
}

impl RegSpec {
    fn defined_mask(&self) -> u32 {
        self.fields.iter().fold(0, |m, f| m | f.mask())
    }
}

const fn mask_of(pos: u8, width: u8) -> u32 {
    ((((1u64 << width) - 1) << pos) & 0xFFFF_FFFF) as u32
}

const fn field(pos: u8, width: u8, mode: Mode) -> Field {
    Field { pos, width, mode }
}

const fn tag(name: &'static str, pos: u8, width: u8) -> Tag {
    Tag { name, pos, width }
}

const RW: Mode = Mode::ReadWrite;

// Register indices into `Usart::regs`.
const CR1: usize = 0;
const CR2: usize = 1;
const CR3: usize = 2;
const BRR: usize = 3;
const RTOR: usize = 4;
const RQR: usize = 5;
const ISR: usize = 6;
const ICR: usize = 7;
const RDR: usize = 8;
const TDR: usize = 9;
const REG_COUNT: usize = 10;

// Bit positions used by the behavior.
const CR1_UE: u32 = 1 << 0;
const CR1_RE: u32 = 1 << 2;
const CR1_TE: u32 = 1 << 3;
const CR1_RXNEIE: u32 = 1 << 5;
const CR1_TCIE: u32 = 1 << 6;
const CR1_TXEIE: u32 = 1 << 7;
const CR1_PS: u32 = 1 << 9;
const CR1_PCE: u32 = 1 << 10;
const CR1_OVER8: u32 = 1 << 15;
const CR1_RTOIE: u32 = 1 << 26;
const CR3_DMAR: u32 = 1 << 6;
const ISR_RXNE: u32 = 1 << 5;
const ISR_TC: u32 = 1 << 6;
const ISR_TXE: u32 = 1 << 7;
const ISR_RTOF: u32 = 1 << 11;
const ISR_TEACK: u32 = 1 << 21;
const ISR_REACK: u32 = 1 << 22;
const ICR_RTOCF: u32 = 1 << 11;

/// ISR reset value of the non low-power instance. Renode parity: bit 29 is covered only by a reserved
/// *tag*, so it is never touched by writes and reads back as 1 forever (the C# value `0x200000C0` was
/// presumably meant to be `0x2000C0`-ish; the platform runs with it as is). TC (bit 6) starts set.
const ISR_RESET: u32 = 0x2000_00C0;

static CR1_FIELDS: [Field; 11] = [
    field(0, 1, RW),  // UE
    field(2, 1, RW),  // RE
    field(3, 1, RW),  // TE
    field(5, 1, RW),  // RXNEIE
    field(6, 1, RW),  // TCIE
    field(7, 1, RW),  // TXEIE
    field(8, 1, RW),  // PEIE
    field(9, 1, RW),  // PS
    field(10, 1, RW), // PCE
    field(15, 1, RW), // OVER8
    field(26, 1, RW), // RTOIE
];
static CR1_TAGS: [Tag; 11] = [
    tag("UESM", 1, 1),
    tag("IDLEIE", 4, 1),
    tag("WAKE", 11, 1),
    tag("MO", 12, 1),
    tag("MME", 13, 1),
    tag("CMIE", 14, 1),
    tag("DEDT", 16, 5),
    tag("DEAT", 21, 5),
    tag("M1", 28, 1),
    tag("RESERVED", 29, 3),
    tag("EOBIE", 27, 1),
];

static CR2_FIELDS: [Field; 1] = [field(12, 2, RW)]; // STOP
static CR2_TAGS: [Tag; 19] = [
    tag("RESERVED", 0, 4),
    tag("ADDM7", 4, 1),
    tag("RESERVED", 7, 1),
    tag("SWAP", 15, 1),
    tag("RXINV", 16, 1),
    tag("TXINV", 17, 1),
    tag("DATAINV", 18, 1),
    tag("MSBFIRST", 19, 1),
    tag("ADD", 24, 8),
    tag("LBDL", 5, 1),
    tag("LBDIE", 6, 1),
    tag("LBCL", 8, 1),
    tag("CPHA", 9, 1),
    tag("CPOL", 10, 1),
    tag("CLKEN", 11, 1),
    tag("LINEN", 14, 1),
    tag("ABREN", 20, 1),
    tag("ABRMOD", 21, 2),
    tag("RTOEN", 23, 1),
];

static CR3_FIELDS: [Field; 2] = [
    field(6, 1, RW), // DMAR
    field(7, 1, RW), // DMAT
];
static CR3_TAGS: [Tag; 21] = [
    tag("EIE", 0, 1),
    tag("HDSEL", 3, 1),
    tag("RTSE", 8, 1),
    tag("CTSE", 9, 1),
    tag("CTSIE", 10, 1),
    tag("OVRDIS", 12, 1),
    tag("DDRE", 13, 1),
    tag("DEM", 14, 1),
    tag("DEP", 15, 1),
    tag("RESERVED", 16, 1),
    tag("WUS", 20, 2),
    tag("WUFIE", 22, 1),
    tag("UCESM", 23, 1),
    tag("RESERVED", 25, 7),
    tag("IREN", 1, 1),
    tag("IRLP", 2, 1),
    tag("NACK", 4, 1),
    tag("SCEN", 5, 1),
    tag("ONEBIT", 11, 1),
    tag("SCARCNT", 17, 3),
    tag("TCBGTIE", 24, 1),
];

static BRR_FIELDS: [Field; 1] = [field(0, 16, RW)];
static BRR_TAGS: [Tag; 1] = [tag("RESERVED", 16, 16)];

static RTOR_FIELDS: [Field; 1] = [field(0, 24, RW)];
static RTOR_TAGS: [Tag; 1] = [tag("BLEN (Block length)", 24, 8)];

static RQR_FIELDS: [Field; 3] = [
    field(1, 1, Mode::Write), // SBKRQ
    field(2, 1, Mode::Write), // MMRQ
    field(3, 1, Mode::Write), // RXFRQ
];
static RQR_TAGS: [Tag; 3] = [tag("RESERVED", 6, 26), tag("ABRRQ", 0, 1), tag("TXFRQ", 5, 1)];

static ISR_FIELDS: [Field; 6] = [
    field(5, 1, Mode::Read),  // RXNE
    field(6, 1, Mode::Read),  // TC
    field(7, 1, Mode::Read),  // TXE
    field(21, 1, Mode::Read), // TEACK
    field(22, 1, Mode::Read), // REACK
    field(11, 1, Mode::Read), // RTOF
];
static ISR_TAGS: [Tag; 20] = [
    tag("PE", 0, 1),
    tag("FE", 1, 1),
    tag("NF", 2, 1),
    tag("ORE", 3, 1),
    tag("IDLE", 4, 1),
    tag("CTSIF", 9, 1),
    tag("CTS", 10, 1),
    tag("RESERVED", 13, 1),
    tag("BUSY", 16, 1),
    tag("CMF", 17, 1),
    tag("SBKF", 18, 1),
    tag("RWU", 19, 1),
    tag("WUF", 20, 1),
    tag("RESERVED", 23, 2),
    tag("RESERVED", 26, 6),
    tag("LBDF", 8, 1),
    tag("EOBF", 12, 1),
    tag("ABRE", 14, 1),
    tag("ABRF", 15, 1),
    tag("TCBGT", 25, 1),
];

static ICR_FIELDS: [Field; 2] = [
    field(6, 1, Mode::ReadWriteOneToClear), // TCCF
    field(11, 1, Mode::WriteOneToClear),    // RTOCF
];
static ICR_TAGS: [Tag; 16] = [
    tag("PECF", 0, 1),
    tag("FECF", 1, 1),
    tag("NCF", 2, 1),
    tag("ORECF", 3, 1),
    tag("IDLECF", 4, 1),
    tag("RESERVED", 5, 1),
    tag("CTSCF", 9, 1),
    tag("RESERVED", 10, 1),
    tag("RESERVED", 13, 4),
    tag("CMCF", 17, 1),
    tag("RESERVED", 18, 2),
    tag("WUCF", 20, 1),
    tag("RESERVED", 21, 11),
    tag("TCBGTCF", 7, 1),
    tag("LBDCF", 8, 1),
    tag("EOBCF", 12, 1),
];

static RDR_FIELDS: [Field; 1] = [field(0, 8, Mode::Read)];
static RDR_TAGS: [Tag; 1] = [tag("RESERVED", 8, 24)];

static TDR_FIELDS: [Field; 1] = [field(0, 8, RW)];
static TDR_TAGS: [Tag; 1] = [tag("RESERVED", 8, 24)];

static SPECS: [RegSpec; REG_COUNT] = [
    RegSpec { offset: offset::CR1, reset: 0, fields: &CR1_FIELDS, tags: &CR1_TAGS },
    RegSpec { offset: offset::CR2, reset: 0, fields: &CR2_FIELDS, tags: &CR2_TAGS },
    RegSpec { offset: offset::CR3, reset: 0, fields: &CR3_FIELDS, tags: &CR3_TAGS },
    RegSpec { offset: offset::BRR, reset: 0, fields: &BRR_FIELDS, tags: &BRR_TAGS },
    RegSpec { offset: offset::RTOR, reset: 0, fields: &RTOR_FIELDS, tags: &RTOR_TAGS },
    RegSpec { offset: offset::RQR, reset: 0, fields: &RQR_FIELDS, tags: &RQR_TAGS },
    RegSpec { offset: offset::ISR, reset: ISR_RESET, fields: &ISR_FIELDS, tags: &ISR_TAGS },
    RegSpec { offset: offset::ICR, reset: 0, fields: &ICR_FIELDS, tags: &ICR_TAGS },
    RegSpec { offset: offset::RDR, reset: 0, fields: &RDR_FIELDS, tags: &RDR_TAGS },
    RegSpec { offset: offset::TDR, reset: 0, fields: &TDR_FIELDS, tags: &TDR_TAGS },
];

fn register_index(offset: u32) -> Option<usize> {
    SPECS.iter().position(|spec| spec.offset == offset)
}

/// Renode `BitHelper.GetSetBitsPretty`: `4`, `12`, `16-20`, ... joined by `, `.
fn set_bits_pretty(mask: u32) -> String {
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
        parts.push(if bit == start { format!("{start}") } else { format!("{start}-{bit}") });
        bit += 1;
    }
    parts.join(", ")
}

// ---- the peripheral -----------------------------------------------------------------------

/// Low byte of the `schedule_action` token of the receiver-timeout action; the rest is the generation.
const EV_RECEIVER_TIMEOUT: u64 = 1;
const KEY_NO_CHARS: u64 = 1 << 62;
const KEY_TX_DISABLED: u64 = (1 << 62) | 1;

/// STM32 USART/UART (Renode `UART.STM32F7_USART`).
pub struct Usart {
    name: String,
    frequency: u32,
    regs: [u32; REG_COUNT],
    rx_queue: VecDeque<u8>,
    buffer_state: BufferState,
    tx_hook: Option<TxHook>,
    tx_total: u64,
    /// Renode cancels a receiver-timeout action through a `CancellationToken` and leaves the (now inert)
    /// action in the clock source, where it still fires. Same here: `schedule_action` entries cannot be
    /// withdrawn, so a canceled action is one whose generation is no longer current.
    rto_generation: u64,
}

impl Usart {
    /// `frequency` is the functional clock used for the baud-rate computation (`frequency: 80000000` in the
    /// `.repl`). Low-power mode (`lowPowerMode: true`) is not ported; the NGC platform never sets it.
    pub fn new(name: impl Into<String>, frequency: u32) -> Self {
        let mut regs = [0u32; REG_COUNT];
        for (reg, spec) in regs.iter_mut().zip(SPECS.iter()) {
            *reg = spec.reset;
        }
        Self {
            name: name.into(),
            frequency,
            regs,
            rx_queue: VecDeque::new(),
            buffer_state: BufferState::Empty,
            tx_hook: None,
            tx_total: 0,
            rto_generation: 0,
        }
    }

    /// Installs (or removes) the transmit observer: it receives every transmitted byte together with the
    /// virtual time of the `TDR` write. This is the passive tap Renode's `NGCUartCapture` uses
    /// (`UARTBase.CharReceived`); it never touches the guest-visible state.
    pub fn set_tx_hook(&mut self, hook: Option<TxHook>) {
        self.tx_hook = hook;
    }

    /// Number of bytes transmitted since creation (reset does not clear it).
    pub fn tx_total(&self) -> u64 {
        self.tx_total
    }

    /// Bytes waiting in the receive queue.
    pub fn rx_pending(&self) -> usize {
        self.rx_queue.len()
    }

    pub fn buffer_state(&self) -> BufferState {
        self.buffer_state
    }

    /// Renode `BaudRate`: `multiplier * frequency / max(1, BRR)` (32-bit wrapping arithmetic as in C#).
    pub fn baud_rate(&self) -> u32 {
        let multiplier = if self.regs[CR1] & CR1_OVER8 != 0 { 2u32 } else { 1u32 };
        multiplier.wrapping_mul(self.frequency) / self.regs[BRR].max(1)
    }

    pub fn stop_bits(&self) -> StopBits {
        match (self.regs[CR2] >> 12) & 3 {
            0 => StopBits::One,
            1 => StopBits::Half,
            2 => StopBits::Two,
            _ => StopBits::OneAndAHalf,
        }
    }

    pub fn parity(&self) -> Parity {
        if self.regs[CR1] & CR1_PCE == 0 {
            Parity::None
        } else if self.regs[CR1] & CR1_PS != 0 {
            Parity::Odd
        } else {
            Parity::Even
        }
    }

    /// Renode `UARTBase.WriteChar`: a byte arrives on the receive line. Dropped (debug log) unless
    /// both `UE` and `RE` are set.
    pub fn receive_byte(&mut self, ctx: &mut Ctx<'_>, value: u8) {
        if !(self.re() && self.ue()) {
            ctx.logf(
                LogLevel::Debug,
                format_args!("UART or receive disabled; dropping the character written: '{}'", value as char),
            );
            return;
        }
        self.rx_queue.push_back(value);
        self.char_written(ctx);
    }

    // ---- flag accessors ----

    fn ue(&self) -> bool {
        self.regs[CR1] & CR1_UE != 0
    }

    fn re(&self) -> bool {
        self.regs[CR1] & CR1_RE != 0
    }

    fn te(&self) -> bool {
        self.regs[CR1] & CR1_TE != 0
    }

    fn rxneie(&self) -> bool {
        self.regs[CR1] & CR1_RXNEIE != 0
    }

    fn tcie(&self) -> bool {
        self.regs[CR1] & CR1_TCIE != 0
    }

    fn txeie(&self) -> bool {
        self.regs[CR1] & CR1_TXEIE != 0
    }

    fn rtoie(&self) -> bool {
        self.regs[CR1] & CR1_RTOIE != 0
    }

    fn dmar(&self) -> bool {
        self.regs[CR3] & CR3_DMAR != 0
    }

    fn tc(&self) -> bool {
        self.regs[ISR] & ISR_TC != 0
    }

    fn set_tc(&mut self, value: bool) {
        if value {
            self.regs[ISR] |= ISR_TC;
        } else {
            self.regs[ISR] &= !ISR_TC;
        }
    }

    fn rto_flag(&self) -> bool {
        self.regs[ICR] & ICR_RTOCF != 0
    }

    // ---- behavior (UARTBase + STM32F7_USART) ----

    /// Renode `UpdateInterrupt`: TXE is assumed always set.
    fn update_interrupt(&self, ctx: &mut Ctx<'_>) {
        let transmit_register_empty = self.txeie();
        let transfer_complete = self.tc() && self.tcie();
        let read_register_not_empty = !self.rx_queue.is_empty() && self.rxneie();
        let receiver_timeout = self.rto_flag() && self.rtoie();
        ctx.set_output(IRQ_LINE, transmit_register_empty || transfer_complete || read_register_not_empty || receiver_timeout);
    }

    /// Renode `BufferState` setter.
    fn set_buffer_state(&mut self, ctx: &mut Ctx<'_>, value: BufferState) {
        if self.buffer_state == value {
            return;
        }
        self.buffer_state = value;
        self.update_interrupt(ctx);
        ctx.set_output(RX_DMA_REQUEST_LINE, self.dmar() && value != BufferState::Empty);
    }

    /// `UARTBase.ClearBuffer`: drop the queue and report `QueueEmptied`.
    fn clear_buffer(&mut self, ctx: &mut Ctx<'_>) {
        self.rx_queue.clear();
        self.set_buffer_state(ctx, BufferState::Empty);
    }

    /// `STM32F7_USART.CharWritten`: a character entered the queue.
    fn char_written(&mut self, ctx: &mut Ctx<'_>) {
        self.set_buffer_state(ctx, BufferState::Ready);
        if self.rtoie() {
            // Cancel the previous receiver-timeout action and arm a new one (`Machine.ScheduleAction`).
            self.cancel_receiver_timeout();
            // Receiver timeout is given in bits of inactivity: (RTO * 8 000 000) / baud microseconds.
            let timeout_us = (u64::from(self.regs[RTOR] & 0xFF_FFFF) * 8_000_000) / u64::from(self.baud_rate().max(1));
            ctx.schedule_action(timeout_us.saturating_mul(TICKS_PER_MICROSECOND), EV_RECEIVER_TIMEOUT | self.rto_generation << 8);
        }
    }

    /// `receiverTimeoutCancellationTokenSrc?.Cancel()`: the pending action (if any) becomes a no-op.
    fn cancel_receiver_timeout(&mut self) {
        self.rto_generation = self.rto_generation.wrapping_add(1);
    }

    /// `HandleReceiveData` (RDR value provider): pops one character.
    fn handle_receive_data(&mut self, ctx: &mut Ctx<'_>) -> u32 {
        match self.rx_queue.pop_front() {
            Some(byte) => {
                if self.rx_queue.is_empty() {
                    self.set_buffer_state(ctx, BufferState::Empty);
                }
                u32::from(byte)
            }
            None => {
                ctx.warn_once(KEY_NO_CHARS, format_args!("No characters in queue."));
                0
            }
        }
    }

    /// `HandleTransmitData` (TDR write callback): the character is emitted immediately.
    fn handle_transmit_data(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        if self.te() && self.ue() {
            self.tx_total += 1;
            if let Some(hook) = self.tx_hook.as_mut() {
                hook(value as u8, ctx.now());
            }
            self.set_tc(true);
            self.update_interrupt(ctx);
        } else {
            ctx.warn_once(
                KEY_TX_DISABLED,
                format_args!("Char was to be sent, but the transmitter (or the whole USART) is not enabled. Ignoring."),
            );
        }
    }

    // ---- register engine (port of PeripheralRegister.ReadInner/WriteInner) ----

    /// Value providers of the ISR fields that have one, applied to `value` (the RDR provider pops a
    /// character and therefore lives in `read_register`).
    fn with_providers(&self, idx: usize, value: u32) -> u32 {
        match idx {
            ISR => {
                let mut v = value;
                let set = |v: &mut u32, bit: u32, on: bool| {
                    if on {
                        *v |= bit
                    } else {
                        *v &= !bit
                    }
                };
                set(&mut v, ISR_RXNE, !self.rx_queue.is_empty());
                set(&mut v, ISR_TXE, true);
                set(&mut v, ISR_RTOF, self.rtoie() && self.rto_flag());
                set(&mut v, ISR_TEACK, self.te());
                set(&mut v, ISR_REACK, self.re());
                v
            }
            _ => value,
        }
    }

    /// Bits of `stored` that a read returns: bits of unreadable *fields* are cleared; bits outside every
    /// field (tags) keep whatever the register holds (their reset value).
    fn readable(idx: usize, stored: u32) -> u32 {
        let mut value = stored;
        for f in SPECS[idx].fields {
            if !f.mode.readable() {
                value &= !f.mask();
            }
        }
        value
    }

    fn read_register(&mut self, ctx: &mut Ctx<'_>, idx: usize) -> u32 {
        match idx {
            ISR => self.regs[ISR] = self.with_providers(ISR, self.regs[ISR]),
            RDR => {
                let byte = self.handle_receive_data(ctx);
                self.regs[RDR] = (self.regs[RDR] & !0xFF) | (byte & 0xFF);
            }
            _ => {}
        }
        Self::readable(idx, self.regs[idx])
    }

    fn peek_register(&self, idx: usize) -> u32 {
        let stored = match idx {
            RDR => (self.regs[RDR] & !0xFF) | u32::from(self.rx_queue.front().copied().unwrap_or(0)),
            _ => self.with_providers(idx, self.regs[idx]),
        };
        Self::readable(idx, stored)
    }

    fn write_register(&mut self, ctx: &mut Ctx<'_>, idx: usize, offset: u32, value: u32) {
        let spec = &SPECS[idx];
        let base = self.regs[idx];
        let difference = base ^ value;
        let mut stored = base;
        for f in spec.fields {
            let mask = f.mask();
            match f.mode {
                Mode::ReadWrite | Mode::Write => {
                    if difference & mask != 0 {
                        stored = (stored & !mask) | (value & mask);
                    }
                }
                Mode::ReadWriteOneToClear | Mode::WriteOneToClear => {
                    if (!difference & value) & mask != 0 {
                        stored &= !(value & mask);
                    }
                }
                Mode::Read => {}
            }
        }
        self.regs[idx] = stored;
        // Field write callbacks run for every field that has one, changed or not, in definition order.
        for f in spec.fields {
            let old_field = (base & f.mask()) >> f.pos;
            let new_field = (value & f.mask()) >> f.pos;
            self.field_write_callback(ctx, idx, f.pos, old_field, new_field);
        }
        self.register_write_callbacks(ctx, idx);
        let unhandled = difference & !spec.defined_mask();
        if unhandled != 0 {
            Self::log_unhandled_bits(ctx, spec, offset, unhandled, value);
        }
    }

    fn field_write_callback(&mut self, ctx: &mut Ctx<'_>, idx: usize, pos: u8, _old: u32, new: u32) {
        match (idx, pos) {
            // CR1.TE: disabling the transmitter marks the transfer complete.
            (CR1, 3) => {
                if new == 0 {
                    self.set_tc(true);
                }
            }
            // ICR.TCCF: write 1 clears the TC flag.
            (ICR, 6) => {
                if new != 0 {
                    self.set_tc(false);
                }
            }
            // RQR.RXFRQ: flush the receive queue.
            (RQR, 3) => {
                if new != 0 {
                    self.clear_buffer(ctx);
                }
            }
            // TDR: transmit.
            (TDR, 0) => self.handle_transmit_data(ctx, new),
            _ => {}
        }
    }

    fn register_write_callbacks(&mut self, ctx: &mut Ctx<'_>, idx: usize) {
        match idx {
            CR1 => {
                self.update_interrupt(ctx);
                // Second callback of the non low-power definition: cancel a pending receiver timeout
                // when it can no longer fire.
                if !self.ue() || !self.re() || !self.rtoie() {
                    self.cancel_receiver_timeout();
                }
            }
            ICR => {
                // Defined twice (common part and non low-power part).
                self.update_interrupt(ctx);
                self.update_interrupt(ctx);
            }
            _ => {}
        }
    }

    /// Framework message for writes that change bits no field handles (only logged when a tag is hit).
    fn log_unhandled_bits(ctx: &mut Ctx<'_>, spec: &RegSpec, offset: u32, unhandled: u32, value: u32) {
        let affected: Vec<String> = spec
            .tags
            .iter()
            .filter(|t| unhandled & t.mask() != 0)
            .map(|t| format!("{} (0x{:X})", t.name, (value & t.mask()) >> t.pos))
            .collect();
        if affected.is_empty() {
            return;
        }
        let key = (1u64 << 63) | (u64::from(offset) << 32) | u64::from(value);
        ctx.warn_once(
            key,
            format_args!(
                "Unhandled write to offset 0x{offset:X}. Unhandled bits: [{}] when writing value 0x{value:X}. Tags: {}.",
                set_bits_pretty(unhandled),
                affected.join(", ")
            ),
        );
    }
}

impl Peripheral for Usart {
    fn name(&self) -> &str {
        &self.name
    }

    /// Renode `[AllowedTranslations(ByteToDoubleWord | WordToDoubleWord)]` on an `IDoubleWordPeripheral`.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD)
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        // UARTBase.Reset: ClearBuffer(); then the register collection, IRQ and DMA request.
        self.clear_buffer(ctx);
        for (reg, spec) in self.regs.iter_mut().zip(SPECS.iter()) {
            *reg = spec.reset;
        }
        ctx.set_output(IRQ_LINE, false);
        self.cancel_receiver_timeout();
        ctx.set_output(RX_DMA_REQUEST_LINE, false);
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match register_index(offset) {
            Some(idx) => self.read_register(ctx, idx),
            None => {
                ctx.warn_once(u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}."));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match register_index(offset) {
            Some(idx) => self.write_register(ctx, idx, offset, value),
            None => {
                let key = (1u64 << 61) | (u64::from(offset) << 32) | u64::from(value);
                ctx.warn_once(key, format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}."));
            }
        }
    }

    /// The receiver-timeout action (`ReportRxTimeout`): runs only if its cancellation token is still valid.
    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        if token & 0xFF == EV_RECEIVER_TIMEOUT && token >> 8 == self.rto_generation {
            self.regs[ICR] |= ICR_RTOCF;
            self.update_interrupt(ctx);
        }
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        register_index(offset).map(|idx| self.peek_register(idx))
    }

    fn summary(&self, _view: &View<'_>) -> String {
        format!(
            "{}: UE={}, TE={}, RE={}, baud={}, txBytes={}, rxPending={}",
            self.name,
            u8::from(self.ue()),
            u8::from(self.te()),
            u8::from(self.re()),
            self.baud_rate(),
            self.tx_total,
            self.rx_queue.len()
        )
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{from_micros, PeriphId};
    use std::cell::RefCell;
    use std::rc::Rc;

    const BASE: u32 = 0x4000_4C00;
    const IRQ: u32 = 52;
    const UE: u32 = CR1_UE;
    const RE: u32 = CR1_RE;
    const TE: u32 = CR1_TE;

    fn rig() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Usart::new("uart4", 80_000_000));
        h.connect_irq(id, IRQ_LINE, IRQ);
        h.clear_irq_changes();
        (h, id)
    }

    type TxLog = Rc<RefCell<Vec<(u8, Time)>>>;

    fn tap(h: &mut Harness, id: PeriphId) -> TxLog {
        let log: TxLog = Rc::new(RefCell::new(Vec::new()));
        let sink = log.clone();
        h.get_mut::<Usart>(id).set_tx_hook(Some(Box::new(move |byte, time| sink.borrow_mut().push((byte, time)))));
        log
    }

    fn inject(h: &mut Harness, id: PeriphId, byte: u8) {
        h.with::<Usart, _>(id, |u, ctx| u.receive_byte(ctx, byte));
    }

    fn reg(offset: u32) -> u32 {
        BASE + offset
    }

    #[test]
    fn reset_values_follow_renode_including_the_isr_quirks() {
        let (mut h, id) = rig();
        // TC (bit 6) starts set, TXE (bit 7) is always 1, bit 29 sits in a reserved *tag* and never changes.
        assert_eq!(h.read32(reg(offset::ISR)), 0x2000_00C0);
        for off in [offset::CR1, offset::CR2, offset::CR3, offset::BRR, offset::RTOR, offset::RQR, offset::ICR, offset::TDR] {
            assert_eq!(h.read32(reg(off)), 0, "register at 0x{off:X}");
        }
        // TEACK/REACK mirror TE/RE immediately.
        h.write32(reg(offset::CR1), UE | TE | RE);
        assert_eq!(h.read32(reg(offset::ISR)), 0x2060_00C0);
        h.write32(reg(offset::CR1), UE | RE);
        assert_eq!(h.read32(reg(offset::ISR)), 0x2040_00C0);
        assert_eq!(h.peek(reg(offset::ISR), Width::Word), Some(0x2040_00C0), "peek equals read");
        assert!(h.warnings().is_empty());
        assert_eq!(h.get::<Usart>(id).tx_total(), 0);
    }

    #[test]
    fn tdr_write_transmits_at_the_write_time_and_reads_back() {
        let (mut h, id) = rig();
        let log = tap(&mut h, id);
        h.write32(reg(offset::CR1), UE | TE);
        h.advance_to(from_micros(10));
        h.write32(reg(offset::TDR), u32::from(b'H'));
        h.advance_to(from_micros(25));
        h.write32(reg(offset::TDR), 0x1234_5649);
        assert_eq!(*log.borrow(), vec![(b'H', from_micros(10)), (0x49, from_micros(25))]);
        assert_eq!(h.read32(reg(offset::TDR)), 0x49, "TDR returns the last written byte");
        assert_eq!(h.get::<Usart>(id).tx_total(), 2);
        // The upper bits of a word write hit the RESERVED tag: framework message with bit ranges.
        assert_eq!(
            h.warnings(),
            vec![
                "Unhandled write to offset 0x28. Unhandled bits: [9-10, 12, 14, 18, 20-21, 25, 28] when writing value 0x12345649. Tags: RESERVED (0x123456)."
                    .to_string()
            ]
        );
    }

    #[test]
    fn tdr_write_is_dropped_with_a_warning_unless_ue_and_te_are_set() {
        let (mut h, id) = rig();
        let log = tap(&mut h, id);
        h.write32(reg(offset::TDR), 0x41);
        h.write32(reg(offset::CR1), UE);
        h.write32(reg(offset::TDR), 0x42);
        h.write32(reg(offset::CR1), TE);
        h.write32(reg(offset::TDR), 0x43);
        assert!(log.borrow().is_empty());
        assert_eq!(h.get::<Usart>(id).tx_total(), 0);
        assert_eq!(
            h.warnings(),
            vec!["Char was to be sent, but the transmitter (or the whole USART) is not enabled. Ignoring.".to_string()],
            "logged once"
        );
        h.write32(reg(offset::CR1), UE | TE);
        h.write32(reg(offset::TDR), 0x44);
        assert_eq!(*log.borrow(), vec![(0x44, 0)]);
    }

    #[test]
    fn transfer_complete_flag_and_interrupt() {
        let (mut h, _id) = rig();
        // TC is set from reset, so enabling TCIE raises the level immediately.
        h.write32(reg(offset::CR1), UE | TE | CR1_TCIE);
        assert!(h.irq_level(IRQ));
        // ICR.TCCF clears TC and drops the line; ICR itself reads 0.
        h.write32(reg(offset::ICR), 1 << 6);
        assert!(!h.irq_level(IRQ));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_TC, 0);
        assert_eq!(h.read32(reg(offset::ICR)), 0);
        // A transmitted character sets TC again.
        h.write32(reg(offset::TDR), 0x55);
        assert!(h.irq_level(IRQ));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_TC, ISR_TC);
        h.write32(reg(offset::ICR), 1 << 6);
        assert!(!h.irq_level(IRQ));
        // Disabling the transmitter sets TC (TE write callback), even without a change of other bits.
        h.write32(reg(offset::CR1), UE | CR1_TCIE);
        assert!(h.irq_level(IRQ));
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn txe_interrupt_is_a_level_that_follows_txeie() {
        let (mut h, _id) = rig();
        h.write32(reg(offset::CR1), UE | TE | CR1_TXEIE);
        assert!(h.irq_level(IRQ));
        h.write32(reg(offset::TDR), 0x30);
        assert!(h.irq_level(IRQ), "TXE stays set, the line stays high");
        h.write32(reg(offset::CR1), UE | TE);
        assert!(!h.irq_level(IRQ));
        let changes: Vec<bool> = h.irq_changes().iter().map(|c| c.level).collect();
        assert_eq!(changes, vec![true, false], "no duplicate notifications while the level is unchanged");
    }

    #[test]
    fn unhandled_registers_and_tagged_bits_log_like_renode() {
        let (mut h, _id) = rig();
        // Messages observed in the Renode reference boot (uart5/usart2).
        assert_eq!(h.read32(reg(0x10)), 0);
        h.write32(reg(0x10), 0xA);
        h.write32(reg(offset::CR1), 0x140C);
        h.write32(reg(offset::CR1), 0x1D);
        h.write32(reg(offset::CR3), 0x2);
        h.write32(reg(offset::CR3), 0x1);
        assert_eq!(
            h.warnings(),
            vec![
                "Unhandled read from offset 0x10.".to_string(),
                "Unhandled write to offset 0x10, value 0xA.".to_string(),
                "Unhandled write to offset 0x0. Unhandled bits: [12] when writing value 0x140C. Tags: MO (0x1).".to_string(),
                // Tag bits are never stored, so only bits that differ from the stored value are "unhandled":
                // after 0x140C (stored 0x40C) the value 0x1D differs from it at bits 10, 4 and 0 -> only bit 4 is a tag.
                "Unhandled write to offset 0x0. Unhandled bits: [4] when writing value 0x1D. Tags: IDLEIE (0x1).".to_string(),
                "Unhandled write to offset 0x8. Unhandled bits: [1] when writing value 0x2. Tags: IREN (0x1).".to_string(),
                "Unhandled write to offset 0x8. Unhandled bits: [0] when writing value 0x1. Tags: EIE (0x1).".to_string(),
            ]
        );
        // The same (offset, value) message is logged once.
        h.write32(reg(offset::CR3), 0x1);
        assert_eq!(h.warnings().len(), 6);
    }

    #[test]
    fn writes_that_hit_no_tag_are_silent_and_isr_bit_29_is_reserved() {
        let (mut h, _id) = rig();
        // RQR bit 4 belongs to no field and no tag: no message.
        h.write32(reg(offset::RQR), 0x10);
        assert!(h.warnings().is_empty());
        assert_eq!(h.read32(reg(offset::RQR)), 0, "RQR is write-only");
        // Writing the ISR (all fields read-only): bit 29 differs from its stored 1 and is tagged RESERVED.
        h.write32(reg(offset::ISR), 0);
        assert_eq!(
            h.warnings(),
            vec!["Unhandled write to offset 0x1C. Unhandled bits: [29] when writing value 0x0. Tags: RESERVED (0x0).".to_string()]
        );
        assert_eq!(h.read32(reg(offset::ISR)), 0x2000_00C0, "reads never change");
    }

    #[test]
    fn sub_word_accesses_are_translated_to_read_modify_write() {
        let (mut h, id) = rig();
        let log = tap(&mut h, id);
        h.write32(reg(offset::CR1), UE | TE | RE);
        // Byte write to the low byte of CR1 keeps the other bytes (RMW through a word access).
        h.write8(reg(offset::CR1), 0x09); // UE | TE
        assert_eq!(h.read32(reg(offset::CR1)), UE | TE);
        h.write16(reg(offset::CR1), 0x0D);
        assert_eq!(h.read32(reg(offset::CR1)), UE | RE | TE);
        assert_eq!(h.read16(reg(offset::ISR)), 0x00C0 | 0x0000, "halfword read of the low half");
        assert_eq!(h.read8(reg(offset::ISR) + 3), 0x20 | 0x00, "byte read at offset +3 returns bits 31:24");
        // A byte store to TDR transmits (the RMW read of TDR has no side effect).
        h.write8(reg(offset::TDR), 0x5A);
        assert_eq!(*log.borrow(), vec![(0x5A, 0)]);
    }

    #[test]
    fn receiver_gates_and_rxne_flow() {
        let (mut h, id) = rig();
        // Receiver disabled: the character is dropped.
        inject(&mut h, id, b'a');
        assert_eq!(h.get::<Usart>(id).rx_pending(), 0);
        h.write32(reg(offset::CR1), UE | RE | CR1_RXNEIE);
        assert!(!h.irq_level(IRQ));
        inject(&mut h, id, b'b');
        inject(&mut h, id, b'c');
        assert_eq!(h.get::<Usart>(id).rx_pending(), 2);
        assert_eq!(h.get::<Usart>(id).buffer_state(), BufferState::Ready);
        assert!(h.irq_level(IRQ));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RXNE, ISR_RXNE);
        assert_eq!(h.peek(reg(offset::RDR), Width::Word), Some(u32::from(b'b')), "peek does not pop");
        assert_eq!(h.read32(reg(offset::RDR)), u32::from(b'b'));
        assert!(h.irq_level(IRQ), "one character left");
        // A byte read of RDR is a translated word read: it pops exactly once.
        assert_eq!(h.read8(reg(offset::RDR)), u32::from(b'c'));
        assert!(!h.irq_level(IRQ));
        assert_eq!(h.get::<Usart>(id).buffer_state(), BufferState::Empty);
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RXNE, 0);
        // Reading an empty queue warns once and returns 0.
        assert_eq!(h.read32(reg(offset::RDR)), 0);
        assert_eq!(h.read32(reg(offset::RDR)), 0);
        assert_eq!(h.warnings(), vec!["No characters in queue.".to_string()]);
    }

    #[test]
    fn rxfrq_flushes_the_queue_and_dmar_drives_the_request_line() {
        let (mut h, id) = rig();
        let dma = h.probe(id, RX_DMA_REQUEST_LINE);
        h.write32(reg(offset::CR1), UE | RE | CR1_RXNEIE);
        inject(&mut h, id, 1);
        assert_eq!(h.probe_changes(dma), vec![], "DMAR clear: no request");
        h.write32(reg(offset::RQR), 1 << 3);
        assert_eq!(h.get::<Usart>(id).rx_pending(), 0);
        assert!(!h.irq_level(IRQ));
        h.write32(reg(offset::CR3), CR3_DMAR);
        inject(&mut h, id, 2);
        assert_eq!(h.probe_changes(dma), vec![(0, true)]);
        assert_eq!(h.read32(reg(offset::RDR)), 2);
        assert_eq!(h.probe_changes(dma), vec![(0, true), (0, false)]);
    }

    #[test]
    fn receiver_timeout_fires_after_the_programmed_idle_time_and_rearms_on_each_character() {
        let (mut h, id) = rig();
        // baud = 80 MHz / 80 = 1 Mbaud; RTO = 125 bits -> 125 * 8 000 000 / 1 000 000 = 1000 us.
        h.write32(reg(offset::BRR), 80);
        h.write32(reg(offset::RTOR), 125);
        h.write32(reg(offset::CR1), UE | RE | CR1_RTOIE);
        assert_eq!(h.get::<Usart>(id).baud_rate(), 1_000_000);
        inject(&mut h, id, b'x');
        assert_eq!(h.next_event_time(), Some(from_micros(1000)));
        h.advance_to(from_micros(500));
        inject(&mut h, id, b'y'); // re-arms: the first action is canceled ...
        assert_eq!(h.next_event_time(), Some(from_micros(1000)), "... but, as in Renode, it stays queued");
        h.advance_to(from_micros(1000)); // the canceled action fires and does nothing
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RTOF, 0);
        assert!(!h.irq_level(IRQ));
        assert_eq!(h.next_event_time(), Some(from_micros(1500)));
        h.advance_to(from_micros(1499));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RTOF, 0);
        assert!(!h.irq_level(IRQ));
        h.advance_to(from_micros(1500));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RTOF, ISR_RTOF);
        assert!(h.irq_level(IRQ));
        // RTOCF (ICR bit 11, write 1 to clear) removes the flag and the interrupt.
        h.write32(reg(offset::ICR), ICR_RTOCF);
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RTOF, 0);
        assert!(!h.irq_level(IRQ));
        // Disabling the receiver cancels a pending timeout: it still fires (2.5 ms) but has no effect.
        inject(&mut h, id, b'z');
        assert_eq!(h.next_event_time(), Some(from_micros(2500)));
        h.write32(reg(offset::CR1), UE | CR1_RTOIE);
        h.advance_to(from_micros(2500));
        assert_eq!(h.read32(reg(offset::ISR)) & ISR_RTOF, 0);
        assert!(!h.irq_level(IRQ));
        assert_eq!(h.next_event_time(), None);
    }

    #[test]
    fn configuration_accessors_report_baud_stop_bits_and_parity() {
        let (mut h, id) = rig();
        h.write32(reg(offset::BRR), 694);
        assert_eq!(h.get::<Usart>(id).baud_rate(), 80_000_000 / 694);
        h.write32(reg(offset::CR1), CR1_OVER8);
        assert_eq!(h.get::<Usart>(id).baud_rate(), 160_000_000 / 694);
        h.write32(reg(offset::BRR), 0);
        assert_eq!(h.get::<Usart>(id).baud_rate(), 160_000_000, "BRR 0 behaves as 1");
        assert_eq!(h.get::<Usart>(id).parity(), Parity::None);
        h.write32(reg(offset::CR1), CR1_PCE);
        assert_eq!(h.get::<Usart>(id).parity(), Parity::Even);
        h.write32(reg(offset::CR1), CR1_PCE | CR1_PS);
        assert_eq!(h.get::<Usart>(id).parity(), Parity::Odd);
        for (bits, expected) in [(0, StopBits::One), (1, StopBits::Half), (2, StopBits::Two), (3, StopBits::OneAndAHalf)] {
            h.write32(reg(offset::CR2), bits << 12);
            assert_eq!(h.get::<Usart>(id).stop_bits(), expected);
        }
    }

    #[test]
    fn reset_restores_registers_clears_the_queue_and_keeps_the_transmit_count() {
        let (mut h, id) = rig();
        let _log = tap(&mut h, id);
        h.write32(reg(offset::CR1), UE | TE | RE | CR1_RXNEIE | CR1_TXEIE);
        h.write32(reg(offset::TDR), 0x20);
        inject(&mut h, id, 7);
        assert!(h.irq_level(IRQ));
        h.core_mut().reset_all();
        // Any harness bus access delivers the IRQ changes queued by the reset.
        assert_eq!(h.read32(reg(offset::CR1)), 0);
        assert!(!h.irq_level(IRQ));
        assert_eq!(h.get::<Usart>(id).rx_pending(), 0);
        assert_eq!(h.get::<Usart>(id).buffer_state(), BufferState::Empty);
        assert_eq!(h.read32(reg(offset::CR1)), 0);
        assert_eq!(h.read32(reg(offset::ISR)), 0x2000_00C0);
        assert_eq!(h.get::<Usart>(id).tx_total(), 1);
    }

    #[test]
    fn summary_describes_the_state() {
        let (mut h, _id) = rig();
        h.write32(reg(offset::BRR), 694);
        h.write32(reg(offset::CR1), UE | TE);
        h.write32(reg(offset::TDR), 0x20);
        let summaries = h.core().summaries();
        let summary = &summaries.iter().find(|(name, _)| name == "uart4").unwrap().1;
        assert_eq!(summary, "uart4: UE=1, TE=1, RE=0, baud=115273, txBytes=1, rxPending=0");
    }
}
