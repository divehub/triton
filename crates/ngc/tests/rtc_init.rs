//! The clock of a new profile with the real TRITON firmware images (skipped when the gitignored SREC files are not available):
//! `crates/ngc/src/rtc_init.rs`, DESIGN.md section 23.
//!
//! What they establish (synthetic reproductions on the functional model, not physical observations):
//!
//! * a new profile (no `rtc-state.json`, no EEPROM date seed) given an `initial_local_time` starts the calendar of both boards from it
//!   before any guest runs: the side-effect-free RTC register peek (`TR`, `DR`) reads that date and time in the 24-hour format with the
//!   right weekday, the prescaler and backup words stay those of the live RTC, the provenance is `host-local-time`, and the state names
//!   it (`rtcInit`); afterwards the calendar advances in virtual time and the original firmware does not overwrite it;
//! * the checkpoint is saved with the profile (the provenance round-trips through `rtc-state.json`), a reopened profile and a Restart
//!   keep it and never apply a new local time on top of it;
//! * a saved checkpoint of one board and the legacy EEPROM date seed of the main board win over the host time, which only takes the
//!   boards they leave out;
//! * without a host time a fresh RTC keeps its 2020-01-01 default and the state says the host supplied none.

use emu_core::{Json, Width};
use ngc::firmware::{self, Firmware, Role, TRITON};
use ngc::persistence::{Profile, Provenance, RtcBoard, RtcState, BOARD_HANDSET, BOARD_MAIN};
use ngc::rtc_init::LocalTime;
use ngc::session::{Session, SessionConfig};
use ngc::system::{Mode, Which};
use std::path::PathBuf;
use stm32::rtc::RtcCheckpoint;

const RTC_TR: u32 = 0x4000_2800;
const RTC_DR: u32 = 0x4000_2804;

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

