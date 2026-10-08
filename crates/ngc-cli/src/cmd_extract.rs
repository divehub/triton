//! `ngc-cli extract-bin --main <srec> --handset <srec> --out-dir <dir> [--force]`
//!
//! Writes the verified binary spans exactly as Renode loaded them (`sysbus LoadBinary ... 0x08004000`
//! in the analysis workspace): `<dir>/main/firmware.bin` and `<dir>/handset/firmware.bin`. Existing files are never
//! overwritten: identical content is reported as unchanged, different content is an error (`--force`
//! replaces it).

use crate::args;
use ngc::firmware::{self, Role};
use ngc::sha256;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "ngc-cli extract-bin [--main <srec>] [--handset <srec>] --out-dir <dir> [--force]\n  \
    Write <dir>/main/firmware.bin and <dir>/handset/firmware.bin (the binary spans Renode loads at\n  \
    0x08004000). Each SREC is identified and verified first. Existing files are not overwritten\n  \
    unless --force is given.";

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Written,
    Unchanged,
}

fn write_binary(path: &Path, bytes: &[u8], force: bool) -> Result<Outcome, String> {
    if path.exists() {
        let existing = std::fs::read(path).map_err(|e| format!("cannot read existing {}: {e}", path.display()))?;
        if existing == bytes {
            return Ok(Outcome::Unchanged);
        }
        if !force {
            return Err(format!("{} already exists with different content (use --force to replace it)", path.display()));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(Outcome::Written)
}

pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match args::parse(args, &["main", "handset", "out-dir"], &["force"]) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(err, "ngc-cli extract-bin: {e}\n{USAGE}");
            return 2;
        }
    };
    let out_dir = match parsed.require("out-dir") {
        Ok(d) => PathBuf::from(d),
        Err(e) => {
            let _ = writeln!(err, "ngc-cli extract-bin: {e}\n{USAGE}");
            return 2;
        }
    };
    if parsed.value("main").is_none() && parsed.value("handset").is_none() {
        let _ = writeln!(err, "ngc-cli extract-bin: give at least one of --main / --handset\n{USAGE}");
        return 2;
    }
    let mut status = 0;
    for (role, path) in [(Role::Main, parsed.value("main")), (Role::Handset, parsed.value("handset"))] {
        let Some(path) = path else { continue };
        let result = std::fs::read(path)
            .map_err(|e| format!("cannot read {path}: {e}"))
            .and_then(|bytes| firmware::load(&bytes, Some(role)).map_err(|e| format!("{path}: {e}")))
            .and_then(|fw| {
                let target = out_dir.join(role.name()).join("firmware.bin");
                let outcome = write_binary(&target, fw.bin(), parsed.flag("force"))?;
                Ok((fw, target, outcome))
            });
        match result {
            Ok((fw, target, outcome)) => {
                let verb = match outcome {
                    Outcome::Written => "wrote",
                    Outcome::Unchanged => "unchanged",
                };
                let _ = writeln!(
                    out,
                    "{role}: {verb} {} ({} bytes, 0x{:08x}..0x{:08x}, sha256 {})",
                    target.display(),
                    fw.bin().len(),
                    fw.span_base,
                    u64::from(fw.span_base) + fw.bin().len() as u64,
                    sha256::to_hex(&fw.bin_sha256)
                );
            }
            Err(e) => {
                let _ = writeln!(err, "ngc-cli extract-bin: {role}: {e}");
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn firmware_dir() -> Option<PathBuf> {
        // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
        let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
        roots
            .into_iter()
            .flatten()
            .map(|root| root.join("TRITON-5.8-65.3"))
            .find(|d| d.join("ngc_main_5.8_TRITON.srec").is_file() && d.join("ngc_handset_65.3_TRITON.srec").is_file())
    }

    /// A fresh directory next to the test executable (inside the cargo target directory).
    fn scratch(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("test executable path");
        let dir = exe.parent().expect("target dir").join("ngc-cli-tests").join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn run_args(items: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = run(&args, &mut out, &mut err);
        (status, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn usage_errors() {
        assert_eq!(run_args(&[]).0, 2);
        assert_eq!(run_args(&["--out-dir", "x"]).0, 2);
        assert_eq!(run_args(&["--main", "x"]).0, 2);
        let (status, _, err) = run_args(&["--main", "/nonexistent.srec", "--out-dir", "/nonexistent-out"]);
        assert_eq!(status, 1);
        assert!(err.contains("cannot read"));
    }

    #[test]
    fn extracts_identical_spans_without_overwriting() {
        let Some(dir) = firmware_dir() else {
            eprintln!("skipping: firmware not available");
            return;
        };
        let main = dir.join("ngc_main_5.8_TRITON.srec");
        let handset = dir.join("ngc_handset_65.3_TRITON.srec");
        let out_dir = scratch("extract");
        let out_str = out_dir.to_str().unwrap();
        let (status, out, err) = run_args(&["--main", main.to_str().unwrap(), "--handset", handset.to_str().unwrap(), "--out-dir", out_str]);
        assert_eq!(status, 0, "{err}");
        assert!(out.contains("wrote") && out.contains("186680 bytes") && out.contains("706380 bytes"), "{out}");
        let main_bin = std::fs::read(out_dir.join("main/firmware.bin")).unwrap();
        let handset_bin = std::fs::read(out_dir.join("handset/firmware.bin")).unwrap();
        assert_eq!(sha256::digest_hex(&main_bin), "76af4ba51029afa93e788fa11bca74bad5dea7be7b14b0f8ee59baea5bfd014d");
        assert_eq!(sha256::digest_hex(&handset_bin), "f9a85fb016081dae1e7e3c7e3007637889557df7b0e3a8142b574c21f91b9d57");
        // Re-running is idempotent.
        let (status, out, _) = run_args(&["--main", main.to_str().unwrap(), "--out-dir", out_str]);
        assert_eq!(status, 0);
        assert!(out.contains("unchanged"), "{out}");
        // A differing file is protected unless forced.
        std::fs::write(out_dir.join("main/firmware.bin"), b"different").unwrap();
        let (status, _, err) = run_args(&["--main", main.to_str().unwrap(), "--out-dir", out_str]);
        assert_eq!(status, 1);
        assert!(err.contains("already exists with different content"), "{err}");
        assert_eq!(std::fs::read(out_dir.join("main/firmware.bin")).unwrap(), b"different");
        let (status, _, _) = run_args(&["--main", main.to_str().unwrap(), "--out-dir", out_str, "--force"]);
        assert_eq!(status, 0);
        assert_eq!(std::fs::read(out_dir.join("main/firmware.bin")).unwrap(), main_bin);
        // Wrong slot is refused before anything is written.
        let wrong = scratch("wrong");
        let (status, _, err) = run_args(&["--main", handset.to_str().unwrap(), "--out-dir", wrong.to_str().unwrap()]);
        assert_eq!(status, 1);
        assert!(err.contains("handset firmware") || err.contains("handset image") || err.contains("this is the handset"), "{err}");
        assert!(!wrong.join("main/firmware.bin").exists());
        let _ = std::fs::remove_dir_all(&out_dir);
    }
}
