//! Engine contract of the main-viewer parity (DESIGN 15.3) with the real firmware images, skipped when the gitignored SREC
//! files are not available: output activity histories in a session (a vibrator and a HUD pulse appear, the epoch changes
//! when the histories start over, fast-forward on and off agree), the HUD color defaults, the I2C idle-high fixture, the
//! nine-digit serial and the firmware releases (identification, mixed pairs, NEPTUN state, cold boot refusal, smoke check).

use emu_core::{Json, Width};
use ngc::firmware::{self, Firmware, Release, Role, NEPTUN, TRITON};
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::Which;
use std::path::PathBuf;

fn release_dir(release: &Release) -> Option<PathBuf> {
    // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    roots
        .into_iter()
        .flatten()
        .map(|root| root.join(release.id))
        .find(|d| d.join(release.main.file_name).is_file() && d.join(release.handset.file_name).is_file())
}

fn images(release: &Release) -> Option<(Firmware, Firmware)> {
    let dir = release_dir(release)?;
    let main = firmware::load(&std::fs::read(dir.join(release.main.file_name)).ok()?, Some(Role::Main)).ok()?;
    let handset = firmware::load(&std::fs::read(dir.join(release.handset.file_name)).ok()?, Some(Role::Handset)).ok()?;
    Some((main, handset))
}

macro_rules! images_or_skip {
    ($release:expr) => {
        match images($release) {
            Some(images) => images,
            None => {
                eprintln!("skipping: the {} SREC files are not available", $release.id);
                return;
            }
        }
    };
}

fn session(config: SessionConfig, main: &Firmware, handset: &Firmware) -> Session {
    Session::new(config, Some(main), handset, Profile::default()).expect("dual session")
}

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn output(state: &Json, id: &str) -> Json {
    state.get("hardwareOutputs").and_then(Json::as_array).and_then(|items| items.iter().find(|o| o.get("id").and_then(Json::as_str) == Some(id)).cloned()).unwrap_or(Json::Null)
}

fn field_u64(json: &Json, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(json, |value, key| value.get(key)).and_then(Json::as_u64)
}

fn events(output: &Json, history: &str) -> Vec<Json> {
    output.get(history).and_then(|h| h.get("events")).and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default()
}

fn write(session: &mut Session, which: Which, address: u32, value: u32) {
    session.system_mut().board_mut(which).expect("board").bus_write(address, Width::Word, value);
}

fn peek(session: &Session, which: Which, address: u32) -> u32 {
    session.system().board(which).expect("board").peek(address, Width::Word).unwrap_or(0xFFFF_FFFF)
}

// ---- output activity histories ---------------------------------------------------------------------------------------

