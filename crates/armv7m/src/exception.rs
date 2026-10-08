//! Exception entry and return (ARMv7-M ARM B1.5) with Renode/tlib behaviour:
//! the exception frame is always 8-byte aligned (see `Cpu::stkalign_always`),
//! entry and return cost no instructions, tail-chaining is realised as
//! "return, then immediately take the pending exception" which is
//! architecturally indistinguishable.

use crate::cpu::*;
use crate::nvic::*;
use crate::CpuBus;

const RESERVED_FP_WORD: u32 = 0xBADC_AFEE;

impl Cpu {
    /// Takes the exception selected by the NVIC (called at an instruction boundary
    /// while the NVIC IRQ line is asserted).
    pub(crate) fn take_pending_exception<B: CpuBus>(&mut self, bus: &mut B) {
        if let Some(exc) = self.nvic.acknowledge() {
            let tail = self.pending_tailchain.take();
            self.enter_exception(bus, exc as u32, tail);
        }
    }

    /// Pushes the exception frame and enters the handler of `exc` (already acknowledged).
    /// `tailchain_lr` re-uses an existing frame (derived exception after an invalid return).
    pub(crate) fn enter_exception<B: CpuBus>(&mut self, bus: &mut B, exc: u32, tailchain_lr: Option<u32>) {
        let lr_value;
        if let Some(lr) = tailchain_lr {
            lr_value = lr;
        } else {
            // EXC_RETURN: 0xFFFFFFE1/E9/ED (extended frame) or F1/F9/FD.
            let mut lr = 0xFFFF_FFF1u32;
            if !self.handler_mode() {
                lr |= 0x8; // return to Thread mode
                if self.control & CONTROL_SPSEL != 0 {
                    lr |= 0x4; // PSP
                }
            }
            let fpca = self.control & CONTROL_FPCA != 0;
            if fpca {
                lr &= !0x10;
            }
            lr_value = lr;

            // ---- stack the frame -----------------------------------------------------------
            let mut xpsr = self.xpsr();
            let mut sp = self.r[13];
            if (self.stkalign_always || self.scb.ccr & CCR_STKALIGN != 0) && sp & 4 != 0 {
                sp = sp.wrapping_sub(4);
                xpsr |= 0x200;
            }
            let frame = if fpca { 0x68 } else { 0x20 };
            let base = sp.wrapping_sub(frame);
            let (r0, r1, r2, r3, r12, lr_now, ret) = (self.r[0], self.r[1], self.r[2], self.r[3], self.r[12], self.r[14], self.r[15]);
            self.st32(bus, base, r0);
            self.st32(bus, base.wrapping_add(0x04), r1);
            self.st32(bus, base.wrapping_add(0x08), r2);
            self.st32(bus, base.wrapping_add(0x0C), r3);
            self.st32(bus, base.wrapping_add(0x10), r12);
            self.st32(bus, base.wrapping_add(0x14), lr_now);
            self.st32(bus, base.wrapping_add(0x18), ret);
            self.st32(bus, base.wrapping_add(0x1C), xpsr);
            if fpca {
                if self.scb.fpccr_lspen() {
                    // Lazy preservation: reserve the space, remember where to save.
                    self.scb.fpcar = base.wrapping_add(0x20);
                    let ready = self.nvic.fpccr_ready_bits(exc as u16);
                    self.scb.fpccr_lazy_allocated(ready);
                } else {
                    for i in 0..16u32 {
                        let v = self.fp.s[i as usize];
                        self.st32(bus, base.wrapping_add(0x20 + 4 * i), v);
                    }
                    let fpscr = self.fp.fpscr;
                    self.st32(bus, base.wrapping_add(0x60), fpscr);
                    self.st32(bus, base.wrapping_add(0x64), RESERVED_FP_WORD);
                }
            }
            self.r[13] = base;
            // The new context starts without an active FP context.
            self.control &= !CONTROL_FPCA;
        }

        // ---- ExceptionTaken ------------------------------------------------------------------------
        self.ipsr = exc;
        self.update_sp_selection();
        self.itstate = 0;
        self.it_changed = true;
        self.exclusive = None;
        self.r[14] = lr_value;
        let vector = self.ld32(bus, self.scb.vtor.wrapping_add(4 * exc));
        self.r[15] = vector & !1;
        self.thumb = vector & 1 != 0;
        if !self.thumb {
            // First handler instruction would execute with EPSR.T clear.
            let t = self.r[15];
            self.usage_fault(CFSR_INVSTATE, t);
        }
        self.nvic_changed();
        self.kick();
    }

