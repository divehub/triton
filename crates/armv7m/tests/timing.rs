//! Renode/tlib timing behavior of the core that the REF micro vectors do not pin down:
//! translation-block boundaries (branch, page, chunk end), chunk rounding, stop requests, MMIO
//! notifications, WFI / WFE / sleep-on-exit, SysTick and DWT as clock entries (lagging writes,
//! exact-time reads, COUNTFLAG, the FreeRTOS initialization order), `advance_idle`, halting and
//! the exactness of idle-loop fast-forward. All times use the 1 ns base (10 ns per instruction).

mod common;
#[path = "generated/snippets.rs"]
mod snippets;

use armv7m::{Cpu, CpuBus, ExitReason, BUS_IRQ_CHANGED, BUS_STOP_REQUESTED};
use common::*;
use snippets::*;

const MAIN: u32 = 0x0800_4000;
const HANDLER_BASE: u32 = 0x0800_6000;
const TOP: u32 = SRAM1_BASE + SRAM1_SIZE as u32;
const COUNTER_A: u32 = 0x2000_0100;
const LOG_PTR: u32 = 0x2000_0300;
const LOG_BASE: u32 = 0x2000_0310;
const CAP: u32 = 0x2000_0400;

const SYST_CSR: u32 = 0xE000_E010;
const SYST_RVR: u32 = 0xE000_E014;
const SYST_CVR: u32 = 0xE000_E018;
const ICSR: u32 = 0xE000_ED04;
const SCR: u32 = 0xE000_ED10;
const ISER0: u32 = 0xE000_E100;
const ISPR0: u32 = 0xE000_E200;
const CPACR: u32 = 0xE000_ED88;
const FPCCR: u32 = 0xE000_EF34;
const TPI: u64 = 10;

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
        assert_eq!(emu_core::TICKS_PER_INSTRUCTION, TPI, "these tests assume the 1 ns time base");
        let mut t = T { h: Harness::new(), next_slot: 0 };
        t.h.bus.poke32(LOG_PTR, LOG_BASE);
        t
    }

    fn main(&mut self, code: &[u16]) {
        self.h.load(MAIN, code);
    }

    fn handler(&mut self, exc: u32, code: &[u16]) -> u32 {
        let addr = HANDLER_BASE + 0x100 * self.next_slot;
        self.next_slot += 1;
        self.h.bus.load_halfwords(addr, code);
        self.h.bus.poke32(FLASH_BASE + 4 * exc, addr | 1);
        addr
    }

    fn poke(&mut self, addr: u32, v: u32) {
        self.h.cpu.ppb_poke32(addr, v, self.h.cpu.clock_time().max(self.h.now));
    }

    fn peek(&self, addr: u32) -> u32 {
        self.h.cpu.ppb_peek32(addr, self.h.now).unwrap()
    }

    fn enable_irq(&mut self, n: u32) {
        self.poke(ISER0 + 4 * (n / 32), 1 << (n % 32));
    }

    fn pulse(&mut self, n: u32) {
        self.h.cpu.set_irq_line(n, true);
        self.h.cpu.set_irq_line(n, false);
    }

    fn log(&self) -> Vec<u8> {
        let end = self.h.bus.peek32(LOG_PTR);
        (LOG_BASE..end).map(|a| self.h.bus.peek8(a)).collect()
    }

    fn pcs(&mut self, cap: usize) {
        self.h.cpu.trace_pcs(cap);
    }

    fn take_pcs(&mut self) -> Vec<u32> {
        self.h.cpu.trace_take_pcs()
    }

    /// SysTick as configured by firmware: RELOAD, a CVR write, CSR.
    fn start_systick(&mut self, reload: u32, csr: u32) {
        self.poke(SYST_RVR, reload);
        self.poke(SYST_CVR, 0);
        self.poke(SYST_CSR, csr);
    }
}

// ---------------------------------------------------------------------------------------------
// Translation-block boundaries

