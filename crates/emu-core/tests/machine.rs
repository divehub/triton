//! Machine semantics through the public API: MMIO dispatch, access translation, signals,
//! events, DMA-style bus access and debugger access.

use emu_core::testing::{Harness, IrqChange};
use emu_core::*;
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

type Shared = Rc<RefCell<Vec<String>>>;

fn shared() -> Shared {
    Rc::new(RefCell::new(Vec::new()))
}

macro_rules! any_impl {
    () => {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    };
}

/// 32-bit register file that records every access it receives.
struct Regs {
    name: String,
    regs: [u32; 16],
    policy: AccessPolicy,
    log: Shared,
}

impl Regs {
    fn new(name: &str, policy: AccessPolicy, log: &Shared) -> Self {
        Regs { name: name.to_string(), regs: [0; 16], policy, log: log.clone() }
    }
}

impl Peripheral for Regs {
    fn name(&self) -> &str {
        &self.name
    }

    fn access_policy(&self) -> AccessPolicy {
        self.policy
    }

    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
        self.log.borrow_mut().push(format!("{}:r{}@{:x}@{}", self.name, width.bytes(), offset, ctx.now()));
        let word = self.regs[(offset as usize / 4) % 16];
        (word >> ((offset & 3) * 8)) & width.mask()
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.log.borrow_mut().push(format!("{}:w{}@{:x}={:x}@{}", self.name, width.bytes(), offset, value, ctx.now()));
        if width == Width::Word {
            self.regs[(offset as usize / 4) % 16] = value;
        } else {
            let shift = (offset & 3) * 8;
            let slot = &mut self.regs[(offset as usize / 4) % 16];
            *slot = (*slot & !(width.mask() << shift)) | (value << shift);
        }
    }

    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        let word = *self.regs.get(offset as usize / 4)?;
        Some((word >> ((offset & 3) * 8)) & width.mask())
    }

    fn poke(&mut self, offset: u32, width: Width, value: u32, _ctx: &mut Ctx<'_>) -> bool {
        let Some(slot) = self.regs.get_mut(offset as usize / 4) else { return false };
        let shift = (offset & 3) * 8;
        *slot = (*slot & !(width.mask() << shift)) | ((value & width.mask()) << shift);
        true
    }

    any_impl!();
}

// ---- MMIO map -----------------------------------------------------------

#[test]
fn offsets_are_relative_to_the_region_base_and_widths_pass_through() {
    let log = shared();
    let mut h = Harness::new();
    h.add_mapped(0x4000_0400, 0x400, Regs::new("a", AccessPolicy::EXACT, &log));
    h.add_mapped(0x4000_0800, 0x400, Regs::new("b", AccessPolicy::EXACT, &log));
    h.write(0x4000_0404, Width::Word, 0x1122_3344);
    h.write(0x4000_0803, Width::Byte, 0x55);
    assert_eq!(h.read(0x4000_0404, Width::Word), 0x1122_3344);
    assert_eq!(h.read(0x4000_0405, Width::Half), 0x2233);
    assert_eq!(h.read(0x4000_0803, Width::Byte), 0x55);
    assert_eq!(
        *log.borrow(),
        ["a:w4@4=11223344@0", "b:w1@3=55@0", "a:r4@4@0", "a:r2@5@0", "b:r1@3@0"]
    );
    // Region boundaries: first and last byte hit, neighbors miss.
    log.borrow_mut().clear();
    h.read8(0x4000_0400);
    h.read8(0x4000_07FF);
    assert_eq!(log.borrow().len(), 2);
    h.read8(0x4000_03FF);
    h.read8(0x4000_0C00);
    assert_eq!(log.borrow().len(), 2, "outside accesses must not reach the peripherals");
}

#[test]
fn region_with_odd_size_is_bounded_exactly() {
    // Like the LCD: size 0x20004 (not a multiple of the 256-byte table granule).
    let log = shared();
    let mut h = Harness::new();
    h.add_mapped(0x6000_0000, 0x2_0004, Regs::new("lcd", AccessPolicy::EXACT, &log));
    h.read16(0x6002_0002);
    assert_eq!(log.borrow().len(), 1);
    h.read16(0x6002_0004);
    h.read16(0x6002_00FE);
    assert_eq!(log.borrow().len(), 1, "same granule but beyond the region");
    assert_eq!(h.warnings().len(), 2, "two distinct unmapped addresses warned");
}