#[test]
fn a_vibrator_and_a_hud_pulse_appear_in_the_histories() {
    let (main, handset) = images_or_skip!(&TRITON);
    let mut s = session(SessionConfig::default(), &main, &handset);
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":2}");
    // The unmodified firmware already drives the vibrator once at boot: the enable edges are exact, the PWM command is sampled.
    let vibrator = output(&state, "handset-vibrator");
    assert!(field_u64(&vibrator, &["activity", "activationCount"]).is_some_and(|n| n >= 1), "the boot pulse is recorded: {vibrator}");
    assert_eq!(vibrator.get("activity").and_then(|a| a.get("source")).and_then(Json::as_str), Some("gpio-enable-command"));
    assert_eq!(events(&vibrator, "activity").first().and_then(|e| e.get("level")), Some(&Json::Bool(true)));
    let first_pwm = events(&vibrator, "pwmActivity");
    assert_eq!(first_pwm.first().and_then(|e| e.get("kind")).and_then(Json::as_str), Some("initial-sample"));
    assert_eq!(first_pwm.first().and_then(|e| e.get("virtualTime")).and_then(Json::as_f64), Some(0.02), "the first sample is at the first 20 ms boundary");
    // Main HUD histories start with an initial sample at 50 ms.
    let hud1 = output(&state, "main-hud-1");
    assert_eq!(events(&hud1, "activity").first().and_then(|e| e.get("virtualTime")).and_then(Json::as_f64), Some(0.05));
    assert_eq!(hud1.get("pwmActivity"), Some(&Json::Null));
    assert_eq!(output(&state, "handset-backlight").get("activity"), Some(&Json::Null));

    // A HUD pulse: TIM4 CH1..CH3 configured by explicit register writes at the paused time, seen at the next 50 ms boundary.
    let before = field_u64(&hud1, &["activity", "eventCount"]).unwrap();
    let cr1 = peek(&s, Which::Main, 0x4000_0800);
    write(&mut s, Which::Main, 0x4000_0800, cr1 | 1);
    write(&mut s, Which::Main, 0x4000_0820, 0x111);
    for (address, value) in [(0x4000_0834, 500), (0x4000_0838, 1000), (0x4000_083C, 0)] {
        write(&mut s, Which::Main, address, value);
    }
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":0.1}");
    let hud1 = output(&state, "main-hud-1");
    let hud3 = output(&state, "main-hud-3");
    let history = events(&hud1, "activity");
    let last = history.last().expect("a HUD event");
    assert!(field_u64(&hud1, &["activity", "eventCount"]).unwrap() > before, "the pulse is a new event");
    assert_eq!(last.get("kind").and_then(Json::as_str), Some("change"));
    assert_eq!(last.get("active"), Some(&Json::Bool(true)));
    assert_eq!(last.get("command").and_then(|c| c.get("ccr")).and_then(Json::as_u64), Some(500));
    assert_eq!(last.get("command").and_then(|c| c.get("timerEnabled")), Some(&Json::Bool(true)));
    assert!(field_u64(&hud1, &["activity", "activationCount"]).is_some_and(|n| n >= 1) && hud1.get("activity").and_then(|a| a.get("lastOnVirtualTime")).is_some_and(|t| !t.is_null()));
    assert_eq!(events(&hud3, "activity").last().and_then(|e| e.get("active")), Some(&Json::Bool(false)), "CCR 0 is a supported but inactive command");
    // The pulse ends: another change, an off time.
    write(&mut s, Which::Main, 0x4000_0800, cr1 & !1);
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":0.1}");
    let hud1 = output(&state, "main-hud-1");
    assert_eq!(events(&hud1, "activity").last().and_then(|e| e.get("active")), Some(&Json::Bool(false)));
    assert!(hud1.get("activity").and_then(|a| a.get("lastOffVirtualTime")).is_some_and(|t| !t.is_null()));
    assert_eq!(hud1.get("activity").and_then(|a| a.get("samplingPeriodSeconds")).and_then(Json::as_f64), Some(0.05));

    // A vibrator enable pulse: PB15 set / reset through BSRR; the edge carries the clock time of the write.
    let vibrator = output(&state, "handset-vibrator");
    let count = field_u64(&vibrator, &["activity", "eventCount"]).unwrap();
    let at = state.get("virtualTime").and_then(Json::as_f64).unwrap();
    write(&mut s, Which::Handset, 0x4800_0418, 0x8000);
    act(&mut s, "{\"action\":\"advance\",\"seconds\":0.05}");
    write(&mut s, Which::Handset, 0x4800_0428, 0x8000);
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":0.05}");
    let vibrator = output(&state, "handset-vibrator");
    let history = events(&vibrator, "activity");
    let tail = &history[history.len() - 2..];
    assert_eq!(field_u64(&vibrator, &["activity", "eventCount"]), Some(count + 2));
    assert_eq!((tail[0].get("level"), tail[1].get("level")), (Some(&Json::Bool(true)), Some(&Json::Bool(false))));
    assert_eq!(tail[0].get("virtualTime").and_then(Json::as_f64), Some(at), "the enable edge is stamped with the time of the write");
    assert!(tail[1].get("virtualTime").and_then(Json::as_f64).unwrap() > at);
    assert_eq!(tail[0].get("active"), Some(&Json::Null));
    assert_eq!(tail[0].get("dutyPercent"), Some(&Json::Null));
    assert_eq!(tail[0].get("command"), Some(&Json::Null));
    // Sequence numbers count without gaps.
    let sequences: Vec<u64> = history.iter().filter_map(|e| e.get("sequence").and_then(Json::as_u64)).collect();
    assert!(sequences.windows(2).all(|w| w[1] == w[0] + 1), "{sequences:?}");
}

