// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32_Timer.cs
// (MIT License, Copyright (c) Antmicro).

//! The register map of `Timers.STM32_Timer`, with the order of operations of Renode's register framework:
//! a write first stores the register's plain fields, then runs the field write callbacks in definition order
//! (whether or not a value changed), then the register-level write callback; bits that belong to a tag are
//! reported as "Unhandled write" afterwards. Reads compute every field from its value provider.
//!
//! # `CCMR1`/`CCMR2` (conditional registers)
//!
//! Renode builds four `DoubleWordRegister`s per CCMR, one for each combination of the two channels being in
//! output or input mode, and the bus picks the one that matches the *current* channel modes at every access.
//! Each variant keeps its own stored bits (`St::ccmr[pair][variant]`, variant = `2 * low_is_input +
//! high_is_input`). The `CompareMode` field object of a channel is the one of the **last built** variant in
//! which that channel's half is an output half (variant 1 for the low channel, 2 for the high one) and the
//! `Prescaler` object is that of variant 3; the OCxM write callback also stores the new mode into it.
//! // Renode parity: this is reproduced, including the stale read-back of OCxM / ICxPSC / ICxF a variant shows
//! after the channel modes changed.
//!
//! # `CCR1..4` (conditional registers)
//!
//! An output-mode channel's `CCRx` reads and sets the compare timer's limit. An input-mode channel's `CCRx`
//! reads the captured value and **clears CCxIF as a side effect of the read** (writes are ignored).

use super::io::Io;
use super::logic::sms;
use super::model::{Model, MAIN};
use emu_core::{Direction, LogLevel, Time, WorkMode};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Register offsets.
pub mod reg {
    pub const CR1: u32 = 0x00;
    pub const CR2: u32 = 0x04;
    pub const SMCR: u32 = 0x08;
    pub const DIER: u32 = 0x0C;
    pub const SR: u32 = 0x10;
    pub const EGR: u32 = 0x14;
    pub const CCMR1: u32 = 0x18;
    pub const CCMR2: u32 = 0x1C;
    pub const CCER: u32 = 0x20;
    pub const CNT: u32 = 0x24;
    pub const PSC: u32 = 0x28;
    pub const ARR: u32 = 0x2C;
    pub const RCR: u32 = 0x30;
    pub const CCR1: u32 = 0x34;
    pub const CCR2: u32 = 0x38;
    pub const CCR3: u32 = 0x3C;
    pub const CCR4: u32 = 0x40;
    pub const BDTR: u32 = 0x44;
}

/// Bits of a register that are known but not implemented (`WithTag`, `WithTaggedFlag`, `WithReservedBits`).
struct Tag {
    name: Cow<'static, str>,
    pos: u32,
    width: u32,
    silent: bool,
}

const fn tag(name: &'static str, pos: u32, width: u32) -> Tag {
    Tag { name: Cow::Borrowed(name), pos, width, silent: false }
}

const fn silent_tag(name: &'static str, pos: u32, width: u32) -> Tag {
    Tag { name: Cow::Borrowed(name), pos, width, silent: true }
}

