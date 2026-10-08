// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/IRQControllers/STM32F4_EXTI.cs and
// STM32_EXTICore.cs (MIT License, Copyright (c) Antmicro).

//! `IRQControllers.STM32F4_EXTI` (`numberOfOutputLines: 24` on both boards).
//!
//! * **Input lines** `0..numberOfOutputLines` (`OnGPIO`): `gpioC@1 -> exti@1` (handset), `gpioA@5 ->
//!   exti@5` (main), `lcd.TE -> exti@3`. Lines below `firstDirectLine` (23) are *configurable*: an input
//!   is accepted only if its `IMR` bit is set and the edge matches `RTSR` (rising) / `FTSR` (falling); an
//!   accepted edge sets the `PR` bit and drives the output line **high until software writes 1 to the
//!   `PR` bit** (a latched request, whatever the input does afterwards). The direct line 23 follows the
//!   input level (output and `PR` bit), ignoring `IMR`.
//! * **Output lines** `0..numberOfOutputLines` (`Connections[n]`): `[0-4] -> nvic@[6-10]`,
//!   `[5-9] -> exti5to9@[0-4]`, `[10-15] -> exti10to15@[0-5]`.
//! * **Registers**: `IMR`, `EMR` (storage only), `RTSR`, `FTSR`, `SWIER`, `PR`.
//!
//! // Renode parity: `SWIER` only drives the output line of every `IMR`-enabled line it writes; it does
//! **not** set the `PR` bit (the pending bit of the manual) and it reads 0 (Renode's `softwareInterrupt`
//! backing field is never set). Writing a 1 to a `PR` bit clears the pending bit *and* drops the output
//! line even if the `PR` bit was not set, which is how software-triggered requests are acknowledged.
//! `EMR` is never consulted (no events). The class derives from `BasicDoubleWordPeripheral`, so unhandled
//! accesses are logged with the `RegisterMapper` annotation (`Unhandled read from offset 0x18
//! (PendingRegister+0x4).`), unlike the GPIO, CRC and RNG messages (recorded from Renode, see
//! `tests/renode_golden.rs`).

use crate::gpio::regfw::{Hooks, Mode, Model, RegisterBuilder, RegisterFile};
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, View, Width};

/// `IKnownSize.Size`.
pub const SIZE: u32 = 0x400;

/// Register offsets.
pub mod reg {
    pub const IMR: u32 = 0x00;
    pub const EMR: u32 = 0x04;
    pub const RTSR: u32 = 0x08;
    pub const FTSR: u32 = 0x0C;
    pub const SWIER: u32 = 0x10;
    pub const PR: u32 = 0x14;
}

const R_IMR: usize = 0;
const R_EMR: usize = 1;
const R_RTSR: usize = 2;
const R_FTSR: usize = 3;
const R_SWIER: usize = 4;
const R_PR: usize = 5;

/// `STM32F4_EXTI.DefaultFirstDirectLine`.
pub const DEFAULT_FIRST_DIRECT_LINE: u32 = 23;
/// Default `numberOfOutputLines` of the C# constructor (the platforms pass 24).
pub const DEFAULT_NUMBER_OF_OUTPUT_LINES: u32 = 14;

/// Warn-once key of the out-of-range input error.
const KEY_LINE_RANGE: u64 = 1 << 40;

struct ExtiDev {
    lines: u32,
    /// `numberOfLinesMask`.
    lines_mask: u32,
    /// `LineConfigurableMask` (bits below `firstDirectLine`).
    configurable_mask: u32,
    /// `softwareInterrupt` (never set by Renode; only cleared by `PR` writes and reset).
    software_interrupt: u32,
}

impl ExtiDev {
    /// `STM32_EXTICore.CanSetInterruptValue(lineNumber, value, out isLineConfigurable)` with the
    /// arguments the EXTI uses (`treatOutOfRangeLinesAsDirect`, no separate configs,
    /// `allowMaskingDirectLines: false`). Returns `(accepted, is_configurable)`.
    fn can_set_interrupt_value(&self, regs: &RegisterFile, line: u32, level: bool) -> (bool, bool) {
        let configurable = self.configurable_mask & (1 << line) != 0;
        let masked = regs.field(R_IMR, 0) & (1 << line) == 0;
        if masked && configurable {
            // A masked direct line is *not* ignored (`allowMaskingDirectLines` is false).
            return (false, configurable);
        }
        if configurable {
            let rising = regs.field(R_RTSR, 0) & (1 << line) != 0 && level;
            let falling = regs.field(R_FTSR, 0) & (1 << line) != 0 && !level;
            return (rising || falling, configurable);
        }
        (true, configurable)
    }

