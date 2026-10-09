//! Custom (native) builds through the host-facing `Session` and the dual `System` (DESIGN.md section 20, work package NATIVE-ENGINE):
//!
//! * **synthetic images** (a few Thumb instructions built by the test, no firmware needed): standby detected from the hardware state (a
//!   sleeping core with `SCB.SCR.SLEEPDEEP` and a PWR standby/shutdown mode), the wake fixture without the original application's backup
//!   marker, the per-board fault registers and the lockup, custom images surviving a machine reset;
//! * **the TRITON pair through the custom path** (skipped without the gitignored SREC files): structurally valid, loaded without release
//!   identification, booted to the B1 battery prompt where Up, Down and Confirm still navigate through the pins, with every original
//!   diagnostic unavailable and the EEPROM left blank; Restart, Cold, Wake and a machine reset keep the images; a custom image never
//!   makes a pair with an original one.
//!
//! The readbacks of the original application RAM below (screen id, battery wizard) are the test's own: the engine reads none of it in
//! custom mode.

use emu_core::{from_millis, Json, Width};
use ngc::firmware::{self, Firmware, Role};
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::{BootMode, Mode, System, SystemConfig, Which};
use std::path::PathBuf;

// ---- synthetic images -------------------------------------------------------------------------------------------------

fn s_record(kind: u8, address_bytes: usize, address: u32, data: &[u8]) -> String {
    let mut body = vec![(address_bytes + data.len() + 1) as u8];
    for i in (0..address_bytes).rev() {
        body.push((address >> (8 * i)) as u8);
    }
    body.extend_from_slice(data);
    let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    body.push(!sum);
    format!("S{kind}{}", body.iter().map(|b| format!("{b:02X}")).collect::<String>())
}

/// A custom image: 64 vectors (SP `0x20018000`, every handler the reset code), the Thumb `code` at `0x08004100` and the literal words
/// `literals` at `0x08004118` (a `ldr rN, [pc, #imm]` reaches them).
fn synthetic(code: &[u16], literals: &[u32]) -> Vec<u8> {
    assert!(code.len() <= 12 && literals.len() <= 4);
    let mut table: Vec<u8> = Vec::new();
    for word in std::iter::once(0x2001_8000u32).chain(std::iter::repeat(0x0800_4101).take(63)) {
        table.extend_from_slice(&word.to_le_bytes());
    }
    let mut body = vec![0u8; 0x20];
    for (i, half) in code.iter().enumerate() {
        body[2 * i..2 * i + 2].copy_from_slice(&half.to_le_bytes());
    }
    for (i, word) in literals.iter().enumerate() {
        body[0x18 + 4 * i..0x1C + 4 * i].copy_from_slice(&word.to_le_bytes());
    }
    let mut lines = vec![s_record(0, 2, 0, b"synthetic.srec")];
    for (address, data) in [(0x0800_4000u32, table), (0x0800_4100, body)] {
        for (i, chunk) in data.chunks(16).enumerate() {
            lines.push(s_record(3, 4, address + 16 * i as u32, chunk));
        }
    }
    lines.push(s_record(7, 4, 0x0800_4101, &[]));
    (lines.join("\n") + "\n").into_bytes()
}

/// `SCB.SCR = scr; PWR.CR1 = lpms;` and then `wfi` (the core sleeps) or a loop of two plain instructions (the core stays awake).
fn power_mode(scr: u8, lpms: u8, sleep: bool) -> Vec<u8> {
    let tail = if sleep { [0xBF30, 0xE7FE] } else { [0xBF00, 0xE7FD] };
    synthetic(
        &[0x4805, 0x2100 | u16::from(scr), 0x6001, 0x4805, 0x2100 | u16::from(lpms), 0x6001, tail[0], tail[1]],
        &[0xE000_ED10, 0x4000_7000],
    )
}

/// An undefined instruction as the reset code: a UsageFault, escalated to a HardFault whose handler is the same code, which faults
/// again at its own priority: lockup.
fn undefined_instruction() -> Vec<u8> {
    synthetic(&[0xDE00], &[])
}

/// `SCB.AIRCR = VECTKEY | SYSRESETREQ`, then a `nop` loop (as `cortex_m::peripheral::SCB::sys_reset` does; a branch-to-self would
/// be executed as `wfi` and the sleeping core would not stop for the request): a machine reset at the end of every quantum.
fn reset_loop() -> Vec<u8> {
    synthetic(&[0x4805, 0x4906, 0x6001, 0xBF00, 0xE7FD], &[0xE000_ED0C, 0x05FA_0004])
}

