// Ported from emulation/models/NGCQuadSPI.cs.

//! `NGCQuadSPI`: the STM32L4 QUADSPI indirect-mode register file combined with a generic NOR flash, as used
//! by the main 5.8 firmware for its littlefs log (`qspi @ 0xA0001000`, size `0x400`).
//!
//! The exact flash part is unknown; the 128 MiB default capacity follows the firmware's `FSIZE = 26`
//! address window and the JEDEC answer (`ef 40 21`) is an explicit synthetic fixture. Only **indirect**
//! transfers are modelled (functional-mode field `FMODE` 0 = write, 1 = read); automatic polling and
//! memory-mapped mode (`FMODE` 2/3) raise the transfer-error flag, as in the C# model. Serial-clock and
//! FIFO timing are not modelled: a transfer completes in the register access that supplies its last byte.
//!
//! # Command model
//!
//! A command starts when `CCR` is written with no address phase (`CCR.ADMODE == 0`) or when `AR` is written
//! (`AR` is the trigger for commands with an address phase). `DLR + 1` data bytes follow through `DR` when
//! `CCR.DMODE != 0`. Commands without data take effect immediately and raise `SR.TCF`:
//! `0x06` write enable (ignored while busy), `0x04` write disable, `0xB7`/`0xE9` enter/exit 4-byte address mode,
//! `0xB9`/`0xAB` power down/up, `0x20`/`0x21` 4 KiB erase, `0x52`/`0x5C` 32 KiB erase, `0xD8`/`0xDC` 64 KiB erase,
//! `0x60`/`0xC7` chip erase. Reads (`0x03 0x13 0x0B 0x0C 0x6B 0x6C 0xEB 0xEC`) return storage bytes from
//! `AR`; `0x05` status (`bit0` busy, `bit1` WEL), `0x35` (always 2), `0x15` (4-byte mode flag) and `0x9F` (JEDEC)
//! are served; any other command reads `0xFF`. Programs (`0x02 0x12 0x32 0x34`) AND the data into the cell,
//! wrapping inside the 256-byte page of `AR`, only if write-enable was set and the device is not busy.
//! Programming, erasing and write-enable use the explicit busy fixtures of 2 ms / 10 ms (50 ms for a chip
//! erase), counted down by a 1 kHz managed thread (`busyMs`): they are approximate periods, not part timing.
//! Data changes at completion of the transfer, so torn writes and wear are not simulated.
//!
//! Registers are plain storage keyed by offset except: `SR` (computed: the stored flags, `BUSY` while bytes
//! remain, the FIFO level), `FCR` (write-1-to-clear of the flags), `DR` (data port: a 32-bit access moves up to
//! four bytes, a byte access one) and `CR.ABORT`. Accesses: byte and 32-bit only (`IBytePeripheral` +
//! `IDoubleWordPeripheral`, no translations): a halfword access is logged and ignored by the bus. A byte
//! read of `DR + 1..3` is a 32-bit `DR` read in the C# class and moves four bytes (kept).
//!
//! # Wiring (`main.repl`)
//!
//! ```text
//! qspi: SPI.NGCQuadSPI @ sysbus 0xa0001000      add_mapped(0xA000_1000, qspi::SIZE, NgcQuadSpi::new("qspi"))
//! ```
//!
//! The `IRQ` output (line [`IRQ`]) is not connected on the main board. The runner's `LoadBackingFile` / `Flush` /
//! `SaveBackingFile` map to [`NgcQuadSpi::load_backing`], [`NgcQuadSpi::take_persist_request`] and
//! [`NgcQuadSpi::serialize_backing`].
//!
//! # Backing store
//!
//! Storage is sparse: 4096-byte pages that were ever programmed are kept (missing pages read `0xFF`, a page
//! that is programmed with `0xFF` stays present). [`NgcQuadSpi::serialize_backing`] and
//! [`NgcQuadSpi::load_backing`] reproduce the C# `SaveBackingFile`/`LoadBackingFile` byte format
//! (`NGCNOR01`, LE `u32` capacity, LE `u32` page count, then ascending `{u32 page, 4096 bytes}` records), so a
//! `nor.ngc` from the Renode runner loads here and vice versa. File I/O is the persistence layer's job: it
//! polls [`NgcQuadSpi::take_persist_request`], which stands for the C# `Flush()` that rewrote the backing file
//! after every completed program/erase/transfer.

use super::clock_control::WordStore;
use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, ManagedThread, Peripheral, Time, Translations, View, Width, Widths};
use std::fmt;

/// `Size`: the register window.
pub const SIZE: u32 = 0x400;
/// Default capacity (`0x08000000`, 128 MiB).
pub const DEFAULT_CAPACITY: u32 = 0x0800_0000;
/// Bytes per storage page of the sparse store and of the backing format.
pub const PAGE_SIZE: usize = 4096;
/// Output line 0: `IRQ` (not connected on the main board).
pub const IRQ: u32 = 0;
/// Magic of the backing format.
pub const BACKING_MAGIC: &[u8; 8] = b"NGCNOR01";

/// Register offsets.
pub mod reg {
    pub const CR: u32 = 0x00;
    pub const DCR: u32 = 0x04;
    pub const SR: u32 = 0x08;
    pub const FCR: u32 = 0x0C;
    pub const DLR: u32 = 0x10;
    pub const CCR: u32 = 0x14;
    pub const AR: u32 = 0x18;
    pub const ABR: u32 = 0x1C;
    pub const DR: u32 = 0x20;
}

/// Token of the 1 kHz busy-countdown thread.
const TICK: u64 = 1;

/// Why a constructor or a backing-image load was refused (the C# exceptions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QspiError {
    /// `ArgumentOutOfRangeException("capacity")`.
    BadCapacity(u32),
    /// `ArgumentException("Incompatible NGC sparse NOR fixture image")`: wrong magic or capacity.
    Incompatible,
    /// `EndOfStreamException`: the image ends inside the header.
    Truncated,
    /// `ArgumentException("Invalid sparse NOR page count")`.
    BadPageCount(u32),
    /// `ArgumentException("Invalid sparse NOR page record")`: page out of range, duplicate or short.
    BadPageRecord,
    /// `ArgumentException("Trailing data in sparse NOR image")`.
    TrailingData,
}

impl fmt::Display for QspiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QspiError::BadCapacity(capacity) => write!(f, "capacity {capacity} must be a non-zero multiple of 4096"),
            QspiError::Incompatible => write!(f, "Incompatible NGC sparse NOR fixture image"),
            QspiError::Truncated => write!(f, "Unable to read beyond the end of the sparse NOR image"),
            QspiError::BadPageCount(count) => write!(f, "Invalid sparse NOR page count {count}"),
            QspiError::BadPageRecord => write!(f, "Invalid sparse NOR page record"),
            QspiError::TrailingData => write!(f, "Trailing data in sparse NOR image"),
        }
    }
}

