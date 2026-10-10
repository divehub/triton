//! The RTC calendar of a profile with the real TRITON firmware images (skipped when the gitignored SREC files are not available):
//! DESIGN.md section 23.
//!
//! What they establish (synthetic reproductions on the functional model, not physical observations):
//!
//! * the engine reads no host clock: a new profile (no `rtc-state.json`, no EEPROM date seed) starts both calendars at the RTC's
//!   2020-01-01 default (`fresh-rtc`), the state has no `rtcInit`, and the main board's `RTC.BKP1R` holds only what the boot fixture
//!   writes (0x32F0 in the default wake boot, nothing in a cold boot, where the original firmware then sets its build default date);
//! * a checkpoint with the provenance `host-local-time`, which the engine builds of 2026-10-10 wrote for new profiles before that
//!   feature was removed, still loads: it is restored like any other checkpoint, advances in virtual time, keeps its provenance when
//!   saved again and loads again after reopening, in the dual and the handset-only mode;
//! * an invalid checkpoint under that provenance still stops the start (it is never replaced silently).

use emu_core::{Json, Width};
use ngc::firmware::{self, Firmware, Role, TRITON};
use ngc::persistence::{Profile, Provenance, RtcState, BOARD_HANDSET, BOARD_MAIN};
use ngc::session::{Session, SessionConfig};
use ngc::system::{BootMode, Mode, Which};
use std::path::PathBuf;

const RTC_TR: u32 = 0x4000_2800;
const RTC_DR: u32 = 0x4000_2804;
const RTC_BKP1R: u32 = 0x4000_2854;

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

fn session(config: SessionConfig, profile: Profile, main: &Firmware, handset: &Firmware) -> Session {
    Session::new(config, Some(main), handset, profile).expect("session")
}

fn advance(session: &mut Session, seconds: f64) {
    let body = format!("{{\"action\":\"advance\",\"seconds\":{seconds}}}");
    session.action(&body).unwrap_or_else(|e| panic!("{body}: {e}"));
}

fn state(session: &Session) -> Json {
    Json::parse(&session.state_json()).expect("state json")
}

fn word(session: &Session, which: Which, address: u32) -> u32 {
    session.system().board(which).expect("board").peek(address, Width::Word).expect("peek")
}

/// The side-effect-free peek of the RTC `TR` and `DR` registers of a board (what the guest would read, without touching the shadow
/// registers).
fn registers(session: &Session, which: Which) -> (u32, u32) {
    (word(session, which, RTC_TR), word(session, which, RTC_DR))
}

/// `(year, month, day, hour, minute, second)` of BCD `TR`/`DR` values (24-hour format).
fn decode((tr, dr): (u32, u32)) -> (u32, u32, u32, u32, u32, u32) {
    let bcd = |value: u32| (value >> 4 & 15) * 10 + (value & 15);
    (2000 + bcd(dr >> 16 & 0xFF), bcd(dr >> 8 & 0x1F), bcd(dr & 0x3F), bcd(tr >> 16 & 0x3F), bcd(tr >> 8 & 0x7F), bcd(tr & 0x7F))
}

