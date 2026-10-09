//! Host-facing `Session` checks with the real firmware images (skipped when the gitignored SREC files are not available):
//! action validation with the runner's messages, step/advance/host pacing, sensor input rounding and profile dirty
//! tracking, the profile lifecycle across shutdown and reopen, captures, the CAN partial application, the serial fixture,
//! the standby rules and the machine reset.

use emu_core::{Json, Width};
use ngc::firmware::{self, Firmware, Role};
use ngc::png;
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::{BootMode, Mode, Which};
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

macro_rules! firmware_or_skip {
    () => {
        match images() {
            Some(images) => images,
            None => {
                eprintln!("skipping: the firmware SREC files are not available");
                return;
            }
        }
    };
}

fn dual(config: SessionConfig, profile: Profile, main: &Firmware, handset: &Firmware) -> Session {
    Session::new(config, Some(main), handset, profile).expect("dual session")
}

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn time(state: &Json) -> f64 {
    state.get("virtualTime").and_then(Json::as_f64).expect("virtualTime")
}

#[test]
fn requests_are_validated_with_the_runner_messages() {
    let (_, handset) = firmware_or_skip!();
    let config = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    let mut session = Session::new(config, None, &handset, Profile::default()).expect("handset session");
    let advance_message = "Advance must be between zero and 20 virtual seconds";
    for (body, message) in [
        ("{\"action\":\"fly\"}", "Unknown action"),
        ("{\"action\":5}", "Unknown action"),
        ("", "Invalid request length"),
        ("{}", "'action'"),
        ("{\"action\":\"advance\",\"seconds\":0}", advance_message),
        ("{\"action\":\"advance\",\"seconds\":-1}", advance_message),
        ("{\"action\":\"advance\",\"seconds\":20.5}", advance_message),
        ("{\"action\":\"can\",\"connected\":false}", "CAN controls require --dual"),
        ("{\"action\":\"inputs\",\"inputs\":5}", "inputs must be an object"),
        ("{\"action\":\"inputs\",\"inputs\":{\"pressure1Mbar\":900}}", "Sensor controls require --dual"),
        ("{\"action\":\"serial\",\"serialNumber\":5}", "Serial fixture requires --dual"),
        ("{\"action\":\"led-colors\",\"colors\":{\"main-hud-1\":\"blue\"}}", "LED colors must map HUD channel IDs to unknown, red or white"),
        ("{\"action\":\"led-colors\",\"colors\":{\"handset-backlight\":\"red\"}}", "LED colors must map HUD channel IDs to unknown, red or white"),
    ] {
        assert_eq!(session.action(body).unwrap_err(), message, "{body}");
    }
    let oversized = format!("{{\"action\":\"{}\"}}", "x".repeat(1100));
    assert_eq!(session.action(&oversized).unwrap_err(), "Invalid request length");
    // Nothing above changed the session.
    assert_eq!(session.virtual_ns(), 0);
    assert!(session.error().is_none());
}

