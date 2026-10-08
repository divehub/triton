//! The scenario validation suite (`ngc-cli scenario <name>` and `crates/ngc/tests/scenarios.rs`).
//!
//! Each scenario drives a [`Session`] through the same actions as the viewer (and the probes of the Renode runner,
//! `emulation/probe_*.py` of the analysis workspace), checks the engine's own functional expectations and compares what it can with the
//! values the Renode runner recorded in the analysis workspace (`emulation/system-validation-result.json`,
//! `emulation/runtime/**/result.json`, `emulation/main-boot/**`). Those values are embedded as constants with their
//! provenance, so no scenario reads a file. The result is a JSON document plus PNG evidence; the library does no file I/O.
//!
//! # Reading a report
//!
//! * `checks`: expectations about this engine; one failed check fails the scenario.
//! * `comparisons`: a value of this engine against the value Renode recorded, with its source file. `agrees` says
//!   whether they are equal, `expected` whether the difference (if any) is explained, and `note` why. Comparisons never
//!   fail a scenario by themselves: the engine is a new functional model (DESIGN.md section 1) and a timing difference
//!   that is documented is not a defect. A comparison marked `must: true` is also a check.
//! * `steps`: the viewer-visible state after each step (virtual time, screen, LCD summary...).
//! * `limitations`: what the scenario does not establish (AGENTS.md evidence discipline: this is a synthetic
//!   reproduction on a functional model, never a physical observation).

use crate::firmware::Firmware;
use crate::fixtures::Inputs;
use crate::persistence::inputs_file_text;
use crate::session::{Profile, Session, SessionConfig};
use crate::system::{BuildOptions, Which};
use emu_core::{Json, Width};

mod battery;
mod button_capture;
mod can_loss;
mod clock_storage;
mod cold_wake;
mod diluent;
pub mod dive;
mod dual_wake;
mod fast_forward;
mod machine_reset;
mod outputs_uart;

/// What a scenario needs from the host: the two verified firmware images and the platform-script options.
pub struct ScenarioEnv<'a> {
    pub main: &'a Firmware,
    pub handset: &'a Firmware,
    pub options: BuildOptions,
    /// Exact routine acceleration of the sessions the scenarios create (default on; results are identical either way,
    /// see `armv7m::accel`; `false` is the reference for the on/off identity checks).
    pub routine_accel: bool,
}

impl<'a> ScenarioEnv<'a> {
    /// The default platform: the main I2C idle-high fixture on, like a viewer session. The comparisons with the Renode
    /// recordings, which predate the fixture, are informational in this environment (see [`Recorder::finish`]).
    pub fn new(main: &'a Firmware, handset: &'a Firmware) -> Self {
        Self { main, handset, options: BuildOptions::default(), routine_accel: true }
    }

    /// The platform scripts the Renode recordings were made with (no I2C idle-high fixture): the comparisons marked
    /// `must` are binding again.
    pub fn recorded(main: &'a Firmware, handset: &'a Firmware) -> Self {
        Self { main, handset, options: BuildOptions { main_i2c_idle_high: false }, routine_accel: true }
    }

    /// The session configuration `config` with this environment's routine-acceleration switch.
    pub(crate) fn configure(&self, config: SessionConfig) -> SessionConfig {
        SessionConfig { routine_accel: self.routine_accel, ..config }
    }
}

/// Name and purpose of a scenario.
#[derive(Clone, Copy, Debug)]
pub struct ScenarioInfo {
    pub name: &'static str,
    pub summary: &'static str,
}

