//! The host-facing session (DESIGN.md section 14): one dual (or handset-only) system with the runner's controls,
//! state document, persistence and evidence capture. `ngc-cli` and `ngc-wasm` use only this API.
//!
//! A [`Session`] is `Emulator` of `emulation/run_emulator.py` without Renode, HTTP, threads and wall-clock pacing:
//!
//! * [`Session::run_for`] advances virtual time (the host paces it against wall time);
//! * [`Session::action`] takes the JSON of `POST /api/action` (`pause`, `resume`, `step`, `advance`, `reset`, `cold`,
//!   `wake`, `up`, `down`, `confirm`, `can`, `inputs`, `led-colors`, `serial`, `capture`) with the runner's payload
//!   fields, validation order and error texts, and answers the state document;
//! * [`Session::state_json`] is the state the viewer polls (`snapshot()` fields, plus `engine` and statistics);
//! * [`Session::frame`] is the LCD as RGBA; [`Session::capture`] the evidence triple `state.json` / `lcd.png` /
//!   `can-trace.tsv`;
//! * the *profile* ([`Profile`]: `eeprom.bin`, `nor.ngc`, `rtc-state.json`, `inputs.json`, `led-colors.json`) goes in at
//!   [`Session::new`] and out through [`Session::take_profile_changes`] (only what changed), [`Session::export_profile`]
//!   and [`Session::shutdown`] (runner close: the RTC checkpoint of both boards is saved first).
//!
//! # Differences from the Renode runner (all deliberate)
//!
//! * Time: the system advances in 100 us quanta; a `run_for`/`advance` request is rounded up to whole quanta. The
//!   standby and PE3 power-gate polls run on the absolute 50 virtual-ms grid, which is where the runner's polls fall
//!   for `step`, running and every `advance` of a multiple of 0.05 s.
//! * `Restart`, `cold`, `wake` and `serial` recreate both boards in the same process (the runner recreates the
//!   Renode process) and keep the same state as the runner: EEPROM, NOR and the RTC checkpoint survive; RAM,
//!   peripherals, the CAN link controls, the UART capture history and the instruction counters start over.
//! * The wall clock is never read: capture names and the `outputHistoryEpoch` come from [`Session::set_utc_micros`]
//!   and an internal counter.
//! * An IWDG expiry or `AIRCR.SYSRESETREQ` performs a Renode-style machine reset instead of ending the run (see
//!   `System::machine_reset`).
//! * Two labelled emulator fixtures act at every board creation, both **on by default** and switchable
//!   ([`SessionConfig::deco_storage_fixture`], [`SessionConfig::start_at_surface`]): the runner has neither. The pre-boot
//!   EEPROM consistency repair ([`crate::deco`]) and the start at the surface ([`crate::surface_start`]; a new session also
//!   resets the oxygen cells). The state names them (`decoStorageFixture`, `startAtSurface`) next to the read-only
//!   `decoHealth`; DESIGN.md section 17.

use crate::actions::{self, Request};
use crate::deco::{self, StorageFixture};
use crate::firmware::Firmware;
use crate::fixtures::{python_float, Inputs};
use crate::models::lcd::NgcParallelLcd;
use crate::models::qspi::NgcQuadSpi;
use crate::persistence::{
    inputs_file_text, parse_inputs_file, plan_restore, LedColors, Provenance, RtcState, BOARD_HANDSET, BOARD_MAIN, EEPROM_FILE, NOR_FILE, RTC_STATE_FILE,
};
use crate::png;
use crate::state::{self, FirmwareDescriptor, RtcInfo, StateView};
use crate::surface_start::{self, SurfaceStart};
pub use crate::system::BuildOptions;
use crate::system::{BootMode, Input, Mode, System, SystemConfig, Which};
use emu_core::json::WriteOptions;
use emu_core::{Json, Time};

/// The profile files of a session (see [`crate::persistence`]).
pub use crate::persistence::Profile;

