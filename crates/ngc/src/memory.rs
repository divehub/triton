//! Memory map shared by the main and handset boards (`emulation/handset.repl` / `main.repl`).
//!
//! Plain memories (identical on both boards):
//!
//! | region | base | size |
//! | --- | --- | --- |
//! | flash (`Memory.MappedMemory`, zero-initialized) | `0x0800_0000` | 1 MiB |
//! | SRAM1 | `0x2000_0000` | 96 KiB |
//! | SRAM2 | `0x1000_0000` | 32 KiB |
//!
//! Everything else on the 32-bit bus is memory-mapped I/O dispatched through `emu_core`'s MMIO
//! table, except `0xE000_0000..=0xE00F_FFFF` (the private peripheral bus: NVIC, SCB, SysTick,
//! MPU registers, FPU control and DWT), which the CPU core handles itself and never forwards.
//! Unmapped addresses read 0 and ignore writes (Renode behaviour), warning once per address.

pub use emu_core::{MemoryLayout, PlainMemory};

/// The layout of both NGC boards.
pub const LAYOUT: MemoryLayout = MemoryLayout::STM32L4_1M;

pub const FLASH_BASE: u32 = LAYOUT.flash_base;
pub const FLASH_SIZE: u32 = LAYOUT.flash_size;
pub const SRAM1_BASE: u32 = LAYOUT.sram1_base;
pub const SRAM1_SIZE: u32 = LAYOUT.sram1_size;
pub const SRAM2_BASE: u32 = LAYOUT.sram2_base;
pub const SRAM2_SIZE: u32 = LAYOUT.sram2_size;

/// Private peripheral bus, handled inside the CPU core.
pub const PPB_BASE: u32 = 0xE000_0000;
pub const PPB_LAST: u32 = 0xE00F_FFFF;

#[inline]
pub const fn is_ppb(addr: u32) -> bool {
    addr >= PPB_BASE && addr <= PPB_LAST
}

#[inline]
pub const fn in_flash(addr: u32) -> bool {
    addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE
}

#[inline]
pub const fn in_sram1(addr: u32) -> bool {
    addr.wrapping_sub(SRAM1_BASE) < SRAM1_SIZE
}

#[inline]
pub const fn in_sram2(addr: u32) -> bool {
    addr.wrapping_sub(SRAM2_BASE) < SRAM2_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_platform_descriptions() {
        assert_eq!((FLASH_BASE, FLASH_SIZE), (0x0800_0000, 0x10_0000));
        assert_eq!((SRAM1_BASE, SRAM1_SIZE), (0x2000_0000, 0x1_8000));
        assert_eq!((SRAM2_BASE, SRAM2_SIZE), (0x1000_0000, 0x8000));
        assert!(in_flash(0x080F_FFFF) && !in_flash(0x0810_0000) && !in_flash(0x07FF_FFFF));
        assert!(in_sram1(0x2001_7FFF) && !in_sram1(0x2001_8000));
        assert!(in_sram2(0x1000_7FFF) && !in_sram2(0x1000_8000));
        assert!(is_ppb(0xE000_ED10) && is_ppb(0xE000_1004) && !is_ppb(0xE010_0000) && !is_ppb(0xDFFF_FFFF));
    }
}