#[test]
fn unmapped_accesses_read_zero_and_warn_once_per_address() {
    let mut h = Harness::new();
    assert_eq!(h.read32(0x5000_0000), 0);
    assert_eq!(h.read32(0x5000_0000), 0);
    h.write32(0x5000_0000, 0x1234);
    h.write32(0x5000_0000, 0x1234);
    assert_eq!(h.read32(0x5000_0004), 0);
    let warnings = h.warnings();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings[0].contains("Read") && warnings[0].contains("0x50000000"), "{}", warnings[0]);
    assert!(warnings[1].contains("Write") && warnings[1].contains("0x1234"), "{}", warnings[1]);
    assert_eq!(h.core().stats.unmapped_reads, 3);
    assert_eq!(h.core().stats.unmapped_writes, 2);
}

#[test]
fn probing_a_huge_unmapped_range_stays_bounded() {
    let mut h = Harness::new();
    for i in 0..40_000u32 {
        assert_eq!(h.read32(0x7000_0000 + i * 4), 0);
    }
    // The warn-once memory is capped: 16384 distinct warnings, then a single overflow notice.
    assert_eq!(h.core().log.count(LogLevel::Warning), WarnSet::DEFAULT_LIMIT as u64 + 1);
    let last = h.core().log.entries().last().unwrap().message.clone();
    assert!(last.contains("warn-once table is full"), "{last}");
    assert_eq!(h.core().stats.unmapped_reads, 40_000);
}

#[test]
fn map_validation() {
    let log = shared();
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    let a = core.add_peripheral(Box::new(Regs::new("a", AccessPolicy::EXACT, &log)));
    let b = core.add_peripheral(Box::new(Regs::new("b", AccessPolicy::EXACT, &log)));
    assert_eq!(core.map(a, 0x4000_0400, 0x400), Ok(()));
    assert_eq!(core.map(b, 0x4000_0401, 0x10), Err(MapError::Misaligned(0x4000_0401)));
    assert_eq!(core.map(b, 0x4000_0000, 0), Err(MapError::ZeroSize));
    assert!(matches!(core.map(b, 0x4000_0700, 0x200), Err(MapError::Overlap { with, .. }) if with == "a"));
    assert!(matches!(core.map(b, 0x4000_0300, 0x200), Err(MapError::Overlap { .. })));
    assert_eq!(core.map(b, 0x4000_0800, 0x400), Ok(()));
    assert!(matches!(core.map(b, 0x2000_0000, 0x100), Err(MapError::OverlapsMemory { .. })));
    assert!(matches!(core.map(b, 0x0800_0000, 0x100), Err(MapError::OverlapsMemory { .. })));
    assert!(matches!(core.map(b, 0xFFFF_FF00, 0x200), Err(MapError::OutOfAddressSpace)));
    assert_eq!(core.map(PeriphId(99), 0x5000_0000, 0x100), Err(MapError::UnknownPeripheral(PeriphId(99))));
    // The last 256 bytes of the address space are mappable.
    assert_eq!(core.map(b, 0xFFFF_FF00, 0x100), Ok(()));
    let regions = core.mapped_regions();
    assert_eq!(regions.len(), 3);
    assert_eq!(regions[0], MappedRegion { name: "a".into(), periph: a, base: 0x4000_0400, size: 0x400 });
    assert_eq!(core.find("b"), Some(b));
    assert_eq!(core.find("zzz"), None);
    assert_eq!(core.name_of(a), "a");
}

#[test]
fn adjacent_256_byte_regions_do_not_collide() {
    // buttons 0x61000200 and telemetry 0x61000300 in the NGC handset.
    let log = shared();
    let mut h = Harness::new();
    h.add_mapped(0x6100_0200, 0x100, Regs::new("buttons", AccessPolicy::EXACT, &log));
    h.add_mapped(0x6100_0300, 0x100, Regs::new("telemetry", AccessPolicy::EXACT, &log));
    h.read32(0x6100_02FC);
    h.read32(0x6100_0300);
    assert_eq!(*log.borrow(), ["buttons:r4@fc@0", "telemetry:r4@0@0"]);
}

// ---- access translation ---------------------------------------------------