#[test]
fn step_advance_and_host_pacing_follow_the_runner() {
    let (_, handset) = firmware_or_skip!();
    let config = SessionConfig { mode: Mode::HandsetOnly, start_paused: true, ..SessionConfig::default() };
    let mut session = Session::new(config, None, &handset, Profile::default()).expect("handset session");
    assert!(!session.running());
    let outcome = session.run_for(0.01);
    assert_eq!((outcome.advanced_ns, outcome.virtual_ns, outcome.running), (0, 0, false), "a paused session ignores the host pacing call");

    // Step: two 50 ms intervals, and the session stays paused.
    let state = act(&mut session, "{\"action\":\"step\"}");
    assert_eq!(time(&state), 0.1);
    assert_eq!(state.get("running"), Some(&Json::Bool(false)));
    // Advance: exactly the requested span (string numbers are accepted like Python's float()).
    let state = act(&mut session, "{\"action\":\"advance\",\"seconds\":0.25}");
    assert_eq!(time(&state), 0.35);
    let state = act(&mut session, "{\"action\":\"advance\",\"seconds\":\"0.05\"}");
    assert_eq!(time(&state), 0.4);
    assert_eq!(state.get("running"), Some(&Json::Bool(false)));

    // Resume: the host paces the run; spans round up to whole 100 us quanta.
    let state = act(&mut session, "{\"action\":\"resume\"}");
    assert_eq!(state.get("running"), Some(&Json::Bool(true)));
    assert_eq!(session.run_for(0.01).advanced_ns, 10_000_000);
    assert_eq!(session.run_for(0.00015).advanced_ns, 200_000);
    assert_eq!(session.virtual_ns(), 410_200_000);
    act(&mut session, "{\"action\":\"pause\"}");
    assert_eq!(session.run_for(0.01).advanced_ns, 0);
    // The handset alone has no CAN, storage or sensor surface in its state document.
    let state = session.state();
    for key in ["canSummary", "mainPC", "inputs", "serialNumber", "storageSummary"] {
        assert!(state.get(key).is_none(), "{key}");
    }
    assert_eq!(state.get("mode").and_then(Json::as_str), Some("Handset only; example board-ID ADC; no CAN peer"));
}

#[test]
fn the_state_document_has_every_runner_field_and_the_engine_fields() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    let state = act(&mut session, "{\"action\":\"advance\",\"seconds\":0.2}");
    for key in [
        "running", "virtualTime", "pc", "lcdSummary", "frameReady", "error", "firmware", "performanceMode", "hostPacing", "hardwareOutputs", "uartConsole", "mode",
        "rtcPersistence", "bootMode", "standby", "standbyTime", "handsetReleaseTime", "powerModel", "outputHistoryEpoch", "buttonSummary", "mainPC", "canSummary",
        "mainBatteryReady", "inputs", "adcSummary", "storageSummary", "flashSummary", "serialNumber", "handsetPowered", "engine", "virtualNs", "instructions", "idleSkip",
        "idleFastForward", "machineResets", "realtimeFactor",
    ] {
        assert!(state.get(key).is_some(), "state field {key} is missing");
    }
    assert!(state.get("engine").and_then(Json::as_str).is_some_and(|e| e.starts_with("ngc-wasm/")));
    assert_eq!(state.get("virtualNs").and_then(Json::as_u64), Some(200_000_000));
    assert_eq!(state.get("instructions").and_then(|i| i.get("main")).and_then(Json::as_u64), Some(20_000_000));
    assert_eq!(state.get("handsetPowered"), Some(&Json::Bool(false)));
    assert_eq!(state.get("realtimeFactor"), Some(&Json::Null));
    assert_eq!(state.get("outputHistoryEpoch").and_then(Json::as_str), Some("0-1"), "<historyNonce>-<generation>");
    assert_eq!(state.get("hardwareOutputs").map(Json::len), Some(5));
    assert_eq!(state.get("uartConsole").map(Json::len), Some(5));
    assert_eq!(state.get("firmware").and_then(|f| f.get("main")).and_then(|m| m.get("sha256")).and_then(Json::as_str).map(str::len), Some(64));
    // DESIGN 15.3e: the release, the per-role facts and the release's address table; 15.3c: the I2C fixture.
    let firmware = state.get("firmware").expect("firmware");
    assert_eq!(firmware.get("release").and_then(|r| r.get("id")).and_then(Json::as_str), Some("TRITON-5.8-65.3"));
    assert_eq!(firmware.get("release").and_then(|r| r.get("label")).and_then(Json::as_str), Some("TRITON main 5.8 / handset 65.3"));
    for role in ["main", "handset"] {
        let facts = firmware.get(role).unwrap_or_else(|| panic!("firmware.{role}"));
        for key in ["srecSha256", "binSha256", "stack", "resetPC"] {
            assert!(facts.get(key).is_some(), "firmware.{role}.{key}");
        }
        assert_eq!(facts.get("binSha256"), facts.get("sha256"));
    }
    assert_eq!(firmware.get("main").and_then(|m| m.get("resetPC")).and_then(Json::as_u64), Some(0x0802_13B8));
    assert_eq!(firmware.get("handset").and_then(|m| m.get("resetPC")).and_then(Json::as_u64), Some(0x0800_8410));
    assert_eq!(firmware.get("addresses").and_then(|a| a.get("handsetOrientation")).and_then(|a| a.get("address")).and_then(Json::as_u64), Some(0x2000_0740));
    assert_eq!(state.get("unavailable").map(Json::len), Some(0), "every TRITON address is known");
    assert_eq!(state.get("i2cIdleHigh"), Some(&Json::Bool(true)), "the I2C idle-high fixture is on by default");
    assert!(state.get("i2cFixture").and_then(Json::as_str).is_some_and(|text| text.contains("PB6/PB7/PB10/PB11") && text.contains("driven high")));
    // Host information is shown when the host supplies it.
    session.set_host_info(ngc::session::HostInfo { pacing: Some("test pacing".to_string()), realtime_factor: Some(12.5) });
    let state = session.state();
    assert_eq!(state.get("hostPacing").and_then(Json::as_str), Some("test pacing"));
    assert_eq!(state.get("realtimeFactor").and_then(Json::as_f64), Some(12.5));
}

