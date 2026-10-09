// Differential replay of register-level sequences recorded from the pinned Renode 1.17.0.
//
// The tables below were recorded by running the unmodified `emulation/handset.repl` platform (without
// firmware: nothing executes and virtual time stays 0) in Renode 1.17.0 and driving it with monitor
// commands (`sysbus ReadDoubleWord/WriteWord/...`, `<peripheral> OnGPIO`, `<peripheral> Reset`). Reads
// carry the value Renode returned; `Irq` steps carry the level of an NVIC input as observed through the
// NVIC's `ISPR`/`ICPR` (clearing a pending interrupt is a no-op while its line is still high); `log` lists
// the WARNING (`W|`) and ERROR (`E|`) lines Renode printed for the step, with later repeats of the same
// message removed because these models log each message once. This is the evidence that the models
// reproduce Renode's register-level behavior, quirks included; it says nothing about physical devices.
// Generated from the recorded data by a script; regenerate rather than edit by hand.


use emu_core::testing::Harness;
use emu_core::{LogEntry, LogLevel};
use ngc::models::adc_handset::NgcAdc;
use ngc::models::clock_control::NgcClockControl;

#[derive(Clone, Copy, Debug)]
enum Op {
    W32(u32, u32),
    W16(u32, u32),
    W8(u32, u32),
    R32(u32, u32),
    R16(u32, u32),
    R8(u32, u32),
}

#[derive(Clone, Copy, Debug)]
struct Step {
    op: Op,
    /// New log lines of the step: `W|<source>: <message>`.
    log: &'static [&'static str],
}

const fn s(op: Op) -> Step {
    Step { op, log: &[] }
}

const fn sl(op: Op, log: &'static [&'static str]) -> Step {
    Step { op, log }
}

fn render(entry: &LogEntry) -> Option<String> {
    let level = match entry.level {
        LogLevel::Warning => 'W',
        LogLevel::Error => 'E',
        _ => return None,
    };
    let text = if entry.message.starts_with(&format!("{}: ", entry.source)) {
        entry.message.clone()
    } else {
        format!("{}: {}", entry.source, entry.message)
    };
    Some(format!("{level}|{text}"))
}

fn replay(h: &mut Harness, section: &str, steps: &[Step]) {
    for (i, step) in steps.iter().enumerate() {
        h.drain_log();
        let context = format!("{section} step {i} {:?}", step.op);
        match step.op {
            Op::W32(a, v) => h.write32(a, v),
            Op::W16(a, v) => h.write16(a, v),
            Op::W8(a, v) => h.write8(a, v),
            Op::R32(a, e) => assert_eq!(h.read32(a), e, "{context}"),
            Op::R16(a, e) => assert_eq!(h.read16(a), e, "{context}"),
            Op::R8(a, e) => assert_eq!(h.read8(a), e, "{context}"),
        }
        let produced: Vec<String> = h.drain_log().iter().filter_map(render).collect();
        assert_eq!(produced, step.log, "{context}: new warning/error lines");
    }
}

#[test]
fn clock_control_matches_the_renode_recording() {
    let mut h = Harness::new();
    h.add_mapped(0x4002_1000, 0x400, NgcClockControl::new("rcc"));
    replay(&mut h, "rcc", RCC);
}

#[test]
fn handset_adc_matches_the_renode_recording() {
    let mut h = Harness::new();
    h.add_mapped(0x5004_0000, 0x400, NgcAdc::new("adc", 400));
    replay(&mut h, "adc", ADC);
}

// ---- recorded sequences ----

const RCC: &[Step] = &[
    s(Op::R32(0x40021000, 0x63)),
    s(Op::R32(0x40021004, 0x0)),
    s(Op::R32(0x40021008, 0x0)),
    s(Op::R32(0x4002100C, 0x0)),
    s(Op::R32(0x4002102C, 0x0)),
    s(Op::R32(0x40021090, 0x0)),
    s(Op::R32(0x40021094, 0x0)),
    s(Op::R32(0x40021098, 0x0)),
    s(Op::R32(0x400213FC, 0x0)),
    sl(Op::R8(0x40021000, 0x0), &["W|rcc: Attempted Byte read isn't supported by the peripheral. Offset 0x0."]),
    sl(Op::R16(0x40021000, 0x0), &["W|rcc: Attempted Word read isn't supported by the peripheral. Offset 0x0."]),
    s(Op::W32(0x40021000, 0x1501_0101)),
    s(Op::R32(0x40021000, 0x3F03_0503)),
    s(Op::W32(0x40021000, 0x7E02_0402)),
    s(Op::R32(0x40021000, 0x7C00_0000)),
    s(Op::W32(0x40021000, 0x4_0060)),
    s(Op::R32(0x40021000, 0x4_0060)),
    s(Op::W32(0x40021008, 0x0)),
    s(Op::R32(0x40021008, 0x0)),
    s(Op::W32(0x40021008, 0x1)),
    s(Op::R32(0x40021008, 0x5)),
    s(Op::W32(0x40021008, 0x2)),
    s(Op::R32(0x40021008, 0xA)),
    s(Op::W32(0x40021008, 0x3)),
    s(Op::R32(0x40021008, 0xF)),
    s(Op::W32(0x40021008, 0xC)),
    s(Op::R32(0x40021008, 0x0)),
    s(Op::W32(0x40021008, 0xFF)),
    s(Op::R32(0x40021008, 0xFF)),
    s(Op::W32(0x40021008, 0x1_0002)),
    s(Op::R32(0x40021008, 0x1_000A)),
    s(Op::W32(0x40021008, 0x1_00F1)),
    s(Op::R32(0x40021008, 0x1_00F5)),
    s(Op::W32(0x40021090, 0x101)),
    s(Op::R32(0x40021090, 0x103)),
    s(Op::W32(0x40021090, 0x102)),
    s(Op::R32(0x40021090, 0x100)),
    s(Op::W32(0x40021094, 0x101)),
    s(Op::R32(0x40021094, 0x103)),
    s(Op::W32(0x40021094, 0x102)),
    s(Op::R32(0x40021094, 0x100)),
    s(Op::W32(0x40021098, 0x101)),
    s(Op::R32(0x40021098, 0x103)),
    s(Op::W32(0x40021098, 0x102)),
    s(Op::R32(0x40021098, 0x100)),
    s(Op::W32(0x4002108C, 0x101)),
    s(Op::R32(0x4002108C, 0x101)),
    s(Op::W32(0x4002108C, 0x102)),
    s(Op::R32(0x4002108C, 0x102)),
    s(Op::W32(0x4002109C, 0x101)),
    s(Op::R32(0x4002109C, 0x101)),
    s(Op::W32(0x4002109C, 0x102)),
    s(Op::R32(0x4002109C, 0x102)),
    s(Op::W32(0x4002104C, 0xA5A5_004C)),
    s(Op::R32(0x4002104C, 0xA5A5_004C)),
    s(Op::W32(0x40021058, 0xFFFF_FFFF)),
    s(Op::R32(0x40021058, 0xFFFF_FFFF)),
    sl(Op::W16(0x4002104C, 0x1), &["W|rcc: Attempted Word write isn't supported by the peripheral. Offset 0x4C, value 0x1."]),
    sl(Op::W8(0x4002104C, 0x1), &["W|rcc: Attempted Byte write isn't supported by the peripheral. Offset 0x4C, value 0x1."]),
    s(Op::R32(0x4002104C, 0xA5A5_004C)),
];

