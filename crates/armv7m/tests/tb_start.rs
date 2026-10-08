//! Translation-block bookkeeping that is visible on the bus: every `CpuBus` access (and `sync_time`)
//! carries the retire count at the start of the *translation block* of the accessing instruction,
//! because Renode's `SyncTime()` reports only the blocks that have been completed. The tests pin the
//! block partition: taken and untaken branches, the 1 KiB page rule, chunk starts, and the blocks that
//! tlib cuts short when a chunk budget ends inside a block it translates for the first time (and then
//! keeps, because it never regenerates a longer block for that address).

mod common;

use armv7m::ExitReason;
use common::*;

const MAIN: u32 = 0x0800_4000;

fn bus_icounts(h: &Harness) -> Vec<(u32, bool, u64)> {
    h.bus.mmio_log.iter().map(|a| (a.addr, a.write, a.icount)).collect()
}

fn harness(code: &[u16]) -> Harness {
    let mut h = Harness::new();
    h.load(MAIN, code);
    h.set(0, MMIO_BASE);
    h
}

/// `nop ; nop ; ldr r1,[r0] ; nop ; ldr r2,[r0,#4] ; nop ; nop ; b 1f ; 1: nop ; b .`: the first block has
/// eight instructions.
const STRAIGHT: &[u16] = &[0xBF00, 0xBF00, 0x6801, 0xBF00, 0x6842, 0xBF00, 0xBF00, 0xE7FF, 0xBF00, 0xE7FE];
/// `nop ; nop ; b 1f ; 1: ldr r1,[r0] ; nop ; str r1,[r0,#4] ; b .`
const BRANCH_THEN_ACCESS: &[u16] = &[0xBF00, 0xBF00, 0xE7FF, 0x6801, 0xBF00, 0x6041, 0xE7FE];

#[test]
fn an_access_reports_the_start_of_its_block_not_its_own_instruction() {
    let mut h = harness(STRAIGHT);
    h.step_once(8);
    // ldr r1 is the 3rd instruction and ldr r2 the 5th of the one block that starts with the chunk.
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 0), (MMIO_BASE + 4, false, 0)]);
}

#[test]
fn a_taken_branch_starts_a_new_block() {
    let mut h = harness(BRANCH_THEN_ACCESS);
    h.step_once(7);
    // nop, nop, b are the first block; the load starts the second (instruction 3) and the store, three
    // instructions later, still belongs to it.
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 3), (MMIO_BASE + 4, true, 3)]);
}

#[test]
fn a_chunk_start_starts_a_new_block_even_in_the_middle_of_straight_line_code() {
    let mut h = harness(STRAIGHT);
    h.step_once(1); // nop
    h.step_once(1); // nop
    let e = h.step_once(5);
    assert_eq!((e.executed, e.reason), (5, ExitReason::Deadline));
    // The third run starts at the ldr: its block begins at instruction 2; the second load is in it.
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 2), (MMIO_BASE + 4, false, 2)]);
}

#[test]
fn the_page_rule_splits_straight_line_code_at_a_one_kib_boundary() {
    // 0x080043F6: nop (last-but-one halfword of the page), 0x080043FE: nop (last halfword), then the
    // load at 0x08004400 starts a new block although nothing branched.
    let base = 0x0800_43F4;
    let mut h = Harness::new();
    h.bus.load_halfwords(base, &[0xBF00, 0xBF00, 0xBF00, 0xBF00, 0xBF00, 0xBF00, 0x6801, 0xBF00, 0x6842, 0xE7FE]);
    h.cpu.set_pc(base);
    h.set(0, MMIO_BASE);
    h.step_once(9);
    // Six nops from 0x43F4 to 0x43FE (the last one ends the page), then ldr r1 at 0x4400 (instruction 6).
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 6), (MMIO_BASE + 4, false, 6)]);
}

#[test]
fn a_32_bit_instruction_straddling_the_page_end_is_the_last_of_its_block() {
    // movw r3, #0x1234 occupies 0x080043FE..0x08004402.
    let base = 0x0800_43FA;
    let mut h = Harness::new();
    h.bus.load_halfwords(base, &[0xBF00, 0xBF00, 0xF241, 0x2334, 0x6801, 0xBF00, 0xE7FE]);
    h.cpu.set_pc(base);
    h.set(0, MMIO_BASE);
    h.step_once(5);
    assert_eq!(h.r(3), 0x1234);
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 3)], "the load starts the block after the straddling movw");
}

#[test]
fn an_untaken_conditional_branch_still_ends_its_block() {
    // movs r5,#1 ; cmp r5,#0 ; beq 1f (not taken) ; ldr r1,[r0] ; b .
    let mut h = harness(&[0x2501, 0x2D00, 0xD000, 0x6801, 0xE7FE]);
    h.step_once(4);
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, 3)]);
}

// ---------------------------------------------------------------------------------------------
// Blocks cut short by the chunk budget

