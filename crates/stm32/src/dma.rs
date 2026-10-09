// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/DMA/STM32LDMA.cs and the copy logic of
// src/Emulator/Main/Peripherals/DMA/DmaEngine.cs (MIT License, Copyright (c) Antmicro).

//! STM32L4 DMA controller as modeled by Renode's `DMA.STM32LDMA`.
//!
//! The model moves **one data unit per request edge**: a rising level on request input `n` (Renode
//! `OnGPIO(n, true)`, e.g. `adc DMARequest -> dma1@0`) performs one transfer on channel `n` through the system
//! bus ([`Ctx::mem_read`]/[`Ctx::mem_write`], so peripheral register reads and writes have their side effects),
//! then updates the counters and the half/complete flags. There is no arbitration, no bus timing, no burst, no
//! memory-to-memory mode (`MEM2MEM` is an unhandled bit) and no transfer-error flag. Enabling a channel does
//! not start anything.
//!
//! Output lines `0..7` are the channel interrupts (`IRQ` of each channel, `.repl` `[0-6] -> nvic@[11-17]` and
//! `[0-4] -> nvic@[56-60]`, `[5-6] -> nvic@[68-69]`). Renode instantiates **eight** channels but the register map
//! decodes only seven (`0x08..=0x8C`); `CSELR` (0xA8) is not modeled and logs as unhandled.
//!
//! Renode quirks reproduced (`// Renode parity`): the destination address of a transfer advances by the
//! *source* data size; the interrupt-status `GIF` bit reports the channel's interrupt *line* (flag AND enable),
//! `TEIF` never reads; interrupts are re-evaluated only when a flag changes, so enabling `TCIE` while `TC` is
//! already set does not raise the line until the next flag change; `EN` stays set after the last transfer;
//! requests with a size combination the engine cannot copy (`DmaEngine` throws) are logged as errors and skipped.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, LogLevel, Peripheral, View, Width};

/// Renode `NumberOfChannels`.
pub const NUMBER_OF_CHANNELS: usize = 8;

/// Register offsets.
pub mod offset {
    /// Interrupt status register (`DMA_ISR`).
    pub const ISR: u32 = 0x00;
    /// Interrupt flag clear register (`DMA_IFCR`).
    pub const IFCR: u32 = 0x04;
    /// Offset of channel `n`'s register block: `CCR = 0`, `CNDTR = 4`, `CPAR = 8`, `CMAR = 0xC` relative to it.
    pub const fn channel(n: u32) -> u32 {
        0x08 + 0x14 * n
    }
    pub const CCR: u32 = 0x0;
    pub const CNDTR: u32 = 0x4;
    pub const CPAR: u32 = 0x8;
    pub const CMAR: u32 = 0xC;
    /// Channel selection register: not modeled by Renode.
    pub const CSELR: u32 = 0xA8;
}

const FIRST_CHANNEL_OFFSET: u32 = 0x08;
const LAST_CHANNEL_OFFSET: u32 = 0x8C;
const CHANNEL_STRIDE: u32 = 0x14;

/// One DMA channel (Renode `STM32LDMA.Channel`).
#[derive(Clone, Copy, Default)]
struct Channel {
    /// `Direction`: false = read from peripheral, true = read from memory.
    from_memory: bool,
    priority: u32,
    number_of_data: u32,
    initial_number_of_data: u32,
    /// `TransferType` in bytes: 1, 2 or 4 (0 after `Reset`, which the engine cannot copy with).
    memory_transfer_type: u32,
    peripheral_transfer_type: u32,
    memory_address: u32,
    peripheral_address: u32,
    peripheral_increment: bool,
    memory_increment: bool,
    circular: bool,
    enabled: bool,
    transfer_complete: bool,
    transfer_complete_interrupt_enabled: bool,
    transfer_error_interrupt_enabled: bool,
    half_transfer: bool,
    half_transfer_interrupt_enabled: bool,
    transfer_in_progress: bool,
    trigger_requested: bool,
}

/// `DmaEngine.Request` for the one shape `STM32LDMA` issues (`size` = source transfer size).
struct CopyRequest {
    source: u32,
    destination: u32,
    size: u32,
    read_transfer_type: u32,
    write_transfer_type: u32,
    increment_read: bool,
    increment_write: bool,
}

/// STM32L DMA controller (Renode `DMA.STM32LDMA`).
pub struct Dma {
    name: String,
    channels: [Channel; NUMBER_OF_CHANNELS],
    /// Mirror of the interrupt output levels (for `peek`; reads use the live output).
    irq: [bool; NUMBER_OF_CHANNELS],
}

impl Dma {
    pub fn new(name: impl Into<String>) -> Dma {
        // The constructor leaves memory and peripheral size at Byte (Reset() would zero them).
        let channel = Channel { memory_transfer_type: 1, peripheral_transfer_type: 1, ..Channel::default() };
        Dma { name: name.into(), channels: [channel; NUMBER_OF_CHANNELS], irq: [false; NUMBER_OF_CHANNELS] }
    }

    /// `IDMA.RequestTransfer(channel)`: `channel` is 1-based. Not used by the NGC platform.
    pub fn request_transfer(&mut self, ctx: &mut Ctx<'_>, channel: i32) {
        if channel > 0 && channel as usize <= NUMBER_OF_CHANNELS {
            self.do_transfer(ctx, channel as usize - 1);
        } else {
            ctx.warn_once(0xFFFF_0001, format_args!("Invalid channel {channel}, no transfer performed."));
        }
    }