const ADC: &[Step] = &[
    s(Op::R32(0x50040000, 0x0)),
    s(Op::R32(0x50040004, 0x0)),
    s(Op::R32(0x50040008, 0x2000_0000)),
    s(Op::R32(0x5004000C, 0x0)),
    s(Op::R32(0x50040010, 0x0)),
    s(Op::R32(0x50040030, 0x0)),
    s(Op::R32(0x50040040, 0x190)),
    s(Op::R32(0x50040308, 0x0)),
    s(Op::W32(0x50040008, 0x0)),
    s(Op::R32(0x50040008, 0x0)),
    s(Op::W32(0x50040008, 0x1000_0000)),
    s(Op::R32(0x50040008, 0x1000_0000)),
    s(Op::W32(0x50040008, 0x9000_0000)),
    s(Op::R32(0x50040008, 0x1000_0000)),
    s(Op::W32(0x50040008, 0x1000_0001)),
    s(Op::R32(0x50040008, 0x1000_0001)),
    s(Op::R32(0x50040000, 0x1)),
    s(Op::W32(0x50040000, 0x1)),
    s(Op::R32(0x50040000, 0x0)),
    s(Op::W32(0x50040008, 0x1000_0000)),
    s(Op::R32(0x50040008, 0x1000_0001)),
    s(Op::W32(0x50040008, 0x4)),
    s(Op::R32(0x50040008, 0x5)),
    s(Op::R32(0x50040000, 0xC)),
    s(Op::R32(0x50040008, 0x1)),
    s(Op::R32(0x50040040, 0x190)),
    s(Op::R32(0x50040000, 0x0)),
    s(Op::W32(0x5004000C, 0x2000)),
    s(Op::W32(0x50040008, 0x1000_0005)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::R32(0x50040040, 0x190)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::R32(0x50040040, 0x190)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::W32(0x50040008, 0x10)),
    s(Op::R32(0x50040008, 0x1)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::R32(0x50040000, 0xD)),
    s(Op::W32(0x5004000C, 0x0)),
    s(Op::W32(0x50040008, 0x1000_0005)),
    s(Op::R16(0x50040000, 0xD)),
    s(Op::R16(0x50040040, 0x190)),
    s(Op::R16(0x50040042, 0x0)),
    s(Op::R32(0x50040000, 0x1)),
    s(Op::W32(0x50040008, 0x1000_0005)),
    s(Op::W32(0x50040008, 0x1000_0002)),
    s(Op::R32(0x50040008, 0x1000_0000)),
    s(Op::R32(0x50040000, 0x0)),
    s(Op::R32(0x50040000, 0x0)),
    s(Op::W32(0x50040008, 0x1000_0004)),
    s(Op::R32(0x50040008, 0x1000_0004)),
    s(Op::R32(0x50040000, 0x0)),
    s(Op::W32(0x50040030, 0xDEAD_BEEF)),
    s(Op::R16(0x50040030, 0xBEEF)),
    s(Op::R16(0x50040032, 0xDEAD)),
    s(Op::R16(0x50040031, 0xBEEF)),
    s(Op::R16(0x50040033, 0xDEAD)),
    s(Op::W16(0x50040030, 0xAAAA)),
    s(Op::W16(0x50040032, 0xBBBB)),
    s(Op::R32(0x50040030, 0xBBBB_AAAA)),
    s(Op::W16(0x5004000E, 0x2)),
    s(Op::R32(0x5004000C, 0x2_0000)),
    s(Op::W16(0x5004000A, 0x8000)),
    s(Op::R32(0x50040008, 0x4)),
    sl(Op::W8(0x50040008, 0x1), &["W|adc: Attempted Byte write isn't supported by the peripheral. Offset 0x8, value 0x1."]),
    sl(Op::R8(0x50040040, 0x0), &["W|adc: Attempted Byte read isn't supported by the peripheral. Offset 0x40."]),
];
