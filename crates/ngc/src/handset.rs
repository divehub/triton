//! Handset 65.3 board assembly: `emulation/handset.repl` (every address, size, constructor parameter
//! and connection) plus the boot sequence of `emulation/handset.resc`.
//!
//! Peripherals are added in `.repl` declaration order, because the creation order of the clock entries
//! decides in which order handlers run when several entries expire in the same nanosecond (Renode
//! `BaseClockSource`, `docs/framework.md` section 3). All connections (`->` lines) are made after the
//! peripherals exist, in `.repl` order, like Renode's creation driver does.
//!
//! | `.repl` entry | address | model |
//! | --- | --- | --- |
//! | `nvic`, `cpu`, `dwt` | `0xE000E000`, `0xE0001000` | inside `armv7m::Cpu` (priorityMask `0xF0`, 80 MHz SysTick and DWT) |
//! | `flash`, `sram1`, `sram2` | `0x08000000` 1 MiB, `0x20000000` 96 KiB, `0x10000000` 32 KiB | `PlainMemory` of the board |
//! | `rcc` | `0x40021000` | `NgcClockControl` |
//! | `pwr`, `flashControl`, `fmc`, `syscfg` | `0x40007000`, `0x40022000`, `0xA0000000` (0x1000), `0x40010000` | `ArrayMemory` (0x400) |
//! | `exti` | `0x40010400` | `Exti` (24 lines): `[0-4] -> nvic@[6-10]`, `[5-9] -> exti5to9`, `[10-15] -> exti10to15` |
//! | `exti5to9`, `exti10to15` | not mapped | `CombinedInput` (5 / 6 inputs) `-> nvic@23` / `nvic@40` |
//! | `gpioA`..`gpioH` | `0x48000000 + 0x400 * n` | `Gpio` (mode reset `0xABFFFFFF` for A, `0xFFFFFEBF` for B), `gpioC 1 -> exti@1` |
//! | `timer3`, `timer6` | `0x40000400`, `0x40001000` | STM32 timer, 80 MHz, limit `0xFFFF`, `-> nvic@29` / `nvic@54` |
//! | `usart3` | `0x40004800` | `Usart` (80 MHz) `IRQ -> nvic@39` |
//! | `can1` | `0x40006400` | `StmCan`, `[0-3] -> nvic@[19-22]` |
//! | `i2c1` | `0x40005400` | `Stm32F7I2c`, `EventInterrupt -> nvic@31`, `ErrorInterrupt -> nvic@32` |
//! | `rng` | `0x50060800` | `Rng` (F7) `-> nvic@80` |
//! | `rtc`, `iwdg` | `0x40002800`, `0x40003000` | RTC (wakeup 32 000 Hz), IWDG (32 kHz) |
//! | `lcd` | `0x60000000` | `NGCParallelLCD`, `TE -> gpioD@3 | exti@3`, `gpioB 4 -> lcd@0` |
//! | `timer2`, `timer15` | `0x40000000`, `0x40014000` | arithmetic-PWM timers, `-> nvic@28` / `nvic@24` |
//! | `crc` | `0x40023000` | `Crc` (F0, configurable polynomial) |
//! | `adc` | `0x50040000` | `NgcAdc` (sample value 400) |
//! | `buttons` | `0x61000200` | `NGCHandsetButtons` on `timer3`: `PE3 -> gpioE@3 | timer3@0`, `PE5 -> gpioE@5 | timer3@2` |
//! | `outputTelemetry` | `0x61000300` | `NGCBoardTelemetry` (handset), `gpioB 15 -> outputTelemetry@0` |

use crate::board::{Board, BoardConfig};
use crate::fixtures::models;
use crate::firmware::Firmware;
use crate::models::adc_handset::NgcAdc;
use crate::models::clock_control::{self, NgcClockControl};
use armv7m::Cpu;
use emu_core::{ArrayMemory, PeriphId, Width};
use stm32::combined_input::{self, CombinedInput};
use stm32::crc::{Crc, Stm32Series};
use stm32::exti::{self, Exti};
use stm32::gpio::{self, Gpio, GpioConfig};
use stm32::i2c::{self, Stm32F7I2c};
use stm32::rng::{self, Rng};
use stm32::usart::{self, Usart};

/// Machine name used in logs, CAN trace lines and UART capture channel names.
pub const NAME: &str = "ngc-handset";

