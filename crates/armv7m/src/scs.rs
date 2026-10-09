// Ported from Renode 1.17.0 src/Emulator/Cores/Arm-M/NVIC.cs and DWT.cs (MIT License, Copyright (c) Antmicro).
//
//! System control space (0xE000_E000..0xE000_EFFF) and DWT (0xE000_1000..0xE000_1FFF)
//! register models. Reads are side-effect free (`ppb_peek_word`); the only
//! read side effect (SYST_CSR.COUNTFLAG clear) is applied by `ppb_read`.
//! Every other PPB address reads 0 and ignores writes (Renode has no model there).

use crate::cpu::*;
use crate::nvic::*;
use emu_core::Time;

const SCS: u32 = 0xE000_E000;
const DWT: u32 = 0xE000_1000;

/// Renode parity: `cortex-m4` CPUID (r0p0).
pub const CPUID_CORTEX_M4: u32 = 0x410F_C240;

/// Renode parity (tlib `tlib_set_mpu_region_size_and_enable`): the attribute halfword is stored whole,
/// the subregion-disable byte and the six size/enable bits too; bits 7:6 are not stored.
const MPU_RASR_MASK: u32 = 0xFFFF_FF3F;

impl Cpu {
    /// Mask of the IRQ lines of register `k` that exist on this core.
    fn irq_reg_mask(&self, k: usize) -> u32 {
        let first = k * 32;
        let n = self.nvic.num_irqs() as usize;
        if first >= n {
            0
        } else if n - first >= 32 {
            u32::MAX
        } else {
            (1u32 << (n - first)) - 1
        }
    }

    /// SysTick counter value at `now` without disturbing the timer.
    fn systick_value_at(&self, now: Time) -> u32 {
        if now <= self.systick.at() {
            self.systick.value()
        } else {
            let mut s = self.systick.clone();
            s.advance_to(now);
            s.value()
        }
    }

    /// DWT CYCCNT at `now` without disturbing the counter.
    fn cyccnt_at(&self, now: Time) -> u32 {
        // The counter is advanced lazily (see `Cpu::advance_clock`); it has always been at least as far as the
        // machine clock, so an earlier `now` shows the value at the clock time.
        let mut d = self.dwt.clone();
        d.advance_to(now.max(self.clock_time));
        d.cyccnt()
    }

    /// A Renode `LimitTimer` setter ran: `RequestReturn()` ends the chunk at the end of the
    /// current translation block.
    #[inline]
    fn limit_timer_touched(&mut self) {
        self.exit_pending = true;
        self.kick();
    }

    /// Side-effect-free read of the aligned PPB word at `a`.
    pub(crate) fn ppb_peek_word(&self, a: u32, now: Time) -> u32 {
        match a {
            0xE000_E000..=0xE000_EFFF => self.scs_read(a - SCS, now),
            0xE000_1000..=0xE000_1FFF => self.dwt_read(a - DWT, now),
            _ => 0,
        }
    }

    /// True for addresses with a register model (everything else warns once).
    pub(crate) fn ppb_modeled(a: u32) -> bool {
        (0xE000_E000..=0xE000_EFFF).contains(&a) || (0xE000_1000..=0xE000_1FFF).contains(&a)
    }