#[test]
fn sensor_inputs_round_through_f32_and_are_reported_once_as_profile_changes() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    // The launch of a fresh profile creates the backing files (erased EEPROM, empty NOR header) and marks the sensor controls
    // as changed (the runner writes inputs.json on startup); a second call has nothing.
    let first = session.take_profile_changes().expect("the files created by the launch");
    assert!(first.inputs.is_some() && first.rtc_state.is_none() && first.led_colors.is_none());
    assert_eq!(first.eeprom, Some(vec![0xFF; 2048]));
    assert_eq!(first.nor.as_ref().map(Vec::len), Some(16), "an empty NOR image is the 16-byte header");
    assert!(session.take_profile_changes().is_none());

    let state = act(&mut session, "{\"action\":\"inputs\",\"inputs\":{\"pressure1Mbar\":1013.3,\"oxygen1Mv\":11.7}}");
    // The state and inputs.json keep the runner's Python doubles as given...
    let shown = state.get("inputs").and_then(|i| i.get("pressure1Mbar")).and_then(Json::as_f64).unwrap();
    assert_eq!(shown, 1013.3);
    let changes = session.take_profile_changes().expect("the sensor change");
    let text = changes.inputs.expect("inputs.json");
    assert!(text.contains("\"pressure1Mbar\": 1013.3,") && text.contains("\"oxygen1Mv\": 11.7,"), "{text}");
    assert!(text.ends_with("}\n"));
    // ...while the models receive the value Renode's monitor produced: the double parsed through f32.
    {
        let main = session.system_mut().main.as_mut().expect("main board");
        let i2c = main.ids.i2c1;
        let sensor = main.board.get_mut::<stm32::i2c::Stm32F7I2c>(i2c).unwrap().target_mut::<ngc::models::ms5837::NgcMs5837>(ngc::main_board::PRESSURE_SENSOR_ADDRESS).expect("pressure sensor 1");
        assert_eq!(sensor.pressure_mbar(), f64::from(1013.3f32));
        assert_ne!(sensor.pressure_mbar(), 1013.3);
    }
    assert!(session.take_profile_changes().is_none());
    // A failed update changes nothing and reports the runner's message.
    let error = session.action("{\"action\":\"inputs\",\"inputs\":{\"pressure1Mbar\":\"high\"}}").unwrap_err();
    assert!(!error.is_empty());
    // LED colors are written once, and only after an assignment.
    assert!(session.take_profile_changes().is_none());
    act(&mut session, "{\"action\":\"led-colors\",\"colors\":{\"main-hud-1\":\"red\"}}");
    let changes = session.take_profile_changes().expect("led-colors.json");
    assert!(changes.led_colors.as_deref().is_some_and(|t| t.contains("\"main-hud-1\": \"red\"")));
    assert!(changes.inputs.is_none());
}

