//! Execution tests through a mock `VfpHost`: loads/stores, moves, VMRS/VMSR,
//! VPUSH/VPOP, alignment faults, and end-to-end arithmetic/conversion
//! instructions assembled from the Arm ARM encoding diagrams.
//!
//! The mini assembler below is written from the encoding diagrams (not from
//! the decoder's tables) so that decode and execute are exercised against an
//! independent description of the instruction set.

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{decode, execute, FpRegs, VfpDecode, VfpExec, VfpFault, VfpHost};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Mock host
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Host {
    r: [u32; 16],
    fp: FpRegs,
    nzcv: u32,
    mem: HashMap<u32, u32>,
    literal_base: u32,
    /// ("L"|"S", address, value) in access order.
    log: Vec<(char, u32, u32)>,
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
        let v = *self.mem.get(&addr).unwrap_or(&0);
        self.log.push(('L', addr, v));
        v
    }
    fn store32(&mut self, addr: u32, value: u32) {
        self.log.push(('S', addr, value));
        self.mem.insert(addr, value);
    }
    fn literal_base(&self) -> u32 {
        self.literal_base
    }
}

fn exec(h: &mut Host, enc: (u16, u16)) -> VfpExec {
    match decode(enc.0, enc.1) {
        VfpDecode::Insn(i) => execute(&i, h),
        other => panic!("{:04x} {:04x} decoded as {:?}", enc.0, enc.1, other),
    }
}

use common::asm::*;


const ONE: u32 = 0x3F80_0000;
const TWO: u32 = 0x4000_0000;
const THREE: u32 = 0x4040_0000;

fn host_with_s(vals: &[(usize, u32)]) -> Host {
    let mut h = Host::default();
    for &(i, v) in vals {
        h.fp.s[i] = v;
    }
    h
}

// ---------------------------------------------------------------------------
// Loads and stores
// ---------------------------------------------------------------------------

#[test]
fn vldr_vstr_single() {
    let mut h = Host::default();
    h.r[0] = 0x2000_0100;
    h.mem.insert(0x2000_0134, 0x3F80_0000);
    assert_eq!(exec(&mut h, vldr(3, 0, 0x34)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], 0x3F80_0000);
    assert_eq!(h.log, vec![('L', 0x2000_0134, 0x3F80_0000)]);
    h.fp.s[31] = 0xDEAD_BEEF;
    assert_eq!(exec(&mut h, vstr(31, 0, -4)), VfpExec::Ok);
    assert_eq!(h.mem[&0x2000_00FC], 0xDEAD_BEEF);
    // Loads and stores do not touch FPSCR or core registers.
    assert_eq!(h.fp.fpscr, 0);
    assert_eq!(h.r[0], 0x2000_0100);
}

#[test]
fn vldr_literal_uses_aligned_pc() {
    let mut h = Host::default();
    h.literal_base = 0x0800_1000;
    h.mem.insert(0x0800_1008, 0x4048_F5C3);
    h.mem.insert(0x0800_0FF0, 0x1234_5678);
    // vldr s14,[pc,#8] and vldr s15,[pc,#-0x10]
    assert_eq!(exec(&mut h, vldr(14, 15, 8)), VfpExec::Ok);
    assert_eq!(exec(&mut h, vldr(15, 15, -0x10)), VfpExec::Ok);
    assert_eq!(h.fp.s[14], 0x4048_F5C3);
    assert_eq!(h.fp.s[15], 0x1234_5678);
}

#[test]
fn vldr_vstr_double_word_order() {
    let mut h = Host::default();
    h.r[1] = 0x2000_0000;
    h.mem.insert(0x2000_0010, 0x1111_1111);
    h.mem.insert(0x2000_0014, 0x2222_2222);
    assert_eq!(exec(&mut h, vldr_d(8, 1, 0x10)), VfpExec::Ok);
    // D8 = S16 (low word, first in memory) : S17 (high word)
    assert_eq!(h.fp.s[16], 0x1111_1111);
    assert_eq!(h.fp.s[17], 0x2222_2222);
    assert_eq!(h.log, vec![('L', 0x2000_0010, 0x1111_1111), ('L', 0x2000_0014, 0x2222_2222)]);
    assert_eq!(exec(&mut h, vstr_d(8, 1, 0x20)), VfpExec::Ok);
    assert_eq!(h.mem[&0x2000_0020], 0x1111_1111);
    assert_eq!(h.mem[&0x2000_0024], 0x2222_2222);
    // D15 as the last register.
    h.fp.s[30] = 7;
    h.fp.s[31] = 9;
    assert_eq!(exec(&mut h, vstr_d(15, 1, 0)), VfpExec::Ok);
    assert_eq!((h.mem[&0x2000_0000], h.mem[&0x2000_0004]), (7, 9));
}

