//! Runner fixtures (`emulation/run_emulator.py` parity): defaults, ranges and constants of the
//! viewer's controls, the power-gate / standby poll conditions, and the validated sensor inputs.
//!
//! The functions here are pure with respect to the system loop (`system.rs` decides when to call
//! them): they read side-effect-free board state through `Board::peek` and never run guest code.

use crate::board::Board;
use armv7m::Cpu;
use emu_core::{Json, Time, Width, TICKS_PER_MILLISECOND};

/// The runner executes in 50 virtual-ms intervals (`run_interval(0.05)`) and polls the power gate and
/// the standby request after each one.
pub const POLL_INTERVAL: Time = 50 * TICKS_PER_MILLISECOND;

/// Main `GPIOE_ODR`; bit 3 (PE3) is the inferred handset supply enable (`run_interval`).
pub const PE3_ODR_ADDRESS: u32 = 0x4800_1014;
pub const PE3_MASK: u32 = 8;
/// Main `PWR_CR1`: `& 7 == 3` is LPMS = standby (with SCB.SCR.SLEEPDEEP set the firmware has requested standby).
pub const PWR_CR1_ADDRESS: u32 = 0x4000_7000;
pub const PWR_CR1_LPMS_MASK: u32 = 7;
pub const PWR_CR1_LPMS_STANDBY: u32 = 3;
/// `SCB.SCR`, bit 2 = SLEEPDEEP.
pub const SCB_SCR_ADDRESS: u32 = 0xE000_ED10;
pub const SCB_SCR_SLEEPDEEP: u32 = 4;
/// Handset program counter of the interrupts-disabled error loop: the runner stops with an error.
pub const TERMINAL_HANDLER_PC: u32 = 0x0800_598E;

/// Handset wake fixture (hardware fixture: existing RTC backup plus standby exit from the handset wake
/// input; the unchanged firmware classifies wake cause 3): main `RTC.BKP1R`, `PWR.SR1`, `RCC.CSR`.
pub const WAKE_FIXTURE: [(u32, u32); 3] = [(0x4000_2854, 0x32F0), (0x4000_7010, 0x104), (0x4002_1094, 0)];

/// Handset RAM byte holding the display orientation; 2 keeps the Up/Down masks, any other value swaps them.
pub const HANDSET_ORIENTATION_ADDRESS: u32 = 0x2000_0740;
/// Main RAM byte `mainBatteryReady` (`snapshot()` reads it as the "battery ready" flag).
pub const MAIN_BATTERY_READY_ADDRESS: u32 = 0x2000_42A1;
/// Application variables the benchmark and the reference checkpoints read (main).
pub const MAIN_WAKE_CAUSE_ADDRESS: u32 = 0x2000_4388;
pub const MAIN_SCREEN_MODE_ADDRESS: u32 = 0x2000_438D;
pub const MAIN_MODE_ADDRESS: u32 = 0x2000_24B2;
pub const MAIN_HAL_TICK_ADDRESS: u32 = 0x2000_4A6C;
pub const MAIN_PRESSURE_ADDRESS: u32 = 0x2000_4378;
pub const MAIN_TEMPERATURE_ADDRESS: u32 = 0x2000_4360;
/// FreeRTOS current-TCB pointers (PendSV literal loads in the paired images).
pub const MAIN_CURRENT_TCB_ADDRESS: u32 = 0x2000_5708;
pub const HANDSET_CURRENT_TCB_ADDRESS: u32 = 0x2000_13FC;

/// `ICSR`, `CFSR`, `HFSR`.
pub const FAULT_REGISTERS: [(&str, u32); 3] = [("ICSR", 0xE000_ED04), ("CFSR", 0xE000_ED28), ("HFSR", 0xE000_ED2C)];

/// Serial-number fixture: the EEPROM must be initialized (validity marker) before the serial is changed.
pub const EEPROM_VALIDITY_OFFSET: u32 = 254;
pub const EEPROM_VALIDITY_MARKER: u8 = 0xA3;