/// Static configuration of a session (`run_emulator.py` command line).
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// `--dual` (real main 5.8 + handset 65.3 over CAN) or the handset alone.
    pub mode: Mode,
    /// `--boot-mode`: the boot fixture of a plain Restart (Cold and Wake override it for one start).
    pub boot_mode: BootMode,
    /// `--simultaneous-start`.
    pub simultaneous_start: bool,
    /// Exact idle-loop fast-forward (does not change results, only host speed).
    pub idle_fast_forward: bool,
    /// Exact acceleration of the runtime-library routines (soft-double divide, `expf`, float/double conversions) by memoized
    /// calls (does not change results, only host speed): **on by default**; `--no-routine-accel` / `routineAccel: false` turn it
    /// off. See `armv7m::accel` and DESIGN.md 16.2.
    pub routine_accel: bool,
    /// Verification mode of the routine acceleration: every memo hit is replayed *and* interpreted and the results compared
    /// (slow; for tests and `--verify-routine-accel`). Only has an effect while `routine_accel` is on.
    pub routine_accel_shadow: bool,
    /// `--adc-sample` (0..=4095): the synthetic handset board-ID ADC value.
    pub adc_sample: u32,
    /// `--paused`: start with execution stopped.
    pub start_paused: bool,
    /// The main board's I2C idle-high fixture (PB6/PB7/PB10/PB11 driven high before the first instruction, as `main.resc` of
    /// the analysis workspace does): **on by default**; `--no-i2c-idle-high` / `i2cIdleHigh: false` turn it off. A functional idle-line
    /// fixture, not electrical I2C modeling; the state names it (`i2cIdleHigh`, `i2cFixture`).
    pub i2c_idle_high: bool,
    /// The host's random nonce of the output-history epoch: `outputHistoryEpoch` is `"<historyNonce>-<generation>"`, where the
    /// generation counts the history domains of this session (creation, every restart/cold/wake/serial/reset, every machine
    /// reset). The browser passes a fresh random value per session so that two sessions never share an epoch.
    pub history_nonce: u64,
    /// The pre-boot EEPROM consistency fixture (`decoStorageFixture`, `--no-deco-storage-fixture`; **on by default**, see
    /// [`crate::deco`]): before every board creation, a stored tissue block that is entirely erased while the saved
    /// decompression date is set loses the date record, so that the firmware takes its own four-day reset path instead of
    /// loading NaN tissues. TRITON only. An emulator fixture; the state names it (`decoStorageFixture`).
    pub deco_storage_fixture: bool,
    /// The start-at-the-surface fixture (`startAtSurface`, `--no-start-at-surface`; **on by default**, see
    /// [`crate::surface_start`]): every board creation starts both pressure inputs at [`SessionConfig::surface_pressure_mbar`]
    /// plus each sensor's offset (depth 0), and a new session also resets the oxygen cells to their defaults. The state
    /// names it (`startAtSurface`).
    pub start_at_surface: bool,
    /// The surface pressure in mbar (`surfacePressureMbar`, 100 to 30000) the start-at-the-surface fixture uses; the
    /// `reset`, `cold`, `wake` and `serial` actions may carry a new value of it.
    pub surface_pressure_mbar: f64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dual,
            boot_mode: BootMode::HandsetWake,
            simultaneous_start: false,
            idle_fast_forward: true,
            routine_accel: true,
            routine_accel_shadow: false,
            adc_sample: crate::handset::DEFAULT_ADC_SAMPLE,
            start_paused: false,
            i2c_idle_high: true,
            history_nonce: 0,
            deco_storage_fixture: true,
            start_at_surface: true,
            surface_pressure_mbar: surface_start::DEFAULT_SURFACE_MBAR,
        }
    }
}

/// The evidence of `action capture`: the files of a runner `captures/<UTC timestamp>/` folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// `state.json` (the state document, `json.dumps(indent=2)` layout).
    pub state_json: String,
    /// `lcd.png` (8-bit RGB).
    pub lcd_png: Vec<u8>,
    /// `can-trace.tsv` (empty for a handset-only session, which writes no trace file).
    pub can_trace_tsv: String,
    /// Folder name: the UTC timestamp `%Y%m%dT%H%M%S%fZ` when the host supplied the time, else `virtual-<ns>`.
    pub name: String,
}

/// What a [`Session::run_for`] call did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutcome {
    /// Virtual nanoseconds advanced by the call (0 while paused, in standby or after an error stop).
    pub advanced_ns: Time,
    /// Virtual time after the call, in nanoseconds.
    pub virtual_ns: Time,
    /// The runner's `running` flag after the call.
    pub running: bool,
    pub standby: bool,
    pub error: Option<String>,
    /// The call ended the run (standby request or error stop).
    pub stopped: bool,
    /// The LCD frame version (changes when visible pixels change; see [`Session::frame`]).
    pub frame_version: u64,
}

/// The handset display as RGBA8888, valid until the next call on the session.
pub struct FrameView<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
    pub version: u64,
}

/// Information only the host has (wall-clock measurements), shown in the state document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostInfo {
    /// `hostPacing` text (default: `HOST_PACING_DEFAULT`).
    pub pacing: Option<String>,
    /// `realtimeFactor`: virtual seconds per wall second, measured by the host.
    pub realtime_factor: Option<f64>,
}

#[derive(Default)]
struct Dirty {
    inputs: bool,
    led_colors: bool,
    rtc: bool,
    storage: bool,
}

struct RtcBook {
    /// The content of the persisted `rtc-state.json`.
    saved: RtcState,
    /// `rtc_state_ready`: a checkpoint may be written from the live RTCs.
    ready: bool,
    /// `rtc_provenance` of the last restore.
    provenance: Vec<(String, Provenance)>,
    info: RtcInfo,
}

/// Prefix of the backing labels shown in the `storageSummary`/`flashSummary`/`rtcPersistence` texts.
#[derive(Clone, Debug, Default)]
struct Labels {
    prefix: String,
}

impl Labels {
    fn eeprom(&self) -> String {
        format!("{}{}", self.prefix, EEPROM_FILE)
    }
    fn nor(&self) -> String {
        format!("{}{}", self.prefix, NOR_FILE)
    }
    fn rtc(&self) -> String {
        format!("{}{}", self.prefix, RTC_STATE_FILE)
    }
}

