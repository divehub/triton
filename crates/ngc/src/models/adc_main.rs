// Ported from emulation/models/NGCMainADC.cs.

//! `NGCMainADC`: the functional STM32L4 ADC1 of the main 5.8 board (`adc @ 0x50040000`): configurable raw
//! analog inputs, a delayed regular sequence, and a DMA request per conversion. It never writes derived
//! battery, oxygen or application values: the firmware reads `DR` (through DMA1 channel 1) like on the
//! device. Reference voltage (2500 mV), battery divider (1.68), oxygen gain (10) and the 100 us per-rank
//! conversion interval are explicit fixtures inferred from the firmware's conversion code, not measurements.
//!
//! # Conversion model
//!
//! A managed thread fires every `1_000_000 / conversionIntervalUs` Hz (integer division; 10 kHz, i.e.
//! every 100 000 ns, by default) from the moment the peripheral exists. Each tick:
//!
//! 1. completes a pending calibration (`ADCAL` self-clears) and a pending enable (`ISR.ADRDY` is raised), so
//!    both take one tick (calibration and enable are not instantaneous);
//! 2. does nothing else unless a regular sequence is running (`CR.ADSTART` accepted with `ADEN` set);
//! 3. while `AcquisitionEnabled` is false it holds the sequence without producing flags or DMA requests;
//! 4. while the *initial acquisition delay* (applied to the first sequence started after a reset) has not
//!    elapsed it counts down `settlingTicks`;
//! 5. otherwise converts one rank: the rank's channel is taken from `SQR1..SQR4`, `DR` receives the sample of
//!    the sequence's input **snapshot** (all six inputs are latched together when a sequence starts or
//!    restarts, so a mid-scan change appears in the next scan), `ISR.EOC` is set (`OVR` if it was still set),
//!    after the last rank `ISR.EOS`; with `CFGR.DMAEN` a DMA request pulse is emitted; after the last rank
//!    the sequence restarts with a fresh snapshot in continuous mode (`CFGR.CONT`) or stops (`ADSTART` cleared).
//!
//! The IRQ output is `ISR & IER & 0x7FF != 0`, recomputed after every ISR/IER/CR write, `DR` read and tick.
//!
//! # DMA ordering (the tick is split in two events)
//!
//! Renode raises the DMA request *inside* the tick; the DMA model reads `DR` through the bus at once, which
//! clears `EOC` and recomputes the IRQ line, and only then does the tick finish (restart/stop the sequence and
//! recompute the IRQ). The framework delivers a `set_output` request when the sender's call returns, so the
//! tick is split exactly there: [`NgcMainAdc::on_event`] part one converts the rank and pulses
//! [`DMA_REQUEST`]; part two (an event scheduled for the same instant, run after the queued delivery and before
//! the CPU resumes) does the end-of-sequence handling and the final IRQ update. Without a DMA request (no
//! `DMAEN`) the tick runs in one piece. This keeps the Renode observable order: no transient IRQ pulse for the
//! `EOC` that the DMA consumes. One difference remains: part two is an ordinary event, so it runs after the
//! other *clock* events that expire in the same nanosecond (Renode runs it inside the tick).
//!
//! Accesses: 16-bit and 32-bit only (`IWordPeripheral` + `IDoubleWordPeripheral`, no translations).
//! `ReadWord` is a full 32-bit read (side effects included) shifted by `offset & 2`; `WriteWord` merges the
//! halfword into the **stored** word and performs a 32-bit write.
//!
//! # Wiring (`main.repl`)
//!
//! ```text
//! adc: Analog.NGCMainADC @ sysbus 0x50040000        add_mapped(0x5004_0000, adc_main::SIZE, NgcMainAdc::with_defaults("adc"))
//!     DMARequest -> dma1@0                          connect_input(adc, adc_main::DMA_REQUEST, dma1, 0)
//!     IRQ -> nvic@18                                connect_irq(adc, adc_main::IRQ, 18)
//! ```
//!
//! # Runner values
//!
//! The Renode monitor parses `double` arguments through single precision (`adc ReferenceMillivolts 0.1` stores
//! 0.10000000149011612), so the values the Python runner sent (`SetBatteryMillivolts`, `SetOxygenMillivolts`) were
//! `f32`-rounded before the C# model saw them. The Rust methods take the exact `f64` they are given; a caller that
//! wants bit-identical raw codes at rounding boundaries applies `f64::from(value as f32)` first (the differential
//! test does).

use super::clock_control::WordStore;
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, ManagedThread, Peripheral, Time, Translations, View, Width, Widths};
use std::fmt;

/// `Size`: the register window.
pub const SIZE: u32 = 0x400;
/// Output line 0: `DMARequest` (`.repl`: `DMARequest -> dma1@0`).
pub const DMA_REQUEST: u32 = 0;
/// Output line 1: `IRQ` (`.repl`: `IRQ -> nvic@18`).
pub const IRQ: u32 = 1;
/// Number of input channels (`inputs[32]`).
pub const CHANNELS: usize = 32;

/// Register offsets and the bits the model interprets.
pub mod reg {
    pub const ISR: u32 = 0x00;
    pub const IER: u32 = 0x04;
    pub const CR: u32 = 0x08;
    pub const CFGR: u32 = 0x0C;
    pub const SQR1: u32 = 0x30;
    pub const SQR2: u32 = 0x34;
    pub const SQR3: u32 = 0x38;
    pub const SQR4: u32 = 0x3C;
    pub const DR: u32 = 0x40;

    pub const ISR_ADRDY: u32 = 1 << 0;
    pub const ISR_EOC: u32 = 1 << 2;
    pub const ISR_EOS: u32 = 1 << 3;
    pub const ISR_OVR: u32 = 1 << 4;
    pub const CR_ADEN: u32 = 1 << 0;
    pub const CR_ADDIS: u32 = 1 << 1;
    pub const CR_ADSTART: u32 = 1 << 2;
    pub const CR_ADSTP: u32 = 1 << 4;
    pub const CR_ADCAL: u32 = 1 << 31;
    pub const CFGR_DMAEN: u32 = 1 << 0;
    pub const CFGR_CONT: u32 = 1 << 13;
}
use reg::*;

/// `CR` after reset (`DEEPPWD`).
const CR_RESET_VALUE: u32 = 0x2000_0000;

/// Token of the conversion thread.
const TICK: u64 = 1;
/// Token of the second half of a tick that issued a DMA request.
const FINISH: u64 = 2;

/// Why a constructor or setter refused its argument (the C# exceptions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdcError {
    /// `ArgumentOutOfRangeException("conversionIntervalUs")`.
    BadConversionInterval(u32),
    /// `ArgumentOutOfRangeException("channel")`.
    ChannelOutOfRange(u32),
    /// `ArgumentOutOfRangeException("bank")`.
    BankOutOfRange(u32),
    /// `ArgumentOutOfRangeException("cell")`.
    CellOutOfRange(u32),
    /// `ArgumentOutOfRangeException("millivolts")`: NaN, infinite or negative.
    BadMillivolts,
    /// `InvalidOperationException("ReferenceMillivolts must be positive")`.
    BadReference,
}