/// HUD channel ids of the main board (`LED_IDS`).
pub const LED_IDS: [&str; 3] = ["main-hud-1", "main-hud-2", "main-hud-3"];

/// `(board, peripheral, label)` of the UART capture channels, in the runner's attachment order.
pub const UART_CHANNELS: [(&str, &str, &str); 5] = [
    ("main", "uart4", "Main UART4 \u{b7} diagnostic console (115200 baud)"),
    ("main", "usart1", "Main USART1 \u{b7} Bluetooth transport"),
    ("main", "usart2", "Main USART2 \u{b7} external transport"),
    ("main", "uart5", "Main UART5 \u{b7} IRDA transport"),
    ("handset", "usart3", "Handset USART3 \u{b7} role unverified"),
];

/// Side-effect-free read of a 32-bit word of a board (plain memory, peripheral `peek` or the core's PPB).
pub fn peek32(board: &Board<Cpu>, address: u32) -> Option<u32> {
    board.peek(address, Width::Word)
}

/// `run_interval`: main `GPIOE_ODR & 8` releases the handset (the inferred main PE3 supply enable).
pub fn handset_supply_enabled(main: &Board<Cpu>) -> bool {
    peek32(main, PE3_ODR_ADDRESS).is_some_and(|odr| odr & PE3_MASK != 0)
}

/// `run_interval`: main PWR CR1 `& 7 == 3` and `SCB.SCR & 4`: the firmware has requested standby.
pub fn standby_requested(main: &Board<Cpu>) -> bool {
    let power = peek32(main, PWR_CR1_ADDRESS).unwrap_or(0);
    let deep = peek32(main, SCB_SCR_ADDRESS).unwrap_or(0);
    power & PWR_CR1_LPMS_MASK == PWR_CR1_LPMS_STANDBY && deep & SCB_SCR_SLEEPDEEP != 0
}

// ---- sensor inputs (`INPUT_DEFAULTS` / `INPUT_RANGES`) -------------------------------------------

/// The viewer's sensor controls of the dual run (`Emulator.inputs`). Analog values are doubles like the
/// runner's (`float(value)`), the counters are integers.
#[derive(Clone, Debug, PartialEq)]
pub struct Inputs {
    pub battery_mv: [f64; 2],
    pub oxygen_mv: [f64; 3],
    pub pressure_mbar: [f64; 2],
    pub temperature_c: [f64; 2],
    pub acquisition_enabled: bool,
    pub acquisition_delay_us: u32,
    pub noise_amplitude_raw: u32,
    pub noise_seed: u32,
    pub pressure_maximum_timing: bool,
}

impl Default for Inputs {
    fn default() -> Self {
        Inputs::defaults()
    }
}

/// Python's `float(value)` for a JSON value: booleans and integers convert, numeric strings parse (surrounding
/// white space is ignored), everything else raises with CPython's message.
pub fn python_float(value: &Json) -> Result<f64, String> {
    match value {
        Json::Bool(flag) => Ok(f64::from(u8::from(*flag))),
        Json::Int(_) | Json::UInt(_) | Json::Float(_) => Ok(value.as_f64().unwrap_or(f64::NAN)),
        Json::Str(text) => text.trim().parse::<f64>().map_err(|_| format!("could not convert string to float: '{text}'")),
        other => {
            let name = match other {
                Json::Null => "NoneType",
                Json::Array(_) => "list",
                _ => "dict",
            };
            Err(format!("float() argument must be a string or a real number, not '{name}'"))
        }
    }
}

