// Ported from emulation/models/NGCEeprom.cs.

//! `NGCEepromStore` and `NGCEepromBank`: the banked 24C-style I2C EEPROM of main 5.8.
//!
//! The exact chip size is unverified; eight 256-byte banks (2 048 bytes) at I2C addresses `0x50..=0x57`
//! are an explicit emulator capacity fixture (`emulation/main-storage-sensors.md`). The firmware forms
//! the device address as `((offset >> 8) + 0x50) * 2` and sends the low offset byte as the pointer.
//!
//! * [`NgcEepromStore`] is the storage: 2 048 cells, erased to `0xFF`, preserved by `Reset` (power loss
//!   keeps nonvolatile cells). Mapped on the system bus at [`EEPROM_DIAGNOSTIC_BASE`] (`0xF0001000`,
//!   byte access only) it is a *diagnostic view*: application access goes through I2C. Typed methods
//!   mirror the C# class (`GetByte`, `SetByte`/`SetWord`/`SetDoubleWord`, `Erase`, `Flush`, `Summary`,
//!   `WriteCount`, `AutoPersist`, `BackingPath`). File I/O is not done here: the persistence layer
//!   moves the raw 2 048-byte `eeprom.bin` image in and out ([`NgcEepromStore::load_backing`],
//!   [`NgcEepromStore::load_image`], [`NgcEepromStore::image`]) and polls
//!   [`NgcEepromStore::take_persist_request`], which stands for the C# `Flush()`/`LoadBackingFile()`
//!   that rewrote the backing file after every completed I2C write transaction.
//! * [`NgcEepromBank`] is the I2C target of one bank (Renode `II2CPeripheral`), attached to the
//!   controller at `0x50 + bank`. It keeps a pointer that a transaction sets **once**: the first
//!   non-empty write chunk after a STOP or a read supplies the pointer byte, later chunks of the same
//!   transaction (the STM32F7 controller delivers the HAL memory-write path as an address chunk and a
//!   data chunk) are data. Writes wrap inside the 16-byte page of the pointer, reads advance linearly
//!   inside the bank (pointer modulo 256). An empty chunk is the address-only readiness probe: it is
//!   acknowledged and changes nothing. `FinishTransmission` flushes the store if the transaction wrote.
//!
//! Sharing: a bank holds a clone of the store, and clones share the same cells (the C# object
//! reference). The types are single-threaded (`Rc`), like the machine.
//!
//! The firmware's duplicate write of setting 0x14 (size 2 against a table entry of size 1, which makes
//! it print `READ: INVALID SIZE` on a fresh profile) is rejected by the *firmware's* own table check
//! before any I2C traffic (`emulation/storage-clock-investigation.md`); the model has no size check
//! and never sees it, so there is nothing here to reproduce or to fix.
//!
//! # Verification and reference values
//!
//! `crates/ngc/tests/renode_i2c_eeprom` replays register traffic (HAL flows, page wraps, bank addressing,
//! random noise) recorded from the unmodified C# models and requires identical store images and call
//! results (the recorded cases are committed, see `testdata/README.md`). In a Renode 1.17.0 dual-wake run
//! recorded on 2026-10-08 the firmware's first initialization made `Summary` go from `writes=0` (at 1.0 s)
//! to `writes=184` (at 1.05 s), and `writes=196` from 3 s on; the final `eeprom.bin` had 173 programmed
//! bytes (SHA-256 prefix `bbcb0517`), marker `0xA3` at offset 254 and an erased serial number.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, Translations, View, Width, Widths};
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use stm32::i2c::{I2cCtx, I2cError, I2cTarget, Stm32F7I2c};
use stm32::impl_i2c_target_any;

/// Capacity of the store in bytes (`new byte[2048]`).
pub const EEPROM_SIZE: usize = 2048;
/// Number of I2C banks.
pub const EEPROM_BANKS: usize = 8;
/// Bytes per bank.
pub const EEPROM_BANK_SIZE: usize = 256;
/// Page size of a write (writes wrap inside the 16-byte page of the pointer).
pub const EEPROM_PAGE_SIZE: usize = 16;
/// I2C address of bank 0 (bank `n` answers at `0x50 + n`).
pub const EEPROM_FIRST_BANK_ADDRESS: u32 = 0x50;
/// Diagnostic bus window of the store on the main board (`eepromStore @ sysbus 0xf0001000`).
pub const EEPROM_DIAGNOSTIC_BASE: u32 = 0xF000_1000;

/// Why an EEPROM operation was refused (the C# exceptions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EepromError {
    /// `ArgumentOutOfRangeException("offset")`.
    OffsetOutOfRange(u32),
    /// `ArgumentException("EEPROM image must contain exactly 2048 bytes")`; carries the length given.
    BadImageSize(usize),
    /// `ArgumentOutOfRangeException("bank")`.
    BankOutOfRange(usize),
}