impl fmt::Display for AdcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdcError::BadConversionInterval(us) => write!(f, "conversionIntervalUs {us} must be in 1..=1000000"),
            AdcError::ChannelOutOfRange(channel) => write!(f, "channel {channel} is outside 0..={}", CHANNELS - 1),
            AdcError::BankOutOfRange(bank) => write!(f, "bank {bank} is outside 0..=1"),
            AdcError::CellOutOfRange(cell) => write!(f, "cell {cell} is outside 0..=2"),
            AdcError::BadMillivolts => write!(f, "millivolts must be a finite non-negative number"),
            AdcError::BadReference => write!(f, "ReferenceMillivolts must be positive"),
        }
    }
}

impl std::error::Error for AdcError {}

/// Constructor arguments of `NGCMainADC` with the C# defaults (the values of `main.repl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdcConfig {
    pub oxygen1_raw: u32,
    pub oxygen2_raw: u32,
    pub oxygen3_raw: u32,
    pub battery1_raw: u32,
    pub battery2_raw: u32,
    pub board_id_raw: u32,
    pub conversion_interval_us: u32,
}

impl Default for AdcConfig {
    fn default() -> Self {
        Self {
            oxygen1_raw: 164,
            oxygen2_raw: 164,
            oxygen3_raw: 164,
            battery1_raw: 1463,
            battery2_raw: 1463,
            board_id_raw: 500,
            conversion_interval_us: 100,
        }
    }
}

/// C# `bool.ToString()`.
fn cs_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// `Analog.NGCMainADC`.
pub struct NgcMainAdc {
    name: String,
    conversion_interval_us: u32,
    reference_millivolts: f64,
    registers: WordStore,
    inputs: [u32; CHANNELS],
    snapshot: [u32; CHANNELS],
    converting: bool,
    calibration_pending: bool,
    enable_pending: bool,
    acquisition_enabled: bool,
    has_started_sequence: bool,
    initial_acquisition_delay_us: u32,
    settling_ticks: u32,
    noise_amplitude_raw: u32,
    noise_seed: u32,
    rank: u32,
    conversion_count: u64,
    sequence_count: u64,
    calibration_count: u64,
    last_channel: u32,
    /// Part two of a tick is due (`FINISH` scheduled) and the sequence length it must use.
    finish_pending: bool,
    finish_length: u32,
    converter: ManagedThread,
}

impl NgcMainAdc {
    /// `new NGCMainADC(machine)` with the platform defaults, named `name` (`adc` in `main.repl`).
    pub fn with_defaults(name: impl Into<String>) -> Self {
        Self::new(name, AdcConfig::default()).expect("the default configuration is valid")
    }

    /// `new NGCMainADC(machine, oxygen1Raw, ..., conversionIntervalUs)`.
    pub fn new(name: impl Into<String>, config: AdcConfig) -> Result<Self, AdcError> {
        let us = config.conversion_interval_us;
        if us == 0 || us > 1_000_000 {
            return Err(AdcError::BadConversionInterval(us));
        }
        let mut inputs = [0u32; CHANNELS];
        inputs[1] = config.oxygen1_raw & 0xFFF;
        inputs[2] = config.oxygen2_raw & 0xFFF;
        inputs[3] = config.oxygen3_raw & 0xFFF;
        inputs[4] = config.battery1_raw & 0xFFF;
        inputs[9] = config.battery2_raw & 0xFFF;
        inputs[6] = config.board_id_raw & 0xFFF;
        let mut adc = Self {
            name: name.into(),
            conversion_interval_us: us,
            reference_millivolts: 2500.0,
            registers: WordStore::new(),
            inputs,
            snapshot: [0; CHANNELS],
            converting: false,
            calibration_pending: false,
            enable_pending: false,
            acquisition_enabled: true,
            has_started_sequence: false,
            initial_acquisition_delay_us: 0,
            settling_ticks: 0,
            noise_amplitude_raw: 0,
            noise_seed: 1,
            rank: 0,
            conversion_count: 0,
            sequence_count: 0,
            calibration_count: 0,
            last_channel: 0,
            finish_pending: false,
            finish_length: 0,
            converter: ManagedThread::new(u64::from(1_000_000 / us), TICK),
        };
        adc.reset_state();
        Ok(adc)
    }

    /// The part of `Reset()` that does not touch the output lines. Inputs and the acquisition/noise
    /// settings are configuration and survive a reset, like in C#.
    fn reset_state(&mut self) {
        self.registers.clear();
        self.registers.set(reg::CR, CR_RESET_VALUE);
        self.rank = 0;
        self.converting = false;
        self.calibration_pending = false;
        self.enable_pending = false;
        self.settling_ticks = 0;
        self.has_started_sequence = false;
        self.conversion_count = 0;
        self.sequence_count = 0;
        self.calibration_count = 0;
        self.last_channel = 0;
        self.finish_pending = false;
    }

    // ---- C# properties ----

    /// `ConversionIntervalUs`.
    pub fn conversion_interval_us(&self) -> u32 {
        self.conversion_interval_us
    }

    /// `ReferenceMillivolts` (default 2500).
    pub fn reference_millivolts(&self) -> f64 {
        self.reference_millivolts
    }

    pub fn set_reference_millivolts(&mut self, millivolts: f64) {
        self.reference_millivolts = millivolts;
    }

    /// `AcquisitionEnabled` (default true): false holds the running sequence.
    pub fn acquisition_enabled(&self) -> bool {
        self.acquisition_enabled
    }

    pub fn set_acquisition_enabled(&mut self, enabled: bool) {
        self.acquisition_enabled = enabled;
    }

    /// `InitialAcquisitionDelayUs`: settling delay applied to the first sequence started after a reset.
    pub fn initial_acquisition_delay_us(&self) -> u32 {
        self.initial_acquisition_delay_us
    }

    pub fn set_initial_acquisition_delay_us(&mut self, microseconds: u32) {
        self.initial_acquisition_delay_us = microseconds;
    }

    /// `NoiseAmplitudeRaw`: bounded code noise (0 = off); the setter clamps to 4095.
    pub fn noise_amplitude_raw(&self) -> u32 {
        self.noise_amplitude_raw
    }

    pub fn set_noise_amplitude_raw(&mut self, amplitude: u32) {
        self.noise_amplitude_raw = amplitude.min(4095);
    }

    /// `NoiseSeed` (default 1).
    pub fn noise_seed(&self) -> u32 {
        self.noise_seed
    }

    pub fn set_noise_seed(&mut self, seed: u32) {
        self.noise_seed = seed;
    }

    /// `Enabled`: `CR.ADEN`.
    pub fn enabled(&self) -> bool {
        self.registers.get(reg::CR) & CR_ADEN != 0
    }

    /// `Converting`: a regular sequence is running.
    pub fn converting(&self) -> bool {
        self.converting
    }

    /// `DataReady`: `ISR.EOC` is set.
    pub fn data_ready(&self) -> bool {
        self.registers.get(reg::ISR) & ISR_EOC != 0
    }

