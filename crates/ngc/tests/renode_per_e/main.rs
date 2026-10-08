//! Regression test against recorded Renode 1.17.0 behaviour: replays the operation lists recorded on 2026-10-08
//! (scripted and seeded-random sequences against the unmodified C# `NGCParallelLCD`, `NGCMainADC`, `NGCMS5837`
//! and `NGCQuadSPI` models, which were driven through a CPU-less Renode machine; the generator is not part of
//! this repository, see `testdata/README.md`) on `ngc::models::{lcd, adc_main, ms5837, qspi}` and requires
//! identical results for every operation: register reads, `Summary` text, GPIO edge logs with their times, DMA
//! reads of the ADC data register, LCD image hashes, I2C read bytes, NOR backing-file hashes after every
//! operation and the set of warning/error log lines. The golden data (`golden.rs`, generated string constants)
//! is Renode output, not Rust output.

mod golden;

use emu_core::testing::Harness;
use emu_core::{impl_peripheral_any, Ctx, Json, LogLevel, Peripheral, Time, Width};
use ngc::models::adc_main::{self, NgcMainAdc};
use ngc::models::lcd::{self, NgcParallelLcd};
use ngc::models::ms5837::NgcMs5837;
use ngc::models::qspi::{self, NgcQuadSpi};
use ngc::sha256::digest_hex;
use std::cell::RefCell;
use std::collections::BTreeSet;
use stm32::i2c::{Stm32F7I2c, I2C_SIZE};

const LCD_BASE: u32 = 0x6000_0000;
const ADC_BASE: u32 = 0x5004_0000;
const I2C_BASE: u32 = 0x4000_5400;
const QSPI_BASE: u32 = 0xA000_1000;

// ---- test doubles (Rust twins of DiffPerE.cs) ----

/// Twin of `DiffEdgeLog`: "<clock time ns>:<input><+|->;" per level change, "-" when drained empty.
struct EdgeLog {
    log: String,
}

impl EdgeLog {
    fn new() -> Self {
        Self { log: String::new() }
    }

    fn drain(&mut self) -> String {
        if self.log.is_empty() {
            "-".to_string()
        } else {
            std::mem::take(&mut self.log)
        }
    }
}

impl Peripheral for EdgeLog {
    fn name(&self) -> &str {
        "edges"
    }
    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }
    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        self.log.push_str(&format!("{}:{}{};", ctx.now(), line, if level { '+' } else { '-' }));
    }
    impl_peripheral_any!();
}

/// Twin of `DiffDmaReader`: on a rising edge of input 0 it reads the ADC data register through the bus.
struct DmaReader {
    log: String,
    armed: bool,
}

impl DmaReader {
    fn drain(&mut self) -> String {
        if self.log.is_empty() {
            "-".to_string()
        } else {
            std::mem::take(&mut self.log)
        }
    }
}

impl Peripheral for DmaReader {
    fn name(&self) -> &str {
        "dma"
    }
    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }
    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        let now = ctx.now();
        self.log.push_str(&format!("{now}:{line}{};", if level { '+' } else { '-' }));
        if level && line == 0 && self.armed {
            let value = ctx.mem_read(ADC_BASE + adc_main::reg::DR, Width::Word);
            self.log.push_str(&format!("{now}:D{value};"));
        }
    }
    impl_peripheral_any!();
}

// ---- helpers ----

fn arg(op: &[Json], index: usize) -> u64 {
    op[index].as_u64().unwrap_or_else(|| panic!("operation argument {index} is not an unsigned integer: {op:?}"))
}

/// A `double` argument as the Renode monitor delivers it: the monitor parses numeric tokens through single
/// precision (`adc ReferenceMillivolts 0.1` stores 0.10000000149011612), so the recorded Renode results belong to
/// the `f32`-rounded value. The models themselves take exact `f64`.
fn farg(op: &[Json], index: usize) -> f64 {
    let value = op[index].as_f64().unwrap_or_else(|| panic!("operation argument {index} is not a number: {op:?}"));
    f64::from(value as f32)
}