impl std::error::Error for QspiError {}

type Page = Box<[u8; PAGE_SIZE]>;

/// `SPI.NGCQuadSPI`.
pub struct NgcQuadSpi {
    name: String,
    capacity: u32,
    registers: WordStore,
    /// `pages`: `capacity / 4096` slots, present when the C# dictionary has the key.
    pages: Vec<Option<Page>>,
    page_count: u32,
    sr: u32,
    mode: u32,
    address: u32,
    remaining: u32,
    transferred: u32,
    busy_ms: i32,
    write_enabled: bool,
    four_byte_address: bool,
    powered_down: bool,
    program_allowed: bool,
    dirty_transfer: bool,
    command_count: u64,
    bytes_read: u64,
    bytes_programmed: u64,
    erase_count: u64,
    last_command: u8,
    auto_persist: bool,
    backing: Option<String>,
    persist_pending: bool,
    /// Bumped whenever the C# class would flush (a completed program transfer, an erase, `EraseStorage`) and
    /// whenever an image is loaded, independent of `AutoPersist` and of the backing label.
    revision: u64,
    timer: ManagedThread,
}

impl NgcQuadSpi {
    /// `new NGCQuadSPI(machine)` named `name` (the `.repl` name is `qspi`): 128 MiB, `AutoPersist` on.
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_capacity(name, DEFAULT_CAPACITY).expect("the default capacity is valid")
    }

    /// `new NGCQuadSPI(machine, capacity)`; the capacity must be a non-zero multiple of 4096.
    pub fn with_capacity(name: impl Into<String>, capacity: u32) -> Result<Self, QspiError> {
        if capacity == 0 || capacity % PAGE_SIZE as u32 != 0 {
            return Err(QspiError::BadCapacity(capacity));
        }
        let slots = (capacity / PAGE_SIZE as u32) as usize;
        let mut pages = Vec::new();
        pages.resize_with(slots, || None);
        let mut qspi = Self {
            name: name.into(),
            capacity,
            registers: WordStore::new(),
            pages,
            page_count: 0,
            sr: 0,
            mode: 0,
            address: 0,
            remaining: 0,
            transferred: 0,
            busy_ms: 0,
            write_enabled: false,
            four_byte_address: false,
            powered_down: false,
            program_allowed: false,
            dirty_transfer: false,
            command_count: 0,
            bytes_read: 0,
            bytes_programmed: 0,
            erase_count: 0,
            last_command: 0,
            auto_persist: true,
            backing: None,
            persist_pending: false,
            revision: 0,
            timer: ManagedThread::new(1000, TICK),
        };
        qspi.reset_state();
        Ok(qspi)
    }

    /// The part of `Reset()` that does not touch the output line.
    fn reset_state(&mut self) {
        self.registers.clear();
        self.sr = 0;
        self.remaining = 0;
        self.transferred = 0;
        self.write_enabled = false;
        self.four_byte_address = false;
        self.powered_down = false;
        self.busy_ms = 0;
        self.program_allowed = false;
        self.dirty_transfer = false;
    }

    // ---- C# properties and runner methods ----

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// `AutoPersist` (default true): whether completed transfers and erases request a save.
    pub fn auto_persist(&self) -> bool {
        self.auto_persist
    }

    pub fn set_auto_persist(&mut self, enabled: bool) {
        self.auto_persist = enabled;
    }

    /// `BackingPath`: the label given to [`load_backing`](NgcQuadSpi::load_backing), if any.
    pub fn backing_path(&self) -> Option<&str> {
        self.backing.as_deref()
    }

    /// `CommandCount`: commands started since the model was created.
    pub fn command_count(&self) -> u64 {
        self.command_count
    }

    /// `BytesRead`: storage bytes returned to read commands.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// `BytesProgrammed`: cells programmed by program commands.
    pub fn bytes_programmed(&self) -> u64 {
        self.bytes_programmed
    }

    /// `EraseCount`: completed sector/block/chip erases.
    pub fn erase_count(&self) -> u64 {
        self.erase_count
    }

    /// `LastCommand`: the instruction byte of the last started command.
    pub fn last_command(&self) -> u8 {
        self.last_command
    }

    /// Pages present in the sparse store (`pages.Count`).
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// `busyMs`: milliseconds of busy time left.
    pub fn busy_ms(&self) -> i32 {
        self.busy_ms
    }

    /// `GetStorageByte(address)`: `0xFF` beyond the capacity or in a missing page.
    pub fn get_storage_byte(&self, address: u32) -> u8 {
        if address >= self.capacity {
            return 0xFF;
        }
        match &self.pages[(address / PAGE_SIZE as u32) as usize] {
            Some(page) => page[(address % PAGE_SIZE as u32) as usize],
            None => 0xFF,
        }
    }

    /// `LoadBackingFile(path)`: remembers `backing` as `BackingPath`, then replaces the storage with `image`
    /// if the persistence layer found one (a malformed image leaves the storage untouched and returns the
    /// error, with the label already set, as in C#) or, when the backing file does not exist yet (`None`),
    /// requests that the current storage be written to it.
    pub fn load_backing(&mut self, backing: &str, image: Option<&[u8]>) -> Result<(), QspiError> {
        self.backing = Some(backing.to_string());
        match image {
            Some(image) => self.load_image(image),
            None => {
                self.persist_pending = true;
                Ok(())
            }
        }
    }

    /// Parses `image` (the `nor.ngc` format) and, only if it is valid and matches the capacity, replaces
    /// the sparse store with it.
    pub fn load_image(&mut self, image: &[u8]) -> Result<(), QspiError> {
        if image.len() < 8 || &image[..8] != BACKING_MAGIC {
            return Err(QspiError::Incompatible);
        }
        let word = |at: usize| -> Option<u32> { image.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
        if word(8).ok_or(QspiError::Truncated)? != self.capacity {
            return Err(QspiError::Incompatible);
        }
        let count = word(12).ok_or(QspiError::Truncated)?;
        let slots = self.capacity / PAGE_SIZE as u32;
        if count > slots {
            return Err(QspiError::BadPageCount(count));
        }
        let mut restored: Vec<(u32, &[u8])> = Vec::with_capacity(count as usize);
        let mut seen = vec![false; slots as usize];
        let mut at = 16usize;
        for _ in 0..count {
            let page = word(at).ok_or(QspiError::Truncated)?;
            at += 4;
            let bytes = image.get(at..at + PAGE_SIZE);
            at = (at + PAGE_SIZE).min(image.len());
            let bytes = match bytes {
                Some(bytes) if page < slots && !seen[page as usize] => bytes,
                _ => return Err(QspiError::BadPageRecord),
            };
            seen[page as usize] = true;
            restored.push((page, bytes));
        }
        if at != image.len() {
            return Err(QspiError::TrailingData);
        }
        self.clear_pages();
        for (page, bytes) in restored {
            let mut cells: Page = Box::new([0xFF; PAGE_SIZE]);
            cells.copy_from_slice(bytes);
            self.pages[page as usize] = Some(cells);
            self.page_count += 1;
        }
        self.revision += 1;
        Ok(())
    }

    /// Counts the changes of the stored pages: it is bumped wherever the C# class flushes (a completed program
    /// transfer, a sector/block/chip erase, `EraseStorage`) and by every successful image load, whatever
    /// `AutoPersist` and the backing label say. A persistence layer can compare it with the revision it last saved
    /// instead of polling [`take_persist_request`](NgcQuadSpi::take_persist_request).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The C# `if(AutoPersist) Flush();` after a change of the stored pages.
    fn storage_modified(&mut self) {
        self.revision += 1;
        if self.auto_persist {
            self.flush();
        }
    }

    /// What `SaveBackingFile` writes: header and the present pages in ascending order.
    pub fn serialize_backing(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.page_count as usize * (4 + PAGE_SIZE));
        out.extend_from_slice(BACKING_MAGIC);
        out.extend_from_slice(&self.capacity.to_le_bytes());
        out.extend_from_slice(&self.page_count.to_le_bytes());
        for (index, page) in self.pages.iter().enumerate() {
            if let Some(page) = page {
                out.extend_from_slice(&(index as u32).to_le_bytes());
                out.extend_from_slice(&page[..]);
            }
        }
        out
    }

    /// `Flush()`: requests a save of the backing store if a backing label is set (regardless of `AutoPersist`).
    pub fn flush(&mut self) {
        if self.backing.is_some() {
            self.persist_pending = true;
        }
    }

    /// `EraseStorage()`: drops every page, then flushes if `AutoPersist` is on.
    pub fn erase_storage(&mut self) {
        self.clear_pages();
        self.storage_modified();
    }

    /// True once after a `Flush` (or a `load_backing` without an image) asked for the backing store to be
    /// saved; the persistence layer then writes [`serialize_backing`](NgcQuadSpi::serialize_backing).
    pub fn take_persist_request(&mut self) -> bool {
        std::mem::take(&mut self.persist_pending)
    }

    /// C# `Summary`.
    pub fn describe(&self) -> String {
        format!(
            "Generic QSPI NOR fixture: capacity={}; pages={}; commands={}; read={}; programmed={}; erases={}; last=0x{:02X}; busyMs={}; backing={}",
            self.capacity,
            self.page_count,
            self.command_count,
            self.bytes_read,
            self.bytes_programmed,
            self.erase_count,
            self.last_command,
            self.busy_ms,
            self.backing.as_deref().unwrap_or("volatile process storage")
        )
    }

    // ---- storage ----

    fn clear_pages(&mut self) {
        for page in &mut self.pages {
            *page = None;
        }
        self.page_count = 0;
    }

    fn get(&self, offset: u32) -> u32 {
        self.registers.get(offset)
    }

    fn update_irq(&self, ctx: &mut Ctx<'_>) {
        ctx.set_output(IRQ, self.get(reg::CR) & (self.sr << 16) & 0x001B_0000 != 0);
    }

    fn begin_command(&mut self, ctx: &mut Ctx<'_>) {
        let ccr = self.get(reg::CCR);
        self.last_command = ccr as u8;
        self.command_count += 1;
        self.mode = (ccr >> 26) & 3;
        self.transferred = 0;
        self.sr &= !2;
        self.address = self.get(reg::AR);
        // `Get(0x10) + 1` is unchecked uint arithmetic in C#.
        self.remaining = if ccr & 0x0300_0000 != 0 { self.get(reg::DLR).wrapping_add(1) } else { 0 };
        self.program_allowed = false;
        self.dirty_transfer = false;
        if self.mode > 1 {
            // Unsupported automatic polling / memory-mapped mode: transfer error.
            self.remaining = 0;
            self.sr |= 1;
            self.update_irq(ctx);
            return;
        }
        if self.powered_down && self.last_command != 0xAB {
            self.remaining = 0;
            self.sr |= 2;
            self.update_irq(ctx);
            return;
        }
        if self.remaining == 0 {
            match self.last_command {
                0x06 => {
                    if self.busy_ms == 0 {
                        self.write_enabled = true;
                    }
                }
                0x04 => self.write_enabled = false,
                0xB7 => self.four_byte_address = true,
                0xE9 => self.four_byte_address = false,
                0xB9 => self.powered_down = true,
                0xAB => self.powered_down = false,
                0x20 | 0x21 => self.erase_sector(4096),
                0x52 | 0x5C => self.erase_sector(32768),
                0xD8 | 0xDC => self.erase_sector(65536),
                0x60 | 0xC7 => {
                    if self.write_enabled && self.busy_ms == 0 {
                        self.clear_pages();
                        self.write_enabled = false;
                        self.busy_ms = 50;
                        self.erase_count += 1;
                        self.storage_modified();
                    }
                }
                _ => {}
            }
            self.sr |= 2;
        } else if self.mode == 0 && matches!(self.last_command, 0x02 | 0x12 | 0x32 | 0x34) {
            self.program_allowed = self.write_enabled && self.busy_ms == 0;
        }
        self.update_irq(ctx);
    }

    fn read_data(&mut self, ctx: &mut Ctx<'_>) -> u8 {
        if self.remaining == 0 || self.mode != 1 {
            return 0;
        }
        let mut result = 0xFFu8;
        match self.last_command {
            0x05 => result = u8::from(self.busy_ms != 0) | if self.write_enabled { 2 } else { 0 },
            // Quad-enable fixture.
            0x35 => result = 2,
            0x15 => result = u8::from(self.four_byte_address),
            // Explicit synthetic JEDEC fixture.
            0x9F => result = [0xEF, 0x40, 0x21][(self.transferred % 3) as usize],
            0x03 | 0x13 | 0x0B | 0x0C | 0x6B | 0x6C | 0xEB | 0xEC => {
                result = self.get_storage_byte(self.address.wrapping_add(self.transferred));
                self.bytes_read += 1;
            }
            _ => {}
        }
        self.transferred = self.transferred.wrapping_add(1);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.sr |= 2;
            self.update_irq(ctx);
        }
        result
    }

    fn write_data(&mut self, value: u8, ctx: &mut Ctx<'_>) {
        if self.remaining == 0 || self.mode != 0 {
            return;
        }
        if self.program_allowed {
            // The program address wraps inside the 256-byte page of `AR`.
            let location = (self.address & !255) | (self.address.wrapping_add(self.transferred) & 255);
            if location < self.capacity {
                let index = (location / PAGE_SIZE as u32) as usize;
                if self.pages[index].is_none() {
                    self.pages[index] = Some(Box::new([0xFF; PAGE_SIZE]));
                    self.page_count += 1;
                }
                if let Some(page) = &mut self.pages[index] {
                    page[(location % PAGE_SIZE as u32) as usize] &= value;
                }
                self.bytes_programmed += 1;
                self.dirty_transfer = true;
            }
        }
        self.transferred = self.transferred.wrapping_add(1);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.sr |= 2;
            if self.program_allowed {
                self.write_enabled = false;
                self.busy_ms = 2;
            }
            if self.dirty_transfer {
                self.storage_modified();
            }
            self.update_irq(ctx);
        }
    }

    fn erase_sector(&mut self, size: u32) {
        if !self.write_enabled || self.busy_ms != 0 {
            return;
        }
        let start = self.address / size * size;
        let mut offset = 0;
        while offset < size {
            let index = ((start + offset) / PAGE_SIZE as u32) as usize;
            if let Some(slot) = self.pages.get_mut(index) {
                if slot.take().is_some() {
                    self.page_count -= 1;
                }
            }
            offset += PAGE_SIZE as u32;
        }
        self.write_enabled = false;
        self.busy_ms = 10;
        self.erase_count += 1;
        self.storage_modified();
    }

    // ---- register access (ReadDoubleWord & co.) ----

    /// The value `SR` reads (computed from the flags and the transfer state).
    fn status(&self) -> u32 {
        self.sr | if self.remaining != 0 { 4 } else { 0 } | if self.mode == 1 { self.remaining.min(16) << 8 } else { 0 }
    }

    fn read_dword(&mut self, offset: u32, ctx: &mut Ctx<'_>) -> u32 {
        if offset == reg::SR {
            return self.status();
        }
        if offset == reg::DR {
            let mut value = 0u32;
            for i in 0..4 {
                if self.remaining == 0 {
                    break;
                }
                value |= u32::from(self.read_data(ctx)) << (8 * i);
            }
            return value;
        }
        self.get(offset)
    }

    fn write_dword(&mut self, offset: u32, value: u32, ctx: &mut Ctx<'_>) {
        if offset == reg::FCR {
            self.sr &= !(value & 0x1B);
            self.update_irq(ctx);
            return;
        }
        if offset == reg::DR {
            for i in 0..4 {
                if self.remaining == 0 {
                    break;
                }
                self.write_data((value >> (8 * i)) as u8, ctx);
            }
            return;
        }
        let previous = self.get(offset);
        self.registers.set(offset, value);
        if offset == reg::CR {
            if value & 2 != 0 {
                // ABORT.
                self.remaining = 0;
                self.sr |= 2;
                self.registers.set(reg::CR, value & !2);
            }
            self.update_irq(ctx);
        } else if offset == reg::CCR {
            // HAL transmit rewrites the same CCR mode without issuing AR again.
            if self.remaining != 0 && self.mode == 0 && value == previous {
                return;
            }
            if value & 0xC00 == 0 {
                self.begin_command(ctx);
            }
        } else if offset == reg::AR {
            self.begin_command(ctx);
        }
    }

    fn read_byte(&mut self, offset: u32, ctx: &mut Ctx<'_>) -> u8 {
        if offset == reg::DR {
            return self.read_data(ctx);
        }
        // Renode parity: only offset DR itself pops a single byte; DR + 1..3 go through the 32-bit read of DR
        // (which pops up to four bytes) and shift.
        (self.read_dword(offset & !3, ctx) >> ((offset & 3) * 8)) as u8
    }

    fn write_byte(&mut self, offset: u32, value: u8, ctx: &mut Ctx<'_>) {
        if offset == reg::DR {
            self.write_data(value, ctx);
            return;
        }
        let aligned = offset & !3;
        let shift = (offset & 3) * 8;
        let merged = (self.get(aligned) & !(0xFFu32 << shift)) | (u32::from(value) << shift);
        self.write_dword(aligned, merged, ctx);
    }

    /// The value a 32-bit read of `offset` returns, without the read's side effects (`DR` shows the next
    /// bytes that a read would consume, in the same way, without consuming them).
    fn peek_dword(&self, offset: u32) -> u32 {
        match offset {
            reg::SR => self.status(),
            reg::DR => {
                let mut value = 0u32;
                if self.mode == 1 {
                    for i in 0..self.remaining.min(4) {
                        value |= u32::from(self.peek_data(i)) << (8 * i);
                    }
                }
                value
            }
            _ => self.get(offset),
        }
    }

    /// The byte `read_data` would return `ahead` bytes from now (mode 1 and `ahead < remaining` assumed).
    fn peek_data(&self, ahead: u32) -> u8 {
        let index = self.transferred.wrapping_add(ahead);
        match self.last_command {
            0x05 => u8::from(self.busy_ms != 0) | if self.write_enabled { 2 } else { 0 },
            0x35 => 2,
            0x15 => u8::from(self.four_byte_address),
            0x9F => [0xEF, 0x40, 0x21][(index % 3) as usize],
            0x03 | 0x13 | 0x0B | 0x0C | 0x6B | 0x6C | 0xEB | 0xEC => self.get_storage_byte(self.address.wrapping_add(index)),
            _ => 0xFF,
        }
    }
}

