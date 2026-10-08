// Ported from emulation/models/NGCMS5837.cs.

//! `NGCMS5837`: command/PROM/ADC model of the two MS5837-30BA pressure/temperature sensors of main 5.8
//! (`pressure1 @ i2c1 0x76`, `pressure2 @ i2c2 0x76`). It is an [`I2cTarget`] of [`stm32::i2c::Stm32F7I2c`].
//!
//! The PROM coefficients `[34982, 36352, 20328, 22354, 26646, 26146]` are the synthetic datasheet example
//! (not a device capture); word 0 carries the recomputed CRC4 in its top nibble. The *physical* inputs
//! [`PressureMbar`](NgcMs5837::pressure_mbar) (default 1013.25) and
//! [`TemperatureCelsius`](NgcMs5837::temperature_celsius) (default 20) are inverted into raw `D1`/`D2`
//! counts through TE's second-order compensation when a conversion is **requested**; the firmware performs the
//! forward compensation itself.
//!
//! Protocol, exactly as the C# class implements it:
//!
//! * a write of `0x1E` resets the device (state, counters and any pending conversion; the physical inputs stay);
//! * a write of `0x40..=0x4A` (D1, pressure) or `0x50..=0x5A` (D2, temperature) with an even command byte
//!   starts a conversion whose oversampling index is `(command & 0xF) / 2`; completion is a Renode
//!   `ScheduleAction` ([`I2cCtx::schedule_action`]): it is measured from the *exact* instruction time of the
//!   I2C register access that carried the command and its callback sees the scheduling time. The default
//!   durations are the datasheet's typical values (540, 1060, 2080, 4130, 8220 and 16440 us for OSR 256..8192);
//!   [`use_maximum_conversion_time`](NgcMs5837::set_use_maximum_conversion_time) selects 600..18080 us;
//! * a write of `0x00` selects the ADC read: the 24-bit result is latched then if the conversion is complete
//!   and the previous one was valid (reading *while a conversion is pending* invalidates that conversion; a
//!   not-ready read returns zero), and `Read(3)` returns it big-endian; the result can be read once, a repeated
//!   read returns zero;
//! * a conversion started while another is pending makes the *new* one invalid (its result is zero), as does
//!   an early ADC read for the pending one (`conversionInvalid`): the physical device's corrupt final result is
//!   not reproduced electrically;
//! * `0xA0..=0xAE` (even) select PROM words 0..7; `Read(2)` returns the word big-endian (further bytes read 0).
//!
//! Electrical noise, supply effects, reset settling, clock stretching and sensor errors are not modelled.
//!
//! # Wiring (`main.repl`)
//!
//! ```text
//! pressure1: Sensors.NGCMS5837 @ i2c1 0x76      i2c1.attach(0x76, Box::new(NgcMs5837::with_defaults("pressure1")))
//! pressure2: Sensors.NGCMS5837 @ i2c2 0x76      i2c2.attach(0x76, Box::new(NgcMs5837::with_defaults("pressure2")))
//! ```
//!
//! The runner's inputs (`PressureMbar`, `TemperatureCelsius`, `UseMaximumConversionTime`) are set with
//! `Stm32F7I2c::target_mut::<NgcMs5837>(0x76)`. As for the ADC, the Renode monitor handed `double` arguments to the
//! C# setters rounded to single precision (`f64::from(value as f32)`); the Rust setters take the exact `f64`.

use emu_core::Time;
use std::fmt;
use stm32::i2c::{I2cCtx, I2cTarget};
use stm32::impl_i2c_target_any;

/// Typical conversion time in microseconds per oversampling index (OSR 256..8192).
pub const TYPICAL_US: [u64; 6] = [540, 1060, 2080, 4130, 8220, 16440];
/// Maximum conversion time in microseconds per oversampling index.
pub const MAXIMUM_US: [u64; 6] = [600, 1170, 2280, 4540, 9040, 18080];
/// The synthetic PROM before the CRC nibble is inserted into word 0.
const PROM_TEMPLATE: [u16; 8] = [0, 34982, 36352, 20328, 22354, 26646, 26146, 0];