#[test]
fn a_pended_exception_is_taken_at_the_end_of_the_translation_block() {
    let mut t = T::new();
    t.main(PEND_PENDSV_NOPS_B);
    let isr = t.handler(PENDSV, H_LOG_5);
    t.pcs(64);
    t.h.step(24);
    let pcs = t.take_pcs();
    // ldr, movs, lsls, str (pends PendSV), nop, nop, b -> block end -> the handler.
    assert_eq!(&pcs[..8], &[MAIN, MAIN + 2, MAIN + 4, MAIN + 6, MAIN + 8, MAIN + 10, MAIN + 12, isr]);
    assert_eq!(pcs[7 + 6], MAIN + 14, "execution continues after the branch once the handler returned");
    assert_eq!(t.log(), vec![5]);
}

#[test]
fn a_1k_page_boundary_ends_the_translation_block() {
    for wide_straddle in [false, true] {
        let mut t = T::new();
        let base = 0x0800_53F0u32; // the page ends at 0x0800_5400
        let mut code = vec![0x602Cu16]; // str r4, [r5]
        if wide_straddle {
            code.extend(std::iter::repeat(0xBF00).take(6)); // nop x6: 0x...F2 .. 0x...FC
            code.extend([0xF240, 0x0001]); // movw r0, #1 at 0x080053FE, straddling the page end
        } else {
            code.extend(std::iter::repeat(0xBF00).take(7)); // nop x7 up to the boundary
        }
        code.extend(std::iter::repeat(0xBF00).take(8));
        code.push(0xE7FE);
        t.h.load(base, &code);
        let isr = t.handler(PENDSV, H_LOG_5);
        t.h.set(5, ICSR);
        t.h.set(4, 1 << 28);
        t.pcs(64);
        t.h.step(30);
        let pcs = t.take_pcs();
        let last_before = if wide_straddle { 0x0800_53FE } else { 0x0800_53FE };
        let i = pcs.iter().position(|&p| p == last_before).expect("block end instruction");
        assert_eq!(pcs[i + 1], isr, "wide={wide_straddle}: the handler follows the instruction that reaches the page end: {pcs:x?}");
        // One instruction before the boundary the exception has not been taken yet.
        assert_ne!(pcs[i], isr);
    }
}

#[test]
fn the_chunk_end_is_a_block_boundary_where_pending_exceptions_are_taken() {
    let mut t = T::new();
    t.main(PEND_PENDSV_NOPS_B);
    t.handler(PENDSV, H_LOG_5);
    let exit = t.h.step_once(4); // ldr, movs, lsls, str: the store pends PendSV in the middle of a block
    assert_eq!((exit.executed, exit.reason), (4, ExitReason::Deadline), "{exit:?}");
    assert_eq!(t.h.cpu.ipsr(), PENDSV, "taken before run returned; entry costs nothing");
    assert_eq!(t.h.cpu.instructions(), 4);
}

#[test]
fn a_board_stop_request_ends_the_chunk_at_the_end_of_the_block() {
    let mut t = T::new();
    t.main(STORE_NOPS_B);
    t.h.bus.write_trigger = Some((MMIO_BASE + 0x10, BUS_STOP_REQUESTED));
    t.h.set(0, MMIO_BASE + 0x10);
    t.h.set(1, 0x55);
    let exit = t.h.step_once(100);
    assert_eq!(exit.reason, ExitReason::StopRequested);
    assert_eq!(exit.executed, 4, "str, nop, nop and the branch that ends the block");
    assert_eq!(exit.now, 4 * TPI);
    // Without a new request the rest runs to the final `b .`, which sleeps.
    let exit = t.h.step_once(100);
    assert_eq!(exit.reason, ExitReason::Sleeping);
    assert_eq!(exit.executed, 3 + 1 + 1);
}

#[test]
fn a_stop_request_raised_while_nothing_ran_is_ignored() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.h.bus.notifications = BUS_STOP_REQUESTED;
    let exit = t.h.step_once(10);
    assert_eq!((exit.executed, exit.reason), (10, ExitReason::Deadline));
}

