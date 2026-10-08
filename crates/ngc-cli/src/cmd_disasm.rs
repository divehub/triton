//! `ngc-cli disasm`: lists instructions of a firmware image (for divergence reports).

use crate::args;
use crate::common;
use ngc::firmware::Role;
use std::io::Write;

pub const USAGE: &str = "ngc-cli disasm [--board handset|main] [--main <srec>] [--handset <srec>] <address> [count]\n  \
    Disassembles `count` (default 16) instructions of the firmware image starting at `address` (hex, 0x prefix\n  \
    optional) with the engine's decoder; the image is placed at 0x08004000 like on the boards.";

pub fn run(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, out) {
        Ok(()) => 0,
        Err((true, message)) => {
            let _ = writeln!(err, "ngc-cli disasm: {message}\n{USAGE}");
            2
        }
        Err((false, message)) => {
            let _ = writeln!(err, "ngc-cli disasm: {message}");
            1
        }
    }
}

fn run_inner(argv: &[String], out: &mut dyn Write) -> Result<(), (bool, String)> {
    let usage = |message: String| (true, message);
    let parsed = args::parse(argv, &["board", "main", "handset"], &[]).map_err(usage)?;
    let board = parsed.value("board").unwrap_or("handset");
    let role = match board {
        "handset" => Role::Handset,
        "main" => Role::Main,
        other => return Err(usage(format!("--board must be handset or main (got '{other}')"))),
    };
    let address_text = parsed.positional.first().ok_or_else(|| usage("missing address".to_string()))?;
    let address = u32::from_str_radix(address_text.trim_start_matches("0x"), 16).map_err(|_| usage(format!("'{address_text}' is not a hex address")))?;
    let count: usize = match parsed.positional.get(1) {
        Some(text) => text.parse().map_err(|_| usage(format!("'{text}' is not a count")))?,
        None => 16,
    };
    let explicit = if role == Role::Handset { parsed.value("handset") } else { parsed.value("main") };
    let path = common::firmware_path(explicit, role).map_err(|e| (false, e))?;
    let firmware = common::load_firmware(&path, role).map_err(|e| (false, e))?;
    let image = firmware.flash_image();
    let base = 0x0800_0000u32;
    let mut pc = address & !1;
    let mut disassembler = armv7m::disasm::Disassembler::new();
    for _ in 0..count {
        let offset = pc.wrapping_sub(base) as usize;
        if offset + 2 > image.len() {
            break;
        }
        let hw1 = u16::from_le_bytes([image[offset], image[offset + 1]]);
        let hw2 = if offset + 4 <= image.len() { u16::from_le_bytes([image[offset + 2], image[offset + 3]]) } else { 0 };
        let (text, length) = disassembler.next(pc, hw1, hw2);
        let raw = if length == 4 { format!("{hw1:04x} {hw2:04x}") } else { format!("{hw1:04x}     ") };
        let _ = writeln!(out, "{pc:08x}:  {raw}  {text}");
        pc = pc.wrapping_add(length as u32);
    }
    Ok(())
}