/// A system that was just built and restored.
struct Launched {
    system: System,
    provenance: Vec<(String, Provenance)>,
    info: RtcInfo,
    /// What the pre-boot EEPROM consistency fixture did for this board creation.
    deco_storage: StorageFixture,
    /// What the start-at-the-surface fixture did for this board creation.
    surface_start: SurfaceStart,
}

/// The session: system, controls, state, persistence.
pub struct Session {
    config: SessionConfig,
    system: System,
    firmware: Vec<FirmwareDescriptor>,
    /// `state["running"]`.
    running: bool,
    /// `state["error"]`.
    error: Option<String>,
    led_colors: LedColors,
    led_colors_persisted: bool,
    /// What the runner's data directory holds (kept in memory).
    stored: Profile,
    dirty: Dirty,
    rtc: RtcBook,
    /// The pre-boot EEPROM consistency fixture of the last board creation (`decoStorageFixture` of the state).
    deco_storage: StorageFixture,
    /// The start-at-the-surface fixture of the last board creation (`startAtSurface` of the state).
    surface_start: SurfaceStart,
    /// `storage_state_ready`: EEPROM and NOR may be saved from the live system.
    storage_ready: bool,
    /// A Restart/cold/wake failed half way: nothing runs until the next successful launch.
    failed: bool,
    last_capture: Option<String>,
    pending_captures: Vec<Capture>,
    host: HostInfo,
    labels: Labels,
    utc_micros: Option<i64>,
    /// Generation of the output-history epoch before the current system's own machine resets are added (see
    /// [`SessionConfig::history_nonce`]).
    history_generation: u64,
    capture_counter: u64,
}

const TERMINAL_HANDLER_MESSAGE: &str = "Firmware stopped in a terminal handler";

/// The refusal of the serial fixture for a value outside nine decimal digits (the physical serial is a 9-digit number;
/// the fixture stores it as the EEPROM `uint32` at offset 0).
pub const SERIAL_RANGE_MESSAGE: &str = "Serial number must be an integer from 0 to 999999999 (at most nine digits)";

impl Session {
    /// Builds the system from the firmware and the profile and starts it (`Emulator.__init__` and `_launch`).
    ///
    /// Fails like the runner's startup: an invalid `rtc-state.json`, `inputs.json` or `led-colors.json`, an EEPROM image
    /// of the wrong size, an incompatible NOR image or a missing main firmware in the dual mode.
    pub fn new(config: SessionConfig, main: Option<&Firmware>, handset: &Firmware, profile: Profile) -> Result<Session, String> {
        Session::new_with(BuildOptions { main_i2c_idle_high: config.i2c_idle_high }, "", config, main, handset, profile)
    }

    /// Like [`Session::new`], with a prefix for the backing labels in the state document (for example the data
    /// directory `emulation/runtime/dual/` of the analysis workspace).
    pub fn new_in(label_prefix: &str, config: SessionConfig, main: Option<&Firmware>, handset: &Firmware, profile: Profile) -> Result<Session, String> {
        Session::new_with(BuildOptions { main_i2c_idle_high: config.i2c_idle_high }, label_prefix, config, main, handset, profile)
    }

    /// The general constructor: platform-script [`BuildOptions`] (the main I2C idle-high fixture of the current
    /// `main.resc`; they override `config.i2c_idle_high`) and the backing-label prefix.
    pub fn new_with(
        options: BuildOptions,
        label_prefix: &str,
        mut config: SessionConfig,
        main: Option<&Firmware>,
        handset: &Firmware,
        profile: Profile,
    ) -> Result<Session, String> {
        config.i2c_idle_high = options.main_i2c_idle_high;
        let dual = config.mode == Mode::Dual;
        if dual && main.is_none() {
            return Err("the dual system needs the main firmware".to_string());
        }
        if let Some(main) = main.filter(|_| dual) {
            // A mixed pair is refused before anything else is built, with the engine's message.
            crate::firmware::common_release(main, handset)?;
        }
        if config.adc_sample > 4095 {
            return Err("the ADC sample must be in 0..=4095".to_string());
        }
        surface_start::validate_surface(config.surface_pressure_mbar)?;
        let labels = Labels { prefix: label_prefix.to_string() };
        // Emulator.__init__: inputs.json only for a dual run; led-colors.json for every run.
        let inputs = match (&profile.inputs, dual) {
            (Some(text), true) => parse_inputs_file(text).map_err(|e| format!("inputs.json: {e}"))?,
            _ => Inputs::defaults(),
        };
        let led_colors = match &profile.led_colors {
            Some(text) => LedColors::from_file_text(text)?,
            None => LedColors::default(),
        };
        let saved = match &profile.rtc_state {
            Some(text) => RtcState::parse(text, &labels.rtc())?,
            None => RtcState::empty(),
        };
        let launched = launch_system(&config, options, main, handset, config.boot_mode, &inputs, true, &profile, &saved, &labels)
            .map_err(|e| format!("Emulator startup failed: {e}"))?;
        let mut firmware = Vec::new();
        if let (true, Some(main)) = (dual, main) {
            firmware.push(FirmwareDescriptor::of(main));
        }
        firmware.push(FirmwareDescriptor::of(handset));
        let session = Session {
            running: !config.start_paused,
            error: None,
            led_colors_persisted: profile.led_colors.is_some(),
            led_colors,
            stored: profile,
            dirty: Dirty { inputs: dual, ..Dirty::default() },
            rtc: RtcBook { saved, ready: true, provenance: launched.provenance, info: launched.info },
            deco_storage: launched.deco_storage,
            surface_start: launched.surface_start,
            storage_ready: dual,
            failed: false,
            last_capture: None,
            pending_captures: Vec::new(),
            host: HostInfo::default(),
            labels,
            utc_micros: None,
            history_generation: 1,
            capture_counter: 0,
            firmware,
            system: launched.system,
            config,
        };
        Ok(session)
    }

