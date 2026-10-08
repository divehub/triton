//! The routine-acceleration machinery on synthetic routines: what it must memoize, what it must refuse, and that it is exact
//! either way. The routine table is replaced by one built from hand-assembled code (identified by the SHA-256 of its bytes
//! like the real routines), so every classification rule of the dependency tracker has a test:
//!
//! * a path that depends on the key alone is memoized (including pushes/pops of callee-saved registers and the return
//!   address, nested calls, IT blocks, conditionally written registers that pass through);
//! * a path that reads a caller register, reads or writes RAM outside the routine's own frame, reads flags it did not
//!   produce, uses SP as data, branches to a caller-supplied address, or reads/writes the whole FPSCR is **not** memoized;
//! * in every case the core with acceleration on ends in exactly the state of a core without it.

mod common;
use armv7m::accel::{anchor_hash, sha256, sha256_hex, Spec};
use armv7m::{ExitReason, RoutineAccelMode};
use common::*;

// ---- a tiny Thumb assembler ----------------------------------------------------------------------------------------------------

const NOP: u16 = 0xBF00;

fn movs(rd: u16, imm: u16) -> u16 {
    0x2000 | rd << 8 | imm
}
fn adds_imm8(rd: u16, imm: u16) -> u16 {
    0x3000 | rd << 8 | imm
}
fn subs_imm8(rd: u16, imm: u16) -> u16 {
    0x3800 | rd << 8 | imm
}
fn adds(rd: u16, rn: u16, rm: u16) -> u16 {
    0x1800 | rm << 6 | rn << 3 | rd
}
fn adcs(rd: u16, rm: u16) -> u16 {
    0x4140 | rm << 3 | rd
}
fn cmp_imm(rn: u16, imm: u16) -> u16 {
    0x2800 | rn << 8 | imm
}
fn mov_reg(rd: u16, rm: u16) -> u16 {
    0x4600 | (rd & 8) << 4 | rm << 3 | (rd & 7)
}
fn push(list: u16, lr: bool) -> u16 {
    0xB400 | if lr { 0x100 } else { 0 } | list
}
fn pop(list: u16, pc: bool) -> u16 {
    0xBC00 | if pc { 0x100 } else { 0 } | list
}
fn bx(rm: u16) -> u16 {
    0x4700 | rm << 3
}
/// `b<cond>` to `target` from the instruction at `at`.
fn bcc(cond: u16, at: u32, target: u32) -> u16 {
    let off = (target as i32 - (at as i32 + 4)) / 2;
    0xD000 | cond << 8 | (off as u16 & 0xFF)
}
fn ldr_imm(rt: u16, rn: u16, imm: u16) -> u16 {
    0x6800 | (imm / 4) << 6 | rn << 3 | rt
}
fn str_imm(rt: u16, rn: u16, imm: u16) -> u16 {
    0x6000 | (imm / 4) << 6 | rn << 3 | rt
}
fn ldr_pc(rt: u16, imm: u16) -> u16 {
    0x4800 | rt << 8 | imm / 4
}
fn it(firstcond: u16, mask: u16) -> u16 {
    0xBF00 | firstcond << 4 | mask
}
fn add_sp_imm(rd: u16, imm: u16) -> u16 {
    0xA800 | rd << 8 | imm / 4
}
const EQ: u16 = 0;
const NE: u16 = 1;
const COND_CS: u16 = 2;

fn bl(from: u32, to: u32) -> [u16; 2] {
    let offset = to.wrapping_sub(from.wrapping_add(4)) as i32;
    let s = ((offset >> 24) & 1) as u32;
    let i1 = ((offset >> 23) & 1) as u32;
    let i2 = ((offset >> 22) & 1) as u32;
    let imm10 = ((offset >> 12) & 0x3FF) as u32;
    let imm11 = ((offset >> 1) & 0x7FF) as u32;
    let j1 = !(i1 ^ s) & 1;
    let j2 = !(i2 ^ s) & 1;
    [(0xF000 | (s << 10) | imm10) as u16, (0xD000 | (j1 << 13) | (j2 << 11) | imm11) as u16]
}