/// All scenarios, in the order `run_all` executes them.
pub const SCENARIOS: [ScenarioInfo; 10] = [
    ScenarioInfo { name: "dual-wake", summary: "handset-wake boot of both original firmware images to the B1 battery prompt (Renode dual-wake evidence)" },
    ScenarioInfo { name: "button-capture", summary: "physical button pulses through TIM3 capture: 250-count pulse accepted, 100-count pulse rejected" },
    ScenarioInfo { name: "battery-setup", summary: "B1/B2 battery commits with the staggered confirm, handset CAN frames, EEPROM persistence across Restart" },
    ScenarioInfo { name: "diluent-menu", summary: "Diluent gases menu: staggered confirm edits and leaves, exactly simultaneous Press 3 does not" },
    ScenarioInfo { name: "clock-storage", summary: "CAN clock setter, calendar and storage retention across Restart, cold, wake and close/reopen" },
    ScenarioInfo { name: "outputs-uart", summary: "HUD/vibrator outputs, LED colour labels, UART capture tails, output epoch on restart" },
    ScenarioInfo { name: "can-loss", summary: "CAN disconnect and selective identifier loss with restore" },
    ScenarioInfo { name: "cold-wake", summary: "cold boot to the observed standby request, then Wake" },
    ScenarioInfo { name: "machine-reset", summary: "Renode-style machine reset: SYSRESETREQ and IWDG expiry keep RAM/PWR/EEPROM, reset peripherals, reboot" },
    ScenarioInfo { name: "fast-forward", summary: "exact idle fast-forward on/off gives identical state" },
];

/// A finished scenario.
#[derive(Debug)]
pub struct ScenarioReport {
    pub name: String,
    pub passed: bool,
    /// The result document (`<out>/<name>.json`).
    pub document: Json,
    /// `(file name, PNG bytes)` evidence images.
    pub images: Vec<(String, Vec<u8>)>,
    /// `(file name, bytes)` further evidence (CAN traces, profile files).
    pub files: Vec<(String, Vec<u8>)>,
}

/// Runs one scenario by name.
pub fn run(name: &str, env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    // The scenarios read TRITON application RAM (wizard offsets, key sampler, screen ids) and compare with values the Renode
    // runner recorded on the TRITON images.
    if env.main.release.id != crate::firmware::TRITON.id || env.handset.release.id != crate::firmware::TRITON.id {
        return Err(format!(
            "the scenarios are written for {} (addresses, application offsets and Renode recordings); this pair is {}",
            crate::firmware::TRITON.id,
            env.main.release.id
        ));
    }
    match name {
        "dual-wake" => dual_wake::run(env),
        "button-capture" => button_capture::run(env),
        "battery-setup" => battery::run(env),
        "diluent-menu" => diluent::run(env),
        "clock-storage" => clock_storage::run(env),
        "outputs-uart" => outputs_uart::run(env),
        "can-loss" => can_loss::run(env),
        "cold-wake" => cold_wake::run(env),
        "machine-reset" => machine_reset::run(env),
        "fast-forward" => fast_forward::run(env),
        other => Err(format!("unknown scenario '{other}' (try: {})", SCENARIOS.iter().map(|s| s.name).collect::<Vec<_>>().join(", "))),
    }
}

// ---- recording -------------------------------------------------------------------------------------------------------

/// Collects checks, comparisons, steps and evidence of one scenario.
pub(crate) struct Recorder {
    name: &'static str,
    checks: Vec<Json>,
    comparisons: Vec<Json>,
    steps: Vec<Json>,
    notes: Vec<Json>,
    limitations: Vec<Json>,
    images: Vec<(String, Vec<u8>)>,
    files: Vec<(String, Vec<u8>)>,
    /// A check of the engine failed.
    failed: bool,
    /// A comparison marked `must` disagreed (binding only on the platform the recording was made with).
    must_failed: bool,
    extra: Vec<(String, Json)>,
}