fn text_arg(op: &[Json], index: usize) -> &str {
    op[index].as_str().unwrap_or_else(|| panic!("operation argument {index} is not a string: {op:?}"))
}

fn flag_arg(op: &[Json], index: usize) -> bool {
    match &op[index] {
        Json::Bool(b) => *b,
        Json::Str(s) => s == "true",
        other => panic!("operation argument {index} is not a flag: {other:?}"),
    }
}

/// The pixel stream of the `pix` operation (must match `lcg_values` in `gen_golden.py`).
fn lcg_values(seed: u64, count: u64, black_mod: u64) -> Vec<u16> {
    let mut state = seed as u32;
    (0..count)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let value = (state >> 16) as u16;
            if black_mod != 0 && u64::from(value) % black_mod == 0 {
                0
            } else {
                value
            }
        })
        .collect()
}

fn error_result(failed: bool) -> Json {
    Json::from_pairs([("error", Json::Bool(failed))])
}

fn ok_or_error<T, E>(result: Result<T, E>) -> Json {
    error_result(result.is_err())
}

/// Framework-level bus messages are logged once per address and width by `emu-core` and by Renode once per
/// access with different texts: `Attempted Word write isn't supported ... value 0x..` is compared without the
/// value, and every access to an unmapped address (`WriteWord to non existing peripheral at ...`, DMA's
/// `Tried to access bytes at non-existing peripheral in range ...`) is one "unmapped access" class.
fn normalize(level: char, source: &str, message: &str) -> String {
    let prefixed = format!("{source}: ");
    let message = message.strip_prefix(&prefixed).unwrap_or(message);
    if message.contains("non existing peripheral") || message.contains("non-existing peripheral") {
        return format!("{level}|bus: unmapped access");
    }
    if message.contains("isn't supported by the peripheral") {
        let cut = message.find(", value 0x").map_or(message, |at| &message[..at]);
        return format!("{level}|{source}: {}", cut.trim_end_matches('.'));
    }
    format!("{level}|{source}: {message}")
}

fn render_log(entries: Vec<emu_core::LogEntry>) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let level = match entry.level {
                LogLevel::Warning => 'W',
                LogLevel::Error => 'E',
                _ => return None,
            };
            Some(normalize(level, &entry.source, &entry.message))
        })
        .collect()
}

fn golden_messages(scenario: &Json) -> BTreeSet<String> {
    scenario
        .get("messages")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .map(|m| {
            let parts = m.as_array().unwrap();
            normalize(parts[0].as_str().unwrap().chars().next().unwrap(), parts[1].as_str().unwrap(), parts[2].as_str().unwrap())
        })
        .collect()
}

/// Walks the operations, applies each with `apply` and compares the result with the recorded one.
fn replay(
    model: &str,
    scenario: &Json,
    h: &mut Harness,
    mut apply: impl FnMut(&mut Harness, &[Json], &mut Time) -> Json,
    mut after_op: impl FnMut(&mut Harness, usize) -> Result<(), String>,
) -> Result<(), String> {
    let name = scenario.get("name").and_then(Json::as_str).unwrap_or("?");
    let ops = scenario.get("ops").and_then(Json::as_array).expect("ops");
    let results = scenario.get("results").and_then(Json::as_array).expect("results");
    assert_eq!(ops.len(), results.len(), "{model}/{name}: golden ops and results must be aligned");
    let mut now: Time = 0;
    for (index, (op, want)) in ops.iter().zip(results).enumerate() {
        let op = op.as_array().expect("op");
        let got = apply(h, op, &mut now);
        if &got != want {
            let shown = |json: &Json| {
                let text = json.to_string();
                if text.len() > 300 {
                    format!("{}... ({} bytes)", &text[..300], text.len())
                } else {
                    text
                }
            };
            return Err(format!("{model}/{name} op #{index} {}: got {} want {}", Json::Array(op.to_vec()), shown(&got), shown(want)));
        }
        after_op(h, index).map_err(|e| format!("{model}/{name} op #{index} {}: {e}", Json::Array(op.to_vec())))?;
    }
    let got = render_log(h.drain_log());
    let want = golden_messages(scenario);
    if got != want {
        return Err(format!("{model}/{name}: log messages differ\n  Rust only: {:?}\n  Renode only: {:?}", got.difference(&want).collect::<Vec<_>>(), want.difference(&got).collect::<Vec<_>>()));
    }
    Ok(())
}

