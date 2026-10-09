//! Cross-checks the exact software core (`soft`) and the fast paths (`ieee`)
//! against the host's hardware FPU (AArch64 only) for every FPSCR rounding
//! mode, flush-to-zero and default-NaN setting. Results (including NaN
//! payloads) and the cumulative exception flags must match exactly.
//!
//! Run the heavy version with `cargo test -p armv7m-vfp --release`.
#![cfg(target_arch = "aarch64")]

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::soft::SIGN;
use armv7m_vfp::{ieee, soft};
use common::*;

struct Tally {
    name: &'static str,
    n: u64,
    bad: u64,
    first: Vec<String>,
}

impl Tally {
    fn new(name: &'static str) -> Self {
        Tally { name, n: 0, bad: 0, first: Vec::new() }
    }
    fn check(&mut self, ctx: impl FnOnce() -> String, exp: (u32, u32), got: (u32, u32)) {
        self.n += 1;
        if exp != got {
            self.bad += 1;
            if self.first.len() < 12 {
                self.first.push(format!(
                    "{}: expected bits {:08x} flags {:#04x}, got bits {:08x} flags {:#04x}",
                    ctx(),
                    exp.0,
                    exp.1,
                    got.0,
                    got.1
                ));
            }
        }
    }
    fn finish(self) {
        println!("{}: {} cases, {} mismatches", self.name, self.n, self.bad);
        if self.bad != 0 {
            panic!("{} mismatches in {}:\n{}", self.bad, self.name, self.first.join("\n"));
        }
    }
}

fn exp_of(o: hw::Out) -> (u32, u32) {
    (o.bits, o.fpsr & FLAGS_MASK)
}

