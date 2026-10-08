// Differential replay of register-level sequences recorded from the pinned Renode 1.17.0.
//
// The tables below were recorded by running the unmodified `emulation/handset.repl` platform (without
// firmware: nothing executes and virtual time stays 0) in Renode 1.17.0 and driving it with monitor
// commands (`sysbus ReadDoubleWord/WriteWord/...`, `<peripheral> OnGPIO`, `<peripheral> Reset`). Reads
// carry the value Renode returned; `Irq` steps carry the level of an NVIC input as observed through the
// NVIC's `ISPR`/`ICPR` (clearing a pending interrupt is a no-op while its line is still high); `log` lists
// the WARNING (`W|`) and ERROR (`E|`) lines Renode printed for the step, with later repeats of the same
// message removed because these models log each message once. This is the evidence that the models
// reproduce Renode's register-level behaviour, quirks included; it says nothing about physical devices.
// Generated from the recorded data by a script; regenerate rather than edit by hand.


use emu_core::testing::Harness;
use emu_core::{LogEntry, LogLevel, PeriphId, Peripheral};
use stm32::combined_input::CombinedInput;
use stm32::crc::{Crc, Stm32Series};
use stm32::exti::Exti;
use stm32::gpio::{Gpio, GpioConfig};
use stm32::rng::Rng;

const GPIO_NAMES: [&str; 8] = ["gpioA", "gpioB", "gpioC", "gpioD", "gpioE", "gpioF", "gpioG", "gpioH"];
const CRC: u32 = 0x4002_3000;

/// The peripherals of `handset.repl` that the recorded sequences touch, wired like the platform file.
struct Platform {
    h: Harness,
    ids: Vec<(&'static str, PeriphId)>,
}

impl Platform {
    fn new() -> Self {
        let mut h = Harness::new();
        let mut ids = Vec::new();
        let exti = h.add_mapped(0x4001_0400, 0x400, Exti::new("exti", 24));
        let exti5to9 = h.add(CombinedInput::new("exti5to9", 5));
        let exti10to15 = h.add(CombinedInput::new("exti10to15", 6));
        ids.push(("exti", exti));
        for (i, name) in GPIO_NAMES.iter().enumerate() {
            let config = match i {
                0 => GpioConfig::default().with_mode_reset(GpioConfig::PORT_A_MODE_RESET),
                1 => GpioConfig::default().with_mode_reset(GpioConfig::PORT_B_MODE_RESET),
                _ => GpioConfig::default(),
            };
            ids.push((*name, h.add_mapped(0x4800_0000 + 0x400 * i as u32, 0x400, Gpio::new(*name, config))));
        }
        let crc = h.add_mapped(CRC, 0x400, Crc::new("crc", Stm32Series::F0, true));
        let rng = h.add_mapped(0x5006_0800, 0x400, Rng::new("rng", Stm32Series::F7));
        ids.push(("crc", crc));
        ids.push(("rng", rng));
        // `[0-4] -> nvic@[6-10]`, `[5-9] -> exti5to9@[0-4]`, `[10-15] -> exti10to15@[0-5]`, `-> nvic@23 / @40`.
        for line in 0..=4 {
            h.connect_irq(exti, line, 6 + line);
        }
        for line in 5..=9 {
            h.connect_input(exti, line, exti5to9, line - 5);
        }
        for line in 10..=15 {
            h.connect_input(exti, line, exti10to15, line - 10);
        }
        h.connect_irq(exti5to9, 0, 23);
        h.connect_irq(exti10to15, 0, 40);
        // `gpioC: 1 -> exti@1`, `rng -> nvic@80`.
        let gpio_c = ids.iter().find(|(n, _)| *n == "gpioC").unwrap().1;
        h.connect_input(gpio_c, 1, exti, 1);
        h.connect_irq(rng, 0, 80);
        h.clear_irq_changes();
        Self { h, ids }
    }

    fn id(&self, name: &str) -> PeriphId {
        self.ids.iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no peripheral {name}")).1
    }

