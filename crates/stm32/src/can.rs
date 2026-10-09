// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/CAN/STMCAN.cs together with
// src/Emulator/Main/Peripherals/CAN/CANMessageFrame.cs and ICAN.cs (MIT License, Copyright (c) Antmicro).

//! bxCAN controller as modeled by Renode's `CAN.STMCAN` (master instance, no CAN2/slave support).
//!
//! The model is functional, not electrical. There is no bit timing, no arbitration and no mailbox
//! scheduling: a write to `CAN_TIxR` with TXRQ set emits the frame **synchronously** (Renode `FrameSent`),
//! marks the mailbox empty and completed, and returns. Frames the controller emits are appended to an
//! out-queue ([`StmCan::take_tx_frames`]) together with the sender's `ctx.now()` stamp; the system delivers
//! them to the peer with [`StmCan::deliver_frame`] at its synchronization points (Renode: the sync phase at
//! the end of the quantum, see `models::can_link`).
//!
//! Renode quirks that are reproduced on purpose (`// Renode parity` markers in the code):
//!
//! * the `MCR`/`MSR` init/sleep handshake exactly as written in `WriteDoubleWord(CAN_MCR)` (a write with both
//!   `INRQ` and `SLEEP` set enters normal mode; the NGC boards pre-write `MCR = 0x00010000` for that reason);
//! * `FrameSent` without a subscriber sets `ESR.LEC = 5` ("bit dominant error") and re-evaluates the SCE
//!   interrupt - [`StmCan::set_frame_sink_attached`] stands for "somebody subscribed to `FrameSent`";
//! * filter banks: `FA1R` writes snapshot which banks belong to which FIFO, only the first identifier/mask pair
//!   of a mask-mode bank is evaluated, 32-bit filters compare 14 extended-id bits, the received `RDTR` never
//!   carries a filter match index or timestamp, `TSR.TERR0` clearing clears `TERR1`;
//! * `RFR`/`TSR`/`MSR` clear semantics, 3-deep FIFOs with overrun, locked/unlocked mode.
//!
//! Output lines: [`LINE_TX`] (0), [`LINE_RX0`] (1), [`LINE_RX1`] (2), [`LINE_SCE`] (3) - Renode `Connections[0..3]`;
//! the `.repl` maps them to NVIC 19..22.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, LogLevel, Peripheral, Time, View, Width};
use std::collections::VecDeque;

pub const LINE_TX: u32 = 0;
pub const LINE_RX0: u32 = 1;
pub const LINE_RX1: u32 = 2;
pub const LINE_SCE: u32 = 3;

/// Register offsets (RM0351 bxCAN register map).
pub mod offset {
    pub const MCR: u32 = 0x00;
    pub const MSR: u32 = 0x04;
    pub const TSR: u32 = 0x08;
    pub const RF0R: u32 = 0x0C;
    pub const RF1R: u32 = 0x10;
    pub const IER: u32 = 0x14;
    pub const ESR: u32 = 0x18;
    pub const BTR: u32 = 0x1C;
    pub const TI0R: u32 = 0x180;
    pub const TDT0R: u32 = 0x184;
    pub const TDL0R: u32 = 0x188;
    pub const TDH0R: u32 = 0x18C;
    pub const TI1R: u32 = 0x190;
    pub const TDT1R: u32 = 0x194;
    pub const TDL1R: u32 = 0x198;
    pub const TDH1R: u32 = 0x19C;
    pub const TI2R: u32 = 0x1A0;
    pub const TDT2R: u32 = 0x1A4;
    pub const TDL2R: u32 = 0x1A8;
    pub const TDH2R: u32 = 0x1AC;
    pub const RI0R: u32 = 0x1B0;
    pub const RDT0R: u32 = 0x1B4;
    pub const RL0R: u32 = 0x1B8;
    pub const RH0R: u32 = 0x1BC;
    pub const RI1R: u32 = 0x1C0;
    pub const RDT1R: u32 = 0x1C4;
    pub const RL1R: u32 = 0x1C8;
    pub const RH1R: u32 = 0x1CC;
    pub const FMR: u32 = 0x200;
    pub const FM1R: u32 = 0x204;
    pub const FS1R: u32 = 0x20C;
    pub const FFA1R: u32 = 0x214;
    pub const FA1R: u32 = 0x21C;
    /// First filter bank register (`CAN_F0R1`); bank `n` register `r` (0 or 1) is at `F0R1 + 8 n + 4 r`.
    pub const F0R1: u32 = 0x240;
    /// Last filter bank register (`CAN_F27R2`).
    pub const F27R2: u32 = 0x31C;
}

const NUMBER_OF_FILTER_BANKS: usize = 28;
const NUMBER_OF_RX_FIFOS: usize = 2;
const MAX_MESSAGES_IN_FIFO: usize = 3;

// MCR
const MCR_DBF: u32 = 1 << 16;
const MCR_RESET: u32 = 1 << 15;
const MCR_TTCM: u32 = 1 << 7;
const MCR_ABOM: u32 = 1 << 6;
const MCR_AWUM: u32 = 1 << 5;
const MCR_NART: u32 = 1 << 4;
const MCR_RFLM: u32 = 1 << 3;
const MCR_TXFP: u32 = 1 << 2;
const MCR_SLEEP: u32 = 1 << 1;
const MCR_INRQ: u32 = 1 << 0;
/// Bits `MasterControlRegister.GetValue` reports (RESET is a one-shot request and never reads back).
const MCR_MASK: u32 = MCR_DBF | MCR_TTCM | MCR_ABOM | MCR_AWUM | MCR_NART | MCR_RFLM | MCR_TXFP | MCR_SLEEP | MCR_INRQ;
const MCR_RESET_VALUE: u32 = 0x0001_0002;

// MSR
const MSR_RX: u32 = 1 << 11;
const MSR_SAMP: u32 = 1 << 10;
const MSR_RXM: u32 = 1 << 9;
const MSR_TXM: u32 = 1 << 8;
const MSR_SLAKI: u32 = 1 << 4;
const MSR_WKUI: u32 = 1 << 3;
const MSR_ERRI: u32 = 1 << 2;
const MSR_SLAK: u32 = 1 << 1;
const MSR_INAK: u32 = 1 << 0;
const MSR_MASK: u32 = MSR_RX | MSR_SAMP | MSR_RXM | MSR_TXM | MSR_SLAKI | MSR_WKUI | MSR_ERRI | MSR_SLAK | MSR_INAK;
const MSR_RESET_VALUE: u32 = 0x0000_0C02;

// TSR
const TSR_RQCP: [u32; 3] = [1 << 0, 1 << 8, 1 << 16];
const TSR_TXOK: [u32; 3] = [1 << 1, 1 << 9, 1 << 17];
const TSR_ALST: [u32; 3] = [1 << 2, 1 << 10, 1 << 18];
const TSR_TERR: [u32; 3] = [1 << 3, 1 << 11, 1 << 19];
const TSR_TME: [u32; 3] = [1 << 26, 1 << 27, 1 << 28];
const TSR_RESET_VALUE: u32 = 0x1C00_0000;

// RFR
const RFR_FULL: u32 = 1 << 3;
const RFR_FOVR: u32 = 1 << 4;
const RFR_RFOM: u32 = 1 << 5;

// IER
const IER_TMEIE: u32 = 1 << 0;
const IER_FMPIE: [u32; 2] = [1 << 1, 1 << 4];
const IER_FFIE: [u32; 2] = [1 << 2, 1 << 5];
const IER_FOVIE: [u32; 2] = [1 << 3, 1 << 6];
const IER_EWGIE: u32 = 1 << 8;
const IER_EPVIE: u32 = 1 << 9;
const IER_BOFIE: u32 = 1 << 10;
const IER_LECIE: u32 = 1 << 11;
const IER_ERRIE: u32 = 1 << 15;
const IER_WKUIE: u32 = 1 << 16;
const IER_SLKIE: u32 = 1 << 17;
const IER_MASK: u32 = IER_TMEIE
    | IER_FMPIE[0]
    | IER_FFIE[0]
    | IER_FOVIE[0]
    | IER_FMPIE[1]
    | IER_FFIE[1]
    | IER_FOVIE[1]
    | IER_EWGIE
    | IER_EPVIE
    | IER_BOFIE
    | IER_LECIE
    | IER_ERRIE
    | IER_WKUIE
    | IER_SLKIE;

// ESR
const ESR_EWGF: u32 = 1 << 0;
const ESR_EPVF: u32 = 1 << 1;
const ESR_BOFF: u32 = 1 << 2;
const LEC_NO_ERROR: u32 = 0;
const LEC_BIT_DOMINANT_ERROR: u32 = 5;
const LEC_SET_BY_SOFTWARE: u32 = 7;

// BTR
const BTR_SILM: u32 = 1 << 31;
const BTR_LBKM: u32 = 1 << 30;
const BTR_RESET_VALUE: u32 = 0x0123_0000;

// FMR
const FMR_FINIT: u32 = 1 << 0;
const FMR_RESET_VALUE: u32 = 0x2A1C_0E01;
const FA1R_MASK: u32 = 0x0FFF_FFFF;

