//! `ngc-cli run`: boots the system and runs it for some virtual seconds.

use crate::args;
use crate::common;
use crate::profile_files;
use emu_core::{from_millis, from_secs_f64, to_secs_f64, Json};
use ngc::session::{Session, SessionConfig};
use ngc::system::{Mode, RoutineAccelMode, System, Which};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

/// What the run drives: a bare [`System`], or a [`Session`] when `--data-dir` gives the run a file-backed profile.
enum Machine {
    Bare(Box<System>),
    Persistent(Box<Session>),
}

impl Machine {
    fn system(&mut self) -> &mut System {
        match self {
            Machine::Bare(system) => system,
            Machine::Persistent(session) => session.system_mut(),
        }
    }
}

pub const USAGE: &str = "ngc-cli run [--main <srec>] [--handset <srec>] [--mode dual|handset] [--seconds S]\n  \
    [--boot-mode handset-wake|cold] [--simultaneous-start] [--no-idle-ff] [--no-routine-accel|--shadow-routine-accel] [--no-i2c-idle-high] [--release ID]\n  \
    [--no-start-at-surface] [--custom] [--uart-text]\n  \
    [--ppm out.ppm] [--can-trace out.tsv]\n  \
    [--pc-trace N out.u32le [--pc-trace-after S] [--board handset|main]] [--json out.json] [--no-warnings] [--log N] [--inputs SCRIPT]\n  \
    [--dump-sram PREFIX] [--peek ADDR[,ADDR...] [--board handset|main]] [--access-trace N [--board handset|main]]\n  \
    [--data-dir DIR]\n  \
    --dump-sram writes PREFIX-<board>-sram1.bin / -sram2.bin of each board at the end of the run.\n  \
    --data-dir keeps the runner profile in DIR (eeprom.bin, nor.ngc, rtc-state.json, inputs.json, led-colors.json, the files of\n  \
    run_emulator.py --data-dir): the ones that exist are loaded before the boot and the ones that changed are written after the\n  \
    run, each replaced atomically. The run then goes through the host-facing Session (storage and RTC restore/save as in a viewer\n  \
    run); a corrupt rtc-state.json stops the start without being replaced.\n  \
    --access-trace prints the latest N CPU accesses outside flash/SRAM (MMIO) of the board with instruction index and time.\n  \
    --peek prints the side-effect-free words at the given hexadecimal addresses (RAM, peripherals, PPB) at the end.\n  \
    --inputs applies scripted host inputs at quantum boundaries: entries separated by ';', each `<seconds> <action>`\n  \
    with up | down | confirm | press MASK | pulse MASK US | can-connect true|false | can-drop ID | set key=value...\n  \
    (for example \"5.0 down; 5.6 up; 6 confirm; 7 set pressure1Mbar=1500 oxygen1Mv=12\").\n  \
    Boots the original firmware images on the emulated boards and runs S virtual seconds (default 5).\n  \
    Without --main/--handset the files under firmware/TRITON-5.8-65.3 (or firmware/<ID> with --release ID,\n  \
    TRITON-5.8-65.3 or NEPTUN-5.8-65.3) are used, in the repository or in the directory named by NGC_FIRMWARE_DIR\n  \
    (you supply them; they are not part of the repository); main and handset must be of the same release.\n  \
    --uart-text prints the captured text of every UART channel that transmitted (the tail the capture keeps) after the report.\n  \
    --custom admits --main / --handset (both required in the dual mode, no default paths) as custom (native) builds: structural\n  \
    validation only (S-record syntax, bounds 0x08004000..0x08100000, vector table, initial SP, Thumb reset vector, entry record),\n  \
    no release hashes; the original-firmware diagnostics (battery, mode, decompression health, terminal-handler stop) are\n  \
    unavailable and the EEPROM factory image is not applied. Progress is read from the UART output and the fault registers.\n  \
    --no-i2c-idle-high leaves the main board's I2C idle inputs PB6/PB7/PB10/PB11 low (by default they are driven high\n  \
    before the first instruction, a functional idle-line fixture; the older Renode recordings predate it).\n  \
    --no-routine-accel turns off the exact acceleration of the runtime-library routines (memoized soft-float calls, DESIGN.md 16.2;\n  \
    results are identical either way, only host speed differs); --shadow-routine-accel replays and interprets every memo hit and\n  \
    compares the two (slow verification mode).\n  \
    With --data-dir the host-facing Session applies two labeled emulator fixtures. When the profile has no eeprom.bin (or an\n  \
    entirely erased one) the EEPROM is created from the factory image once: the records the firmware's first-boot defaults never\n  \
    write (serial, oxygen-toxicity model and dose, the tissue block, the no-fly records; docs/eeprom.md) get the value their firmware\n  \
    code implies, and an existing eeprom.bin is never touched (there is no option; without --data-dir the run uses a bare system\n  \
    with an erased EEPROM). --no-start-at-surface turns off the other fixture, which is on by default: every board creation starts\n  \
    at the surface pressure, a new session with the oxygen cells at their defaults.\n  \
    --mode handset runs the handset alone (no CAN peer, like the viewer without --dual).\n  \
    --pc-trace records the first N executed instruction addresses of a board (default handset) as little-endian\n  \
    u32 words; that board runs without idle fast-forward until N instructions were traced. With --pc-trace-after S the\n  \
    system first runs S virtual seconds (a steady-state window).\n  \
    Prints virtual and wall time, the realtime factor, executed instructions per board, idle-skip statistics\n  \
    and a short state summary.";

