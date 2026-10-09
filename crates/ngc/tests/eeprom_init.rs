//! The EEPROM factory-init fixture with the real firmware images (skipped when the gitignored SREC files are not available):
//! `crates/ngc/src/eeprom_init.rs`, DESIGN.md section 18, `docs/eeprom.md`.
//!
//! What they establish (synthetic reproductions on the functional model, not physical observations):
//!
//! * a fresh profile starts from an erased EEPROM; the original first-boot defaults (`0x08009fea`) still run exactly as before,
//!   and the records they never write receive the firmware-derived values: the oxygen-toxicity dose base makes the delta vital
//!   capacity finite (`0.00%` on the handset, a finite and rising CAN `0x226`), the toxicity model makes the main send CAN
//!   `0x225`, the serial reads 1, the no-fly time is 0, and the tissues are finite after a Restart with the repair fixture off;
//! * an existing profile only has its still-erased inventoried records filled (never a stored byte, never the calibration);
//! * switched off, the fixture reproduces the old NaN states;
//! * the record table the fixture relies on is the same 568 bytes in both releases, and the records the first boot leaves erased
//!   after the fixture are exactly the ones `docs/eeprom.md` lists as left erased.

use emu_core::{Json, Width};
use ngc::deco::{EEPROM_BYTES, TISSUE_HE_OFFSET, TISSUE_N2_OFFSET, TISSUE_RECORDS, TISSUE_STRIDE};
use ngc::eeprom_init::{record_table, RECORD_TABLE_BYTES, RECORD_TABLE_SHA256};
use ngc::firmware::{self, Firmware, Release, Role, NEPTUN, TRITON};
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