/// `handset.resc`: `cpu VectorTableOffset`, `cpu SP`, `cpu PC`. `INITIAL_SP` and `RESET_PC` are the TRITON image's
/// vectors; the board boots from the vectors of the firmware it is given.
pub const VECTOR_TABLE: u32 = 0x0800_4000;
pub const INITIAL_SP: u32 = 0x2001_8000;
pub const RESET_PC: u32 = 0x0800_8410;
/// `handset.resc`: `sysbus WriteDoubleWord 0x40006400 0x10000` (CAN MCR), before the first instruction.
pub const CAN_MCR_ADDRESS: u32 = 0x4000_6400;
pub const CAN_MCR_BOOT_VALUE: u32 = 0x0001_0000;
/// Default `adc sampleValue` (a synthetic board-ID input; the physical revision is unverified).
pub const DEFAULT_ADC_SAMPLE: u32 = 400;

/// Peripheral clock of the STM32 timers and the USART (`frequency: 80000000`).
pub const PERIPHERAL_HZ: u64 = 80_000_000;

/// Identifiers of the handset's peripherals inside its `MachineCore`, named like the `.repl` entries.
#[derive(Clone, Copy, Debug)]
pub struct HandsetIds {
    pub rcc: PeriphId,
    pub pwr: PeriphId,
    pub flash_control: PeriphId,
    pub fmc: PeriphId,
    pub syscfg: PeriphId,
    pub exti: PeriphId,
    pub exti5to9: PeriphId,
    pub exti10to15: PeriphId,
    /// `gpioA`..`gpioH`.
    pub gpio: [PeriphId; 8],
    pub timer3: PeriphId,
    pub timer6: PeriphId,
    pub usart3: PeriphId,
    pub can1: PeriphId,
    pub i2c1: PeriphId,
    pub rng: PeriphId,
    pub rtc: PeriphId,
    pub iwdg: PeriphId,
    pub lcd: PeriphId,
    pub timer2: PeriphId,
    pub timer15: PeriphId,
    pub crc: PeriphId,
    pub adc: PeriphId,
    pub buttons: PeriphId,
    pub telemetry: PeriphId,
}

/// Build-time options of the handset.
#[derive(Clone, Copy, Debug)]
pub struct HandsetOptions {
    /// `adc SampleValue` (runner `--adc-sample`, default 400).
    pub adc_sample: u32,
    /// `connector Connect can1 ngcCAN` was done (dual run): the controller has a `FrameSent` subscriber.
    pub can_link: bool,
}

impl Default for HandsetOptions {
    fn default() -> Self {
        Self { adc_sample: DEFAULT_ADC_SAMPLE, can_link: false }
    }
}

/// The assembled handset: its board (CPU + machine) and the peripheral ids.
pub struct HandsetBoard {
    pub board: Board<Cpu>,
    pub ids: HandsetIds,
}

fn mapped(board: &mut Board<Cpu>, base: u32, size: u32, name: &str, peripheral: Box<dyn emu_core::Peripheral>) -> Result<PeriphId, String> {
    board.add_mapped(base, size, peripheral).map_err(|e| format!("{NAME}: cannot map {name} at 0x{base:08X}: {e}"))
}

fn link(result: Result<(), emu_core::MapError>, what: &str) -> Result<(), String> {
    result.map_err(|e| format!("{NAME}: cannot connect {what}: {e}"))
}

impl HandsetBoard {
    /// Assembles the platform and boots it like `handset.resc`: firmware span loaded into flash, CAN MCR
    /// written through the bus, VTOR / SP / PC set. No instruction has run yet.
    pub fn new(firmware: &Firmware, options: HandsetOptions) -> Result<HandsetBoard, String> {
        let mut board = Board::new(BoardConfig::new(NAME));
        let ids = assemble(&mut board, options)?;
        let mut handset = HandsetBoard { board, ids };
        handset.boot(firmware, options)?;
        Ok(handset)
    }

    fn boot(&mut self, firmware: &Firmware, options: HandsetOptions) -> Result<(), String> {
        // `sysbus LoadBinary ... 0x08004000`: the span goes into the zero-initialised flash.
        self.board.load(firmware.span_base, firmware.bin()).map_err(|e| format!("{NAME}: {e}"))?;
        debug_assert_eq!(firmware.span_base, VECTOR_TABLE);
        if options.can_link {
            // `connector Connect can1 ngcCAN` gives the controller a `FrameSent` subscriber.
            self.board.get_mut::<stm32::can::StmCan>(self.ids.can1).ok_or("handset can1 missing")?.set_frame_sink_attached(true);
        }
        // Renode's bxCAN sleep/init interaction differs from the L4 boot sequence: start the model awake.
        self.board.bus_write(CAN_MCR_ADDRESS, Width::Word, CAN_MCR_BOOT_VALUE);
        // `handset.resc` sets SP and PC from the image's own vector table (TRITON: `INITIAL_SP` / `RESET_PC`).
        self.board.cpu.set_vtor(VECTOR_TABLE);
        self.board.cpu.set_sp(firmware.initial_sp());
        self.board.cpu.set_pc(firmware.reset_pc());
        Ok(())
    }

