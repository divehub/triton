//! Exception model tests: entry/return and frame layout, stack alignment, EXC_RETURN values,
//! preemption / tail-chaining / PRIGROUP / BASEPRI / PRIMASK / FAULTMASK, NMI, PendSV, fault
//! escalation and lockup, lazy floating point stacking, IT state across exceptions.
//! Programs and handlers are assembler-verified snippets (`tests/cases/snippets.snip`).

mod common;
#[path = "generated/snippets.rs"]
mod snippets;

use armv7m::{ExitReason, TraceEntry};
use common::*;
use snippets::*;

const MAIN: u32 = 0x0800_4000;
const HANDLER_BASE: u32 = 0x0800_4400;
const LOG_PTR: u32 = 0x2000_0300;
const LOG_BASE: u32 = 0x2000_0310;
const CAP: u32 = 0x2000_0400;
const COUNTER_A: u32 = 0x2000_0100;
const TOP_OF_STACK: u32 = SRAM1_BASE + SRAM1_SIZE as u32; // 0x20018000

const ICSR: u32 = 0xE000_ED04;
const AIRCR: u32 = 0xE000_ED0C;
const SHCSR: u32 = 0xE000_ED24;
const CFSR: u32 = 0xE000_ED28;
const HFSR: u32 = 0xE000_ED2C;
const ISER0: u32 = 0xE000_E100;
const ISPR0: u32 = 0xE000_E200;
const IPR0: u32 = 0xE000_E400;
const SHPR3: u32 = 0xE000_ED20;

/// Exception numbers.
const NMI: u32 = 2;
const HARDFAULT: u32 = 3;
const USAGEFAULT: u32 = 6;
const SVC: u32 = 11;
const PENDSV: u32 = 14;
const SYSTICK: u32 = 15;
fn irq(n: u32) -> u32 {
    16 + n
}

struct T {
    h: Harness,
    next_slot: u32,
}

impl T {
    fn new() -> T {
        let mut t = T { h: Harness::new(), next_slot: 0 };
        t.h.bus.poke32(LOG_PTR, LOG_BASE);
        t
    }

    fn main(&mut self, code: &[u16]) {
        self.h.load(MAIN, code);
    }

    /// Installs a handler for exception `exc`; returns its address.
    fn handler(&mut self, exc: u32, code: &[u16]) -> u32 {
        let addr = HANDLER_BASE + 0x100 * self.next_slot;
        self.next_slot += 1;
        self.h.bus.load_halfwords(addr, code);
        self.h.bus.poke32(FLASH_BASE + 4 * exc, addr | 1);
        addr
    }

    fn poke(&mut self, addr: u32, v: u32) {
        self.h.cpu.ppb_poke32(addr, v, self.h.now);
    }

    fn peek(&self, addr: u32) -> u32 {
        self.h.cpu.ppb_peek32(addr, self.h.now).unwrap()
    }

    fn enable_irq(&mut self, n: u32) {
        self.poke(ISER0 + 4 * (n / 32), 1 << (n % 32));
    }

    fn set_irq_priority(&mut self, n: u32, prio: u8) {
        let a = IPR0 + (n & !3);
        let shift = (n & 3) * 8;
        let w = (self.peek(a) & !(0xFF << shift)) | ((prio as u32) << shift);
        self.poke(a, w);
    }

    fn set_sys_priority(&mut self, exc: u32, prio: u8) {
        // SHPR1..3 cover exceptions 4..15; only the bytes Renode maps are writable.
        let a = 0xE000_ED18 + ((exc - 4) & !3);
        let shift = ((exc - 4) & 3) * 8;
        let w = (self.peek(a) & !(0xFF << shift)) | ((prio as u32) << shift);
        self.poke(a, w);
    }

    /// Pulses an external IRQ line (pending stays set after the line drops).
    fn pulse(&mut self, n: u32) {
        self.h.cpu.set_irq_line(n, true);
        self.h.cpu.set_irq_line(n, false);
    }

    fn log(&self) -> Vec<u8> {
        let end = self.h.bus.peek32(LOG_PTR);
        (LOG_BASE..end).map(|a| self.h.bus.peek8(a)).collect()
    }

