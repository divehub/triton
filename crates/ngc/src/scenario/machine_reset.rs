//! `machine-reset`: a Renode-style `Machine.Reset()` of one board, requested by the firmware (`AIRCR.SYSRESETREQ`) or
//! by the independent watchdog (expiry).
//!
//! Evidence compared:
//!
//! * a probe recorded on 2026-10-08 with the pinned Renode 1.17.0 runtime (the probe script `machine_reset_probe.py` and its
//!   `result.json` are not part of this repository; the expected values are embedded below): handset-only platform,
//!   `machine Reset` after 0.5 s. It shows what the reset keeps (SRAM, the
//!   `ArrayMemory` register stores such as PWR, the RTC backup registers, flash), what it resets (CPU, executed
//!   instruction counter, CAN and LCD controllers) and what the stock platform does afterwards (VTOR, SP and PC are read
//!   from address 0, nothing is mapped there, the core locks up with `UsageFault.INVSTATE`).
//! * `crates/stm32/tests/renode_timer/golden.txt`: machine resets of the watchdog carried out by Renode at the
//!   synchronization point after the request (the `resets` records).
//!
//! The engine's default start state after a reset is the application vector table (`ResetStart::ApplicationVectors`), which
//! stands in for the manufacturer bootloader that the supplied images omit; the Renode-literal start (`ResetStart::
//! RenodeLiteral`) is available for the comparison and approximates the lockup with a halted core.

use super::*;
use crate::system::{round_up_to_quantum, Mode, ResetStart};
use emu_core::QUANTUM;
use stm32::iwdg::Iwdg;

const SOURCE: &str = "Renode 1.17.0 machine-reset probe of 2026-10-08 (script and result.json are not part of this repository)";
const SOURCE_GOLDEN: &str = "crates/stm32/tests/renode_timer/golden.txt";
const SRAM_MARKER: u32 = 0x2000_7000;
const PWR_MARKER: u32 = 0x4000_7020;
const RTC_BASE: u32 = 0x4000_2800;
const IWDG_BASE: u32 = 0x4000_3000;
const CAN_MCR: u32 = 0x4000_6400;

/// The register set the Renode probe read before and after the reset.
fn snapshot(rig: &Rig) -> Json {
    let which = Which::Handset;
    let board = rig.board(which);
    let system = rig.session.system();
    Json::object()
        .with("pc", u64::from(board.cpu.pc()))
        .with("sp", u64::from(board.cpu.reg(13)))
        .with("lr", u64::from(board.cpu.reg(14)))
        .with("vtor", u64::from(rig.u32(which, 0xE000_ED08)))
        .with("executed", system.instructions(which).unwrap_or(0))
        .with("sramMarker", u64::from(rig.u32(which, SRAM_MARKER)))
        .with("pwrMarker", u64::from(rig.u32(which, PWR_MARKER)))
        .with("rtcTR", u64::from(rig.u32(which, RTC_BASE)))
        .with("rtcDR", u64::from(rig.u32(which, RTC_BASE + 4)))
        .with("rtcBKP4", u64::from(rig.u32(which, RTC_BASE + 0x60)))
        .with("flashWord0", u64::from(rig.u32(which, 0x0800_4000)))
        .with("canMCR", u64::from(rig.u32(which, CAN_MCR)))
        .with("faultCFSR", u64::from(rig.u32(which, 0xE000_ED28)))
        .with("lcd", system.lcd_summary())
}