    // ---- running ----------------------------------------------------------------------------

    /// Advances virtual time by `virtual_seconds` (rounded up to whole 100 us quanta) while the session is running.
    /// Does nothing while paused, in standby, after an error stop or after a failed restart. The host paces the calls
    /// against wall time; the engine never reads a clock.
    pub fn run_for(&mut self, virtual_seconds: f64) -> RunOutcome {
        let before = self.system.time();
        if self.running && !self.failed && virtual_seconds.is_finite() && virtual_seconds > 0.0 {
            self.system.run_for_secs(virtual_seconds);
        }
        let stopped = self.update_stop_state();
        RunOutcome {
            advanced_ns: self.system.time() - before,
            virtual_ns: self.system.time(),
            running: self.running,
            standby: self.system.is_standby(),
            error: self.error.clone(),
            stopped,
            frame_version: self.frame_version(),
        }
    }

    /// The runner's `running` flag: execution is on (not paused, not in standby, no error stop).
    pub fn running(&self) -> bool {
        self.running
    }

    /// Virtual time in nanoseconds.
    pub fn virtual_ns(&self) -> Time {
        self.system.time()
    }

    pub fn standby(&self) -> bool {
        self.system.is_standby()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The system (read-only), for evidence readbacks in tests and scenarios.
    pub fn system(&self) -> &System {
        &self.system
    }

    /// The system, mutable: tests and scenarios use it for fixtures the runner's controls do not offer. Hosts do not.
    pub fn system_mut(&mut self) -> &mut System {
        &mut self.system
    }

    /// Exact idle-loop fast-forward on or off (results are identical either way).
    pub fn set_idle_fast_forward(&mut self, enabled: bool) {
        self.config.idle_fast_forward = enabled;
        self.system.set_idle_fast_forward(enabled);
    }

    /// Routine acceleration on or off (results are identical either way); `shadow` selects the verification mode.
    pub fn set_routine_accel(&mut self, enabled: bool, shadow: bool) {
        self.config.routine_accel = enabled;
        self.config.routine_accel_shadow = shadow;
        self.system.set_routine_accel(routine_accel_mode(&self.config));
    }

    /// Wall-clock information for `hostPacing` / `realtimeFactor` of the state document.
    pub fn set_host_info(&mut self, host: HostInfo) {
        self.host = host;
    }

    /// The current UTC time in microseconds since the Unix epoch, for capture names (the engine has no clock).
    pub fn set_utc_micros(&mut self, micros: i64) {
        self.utc_micros = Some(micros);
    }

    /// Kept for hosts that still call it: the `outputHistoryEpoch` is `"<historyNonce>-<generation>"` with the nonce of the
    /// [`SessionConfig`], so host entropy no longer enters here and the call has no effect.
    pub fn set_epoch_seed(&mut self, _seed: u64) {}

    /// The output-history epoch of the state document: `"<historyNonce>-<generation>"`. The generation starts at 1 with
    /// the session and counts every board recreation (Restart, Cold, Wake, serial, reset) and every machine reset, because
    /// each of them clears the output activity histories.
    pub fn output_history_epoch(&self) -> String {
        format!("{}-{}", self.config.history_nonce, self.history_generation + self.system.machine_reset_count())
    }

    // ---- state ------------------------------------------------------------------------------------

    /// The state document of the viewer (`/api/state`) as compact JSON.
    pub fn state_json(&self) -> String {
        self.state().to_string_with(&WriteOptions::compact())
    }

    /// The state document as a JSON value.
    pub fn state(&self) -> Json {
        let pacing = self.host.pacing.as_deref().unwrap_or(state::HOST_PACING_DEFAULT);
        let epoch = self.output_history_epoch();
        state::build(&StateView {
            system: &self.system,
            running: self.running,
            error: self.error.as_deref(),
            led_colors: &self.led_colors,
            firmware: &self.firmware,
            rtc: &self.rtc.info,
            last_capture: self.last_capture.as_deref(),
            host_pacing: pacing,
            realtime_factor: self.host.realtime_factor,
            output_history_epoch: &epoch,
            deco_storage: &self.deco_storage,
            surface_start: &self.surface_start,
        })
    }

    fn lcd_id(&self) -> emu_core::PeriphId {
        self.system.handset.ids.lcd
    }

    fn frame_version(&self) -> u64 {
        self.system.handset.board.get::<NgcParallelLcd>(self.lcd_id()).map_or(0, NgcParallelLcd::frame_version)
    }

    /// The LCD as RGBA (the visible buffer, brought up to date first).
    pub fn frame(&mut self) -> FrameView<'_> {
        let id = self.lcd_id();
        let lcd = self.system.handset.board.get_mut::<NgcParallelLcd>(id).expect("the handset has an LCD");
        let version = lcd.sync_frame();
        let (width, height) = (lcd.width() as u32, lcd.height() as u32);
        let rgba = lcd.frame_rgba();
        FrameView { width, height, rgba, version }
    }

