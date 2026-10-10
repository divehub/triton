//! The decompression state handling with the real TRITON firmware images (skipped when the gitignored SREC files are not
//! available): the read-only `decoHealth` report and the start at the surface (`crates/ngc/src/deco.rs`, `surface_start.rs`;
//! DESIGN.md "Decompression state handling").
//!
//! What they establish (synthetic reproductions on the functional model, not physical observations):
//!
//! * on a first boot the original TRITON firmware resets its tissues and saves the decompression date but never saves the
//!   tissues, so with a blank EEPROM (the factory image switched off through the internal field) the next start loads 32 erased
//!   words as NaN and keeps them: the no-decompression limit stays at 99 minutes at depth, and `decoHealth` reports it. The new
//!   EEPROM of a real session does not have this problem (`eeprom_init.rs`); nothing repairs an older profile;
//! * the health report follows the oxygen calibration, and a cold boot makes the firmware clear it again;
//! * every board creation starts at the surface pressure, a new session also with the oxygen cells at their defaults.
//!
//! The oxygen calibration goes through the firmware's own protocol (the CAN commands the handset's calibration menu sends:
//! enter, air, 1000 mbar, test, commit after the success reply); no calibration flag or ppO2 value is set directly.

use emu_core::{Json, Width};
use ngc::deco::{EEPROM_BYTES, EEPROM_DATE_BYTES, EEPROM_TISSUE_BYTES};
use ngc::firmware::{self, Firmware, Role, TRITON};
use ngc::fixtures::Inputs;
use ngc::persistence::inputs_file_text;
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::Which;
use std::path::PathBuf;

/// Physical EEPROM offsets of the records (see `ReleaseAddresses`): tissue block and date record.
const TISSUES: usize = 0x0FF;
const DATE: usize = 0x17F;
/// Main RAM of the TRITON image (see `ReleaseAddresses`): the raw no-decompression limit.
const RAW_NDL: u32 = 0x2000_2108;
/// About 35 m of EN13319 water on 1013.25 mbar.
const DEEP_MBAR: f64 = 4600.0;

fn images() -> Option<(Firmware, Firmware)> {
    // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    let dir = roots
        .into_iter()
        .flatten()
        .map(|root| root.join(TRITON.id))
        .find(|d| d.join(TRITON.main.file_name).is_file() && d.join(TRITON.handset.file_name).is_file())?;
    let main = firmware::load(&std::fs::read(dir.join(TRITON.main.file_name)).ok()?, Some(Role::Main)).ok()?;
    let handset = firmware::load(&std::fs::read(dir.join(TRITON.handset.file_name)).ok()?, Some(Role::Handset)).ok()?;
    Some((main, handset))
}

macro_rules! images_or_skip {
    () => {
        match images() {
            Some(images) => images,
            None => {
                eprintln!("skipping: the {} SREC files are not available", TRITON.id);
                return;
            }
        }
    };
}

/// The default configuration with the EEPROM factory image switched off (the internal field): a blank EEPROM, as an older build's
/// first boot left it.
fn blank() -> SessionConfig {
    SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() }
}

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn advance(session: &mut Session, seconds: f64) -> Json {
    let mut remaining = seconds;
    let mut state = Json::Null;
    while remaining > 1e-9 {
        let chunk = remaining.min(20.0);
        state = act(session, &format!("{{\"action\":\"advance\",\"seconds\":{chunk}}}"));
        remaining -= chunk;
    }
    state
}

fn state(session: &Session) -> Json {
    Json::parse(&session.state_json()).expect("state json")
}

fn set_pressure(session: &mut Session, mbar: f64) -> Json {
    act(session, &format!("{{\"action\":\"inputs\",\"inputs\":{{\"pressure1Mbar\":{mbar},\"pressure2Mbar\":{mbar}}}}}"))
}

fn main_word(session: &Session, address: u32) -> u32 {
    session.system().board(Which::Main).expect("main board").peek(address, Width::Word).expect("main RAM")
}

fn raw_ndl(session: &Session) -> i32 {
    main_word(session, RAW_NDL) as i32
}

fn eeprom(session: &Session) -> [u8; EEPROM_BYTES] {
    session.system().main.as_ref().expect("main board").eeprom.image()
}