#[test]
fn translated_accesses_follow_renode_through_the_bus() {
    let log = shared();
    let mut h = Harness::new();
    // STM32_GPIOPort-like: dword only, halfword allowed.
    let gpio = AccessPolicy::WORD_ONLY.with_translations(Translations::HALF_TO_WORD);
    let id = h.add_mapped(0x4800_0000, 0x400, Regs::new("gpio", gpio, &log));
    h.get_mut::<Regs>(id).regs[1] = 0xAABB_CCDD;
    log.borrow_mut().clear();
    // Halfword read -> aligned word read.
    assert_eq!(h.read16(0x4800_0006), 0xAABB);
    assert_eq!(*log.borrow(), ["gpio:r4@4@0"]);
    log.borrow_mut().clear();
    // Halfword write -> read-modify-write (the read has side effects).
    h.write16(0x4800_0004, 0x1234);
    assert_eq!(*log.borrow(), ["gpio:r4@4@0", "gpio:w4@4=aabb1234@0"]);
    assert_eq!(h.get::<Regs>(id).regs[1], 0xAABB_1234);
    // Byte accesses are not allowed: peripheral untouched, read 0, one warning, write dropped.
    log.borrow_mut().clear();
    assert_eq!(h.read8(0x4800_0004), 0);
    h.write8(0x4800_0004, 0xFF);
    assert!(log.borrow().is_empty());
    assert_eq!(h.get::<Regs>(id).regs[1], 0xAABB_1234);
    let warnings = h.warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("Attempted Byte read isn't supported"), "{}", warnings[0]);
    assert!(warnings[1].contains("Attempted Byte write isn't supported") && warnings[1].contains("0xFF"), "{}", warnings[1]);
    // Repeating the same unsupported access does not repeat the warning.
    h.read8(0x4800_0004);
    assert_eq!(h.warnings().len(), 2);
    // Native word access keeps the original unaligned offset.
    log.borrow_mut().clear();
    h.read32(0x4800_0002);
    assert_eq!(*log.borrow(), ["gpio:r4@2@0"]);
}

#[test]
fn byte_writes_through_a_timer_like_policy() {
    let log = shared();
    let mut h = Harness::new();
    let timer = AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD);
    let id = h.add_mapped(0x4000_0400, 0x400, Regs::new("tim", timer, &log));
    h.write32(0x4000_0400, 0x1111_1111);
    h.write8(0x4000_0402, 0xEE);
    assert_eq!(h.get::<Regs>(id).regs[0], 0x11EE_1111);
    assert_eq!(h.read8(0x4000_0403), 0x11);
    assert_eq!(h.read16(0x4000_0402), 0x11EE);
}

// ---- signals -------------------------------------------------------------

/// Drives output lines on demand and forwards inputs to a script.
struct Pin {
    name: String,
    log: Shared,
    /// For each input line: outputs to drive in reaction `(out_line, level_follows_input)`.
    reactions: Vec<(u32, u32)>,
    inputs_seen: u32,
}

impl Pin {
    fn new(name: &str, log: &Shared) -> Self {
        Pin { name: name.into(), log: log.clone(), reactions: Vec::new(), inputs_seen: 0 }
    }

    fn reacting(mut self, in_line: u32, out_line: u32) -> Self {
        self.reactions.push((in_line, out_line));
        self
    }
}

impl Peripheral for Pin {
    fn name(&self) -> &str {
        &self.name
    }

    /// Write offset = output line, value = level.
    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        ctx.set_output(offset, value != 0);
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        u32::from(ctx.output(offset))
    }

    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        self.inputs_seen += 1;
        self.log.borrow_mut().push(format!("{}.in{}={}", self.name, line, u8::from(level)));
        let reactions: Vec<(u32, u32)> = self.reactions.iter().copied().filter(|r| r.0 == line).collect();
        for (_, out) in reactions {
            ctx.set_output(out, level);
        }
    }

    any_impl!();
}

#[test]
fn outputs_notify_only_on_level_change() {
    let log = shared();
    let mut h = Harness::new();
    let src = h.add_mapped(0x4000_0000, 0x100, Pin::new("src", &log));
    let dst = h.add(Pin::new("dst", &log));
    h.connect_input(src, 3, dst, 7);
    // Connect pushes the current (low) level immediately.
    assert_eq!(*log.borrow(), ["dst.in7=0"]);
    log.borrow_mut().clear();
    h.write32(0x4000_0003, 0); // already low: no delivery
    assert!(log.borrow().is_empty());
    h.write32(0x4000_0003, 1);
    h.write32(0x4000_0003, 1); // no change
    h.write32(0x4000_0003, 0);
    assert_eq!(*log.borrow(), ["dst.in7=1", "dst.in7=0"]);
    assert!(!h.core().output_level(src, 3));
    // A duplicate connection is ignored and does not push again.
    log.borrow_mut().clear();
    h.connect_input(src, 3, dst, 7);
    assert!(log.borrow().is_empty());
    // Connecting while high pushes high.
    h.write32(0x4000_0004, 1);
    h.connect_input(src, 4, dst, 1);
    assert_eq!(*log.borrow(), ["dst.in1=1"]);
}

