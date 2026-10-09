// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/CRC/STM32_CRC.cs,
// src/Emulator/Main/Utilities/CRCEngine.cs and src/Emulator/Peripherals/Peripherals/STM32Series.cs
// (MIT License, Copyright (c) Antmicro, Copyright (c) 2022 Pieter Agten).

//! `CRC.STM32_CRC` (`series: STM32Series.F0`, `configurablePoly: true` on both boards).
//!
//! Registers: `DR` (data/result), `IDR` (independent data, a tag: reads 0, writes only log), `CR`
//! (`RESET`, `POLYSIZE`, `REV_IN`, `REV_OUT`), `INIT`, `POL`. The CRC is computed MSB first over the bytes
//! of every write to `DR` (a 32-bit write feeds 4 bytes, a halfword write 2, a byte write 1); `DR` reads
//! return the current value, optionally bit-reversed (`REV_OUT`). Input bit reversal (`REV_IN`) is applied
//! to the write unit as in `UpdateCRC`.
//!
//! The access policy is [`AccessPolicy::EXACT`]: the class implements `IBytePeripheral`,
//! `IWordPeripheral` and `IDoubleWordPeripheral`. Byte and halfword writes only do something at `DR`
//! (offset 0); elsewhere they log `Unhandled write ...` like `LogUnhandledWrite`. Byte and halfword reads
//! are a full register read truncated to the width ("only properly aligned reads will be handled
//! correctly here").
//!
//! // Renode parity (all deliberate):
//! * the CRC engine is (re)built lazily at the next `DR` access after **any** write to `CR` or `POL`
//!   (a dirty flag), and a rebuild always restarts the CRC from `INIT` (clamped to the polynomial width).
//!   So `CR.RESET` is not a field callback: *every* `CR` write resets the CRC, writing `INIT` alone does
//!   not, and the new `INIT` is picked up by the next `CR`/`POL` write;
//! * a configuration Renode rejects with an exception (polynomial wider than `POLYSIZE`) is logged as an
//!   error here and the `DR` access is abandoned instead: writes are dropped, reads return the value of the
//!   previous engine (0 if there never was one), and the rebuild is retried at the next access.

use crate::gpio::regfw::{log_unhandled_write, Hooks, Mode, Model, RegisterBuilder, RegisterFile};
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, View, Width};

/// `STM32Series`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stm32Series {
    F0,
    F1,
    F4,
    F7,
    H7,
    G0,
    L0,
    L1,
    L5,
    WBA,
}

impl std::fmt::Display for Stm32Series {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// `IKnownSize.Size`.
pub const SIZE: u32 = 0x400;

/// Register offsets.
pub mod reg {
    pub const DR: u32 = 0x00;
    pub const IDR: u32 = 0x04;
    pub const CR: u32 = 0x08;
    pub const INIT: u32 = 0x10;
    pub const POL: u32 = 0x14;
}

// Register numbering (the dictionary order of the C# class).
const R_DR: usize = 0;
const R_CR: usize = 1;
const R_INIT: usize = 2;
const R_POL: usize = 3;
const R_IDR: usize = 4;

/// `DefaultPolymonial` / `DefaultInitialValue`.
const DEFAULT_POLYNOMIAL: u32 = 0x04C1_1DB7;
const DEFAULT_INITIAL_VALUE: u32 = 0xFFFF_FFFF;

/// `STM32Config` of a series.
#[derive(Clone, Copy, Debug)]
struct SeriesConfig {
    configurable_initial_value: bool,
    has_poly_size_bits: bool,
    reversible_io: bool,
    independent_data_width: u32,
}

fn series_config(series: Stm32Series) -> Option<SeriesConfig> {
    match series {
        Stm32Series::F0 => Some(SeriesConfig {
            configurable_initial_value: true,
            has_poly_size_bits: true,
            reversible_io: true,
            independent_data_width: 8,
        }),
        Stm32Series::F4 => Some(SeriesConfig {
            configurable_initial_value: false,
            has_poly_size_bits: false,
            reversible_io: false,
            independent_data_width: 8,
        }),
        Stm32Series::WBA => Some(SeriesConfig {
            configurable_initial_value: true,
            has_poly_size_bits: true,
            reversible_io: true,
            independent_data_width: 32,
        }),
        _ => None,
    }
}

/// `CRC_CR.POLYSIZE`.
fn poly_size_to_width(poly_size: u32) -> u32 {
    match poly_size & 3 {
        0 => 32,
        1 => 16,
        2 => 8,
        _ => 7,
    }
}

/// `BitHelper.ReverseBitsByByte`.
fn reverse_bits_by_byte(i: u32) -> u32 {
    let i = ((i >> 1) & 0x5555_5555) | ((i & 0x5555_5555) << 1);
    let i = ((i >> 2) & 0x3333_3333) | ((i & 0x3333_3333) << 2);
    ((i >> 4) & 0x0F0F_0F0F) | ((i & 0x0F0F_0F0F) << 4)
}

/// `BitHelper.ReverseBitsByWord`: bits reversed within each 16-bit half.
fn reverse_bits_by_word(i: u32) -> u32 {
    let i = reverse_bits_by_byte(i);
    ((i >> 8) & 0x00FF_00FF) | ((i & 0x00FF_00FF) << 8)
}

/// `BitHelper.ReverseBits(uint)`.
fn reverse_bits(i: u32) -> u32 {
    let i = reverse_bits_by_word(i);
    (i >> 16) | (i << 16)
}

/// `CRCConfig` of the STM32 CRC (`reflectInput` is always false, `xorOutput` 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CrcConfig {
    polynomial: u32,
    width: u32,
    reflect_output: bool,
    /// Already clamped to `width` bits.
    init: u32,
}