/// 32-bit VFP encodings.
const VMOV_S0_R0: [u16; 2] = [0xEE00, 0x0A10]; // vmov s0, r0
const VMOV_R0_S0: [u16; 2] = [0xEE10, 0x0A10]; // vmov r0, s0
const VADD_S0_S0_S1: [u16; 2] = [0xEE30, 0x0A20]; // vadd.f32 s0, s0, s1
const VMUL_S2_S0_S1: [u16; 2] = [0xEE20, 0x1A20]; // vmul.f32 s2, s0, s1
const VMSR_FPSCR_R1: [u16; 2] = [0xEEE1, 0x1A10]; // vmsr fpscr, r1
const VMRS_R2_FPSCR: [u16; 2] = [0xEEF1, 0x2A10]; // vmrs r2, fpscr
const VCMP_S0_S1: [u16; 2] = [0xEEB4, 0x0A60]; // vcmp.f32 s0, s1
const VMRS_APSR: [u16; 2] = [0xEEF1, 0xFA10]; // vmrs APSR_nzcv, fpscr
const VPUSH_S16_S17: [u16; 2] = [0xED2D, 0x8A02]; // vpush {s16, s17}
const VPOP_S16_S17: [u16; 2] = [0xECBD, 0x8A02]; // vpop {s16, s17}
const VMOV_S16_S0: [u16; 2] = [0xEEB0, 0x8A40]; // vmov.f32 s16, s0
const VMOV_S0_S16: [u16; 2] = [0xEEB0, 0x0A48]; // vmov.f32 s0, s16

// ---- routines ---------------------------------------------------------------------------------------------------------------------

struct Routine {
    name: &'static str,
    code: Vec<u16>,
    core_key: u16,
    s_key: u32,
    fp: bool,
    /// Expected: the routine is memoized (some key records) / every path is unsafe.
    safe: bool,
    entry: u32,
}

fn routine(name: &'static str, slot: u32, core_key: u16, s_key: u32, fp: bool, safe: bool, build: impl FnOnce(u32) -> Vec<u16>) -> Routine {
    let entry = 0x0801_0000 + slot * 0x100;
    Routine { name, code: build(entry), core_key, s_key, fp, safe, entry }
}