    /// `ConversionCount`: ranks converted since reset.
    pub fn conversion_count(&self) -> u64 {
        self.conversion_count
    }

    /// `SequenceCount`: completed sequences since reset.
    pub fn sequence_count(&self) -> u64 {
        self.sequence_count
    }

    /// `CalibrationCount`: `ADCAL` requests since reset.
    pub fn calibration_count(&self) -> u64 {
        self.calibration_count
    }

    /// `LastChannel`: the channel of the last converted rank.
    pub fn last_channel(&self) -> u32 {
        self.last_channel
    }

    /// `settlingTicks` still to elapse (diagnostics).
    pub fn settling_ticks(&self) -> u32 {
        self.settling_ticks
    }

    /// The stored word at `offset` (`Get(offset)`).
    pub fn stored(&self, offset: u32) -> u32 {
        self.registers.get(offset)
    }

    /// `GetInput(channel)`: the raw input pin value (12 bit), not the latched snapshot.
    pub fn input_raw(&self, channel: u32) -> Option<u32> {
        self.inputs.get(channel as usize).copied()
    }

    /// `Oxygen1Raw`, ... (channels 1..3).
    pub fn oxygen_raw(&self, cell: u32) -> Option<u32> {
        (cell < 3).then(|| self.inputs[cell as usize + 1])
    }

    /// `Battery1Raw` (channel 4) / `Battery2Raw` (channel 9).
    pub fn battery_raw(&self, bank: u32) -> Option<u32> {
        match bank {
            0 => Some(self.inputs[4]),
            1 => Some(self.inputs[9]),
            _ => None,
        }
    }

    /// `BoardIdRaw` (channel 6).
    pub fn board_id_raw(&self) -> u32 {
        self.inputs[6]
    }

    // ---- runner / monitor methods ----

    /// `SetRawInputs`: the six inputs in the order O2-1, O2-2, O2-3, B1, B2, board ID (12 bit each). They are
    /// latched together at the next sequence boundary.
    pub fn set_raw_inputs(&mut self, oxygen1: u32, oxygen2: u32, oxygen3: u32, battery1: u32, battery2: u32, board_id: u32) {
        self.inputs[1] = oxygen1 & 0xFFF;
        self.inputs[2] = oxygen2 & 0xFFF;
        self.inputs[3] = oxygen3 & 0xFFF;
        self.inputs[4] = battery1 & 0xFFF;
        self.inputs[9] = battery2 & 0xFFF;
        self.inputs[6] = board_id & 0xFFF;
    }

    /// `SetChannelRaw(channel, value)`.
    pub fn set_channel_raw(&mut self, channel: u32, value: u32) -> Result<(), AdcError> {
        let slot = self.inputs.get_mut(channel as usize).ok_or(AdcError::ChannelOutOfRange(channel))?;
        *slot = value & 0xFFF;
        Ok(())
    }

    /// `Oxygen1Raw`/`Oxygen2Raw`/`Oxygen3Raw = value` (`cell` 0..=2 are channels 1..=3).
    pub fn set_oxygen_raw(&mut self, cell: u32, value: u32) -> Result<(), AdcError> {
        if cell > 2 {
            return Err(AdcError::CellOutOfRange(cell));
        }
        self.set_channel_raw(cell + 1, value)
    }

    /// `Battery1Raw` (`bank` 0, channel 4) / `Battery2Raw` (`bank` 1, channel 9) `= value`.
    pub fn set_battery_raw(&mut self, bank: u32, value: u32) -> Result<(), AdcError> {
        match bank {
            0 => self.set_channel_raw(4, value),
            1 => self.set_channel_raw(9, value),
            _ => Err(AdcError::BankOutOfRange(bank)),
        }
    }

    /// `BoardIdRaw = value` (channel 6).
    pub fn set_board_id_raw(&mut self, value: u32) {
        self.inputs[6] = value & 0xFFF;
    }

    /// `SetPinMillivolts(channel, mV)`: `round(mV * 4096 / ReferenceMillivolts)` (ties to even, as
    /// `Math.Round`), clipped to 4095.
    pub fn set_pin_millivolts(&mut self, channel: u32, millivolts: f64) -> Result<(), AdcError> {
        if millivolts.is_nan() || millivolts.is_infinite() || millivolts < 0.0 {
            return Err(AdcError::BadMillivolts);
        }
        if self.reference_millivolts <= 0.0 {
            return Err(AdcError::BadReference);
        }
        // Renode parity: `Math.Round(double)` rounds half to even (Rust's `round` would round half away from
        // zero) and the product is formed before the division, exactly as the C# expression.
        let raw = (millivolts * 4096.0 / self.reference_millivolts).round_ties_even();
        self.set_channel_raw(channel, raw.min(4095.0) as u32)
    }

    /// `SetBatteryMillivolts(bank, mV)`: the firmware's uncalibrated divider factor 1.68 (bank 0 is channel 4,
    /// bank 1 channel 9).
    pub fn set_battery_millivolts(&mut self, bank: u32, millivolts: f64) -> Result<(), AdcError> {
        if bank > 1 {
            return Err(AdcError::BankOutOfRange(bank));
        }
        self.set_pin_millivolts(if bank == 0 { 4 } else { 9 }, millivolts / 1.68)
    }

    /// `SetOxygenMillivolts(cell, mV)`: the firmware fallback reports cell mV as ADC pin mV / 10.
    pub fn set_oxygen_millivolts(&mut self, cell: u32, millivolts: f64) -> Result<(), AdcError> {
        if cell > 2 {
            return Err(AdcError::CellOutOfRange(cell));
        }
        self.set_pin_millivolts(cell + 1, millivolts * 10.0)
    }

    /// C# `Summary`.
    pub fn describe(&self) -> String {
        format!(
            "Main ADC fixture: conversions={}; sequences={}; calibrations={}; lastChannel={}; rankIntervalUs={}; CR=0x{:X}; ISR=0x{:X}; CFGR=0x{:X}; SQR1=0x{:X}; SQR2=0x{:X}; acquisitionEnabled={}; initialDelayUs={}; settlingTicks={}; noiseRaw={}",
            self.conversion_count,
            self.sequence_count,
            self.calibration_count,
            self.last_channel,
            self.conversion_interval_us,
            self.get(reg::CR),
            self.get(reg::ISR),
            self.get(reg::CFGR),
            self.get(reg::SQR1),
            self.get(reg::SQR2),
            cs_bool(self.acquisition_enabled),
            self.initial_acquisition_delay_us,
            self.settling_ticks,
            self.noise_amplitude_raw
        )
    }

    // ---- model ----

    #[inline]
    fn get(&self, offset: u32) -> u32 {
        self.registers.get(offset)
    }

    fn update_irq(&self, ctx: &mut Ctx<'_>) {
        ctx.set_output(IRQ, self.get(reg::ISR) & self.get(reg::IER) & 0x7FF != 0);
    }

