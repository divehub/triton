//! The EEPROM factory image with the real TRITON firmware images (skipped when the gitignored SREC files are not available):
//! `crates/ngc/src/eeprom_init.rs`, DESIGN.md section 18, `docs/eeprom.md`.
//!
//! What they establish (synthetic reproductions on the functional model, not physical observations):
//!
//! * a **new** EEPROM (no `eeprom.bin`, or a stored image of 2048 bytes of `0xFF`) is created from the factory image once, and the
//!   image is saved with the profile; the original first-boot defaults (`0x08009fea`) still run exactly as before, and the records
//!   they never write receive the firmware-derived values: the oxygen-toxicity dose base makes the delta vital capacity finite
//!   (`0.00%` on the handset, a finite and rising CAN `0x226`), the toxicity model makes the main send CAN `0x225`, the serial reads 1,
//!   the no-fly time is 0, and the tissues are finite after a Restart;
//! * an **existing** ("dirty") EEPROM is never touched, even when inventoried records in it are still erased; the state says so, and
//!   an older profile with blank tissues still loads NaN, which `decoHealth` reports (there is no repair);
//! * the record table the image relies on is the verified 568-byte table, and the records the first boot leaves erased after the
//!   image are exactly the ones `docs/eeprom.md` lists as left erased.
//!
//! The slow-tier contrast (`delta_vc_is_nan_without_the_factory_image`, `--ignored`) shows the NaN a blank EEPROM leaves.

use emu_core::{Json, Width};
use ngc::deco::{EEPROM_BYTES, TISSUE_HE_OFFSET, TISSUE_N2_OFFSET, TISSUE_RECORDS, TISSUE_STRIDE};
use ngc::eeprom_init::{factory_image, record_table, RECORD_TABLE_BYTES, RECORD_TABLE_SHA256};
use ngc::firmware::{self, Firmware, Role, TRITON};
use ngc::session::{Profile, Session, SessionConfig};
use ngc::system::Which;
use std::path::PathBuf;

/// Main RAM (TRITON): the raw no-decompression limit and the first tissue record.
const RAW_NDL: u32 = 0x2000_2108;
const TISSUE_RAM: u32 = 0x2000_1E94;
/// Handset RAM (TRITON): the TouchGFX text buffer of the delta-vital-capacity value (UTF-16, NUL terminated), valid while the
/// handset shows the page with the oxygen-toxicity values.
const HANDSET_DELTA_VC_TEXT: u32 = 0x2000_6424;
/// About 35 m of EN13319 water on 1013.25 mbar, and 20 m.
const DEEP_MBAR: f64 = 4600.0;
const TWENTY_METERS_MBAR: f64 = 3013.3;
const SURFACE_MBAR: f64 = 1013.25;

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

/// A session with the factory image switched off through the internal field: what a blank EEPROM (an older build's) looks like.
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

fn set_pressure(session: &mut Session, mbar: f64) {
    act(session, &format!("{{\"action\":\"inputs\",\"inputs\":{{\"pressure1Mbar\":{mbar},\"pressure2Mbar\":{mbar}}}}}"));
}

fn peek(session: &Session, which: Which, address: u32) -> u32 {
    session.system().board(which).expect("board").peek(address, Width::Word).expect("RAM")
}

fn eeprom(session: &Session) -> [u8; EEPROM_BYTES] {
    session.system().main.as_ref().expect("main board").eeprom.image()
}

fn health(state: &Json, member: &str) -> String {
    state.get("decoHealth").and_then(|h| h.get(member)).and_then(Json::as_str).unwrap_or_default().to_string()
}

fn factory(state: &Json) -> Json {
    state.get("eepromFactoryInit").cloned().expect("eepromFactoryInit")
}

fn applied(report: &Json) -> bool {
    matches!(report.get("applied"), Some(Json::Bool(true)))
}

fn reason(report: &Json) -> String {
    report.get("reason").and_then(Json::as_str).unwrap_or_default().to_string()
}

/// The air calibration the handset's menu drives, sent as the same CAN commands (see `deco_fixtures.rs`).
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

/// The battery wizard as the scenarios drive it (see `battery_default.rs`): Li-Ion 3.7V-18650 (three Downs) for B1 and B2, which
/// fits the 4100 mV of a fresh profile, so the next start does not ask for a battery change.
fn battery_wizard_li_ion(session: &mut Session) {
    advance(session, 6.5);
    for _bank in 0..2 {
        for action in ["down", "down", "down", "confirm", "confirm"] {
            act(session, &format!("{{\"action\":\"{action}\"}}"));
            advance(session, 0.65);
        }
    }
}