impl fmt::Display for EepromError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EepromError::OffsetOutOfRange(offset) => {
                write!(f, "EEPROM offset {offset} is outside the {EEPROM_SIZE}-byte store")
            }
            EepromError::BadImageSize(len) => {
                write!(f, "EEPROM image must contain exactly {EEPROM_SIZE} bytes (got {len})")
            }
            EepromError::BankOutOfRange(bank) => write!(f, "EEPROM bank {bank} is outside 0..={}", EEPROM_BANKS - 1),
        }
    }
}

impl std::error::Error for EepromError {}

struct Cells {
    data: [u8; EEPROM_SIZE],
    /// `WriteCount`: every `SetByte` (I2C writes, monitor setters, the diagnostic view).
    write_count: u64,
    auto_persist: bool,
    /// `BackingPath`: a label chosen by the persistence layer.
    backing: Option<String>,
    persist_pending: bool,
}

/// `NGCEepromStore`: the shared 2 048-byte storage and its bus peripheral. Cloning it yields another
/// handle to the same cells; banks hold such a clone.
#[derive(Clone)]
pub struct NgcEepromStore {
    name: String,
    cells: Rc<RefCell<Cells>>,
}

impl Default for NgcEepromStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NgcEepromStore {
    /// `new NGCEepromStore()` named `eepromStore` (the `.repl` name): all cells erased (`0xFF`).
    pub fn new() -> Self {
        Self::named("eepromStore")
    }

    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cells: Rc::new(RefCell::new(Cells {
                data: [0xFF; EEPROM_SIZE],
                write_count: 0,
                auto_persist: true,
                backing: None,
                persist_pending: false,
            })),
        }
    }

    /// `new NGCEepromBank(this, bank)` for the I2C controller (attach it at `0x50 + bank`).
    pub fn new_bank(&self, bank: usize) -> Result<NgcEepromBank, EepromError> {
        NgcEepromBank::new(self, bank)
    }

    /// Attaches the eight banks to `controller` at `0x50..=0x57`: the `eeprom0`..`eeprom7` lines of `main.repl`.
    pub fn attach_banks(&self, controller: &mut Stm32F7I2c) -> Result<(), I2cError> {
        for bank in 0..EEPROM_BANKS {
            let target = NgcEepromBank::new(self, bank).expect("bank index is in range");
            controller.attach(EEPROM_FIRST_BANK_ADDRESS + bank as u32, Box::new(target))?;
        }
        Ok(())
    }

    /// `AutoPersist` (default true): whether `Flush` requests a save.
    pub fn auto_persist(&self) -> bool {
        self.cells.borrow().auto_persist
    }

    pub fn set_auto_persist(&self, enabled: bool) {
        self.cells.borrow_mut().auto_persist = enabled;
    }

    /// `BackingPath`: the label given to [`load_backing`](NgcEepromStore::load_backing), if any.
    pub fn backing_path(&self) -> Option<String> {
        self.cells.borrow().backing.clone()
    }

    /// `WriteCount`: number of byte writes since the store was created (not reset by `Erase`/loads).
    pub fn write_count(&self) -> u64 {
        self.cells.borrow().write_count
    }

    /// `GetByte`.
    pub fn get_byte(&self, offset: u32) -> Result<u8, EepromError> {
        self.cells.borrow().data.get(offset as usize).copied().ok_or(EepromError::OffsetOutOfRange(offset))
    }

    /// `SetByte`: stores the byte and counts a write.
    pub fn set_byte(&self, offset: u32, value: u8) -> Result<(), EepromError> {
        let mut cells = self.cells.borrow_mut();
        let cell = cells.data.get_mut(offset as usize).ok_or(EepromError::OffsetOutOfRange(offset))?;
        *cell = value;
        cells.write_count += 1;
        Ok(())
    }

    /// `SetWord`: little-endian, two `SetByte` calls (the first stays written if the second is out of range).
    pub fn set_word(&self, offset: u32, value: u16) -> Result<(), EepromError> {
        self.set_byte(offset, value as u8)?;
        self.set_byte(offset.wrapping_add(1), (value >> 8) as u8)
    }

    /// `SetDoubleWord`: little-endian, four `SetByte` calls.
    pub fn set_double_word(&self, offset: u32, value: u32) -> Result<(), EepromError> {
        for i in 0..4u32 {
            self.set_byte(offset.wrapping_add(i), (value >> (8 * i)) as u8)?;
        }
        Ok(())
    }

    /// Four `GetByte` calls combined little-endian (the runner's `serialNumber` computation; no C# method).
    pub fn get_double_word(&self, offset: u32) -> Result<u32, EepromError> {
        let mut value = 0u32;
        for i in 0..4u32 {
            value |= u32::from(self.get_byte(offset.wrapping_add(i))?) << (8 * i);
        }
        Ok(value)
    }

    /// `Erase`: every cell back to `0xFF` (does not touch `WriteCount`).
    pub fn erase(&self) {
        self.cells.borrow_mut().data = [0xFF; EEPROM_SIZE];
    }

    /// `LoadBackingFile(path)`: remembers `backing` as `BackingPath`, then loads `image` if the persistence
    /// layer found one (exactly 2 048 bytes, else [`EepromError::BadImageSize`] with the label already
    /// set, as in C#) or, when the backing file does not exist yet (`None`), requests that the current
    /// cells be written to it.
    pub fn load_backing(&self, backing: &str, image: Option<&[u8]>) -> Result<(), EepromError> {
        let mut cells = self.cells.borrow_mut();
        cells.backing = Some(backing.to_string());
        match image {
            Some(image) => {
                if image.len() != EEPROM_SIZE {
                    return Err(EepromError::BadImageSize(image.len()));
                }
                cells.data.copy_from_slice(image);
            }
            None => cells.persist_pending = true,
        }
        Ok(())
    }

    /// Copies a raw 2 048-byte `eeprom.bin` image into the cells without touching `BackingPath` or `WriteCount`.
    pub fn load_image(&self, image: &[u8]) -> Result<(), EepromError> {
        if image.len() != EEPROM_SIZE {
            return Err(EepromError::BadImageSize(image.len()));
        }
        self.cells.borrow_mut().data.copy_from_slice(image);
        Ok(())
    }

    /// The raw 2 048-byte image (what `SaveBackingFile` writes).
    pub fn image(&self) -> [u8; EEPROM_SIZE] {
        self.cells.borrow().data
    }

    /// `Flush`: when `AutoPersist` is on and a backing label is set, requests a save of the image.
    pub fn flush(&self) {
        let mut cells = self.cells.borrow_mut();
        if cells.auto_persist && cells.backing.is_some() {
            cells.persist_pending = true;
        }
    }

    /// True once after [`flush`](NgcEepromStore::flush) (or a `load_backing` without an image) asked for the
    /// image to be saved; the persistence layer then writes [`image`](NgcEepromStore::image) to the backing store.
    pub fn take_persist_request(&self) -> bool {
        std::mem::take(&mut self.cells.borrow_mut().persist_pending)
    }

    /// C# `Summary`.
    pub fn summary_text(&self) -> String {
        let cells = self.cells.borrow();
        format!(
            "EEPROM fixture: bytes={EEPROM_SIZE}; writes={}; backing={}",
            cells.write_count,
            cells.backing.as_deref().unwrap_or("volatile process storage")
        )
    }
}