/// One record of the Renode probe: `(label, [(field, value, must, note)])`.
type Record = (&'static str, Vec<(&'static str, Json, bool, &'static str)>);

const LCD_RESET: &str = "240x320; panelOn=False; sleeping=True; MADCTL=0x00; COLMOD=0x05; commands=0; data=0; pixels=0; nonBlackGRAM=0; reads=0; TE=0";

fn renode_records() -> Vec<Record> {
    let n = |v: u64| Json::from(v);
    let retained = |pc: u64, sp: u64, lr: u64, cfsr: u64, can: u64| {
        vec![
            ("pc", n(pc), pc == 0, "after the reset the register is read back from address 0 (unmapped) or is the lockup vector"),
            ("sp", n(sp), sp == 0, ""),
            ("lr", n(lr), lr == 0, ""),
            ("vtor", n(0), true, "CortexM.Reset clears VTOR"),
            ("executed", n(0), true, "the executed-instruction counter restarts (tlib cpu_reset)"),
            ("sramMarker", n(0xC0FF_EE11), true, "MappedMemory.Reset() does nothing: SRAM keeps its content"),
            ("pwrMarker", n(0xABCD_0123), true, "ArrayMemory.Reset() does nothing: the PWR register store is kept"),
            ("rtcTR", n(0), true, "RTC calendar (the pinned script's RTC is at its reset value at 0.5 s)"),
            ("rtcDR", n(2_105_601), true, ""),
            ("rtcBKP4", n(0x1234_ABCD), true, "the RTC backup registers survive the reset"),
            ("flashWord0", n(0x2001_8000), true, "flash is untouched"),
            ("canMCR", n(can), true, "the CAN controller is reset to MCR = 0x00010002 (the .resc fixture is not re-run)"),
            ("faultCFSR", n(cfsr), cfsr == 0, "Renode locks up on the first fetch (UsageFault.INVSTATE); the engine holds the core instead"),
            ("lcd", Json::from(LCD_RESET), true, "the LCD controller and its counters are reset"),
        ]
    };
    vec![
        (
            "before reset (0.5 s)",
            vec![
                ("pc", n(134_271_116), false, "firmware phase at 0.5 s"),
                ("sp", n(536_874_504), false, "firmware phase at 0.5 s"),
                ("lr", n(134_274_409), false, "firmware phase at 0.5 s"),
                ("vtor", n(134_234_112), true, "VTOR 0x08004000"),
                ("executed", n(50_000_000), true, "100 MIPS x 0.5 s"),
                ("sramMarker", n(0xC0FF_EE11), true, "the fixture written by the probe"),
                ("pwrMarker", n(0xABCD_0123), true, "the fixture written by the probe"),
                ("rtcTR", n(0), true, ""),
                ("rtcDR", n(2_105_601), true, ""),
                ("rtcBKP4", n(0x1234_ABCD), true, "the fixture written by the probe"),
                ("flashWord0", n(0x2001_8000), true, "initial SP in flash"),
                ("canMCR", n(0x10010), false, "firmware-programmed (NART) on top of the .resc fixture"),
                ("faultCFSR", n(0), true, ""),
                ("lcd", Json::from("320x240; panelOn=True; sleeping=False; MADCTL=0x60; COLMOD=0x05; commands=60; data=230557; pixels=230400; nonBlackGRAM=0; reads=4; TE=12"), false, "firmware phase at 0.5 s"),
            ],
        ),
        ("right after machine Reset", retained(0, 0, 0, 0, 0x10002)),
        ("1 ms after reset", retained(0xEFFF_FFFE, 0xFFFF_FFE0, 0xFFFF_FFF9, 0x2_0000, 0x10002)),
        ("101 ms after reset", retained(0xEFFF_FFFE, 0xFFFF_FFE0, 0xFFFF_FFF9, 0x2_0000, 0x10002)),
    ]
}

/// Compares one engine snapshot with a Renode record, field by field.
fn compare_record(rec: &mut Recorder, label: &str, ours: &Json, record: &Record) {
    for (field, renode, must, note) in &record.1 {
        let value = ours.get(field).cloned().unwrap_or(Json::Null);
        rec.compare(&format!("{label}: {field}"), value, renode.clone(), SOURCE, *must, note);
    }
}

fn handset_only(env: &ScenarioEnv<'_>) -> Result<Rig, String> {
    let config = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    Ok(Rig { session: Session::new_with(env.options, "", config, None, env.handset, Profile::default())? })
}

fn mark(rig: &mut Rig, which: Which) {
    let board = rig.session.system_mut().board_mut(which).expect("board");
    assert!(board.poke(SRAM_MARKER, Width::Word, 0xC0FF_EE11));
    assert!(board.poke(PWR_MARKER, Width::Word, 0xABCD_0123));
    board.bus_write(RTC_BASE + 0x60, Width::Word, 0x1234_ABCD);
}

/// The firmware-side `SCB->AIRCR = VECTKEY | SYSRESETREQ`.
fn request_sysresetreq(rig: &mut Rig, which: Which) {
    let board = rig.session.system_mut().board_mut(which).expect("board");
    let now = board.now();
    board.cpu.ppb_poke32(0xE000_ED0C, 0x05FA_0004, now);
}

/// Arms the independent watchdog of a board to expire `reload * 125 us` after the call (prescaler /4 at 32 kHz).
fn start_watchdog(rig: &mut Rig, which: Which, reload: u32) {
    let board = rig.session.system_mut().board_mut(which).expect("board");
    for (offset, value) in [(0x0, 0x5555), (0x4, 0), (0x8, reload), (0x0, 0xAAAA), (0x0, 0xCCCC)] {
        board.bus_write(IWDG_BASE + offset, Width::Word, value);
    }
}

fn instructions(rig: &Rig, which: Which) -> u64 {
    rig.session.system().instructions(which).unwrap_or(0)
}

fn resets(rig: &Rig) -> usize {
    rig.session.system().reset_log().len()
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("machine-reset");
    let records = renode_records();

    // ---- 1. the Renode probe on the handset-only machine, Renode-literal start ---------------------------------------------------------
    let mut rig = handset_only(env)?;
    rig.session.system_mut().set_reset_start(ResetStart::RenodeLiteral);
    let state = rig.advance(0.5)?;
    mark(&mut rig, Which::Handset);
    let before = snapshot(&rig);
    rec.step("handset only, 0.5 s, markers written", &state, Json::object().with("registers", before.clone()));
    compare_record(&mut rec, "handset only", &before, &records[0]);
    let before_time = rig.session.virtual_ns();
    request_sysresetreq(&mut rig, Which::Handset);
    rig.advance(0.0001)?;
    let after = snapshot(&rig);
    let log = rig.session.system().reset_log().to_vec();
    rec.step("handset only, right after the reset", &rig.state(), Json::object().with("registers", after.clone()));
    rec.check("exactly one machine reset, of the handset, caused by SYSRESETREQ", log.len() == 1 && log[0].board == Which::Handset && log[0].cause.name() == "sysresetreq", Json::from_items(log.iter().map(|e| Json::from(format!("{} {} requested {} applied {}", e.board.name(), e.cause.name(), e.requested_at, e.applied_at)))));
    rec.check(
        "the reset is carried out at the end of the quantum in which it was requested",
        log.first().is_some_and(|e| e.applied_at == before_time + QUANTUM && e.requested_at >= before_time && e.requested_at <= e.applied_at),
        log.first().map(|e| e.applied_at.saturating_sub(before_time)),
    );
    compare_record(&mut rec, "right after machine Reset", &after, &records[1]);
    rig.advance(0.001)?;
    let one_ms = snapshot(&rig);
    compare_record(&mut rec, "1 ms after reset", &one_ms, &records[2]);
    rig.advance(0.1)?;
    let later = snapshot(&rig);
    compare_record(&mut rec, "101 ms after reset", &later, &records[3]);
    rec.check("virtual time keeps running through the reset and no error is raised", rig.session.error().is_none() && rig.session.virtual_ns() == before_time + QUANTUM + 1_000_000 + 100_000_000, rig.session.virtual_ns());
    rec.note("Renode literal start: the supplied images omit the manufacturer bootloader, so the stock platform has no vector table at address 0 and its core locks up (PC 0xEFFFFFFE, CFSR 0x20000, measured). The engine holds the core halted at PC = SP = VTOR = 0 instead of modelling the lockup; the firmware does not run again in either.");

    // ---- 2. the same on the dual system with the default start (application vectors): the firmware reboots -------------------------------------
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;
    rig.advance(3.0)?;
    mark(&mut rig, Which::Handset);
    let main_before = instructions(&rig, Which::Main);
    let handset_before = instructions(&rig, Which::Handset);
    let main_calendar = rig.session.system().rtc_checkpoint(Which::Main);
    let handset_calendar = rig.session.system().rtc_checkpoint(Which::Handset);
    rec.check("the handset UI is up before the reset", rig.session.system().lcd_summary().contains("panelOn=True") && handset_before > 90_000_000, handset_before);
    let t0 = rig.session.virtual_ns();
    request_sysresetreq(&mut rig, Which::Handset);
    rig.advance(0.0001)?;
    let event = rig.session.system().reset_log().first().copied();
    rec.check("dual: the handset SYSRESETREQ is applied at the end of its quantum", event.is_some_and(|e| e.board == Which::Handset && e.cause.name() == "sysresetreq" && e.applied_at == t0 + QUANTUM), event.map(|e| e.applied_at));
    rec.expect("dual: the handset's executed-instruction counter restarted", instructions(&rig, Which::Handset), 0);
    rec.check("dual: the main board kept executing (counter continues)", instructions(&rig, Which::Main) == main_before + 10_000, instructions(&rig, Which::Main) - main_before);
    rec.expect("dual: SRAM marker kept", rig.u32(Which::Handset, SRAM_MARKER), 0xC0FF_EE11);
    rec.expect("dual: PWR register-store marker kept", rig.u32(Which::Handset, PWR_MARKER), 0xABCD_0123);
    rec.expect("dual: RTC backup register kept", rig.u32(Which::Handset, RTC_BASE + 0x60), 0x1234_ABCD);
    rec.compare("dual: LCD controller reset", rig.session.system().lcd_summary(), LCD_RESET.to_string(), SOURCE, true, "same reset string as the Renode probe");
    let checkpoint = rig.session.system().rtc_checkpoint(Which::Handset);
    rec.check(
        "dual: the handset calendar continues (reset_keeps_rtc = true restores the checkpoint)",
        matches!((&handset_calendar, &checkpoint), (Some(a), Some(b)) if a.date_register == b.date_register && b.time_register >= a.time_register),
        checkpoint.map(|c| Json::object().with("TR", u64::from(c.time_register)).with("DR", u64::from(c.date_register))),
    );
    rec.note("dual: the CAN controller MCR is rewritten to 0x00010000 at the start (the .resc register fixture), unlike the stock Renode machine reset which leaves the reset value 0x00010002; the engine models the application hand-over of the omitted bootloader");
    rig.advance(0.001)?;
    let rebooted = rig.advance(3.0)?;
    rec.step("dual: 3 s after the handset reset", &rebooted, Json::object());
    rec.check("dual: the firmware boots again (LCD on, no error, no second reset)", rebooted.get("lcdSummary").and_then(Json::as_str).is_some_and(|s| s.contains("panelOn=True")) && rebooted.get("error").is_some_and(Json::is_null) && resets(&rig) == 1, rebooted.get("lcdSummary").cloned().unwrap_or(Json::Null));
    rec.check("dual: faults clear after the reboot", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    rec.expect("dual: machineResets in the state document", rebooted.get("machineResets").map(Json::len), Some(1));
    rec.check("dual: instruction counter restarted from the reset (about 3 s of 100 MIPS)", instructions(&rig, Which::Handset) > 290_000_000 && instructions(&rig, Which::Handset) < 310_000_000, instructions(&rig, Which::Handset));
    let png = rig.png();
    rec.image("handset-after-sysresetreq.png", png);
    let _ = main_calendar;

    // ---- 3. watchdog expiry on the handset -------------------------------------------------------------------------------------------------------------
    let iwdg_registers = |rig: &Rig, which: Which| {
        Json::object()
            .with("PR", u64::from(rig.u32(which, IWDG_BASE + 4)))
            .with("RLR", u64::from(rig.u32(which, IWDG_BASE + 8)))
            .with("WINR", u64::from(rig.u32(which, IWDG_BASE + 0x10)))
    };
    rec.note(format!(
        "watchdog registers set by the original firmware after boot: main {}, handset {}",
        iwdg_registers(&rig, Which::Main),
        iwdg_registers(&rig, Which::Handset)
    ));
    let first = resets(&rig);
    let t1 = rig.session.virtual_ns();
    start_watchdog(&mut rig, Which::Handset, 40);
    rig.advance(0.0049)?;
    rec.expect("watchdog: no reset before the 5 ms timeout", resets(&rig), first);
    rig.advance(0.0002)?;
    let log = rig.session.system().reset_log().to_vec();
    let event = log.last().copied();
    rec.check(
        "watchdog: expiry after reload 40 at 8 kHz requests a reset exactly 5 ms after the start",
        event.is_some_and(|e| e.board == Which::Handset && e.cause.name() == "iwdg" && e.requested_at == t1 + 5_000_000),
        event.map(|e| e.requested_at.wrapping_sub(t1)),
    );
    rec.compare(
        "watchdog: reset applied at the end of the quantum containing the request",
        event.map(|e| e.applied_at),
        event.map(|e| round_up_to_quantum(e.requested_at)),
        SOURCE_GOLDEN,
        true,
        "Renode RequestReset runs Machine.Reset at the next synchronization point; PER-D's renode_timer golden covers expiry and window violations",
    );
    rec.check("watchdog: a single reset and no error stop", resets(&rig) == first + 1 && rig.session.error().is_none(), resets(&rig));
    rec.expect("watchdog: the handset counter restarted", instructions(&rig, Which::Handset) < 1_000_000, true);
    let after_wd = rig.advance(3.0)?;
    rec.check("watchdog: the handset firmware boots again after the watchdog reset", after_wd.get("lcdSummary").and_then(Json::as_str).is_some_and(|s| s.contains("panelOn=True")) && after_wd.get("error").is_some_and(Json::is_null), after_wd.get("lcdSummary").cloned().unwrap_or(Json::Null));
    let watchdog = rig.session.system().handset.board.get::<Iwdg>(rig.session.system().handset.ids.iwdg).map(|w| w.reset_request_count());
    rec.check("watchdog: one request counted by the model", watchdog == Some(1), watchdog);

    // ---- 4. watchdog expiry on the main board; the handset and the CAN link carry on ------------------------------------------------------------------------
    let handset_before = instructions(&rig, Which::Handset);
    let main_calendar = rig.session.system().rtc_checkpoint(Which::Main);
    let frames_before = rig.session.system().link.trace_text().lines().count();
    let resets_before = resets(&rig);
    let t2 = rig.session.virtual_ns();
    start_watchdog(&mut rig, Which::Main, 80);
    rig.advance(0.0105)?;
    let log = rig.session.system().reset_log().to_vec();
    let event = log.last().copied();
    rec.check(
        "main watchdog: reset of the main board 10 ms after the start, applied at the quantum end",
        resets(&rig) == resets_before + 1 && event.is_some_and(|e| e.board == Which::Main && e.cause.name() == "iwdg" && e.requested_at == t2 + 10_000_000 && e.applied_at == round_up_to_quantum(e.requested_at)),
        event.map(|e| e.requested_at.wrapping_sub(t2)),
    );
    rec.check("main watchdog: the main instruction counter restarted", instructions(&rig, Which::Main) < 1_000_000, instructions(&rig, Which::Main));
    rec.check("main watchdog: the handset kept executing", instructions(&rig, Which::Handset) == handset_before + 105_000 * 10, instructions(&rig, Which::Handset) - handset_before);
    let calendar = rig.session.system().rtc_checkpoint(Which::Main);
    rec.check(
        "main watchdog: the main calendar continues across the reset",
        matches!((&main_calendar, &calendar), (Some(a), Some(b)) if a.date_register == b.date_register && b.time_register >= a.time_register && a.backup_registers == b.backup_registers),
        calendar.map(|c| Json::object().with("TR", u64::from(c.time_register)).with("DR", u64::from(c.date_register))),
    );
    let after_main = rig.advance(3.0)?;
    rec.step("main watchdog reset, then 3 s", &after_main, Json::object());
    rec.check("main watchdog: no error stop and no standby after the main reboot", after_main.get("error").is_some_and(Json::is_null) && after_main.get("standby") == Some(&Json::Bool(false)), after_main.get("error").cloned().unwrap_or(Json::Null));
    rec.check("main watchdog: the link kept carrying frames", rig.session.system().link.trace_text().lines().count() > frames_before, rig.session.system().link.trace_text().lines().count() - frames_before);
    rec.check("main watchdog: faults clear on both boards", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    rec.extra("machineResets", after_main.get("machineResets").cloned().unwrap_or(Json::Null));
    let main_text = rig.state().get("uartConsole").and_then(Json::as_array).and_then(|c| c.first().and_then(|s| s.get("text")).and_then(Json::as_str).map(str::to_string)).unwrap_or_default();
    rec.note(format!("main console tail after the watchdog reset: {}", main_text.chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect::<String>().replace('\r', "").replace('\n', " | ")));
    rec.limitation("The reset is modelled at the machine level only (peripheral reset values, kept memories, restarted counters). No reset-cause flags are set in RCC.CSR or PWR (Renode does not either), the IWDG itself restarts disabled, and the application start state stands in for the omitted bootloader. Physical reset behaviour of the device is not established.");
    Ok(rec.finish(env))
}
