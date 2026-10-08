//! The runtime-library routines that are accelerated, and how they are found in a firmware image.
//!
//! Routines are identified by their **code bytes**: a SHA-256 over the routine body plus its literal pool
//! (`len` bytes from the entry). The scan reads the image's code region, not addresses: it tries the address
//! the routine has in the TRITON main 5.8 image first and otherwise looks at every halfword offset whose
//! first eight bytes hash to `anchor`; a candidate counts only if the full SHA-256 matches. Only hashes and
//! addresses are kept here, never code bytes.

use super::sha256::{hex, sha256};

/// Description of one accelerated routine.
pub struct Spec {
    pub name: &'static str,
    /// Bytes hashed from the entry: code plus literal pool.
    pub len: u32,
    /// Lower-case hex SHA-256 of those bytes.
    pub sha256: &'static str,
    /// Entry addresses in the known images (TRITON / NEPTUN main 5.8), tried before scanning.
    pub hints: &'static [u32],
    /// [`anchor_hash`] of the first eight bytes (scan filter).
    pub anchor: u32,
    /// Argument registers (bit i = r_i, r0..r3) the routine reads: the memo key.
    pub core_key: u16,
    /// Argument S registers (bit i = s_i, s0..s1).
    pub s_key: u32,
    /// The routine uses VFP: the FPSCR control fields join the key and the FP context must be live.
    pub fp: bool,
}

/// Hash of the first eight bytes of a routine: a cheap scan filter.
pub fn anchor_hash(first8: &[u8]) -> u32 {
    let mut b = [0u8; 8];
    let n = first8.len().min(8);
    b[..n].copy_from_slice(&first8[..n]);
    ((u64::from_le_bytes(b).wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 32) as u32
}

pub const SPECS: &[Spec] = &[
    Spec { name: "ddiv", len: 464, sha256: "6b4fac8790015d7e6e944492c01946b137215536ce6f46025f44a2b487ded4cb", hints: &[0x0800_486c], anchor: 0x0c28_1962, core_key: 0xF, s_key: 0, fp: false },
    Spec { name: "f2d", len: 66, sha256: "0a095c14d3a5744a4c566cfc2921c22c80a16f3f786cd7195891b7f617b94c73", hints: &[0x0800_4568], anchor: 0x0f9e_5ec4, core_key: 0x1, s_key: 0, fp: false },
    Spec { name: "d2f", len: 158, sha256: "bf8cad5c441debdaba9579a2f86faf5f691e45e19ad85f06619a6622c695dee1", hints: &[0x0800_4c08], anchor: 0x9574_1114, core_key: 0x3, s_key: 0, fp: false },
    Spec { name: "unorddf2", len: 44, sha256: "2f92f4985a2a53cdd4afd8c41e5a49922cb187a30260b3922776b69a77c186ce", hints: &[0x0800_4b4c], anchor: 0x42ad_aebd, core_key: 0xF, s_key: 0, fp: false },
    // TRITON main 5.8 at 0x0802be20, NEPTUN main 5.8 at 0x0804bfe8: the same bytes.
    Spec { name: "isfinitef", len: 20, sha256: "acdf2eedc62c324f353aaabf8597d75b0f1f1548e33fec0fb739f204bbb6e7d8", hints: &[0x0802_be20, 0x0804_bfe8], anchor: 0x9493_8b92, core_key: 0, s_key: 0x1, fp: true },
    // `expf` and its worker `__ieee754_expf`: the same code in NEPTUN, but its literal pools and branch targets differ
    // (another layout), so NEPTUN's copies have their own hashes (the second pair of entries).
    Spec { name: "expf_core", len: 488, sha256: "ebd11f2d3c812c7dc80ddf53ad0e25c022cec5934a94b6abb7c8e0047ce63293", hints: &[0x0802_cc7c], anchor: 0xbe93_8b92, core_key: 0, s_key: 0x1, fp: true },
    Spec { name: "expf", len: 120, sha256: "7a11dd0db1d360fc9cd2711b7afe407acc96f732787404162a5f6a2441f917cb", hints: &[0x0802_bc90], anchor: 0x8fa3_a8ec, core_key: 0, s_key: 0x1, fp: true },
    Spec { name: "expf_core", len: 488, sha256: "21fe024d57b8d07f6e89462070ad38b091c81f185284261fa5d060c1a5944281", hints: &[0x0804_ce44], anchor: 0xbe93_8b92, core_key: 0, s_key: 0x1, fp: true },
    Spec { name: "expf", len: 120, sha256: "1d4b099683d3bb96df8a19b561bee9665b649fa579aa4b50e8e84def383f25f4", hints: &[0x0804_be58], anchor: 0x8fa3_a8ec, core_key: 0, s_key: 0x1, fp: true },
];

/// A routine found in an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// Index into [`SPECS`].
    pub spec: usize,
    pub name: &'static str,
    pub entry: u32,
    pub len: u32,
}

fn matches_at(spec: &Spec, bytes: &[u8], offset: usize) -> bool {
    let end = offset + spec.len as usize;
    end <= bytes.len() && hex(&sha256(&bytes[offset..end])) == spec.sha256
}

/// Finds the routines of [`SPECS`] in a code region that starts at address `base`.
pub fn scan(base: u32, bytes: &[u8]) -> Vec<Match> {
    scan_specs(SPECS, base, bytes)
}

/// [`scan`] for another routine table (the machinery's tests use synthetic routines).
pub fn scan_specs(specs: &[Spec], base: u32, bytes: &[u8]) -> Vec<Match> {
    let mut found = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        if spec.sha256.is_empty() {
            continue;
        }
        let mut hit = false;
        for &hint in spec.hints {
            let offset = hint.wrapping_sub(base) as usize;
            if offset < bytes.len() && matches_at(spec, bytes, offset) {
                found.push(Match { spec: index, name: spec.name, entry: hint, len: spec.len });
                hit = true;
            }
        }
        if hit {
            continue;
        }
        let mut offset = 0usize;
        while offset + 8 <= bytes.len() {
            if anchor_hash(&bytes[offset..offset + 8]) == spec.anchor && matches_at(spec, bytes, offset) {
                found.push(Match { spec: index, name: spec.name, entry: base.wrapping_add(offset as u32), len: spec.len });
            }
            offset += 2;
        }
    }
    found
}