#[test]
fn an_irq_asserted_by_an_mmio_access_is_taken_at_the_block_end_and_ends_the_chunk() {
    let mut t = T::new();
    t.main(STORE_NOPS_B);
    t.handler(irq(5), H_LOG_1);
    t.enable_irq(5);
    t.h.bus.write_trigger = Some((MMIO_BASE + 0x20, BUS_IRQ_CHANGED));
    t.h.bus.trigger_irqs = vec![(5, true)];
    t.h.set(0, MMIO_BASE + 0x20);
    t.pcs(32);
    let exit = t.h.step_once(100);
    assert_eq!(exit.reason, ExitReason::StopRequested, "the rising IRQ line is tlib's exit request");
    assert_eq!(exit.executed, 4);
    assert_eq!(t.h.cpu.ipsr(), irq(5), "entered before run returned");
    let pcs = t.take_pcs();
    assert_eq!(pcs, vec![MAIN, MAIN + 2, MAIN + 4, MAIN + 6]);
}

#[test]
fn until_is_rounded_down_to_whole_instructions_with_a_minimum_of_one() {
    let mut h = Harness::new();
    h.load(MAIN, COUNT_LOOP);
    let e = h.cpu.run(&mut h.bus, 0, 25);
    assert_eq!((e.executed, e.now, e.reason), (2, 20, ExitReason::Deadline), "floor(25 / 10)");
    let e = h.cpu.run(&mut h.bus, 20, 29);
    assert_eq!((e.executed, e.now), (1, 30), "a remainder below one instruction still runs one");
    let e = h.cpu.run(&mut h.bus, 30, 30);
    assert_eq!((e.executed, e.now, e.reason), (0, 30, ExitReason::Deadline), "nothing to do");
    let e = h.cpu.run(&mut h.bus, 30, 10);
    assert_eq!((e.executed, e.now), (0, 30), "until in the past");
    let e = h.cpu.run(&mut h.bus, 30, 10_030);
    assert_eq!(e.executed, 1000);
    assert_eq!(h.cpu.instructions(), 2 + 1 + 1000);
}

#[test]
fn the_systick_deadline_bounds_the_chunk_like_any_clock_entry() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(SYSTICK, H_COUNT_A);
    t.start_systick(999, 7); // 999 cycles = 12 487.5 ns -> expiry at 12 488 ns
    assert_eq!(t.h.cpu.next_internal_deadline(), Some(12_488));
    // The board asks for a whole quantum; the core stops at the SysTick limit by itself.
    let e = t.h.cpu.run(&mut t.h.bus, 0, 100_000);
    assert_eq!((e.executed, e.now), (1248, 12_480), "floor(12 488 / 10)");
    let e = t.h.cpu.run(&mut t.h.bus, 12_480, 100_000);
    assert_eq!((e.executed, e.now), (1, 12_490), "the 8 ns that are left run as a one-instruction chunk");
    assert_eq!(t.h.cpu.ipsr(), 0, "the expiry is processed when the clock catches up at the next chunk");
    let e = t.h.cpu.run(&mut t.h.bus, 12_490, 12_500);
    assert_eq!(e.executed, 1);
    assert_eq!(t.h.cpu.ipsr(), SYSTICK, "entered at the start of the chunk, before its first instruction");
    assert_eq!(t.h.cpu.next_internal_deadline(), Some(12_488 + 12_488));
}

// ---------------------------------------------------------------------------------------------
// WFI / WFE / sleeping

#[test]
fn wfi_is_one_instruction_ends_the_chunk_and_sleeps_until_something_is_pending() {
    let mut t = T::new();
    t.main(WFI_PROG); // movs r0,#1 ; wfi ; movs r0,#2 ; b .
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (2, ExitReason::Sleeping));
    assert!(t.h.cpu.is_sleeping());
    assert_eq!(t.h.r(0), 1);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (0, ExitReason::Sleeping), "still asleep, nothing executed");
    t.pulse(5);
    let e = t.h.step_once(100);
    assert!(!t.h.cpu.is_sleeping() || e.reason == ExitReason::Sleeping);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1, "the handler ran");
    assert_eq!(t.h.r(0), 2, "execution resumed after the wfi");
}