/// The payloads of the frames `id` the main board sent, in order.
fn can_frames(session: &Session, id: &str) -> Vec<Vec<u8>> {
    let needle = format!("\t{id}\t");
    session
        .system()
        .link
        .trace_text()
        .lines()
        .filter(|line| line.contains("ngc-main") && line.contains(&needle))
        .map(|line| {
            let hex = line.split('\t').nth(3).unwrap_or_default();
            (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect()
        })
        .collect()
}

fn f32_of(frame: &[u8]) -> f32 {
    f32::from_le_bytes(frame[..4].try_into().expect("four bytes"))
}

/// The handset's text buffer of the delta-vital-capacity value, as text.
fn delta_vc_text(session: &Session) -> String {
    let mut text = String::new();
    for word in 0..8 {
        let value = peek(session, Which::Handset, HANDSET_DELTA_VC_TEXT + 4 * word);
        for half in [value & 0xFFFF, value >> 16] {
            if half == 0 {
                return text;
            }
            text.push(char::from_u32(half).unwrap_or('?'));
        }
    }
    text
}

/// Presses Down every three seconds (the first presses acknowledge the dive notifications, the next ones turn the pages) until the
/// handset's value buffer holds a delta-vital-capacity text: `0.00` or `nan`. That buffer is shared by the pages, which put other text
/// in it (`15` for the NDL, dates), so finding one of the two texts there means the page with the oxygen-toxicity values is on screen.
/// Returns the text.
fn open_toxicity_page(session: &mut Session) -> String {
    for _ in 0..10 {
        act(session, "{\"action\":\"down\"}");
        advance(session, 3.0);
        let text = delta_vc_text(session);
        if text == "0.00" || text == "nan" {
            return text;
        }
    }
    panic!("the toxicity page did not appear; the value buffer holds {:?}", delta_vc_text(session));
}

/// The whole first-use flow on a fresh profile: wizard, calibration through the firmware's protocol, a 20 m dive, the toxicity page.
/// Returns the session and the text of the delta-vital-capacity value the handset shows there.
fn dive_to_the_toxicity_page(config: SessionConfig, main: &Firmware, handset: &Firmware) -> (Session, String) {
    let mut session = Session::new(config, Some(main), handset, Profile::default()).expect("dual session");
    battery_wizard_li_ion(&mut session);
    advance(&mut session, 2.0);
    calibrate_oxygen(&mut session);
    set_pressure(&mut session, TWENTY_METERS_MBAR);
    advance(&mut session, 18.0);
    let text = open_toxicity_page(&mut session);
    (session, text)
}

// ---- a new EEPROM ------------------------------------------------------------------------------------------------------------

#[test]
fn a_new_profile_is_initialized_once_and_the_firmware_defaults_still_run_unchanged() {
    let (main, handset) = images_or_skip!();
    let mut on = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let mut off = Session::new(blank(), Some(&main), &handset, Profile::default()).expect("session");

    // This session created its EEPROM from the factory image: the report is there from the first board creation, and the image is what
    // the profile saves (the validity marker is still erased: the firmware's first-boot defaults have not run yet).
    let created = factory(&state(&on));
    assert!(applied(&created) && reason(&created).starts_with("This session created the EEPROM"), "{created:?}");
    assert_eq!(created.len(), 2, "the report is just {{applied, reason}}");
    let saved = on.take_profile_changes().and_then(|changes| changes.eeprom).expect("the EEPROM is saved at the start");
    assert_eq!(saved, factory_image(), "the profile holds the factory image");
    assert_eq!(saved[254], 0xFF, "the marker is the firmware's to write");
    // Switched off internally (the recorded scenarios), a new EEPROM stays erased as before.
    let blank_report = factory(&state(&off));
    assert!(!applied(&blank_report) && reason(&blank_report).starts_with("Not applied: switched off"), "{blank_report:?}");
    assert!(eeprom(&off).iter().all(|&b| b == 0xFF), "switched off: a fresh EEPROM starts erased as before");

    advance(&mut on, 10.0);
    advance(&mut off, 10.0);
    let (with, without) = (eeprom(&on), eeprom(&off));

    // The inventoried records hold the values; the serial reads 1 in the state and the page's field.
    assert_eq!(&with[0..4], &[1, 0, 0, 0]);
    assert_eq!(state(&on).get("serialNumber").and_then(Json::as_u64), Some(1));
    assert_eq!(with[0x0AF], 0, "the toxicity model");
    assert_eq!(&with[0x0B8..0x0BF], &[0, 0, 0, 0, 0, 0, 110], "dose base 0.0, ESOT 0, last ppO2 110");
    assert_eq!(&with[0x183..0x187], &[0; 4]);
    assert_eq!(&with[0x190..0x194], &[0; 4]);
    for record in 0..TISSUE_RECORDS as usize {
        let at = 0x0FF + 8 * record;
        assert_eq!(&with[at..at + 8], &[0x4D, 0x30, 0x40, 0x3F, 0, 0, 0, 0], "tissue record {record}");
    }
    assert_eq!(without[0..4], [0xFF; 4], "without the image the serial is erased: 4294967295");

    // The firmware's first-boot defaults ran exactly as before: wherever the blank run holds a byte, the image run holds the same one
    // (the validity marker, every default the routine writes, the dates), and every other byte differs only inside the inventory.
    assert_eq!(with[254], 0xA3);
    let inventoried = |offset: usize| offset < 4 || offset == 0x0AF || (0x0B8..0x0BF).contains(&offset) || (0x0FF..0x17F).contains(&offset) || (0x183..0x187).contains(&offset) || (0x190..0x194).contains(&offset);
    let mut programmed = 0;
    for offset in 0..EEPROM_BYTES {
        if without[offset] != 0xFF {
            programmed += 1;
            assert_eq!(with[offset], without[offset], "the default routine's byte 0x{offset:03x}");
        } else if !inventoried(offset) {
            assert_eq!(with[offset], 0xFF, "0x{offset:03x} is outside the inventory and stays erased");
        }
    }
    assert!(programmed > 150, "the firmware wrote its defaults ({programmed} bytes)");
    // The firmware's own RAM after its first-boot reset holds the very values the image stores (the tissue constant is derived there).
    for record in 0..TISSUE_RECORDS {
        let base = TISSUE_RAM + TISSUE_STRIDE * record;
        assert_eq!(peek(&on, Which::Main, base + TISSUE_N2_OFFSET), 0x3F40_304D, "N2 of tissue {record}");
        assert_eq!(peek(&on, Which::Main, base + TISSUE_HE_OFFSET), 0, "He of tissue {record}");
    }

    // The records the first boot leaves erased are exactly the ones `docs/eeprom.md` lists as left erased on purpose (logical record
    // IDs of the main image's table); without the image the inventory is erased as well.
    let left_erased: Vec<u32> = vec![0x0C, 0x13, 0x24, 0x25, 0x4D, 0x50, 0x54, 0x55, 0x56, 0x57, 0x5D, 0x5F, 0x60, 0x61];
    let table = record_table(&TRITON, &main).expect("the TRITON record table");
    let erased_records = |image: &[u8; EEPROM_BYTES]| -> Vec<u32> {
        (1..=0x8Du32)
            .filter(|&id| {
                let (offset, size) = (u16::from_le_bytes([table[4 * id as usize], table[4 * id as usize + 1]]) as usize, table[4 * id as usize + 2] as usize);
                image[offset..offset + size].iter().all(|&b| b == 0xFF)
            })
            .collect()
    };
    assert_eq!(erased_records(&with), left_erased, "the records still entirely erased after a first boot with the image");
    let mut everything = left_erased.clone();
    everything.extend([0x01, 0x2B, 0x67, 0x68, 0x69]);
    everything.extend(0x6A..=0x89);
    everything.extend([0x8B, 0x8D]);
    everything.sort_unstable();
    assert_eq!(erased_records(&without), everything, "the records a first boot leaves erased without the image");

    // Once: a Restart, a serial change and a reopening find the saved image and never apply it again; the session keeps saying that it
    // created the EEPROM, and what was stored stays.
    let after = act(&mut on, "{\"action\":\"serial\",\"serialNumber\":7}");
    assert_eq!(after.get("serialNumber").and_then(Json::as_u64), Some(7));
    assert_eq!(factory(&after), created, "the report of the session is kept across the board recreation");
    advance(&mut on, 3.0);
    assert_eq!(&eeprom(&on)[0..4], &[7, 0, 0, 0]);
    let again = act(&mut on, "{\"action\":\"reset\"}");
    assert_eq!(again.get("serialNumber").and_then(Json::as_u64), Some(7), "a Restart keeps the stored serial");
    assert_eq!(factory(&again), created);
    let closed = on.shutdown();
    let reopened = Session::new(SessionConfig::default(), Some(&main), &handset, closed.clone()).expect("reopen");
    assert_eq!(&eeprom(&reopened)[..], &closed.eeprom.expect("eeprom")[..], "a second opening changes nothing");
    let report = factory(&state(&reopened));
    assert!(!applied(&report) && reason(&report).starts_with("Not applied: the profile already holds an EEPROM"), "{report:?}");
}

#[test]
fn an_entirely_erased_stored_image_is_a_new_eeprom() {
    let (main, handset) = images_or_skip!();
    // The same result as a profile without eeprom.bin: the factory image, saved with the profile.
    let from_nothing = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let mut erased = Session::new(SessionConfig::default(), Some(&main), &handset, Profile { eeprom: Some(vec![0xFF; EEPROM_BYTES]), ..Profile::default() }).expect("session");
    assert_eq!(eeprom(&erased)[..], eeprom(&from_nothing)[..]);
    assert_eq!(eeprom(&erased)[..], factory_image()[..]);
    let report = factory(&state(&erased));
    assert!(applied(&report) && reason(&report).contains("entirely erased"), "{report:?}");
    assert_eq!(erased.take_profile_changes().and_then(|changes| changes.eeprom), Some(factory_image()), "and it is saved");
    // One stored byte anywhere makes it an existing EEPROM (see the next test); a wrong size is the loader's to refuse, untouched.
    let wrong_size = Profile { eeprom: Some(vec![0xFF; 100]), ..Profile::default() };
    assert!(Session::new(SessionConfig::default(), Some(&main), &handset, wrong_size).is_err());
}

// ---- an existing EEPROM -----------------------------------------------------------------------------------------------------

#[test]
fn an_existing_profile_is_never_touched_and_an_older_profile_with_blank_tissues_stays_invalid() {
    let (main, handset) = images_or_skip!();
    let open = |image: &[u8]| Session::new(SessionConfig::default(), Some(&main), &handset, Profile { eeprom: Some(image.to_vec()), ..Profile::default() }).expect("reopen");

    // Images that are not entirely erased, whatever they hold: one stored byte, an older build's calibrated profile, the factory image
    // with the serial erased, all zeros. Every inventoried record that is erased in them stays erased.
    let mut one_byte = vec![0xFF; EEPROM_BYTES];
    one_byte[EEPROM_BYTES - 1] = 0x00;
    let mut older = vec![0xFF; EEPROM_BYTES];
    older[0x38..0x3E].copy_from_slice(&[0xE8, 0x03, 0xE8, 0x03, 0xE8, 0x03]);
    older[0x3E..0x41].copy_from_slice(&[0x09, 0x09, 0x09]);
    older[254] = 0xA3;
    let mut no_serial = factory_image();
    no_serial[0..4].fill(0xFF);
    let mut stored_nan = older.clone();
    stored_nan[0x0B8..0x0BC].copy_from_slice(&f32::NAN.to_le_bytes()); // a stored NaN is data, not erasure
    for (label, image) in [("one stored byte", one_byte), ("an older build's calibrated profile", older), ("the factory image without its serial", no_serial), ("a stored NaN", stored_nan), ("all zeros", vec![0x00; EEPROM_BYTES])] {
        let session = open(&image);
        assert_eq!(eeprom(&session)[..], image[..], "{label}: an existing EEPROM is loaded as it is");
        let report = factory(&state(&session));
        assert!(!applied(&report) && reason(&report).starts_with("Not applied: the profile already holds an EEPROM"), "{label}: {report:?}");
    }

    // A real older profile: a first boot without the image saved a decompression date but no tissues. Reopened with the engine's
    // default configuration it is loaded as it is, the firmware loads 32 NaN words (the original firmware's behavior) and the read-only
    // report says so; nothing repairs it.
    let mut first = Session::new(blank(), Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut first, 8.0);
    let saved = first.shutdown();
    let image = saved.eeprom.clone().expect("eeprom");
    assert!(image[0x0FF..0x17F].iter().all(|&b| b == 0xFF) && image[0x17F..0x183].iter().any(|&b| b != 0xFF) && image[0..4] == [0xFF; 4], "a saved date, no tissues, no serial");
    let mut old = Session::new(SessionConfig::default(), Some(&main), &handset, saved).expect("reopen");
    assert_eq!(eeprom(&old)[..], image[..], "the older profile is loaded as it is");
    assert!(!applied(&factory(&state(&old))));
    let booted = advance(&mut old, 4.0);
    assert_eq!(health(&booted, "tissues"), "invalid", "{:?}: the blank tissues load as NaN and the report says so", booted.get("decoHealth"));
    assert!(booted.get("decoStorageFixture").is_none(), "there is no repair fixture any more");
}

// ---- what the new EEPROM does ---------------------------------------------------------------------------------------------------

#[test]
fn delta_vc_is_finite_and_rising_after_a_dive_on_a_new_profile() {
    let (main, handset) = images_or_skip!();
    let (mut session, text) = dive_to_the_toxicity_page(SessionConfig::default(), &main, &handset);
    let frames = can_frames(&session, "0x226");
    let first = f32_of(frames.last().expect("a frame of 0x226"));
    // The handset's value widget (what the LCD draws, `ΔvC0.00%`) and the CAN frame the main sends for it.
    assert_eq!(text, "0.00", "the handset prints 0.00 (percent) at first");
    assert!(first.is_finite() && first >= 0.0, "{first}");
    advance(&mut session, 25.0);
    let later = f32_of(can_frames(&session, "0x226").last().expect("frame"));
    assert!(later.is_finite() && later > first, "the value rises with the dive, {first:e} then {later:e}");
    // The toxicity model takes its inventoried value: the main sends CAN 0x225 with model 0 (OTU/UPTD).
    let model = can_frames(&session, "0x225");
    assert!(!model.is_empty() && model.iter().all(|f| f.len() == 3 && f[0] == 0), "{model:?}");
    assert_eq!(eeprom(&session)[0x0AF], 0);
}

/// The contrast of the test above, kept out of the quick loop: with a blank EEPROM the erased dose base is NaN.
#[test]
#[ignore = "slow: a second wizard, calibration and 20 m dive on a blank EEPROM (about 5 s); run with --ignored"]
fn delta_vc_is_nan_without_the_factory_image() {
    let (main, handset) = images_or_skip!();
    let (session, text) = dive_to_the_toxicity_page(blank(), &main, &handset);
    let first = f32_of(can_frames(&session, "0x226").last().expect("a frame of 0x226"));
    assert_eq!(text, "nan", "the handset prints nan (the LCD shows ?a?%)");
    assert!(first.is_nan(), "the erased dose base is NaN: {first}");
    assert!(can_frames(&session, "0x225").is_empty(), "an erased model (0xff) means no CAN 0x225");
    assert_eq!(eeprom(&session)[0x0AF], 0xFF);
}

#[test]
fn a_restart_after_a_new_eeprom_loads_finite_tissues_and_an_ndl_below_99() {
    let (main, handset) = images_or_skip!();
    // The start at the surface is off, so the test moves the depth itself; nothing else changes the tissues.
    let config = SessionConfig { start_at_surface: false, ..SessionConfig::default() };
    let mut session = Session::new(config, Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut session, 8.0);
    calibrate_oxygen(&mut session);
    let first = state(&session);
    assert_eq!((health(&first, "tissues"), health(&first, "oxygen")), ("valid".to_string(), "calibrated".to_string()), "the first boot reset its tissues itself");
    // Back at the surface, then the Restart: the saved profile holds a date and, from the first boot, a tissue block of surface values
    // (the firmware saves its own only on power-down).
    set_pressure(&mut session, DEEP_MBAR);
    advance(&mut session, 3.0);
    set_pressure(&mut session, SURFACE_MBAR);
    advance(&mut session, 1.0);
    let restarted = act(&mut session, "{\"action\":\"reset\"}");
    assert!(applied(&factory(&restarted)), "the session still says it created the EEPROM");
    advance(&mut session, 4.0);
    assert_eq!(health(&state(&session), "oxygen"), "calibrated");
    assert_eq!(peek(&session, Which::Main, TISSUE_RAM + TISSUE_N2_OFFSET), 0x3F40_304D, "the stored surface words were loaded");
    set_pressure(&mut session, DEEP_MBAR);
    let mut ndl = peek(&session, Which::Main, RAW_NDL) as i32;
    for _ in 0..12 {
        advance(&mut session, 2.5);
        ndl = peek(&session, Which::Main, RAW_NDL) as i32;
        if ndl < 99 {
            break;
        }
    }
    let deep = state(&session);
    assert_eq!(health(&deep, "tissues"), "valid", "{:?}", deep.get("decoHealth"));
    assert!((1..99).contains(&ndl), "the no-decompression limit is below 99 at depth, was {ndl}");
}

#[test]
fn the_stored_tissue_block_is_n2_then_he_per_record() {
    let (main, handset) = images_or_skip!();
    // A first boot without the image, then a stored block of recognizable words (N2 1.0, He 0.25) next to the date it saved.
    let mut first = Session::new(blank(), Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut first, 8.0);
    let mut profile = first.shutdown();
    let mut image = profile.eeprom.take().expect("eeprom").to_vec();
    assert!(image[0x0FF..0x17F].iter().all(|&b| b == 0xFF) && image[0x17F..0x183].iter().any(|&b| b != 0xFF), "a saved date and no tissues");
    for record in 0..16 {
        image[0x0FF + 8 * record..0x0FF + 8 * record + 4].copy_from_slice(&1.0f32.to_le_bytes());
        image[0x0FF + 8 * record + 4..0x0FF + 8 * record + 8].copy_from_slice(&0.25f32.to_le_bytes());
    }
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile { eeprom: Some(image.clone()), ..profile.clone() }).expect("session");
    assert_eq!(&eeprom(&session)[..], &image[..], "an existing EEPROM, the stored block and the date next to it included, is never touched");
    advance(&mut session, 5.0);
    for record in 0..TISSUE_RECORDS {
        let base = TISSUE_RAM + TISSUE_STRIDE * record;
        // The loaded values start the firmware's own tissue update, which has pulled them a little towards the surface (N2 0.7507 bar, no
        // He) by now: N2 stays near 1.0 and He near 0.25, which also proves the word order (a swapped block would read the other way round).
        let n2 = f32::from_bits(peek(&session, Which::Main, base + TISSUE_N2_OFFSET));
        let he = f32::from_bits(peek(&session, Which::Main, base + TISSUE_HE_OFFSET));
        assert!(n2 > 0.95 && n2 <= 1.0, "N2 of tissue {record}: {n2}");
        assert!(he > 0.2 && he <= 0.25, "He of tissue {record}: {he}");
    }
}