    // ---- interrupts ----

    fn update_interrupts(&mut self, ctx: &mut Ctx<'_>, ch: usize) {
        let c = &self.channels[ch];
        let level = (c.transfer_complete && c.transfer_complete_interrupt_enabled) || (c.half_transfer && c.half_transfer_interrupt_enabled);
        self.irq[ch] = level;
        ctx.set_output(ch as u32, level);
    }

    fn set_transfer_complete(&mut self, ctx: &mut Ctx<'_>, ch: usize, value: bool) {
        self.channels[ch].transfer_complete = value;
        self.update_interrupts(ctx, ch);
    }

    fn set_half_transfer(&mut self, ctx: &mut Ctx<'_>, ch: usize, value: bool) {
        self.channels[ch].half_transfer = value;
        self.update_interrupts(ctx, ch);
    }

    // ---- transfers ----

    /// `Channel.OnGPIO`: the request level; every rising edge (a `true` that finds no transfer running)
    /// runs transfers while the trigger is re-requested.
    fn request(&mut self, ctx: &mut Ctx<'_>, ch: usize, level: bool) {
        self.channels[ch].trigger_requested = level;
        if self.channels[ch].transfer_in_progress {
            return;
        }
        self.channels[ch].transfer_in_progress = true;
        while self.channels[ch].trigger_requested {
            self.channels[ch].trigger_requested = false;
            self.do_transfer(ctx, ch);
        }
        self.channels[ch].transfer_in_progress = false;
    }

    /// `Channel.DoTransfer`.
    fn do_transfer(&mut self, ctx: &mut Ctx<'_>, ch: usize) {
        let c = self.channels[ch];
        if c.number_of_data == 0 {
            ctx.logf(LogLevel::Debug, format_args!("Channel {ch}: 0 bytes of data left, transfer stopped."));
            return;
        }
        if !c.enabled {
            // Legitimate when a deferred request arrives while software reconfigures the channel.
            ctx.logf(LogLevel::Debug, format_args!("Channel {ch}: Cannot transfer on disabled channel"));
            return;
        }
        let (mut source, mut destination, increment_source, increment_destination, source_type, destination_type) = if c.from_memory {
            (c.memory_address, c.peripheral_address, c.memory_increment, c.peripheral_increment, c.memory_transfer_type, c.peripheral_transfer_type)
        } else {
            (c.peripheral_address, c.memory_address, c.peripheral_increment, c.memory_increment, c.peripheral_transfer_type, c.memory_transfer_type)
        };
        let done = c.initial_number_of_data.wrapping_sub(c.number_of_data);
        if increment_source {
            source = source.wrapping_add(source_type.wrapping_mul(done));
        }
        if increment_destination {
            // Renode parity: the destination advances by the *source* transfer size.
            destination = destination.wrapping_add(source_type.wrapping_mul(done));
        }
        let request = CopyRequest {
            source,
            destination,
            size: source_type,
            read_transfer_type: source_type,
            write_transfer_type: destination_type,
            increment_read: increment_source,
            increment_write: increment_destination,
        };
        if let Err(problem) = issue_copy(ctx, &request) {
            // DmaEngine throws here in Renode; the transfer is abandoned before any counter changes.
            ctx.error_once(
                0xDA00 | ch as u64,
                format_args!("Channel {ch}: DMA request cannot be served: {problem} (source type {source_type}, destination type {destination_type})"),
            );
            return;
        }
        self.channels[ch].number_of_data -= 1;
        let remaining = self.channels[ch].number_of_data;
        if remaining == 0 {
            self.set_transfer_complete(ctx, ch, true);
            let channel = &mut self.channels[ch];
            if channel.circular {
                channel.number_of_data = channel.initial_number_of_data;
            }
        } else if remaining == self.channels[ch].initial_number_of_data / 2 {
            self.set_half_transfer(ctx, ch, true);
        }
    }

    // ---- registers ----

    fn config_value(c: &Channel) -> u32 {
        u32::from(c.enabled)
            | u32::from(c.transfer_complete_interrupt_enabled) << 1
            | u32::from(c.half_transfer_interrupt_enabled) << 2
            | u32::from(c.transfer_error_interrupt_enabled) << 3
            | u32::from(c.from_memory) << 4
            | u32::from(c.circular) << 5
            | u32::from(c.peripheral_increment) << 6
            | u32::from(c.memory_increment) << 7
            | (c.peripheral_transfer_type >> 1) << 8
            | (c.memory_transfer_type >> 1) << 10
            | c.priority << 12
    }

    fn channel_read(&self, ctx: &mut Ctx<'_>, ch: usize, rel: u32) -> u32 {
        let c = &self.channels[ch];
        match rel {
            offset::CCR => Self::config_value(c),
            offset::CNDTR => c.number_of_data,
            offset::CPAR => c.peripheral_address,
            offset::CMAR => c.memory_address,
            _ => {
                ctx.warn_once(
                    (1 << 40) | ((ch as u64) << 32) | u64::from(rel),
                    format_args!("Channel {ch}: unhandled read from 0x{rel:X}."),
                );
                0
            }
        }
    }