fn custom(bytes: &[u8], role: Role) -> Firmware {
    firmware::load_custom(bytes, role).unwrap_or_else(|e| panic!("{e}"))
}

fn custom_pair(main: &[u8], handset: &[u8]) -> (Firmware, Firmware) {
    (custom(main, Role::Main), custom(handset, Role::Handset))
}

fn idle_handset() -> Vec<u8> {
    synthetic(&[0xE7FE], &[])
}

#[test]
fn standby_is_detected_from_a_sleeping_core_with_sleepdeep_and_a_standby_or_shutdown_mode() {
    // (SCB.SCR, PWR.CR1.LPMS, the core sleeps, standby expected): LPMS 3 is Standby, 4 Shutdown, 0..2 the Stop modes.
    let cases = [
        (4u8, 3u8, true, true),
        (4, 4, true, true),
        (4, 0, true, false),
        (4, 1, true, false),
        (4, 2, true, false),
        (0, 3, true, false),
        (4, 3, false, false),
        (0, 0, true, false),
    ];
    for (scr, lpms, sleep, standby) in cases {
        let (main, handset) = custom_pair(&power_mode(scr, lpms, sleep), &idle_handset());
        let mut system = System::new(SystemConfig::dual(), Some(&main), &handset).expect("system");
        system.run_until(from_millis(200));
        let label = format!("SCR {scr:#x} LPMS {lpms} sleeping {sleep}");
        assert_eq!(system.is_standby(), standby, "{label}");
        if standby {
            // The poll on the 50 ms grid sees it; both CPUs are halted; the run stops until Wake.
            assert_eq!(system.standby_time(), Some(from_millis(50)), "{label}");
            assert!(!system.can_run() && system.board(Which::Main).unwrap().cpu.is_halted() && system.board(Which::Handset).unwrap().cpu.is_halted(), "{label}");
            let before = system.time();
            system.run_until(from_millis(400));
            assert_eq!(system.time(), before, "{label}");
        } else {
            assert_eq!(system.time(), from_millis(200), "{label}");
        }
    }
}

#[test]
fn a_deep_sleep_that_an_interrupt_already_ended_still_counts_while_the_registers_hold_the_selection() {
    // On the device the sleep is the power-down. The emulated peripherals go on and may wake the core at once: `slept` carries the
    // sleep that started since the last look. The registers must still select standby or shutdown with SLEEPDEEP set.
    for (lpms, slept, expected) in [(3u8, true, true), (4, true, true), (3, false, false), (0, true, false), (2, true, false)] {
        let (main, handset) = custom_pair(&power_mode(4, lpms, false), &idle_handset());
        let mut system = System::new(SystemConfig::dual(), Some(&main), &handset).expect("system");
        system.run_until(from_millis(10));
        let board = system.board(Which::Main).unwrap();
        assert!(!board.cpu.is_sleeping(), "the core spins awake");
        assert_eq!(ngc::fixtures::standby_entered(board, slept), expected, "LPMS {lpms}, slept {slept}");
        assert!(!system.is_standby(), "the poll does not count a core that never slept");
    }
    // The counter that carries it: a WFI under SLEEPDEEP counts, an awake core or a plain sleep does not.
    for (scr, sleep, entries) in [(4u8, true, 1u64), (0, true, 0), (4, false, 0)] {
        let (main, handset) = custom_pair(&power_mode(scr, 3, sleep), &idle_handset());
        let mut system = System::new(SystemConfig::dual(), Some(&main), &handset).expect("system");
        system.run_until(from_millis(10));
        assert_eq!(system.board(Which::Main).unwrap().cpu.deep_sleep_entries(), entries, "SCR {scr:#x}, sleeping {sleep}");
    }
}