/// `CRCEngine`: MSB-first, table driven, register left-aligned in 32 bits.
struct CrcEngine {
    config: CrcConfig,
    table: Box<[u32; 256]>,
    /// `crc` (left aligned: the CRC occupies the top `width` bits).
    crc: u32,
}

impl CrcEngine {
    fn new(config: CrcConfig) -> Self {
        let mut engine = Self { config, table: Box::new(Self::table_for(config)), crc: 0 };
        engine.reset();
        engine
    }

    /// `GenerateLookupTable`.
    fn table_for(config: CrcConfig) -> [u32; 256] {
        let shifted_poly = config.polynomial << (32 - config.width);
        let mut table = [0u32; 256];
        for (dividend, entry) in table.iter_mut().enumerate() {
            let mut value = (dividend as u32) << 24;
            for _ in 0..8 {
                if value & 0x8000_0000 != 0 {
                    value <<= 1;
                    value ^= shifted_poly;
                } else {
                    value <<= 1;
                }
            }
            *entry = value;
        }
        table
    }

    /// Adopts a new configuration, keeping the table when polynomial and width are unchanged, and
    /// restarts from `init` (the effect of both branches of `ReloadCRCConfig`).
    fn reconfigure(&mut self, config: CrcConfig) {
        if config.polynomial != self.config.polynomial || config.width != self.config.width {
            *self.table = Self::table_for(config);
        }
        self.config = config;
        self.reset();
    }

    fn reset(&mut self) {
        self.crc = self.config.init << (32 - self.config.width);
    }

    /// `Update`: feeds bytes, first byte first.
    fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.crc ^= u32::from(byte) << 24;
            self.crc = (self.crc << 8) ^ self.table[(self.crc >> 24) as usize];
        }
    }

    /// `Value` (`XorOutput` is 0 here).
    fn value(&self) -> u32 {
        value_of(self.crc, self.config)
    }
}

/// `CRCEngine.Value` for a left-aligned register.
fn value_of(crc: u32, config: CrcConfig) -> u32 {
    if config.reflect_output {
        reverse_bits(crc)
    } else {
        crc >> (32 - config.width)
    }
}

/// Field indices of `CR` (`None` when the series has no such field).
#[derive(Clone, Copy, Debug)]
struct CrFields {
    poly_size: Option<usize>,
    rev_in: Option<usize>,
    rev_out: Option<usize>,
}

struct CrcDev {
    configurable_poly: bool,
    cr: CrFields,
    engine: Option<CrcEngine>,
    /// `crcConfigDirty`.
    dirty: bool,
}

/// Warn-once key of the invalid configuration error.
const KEY_BAD_CONFIG: u64 = 1 << 40;

impl CrcDev {
    /// The configuration `ReloadCRCConfig` would build from the register fields.
    fn config_from_registers(&self, regs: &RegisterFile) -> Result<CrcConfig, String> {
        let polynomial = regs.field(R_POL, 0);
        let width = self.cr.poly_size.map_or(32, |f| poly_size_to_width(regs.field(R_CR, f)));
        let reflect_output = self.cr.rev_out.is_some_and(|f| regs.field(R_CR, f) != 0);
        let init_mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
        let init = regs.field(R_INIT, 0) & init_mask;
        // `CRCPolynomial`: BitHelper.GetMostSignificantSetBitIndex(polynomial) + 1 must fit in the width.
        if 32 - polynomial.leading_zeros() > width {
            return Err(format!("CRCConfig: width ({width}) is too small for given polynomial 0x{polynomial:X}."));
        }
        Ok(CrcConfig { polynomial, width, reflect_output, init })
    }

    /// `ReloadCRCConfig`.
    fn reload(&mut self, regs: &RegisterFile, ctx: &mut Ctx<'_>) {
        match self.config_from_registers(regs) {
            Ok(config) => {
                match &mut self.engine {
                    Some(engine) => engine.reconfigure(config),
                    None => self.engine = Some(CrcEngine::new(config)),
                }
                self.dirty = false;
            }
            Err(message) => ctx.error_once(KEY_BAD_CONFIG | u64::from(regs.field(R_POL, 0)), format_args!("{message}")),
        }
    }