    fn channel_write(&mut self, ctx: &mut Ctx<'_>, ch: usize, rel: u32, value: u32) {
        let c = &mut self.channels[ch];
        match rel {
            offset::CCR => {
                c.enabled = value & 1 != 0;
                c.transfer_complete_interrupt_enabled = value & (1 << 1) != 0;
                c.half_transfer_interrupt_enabled = value & (1 << 2) != 0;
                c.transfer_error_interrupt_enabled = value & (1 << 3) != 0;
                c.from_memory = (value >> 4) & 1 != 0;
                c.circular = value & (1 << 5) != 0;
                c.peripheral_increment = value & (1 << 6) != 0;
                c.memory_increment = value & (1 << 7) != 0;
                // PSIZE and MSIZE are read-only while EN = 1 (judged on the written value).
                if value & 1 == 0 {
                    let mut invalid = false;
                    let psize = (value >> 8) & 3;
                    if psize == 3 {
                        invalid = true;
                    } else {
                        c.peripheral_transfer_type = 1 << psize;
                    }
                    let msize = (value >> 10) & 3;
                    if msize == 3 {
                        invalid = true;
                    } else {
                        c.memory_transfer_type = 1 << msize;
                    }
                    if invalid {
                        ctx.warn_once((1 << 41) | (ch as u64), format_args!("Channel {ch}: Invalid reserved value for size"));
                    }
                }
                c.priority = (value >> 12) & 3;
                if value & !0x3FFF != 0 {
                    ctx.warn_once(
                        (1 << 42) | (u64::from(value) << 4) | ch as u64,
                        format_args!("Channel {ch}: some unhandled bits were written to configuration register. Value is 0x{value:X}."),
                    );
                }
            }
            offset::CNDTR => {
                c.number_of_data = value;
                c.initial_number_of_data = value;
            }
            offset::CPAR => c.peripheral_address = value,
            offset::CMAR => c.memory_address = value,
            _ => {
                // Renode's format string for this message has three placeholders for two arguments; the
                // evident intent (channel, offset, value) is logged instead.
                ctx.warn_once(
                    (1 << 43) | ((ch as u64) << 32) | u64::from(rel),
                    format_args!("Channel {ch}: unhandled write to 0x{rel:X}, value 0x{value:X}."),
                );
            }
        }
    }

    /// `HandleInterruptStatusRead`. Renode parity: `GIF` is the channel's interrupt *line*, `TEIF` never reads.
    fn interrupt_status(&self, line_level: impl Fn(usize) -> bool) -> u32 {
        let mut status = 0u32;
        for (i, c) in self.channels.iter().enumerate() {
            if line_level(i) {
                status |= 1 << (i * 4);
            }
            if c.transfer_complete {
                status |= 1 << (i * 4 + 1);
            }
            if c.half_transfer {
                status |= 1 << (i * 4 + 2);
            }
        }
        status
    }

    /// `HandleClearInterrupt`.
    fn clear_interrupts(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        for ch in 0..NUMBER_OF_CHANNELS {
            let global = 4 * ch;
            if value & (1 << global) != 0 {
                self.set_transfer_complete(ctx, ch, false);
                self.set_half_transfer(ctx, ch, false);
            }
            if value & (1 << (global + 1)) != 0 {
                self.set_transfer_complete(ctx, ch, false);
            }
            if value & (1 << (global + 2)) != 0 {
                self.set_half_transfer(ctx, ch, false);
            }
        }
    }

    fn decode_channel(offset: u32) -> Option<(usize, u32)> {
        if (FIRST_CHANNEL_OFFSET..=LAST_CHANNEL_OFFSET).contains(&offset) {
            let ch = (offset - FIRST_CHANNEL_OFFSET) / CHANNEL_STRIDE;
            Some((ch as usize, offset - FIRST_CHANNEL_OFFSET - ch * CHANNEL_STRIDE))
        } else {
            None
        }
    }
}