#[test]
fn the_wake_fixture_of_a_custom_build_sets_the_hardware_flags_and_leaves_the_backup_registers_alone() {
    let (main, handset) = custom_pair(&idle_handset(), &idle_handset());
    // SR1 = 0x104 (standby flag and wake-up flag 3), CSR = 0; the original application's marker in RTC.BKP1R is not written.
    let wake = System::new(SystemConfig::dual(), Some(&main), &handset).expect("system");
    let peek = |system: &System, address| system.board(Which::Main).unwrap().peek(address, Width::Word);
    assert_eq!((peek(&wake, 0x4000_7010), peek(&wake, 0x4002_1094), peek(&wake, 0x4000_2854)), (Some(0x104), Some(0), Some(0)));
    // Cold boot is possible for a custom build (the standby is observed from the hardware state): zero flags.
    let cold = System::new(SystemConfig { boot_mode: BootMode::Cold, ..SystemConfig::dual() }, Some(&main), &handset).expect("a custom pair may boot cold");
    assert_eq!((peek(&cold, 0x4000_7010), peek(&cold, 0x4002_1094), peek(&cold, 0x4000_2854)), (Some(0), Some(0), Some(0)));
    // The session says so: no BKP1R override is recorded for a custom build.
    let session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let state = session.state();
    assert_eq!(state.get("rtcPersistence").and_then(|r| r.get("mainBkp1WakeOverride")), Some(&Json::Bool(false)));
    assert_eq!(state.get("bootMode").and_then(Json::as_str), Some("handset-wake"));
}

#[test]
fn faults_report_cfsr_hfsr_and_the_lockup_of_each_board() {
    let (_, faulting) = custom_pair(&idle_handset(), &undefined_instruction());
    let config = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    let mut session = Session::new(config, None, &faulting, Profile::default()).expect("handset session");
    let quiet = session.state();
    assert_eq!(quiet.get("faults").and_then(|f| f.get("handset")).and_then(|b| b.get("cfsr")).and_then(Json::as_u64), Some(0));
    session.run_for(0.2);
    let state = session.state();
    let faults = state.get("faults").expect("faults");
    assert!(faults.get("main").is_none(), "a handset-only run has no main board");
    let handset = faults.get("handset").expect("faults.handset");
    assert_eq!(handset.get("cfsr").and_then(Json::as_u64).map(|v| v & 0x0001_0000), Some(0x0001_0000), "UsageFault.UNDEFINSTR: {handset:?}");
    assert_eq!(handset.get("hfsr").and_then(Json::as_u64).map(|v| v & 0x4000_0000), Some(0x4000_0000), "HardFault.FORCED: {handset:?}");
    assert!(handset.get("lockup").and_then(Json::as_str).is_some_and(|reason| !reason.is_empty()), "a fault inside the HardFault handler locks the core up: {handset:?}");
    // The same document for the original images: every board, zeros, no lockup.
    let (main, handset) = custom_pair(&idle_handset(), &idle_handset());
    let dual = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("dual").state();
    for board in ["main", "handset"] {
        let faults = dual.get("faults").and_then(|f| f.get(board)).unwrap_or_else(|| panic!("faults.{board}"));
        assert_eq!((faults.get("cfsr").and_then(Json::as_u64), faults.get("hfsr").and_then(Json::as_u64), faults.get("lockup")), (Some(0), Some(0), Some(&Json::Null)), "{board}: {faults:?}");
    }
}

#[test]
fn a_machine_reset_keeps_the_custom_image_and_the_state_keeps_naming_it() {
    let (main, handset) = custom_pair(&reset_loop(), &idle_handset());
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let before = session.state();
    session.run_for(0.01);
    let state = session.state();
    let resets = state.get("machineResets").and_then(Json::as_array).expect("machineResets");
    assert!(resets.len() >= 10, "the reset code runs again after every reset: {}", resets.len());
    assert!(resets.iter().all(|r| r.get("board").and_then(Json::as_str) == Some("main") && r.get("cause").and_then(Json::as_str) == Some("sysresetreq")));
    // The images are still the custom ones: the main core restarts at the reset vector of the flash content.
    assert_eq!(state.get("firmware"), before.get("firmware"));
    assert_eq!(state.get("firmware").and_then(|f| f.get("release")).and_then(|r| r.get("id")).and_then(Json::as_str), Some("CUSTOM"));
    let pc = session.system().pc(Which::Main).unwrap();
    assert!((0x0800_4100..0x0800_410A).contains(&pc), "{pc:#x}");
}

// ---- the TRITON pair through the custom path --------------------------------------------------------------------------

fn firmware_dir() -> Option<PathBuf> {
    // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    roots
        .into_iter()
        .flatten()
        .map(|root| root.join("TRITON-5.8-65.3"))
        .find(|d| d.join("ngc_main_5.8_TRITON.srec").is_file() && d.join("ngc_handset_65.3_TRITON.srec").is_file())
}

