//! Differential tests of the routine acceleration on the real main firmware (skipped when the SREC is absent).
//!
//! Two cores run the same sequence of calls to the accelerated routines, with random arguments, edge cases and
//! random caller state (callee-saved registers, scratch registers, flags, stack pointer, return address): one with
//! acceleration `Off` (plain interpretation), one `On` or `Shadow`. Calls start either at the beginning of a chunk
//! or in the middle of one, after a random number of instructions of a stub that reaches the routine through `bl`.
//! Every call runs in a chunk of a random length (sometimes shorter than the routine, so the chunk end cuts the
//! call), sometimes with an interrupt line asserted and sometimes without a live FP context. After every call the
//! registers, flags, FPSCR, VFP registers, RAM and retire counts must be identical; at intervals the full
//! exactness digest (including the predecode cache and the cut-block history) must be.

mod common;
use armv7m::{ExitReason, RoutineAccelMode};
use common::*;

const RETS: [u32; 3] = [0x0807_0000, 0x0807_0400, 0x0807_0800];
const STUBS: u32 = 0x0808_0000;
const STUB_NOPS: u32 = 40;
const VECTORS: u32 = 0x0809_0000;
const HANDLER: u32 = 0x0809_1000;
const TEST_IRQ: u32 = 5;

/// A main-board image and the entry addresses of the accelerated routines in it.
#[derive(Clone, Copy)]
struct Image {
    srec: &'static str,
    /// `ddiv`, `f2d`, `d2f`, `unorddf2`, `isfinitef`, `expf_core`, `expf`.
    entries: [u32; 7],
}

const I_DDIV: usize = 0;
const I_F2D: usize = 1;
const I_D2F: usize = 2;
const I_UNORD: usize = 3;
const I_ISFINITE: usize = 4;
const I_EXPF_CORE: usize = 5;
const I_EXPF: usize = 6;

const TRITON: Image = Image {
    srec: "firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec",
    entries: [0x0800_486c, 0x0800_4568, 0x0800_4c08, 0x0800_4b4c, 0x0802_be20, 0x0802_cc7c, 0x0802_bc90],
};

/// NEPTUN has the same double-precision helpers at the same addresses; `isfinitef` has the same bytes elsewhere; `expf` and
/// its worker are laid out differently (they have their own hashes in the routine table).
const NEPTUN: Image = Image {
    srec: "firmware/NEPTUN-5.8-65.3/ngc_main_5.8_NEPTUN.srec",
    entries: [0x0800_486c, 0x0800_4568, 0x0800_4c08, 0x0800_4b4c, 0x0804_bfe8, 0x0804_ce44, 0x0804_be58],
};

