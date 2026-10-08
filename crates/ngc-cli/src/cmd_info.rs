//! `ngc-cli info --main <srec> --handset <srec> [--vectors] [--json]`
//!
//! Identity, hashes, segments and vector table of the two firmware images, with the verification
//! checks of `ngc::firmware::inspect`. Exit status: 0 all supplied images verified, 1 a check
//! failed or a file could not be read, 2 usage error.

use crate::args;
use emu_core::Json;
use ngc::firmware::{self, Report, Role, VectorInfo};
use std::io::Write;

pub const USAGE: &str = "ngc-cli info --main <srec> --handset <srec> [--vectors] [--json]\n  \
    Print identity, SHA-256 hashes, segments, binary span and vector table of the firmware images\n  \
    and verify them against the recorded facts. At least one of --main / --handset is required.\n  \
    --vectors  list all 99 vector table entries\n  \
    --json     machine-readable output";

const SYSTEM_VECTORS: [&str; 16] = [
    "initial SP",
    "Reset",
    "NMI",
    "HardFault",
    "MemManage",
    "BusFault",
    "UsageFault",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "SVCall",
    "DebugMon",
    "reserved",
    "PendSV",
    "SysTick",
];

pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match args::parse(args, &["main", "handset"], &["vectors", "json"]) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(err, "ngc-cli info: {e}\n{USAGE}");
            return 2;
        }
    };
    if !parsed.positional.is_empty() {
        let _ = writeln!(err, "ngc-cli info: unexpected argument '{}'\n{USAGE}", parsed.positional[0]);
        return 2;
    }
    let inputs: Vec<(Role, &str)> = [(Role::Main, parsed.value("main")), (Role::Handset, parsed.value("handset"))]
        .into_iter()
        .filter_map(|(role, path)| path.map(|p| (role, p)))
        .collect();
    if inputs.is_empty() {
        let _ = writeln!(err, "ngc-cli info: give at least one of --main / --handset\n{USAGE}");
        return 2;
    }

    let mut status = 0;
    let mut json = Json::object();
    for (slot, path) in inputs {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                let _ = writeln!(err, "ngc-cli info: cannot read {path}: {e}");
                status = 1;
                continue;
            }
        };
        let report = firmware::inspect(&bytes);
        let slot_ok = report.ok() && report.identified == Some(slot);
        if !slot_ok {
            status = 1;
        }
        if parsed.flag("json") {
            let mut entry = report.to_json();
            entry.insert("path", path);
            entry.insert("slot", slot.name());
            entry.insert("slotOk", slot_ok);
            json.insert(slot.name(), entry);
        } else {
            print_report(out, slot, path, &report, parsed.flag("vectors"));
            if let Some(found) = report.identified {
                if found != slot {
                    let _ = writeln!(out, "  WARNING: this file is the {found} image but was given as --{slot}");
                }
            }
        }
    }
    if parsed.flag("json") {
        let _ = writeln!(out, "{}", json.to_pretty_string());
    }
    status
}

fn print_report(out: &mut dyn Write, slot: Role, path: &str, report: &Report, all_vectors: bool) {
    let _ = writeln!(out, "{slot}: {path}");
    let _ = writeln!(out, "  file:        {} bytes", report.srec_bytes);
    let _ = writeln!(out, "  sha256:      {}", report.srec_sha256);
    match (report.identified, report.release) {
        (Some(role), Some(release)) => {
            let e = release.expected(role);
            let _ = writeln!(out, "  identity:    {} ({}); release {} ({})", e.label, e.file_name, release.id, release.label);
        }
        (Some(role), None) => {
            let _ = writeln!(out, "  identity:    {} (release unknown)", role.expected().label);
        }
        (None, _) => {
            let _ = writeln!(out, "  identity:    UNRECOGNIZED (not main 5.8 or handset 65.3 of a supported release)");
        }
    }
    if let Some(error) = &report.parse_error {
        let _ = writeln!(out, "  parse error: {error}");
    } else {
        let _ = writeln!(out, "  header:      {}", report.header);
        let counts: Vec<String> = report
            .record_counts
            .iter()
            .enumerate()
            .filter(|(_, &n)| n > 0)
            .map(|(digit, n)| format!("S{digit} x{n}"))
            .collect();
        let _ = writeln!(out, "  records:     {}", counts.join(", "));
        for (i, &(start, end)) in report.segments.iter().enumerate() {
            let _ = writeln!(out, "  segment {i}:   0x{start:08x}..0x{end:08x} ({} bytes)", end - start);
        }
        if let Some((start, end)) = report.span {
            let _ = writeln!(out, "  span:        0x{start:08x}..0x{end:08x} ({} bytes, holes filled with 0xFF)", end - start);
        }
        if let Some(hash) = &report.bin_sha256 {
            let _ = writeln!(out, "  bin sha256:  {hash}");
        }
        if let Some(entry) = report.entry {
            let _ = writeln!(out, "  S-record entry: 0x{entry:08x}");
        }
        if let Some(vectors) = &report.vectors {
            print_vectors(out, vectors, all_vectors);
        }
    }
    let _ = writeln!(out, "  checks:");
    for check in &report.checks {
        let _ = writeln!(out, "    {} {:<14} {}", if check.ok { "ok  " } else { "FAIL" }, check.name, check.detail);
    }
    let _ = writeln!(out, "  result:      {}", if report.ok() { "verified" } else { "NOT VERIFIED" });
}

