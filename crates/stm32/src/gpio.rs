// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/GPIOPort/STM32_GPIOPort.cs and
// src/Emulator/Main/Peripherals/GPIOPort/BaseGPIOPort.cs (MIT License, Copyright (c) Antmicro).

//! `GPIOPort.STM32_GPIOPort`: one 16-pin STM32 GPIO port (`gpioA` ... `gpioH` of both boards).
//!
//! # Lines
//!
//! * **Output lines 0..=15** are Renode's `Connections[pin]`: the level of pin `n` as a signal. They
//!   change on every `ODR`/`BSRR`/`BRR` write that changes a pin and on every input (`5 -> exti@5`,
//!   `4 -> lcd@0`, `15 -> outputTelemetry@0`). Like Renode, the line follows the stored pin state
//!   regardless of the pin mode.
//! * **Input lines 0..=15** are `BaseGPIOPort.OnGPIO(n, level)`: an external driver (`buttons.PE3 ->
//!   gpioE@3`, `Board::set_input`) sets the pin state, which `IDR` (and, because Renode keeps a single
//!   `State[]` for both, `ODR`) then reports, and the change is forwarded to the output line.
//! * **Input lines `AF_INPUT_BASE + 16 * pin + af`** are the per-pin alternate-function receivers of
//!   `ILocalGPIOReceiver.GetLocalReceiver(pin)`: a signal from alternate function `af` of pin `pin` reaches
//!   the pin only while the pin is in alternate-function mode and `af` is its selected function
//!   (optionally inverted, `GpioConfig::inverted_af_pins`). The NGC platforms do not use them.
//!
//! # Registers (offsets in [`reg`])
//!
//! `MODER`, `OSPEEDR`, `PUPDR`, `AFRL`, `AFRH` are views of per-pin arrays; `IDR` and `ODR` both return the
//! pin state; `BSRR`/`BRR` are write-only; `OTYPER` has no storage (reads 0, writes only log); `LCKR`
//! implements the 3-write + 2-read lock sequence. Everything else (`0x2C`, ...) is "Unhandled read/write"
//! and reads 0 (observed on `gpioA`/`gpioC` of both boards). All registers are built on the Renode register
//! framework port in [`regfw`].
//!
//! // Renode parity: the register framework keeps an *underlying value* per register and detects changes
//! against it, so `MODER`/`OSPEEDR`/`PUPDR`/`AFRx` writes that equal the stale underlying value are not
//! applied. After a reset the underlying values are 0 while the pin arrays hold the reset values
//! (`modeResetValue`), so writing 0 to `MODER` without reading it first changes nothing. Firmware that
//! does read-modify-write (HAL, LL) refreshes the value by reading first. Reads through [`Gpio::peek`]
//! never refresh it.
//!
//! The bus accepts only 32-bit accesses plus halfword accesses translated to a word read-modify-write
//! (`[AllowedTranslations(WordToDoubleWord)]`), so a halfword store to `BSRR` reads `BSRR` (0) first.

// The engine is a shared utility of this crate's register-framework models (EXTI, CRC, RNG, ...); it
// implements Renode's full field-mode set, of which each model uses a subset.
#[allow(dead_code)]
pub(crate) mod regfw;

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, Translations, View, Width};
use regfw::{Hooks, Mode, Model, RegisterBuilder, RegisterFile};

/// Size of the register window of a port (`<0x48000000, +0x400>` in the `.repl` files).
pub const SIZE: u32 = 0x400;

/// Number of pins of a port (`BaseGPIOPort` is constructed with 16 connections).
pub const NUMBER_OF_PINS: usize = 16;

/// First numbered input line of the alternate-function receivers (see the module documentation).
pub const AF_INPUT_BASE: u32 = 16;

/// Input line that delivers alternate function `af` of pin `pin` (`.repl`: `source -> gpioX#pin@af`).
pub const fn af_input_line(pin: u32, af: u32) -> u32 {
    AF_INPUT_BASE + pin * 16 + af
}

/// Register offsets.
pub mod reg {
    pub const MODER: u32 = 0x00;
    pub const OTYPER: u32 = 0x04;
    pub const OSPEEDR: u32 = 0x08;
    pub const PUPDR: u32 = 0x0C;
    pub const IDR: u32 = 0x10;
    pub const ODR: u32 = 0x14;
    pub const BSRR: u32 = 0x18;
    pub const LCKR: u32 = 0x1C;
    pub const AFRL: u32 = 0x20;
    pub const AFRH: u32 = 0x24;
    pub const BRR: u32 = 0x28;
}

// Register numbering inside the register file (callbacks identify registers by these).
const R_MODER: usize = 0;
const R_OTYPER: usize = 1;
const R_OSPEEDR: usize = 2;
const R_PUPDR: usize = 3;
const R_IDR: usize = 4;
const R_ODR: usize = 5;
const R_BSRR: usize = 6;
const R_LCKR: usize = 7;
const R_AFRL: usize = 8;
const R_AFRH: usize = 9;
const R_BRR: usize = 10;

/// `LCKR.LCKK`.
const LCKK_BIT: u32 = 1 << 16;

/// Constructor parameters of `STM32_GPIOPort` (`numberOfAFs`, `modeResetValue`, `outputSpeedResetValue`,
/// `pullUpPullDownResetValue`, `invertedAFPins`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpioConfig {
    /// Number of alternate functions per pin, 1..=16 (`numberOfAFs`, 16 on both boards).
    pub number_of_afs: u32,
    /// Reset value of `MODER` (2 bits per pin): `0xABFFFFFF` for port A, `0xFFFFFEBF` for port B, 0 otherwise.
    pub mode_reset_value: u32,
    /// Reset value of `OSPEEDR`.
    pub output_speed_reset_value: u32,
    /// Reset value of `PUPDR`.
    pub pull_up_pull_down_reset_value: u32,
    /// `invertedAFPins`: `(pin, [af, ...])`: signals of those alternate functions are inverted on that pin.
    pub inverted_af_pins: Vec<(u32, Vec<u32>)>,
}

impl GpioConfig {
    /// `modeResetValue` of port A in `handset.repl`/`main.repl` (SWD pins in alternate function mode).
    pub const PORT_A_MODE_RESET: u32 = 0xABFF_FFFF;
    /// `modeResetValue` of port B in `handset.repl`/`main.repl`.
    pub const PORT_B_MODE_RESET: u32 = 0xFFFF_FEBF;

    pub fn with_mode_reset(mut self, value: u32) -> Self {
        self.mode_reset_value = value;
        self
    }

    pub fn with_output_speed_reset(mut self, value: u32) -> Self {
        self.output_speed_reset_value = value;
        self
    }

    pub fn with_pull_up_pull_down_reset(mut self, value: u32) -> Self {
        self.pull_up_pull_down_reset_value = value;
        self
    }

    pub fn with_alternate_functions(mut self, count: u32) -> Self {
        self.number_of_afs = count;
        self
    }

    pub fn with_inverted_af_pins(mut self, pin: u32, afs: &[u32]) -> Self {
        self.inverted_af_pins.push((pin, afs.to_vec()));
        self
    }