/// `DmaEngine.IssueCopy` for requests built by `STM32LDMA` (`size` equals the read transfer type).
/// Peripheral accesses go through the bus with the transfer width; plain memory is copied directly.
/// Renode parity: an unmapped address counts as "continuous memory" there; here only flash/SRAM do, so an
/// unmapped source reads 0 and an unmapped destination drops the write, with the bus's own warnings.
fn issue_copy(ctx: &mut Ctx<'_>, r: &CopyRequest) -> Result<(), &'static str> {
    if r.read_transfer_type == 0 || r.write_transfer_type == 0 {
        // C#: DivideByZeroException in the alignment check.
        return Err("transfer size is 0 (the channel's PSIZE/MSIZE was never programmed after a reset)");
    }
    if r.size % r.read_transfer_type != 0 || r.size % r.write_transfer_type != 0 {
        return Err("request size is not aligned properly to the read or write transfer type");
    }
    if r.size > 4 {
        return Err("request size above 4 bytes is not produced by this controller");
    }
    let size = r.size as usize;
    let read_len = r.read_transfer_type as usize;
    let write_len = r.write_transfer_type as usize;
    let mut buffer = [0u8; 4];

    // ---- read side ----
    let source_is_memory = ctx.is_plain_memory(r.source);
    if source_is_memory {
        if r.increment_read {
            ctx.mem_read_bytes(r.source, &mut buffer[..size]);
        } else {
            // Only the last unit is used when the address does not advance.
            ctx.mem_read_bytes(r.source, &mut buffer[..read_len]);
        }
    } else {
        let mut transferred = 0usize;
        let mut offset = 0u32;
        while transferred < size {
            let address = r.source.wrapping_add(offset);
            match r.read_transfer_type {
                1 => buffer[transferred] = ctx.mem_read(address, Width::Byte) as u8,
                2 => buffer[transferred..transferred + 2].copy_from_slice(&(ctx.mem_read(address, Width::Half) as u16).to_le_bytes()),
                _ => buffer[transferred..transferred + 4].copy_from_slice(&ctx.mem_read(address, Width::Word).to_le_bytes()),
            }
            transferred += read_len;
            if r.increment_read {
                offset = offset.wrapping_add(r.read_transfer_type);
            }
        }
    }

    // ---- write side ----
    if ctx.is_plain_memory(r.destination) {
        if r.increment_write {
            if r.increment_read || !source_is_memory {
                ctx.mem_write_bytes(r.destination, &buffer[..size]);
            } else {
                // Read address fixed: every destination unit receives the first source unit.
                let chunk = buffer[..write_len].to_vec();
                let mut start = 0usize;
                while start < size {
                    ctx.mem_write_bytes(r.destination.wrapping_add(start as u32), &chunk);
                    start += write_len;
                }
            }
        } else {
            // Write address fixed: effectively only the last unit is written.
            let skip = if size == write_len { 0 } else { size - write_len };
            ctx.mem_write_bytes(r.destination, &buffer[skip..size]);
        }
    } else {
        let mut transferred = 0usize;
        let mut offset = 0u32;
        while transferred < size {
            let address = r.destination.wrapping_add(offset);
            match r.write_transfer_type {
                1 => ctx.mem_write(address, Width::Byte, u32::from(buffer[transferred])),
                2 => ctx.mem_write(address, Width::Half, u32::from(u16::from_le_bytes([buffer[transferred], buffer[transferred + 1]]))),
                _ => ctx.mem_write(
                    address,
                    Width::Word,
                    u32::from_le_bytes([buffer[transferred], buffer[transferred + 1], buffer[transferred + 2], buffer[transferred + 3]]),
                ),
            }
            transferred += write_len;
            if r.increment_write {
                offset = offset.wrapping_add(r.write_transfer_type);
            }
        }
    }
    Ok(())
}

impl Peripheral for Dma {
    fn name(&self) -> &str {
        &self.name
    }

    /// `IDoubleWordPeripheral` without `[AllowedTranslations]`.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        for ch in 0..NUMBER_OF_CHANNELS {
            let c = &mut self.channels[ch];
            c.peripheral_increment = false;
            c.peripheral_address = 0;
            c.memory_address = 0;
            c.memory_increment = false;
            c.memory_transfer_type = 0;
            c.peripheral_transfer_type = 0;
            c.transfer_complete_interrupt_enabled = false;
            c.transfer_error_interrupt_enabled = false;
            c.number_of_data = 0;
            c.initial_number_of_data = 0;
            c.priority = 0;
            c.from_memory = false;
            c.circular = false;
            c.half_transfer_interrupt_enabled = false;
            self.set_transfer_complete(ctx, ch, false);
            self.set_half_transfer(ctx, ch, false);
            let c = &mut self.channels[ch];
            c.enabled = false;
            c.transfer_in_progress = false;
            c.trigger_requested = false;
        }
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        if let Some((ch, rel)) = Self::decode_channel(offset) {
            return self.channel_read(ctx, ch, rel);
        }
        if offset == offset::ISR {
            let mut levels = [false; NUMBER_OF_CHANNELS];
            for (i, level) in levels.iter_mut().enumerate() {
                *level = ctx.output(i as u32);
            }
            return self.interrupt_status(|i| levels[i]);
        }
        ctx.warn_once(u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}."));
        0
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        if let Some((ch, rel)) = Self::decode_channel(offset) {
            self.channel_write(ctx, ch, rel, value);
            return;
        }
        if offset == offset::IFCR {
            self.clear_interrupts(ctx, value);
            return;
        }
        let key = (1u64 << 63) | (u64::from(offset) << 32) | u64::from(value);
        ctx.warn_once(key, format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}."));
    }

    /// `OnGPIO(number, value)`: request input `number` is the request line of channel `number`.
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        if line as usize >= NUMBER_OF_CHANNELS {
            ctx.warn_once(
                (1 << 44) | u64::from(line),
                format_args!("Attempted to signal DMA channel {line}. Maximum value is {}", NUMBER_OF_CHANNELS - 1),
            );
            return;
        }
        self.request(ctx, line as usize, level);
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        if let Some((ch, rel)) = Self::decode_channel(offset) {
            let c = &self.channels[ch];
            return match rel {
                offset::CCR => Some(Self::config_value(c)),
                offset::CNDTR => Some(c.number_of_data),
                offset::CPAR => Some(c.peripheral_address),
                offset::CMAR => Some(c.memory_address),
                _ => None,
            };
        }
        if offset == offset::ISR {
            return Some(self.interrupt_status(|i| self.irq[i]));
        }
        None
    }

    fn summary(&self, _view: &View<'_>) -> String {
        let mut text = format!("{}:", self.name);
        for (i, c) in self.channels.iter().enumerate() {
            if c.enabled || c.number_of_data != 0 {
                text.push_str(&format!(
                    " ch{i}[en={} n={}/{} dir={} circ={} tc={} ht={}]",
                    u8::from(c.enabled),
                    c.number_of_data,
                    c.initial_number_of_data,
                    if c.from_memory { "mem->per" } else { "per->mem" },
                    u8::from(c.circular),
                    u8::from(c.transfer_complete),
                    u8::from(c.half_transfer)
                ));
            }
        }
        text
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::PeriphId;

    const DMA: u32 = 0x4002_0000;
    const SRC: u32 = 0x5004_0000;
    const SRAM: u32 = 0x2000_1000;
    const DR: u32 = SRC + 0x40;

    const EN: u32 = 1;
    const TCIE: u32 = 1 << 1;
    const HTIE: u32 = 1 << 2;
    const DIR_FROM_MEMORY: u32 = 1 << 4;
    const CIRC: u32 = 1 << 5;
    const PINC: u32 = 1 << 6;
    const MINC: u32 = 1 << 7;
    const PSIZE_8: u32 = 0;
    const PSIZE_16: u32 = 1 << 8;
    const PSIZE_32: u32 = 2 << 8;
    const MSIZE_8: u32 = 0;
    const MSIZE_16: u32 = 1 << 10;
    const MSIZE_32: u32 = 2 << 10;

    /// A converter-like peripheral: reading `DR` returns an incrementing counter and clears `EOC`
    /// (a read side effect); writes are recorded; output line 0 is the DMA request.
    struct FakeSource {
        next: u32,
        dr_reads: u32,
        eoc: bool,
        writes: Vec<(u32, u32, Width)>,
    }

    impl FakeSource {
        fn new(first: u32) -> Self {
            FakeSource { next: first, dr_reads: 0, eoc: false, writes: Vec::new() }
        }

        /// The conversion finished: raise EOC and pulse the request line.
        fn request(&mut self, ctx: &mut Ctx<'_>) {
            self.eoc = true;
            ctx.set_output(0, true);
            ctx.set_output(0, false);
        }
    }

    impl Peripheral for FakeSource {
        fn name(&self) -> &str {
            "adc"
        }

        fn read(&mut self, offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
            match offset {
                0x00 => u32::from(self.eoc),
                0x40 => {
                    self.dr_reads += 1;
                    self.eoc = false;
                    let value = self.next;
                    self.next = self.next.wrapping_add(1);
                    value
                }
                _ => 0,
            }
        }

        fn write(&mut self, offset: u32, width: Width, value: u32, _ctx: &mut Ctx<'_>) {
            self.writes.push((offset, value, width));
        }

        impl_peripheral_any!();
    }

    fn rig(first: u32) -> (Harness, PeriphId, PeriphId) {
        let mut h = Harness::new();
        let dma = h.add_mapped(DMA, 0x400, Dma::new("dma1"));
        let src = h.add_mapped(SRC, 0x400, FakeSource::new(first));
        h.connect_input(src, 0, dma, 0);
        for ch in 0..7u32 {
            h.connect_irq(dma, ch, 11 + ch);
        }
        h.clear_irq_changes();
        (h, dma, src)
    }

    fn ch(n: u32, rel: u32) -> u32 {
        DMA + offset::channel(n) + rel
    }

    /// HAL_DMA_Init + HAL_DMA_Start: sizes are programmed while the channel is disabled (PSIZE/MSIZE ignore
    /// writes made together with EN = 1), then EN is set.
    fn configure(h: &mut Harness, n: u32, config: u32, count: u32, peripheral: u32, memory: u32) {
        h.write32(ch(n, offset::CPAR), peripheral);
        h.write32(ch(n, offset::CMAR), memory);
        h.write32(ch(n, offset::CNDTR), count);
        h.write32(ch(n, offset::CCR), config & !EN);
        if config & EN != 0 {
            h.write32(ch(n, offset::CCR), config);
        }
    }

    fn request(h: &mut Harness, src: PeriphId) {
        h.with::<FakeSource, _>(src, |s, ctx| s.request(ctx));
    }

    fn halfwords(h: &Harness, count: u32) -> Vec<u32> {
        (0..count).map(|i| h.peek(SRAM + 2 * i, Width::Half).unwrap()).collect()
    }

    #[test]
    fn configuration_register_reads_back_what_the_channel_decodes() {
        let (mut h, _dma, _src) = rig(0);
        let config = TCIE | HTIE | (1 << 3) | DIR_FROM_MEMORY | CIRC | PINC | MINC | PSIZE_16 | MSIZE_32 | (2 << 12);
        h.write32(ch(2, offset::CCR), config);
        assert_eq!(h.read32(ch(2, offset::CCR)), config);
        h.write32(ch(2, offset::CNDTR), 77);
        h.write32(ch(2, offset::CPAR), 0x4000_1234);
        h.write32(ch(2, offset::CMAR), 0x2000_0042);
        assert_eq!(
            [offset::CNDTR, offset::CPAR, offset::CMAR].map(|rel| h.read32(ch(2, rel))),
            [77, 0x4000_1234, 0x2000_0042]
        );
        // Channels are independent (stride 0x14).
        assert_eq!(h.read32(ch(3, offset::CNDTR)), 0);
        assert_eq!(h.read32(ch(1, offset::CCR)), MSIZE_8 | PSIZE_8);
        // PSIZE/MSIZE are frozen while EN = 1 (judged on the written value); the other bits still update.
        h.write32(ch(2, offset::CCR), config | EN);
        h.write32(ch(2, offset::CCR), EN | TCIE | PSIZE_8 | MSIZE_8);
        assert_eq!(h.read32(ch(2, offset::CCR)), EN | TCIE | PSIZE_16 | MSIZE_32);
        h.write32(ch(2, offset::CCR), PSIZE_8 | MSIZE_8);
        assert_eq!(h.read32(ch(2, offset::CCR)), PSIZE_8 | MSIZE_8);
        assert_eq!(h.peek(ch(2, offset::CNDTR), Width::Word), Some(77));
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn sizes_written_together_with_en_are_ignored() {
        // Renode parity: PSIZE/MSIZE only change when the written value has EN = 0, so a one-shot
        // "configure and enable" write keeps the constructor's byte sizes.
        let (mut h, dma, src) = rig(0x100);
        h.write32(ch(0, offset::CPAR), DR);
        h.write32(ch(0, offset::CMAR), SRAM);
        h.write32(ch(0, offset::CNDTR), 2);
        h.write32(ch(0, offset::CCR), EN | MINC | PSIZE_16 | MSIZE_16);
        assert_eq!(h.read32(ch(0, offset::CCR)), EN | MINC);
        h.set_input(dma, 0, true);
        h.set_input(dma, 0, true);
        // Two byte transfers of the low byte of 0x100 and 0x101.
        assert_eq!(halfwords(&h, 1), vec![0x0100]);
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 2);
    }

    #[test]
    fn unhandled_bits_and_reserved_sizes_warn() {
        let (mut h, _dma, _src) = rig(0);
        h.write32(ch(0, offset::CCR), (1 << 14) | MSIZE_16); // MEM2MEM is not modeled
        h.write32(ch(0, offset::CCR), 3 << 8); // reserved PSIZE
        assert_eq!(
            h.warnings(),
            vec![
                "Channel 0: some unhandled bits were written to configuration register. Value is 0x4400.".to_string(),
                "Channel 0: Invalid reserved value for size".to_string(),
            ]
        );
        // The reserved size is ignored, the valid one next to it applies.
        h.write32(ch(0, offset::CCR), (3 << 8) | MSIZE_16);
        assert_eq!(h.read32(ch(0, offset::CCR)), MSIZE_16);
    }

    #[test]
    fn peripheral_to_memory_halfword_transfers_run_per_request_with_flags_and_interrupts() {
        let (mut h, _dma, src) = rig(0x100);
        configure(&mut h, 0, EN | TCIE | HTIE | MINC | PSIZE_16 | MSIZE_16, 4, DR, SRAM);
        request(&mut h, src);
        request(&mut h, src);
        // Two of four data moved: half transfer flag, interrupt (HTIE), counter 2.
        assert_eq!(halfwords(&h, 4), vec![0x100, 0x101, 0, 0]);
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 2);
        assert_eq!(h.read32(DMA + offset::ISR), 0b101, "GIF1 (the line) and HTIF1");
        assert!(h.irq_level(11));
        assert!(!h.get::<FakeSource>(src).eoc, "the DR read cleared EOC during the transfer");
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 2);
        // Clearing HTIF drops the line.
        h.write32(DMA + offset::IFCR, 1 << 2);
        assert!(!h.irq_level(11));
        assert_eq!(h.read32(DMA + offset::ISR), 0);
        request(&mut h, src);
        request(&mut h, src);
        assert_eq!(halfwords(&h, 4), vec![0x100, 0x101, 0x102, 0x103]);
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 0);
        assert_eq!(h.read32(DMA + offset::ISR), 0b011, "GIF1 and TCIF1");
        assert!(h.irq_level(11));
        // EN stays set after the last transfer (Renode); a further request does nothing.
        assert_eq!(h.read32(ch(0, offset::CCR)) & EN, EN);
        request(&mut h, src);
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 4, "no data left: the source is not read");
        // GIF clears both flags.
        h.write32(DMA + offset::IFCR, 1);
        assert_eq!(h.read32(DMA + offset::ISR), 0);
        assert!(!h.irq_level(11));
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn requests_are_edge_driven_a_falling_level_alone_transfers_nothing() {
        let (mut h, dma, src) = rig(5);
        configure(&mut h, 0, EN | MINC | PSIZE_16 | MSIZE_16, 3, DR, SRAM);
        h.set_input(dma, 0, false);
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 0);
        h.set_input(dma, 0, true);
        h.set_input(dma, 0, false);
        assert_eq!(halfwords(&h, 1), vec![5]);
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 1);
    }

    #[test]
    fn circular_mode_reloads_the_count_and_wraps_the_destination() {
        let (mut h, _dma, src) = rig(10);
        configure(&mut h, 0, EN | TCIE | CIRC | MINC | PSIZE_16 | MSIZE_16, 3, DR, SRAM);
        for _ in 0..7 {
            request(&mut h, src);
        }
        // Writes went to offsets 0, 2, 4, 0, 2, 4, 0.
        assert_eq!(halfwords(&h, 3), vec![16, 14, 15]);
        // After the 7th request the counter is 2 of 3 (seventh = first of the third lap).
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 2);
        assert_eq!(h.read32(DMA + offset::ISR) & 0b11, 0b11, "TC was set at the end of each lap");
        h.write32(DMA + offset::IFCR, 1 << 1);
        assert_eq!(h.read32(DMA + offset::ISR) & 0b11, 0);
        assert!(!h.irq_level(11));
        // A lap boundary right after a reload: the counter equals the programmed value again.
        request(&mut h, src);
        request(&mut h, src);
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 3, "reloaded after the third transfer of the lap");
        assert_eq!(h.read32(DMA + offset::ISR) & 0b10, 0b10);
        assert!(h.irq_level(11));
    }

    #[test]
    fn memory_to_peripheral_writes_each_unit_to_the_peripheral_register() {
        let (mut h, dma, src) = rig(0);
        for (i, byte) in [0xA1u8, 0xB2, 0xC3].iter().enumerate() {
            h.write8(SRAM + i as u32, u32::from(*byte));
        }
        configure(&mut h, 1, EN | DIR_FROM_MEMORY | MINC | PSIZE_8 | MSIZE_8, 3, SRC + 0x44, SRAM);
        for _ in 0..3 {
            h.set_input(dma, 1, true);
            h.set_input(dma, 1, false);
        }
        let writes = h.get::<FakeSource>(src).writes.clone();
        assert_eq!(writes, vec![(0x44, 0xA1, Width::Byte), (0x44, 0xB2, Width::Byte), (0x44, 0xC3, Width::Byte)]);
        assert_eq!(h.read32(ch(1, offset::CNDTR)), 0);
        // With PINC the peripheral address advances by the unit size as well.
        configure(&mut h, 2, EN | DIR_FROM_MEMORY | MINC | PINC | PSIZE_8 | MSIZE_8, 2, SRC + 0x50, SRAM);
        for _ in 0..2 {
            h.set_input(dma, 2, true);
        }
        let writes = h.get::<FakeSource>(src).writes.clone();
        assert_eq!(&writes[3..], &[(0x50, 0xA1, Width::Byte), (0x51, 0xB2, Width::Byte)]);
    }

    #[test]
    fn word_sized_memory_to_peripheral_and_fixed_memory_address() {
        let (mut h, dma, src) = rig(0);
        h.write32(SRAM, 0x1122_3344);
        h.write32(SRAM + 4, 0x5566_7788);
        // MINC clear: the same memory word is sent twice.
        configure(&mut h, 0, EN | DIR_FROM_MEMORY | PSIZE_32 | MSIZE_32, 2, SRC + 0x44, SRAM);
        for _ in 0..2 {
            h.set_input(dma, 0, true);
        }
        let writes = h.get::<FakeSource>(src).writes.clone();
        assert_eq!(writes, vec![(0x44, 0x1122_3344, Width::Word), (0x44, 0x1122_3344, Width::Word)]);
        // Peripheral to memory without memory increment: the last value stays at the fixed address.
        configure(&mut h, 3, EN | PSIZE_32 | MSIZE_32, 3, DR, SRAM + 0x20);
        for _ in 0..3 {
            h.set_input(dma, 3, true);
        }
        assert_eq!(h.read32(SRAM + 0x20), 2, "the third conversion value (counter 0, 1, 2)");
        assert_eq!(h.read32(SRAM + 0x24), 0);
    }

    #[test]
    fn disabled_or_empty_channels_do_nothing() {
        let (mut h, dma, src) = rig(1);
        // Never configured: count 0.
        h.set_input(dma, 0, true);
        // Count programmed but channel disabled (the window while software reconfigures it).
        configure(&mut h, 0, MINC | PSIZE_16 | MSIZE_16, 4, DR, SRAM);
        h.set_input(dma, 0, false);
        h.set_input(dma, 0, true);
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 0);
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 4);
        assert_eq!(halfwords(&h, 1), vec![0]);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn interrupt_status_reports_the_line_not_the_flags_and_enabling_late_raises_nothing() {
        let (mut h, dma, _src) = rig(0);
        // TCIE clear: TC is flagged, GIF (the line) is not.
        configure(&mut h, 4, EN | MINC | PSIZE_16 | MSIZE_16, 1, DR, SRAM);
        h.set_input(dma, 4, true);
        h.set_input(dma, 4, false);
        assert_eq!(h.read32(DMA + offset::ISR), 1 << (4 * 4 + 1), "TCIF5 without GIF5");
        assert!(!h.irq_level(15));
        // Enabling TCIE afterwards does not raise the line (only flag changes re-evaluate it) ...
        h.write32(ch(4, offset::CCR), EN | TCIE | MINC | PSIZE_16 | MSIZE_16);
        assert!(!h.irq_level(15));
        assert_eq!(h.read32(DMA + offset::ISR), 1 << (4 * 4 + 1));
        // ... the next flag change does.
        configure(&mut h, 4, EN | TCIE | MINC | PSIZE_16 | MSIZE_16, 1, DR, SRAM + 2);
        h.set_input(dma, 4, true);
        assert!(h.irq_level(15));
        assert_eq!(h.read32(DMA + offset::ISR), 0b11 << (4 * 4));
        // Per-channel clear bits: TCIF only.
        h.write32(DMA + offset::IFCR, 1 << (4 * 4 + 1));
        assert!(!h.irq_level(15));
        assert_eq!(h.read32(DMA + offset::ISR), 0);
    }

    #[test]
    fn mixed_transfer_sizes_follow_the_engine_rules() {
        let (mut h, dma, _src) = rig(0x1234_5678);
        // 32-bit peripheral to 16-bit memory: size 4 is a multiple of 2, four bytes are written and the
        // destination advances by the source size.
        configure(&mut h, 0, EN | MINC | PSIZE_32 | MSIZE_16, 2, DR, SRAM);
        h.set_input(dma, 0, true);
        h.set_input(dma, 0, true);
        assert_eq!(h.read32(SRAM), 0x1234_5678);
        assert_eq!(h.read32(SRAM + 4), 0x1234_5679);
        // 16-bit peripheral to 32-bit memory cannot be copied by the engine: error, counters untouched.
        configure(&mut h, 1, EN | MINC | PSIZE_16 | MSIZE_32, 2, DR, SRAM + 0x40);
        h.set_input(dma, 1, true);
        assert_eq!(h.read32(ch(1, offset::CNDTR)), 2);
        assert_eq!(h.read32(SRAM + 0x40), 0);
        let errors: Vec<String> = h.core().log.entries().filter(|e| e.level == LogLevel::Error).map(|e| e.message.clone()).collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("Channel 1: DMA request cannot be served"), "{}", errors[0]);
    }

    #[test]
    fn unhandled_offsets_log_like_renode() {
        let (mut h, _dma, _src) = rig(0);
        // Messages seen in the Renode reference boot for dma1.
        assert_eq!(h.read32(DMA + offset::CSELR), 0);
        h.write32(DMA + offset::CSELR, 0x20_0000);
        // IFCR cannot be read; the seventh channel's CMAR (0x8C) is the last decoded register.
        assert_eq!(h.read32(DMA + offset::IFCR), 0);
        assert_eq!(h.read32(DMA + 0x9C), 0);
        assert_eq!(h.read32(DMA + 0x8C), 0);
        // A channel's reserved slot (offset 0x10 of its block) and unsupported widths.
        assert_eq!(h.read32(ch(0, 0x10)), 0);
        h.write32(ch(0, 0x10), 5);
        assert_eq!(h.read8(DMA), 0);
        assert_eq!(
            h.warnings(),
            vec![
                "Unhandled read from offset 0xA8.".to_string(),
                "Unhandled write to offset 0xA8, value 0x200000.".to_string(),
                "Unhandled read from offset 0x4.".to_string(),
                "Unhandled read from offset 0x9C.".to_string(),
                "Channel 0: unhandled read from 0x10.".to_string(),
                "Channel 0: unhandled write to 0x10, value 0x5.".to_string(),
                "dma1: Attempted Byte read isn't supported by the peripheral. Offset 0x0.".to_string(),
            ]
        );
    }

    #[test]
    fn requests_beyond_the_last_channel_warn() {
        let (mut h, dma, _src) = rig(0);
        h.set_input(dma, 8, true);
        assert_eq!(h.warnings(), vec!["Attempted to signal DMA channel 8. Maximum value is 7".to_string()]);
        h.set_input(dma, 7, true); // channel 7 exists in Renode even though its registers are not decoded
        assert_eq!(h.warnings().len(), 1);
    }

    #[test]
    fn reset_clears_channels_flags_and_interrupts() {
        let (mut h, _dma, src) = rig(0);
        configure(&mut h, 0, EN | TCIE | MINC | PSIZE_16 | MSIZE_16, 1, DR, SRAM);
        request(&mut h, src);
        assert!(h.irq_level(11));
        h.core_mut().reset_all();
        assert_eq!(h.read32(ch(0, offset::CCR)), 0);
        assert_eq!(h.read32(ch(0, offset::CNDTR)), 0);
        assert_eq!(h.read32(DMA + offset::ISR), 0);
        assert!(!h.irq_level(11));
        // Reset zeroes the transfer sizes (the constructor uses bytes): an enabled channel cannot copy until
        // the sizes are programmed again.
        h.write32(ch(0, offset::CNDTR), 1);
        h.write32(ch(0, offset::CCR), EN);
        assert_eq!(h.read32(ch(0, offset::CCR)), EN);
    }

    #[test]
    fn request_transfer_is_one_based() {
        let (mut h, dma, src) = rig(9);
        configure(&mut h, 0, EN | MINC | PSIZE_16 | MSIZE_16, 2, DR, SRAM);
        h.with::<Dma, _>(dma, |d, ctx| d.request_transfer(ctx, 1));
        assert_eq!(halfwords(&h, 1), vec![9]);
        h.with::<Dma, _>(dma, |d, ctx| d.request_transfer(ctx, 0));
        assert_eq!(h.get::<FakeSource>(src).dr_reads, 1);
        assert_eq!(h.warnings(), vec!["Invalid channel 0, no transfer performed.".to_string()]);
    }

    #[test]
    fn summary_lists_active_channels() {
        let (mut h, dma, _src) = rig(0);
        configure(&mut h, 2, EN | MINC | PSIZE_16 | MSIZE_16, 4, DR, SRAM);
        h.set_input(dma, 2, true);
        let summaries = h.core().summaries();
        assert_eq!(summaries[0], ("dma1".to_string(), "dma1: ch2[en=1 n=3/4 dir=per->mem circ=0 tc=0 ht=0]".to_string()));
    }
}