/// `(key, lower, upper)` of `INPUT_RANGES` for the numeric inputs.
const RANGES: [(&str, f64, f64); 12] = [
    ("battery1Mv", 0.0, 4200.0),
    ("battery2Mv", 0.0, 4200.0),
    ("oxygen1Mv", 0.0, 250.0),
    ("oxygen2Mv", 0.0, 250.0),
    ("oxygen3Mv", 0.0, 250.0),
    ("pressure1Mbar", 100.0, 30000.0),
    ("pressure2Mbar", 100.0, 30000.0),
    ("temperature1C", -20.0, 85.0),
    ("temperature2C", -20.0, 85.0),
    ("acquisitionDelayUs", 0.0, 10_000_000.0),
    ("noiseAmplitudeRaw", 0.0, 4095.0),
    ("noiseSeed", 1.0, 4_294_967_295.0),
];

/// Battery voltage of a fresh profile (both banks), in millivolts: a charged cell, 4.1 V. A saved `inputs.json` keeps
/// whatever it stores. The range stays 0 to 4200 mV.
pub const DEFAULT_BATTERY_MV: f64 = 4100.0;

/// The battery voltage the Renode runner's `INPUT_DEFAULTS` used (1500 mV) and so every recording of the analysis
/// workspace was made with. Scenarios and tests that compare with those recordings pin it explicitly
/// ([`Inputs::recorded_evidence`]).
pub const RECORDED_BATTERY_MV: f64 = 1500.0;

impl Inputs {
    /// A fresh profile: batteries [`DEFAULT_BATTERY_MV`] (4100 mV, the runner's earlier default was 1500 mV), oxygen
    /// cells 10 mV, 1013.25 mbar, 20 C, acquisition on, no noise.
    pub fn defaults() -> Inputs {
        Inputs {
            battery_mv: [DEFAULT_BATTERY_MV; 2],
            oxygen_mv: [10.0; 3],
            pressure_mbar: [1013.25; 2],
            temperature_c: [20.0; 2],
            acquisition_enabled: true,
            acquisition_delay_us: 0,
            noise_amplitude_raw: 0,
            noise_seed: 1,
            pressure_maximum_timing: false,
        }
    }

    /// The inputs of the runner recordings: the defaults with both batteries at [`RECORDED_BATTERY_MV`] (1500 mV).
    pub fn recorded_evidence() -> Inputs {
        Inputs { battery_mv: [RECORDED_BATTERY_MV; 2], ..Inputs::defaults() }
    }

    /// `apply_inputs` validation: `updates` is a JSON object whose keys are the input names. All entries
    /// are validated before anything changes (the runner applies nothing on error either).
    pub fn apply_json(&mut self, updates: &Json) -> Result<(), String> {
        let Some(items) = updates.as_object() else {
            return Err("inputs must be an object".to_string());
        };
        let mut values = self.clone();
        for (key, value) in items {
            values.apply_one(key, value)?;
        }
        *self = values;
        Ok(())
    }

    fn apply_one(&mut self, key: &str, value: &Json) -> Result<(), String> {
        if key == "acquisitionEnabled" || key == "pressureMaximumTiming" {
            let Json::Bool(flag) = value else {
                return Err(format!("{key} must be a boolean"));
            };
            if key == "acquisitionEnabled" {
                self.acquisition_enabled = *flag;
            } else {
                self.pressure_maximum_timing = *flag;
            }
            return Ok(());
        }
        let Some(&(_, lower, upper)) = RANGES.iter().find(|(name, _, _)| *name == key) else {
            return Err(format!("Unknown input: {key}"));
        };
        let number = python_float(value)?;
        if !number.is_finite() || number < lower || number > upper {
            return Err(format!("{key} must be between {} and {}", lower as i64, upper as i64));
        }
        let integer = matches!(key, "acquisitionDelayUs" | "noiseAmplitudeRaw" | "noiseSeed");
        if integer && number != number.trunc() {
            return Err(format!("{key} must be an integer"));
        }
        match key {
            "battery1Mv" => self.battery_mv[0] = number,
            "battery2Mv" => self.battery_mv[1] = number,
            "oxygen1Mv" => self.oxygen_mv[0] = number,
            "oxygen2Mv" => self.oxygen_mv[1] = number,
            "oxygen3Mv" => self.oxygen_mv[2] = number,
            "pressure1Mbar" => self.pressure_mbar[0] = number,
            "pressure2Mbar" => self.pressure_mbar[1] = number,
            "temperature1C" => self.temperature_c[0] = number,
            "temperature2C" => self.temperature_c[1] = number,
            "acquisitionDelayUs" => self.acquisition_delay_us = number as u32,
            "noiseAmplitudeRaw" => self.noise_amplitude_raw = number as u32,
            "noiseSeed" => self.noise_seed = number as u32,
            _ => unreachable!("every ranged key is handled"),
        }
        Ok(())
    }