static CR1_TAGS: &[Tag] = &[tag("Clock Division (CKD)", 8, 2), tag("RESERVED", 10, 22)];
static CR2_TAGS: &[Tag] = &[
    tag("Capture/compare preloaded control (CCPC)", 0, 1),
    tag("RESERVED", 1, 1),
    tag("Capture/compare control update selection (CCUS)", 2, 1),
    tag("Capture/compare DMA selection (CCDS)", 3, 1),
    tag("Master mode selection (MMS)", 4, 2),
    tag("Output Idle state 1 (OC1 output) (OIS1)", 8, 1),
    tag("Output Idle state 1 (OC1N output) (OIS1N)", 9, 1),
    tag("Output Idle state 2 (OC2 output) (OIS2)", 10, 1),
    tag("Output Idle state 2 (OC2N output) (OIS2N)", 11, 1),
    tag("Output Idle state 3 (OC3 output) (OIS3)", 12, 1),
    tag("Output Idle state 3 (OC3N output) (OIS3N)", 13, 1),
    tag("Output Idle state 4 (OC4 output) (OIS4)", 14, 1),
    tag("RESERVED", 15, 17),
];
static SMCR_TAGS: &[Tag] = &[
    tag("OCREF clear selection (OCCS)", 3, 1),
    tag("Master/Slave mode (MSM)", 7, 1),
    tag("External trigger filter (ETF)", 8, 3),
    tag("External trigger prescaler (ETPS)", 12, 2),
    tag("External clock enable (ECE)", 14, 1),
    tag("External trigger polarity (ETP)", 15, 1),
    tag("RESERVED", 16, 16),
];
static DIER_TAGS: &[Tag] = &[
    tag("RESERVED", 5, 1),
    tag("RESERVED", 7, 1),
    tag("Update DMA request enable (UDE)", 8, 1),
    tag("Capture/Compare 1 DMA request enable (CC1DE)", 9, 1),
    tag("Capture/Compare 2 DMA request enable (CC2DE)", 10, 1),
    tag("Capture/Compare 3 DMA request enable (CC3DE)", 11, 1),
    tag("Capture/Compare 4 DMA request enable (CC4DE)", 12, 1),
    tag("RESERVED", 13, 1),
    tag("Trigger DMA request enable (TDE)", 14, 1),
    tag("RESERVED", 15, 17),
];
static SR_TAGS: &[Tag] = &[
    silent_tag("COM interrupt flag (COMIF)", 5, 1),
    silent_tag("Break interrupt flag (BIF)", 7, 1),
    silent_tag("Break 2 interrupt flag (B2IF)", 8, 1),
    silent_tag("System Break interrupt flag (SBIF)", 13, 1),
    silent_tag("RESERVED", 14, 18),
];
static EGR_TAGS: &[Tag] = &[
    tag("Capture/compare 1 generation (CC1G)", 1, 1),
    tag("Capture/compare 2 generation (CC2G)", 2, 1),
    tag("Capture/compare 3 generation (CC3G)", 3, 1),
    tag("Capture/compare 4 generation (CC4G)", 4, 1),
    tag("Capture/compare update generation (COMG)", 5, 1),
    tag("Trigger generation (TG)", 6, 1),
    tag("RESERVED", 7, 25),
];
static CCER_TAGS: &[Tag] = &[
    tag("Capture/Compare 1 complementary output enable (CC1NE)", 2, 1),
    tag("Capture/Compare 2 complementary output enable (CC2NE)", 6, 1),
    tag("Capture/Compare 3 complementary output enable (CC3NE)", 10, 1),
    tag("RESERVED", 14, 1),
    tag("RESERVED", 16, 16),
];
static PSC_TAGS: &[Tag] = &[tag("RESERVED", 16, 16)];
static RCR_TAGS: &[Tag] = &[tag("RESERVED", 8, 24)];
static BDTR_TAGS: &[Tag] = &[
    tag("Dead Time Generator (DTG)", 0, 8),
    tag("LOCK", 8, 2),
    tag("Off-state selection idle mode (OSSI)", 10, 1),
    tag("Off-state selection run mode (OSSR)", 11, 1),
    tag("Break enable (BKE)", 12, 1),
    tag("Break polarity (BKP)", 13, 1),
    tag("Automatic output enable (AOE)", 14, 1),
    tag("Main Output Enable (MOE)", 15, 1),
    tag("RESERVED", 16, 16),
];

/// `BitHelper.GetSetBitsPretty`: `"0, 3-5, 7"`.
fn set_bits_pretty(mask: u32) -> String {
    let mut out = String::new();
    let mut bit = 0;
    while bit < 32 {
        if mask & (1 << bit) == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 32 && mask & (1 << (bit + 1)) != 0 {
            bit += 1;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if start == bit {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}-{bit}");
        }
        bit += 1;
    }
    out
}

fn field_mask(pos: u32, width: u32) -> u32 {
    if width == 0 {
        0
    } else if width >= 32 {
        u32::MAX
    } else {
        ((1u32 << width) - 1) << pos
    }
}