#[test]
fn wfi_wakes_for_a_masked_pending_exception_once_per_chunk() {
    // Renode quirk: the wake-up condition ignores PRIMASK, so the core runs `b 1b ; wfi` again
    // every chunk although the handler can never run.
    let mut t = T::new();
    t.main(&[0xB672, 0xBF30, 0xE7FD, 0xE7FE]); // cpsid i ; 1: wfi ; b 1b ; b .
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (2, ExitReason::Sleeping)); // cpsid, wfi
    t.pulse(5);
    for _ in 0..4 {
        let e = t.h.step_once(100);
        assert_eq!((e.executed, e.reason), (2, ExitReason::Sleeping), "b 1b ; wfi");
    }
    assert_eq!(t.h.bus.peek32(COUNTER_A), 0, "PRIMASK keeps the handler from running");
}

#[test]
fn branch_to_self_sleeps_and_resumes_at_the_same_instruction() {
    let mut t = T::new();
    t.main(SPIN);
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (1, ExitReason::Sleeping), "`B .` executes as WFI");
    assert_eq!(t.h.cpu.pc(), MAIN, "and retries the same instruction");
    t.pulse(5);
    let e = t.h.step_once(100);
    assert_eq!(e.reason, ExitReason::Sleeping);
    assert_eq!(e.executed, 5 + 1, "handler, then the retried `B .` sleeps again");
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
    assert_eq!(t.h.cpu.pc(), MAIN);
}

#[test]
fn wfe_returns_at_once_when_the_event_register_is_set() {
    let mut t = T::new();
    t.main(SEV_WFE); // sev ; wfe ; movs r0,#1 ; wfe ; movs r0,#2 ; b .
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (4, ExitReason::Sleeping), "sev, wfe (event consumed), movs, wfe sleeps");
    assert_eq!(t.h.r(0), 1);
    t.pulse(5);
    t.h.step_once(100);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
    assert_eq!(t.h.r(0), 2, "an exception wakes a WFE sleeper");
}

#[test]
fn sleep_on_exit_goes_back_to_sleep_after_every_exception_return() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(irq(5), H_COUNT_A);
    t.enable_irq(5);
    t.poke(SCR, 2); // SLEEPONEXIT
    t.pulse(5);
    let e = t.h.step(100);
    assert_eq!(e.reason, ExitReason::Sleeping);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
    assert_eq!(t.h.cpu.instructions(), 5, "only the handler ran, the loop never resumed");
    assert!(t.h.cpu.is_sleeping());
    t.pulse(5);
    let e = t.h.step(100);
    assert_eq!(e.reason, ExitReason::Sleeping);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 2);
    assert_eq!(t.h.cpu.instructions(), 10);
}

#[test]
fn wfi_executed_with_an_interrupt_asserted_in_the_same_block_leaves_the_sleep_flag_set() {
    // Renode quirk, derived from tlib: `helper_wfi` sets `env->wfi` at the end of the block; the
    // asserted line is then taken at the block boundary (the WFI exit is overwritten) and the
    // chunk ends there. The `wfi` flag is only cleared by `cpu_has_work` when the NVIC reports a
    // pending exception - there is none after the entry - so the core sleeps at the handler's
    // first instruction until another interrupt arrives.
    let mut t = T::new();
    // str r1,[r0] (ISPR) ; wfi ; movs r0,#7 ; b .
    t.main(&[0x6001, 0xBF30, 0x2007, 0xE7FE]);
    let isr = t.handler(irq(5), H_COUNT_A);
    t.handler(irq(6), H_LOG_1);
    t.enable_irq(5);
    t.poke(0xE000_E404, (0x40 << 8) | (0x20 << 16)); // IRQ5 priority 0x40, IRQ6 priority 0x20
    t.h.set(0, ISPR0);
    t.h.set(1, 1 << 5);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (2, ExitReason::StopRequested));
    assert_eq!(t.h.cpu.ipsr(), irq(5));
    assert_eq!(t.h.cpu.pc(), isr);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (0, ExitReason::Sleeping), "the stale wfi flag keeps the handler from starting");
    t.pulse(6); // not enabled: nothing to wake for
    assert_eq!(t.h.step_once(100).executed, 0);
    t.poke(ISER0, 1 << 6);
    t.pulse(6); // an enabled pending exception that can preempt wakes the core
    let e = t.h.step_once(100);
    assert!(e.executed > 0, "{e:?}");
    assert_eq!(t.log()[0], 1, "the IRQ6 handler ran first");
}