#[test]
fn vldm_vstm_increment_after_with_and_without_writeback() {
    let mut h = Host::default();
    h.r[2] = 0x2000_0200;
    for i in 0..4 {
        h.fp.s[4 + i] = 0xA0 + i as u32;
    }
    // vstmia r2, {s4-s7}: no writeback
    assert_eq!(exec(&mut h, vstm(4, 4, 2, false, false)), VfpExec::Ok);
    assert_eq!(h.r[2], 0x2000_0200);
    for i in 0..4u32 {
        assert_eq!(h.mem[&(0x2000_0200 + 4 * i)], 0xA0 + i);
    }
    // vldmia r2!, {s20-s23}: writeback by 16
    assert_eq!(exec(&mut h, vldm(20, 4, 2, false, true)), VfpExec::Ok);
    assert_eq!(h.r[2], 0x2000_0210);
    for i in 0..4usize {
        assert_eq!(h.fp.s[20 + i], 0xA0 + i as u32);
    }
    // Ascending address order of the transfers.
    let addrs: Vec<u32> = h.log.iter().filter(|e| e.0 == 'L').map(|e| e.1).collect();
    assert_eq!(addrs, vec![0x2000_0200, 0x2000_0204, 0x2000_0208, 0x2000_020C]);
}

#[test]
fn vstm_decrement_before_writes_below_base() {
    let mut h = Host::default();
    h.r[3] = 0x2000_0300;
    h.fp.s[0] = 1;
    h.fp.s[1] = 2;
    h.fp.s[2] = 3;
    // vstmdb r3!, {s0-s2}: stores at base-12 .. base-4, base -= 12
    assert_eq!(exec(&mut h, vstm(0, 3, 3, true, true)), VfpExec::Ok);
    assert_eq!(h.r[3], 0x2000_02F4);
    assert_eq!(h.mem[&0x2000_02F4], 1);
    assert_eq!(h.mem[&0x2000_02F8], 2);
    assert_eq!(h.mem[&0x2000_02FC], 3);
    assert!(!h.mem.contains_key(&0x2000_0300));
    // vldmia r3!, {s8-s10} restores them and the base.
    assert_eq!(exec(&mut h, vldm(8, 3, 3, false, true)), VfpExec::Ok);
    assert_eq!(h.r[3], 0x2000_0300);
    assert_eq!((h.fp.s[8], h.fp.s[9], h.fp.s[10]), (1, 2, 3));
}

#[test]
fn vpush_vpop_double_registers_use_sp() {
    let mut h = Host::default();
    h.r[13] = 0x2000_8000;
    h.fp.s[16] = 0x1000_0001; // D8 low
    h.fp.s[17] = 0x1000_0002; // D8 high
    h.fp.s[18] = 0x2000_0001; // D9 low
    h.fp.s[19] = 0x2000_0002; // D9 high
    // vpush {d8,d9}: sp -= 16, D8 at the lowest address.
    assert_eq!(exec(&mut h, vpush_d(8, 2)), VfpExec::Ok);
    assert_eq!(h.r[13], 0x2000_7FF0);
    assert_eq!(h.mem[&0x2000_7FF0], 0x1000_0001);
    assert_eq!(h.mem[&0x2000_7FF4], 0x1000_0002);
    assert_eq!(h.mem[&0x2000_7FF8], 0x2000_0001);
    assert_eq!(h.mem[&0x2000_7FFC], 0x2000_0002);
    // Clobber, then vpop {d8,d9}.
    for i in 16..20 {
        h.fp.s[i] = 0;
    }
    assert_eq!(exec(&mut h, vpop_d(8, 2)), VfpExec::Ok);
    assert_eq!(h.r[13], 0x2000_8000);
    assert_eq!(&h.fp.s[16..20], &[0x1000_0001, 0x1000_0002, 0x2000_0001, 0x2000_0002]);
    // vpush of a single double register saves S16/S17 (the firmware prologue).
    assert_eq!(exec(&mut h, vpush_d(8, 1)), VfpExec::Ok);
    assert_eq!(h.r[13], 0x2000_7FF8);
}