#[test]
fn the_epoch_is_the_nonce_and_a_generation_that_changes_when_the_histories_start_over() {
    let (main, handset) = images_or_skip!(&TRITON);
    let config = SessionConfig { history_nonce: 123_456_789_012_345, ..SessionConfig::default() };
    let mut s = session(config, &main, &handset);
    let epoch = |s: &Session| s.state().get("outputHistoryEpoch").and_then(Json::as_str).unwrap().to_string();
    assert_eq!(epoch(&s), "123456789012345-1");
    act(&mut s, "{\"action\":\"advance\",\"seconds\":2}");
    assert_eq!(epoch(&s), "123456789012345-1", "running does not change the epoch");
    assert_eq!(act(&mut s, "{\"action\":\"capture\"}").get("outputHistoryEpoch").and_then(Json::as_str), Some("123456789012345-1"));
    // Restart, Cold and Wake recreate the boards: a new generation each time, the histories start over.
    let state = act(&mut s, "{\"action\":\"reset\"}");
    assert_eq!(state.get("outputHistoryEpoch").and_then(Json::as_str), Some("123456789012345-2"));
    assert_eq!(field_u64(&output(&state, "handset-vibrator"), &["activity", "eventCount"]), Some(0));
    assert_eq!(act(&mut s, "{\"action\":\"cold\"}").get("outputHistoryEpoch").and_then(Json::as_str), Some("123456789012345-3"));
    assert_eq!(act(&mut s, "{\"action\":\"wake\"}").get("outputHistoryEpoch").and_then(Json::as_str), Some("123456789012345-4"));
    // A machine reset of one board (SYSRESETREQ) clears the histories in place and takes a generation too.
    act(&mut s, "{\"action\":\"advance\",\"seconds\":2}");
    assert!(field_u64(&output(&s.state(), "handset-vibrator"), &["activity", "eventCount"]).is_some_and(|n| n > 0));
    {
        let board = s.system_mut().board_mut(Which::Handset).unwrap();
        let now = board.now();
        board.cpu.ppb_poke32(0xE000_ED0C, 0x05FA_0004, now); // AIRCR.VECTKEY | SYSRESETREQ
    }
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":0.001}");
    assert_eq!(state.get("machineResets").map(Json::len), Some(1));
    assert_eq!(state.get("outputHistoryEpoch").and_then(Json::as_str), Some("123456789012345-5"));
    assert_eq!(field_u64(&output(&state, "handset-vibrator"), &["activity", "eventCount"]), Some(0), "the handset histories are cleared by its reset");
    // The default nonce is 0; the deprecated seed call has no effect on the id.
    let mut plain = session(SessionConfig::default(), &main, &handset);
    plain.set_epoch_seed(0xDEAD_BEEF);
    assert_eq!(epoch(&plain), "0-1");
}

#[test]
fn fast_forward_on_and_off_give_identical_histories_and_digests() {
    let (main, handset) = images_or_skip!(&TRITON);
    let run = |fast_forward: bool| {
        let config = SessionConfig { idle_fast_forward: fast_forward, ..SessionConfig::default() };
        let mut s = session(config, &main, &handset);
        act(&mut s, "{\"action\":\"advance\",\"seconds\":2}");
        write(&mut s, Which::Main, 0x4000_0820, 0x111);
        write(&mut s, Which::Main, 0x4000_0834, 700);
        write(&mut s, Which::Handset, 0x4800_0418, 0x8000);
        let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":0.5}");
        (state.get("hardwareOutputs").cloned().unwrap(), s.system().fingerprint(), s.system().guest_fingerprint())
    };
    let (on, off) = (run(true), run(false));
    assert_eq!(on.0, off.0, "hardwareOutputs including the activity histories");
    assert_eq!(on.1, off.1, "the system fingerprint, which includes the histories");
    assert_eq!(on.2, off.2, "the guest fingerprint");
    assert!(field_u64(&output(&Json::object().with("hardwareOutputs", on.0), "main-hud-1"), &["activity", "eventCount"]).is_some_and(|n| n >= 2));
}