    /// Latches the inputs, adding the deterministic noise (`Snapshot()`).
    fn take_snapshot(&mut self) {
        self.snapshot = self.inputs;
        let amplitude = self.noise_amplitude_raw;
        if amplitude == 0 {
            return;
        }
        for (channel, sample) in self.snapshot.iter_mut().enumerate() {
            // Reproducible bounded code noise, not a model of real electrical noise.
            let mut hash = self.noise_seed
                ^ (self.sequence_count as u32).wrapping_mul(1_664_525)
                ^ (channel as u32).wrapping_mul(1_013_904_223);
            hash ^= hash << 13;
            hash ^= hash >> 17;
            hash ^= hash << 5;
            let delta = (hash % (amplitude * 2 + 1)) as i32 - amplitude as i32;
            *sample = (*sample as i32 + delta).clamp(0, 4095) as u32;
        }
    }

    /// `Tick()`, up to and including the DMA request; the rest is [`NgcMainAdc::finish_tick`].
    fn tick(&mut self, ctx: &mut Ctx<'_>) {
        if self.calibration_pending {
            self.calibration_pending = false;
            self.registers.set(reg::CR, self.get(reg::CR) & !CR_ADCAL);
        }
        if self.enable_pending {
            self.enable_pending = false;
            self.registers.set(reg::ISR, self.get(reg::ISR) | ISR_ADRDY);
        }
        if !self.converting || !self.acquisition_enabled {
            self.update_irq(ctx);
            return;
        }
        if self.settling_ticks != 0 {
            self.settling_ticks -= 1;
            self.update_irq(ctx);
            return;
        }
        let length = (self.get(reg::SQR1) & 0xF) + 1;
        let rank = self.rank;
        let (sqr, shift) = match rank {
            0..=3 => (reg::SQR1, (rank + 1) * 6),
            4..=8 => (reg::SQR2, (rank - 4) * 6),
            9..=13 => (reg::SQR3, (rank - 9) * 6),
            _ => (reg::SQR4, (rank - 14) * 6),
        };
        let channel = (self.get(sqr) >> shift) & 0x1F;
        self.last_channel = channel;
        if self.get(reg::ISR) & ISR_EOC != 0 {
            self.registers.set(reg::ISR, self.get(reg::ISR) | ISR_OVR);
        }
        self.registers.set(reg::DR, self.snapshot[channel as usize] & 0xFFF);
        self.registers.set(reg::ISR, self.get(reg::ISR) | ISR_EOC);
        self.conversion_count += 1;
        self.rank += 1;
        if self.rank >= length {
            self.registers.set(reg::ISR, self.get(reg::ISR) | ISR_EOS);
            self.sequence_count += 1;
        }
        // DMA reads the real ADC DR through the bus. The ADC never writes the application buffer or
        // derived sensor fields directly.
        if self.get(reg::CFGR) & CFGR_DMAEN != 0 {
            // Renode parity: the request pulse is raised in the middle of the tick, the receiver reads DR (clearing
            // EOC and recomputing the IRQ line) before the end-of-sequence handling and the final IRQ update run.
            ctx.set_output(DMA_REQUEST, true);
            ctx.set_output(DMA_REQUEST, false);
            // The receiver runs when this call returns; the end of the tick follows at the same instant.
            self.finish_pending = true;
            self.finish_length = length;
            ctx.schedule_at(ctx.now(), FINISH);
            return;
        }
        self.finish_sequence(length, ctx);
    }

    /// Part two of a tick that issued a DMA request.
    fn finish_tick(&mut self, ctx: &mut Ctx<'_>) {
        if !self.finish_pending {
            return;
        }
        self.finish_pending = false;
        self.finish_sequence(self.finish_length, ctx);
    }

    /// End of a tick: restart or stop the sequence after its last rank, then recompute the IRQ.
    fn finish_sequence(&mut self, length: u32, ctx: &mut Ctx<'_>) {
        if self.rank >= length {
            if self.get(reg::CFGR) & CFGR_CONT != 0 {
                self.rank = 0;
                self.take_snapshot();
            } else {
                self.converting = false;
                self.registers.set(reg::CR, self.get(reg::CR) & !CR_ADSTART);
            }
        }
        self.update_irq(ctx);
    }

    fn read_dword(&mut self, offset: u32, ctx: &mut Ctx<'_>) -> u32 {
        let value = self.get(offset);
        if offset == reg::DR {
            // DR consumes EOC; EOS is cleared through ISR write-one-to-clear.
            self.registers.set(reg::ISR, self.get(reg::ISR) & !ISR_EOC);
            self.update_irq(ctx);
        }
        value
    }

    fn write_dword(&mut self, offset: u32, value: u32, ctx: &mut Ctx<'_>) {
        match offset {
            reg::ISR => {
                // ISR write one to clear.
                self.registers.set(reg::ISR, self.get(reg::ISR) & !value);
                self.update_irq(ctx);
            }
            reg::IER => {
                self.registers.set(reg::IER, value);
                self.update_irq(ctx);
            }
            reg::CR => {
                // Renode parity: ADEN is sticky (an ADEN zero write preserves an enabled ADC; HAL starts a conversion
                // with ADSTART = 1 and ADEN = 0 in the written value), ADCAL stays set until the next tick, ADDIS
                // is never stored.
                let mut control = value | (self.get(reg::CR) & CR_ADEN);
                if value & CR_ADCAL != 0 {
                    self.calibration_pending = true;
                    self.calibration_count += 1;
                }
                if value & CR_ADDIS != 0 {
                    control &= !(CR_ADEN | CR_ADDIS);
                    self.registers.set(reg::ISR, self.get(reg::ISR) & !ISR_ADRDY);
                    self.converting = false;
                    self.enable_pending = false;
                } else if value & CR_ADEN != 0 && self.get(reg::CR) & CR_ADEN == 0 {
                    self.enable_pending = true;
                }
                if value & CR_ADSTP != 0 {
                    self.converting = false;
                    control &= !(CR_ADSTART | CR_ADSTP);
                } else if value & CR_ADSTART != 0 && control & CR_ADEN != 0 && !self.converting {
                    self.converting = true;
                    self.rank = 0;
                    // Unchecked uint arithmetic in C#.
                    self.settling_ticks = if self.has_started_sequence {
                        0
                    } else {
                        self.initial_acquisition_delay_us
                            .wrapping_add(self.conversion_interval_us)
                            .wrapping_sub(1)
                            / self.conversion_interval_us
                    };
                    self.has_started_sequence = true;
                    self.take_snapshot();
                }
                self.registers.set(reg::CR, control & !CR_ADDIS);
                self.update_irq(ctx);
            }
            _ => self.registers.set(offset, value),
        }
    }

    fn read_word(&mut self, offset: u32, ctx: &mut Ctx<'_>) -> u32 {
        (self.read_dword(offset & !3, ctx) >> ((offset & 2) * 8)) & 0xFFFF
    }

    fn write_word(&mut self, offset: u32, value: u32, ctx: &mut Ctx<'_>) {
        let aligned = offset & !3;
        let shift = (offset & 2) * 8;
        let merged = (self.get(aligned) & !(0xFFFFu32 << shift)) | ((value & 0xFFFF) << shift);
        self.write_dword(aligned, merged, ctx);
    }
}