// ---- LCD ----

fn replay_lcd(scenario: &Json) -> Result<(), String> {
    let simulate_te = scenario.get("params").and_then(|p| p.get("simulateTE")).and_then(Json::as_bool).expect("simulateTE");
    let mut h = Harness::new();
    let id = h.add_mapped(LCD_BASE, lcd::SIZE, NgcParallelLcd::new("lcd", simulate_te));
    let edges = h.add(EdgeLog::new());
    h.connect_input(id, lcd::TE, edges, 0);
    replay(
        "lcd",
        scenario,
        &mut h,
        |h, op, now| {
            let at = |index: usize| LCD_BASE + arg(op, index) as u32;
            match op[0].as_str().unwrap() {
                "w16" => h.write16(at(1), arg(op, 2) as u32),
                "w32" => h.write32(at(1), arg(op, 2) as u32),
                "w8" => h.write8(at(1), arg(op, 2) as u32),
                "r16" => return Json::from(u64::from(h.read16(at(1)))),
                "r32" => return Json::from(u64::from(h.read32(at(1)))),
                "r8" => return Json::from(u64::from(h.read8(at(1)))),
                "pix" => {
                    for value in lcg_values(arg(op, 2), arg(op, 1), arg(op, 3)) {
                        h.write16(LCD_BASE + lcd::DATA_OFFSET, u32::from(value));
                    }
                }
                "flat" => {
                    for _ in 0..arg(op, 1) {
                        h.write16(LCD_BASE + lcd::DATA_OFFSET, arg(op, 2) as u32);
                    }
                }
                "run" => {
                    *now += arg(op, 1);
                    h.advance_to(*now);
                }
                "sum" => return Json::from(h.get::<NgcParallelLcd>(id).describe()),
                "drain" => return Json::from(h.get_mut::<EdgeLog>(edges).drain()),
                "ppm" => {
                    let data = h.get_mut::<NgcParallelLcd>(id).export_ppm();
                    return Json::from_pairs([("len", Json::from(data.len())), ("sha", Json::from(digest_hex(&data)))]);
                }
                "in" => h.set_input(id, lcd::RESET_INPUT, arg(op, 1) != 0),
                "reset" => h.with::<NgcParallelLcd, _>(id, |l, ctx| Peripheral::reset(l, ctx)),
                other => panic!("unknown LCD op {other}"),
            }
            Json::Null
        },
        |_, _| Ok(()),
    )
}

// ---- main ADC ----