    fn frame_words(&self, sp: u32, n: u32) -> Vec<u32> {
        (0..n).map(|i| self.h.bus.peek32(sp + 4 * i)).collect()
    }
}

// --------------------------------------------------------------------------------------------------
// SVC, entry, frame layout, return

#[test]
fn svc_entry_frame_and_return() {
    let mut t = T::new();
    t.main(SVC5_PROG);
    t.handler(SVC, H_SVC_NUMBER);
    t.h.set(1, 0x1111);
    t.h.step(2); // movs r0, #1 ; svc #5: `svc` ends its translation block, which is also the chunk end
    assert_eq!(t.h.cpu.ipsr(), SVC, "the SVCall is taken at the block boundary before `run` returns");
    assert_eq!(t.h.cpu.instructions(), 2, "entry costs no instruction");
    t.h.step(1); // first handler instruction
    assert_eq!(t.h.r(14), 0xFFFF_FFF9, "EXC_RETURN: thread mode, MSP, basic frame");
    assert_eq!(t.h.r(13), TOP_OF_STACK - 0x20);
    let sp = TOP_OF_STACK - 0x20;
    let f = t.frame_words(sp, 8);
    assert_eq!(f[0], 1, "stacked r0");
    assert_eq!(f[1], 0x1111, "stacked r1");
    assert_eq!(f[6], MAIN + 4, "return address is the instruction after SVC");
    assert_eq!(f[7], 0x0100_0000, "stacked xPSR: Thumb bit only");
    t.h.step(20);
    assert_eq!(t.h.cpu.ipsr(), 0);
    assert_eq!(t.h.r(13), TOP_OF_STACK);
    assert_eq!(t.h.r(0), 1);
    assert_eq!(t.h.r(2), 2, "execution continues after the SVC");
    assert_eq!(t.h.bus.peek32(CAP + 16), 5, "SVC number fetched through the stacked PC");
}

#[test]
fn exceptions_cost_no_instructions() {
    let mut t = T::new();
    t.main(SVC5_PROG);
    t.handler(SVC, H_SVC_NUMBER);
    // movs, svc, 6 handler instructions, movs r2 = 9 retired instructions in total.
    let exit = t.h.step(9);
    assert_eq!(exit.executed, 9);
    assert_eq!(t.h.r(2), 2);
    assert_eq!(t.h.cpu.instructions(), 9);
}

#[test]
fn irq_entry_resume_and_return() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    t.h.step(100);
    assert_eq!(t.h.r(0), 50);
    t.pulse(5);
    t.h.step(100);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
    // 100 instructions: the handler used 5, the loop the rest, nothing was skipped or repeated.
    assert_eq!(t.h.r(0), 50 + 48);
    assert_eq!(t.h.cpu.ipsr(), 0);
    assert_eq!(t.h.r(13), TOP_OF_STACK);
}

#[test]
fn irq_not_taken_when_disabled_or_never_pending() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_COUNT_A);
    t.pulse(5); // pending but not enabled
    t.h.step(50);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 0);
    assert_eq!(t.peek(ISPR0) & 0x20, 0x20, "ISPR shows the pending bit");
    t.enable_irq(5);
    t.h.step(50);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1, "enabling the pending interrupt takes it");
    assert_eq!(t.peek(ISPR0) & 0x20, 0, "pending cleared on entry");
}

#[test]
fn level_sensitive_line_retriggers_until_dropped() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    t.h.cpu.set_irq_line(5, true);
    t.h.step(100); // handler returns while the line is still high -> pends again
    let n = t.h.bus.peek32(COUNTER_A);
    assert!(n >= 10, "level-sensitive line re-pends after every return, got {n}");
    t.h.cpu.set_irq_line(5, false);
    t.h.step(100);
    let after = t.h.bus.peek32(COUNTER_A);
    t.h.step(100);
    assert_eq!(t.h.bus.peek32(COUNTER_A), after, "stops once the line is low (at most one more entry)");
}