fn source_of(state: &Json, board: &str) -> String {
    let sources = state.get("rtcPersistence").and_then(|r| r.get("sources")).expect("rtcPersistence.sources");
    sources.get(board).and_then(|s| s.get("source")).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn restored(state: &Json) -> Vec<String> {
    let boards = state.get("rtcPersistence").and_then(|r| r.get("restoredBoards")).and_then(Json::as_array).expect("restoredBoards");
    boards.iter().filter_map(|b| b.as_str().map(str::to_string)).collect()
}

/// 2026-10-10 (a Saturday, ISO weekday 6) in the RTC `DR` layout.
const SATURDAY_DR: u32 = 0x26_0000 | 6 << 13 | 0x1000 | 0x10;
const MAIN_TR: u32 = 0x14_0322;
const HANDSET_TR: u32 = 0x14_0324;

/// One board of an `rtc-state.json` as the engine builds of 2026-10-10 wrote it for a new profile: the 24-hour calendar started from
/// the browser's local time, the live RTC's default prescaler and backup words (the main board's `RTC.BKP1R` as the wake fixture left
/// it), and the provenance `host-local-time`.
fn old_board(time: u32, bkp1: u32, extra_provenance: &str) -> String {
    let mut backup = [0u32; 20];
    backup[1] = bkp1;
    let words = backup.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "{{\"timeRegister\": {time}, \"dateRegister\": {SATURDAY_DR}, \"prescalerRegister\": {}, \"format12Hour\": false, \"backupRegisters\": [{words}], \"provenance\": {{\"source\": \"host-local-time\"{extra_provenance}}}}}",
        0x007F_00FFu32
    )
}

fn old_rtc_state(main_extra_provenance: &str) -> String {
    format!(
        "{{\"version\": 1, \"clockPolicy\": \"virtual-time-only\", \"precision\": \"whole-calendar-seconds\", \"boards\": {{\"{BOARD_MAIN}\": {}, \"{BOARD_HANDSET}\": {}}}}}\n",
        old_board(MAIN_TR, 0x32F0, main_extra_provenance),
        old_board(HANDSET_TR, 0, "")
    )
}

#[test]
fn a_new_profile_starts_both_calendars_at_the_rtc_default_and_reads_no_host_clock() {
    let (main, handset) = images_or_skip!();
    let mut wake = session(SessionConfig::default(), Profile::default(), &main, &handset);
    for which in [Which::Main, Which::Handset] {
        assert_eq!(decode(registers(&wake, which)), (2020, 1, 1, 0, 0, 0), "{which:?}");
    }
    let start = state(&wake);
    assert!(start.get("rtcInit").is_none(), "the state reports no host clock");
    assert_eq!((source_of(&start, BOARD_MAIN), source_of(&start, BOARD_HANDSET)), ("fresh-rtc".to_string(), "fresh-rtc".to_string()));
    assert!(restored(&start).is_empty());
    // The default wake boot: the wake fixture writes the main's RTC.BKP1R marker; the handset's backup words are untouched.
    assert_eq!((word(&wake, Which::Main, RTC_BKP1R), word(&wake, Which::Handset, RTC_BKP1R)), (0x32F0, 0));
    // The calendar advances in virtual time from the default and the original firmware keeps it.
    advance(&mut wake, 4.5);
    let (year, month, day, hour, minute, second) = decode(registers(&wake, Which::Main));
    assert_eq!((year, month, day, hour, minute), (2020, 1, 1, 0, 0));
    assert!((3..=5).contains(&second), "main second {second}");

    // A cold first boot supplies no wake flags and no marker: the main application's RTC initialization (BKP1R compared with
    // 0x32F0) then sets the firmware's build default date, 2022-12-06 12:48, as before the host's local time existed.
    let mut cold = session(SessionConfig { boot_mode: BootMode::Cold, ..SessionConfig::default() }, Profile::default(), &main, &handset);
    assert_eq!(word(&cold, Which::Main, RTC_BKP1R), 0);
    advance(&mut cold, 3.0);
    let (year, month, day, hour, minute, _) = decode(registers(&cold, Which::Main));
    assert_eq!((year, month, day, hour, minute), (2022, 12, 6, 12, 48));
    assert_eq!(state(&cold).get("error"), Some(&Json::Null));
}