#[test]
fn fan_out_reaches_irq_and_inputs_in_connection_order() {
    let log = shared();
    let mut h = Harness::new();
    let src = h.add_mapped(0x4000_0000, 0x100, Pin::new("src", &log));
    let a = h.add(Pin::new("a", &log));
    let b = h.add(Pin::new("b", &log));
    h.connect_input(src, 0, a, 0);
    h.connect_irq(src, 0, 9);
    h.connect_input(src, 0, b, 5);
    h.clear_irq_changes();
    log.borrow_mut().clear();
    h.write32(0x4000_0000, 1);
    assert_eq!(*log.borrow(), ["a.in0=1", "b.in5=1"]);
    assert!(h.irq_level(9));
    assert_eq!(h.irq_changes(), [IrqChange { time: 0, irq: 9, level: true }]);
    h.write32(0x4000_0000, 0);
    assert!(!h.irq_level(9));
    assert_eq!(h.irq_changes().len(), 2);
}

#[test]
fn pulses_deliver_both_edges_in_order() {
    let mut h = Harness::new();
    let log = shared();
    let src = h.add(Pin::new("src", &log));
    h.connect_irq(src, 0, 20);
    h.clear_irq_changes();
    // A set(true); set(false) pair inside one handler produces two queued changes.
    h.core_mut().drive_output(src, 0, true);
    h.core_mut().drive_output(src, 0, false);
    let mut seen = Vec::new();
    h.core_mut().drain_irq_changes(&mut |irq, level| seen.push((irq, level)));
    assert_eq!(seen, [(20, true), (20, false)]);
    assert!(h.core_mut().irq_changes().is_empty());
}

#[test]
fn deliveries_run_after_the_sender_returns_depth_first() {
    // S line0 -> A.0 and B.0 ; A reacts by driving its line 0 -> C.0
    let log = shared();
    let mut h = Harness::new();
    let s = h.add_mapped(0x4000_0000, 0x100, Pin::new("S", &log));
    let a = h.add(Pin::new("A", &log).reacting(0, 0));
    let b = h.add(Pin::new("B", &log));
    let c = h.add(Pin::new("C", &log));
    h.connect_input(s, 0, a, 0);
    h.connect_input(s, 0, b, 0);
    h.connect_input(a, 0, c, 0);
    log.borrow_mut().clear();
    h.write32(0x4000_0000, 1);
    // Depth first: C (A's consequence) runs before B (S's second target).
    assert_eq!(*log.borrow(), ["A.in0=1", "C.in0=1", "B.in0=1"]);
}

#[test]
fn ping_pong_between_two_peripherals_terminates() {
    struct Echo {
        name: &'static str,
        limit: u32,
        count: u32,
        armed: bool,
    }
    impl Peripheral for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            self.count
        }
        fn write(&mut self, _o: u32, _w: Width, v: u32, ctx: &mut Ctx<'_>) {
            ctx.set_output(0, v != 0);
        }
        fn on_input(&mut self, _line: u32, _level: bool, ctx: &mut Ctx<'_>) {
            if !self.armed {
                return; // ignore the connect-time level pushes
            }
            self.count += 1;
            if self.count < self.limit {
                ctx.toggle_output(0);
            }
        }
        any_impl!();
    }
    let mut h = Harness::new();
    let a = h.add_mapped(0x4000_0000, 0x100, Echo { name: "a", limit: 4, count: 0, armed: false });
    let b = h.add(Echo { name: "b", limit: 4, count: 0, armed: false });
    h.connect_input(a, 0, b, 0);
    h.connect_input(b, 0, a, 0);
    h.get_mut::<Echo>(a).armed = true;
    h.get_mut::<Echo>(b).armed = true;
    // a rises -> b1 -> a1 -> b2 -> a2 -> b3 -> a3 -> b4 (b reaches its limit and stops the chain).
    h.write32(0x4000_0000, 1);
    assert_eq!((h.get::<Echo>(a).count, h.get::<Echo>(b).count), (3, 4));
    assert!(h.warnings().is_empty());
}