    /// The getter of the `CRC` property: rebuilds the engine first when it is missing or the configuration
    /// is dirty. False when the (invalid) configuration leaves no current engine: Renode's exception
    /// aborts the access at that point.
    fn refresh(&mut self, regs: &RegisterFile, ctx: &mut Ctx<'_>) -> bool {
        if self.engine.is_none() || self.dirty {
            self.reload(regs, ctx);
        }
        self.engine.is_some() && !self.dirty
    }

    /// `UpdateCRC(value, bytesCount)`: input bit reversal, then the bytes MSB first.
    fn update_crc(&mut self, regs: &RegisterFile, ctx: &mut Ctx<'_>, value: u32, bytes: u32) {
        let reversal = self.cr.rev_in.map_or(0, |f| regs.field(R_CR, f));
        let value = match (reversal, bytes) {
            (1, _) => reverse_bits_by_byte(value),
            (2, 1) => reverse_bits_by_byte(value),
            (2, _) => reverse_bits_by_word(value),
            (3, 1) => reverse_bits_by_byte(value),
            (3, 2) => reverse_bits_by_word(value),
            (3, _) => reverse_bits(value),
            _ => value,
        };
        // `BitHelper.GetBytesFromValue(value, bytesCount)`: most significant byte first.
        let big_endian = value.to_be_bytes();
        let data = &big_endian[(4 - bytes) as usize..];
        if self.refresh(regs, ctx) {
            if let Some(engine) = self.engine.as_mut() {
                engine.update(data);
            }
        }
    }

    /// The value a `DR` read returns, without rebuilding anything (what the rebuilt engine would report
    /// when the configuration is dirty).
    fn peek_data(&self, regs: &RegisterFile) -> u32 {
        match (&self.engine, self.dirty) {
            (Some(engine), false) => engine.value(),
            _ => match self.config_from_registers(regs) {
                Ok(config) => value_of(config.init << (32 - config.width), config),
                Err(_) => self.engine.as_ref().map_or(0, CrcEngine::value),
            },
        }
    }
}

impl Model for CrcDev {
    fn provide(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, _field: usize, current: u32) -> u32 {
        match reg {
            R_DR => {
                // With an invalid configuration the previous engine's value stands in for the exception.
                self.refresh(regs, ctx);
                self.engine.as_ref().map_or(0, CrcEngine::value)
            }
            // `RESET` always reads back as 0.
            R_CR => 0,
            _ => current,
        }
    }

    fn field_written(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, _field: usize, _old: u32, written: u32) {
        if reg == R_DR {
            // The 32-bit write; byte and halfword writes are handled in `Crc::write`.
            self.update_crc(regs, ctx, written, 4);
        }
    }

    fn register_written(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, _old: u32, _written: u32) {
        if reg == R_CR || reg == R_POL {
            self.dirty = true;
        }
    }
}

fn build_registers(conf: SeriesConfig, configurable_poly: bool) -> (RegisterFile, CrFields) {
    let mut cr = RegisterBuilder::new(reg::CR)
        // RESET: `FieldMode.Read | FieldMode.WriteOneToClear`, value provider always false.
        .field(0, 1, Mode::READ_WRITE_ONE_TO_CLEAR, Hooks::PROVIDER)
        .reserved(1, 2);
    let mut fields = CrFields { poly_size: None, rev_in: None, rev_out: None };
    let mut next_field = 1;
    if conf.has_poly_size_bits {
        cr = cr.field(3, 2, Mode::READ_WRITE, Hooks::NONE);
        fields.poly_size = Some(next_field);
        next_field += 1;
    } else {
        cr = cr.reserved(3, 2);
    }
    if conf.reversible_io {
        cr = cr.field(5, 2, Mode::READ_WRITE, Hooks::NONE).field(7, 1, Mode::READ_WRITE, Hooks::NONE);
        fields.rev_in = Some(next_field);
        fields.rev_out = Some(next_field + 1);
    } else {
        cr = cr.reserved(5, 3);
    }
    cr = cr.reserved(8, 24).on_write();

    let init_mode = if conf.configurable_initial_value { Mode::READ_WRITE } else { Mode::READ };
    let pol_mode = if configurable_poly { Mode::READ_WRITE } else { Mode::READ };
    let mut idr = RegisterBuilder::new(reg::IDR).tag("CRC_IDR", 0, conf.independent_data_width);
    if conf.independent_data_width != 32 {
        idr = idr.reserved(conf.independent_data_width, 32 - conf.independent_data_width);
    }
    let regs = RegisterFile::new(vec![
        RegisterBuilder::new(reg::DR).field(0, 32, Mode::READ_WRITE, Hooks::PROVIDER | Hooks::WRITE),
        cr,
        RegisterBuilder::new(reg::INIT).reset_value(DEFAULT_INITIAL_VALUE).field(0, 32, init_mode, Hooks::NONE),
        RegisterBuilder::new(reg::POL).reset_value(DEFAULT_POLYNOMIAL).field(0, 32, pol_mode, Hooks::NONE).on_write(),
        idr,
    ]);
    (regs, fields)
}

/// `CRC.STM32_CRC`.
pub struct Crc {
    name: String,
    regs: RegisterFile,
    dev: CrcDev,
}