    /// Exception return: `excret` is the EXC_RETURN value loaded into PC in Handler mode.
    pub(crate) fn exception_return<B: CpuBus>(&mut self, bus: &mut B, excret: u32) {
        let exc = self.ipsr as usize;
        // FAULTMASK is cleared on return from every exception except NMI.
        if exc != EXC_NMI {
            self.nvic.faultmask = false;
        }
        let completed = self.nvic.complete(exc);
        if !completed {
            self.invalid_exception_return(excret);
            return;
        }

        let to_thread = excret & 0x8 != 0;
        let spsel = excret & 0x4 != 0;

        // ---- pop the frame -------------------------------------------------------------------------------
        let mut sp = if to_thread && spsel { self.psp() } else { self.msp() };
        let ld = |cpu: &mut Cpu, bus: &mut B, a: u32| cpu.ld32(bus, a);
        let r0 = ld(self, bus, sp);
        let r1 = ld(self, bus, sp.wrapping_add(0x04));
        let r2 = ld(self, bus, sp.wrapping_add(0x08));
        let r3 = ld(self, bus, sp.wrapping_add(0x0C));
        let r12 = ld(self, bus, sp.wrapping_add(0x10));
        let lr = ld(self, bus, sp.wrapping_add(0x14));
        let ret = ld(self, bus, sp.wrapping_add(0x18));
        let xpsr = ld(self, bus, sp.wrapping_add(0x1C));

        // Handler mode requires a non-zero stacked IPSR, Thread mode a zero one.
        let stacked_exception = xpsr & 0x1FF;
        if to_thread == (stacked_exception != 0) {
            // The frame stays in place for the derived exception.
            self.invalid_exception_return(excret);
            return;
        }

        // CONTROL.SPSEL follows EXC_RETURN bit 2 (zero when returning to Handler mode);
        // the target stack becomes the active one.
        self.control = (self.control & !CONTROL_SPSEL) | if spsel { CONTROL_SPSEL } else { 0 };
        self.select_sp(to_thread && spsel);
        self.r[0] = r0;
        self.r[1] = r1;
        self.r[2] = r2;
        self.r[3] = r3;
        self.r[12] = r12;
        self.r[14] = lr;
        self.r[15] = ret & !1;
        sp = sp.wrapping_add(0x20);

        let extended = excret & 0x10 == 0;
        if extended {
            if self.scb.fpccr_lspact() {
                // State was never preserved: simply discard the reserved space.
                self.scb.set_fpccr_lspact(false);
            } else {
                for i in 0..16u32 {
                    let v = self.ld32(bus, sp.wrapping_add(4 * i));
                    self.fp.s[i as usize] = v;
                }
                self.fp.fpscr = self.ld32(bus, sp.wrapping_add(0x40)) & crate::vfp::fpscr::WRITE_MASK;
            }
            sp = sp.wrapping_add(0x48);
            self.control |= CONTROL_FPCA;
        } else {
            self.control &= !CONTROL_FPCA;
        }
        if xpsr & 0x200 != 0 {
            sp |= 4;
        }
        self.r[13] = sp;

        // ---- restore xPSR ----------------------------------------------------------------------------------
        self.apsr = xpsr & 0xF800_0000;
        self.ge = (xpsr >> 16) & 0xF;
        self.itstate = (((xpsr >> 25) & 3) | (((xpsr >> 10) & 0x3F) << 2)) as u8;
        self.thumb = xpsr & (1 << 24) != 0;
        self.ipsr = stacked_exception;
        self.update_sp_selection();
        self.it_changed = true;
        self.exclusive = None;
        self.event_flag = true;
        if self.sleep_on_exit {
            // Renode parity (tlib `automatic_sleep_after_interrupt`): with SCR.SLEEPONEXIT set the
            // core goes to sleep after *every* exception return, not only the outermost one.
            self.wfi = true;
            self.exit_pending = true;
        }
        if !self.thumb {
            let t = self.r[15];
            self.usage_fault(CFSR_INVSTATE, t);
        }
        // Tail-chaining: whatever is pending now is taken at the next boundary.
        self.nvic_changed();
        self.kick();
    }

    /// INVPC: EXC_RETURN did not match the active exception state. The UsageFault is
    /// taken as a derived exception re-using the existing frame (EXC_RETURN stays in LR).
    fn invalid_exception_return(&mut self, excret: u32) {
        self.scb.cfsr |= CFSR_INVPC;
        self.r[15] = excret & !1;
        self.insn_faulted = true;
        self.pending_tailchain = Some(excret);
        self.raise_sync(EXC_USAGEFAULT);
    }
}
