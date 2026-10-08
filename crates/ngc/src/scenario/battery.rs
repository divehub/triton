//! `battery-setup`: the fresh-profile battery wizard driven through physical-pin pulses, the staggered confirm
//! gesture and the genuine handset-to-main CAN commits, then persistence across Restart and across closing and
//! reopening the profile (`emulation/probe_button_chord.py`, `emulation/runtime/button-chord/20261007T124402955263Z`,
//! `emulation/runtime/system-validation/restart-readback.json`).
//!
//! Sequence (virtual time as in the probe): 6.5 s boot to the B1 prompt; an 81.92 ms pulse (100 counts) does not move
//! the selection; Down selects the Alkaline row; Confirm (PE3 low, PE5 low 50 ms later, each 204.8 ms) selects it and
//! commits it (handset CAN `0x082`, payload `01`); the same for B2 (`0x083`); both main battery types become 1 and the
//! wizard exits; an exactly simultaneous `Press 3` is still available afterwards.

use super::dual_wake::battery_wizard;
use super::*;
use crate::system::Input;

// The expected values below are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/button-chord/20261007T124402955263Z/result.json";
const SOURCE_RESTART: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/system-validation/restart-readback.json";
/// Main RAM bytes of the two battery types (`mainTypes` of the probe).
const MAIN_B1_TYPE: u32 = 0x2000_2444;
const MAIN_B2_TYPE: u32 = 0x2000_2445;
const BUSY: &str = "A button pulse is already in progress";

fn buttons(state: &Json) -> (bool, bool, u64, u64, u64, u64) {
    let summary = state.get("buttonSummary").and_then(Json::as_str).unwrap_or("");
    (
        summary_field(summary, "PE3") == Some("True"),
        summary_field(summary, "PE5") == Some("True"),
        summary_number(summary, "pulses").unwrap_or(0),
        summary_number(summary, "releases").unwrap_or(0),
        summary_number(summary, "activeMask").unwrap_or(0),
        summary_number(summary, "pendingMask").unwrap_or(0),
    )
}

fn main_types(rig: &Rig) -> Json {
    Json::from_items([u64::from(rig.u8(Which::Main, MAIN_B1_TYPE)), u64::from(rig.u8(Which::Main, MAIN_B2_TYPE))])
}