impl Crc {
    /// Builds the CRC unit of `series` (F0, F4 and WBA are supported); panics on another series (Renode
    /// throws a `ConstructionException`).
    pub fn new(name: impl Into<String>, series: Stm32Series, configurable_poly: bool) -> Self {
        match Self::try_new(name, series, configurable_poly) {
            Ok(crc) => crc,
            Err(message) => panic!("cannot construct STM32_CRC: {message}"),
        }
    }

    pub fn try_new(name: impl Into<String>, series: Stm32Series, configurable_poly: bool) -> Result<Self, String> {
        let conf = series_config(series).ok_or_else(|| format!("Unknown STM32 series value: {series}!"))?;
        let (regs, cr) = build_registers(conf, configurable_poly);
        Ok(Self { name: name.into(), regs, dev: CrcDev { configurable_poly, cr, engine: None, dirty: false } })
    }

    /// Whether `POL` is writable (`configurablePoly`).
    pub fn configurable_poly(&self) -> bool {
        self.dev.configurable_poly
    }

    /// The value a read of `DR` would return (see [`CrcDev::peek_data`]).
    pub fn data(&self) -> u32 {
        self.dev.peek_data(&self.regs)
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        match self.dev.config_from_registers(&self.regs) {
            Ok(config) => format!(
                "{}: CRC=0x{:X} width={} polynomial=0x{:X} init=0x{:X} reverse_output={}{}",
                self.name,
                self.data(),
                config.width,
                config.polynomial,
                config.init,
                u8::from(config.reflect_output),
                if self.dev.dirty { " (pending reload)" } else { "" }
            ),
            Err(message) => format!("{}: invalid configuration ({message})", self.name),
        }
    }

    /// What a read of the register at `offset` returns, without side effects (0 where Renode logs an
    /// unhandled read).
    fn peek_register(&self, offset: u32) -> u32 {
        match offset {
            reg::DR => self.data(),
            // `RESET` reads 0; the other fields are stored.
            reg::CR => self.regs.value(R_CR) & !1,
            reg::INIT => self.regs.value(R_INIT),
            reg::POL => self.regs.value(R_POL),
            // Tags only: nothing is stored.
            reg::IDR => self.regs.value(R_IDR),
            _ => 0,
        }
    }
}