impl Peripheral for NgcEepromStore {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset()`: nothing, power loss preserves nonvolatile cells.
    fn reset(&mut self, _ctx: &mut Ctx<'_>) {}

    // `IBytePeripheral` only, no `[AllowedTranslations]`: halfword and word accesses are not supported.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::new(Widths::BYTE, Translations::NONE)
    }

    /// `ReadByte(offset)` = `GetByte`.
    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.get_byte(offset) {
            Ok(value) => u32::from(value),
            Err(error) => {
                // C# throws ArgumentOutOfRangeException; the bus window is exactly 2048 bytes.
                ctx.warn_once(u64::from(offset), format_args!("{error}"));
                0
            }
        }
    }

    /// `WriteByte(offset, value)` = `SetByte`.
    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        if let Err(error) = self.set_byte(offset, value as u8) {
            ctx.warn_once(u64::from(offset) | 1 << 32, format_args!("{error}"));
        }
    }

    fn peek(&self, offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        self.get_byte(offset).ok().map(u32::from)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.summary_text()
    }

    impl_peripheral_any!();
}

/// `NGCEepromBank`: the I2C target of one 256-byte bank of the store.
pub struct NgcEepromBank {
    name: String,
    storage: NgcEepromStore,
    bank: usize,
    pointer: u8,
    dirty: bool,
    address_pending: bool,
}

impl NgcEepromBank {
    /// `new NGCEepromBank(storage, bank)`, named `eeprom<bank>` (the `.repl` name). `bank` must be 0..=7.
    pub fn new(storage: &NgcEepromStore, bank: usize) -> Result<Self, EepromError> {
        if bank >= EEPROM_BANKS {
            return Err(EepromError::BankOutOfRange(bank));
        }
        Ok(Self {
            name: format!("eeprom{bank}"),
            storage: storage.clone(),
            bank,
            pointer: 0,
            dirty: false,
            address_pending: true,
        })
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn bank(&self) -> usize {
        self.bank
    }

    /// Current address pointer (low byte of the offset inside the bank).
    pub fn pointer(&self) -> u8 {
        self.pointer
    }

    /// True while the next non-empty write chunk is taken as the pointer (after a STOP or a read).
    pub fn address_pending(&self) -> bool {
        self.address_pending
    }

    /// True while the current transaction wrote cells (`FinishTransmission` then flushes).
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    fn cell(&self, offset_in_bank: usize) -> u32 {
        (self.bank * EEPROM_BANK_SIZE + offset_in_bank) as u32
    }

    fn reset_state(&mut self) {
        self.pointer = 0;
        self.dirty = false;
        self.address_pending = true;
    }
}

impl I2cTarget for NgcEepromBank {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Write(bytes)`: an empty chunk (address-only readiness probe) is ACKed and ignored; otherwise the
    /// first chunk after a STOP/read starts with the pointer byte, the rest (and later chunks) is data
    /// written inside the 16-byte page of the pointer, which wraps.
    fn write(&mut self, bytes: &[u8], _ctx: &mut I2cCtx<'_, '_>) {
        if bytes.is_empty() {
            return;
        }
        let mut first = 0;
        if self.address_pending {
            self.pointer = bytes[0];
            self.address_pending = false;
            first = 1;
        }
        let page = usize::from(self.pointer & 0xF0);
        let mut index = usize::from(self.pointer & 0x0F);
        for &byte in &bytes[first..] {
            // bank <= 7, page <= 0xF0 and index <= 0xF keep the offset below 2048.
            let _ = self.storage.set_byte(self.cell(page + index), byte);
            index = (index + 1) & 0x0F;
            self.dirty = true;
        }
        if bytes.len() > first {
            self.pointer = (page | index) as u8; // 16-byte page write wraps
        }
    }

