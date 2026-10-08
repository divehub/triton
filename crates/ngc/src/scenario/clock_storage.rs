//! `clock-storage`: the genuine CAN clock-setting route of the main firmware and the retention of calendar and storage
//! across Restart, cold boot, Wake and closing/reopening the profile (`emulation/probe_clock_storage.py`,
//! `emulation/storage-clock-investigation.md`, `emulation/probe_rtc_recovery.py`, the fresh-profile evidence
//! `emulation/runtime/clock-storage/20261007T122012979393Z/result.json`).
//!
//! The clock screen is selected with a zero-length standard frame `0x129` (screen 10), then `0x7A` with the packed date
//! `9A 1E 26 E2` (2026-10-07 12:34:56) reaches the original setter, which stores the packed value in the EEPROM
//! (offset `0x2D`, `0xE2261E9A`) and programs the RTC. The frames are written into the handset CAN controller's transmit
//! mailbox like the Renode `NGCCANStimulus`; nothing is injected into application RAM.

use super::*;
use crate::persistence::{Provenance, RtcState, BOARD_MAIN};
use crate::session::Session;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/clock-storage/20261007T122012979393Z/result.json";
const PACKED_DATE: u32 = 0xE226_1E9A;

fn calendar(rig: &Rig, which: Which) -> (u32, u32) {
    let checkpoint = rig.session.system().rtc_checkpoint(which).expect("RTC");
    (checkpoint.time_register, checkpoint.date_register)
}

fn calendars(rig: &Rig) -> Json {
    let (mtr, mdr) = calendar(rig, Which::Main);
    let (htr, hdr) = calendar(rig, Which::Handset);
    Json::object()
        .with("main", Json::object().with("tr", u64::from(mtr)).with("dr", u64::from(mdr)))
        .with("handset", Json::object().with("tr", u64::from(htr)).with("dr", u64::from(hdr)))
}

fn uart4_text(state: &Json) -> String {
    state
        .get("uartConsole")
        .and_then(Json::as_array)
        .and_then(|channels| channels.iter().find(|c| c.get("id").and_then(Json::as_str) == Some("main.uart4")))
        .and_then(|c| c.get("text").and_then(Json::as_str))
        .unwrap_or("")
        .to_string()
}

fn packed_eeprom(rig: &Rig) -> u32 {
    rig.session.system().main.as_ref().and_then(|m| m.eeprom.get_double_word(0x2D).ok()).unwrap_or(0)
}