    fn reset(&mut self, name: &str) {
        let id = self.id(name);
        match name {
            "exti" => self.h.with::<Exti, _>(id, |m, ctx| m.reset(ctx)),
            "rng" => self.h.with::<Rng, _>(id, |m, ctx| m.reset(ctx)),
            "crc" => self.h.with::<Crc, _>(id, |m, ctx| m.reset(ctx)),
            gpio if GPIO_NAMES.contains(&gpio) => self.h.with::<Gpio, _>(id, |m, ctx| m.reset(ctx)),
            other => panic!("no reset defined for {other}"),
        }
    }
}

fn replay(p: &mut Platform, section: &str, steps: &[Step]) {
    for (i, step) in steps.iter().enumerate() {
        p.h.drain_log();
        let context = format!("{section} step {i} {:?}", step.op);
        let mut lenient = false;
        match step.op {
            Op::W32(a, v) => p.h.write32(a, v),
            Op::W16(a, v) => p.h.write16(a, v),
            Op::W8(a, v) => p.h.write8(a, v),
            Op::R32(a, e) => assert_eq!(p.h.read32(a), e, "{context}"),
            Op::R16(a, e) => assert_eq!(p.h.read16(a), e, "{context}"),
            Op::R8(a, e) => assert_eq!(p.h.read8(a), e, "{context}"),
            Op::R32Random(a) => {
                let v = p.h.read32(a);
                assert!(v < 0x7FFF_FFFF, "{context}: 0x{v:08X} is not a 31-bit random number");
            }
            Op::LenientRead(a) => {
                lenient = true;
                let _ = p.h.read32(a);
            }
            Op::LenientWrite(a, v) => {
                lenient = true;
                p.h.write32(a, v);
            }
            Op::Input(target, line, level) => {
                let id = p.id(target);
                p.h.set_input(id, line, level);
            }
            Op::Irq(n, expected) => assert_eq!(p.h.irq_level(n), expected, "{context}"),
            Op::Reset(target) => p.reset(target),
        }
        let produced: Vec<String> = p.h.drain_log().iter().filter_map(render).collect();
        if !lenient {
            assert_eq!(produced, step.log, "{context}: new warning/error lines");
        }
    }
}


#[derive(Clone, Copy, Debug)]
enum Op {
    W32(u32, u32),
    W16(u32, u32),
    W8(u32, u32),
    R32(u32, u32),
    R16(u32, u32),
    R8(u32, u32),
    /// A 32-bit read of a random number: only checked to be a non-negative 31-bit value.
    R32Random(u32),
    /// Accesses for which Renode raised an exception: executed, nothing is compared.
    LenientRead(u32),
    LenientWrite(u32, u32),
    /// `<target> OnGPIO <line> <level>`.
    Input(&'static str, u32, bool),
    /// Level of NVIC input `n`.
    Irq(u32, bool),
    /// `<target> Reset`.
    Reset(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Step {
    op: Op,
    /// New log lines of the step: `W|<source>: <message>` or `E|...`.
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
    // Framework messages about unsupported widths already start with the peripheral name.
    let text = if entry.message.starts_with(&format!("{}: ", entry.source)) {
        entry.message.clone()
    } else {
        format!("{}: {}", entry.source, entry.message)
    };
    Some(format!("{level}|{text}"))
}


#[test]
fn gpio_registers_match_the_renode_recording() {
    let mut p = Platform::new();
    replay(&mut p, "gpio", GPIO);
}

#[test]
fn exti_matches_the_renode_recording() {
    let mut p = Platform::new();
    replay(&mut p, "exti", EXTI);
}

#[test]
fn crc_matches_the_renode_recording() {
    let mut p = Platform::new();
    replay(&mut p, "crc prefix", CRC_PREFIX);
    // Every POLYSIZE / polynomial / REV_IN / REV_OUT combination, fed with the same mixed-width data set.
    for (i, &(size, poly, init, rev_in, rev_out, initial, last)) in CRC_CONFIGS.iter().enumerate() {
        let context = format!("crc config {i}: size {size} poly 0x{poly:X} init 0x{init:X} rev_in {rev_in} rev_out {rev_out}");
        p.h.drain_log();
        p.h.write32(CRC + 0x14, poly);
        p.h.write32(CRC + 0x10, init);
        p.h.write32(CRC + 0x08, 1 | size << 3 | rev_in << 5 | rev_out << 7);
        assert_eq!(p.h.read32(CRC), initial, "{context}: after configuration");
        p.h.write32(CRC, 0x1234_5678);
        p.h.write16(CRC, 0x9ABC);
        p.h.write8(CRC, 0xDE);
        p.h.write32(CRC, 0xF0E1_D2C3);
        p.h.write8(CRC, 0x01);
        p.h.write16(CRC, 0x0203);
        assert_eq!(p.h.read32(CRC), last, "{context}: after the data set");
        assert!(p.h.drain_log().iter().filter_map(render).next().is_none(), "{context}: unexpected log lines");
    }
    replay(&mut p, "crc tail", CRC_TAIL);
}

#[test]
fn rng_matches_the_renode_recording() {
    let mut p = Platform::new();
    replay(&mut p, "rng", RNG);
}

#[test]
fn peripheral_resets_match_the_renode_recording() {
    let mut p = Platform::new();
    replay(&mut p, "reset", RESET);
}

// ---- recorded sequences ----

const GPIO: &[Step] = &[
    s(Op::W32(0x48000400, 0x0)),
    s(Op::R32(0x48000400, 0xFFFF_FEBF)),
    s(Op::W32(0x48000400, 0x0)),
    s(Op::R32(0x48000400, 0x0)),
    s(Op::R32(0x48000000, 0xABFF_FFFF)),
    s(Op::W32(0x48000000, 0x0)),
    s(Op::R32(0x48000000, 0x0)),
    s(Op::R32(0x48000000, 0x0)),
    s(Op::R32(0x48000004, 0x0)),
    s(Op::R32(0x48000008, 0x0)),
    s(Op::R32(0x4800000C, 0x0)),
    s(Op::R32(0x48000010, 0x0)),
    s(Op::R32(0x48000014, 0x0)),
    s(Op::R32(0x48000018, 0x0)),
    s(Op::R32(0x4800001C, 0x0)),
    s(Op::R32(0x48000020, 0x0)),
    s(Op::R32(0x48000024, 0x0)),
    s(Op::R32(0x48000028, 0x0)),
    sl(Op::R32(0x4800002C, 0x0), &["W|gpioA: Unhandled read from offset 0x2C."]),
    sl(Op::R32(0x48000030, 0x0), &["W|gpioA: Unhandled read from offset 0x30."]),
    s(Op::R32(0x48001400, 0x0)),
    s(Op::R32(0x48001404, 0x0)),
    s(Op::R32(0x48001408, 0x0)),
    s(Op::R32(0x4800140C, 0x0)),
    s(Op::R32(0x48001410, 0x0)),
    s(Op::R32(0x48001414, 0x0)),
    s(Op::R32(0x48001418, 0x0)),
    s(Op::R32(0x4800141C, 0x0)),
    s(Op::R32(0x48001420, 0x0)),
    s(Op::R32(0x48001424, 0x0)),
    s(Op::R32(0x48001428, 0x0)),
    sl(Op::R32(0x4800142C, 0x0), &["W|gpioF: Unhandled read from offset 0x2C."]),
    sl(Op::R32(0x48001430, 0x0), &["W|gpioF: Unhandled read from offset 0x30."]),
    s(Op::W32(0x48001414, 0x8005)),
    s(Op::R32(0x48001414, 0x8005)),
    s(Op::R32(0x48001410, 0x8005)),
    s(Op::W32(0x48001414, 0x4)),
    s(Op::R32(0x48001414, 0x4)),
    s(Op::W32(0x48001418, 0xA)),
    s(Op::R32(0x48001414, 0xE)),
    s(Op::W32(0x48001418, 0x2_0000)),
    s(Op::R32(0x48001414, 0xC)),
    s(Op::W32(0x48001428, 0xF)),
    s(Op::R32(0x48001414, 0x0)),
    s(Op::W32(0x48001418, 0x8_0008)),
    s(Op::R32(0x48001414, 0x0)),
    s(Op::R32(0x48001418, 0x0)),
    s(Op::R32(0x48001428, 0x0)),
    sl(Op::W32(0x48001414, 0x1_0003), &["W|gpioF: Unhandled write to offset 0x14. Unhandled bits: [16] when writing value 0x10003. Tags: RESERVED (0x1)."]),
    s(Op::R32(0x48001414, 0x3)),
    s(Op::W32(0x48001410, 0xFFFC)),
    s(Op::R32(0x48001410, 0x3)),
    sl(Op::W32(0x48001410, 0x8000_0000), &["W|gpioF: Unhandled write to offset 0x10. Unhandled bits: [31] when writing value 0x80000000. Tags: RESERVED (0x8000)."]),
    sl(Op::W32(0x48001428, 0x1_0010), &["W|gpioF: Unhandled write to offset 0x28. Unhandled bits: [16] when writing value 0x10010. Tags: RESERVED (0x1)."]),
    s(Op::R32(0x48001414, 0x3)),
    s(Op::Input("gpioF", 3, true)),
    s(Op::R32(0x48001410, 0xB)),
    s(Op::R32(0x48001414, 0xB)),
    s(Op::Input("gpioF", 3, false)),
    s(Op::R32(0x48001410, 0x3)),
    sl(Op::W32(0x48001404, 0x40), &["W|gpioF: Unhandled write to offset 0x4. Unhandled bits: [6] when writing value 0x40. Tags: OT6 (0x1)."]),
    s(Op::R32(0x48001404, 0x0)),
    sl(Op::W32(0x48001404, 0x80), &["W|gpioF: Unhandled write to offset 0x4. Unhandled bits: [7] when writing value 0x80. Tags: OT7 (0x1)."]),
    sl(Op::W32(0x48001404, 0x400), &["W|gpioF: Unhandled write to offset 0x4. Unhandled bits: [10] when writing value 0x400. Tags: OT10 (0x1)."]),
    sl(Op::W32(0x48001404, 0x1_0309), &["W|gpioF: Unhandled write to offset 0x4. Unhandled bits: [0, 3, 8-9, 16] when writing value 0x10309. Tags: OT0 (0x1), OT3 (0x1), OT8 (0x1), OT9 (0x1), RESERVED (0x1)."]),
    s(Op::R32(0x48001404, 0x0)),
    s(Op::R32(0x48001400, 0x0)),
    s(Op::W32(0x48001400, 0x2800_0501)),
    s(Op::R32(0x48001400, 0x2800_0501)),
    s(Op::R32(0x48001408, 0x0)),
    s(Op::W32(0x48001408, 0xF0F0_0FFF)),
    s(Op::R32(0x48001408, 0xF0F0_0FFF)),
    s(Op::R32(0x4800140C, 0x0)),
    s(Op::W32(0x4800140C, 0x6400_0000)),
    s(Op::R32(0x4800140C, 0x6400_0000)),
    s(Op::R32(0x48001420, 0x0)),
    s(Op::W32(0x48001420, 0x7654_3210)),
    s(Op::R32(0x48001420, 0x7654_3210)),
    s(Op::R32(0x48001424, 0x0)),
    s(Op::W32(0x48001424, 0xFEDC_BA98)),
    s(Op::R32(0x48001424, 0xFEDC_BA98)),
    s(Op::R32(0x4800142C, 0x0)),
    sl(Op::W32(0x4800142C, 0x1), &["W|gpioF: Unhandled write to offset 0x2C, value 0x1."]),
    s(Op::R32(0x48001430, 0x0)),
    sl(Op::R32(0x480017FC, 0x0), &["W|gpioF: Unhandled read from offset 0x3FC."]),
    s(Op::W16(0x48001414, 0xA5)),
    s(Op::R32(0x48001414, 0xA5)),
    s(Op::R16(0x48001414, 0xA5)),
    s(Op::R16(0x48001416, 0x0)),
    s(Op::R16(0x48001415, 0x0)),
    s(Op::W16(0x4800141A, 0x1)),
    s(Op::R32(0x48001414, 0xA4)),
    sl(Op::W8(0x48001414, 0xFF), &["W|gpioF: Attempted Byte write isn't supported by the peripheral. Offset 0x14, value 0xFF."]),
    sl(Op::R8(0x48001414, 0x0), &["W|gpioF: Attempted Byte read isn't supported by the peripheral. Offset 0x14."]),
    sl(Op::R8(0x48001410, 0x0), &["W|gpioF: Attempted Byte read isn't supported by the peripheral. Offset 0x10."]),
    s(Op::R16(0x48001417, 0x0)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x4)),
    s(Op::R32(0x4800181C, 0x3)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x3)),
    s(Op::W32(0x4800181C, 0x3)),
    s(Op::R32(0x4800181C, 0x3)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x3)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x48001800, 0x0)),
    sl(Op::W32(0x48001800, 0x15), &["W|gpioG: Ignoring attempt to change MODER configuration of the locked pin #0", "W|gpioG: Ignoring attempt to change MODER configuration of the locked pin #1"]),
    s(Op::R32(0x48001800, 0x10)),
    s(Op::R32(0x48001808, 0x0)),
    sl(Op::W32(0x48001808, 0x3F), &["W|gpioG: Ignoring attempt to change OSPEEDR configuration of the locked pin #0", "W|gpioG: Ignoring attempt to change OSPEEDR configuration of the locked pin #1"]),
    s(Op::R32(0x48001808, 0x30)),
    s(Op::R32(0x4800180C, 0x0)),
    sl(Op::W32(0x4800180C, 0x2A), &["W|gpioG: Ignoring attempt to change PUPDR0 configuration of the locked pin #0", "W|gpioG: Ignoring attempt to change PUPDR0 configuration of the locked pin #1"]),
    s(Op::R32(0x4800180C, 0x20)),
    s(Op::R32(0x48001820, 0x0)),
    sl(Op::W32(0x48001820, 0x555), &["W|gpioG: Ignoring attempt to change AFSEL configuration of the locked pin #0", "W|gpioG: Ignoring attempt to change AFSEL configuration of the locked pin #1"]),
    s(Op::R32(0x48001820, 0x500)),
    s(Op::R32(0x48001824, 0x0)),
    s(Op::W32(0x48001824, 0x5)),
    s(Op::R32(0x48001824, 0x5)),
    s(Op::W32(0x4800181C, 0x7)),
    s(Op::R32(0x4800181C, 0x3)),
    s(Op::W32(0x48001C1C, 0x1_0003)),
    s(Op::W32(0x48001C1C, 0x3)),
    s(Op::W32(0x48001C1C, 0x1_0003)),
    s(Op::W32(0x48001C1C, 0x1_0003)),
    s(Op::R32(0x48001C1C, 0x1_0003)),
    s(Op::R32(0x48001C1C, 0x1_0003)),
    s(Op::R32(0x48001C00, 0x0)),
    s(Op::W32(0x48001C00, 0x15)),
    s(Op::R32(0x48001C00, 0x15)),
    sl(Op::W32(0x48001C1C, 0x2_0000), &["W|gpioH: Unhandled write to offset 0x1C. Unhandled bits: [17] when writing value 0x20000. Tags: RESERVED (0x1)."]),
];