    /// `lcd.png` of the runner: the LCD as an 8-bit RGB PNG.
    pub fn lcd_png(&mut self) -> Vec<u8> {
        let ppm = self.system.lcd_ppm().unwrap_or_default();
        png::encode_ppm(&ppm, 1).unwrap_or_default()
    }

    // ---- profile ----------------------------------------------------------------------------------

    /// The profile items that changed since the last call (`None` when nothing did): EEPROM and NOR when the models asked
    /// for a save (the runner's backing files are rewritten after every completed transaction), `inputs.json` and
    /// `led-colors.json` after the actions that rewrite them, and `rtc-state.json` after a checkpoint was saved
    /// (Restart, cold, wake, serial, [`Session::export_profile`], [`Session::shutdown`]).
    pub fn take_profile_changes(&mut self) -> Option<Profile> {
        let mut changes = Profile::default();
        if self.storage_ready {
            if let Some(main) = self.system.main.as_mut() {
                if main.eeprom.take_persist_request() {
                    changes.eeprom = Some(main.eeprom.image().to_vec());
                }
                let qspi_id = main.ids.qspi;
                if let Some(qspi) = main.board.get_mut::<NgcQuadSpi>(qspi_id) {
                    if qspi.take_persist_request() {
                        changes.nor = Some(qspi.serialize_backing());
                    }
                }
            }
        }
        if std::mem::take(&mut self.dirty.storage) {
            changes.eeprom = changes.eeprom.take().or_else(|| self.stored.eeprom.clone());
            changes.nor = changes.nor.take().or_else(|| self.stored.nor.clone());
        }
        if std::mem::take(&mut self.dirty.inputs) && self.config.mode == Mode::Dual {
            changes.inputs = Some(inputs_file_text(self.system.inputs()));
        }
        if std::mem::take(&mut self.dirty.led_colors) {
            changes.led_colors = Some(self.led_colors.file_text());
        }
        if std::mem::take(&mut self.dirty.rtc) {
            changes.rtc_state = self.stored.rtc_state.clone();
        }
        self.stored.merge(Profile { eeprom: changes.eeprom.clone(), nor: changes.nor.clone(), inputs: changes.inputs.clone(), led_colors: changes.led_colors.clone(), rtc_state: None });
        if changes.is_empty() {
            None
        } else {
            Some(changes)
        }
    }

    /// The whole profile with a fresh RTC checkpoint of the live boards (what a clean close would leave behind), without
    /// stopping the session.
    pub fn export_profile(&mut self) -> Profile {
        if self.rtc.ready {
            let _ = self.save_rtc_checkpoint();
        }
        if self.storage_ready {
            self.save_storage();
        }
        self.full_profile()
    }

    /// Runner close (`Emulator.close`/`shutdown_process`): saves the RTC checkpoint of both boards and the storage, and
    /// returns the whole profile. When the checkpoint cannot be saved the previous valid one is returned.
    pub fn shutdown(mut self) -> Profile {
        let _ = self.shutdown_process();
        self.full_profile()
    }

    fn full_profile(&self) -> Profile {
        Profile {
            eeprom: self.stored.eeprom.clone(),
            nor: self.stored.nor.clone(),
            rtc_state: self.stored.rtc_state.clone(),
            inputs: (self.config.mode == Mode::Dual).then(|| inputs_file_text(self.system.inputs())),
            led_colors: self.led_colors_persisted.then(|| self.led_colors.file_text()),
        }
    }

    /// `save_rtc_state` for the boards of this session; keeps the saved state of boards that are not running.
    fn save_rtc_checkpoint(&mut self) -> Result<(), String> {
        let mut live: Vec<(&str, stm32::rtc::RtcCheckpoint)> = Vec::new();
        if self.config.mode == Mode::Dual {
            live.push((BOARD_MAIN, self.system.rtc_checkpoint(Which::Main).ok_or("the main RTC is missing")?));
        }
        live.push((BOARD_HANDSET, self.system.rtc_checkpoint(Which::Handset).ok_or("the handset RTC is missing")?));
        let next = self.rtc.saved.capture(&live, &self.rtc.provenance)?;
        self.stored.rtc_state = Some(next.to_file_text());
        self.rtc.saved = next;
        self.dirty.rtc = true;
        Ok(())
    }

    /// `SaveBackingFile` of the EEPROM store and the NOR flash.
    fn save_storage(&mut self) {
        if let Some(main) = self.system.main.as_ref() {
            self.stored.eeprom = Some(main.eeprom.image().to_vec());
            if let Some(qspi) = main.board.get::<NgcQuadSpi>(main.ids.qspi) {
                self.stored.nor = Some(qspi.serialize_backing());
            }
            self.dirty.storage = true;
        }
    }

