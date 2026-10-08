//! Exact routine acceleration at system level (DESIGN.md 16.1/16.2), gated on the local firmware SREC files.
//!
//! * the committed dive benchmark produces identical checkpoint digests (fingerprint, FPSCR/VFP registers, predecode cache
//!   and cut-block history, retire counts, LCD) with the acceleration on, off and in shadow-verification mode, and the shadow
//!   mode finds no mismatch;
//! * every scenario of the suite produces byte-identical documents and evidence with the acceleration on and off;
//! * the switch is plumbed through `SessionConfig` and shows in the state document.

use ngc::firmware::{self, Firmware, Role};
use ngc::scenario::dive::{self, DiveConfig, DEPTH_20M};
use ngc::scenario::{self, ScenarioEnv, SCENARIOS};
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::RoutineAccelMode;
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

fn run_dive(env: &ScenarioEnv<'_>, mode: RoutineAccelMode) -> dive::DiveReport {
    let config = DiveConfig { routine_accel: mode, depths: vec![DEPTH_20M], dive_seconds: 20.0, ..DiveConfig::default() };
    let started = std::time::Instant::now();
    let mut clock = move || started.elapsed().as_secs_f64();
    dive::run(env, &config, &mut clock).unwrap_or_else(|e| panic!("dive benchmark ({mode:?}): {e}"))
}

#[test]
fn the_dive_benchmark_is_identical_with_routine_acceleration_on_off_and_shadow() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let env = ScenarioEnv::new(&main, &handset);
    let on = run_dive(&env, RoutineAccelMode::On);
    let off = run_dive(&env, RoutineAccelMode::Off);
    let shadow = run_dive(&env, RoutineAccelMode::Shadow);
    assert!(on.dives[0].tissues_finite, "the benchmark's premise: finite tissues at the start of the dive");
    assert!(on.dives[0].replaced_calls > 1_000_000, "the dive replaced only {} calls", on.dives[0].replaced_calls);
    assert_eq!(off.dives[0].replaced_calls, 0);
    assert!(on.differences(&off).is_empty(), "on vs off: {:?}", on.differences(&off));
    assert!(on.differences(&shadow).is_empty(), "on vs shadow: {:?}", on.differences(&shadow));
    assert_eq!(shadow.dives[0].shadow_mismatches, 0);
    assert!(shadow.dives[0].replaced_calls > 1_000_000, "the shadow run checked only {} calls", shadow.dives[0].replaced_calls);
    // 4 profile stages + the dive's checkpoints.
    assert!(on.checkpoints().len() >= 9, "{}", on.checkpoints().len());
}

#[test]
fn every_scenario_is_byte_identical_with_routine_acceleration_on_and_off() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    for info in SCENARIOS.iter().filter(|s| s.name != "fast-forward") {
        let mut with = ScenarioEnv::new(&main, &handset);
        with.routine_accel = true;
        let mut without = ScenarioEnv::new(&main, &handset);
        without.routine_accel = false;
        let a = scenario::run(info.name, &with).unwrap_or_else(|e| panic!("{}: {e}", info.name));
        let b = scenario::run(info.name, &without).unwrap_or_else(|e| panic!("{}: {e}", info.name));
        assert_eq!(a.document.to_pretty_string(), b.document.to_pretty_string(), "{}: the result documents differ", info.name);
        assert_eq!(a.images, b.images, "{}: the evidence images differ", info.name);
        assert_eq!(a.files, b.files, "{}: the evidence files differ", info.name);
    }
}

#[test]
fn the_switch_is_part_of_the_session_and_the_state() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let mode_of = |config: SessionConfig| {
        let session = Session::new(config, Some(&main), &handset, Profile::default()).expect("session");
        session.state().get("routineAccel").and_then(|r| r.get("mode")).and_then(|m| m.as_str().map(str::to_string))
    };
    assert_eq!(mode_of(SessionConfig::default()).as_deref(), Some("on"), "on by default");
    assert_eq!(mode_of(SessionConfig { routine_accel: false, ..SessionConfig::default() }).as_deref(), Some("off"));
    assert_eq!(mode_of(SessionConfig { routine_accel_shadow: true, ..SessionConfig::default() }).as_deref(), Some("shadow"));
    // A running session switches and reports.
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    session.run_for(3.0);
    let hits = |s: &Session| s.state().get("routineAccel").and_then(|r| r.get("main")).and_then(|m| m.get("hits")).and_then(|h| h.as_u64()).unwrap_or(0);
    assert!(hits(&session) > 0, "a plain boot already replaces calls");
    session.set_routine_accel(false, false);
    assert_eq!(session.state().get("routineAccel").and_then(|r| r.get("mode")).and_then(|m| m.as_str()), Some("off"));
}