fn replay_adc(scenario: &Json) -> Result<(), String> {
    let interval = scenario.get("params").and_then(|p| p.get("intervalUs")).and_then(Json::as_u64).expect("intervalUs") as u32;
    let mut h = Harness::new();
    let config = adc_main::AdcConfig { conversion_interval_us: interval, ..adc_main::AdcConfig::default() };
    let id = h.add_mapped(ADC_BASE, adc_main::SIZE, NgcMainAdc::new("adc", config).unwrap());
    let dma = h.add(DmaReader { log: String::new(), armed: true });
    let irq = h.add(EdgeLog::new());
    h.connect_input(id, adc_main::DMA_REQUEST, dma, 0);
    h.connect_input(id, adc_main::IRQ, irq, 0);
    replay(
        "adc",
        scenario,
        &mut h,
        |h, op, now| {
            let at = |index: usize| ADC_BASE + arg(op, index) as u32;
            match op[0].as_str().unwrap() {
                "w16" => h.write16(at(1), arg(op, 2) as u32),
                "w32" => h.write32(at(1), arg(op, 2) as u32),
                "w8" => h.write8(at(1), arg(op, 2) as u32),
                "r16" => return Json::from(u64::from(h.read16(at(1)))),
                "r32" => return Json::from(u64::from(h.read32(at(1)))),
                "r8" => return Json::from(u64::from(h.read8(at(1)))),
                "run" => {
                    *now += arg(op, 1);
                    h.advance_to(*now);
                }
                "sum" => return Json::from(h.get::<NgcMainAdc>(id).describe()),
                "irq" => return Json::from(h.get_mut::<EdgeLog>(irq).drain()),
                "obs" => {
                    let dma_text = h.get_mut::<DmaReader>(dma).drain();
                    let irq_text = h.get_mut::<EdgeLog>(irq).drain();
                    return Json::from_items([dma_text, irq_text]);
                }
                "dma" => h.get_mut::<DmaReader>(dma).armed = flag_arg(op, 1),
                "reset" => h.with::<NgcMainAdc, _>(id, |a, ctx| Peripheral::reset(a, ctx)),
                "set" => return adc_method(h.get_mut::<NgcMainAdc>(id), op),
                "prop" => return adc_property(h.get_mut::<NgcMainAdc>(id), op),
                "get" => return adc_getter(h.get::<NgcMainAdc>(id), op),
                other => panic!("unknown ADC op {other}"),
            }
            Json::Null
        },
        |_, _| Ok(()),
    )
}

/// `adc <Method> args` of the monitor: the runner methods; the result says whether the C# method threw.
fn adc_method(adc: &mut NgcMainAdc, op: &[Json]) -> Json {
    let result = match text_arg(op, 1) {
        "SetRawInputs" => {
            adc.set_raw_inputs(arg(op, 2) as u32, arg(op, 3) as u32, arg(op, 4) as u32, arg(op, 5) as u32, arg(op, 6) as u32, arg(op, 7) as u32);
            Ok(())
        }
        "SetChannelRaw" => adc.set_channel_raw(arg(op, 2) as u32, arg(op, 3) as u32),
        "SetPinMillivolts" => adc.set_pin_millivolts(arg(op, 2) as u32, farg(op, 3)),
        "SetBatteryMillivolts" => adc.set_battery_millivolts(arg(op, 2) as u32, farg(op, 3)),
        "SetOxygenMillivolts" => adc.set_oxygen_millivolts(arg(op, 2) as u32, farg(op, 3)),
        other => panic!("unknown ADC method {other}"),
    };
    ok_or_error(result)
}

fn adc_property(adc: &mut NgcMainAdc, op: &[Json]) -> Json {
    match text_arg(op, 1) {
        "AcquisitionEnabled" => adc.set_acquisition_enabled(flag_arg(op, 2)),
        "InitialAcquisitionDelayUs" => adc.set_initial_acquisition_delay_us(arg(op, 2) as u32),
        "NoiseAmplitudeRaw" => adc.set_noise_amplitude_raw(arg(op, 2) as u32),
        "NoiseSeed" => adc.set_noise_seed(arg(op, 2) as u32),
        "ReferenceMillivolts" => adc.set_reference_millivolts(farg(op, 2)),
        other => panic!("unknown ADC property {other}"),
    }
    error_result(false)
}

fn adc_getter(adc: &NgcMainAdc, op: &[Json]) -> Json {
    let int = |value: Option<u32>| Json::from(u64::from(value.expect("channel")));
    match text_arg(op, 1) {
        "Oxygen1Raw" => int(adc.oxygen_raw(0)),
        "Oxygen2Raw" => int(adc.oxygen_raw(1)),
        "Oxygen3Raw" => int(adc.oxygen_raw(2)),
        "Battery1Raw" => int(adc.battery_raw(0)),
        "Battery2Raw" => int(adc.battery_raw(1)),
        "BoardIdRaw" => Json::from(u64::from(adc.board_id_raw())),
        "NoiseAmplitudeRaw" => Json::from(u64::from(adc.noise_amplitude_raw())),
        "Enabled" => Json::Bool(adc.enabled()),
        "Converting" => Json::Bool(adc.converting()),
        "DataReady" => Json::Bool(adc.data_ready()),
        other => panic!("unknown ADC getter {other}"),
    }
}