const EXTI: &[Step] = &[
    s(Op::R32(0x40010400, 0x0)),
    s(Op::R32(0x40010404, 0x0)),
    s(Op::R32(0x40010408, 0x0)),
    s(Op::R32(0x4001040C, 0x0)),
    s(Op::R32(0x40010410, 0x0)),
    s(Op::R32(0x40010414, 0x0)),
    sl(Op::R32(0x40010418, 0x0), &["W|exti: Unhandled read from offset 0x18 (PendingRegister+0x4)."]),
    s(Op::W32(0x40010400, 0xFFFF_FFFF)),
    s(Op::R32(0x40010400, 0xFFFF_FFFF)),
    s(Op::W32(0x40010404, 0x1234_5678)),
    s(Op::R32(0x40010404, 0x1234_5678)),
    s(Op::W32(0x40010408, 0xFFFF_FFFF)),
    s(Op::R32(0x40010408, 0xFFFF_FFFF)),
    s(Op::W32(0x4001040C, 0xDEAD_BEEF)),
    s(Op::R32(0x4001040C, 0xDEAD_BEEF)),
    s(Op::W32(0x40010400, 0x0)),
    s(Op::W32(0x40010404, 0x0)),
    s(Op::W32(0x40010408, 0x0)),
    s(Op::W32(0x4001040C, 0x0)),
    s(Op::W32(0x40010400, 0x20)),
    s(Op::W32(0x40010408, 0x20)),
    s(Op::Input("exti", 5, true)),
    s(Op::R32(0x40010414, 0x20)),
    s(Op::Irq(23, true)),
    s(Op::Input("exti", 5, false)),
    s(Op::Irq(23, true)),
    s(Op::R32(0x40010414, 0x20)),
    s(Op::W32(0x40010414, 0x20)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(23, false)),
    s(Op::W32(0x40010400, 0x2)),
    s(Op::W32(0x40010408, 0x0)),
    s(Op::W32(0x4001040C, 0x2)),
    s(Op::Input("exti", 1, true)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(7, false)),
    s(Op::Input("exti", 1, false)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40010408, 0x2)),
    s(Op::Input("exti", 1, true)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::Input("exti", 1, false)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40010400, 0x0)),
    s(Op::Input("exti", 1, true)),
    s(Op::Input("exti", 1, false)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40010400, 0x2)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Input("exti", 1, true)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40010400, 0x8)),
    s(Op::W32(0x40010408, 0x0)),
    s(Op::W32(0x4001040C, 0x0)),
    s(Op::Input("exti", 3, true)),
    s(Op::Input("exti", 3, false)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(9, false)),
    s(Op::W32(0x40010408, 0x0)),
    s(Op::W32(0x4001040C, 0x0)),
    s(Op::W32(0x40010400, 0x110)),
    s(Op::W32(0x40010410, 0x150)),
    s(Op::R32(0x40010410, 0x0)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(10, true)),
    s(Op::Irq(23, true)),
    s(Op::W32(0x40010414, 0x110)),
    s(Op::Irq(10, false)),
    s(Op::Irq(23, false)),
    s(Op::W32(0x40010400, 0x10)),
    s(Op::W32(0x40010410, 0x10)),
    s(Op::Irq(10, true)),
    s(Op::W32(0x40010414, 0x10)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(10, false)),
    s(Op::W32(0x40010400, 0x0)),
    s(Op::Input("exti", 23, true)),
    s(Op::R32(0x40010414, 0x80_0000)),
    s(Op::Input("exti", 23, false)),
    s(Op::R32(0x40010414, 0x0)),
    sl(Op::Input("exti", 24, true), &["E|exti: GPIO number 24 is out of range [0; 24)"]),
    sl(Op::Input("exti", 40, true), &["E|exti: GPIO number 40 is out of range [0; 24)"]),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::W32(0x40010400, 0xFFFF)),
    s(Op::W32(0x40010408, 0xFFFF)),
    s(Op::Input("exti", 5, true)),
    s(Op::Input("exti", 7, true)),
    s(Op::Irq(23, true)),
    s(Op::W32(0x40010414, 0x20)),
    s(Op::Irq(23, true)),
    s(Op::W32(0x40010414, 0x80)),
    s(Op::Irq(23, false)),
    s(Op::Input("exti", 5, false)),
    s(Op::Input("exti", 7, false)),
    s(Op::Input("exti", 10, true)),
    s(Op::Input("exti", 15, true)),
    s(Op::Irq(40, true)),
    s(Op::W32(0x40010414, 0x400)),
    s(Op::Irq(40, true)),
    s(Op::W32(0x40010414, 0x8000)),
    s(Op::Irq(40, false)),
    s(Op::Input("exti", 0, true)),
    s(Op::Irq(6, true)),
    s(Op::Input("exti", 4, true)),
    s(Op::Irq(10, true)),
    s(Op::Irq(6, true)),
    s(Op::W32(0x40010414, 0x11)),
    s(Op::Irq(6, false)),
    s(Op::Irq(10, false)),
    s(Op::R32(0x40010418, 0x0)),
    sl(Op::W32(0x40010418, 0x5), &["W|exti: Unhandled write to offset 0x18 (PendingRegister+0x4), value 0x5."]),
    sl(Op::R32(0x400107FC, 0x0), &["W|exti: Unhandled read from offset 0x3FC (PendingRegister+0x3e8)."]),
    sl(Op::W16(0x40010400, 0xFFFF), &["W|exti: Attempted Word write isn't supported by the peripheral. Offset 0x0, value 0xFFFF."]),
    sl(Op::W8(0x40010400, 0xFF), &["W|exti: Attempted Byte write isn't supported by the peripheral. Offset 0x0, value 0xFF."]),
    sl(Op::R16(0x40010400, 0x0), &["W|exti: Attempted Word read isn't supported by the peripheral. Offset 0x0."]),
    sl(Op::R8(0x40010400, 0x0), &["W|exti: Attempted Byte read isn't supported by the peripheral. Offset 0x0."]),
    s(Op::R32(0x40010400, 0xFFFF)),
    s(Op::W32(0x40010414, 0xFFFF_FFFF)),
    s(Op::W32(0x40010400, 0x2)),
    s(Op::W32(0x40010408, 0x0)),
    s(Op::W32(0x4001040C, 0x2)),
    s(Op::W32(0x48000814, 0x2)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x48000818, 0x2_0000)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x48000818, 0x2_0002)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::Input("gpioC", 1, true)),
    s(Op::R32(0x48000810, 0x2)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Input("gpioC", 1, false)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40010400, 0xFFFF)),
    s(Op::W32(0x40010408, 0xFFFF)),
    s(Op::Input("exti", 2, true)),
    s(Op::Input("exti", 6, true)),
    s(Op::Irq(8, true)),
    s(Op::Irq(23, true)),
    s(Op::Reset("exti")),
    s(Op::Irq(8, false)),
    s(Op::Irq(23, false)),
    s(Op::R32(0x40010400, 0x0)),
    s(Op::R32(0x40010414, 0x0)),
];

