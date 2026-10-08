//! Randomized and edge-case comparison against the host's native floating
//! point operations (round-to-nearest-even, which every host provides) and,
//! on every host, of the native fast paths (`ieee`) against the exact software
//! reference (`soft`) for results *and* flags in all FPSCR modes.
//!
//! Native NaN payloads are not architectural, so for NaN results the native
//! oracle only checks that a NaN is produced; the exact Arm propagation rules
//! (first signalling NaN quietened, else first quiet NaN, else default NaN) are
//! checked against a small independent model below.
//!
//! The heavy version runs with `cargo test -p armv7m-vfp --release`.

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::soft::{DEFAULT_NAN, QUIET_BIT};
use armv7m_vfp::{ieee, soft};
use common::*;

fn is_snan(x: u32) -> bool {
    is_nan_bits(x) && x & QUIET_BIT == 0
}

/// Independent model of FPProcessNaNs for two operands (DN = 0).
fn nan2_model(a: u32, b: u32) -> Option<u32> {
    if is_snan(a) {
        Some(a | QUIET_BIT)
    } else if is_snan(b) {
        Some(b | QUIET_BIT)
    } else if is_nan_bits(a) {
        Some(a)
    } else if is_nan_bits(b) {
        Some(b)
    } else {
        None
    }
}

fn nan3_model(a: u32, b: u32, c: u32) -> Option<u32> {
    [a, b, c]
        .iter()
        .find(|&&x| is_snan(x))
        .map(|&x| x | QUIET_BIT)
        .or_else(|| [a, b, c].iter().find(|&&x| is_nan_bits(x)).copied())
}

type Op2 = fn(u32, u32, &mut u32) -> u32;

fn check_binary(name: &str, native: fn(f32, f32) -> f32, ops: [(&str, Op2); 2], n: u64, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut checked = 0u64;
    let run = |a: u32, b: u32| {
        let want = native(f32::from_bits(a), f32::from_bits(b)).to_bits();
        for (impl_name, op) in ops {
            let mut f = 0;
            let got = op(a, b, &mut f);
            if let Some(m) = nan2_model(a, b) {
                assert_eq!(got, m, "{name}/{impl_name}: NaN propagation a={a:08x} b={b:08x}");
                assert_eq!(f & IOC != 0, is_snan(a) || is_snan(b), "{name}/{impl_name}: IOC a={a:08x} b={b:08x}");
            } else if is_nan_bits(want) {
                assert_eq!(got, DEFAULT_NAN, "{name}/{impl_name}: default NaN a={a:08x} b={b:08x}");
                assert_eq!(f & IOC, IOC, "{name}/{impl_name}: IOC a={a:08x} b={b:08x}");
            } else {
                assert_eq!(got, want, "{name}/{impl_name}: a={a:08x} b={b:08x}");
                // Overflow always reports OFC together with IXC.
                if f32::from_bits(got).is_infinite() && !f32::from_bits(a).is_infinite() && !f32::from_bits(b).is_infinite() && name != "div" {
                    assert_eq!(f & (OFC | IXC), OFC | IXC, "{name}/{impl_name}: overflow flags a={a:08x} b={b:08x}");
                }
            }
        }
    };
    // Every pair of special values, then random pairs.
    for &a in SPECIALS {
        for &b in SPECIALS {
            run(a, b);
            checked += 1;
        }
    }
    for _ in 0..n {
        let (a, b) = gen_pair(&mut rng);
        run(a, b);
        checked += 1;
    }
    println!("{name}: {checked} cases");
}

#[test]
fn add_matches_native_rne() {
    check_binary("add", |a, b| a + b, [("soft", soft::add), ("fast", ieee::add)], cases(10_000_000, 500_000), 101);
}

#[test]
fn sub_matches_native_rne() {
    check_binary("sub", |a, b| a - b, [("soft", soft::sub), ("fast", ieee::sub)], cases(10_000_000, 500_000), 102);
}

#[test]
fn mul_matches_native_rne() {
    check_binary("mul", |a, b| a * b, [("soft", soft::mul), ("fast", ieee::mul)], cases(10_000_000, 500_000), 103);
}

#[test]
fn div_matches_native_rne() {
    check_binary("div", |a, b| a / b, [("soft", soft::div), ("fast", ieee::div)], cases(10_000_000, 500_000), 104);
}