#[test]
fn deep_sleep_halts_systick_until_an_external_interrupt_asserts() {
    let mut t = T::new();
    t.main(WFI_PROG);
    t.handler(irq(9), H_COUNT_B);
    t.enable_irq(9);
    t.start_systick(1000, 7);
    t.poke(SCR, 4); // SLEEPDEEP
    let e = t.h.step_once(100);
    assert_eq!(e.reason, ExitReason::Sleeping);
    // The sleep hook of the NVIC runs when the core is next asked to run without work.
    assert_eq!(t.peek(SYST_CSR) & 1, 1);
    let e = t.h.step_once(1);
    assert_eq!((e.executed, e.reason), (0, ExitReason::Sleeping));
    assert_eq!(t.peek(SYST_CSR) & 1, 0, "SysTick is halted while in deep sleep");
    let now = t.h.now;
    t.h.cpu.advance_idle(now, now + 10_000_000);
    t.h.now = now + 10_000_000;
    assert_eq!(t.peek(ICSR) & (1 << 26), 0, "no SysTick expiry while halted");
    t.h.cpu.set_irq_line(9, true);
    assert_eq!(t.peek(SYST_CSR) & 1, 1, "an asserted external interrupt re-enables SysTick");
}

#[test]
fn advance_idle_lets_systick_expire_while_the_core_sleeps() {
    let mut t = T::new();
    t.main(WFI_PROG);
    t.handler(SYSTICK, H_COUNT_A);
    t.start_systick(1000, 7); // period 12 500 ns
    let e = t.h.step_once(100);
    assert_eq!(e.reason, ExitReason::Sleeping);
    let now = t.h.now;
    let deadline = t.h.cpu.next_internal_deadline().unwrap();
    assert_eq!(deadline, 12_500);
    t.h.cpu.advance_idle(now, deadline);
    t.h.now = deadline;
    assert_ne!(t.peek(ICSR) & (1 << 26), 0, "PENDSTSET");
    let e = t.h.step_once(100);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1, "{e:?}");
    assert_eq!(t.h.r(0), 2, "woken and resumed after the wfi");
    assert_eq!(t.h.cpu.next_internal_deadline(), Some(25_000));
}

#[test]
fn a_halted_core_does_not_run_but_its_clock_does() {
    let mut t = T::new();
    t.main(COUNT_LOOP);
    t.handler(SYSTICK, H_COUNT_A);
    t.start_systick(100, 7); // 1250 ns
    t.h.cpu.set_halted(true);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (0, ExitReason::Halted));
    t.h.cpu.advance_idle(0, 5_000);
    t.h.now = 5_000;
    t.h.cpu.set_halted(false);
    let e = t.h.step_once(100);
    assert!(e.executed > 0);
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1, "the expiry that happened while halted is taken at once");
    assert_eq!(t.h.cpu.instructions(), 100, "no instruction ran while halted");
}

// ---------------------------------------------------------------------------------------------
// SysTick and DWT as clock entries

#[test]
fn cvr_read_sees_the_start_of_its_translation_block() {
    let mut t = T::new();
    t.main(SYSTICK_READ_PROG);
    let e = t.h.step_once(1000);
    // The CVR write (and the CSR write) end the chunk at the next block end: ... str, b 1f.
    assert_eq!((e.executed, e.reason), (8, ExitReason::StopRequested));
    assert_eq!(t.h.cpu.clock_time(), 0, "the machine clock has not seen the first chunk yet");
    t.h.step_once(1000);
    // SysTick counts from the chunk start (t = 0) although the enable store was the 7th instruction.
    // Renode's `SyncTime()` reports only the instructions of the *completed* blocks: the read is the 6th
    // instruction of the block that starts after `b 1f` (the 9th instruction, at 80 ns), so it sees
    // 80 ns = 6.4 cycles instead of the 130 ns at which the load itself starts.
    assert_eq!(t.h.r(2), 79_999 - 6);
}

#[test]
fn a_taken_branch_starts_a_new_block_for_the_next_sync_read() {
    let mut t = T::new();
    t.main(SYSTICK_READ_AFTER_BRANCH_PROG);
    t.h.step_once(1000);
    t.h.step_once(1000);
    // The same instructions as above with `b 2f` right before the load: the load is the first instruction of
    // its block and sees its own start (14th instruction, 130 ns = 10.4 cycles).
    assert_eq!(t.h.r(2), 79_999 - 10);
}