fn images(release: &Release) -> Option<(Firmware, Firmware)> {
    // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    let dir = roots
        .into_iter()
        .flatten()
        .map(|root| root.join(release.id))
        .find(|d| d.join(release.main.file_name).is_file() && d.join(release.handset.file_name).is_file())?;
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

fn flag(json: &Json, key: &str) -> bool {
    matches!(json.get(key), Some(Json::Bool(true)))
}

fn record_ids(report: &Json) -> Vec<String> {
    report.get("records").and_then(Json::as_array).expect("records").iter().map(|r| r.get("id").and_then(Json::as_str).unwrap_or_default().to_string()).collect()
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

// ---- the first boot ------------------------------------------------------------------------------------------------------

#[test]
fn a_fresh_profile_gets_the_inventory_and_the_firmware_defaults_still_run_unchanged() {
    let (main, handset) = images_or_skip!(&TRITON);
    let mut on = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let mut off = Session::new(SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() }, Some(&main), &handset, Profile::default()).expect("session");
    // Before the firmware runs the image is the fixture's: the report is there from the first board creation.
    let created = factory(&state(&on));
    assert!(flag(&created, "enabled") && flag(&created, "applied"), "{created:?}");
    assert_eq!(record_ids(&created), ["0x01", "0x2b", "0x67", "0x68", "0x69", "0x6a..0x89", "0x8b", "0x8d"]);
    assert_eq!(factory(&state(&off)).get("applied"), Some(&Json::Bool(false)));
    assert!(factory(&state(&off)).get("reason").and_then(Json::as_str).unwrap_or_default().starts_with("Switched off"));
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
    assert_eq!(without[0..4], [0xFF; 4], "switched off the serial is erased: 4294967295");

    // The firmware's first-boot defaults ran exactly as before: wherever the switched-off run holds a byte, the switched-on run
    // holds the same one (the validity marker, every default the routine writes, the dates), and every other byte differs only
    // inside the inventoried records.
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
    // The firmware's own RAM after its first-boot reset holds the very values the fixture stores (the tissue constant is derived there).
    for record in 0..TISSUE_RECORDS {
        let base = TISSUE_RAM + TISSUE_STRIDE * record;
        assert_eq!(peek(&on, Which::Main, base + TISSUE_N2_OFFSET), 0x3F40_304D, "N2 of tissue {record}");
        assert_eq!(peek(&on, Which::Main, base + TISSUE_HE_OFFSET), 0, "He of tissue {record}");
    }
}

#[test]
fn a_fresh_start_asks_for_the_filled_image_to_be_saved_and_the_marker_is_still_erased_until_the_firmware_runs() {
    let (main, handset) = images_or_skip!(&TRITON);
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    let created = session.take_profile_changes().expect("the files created by the launch");
    let image = created.eeprom.expect("the EEPROM image is saved at the start");
    assert_eq!(image.len(), EEPROM_BYTES);
    assert_eq!(&image[0..4], &[1, 0, 0, 0], "the filled image, not an erased one");
    assert_eq!(image[254], 0xFF, "the validity marker is the firmware's to write: its first-boot defaults have not run yet");
    // The same image is what a clean close leaves behind, with the firmware's defaults on top once it ran.
    advance(&mut session, 4.0);
    let closed = session.shutdown().eeprom.expect("eeprom");
    assert_eq!(closed[254], 0xA3);
    assert_eq!(&closed[0..4], &[1, 0, 0, 0]);
}

#[test]
fn the_records_the_first_boot_leaves_erased_are_the_ones_the_inventory_documents() {
    let (main, handset) = images_or_skip!(&TRITON);
    // The set `docs/eeprom.md` lists as left erased on purpose (logical record IDs of the main image's table).
    let left_erased: Vec<u32> = vec![0x0C, 0x13, 0x24, 0x25, 0x4D, 0x50, 0x54, 0x55, 0x56, 0x57, 0x5D, 0x5F, 0x60, 0x61];
    let table = record_table(&TRITON, &main).expect("the TRITON record table");
    let entry = |id: usize| (u16::from_le_bytes([table[4 * id], table[4 * id + 1]]) as usize, table[4 * id + 2] as usize);
    for (config, expect_filled) in [(SessionConfig::default(), true), (SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() }, false)] {
        let mut session = Session::new(config, Some(&main), &handset, Profile::default()).expect("session");
        advance(&mut session, 10.0);
        let image = eeprom(&session);
        let erased: Vec<u32> = (1..=0x8Du32)
            .filter(|&id| {
                let (offset, size) = entry(id as usize);
                image[offset..offset + size].iter().all(|&b| b == 0xFF)
            })
            .collect();
        if expect_filled {
            assert_eq!(erased, left_erased, "the records still entirely erased after a first boot with the fixture");
        } else {
            let mut everything = left_erased.clone();
            everything.extend([0x01, 0x2B, 0x67, 0x68, 0x69]);
            everything.extend(0x6A..=0x89);
            everything.extend([0x8B, 0x8D]);
            everything.sort_unstable();
            assert_eq!(erased, everything, "the records a first boot leaves erased without the fixture");
        }
    }
}

// ---- what the filled records do ---------------------------------------------------------------------------------------------

