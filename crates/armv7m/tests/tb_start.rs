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

// ---------------------------------------------------------------------------------------------
// The idle fast-forward leaves the translation state as interpretation does

/// `movs r3,#0 ; head: adds r3,#1 ; cmp r3,#3 ; bne skip ; X: nop ; nop ; ldr r1,[r0] ; nop ; skip: cmp r3,#5 ;
/// bne head ; movs r3,#2 ; b head`. The backward branch at `bne head` closes a loop whose body is pure by its kinds, so the
/// fast-forward looks at it after the first iteration; the path through X is taken for the first time in the third
/// iteration, and its load (MMIO) then ends every verification.
const LATE_PATH_LOOP: &[u16] = &[0x2300, 0x3301, 0x2B03, 0xD103, 0xBF00, 0xBF00, 0x6801, 0xBF00, 0x2B05, 0xD1F6, 0x2302, 0xE7F4];

/// `head: ldr r1,[r2] ; cmp r1,#1 ; bne skip ; X: nop ; nop ; skip: cmp r1,#0 ; beq head ; b .`: a polling loop on plain
/// RAM (r2 points to a zero word) that the fast-forward proves to be a fixed point and skips; the two instructions at X are
/// part of the loop body but never execute.
const SKIPPED_POLL_LOOP: &[u16] = &[0x6811, 0x2901, 0xD101, 0xBF00, 0xBF00, 0x2900, 0xD0F8, 0xE7FE];

/// Per chunk: the MMIO accesses (with the block-start retire count they report), the retire count and the exactness digest.
type ChunkPoint = (Vec<(u32, bool, u64)>, u64, u64);

/// Runs `code` with the fast-forward on or off: a first chunk of `first` instructions, then the `later` chunks. Returns a
/// [`ChunkPoint`] per chunk (the exactness digest covers the registers, the retire counts, the predecode cache and the
/// cut-block history) and the number of instructions the fast-forward skipped.
fn chunks_with_fast_forward(code: &[u16], fast_forward: bool, first: u64, later: &[u64]) -> (Vec<ChunkPoint>, u64) {
    let mut h = harness(code);
    h.set(2, SRAM1_BASE + 0x100);
    h.cpu.set_idle_fast_forward(fast_forward);
    let mut points = Vec::new();
    for &n in std::iter::once(&first).chain(later) {
        h.bus.mmio_log.clear();
        h.step(n);
        points.push((bus_icounts(&h), h.cpu.instructions(), h.cpu.exactness_digest()));
    }
    (points, h.cpu.fast_forward_stats().skipped_instructions)
}

#[test]
fn the_fast_forward_does_not_translate_a_loop_body_ahead_of_execution() {
    // Every first chunk from one instruction to past the third iteration (X starts a block at instruction 14 and the load
    // is the 17th instruction): whether the chunk ends inside the block that starts at X (the first time X is translated,
    // which tlib keeps cut) must not depend on the fast-forward having looked at the loop. Later chunks of several sizes then
    // see the same block starts for the load. (A first chunk of 15 is the reviewed case: with the loop translated ahead,
    // later loads reported the block start one instruction earlier than interpretation.)
    for first in 1..=22 {
        for later in [&[200u64, 200, 200][..], &[7, 13, 300, 5, 150]] {
            let (on, _) = chunks_with_fast_forward(LATE_PATH_LOOP, true, first, later);
            let (off, skipped) = chunks_with_fast_forward(LATE_PATH_LOOP, false, first, later);
            assert_eq!(skipped, 0);
            for (index, (a, b)) in on.iter().zip(&off).enumerate() {
                assert_eq!(a.0, b.0, "first chunk {first}, later {later:?}: block starts of the loads in chunk {index}");
                assert_eq!(a.1, b.1, "first chunk {first}, later {later:?}: retire count after chunk {index}");
                assert_eq!(a.2, b.2, "first chunk {first}, later {later:?}: exactness digest after chunk {index}");
            }
        }
    }
}

#[test]
fn a_skipped_polling_loop_leaves_its_untaken_path_untranslated() {
    // The loop is a fixed point and the fast-forward skips most of it; the predecode cache must still hold only what was
    // executed (the two instructions at X stay untranslated), so the digests agree at every chunk boundary.
    for first in [1, 3, 6, 50, 500] {
        let (on, skipped) = chunks_with_fast_forward(SKIPPED_POLL_LOOP, true, first, &[1000, 5000, 37, 20_000]);
        let (off, none) = chunks_with_fast_forward(SKIPPED_POLL_LOOP, false, first, &[1000, 5000, 37, 20_000]);
        assert!(skipped > 0 && none == 0, "the fast-forward must skip iterations ({skipped})");
        assert_eq!(on, off, "first chunk {first}");
    }
}