const CRC_PREFIX: &[Step] = &[
    s(Op::R32(0x40023000, 0xFFFF_FFFF)),
    s(Op::R32(0x40023004, 0x0)),
    s(Op::R32(0x40023008, 0x0)),
    sl(Op::R32(0x4002300C, 0x0), &["W|crc: Unhandled read from offset 0xC."]),
    s(Op::R32(0x40023010, 0xFFFF_FFFF)),
    s(Op::R32(0x40023014, 0x4C1_1DB7)),
    sl(Op::R32(0x40023018, 0x0), &["W|crc: Unhandled read from offset 0x18."]),
    s(Op::R8(0x40023000, 0xFF)),
    s(Op::R16(0x40023000, 0xFFFF)),
    s(Op::R16(0x40023008, 0x0)),
    s(Op::R8(0x40023010, 0xFF)),
    s(Op::W32(0x40023000, 0x1234_5678)),
    s(Op::R32(0x40023000, 0xDF8A_8A2B)),
    s(Op::W32(0x40023008, 0x1)),
    s(Op::R32(0x40023008, 0x0)),
    s(Op::R32(0x40023000, 0xFFFF_FFFF)),
    s(Op::W32(0x40023000, 0x0)),
    s(Op::R32(0x40023000, 0xC704_DD7B)),
    s(Op::W32(0x40023008, 0x0)),
    s(Op::R32(0x40023000, 0xFFFF_FFFF)),
    s(Op::W8(0x40023000, 0x12)),
    s(Op::W8(0x40023000, 0x34)),
    s(Op::W8(0x40023000, 0x56)),
    s(Op::W8(0x40023000, 0x78)),
    s(Op::R32(0x40023000, 0xDF8A_8A2B)),
    s(Op::W32(0x40023008, 0x1)),
    s(Op::W16(0x40023000, 0x1234)),
    s(Op::W16(0x40023000, 0x5678)),
    s(Op::R32(0x40023000, 0xDF8A_8A2B)),
    s(Op::R8(0x40023000, 0x2B)),
    s(Op::R16(0x40023000, 0x8A2B)),
    sl(Op::R8(0x40023001, 0x0), &["W|crc: Unhandled read from offset 0x1."]),
    sl(Op::W8(0x40023001, 0xFF), &["W|crc: Unhandled write to offset 0x1, value 0xFF."]),
    sl(Op::W16(0x40023010, 0x1234), &["W|crc: Unhandled write to offset 0x10, value 0x1234."]),
    s(Op::R32(0x40023010, 0xFFFF_FFFF)),
    s(Op::W8(0x40023010, 0x1)),
    s(Op::R32(0x40023010, 0xFFFF_FFFF)),
    s(Op::W32(0x40023008, 0x1)),
    s(Op::W32(0x40023000, 0x0)),
    s(Op::R32(0x40023000, 0xC704_DD7B)),
    s(Op::W32(0x40023010, 0x0)),
    s(Op::R32(0x40023000, 0xC704_DD7B)),
    s(Op::W32(0x40023008, 0x1)),
    s(Op::R32(0x40023000, 0x0)),
    s(Op::W32(0x40023010, 0xFFFF_FFFF)),
    s(Op::W32(0x40023014, 0x4C1_1DB7)),
    s(Op::R32(0x40023000, 0xFFFF_FFFF)),
    sl(Op::W32(0x40023004, 0xA5), &["W|crc: Unhandled write to offset 0x4. Unhandled bits: [0, 2, 5, 7] when writing value 0xA5. Tags: CRC_IDR (0xA5)."]),
    s(Op::R32(0x40023004, 0x0)),
    sl(Op::W32(0x40023008, 0x106), &["W|crc: Unhandled write to offset 0x8. Unhandled bits: [1-2, 8] when writing value 0x106. Tags: RESERVED (0x3), RESERVED (0x1)."]),
    s(Op::R32(0x40023008, 0x0)),
    s(Op::W32(0x40023008, 0x0)),
];

