//! `ngc-cli bench --dive`: the committed dive benchmark (DESIGN.md 16.3, `ngc::scenario::dive`).

use crate::args::Args;
use crate::common;
use emu_core::Json;
use ngc::firmware::Firmware;
use ngc::scenario::dive::{self, Depth, DiveConfig, DiveReport, DEPTH_20M, DEPTH_30M};
use ngc::scenario::ScenarioEnv;
use ngc::system::{BuildOptions, RoutineAccelMode};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

pub const USAGE: &str = "ngc-cli bench --dive [--dive-seconds S] [--dive-depths 20,30] [--no-idle-ff] [--no-routine-accel|--shadow-routine-accel]\n  \
    [--verify-routine-accel] [--no-i2c-idle-high] [--release ID] [--json out.json]\n  \
    The committed dive benchmark (DESIGN.md 16.3): builds a valid-tissue profile from scratch through firmware routes only\n  \
    (battery wizard, air calibration through the menu, a 150 s NaN dive that saves a decompression date, a +5 day main RTC\n  \
    checkpoint fixture, restart and recalibration), then dives at each depth (default 20 m and 30 m) for --dive-seconds\n  \
    (default 60) after the bubble check (--dive-png PREFIX writes the last LCD frame of each dive). The sessions are driven like the browser worker (10 virtual-ms slices). Reports the\n  \
    average speed, the speed of the main board's compute bursts and of the quiet periods, and prints the state digests of\n  \
    the checkpoints (the end of every stage and every 10 virtual seconds of a dive). --verify-routine-accel runs the whole\n  \
    benchmark with the routine acceleration on and off and requires identical digests and retire counts at every checkpoint\n  \
    (exit status 1 otherwise).";

fn depths(text: Option<&str>) -> Result<Vec<Depth>, String> {
    let Some(text) = text else { return Ok(vec![DEPTH_20M, DEPTH_30M]) };
    text.split(',')
        .map(|item| match item.trim() {
            "20" => Ok(DEPTH_20M),
            "30" => Ok(DEPTH_30M),
            other => Err(format!("--dive-depths takes 20 and/or 30 (got '{other}')")),
        })
        .collect()
}

fn mode_name(mode: RoutineAccelMode) -> &'static str {
    match mode {
        RoutineAccelMode::Off => "off",
        RoutineAccelMode::On => "on",
        RoutineAccelMode::Shadow => "shadow",
    }
}

fn execute(env: &ScenarioEnv<'_>, config: &DiveConfig) -> Result<DiveReport, String> {
    let started = Instant::now();
    let mut clock = move || started.elapsed().as_secs_f64();
    dive::run(env, config, &mut clock)
}

fn print_report(out: &mut dyn Write, report: &DiveReport) {
    let _ = writeln!(out, "-- dive benchmark: routine acceleration {}, idle fast-forward {} --", mode_name(report.config.routine_accel), if report.config.idle_fast_forward { "on" } else { "off" });
    for stage in &report.stages {
        let _ = writeln!(
            out,
            "profile stage {:<16} {:>6.1} virtual s in {:>6.2} s wall = {:>6.2}x   fingerprint {}",
            stage.name,
            stage.virtual_seconds,
            stage.wall_seconds,
            stage.virtual_seconds / stage.wall_seconds.max(1e-12),
            &stage.checkpoint.fingerprint[..16]
        );
    }
    for d in &report.dives {
        let (all, burst, quiet) = d.speeds();
        let _ = writeln!(
            out,
            "dive {}: average {:.2}x ({:.1} s in {:.2} s); bursts {:.2}x ({} steps, worst burst {:.2}x); quiet {:.2}x ({} steps); replaced {} calls / {} instructions; tissues finite {} (word {:#010x}, raw NDL {}){}",
            d.depth.name,
            all.factor(),
            all.virtual_seconds,
            all.wall_seconds,
            burst.factor(),
            burst.steps,
            d.worst_burst_factor(),
            quiet.factor(),
            quiet.steps,
            d.replaced_calls,
            d.replaced_instructions,
            d.tissues_finite,
            d.tissue_word,
            d.raw_ndl,
            if d.shadow_mismatches > 0 { format!("; SHADOW MISMATCHES {}", d.shadow_mismatches) } else { String::new() }
        );
        for c in &d.checkpoints {
            let _ = writeln!(out, "    {:<16} t {:>8.3} s  fingerprint {}  exact {:016x}/{:016x}", c.name, c.virtual_ns as f64 / 1e9, &c.fingerprint[..16], c.exact[0], c.exact[1]);
        }
    }
}