// CAN message layout (CANMessage constants)
const IDSHIFT: u32 = 3;
const STIDSHIFT: u32 = 21;
const STIDMASK: u32 = 0x7FF;
const EXIDSHIFT: u32 = 3;
const EXIDMASK: u32 = 0x3_FFFF;
const IDESHIFT: u32 = 2;
const RTRSHIFT: u32 = 1;
const RIRMASK: u32 = (STIDMASK << STIDSHIFT) | (EXIDMASK << EXIDSHIFT) | (1 << IDESHIFT) | (1 << RTRSHIFT);
const RDTRMASK: u32 = 0xF;
const STANDARD_ID_OFFSET: u32 = 18;

/// A CAN frame as exchanged between controllers (Renode `CANMessageFrame`).
///
/// `id` is the 11-bit identifier for standard frames and the full 29-bit identifier for extended ones.
/// Renode's STMCAN reports a zero-length frame with a `null` payload (the link normalizes it); here the
/// payload of such a frame is an empty vector.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanFrame {
    pub id: u32,
    pub data: Vec<u8>,
    pub extended: bool,
    pub remote: bool,
    pub fd: bool,
    pub bit_rate_switch: bool,
}

impl CanFrame {
    pub fn standard(id: u32, data: &[u8]) -> CanFrame {
        CanFrame { id, data: data.to_vec(), ..CanFrame::default() }
    }

    pub fn extended(id: u32, data: &[u8]) -> CanFrame {
        CanFrame { id, data: data.to_vec(), extended: true, ..CanFrame::default() }
    }

    /// Renode `ExtendedId`: the 29-bit identifier (`Id << 18` for standard frames).
    pub fn extended_id(&self) -> u32 {
        if self.extended {
            self.id
        } else {
            self.id << STANDARD_ID_OFFSET
        }
    }
}

/// A frame emitted by a controller with the virtual time of the transmitting register write
/// (what Renode's link takes as the frame stamp: the sender clock's current time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxFrame {
    pub time: Time,
    pub frame: CanFrame,
}

/// Renode `STMCAN.CANMessage`: the register image of a mailbox message plus the decoded identifier fields.
#[derive(Clone, Debug)]
struct CanMessage {
    rir: u32,
    rdtr: u32,
    rlr: u32,
    rhr: u32,
    stid: u32,
    exid: u32,
    ide: u32,
    rtr: u32,
    dlc: u32,
    data: Vec<u8>,
    rx_fifo: u32,
}

impl CanMessage {
    /// `CANMessage(CANMessageFrame)` - the receive path.
    fn from_frame(frame: &CanFrame) -> CanMessage {
        let mut rir = frame.extended_id() << IDSHIFT;
        set_bit(&mut rir, IDESHIFT, frame.extended);
        set_bit(&mut rir, RTRSHIFT, frame.remote);
        let dlc = frame.data.len() as u32;
        let mut msg = CanMessage {
            rir,
            rdtr: 0,
            rlr: 0,
            rhr: 0,
            stid: 0,
            exid: 0,
            ide: 0,
            rtr: 0,
            dlc,
            data: frame.data.clone(),
            rx_fifo: 0,
        };
        msg.extract_rir_register();
        // GenerateRDTRRegister: timestamp and filter match index are still 0 at this point and
        // are never regenerated after filtering. Renode parity: they never appear in RDTR.
        msg.rdtr = dlc & 0xF;
        msg.generate_data_registers();
        msg
    }

    /// `CANMessage(rir, rdtr, rlr, rhr)` - the transmit path.
    fn from_registers(rir: u32, rdtr: u32, rlr: u32, rhr: u32) -> CanMessage {
        let mut msg = CanMessage {
            rir: rir & RIRMASK,
            rdtr: rdtr & RDTRMASK,
            rlr,
            rhr,
            stid: 0,
            exid: 0,
            ide: 0,
            rtr: 0,
            dlc: 0,
            data: Vec::new(),
            rx_fifo: 0,
        };
        msg.extract_rir_register();
        msg.dlc = msg.rdtr & 0xF;
        msg.extract_data_registers();
        msg
    }

    fn extract_rir_register(&mut self) {
        self.stid = (self.rir >> STIDSHIFT) & STIDMASK;
        self.exid = (self.rir >> EXIDSHIFT) & EXIDMASK;
        self.ide = (self.rir >> IDESHIFT) & 1;
        self.rtr = (self.rir >> RTRSHIFT) & 1;
    }

    /// `ExtractDataRegisters`: DLC bytes (up to 15, bytes above 8 stay 0); no payload for DLC 0.
    fn extract_data_registers(&mut self) {
        if self.dlc > 0 {
            let mut data = vec![0u8; self.dlc as usize];
            for i in 0..4usize {
                if i < self.dlc as usize {
                    data[i] = (self.rlr >> (i * 8)) as u8;
                }
            }
            for i in 0..4usize {
                if i + 4 < self.dlc as usize {
                    data[i + 4] = (self.rhr >> (i * 8)) as u8;
                }
            }
            self.data = data;
        }
    }

    fn generate_data_registers(&mut self) {
        let byte = |i: usize| -> u32 { self.data.get(i).copied().map_or(0, u32::from) };
        self.rlr = byte(0) | byte(1) << 8 | byte(2) << 16 | byte(3) << 24;
        self.rhr = byte(4) | byte(5) << 8 | byte(6) << 16 | byte(7) << 24;
    }

    /// `ToCANMessageFrame`: id from RIR bits [31:3]; standard frames carry `id >> 18`.
    fn to_frame(&self) -> CanFrame {
        let id = (self.rir >> IDSHIFT) & 0x1FFF_FFFF;
        let extended = (self.rir >> IDESHIFT) & 1 != 0;
        CanFrame {
            id: if extended { id } else { id >> STANDARD_ID_OFFSET },
            data: self.data.clone(),
            extended,
            remote: (self.rir >> RTRSHIFT) & 1 != 0,
            fd: false,
            bit_rate_switch: false,
        }
    }
}

fn set_bit(value: &mut u32, bit: u32, on: bool) {
    if on {
        *value |= 1 << bit;
    } else {
        *value &= !(1 << bit);
    }
}

/// One decoded filter (Renode `Filter`).
#[derive(Clone, Copy, Default)]
struct Filter {
    stid: u32,
    rtr: u32,
    ide: u32,
    exid: u32,
    exid_mask: u32,
}

/// Renode `FilterBank`.
#[derive(Clone, Copy)]
struct FilterBank {
    active: bool,
    /// `FilterBankMode.FilterIdentifierList` (else identifier mask).
    list_mode: bool,
    /// `FilterBankScale.FilterScale32Bit` (else 16 bit).
    scale32: bool,
    fifo: u32,
    fr: [u32; 2],
    belongs_to_master: bool,
}

impl FilterBank {
    const fn new() -> FilterBank {
        FilterBank { active: false, list_mode: false, scale32: false, fifo: 0, fr: [0; 2], belongs_to_master: true }
    }

    /// `ExtractFilters`.
    fn extract_filters(&self) -> [Filter; 4] {
        let mut filters = [Filter::default(); 4];
        if self.scale32 {
            for (i, filter) in filters.iter_mut().take(2).enumerate() {
                let fr = self.fr[i];
                *filter = Filter {
                    stid: (fr >> 21) & 0x7FF,
                    exid: (fr >> 3) & 0x3FFFF,
                    ide: (fr >> 2) & 1,
                    rtr: (fr >> 1) & 1,
                    // Renode parity: only 14 extended-id bits take part in 32-bit comparisons.
                    exid_mask: 0x3FFF,
                };
            }
        } else {
            let exid_mask = 0x7 << 15;
            let (fr0, fr1) = (self.fr[0], self.fr[1]);
            // Renode parity: the high half of FRx decodes as the first filter, the low half as the second
            // (the opposite of the reference manual's identifier/mask halves).
            filters[0] = Filter { stid: (fr0 >> 21) & 0x7FF, ide: (fr0 >> 20) & 1, rtr: (fr0 >> 19) & 1, exid: ((fr0 >> 16) & 7) << 15, exid_mask };
            filters[1] = Filter { stid: (fr0 >> 5) & 0x7FF, ide: (fr0 >> 4) & 1, rtr: (fr0 >> 3) & 1, exid: (fr0 & 7) << 15, exid_mask };
            filters[2] = Filter { stid: (fr1 >> 21) & 0x7FF, ide: (fr1 >> 20) & 1, rtr: (fr1 >> 19) & 1, exid: ((fr1 >> 16) & 7) << 15, exid_mask };
            filters[3] = Filter { stid: (fr1 >> 5) & 0x7FF, ide: (fr1 >> 4) & 1, rtr: (fr1 >> 3) & 1, exid: (fr1 & 7) << 15, exid_mask };
        }
        filters
    }