#[test]
fn the_profile_survives_shutdown_and_reopen() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    act(&mut session, "{\"action\":\"advance\",\"seconds\":6}");
    // The first boot initializes the EEPROM and the NOR flash; the RTC checkpoint only exists after a save point.
    let changes = session.take_profile_changes().expect("the first boot writes the EEPROM, the NOR flash and inputs.json");
    assert!(changes.eeprom.as_ref().is_some_and(|e| e.len() == 2048 && e[254] == 0xA3));
    assert!(changes.nor.is_some() && changes.inputs.is_some());
    assert!(changes.rtc_state.is_none(), "the RTC checkpoint is saved at restart and shutdown only");
    assert!(session.take_profile_changes().is_none());
    let exported = session.export_profile();
    let eeprom = exported.eeprom.clone().expect("eeprom.bin");
    assert_eq!(eeprom.len(), 2048);
    assert_eq!(eeprom[254], 0xA3, "the EEPROM validity marker written by the first boot");
    assert!(exported.nor.is_some());
    let rtc_text = exported.rtc_state.clone().expect("rtc-state.json");
    assert!(rtc_text.ends_with("}\n") && rtc_text.contains("\"ngc-main\"") && rtc_text.contains("\"ngc-handset\""), "{rtc_text}");
    let serial = session.state().get("serialNumber").and_then(Json::as_u64).unwrap();

    let profile = session.shutdown();
    assert_eq!(profile.eeprom.as_deref(), Some(&eeprom[..]));
    assert!(profile.inputs.is_some());
    assert!(profile.led_colors.is_none(), "no color was assigned");

    let reopened = dual(SessionConfig::default(), profile.clone(), &main, &handset);
    let state = reopened.state();
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(serial));
    let restored = state.get("rtcPersistence").and_then(|r| r.get("restoredBoards")).and_then(Json::as_array).unwrap();
    assert_eq!(restored.len(), 2, "both boards were restored from rtc-state.json");
    // A damaged rtc-state.json stops the start and is never replaced silently.
    let damaged = Profile { rtc_state: Some("{\"version\": 2}".to_string()), ..profile };
    let error = Session::new(SessionConfig::default(), Some(&main), &handset, damaged).err().expect("startup must fail");
    assert!(error.contains("rtc-state.json") || error.contains("RTC"), "{error}");
}

#[test]
fn captures_hold_the_state_the_lcd_png_and_the_can_trace() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    session.set_utc_micros(1_791_364_180_955_472);
    act(&mut session, "{\"action\":\"advance\",\"seconds\":6}");
    act(&mut session, "{\"action\":\"capture\"}");
    let captures = session.take_captures();
    assert_eq!(captures.len(), 1);
    assert!(session.take_captures().is_empty());
    let capture = &captures[0];
    assert_eq!(capture.name, "20261007T090940955472Z");
    assert!(capture.state_json.starts_with("{\n  \"running\": ") && capture.state_json.ends_with("}\n"), "{}", &capture.state_json[..40]);
    let captured = Json::parse(&capture.state_json).expect("state.json");
    assert_eq!(captured.get("virtualTime").and_then(Json::as_f64), Some(6.0));
    assert!(captured.get("lastCapture").is_none(), "the document is the state before the capture's own entry");
    assert!(capture.can_trace_tsv.lines().count() > 10 && capture.can_trace_tsv.starts_with("00:00:"), "CAN trace of the dual run");
    // The PNG decodes to the same pixels as the LCD's PPM.
    let (width, height, rgb) = png::decode_rgb(&capture.lcd_png).expect("lcd.png");
    let ppm = session.system_mut().lcd_ppm().expect("ppm");
    let header = format!("P6\n{width} {height}\n255\n");
    assert!(ppm.starts_with(header.as_bytes()), "{:?}", &ppm[..16]);
    assert_eq!((width, height), (320, 240));
    assert!(rgb == ppm[header.len()..], "the PNG pixels equal the PPM pixels");
    assert!(rgb.iter().any(|&b| b != 0), "the B1 prompt is on the screen");
    assert_eq!(session.state().get("lastCapture").and_then(Json::as_str), Some("captures/20261007T090940955472Z"));
    // A second capture in the same microsecond gets a suffix; without a host time the name carries the virtual time.
    act(&mut session, "{\"action\":\"capture\"}");
    let second = session.take_captures();
    assert_eq!(second[0].name, "20261007T090940955472Z-2");
}

