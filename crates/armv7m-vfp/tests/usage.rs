//! `VfpInsn::usage` against `execute`: a metamorphic check of the register/memory/FPSCR usage tables that
//! the core's exact routine acceleration relies on.
//!
//! For thousands of random valid encodings, executed from random states:
//! * everything the instruction changes is inside its declared write sets (core registers, S registers,
//!   FPSCR parts, APSR flags, stores);
//! * anything it does not declare as read cannot influence a result (perturbing it leaves every output
//!   unchanged): core registers, S registers, the FPSCR control fields, the FPSCR flags and the cumulative
//!   exception flags (these only accumulate; no result ever depends on them);
//! * the memory access description matches the accesses `execute` makes;
//! * the declared write sets are tight: each declared S register is changed by some random execution.

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{decode, execute, FpRegs, Loc, VfpDecode, VfpExec, VfpHost, VfpInsn};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32
    }
}

#[derive(Clone, Default)]
struct Host {
    r: [u32; 16],
    fp: FpRegs,
    nzcv: u32,
    literal_base: u32,
    loads: Vec<u32>,
    stores: Vec<(u32, u32)>,
}

fn mem_value(addr: u32) -> u32 {
    let mut z = u64::from(addr).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (z ^ (z >> 27)) as u32
}

impl VfpHost for Host {
    fn reg(&self, n: u32) -> u32 {
        assert!(n <= 14, "R{n} requested");
        self.r[n as usize]
    }
    fn set_reg(&mut self, n: u32, value: u32) {
        assert!(n <= 14, "R{n} written");
        self.r[n as usize] = value;
    }
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.nzcv = nzcv & 0xF000_0000;
    }
    fn fp(&mut self) -> &mut FpRegs {
        &mut self.fp
    }
    fn load32(&mut self, addr: u32) -> u32 {
        self.loads.push(addr);
        mem_value(addr)
    }
    fn store32(&mut self, addr: u32, value: u32) {
        self.stores.push((addr, value));
    }
    fn literal_base(&self) -> u32 {
        self.literal_base
    }
}

fn random_host(rng: &mut Rng) -> Host {
    let mut h = Host::default();
    for r in h.r.iter_mut() {
        // Word-aligned bases keep most load/store encodings out of the alignment fault path.
        *r = rng.next() & !3;
    }
    for s in h.fp.s.iter_mut() {
        *s = match rng.next() % 8 {
            0 => 0,
            1 => 0x7F80_0000,
            2 => 0x7FC0_0000,
            3 => rng.next() & 0x807F_FFFF, // denormal
            _ => rng.next(),
        };
    }
    h.fp.fpscr = rng.next() & WRITE_MASK;
    h.nzcv = rng.next() & 0xF000_0000;
    h.literal_base = rng.next() & !3;
    h
}