#[test]
fn stack_alignment_pads_odd_word_sp() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_CAPTURE);
    t.enable_irq(5);
    t.h.cpu.set_sp(TOP_OF_STACK - 4);
    t.h.step(10);
    t.pulse(5);
    t.h.step(1);
    // 8-byte alignment: the pad word sits above the frame, xPSR bit 9 records it.
    let frame = TOP_OF_STACK - 4 - 4 - 0x20;
    assert_eq!(t.h.r(13), frame);
    assert_eq!(t.h.bus.peek32(frame + 0x1C) & 0x200, 0x200);
    t.h.step(30);
    assert_eq!(t.h.r(13), TOP_OF_STACK - 4, "SP restored including the pad word");
    // Aligned SP: no padding, bit 9 clear.
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_CAPTURE);
    t.enable_irq(5);
    t.h.step(10);
    t.pulse(5);
    t.h.step(1);
    assert_eq!(t.h.bus.peek32(TOP_OF_STACK - 0x20 + 0x1C) & 0x200, 0);
}

#[test]
fn stkalign_parity_switch() {
    // Default (Renode parity): always aligned. With parity off, CCR.STKALIGN decides.
    for (parity, ccr, padded) in [(true, 0u32, true), (false, 0, false), (false, 0x200, true)] {
        let mut t = T::new();
        t.h.cpu.set_renode_stkalign_parity(parity);
        t.poke(0xE000_ED14, ccr);
        t.main(COUNT_LOOP);
        t.handler(irq(5), H_CAPTURE);
        t.enable_irq(5);
        t.h.cpu.set_sp(TOP_OF_STACK - 4);
        t.h.step(10);
        t.pulse(5);
        t.h.step(1);
        let expect_sp = if padded { TOP_OF_STACK - 8 - 0x20 } else { TOP_OF_STACK - 4 - 0x20 };
        assert_eq!(t.h.r(13), expect_sp, "parity={parity} ccr={ccr:#x}");
        t.h.step(30);
        assert_eq!(t.h.r(13), TOP_OF_STACK - 4);
    }
}

#[test]
fn exc_return_values_and_psp_frames() {
    // Thread mode on PSP: LR = 0xFFFFFFFD, frame on PSP, handler on MSP.
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_CAPTURE);
    t.enable_irq(5);
    t.h.cpu.set_control(2); // SPSEL
    t.h.cpu.set_sp(TOP_OF_STACK); // MSP
    t.h.set(13, 0x2000_8000); // PSP is the active SP now
    t.h.step(10);
    t.pulse(5);
    t.h.step(1);
    assert_eq!(t.h.r(14), 0xFFFF_FFFD);
    assert_eq!(t.h.cpu.psp(), 0x2000_8000 - 0x20, "frame pushed on PSP");
    assert_eq!(t.h.r(13), TOP_OF_STACK, "handler runs on MSP (no frame there)");
    t.h.step(30);
    assert_eq!(t.h.cpu.control() & 2, 2, "SPSEL restored by EXC_RETURN");
    assert_eq!(t.h.r(13), 0x2000_8000);
    assert_eq!(t.h.cpu.ipsr(), 0);
    assert_eq!(t.h.bus.peek32(CAP + 4), 0xFFFF_FFFD);
}

#[test]
fn nested_exception_lr_is_handler_mode() {
    let mut t = T::new();
    t.main(SPIN);
    t.handler(irq(5), H_A_PENDS_IRQ6_ISB);
    t.handler(irq(6), H_CAPTURE);
    t.enable_irq(5);
    t.enable_irq(6);
    t.set_irq_priority(5, 0x40);
    t.set_irq_priority(6, 0x20);
    t.pulse(5);
    t.h.step(40);
    // IRQ6 preempted the handler of IRQ5: its EXC_RETURN is "return to handler mode, MSP".
    assert_eq!(t.h.bus.peek32(CAP + 4), 0xFFFF_FFF1);
    assert_eq!(t.h.bus.peek32(CAP + 8), irq(6));
    assert_eq!(t.log(), vec![1, 2], "handler A ran to completion after the preemption");
    assert_eq!(t.h.cpu.ipsr(), 0);
    assert_eq!(t.h.r(13), TOP_OF_STACK);
}