// ---- main ADC + the real DMA model + SRAM ----

fn replay_adc_dma(scenario: &Json) -> Result<(), String> {
    let interval = scenario.get("params").and_then(|p| p.get("intervalUs")).and_then(Json::as_u64).expect("intervalUs") as u32;
    let mut h = Harness::new();
    let config = adc_main::AdcConfig { conversion_interval_us: interval, ..adc_main::AdcConfig::default() };
    // Platform order of the golden .repl: dma1 first (its channel-1 IRQ goes to an edge log), then the ADC.
    let dma = h.add_mapped(0x4002_0000, 0x400, stm32::dma::Dma::new("dma1"));
    let adc = h.add_mapped(ADC_BASE, adc_main::SIZE, NgcMainAdc::new("adc", config).unwrap());
    let dirq = h.add(EdgeLog::new());
    let airq = h.add(EdgeLog::new());
    h.connect_input(dma, 0, dirq, 0);
    h.connect_input(adc, adc_main::DMA_REQUEST, dma, 0);
    h.connect_input(adc, adc_main::IRQ, airq, 0);
    replay(
        "adcdma",
        scenario,
        &mut h,
        |h, op, now| {
            match op[0].as_str().unwrap() {
                "w32" => h.write32(arg(op, 1) as u32, arg(op, 2) as u32),
                "r32" => return Json::from(u64::from(h.read32(arg(op, 1) as u32))),
                "run" => {
                    *now += arg(op, 1);
                    h.advance_to(*now);
                }
                "sum" => return Json::from(h.get::<NgcMainAdc>(adc).describe()),
                "obs" => {
                    let dma_edges = h.get_mut::<EdgeLog>(dirq).drain();
                    let adc_edges = h.get_mut::<EdgeLog>(airq).drain();
                    return Json::from_items([dma_edges, adc_edges]);
                }
                "reset" => h.with::<NgcMainAdc, _>(adc, |a, ctx| Peripheral::reset(a, ctx)),
                "set" => return adc_method(h.get_mut::<NgcMainAdc>(adc), op),
                "prop" => return adc_property(h.get_mut::<NgcMainAdc>(adc), op),
                other => panic!("unknown ADC+DMA op {other}"),
            }
            Json::Null
        },
        |_, _| Ok(()),
    )
}

// ---- MS5837 ----

