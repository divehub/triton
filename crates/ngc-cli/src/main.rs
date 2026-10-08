//! Native runner/benchmark/validation CLI. See `DESIGN.md`.
//!
//! Commands: `info`, `extract-bin`, `run`, `bench`, `scenario` and `disasm`.

mod args;
mod cmd_bench;
mod cmd_disasm;
mod cmd_extract;
mod cmd_info;
mod cmd_run;
mod cmd_scenario;
mod common;
mod profile_files;

use std::io::Write;

const USAGE: &str = "usage: ngc-cli <command> [options]\n\n\
commands:\n  \
info               describe and verify the firmware SREC files\n  \
extract-bin        write the binary spans Renode loads\n  \
run                boot the original firmware on the emulated boards and run it\n  \
bench              boot, steady-state and button-redraw benchmark\n  \
scenario          run the validation scenarios (`scenario list`)\n  \
disasm             list instructions of a firmware image\n  \
help               show this text (or `ngc-cli <command> --help`)\n";

fn dispatch(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(command) = args.first() else {
        let _ = write!(err, "{USAGE}");
        return 2;
    };
    let rest = &args[1..];
    let wants_help = rest.iter().any(|a| a == "--help" || a == "-h");
    match command.as_str() {
        "info" if wants_help => {
            let _ = writeln!(out, "{}", cmd_info::USAGE);
            0
        }
        "extract-bin" if wants_help => {
            let _ = writeln!(out, "{}", cmd_extract::USAGE);
            0
        }
        "run" if wants_help => {
            let _ = writeln!(out, "{}", cmd_run::USAGE);
            0
        }
        "bench" if wants_help => {
            let _ = writeln!(out, "{}", cmd_bench::USAGE);
            0
        }
        "disasm" if wants_help => {
            let _ = writeln!(out, "{}", cmd_disasm::USAGE);
            0
        }
        "scenario" if wants_help => {
            let _ = writeln!(out, "{}", cmd_scenario::USAGE);
            0
        }
        "scenario" => cmd_scenario::run(rest, out, err),
        "disasm" => cmd_disasm::run(rest, out, err),
        "info" => cmd_info::run(rest, out, err),
        "extract-bin" => cmd_extract::run(rest, out, err),
        "run" => cmd_run::run(rest, out, err),
        "bench" => cmd_bench::run(rest, out, err),
        "help" | "--help" | "-h" => {
            let _ = write!(out, "{USAGE}");
            0
        }
        other => {
            let _ = writeln!(err, "ngc-cli: unknown command '{other}'\n");
            let _ = write!(err, "{USAGE}");
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let status = dispatch(&args, &mut stdout.lock(), &mut stderr.lock());
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(items: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = dispatch(&args, &mut out, &mut err);
        (status, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn dispatches_commands_and_help() {
        let (status, _, err) = run(&[]);
        assert_eq!(status, 2);
        assert!(err.contains("usage: ngc-cli"));
        let (status, out, _) = run(&["help"]);
        assert_eq!(status, 0);
        assert!(out.contains("extract-bin") && out.contains("scenario") && out.contains("bench") && !out.contains("compare-reference"));
        let (status, out, _) = run(&["info", "--help"]);
        assert_eq!(status, 0);
        assert!(out.starts_with("ngc-cli info"));
        let (status, out, _) = run(&["extract-bin", "-h"]);
        assert_eq!(status, 0);
        assert!(out.contains("--out-dir"));
        for command in ["run", "bench", "scenario"] {
            let (status, out, _) = run(&[command, "--help"]);
            assert_eq!(status, 0, "{command}");
            assert!(out.starts_with(&format!("ngc-cli {command}")), "{command}: {out}");
        }
        let (status, _, err) = run(&["frobnicate"]);
        assert_eq!(status, 2);
        assert!(err.contains("unknown command 'frobnicate'"));
        assert_eq!(run(&["info"]).0, 2);
        assert_eq!(run(&["run", "--bogus"]).0, 2);
    }

    #[test]
    fn usage_errors_of_the_sys_commands() {
        let (status, _, err) = run(&["run", "--mode", "sideways"]);
        assert_eq!(status, 2);
        assert!(err.contains("--mode must be dual or handset"), "{err}");
        let (status, _, err) = run(&["run", "--boot-mode", "warm"]);
        assert_eq!(status, 2);
        assert!(err.contains("--boot-mode"), "{err}");
        let (status, _, err) = run(&["run", "--pc-trace", "100"]);
        assert_eq!(status, 2);
        assert!(err.contains("needs an output file"), "{err}");
        let (status, _, err) = run(&["run", "--mode", "handset", "--board", "main", "--pc-trace", "5", "x"]);
        assert_eq!(status, 2);
        assert!(err.contains("--board main needs --mode dual"), "{err}");
        let (status, _, err) = run(&["bench", "--boot-seconds", "1"]);
        assert_eq!(status, 2);
        assert!(err.contains("--boot-seconds"), "{err}");
        let (status, _, err) = run(&["compare-reference"]);
        assert_eq!(status, 2, "the Renode comparison command is gone");
        assert!(err.contains("unknown command 'compare-reference'"), "{err}");
    }

    #[test]
    fn the_scenario_command_lists_the_suite_and_validates_its_arguments() {
        let (status, out, _) = run(&["scenario", "list"]);
        assert_eq!(status, 0);
        for name in ["dual-wake", "button-capture", "battery-setup", "diluent-menu", "clock-storage", "outputs-uart", "can-loss", "cold-wake", "machine-reset", "fast-forward"] {
            assert!(out.contains(name), "{name}: {out}");
        }
        let (status, out, _) = run(&["scenario", "--help"]);
        assert_eq!(status, 0);
        assert!(out.starts_with("ngc-cli scenario") && out.contains("--out"), "{out}");
        let (status, _, err) = run(&["scenario"]);
        assert_eq!(status, 2);
        assert!(err.contains("missing scenario name"), "{err}");
        let (status, _, err) = run(&["scenario", "nothing"]);
        assert_eq!(status, 2);
        assert!(err.contains("unknown scenario 'nothing'"), "{err}");
        let (status, _, err) = run(&["scenario", "dual-wake", "extra"]);
        assert_eq!(status, 2);
        assert!(err.contains("unexpected argument 'extra'"), "{err}");
        let (status, _, err) = run(&["scenario", "dual-wake", "--bogus"]);
        assert_eq!(status, 2);
        assert!(err.contains("unknown option --bogus"), "{err}");
    }

    #[test]
    fn run_with_a_data_dir_keeps_the_profile_between_runs() {
        if common::default_firmware_dir().is_none() {
            eprintln!("skipping: firmware not available");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ngc-cli-data-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir_arg = dir.to_str().unwrap().to_string();
        let (status, out, err) = run(&["run", "--seconds", "0.5", "--no-warnings", "--data-dir", &dir_arg]);
        assert_eq!(status, 0, "{err}\n{out}");
        assert!(out.contains("wrote eeprom.bin, nor.ngc, rtc-state.json, inputs.json"), "{out}");
        assert_eq!(std::fs::read(dir.join("eeprom.bin")).unwrap().len(), 2048);
        let rtc = std::fs::read_to_string(dir.join("rtc-state.json")).unwrap();
        assert!(rtc.contains("\"clockPolicy\": \"virtual-time-only\"") && rtc.contains("\"ngc-main\"") && rtc.ends_with("}\n"), "{rtc}");
        assert!(!dir.join("rtc-state.json.tmp").exists(), "the checkpoint is replaced atomically");
        // The next run loads what the first one left and rewrites only what changed.
        let (status, out, err) = run(&["run", "--seconds", "0.5", "--no-warnings", "--data-dir", &dir_arg]);
        assert_eq!(status, 0, "{err}\n{out}");
        assert!(out.contains("profile ") && !out.contains("wrote eeprom.bin"), "{out}");
        // A damaged checkpoint stops the start and is never replaced.
        std::fs::write(dir.join("rtc-state.json"), "{broken").unwrap();
        let (status, _, err) = run(&["run", "--seconds", "0.1", "--data-dir", &dir_arg]);
        assert_eq!(status, 1, "{err}");
        assert!(err.contains("rtc-state.json"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("rtc-state.json")).unwrap(), "{broken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_boots_the_handset_and_counts_instructions() {
        if common::default_firmware_dir().is_none() {
            eprintln!("skipping: firmware not available");
            return;
        }
        let json_path = std::env::temp_dir().join(format!("ngc-cli-run-test-{}.json", std::process::id()));
        let json_arg = json_path.to_str().unwrap().to_string();
        let (status, out, err) = run(&["run", "--mode", "handset", "--seconds", "0.01", "--json", &json_arg]);
        assert_eq!(status, 0, "{err}\n{out}");
        assert!(out.contains("virtual time 0.0100 s"), "{out}");
        let json = emu_core::Json::parse(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&json_path);
        assert_eq!(json.get("boards").and_then(|b| b.get("handset")).and_then(|h| h.get("instructions")).and_then(emu_core::Json::as_u64), Some(1_000_000));
        assert_eq!(json.get("mode").and_then(emu_core::Json::as_str), Some("handset"));
        assert!(json.get("fingerprint").and_then(emu_core::Json::as_str).is_some_and(|f| f.len() == 64));
    }
}