// --------------------------------------------------------------------------------------------------
// Priorities: preemption, tail-chaining, PRIGROUP, masking

fn nested_log(prio_a: u8, prio_b: u8, prigroup: u32) -> Vec<u8> {
    let mut t = T::new();
    t.main(SPIN);
    // `isb` after the ISPR store ends the translation block, so the pended IRQ can preempt.
    t.handler(irq(5), H_A_PENDS_IRQ6_ISB);
    t.handler(irq(6), H_LOG_3);
    t.enable_irq(5);
    t.enable_irq(6);
    t.set_irq_priority(5, prio_a);
    t.set_irq_priority(6, prio_b);
    t.poke(AIRCR, 0x05FA_0000 | (prigroup << 8));
    t.pulse(5);
    t.h.step(60);
    assert_eq!(t.h.cpu.ipsr(), 0);
    t.log()
}

#[test]
fn higher_priority_preempts_handler() {
    assert_eq!(nested_log(0x40, 0x20, 0), vec![1, 3, 2]);
}

#[test]
fn equal_or_lower_priority_does_not_preempt() {
    assert_eq!(nested_log(0x40, 0x40, 0), vec![1, 2, 3]);
    assert_eq!(nested_log(0x40, 0x80, 0), vec![1, 2, 3]);
}

#[test]
fn prigroup_decides_preemption_between_sub_priorities() {
    // PRIGROUP 3: all four implemented bits are group priority -> 0x40 preempts 0x50.
    assert_eq!(nested_log(0x50, 0x40, 3), vec![1, 3, 2]);
    // PRIGROUP 4: bit 4 becomes sub-priority; 0x40 and 0x50 share a group -> no preemption.
    assert_eq!(nested_log(0x50, 0x40, 4), vec![1, 2, 3]);
    // PRIGROUP 7: nothing preempts anything (all sub-priority).
    assert_eq!(nested_log(0x80, 0x10, 7), vec![1, 2, 3]);
}

#[test]
fn simultaneous_pending_is_ordered_by_priority_then_number() {
    let mut t = T::new();
    t.main(SPIN);
    t.handler(irq(5), H_LOG_1);
    t.handler(irq(6), H_LOG_2);
    t.handler(irq(7), H_LOG_3);
    for n in [5, 6, 7] {
        t.enable_irq(n);
    }
    t.set_irq_priority(5, 0x60);
    t.set_irq_priority(6, 0x40);
    t.set_irq_priority(7, 0x40);
    t.pulse(5);
    t.pulse(6);
    t.pulse(7);
    t.h.step(60);
    assert_eq!(t.log(), vec![2, 3, 1], "0x40 before 0x60; equal priority: lower IRQ number first");
}

#[test]
fn tail_chaining_runs_back_to_back_without_thread_code() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    let ha = t.handler(irq(5), H_LOG_1);
    let hb = t.handler(irq(6), H_LOG_2);
    t.enable_irq(5);
    t.enable_irq(6);
    t.h.step(10);
    t.h.cpu.trace_to_buffer(100, false);
    t.pulse(5);
    t.pulse(6);
    let r0_before = t.h.r(0);
    t.h.step(30);
    let trace: Vec<TraceEntry> = t.h.cpu.trace_take();
    let pcs: Vec<u32> = trace.iter().map(|e| e.pc).collect();
    // Handler A (6 instructions), then handler B (6 instructions) with no thread instruction between.
    assert_eq!(pcs[0] & !1, ha, "first executed instruction belongs to handler A");
    let idx_b = pcs.iter().position(|&p| p == hb).expect("handler B executed");
    assert!(pcs[..idx_b].iter().all(|&p| p >= HANDLER_BASE), "no thread-mode instruction between the handlers: {pcs:x?}");
    assert_eq!(idx_b, 6);
    assert_eq!(t.log(), vec![1, 2]);
    assert_eq!(t.h.r(13), TOP_OF_STACK);
    assert!(t.h.r(0) > r0_before);
}