fn replay_ms5837(scenario: &Json) -> Result<(), String> {
    let mut h = Harness::new();
    let mut controller = Stm32F7I2c::new("i2c1");
    controller.attach(0x76, Box::new(NgcMs5837::with_defaults("pressure1"))).unwrap();
    let id = h.add_mapped(I2C_BASE, I2C_SIZE, controller);
    replay(
        "ms5837",
        scenario,
        &mut h,
        |h, op, now| {
            let at = |index: usize| I2C_BASE + arg(op, index) as u32;
            match op[0].as_str().unwrap() {
                "w32" => h.write32(at(1), arg(op, 2) as u32),
                "r32" => return Json::from(u64::from(h.read32(at(1)))),
                "run" => {
                    *now += arg(op, 1);
                    h.advance_to(*now);
                }
                "tx" => {
                    let dev = arg(op, 1) as u32;
                    let data = op[2].as_array().unwrap();
                    h.write32(I2C_BASE + 0x04, (dev & 0x3FF) | ((data.len() as u32) << 16) | (1 << 25) | (1 << 13));
                    for byte in data {
                        h.write32(I2C_BASE + 0x28, byte.as_u64().unwrap() as u32);
                    }
                    h.write32(I2C_BASE + 0x1C, 0x20);
                    h.write32(I2C_BASE + 0x04, 0);
                }
                "rx" => {
                    let dev = arg(op, 1) as u32;
                    let count = arg(op, 2) as u32;
                    h.write32(I2C_BASE + 0x04, (dev & 0x3FF) | (1 << 10) | (count << 16) | (1 << 25) | (1 << 13));
                    let bytes: Vec<u64> = (0..count).map(|_| u64::from(h.read32(I2C_BASE + 0x24) & 0xFF)).collect();
                    h.write32(I2C_BASE + 0x1C, 0x20);
                    h.write32(I2C_BASE + 0x04, 0);
                    return Json::from_items(bytes);
                }
                "sum" => return Json::from(h.get::<Stm32F7I2c>(id).target::<NgcMs5837>(0x76).unwrap().describe()),
                "reset" => h.get_mut::<Stm32F7I2c>(id).target_mut::<NgcMs5837>(0x76).unwrap().reset_device(),
                "prop" => {
                    let sensor = h.get_mut::<Stm32F7I2c>(id).target_mut::<NgcMs5837>(0x76).unwrap();
                    return match text_arg(op, 1) {
                        "PressureMbar" => ok_or_error(sensor.set_pressure_mbar(farg(op, 2))),
                        "TemperatureCelsius" => ok_or_error(sensor.set_temperature_celsius(farg(op, 2))),
                        "UseMaximumConversionTime" => {
                            sensor.set_use_maximum_conversion_time(flag_arg(op, 2));
                            error_result(false)
                        }
                        other => panic!("unknown MS5837 property {other}"),
                    };
                }
                "get" => {
                    let sensor = h.get::<Stm32F7I2c>(id).target::<NgcMs5837>(0x76).unwrap();
                    return Json::from(match text_arg(op, 1) {
                        "RawD1" => u64::from(sensor.raw_d1()),
                        "RawD2" => u64::from(sensor.raw_d2()),
                        "PressureConversions" => sensor.pressure_conversions(),
                        "TemperatureConversions" => sensor.temperature_conversions(),
                        "PromReads" => sensor.prom_reads(),
                        other => panic!("unknown MS5837 getter {other}"),
                    });
                }
                "getprom" => {
                    let sensor = h.get::<Stm32F7I2c>(id).target::<NgcMs5837>(0x76).unwrap();
                    return match sensor.prom_word(arg(op, 1) as u32) {
                        Ok(word) => Json::from(u64::from(word)),
                        Err(_) => error_result(true),
                    };
                }
                other => panic!("unknown MS5837 op {other}"),
            }
            Json::Null
        },
        |_, _| Ok(()),
    )
}

// ---- QSPI ----

