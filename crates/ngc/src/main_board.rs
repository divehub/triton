//! Main 5.8 board assembly: `emulation/main.repl` (every address, size, constructor parameter and
//! connection) plus the boot sequence of `emulation/main.resc`.
//!
//! Peripherals are added in `.repl` declaration order (clock-entry creation order, see `handset.rs`);
//! connections are made afterwards in `.repl` order.
//!
//! | `.repl` entry | address | model |
//! | --- | --- | --- |
//! | `nvic`, `cpu`, `dwt` | `0xE000E000`, `0xE0001000` | inside `armv7m::Cpu` (priorityMask `0xF0`, 80 MHz SysTick and DWT) |
//! | `flash`, `sram1`, `sram2` | `0x08000000` 1 MiB, `0x20000000` 96 KiB, `0x10000000` 32 KiB | `PlainMemory` of the board |
//! | `rcc` | `0x40021000` | `NgcClockControl` |
//! | `pwr`, `flashControl`, `fmc` | `0x40007000`, `0x40022000`, `0xA0000000` (0x1000) | `ArrayMemory` |
//! | `qspi` | `0xA0001000` | `NGCQuadSPI` |
//! | `syscfg` | `0x40010000` | `ArrayMemory` |
//! | `exti`, `exti5to9`, `exti10to15` | `0x40010400`, not mapped | as on the handset, `gpioA 5 -> exti@5` |
//! | `gpioA`..`gpioH` | `0x48000000 + 0x400 * n` | `Gpio` |
//! | `timer4`, `timer6`, `timer7` | `0x40000800`, `0x40001000`, `0x40001400` | STM32 timers, `-> nvic@30` / `54` / `55` |
//! | `usart1`, `usart2`, `uart4`, `uart5` | `0x40013800`, `0x40004400`, `0x40004C00`, `0x40005000` | `Usart` (80 MHz), `IRQ -> nvic@37` / `38` / `52` / `53` |
//! | `can1` | `0x40006400` | `StmCan`, `[0-3] -> nvic@[19-22]` |
//! | `i2c1`, `i2c2` | `0x40005400`, `0x40005800` | `Stm32F7I2c`, `-> nvic@31/32` and `nvic@33/34` |
//! | `eepromStore` | `0xF0001000` (2048 bytes) | `NgcEepromStore` (diagnostic byte view) |
//! | `eeprom0`..`eeprom7` | `i2c1` `0x50..0x57` | `NgcEepromBank` (bank 0..7) |
//! | `pressure1`, `pressure2` | `i2c1` / `i2c2` `0x76` | `NGCMS5837` |
//! | `rtc`, `iwdg` | `0x40002800`, `0x40003000` | RTC (wakeup 32 768 Hz), IWDG (32 kHz) |
//! | `dma1`, `dma2` | `0x40020000`, `0x40020400` | `Dma`, `[0-6] -> nvic@[11-17]`; `[0-4] -> nvic@[56-60]`, `[5-6] -> nvic@[68-69]` |
//! | `adc` | `0x50040000` | `NGCMainADC`, `DMARequest -> dma1@0`, `IRQ -> nvic@18` |
//! | `crc` | `0x40023000` | `Crc` (F0, configurable polynomial) |
//! | `outputTelemetry` | `0x61000300` | `NGCBoardTelemetry` (main) |

use crate::board::{Board, BoardConfig};
use crate::firmware::Firmware;
use crate::fixtures::models;
use crate::models::clock_control::{self, NgcClockControl};
use crate::models::eeprom::{NgcEepromStore, EEPROM_DIAGNOSTIC_BASE, EEPROM_SIZE};
use armv7m::Cpu;
use emu_core::{ArrayMemory, PeriphId, Width};
use stm32::can::StmCan;
use stm32::combined_input::{self, CombinedInput};
use stm32::crc::{Crc, Stm32Series};
use stm32::dma::Dma;
use stm32::exti::{self, Exti};
use stm32::gpio::{self, Gpio, GpioConfig};
use stm32::i2c::{self, Stm32F7I2c};
use stm32::usart::{self, Usart};

/// Machine name used in logs, CAN trace lines and UART capture channel names.
pub const NAME: &str = "ngc-main";