#[test]
fn alignment_faults_leave_state_untouched() {
    let mut h = Host::default();
    h.r[0] = 0x2000_0002;
    h.r[1] = 0x2000_0102;
    h.fp.s[0] = 0xCAFE_F00D;
    assert_eq!(exec(&mut h, vldr(1, 0, 0)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0002)));
    assert_eq!(exec(&mut h, vstr(0, 0, 4)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0006)));
    assert_eq!(exec(&mut h, vldr_d(0, 0, 0)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0002)));
    assert_eq!(exec(&mut h, vstr_d(0, 0, 0)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0002)));
    // VLDM/VSTM/VPUSH/VPOP: the first address is checked; no writeback on a fault.
    assert_eq!(exec(&mut h, vldm(0, 2, 1, false, true)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0102)));
    assert_eq!(exec(&mut h, vstm(0, 2, 1, false, true)), VfpExec::Fault(VfpFault::Unaligned(0x2000_0102)));
    h.r[13] = 0x2000_8002;
    assert_eq!(exec(&mut h, vpush_d(8, 1)), VfpExec::Fault(VfpFault::Unaligned(0x2000_7FFA)));
    assert_eq!(exec(&mut h, vpop_d(8, 1)), VfpExec::Fault(VfpFault::Unaligned(0x2000_8002)));
    assert_eq!(h.r[1], 0x2000_0102);
    assert_eq!(h.r[13], 0x2000_8002);
    assert!(h.log.is_empty(), "faulting instructions must not access memory: {:?}", h.log);
    assert_eq!(h.fp.s[1], 0);
}

// ---------------------------------------------------------------------------
// Moves and FPSCR transfers
// ---------------------------------------------------------------------------