/// `(POLYSIZE, POL, INIT, REV_IN, REV_OUT, DR after the configuration, DR after the data set)`.
const CRC_CONFIGS: &[(u32, u32, u32, u32, u32, u32, u32)] = &[
    (0, 0x4C1_1DB7, 0x5A5A_5A5A, 0, 0, 0x5A5A_5A5A, 0x3C57_7C0D),
    (0, 0x4C1_1DB7, 0xFFFF_FFFF, 0, 1, 0xFFFF_FFFF, 0x14CE_1B98),
    (0, 0x4C1_1DB7, 0x5A5A_5A5A, 1, 0, 0x5A5A_5A5A, 0x98B3_E84D),
    (0, 0x4C1_1DB7, 0xFFFF_FFFF, 1, 1, 0xFFFF_FFFF, 0x16E7_3CBD),
    (0, 0x4C1_1DB7, 0x5A5A_5A5A, 2, 0, 0x5A5A_5A5A, 0xB1E1_4E0B),
    (0, 0x4C1_1DB7, 0xFFFF_FFFF, 2, 1, 0xFFFF_FFFF, 0x7482_7629),
    (0, 0x4C1_1DB7, 0x5A5A_5A5A, 3, 0, 0x5A5A_5A5A, 0x94B8_716C),
    (0, 0x4C1_1DB7, 0xFFFF_FFFF, 3, 1, 0xFFFF_FFFF, 0x927E_EC8D),
    (0, 0x1EDC_6F41, 0x5A5A_5A5A, 0, 0, 0x5A5A_5A5A, 0x23C9_0B5C),
    (0, 0x1EDC_6F41, 0xFFFF_FFFF, 0, 1, 0xFFFF_FFFF, 0x7A5D_74F5),
    (0, 0x1EDC_6F41, 0x5A5A_5A5A, 1, 0, 0x5A5A_5A5A, 0x48D0_6153),
    (0, 0x1EDC_6F41, 0xFFFF_FFFF, 1, 1, 0xFFFF_FFFF, 0x8A0B_EC23),
    (0, 0x1EDC_6F41, 0x5A5A_5A5A, 2, 0, 0x5A5A_5A5A, 0xE546_3511),
    (0, 0x1EDC_6F41, 0xFFFF_FFFF, 2, 1, 0xFFFF_FFFF, 0xC821_8596),
    (0, 0x1EDC_6F41, 0x5A5A_5A5A, 3, 0, 0x5A5A_5A5A, 0x9166_5D72),
    (0, 0x1EDC_6F41, 0xFFFF_FFFF, 3, 1, 0xFFFF_FFFF, 0xE37_81B8),
    (1, 0x1021, 0x5A5A_5A5A, 0, 0, 0x5A5A, 0xA270),
    (1, 0x1021, 0xFFFF_FFFF, 0, 1, 0xFFFF, 0xC06C),
    (1, 0x1021, 0x5A5A_5A5A, 1, 0, 0x5A5A, 0xD51A),
    (1, 0x1021, 0xFFFF_FFFF, 1, 1, 0xFFFF, 0x9682),
    (1, 0x1021, 0x5A5A_5A5A, 2, 0, 0x5A5A, 0x1EA3),
    (1, 0x1021, 0xFFFF_FFFF, 2, 1, 0xFFFF, 0xB51),
    (1, 0x1021, 0x5A5A_5A5A, 3, 0, 0x5A5A, 0xDBC),
    (1, 0x1021, 0xFFFF_FFFF, 3, 1, 0xFFFF, 0xF399),
    (1, 0x8005, 0x5A5A_5A5A, 0, 0, 0x5A5A, 0x319E),
    (1, 0x8005, 0xFFFF_FFFF, 0, 1, 0xFFFF, 0x78A5),
    (1, 0x8005, 0x5A5A_5A5A, 1, 0, 0x5A5A, 0x7430),
    (1, 0x8005, 0xFFFF_FFFF, 1, 1, 0xFFFF, 0xD07),
    (1, 0x8005, 0x5A5A_5A5A, 2, 0, 0x5A5A, 0x7EC),
    (1, 0x8005, 0xFFFF_FFFF, 2, 1, 0xFFFF, 0x36C9),
    (1, 0x8005, 0x5A5A_5A5A, 3, 0, 0x5A5A, 0x5F7B),
    (1, 0x8005, 0xFFFF_FFFF, 3, 1, 0xFFFF, 0xDFD3),
    (2, 0x7, 0x5A5A_5A5A, 0, 0, 0x5A, 0xE2),
    (2, 0x7, 0xFFFF_FFFF, 0, 1, 0xFF, 0xED),
    (2, 0x7, 0x5A5A_5A5A, 1, 0, 0x5A, 0x33),
    (2, 0x7, 0xFFFF_FFFF, 1, 1, 0xFF, 0x66),
    (2, 0x7, 0x5A5A_5A5A, 2, 0, 0x5A, 0x90),
    (2, 0x7, 0xFFFF_FFFF, 2, 1, 0xFF, 0xA3),
    (2, 0x7, 0x5A5A_5A5A, 3, 0, 0x5A, 0x53),
    (2, 0x7, 0xFFFF_FFFF, 3, 1, 0xFF, 0x60),
    (2, 0x31, 0x5A5A_5A5A, 0, 0, 0x5A, 0xD1),
    (2, 0x31, 0xFFFF_FFFF, 0, 1, 0xFF, 0x35),
    (2, 0x31, 0x5A5A_5A5A, 1, 0, 0x5A, 0x17),
    (2, 0x31, 0xFFFF_FFFF, 1, 1, 0xFF, 0x56),
    (2, 0x31, 0x5A5A_5A5A, 2, 0, 0x5A, 0x9F),
    (2, 0x31, 0xFFFF_FFFF, 2, 1, 0xFF, 0x47),
    (2, 0x31, 0x5A5A_5A5A, 3, 0, 0x5A, 0xE2),
    (2, 0x31, 0xFFFF_FFFF, 3, 1, 0xFF, 0xF9),
    (3, 0x9, 0x5A5A_5A5A, 0, 0, 0x5A, 0x59),
    (3, 0x9, 0xFFFF_FFFF, 0, 1, 0x7F, 0x7E),
    (3, 0x9, 0x5A5A_5A5A, 1, 0, 0x5A, 0x7A),
    (3, 0x9, 0xFFFF_FFFF, 1, 1, 0x7F, 0x1C),
    (3, 0x9, 0x5A5A_5A5A, 2, 0, 0x5A, 0x79),
    (3, 0x9, 0xFFFF_FFFF, 2, 1, 0x7F, 0x7C),
    (3, 0x9, 0x5A5A_5A5A, 3, 0, 0x5A, 0xA),
    (3, 0x9, 0xFFFF_FFFF, 3, 1, 0x7F, 0x1B),
    (3, 0x45, 0x5A5A_5A5A, 0, 0, 0x5A, 0x44),
    (3, 0x45, 0xFFFF_FFFF, 0, 1, 0x7F, 0x23),
    (3, 0x45, 0x5A5A_5A5A, 1, 0, 0x5A, 0x18),
    (3, 0x45, 0xFFFF_FFFF, 1, 1, 0x7F, 0x3E),
    (3, 0x45, 0x5A5A_5A5A, 2, 0, 0x5A, 0x11),
    (3, 0x45, 0xFFFF_FFFF, 2, 1, 0x7F, 0x76),
    (3, 0x45, 0x5A5A_5A5A, 3, 0, 0x5A, 0x44),
    (3, 0x45, 0xFFFF_FFFF, 3, 1, 0x7F, 0x23),
];