// ---- the record table and the handset-only run ---------------------------------------------------------------------------------

#[test]
fn the_verified_record_table_matches_the_inventory() {
    let (main, _) = images_or_skip!();
    let table = record_table(&TRITON, &main).expect("the table is inside the image");
    assert_eq!(table.len(), RECORD_TABLE_BYTES);
    assert_eq!(ngc::sha256::digest_hex(table), RECORD_TABLE_SHA256);
    let entry = |id: usize| (u16::from_le_bytes([table[4 * id], table[4 * id + 1]]) as usize, table[4 * id + 2] as usize);
    // The inventory (physical offset and size per logical ID) is what the image's table says.
    for (id, offset, size) in [(0x01, 0x000, 4), (0x2B, 0x0AF, 1), (0x67, 0x0B8, 4), (0x68, 0x0BC, 2), (0x69, 0x0BE, 1), (0x8B, 0x183, 4), (0x8D, 0x190, 4)] {
        assert_eq!(entry(id), (offset, size), "record 0x{id:02x}");
    }
    for id in 0x6Ausize..=0x89 {
        assert_eq!(entry(id), (0x0FF + 4 * (id - 0x6A), 4), "tissue record 0x{id:02x}");
    }
    // The validity marker the default routine reads is record 0x63 at offset 254, which is not part of the inventory.
    assert_eq!(entry(0x63), (254, 1));
}

#[test]
fn a_handset_only_run_has_no_eeprom_and_reports_that() {
    let (_, handset) = images_or_skip!();
    let config = SessionConfig { mode: ngc::system::Mode::HandsetOnly, ..SessionConfig::default() };
    let session = Session::new(config, None, &handset, Profile::default()).expect("handset-only session");
    let report = factory(&state(&session));
    assert!(!applied(&report) && reason(&report).starts_with("Not applicable"), "{report:?}");
}