#[test]
fn cyccnt_read_sees_the_start_of_its_translation_block() {
    let mut t = T::new();
    t.main(DWT_READ_PROG);
    let e = t.h.step_once(1000);
    assert_eq!((e.executed, e.reason), (4, ExitReason::StopRequested), "a DWT register write requests a return");
    t.h.step_once(1000);
    // Enabled at t = 0 (chunk-start clock); the read is the 3rd instruction of the block that starts at the 5th
    // instruction (40 ns = 3.2 cycles), its own start would be 60 ns = 4.8 cycles.
    assert_eq!(t.h.r(2), 3);
}

#[test]
fn systick_and_dwt_reads_synchronize_the_board_clock_first() {
    struct Recording {
        inner: TestBus,
        syncs: Vec<u64>,
    }
    impl CpuBus for Recording {
        fn read8(&mut self, a: u32, i: u64) -> u8 {
            self.inner.read8(a, i)
        }
        fn read16(&mut self, a: u32, i: u64) -> u16 {
            self.inner.read16(a, i)
        }
        fn read32(&mut self, a: u32, i: u64) -> u32 {
            self.inner.read32(a, i)
        }
        fn write8(&mut self, a: u32, v: u8, i: u64) {
            self.inner.write8(a, v, i)
        }
        fn write16(&mut self, a: u32, v: u16, i: u64) {
            self.inner.write16(a, v, i)
        }
        fn write32(&mut self, a: u32, v: u32, i: u64) {
            self.inner.write32(a, v, i)
        }
        fn code_region(&self, a: u32) -> Option<(u32, &[u8])> {
            self.inner.code_region(a)
        }
        fn fetch16(&mut self, a: u32) -> u16 {
            self.inner.fetch16(a)
        }
        fn is_plain_memory(&self, a: u32) -> bool {
            self.inner.is_plain_memory(a)
        }
        fn take_notifications(&mut self) -> u32 {
            self.inner.take_notifications()
        }
        fn drain_irq_changes(&mut self, sink: &mut dyn FnMut(u32, bool)) {
            self.inner.drain_irq_changes(sink)
        }
        fn sync_time(&mut self, icount: u64) {
            self.syncs.push(icount);
        }
    }
    for (prog, expected_icount) in [(SYSTICK_READ_PROG, 8u64), (DWT_READ_PROG, 4)] {
        let mut cpu = Cpu::new(Default::default());
        let mut bus = Recording { inner: TestBus::new(), syncs: Vec::new() };
        bus.inner.load_halfwords(MAIN, prog);
        cpu.set_pc(MAIN);
        let mut now = 0;
        for _ in 0..3 {
            let e = cpu.run(&mut bus, now, now + 1000 * TPI);
            now = e.now;
        }
        assert_eq!(bus.syncs, vec![expected_icount], "instructions retired before the translation block of the register read started");
    }
}

#[test]
fn countflag_is_read_to_clear() {
    let mut t = T::new();
    t.main(&[0x6802, 0x6803, 0xE7FE]); // ldr r2,[r0] ; ldr r3,[r0] ; b .
    t.h.set(0, SYST_CSR);
    t.start_systick(99, 1); // ENABLE only: 1250 ns period... 99 cycles = 1237.5 ns
    t.h.cpu.advance_idle(0, 5_000);
    t.h.now = 5_000;
    t.h.step_once(10);
    assert_eq!(t.h.r(2), 0x0001_0005, "COUNTFLAG | CLKSOURCE | ENABLE ... ");
}

#[test]
fn the_freertos_initialization_order_expires_immediately_without_pending_the_exception() {
    // CSR = 0 ; CVR = 0 ; RVR = n ; CSR = ENABLE | TICKINT | CLKSOURCE. CVR was written while
    // RELOAD was still 0 (so the counter reads 0); enabling the counter reaches its limit in the
    // zero-time update, but TICKINT is applied after ENABLE in the same register write.
    let mut t = T::new();
    t.main(FREERTOS_SYSTICK_INIT);
    t.h.step_once(1000);
    t.h.step_once(1000);
    assert_eq!(t.h.r(2), 0x0001_0007, "COUNTFLAG is already set when the first read returns");
    assert_eq!(t.peek(ICSR) & (1 << 26), 0, "no SysTick exception for the immediate expiry");
    assert_eq!(t.peek(SYST_CSR) & 0x10007, 0x7, "COUNTFLAG cleared by the read");
    // The counter then runs a full period from the reload.
    assert_eq!(t.h.cpu.next_internal_deadline(), Some(999_988));
}