impl Recorder {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            checks: Vec::new(),
            comparisons: Vec::new(),
            steps: Vec::new(),
            notes: Vec::new(),
            limitations: Vec::new(),
            images: Vec::new(),
            files: Vec::new(),
            failed: false,
            must_failed: false,
            extra: Vec::new(),
        }
    }

    /// An expectation about this engine. Returns `ok`.
    pub fn check(&mut self, name: &str, ok: bool, observed: impl Into<Json>) -> bool {
        self.checks.push(Json::object().with("name", name).with("passed", ok).with("observed", observed));
        self.failed |= !ok;
        ok
    }

    /// `observed == expected` as a check; the expected value is recorded.
    pub fn expect<T: PartialEq + Clone + Into<Json>>(&mut self, name: &str, observed: T, expected: T) -> bool {
        let ok = observed == expected;
        self.checks.push(Json::object().with("name", name).with("passed", ok).with("observed", observed.into()).with("expected", expected.into()));
        self.failed |= !ok;
        ok
    }

    /// A value of this engine against the value Renode recorded. Informational unless `must` is set (then it is also
    /// a check). `note` explains an expected difference.
    pub fn compare(&mut self, name: &str, ours: impl Into<Json>, renode: impl Into<Json>, source: &str, must: bool, note: &str) -> bool {
        let (ours, renode) = (ours.into(), renode.into());
        let agrees = ours == renode;
        self.comparisons.push(
            Json::object()
                .with("name", name)
                .with("engine", ours)
                .with("renode", renode)
                .with("agrees", agrees)
                .with("must", must)
                .with("source", source)
                .with("note", note),
        );
        if must {
            self.must_failed |= !agrees;
        }
        agrees
    }

    /// The state after a step: label, virtual time and the given fields of the state document.
    pub fn step(&mut self, label: &str, state: &Json, extra: Json) {
        let mut step = Json::object().with("label", label);
        if let Some(time) = state.get("virtualTime") {
            step.insert("virtualTime", time.clone());
        }
        for key in ["running", "error", "standby", "mainBatteryReady", "lcdSummary", "canSummary", "buttonSummary"] {
            if let Some(value) = state.get(key) {
                step.insert(key, value.clone());
            }
        }
        if let Json::Object(members) = extra {
            for (key, value) in members {
                step.insert(key, value);
            }
        }
        self.steps.push(step);
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.notes.push(Json::from(text.into()));
    }

    pub fn limitation(&mut self, text: &str) {
        self.limitations.push(Json::from(text));
    }

    pub fn image(&mut self, name: &str, png: Vec<u8>) {
        self.images.push((name.to_string(), png));
    }

    pub fn file(&mut self, name: &str, bytes: Vec<u8>) {
        self.files.push((name.to_string(), bytes));
    }

    pub fn extra(&mut self, key: &str, value: Json) {
        self.extra.push((key.to_string(), value));
    }

    /// Closes the report. A comparison marked `must` is binding only on the platform the Renode recording was made with
    /// (no I2C idle-high fixture): with the fixture on, a disagreeing one is demoted to informational (`must: false`, with
    /// a note), because the recording predates a fixture that changes the main board's start-up; the engine's own checks
    /// are binding on both platforms.
    pub fn finish(mut self, env: &ScenarioEnv<'_>) -> ScenarioReport {
        let fixture_on = env.options.main_i2c_idle_high;
        if fixture_on {
            for comparison in &mut self.comparisons {
                if comparison.get("must") == Some(&Json::Bool(true)) && comparison.get("agrees") == Some(&Json::Bool(false)) {
                    comparison.insert("must", false);
                    let note = comparison.get("note").and_then(Json::as_str).unwrap_or("").to_string();
                    let demoted = "recorded without the I2C idle-high fixture: informational with it on (--no-i2c-idle-high reproduces the recorded start-up)";
                    comparison.insert("note", if note.is_empty() { demoted.to_string() } else { format!("{note}; {demoted}") });
                }
            }
        }
        let passed = !self.failed && (fixture_on || !self.must_failed);
        let mut document = Json::object()
            .with("scenario", self.name)
            .with("passed", passed)
            .with("engine", crate::state::ENGINE)
            .with("scope", "Original main 5.8 + handset 65.3 firmware on the functional Rust model with synthetic fixtures; not a physical observation")
            .with(
                "firmware",
                Json::object()
                    .with("main", crate::sha256::to_hex(&env.main.bin_sha256))
                    .with("handset", crate::sha256::to_hex(&env.handset.bin_sha256)),
            )
            .with(
                "platformOptions",
                Json::object()
                    .with("mainI2cIdleHigh", env.options.main_i2c_idle_high)
                    // Pinned to the value of the Renode recordings (a fresh profile starts at 4100 mV; see `recorded_inputs`).
                    .with("batteryMv", Json::from_items([crate::fixtures::RECORDED_BATTERY_MV; 2]))
                    // The decompression fixtures are on by default in a session; the recordings were made without them.
                    .with("decoStorageFixture", false)
                    .with("startAtSurface", false),
            )
            .with("checks", Json::Array(self.checks))
            .with("comparisons", Json::Array(self.comparisons))
            .with("steps", Json::Array(self.steps));
        for (key, value) in self.extra {
            document.insert(key, value);
        }
        document.insert("notes", Json::Array(self.notes));
        document.insert("limitations", Json::Array(self.limitations));
        ScenarioReport { name: self.name.to_string(), passed, document, images: self.images, files: self.files }
    }
}