#[test]
fn delta_vc_is_finite_and_rising_after_a_dive_on_a_fresh_profile_and_nan_without_the_fixture() {
    let (main, handset) = images_or_skip!(&TRITON);
    for fixture in [true, false] {
        let label = if fixture { "fixture on (the default)" } else { "fixture off" };
        let config = SessionConfig { eeprom_factory_init: fixture, ..SessionConfig::default() };
        let (mut session, text) = dive_to_the_toxicity_page(config, &main, &handset);
        let frames = can_frames(&session, "0x226");
        let first = f32_of(frames.last().expect("a frame of 0x226"));
        println!("{label}: CAN 0x226 {first:e}, handset text {text:?}");
        if fixture {
            // The handset's value widget (what the LCD draws, `ΔvC0.00%`) and the CAN frame the main sends for it.
            assert_eq!(text, "0.00", "{label}: the handset prints 0.00 (percent) at first");
            assert!(first.is_finite() && first >= 0.0, "{label}: {first}");
            advance(&mut session, 25.0);
            let later = f32_of(can_frames(&session, "0x226").last().expect("frame"));
            assert!(later.is_finite() && later > first, "{label}: the value rises with the dive, {first:e} then {later:e}");
            // The toxicity model takes its inventoried value: the main sends CAN 0x225 with model 0 (OTU/UPTD).
            let model = can_frames(&session, "0x225");
            assert!(!model.is_empty() && model.iter().all(|f| f.len() == 3 && f[0] == 0), "{label}: {model:?}");
            assert_eq!(eeprom(&session)[0x0AF], 0);
        } else {
            assert_eq!(text, "nan", "{label}: the handset prints nan (the LCD shows ?a?%)");
            assert!(first.is_nan(), "{label}: the erased dose base is NaN: {first}");
            assert!(can_frames(&session, "0x225").is_empty(), "{label}: an erased model (0xff) means no CAN 0x225");
            assert_eq!(eeprom(&session)[0x0AF], 0xFF);
        }
    }
}

#[test]
fn the_serial_action_still_works_and_the_stored_serial_survives_a_restart() {
    let (main, handset) = images_or_skip!(&TRITON);
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut session, 8.0);
    assert_eq!(state(&session).get("serialNumber").and_then(Json::as_u64), Some(1), "the factory serial");
    // The serial action writes the new number; the fixture finds it non-erased at the recreation.
    let after = act(&mut session, "{\"action\":\"serial\",\"serialNumber\":7}");
    assert_eq!(after.get("serialNumber").and_then(Json::as_u64), Some(7));
    assert!(!flag(&factory(&after), "applied"), "{:?}", factory(&after));
    advance(&mut session, 3.0);
    assert_eq!(&eeprom(&session)[0..4], &[7, 0, 0, 0]);
    let again = act(&mut session, "{\"action\":\"reset\"}");
    assert_eq!(again.get("serialNumber").and_then(Json::as_u64), Some(7), "a Restart keeps the stored serial");
    assert!(factory(&again).get("reason").and_then(Json::as_str).unwrap_or_default().starts_with("Not needed"));
}

#[test]
fn a_restart_gives_finite_tissues_and_an_ndl_below_99_with_both_repair_fixtures_off_and_nan_without_the_factory_init() {
    let (main, handset) = images_or_skip!(&TRITON);
    for factory_init in [true, false] {
        let label = if factory_init { "factory init on" } else { "factory init off" };
        // Both decompression fixtures off: the factory init alone has to make the tissues finite.
        let config = SessionConfig { eeprom_factory_init: factory_init, deco_storage_fixture: false, start_at_surface: false, ..SessionConfig::default() };
        let mut session = Session::new(config, Some(&main), &handset, Profile::default()).expect("session");
        advance(&mut session, 8.0);
        calibrate_oxygen(&mut session);
        let first = state(&session);
        assert_eq!((health(&first, "tissues"), health(&first, "oxygen")), ("valid".to_string(), "calibrated".to_string()), "{label}: the first boot reset its tissues itself");
        // Back at the surface (the start-at-the-surface fixture is off), then the Restart: the saved profile holds a date and, without the
        // factory init, an erased tissue block.
        set_pressure(&mut session, DEEP_MBAR);
        advance(&mut session, 3.0);
        set_pressure(&mut session, SURFACE_MBAR);
        advance(&mut session, 1.0);
        let restarted = act(&mut session, "{\"action\":\"reset\"}");
        assert!(!flag(restarted.get("decoStorageFixture").expect("decoStorageFixture"), "applied"), "{label}: the date-erase repair is off");
        advance(&mut session, 4.0);
        let after = state(&session);
        assert_eq!(health(&after, "oxygen"), "calibrated", "{label}");
        set_pressure(&mut session, DEEP_MBAR);
        let mut ndl = peek(&session, Which::Main, RAW_NDL) as i32;
        for _ in 0..12 {
            advance(&mut session, 2.5);
            ndl = peek(&session, Which::Main, RAW_NDL) as i32;
            if factory_init && ndl < 99 {
                break;
            }
        }
        let deep = state(&session);
        println!("{label}: raw NDL {ndl} min, tissues {}", health(&deep, "tissues"));
        if factory_init {
            assert_eq!(health(&deep, "tissues"), "valid", "{label}: {:?}", deep.get("decoHealth"));
            assert!((1..99).contains(&ndl), "{label}: the no-decompression limit is below 99 at depth, was {ndl}");
        } else {
            assert_eq!(health(&deep, "tissues"), "invalid", "{label}: the old NaN state: {:?}", deep.get("decoHealth"));
            advance(&mut session, 15.0);
            assert_eq!(peek(&session, Which::Main, RAW_NDL) as i32, 99, "{label}: the limit stays at 99 with NaN tissues");
        }
    }
}

