//! `cold-wake`: a cold boot (zero wake flags) of a fresh profile runs until the original firmware requests standby, which
//! the host observes (main `PWR.CR1` LPMS = 3 with `SCB.SCR.SLEEPDEEP`) and answers by halting both CPUs; Wake then restarts
//! with the handset-wake fixture and the storage written by the cold boot (`emulation/main-boot/runner-cold-wake/result.json`,
//! `automatic-result.json`, `emulation/main-boot/standby-wake.md`).

use super::*;
use crate::fixtures;
use crate::system::BootMode;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/main-boot/runner-cold-wake/result.json";
const SOURCE_AUTO: &str = "recorded by the Renode runner in the analysis workspace, emulation/main-boot/runner-cold-wake/automatic-result.json";

fn hardware(rig: &Rig) -> Json {
    let main = rig.board(Which::Main);
    let handset = rig.board(Which::Handset);
    Json::object()
        .with("wakeCause", u64::from(rig.u8(Which::Main, fixtures::MAIN_WAKE_CAUSE_ADDRESS)))
        .with("mode", u64::from(rig.u8(Which::Main, fixtures::MAIN_MODE_ADDRESS)))
        .with("screenState", u64::from(rig.u8(Which::Main, fixtures::MAIN_SCREEN_MODE_ADDRESS)))
        .with("batteryReady", u64::from(rig.u8(Which::Main, fixtures::MAIN_BATTERY_READY_ADDRESS)))
        .with("halTick", u64::from(rig.u32(Which::Main, fixtures::MAIN_HAL_TICK_ADDRESS)))
        .with("pwrCR1", u64::from(rig.u32(Which::Main, fixtures::PWR_CR1_ADDRESS)))
        .with("pwrSR1", u64::from(rig.u32(Which::Main, 0x4000_7010)))
        .with("scbSCR", u64::from(rig.u32(Which::Main, fixtures::SCB_SCR_ADDRESS)))
        .with("mainPE3Output", u64::from(rig.u32(Which::Main, fixtures::PE3_ODR_ADDRESS)))
        .with("mainHalted", main.cpu.is_halted())
        .with("handsetHalted", handset.cpu.is_halted())
        .with("eepromValidity", u64::from(rig.session.system().main.as_ref().and_then(|m| m.eeprom.get_byte(254).ok()).unwrap_or(0)))
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("cold-wake");

    // ---- paused cold boot ------------------------------------------------------------------------------------------------
    let config = SessionConfig { boot_mode: BootMode::Cold, start_paused: true, ..SessionConfig::default() };
    let mut rig = Rig::new(env, config, Profile::default())?;
    let state = rig.state();
    rec.step("cold start (paused)", &state, Json::object());
    rec.compare("initial state: boot mode, time, running, handset gate", Json::from_items([state.get("bootMode").cloned().unwrap_or(Json::Null), state.get("virtualTime").cloned().unwrap_or(Json::Null), state.get("running").cloned().unwrap_or(Json::Null), state.get("handsetPowered").cloned().unwrap_or(Json::Null)]), Json::from_items([Json::from("cold"), Json::from(0.0f64), Json::from(false), Json::from(false)]), SOURCE, true, "paused, handset held by the PE3 gate");
    rec.compare("initial main PC is the reset vector", state.get("mainPC").and_then(Json::as_u64), Some(134_353_848u64), SOURCE, true, "0x080213B8");
    rec.compare("initial handset PC is its reset vector", state.get("pc").and_then(Json::as_u64), Some(134_251_536u64), SOURCE, true, "0x08008410");
    rec.compare("initial LCD summary (not initialised)", state.get("lcdSummary").and_then(Json::as_str).map(str::to_string), Some("240x320; panelOn=False; sleeping=True; MADCTL=0x00; COLMOD=0x05; commands=0; data=0; pixels=0; nonBlackGRAM=0; reads=0; TE=0".to_string()), SOURCE, true, "");
    rec.check("wake fixture not applied on a cold boot (PWR.SR1 = 0)", rig.u32(Which::Main, 0x4000_7010) == 0, u64::from(rig.u32(Which::Main, 0x4000_7010)));
    let state = rig.advance(2.0)?;
    rec.step("cold standby", &state, Json::object().with("hardware", hardware(&rig)));
    rec.compare("observed standby time (virtual s)", state.get("standbyTime").and_then(Json::as_f64), Some(1.55f64), SOURCE, true, "");
    rec.check("standby: both CPUs halted, not running, handset unpowered", state.get("standby") == Some(&Json::Bool(true)) && state.get("running") == Some(&Json::Bool(false)) && state.get("handsetPowered") == Some(&Json::Bool(false)), state.get("standby").cloned().unwrap_or(Json::Null));
    let hardware_cold = hardware(&rig);
    rec.compare(
        "cold hardware readbacks (wake cause, mode, screen, battery ready, HAL tick, PWR.CR1, PWR.SR1, SCB.SCR, PE3, halted flags, EEPROM marker)",
        hardware_cold.clone(),
        Json::object()
            .with("wakeCause", 0u64)
            .with("mode", 0u64)
            .with("screenState", 0u64)
            .with("batteryReady", 1u64)
            .with("halTick", 1513u64)
            .with("pwrCR1", 771u64)
            .with("pwrSR1", 0u64)
            .with("scbSCR", 4u64)
            .with("mainPE3Output", 8u64)
            .with("mainHalted", true)
            .with("handsetHalted", true)
            .with("eepromValidity", 163u64),
        SOURCE,
        true,
        "cause 0: the firmware clears the battery configuration and requests standby",
    );
    rec.compare("cold main PC", state.get("mainPC").and_then(Json::as_u64), Some(134_379_172u64), SOURCE, true, "0x080276A4, the FreeRTOS idle loop");
    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("cold CAN traffic (transmitted, last identifier)", Json::object().with("transmitted", summary_number(&can, "transmitted")).with("lastId", summary_field(&can, "lastId").map(str::to_string)), Json::object().with("transmitted", 1u64).with("lastId", "0x154"), SOURCE, true, "");
    let adc = state.get("adcSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("cold ADC (conversions, sequences, calibrations, last channel)", Json::from_items([summary_number(&adc, "conversions"), summary_number(&adc, "sequences"), summary_number(&adc, "calibrations"), summary_number(&adc, "lastChannel")]), Json::from_items([15_464u64, 2_577, 1, 2]), SOURCE, true, "");
    let storage = state.get("storageSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("cold EEPROM write count", summary_number(&storage, "writes"), Some(195u64), SOURCE, true, "first initialisation of the settings");
    rec.compare("serial number of an erased EEPROM", state.get("serialNumber").and_then(Json::as_u64), Some(4_294_967_295u64), SOURCE, true, "");
    let png = rig.png();
    rec.image("cold-standby.png", png);
    let faults_ok = rig.faults_clear();
    rec.check("no CPU faults at standby", faults_ok, Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));

    // ---- Resume is refused in standby; Step and Advance do not move time ------------------------------------------------
    let refused = rig.act("{\"action\":\"resume\"}");
    rec.compare("Resume in standby is refused with the runner's message", refused.err(), Some("The firmware requested standby; use Wake system".to_string()), "emulation/run_emulator.py Emulator.action", true, "");
    let before = rig.session.virtual_ns();
    rig.act("{\"action\":\"step\"}")?;
    rig.advance(1.0)?;
    rec.expect("Step and Advance do not advance a system in standby", rig.session.virtual_ns(), before);

    // ---- Wake ------------------------------------------------------------------------------------------------------------------
    let state = rig.act("{\"action\":\"wake\"}")?;
    rec.step("wake start", &state, Json::object());
    rec.compare("wake start: boot mode, time, handset gate, standby", Json::from_items([state.get("bootMode").cloned().unwrap_or(Json::Null), state.get("virtualTime").cloned().unwrap_or(Json::Null), state.get("handsetPowered").cloned().unwrap_or(Json::Null), state.get("standby").cloned().unwrap_or(Json::Null)]), Json::from_items([Json::from("handset-wake"), Json::from(0.0f64), Json::from(false), Json::from(false)]), SOURCE, true, "");
    rec.compare("wake start: EEPROM write counter restarts and the settings survive (marker)", Json::from_items([summary_number(state.get("storageSummary").and_then(Json::as_str).unwrap_or(""), "writes"), Some(u64::from(rig.session.system().main.as_ref().and_then(|m| m.eeprom.get_byte(254).ok()).unwrap_or(0)))]), Json::from_items([0u64, 163]), SOURCE, true, "the runner recreates the machine, EEPROM content persists");
    let state = rig.advance(4.5)?;
    rec.step("wake (4.5 s)", &state, Json::object().with("hardware", hardware(&rig)));
    let hardware_wake = hardware(&rig);
    rec.compare(
        "wake hardware readbacks",
        hardware_wake.clone(),
        Json::object()
            .with("wakeCause", 3u64)
            .with("mode", 1u64)
            .with("screenState", 34u64)
            .with("batteryReady", 1u64)
            .with("halTick", 4504u64)
            .with("pwrCR1", 768u64)
            .with("pwrSR1", 260u64)
            .with("scbSCR", 0u64)
            .with("mainPE3Output", 8u64)
            .with("mainHalted", false)
            .with("handsetHalted", false)
            .with("eepromValidity", 163u64),
        SOURCE,
        true,
        "cause 3, surface mode 1, state 0x22",
    );
    rec.compare("wake: handset release time", state.get("handsetReleaseTime").and_then(Json::as_f64), Some(1.05f64), SOURCE, true, "");
    rec.compare("wake: LCD summary at 4.5 s", state.get("lcdSummary").and_then(Json::as_str).map(str::to_string), Some("320x240; panelOn=True; sleeping=False; MADCTL=0x60; COLMOD=0x05; commands=169; data=461246; pixels=460800; nonBlackGRAM=18179; reads=4; TE=189".to_string()), SOURCE, true, "initialised settings: a shorter first screen than a fresh profile");
    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("wake: CAN traffic (transmitted, last identifier)", Json::object().with("transmitted", summary_number(&can, "transmitted")).with("lastId", summary_field(&can, "lastId").map(str::to_string)), Json::object().with("transmitted", 48u64).with("lastId", "0x83"), SOURCE, true, "");
    let adc = state.get("adcSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("wake: ADC conversions", summary_number(&adc, "conversions"), Some(44_964u64), SOURCE, true, "");
    let flash = state.get("flashSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("wake: NOR pages, commands, read, programmed, erases", Json::from_items([summary_number(&flash, "pages"), summary_number(&flash, "commands"), summary_number(&flash, "read"), summary_number(&flash, "programmed"), summary_number(&flash, "erases")]), Json::from_items([4u64, 1790, 15_920, 2304, 5]), SOURCE, true, "");
    rec.compare("wake: main PC", state.get("mainPC").and_then(Json::as_u64), Some(134_379_182u64), SOURCE, false, "idle-loop phase differs between runs (within Renode's own envelope)");
    rec.compare("wake: EEPROM write counter", summary_number(state.get("storageSummary").and_then(Json::as_str).unwrap_or(""), "writes"), Some(10u64), SOURCE, true, "");
    rec.check("wake: frame ready, no faults", state.get("frameReady") == Some(&Json::Bool(true)) && rig.faults_clear(), state.get("frameReady").cloned().unwrap_or(Json::Null));
    let png = rig.png();
    rec.image("wake.png", png);
    rec.file("wake-can-trace.tsv", rig.can_trace().into_bytes());

    // ---- running (not paused): the automatic stop at the observed standby ------------------------------------------------
    let mut auto = Rig::new(env, SessionConfig { boot_mode: BootMode::Cold, ..SessionConfig::default() }, Profile::default())?;
    rec.check("automatic run: the session starts running", auto.session.running(), auto.session.running());
    let mut calls = 0;
    while auto.session.running() && calls < 400 {
        auto.session.run_for(0.05);
        calls += 1;
    }
    let state = auto.state();
    rec.step("automatic cold standby", &state, Json::object());
    rec.compare("automatic run: stopped by itself at standby (running, standby, standbyTime, time)", Json::from_items([state.get("running").cloned().unwrap_or(Json::Null), state.get("standby").cloned().unwrap_or(Json::Null), state.get("standbyTime").cloned().unwrap_or(Json::Null), state.get("virtualTime").cloned().unwrap_or(Json::Null)]), Json::from_items([Json::from(false), Json::from(true), Json::from(1.55f64), Json::from(1.55f64)]), SOURCE_AUTO, true, "the host observes the standby request after the 50 ms poll and takes the final snapshot");
    rec.compare("automatic run: main PC and CAN traffic equal the paused run", Json::from_items([state.get("mainPC").and_then(Json::as_u64).map(Json::from).unwrap_or(Json::Null)]), Json::from_items([134_379_172u64]), SOURCE_AUTO, true, "");
    let can = state.get("canSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.compare("automatic run: CAN transmitted", summary_number(&can, "transmitted"), Some(1u64), SOURCE_AUTO, true, "");
    let outcome = auto.session.run_for(1.0);
    rec.check("a stopped session does not advance (run_for returns without progress)", outcome.advanced_ns == 0 && outcome.standby && !outcome.running, outcome.advanced_ns);
    rec.limitation("Standby is observed on the host (PWR LPMS = 3 with SLEEPDEEP, polled every 50 virtual ms): full rail removal, wake and reset electrical behaviour are not modelled; PWR/FLASH/FMC are ArrayMemory stores.");
    Ok(rec.finish(env))
}
