// `ArrayMemory` is ported from Renode 1.17.0 src/Emulator/Main/Peripherals/Memory/ArrayMemory.cs
// (MIT License, Copyright (c) Antmicro).

//! Plain memories (flash, SRAM) and the `ArrayMemory` register store.

use crate::access::Width;
use crate::machine::{Ctx, View};
use crate::peripheral::Peripheral;
use std::any::Any;

/// Placement of the machine's plain memories. Both NGC boards use [`MemoryLayout::STM32L4_1M`]
/// (`handset.repl` / `main.repl`: flash 1 MiB at 0x08000000, SRAM1 96 KiB at 0x20000000,
/// SRAM2 32 KiB at 0x10000000).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLayout {
    pub flash_base: u32,
    pub flash_size: u32,
    pub sram1_base: u32,
    pub sram1_size: u32,
    pub sram2_base: u32,
    pub sram2_size: u32,
}

impl MemoryLayout {
    pub const STM32L4_1M: MemoryLayout = MemoryLayout {
        flash_base: 0x0800_0000,
        flash_size: 0x10_0000,
        sram1_base: 0x2000_0000,
        sram1_size: 0x1_8000,
        sram2_base: 0x1000_0000,
        sram2_size: 0x8000,
    };

    /// True if `[addr, addr + len)` intersects any plain memory (used to reject MMIO overlaps).
    pub fn overlaps(&self, addr: u32, len: u32) -> bool {
        let end = u64::from(addr) + u64::from(len);
        [
            (self.flash_base, self.flash_size),
            (self.sram1_base, self.sram1_size),
            (self.sram2_base, self.sram2_size),
        ]
        .iter()
        .any(|&(base, size)| u64::from(addr) < u64::from(base) + u64::from(size) && end > u64::from(base))
    }
}

impl Default for MemoryLayout {
    fn default() -> Self {
        MemoryLayout::STM32L4_1M
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    Flash,
    Sram1,
    Sram2,
}

/// Flash and SRAM contents. All are plain little-endian byte arrays (Renode `MappedMemory`):
/// reads never have side effects, writes always take effect (including to flash, as the stock
/// platform maps flash as ordinary memory; flash writes bump [`PlainMemory::flash_epoch`]).
pub struct PlainMemory {
    pub layout: MemoryLayout,
    pub flash: Box<[u8]>,
    pub sram1: Box<[u8]>,
    pub sram2: Box<[u8]>,
    flash_epoch: u32,
}

impl PlainMemory {
    /// Zero-initialized memories (Renode `LoadBinary` into a fresh `MappedMemory`).
    pub fn new(layout: MemoryLayout) -> Self {
        Self {
            layout,
            flash: vec![0u8; layout.flash_size as usize].into_boxed_slice(),
            sram1: vec![0u8; layout.sram1_size as usize].into_boxed_slice(),
            sram2: vec![0u8; layout.sram2_size as usize].into_boxed_slice(),
            flash_epoch: 0,
        }
    }

    /// Incremented by every write that lands in flash; consumers that cache decoded flash
    /// contents (predecode) compare it to detect self-modification.
    #[inline]
    pub fn flash_epoch(&self) -> u32 {
        self.flash_epoch
    }

    /// Region containing `[addr, addr + len)` entirely, with the offset into it.
    #[inline]
    pub fn locate(&self, addr: u32, len: u32) -> Option<(RegionKind, usize)> {
        let l = &self.layout;
        let o = addr.wrapping_sub(l.sram1_base);
        if o < l.sram1_size && len <= l.sram1_size - o {
            return Some((RegionKind::Sram1, o as usize));
        }
        let o = addr.wrapping_sub(l.flash_base);
        if o < l.flash_size && len <= l.flash_size - o {
            return Some((RegionKind::Flash, o as usize));
        }
        let o = addr.wrapping_sub(l.sram2_base);
        if o < l.sram2_size && len <= l.sram2_size - o {
            return Some((RegionKind::Sram2, o as usize));
        }
        None
    }

