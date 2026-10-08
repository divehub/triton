//! Development aid for `scripts/objdump_crosscheck.py` (ignored by default).
//!
//! Reads a binary of little-endian halfword pairs from `$NGC_VFP_DUMP_IN`
//! and writes one line per pair to `$NGC_VFP_DUMP_OUT`: the disassembly of the
//! decoded instruction, `-` for `Undefined` and `=` for `NotVfp`.
//!
//! Run via the script, or by hand:
//! `NGC_VFP_DUMP_IN=in.bin NGC_VFP_DUMP_OUT=out.txt cargo test -p armv7m-vfp --release -- --ignored objdump_dump`

use armv7m_vfp::{decode, VfpDecode};
use std::io::Write;

#[test]
#[ignore]
fn objdump_dump() {
    let (Ok(input), Ok(output)) = (std::env::var("NGC_VFP_DUMP_IN"), std::env::var("NGC_VFP_DUMP_OUT"))
    else {
        println!("NGC_VFP_DUMP_IN / NGC_VFP_DUMP_OUT not set; nothing to do");
        return;
    };
    let data = std::fs::read(input).expect("read input");
    assert_eq!(data.len() % 4, 0);
    let mut out = std::io::BufWriter::new(std::fs::File::create(output).expect("create output"));
    for chunk in data.chunks_exact(4) {
        let hw1 = u16::from_le_bytes([chunk[0], chunk[1]]);
        let hw2 = u16::from_le_bytes([chunk[2], chunk[3]]);
        match decode(hw1, hw2) {
            VfpDecode::Insn(i) => writeln!(out, "{}", i.disassemble()).unwrap(),
            VfpDecode::Undefined => writeln!(out, "-").unwrap(),
            VfpDecode::NotVfp => writeln!(out, "=").unwrap(),
        }
    }
    out.flush().unwrap();
}