// ---- a session with conveniences --------------------------------------------------------------------------------------

/// Pins the inputs the Renode recordings were made with. A fresh profile starts with 4100 mV batteries, but every value
/// the scenarios compare with (the battery lines of the UART console, the CAN traffic, the LCD frames) was recorded
/// with the runner's defaults, 1500 mV ([`Inputs::recorded_evidence`]); a profile that already carries `inputs.json`
/// (a reopened one) keeps its own values.
pub(crate) fn recorded_inputs(mut profile: Profile) -> Profile {
    if profile.inputs.is_none() {
        profile.inputs = Some(inputs_file_text(&Inputs::recorded_evidence()));
    }
    profile
}

/// Pins the two decompression fixtures off, explicitly, like [`recorded_inputs`] pins the battery voltage: the Renode
/// recordings the scenarios compare with were made without them (a stored tissue block that was never saved keeps its date
/// record, the EEPROM image is compared byte for byte, a reopened profile uses its inputs as saved). The fixtures have
/// their own tests (`crates/ngc/tests/deco_fixtures.rs`).
pub(crate) fn recorded_config(config: SessionConfig) -> SessionConfig {
    SessionConfig { deco_storage_fixture: false, start_at_surface: false, ..config }
}

/// A [`Session`] plus the helpers every scenario uses: actions as JSON, side-effect-free RAM readbacks, evidence.
pub(crate) struct Rig {
    pub session: Session,
}

impl Rig {
    pub fn new(env: &ScenarioEnv<'_>, config: SessionConfig, profile: Profile) -> Result<Rig, String> {
        let session = Session::new_with(env.options, "", env.configure(recorded_config(config)), Some(env.main), env.handset, recorded_inputs(profile))?;
        Ok(Rig { session })
    }

    /// One viewer action; the state document it answers.
    pub fn act(&mut self, request: &str) -> Result<Json, String> {
        let text = self.session.action(request)?;
        Json::parse(&text).map_err(|e| e.to_string())
    }

    /// `advance` of the viewer API (at most 20 virtual seconds per call, longer spans are split).
    pub fn advance(&mut self, seconds: f64) -> Result<Json, String> {
        let mut remaining = seconds;
        loop {
            let chunk = remaining.min(20.0);
            let state = self.act(&format!("{{\"action\":\"advance\",\"seconds\":{chunk}}}"))?;
            remaining -= chunk;
            if remaining <= 1e-9 {
                return Ok(state);
            }
        }
    }

    pub fn state(&self) -> Json {
        Json::parse(&self.session.state_json()).expect("state json")
    }

    pub fn board(&self, which: Which) -> &crate::board::Board<armv7m::Cpu> {
        self.session.system().board(which).expect("board")
    }

    /// Side-effect-free RAM/register readbacks.
    pub fn u8(&self, which: Which, address: u32) -> u32 {
        self.board(which).peek(address, Width::Byte).unwrap_or(0xFFFF_FFFF)
    }

    pub fn u16(&self, which: Which, address: u32) -> u32 {
        self.board(which).peek(address, Width::Half).unwrap_or(0xFFFF_FFFF)
    }

    pub fn u32(&self, which: Which, address: u32) -> u32 {
        self.board(which).peek(address, Width::Word).unwrap_or(0xFFFF_FFFF)
    }

