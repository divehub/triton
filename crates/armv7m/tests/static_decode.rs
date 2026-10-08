//! Static decode regression over both firmware images:
//!  * every instruction start that a Ghidra analysis of the images listed must decode (only the
//!    compiler's permanently-undefined `UDF` encodings and the two non-FP `stc` data words are
//!    rejected) and coprocessor encodings must resolve through the FPU decoder. The start addresses
//!    are committed as one bit per halfword in `testdata/instruction-starts-<role>.txt` (no bytes, no
//!    mnemonics; derived by `testdata/tools/instruction_starts.py` from the listing of the Renode-based analysis workspace (not public)); the
//!    instruction bytes come from the user's local SREC;
//!  * Ghidra misses real VFP instructions inside two functions (handset 0x080369fa..0x080372b2,
//!    main 0x0801d9da..0x0801da70); those are checked from the raw image bytes against a table of
//!    addresses taken from `arm-none-eabi-objdump` (mnemonic roots included).
//! Skipped gracefully when the (local, ignored) SRECs are absent.

mod common;
use armv7m::decode::decode;
use armv7m::op::Kind;
use armv7m::vfp;
use common::*;

/// The committed instruction-start addresses of an image (`role` = `main` | `handset`).
fn instruction_starts(role: &str, base: u32) -> Vec<u32> {
    let path = format!("{}/../../testdata/instruction-starts-{role}.txt", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let hex: String = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(hex.len() % 2, 0, "{path}");
    let mut starts = Vec::new();
    for (byte_index, pair) in hex.as_bytes().chunks(2).enumerate() {
        let byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        for bit in 0..8 {
            if byte >> bit & 1 != 0 {
                starts.push(base + 2 * (byte_index as u32 * 8 + bit));
            }
        }
    }
    starts
}

fn image(srec: &str) -> Option<(u32, Vec<u8>)> {
    let text = std::fs::read_to_string(repo_path(srec)).ok()?;
    let segs = parse_srec(&text);
    let lo = segs.iter().map(|s| s.0).min()?;
    let hi = segs.iter().map(|s| s.0 + s.1.len() as u32).max()?;
    let mut bytes = vec![0xFFu8; (hi - lo) as usize];
    for (a, d) in &segs {
        bytes[(*a - lo) as usize..(*a - lo) as usize + d.len()].copy_from_slice(d);
    }
    Some((lo, bytes))
}

/// Decodes every committed instruction start of an image; returns (total, unexpected rejections).
fn sweep_starts(starts: &[u32], base: u32, bytes: &[u8]) -> (usize, Vec<String>) {
    let mut bad = Vec::new();
    for &addr in starts {
        let off = (addr - base) as usize;
        let hw1 = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        // Thumb-2 (32-bit) encodings start with 0b11101, 0b11110 or 0b11111.
        let wide = hw1 >> 11 >= 0b11101;
        let hw2 = if wide { u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]) } else { 0 };
        let shown = if wide { format!("{hw1:04x} {hw2:04x}") } else { format!("{hw1:04x}") };
        let op = decode(addr, hw1, hw2);
        let is_udf16 = !wide && hw1 >> 8 == 0xDE;
        let is_udf32 = wide && hw1 & 0xFFF0 == 0xF7F0 && hw2 & 0xF000 == 0xA000;
        match op.kind {
            Kind::Undefined if is_udf16 || is_udf32 => {}
            Kind::Undefined => bad.push(format!("{addr:08x}: {shown} rejected")),
            Kind::Coproc => match vfp::decode(hw1, hw2) {
                vfp::VfpDecode::Insn(_) => {}
                vfp::VfpDecode::NotVfp if hw1 & 0xFE50 == 0xEC00 && hw2 >> 8 == 0xE0 => {} // stc p0, c14 data words
                other => bad.push(format!("{addr:08x}: {shown} VFP decode {other:?}")),
            },
            _ => {}
        }
    }
    (starts.len(), bad)
}

#[test]
fn listed_instruction_starts_decode() {
    for (role, srec, min) in [
        ("handset", "firmware/TRITON-5.8-65.3/ngc_handset_65.3_TRITON.srec", 130_000usize),
        ("main", "firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec", 57_000),
    ] {
        let Some((base, bytes)) = image(srec) else {
            eprintln!("skipping {role}: SREC not present");
            continue;
        };
        let starts = instruction_starts(role, base);
        let (total, bad) = sweep_starts(&starts, base, &bytes);
        eprintln!("{role}: {total} listed instructions decoded, {} unexpected rejections", bad.len());
        for b in bad.iter().take(20) {
            eprintln!("  {b}");
        }
        assert!(total >= min, "{role}: only {total} instruction starts in the committed data");
        assert!(bad.is_empty(), "{role}: {} unexpected rejections", bad.len());
    }
}