#[test]
fn sqrt_matches_native_rne() {
    let mut rng = Rng::new(105);
    let mut checked = 0u64;
    let run = |a: u32| {
        let want = f32::from_bits(a).sqrt().to_bits();
        for (impl_name, op) in [("soft", soft::sqrt as fn(u32, &mut u32) -> u32), ("fast", ieee::sqrt)] {
            let mut f = 0;
            let got = op(a, &mut f);
            if is_nan_bits(a) {
                assert_eq!(got, a | QUIET_BIT, "sqrt/{impl_name}: NaN a={a:08x}");
                assert_eq!(f & IOC != 0, is_snan(a));
            } else if is_nan_bits(want) {
                assert_eq!(got, DEFAULT_NAN, "sqrt/{impl_name}: negative a={a:08x}");
                assert_eq!(f & IOC, IOC);
            } else {
                assert_eq!(got, want, "sqrt/{impl_name}: a={a:08x}");
                assert_eq!(f & (IOC | OFC | UFC | DZC | IDC), 0, "sqrt/{impl_name}: flags a={a:08x}");
                // Inexact iff root * root != operand (checked exactly in f64).
                let r = f32::from_bits(got) as f64;
                assert_eq!(f & IXC != 0, r * r != f32::from_bits(a) as f64, "sqrt/{impl_name}: IXC a={a:08x}");
            }
        }
    };
    for &a in SPECIALS {
        run(a);
        checked += 1;
    }
    for _ in 0..cases(10_000_000, 500_000) {
        let mut a = gen_f32(&mut rng);
        match rng.below(4) {
            0 | 1 => a &= 0x7FFF_FFFF,
            2 => {
                let r = f32::from_bits(gen_f32(&mut rng) & 0x7FFF_FFFF);
                a = (r * r).to_bits();
            }
            _ => {}
        }
        run(a);
        checked += 1;
    }
    println!("sqrt: {checked} cases");
}

#[test]
fn fma_matches_native_fused_multiply_add() {
    // f32::mul_add is an exactly-rounded fused operation on this host.
    let mut rng = Rng::new(106);
    let mut checked = 0u64;
    let run = |d: u32, n: u32, m: u32| {
        let want = f32::from_bits(n).mul_add(f32::from_bits(m), f32::from_bits(d)).to_bits();
        let nan = nan3_model(d, n, m);
        for (impl_name, op) in [("soft", soft::fma as fn(u32, u32, u32, &mut u32) -> u32), ("fast", ieee::fma)] {
            let mut f = 0;
            let got = op(d, n, m, &mut f);
            // inf * 0 + QNaN returns the default NaN with IOC; handled below.
            let prod_invalid = (f32::from_bits(n).is_infinite() && f32::from_bits(m) == 0.0)
                || (f32::from_bits(n) == 0.0 && f32::from_bits(m).is_infinite());
            if let Some(model) = nan {
                if prod_invalid && !is_snan(d) && !is_snan(n) && !is_snan(m) && is_nan_bits(d) {
                    assert_eq!(got, DEFAULT_NAN, "fma/{impl_name}: inf*0+QNaN d={d:08x} n={n:08x} m={m:08x}");
                    assert_eq!(f & IOC, IOC);
                } else {
                    assert_eq!(got, model, "fma/{impl_name}: NaN d={d:08x} n={n:08x} m={m:08x}");
                    assert_eq!(f & IOC != 0, is_snan(d) || is_snan(n) || is_snan(m));
                }
            } else if is_nan_bits(want) {
                assert_eq!(got, DEFAULT_NAN, "fma/{impl_name}: invalid d={d:08x} n={n:08x} m={m:08x}");
                assert_eq!(f & IOC, IOC);
            } else {
                assert_eq!(got, want, "fma/{impl_name}: d={d:08x} n={n:08x} m={m:08x}");
            }
        }
    };
    for &d in SPECIALS {
        for &n in SPECIALS {
            for &m in SPECIALS {
                run(d, n, m);
                checked += 1;
            }
        }
    }
    for _ in 0..cases(10_000_000, 500_000) {
        let (d, n, m) = gen_triple(&mut rng);
        run(d, n, m);
        checked += 1;
    }
    println!("fma: {checked} cases");
}

#[test]
fn int_float_conversions_match_native() {
    let mut rng = Rng::new(107);
    let mut checked = 0u64;
    let ints = |rng: &mut Rng| -> u32 {
        match rng.below(6) {
            0 => rng.below(100),
            1 => 0u32.wrapping_sub(rng.below(100)),
            2 => 1 << rng.below(32),
            3 => (1u32 << rng.below(32)).wrapping_add(rng.below(5)).wrapping_sub(2),
            4 => rng.next32() >> rng.below(32),
            _ => rng.next32(),
        }
    };
    for _ in 0..cases(10_000_000, 500_000) {
        // int -> float (RNE): native `as` conversions are RNE.
        let x = ints(&mut rng);
        for (name, f) in [("soft", soft::from_fixed as fn(u32, u32, u32, bool, &mut u32) -> u32), ("fast", ieee::from_fixed)] {
            let mut fl = 0;
            assert_eq!(f(x, 32, 0, false, &mut fl), (x as i32 as f32).to_bits(), "{name} s32 {x:08x}");
            assert_eq!(fl & IXC != 0, (x as i32 as f32) as f64 != x as i32 as f64);
            let mut fl = 0;
            assert_eq!(f(x, 32, 0, true, &mut fl), (x as f32).to_bits(), "{name} u32 {x:08x}");
        }
        let mut fl = 0;
        assert_eq!(ieee::from_int(x, false, &mut fl), (x as i32 as f32).to_bits());
        assert_eq!(ieee::from_int(x, true, &mut fl), (x as f32).to_bits());

        // float -> int, round toward zero, saturating: Rust's `as` saturates and maps NaN to 0.
        let a = if rng.below(2) == 0 { gen_f32(&mut rng) } else { (ints(&mut rng) as i32 as f32 * 0.37).to_bits() };
        let fa = f32::from_bits(a);
        let mut fl = 0;
        assert_eq!(ieee::to_int(a, false, true, &mut fl), fa as i32 as u32, "s32 {a:08x}");
        let mut fl2 = 0;
        assert_eq!(soft::to_fixed(a, 32, 0, false, RMODE_RZ, &mut fl2), fa as i32 as u32, "soft s32 {a:08x}");
        assert_eq!(fl, fl2, "s32 flags {a:08x}");
        let mut fl = 0;
        assert_eq!(ieee::to_int(a, true, true, &mut fl), fa as u32, "u32 {a:08x}");
        let mut fl2 = 0;
        assert_eq!(soft::to_fixed(a, 32, 0, true, RMODE_RZ, &mut fl2), fa as u32, "soft u32 {a:08x}");
        assert_eq!(fl, fl2, "u32 flags {a:08x}");
        checked += 1;
    }
    println!("conversions: {checked} cases");
}