    fn validate(&self) -> Result<(), String> {
        if self.number_of_afs < 1 || self.number_of_afs > 16 {
            return Err("Number of alternate functions can't be lower than 1 or higher than 16".to_string());
        }
        for (pin, afs) in &self.inverted_af_pins {
            if *pin >= NUMBER_OF_PINS as u32 {
                return Err(format!("Pin {pin} out of range [0, {}]", NUMBER_OF_PINS - 1));
            }
            for af in afs {
                if *af >= self.number_of_afs {
                    return Err(format!("Alternate function {af} out of range [0, {}]", self.number_of_afs - 1));
                }
            }
        }
        Ok(())
    }
}

impl Default for GpioConfig {
    fn default() -> Self {
        Self {
            number_of_afs: 16,
            mode_reset_value: 0,
            output_speed_reset_value: 0,
            pull_up_pull_down_reset_value: 0,
            inverted_af_pins: Vec::new(),
        }
    }
}

/// `MODER` pin mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinMode {
    Input = 0,
    Output = 1,
    AlternateFunction = 2,
    Analog = 3,
}

impl PinMode {
    fn from_bits(bits: u8) -> PinMode {
        match bits & 3 {
            0 => PinMode::Input,
            1 => PinMode::Output,
            2 => PinMode::AlternateFunction,
            _ => PinMode::Analog,
        }
    }
}

/// State of the `LCKR` lock key sequence (`LockSequence` in the C# class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockSequence {
    Idle,
    /// LCKK=1 written.
    Step0,
    /// LCKK=0 written.
    Step1,
    /// LCKK=1 written again; the next `LCKR` read arms the lock.
    Step2,
    /// The configuration of the locked pins is frozen until reset.
    Armed,
}

/// Everything except the register file (so register callbacks can borrow it mutably).
struct GpioDev {
    number_of_afs: u32,
    mode_reset: u32,
    output_speed_reset: u32,
    pull_reset: u32,
    /// Bit `af` of `inverted[pin]`: signals of that alternate function are inverted on the pin.
    inverted: [u16; NUMBER_OF_PINS],
    /// `State[]`: the pin levels seen by `IDR` and `ODR` and mirrored on the output lines.
    state: u16,
    mode: [u8; NUMBER_OF_PINS],
    output_speed: [u8; NUMBER_OF_PINS],
    pull: [u8; NUMBER_OF_PINS],
    /// `GPIOAlternateFunction.ActiveFunction` of every pin.
    active_function: [u8; NUMBER_OF_PINS],
    /// Bit `pin`: `GPIOAlternateFunction.IsConnected` (the pin is in alternate-function mode).
    af_connected: u16,
    locked_pins: u16,
    lock: LockSequence,
}

// Warn-once keys.
const KEY_LOCKED: u64 = 1 << 40;
const KEY_AF_RANGE: u64 = 2 << 40;
const KEY_PIN_RANGE: u64 = 3 << 40;

impl GpioDev {
    fn new(config: &GpioConfig) -> Self {
        let mut inverted = [0u16; NUMBER_OF_PINS];
        for (pin, afs) in &config.inverted_af_pins {
            for af in afs {
                inverted[*pin as usize] |= 1 << af;
            }
        }
        let mut dev = Self {
            number_of_afs: config.number_of_afs,
            mode_reset: config.mode_reset_value,
            output_speed_reset: config.output_speed_reset_value,
            pull_reset: config.pull_up_pull_down_reset_value,
            inverted,
            state: 0,
            mode: [0; NUMBER_OF_PINS],
            output_speed: [0; NUMBER_OF_PINS],
            pull: [0; NUMBER_OF_PINS],
            active_function: [0; NUMBER_OF_PINS],
            af_connected: 0,
            locked_pins: 0,
            lock: LockSequence::Idle,
        };
        dev.reset_state();
        dev
    }

    /// The configuration part of `STM32_GPIOPort.Reset()` (everything but the output lines).
    fn reset_state(&mut self) {
        self.state = 0;
        self.locked_pins = 0;
        self.lock = LockSequence::Idle;
        self.af_connected = 0;
        for pin in 0..NUMBER_OF_PINS {
            self.active_function[pin] = 0;
            self.change_mode(pin, ((self.mode_reset >> (2 * pin)) & 3) as u8);
            self.output_speed[pin] = ((self.output_speed_reset >> (2 * pin)) & 3) as u8;
            self.pull[pin] = ((self.pull_reset >> (2 * pin)) & 3) as u8;
        }
    }

    /// `ChangeMode`: stores the mode and (dis)connects the pin's alternate-function receiver.
    fn change_mode(&mut self, pin: usize, mode: u8) {
        self.mode[pin] = mode;
        if mode == PinMode::AlternateFunction as u8 {
            self.af_connected |= 1 << pin;
        } else {
            self.af_connected &= !(1 << pin);
        }
    }

    /// `WritePin`: stores the level and drives the pin's output line.
    fn write_pin(&mut self, ctx: &mut Ctx<'_>, pin: usize, level: bool) {
        if level {
            self.state |= 1 << pin;
        } else {
            self.state &= !(1 << pin);
        }
        ctx.set_output(pin as u32, level);
    }

    /// `WriteState`: every pin is stored and driven, lowest first (lines only notify on a change).
    fn write_state(&mut self, ctx: &mut Ctx<'_>, value: u16) {
        for pin in 0..NUMBER_OF_PINS {
            self.write_pin(ctx, pin, (value >> pin) & 1 != 0);
        }
    }

    /// `GuardPinAction`: false (with a warning) when the lock is armed and the pin is locked.
    fn guard(&self, ctx: &mut Ctx<'_>, pin: usize, name: &str, name_id: u64) -> bool {
        if self.lock == LockSequence::Armed && self.locked_pins & (1 << pin) != 0 {
            ctx.warn_once(
                KEY_LOCKED | (name_id << 8) | pin as u64,
                format_args!("Ignoring attempt to change {name} configuration of the locked pin #{pin}"),
            );
            return false;
        }
        true
    }

    /// `GPIOAlternateFunction.CheckAFNumber`.
    fn check_af_number(&self, ctx: &mut Ctx<'_>, af: u32) -> bool {
        if af >= self.number_of_afs {
            ctx.error_once(
                KEY_AF_RANGE | u64::from(af),
                format_args!("Alternate function number must be between 0 and {}, but {af} was given instead.", self.number_of_afs - 1),
            );
            return false;
        }
        true
    }

    /// `GPIOAlternateFunction.ActiveFunction` setter.
    fn set_active_function(&mut self, ctx: &mut Ctx<'_>, pin: usize, af: u32) {
        if self.check_af_number(ctx, af) {
            self.active_function[pin] = af as u8;
        }
    }

    /// `GPIOAlternateFunction.OnGPIO`.
    fn af_input(&mut self, ctx: &mut Ctx<'_>, pin: usize, af: u32, level: bool) {
        if !self.check_af_number(ctx, af) || self.af_connected & (1 << pin) == 0 || af != u32::from(self.active_function[pin]) {
            // Valid and silent: alternate-function sources are always connected and always sending.
            return;
        }
        let invert = self.inverted[pin] & (1 << af) != 0;
        self.write_pin(ctx, pin, level ^ invert);
    }

    fn array_register(values: &[u8; NUMBER_OF_PINS], first: usize, bits: u32, count: usize) -> u32 {
        (0..count).fold(0, |acc, i| acc | u32::from(values[first + i]) << (bits * i as u32))
    }
}