    /// `BitHelper.ForeachActiveBit` over the lines of `mask`, lowest first.
    fn drive_lines(ctx: &mut Ctx<'_>, mask: u32, level: bool) {
        for line in 0..32 {
            if mask & (1 << line) != 0 {
                ctx.set_output(line, level);
            }
        }
    }
}

impl Model for ExtiDev {
    fn provide(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, _field: usize, current: u32) -> u32 {
        match reg {
            R_SWIER => self.software_interrupt,
            _ => current,
        }
    }

    fn field_written(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, _field: usize, _old: u32, written: u32) {
        match reg {
            R_SWIER => {
                let lines = written & self.lines_mask & regs.field(R_IMR, 0);
                Self::drive_lines(ctx, lines, true);
            }
            R_PR => {
                self.software_interrupt &= !written;
                Self::drive_lines(ctx, written & self.lines_mask, false);
            }
            _ => {}
        }
    }
}

/// `IRQControllers.STM32F4_EXTI`.
pub struct Exti {
    name: String,
    regs: RegisterFile,
    dev: ExtiDev,
}

impl Exti {
    /// EXTI with `number_of_output_lines` lines (1..=32) and the default direct line (23).
    pub fn new(name: impl Into<String>, number_of_output_lines: u32) -> Self {
        Self::with_first_direct_line(name, number_of_output_lines, DEFAULT_FIRST_DIRECT_LINE)
    }

    /// Lines below `first_direct_line` are configurable, the others follow their input level.
    pub fn with_first_direct_line(name: impl Into<String>, number_of_output_lines: u32, first_direct_line: u32) -> Self {
        assert!(
            (1..=32).contains(&number_of_output_lines),
            "STM32F4_EXTI: numberOfOutputLines must be 1..=32 (the registers are 32 bits wide)"
        );
        assert!(first_direct_line <= 64, "STM32F4_EXTI: firstDirectLine must be at most 64");
        let lines_mask = if number_of_output_lines == 32 { u32::MAX } else { (1u32 << number_of_output_lines) - 1 };
        let configurable_mask = if first_direct_line >= 32 { u32::MAX } else { (1u32 << first_direct_line) - 1 };
        let regs = RegisterFile::new(vec![
            RegisterBuilder::new(reg::IMR).field(0, 32, Mode::READ_WRITE, Hooks::NONE),
            // Blank implementation in Renode: storage only.
            RegisterBuilder::new(reg::EMR).field(0, 32, Mode::READ_WRITE, Hooks::NONE),
            RegisterBuilder::new(reg::RTSR).field(0, 32, Mode::READ_WRITE, Hooks::NONE),
            RegisterBuilder::new(reg::FTSR).field(0, 32, Mode::READ_WRITE, Hooks::NONE),
            RegisterBuilder::new(reg::SWIER).field(0, 32, Mode::READ_WRITE, Hooks::PROVIDER | Hooks::WRITE),
            RegisterBuilder::new(reg::PR).field(0, 32, Mode::READ_WRITE_ONE_TO_CLEAR, Hooks::WRITE),
        ])
        // The class derives from BasicDoubleWordPeripheral: unhandled accesses name the closest register of
        // its `Registers` enum, e.g. "Unhandled read from offset 0x18 (PendingRegister+0x4)."
        .with_offset_names(&[
            (reg::IMR, "InterruptMask"),
            (reg::EMR, "EventMask"),
            (reg::RTSR, "RisingTriggerSelection"),
            (reg::FTSR, "FallingTriggerSelection"),
            (reg::SWIER, "SoftwareInterruptEvent"),
            (reg::PR, "PendingRegister"),
        ]);
        Self {
            name: name.into(),
            regs,
            dev: ExtiDev { lines: number_of_output_lines, lines_mask, configurable_mask, software_interrupt: 0 },
        }
    }