const CRC_TAIL: &[Step] = &[
    s(Op::W32(0x40023014, 0x4C1_1DB7)),
    s(Op::W32(0x40023010, 0xFFFF_FFFF)),
    s(Op::W32(0x40023008, 0x9)),
    s(Op::LenientRead(0x40023000)),
    s(Op::LenientWrite(0x40023000, 0x1)),
    s(Op::LenientRead(0x40023000)),
    s(Op::W32(0x40023014, 0x1021)),
    s(Op::R32(0x40023000, 0xFFFF)),
    s(Op::W32(0x40023000, 0x0)),
    s(Op::R32(0x40023000, 0x84C0)),
];

const RNG: &[Step] = &[
    s(Op::R32(0x50060800, 0x0)),
    s(Op::R32(0x50060804, 0x0)),
    s(Op::R32(0x50060808, 0x0)),
    sl(Op::R32(0x5006080C, 0x0), &["W|rng: Unhandled read from offset 0xC."]),
    sl(Op::R32(0x50060810, 0x0), &["W|rng: Unhandled read from offset 0x10."]),
    s(Op::R32(0x50060808, 0x0)),
    s(Op::R32(0x50060808, 0x0)),
    s(Op::W32(0x50060800, 0x4)),
    s(Op::R32(0x50060800, 0x4)),
    s(Op::R32(0x50060804, 0x1)),
    s(Op::R32Random(0x50060808)),
    s(Op::R32Random(0x50060808)),
    s(Op::Irq(80, false)),
    s(Op::W32(0x50060800, 0xC)),
    s(Op::Irq(80, true)),
    s(Op::W32(0x50060800, 0x4)),
    s(Op::Irq(80, false)),
    s(Op::W32(0x50060800, 0x8)),
    s(Op::Irq(80, false)),
    s(Op::W32(0x50060800, 0xC)),
    s(Op::Irq(80, true)),
    s(Op::W32(0x50060800, 0x8)),
    s(Op::Irq(80, false)),
    sl(Op::W32(0x50060800, 0xF), &["W|rng: Unhandled write to offset 0x0. Unhandled bits: [0-1] when writing value 0xF. Tags: RESERVED (0x3)."]),
    s(Op::R32(0x50060800, 0xC)),
    sl(Op::W32(0x50060800, 0xFFFF_FFFC), &["W|rng: Unhandled write to offset 0x0. Unhandled bits: [4-31] when writing value 0xFFFFFFFC. Tags: RESERVED (0xFFFFFFF)."]),
    s(Op::R32(0x50060800, 0xC)),
    s(Op::W32(0x50060800, 0x0)),
    sl(Op::W32(0x50060804, 0xFFFF_FFDF), &["W|rng: Unhandled write to offset 0x4. Unhandled bits: [1-4, 6-31] when writing value 0xFFFFFFDF. Tags: CECS (0x1), SECS (0x1), RESERVED (0x3), SEIS (0x1), RESERVED (0x1FFFFFF)."]),
    s(Op::R32(0x50060804, 0x0)),
    s(Op::W32(0x50060804, 0x0)),
    s(Op::W32(0x50060808, 0xFFFF_FFFF)),
    s(Op::W32(0x50060800, 0xC)),
    s(Op::Irq(80, true)),
    s(Op::Reset("rng")),
    s(Op::R32(0x50060800, 0x0)),
    s(Op::R32(0x50060804, 0x0)),
    s(Op::R32(0x50060808, 0x0)),
    s(Op::Irq(80, true)),
    s(Op::W32(0x50060800, 0xC)),
    s(Op::Irq(80, true)),
    s(Op::W32(0x50060800, 0x0)),
    s(Op::Irq(80, false)),
    sl(Op::W16(0x50060800, 0x4), &["W|rng: Attempted Word write isn't supported by the peripheral. Offset 0x0, value 0x4."]),
    sl(Op::W8(0x50060804, 0x1), &["W|rng: Attempted Byte write isn't supported by the peripheral. Offset 0x4, value 0x1."]),
    sl(Op::R8(0x50060804, 0x0), &["W|rng: Attempted Byte read isn't supported by the peripheral. Offset 0x4."]),
    sl(Op::R16(0x50060800, 0x0), &["W|rng: Attempted Word read isn't supported by the peripheral. Offset 0x0."]),
];

