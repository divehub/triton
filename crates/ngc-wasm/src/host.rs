//! The seam between the C ABI (`lib.rs`) and the engine session.
//!
//! The browser worker talks to one [`Host`]: a running emulation session (dual or handset-only system with
//! its runner fixtures, profile persistence and evidence capture). The ABI layer only moves bytes, JSON
//! text and numbers; everything behavioral lives behind this trait.

use emu_core::Json;

/// A named binary blob: a profile file (`eeprom.bin`, `nor.ngc`, `rtc-state.json`, `inputs.json`,
/// `led-colors.json`) or a capture file (`state.json`, `lcd.png`, `can-trace.tsv`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub data: Vec<u8>,
}

impl Part {
    pub fn new(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Part {
        Part { name: name.into(), data: data.into() }
    }
}

/// The LCD frame as raw memory in the module's linear memory: valid until the next call that runs or
/// restarts the session. `rgba` is `width * height * 4` bytes, row-major, alpha 255.
#[derive(Clone, Copy, Debug)]
pub struct FrameRef {
    pub width: u32,
    pub height: u32,
    pub version: u64,
    pub ptr: *const u8,
    pub len: usize,
}

impl FrameRef {
    pub const EMPTY: FrameRef = FrameRef { width: 0, height: 0, version: 0, ptr: std::ptr::null(), len: 0 };
}

impl Default for FrameRef {
    fn default() -> Self {
        FrameRef::EMPTY
    }
}

/// Profile files handed to a new session (all optional, byte-compatible with the Renode runner's files).
#[derive(Clone, Debug, Default)]
pub struct StagedProfile {
    pub eeprom: Option<Vec<u8>>,
    pub nor: Option<Vec<u8>>,
    pub rtc_state: Option<Vec<u8>>,
    pub inputs: Option<Vec<u8>>,
    pub led_colors: Option<Vec<u8>>,
}

impl StagedProfile {
    /// Slot numbers of the ABI: 0 eeprom.bin, 1 nor.ngc, 2 rtc-state.json, 3 inputs.json, 4 led-colors.json.
    pub fn slot_mut(&mut self, kind: u32) -> Option<&mut Option<Vec<u8>>> {
        match kind {
            0 => Some(&mut self.eeprom),
            1 => Some(&mut self.nor),
            2 => Some(&mut self.rtc_state),
            3 => Some(&mut self.inputs),
            4 => Some(&mut self.led_colors),
            _ => None,
        }
    }
}

/// Session configuration (`ngc_session_create`): the runner's command-line options.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// `true`: real main 5.8 + handset 65.3 over CAN (`--dual`); `false`: handset only.
    pub dual: bool,
    /// `true`: `--boot-mode cold`; `false`: handset wake (default).
    pub cold: bool,
    pub simultaneous_start: bool,
    pub idle_fast_forward: bool,
    /// Exact routine acceleration (`routineAccel`, default on; `--no-routine-accel` of the CLI).
    pub routine_accel: bool,
    /// Verification mode of the routine acceleration (`routineAccelShadow`, default off): every memo hit is replayed and
    /// interpreted and compared. Slow; for tests.
    pub routine_accel_shadow: bool,
    pub adc_sample: u32,
    pub start_paused: bool,
    /// The main board's I2C idle-high fixture (`i2cIdleHigh`, default on; `--no-i2c-idle-high` of the CLI).
    pub i2c_idle_high: bool,
    /// The host's random nonce of the output-history epoch (`historyNonce`, default 0).
    pub history_nonce: u64,
    /// Benchmark and test hook (`blankEeprom`, default off; the page never sends it and there is no CLI flag): a new EEPROM stays
    /// erased instead of becoming the factory image, which the Renode-recorded workload of `web/bench-node.mjs --dive` needs to match
    /// the native dive benchmark. It selects that recorded workload as a whole by applying `ngc::scenario::recorded_config`: it also
    /// keeps the handset buttons as the Renode model of the recordings (`SessionConfig::button_pull_up` false) and switches the start
    /// at the surface off whatever `startAtSurface` says. It is not the user option the factory image does not have.
    pub blank_eeprom: bool,
    /// The start-at-the-surface fixture (`startAtSurface`, default on; `--no-start-at-surface` of the CLI).
    pub start_at_surface: bool,
    /// The surface pressure in mbar of that fixture (`surfacePressureMbar`, 100 to 30000, default 1013.25).
    pub surface_pressure_mbar: f64,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            dual: true,
            cold: false,
            simultaneous_start: false,
            idle_fast_forward: true,
            routine_accel: true,
            routine_accel_shadow: false,
            adc_sample: 400,
            start_paused: false,
            i2c_idle_high: true,
            history_nonce: 0,
            blank_eeprom: false,
            start_at_surface: true,
            surface_pressure_mbar: ngc::surface_start::DEFAULT_SURFACE_MBAR,
        }
    }
}