fn date_is_set(rig: &Rig, which: Which) -> bool {
    let (tr, dr) = calendar(rig, which);
    dr & 0x00FF_1F3F == 0x0026_1007 && tr & 0x003F_7F00 != u32::MAX
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("clock-storage");
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;

    // ---- first boot of a fresh profile ---------------------------------------------------------------------------
    let state = rig.advance(4.5)?;
    rec.step("first boot (4.5 s)", &state, Json::object().with("rtc", calendars(&rig)));
    rec.check("frame ready, batteries ready, no error", state.get("frameReady") == Some(&Json::Bool(true)) && state.get("mainBatteryReady") == Some(&Json::Bool(true)) && state.get("error").is_some_and(Json::is_null), state.get("error").cloned().unwrap_or(Json::Null));
    rec.check("no CPU faults on either board", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    let first_text = uart4_text(&state);
    let invalid_size = first_text.matches("[eeprom_driver] READ: INVALID SIZE").count();
    rec.compare("first-boot console: EEPROM size diagnostics from the setting 0x14 fall-through (count)", invalid_size, 1usize, SOURCE, true, "one invalid duplicate write of ID 20 with size 2 (static defect at 0x08009744, LR 0x080097D1)");
    rec.compare("first-boot console: erased-NOR littlefs diagnostic", first_text.contains("Corrupted dir pair at {0x0, 0x1}"), true, SOURCE, true, "a fresh synthetic NOR image has no littlefs metadata");
    rec.check("first-boot console starts with the handset wake line", first_text.starts_with("Wakeup from: HANDSET"), first_text.lines().next().unwrap_or("").to_string());
    rec.compare("EEPROM validity marker after the first boot", u64::from(rig.session.system().main.as_ref().and_then(|m| m.eeprom.get_byte(254).ok()).unwrap_or(0)), 0xA3u64, SOURCE, true, "");
    let first = calendars(&rig);
    rec.compare("RTC calendars at 4.5 s of a fresh profile (2020-01-01 default, ticking since init)", first, Json::object().with("main", Json::object().with("tr", 4u64).with("dr", 2_105_601u64)).with("handset", Json::object().with("tr", 3u64).with("dr", 2_105_601u64)), SOURCE, true, "");
    let first_png = rig.png();
    rec.image("first-boot.png", first_png);

    // ---- the genuine clock-setting route ---------------------------------------------------------------------------
    rig.session.inject_can_from_handset(0x129, &[])?;
    rig.advance(0.5)?;
    rec.compare("main screen state after the clock-screen request 0x129", rig.u8(Which::Main, 0x2000_438D), 10u32, SOURCE, true, "state 10 = date settings (request table 0x080144B4)");
    rig.session.inject_can_from_handset(0x7A, &[0x9A, 0x1E, 0x26, 0xE2])?;
    let state = rig.advance(0.5)?;
    let set = calendars(&rig);
    rec.step("clock set (5.5 s)", &state, Json::object().with("rtc", set.clone()));
    let (mtr, mdr) = calendar(&rig, Which::Main);
    rec.check("main RTC date is 2026-10-07", mdr & 0x00FF_1F3F == 0x0026_1007, u64::from(mdr));
    rec.check("main RTC time is 12:34:xx", mtr & 0x003F_7F00 == 0x0012_3400, u64::from(mtr));
    rec.compare("main RTC registers 0.5 s after the setter", Json::from_items([u64::from(mtr), u64::from(mdr)]), Json::from_items([1_193_046u64, 2_510_855]), SOURCE, true, "12:34:56 on 2026-10-07 with the firmware's own weekday field");
    rec.compare("packed calendar checkpoint in the EEPROM (0x2D)", u64::from(packed_eeprom(&rig)), u64::from(PACKED_DATE), SOURCE, true, "year offset 26, month 10, day 7, 12:34:56");
    let saved_calendar = calendars(&rig);
    // The link trace of this first session holds the handshake and the two stimulus frames (0x129, 0x7A).
    let clock_trace = rig.can_trace();
    rec.check("the CAN trace records the clock-screen request 0x129 and the setter 0x7A from the handset", clock_trace.contains("ngc-handset.can1\t0x129") && clock_trace.contains("ngc-handset.can1\t0x07A"), clock_trace.lines().filter(|l| l.contains("ngc-handset.can1")).count() as u64);

    // ---- the legacy EEPROM seed: the same EEPROM without an RTC checkpoint ------------------------------------------
    let exported = rig.session.export_profile();
    let legacy = Profile { eeprom: exported.eeprom.clone(), nor: exported.nor.clone(), ..Profile::default() };
    let seeded = Rig::new(env, SessionConfig::default(), legacy)?;
    let seeded_state = seeded.state();
    let sources = seeded_state.get("rtcPersistence").and_then(|r| r.get("sources")).cloned().unwrap_or(Json::Null);
    rec.compare(
        "legacy migration: provenance of the main calendar",
        sources.get("ngc-main").cloned().unwrap_or(Json::Null),
        Json::object().with("source", "eeprom-packed-date").with("packedBackup", u64::from(PACKED_DATE)),
        "emulation/test_rtc_persistence.py (valid_eeprom_migration...)",
        true,
        "marker 0xA3 and a real packed date",
    );
    rec.check("legacy migration: the handset stays on its fresh RTC and no board counts as restored", sources.get("ngc-handset").and_then(|s| s.get("source")).and_then(Json::as_str) == Some("fresh-rtc") && seeded_state.get("rtcPersistence").and_then(|r| r.get("restoredBoards")).is_some_and(|b| b.is_empty()), sources.clone());
    let (str_, sdr) = calendar(&seeded, Which::Main);
    rec.check("legacy migration: the seeded main calendar is 2026-10-07 12:34:56 (ISO weekday 3)", str_ == 0x0012_3456 && sdr == 0x0026_7007, Json::from_items([u64::from(str_), u64::from(sdr)]));
    rec.check("legacy migration read the EEPROM without changing it", seeded.session.system().main.as_ref().map(|m| m.eeprom.image().to_vec()) == exported.eeprom, "eeprom image unchanged");

    // ---- backup words survive Restart ---------------------------------------------------------------------------------
    {
        let system = rig.session.system_mut();
        system.board_mut(Which::Main).unwrap().bus_write(0x4000_2860, Width::Word, 0x1234_ABCD);
        system.board_mut(Which::Handset).unwrap().bus_write(0x4000_2860, Width::Word, 0x5678_DCBA);
    }
    let bkp4 = |rig: &Rig| {
        Json::from_items([
            u64::from(rig.session.system().rtc_checkpoint(Which::Main).unwrap().backup_registers[4]),
            u64::from(rig.session.system().rtc_checkpoint(Which::Handset).unwrap().backup_registers[4]),
        ])
    };
    let saved_bkp = bkp4(&rig);
    rec.expect("backup register 4 of both boards holds the probe words", saved_bkp.clone(), Json::from_items([0x1234_ABCDu64, 0x5678_DCBA]));

    // ---- Restart: whole-second calendars are restored before the guests run ------------------------------------------------
    rig.act("{\"action\":\"reset\"}")?;
    let after = calendars(&rig);
    rec.compare("calendars immediately after Restart equal the saved ones", after.clone(), saved_calendar, SOURCE, true, "restored through the protected RTC sequence before guest execution");
    rec.compare("the Renode calendars after Restart (main TR/DR, handset TR)", Json::from_items([after.get("main").and_then(|m| m.get("tr")).and_then(Json::as_u64).unwrap_or(0), after.get("handset").and_then(|m| m.get("tr")).and_then(Json::as_u64).unwrap_or(0)]), Json::from_items([1_193_046u64, 4]), SOURCE, true, "");
    rec.expect("backup register 4 after Restart", bkp4(&rig), saved_bkp);
    let state = rig.advance(4.5)?;
    let second_text = uart4_text(&state);
    rec.step("second boot (4.5 s)", &state, Json::object().with("rtc", calendars(&rig)));
    rec.check("second boot: no EEPROM size diagnostic and no littlefs directory error (persisted storage)", !second_text.contains("[eeprom_driver]") && !second_text.contains("Corrupted dir pair"), second_text.clone());
    rec.check("second boot: date still 2026-10-07", date_is_set(&rig, Which::Main), calendars(&rig));
    rec.compare("calendars at the end of the second boot (main TR, handset TR)", Json::from_items([u64::from(calendar(&rig, Which::Main).0), u64::from(calendar(&rig, Which::Handset).0)]), Json::from_items([1_193_216u64, 8]), SOURCE, true, "12:35:00 and 4 handset RTC seconds later");
    rec.check("second boot: frame ready, no faults", state.get("frameReady") == Some(&Json::Bool(true)) && rig.faults_clear(), state.get("frameReady").cloned().unwrap_or(Json::Null));

    // ---- cold boot to observed standby, then Wake ----------------------------------------------------------------------------
    rig.act("{\"action\":\"cold\"}")?;
    let state = rig.advance(2.0)?;
    rec.step("cold (standby)", &state, Json::object().with("rtc", calendars(&rig)));
    rec.compare("cold boot: observed standby time", state.get("standbyTime").and_then(Json::as_f64), Some(1.55f64), SOURCE, true, "the original firmware selects wake cause 0 and requests standby");
    rec.check("cold boot: standby, not running", state.get("standby") == Some(&Json::Bool(true)) && state.get("running") == Some(&Json::Bool(false)), state.get("standby").cloned().unwrap_or(Json::Null));
    rec.check("cold boot: the calendar date is retained", date_is_set(&rig, Which::Main), calendars(&rig));
    let cold_clock = calendars(&rig);
    rec.compare("cold boot: calendars at standby (main TR, handset TR)", Json::from_items([u64::from(calendar(&rig, Which::Main).0), u64::from(calendar(&rig, Which::Handset).0)]), Json::from_items([1_193_217u64, 9]), SOURCE, true, "");
    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare(
        "cold boot: CAN traffic before standby (transmitted, last identifier)",
        Json::object().with("transmitted", summary_number(&can, "transmitted")).with("lastId", summary_field(&can, "lastId").map(str::to_string)),
        Json::object().with("transmitted", 1u64).with("lastId", "0x154"),
        SOURCE,
        true,
        "",
    );
    let png = rig.png();
    rec.image("cold-standby.png", png);
    rig.act("{\"action\":\"wake\"}")?;
    rec.compare("Wake keeps the calendars of the cold boot", calendars(&rig), cold_clock, SOURCE, true, "");
    let state = rig.advance(4.5)?;
    rec.step("wake (4.5 s)", &state, Json::object().with("rtc", calendars(&rig)));
    rec.check("wake: frame ready, not in standby, date retained", state.get("frameReady") == Some(&Json::Bool(true)) && state.get("standby") == Some(&Json::Bool(false)) && date_is_set(&rig, Which::Main), state.get("frameReady").cloned().unwrap_or(Json::Null));
    rec.compare("wake: calendars at 4.5 s (main TR, handset TR)", Json::from_items([u64::from(calendar(&rig, Which::Main).0), u64::from(calendar(&rig, Which::Handset).0)]), Json::from_items([1_193_221u64, 19]), SOURCE, true, "");
    rec.check("wake: no faults", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));

    // ---- close and reopen ---------------------------------------------------------------------------------------------------------
    let before_close = calendars(&rig);
    let Rig { session } = rig;
    let profile = session.shutdown();
    let state_text = profile.rtc_state.clone().unwrap_or_default();
    let parsed = RtcState::parse(&state_text, "rtc-state.json");
    rec.check("the closed profile's rtc-state.json parses strictly and holds both boards", parsed.as_ref().is_ok_and(|s| s.boards.len() == 2), parsed.as_ref().err().cloned().unwrap_or_default());
    if let Ok(parsed) = &parsed {
        rec.check("the saved main provenance is rtc-registers", parsed.board(BOARD_MAIN).map(|b| b.provenance.clone()) == Some(Provenance::RtcRegisters), "rtc-registers");
    }
    let reopened = Session::new_with(env.options, "", env.configure(super::recorded_config(SessionConfig::default())), Some(env.main), env.handset, profile.clone())?;
    let reopened = Rig { session: reopened };
    rec.compare("calendars after closing and reopening the profile", calendars(&reopened), before_close, SOURCE, true, "reopened: saved main/handset calendars equal what ran before the close");
    rec.file("rtc-state.json", state_text.into_bytes());
    rec.file("eeprom.bin", profile.eeprom.unwrap_or_default());
    rec.file("can-trace-clock-set.tsv", clock_trace.into_bytes());
    rec.limitation("Calendar progression is virtual-time only: Pause, observed standby and a closed session freeze it, no host downtime is added. Retention is whole calendar seconds; alarm/wakeup phases and pending interrupts are not retained.");
    rec.limitation("The clock command is a synthetic mailbox stimulus through the genuine protocol handler, not a physical button event. Static analysis, not this scenario, attributes the EEPROM size diagnostic to the setting-0x14 fall-through.");
    Ok(rec.finish(env))
}