    /// `MatchMessage`; sets `msg.rx_fifo` on a match.
    fn match_message(&self, msg: &mut CanMessage) -> bool {
        let filters = self.extract_filters();
        let number_of_filters = if self.scale32 { 2 } else { 4 };
        if !self.list_mode {
            // Renode parity: `for(i = 0; i < numOfFilters / 2; i += 2, ...)` - only the first
            // identifier/mask pair is ever evaluated, in 32-bit and in 16-bit scale.
            let mut i = 0;
            while i < number_of_filters / 2 {
                let (id, mask) = (&filters[i], &filters[i + 1]);
                if (id.stid & mask.stid) == (msg.stid & mask.stid)
                    && (id.exid & mask.exid & mask.exid_mask) == (msg.exid & mask.exid & mask.exid_mask)
                    && (id.ide & mask.ide) == (msg.ide & mask.ide)
                    && (id.rtr & mask.rtr) == (msg.rtr & mask.rtr)
                {
                    msg.rx_fifo = self.fifo;
                    return true;
                }
                i += 2;
            }
        } else {
            for filter in filters.iter().take(number_of_filters) {
                if filter.stid == msg.stid
                    && (filter.exid & filter.exid_mask) == (msg.exid & filter.exid_mask)
                    && filter.ide == msg.ide
                    && filter.rtr == msg.rtr
                {
                    msg.rx_fifo = self.fifo;
                    return true;
                }
            }
        }
        false
    }
}

/// Receive FIFO: `ReceiveFifoRegister` plus its message queue.
#[derive(Default)]
struct RxFifo {
    queue: VecDeque<CanMessage>,
    full: bool,
    overrun: bool,
}

impl RxFifo {
    fn value(&self) -> u32 {
        (if self.full { RFR_FULL } else { 0 }) | (if self.overrun { RFR_FOVR } else { 0 }) | (self.queue.len() as u32 & 0x3)
    }

    /// `ReceiveMessage`: three entries; the fourth either replaces the oldest (unlocked) or is dropped (locked).
    fn receive(&mut self, msg: CanMessage, locked: bool) {
        if self.queue.len() < MAX_MESSAGES_IN_FIFO {
            self.queue.push_back(msg);
            if self.queue.len() == MAX_MESSAGES_IN_FIFO {
                self.full = true;
            }
        } else if !locked {
            self.queue.pop_front();
            self.queue.push_back(msg);
            self.overrun = true;
        } else {
            self.overrun = true;
        }
    }
}

/// bxCAN controller (Renode `CAN.STMCAN`).
pub struct StmCan {
    name: String,
    mcr: u32,
    mcr_reset_request: bool,
    msr: u32,
    tsr: u32,
    ier: u32,
    esr_flags: u32,
    esr_lec: u32,
    esr_tec: u32,
    esr_rec: u32,
    btr: u32,
    fmr: u32,
    fm1r: u32,
    fs1r: u32,
    ffa1r: u32,
    fa1r: u32,
    ti: [u32; 3],
    tdt: [u32; 3],
    tdl: [u32; 3],
    tdh: [u32; 3],
    rx: [RxFifo; NUMBER_OF_RX_FIFOS],
    banks: [FilterBank; NUMBER_OF_FILTER_BANKS],
    /// `FifoFiltersPrioritized`: bank indices per FIFO as of the last `FA1R` write.
    fifo_filters: [Vec<usize>; NUMBER_OF_RX_FIFOS],
    sink_attached: bool,
    tx_out: Vec<TxFrame>,
    frames_sent: u64,
    frames_received: u64,
}

impl StmCan {
    pub fn new(name: impl Into<String>) -> Self {
        let mut can = StmCan {
            name: name.into(),
            mcr: 0,
            mcr_reset_request: false,
            msr: 0,
            tsr: 0,
            ier: 0,
            esr_flags: 0,
            esr_lec: 0,
            esr_tec: 0,
            esr_rec: 0,
            btr: 0,
            fmr: 0,
            fm1r: 0,
            fs1r: 0,
            ffa1r: 0,
            fa1r: 0,
            ti: [0; 3],
            tdt: [0; 3],
            tdl: [0; 3],
            tdh: [0; 3],
            rx: [RxFifo::default(), RxFifo::default()],
            banks: [FilterBank::new(); NUMBER_OF_FILTER_BANKS],
            fifo_filters: [Vec::new(), Vec::new()],
            sink_attached: false,
            tx_out: Vec::new(),
            frames_sent: 0,
            frames_received: 0,
        };
        // The constructor ends with Reset().
        can.reset_registers(None, true);
        can.reset_filter_banks();
        can
    }

    /// Whether anything is subscribed to `FrameSent` (a link is attached). Without a subscriber a transmit
    /// request sets `ESR.LEC = 5` (bit dominant error) instead of emitting a frame; with one the frame is
    /// appended to the out-queue.
    pub fn set_frame_sink_attached(&mut self, attached: bool) {
        self.sink_attached = attached;
    }

    pub fn frame_sink_attached(&self) -> bool {
        self.sink_attached
    }

    /// Frames emitted since the last drain, oldest first.
    pub fn tx_frames(&self) -> &[TxFrame] {
        &self.tx_out
    }

    /// Drains the out-queue. The system calls this after every quantum and hands the frames to the link.
    pub fn take_tx_frames(&mut self) -> Vec<TxFrame> {
        std::mem::take(&mut self.tx_out)
    }

    /// Total frames the controller has emitted (including those lost to a missing sink).
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// Total frames handed to [`StmCan::deliver_frame`].
    pub fn frames_received(&self) -> u64 {
        self.frames_received
    }

    /// Messages currently held by receive FIFO 0 or 1.
    pub fn fifo_len(&self, fifo: usize) -> usize {
        self.rx[fifo].queue.len()
    }

    /// `OnFrameReceived`: a frame arrives from the bus (the link calls this at a synchronization point).
    pub fn deliver_frame(&mut self, ctx: &mut Ctx<'_>, frame: &CanFrame) {
        if self.mcr & MCR_SLEEP != 0 {
            // Wake up if autowake up is on.
            if self.mcr & MCR_AWUM != 0 {
                self.mcr &= !MCR_SLEEP;
                self.msr &= !MSR_SLAK;
            }
            // Signal wake up interrupt.
            self.msr |= MSR_WKUI;
            self.update_sce_line(ctx);
        } else if self.btr & BTR_LBKM == 0 {
            let mut msg = CanMessage::from_frame(frame);
            for fifo in 0..NUMBER_OF_RX_FIFOS {
                if self.filter_message(fifo, &mut msg) {
                    ctx.logf(LogLevel::Debug, format_args!("Message received RIR={:X}", msg.rir));
                    self.receive_message(ctx, msg.clone());
                } else {
                    ctx.logf(LogLevel::Debug, format_args!("Message dropped by filter RIR={:X}", msg.rir));
                }
            }
        }
        self.frames_received += 1;
    }

    // ---- register images ----

    fn esr_value(&self) -> u32 {
        self.esr_flags | ((self.esr_lec & 0x7) << 4) | ((self.esr_tec & 0xFF) << 16) | ((self.esr_rec & 0xFF) << 24)
    }

    /// `DeviceRegisters.Reset`, in the original order. `ctx` is `None` only while constructing (the
    /// receive-FIFO writes would re-evaluate interrupt lines that are low anyway).
    fn reset_registers(&mut self, mut ctx: Option<&mut Ctx<'_>>, reset_filters: bool) {
        self.mcr = MCR_RESET_VALUE & MCR_MASK;
        self.mcr_reset_request = false;
        self.msr = MSR_RESET_VALUE & MSR_MASK;
        self.tsr = TSR_RESET_VALUE;
        for fifo in 0..NUMBER_OF_RX_FIFOS {
            // CAN_RFR[x].SetValue(0): clears nothing but re-evaluates the interrupt line with the
            // *current* (not yet reset) IER and FIFO state. Renode parity.
            if let Some(ctx) = ctx.as_deref_mut() {
                self.update_fifo_line(ctx, fifo);
            }
        }
        self.ier = 0;
        self.esr_flags = 0;
        self.esr_lec = 0;
        self.esr_tec = 0;
        self.esr_rec = 0;
        self.btr = BTR_RESET_VALUE;
        if reset_filters {
            self.fmr = FMR_RESET_VALUE;
            self.fm1r = 0;
            self.fs1r = 0;
            self.ffa1r = 0;
            self.fa1r = 0;
        }
    }

    /// The bank part of `Reset()`: banks go inactive/16-bit/mask/FIFO 0; FR values and the prioritized
    /// lists are kept (Renode parity).
    fn reset_filter_banks(&mut self) {
        for bank in &mut self.banks {
            bank.active = false;
            bank.list_mode = false;
            bank.fifo = 0;
            bank.scale32 = false;
        }
        self.update_filter_can_assignment();
    }

    fn update_filter_can_assignment(&mut self) {
        let can2_start_bank = ((self.fmr >> 8) & 0x3F) as usize;
        for (i, bank) in self.banks.iter_mut().enumerate() {
            bank.belongs_to_master = i < can2_start_bank;
        }
    }

    fn prioritize_fifo_filters(&mut self, fifo: usize) {
        // The Renode list is also sorted with an order that is not observable (the filter match index is
        // generated before matching and never reaches a register), so membership in bank order is enough.
        self.fifo_filters[fifo] = (0..NUMBER_OF_FILTER_BANKS).filter(|&i| self.banks[i].fifo == fifo as u32).collect();
        self.update_filter_can_assignment();
    }