#[test]
fn basepri_masks_equal_and_lower_priorities() {
    let mut t = T::new();
    // msr basepri, r0 ; nop x3 ; msr basepri, r1 ; nop ; nop ; b .
    t.main(&[0xF380, 0x8811, 0xBF00, 0xBF00, 0xBF00, 0xF381, 0x8811, 0xBF00, 0xBF00, 0xE7FE]);
    t.handler(irq(5), H_LOG_1);
    t.handler(irq(6), H_LOG_2);
    for n in [5, 6] {
        t.enable_irq(n);
    }
    t.set_irq_priority(5, 0x40);
    t.set_irq_priority(6, 0x30);
    t.h.set(0, 0x40);
    t.h.set(1, 0);
    t.h.step(1); // msr basepri, r0
    t.pulse(5);
    t.pulse(6);
    t.h.step(6 + 3); // handler of IRQ6 (6 instructions) and the three nops
    assert_eq!(t.log(), vec![2], "only the priority above BASEPRI is taken");
    assert_eq!(t.peek(ISPR0) & 0x20, 0x20, "IRQ5 stays pending");
    assert_eq!(t.h.cpu.basepri(), 0x40);
    t.h.step(1); // msr basepri, r1: BASEPRI = 0 unmasks IRQ5, taken at the block end
    assert_eq!(t.h.cpu.ipsr(), irq(5));
    t.h.step(30);
    assert_eq!(t.log(), vec![2, 1], "BASEPRI = 0 disables masking");
}

#[test]
fn primask_defers_until_cpsie_and_is_taken_before_next_instruction() {
    let mut t = T::new();
    t.main(MASK_THEN_UNMASK);
    t.handler(irq(5), H_LOG_1);
    t.enable_irq(5);
    t.pulse(5); // pending before the program even starts
    t.h.step(3); // the interrupt is taken at the first block boundary, before `cpsid i`
    assert_eq!(t.log(), vec![], "handler body has not completed yet");
    assert_eq!(t.h.cpu.ipsr(), irq(5));
    let mut t = T::new();
    t.main(MASK_THEN_UNMASK);
    t.handler(irq(5), H_LOG_1);
    t.enable_irq(5);
    t.h.step(1); // cpsid i
    t.pulse(5);
    t.h.step(2); // movs r0,#1 ; movs r0,#2
    assert_eq!(t.log(), vec![], "masked while PRIMASK is set");
    assert_eq!(t.h.cpu.ipsr(), 0);
    t.h.step(1); // cpsie i ends its block: the pending interrupt is taken before `run` returns
    assert_eq!(t.h.cpu.ipsr(), irq(5), "taken before `movs r0, #3`");
    assert_eq!(t.h.bus.peek32(TOP_OF_STACK - 0x20 + 0x18), MAIN + 8, "stacked PC = the instruction after cpsie");
    assert_eq!(t.h.bus.peek32(TOP_OF_STACK - 0x20), 2, "stacked r0 = 2: movs r0,#3 not yet executed");
    t.h.step(30);
    assert_eq!(t.log(), vec![1]);
    assert_eq!(t.h.r(0), 4);
}

#[test]
fn nmi_ignores_primask_and_faultmask() {
    for mask in [0xB672u16 /* cpsid i */, 0xB671 /* cpsid f */] {
        let mut t = T::new();
        t.main(&[mask, 0xE7FE]);
        t.handler(NMI, H_LOG_4);
        t.handler(irq(5), H_LOG_1);
        t.enable_irq(5);
        t.h.step(2);
        t.pulse(5);
        t.poke(ICSR, 1 << 31); // PENDNMISET
        t.h.step(30);
        assert_eq!(t.log(), vec![4], "NMI taken, the maskable IRQ stays pending (mask {mask:#x})");
    }
}