impl Model for GpioDev {
    fn provide(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, field: usize, current: u32) -> u32 {
        match reg {
            R_MODER => u32::from(self.mode[field]),
            R_OSPEEDR => u32::from(self.output_speed[field]),
            R_PUPDR => u32::from(self.pull[field]),
            R_IDR | R_ODR => u32::from(self.state),
            R_LCKR => {
                // `LCK` value provider: a read advances the lock sequence.
                if self.lock == LockSequence::Step2 {
                    self.lock = LockSequence::Armed;
                }
                if self.lock != LockSequence::Armed {
                    self.lock = LockSequence::Idle;
                }
                u32::from(self.locked_pins)
            }
            R_AFRL => u32::from(self.active_function[field]),
            R_AFRH => u32::from(self.active_function[field + 8]),
            _ => current,
        }
    }

    fn field_written(&mut self, _regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, field: usize, _old: u32, written: u32) {
        let state = u32::from(self.state);
        match (reg, field) {
            (R_ODR, 0) => self.write_state(ctx, written as u16),
            // GPIOx_BS then GPIOx_BR: a bit present in both halves pulses (set, then reset).
            (R_BSRR, 0) if written != 0 => self.write_state(ctx, (state | written) as u16),
            (R_BSRR, 1) | (R_BRR, 0) if written != 0 => self.write_state(ctx, (state & !written) as u16),
            _ => {}
        }
    }

    fn field_changed(&mut self, _regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, field: usize, _old: u32, new: u32) {
        match reg {
            R_MODER if self.guard(ctx, field, "MODER", 1) => self.change_mode(field, new as u8),
            R_OSPEEDR if self.guard(ctx, field, "OSPEEDR", 2) => self.output_speed[field] = new as u8,
            R_PUPDR if self.guard(ctx, field, "PUPDR0", 3) => self.pull[field] = new as u8,
            R_AFRL if self.guard(ctx, field, "AFSEL", 4) => self.set_active_function(ctx, field, new),
            R_AFRH if self.guard(ctx, field + 8, "AFSEL", 4) => self.set_active_function(ctx, field + 8, new),
            _ => {}
        }
    }

    fn register_written(&mut self, regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, _old: u32, _written: u32) {
        if reg != R_LCKR {
            return;
        }
        // The LCKR write callback: field values are already updated (`pendingLockPins`, `lockBit`).
        let pending = regs.field(R_LCKR, 0) as u16;
        let lock_bit = regs.field(R_LCKR, 1) != 0;
        match self.lock {
            LockSequence::Idle => {
                if !lock_bit {
                    return;
                }
                self.locked_pins = pending;
                self.lock = LockSequence::Step0;
            }
            LockSequence::Step0 => {
                self.lock = if lock_bit || pending != self.locked_pins { LockSequence::Idle } else { LockSequence::Step1 };
            }
            LockSequence::Step1 => {
                self.lock = if !lock_bit || pending != self.locked_pins { LockSequence::Idle } else { LockSequence::Step2 };
            }
            // Ignore the write.
            LockSequence::Armed => {}
            // Writing again before the arming read restarts the sequence (`default:` branch).
            LockSequence::Step2 => self.lock = LockSequence::Idle,
        }
    }
}

fn build_registers() -> RegisterFile {
    let per_pin = Hooks::PROVIDER | Hooks::CHANGE;
    RegisterFile::new(vec![
        // MODER: 16 enum fields.
        RegisterBuilder::new(reg::MODER).fields(0, 2, 16, Mode::READ_WRITE, per_pin),
        // OTYPER: OT0..OT15 are tags (no storage), the upper half is reserved.
        RegisterBuilder::new(reg::OTYPER).tagged_flags("OT", 0, 16).reserved(16, 16),
        // OSPEEDR, PUPDR: 16 enum fields each.
        RegisterBuilder::new(reg::OSPEEDR).fields(0, 2, 16, Mode::READ_WRITE, per_pin),
        RegisterBuilder::new(reg::PUPDR).fields(0, 2, 16, Mode::READ_WRITE, per_pin),
        // IDR: read-only value field, reserved upper half.
        RegisterBuilder::new(reg::IDR).field(0, 16, Mode::READ, Hooks::PROVIDER).reserved(16, 16),
        // ODR: the write callback stores and drives the pins.
        RegisterBuilder::new(reg::ODR).field(0, 16, Mode::READ_WRITE, Hooks::PROVIDER | Hooks::WRITE).reserved(16, 16),
        // BSRR: GPIOx_BS and GPIOx_BR, write-only.
        RegisterBuilder::new(reg::BSRR).field(0, 16, Mode::WRITE, Hooks::WRITE).field(16, 16, Mode::WRITE, Hooks::WRITE),
        // LCKR: LCK (provider), LCKK, reserved, plus the register-level write callback.
        RegisterBuilder::new(reg::LCKR)
            .field(0, 16, Mode::READ_WRITE, Hooks::PROVIDER)
            .field(16, 1, Mode::READ_WRITE, Hooks::NONE)
            .reserved(17, 15)
            .on_write(),
        // AFRL, AFRH: eight 4 bit fields each.
        RegisterBuilder::new(reg::AFRL).fields(0, 4, 8, Mode::READ_WRITE, per_pin),
        RegisterBuilder::new(reg::AFRH).fields(0, 4, 8, Mode::READ_WRITE, per_pin),
        // BRR: write-only GPIOx_BRR, reserved upper half.
        RegisterBuilder::new(reg::BRR).field(0, 16, Mode::WRITE, Hooks::WRITE).reserved(16, 16),
    ])
}

/// `GPIOPort.STM32_GPIOPort`.
pub struct Gpio {
    name: String,
    regs: RegisterFile,
    dev: GpioDev,
}

impl Gpio {
    /// Builds a port; panics on an invalid configuration (Renode throws a `ConstructionException`).
    pub fn new(name: impl Into<String>, config: GpioConfig) -> Self {
        match Self::try_new(name, config) {
            Ok(gpio) => gpio,
            Err(message) => panic!("cannot construct STM32_GPIOPort: {message}"),
        }
    }

    /// Port with the default configuration but the given `modeResetValue`.
    pub fn with_mode_reset(name: impl Into<String>, mode_reset_value: u32) -> Self {
        Self::new(name, GpioConfig::default().with_mode_reset(mode_reset_value))
    }

