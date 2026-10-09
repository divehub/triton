//! Regression test against recorded Renode 1.17.0 behavior: replays the operation lists recorded on
//! 2026-10-07 (scripted flows and seeded random register traffic against the unmodified C# `STM32F7_I2C`
//! and `NGCEeprom` models; the generator is not part of this repository, see `testdata/README.md`) on
//! `stm32::i2c::Stm32F7I2c` / `ngc::models::eeprom` and requires identical register reads,
//! ISR/CR2/interrupt-line observations after every step, target call logs, final EEPROM images and log
//! messages. The golden data (`golden.rs`, a generated string constant) is Renode output, not Rust output.

mod golden;

use emu_core::testing::Harness;
use emu_core::{Json, LogLevel, PeriphId};
use ngc::models::eeprom::{NgcEepromStore, EEPROM_DIAGNOSTIC_BASE, EEPROM_SIZE};
use stm32::i2c::{regs, I2cCtx, I2cTarget, Stm32F7I2c, DMA_RECEIVE, ERROR_INTERRUPT, EVENT_INTERRUPT, I2C_SIZE};
use stm32::impl_i2c_target_any;

const BASE: u32 = 0x4000_5400;

/// Rust twin of `DiffMock` in `DiffI2C.cs`: records every call as text and serves an incrementing byte stream.
struct DiffMock {
    name: String,
    calls: String,
    next: u8,
}

impl DiffMock {
    fn new(name: String) -> Self {
        Self { name, calls: "L:".to_string(), next: 0 }
    }
}

impl I2cTarget for DiffMock {
    fn name(&self) -> &str {
        &self.name
    }

    fn write(&mut self, data: &[u8], _ctx: &mut I2cCtx<'_, '_>) {
        let hex: Vec<String> = data.iter().map(|byte| format!("{byte:02X}")).collect();
        self.calls.push_str(&format!("W[{}];", hex.join("-")));
    }

    fn read(&mut self, count: usize, _ctx: &mut I2cCtx<'_, '_>) -> Vec<u8> {
        self.calls.push_str(&format!("R{count};"));
        (0..count)
            .map(|_| {
                let value = self.next;
                self.next = self.next.wrapping_add(1);
                value
            })
            .collect()
    }

    fn finish_transmission(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
        self.calls.push_str("F;");
    }

    fn reset(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
        self.next = 0;
        self.calls.push_str("RESET;");
    }

    impl_i2c_target_any!();
}

#[derive(Debug, PartialEq)]
enum Observed {
    Nothing,
    Value(u64),
    Bytes(Vec<u64>),
    Obs { isr: u64, cr2: u64, levels: u64, deltas: Vec<String> },
}

fn parse_result(json: &Json) -> Observed {
    match json {
        Json::Null => Observed::Nothing,
        Json::Object(_) => {
            Observed::Bytes(json.get("bytes").unwrap().as_array().unwrap().iter().map(|b| b.as_u64().unwrap()).collect())
        }
        Json::Array(items) => Observed::Obs {
            isr: items[0].as_u64().unwrap(),
            cr2: items[1].as_u64().unwrap(),
            levels: items[2].as_u64().unwrap(),
            deltas: items[3].as_array().unwrap().iter().map(|d| d.as_str().unwrap().to_string()).collect(),
        },
        number => Observed::Value(number.as_u64().expect("number")),
    }
}

struct Rig {
    h: Harness,
    id: PeriphId,
    store: Option<NgcEepromStore>,
    mocks: Vec<u32>,
    previous_calls: Vec<usize>,
}

fn build(mocks: &[u32], eeprom: bool) -> Rig {
    let mut h = Harness::new();
    let mut controller = Stm32F7I2c::new("i2c1");
    let store = eeprom.then(NgcEepromStore::new);
    if let Some(store) = &store {
        store.attach_banks(&mut controller).unwrap();
    }
    for &address in mocks {
        controller.attach(address, Box::new(DiffMock::new(format!("mock{address:x}")))).unwrap();
    }
    let id = h.add_mapped(BASE, I2C_SIZE, controller);
    h.connect_irq(id, EVENT_INTERRUPT, 31);
    h.connect_irq(id, ERROR_INTERRUPT, 32);
    if let Some(store) = &store {
        h.add_mapped(EEPROM_DIAGNOSTIC_BASE, EEPROM_SIZE as u32, store.clone());
    }
    Rig { h, id, store, mocks: mocks.to_vec(), previous_calls: vec![2; mocks.len()] }
}