/// Why a setter or accessor refused its argument (the C# exceptions).
#[derive(Clone, Debug, PartialEq)]
pub enum Ms5837Error {
    /// `ArgumentOutOfRangeException("PressureMbar")`: NaN, negative or above 30000.
    PressureOutOfRange(f64),
    /// `ArgumentOutOfRangeException("TemperatureCelsius")`: NaN or outside -20..=85.
    TemperatureOutOfRange(f64),
    /// `ArgumentOutOfRangeException("index")`.
    PromIndexOutOfRange(u32),
}

impl fmt::Display for Ms5837Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ms5837Error::PressureOutOfRange(v) => write!(f, "PressureMbar {v} is outside 0..=30000"),
            Ms5837Error::TemperatureOutOfRange(v) => write!(f, "TemperatureCelsius {v} is outside -20..=85"),
            Ms5837Error::PromIndexOutOfRange(index) => write!(f, "PROM index {index} is outside 0..=7"),
        }
    }
}

impl std::error::Error for Ms5837Error {}

/// C# `bool.ToString()`.
fn cs_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// MS5837 CRC-4 over the PROM words (word 0 without its CRC nibble, word 7 as zero).
fn crc4(words: &[u16; 8]) -> u32 {
    let mut remainder = 0u32;
    for i in 0..16usize {
        let word = if i / 2 == 0 {
            words[0] & 0xFFF
        } else if i / 2 == 7 {
            0
        } else {
            words[i / 2]
        };
        remainder ^= if i & 1 == 0 { u32::from(word >> 8) } else { u32::from(word & 0xFF) };
        for _ in 0..8 {
            remainder = (if remainder & 0x8000 != 0 { (remainder << 1) ^ 0x3000 } else { remainder << 1 }) & 0xFFFF;
        }
    }
    (remainder >> 12) & 0xF
}

/// `Sensors.NGCMS5837`.
pub struct NgcMs5837 {
    name: String,
    pressure: f64,
    temperature: f64,
    use_maximum_conversion_time: bool,
    prom: [u16; 8],
    command: u8,
    adc: u32,
    read_value: u32,
    pending_d1: u32,
    pending_d2: u32,
    read_index: i32,
    generation: u64,
    last_duration_us: u64,
    pending_temperature: bool,
    adc_ready: bool,
    conversion_invalid: bool,
    pending: bool,
    raw_d1: u32,
    raw_d2: u32,
    pressure_conversions: u64,
    temperature_conversions: u64,
    prom_reads: u64,
}

impl NgcMs5837 {
    /// `new NGCMS5837(machine)`: 1013.25 mbar, 20 C (the `pressure1`/`pressure2` lines of `main.repl`).
    pub fn with_defaults(name: impl Into<String>) -> Self {
        Self::new(name, 1013.25, 20.0).expect("the default inputs are valid")
    }

    /// `new NGCMS5837(machine, pressureMbar, temperatureCelsius)`.
    pub fn new(name: impl Into<String>, pressure_mbar: f64, temperature_celsius: f64) -> Result<Self, Ms5837Error> {
        let mut prom = PROM_TEMPLATE;
        prom[0] = (crc4(&prom) << 12) as u16;
        let mut sensor = Self {
            name: name.into(),
            pressure: 0.0,
            temperature: 0.0,
            use_maximum_conversion_time: false,
            prom,
            command: 0,
            adc: 0,
            read_value: 0,
            pending_d1: 0,
            pending_d2: 0,
            read_index: 0,
            generation: 0,
            last_duration_us: 0,
            pending_temperature: false,
            adc_ready: false,
            conversion_invalid: false,
            pending: false,
            raw_d1: 0,
            raw_d2: 0,
            pressure_conversions: 0,
            temperature_conversions: 0,
            prom_reads: 0,
        };
        sensor.set_pressure_mbar(pressure_mbar)?;
        sensor.set_temperature_celsius(temperature_celsius)?;
        sensor.reset_state();
        Ok(sensor)
    }

    /// `Reset()`: protocol state, counters and (by bumping the generation) any scheduled completion; the
    /// physical inputs, the timing choice, `readValue` and the last pending raw values stay.
    fn reset_state(&mut self) {
        // Renode parity: the generation counter is bumped by every Reset (also the construction-time one), which is
        // what invalidates the ScheduleAction callback of a conversion that was in flight.
        self.command = 0;
        self.pending = false;
        self.generation += 1;
        self.adc = 0;
        self.read_index = 0;
        self.adc_ready = false;
        self.conversion_invalid = false;
        self.raw_d1 = 0;
        self.raw_d2 = 0;
        self.pressure_conversions = 0;
        self.temperature_conversions = 0;
        self.prom_reads = 0;
    }