#[test]
fn the_stored_tissue_block_is_n2_then_he_per_record_and_a_filled_block_loads_at_a_restart() {
    let (main, handset) = images_or_skip!(&TRITON);
    // A first boot with the fixture off, then a stored block of recognizable words (N2 1.0, He 0.25) next to the date it saved.
    let mut first = Session::new(SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() }, Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut first, 8.0);
    let mut profile = first.shutdown();
    let mut image = profile.eeprom.take().expect("eeprom").to_vec();
    assert!(image[0x0FF..0x17F].iter().all(|&b| b == 0xFF) && image[0x17F..0x183].iter().any(|&b| b != 0xFF), "a saved date and no tissues");
    for record in 0..16 {
        image[0x0FF + 8 * record..0x0FF + 8 * record + 4].copy_from_slice(&1.0f32.to_le_bytes());
        image[0x0FF + 8 * record + 4..0x0FF + 8 * record + 8].copy_from_slice(&0.25f32.to_le_bytes());
    }
    let mut session = Session::new(SessionConfig::default(), Some(&main), &handset, Profile { eeprom: Some(image.clone()), ..profile.clone() }).expect("session");
    assert_eq!(&eeprom(&session)[0x0FF..0x183], &image[0x0FF..0x183], "a stored block and the date record next to it are never touched");
    assert_eq!(&eeprom(&session)[0..4], &[1, 0, 0, 0], "while the erased serial next to them is filled");
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
    // The fixture's own block (surface values) loads the same way at a Restart.
    let mut second = Session::new(SessionConfig { deco_storage_fixture: false, ..SessionConfig::default() }, Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut second, 8.0);
    act(&mut second, "{\"action\":\"reset\"}");
    advance(&mut second, 5.0);
    assert_eq!(health(&state(&second), "tissues"), "valid");
    assert_eq!(peek(&second, Which::Main, TISSUE_RAM + TISSUE_N2_OFFSET), 0x3F40_304D);
}

// ---- existing profiles --------------------------------------------------------------------------------------------------------