pub fn run(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, out) {
        Ok(()) => 0,
        Err(RunError::Usage(message)) => {
            let _ = writeln!(err, "ngc-cli run: {message}\n{USAGE}");
            2
        }
        Err(RunError::Failed(message)) => {
            let _ = writeln!(err, "ngc-cli run: {message}");
            1
        }
    }
}

enum RunError {
    Usage(String),
    Failed(String),
}

impl From<String> for RunError {
    fn from(message: String) -> Self {
        RunError::Failed(message)
    }
}

fn run_inner(argv: &[String], out: &mut dyn Write) -> Result<(), RunError> {
    let parsed = args::parse(
        argv,
        &["main", "handset", "mode", "seconds", "boot-mode", "ppm", "can-trace", "pc-trace", "board", "json", "pc-trace-out", "log", "inputs", "dump-sram", "peek", "access-trace", "data-dir", "release", "pc-trace-after"],
        &["simultaneous-start", "no-idle-ff", "no-warnings", "no-i2c-idle-high", "no-routine-accel", "shadow-routine-accel", "no-start-at-surface", "custom", "uart-text"],
    )
    .map_err(RunError::Usage)?;
    let routine_accel = common::routine_accel_mode(&parsed).map_err(RunError::Usage)?;
    let release = common::parse_release(parsed.value("release")).map_err(RunError::Usage)?;
    let i2c_idle_high = !parsed.flag("no-i2c-idle-high");
    let log_lines = common::parse_u64("log", parsed.value("log")).map_err(RunError::Usage)?.unwrap_or(0) as usize;
    let mode = common::parse_mode(parsed.value("mode")).map_err(RunError::Usage)?;
    let boot_mode = common::parse_boot_mode(parsed.value("boot-mode")).map_err(RunError::Usage)?;
    let seconds = common::parse_f64("seconds", parsed.value("seconds"), 5.0).map_err(RunError::Usage)?;
    if seconds < 0.0 {
        return Err(RunError::Usage("--seconds must not be negative".to_string()));
    }
    let trace_count = common::parse_u64("pc-trace", parsed.value("pc-trace")).map_err(RunError::Usage)?;
    let trace_path = parsed.value("pc-trace-out").map(str::to_string).or_else(|| parsed.positional.first().cloned());
    if trace_count.is_some() && trace_path.is_none() {
        return Err(RunError::Usage("--pc-trace N needs an output file".to_string()));
    }
    let trace_board = common::parse_which(parsed.value("board"), Which::Handset).map_err(RunError::Usage)?;
    if trace_board == Which::Main && mode == Mode::HandsetOnly {
        return Err(RunError::Usage("--board main needs --mode dual".to_string()));
    }

    let (main, handset) = if parsed.flag("custom") {
        if parsed.value("release").is_some() {
            return Err(RunError::Usage("--custom and --release exclude each other".to_string()));
        }
        if parsed.value("handset").is_none() {
            return Err(RunError::Usage("--custom needs --handset <srec>".to_string()));
        }
        if mode == Mode::Dual && parsed.value("main").is_none() {
            return Err(RunError::Usage("--custom needs --main <srec> (or --mode handset)".to_string()));
        }
        common::load_images_custom(parsed.value("main"), parsed.value("handset"), mode)?
    } else {
        common::load_images_in(parsed.value("main"), parsed.value("handset"), mode, release)?
    };
    let data_dir = parsed.value("data-dir").map(PathBuf::from);
    let setup_started = Instant::now();
    let mut machine = match &data_dir {
        None => Machine::Bare(Box::new(common::build_system(mode, boot_mode, parsed.flag("simultaneous-start"), !parsed.flag("no-idle-ff"), routine_accel, i2c_idle_high, main.as_ref(), &handset)?)),
        Some(dir) => {
            let profile = profile_files::read_profile(dir)?;
            let config = SessionConfig {
                mode,
                boot_mode,
                simultaneous_start: parsed.flag("simultaneous-start"),
                idle_fast_forward: !parsed.flag("no-idle-ff"),
                routine_accel: routine_accel != RoutineAccelMode::Off,
                routine_accel_shadow: routine_accel == RoutineAccelMode::Shadow,
                i2c_idle_high,
                start_at_surface: !parsed.flag("no-start-at-surface"),
                ..SessionConfig::default()
            };
            let label = format!("{}/", dir.display());
            Machine::Persistent(Box::new(Session::new_in(&label, config, main.as_ref(), &handset, profile)?))
        }
    };
    let setup_seconds = setup_started.elapsed().as_secs_f64();
    let system = machine.system();
    if let Some(script) = parsed.value("inputs") {
        for (at, input) in common::parse_inputs(script).map_err(RunError::Usage)? {
            system.schedule_input(at, input);
        }
    }

    let target = from_secs_f64(seconds);
    let access_count = common::parse_u64("access-trace", parsed.value("access-trace")).map_err(RunError::Usage)?;
    if let Some(count) = access_count {
        ngc::bus::access_trace::start(usize::try_from(count).map_err(|_| RunError::Usage("--access-trace is too large".to_string()))?);
    }
    let started = Instant::now();
    let mut traced = None;
    if let (Some(count), Some(path)) = (trace_count, trace_path.as_deref()) {
        let count = usize::try_from(count).map_err(|_| RunError::Usage("--pc-trace is too large".to_string()))?;
        // `--pc-trace-after S` runs S virtual seconds first, so a steady-state window can be recorded.
        let trace_after = common::parse_f64("pc-trace-after", parsed.value("pc-trace-after"), 0.0).map_err(RunError::Usage)?;
        if trace_after > 0.0 {
            system.run_until(from_secs_f64(trace_after));
        }
        let base = system.instructions(trace_board).unwrap_or(0) as usize;
        system.trace_pcs(trace_board, count);
        while system.time() < target && system.can_run() && (system.instructions(trace_board).unwrap_or(0) as usize) - base < count {
            system.run_for(from_millis(1));
        }
        let words = system.take_pc_trace(trace_board);
        common::write_u32le(&PathBuf::from(path), &words)?;
        traced = Some((words.len(), path.to_string()));
    }
    system.run_until(target);
    let wall = started.elapsed().as_secs_f64();
    let accesses = if access_count.is_some() { ngc::bus::access_trace::stop() } else { Vec::new() };

    if let Some(path) = parsed.value("can-trace") {
        std::fs::write(path, system.link.trace_text()).map_err(|e| format!("cannot write {path}: {e}"))?;
    }
    if let Some(path) = parsed.value("ppm") {
        write_ppm(system, path)?;
    }
    if let Some(prefix) = parsed.value("dump-sram") {
        for which in [Which::Main, Which::Handset] {
            let Some(board) = system.board(which) else { continue };
            for (name, base, size) in [("sram1", ngc::memory::SRAM1_BASE, ngc::memory::SRAM1_SIZE), ("sram2", ngc::memory::SRAM2_BASE, ngc::memory::SRAM2_SIZE)] {
                let bytes = board.memory_slice(base, size as usize).ok_or_else(|| format!("cannot read {name}"))?;
                let path = format!("{prefix}-{}-{name}.bin", which.name());
                std::fs::write(&path, bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
            }
        }
    }
    report(system, out, wall, setup_seconds, traced.as_ref().map(|(n, p)| (*n, p.as_str())), parsed.flag("no-warnings"), log_lines);
    if parsed.flag("uart-text") {
        for stream in system.uart.snapshot().into_iter().filter(|s| s.tx_bytes > 0) {
            let _ = writeln!(out, "uart text {} ({} bytes{}):", stream.name, stream.tx_bytes, if stream.truncated { ", tail only" } else { "" });
            for line in stream.text.lines() {
                let _ = writeln!(out, "    | {line}");
            }
        }
    }
    if access_count.is_some() {
        let tag = if trace_board == Which::Main { 0 } else { 1 };
        let tpi = emu_core::TICKS_PER_INSTRUCTION;
        for record in accesses.iter().filter(|r| r.tag == tag) {
            let _ = writeln!(
                out,
                "access {} icount {} (t={} ns) {} {:?} 0x{:08x} = 0x{:x}",
                trace_board.name(),
                record.icount,
                record.icount * tpi,
                if record.write { "W" } else { "R" },
                record.width,
                record.address,
                record.value
            );
        }
    }
    if let Some(list) = parsed.value("peek") {
        let board = system.board(trace_board).ok_or_else(|| format!("the {} board does not exist in this mode", trace_board.name()))?;
        for item in list.split(',').filter(|s| !s.is_empty()) {
            let address = common::parse_address(item).map_err(RunError::Usage)?;
            match board.peek(address, emu_core::Width::Word) {
                Some(value) => {
                    let _ = writeln!(out, "peek {} 0x{address:08x} = 0x{value:08x} ({value})", trace_board.name());
                }
                None => {
                    let _ = writeln!(out, "peek {} 0x{address:08x} = unmapped", trace_board.name());
                }
            }
        }
    }
    let mut result = parsed.value("json").map(|_| result_json(system, wall, setup_seconds));
    // The end of a file-backed run: the session saves the RTC checkpoint and the storage (like the runner's shutdown) and
    // the profile files that changed are replaced.
    if let (Machine::Persistent(session), Some(dir)) = (machine, &data_dir) {
        let profile = session.shutdown();
        let written = profile_files::write_profile(dir, &profile)?;
        let _ = writeln!(out, "profile {}: {}", dir.display(), if written.is_empty() { "unchanged".to_string() } else { format!("wrote {}", written.join(", ")) });
        if let Some(json) = result.as_mut() {
            json.insert("profile", Json::object().with("dataDir", dir.display().to_string()).with("written", Json::from_items(written.iter().copied())));
        }
    }
    if let (Some(path), Some(json)) = (parsed.value("json"), result) {
        common::write_json(&PathBuf::from(path), &json)?;
    }
    Ok(())
}

/// Writes the handset LCD as a PPM (the Renode model's byte-identical export).
fn write_ppm(system: &mut System, path: &str) -> Result<(), String> {
    match system.lcd_ppm() {
        Some(bytes) => std::fs::write(path, bytes).map_err(|e| format!("cannot write {path}: {e}")),
        None => Err("the LCD model is not available yet".to_string()),
    }
}

pub fn result_json(system: &System, wall_seconds: f64, setup_seconds: f64) -> Json {
    let virtual_seconds = system.seconds();
    let mut json = system.status_json();
    json.insert("wallSeconds", wall_seconds);
    json.insert("setupSeconds", setup_seconds);
    json.insert("realtimeFactor", if wall_seconds > 0.0 { virtual_seconds / wall_seconds } else { 0.0 });
    json.insert("host", common::host_json());
    json.insert("fingerprint", system.fingerprint());
    // The digest of the guest state alone (older builds' fingerprint): unchanged by the output histories.
    json.insert("guestFingerprint", system.guest_fingerprint());
    json.insert("release", system.release().to_json());
    json.insert("decoHealth", ngc::deco::health(system).to_json());
    json.insert("i2cIdleHigh", system.options().main_i2c_idle_high && system.main.is_some());
    let mut boards = Json::object();
    for which in [Which::Main, Which::Handset] {
        if let Some(counters) = system.counters(which) {
            boards.insert(
                which.name(),
                Json::object()
                    .with("pc", system.pc(which).map(u64::from))
                    .with("instructions", counters.instructions)
                    .with("slices", counters.slices)
                    .with("eventsFired", counters.events_fired)
                    .with("idleJumps", counters.idle_jumps)
                    .with("stopRequests", counters.stop_requests)
                    .with("fastForwardLoops", counters.fast_forward.loops)
                    .with("fastForwardSkippedInstructions", counters.fast_forward.skipped_instructions)
                    .with("fastForwardFailedVerifications", counters.fast_forward.failed_verifications),
            );
        }
    }
    json.insert("boards", boards);
    json
}

fn report(system: &System, out: &mut dyn Write, wall: f64, setup: f64, traced: Option<(usize, &str)>, no_warnings: bool, log_lines: usize) {
    let virtual_seconds = system.seconds();
    let factor = if wall > 0.0 { virtual_seconds / wall } else { f64::INFINITY };
    let _ = writeln!(
        out,
        "mode {} boot {} idle-ff {} routine-accel {}",
        if system.main.is_some() { "dual" } else { "handset" },
        system.boot_mode().name(),
        if system.config().idle_fast_forward { "on" } else { "off" },
        match system.routine_accel_mode() {
            RoutineAccelMode::Off => "off",
            RoutineAccelMode::On => "on",
            RoutineAccelMode::Shadow => "shadow",
        }
    );
    for which in [Which::Main, Which::Handset] {
        if let Some(stats) = system.routine_accel_stats(which) {
            if stats.hits() + stats.shadow_checks > 0 {
                let _ = writeln!(
                    out,
                    "{:<7} routine acceleration: {} calls replaced ({} instructions){}",
                    which.name(),
                    stats.hits() + stats.shadow_checks,
                    stats.instructions_replaced(),
                    if stats.shadow_checks > 0 { format!(", {} shadow checks, {} mismatches", stats.shadow_checks, stats.shadow_mismatches) } else { String::new() }
                );
                for r in stats.routines.iter().filter(|r| r.hits + r.misses + r.shadow_checks > 0) {
                    let _ = writeln!(
                        out,
                        "    {:<10} {:#010x}: {} hits ({} instructions), {} misses, {} memo entries, {} unsafe paths, {} over the chunk budget, {} declined",
                        r.name, r.entry, r.hits, r.instructions_replaced, r.misses, r.memo_entries, r.unsafe_paths, r.budget_skips, r.declined
                    );
                }
                if !stats.unsafe_reasons.is_empty() {
                    let _ = writeln!(out, "    unsafe paths: {}", stats.unsafe_reasons.iter().map(|(why, n)| format!("{why} x{n}")).collect::<Vec<_>>().join("; "));
                }
            }
        }
    }
    let _ = writeln!(out, "virtual time {:.4} s ({} ns), wall {:.3} s (setup {:.3} s), realtime factor {:.2}x", virtual_seconds, system.time(), wall, setup, factor);
    for which in [Which::Main, Which::Handset] {
        let Some(counters) = system.counters(which) else { continue };
        let ff = counters.fast_forward;
        let share = if counters.instructions > 0 { 100.0 * ff.skipped_instructions as f64 / counters.instructions as f64 } else { 0.0 };
        let _ = writeln!(
            out,
            "{:<7} pc 0x{:08x} instructions {} (idle-skipped {} = {:.1}%, loops {}, failed {}) slices {} events {} idle-jumps {}",
            which.name(),
            system.pc(which).unwrap_or(0),
            counters.instructions,
            ff.skipped_instructions,
            share,
            ff.loops,
            ff.failed_verifications,
            counters.slices,
            counters.events_fired,
            counters.idle_jumps
        );
    }
    if system.main.is_some() {
        let _ = writeln!(
            out,
            "handset powered {} (release poll {}), standby {}, battery ready {}",
            system.handset_powered(),
            system.handset_release_time().map_or("-".to_string(), |t| format!("{:.4} s", to_secs_f64(t))),
            system.is_standby(),
            system.main_battery_ready_flag().map_or("n/a".to_string(), |ready| ready.to_string())
        );
        let _ = writeln!(out, "{}", system.link.summary());
        let deco = ngc::deco::health(system);
        let _ = writeln!(out, "decompression state: tissues {}, oxygen {} (read-only report; details in --json)", deco.tissues.name(), deco.oxygen.name());
        if let Some(app) = system.main_application() {
            let show = |value: Option<u32>| value.map_or("n/a".to_string(), |v| v.to_string());
            let _ = writeln!(
                out,
                "main application ({}): wakeCause {} screenMode {} mainMode {} batteryReady {} halTick {} pressure {} temperature {}",
                system.release().id,
                show(app.wake_cause),
                show(app.screen_mode),
                show(app.main_mode),
                show(app.battery_ready),
                show(app.hal_tick),
                show(app.pressure),
                show(app.temperature)
            );
        }
    }
    for which in [Which::Main, Which::Handset] {
        if let Some(faults) = system.fault_registers(which) {
            let text: Vec<String> = faults.iter().map(|(name, value)| format!("{name}=0x{value:08x}")).collect();
            let lockup = system.board_faults(which).and_then(|f| f.lockup).map_or(String::new(), |reason| format!(" LOCKUP ({reason})"));
            let _ = writeln!(out, "{} faults: {}{lockup}", which.name(), text.join(" "));
        }
    }
    for stream in system.uart.snapshot() {
        if stream.tx_bytes > 0 {
            let _ = writeln!(out, "uart {}: {} bytes", stream.name, stream.tx_bytes);
        }
    }
    if let Some(error) = system.error() {
        let _ = writeln!(out, "ERROR: {error}");
    }
    if let Some((count, path)) = traced {
        let _ = writeln!(out, "pc trace: {count} instructions written to {path}");
    }
    if !no_warnings {
        for which in [Which::Main, Which::Handset] {
            let Some(board) = system.board(which) else { continue };
            let log = &board.core.log;
            let warnings = log.count(emu_core::LogLevel::Warning);
            let errors = log.count(emu_core::LogLevel::Error);
            if warnings + errors > 0 {
                let _ = writeln!(out, "{} log: {} warnings, {} errors", which.name(), warnings, errors);
            }
            for entry in log.entries().filter(|e| e.level >= emu_core::LogLevel::Warning).take(log_lines) {
                let _ = writeln!(out, "    {} {entry}", which.name());
            }
            let undefined = board.cpu.undefined_log();
            if !undefined.is_empty() {
                let list: Vec<String> = undefined.iter().take(4).map(|(pc, raw)| format!("0x{pc:08x}:{raw:08x}")).collect();
                let _ = writeln!(out, "{} executed undefined/unimplemented encodings at {}", which.name(), list.join(", "));
            }
        }
    }
    let lcd = system.lcd_summary();
    if !lcd.is_empty() {
        let _ = writeln!(out, "lcd: {lcd}");
    }
}