    // ---- C# properties ----

    /// `UseMaximumConversionTime` (default false): datasheet maximum instead of typical durations.
    pub fn use_maximum_conversion_time(&self) -> bool {
        self.use_maximum_conversion_time
    }

    pub fn set_use_maximum_conversion_time(&mut self, maximum: bool) {
        self.use_maximum_conversion_time = maximum;
    }

    /// `PressureMbar`.
    pub fn pressure_mbar(&self) -> f64 {
        self.pressure
    }

    /// `PressureMbar = value`: NaN, negative or above 30000 is refused.
    pub fn set_pressure_mbar(&mut self, value: f64) -> Result<(), Ms5837Error> {
        if value.is_nan() || value < 0.0 || value > 30000.0 {
            return Err(Ms5837Error::PressureOutOfRange(value));
        }
        self.pressure = value;
        Ok(())
    }

    /// `TemperatureCelsius`.
    pub fn temperature_celsius(&self) -> f64 {
        self.temperature
    }

    /// `TemperatureCelsius = value`: NaN or outside -20..=85 is refused.
    pub fn set_temperature_celsius(&mut self, value: f64) -> Result<(), Ms5837Error> {
        if value.is_nan() || value < -20.0 || value > 85.0 {
            return Err(Ms5837Error::TemperatureOutOfRange(value));
        }
        self.temperature = value;
        Ok(())
    }

    /// `RawD1`: the last completed pressure conversion (0 after reset or an invalid conversion).
    pub fn raw_d1(&self) -> u32 {
        self.raw_d1
    }

    /// `RawD2`: the last completed temperature conversion.
    pub fn raw_d2(&self) -> u32 {
        self.raw_d2
    }

    /// `PressureConversions`.
    pub fn pressure_conversions(&self) -> u64 {
        self.pressure_conversions
    }

    /// `TemperatureConversions`.
    pub fn temperature_conversions(&self) -> u64 {
        self.temperature_conversions
    }

    /// `PromReads`: PROM word reads (one per `Read` of a PROM command).
    pub fn prom_reads(&self) -> u64 {
        self.prom_reads
    }

    /// `GetPromWord(index)`.
    pub fn prom_word(&self, index: u32) -> Result<u16, Ms5837Error> {
        self.prom.get(index as usize).copied().ok_or(Ms5837Error::PromIndexOutOfRange(index))
    }

    /// A conversion is in flight.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// The C# `Reset()` called directly (monitor `pressure1 Reset`). On the bus the controller's reset
    /// reaches [`I2cTarget::reset`] instead, which does the same.
    pub fn reset_device(&mut self) {
        self.reset_state();
    }

    /// Duration of the last started conversion in microseconds (`lastDurationUs`).
    pub fn last_duration_us(&self) -> u64 {
        self.last_duration_us
    }

    /// C# `Summary`.
    pub fn describe(&self) -> String {
        format!(
            "MS5837 synthetic PROM: pressure={:.2}mbar; temperature={:.2}C; D1={}; D2={}; conversions={}/{}; PROMreads={}; pending={}; durationUs={}; maximumTiming={}",
            self.pressure,
            self.temperature,
            self.raw_d1,
            self.raw_d2,
            self.pressure_conversions,
            self.temperature_conversions,
            self.prom_reads,
            cs_bool(self.pending),
            self.last_duration_us,
            cs_bool(self.use_maximum_conversion_time)
        )
    }

    // ---- model ----

    fn complete_conversion(&mut self, generation: u64) {
        if !self.pending || generation != self.generation {
            return;
        }
        self.pending = false;
        if self.pending_temperature {
            self.raw_d2 = if self.conversion_invalid { 0 } else { self.pending_d2 };
            self.adc = self.raw_d2;
            self.temperature_conversions += 1;
        } else {
            self.raw_d1 = if self.conversion_invalid { 0 } else { self.pending_d1 };
            self.adc = self.raw_d1;
            self.pressure_conversions += 1;
        }
        self.adc_ready = true;
    }