fn health(state: &Json, member: &str) -> String {
    state.get("decoHealth").and_then(|h| h.get(member)).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn input(state: &Json, key: &str) -> f64 {
    state.get("inputs").and_then(|i| i.get(key)).and_then(Json::as_f64).unwrap_or_else(|| panic!("input {key}"))
}

/// The air calibration the handset's menu drives, sent as the same CAN commands (standard identifiers, from the handset's
/// controller): `0x42` enter, `0x44` air (21 %), `0x49` 1000 mbar, `0x52` test, then `0x55` commit once the main board
/// answered `0x54` (success). The inputs are the defaults (three 10 mV cells, 1013.25 mbar), which the firmware's acceptance
/// window takes.
fn calibrate_oxygen(session: &mut Session) {
    for (id, payload) in [(0x42, &[][..]), (0x44, &[0x15][..]), (0x49, &[0xE8, 0x03][..]), (0x52, &[][..])] {
        session.inject_can_from_handset(id, payload).expect("inject");
        advance(session, 1.0);
    }
    for _ in 0..40 {
        if session.system().link.trace_text().contains("ngc-main.can1\t0x054\t") {
            break;
        }
        advance(session, 0.5);
    }
    assert!(session.system().link.trace_text().contains("ngc-main.can1\t0x054\t"), "the main board reported a successful test (0x54)");
    session.inject_can_from_handset(0x55, &[]).expect("commit");
    advance(session, 2.0);
}

#[test]
fn a_restart_after_a_blank_first_boot_loads_nan_tissues_and_the_health_report_says_so() {
    let (main, handset) = images_or_skip!();
    let mut session = Session::new(blank(), Some(&main), &handset, Profile::default()).expect("dual session");
    advance(&mut session, 8.0);
    calibrate_oxygen(&mut session);
    let image = eeprom(&session);
    assert_eq!(&image[0x38..0x3E], &[0xE8, 0x03, 0xE8, 0x03, 0xE8, 0x03], "the firmware stored 10.00 mV for three cells");
    assert_eq!(&image[0x3E..0x41], &[0x09, 0x09, 0x09], "enabled and freshly calibrated");
    let first = state(&session);
    assert_eq!((health(&first, "tissues"), health(&first, "oxygen")), ("valid".to_string(), "calibrated".to_string()), "the first boot reset its tissues itself");
    // The unit goes under water and is restarted: the saved profile holds a date and an erased tissue block.
    set_pressure(&mut session, DEEP_MBAR);
    advance(&mut session, 3.0);
    let stored = eeprom(&session);
    assert!(stored[TISSUES..TISSUES + EEPROM_TISSUE_BYTES].iter().all(|&b| b == 0xFF), "the start-up never saves the tissues");
    assert!(stored[DATE..DATE + EEPROM_DATE_BYTES].iter().any(|&b| b != 0xFF), "but it saves the decompression date");

    let restarted = act(&mut session, "{\"action\":\"reset\"}");
    assert!(restarted.get("decoStorageFixture").is_none(), "there is no repair fixture any more");
    advance(&mut session, 4.0);
    let after = state(&session);
    assert_eq!(health(&after, "oxygen"), "calibrated", "the calibration survives a Restart");
    // Back at the surface after the Restart (the start-at-the-surface fixture); now the dive.
    assert_eq!((input(&after, "pressure1Mbar"), input(&after, "pressure2Mbar")), (1013.25, 1013.25), "Restart starts at the surface");
    set_pressure(&mut session, DEEP_MBAR);
    advance(&mut session, 15.0);
    // The original firmware's own behavior, reproduced: 32 NaN words, kept (the elapsed time is under four days), so the limit
    // stays at 99 and the read-only report says that the tissues are invalid.
    let deep = state(&session);
    assert_eq!(health(&deep, "tissues"), "invalid", "{:?}", deep.get("decoHealth"));
    let details = deep.get("decoHealth").and_then(|h| h.get("details")).expect("details");
    assert_eq!(details.get("nonFiniteTissueWords").and_then(Json::as_u64), Some(32), "{details:?}");
    assert_eq!(raw_ndl(&session), 99, "the limit stays at 99 with NaN tissues");
}

#[test]
fn every_board_creation_starts_at_the_surface_and_a_new_session_resets_the_oxygen_cells() {
    let (main, handset) = images_or_skip!();
    // A profile left under water with unusual oxygen cells.
    let left_under_water = Inputs { pressure_mbar: [4600.5, 4601.5], oxygen_mv: [60.0, 61.0, 59.0], battery_mv: [1500.0, 1500.0], ..Inputs::defaults() };
    let profile = Profile { inputs: Some(inputs_file_text(&left_under_water)), ..Profile::default() };
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, profile.clone()).expect("session");
    let started = state(&session);
    assert_eq!((input(&started, "pressure1Mbar"), input(&started, "pressure2Mbar")), (1012.75, 1013.75), "surface plus each sensor's offset (-0.5 and +0.5 mbar from the mean)");
    assert_eq!((input(&started, "oxygen1Mv"), input(&started, "oxygen2Mv"), input(&started, "oxygen3Mv")), (10.0, 10.0, 10.0), "a new session resets the cells");
    assert_eq!(input(&started, "battery1Mv"), 1500.0, "nothing else is touched");
    let report = started.get("startAtSurface").expect("startAtSurface");
    assert_eq!(report.get("enabled"), Some(&Json::Bool(true)));
    assert_eq!(report.get("oxygenReset"), Some(&Json::Bool(true)));
    assert_eq!(report.get("changedInputs").and_then(Json::as_array).map(<[Json]>::len), Some(5));
    let saved = session.take_profile_changes().and_then(|p| p.inputs).expect("inputs.json is rewritten at the start");
    // The file keeps its format byte for byte: the same keys and layout as the runner's file, no new field.
    let expected = Inputs { pressure_mbar: [1012.75, 1013.75], oxygen_mv: [10.0; 3], ..left_under_water };
    assert_eq!(saved, inputs_file_text(&expected));

    advance(&mut session, 6.0);
    // The user sets other cells and goes down; Restart, Cold and Wake bring the depth back but keep the cells.
    act(&mut session, "{\"action\":\"inputs\",\"inputs\":{\"oxygen1Mv\":12.5,\"oxygen2Mv\":12.0,\"oxygen3Mv\":12.25,\"pressure1Mbar\":3013.5,\"pressure2Mbar\":3015.5}}");
    for action in ["reset", "cold", "wake", "serial"] {
        act(&mut session, "{\"action\":\"inputs\",\"inputs\":{\"pressure1Mbar\":3013.5,\"pressure2Mbar\":3015.5}}");
        let request = if action == "serial" { "{\"action\":\"serial\",\"serialNumber\":7}".to_string() } else { format!("{{\"action\":\"{action}\"}}") };
        let after = act(&mut session, &request);
        assert_eq!((input(&after, "pressure1Mbar"), input(&after, "pressure2Mbar")), (1012.25, 1014.25), "{action}: depth 0, the offsets of the sensors kept");
        assert_eq!((input(&after, "oxygen1Mv"), input(&after, "oxygen2Mv"), input(&after, "oxygen3Mv")), (12.5, 12.0, 12.25), "{action}: the cells are kept");
        assert_eq!(after.get("startAtSurface").and_then(|r| r.get("oxygenReset")), Some(&Json::Bool(false)), "{action}");
        advance(&mut session, 1.0);
    }

    // The surface pressure is a setting of the page; the actions that recreate the boards may carry it.
    let altitude = act(&mut session, "{\"action\":\"reset\",\"surfacePressureMbar\":900}");
    assert_eq!((input(&altitude, "pressure1Mbar"), input(&altitude, "pressure2Mbar")), (899.0, 901.0));
    assert_eq!(altitude.get("startAtSurface").and_then(|r| r.get("surfacePressureMbar")).and_then(Json::as_f64), Some(900.0));
    // A refused value shuts nothing down: the boards keep their virtual time (a Restart would start it at 0 again) and the history
    // epoch (every board creation advances it). Numbers only, as at session creation: no numeric string, no boolean.
    advance(&mut session, 0.5);
    let (before, epoch) = (session.virtual_ns(), session.output_history_epoch());
    assert!(before > 0);
    for bad in ["99", "30001", "\"x\"", "\"900\"", "true", "null"] {
        let error = session.action(&format!("{{\"action\":\"reset\",\"surfacePressureMbar\":{bad}}}")).unwrap_err();
        assert_eq!(error, "surfacePressureMbar must be between 100 and 30000", "{bad}");
    }
    assert_eq!((session.virtual_ns(), session.output_history_epoch()), (before, epoch), "a refused value shuts nothing down");
    assert_eq!(state(&session).get("startAtSurface").and_then(|r| r.get("surfacePressureMbar")).and_then(Json::as_f64), Some(900.0), "and keeps the setting");
    // The next plain Restart keeps the last valid surface pressure of the session.
    set_pressure(&mut session, 3000.0);
    let again = act(&mut session, "{\"action\":\"reset\"}");
    assert_eq!(input(&again, "pressure1Mbar"), 900.0);

    // A new session with the setting of the host and, switched off, the saved inputs as they are.
    let config = SessionConfig { surface_pressure_mbar: 950.0, ..SessionConfig::default() };
    let hosted = Session::new(config, Some(&main), &handset, profile.clone()).expect("session");
    assert_eq!(input(&state(&hosted), "pressure1Mbar"), 949.5);
    let off = SessionConfig { start_at_surface: false, ..SessionConfig::default() };
    let mut kept = Session::new(off, Some(&main), &handset, profile).expect("session");
    let kept_state = state(&kept);
    assert_eq!((input(&kept_state, "pressure1Mbar"), input(&kept_state, "oxygen1Mv")), (4600.5, 60.0), "switched off: the saved inputs are used as they are");
    assert_eq!(kept_state.get("startAtSurface").and_then(|r| r.get("enabled")), Some(&Json::Bool(false)));
    let restarted = act(&mut kept, "{\"action\":\"reset\"}");
    assert_eq!((input(&restarted, "pressure1Mbar"), input(&restarted, "oxygen3Mv")), (4600.5, 59.0), "and a Restart keeps them");
}

