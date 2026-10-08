//! `dual-wake`: both original images boot in the default handset-wake fixture; the handset is released by the
//! PE3 poll at 1.05 s, the CAN handshake runs and the handset shows the first battery prompt (B1).
//!
//! Renode evidence compared (all with the platform scripts of the analysis workspace, i.e. without the later I2C idle-high
//! fixture): `emulation/main-boot/dual-handset-wake/result.json` (4.5 s), `emulation/runtime/performance/
//! arithmetic-sustained-20261007/result.json` (5.5 s, LCD SHA-256 of the PPM) and
//! `emulation/runtime/button-chord/20261007T124402955263Z/result.json` (the fresh B1 prompt at 6.5 s), plus the REF
//! measurement of the executed handset instructions (`docs/renode-semantics.md` 5.6).

use super::*;
use crate::fixtures;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07) or measured in Renode
// 1.17.0 with a reference harness that is not part of this repository.
const SOURCE_WAKE: &str = "recorded by the Renode runner in the analysis workspace, emulation/main-boot/dual-handset-wake/result.json";
const SOURCE_PERF: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/performance/arithmetic-sustained-20261007/result.json";
const SOURCE_CHORD: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/button-chord/20261007T124402955263Z/result.json";
const SOURCE_REF: &str = "docs/renode-semantics.md section 5.6 (Renode 1.17.0 measurement)";

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("dual-wake");
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;

    // ---- 4.5 s: the state of the Renode dual-handset-wake probe -------------------------------------------------
    let state = rig.advance(4.5)?;
    rec.step("4.5 s", &state, Json::object());
    rec.expect("handset released by the 1.05 s power-gate poll", state.get("handsetReleaseTime").and_then(Json::as_f64), Some(1.05));
    rec.check("handset supply enabled", state.get("handsetPowered") == Some(&Json::Bool(true)), state.get("handsetPowered").cloned().unwrap_or(Json::Null));
    rec.check("main batteries ready", state.get("mainBatteryReady") == Some(&Json::Bool(true)), state.get("mainBatteryReady").cloned().unwrap_or(Json::Null));
    rec.check("no error, not in standby", state.get("error").is_some_and(Json::is_null) && state.get("standby") == Some(&Json::Bool(false)), state.get("error").cloned().unwrap_or(Json::Null));
    rec.check("both cores without faults (CFSR/HFSR)", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));

    let wake_cause = rig.u8(Which::Main, fixtures::MAIN_WAKE_CAUSE_ADDRESS);
    let screen_mode = rig.u8(Which::Main, fixtures::MAIN_SCREEN_MODE_ADDRESS);
    let main_mode = rig.u8(Which::Main, fixtures::MAIN_MODE_ADDRESS);
    let hal_tick = rig.u32(Which::Main, fixtures::MAIN_HAL_TICK_ADDRESS);
    rec.compare("main wake cause (handset wake)", wake_cause, 3u32, SOURCE_WAKE, true, "");
    rec.compare("main screen state", screen_mode, 0x22u32, SOURCE_WAKE, true, "");
    rec.compare("main mode (surface)", main_mode, 1u32, SOURCE_WAKE, true, "");
    rec.compare("main HAL tick at 4.5 s", hal_tick, 0x1198u32, SOURCE_WAKE, true, "999 us HAL ticks of the main TIM6");
    let marker = rig.session.system().main.as_ref().map(|m| m.eeprom.get_byte(254).unwrap_or(0)).unwrap_or(0);
    rec.compare("EEPROM validity marker written by the first boot", u32::from(marker), 0xA3u32, SOURCE_WAKE, true, "");

    let instructions = (rig.session.system().instructions(Which::Main), rig.session.system().instructions(Which::Handset));
    rec.compare("main executed instructions at 4.5 s", instructions.0, Some(450_000_000u64), SOURCE_REF, true, "100 MIPS x 4.5 s");
    rec.compare("handset executed instructions at 4.5 s", instructions.1, Some(344_990_000u64), SOURCE_REF, true, "the released handset starts one quantum late: (4.5 - 1.0501) s x 100 MIPS");

    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("CAN frames transmitted by 4.5 s", summary_number(&can, "transmitted"), Some(50u64), SOURCE_WAKE, true, "");
    rec.compare("last CAN identifier", summary_field(&can, "lastId").map(str::to_string), Some("0x83".to_string()), SOURCE_WAKE, true, "genuine handshake 0x173 -> 0x172 -> 0x82/0x83");
    let handshake: Vec<u32> = rig.handset_frames().iter().chain(rig.main_frames().iter()).map(|(id, _)| *id).filter(|id| matches!(id, 0x172 | 0x173 | 0x82 | 0x83)).collect();
    rec.check("the handshake 0x173, 0x172, 0x82, 0x83 appears on the link", [0x173, 0x172].iter().all(|id| handshake.contains(id)), Json::from_items(handshake.iter().map(|id| u64::from(*id))));
    let lcd = state.get("lcdSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare(
        "LCD summary at 4.5 s",
        lcd.clone(),
        "320x240; panelOn=True; sleeping=False; MADCTL=0x60; COLMOD=0x05; commands=241; data=615038; pixels=614400; nonBlackGRAM=18179; reads=4; TE=189".to_string(),
        SOURCE_WAKE,
        false,
        "TE counts the 120 Hz tear pulses since the panel was initialised; the CPU phase of the init differs slightly between Renode runs",
    );
    let flash = state.get("flashSummary").and_then(Json::as_str).unwrap_or("").to_string();
    for (key, renode) in [("pages", 4u64), ("commands", 1790), ("read", 15920), ("programmed", 2304), ("erases", 5)] {
        rec.compare(&format!("NOR {key} at 4.5 s"), summary_number(&flash, key), Some(renode), SOURCE_WAKE, false, "real filesystem traffic of the first boot into the functional NOR model");
    }
    rec.check("LCD frame is ready", state.get("frameReady") == Some(&Json::Bool(true)), lcd);

    // ---- 5.5 s: LCD bytes and counters of the sustained performance run -------------------------------------------
    let state = rig.advance(1.0)?;
    rec.step("5.5 s", &state, Json::object());
    let ppm = rig.session.system_mut().lcd_ppm().unwrap_or_default();
    rec.compare("LCD PPM SHA-256 at 5.5 s", sha256_hex(&ppm), "62c3a30e54031ff3c2aeffa16a6b9f361c64ab2326db35da7e345c5318f7632d".to_string(), SOURCE_PERF, true, "byte-identical frame to the Renode model");
    rec.compare("main HAL tick at 5.5 s", rig.u32(Which::Main, fixtures::MAIN_HAL_TICK_ADDRESS), 5505u32, SOURCE_PERF, true, "");
    rec.compare("main instructions at 5.5 s", rig.session.system().instructions(Which::Main), Some(550_000_000u64), SOURCE_PERF, true, "");
    rec.compare("handset instructions at 5.5 s", rig.session.system().instructions(Which::Handset), Some(444_990_000u64), SOURCE_PERF, true, "");
    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("CAN frames transmitted by 5.5 s", summary_number(&can, "transmitted"), Some(52u64), SOURCE_PERF, true, "");
    rec.compare("last CAN identifier at 5.5 s", summary_field(&can, "lastId").map(str::to_string), Some("0x10".to_string()), SOURCE_PERF, true, "");
    let adc = state.get("adcSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("main ADC conversions at 5.5 s", summary_number(&adc, "conversions"), Some(54_964u64), SOURCE_PERF, false, "DMA sequences of six 100 us ranks; the exact count depends on the CPU phase of the ADC start");
    rec.compare("main ADC sequences at 5.5 s", summary_number(&adc, "sequences"), Some(9_160u64), SOURCE_PERF, false, "");
    let lcd = state.get("lcdSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare(
        "LCD summary at 5.5 s",
        lcd,
        "320x240; panelOn=True; sleeping=False; MADCTL=0x60; COLMOD=0x05; commands=241; data=615038; pixels=614400; nonBlackGRAM=18179; reads=4; TE=249".to_string(),
        SOURCE_PERF,
        false,
        "",
    );
    rec.check("no faults at 5.5 s", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));

    // ---- 6.5 s: the fresh B1 prompt of the battery wizard ------------------------------------------------------------
    let state = rig.advance(1.0)?;
    let view = rig.view();
    let wizard = battery_wizard(&rig, view);
    rec.step("6.5 s: B1 prompt", &state, Json::object().with("screen", u64::from(rig.screen())).with("wizard", wizard.clone()));
    rec.compare("handset screen id (battery wizard)", rig.screen(), 0x29u32, SOURCE_CHORD, true, "");
    rec.compare("wizard view object address", view, 0x2000_562Cu32, SOURCE_CHORD, false, "the heap address of the TouchGFX view");
    rec.compare(
        "wizard state at the fresh prompt (active bank, types, done flags, phase, selection)",
        wizard.clone(),
        Json::object().with("activeBank", 1u64).with("b1Type", 255u64).with("b2Type", 255u64).with("b1Done", 0u64).with("b2Done", 0u64).with("phase", 0u64).with("selection", 0u64),
        SOURCE_CHORD,
        true,
        "B1 not yet chosen",
    );
    rec.check("main battery types are still unset (0xFF)", rig.u8(Which::Main, 0x2000_2444) == 0xFF && rig.u8(Which::Main, 0x2000_2445) == 0xFF, Json::from_items([u64::from(rig.u8(Which::Main, 0x2000_2444)), u64::from(rig.u8(Which::Main, 0x2000_2445))]));
    let png = rig.png();
    rec.image("b1-prompt.png", png);
    rec.file("can-trace.tsv", rig.can_trace().into_bytes());
    rec.limitation("Synthetic reproduction on a functional model: CAN timing, ADC circuit, pressure PROM and the standby/wake rails are fixtures; nothing here observes the physical device.");
    rec.limitation("After the handset joins at 1.05 s Renode's two CPU threads race (CAN stamps differ by up to ~80 us between two Renode runs); this engine's fixed order is one member of that envelope (docs/renode-semantics.md section 15).");
    Ok(rec.finish(env))
}

/// The battery wizard fields of the handset view (offsets recovered by `probe_button_chord.py`).
pub(super) fn battery_wizard(rig: &Rig, view: u32) -> Json {
    Json::object()
        .with("activeBank", u64::from(rig.u32(Which::Handset, view + 0x37DC)))
        .with("b1Type", u64::from(rig.u8(Which::Handset, view + 0x37E4)))
        .with("b2Type", u64::from(rig.u8(Which::Handset, view + 0x37EC)))
        .with("b1Done", u64::from(rig.u8(Which::Handset, view + 0x37E5)))
        .with("b2Done", u64::from(rig.u8(Which::Handset, view + 0x37ED)))
        .with("phase", u64::from(rig.u8(Which::Handset, view + 0x37F0)))
        .with("selection", u64::from(rig.u32(Which::Handset, view + 0x1C8 + 0x640)))
}