/// `main.resc`: `cpu VectorTableOffset`, `cpu SP`, `cpu PC`. `INITIAL_SP` and `RESET_PC` are the TRITON image's
/// vectors; the board boots from the vectors of the firmware it is given.
pub const VECTOR_TABLE: u32 = 0x0800_4000;
pub const INITIAL_SP: u32 = 0x2001_8000;
pub const RESET_PC: u32 = 0x0802_13B8;
/// `main.resc`: `sysbus WriteDoubleWord 0x40006400 0x10000` (CAN MCR), before the first instruction.
pub const CAN_MCR_ADDRESS: u32 = 0x4000_6400;
pub const CAN_MCR_BOOT_VALUE: u32 = 0x0001_0000;

/// Peripheral clock of the STM32 timers and the USARTs (`frequency: 80000000`).
pub const PERIPHERAL_HZ: u64 = 80_000_000;

/// I2C address of the pressure sensors (`pressure1`, `pressure2`).
pub const PRESSURE_SENSOR_ADDRESS: u32 = 0x76;

/// Identifiers of the main board's peripherals inside its `MachineCore`, named like the `.repl` entries.
#[derive(Clone, Copy, Debug)]
pub struct MainIds {
    pub rcc: PeriphId,
    pub pwr: PeriphId,
    pub flash_control: PeriphId,
    pub fmc: PeriphId,
    pub qspi: PeriphId,
    pub syscfg: PeriphId,
    pub exti: PeriphId,
    pub exti5to9: PeriphId,
    pub exti10to15: PeriphId,
    /// `gpioA`..`gpioH`.
    pub gpio: [PeriphId; 8],
    pub timer4: PeriphId,
    pub timer6: PeriphId,
    pub timer7: PeriphId,
    pub usart1: PeriphId,
    pub usart2: PeriphId,
    pub uart4: PeriphId,
    pub uart5: PeriphId,
    pub can1: PeriphId,
    pub i2c1: PeriphId,
    pub i2c2: PeriphId,
    pub eeprom_store: PeriphId,
    pub rtc: PeriphId,
    pub iwdg: PeriphId,
    pub dma1: PeriphId,
    pub dma2: PeriphId,
    pub adc: PeriphId,
    pub crc: PeriphId,
    pub telemetry: PeriphId,
}

/// The assembled main board: its board (CPU + machine), the peripheral ids and a shared handle to the
/// EEPROM cells (the same cells the diagnostic window and the I2C banks use).
pub struct MainBoard {
    pub board: Board<Cpu>,
    pub ids: MainIds,
    pub eeprom: NgcEepromStore,
}

fn mapped(board: &mut Board<Cpu>, base: u32, size: u32, name: &str, peripheral: Box<dyn emu_core::Peripheral>) -> Result<PeriphId, String> {
    board.add_mapped(base, size, peripheral).map_err(|e| format!("{NAME}: cannot map {name} at 0x{base:08X}: {e}"))
}

fn link(result: Result<(), emu_core::MapError>, what: &str) -> Result<(), String> {
    result.map_err(|e| format!("{NAME}: cannot connect {what}: {e}"))
}

impl MainBoard {
    /// Assembles the platform and boots it like `main.resc`: firmware span loaded into flash, CAN MCR
    /// written through the bus, VTOR / SP / PC set. No instruction has run yet. Main exists only in the
    /// dual system, so its CAN controller always has the link attached.
    pub fn new(firmware: &Firmware) -> Result<MainBoard, String> {
        let mut board = Board::new(BoardConfig::new(NAME));
        let eeprom = NgcEepromStore::new();
        let ids = assemble(&mut board, &eeprom)?;
        let mut main = MainBoard { board, ids, eeprom };
        main.boot(firmware)?;
        Ok(main)
    }

