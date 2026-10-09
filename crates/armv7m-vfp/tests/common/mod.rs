//! Shared helpers for the integration tests: deterministic PRNG, operand
//! generators that stress the interesting corners of binary32 arithmetic, and
//! (on aarch64) a hardware oracle that runs the real FPU with a chosen
//! FPCR (rounding mode, flush-to-zero, default NaN, alternative half
//! precision) and reports the cumulative exception flags.
#![allow(dead_code)]

use armv7m_vfp::fpscr::*;

pub mod asm;
pub mod vectors;

/// Number of random cases per operation/mode: large in `--release`, modest otherwise.
pub fn cases(release: u64, debug: u64) -> u64 {
    if let Ok(v) = std::env::var("NGC_VFP_CASES") {
        if let Ok(n) = v.parse::<u64>() {
            return n;
        }
    }
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

/// splitmix64.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    #[inline]
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    #[inline]
    pub fn next32(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }
    #[inline]
    pub fn below(&mut self, n: u32) -> u32 {
        ((self.next() >> 32) * n as u64 >> 32) as u32
    }
}

/// Interesting single values.
pub const SPECIALS: &[u32] = &[
    0x0000_0000, // +0
    0x8000_0000, // -0
    0x3F80_0000, // 1
    0xBF80_0000, // -1
    0x4000_0000, // 2
    0x3F00_0000, // 0.5
    0x7F80_0000, // +inf
    0xFF80_0000, // -inf
    0x7F7F_FFFF, // max
    0xFF7F_FFFF, // -max
    0x0080_0000, // min normal
    0x8080_0000,
    0x007F_FFFF, // max subnormal
    0x807F_FFFF,
    0x0000_0001, // min subnormal
    0x8000_0001,
    0x7FC0_0000, // default NaN
    0xFFC0_0000, // negative quiet NaN
    0x7F80_0001, // signaling NaN
    0xFF80_0001,
    0x7FBF_FFFF, // largest signaling NaN
    0x7FFF_FFFF, // quiet NaN, all-ones payload
    0x7FC0_1234,
    0x7F80_5678,
    0x4B00_0000, // 2^23
    0x4B80_0000, // 2^24
    0x4F00_0000, // 2^31
    0xCF00_0000, // -2^31
    0x4F80_0000, // 2^32
    0x3F7F_FFFF, // 1 - 2^-24
    0x3F80_0001, // 1 + 2^-23
    0x7E80_0000, // 2^126
    0x0100_0000, // 2^-126 * 2
    0x3300_0000, // 2^-25
    0x3380_0000,
];

/// A random binary32 pattern biased toward the edges of the format.
pub fn gen_f32(rng: &mut Rng) -> u32 {
    let r = rng.next();
    let sign = ((r >> 40) as u32 & 1) << 31;
    let frac_rand = rng.next32() & 0x7F_FFFF;
    match r % 24 {
        0 | 1 => SPECIALS[rng.below(SPECIALS.len() as u32) as usize],
        2 => {
            // NaN with arbitrary payload.
            let f = (rng.next32() & 0x7F_FFFF) | 1;
            sign | 0x7F80_0000 | f
        }
        3 | 4 => sign | frac_rand,                                    // subnormal / zero
        5 => sign | ((1 + rng.below(3)) << 23) | frac_rand,           // near min normal
        6 => sign | ((253 + rng.below(2)) << 23) | frac_rand,         // near max
        7..=10 => rng.next32(),                                       // uniform
        11..=16 => sign | ((127 - 30 + rng.below(61)) << 23) | frac_rand, // moderate exponents
        17 => sign | (rng.below(255) << 23) | 0,                      // exact powers of two
        18 => sign | (rng.below(255) << 23) | 0x7F_FFFF,              // all-ones fraction
        19 => sign | (rng.below(255) << 23) | (1 << rng.below(23)),   // single set bit
        20 => sign | (rng.below(255) << 23) | (0x7F_FFFF ^ (1 << rng.below(23))),
        21 => sign | ((127 + rng.below(40)) << 23) | frac_rand,       // large integers / 2^k
        22 => sign | ((127 - rng.below(40)) << 23) | frac_rand,
        _ => sign | (rng.below(256) << 23) | frac_rand,
    }
}