fn local(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> LocalTime {
    LocalTime::new(year, month, day, hour, minute, second).expect("a valid local time")
}

/// 2026-10-10 14:03:22, a Saturday.
fn saturday() -> LocalTime {
    local(2026, 10, 10, 14, 3, 22)
}

fn config(time: Option<LocalTime>) -> SessionConfig {
    SessionConfig { initial_local_time: time, ..SessionConfig::default() }
}

fn session(config: SessionConfig, profile: Profile, main: &Firmware, handset: &Firmware) -> Session {
    Session::new(config, Some(main), handset, profile).expect("session")
}

fn act(session: &mut Session, body: &str) -> Json {
    Json::parse(&session.action(body).unwrap_or_else(|e| panic!("{body}: {e}"))).expect("state json")
}

fn advance(session: &mut Session, seconds: f64) {
    let mut remaining = seconds;
    while remaining > 1e-9 {
        let chunk = remaining.min(20.0);
        act(session, &format!("{{\"action\":\"advance\",\"seconds\":{chunk}}}"));
        remaining -= chunk;
    }
}

fn state(session: &Session) -> Json {
    Json::parse(&session.state_json()).expect("state json")
}

/// The side-effect-free peek of the RTC `TR` and `DR` registers of a board (what the guest would read, without touching the shadow
/// registers).
fn registers(session: &Session, which: Which) -> (u32, u32) {
    let board = session.system().board(which).expect("board");
    (board.peek(RTC_TR, Width::Word).expect("TR"), board.peek(RTC_DR, Width::Word).expect("DR"))
}

/// `(year, month, day, ISO weekday, hour, minute, second)` of BCD `TR`/`DR` values (24-hour format).
fn decode((tr, dr): (u32, u32)) -> (u32, u32, u32, u32, u32, u32, u32) {
    let bcd = |value: u32| (value >> 4 & 15) * 10 + (value & 15);
    (2000 + bcd(dr >> 16 & 0xFF), bcd(dr >> 8 & 0x1F), bcd(dr & 0x3F), dr >> 13 & 7, bcd(tr >> 16 & 0x3F), bcd(tr >> 8 & 0x7F), bcd(tr & 0x7F))
}

fn rtc_init(state: &Json) -> Json {
    state.get("rtcInit").cloned().expect("rtcInit")
}

fn flag(report: &Json, name: &str) -> bool {
    matches!(report.get(name), Some(Json::Bool(true)))
}

fn text(report: &Json, name: &str) -> String {
    report.get(name).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn sources(state: &Json) -> Json {
    state.get("rtcPersistence").and_then(|r| r.get("sources")).cloned().expect("rtcPersistence.sources")
}

fn source_of(state: &Json, board: &str) -> String {
    sources(state).get(board).and_then(|s| s.get("source")).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn boards(report: &Json) -> Vec<String> {
    report.get("boards").and_then(Json::as_array).map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

#[test]
fn a_new_profile_starts_both_calendars_from_the_host_local_time_before_any_guest_runs() {
    let (main, handset) = images_or_skip!();
    let mut session = session(config(Some(saturday())), Profile::default(), &main, &handset);
    // Virtual time 0: the registers read exactly the supplied date and time, 24-hour format, ISO weekday 6 (Saturday).
    for which in [Which::Main, Which::Handset] {
        assert_eq!(registers(&session, which), (0x14_0322, 0x26_0000 | 6 << 13 | 0x1000 | 0x10), "{which:?}");
        assert_eq!(decode(registers(&session, which)), (2026, 10, 10, 6, 14, 3, 22));
        let checkpoint = session.system().rtc_checkpoint(which).expect("RTC");
        assert!(!checkpoint.format_12_hour);
        assert!(checkpoint.validate().is_ok(), "the seed is a valid checkpoint of the rtc-state.json rules");
    }
    let start = state(&session);
    let report = rtc_init(&start);
    assert!(flag(&report, "applied"), "{report:?}");
    assert_eq!(text(&report, "localTime"), "2026-10-10T14:03:22");
    assert_eq!(boards(&report), [BOARD_MAIN, BOARD_HANDSET]);
    assert!(text(&report, "reason").contains("2026-10-10T14:03:22") && text(&report, "reason").contains("labeled fixture"), "{report:?}");
    assert_eq!(source_of(&start, BOARD_MAIN), "host-local-time");
    assert_eq!(source_of(&start, BOARD_HANDSET), "host-local-time");
    let restored = start.get("rtcPersistence").and_then(|r| r.get("restoredBoards")).and_then(Json::as_array).map(<[Json]>::len);
    assert_eq!(restored, Some(0), "a seed is not a restored checkpoint");
    // The EEPROM factory image is created in the same start, and the two fixtures do not disturb each other.
    assert!(flag(start.get("eepromFactoryInit").expect("eepromFactoryInit"), "applied"));

    // The calendar then advances in virtual time, and the original firmware does not replace it (the fresh-RTC scenario shows the main
    // board 4 s after its 2020-01-01 start at 4.5 s).
    advance(&mut session, 4.5);
    let main_now = decode(registers(&session, Which::Main));
    assert_eq!(&(main_now.0, main_now.1, main_now.2, main_now.3, main_now.4, main_now.5), &(2026, 10, 10, 6, 14, 3), "{main_now:?}");
    assert!((24..=27).contains(&main_now.6), "main second {main_now:?}");
    let handset_now = decode(registers(&session, Which::Handset));
    assert_eq!(&(handset_now.0, handset_now.1, handset_now.2, handset_now.3, handset_now.4, handset_now.5), &(2026, 10, 10, 6, 14, 3), "{handset_now:?}");
    assert!((22..=27).contains(&handset_now.6), "handset second {handset_now:?}");
    let running = state(&session);
    assert!(flag(&rtc_init(&running), "applied"), "the report stays");
    assert_eq!(running.get("error"), Some(&Json::Null));
}

#[test]
fn the_checkpoint_is_saved_with_the_profile_and_a_reopened_profile_or_a_restart_never_applies_a_new_time() {
    let (main, handset) = images_or_skip!();
    let mut first = session(config(Some(saturday())), Profile::default(), &main, &handset);
    advance(&mut first, 2.0);
    let before_restart = decode(registers(&first, Which::Main));
    // A Restart saves the checkpoint and finds it again: the calendar continues, nothing is applied a second time, the report of the
    // first launch stays.
    let after_restart = {
        act(&mut first, r#"{"action":"reset"}"#);
        decode(registers(&first, Which::Main))
    };
    assert_eq!((after_restart.0, after_restart.1, after_restart.2, after_restart.4, after_restart.5), (2026, 10, 10, 14, 3));
    assert!(after_restart.6 >= before_restart.6, "continues, never goes back to 22 s: {before_restart:?} {after_restart:?}");
    let state_after = state(&first);
    assert!(flag(&rtc_init(&state_after), "applied") && text(&rtc_init(&state_after), "localTime") == "2026-10-10T14:03:22", "{:?}", rtc_init(&state_after));
    assert_eq!(source_of(&state_after, BOARD_MAIN), "host-local-time", "the provenance of the saved board is kept");

    // The closed profile: rtc-state.json parses strictly, holds both boards with the new provenance and round-trips byte for byte.
    let profile = first.shutdown();
    let rtc_text = profile.rtc_state.clone().expect("rtc-state.json");
    assert!(rtc_text.contains("\"source\": \"host-local-time\""), "{rtc_text}");
    let parsed = RtcState::parse(&rtc_text, "rtc-state.json").expect("strict parse");
    assert_eq!(parsed.boards.len(), 2);
    assert_eq!(parsed.board(BOARD_MAIN).map(|b| b.provenance.clone()), Some(Provenance::HostLocalTime));
    assert_eq!(parsed.board(BOARD_HANDSET).map(|b| b.provenance.clone()), Some(Provenance::HostLocalTime));
    assert_eq!(parsed.to_file_text(), rtc_text);

    // Reopened with a different host time: the saved checkpoint wins and the state says so (the supplied value is still reported).
    let saved_main = parsed.board(BOARD_MAIN).expect("main").checkpoint.clone();
    let reopened = session(config(Some(local(2030, 1, 2, 3, 4, 5))), profile, &main, &handset);
    assert_eq!(registers(&reopened, Which::Main), (saved_main.time_register, saved_main.date_register));
    let reopened_state = state(&reopened);
    let report = rtc_init(&reopened_state);
    assert!(!flag(&report, "applied") && boards(&report).is_empty(), "{report:?}");
    assert_eq!(text(&report, "localTime"), "2030-01-02T03:04:05");
    assert!(text(&report, "reason").starts_with("Not applied: every board already had a saved RTC checkpoint"), "{report:?}");
    assert_eq!(source_of(&reopened_state, BOARD_MAIN), "host-local-time", "the saved provenance is read back and kept");
    let restored = reopened_state.get("rtcPersistence").and_then(|r| r.get("restoredBoards")).and_then(Json::as_array).map(<[Json]>::len);
    assert_eq!(restored, Some(2));
}

fn eeprom_with_date(packed: u32) -> Vec<u8> {
    let mut eeprom = vec![0xFF; 2048];
    eeprom[254] = 0xA3;
    eeprom[0x2D..0x31].copy_from_slice(&packed.to_le_bytes());
    eeprom
}

#[test]
fn the_eeprom_date_seed_and_a_saved_checkpoint_win_over_the_host_time_which_takes_only_the_boards_left() {
    let (main, handset) = images_or_skip!();
    // The legacy EEPROM calendar (2026-10-07 17:20:05, a Wednesday) without a checkpoint: the main board migrates from the EEPROM, the
    // handset has nothing and takes the host time.
    let packed = 26 | (10 << 6) | (7 << 10) | (17 << 15) | (20 << 20) | (5 << 26);
    let profile = Profile { eeprom: Some(eeprom_with_date(packed)), ..Profile::default() };
    let seeded = session(config(Some(saturday())), profile, &main, &handset);
    assert_eq!(decode(registers(&seeded, Which::Main)), (2026, 10, 7, 3, 17, 20, 5), "the EEPROM seed wins for the main board");
    assert_eq!(decode(registers(&seeded, Which::Handset)), (2026, 10, 10, 6, 14, 3, 22), "the host time takes the handset");
    let seeded_state = state(&seeded);
    assert_eq!(source_of(&seeded_state, BOARD_MAIN), "eeprom-packed-date");
    assert_eq!(source_of(&seeded_state, BOARD_HANDSET), "host-local-time");
    let report = rtc_init(&seeded_state);
    assert!(flag(&report, "applied") && boards(&report) == [BOARD_HANDSET], "{report:?}");
    assert!(text(&report, "reason").contains("ngc-main kept"), "{report:?}");

    // A saved checkpoint of the handset alone (an older handset-only profile): the main board takes the host time, the handset keeps its own.
    let live = seeded.system().rtc_checkpoint(Which::Handset).expect("RTC");
    let mut old = RtcState::empty();
    old.set_board(
        BOARD_HANDSET,
        RtcBoard { checkpoint: RtcCheckpoint { time_register: 0x01_0203, date_register: 0x25_0000 | 1 << 13 | 0x0100 | 0x01, ..live }, provenance: Provenance::RtcRegisters },
    );
    let profile = Profile { rtc_state: Some(old.to_file_text()), ..Profile::default() };
    let mixed = session(config(Some(saturday())), profile, &main, &handset);
    assert_eq!(decode(registers(&mixed, Which::Handset)), (2025, 1, 1, 1, 1, 2, 3), "the saved handset checkpoint is untouched");
    assert_eq!(decode(registers(&mixed, Which::Main)), (2026, 10, 10, 6, 14, 3, 22));
    let mixed_state = state(&mixed);
    assert_eq!(boards(&rtc_init(&mixed_state)), [BOARD_MAIN]);
    assert_eq!(source_of(&mixed_state, BOARD_HANDSET), "rtc-registers");
}

#[test]
fn a_handset_only_run_starts_only_the_handset_and_without_a_host_time_nothing_changes() {
    let (main, handset) = images_or_skip!();
    let alone = SessionConfig { mode: Mode::HandsetOnly, initial_local_time: Some(saturday()), ..SessionConfig::default() };
    let session_alone = Session::new(alone, None, &handset, Profile::default()).expect("handset only");
    assert_eq!(decode(registers(&session_alone, Which::Handset)), (2026, 10, 10, 6, 14, 3, 22));
    assert_eq!(boards(&rtc_init(&state(&session_alone))), [BOARD_HANDSET]);

    // No host time: the fresh RTC keeps its 2020-01-01 default calendar and the state says the host supplied none.
    let plain = session(config(None), Profile::default(), &main, &handset);
    let (year, month, day, ..) = decode(registers(&plain, Which::Main));
    assert_eq!((year, month, day), (2020, 1, 1));
    let plain_state = state(&plain);
    let report = rtc_init(&plain_state);
    assert!(!flag(&report, "applied") && report.get("localTime").is_some_and(Json::is_null) && boards(&report).is_empty(), "{report:?}");
    assert!(text(&report, "reason").starts_with("Not applied: the host supplied no local time"), "{report:?}");
    assert_eq!(source_of(&plain_state, BOARD_MAIN), "fresh-rtc");
}
