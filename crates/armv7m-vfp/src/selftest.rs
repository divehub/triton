//! Deterministic self-test of the floating-point layer, usable on any target
//! (including WebAssembly): runs a reproducible pseudo-random stream of
//! operations in random FPSCR modes through both the exact software core
//! (`soft`) and the native fast-path layer (`ieee`), counts disagreements and
//! folds every result and flag into a checksum. The checksum is a pure function
//! of `(seed, n)`: native builds and WebAssembly builds must report the same
//! value, which proves the fast paths behave identically on every host.

use crate::fpscr::*;
use crate::{ieee, soft};

/// Outcome of [`selftest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfTestReport {
    /// FNV-1a over all results/flags of the exact software core.
    pub checksum: u64,
    /// Number of fast-path results or flags that differ from the exact core.
    pub mismatches: u32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next32(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }
}

const SPECIALS: [u32; 16] = [
    0x0000_0000, 0x8000_0000, 0x3F80_0000, 0xBF80_0000, 0x7F80_0000, 0xFF80_0000, 0x7F7F_FFFF,
    0x0080_0000, 0x007F_FFFF, 0x0000_0001, 0x7FC0_0000, 0x7F80_0001, 0x4F00_0000, 0xCF00_0000,
    0x3380_0000, 0x7E80_0000,
];

fn operand(rng: &mut Rng) -> u32 {
    let r = rng.next32();
    let sign = rng.next32() & 0x8000_0000;
    match r % 8 {
        0 => SPECIALS[(rng.next32() % 16) as usize],
        1 => sign | (rng.next32() & 0x7F_FFFF),
        2 => sign | ((127 - 30 + rng.next32() % 61) << 23) | (rng.next32() & 0x7F_FFFF),
        3 => sign | ((rng.next32() % 255) << 23) | (rng.next32() & 0x7F_FFFF),
        4 => rng.next32(),
        _ => sign | ((127 - 12 + rng.next32() % 25) << 23) | (rng.next32() & 0x7F_FFFF),
    }
}

#[inline]
fn fold(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

/// Runs `n` rounds of the self-test (each round exercises every operation once).
pub fn selftest(seed: u64, n: u32) -> SelfTestReport {
    let mut rng = Rng(seed);
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    let mut mismatches = 0u32;
    for i in 0..n {
        let m = ((rng.next32() & 3) << RMODE_SHIFT)
            | if rng.next32() & 1 != 0 { FZ } else { 0 }
            | if rng.next32() & 1 != 0 { DN } else { 0 }
            | if rng.next32() % 4 == 0 { AHP } else { 0 };
        let (a, b, c) = (operand(&mut rng), operand(&mut rng), operand(&mut rng));
        let fb = rng.next32() % 33;
        let unsigned = i & 1 != 0;

        macro_rules! both {
            ($s:expr, $f:expr) => {{
                let mut fs = m;
                let mut ff = m;
                let rs: u32 = $s(&mut fs);
                let rf: u32 = $f(&mut ff);
                if rs != rf || fs != ff {
                    mismatches += 1;
                }
                fold(&mut h, rs);
                fold(&mut h, fs);
            }};
        }
        both!(|f: &mut u32| soft::add(a, b, f), |f: &mut u32| ieee::add(a, b, f));
        both!(|f: &mut u32| soft::sub(a, b, f), |f: &mut u32| ieee::sub(a, b, f));
        both!(|f: &mut u32| soft::mul(a, b, f), |f: &mut u32| ieee::mul(a, b, f));
        both!(|f: &mut u32| soft::div(a, b, f), |f: &mut u32| ieee::div(a, b, f));
        both!(|f: &mut u32| soft::sqrt(a, f), |f: &mut u32| ieee::sqrt(a, f));
        both!(|f: &mut u32| soft::fma(a, b, c, f), |f: &mut u32| ieee::fma(a, b, c, f));
        both!(|f: &mut u32| soft::compare(a, b, i & 2 != 0, f), |f: &mut u32| ieee::compare(a, b, i & 2 != 0, f));
        both!(
            |f: &mut u32| soft::from_fixed(a, 32, 0, unsigned, f),
            |f: &mut u32| ieee::from_int(a, unsigned, f)
        );
        both!(
            |f: &mut u32| soft::from_fixed(a, 32, fb, unsigned, f),
            |f: &mut u32| ieee::from_fixed(a, 32, fb, unsigned, f)
        );
        let rm = if i & 4 != 0 { RMODE_RZ } else { rmode(m) };
        both!(
            |f: &mut u32| soft::to_fixed(b, 32, 0, unsigned, rm, f),
            |f: &mut u32| ieee::to_int(b, unsigned, i & 4 != 0, f)
        );
        both!(
            |f: &mut u32| soft::to_fixed(b, 32, fb, unsigned, RMODE_RZ, f),
            |f: &mut u32| ieee::to_fixed(b, 32, fb, unsigned, f)
        );
        both!(
            |f: &mut u32| soft::f32_to_f16(c, f) as u32,
            |f: &mut u32| ieee::f32_to_f16(c, f) as u32
        );
        both!(
            |f: &mut u32| soft::f16_to_f32(c as u16, f),
            |f: &mut u32| ieee::f16_to_f32(c as u16, f)
        );
    }
    SelfTestReport { checksum: h, mismatches }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selftest_is_clean_and_reproducible() {
        let a = selftest(1, 20_000);
        let b = selftest(1, 20_000);
        assert_eq!(a, b);
        assert_eq!(a.mismatches, 0);
        assert_ne!(selftest(2, 20_000).checksum, a.checksum);
    }
}