/// A pair of operands, with a good share of nearby-magnitude pairs
/// (cancellation, ties, exact results) and nearby exponents.
pub fn gen_pair(rng: &mut Rng) -> (u32, u32) {
    let a = gen_f32(rng);
    let b = match rng.below(10) {
        0 => a ^ 0x8000_0000,                                     // x, -x
        1 => a.wrapping_add(1),                                   // 1 ulp apart
        2 => a.wrapping_sub(1) ^ 0x8000_0000,
        3 => (a & 0xFF80_0000) | (rng.next32() & 0x7F_FFFF),      // same exponent
        4 => {
            // exponent within +-3
            let e = ((a >> 23) & 0xFF) as i32 + rng.below(7) as i32 - 3;
            let e = e.clamp(0, 255) as u32;
            (a & 0x8000_0000) | (e << 23) | (rng.next32() & 0x7F_FFFF)
        }
        5 => {
            let e = ((a >> 23) & 0xFF) as i32 + rng.below(61) as i32 - 30;
            let e = e.clamp(0, 255) as u32;
            (rng.next32() & 0x8000_0000) | (e << 23) | (rng.next32() & 0x7F_FFFF)
        }
        _ => gen_f32(rng),
    };
    if rng.below(2) == 0 {
        (a, b)
    } else {
        (b, a)
    }
}

pub fn gen_triple(rng: &mut Rng) -> (u32, u32, u32) {
    let (a, b) = gen_pair(rng);
    let c = match rng.below(8) {
        0 => {
            // addend close to -(a*b) so that the fused sum cancels
            let p = f32::from_bits(a) * f32::from_bits(b);
            let c = (-p).to_bits();
            c.wrapping_add(rng.below(5)).wrapping_sub(2)
        }
        1 => {
            let p = f32::from_bits(a) * f32::from_bits(b);
            (-p).to_bits() ^ rng.below(0x100)
        }
        _ => gen_f32(rng),
    };
    if rng.below(3) == 0 {
        (c, a, b)
    } else {
        (a, b, c)
    }
}

pub fn is_nan_bits(x: u32) -> bool {
    x & 0x7FFF_FFFF > 0x7F80_0000
}

/// FPSCR mode bits for a rounding mode / FZ / DN / AHP combination.
pub fn mode(rm: u32, fz: bool, dn: bool, ahp: bool) -> u32 {
    (rm << RMODE_SHIFT) | if fz { FZ } else { 0 } | if dn { DN } else { 0 } | if ahp { AHP } else { 0 }
}

/// All 32 combinations of rounding mode, FZ, DN (AHP off).
pub fn all_modes() -> Vec<u32> {
    let mut v = Vec::new();
    for rm in 0..4 {
        for fz in [false, true] {
            for dn in [false, true] {
                v.push(mode(rm, fz, dn, false));
            }
        }
    }
    v
}

#[cfg(target_arch = "aarch64")]
pub mod hw {
    //! Hardware oracle: the host's AArch64 FPU implements the same FP
    //! semantics (FPRound/FPUnpack/NaN propagation, FPCR.{RMode,FZ,DN,AHP}
    //! at the same bit positions, FPSR flags at the same bit positions) as the
    //! AArch32 VFP pseudocode. Each call runs a single asm block that installs
    //! the FPCR, clears FPSR, executes one instruction, reads FPSR (and NZCV),
    //! then restores FPCR = 0.
    use core::arch::asm;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Out {
        pub bits: u32,
        /// FPSR cumulative flags (bits 0..4, 7).
        pub fpsr: u32,
    }

    macro_rules! bin {
        ($name:ident, $insn:literal) => {
            pub fn $name(a: u32, b: u32, fpcr: u32) -> Out {
                let r: u32;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {r:s}, {a:s}, {b:s}"),
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        a = in(vreg) a,
                        b = in(vreg) b,
                        r = out(vreg) r,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                Out { bits: r, fpsr: fpsr as u32 }
            }
        };
    }
    bin!(fadd, "fadd");
    bin!(fsub, "fsub");
    bin!(fmul, "fmul");
    bin!(fdiv, "fdiv");
    bin!(fnmul, "fnmul");

