//! `outputs-uart`: the passive HUD / vibrator / UART observation and the LED colour labels
//! (`emulation/probe_outputs.py` of the pinned scripts, `emulation/outputs-validation.md`, `emulation/console-map.md`,
//! `emulation/handset-outputs.md`; Renode evidence `emulation/runtime/outputs/20261007T113732983860Z/result.json`).
//!
//! The boot is the original firmware; the fixtures afterwards are explicit register stimuli through the system bus (HUD PWM
//! on main TIM4, the vibrator enable PB15 on the handset, a TIM15 capture edge, 17 000 bytes into the main UART4 transmit data
//! register), testing the observation plumbing only. The state document must not disturb the guest (reading it is
//! side-effect free), the UART capture keeps 16 KiB tails with exact hex and escaped text, and the colour labels persist.

use super::*;
use crate::persistence::LedColors;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/outputs/20261007T113732983860Z/result.json";
const UART4_TDR: u32 = 0x4000_4C28;

fn outputs(rig: &Rig) -> Vec<Json> {
    rig.state().get("hardwareOutputs").and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default()
}

fn output(rig: &Rig, id: &str) -> Json {
    outputs(rig).into_iter().find(|o| o.get("id").and_then(Json::as_str) == Some(id)).unwrap_or(Json::Null)
}

fn field_text(output: &Json, key: &str) -> String {
    output.get(key).map_or("null".to_string(), |v| v.to_string())
}

fn write(rig: &mut Rig, which: Which, address: u32, value: u32) {
    rig.session.system_mut().board_mut(which).expect("board").bus_write(address, Width::Word, value);
}

fn uart4(rig: &Rig) -> Json {
    rig.state().get("uartConsole").and_then(Json::as_array).and_then(|c| c.iter().find(|s| s.get("id").and_then(Json::as_str) == Some("main.uart4")).cloned()).unwrap_or(Json::Null)
}