fn routines() -> Vec<Routine> {
    let helper_entry = |slot: u32| 0x0801_0000 + slot * 0x100;
    vec![
        // adds r0, r0, r1; bx lr
        routine("add", 0, 0x3, 0, false, true, |_| vec![adds(0, 0, 1), bx(14)]),
        // reads the caller's r4
        routine("reads_r4", 1, 0x1, 0, false, false, |_| vec![adds(0, 0, 4), bx(14)]),
        // push {r4, lr}; movs r4,#5; adds r0, r0, r4; pop {r4, pc}
        routine("pushpop", 2, 0x1, 0, false, true, |_| vec![push(0x10, true), movs(4, 5), adds(0, 0, 4), pop(0x10, true)]),
        // ldr r0, [r1]: a load from key-determined RAM
        routine("ldr_ram", 3, 0x3, 0, false, false, |_| vec![ldr_imm(0, 1, 0), bx(14)]),
        // ldr r0, [pc, #imm] (literal in flash), adds r0, r0, r1
        routine("literal", 4, 0x2, 0, false, true, |_| vec![ldr_pc(0, 8), adds(0, 0, 1), bx(14), NOP, 0x1234, 0x5678]),
        // str r0, [r1]: a store outside the frame
        routine("store_other", 5, 0x3, 0, false, false, |_| vec![str_imm(0, 1, 0), bx(14)]),
        // bne on flags nobody produced
        routine("flags_in", 6, 0x1, 0, false, false, |entry| vec![bcc(NE, entry, entry + 6), movs(0, 1), bx(14), movs(0, 2), bx(14)]),
        // adcs r0, r1: carry in
        routine("carry_in", 7, 0x3, 0, false, false, |_| vec![adcs(0, 1), bx(14)]),
        // cmp r0, #3; ite eq; moveq r1, #1; movne r1, #2; bx lr
        routine("it_block", 8, 0x1, 0, false, true, |_| vec![cmp_imm(0, 3), it(EQ, 0xC), movs(1, 1), movs(1, 2), bx(14)]),
        // push {lr}; bl helper; pop {pc}; helper: adds r0, r0, #1; bx lr
        routine("nested", 9, 0x1, 0, false, true, move |entry| {
            let mut code = vec![push(0, true)];
            code.extend(bl(entry + 2, helper_entry(10)));
            code.push(pop(0, true));
            code
        }),
        // (the helper of `nested`)
        routine("helper", 10, 0x1, 0, false, true, |_| vec![adds_imm8(0, 1), bx(14)]),
        // add r0, sp, #4: SP used as data
        routine("sp_data", 11, 0x1, 0, false, false, |_| vec![add_sp_imm(0, 4), bx(14)]),
        // cmp r0, #0; it ne; movne r3, #7; bx lr: r3 is a pass-through when r0 == 0
        routine("conditional_write", 12, 0x1, 0, false, true, |_| vec![cmp_imm(0, 0), it(NE, 0x8), movs(3, 7), bx(14)]),
        // bx r4: a caller-supplied target
        routine("bx_r4", 13, 0x1, 0, false, false, |_| vec![bx(4)]),
        // vmov s0, r0; vadd.f32 s0, s0, s1; vmov r0, s0; bx lr (floating point, accumulates cumulative flags)
        routine("vfp_add", 14, 0x1, 0x2, true, true, |_| {
            let mut code = Vec::new();
            code.extend(VMOV_S0_R0);
            code.extend(VADD_S0_S0_S1);
            code.extend(VMOV_R0_S0);
            code.push(bx(14));
            code
        }),
        // vmsr fpscr, r1: replaces the whole FPSCR (the recording must put the held-back flags back first)
        routine("vmsr", 15, 0x2, 0x3, true, false, |_| {
            let mut code = Vec::new();
            code.extend(VADD_S0_S0_S1);
            code.extend(VMSR_FPSCR_R1);
            code.push(bx(14));
            code
        }),
        // vmrs r2, fpscr: reads the whole FPSCR
        routine("vmrs", 16, 0x1, 0x3, true, false, |_| {
            let mut code = Vec::new();
            code.extend(VMUL_S2_S0_S1);
            code.extend(VMRS_R2_FPSCR);
            code.push(bx(14));
            code
        }),
        // vcmp + vmrs APSR_nzcv + a conditional return value
        routine("vcmp", 17, 0, 0x3, true, true, |entry| {
            let mut code = Vec::new();
            code.extend(VCMP_S0_S1);
            code.extend(VMRS_APSR);
            code.push(bcc(COND_CS, entry + 10, entry + 16));
            code.push(movs(0, 1));
            code.push(bx(14));
            code.push(NOP);
            code.push(movs(0, 2));
            code.push(bx(14));
            code
        }),
        // vpush {s16, s17}; vmov s16, s0; vadd s0, s0, s1; vmov s0, s16; vpop; bx lr (callee-saved S registers)
        routine("vpushpop", 18, 0, 0x3, true, true, |_| {
            let mut code = Vec::new();
            code.extend(VPUSH_S16_S17);
            code.extend(VMOV_S16_S0);
            code.extend(VADD_S0_S0_S1);
            code.extend(VMOV_S0_S16);
            code.extend(VPOP_S16_S17);
            code.push(bx(14));
            code
        }),
        // a path of thousands of instructions: too long to record
        routine("long", 19, 0x1, 0, false, false, |entry| {
            // r1 = r0; loop: subs r1, #1; bne loop: 2 instructions per iteration, r0 iterations (the key)
            vec![mov_reg(1, 0), subs_imm8(1, 1), bcc(NE, entry + 4, entry + 2), bx(14)]
        }),
    ]
}

fn spec_table(list: &[Routine]) -> &'static [Spec] {
    let specs: Vec<Spec> = list
        .iter()
        .map(|r| {
            let bytes: Vec<u8> = r.code.iter().flat_map(|h| h.to_le_bytes()).collect();
            // The anchor reads eight bytes: pad short routines with the bytes that follow in the image (zeros).
            let mut padded = bytes.clone();
            padded.resize(padded.len().max(8), 0);
            Spec {
                name: r.name,
                len: bytes.len() as u32,
                sha256: Box::leak(sha256_hex(&sha256(&bytes)).into_boxed_str()),
                hints: Box::leak(vec![r.entry].into_boxed_slice()),
                anchor: anchor_hash(&padded[..8]),
                core_key: r.core_key,
                s_key: r.s_key,
                fp: r.fp,
            }
        })
        .collect();
    Box::leak(specs.into_boxed_slice())
}