    pub fn try_new(name: impl Into<String>, config: GpioConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self { name: name.into(), regs: build_registers(), dev: GpioDev::new(&config) })
    }

    // ---- side-effect-free accessors (telemetry, fixtures, snapshots) --------------------

    /// `IDR` and `ODR` (Renode keeps one `State[]` for both).
    pub fn pin_state(&self) -> u16 {
        self.dev.state
    }

    pub fn idr(&self) -> u32 {
        u32::from(self.dev.state)
    }

    pub fn odr(&self) -> u32 {
        u32::from(self.dev.state)
    }

    /// Level of pin `pin` (0..=15).
    pub fn pin(&self, pin: usize) -> bool {
        self.dev.state & (1 << pin) != 0
    }

    pub fn moder(&self) -> u32 {
        GpioDev::array_register(&self.dev.mode, 0, 2, 16)
    }

    pub fn ospeedr(&self) -> u32 {
        GpioDev::array_register(&self.dev.output_speed, 0, 2, 16)
    }

    pub fn pupdr(&self) -> u32 {
        GpioDev::array_register(&self.dev.pull, 0, 2, 16)
    }

    pub fn afrl(&self) -> u32 {
        GpioDev::array_register(&self.dev.active_function, 0, 4, 8)
    }

    pub fn afrh(&self) -> u32 {
        GpioDev::array_register(&self.dev.active_function, 8, 4, 8)
    }

    pub fn pin_mode(&self, pin: usize) -> PinMode {
        PinMode::from_bits(self.dev.mode[pin])
    }

    /// Selected alternate function of pin `pin`.
    pub fn active_function(&self, pin: usize) -> u8 {
        self.dev.active_function[pin]
    }

    /// Pins frozen by the lock sequence (meaningful once [`Gpio::lock_sequence`] is `Armed`).
    pub fn locked_pins(&self) -> u16 {
        self.dev.locked_pins
    }

    pub fn lock_sequence(&self) -> LockSequence {
        self.dev.lock
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        format!(
            "{}: IDR/ODR=0x{:04X} MODER=0x{:08X} OSPEEDR=0x{:08X} PUPDR=0x{:08X} AFRL=0x{:08X} AFRH=0x{:08X} lock={:?} locked=0x{:04X}",
            self.name,
            self.dev.state,
            self.moder(),
            self.ospeedr(),
            self.pupdr(),
            self.afrl(),
            self.afrh(),
            self.dev.lock,
            self.dev.locked_pins
        )
    }

    /// The value a read of the register at `offset` returns, without any side effect (no provider
    /// refresh of the stored values, no lock-sequence step). Unimplemented offsets read 0.
    fn peek_register(&self, offset: u32) -> u32 {
        match offset {
            reg::MODER => self.moder(),
            reg::OTYPER => self.regs.value(R_OTYPER),
            reg::OSPEEDR => self.ospeedr(),
            reg::PUPDR => self.pupdr(),
            reg::IDR | reg::ODR => self.idr(),
            reg::LCKR => u32::from(self.dev.locked_pins) | (self.regs.value(R_LCKR) & LCKK_BIT),
            reg::AFRL => self.afrl(),
            reg::AFRH => self.afrh(),
            _ => 0,
        }
    }
}

impl Peripheral for Gpio {
    fn name(&self) -> &str {
        &self.name
    }