/// The fast paths must agree with the exact software reference in every mode,
/// on results and on all exception flags (portable: no hardware oracle needed).
#[test]
fn fast_paths_equal_soft_in_all_modes() {
    let modes = all_modes();
    let mut rng = Rng::new(108);
    let n = cases(4_000_000, 300_000);
    for i in 0..n {
        let m = modes[rng.below(modes.len() as u32) as usize];
        let (a, b) = gen_pair(&mut rng);
        let (d, nn, mm) = gen_triple(&mut rng);
        let cmp = |name: &str, s: (u32, u32), f: (u32, u32)| {
            assert_eq!(s, f, "{name} mismatch (soft vs fast) a={a:08x} b={b:08x} d={d:08x} n={nn:08x} m={mm:08x} mode={m:#x} i={i}");
        };
        macro_rules! both2 {
            ($name:expr, $soft:path, $fast:path) => {{
                let (mut f1, mut f2) = (m, m);
                let r1 = $soft(a, b, &mut f1);
                let r2 = $fast(a, b, &mut f2);
                cmp($name, (r1, f1), (r2, f2));
            }};
        }
        both2!("add", soft::add, ieee::add);
        both2!("sub", soft::sub, ieee::sub);
        both2!("mul", soft::mul, ieee::mul);
        both2!("div", soft::div, ieee::div);
        {
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::sqrt(a, &mut f1);
            let r2 = ieee::sqrt(a, &mut f2);
            cmp("sqrt", (r1, f1), (r2, f2));
        }
        {
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::fma(d, nn, mm, &mut f1);
            let r2 = ieee::fma(d, nn, mm, &mut f2);
            cmp("fma", (r1, f1), (r2, f2));
        }
        {
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::compare(a, b, i % 2 == 0, &mut f1);
            let r2 = ieee::compare(a, b, i % 2 == 0, &mut f2);
            cmp("compare", (r1, f1), (r2, f2));
        }
        {
            let unsigned = i % 2 == 0;
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::from_fixed(a, 32, 0, unsigned, &mut f1);
            let r2 = ieee::from_int(a, unsigned, &mut f2);
            cmp("from_int", (r1, f1), (r2, f2));
            let fb = rng.below(33);
            let size = if i % 3 == 0 { 16 } else { 32 };
            let fb = if size == 16 { fb.min(16) } else { fb };
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::from_fixed(a, size, fb, unsigned, &mut f1);
            let r2 = ieee::from_fixed(a, size, fb, unsigned, &mut f2);
            cmp("from_fixed", (r1, f1), (r2, f2));
            let (mut f1, mut f2) = (m, m);
            let rz = i % 5 != 0;
            let rm = if rz { RMODE_RZ } else { rmode(m) };
            let r1 = soft::to_fixed(a, 32, 0, unsigned, rm, &mut f1);
            let r2 = ieee::to_int(a, unsigned, rz, &mut f2);
            cmp("to_int", (r1, f1), (r2, f2));
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::to_fixed(a, size, fb, unsigned, RMODE_RZ, &mut f1);
            let r2 = ieee::to_fixed(a, size, fb, unsigned, &mut f2);
            cmp("to_fixed", (r1, f1), (r2, f2));
            // Values that land in range after scaling.
            let b2 = ((rng.next32() >> rng.below(24)) as f32 / (1u64 << fb) as f32).to_bits() ^ (rng.next32() & 0x8000_0000);
            let (mut f1, mut f2) = (m, m);
            let r1 = soft::to_fixed(b2, size, fb, unsigned, RMODE_RZ, &mut f1);
            let r2 = ieee::to_fixed(b2, size, fb, unsigned, &mut f2);
            cmp("to_fixed in range", (r1, f1), (r2, f2));
        }
    }
}