    fn filter_message(&self, fifo: usize, msg: &mut CanMessage) -> bool {
        for &index in &self.fifo_filters[fifo] {
            let bank = &self.banks[index];
            if bank.belongs_to_master && bank.active && bank.match_message(msg) {
                return true;
            }
        }
        false
    }

    // ---- interrupt lines ----

    fn fifo_interrupt_enabled(&self, fifo: usize) -> bool {
        let rx = &self.rx[fifo];
        (self.ier & IER_FMPIE[fifo] != 0 && !rx.queue.is_empty())
            || (self.ier & IER_FFIE[fifo] != 0 && rx.full)
            || (self.ier & IER_FOVIE[fifo] != 0 && rx.overrun)
    }

    fn update_fifo_line(&self, ctx: &mut Ctx<'_>, fifo: usize) {
        ctx.set_output(if fifo == 0 { LINE_RX0 } else { LINE_RX1 }, self.fifo_interrupt_enabled(fifo));
    }

    fn update_transmit_line(&self, ctx: &mut Ctx<'_>) {
        let completed = TSR_RQCP.iter().any(|&bit| self.tsr & bit != 0);
        ctx.set_output(LINE_TX, self.ier & IER_TMEIE != 0 && completed);
    }

    fn sce_interrupt_enabled(&self) -> bool {
        let lec_pending = self.esr_lec > LEC_NO_ERROR && self.esr_lec < LEC_SET_BY_SOFTWARE;
        (self.ier & IER_ERRIE != 0 && self.msr & MSR_ERRI != 0)
            || (self.ier & IER_EWGIE != 0 && self.esr_flags & ESR_EWGF != 0)
            || (self.ier & IER_EPVIE != 0 && self.esr_flags & ESR_EPVF != 0)
            || (self.ier & IER_BOFIE != 0 && self.esr_flags & ESR_BOFF != 0)
            || (self.ier & IER_LECIE != 0 && lec_pending)
            || (self.ier & IER_SLKIE != 0 && self.msr & MSR_SLAKI != 0)
            || (self.ier & IER_WKUIE != 0 && self.msr & MSR_WKUI != 0)
    }

    fn update_sce_line(&self, ctx: &mut Ctx<'_>) {
        ctx.set_output(LINE_SCE, self.sce_interrupt_enabled());
    }

    // ---- receive / transmit ----

    /// `ReceiveCANMessage`.
    fn receive_message(&mut self, ctx: &mut Ctx<'_>, msg: CanMessage) {
        let fifo = msg.rx_fifo as usize;
        if fifo < NUMBER_OF_RX_FIFOS {
            let locked = self.mcr & MCR_RFLM != 0;
            self.rx[fifo].receive(msg, locked);
            self.update_fifo_line(ctx, fifo);
        }
    }

    /// `TransmitData`: `FrameSent`, the missing-subscriber dominant error, and loopback.
    fn transmit_data(&mut self, ctx: &mut Ctx<'_>, mut msg: CanMessage) {
        if self.btr & BTR_SILM == 0 {
            self.frames_sent += 1;
            if self.sink_attached {
                self.tx_out.push(TxFrame { time: ctx.now(), frame: msg.to_frame() });
            } else {
                // Renode parity: nobody listens -> bit dominant error.
                self.esr_lec = LEC_BIT_DOMINANT_ERROR;
                self.update_sce_line(ctx);
            }
        }
        if self.btr & BTR_LBKM != 0 {
            for fifo in 0..NUMBER_OF_RX_FIFOS {
                if self.filter_message(fifo, &mut msg) {
                    self.receive_message(ctx, msg.clone());
                }
            }
        }
    }

    fn write_ti(&mut self, ctx: &mut Ctx<'_>, mailbox: usize, value: u32) {
        self.ti[mailbox] = value & 0xFFFF_FFFE;
        if value & 1 != 0 {
            // registers.CAN_TDTxR = timestamp_me; FIXME (not implemented in Renode either)
            let msg = CanMessage::from_registers(self.ti[mailbox], self.tdt[mailbox], self.tdl[mailbox], self.tdh[mailbox]);
            self.transmit_data(ctx, msg);
            // Transmission done.
            self.tsr |= TSR_TME[mailbox] | TSR_TXOK[mailbox] | TSR_RQCP[mailbox];
            self.update_transmit_line(ctx);
        }
    }

    // ---- register access ----

    /// Value of the register at `offset`, `None` where Renode logs an unhandled access.
    fn read_register(&self, offset: u32) -> Option<u32> {
        if (offset::F0R1..=offset::F27R2).contains(&offset) {
            let bank = ((offset - offset::F0R1) / 8) as usize;
            let reg = (((offset - offset::F0R1) / 4) % 2) as usize;
            return Some(self.banks[bank].fr[reg]);
        }
        let fifo_register = |fifo: usize, pick: fn(&CanMessage) -> u32| -> u32 {
            self.rx[fifo].queue.front().map_or(0, pick)
        };
        Some(match offset {
            offset::MCR => self.mcr & MCR_MASK,
            offset::MSR => self.msr & MSR_MASK,
            offset::TSR => self.tsr,
            offset::RF0R => self.rx[0].value(),
            offset::RF1R => self.rx[1].value(),
            offset::IER => self.ier & IER_MASK,
            offset::ESR => self.esr_value(),
            // BTR is only readable (and writable) in initialization mode.
            offset::BTR => {
                if self.msr & MSR_INAK != 0 {
                    self.btr
                } else {
                    0
                }
            }
            offset::FMR => self.fmr,
            offset::FM1R => self.fm1r,
            offset::FS1R => self.fs1r,
            offset::FFA1R => self.ffa1r,
            offset::FA1R => self.fa1r,
            offset::TI0R => self.ti[0],
            offset::TDT0R => self.tdt[0],
            offset::TDL0R => self.tdl[0],
            offset::TDH0R => self.tdh[0],
            offset::TI1R => self.ti[1],
            offset::TDT1R => self.tdt[1],
            offset::TDL1R => self.tdl[1],
            offset::TDH1R => self.tdh[1],
            offset::TI2R => self.ti[2],
            offset::TDT2R => self.tdt[2],
            offset::TDL2R => self.tdl[2],
            offset::TDH2R => self.tdh[2],
            offset::RI0R => fifo_register(0, |m| m.rir),
            offset::RDT0R => fifo_register(0, |m| m.rdtr),
            offset::RL0R => fifo_register(0, |m| m.rlr),
            offset::RH0R => fifo_register(0, |m| m.rhr),
            offset::RI1R => fifo_register(1, |m| m.rir),
            offset::RDT1R => fifo_register(1, |m| m.rdtr),
            offset::RL1R => fifo_register(1, |m| m.rlr),
            offset::RH1R => fifo_register(1, |m| m.rhr),
            _ => return None,
        })
    }

    /// `ReceiveFifoRegister.SetValue` (RFxR write): FULL/FOVR are write-1-to-clear, RFOM releases the head.
    fn write_rfr(&mut self, ctx: &mut Ctx<'_>, fifo: usize, value: u32) {
        let rx = &mut self.rx[fifo];
        if value & RFR_FULL != 0 {
            rx.full = false;
        }
        if value & RFR_FOVR != 0 {
            rx.overrun = false;
        }
        if value & RFR_RFOM != 0 {
            rx.queue.pop_front();
        }
        self.update_fifo_line(ctx, fifo);
    }

    /// `TransmitStatusRegister.SetValue`.
    fn write_tsr(&mut self, value: u32) {
        let mut reg = self.tsr;
        for mailbox in (0..3).rev() {
            if value & TSR_TERR[mailbox] != 0 {
                // Renode parity: the mailbox 0 branch clears TERR1 instead of TERR0.
                reg &= !(if mailbox == 0 { TSR_TERR[1] } else { TSR_TERR[mailbox] });
            }
            if value & TSR_ALST[mailbox] != 0 {
                reg &= !TSR_ALST[mailbox];
            }
            if value & TSR_TXOK[mailbox] != 0 {
                reg &= !TSR_TXOK[mailbox];
            }
        }
        for mailbox in 0..3 {
            if value & TSR_RQCP[mailbox] != 0 {
                reg &= !(TSR_TXOK[mailbox] | TSR_ALST[mailbox] | TSR_TERR[mailbox] | TSR_RQCP[mailbox]);
            }
        }
        self.tsr = reg;
    }

    fn write_mcr(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        self.mcr = value & MCR_MASK;
        self.mcr_reset_request = value & MCR_RESET != 0;
        let init_request = self.mcr & MCR_INRQ != 0;
        let sleep_request = self.mcr & MCR_SLEEP != 0;
        if init_request && !sleep_request {
            // Enter initialization mode.
            self.msr |= MSR_INAK;
            self.msr &= !MSR_SLAK;
        } else if sleep_request && !init_request {
            // Enter sleep mode.
            self.msr |= MSR_SLAK | MSR_SLAKI;
            self.msr &= !MSR_INAK;
            self.update_sce_line(ctx);
        } else {
            // Enter normal mode. Renode parity: INRQ together with SLEEP lands here too.
            self.msr &= !(MSR_SLAK | MSR_INAK);
        }
        if self.mcr_reset_request {
            self.reset_registers(Some(ctx), true);
        }
    }

