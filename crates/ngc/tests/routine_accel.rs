//! Exact routine acceleration at system level (DESIGN.md 16.1/16.2), gated on the local firmware SREC files.
//!
//! **Quick loop** (default run):
//!
//! * a short real-firmware run, a boot and the first seconds of a 20 m descent with the new default EEPROM, produces identical
//!   checkpoint digests (fingerprint, FPSCR/VFP registers, predecode cache and cut-block history, retire counts, LCD) with the
//!   acceleration on, off and in shadow-verification mode, the shadow mode finds no mismatch, and the acceleration really replaced
//!   calls;
//! * the cheapest scenarios produce byte-identical documents and evidence with the acceleration on and off;
//! * the switch is plumbed through `SessionConfig` and shows in the state document.
//!
//! **Slow tier** (`./cargo test --workspace --release -- --ignored`; the same checks as `ngc-cli bench --dive
//! --verify-routine-accel` and `ngc-cli scenario all`):
//!
//! * the committed dive benchmark (valid-tissue profile, 20 m dive) is identical with the acceleration on, off and in shadow mode;
//! * every scenario of the suite is byte-identical with the acceleration on and off.

use ngc::firmware::{self, Firmware, Role};
use ngc::scenario::dive::{self, DiveConfig, DEPTH_20M};
use ngc::scenario::{self, ScenarioEnv, SCENARIOS};
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::{RoutineAccelMode, Which};
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

/// Scenario documents and evidence with the acceleration on and off must be byte-identical.
fn assert_scenarios_identical(main: &Firmware, handset: &Firmware, names: &[&str]) {
    for name in names {
        let mut with = ScenarioEnv::new(main, handset);
        with.routine_accel = true;
        let mut without = ScenarioEnv::new(main, handset);
        without.routine_accel = false;
        let a = scenario::run(name, &with).unwrap_or_else(|e| panic!("{name}: {e}"));
        let b = scenario::run(name, &without).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(a.document.to_pretty_string(), b.document.to_pretty_string(), "{name}: the result documents differ");
        assert_eq!(a.images, b.images, "{name}: the evidence images differ");
        assert_eq!(a.files, b.files, "{name}: the evidence files differ");
    }
}

// ---- quick loop ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_short_run_is_identical_with_routine_acceleration_on_off_and_shadow() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    // A dual boot with the engine's default configuration (the new EEPROM of a session), then the first seconds of a 20 m descent:
    // the main board's decompression code runs the accelerated soft-double routines.
    let run = |mode: RoutineAccelMode| {
        let config = SessionConfig { routine_accel: mode != RoutineAccelMode::Off, routine_accel_shadow: mode == RoutineAccelMode::Shadow, ..SessionConfig::default() };
        let mut session = Session::new(config, Some(&main), &handset, Profile::default()).expect("session");
        session.action("{\"action\":\"advance\",\"seconds\":4}").expect("boot");
        session.action("{\"action\":\"inputs\",\"inputs\":{\"pressure1Mbar\":3013.3,\"pressure2Mbar\":3013.3}}").expect("descent");
        session.action("{\"action\":\"advance\",\"seconds\":10}").expect("descent");
        let stats = session.system().routine_accel_stats(Which::Main).expect("main board");
        (dive::checkpoint(&mut session, "short run"), stats)
    };
    let (on, on_stats) = run(RoutineAccelMode::On);
    let (off, off_stats) = run(RoutineAccelMode::Off);
    let (shadow, shadow_stats) = run(RoutineAccelMode::Shadow);
    eprintln!("short run: {} calls replaced, {} shadow checks", on_stats.hits(), shadow_stats.shadow_checks);
    assert!(on_stats.hits() > 10_000, "the run replaced only {} calls", on_stats.hits());
    assert_eq!(off_stats.hits() + off_stats.shadow_checks, 0, "off replaces nothing");
    assert_eq!(on, off, "on vs off");
    assert_eq!(on, shadow, "on vs shadow");
    assert_eq!((shadow_stats.shadow_mismatches, shadow_stats.first_mismatch), (0, None));
    assert!(shadow_stats.shadow_checks > 10_000, "the shadow run checked only {} calls", shadow_stats.shadow_checks);
}

#[test]
fn the_cheapest_scenarios_are_byte_identical_with_routine_acceleration_on_and_off() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    assert_scenarios_identical(&main, &handset, &["dual-wake", "outputs-uart"]);
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

// ---- slow tier ---------------------------------------------------------------------------------------------------------------

#[test]
#[ignore = "slow: the committed dive benchmark three times (about 45 s); run with --ignored or ngc-cli bench --dive --verify-routine-accel"]
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
#[ignore = "slow: every scenario twice (about 20 s); run with --ignored or ngc-cli scenario all"]
fn every_scenario_is_byte_identical_with_routine_acceleration_on_and_off() {
    let Some((main, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let names: Vec<&str> = SCENARIOS.iter().map(|s| s.name).filter(|name| *name != "fast-forward").collect();
    assert_scenarios_identical(&main, &handset, &names);
}
