//! The battery voltage of a fresh profile (4100 mV; every Renode recording used 1500 mV) with the real firmware images,
//! skipped when the gitignored SREC files are not available.
//!
//! What it establishes (a synthetic reproduction on the functional model, not a physical observation):
//!
//! * a fresh profile starts with both batteries at 4100 mV and a saved `inputs.json` keeps its own values;
//! * before a battery type is chosen the firmware shows the same B1 prompt at 4100 mV as at 1500 mV (identical LCD frame);
//!   only the voltage it prints on its diagnostic UART differs;
//! * after the types are chosen the voltage matters: Alkaline (1.5 V) at 4100 mV makes the next start show "Change battery"
//!   and the main board stand by, while Alkaline at 1500 mV and Li-Ion 3.7V-18650 at 4100 mV start normally.

use emu_core::Json;
use ngc::firmware::{self, Firmware, Role, TRITON};
use ngc::fixtures::Inputs;
use ngc::persistence::inputs_file_text;
use ngc::session::{Profile, Session, SessionConfig};
use std::path::PathBuf;

/// The B1 battery-selection prompt (SHA-256 of the LCD's PPM), recorded by the Renode runner with 1500 mV batteries.
const B1_PPM_SHA: &str = "62c3a30e54031ff3c2aeffa16a6b9f361c64ab2326db35da7e345c5318f7632d";

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

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn session(battery_mv: Option<f64>, main: &Firmware, handset: &Firmware) -> Session {
    // `None`: a fresh profile. `Some(mv)`: a profile whose `inputs.json` holds that battery voltage in both banks.
    let inputs = battery_mv.map(|mv| inputs_file_text(&Inputs { battery_mv: [mv; 2], ..Inputs::defaults() }));
    Session::new(SessionConfig::default(), Some(main), handset, Profile { inputs, ..Profile::default() }).expect("dual session")
}

fn battery_inputs(state: &Json) -> Vec<f64> {
    ["battery1Mv", "battery2Mv"].iter().map(|key| state.get("inputs").and_then(|i| i.get(key)).and_then(Json::as_f64).expect("battery input")).collect()
}

fn uart4_text(state: &Json) -> String {
    let channel = state.get("uartConsole").and_then(Json::as_array).and_then(|c| c.iter().find(|s| s.get("id").and_then(Json::as_str) == Some("main.uart4"))).expect("main.uart4");
    channel.get("text").and_then(Json::as_str).unwrap_or_default().to_string()
}

fn lcd_sha(session: &mut Session) -> String {
    let ppm = session.system_mut().lcd_ppm().expect("lcd");
    ngc::sha256::digest_hex(&ppm)
}

/// The battery wizard as the scenarios drive it: an action, then 0.65 virtual seconds. `downs` Downs select the row
/// (1 = Alkaline, 3 = Li-Ion 3.7V-18650), the first Confirm selects it and the second commits it; B1, then B2.
fn choose_battery_type(session: &mut Session, downs: usize) {
    for _bank in 0..2 {
        for action in std::iter::repeat("down").take(downs).chain(["confirm", "confirm"]) {
            act(session, &format!("{{\"action\":\"{action}\"}}"));
            act(session, "{\"action\":\"advance\",\"seconds\":0.65}");
        }
    }
}

#[test]
fn a_fresh_profile_starts_at_4100_mv_and_shows_the_same_b1_prompt_as_at_1500_mv() {
    let (main, handset) = images_or_skip!();
    let mut fresh = session(None, &main, &handset);
    let state = act(&mut fresh, "{\"action\":\"advance\",\"seconds\":10.5}");
    assert_eq!(battery_inputs(&state), [4100.0, 4100.0], "a fresh profile");
    assert_eq!((state.get("handsetReleaseTime").and_then(Json::as_f64), state.get("error")), (Some(1.05), Some(&Json::Null)));
    assert_eq!((state.get("mainBatteryReady"), state.get("standby"), state.get("frameReady")), (Some(&Json::Bool(true)), Some(&Json::Bool(false)), Some(&Json::Bool(true))));
    assert_eq!(lcd_sha(&mut fresh), B1_PPM_SHA, "the B1 prompt does not depend on the battery voltage");
    // The firmware reads the voltage through the 12-bit ADC and prints it on its diagnostic UART.
    let at_4100 = uart4_text(&state);
    assert!(at_4100.contains("Main voltage: 4099 mV") && at_4100.contains("Bkup voltage: 4099 mV"), "{at_4100:?}");

    // The recordings pin 1500 mV and reach the identical screen.
    let mut recorded = session(Some(1500.0), &main, &handset);
    let state = act(&mut recorded, "{\"action\":\"advance\",\"seconds\":10.5}");
    assert_eq!(battery_inputs(&state), [1500.0, 1500.0]);
    assert_eq!(lcd_sha(&mut recorded), B1_PPM_SHA);
    let at_1500 = uart4_text(&state);
    assert!(at_1500.contains("Main voltage: 1500 mV") && !at_1500.contains("4099"), "{at_1500:?}");

    // A saved profile keeps its own values: a stored 1450 mV bank 1 stays, the other bank (absent from the file) takes the new default.
    let partial = Profile { inputs: Some("{\"battery1Mv\": 1450}".to_string()), ..Profile::default() };
    let kept = Session::new(SessionConfig::default(), Some(&main), &handset, partial).expect("session");
    assert_eq!(battery_inputs(&Json::parse(&kept.state_json()).unwrap()), [1450.0, 4100.0]);
}

#[test]
fn the_battery_type_must_fit_the_voltage_alkaline_at_4100_mv_changes_battery_and_stands_by_after_a_restart() {
    let (main, handset) = images_or_skip!();
    // (label, battery mV, Downs to the row, expected standby after Restart)
    for (label, battery_mv, downs, standby) in [
        ("Alkaline at 4100 mV", 4100.0, 1usize, true),
        ("Alkaline at 1500 mV (the recordings)", 1500.0, 1, false),
        ("Li-Ion 3.7V-18650 at 4100 mV", 4100.0, 3, false),
    ] {
        let mut s = session(Some(battery_mv), &main, &handset);
        act(&mut s, "{\"action\":\"advance\",\"seconds\":6.5}");
        choose_battery_type(&mut s, downs);
        act(&mut s, "{\"action\":\"advance\",\"seconds\":2}");
        // Before the restart the unit keeps running whatever the combination.
        assert_eq!(act(&mut s, "{\"action\":\"advance\",\"seconds\":0.1}").get("standby"), Some(&Json::Bool(false)), "{label}: before the restart");
        act(&mut s, "{\"action\":\"reset\"}");
        let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":12}");
        assert_eq!(state.get("error"), Some(&Json::Null), "{label}");
        assert_eq!(state.get("standby"), Some(&Json::Bool(standby)), "{label}: standby after the restart");
        assert_eq!(state.get("handsetPowered"), Some(&Json::Bool(!standby)), "{label}: the handset supply after the restart");
    }
}