    /// `[ICSR, CFSR, HFSR]` of a board.
    pub fn faults(&self, which: Which) -> Json {
        let [icsr, cfsr, hfsr] = [0xE000_ED04, 0xE000_ED28, 0xE000_ED2C].map(|a| self.u32(which, a));
        Json::object().with("ICSR", u64::from(icsr)).with("CFSR", u64::from(cfsr)).with("HFSR", u64::from(hfsr))
    }

    pub fn faults_clear(&self) -> bool {
        [Which::Main, Which::Handset].into_iter().all(|w| self.u32(w, 0xE000_ED28) == 0 && self.u32(w, 0xE000_ED2C) == 0)
    }

    /// Handset screen id (`0x2000A6E8`) and its view object pointer (`0x20015E48`), as the probes read them.
    pub fn screen(&self) -> u32 {
        self.u32(Which::Handset, 0x2000_A6E8)
    }

    pub fn view(&self) -> u32 {
        self.u32(Which::Handset, 0x2001_5E48)
    }

    pub fn png(&mut self) -> Vec<u8> {
        self.session.lcd_png()
    }

    /// The handset CAN frames of the trace (`ngc-handset.can1`), as `(identifier, payload hex)` in order.
    pub fn handset_frames(&self) -> Vec<(u32, String)> {
        trace_frames(&self.session.system().link.trace_text(), "ngc-handset.can1")
    }

    pub fn main_frames(&self) -> Vec<(u32, String)> {
        trace_frames(&self.session.system().link.trace_text(), "ngc-main.can1")
    }

    pub fn can_trace(&self) -> String {
        self.session.system().link.trace_text()
    }
}

/// `(identifier, payload hex)` of the scheduled frames sent by `sender` in a CAN trace TSV.
pub(crate) fn trace_frames(trace: &str, sender: &str) -> Vec<(u32, String)> {
    trace
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 5 || fields[1] != sender || fields[4] != "scheduled" {
                return None;
            }
            let id = u32::from_str_radix(fields[2].trim_start_matches("0x"), 16).ok()?;
            Some((id, fields[3].to_string()))
        })
        .collect()
}

/// Virtual seconds of a trace line stamp (`hh:mm:ss.nnnnnnnnn`).
pub(crate) fn trace_stamp_seconds(stamp: &str) -> Option<f64> {
    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// The numeric `key=value` fields of a model summary (`a=1; b=0x2; c=True`), as text values.
pub(crate) fn summary_field<'a>(summary: &'a str, key: &str) -> Option<&'a str> {
    // The first item carries the model label (`Main ADC fixture: conversions=1`).
    summary
        .split(';')
        .filter_map(|item| {
            let item = item.trim();
            item.rsplit(": ").next().unwrap_or(item).split_once('=')
        })
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim())
}

pub(crate) fn summary_number(summary: &str, key: &str) -> Option<u64> {
    let value = summary_field(summary, key)?;
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// SHA-256 (hex) of bytes.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    crate::sha256::digest_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_helpers_parse_the_tsv_and_summaries() {
        let trace = "00:00:01.050000000\tngc-main.can1\t0x173\t\tscheduled\textended=False;remote=False\n\
                     00:00:08.736313400\tngc-handset.can1\t0x082\t01\tscheduled\textended=False;remote=False\n\
                     00:00:08.800000000\tngc-handset.can1\t0x083\t01\tdropped\textended=False;remote=False\n";
        assert_eq!(trace_frames(trace, "ngc-handset.can1"), [(0x82, "01".to_string())]);
        assert_eq!(trace_frames(trace, "ngc-main.can1"), [(0x173, String::new())]);
        assert_eq!(trace_stamp_seconds("00:00:08.736313400"), Some(8.7363134));
        let summary = "Main ADC fixture: conversions=54964; sequences=9160; CR=0x10000005; acquisitionEnabled=True";
        assert_eq!(summary_number(summary, "conversions"), Some(54964));
        assert_eq!(summary_number(summary, "sequences"), Some(9160));
        assert_eq!(summary_number(summary, "CR"), Some(0x1000_0005));
        assert_eq!(summary_field(summary, "acquisitionEnabled"), Some("True"));
        assert_eq!(summary_field(summary, "missing"), None);
    }
}