    /// `shutdown_process()`: checkpoint the RTCs, then save the storage. A failed RTC save is reported after the storage
    /// was saved ("Retention failure is visible to reset/close callers").
    fn shutdown_process(&mut self) -> Result<(), String> {
        let mut rtc_error = None;
        if self.rtc.ready {
            if let Err(error) = self.save_rtc_checkpoint() {
                rtc_error = Some(error);
            }
        }
        self.rtc.ready = false;
        if self.storage_ready {
            self.save_storage();
        }
        self.storage_ready = false;
        match rtc_error {
            Some(error) => Err(format!("RTC checkpoint save failed: {error}")),
            None => Ok(()),
        }
    }

    // ---- launching --------------------------------------------------------------------------------

    /// `Emulator.launch(boot_mode)`: Restart (`None`: the configured boot mode), Cold or Wake.
    fn launch(&mut self, boot_mode: Option<BootMode>) -> Result<(), String> {
        let result = self.launch_inner(boot_mode);
        if let Err(error) = &result {
            // A replacement system must never run or checkpoint partially restored devices.
            self.running = false;
            self.error = Some(format!("Emulator startup failed: {error}"));
            self.rtc.ready = false;
            self.storage_ready = false;
            self.failed = true;
        }
        result
    }

    fn launch_inner(&mut self, boot_mode: Option<BootMode>) -> Result<(), String> {
        self.shutdown_process()?;
        let chosen = boot_mode.unwrap_or(self.config.boot_mode);
        let inputs = self.system.inputs().clone();
        let launched = launch_system(
            &self.config,
            self.system.options(),
            self.system.main_firmware(),
            self.system.handset_firmware(),
            chosen,
            &inputs,
            false,
            &self.stored,
            &self.rtc.saved,
            &self.labels,
        )?;
        // A new history domain: the generation moves past everything the replaced system had used.
        self.history_generation += self.system.machine_reset_count() + 1;
        self.system = launched.system;
        self.rtc.provenance = launched.provenance;
        self.rtc.info = launched.info;
        self.deco_storage = launched.deco_storage;
        self.surface_start = launched.surface_start;
        self.rtc.ready = true;
        self.storage_ready = self.config.mode == Mode::Dual;
        self.failed = false;
        self.error = None;
        self.dirty.inputs |= self.config.mode == Mode::Dual;
        Ok(())
    }

    // ---- stop state -------------------------------------------------------------------------------

    /// `snapshot()` side effects and the run loop's stop conditions: observed standby or the handset's terminal
    /// handler end the run (`running = false`, with the error text for the latter). Returns true if this call stopped it.
    fn update_stop_state(&mut self) -> bool {
        let was_running = self.running;
        if self.system.is_standby() {
            self.running = false;
        }
        let terminal_pc = self.system.terminal_handler_pc().filter(|pc| self.system.pc(Which::Handset) == Some(*pc));
        if terminal_pc.is_some() || self.system.error().is_some() {
            if self.error.is_none() {
                self.error = Some(match (self.system.error(), terminal_pc) {
                    (Some(text), _) => text.to_string(),
                    (None, pc) => format!("{TERMINAL_HANDLER_MESSAGE} at 0x{:08x}; inspect the log.", pc.unwrap_or(0)),
                });
            }
            self.running = false;
        }
        was_running && !self.running
    }

    // ---- actions ----------------------------------------------------------------------------------

    /// One `POST /api/action` of the runner. `request_json` is the body (`{"action": "...", ...}`); the answer is the
    /// state document. Errors carry the runner's messages.
    pub fn action(&mut self, request_json: &str) -> Result<String, String> {
        let request = actions::parse_request(request_json)?;
        self.perform(&request)?;
        self.update_stop_state();
        Ok(self.state_json())
    }