#[test]
fn input_for_a_running_peripheral_is_delivered_after_it_returns() {
    // X's write handler pokes Y through the bus; Y answers on a line wired to X. X must not be
    // re-entered: the input arrives only after X's handler has finished.
    struct X {
        log: Shared,
        y_addr: u32,
    }
    impl Peripheral for X {
        fn name(&self) -> &str {
            "x"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push("x.write.begin".into());
            ctx.mem_write(self.y_addr, Width::Word, 1);
            self.log.borrow_mut().push("x.write.end".into());
        }
        fn on_input(&mut self, line: u32, level: bool, _ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push(format!("x.in{line}={}", u8::from(level)));
        }
        any_impl!();
    }
    struct Y {
        log: Shared,
    }
    impl Peripheral for Y {
        fn name(&self) -> &str {
            "y"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, v: u32, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push("y.write".into());
            ctx.set_output(0, v != 0);
        }
        any_impl!();
    }
    let log = shared();
    let mut h = Harness::new();
    let x = h.add_mapped(0x4000_0000, 0x100, X { log: log.clone(), y_addr: 0x4000_0100 });
    let y = h.add_mapped(0x4000_0100, 0x100, Y { log: log.clone() });
    h.connect_input(y, 0, x, 2);
    log.borrow_mut().clear();
    h.write32(0x4000_0000, 0);
    assert_eq!(*log.borrow(), ["x.write.begin", "y.write", "x.write.end", "x.in2=1"]);
    assert!(h.warnings().is_empty());
}

#[test]
fn dma_style_read_back_of_the_sender_works() {
    // ADC raises a DMA request from its event; the DMA controller reads the ADC's data
    // register through the bus and stores it. The ADC must be readable at that point.
    const ADC: u32 = 0x5004_0000;
    struct Adc {
        eoc: bool,
        value: u32,
    }
    impl Peripheral for Adc {
        fn name(&self) -> &str {
            "adc"
        }
        fn read(&mut self, offset: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            match offset {
                0x00 => u32::from(self.eoc),
                0x40 => {
                    self.eoc = false;
                    self.value
                }
                _ => 0,
            }
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            ctx.schedule_in(100, 1);
        }
        fn on_event(&mut self, _token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
            self.value = 0x123;
            self.eoc = true;
            ctx.set_output(0, true);
            ctx.set_output(0, false);
        }
        any_impl!();
    }
    struct Dma;
    impl Peripheral for Dma {
        fn name(&self) -> &str {
            "dma"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        fn on_input(&mut self, _line: u32, level: bool, ctx: &mut Ctx<'_>) {
            if level {
                let data = ctx.mem_read(ADC + 0x40, Width::Word);
                ctx.mem_write(0x2000_0100, Width::Word, data);
            }
        }
        any_impl!();
    }
    let mut h = Harness::new();
    let adc = h.add_mapped(ADC, 0x400, Adc { eoc: false, value: 0 });
    let dma = h.add(Dma);
    h.connect_input(adc, 0, dma, 0);
    h.write32(ADC, 1);
    h.advance_by(100);
    assert_eq!(h.read32(0x2000_0100), 0x123);
    assert!(!h.get::<Adc>(adc).eoc, "DMA consumed the conversion result");
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
}

#[test]
fn bus_access_to_the_running_peripheral_is_a_logged_error() {
    struct Selfish;
    impl Peripheral for Selfish {
        fn name(&self) -> &str {
            "selfish"
        }
        fn read(&mut self, offset: u32, _w: Width, ctx: &mut Ctx<'_>) -> u32 {
            // Reads its own register through the bus: cannot work, must not crash.
            ctx.mem_read(0x4000_0000 + offset + 4, Width::Word) + 7
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        any_impl!();
    }
    let mut h = Harness::new();
    h.add_mapped(0x4000_0000, 0x100, Selfish);
    assert_eq!(h.read32(0x4000_0000), 7);
    let warnings = h.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("re-entrant"), "{}", warnings[0]);
}

// ---- events ----------------------------------------------------------------

struct Ticker {
    period: Time,
    remaining: u32,
    fired: Vec<(Time, Time)>,
    last: EventId,
}

impl Peripheral for Ticker {
    fn name(&self) -> &str {
        "ticker"
    }
    fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
        0
    }
    fn write(&mut self, offset: u32, _w: Width, value: u32, ctx: &mut Ctx<'_>) {
        match offset {
            0 => self.last = ctx.schedule_at(u64::from(value), 7),
            4 => {
                let id = self.last;
                assert!(ctx.cancel(id));
            }
            _ => {}
        }
    }
    fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
        self.fired.push((scheduled, ctx.now()));
        if token == 1 && self.remaining > 0 {
            self.remaining -= 1;
            self.last = ctx.schedule_at(scheduled + self.period, 1);
        }
    }
    any_impl!();
}