/// `PeripheralRegister.LogUnhandledWrites`: written bits that belong to no field and overlap a tag are
/// reported (non-silent tags as a warning, silent ones at Noisy level).
fn warn_tags(io: &mut dyn Io, offset: u32, value: u32, defined: u32, tags: &[Tag]) {
    let unhandled = value & !defined;
    if unhandled == 0 {
        return;
    }
    for silent in [false, true] {
        let level = if silent { LogLevel::Noisy } else { LogLevel::Warning };
        let mut names = String::new();
        for t in tags.iter().filter(|t| t.silent == silent && field_mask(t.pos, t.width) & unhandled != 0) {
            if !names.is_empty() {
                names.push_str(", ");
            }
            let _ = write!(names, "{} (0x{:X})", t.name, (value & field_mask(t.pos, t.width)) >> t.pos);
        }
        if names.is_empty() {
            continue;
        }
        let key = (u64::from(silent) << 62) | (u64::from(unhandled) << 16) | u64::from(offset & 0xFFFF);
        io.log_once(
            level,
            key,
            format_args!(
                "Unhandled write to offset 0x{offset:X}. Unhandled bits: [{}] when writing value 0x{value:X}. Tags: {names}.",
                set_bits_pretty(unhandled)
            ),
        );
    }
}

/// `WithReservedBits(bits, 32 - bits)` of the counter-width registers (CNT, ARR, CCRx).
fn warn_reserved_above(io: &mut dyn Io, offset: u32, value: u32, bits: u32) {
    if bits >= 32 {
        return;
    }
    let tags = [tag("RESERVED", bits, 32 - bits)];
    warn_tags(io, offset, value, field_mask(0, bits), &tags);
}

fn bit(value: u32, n: u32) -> bool {
    value & (1 << n) != 0
}

impl Model {
    // ---- CCMR variants ----------------------------------------------------------------------------

    /// The variant of `CCMR1` (pair 0) / `CCMR2` (pair 1) the bus selects now.
    pub(crate) fn ccmr_variant(&self, pair: usize) -> usize {
        2 * usize::from(self.is_input_mode(2 * pair)) + usize::from(self.is_input_mode(2 * pair + 1))
    }

    /// Whether half `half` (0 = low channel) of `variant` is an output half.
    fn half_is_output(variant: usize, half: usize) -> bool {
        (variant >> (1 - half)) & 1 == 0
    }

    /// Bits of a variant that live in its underlying value (everything except the CCxS provider bits and tags).
    fn ccmr_stored_mask(variant: usize) -> u32 {
        let mut mask = 0;
        for half in 0..2 {
            mask |= if Self::half_is_output(variant, half) { 0x70 } else { 0xFC } << (half * 8);
        }
        mask
    }

    fn ccmr_value(&self, pair: usize) -> u32 {
        let variant = self.ccmr_variant(pair);
        let mut value = self.st.ccmr[pair][variant] & Self::ccmr_stored_mask(variant);
        for half in 0..2 {
            value |= u32::from(self.st.ch[2 * pair + half].mode & 3) << (half * 8);
        }
        value
    }

    fn write_ccmr(&mut self, io: &mut dyn Io, pair: usize, value: u32) {
        let variant = self.ccmr_variant(pair);
        let stored = Self::ccmr_stored_mask(variant);
        let base = self.st.ccmr[pair][variant];
        self.st.ccmr[pair][variant] = (base & !stored) | (value & stored);
        for half in 0..2 {
            let channel = 2 * pair + half;
            self.write_cc_selection(io, channel, (value >> (half * 8)) & 3);
            if Self::half_is_output(variant, half) {
                let mode = (value >> (half * 8 + 4)) & 7;
                // `channel.CompareMode.Value = val`: the designated field object (variant 1 for the low
                // channel, variant 2 for the high one) receives the new mode.
                let designated = if half == 0 { 1 } else { 2 };
                let shift = half * 8 + 4;
                let slot = &mut self.st.ccmr[pair][designated];
                *slot = (*slot & !(7 << shift)) | (mode << shift);
                self.write_output_compare_mode(io, channel, mode);
            }
        }
        // Tags (OCxFE, OCxPE, OCxCE of output halves) and the reserved upper half of the variant that was
        // selected for this write.
        let mut defined = 0u32;
        for half in 0..2usize {
            let base_bit = (half * 8) as u32;
            defined |= if Self::half_is_output(variant, half) {
                field_mask(base_bit, 2) | field_mask(base_bit + 4, 3)
            } else {
                field_mask(base_bit, 2) | field_mask(base_bit + 2, 2) | field_mask(base_bit + 4, 4)
            };
        }
        if value & !defined != 0 {
            let mut tags: Vec<Tag> = Vec::new();
            for half in 0..2usize {
                if Self::half_is_output(variant, half) {
                    let n = 2 * pair + half + 1;
                    let base_bit = (half * 8) as u32;
                    for (shift, what, short) in [(2, "fast", "FE"), (3, "preload", "PE"), (7, "clear", "CE")] {
                        tags.push(Tag {
                            name: Cow::Owned(format!("Output compare {n} {what} enable (OC{n}{short})")),
                            pos: base_bit + shift,
                            width: 1,
                            silent: false,
                        });
                    }
                }
            }
            tags.push(tag("RESERVED", 16, 16));
            let offset = if pair == 0 { reg::CCMR1 } else { reg::CCMR2 };
            warn_tags(io, offset, value, defined, &tags);
        }
    }

