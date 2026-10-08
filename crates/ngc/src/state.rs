//! The runner's state snapshot (`Emulator.state` of `emulation/run_emulator.py`) as JSON, plus the engine fields.
//!
//! Every field the Python runner exposes through `snapshot()`/`_launch()` keeps its name and value format, so the
//! unchanged `viewer.html` (and the evidence files recorded by the Renode runner) read it as before:
//!
//! | field | value |
//! | --- | --- |
//! | `running`, `error` | the execution flag and the stop reason (terminal handler, startup failure) |
//! | `virtualTime`, `standbyTime`, `handsetReleaseTime` | seconds as a float, formed like `renode_client.virtual_seconds()` from the `hh:mm:ss.nnnnnnnnn` text |
//! | `pc`, `mainPC` | the program counters (integers) |
//! | `lcdSummary`, `buttonSummary`, `canSummary`, `adcSummary`, `storageSummary`, `flashSummary` | the models' own `Summary` texts |
//! | `frameReady` | `panelOn=True` and `sleeping=False` in the LCD summary |
//! | `mainBatteryReady` | main RAM byte `0x200042A1` is non-zero |
//! | `inputs`, `serialNumber`, `handsetPowered` | the sensor controls, the EEPROM uint32 at offset 0, the PE3 supply gate |
//! | `hardwareOutputs` | the telemetry outputs, each with its `color` label from `led-colors.json` |
//! | `uartConsole` | one object per UART capture channel (`id`, `board`, `peripheral`, `label` and the capture fields) |
//! | `mode`, `bootMode`, `powerModel`, `performanceMode`, `hostPacing`, `firmware`, `rtcPersistence`, `outputHistoryEpoch`, `standby` | descriptive launch fields |
//!
//! Added by this engine: `engine` (`ngc-wasm/<version>`), `virtualNs`, `instructions`, `idleSkip`, `idleFastForward`,
//! `machineResets`, `realtimeFactor` (measured by the host, `null` until it supplies one), `i2cIdleHigh` / `i2cFixture` (the
//! main board's I2C idle-high fixture, DESIGN 15.3c) and `unavailable`.
//!
//! * `hardwareOutputs[]` entries end with `activity` / `pwmActivity` (the bounded output histories of
//!   [`crate::models::telemetry`]).
//! * `outputHistoryEpoch` is `"<historyNonce>-<generation>"` ([`crate::session::Session::output_history_epoch`]); it changes
//!   on session creation and on every board recreation and machine reset, i.e. whenever the histories start over.
//! * `firmware` is `{release: {id, label}, main: {...}, handset: {...}, addresses: {...}}`; each role object has `path`,
//!   `label`, `sha256` (= `binSha256`, the binary span), `srecSha256`, `stack` and `resetPC`; `addresses` is the release's
//!   table of firmware-specific addresses ([`crate::firmware::ReleaseAddresses`]), an address that is not proven for the
//!   release being `{"address": null, "reason": "..."}`.
//! * `unavailable` maps the state fields that are `null` because of such an address to the reason (empty for TRITON): for
//!   NEPTUN `mainBatteryReady`.

use crate::firmware::Firmware;
use crate::fixtures::{self, UART_CHANNELS};
use crate::persistence::{LedColors, Provenance};
use crate::system::{BootMode, Mode, System, Which};
use emu_core::{to_secs_f64, Json, Time};

/// `"engine": "ngc-wasm/<version>"` of every state and capture.
pub const ENGINE: &str = concat!("ngc-wasm/", env!("CARGO_PKG_VERSION"));

pub const MODE_DUAL: &str = "Real main 5.8 + handset 65.3 over functional CAN";
pub const MODE_HANDSET: &str = "Handset only; example board-ID ADC; no CAN peer";
pub const POWER_MODEL_GATED: &str = "Inferred main PE3 handset supply enable; sampled every 50 virtual ms";
pub const POWER_MODEL_SIMULTANEOUS: &str = "Simultaneous board start";
/// This engine has no per-period timer events to switch: timers are computed from virtual time and events exist only
/// for IRQs, DMA requests, input capture and observed outputs (DESIGN.md section 9).
pub const PERFORMANCE_MODE: &str = "Arithmetic timer counters; observable timer events retained";
pub const HOST_PACING_DEFAULT: &str = "Host-paced virtual time (the engine never reads a wall clock)";

