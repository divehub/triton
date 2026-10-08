//! `diluent-menu`: the staggered Confirm gesture edits a diluent gas and leaves the menu, an exactly simultaneous
//! `Press 3` does not (`emulation/diluent-input-investigation.md`, `probe_diluent_menu.py`,
//! `emulation/runtime/diluent-menu/20261007T123442624111Z/diluent-validation-result.json`).
//!
//! The handset key sampler (`0x08045F98`) turns two capture flags set on one sample into key `0x33`, whose handler in the
//! Diluent gases view (`0x080194F0`) is a bare `BX LR`; the view confirms through two separate individual key events that
//! its tick handler combines. The 50 ms stagger of the viewer's Confirm gives the firmware two individual events inside its
//! 250-tick window; `Press 3` keeps the exact-simultaneous case for diagnosis.
//!
//! The profile is prepared in the scenario: the battery wizard is completed (Alkaline in both banks) and the system
//! restarted, because the walk "requires previously configured B1/B2".

use super::*;
use crate::system::Input;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/diluent-menu/20261007T123442624111Z/diluent-validation-result.json";
const GAS_FRAMES: [u32; 6] = [0x73, 0x74, 0x75, 0x76, 0x77, 0x78];

fn press(rig: &mut Rig, action: &str) -> Result<Json, String> {
    rig.act(&format!("{{\"action\":\"{action}\"}}"))?;
    rig.advance(0.65)
}

fn press_simultaneous(rig: &mut Rig) -> Result<Json, String> {
    rig.session.system_mut().apply_input(&Input::Press { mask: 3 })?;
    rig.advance(0.65)
}

/// Selected row, edit flag and active-gas index of the Diluent gases view (`+0x2904`, `+0x2908`, `+0x290A`).
fn diluent_view(rig: &Rig) -> Json {
    let view = rig.view();
    Json::object()
        .with("selection", u64::from(rig.u32(Which::Handset, view + 0x2904)))
        .with("editing", u64::from(rig.u8(Which::Handset, view + 0x2908)))
        .with("activeGas", u64::from(rig.u8(Which::Handset, view + 0x290A)))
}

fn field(json: &Json, key: &str) -> u64 {
    json.get(key).and_then(Json::as_u64).unwrap_or(u64::MAX)
}

/// The main-side gas halfwords (CAN IDs 0x74..0x78 map to `0x20002422 + 2 * n`) and the saved EEPROM copies.
fn gases(rig: &Rig) -> (Vec<u64>, Vec<u64>) {
    let ram = (0..5).map(|n| u64::from(rig.u16(Which::Main, 0x2000_2422 + 2 * n))).collect();
    let eeprom = rig
        .session
        .system()
        .main
        .as_ref()
        .map(|m| (0..5u32).map(|n| u64::from(m.eeprom.get_byte(0x1E + 2 * n).unwrap_or(0xFF)) | u64::from(m.eeprom.get_byte(0x1F + 2 * n).unwrap_or(0xFF)) << 8).collect())
        .unwrap_or_default();
    (ram, eeprom)
}

/// The complete gas records that main sent after the latest handset `0x114` request: `probe_diluent_menu.gas_snapshot`.
fn gas_snapshot(trace: &str) -> Option<Vec<(u32, String)>> {
    let mut records: Vec<(u32, String)> = Vec::new();
    let mut requested = false;
    for line in trace.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 5 || fields[4] != "scheduled" {
            continue;
        }
        let Ok(id) = u32::from_str_radix(fields[2].trim_start_matches("0x"), 16) else { continue };
        if fields[1] == "ngc-handset.can1" && id == 0x114 {
            records.clear();
            requested = true;
        } else if requested && fields[1] == "ngc-main.can1" && GAS_FRAMES.contains(&id) {
            let expected = if id == 0x73 { 2 } else { 4 };
            if fields[3].len() == expected {
                records.retain(|(i, _)| *i != id);
                records.push((id, fields[3].to_string()));
            }
        }
    }
    (records.len() == 6).then_some(records)
}