#[test]
fn pendsv_is_pended_through_icsr_and_taken() {
    let mut t = T::new();
    t.main(SPIN);
    t.handler(PENDSV, H_LOG_5);
    t.poke(ICSR, 1 << 28);
    assert_ne!(t.peek(ICSR) & (1 << 28), 0, "PENDSVSET reads back while pending");
    assert_eq!((t.peek(ICSR) >> 12) & 0x1FF, PENDSV, "VECTPENDING");
    t.h.step(30);
    assert_eq!(t.log(), vec![5]);
    assert_eq!(t.peek(ICSR) & (1 << 28), 0, "pending cleared on entry");
    // PENDSVCLR cancels a pending PendSV.
    t.poke(ICSR, 1 << 28);
    t.poke(ICSR, 1 << 27);
    t.h.step(30);
    assert_eq!(t.log(), vec![5]);
}

#[test]
fn systick_and_pendsv_priorities_via_shpr() {
    // PendSV at the lowest priority waits for the SysTick handler it was pended from.
    let mut t = T::new();
    t.main(SPIN);
    t.handler(PENDSV, H_LOG_1);
    t.handler(SYSTICK, H_LOG_2);
    t.set_sys_priority(PENDSV, 0xF0);
    t.set_sys_priority(SYSTICK, 0x20);
    assert_eq!(t.peek(SHPR3) >> 16 & 0xFFFF, 0x20F0, "SHPR3 holds PendSV in byte 2 and SysTick in byte 3");
    t.poke(ICSR, (1 << 28) | (1 << 26));
    t.h.step(40);
    assert_eq!(t.log(), vec![2, 1], "higher priority SysTick first");
}

// --------------------------------------------------------------------------------------------------
// Faults, escalation, lockup

#[test]
fn undefined_instruction_escalates_to_hardfault_when_usagefault_disabled() {
    let mut t = T::new();
    t.main(UDF_PROG);
    t.handler(HARDFAULT, H_SKIP_FAULTING_16BIT);
    t.h.step(30);
    assert_eq!(t.h.r(0), 2, "execution resumed after the skipped UDF");
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
    assert_eq!(t.peek(CFSR) & 0x10000, 0x10000, "UFSR.UNDEFINSTR");
    assert_eq!(t.peek(HFSR) & 0x4000_0000, 0x4000_0000, "HFSR.FORCED");
    // The stacked return address is the faulting instruction.
    let f = t.frame_words(TOP_OF_STACK - 0x20, 8);
    assert_eq!(f[6], MAIN + 2 + 2, "handler advanced the stacked PC past the 2-byte UDF");
}

#[test]
fn usagefault_taken_when_enabled_and_status_is_write_one_to_clear() {
    let mut t = T::new();
    t.main(UDF_PROG);
    t.handler(USAGEFAULT, H_SKIP_FAULTING_16BIT);
    t.poke(SHCSR, 1 << 18); // USGFAULTENA
    t.h.step(30);
    assert_eq!(t.h.r(0), 2);
    assert_eq!(t.peek(HFSR), 0, "no escalation");
    assert_eq!(t.peek(CFSR), 0x10000);
    t.poke(CFSR, 0x10000);
    assert_eq!(t.peek(CFSR), 0, "write one to clear");
}

#[test]
fn fault_inside_hardfault_handler_locks_up() {
    let mut t = T::new();
    t.main(UDF_PROG);
    t.handler(HARDFAULT, H_FAULT_IN_HANDLER);
    let exit = t.h.step(50);
    assert_eq!(exit.reason, ExitReason::Lockup);
    assert!(t.h.cpu.is_locked_up());
    assert!(t.h.cpu.lockup_reason().is_some());
    // The core stays locked up.
    let exit = t.h.step(10);
    assert_eq!(exit.reason, ExitReason::Lockup);
    assert_eq!(exit.executed, 0);
}

#[test]
fn nmi_wakes_a_locked_up_core() {
    let mut t = T::new();
    t.main(UDF_PROG);
    t.handler(HARDFAULT, H_FAULT_IN_HANDLER);
    t.handler(NMI, H_LOG_4);
    assert_eq!(t.h.step(50).reason, ExitReason::Lockup);
    t.poke(ICSR, 1 << 31);
    let exit = t.h.step(20);
    assert_ne!(exit.reason, ExitReason::Lockup);
    assert_eq!(t.log(), vec![4]);
}