#[test]
fn histories_do_not_perturb_the_guest() {
    // The guest fingerprint ignores the histories, so an observer that samples (or not) cannot change it: reading the state
    // document, which renders the histories, between two runs leaves the same result as not reading it.
    let (main, handset) = images_or_skip!(&TRITON);
    let plain = {
        let mut s = session(SessionConfig::default(), &main, &handset);
        act(&mut s, "{\"action\":\"advance\",\"seconds\":2.5}");
        s.system().guest_fingerprint()
    };
    let observed = {
        let mut s = session(SessionConfig::default(), &main, &handset);
        for _ in 0..25 {
            act(&mut s, "{\"action\":\"advance\",\"seconds\":0.1}");
            let _ = s.state_json();
        }
        s.system().guest_fingerprint()
    };
    assert_eq!(plain, observed);
}

// ---- HUD colors, I2C fixture, serial --------------------------------------------------------------------------------

#[test]
fn hud_color_defaults_and_saved_choices() {
    let (main, handset) = images_or_skip!(&TRITON);
    let s = session(SessionConfig::default(), &main, &handset);
    let colors = |state: &Json| ["main-hud-1", "main-hud-2", "main-hud-3"].map(|id| output(state, id).get("color").and_then(Json::as_str).unwrap_or("?").to_string());
    assert_eq!(colors(&s.state()), ["unknown", "white", "red"], "HUD 1 unknown, HUD 2 white, HUD 3 red");
    // A saved led-colors.json overrides the defaults it names; the others keep theirs.
    let profile = Profile { led_colors: Some("{\"main-hud-2\": \"red\", \"main-hud-3\": \"white\"}".to_string()), ..Profile::default() };
    let s = Session::new(SessionConfig::default(), Some(&main), &handset, profile).expect("session");
    assert_eq!(colors(&s.state()), ["unknown", "red", "white"]);
}

#[test]
fn the_i2c_idle_high_fixture_is_on_by_default_and_can_be_turned_off() {
    let (main, handset) = images_or_skip!(&TRITON);
    let idr = |s: &Session| peek(s, Which::Main, 0x4800_0410) & 0xCC0;
    let on = session(SessionConfig::default(), &main, &handset);
    assert_eq!(idr(&on), 0xCC0, "PB6, PB7, PB10 and PB11 read high before the first instruction");
    let state = on.state();
    assert_eq!(state.get("i2cIdleHigh"), Some(&Json::Bool(true)));
    assert!(state.get("i2cFixture").and_then(Json::as_str).is_some_and(|text| text.contains("driven high")));
    let off = session(SessionConfig { i2c_idle_high: false, ..SessionConfig::default() }, &main, &handset);
    assert_eq!(idr(&off), 0, "without the fixture the lines stay low");
    let state = off.state();
    assert_eq!(state.get("i2cIdleHigh"), Some(&Json::Bool(false)));
    assert!(state.get("i2cFixture").and_then(Json::as_str).is_some_and(|text| text.contains("off")));
    // It survives Restart and a machine reset (it is part of the platform script).
    let mut on = on;
    act(&mut on, "{\"action\":\"reset\"}");
    assert_eq!(idr(&on), 0xCC0);
    // The fixture changes the start-up it is meant to fix: the two runs are different boots (and both reach the B1 prompt).
    let mut a = session(SessionConfig::default(), &main, &handset);
    let mut b = session(SessionConfig { i2c_idle_high: false, ..SessionConfig::default() }, &main, &handset);
    act(&mut a, "{\"action\":\"advance\",\"seconds\":1.4}");
    act(&mut b, "{\"action\":\"advance\",\"seconds\":1.4}");
    assert_ne!(a.system().guest_fingerprint(), b.system().guest_fingerprint());
}

#[test]
fn the_serial_accepts_nine_digits_and_refuses_ten() {
    let (main, handset) = images_or_skip!(&TRITON);
    let mut s = session(SessionConfig::default(), &main, &handset);
    act(&mut s, "{\"action\":\"advance\",\"seconds\":6}");
    for body in ["{\"action\":\"serial\",\"serialNumber\":1000000000}", "{\"action\":\"serial\",\"serialNumber\":-1}", "{\"action\":\"serial\",\"serialNumber\":4294967295}"] {
        let message = s.action(body).unwrap_err();
        assert_eq!(message, ngc::session::SERIAL_RANGE_MESSAGE, "{body}");
        assert!(message.contains("0 to 999999999") && message.contains("nine digits"));
    }
    let state = act(&mut s, "{\"action\":\"serial\",\"serialNumber\":999999999}");
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(999_999_999));
    let state = act(&mut s, "{\"action\":\"serial\",\"serialNumber\":0}");
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(0));
}