#[test]
fn only_effective_systick_writes_request_a_chunk_end() {
    // RVR write with RELOAD == 0 and the counter stopped: nothing in the timer changes.
    let mut t = T::new();
    t.main(STORE_NOPS_B);
    t.h.set(0, SYST_RVR);
    t.h.set(1, 0x1000);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (9, ExitReason::Sleeping), "ran to the final `b .`");
    // A CVR write always touches the LimitTimer.
    let mut t = T::new();
    t.main(STORE_NOPS_B);
    t.h.set(0, SYST_CVR);
    let e = t.h.step_once(100);
    assert_eq!((e.executed, e.reason), (4, ExitReason::StopRequested));
}

// ---------------------------------------------------------------------------------------------
// Exactness of the idle-loop fast-forward

fn idle_scenario(ff: bool, irq_at: Option<u64>) -> (u64, u32, u64, u32, u32, u32) {
    let mut t = T::new();
    t.h.cpu.set_idle_fast_forward(ff);
    t.main(IDLE_LOOP);
    t.handler(SYSTICK, H_COUNT_A);
    t.handler(irq(5), H_LOG_3);
    t.enable_irq(5);
    t.start_systick(999, 7);
    let mut injected = false;
    while t.h.now < 400_000 && !t.h.cpu.is_sleeping() {
        let quantum_end = (t.h.now / 100_000 + 1) * 100_000;
        if let Some(at) = irq_at {
            if !injected && t.h.now >= at {
                t.h.cpu.set_irq_line(5, true);
                t.h.cpu.set_irq_line(5, false);
                injected = true;
            }
        }
        t.h.run_until(quantum_end);
    }
    (t.h.cpu.instructions(), t.h.cpu.pc(), t.h.cpu.clock_time(), t.h.r(0), t.h.bus.peek32(COUNTER_A), t.h.bus.peek32(LOG_PTR))
}

#[test]
fn idle_loop_fast_forward_is_exact() {
    for irq_at in [None, Some(5_000)] {
        let off = idle_scenario(false, irq_at);
        let on = idle_scenario(true, irq_at);
        assert_eq!(on, off, "fast-forward on/off must give identical state (irq at {irq_at:?})");
        assert_eq!(on.3, 99, "the loop left through the SysTick handler");
    }
    // And it really skips something.
    let mut t = T::new();
    t.main(IDLE_LOOP);
    t.handler(SYSTICK, H_COUNT_A);
    t.start_systick(999, 7);
    while !t.h.cpu.is_sleeping() {
        let end = (t.h.now / 100_000 + 1) * 100_000;
        t.h.run_until(end);
    }
    let stats = t.h.cpu.fast_forward_stats();
    assert!(stats.loops >= 1 && stats.skipped_instructions > 1000, "{stats:?}");
}

// ---------------------------------------------------------------------------------------------
// Lazy floating point context

const FP_ONE: u32 = 0x3F80_0000;
const FP_TWO: u32 = 0x4000_0000;

fn fp_setup(fpccr: u32, handler: &[u16]) -> T {
    let mut t = T::new();
    t.poke(CPACR, 0x00F0_0000);
    t.poke(FPCCR, fpccr);
    t.main(FP_MAIN); // vmov s0,#1.0 ; vmov s1,#2.0 ; vmov s16,#4.0 ; adds/b loop
    t.handler(irq(5), handler);
    t.enable_irq(5);
    t.h.step(3);
    assert_eq!(t.h.cpu.control() & 4, 4, "the first FP instruction creates the context (CONTROL.FPCA)");
    t
}