fn decode_random(rng: &mut Rng) -> Option<VfpInsn> {
    let hw1 = 0xEC00 | (rng.next() & 0x03FF) as u16;
    let coproc = 0xA | (rng.next() & 1);
    let hw2 = ((rng.next() & 0xFFFF) as u16 & 0xF0FF) | (coproc as u16) << 8;
    match decode(hw1, hw2) {
        VfpDecode::Insn(i) => Some(i),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Outcome {
    r: [u32; 16],
    s: [u32; 32],
    fpscr: u32,
    nzcv: u32,
    loads: Vec<u32>,
    stores: Vec<(u32, u32)>,
    ok: bool,
}

fn run(insn: &VfpInsn, mut h: Host) -> (Host, Outcome) {
    let result = execute(insn, &mut h);
    let ok = matches!(result, VfpExec::Ok);
    let outcome = Outcome { r: h.r, s: h.fp.s, fpscr: h.fp.fpscr, nzcv: h.nzcv, loads: h.loads.clone(), stores: h.stores.clone(), ok };
    (h, outcome)
}

const CTRL_MASK: u32 = AHP | DN | FZ | RMODE_MASK;
const NZCV_MASK: u32 = N | Z | C | V;

/// Equality of two outcomes ignoring one core register, one S register and some FPSCR bits.
fn same_modulo(x: &Outcome, y: &Outcome, core: Option<usize>, s: Option<usize>, fpscr_bits: u32) -> bool {
    let mut x = x.clone();
    let mut y = y.clone();
    if let Some(i) = core {
        x.r[i] = 0;
        y.r[i] = 0;
    }
    if let Some(i) = s {
        x.s[i] = 0;
        y.s[i] = 0;
    }
    x.fpscr &= !fpscr_bits;
    y.fpscr &= !fpscr_bits;
    x == y
}

#[test]
fn usage_matches_execution() {
    let mut rng = Rng(0x5EED_0001);
    let mut checked = 0u32;
    let mut loose_writes = 0u32;
    let mut attempts = 0u64;
    while checked < 6000 && attempts < 20_000_000 {
        attempts += 1;
        let Some(insn) = decode_random(&mut rng) else { continue };
        let u = insn.usage();
        let base = random_host(&mut rng);
        let (after, a) = run(&insn, base.clone());
        if !a.ok {
            continue; // alignment fault: no state-change contract
        }
        checked += 1;
        let enc = insn.encoding();

        // ---- writes -----------------------------------------------------------------------------------------
        for i in 0..15 {
            if after.r[i] != base.r[i] {
                assert!(u.writes_core & (1 << i) != 0, "{enc:x?}: r{i} changed but is not a declared write ({u:?})");
            }
        }
        for i in 0..32 {
            if after.fp.s[i] != base.fp.s[i] {
                assert!(u.writes_s & (1 << i) != 0, "{enc:x?}: s{i} changed but is not a declared write ({u:?})");
            }
        }
        let fp_changed = after.fp.fpscr ^ base.fp.fpscr;
        if fp_changed & CTRL_MASK != 0 {
            assert!(u.writes_fpscr_all, "{enc:x?}: control bits changed ({u:?})");
        }
        if fp_changed & NZCV_MASK != 0 {
            assert!(u.writes_fpscr_nzcv || u.writes_fpscr_all, "{enc:x?}: FPSCR flags changed ({u:?})");
        }
        if fp_changed & FLAGS_MASK != 0 {
            assert!(u.raises_flags || u.writes_fpscr_all, "{enc:x?}: cumulative flags changed ({u:?})");
        }
        if after.nzcv != base.nzcv {
            assert!(u.writes_apsr_nzcv, "{enc:x?}: APSR flags changed ({u:?})");
        }
        assert_eq!(!after.stores.is_empty(), u.mem.is_some_and(|m| m.store), "{enc:x?}: store mismatch ({u:?})");
        assert_eq!(!after.loads.is_empty(), u.mem.is_some_and(|m| !m.store), "{enc:x?}: load mismatch ({u:?})");

        // ---- memory description ---------------------------------------------------------------------------
        if let Some(m) = u.mem {
            let base_value = if m.base == 15 { base.literal_base } else { base.r[m.base as usize] };
            let start = if m.block {
                if m.decrement_before {
                    base_value.wrapping_sub(m.offset)
                } else {
                    base_value
                }
            } else {
                base_value.wrapping_add(m.offset)
            };
            let observed: Vec<u32> = if m.store { after.stores.iter().map(|s| s.0).collect() } else { after.loads.clone() };
            let expected: Vec<u32> = (0..u32::from(m.words)).map(|i| start.wrapping_add(4 * i)).collect();
            assert_eq!(observed, expected, "{enc:x?}: addresses ({m:?})");
            if m.block {
                assert_eq!(m.offset, 4 * u32::from(m.words), "{enc:x?}: block size");
            }
            if m.writeback {
                let new_base = if m.decrement_before { start } else { base_value.wrapping_add(m.offset) };
                assert_eq!(after.r[m.base as usize], new_base, "{enc:x?}: writeback");
            }
            // The transferred S registers are `first_s ..` modulo 32 and are declared as read/written.
            let regs = (0..u32::from(m.words)).fold(0u32, |acc, i| acc | 1 << ((u32::from(m.first_s) + i) & 31));
            assert_eq!(regs, if m.store { u.reads_s } else { u.writes_s }, "{enc:x?}: transferred registers");
        }

        // ---- verbatim copies ------------------------------------------------------------------------------------------
        for (from, to) in u.moves.iter().flatten() {
            let value = |h: &Host, l: &Loc| match *l {
                Loc::Core(r) => h.r[r as usize],
                Loc::S(i) => h.fp.s[i as usize],
            };
            assert_eq!(value(&after, to), value(&base, from), "{enc:x?}: {from:?} -> {to:?} is not a bit copy");
        }

        // ---- reads (perturbation) ----------------------------------------------------------------------------------
        for i in 0..15usize {
            if u.reads_core & (1 << i) != 0 {
                continue;
            }
            let mut p = base.clone();
            p.r[i] = base.r[i] ^ (rng.next() | 4);
            let (_, b) = run(&insn, p);
            assert!(same_modulo(&a, &b, Some(i), None, 0), "{enc:x?}: r{i} influences the result but is not a declared read ({u:?})");
        }
        for i in 0..32usize {
            if u.reads_s & (1 << i) != 0 {
                continue;
            }
            let mut p = base.clone();
            p.fp.s[i] = base.fp.s[i] ^ (rng.next() | 1);
            let (_, b) = run(&insn, p);
            assert!(same_modulo(&a, &b, None, Some(i), 0), "{enc:x?}: s{i} influences the result but is not a declared read ({u:?})");
        }
        if !u.reads_fpscr_ctrl && !u.reads_fpscr_all {
            let mut p = base.clone();
            p.fp.fpscr = (base.fp.fpscr ^ (rng.next() & CTRL_MASK)) & WRITE_MASK;
            let (_, b) = run(&insn, p);
            assert!(same_modulo(&a, &b, None, None, CTRL_MASK), "{enc:x?}: control bits influence the result ({u:?})");
        }
        if !u.reads_fpscr_all {
            // No result ever depends on the FPSCR flags or on the cumulative exception flags.
            let mut p = base.clone();
            p.fp.fpscr = (base.fp.fpscr ^ (rng.next() & (NZCV_MASK | FLAGS_MASK))) & WRITE_MASK;
            let (_, b) = run(&insn, p);
            assert!(same_modulo(&a, &b, None, None, NZCV_MASK | FLAGS_MASK), "{enc:x?}: FPSCR flags influence the result ({u:?})");
        }

        // ---- tightness of the S write set --------------------------------------------------------------------------
        let mut changed = 0u32;
        for _ in 0..12 {
            let h = random_host(&mut rng);
            let (after, o) = run(&insn, h.clone());
            if o.ok {
                for i in 0..32 {
                    if after.fp.s[i] != h.fp.s[i] {
                        changed |= 1 << i;
                    }
                }
            }
        }
        if u.writes_s & !changed != 0 {
            loose_writes += 1;
        }
    }
    eprintln!("usage: {checked} encodings checked in {attempts} draws; {loose_writes} declare S writes not observed in 12 random trials");
    assert!(checked >= 3000, "too few valid encodings ({checked})");
    assert!(loose_writes * 50 <= checked, "write sets look loose: {loose_writes} of {checked}");
}

#[test]
fn every_decoded_instruction_class_has_a_usage() {
    // A smoke test over a coarse sweep of the encoding space: usage() must not panic.
    let mut rng = Rng(7);
    let mut n = 0;
    for _ in 0..500_000 {
        if let Some(i) = decode_random(&mut rng) {
            let _ = i.usage();
            n += 1;
        }
    }
    assert!(n > 1000);
}
