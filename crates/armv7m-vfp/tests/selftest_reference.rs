//! The self-test checksum is a pure function of (seed, n): pin it so that a
//! native build, a WebAssembly build (`scripts/wasm_selftest.mjs`) and any
//! future change are all held to the same reference value.

use armv7m_vfp::selftest::selftest;

pub const SEED: u64 = 0x1234_5678_9ABC_DEF0;
pub const ROUNDS: u32 = 200_000;
/// Reference checksum of `selftest(SEED, ROUNDS)`. Keep in sync with `scripts/wasm_selftest.mjs`.
pub const CHECKSUM: u64 = 0xaa54_23a2_bbce_36ce;

/// `ROUNDS` override for comparing a longer WebAssembly run with the native checksum:
/// `NGC_VFP_SELFTEST_ROUNDS=5000000 cargo test ... -- --ignored --nocapture`.
#[test]
#[ignore]
fn selftest_custom_rounds_print_checksum() {
    let n: u32 = std::env::var("NGC_VFP_SELFTEST_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(ROUNDS);
    let r = selftest(SEED, n);
    println!("rounds {n}: checksum {:#x} mismatches {}", r.checksum, r.mismatches);
    assert_eq!(r.mismatches, 0);
}

#[test]
fn selftest_matches_reference_checksum() {
    let r = selftest(SEED, ROUNDS);
    println!("checksum {:#018x} mismatches {}", r.checksum, r.mismatches);
    assert_eq!(r.mismatches, 0);
    assert_eq!(r.checksum, CHECKSUM, "update CHECKSUM here and in scripts/wasm_selftest.mjs if the semantics changed deliberately");
}