/// An action followed by 0.65 virtual seconds (the probe's `move`).
fn step(rig: &mut Rig, rec: &mut Recorder, action: &str, label: &str) -> Result<Json, String> {
    rig.act(&format!("{{\"action\":\"{action}\"}}"))?;
    let state = rig.advance(0.65)?;
    let view = rig.view();
    let screen = rig.screen();
    let extra = Json::object().with("screen", u64::from(screen)).with("mainTypes", main_types(rig));
    let extra = if screen == 0x29 { extra.with("wizard", battery_wizard(rig, view)) } else { extra };
    rec.step(label, &state, extra);
    Ok(state)
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("battery-setup");
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;

    // ---- fresh profile: B1 prompt -------------------------------------------------------------------------------
    let state = rig.advance(6.5)?;
    let view = rig.view();
    let wizard = battery_wizard(&rig, view);
    rec.step("fresh-battery-ready", &state, Json::object().with("screen", u64::from(rig.screen())).with("wizard", wizard.clone()));
    rec.compare("screen at the fresh prompt", rig.screen(), 0x29u32, SOURCE, true, "battery wizard");
    rec.check("both batteries unset, B1 active", wizard.get("activeBank").and_then(Json::as_u64) == Some(1) && wizard.get("b1Type").and_then(Json::as_u64) == Some(255) && wizard.get("b2Type").and_then(Json::as_u64) == Some(255), wizard.clone());
    rec.check("main battery types are 0xFF", main_types(&rig) == Json::from_items([255u64, 255]), main_types(&rig));
    let eeprom_before = rig.session.system().main.as_ref().map(|m| m.eeprom.image().to_vec()).unwrap_or_default();

    // ---- a 100-count pulse is below the 150-count bound: nothing moves -------------------------------------------
    rig.session.system_mut().apply_input(&Input::Pulse { mask: 1, duration_us: 81_920 })?;
    let state = rig.advance(0.65)?;
    let wizard_after = battery_wizard(&rig, rig.view());
    rec.step("invalid-width-rejected", &state, Json::object().with("wizard", wizard_after.clone()));
    rec.compare("an 81.92 ms pulse leaves the wizard unchanged", wizard_after.clone(), wizard, SOURCE, true, "rejected by the original width filter");

    // ---- Down: Alkaline row -----------------------------------------------------------------------------------------
    step(&mut rig, &mut rec, "down", "b1-choice")?;
    let wizard = battery_wizard(&rig, rig.view());
    rec.compare("B1 choice: selection row", wizard.get("selection").and_then(Json::as_u64), Some(1u64), SOURCE, true, "Alkaline");
    rec.check("B1 choice: still in the selection phase", wizard.get("phase").and_then(Json::as_u64) == Some(0), wizard);

    // ---- staggered confirm: pin phases and busy rejection --------------------------------------------------------
    let initial = buttons(&rig.state());
    rig.act("{\"action\":\"confirm\"}")?;
    let busy = rig.session.system_mut().apply_input(&Input::Press { mask: 1 });
    rec.check("a second gesture is rejected before the first pin falls (busy)", busy.as_ref().err().is_some_and(|e| e.contains(BUSY)), busy.err().unwrap_or_default());
    let mut elapsed = 0.0f64;
    let mut phases = Vec::new();
    for (offset, pe3_high, pe5_high) in [(0.01f64, false, true), (0.06, false, false), (0.215, true, false), (0.27, true, true)] {
        rig.advance(offset - elapsed)?;
        elapsed = offset;
        let (pe3, pe5, pulses, releases, active, pending) = buttons(&rig.state());
        phases.push(
            Json::object()
                .with("offsetSeconds", offset)
                .with("virtualTime", rig.state().get("virtualTime").cloned().unwrap_or(Json::Null))
                .with("pe3High", pe3)
                .with("pe5High", pe5)
                .with("pulses", pulses)
                .with("releases", releases)
                .with("activeMask", active)
                .with("pendingMask", pending),
        );
        rec.compare(&format!("pins at +{offset} s of the confirm gesture (PE3 high, PE5 high)"), Json::from_items([pe3, pe5]), Json::from_items([pe3_high, pe5_high]), SOURCE, true, "PE3 low first, PE5 low 50 ms later, each 204.8 ms");
        rec.check(&format!("gesture counters at +{offset} s"), pulses == initial.2 + 1 && releases == initial.3 + u64::from(offset >= 0.27), Json::from_items([pulses, releases]));
        if (offset - 0.215).abs() < 1e-9 {
            let busy = rig.session.system_mut().apply_input(&Input::Press { mask: 1 });
            rec.check("a second gesture is rejected while the second pin is still held", busy.as_ref().err().is_some_and(|e| e.contains(BUSY)), busy.err().unwrap_or_default());
        }
    }
    rec.extra("pinPhases", Json::Array(phases));
    let state = rig.advance(0.65 - elapsed)?;
    let wizard = battery_wizard(&rig, rig.view());
    rec.step("b1-selected-by-chord", &state, Json::object().with("wizard", wizard.clone()));
    rec.compare("B1 selected by the staggered confirm (phase, b1Type)", Json::from_items([wizard.get("phase").and_then(Json::as_u64).unwrap_or(99), wizard.get("b1Type").and_then(Json::as_u64).unwrap_or(99)]), Json::from_items([1u64, 1]), SOURCE, true, "the two individual key events inside the 250-tick combination window");

    // ---- commit B1, choose and commit B2 -----------------------------------------------------------------------------
    step(&mut rig, &mut rec, "confirm", "b1-committed")?;
    let wizard = battery_wizard(&rig, rig.view());
    rec.compare("B1 committed: main type, active bank, phase", Json::from_items([u64::from(rig.u8(Which::Main, MAIN_B1_TYPE)), wizard.get("activeBank").and_then(Json::as_u64).unwrap_or(99), wizard.get("phase").and_then(Json::as_u64).unwrap_or(99)]), Json::from_items([1u64, 2, 0]), SOURCE, true, "");
    step(&mut rig, &mut rec, "down", "b2-choice")?;
    step(&mut rig, &mut rec, "confirm", "b2-selected-by-chord")?;
    let wizard = battery_wizard(&rig, rig.view());
    rec.compare("B2 selected (b2Type, phase)", Json::from_items([wizard.get("b2Type").and_then(Json::as_u64).unwrap_or(99), wizard.get("phase").and_then(Json::as_u64).unwrap_or(99)]), Json::from_items([1u64, 1]), SOURCE, true, "");
    step(&mut rig, &mut rec, "confirm", "both-batteries-committed")?;
    rec.compare("both main battery types after the commits", main_types(&rig), Json::from_items([1u64, 1]), SOURCE, true, "Alkaline in both banks");
    rec.compare("the wizard exits (screen id)", rig.screen(), 2u32, SOURCE, false, "any screen other than the wizard 0x29 counts");
    rec.check("the wizard screen was left", rig.screen() != 0x29, u64::from(rig.screen()));
    let commits: Vec<(u32, String)> = rig.handset_frames().into_iter().filter(|(id, payload)| matches!(id, 0x82 | 0x83) && payload == "01").collect();
    rec.compare("genuine handset commit frames (0x082, 0x083 with payload 01)", Json::from_items(commits.iter().map(|(id, _)| u64::from(*id))), Json::from_items([0x82u64, 0x83]), SOURCE, true, "");
    let stamps: Vec<f64> = rig
        .can_trace()
        .lines()
        .filter(|l| l.contains("ngc-handset.can1") && (l.contains("\t0x082\t01\t") || l.contains("\t0x083\t01\t")))
        .filter_map(|l| trace_stamp_seconds(l.split('\t').next().unwrap_or("")))
        .collect();
    rec.compare("commit frame stamps in virtual seconds (informational)", Json::from_items(stamps.iter().copied()), Json::from_items([8.7363134f64, 10.68629]), SOURCE, false, "stamps move with the CPU phase of the button timing; both commits happen within the same confirm gesture window");
    rec.check("no CPU faults after the sequence", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));

    // ---- exactly simultaneous Press 3 stays available ---------------------------------------------------------------
    let initial = buttons(&rig.state());
    rig.session.system_mut().apply_input(&Input::Press { mask: 3 })?;
    rig.advance(0.01)?;
    let (pe3, pe5, ..) = buttons(&rig.state());
    rec.compare("exactly simultaneous Press 3: both pins low together after 10 ms", Json::from_items([pe3, pe5]), Json::from_items([false, false]), SOURCE, true, "diagnostic gesture retained");
    rig.advance(0.64)?;
    let (pe3, pe5, pulses, releases, ..) = buttons(&rig.state());
    rec.check("Press 3 completes with both pins high again, one pulse and one release", pe3 && pe5 && pulses == initial.2 + 1 && releases == initial.3 + 1, Json::from_items([pulses, releases]));
    let png = rig.png();
    rec.image("batteries-committed.png", png);

    // ---- EEPROM persistence: export, then Restart --------------------------------------------------------------------
    let exported = rig.session.export_profile();
    let eeprom_after = exported.eeprom.clone().unwrap_or_default();
    let changed: Vec<u64> = eeprom_before.iter().zip(eeprom_after.iter()).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i as u64).collect();
    rec.check("the EEPROM image changed (settings were written)", !changed.is_empty() && eeprom_after.len() == 2048, Json::from_items(changed.iter().copied()));
    rec.extra("eepromChangedOffsets", Json::from_items(changed.iter().copied()));

    rig.act("{\"action\":\"reset\"}")?;
    let state = rig.advance(6.5)?;
    rec.step("after Restart (6.5 s)", &state, Json::object().with("screen", u64::from(rig.screen())).with("mainTypes", main_types(&rig)));
    rec.compare("battery types survive Restart (EEPROM)", main_types(&rig), Json::from_items([1u64, 1]), SOURCE_RESTART, true, "b1 0x01, b2 0x01");
    rec.check("no battery wizard after Restart", rig.screen() != 0x29, u64::from(rig.screen()));
    rec.check("main batteries ready after Restart", state.get("mainBatteryReady") == Some(&Json::Bool(true)), state.get("mainBatteryReady").cloned().unwrap_or(Json::Null));

    // ---- close and reopen the profile in a new session ----------------------------------------------------------------
    let profile = rig.session.shutdown();
    rec.check("the closed profile carries EEPROM, NOR, RTC checkpoint and inputs", profile.eeprom.is_some() && profile.nor.is_some() && profile.rtc_state.is_some() && profile.inputs.is_some(), Json::from_items(profile.files().iter().map(|(n, b)| Json::from(format!("{n} ({} bytes)", b.len())))));
    let mut reopened = Rig::new(env, SessionConfig::default(), profile.clone())?;
    let state = reopened.advance(6.5)?;
    rec.step("reopened profile (6.5 s)", &state, Json::object().with("screen", u64::from(reopened.screen())).with("mainTypes", main_types(&reopened)));
    rec.check("the reopened profile boots with both batteries configured", main_types(&reopened) == Json::from_items([1u64, 1]) && reopened.screen() != 0x29, main_types(&reopened));
    rec.file("eeprom.bin", profile.eeprom.unwrap_or_default());
    rec.file("rtc-state.json", profile.rtc_state.unwrap_or_default().into_bytes());
    let png = reopened.png();
    rec.image("reopened.png", png);
    rec.limitation("Pin pulses are synthetic (204.8 ms, 50 ms stagger); the stagger is a functional fixture, not a measured switch skew. Which EEPROM offsets hold the battery types is not asserted: the scenario reports the offsets that changed.");
    Ok(rec.finish(env))
}