fn print_vectors(out: &mut dyn Write, vectors: &VectorInfo, all: bool) {
    let _ = writeln!(
        out,
        "  vectors:     initial SP 0x{:08x}, reset PC 0x{:08x} (vector 0x{:08x}), {} entries",
        vectors.initial_sp,
        vectors.reset_pc(),
        vectors.reset_vector,
        vectors.entries.len()
    );
    let mut distinct: Vec<u32> = vectors.entries[16..].to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let _ = writeln!(out, "               {} external IRQ entries, {} distinct targets", vectors.entries.len() - 16, distinct.len());
    if all {
        for (i, &word) in vectors.entries.iter().enumerate() {
            let label = if i < 16 { SYSTEM_VECTORS[i].to_string() } else { format!("IRQ {}", i - 16) };
            let _ = writeln!(out, "    [{i:2}] 0x{word:08x}  {label}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn firmware_dir() -> Option<PathBuf> {
        // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
        let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
        roots
            .into_iter()
            .flatten()
            .map(|root| root.join("TRITON-5.8-65.3"))
            .find(|d| d.join("ngc_main_5.8_TRITON.srec").is_file() && d.join("ngc_handset_65.3_TRITON.srec").is_file())
    }

    fn run_args(items: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = run(&args, &mut out, &mut err);
        (status, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn usage_errors() {
        let (status, _, err) = run_args(&[]);
        assert_eq!(status, 2);
        assert!(err.contains("at least one of --main"));
        let (status, _, err) = run_args(&["--bogus"]);
        assert_eq!(status, 2);
        assert!(err.contains("unknown option"));
        let (status, _, err) = run_args(&["--main", "/nonexistent/file.srec"]);
        assert_eq!(status, 1);
        assert!(err.contains("cannot read"));
    }

    #[test]
    fn describes_both_real_images() {
        let Some(dir) = firmware_dir() else {
            eprintln!("skipping: firmware not available");
            return;
        };
        let main = dir.join("ngc_main_5.8_TRITON.srec");
        let handset = dir.join("ngc_handset_65.3_TRITON.srec");
        let (status, out, err) = run_args(&["--main", main.to_str().unwrap(), "--handset", handset.to_str().unwrap()]);
        assert_eq!(status, 0, "{err}\n{out}");
        assert!(out.contains("main 5.8"), "{out}");
        assert!(out.contains("handset 65.3"), "{out}");
        assert!(out.contains("838cb050fa572dddca3153f43a1768db0a0665db4cde0567749fb7be8d18d6ea"));
        assert!(out.contains("76af4ba51029afa93e788fa11bca74bad5dea7be7b14b0f8ee59baea5bfd014d"));
        assert!(out.contains("f9a85fb016081dae1e7e3c7e3007637889557df7b0e3a8142b574c21f91b9d57"));
        assert!(out.contains("reset PC 0x080213b8") && out.contains("reset PC 0x08008410"), "{out}");
        assert!(out.contains("0x08004000..0x0800418c") && out.contains("0x08004190..0x08031938"), "{out}");
        assert!(!out.contains("FAIL"), "{out}");
        // Swapped slots are reported.
        let (status, out, _) = run_args(&["--main", handset.to_str().unwrap()]);
        assert_eq!(status, 1);
        assert!(out.contains("WARNING: this file is the handset image but was given as --main"), "{out}");
        // Full vector listing and JSON.
        let (status, out, _) = run_args(&["--main", main.to_str().unwrap(), "--vectors"]);
        assert_eq!(status, 0);
        assert!(out.contains("[ 1] 0x080213b9  Reset") && out.contains("IRQ 82"), "{out}");
        let (status, out, _) = run_args(&["--main", main.to_str().unwrap(), "--json"]);
        assert_eq!(status, 0);
        let json = Json::parse(&out).unwrap();
        assert_eq!(json.get("main").unwrap().get("slotOk").and_then(Json::as_bool), Some(true));
        assert_eq!(json.get("main").unwrap().get("role").and_then(Json::as_str), Some("main"));
    }
}