    fn scs_read(&self, off: u32, now: Time) -> u32 {
        match off {
            0x004 => 7, // ICTR (Renode parity: fixed value)
            0x010 => {
                let s = &self.systick;
                (s.enabled as u32) | ((s.tickint as u32) << 1) | (1 << 2) | ((s.countflag as u32) << 16)
            }
            0x014 => self.systick.reload(),
            0x018 => self.systick_value_at(now),
            0x01C => self.systick.calib(),
            0x100..=0x13C => {
                let k = ((off - 0x100) >> 2) as usize;
                self.nvic.enabled_irq_reg(k) & self.irq_reg_mask(k)
            }
            0x180..=0x1BC => {
                let k = ((off - 0x180) >> 2) as usize;
                self.nvic.enabled_irq_reg(k) & self.irq_reg_mask(k)
            }
            0x200..=0x23C => {
                let k = ((off - 0x200) >> 2) as usize;
                self.nvic.pending_irq_reg(k) & self.irq_reg_mask(k)
            }
            0x280..=0x2BC => {
                let k = ((off - 0x280) >> 2) as usize;
                self.nvic.pending_irq_reg(k) & self.irq_reg_mask(k)
            }
            0x300..=0x31C => {
                let k = ((off - 0x300) >> 2) as usize;
                self.nvic.active_irq_reg(k) & self.irq_reg_mask(k)
            }
            0x400..=0x7EF => {
                let first = (off - 0x400) as usize;
                let mut v = 0u32;
                for j in 0..4 {
                    let exc = EXC_IRQ0 + first + j;
                    if exc < MAX_EXC {
                        v |= (self.nvic.priority(exc) as u32) << (8 * j);
                    }
                }
                v
            }
            0xD00 => CPUID_CORTEX_M4,
            0xD04 => self.icsr_read(),
            0xD08 => self.scb.vtor,
            // VECTKEYSTAT, BFHFNMINS (bit 13: Renode reports it set when TrustZone is absent) and PRIGROUP.
            0xD0C => 0xFA05_2000 | (self.nvic.prigroup << 8),
            0xD10 => self.scb.scr,
            0xD14 => self.scb.ccr,
            0xD18 | 0xD1C | 0xD20 => {
                let first = 4 + (off - 0xD18) as usize;
                let mut v = 0u32;
                for j in 0..4 {
                    v |= (self.nvic.priority(first + j) as u32) << (8 * j);
                }
                v
            }
            0xD24 => self.shcsr_read(),
            0xD28 => self.scb.cfsr,
            0xD2C => ((self.nvic.hardfault_vecttbl as u32) << 1) | ((self.nvic.hardfault_forced as u32) << 30),
            0xD34 => self.scb.mmfar,
            0xD38 => self.scb.bfar,
            0xD88 => self.scb.cpacr,
            0xD90 => 8 << 8,
            0xD94 => self.scb.mpu_ctrl,
            0xD98 => self.scb.mpu_rnr,
            0xD9C | 0xDA4 | 0xDAC | 0xDB4 => {
                let r = (self.scb.mpu_rnr & 7) as usize;
                self.scb.mpu_rbar[r] | r as u32
            }
            0xDA0 | 0xDA8 | 0xDB0 | 0xDB8 => self.scb.mpu_rasr[(self.scb.mpu_rnr & 7) as usize],
            0xDF0 => (self.in_sleep as u32) << 18,
            0xDFC => self.scb.demcr,
            0xF34 => {
                if self.privileged() {
                    self.scb.fpccr()
                } else {
                    0
                }
            }
            0xF38 => {
                if self.privileged() {
                    self.scb.fpcar
                } else {
                    0
                }
            }
            0xF3C => {
                if self.privileged() {
                    self.scb.fpdscr
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    fn icsr_read(&self) -> u32 {
        let vectactive = self.nvic.current_exception() as u32 & 0x1FF;
        // Renode: RETTOBASE is set while at most one *system* exception is active.
        let sys_active = self.nvic.active_stack().iter().filter(|&&e| matches!(e, 1..=7 | 11 | 12 | 14 | 15)).count();
        let rettobase = (sys_active <= 1) as u32;
        let vectpending = self.nvic.peek_pending().unwrap_or(0) as u32 & 0x1FF;
        vectactive
            | (rettobase << 11)
            | (vectpending << 12)
            | ((self.nvic.is_pending(EXC_SYSTICK) as u32) << 26)
            | ((self.nvic.is_pending(EXC_PENDSV) as u32) << 28)
            | ((self.nvic.is_pending(EXC_NMI) as u32) << 31)
    }

    fn shcsr_read(&self) -> u32 {
        let n = &self.nvic;
        (n.is_active(EXC_MEMMANAGE) as u32)
            | ((n.is_active(EXC_BUSFAULT) as u32) << 1)
            | ((n.is_active(EXC_USAGEFAULT) as u32) << 3)
            | ((n.is_active(EXC_SVCALL) as u32) << 7)
            | ((n.is_active(EXC_PENDSV) as u32) << 10)
            | ((n.is_active(EXC_SYSTICK) as u32) << 11)
            | ((n.is_pending(EXC_USAGEFAULT) as u32) << 12)
            | ((n.is_pending(EXC_MEMMANAGE) as u32) << 13)
            | ((n.is_pending(EXC_BUSFAULT) as u32) << 14)
            | ((n.is_pending(EXC_SVCALL) as u32) << 15)
            | ((n.fault_enable[0] as u32) << 16)
            | ((n.fault_enable[1] as u32) << 17)
            | ((n.fault_enable[2] as u32) << 18)
    }

    fn dwt_read(&self, off: u32, now: Time) -> u32 {
        match off {
            0x000 => self.dwt.enabled() as u32,
            0x004 => self.cyccnt_at(now),
            0xFD0 => 0x04,
            0xFE0 => 0x02,
            0xFE4 => 0xB0,
            0xFE8 => 0x1B,
            0xFF0 => 0x0D,
            0xFF4 => 0xE0,
            0xFF8 => 0x05,
            0xFFC => 0xB1,
            _ => 0,
        }
    }

    // ---- writes ---------------------------------------------------------------------------

    /// Sub-word PPB write (`shift` is the bit position of the lane inside the aligned word).
    pub(crate) fn ppb_write_partial(&mut self, aligned: u32, shift: u32, size: u32, value: u32) {
        let mask = if size >= 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
        let lane = (value & mask) << shift;
        let write_one_to_act = matches!(aligned, 0xE000_E100..=0xE000_E13F | 0xE000_E180..=0xE000_E1BF | 0xE000_E200..=0xE000_E23F | 0xE000_E280..=0xE000_E2BF);
        if write_one_to_act {
            // ISER/ICER/ISPR/ICPR are write-one-to-act: only the addressed lane acts.
            self.ppb_write_word(aligned, lane);
            return;
        }
        // The hidden read of Renode's byte -> dword translation is a plain register read.
        let old = self.ppb_peek_word(aligned, self.clock_time);
        let merged = (old & !(mask << shift)) | lane;
        self.ppb_write_word(aligned, merged);
    }

    /// Aligned 32-bit PPB write. Timer registers act at the machine clock time (Renode's clock
    /// source has not seen the instructions of the current chunk yet).
    pub(crate) fn ppb_write_word(&mut self, a: u32, v: u32) {
        match a {
            0xE000_E000..=0xE000_EFFF => self.scs_write(a - SCS, v),
            0xE000_1000..=0xE000_1FFF => match a - DWT {
                0x000 => {
                    // The counter is advanced lazily (it never raises an event): catch up to the machine clock first.
                    self.dwt.advance_to(self.clock_time);
                    self.dwt.set_enabled(v & 1 != 0);
                    self.limit_timer_touched();
                }
                0x004 => {
                    self.dwt.advance_to(self.clock_time);
                    self.dwt.set_cyccnt(v);
                    self.limit_timer_touched();
                }
                _ => {}
            },
            _ => self.warn_once(a, || format!("PPB write to unmodeled address 0x{a:08x} ignored")),
        }
    }

    fn scs_write(&mut self, off: u32, v: u32) {
        match off {
            0x010 => {
                // Register fields run in order: ENABLE first (an immediate expiry still sees the
                // old TICKINT), then TICKINT.
                let fx = self.systick.set_enable(v & 1 != 0);
                self.systick.tickint = v & 2 != 0;
                self.apply_timer_effects(fx);
            }
            0x014 => {
                let fx = self.systick.set_reload(v & 0x00FF_FFFF);
                self.apply_timer_effects(fx);
            }
            0x018 => {
                let fx = self.systick.write_value();
                self.apply_timer_effects(fx);
            }
            0x100..=0x13C | 0x180..=0x1BC | 0x200..=0x23C | 0x280..=0x2BC => {
                let (base, kind) = match off {
                    0x100..=0x13C => (0x100, 0),
                    0x180..=0x1BC => (0x180, 1),
                    0x200..=0x23C => (0x200, 2),
                    _ => (0x280, 3),
                };
                let k = ((off - base) >> 2) as usize;
                let n = self.nvic.num_irqs() as usize;
                for bit in 0..32usize {
                    if v & (1 << bit) == 0 {
                        continue;
                    }
                    let irq = k * 32 + bit;
                    if irq >= n {
                        continue;
                    }
                    let exc = EXC_IRQ0 + irq;
                    match kind {
                        0 => self.nvic.set_enabled(exc, true),
                        1 => self.nvic.set_enabled(exc, false),
                        2 => self.nvic.set_pending(exc),
                        _ => self.nvic.clear_pending(exc),
                    }
                }
                self.nvic_changed();
            }
            0x300..=0x31C => {} // IABR: read only
            0x400..=0x7EF => {
                let first = (off - 0x400) as usize;
                for j in 0..4 {
                    let exc = EXC_IRQ0 + first + j;
                    if first + j < self.nvic.num_irqs() as usize {
                        let prio = (v >> (8 * j)) as u8;
                        if prio & !self.nvic.priority_mask() != 0 {
                            self.warn_once(0xE000_E400 + (first + j) as u32, || format!("priority of IRQ {} set to 0x{:02x}: only mask 0x{:02x} is implemented", first + j, prio, 0xF0u8));
                        }
                        self.nvic.set_priority(exc, prio);
                    }
                }
                self.nvic_changed();
            }
            0xD04 => {
                // Clears act before sets (Renode register field order).
                if v & (1 << 25) != 0 {
                    self.nvic.clear_pending(EXC_SYSTICK);
                }
                if v & (1 << 26) != 0 {
                    self.nvic.set_pending(EXC_SYSTICK);
                }
                if v & (1 << 27) != 0 {
                    self.nvic.clear_pending(EXC_PENDSV);
                }
                if v & (1 << 28) != 0 {
                    self.nvic.set_pending(EXC_PENDSV);
                }
                if v & (1 << 30) != 0 {
                    self.nvic.clear_pending(EXC_NMI);
                }
                if v & (1 << 31) != 0 {
                    self.nvic.set_pending(EXC_NMI);
                }
                self.nvic_changed();
            }
            0xD08 => self.scb.vtor = v & 0xFFFF_FF80,
            0xD0C => {
                if v >> 16 != 0x05FA {
                    return;
                }
                self.nvic.prigroup = (v >> 8) & 7;
                if v & 4 != 0 {
                    self.reset_requested = true;
                    self.kick();
                }
                self.nvic_changed();
            }
            0xD10 => {
                self.scb.scr = v & 0x16;
                self.nvic.sevonpend = v & 0x10 != 0;
                self.sleep_on_exit = v & 2 != 0;
            }
            0xD14 => {
                let mut ccr = v & 0x31B;
                if self.filter_ccr_div0 {
                    // Renode parity: DIV_0_TRP writes are blocked so the core never faults on division by zero.
                    ccr &= !CCR_DIV_0_TRP;
                }
                self.scb.ccr = ccr;
            }
            0xD18 => {
                self.nvic.set_priority(4, v as u8);
                self.nvic.set_priority(5, (v >> 8) as u8);
                self.nvic.set_priority(6, (v >> 16) as u8);
                self.nvic.set_priority(7, (v >> 24) as u8);
                self.nvic_changed();
            }
            0xD1C => {
                self.nvic.set_priority(11, (v >> 24) as u8);
                self.nvic_changed();
            }
            0xD20 => {
                self.nvic.set_priority(14, (v >> 16) as u8);
                self.nvic.set_priority(15, (v >> 24) as u8);
                self.nvic_changed();
            }
            0xD24 => self.shcsr_write(v),
            0xD28 => self.scb.cfsr &= !v,
            0xD2C => {
                if v & 2 != 0 {
                    self.nvic.hardfault_vecttbl = false;
                }
                if v & (1 << 30) != 0 {
                    self.nvic.hardfault_forced = false;
                }
            }
            0xD88 => self.scb.cpacr = v & 0x00F0_0000,
            0xD94 => {
                let en_before = self.scb.mpu_ctrl & 1 != 0;
                // Renode parity: MPU_CTRL is stored as written (only ENABLE is acted upon).
                self.scb.mpu_ctrl = v;
                if !en_before && v & 1 != 0 {
                    self.warn_once(0xE000_ED94, || "MPU enabled by firmware: region permissions are stored but not enforced".to_string());
                }
            }
            0xD98 => self.scb.mpu_rnr = v & 7,
            0xD9C | 0xDA4 | 0xDAC | 0xDB4 => {
                if v & 0x10 != 0 {
                    self.scb.mpu_rnr = v & 7;
                }
                let r = (self.scb.mpu_rnr & 7) as usize;
                self.scb.mpu_rbar[r] = v & 0xFFFF_FFE0;
            }
            0xDA0 | 0xDA8 | 0xDB0 | 0xDB8 => {
                let r = (self.scb.mpu_rnr & 7) as usize;
                self.scb.mpu_rasr[r] = v & MPU_RASR_MASK;
            }
            0xDFC => self.scb.demcr = v & (1 << 24),
            0xF00 => {
                let n = v & 0x1FF;
                if n < self.nvic.num_irqs() {
                    self.nvic.set_pending_irq(EXC_IRQ0 + n as usize);
                    self.nvic_changed();
                }
            }
            0xF34 => {
                if self.privileged() {
                    // Renode stores every writable bit as written (the readiness bits too).
                    self.scb.set_fpccr(v);
                }
            }
            0xF38 => {
                if self.privileged() {
                    self.scb.fpcar = v & !7;
                }
            }
            0xF3C => {
                if self.privileged() {
                    self.scb.fpdscr = v & 0x07C0_0000;
                }
            }
            _ => {}
        }
    }

    fn shcsr_write(&mut self, v: u32) {
        let n = &mut self.nvic;
        // Active bits force the Active flag.
        n.force_active(EXC_MEMMANAGE, v & 1 != 0);
        n.force_active(EXC_BUSFAULT, v & (1 << 1) != 0);
        n.force_active(EXC_USAGEFAULT, v & (1 << 3) != 0);
        n.force_active(EXC_SVCALL, v & (1 << 7) != 0);
        n.force_active(EXC_PENDSV, v & (1 << 10) != 0);
        n.force_active(EXC_SYSTICK, v & (1 << 11) != 0);
        // Pended bits pend or clear.
        for (bit, exc) in [(12, EXC_USAGEFAULT), (13, EXC_MEMMANAGE), (14, EXC_BUSFAULT), (15, EXC_SVCALL)] {
            if v & (1 << bit) != 0 {
                n.set_pending(exc);
            } else {
                n.clear_pending(exc);
            }
        }
        n.fault_enable[0] = v & (1 << 16) != 0;
        n.fault_enable[1] = v & (1 << 17) != 0;
        n.fault_enable[2] = v & (1 << 18) != 0;
        self.nvic_changed();
    }
}