fn srec_files() -> Option<(Vec<u8>, Vec<u8>)> {
    let dir = firmware_dir()?;
    Some((std::fs::read(dir.join("ngc_main_5.8_TRITON.srec")).ok()?, std::fs::read(dir.join("ngc_handset_65.3_TRITON.srec")).ok()?))
}

/// The TRITON pair admitted as custom builds.
fn triton_as_custom() -> Option<(Firmware, Firmware)> {
    let (main, handset) = srec_files()?;
    Some(custom_pair(&main, &handset))
}

macro_rules! triton_or_skip {
    () => {
        match triton_as_custom() {
            Some(pair) => pair,
            None => {
                eprintln!("skipping: the firmware SREC files are not available");
                return;
            }
        }
    };
}

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn advance(session: &mut Session, seconds: f64) -> Json {
    act(session, &format!("{{\"action\":\"advance\",\"seconds\":{seconds}}}"))
}

/// The original handset's screen id and its battery wizard (test-side readbacks of original application RAM).
fn screen(session: &Session) -> u32 {
    session.system().board(Which::Handset).unwrap().peek(0x2000_A6E8, Width::Word).unwrap_or(0xFFFF_FFFF)
}

fn wizard(session: &Session) -> (u32, u32, u32) {
    let board = session.system().board(Which::Handset).unwrap();
    let view = board.peek(0x2001_5E48, Width::Word).unwrap_or(0);
    let byte = |offset: u32| board.peek(view + offset, Width::Byte).unwrap_or(0xFF);
    // (selection, phase, B1 type)
    (board.peek(view + 0x1C8 + 0x640, Width::Word).unwrap_or(0xFFFF_FFFF), byte(0x37F0), byte(0x37E4))
}