const RET: u32 = 0x0807_0000;

struct Pair {
    plain: Harness,
    fast: Harness,
    list: Vec<Routine>,
    calls: u32,
}

fn machine(list: &[Routine], mode: RoutineAccelMode) -> Harness {
    let mut h = Harness::new();
    for r in list {
        h.bus.load_halfwords(r.entry, &r.code);
    }
    // After the routine returns the core idles in a sled.
    let mut sled = vec![NOP; 64];
    sled.push(0xE7BE);
    h.bus.load_halfwords(RET, &sled);
    h.cpu.set_idle_fast_forward(false);
    h.cpu.set_routine_specs(spec_table(list));
    h.cpu.set_routine_accel(mode);
    h.cpu.ppb_poke32(0xE000_ED88, 0xF << 20, 0);
    h.cpu.set_control(4);
    h
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32
    }
}

impl Pair {
    fn new(mode: RoutineAccelMode) -> Pair {
        let list = routines();
        Pair { plain: machine(&list, RoutineAccelMode::Off), fast: machine(&list, mode), list, calls: 0 }
    }

    /// One call of routine `name` with the given key registers; the rest of the caller state is random.
    fn call(&mut self, name: &str, rng: &mut Rng, key: [u32; 4], s: [u32; 2], ctrl: u32, budget: u64) {
        let entry = self.list.iter().find(|r| r.name == name).expect(name).entry;
        let mut regs = [0u32; 13];
        for r in regs.iter_mut() {
            *r = rng.next();
        }
        regs[..4].copy_from_slice(&key);
        if name == "bx_r4" {
            regs[4] = RET | 1; // a legitimate target supplied by the caller
        }
        let sp = SRAM1_BASE + 0x4000 + (rng.next() & 0xFF8);
        let mut sregs = [0u32; 32];
        for v in sregs.iter_mut() {
            *v = rng.next() & 0x7F7F_FFFF | 0x3000_0000;
        }
        sregs[0] = s[0];
        sregs[1] = s[1];
        // The control fields (rounding mode, flush-to-zero) are part of the key: a few combinations repeat.
        let fpscr = rng.next() & 0xF000_009F | ctrl;
        let apsr = rng.next() & 0xF800_0000;
        let mut exits = Vec::new();
        for h in [&mut self.plain, &mut self.fast] {
            for (i, &v) in regs.iter().enumerate() {
                h.cpu.set_reg(i, v);
            }
            h.cpu.set_reg(13, sp);
            h.cpu.set_reg(14, RET | 1);
            h.cpu.set_apsr(apsr);
            h.cpu.fp_regs_mut().s = sregs;
            h.cpu.fp_regs_mut().fpscr = fpscr & armv7m::vfp::fpscr::WRITE_MASK;
            h.cpu.set_pc(entry);
            let exit = h.step_once(budget);
            let mut total = exit.executed;
            let mut reason = exit.reason;
            let mut rounds = 0;
            while h.cpu.pc() & !0x3FF != RET && rounds < 8 {
                let more = h.step_once(2000);
                total += more.executed;
                reason = more.reason;
                rounds += 1;
            }
            exits.push((total, reason));
        }
        self.calls += 1;
        assert_eq!(exits[0], exits[1], "{name}: run exits (call {})", self.calls);
        assert!(matches!(exits[0].1, ExitReason::Deadline | ExitReason::StopRequested), "{name}: {:?} {:?}", exits[0], self.plain.cpu.lockup_reason());
        let regs = |h: &Harness| (0..16).map(|i| h.r(i)).collect::<Vec<_>>();
        assert_eq!(regs(&self.plain), regs(&self.fast), "{name}: registers (call {})", self.calls);
        assert_eq!(self.plain.cpu.xpsr(), self.fast.cpu.xpsr(), "{name}: xPSR");
        assert_eq!(self.plain.cpu.fp_regs(), self.fast.cpu.fp_regs(), "{name}: VFP state (call {})", self.calls);
        assert_eq!(self.plain.cpu.instructions(), self.fast.cpu.instructions(), "{name}: retire counts");
        assert_eq!(self.plain.bus.sram1, self.fast.bus.sram1, "{name}: RAM (call {})", self.calls);
        assert_eq!(self.plain.cpu.exactness_digest(), self.fast.cpu.exactness_digest(), "{name}: translation state (call {})", self.calls);
    }