    /// Inverts the physical inputs into raw `(D1, D2)`: bisection for `D2` on the compensated temperature, then
    /// `D1` from the compensated offset/sensitivity.
    fn generate_raw(&self) -> (u32, u32) {
        let (mut low, mut high) = (0u32, 0x00FF_FFFFu32);
        for _ in 0..24 {
            let middle = low + (high - low) / 2;
            let (temp, _, _) = self.compensate(middle);
            if temp < self.temperature * 100.0 {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let d2 = low;
        let (_, off, sens) = self.compensate(d2);
        let raw = ((self.pressure * 10.0 * 8192.0 + off) * 2_097_152.0 / sens).round_ties_even();
        // `(uint)Math.Max(0, Math.Min(0xffffff, raw))`: a NaN or out-of-range raw saturates like the cast.
        let d1 = raw.min(f64::from(0x00FF_FFFFu32)).max(0.0) as u32;
        (d1, d2)
    }

    /// TE's second-order compensation as the C# `Compensate` writes it: `(temperature in 0.01 C, offset, sensitivity)`.
    fn compensate(&self, d2: u32) -> (f64, f64, f64) {
        let prom = |i: usize| f64::from(self.prom[i]);
        let dt = f64::from(d2) - prom(5) * 256.0;
        let mut temp = 2000.0 + dt * prom(6) / 8_388_608.0;
        let mut off = prom(2) * 65536.0 + prom(4) * dt / 128.0;
        let mut sens = prom(1) * 32768.0 + prom(3) * dt / 256.0;
        let (ti, oi, si);
        if temp < 2000.0 {
            ti = 3.0 * dt * dt / 8_589_934_592.0;
            let mut o = 3.0 * (temp - 2000.0) * (temp - 2000.0) / 2.0;
            let mut s = 5.0 * (temp - 2000.0) * (temp - 2000.0) / 8.0;
            if temp < -1500.0 {
                o += 7.0 * (temp + 1500.0) * (temp + 1500.0);
                s += 4.0 * (temp + 1500.0) * (temp + 1500.0);
            }
            oi = o;
            si = s;
        } else {
            ti = 2.0 * dt * dt / 137_438_953_472.0;
            oi = (temp - 2000.0) * (temp - 2000.0) / 16.0;
            si = 0.0;
        }
        temp -= ti;
        off -= oi;
        sens -= si;
        (temp, off, sens)
    }
}

impl I2cTarget for NgcMs5837 {
    fn name(&self) -> &str {
        &self.name
    }

    fn write(&mut self, data: &[u8], ctx: &mut I2cCtx<'_, '_>) {
        let Some(&command) = data.first() else { return };
        self.command = command;
        self.read_index = 0;
        if command == 0x1E {
            self.reset_state();
            return;
        }
        if command == 0 {
            if self.pending {
                self.conversion_invalid = true;
            }
            self.read_value = if self.adc_ready && !self.pending { self.adc } else { 0 };
        }
        if command & 1 == 0 && ((0x40..=0x4A).contains(&command) || (0x50..=0x5A).contains(&command)) {
            let index = usize::from((command & 0xF) / 2);
            self.conversion_invalid = self.pending;
            self.adc_ready = false;
            self.pending = true;
            self.pending_temperature = command & 0x10 != 0;
            let (d1, d2) = self.generate_raw();
            self.pending_d1 = d1;
            self.pending_d2 = d2;
            self.generation += 1;
            let generation = self.generation;
            self.last_duration_us = if self.use_maximum_conversion_time { MAXIMUM_US[index] } else { TYPICAL_US[index] };
            // `machine.ScheduleAction(TimeInterval.FromMicroseconds(duration), _ => CompleteConversion(generation))`.
            ctx.schedule_action(self.last_duration_us * 1000, generation);
        }
    }

    fn read(&mut self, count: usize, _ctx: &mut I2cCtx<'_, '_>) -> Vec<u8> {
        let mut result = vec![0u8; count];
        let command = self.command;
        if (0xA0..=0xAE).contains(&command) && command & 1 == 0 {
            let word = self.prom[usize::from((command - 0xA0) / 2)];
            for byte in result.iter_mut() {
                *byte = match self.read_index {
                    0 => (word >> 8) as u8,
                    1 => word as u8,
                    _ => 0,
                };
                self.read_index = self.read_index.wrapping_add(1);
            }
            self.prom_reads += 1;
        } else if command == 0 {
            for byte in result.iter_mut() {
                *byte = if (0..3).contains(&self.read_index) { (self.read_value >> (16 - 8 * self.read_index)) as u8 } else { 0 };
                self.read_index = self.read_index.wrapping_add(1);
            }
            if self.read_index >= 3 {
                self.adc_ready = false;
            }
        }
        result
    }

    fn finish_transmission(&mut self, _ctx: &mut I2cCtx<'_, '_>) {}

    fn reset(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
        self.reset_state();
    }

    /// The conversion-complete action: `token` is the generation that scheduled it, `scheduled` the time of
    /// the I2C access that carried the command (what Renode passes to the callback, unused by the C# lambda).
    fn on_event(&mut self, token: u64, _scheduled: Time, _ctx: &mut I2cCtx<'_, '_>) {
        self.complete_conversion(token);
    }

    fn summary(&self) -> String {
        self.describe()
    }

    impl_i2c_target_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{PeriphId, TICKS_PER_MICROSECOND as US};
    use stm32::i2c::testing::{master_receive, master_transmit};
    use stm32::i2c::{Stm32F7I2c, I2C_SIZE};

    const BASE: u32 = 0x4000_5400;
    const DEV: u32 = 0x76 << 1;

    fn setup() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let mut i2c = Stm32F7I2c::new("i2c1");
        i2c.attach(0x76, Box::new(NgcMs5837::with_defaults("pressure1"))).unwrap();
        let id = h.add_mapped(BASE, I2C_SIZE, i2c);
        h.write32(BASE, 1); // CR1.PE
        (h, id)
    }

    fn sensor(h: &Harness, id: PeriphId) -> &NgcMs5837 {
        h.get::<Stm32F7I2c>(id).target::<NgcMs5837>(0x76).unwrap()
    }

    fn sensor_mut(h: &mut Harness, id: PeriphId) -> &mut NgcMs5837 {
        h.get_mut::<Stm32F7I2c>(id).target_mut::<NgcMs5837>(0x76).unwrap()
    }

    fn command(h: &mut Harness, byte: u8) {
        master_transmit(h, BASE, DEV, &[byte]);
    }

    fn prom_word(h: &mut Harness, index: u8) -> u16 {
        command(h, 0xA0 + 2 * index);
        let bytes = master_receive(h, BASE, DEV, 2);
        u16::from_be_bytes([bytes[0], bytes[1]])
    }

    fn adc_read(h: &mut Harness) -> u32 {
        command(h, 0x00);
        let bytes = master_receive(h, BASE, DEV, 3);
        u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2])
    }

    /// TE's forward compensation (the firmware's job), in doubles: `(pressure in mbar, temperature in C)`.
    fn compensated(prom: &[u16; 8], d1: u32, d2: u32) -> (f64, f64) {
        let p = |i: usize| f64::from(prom[i]);
        let dt = f64::from(d2) - p(5) * 256.0;
        let mut t = 2000.0 + dt * p(6) / 8_388_608.0;
        let mut off = p(2) * 65536.0 + p(4) * dt / 128.0;
        let mut sens = p(1) * 32768.0 + p(3) * dt / 256.0;
        let (ti, oi, si);
        if t < 2000.0 {
            ti = 3.0 * dt * dt / 8_589_934_592.0;
            let mut o = 3.0 * (t - 2000.0) * (t - 2000.0) / 2.0;
            let mut s = 5.0 * (t - 2000.0) * (t - 2000.0) / 8.0;
            if t < -1500.0 {
                o += 7.0 * (t + 1500.0) * (t + 1500.0);
                s += 4.0 * (t + 1500.0) * (t + 1500.0);
            }
            oi = o;
            si = s;
        } else {
            ti = 2.0 * dt * dt / 137_438_953_472.0;
            oi = (t - 2000.0) * (t - 2000.0) / 16.0;
            si = 0.0;
        }
        t -= ti;
        off -= oi;
        sens -= si;
        (((f64::from(d1) * sens / 2_097_152.0 - off) / 8192.0) / 10.0, t / 100.0)
    }

    #[test]
    fn prom_words_crc_and_reads() {
        let (mut h, id) = setup();
        let words: Vec<u16> = (0..7).map(|i| prom_word(&mut h, i)).collect();
        assert_eq!(&words[1..], [34982, 36352, 20328, 22354, 26646, 26146]);
        assert_eq!(words[0] & 0x0FFF, 0, "the CRC nibble sits in the top four bits of word 0");
        assert_eq!(words[0] >> 12, 2, "the recomputed CRC4 of the synthetic PROM (docs: CRC 2)");
        assert_eq!(prom_word(&mut h, 7), 0);
        assert_eq!(sensor(&h, id).prom_reads(), 8);
        assert_eq!(sensor(&h, id).prom_word(3), Ok(20328));
        assert_eq!(sensor(&h, id).prom_word(8), Err(Ms5837Error::PromIndexOutOfRange(8)));
        // CRC4 as the datasheet's algorithm (word 0 with its CRC nibble cleared, word 7 zero).
        let n_prom = [words[0] & 0x0FFF, words[1], words[2], words[3], words[4], words[5], words[6], 0u16];
        let mut n_rem = 0u16;
        for cnt in 0..16 {
            n_rem ^= if cnt % 2 == 1 { n_prom[cnt >> 1] & 0x00FF } else { n_prom[cnt >> 1] >> 8 };
            for _ in 0..8 {
                n_rem = if n_rem & 0x8000 != 0 { (n_rem << 1) ^ 0x3000 } else { n_rem << 1 };
            }
        }
        assert_eq!((n_rem >> 12) & 0xF, words[0] >> 12);
        // Odd or out-of-range commands are not PROM reads: the read returns zeros without counting.
        let reads = sensor(&h, id).prom_reads();
        command(&mut h, 0xA1);
        assert_eq!(master_receive(&mut h, BASE, DEV, 2), [0, 0]);
        command(&mut h, 0xB0);
        assert_eq!(master_receive(&mut h, BASE, DEV, 2), [0, 0]);
        assert_eq!(sensor(&h, id).prom_reads(), reads);
        // Reading past the word returns zeros.
        command(&mut h, 0xA2);
        let bytes = master_receive(&mut h, BASE, DEV, 5);
        assert_eq!(bytes, [(34982 >> 8) as u8, (34982 & 0xFF) as u8, 0, 0, 0]);
    }

    #[test]
    fn conversion_completes_at_the_exact_duration_and_inverts_to_the_inputs() {
        let (mut h, id) = setup();
        let t0 = h.now();
        command(&mut h, 0x4A); // D1, OSR 8192
        assert!(h.take_stop_request(), "schedule_action asks the CPU to return");
        assert!(sensor(&h, id).pending());
        assert_eq!(sensor(&h, id).last_duration_us(), 16440);
        assert_eq!(h.next_event_time(), Some(t0 + 16_440_000));
        h.advance_to(t0 + 16_440_000 - 1);
        assert!(sensor(&h, id).pending());
        h.advance_to(t0 + 16_440_000);
        assert!(!sensor(&h, id).pending());
        let d1 = adc_read(&mut h);
        assert_eq!(d1, sensor(&h, id).raw_d1());
        assert!(d1 > 0);
        assert_eq!(sensor(&h, id).pressure_conversions(), 1);
        assert_eq!(adc_read(&mut h), 0, "the result can be read once");
        command(&mut h, 0x5A); // D2
        let t1 = h.now();
        h.advance_to(t1 + 16_440_000);
        let d2 = adc_read(&mut h);
        assert!(d2 > 0);
        assert_eq!(sensor(&h, id).temperature_conversions(), 1);
        let prom = [sensor(&h, id).prom_word(0).unwrap(), 34982, 36352, 20328, 22354, 26646, 26146, 0];
        let (pressure, temperature) = compensated(&prom, d1, d2);
        assert!((pressure - 1013.25).abs() < 0.1, "{pressure}");
        assert!((temperature - 20.0).abs() < 0.01, "{temperature}");
        // The docs' evidence: D1/D2 = [4510425, 6821376] for the defaults.
        assert_eq!((d1, d2), (4_510_425, 6_821_376));
        // A changed physical state (low temperature branch of the compensation).
        sensor_mut(&mut h, id).set_pressure_mbar(3500.0).unwrap();
        sensor_mut(&mut h, id).set_temperature_celsius(-5.0).unwrap();
        command(&mut h, 0x4A);
        let t2 = h.now();
        h.advance_to(t2 + 16_440_000);
        let d1 = adc_read(&mut h);
        command(&mut h, 0x5A);
        let t3 = h.now();
        h.advance_to(t3 + 16_440_000);
        let d2 = adc_read(&mut h);
        let (pressure, temperature) = compensated(&prom, d1, d2);
        assert!((pressure - 3500.0).abs() < 0.1, "{pressure}");
        assert!((temperature + 5.0).abs() < 0.01, "{temperature}");
    }

    #[test]
    fn durations_per_oversampling_index_and_timing_choice() {
        let (mut h, id) = setup();
        for (index, (typical, maximum)) in TYPICAL_US.iter().zip(MAXIMUM_US).enumerate() {
            for (cmd_base, name) in [(0x40u8, "D1"), (0x50u8, "D2")] {
                for maximum_timing in [false, true] {
                    sensor_mut(&mut h, id).set_use_maximum_conversion_time(maximum_timing);
                    let start = h.now();
                    command(&mut h, cmd_base + 2 * index as u8);
                    let expected = if maximum_timing { maximum } else { *typical };
                    assert_eq!(sensor(&h, id).last_duration_us(), expected, "{name} index {index}");
                    assert_eq!(h.next_event_time(), Some(start + expected * US));
                    h.advance_to(start + expected * US);
                    assert!(!sensor(&h, id).pending());
                    adc_read(&mut h);
                }
            }
        }
        // Commands that are not conversions (odd, or beyond 0x4A / 0x5A) start nothing.
        for cmd in [0x41, 0x4B, 0x4C, 0x4E, 0x51, 0x5B, 0x5C, 0x60, 0x2E] {
            command(&mut h, cmd);
            assert!(!sensor(&h, id).pending(), "command 0x{cmd:02X}");
        }
        assert_eq!(h.next_event_time(), None);
    }

    #[test]
    fn early_reads_and_overlapping_conversions_return_invalid_zero() {
        let (mut h, id) = setup();
        // An ADC read while the conversion is pending returns 0 and invalidates that conversion.
        command(&mut h, 0x48); // OSR 4096, 8220 us
        let start = h.now();
        h.advance_to(start + 5000 * US);
        assert_eq!(adc_read(&mut h), 0, "premature read");
        h.advance_to(start + 8220 * US);
        assert!(!sensor(&h, id).pending());
        assert_eq!(sensor(&h, id).raw_d1(), 0, "the conversion completed as invalid");
        assert_eq!(sensor(&h, id).pressure_conversions(), 1);
        assert_eq!(adc_read(&mut h), 0, "and its result reads zero");
        // A conversion started while another is pending is invalid; the old completion is superseded.
        command(&mut h, 0x40); // 540 us
        let a = h.now();
        h.advance_to(a + 100 * US);
        command(&mut h, 0x40);
        let b = h.now();
        assert!(sensor(&h, id).pending());
        h.advance_to(a + 540 * US);
        assert!(sensor(&h, id).pending(), "the first completion no longer applies (generation)");
        h.advance_to(b + 540 * US);
        assert!(!sensor(&h, id).pending());
        assert_eq!(adc_read(&mut h), 0, "overlapping conversion is invalid");
        // A fresh, undisturbed conversion is valid again.
        command(&mut h, 0x40);
        let c = h.now();
        h.advance_to(c + 540 * US);
        assert!(adc_read(&mut h) > 0);
        // Reset (0x1E) cancels a pending conversion and clears the counters, keeps the inputs.
        command(&mut h, 0x4A);
        command(&mut h, 0x1E);
        assert!(!sensor(&h, id).pending());
        let d = h.now();
        h.advance_to(d + 20_000 * US);
        assert_eq!(sensor(&h, id).pressure_conversions(), 0);
        assert_eq!(sensor(&h, id).raw_d1(), 0);
        assert_eq!(sensor(&h, id).pressure_mbar(), 1013.25);
        assert_eq!(adc_read(&mut h), 0);
    }

    #[test]
    fn reset_from_the_bus_resets_the_target_and_cancels_events() {
        let (mut h, id) = setup();
        command(&mut h, 0x4A);
        h.core_mut().reset_all();
        assert!(!sensor(&h, id).pending());
        let t = h.now();
        h.advance_to(t + 20_000 * US);
        assert_eq!(sensor(&h, id).pressure_conversions(), 0, "the generation was bumped by the reset");
    }

    #[test]
    fn schedule_action_is_measured_from_the_exact_cpu_time() {
        // A CPU access in the middle of a chunk: the command is carried by an I2C register write whose exact
        // time is later than the clock time; ScheduleAction first syncs to it.
        let (mut h, id) = setup();
        let chunk_start = h.now();
        let exact = chunk_start + 37 * US;
        // HAL_I2C_Master_Transmit with the CPU's exact time on the transfer-starting CR2 write.
        h.cpu_write32(BASE + 0x04, (DEV & 0x3FF) | (1 << 16) | (1 << 25) | (1 << 13), exact); // CR2: START, AUTOEND, NBYTES=1
        h.cpu_write32(BASE + 0x28, 0x48, exact); // TXDR: the command byte reaches the target here
        h.end_chunk(exact + 10 * US);
        assert!(sensor(&h, id).pending());
        assert_eq!(h.next_event_time(), Some(exact + 8_220_000), "completion = exact access time + duration");
    }

    #[test]
    fn inputs_are_validated_like_the_csharp_setters() {
        let mut sensor = NgcMs5837::with_defaults("p");
        for bad in [f64::NAN, -0.1, 30000.1, f64::INFINITY] {
            assert!(matches!(sensor.set_pressure_mbar(bad), Err(Ms5837Error::PressureOutOfRange(_))), "{bad}");
        }
        assert_eq!(sensor.pressure_mbar(), 1013.25, "a refused value changes nothing");
        sensor.set_pressure_mbar(0.0).unwrap();
        sensor.set_pressure_mbar(30000.0).unwrap();
        for bad in [f64::NAN, -20.1, 85.1, f64::NEG_INFINITY] {
            assert!(matches!(sensor.set_temperature_celsius(bad), Err(Ms5837Error::TemperatureOutOfRange(_))), "{bad}");
        }
        assert_eq!(sensor.temperature_celsius(), 20.0);
        sensor.set_temperature_celsius(-20.0).unwrap();
        sensor.set_temperature_celsius(85.0).unwrap();
        assert!(NgcMs5837::new("p", 31000.0, 20.0).is_err());
        assert!(NgcMs5837::new("p", 1000.0, 90.0).is_err());
        assert!(NgcMs5837::new("p", 1000.0, 25.0).is_ok());
        // Extreme but legal inputs saturate D1 into 24 bits instead of overflowing.
        let (d1, d2) = {
            let mut extreme = NgcMs5837::new("p", 30000.0, -20.0).unwrap();
            extreme.set_use_maximum_conversion_time(true);
            extreme.generate_raw()
        };
        assert!(d1 <= 0x00FF_FFFF && d2 <= 0x00FF_FFFF);
    }

    #[test]
    fn summary_text_has_the_two_decimal_format() {
        let mut sensor = NgcMs5837::with_defaults("p");
        assert_eq!(
            sensor.describe(),
            "MS5837 synthetic PROM: pressure=1013.25mbar; temperature=20.00C; D1=0; D2=0; conversions=0/0; PROMreads=0; pending=False; durationUs=0; maximumTiming=False"
        );
        sensor.set_pressure_mbar(1013.125).unwrap();
        sensor.set_temperature_celsius(20.005).unwrap();
        sensor.set_use_maximum_conversion_time(true);
        // .NET formats exact decimal expansions with ties to even like Rust: 1013.125 -> 1013.12, 20.005 is below the tie.
        assert!(sensor.describe().starts_with("MS5837 synthetic PROM: pressure=1013.12mbar; temperature=20.00C;"));
        assert!(sensor.describe().ends_with("maximumTiming=True"));
    }
}