#[test]
fn an_existing_profile_only_has_its_erased_inventoried_records_filled() {
    let (main, handset) = images_or_skip!(&TRITON);
    // A profile an older build left behind: a first boot with a calibration, no factory values.
    let mut old = Session::new(SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() }, Some(&main), &handset, Profile::default()).expect("session");
    advance(&mut old, 8.0);
    calibrate_oxygen(&mut old);
    let saved_profile = old.shutdown();
    let saved = saved_profile.eeprom.clone().expect("eeprom");
    assert!(saved[0x0FF..0x17F].iter().all(|&b| b == 0xFF) && saved[0..4] == [0xFF; 4]);

    let open = |profile: &Profile, config: SessionConfig| Session::new(config, Some(&main), &handset, profile.clone()).expect("reopen");
    let mut filled = open(&saved_profile, SessionConfig::default());
    let image = eeprom(&filled);
    let report = factory(&state(&filled));
    assert!(flag(&report, "applied"), "{report:?}");
    assert_eq!(record_ids(&report).len(), 8);
    // Only erased bytes of the inventoried records changed; the calibration (values, flags, gas, pressure, time) did not.
    let changed: Vec<usize> = (0..EEPROM_BYTES).filter(|&i| image[i] != saved[i]).collect();
    assert!(!changed.is_empty());
    assert!(changed.iter().all(|&i| saved[i] == 0xFF), "no stored byte changed");
    assert!(changed.iter().all(|&i| i < 4 || i == 0x0AF || (0x0B8..0x0BF).contains(&i) || (0x0FF..0x17F).contains(&i) || (0x183..0x187).contains(&i) || (0x190..0x194).contains(&i)), "{changed:?}");
    assert_eq!(&image[0x38..0x47], &saved[0x38..0x47], "the oxygen calibration is untouched");
    assert_eq!(&image[0x38..0x3E], &[0xE8, 0x03, 0xE8, 0x03, 0xE8, 0x03]);
    assert_eq!(image[254], 0xA3);
    // The date-erase repair has nothing to do now, and says why.
    let deco = state(&filled).get("decoStorageFixture").cloned().expect("decoStorageFixture");
    assert!(!flag(&deco, "applied") && deco.get("reason").and_then(Json::as_str).unwrap_or_default().contains("factory-init"), "{deco:?}");
    // The filled image is the profile from now on, and a second opening finds nothing to fill.
    let reopened_profile = filled.export_profile();
    assert!(reopened_profile.eeprom.as_deref() == Some(&image[..]), "the profile holds the filled image");
    let again = open(&reopened_profile, SessionConfig::default());
    assert!(eeprom(&again)[..] == image[..], "a second opening changes nothing");
    let report = factory(&state(&again));
    assert!(!flag(&report, "applied") && report.get("reason").and_then(Json::as_str).unwrap_or_default().starts_with("Not needed"), "{report:?}");
    assert_eq!(record_ids(&report).len(), 0);

    // With the fixture off the saved image is loaded as it is (the old deco repair applies instead).
    let off = open(&saved_profile, SessionConfig { eeprom_factory_init: false, deco_storage_fixture: false, ..SessionConfig::default() });
    assert!(eeprom(&off)[..] == saved[..], "switched off (and without the date-erase repair) the saved image is loaded as it is");
    let repaired = open(&saved_profile, SessionConfig { eeprom_factory_init: false, ..SessionConfig::default() });
    let repaired_image = eeprom(&repaired);
    let changed: Vec<usize> = (0..EEPROM_BYTES).filter(|&i| repaired_image[i] != saved[i]).collect();
    assert_eq!(changed, [0x17F, 0x180, 0x181, 0x182], "without the factory init the older date-erase repair applies, as before");

    // A profile whose inventoried records all hold values is untouched, whatever the values are.
    let mut stored = saved.to_vec();
    stored[0..4].copy_from_slice(&[7, 0, 0, 0]);
    stored[0x0AF] = 1;
    stored[0x0B8..0x0BF].copy_from_slice(&[0x00, 0x00, 0x80, 0x3F, 0x05, 0x00, 0x32]);
    for (i, byte) in stored[0x0FF..0x17F].iter_mut().enumerate() {
        *byte = (i % 7) as u8;
    }
    stored[0x183..0x187].copy_from_slice(&[0x10, 0x0E, 0, 0]);
    stored[0x190..0x194].copy_from_slice(&[1, 2, 3, 4]);
    let kept = open(&Profile { eeprom: Some(stored.clone()), ..saved_profile.clone() }, SessionConfig::default());
    assert!(eeprom(&kept)[..] == stored[..], "a profile whose inventoried records hold values is untouched");
    assert!(!flag(&factory(&state(&kept)), "applied"));
    // A stored NaN is data, not erasure: the dose base stays whatever it holds.
    let mut nan = saved.to_vec();
    nan[0x0B8..0x0BC].copy_from_slice(&f32::NAN.to_le_bytes());
    let nan_session = open(&Profile { eeprom: Some(nan.clone()), ..saved_profile.clone() }, SessionConfig::default());
    assert_eq!(&eeprom(&nan_session)[0x0B8..0x0BC], &nan[0x0B8..0x0BC]);
}