    fn stats(&self, name: &str) -> armv7m::RoutineStats {
        let all = self.fast.cpu.routine_accel_stats();
        assert_eq!(all.shadow_mismatches, 0, "{:?}", all.first_mismatch);
        all.routines.into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("{name} was not found by its code hash"))
    }
}

/// Calls every routine with a few keys, each several times with different caller state, in chunks long enough to record.
fn exercise(mode: RoutineAccelMode, only: Option<&str>) -> Pair {
    let mut pair = Pair::new(mode);
    let mut rng = Rng(0x5EED);
    let names: Vec<&'static str> = pair.list.iter().map(|r| r.name).filter(|n| only.is_none_or(|o| o == *n)).collect();
    for round in 0..40u32 {
        for name in &names {
            // Four keys, two FPSCR control settings: 40 rounds repeat every combination five times.
            let key = match round % 4 {
                0 => [0, 0, 0, 0],
                1 => [3, 9, 0, 0],
                2 => [0xFFFF_FFFF, 1, 2, 3],
                _ => [0x8000_0000, 0x8000_0000, 0, 0],
            };
            let s = match round % 4 {
                0 => [0x3F80_0000, 0x4000_0000],
                1 => [0x7F7F_FFFF, 0x7F7F_FFFF],
                2 => [0x7FC0_0000, 0x3F80_0000],
                _ => [0x0000_0001, 0xBF80_0000],
            };
            let ctrl = [0u32, 0x0040_0000][((round / 4) % 2) as usize];
            let key = if *name == "long" { [if round % 2 == 0 { 1500 } else { 1400 }, 0, 0, 0] } else { key };
            // Chunks longer than the recording limit, and short ones that cut the call.
            let budget = if round % 7 == 3 { 30 } else { 2600 + u64::from(rng.next() % 3000) };
            pair.call(name, &mut rng, key, s, ctrl, budget);
        }
    }
    pair
}

/// Why the tracker must reject the (unsafe) routines of the table.
fn expected_reason(name: &str) -> &'static str {
    match name {
        "reads_r4" => "reads a caller register",
        "ldr_ram" => "load from outside flash",
        "store_other" => "instruction kind not tracked",
        "flags_in" | "carry_in" => "reads flags the path did not produce",
        "sp_data" => "SP used as a data operand",
        "bx_r4" => "branches to a caller-supplied address",
        "vmsr" | "vmrs" => "accesses the whole FPSCR",
        "long" => "path too long or interrupted",
        _ => "",
    }
}

#[test]
fn safe_routines_are_memoized_and_unsafe_ones_are_not() {
    let pair = exercise(RoutineAccelMode::On, None);
    for r in &pair.list {
        let s = pair.stats(r.name);
        if r.safe {
            assert!(s.hits > 0 && s.recorded > 0 && s.unsafe_paths == 0, "{}: expected memo hits ({s:?})", r.name);
        } else {
            assert!(s.hits == 0 && s.recorded == 0 && s.unsafe_paths > 0, "{}: must never be replayed ({s:?})", r.name);
        }
    }
}

#[test]
fn every_synthetic_routine_is_exact_in_shadow_mode() {
    let pair = exercise(RoutineAccelMode::Shadow, None);
    for r in pair.list.iter().filter(|r| r.safe) {
        let s = pair.stats(r.name);
        assert!(s.shadow_checks > 0, "{}: no shadow check ({s:?})", r.name);
    }
}