impl Peripheral for Crc {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset`: registers to their reset values, then the engine is rebuilt (CRC = `INIT`).
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.regs.reset();
        self.dev.reload(&self.regs, ctx);
    }

    /// Byte and halfword reads are a full register read cut to the width.
    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
        let value = self.regs.read(offset, &mut self.dev, ctx);
        value & width.mask()
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match width {
            Width::Word => self.regs.write(offset, value, &mut self.dev, ctx),
            Width::Byte | Width::Half => {
                if offset == reg::DR {
                    self.dev.update_crc(&self.regs, ctx, value, width.bytes());
                } else {
                    log_unhandled_write(ctx, offset, value);
                }
            }
        }
    }

    // IBytePeripheral + IWordPeripheral + IDoubleWordPeripheral.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::EXACT
    }

    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        Some(self.peek_register(offset) & width.mask())
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
    use emu_core::{LogLevel, PeriphId};

    const BASE: u32 = 0x4002_3000;

    const DR: u32 = BASE + reg::DR;
    const IDR: u32 = BASE + reg::IDR;
    const CR: u32 = BASE + reg::CR;
    const INIT: u32 = BASE + reg::INIT;
    const POL: u32 = BASE + reg::POL;

    const CR_RESET: u32 = 1;
    const CR_POLY_32: u32 = 0 << 3;
    const CR_POLY_16: u32 = 1 << 3;
    const CR_POLY_8: u32 = 2 << 3;
    const CR_POLY_7: u32 = 3 << 3;
    const CR_REV_IN_BYTE: u32 = 1 << 5;
    const CR_REV_IN_HALF: u32 = 2 << 5;
    const CR_REV_IN_WORD: u32 = 3 << 5;
    const CR_REV_OUT: u32 = 1 << 7;

    fn f0() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, Crc::new("crc", Stm32Series::F0, true));
        (h, id)
    }

    /// Programs width/polynomial/init/reversal like `HAL_CRC_Init` and resets the unit.
    fn configure(h: &mut Harness, cr: u32, polynomial: u32, init: u32) {
        h.write32(POL, polynomial);
        h.write32(INIT, init);
        h.write32(CR, cr | CR_RESET);
    }

    fn feed_bytes(h: &mut Harness, data: &[u8]) {
        for &byte in data {
            h.write8(DR, u32::from(byte));
        }
    }

    // ---- registers -----------------------------------------------------------------------

    #[test]
    fn reset_values_and_register_access() {
        let (mut h, id) = f0();
        assert_eq!(h.read32(DR), 0xFFFF_FFFF, "DR reads the CRC, which starts at INIT");
        assert_eq!(h.read32(IDR), 0);
        assert_eq!(h.read32(CR), 0);
        assert_eq!(h.read32(INIT), 0xFFFF_FFFF);
        assert_eq!(h.read32(POL), 0x04C1_1DB7);
        assert!(h.warnings().is_empty());
        // Everything is writable on F0 with a configurable polynomial.
        h.write32(INIT, 0x1234_5678);
        h.write32(POL, 0x1021);
        assert_eq!(h.read32(INIT), 0x1234_5678);
        assert_eq!(h.read32(POL), 0x1021);
        h.write32(CR, CR_POLY_16 | CR_REV_IN_HALF | CR_REV_OUT);
        assert_eq!(h.read32(CR), CR_POLY_16 | CR_REV_IN_HALF | CR_REV_OUT);
        assert!(h.get::<Crc>(id).configurable_poly());
    }

    #[test]
    fn reset_bit_reads_zero_and_every_cr_write_restarts_the_crc() {
        let (mut h, _id) = f0();
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        h.write32(CR, CR_RESET);
        assert_eq!(h.read32(CR), 0, "RESET is read as 0");
        assert_eq!(h.read32(DR), 0xFFFF_FFFF, "back at INIT");
        // A CR write without RESET resets as well (Renode reloads the engine on any CR write).
        h.write32(DR, 0x1234_5678);
        h.write32(CR, 0);
        assert_eq!(h.read32(DR), 0xFFFF_FFFF);
    }

    #[test]
    fn init_alone_does_not_restart_the_crc_but_the_next_cr_write_picks_it_up() {
        let (mut h, _id) = f0();
        h.write32(DR, 0);
        let before = h.read32(DR);
        assert_eq!(before, 0xC704_DD7B);
        h.write32(INIT, 0);
        assert_eq!(h.read32(DR), before, "INIT is stored but the engine is not touched");
        h.write32(CR, CR_RESET);
        assert_eq!(h.read32(DR), 0, "INIT = 0 now");
        h.write32(DR, 0);
        assert_eq!(h.read32(DR), 0, "CRC-32/MPEG-2 with init 0 over zeroes stays 0");
    }

    #[test]
    fn polynomial_write_restarts_the_crc_at_the_next_access() {
        let (mut h, _id) = f0();
        h.write32(DR, 0xDEAD_BEEF);
        h.write32(POL, 0x04C1_1DB7);
        assert_eq!(h.read32(DR), 0xFFFF_FFFF);
    }

    #[test]
    fn fixed_polynomial_register_is_read_only_but_still_marks_the_config_dirty() {
        let mut h = Harness::new();
        h.add_mapped(BASE, 0x400, Crc::new("crc", Stm32Series::F0, false));
        h.write32(POL, 0x1021);
        assert_eq!(h.read32(POL), 0x04C1_1DB7, "configurablePoly=false: POL ignores writes");
        h.write32(DR, 1);
        h.write32(POL, 0);
        assert_eq!(h.read32(DR), 0xFFFF_FFFF, "the write callback still ran: CRC restarted");
    }

    #[test]
    fn independent_data_register_has_no_storage() {
        let (mut h, _id) = f0();
        h.write32(IDR, 0xA5);
        assert_eq!(h.read32(IDR), 0);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x4. Unhandled bits: [0, 2, 5, 7] when writing value 0xA5. Tags: CRC_IDR (0xA5)."]);
        h.write32(IDR, 0x1_0000);
        assert!(h.warnings()[1].ends_with("Tags: RESERVED (0x100)."), "{:?}", h.warnings());
    }

    #[test]
    fn reserved_cr_bits_warn() {
        let (mut h, _id) = f0();
        h.write32(CR, 0x0000_0106);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "Unhandled write to offset 0x8. Unhandled bits: [1-2, 8] when writing value 0x106. Tags: RESERVED (0x3), RESERVED (0x1)."
        );
    }

    #[test]
    fn unimplemented_offsets_log_and_read_zero() {
        let (mut h, _id) = f0();
        assert_eq!(h.read32(BASE + 0xC), 0);
        h.write32(BASE + 0xC, 7);
        assert_eq!(h.read32(BASE + 0x18), 0);
        assert_eq!(
            h.warnings(),
            ["Unhandled read from offset 0xC.", "Unhandled write to offset 0xC, value 0x7.", "Unhandled read from offset 0x18."]
        );
    }

    // ---- sub-word accesses ------------------------------------------------------------------

    #[test]
    fn byte_and_halfword_writes_feed_the_crc_at_dr_only() {
        let (mut h, _id) = f0();
        // Four byte writes equal one word write (MSB first).
        feed_bytes(&mut h, &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        h.write32(CR, CR_RESET);
        h.write16(DR, 0x1234);
        h.write16(DR, 0x5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        // Other offsets: Unhandled write, no state change.
        h.write8(BASE + 0x1, 0xFF);
        h.write16(BASE + 0x10, 0x1234);
        assert_eq!(h.read32(INIT), 0xFFFF_FFFF, "a halfword write to INIT does nothing");
        let warnings = h.warnings();
        assert_eq!(warnings, ["Unhandled write to offset 0x1, value 0xFF.", "Unhandled write to offset 0x10, value 0x1234."]);
    }

    #[test]
    fn matches_the_access_probe_of_the_renode_reference_run() {
        // Access probe recorded from Renode 1.17.0 on 2026-10-08: `sysbus WriteByte 0x40023000 0x12` then
        // `sysbus ReadByte 0x40023000` replied 0xAA (native byte accesses, no log).
        let (mut h, _id) = f0();
        h.write8(DR, 0x12);
        assert_eq!(h.read8(DR), 0xAA);
        assert_eq!(h.read32(DR), 0x0B9B_5FAA);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn byte_and_halfword_reads_are_register_reads_cut_to_the_width() {
        let (mut h, _id) = f0();
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        assert_eq!(h.read8(DR), 0x2B);
        assert_eq!(h.read16(DR), 0x8A2B);
        // Only exactly aligned register offsets work: DR+1 is not a register.
        assert_eq!(h.read8(DR + 1), 0);
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x1."]);
        assert_eq!(h.read8(CR), 0);
        assert_eq!(h.read16(POL), 0x1DB7);
        assert_eq!(h.read8(INIT), 0xFF);
    }

    // ---- known vectors -----------------------------------------------------------------------

    const CHECK: &[u8] = b"123456789";

    fn check_vector(cr: u32, polynomial: u32, init: u32, expected: u32, what: &str) {
        let (mut h, _id) = f0();
        configure(&mut h, cr, polynomial, init);
        feed_bytes(&mut h, CHECK);
        assert_eq!(h.read32(DR), expected, "{what}");
    }

    #[test]
    fn stm32_default_configuration_is_crc32_mpeg2() {
        let (mut h, _id) = f0();
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        h.write32(CR, CR_RESET);
        h.write32(DR, 0);
        assert_eq!(h.read32(DR), 0xC704_DD7B);
        check_vector(CR_POLY_32, 0x04C1_1DB7, 0xFFFF_FFFF, 0x0376_E6E7, "CRC-32/MPEG-2");
    }

    #[test]
    fn catalog_check_values_for_every_polynomial_size_and_reversal() {
        // Values from the CRC catalog / an independent bit-serial implementation: CRC of "123456789".
        check_vector(CR_POLY_32 | CR_REV_IN_BYTE | CR_REV_OUT, 0x04C1_1DB7, 0xFFFF_FFFF, 0x340B_C6D9, "CRC-32/JAMCRC (zlib CRC-32 without the final xor)");
        check_vector(CR_POLY_32 | CR_REV_IN_BYTE | CR_REV_OUT, 0x1EDC_6F41, 0xFFFF_FFFF, 0x1CF9_6D7C, "CRC-32C without the final xor");
        check_vector(CR_POLY_16, 0x1021, 0xFFFF, 0x29B1, "CRC-16/CCITT-FALSE");
        check_vector(CR_POLY_16, 0x1021, 0x0000, 0x31C3, "CRC-16/XMODEM");
        check_vector(CR_POLY_16 | CR_REV_IN_BYTE | CR_REV_OUT, 0x8005, 0x0000, 0xBB3D, "CRC-16/ARC");
        check_vector(CR_POLY_16, 0x8005, 0x0000, 0xFEE8, "CRC-16/BUYPASS");
        check_vector(CR_POLY_16 | CR_REV_IN_BYTE | CR_REV_OUT, 0x8005, 0xFFFF, 0x4B37, "CRC-16/MODBUS");
        check_vector(CR_POLY_8, 0x07, 0x00, 0xF4, "CRC-8");
        check_vector(CR_POLY_8 | CR_REV_IN_BYTE | CR_REV_OUT, 0x07, 0xFF, 0xD0, "CRC-8/ROHC");
        check_vector(CR_POLY_7, 0x09, 0x00, 0x75, "CRC-7/MMC");
        check_vector(CR_POLY_7, 0x45, 0x00, 0x61, "CRC-7/UMTS");
    }

    #[test]
    fn init_is_clamped_to_the_polynomial_width() {
        let (mut h, _id) = f0();
        configure(&mut h, CR_POLY_16, 0x1021, 0xFFFF_FFFF);
        assert_eq!(h.read32(DR), 0xFFFF, "16-bit CRC: only the low 16 bits of INIT");
        configure(&mut h, CR_POLY_7, 0x09, 0xFFFF_FFFF);
        assert_eq!(h.read32(DR), 0x7F);
        configure(&mut h, CR_POLY_8 | CR_REV_OUT, 0x07, 0x01);
        assert_eq!(h.read32(DR), 0x80, "REV_OUT reverses the 8-bit register");
        configure(&mut h, CR_POLY_32 | CR_REV_OUT, 0x04C1_1DB7, 0x0000_0001);
        assert_eq!(h.read32(DR), 0x8000_0000);
    }

    // ---- exhaustive comparison with an independent implementation --------------------------------

    /// Bit-serial, bit-by-bit CRC with the reversal rules written out directly (no shared code with the model).
    fn reference(width: u32, poly: u32, init: u32, rev_in: u32, rev_out: bool, units: &[(u32, u32)]) -> u32 {
        let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
        let mut crc = init & mask;
        for &(value, bytes) in units {
            // Bits of the unit, as a vector indexed by bit number, after the input reversal.
            let mut bits: Vec<bool> = (0..bytes * 8).map(|i| value >> i & 1 != 0).collect();
            let group = match (rev_in, bytes) {
                (0, _) => 0,
                (1, _) | (2, 1) | (3, 1) => 8,
                (2, _) | (3, 2) => 16,
                (3, _) => 32,
                _ => unreachable!(),
            };
            if group != 0 {
                for chunk in bits.chunks_mut(group as usize) {
                    chunk.reverse();
                }
            }
            // MSB first.
            for i in (0..bytes * 8).rev() {
                let input = bits[i as usize];
                let top = crc >> (width - 1) & 1 != 0;
                crc = (crc << 1) & mask;
                if top != input {
                    crc ^= poly;
                }
            }
        }
        if rev_out {
            (0..width).fold(0, |acc, i| acc | (crc >> i & 1) << (width - 1 - i))
        } else {
            crc
        }
    }

    /// SplitMix64 for the test data.
    struct Rand(u64);

    impl Rand {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn every_configuration_matches_the_independent_implementation() {
        let mut rand = Rand(0x5EED);
        let sizes = [(CR_POLY_32, 32u32, 0x04C1_1DB7u32), (CR_POLY_32, 32, 0x1EDC_6F41), (CR_POLY_16, 16, 0x1021), (CR_POLY_16, 16, 0x8005), (CR_POLY_8, 8, 0x07), (CR_POLY_8, 8, 0x31), (CR_POLY_7, 7, 0x09), (CR_POLY_7, 7, 0x45)];
        let reversals = [(0, 0), (CR_REV_IN_BYTE, 1), (CR_REV_IN_HALF, 2), (CR_REV_IN_WORD, 3)];
        let mut cases = 0;
        for &(size_bits, width, poly) in &sizes {
            for &(rev_in_bits, rev_in) in &reversals {
                for rev_out in [false, true] {
                    let (mut h, _id) = f0();
                    let init = rand.next() as u32;
                    let cr = size_bits | rev_in_bits | if rev_out { CR_REV_OUT } else { 0 };
                    configure(&mut h, cr, poly, init);
                    let mut units = Vec::new();
                    for _ in 0..24 {
                        let r = rand.next();
                        let (bytes, value) = match r % 3 {
                            0 => (1u32, (r >> 8) as u32 & 0xFF),
                            1 => (2, (r >> 8) as u32 & 0xFFFF),
                            _ => (4, (r >> 8) as u32),
                        };
                        match bytes {
                            1 => h.write8(DR, value),
                            2 => h.write16(DR, value),
                            _ => h.write32(DR, value),
                        }
                        units.push((value, bytes));
                        let expected = reference(width, poly, init, rev_in, rev_out, &units);
                        assert_eq!(
                            h.read32(DR),
                            expected,
                            "width {width} poly 0x{poly:X} rev_in {rev_in} rev_out {rev_out} init 0x{init:X} after {units:X?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 8 * 4 * 2 * 24);
    }

    #[test]
    fn input_reversal_applies_to_the_size_of_each_write() {
        // ByWord with a byte write reverses within the byte; ByDoubleWord with a halfword reverses 16 bits.
        for (rev, cr_bits) in [(1, CR_REV_IN_BYTE), (2, CR_REV_IN_HALF), (3, CR_REV_IN_WORD)] {
            let (mut h, _id) = f0();
            configure(&mut h, CR_POLY_16 | cr_bits, 0x1021, 0xFFFF);
            h.write8(DR, 0x01);
            assert_eq!(h.read32(DR), reference(16, 0x1021, 0xFFFF, rev, false, &[(0x01, 1)]), "byte, mode {rev}");
            h.write16(DR, 0x0102);
            assert_eq!(h.read32(DR), reference(16, 0x1021, 0xFFFF, rev, false, &[(0x01, 1), (0x0102, 2)]), "half, mode {rev}");
            h.write32(DR, 0x0102_0304);
            assert_eq!(
                h.read32(DR),
                reference(16, 0x1021, 0xFFFF, rev, false, &[(0x01, 1), (0x0102, 2), (0x0102_0304, 4)]),
                "word, mode {rev}"
            );
        }
    }

    // ---- error handling and other series -----------------------------------------------------------

    #[test]
    fn polynomial_wider_than_polysize_is_an_error_and_keeps_the_previous_engine() {
        let (mut h, _id) = f0();
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        // 16-bit size with the 32-bit polynomial: Renode's CRCPolynomial throws.
        h.write32(CR, CR_POLY_16);
        h.write32(DR, 1);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B, "the previous engine is kept, the write was dropped");
        let errors: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == LogLevel::Error).map(|e| e.message).collect();
        assert_eq!(errors, ["CRCConfig: width (16) is too small for given polynomial 0x4C11DB7."]);
        // Fixing the polynomial recovers (the pending reload then succeeds).
        h.write32(POL, 0x1021);
        h.write32(DR, 0);
        assert_eq!(h.read32(DR), reference(16, 0x1021, 0xFFFF_FFFF, 0, false, &[(0, 4)]));
    }

    #[test]
    fn f4_series_has_fixed_init_and_no_reversal_fields() {
        let mut h = Harness::new();
        h.add_mapped(BASE, 0x400, Crc::new("crc", Stm32Series::F4, false));
        h.write32(INIT, 0);
        assert_eq!(h.read32(INIT), 0xFFFF_FFFF, "read-only INIT");
        h.write32(CR, CR_POLY_16 | CR_REV_IN_BYTE | CR_REV_OUT);
        assert_eq!(h.read32(CR), 0, "POLYSIZE/REV bits are reserved on F4");
        assert_eq!(
            h.warnings(),
            ["Unhandled write to offset 0x8. Unhandled bits: [3, 5, 7] when writing value 0xA8. Tags: RESERVED (0x1), RESERVED (0x5)."]
        );
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.read32(DR), 0xDF8A_8A2B);
        h.write8(IDR, 1);
        assert_eq!(h.warnings().last().unwrap(), "Unhandled write to offset 0x4, value 0x1.");
    }

    #[test]
    fn wba_series_has_a_32_bit_independent_data_register() {
        let mut h = Harness::new();
        h.add_mapped(BASE, 0x400, Crc::new("crc", Stm32Series::WBA, true));
        h.write32(IDR, 0x8000_0001);
        assert_eq!(h.warnings(), ["Unhandled write to offset 0x4. Unhandled bits: [0, 31] when writing value 0x80000001. Tags: CRC_IDR (0x80000001)."]);
    }

    #[test]
    fn unsupported_series_are_rejected() {
        assert!(Crc::try_new("crc", Stm32Series::F7, true).is_err());
        assert_eq!(Crc::try_new("crc", Stm32Series::L1, true).err().unwrap(), "Unknown STM32 series value: L1!");
        assert!(Crc::try_new("crc", Stm32Series::F0, true).is_ok());
    }

    // ---- reset, peek, summary --------------------------------------------------------------------------

    #[test]
    fn reset_restores_registers_and_restarts_the_crc() {
        let (mut h, _id) = f0();
        configure(&mut h, CR_POLY_16 | CR_REV_OUT, 0x1021, 0x1234);
        h.write32(DR, 0xFFFF);
        h.core_mut().reset_all();
        assert_eq!(h.read32(CR), 0);
        assert_eq!(h.read32(POL), 0x04C1_1DB7);
        assert_eq!(h.read32(INIT), 0xFFFF_FFFF);
        assert_eq!(h.read32(DR), 0xFFFF_FFFF);
    }

    #[test]
    fn peek_reports_what_a_read_would_return_without_rebuilding() {
        let (mut h, id) = f0();
        h.write32(DR, 0x1234_5678);
        assert_eq!(h.peek(DR, Width::Word), Some(0xDF8A_8A2B));
        assert_eq!(h.peek(DR, Width::Byte), Some(0x2B));
        // A CR write makes the config dirty: the peek already shows the restarted value ...
        h.write32(CR, CR_POLY_16 | CR_REV_OUT);
        h.write32(POL, 0x1021);
        h.write32(INIT, 0x0001);
        // ... (init is read at the *reload*, which has not happened yet, so peek uses the current fields)
        assert_eq!(h.peek(DR, Width::Word), Some(0x8000), "reflected 16-bit init 0x0001");
        assert!(h.get::<Crc>(id).describe().contains("(pending reload)"));
        // ... and reading it really rebuilds the engine with the same value.
        assert_eq!(h.read32(DR), 0x8000);
        assert!(!h.get::<Crc>(id).describe().contains("pending"));
        assert_eq!(h.peek(CR, Width::Word), Some(CR_POLY_16 | CR_REV_OUT));
        assert_eq!(h.peek(POL, Width::Word), Some(0x1021));
        assert_eq!(h.peek(INIT, Width::Word), Some(1));
        assert_eq!(h.peek(IDR, Width::Word), Some(0));
    }

    #[test]
    fn summary_describes_the_configuration() {
        let (mut h, id) = f0();
        configure(&mut h, CR_POLY_8, 0x07, 0x0);
        let _ = h.read32(DR);
        assert_eq!(h.get::<Crc>(id).describe(), "crc: CRC=0x0 width=8 polynomial=0x7 init=0x0 reverse_output=0");
        h.write32(CR, CR_POLY_7);
        h.write32(POL, 0x80);
        assert!(h.get::<Crc>(id).describe().contains("invalid configuration"));
        assert!(h.core().summaries().iter().any(|(n, s)| n == "crc" && s.contains("invalid configuration")));
    }

    #[test]
    fn access_policy_accepts_every_width() {
        let crc = Crc::new("crc", Stm32Series::F0, true);
        let policy = crc.access_policy();
        assert!(policy.native.contains(Width::Byte) && policy.native.contains(Width::Half) && policy.native.contains(Width::Word));
    }
}