// ---- firmware releases -----------------------------------------------------------------------------------------------

#[test]
fn both_releases_are_identified_per_role() {
    for release in firmware::RELEASES {
        let Some(dir) = release_dir(release) else {
            eprintln!("skipping: the {} SREC files are not available", release.id);
            continue;
        };
        for role in [Role::Main, Role::Handset] {
            let bytes = std::fs::read(dir.join(release.expected(role).file_name)).unwrap();
            let (found, found_role) = firmware::identify_release(&bytes).expect("identified");
            assert_eq!((found.id, found_role), (release.id, role));
            let report = firmware::inspect(&bytes);
            assert!(report.ok() && report.release.map(|r| r.id) == Some(release.id), "{:?}", report.failed_checks());
            let json = report.to_json();
            assert_eq!(json.get("release").and_then(|r| r.get("id")).and_then(Json::as_str), Some(release.id));
            assert_eq!(json.get("release").and_then(|r| r.get("label")).and_then(Json::as_str), Some(release.label));
            assert_eq!(json.get("role").and_then(Json::as_str), Some(role.name()));
            let loaded = firmware::load(&bytes, Some(role)).expect("loads");
            assert_eq!(loaded.release.id, release.id);
            // The vectors of the image itself decide where the CPU starts.
            assert_eq!((loaded.initial_sp(), loaded.vectors.reset_vector), (0x2001_8000, release.expected(role).reset_vector));
        }
    }
    // An unknown image has no release.
    let report = firmware::inspect(b"S0030000FC\nS1050000AABB95\nS9030000FC\n");
    assert!(report.release.is_none() && report.to_json().get("release") == Some(&Json::Null));
}

#[test]
fn a_mixed_pair_is_refused_in_both_directions() {
    let (Some((triton_main, triton_handset)), Some((neptun_main, neptun_handset))) = (images(&TRITON), images(&NEPTUN)) else {
        eprintln!("skipping: both firmware releases are needed");
        return;
    };
    for (main, handset, text) in [(&triton_main, &neptun_handset, "main image is TRITON-5.8-65.3"), (&neptun_main, &triton_handset, "main image is NEPTUN-5.8-65.3")] {
        let error = Session::new(SessionConfig::default(), Some(main), handset, Profile::default()).err().expect("refused");
        assert!(error.contains("Mixed firmware releases") && error.contains(text) && error.contains("same release"), "{error}");
        assert!(ngc::system::System::new(ngc::system::SystemConfig::default(), Some(main), handset).is_err());
        assert!(firmware::common_release(main, handset).is_err());
    }
    // The handset alone does not need a main image, and a matching pair is accepted.
    let alone = SessionConfig { mode: ngc::system::Mode::HandsetOnly, ..SessionConfig::default() };
    assert!(Session::new(alone, None, &neptun_handset, Profile::default()).is_ok());
    assert_eq!(firmware::common_release(&neptun_main, &neptun_handset).map(|r| r.id), Ok("NEPTUN-5.8-65.3"));
}

