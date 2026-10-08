//! Instruction-level differential test against the host's hardware FPU
//! (AArch64 only): random encodings (register aliasing included) are decoded
//! and executed on random register files in random FPSCR modes; the result
//! register, the NZCV copy, the cumulative flags and the untouched registers
//! must match what the hardware produces for the same operation. The
//! multiply-accumulate family (no AArch64 equivalent) is compared against
//! sequences of hardware primitives with the Arm pseudocode's sign flips.
#![cfg(target_arch = "aarch64")]

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{decode, execute, FpRegs, VfpDecode, VfpExec, VfpHost};
use common::asm::*;
use common::*;

struct Host {
    fp: FpRegs,
    nzcv: u32,
}

impl VfpHost for Host {
    fn reg(&self, n: u32) -> u32 {
        panic!("unexpected core register read R{n}")
    }
    fn set_reg(&mut self, n: u32, _v: u32) {
        panic!("unexpected core register write R{n}")
    }
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.nzcv = nzcv;
    }
    fn fp(&mut self) -> &mut FpRegs {
        &mut self.fp
    }
    fn load32(&mut self, _a: u32) -> u32 {
        panic!("unexpected load")
    }
    fn store32(&mut self, _a: u32, _v: u32) {
        panic!("unexpected store")
    }
    fn literal_base(&self) -> u32 {
        0
    }
}

fn run(enc: Enc, s: &[u32; 32], fpscr: u32) -> FpRegs {
    let mut host = Host { fp: FpRegs { s: *s, fpscr }, nzcv: 0 };
    match decode(enc.0, enc.1) {
        VfpDecode::Insn(i) => assert_eq!(execute(&i, &mut host), VfpExec::Ok, "{:04x} {:04x}", enc.0, enc.1),
        other => panic!("{:04x} {:04x} decoded as {other:?}", enc.0, enc.1),
    }
    host.fp
}

fn replace_half(old: u32, new_half: u32, top: bool) -> u32 {
    if top {
        (old & 0xFFFF) | (new_half << 16)
    } else {
        (old & 0xFFFF_0000) | new_half
    }
}