#[test]
fn the_can_controls_apply_connected_before_validating_drop_id() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    act(&mut session, "{\"action\":\"advance\",\"seconds\":0.3}");
    for (body, message) in [
        ("{\"action\":\"can\",\"connected\":\"yes\"}", "connected must be a boolean"),
        ("{\"action\":\"can\",\"dropId\":2048}", "dropId must be -1 or a standard CAN ID (0..2047)"),
        ("{\"action\":\"can\",\"dropId\":-2}", "dropId must be -1 or a standard CAN ID (0..2047)"),
        ("{\"action\":\"can\",\"dropId\":true}", "dropId must be -1 or a standard CAN ID (0..2047)"),
        ("{\"action\":\"can\",\"dropId\":1.5}", "dropId must be -1 or a standard CAN ID (0..2047)"),
    ] {
        assert_eq!(session.action(body).unwrap_err(), message, "{body}");
    }
    let error = session.action("{\"action\":\"can\",\"connected\":false,\"dropId\":5000}").unwrap_err();
    assert_eq!(error, "dropId must be -1 or a standard CAN ID (0..2047)");
    let summary = session.state().get("canSummary").and_then(Json::as_str).unwrap().to_string();
    assert!(summary.contains("connected=False") && summary.contains("dropId=-1"), "connected was applied before the drop id was rejected: {summary}");
    let state = act(&mut session, "{\"action\":\"can\",\"connected\":true,\"dropId\":342}");
    let summary = state.get("canSummary").and_then(Json::as_str).unwrap();
    assert!(summary.contains("connected=True") && summary.contains("dropId=342"), "{summary}");
}

#[test]
fn the_serial_fixture_needs_an_initialized_eeprom_and_restarts_the_system() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    assert_eq!(
        session.action("{\"action\":\"serial\",\"serialNumber\":123456}").unwrap_err(),
        "Wait for the first boot to initialize EEPROM before changing the emulated serial"
    );
    act(&mut session, "{\"action\":\"advance\",\"seconds\":6}");
    for body in [
        "{\"action\":\"serial\",\"serialNumber\":-1}",
        "{\"action\":\"serial\",\"serialNumber\":1000000000}",
        "{\"action\":\"serial\",\"serialNumber\":\"7\"}",
        "{\"action\":\"serial\",\"serialNumber\":true}",
        "{\"action\":\"serial\",\"serialNumber\":1.5}",
        "{\"action\":\"serial\"}",
    ] {
        assert_eq!(session.action(body).unwrap_err(), ngc::session::SERIAL_RANGE_MESSAGE, "{body}");
    }
    // The largest nine-digit serial is accepted; it is stored as the EEPROM uint32 at offset 0.
    let state = act(&mut session, "{\"action\":\"serial\",\"serialNumber\":999999999}");
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(999_999_999));
    act(&mut session, "{\"action\":\"advance\",\"seconds\":0.1}");
    let epoch_before = session.state().get("outputHistoryEpoch").and_then(Json::as_str).unwrap().to_string();
    let state = act(&mut session, "{\"action\":\"serial\",\"serialNumber\":123456}");
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(123_456));
    assert!(time(&state) < 0.01, "the fixture restarts the system: {}", time(&state));
    assert_ne!(state.get("outputHistoryEpoch").and_then(Json::as_str), Some(epoch_before.as_str()));
    // The serial is part of the persisted EEPROM and survives another restart.
    act(&mut session, "{\"action\":\"reset\"}");
    assert_eq!(session.state().get("serialNumber").and_then(Json::as_u64), Some(123_456));
    let profile = session.shutdown();
    let eeprom = profile.eeprom.expect("eeprom.bin");
    assert_eq!(u32::from_le_bytes([eeprom[0], eeprom[1], eeprom[2], eeprom[3]]), 123_456);
}

