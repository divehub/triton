//! `ngc-cli bench`: boot, steady-state and button-redraw benchmark of the dual system.
//!
//! Mirrors `emulation/performance/benchmark_system.py` (the Renode arithmetic-PWM fixture): fresh storage, the
//! default handset-wake boot with the 50 virtual-ms power-gate poll up to `--boot-seconds` (4.5 s), then
//! `--steady-samples` (3) steady intervals of `--seconds` (1 s) and finally the menu-redraw interval (two
//! 0.5 s intervals, each started by a physical Down / Up button press). Each measurement reports wall time,
//! virtual seconds per wall second, executed instructions per board, the idle fast-forward statistics and
//! a state digest. The wall time covers only the `run`/input calls, never the state readout.

use crate::args;
use crate::common;
use emu_core::{from_secs_f64, to_secs_f64, Json};
use ngc::sha256;
use ngc::system::{BoardCounters, Input, Mode, RoutineAccelMode, System, Which};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

pub const USAGE: &str = "ngc-cli bench [--main <srec>] [--handset <srec>] [--release ID] [--boot-seconds S] [--seconds S] [--steady-samples N]\n  \
    [--no-idle-ff] [--verify-idle-ff] [--no-routine-accel|--shadow-routine-accel] [--verify-routine-accel] [--no-i2c-idle-high]\n  \
    [--json out.json] [--no-menu]\n  \
    Fresh-storage dual benchmark: boot to --boot-seconds (default 4.5), then N steady intervals of --seconds\n  \
    (defaults 3 x 1 s), then a menu-redraw interval (Down / Up button presses, 2 x 0.5 s). Prints wall seconds,\n  \
    virtual seconds per wall second, instructions and idle-skip statistics per interval.\n  \
    --verify-idle-ff runs the whole benchmark twice (idle fast-forward on and off) and requires identical state\n  \
    digests (guest state and output activity histories, and the exactness digest of each core: registers, retire counts,\n  \
    predecode cache and cut-block history) at every measurement point.\n  \
    --release ID selects the default SREC directory (TRITON-5.8-65.3 or NEPTUN-5.8-65.3); --no-i2c-idle-high leaves the\n  \
    main board's I2C idle inputs low (the idle-high fixture is on by default).\n  \
    --no-routine-accel turns off the exact routine acceleration (memoized soft-float library calls, on by default),\n  \
    --shadow-routine-accel selects its verification mode, --verify-routine-accel runs the whole benchmark with it on and off and\n  \
    requires identical state digests (state, FPSCR/VFP registers, predecode cache) at every measurement point.\n  \
    --dive [--dive-seconds S] [--dive-depths 20,30] [--dive-png PREFIX] runs the committed dive benchmark instead (valid tissues, profile built\n  \
    through firmware routes; average / burst / quiet speed and checkpoint digests; `ngc-cli bench --dive --help`). The options of\n  \
    one benchmark are refused by the other.";

struct Measurement {
    label: String,
    requested: f64,
    elapsed: f64,
    wall: f64,
    runs: u32,
    buttons: Vec<String>,
    instructions: [u64; 2],
    counters: [Option<BoardCounters>; 2],
    before: [Option<BoardCounters>; 2],
    /// `System::fingerprint`: the guest state plus the output activity histories.
    digest: String,
    /// `System::guest_fingerprint`: the guest state alone (what an observation change must not alter).
    guest_digest: String,
    /// `Cpu::exactness_digest` of the main and handset cores: the architectural state including FPSCR and the VFP
    /// registers, the predecode cache and the cut-block history (what routine acceleration must not alter).
    exact: [u64; 2],
    lcd_sha256: Option<String>,
    can_trace_sha256: String,
    pcs: [Option<u32>; 2],
    battery_ready: Option<bool>,
    hal_tick: Option<u32>,
    main_mode: Option<u32>,
    faults_clear: bool,
}

impl Measurement {
    fn factor(&self) -> f64 {
        if self.wall > 0.0 {
            self.elapsed / self.wall
        } else {
            f64::INFINITY
        }
    }