#[test]
fn unsafe_reasons_name_what_was_rejected() {
    let names: Vec<&'static str> = routines().iter().filter(|r| !r.safe).map(|r| r.name).collect();
    assert!(names.len() >= 9);
    for name in names {
        let pair = exercise(RoutineAccelMode::On, Some(name));
        let all = pair.fast.cpu.routine_accel_stats();
        let reasons: Vec<&str> = all.unsafe_reasons.iter().map(|(why, _)| *why).collect();
        assert!(reasons.contains(&expected_reason(name)), "{name}: expected {:?} among {reasons:?}", expected_reason(name));
    }
}

/// A memo entry is replayed only when the whole call fits the chunk: budgets from just below to just above the call's length
/// (the chunk then ends inside the call, exactly at its last instruction, or after it) leave exactly the interpreter's state.
#[test]
fn chunk_budgets_around_the_length_of_the_call_are_exact() {
    for name in ["add", "pushpop", "it_block", "nested", "conditional_write", "vfp_add", "vpushpop", "vcmp", "literal"] {
        let mut pair = Pair::new(RoutineAccelMode::On);
        let mut rng = Rng(11);
        let key = [3, 5, 0, 0];
        let s = [0x3F80_0000, 0x4000_0000];
        for _ in 0..5 {
            pair.call(name, &mut rng, key, s, 0, 4000);
        }
        let stats = pair.stats(name);
        assert!(stats.hits > 0, "{name}: {stats:?}");
        let length = stats.instructions_replaced / stats.hits;
        let hits_before = stats.hits;
        for budget in length.saturating_sub(3).max(1)..=length + 3 {
            pair.call(name, &mut rng, key, s, 0, budget);
        }
        let after = pair.stats(name);
        // Calls with a budget of at least the length replay; shorter ones are interpreted (and counted as over the budget).
        assert_eq!(after.hits - hits_before, 4, "{name}: length {length}: {after:?}");
        assert!(after.budget_skips >= (length - 1).min(3), "{name}: {after:?}");
    }
}

/// Micro-benchmark of the hit path on tiny routines (`--ignored --nocapture`): wall time per call with a warm memo against
/// interpretation, so that the difference is the overhead of the hit path minus the instructions it saves.
#[test]
#[ignore]
fn hit_cost() {
    for name in ["add", "pushpop", "vfp_add", "vpushpop", "it_block"] {
        let list = routines();
        let entry = list.iter().find(|r| r.name == name).unwrap().entry;
        let mut results = Vec::new();
        for mode in [RoutineAccelMode::Off, RoutineAccelMode::On] {
            let mut h = machine(&list, mode);
            let run = |h: &mut Harness, budget: u64, i: u32| {
                for r in 0..13 {
                    h.cpu.set_reg(r, 0x1234_0000 + r as u32);
                }
                h.cpu.set_reg(0, i % 4);
                h.cpu.set_reg(1, 7);
                h.cpu.set_reg(13, SRAM1_BASE + 0x4000);
                h.cpu.set_reg(14, RET | 1);
                h.cpu.fp_regs_mut().s[0] = 0x3F80_0000 + (i % 4);
                h.cpu.fp_regs_mut().s[1] = 0x4000_0000;
                h.cpu.set_pc(entry);
                h.step_once(budget);
            };
            for i in 0..16 {
                run(&mut h, 3000, i); // record
            }
            let n = 400_000u32;
            let started = std::time::Instant::now();
            for i in 0..n {
                run(&mut h, 40, i);
            }
            results.push(started.elapsed().as_secs_f64() / f64::from(n) * 1e9);
        }
        eprintln!("{name}: interpreted {:.1} ns/call, memoized {:.1} ns/call", results[0], results[1]);
    }
}

#[test]
fn the_routine_must_match_its_code_hash() {
    // A routine whose bytes differ from the table (one flipped bit) is not accelerated, and nothing else changes.
    let list = routines();
    let specs = spec_table(&list);
    let mut h = Harness::new();
    for r in &list {
        h.bus.load_halfwords(r.entry, &r.code);
    }
    let add = list.iter().find(|r| r.name == "add").unwrap();
    h.bus.poke16(add.entry, h.bus.peek16(add.entry) ^ 0x0040);
    let found = armv7m::accel::scan_specs(specs, 0x0800_0000, &h.bus.flash);
    assert!(!found.iter().any(|m| m.name == "add"), "a modified routine must not match: {found:?}");
    assert!(found.iter().any(|m| m.name == "pushpop"));
}