/// One output against the Renode record: active, level, duty (within 1e-9) and the details text.
fn compare_output(rec: &mut Recorder, name: &str, ours: &Json, active: Option<bool>, level: Option<bool>, duty: Option<f64>, details: &str) {
    let duty_ours = ours.get("dutyPercent").and_then(Json::as_f64);
    let duty_ok = match (duty_ours, duty) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    };
    let ours_view = Json::object()
        .with("active", ours.get("active").cloned().unwrap_or(Json::Null))
        .with("level", ours.get("level").cloned().unwrap_or(Json::Null))
        .with("dutyPercent", duty_ours)
        .with("details", ours.get("details").cloned().unwrap_or(Json::Null));
    let renode_view = Json::object().with("active", active).with("level", level).with("dutyPercent", if duty_ok { duty_ours } else { duty }).with("details", details);
    rec.compare(name, ours_view, renode_view, SOURCE, true, "");
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("outputs-uart");
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;
    let early = outputs(&rig);
    rec.compare(
        "outputs before the firmware configures anything (backlight details text)",
        output(&rig, "handset-backlight").get("details").cloned().unwrap_or(Json::Null),
        Json::from("TIM2 CH2; CEN=0; CCR=4294967295; ARR=4294967295; pin/PWM configuration not ready or unsupported"),
        SOURCE,
        true,
        "reset snapshot of the Renode run",
    );
    rec.check("five hardware outputs exist from time zero", early.len() == 5, early.len() as u64);

    // ---- boot --------------------------------------------------------------------------------------------------------------------
    let state = rig.advance(4.5)?;
    rec.step("boot (4.5 s)", &state, Json::object());
    rec.check("frame ready, batteries ready, no error", state.get("frameReady") == Some(&Json::Bool(true)) && state.get("mainBatteryReady") == Some(&Json::Bool(true)) && state.get("error").is_some_and(Json::is_null), state.get("error").cloned().unwrap_or(Json::Null));
    rec.check("faults: none", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    let ids: Vec<String> = outputs(&rig).iter().filter_map(|o| o.get("id").and_then(Json::as_str).map(str::to_string)).collect();
    rec.compare("output ids in order", Json::from_items(ids.iter().map(String::as_str)), Json::from_items(["handset-backlight", "handset-vibrator", "main-hud-1", "main-hud-2", "main-hud-3"]), SOURCE, true, "");
    let channels: Vec<String> = state.get("uartConsole").and_then(Json::as_array).map(|c| c.iter().filter_map(|s| s.get("id").and_then(Json::as_str).map(str::to_string)).collect()).unwrap_or_default();
    rec.compare("UART channels in order", Json::from_items(channels.iter().map(String::as_str)), Json::from_items(["main.uart4", "main.usart1", "main.usart2", "main.uart5", "handset.usart3"]), SOURCE, true, "");
    let console = uart4(&rig);
    rec.compare("UART4 transmitted bytes at 4.5 s (first boot diagnostic console)", console.get("txBytes").and_then(Json::as_u64), Some(382u64), SOURCE, true, "genuine main console output: wake line, battery configuration, EEPROM size and littlefs diagnostics");
    let last_tx = console.get("lastTxVirtualTime").and_then(Json::as_f64).unwrap_or(f64::NAN);
    rec.compare(
        "UART4 time of the last transmitted byte (exact)",
        last_tx,
        2.8884f64,
        SOURCE,
        false,
        "Renode stamps `machine.ClockSource.CurrentValue`, a multiple of the 100 us quantum here; this engine stamps the board clock one quantum earlier for this byte. The comparison with the dual-wake Renode run (recorded with a reference harness that is not part of this repository) reported the same -100 us (envelope verdict), while the first two stamps (3.26866 ms, 1.0032 s) are exact",
    );
    if env.options.main_i2c_idle_high {
        // The recorded boot predates the I2C idle-high fixture, which changes when the main firmware prints its diagnostics.
        rec.note(format!("UART4 last-byte stamp {last_tx:.4} s is not compared with Renode's 2.8884 s: the I2C idle-high fixture changes the boot timing; run with --no-i2c-idle-high for the recorded start-up"));
    } else {
        rec.check("UART4 last-byte stamp is within one 100 us quantum of Renode's 2.8884 s", (last_tx - 2.8884).abs() <= 100e-6 + 1e-9, last_tx);
    }
    compare_output(&mut rec, "boot: handset backlight (TIM2 CH2)", &output(&rig, "handset-backlight"), Some(true), None, Some(15.151515151515152), "TIM2 CH2; CEN=1; CCR=15; ARR=99; sampled register duty");
    compare_output(
        &mut rec,
        "boot: handset vibrator (TIM15 CH1, PB15 enable)",
        &output(&rig, "handset-vibrator"),
        Some(false),
        Some(false),
        Some(51.0),
        "TIM15 CH1; CEN=1; CCR=51; ARR=100; MOE unmodeled; commanded enable, not motor current; sampled register duty; enable activations=1; last enable=1.371401 virtual s",
    );
    for n in 1..=3 {
        compare_output(&mut rec, &format!("boot: main HUD {n}"), &output(&rig, &format!("main-hud-{n}")), Some(false), None, Some(0.0), &format!("TIM4 CH{n}; CEN=0; CCR=0; ARR=1999; sampled register duty"));
    }
    let source = |output: &Json, key: &str| output.get(key).and_then(|history| history.get("source")).and_then(Json::as_str).map(str::to_string);
    let backlight = output(&rig, "handset-backlight");
    let vibrator = output(&rig, "handset-vibrator");
    let histories_ok = backlight.get("activity").is_some_and(Json::is_null)
        && backlight.get("pwmActivity").is_some_and(Json::is_null)
        && source(&vibrator, "activity").as_deref() == Some("gpio-enable-command")
        && source(&vibrator, "pwmActivity").as_deref() == Some("sampled-gated-pwm-command")
        && (1..=3).all(|n| {
            let hud = output(&rig, &format!("main-hud-{n}"));
            source(&hud, "activity").as_deref() == Some("sampled-pwm-command") && hud.get("pwmActivity").is_some_and(Json::is_null)
        });
    rec.check("output histories after the boot: backlight none, vibrator enable edges + sampled PWM, HUD sampled commands", histories_ok, Json::from_items(outputs(&rig).iter().filter_map(|o| o.get("activity").and_then(|a| a.get("eventCount")).cloned())));
    rec.check("LED colour labels default to unknown / white / red (HUD1 / HUD2 / HUD3)", ["unknown", "white", "red"].iter().enumerate().all(|(i, c)| output(&rig, &format!("main-hud-{}", i + 1)).get("color").and_then(Json::as_str) == Some(c)), Json::from_items(outputs(&rig).iter().filter_map(|o| o.get("color").cloned())));
    let png = rig.png();
    rec.image("boot.png", png);

    // ---- passive observation: the state document must not disturb the guest ----------------------------------------------------------
    let timers = |rig: &Rig| {
        Json::from_items([0x4000_0000u32, 0x4001_4000].map(|base| Json::from_items([0u32, 0xC, 0x10, 0x24].map(|offset| u64::from(rig.u32(Which::Handset, base + offset))))))
    };
    let (fingerprint, before) = (rig.session.system().fingerprint(), timers(&rig));
    for _ in 0..3 {
        let _ = rig.session.state_json();
        let _ = rig.session.system_mut().lcd_ppm();
    }
    rec.check("three state snapshots leave the whole system state bit-identical (fingerprint)", rig.session.system().fingerprint() == fingerprint, fingerprint);
    rec.compare("passive reads keep the PWM timers' CR1/DIER/SR/CNT", timers(&rig), before.clone(), SOURCE, true, "passiveTimerState before == after");
    rec.compare("TIM2/TIM15 CR1, DIER, SR (CNT is a phase)", Json::from_items([before.at(0).and_then(|t| t.at(0)).cloned().unwrap_or(Json::Null), before.at(0).and_then(|t| t.at(2)).cloned().unwrap_or(Json::Null), before.at(1).and_then(|t| t.at(2)).cloned().unwrap_or(Json::Null)]), Json::from_items([1u64, 1, 1]), SOURCE, true, "CEN set, update flag set in both");

    // ---- HUD fixture: main TIM4 PWM ------------------------------------------------------------------------------------------------------
    let cr1 = rig.u32(Which::Main, 0x4000_0800);
    write(&mut rig, Which::Main, 0x4000_0800, cr1 | 1);
    write(&mut rig, Which::Main, 0x4000_0820, 0x111);
    for (address, value) in [(0x4000_0834, 500), (0x4000_0838, 1000), (0x4000_083C, 0)] {
        write(&mut rig, Which::Main, address, value);
    }
    compare_output(&mut rec, "HUD fixture: HUD 1 (CCR 500 of ARR 1999)", &output(&rig, "main-hud-1"), Some(true), None, Some(25.012506253126567), "TIM4 CH1; CEN=1; CCR=500; ARR=1999; sampled register duty");
    compare_output(&mut rec, "HUD fixture: HUD 2 (CCR 1000)", &output(&rig, "main-hud-2"), Some(true), None, Some(50.025012506253134), "TIM4 CH2; CEN=1; CCR=1000; ARR=1999; sampled register duty");
    compare_output(&mut rec, "HUD fixture: HUD 3 (CCR 0)", &output(&rig, "main-hud-3"), Some(false), None, Some(0.0), "TIM4 CH3; CEN=1; CCR=0; ARR=1999; sampled register duty");

    // ---- vibrator fixtures: PB15 enable gate over the TIM15 command -------------------------------------------------------------------------
    let bdtr = rig.u32(Which::Handset, 0x4001_4044);
    write(&mut rig, Which::Handset, 0x4001_4044, bdtr | 0x8000);
    rec.compare("MOE (BDTR bit 15) is tagged, not stored: reads back 0", rig.u32(Which::Handset, 0x4001_4044), 0u32, SOURCE, true, "the stock timer does not implement MOE; the vibrator status is a commanded-drive observation");
    write(&mut rig, Which::Handset, 0x4800_0418, 0x8000);
    compare_output(&mut rec, "vibrator: PB15 enabled", &output(&rig, "handset-vibrator"), Some(true), Some(true), Some(51.0), "TIM15 CH1; CEN=1; CCR=51; ARR=100; MOE unmodeled; commanded enable, not motor current; sampled register duty; enable activations=2; last enable=4.500000 virtual s");
    write(&mut rig, Which::Handset, 0x4800_0428, 0x8000);
    compare_output(&mut rec, "vibrator: PB15 low", &output(&rig, "handset-vibrator"), Some(false), Some(false), Some(51.0), "TIM15 CH1; CEN=1; CCR=51; ARR=100; MOE unmodeled; commanded enable, not motor current; sampled register duty; enable activations=2; last enable=4.500000 virtual s");
    write(&mut rig, Which::Handset, 0x4800_0418, 0x8000);
    let cr1 = rig.u32(Which::Handset, 0x4001_4000);
    write(&mut rig, Which::Handset, 0x4001_4000, cr1 & !1);
    compare_output(&mut rec, "vibrator: timer disabled while PB15 is high", &output(&rig, "handset-vibrator"), Some(false), Some(true), Some(0.0), "TIM15 CH1; CEN=0; CCR=51; ARR=100; MOE unmodeled; commanded enable, not motor current; sampled register duty; enable activations=3; last enable=4.500000 virtual s");
    write(&mut rig, Which::Handset, 0x4001_4000, cr1);
    write(&mut rig, Which::Handset, 0x4001_4044, bdtr);
    write(&mut rig, Which::Handset, 0x4800_0428, 0x8000);

    // ---- a TIM15 capture edge: the telemetry must not read the capture register -------------------------------------------------------------
    let ccmr = rig.u32(Which::Handset, 0x4001_4018);
    write(&mut rig, Which::Handset, 0x4001_4018, (ccmr & !3) | 1);
    let dier = rig.u32(Which::Handset, 0x4001_400C);
    write(&mut rig, Which::Handset, 0x4001_400C, dier | 2);
    let timer15 = rig.session.system().handset.ids.timer15;
    {
        let board = rig.session.system_mut().board_mut(Which::Handset).expect("handset");
        board.set_input(timer15, 0, false);
        board.set_input(timer15, 0, true);
    }
    let flags_before = rig.u32(Which::Handset, 0x4001_4010);
    let capture = output(&rig, "handset-vibrator");
    let flags_after = rig.u32(Which::Handset, 0x4001_4010);
    rec.compare("capture mode: SR before and after the telemetry read", Json::from_items([u64::from(flags_before), u64::from(flags_after)]), Json::from_items([3u64, 3]), SOURCE, true, "captureRegisterFixture: CC1IF stays set (the stock CCR read would clear it)");
    rec.compare("capture mode: the output is unknown and says the CCR was not read", Json::from_items([capture.get("active").cloned().unwrap_or(Json::Null), Json::from(field_text(&capture, "details").contains("not read (capture)"))]), Json::from_items([Json::Null, Json::from(true)]), SOURCE, true, "");
    let _ = rig.session.system_mut().board_mut(Which::Handset).expect("handset").bus_read(0x4001_4034, Width::Word);
    rec.compare("positive control: a direct CCR read consumes CC1IF (SR)", rig.u32(Which::Handset, 0x4001_4010), 1u32, SOURCE, true, "afterDirectCcr");

    // ---- UART: the full byte range through the real TDR path -------------------------------------------------------------------------------------
    let before = uart4(&rig).get("txBytes").and_then(Json::as_u64).unwrap_or(0);
    for i in 0..17_000u32 {
        write(&mut rig, Which::Main, UART4_TDR, i & 255);
    }
    let console = uart4(&rig);
    let total = console.get("txBytes").and_then(Json::as_u64).unwrap_or(0);
    let hex = console.get("hex").and_then(Json::as_str).unwrap_or("");
    let tail: Vec<u8> = hex.split(' ').filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
    let expected_tail: Vec<u8> = (17_000u32 - 16_384..17_000).map(|i| (i & 255) as u8).collect();
    let expected_text: String = expected_tail.iter().map(|&b| if b == 9 || b == 10 || b == 13 || (32..127).contains(&b) { (b as char).to_string() } else { format!("\\x{b:02X}") }).collect();
    rec.compare("UART4: bytes transmitted by the burst", total - before, 17_000u64, SOURCE, true, "");
    rec.compare("UART4: total after the burst, retained tail, truncated", Json::from_items([Json::from(total), Json::from(tail.len() as u64), console.get("truncated").cloned().unwrap_or(Json::Null)]), Json::from_items([Json::from(17_382u64), Json::from(16_384u64), Json::from(true)]), SOURCE, true, "bounded 16 KiB tail with the total kept");
    rec.compare("UART4: the retained tail is exact (hex)", tail == expected_tail, true, SOURCE, true, "");
    rec.compare("UART4: text escapes every non-text byte", console.get("text").and_then(Json::as_str) == Some(expected_text.as_str()) && expected_text.contains("\\x00") && expected_text.contains("\\x1B") && expected_text.contains("\\xFF"), true, SOURCE, true, "tab, LF, CR and printable ASCII verbatim, everything else \\xNN");
    rec.compare("UART4: tail SHA-256", sha256_hex(&tail), "acf08e9aa91aa1a3437c1112304f0e65cbc33ca76be4382965b2a2b8a8a61cc9".to_string(), SOURCE, true, "");
    rec.compare("UART4: time of the last byte (virtual s)", console.get("lastTxVirtualTime").and_then(Json::as_f64), Some(4.5f64), SOURCE, true, "the bus writes happen at the paused time");

    // ---- LED colour labels: validation, persistence across Restart and reopen -----------------------------------------------------------------------
    rig.act("{\"action\":\"led-colors\",\"colors\":{\"main-hud-1\":\"red\",\"main-hud-2\":\"white\"}}")?;
    for bad in ["{\"action\":\"led-colors\"}", "{\"action\":\"led-colors\",\"colors\":{\"handset-backlight\":\"red\"}}", "{\"action\":\"led-colors\",\"colors\":{\"main-hud-1\":\"blue\"}}"] {
        rec.compare("an invalid colour mapping is refused", rig.act(bad).err(), Some("LED colors must map HUD channel IDs to unknown, red or white".to_string()), "emulation/run_emulator.py validated_led_colors", true, bad.to_string().as_str());
    }
    let epoch_before = rig.state().get("outputHistoryEpoch").cloned();
    rig.act("{\"action\":\"reset\"}")?;
    let after_reset = rig.state();
    rec.check(
        "Restart creates a new output-history epoch (\"<historyNonce>-<generation>\", generation 1 -> 2)",
        epoch_before.as_ref().and_then(Json::as_str) == Some("0-1") && after_reset.get("outputHistoryEpoch").and_then(Json::as_str) == Some("0-2"),
        after_reset.get("outputHistoryEpoch").cloned().unwrap_or(Json::Null),
    );
    rec.check("Restart keeps the colour assignments", output(&rig, "main-hud-1").get("color").and_then(Json::as_str) == Some("red") && output(&rig, "main-hud-2").get("color").and_then(Json::as_str) == Some("white"), Json::from_items(outputs(&rig).iter().filter_map(|o| o.get("color").cloned())));
    rec.compare("Restart recreates the UART capture history (txBytes of every channel)", Json::from_items(after_reset.get("uartConsole").and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(|s| s.get("txBytes").cloned())), Json::from_items([0u64, 0, 0, 0, 0]), SOURCE, true, "reset snapshot");
    let reset_output = output(&rig, "handset-vibrator");
    rec.compare("Restart resets the output history (vibrator details text)", reset_output.get("details").cloned().unwrap_or(Json::Null), Json::from("TIM15 CH1; CEN=0; CCR=65535; ARR=65535; MOE unmodeled; commanded enable, not motor current; pin/PWM configuration not ready or unsupported; enable activations=0; last enable=none"), SOURCE, true, "");
    let Rig { session } = rig;
    let profile = session.shutdown();
    let reopened = Rig::new(env, SessionConfig::default(), profile.clone())?;
    let colors = LedColors::from_file_text(profile.led_colors.as_deref().unwrap_or("{}"))?;
    rec.check("led-colors.json written by the session reads back", colors.get("main-hud-1") == "red" && colors.get("main-hud-2") == "white", profile.led_colors.clone().unwrap_or_default());
    rec.compare(
        "colour labels after reopening the profile (HUD1, HUD2, HUD3)",
        Json::from_items((1..=3).map(|n| output(&reopened, &format!("main-hud-{n}")).get("color").cloned().unwrap_or(Json::Null))),
        Json::from_items(["red", "white", "unknown"]),
        SOURCE,
        false,
        "the Renode run of that date had HUD3 unknown by default; the current runner defaults HUD2 white and HUD3 red (user-identified channels), which this engine follows",
    );
    rec.file("led-colors.json", profile.led_colors.unwrap_or_default().into_bytes());
    rec.limitation("The outputs are sampled register duty and commanded enable states, not LED brightness, motor current or physical pin measurements; the stock timer's TIM15 MOE gate is unmodeled.");
    rec.limitation("The `activity` / `pwmActivity` histories (PB15 enable edges, change-only samples of the PWM commands every 50 ms (HUD) and 20 ms (vibrator) at the system's quantum boundaries) are this engine's port of the current Renode runner's; the Renode recordings of this scenario predate them and have no histories to compare with.");
    Ok(rec.finish(env))
}
