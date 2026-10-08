//! The scenario validation suite as integration tests, gated on the local firmware SREC files (they are not in the
//! repository: without them every test skips with a message).
//!
//! Each test runs one scenario of `ngc::scenario` (the same code as `ngc-cli scenario <name>`): every check of the engine
//! has to hold, and every comparison with the Renode runner's recorded values that is marked `must` has to agree; the
//! other comparisons document known, explained differences and are reported in the failure text only. Set
//! `NGC_SCENARIO_OUT=<dir>` to also write the JSON documents and PNG evidence of the run (as `ngc-cli scenario --out`).

use emu_core::Json;
use ngc::firmware::{self, Firmware, Role};
use ngc::scenario::{self, ScenarioReport, SCENARIOS};
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

fn images() -> Option<(Firmware, Firmware)> {
    let dir = firmware_dir()?;
    let main = firmware::load(&std::fs::read(dir.join("ngc_main_5.8_TRITON.srec")).ok()?, Some(Role::Main)).ok()?;
    let handset = firmware::load(&std::fs::read(dir.join("ngc_handset_65.3_TRITON.srec")).ok()?, Some(Role::Handset)).ok()?;
    Some((main, handset))
}

/// The failures of a report as text: failed checks and `must` comparisons that disagree.
fn failures(report: &ScenarioReport) -> Vec<String> {
    let mut problems = Vec::new();
    for check in report.document.get("checks").and_then(Json::as_array).unwrap_or(&[]) {
        if check.get("passed") == Some(&Json::Bool(false)) {
            problems.push(format!("check '{}' failed, observed {}", check.get("name").and_then(Json::as_str).unwrap_or("?"), check.get("observed").map_or(String::new(), |v| v.to_string())));
        }
    }
    for comparison in report.document.get("comparisons").and_then(Json::as_array).unwrap_or(&[]) {
        if comparison.get("must") == Some(&Json::Bool(true)) && comparison.get("agrees") == Some(&Json::Bool(false)) {
            problems.push(format!(
                "comparison '{}' must agree with Renode: engine {} / renode {}",
                comparison.get("name").and_then(Json::as_str).unwrap_or("?"),
                comparison.get("engine").map_or(String::new(), |v| v.to_string()),
                comparison.get("renode").map_or(String::new(), |v| v.to_string())
            ));
        }
    }
    problems
}

fn write_evidence(report: &ScenarioReport) {
    let Some(dir) = std::env::var_os("NGC_SCENARIO_OUT") else { return };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create the evidence directory");
    std::fs::write(dir.join(format!("{}.json", report.name)), format!("{}\n", report.document.to_pretty_string())).expect("write the scenario JSON");
    for (name, bytes) in report.images.iter().chain(report.files.iter()) {
        std::fs::write(dir.join(format!("{}-{name}", report.name)), bytes).expect("write the scenario evidence");
    }
}

fn run_scenario(name: &str) {
    let Some((main, handset)) = images() else {
        eprintln!("skipping scenario {name}: the firmware SREC files are not available");
        return;
    };
    // Twice: on the default platform (main I2C idle-high fixture on, the comparisons with the older Renode recordings are
    // informational) and on the platform the recordings were made with (fixture off: the `must` comparisons are binding).
    for (platform, env) in [("default platform", scenario::ScenarioEnv::new(&main, &handset)), ("recorded platform", scenario::ScenarioEnv::recorded(&main, &handset))] {
        let report = scenario::run(name, &env).unwrap_or_else(|error| panic!("scenario {name} could not run: {error}"));
        if platform == "default platform" {
            write_evidence(&report);
        }
        let problems = failures(&report);
        assert!(report.passed && problems.is_empty(), "scenario {name} failed on the {platform}:\n  {}", problems.join("\n  "));
        // Every scenario produces its JSON document and, for the visual ones, a PNG that decodes.
        assert_eq!(report.document.get("scenario").and_then(Json::as_str), Some(name));
        assert!(report.document.get("checks").and_then(Json::as_array).is_some_and(|c| !c.is_empty()), "scenario {name} has no checks");
        for (image, bytes) in &report.images {
            assert!(ngc::png::decode_rgb(bytes).is_ok(), "{name}: {image} is not a valid PNG");
        }
        let fixture = report.document.get("platformOptions").and_then(|o| o.get("mainI2cIdleHigh"));
        assert_eq!(fixture, Some(&Json::Bool(platform == "default platform")), "{name}: {platform}");
    }
}

#[test]
fn dual_wake() {
    run_scenario("dual-wake");
}

#[test]
fn button_capture() {
    run_scenario("button-capture");
}

#[test]
fn battery_setup() {
    run_scenario("battery-setup");
}

#[test]
fn diluent_menu() {
    run_scenario("diluent-menu");
}

#[test]
fn clock_storage() {
    run_scenario("clock-storage");
}

#[test]
fn outputs_uart() {
    run_scenario("outputs-uart");
}

#[test]
fn can_loss() {
    run_scenario("can-loss");
}

#[test]
fn cold_wake() {
    run_scenario("cold-wake");
}

#[test]
fn machine_reset() {
    run_scenario("machine-reset");
}

#[test]
fn fast_forward() {
    run_scenario("fast-forward");
}

#[test]
fn every_scenario_of_the_suite_has_a_test_in_this_file() {
    let source = include_str!("scenarios.rs");
    for info in SCENARIOS {
        assert!(source.contains(&format!("run_scenario(\"{}\")", info.name)), "scenario {} has no integration test", info.name);
    }
    assert_eq!(SCENARIOS.len(), 10);
}

#[test]
fn an_unknown_scenario_is_refused_with_the_list_of_names() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: the firmware SREC files are not available");
        return;
    };
    let env = scenario::ScenarioEnv::new(&main, &handset);
    let error = scenario::run("nothing", &env).unwrap_err();
    assert!(error.contains("unknown scenario 'nothing'") && error.contains("dual-wake") && error.contains("fast-forward"), "{error}");
}

#[test]
fn the_scenarios_refuse_other_releases() {
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    let Some(dir) = roots
        .into_iter()
        .flatten()
        .map(|root| root.join("NEPTUN-5.8-65.3"))
        .find(|d| d.join("ngc_main_5.8_NEPTUN.srec").is_file() && d.join("ngc_handset_65.3_NEPTUN.srec").is_file())
    else {
        eprintln!("skipping: the NEPTUN firmware is not available");
        return;
    };
    let main = firmware::load(&std::fs::read(dir.join("ngc_main_5.8_NEPTUN.srec")).unwrap(), Some(Role::Main)).unwrap();
    let handset = firmware::load(&std::fs::read(dir.join("ngc_handset_65.3_NEPTUN.srec")).unwrap(), Some(Role::Handset)).unwrap();
    let env = scenario::ScenarioEnv::new(&main, &handset);
    let error = scenario::run("dual-wake", &env).unwrap_err();
    assert!(error.contains("TRITON-5.8-65.3") && error.contains("NEPTUN-5.8-65.3"), "{error}");
}