    /// True if `addr` itself lies in a plain memory (the access may still run off its end).
    #[inline]
    pub fn contains(&self, addr: u32) -> bool {
        self.locate(addr, 1).is_some()
    }

    fn region(&self, kind: RegionKind) -> &[u8] {
        match kind {
            RegionKind::Flash => &self.flash,
            RegionKind::Sram1 => &self.sram1,
            RegionKind::Sram2 => &self.sram2,
        }
    }

    fn region_mut(&mut self, kind: RegionKind) -> &mut [u8] {
        match kind {
            RegionKind::Flash => &mut self.flash,
            RegionKind::Sram1 => &mut self.sram1,
            RegionKind::Sram2 => &mut self.sram2,
        }
    }

    /// Little-endian read; `None` unless the whole access lies inside one plain memory.
    #[inline]
    pub fn read(&self, addr: u32, width: Width) -> Option<u32> {
        let (kind, o) = self.locate(addr, width.bytes())?;
        let data = self.region(kind);
        Some(match width {
            Width::Byte => u32::from(data[o]),
            Width::Half => u32::from(u16::from_le_bytes([data[o], data[o + 1]])),
            Width::Word => u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
        })
    }

    /// Little-endian write; `false` unless the whole access lies inside one plain memory.
    #[inline]
    pub fn write(&mut self, addr: u32, width: Width, value: u32) -> bool {
        let Some((kind, o)) = self.locate(addr, width.bytes()) else { return false };
        if kind == RegionKind::Flash {
            self.flash_epoch = self.flash_epoch.wrapping_add(1);
        }
        let data = self.region_mut(kind);
        match width {
            Width::Byte => data[o] = value as u8,
            Width::Half => data[o..o + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            Width::Word => data[o..o + 4].copy_from_slice(&value.to_le_bytes()),
        }
        true
    }

    /// Borrowed view of `[addr, addr + len)` if it lies inside one plain memory.
    pub fn slice(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let len32 = u32::try_from(len).ok()?;
        let (kind, o) = self.locate(addr, len32)?;
        Some(&self.region(kind)[o..o + len])
    }

    /// Mutable view of `[addr, addr + len)`; counts as a flash write when it lies in flash.
    pub fn slice_mut(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        let len32 = u32::try_from(len).ok()?;
        let (kind, o) = self.locate(addr, len32)?;
        if kind == RegionKind::Flash {
            self.flash_epoch = self.flash_epoch.wrapping_add(1);
        }
        Some(&mut self.region_mut(kind)[o..o + len])
    }

    /// Copies `bytes` to `addr` (Renode `LoadBinary`).
    pub fn load(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        match self.slice_mut(addr, bytes.len()) {
            Some(target) => {
                target.copy_from_slice(bytes);
                Ok(())
            }
            None => Err(format!("{} bytes at 0x{addr:08x} do not fit in a plain memory", bytes.len())),
        }
    }
}

/// Renode `Memory.ArrayMemory`: a byte array that implements every access width natively,
/// little-endian, any alignment. Used for the simplified PWR, FLASH-control, FMC and SYSCFG
/// register blocks (they read back what was written and have no behavior).
///
/// Renode parity (`ArrayMemory.IsCorrectOffset`): an access that does not lie entirely inside
/// the array returns 0 / is dropped and is logged as an error.
pub struct ArrayMemory {
    name: String,
    data: Vec<u8>,
}

impl ArrayMemory {
    pub fn new(name: impl Into<String>, size: usize) -> Self {
        Self { name: name.into(), data: vec![0; size] }
    }