    fn boot(&mut self, firmware: &Firmware) -> Result<(), String> {
        self.board.load(firmware.span_base, firmware.bin()).map_err(|e| format!("{NAME}: {e}"))?;
        debug_assert_eq!(firmware.span_base, VECTOR_TABLE);
        self.board.get_mut::<StmCan>(self.ids.can1).ok_or("main can1 missing")?.set_frame_sink_attached(true);
        self.board.bus_write(CAN_MCR_ADDRESS, Width::Word, CAN_MCR_BOOT_VALUE);
        // `main.resc` sets SP and PC from the image's own vector table (TRITON: `INITIAL_SP` / `RESET_PC`).
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
fn assemble(board: &mut Board<Cpu>, eeprom: &NgcEepromStore) -> Result<MainIds, String> {
    // --- peripherals, in `.repl` order -------------------------------------------------------
    let rcc = mapped(board, 0x4002_1000, clock_control::SIZE, "rcc", Box::new(NgcClockControl::new("rcc")))?;
    let pwr = mapped(board, 0x4000_7000, 0x400, "pwr", Box::new(ArrayMemory::new("pwr", 0x400)))?;
    let flash_control = mapped(board, 0x4002_2000, 0x400, "flashControl", Box::new(ArrayMemory::new("flashControl", 0x400)))?;
    let fmc = mapped(board, 0xA000_0000, 0x1000, "fmc", Box::new(ArrayMemory::new("fmc", 0x1000)))?;
    let qspi = mapped(board, 0xA000_1000, 0x400, "qspi", models::qspi("qspi"))?;
    let syscfg = mapped(board, 0x4001_0000, 0x400, "syscfg", Box::new(ArrayMemory::new("syscfg", 0x400)))?;
    let exti = mapped(board, 0x4001_0400, exti::SIZE, "exti", Box::new(Exti::new("exti", 24)))?;
    let exti5to9 = board.add_peripheral(Box::new(CombinedInput::new("exti5to9", 5)));
    let exti10to15 = board.add_peripheral(Box::new(CombinedInput::new("exti10to15", 6)));

    let mut gpio_ids = Vec::with_capacity(8);
    for (index, letter) in "ABCDEFGH".chars().enumerate() {
        let mode_reset = match letter {
            'A' => GpioConfig::PORT_A_MODE_RESET,
            'B' => GpioConfig::PORT_B_MODE_RESET,
            _ => 0,
        };
        let name = format!("gpio{letter}");
        let base = 0x4800_0000 + 0x400 * index as u32;
        let config = GpioConfig::default().with_alternate_functions(16).with_mode_reset(mode_reset);
        gpio_ids.push(mapped(board, base, gpio::SIZE, &name, Box::new(Gpio::new(name.clone(), config)))?);
    }
    let gpio: [PeriphId; 8] = gpio_ids.try_into().map_err(|_| "gpio ids".to_string())?;

    let timer4 = mapped(board, 0x4000_0800, 0x400, "timer4", models::timer("timer4", PERIPHERAL_HZ, 0xFFFF))?;
    let timer6 = mapped(board, 0x4000_1000, 0x400, "timer6", models::timer("timer6", PERIPHERAL_HZ, 0xFFFF))?;
    let timer7 = mapped(board, 0x4000_1400, 0x400, "timer7", models::timer("timer7", PERIPHERAL_HZ, 0xFFFF))?;
    let usart1 = mapped(board, 0x4001_3800, 0x400, "usart1", Box::new(Usart::new("usart1", PERIPHERAL_HZ as u32)))?;
    let usart2 = mapped(board, 0x4000_4400, 0x400, "usart2", Box::new(Usart::new("usart2", PERIPHERAL_HZ as u32)))?;
    let uart4 = mapped(board, 0x4000_4C00, 0x400, "uart4", Box::new(Usart::new("uart4", PERIPHERAL_HZ as u32)))?;
    let uart5 = mapped(board, 0x4000_5000, 0x400, "uart5", Box::new(Usart::new("uart5", PERIPHERAL_HZ as u32)))?;
    let can1 = mapped(board, 0x4000_6400, 0x400, "can1", Box::new(StmCan::new("can1")))?;
    let i2c1 = mapped(board, 0x4000_5400, i2c::I2C_SIZE, "i2c1", Box::new(Stm32F7I2c::new("i2c1")))?;
    let i2c2 = mapped(board, 0x4000_5800, i2c::I2C_SIZE, "i2c2", Box::new(Stm32F7I2c::new("i2c2")))?;
    let eeprom_store = mapped(board, EEPROM_DIAGNOSTIC_BASE, EEPROM_SIZE as u32, "eepromStore", Box::new(eeprom.clone()))?;
    // eeprom0..eeprom7 @ i2c1 0x50..0x57 and pressure1 @ i2c1 0x76 / pressure2 @ i2c2 0x76 are I2C targets
    // (not peripherals of the machine): they register with their controller in `.repl` order.
    eeprom
        .attach_banks(board.get_mut::<Stm32F7I2c>(i2c1).ok_or("main i2c1 missing")?)
        .map_err(|e| format!("{NAME}: cannot attach the EEPROM banks to i2c1: {e}"))?;
    for (controller, name) in [(i2c1, "pressure1"), (i2c2, "pressure2")] {
        if let Some(sensor) = models::pressure_sensor(name) {
            board
                .get_mut::<Stm32F7I2c>(controller)
                .ok_or("main i2c missing")?
                .attach(PRESSURE_SENSOR_ADDRESS, sensor)
                .map_err(|e| format!("{NAME}: cannot attach {name}: {e}"))?;
        }
    }
    let rtc = mapped(board, 0x4000_2800, 0x400, "rtc", models::rtc("rtc", 32_768))?;
    let iwdg = mapped(board, 0x4000_3000, 0x400, "iwdg", models::iwdg("iwdg", 32_000))?;
    let dma1 = mapped(board, 0x4002_0000, 0x400, "dma1", Box::new(Dma::new("dma1")))?;
    let dma2 = mapped(board, 0x4002_0400, 0x400, "dma2", Box::new(Dma::new("dma2")))?;
    let adc = mapped(board, 0x5004_0000, 0x400, "adc", models::main_adc("adc"))?;
    let crc = mapped(board, 0x4002_3000, 0x400, "crc", Box::new(Crc::new("crc", Stm32Series::F0, true)))?;
    let telemetry = mapped(board, 0x6100_0300, 0x100, "outputTelemetry", models::telemetry("outputTelemetry", false))?;

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
    link(board.connect_input(gpio[0], 5, exti, 5), "gpioA 5 -> exti@5")?;
    link(board.connect_irq(timer4, models::TIMER_IRQ_LINE, 30), "timer4 -> nvic@30")?;
    link(board.connect_irq(timer6, models::TIMER_IRQ_LINE, 54), "timer6 -> nvic@54")?;
    link(board.connect_irq(timer7, models::TIMER_IRQ_LINE, 55), "timer7 -> nvic@55")?;
    link(board.connect_irq(usart1, usart::IRQ_LINE, 37), "usart1 -> nvic@37")?;
    link(board.connect_irq(usart2, usart::IRQ_LINE, 38), "usart2 -> nvic@38")?;
    link(board.connect_irq(uart4, usart::IRQ_LINE, 52), "uart4 -> nvic@52")?;
    link(board.connect_irq(uart5, usart::IRQ_LINE, 53), "uart5 -> nvic@53")?;
    for line in 0..4 {
        link(board.connect_irq(can1, line, 19 + line), "can1[0-3] -> nvic[19-22]")?;
    }
    link(board.connect_irq(i2c1, i2c::EVENT_INTERRUPT, 31), "i2c1 EventInterrupt -> nvic@31")?;
    link(board.connect_irq(i2c1, i2c::ERROR_INTERRUPT, 32), "i2c1 ErrorInterrupt -> nvic@32")?;
    link(board.connect_irq(i2c2, i2c::EVENT_INTERRUPT, 33), "i2c2 EventInterrupt -> nvic@33")?;
    link(board.connect_irq(i2c2, i2c::ERROR_INTERRUPT, 34), "i2c2 ErrorInterrupt -> nvic@34")?;
    for line in 0..7 {
        link(board.connect_irq(dma1, line, 11 + line), "dma1[0-6] -> nvic[11-17]")?;
    }
    for line in 0..5 {
        link(board.connect_irq(dma2, line, 56 + line), "dma2[0-4] -> nvic[56-60]")?;
    }
    for line in 5..7 {
        link(board.connect_irq(dma2, line, 63 + line), "dma2[5-6] -> nvic[68-69]")?;
    }
    link(board.connect_input(adc, models::MAIN_ADC_DMA_REQUEST_LINE, dma1, 0), "adc DMARequest -> dma1@0")?;
    link(board.connect_irq(adc, models::MAIN_ADC_IRQ_LINE, 18), "adc IRQ -> nvic@18")?;

    Ok(MainIds {
        rcc,
        pwr,
        flash_control,
        fmc,
        qspi,
        syscfg,
        exti,
        exti5to9,
        exti10to15,
        gpio,
        timer4,
        timer6,
        timer7,
        usart1,
        usart2,
        uart4,
        uart5,
        can1,
        i2c1,
        i2c2,
        eeprom_store,
        rtc,
        iwdg,
        dma1,
        dma2,
        adc,
        crc,
        telemetry,
    })
}