fn load_flash(image: Image) -> Option<Vec<u8>> {
    let text = std::fs::read_to_string(repo_path(image.srec)).ok()?;
    let mut flash = vec![0u8; FLASH_SIZE];
    for (addr, data) in &parse_srec(&text) {
        let o = (*addr - FLASH_BASE) as usize;
        flash[o..o + data.len()].copy_from_slice(data);
    }
    Some(flash)
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

    fn next64(&mut self) -> u64 {
        u64::from(self.next()) << 32 | u64::from(self.next())
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

const D_EDGE: &[u64] = &[
    0,
    0x8000_0000_0000_0000,
    0x3FF0_0000_0000_0000,
    0xBFF0_0000_0000_0000,
    0x4000_0000_0000_0000,
    0x4008_0000_0000_0000,
    0x7FF0_0000_0000_0000,
    0xFFF0_0000_0000_0000,
    0x7FF8_0000_0000_0000,
    0x7FF0_0000_0000_0001,
    0xFFF8_0000_0000_1234,
    0x0000_0000_0000_0001,
    0x000F_FFFF_FFFF_FFFF,
    0x0010_0000_0000_0000,
    0x7FEF_FFFF_FFFF_FFFF,
    0x3FE6_2E42_FEFA_39EF,
    0x4059_0000_0000_0000,
    0x3FF0_0000_0000_0001,
    0x3FEF_FFFF_FFFF_FFFF,
    0x0008_0000_0000_0000,
    0x7FE0_0000_0000_0000,
    0x0020_0000_0000_0000,
];

const F_EDGE: &[u32] = &[
    0,
    0x8000_0000,
    0x3F80_0000,
    0xBF80_0000,
    0x4000_0000,
    0x7F80_0000,
    0xFF80_0000,
    0x7FC0_0000,
    0x7F80_0001,
    0xFFC0_1234,
    1,
    0x007F_FFFF,
    0x0080_0000,
    0x7F7F_FFFF,
    0x42B1_7218, // ln(FLT_MAX) region
    0x42B1_7217,
    0xC2CF_F1B5,
    0xC2CF_F1B4,
    0xC2D0_0000,
    0x3F31_7218,
    0x3380_0000,
    0x3300_0000,
    0xB300_0000,
    0x4209_0000,
];

fn random_double(rng: &mut Rng) -> u64 {
    match rng.below(8) {
        0 | 1 => D_EDGE[rng.below(D_EDGE.len() as u32) as usize],
        2 => rng.next64(),
        3 => rng.next64() & 0x800F_FFFF_FFFF_FFFF,
        4 => (rng.next64() & 0x800F_FFFF_FFFF_FFFF) | (u64::from(rng.below(4) + 0x7FC) << 52),
        _ => (rng.next64() & 0x800F_FFFF_FFFF_FFFF) | (u64::from(0x3F0 + rng.below(0x30)) << 52),
    }
}

fn random_float(rng: &mut Rng) -> u32 {
    match rng.below(6) {
        0 | 1 => F_EDGE[rng.below(F_EDGE.len() as u32) as usize],
        2 => rng.next(),
        3 => (rng.next() & 0x807F_FFFF) | (rng.below(3) << 23),
        _ => (rng.next() & 0x807F_FFFF) | ((0x70 + rng.below(0x20)) << 23),
    }
}

/// Thumb-2 `BL` from `from` to `to`.
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

fn stub_address(entry_index: usize, stub: usize) -> u32 {
    STUBS + ((entry_index * 3 + stub) as u32) * 0x400
}

struct Machine {
    h: Harness,
}

fn machine(mode: RoutineAccelMode, flash: &[u8], entries: &[u32; 7]) -> Machine {
    let mut h = Harness::new();
    h.bus.flash.copy_from_slice(flash);
    for ret in RETS {
        // 64 NOPs and a branch back to the first: after the routine returns, the core idles in a loop.
        let mut sled = vec![0xBF00u16; 64];
        sled.push(0xE7BE);
        h.bus.load_halfwords(ret, &sled);
    }
    // Call stubs: NOPs, `bl entry`, then a sled like the above (the branch targets the first NOP after the call).
    for (ei, &entry) in entries.iter().enumerate() {
        for stub in 0..3 {
            let base = stub_address(ei, stub);
            let mut code = vec![0xBF00u16; STUB_NOPS as usize];
            let call_at = base + 2 * STUB_NOPS;
            code.extend(bl(call_at, entry));
            code.extend(vec![0xBF00u16; 64]);
            code.push(0xE7BE);
            h.bus.load_halfwords(base, &code);
        }
    }
    // Vector table with one trivial handler (`bx lr`) for the test interrupt.
    for i in 0..(16 + 32) {
        h.bus.poke32(VECTORS + 4 * i, HANDLER | 1);
    }
    h.bus.load_halfwords(HANDLER, &[0x4770]);
    h.cpu.set_vtor(VECTORS);
    h.cpu.set_idle_fast_forward(false);
    h.cpu.set_routine_accel(mode);
    h.cpu.ppb_poke32(0xE000_ED88, 0xF << 20, 0); // CPACR: CP10/CP11 full access
    h.cpu.ppb_poke32(0xE000_E100, 1 << TEST_IRQ, 0); // NVIC: enable the test interrupt
    h.cpu.set_control(4); // FPCA: a live floating-point context
    Machine { h }
}

#[derive(Clone)]
struct Call {
    entry_index: usize,
    entry: u32,
    ret: u32,
    sp: u32,
    regs: [u32; 13],
    s: [u32; 32],
    fpscr: u32,
    apsr: u32,
    budget: u64,
    /// Reach the routine through the call stub (`Some((stub, nops to skip))`) instead of starting at its entry.
    stub: Option<(usize, u32)>,
    irq: bool,
    fpca: bool,
}

fn start(m: &mut Machine, c: &Call) {
    let h = &mut m.h;
    for (i, &v) in c.regs.iter().enumerate() {
        h.cpu.set_reg(i, v);
    }
    h.cpu.set_reg(13, c.sp);
    h.cpu.set_reg(14, c.ret | 1);
    h.cpu.set_apsr(c.apsr);
    h.cpu.set_control(if c.fpca { 4 } else { 0 });
    h.cpu.fp_regs_mut().s = c.s;
    h.cpu.fp_regs_mut().fpscr = c.fpscr;
    match c.stub {
        Some((stub, skip)) => h.cpu.set_pc(stub_address(c.entry_index, stub) + 2 * skip),
        None => h.cpu.set_pc(c.entry),
    }
    if c.irq {
        h.cpu.set_irq_line(TEST_IRQ, true);
    }
}

fn in_sled(pc: u32, entry_index: usize) -> bool {
    RETS.iter().any(|r| (pc & !0x3FF) == *r) || (0..3).any(|s| pc >= stub_address(entry_index, s) + 2 * STUB_NOPS + 4 && pc < stub_address(entry_index, s) + 0x400)
}

fn state(m: &Machine) -> (Vec<u32>, Vec<u32>, u64, u64, u32, u32) {
    let h = &m.h;
    let regs: Vec<u32> = (0..16).map(|i| h.r(i)).collect();
    let fp = h.cpu.fp_regs();
    let mut v = fp.s.to_vec();
    v.push(fp.fpscr);
    // RAM: hash of SRAM1 (the stack lives there).
    let mut hsh = 0xCBF2_9CE4_8422_2325u64;
    for chunk in h.bus.sram1.chunks_exact(8) {
        hsh = (hsh ^ u64::from_le_bytes(chunk.try_into().unwrap())).wrapping_mul(0x0000_0100_0000_01B3);
        hsh ^= hsh >> 31;
    }
    (regs, v, h.cpu.instructions(), hsh, h.cpu.xpsr(), h.cpu.control())
}

struct Reference {
    plain: Machine,
    fast: Machine,
    rng: Rng,
    calls: u32,
    entries: [u32; 7],
}

impl Reference {
    fn new(flash: &[u8], image: Image, mode: RoutineAccelMode, seed: u64) -> Reference {
        Reference { plain: machine(RoutineAccelMode::Off, flash, &image.entries), fast: machine(mode, flash, &image.entries), rng: Rng(seed), calls: 0, entries: image.entries }
    }

    fn random_call(&mut self, entry: u32) -> Call {
        let entries = self.entries;
        let rng = &mut self.rng;
        let mut regs = [0u32; 13];
        for r in regs.iter_mut() {
            *r = rng.next();
        }
        let mut s = [0u32; 32];
        for v in s.iter_mut() {
            *v = random_float(rng);
        }
        // Realistic stack pointers (8-byte aligned, well inside SRAM1) with some variety.
        let sp = SRAM1_BASE + 0x2000 + (rng.below(0x1000) & !7);
        let fpscr = (rng.next() & 0xF000_009F) | if rng.below(4) == 0 { rng.next() & 0x07C0_0000 } else { 0 };
        let entry_index = entries.iter().position(|&e| e == entry).expect("known entry");
        let stub = if rng.below(2) == 0 { Some((rng.below(3) as usize, rng.below(STUB_NOPS))) } else { None };
        Call {
            entry_index,
            entry,
            ret: RETS[rng.below(3) as usize],
            sp,
            regs,
            s,
            fpscr: fpscr & armv7m::vfp::fpscr::WRITE_MASK,
            apsr: rng.next() & 0xF80F_0000,
            budget: 0,
            stub,
            irq: rng.below(12) == 0,
            fpca: rng.below(12) != 0,
        }
    }

    /// Runs `call` on both machines and compares everything.
    fn run(&mut self, mut call: Call) {
        call.budget = match self.rng.below(10) {
            0 => 20 + u64::from(self.rng.below(500)),
            _ => 2200 + u64::from(self.rng.below(7000)),
        };
        // Identical chunk structure on both cores: one chunk of `budget` instructions, then (when the call was cut
        // by the chunk end or an interrupt) further chunks until the routine has returned into its sled.
        let mut exits = Vec::new();
        for m in [&mut self.plain, &mut self.fast] {
            start(m, &call);
            let exit = m.h.step_once(call.budget);
            if call.irq {
                m.h.cpu.set_irq_line(TEST_IRQ, false);
            }
            let mut total = exit.executed;
            let mut now = exit.now;
            let mut reason = exit.reason;
            let mut rounds = 0;
            while !in_sled(m.h.cpu.pc(), call.entry_index) && rounds < 8 {
                let more = m.h.step_once(2000);
                total += more.executed;
                now = more.now;
                reason = more.reason;
                rounds += 1;
            }
            exits.push((total, reason, now));
        }
        self.calls += 1;
        assert_eq!(exits[0], exits[1], "run exits diverge (call {}, entry {:#x})", self.calls, call.entry);
        assert!(
            matches!(exits[0].1, ExitReason::Deadline | ExitReason::StopRequested),
            "{:?} on call {} entry {:#x} args {:x?} sp {:#x} lr {:#x} budget {} lockup {:?}",
            exits[0],
            self.calls,
            call.entry,
            &call.regs[..4],
            call.sp,
            call.ret,
            call.budget,
            self.plain.h.cpu.lockup_reason()
        );
        let (a, b) = (state(&self.plain), state(&self.fast));
        let ctx = format!("call {}, entry {:#x}, budget {}, stub {:?}, irq {}, fpca {}", self.calls, call.entry, call.budget, call.stub, call.irq, call.fpca);
        assert_eq!(a.0, b.0, "registers diverge ({ctx})");
        assert_eq!(a.1, b.1, "VFP state diverges ({ctx})");
        assert_eq!(a.2, b.2, "retire counts ({ctx})");
        assert_eq!(a.3, b.3, "RAM diverges ({ctx})");
        assert_eq!((a.4, a.5), (b.4, b.5), "xPSR/CONTROL ({ctx})");
        if self.calls % 100 == 0 {
            assert_eq!(self.plain.h.cpu.exactness_digest(), self.fast.h.cpu.exactness_digest(), "translation/architectural digest diverges after {ctx}");
        }
    }

    fn finish(&self, what: &str) {
        assert_eq!(self.plain.h.cpu.exactness_digest(), self.fast.h.cpu.exactness_digest(), "{what}: final digest");
        let stats = self.fast.h.cpu.routine_accel_stats();
        eprintln!("{what}: {} calls; hits {} (replaced {} instructions); shadow checks {} mismatches {} {:?}", self.calls, stats.hits(), stats.instructions_replaced(), stats.shadow_checks, stats.shadow_mismatches, stats.first_mismatch);
        for r in stats.routines.iter().filter(|r| r.misses + r.hits + r.shadow_checks > 0) {
            eprintln!("    {:10} entry {:#010x}: hits {} misses {} recorded {} unsafe {} budget-skips {} declined {} memos {} shadow {}", r.name, r.entry, r.hits, r.misses, r.recorded, r.unsafe_paths, r.budget_skips, r.declined, r.memo_entries, r.shadow_checks);
        }
        eprintln!("    unsafe reasons: {:?}", stats.unsafe_reasons);
        assert_eq!(stats.shadow_mismatches, 0, "{:?}", stats.first_mismatch);
    }
}

/// A small pool of keys so that every key is seen several times (miss, record, hits).
fn run_pool(r: &mut Reference, entry: u32, pool: &[[u32; 4]], s0_pool: &[u32], calls: u32) {
    for _ in 0..calls {
        let mut c = r.random_call(entry);
        let which = r.rng.below(pool.len() as u32) as usize;
        c.regs[..4].copy_from_slice(&pool[which]);
        if !s0_pool.is_empty() {
            c.s[0] = s0_pool[r.rng.below(s0_pool.len() as u32) as usize];
        }
        r.run(c);
    }
}

fn double_pairs(rng: &mut Rng, n: usize) -> Vec<[u32; 4]> {
    (0..n)
        .map(|_| {
            let (a, b) = (random_double(rng), random_double(rng));
            [a as u32, (a >> 32) as u32, b as u32, (b >> 32) as u32]
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Args {
    DoublePair,
    Double,
    Float,
    SoftFloat,
}

fn check_routine(image: Image, index: usize, name: &str, mode: RoutineAccelMode, keys: usize, calls: u32, args: Args) {
    let Some(flash) = load_flash(image) else {
        eprintln!("skipping {name}: firmware not available");
        return;
    };
    let entry = image.entries[index];
    let mut r = Reference::new(&flash, image, mode, 0xACCE_0000 ^ u64::from(entry));
    let mut rng = Rng(99 ^ u64::from(entry));
    let (pool, s0): (Vec<[u32; 4]>, Vec<u32>) = match args {
        Args::DoublePair => (double_pairs(&mut rng, keys), Vec::new()),
        Args::Double => (
            (0..keys)
                .map(|_| {
                    let a = random_double(&mut rng);
                    [a as u32, (a >> 32) as u32, 0, 0]
                })
                .collect(),
            Vec::new(),
        ),
        Args::SoftFloat => ((0..keys).map(|_| [random_float(&mut rng), 0, 0, 0]).collect(), Vec::new()),
        Args::Float => (vec![[0; 4]], (0..keys).map(|_| random_float(&mut rng)).collect()),
    };
    run_pool(&mut r, entry, &pool, &s0, calls);
    r.finish(name);
    let stats = r.fast.h.cpu.routine_accel_stats();
    let me = stats.routines.iter().find(|x| x.entry == entry).unwrap_or_else(|| panic!("{name} not found in the image"));
    assert!(me.hits > 0 || me.shadow_checks > 0, "{name}: no memo hit in {calls} calls ({me:?})");
}

#[test]
fn ddiv_matches_interpretation() {
    check_routine(TRITON, I_DDIV, "ddiv", RoutineAccelMode::On, 120, 6000, Args::DoublePair);
}

#[test]
fn ddiv_shadow_mode() {
    check_routine(TRITON, I_DDIV, "ddiv (shadow)", RoutineAccelMode::Shadow, 120, 4000, Args::DoublePair);
}

#[test]
fn f2d_matches_interpretation() {
    check_routine(TRITON, I_F2D, "f2d", RoutineAccelMode::On, 60, 4000, Args::SoftFloat);
}

#[test]
fn d2f_matches_interpretation() {
    check_routine(TRITON, I_D2F, "d2f", RoutineAccelMode::On, 200, 5000, Args::Double);
}

#[test]
fn unord_matches_interpretation() {
    check_routine(TRITON, I_UNORD, "unorddf2", RoutineAccelMode::On, 100, 4000, Args::DoublePair);
}

#[test]
fn isfinite_matches_interpretation() {
    check_routine(TRITON, I_ISFINITE, "isfinitef", RoutineAccelMode::On, 40, 3000, Args::Float);
}

#[test]
fn expf_core_matches_interpretation() {
    check_routine(TRITON, I_EXPF_CORE, "expf_core", RoutineAccelMode::On, 80, 4000, Args::Float);
}

#[test]
fn expf_matches_interpretation() {
    check_routine(TRITON, I_EXPF, "expf", RoutineAccelMode::On, 80, 4000, Args::Float);
}

#[test]
fn expf_shadow_mode() {
    check_routine(TRITON, I_EXPF, "expf (shadow)", RoutineAccelMode::Shadow, 60, 3000, Args::Float);
}

/// The routines are found by the SHA-256 of their code bytes: all seven in both main images, at the expected addresses, and
/// not when a byte of the routine differs.
#[test]
fn routines_are_identified_by_their_code_bytes() {
    for (image, name) in [(TRITON, "TRITON"), (NEPTUN, "NEPTUN")] {
        let Some(flash) = load_flash(image) else {
            eprintln!("skipping {name}: firmware not available");
            continue;
        };
        let found = armv7m::accel::scan(0x0800_0000, &flash);
        let mut entries: Vec<u32> = found.iter().map(|m| m.entry).collect();
        entries.sort_unstable();
        let mut expected = image.entries.to_vec();
        expected.sort_unstable();
        assert_eq!(entries, expected, "{name}: {found:?}");
        // The scan does not rely on the addresses: shift every routine by relocating the image one page up.
        let mut moved = vec![0u8; flash.len()];
        moved[0x1000..].copy_from_slice(&flash[..flash.len() - 0x1000]);
        let found_moved = armv7m::accel::scan(0x0800_0000, &moved);
        let mut shifted: Vec<u32> = found_moved.iter().map(|m| m.entry).collect();
        shifted.sort_unstable();
        // Position-independent routines (no absolute literals, no calls out of the body) survive the move; the others do not.
        assert!(shifted.iter().all(|e| expected.contains(&(e - 0x1000))), "{name}: {found_moved:?}");
        assert!(found_moved.iter().any(|m| m.name == "ddiv"), "{name}: ddiv is position independent");
        // A routine with one flipped bit is not accelerated.
        for m in &found {
            let mut broken = flash.clone();
            let at = (m.entry - 0x0800_0000) as usize + 3;
            broken[at] ^= 0x01;
            let after = armv7m::accel::scan(0x0800_0000, &broken);
            assert!(!after.iter().any(|x| x.entry == m.entry), "{name}: {} still matches with a flipped bit", m.name);
        }
    }
}

/// Micro-benchmark of the memo hit path (run with `--ignored --nocapture`): wall time per call of each routine with a warm memo
/// against plain interpretation.
#[test]
#[ignore]
fn hit_path_cost() {
    let Some(flash) = load_flash(TRITON) else { return };
    for (index, name, args) in [(I_DDIV, "ddiv", Args::DoublePair), (I_F2D, "f2d", Args::SoftFloat), (I_D2F, "d2f", Args::Double), (I_UNORD, "unorddf2", Args::DoublePair), (I_EXPF, "expf", Args::Float)] {
        let entry = TRITON.entries[index];
        let mut rng = Rng(5);
        let (pool, s0): (Vec<[u32; 4]>, Vec<u32>) = match args {
            Args::DoublePair => (double_pairs(&mut rng, 8), Vec::new()),
            Args::Double => ((0..8).map(|_| { let a = random_double(&mut rng); [a as u32, (a >> 32) as u32, 0, 0] }).collect(), Vec::new()),
            Args::SoftFloat => ((0..8).map(|_| [random_float(&mut rng), 0, 0, 0]).collect(), Vec::new()),
            Args::Float => (vec![[0; 4]], (0..8).map(|_| random_float(&mut rng)).collect()),
        };
        let mut times = Vec::new();
        for mode in [RoutineAccelMode::Off, RoutineAccelMode::On] {
            let mut m = machine(mode, &flash, &TRITON.entries);
            let n = 100_000u32;
            let mut executed = 0u64;
            let mut started = std::time::Instant::now();
            for i in 0..n + 40 {
                if i == 40 {
                    started = std::time::Instant::now(); // warm-up: keys recorded
                    executed = m.h.cpu.instructions();
                }
                let k = (i as usize) % pool.len();
                for r in 0..13 {
                    m.h.cpu.set_reg(r, 0x1234_0000 + r as u32);
                }
                m.h.cpu.set_reg(0, pool[k][0]);
                m.h.cpu.set_reg(1, pool[k][1]);
                m.h.cpu.set_reg(2, pool[k][2]);
                m.h.cpu.set_reg(3, pool[k][3]);
                m.h.cpu.set_reg(13, SRAM1_BASE + 0x4000);
                m.h.cpu.set_reg(14, RETS[0] | 1);
                if !s0.is_empty() {
                    m.h.cpu.fp_regs_mut().s[0] = s0[k % s0.len()];
                }
                m.h.cpu.set_pc(entry);
                m.h.step_once(2600);
            }
            let wall = started.elapsed().as_secs_f64();
            let instr = (m.h.cpu.instructions() - executed) as f64 / f64::from(n);
            times.push((wall / f64::from(n) * 1e9, instr));
        }
        eprintln!("{name}: plain {:.0} ns/call, accelerated {:.0} ns/call (both include {:.0} sled instructions)", times[0].0, times[1].0, times[0].1 - 0.0);
    }
}

/// The NEPTUN main image has the same routines (identified by their own code bytes, at their own addresses).
#[test]
fn neptun_routines_match_interpretation() {
    check_routine(NEPTUN, I_DDIV, "neptun ddiv", RoutineAccelMode::On, 100, 3000, Args::DoublePair);
    check_routine(NEPTUN, I_F2D, "neptun f2d", RoutineAccelMode::On, 50, 2000, Args::SoftFloat);
    check_routine(NEPTUN, I_D2F, "neptun d2f", RoutineAccelMode::On, 100, 2500, Args::Double);
    check_routine(NEPTUN, I_UNORD, "neptun unorddf2", RoutineAccelMode::On, 80, 2000, Args::DoublePair);
    check_routine(NEPTUN, I_ISFINITE, "neptun isfinitef", RoutineAccelMode::On, 40, 2000, Args::Float);
    check_routine(NEPTUN, I_EXPF_CORE, "neptun expf_core", RoutineAccelMode::On, 80, 3000, Args::Float);
    check_routine(NEPTUN, I_EXPF, "neptun expf", RoutineAccelMode::On, 80, 3000, Args::Float);
    check_routine(NEPTUN, I_EXPF, "neptun expf (shadow)", RoutineAccelMode::Shadow, 50, 2000, Args::Float);
}