#[test]
fn an_observed_standby_stops_the_run_until_wake() {
    let (main, handset) = firmware_or_skip!();
    let config = SessionConfig { boot_mode: BootMode::Cold, ..SessionConfig::default() };
    let mut session = dual(config, Profile::default(), &main, &handset);
    let outcome = session.run_for(3.0);
    assert!(outcome.standby && outcome.stopped && !outcome.running, "{outcome:?}");
    assert!(outcome.advanced_ns < 3_000_000_000, "the run stops at the standby request: {}", outcome.advanced_ns);
    let state = session.state();
    assert_eq!(state.get("standby"), Some(&Json::Bool(true)));
    assert_eq!(state.get("running"), Some(&Json::Bool(false)));
    assert_eq!(session.action("{\"action\":\"resume\"}").unwrap_err(), "The firmware requested standby; use Wake system");
    // Advance and the host's pacing do nothing in standby.
    let stopped_at = time(&state);
    assert_eq!(time(&act(&mut session, "{\"action\":\"advance\",\"seconds\":0.5}")), stopped_at);
    assert_eq!(session.run_for(0.5).advanced_ns, 0);
    // Wake restarts with the handset-wake fixture and leaves standby; the host can then resume.
    let state = act(&mut session, "{\"action\":\"wake\"}");
    assert_eq!(state.get("standby"), Some(&Json::Bool(false)));
    assert_eq!(state.get("bootMode").and_then(Json::as_str), Some("handset-wake"));
    act(&mut session, "{\"action\":\"resume\"}");
    assert!(session.run_for(0.5).advanced_ns >= 500_000_000);
}

#[test]
fn a_firmware_system_reset_request_resets_the_machine_and_the_firmware_boots_again() {
    let (main, handset) = firmware_or_skip!();
    let mut session = dual(SessionConfig::default(), Profile::default(), &main, &handset);
    session.run_for(2.0);
    assert!(session.system().handset_powered());
    let before = session.system().instructions(Which::Handset).unwrap();
    assert!(before > 90_000_000);
    // A marker in plain RAM and in an ArrayMemory register store (PWR) must survive; the executed-instruction counter
    // restarts like tlib's.
    {
        let board = session.system_mut().board_mut(Which::Handset).unwrap();
        assert!(board.poke(0x2000_7000, Width::Word, 0xC0FF_EE11));
        assert!(board.poke(0x4000_7020, Width::Word, 0xABCD_0123));
        let now = board.now();
        board.cpu.ppb_poke32(0xE000_ED0C, 0x05FA_0004, now); // AIRCR.VECTKEY | SYSRESETREQ
    }
    let time = session.virtual_ns();
    session.run_for(0.0003);
    let system = session.system();
    assert_eq!(system.reset_log().len(), 1, "{:?}", system.reset_log());
    let event = system.reset_log()[0];
    assert_eq!(event.board, Which::Handset);
    assert_eq!(event.cause.name(), "sysresetreq");
    assert!(event.applied_at > time && event.applied_at <= time + 400_000, "{event:?}");
    let board = system.board(Which::Handset).unwrap();
    assert_eq!(board.peek(0x2000_7000, Width::Word), Some(0xC0FF_EE11), "SRAM keeps its content (MappedMemory.Reset does nothing)");
    assert_eq!(board.peek(0x4000_7020, Width::Word), Some(0xABCD_0123), "ArrayMemory keeps its content");
    let after = system.instructions(Which::Handset).unwrap();
    assert!(after < 100_000, "the instruction counter restarted: {after}");
    assert!(session.error().is_none());
    // The application runs again from its reset vector: the LCD is re-initialized and the UI comes back.
    session.run_for(3.0);
    let state = Json::parse(&session.state_json()).unwrap();
    assert!(state.get("lcdSummary").and_then(Json::as_str).is_some_and(|s| s.contains("panelOn=True")), "{:?}", state.get("lcdSummary"));
    assert_eq!(state.get("machineResets").map(Json::len), Some(1));
}