#[test]
fn the_triton_pair_boots_through_the_custom_path_to_b1_and_up_down_confirm_navigate() {
    let (main, handset) = triton_or_skip!();
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("custom session");

    // ---- the state names the custom build and offers no original diagnostics ----
    let state = session.state();
    let firmware = state.get("firmware").expect("firmware");
    assert_eq!(firmware.get("release").and_then(|r| r.get("id")).and_then(Json::as_str), Some("CUSTOM"));
    assert_eq!(firmware.get("release").and_then(|r| r.get("label")).and_then(Json::as_str), Some("Custom build"));
    let (main_srec, handset_srec) = srec_files().unwrap();
    for (role, label, srec) in [("main", "Custom main build", &main_srec), ("handset", "Custom handset build", &handset_srec)] {
        let facts = firmware.get(role).unwrap_or_else(|| panic!("firmware.{role}"));
        assert_eq!(facts.get("custom"), Some(&Json::Bool(true)), "{role}");
        assert_eq!(facts.get("label").and_then(Json::as_str), Some(label));
        assert_eq!(facts.get("srecSha256").and_then(Json::as_str), Some(ngc::sha256::digest_hex(srec).as_str()));
        for key in ["binSha256", "sha256", "stack", "resetPC", "path"] {
            assert!(facts.get(key).is_some(), "firmware.{role}.{key}");
        }
        assert_eq!(facts.get("stack").and_then(Json::as_u64), Some(0x2001_8000));
    }
    assert_eq!(firmware.get("main").and_then(|m| m.get("resetPC")).and_then(Json::as_u64), Some(0x0802_13B8));
    assert_ne!(firmware.get("main").and_then(|m| m.get("binSha256")), Some(&Json::from(firmware::MAIN.bin_sha256)), "the hole behind the vector table is 0x00 here, 0xFF in the original reconstruction");
    let addresses = firmware.get("addresses").and_then(Json::as_object).expect("addresses");
    assert_eq!(addresses.len(), 18);
    for (name, entry) in addresses {
        assert_eq!(entry.get("address"), Some(&Json::Null), "{name}");
        assert_eq!(entry.get("reason").and_then(Json::as_str), Some("custom build: original firmware addresses do not apply"), "{name}");
    }
    let unavailable = state.get("unavailable").and_then(Json::as_object).expect("unavailable");
    assert!(unavailable.iter().any(|(k, _)| k == "mainBatteryReady") && unavailable.iter().any(|(k, _)| k == "terminalHandlerDetection"));
    assert!(unavailable.iter().all(|(k, _)| k != "navigationOrientation"), "the orientation byte is not read any more");
    assert_eq!(state.get("mainBatteryReady"), Some(&Json::Null));
    assert_eq!(state.get("decoHealth").and_then(|d| d.get("tissues")).and_then(Json::as_str), Some("unknown"));
    assert_eq!(state.get("decoHealth").and_then(|d| d.get("oxygen")).and_then(Json::as_str), Some("unknown"));
    assert!(state.get("decoHealth").and_then(|d| d.get("details")).and_then(|d| d.get("tissues")).and_then(Json::as_str).is_some_and(|t| t.starts_with("Unknown for CUSTOM: custom build")));
    assert_eq!(state.get("eepromFactoryInit").and_then(|e| e.get("applied")), Some(&Json::Bool(false)));
    assert!(state.get("eepromFactoryInit").and_then(|e| e.get("reason")).and_then(Json::as_str).is_some_and(|r| r.starts_with("Skipped for CUSTOM")));
    assert_eq!(session.system().terminal_handler_pc(), None, "no terminal-handler stop without the original address");
    assert_eq!(session.system().main_battery_ready_flag(), None);

    // ---- boot to the B1 prompt: the same screen the original path reaches ----
    let state = advance(&mut session, 6.5);
    assert_eq!(screen(&session), 0x29, "the battery wizard");
    assert_eq!(wizard(&session), (0, 0, 255), "B1 not chosen, the first row selected");
    assert_eq!(state.get("error"), Some(&Json::Null));
    assert_eq!(state.get("mainBatteryReady"), Some(&Json::Null), "still unknown for a custom build");
    // A new EEPROM stays blank: the firmware's first-boot defaults run on erased cells, without the factory image.
    let eeprom = session.system().main.as_ref().unwrap().eeprom.image().to_vec();
    assert_eq!(&eeprom[0..4], &[0xFF; 4], "no serial number was written (the factory image would have stored 1)");
    assert_eq!(state.get("serialNumber").and_then(Json::as_u64), Some(0xFFFF_FFFF));
    // The exact routine acceleration still works: the code bytes are the original's.
    assert!(state.get("routineAccel").and_then(|r| r.get("main")).and_then(|m| m.get("hits")).and_then(Json::as_u64).is_some_and(|hits| hits > 0));

    // ---- Down, Up, Down, Confirm through the pins (Up is PE5, Down is PE3) ----
    act(&mut session, "{\"action\":\"down\"}");
    advance(&mut session, 0.65);
    assert_eq!(wizard(&session).0, 1, "Down selects the Alkaline row");
    act(&mut session, "{\"action\":\"up\"}");
    advance(&mut session, 0.65);
    assert_eq!(wizard(&session).0, 0, "Up goes back");
    act(&mut session, "{\"action\":\"down\"}");
    advance(&mut session, 0.65);
    act(&mut session, "{\"action\":\"confirm\"}");
    advance(&mut session, 0.65);
    assert_eq!((wizard(&session).1, wizard(&session).2), (1, 1), "the staggered Confirm selected the Alkaline battery for B1");
    let faults = session.state();
    for board in ["main", "handset"] {
        assert_eq!(faults.get("faults").and_then(|f| f.get(board)).and_then(|b| b.get("cfsr")).and_then(Json::as_u64), Some(0), "{board}");
        assert_eq!(faults.get("faults").and_then(|f| f.get(board)).and_then(|b| b.get("lockup")), Some(&Json::Null), "{board}");
    }
    // The state document has nothing left of the orientation handling.
    assert!(!session.state_json().contains("navigationOrientation"));
}