    pub fn number_of_lines(&self) -> u32 {
        self.dev.lines
    }

    /// `IMR`.
    pub fn interrupt_mask(&self) -> u32 {
        self.regs.value(R_IMR)
    }

    /// `EMR` (stored, never used).
    pub fn event_mask(&self) -> u32 {
        self.regs.value(R_EMR)
    }

    /// `RTSR`.
    pub fn rising_trigger(&self) -> u32 {
        self.regs.value(R_RTSR)
    }

    /// `FTSR`.
    pub fn falling_trigger(&self) -> u32 {
        self.regs.value(R_FTSR)
    }

    /// `PR`.
    pub fn pending(&self) -> u32 {
        self.regs.value(R_PR)
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        format!(
            "{}: IMR=0x{:08X} EMR=0x{:08X} RTSR=0x{:08X} FTSR=0x{:08X} PR=0x{:08X}",
            self.name,
            self.interrupt_mask(),
            self.event_mask(),
            self.rising_trigger(),
            self.falling_trigger(),
            self.pending()
        )
    }

    fn peek_register(&self, offset: u32) -> u32 {
        match offset {
            reg::IMR => self.regs.value(R_IMR),
            reg::EMR => self.regs.value(R_EMR),
            reg::RTSR => self.regs.value(R_RTSR),
            reg::FTSR => self.regs.value(R_FTSR),
            reg::SWIER => self.dev.software_interrupt,
            reg::PR => self.regs.value(R_PR),
            _ => 0,
        }
    }
}