fn ticker(period: Time, remaining: u32) -> Ticker {
    Ticker { period, remaining, fired: Vec::new(), last: EventId::NONE }
}

#[test]
fn periodic_events_do_not_drift() {
    let mut h = Harness::new();
    let id = h.add(ticker(1000, 5));
    h.with::<Ticker, _>(id, |_t, ctx| {
        ctx.schedule_at(1000, 1);
    });
    h.advance_to(10_000);
    let times: Vec<Time> = h.get::<Ticker>(id).fired.iter().map(|f| f.0).collect();
    assert_eq!(times, [1000, 2000, 3000, 4000, 5000, 6000]);
    // The harness fires at the exact scheduled time.
    assert!(h.get::<Ticker>(id).fired.iter().all(|f| f.0 == f.1));
    assert_eq!(h.now(), 10_000);
}

#[test]
fn events_in_the_past_fire_at_the_current_clock_time_and_keep_their_scheduled_time() {
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    let id = core.add_peripheral(Box::new(ticker(1000, 2)));
    core.advance_clock(1384);
    // Scheduling at a time that is already past runs the handler as soon as the call returns, at the clock
    // time, with its own scheduled time as the argument.
    core.with_peripheral::<Ticker, _>(id, |_t, ctx| {
        ctx.schedule_at(1000, 1);
    });
    let fired = core.get::<Ticker>(id).unwrap().fired.clone();
    assert_eq!(fired, [(1000, 1384)]);
    // The next period is relative to the scheduled time (2000), not the late 'now'.
    assert_eq!(core.events.peek_time(), Some(2000));
    assert_eq!(core.advance_clock(1999), 0);
    assert_eq!(core.advance_clock(2000), 1);
    assert_eq!(core.get::<Ticker>(id).unwrap().fired.last(), Some(&(2000, 2000)), "queued events fire at their own time");
}

#[test]
fn advance_clock_sets_the_clock_to_each_events_own_time() {
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    let id = core.add_peripheral(Box::new(ticker(100, 3)));
    core.with_peripheral::<Ticker, _>(id, |_t, ctx| {
        ctx.schedule_at(100, 1);
    });
    assert_eq!(core.advance_clock(1_000), 4);
    let fired = core.get::<Ticker>(id).unwrap().fired.clone();
    assert_eq!(fired, [(100, 100), (200, 200), (300, 300), (400, 400)]);
    assert_eq!(core.clock_time(), 1_000);
    assert_eq!(core.now(), 1_000);
    assert_eq!(core.stats.events_fired, 4);
}

#[test]
fn events_at_one_instant_fire_in_scheduling_order_and_cancel_works() {
    let mut h = Harness::new();
    let id = h.add_mapped(0x4000_0000, 0x100, ticker(0, 0));
    h.write32(0x4000_0000, 500); // schedule at 500 (token 7)
    h.write32(0x4000_0000, 500);
    h.write32(0x4000_0004, 0); // cancel the second
    assert_eq!(h.pending_events().len(), 1);
    h.advance_to(499);
    assert!(h.get::<Ticker>(id).fired.is_empty());
    assert_eq!(h.next_event_time(), Some(500));
    h.advance_to(500);
    assert_eq!(h.get::<Ticker>(id).fired, [(500, 500)]);
    assert_eq!(h.next_event_time(), None);
}

#[test]
fn ordinary_events_do_not_request_a_cpu_return_like_renode_clock_entries() {
    let mut h = Harness::new();
    let id = h.add_mapped(0x4000_0000, 0x100, ticker(0, 0));
    h.write32(0x4000_0000, 20_000);
    h.write32(0x4000_0000, 9_999);
    h.write32(0x4000_0000, 0); // already due
    assert!(!h.take_stop_request(), "only request_return / LimitTimer setters / schedule_action ask the CPU to return");
    assert_eq!(h.get::<Ticker>(id).fired, [(0, 0)], "the already-due event ran right after the write");
    assert_eq!(h.pending_events().len(), 2);
}