    pub fn size(&self) -> usize {
        self.data.len()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Value at `offset`, or `None` if the access does not fit.
    pub fn read_value(&self, offset: u32, width: Width) -> Option<u32> {
        let o = offset as usize;
        let n = width.bytes() as usize;
        let bytes = self.data.get(o..o.checked_add(n)?)?;
        Some(match width {
            Width::Byte => u32::from(bytes[0]),
            Width::Half => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            Width::Word => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        })
    }

    /// Stores `value`; `false` if the access does not fit.
    pub fn write_value(&mut self, offset: u32, width: Width, value: u32) -> bool {
        let o = offset as usize;
        let n = width.bytes() as usize;
        let Some(end) = o.checked_add(n) else { return false };
        match self.data.get_mut(o..end) {
            Some(bytes) => {
                bytes.copy_from_slice(&value.to_le_bytes()[..n]);
                true
            }
            None => false,
        }
    }

    /// Little-endian 32-bit register at a word-aligned offset (0 if out of range).
    pub fn word(&self, offset: u32) -> u32 {
        self.read_value(offset, Width::Word).unwrap_or(0)
    }

    pub fn set_word(&mut self, offset: u32, value: u32) {
        self.write_value(offset, Width::Word, value);
    }
}

impl Peripheral for ArrayMemory {
    fn name(&self) -> &str {
        &self.name
    }

    fn reset(&mut self, _ctx: &mut Ctx<'_>) {
        // Renode: "nothing happens" - register contents survive a peripheral reset.
    }

    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.read_value(offset, width) {
            Some(value) => value,
            None => {
                ctx.error_once(
                    u64::from(offset) | (u64::from(width.bytes()) << 32),
                    format_args!(
                        "Tried to read {} byte(s) at offset 0x{:X} outside the range of the peripheral 0x0 - 0x{:X}",
                        width.bytes(),
                        offset,
                        self.data.len().saturating_sub(1)
                    ),
                );
                0
            }
        }
    }

    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
        if !self.write_value(offset, width, value) {
            ctx.error_once(
                (1 << 63) | u64::from(offset) | (u64::from(width.bytes()) << 32),
                format_args!(
                    "Tried to write {} byte(s) at offset 0x{:X} outside the range of the peripheral 0x0 - 0x{:X}",
                    width.bytes(),
                    offset,
                    self.data.len().saturating_sub(1)
                ),
            );
        }
    }

    fn peek(&self, offset: u32, width: Width, _view: &View<'_>) -> Option<u32> {
        self.read_value(offset, width)
    }

    fn poke(&mut self, offset: u32, width: Width, value: u32, _ctx: &mut Ctx<'_>) -> bool {
        self.write_value(offset, width, value)
    }