    /// The runner's `inputs` object (keys in `INPUT_DEFAULTS` order).
    pub fn to_json(&self) -> Json {
        Json::object()
            .with("battery1Mv", self.battery_mv[0])
            .with("battery2Mv", self.battery_mv[1])
            .with("oxygen1Mv", self.oxygen_mv[0])
            .with("oxygen2Mv", self.oxygen_mv[1])
            .with("oxygen3Mv", self.oxygen_mv[2])
            .with("pressure1Mbar", self.pressure_mbar[0])
            .with("pressure2Mbar", self.pressure_mbar[1])
            .with("temperature1C", self.temperature_c[0])
            .with("temperature2C", self.temperature_c[1])
            .with("acquisitionEnabled", self.acquisition_enabled)
            .with("acquisitionDelayUs", u64::from(self.acquisition_delay_us))
            .with("noiseAmplitudeRaw", u64::from(self.noise_amplitude_raw))
            .with("noiseSeed", u64::from(self.noise_seed))
            .with("pressureMaximumTiming", self.pressure_maximum_timing)
    }
}

/// Mask of the physical button to pulse for a navigation action, mirroring the runner:
/// `up` is mask 1 and `down` mask 2 when the handset orientation byte is 2, swapped otherwise.
pub fn navigation_mask(up: bool, orientation: u8) -> u32 {
    let mask = if up { 1 } else { 2 };
    if orientation != 2 {
        3 - mask
    } else {
        mask
    }
}

/// Constructors (with the `.repl` parameters) and output-line constants of the models the board assembly in
/// `handset.rs` / `main_board.rs` instantiates; the one place that names the model types of the peripheral
/// work packages (timers, RTC, IWDG, LCD, buttons, telemetry, the main-board ADC / QSPI / pressure sensors).
pub(crate) mod models {
    use emu_core::Peripheral;

    /// Output line 0 of an STM32 timer is its interrupt request (the `.repl` `-> nvic@n`).
    pub use stm32::timer::IRQ_LINE as TIMER_IRQ_LINE;
    /// `NGCParallelLCD.Size` (`DataOffset + 2`) and its `TE` output line.
    pub use crate::models::lcd::{SIZE as LCD_SIZE, TE as LCD_TE_LINE};
    /// `NGCHandsetButtons` GPIO outputs (`PE3`, `PE5`) and the telemetry's vibrator-enable input.
    pub use crate::models::buttons::{PE3_LINE as BUTTONS_PE3_LINE, PE5_LINE as BUTTONS_PE5_LINE};
    pub use crate::models::telemetry::VIBRATOR_ENABLE_LINE as TELEMETRY_VIBRATOR_LINE;

    pub use stm32::timer::Scheduling;

    /// `Timers.STM32_Timer`: `frequency` and `initialLimit` of the `.repl`. Plain `STM32_Timer` instances (the main
    /// board's TIM4/TIM6/TIM7, the handset's TIM3/TIM6) kept Renode's stock events in the runner, so every limit is a
    /// machine event ([`Scheduling::Stock`]); the CPU chunk boundaries they cause are guest-visible.
    pub fn timer(name: &str, frequency: u64, initial_limit: u64) -> Box<dyn Peripheral> {
        Box::new(stm32::timer::Stm32Timer::new(name, frequency, initial_limit as u32).with_scheduling(Scheduling::Stock))
    }

