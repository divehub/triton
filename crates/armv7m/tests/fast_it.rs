//! Shadow verification of `Cpu::set_fast_it`: random programs full of IT blocks run twice, once with the
//! IT block bodies stepped one at a time by the outer loop (`fast_it` off, the reference) and once inside
//! the hot loop (`fast_it` on). After every chunk the complete architectural state, the FP state, the
//! memory, the MMIO access log (which carries the translation-block-start instruction counts), the
//! SysTick / DWT readings and the exit report of the chunk must be identical.
//!
//! The programs mix flag-setting 16-bit instructions (which behave differently inside an IT block), 32-bit
//! instructions, loads and stores (RAM, MMIO that requests a chunk return, the SysTick and DWT registers),
//! VFP, taken and untaken branches as the last instruction of a block, faults (UDF, SVC, BKPT) inside and
//! behind blocks, blocks that straddle a 1 KiB page boundary, code in SRAM (the uncached path) and
//! interrupts and SysTick expiries that arrive anywhere, including between the instructions of a block.

mod common;

use armv7m::{Cpu, CpuConfig, ExitReason, BUS_STOP_REQUESTED};
use common::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.next() >> 11) % n
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    fn reg(&mut self) -> u16 {
        self.below(4) as u16
    }
}

const NOP: u16 = 0xBF00;
const UDF: u16 = 0xDE00;
const SVC: u16 = 0xDF00;
const BKPT: u16 = 0xBE00;

fn vfp_regs(rng: &mut Rng) -> (u16, u16, u16) {
    (rng.below(3) as u16, rng.below(3) as u16, rng.below(3) as u16)
}