#[test]
fn instructions_match_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(77);
    let n = cases(4_000_000, 300_000);
    let mut bad = 0u64;
    let mut first = Vec::new();
    for i in 0..n {
        let kind = i % 24;
        let mut m = modes[rng.below(modes.len() as u32) as usize];
        let pool = if rng.below(3) == 0 { 4 } else { 32 };
        let (d, rn, rm) = (rng.below(pool), rng.below(pool), rng.below(pool));
        let mut s = [0u32; 32];
        for x in s.iter_mut() {
            *x = gen_f32(&mut rng);
        }
        // Make some operands related, to hit cancellation and exact cases.
        if rng.below(3) == 0 {
            s[rm as usize] = s[rn as usize] ^ (rng.next32() & 0x8000_0001);
        }
        let (sd, sn, sm) = (s[d as usize], s[rn as usize], s[rm as usize]);
        let unsigned = rng.below(2) == 0;
        let e = rng.below(2) == 0;
        let fbits = 1 + rng.below(32);
        let top = rng.below(2) == 0;

        // (encoding, expected destination value (None = unchanged), expected NZCV, expected flags, mode used)
        let (enc, dest, nzcv, flags): (Enc, Option<u32>, u32, u32) = match kind {
            0 => { let o = hw::fadd(sn, sm, m); (vadd(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            1 => { let o = hw::fsub(sn, sm, m); (vsub(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            2 => { let o = hw::fmul(sn, sm, m); (vmul(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            3 => { let o = hw::fnmul(sn, sm, m); (vnmul(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            4 => { let o = hw::fdiv(sn, sm, m); (vdiv(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            5 => { let o = hw::fsqrt(sm, m); (vsqrt(d, rm), Some(o.bits), 0, o.fpsr) }
            6 => {
                // VABS / VNEG: bitwise, never signal (even for NaNs)
                if (i / 24) % 2 == 0 {
                    (vabs(d, rm), Some(sm & 0x7FFF_FFFF), 0, 0)
                } else {
                    (vneg(d, rm), Some(sm ^ 0x8000_0000), 0, 0)
                }
            }
            7 => {
                // VMLA / VMLS / VNMLA / VNMLS (separately rounded)
                match (i / 24) % 4 {
                    0 => { let o = hw::vmla(sd, sn, sm, m); (vmla(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    1 => { let o = hw::vmls(sd, sn, sm, m); (vmls(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    2 => { let o = hw::vnmla(sd, sn, sm, m); (vnmla(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    _ => { let o = hw::vnmls(sd, sn, sm, m); (vnmls(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                }
            }
            8 => {
                // VFMA / VFMS / VFNMA / VFNMS
                match (i / 24) % 4 {
                    0 => { let o = hw::fmadd(sn, sm, sd, m); (vfma(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    1 => { let o = hw::fmsub(sn, sm, sd, m); (vfms(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    2 => { let o = hw::fnmadd(sn, sm, sd, m); (vfnma(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                    _ => { let o = hw::fnmsub(sn, sm, sd, m); (vfnms(d, rn, rm), Some(o.bits), 0, o.fpsr) }
                }
            }
            9 => {
                // VCMP / VCMPE with a register, or with zero
                if (i / 24) % 2 == 0 {
                    let (nz, f) = if e { hw::fcmpe(sd, sm, m) } else { hw::fcmp(sd, sm, m) };
                    (vcmp(d, rm, e), None, nz, f)
                } else {
                    let (nz, f) = if e { hw::fcmpe_zero(sd, m) } else { hw::fcmp_zero(sd, m) };
                    (vcmp0(d, e), None, nz, f)
                }
            }
            10 => {
                let o = if unsigned { hw::ucvtf(sm, m) } else { hw::scvtf(sm, m) };
                (if unsigned { vcvt_f32_u32(d, rm) } else { vcvt_f32_s32(d, rm) }, Some(o.bits), 0, o.fpsr)
            }
            11 => {
                let o = if unsigned { hw::fcvtzu(sm, m) } else { hw::fcvtzs(sm, m) };
                (if unsigned { vcvt_u32_f32(d, rm) } else { vcvt_s32_f32(d, rm) }, Some(o.bits), 0, o.fpsr)
            }
            12 => {
                let rmode_ = rmode(m);
                let o = match (unsigned, rmode_) {
                    (false, RMODE_RN) => hw::fcvtns(sm, m),
                    (false, RMODE_RP) => hw::fcvtps(sm, m),
                    (false, RMODE_RM) => hw::fcvtms(sm, m),
                    (false, _) => hw::fcvtzs(sm, m),
                    (true, RMODE_RN) => hw::fcvtnu(sm, m),
                    (true, RMODE_RP) => hw::fcvtpu(sm, m),
                    (true, RMODE_RM) => hw::fcvtmu(sm, m),
                    (true, _) => hw::fcvtzu(sm, m),
                };
                (if unsigned { vcvtr_u32_f32(d, rm) } else { vcvtr_s32_f32(d, rm) }, Some(o.bits), 0, o.fpsr)
            }
            13 => {
                let o = hw::to_fixed32(sd, fbits, unsigned, m);
                (vcvt_fixed(d, true, unsigned, true, fbits), Some(o.bits), 0, o.fpsr)
            }
            14 => {
                let o = hw::from_fixed32(sd, fbits, unsigned, m);
                (vcvt_fixed(d, false, unsigned, true, fbits), Some(o.bits), 0, o.fpsr)
            }
            15 => {
                // single -> half (alternative format on half of the cases)
                if rng.below(2) == 0 {
                    m |= AHP;
                }
                let o = hw::fcvt_f32_f16(sm, m);
                (
                    if top { vcvtt_f16_f32(d, rm) } else { vcvtb_f16_f32(d, rm) },
                    Some(replace_half(sd, o.bits, top)),
                    0,
                    o.fpsr,
                )
            }
            16 => {
                if rng.below(2) == 0 {
                    m |= AHP;
                }
                let half = if top { sm >> 16 } else { sm & 0xFFFF };
                let o = hw::fcvt_f16_f32(half as u16, m);
                (if top { vcvtt_f32_f16(d, rm) } else { vcvtb_f32_f16(d, rm) }, Some(o.bits), 0, o.fpsr)
            }
            // The remaining slots repeat the most common instructions with extra weight.
            17 | 18 => { let o = hw::fadd(sn, sm, m); (vadd(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            19 | 20 => { let o = hw::fmul(sn, sm, m); (vmul(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            21 => { let o = hw::fdiv(sn, sm, m); (vdiv(d, rn, rm), Some(o.bits), 0, o.fpsr) }
            _ => { let o = hw::fmadd(sn, sm, sd, m); (vfma(d, rn, rm), Some(o.bits), 0, o.fpsr) }
        };
        let exp_flags = flags & FLAGS_MASK;
        let after = run(enc, &s, m);
        let mut want = s;
        if let Some(v) = dest {
            want[d as usize] = v;
        }
        let want_fpscr = m | exp_flags | nzcv;
        if after.s != want || after.fpscr != want_fpscr {
            bad += 1;
            if first.len() < 10 {
                let diff: Vec<usize> = (0..32).filter(|&k| after.s[k] != want[k]).collect();
                first.push(format!(
                    "kind {kind} enc {:04x} {:04x} d={d} n={rn} m={rm} mode={m:#010x} s[d]={sd:08x} s[n]={sn:08x} s[m]={sm:08x}: \
                     regs differing {diff:?}, got s[d]={:08x} want {:08x}; fpscr got {:#010x} want {want_fpscr:#010x}",
                    enc.0, enc.1, after.s[d as usize], want[d as usize], after.fpscr
                ));
            }
        }
    }
    println!("{n} random instruction executions compared with the hardware, {bad} mismatches");
    assert_eq!(bad, 0, "mismatches:\n{}", first.join("\n"));
}