    fn skipped(&self, index: usize) -> u64 {
        let now = self.counters[index].map_or(0, |c| c.fast_forward.skipped_instructions);
        let before = self.before[index].map_or(0, |c| c.fast_forward.skipped_instructions);
        now - before
    }

    fn loops(&self, index: usize) -> u64 {
        let now = self.counters[index].map_or(0, |c| c.fast_forward.loops);
        let before = self.before[index].map_or(0, |c| c.fast_forward.loops);
        now - before
    }

    fn failed(&self, index: usize) -> u64 {
        let now = self.counters[index].map_or(0, |c| c.fast_forward.failed_verifications);
        let before = self.before[index].map_or(0, |c| c.fast_forward.failed_verifications);
        now - before
    }

    fn slices(&self, index: usize) -> u64 {
        self.counters[index].map_or(0, |c| c.slices) - self.before[index].map_or(0, |c| c.slices)
    }

    fn to_json(&self) -> Json {
        let mut idle = Json::object();
        let mut executed = Json::object();
        for (index, which) in [Which::Main, Which::Handset].into_iter().enumerate() {
            executed.insert(which.name(), self.instructions[index]);
            idle.insert(
                which.name(),
                Json::object()
                    .with("skippedInstructions", self.skipped(index))
                    .with("skippedShare", if self.instructions[index] > 0 { self.skipped(index) as f64 / self.instructions[index] as f64 } else { 0.0 })
                    .with("loops", self.loops(index))
                    .with("failedVerifications", self.failed(index))
                    .with("slices", self.slices(index)),
            );
        }
        Json::object()
            .with("label", self.label.as_str())
            .with("requestedVirtualSeconds", self.requested)
            .with("elapsedVirtualSeconds", self.elapsed)
            .with("wallSeconds", self.wall)
            .with("virtualSecondsPerWallSecond", self.factor())
            .with("runForCalls", u64::from(self.runs))
            .with("buttonEvents", Json::from_items(self.buttons.iter().map(String::as_str)))
            .with("executedInstructions", executed)
            .with("idleFastForward", idle)
            .with("stateDigest", self.digest.as_str())
            .with("guestStateDigest", self.guest_digest.as_str())
            .with("lcdSha256", self.lcd_sha256.as_deref())
            .with("canTraceSha256", self.can_trace_sha256.as_str())
            .with("mainPC", self.pcs[0].map(u64::from))
            .with("handsetPC", self.pcs[1].map(u64::from))
            .with("mainBatteryReady", self.battery_ready)
            .with("mainHalTick", self.hal_tick.map(u64::from))
            .with("mainMode", self.main_mode.map(u64::from))
            .with("faultRegistersClear", self.faults_clear)
    }
}

struct Report {
    boot_seconds: f64,
    boot_wall: f64,
    boot_digest: String,
    boot_guest_digest: String,
    boot_exact: [u64; 2],
    boot_counters: [Option<BoardCounters>; 2],
    release_seconds: Option<f64>,
    measurements: Vec<Measurement>,
}

fn counters(system: &System) -> [Option<BoardCounters>; 2] {
    [system.counters(Which::Main), system.counters(Which::Handset)]
}

fn exact_digests(system: &System) -> [u64; 2] {
    [system.exactness_digest(Which::Main).unwrap_or(0), system.exactness_digest(Which::Handset).unwrap_or(0)]
}

fn accel_name(mode: RoutineAccelMode) -> &'static str {
    match mode {
        RoutineAccelMode::Off => "off",
        RoutineAccelMode::On => "on",
        RoutineAccelMode::Shadow => "shadow",
    }
}