    /// `Read(count)`: reads from the pointer, advancing it modulo 256 (inside the bank). A read ends the
    /// pointer phase (`addressPending`), so a repeated START moves from the pointer write to the read.
    fn read(&mut self, count: usize, _ctx: &mut I2cCtx<'_, '_>) -> Vec<u8> {
        self.address_pending = true;
        let mut bytes = Vec::with_capacity(count);
        for _ in 0..count {
            bytes.push(self.storage.get_byte(self.cell(usize::from(self.pointer))).unwrap_or(0xFF));
            self.pointer = self.pointer.wrapping_add(1);
        }
        bytes
    }

    /// `FinishTransmission()`: flushes the store when the transaction wrote, and re-arms the pointer phase.
    fn finish_transmission(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
        if self.dirty {
            self.storage.flush();
            self.dirty = false;
        }
        self.address_pending = true;
    }

    /// `Reset()`: pointer 0, not dirty, pointer phase pending. The cells are untouched.
    fn reset(&mut self, _ctx: &mut I2cCtx<'_, '_>) {
        self.reset_state();
    }

    fn summary(&self) -> String {
        format!(
            "EEPROM bank {}: pointer=0x{:02X}; addressPending={}; dirty={}",
            self.bank, self.pointer, self.address_pending, self.dirty
        )
    }

    impl_i2c_target_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::PeriphId;
    use stm32::i2c::testing::{
        master_transmit, mem_read, mem_write, transfer_config, AUTOEND_MODE, GENERATE_START_WRITE, NO_STARTSTOP,
        RELOAD_MODE,
    };
    use stm32::i2c::{regs, I2C_SIZE};

    const I2C_BASE: u32 = 0x4000_5400;

    struct Rig {
        h: Harness,
        i2c: PeriphId,
        store_id: PeriphId,
        store: NgcEepromStore,
    }

    /// Main-board wiring of `main.repl`: `i2c1` with eight banks at 0x50..0x57 and the diagnostic view.
    fn rig() -> Rig {
        let mut h = Harness::new();
        let mut controller = Stm32F7I2c::new("i2c1");
        let store = NgcEepromStore::new();
        store.attach_banks(&mut controller).unwrap();
        let i2c = h.add_mapped(I2C_BASE, I2C_SIZE, controller);
        let store_id = h.add_mapped(EEPROM_DIAGNOSTIC_BASE, EEPROM_SIZE as u32, store.clone());
        Rig { h, i2c, store_id, store }
    }

    /// HAL device address of a bank: `((offset >> 8) + 0x50) * 2`.
    fn dev(bank: usize) -> u32 {
        (EEPROM_FIRST_BANK_ADDRESS + bank as u32) << 1
    }

    fn bank(rig: &Rig, bank: usize) -> &NgcEepromBank {
        rig.h
            .get::<Stm32F7I2c>(rig.i2c)
            .target::<NgcEepromBank>(EEPROM_FIRST_BANK_ADDRESS + bank as u32)
            .expect("bank target")
    }