/// An instruction that may sit anywhere in an IT block (it does not branch).
fn body(rng: &mut Rng) -> Vec<u16> {
    let (rd, rn, rm) = (rng.reg(), rng.reg(), rng.reg());
    let imm8 = rng.below(256) as u16;
    match rng.below(40) {
        0 => vec![0x2000 | rd << 8 | imm8],                                                 // movs rd, #imm8 (flags outside IT only)
        1 => vec![0x3000 | rd << 8 | imm8],                                                 // adds rd, #imm8
        2 => vec![0x3800 | rd << 8 | imm8],                                                 // subs rd, #imm8
        3 => vec![0x2800 | rn << 8 | imm8],                                                 // cmp rn, #imm8
        4 => vec![0x1800 | rm << 6 | rn << 3 | rd],                                         // adds rd, rn, rm
        5 => vec![0x1A00 | rm << 6 | rn << 3 | rd],                                         // subs rd, rn, rm
        6 => vec![0x1C00 | (rng.below(8) as u16) << 6 | rn << 3 | rd],                      // adds rd, rn, #imm3
        7 | 8 | 9 | 10 => vec![0x4000 | (rng.below(16) as u16) << 6 | rm << 3 | rd],         // the register data-processing group
        11 => vec![(rng.below(3) as u16) << 11 | (rng.below(32) as u16) << 6 | rm << 3 | rd], // lsls/lsrs/asrs rd, rm, #imm5
        12 => vec![0x4600 | rm << 3 | rd],                                                  // mov rd, rm
        13 => vec![0x6800 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // ldr rd, [r4, #imm]
        14 => vec![0x6000 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // str rd, [r4, #imm]
        15 => vec![0x7800 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // ldrb
        16 => vec![0x7000 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // strb
        17 => vec![0x8800 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // ldrh
        18 => vec![0x8000 | (rng.below(32) as u16) << 6 | 4 << 3 | rd],                     // strh
        19 => vec![0x4800 | rd << 8 | rng.below(16) as u16],                                // ldr rd, [pc, #imm]
        20 => vec![NOP],
        21 => vec![0xF04F, rd << 8 | imm8],                                                 // mov.w rd, #imm8
        22 => vec![0xF05F, rd << 8 | imm8],                                                 // movs.w
        23 => vec![0xEB00 | rn, rd << 8 | rm],                                              // add.w rd, rn, rm
        24 => vec![0xEB10 | rn, rd << 8 | rm],                                              // adds.w
        25 => vec![0xEBA0 | rn, rd << 8 | rm],                                              // sub.w
        26 => vec![0xEBB0 | rn, rd << 8 | rm],                                              // subs.w
        27 => vec![0xF2C0, (rng.below(8) as u16) << 12 | rd << 8 | imm8],                   // movt
        28 => vec![0xF8D0 | 4, rd << 12 | (rng.below(1024) as u16)],                        // ldr.w rd, [r4, #imm12]
        29 => vec![0xF8C0 | 4, rd << 12 | (rng.below(1024) as u16)],                        // str.w
        30 => vec![if rng.chance(2) { 0xFB90 } else { 0xFBB0 } | rn, 0xF0F0 | rd << 8 | rm], // sdiv / udiv
        31 => {
            // VFP: vadd.f32, vmov.f32, vcmp (sets FPSCR flags), vmrs APSR_nzcv, vldr, vstr
            let (d, n, m) = vfp_regs(rng);
            match rng.below(6) {
                0 => vec![0xEE30, d << 12 | 0x0A00 | (n & 1) << 7 | (m & 1) << 5 | (m >> 1)],
                1 => vec![0xEEB0, d << 12 | 0x0A40 | (m & 1) << 5 | (m >> 1)],
                2 => vec![0xEEB4, d << 12 | 0x0A40 | (m & 1) << 5 | (m >> 1)],
                3 => vec![0xEEF1, 0xFA10],
                4 => vec![0xED90 | 4, d << 12 | 0x0A00 | rng.below(4) as u16],
                _ => vec![0xED80 | 4, d << 12 | 0x0A00 | rng.below(4) as u16],
            }
        }
        32 => vec![0x6800 | 1 << 6 | 5 << 3 | rd],                                          // ldr rd, [r5, #4]: DWT_CYCCNT (syncs the time)
        33 => vec![0x6800 | 2 << 6 | 7 << 3 | rd],                                          // ldr rd, [r7, #8]: SysTick CVR (syncs the time)
        34 => vec![0x6800 | (rng.below(3) as u16) << 6 | 7 << 3 | rd],                      // SysTick CSR / RVR
        35 | 36 => vec![0x6000 | (rng.below(10) as u16) << 6 | 6 << 3 | rd],                // str rd, [r6, #imm]: MMIO, offset 0x10 requests a chunk return
        37 => vec![0x6800 | (rng.below(10) as u16) << 6 | 6 << 3 | rd],                     // ldr rd, [r6, #imm]: MMIO read (logged)
        38 => vec![0xF3AF, 0x8000],                                                         // nop.w
        _ => vec![0xBA00 | rm << 3 | rd],                                                   // rev
    }
}

/// A branch or fault that may end an IT block (and ends a translation block).
fn terminator_in_block(rng: &mut Rng) -> Vec<u16> {
    match rng.below(5) {
        0 | 1 => vec![0xE000, NOP],               // b.n +0: skips the padding nop
        2 => vec![0xF000, 0xF800],                // bl +0: the next instruction
        3 => vec![UDF, NOP],                      // UsageFault escalated to HardFault; the handler skips the padding
        _ => vec![SVC, NOP, NOP],
    }
}

/// An instruction outside any IT block that ends a translation block or changes the flags / interrupt masks.
fn standalone(rng: &mut Rng) -> Vec<u16> {
    let rn = rng.reg();
    match rng.below(14) {
        0 | 1 => vec![0xD000 | (rng.below(14) as u16) << 8, NOP], // b<c> +0 over a nop
        2 => vec![0xE000, NOP],
        3 => vec![0xF000, 0xF800],
        4 => vec![0xB100 | rn, NOP],  // cbz rn, +4
        5 => vec![0xB900 | rn, NOP],  // cbnz
        6 => vec![0xB672],            // cpsid i
        7 => vec![0xB662],            // cpsie i
        8 => vec![0xF380 | rn, 0x8800], // msr apsr_nzcvq, rn: random flags
        9 => vec![0xF3BF, 0x8F5F],    // dmb
        10 => vec![0xB40F, 0xBC0F],   // push {r0-r3} ; pop {r0-r3}
        11 => vec![UDF, NOP],
        12 => vec![BKPT, NOP],
        _ => vec![SVC, NOP, NOP],
    }
}

/// IT mask for `n` instructions after the first with the then/else pattern `pattern` (true = same condition).
fn it_word(first_cond: u16, pattern: &[bool]) -> u16 {
    let cond0 = first_cond & 1;
    let mut mask = 0u16;
    for (k, then) in pattern.iter().enumerate() {
        let bit = if *then { cond0 } else { 1 - cond0 };
        mask |= bit << (3 - k);
    }
    mask |= 1 << (3 - pattern.len());
    0xBF00 | first_cond << 4 | mask
}

fn it_block(rng: &mut Rng) -> Vec<u16> {
    let n = 1 + rng.below(4) as usize;
    let first_cond = if n == 1 && rng.chance(8) { 14 } else { rng.below(14) as u16 };
    let pattern: Vec<bool> = (0..n - 1).map(|_| rng.chance(2)).collect();
    let mut out = vec![it_word(first_cond, &pattern)];
    for k in 0..n {
        if k == n - 1 && rng.chance(5) {
            out.extend(terminator_in_block(rng));
        } else {
            out.extend(body(rng));
        }
    }
    out
}

/// At most 1800 bytes: the closing `b.n` back to the start reaches +-2 KiB.
fn program(rng: &mut Rng, snippets: usize) -> Vec<u16> {
    let mut out = Vec::new();
    for _ in 0..snippets {
        if out.len() > 880 {
            break;
        }
        match rng.below(10) {
            0..=4 => out.extend(it_block(rng)),
            5..=8 => out.extend(body(rng)),
            _ => out.extend(standalone(rng)),
        }
    }
    out
}

/// Addresses used by the setup.
const IRQ_HANDLER: u32 = 0x0800_4800;
const FAULT_HANDLER: u32 = 0x0800_4900;
const TICK_HANDLER: u32 = 0x0800_4A00;

fn setup(program: &[u16], origin: u32, fast_it: bool, seed: u64) -> Harness {
    let mut h = Harness::with_config(CpuConfig::default());
    h.cpu.set_fast_it(fast_it);
    // Vector table: IRQ handlers, SysTick, and one handler for every fault-like exception.
    let irq_handler = [0x6FE0u16, 0x3001, 0x67E0, 0x4770]; // ldr r0,[r4,#0x7c]; adds r0,#1; str r0,[r4,#0x7c]; bx lr
    let fault_handler = [0x9806u16, 0x3002, 0x9006, 0x4770]; // ldr r0,[sp,#24]; adds r0,#2; str r0,[sp,#24]; bx lr
    let tick_handler = [0x6FE0u16, 0x3002, 0x67E0, 0x4770];
    h.bus.load_halfwords(IRQ_HANDLER, &irq_handler);
    h.bus.load_halfwords(FAULT_HANDLER, &fault_handler);
    h.bus.load_halfwords(TICK_HANDLER, &tick_handler);
    for exc in [3u32, 4, 5, 6, 11, 12, 14] {
        h.bus.poke32(FLASH_BASE + 4 * exc, FAULT_HANDLER | 1);
    }
    h.bus.poke32(FLASH_BASE + 4 * 15, TICK_HANDLER | 1);
    for irq in 0..4u32 {
        h.bus.poke32(FLASH_BASE + 4 * (16 + irq), IRQ_HANDLER | 1);
    }
    h.bus.load_halfwords(origin, program);
    // Loop back to the start behind the last snippet: b.n origin.
    let end = origin + 2 * program.len() as u32;
    let offset = (i64::from(origin) - i64::from(end) - 4) / 2;
    h.bus.load_halfwords(end, &[0xE000 | (offset as u16 & 0x7FF)]);
    h.cpu.set_pc(origin);
    h.bus.write_trigger = Some((MMIO_BASE + 0x10, BUS_STOP_REQUESTED));
    let now = h.now;
    h.cpu.ppb_poke32(0xE000_ED88, 0x00F0_0000, now); // CPACR: FPU
    h.cpu.ppb_poke32(0xE000_E100, 0xF, now); // enable IRQ 0..3
    h.cpu.ppb_poke32(0xE000_E014, 4_000 + (seed % 1000) as u32, now); // SysTick reload
    h.cpu.ppb_poke32(0xE000_E010, 7, now); // SysTick: enable, interrupt, processor clock
    h.cpu.ppb_poke32(0xE000_1000, 1, now); // DWT: CYCCNTENA
    let mut rng = Rng(seed ^ 0xABCDEF);
    for r in 0..4 {
        h.set(r, rng.next() as u32);
    }
    h.set(4, SRAM1_BASE + 0x1000);
    h.set(5, 0xE000_1000);
    h.set(6, MMIO_BASE);
    h.set(7, 0xE000_E010);
    h.cpu.set_apsr(rng.next() as u32 & 0xF800_0000);
    h
}

/// Equality of two byte ranges with a short failure message (the first differing offset).
fn same_bytes(x: &[u8], y: &[u8], what: &str, ctx: &str) {
    if let Some(i) = x.iter().zip(y).position(|(p, q)| p != q) {
        panic!("{ctx}: {what} differs at offset {i:#x}: {:#04x} vs {:#04x}", x[i], y[i]);
    }
}

fn compare(a: &Harness, b: &Harness, ea: armv7m::RunExit, eb: armv7m::RunExit, ctx: &str) {
    assert_eq!(ea, eb, "{ctx}: exit report");
    assert_eq!(a.cpu.snapshot(), b.cpu.snapshot(), "{ctx}: core registers");
    assert_eq!(a.cpu.fp_regs(), b.cpu.fp_regs(), "{ctx}: FP registers");
    assert_eq!(a.cpu.xpsr(), b.cpu.xpsr(), "{ctx}: xPSR");
    assert_eq!(a.cpu.is_locked_up(), b.cpu.is_locked_up(), "{ctx}: lockup");
    assert_eq!(a.cpu.undefined_log(), b.cpu.undefined_log(), "{ctx}: undefined log");
    same_bytes(&a.bus.sram1[..0x3000], &b.bus.sram1[..0x3000], "SRAM1 data", ctx);
    same_bytes(&a.bus.sram1[0x17000..], &b.bus.sram1[0x17000..], "SRAM1 stack", ctx);
    same_bytes(&a.bus.mmio, &b.bus.mmio, "MMIO window", ctx);
    assert_eq!(a.bus.mmio_log.len(), b.bus.mmio_log.len(), "{ctx}: MMIO access count");
    if let Some(i) = a.bus.mmio_log.iter().zip(&b.bus.mmio_log).position(|(p, q)| p != q) {
        panic!("{ctx}: MMIO access {i} differs (carries the translation-block start count): {:?} vs {:?}", a.bus.mmio_log[i], b.bus.mmio_log[i]);
    }
    for reg in [0xE000_E018u32, 0xE000_E010, 0xE000_1004, 0xE000_ED04, 0xE000_ED28] {
        assert_eq!(a.cpu.ppb_peek32(reg, a.now), b.cpu.ppb_peek32(reg, b.now), "{ctx}: PPB register {reg:#x}");
    }
    assert_eq!(a.cpu.clock_time(), b.cpu.clock_time(), "{ctx}: machine clock");
}

fn shadow_run(seed: u64, origin: u32, steps: usize) -> (u64, u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let snippets = 40 + rng.below(160) as usize;
    let prog = program(&mut rng, snippets);
    let mut a = setup(&prog, origin, false, seed);
    let mut b = setup(&prog, origin, true, seed);
    let (mut executed, mut sleeps) = (0u64, 0u64);
    for step in 0..steps {
        // The same stimulus for both: an IRQ pulse, a stop request, then a chunk of random length.
        if rng.chance(12) {
            let n = rng.below(4) as u32;
            a.cpu.set_irq_line(n, true);
            a.cpu.set_irq_line(n, false);
            b.cpu.set_irq_line(n, true);
            b.cpu.set_irq_line(n, false);
        }
        let n = match rng.below(6) {
            0 => 1 + rng.below(3),
            1 => 1 + rng.below(12),
            2 => 1 + rng.below(40),
            3 => 1 + rng.below(200),
            4 => 1 + rng.below(1500),
            _ => 20 + rng.below(30),
        };
        let ea = a.step_once(n);
        let eb = b.step_once(n);
        executed += ea.executed;
        if matches!(ea.reason, ExitReason::Sleeping | ExitReason::Lockup | ExitReason::Halted) {
            sleeps += 1;
        }
        compare(&a, &b, ea, eb, &format!("seed {seed:#x} origin {origin:#x} step {step} chunk {n}"));
    }
    (executed, sleeps)
}

#[test]
fn it_blocks_in_the_hot_loop_equal_the_stepped_reference() {
    let mut total = 0u64;
    let mut sleeps = 0u64;
    for seed in 1..=240u64 {
        // Origins at various distances before a 1 KiB page boundary (blocks straddling it), and code in SRAM.
        let origin = match seed % 6 {
            0 => 0x0800_4000,
            1 => 0x0800_4000 + 0x400 - 2 * (seed % 23) as u32,
            2 => 0x0800_4002 + 0x400 - 2 * (seed % 11) as u32,
            3 => 0x0800_6000 + 0x400 - 2 * (seed % 5) as u32,
            4 => 0x2000_4000 + 2 * (seed % 7) as u32, // uncached code
            _ => 0x0800_5000 + 2 * (seed % 31) as u32,
        };
        let (e, s) = shadow_run(seed, origin, 600);
        total += e;
        sleeps += s;
    }
    eprintln!("fast_it shadow: {total} instructions compared, {sleeps} chunks ended in a lockup or sleep");
    assert!(total > 2_000_000, "the programs ran ({total} instructions)");
    // A program that locks up (a fault the core cannot take) stops contributing; most must keep running.
    assert!(sleeps < 240 * 600 / 4, "too many chunks ended in a lockup or sleep ({sleeps})");
}

#[test]
fn flag_setting_forms_behave_by_block_membership() {
    for fast in [false, true] {
        // adds r1, #1 (16-bit) sets the flags outside an IT block and leaves them alone inside one.
        let mut h = Harness::new();
        h.cpu.set_fast_it(fast);
        // cmp r0, r0 (Z=1, C=1) ; it eq ; adds r1, #1 ; adds r1, #1 ; nop
        h.load(0x0800_4000, &[0x4280, 0xBF08, 0x3101, 0x3101, NOP]);
        h.set(1, 5);
        h.step(3);
        assert_eq!((h.r(1), h.nzcv()), (6, 0b0110), "fast_it {fast}: inside the block the flags are untouched");
        h.step(1);
        assert_eq!((h.r(1), h.nzcv()), (7, 0b0000), "fast_it {fast}: behind the block adds sets them");
        // adds.w always sets the flags, inside a block as well.
        let mut h = Harness::new();
        h.cpu.set_fast_it(fast);
        // cmp r0, r0 ; it eq ; adds.w r2, r2, r3 ; nop
        h.load(0x0800_4000, &[0x4280, 0xBF08, 0xEB12, 0x0203, NOP]);
        h.set(2, 0x7FFF_FFFF);
        h.set(3, 1);
        h.step(3);
        assert_eq!((h.r(2), h.nzcv()), (0x8000_0000, 0b1001), "fast_it {fast}: N and V from the 32-bit adds");
        // A skipped instruction changes nothing and still ends its slot: the first slot keeps Z set, so the
        // else slot (NE) is skipped.
        let mut h = Harness::new();
        h.cpu.set_fast_it(fast);
        // cmp r0, r0 ; ite eq ; adds.w r2, r2, r3 (0 + 0) ; adds.w r2, r2, r3 (skipped) ; nop
        h.load(0x0800_4000, &[0x4280, 0xBF0C, 0xEB12, 0x0203, 0xEB12, 0x0203, NOP]);
        h.step(3);
        assert_eq!((h.r(2), h.nzcv(), h.cpu.itstate() != 0), (0, 0b0100, true), "fast_it {fast}: Z stays, C cleared by the add");
        h.step(1);
        assert_eq!((h.r(2), h.nzcv(), h.cpu.itstate()), (0, 0b0100, 0), "fast_it {fast}: the else slot was skipped and the block ended");
    }
}

#[test]
fn chunk_end_inside_a_block_resumes_with_the_same_state() {
    // The chunk budget ends between the instructions of a block; the next chunk continues it.
    for fast in [false, true] {
        let mut h = Harness::new();
        h.cpu.set_fast_it(fast);
        // cmp r0, r0 ; itttt eq ; movs r1,#1 ; movs r2,#2 ; movs r3,#3 ; adds r0,#7 ; nop ; b .
        h.load(0x0800_4000, &[0x4280, 0xBF01, 0x2101, 0x2202, 0x2303, 0x3007, NOP, 0xE7FE]);
        h.step_once(3); // cmp, it, movs r1
        assert_eq!(h.r(1), 1);
        assert_ne!(h.cpu.itstate(), 0, "fast_it {fast}: in the middle of the block");
        h.step_once(1);
        assert_eq!((h.r(2), h.r(3)), (2, 0));
        h.step_once(10);
        assert_eq!((h.r(2), h.r(3), h.r(0)), (2, 3, 7));
        assert_eq!(h.cpu.itstate(), 0);
    }
}

#[test]
fn undefined_instruction_inside_a_block_keeps_itstate_for_the_handler() {
    for fast in [false, true] {
        let mut h = Harness::new();
        h.cpu.set_fast_it(fast);
        let handler = 0x0800_4400;
        h.bus.load_halfwords(handler, &[0x9806, 0x3002, 0x9006, 0x4770]);
        for exc in [3u32, 6] {
            h.bus.poke32(FLASH_BASE + 4 * exc, handler | 1);
        }
        // cmp r0, r0 ; itt eq ; udf ; movs.. ; nop ; b .
        h.load(0x0800_4000, &[0x4280, 0xBF04, UDF, 0x2105, NOP, 0xE7FE]);
        h.step(2);
        let before = h.cpu.instructions();
        h.step(1); // udf: faults, the block's state stays; the handler entry costs nothing
        assert_eq!(h.cpu.instructions(), before + 1);
        let frame = SRAM1_BASE + SRAM1_SIZE as u32 - 0x20;
        let xpsr = h.bus.peek32(frame + 0x1C);
        assert_eq!(h.bus.peek32(frame + 0x18), 0x0800_4004, "return address is the faulting udf (fast_it {fast})");
        assert_eq!(xpsr & 0x0600_FC00, 0x0000_0400, "ITSTATE of the interrupted block is stacked (fast_it {fast})");
        h.step(30);
        assert_eq!(h.r(1), 5, "fast_it {fast}: the block continued after the handler skipped the udf");
    }
}

#[test]
fn cpu_default_is_the_fast_path() {
    assert!(Cpu::new(CpuConfig::default()).fast_it());
}