#[test]
fn the_neptun_address_table_is_proven_against_the_images() {
    let Some((main, handset)) = images(&NEPTUN) else {
        eprintln!("skipping: the NEPTUN SREC files are not available");
        return;
    };
    let word = |firmware: &Firmware, address: u32| {
        let offset = (address - firmware.span_base) as usize;
        u32::from_le_bytes(firmware.span[offset..offset + 4].try_into().unwrap())
    };
    let half = |firmware: &Firmware, address: u32| {
        let offset = (address - firmware.span_base) as usize;
        u16::from_le_bytes([firmware.span[offset], firmware.span[offset + 1]])
    };
    let addresses = &NEPTUN.addresses;
    // handset error loop: `cpsid i; b .` at the table's address (the HAL error handler).
    let pc = addresses.handset_error_loop.address().expect("known");
    assert_eq!((half(&handset, pc - 2), half(&handset, pc)), (0xB672, 0xE7FE));
    // FreeRTOS PendSV handler (vector 14): its first literal load is pxCurrentTCB.
    let pendsv = handset_or_main_pendsv(&main);
    assert_eq!(half(&main, pendsv + 8), 0x4B15, "ldr r3, [pc, #0x54]");
    assert_eq!(word(&main, ((pendsv + 8 + 4) & !3) + 0x54), addresses.main_current_tcb.address().expect("known"));
    // The handset's key sampler and kernel load the table's literals.
    let loads = |firmware: &Firmware, wanted: u32| {
        let mut count = 0;
        let mut address = firmware.span_base;
        let end = firmware.span_base + firmware.span.len() as u32 - 4;
        while address < end {
            let hw = half(firmware, address);
            if hw & 0xF800 == 0x4800 {
                let target = ((address + 4) & !3) + u32::from(hw & 0xFF) * 4;
                if target + 4 <= firmware.span_base + firmware.span.len() as u32 && word(firmware, target) == wanted {
                    count += 1;
                }
            }
            address += 2;
        }
        count
    };
    assert!(loads(&handset, addresses.handset_orientation.address().unwrap()) >= 2, "the key sampler loads the orientation byte's address");
    assert!(loads(&handset, addresses.handset_current_tcb.address().unwrap()) >= 8, "the FreeRTOS kernel loads pxCurrentTCB");
    // Everything that could not be proven is unavailable with a reason, never a TRITON value.
    for (name, entry) in addresses.entries() {
        match entry {
            firmware::AddressEntry::Known { address, basis } => assert!(!basis.is_empty() && *address != 0, "{name}"),
            firmware::AddressEntry::Unavailable { reason } => assert!(reason.contains("not proven") || reason.contains("different"), "{name}: {reason}"),
        }
    }
    assert_ne!(addresses.main_current_tcb.address(), TRITON.addresses.main_current_tcb.address(), "the main RAM layout differs");
    assert!(addresses.main_battery_ready.address().is_none() && addresses.main_hal_tick.address().is_none());
}

/// The PendSV handler address (Thumb bit cleared) from vector 14 of an image.
fn handset_or_main_pendsv(firmware: &Firmware) -> u32 {
    firmware.vectors.entries[14] & !1
}