#[test]
fn invalid_exc_return_raises_invpc() {
    let mut t = T::new();
    t.main(SPIN);
    t.handler(irq(5), H_BAD_RETURN);
    t.handler(HARDFAULT, H_LOG_6);
    t.enable_irq(5);
    t.pulse(5);
    t.h.step(40);
    assert_eq!(t.peek(CFSR) & 0x40000, 0x40000, "UFSR.INVPC");
    assert_eq!(t.peek(HFSR) & 0x4000_0000, 0x4000_0000, "escalated (UsageFault disabled)");
    // The HardFault handler returns with the same invalid EXC_RETURN (LR is inherited), which
    // faults again: the log fills while steps remain, like it would on silicon.
    assert_eq!(t.log()[0], 6, "HardFault handler entered");
    assert_eq!(t.h.bus.peek32(CAP), 0);
}

#[test]
fn bx_to_even_address_is_invstate() {
    let mut t = T::new();
    t.main(&[0x4700, 0xE7FE]); // bx r0 ; b .
    t.handler(HARDFAULT, H_LOG_1);
    t.h.set(0, MAIN + 0x100); // bit 0 clear
    t.h.step(30);
    assert_eq!(t.peek(CFSR) & 0x20000, 0x20000, "UFSR.INVSTATE");
    // The handler returns into the same bad state, which faults again until the steps run out.
    assert_eq!(t.log()[0], 1);
    // The first fault stacked the branch target as its return address.
    let f = t.frame_words(TOP_OF_STACK - 0x20, 8);
    assert_eq!(f[6], MAIN + 0x100);
}

#[test]
fn vector_with_cleared_thumb_bit_raises_invstate() {
    let mut t = T::new();
    t.main(SVC5_PROG);
    let h = t.handler(SVC, H_LOG_1);
    t.h.bus.poke32(FLASH_BASE + 4 * SVC, h); // even handler address
    t.handler(HARDFAULT, H_LOG_2);
    t.h.step(30);
    assert_eq!(t.peek(CFSR) & 0x20000, 0x20000, "INVSTATE for the first handler instruction");
    // The HardFault handler ran (and, returning into the bad vector again, faults repeatedly).
    assert_eq!(t.log()[0], 2);
}

#[test]
fn exclusive_monitor_is_cleared_by_exception_entry() {
    let mut t = T::new();
    t.main(LDREX_PROG);
    t.handler(irq(5), H_LOG_1);
    t.enable_irq(5);
    t.h.bus.poke32(COUNTER_A, 0x55);
    t.h.step(3); // ldr r1 ; ldrex ; nop
    t.pulse(5);
    t.h.step(40);
    assert_eq!(t.h.r(2), 1, "strex fails after the intervening exception");
    assert_eq!(t.h.bus.peek32(COUNTER_A), 0x55);
    // Without the interrupt it succeeds.
    let mut t = T::new();
    t.main(LDREX_PROG);
    t.h.bus.poke32(COUNTER_A, 0x55);
    t.h.step(10);
    assert_eq!(t.h.r(2), 0);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 5);
}

#[test]
fn interrupt_inside_it_block_preserves_itstate() {
    // cmp r0, r0 ; itt eq ; moveq r1, #1 ; moveq r2, #2 ; b .
    let code = [0x4280u16, 0xBF04, 0x2101, 0x2202, 0xE7FE];
    let mut t = T::new();
    t.main(&code);
    t.handler(irq(5), H_CAPTURE);
    t.enable_irq(5);
    t.h.step(2); // cmp, itt
    t.pulse(5);
    t.h.step(1); // entry + first handler instruction
    let xpsr = t.h.bus.peek32(TOP_OF_STACK - 0x20 + 0x1C);
    assert_eq!(xpsr & 0x0600_FC00, 0x0000_0400 | 0, "ITSTATE 0x04 stacked in xPSR[15:10]/[26:25]");
    assert_eq!(t.h.cpu.itstate(), 0, "IT state is cleared while in the handler");
    t.h.step(30);
    assert_eq!(t.h.r(1), 1);
    assert_eq!(t.h.r(2), 2, "the IT block continued after the handler returned");
}