    #[test]
    fn store_starts_erased_and_counts_every_byte_write() {
        let store = NgcEepromStore::new();
        assert!(store.image().iter().all(|&byte| byte == 0xFF));
        assert_eq!(store.summary_text(), "EEPROM fixture: bytes=2048; writes=0; backing=volatile process storage");
        assert!(store.auto_persist());
        assert_eq!(store.backing_path(), None);

        store.set_byte(5, 0x12).unwrap();
        store.set_word(6, 0x3456).unwrap();
        store.set_double_word(8, 0x789A_BCDE).unwrap();
        assert_eq!(store.image()[5..12], [0x12, 0x56, 0x34, 0xDE, 0xBC, 0x9A, 0x78], "little-endian");
        assert_eq!(store.write_count(), 1 + 2 + 4);
        assert_eq!(store.get_double_word(8), Ok(0x789A_BCDE));
        assert_eq!(store.summary_text(), "EEPROM fixture: bytes=2048; writes=7; backing=volatile process storage");

        // Offsets outside the 2048 cells are refused (C# ArgumentOutOfRangeException).
        assert_eq!(store.get_byte(2048), Err(EepromError::OffsetOutOfRange(2048)));
        assert_eq!(store.set_byte(2048, 0), Err(EepromError::OffsetOutOfRange(2048)));
        // SetDoubleWord is four SetByte calls: the first two land, then the exception.
        assert_eq!(store.set_double_word(2046, 0x1122_3344), Err(EepromError::OffsetOutOfRange(2048)));
        assert_eq!(store.get_byte(2046), Ok(0x44));
        assert_eq!(store.get_byte(2047), Ok(0x33));
        assert_eq!(store.write_count(), 7 + 2);

        // Erase restores 0xFF and does not count as a write.
        store.erase();
        assert!(store.image().iter().all(|&byte| byte == 0xFF));
        assert_eq!(store.write_count(), 9);
        // Clones share the cells (the C# object reference).
        let alias = store.clone();
        alias.set_byte(0, 1).unwrap();
        assert_eq!(store.get_byte(0), Ok(1));
        assert_eq!(store.write_count(), 10);
    }