#[allow(clippy::too_many_arguments)]
fn measure(system: &mut System, label: &str, chunks: &[f64], press: bool) -> Measurement {
    let before = counters(system);
    let before_time = system.time();
    let started = Instant::now();
    let mut buttons = Vec::new();
    for (index, seconds) in chunks.iter().enumerate() {
        if press {
            let up = index % 2 == 1;
            match system.apply_input(&Input::Navigate { up }) {
                Ok(()) => buttons.push(format!("{} accepted at {:.4} s", if up { "up" } else { "down" }, system.seconds())),
                Err(error) => buttons.push(format!("{} failed: {error}", if up { "up" } else { "down" })),
            }
        }
        system.run_for(from_secs_f64(*seconds));
    }
    let wall = started.elapsed().as_secs_f64();
    let after = counters(system);
    let instructions = [0, 1].map(|i| after[i].map_or(0, |c| c.instructions) - before[i].map_or(0, |c| c.instructions));
    let app = system.main_application();
    let faults_clear = [Which::Main, Which::Handset].iter().all(|which| {
        system.fault_registers(*which).is_none_or(|faults| faults.iter().filter(|(name, _)| *name != "ICSR").all(|(_, value)| *value == 0))
    });
    Measurement {
        label: label.to_string(),
        requested: chunks.iter().sum(),
        elapsed: to_secs_f64(system.time() - before_time),
        wall,
        runs: chunks.len() as u32,
        buttons,
        instructions,
        counters: after,
        before,
        digest: system.fingerprint(),
        guest_digest: system.guest_fingerprint(),
        exact: exact_digests(system),
        lcd_sha256: system.lcd_ppm().map(|ppm| sha256::digest_hex(&ppm)),
        can_trace_sha256: sha256::digest_hex(system.link.trace_text().as_bytes()),
        pcs: [system.pc(Which::Main), system.pc(Which::Handset)],
        battery_ready: system.main_battery_ready_flag(),
        hal_tick: app.and_then(|a| a.hal_tick),
        main_mode: app.and_then(|a| a.main_mode),
        faults_clear,
    }
}

fn run_bench(
    main: &ngc::firmware::Firmware,
    handset: &ngc::firmware::Firmware,
    idle_ff: bool,
    routine_accel: RoutineAccelMode,
    i2c_idle_high: bool,
    boot_seconds: f64,
    seconds: f64,
    samples: u32,
    menu: bool,
    out: &mut dyn Write,
) -> Result<Report, String> {
    let mut system = common::build_system(Mode::Dual, ngc::system::BootMode::HandsetWake, false, idle_ff, routine_accel, i2c_idle_high, Some(main), handset)?;
    let _ = writeln!(out, "-- idle fast-forward {}, routine acceleration {} --", if idle_ff { "on" } else { "off" }, accel_name(routine_accel));
    let started = Instant::now();
    system.run_for(from_secs_f64(boot_seconds));
    let boot_wall = started.elapsed().as_secs_f64();
    let release = system.handset_release_time().map(to_secs_f64);
    let boot_digest = system.fingerprint();
    let boot_guest_digest = system.guest_fingerprint();
    let boot_exact = exact_digests(&system);
    let boot_counters = counters(&system);
    let boot_seconds_done = system.seconds();
    let _ = writeln!(
        out,
        "boot: {:.3} virtual s / {:.3} wall s = {:.3}x (handset release {}, battery ready {})",
        boot_seconds_done,
        boot_wall,
        boot_seconds_done / boot_wall.max(1e-9),
        release.map_or("never".to_string(), |t| format!("{t:.4} s")),
        system.main_battery_ready_flag().map_or("n/a".to_string(), |ready| ready.to_string())
    );
    if release.is_none() || system.main_battery_ready_flag() == Some(false) {
        let _ = writeln!(out, "WARNING: the firmware did not reach the expected paired sensor-ready boot state");
    }
    let mut report = Report { boot_seconds: boot_seconds_done, boot_wall, boot_digest, boot_guest_digest, boot_exact, boot_counters, release_seconds: release, measurements: Vec::new() };
    for sample in 0..samples {
        let label = if samples == 1 { "steady".to_string() } else { format!("steady_sample_{}", sample + 1) };
        let m = measure(&mut system, &label, &[seconds], false);
        print_measurement(out, &m);
        report.measurements.push(m);
    }
    if menu {
        let m = measure(&mut system, "menu_redraw_after_steady", &[0.5, 0.5], true);
        print_measurement(out, &m);
        report.measurements.push(m);
    }
    Ok(report)
}