// ---- the record table and the releases ---------------------------------------------------------------------------------------

#[test]
fn both_releases_carry_the_verified_record_table_and_the_inventory_matches_it() {
    let available: Vec<(&Release, Firmware)> = [&TRITON, &NEPTUN].into_iter().filter_map(|release| images(release).map(|(main, _)| (release, main))).collect();
    if available.is_empty() {
        eprintln!("skipping: no SREC files are available");
        return;
    }
    for (release, main) in &available {
        let table = record_table(release, main).unwrap_or_else(|| panic!("{}: the table is inside the image", release.id));
        assert_eq!(table.len(), RECORD_TABLE_BYTES);
        assert_eq!(ngc::sha256::digest_hex(table), RECORD_TABLE_SHA256, "{}: the same 568 bytes in both releases", release.id);
        let entry = |id: usize| (u16::from_le_bytes([table[4 * id], table[4 * id + 1]]) as usize, table[4 * id + 2] as usize);
        // The inventory (physical offset and size per logical ID) is what the image's table says.
        for (id, offset, size) in [(0x01, 0x000, 4), (0x2B, 0x0AF, 1), (0x67, 0x0B8, 4), (0x68, 0x0BC, 2), (0x69, 0x0BE, 1), (0x8B, 0x183, 4), (0x8D, 0x190, 4)] {
            assert_eq!(entry(id), (offset, size), "{} record 0x{id:02x}", release.id);
        }
        for id in 0x6Ausize..=0x89 {
            assert_eq!(entry(id), (0x0FF + 4 * (id - 0x6A), 4), "{} tissue record 0x{id:02x}", release.id);
        }
        // The validity marker the default routine reads is record 0x63 at offset 254, which is not part of the inventory.
        assert_eq!(entry(0x63), (254, 1), "{}", release.id);
    }
}

#[test]
fn a_fresh_neptun_profile_gets_the_same_inventory_and_a_finite_delta_vc() {
    let (main, handset) = images_or_skip!(&NEPTUN);
    for fixture in [true, false] {
        let config = SessionConfig { eeprom_factory_init: fixture, ..SessionConfig::default() };
        let (mut session, _) = dive_to_the_toxicity_page(config, &main, &handset);
        let report = factory(&state(&session));
        let frame = f32_of(can_frames(&session, "0x226").last().expect("a frame of 0x226"));
        if fixture {
            assert!(flag(&report, "applied") && record_ids(&report).len() == 8, "{report:?}");
            assert_eq!(&eeprom(&session)[0..4], &[1, 0, 0, 0]);
            assert_eq!(state(&session).get("serialNumber").and_then(Json::as_u64), Some(1));
            assert!(frame.is_finite() && frame >= 0.0, "{frame}");
            advance(&mut session, 25.0);
            assert!(f32_of(can_frames(&session, "0x226").last().unwrap()) > frame, "the value rises");
        } else {
            assert!(!flag(&report, "applied"));
            assert!(frame.is_nan(), "NEPTUN reproduces the NaN of the erased dose base too: {frame}");
        }
    }
}

#[test]
fn a_handset_only_run_has_no_eeprom_and_reports_that() {
    let (_, handset) = images_or_skip!(&TRITON);
    let config = SessionConfig { mode: ngc::system::Mode::HandsetOnly, ..SessionConfig::default() };
    let session = Session::new(config, None, &handset, Profile::default()).expect("handset-only session");
    let report = factory(&state(&session));
    assert!(flag(&report, "enabled") && !flag(&report, "applied"));
    assert!(report.get("reason").and_then(Json::as_str).unwrap_or_default().starts_with("Not applicable"), "{report:?}");
}