/// `(address, hw1, hw2, mnemonic root)` of VFP instructions missing from the Ghidra listings
/// (taken from `arm-none-eabi-objdump -M force-thumb` of the extracted images).
const HANDSET_MISSED: &[(u32, u16, u16, &str)] = &[
    (0x080369fa, 0xed2d, 0x8b02, "vpush"),
    (0x08036ad2, 0xecbd, 0x8b02, "vpop"),
    (0x08036b40, 0xecbd, 0x8b02, "vpop"),
    (0x08036b6a, 0xee00, 0x3a10, "vmov"),
    (0x08036b70, 0xecbd, 0x8b02, "vpop"),
    (0x08036ba4, 0xecbd, 0x8b02, "vpop"),
    (0x08036be0, 0xeddf, 0x7aae, "vldr"),
    (0x08036be6, 0xee00, 0x3a10, "vmov"),
    (0x08036bea, 0xeeb8, 0x0a40, "vcvt"),
    (0x08036bee, 0xee80, 0x0a27, "vdiv"),
    (0x08036bf4, 0xecbd, 0x8b02, "vpop"),
    (0x08036c02, 0xeddf, 0x7aa6, "vldr"),
    (0x08036c08, 0xee00, 0x3a10, "vmov"),
    (0x08036c0c, 0xeeb8, 0x0a40, "vcvt"),
    (0x08036c10, 0xee80, 0x0a27, "vdiv"),
    (0x08036c16, 0xecbd, 0x8b02, "vpop"),
    (0x08036c90, 0xecbd, 0x8b02, "vpop"),
    (0x08036cc0, 0xecbd, 0x8b02, "vpop"),
    (0x08036cd2, 0xecbd, 0x8b02, "vpop"),
    (0x08036d52, 0xeddf, 0x7a52, "vldr"),
    (0x08036d58, 0xee00, 0x3a10, "vmov"),
    (0x08036d5c, 0xeeb8, 0x0ac0, "vcvt"),
    (0x08036d60, 0xee80, 0x0a27, "vdiv"),
    (0x08036d66, 0xecbd, 0x8b02, "vpop"),
    (0x08036e6e, 0xecbd, 0x8b02, "vpop"),
    (0x080370dc, 0xed93, 0x0a04, "vldr"),
    (0x080370ee, 0xed83, 0x0a0d, "vstr"),
    (0x080370fc, 0xedd3, 0x0a0e, "vldr"),
    (0x0803710c, 0xeef0, 0x0a40, "vmov"),
    (0x08037112, 0xed83, 0x0a0e, "vstr"),
    (0x0803711e, 0xed93, 0x0a0d, "vldr"),
    (0x080372b2, 0xed93, 0x0a06, "vldr"),
];

const MAIN_MISSED: &[(u32, u16, u16, &str)] = &[
    (0x0801d9da, 0xed82, 0x0a00, "vstr"),
    (0x0801d9e4, 0xedd3, 0x7a00, "vldr"),
    (0x0801d9e8, 0xeef4, 0x7a40, "vcmp"),
    (0x0801d9ec, 0xeef1, 0xfa10, "vmrs"),
    (0x0801da4a, 0xeef7, 0x0a00, "vmov"),
    (0x0801da50, 0xed9f, 0x0a10, "vldr"),
    (0x0801da70, 0xed9f, 0x0a09, "vldr"),
];

fn check_missed(role: &str, srec: &str, table: &[(u32, u16, u16, &str)]) {
    let Some((lo, bytes)) = image(srec) else {
        eprintln!("skipping {role}: SREC not present");
        return;
    };
    for &(addr, hw1, hw2, root) in table {
        let off = (addr - lo) as usize;
        let img1 = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        let img2 = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]);
        assert_eq!((img1, img2), (hw1, hw2), "{role} {addr:08x}: image bytes differ from the table");
        let op = decode(addr, img1, img2);
        assert_eq!(op.kind, Kind::Coproc, "{role} {addr:08x}");
        match vfp::decode(img1, img2) {
            vfp::VfpDecode::Insn(_) => {}
            other => panic!("{role} {addr:08x}: {other:?}"),
        }
        let text = vfp::disassemble(img1, img2);
        assert!(text.starts_with(root), "{role} {addr:08x}: disassembly '{text}' does not start with '{root}'");
    }
    eprintln!("{role}: {} Ghidra-missed VFP instructions decode", table.len());
}

#[test]
fn ghidra_missed_vfp_instructions_decode() {
    check_missed("handset", "firmware/TRITON-5.8-65.3/ngc_handset_65.3_TRITON.srec", HANDSET_MISSED);
    check_missed("main", "firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec", MAIN_MISSED);
}