    // ---- register values --------------------------------------------------------------------------

    /// Side-effect-free value of the register at `offset` as of clock time `now`; `None` for offsets
    /// without a register. All model events up to `now` must have been processed (or replayed on a copy).
    pub(crate) fn reg_value(&self, offset: u32, now: Time) -> Option<u32> {
        let st = &self.st;
        let mask = self.cfg.mask();
        let value = match offset {
            reg::CR1 => {
                let main = self.entry_at(MAIN, now);
                u32::from(st.enable_requested)
                    | u32::from(st.udis) << 1
                    | u32::from(st.urs) << 2
                    | u32::from(main.mode() == WorkMode::OneShot) << 3
                    | u32::from(main.direction() == Direction::Descending) << 4
                    | u32::from(st.cms) << 5
                    | u32::from(st.apre) << 7
            }
            reg::CR2 => u32::from(st.ti1s) << 7,
            reg::SMCR => u32::from(st.sms) | u32::from(st.ts) << 4,
            reg::DIER => {
                let mut v = u32::from(st.uie) | u32::from(st.tie) << 6;
                for (i, c) in st.ch.iter().enumerate() {
                    v |= u32::from(c.ie) << (1 + i);
                }
                v
            }
            reg::SR => {
                let mut v = u32::from(st.update_flag) | u32::from(st.tif) << 6;
                for (i, c) in st.ch.iter().enumerate() {
                    v |= u32::from(c.iflag) << (1 + i) | u32::from(c.overcapture) << (9 + i);
                }
                v
            }
            reg::EGR => 0,
            reg::CCMR1 => self.ccmr_value(0),
            reg::CCMR2 => self.ccmr_value(1),
            reg::CCER => {
                let mut v = 0;
                for (i, c) in st.ch.iter().enumerate() {
                    v |= u32::from(c.oe) << (4 * i) | u32::from(c.polarity) << (4 * i + 1) | u32::from(c.comp_polarity) << (4 * i + 3);
                }
                v
            }
            reg::CNT => (self.entry_at(MAIN, now).value() as u32) & mask,
            reg::PSC => ((self.clk.div[MAIN] - 1) as u32) & 0xFFFF,
            reg::ARR => st.auto_reload & mask,
            reg::RCR => u32::from(st.rcr),
            reg::CCR1 | reg::CCR2 | reg::CCR3 | reg::CCR4 => {
                let i = ((offset - reg::CCR1) / 4) as usize;
                if self.is_output_mode(i) {
                    (self.entry_at(1 + i, now).period() as u32) & mask
                } else {
                    st.ch[i].captured & mask
                }
            }
            reg::BDTR => 0,
            _ => return None,
        };
        Some(value)
    }

    // ---- bus access -------------------------------------------------------------------------------