fn print_measurement(out: &mut dyn Write, m: &Measurement) {
    let skipped = m.skipped(0) + m.skipped(1);
    let total = m.instructions[0] + m.instructions[1];
    let _ = writeln!(
        out,
        "{}: {:.3} virtual s / {:.3} wall s = {:.3}x; instructions main {} handset {}; idle-skipped {:.1}% (main {}, handset {}); slices main {} handset {}",
        m.label,
        m.elapsed,
        m.wall,
        m.factor(),
        m.instructions[0],
        m.instructions[1],
        if total > 0 { 100.0 * skipped as f64 / total as f64 } else { 0.0 },
        m.skipped(0),
        m.skipped(1),
        m.slices(0),
        m.slices(1)
    );
    for button in &m.buttons {
        let _ = writeln!(out, "    button {button}");
    }
}

pub fn run(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, out) {
        Ok(code) => code,
        Err((true, message)) => {
            let _ = writeln!(err, "ngc-cli bench: {message}\n{USAGE}");
            2
        }
        Err((false, message)) => {
            let _ = writeln!(err, "ngc-cli bench: {message}");
            1
        }
    }
}

fn run_inner(argv: &[String], out: &mut dyn Write) -> Result<i32, (bool, String)> {
    let usage = |message: String| (true, message);
    let failed = |message: String| (false, message);
    let parsed = args::parse(
        argv,
        &["main", "handset", "boot-seconds", "seconds", "steady-samples", "json", "release", "dive-seconds", "dive-depths", "dive-png"],
        &["no-idle-ff", "verify-idle-ff", "no-menu", "no-i2c-idle-high", "no-routine-accel", "shadow-routine-accel", "verify-routine-accel", "dive"],
    )
    .map_err(usage)?;
    let routine_accel = common::routine_accel_mode(&parsed).map_err(usage)?;
    let release = common::parse_release(parsed.value("release")).map_err(usage)?;
    if !parsed.positional.is_empty() {
        return Err(usage(format!("unexpected argument '{}'", parsed.positional[0])));
    }
    // The options of the other benchmark are refused, not ignored (a dive run with --seconds would silently use --dive-seconds).
    let dive = parsed.flag("dive");
    let (values, flags): (&[&str], &[&str]) = if dive { (&["boot-seconds", "seconds", "steady-samples"], &["verify-idle-ff", "no-menu"]) } else { (&["dive-seconds", "dive-depths", "dive-png"], &[]) };
    let other = if dive { "the boot/steady benchmark" } else { "--dive" };
    if let Some(name) = values.iter().find(|name| parsed.value(name).is_some()).or_else(|| flags.iter().find(|name| parsed.flag(name))) {
        return Err(usage(format!("--{name} belongs to {other}")));
    }
    if dive {
        // (The dive options are validated before the firmware is read.)
        return crate::cmd_dive::run(&parsed, release, routine_accel, out);
    }
    let i2c_idle_high = !parsed.flag("no-i2c-idle-high");
    let boot_seconds = common::parse_f64("boot-seconds", parsed.value("boot-seconds"), 4.5).map_err(usage)?;
    let seconds = common::parse_f64("seconds", parsed.value("seconds"), 1.0).map_err(usage)?;
    let samples = common::parse_u64("steady-samples", parsed.value("steady-samples")).map_err(usage)?.unwrap_or(3);
    if boot_seconds < 2.0 || boot_seconds > 60.0 {
        return Err(usage("--boot-seconds must be between 2 and 60".to_string()));
    }
    if seconds <= 0.0 || seconds > 20.0 || samples == 0 || samples > 20 {
        return Err(usage("--seconds must be in (0, 20] and --steady-samples in 1..=20".to_string()));
    }
    let (main, handset) = common::load_images_in(parsed.value("main"), parsed.value("handset"), Mode::Dual, release).map_err(failed)?;
    let (main, handset) = (main.expect("dual"), handset);
    let menu = !parsed.flag("no-menu");
    let verify = parsed.flag("verify-idle-ff");
    let verify_accel = parsed.flag("verify-routine-accel");
    if verify && verify_accel {
        return Err(usage("--verify-idle-ff and --verify-routine-accel exclude each other".to_string()));
    }
    let idle_ff = !parsed.flag("no-idle-ff");
    let modes: Vec<(bool, RoutineAccelMode)> = if verify {
        vec![(true, routine_accel), (false, routine_accel)]
    } else if verify_accel {
        vec![(idle_ff, RoutineAccelMode::On), (idle_ff, RoutineAccelMode::Off)]
    } else {
        vec![(idle_ff, routine_accel)]
    };
    let mut reports = Vec::new();
    for (idle_ff, accel) in &modes {
        reports.push((*idle_ff, *accel, run_bench(&main, &handset, *idle_ff, *accel, i2c_idle_high, boot_seconds, seconds, samples as u32, menu, out).map_err(failed)?));
    }
    let mut status = 0;
    let mut verification = None;
    if verify || verify_accel {
        // Both verifications compare the exactness digest too (registers, retire counts, FPSCR/VFP, the predecode cache and the
        // cut-block history): neither optimization may change the translation state that decides later block partitions.
        let (on, off) = (&reports[0].2, &reports[1].2);
        let mut identical = on.boot_digest == off.boot_digest && on.boot_exact == off.boot_exact;
        let mut details = vec![format!("boot digest {}", if identical { "identical" } else { "DIFFERENT" })];
        for (a, b) in on.measurements.iter().zip(off.measurements.iter()) {
            let same = a.digest == b.digest && a.instructions == b.instructions && a.pcs == b.pcs && a.exact == b.exact;
            identical &= same;
            details.push(format!("{} {}", a.label, if same { "identical" } else { "DIFFERENT" }));
        }
        let what = if verify { "idle fast-forward on/off" } else { "routine acceleration on/off" };
        let _ = writeln!(out, "{what}: {} ({})", if identical { "IDENTICAL" } else { "MISMATCH" }, details.join(", "));
        if !identical {
            status = 1;
        }
        verification = Some(Json::object().with("identical", identical).with("details", Json::from_items(details.iter().map(String::as_str))));
    }
    if let Some(path) = parsed.value("json") {
        let runs = Json::from_items(reports.iter().map(|(idle_ff, accel, report)| {
            Json::object()
                .with("idleFastForward", *idle_ff)
                .with("routineAccel", accel_name(*accel))
                .with("bootVirtualSeconds", report.boot_seconds)
                .with("bootWallSeconds", report.boot_wall)
                .with("bootVirtualSecondsPerWallSecond", report.boot_seconds / report.boot_wall.max(1e-12))
                .with("handsetReleaseVirtualSeconds", report.release_seconds)
                .with("bootStateDigest", report.boot_digest.as_str())
                .with("bootGuestStateDigest", report.boot_guest_digest.as_str())
                .with(
                    "bootIdleSkippedInstructions",
                    Json::object()
                        .with("main", report.boot_counters[0].map_or(0, |c| c.fast_forward.skipped_instructions))
                        .with("handset", report.boot_counters[1].map_or(0, |c| c.fast_forward.skipped_instructions)),
                )
                .with("measurements", Json::from_items(report.measurements.iter().map(Measurement::to_json)))
        }));
        let mut json = Json::object()
            .with("engine", concat!("ngc-wasm/", env!("CARGO_PKG_VERSION")))
            .with("shape", "emulation/performance/benchmark_system.py: boot, steady intervals, menu redraw")
            .with("host", common::host_json())
            .with("timingFixture", Json::object().with("performanceInMipsPerCpu", 100u64).with("globalQuantumUs", 100u64).with("pollIntervalMs", 50u64))
            .with("storage", "fresh in-memory EEPROM/NOR")
            .with("runs", runs);
        if let Some(verification) = verification {
            json.insert(if verify_accel { "routineAccelVerification" } else { "idleFastForwardVerification" }, verification);
        }
        common::write_json(&PathBuf::from(path), &json).map_err(failed)?;
        let _ = writeln!(out, "result written to {path}");
    }
    Ok(status)
}