#[test]
fn request_cpu_stop_from_a_handler() {
    struct Stopper;
    impl Peripheral for Stopper {
        fn name(&self) -> &str {
            "stopper"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            ctx.request_cpu_stop();
        }
        any_impl!();
    }
    let mut h = Harness::new();
    h.add_mapped(0x4000_0000, 0x100, Stopper);
    assert!(!h.take_stop_request());
    h.write32(0x4000_0000, 0);
    assert!(h.take_stop_request());
}

#[test]
fn runaway_event_loop_is_cut_off() {
    struct Runaway;
    impl Peripheral for Runaway {
        fn name(&self) -> &str {
            "runaway"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            ctx.schedule_at(0, 0);
        }
        fn on_event(&mut self, _t: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
            ctx.schedule_at(scheduled, 0); // zero period: due again immediately
        }
        any_impl!();
    }
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    let id = core.add_peripheral(Box::new(Runaway));
    core.with_peripheral::<Runaway, _>(id, |_r, ctx| {
        ctx.schedule_at(5, 0);
    });
    assert_eq!(core.advance_clock(5), MAX_EVENTS_PER_DRAIN);
    assert!(core.log.contains(LogLevel::Error, "runaway"));
}

// ---- memory access from peripherals ---------------------------------------------

#[test]
fn ctx_memory_access_covers_plain_memory_mmio_and_bulk_transfers() {
    let log = shared();
    let mut h = Harness::new();
    let id = h.add_mapped(0x4000_0000, 0x100, Regs::new("regs", AccessPolicy::EXACT, &log));
    h.get_mut::<Regs>(id).regs[0] = 0xCAFE_F00D;
    log.borrow_mut().clear();
    h.with::<Regs, _>(id, |_r, _ctx| {}); // sanity: typed access works while idle
    struct Dummy;
    impl Peripheral for Dummy {
        fn name(&self) -> &str {
            "dummy"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        any_impl!();
    }
    let dummy = h.add(Dummy);
    h.with::<Dummy, _>(dummy, |_d, ctx| {
        assert_eq!(ctx.mem_read(0x4000_0000, Width::Word), 0xCAFE_F00D);
        ctx.mem_write(0x2000_0000, Width::Word, 0x0102_0304);
        ctx.mem_write(0x4000_0004, Width::Half, 0xBEEF);
        let mut buffer = [0u8; 4];
        ctx.mem_read_bytes(0x2000_0000, &mut buffer);
        assert_eq!(buffer, [4, 3, 2, 1]);
        ctx.mem_write_bytes(0x1000_0000, &[9, 8, 7]);
        assert_eq!(ctx.mem_read(0x1000_0000, Width::Half), 0x0809);
        assert!(ctx.is_plain_memory(0x2000_0000) && ctx.is_plain_memory(0x0800_0000));
        assert!(!ctx.is_plain_memory(0x4000_0000));
        // Bulk access into an MMIO region falls back to bytewise bus accesses.
        ctx.mem_write_bytes(0x4000_0008, &[1, 2]);
        assert_eq!(ctx.mem_peek(0x4000_0008, Width::Half), Some(0x0201));
        assert_eq!(ctx.mem_peek(0x2000_0000, Width::Word), Some(0x0102_0304));
        assert_eq!(ctx.mem_peek(0x5000_0000, Width::Word), None);
    });
    assert_eq!(
        *log.borrow(),
        ["regs:r4@0@0", "regs:w2@4=beef@0", "regs:w1@8=1@0", "regs:w1@9=2@0"]
    );
}

#[test]
fn accesses_straddling_plain_memory_are_split() {
    let mut h = Harness::new();
    // Word read whose last byte is past the end of SRAM2: bytes outside are unmapped (0 + one warning each).
    h.write8(0x1000_7FFF, 0xAB);
    h.write8(0x1000_7FFE, 0xCD);
    assert_eq!(h.read(0x1000_7FFE, Width::Word), 0xABCD, "bytes past the end read as zero");
    assert_eq!(h.read(0x1000_7FFE, Width::Half), 0xABCD);
    // Spanning two adjacent plain regions is impossible with this layout; a straddling write
    // must not corrupt the in-range part's neighbors.
    h.write(0x1000_7FFF, Width::Half, 0xFFFF);
    assert_eq!(h.read8(0x1000_7FFF), 0xFF);
    assert_eq!(h.read8(0x1000_7FFE), 0xCD);
}

// ---- debugger access --------------------------------------------------------------

#[test]
fn peek_and_poke_have_no_side_effects() {
    let log = shared();
    let mut h = Harness::new();
    let timer = AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD);
    let id = h.add_mapped(0x4000_0400, 0x400, Regs::new("tim", timer, &log));
    assert!(h.core_mut().poke(0x4000_0404, Width::Word, 0x1122_3344));
    log.borrow_mut().clear();
    assert_eq!(h.peek(0x4000_0404, Width::Word), Some(0x1122_3344));
    assert_eq!(h.peek(0x4000_0405, Width::Byte), Some(0x33), "peeks are translated like reads");
    assert_eq!(h.peek(0x4000_0406, Width::Half), Some(0x1122));
    assert_eq!(h.peek(0x4000_0FFF, Width::Word), None);
    assert!(log.borrow().is_empty(), "peek must not touch the bus path");
    // poke only for native widths.
    assert!(!h.core_mut().poke(0x4000_0405, Width::Byte, 1));
    // Plain memory.
    assert!(h.core_mut().poke(0x2000_0010, Width::Word, 0xDEAD_BEEF));
    assert_eq!(h.peek(0x2000_0010, Width::Word), Some(0xDEAD_BEEF));
    assert_eq!(h.get::<Regs>(id).regs[1], 0x1122_3344);
}