    fn summary(&self, _view: &View<'_>) -> String {
        format!("array memory: {} bytes", self.data.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_overlap_detection() {
        let l = MemoryLayout::STM32L4_1M;
        assert!(l.overlaps(0x0800_0000, 4));
        assert!(l.overlaps(0x080F_FFFF, 1));
        assert!(!l.overlaps(0x0810_0000, 4));
        assert!(l.overlaps(0x2001_7FFF, 4));
        assert!(!l.overlaps(0x2001_8000, 4));
        assert!(l.overlaps(0x1000_7FFC, 4));
        assert!(!l.overlaps(0x1000_8000, 0x1000));
        assert!(!l.overlaps(0x4000_0000, 0x400));
    }

    #[test]
    fn plain_memory_round_trips_all_widths_and_alignments() {
        let mut m = PlainMemory::new(MemoryLayout::STM32L4_1M);
        for (base, size) in [(0x2000_0000u32, 0x1_8000u32), (0x1000_0000, 0x8000), (0x0800_0000, 0x10_0000)] {
            assert!(m.write(base, Width::Word, 0x1122_3344));
            assert_eq!(m.read(base, Width::Word), Some(0x1122_3344));
            assert_eq!(m.read(base, Width::Byte), Some(0x44));
            assert_eq!(m.read(base + 1, Width::Half), Some(0x2233));
            assert_eq!(m.read(base + 3, Width::Byte), Some(0x11));
            // Unaligned word straddling two words.
            assert!(m.write(base + 5, Width::Word, 0xAABB_CCDD));
            assert_eq!(m.read(base + 5, Width::Word), Some(0xAABB_CCDD));
            assert_eq!(m.read(base + 4, Width::Byte), Some(0));
            assert_eq!(m.read(base + 9, Width::Byte), Some(0));
            // Last bytes of the region.
            assert!(m.write(base + size - 4, Width::Word, 0xDEAD_BEEF));
            assert_eq!(m.read(base + size - 4, Width::Word), Some(0xDEAD_BEEF));
            assert_eq!(m.read(base + size - 1, Width::Byte), Some(0xDE));
            // Accesses running off the end or starting outside are rejected.
            assert_eq!(m.read(base + size - 3, Width::Word), None);
            assert_eq!(m.read(base + size - 1, Width::Half), None);
            assert!(!m.write(base + size - 2, Width::Word, 1));
            assert_eq!(m.read(base + size, Width::Byte), None);
            assert_eq!(m.read(base.wrapping_sub(1), Width::Byte), None);
        }
        assert_eq!(m.read(0x4000_0000, Width::Word), None);
        assert!(!m.contains(0));
        assert!(m.contains(0x2001_7FFF));
        assert!(!m.contains(0x2001_8000));
    }

    #[test]
    fn flash_writes_bump_epoch_and_load_works() {
        let mut m = PlainMemory::new(MemoryLayout::STM32L4_1M);
        assert_eq!(m.flash_epoch(), 0);
        assert!(m.write(0x2000_0000, Width::Word, 1));
        assert_eq!(m.flash_epoch(), 0);
        assert!(m.write(0x0800_4000, Width::Byte, 0x5A));
        assert_eq!(m.flash_epoch(), 1);
        m.load(0x0800_4000, &[1, 2, 3, 4]).unwrap();
        assert_eq!(m.read(0x0800_4000, Width::Word), Some(0x0403_0201));
        assert!(m.load(0x080F_FFFE, &[0; 4]).is_err());
        assert!(m.load(0x3000_0000, &[0; 4]).is_err());
        assert_eq!(m.slice(0x0800_4000, 4), Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(m.slice(0x080F_FFFF, 2), None);
        m.slice_mut(0x2000_0010, 3).unwrap().copy_from_slice(&[7, 8, 9]);
        assert_eq!(m.read(0x2000_0010, Width::Half), Some(0x0807));
    }

    #[test]
    fn flash_is_zero_initialized() {
        let m = PlainMemory::new(MemoryLayout::STM32L4_1M);
        assert!(m.flash.iter().all(|&b| b == 0));
        assert_eq!(m.flash.len(), 0x10_0000);
        assert_eq!(m.sram1.len(), 0x1_8000);
        assert_eq!(m.sram2.len(), 0x8000);
    }

    #[test]
    fn array_memory_bounds_and_widths() {
        let mut a = ArrayMemory::new("pwr", 0x400);
        assert!(a.write_value(0x10, Width::Word, 0x104));
        assert_eq!(a.read_value(0x10, Width::Word), Some(0x104));
        assert_eq!(a.read_value(0x11, Width::Byte), Some(0x01));
        assert_eq!(a.read_value(0x0F, Width::Half), Some(0x0400));
        assert!(a.write_value(0x3FC, Width::Word, 0xFFFF_FFFF));
        assert_eq!(a.word(0x3FC), 0xFFFF_FFFF);
        // Straddling the end: nothing is read or written, not even the in-range bytes.
        assert_eq!(a.read_value(0x3FD, Width::Word), None);
        assert!(!a.write_value(0x3FE, Width::Word, 0));
        assert_eq!(a.word(0x3FC), 0xFFFF_FFFF);
        assert_eq!(a.read_value(0x400, Width::Byte), None);
        assert_eq!(a.read_value(u32::MAX, Width::Word), None);
        assert!(!a.write_value(u32::MAX, Width::Half, 1));
        a.set_word(0, 0xAABB_CCDD);
        assert_eq!(a.bytes()[..4], [0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(a.size(), 0x400);
    }
}
