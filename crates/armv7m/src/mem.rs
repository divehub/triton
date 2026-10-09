//! Data-path memory access: plain bus access with MMIO notification polling,
//! and the private peripheral bus (0xE000_0000..=0xE00F_FFFF) handled in the core.

use crate::cpu::Cpu;
use crate::CpuBus;

impl Cpu {
    #[inline(always)]
    pub(crate) fn ld8<B: CpuBus>(&mut self, bus: &mut B, addr: u32) -> u32 {
        if addr >> 20 == 0xE00 {
            return self.ppb_read(bus, addr, 1);
        }
        let v = bus.read8(addr, self.tb_icount) as u32;
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
        v
    }

    #[inline(always)]
    pub(crate) fn ld16<B: CpuBus>(&mut self, bus: &mut B, addr: u32) -> u32 {
        if addr >> 20 == 0xE00 {
            return self.ppb_read(bus, addr, 2);
        }
        let v = bus.read16(addr, self.tb_icount) as u32;
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
        v
    }

    #[inline(always)]
    pub(crate) fn ld32<B: CpuBus>(&mut self, bus: &mut B, addr: u32) -> u32 {
        if addr >> 20 == 0xE00 {
            return self.ppb_read(bus, addr, 4);
        }
        let v = bus.read32(addr, self.tb_icount);
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
        v
    }

    #[inline(always)]
    pub(crate) fn st8<B: CpuBus>(&mut self, bus: &mut B, addr: u32, v: u32) {
        if addr >> 20 == 0xE00 {
            self.ppb_write(addr, 1, v);
            return;
        }
        bus.write8(addr, v as u8, self.tb_icount);
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
    }

    #[inline(always)]
    pub(crate) fn st16<B: CpuBus>(&mut self, bus: &mut B, addr: u32, v: u32) {
        if addr >> 20 == 0xE00 {
            self.ppb_write(addr, 2, v);
            return;
        }
        bus.write16(addr, v as u16, self.tb_icount);
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
    }

    #[inline(always)]
    pub(crate) fn st32<B: CpuBus>(&mut self, bus: &mut B, addr: u32, v: u32) {
        if addr >> 20 == 0xE00 {
            self.ppb_write(addr, 4, v);
            return;
        }
        bus.write32(addr, v, self.tb_icount);
        let n = bus.take_notifications();
        if n != 0 {
            self.handle_notifications(bus, n);
        }
    }

    /// Word load that must be aligned (LDM/LDRD/LDREX/...): raises UNALIGNED and returns `None`.
    #[inline(always)]
    pub(crate) fn require_aligned(&mut self, addr: u32, pc: u32) -> bool {
        if addr & 3 != 0 {
            self.usage_fault(crate::cpu::CFSR_UNALIGNED, pc);
            return false;
        }
        true
    }

    /// Unaligned plain access check (CCR.UNALIGN_TRP). Returns true when the access may proceed.
    #[inline(always)]
    pub(crate) fn unaligned_ok(&mut self, addr: u32, mask: u32, pc: u32) -> bool {
        if addr & mask != 0 && self.scb.ccr & crate::cpu::CCR_UNALIGN_TRP != 0 {
            self.usage_fault(crate::cpu::CFSR_UNALIGNED, pc);
            return false;
        }
        true
    }

    // ---- private peripheral bus -------------------------------------------------------

    #[cold]
    #[inline(never)]
    pub(crate) fn ppb_read<B: CpuBus>(&mut self, bus: &mut B, addr: u32, size: u32) -> u32 {
        let aligned = addr & !3;
        let shift = (addr & 3) * 8;
        if !Cpu::ppb_modeled(aligned) {
            self.warn_once(aligned, || format!("PPB read from unmodeled address 0x{aligned:08x} returns 0"));
        }
        // Renode calls `cpu.SyncTime()` before returning SysTick CVR or DWT CYCCNT: the machine
        // clock catches up with the start of this instruction.
        if aligned == 0xE000_E018 || aligned == 0xE000_1004 {
            self.sync_time(bus);
        }
        let now = self.clock_time;
        let w0 = self.ppb_peek_word(aligned, now);
        if aligned == 0xE000_E010 {
            // SYST_CSR.COUNTFLAG is read-to-clear.
            self.systick.countflag = false;
        }
        match size {
            1 => (w0 >> shift) & 0xFF,
            2 => {
                if shift <= 16 {
                    (w0 >> shift) & 0xFFFF
                } else {
                    let w1 = self.ppb_peek_word(aligned.wrapping_add(4), now);
                    ((w0 >> 24) | (w1 << 8)) & 0xFFFF
                }
            }
            _ => {
                if shift == 0 {
                    w0
                } else {
                    let w1 = self.ppb_peek_word(aligned.wrapping_add(4), now);
                    (w0 >> shift) | (w1 << (32 - shift))
                }
            }
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn ppb_write(&mut self, addr: u32, size: u32, value: u32) {
        let aligned = addr & !3;
        let shift = (addr & 3) * 8;
        match size {
            4 if shift == 0 => self.ppb_write_word(aligned, value),
            4 => {
                // Unaligned word store: split into byte stores.
                for i in 0..4u32 {
                    let a = addr.wrapping_add(i);
                    self.ppb_write_partial(a & !3, (a & 3) * 8, 1, (value >> (8 * i)) & 0xFF);
                }
            }
            2 if shift == 24 => {
                self.ppb_write_partial(aligned, 24, 1, value & 0xFF);
                self.ppb_write_partial(aligned.wrapping_add(4), 0, 1, (value >> 8) & 0xFF);
            }
            _ => self.ppb_write_partial(aligned, shift, size, value),
        }
    }
}