    fn perform(&mut self, request: &Request) -> Result<(), String> {
        let payload = &request.payload;
        let dual = self.config.mode == Mode::Dual;
        match request.name() {
            Some("pause") => self.running = false,
            Some("resume") => {
                if self.system.is_standby() {
                    return Err("The firmware requested standby; use Wake system".to_string());
                }
                self.running = true;
                self.error = None;
                self.system.clear_error();
            }
            Some("step") => {
                self.running = false;
                self.run_interval(0.05);
                self.run_interval(0.05);
            }
            Some("advance") => {
                let seconds = match payload.get("seconds") {
                    Some(value) => python_float(value)?,
                    None => 1.0,
                };
                if !seconds.is_finite() || seconds <= 0.0 || seconds > 20.0 {
                    return Err("Advance must be between zero and 20 virtual seconds".to_string());
                }
                self.running = false;
                let mut remaining = seconds;
                while remaining > 0.000001 {
                    let interval = remaining.min(0.05);
                    self.run_interval(interval);
                    remaining -= interval;
                }
            }
            Some(name @ ("reset" | "cold" | "wake")) => {
                if name == "cold" && dual {
                    // Refused before anything is shut down: the session keeps running.
                    if let Some(reason) = self.system.release().cold_boot_refusal {
                        return Err(reason.to_string());
                    }
                }
                self.take_surface_pressure(payload)?;
                let running = self.running;
                let mode = match name {
                    "cold" => Some(BootMode::Cold),
                    "wake" => Some(BootMode::HandsetWake),
                    _ => None,
                };
                self.launch(mode)?;
                self.running = running;
            }
            Some("up") => self.system.apply_input(&Input::Navigate { up: true })?,
            Some("down") => self.system.apply_input(&Input::Navigate { up: false })?,
            Some("confirm") => self.system.apply_input(&Input::Confirm)?,
            Some("can") => {
                if !dual {
                    return Err("CAN controls require --dual".to_string());
                }
                if let Some(connected) = payload.get("connected") {
                    let Json::Bool(connected) = connected else {
                        return Err("connected must be a boolean".to_string());
                    };
                    self.system.apply_input(&Input::CanConnected(*connected))?;
                }
                if let Some(drop_id) = payload.get("dropId") {
                    let valid = match drop_id {
                        Json::Int(id) if (-1..=2047).contains(id) => Some(*id as i32),
                        _ => None,
                    };
                    let Some(id) = valid else {
                        return Err("dropId must be -1 or a standard CAN ID (0..2047)".to_string());
                    };
                    self.system.apply_input(&Input::CanDropId(id))?;
                }
            }
            Some("inputs") => {
                let updates = payload.get("inputs").filter(|u| u.as_object().is_some());
                let Some(updates) = updates else {
                    return Err("inputs must be an object".to_string());
                };
                if !dual {
                    return Err("Sensor controls require --dual".to_string());
                }
                self.system.apply_input(&Input::Sensors(updates.clone()))?;
                self.dirty.inputs = true;
            }
            Some("led-colors") => {
                let colors = payload.get("colors").cloned().unwrap_or(Json::Null);
                self.led_colors.update(&colors)?;
                self.led_colors_persisted = true;
                self.dirty.led_colors = true;
            }
            Some("serial") => {
                if !dual {
                    return Err("Serial fixture requires --dual".to_string());
                }
                let serial = match payload.get("serialNumber") {
                    Some(Json::Int(n)) if (0..=999_999_999).contains(n) => *n as u32,
                    _ => return Err(SERIAL_RANGE_MESSAGE.to_string()),
                };
                let main = self.system.main.as_ref().ok_or("Serial fixture requires --dual")?;
                let marker = main.eeprom.get_byte(crate::fixtures::EEPROM_VALIDITY_OFFSET).map_err(|e| e.to_string())?;
                if marker != crate::fixtures::EEPROM_VALIDITY_MARKER {
                    return Err("Wait for the first boot to initialize EEPROM before changing the emulated serial".to_string());
                }
                self.take_surface_pressure(payload)?;
                let main = self.system.main.as_ref().ok_or("Serial fixture requires --dual")?;
                main.eeprom.set_double_word(0, serial).map_err(|e| e.to_string())?;
                main.eeprom.flush();
                let running = self.running;
                self.launch(None)?;
                self.running = running;
            }
            Some("capture") => {
                let capture = self.capture();
                self.pending_captures.push(capture);
            }
            _ => return Err("Unknown action".to_string()),
        }
        Ok(())
    }

    /// The optional `surfacePressureMbar` of the actions that recreate the boards (`reset`, `cold`, `wake`, `serial`): the
    /// page's surface-pressure setting, which the start-at-the-surface fixture uses from this board creation on. Validated
    /// before anything is shut down.
    fn take_surface_pressure(&mut self, payload: &Json) -> Result<(), String> {
        let Some(value) = payload.get("surfacePressureMbar") else { return Ok(()) };
        let mbar = python_float(value).map_err(|_| surface_start::SURFACE_RANGE_MESSAGE.to_string())?;
        self.config.surface_pressure_mbar = surface_start::validate_surface(mbar)?;
        Ok(())
    }

    /// `run_interval(seconds)`: nothing in standby, else the interval, ignoring an earlier error stop (the runner's
    /// Step and Advance still execute after the terminal handler was hit).
    fn run_interval(&mut self, seconds: f64) {
        if self.system.is_standby() || self.failed {
            return;
        }
        self.system.run_for_secs_ignoring_error(seconds);
    }

    // ---- evidence ---------------------------------------------------------------------------------

    /// `action capture`: the state document, the LCD PNG and the CAN trace. The state document is the one the viewer
    /// shows before the capture's own `lastCapture` entry is added (as in the runner), which then names this capture.
    pub fn capture(&mut self) -> Capture {
        self.update_stop_state();
        let mut text = self.state().to_string_with(&WriteOptions::python_indent2());
        text.push('\n');
        let name = self.capture_name();
        let lcd_png = self.lcd_png();
        let can_trace_tsv = if self.config.mode == Mode::Dual { self.system.link.trace_text() } else { String::new() };
        self.last_capture = Some(format!("captures/{name}"));
        Capture { state_json: text, lcd_png, can_trace_tsv, name }
    }

    /// Captures made by `action capture` that the host has not collected yet.
    pub fn take_captures(&mut self) -> Vec<Capture> {
        std::mem::take(&mut self.pending_captures)
    }

    fn capture_name(&mut self) -> String {
        self.capture_counter += 1;
        let base = match self.utc_micros {
            Some(micros) => utc_name(micros),
            None => format!("virtual-{:015}", self.system.time()),
        };
        if self.last_capture.as_deref().is_some_and(|last| last == format!("captures/{base}")) {
            format!("{base}-{}", self.capture_counter)
        } else {
            base
        }
    }