    fn write_register(&mut self, ctx: &mut Ctx<'_>, offset: u32, value: u32) -> bool {
        // Filter bank registers: writable while FINIT is set or the bank is inactive.
        if (offset::F0R1..=offset::F27R2).contains(&offset) {
            let bank = ((offset - offset::F0R1) / 8) as usize;
            if self.fmr & FMR_FINIT != 0 || self.fa1r & (1 << bank) == 0 {
                let reg = (((offset - offset::F0R1) / 4) % 2) as usize;
                self.banks[bank].fr[reg] = value;
            }
            return true;
        }
        match offset {
            offset::MSR => {
                // SLAKI, WKUI and ERRI are write-1-to-clear.
                self.msr &= !(value & (MSR_SLAKI | MSR_WKUI | MSR_ERRI));
                self.update_sce_line(ctx);
            }
            offset::MCR => self.write_mcr(ctx, value),
            offset::TSR => {
                self.write_tsr(value);
                self.update_transmit_line(ctx);
            }
            offset::RF0R => self.write_rfr(ctx, 0, value),
            offset::RF1R => self.write_rfr(ctx, 1, value),
            offset::IER => {
                self.ier = value & IER_MASK;
                self.update_transmit_line(ctx);
                self.update_fifo_line(ctx, 0);
                self.update_fifo_line(ctx, 1);
                self.update_sce_line(ctx);
            }
            offset::ESR => {
                self.esr_lec = (value >> 4) & 0x7;
                self.esr_tec = (value >> 16) & 0xFF;
                self.esr_rec = (value >> 24) & 0xFF;
                self.update_sce_line(ctx);
            }
            offset::BTR => {
                if self.msr & MSR_INAK != 0 {
                    self.btr = value;
                }
            }
            offset::FMR => {
                self.fmr = value;
                self.update_filter_can_assignment();
            }
            offset::FM1R => {
                if self.fmr & FMR_FINIT != 0 {
                    self.fm1r = value;
                    for (i, bank) in self.banks.iter_mut().enumerate() {
                        bank.list_mode = value & (1 << i) != 0;
                    }
                }
            }
            offset::FS1R => {
                if self.fmr & FMR_FINIT != 0 {
                    self.fs1r = value;
                    for (i, bank) in self.banks.iter_mut().enumerate() {
                        bank.scale32 = value & (1 << i) != 0;
                    }
                }
            }
            offset::FFA1R => {
                if self.fmr & FMR_FINIT != 0 {
                    self.ffa1r = value;
                    for (i, bank) in self.banks.iter_mut().enumerate() {
                        bank.fifo = u32::from(value & (1 << i) != 0);
                    }
                }
            }
            offset::FA1R => {
                self.fa1r = value & FA1R_MASK;
                for (i, bank) in self.banks.iter_mut().enumerate() {
                    bank.active = self.fa1r & (1 << i) != 0;
                }
                for fifo in 0..NUMBER_OF_RX_FIFOS {
                    self.prioritize_fifo_filters(fifo);
                }
            }
            offset::TI0R => self.write_ti(ctx, 0, value),
            offset::TDT0R => self.tdt[0] = value,
            offset::TDL0R => self.tdl[0] = value,
            offset::TDH0R => self.tdh[0] = value,
            offset::TI1R => self.write_ti(ctx, 1, value),
            offset::TDT1R => self.tdt[1] = value,
            offset::TDL1R => self.tdl[1] = value,
            offset::TDH1R => self.tdh[1] = value,
            offset::TI2R => self.write_ti(ctx, 2, value),
            offset::TDT2R => self.tdt[2] = value,
            offset::TDL2R => self.tdl[2] = value,
            offset::TDH2R => self.tdh[2] = value,
            _ => return false,
        }
        true
    }
}

impl Peripheral for StmCan {
    fn name(&self) -> &str {
        &self.name
    }

    /// `IDoubleWordPeripheral` without `[AllowedTranslations]`: byte and halfword accesses are refused.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.reset_registers(Some(ctx), true);
        self.reset_filter_banks();
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.read_register(offset) {
            Some(value) => value,
            None => {
                ctx.warn_once(u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}."));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        if !self.write_register(ctx, offset, value) {
            let key = (1u64 << 63) | (u64::from(offset) << 32) | u64::from(value);
            ctx.warn_once(key, format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}."));
        }
    }

    /// Every register read of this model is free of side effects, so `peek` is the read.
    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        self.read_register(offset)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        format!(
            "{}: MCR=0x{:08X} MSR=0x{:08X} TSR=0x{:08X} IER=0x{:05X} sent={} received={} fifo0={} fifo1={} sinkAttached={}",
            self.name,
            self.mcr & MCR_MASK,
            self.msr,
            self.tsr,
            self.ier,
            self.frames_sent,
            self.frames_received,
            self.rx[0].queue.len(),
            self.rx[1].queue.len(),
            self.sink_attached
        )
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{from_micros, PeriphId};

    const BASE: u32 = 0x4000_6400;
    const NVIC_TX: u32 = 19;
    const NVIC_RX0: u32 = 20;
    const NVIC_RX1: u32 = 21;
    const NVIC_SCE: u32 = 22;