/// `renode_client.virtual_seconds()`: parses `machine ElapsedVirtualTime` (`[d.]hh:mm:ss.nnnnnnnnn`) as
/// `days * 86400 + hours * 3600 + minutes * 60 + float(seconds)`, so the double rounds exactly like the runner's.
pub fn runner_virtual_seconds(time: Time) -> f64 {
    const NS: u64 = 1_000_000_000;
    let total_seconds = time / NS;
    let nanos = time % NS;
    let days = total_seconds / 86_400;
    let hours = total_seconds % 86_400 / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    let text = format!("{seconds:02}.{nanos:09}");
    let seconds: f64 = text.parse().expect("seconds text");
    days as f64 * 86_400.0 + hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds
}

/// `Emulator.firmware[role]`: path, sha256, stack and reset PC; the paths of this engine are the SREC file names (the
/// browser has no file system) and `sha256` is the hash of the binary span the Renode runner loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareDescriptor {
    pub role: &'static str,
    pub label: &'static str,
    pub path: String,
    pub sha256: String,
    pub srec_sha256: String,
    pub stack: u32,
    pub reset_pc: u32,
}

impl FirmwareDescriptor {
    pub fn of(firmware: &Firmware) -> Self {
        let expected = firmware.release.expected(firmware.role);
        Self {
            role: firmware.role.name(),
            label: expected.label,
            path: expected.file_name.to_string(),
            sha256: crate::sha256::to_hex(&firmware.bin_sha256),
            srec_sha256: crate::sha256::to_hex(&firmware.srec_sha256),
            stack: firmware.initial_sp(),
            reset_pc: firmware.reset_pc(),
        }
    }

    /// `{path, sha256, binSha256, srecSha256, stack, resetPC, label}`; `sha256` is the runner's name of the binary-span
    /// hash and equals `binSha256`.
    pub fn to_json(&self) -> Json {
        Json::object()
            .with("path", self.path.as_str())
            .with("sha256", self.sha256.as_str())
            .with("binSha256", self.sha256.as_str())
            .with("stack", u64::from(self.stack))
            .with("resetPC", u64::from(self.reset_pc))
            .with("label", self.label)
            .with("srecSha256", self.srec_sha256.as_str())
    }
}

/// The `rtcPersistence` bookkeeping of the last launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcInfo {
    /// `str(self.rtc_path)`.
    pub path: String,
    /// Boards that were present in the loaded `rtc-state.json`.
    pub restored_boards: Vec<String>,
    /// Origin of each board's calendar (`fresh-rtc` when nothing was restored).
    pub sources: Vec<(String, Provenance)>,
    /// `mainBkp1WakeOverride`: a dual handset-wake boot overrode main `RTC.BKP1R`.
    pub main_bkp1_wake_override: bool,
}

impl RtcInfo {
    pub fn to_json(&self) -> Json {
        let mut sources = Json::object();
        for (name, provenance) in &self.sources {
            sources.insert(name.clone(), provenance.to_json());
        }
        Json::object()
            .with("path", self.path.as_str())
            .with("policy", crate::persistence::RTC_CLOCK_POLICY)
            .with("precision", crate::persistence::RTC_PRECISION)
            .with("restoredBoards", Json::from_items(self.restored_boards.iter().map(String::as_str)))
            .with("sources", sources)
            .with("mainBkp1WakeOverride", self.main_bkp1_wake_override)
    }
}

/// Everything the snapshot needs besides the system itself.
pub struct StateView<'a> {
    pub system: &'a System,
    pub running: bool,
    pub error: Option<&'a str>,
    pub led_colors: &'a LedColors,
    pub firmware: &'a [FirmwareDescriptor],
    pub rtc: &'a RtcInfo,
    pub last_capture: Option<&'a str>,
    pub host_pacing: &'a str,
    pub realtime_factor: Option<f64>,
    pub output_history_epoch: &'a str,
}