    /// Separately rounded multiply-accumulate sequences built from hardware
    /// primitives in one asm block (VMLA, VMLS, VNMLA, VNMLS semantics with the
    /// Arm pseudocode's FPNeg = bitwise sign flip): returns the new Sd.
    macro_rules! mac {
        ($name:ident, $body:literal) => {
            pub fn $name(d: u32, n: u32, m: u32, fpcr: u32) -> Out {
                let r: u32;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($body, "\n/* {u} */"),
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        d = in(vreg) d,
                        n = in(vreg) n,
                        m = in(vreg) m,
                        t = out(vreg) _,
                        u = out(vreg) _,
                        r = out(vreg) r,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                Out { bits: r, fpsr: fpsr as u32 }
            }
        };
    }
    // r = d + n*m (product rounded first)
    mac!(vmla, "fmul {t:s}, {n:s}, {m:s}\nfadd {r:s}, {d:s}, {t:s}");
    // r = d + (-(n*m))
    mac!(vmls, "fmul {t:s}, {n:s}, {m:s}\nfneg {t:s}, {t:s}\nfadd {r:s}, {d:s}, {t:s}");
    // r = (-d) + (-(n*m))
    mac!(vnmla, "fneg {u:s}, {d:s}\nfmul {t:s}, {n:s}, {m:s}\nfneg {t:s}, {t:s}\nfadd {r:s}, {u:s}, {t:s}");
    // r = (-d) + n*m
    mac!(vnmls, "fneg {u:s}, {d:s}\nfmul {t:s}, {n:s}, {m:s}\nfadd {r:s}, {u:s}, {t:s}");

    pub fn fsqrt(a: u32, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "fsqrt {r:s}, {a:s}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                a = in(vreg) a,
                r = out(vreg) r,
                fpsr = out(reg) fpsr,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }

    macro_rules! fused {
        ($name:ident, $insn:literal) => {
            /// `Sd = fused op of (n, m, a)` with the AArch64 operand order (n, m, a).
            pub fn $name(n: u32, m: u32, a: u32, fpcr: u32) -> Out {
                let r: u32;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {r:s}, {n:s}, {m:s}, {a:s}"),
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        n = in(vreg) n,
                        m = in(vreg) m,
                        a = in(vreg) a,
                        r = out(vreg) r,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                Out { bits: r, fpsr: fpsr as u32 }
            }
        };
    }
    fused!(fmadd, "fmadd"); // a + n*m
    fused!(fmsub, "fmsub"); // a - n*m
    fused!(fnmadd, "fnmadd"); // -a - n*m
    fused!(fnmsub, "fnmsub"); // n*m - a

    /// Returns (NZCV in bits 31:28, FPSR flags).
    macro_rules! cmp {
        ($name:ident, $insn:literal) => {
            pub fn $name(a: u32, b: u32, fpcr: u32) -> (u32, u32) {
                let nzcv: u64;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {a:s}, {b:s}"),
                        "mrs {nzcv}, nzcv",
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        a = in(vreg) a,
                        b = in(vreg) b,
                        nzcv = out(reg) nzcv,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                ((nzcv as u32) & 0xF000_0000, fpsr as u32)
            }
        };
    }
    cmp!(fcmp, "fcmp");
    cmp!(fcmpe, "fcmpe");

    macro_rules! cmpz {
        ($name:ident, $insn:literal) => {
            pub fn $name(a: u32, fpcr: u32) -> (u32, u32) {
                let nzcv: u64;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {a:s}, #0.0"),
                        "mrs {nzcv}, nzcv",
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        a = in(vreg) a,
                        nzcv = out(reg) nzcv,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                ((nzcv as u32) & 0xF000_0000, fpsr as u32)
            }
        };
    }
    cmpz!(fcmp_zero, "fcmp");
    cmpz!(fcmpe_zero, "fcmpe");

    macro_rules! f2i {
        ($name:ident, $insn:literal) => {
            pub fn $name(a: u32, fpcr: u32) -> Out {
                let r: u32;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {r:w}, {a:s}"),
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        a = in(vreg) a,
                        r = out(reg) r,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                Out { bits: r, fpsr: fpsr as u32 }
            }
        };
    }
    f2i!(fcvtzs, "fcvtzs");
    f2i!(fcvtzu, "fcvtzu");
    f2i!(fcvtns, "fcvtns");
    f2i!(fcvtnu, "fcvtnu");
    f2i!(fcvtps, "fcvtps");
    f2i!(fcvtpu, "fcvtpu");
    f2i!(fcvtms, "fcvtms");
    f2i!(fcvtmu, "fcvtmu");