fn run2(
    name: &'static str,
    n: u64,
    seed: u64,
    hw_op: fn(u32, u32, u32) -> hw::Out,
    soft_op: fn(u32, u32, &mut u32) -> u32,
    fast_op: fn(u32, u32, &mut u32) -> u32,
) {
    let modes = all_modes();
    let mut rng = Rng::new(seed);
    let mut t_soft = Tally::new(name);
    let mut t_fast = Tally::new(name);
    for _ in 0..n {
        let (a, b) = gen_pair(&mut rng);
        let m = modes[rng.below(modes.len() as u32) as usize];
        let exp = exp_of(hw_op(a, b, m));
        let mut f = m;
        let r = soft_op(a, b, &mut f);
        t_soft.check(|| format!("soft a={a:08x} b={b:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
        let mut f = m;
        let r = fast_op(a, b, &mut f);
        t_fast.check(|| format!("fast a={a:08x} b={b:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
    }
    t_soft.finish();
    t_fast.finish();
}

#[test]
fn add_vs_hardware() {
    run2("fadd", cases(8_000_000, 400_000), 1, hw::fadd, soft::add, ieee::add);
}

#[test]
fn sub_vs_hardware() {
    run2("fsub", cases(8_000_000, 400_000), 2, hw::fsub, soft::sub, ieee::sub);
}

#[test]
fn mul_vs_hardware() {
    run2("fmul", cases(8_000_000, 400_000), 3, hw::fmul, soft::mul, ieee::mul);
}

#[test]
fn div_vs_hardware() {
    run2("fdiv", cases(8_000_000, 400_000), 4, hw::fdiv, soft::div, ieee::div);
}

#[test]
fn sqrt_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(5);
    let mut t_soft = Tally::new("fsqrt-soft");
    let mut t_fast = Tally::new("fsqrt-fast");
    for _ in 0..cases(8_000_000, 400_000) {
        let mut a = gen_f32(&mut rng);
        // Favor non-negative operands and perfect squares.
        match rng.below(6) {
            0 | 1 => a &= 0x7FFF_FFFF,
            2 => {
                let r = f32::from_bits(gen_f32(&mut rng) & 0x7FFF_FFFF);
                a = (r * r).to_bits();
            }
            _ => {}
        }
        let m = modes[rng.below(modes.len() as u32) as usize];
        let exp = exp_of(hw::fsqrt(a, m));
        let mut f = m;
        let r = soft::sqrt(a, &mut f);
        t_soft.check(|| format!("soft a={a:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
        let mut f = m;
        let r = ieee::sqrt(a, &mut f);
        t_fast.check(|| format!("fast a={a:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
    }
    t_soft.finish();
    t_fast.finish();
}

#[test]
fn fma_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(6);
    let mut t_soft = Tally::new("fma-soft");
    let mut t_fast = Tally::new("fma-fast");
    for i in 0..cases(8_000_000, 400_000) {
        // (addend d, operand n, operand m)
        let (d, n, m) = gen_triple(&mut rng);
        let mode = modes[rng.below(modes.len() as u32) as usize];
        // VFMA, VFMS, VFNMA, VFNMS in turn.
        let (exp, d2, n2) = match i % 4 {
            0 => (hw::fmadd(n, m, d, mode), d, n),
            1 => (hw::fmsub(n, m, d, mode), d, n ^ SIGN),
            2 => (hw::fnmadd(n, m, d, mode), d ^ SIGN, n ^ SIGN),
            _ => (hw::fnmsub(n, m, d, mode), d ^ SIGN, n),
        };
        let exp = exp_of(exp);
        let mut f = mode;
        let r = soft::fma(d2, n2, m, &mut f);
        t_soft.check(
            || format!("soft variant={} d={d:08x} n={n:08x} m={m:08x} mode={mode:#010x}", i % 4),
            exp,
            (r, f & FLAGS_MASK),
        );
        let mut f = mode;
        let r = ieee::fma(d2, n2, m, &mut f);
        t_fast.check(
            || format!("fast variant={} d={d:08x} n={n:08x} m={m:08x} mode={mode:#010x}", i % 4),
            exp,
            (r, f & FLAGS_MASK),
        );
    }
    t_soft.finish();
    t_fast.finish();
}

#[test]
fn compare_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(7);
    let mut t_soft = Tally::new("fcmp-soft");
    let mut t_fast = Tally::new("fcmp-fast");
    for i in 0..cases(4_000_000, 400_000) {
        let (a, mut b) = gen_pair(&mut rng);
        let zero = i % 4 == 3;
        if zero {
            b = 0;
        }
        let e = i % 2 == 1;
        let m = modes[rng.below(modes.len() as u32) as usize];
        let (nzcv, fpsr) = match (zero, e) {
            (false, false) => hw::fcmp(a, b, m),
            (false, true) => hw::fcmpe(a, b, m),
            (true, false) => hw::fcmp_zero(a, m),
            (true, true) => hw::fcmpe_zero(a, m),
        };
        let exp = (nzcv, fpsr & FLAGS_MASK);
        let mut f = m;
        let r = soft::compare(a, b, e, &mut f);
        t_soft.check(|| format!("soft a={a:08x} b={b:08x} e={e} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
        let mut f = m;
        let r = ieee::compare(a, b, e, &mut f);
        t_fast.check(|| format!("fast a={a:08x} b={b:08x} e={e} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
    }
    t_soft.finish();
    t_fast.finish();
}

fn gen_int(rng: &mut Rng) -> u32 {
    match rng.below(8) {
        0 => rng.below(64),
        1 => 0u32.wrapping_sub(rng.below(64)),
        2 => 1u32 << rng.below(32),
        3 => (1u32 << rng.below(32)).wrapping_add(rng.below(5)).wrapping_sub(2),
        4 => rng.next32() >> rng.below(32),
        5 => (rng.next32() >> rng.below(32)) | (1 << rng.below(32)),
        6 => 0x7FFF_FFFF ^ rng.below(8),
        _ => rng.next32(),
    }
}

#[test]
fn int_to_float_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(8);
    let mut t_soft = Tally::new("cvt-i2f-soft");
    let mut t_fast = Tally::new("cvt-i2f-fast");
    for i in 0..cases(4_000_000, 400_000) {
        let x = gen_int(&mut rng);
        let unsigned = i % 2 == 0;
        let m = modes[rng.below(modes.len() as u32) as usize];
        let exp = exp_of(if unsigned { hw::ucvtf(x, m) } else { hw::scvtf(x, m) });
        let mut f = m;
        let r = soft::from_fixed(x, 32, 0, unsigned, &mut f);
        t_soft.check(|| format!("soft x={x:08x} unsigned={unsigned} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
        let mut f = m;
        let r = ieee::from_int(x, unsigned, &mut f);
        t_fast.check(|| format!("fast x={x:08x} unsigned={unsigned} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
    }
    t_soft.finish();
    t_fast.finish();
}

fn gen_conv_f32(rng: &mut Rng) -> u32 {
    match rng.below(8) {
        0 => {
            // around integer boundaries and ties
            let base = match rng.below(6) {
                0 => 2147483648.0f32,
                1 => 4294967296.0f32,
                2 => 8388608.0f32,
                3 => 16777216.0f32,
                4 => 1.0f32,
                _ => (rng.next32() >> rng.below(32)) as f32,
            };
            let ulps = rng.below(7) as i32 - 3;
            let v = base.to_bits().wrapping_add(ulps as u32);
            v | if rng.below(2) == 0 { SIGN } else { 0 }
        }
        1 => {
            // x.5, x.25, x.75 style fractions
            let i = (rng.next32() >> (8 + rng.below(24))) as f32;
            let frac = [0.0f32, 0.25, 0.5, 0.75, 0.125, 0.375, 0.999_999_94][rng.below(7) as usize];
            let v = i + frac;
            let v = if rng.below(2) == 0 { -v } else { v };
            v.to_bits()
        }
        _ => gen_f32(rng),
    }
}

#[test]
fn float_to_int_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(9);
    let mut t_soft = Tally::new("cvt-f2i-soft");
    let mut t_fast = Tally::new("cvt-f2i-fast");
    for i in 0..cases(6_000_000, 400_000) {
        let a = gen_conv_f32(&mut rng);
        let unsigned = i % 2 == 0;
        let round_zero = i % 3 != 0; // VCVT (true) or VCVTR (false)
        let m = modes[rng.below(modes.len() as u32) as usize];
        let rm = if round_zero { RMODE_RZ } else { rmode(m) };
        let o = match (unsigned, rm) {
            (false, RMODE_RN) => hw::fcvtns(a, m),
            (false, RMODE_RP) => hw::fcvtps(a, m),
            (false, RMODE_RM) => hw::fcvtms(a, m),
            (false, _) => hw::fcvtzs(a, m),
            (true, RMODE_RN) => hw::fcvtnu(a, m),
            (true, RMODE_RP) => hw::fcvtpu(a, m),
            (true, RMODE_RM) => hw::fcvtmu(a, m),
            (true, _) => hw::fcvtzu(a, m),
        };
        let exp = exp_of(o);
        let mut f = m;
        let r = soft::to_fixed(a, 32, 0, unsigned, rm, &mut f);
        t_soft.check(
            || format!("soft a={a:08x} unsigned={unsigned} rm={rm} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
        let mut f = m;
        let r = ieee::to_int(a, unsigned, round_zero, &mut f);
        t_fast.check(
            || format!("fast a={a:08x} unsigned={unsigned} round_zero={round_zero} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
    }
    t_soft.finish();
    t_fast.finish();
}

#[test]
fn fixed_32_vs_hardware() {
    let modes = all_modes();
    let mut rng = Rng::new(10);
    let mut t_to = Tally::new("cvt-to-fixed32");
    let mut t_from_soft = Tally::new("cvt-from-fixed32-soft");
    let mut t_from_fast = Tally::new("cvt-from-fixed32-fast");
    for i in 0..cases(3_000_000, 300_000) {
        let fbits = 1 + rng.below(32);
        let unsigned = i % 2 == 0;
        let m = modes[rng.below(modes.len() as u32) as usize];
        // float -> fixed (always round toward zero)
        let a = if i % 3 == 0 {
            // scale a plausible fixed value so results land in range
            let x = (rng.next32() >> rng.below(32)) as f32 / (1u64 << fbits) as f32;
            if rng.below(2) == 0 { x.to_bits() } else { (-x).to_bits() }
        } else {
            gen_conv_f32(&mut rng)
        };
        let exp = exp_of(hw::to_fixed32(a, fbits, unsigned, m));
        let mut f = m;
        let r = soft::to_fixed(a, 32, fbits, unsigned, RMODE_RZ, &mut f);
        t_to.check(
            || format!("to_fixed a={a:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
        let mut f = m;
        let r = ieee::to_fixed(a, 32, fbits, unsigned, &mut f);
        t_to.check(
            || format!("to_fixed fast a={a:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
        // fixed -> float (FPSCR rounding)
        let x = gen_int(&mut rng);
        let exp = exp_of(hw::from_fixed32(x, fbits, unsigned, m));
        let mut f = m;
        let r = soft::from_fixed(x, 32, fbits, unsigned, &mut f);
        t_from_soft.check(
            || format!("from_fixed soft x={x:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
        let mut f = m;
        let r = ieee::from_fixed(x, 32, fbits, unsigned, &mut f);
        t_from_fast.check(
            || format!("from_fixed fast x={x:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
    }
    t_to.finish();
    t_from_soft.finish();
    t_from_fast.finish();
}

/// 16-bit fixed point has no AArch64 equivalent: derive the expected result
/// from the 32-bit hardware conversion (same integer, then saturate to 16 bits;
/// out-of-range results signal IOC only).
#[test]
fn fixed_16_vs_hardware_derived() {
    let modes = all_modes();
    let mut rng = Rng::new(11);
    let mut t_to = Tally::new("cvt-to-fixed16");
    let mut t_from = Tally::new("cvt-from-fixed16");
    for i in 0..cases(2_000_000, 200_000) {
        let fbits = rng.below(17); // 0..=16
        let unsigned = i % 2 == 0;
        let m = modes[rng.below(modes.len() as u32) as usize];
        let a = if i % 3 == 0 {
            let x = (rng.next32() >> (16 + rng.below(16))) as f32 / (1u64 << fbits) as f32;
            if rng.below(2) == 0 { x.to_bits() } else { (-x).to_bits() }
        } else {
            gen_conv_f32(&mut rng)
        };
        // Scale by 2^fbits exactly in the (wider) 32-bit domain: use fbits=0 hardware
        // conversion on the scaled float when that is exact, else the fixed-point one.
        let hw32 = if fbits == 0 {
            if unsigned { hw::fcvtzu(a, m) } else { hw::fcvtzs(a, m) }
        } else {
            hw::to_fixed32(a, fbits, unsigned, m)
        };
        let (lo, hi): (i64, i64) = if unsigned { (0, 0xFFFF) } else { (-0x8000, 0x7FFF) };
        let v32 = if unsigned { hw32.bits as i64 } else { hw32.bits as i32 as i64 };
        let sat32 = hw32.fpsr & IOC != 0;
        let (bits, flags) = if sat32 || v32 < lo || v32 > hi {
            let nan_or_pos: u32 = if unsigned {
                // negative inputs saturate to 0, everything else (incl. NaN -> 0 handled by hw) high
                if sat32 && hw32.bits == 0 { 0 } else if v32 < lo { 0 } else { 0xFFFF }
            } else if sat32 && hw32.bits == 0 {
                0
            } else if v32 < lo {
                0xFFFF_8000
            } else {
                0x7FFF
            };
            (nan_or_pos, IOC)
        } else {
            (hw32.bits & if unsigned { 0xFFFF } else { 0xFFFF_FFFF }, hw32.fpsr & FLAGS_MASK)
        };
        let mut f = m;
        let r = soft::to_fixed(a, 16, fbits, unsigned, RMODE_RZ, &mut f);
        t_to.check(
            || format!("to_fixed16 a={a:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            (bits, flags),
            (r, f & FLAGS_MASK),
        );
        let mut f = m;
        let r = ieee::to_fixed(a, 16, fbits, unsigned, &mut f);
        t_to.check(
            || format!("to_fixed16 fast a={a:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            (bits, flags),
            (r, f & FLAGS_MASK),
        );

        // from fixed16: sign/zero extend the 16-bit pattern and use the 32-bit hardware op
        let x16 = rng.next32() & 0xFFFF;
        let ext = if unsigned { x16 } else { x16 as u16 as i16 as i32 as u32 };
        let exp = exp_of(if fbits == 0 {
            if unsigned { hw::ucvtf(ext, m) } else { hw::scvtf(ext, m) }
        } else {
            hw::from_fixed32(ext, fbits, unsigned, m)
        });
        // Upper bits of the source register must be ignored.
        let src = x16 | (rng.next32() & 0xFFFF_0000);
        let mut f = m;
        let r = soft::from_fixed(src, 16, fbits, unsigned, &mut f);
        t_from.check(
            || format!("from_fixed16 x={src:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
        let mut f = m;
        let r = ieee::from_fixed(src, 16, fbits, unsigned, &mut f);
        t_from.check(
            || format!("from_fixed16 fast x={src:08x} fbits={fbits} unsigned={unsigned} mode={m:#010x}"),
            exp,
            (r, f & FLAGS_MASK),
        );
    }
    t_to.finish();
    t_from.finish();
}

#[test]
fn half_to_single_exhaustive_vs_hardware() {
    let mut t = Tally::new("f16-to-f32");
    for ahp in [false, true] {
        for dn in [false, true] {
            let m = mode(0, false, dn, ahp);
            for h in 0..=0xFFFFu32 {
                let exp = exp_of(hw::fcvt_f16_f32(h as u16, m));
                let mut f = m;
                let r = soft::f16_to_f32(h as u16, &mut f);
                t.check(|| format!("h={h:04x} ahp={ahp} dn={dn}"), exp, (r, f & FLAGS_MASK));
            }
        }
    }
    t.finish();
}

#[test]
fn single_to_half_vs_hardware() {
    let mut rng = Rng::new(12);
    let mut t = Tally::new("f32-to-f16");
    let mut modes = Vec::new();
    for rm in 0..4 {
        for fz in [false, true] {
            for dn in [false, true] {
                for ahp in [false, true] {
                    modes.push(mode(rm, fz, dn, ahp));
                }
            }
        }
    }
    // Random plus a structured sweep over the exponent / high fraction bits.
    for i in 0..cases(6_000_000, 400_000) {
        let a = if i % 2 == 0 {
            gen_f32(&mut rng)
        } else {
            // exponents around the half-precision range with random fractions
            let e = 112 - 12 + rng.below(50);
            ((rng.next32() & 0x8000_0000) | (e << 23) | (rng.next32() & 0x7F_FFFF)) ^ (rng.below(4) << 12)
        };
        let m = modes[rng.below(modes.len() as u32) as usize];
        let exp = exp_of(hw::fcvt_f32_f16(a, m));
        let mut f = m;
        let r = soft::f32_to_f16(a, &mut f) as u32;
        t.check(|| format!("a={a:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
    }
    t.finish();
}

#[test]
fn single_to_half_sweep_vs_hardware() {
    // Every exponent / top-fraction pattern with a few low-bit patterns.
    let step = if cfg!(debug_assertions) { 1 << 6 } else { 1 };
    let mut t = Tally::new("f32-to-f16-sweep");
    let modes = [mode(0, false, false, false), mode(3, false, true, true), mode(1, true, false, false), mode(2, false, false, true)];
    let lows = [0u32, 1, 0x0FFF, 0x1000, 0x1FFF, 0x0800, 0x17FF];
    let mut hi = 0u32;
    while hi < (1 << 19) {
        for &low in &lows {
            let a = (hi << 13) | low;
            for &m in &modes {
                let exp = exp_of(hw::fcvt_f32_f16(a, m));
                let mut f = m;
                let r = soft::f32_to_f16(a, &mut f) as u32;
                t.check(|| format!("a={a:08x} mode={m:#010x}"), exp, (r, f & FLAGS_MASK));
            }
        }
        hi += step;
    }
    t.finish();
}