    pub fn name(&self) -> &'static str {
        NAME
    }

    /// Address of the next instruction.
    pub fn pc(&self) -> u32 {
        self.board.cpu.pc()
    }

    /// Executed instructions (`cpu ExecutedInstructions`).
    pub fn instructions(&self) -> u64 {
        self.board.cpu.instructions()
    }

    /// Port `A..H` (`0..8`).
    pub fn gpio(&self, port: usize) -> Option<&Gpio> {
        self.board.get::<Gpio>(*self.ids.gpio.get(port)?)
    }
}

/// Creates every peripheral and wires the board. Returns the ids.
fn assemble(board: &mut Board<Cpu>, options: HandsetOptions) -> Result<HandsetIds, String> {
    // --- peripherals, in `.repl` order -------------------------------------------------------
    let rcc = mapped(board, 0x4002_1000, clock_control::SIZE, "rcc", Box::new(NgcClockControl::new("rcc")))?;
    let pwr = mapped(board, 0x4000_7000, 0x400, "pwr", Box::new(ArrayMemory::new("pwr", 0x400)))?;
    let flash_control = mapped(board, 0x4002_2000, 0x400, "flashControl", Box::new(ArrayMemory::new("flashControl", 0x400)))?;
    let fmc = mapped(board, 0xA000_0000, 0x1000, "fmc", Box::new(ArrayMemory::new("fmc", 0x1000)))?;
    let syscfg = mapped(board, 0x4001_0000, 0x400, "syscfg", Box::new(ArrayMemory::new("syscfg", 0x400)))?;
    let exti = mapped(board, 0x4001_0400, exti::SIZE, "exti", Box::new(Exti::new("exti", 24)))?;
    let exti5to9 = board.add_peripheral(Box::new(CombinedInput::new("exti5to9", 5)));
    let exti10to15 = board.add_peripheral(Box::new(CombinedInput::new("exti10to15", 6)));

    let gpio_config = |mode_reset: u32| GpioConfig::default().with_alternate_functions(16).with_mode_reset(mode_reset);
    let mut gpio_ids = Vec::with_capacity(8);
    for (index, letter) in "ABCDEFGH".chars().enumerate() {
        let mode_reset = match letter {
            'A' => GpioConfig::PORT_A_MODE_RESET,
            'B' => GpioConfig::PORT_B_MODE_RESET,
            _ => 0,
        };
        let name = format!("gpio{letter}");
        let base = 0x4800_0000 + 0x400 * index as u32;
        let id = mapped(board, base, gpio::SIZE, &name, Box::new(Gpio::new(name.clone(), gpio_config(mode_reset))))?;
        gpio_ids.push(id);
    }
    let gpio: [PeriphId; 8] = gpio_ids.try_into().map_err(|_| "gpio ids".to_string())?;

    let timer3 = mapped(board, 0x4000_0400, 0x400, "timer3", models::timer("timer3", PERIPHERAL_HZ, 0xFFFF))?;
    let timer6 = mapped(board, 0x4000_1000, 0x400, "timer6", models::timer("timer6", PERIPHERAL_HZ, 0xFFFF))?;
    let usart3 = mapped(board, 0x4000_4800, 0x400, "usart3", Box::new(Usart::new("usart3", PERIPHERAL_HZ as u32)))?;
    let can1 = mapped(board, 0x4000_6400, 0x400, "can1", Box::new(stm32::can::StmCan::new("can1")))?;
    let i2c1 = mapped(board, 0x4000_5400, i2c::I2C_SIZE, "i2c1", Box::new(Stm32F7I2c::new("i2c1")))?;
    let rng = mapped(board, 0x5006_0800, rng::SIZE, "rng", Box::new(Rng::new("rng", Stm32Series::F7)))?;
    let rtc = mapped(board, 0x4000_2800, 0x400, "rtc", models::rtc("rtc", 32_000))?;
    let iwdg = mapped(board, 0x4000_3000, 0x400, "iwdg", models::iwdg("iwdg", 32_000))?;
    let lcd = mapped(board, 0x6000_0000, models::LCD_SIZE, "lcd", models::lcd("lcd"))?;
    // `Timers.NGCLazyPwmTimer` in the .repl: the runner's arithmetic mode for the unconnected PWM counters.
    let timer2 = mapped(board, 0x4000_0000, 0x400, "timer2", models::lazy_pwm_timer("timer2", PERIPHERAL_HZ, 0xFFFF_FFFF))?;
    let timer15 = mapped(board, 0x4001_4000, 0x400, "timer15", models::lazy_pwm_timer("timer15", PERIPHERAL_HZ, 0xFFFF))?;
    let crc = mapped(board, 0x4002_3000, 0x400, "crc", Box::new(Crc::new("crc", Stm32Series::F0, true)))?;
    let adc = mapped(board, 0x5004_0000, 0x400, "adc", Box::new(NgcAdc::new("adc", options.adc_sample)))?;
    let buttons = mapped(board, 0x6100_0200, 0x100, "buttons", models::handset_buttons("buttons", 0x4000_0400))?;
    let telemetry = mapped(board, 0x6100_0300, 0x100, "outputTelemetry", models::telemetry("outputTelemetry", true))?;

    // --- connections, in `.repl` order ---------------------------------------------------------------
    for i in 0..5 {
        link(board.connect_irq(exti, i, 6 + i), "exti[0-4] -> nvic")?;
    }
    for i in 0..5 {
        link(board.connect_input(exti, 5 + i, exti5to9, i), "exti[5-9] -> exti5to9")?;
    }
    for i in 0..6 {
        link(board.connect_input(exti, 10 + i, exti10to15, i), "exti[10-15] -> exti10to15")?;
    }
    link(board.connect_irq(exti5to9, combined_input::OUTPUT_LINE, 23), "exti5to9 -> nvic@23")?;
    link(board.connect_irq(exti10to15, combined_input::OUTPUT_LINE, 40), "exti10to15 -> nvic@40")?;
    link(board.connect_input(gpio[2], 1, exti, 1), "gpioC 1 -> exti@1")?;
    link(board.connect_irq(timer3, models::TIMER_IRQ_LINE, 29), "timer3 -> nvic@29")?;
    link(board.connect_irq(timer6, models::TIMER_IRQ_LINE, 54), "timer6 -> nvic@54")?;
    link(board.connect_irq(usart3, usart::IRQ_LINE, 39), "usart3 -> nvic@39")?;
    for line in 0..4 {
        link(board.connect_irq(can1, line, 19 + line), "can1[0-3] -> nvic[19-22]")?;
    }
    link(board.connect_irq(i2c1, i2c::EVENT_INTERRUPT, 31), "i2c1 EventInterrupt -> nvic@31")?;
    link(board.connect_irq(i2c1, i2c::ERROR_INTERRUPT, 32), "i2c1 ErrorInterrupt -> nvic@32")?;
    link(board.connect_irq(rng, rng::IRQ_LINE, 80), "rng -> nvic@80")?;
    link(board.connect_input(lcd, models::LCD_TE_LINE, gpio[3], 3), "lcd TE -> gpioD@3")?;
    link(board.connect_input(lcd, models::LCD_TE_LINE, exti, 3), "lcd TE -> exti@3")?;
    link(board.connect_input(gpio[1], 4, lcd, 0), "gpioB 4 -> lcd@0")?;
    link(board.connect_irq(timer2, models::TIMER_IRQ_LINE, 28), "timer2 -> nvic@28")?;
    link(board.connect_irq(timer15, models::TIMER_IRQ_LINE, 24), "timer15 -> nvic@24")?;
    link(board.connect_input(buttons, models::BUTTONS_PE3_LINE, gpio[4], 3), "buttons PE3 -> gpioE@3")?;
    link(board.connect_input(buttons, models::BUTTONS_PE3_LINE, timer3, 0), "buttons PE3 -> timer3@0")?;
    link(board.connect_input(buttons, models::BUTTONS_PE5_LINE, gpio[4], 5), "buttons PE5 -> gpioE@5")?;
    link(board.connect_input(buttons, models::BUTTONS_PE5_LINE, timer3, 2), "buttons PE5 -> timer3@2")?;
    link(board.connect_input(gpio[1], 15, telemetry, models::TELEMETRY_VIBRATOR_LINE), "gpioB 15 -> outputTelemetry@0")?;

    Ok(HandsetIds {
        rcc,
        pwr,
        flash_control,
        fmc,
        syscfg,
        exti,
        exti5to9,
        exti10to15,
        gpio,
        timer3,
        timer6,
        usart3,
        can1,
        i2c1,
        rng,
        rtc,
        iwdg,
        lcd,
        timer2,
        timer15,
        crc,
        adc,
        buttons,
        telemetry,
    })
}