    macro_rules! i2f {
        ($name:ident, $insn:literal) => {
            pub fn $name(x: u32, fpcr: u32) -> Out {
                let r: u32;
                let fpsr: u64;
                unsafe {
                    asm!(
                        "msr fpcr, {fpcr}",
                        "msr fpsr, xzr",
                        concat!($insn, " {r:s}, {x:w}"),
                        "mrs {fpsr}, fpsr",
                        "msr fpcr, xzr",
                        fpcr = in(reg) fpcr as u64,
                        x = in(reg) x,
                        r = out(vreg) r,
                        fpsr = out(reg) fpsr,
                        options(nomem, nostack),
                    );
                }
                Out { bits: r, fpsr: fpsr as u32 }
            }
        };
    }
    i2f!(scvtf, "scvtf");
    i2f!(ucvtf, "ucvtf");

    /// Float to 32-bit fixed point, round toward zero, `FB` fraction bits (1..=32).
    pub fn fcvtzs_fixed<const FB: u32>(a: u32, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "fcvtzs {r:w}, {a:s}, #{fb}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                a = in(vreg) a,
                r = out(reg) r,
                fpsr = out(reg) fpsr,
                fb = const FB,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }

    pub fn fcvtzu_fixed<const FB: u32>(a: u32, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "fcvtzu {r:w}, {a:s}, #{fb}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                a = in(vreg) a,
                r = out(reg) r,
                fpsr = out(reg) fpsr,
                fb = const FB,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }

    pub fn scvtf_fixed<const FB: u32>(x: u32, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "scvtf {r:s}, {x:w}, #{fb}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                x = in(reg) x,
                r = out(vreg) r,
                fpsr = out(reg) fpsr,
                fb = const FB,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }

    pub fn ucvtf_fixed<const FB: u32>(x: u32, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "ucvtf {r:s}, {x:w}, #{fb}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                x = in(reg) x,
                r = out(vreg) r,
                fpsr = out(reg) fpsr,
                fb = const FB,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }

    macro_rules! by_fbits {
        ($fb:expr, $func:ident, $x:expr, $c:expr) => {
            by_fbits!(@go $fb, $func, $x, $c,
                1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32)
        };
        (@go $fb:expr, $func:ident, $x:expr, $c:expr, $($n:literal)+) => {
            match $fb {
                $($n => $func::<$n>($x, $c),)+
                _ => panic!("fbits out of range"),
            }
        };
    }

    /// `fbits` in 1..=32.
    pub fn to_fixed32(a: u32, fbits: u32, unsigned: bool, fpcr: u32) -> Out {
        if unsigned {
            by_fbits!(fbits, fcvtzu_fixed, a, fpcr)
        } else {
            by_fbits!(fbits, fcvtzs_fixed, a, fpcr)
        }
    }

    pub fn from_fixed32(x: u32, fbits: u32, unsigned: bool, fpcr: u32) -> Out {
        if unsigned {
            by_fbits!(fbits, ucvtf_fixed, x, fpcr)
        } else {
            by_fbits!(fbits, scvtf_fixed, x, fpcr)
        }
    }

    /// Single to half; `bits` holds the 16-bit result.
    pub fn fcvt_f32_f16(a: u32, fpcr: u32) -> Out {
        let r: u16;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "fcvt {r:h}, {a:s}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                a = in(vreg) a,
                r = out(vreg) r,
                fpsr = out(reg) fpsr,
                options(nomem, nostack),
            );
        }
        Out { bits: r as u32, fpsr: fpsr as u32 }
    }

    /// Half to single.
    pub fn fcvt_f16_f32(h: u16, fpcr: u32) -> Out {
        let r: u32;
        let fpsr: u64;
        unsafe {
            asm!(
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                "fcvt {r:s}, {a:h}",
                "mrs {fpsr}, fpsr",
                "msr fpcr, xzr",
                fpcr = in(reg) fpcr as u64,
                a = in(vreg) h,
                r = out(vreg) r,
                fpsr = out(reg) fpsr,
                options(nomem, nostack),
            );
        }
        Out { bits: r, fpsr: fpsr as u32 }
    }
}