#[test]
fn summaries_and_typed_lookup() {
    let log = shared();
    let mut h = Harness::new();
    let id = h.add_mapped(0x4000_0400, 0x400, ArrayMemory::new("pwr", 0x400));
    let other = h.add_mapped(0x4000_0800, 0x400, Regs::new("regs", AccessPolicy::EXACT, &log));
    assert!(h.core().get::<Regs>(id).is_none(), "wrong type");
    assert!(h.core().get::<ArrayMemory>(id).is_some());
    assert!(h.core().get::<Regs>(other).is_some());
    let summaries = h.core().summaries();
    assert_eq!(summaries[0], ("pwr".to_string(), "array memory: 1024 bytes".to_string()));
    assert_eq!(summaries[1].0, "regs");
}

#[test]
fn array_memory_behaves_like_renode_through_the_bus() {
    let mut h = Harness::new();
    h.add_mapped(0x4000_7000, 0x400, ArrayMemory::new("pwr", 0x400));
    h.write32(0x4000_7010, 0x104);
    assert_eq!(h.read32(0x4000_7010), 0x104);
    assert_eq!(h.read8(0x4000_7011), 1);
    h.write8(0x4000_7013, 0x80);
    assert_eq!(h.read32(0x4000_7010), 0x8000_0104);
    // An access running off the end reads 0 / is dropped, and is logged as an error.
    h.write32(0x4000_73FE, 0xFFFF_FFFF);
    assert_eq!(h.read32(0x4000_73FE), 0);
    assert_eq!(h.read16(0x4000_73FE), 0, "bytes inside the array are untouched");
    assert_eq!(h.warnings().len(), 2);
    assert!(h.core().log.count(LogLevel::Error) >= 2);
}

#[test]
fn reset_all_resets_peripherals_in_registration_order() {
    struct Resettable {
        name: &'static str,
        log: Shared,
    }
    impl Peripheral for Resettable {
        fn name(&self) -> &str {
            self.name
        }
        fn reset(&mut self, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push(format!("reset {} @{}", self.name, ctx.now()));
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        any_impl!();
    }
    let log = shared();
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    core.add_peripheral(Box::new(Resettable { name: "one", log: log.clone() }));
    core.add_peripheral(Box::new(Resettable { name: "two", log: log.clone() }));
    core.advance_clock(42);
    core.reset_all();
    assert_eq!(*log.borrow(), ["reset one @42", "reset two @42"]);
}

#[test]
fn connect_validation() {
    let log = shared();
    let mut core = MachineCore::new(MemoryLayout::STM32L4_1M);
    let a = core.add_peripheral(Box::new(Pin::new("a", &log)));
    assert_eq!(core.connect_irq(a, 64, 1), Err(MapError::LineOutOfRange(64)));
    assert_eq!(core.connect_irq(PeriphId(9), 0, 1), Err(MapError::UnknownPeripheral(PeriphId(9))));
    assert_eq!(core.connect_input(a, 0, PeriphId(9), 0), Err(MapError::UnknownPeripheral(PeriphId(9))));
    assert_eq!(core.connect_irq(a, 63, 1), Ok(()));
    // Level >= 64 requests from a handler are logged, not fatal.
    core.drive_output(a, 64, true);
    assert!(core.log.contains(LogLevel::Error, "out of range"));
}