#[test]
fn lazy_stacking_reserves_the_frame_and_defers_the_stores() {
    let mut t = fp_setup(0xC000_0000, H_FP_CAPTURE);
    t.pulse(5);
    t.h.step(11);
    let frame = TOP - 0x68;
    assert_eq!(t.h.bus.peek32(CAP + 4), 0xFFFF_FFE9, "EXC_RETURN: thread, MSP, extended frame");
    assert_eq!(t.h.bus.peek32(CAP + 12), 0, "FPCA is cleared on entry");
    assert_eq!(t.h.bus.peek32(CAP + 16) & 0xC000_0001, 0xC000_0001, "ASPEN, LSPEN and LSPACT");
    // Renode's tlib (`fpccr_update`) records HFRDY (the NVIC says a HardFault could preempt) but never USER
    // or THREAD: it evaluates the mode after the exception was acknowledged, so the interrupted Thread mode
    // is lost and bit 3 stays clear. The fault handlers are disabled here, so MMRDY / BFRDY / UFRDY are clear.
    assert_eq!(t.h.bus.peek32(CAP + 16), 0xC000_0011, "ASPEN | LSPEN | HFRDY | LSPACT");
    assert_eq!(t.h.bus.peek32(CAP + 20), frame + 0x20, "FPCAR points at the reserved S0 slot");
    assert_eq!(t.h.bus.peek32(frame + 0x20), 0, "S0 has not been stored yet");
    // The return discards the reservation: the live registers were never touched.
    assert_eq!(t.h.cpu.control() & 4, 4, "FPCA restored by the extended EXC_RETURN");
    assert_eq!(t.peek(FPCCR), 0xC000_0010, "LSPACT cleared by the return; the readiness snapshot stays (Renode reads 0xC0000010)");
    assert_eq!(t.h.cpu.fp_regs().s[0], FP_ONE);
    assert_eq!(t.h.r(13), TOP);
}

#[test]
fn a_floating_point_instruction_in_the_handler_triggers_the_lazy_store() {
    let mut t = fp_setup(0xC000_0000, H_FP_TOUCH);
    t.pulse(5);
    let frame = TOP - 0x68;
    t.h.step(1); // vmov.f32 s0, #3.0: preserves the interrupted state first
    assert_eq!(t.h.bus.peek32(frame + 0x20), FP_ONE, "S0 stored in the reserved slot");
    assert_eq!(t.h.bus.peek32(frame + 0x24), FP_TWO, "S1");
    assert_eq!(t.h.bus.peek32(frame + 0x20 + 16 * 4 + 0x4 - 4), 0, "FPSCR slot holds the interrupted FPSCR");
    assert_eq!(t.peek(FPCCR) & 1, 0, "LSPACT cleared once the state is preserved");
    assert_eq!(t.h.cpu.fp_regs().s[0], 0x4040_0000, "the handler works on its own context");
    t.h.step(30);
    assert_eq!(t.h.cpu.fp_regs().s[0], FP_ONE, "restored on return");
    assert_eq!(t.h.cpu.fp_regs().s[1], FP_TWO);
    assert_eq!(t.h.cpu.fp_regs().s[16], 0x4080_0000, "callee-saved registers are untouched");
    assert_eq!(t.h.bus.peek32(COUNTER_A), 1);
}

#[test]
fn without_lazy_preservation_the_state_is_stacked_at_entry() {
    let mut t = fp_setup(0x8000_0000, H_FP_CAPTURE); // ASPEN only
    t.pulse(5);
    t.h.step(1);
    let frame = TOP - 0x68;
    assert_eq!(t.h.r(14), 0xFFFF_FFE9);
    assert_eq!(t.h.r(13), frame);
    assert_eq!(t.h.bus.peek32(frame + 0x20), FP_ONE, "S0");
    assert_eq!(t.h.bus.peek32(frame + 0x24), FP_TWO, "S1");
    assert_eq!(t.h.bus.peek32(frame + 0x60), 0, "FPSCR");
    assert_eq!(t.h.bus.peek32(frame + 0x64), 0xBADC_AFEE, "reserved word");
    assert_eq!(t.peek(FPCCR) & 1, 0, "no lazy state pending");
    t.h.step(30);
    assert_eq!(t.h.cpu.fp_regs().s[0], FP_ONE);
    assert_eq!(t.h.r(13), TOP);
}