impl HostConfig {
    /// Parses the JSON configuration object. Unknown keys are an error (a typo must not silently
    /// start a different fixture).
    pub fn from_json(text: &str) -> Result<HostConfig, String> {
        let json = if text.trim().is_empty() { Json::object() } else { Json::parse(text).map_err(|e| e.to_string())? };
        let Some(members) = json.as_object() else {
            return Err("the session configuration must be a JSON object".to_string());
        };
        let mut config = HostConfig::default();
        for (key, value) in members {
            let flag = |name: &str| value.as_bool().ok_or_else(|| format!("{name} must be a boolean"));
            match key.as_str() {
                "mode" => match value.as_str() {
                    Some("dual") => config.dual = true,
                    Some("handset") | Some("handset-only") => config.dual = false,
                    _ => return Err("mode must be \"dual\" or \"handset\"".to_string()),
                },
                "bootMode" => match value.as_str() {
                    Some("handset-wake") => config.cold = false,
                    Some("cold") => config.cold = true,
                    _ => return Err("bootMode must be \"handset-wake\" or \"cold\"".to_string()),
                },
                "simultaneousStart" => config.simultaneous_start = flag("simultaneousStart")?,
                "idleFastForward" => config.idle_fast_forward = flag("idleFastForward")?,
                "routineAccel" => config.routine_accel = flag("routineAccel")?,
                "routineAccelShadow" => config.routine_accel_shadow = flag("routineAccelShadow")?,
                "startPaused" => config.start_paused = flag("startPaused")?,
                "i2cIdleHigh" => config.i2c_idle_high = flag("i2cIdleHigh")?,
                "blankEeprom" => config.blank_eeprom = flag("blankEeprom")?,
                "startAtSurface" => config.start_at_surface = flag("startAtSurface")?,
                "surfacePressureMbar" => {
                    let mbar = value.as_f64().ok_or(ngc::surface_start::SURFACE_RANGE_MESSAGE)?;
                    config.surface_pressure_mbar = ngc::surface_start::validate_surface(mbar)?;
                }
                "historyNonce" => {
                    config.history_nonce = value.as_u64().ok_or("historyNonce must be a non-negative integer below 2^64")?;
                }
                "adcSample" => {
                    let sample = value.as_u64().ok_or("adcSample must be an integer in 0..=4095")?;
                    if sample > 4095 {
                        return Err("adcSample must be an integer in 0..=4095".to_string());
                    }
                    config.adc_sample = sample as u32;
                }
                other => return Err(format!("unknown session option: {other}")),
            }
        }
        Ok(config)
    }
}

/// What the ABI needs from a running session.
pub trait Host {
    /// Runs `seconds` of virtual time (rounded up to whole 100 us quanta). Does nothing while paused,
    /// in standby or after an error stop.
    fn run_for(&mut self, seconds: f64);
    /// False while paused, in standby or stopped by an error.
    fn running(&self) -> bool;
    /// Virtual time in seconds.
    fn time_seconds(&self) -> f64;
    /// One UI action (`POST /api/action` schema of `run_emulator.py`); returns the state JSON.
    fn action(&mut self, request_json: &str) -> Result<String, String>;
    /// The runner's state snapshot as JSON text.
    fn state_json(&self) -> String;
    /// Digests of the whole machine state (JSON: `fingerprint`, `exactMain`, `exactHandset`, `instructionsMain`,
    /// `instructionsHandset`, `lcdSha256`, `virtualNs`): the checkpoint the dive benchmark compares between runs, native
    /// and WebAssembly (`ngc::scenario::dive::checkpoint`).
    fn checkpoint_json(&mut self) -> String;
    /// The LCD frame (brings the visible buffer up to date first).
    fn frame(&mut self) -> FrameRef;
    /// Storage files that changed since the last call (empty when nothing is dirty).
    fn take_profile_changes(&mut self) -> Vec<Part>;
    /// The complete profile, including a fresh RTC checkpoint.
    fn export_profile(&mut self) -> Vec<Part>;
    /// `(name, files)` of an evidence capture: `state.json`, `lcd.png` and, when a CAN link exists,
    /// `can-trace.tsv`.
    fn capture(&mut self) -> (String, Vec<Part>);
    /// Closes the session with the runner's shutdown semantics (RTC checkpoint saved); returns the profile.
    fn shutdown(self: Box<Self>) -> Vec<Part>;
    /// The current UTC time in microseconds since the Unix epoch (the engine has no clock; capture names use it).
    fn set_clock(&mut self, utc_micros: i64);
    /// Host entropy for the ids of later launches (`outputHistoryEpoch`).
    fn set_seed(&mut self, seed: u64);
    /// What only the host can measure: the real-time factor and a description of the pacing, shown in the state.
    fn set_host_info(&mut self, realtime_factor: Option<f64>, pacing: Option<String>);
}