/// Runs `STRAIGHT` from a cold cache with a first chunk of `first` instructions, then rewinds and
/// runs it again with a long chunk. Returns the bus accesses of the second run (the first run's are
/// dropped) and the retire count at which it started.
fn second_run(first: u64) -> (Vec<(u32, bool, u64)>, u64) {
    let mut h = harness(STRAIGHT);
    h.step_once(first);
    h.bus.mmio_log.clear();
    h.cpu.set_pc(MAIN);
    let base = h.cpu.instructions();
    h.step_once(8);
    (bus_icounts(&h), base)
}

#[test]
fn a_block_cut_by_the_budget_at_its_first_translation_stays_cut() {
    // Control: translated whole (the first chunk is long enough), the second load is in the first block.
    let (log, base) = second_run(8);
    assert_eq!(log, vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base)]);
    // The first chunk ends after 3 instructions inside the cold block: tlib caches a 3-instruction block
    // and finds it again for every later lookup at that address, so the remaining instructions are a
    // block of their own (the second load sees instruction 3 of the run).
    let (log, base) = second_run(3);
    assert_eq!(log, vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base + 3)]);
    // Cut right behind the first instruction.
    let (log, base) = second_run(1);
    assert_eq!(log, vec![(MMIO_BASE, false, base + 1), (MMIO_BASE + 4, false, base + 1)], "{log:?}");
}

#[test]
fn only_the_first_translation_of_an_address_is_remembered() {
    // The block was translated whole by the first run; a later chunk that ends inside it does not
    // change what the next long run sees (the model leaves tlib's chain and small-budget effects out).
    let mut h = harness(STRAIGHT);
    h.step_once(8);
    for first in [1, 2, 3] {
        h.cpu.set_pc(MAIN);
        h.step_once(first);
    }
    h.bus.mmio_log.clear();
    h.cpu.set_pc(MAIN);
    let base = h.cpu.instructions();
    h.step_once(8);
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base)]);
}

#[test]
fn a_smaller_budget_adds_a_cut_and_the_largest_one_that_fits_is_used() {
    let mut h = harness(STRAIGHT);
    h.step_once(5); // cold: cut at 5 instructions
    h.cpu.set_pc(MAIN);
    h.step_once(2); // nothing known fits 2: cut at 2
    h.cpu.set_pc(MAIN);
    h.step_once(5); // largest cut that fits 5 is 5 itself
    h.bus.mmio_log.clear();
    h.cpu.set_pc(MAIN);
    let base = h.cpu.instructions();
    h.step_once(8);
    // Cuts {2, 5}: a long budget takes the 5-instruction block; the rest (nop, nop, b) is another block.
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base)]);
    h.bus.mmio_log.clear();
    h.cpu.set_pc(MAIN);
    let base = h.cpu.instructions();
    h.step_once(4);
    // Budget 4: the largest known block that fits is the 2-instruction one; the load at instruction 2
    // starts the next block and the second load, instruction 4, would be past the budget.
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, base + 2)]);
}

#[test]
fn the_cut_is_the_same_while_tracing() {
    let mut h = harness(STRAIGHT);
    h.cpu.trace_pcs(100);
    h.step_once(3);
    h.bus.mmio_log.clear();
    h.cpu.set_pc(MAIN);
    let base = h.cpu.instructions();
    h.step_once(8);
    assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base + 3)]);
}

#[test]
fn reset_and_cache_invalidation_forget_the_cut_blocks() {
    for how in ["reset", "invalidate"] {
        let mut h = harness(STRAIGHT);
        h.step_once(3);
        match how {
            "reset" => h.cpu.reset(),
            _ => h.cpu.invalidate_code_cache(),
        }
        h.set(0, MMIO_BASE);
        h.cpu.set_vtor(FLASH_BASE);
        h.cpu.set_sp(SRAM1_BASE + SRAM1_SIZE as u32);
        h.cpu.set_pc(MAIN);
        h.bus.mmio_log.clear();
        let base = h.cpu.instructions();
        h.step_once(8);
        assert_eq!(bus_icounts(&h), vec![(MMIO_BASE, false, base), (MMIO_BASE + 4, false, base)], "{how}");
    }
}

#[test]
fn cuts_change_block_boundaries_but_never_the_instruction_stream() {
    // A loop with a body longer than most chunks: movs/adds/subs/cmp/bne, run in chunks of many sizes.
    // adds r0,#3 ; adds r1,r1,r0 ; eors r2,r1 ; subs r3,#1 ; bne loop ; (then spin)
    let prog: &[u16] = &[0x3003, 0x1809, 0x404A, 0x3B01, 0xD1FA, 0xE7FE];
    let run = |chunks: &[u64]| {
        let mut h = Harness::new();
        h.load(MAIN, prog);
        h.set(3, 1000);
        let mut i = 0;
        while h.cpu.instructions() < 4000 {
            let n = chunks[i % chunks.len()].min(4000 - h.cpu.instructions());
            h.step_once(n);
            i += 1;
        }
        (h.cpu.instructions(), h.r(0), h.r(1), h.r(2), h.r(3), h.cpu.pc())
    };
    let whole = run(&[4000]);
    for chunks in [&[1u64][..], &[3], &[7, 2, 11], &[5, 1, 1, 9]] {
        let got = run(chunks);
        assert_eq!(got, whole, "chunks {chunks:?}");
    }
}