impl Rig {
    fn levels(&self) -> u64 {
        u64::from(self.h.output(self.id, EVENT_INTERRUPT))
            | u64::from(self.h.output(self.id, ERROR_INTERRUPT)) << 1
            | u64::from(self.h.output(self.id, DMA_RECEIVE)) << 2
    }

    fn apply(&mut self, op: &[Json]) -> Observed {
        let kind = op[0].as_str().unwrap();
        let arg = |index: usize| op[index].as_u64().unwrap() as u32;
        match kind {
            "w32" => self.h.write32(BASE + arg(1), arg(2)),
            "w16" => self.h.write16(BASE + arg(1), arg(2)),
            "w8" => self.h.write8(BASE + arg(1), arg(2)),
            "r32" => return Observed::Value(u64::from(self.h.read32(BASE + arg(1)))),
            "r16" => return Observed::Value(u64::from(self.h.read16(BASE + arg(1)))),
            "r8" => return Observed::Value(u64::from(self.h.read8(BASE + arg(1)))),
            "obs" => {
                let isr = u64::from(self.h.read32(BASE + regs::ISR));
                let cr2 = u64::from(self.h.read32(BASE + regs::CR2));
                let levels = self.levels();
                let mut deltas = Vec::new();
                for (slot, &address) in self.mocks.iter().enumerate() {
                    let calls =
                        self.h.get::<Stm32F7I2c>(self.id).target::<DiffMock>(address).expect("mock").calls.clone();
                    deltas.push(calls[self.previous_calls[slot]..].to_string());
                    self.previous_calls[slot] = calls.len();
                }
                return Observed::Obs { isr, cr2, levels, deltas };
            }
            "reset" => self.h.core_mut().reset_all(),
            "slave_write" => {
                let data: Vec<u8> = op[1].as_array().unwrap().iter().map(|b| b.as_u64().unwrap() as u8).collect();
                self.h.with::<Stm32F7I2c, _>(self.id, |i2c, _ctx| i2c.slave_write(&data));
            }
            "slave_read" => {
                let count = arg(1) as usize;
                let bytes = self.h.with::<Stm32F7I2c, _>(self.id, |i2c, ctx| i2c.slave_read(count, ctx));
                return Observed::Bytes(bytes.into_iter().map(u64::from).collect());
            }
            "store_get" => {
                let value = self.store.as_ref().unwrap().get_byte(arg(1)).unwrap();
                return Observed::Value(u64::from(value));
            }
            "store_set_byte" => self.store.as_ref().unwrap().set_byte(arg(1), arg(2) as u8).unwrap(),
            "store_set_dword" => self.store.as_ref().unwrap().set_double_word(arg(1), arg(2)).unwrap(),
            "store_flush" => self.store.as_ref().unwrap().flush(),
            "store_erase" => self.store.as_ref().unwrap().erase(),
            other => panic!("unknown op {other}"),
        }
        Observed::Nothing
    }

    /// `(LEVEL, source, message)` of every log entry at Info or above, normalized like the Renode log.
    fn messages(&self) -> Vec<(String, String, String)> {
        let found = self
            .h
            .core()
            .log
            .entries()
            .filter(|entry| entry.level >= LogLevel::Info)
            .map(|entry| {
                let level = entry.level.name().to_uppercase();
                // The framework's width-translation warnings carry the peripheral name in the text
                // ("i2c1: Attempted ..."); Renode puts it in the source field of the log line.
                let message = entry.message.strip_prefix("i2c1: ").unwrap_or(&entry.message);
                let source = if entry.source == "machine" { "i2c1" } else { entry.source.as_str() };
                (level, source.to_string(), message.to_string())
            })
            .collect();
        normalized(found)
    }
}

/// Sorted distinct messages. The value of an "Attempted <width> write isn't supported" warning is dropped:
/// the framework logs that condition once per offset and width, Renode once per distinct value.
fn normalized(messages: Vec<(String, String, String)>) -> Vec<(String, String, String)> {
    let mut messages: Vec<(String, String, String)> = messages
        .into_iter()
        .map(|(level, source, message)| match message.find(", value 0x") {
            Some(index) if message.starts_with("Attempted ") => (level, source, format!("{}.", &message[..index])),
            _ => (level, source, message),
        })
        .collect();
    messages.sort();
    messages.dedup();
    messages
}