impl Peripheral for NgcQuadSpi {
    fn name(&self) -> &str {
        &self.name
    }

    /// The 1 kHz busy countdown runs from the moment the peripheral exists (`timer.Start()` in the constructor).
    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.attach(ctx);
        self.timer.start(ctx);
    }

    /// `Reset`: registers, flags and busy time; the storage, statistics and the backing label persist.
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.reset_state();
        ctx.set_output(IRQ, false);
    }

    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match width {
            Width::Word => self.read_dword(offset, ctx),
            Width::Byte => u32::from(self.read_byte(offset, ctx)),
            // Not delivered: the bus rejects halfword accesses for this policy.
            Width::Half => 0,
        }
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match width {
            Width::Word => self.write_dword(offset, value, ctx),
            Width::Byte => self.write_byte(offset, value as u8, ctx),
            Width::Half => {}
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, _ctx: &mut Ctx<'_>) {
        if token == TICK && self.busy_ms > 0 {
            self.busy_ms -= 1;
        }
    }

    // IBytePeripheral + IDoubleWordPeripheral, no [AllowedTranslations]: halfword accesses are not supported.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::new(Widths::BYTE | Widths::WORD, Translations::NONE)
    }

    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        match width {
            Width::Word => Some(self.peek_dword(offset)),
            Width::Byte if offset == reg::DR => (self.mode == 1 && self.remaining != 0).then(|| u32::from(self.peek_data(0))),
            Width::Byte => Some((self.peek_dword(offset & !3) >> ((offset & 3) * 8)) & 0xFF),
            Width::Half => None,
        }
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
    use emu_core::PeriphId;
    use emu_core::TICKS_PER_MILLISECOND as MS;

    const BASE: u32 = 0xA000_1000;
    const CR: u32 = BASE + reg::CR;
    const SR: u32 = BASE + reg::SR;
    const FCR: u32 = BASE + reg::FCR;
    const DLR: u32 = BASE + reg::DLR;
    const CCR: u32 = BASE + reg::CCR;
    const AR: u32 = BASE + reg::AR;
    const DR: u32 = BASE + reg::DR;

    // CCR fields used by the firmware: instruction, ADMODE (bits 11:10), DMODE (25:24), FMODE (27:26).
    fn ccr(instruction: u32, address: bool, data: bool, read: bool) -> u32 {
        instruction | if address { 1 << 10 } else { 0 } | if data { 1 << 24 } else { 0 } | if read { 1 << 26 } else { 0 }
    }

    fn setup() -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, NgcQuadSpi::new("qspi"));
        (h, id)
    }

    fn qspi(h: &Harness, id: PeriphId) -> &NgcQuadSpi {
        h.get::<NgcQuadSpi>(id)
    }

    fn write_enable(h: &mut Harness) {
        h.write32(CCR, ccr(0x06, false, false, false));
    }

    /// Indirect page program of `data` at `address` (instruction 0x02).
    fn program(h: &mut Harness, address: u32, data: &[u8]) {
        h.write32(DLR, data.len() as u32 - 1);
        h.write32(CCR, ccr(0x02, true, true, false));
        h.write32(AR, address);
        for &byte in data {
            h.write8(DR, u32::from(byte));
        }
    }

    fn read_bytes(h: &mut Harness, address: u32, count: usize) -> Vec<u8> {
        h.write32(DLR, count as u32 - 1);
        h.write32(CCR, ccr(0x03, true, true, true));
        h.write32(AR, address);
        (0..count).map(|_| h.read8(DR) as u8).collect()
    }

    fn status(h: &mut Harness) -> u8 {
        h.write32(DLR, 0);
        h.write32(CCR, ccr(0x05, false, true, true));
        h.read8(DR) as u8
    }

    #[test]
    fn reset_state_and_bus_widths() {
        let (mut h, id) = setup();
        for offset in [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x1C, 0x20, 0x24, 0x3FC] {
            assert_eq!(h.read32(BASE + offset), 0, "offset 0x{offset:X}");
        }
        assert_eq!(qspi(&h, id).capacity(), 0x0800_0000);
        assert_eq!(qspi(&h, id).page_count(), 0);
        assert_eq!(h.read16(SR), 0, "halfword reads are not supported by the peripheral");
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Attempted Word read isn't supported"), "{warnings:?}");
        h.write16(CR, 0x1234);
        assert_eq!(h.read32(CR), 0, "halfword writes are dropped");
        assert_eq!(h.warnings().len(), 2);
        // Plain storage registers keep what is written.
        h.write32(BASE + reg::DCR, 0x001A_0000);
        h.write32(BASE + reg::ABR, 0xDEAD_BEEF);
        assert_eq!(h.read32(BASE + reg::DCR), 0x001A_0000);
        assert_eq!(h.read32(BASE + reg::ABR), 0xDEAD_BEEF);
        assert_eq!(h.read8(BASE + reg::ABR + 1), 0xBE);
        h.write8(BASE + reg::ABR + 2, 0x11);
        assert_eq!(h.read32(BASE + reg::ABR), 0xDE11_BEEF, "a byte write merges into the stored word");
    }

    #[test]
    fn jedec_status_and_simple_commands() {
        let (mut h, id) = setup();
        h.write32(DLR, 2);
        h.write32(CCR, ccr(0x9F, false, true, true));
        assert_eq!(h.read32(SR), 0x4 | (3 << 8), "BUSY and a FIFO level of three bytes while data is pending");
        assert_eq!([h.read8(DR), h.read8(DR), h.read8(DR)], [0xEF, 0x40, 0x21]);
        assert_eq!(h.read32(SR), 0x2, "TCF after the last byte");
        h.write32(FCR, 0x2);
        assert_eq!(h.read32(SR), 0);
        assert_eq!(status(&mut h), 0, "idle, write disabled");
        write_enable(&mut h);
        assert_eq!(status(&mut h), 2, "WEL after write enable");
        h.write32(CCR, ccr(0x04, false, false, false));
        assert_eq!(status(&mut h), 0);
        h.write32(DLR, 0);
        h.write32(CCR, ccr(0x35, false, true, true));
        assert_eq!(h.read8(DR), 2, "quad enable fixture");
        h.write32(CCR, ccr(0xB7, false, false, false));
        h.write32(DLR, 0);
        h.write32(CCR, ccr(0x15, false, true, true));
        assert_eq!(h.read8(DR), 1, "4-byte mode");
        h.write32(CCR, ccr(0xE9, false, false, false));
        h.write32(DLR, 0);
        h.write32(CCR, ccr(0x15, false, true, true));
        assert_eq!(h.read8(DR), 0);
        assert_eq!(qspi(&h, id).last_command(), 0x15);
        assert_eq!(qspi(&h, id).command_count(), 11);
    }

    #[test]
    fn program_read_and_and_semantics() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        program(&mut h, 0x1234, &[0xAB, 0xCD]);
        assert_eq!(qspi(&h, id).bytes_programmed(), 2);
        assert_eq!(qspi(&h, id).page_count(), 1);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1234), 0xAB);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1235), 0xCD);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1236), 0xFF);
        assert_eq!(status(&mut h), 1, "busy after a program, write enable consumed");
        h.advance_to(3 * MS);
        assert_eq!(status(&mut h), 0, "two 1 kHz ticks later the device is idle");
        write_enable(&mut h);
        program(&mut h, 0x1234, &[0x0F, 0xF0]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1234), 0x0B, "programming ANDs the data into the cell");
        assert_eq!(qspi(&h, id).get_storage_byte(0x1235), 0xC0);
        h.advance_to(6 * MS);
        assert_eq!(read_bytes(&mut h, 0x1234, 3), [0x0B, 0xC0, 0xFF]);
        assert_eq!(qspi(&h, id).bytes_read(), 3);
    }

    #[test]
    fn program_without_write_enable_or_while_busy_is_ignored() {
        let (mut h, id) = setup();
        program(&mut h, 0x40, &[0x11]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x40), 0xFF);
        assert_eq!(qspi(&h, id).bytes_programmed(), 0);
        assert_eq!(qspi(&h, id).page_count(), 0);
        write_enable(&mut h);
        program(&mut h, 0x40, &[0x11]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x40), 0x11);
        write_enable(&mut h); // ignored while busy
        program(&mut h, 0x41, &[0x22]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x41), 0xFF, "no program while busy");
        h.advance_to(2 * MS + 1);
        write_enable(&mut h);
        program(&mut h, 0x41, &[0x22]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x41), 0x22);
    }

    #[test]
    fn page_program_wraps_inside_the_256_byte_page() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        program(&mut h, 0x2FE, &[1, 2, 3, 4]);
        let q = qspi(&h, id);
        assert_eq!([q.get_storage_byte(0x2FE), q.get_storage_byte(0x2FF), q.get_storage_byte(0x200), q.get_storage_byte(0x201)], [1, 2, 3, 4]);
        assert_eq!(q.get_storage_byte(0x300), 0xFF);
    }

    #[test]
    fn word_data_port_moves_four_bytes() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        h.write32(DLR, 5);
        h.write32(CCR, ccr(0x02, true, true, false));
        h.write32(AR, 0x100);
        h.write32(DR, 0x4433_2211);
        h.write32(DR, 0xAAAA_6655);
        let q = qspi(&h, id);
        assert_eq!((0..6).map(|i| q.get_storage_byte(0x100 + i)).collect::<Vec<_>>(), [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        h.advance_to(4 * MS);
        h.write32(DLR, 5);
        h.write32(CCR, ccr(0x13, true, true, true));
        h.write32(AR, 0x100);
        assert_eq!(h.read32(DR), 0x4433_2211);
        assert_eq!(h.read32(DR), 0x0000_6655, "only the bytes that remain");
        assert_eq!(h.read32(DR), 0);
    }

    #[test]
    fn hal_transmit_rewriting_ccr_does_not_restart_the_command() {
        let (mut h, id) = setup();
        // A command with data but no address phase starts when CCR is written; HAL_QSPI_Transmit then
        // rewrites the same CCR value, which must not start it again.
        let write_status = ccr(0x01, false, true, false);
        h.write32(DLR, 2);
        h.write32(CCR, write_status);
        assert_eq!(qspi(&h, id).command_count(), 1);
        assert_eq!(h.read32(SR), 4, "BUSY, three bytes pending, no FIFO level in write mode");
        h.write8(DR, 0x01);
        h.write32(CCR, write_status);
        assert_eq!(qspi(&h, id).command_count(), 1, "identical rewrite while a write is in progress is ignored");
        assert_eq!(h.read32(SR), 4);
        h.write8(DR, 0x02);
        h.write8(DR, 0x03);
        assert_eq!(h.read32(SR), 2, "TCF after the last byte");
        h.write32(CCR, write_status);
        assert_eq!(qspi(&h, id).command_count(), 2, "with nothing in progress the same value is a new command");
        // A different value starts a new command even while data is pending.
        h.write8(DR, 0x04);
        h.write32(CCR, ccr(0x31, false, true, false));
        assert_eq!(qspi(&h, id).command_count(), 3);
        // With an address phase the command waits for AR.
        h.write32(CCR, ccr(0x02, true, true, false));
        assert_eq!(qspi(&h, id).command_count(), 3);
        h.write32(AR, 0x80);
        assert_eq!(qspi(&h, id).command_count(), 4);
    }

    #[test]
    fn erase_commands_clear_pages_and_set_busy() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        program(&mut h, 0x0000, &[0x12]);
        h.advance_to(3 * MS);
        write_enable(&mut h);
        program(&mut h, 0x1000, &[0x34]);
        h.advance_to(6 * MS);
        write_enable(&mut h);
        program(&mut h, 0x8000, &[0x56]);
        h.advance_to(9 * MS);
        assert_eq!(qspi(&h, id).page_count(), 3);
        // 4 KiB sector erase of page 1 only.
        write_enable(&mut h);
        h.write32(CCR, ccr(0x21, true, false, false));
        h.write32(AR, 0x1ABC);
        assert_eq!(qspi(&h, id).page_count(), 2);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1000), 0xFF);
        assert_eq!(qspi(&h, id).get_storage_byte(0x0000), 0x12);
        assert_eq!(qspi(&h, id).erase_count(), 1);
        assert_eq!(qspi(&h, id).busy_ms(), 10);
        assert_eq!(h.read32(SR) & 2, 2, "TCF");
        // A second erase without write enable does nothing (and busy ignores write enable).
        h.write32(CCR, ccr(0x21, true, false, false));
        h.write32(AR, 0);
        assert_eq!(qspi(&h, id).erase_count(), 1);
        h.advance_to(20 * MS);
        // 64 KiB block erase removes pages 0 and 8 (0x0000 and 0x8000).
        write_enable(&mut h);
        h.write32(CCR, ccr(0xD8, true, false, false));
        h.write32(AR, 0xFFFF);
        assert_eq!(qspi(&h, id).page_count(), 0);
        assert_eq!(qspi(&h, id).erase_count(), 2);
        h.advance_to(40 * MS);
        // 32 KiB erase and chip erase.
        write_enable(&mut h);
        program(&mut h, 0x20000, &[0x77]);
        h.advance_to(45 * MS);
        write_enable(&mut h);
        h.write32(CCR, ccr(0x52, true, false, false));
        h.write32(AR, 0x20000);
        assert_eq!(qspi(&h, id).get_storage_byte(0x20000), 0xFF);
        h.advance_to(60 * MS);
        write_enable(&mut h);
        program(&mut h, 0x20000, &[0x77]);
        h.advance_to(70 * MS);
        write_enable(&mut h);
        h.write32(CCR, ccr(0xC7, false, false, false));
        assert_eq!(qspi(&h, id).page_count(), 0);
        assert_eq!(qspi(&h, id).busy_ms(), 50);
        assert_eq!(qspi(&h, id).erase_count(), 4);
    }

    #[test]
    fn power_down_blocks_commands_until_release() {
        let (mut h, id) = setup();
        h.write32(CCR, ccr(0xB9, false, false, false));
        write_enable(&mut h);
        assert_eq!(h.read32(SR) & 2, 2, "TCF is raised without effect while powered down");
        h.write32(FCR, 2);
        h.write32(CCR, ccr(0xAB, false, false, false));
        write_enable(&mut h);
        assert_eq!(status(&mut h), 2);
        let _ = id;
    }

    #[test]
    fn unsupported_modes_raise_the_transfer_error_flag() {
        let (mut h, _id) = setup();
        h.write32(CCR, 3 << 26); // memory mapped
        assert_eq!(h.read32(SR), 1, "TEF");
        h.write32(FCR, 1);
        assert_eq!(h.read32(SR), 0);
        h.write32(DLR, 0);
        h.write32(CCR, (2 << 26) | (1 << 24)); // automatic polling
        assert_eq!(h.read32(SR) & 1, 1);
    }

    #[test]
    fn abort_clears_the_transfer_and_irq_follows_enable_bits() {
        let (mut h, id) = setup();
        h.connect_irq(id, IRQ, 99);
        h.clear_irq_changes();
        h.write32(CR, 1 << 17); // TCIE
        assert!(!h.irq_level(99));
        h.write32(DLR, 3);
        h.write32(CCR, ccr(0x9F, false, true, true));
        assert_eq!(h.read32(SR) & 4, 4, "BUSY");
        h.write32(CR, (1 << 17) | 2); // ABORT
        assert_eq!(h.read32(CR), 1 << 17, "ABORT self-clears");
        assert_eq!(h.read32(SR), 2, "TCF set by the abort");
        assert!(h.irq_level(99), "TCIE and TCF raise the line");
        h.write32(FCR, 2);
        assert!(!h.irq_level(99));
        // FTIE (bit 18) is not part of the model's interrupt mask.
        h.write32(CR, 1 << 18);
        h.write32(CCR, ccr(0x06, false, false, false));
        assert!(!h.irq_level(99));
        h.write32(CR, (1 << 17) | (1 << 18));
        assert!(h.irq_level(99));
    }

    #[test]
    fn byte_read_at_dr_plus_one_pops_a_word() {
        let (mut h, _id) = setup();
        h.write32(DLR, 7);
        h.write32(CCR, ccr(0x9F, false, true, true));
        // Quirk kept from the C# class: only offset DR itself is a byte pop; DR+1 is a 32-bit DR read
        // (four bytes consumed, JEDEC bytes cycle ef 40 21 ef ...) shifted right by eight bits.
        assert_eq!(h.read8(DR + 1), 0x40);
        assert_eq!(h.read32(SR) >> 8, 4, "four of the eight bytes were consumed");
        assert_eq!(h.read8(DR), 0x40, "the next byte pop continues with the fifth JEDEC byte");
    }

    #[test]
    fn busy_countdown_runs_at_1_khz_from_creation() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        h.advance_to(MS / 2);
        program(&mut h, 0, &[0]);
        assert_eq!(qspi(&h, id).busy_ms(), 2);
        h.advance_to(MS - 1);
        assert_eq!(qspi(&h, id).busy_ms(), 2);
        h.advance_to(MS);
        assert_eq!(qspi(&h, id).busy_ms(), 1, "first tick at exactly 1 ms");
        h.advance_to(2 * MS);
        assert_eq!(qspi(&h, id).busy_ms(), 0);
        h.advance_to(5 * MS);
        assert_eq!(qspi(&h, id).busy_ms(), 0);
    }

    #[test]
    fn reset_keeps_storage_and_clears_the_registers() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        program(&mut h, 0x10, &[0x5A]);
        h.write32(BASE + reg::ABR, 0x77);
        h.write32(CR, 1 << 17);
        assert_eq!(qspi(&h, id).busy_ms(), 2);
        h.core_mut().reset_all();
        assert_eq!(h.read32(CR), 0);
        assert_eq!(h.read32(BASE + reg::ABR), 0);
        assert_eq!(qspi(&h, id).get_storage_byte(0x10), 0x5A, "NOR cells survive a reset");
        assert_eq!(qspi(&h, id).busy_ms(), 0);
        assert_eq!(qspi(&h, id).command_count(), 2, "statistics survive a reset");
    }

    #[test]
    fn capacity_validation_and_bounds() {
        assert_eq!(NgcQuadSpi::with_capacity("q", 0).err(), Some(QspiError::BadCapacity(0)));
        assert_eq!(NgcQuadSpi::with_capacity("q", 4097).err(), Some(QspiError::BadCapacity(4097)));
        let small = NgcQuadSpi::with_capacity("q", 8192).unwrap();
        assert_eq!(small.get_storage_byte(8191), 0xFF);
        assert_eq!(small.get_storage_byte(8192), 0xFF);
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, SIZE, small);
        write_enable(&mut h);
        program(&mut h, 0x1FFF, &[0x00]);
        assert_eq!(qspi(&h, id).get_storage_byte(0x1FFF), 0x00);
        h.advance_to(5 * MS);
        write_enable(&mut h);
        program(&mut h, 0x2000, &[0x00]);
        assert_eq!(qspi(&h, id).page_count(), 1, "a program beyond the capacity is dropped");
    }

    #[test]
    fn summary_text() {
        let (mut h, id) = setup();
        assert_eq!(
            qspi(&h, id).describe(),
            "Generic QSPI NOR fixture: capacity=134217728; pages=0; commands=0; read=0; programmed=0; erases=0; last=0x00; busyMs=0; backing=volatile process storage"
        );
        write_enable(&mut h);
        program(&mut h, 0, &[1, 2, 3]);
        h.with::<NgcQuadSpi, _>(id, |q, _| q.load_backing("/tmp/x/nor.ngc", None).unwrap());
        assert_eq!(
            h.core().summaries().iter().find(|(n, _)| n == "qspi").unwrap().1,
            "Generic QSPI NOR fixture: capacity=134217728; pages=1; commands=2; read=0; programmed=3; erases=0; last=0x02; busyMs=2; backing=/tmp/x/nor.ngc"
        );
    }

    // ---- backing store ----

    fn sample() -> (Harness, PeriphId) {
        let (mut h, id) = setup();
        let mut t = 0;
        for (address, byte) in [(0x0u32, 0x11u8), (0x2000, 0x22), (0x7FF_F000, 0x33), (0x5000, 0xFF)] {
            write_enable(&mut h);
            program(&mut h, address, &[byte]);
            t += 5 * MS;
            h.advance_to(t);
        }
        (h, id)
    }

    #[test]
    fn backing_format_layout_and_round_trip() {
        let (h, id) = sample();
        let image = qspi(&h, id).serialize_backing();
        assert_eq!(image.len(), 16 + 4 * (4 + 4096));
        assert_eq!(&image[..8], b"NGCNOR01");
        assert_eq!(&image[8..12], &0x0800_0000u32.to_le_bytes());
        assert_eq!(&image[12..16], &4u32.to_le_bytes());
        // Records are ascending by page index; a page programmed with 0xFF is present.
        let pages: Vec<u32> = (0..4).map(|i| u32::from_le_bytes(image[16 + i * 4100..20 + i * 4100].try_into().unwrap())).collect();
        assert_eq!(pages, [0, 2, 5, 0x7FFF]);
        assert_eq!(image[20], 0x11);
        assert_eq!(image[20 + 4100], 0x22);
        assert!(image[20 + 2 * 4100..20 + 2 * 4100 + 4096].iter().all(|&b| b == 0xFF));
        assert_eq!(image[20 + 3 * 4100], 0x33);
        let mut other = NgcQuadSpi::new("other");
        other.load_backing("label", Some(&image)).unwrap();
        assert_eq!(other.page_count(), 4);
        assert_eq!(other.get_storage_byte(0x7FF_F000), 0x33);
        assert_eq!(other.serialize_backing(), image);
        assert_eq!(other.backing_path(), Some("label"));
    }

    #[test]
    fn backing_load_validation_matches_the_csharp_checks() {
        let (h, id) = sample();
        let image = qspi(&h, id).serialize_backing();
        let mut q = NgcQuadSpi::new("q");
        q.load_image(&image).unwrap();
        let before = q.serialize_backing();
        let reject = |q: &mut NgcQuadSpi, image: &[u8], expected: QspiError| {
            assert_eq!(q.load_image(image).err(), Some(expected));
            assert_eq!(q.serialize_backing(), before, "a rejected image must not change the storage");
        };
        reject(&mut q, &[], QspiError::Incompatible);
        reject(&mut q, b"NGCNOR0", QspiError::Incompatible);
        let mut bad_magic = image.clone();
        bad_magic[7] = b'2';
        reject(&mut q, &bad_magic, QspiError::Incompatible);
        reject(&mut q, b"NGCNOR01\x00\x00", QspiError::Truncated);
        let mut wrong_capacity = image.clone();
        wrong_capacity[8] = 1;
        reject(&mut q, &wrong_capacity, QspiError::Incompatible);
        reject(&mut q, &image[..12], QspiError::Truncated);
        let mut huge_count = image.clone();
        huge_count[12..16].copy_from_slice(&0x8001u32.to_le_bytes());
        reject(&mut q, &huge_count, QspiError::BadPageCount(0x8001));
        reject(&mut q, &image[..image.len() - 1], QspiError::BadPageRecord);
        reject(&mut q, &image[..18], QspiError::Truncated);
        let mut trailing = image.clone();
        trailing.push(0);
        reject(&mut q, &trailing, QspiError::TrailingData);
        let mut duplicate = image.clone();
        duplicate[16 + 4100..20 + 4100].copy_from_slice(&0u32.to_le_bytes());
        reject(&mut q, &duplicate, QspiError::BadPageRecord);
        let mut out_of_range = image.clone();
        out_of_range[16..20].copy_from_slice(&0x8000u32.to_le_bytes());
        reject(&mut q, &out_of_range, QspiError::BadPageRecord);
        // An empty store is a 16-byte image; a smaller capacity is incompatible.
        let empty = NgcQuadSpi::new("e").serialize_backing();
        assert_eq!(empty.len(), 16);
        q.load_image(&empty).unwrap();
        assert_eq!(q.page_count(), 0);
        let mut small = NgcQuadSpi::with_capacity("s", 8192).unwrap();
        assert_eq!(small.load_image(&empty).err(), Some(QspiError::Incompatible));
    }

    #[test]
    fn persistence_requests_follow_auto_persist() {
        let (mut h, id) = setup();
        assert!(!h.get_mut::<NgcQuadSpi>(id).take_persist_request());
        h.get_mut::<NgcQuadSpi>(id).load_backing("p", None).unwrap();
        assert!(h.get_mut::<NgcQuadSpi>(id).take_persist_request(), "a missing backing file is created");
        assert!(!h.get_mut::<NgcQuadSpi>(id).take_persist_request());
        write_enable(&mut h);
        program(&mut h, 0, &[0x01]);
        assert!(h.get_mut::<NgcQuadSpi>(id).take_persist_request(), "a completed program flushes");
        h.advance_to(5 * MS);
        h.get_mut::<NgcQuadSpi>(id).set_auto_persist(false);
        write_enable(&mut h);
        program(&mut h, 1, &[0x01]);
        assert!(!h.get_mut::<NgcQuadSpi>(id).take_persist_request(), "AutoPersist off");
        h.get_mut::<NgcQuadSpi>(id).flush();
        assert!(h.get_mut::<NgcQuadSpi>(id).take_persist_request(), "an explicit Flush always saves");
        h.get_mut::<NgcQuadSpi>(id).erase_storage();
        assert!(!h.get_mut::<NgcQuadSpi>(id).take_persist_request());
        assert_eq!(h.get::<NgcQuadSpi>(id).page_count(), 0);
        h.get_mut::<NgcQuadSpi>(id).set_auto_persist(true);
        h.get_mut::<NgcQuadSpi>(id).erase_storage();
        assert!(h.get_mut::<NgcQuadSpi>(id).take_persist_request());
    }

    #[test]
    fn revision_counts_changes_of_the_stored_pages() {
        let (mut h, id) = setup();
        assert_eq!(qspi(&h, id).revision(), 0);
        write_enable(&mut h);
        assert_eq!(qspi(&h, id).revision(), 0, "commands without data do not change the pages");
        program(&mut h, 0x10, &[0xA5]);
        assert_eq!(qspi(&h, id).revision(), 1);
        h.advance_to(5 * MS);
        let _ = status(&mut h);
        let _ = read_bytes(&mut h, 0x10, 1);
        assert_eq!(qspi(&h, id).revision(), 1, "reads and status polls change nothing");
        program(&mut h, 0x11, &[0x01]); // no write enable: ignored
        assert_eq!(qspi(&h, id).revision(), 1);
        write_enable(&mut h);
        h.write32(CCR, ccr(0x21, true, false, false));
        h.write32(AR, 0x10);
        assert_eq!(qspi(&h, id).revision(), 2, "sector erase");
        h.advance_to(30 * MS);
        write_enable(&mut h);
        h.write32(CCR, ccr(0xC7, false, false, false));
        assert_eq!(qspi(&h, id).revision(), 3, "chip erase");
        // The revision is independent of AutoPersist and of the backing label.
        h.get_mut::<NgcQuadSpi>(id).set_auto_persist(false);
        h.get_mut::<NgcQuadSpi>(id).erase_storage();
        assert_eq!(qspi(&h, id).revision(), 4);
        assert!(!h.get_mut::<NgcQuadSpi>(id).take_persist_request());
        let image = qspi(&h, id).serialize_backing();
        h.get_mut::<NgcQuadSpi>(id).load_image(&image).unwrap();
        assert_eq!(qspi(&h, id).revision(), 5);
        assert!(h.get_mut::<NgcQuadSpi>(id).load_image(b"nope").is_err());
        assert_eq!(qspi(&h, id).revision(), 5, "a rejected image changes nothing");
    }

    #[test]
    fn peek_matches_read_without_side_effects() {
        let (mut h, id) = setup();
        write_enable(&mut h);
        program(&mut h, 0, &[0xA1, 0xB2, 0xC3, 0xD4, 0xE5]);
        h.advance_to(5 * MS);
        h.write32(DLR, 4);
        h.write32(CCR, ccr(0x03, true, true, true));
        h.write32(AR, 0);
        assert_eq!(h.peek(DR, Width::Word), Some(0xD4C3_B2A1));
        assert_eq!(h.peek(DR, Width::Byte), Some(0xA1));
        assert_eq!(h.peek(SR, Width::Word), Some(h.read32(SR)));
        assert_eq!(h.read32(DR), 0xD4C3_B2A1, "peek consumed nothing");
        assert_eq!(h.peek(DR, Width::Word), Some(0xE5));
        assert_eq!(qspi(&h, id).bytes_read(), 4);
        assert_eq!(h.peek(SR, Width::Half), None);
    }
}