impl Peripheral for NgcMainAdc {
    fn name(&self) -> &str {
        &self.name
    }

    /// The conversion thread exists and runs from the moment the peripheral is created.
    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.converter.attach(ctx);
        self.converter.start(ctx);
    }

    /// `Reset`: registers, counters and the running sequence; inputs, acquisition and noise settings stay.
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.reset_state();
        ctx.set_output(IRQ, false);
        ctx.set_output(DMA_REQUEST, false);
    }

    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match width {
            Width::Word => self.read_dword(offset, ctx),
            Width::Half => self.read_word(offset, ctx),
            // Not delivered: the bus rejects byte accesses for this policy.
            Width::Byte => 0,
        }
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match width {
            Width::Word => self.write_dword(offset, value, ctx),
            Width::Half => self.write_word(offset, value, ctx),
            Width::Byte => {}
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        match token {
            TICK => self.tick(ctx),
            FINISH => self.finish_tick(ctx),
            _ => {}
        }
    }

    // IDoubleWordPeripheral + IWordPeripheral, no [AllowedTranslations]: byte accesses are not supported.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::new(Widths::HALF | Widths::WORD, Translations::NONE)
    }

    /// `DR` and the other registers read back their stored value; nothing is consumed.
    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        match width {
            Width::Word => Some(self.get(offset)),
            Width::Half => Some((self.get(offset & !3) >> ((offset & 2) * 8)) & 0xFFFF),
            Width::Byte => None,
        }
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.describe()
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{PeriphId, TICKS_PER_MICROSECOND as US};
    use std::any::Any;

    const BASE: u32 = 0x5004_0000;
    const ISR: u32 = BASE + reg::ISR;
    const IER: u32 = BASE + reg::IER;
    const CR: u32 = BASE + reg::CR;
    const CFGR: u32 = BASE + reg::CFGR;
    const SQR1: u32 = BASE + reg::SQR1;
    const SQR2: u32 = BASE + reg::SQR2;
    const DR: u32 = BASE + reg::DR;

    /// DMA request receiver: on a rising edge it reads `DR` through the bus like the STM32LDMA model.
    struct DmaReader {
        reads: Vec<(Time, u32)>,
        edges: Vec<(Time, bool)>,
    }

    impl Peripheral for DmaReader {
        fn name(&self) -> &str {
            "dma"
        }
        fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}
        fn on_input(&mut self, _line: u32, level: bool, ctx: &mut Ctx<'_>) {
            self.edges.push((ctx.now(), level));
            if level {
                let value = ctx.mem_read(BASE + reg::DR, Width::Word);
                self.reads.push((ctx.now(), value));
            }
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    struct Rig {
        h: Harness,
        adc: PeriphId,
        dma: Option<PeriphId>,
    }

    impl Rig {
        fn new() -> Rig {
            Self::with_config(AdcConfig::default(), false)
        }

        fn with_config(config: AdcConfig, with_dma: bool) -> Rig {
            let mut h = Harness::new();
            let adc = h.add_mapped(BASE, SIZE, NgcMainAdc::new("adc", config).unwrap());
            h.connect_irq(adc, IRQ, 18);
            h.clear_irq_changes();
            let dma = with_dma.then(|| {
                let dma = h.add(DmaReader { reads: Vec::new(), edges: Vec::new() });
                h.connect_input(adc, DMA_REQUEST, dma, 0);
                // `connect` pushes the current (low) level to the new receiver: forget that initial entry.
                assert_eq!(h.get::<DmaReader>(dma).edges, [(0, false)]);
                h.get_mut::<DmaReader>(dma).edges.clear();
                dma
            });
            Rig { h, adc, dma }
        }

        fn adc(&self) -> &NgcMainAdc {
            self.h.get::<NgcMainAdc>(self.adc)
        }

        fn adc_mut(&mut self) -> &mut NgcMainAdc {
            self.h.get_mut::<NgcMainAdc>(self.adc)
        }

        fn dma(&self) -> &DmaReader {
            self.h.get::<DmaReader>(self.dma.unwrap())
        }

        /// The HAL start-up the main firmware performs: leave deep power down, calibrate, enable, configure the
        /// six-rank sequence (`SQR1 = 0x040C2045`, `SQR2 = 0x246`), then start.
        fn start_six_rank(&mut self, cfgr: u32) {
            self.h.write32(CR, 0x9000_0000);
            self.h.advance_by(US * 100);
            self.h.write32(CR, 0x1000_0001);
            self.h.advance_by(US * 100);
            self.h.write32(SQR1, 0x040C_2045);
            self.h.write32(SQR2, 0x246);
            self.h.write32(CFGR, cfgr);
            self.h.write32(CR, 0x1000_0004);
        }
    }

    #[test]
    fn reset_state_and_register_storage() {
        let mut rig = Rig::new();
        assert_eq!(rig.h.read32(CR), 0x2000_0000);
        for offset in [0x00, 0x04, 0x0C, 0x30, 0x34, 0x38, 0x3C, 0x40, 0x100, 0x3FC] {
            assert_eq!(rig.h.read32(BASE + offset), 0, "offset 0x{offset:X}");
        }
        assert_eq!(rig.h.read8(BASE), 0, "byte access is not supported");
        assert_eq!(rig.h.warnings().len(), 1);
        rig.h.write32(BASE + 0x100, 0xDEAD_BEEF);
        rig.h.write32(CFGR, 0x12345);
        assert_eq!(rig.h.read32(BASE + 0x100), 0xDEAD_BEEF);
        assert_eq!(rig.h.read32(CFGR), 0x12345);
        // Halfword access: the read is a full 32-bit read shifted by offset & 2, the write merges into the stored word.
        assert_eq!(rig.h.read16(CFGR), 0x2345);
        assert_eq!(rig.h.read16(CFGR + 2), 0x1);
        assert_eq!(rig.h.read16(CFGR + 1), 0x2345, "bit 0 of the offset is ignored");
        rig.h.write16(CFGR + 2, 0x00AA);
        assert_eq!(rig.h.read32(CFGR), 0x00AA_2345);
        rig.h.write16(CFGR, 0xFFFF);
        assert_eq!(rig.h.read32(CFGR), 0x00AA_FFFF);
        assert_eq!(rig.adc().conversion_count(), 0);
        assert_eq!(rig.h.peek(CFGR, Width::Word), Some(0x00AA_FFFF));
        assert_eq!(rig.h.peek(BASE, Width::Byte), None);
    }

    #[test]
    fn defaults_and_input_conversions_match_the_csharp_fixture() {
        let mut rig = Rig::new();
        let adc = rig.adc();
        assert_eq!(
            (adc.oxygen_raw(0), adc.oxygen_raw(1), adc.oxygen_raw(2), adc.battery_raw(0), adc.battery_raw(1), adc.board_id_raw()),
            (Some(164), Some(164), Some(164), Some(1463), Some(1463), 500)
        );
        assert_eq!(adc.reference_millivolts(), 2500.0);
        // The runner defaults reproduce the constructor defaults: 1500 mV batteries -> 1463, 10 mV cells -> 164.
        rig.adc_mut().set_battery_millivolts(0, 1500.0).unwrap();
        rig.adc_mut().set_battery_millivolts(1, 1500.0).unwrap();
        for cell in 0..3 {
            rig.adc_mut().set_oxygen_millivolts(cell, 10.0).unwrap();
        }
        let adc = rig.adc();
        assert_eq!((adc.battery_raw(0), adc.battery_raw(1)), (Some(1463), Some(1463)));
        assert_eq!((adc.oxygen_raw(0), adc.oxygen_raw(1), adc.oxygen_raw(2)), (Some(164), Some(164), Some(164)));
        // Full scale clips to 4095, as do the extreme runner values (250 mV cell, 4200 mV battery).
        rig.adc_mut().set_oxygen_millivolts(0, 250.0).unwrap();
        rig.adc_mut().set_battery_millivolts(0, 4200.0).unwrap();
        assert_eq!(rig.adc().oxygen_raw(0), Some(4095));
        assert_eq!(rig.adc().battery_raw(0), Some(4095));
        // Math.Round uses banker's rounding: 0.5 -> 0, 1.5 -> 2, 2.5 -> 2 (reference 4096 mV makes mV the code).
        rig.adc_mut().set_reference_millivolts(4096.0);
        for (millivolts, raw) in [(0.5, 0), (1.5, 2), (2.5, 2), (3.5, 4), (4.4, 4), (4.6, 5)] {
            rig.adc_mut().set_pin_millivolts(5, millivolts).unwrap();
            assert_eq!(rig.adc().input_raw(5), Some(raw), "{millivolts} mV");
        }
        // Range errors.
        assert_eq!(rig.adc_mut().set_pin_millivolts(5, f64::NAN), Err(AdcError::BadMillivolts));
        assert_eq!(rig.adc_mut().set_pin_millivolts(5, f64::INFINITY), Err(AdcError::BadMillivolts));
        assert_eq!(rig.adc_mut().set_pin_millivolts(5, -0.1), Err(AdcError::BadMillivolts));
        assert_eq!(rig.adc_mut().set_pin_millivolts(32, 1.0), Err(AdcError::ChannelOutOfRange(32)));
        assert_eq!(rig.adc_mut().set_battery_millivolts(2, 1.0), Err(AdcError::BankOutOfRange(2)));
        assert_eq!(rig.adc_mut().set_oxygen_millivolts(3, 1.0), Err(AdcError::CellOutOfRange(3)));
        rig.adc_mut().set_reference_millivolts(0.0);
        assert_eq!(rig.adc_mut().set_pin_millivolts(5, 1.0), Err(AdcError::BadReference));
        assert_eq!(rig.adc_mut().set_channel_raw(31, 0x1FFF), Ok(()));
        assert_eq!(rig.adc().input_raw(31), Some(0xFFF), "raw values are clipped to 12 bits");
        // The named raw properties are channel aliases: O2 cells 1..3, B1 = 4, B2 = 9, board ID = 6.
        rig.adc_mut().set_oxygen_raw(2, 0x1ABC).unwrap();
        rig.adc_mut().set_battery_raw(1, 77).unwrap();
        rig.adc_mut().set_battery_raw(0, 88).unwrap();
        rig.adc_mut().set_board_id_raw(0x2123);
        assert_eq!((rig.adc().input_raw(3), rig.adc().input_raw(9), rig.adc().input_raw(4), rig.adc().input_raw(6)), (Some(0xABC), Some(77), Some(88), Some(0x123)));
        assert_eq!(rig.adc_mut().set_oxygen_raw(3, 1), Err(AdcError::CellOutOfRange(3)));
        assert_eq!(rig.adc_mut().set_battery_raw(2, 1), Err(AdcError::BankOutOfRange(2)));
        assert_eq!(rig.adc().input_raw(32), None);
        rig.adc_mut().set_noise_amplitude_raw(5000);
        assert_eq!(rig.adc().noise_amplitude_raw(), 4095);
        assert_eq!(NgcMainAdc::new("x", AdcConfig { conversion_interval_us: 0, ..AdcConfig::default() }).err(), Some(AdcError::BadConversionInterval(0)));
        assert!(NgcMainAdc::new("x", AdcConfig { conversion_interval_us: 1_000_001, ..AdcConfig::default() }).is_err());
        assert!(NgcMainAdc::new("x", AdcConfig { conversion_interval_us: 1_000_000, ..AdcConfig::default() }).is_ok());
    }

    #[test]
    fn calibration_and_enable_complete_on_the_conversion_clock() {
        let mut rig = Rig::new();
        rig.h.write32(CR, 0x9000_0000);
        assert_eq!(rig.h.read32(CR), 0x9000_0000, "ADCAL stays set until the next conversion tick");
        assert_eq!(rig.adc().calibration_count(), 1);
        rig.h.advance_to(100 * US - 1);
        assert_eq!(rig.h.read32(CR), 0x9000_0000);
        rig.h.advance_to(100 * US);
        assert_eq!(rig.h.read32(CR), 0x1000_0000, "ADCAL self-clears at exactly 100 us");
        rig.h.write32(CR, 0x1000_0001);
        assert_eq!(rig.h.read32(ISR), 0, "ADRDY is raised by the next tick, not by the write");
        assert!(rig.adc().enabled());
        rig.h.advance_to(200 * US);
        assert_eq!(rig.h.read32(ISR), 1, "ADRDY");
        // Writing ADEN = 0 never disables; ADDIS disables and clears ADRDY (and itself).
        rig.h.write32(CR, 0x1000_0000);
        assert!(rig.adc().enabled());
        rig.h.write32(CR, 0x1000_0002);
        assert_eq!(rig.h.read32(CR), 0x1000_0000);
        assert_eq!(rig.h.read32(ISR), 0);
        assert!(!rig.adc().enabled());
        // ADEN while already enabled does not re-arm ADRDY; clearing ADRDY by writing 1 works.
        rig.h.write32(CR, 0x1000_0001);
        rig.h.write32(CR, 0x1000_0001);
        rig.h.advance_to(300 * US);
        assert_eq!(rig.h.read32(ISR), 1);
        rig.h.write32(ISR, 1);
        assert_eq!(rig.h.read32(ISR), 0);
    }

    #[test]
    fn six_rank_scan_with_dma_matches_the_renode_smoke_test() {
        // emulation/main-adc-smoke.resc: RawInputs 11 22 33 1463 1365 500; ranks channel 1,2,3,4,6,9.
        let mut rig = Rig::with_config(AdcConfig::default(), true);
        rig.adc_mut().set_raw_inputs(11, 22, 33, 1463, 1365, 500);
        rig.start_six_rank(3); // CFGR = 3 as in the script: DMAEN, no CONT (bit 13), so one sequence only
        // Scan starts at the 200 us tick boundary; the first rank converts at the next tick.
        let start = rig.h.now();
        rig.h.advance_to(start + 600 * US);
        let reads: Vec<u32> = rig.dma().reads.iter().map(|&(_, v)| v).collect();
        assert_eq!(reads, [11, 22, 33, 1463, 500, 1365]);
        assert_eq!(rig.adc().conversion_count(), 6);
        assert_eq!(rig.adc().sequence_count(), 1);
        assert_eq!(rig.adc().last_channel(), 9);
        assert!(!rig.adc().converting(), "single sequence: CONT is not set");
        assert_eq!(rig.h.read32(CR) & CR_ADSTART, 0);
        assert_eq!(rig.h.read32(ISR), 0x9, "ADRDY | EOS, EOC consumed by the DMA read");
        // Request pulses: one per rank, both edges at the same instant.
        let edges = &rig.dma().edges;
        assert_eq!(edges.len(), 12);
        assert!(edges.chunks(2).all(|p| p[0].1 && !p[1].1 && p[0].0 == p[1].0));
    }

    #[test]
    fn continuous_scans_snapshot_inputs_at_sequence_boundaries() {
        let mut rig = Rig::with_config(AdcConfig::default(), true);
        rig.adc_mut().set_raw_inputs(11, 22, 33, 1463, 1365, 500);
        rig.start_six_rank(0x2001); // DMAEN | CONT
        let start = rig.h.now();
        // Half way through the first scan the inputs change: this scan keeps the old snapshot.
        rig.h.advance_to(start + 300 * US);
        rig.adc_mut().set_raw_inputs(101, 102, 103, 104, 105, 106);
        rig.h.advance_to(start + 1200 * US);
        let reads: Vec<u32> = rig.dma().reads.iter().map(|&(_, v)| v).collect();
        assert_eq!(reads[..6], [11, 22, 33, 1463, 500, 1365], "first scan: old inputs, no mixed samples");
        assert_eq!(reads[6..12], [101, 102, 103, 104, 106, 105], "second scan: new inputs");
        assert_eq!(rig.adc().sequence_count(), 2);
        assert!(rig.adc().converting());
    }

    #[test]
    fn acquisition_enable_and_initial_delay() {
        let mut rig = Rig::with_config(AdcConfig::default(), true);
        rig.adc_mut().set_acquisition_enabled(false);
        rig.adc_mut().set_initial_acquisition_delay_us(1000);
        rig.start_six_rank(0x2001);
        assert_eq!(rig.adc().settling_ticks(), 10, "ceil(1000 / 100) ticks of settling, applied to the first start");
        rig.h.advance_by(2000 * US);
        assert_eq!(rig.adc().conversion_count(), 0, "held: no EOC, no DMA");
        assert!(rig.dma().edges.is_empty());
        assert_eq!(rig.adc().settling_ticks(), 10);
        rig.adc_mut().set_acquisition_enabled(true);
        rig.h.advance_by(1000 * US);
        assert_eq!(rig.adc().conversion_count(), 0, "the initial delay still has to elapse");
        assert_eq!(rig.adc().settling_ticks(), 0);
        rig.h.advance_by(600 * US);
        assert_eq!(rig.adc().conversion_count(), 6);
        // The delay applies once: stopping and restarting begins at once.
        rig.h.write32(CR, 0x1000_0010);
        assert!(!rig.adc().converting());
        assert_eq!(rig.h.read32(CR) & 0x14, 0, "ADSTP clears ADSTART and itself");
        rig.h.write32(CR, 0x1000_0004);
        assert_eq!(rig.adc().settling_ticks(), 0);
        rig.h.advance_by(100 * US);
        assert_eq!(rig.adc().conversion_count(), 7);
    }

    #[test]
    fn overrun_and_polling_flow_without_dma() {
        let mut rig = Rig::new();
        rig.h.connect_irq(rig.adc, IRQ, 18);
        rig.adc_mut().set_raw_inputs(7, 8, 9, 10, 11, 12);
        rig.h.write32(CR, 0x1000_0001);
        rig.h.advance_by(100 * US);
        rig.h.write32(SQR1, 0x0000_0041); // two ranks: channel 1, then channel 0
        rig.h.write32(CR, 0x1000_0005);
        rig.h.advance_by(100 * US);
        assert_eq!(rig.h.read32(ISR), 0x1 | 0x4, "ADRDY | EOC after the first rank");
        assert_eq!(rig.h.read32(DR), 7);
        assert_eq!(rig.h.read32(ISR), 0x1, "reading DR consumed EOC");
        rig.h.advance_by(100 * US);
        assert_eq!(rig.h.read32(ISR), 0x1 | 0x4 | 0x8, "second rank: EOC and EOS");
        assert_eq!(rig.h.read32(DR), 0, "channel 0 has no input");
        assert!(!rig.adc().converting());
        assert_eq!(rig.h.read32(CR), 0x1000_0001, "ADSTART cleared at the end of a single sequence");
        // Overrun: EOC not consumed when the next conversion arrives.
        rig.h.write32(ISR, 0xFFFF_FFFF);
        rig.h.write32(CR, 0x1000_0005);
        rig.h.advance_by(100 * US);
        rig.h.advance_by(100 * US);
        assert_eq!(rig.h.read32(ISR) & 0x1C, 0x1C, "EOC, EOS and OVR (the second conversion found EOC set)");
    }

    #[test]
    fn irq_follows_isr_and_ier() {
        let mut rig = Rig::new();
        rig.h.write32(IER, 0x4); // EOCIE
        rig.h.write32(CR, 0x1000_0001);
        rig.h.advance_by(100 * US);
        rig.h.write32(SQR1, 0x40);
        rig.h.write32(CR, 0x1000_0005);
        assert!(!rig.h.irq_level(18));
        rig.h.advance_by(100 * US);
        assert!(rig.h.irq_level(18), "EOC with EOCIE");
        let t = rig.h.irq_changes().last().unwrap().time;
        assert_eq!(t, 200 * US, "the sequence started at 100 us; its single rank converts on the 200 us tick");
        rig.h.read32(DR);
        assert!(!rig.h.irq_level(18), "the DR read recomputes the line");
        // IER bits above bit 10 are not part of the IRQ mask.
        rig.h.write32(IER, 0xFFFF_F800);
        assert!(!rig.h.irq_level(18));
        rig.h.write32(IER, 0xFFFF_F804);
        assert!(!rig.h.irq_level(18));
        rig.h.write32(ISR, 4);
        rig.h.write32(CR, 0x1000_0005);
        rig.h.advance_by(100 * US);
        assert!(rig.h.irq_level(18));
        rig.h.write32(ISR, 4);
        assert!(!rig.h.irq_level(18), "ISR write-one-to-clear recomputes the line");
    }

    #[test]
    fn dma_consuming_eoc_does_not_pulse_the_irq() {
        // Renode order: request pulse -> DMA reads DR (clears EOC, IRQ recomputed) -> end of tick. With EOCIE set the
        // line must never rise for an EOC that the DMA consumes; a naive deferred delivery would pulse it.
        let mut rig = Rig::with_config(AdcConfig::default(), true);
        rig.h.connect_irq(rig.adc, IRQ, 18);
        rig.h.write32(IER, 0x4);
        rig.start_six_rank(0x2001);
        rig.h.clear_irq_changes();
        rig.h.advance_by(1200 * US);
        assert!(rig.adc().conversion_count() >= 11);
        assert!(rig.h.irq_changes().is_empty(), "no IRQ edge at all: {:?}", rig.h.irq_changes());
        // EOS is not consumed by DR reads: with EOSIE the line rises after the sixth rank and stays.
        rig.h.write32(IER, 0x8);
        rig.h.advance_by(600 * US);
        assert!(rig.h.irq_level(18));
        let last = rig.dma().reads.len();
        assert!(last >= 12);
    }

    #[test]
    fn noise_is_the_documented_deterministic_hash() {
        // Independent restatement of the C# Snapshot() formula.
        fn expected(seed: u32, sequence: u32, channel: u32, amplitude: u32, input: u32) -> u32 {
            let mut hash = seed ^ sequence.wrapping_mul(1_664_525) ^ channel.wrapping_mul(1_013_904_223);
            hash ^= hash << 13;
            hash ^= hash >> 17;
            hash ^= hash << 5;
            let delta = (hash % (amplitude * 2 + 1)) as i64 - i64::from(amplitude);
            (i64::from(input) + delta).clamp(0, 4095) as u32
        }
        let mut rig = Rig::with_config(AdcConfig::default(), true);
        rig.adc_mut().set_noise_amplitude_raw(2);
        rig.adc_mut().set_noise_seed(42);
        rig.start_six_rank(0x2001);
        rig.h.advance_by(600 * US * 3 + 1);
        let reads: Vec<u32> = rig.dma().reads.iter().map(|&(_, v)| v).collect();
        let channels = [1u32, 2, 3, 4, 6, 9];
        let inputs = [164u32, 164, 164, 1463, 500, 1463];
        for sequence in 0..3u32 {
            for rank in 0..6usize {
                assert_eq!(
                    reads[sequence as usize * 6 + rank],
                    expected(42, sequence, channels[rank], 2, inputs[rank]),
                    "sequence {sequence} rank {rank}"
                );
            }
        }
        assert!(reads.iter().all(|&v| v <= 4095));
        // Noise never leaves the 12-bit range.
        rig.adc_mut().set_raw_inputs(0, 4095, 0, 4095, 0, 4095);
        rig.adc_mut().set_noise_amplitude_raw(4095);
        rig.h.advance_by(600 * US * 4);
        assert!(rig.dma().reads.iter().all(|&(_, v)| v <= 4095));
    }

    #[test]
    fn conversion_clock_period_and_summary() {
        let config = AdcConfig { conversion_interval_us: 70, ..AdcConfig::default() };
        let mut rig = Rig::with_config(config, false);
        // 1_000_000 / 70 = 14285 Hz (integer division): period ceil(1e9 / 14285) = 70_004 ns.
        rig.h.write32(CR, 0x9000_0000);
        rig.h.advance_to(70_003);
        assert_eq!(rig.h.read32(CR), 0x9000_0000);
        rig.h.advance_to(70_004);
        assert_eq!(rig.h.read32(CR), 0x1000_0000);
        let mut rig = Rig::new();
        assert_eq!(
            rig.adc().describe(),
            "Main ADC fixture: conversions=0; sequences=0; calibrations=0; lastChannel=0; rankIntervalUs=100; CR=0x20000000; ISR=0x0; CFGR=0x0; SQR1=0x0; SQR2=0x0; acquisitionEnabled=True; initialDelayUs=0; settlingTicks=0; noiseRaw=0"
        );
        rig.start_six_rank(0x3003);
        rig.h.advance_by(600 * US * 2);
        // No DMA reader is connected, so EOC is never consumed and the second conversion sets OVR.
        assert_eq!(
            rig.h.core().summaries().iter().find(|(n, _)| n == "adc").unwrap().1,
            "Main ADC fixture: conversions=12; sequences=2; calibrations=1; lastChannel=9; rankIntervalUs=100; CR=0x10000005; ISR=0x1D; CFGR=0x3003; SQR1=0x40C2045; SQR2=0x246; acquisitionEnabled=True; initialDelayUs=0; settlingTicks=0; noiseRaw=0"
        );
    }

    /// Cost of one conversion tick with the real DMA model behind it (printed, not asserted; `--nocapture`).
    #[test]
    #[ignore = "timing measurement"]
    fn tick_cost_with_dma() {
        use std::time::Instant;
        use stm32::dma::Dma;
        let mut h = Harness::new();
        let dma = h.add_mapped(0x4002_0000, 0x400, Dma::new("dma1"));
        let adc = h.add_mapped(BASE, SIZE, NgcMainAdc::with_defaults("adc"));
        h.connect_irq(adc, IRQ, 18);
        h.connect_input(adc, DMA_REQUEST, dma, 0);
        // DMA1 channel 1 as the firmware sets it up: circular, 32 bit both sides, memory increment, six words.
        h.write32(0x4002_0008, 0x2AA2);
        h.write32(0x4002_000C, 6);
        h.write32(0x4002_0010, BASE + 0x40);
        h.write32(0x4002_0014, 0x2000_0000);
        h.write32(0x4002_0008, 0x2AA3);
        let mut rig = Rig { h, adc, dma: None };
        rig.start_six_rank(0x2001);
        let start_time = rig.h.now();
        let wall = Instant::now();
        rig.h.advance_to(start_time + 2_000_000_000);
        let elapsed = wall.elapsed();
        let ticks = 20_000.0;
        println!("ADC tick with DMA: {:.0} ns per 100 us tick ({:.2}% of real time)", elapsed.as_secs_f64() * 1e9 / ticks, elapsed.as_secs_f64() / 2.0 * 100.0);
        assert!(rig.adc().conversion_count() >= 19_000);
    }

    #[test]
    fn reset_keeps_inputs_and_settings() {
        let mut rig = Rig::new();
        rig.adc_mut().set_noise_amplitude_raw(3);
        rig.adc_mut().set_battery_millivolts(0, 1000.0).unwrap();
        rig.h.connect_irq(rig.adc, IRQ, 18);
        rig.h.write32(IER, 4);
        rig.start_six_rank(0x2001);
        rig.h.advance_by(300 * US);
        assert!(rig.adc().conversion_count() > 0);
        rig.h.core_mut().reset_all();
        assert_eq!(rig.h.read32(CR), 0x2000_0000);
        assert_eq!(rig.h.read32(IER), 0);
        assert_eq!(rig.adc().conversion_count(), 0);
        assert!(!rig.adc().converting());
        assert_eq!(rig.adc().noise_amplitude_raw(), 3);
        assert_eq!(rig.adc().battery_raw(0), Some(((1000.0f64 / 1.68) * 4096.0 / 2500.0).round_ties_even() as u32));
        assert!(!rig.h.irq_level(18));
        // The initial acquisition delay applies again to the first sequence after a reset.
        rig.adc_mut().set_initial_acquisition_delay_us(250);
        rig.start_six_rank(0x2001);
        assert_eq!(rig.adc().settling_ticks(), 3);
    }
}