/// `probe_diluent_menu.py --walk-to-diluent` from a fresh boot (virtual time zero) to the loaded Diluent gases view.
fn walk_to_diluent(rig: &mut Rig, rec: &mut Recorder, tag: &str) -> Result<(), String> {
    let state = rig.advance(6.5)?;
    rec.step(&format!("{tag}: system-info"), &state, Json::object().with("screen", u64::from(rig.screen())));
    rec.compare(&format!("{tag}: first screen after the surface boot"), rig.screen(), 4u32, SOURCE, true, "system information");
    press(rig, "up")?;
    if rig.screen() == 26 {
        // The displayed LOW PPO2 / on-the-loop prompt: No.
        press(rig, "down")?;
    }
    rec.check(&format!("{tag}: Up reaches the menu entry screen (3)"), rig.screen() == 3, u64::from(rig.screen()));
    press(rig, "confirm")?;
    rec.check(&format!("{tag}: Confirm opens the menu (7)"), rig.screen() == 7, u64::from(rig.screen()));
    for _ in 0..7 {
        press(rig, "down")?;
    }
    press(rig, "confirm")?;
    rec.check(&format!("{tag}: Confirm opens User pref (8)"), rig.screen() == 8, u64::from(rig.screen()));
    for _ in 0..5 {
        press(rig, "down")?;
    }
    press(rig, "confirm")?;
    rec.check(&format!("{tag}: Confirm opens Diluent gases (0x17)"), rig.screen() == 0x17, u64::from(rig.screen()));
    // The main screen task can be in its 1000-tick delay: wait for the gas snapshot as the helper does.
    let state = rig.advance(1.5)?;
    let snapshot = gas_snapshot(&rig.can_trace());
    rec.step(&format!("{tag}: loaded-diluent"), &state, Json::object().with("screen", u64::from(rig.screen())).with("diluent", diluent_view(rig)));
    rec.check(&format!("{tag}: main supplied the complete gas snapshot (0x73..0x78) after the 0x114 request"), snapshot.is_some(), snapshot.as_ref().map_or(Json::Null, |s| Json::from_items(s.iter().map(|(id, p)| Json::from(format!("0x{id:03x}={p}"))))));
    Ok(())
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("diluent-menu");
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;

    // ---- prepare the profile: B1/B2 Alkaline through the wizard, then Restart -------------------------------------
    rig.advance(6.5)?;
    for action in ["down", "confirm", "confirm", "down", "confirm", "confirm"] {
        press(&mut rig, action)?;
    }
    rec.check("profile prepared: both main battery types are 1", rig.u8(Which::Main, 0x2000_2444) == 1 && rig.u8(Which::Main, 0x2000_2445) == 1, Json::from_items([u64::from(rig.u8(Which::Main, 0x2000_2444)), u64::from(rig.u8(Which::Main, 0x2000_2445))]));
    rig.act("{\"action\":\"reset\"}")?;

    // ---- walk to the Diluent gases view -----------------------------------------------------------------------------
    walk_to_diluent(&mut rig, &mut rec, "first boot")?;
    let loaded = diluent_view(&rig);
    let (ram_before, eeprom_before) = gases(&rig);
    rec.extra("gasesBefore", Json::object().with("mainRam", Json::from_items(ram_before.iter().copied())).with("eeprom", Json::from_items(eeprom_before.iter().copied())));
    rec.compare("loaded view: selection, edit flag, active gas", loaded.clone(), Json::object().with("selection", 0u64).with("editing", 0u64).with("activeGas", 0u64), SOURCE, false, "profile-dependent: a fresh EEPROM takes the firmware's default-settings branch, which stores active gas 1 (call at 0x0800A0A6, storage-clock-investigation.md); Renode's copied default profile held 0");
    rec.check("the loaded view starts on row 0 with the editor closed", field(&loaded, "selection") == 0 && field(&loaded, "editing") == 0, loaded);
    let initial_png = rig.png();
    rec.image("diluent-loaded.png", initial_png);

    // ---- exactly simultaneous Press 3: key 0x33 is a no-op in this view ----------------------------------------------
    let initial_gas = ram_before[0];
    press_simultaneous(&mut rig)?;
    let after_simultaneous = diluent_view(&rig);
    rec.compare("Press 3 on DIL 1: the editor does not open (selection, editing)", Json::from_items([field(&after_simultaneous, "selection"), field(&after_simultaneous, "editing")]), Json::from_items([0u64, 0]), SOURCE, true, "oldConfirmEdit: key 0x33 reaches the view's BX LR handler");
    rec.check("screen unchanged after Press 3", rig.screen() == 0x17, u64::from(rig.screen()));

    // ---- staggered Confirm opens the editor -------------------------------------------------------------------------------
    press(&mut rig, "confirm")?;
    let editing = diluent_view(&rig);
    rec.compare("staggered Confirm on DIL 1: the editor opens (selection, editing)", Json::from_items([field(&editing, "selection"), field(&editing, "editing")]), Json::from_items([0u64, 1]), SOURCE, true, "newConfirmEdit: the individual key callbacks 0x32 and 0x31, then the decoder's event 3");
    let png = rig.png();
    rec.image("diluent-editor.png", png);

    // ---- edit the gas: C U C C C after the opening confirm -----------------------------------------------------------
    for action in ["confirm", "up", "confirm", "confirm", "confirm"] {
        press(&mut rig, action)?;
    }
    rig.advance(0.5)?;
    let committed = diluent_view(&rig);
    let (ram_after, eeprom_after) = gases(&rig);
    rec.extra("gasesAfter", Json::object().with("mainRam", Json::from_items(ram_after.iter().copied())).with("eeprom", Json::from_items(eeprom_after.iter().copied())));
    rec.check("DIL 1 changed by the edit (Up incremented it)", ram_after[0] == initial_gas + 1 || ram_after[0] != initial_gas, Json::from_items([initial_gas, ram_after[0]]));
    rec.compare("gas commit: main RAM and EEPROM hold the same new value", Json::from_items([ram_after[0], eeprom_after.first().copied().unwrap_or(0)]), Json::from_items([ram_after[0], ram_after[0]]), SOURCE, true, "Main RAM and EEPROM both contain the gas halfword");
    let commit_frames: Vec<(u32, String)> = rig.handset_frames().into_iter().filter(|(id, _)| *id == 0x74).collect();
    rec.compare("the handset transmitted ID 0x074 with the new gas halfword", commit_frames.last().map(|(id, p)| format!("0x{id:03x} {p}")), Some(format!("0x074 {:02x}{:02x}", ram_after[0] & 0xFF, ram_after[0] >> 8)), SOURCE, true, "payload is the little-endian halfword (Renode: 0x074 1600 for 22)");
    rec.compare("after the commit: selection, editing, active gas", Json::from_items([field(&committed, "selection"), field(&committed, "editing"), field(&committed, "activeGas")]), Json::from_items([0u64, 0, 1]), SOURCE, false, "gasCommit: editor closed; the active-gas index differs with the profile");

    // ---- Back: Down x6, exact simultaneity does nothing, staggered Confirm leaves ------------------------------------
    for _ in 0..6 {
        press(&mut rig, "down")?;
    }
    let at_back = diluent_view(&rig);
    rec.check("six Downs select Back (row 6)", field(&at_back, "selection") == 6, at_back.clone());
    press_simultaneous(&mut rig)?;
    let back_simultaneous = diluent_view(&rig);
    rec.compare("Press 3 on Back: screen, selection, editing stay", Json::from_items([u64::from(rig.screen()), field(&back_simultaneous, "selection"), field(&back_simultaneous, "editing")]), Json::from_items([0x17u64, 6, 0]), SOURCE, true, "retainedSimultaneousBack: the exact-simultaneous gesture still reaches key 0x33");
    press(&mut rig, "confirm")?;
    rec.compare("staggered Confirm on Back returns to User pref (screen id)", rig.screen(), 8u32, SOURCE, true, "newConfirmBack");
    rec.check("no CPU faults", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    let png = rig.png();
    rec.image("user-pref.png", png);

    // ---- persistence: Restart and walk again --------------------------------------------------------------------------
    let new_gas = ram_after[0];
    rig.act("{\"action\":\"reset\"}")?;
    walk_to_diluent(&mut rig, &mut rec, "after Restart")?;
    let (ram_restart, _) = gases(&rig);
    rec.compare("DIL 1 after Restart (main RAM)", ram_restart[0], new_gas, SOURCE, true, "restartPersistence: the gas survives in the EEPROM");
    let snapshot = gas_snapshot(&rig.can_trace());
    let first = snapshot.as_ref().and_then(|s| s.iter().find(|(id, _)| *id == 0x74)).map(|(_, p)| p.clone());
    rec.compare("main supplied ID 0x074 with the retained gas after Restart", first, Some(format!("{:02x}{:02x}", new_gas & 0xFF, new_gas >> 8)), SOURCE, true, "restartGasFrames: 0x074 1600");
    rec.file("can-trace.tsv", rig.can_trace().into_bytes());
    rec.limitation("The 50 ms stagger of Confirm is a functional fixture so that the firmware sees two individual key events inside its combination window; it is not a measured switch skew. The scenario does not claim the physical handset behaves identically.");
    rec.limitation("Renode's evidence used a copied default profile; this scenario starts from a fresh profile (every gas 21/00, as the Renode profile's DIL 1 did) so only the active-gas index differs; relations (editor opens, commit frame equals RAM and EEPROM, Back works, persistence) are what is compared.");
    Ok(rec.finish(env))
}