#[test]
fn a_neptun_session_reports_its_release_and_leaves_unproven_fields_unavailable() {
    let (main, handset) = images_or_skip!(&NEPTUN);
    let mut s = session(SessionConfig::default(), &main, &handset);
    let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":1.5}");
    let firmware = state.get("firmware").expect("firmware");
    assert_eq!(firmware.get("release").and_then(|r| r.get("id")).and_then(Json::as_str), Some("NEPTUN-5.8-65.3"));
    assert_eq!(firmware.get("release").and_then(|r| r.get("label")).and_then(Json::as_str), Some("NEPTUN main 5.8 / handset 65.3"));
    assert_eq!(firmware.get("main").and_then(|m| m.get("resetPC")).and_then(Json::as_u64), Some(0x0803_90B8));
    assert_eq!(firmware.get("handset").and_then(|m| m.get("resetPC")).and_then(Json::as_u64), Some(0x0800_8444));
    assert_eq!(firmware.get("main").and_then(|m| m.get("srecSha256")).and_then(Json::as_str), Some("e462bc7345d6ded69124b97b87de9a68884f44b8e716fbbf4fe3839ff8da8c89"));
    assert_eq!(firmware.get("handset").and_then(|m| m.get("srecSha256")).and_then(Json::as_str), Some("f91adcf461fa0e06ef40ab3f757dd66754180b9711956542736efb0d4be8162e"));
    // Not proven: the battery readiness byte is null with a reason (never TRITON's 0x200042A1), in both places.
    assert_eq!(state.get("mainBatteryReady"), Some(&Json::Null));
    let reason = state.get("unavailable").and_then(|u| u.get("mainBatteryReady")).and_then(Json::as_str).expect("reason");
    assert!(reason.contains("not proven"), "{reason}");
    let battery = firmware.get("addresses").and_then(|a| a.get("mainBatteryReady")).expect("address entry");
    assert_eq!((battery.get("address"), battery.get("reason").and_then(Json::as_str).is_some()), (Some(&Json::Null), true));
    assert_eq!(firmware.get("addresses").and_then(|a| a.get("handsetOrientation")).and_then(|o| o.get("address")).and_then(Json::as_u64), Some(0x2000_0740));
    assert_eq!(firmware.get("addresses").and_then(|a| a.get("mainCurrentTcb")).and_then(|o| o.get("address")).and_then(Json::as_u64), Some(0x2000_53A8));
    // The handset is released at the 1.05 s poll and Up/Down work through the proven orientation byte.
    assert_eq!(state.get("handsetReleaseTime").and_then(Json::as_f64), Some(1.05));
    act(&mut s, "{\"action\":\"up\"}");
    act(&mut s, "{\"action\":\"advance\",\"seconds\":0.3}");
    act(&mut s, "{\"action\":\"down\"}");
    // Cold boot has no observed-standby route on NEPTUN: refused with the reason, nothing is shut down.
    let before = s.virtual_ns();
    let error = s.action("{\"action\":\"cold\"}").unwrap_err();
    assert!(error.contains("cold-boot fixture") && error.contains("TRITON-5.8-65.3 only"), "{error}");
    assert_eq!(s.virtual_ns(), before, "a refused cold boot leaves the session as it was (nothing was recreated)");
    assert_eq!(s.state().get("outputHistoryEpoch").and_then(Json::as_str), Some("0-1"));
    let config = SessionConfig { boot_mode: ngc::system::BootMode::Cold, ..SessionConfig::default() };
    let error = Session::new(config, Some(&main), &handset, Profile::default()).err().expect("refused at creation");
    assert!(error.contains("TRITON-5.8-65.3 only"), "{error}");
    // Wake and Restart work.
    act(&mut s, "{\"action\":\"wake\"}");
    act(&mut s, "{\"action\":\"reset\"}");
}

#[test]
fn neptun_dual_boot_reaches_the_battery_selection_screen() {
    let (main, handset) = images_or_skip!(&NEPTUN);
    // The B1 frame it is compared with was recorded by the Renode runner with its 1500 mV batteries (a fresh profile starts
    // at 4100 mV): pin the recorded voltage explicitly.
    let recorded = Profile { inputs: Some(ngc::persistence::inputs_file_text(&ngc::fixtures::Inputs::recorded_evidence())), ..Profile::default() };
    for idle_high in [true, false] {
        let config = SessionConfig { i2c_idle_high: idle_high, ..SessionConfig::default() };
        let mut s = Session::new(config, Some(&main), &handset, recorded.clone()).expect("dual session");
        let state = act(&mut s, "{\"action\":\"advance\",\"seconds\":10.5}");
        assert_eq!(state.get("handsetReleaseTime").and_then(Json::as_f64), Some(1.05), "idle_high {idle_high}");
        assert_eq!(state.get("error"), Some(&Json::Null));
        assert_eq!(state.get("frameReady"), Some(&Json::Bool(true)));
        for which in [Which::Main, Which::Handset] {
            assert_eq!((peek(&s, which, 0xE000_ED28), peek(&s, which, 0xE000_ED2C)), (0, 0), "CFSR/HFSR of {which:?}, idle_high {idle_high}");
        }
        let summary = state.get("canSummary").and_then(Json::as_str).unwrap();
        assert!(summary.contains("dropped=0") && !summary.contains("transmitted=0;"), "{summary}");
        let frames = summary.split("transmitted=").nth(1).and_then(|t| t.split(';').next()).and_then(|n| n.parse::<u32>().ok()).unwrap();
        assert!(frames >= 50, "CAN frames forwarded: {frames}");
        // The frame is pixel-identical to the TRITON B1 battery-selection prompt (SHA-256 recorded from Renode for TRITON).
        let ppm = s.system_mut().lcd_ppm().expect("lcd");
        assert_eq!(ngc::sha256::digest_hex(&ppm), "62c3a30e54031ff3c2aeffa16a6b9f361c64ab2326db35da7e345c5318f7632d", "idle_high {idle_high}");
    }
}