fn run_scenario(scenario: &Json) -> Result<usize, String> {
    let name = scenario.get("name").unwrap().as_str().unwrap();
    let mocks: Vec<u32> = scenario.get("mocks").unwrap().as_array().unwrap().iter().map(|m| m.as_u64().unwrap() as u32).collect();
    let eeprom = scenario.get("eeprom").unwrap().as_bool().unwrap();
    let ops = scenario.get("ops").unwrap().as_array().unwrap();
    let results = scenario.get("results").unwrap().as_array().unwrap();
    assert_eq!(ops.len(), results.len(), "{name}: golden file is inconsistent");
    let mut rig = build(&mocks, eeprom);

    for (index, (op, expected)) in ops.iter().zip(results).enumerate() {
        let op_items = op.as_array().unwrap();
        let actual = rig.apply(op_items);
        let expected = parse_result(expected);
        if actual != expected {
            let context: Vec<String> =
                ops[index.saturating_sub(6)..=index].iter().map(|op| op.to_string()).collect();
            return Err(format!(
                "{name}: op #{index} {op}\n  expected {expected:?}\n  actual   {actual:?}\n  recent ops: {}",
                context.join(" ")
            ));
        }
    }

    // Final EEPROM image and summary.
    let final_state = scenario.get("final").unwrap();
    if let Some(store) = &rig.store {
        let image = store.image();
        let mut segments: Vec<(u64, String)> = Vec::new();
        let mut offset = 0;
        while offset < image.len() {
            if image[offset] == 0xFF {
                offset += 1;
                continue;
            }
            let start = offset;
            while offset < image.len() && image[offset] != 0xFF {
                offset += 1;
            }
            let hex: String = image[start..offset].iter().map(|byte| format!("{byte:02x}")).collect();
            segments.push((start as u64, hex));
        }
        let expected_segments: Vec<(u64, String)> = final_state
            .get("segments")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s.at(0).unwrap().as_u64().unwrap(), s.at(1).unwrap().as_str().unwrap().to_string()))
            .collect();
        if segments != expected_segments {
            return Err(format!("{name}: final EEPROM image differs\n  expected {expected_segments:?}\n  actual   {segments:?}"));
        }
        let summary = final_state.get("summary").unwrap().as_str().unwrap();
        if store.summary_text() != summary {
            return Err(format!("{name}: summary {:?} != {summary:?}", store.summary_text()));
        }
    } else {
        assert!(final_state.is_null(), "{name}: unexpected final state");
    }

    // Log messages: the set of distinct (level, source, message) must match.
    let expected_messages = normalized(
        scenario
            .get("messages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let text = |index: usize| m.at(index).unwrap().as_str().unwrap().to_string();
                (text(0), text(1), text(2))
            })
            .collect(),
    );
    let actual_messages = rig.messages();
    if actual_messages != expected_messages {
        let missing: Vec<_> = expected_messages.iter().filter(|m| !actual_messages.contains(m)).collect();
        let extra: Vec<_> = actual_messages.iter().filter(|m| !expected_messages.contains(m)).collect();
        return Err(format!("{name}: log messages differ\n  only in Renode: {missing:#?}\n  only in Rust: {extra:#?}"));
    }
    Ok(ops.len())
}

#[test]
fn replays_renode_transcripts_exactly() {
    let golden = Json::parse(golden::GOLDEN).expect("golden transcript");
    assert_eq!(golden.get("format").and_then(Json::as_u64), Some(1));
    let scenarios = golden.get("scenarios").unwrap().as_array().unwrap();
    assert!(scenarios.len() >= 38, "golden file lost scenarios");
    let mut failures = Vec::new();
    let mut total_ops = 0;
    for scenario in scenarios {
        match run_scenario(scenario) {
            Ok(count) => total_ops += count,
            Err(message) => failures.push(message),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ from Renode:\n{}", failures.len(), scenarios.len(), failures.join("\n\n"));
    eprintln!("{} scenarios, {total_ops} operations identical to Renode", scenarios.len());
}
