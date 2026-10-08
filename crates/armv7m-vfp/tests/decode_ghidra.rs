//! Decode coverage against the real firmware: every 32-bit coprocessor-space
//! instruction in the Ghidra listings of the paired main 5.8 / handset 65.3
//! images must decode, and the debug disassembler must agree with Ghidra's
//! mnemonic and operands.
//!
//! The Ghidra exports are not part of this repository (they were produced in the
//! separate Renode-based analysis workspace, which is not public), so this check
//! is **opt-in**: set `NGC_FIRMWARE_DIR` to a firmware directory whose
//! `TRITON-5.8-65.3/static-analysis/{main,handset}/ghidra/disassembly.txt` exist.
//! Without it the test prints a note and passes (unless
//! `NGC_REQUIRE_FIRMWARE_DISASM=1` is set).
//! The instruction coverage of the real images that does not need the exports
//! lives in `crates/armv7m/tests/static_decode.rs`.

use armv7m_vfp::{decode, disassemble, VfpDecode};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CONDS: &[&str] = &[
    "eq", "ne", "cs", "hs", "cc", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le",
    "al",
];

fn find_disassembly(which: &str) -> Option<PathBuf> {
    let rel = format!("TRITON-5.8-65.3/static-analysis/{which}/ghidra/disassembly.txt");
    let dir = std::env::var("NGC_FIRMWARE_DIR").ok()?;
    let p = Path::new(&dir).join(&rel);
    p.is_file().then_some(p)
}

#[derive(Clone)]
struct Line {
    addr: u32,
    hw1: u16,
    hw2: u16,
    asm: String,
}

/// 32-bit coprocessor-space instructions of a listing.
fn coprocessor_lines(path: &Path) -> Vec<Line> {
    let text = std::fs::read_to_string(path).expect("read disassembly");
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, " | ");
        let (Some(addr), Some(bytes), Some(asm)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(addr) = u32::from_str_radix(addr.trim(), 16) else { continue };
        let b: Vec<u8> = bytes
            .split_whitespace()
            .filter_map(|x| u8::from_str_radix(x, 16).ok())
            .collect();
        if b.len() != 4 {
            continue;
        }
        let hw1 = u16::from_le_bytes([b[0], b[1]]);
        let hw2 = u16::from_le_bytes([b[2], b[3]]);
        if hw1 & 0xEC00 != 0xEC00 {
            continue;
        }
        out.push(Line { addr, hw1, hw2, asm: asm.trim().to_string() });
    }
    out
}

/// Ghidra puts the IT condition into the mnemonic (`vmov.eq.f32`): strip it.
fn strip_condition(asm: &str) -> String {
    let (mn, ops) = match asm.split_once(' ') {
        Some((m, o)) => (m, Some(o)),
        None => (asm, None),
    };
    let mut parts: Vec<&str> = mn.split('.').collect();
    if parts.len() > 1 && CONDS.contains(&parts[1]) {
        parts.remove(1);
    }
    let mn = parts.join(".");
    match ops {
        Some(o) => format!("{mn} {o}"),
        None => mn,
    }
}

fn is_cp1011(hw2: u16) -> bool {
    (hw2 >> 9) & 7 == 0b101
}

#[test]
fn firmware_vfp_instructions_decode_and_disassemble_like_ghidra() {
    let mut total = 0usize;
    let mut found_any = false;
    for which in ["handset", "main"] {
        let Some(path) = find_disassembly(which) else {
            println!("note: Ghidra listing for {which} not found; skipping");
            continue;
        };
        found_any = true;
        let lines = coprocessor_lines(&path);
        assert!(lines.len() > 1000, "{which}: only {} coprocessor instructions", lines.len());
        let mut by_mnemonic: BTreeMap<String, usize> = BTreeMap::new();
        let mut not_vfp = 0usize;
        let mut vcmp_quirk = 0usize;
        let mut mismatches = Vec::new();
        for l in &lines {
            let ghidra = strip_condition(&l.asm);
            match decode(l.hw1, l.hw2) {
                VfpDecode::Insn(insn) => {
                    assert!(is_cp1011(l.hw2), "{which} {:08x}: Insn for non CP10/11 {}", l.addr, l.asm);
                    assert_eq!(insn.encoding(), (l.hw1, l.hw2));
                    let mine = insn.disassemble();
                    *by_mnemonic.entry(mine.split(' ').next().unwrap().to_string()).or_default() += 1;
                    if mine == ghidra {
                        continue;
                    }
                    // Ghidra prints every VCMP/VCMPE as "vcmpe" even when the E bit
                    // (hw2 bit 7) is clear: a Ghidra mnemonic quirk, not a decode error.
                    if mine.starts_with("vcmp.f32 ") && ghidra == mine.replacen("vcmp", "vcmpe", 1) {
                        vcmp_quirk += 1;
                        continue;
                    }
                    mismatches.push(format!(
                        "{which} {:08x} {:04x} {:04x}: ghidra `{}` vs ours `{}`",
                        l.addr, l.hw1, l.hw2, l.asm, mine
                    ));
                }
                VfpDecode::NotVfp => {
                    assert!(!is_cp1011(l.hw2), "{which} {:08x}: NotVfp for CP10/11 {}", l.addr, l.asm);
                    not_vfp += 1;
                    // The only non-VFP coprocessor instruction in the images is a
                    // data word that Ghidra shows as `stc p0,cr14,[r8,#0x0]`.
                    assert!(l.asm.starts_with("stc "), "{which} {:08x}: unexpected `{}`", l.addr, l.asm);
                }
                VfpDecode::Undefined => {
                    mismatches.push(format!(
                        "{which} {:08x} {:04x} {:04x}: `{}` decoded as Undefined ({})",
                        l.addr,
                        l.hw1,
                        l.hw2,
                        l.asm,
                        disassemble(l.hw1, l.hw2)
                    ));
                }
            }
        }
        println!(
            "{which}: {} coprocessor-space instructions: {} VFP decoded, {} non-VFP, {} Ghidra vcmp/vcmpe naming quirks",
            lines.len(),
            lines.len() - not_vfp,
            not_vfp,
            vcmp_quirk
        );
        for (mn, c) in &by_mnemonic {
            println!("  {mn:12} {c}");
        }
        assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
        total += lines.len();
    }
    if !found_any {
        if std::env::var("NGC_REQUIRE_FIRMWARE_DISASM").as_deref() == Ok("1") {
            panic!("Ghidra disassembly listings not found");
        }
        println!("note: no Ghidra listings found, firmware decode coverage skipped");
    } else {
        println!("total coprocessor-space instructions checked: {total}");
    }
}