    /// A register read with its side effects (`CCRx` of an input channel clears CCxIF).
    pub(crate) fn read(&mut self, io: &mut dyn Io, offset: u32) -> u32 {
        self.begin(io);
        let now = self.t;
        let Some(value) = self.reg_value(offset, now) else {
            io.log_once(LogLevel::Warning, 0x4EAD_0000 | u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}."));
            return 0;
        };
        if matches!(offset, reg::CCR1 | reg::CCR2 | reg::CCR3 | reg::CCR4) {
            let i = ((offset - reg::CCR1) / 4) as usize;
            if self.is_input_mode(i) {
                // Provider of an input-mode CCRx: the flag is cleared and the interrupt line re-evaluated.
                self.st.ch[i].iflag = false;
                self.update_interrupts(io);
                self.dirty = true;
            }
        }
        value
    }

    /// A register write. Everything the callbacks do to the clock entries requests a CPU return like the
    /// `LimitTimer` setters of Renode do.
    pub(crate) fn write(&mut self, io: &mut dyn Io, offset: u32, value: u32) {
        self.begin(io);
        self.periodic = None;
        self.dirty = true;
        let saved = self.emit_return;
        self.emit_return = true;
        // NGCLazyPwmTimer.WriteDoubleWord: `Restore()` before the stock write, whatever the offset.
        self.restore(io);
        let bits = self.cfg.bits;
        let mask = self.cfg.mask();
        match offset {
            reg::CR1 => {
                let st = &mut self.st;
                st.udis = bit(value, 1);
                st.urs = bit(value, 2);
                st.cms = ((value >> 5) & 3) as u8;
                st.apre = bit(value, 7);
                // CEN
                self.st.enable_requested = bit(value, 0);
                if !(self.is_trigger_mode() || self.is_encoder_mode()) {
                    let enable = self.st.enable_requested && self.st.auto_reload > 0;
                    self.lt_set_enabled(io, MAIN, enable);
                }
                // OPM
                let mode = if bit(value, 3) { WorkMode::OneShot } else { WorkMode::Periodic };
                self.lt_set_mode(io, MAIN, mode);
                // DIR
                if !(self.is_encoder_mode() || self.st.cms != 0) {
                    let direction = if bit(value, 4) { Direction::Descending } else { Direction::Ascending };
                    self.lt_set_direction(io, MAIN, direction);
                }
                self.update_capture_compare_timers(io);
                self.update_interrupts(io);
                warn_tags(io, offset, value, 0xFF, CR1_TAGS);
            }
            reg::CR2 => {
                self.st.ti1s = bit(value, 7);
                warn_tags(io, offset, value, 0x80, CR2_TAGS);
            }
            reg::SMCR => {
                self.st.sms = (value & 7) as u8;
                self.st.ts = ((value >> 4) & 7) as u8;
                let selection = value & 7;
                if selection == sms::EXTERNAL_CLOCK1 {
                    io.log(LogLevel::Warning, format_args!("External Clock mode 1 is not supported"));
                } else if self.is_trigger_mode() || self.is_encoder_mode() {
                    self.lt_set_enabled(io, MAIN, false);
                    self.sync(io);
                } else {
                    let enable = self.st.enable_requested && self.st.auto_reload > 0;
                    self.lt_set_enabled(io, MAIN, enable);
                }
                warn_tags(io, offset, value, 0x77, SMCR_TAGS);
            }
            reg::DIER => {
                self.st.uie = bit(value, 0);
                self.st.tie = bit(value, 6);
                for i in 0..4 {
                    self.write_cc_interrupt_enable(io, i, bit(value, 1 + i as u32));
                }
                self.update_interrupts(io);
                warn_tags(io, offset, value, 0x5F, DIER_TAGS);
            }
            reg::SR => {
                // WriteZeroToClear fields with plain storage (TIF, CCxOF), then the callbacks of UIF and CCxIF.
                self.st.tif &= bit(value, 6);
                for i in 0..4 {
                    self.st.ch[i].overcapture &= bit(value, 9 + i as u32);
                }
                if !bit(value, 0) {
                    self.st.update_flag = false;
                    io.log(LogLevel::Noisy, format_args!("IRQ claimed"));
                }
                for i in 0..4 {
                    self.claim_cc_interrupt(i, bit(value, 1 + i as u32));
                }
                self.update_interrupts(io);
                warn_tags(io, offset, value, 0x1E5F, SR_TAGS);
            }
            reg::EGR => {
                // Renode parity: the UG callback ignores the written bit, so *any* write generates an update.
                if !self.st.udis {
                    if self.entry(MAIN).direction() == Direction::Ascending {
                        self.lt_set_value(io, MAIN, 0);
                    } else {
                        let reload = u64::from(self.st.auto_reload);
                        self.lt_set_value(io, MAIN, reload);
                    }
                    self.st.repetitions_left = u32::from(self.st.rcr);
                    if !self.st.urs && self.st.uie {
                        io.log(LogLevel::Noisy, format_args!("IRQ pending"));
                        self.st.update_flag = true;
                    }
                    for i in 0..4 {
                        if self.lt_enabled(1 + i) {
                            let main = self.lt_value(MAIN);
                            self.lt_set_value(io, 1 + i, main);
                        }
                    }
                }
                self.update_interrupts(io);
                warn_tags(io, offset, value, 0x1, EGR_TAGS);
            }
            reg::CCMR1 => self.write_ccmr(io, 0, value),
            reg::CCMR2 => self.write_ccmr(io, 1, value),
            reg::CCER => {
                for i in 0..4 {
                    self.st.ch[i].polarity = bit(value, 4 * i as u32 + 1);
                    self.st.ch[i].comp_polarity = bit(value, 4 * i as u32 + 3);
                }
                for i in 0..4 {
                    self.write_cc_output_enable(io, i, bit(value, 4 * i as u32));
                }
                warn_tags(io, offset, value, 0xBBBB, CCER_TAGS);
            }
            reg::CNT => {
                let field = value & mask;
                self.lt_set_value(io, MAIN, u64::from(field));
                for i in 0..4 {
                    if u64::from(value) < self.lt_limit(1 + i) {
                        self.lt_set_value(io, 1 + i, u64::from(value));
                    }
                }
                self.update_interrupts(io);
                warn_reserved_above(io, offset, value, bits);
            }
            reg::PSC => {
                let divider = u64::from(value & 0xFFFF) + 1;
                self.lt_set_divider(io, MAIN, divider);
                let divider = self.clk.div[MAIN];
                for i in 0..4 {
                    self.lt_set_divider(io, 1 + i, divider);
                }
                self.update_interrupts(io);
                warn_tags(io, offset, value, 0xFFFF, PSC_TAGS);
            }
            reg::ARR => {
                self.st.auto_reload = value & mask;
                let enable = self.st.enable_requested && self.st.auto_reload > 0;
                self.lt_set_enabled(io, MAIN, enable);
                if !self.st.apre {
                    let limit = u64::from(self.st.auto_reload);
                    self.lt_set_limit(io, MAIN, limit);
                }
                self.update_interrupts(io);
                warn_reserved_above(io, offset, value, bits);
            }
            reg::RCR => {
                self.st.rcr = (value & 0xFF) as u8;
                warn_tags(io, offset, value, 0xFF, RCR_TAGS);
            }
            reg::CCR1 | reg::CCR2 | reg::CCR3 | reg::CCR4 => {
                let i = ((offset - reg::CCR1) / 4) as usize;
                if self.is_output_mode(i) {
                    let field = value & mask;
                    if field == 0 {
                        self.lt_set_enabled(io, 1 + i, false);
                    }
                    self.lt_set_limit(io, 1 + i, u64::from(field));
                    self.update_timer(io, i);
                    self.update_interrupts(io);
                }
                warn_reserved_above(io, offset, value, bits);
            }
            reg::BDTR => warn_tags(io, offset, value, 0, BDTR_TAGS),
            _ => {
                io.log_once(
                    LogLevel::Warning,
                    0x4EAD_8000 | u64::from(offset),
                    format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}."),
                );
            }
        }
        self.emit_return = saved;
    }

    /// Side-effect-free register value at `now` (replays pending model events on a copy).
    pub(crate) fn peek_register(&self, offset: u32, now: Time) -> Option<u32> {
        match self.replayed(now) {
            Some(copy) => copy.reg_value(offset, now),
            None => self.reg_value(offset, now),
        }
    }
}
