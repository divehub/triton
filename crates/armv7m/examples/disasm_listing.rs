//! Disassembles every instruction of a Ghidra listing (`address | bytes | text`)
//! with this crate's decoder and prints `address<TAB>bytes<TAB>my text`, plus a
//! summary of encodings the decoder rejects on stderr. Used by
//! `tests/tools/compare_objdump.py` (decoder validation against objdump) and by the
//! "decode every listed instruction" regression test.
//!
//! Usage: `cargo run -p armv7m --example disasm_listing -- <disassembly.txt>`

use armv7m::decode;
use armv7m::disasm::Disassembler;
use armv7m::op::Kind;

fn main() {
    let path = std::env::args().nth(1).expect("usage: disasm_listing <ghidra disassembly.txt>");
    let text = std::fs::read_to_string(&path).expect("cannot read listing");
    let mut dis = Disassembler::new();
    let mut expected_next: Option<u32> = None;
    let mut rejected: Vec<(u32, String)> = Vec::new();
    let mut total = 0usize;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').map(|p| p.trim()).collect();
        if parts.len() < 3 {
            continue;
        }
        let addr = match u32::from_str_radix(parts[0], 16) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let bytes: Vec<u8> = parts[1].split_whitespace().filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
        if bytes.len() < 2 {
            continue;
        }
        let hw1 = u16::from_le_bytes([bytes[0], bytes[1]]);
        let hw2 = if bytes.len() >= 4 { u16::from_le_bytes([bytes[2], bytes[3]]) } else { 0 };
        if expected_next != Some(addr) {
            dis = Disassembler::new();
        }
        let op = decode::decode(addr, hw1, hw2);
        if op.kind == Kind::Undefined {
            rejected.push((addr, parts[1].to_string()));
        }
        let (txt, len) = dis.next(addr, hw1, hw2);
        if len != bytes.len() {
            eprintln!("LENGTH MISMATCH at {:08x}: decoder says {} bytes, listing has {}", addr, len, bytes.len());
        }
        expected_next = Some(addr + len as u32);
        println!("{:08x}\t{}\t{}", addr, parts[1], txt);
        total += 1;
    }
    eprintln!("decoded {} instructions, {} rejected", total, rejected.len());
    for (a, b) in rejected.iter().take(50) {
        eprintln!("REJECTED {:08x}: {}", a, b);
    }
}