#[test]
fn a_checkpoint_with_the_host_local_time_provenance_still_loads() {
    let (main, handset) = images_or_skip!();
    let text = old_rtc_state("");
    let profile = Profile { rtc_state: Some(text.clone()), ..Profile::default() };
    let mut first = session(SessionConfig::default(), profile, &main, &handset);
    // Restored before any guest runs, exactly as saved.
    assert_eq!(registers(&first, Which::Main), (MAIN_TR, SATURDAY_DR));
    assert_eq!(registers(&first, Which::Handset), (HANDSET_TR, SATURDAY_DR));
    let start = state(&first);
    assert_eq!(restored(&start), [BOARD_MAIN, BOARD_HANDSET]);
    assert_eq!((source_of(&start, BOARD_MAIN), source_of(&start, BOARD_HANDSET)), ("host-local-time".to_string(), "host-local-time".to_string()));
    assert!(start.get("rtcInit").is_none());
    assert_eq!(start.get("error"), Some(&Json::Null));

    // It advances in virtual time and the original firmware keeps it (the wake boot marks the main's clock as set).
    advance(&mut first, 4.5);
    let main_now = decode(registers(&first, Which::Main));
    assert_eq!((main_now.0, main_now.1, main_now.2, main_now.3, main_now.4), (2026, 10, 10, 14, 3), "{main_now:?}");
    assert!((25..=27).contains(&main_now.5), "main second {main_now:?}");
    let handset_now = decode(registers(&first, Which::Handset));
    assert_eq!((handset_now.0, handset_now.1, handset_now.2, handset_now.3, handset_now.4), (2026, 10, 10, 14, 3), "{handset_now:?}");
    assert!((27..=29).contains(&handset_now.5), "handset second {handset_now:?}");

    // Saved again on close: the strict format, the restored boards keep their provenance (as every restored board does), the
    // calendar moved on.
    let closed = first.shutdown();
    let saved_text = closed.rtc_state.clone().expect("rtc-state.json");
    let saved = RtcState::parse(&saved_text, "rtc-state.json").expect("strict parse");
    assert_eq!(saved.board(BOARD_MAIN).map(|b| b.provenance.clone()), Some(Provenance::HostLocalTime));
    assert_eq!(saved.board(BOARD_HANDSET).map(|b| b.provenance.clone()), Some(Provenance::HostLocalTime));
    let saved_main = saved.board(BOARD_MAIN).expect("main").checkpoint.clone();
    assert_ne!(saved_main.time_register, MAIN_TR, "the saved calendar advanced");

    // Reopened: restored again, continuing from the saved calendar.
    let reopened = session(SessionConfig::default(), closed, &main, &handset);
    assert_eq!(registers(&reopened, Which::Main), (saved_main.time_register, saved_main.date_register));
    assert_eq!(restored(&state(&reopened)).len(), 2);

    // The handset-only mode restores the handset board of the same file.
    let alone = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    let handset_only = Session::new(alone, None, &handset, Profile { rtc_state: Some(text), ..Profile::default() }).expect("handset only");
    assert_eq!(registers(&handset_only, Which::Handset), (HANDSET_TR, SATURDAY_DR));
    assert_eq!(source_of(&state(&handset_only), BOARD_HANDSET), "host-local-time");
}

#[test]
fn an_invalid_checkpoint_with_that_provenance_still_stops_the_start() {
    let (main, handset) = images_or_skip!();
    // The provenance carries nothing else, like the other plain sources: a stray field is refused, and the start stops.
    let stray = Profile { rtc_state: Some(old_rtc_state(", \"packedBackup\": 1")), ..Profile::default() };
    let error = Session::new(SessionConfig::default(), Some(&main), &handset, stray).err().expect("startup must fail");
    assert!(error.contains("Invalid RTC migration provenance for ngc-main"), "{error}");
    // An impossible calendar under it (2026-02-30) as well.
    let impossible = old_rtc_state("").replacen(&SATURDAY_DR.to_string(), &(0x26_0000u32 | 6 << 13 | 0x0200 | 0x30).to_string(), 1);
    let error = Session::new(SessionConfig::default(), Some(&main), &handset, Profile { rtc_state: Some(impossible), ..Profile::default() })
        .err()
        .expect("startup must fail");
    assert!(error.contains("RTC"), "{error}");
}