#[test]
fn the_health_report_follows_the_oxygen_calibration_and_a_cold_boot_clears_it() {
    let (main, handset) = images_or_skip!();
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    // Before the firmware starts, nothing is known (RAM is zero).
    let early = state(&session);
    assert_eq!((health(&early, "tissues"), health(&early, "oxygen")), ("unknown".to_string(), "unknown".to_string()));
    let booted = advance(&mut session, 8.0);
    assert_eq!(health(&booted, "tissues"), "valid");
    assert_eq!(health(&booted, "oxygen"), "uncalibrated", "{:?}: a fresh profile has no calibration (cached cell flags)", booted.get("decoHealth"));
    // Under water the firmware evaluates the cells and its ppO2 is NaN.
    set_pressure(&mut session, DEEP_MBAR);
    let deep = advance(&mut session, 8.0);
    let details = deep.get("decoHealth").and_then(|h| h.get("details")).expect("details");
    assert_eq!(health(&deep, "oxygen"), "uncalibrated");
    assert_eq!((details.get("breathingMode").and_then(Json::as_u64), details.get("ppO2"), details.get("ppO2Word").and_then(Json::as_str)), (Some(2), Some(&Json::Null), Some("0x7fc00000")));

    // A second fresh session: calibrate at the surface through the firmware's protocol.
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut session, 8.0);
    calibrate_oxygen(&mut session);
    let calibrated = state(&session);
    assert_eq!(health(&calibrated, "oxygen"), "calibrated", "{:?}", calibrated.get("decoHealth"));
    let ppo2 = calibrated.get("decoHealth").and_then(|h| h.get("details")).and_then(|d| d.get("ppO2")).and_then(Json::as_f64).expect("ppO2");
    assert!((ppo2 - 0.21).abs() < 1e-6, "air at the surface: {ppo2}");

    // A Restart and a Wake keep the calibration; a cold boot makes the firmware clear the calibration flags again.
    act(&mut session, "{\"action\":\"wake\"}");
    advance(&mut session, 4.0);
    assert_eq!(health(&state(&session), "oxygen"), "calibrated", "a Wake keeps the calibration");
    act(&mut session, "{\"action\":\"cold\"}");
    advance(&mut session, 3.0);
    let cold = state(&session);
    assert_eq!(&eeprom(&session)[0x3E..0x41], &[0x01, 0x01, 0x01], "the firmware rewrote the cell flags (0x09 to 0x01) on the cold-boot wake cause");
    assert_eq!(health(&cold, "oxygen"), "uncalibrated", "{:?}", cold.get("decoHealth"));
}