const RESET: &[Step] = &[
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::W32(0x4800181C, 0x3)),
    s(Op::W32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x4800181C, 0x1_0003)),
    s(Op::R32(0x48001800, 0x0)),
    sl(Op::W32(0x48001800, 0x15), &["W|gpioG: Ignoring attempt to change MODER configuration of the locked pin #0", "W|gpioG: Ignoring attempt to change MODER configuration of the locked pin #1"]),
    s(Op::R32(0x48001800, 0x10)),
    s(Op::Reset("gpioG")),
    s(Op::R32(0x4800181C, 0x0)),
    s(Op::R32(0x48001800, 0x0)),
    s(Op::W32(0x48001800, 0x15)),
    s(Op::R32(0x48001800, 0x15)),
    s(Op::R32(0x48000000, 0xABFF_FFFF)),
    s(Op::W32(0x48000000, 0x0)),
    s(Op::W32(0x48000008, 0xFF)),
    s(Op::W32(0x48000014, 0x31)),
    s(Op::R32(0x48000014, 0x31)),
    s(Op::R32(0x48000000, 0x0)),
    s(Op::Reset("gpioA")),
    s(Op::R32(0x48000000, 0xABFF_FFFF)),
    s(Op::R32(0x48000008, 0x0)),
    s(Op::R32(0x48000014, 0x0)),
    s(Op::R32(0x48000010, 0x0)),
    s(Op::W32(0x40010400, 0x2)),
    s(Op::W32(0x4001040C, 0x2)),
    s(Op::W32(0x48000814, 0x2)),
    s(Op::R32(0x40010414, 0x0)),
    s(Op::Irq(7, false)),
    s(Op::Reset("gpioC")),
    s(Op::R32(0x48000814, 0x0)),
    s(Op::R32(0x40010414, 0x2)),
    s(Op::Irq(7, true)),
    s(Op::W32(0x40010414, 0x2)),
    s(Op::Irq(7, false)),
    s(Op::W32(0x40023014, 0x1021)),
    s(Op::W32(0x40023010, 0x1234)),
    s(Op::W32(0x40023008, 0xE9)),
    s(Op::W32(0x40023000, 0xDEAD_BEEF)),
    s(Op::R32(0x40023000, 0xE9A9)),
    s(Op::R32(0x40023008, 0xE8)),
    s(Op::Reset("crc")),
    s(Op::R32(0x40023008, 0x0)),
    s(Op::R32(0x40023010, 0xFFFF_FFFF)),
    s(Op::R32(0x40023014, 0x4C1_1DB7)),
    s(Op::R32(0x40023000, 0xFFFF_FFFF)),
    s(Op::W32(0x40023000, 0x1234_5678)),
    s(Op::R32(0x40023000, 0xDF8A_8A2B)),
];