    #[test]
    fn backing_image_load_save_round_trip() {
        let store = NgcEepromStore::new();
        // A missing backing file: the label is set and the (erased) store is to be written out.
        store.load_backing("profile/eeprom.bin", None).unwrap();
        assert_eq!(store.backing_path().as_deref(), Some("profile/eeprom.bin"));
        assert!(store.take_persist_request());
        assert!(!store.take_persist_request(), "the request is consumed");
        assert_eq!(
            store.summary_text(),
            "EEPROM fixture: bytes=2048; writes=0; backing=profile/eeprom.bin"
        );

        // An existing 2048-byte image is copied in; WriteCount is not touched.
        let mut image = [0xFFu8; EEPROM_SIZE];
        image[254] = 0xA3;
        image[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        image[2047] = 0x5A;
        store.load_backing("profile/eeprom.bin", Some(&image)).unwrap();
        assert_eq!(store.image(), image);
        assert_eq!(store.write_count(), 0);
        assert!(!store.take_persist_request(), "loading an existing file does not rewrite it");

        // A wrong-sized image is refused after the label was set (C# order); the cells are unchanged.
        let result = store.load_backing("other.bin", Some(&image[..2047]));
        assert_eq!(result, Err(EepromError::BadImageSize(2047)));
        assert_eq!(store.backing_path().as_deref(), Some("other.bin"));
        assert_eq!(store.image(), image);
        assert_eq!(store.load_image(&[0u8; 2049]), Err(EepromError::BadImageSize(2049)));
        assert!(EepromError::BadImageSize(2047).to_string().contains("exactly 2048 bytes"));

        // load_image copies without touching the label.
        let mut other = [0x11u8; EEPROM_SIZE];
        other[7] = 0x77;
        store.load_image(&other).unwrap();
        assert_eq!(store.image(), other);
        assert_eq!(store.backing_path().as_deref(), Some("other.bin"));
    }

    #[test]
    fn flush_requests_a_save_only_with_autopersist_and_a_backing_label() {
        let store = NgcEepromStore::new();
        store.flush();
        assert!(!store.take_persist_request(), "volatile process storage");
        store.load_backing("eeprom.bin", Some(&[0xFFu8; EEPROM_SIZE])).unwrap();
        store.flush();
        assert!(store.take_persist_request());
        store.set_auto_persist(false);
        store.flush();
        assert!(!store.take_persist_request(), "AutoPersist off");
        store.set_auto_persist(true);
        store.flush();
        assert!(store.take_persist_request());
    }

    #[test]
    fn hal_memory_write_and_read_through_bank_zero() {
        let mut rig = rig();
        // The firmware's validity marker: logical ID 0x63 at offset 0xFE, value 0xA3.
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0xFE, &[0xA3]);
        assert_eq!(rig.store.get_byte(254), Ok(0xA3));
        assert_eq!(rig.store.write_count(), 1);
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(0), 0xFE, 1), [0xA3]);
        assert_eq!(rig.store.write_count(), 1, "reads never write");
        // Little-endian multi-byte values (serial number style) within a page.
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x00, &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(rig.store.get_double_word(0), Ok(0x1234_5678));
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(0), 0x00, 4), [0x78, 0x56, 0x34, 0x12]);
        assert!(rig.h.warnings().is_empty(), "{:?}", rig.h.warnings());
    }

    #[test]
    fn address_and_data_chunks_share_one_pointer_phase() {
        let mut rig = rig();
        let h = &mut rig.h;
        // First chunk of HAL_I2C_Mem_Write: RELOAD, NBYTES = 1, the pointer byte.
        transfer_config(h, I2C_BASE, dev(2), 1, RELOAD_MODE, GENERATE_START_WRITE);
        h.write32(I2C_BASE + regs::TXDR, 0x40);
        {
            let target = rig.h.get::<Stm32F7I2c>(rig.i2c).target::<NgcEepromBank>(0x52).unwrap();
            assert_eq!(target.pointer(), 0x40);
            assert!(!target.address_pending(), "the next chunk is data, not another pointer");
            assert!(!target.dirty());
        }
        // Second chunk: data. A model that took every chunk as a new pointer would store nothing.
        let h = &mut rig.h;
        transfer_config(h, I2C_BASE, dev(2), 3, AUTOEND_MODE, NO_STARTSTOP);
        for byte in [0xA1, 0xA2, 0xA3] {
            h.write32(I2C_BASE + regs::TXDR, byte);
        }
        let image = rig.store.image();
        assert_eq!(image[2 * 256 + 0x40..2 * 256 + 0x43], [0xA1, 0xA2, 0xA3]);
        assert_eq!(image.iter().filter(|&&byte| byte != 0xFF).count(), 3);
        // AUTOEND finished the transaction: pointer phase pending again, dirty flushed.
        let target = bank(&rig, 2);
        assert!(target.address_pending());
        assert!(!target.dirty());
        assert_eq!(target.pointer(), 0x43);
    }

    #[test]
    fn page_write_wraps_inside_the_16_byte_page() {
        let mut rig = rig();
        let data: Vec<u8> = (0..20).map(|i| 0x10 + i as u8).collect();
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0xF8, &data);
        let image = rig.store.image();
        // Bytes 0..7 fill 0xF8..0xFF, the rest wraps to 0xF0 and overwrites 0xF8..0xFB.
        assert_eq!(image[0xF0..0xF8], data[8..16]);
        assert_eq!(image[0xF8..0xFC], data[16..20]);
        assert_eq!(image[0xFC..0x100], data[4..8]);
        assert_eq!(image[..0xF0].iter().filter(|&&byte| byte != 0xFF).count(), 0);
        assert_eq!(rig.store.write_count(), 20);
        assert_eq!(bank(&rig, 0).pointer(), 0xFC, "page | index after the last byte");
        // Reads are not page limited: they continue across the 16-byte boundary.
        rig.store.erase();
        for i in 0..32u32 {
            rig.store.set_byte(i, i as u8).unwrap();
        }
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(0), 0x0E, 4), [0x0E, 0x0F, 0x10, 0x11]);
    }

    #[test]
    fn banks_address_separate_256_byte_regions_and_reads_wrap_inside_the_bank() {
        let mut rig = rig();
        for b in 0..EEPROM_BANKS {
            mem_write(&mut rig.h, I2C_BASE, dev(b), 0x10, &[0x80 + b as u8]);
        }
        let image = rig.store.image();
        for b in 0..EEPROM_BANKS {
            assert_eq!(image[b * 256 + 0x10], 0x80 + b as u8, "bank {b}");
        }
        assert_eq!(image.iter().filter(|&&byte| byte != 0xFF).count(), EEPROM_BANKS);
        // Pointer 0xFF of bank 7: the next byte is bank 7's offset 0, not bank 0's.
        rig.store.set_byte(7 * 256 + 0xFF, 0xEE).unwrap();
        rig.store.set_byte(7 * 256, 0xDD).unwrap();
        rig.store.set_byte(0, 0xCC).unwrap();
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(7), 0xFF, 2), [0xEE, 0xDD]);
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(0), 0x00, 1), [0xCC]);
        // A bank answers only at its own address: 0x58 is nobody (Renode: warning, no ACK/NACK flags).
        assert_eq!(EEPROM_FIRST_BANK_ADDRESS + EEPROM_BANKS as u32, 0x58);
        assert!(rig.h.get::<Stm32F7I2c>(rig.i2c).target_at(0x58).is_none());
    }

    #[test]
    fn address_only_probe_is_acknowledged_without_side_effects() {
        let mut rig = rig();
        // 0x0800fea0: HAL master transmit to 0xA0 with a null buffer and zero bytes.
        master_transmit(&mut rig.h, I2C_BASE, 0xA0, &[]);
        master_transmit(&mut rig.h, I2C_BASE, dev(7), &[]);
        assert_eq!(rig.store.write_count(), 0);
        assert!(rig.store.image().iter().all(|&byte| byte == 0xFF));
        let target = bank(&rig, 0);
        assert_eq!(target.pointer(), 0);
        assert!(target.address_pending());
        assert!(!target.dirty());
        assert!(rig.h.warnings().is_empty());
        // A probe in the middle of a pointer phase does not disturb it either.
        transfer_config(&mut rig.h, I2C_BASE, dev(0), 1, RELOAD_MODE, GENERATE_START_WRITE);
        rig.h.write32(I2C_BASE + regs::TXDR, 0x30);
        let target = bank(&rig, 0);
        assert!(!target.address_pending());
        assert_eq!(target.pointer(), 0x30);
    }

    #[test]
    fn size_mismatch_is_rejected_by_the_firmware_not_the_model() {
        let mut rig = rig();
        // Setting 0x14 sits at physical offset 0x1D with size 1 (map entry `1d 00 01 00`). The case-0 setter
        // (0x08009744) writes it with size 1 and falls through to a second request with size 2, which the
        // firmware wrapper (0x08010430) rejects before any I2C traffic: only the valid request reaches here.
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x1D, &[0x01]);
        assert_eq!(rig.store.get_byte(0x1D), Ok(0x01));
        assert_eq!(rig.store.get_byte(0x1E), Ok(0xFF), "the neighbouring setting is untouched");
        assert_eq!(rig.store.write_count(), 1);
        // The bank itself has no size validation: a two-byte write that did reach it is stored in full.
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x1D, &[0x02, 0x03]);
        assert_eq!(rig.store.get_byte(0x1D), Ok(0x02));
        assert_eq!(rig.store.get_byte(0x1E), Ok(0x03));
    }

    #[test]
    fn transactions_flush_the_store_only_when_they_wrote() {
        let mut rig = rig();
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x00, &[1]);
        assert!(!rig.store.take_persist_request(), "volatile storage has nothing to save");
        rig.store.load_backing("eeprom.bin", Some(&[0xFFu8; EEPROM_SIZE])).unwrap();
        assert!(!rig.store.take_persist_request());
        mem_read(&mut rig.h, I2C_BASE, dev(0), 0x00, 1);
        assert!(!rig.store.take_persist_request(), "reads do not flush");
        master_transmit(&mut rig.h, I2C_BASE, dev(0), &[]);
        assert!(!rig.store.take_persist_request(), "probes do not flush");
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x00, &[2]);
        assert!(rig.store.take_persist_request(), "FinishTransmission after a write flushes");
        // The flush happens once per transaction, at its end: a RELOAD chain of chunks flushes once.
        let data: Vec<u8> = (0..16).collect();
        mem_write(&mut rig.h, I2C_BASE, dev(1), 0x10, &data);
        assert!(rig.store.take_persist_request());
        assert!(!rig.store.take_persist_request());
        rig.store.set_auto_persist(false);
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0x00, &[3]);
        assert!(!rig.store.take_persist_request(), "AutoPersist off");
    }

    #[test]
    fn serial_fixture_sequence_of_the_runner() {
        let mut rig = rig();
        let id = rig.store_id;
        // `run_emulator.py` "serial": refuse until the first boot stored the marker (byte 254 == 0xA3).
        let marker = rig.h.with::<NgcEepromStore, _>(id, |store, _ctx| store.get_byte(254));
        assert_eq!(marker, Ok(0xFF), "erased profile: the fixture must wait for the first boot");
        // The firmware's own initialization stores the marker through I2C.
        mem_write(&mut rig.h, I2C_BASE, dev(0), 0xFE, &[0xA3]);
        rig.store.load_backing("profile/eeprom.bin", Some(&rig.store.image())).unwrap();
        assert!(!rig.store.take_persist_request());
        let marker = rig.h.with::<NgcEepromStore, _>(id, |store, _ctx| store.get_byte(254));
        assert_eq!(marker, Ok(0xA3));
        // `eepromStore SetDoubleWord 0 <serial>` and `eepromStore Flush`.
        let serial = 0xDEAD_BEEF_u32;
        rig.h.with::<NgcEepromStore, _>(id, |store, _ctx| {
            store.set_double_word(0, serial).unwrap();
            store.flush();
        });
        assert!(rig.store.take_persist_request());
        assert_eq!(rig.store.image()[0..4], [0xEF, 0xBE, 0xAD, 0xDE]);
        // The snapshot's `serialNumber`: four GetByte calls combined little-endian.
        let snapshot: u32 =
            (0..4u32).map(|i| u32::from(rig.store.get_byte(i).unwrap()) << (8 * i)).fold(0, |acc, byte| acc | byte);
        assert_eq!(snapshot, serial);
        assert_eq!(rig.store.get_double_word(0), Ok(serial));
        // The firmware (loader 0x0800a2c8) reads it back through bank 0, pointer 0.
        assert_eq!(mem_read(&mut rig.h, I2C_BASE, dev(0), 0x00, 4), serial.to_le_bytes());
        assert!(rig.store.summary_text().ends_with("backing=profile/eeprom.bin"));
    }

    #[test]
    fn reset_preserves_cells_and_rearms_the_banks() {
        let mut rig = rig();
        mem_write(&mut rig.h, I2C_BASE, dev(3), 0x20, &[0x42]);
        // Leave a transaction half done: pointer phase consumed.
        transfer_config(&mut rig.h, I2C_BASE, dev(3), 1, RELOAD_MODE, GENERATE_START_WRITE);
        rig.h.write32(I2C_BASE + regs::TXDR, 0x77);
        assert!(!bank(&rig, 3).address_pending());
        assert_eq!(bank(&rig, 3).pointer(), 0x77);
        rig.h.core_mut().reset_all();
        assert_eq!(rig.store.get_byte(3 * 256 + 0x20), Ok(0x42), "power loss preserves nonvolatile cells");
        assert_eq!(rig.store.write_count(), 1);
        let target = bank(&rig, 3);
        assert_eq!(target.pointer(), 0);
        assert!(target.address_pending());
        assert!(!target.dirty());
        // The reset controller works again from a clean state.
        mem_write(&mut rig.h, I2C_BASE, dev(3), 0x21, &[0x43]);
        assert_eq!(rig.store.get_byte(3 * 256 + 0x21), Ok(0x43));
    }

    #[test]
    fn diagnostic_bus_view_is_byte_wide() {
        let mut rig = rig();
        let base = EEPROM_DIAGNOSTIC_BASE;
        assert_eq!(rig.h.read8(base + 254), 0xFF);
        rig.h.write8(base + 254, 0xA3);
        assert_eq!(rig.store.get_byte(254), Ok(0xA3));
        assert_eq!(rig.store.write_count(), 1, "the diagnostic write counts like SetByte");
        assert_eq!(rig.h.read8(base + 254), 0xA3);
        assert_eq!(rig.h.read8(base + 2047), 0xFF);
        assert_eq!(rig.h.peek(base + 254, Width::Byte), Some(0xA3));
        // IBytePeripheral only, no allowed translations: halfword/word accesses are not supported.
        assert_eq!(rig.h.read16(base + 254), 0);
        assert_eq!(rig.h.read32(base + 254), 0);
        rig.h.write16(base + 10, 0x1234);
        rig.h.write32(base + 12, 0x1234_5678);
        assert_eq!(rig.store.get_byte(10), Ok(0xFF));
        assert_eq!(rig.store.get_byte(12), Ok(0xFF));
        assert_eq!(rig.store.write_count(), 1);
        assert_eq!(rig.h.warnings().len(), 4, "{:?}", rig.h.warnings());
        assert!(rig.h.warnings()[0].contains("Attempted Word read isn't supported by the peripheral"));
        let summaries = rig.h.core().summaries();
        let (_, text) = summaries.iter().find(|(name, _)| name == "eepromStore").expect("store summary");
        assert_eq!(*text, rig.store.summary_text());
        assert!(text.contains("writes=1"));
    }

    #[test]
    fn bank_index_is_validated() {
        let store = NgcEepromStore::new();
        assert!(store.new_bank(7).is_ok());
        assert_eq!(store.new_bank(8).err(), Some(EepromError::BankOutOfRange(8)));
        assert_eq!(NgcEepromBank::new(&store, usize::MAX).err(), Some(EepromError::BankOutOfRange(usize::MAX)));
        let bank = store.new_bank(5).unwrap();
        assert_eq!(bank.bank(), 5);
        assert_eq!(bank.summary(), "EEPROM bank 5: pointer=0x00; addressPending=true; dirty=false");
        assert_eq!(bank.name(), "eeprom5");
        assert_eq!(bank.with_name("custom").name(), "custom");
    }
}