impl Peripheral for Exti {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset`: registers cleared, then every output line dropped (lowest line first).
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.regs.reset();
        self.dev.software_interrupt = 0;
        for line in 0..self.dev.lines {
            ctx.set_output(line, false);
        }
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        self.regs.read(offset, &mut self.dev, ctx)
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.regs.write(offset, value, &mut self.dev, ctx);
    }

    /// `STM32F4_EXTI.OnGPIO`.
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        if line >= self.dev.lines {
            ctx.error_once(
                KEY_LINE_RANGE | u64::from(line),
                format_args!("GPIO number {line} is out of range [0; {})", self.dev.lines),
            );
            return;
        }
        let (accepted, configurable) = self.dev.can_set_interrupt_value(&self.regs, line, level);
        if !accepted {
            return;
        }
        // Configurable lines can only be raised here (latched high until `PR` is written).
        let level = if configurable { true } else { level };
        // `STM32_EXTICore.UpdatePendingValue`: set or clear the `PR` bit.
        let pending = self.regs.field(R_PR, 0);
        let pending = if level { pending | (1 << line) } else { pending & !(1 << line) };
        self.regs.set_field(R_PR, 0, pending);
        ctx.set_output(line, level);
    }

    // IDoubleWordPeripheral without [AllowedTranslations].
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        Some(self.peek_register(offset))
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.describe()
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combined_input::CombinedInput;
    use crate::gpio::{reg as gpio_reg, Gpio, GpioConfig};
    use emu_core::testing::Harness;
    use emu_core::{LogLevel, PeriphId, Time};

    const BASE: u32 = 0x4001_0400;
    const GPIO_A: u32 = 0x4800_0000;

    /// The EXTI wiring of `handset.repl` / `main.repl`, plus gpioA pin `n` -> EXTI line `n` for the
    /// lines used in the tests (the platforms connect only one pin of one port).
    struct Platform {
        h: Harness,
        exti: PeriphId,
        gpio: PeriphId,
        exti5to9: PeriphId,
        exti10to15: PeriphId,
    }

    fn platform() -> Platform {
        let mut h = Harness::new();
        let exti = h.add_mapped(BASE, 0x400, Exti::new("exti", 24));
        let exti5to9 = h.add(CombinedInput::new("exti5to9", 5));
        let exti10to15 = h.add(CombinedInput::new("exti10to15", 6));
        let gpio = h.add_mapped(GPIO_A, 0x400, Gpio::new("gpioA", GpioConfig::default()));
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
        for pin in 0..16 {
            h.connect_input(gpio, pin, exti, pin);
        }
        h.clear_irq_changes();
        Platform { h, exti, gpio, exti5to9, exti10to15 }
    }

    fn setup() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Exti::new("exti", 24));
        (h, id)
    }

    fn levels(changes: Vec<(Time, bool)>) -> Vec<bool> {
        changes.into_iter().map(|(_, level)| level).collect()
    }

    fn exti(p: &Platform) -> &Exti {
        p.h.get::<Exti>(p.exti)
    }

    // ---- registers ------------------------------------------------------------------

    #[test]
    fn registers_reset_to_zero_and_store_their_values() {
        let (mut h, id) = setup();
        for offset in [reg::IMR, reg::EMR, reg::RTSR, reg::FTSR, reg::SWIER, reg::PR] {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:02X}");
        }
        for (offset, value) in [(reg::IMR, 0xFFFF_FFFFu32), (reg::EMR, 0x1234_5678), (reg::RTSR, 0x00AA_AAAA), (reg::FTSR, 0xDEAD_BEEF)] {
            h.write32(BASE + offset, value);
            assert_eq!(h.read32(BASE + offset), value, "offset 0x{offset:02X} stores all 32 bits");
        }
        let e = h.get::<Exti>(id);
        assert_eq!((e.interrupt_mask(), e.event_mask(), e.rising_trigger(), e.falling_trigger()), (0xFFFF_FFFF, 0x1234_5678, 0xAA_AAAA, 0xDEAD_BEEF));
        assert!(h.warnings().is_empty());
        assert_eq!(h.core().summaries().iter().find(|(n, _)| n == "exti").unwrap().1, e.describe());
        assert_eq!(e.describe(), "exti: IMR=0xFFFFFFFF EMR=0x12345678 RTSR=0x00AAAAAA FTSR=0xDEADBEEF PR=0x00000000");
    }

    #[test]
    fn unimplemented_offsets_log_and_read_zero() {
        let (mut h, _id) = setup();
        assert_eq!(h.read32(BASE + 0x18), 0);
        h.write32(BASE + 0x18, 5);
        assert_eq!(h.read32(BASE + 0x3FC), 0);
        // Texts recorded from Renode: the class is a BasicDoubleWordPeripheral, whose RegisterMapper names
        // the closest lower register of its enum.
        assert_eq!(
            h.warnings(),
            [
                "Unhandled read from offset 0x18 (PendingRegister+0x4).",
                "Unhandled write to offset 0x18 (PendingRegister+0x4), value 0x5.",
                "Unhandled read from offset 0x3FC (PendingRegister+0x3e8)."
            ]
        );
    }

    #[test]
    fn halfword_and_byte_accesses_are_not_supported() {
        let (mut h, _id) = setup();
        h.write16(BASE + reg::IMR, 0xFFFF);
        h.write8(BASE + reg::IMR, 0xFF);
        assert_eq!(h.read16(BASE + reg::IMR), 0);
        assert_eq!(h.read8(BASE + reg::IMR), 0);
        assert_eq!(h.read32(BASE + reg::IMR), 0, "nothing reached the register");
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings[0].contains("Attempted Word write isn't supported by the peripheral"));
    }

    // ---- edge detection ---------------------------------------------------------------

    #[test]
    fn rising_edge_sets_pending_and_latches_the_output_until_pr_is_written() {
        let mut p = platform();
        let line5 = p.h.probe(p.exti, 5);
        p.h.write32(BASE + reg::IMR, 1 << 5);
        p.h.write32(BASE + reg::RTSR, 1 << 5);
        p.h.set_input(p.exti, 5, true);
        assert_eq!(p.h.read32(BASE + reg::PR), 1 << 5);
        assert!(p.h.irq_level(23), "line 5 reaches NVIC 23 through exti5to9");
        // The request stays latched when the input goes low again.
        p.h.set_input(p.exti, 5, false);
        assert!(p.h.irq_level(23));
        assert_eq!(p.h.read32(BASE + reg::PR), 1 << 5);
        assert_eq!(levels(p.h.probe_changes(line5)), [true]);
        // Writing 1 to the PR bit clears it and drops the line.
        p.h.write32(BASE + reg::PR, 1 << 5);
        assert_eq!(p.h.read32(BASE + reg::PR), 0);
        assert!(!p.h.irq_level(23));
        assert_eq!(levels(p.h.probe_changes(line5)), [true, false]);
    }

    #[test]
    fn falling_edge_needs_ftsr_and_both_edges_trigger_independently() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 1 << 1);
        p.h.write32(BASE + reg::FTSR, 1 << 1);
        p.h.set_input(p.exti, 1, true);
        assert_eq!(exti(&p).pending(), 0, "a rising edge is ignored when only FTSR is set");
        assert!(!p.h.irq_level(7));
        p.h.set_input(p.exti, 1, false);
        assert_eq!(exti(&p).pending(), 1 << 1);
        assert!(p.h.irq_level(7), "line 1 -> NVIC 7");
        p.h.write32(BASE + reg::PR, 1 << 1);
        assert!(!p.h.irq_level(7));
        // Both edges selected: each edge is a new request once software acknowledged the previous one.
        p.h.write32(BASE + reg::RTSR, 1 << 1);
        p.h.set_input(p.exti, 1, true);
        assert!(p.h.irq_level(7));
        p.h.write32(BASE + reg::PR, 1 << 1);
        assert!(!p.h.irq_level(7));
        p.h.set_input(p.exti, 1, false);
        assert!(p.h.irq_level(7));
        assert_eq!(exti(&p).pending(), 1 << 1);
    }

    #[test]
    fn no_edge_selection_or_a_masked_line_means_no_request() {
        let mut p = platform();
        // Unmasked but no trigger selected.
        p.h.write32(BASE + reg::IMR, 1 << 3);
        p.h.set_input(p.exti, 3, true);
        p.h.set_input(p.exti, 3, false);
        assert_eq!(exti(&p).pending(), 0);
        // Trigger selected but masked.
        p.h.write32(BASE + reg::IMR, 0);
        p.h.write32(BASE + reg::RTSR, 1 << 3);
        p.h.write32(BASE + reg::FTSR, 1 << 3);
        p.h.set_input(p.exti, 3, true);
        p.h.set_input(p.exti, 3, false);
        assert_eq!(exti(&p).pending(), 0);
        assert!(!p.h.irq_level(9));
        // Unmasking afterwards does not recreate the lost edge: the next edge is accepted.
        p.h.write32(BASE + reg::IMR, 1 << 3);
        assert_eq!(exti(&p).pending(), 0);
        p.h.set_input(p.exti, 3, true);
        assert_eq!(exti(&p).pending(), 1 << 3);
        assert!(p.h.irq_level(9));
    }

    #[test]
    fn pr_write_clears_only_the_written_bits() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 0xFFFF);
        p.h.write32(BASE + reg::RTSR, 0xFFFF);
        p.h.set_input(p.exti, 2, true);
        p.h.set_input(p.exti, 12, true);
        assert_eq!(exti(&p).pending(), (1 << 2) | (1 << 12));
        assert!(p.h.irq_level(8) && p.h.irq_level(40));
        p.h.write32(BASE + reg::PR, 1 << 12);
        assert_eq!(exti(&p).pending(), 1 << 2);
        assert!(p.h.irq_level(8) && !p.h.irq_level(40));
        // Writing zeroes (or a bit that is not pending) leaves everything alone.
        p.h.write32(BASE + reg::PR, 0);
        p.h.write32(BASE + reg::PR, 1 << 7);
        assert_eq!(exti(&p).pending(), 1 << 2);
        assert!(p.h.irq_level(8));
    }

    // ---- software interrupts ----------------------------------------------------------

    #[test]
    fn swier_raises_unmasked_lines_without_setting_the_pending_bit_and_reads_zero() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, (1 << 4) | (1 << 8));
        p.h.write32(BASE + reg::SWIER, (1 << 4) | (1 << 6) | (1 << 8));
        assert_eq!(p.h.read32(BASE + reg::SWIER), 0, "Renode's SWIER always reads 0");
        assert!(p.h.irq_level(10), "line 4 -> NVIC 10");
        assert!(p.h.irq_level(23), "line 8 -> exti5to9 -> NVIC 23");
        assert!(!p.h.core().output_level(p.exti, 6), "line 6 is masked");
        assert_eq!(p.h.read32(BASE + reg::PR), 0, "Renode does not set PR for software requests");
        // The request is acknowledged by writing PR even though the bit was never set.
        p.h.write32(BASE + reg::PR, (1 << 4) | (1 << 8));
        assert!(!p.h.irq_level(10) && !p.h.irq_level(23));
    }

    #[test]
    fn swier_ignores_lines_beyond_the_configured_count() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 0xFFFF_FFFF);
        p.h.write32(BASE + reg::SWIER, 1 << 30 | 1 << 24 | 1 << 23);
        assert!(p.h.core().output_level(p.exti, 23));
        assert!(!p.h.core().output_level(p.exti, 24));
        assert!(!p.h.core().output_level(p.exti, 30));
        p.h.write32(BASE + reg::PR, 1 << 23);
        assert!(!p.h.core().output_level(p.exti, 23));
        assert!(p.h.warnings().is_empty());
    }

    // ---- direct line ---------------------------------------------------------------------

    #[test]
    fn direct_line_follows_the_input_and_ignores_imr_and_trigger_selection() {
        let mut p = platform();
        let line23 = p.h.probe(p.exti, 23);
        assert_eq!(p.h.read32(BASE + reg::IMR), 0);
        p.h.set_input(p.exti, 23, true);
        assert_eq!(exti(&p).pending(), 1 << 23, "PR follows the level of a direct line");
        assert!(p.h.core().output_level(p.exti, 23));
        p.h.set_input(p.exti, 23, false);
        assert_eq!(exti(&p).pending(), 0);
        assert!(!p.h.core().output_level(p.exti, 23));
        assert_eq!(levels(p.h.probe_changes(line23)), [true, false]);
    }

    #[test]
    fn with_first_direct_line_changes_which_lines_are_configurable() {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Exti::with_first_direct_line("exti", 16, 10));
        h.set_input(id, 12, true);
        assert!(h.core().output_level(id, 12), "line 12 is direct: it follows the input without IMR");
        h.set_input(id, 5, true);
        assert!(!h.core().output_level(id, 5), "line 5 is configurable and masked");
    }

    // ---- inputs out of range --------------------------------------------------------------

    #[test]
    fn input_beyond_the_last_line_is_an_error() {
        let (mut h, id) = setup();
        h.set_input(id, 24, true);
        h.set_input(id, 24, true);
        h.set_input(id, 40, true);
        assert_eq!(h.read32(BASE + reg::PR), 0);
        let errors: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == LogLevel::Error).map(|e| e.message).collect();
        assert_eq!(errors, ["GPIO number 24 is out of range [0; 24)", "GPIO number 40 is out of range [0; 24)"]);
    }

    // ---- fan-out ---------------------------------------------------------------------------

    #[test]
    fn lines_fan_out_like_the_platform_wiring() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 0xFFFF);
        p.h.write32(BASE + reg::RTSR, 0xFFFF);
        // Lines 0..=4 have their own NVIC line (6..=10).
        for line in 0..=4u32 {
            p.h.set_input(p.exti, line, true);
            assert!(p.h.irq_level(6 + line), "line {line}");
            for other in 0..=4u32 {
                assert_eq!(p.h.irq_level(6 + other), other <= line, "line {line}, nvic {}", 6 + other);
            }
        }
        p.h.write32(BASE + reg::PR, 0x1F);
        for irq in 6..=10 {
            assert!(!p.h.irq_level(irq));
        }
        // Lines 5..=9 share NVIC 23: it stays high until the last request is acknowledged.
        p.h.set_input(p.exti, 5, true);
        p.h.set_input(p.exti, 9, true);
        assert!(p.h.irq_level(23));
        assert!(p.h.get::<CombinedInput>(p.exti5to9).input(0) && p.h.get::<CombinedInput>(p.exti5to9).input(4));
        p.h.write32(BASE + reg::PR, 1 << 5);
        assert!(p.h.irq_level(23));
        p.h.write32(BASE + reg::PR, 1 << 9);
        assert!(!p.h.irq_level(23));
        // Lines 10..=15 share NVIC 40.
        p.h.set_input(p.exti, 10, true);
        p.h.set_input(p.exti, 15, true);
        assert!(p.h.irq_level(40));
        assert!(p.h.get::<CombinedInput>(p.exti10to15).input(5));
        p.h.write32(BASE + reg::PR, (1 << 10) | (1 << 15));
        assert!(!p.h.irq_level(40));
        // Lines above 15 have no target.
        p.h.set_input(p.exti, 16, true);
        p.h.write32(BASE + reg::IMR, 0xFFFF_FFFF);
        p.h.write32(BASE + reg::RTSR, 0xFFFF_FFFF);
        p.h.set_input(p.exti, 17, true);
        assert!(p.h.core().output_level(p.exti, 17));
        assert!(p.h.warnings().is_empty());
    }

    #[test]
    fn connecting_a_low_input_to_a_masked_line_is_harmless() {
        // `gpioC:1 -> exti@1` pushes the current (low) level at connect time; IMR is clear then.
        let p = platform();
        assert_eq!(exti(&p).pending(), 0);
        for irq in (6..=10).chain([23, 40]) {
            assert!(!p.h.irq_level(irq), "NVIC {irq}");
        }
        for line in 0..24 {
            assert!(!p.h.core().output_level(p.exti, line), "EXTI line {line}");
        }
    }

    #[test]
    fn gpio_pin_edges_reach_the_nvic_through_the_exti() {
        let mut p = platform();
        // PA5 -> EXTI5 -> NVIC 23 (the main board's wiring).
        p.h.write32(BASE + reg::IMR, 1 << 5);
        p.h.write32(BASE + reg::FTSR, 1 << 5);
        p.h.write32(GPIO_A + gpio_reg::ODR, 1 << 5);
        assert!(!p.h.irq_level(23), "rising edge, falling trigger only");
        p.h.write32(GPIO_A + gpio_reg::BSRR, 1 << (5 + 16));
        assert!(p.h.irq_level(23), "falling edge raises the interrupt");
        assert_eq!(exti(&p).pending(), 1 << 5);
        p.h.write32(BASE + reg::PR, 1 << 5);
        assert!(!p.h.irq_level(23));
        // An external drive of the pin (a button, the LCD TE line) behaves the same.
        p.h.set_input(p.gpio, 5, true);
        p.h.set_input(p.gpio, 5, false);
        assert!(p.h.irq_level(23));
    }

    // ---- reset ----------------------------------------------------------------------------

    #[test]
    fn reset_clears_registers_and_drops_every_output_line() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 0xFFFF);
        p.h.write32(BASE + reg::RTSR, 0xFFFF);
        p.h.write32(BASE + reg::EMR, 0x55);
        p.h.set_input(p.exti, 2, true);
        p.h.set_input(p.exti, 7, true);
        assert!(p.h.irq_level(8) && p.h.irq_level(23));
        let line2 = p.h.probe(p.exti, 2);
        p.h.core_mut().reset_all();
        // Any harness operation delivers the IRQ changes queued by the reset (a masked line: no effect).
        p.h.set_input(p.exti, 3, false);
        assert_eq!(levels(p.h.probe_changes(line2)), [false], "line 2 dropped at reset");
        assert!(!p.h.irq_level(8) && !p.h.irq_level(23));
        for offset in [reg::IMR, reg::EMR, reg::RTSR, reg::FTSR, reg::PR] {
            assert_eq!(p.h.read32(BASE + offset), 0, "offset 0x{offset:02X}");
        }
    }

    #[test]
    fn peek_matches_read() {
        let mut p = platform();
        p.h.write32(BASE + reg::IMR, 0xF0F0);
        p.h.write32(BASE + reg::RTSR, 0x00F0);
        p.h.write32(BASE + reg::FTSR, 0x0F00);
        p.h.set_input(p.exti, 5, true);
        for offset in [reg::IMR, reg::EMR, reg::RTSR, reg::FTSR, reg::SWIER, reg::PR] {
            assert_eq!(p.h.peek(BASE + offset, Width::Word), Some(p.h.read32(BASE + offset)), "offset 0x{offset:02X}");
        }
        assert_eq!(p.h.peek(BASE + 0x40, Width::Word), Some(0));
    }

    #[test]
    #[should_panic(expected = "numberOfOutputLines")]
    fn too_many_lines_are_rejected() {
        let _ = Exti::new("exti", 33);
    }
}