    /// `Timers.NGCLazyPwmTimer` (the handset's TIM2 and TIM15): the runner's default arithmetic mode for unconnected
    /// PWM counters ([`Scheduling::NgcArithmeticPwm`]).
    pub fn lazy_pwm_timer(name: &str, frequency: u64, initial_limit: u64) -> Box<dyn Peripheral> {
        Box::new(stm32::timer::Stm32Timer::new(name, frequency, initial_limit as u32).with_scheduling(Scheduling::NgcArithmeticPwm))
    }

    /// `Timers.STM32F4_RTC`: `wakeupTimerFrequency` 32000 (handset) / 32768 (main).
    pub fn rtc(name: &str, wakeup_frequency: u64) -> Box<dyn Peripheral> {
        Box::new(stm32::rtc::Rtc::new(name, wakeup_frequency))
    }

    /// `Timers.STM32_IndependentWatchdog`: `frequency` 32000, window option on, default prescaler 0.
    pub fn iwdg(name: &str, frequency: u64) -> Box<dyn Peripheral> {
        Box::new(stm32::iwdg::Iwdg::new(name, frequency, true, 0))
    }

    /// `lcd: Video.NGCParallelLCD`, `simulateTE: true`.
    pub fn lcd(name: &str) -> Box<dyn Peripheral> {
        Box::new(crate::models::lcd::NgcParallelLcd::new(name, true))
    }

    /// `buttons: GPIOPort.NGCHandsetButtons { timer: timer3 }`; `timer_base` is where `timer3` is mapped.
    pub fn handset_buttons(name: &str, timer_base: u32) -> Box<dyn Peripheral> {
        Box::new(crate::models::buttons::Buttons::new(name, timer_base))
    }

    /// `outputTelemetry: Miscellaneous.NGCBoardTelemetry { handset: true|false }`.
    pub fn telemetry(name: &str, handset: bool) -> Box<dyn Peripheral> {
        Box::new(crate::models::telemetry::Telemetry::new(name, handset))
    }

    /// Output lines of `NGCMainADC`.
    pub use crate::models::adc_main::{DMA_REQUEST as MAIN_ADC_DMA_REQUEST_LINE, IRQ as MAIN_ADC_IRQ_LINE};

    pub fn main_adc(name: &str) -> Box<dyn Peripheral> {
        Box::new(crate::models::adc_main::NgcMainAdc::with_defaults(name))
    }

    pub fn qspi(name: &str) -> Box<dyn Peripheral> {
        Box::new(crate::models::qspi::NgcQuadSpi::new(name))
    }

    /// `pressure1` / `pressure2`: the MS5837 I2C target at 0x76.
    pub fn pressure_sensor(name: &str) -> Option<Box<dyn stm32::i2c::I2cTarget>> {
        Some(Box::new(crate::models::ms5837::NgcMs5837::with_defaults(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_profile_starts_with_4100_mv_batteries_and_the_recordings_pin_1500_mv() {
        let defaults = Inputs::defaults();
        assert_eq!(defaults.battery_mv, [4100.0, 4100.0]);
        assert_eq!(Inputs::default(), defaults);
        let recorded = Inputs::recorded_evidence();
        assert_eq!(recorded.battery_mv, [1500.0, 1500.0]);
        // Nothing but the batteries differs from the defaults.
        assert_eq!(Inputs { battery_mv: defaults.battery_mv, ..recorded }, defaults);
        // The range is unchanged: 0 to 4200 mV, both limits inclusive.
        let mut inputs = Inputs::defaults();
        inputs.apply_json(&Json::parse("{\"battery1Mv\": 4200, \"battery2Mv\": 0}").unwrap()).unwrap();
        assert_eq!(inputs.battery_mv, [4200.0, 0.0]);
        let error = inputs.apply_json(&Json::parse("{\"battery1Mv\": 4201}").unwrap()).unwrap_err();
        assert_eq!(error, "battery1Mv must be between 0 and 4200");
    }
}