pub fn run(parsed: &Args, main: &Firmware, handset: &Firmware, routine_accel: RoutineAccelMode, out: &mut dyn Write) -> Result<i32, (bool, String)> {
    let usage = |message: String| (true, message);
    let failed = |message: String| (false, message);
    let dive_seconds = common::parse_f64("dive-seconds", parsed.value("dive-seconds"), 60.0).map_err(usage)?;
    if !(1.0..=3600.0).contains(&dive_seconds) {
        return Err(usage("--dive-seconds must be between 1 and 3600".to_string()));
    }
    let depths = depths(parsed.value("dive-depths")).map_err(usage)?;
    let verify = parsed.flag("verify-routine-accel");
    let idle_ff = !parsed.flag("no-idle-ff");
    let env = ScenarioEnv { main, handset, options: BuildOptions { main_i2c_idle_high: !parsed.flag("no-i2c-idle-high") }, routine_accel: true };
    let modes: Vec<RoutineAccelMode> = if verify { vec![RoutineAccelMode::On, RoutineAccelMode::Off] } else { vec![routine_accel] };
    let mut reports = Vec::new();
    for mode in modes {
        let config = DiveConfig { routine_accel: mode, idle_fast_forward: idle_ff, depths: depths.clone(), dive_seconds, keep_images: parsed.value("dive-png").is_some() };
        let report = execute(&env, &config).map_err(failed)?;
        print_report(out, &report);
        if let (Some(prefix), RoutineAccelMode::On | RoutineAccelMode::Shadow) = (parsed.value("dive-png"), mode) {
            for d in &report.dives {
                let path = format!("{prefix}-{}.png", d.depth.name.replace(' ', ""));
                std::fs::write(&path, &d.final_png).map_err(|e| failed(format!("cannot write {path}: {e}")))?;
                let _ = writeln!(out, "wrote {path}");
            }
        }
        reports.push(report);
    }
    let mut status = 0;
    let mut verification = None;
    if verify {
        let differences = reports[0].differences(&reports[1]);
        let identical = differences.is_empty();
        let _ = writeln!(
            out,
            "routine acceleration on/off: {} ({} checkpoints compared{})",
            if identical { "IDENTICAL" } else { "MISMATCH" },
            reports[0].checkpoints().len(),
            if identical { String::new() } else { format!("; {}", differences.join("; ")) }
        );
        for (on, off) in reports[0].dives.iter().zip(reports[1].dives.iter()) {
            let (a, b) = (on.speeds(), off.speeds());
            let _ = writeln!(
                out,
                "dive {}: average {:.2}x -> {:.2}x ({:.2}x faster), bursts {:.2}x -> {:.2}x ({:.2}x faster)",
                on.depth.name,
                b.0.factor(),
                a.0.factor(),
                a.0.factor() / b.0.factor(),
                b.1.factor(),
                a.1.factor(),
                a.1.factor() / b.1.factor()
            );
        }
        if !identical {
            status = 1;
        }
        verification = Some(Json::object().with("identical", identical).with("differences", Json::from_items(differences.iter().map(String::as_str))));
    }
    if reports.iter().any(|r| r.dives.iter().any(|d| d.shadow_mismatches > 0)) {
        status = 1;
    }
    if let Some(path) = parsed.value("json") {
        let mut json = Json::object()
            .with("engine", concat!("ngc-wasm/", env!("CARGO_PKG_VERSION")))
            .with("benchmark", "dive (DESIGN.md 16.3): valid-tissue dive from a profile built through firmware routes")
            .with("host", common::host_json())
            .with("runs", Json::from_items(reports.iter().map(DiveReport::to_json)));
        if let Some(verification) = verification {
            json.insert("routineAccelVerification", verification);
        }
        common::write_json(&PathBuf::from(path), &json).map_err(failed)?;
        let _ = writeln!(out, "result written to {path}");
    }
    Ok(status)
}