#[test]
fn vmov_immediate_and_register() {
    let mut h = Host::default();
    // imm8 0x08 = 3.0, 0x70 = 1.0, 0xF0 = -1.0, 0x00 = 2.0, 0x7F = 1.9375, 0x80 = -2.0
    for (imm8, bits) in [
        (0x08, 0x4040_0000),
        (0x70, 0x3F80_0000),
        (0xF0, 0xBF80_0000),
        (0x00, 0x4000_0000),
        (0x7F, 0x3FF8_0000),
        (0x80, 0xC000_0000),
    ] {
        assert_eq!(exec(&mut h, vmov_imm(5, imm8)), VfpExec::Ok);
        assert_eq!(h.fp.s[5], bits, "imm8 {imm8:#x}");
    }
    h.fp.s[9] = 0x7FC0_1234; // a NaN moves unchanged and without flags
    assert_eq!(exec(&mut h, vmov_reg(2, 9)), VfpExec::Ok);
    assert_eq!(h.fp.s[2], 0x7FC0_1234);
    assert_eq!(h.fp.fpscr, 0);
    // VABS / VNEG are pure sign-bit operations (no NaN processing, no flags).
    h.fp.s[1] = 0xFF80_0001; // -SNaN
    assert_eq!(exec(&mut h, vabs(0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x7F80_0001);
    assert_eq!(exec(&mut h, vneg(3, 0)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], 0xFF80_0001);
    assert_eq!(h.fp.fpscr, 0);
}

#[test]
fn vmov_core_single_and_pairs() {
    let mut h = Host::default();
    h.r[1] = 0x1234_5678;
    h.r[2] = 0x9ABC_DEF0;
    assert_eq!(exec(&mut h, vmov_to_s(15, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[15], 0x1234_5678);
    assert_eq!(exec(&mut h, vmov_from_s(5, 15)), VfpExec::Ok);
    assert_eq!(h.r[5], 0x1234_5678);
    // Two core registers <-> two consecutive single registers.
    assert_eq!(exec(&mut h, vmov2_to_s(20, 1, 2)), VfpExec::Ok);
    assert_eq!((h.fp.s[20], h.fp.s[21]), (0x1234_5678, 0x9ABC_DEF0));
    assert_eq!(exec(&mut h, vmov2_from_s(7, 8, 20)), VfpExec::Ok);
    assert_eq!((h.r[7], h.r[8]), (0x1234_5678, 0x9ABC_DEF0));
    // Two core registers <-> doubleword register (low word first).
    assert_eq!(exec(&mut h, vmov2_to_d(3, 2, 1)), VfpExec::Ok);
    assert_eq!((h.fp.s[6], h.fp.s[7]), (0x9ABC_DEF0, 0x1234_5678));
    assert_eq!(exec(&mut h, vmov2_from_d(9, 10, 3)), VfpExec::Ok);
    assert_eq!((h.r[9], h.r[10]), (0x9ABC_DEF0, 0x1234_5678));
    // 32-bit scalar halves of D registers.
    h.r[4] = 0xAAAA_0001;
    h.r[6] = 0xBBBB_0002;
    assert_eq!(exec(&mut h, vmov32_to_scalar(10, 0, 4)), VfpExec::Ok);
    assert_eq!(exec(&mut h, vmov32_to_scalar(10, 1, 6)), VfpExec::Ok);
    assert_eq!((h.fp.s[20], h.fp.s[21]), (0xAAAA_0001, 0xBBBB_0002));
    assert_eq!(exec(&mut h, vmov32_from_scalar(11, 10, 1)), VfpExec::Ok);
    assert_eq!(h.r[11], 0xBBBB_0002);
    assert_eq!(exec(&mut h, vmov32_from_scalar(12, 10, 0)), VfpExec::Ok);
    assert_eq!(h.r[12], 0xAAAA_0001);
    // SP and LR are allowed core registers.
    h.r[13] = 0x2000_7000;
    assert_eq!(exec(&mut h, vmov_to_s(0, 13)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x2000_7000);
    assert_eq!(exec(&mut h, vmov_from_s(14, 0)), VfpExec::Ok);
    assert_eq!(h.r[14], 0x2000_7000);
}

#[test]
fn vmrs_vmsr_fpscr() {
    let mut h = Host::default();
    // Only the architecturally writable bits stick.
    h.r[0] = 0xFFFF_FFFF;
    assert_eq!(exec(&mut h, vmsr(0)), VfpExec::Ok);
    assert_eq!(h.fp.fpscr, WRITE_MASK);
    assert_eq!(WRITE_MASK, 0xF7C0_009F);
    assert_eq!(exec(&mut h, vmrs(3)), VfpExec::Ok);
    assert_eq!(h.r[3], 0xF7C0_009F);
    // VMRS APSR_nzcv, FPSCR copies the flags and leaves the rest alone.
    h.fp.fpscr = 0x6000_0013;
    assert_eq!(exec(&mut h, vmrs(15)), VfpExec::Ok);
    assert_eq!(h.nzcv, 0x6000_0000);
    // Cumulative flags can be cleared by writing zero bits.
    h.r[0] = 0;
    assert_eq!(exec(&mut h, vmsr(0)), VfpExec::Ok);
    assert_eq!(h.fp.fpscr, 0);
    // Rounding mode / FZ / DN / AHP round trip.
    h.r[0] = RMODE_MASK | FZ | DN | AHP;
    assert_eq!(exec(&mut h, vmsr(0)), VfpExec::Ok);
    assert_eq!(h.fp.fpscr, RMODE_MASK | FZ | DN | AHP);
}

// ---------------------------------------------------------------------------
// Arithmetic end to end
// ---------------------------------------------------------------------------

#[test]
fn basic_arithmetic_and_flags() {
    let mut h = host_with_s(&[(1, ONE), (2, TWO), (3, THREE)]);
    assert_eq!(exec(&mut h, vadd(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], THREE);
    assert_eq!(exec(&mut h, vsub(0, 3, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], TWO);
    assert_eq!(exec(&mut h, vmul(0, 2, 3)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x40C0_0000); // 6.0
    assert_eq!(exec(&mut h, vnmul(0, 2, 3)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0xC0C0_0000); // -6.0
    assert_eq!(h.fp.fpscr, 0);
    assert_eq!(exec(&mut h, vdiv(0, 1, 3)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x3EAA_AAAB);
    assert_eq!(h.fp.fpscr, IXC);
    h.fp.s[4] = 0x4080_0000; // 4.0
    assert_eq!(exec(&mut h, vsqrt(5, 4)), VfpExec::Ok);
    assert_eq!(h.fp.s[5], TWO);
    // Divide by zero.
    h.fp.s[6] = 0;
    assert_eq!(exec(&mut h, vdiv(7, 1, 6)), VfpExec::Ok);
    assert_eq!(h.fp.s[7], 0x7F80_0000);
    assert_eq!(h.fp.fpscr, IXC | DZC);
    // Flags are cumulative: a later exact operation does not clear them.
    assert_eq!(exec(&mut h, vadd(0, 1, 1)), VfpExec::Ok);
    assert_eq!(h.fp.fpscr, IXC | DZC);
}

#[test]
fn destination_may_alias_sources() {
    let mut h = host_with_s(&[(0, ONE), (1, TWO)]);
    assert_eq!(exec(&mut h, vadd(0, 0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], THREE);
    assert_eq!(exec(&mut h, vmul(1, 1, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[1], 0x4080_0000);
    // VMLA with d == n == m
    h.fp.s[2] = TWO;
    assert_eq!(exec(&mut h, vmla(2, 2, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[2], 0x40C0_0000); // 2 + 2*2
}

#[test]
fn multiply_accumulate_family_signs() {
    // d = 10, n = 3, m = 4 -> product 12
    let base = |d: u32| host_with_s(&[(0, d), (1, 0x4040_0000), (2, 0x4080_0000)]);
    let ten = 0x4120_0000;
    let cases: [(fn(u32, u32, u32) -> Enc, &str, u32); 8] = [
        (vmla, "vmla", 0x41B0_0000),   // 10 + 12 = 22
        (vmls, "vmls", 0xC000_0000),   // 10 - 12 = -2
        (vnmla, "vnmla", 0xC1B0_0000), // -10 - 12 = -22
        (vnmls, "vnmls", 0x4000_0000), // -10 + 12 = 2
        (vfma, "vfma", 0x41B0_0000),
        (vfms, "vfms", 0xC000_0000),
        (vfnma, "vfnma", 0xC1B0_0000),
        (vfnms, "vfnms", 0x4000_0000),
    ];
    for (f, name, expect) in cases {
        let mut h = base(ten);
        assert_eq!(exec(&mut h, f(0, 1, 2)), VfpExec::Ok);
        assert_eq!(h.fp.s[0], expect, "{name}");
        assert_eq!(h.fp.fpscr, 0, "{name}");
    }
}

#[test]
fn vmla_rounds_twice_but_vfma_rounds_once() {
    // a = b = 1 + 2^-13, c = -(1 + 2^-12): a*b = 1 + 2^-12 + 2^-26.
    let a = 0x3F80_0400;
    let c = 0xBF80_0800;
    // Separately rounded: product rounds to 1 + 2^-12, sum is exactly zero.
    let mut h = host_with_s(&[(0, c), (1, a), (2, a)]);
    assert_eq!(exec(&mut h, vmla(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0);
    assert_eq!(h.fp.fpscr, IXC);
    // Fused: the residual 2^-26 survives and the result is exact.
    let mut h = host_with_s(&[(0, c), (1, a), (2, a)]);
    assert_eq!(exec(&mut h, vfma(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x3280_0000); // 2^-26
    assert_eq!(h.fp.fpscr, 0);
    // VFMS: d - n*m with the same operands but d = +(1+2^-12) gives -2^-26.
    let mut h = host_with_s(&[(0, 0x3F80_0800), (1, a), (2, a)]);
    assert_eq!(exec(&mut h, vfms(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0xB280_0000);
}

#[test]
fn vcmp_sets_fpscr_flags_and_vmrs_transfers_them() {
    let mut h = host_with_s(&[(0, ONE), (1, TWO), (2, ONE), (3, 0x7FC0_0000), (4, 0x7F80_0001)]);
    let nzcv = |h: &Host| h.fp.fpscr & 0xF000_0000;
    assert_eq!(exec(&mut h, vcmp(0, 1, false)), VfpExec::Ok);
    assert_eq!(nzcv(&h), N); // 1.0 < 2.0
    assert_eq!(exec(&mut h, vcmp(1, 0, false)), VfpExec::Ok);
    assert_eq!(nzcv(&h), C); // 2.0 > 1.0
    assert_eq!(exec(&mut h, vcmp(0, 2, false)), VfpExec::Ok);
    assert_eq!(nzcv(&h), Z | C); // equal
    assert_eq!(h.fp.fpscr & FLAGS_MASK, 0);
    // Quiet NaN: unordered; only VCMPE (and any signaling NaN) raise IOC.
    assert_eq!(exec(&mut h, vcmp(0, 3, false)), VfpExec::Ok);
    assert_eq!(nzcv(&h), C | V);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, 0);
    assert_eq!(exec(&mut h, vcmp(0, 3, true)), VfpExec::Ok);
    assert_eq!(nzcv(&h), C | V);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IOC);
    h.fp.fpscr = 0;
    assert_eq!(exec(&mut h, vcmp(0, 4, false)), VfpExec::Ok);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IOC);
    // Compare with zero; +0 == -0.
    h.fp.fpscr = 0;
    h.fp.s[5] = 0x8000_0000;
    assert_eq!(exec(&mut h, vcmp0(5, true)), VfpExec::Ok);
    assert_eq!(nzcv(&h), Z | C);
    // VMRS APSR_nzcv copies them into the core flags.
    assert_eq!(exec(&mut h, vmrs(15)), VfpExec::Ok);
    assert_eq!(h.nzcv, Z | C);
    // The comparison result replaces the flags: it does not OR into them.
    h.fp.fpscr |= N | V;
    assert_eq!(exec(&mut h, vcmp0(0, false)), VfpExec::Ok); // 1.0 > 0
    assert_eq!(nzcv(&h), C);
}

#[test]
fn conversions_through_execute() {
    let mut h = Host::default();
    // float -> int (round toward zero), signed and unsigned
    h.fp.s[1] = 0x406C_CCCD; // 3.7
    assert_eq!(exec(&mut h, vcvt_s32_f32(0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 3);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IXC);
    h.fp.fpscr = 0;
    h.fp.s[1] = 0xC06C_CCCD; // -3.7
    assert_eq!(exec(&mut h, vcvt_s32_f32(0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0xFFFF_FFFD);
    h.fp.fpscr = 0;
    assert_eq!(exec(&mut h, vcvt_u32_f32(0, 1)), VfpExec::Ok); // negative -> 0 with IOC
    assert_eq!(h.fp.s[0], 0);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IOC);
    // int -> float
    h.fp.fpscr = 0;
    h.fp.s[2] = 0xFFFF_FFFF;
    assert_eq!(exec(&mut h, vcvt_f32_s32(3, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], 0xBF80_0000); // -1.0
    assert_eq!(exec(&mut h, vcvt_f32_u32(3, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], 0x4F80_0000); // 4294967296.0 (rounded)
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IXC);
    // VCVTR honors FPSCR.RMode: 2.5 -> RN 2, RP 3, RM 2, RZ 2
    for (rm, expect) in [(0, 2), (1, 3), (2, 2), (3, 2)] {
        h.fp.fpscr = rm << RMODE_SHIFT;
        h.fp.s[4] = 0x4020_0000; // 2.5
        assert_eq!(exec(&mut h, vcvtr_s32_f32(5, 4)), VfpExec::Ok);
        assert_eq!(h.fp.s[5], expect, "rm {rm}");
        assert_eq!(exec(&mut h, vcvtr_u32_f32(5, 4)), VfpExec::Ok);
        assert_eq!(h.fp.s[5], expect, "rm {rm} unsigned");
    }
    // ... while VCVT always truncates, whatever RMode says.
    h.fp.fpscr = 1 << RMODE_SHIFT;
    h.fp.s[4] = 0x4020_0000;
    assert_eq!(exec(&mut h, vcvt_s32_f32(5, 4)), VfpExec::Ok);
    assert_eq!(h.fp.s[5], 2);
}

#[test]
fn fixed_point_conversions_through_execute() {
    let mut h = Host::default();
    // float -> s32 with 8 fraction bits: 1.5 -> 384
    h.fp.s[0] = 0x3FC0_0000;
    assert_eq!(exec(&mut h, vcvt_fixed(0, true, false, true, 8)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 384);
    // and back
    assert_eq!(exec(&mut h, vcvt_fixed(0, false, false, true, 8)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x3FC0_0000);
    // float -> s16 saturates (and is sign-extended in the register)
    h.fp.s[1] = 0x4348_0000; // 200.0
    assert_eq!(exec(&mut h, vcvt_fixed(1, true, false, false, 8)), VfpExec::Ok);
    assert_eq!(h.fp.s[1], 0x7FFF);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, IOC);
    h.fp.fpscr = 0;
    h.fp.s[1] = 0xC348_0000; // -200.0
    assert_eq!(exec(&mut h, vcvt_fixed(1, true, false, false, 8)), VfpExec::Ok);
    assert_eq!(h.fp.s[1], 0xFFFF_8000);
    // u16 with 4 fraction bits: 4095.9375 -> 0xFFFF, exact
    h.fp.fpscr = 0;
    h.fp.s[2] = 0x457F_FF00; // 4095.9375
    assert_eq!(exec(&mut h, vcvt_fixed(2, true, true, false, 4)), VfpExec::Ok);
    assert_eq!(h.fp.s[2], 0xFFFF);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, 0);
    // s16 -> float uses only the low 16 bits of the register: 0xFFFE = -2, 1 fraction bit -> -1.0
    h.fp.s[3] = 0x1234_FFFE;
    assert_eq!(exec(&mut h, vcvt_fixed(3, false, false, false, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], 0xBF80_0000);
    // u32 0x8000_0000 with 31 fraction bits -> 1.0
    h.fp.s[4] = 0x8000_0000;
    assert_eq!(exec(&mut h, vcvt_fixed(4, false, true, true, 31)), VfpExec::Ok);
    assert_eq!(h.fp.s[4], 0x3F80_0000);
    // 32 fraction bits (the maximum) with a signed value
    h.fp.s[5] = 0xC000_0000; // -2^30 / 2^32 = -0.25
    assert_eq!(exec(&mut h, vcvt_fixed(5, false, false, true, 32)), VfpExec::Ok);
    assert_eq!(h.fp.s[5], 0xBE80_0000);
}

#[test]
fn half_precision_conversions_preserve_the_other_half() {
    let mut h = Host::default();
    h.fp.s[1] = ONE;
    h.fp.s[0] = 0xAAAA_BBBB;
    assert_eq!(exec(&mut h, vcvtb_f16_f32(0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0xAAAA_3C00);
    h.fp.s[1] = TWO;
    assert_eq!(exec(&mut h, vcvtt_f16_f32(0, 1)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x4000_3C00);
    // half -> single from the bottom and top halves
    assert_eq!(exec(&mut h, vcvtb_f32_f16(2, 0)), VfpExec::Ok);
    assert_eq!(h.fp.s[2], ONE);
    assert_eq!(exec(&mut h, vcvtt_f32_f16(3, 0)), VfpExec::Ok);
    assert_eq!(h.fp.s[3], TWO);
    assert_eq!(h.fp.fpscr & FLAGS_MASK, 0);
    // IEEE vs alternative format: 0x7C00 is +inf or 65536.0
    h.fp.s[4] = 0x0000_7C00;
    assert_eq!(exec(&mut h, vcvtb_f32_f16(5, 4)), VfpExec::Ok);
    assert_eq!(h.fp.s[5], 0x7F80_0000);
    h.fp.fpscr = AHP;
    assert_eq!(exec(&mut h, vcvtb_f32_f16(5, 4)), VfpExec::Ok);
    assert_eq!(h.fp.s[5], 0x4780_0000);
}

#[test]
fn fpscr_modes_apply_to_instructions() {
    // Flush-to-zero: denormal input flushed (IDC), denormal result flushed (UFC).
    let mut h = host_with_s(&[(1, 0x0000_0001), (2, ONE)]);
    h.fp.fpscr = FZ;
    assert_eq!(exec(&mut h, vadd(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], ONE);
    assert_eq!(h.fp.fpscr, FZ | IDC);
    let mut h = host_with_s(&[(1, 0x0080_0000), (2, TWO)]); // min normal / 2 = 2^-127
    h.fp.fpscr = FZ;
    assert_eq!(exec(&mut h, vdiv(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0);
    assert_eq!(h.fp.fpscr, FZ | UFC);
    // Default NaN: a NaN operand becomes 0x7FC00000.
    let mut h = host_with_s(&[(1, 0x7FC0_1234), (2, ONE)]);
    h.fp.fpscr = DN;
    assert_eq!(exec(&mut h, vadd(0, 1, 2)), VfpExec::Ok);
    assert_eq!(h.fp.s[0], 0x7FC0_0000);
    // Rounding mode: 1 + 2^-24 rounds up under RP only.
    for (rm, expect) in [(0, 0x3F80_0000), (1, 0x3F80_0001), (2, 0x3F80_0000), (3, 0x3F80_0000)] {
        let mut h = host_with_s(&[(1, ONE), (2, 0x3380_0000)]);
        h.fp.fpscr = rm << RMODE_SHIFT;
        assert_eq!(exec(&mut h, vadd(0, 1, 2)), VfpExec::Ok);
        assert_eq!(h.fp.s[0], expect, "rm {rm}");
        assert_eq!(h.fp.fpscr & FLAGS_MASK, IXC);
    }
}

// ---------------------------------------------------------------------------
// Classification of encodings
// ---------------------------------------------------------------------------

#[test]
fn undefined_and_non_vfp_encodings() {
    // Double-precision data processing is UNDEFINED on FPv4-SP.
    assert_eq!(decode(0xEE30, 0x0B00), VfpDecode::Undefined); // vadd.f64 d0,d0,d0
    assert_eq!(decode(0xEEB0, 0x0B40), VfpDecode::Undefined); // vmov.f64 d0,d0
    assert_eq!(decode(0xEEB0, 0x0BC0), VfpDecode::Undefined); // vabs.f64
    assert_eq!(decode(0xEE80, 0x0B00), VfpDecode::Undefined); // vdiv.f64
    assert_eq!(decode(0xEEB7, 0x0AC0), VfpDecode::Undefined); // vcvt.f64.f32
    assert_eq!(decode(0xEEB4, 0x0B40), VfpDecode::Undefined); // vcmp.f64
    // Armv8 additions (unconditional-prefix encodings in CP10/CP11).
    assert_eq!(decode(0xFE00, 0x0A00), VfpDecode::Undefined); // vsel
    assert_eq!(decode(0xFE80, 0x0A00), VfpDecode::Undefined); // vmaxnm / vminnm
    assert_eq!(decode(0xFEBC, 0x0A40), VfpDecode::Undefined); // vcvta
    assert_eq!(decode(0xEEB6, 0x0A40), VfpDecode::Undefined); // vrintr
    // FPSID / MVFR0 / FPEXC are not accessible through VMRS on the Cortex-M4F.
    assert_eq!(decode(0xEEF0, 0x1A10), VfpDecode::Undefined); // vmrs r1, fpsid
    assert_eq!(decode(0xEEF7, 0x1A10), VfpDecode::Undefined); // vmrs r1, mvfr0
    assert_eq!(decode(0xEEE0, 0x1A10), VfpDecode::Undefined); // vmsr fpsid, r1
    // Advanced SIMD style scalar moves / VDUP.
    assert_eq!(decode(0xEE80, 0x1B10), VfpDecode::Undefined); // vdup.32 d0, r1
    assert_eq!(decode(0xEE40, 0x1B10), VfpDecode::Undefined); // vmov.16 d0[?], r1
    assert_eq!(decode(0xEE00, 0x1B30), VfpDecode::Undefined); // vmov.8 d0[x], r1
    // Other coprocessors raise NOCP instead.
    assert_eq!(decode(0xEE10, 0x0F10), VfpDecode::NotVfp); // mrc p15
    assert_eq!(decode(0xED08, 0xE000), VfpDecode::NotVfp); // stc p0, c14, [r8]
    assert_eq!(decode(0xFC00, 0x0000), VfpDecode::NotVfp); // stc2 p0
    assert_eq!(decode(0xEC40, 0x0F00), VfpDecode::NotVfp); // mcrr p15
    // Unpredictable / unsupported forms are UNDEFINED here.
    assert_eq!(decode(0xEC9F, 0x1A01), VfpDecode::Undefined); // vldmia pc!, ...
    assert_eq!(decode(0xED0F, 0x0A01), VfpDecode::Undefined); // vstr s0,[pc,#-4]
    assert_eq!(decode(0xEC90, 0x0A00), VfpDecode::Undefined); // vldmia r0, {} (empty list)
    assert_eq!(decode(0xEC10, 0x0A00), VfpDecode::Undefined); // P=U=W=0, D=0: unallocated
    assert_eq!(decode(0xEC90, 0x1B03), VfpDecode::Undefined); // fldmx (odd word count)
    assert_eq!(decode(0xECB0, 0x0A21), VfpDecode::Undefined); // s0 + 33 registers > 32
    // Something that is not a coprocessor encoding at all.
    assert_eq!(decode(0x4770, 0x0000), VfpDecode::NotVfp);
    assert_eq!(decode(0xEFFF, 0x0A00), VfpDecode::Undefined);
}

#[test]
fn vmrs_vmsr_register_variants_decode() {
    for rt in 0..15 {
        assert!(matches!(decode(0xEEF1, 0x0A10 | (rt << 12)), VfpDecode::Insn(_)), "vmrs r{rt}");
        assert!(matches!(decode(0xEEE1, 0x0A10 | (rt << 12)), VfpDecode::Insn(_)), "vmsr r{rt}");
    }
    assert!(matches!(decode(0xEEF1, 0xFA10), VfpDecode::Insn(_))); // vmrs APSR_nzcv, fpscr
    assert_eq!(decode(0xEEE1, 0xFA10), VfpDecode::Undefined); // vmsr fpscr, pc
}
