//! Exhaustive structural checks of the decoder over the whole 32-bit
//! coprocessor encoding space, and robustness of `execute` for every kind of
//! decodable instruction (no panics, only architectural outcomes, no access to
//! R15, FPSCR reserved bits stay clear).

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{decode, disassemble, execute, FpRegs, VfpDecode, VfpExec, VfpFault, VfpHost, VfpInsn};
use common::*;

#[test]
fn insn_is_small_and_copy() {
    assert!(std::mem::size_of::<VfpInsn>() <= 16, "VfpInsn is {} bytes", std::mem::size_of::<VfpInsn>());
    fn is_copy<T: Copy>() {}
    is_copy::<VfpInsn>();
    is_copy::<VfpDecode>();
    assert!(std::mem::size_of::<VfpDecode>() <= 24);
}

fn expected_class(hw1: u16, hw2: u16) -> &'static str {
    if hw1 & 0xEC00 != 0xEC00 {
        return "notvfp";
    }
    if hw1 & 0x0300 == 0x0300 {
        return "undefined";
    }
    let coproc = (hw2 >> 8) & 0xF;
    if coproc != 10 && coproc != 11 {
        return "notvfp";
    }
    if hw1 & 0xF000 != 0xE000 {
        return "undefined";
    }
    "either"
}

#[test]
fn classification_over_the_whole_coprocessor_space() {
    // All 2048 hw1 values of the coprocessor spaces x all 65536 hw2 values in --release;
    // every 37th hw1 in debug builds.
    let step = if cfg!(debug_assertions) { 37 } else { 1 };
    let mut counts = [0u64; 3];
    let mut hw1s: Vec<u32> = (0xEC00..0xF000).chain(0xFC00..0x10000).collect();
    hw1s.extend([0x0000, 0x4770, 0xE000, 0xE7FE, 0xEB00, 0xF000, 0xF800]);
    for (i, &hw1) in hw1s.iter().enumerate() {
        if i % step != 0 {
            continue;
        }
        let hw1 = hw1 as u16;
        for hw2 in 0..=0xFFFFu16 {
            let d = decode(hw1, hw2);
            match (expected_class(hw1, hw2), d) {
                ("notvfp", VfpDecode::NotVfp) => counts[0] += 1,
                ("undefined", VfpDecode::Undefined) => counts[1] += 1,
                ("either", VfpDecode::Undefined) => counts[1] += 1,
                ("either", VfpDecode::Insn(insn)) => {
                    counts[2] += 1;
                    assert_eq!(insn.encoding(), (hw1, hw2));
                }
                (want, got) => panic!("{hw1:04x} {hw2:04x}: expected {want}, got {got:?}"),
            }
        }
    }
    println!("not-vfp {}, undefined {}, instructions {}", counts[0], counts[1], counts[2]);
    assert!(counts[2] > 5_000);
}

#[test]
fn every_decodable_instruction_disassembles() {
    let mut rng = Rng::new(1);
    let n = cases(2_000_000, 200_000);
    let mut seen = 0u64;
    for _ in 0..n {
        let hw1 = 0xE000 | 0x0C00 | (rng.next32() as u16 & 0x03FF);
        let hw2 = rng.next32() as u16 & 0xF1FF | (0x0A00 | (rng.next32() as u16 & 0x100));
        if let VfpDecode::Insn(i) = decode(hw1, hw2) {
            let text = i.disassemble();
            assert!(text.starts_with('v'), "{hw1:04x} {hw2:04x}: {text}");
            assert!(!text.contains("undefined"));
            seen += 1;
        } else {
            let t = disassemble(hw1, hw2);
            assert!(t == "undefined" || t == "(not vfp)");
        }
    }
    assert!(seen > n / 20, "only {seen} of {n} random encodings decoded");
}

// --- execute robustness -----------------------------------------------------------

struct Host {
    r: [u32; 16],
    fp: FpRegs,
    nzcv: u32,
    rng: Rng,
}

impl VfpHost for Host {
    fn reg(&self, n: u32) -> u32 {
        assert!(n <= 14, "R{n} requested through reg()");
        self.r[n as usize]
    }
    fn set_reg(&mut self, n: u32, value: u32) {
        assert!(n <= 14, "R{n} written");
        self.r[n as usize] = value;
    }
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.nzcv = nzcv;
    }
    fn fp(&mut self) -> &mut FpRegs {
        &mut self.fp
    }
    fn load32(&mut self, addr: u32) -> u32 {
        assert_eq!(addr & 3, 0, "unaligned load reached the bus");
        self.rng.next32() ^ addr
    }
    fn store32(&mut self, addr: u32, _value: u32) {
        assert_eq!(addr & 3, 0, "unaligned store reached the bus");
    }
    fn literal_base(&self) -> u32 {
        0x0800_1000
    }
}

#[test]
fn execute_never_panics_and_keeps_fpscr_reserved_bits_clear() {
    let mut rng = Rng::new(2);
    let n = cases(3_000_000, 300_000);
    let mut host = Host { r: [0; 16], fp: FpRegs::default(), nzcv: 0, rng: Rng::new(3) };
    let mut executed = 0u64;
    let mut faults = 0u64;
    for _ in 0..n {
        let hw1 = 0xEC00 | (rng.next32() as u16 & 0x03FF) & !0x0300 | ((rng.below(3) as u16) << 8);
        let hw2 = (rng.next32() as u16 & 0xF0FF) | 0x0A00 | (((rng.next32() & 1) as u16) << 8);
        let VfpDecode::Insn(insn) = decode(hw1, hw2) else { continue };
        for r in host.r.iter_mut() {
            *r = if rng.below(2) == 0 { 0x2000_0000 | (rng.next32() & 0x7FFF) } else { rng.next32() };
        }
        for s in host.fp.s.iter_mut() {
            *s = gen_f32(&mut rng);
        }
        host.fp.fpscr = rng.next32() & WRITE_MASK;
        let before_mode = host.fp.fpscr & (RMODE_MASK | FZ | DN | AHP);
        match execute(&insn, &mut host) {
            VfpExec::Ok => executed += 1,
            VfpExec::Fault(VfpFault::Unaligned(a)) => {
                assert_ne!(a & 3, 0);
                faults += 1;
            }
            VfpExec::Undefined => panic!("execute returned Undefined for {hw1:04x} {hw2:04x}"),
        }
        // Only VMSR may change the mode bits, and nothing may set reserved bits.
        assert_eq!(host.fp.fpscr & !WRITE_MASK, 0, "{hw1:04x} {hw2:04x}: reserved FPSCR bits set");
        let text = insn.disassemble();
        if !text.starts_with("vmsr") {
            assert_eq!(host.fp.fpscr & (RMODE_MASK | FZ | DN | AHP), before_mode, "{text}");
        }
    }
    println!("executed {executed}, alignment faults {faults}");
    assert!(executed > n / 10);
    assert!(faults > 0);
}
