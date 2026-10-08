//! `ngc-cli scenario`: the scenario validation suite of the FEATURES work package (`ngc::scenario`).

use crate::args;
use crate::common;
use emu_core::Json;
use ngc::firmware::Role;
use ngc::scenario::{self, ScenarioEnv, ScenarioReport, SCENARIOS};
use ngc::system::BuildOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "ngc-cli scenario list\n\
    ngc-cli scenario <name>|all [--main <srec>] [--handset <srec>] [--release ID] [--out DIR] [--no-i2c-idle-high]\n  \
    Runs a scenario of the validation suite on the original TRITON firmware (default: the repository SREC files) and\n  \
    writes <out>/<name>.json plus its evidence images (<name>-<label>.png) and traces (<name>-<file>). The JSON lists the\n  \
    engine's own checks and the comparisons with the values the Renode runner recorded (embedded in the scenarios); exit\n  \
    status 1 when a check of any scenario failed, 2 on a usage error. `scenario list` prints the names. The scenarios\n  \
    are written for TRITON-5.8-65.3 (addresses, wizard offsets and Renode recordings); other releases are refused.\n  \
    --out defaults to target/scenarios (ignored by git).\n  \
    The main board's I2C idle-high fixture is on by default. The Renode recordings predate it: with the fixture on their\n  \
    comparisons are informational, `--no-i2c-idle-high` reproduces the recorded start-up and makes them binding again.";

pub fn run(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, out) {
        Ok(code) => code,
        Err((true, message)) => {
            let _ = writeln!(err, "ngc-cli scenario: {message}\n{USAGE}");
            2
        }
        Err((false, message)) => {
            let _ = writeln!(err, "ngc-cli scenario: {message}");
            1
        }
    }
}

fn run_inner(argv: &[String], out: &mut dyn Write) -> Result<i32, (bool, String)> {
    let usage = |message: String| (true, message);
    let failed = |message: String| (false, message);
    let parsed = args::parse(argv, &["main", "handset", "out", "release"], &["no-i2c-idle-high"]).map_err(usage)?;
    let release = common::parse_release(parsed.value("release")).map_err(usage)?;
    let Some(target) = parsed.positional.first().cloned() else {
        return Err(usage("missing scenario name (or `list`)".to_string()));
    };
    if parsed.positional.len() > 1 {
        return Err(usage(format!("unexpected argument '{}'", parsed.positional[1])));
    }
    if target == "list" {
        for info in SCENARIOS {
            let _ = writeln!(out, "{:<16} {}", info.name, info.summary);
        }
        return Ok(0);
    }
    let names: Vec<&str> = if target == "all" {
        SCENARIOS.iter().map(|s| s.name).collect()
    } else if SCENARIOS.iter().any(|s| s.name == target) {
        vec![target.as_str()]
    } else {
        return Err(usage(format!("unknown scenario '{target}' (try `scenario list`)")));
    };
    let main = common::load_firmware(&common::firmware_path_in(parsed.value("main"), Role::Main, release).map_err(failed)?, Role::Main).map_err(failed)?;
    let handset = common::load_firmware(&common::firmware_path_in(parsed.value("handset"), Role::Handset, release).map_err(failed)?, Role::Handset).map_err(failed)?;
    ngc::firmware::common_release(&main, &handset).map_err(failed)?;
    let out_dir = parsed.value("out").map(PathBuf::from).unwrap_or_else(common::default_scenario_dir);
    let env = ScenarioEnv { main: &main, handset: &handset, options: BuildOptions { main_i2c_idle_high: !parsed.flag("no-i2c-idle-high") } };
    let mut any_failed = false;
    for name in names {
        let started = std::time::Instant::now();
        match scenario::run(name, &env) {
            Ok(report) => {
                let wall = started.elapsed().as_secs_f64();
                write_report(&out_dir, &report).map_err(failed)?;
                any_failed |= !report.passed;
                summarize(out, &report, wall, &out_dir);
            }
            Err(error) => {
                any_failed = true;
                let _ = writeln!(out, "{name}: ERROR {error}");
            }
        }
    }
    Ok(i32::from(any_failed))
}

fn write_report(dir: &Path, report: &ScenarioReport) -> Result<(), String> {
    common::write_json(&dir.join(format!("{}.json", report.name)), &report.document)?;
    for (name, bytes) in report.images.iter().chain(report.files.iter()) {
        let path = dir.join(format!("{}-{name}", report.name));
        std::fs::write(&path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn summarize(out: &mut dyn Write, report: &ScenarioReport, wall: f64, dir: &Path) {
    let doc = &report.document;
    let checks = doc.get("checks").and_then(Json::as_array).unwrap_or(&[]);
    let comparisons = doc.get("comparisons").and_then(Json::as_array).unwrap_or(&[]);
    let agree = comparisons.iter().filter(|c| c.get("agrees") == Some(&Json::Bool(true))).count();
    let _ = writeln!(
        out,
        "{}: {} ({} checks, {} comparisons with Renode: {} equal, {} different; {:.1} s) -> {}",
        report.name,
        if report.passed { "PASSED" } else { "FAILED" },
        checks.len(),
        comparisons.len(),
        agree,
        comparisons.len() - agree,
        wall,
        dir.join(format!("{}.json", report.name)).display()
    );
    for check in checks.iter().filter(|c| c.get("passed") == Some(&Json::Bool(false))) {
        let _ = writeln!(out, "    FAILED check: {} observed {}", check.get("name").and_then(Json::as_str).unwrap_or("?"), check.get("observed").map_or(String::new(), |v| v.to_string()));
    }
    for comparison in comparisons.iter().filter(|c| c.get("agrees") == Some(&Json::Bool(false))) {
        let _ = writeln!(
            out,
            "    differs{}: {} engine {} / renode {}{}",
            if comparison.get("must") == Some(&Json::Bool(true)) { " (MUST AGREE)" } else { "" },
            comparison.get("name").and_then(Json::as_str).unwrap_or("?"),
            comparison.get("engine").map_or(String::new(), |v| v.to_string()),
            comparison.get("renode").map_or(String::new(), |v| v.to_string()),
            comparison.get("note").and_then(Json::as_str).filter(|n| !n.is_empty()).map_or(String::new(), |n| format!(" ({n})"))
        );
    }
}