    /// `STM32_GPIOPort.Reset`: outputs low, `State` cleared, registers, lock and pin configuration back to
    /// their reset values.
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        for pin in 0..NUMBER_OF_PINS {
            ctx.set_output(pin as u32, false);
        }
        self.regs.reset();
        self.dev.reset_state();
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        self.regs.read(offset, &mut self.dev, ctx)
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.regs.write(offset, value, &mut self.dev, ctx);
    }

    /// `BaseGPIOPort.OnGPIO` + `Connections[n].Set`, or an alternate-function receiver (see the module docs).
    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        if line < NUMBER_OF_PINS as u32 {
            self.dev.write_pin(ctx, line as usize, level);
        } else if line < af_input_line(NUMBER_OF_PINS as u32, 0) {
            let index = line - AF_INPUT_BASE;
            self.dev.af_input(ctx, (index / 16) as usize, index % 16, level);
        } else {
            ctx.error_once(
                KEY_PIN_RANGE | u64::from(line),
                format_args!("This peripheral supports gpio inputs from 0 to {NUMBER_OF_PINS}, but {line} was called."),
            );
        }
    }

    // [AllowedTranslations(AllowedTranslation.WordToDoubleWord)] on an IDoubleWordPeripheral.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY.with_translations(Translations::HALF_TO_WORD)
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
    use emu_core::testing::Harness;
    use emu_core::{PeriphId, Time};

    const BASE: u32 = 0x4800_0000;

    fn port(config: GpioConfig) -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Gpio::new("gpioA", config));
        (h, id)
    }

    fn default_port() -> (Harness, PeriphId) {
        port(GpioConfig::default())
    }

    fn gpio(h: &Harness, id: PeriphId) -> &Gpio {
        h.get::<Gpio>(id)
    }

    fn levels(changes: Vec<(Time, bool)>) -> Vec<bool> {
        changes.into_iter().map(|(_, level)| level).collect()
    }

    /// Writes LCKR three times as the HAL lock sequence does, without the final reads.
    fn lock_writes(h: &mut Harness, pins: u32) {
        h.write32(BASE + reg::LCKR, 0x1_0000 | pins);
        h.write32(BASE + reg::LCKR, pins);
        h.write32(BASE + reg::LCKR, 0x1_0000 | pins);
    }

    // ---- reset values and plain registers ----------------------------------------------

    #[test]
    fn default_port_resets_to_all_zero() {
        let (mut h, id) = default_port();
        for offset in [reg::MODER, reg::OTYPER, reg::OSPEEDR, reg::PUPDR, reg::IDR, reg::ODR, reg::BSRR, reg::LCKR, reg::AFRL, reg::AFRH, reg::BRR] {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:02X}");
        }
        assert!(h.warnings().is_empty());
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
    }

    #[test]
    fn mode_reset_values_of_the_platform_ports() {
        let (mut h, id) = port(GpioConfig::default().with_mode_reset(GpioConfig::PORT_A_MODE_RESET));
        assert_eq!(h.peek(BASE + reg::MODER, Width::Word), Some(0xABFF_FFFF), "visible before any read");
        assert_eq!(h.read32(BASE + reg::MODER), 0xABFF_FFFF);
        let g = gpio(&h, id);
        assert_eq!(g.pin_mode(0), PinMode::Analog);
        assert_eq!(g.pin_mode(11), PinMode::Analog);
        assert_eq!(g.pin_mode(12), PinMode::Analog);
        assert_eq!(g.pin_mode(13), PinMode::AlternateFunction);
        assert_eq!(g.pin_mode(14), PinMode::AlternateFunction);
        assert_eq!(g.pin_mode(15), PinMode::AlternateFunction);

        let (mut h, id) = port(GpioConfig::default().with_mode_reset(GpioConfig::PORT_B_MODE_RESET));
        assert_eq!(h.read32(BASE + reg::MODER), 0xFFFF_FEBF);
        let g = gpio(&h, id);
        assert_eq!(g.pin_mode(2), PinMode::Analog);
        assert_eq!(g.pin_mode(3), PinMode::AlternateFunction, "PB3 (SWO)");
        assert_eq!(g.pin_mode(4), PinMode::AlternateFunction, "PB4 (NJTRST)");
        assert_eq!(g.pin_mode(5), PinMode::Analog);
    }

    #[test]
    fn output_speed_and_pull_reset_values_come_from_the_configuration() {
        let (mut h, _id) = port(GpioConfig::default().with_output_speed_reset(0x0C00_00FF).with_pull_up_pull_down_reset(0x6400_0000));
        assert_eq!(h.read32(BASE + reg::OSPEEDR), 0x0C00_00FF);
        assert_eq!(h.read32(BASE + reg::PUPDR), 0x6400_0000);
    }

    #[test]
    fn moder_ospeedr_pupdr_and_afr_round_trip_through_read_modify_write() {
        let (mut h, id) = default_port();
        for (offset, value) in [(reg::MODER, 0x2800_0501), (reg::OSPEEDR, 0xF0F0_0FFF), (reg::PUPDR, 0x6400_0000), (reg::AFRL, 0x7654_3210), (reg::AFRH, 0xFEDC_BA98)] {
            let old = h.read32(BASE + offset);
            h.write32(BASE + offset, old ^ value);
            assert_eq!(h.read32(BASE + offset), value, "offset 0x{offset:02X}");
        }
        let g = gpio(&h, id);
        assert_eq!(g.moder(), 0x2800_0501);
        assert_eq!(g.pin_mode(0), PinMode::Output);
        assert_eq!(g.pin_mode(2), PinMode::Input);
        assert_eq!(g.pin_mode(4), PinMode::Output);
        assert_eq!(g.pin_mode(5), PinMode::Output);
        assert_eq!(g.pin_mode(13), PinMode::AlternateFunction);
        assert_eq!(g.pin_mode(14), PinMode::AlternateFunction);
        assert_eq!(g.ospeedr(), 0xF0F0_0FFF);
        assert_eq!(g.pupdr(), 0x6400_0000);
        assert_eq!(g.afrl(), 0x7654_3210);
        assert_eq!(g.afrh(), 0xFEDC_BA98);
        assert_eq!(g.active_function(15), 0xF);
        assert_eq!(g.active_function(7), 7);
        assert_eq!(g.active_function(0), 0);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn writes_equal_to_the_stale_underlying_value_are_lost_until_a_read_refreshes_it() {
        // Renode parity: the register framework compares with the stored (underlying) value, which is
        // 0 after reset although `modeResetValue` is 0xFFFFFFFF. A write of 0 therefore changes nothing.
        let (mut h, id) = port(GpioConfig::default().with_mode_reset(0xFFFF_FFFF));
        h.write32(BASE + reg::MODER, 0);
        assert_eq!(gpio(&h, id).moder(), 0xFFFF_FFFF, "write without a preceding read is not applied");
        assert_eq!(h.read32(BASE + reg::MODER), 0xFFFF_FFFF);
        // After the read the stored value is current, so the write is a change.
        h.write32(BASE + reg::MODER, 0);
        assert_eq!(gpio(&h, id).moder(), 0);
        // peek does not refresh the stored value.
        let (mut h, id) = port(GpioConfig::default().with_mode_reset(0xFFFF_FFFF));
        assert_eq!(h.peek(BASE + reg::MODER, Width::Word), Some(0xFFFF_FFFF));
        h.write32(BASE + reg::MODER, 0);
        assert_eq!(gpio(&h, id).moder(), 0xFFFF_FFFF);
    }

    // ---- pin state: ODR / BSRR / BRR / inputs -------------------------------------------

    #[test]
    fn odr_write_drives_the_output_lines_in_pin_order() {
        let (mut h, id) = default_port();
        let probes: Vec<_> = (0..16).map(|pin| h.probe(id, pin)).collect();
        h.write32(BASE + reg::ODR, 0x8005);
        for (pin, probe) in probes.iter().enumerate() {
            let expected = matches!(pin, 0 | 2 | 15);
            assert_eq!(levels(h.probe_changes(*probe)), if expected { vec![true] } else { vec![] }, "pin {pin}");
        }
        assert_eq!(h.read32(BASE + reg::ODR), 0x8005);
        assert_eq!(h.read32(BASE + reg::IDR), 0x8005, "IDR and ODR share the pin state");
        // Writing the same value again does not notify anybody.
        h.write32(BASE + reg::ODR, 0x8005);
        assert_eq!(levels(h.probe_changes(probes[0])), [true]);
        // Clearing drops the lines.
        h.write32(BASE + reg::ODR, 0x0004);
        assert_eq!(levels(h.probe_changes(probes[0])), [true, false]);
        assert_eq!(levels(h.probe_changes(probes[15])), [true, false]);
        assert_eq!(levels(h.probe_changes(probes[2])), [true]);
        assert_eq!(gpio(&h, id).pin_state(), 4);
        assert!(gpio(&h, id).pin(2) && !gpio(&h, id).pin(0));
    }

    #[test]
    fn odr_upper_half_is_reserved() {
        let (mut h, id) = default_port();
        h.write32(BASE + reg::ODR, 0x0001_0003);
        assert_eq!(h.read32(BASE + reg::ODR), 3);
        assert_eq!(gpio(&h, id).pin_state(), 3);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x14. Unhandled bits: [16] when writing value 0x10003. Tags: RESERVED (0x1)."]);
    }

    #[test]
    fn bsrr_sets_and_resets_pins_and_reads_zero() {
        let (mut h, id) = default_port();
        let p1 = h.probe(id, 1);
        let p3 = h.probe(id, 3);
        h.write32(BASE + reg::BSRR, 0x0000_000A);
        assert_eq!(h.read32(BASE + reg::ODR), 0x0A);
        assert_eq!(h.read32(BASE + reg::BSRR), 0);
        h.write32(BASE + reg::BSRR, 0x0002_0000);
        assert_eq!(h.read32(BASE + reg::ODR), 0x08);
        assert_eq!(levels(h.probe_changes(p1)), [true, false]);
        assert_eq!(levels(h.probe_changes(p3)), [true]);
        // Zero halves are no-ops.
        h.write32(BASE + reg::BSRR, 0);
        assert_eq!(h.read32(BASE + reg::ODR), 0x08);
        assert!(h.warnings().is_empty(), "BSRR has no reserved bits");
    }

    #[test]
    fn bsrr_bit_in_both_halves_pulses_a_low_pin_and_leaves_it_low() {
        let (mut h, id) = default_port();
        let p3 = h.probe(id, 3);
        let p4 = h.probe(id, 4);
        h.write32(BASE + reg::BSRR, 0x0008_0008);
        assert_eq!(h.read32(BASE + reg::ODR), 0, "BR is applied after BS: reset wins");
        assert_eq!(levels(h.probe_changes(p3)), [true, false], "set, then reset: a pulse the receivers see");
        // A pin that is already high just goes low.
        h.write32(BASE + reg::ODR, 0x0010);
        h.write32(BASE + reg::BSRR, 0x0010_0010);
        assert_eq!(levels(h.probe_changes(p4)), [true, false]);
        assert_eq!(h.read32(BASE + reg::ODR), 0);
    }

    #[test]
    fn brr_resets_pins_and_warns_about_reserved_bits() {
        let (mut h, id) = default_port();
        h.write32(BASE + reg::ODR, 0x00FF);
        h.write32(BASE + reg::BRR, 0x000F);
        assert_eq!(h.read32(BASE + reg::ODR), 0x00F0);
        assert_eq!(h.read32(BASE + reg::BRR), 0);
        assert!(h.warnings().is_empty());
        h.write32(BASE + reg::BRR, 0x0001_0010);
        assert_eq!(h.read32(BASE + reg::ODR), 0x00E0);
        assert_eq!(h.warnings().len(), 1);
        assert!(h.warnings()[0].contains("offset 0x28") && h.warnings()[0].contains("RESERVED (0x1)"));
        assert_eq!(gpio(&h, id).pin_state(), 0xE0);
    }

    #[test]
    fn writing_idr_changes_nothing_and_reserved_bits_warn() {
        let (mut h, _id) = default_port();
        h.write32(BASE + reg::ODR, 0x0003);
        h.write32(BASE + reg::IDR, 0x0000_FFFC);
        assert_eq!(h.read32(BASE + reg::IDR), 3);
        assert!(h.warnings().is_empty(), "the low half of IDR is a read-only field");
        h.write32(BASE + reg::IDR, 0x8000_0000);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x10. Unhandled bits: [31] when writing value 0x80000000. Tags: RESERVED (0x8000)."]);
    }

    #[test]
    fn external_inputs_update_idr_odr_and_forward_to_the_output_line() {
        let (mut h, id) = default_port();
        let p4 = h.probe(id, 4);
        let p5 = h.probe(id, 5);
        h.set_input(id, 4, true);
        assert_eq!(h.read32(BASE + reg::IDR), 0x10);
        assert_eq!(h.read32(BASE + reg::ODR), 0x10, "Renode shares one state array: the input shows in ODR");
        assert_eq!(levels(h.probe_changes(p4)), [true]);
        assert!(h.probe_changes(p5).is_empty());
        // Setting the same level again does not produce an edge.
        h.set_input(id, 4, true);
        assert_eq!(levels(h.probe_changes(p4)), [true]);
        h.set_input(id, 4, false);
        assert_eq!(levels(h.probe_changes(p4)), [true, false]);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        // The pin mode does not matter: an output pin follows an external drive.
        let old = h.read32(BASE + reg::MODER);
        h.write32(BASE + reg::MODER, old | (1 << 10));
        h.set_input(id, 5, true);
        assert_eq!(h.read32(BASE + reg::IDR), 0x20);
        assert_eq!(levels(h.probe_changes(p5)), [true]);
    }

    #[test]
    fn input_line_out_of_range_is_an_error() {
        let (mut h, id) = default_port();
        h.set_input(id, 300, true);
        h.set_input(id, 300, true);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        let errors: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == emu_core::LogLevel::Error).map(|e| e.message).collect();
        assert_eq!(errors, ["This peripheral supports gpio inputs from 0 to 16, but 300 was called."]);
    }

    #[test]
    fn connecting_pushes_the_current_level_even_when_low() {
        let (mut h, id) = default_port();
        h.write32(BASE + reg::ODR, 0x0002);
        let p0 = h.probe(id, 0);
        let p1 = h.probe(id, 1);
        let first0 = h.probe_events(p0);
        let first1 = h.probe_events(p1);
        assert_eq!(first0.len(), 1);
        assert!(!first0[0].level, "pin 0 is low: the low level is pushed at connect time");
        assert_eq!(first1.len(), 1);
        assert!(first1[0].level);
    }

    #[test]
    fn irq_style_line_to_exti_input_example_follows_the_pin() {
        // gpioC:1 -> exti@1 is wired as (gpio line 1) -> (exti input 1); here a second port stands in for it.
        let mut h = Harness::new();
        let a = h.add_mapped(BASE, 0x400, Gpio::new("gpioA", GpioConfig::default()));
        let b = h.add_mapped(BASE + 0x400, 0x400, Gpio::new("gpioB", GpioConfig::default()));
        h.connect_input(a, 5, b, 2);
        h.write32(BASE + reg::ODR, 0x20);
        assert_eq!(h.read32(BASE + 0x400 + reg::IDR), 0x04, "pin 5 of A drives pin 2 of B");
        h.write32(BASE + reg::ODR, 0);
        assert_eq!(h.read32(BASE + 0x400 + reg::IDR), 0);
    }

    // ---- OTYPER and unimplemented registers (messages observed in the reference boots) -------

    #[test]
    fn otyper_has_no_storage_and_logs_tagged_bits_like_renode() {
        let (mut h, _id) = default_port();
        for (value, pin) in [(0x40u32, 6), (0x80, 7), (0x400, 10), (0x800, 11)] {
            h.write32(BASE + reg::OTYPER, value);
            assert_eq!(h.read32(BASE + reg::OTYPER), 0, "OTYPER is not stored");
            let expected = format!("Unhandled write to offset 0x4. Unhandled bits: [{pin}] when writing value 0x{value:X}. Tags: OT{pin} (0x1).");
            assert_eq!(h.warnings().last().unwrap(), &expected);
        }
        assert_eq!(h.warnings().len(), 4);
        // Several bits at once, and the reserved upper half.
        h.write32(BASE + reg::OTYPER, 0x0001_0309);
        assert_eq!(
            h.warnings().last().unwrap(),
            "Unhandled write to offset 0x4. Unhandled bits: [0, 3, 8-9, 16] when writing value 0x10309. Tags: OT0 (0x1), OT3 (0x1), OT8 (0x1), OT9 (0x1), RESERVED (0x1)."
        );
    }

    #[test]
    fn unimplemented_offsets_log_and_read_zero() {
        // gpioA/gpioC offset 0x2C (ASCR on L4) is read and written by both firmwares.
        let (mut h, _id) = default_port();
        assert_eq!(h.read32(BASE + 0x2C), 0);
        h.write32(BASE + 0x2C, 1);
        assert_eq!(h.read32(BASE + 0x2C), 0, "nothing is stored");
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x2C.", "Unhandled write to offset 0x2C, value 0x1."]);
        assert_eq!(h.read32(BASE + 0x30), 0);
        assert_eq!(h.read32(BASE + 0x3FC), 0);
        assert_eq!(h.warnings().len(), 4);
        assert_eq!(h.peek(BASE + 0x2C, Width::Word), Some(0), "peek of an unimplemented offset is 0 without a log");
    }

    // ---- access widths ---------------------------------------------------------------------

    #[test]
    fn halfword_writes_are_word_read_modify_writes_and_bytes_are_rejected() {
        let (mut h, id) = default_port();
        h.write32(BASE + reg::ODR, 0x0003);
        // GPIOx->BSRRH-style halfword store of the reset half: reads BSRR (0) first, then writes the word.
        h.write16(BASE + reg::BSRR + 2, 0x0001);
        assert_eq!(h.read32(BASE + reg::ODR), 0x0002);
        // Low halfword store to ODR: read-modify-write keeps the (reserved, zero) upper half.
        h.write16(BASE + reg::ODR, 0x00A5);
        assert_eq!(h.read32(BASE + reg::ODR), 0x00A5);
        assert_eq!(h.read16(BASE + reg::ODR + 2), 0, "halfword reads shift out of the word");
        assert_eq!(h.read16(BASE + reg::ODR), 0x00A5);
        assert!(h.warnings().is_empty());
        // Bytes are not supported by an IDoubleWordPeripheral with WordToDoubleWord only.
        h.write8(BASE + reg::ODR, 0xFF);
        assert_eq!(gpio(&h, id).pin_state(), 0x00A5);
        assert_eq!(h.read8(BASE + reg::ODR), 0);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("Attempted Byte write isn't supported by the peripheral"));
        assert!(warnings[1].contains("Attempted Byte read isn't supported by the peripheral"));
    }

    #[test]
    fn matches_the_access_probe_of_the_renode_reference_run() {
        // Access probe recorded from Renode 1.17.0 on 2026-10-08 (monitor accesses on gpioA of the handset platform):
        let (mut h, _id) = default_port();
        h.write8(BASE + 0x14, 0x5); // "Attempted Byte write isn't supported by the peripheral. Offset 0x14, value 0x5."
        assert_eq!(h.read8(BASE + 0x14), 0); // "Attempted Byte read isn't supported by the peripheral. Offset 0x14."
        h.write16(BASE + 0x14, 0x7); // read-modify-write through the dword, no log
        assert_eq!(h.read32(BASE + 0x14), 0x0000_0007);
        h.write16(BASE + 0x18, 0x1); // BSRR halfword write: the RMW reads BSRR (0) first
        assert_eq!(h.read32(BASE + 0x14), 0x0000_0007);
        assert_eq!(h.read32(BASE + 0x2C), 0); // "Unhandled read from offset 0x2C."
        h.write32(BASE + 0x2C, 0x1); // "Unhandled write to offset 0x2C, value 0x1."
        assert_eq!(
            h.warnings(),
            [
                "gpioA: Attempted Byte write isn't supported by the peripheral. Offset 0x14, value 0x5.",
                "gpioA: Attempted Byte read isn't supported by the peripheral. Offset 0x14.",
                "Unhandled read from offset 0x2C.",
                "Unhandled write to offset 0x2C, value 0x1.",
            ]
        );
    }

    #[test]
    fn unaligned_word_offsets_do_not_reach_a_register() {
        let (mut h, _id) = default_port();
        assert_eq!(h.read(BASE + reg::ODR + 1, Width::Word), 0, "a host access is not split like a CPU access");
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x15."]);
    }

    // ---- lock sequence -----------------------------------------------------------------------

    #[test]
    fn lock_sequence_arms_on_the_read_and_freezes_configuration_of_locked_pins() {
        let (mut h, id) = default_port();
        lock_writes(&mut h, 0x0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Step2);
        assert_eq!(h.peek(BASE + reg::LCKR, Width::Word), Some(0x1_0003));
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Step2, "peek does not step the sequence");
        // The read arms the lock and returns the locked pins; LCKK reads back as last written (Renode).
        assert_eq!(h.read32(BASE + reg::LCKR), 0x1_0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Armed);
        assert_eq!(gpio(&h, id).locked_pins(), 3);
        assert_eq!(h.read32(BASE + reg::LCKR), 0x1_0003, "further reads keep it armed");
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Armed);

        // Configuration of pin 0 and 1 is frozen, pin 2 is not.
        let moder = h.read32(BASE + reg::MODER);
        h.write32(BASE + reg::MODER, moder | 0b01_01_01);
        assert_eq!(h.read32(BASE + reg::MODER), 0b01_00_00, "only pin 2 changed to output");
        let ospeed = h.read32(BASE + reg::OSPEEDR);
        h.write32(BASE + reg::OSPEEDR, ospeed | 0b11_11_11);
        assert_eq!(h.read32(BASE + reg::OSPEEDR), 0b11_00_00);
        let pupd = h.read32(BASE + reg::PUPDR);
        h.write32(BASE + reg::PUPDR, pupd | 0b10_10_10);
        assert_eq!(h.read32(BASE + reg::PUPDR), 0b10_00_00);
        let afrl = h.read32(BASE + reg::AFRL);
        h.write32(BASE + reg::AFRL, afrl | 0x555);
        assert_eq!(h.read32(BASE + reg::AFRL), 0x500);
        // Pins of AFRH are not locked.
        let afrh = h.read32(BASE + reg::AFRH);
        h.write32(BASE + reg::AFRH, afrh | 0x5);
        assert_eq!(h.read32(BASE + reg::AFRH), 0x5);

        let warnings = h.warnings();
        for expected in [
            "Ignoring attempt to change MODER configuration of the locked pin #0",
            "Ignoring attempt to change MODER configuration of the locked pin #1",
            "Ignoring attempt to change OSPEEDR configuration of the locked pin #0",
            "Ignoring attempt to change PUPDR0 configuration of the locked pin #1",
            "Ignoring attempt to change AFSEL configuration of the locked pin #0",
        ] {
            assert!(warnings.iter().any(|w| w == expected), "missing {expected:?} in {warnings:?}");
        }
        // Once armed, LCKR writes are ignored by the state machine.
        h.write32(BASE + reg::LCKR, 0x0000_0007);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Armed);
        assert_eq!(gpio(&h, id).locked_pins(), 3);
    }

    #[test]
    fn lock_sequence_is_aborted_by_a_read_a_wrong_pattern_or_an_extra_write() {
        let (mut h, id) = default_port();
        // A read between the writes restarts the sequence.
        h.write32(BASE + reg::LCKR, 0x1_0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Step0);
        let _ = h.read32(BASE + reg::LCKR);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        // A different pin pattern in the second write.
        h.write32(BASE + reg::LCKR, 0x1_0003);
        h.write32(BASE + reg::LCKR, 0x0004);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        // LCKK still set in the second write.
        h.write32(BASE + reg::LCKR, 0x1_0003);
        h.write32(BASE + reg::LCKR, 0x1_0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        // Third write with LCKK clear.
        h.write32(BASE + reg::LCKR, 0x1_0003);
        h.write32(BASE + reg::LCKR, 0x0003);
        h.write32(BASE + reg::LCKR, 0x0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        // A fourth write before the arming read aborts.
        lock_writes(&mut h, 0x0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Step2);
        h.write32(BASE + reg::LCKR, 0x1_0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        let _ = h.read32(BASE + reg::LCKR);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle, "never armed");
        // A write with LCKK clear in the idle state does nothing.
        h.write32(BASE + reg::LCKR, 0x0003);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        // Full sequence still works afterwards.
        lock_writes(&mut h, 0x00F0);
        let _ = h.read32(BASE + reg::LCKR);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Armed);
        assert_eq!(gpio(&h, id).locked_pins(), 0xF0);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn lckr_reserved_bits_warn() {
        let (mut h, _id) = default_port();
        h.write32(BASE + reg::LCKR, 0x0002_0000);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x1C. Unhandled bits: [17] when writing value 0x20000. Tags: RESERVED (0x1)."]);
    }

    // ---- alternate functions -----------------------------------------------------------------

    #[test]
    fn alternate_function_numbers_are_limited_by_the_configuration() {
        let (mut h, id) = port(GpioConfig::default().with_alternate_functions(8));
        let old = h.read32(BASE + reg::AFRL);
        h.write32(BASE + reg::AFRL, old | 0x0000_0009);
        assert_eq!(h.read32(BASE + reg::AFRL), 0, "AF 9 is not accepted with 8 AFs");
        h.write32(BASE + reg::AFRL, 0x7);
        assert_eq!(h.read32(BASE + reg::AFRL), 0x7);
        assert_eq!(gpio(&h, id).active_function(0), 7);
        let errors: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == emu_core::LogLevel::Error).map(|e| e.message).collect();
        assert_eq!(errors, ["Alternate function number must be between 0 and 7, but 9 was given instead."]);
    }

    #[test]
    fn alternate_function_inputs_drive_the_pin_only_in_af_mode_with_the_selected_function() {
        let (mut h, id) = port(GpioConfig::default().with_inverted_af_pins(6, &[2]));
        let p5 = h.probe(id, 5);
        let p6 = h.probe(id, 6);
        // Not in AF mode yet: ignored.
        h.set_input(id, af_input_line(5, 3), true);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        // AF mode for pin 5 and 6, AF3 on pin 5, AF2 on pin 6.
        h.write32(BASE + reg::MODER, 0b10 << 10 | 0b10 << 12);
        h.write32(BASE + reg::AFRL, 3 << 20 | 2 << 24);
        h.set_input(id, af_input_line(5, 3), true);
        assert_eq!(h.read32(BASE + reg::IDR), 0x20);
        assert_eq!(levels(h.probe_changes(p5)), [true]);
        // Another function of the same pin is ignored.
        h.set_input(id, af_input_line(5, 4), false);
        assert_eq!(h.read32(BASE + reg::IDR), 0x20);
        h.set_input(id, af_input_line(5, 3), false);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        // Inverted function.
        h.set_input(id, af_input_line(6, 2), false);
        assert_eq!(h.read32(BASE + reg::IDR), 0x40, "inverted: low input drives the pin high");
        assert_eq!(levels(h.probe_changes(p6)), [true]);
        h.set_input(id, af_input_line(6, 2), true);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        // Leaving AF mode disconnects the receiver again.
        h.write32(BASE + reg::MODER, 0);
        h.set_input(id, af_input_line(5, 3), true);
        assert_eq!(h.read32(BASE + reg::IDR), 0);
        assert!(h.drain_log().iter().all(|e| e.level < emu_core::LogLevel::Error));
    }

    // ---- reset -------------------------------------------------------------------------------

    #[test]
    fn reset_drops_outputs_and_restores_every_register() {
        let (mut h, id) = port(GpioConfig::default().with_mode_reset(GpioConfig::PORT_B_MODE_RESET));
        let p0 = h.probe(id, 0);
        let p9 = h.probe(id, 9);
        let moder = h.read32(BASE + reg::MODER);
        h.write32(BASE + reg::MODER, (moder & !0b11) | 0b01);
        assert_eq!(gpio(&h, id).pin_mode(0), PinMode::Output);
        h.write32(BASE + reg::OSPEEDR, 0xFFFF);
        h.write32(BASE + reg::AFRH, 0x70);
        h.write32(BASE + reg::ODR, 0x0201);
        lock_writes(&mut h, 0x0001);
        let _ = h.read32(BASE + reg::LCKR);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Armed);
        h.core_mut().reset_all();
        assert_eq!(levels(h.probe_changes(p0)), [true, false]);
        assert_eq!(levels(h.probe_changes(p9)), [true, false]);
        assert_eq!(h.read32(BASE + reg::ODR), 0);
        assert_eq!(h.read32(BASE + reg::MODER), 0xFFFF_FEBF);
        assert_eq!(h.read32(BASE + reg::OSPEEDR), 0);
        assert_eq!(h.read32(BASE + reg::AFRH), 0);
        assert_eq!(gpio(&h, id).lock_sequence(), LockSequence::Idle);
        assert_eq!(h.read32(BASE + reg::LCKR) & 0xFFFF, 0);
        assert_eq!(h.read32(BASE + reg::LCKR) >> 16, 0, "the stored LCKK bit was reset too");
        // Configuration is changeable again (the old lock is gone).
        let moder = h.read32(BASE + reg::MODER);
        h.write32(BASE + reg::MODER, moder & !0b11);
        assert_eq!(gpio(&h, id).pin_mode(0), PinMode::Input);
    }

    // ---- misc --------------------------------------------------------------------------------

    #[test]
    fn peek_matches_read_without_side_effects() {
        let (mut h, _id) = default_port();
        h.write32(BASE + reg::ODR, 0x1234);
        let old = h.read32(BASE + reg::MODER);
        h.write32(BASE + reg::MODER, old | 0x55);
        h.write32(BASE + reg::AFRL, 0x9876_5432);
        for offset in [reg::MODER, reg::OTYPER, reg::OSPEEDR, reg::PUPDR, reg::IDR, reg::ODR, reg::BSRR, reg::AFRL, reg::AFRH, reg::BRR] {
            let peeked = h.peek(BASE + offset, Width::Word);
            assert_eq!(peeked, Some(h.read32(BASE + offset)), "offset 0x{offset:02X}");
        }
        assert_eq!(h.peek(BASE + reg::ODR, Width::Word), Some(0x1234));
        assert_eq!(h.peek(BASE + reg::ODR, Width::Half), Some(0x1234), "halfword peek goes through the word");
    }

    #[test]
    fn summary_reports_the_registers() {
        let (mut h, id) = port(GpioConfig::default().with_mode_reset(0xABFF_FFFF));
        h.write32(BASE + reg::ODR, 0x0010);
        let summary = h.get::<Gpio>(id).describe();
        assert_eq!(
            summary,
            "gpioA: IDR/ODR=0x0010 MODER=0xABFFFFFF OSPEEDR=0x00000000 PUPDR=0x00000000 AFRL=0x00000000 AFRH=0x00000000 lock=Idle locked=0x0000"
        );
        assert!(h.core().summaries().iter().any(|(name, s)| name == "gpioA" && *s == summary));
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        assert!(Gpio::try_new("g", GpioConfig::default().with_alternate_functions(0)).is_err());
        assert!(Gpio::try_new("g", GpioConfig::default().with_alternate_functions(17)).is_err());
        assert!(Gpio::try_new("g", GpioConfig::default().with_inverted_af_pins(16, &[0])).is_err());
        assert!(Gpio::try_new("g", GpioConfig::default().with_alternate_functions(4).with_inverted_af_pins(3, &[4])).is_err());
        assert!(Gpio::try_new("g", GpioConfig::default().with_alternate_functions(4).with_inverted_af_pins(3, &[3])).is_ok());
        let panic = std::panic::catch_unwind(|| Gpio::new("g", GpioConfig::default().with_alternate_functions(0)));
        assert!(panic.is_err());
    }

    /// Cost of the register-framework path (`cargo test -p stm32 --release -- --ignored --nocapture bench`).
    #[test]
    #[ignore = "micro-benchmark; run with --ignored --nocapture"]
    fn bench_register_access_paths() {
        let (mut h, _id) = port(GpioConfig::default().with_mode_reset(GpioConfig::PORT_B_MODE_RESET));
        const N: u32 = 2_000_000;
        let time = |label: &str, h: &mut Harness, f: &mut dyn FnMut(&mut Harness, u32)| {
            let start = std::time::Instant::now();
            for i in 0..N {
                f(h, i);
            }
            let ns = start.elapsed().as_nanos() as f64 / f64::from(N);
            println!("{label:<28} {ns:7.1} ns/access");
        };
        time("IDR read", &mut h, &mut |h, _| {
            std::hint::black_box(h.read32(BASE + reg::IDR));
        });
        time("ODR write (toggle pin 4)", &mut h, &mut |h, i| h.write32(BASE + reg::ODR, (i & 1) << 4));
        time("BSRR write (set/reset pin 4)", &mut h, &mut |h, i| h.write32(BASE + reg::BSRR, if i & 1 == 0 { 1 << 4 } else { 1 << 20 }));
        time("MODER read (16 providers)", &mut h, &mut |h, _| {
            std::hint::black_box(h.read32(BASE + reg::MODER));
        });
        time("unimplemented 0x2C read", &mut h, &mut |h, _| {
            std::hint::black_box(h.read32(BASE + 0x2C));
        });
    }

    #[test]
    fn access_policy_is_word_only_with_halfword_translation() {
        let g = Gpio::with_mode_reset("g", 0);
        let policy = g.access_policy();
        assert!(policy.native.contains(Width::Word) && !policy.native.contains(Width::Half) && !policy.native.contains(Width::Byte));
        assert!(policy.translations.contains(Translations::HALF_TO_WORD));
        assert!(!policy.translations.contains(Translations::BYTE_TO_WORD));
    }
}