    /// Sends a classical CAN frame from the handset's controller as the runner's `NGCCANStimulus` does: the frame
    /// is written to the handset's idle transmit mailbox 0, so it travels through the CAN link like firmware traffic.
    pub fn inject_can_from_handset(&mut self, id: u32, data: &[u8]) -> Result<(), String> {
        self.system.inject_can_from_handset(id, data)
    }
}

/// The core-level mode of the session's routine-acceleration switches.
fn routine_accel_mode(config: &SessionConfig) -> armv7m::RoutineAccelMode {
    match (config.routine_accel, config.routine_accel_shadow) {
        (false, _) => armv7m::RoutineAccelMode::Off,
        (true, false) => armv7m::RoutineAccelMode::On,
        (true, true) => armv7m::RoutineAccelMode::Shadow,
    }
}

/// `launch_system`: builds, loads the storage, restores the RTC and applies the boot fixtures, in the runner's order.
/// The two emulator fixtures of the decompression handling act here: the start-at-the-surface fixture on the `inputs` the
/// system is built with (`new_session` is true for [`Session::new`]: the oxygen cells are reset as well) and the pre-boot
/// EEPROM consistency fixture on the stored EEPROM image before it is loaded.
#[allow(clippy::too_many_arguments)]
fn launch_system(
    config: &SessionConfig,
    options: BuildOptions,
    main: Option<&Firmware>,
    handset: &Firmware,
    boot_mode: BootMode,
    inputs: &Inputs,
    new_session: bool,
    stored: &Profile,
    rtc_saved: &RtcState,
    labels: &Labels,
) -> Result<Launched, String> {
    let dual = config.mode == Mode::Dual;
    let (inputs, surface_start) = surface_start::apply(config.start_at_surface, dual, config.surface_pressure_mbar, new_session, inputs);
    let system_config = SystemConfig {
        mode: config.mode,
        boot_mode,
        simultaneous_start: config.simultaneous_start,
        idle_fast_forward: config.idle_fast_forward,
        routine_accel: routine_accel_mode(config),
        adc_sample: config.adc_sample,
        inputs,
        ..SystemConfig::default()
    };
    let mut system = System::build_with(system_config, main, handset, options)?;
    let mut eeprom_image = stored.eeprom.clone();
    let deco_storage = deco::storage_fixture(config.deco_storage_fixture, dual, system.release(), eeprom_image.as_deref_mut());
    if let Some(main_board) = system.main.as_mut() {
        main_board.eeprom.load_backing(&labels.eeprom(), eeprom_image.as_deref()).map_err(|e| e.to_string())?;
        if deco_storage.applied {
            // The repaired image is the profile from now on: ask for it to be saved.
            main_board.eeprom.flush();
        }
        let qspi_id = main_board.ids.qspi;
        let qspi = main_board.board.get_mut::<NgcQuadSpi>(qspi_id).ok_or("the main QSPI is missing")?;
        qspi.load_backing(&labels.nor(), stored.nor.as_deref()).map_err(|e| e.to_string())?;
    }
    // restore_rtc_state(client, rtc_state, rtc_boards, allow_eeprom_seed=dual)
    let names: Vec<&str> = if dual { vec![BOARD_MAIN, BOARD_HANDSET] } else { vec![BOARD_HANDSET] };
    let live_main = system.rtc_checkpoint(Which::Main);
    let eeprom_image = system.main.as_ref().map(|m| m.eeprom.image());
    let plans = plan_restore(rtc_saved, &names, dual, eeprom_image.as_ref().map(|i| &i[..]), live_main.as_ref());
    let mut provenance = Vec::new();
    for (name, board) in &plans {
        let which = if name == BOARD_MAIN { Which::Main } else { Which::Handset };
        system.restore_rtc_checkpoint(which, &board.checkpoint)?;
        provenance.push((name.clone(), board.provenance.clone()));
    }
    let info = RtcInfo {
        path: labels.rtc(),
        restored_boards: names.iter().filter(|name| rtc_saved.board(name).is_some()).map(|name| name.to_string()).collect(),
        sources: names
            .iter()
            .map(|name| (name.to_string(), provenance.iter().find(|(n, _)| n == name).map(|(_, p)| p.clone()).unwrap_or(Provenance::FreshRtc)))
            .collect(),
        main_bkp1_wake_override: dual && boot_mode == BootMode::HandsetWake,
    };
    system.apply_boot_fixtures()?;
    Ok(Launched { system, provenance, info, deco_storage, surface_start })
}

/// `%Y%m%dT%H%M%S%fZ` of a UTC time in microseconds since the Unix epoch.
fn utc_name(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}{fraction:06}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_names_follow_strftime() {
        // 2026-10-07 09:09:40.955472 UTC (the capture folder of the connected validation run).
        let micros = 1_791_364_180_955_472;
        assert_eq!(utc_name(micros), "20261007T090940955472Z");
        assert_eq!(utc_name(0), "19700101T000000000000Z");
        assert_eq!(utc_name(951_782_400_000_000 + 86_399_999_999), "20000229T235959999999Z");
    }
}