/// The runner state of the viewer (`Emulator.state` after `snapshot()`), key order as in the Python dictionary.
pub fn build(view: &StateView<'_>) -> Json {
    let system = view.system;
    let dual = system.mode() == Mode::Dual;
    let lcd_summary = system.lcd_summary();
    let frame_ready = lcd_summary.contains("panelOn=True") && lcd_summary.contains("sleeping=False");
    // `firmware`: the release both images belong to, one descriptor per loaded role and the release's firmware-specific
    // address table (an unavailable address is `{"address": null, "reason": ...}`).
    let mut firmware = Json::object();
    firmware.insert("release", system.release().to_json());
    for descriptor in view.firmware {
        firmware.insert(descriptor.role, descriptor.to_json());
    }
    firmware.insert("addresses", system.release().addresses.to_json());
    let mut unavailable = Json::object();
    for (field, reason) in system.unavailable_fields() {
        unavailable.insert(field, reason);
    }

    let mut state = Json::object();
    state.insert("running", view.running);
    state.insert("virtualTime", runner_virtual_seconds(system.time()));
    state.insert("pc", u64::from(system.pc(Which::Handset).unwrap_or(0)));
    state.insert("lcdSummary", lcd_summary);
    state.insert("frameReady", frame_ready);
    state.insert("error", view.error);
    state.insert("firmware", firmware);
    state.insert("unavailable", unavailable);
    // The main board's I2C idle-high fixture (DESIGN 15.3c): named so that a capture says which start-up it came from.
    state.insert("i2cIdleHigh", dual && system.options().main_i2c_idle_high);
    state.insert(
        "i2cFixture",
        if dual { system.options().i2c_fixture_text() } else { "Not applicable: handset-only run, no main board" },
    );
    state.insert("performanceMode", PERFORMANCE_MODE);
    state.insert("hostPacing", view.host_pacing);
    state.insert("hardwareOutputs", hardware_outputs(system, view.led_colors));
    state.insert("uartConsole", uart_console(system));
    state.insert("mode", if dual { MODE_DUAL } else { MODE_HANDSET });
    state.insert("rtcPersistence", view.rtc.to_json());
    if dual {
        state.insert("bootMode", system.boot_mode().name());
        state.insert("standby", system.is_standby());
        state.insert("standbyTime", system.standby_time().map(runner_virtual_seconds));
        state.insert("handsetReleaseTime", system.handset_release_time().map(runner_virtual_seconds));
        state.insert("powerModel", if system.config().simultaneous_start { POWER_MODEL_SIMULTANEOUS } else { POWER_MODEL_GATED });
    }
    state.insert("outputHistoryEpoch", view.output_history_epoch);
    state.insert("buttonSummary", system.button_summary());
    if let Some(main) = system.main.as_ref() {
        state.insert("mainPC", u64::from(main.pc()));
        state.insert("canSummary", system.link.summary());
        state.insert("mainBatteryReady", system.main_battery_ready_flag());
        state.insert("inputs", system.inputs().to_json());
        state.insert("adcSummary", system.adc_summary().unwrap_or_default());
        state.insert("storageSummary", main.eeprom.summary_text());
        state.insert("flashSummary", system.flash_summary().unwrap_or_default());
        state.insert("serialNumber", u64::from(system.serial_number().unwrap_or(0)));
        state.insert("handsetPowered", system.handset_powered());
    }
    if let Some(path) = view.last_capture {
        state.insert("lastCapture", path);
    }
    // Engine fields (not part of the Renode runner state).
    state.insert("engine", ENGINE);
    state.insert("virtualNs", system.time());
    state.insert("instructions", instruction_counts(system));
    state.insert("idleSkip", idle_skip(system));
    state.insert("idleFastForward", system.config().idle_fast_forward);
    state.insert("routineAccel", routine_accel(system));
    state.insert("machineResets", machine_resets(system));
    state.insert("realtimeFactor", view.realtime_factor);
    state
}

fn hardware_outputs(system: &System, colors: &LedColors) -> Json {
    let mut outputs = system.hardware_outputs();
    if let Json::Array(items) = &mut outputs {
        for item in items.iter_mut() {
            let color = item.get("id").and_then(Json::as_str).map(|id| colors.get(id).to_string()).unwrap_or_else(|| "unknown".to_string());
            item.insert("color", color);
        }
    }
    outputs
}