#[test]
fn restart_wake_cold_and_a_machine_reset_keep_the_custom_pair() {
    let (main, handset) = triton_or_skip!();
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("custom session");
    let named = |state: &Json| state.get("firmware").cloned().unwrap_or(Json::Null);
    let first = named(&session.state());
    session.run_for(2.0);
    for action in ["reset", "wake"] {
        let state = act(&mut session, &format!("{{\"action\":\"{action}\"}}"));
        assert_eq!(named(&state), first, "{action}");
        assert!(state.get("virtualTime").and_then(Json::as_f64).is_some_and(|t| t < 0.1), "{action} starts a new boot");
        // The wake flags are set (BKP1R is restored from the RTC checkpoint, the fixture does not write it: see the synthetic test).
        let board = session.system().board(Which::Main).unwrap();
        assert_eq!(board.peek(0x4000_7010, Width::Word), Some(0x104), "{action}");
        session.run_for(1.0);
    }
    // Cold is allowed for a custom build: zero wake flags. The unchanged TRITON main requests standby by setting `PWR_CR1.LPMS = 3` and
    // `SCB.SCR.SLEEPDEEP` and then idling; it never goes to sleep with them (the original runner's heuristic looks at the registers
    // alone). Only a core that sleeps counts for a custom build, so no standby is observed here and the run goes on.
    let state = act(&mut session, "{\"action\":\"cold\"}");
    assert_eq!(named(&state), first, "cold");
    assert_eq!(session.system().board(Which::Main).unwrap().peek(0x4000_7010, Width::Word), Some(0));
    session.run_for(4.0);
    let state = session.state();
    let main = session.system().board(Which::Main).unwrap();
    assert_eq!((main.peek(0x4000_7000, Width::Word), main.peek(0xE000_ED10, Width::Word)), (Some(0x303), Some(4)), "the standby request is in the registers");
    assert_eq!((main.cpu.deep_sleep_entries(), main.cpu.is_sleeping()), (0, false), "but the core never slept with SLEEPDEEP");
    assert_eq!((state.get("standby"), state.get("running")), (Some(&Json::Bool(false)), Some(&Json::Bool(true))));
    assert_eq!(named(&state), first);
    // Wake restarts with the same images, to the same B1 prompt.
    let state = act(&mut session, "{\"action\":\"wake\"}");
    assert_eq!(state.get("standby"), Some(&Json::Bool(false)));
    assert_eq!(named(&state), first, "wake");
    session.run_for(6.5);
    assert_eq!(screen(&session), 0x29);
    // A machine reset of the handset (SYSRESETREQ) reboots from the same flash.
    {
        let board = session.system_mut().board_mut(Which::Handset).unwrap();
        let now = board.now();
        board.cpu.ppb_poke32(0xE000_ED0C, 0x05FA_0004, now);
    }
    session.run_for(0.01);
    assert_eq!(session.system().machine_reset_count(), 1);
    assert_eq!(named(&session.state()), first);
    session.run_for(1.0);
    assert!(session.system().instructions(Which::Handset).unwrap_or(0) > 1_000_000, "the handset runs again after the reset");
}

#[test]
fn a_custom_image_never_makes_a_pair_with_an_original_one() {
    let Some((custom_main, custom_handset)) = triton_as_custom() else {
        eprintln!("skipping: the firmware SREC files are not available");
        return;
    };
    let (main_srec, handset_srec) = srec_files().unwrap();
    let original_main = firmware::load(&main_srec, Some(Role::Main)).unwrap();
    let original_handset = firmware::load(&handset_srec, Some(Role::Handset)).unwrap();
    let error = Session::new(SessionConfig::default(), Some(&custom_main), &original_handset, Profile::default()).err().expect("refused");
    assert!(error.starts_with("Mixed firmware: the main image is a custom build but the handset image is TRITON-5.8-65.3"), "{error}");
    let error = Session::new(SessionConfig::default(), Some(&original_main), &custom_handset, Profile::default()).err().expect("refused");
    assert!(error.starts_with("Mixed firmware: the main image is TRITON-5.8-65.3") && error.contains("the handset image is a custom build"), "{error}");
    let error = System::new(SystemConfig::dual(), Some(&custom_main), &original_handset).err().expect("refused");
    assert!(error.starts_with("Mixed firmware"), "{error}");
    // Two custom builds, or two originals, are fine; so is a handset alone.
    assert!(Session::new(SessionConfig::default(), Some(&custom_main), &custom_handset, Profile::default()).is_ok());
    assert!(Session::new(SessionConfig::default(), Some(&original_main), &original_handset, Profile::default()).is_ok());
    let alone = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    assert!(Session::new(alone, None, &custom_handset, Profile::default()).is_ok());
    // The original release keeps every address and the EEPROM factory image.
    let original = Session::new(SessionConfig::default(), Some(&original_main), &original_handset, Profile::default()).unwrap().state();
    assert_eq!(original.get("firmware").and_then(|f| f.get("release")).and_then(|r| r.get("id")).and_then(Json::as_str), Some("TRITON-5.8-65.3"));
    assert!(original.get("firmware").and_then(|f| f.get("main")).and_then(|m| m.get("custom")).is_none(), "an original role object has no custom key");
    assert_eq!(original.get("eepromFactoryInit").and_then(|e| e.get("applied")), Some(&Json::Bool(true)));
    assert_eq!(original.get("unavailable").map(Json::len), Some(0));
}