    fn rig() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, StmCan::new("can1"));
        for (line, irq) in [(LINE_TX, NVIC_TX), (LINE_RX0, NVIC_RX0), (LINE_RX1, NVIC_RX1), (LINE_SCE, NVIC_SCE)] {
            h.connect_irq(id, line, irq);
        }
        h.clear_irq_changes();
        (h, id)
    }

    fn reg(off: u32) -> u32 {
        BASE + off
    }

    /// `sysbus WriteDoubleWord 0x40006400 0x10000` of the `.resc` files.
    fn renode_boot(h: &mut Harness) {
        h.write32(reg(offset::MCR), 0x0001_0000);
    }

    fn attach(h: &mut Harness, id: PeriphId) {
        h.get_mut::<StmCan>(id).set_frame_sink_attached(true);
    }

    fn send(h: &mut Harness, mailbox: u32, ti: u32, dlc: u32, low: u32, high: u32) {
        let base = offset::TI0R + 0x10 * mailbox;
        h.write32(reg(base + 4), dlc);
        h.write32(reg(base + 8), low);
        h.write32(reg(base + 12), high);
        h.write32(reg(base), ti | 1);
    }

    fn deliver(h: &mut Harness, id: PeriphId, frame: CanFrame) {
        h.with::<StmCan, _>(id, |can, ctx| can.deliver_frame(ctx, &frame));
    }

    fn rx_head(h: &mut Harness, fifo: u32) -> [u32; 4] {
        let base = offset::RI0R + 0x10 * fifo;
        [h.read32(reg(base)), h.read32(reg(base + 4)), h.read32(reg(base + 8)), h.read32(reg(base + 12))]
    }

    /// HAL_CAN_ConfigFilter sequence.
    #[allow(clippy::too_many_arguments)]
    fn config_filter(h: &mut Harness, bank: u32, fifo: u32, scale32: bool, list: bool, fr1: u32, fr2: u32, activate: bool) {
        let bit = 1u32 << bank;
        let fmr = h.read32(reg(offset::FMR));
        h.write32(reg(offset::FMR), fmr | 1);
        let fa1r = h.read32(reg(offset::FA1R));
        h.write32(reg(offset::FA1R), fa1r & !bit);
        for (off, set) in [(offset::FS1R, scale32), (offset::FM1R, list), (offset::FFA1R, fifo == 1)] {
            let value = h.read32(reg(off));
            h.write32(reg(off), if set { value | bit } else { value & !bit });
        }
        h.write32(reg(offset::F0R1 + 8 * bank), fr1);
        h.write32(reg(offset::F0R1 + 8 * bank + 4), fr2);
        if activate {
            let fa1r = h.read32(reg(offset::FA1R));
            h.write32(reg(offset::FA1R), fa1r | bit);
        }
        h.write32(reg(offset::FMR), fmr & !1);
    }

    fn accept_all(h: &mut Harness) {
        config_filter(h, 0, 0, true, false, 0, 0, true);
    }

    /// Delivers each frame and releases it again, returning the identifiers the filters accepted.
    fn accepted_ids(h: &mut Harness, id: PeriphId, frames: Vec<CanFrame>) -> Vec<u32> {
        let mut accepted = Vec::new();
        for frame in frames {
            let before = h.get::<StmCan>(id).fifo_len(0);
            let std_id = frame.id;
            deliver(h, id, frame);
            if h.get::<StmCan>(id).fifo_len(0) > before {
                accepted.push(std_id);
                h.write32(reg(offset::RF0R), RFR_RFOM);
            }
        }
        accepted
    }

    fn ids_in_fifo0(h: &mut Harness, count: usize) -> Vec<u32> {
        // Pops `count` messages through RFOM and reports the standard id of each head.
        let mut ids = Vec::new();
        for _ in 0..count {
            let rir = h.read32(reg(offset::RI0R));
            ids.push(rir >> 21);
            h.write32(reg(offset::RF0R), RFR_RFOM);
        }
        ids
    }

    #[test]
    fn reset_values_and_btr_gating() {
        let (mut h, id) = rig();
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0002);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C02);
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C00_0000);
        assert_eq!(h.read32(reg(offset::FMR)), 0x2A1C_0E01);
        for off in [
            offset::RF0R,
            offset::RF1R,
            offset::IER,
            offset::ESR,
            offset::BTR,
            offset::FM1R,
            offset::FS1R,
            offset::FFA1R,
            offset::FA1R,
            offset::TI0R,
            offset::TDT1R,
            offset::TDL2R,
            offset::TDH0R,
            offset::RI0R,
            offset::RDT0R,
            offset::RL1R,
            offset::RH1R,
            offset::F0R1,
            offset::F27R2,
        ] {
            assert_eq!(h.read32(reg(off)), 0, "register 0x{off:X}");
        }
        // BTR only reads back in initialization mode.
        h.write32(reg(offset::MCR), 0x0001_0001);
        assert_eq!(h.read32(reg(offset::MSR)) & MSR_INAK, 1);
        assert_eq!(h.read32(reg(offset::BTR)), 0x0123_0000);
        // peek is the read.
        for off in [offset::MCR, offset::MSR, offset::TSR, offset::BTR, offset::FMR] {
            assert_eq!(h.peek(reg(off), Width::Word), Some(h.read32(reg(off))));
        }
        assert!(h.warnings().is_empty());
        assert!(h.get::<StmCan>(id).tx_frames().is_empty());
    }

    #[test]
    fn init_and_sleep_handshake_follows_renode() {
        let (mut h, _id) = rig();
        // The fixture the boards apply before execution.
        renode_boot(&mut h);
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0000);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C00, "normal mode: SLAK and INAK clear");
        h.write32(reg(offset::MCR), 0x0001_0001);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C01, "INRQ alone: INAK");
        // Renode parity: INRQ together with SLEEP is neither init nor sleep -> normal mode.
        h.write32(reg(offset::MCR), 0x0001_0003);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C00);
        h.write32(reg(offset::MCR), 0x0001_0002);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C12, "SLEEP alone: SLAK and the SLAKI flag");
        h.write32(reg(offset::MSR), 1 << 4);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C02, "SLAKI is write-1-to-clear");
    }

    #[test]
    fn why_the_boards_pre_write_mcr() {
        // From reset (MCR = 0x00010002, SLEEP set) a HAL that sets INRQ without clearing SLEEP first writes
        // INRQ|SLEEP; Renode answers with normal mode and INAK never rises.
        let (mut h, _id) = rig();
        let mcr = h.read32(reg(offset::MCR));
        h.write32(reg(offset::MCR), mcr | MCR_INRQ);
        assert_eq!(h.read32(reg(offset::MSR)) & MSR_INAK, 0);
        // The documented HAL order (leave sleep, then request init) works.
        let (mut h, _id) = rig();
        let mcr = h.read32(reg(offset::MCR));
        h.write32(reg(offset::MCR), mcr & !MCR_SLEEP);
        assert_eq!(h.read32(reg(offset::MSR)) & MSR_SLAK, 0);
        let mcr = h.read32(reg(offset::MCR));
        h.write32(reg(offset::MCR), mcr | MCR_INRQ);
        assert_eq!(h.read32(reg(offset::MSR)) & MSR_INAK, MSR_INAK);
        // BTR is accepted in init mode and ignored outside it.
        h.write32(reg(offset::BTR), 0x001E_0003);
        assert_eq!(h.read32(reg(offset::BTR)), 0x001E_0003);
        h.write32(reg(offset::MCR), 0x0001_0000);
        h.write32(reg(offset::BTR), 0x4000_0000);
        h.write32(reg(offset::MCR), 0x0001_0001);
        assert_eq!(h.read32(reg(offset::BTR)), 0x001E_0003, "write outside init mode was ignored");
    }

    #[test]
    fn sleep_acknowledge_raises_the_sce_interrupt_when_enabled() {
        let (mut h, _id) = rig();
        h.write32(reg(offset::IER), IER_SLKIE);
        assert!(!h.irq_level(NVIC_SCE));
        h.write32(reg(offset::MCR), 0x0001_0002);
        assert!(h.irq_level(NVIC_SCE));
        h.write32(reg(offset::MSR), MSR_SLAKI);
        assert!(!h.irq_level(NVIC_SCE));
    }

    #[test]
    fn transmit_emits_frames_synchronously_in_write_order() {
        let (mut h, id) = rig();
        attach(&mut h, id);
        renode_boot(&mut h);
        h.advance_to(from_micros(100));
        send(&mut h, 0, 0x103 << 21, 2, 0x0201, 0);
        assert_eq!(h.get::<StmCan>(id).tx_frames().len(), 1, "the frame is out at the register write");
        h.advance_to(from_micros(250));
        // The mailbox number does not influence the order (no arbitration, TXFP is stored only).
        send(&mut h, 2, 0x089 << 21, 4, 0x0203_4100, 0);
        send(&mut h, 1, 0x173 << 21, 0, 0, 0);
        let frames = h.get_mut::<StmCan>(id).take_tx_frames();
        assert_eq!(
            frames,
            vec![
                TxFrame { time: from_micros(100), frame: CanFrame::standard(0x103, &[1, 2]) },
                TxFrame { time: from_micros(250), frame: CanFrame::standard(0x089, &[0x00, 0x41, 0x03, 0x02]) },
                TxFrame { time: from_micros(250), frame: CanFrame::standard(0x173, &[]) },
            ]
        );
        assert!(h.get::<StmCan>(id).tx_frames().is_empty(), "drained");
        assert_eq!(h.get::<StmCan>(id).frames_sent(), 3);
        // Mailboxes complete immediately: TME stays set, TXOK and RQCP are raised per mailbox.
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C03_0303);
        // TI reads back without TXRQ.
        assert_eq!(h.read32(reg(offset::TI2R)), 0x089 << 21);
        // RQCPx is write-1-to-clear and takes TXOKx with it.
        h.write32(reg(offset::TSR), 1);
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C03_0300, "RQCP0 took TXOK0 with it");
        h.write32(reg(offset::TSR), 1 << 9);
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C03_0100, "TXOK1 clears on its own, RQCP1 stays");
        h.write32(reg(offset::TSR), (1 << 8) | (1 << 16));
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C00_0000);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn transmit_frame_formats() {
        let (mut h, id) = rig();
        attach(&mut h, id);
        renode_boot(&mut h);
        // Extended + remote: id 0x12345678.
        send(&mut h, 0, (0x1234_5678u32 << 3) | 0b110, 0, 0, 0);
        // DLC 15: only the first eight bytes come from TDL/TDH, the rest is zero; TDT's other bits are masked.
        send(&mut h, 1, 0x7FF << 21, 0x1234_000F, 0x0403_0201, 0x0807_0605);
        let frames = h.get_mut::<StmCan>(id).take_tx_frames();
        assert_eq!(frames[0].frame, CanFrame { id: 0x1234_5678, data: vec![], extended: true, remote: true, fd: false, bit_rate_switch: false });
        assert_eq!(frames[1].frame, CanFrame::standard(0x7FF, &[1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0]));
        // A partial high word: DLC 5 -> five bytes.
        send(&mut h, 2, 0x001 << 21, 5, 0x4433_2211, 0xAABB_CC55);
        assert_eq!(h.get_mut::<StmCan>(id).take_tx_frames()[0].frame.data, vec![0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn transmit_complete_interrupt_follows_ier_and_tsr() {
        let (mut h, id) = rig();
        attach(&mut h, id);
        renode_boot(&mut h);
        h.write32(reg(offset::IER), IER_TMEIE);
        assert!(!h.irq_level(NVIC_TX));
        send(&mut h, 0, 0x100 << 21, 0, 0, 0);
        assert!(h.irq_level(NVIC_TX));
        h.write32(reg(offset::TSR), TSR_RQCP[0]);
        assert!(!h.irq_level(NVIC_TX));
        // Enabling the interrupt while a completion is pending raises the line.
        h.write32(reg(offset::IER), 0);
        send(&mut h, 1, 0x100 << 21, 0, 0, 0);
        h.write32(reg(offset::IER), IER_TMEIE);
        assert!(h.irq_level(NVIC_TX));
        h.write32(reg(offset::TSR), TSR_RQCP[1]);
        assert!(!h.irq_level(NVIC_TX));
    }

    #[test]
    fn transmit_without_a_listener_sets_the_bit_dominant_error() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        h.write32(reg(offset::IER), IER_LECIE);
        send(&mut h, 0, 0x103 << 21, 0, 0, 0);
        assert!(h.get::<StmCan>(id).tx_frames().is_empty());
        assert_eq!(h.read32(reg(offset::ESR)), 0x50, "LEC = 5");
        assert!(h.irq_level(NVIC_SCE));
        // The mailbox still completes.
        assert_eq!(h.read32(reg(offset::TSR)) & TSR_RQCP[0], 1);
        // Software clears the error by writing LEC.
        h.write32(reg(offset::ESR), 0);
        assert_eq!(h.read32(reg(offset::ESR)), 0);
        assert!(!h.irq_level(NVIC_SCE));
        // With a listener attached nothing goes wrong.
        attach(&mut h, id);
        send(&mut h, 1, 0x103 << 21, 0, 0, 0);
        assert_eq!(h.get::<StmCan>(id).tx_frames().len(), 1);
        assert_eq!(h.read32(reg(offset::ESR)), 0);
        // LEC = 7 (set by software) is not a pending error.
        h.write32(reg(offset::ESR), 7 << 4);
        assert!(!h.irq_level(NVIC_SCE));
    }

    #[test]
    fn silent_mode_neither_sends_nor_complains() {
        let (mut h, id) = rig();
        h.write32(reg(offset::MCR), 0x0001_0001);
        h.write32(reg(offset::BTR), BTR_SILM | 0x0123_0000);
        h.write32(reg(offset::MCR), 0x0001_0000);
        send(&mut h, 0, 0x103 << 21, 0, 0, 0);
        assert!(h.get::<StmCan>(id).tx_frames().is_empty());
        assert_eq!(h.read32(reg(offset::ESR)), 0);
        assert_eq!(h.get::<StmCan>(id).frames_sent(), 0);
    }

    #[test]
    fn accept_all_filter_receives_standard_and_extended_frames() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        deliver(&mut h, id, CanFrame::standard(0x123, &[1, 2, 3]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
        // RIR = id << 21; RDTR carries only the DLC (no timestamp, no filter match index); RLR/RHR the data.
        assert_eq!(rx_head(&mut h, 0), [0x123 << 21, 3, 0x0003_0201, 0]);
        deliver(&mut h, id, CanFrame::extended(0x1ABC_DE5, &[0xAA; 8]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 2);
        h.write32(reg(offset::RF0R), RFR_RFOM);
        assert_eq!(rx_head(&mut h, 0), [0x0D5E_6F2C, 8, 0xAAAA_AAAA, 0xAAAA_AAAA]);
        deliver(&mut h, id, CanFrame { id: 0x7, data: vec![], remote: true, ..CanFrame::default() });
        h.write32(reg(offset::RF0R), RFR_RFOM);
        assert_eq!(rx_head(&mut h, 0), [(0x7 << 21) | 0b10, 0, 0, 0]);
        assert_eq!(h.get::<StmCan>(id).frames_received(), 3);
    }

    #[test]
    fn frames_are_dropped_until_a_filter_is_active() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        deliver(&mut h, id, CanFrame::standard(0x1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0, "no FA1R write yet: nothing prioritized");
        config_filter(&mut h, 0, 0, true, false, 0, 0, false);
        deliver(&mut h, id, CanFrame::standard(0x1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0, "bank configured but inactive");
        let fa1r = h.read32(reg(offset::FA1R));
        h.write32(reg(offset::FA1R), fa1r | 1);
        deliver(&mut h, id, CanFrame::standard(0x1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
    }

    #[test]
    fn mask_mode_32_bit_matches_identifier_bits_selected_by_the_mask() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        config_filter(&mut h, 0, 0, true, false, 0x123 << 21, 0x7FF << 21, true);
        for frame in [
            CanFrame::standard(0x123, &[]),
            CanFrame::standard(0x124, &[]),
            // The mask has no IDE bit, so an extended frame with the same top 11 bits matches too.
            CanFrame::extended((0x123 << 18) | 5, &[]),
            CanFrame::extended(0x124 << 18, &[]),
        ] {
            deliver(&mut h, id, frame);
        }
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 2);
        assert_eq!(ids_in_fifo0(&mut h, 2), vec![0x123, 0x123 /* extended: RIR top bits */]);
    }

    #[test]
    fn mask_mode_can_require_ide_and_rtr() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        // Identifier 0x100 standard data frame; mask: STID, IDE and RTR.
        config_filter(&mut h, 0, 0, true, false, 0x100 << 21, (0x7FF << 21) | 0b110, true);
        deliver(&mut h, id, CanFrame::standard(0x100, &[]));
        deliver(&mut h, id, CanFrame::extended(0x100 << 18, &[]));
        deliver(&mut h, id, CanFrame { id: 0x100, remote: true, ..CanFrame::default() });
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
    }

    #[test]
    fn list_mode_32_bit_matches_two_exact_identifiers() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        config_filter(&mut h, 0, 0, true, true, 0x100 << 21, 0x200 << 21, true);
        for frame in [
            CanFrame::standard(0x100, &[]),
            CanFrame::standard(0x200, &[]),
            CanFrame::standard(0x101, &[]),
            CanFrame { id: 0x100, remote: true, ..CanFrame::default() },
            CanFrame::extended(0x100 << 18, &[]),
        ] {
            deliver(&mut h, id, frame);
        }
        assert_eq!(ids_in_fifo0(&mut h, 2), vec![0x100, 0x200]);
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
    }

    #[test]
    fn list_mode_16_bit_matches_four_identifiers() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        // Renode decodes the high half of FRx as the first filter and the low half as the second.
        config_filter(&mut h, 0, 0, false, true, (0x111 << 21) | (0x222 << 5), (0x333 << 21) | (0x444 << 5), true);
        let frames = [0x111, 0x222, 0x333, 0x444, 0x555, 0x110].iter().map(|&i| CanFrame::standard(i, &[])).collect();
        assert_eq!(accepted_ids(&mut h, id, frames), vec![0x111, 0x222, 0x333, 0x444]);
    }

    #[test]
    fn mask_mode_16_bit_only_evaluates_the_first_pair() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        // Pair 0: identifier 0x300 (high half of FR1), mask 0x7FF (low half). Pair 1 (FR2) would accept 0x400
        // on hardware, but Renode's loop never gets to it.
        config_filter(&mut h, 0, 0, false, false, (0x300 << 21) | (0x7FF << 5), (0x400 << 21) | (0x7FF << 5), true);
        let frames = [0x300, 0x400, 0x301].iter().map(|&i| CanFrame::standard(i, &[])).collect();
        assert_eq!(accepted_ids(&mut h, id, frames), vec![0x300]);
    }

    #[test]
    fn fifo_assignment_selects_the_fifo_and_its_interrupt() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        h.write32(reg(offset::IER), IER_FMPIE[0] | IER_FMPIE[1]);
        config_filter(&mut h, 1, 1, true, false, 0, 0, true);
        deliver(&mut h, id, CanFrame::standard(0x42, &[9]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
        assert_eq!(h.get::<StmCan>(id).fifo_len(1), 1);
        assert!(h.irq_level(NVIC_RX1));
        assert!(!h.irq_level(NVIC_RX0));
        assert_eq!(rx_head(&mut h, 1), [0x42 << 21, 1, 9, 0]);
        h.write32(reg(offset::RF1R), RFR_RFOM);
        assert!(!h.irq_level(NVIC_RX1));
    }

    #[test]
    fn stale_fifo_membership_quirk() {
        // FA1R snapshots which banks sit in which FIFO's list; changing FFA1R afterwards leaves the old list in
        // place, but a match stores the message in the bank's *current* FIFO. Renode parity.
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::FMR), 0x2A1C_0E01);
        h.write32(reg(offset::FFA1R), 1);
        h.write32(reg(offset::FMR), 0x2A1C_0E00);
        deliver(&mut h, id, CanFrame::standard(0x10, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
        assert_eq!(h.get::<StmCan>(id).fifo_len(1), 1);
    }

    #[test]
    fn filter_registers_are_write_protected_for_active_banks_outside_init_mode() {
        let (mut h, _id) = rig();
        accept_all(&mut h); // leaves FINIT clear, bank 0 active
        h.write32(reg(offset::F0R1), 0x1234_5678);
        assert_eq!(h.read32(reg(offset::F0R1)), 0);
        h.write32(reg(offset::F0R1 + 8), 0x1111_1111); // bank 1 is inactive: writable
        assert_eq!(h.read32(reg(offset::F0R1 + 8)), 0x1111_1111);
        // FM1R/FS1R/FFA1R are only writable in filter init mode.
        h.write32(reg(offset::FM1R), 0xFFFF);
        assert_eq!(h.read32(reg(offset::FM1R)), 0);
        let fmr = h.read32(reg(offset::FMR));
        h.write32(reg(offset::FMR), fmr | 1);
        h.write32(reg(offset::FM1R), 0xFFFF);
        assert_eq!(h.read32(reg(offset::FM1R)), 0xFFFF);
        // FA1R keeps 28 bits.
        h.write32(reg(offset::FA1R), 0xFFFF_FFFF);
        assert_eq!(h.read32(reg(offset::FA1R)), 0x0FFF_FFFF);
        // With FINIT set every bank register can be written.
        h.write32(reg(offset::F0R1), 0x1234_5678);
        assert_eq!(h.read32(reg(offset::F0R1)), 0x1234_5678);
    }

    #[test]
    fn banks_at_or_above_can2_start_bank_belong_to_the_other_controller() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        config_filter(&mut h, 20, 0, true, false, 0, 0, true);
        deliver(&mut h, id, CanFrame::standard(0x1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0, "bank 20 >= CAN2SB 14");
        // Moving the split to 28 hands every bank to this controller (FMR bits 13:8).
        h.write32(reg(offset::FMR), (28 << 8) | (0x2A1C_0000 & !0x3F00));
        assert_eq!(h.read32(reg(offset::FMR)) >> 8 & 0x3F, 28);
        deliver(&mut h, id, CanFrame::standard(0x1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
    }

    #[test]
    fn fifo_depth_overrun_release_and_interrupt_lines() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::IER), IER_FMPIE[0]);
        for std_id in 1..=3 {
            deliver(&mut h, id, CanFrame::standard(std_id, &[]));
        }
        assert_eq!(h.read32(reg(offset::RF0R)), 0x0B, "FULL | 3 messages");
        assert!(h.irq_level(NVIC_RX0));
        // Unlocked mode: the fourth frame replaces the oldest and raises FOVR.
        deliver(&mut h, id, CanFrame::standard(4, &[]));
        assert_eq!(h.read32(reg(offset::RF0R)), 0x1B);
        assert_eq!(rx_head(&mut h, 0)[0] >> 21, 2);
        h.write32(reg(offset::RF0R), RFR_RFOM);
        assert_eq!(h.read32(reg(offset::RF0R)), 0x1A);
        assert_eq!(rx_head(&mut h, 0)[0] >> 21, 3);
        // FULL and FOVR clear on write-1; they do not touch the queue.
        h.write32(reg(offset::RF0R), RFR_FULL | RFR_FOVR);
        assert_eq!(h.read32(reg(offset::RF0R)), 0x02);
        h.write32(reg(offset::RF0R), RFR_RFOM);
        h.write32(reg(offset::RF0R), RFR_RFOM);
        assert_eq!(h.read32(reg(offset::RF0R)), 0);
        assert!(!h.irq_level(NVIC_RX0));
        assert_eq!(rx_head(&mut h, 0), [0, 0, 0, 0], "an empty FIFO reads zero");
        h.write32(reg(offset::RF0R), RFR_RFOM); // releasing an empty FIFO is harmless
        assert_eq!(h.read32(reg(offset::RF0R)), 0);
    }

    #[test]
    fn locked_fifo_keeps_the_oldest_messages() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::MCR), 0x0001_0000 | MCR_RFLM);
        for std_id in 1..=4 {
            deliver(&mut h, id, CanFrame::standard(std_id, &[]));
        }
        assert_eq!(h.read32(reg(offset::RF0R)), 0x1B, "overrun flagged, three messages kept");
        assert_eq!(ids_in_fifo0(&mut h, 3), vec![1, 2, 3]);
    }

    #[test]
    fn fifo_full_and_overrun_interrupts_are_independent() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::IER), IER_FFIE[0]);
        for std_id in 1..=2 {
            deliver(&mut h, id, CanFrame::standard(std_id, &[]));
        }
        assert!(!h.irq_level(NVIC_RX0), "FFIE0 only reacts to FULL");
        deliver(&mut h, id, CanFrame::standard(3, &[]));
        assert!(h.irq_level(NVIC_RX0));
        h.write32(reg(offset::RF0R), RFR_FULL);
        assert!(!h.irq_level(NVIC_RX0));
        h.write32(reg(offset::IER), IER_FOVIE[0]);
        assert!(!h.irq_level(NVIC_RX0));
        deliver(&mut h, id, CanFrame::standard(4, &[]));
        assert!(h.irq_level(NVIC_RX0), "FOVIE0 follows the overrun flag");
        h.write32(reg(offset::RF0R), RFR_FOVR);
        assert!(!h.irq_level(NVIC_RX0));
    }

    #[test]
    fn loopback_mode_feeds_transmitted_frames_through_the_filters() {
        let (mut h, id) = rig();
        attach(&mut h, id);
        h.write32(reg(offset::MCR), 0x0001_0001);
        h.write32(reg(offset::BTR), BTR_LBKM | 0x0123_0000);
        h.write32(reg(offset::MCR), 0x0001_0000);
        accept_all(&mut h);
        send(&mut h, 0, 0x2A5 << 21, 2, 0xBEEF, 0);
        assert_eq!(h.get::<StmCan>(id).tx_frames().len(), 1, "not silent: the frame also goes out");
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
        assert_eq!(rx_head(&mut h, 0), [0x2A5 << 21, 2, 0xBEEF, 0]);
        // In loopback mode frames from the bus are ignored.
        deliver(&mut h, id, CanFrame::standard(1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
        // Silent + loopback: nothing goes out and nothing is flagged, the FIFO still receives.
        h.write32(reg(offset::MCR), 0x0001_0001);
        h.write32(reg(offset::BTR), BTR_LBKM | BTR_SILM | 0x0123_0000);
        h.write32(reg(offset::MCR), 0x0001_0000);
        send(&mut h, 1, 0x2A6 << 21, 0, 0, 0);
        assert_eq!(h.get::<StmCan>(id).tx_frames().len(), 1);
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 2);
        assert_eq!(h.read32(reg(offset::ESR)), 0);
    }

    #[test]
    fn frames_arriving_in_sleep_mode_only_signal_wakeup() {
        let (mut h, id) = rig();
        accept_all(&mut h);
        h.write32(reg(offset::IER), IER_WKUIE);
        h.write32(reg(offset::MCR), 0x0001_0002);
        deliver(&mut h, id, CanFrame::standard(1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
        assert_eq!(h.read32(reg(offset::MSR)) & MSR_WKUI, MSR_WKUI);
        assert!(h.irq_level(NVIC_SCE));
        assert_eq!(h.read32(reg(offset::MCR)) & MCR_SLEEP, MCR_SLEEP, "no AWUM: still asleep");
        h.write32(reg(offset::MSR), MSR_WKUI);
        assert!(!h.irq_level(NVIC_SCE));
        // Automatic wake-up leaves sleep mode (the frame itself is lost).
        h.write32(reg(offset::MCR), 0x0001_0000 | MCR_AWUM | MCR_SLEEP);
        deliver(&mut h, id, CanFrame::standard(1, &[]));
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0000 | MCR_AWUM);
        assert_eq!(h.read32(reg(offset::MSR)) & (MSR_SLAK | MSR_WKUI), MSR_WKUI);
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
    }

    #[test]
    fn mcr_reset_restores_registers_but_not_fifos_or_lines() {
        let (mut h, id) = rig();
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::IER), IER_FMPIE[0]);
        deliver(&mut h, id, CanFrame::standard(5, &[]));
        assert!(h.irq_level(NVIC_RX0));
        h.write32(reg(offset::MCR), 0x0001_0000 | MCR_RESET);
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0002);
        assert_eq!(h.read32(reg(offset::MSR)), 0x0000_0C02);
        assert_eq!(h.read32(reg(offset::IER)), 0);
        assert_eq!(h.read32(reg(offset::FMR)), 0x2A1C_0E01);
        assert_eq!(h.read32(reg(offset::FA1R)), 0);
        // The FIFO line was re-evaluated with the old IER before it was cleared, and is not re-evaluated after.
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 1);
        assert!(h.irq_level(NVIC_RX0));
        // The next IER write recomputes it.
        h.write32(reg(offset::IER), 0);
        assert!(!h.irq_level(NVIC_RX0));
    }

    #[test]
    fn peripheral_reset_matches_the_constructor_state() {
        let (mut h, id) = rig();
        attach(&mut h, id);
        renode_boot(&mut h);
        accept_all(&mut h);
        h.write32(reg(offset::IER), IER_TMEIE | IER_LECIE);
        send(&mut h, 0, 0x1 << 21, 0, 0, 0);
        assert!(h.irq_level(NVIC_TX));
        h.core_mut().reset_all();
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0002);
        assert_eq!(h.read32(reg(offset::TSR)), 0x1C00_0000);
        assert_eq!(h.read32(reg(offset::IER)), 0);
        assert_eq!(h.read32(reg(offset::FA1R)), 0);
        // Renode's Reset() does not re-evaluate the TX line either; the next IER/TSR write does.
        assert!(h.irq_level(NVIC_TX));
        h.write32(reg(offset::IER), 0);
        assert!(!h.irq_level(NVIC_TX));
        // Bank configuration is reset (inactive), the stored FR value is not.
        deliver(&mut h, id, CanFrame::standard(1, &[]));
        assert_eq!(h.get::<StmCan>(id).fifo_len(0), 0);
    }

    #[test]
    fn unhandled_offsets_and_unsupported_widths_log_like_renode() {
        let (mut h, id) = rig();
        assert_eq!(h.read32(reg(0x20)), 0);
        h.write32(reg(0x24), 5);
        assert_eq!(h.read32(reg(0x208)), 0);
        assert_eq!(h.peek(reg(0x20), Width::Word), None);
        assert_eq!(h.read8(reg(offset::MCR)), 0);
        h.write16(reg(offset::MCR), 1);
        assert_eq!(
            h.warnings(),
            vec![
                "Unhandled read from offset 0x20.".to_string(),
                "Unhandled write to offset 0x24, value 0x5.".to_string(),
                "Unhandled read from offset 0x208.".to_string(),
                "can1: Attempted Byte read isn't supported by the peripheral. Offset 0x0.".to_string(),
                "can1: Attempted Word write isn't supported by the peripheral. Offset 0x0, value 0x1.".to_string(),
            ]
        );
        assert_eq!(h.read32(reg(offset::MCR)), 0x0001_0002, "the refused write did nothing");
        let summaries = h.core().summaries();
        let summary = &summaries[0].1;
        assert!(summary.starts_with("can1: MCR=0x00010002 MSR=0x00000C02"), "{summary}");
        assert!(!h.get::<StmCan>(id).frame_sink_attached());
    }
}