fn uart_console(system: &System) -> Json {
    let dual = system.mode() == Mode::Dual;
    let streams = system.uart.snapshot();
    let mut console = Json::array();
    for (board, peripheral, label) in UART_CHANNELS {
        if board == "main" && !dual {
            continue;
        }
        let board_name = format!("ngc-{board}");
        let suffix = format!(".{peripheral}");
        let Some(stream) = streams.iter().find(|s| s.name.ends_with(&suffix) && s.name.contains(&board_name)) else { continue };
        let mut entry = stream.to_json();
        entry.insert("id", format!("{board}.{peripheral}"));
        entry.insert("board", board);
        entry.insert("peripheral", peripheral);
        entry.insert("label", label);
        console.push(entry);
    }
    console
}

fn instruction_counts(system: &System) -> Json {
    let mut counts = Json::object();
    if let Some(n) = system.instructions(Which::Main) {
        counts.insert("main", n);
    }
    if let Some(n) = system.instructions(Which::Handset) {
        counts.insert("handset", n);
    }
    counts
}

fn idle_skip(system: &System) -> Json {
    let mut skip = Json::object();
    for which in [Which::Main, Which::Handset] {
        if let Some(c) = system.counters(which) {
            skip.insert(
                which.name(),
                Json::object()
                    .with("loops", c.fast_forward.loops)
                    .with("skippedInstructions", c.fast_forward.skipped_instructions)
                    .with("failedVerifications", c.fast_forward.failed_verifications)
                    .with("slices", c.slices)
                    .with("eventsFired", c.events_fired)
                    .with("idleJumps", c.idle_jumps),
            );
        }
    }
    skip
}

/// Exact routine acceleration (`armv7m::accel`): the mode and, per core, how many calls memo entries replaced.
fn routine_accel(system: &System) -> Json {
    use armv7m::RoutineAccelMode;
    let mode = match system.routine_accel_mode() {
        RoutineAccelMode::Off => "off",
        RoutineAccelMode::On => "on",
        RoutineAccelMode::Shadow => "shadow",
    };
    let mut out = Json::object().with("mode", mode);
    for which in [Which::Main, Which::Handset] {
        if let Some(stats) = system.routine_accel_stats(which) {
            let routines = Json::from_items(stats.routines.iter().map(|r| {
                Json::object()
                    .with("name", r.name)
                    .with("entry", u64::from(r.entry))
                    .with("hits", r.hits)
                    .with("instructionsReplaced", r.instructions_replaced)
                    .with("recorded", r.recorded)
                    .with("unsafePaths", r.unsafe_paths)
                    .with("budgetSkips", r.budget_skips)
                    .with("memoEntries", r.memo_entries)
            }));
            out.insert(
                which.name(),
                Json::object()
                    .with("hits", stats.hits())
                    .with("instructionsReplaced", stats.instructions_replaced())
                    .with("shadowChecks", stats.shadow_checks)
                    .with("shadowMismatches", stats.shadow_mismatches)
                    .with("routines", routines),
            );
        }
    }
    out
}

fn machine_resets(system: &System) -> Json {
    Json::from_items(system.reset_log().iter().map(|event| {
        Json::object()
            .with("board", event.board.name())
            .with("cause", event.cause.name())
            .with("requestedAt", to_secs_f64(event.requested_at))
            .with("appliedAt", to_secs_f64(event.applied_at))
    }))
}

/// The boot mode text of a launch for display (`bootMode`).
pub fn boot_mode_name(mode: BootMode) -> &'static str {
    mode.name()
}

/// Hardware constants re-exported for hosts that show the HUD channel ids.
pub const LED_CHANNELS: [&str; 3] = fixtures::LED_IDS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_seconds_follow_the_runner_parser() {
        assert_eq!(runner_virtual_seconds(0), 0.0);
        assert_eq!(runner_virtual_seconds(1_050_000_000), 1.05);
        assert_eq!(runner_virtual_seconds(7_200_000_000), 7.2);
        assert_eq!(runner_virtual_seconds(4_500_000_000), 4.5);
        // Minutes and hours are added as floats, like int(minutes) * 60 + float(seconds).
        assert_eq!(runner_virtual_seconds(65_123_456_789), 60.0 + 5.123456789);
        assert_eq!(runner_virtual_seconds(3_661_000_000_000), 3661.0);
        assert_eq!(runner_virtual_seconds(90_000_000_000 * 1000), 90_000.0);
    }
}