fn replay_qspi(scenario: &Json) -> Result<(), String> {
    let mut h = Harness::new();
    let id = h.add_mapped(QSPI_BASE, qspi::SIZE, NgcQuadSpi::new("qspi"));
    let irq = h.add(EdgeLog::new());
    h.connect_input(id, qspi::IRQ, irq, 0);
    let golden_files = scenario.get("files").and_then(Json::as_array).expect("files").to_vec();
    // The "disk": the backing file as the persistence layer writes it (every flush request rewrites it), and
    // the digest of Renode's file after the operation (carried forward while the operation did not change it).
    let disk: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    let golden_digest: RefCell<Option<String>> = RefCell::new(None);
    replay(
        "qspi",
        scenario,
        &mut h,
        |h, op, now| {
            let at = |index: usize| QSPI_BASE + arg(op, index) as u32;
            match op[0].as_str().unwrap() {
                "w16" => h.write16(at(1), arg(op, 2) as u32),
                "w32" => h.write32(at(1), arg(op, 2) as u32),
                "w8" => h.write8(at(1), arg(op, 2) as u32),
                "r16" => return Json::from(u64::from(h.read16(at(1)))),
                "r32" => return Json::from(u64::from(h.read32(at(1)))),
                "r8" => return Json::from(u64::from(h.read8(at(1)))),
                "run" => {
                    *now += arg(op, 1);
                    h.advance_to(*now);
                }
                "sum" => return Json::from(h.get::<NgcQuadSpi>(id).describe()),
                "irq" => return Json::from(h.get_mut::<EdgeLog>(irq).drain()),
                "get_storage" => return Json::from(u64::from(h.get::<NgcQuadSpi>(id).get_storage_byte(arg(op, 1) as u32))),
                "m" => match text_arg(op, 1) {
                    "LoadBackingFile" => {
                        let existing = disk.borrow().clone();
                        h.get_mut::<NgcQuadSpi>(id).load_backing("{BACKING}", existing.as_deref()).unwrap();
                    }
                    "SaveBackingFile" => *disk.borrow_mut() = Some(h.get::<NgcQuadSpi>(id).serialize_backing()),
                    "Flush" => h.get_mut::<NgcQuadSpi>(id).flush(),
                    "EraseStorage" => h.get_mut::<NgcQuadSpi>(id).erase_storage(),
                    "AutoPersist" => h.get_mut::<NgcQuadSpi>(id).set_auto_persist(flag_arg(op, 2)),
                    "Reset" => h.with::<NgcQuadSpi, _>(id, |q, ctx| Peripheral::reset(q, ctx)),
                    other => panic!("unknown QSPI method {other}"),
                },
                other => panic!("unknown QSPI op {other}"),
            }
            Json::Null
        },
        |h, index| {
            if h.get_mut::<NgcQuadSpi>(id).take_persist_request() {
                *disk.borrow_mut() = Some(h.get::<NgcQuadSpi>(id).serialize_backing());
            }
            if let Some(entry) = golden_files.get(index).filter(|entry| !entry.is_null()) {
                *golden_digest.borrow_mut() = entry.get("sha").and_then(Json::as_str).map(str::to_string);
                let want_len = entry.get("len").and_then(Json::as_u64).unwrap_or(0) as usize;
                let have_len = disk.borrow().as_ref().map_or(0, Vec::len);
                if have_len != want_len {
                    return Err(format!("backing file has {have_len} bytes, Renode's has {want_len}"));
                }
            }
            let ours = disk.borrow().as_ref().map(|bytes| digest_hex(bytes));
            if ours != *golden_digest.borrow() {
                return Err(format!("backing file digest {ours:?} differs from Renode's {:?}", golden_digest.borrow()));
            }
            Ok(())
        },
    )
}

// ---- driver ----

fn run_model(model: &str, text: &str, replay_one: fn(&Json) -> Result<(), String>) {
    let json = Json::parse(text).expect("golden json");
    let scenarios: Vec<&Json> =
        json.get("scenarios").and_then(Json::as_array).expect("scenarios").iter().filter(|s| s.get("model").and_then(Json::as_str) == Some(model)).collect();
    assert!(!scenarios.is_empty(), "no {model} scenarios in the golden data");
    let mut failures = Vec::new();
    let mut operations = 0usize;
    for scenario in &scenarios {
        operations += scenario.get("ops").and_then(Json::as_array).map_or(0, <[Json]>::len);
        if let Err(message) = replay_one(scenario) {
            failures.push(message);
        }
    }
    println!("{model}: {} scenarios, {operations} operations replayed against Renode's recording", scenarios.len());
    assert!(failures.is_empty(), "{} of {} {model} scenarios differ from Renode:\n{}", failures.len(), scenarios.len(), failures.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn lcd_matches_the_renode_recording() {
    run_model("lcd", golden::LCD, replay_lcd);
}

#[test]
fn main_adc_matches_the_renode_recording() {
    run_model("adc", golden::ADC, replay_adc);
}

#[test]
fn main_adc_with_the_dma_model_matches_the_renode_recording() {
    run_model("adcdma", golden::ADCDMA, replay_adc_dma);
}

#[test]
fn ms5837_matches_the_renode_recording() {
    run_model("ms5837", golden::MS5837, replay_ms5837);
}

#[test]
fn qspi_matches_the_renode_recording() {
    run_model("qspi", golden::QSPI, replay_qspi);
}
