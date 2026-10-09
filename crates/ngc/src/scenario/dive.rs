//! The committed dive benchmark (DESIGN.md 16.3): a deterministic valid-tissue dive of the original TRITON firmware, built
//! from a fresh profile **only through firmware routes**, with speed measurements and fingerprints at checkpoints.
//!
//! # The profile
//!
//! Every stage reopens the profile of the previous one, like closing and reopening the viewer:
//!
//! 1. **Battery wizard** (12.5 s): Photolithium for both banks, through the handset's buttons.
//! 2. **Air calibration** (27 s): the handset's calibration menu (gas Air, pressure Auto), ~9 s test, Save.
//! 3. **Short NaN dive** (150 s): with the saved calibration but without a stored decompression date, the firmware's tissue
//!    state is NaN (the main board then takes a cheap path); the dive makes it save a *last decompression date* in EEPROM.
//! 4. **Clock fixture**: the main board's saved RTC checkpoint is advanced by 5 days (an explicit profile fixture: four or
//!    more days make the original firmware reinitialize its tissue pressures through its own start-up path) and the saved
//!    sensor inputs are the surface defaults.
//! 5. **Restart and recalibration**: the aged-calibration notification, "LOW PPO2! On the loop?" answered NO, the menu and a
//!    second air calibration (the calibration age expired with the clock jump), then the depth is applied.
//!
//! # The dives
//!
//! From that profile one session per depth (20 m, 30 m; EN13319 water at 1013.25 mbar surface pressure, cells at 1.5 bar
//! ppO2 equivalent) runs the recalibration, applies the depth at 34 s, acknowledges the "Bubble check!" notification at 42 s
//! and keeps diving for `dive_seconds`. The valid tissues make the main board's decompression code run for real: bursts of
//! compute (about 1.4 s every 4 s) in which the main board retires 100 M instructions per second, between quiet periods in
//! which it idles.
//!
//! # Driving and measuring
//!
//! Sessions are driven like the browser worker does: 10 virtual-ms slices (`Session::run_for(0.01)`), actions applied between
//! slices at the first slice boundary at or after their time. Native and WebAssembly runs therefore produce the same
//! fingerprints. Speed is measured per 0.25 s step (wall time of `run_for` only, no frame or state readout); a step in
//! which the main board executes at least [`BURST_EXECUTED_MIPS`] million non-idle instructions per virtual second is a
//! **burst** step. That classification uses retire counts minus idle-skipped instructions, which are identical with routine
//! acceleration on and off.

use super::ScenarioEnv;
use crate::persistence::{Profile, RtcState, BOARD_MAIN};
use crate::session::{Session, SessionConfig};
use crate::sha256;
use crate::system::{RoutineAccelMode, Which};
use emu_core::{Json, Width};

/// Virtual seconds per `run_for` call (the browser worker's slice).
pub const SLICE_SECONDS: f64 = 0.01;
/// Virtual seconds per speed step.
pub const STEP_SECONDS: f64 = 0.25;
/// Executed (not idle-skipped) main-board instructions per virtual second, in millions, from which a step is a burst.
pub const BURST_EXECUTED_MIPS: f64 = 40.0;
/// Checkpoints inside a dive are taken at multiples of this many virtual seconds.
pub const CHECKPOINT_SECONDS: f64 = 10.0;

const NAN_DIVE_SECONDS: f64 = 150.0;
/// Days the main RTC checkpoint is advanced by (>= 4 triggers the firmware's own tissue reinitialization).
const CLOCK_JUMP_DAYS: u32 = 5;
/// Seconds into a dive session at which the depth is applied and the bubble check acknowledged.
const DEPTH_AT: f64 = 34.0;
const BUBBLE_AT: f64 = 42.0;

/// Main RAM words of the TRITON firmware: the first tissue word (a float; all ones is the NaN state) and the raw NDL.
const TISSUE_WORD: u32 = 0x2000_1EAC;
const RAW_NDL_WORD: u32 = 0x2000_2108;

const DOWN: &str = r#"{"action":"down"}"#;
const UP: &str = r#"{"action":"up"}"#;
const CONFIRM: &str = r#"{"action":"confirm"}"#;

/// A depth of the benchmark.
#[derive(Clone, Copy, Debug)]
pub struct Depth {
    pub name: &'static str,
    pub meters: f64,
    /// Pressure of both sensors in mbar (1013.25 + 1020 * 9.80665 * meters / 100, rounded to 0.1 mbar).
    pub mbar: f64,
}

pub const DEPTH_20M: Depth = Depth { name: "20 m", meters: 20.0, mbar: 3013.3 };
pub const DEPTH_30M: Depth = Depth { name: "30 m", meters: 30.0, mbar: 4014.1 };

/// What to run.
#[derive(Clone, Debug)]
pub struct DiveConfig {
    pub routine_accel: RoutineAccelMode,
    pub idle_fast_forward: bool,
    pub depths: Vec<Depth>,
    /// Virtual seconds of diving after the bubble check, per depth.
    pub dive_seconds: f64,
    /// Keep the last LCD frame of every dive as a PNG (`DiveRun::final_png`).
    pub keep_images: bool,
}

impl Default for DiveConfig {
    fn default() -> Self {
        DiveConfig { routine_accel: RoutineAccelMode::On, idle_fast_forward: true, depths: vec![DEPTH_20M, DEPTH_30M], dive_seconds: 60.0, keep_images: false }
    }
}

/// The state of both boards at a point of the run, as digests (equal for runs that must be identical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub name: String,
    pub virtual_ns: u64,
    /// `System::fingerprint` (registers, both SRAMs, LCD, CAN trace, UART tails, output histories).
    pub fingerprint: String,
    /// `Cpu::exactness_digest` of the main and handset cores (FPSCR, VFP registers, predecode cache, cut-block history).
    pub exact: [u64; 2],
    /// Retire counts of the main and handset cores.
    pub instructions: [u64; 2],
    pub lcd_sha256: String,
}

impl Checkpoint {
    pub fn to_json(&self) -> Json {
        Json::object()
            .with("name", self.name.as_str())
            .with("virtualNs", self.virtual_ns)
            .with("fingerprint", self.fingerprint.as_str())
            .with("exactMain", format!("{:016x}", self.exact[0]))
            .with("exactHandset", format!("{:016x}", self.exact[1]))
            .with("instructionsMain", self.instructions[0])
            .with("instructionsHandset", self.instructions[1])
            .with("lcdSha256", self.lcd_sha256.as_str())
    }
}

/// One speed step of [`STEP_SECONDS`] virtual seconds.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub virtual_seconds: f64,
    pub wall_seconds: f64,
    /// Main-board instructions retired minus idle-skipped ones.
    pub main_executed: u64,
    pub burst: bool,
}

/// Speed summary of a set of steps.
#[derive(Clone, Copy, Debug, Default)]
pub struct Speed {
    pub steps: u32,
    pub virtual_seconds: f64,
    pub wall_seconds: f64,
}

impl Speed {
    pub fn factor(&self) -> f64 {
        if self.wall_seconds > 0.0 {
            self.virtual_seconds / self.wall_seconds
        } else {
            f64::INFINITY
        }
    }

    fn add(&mut self, step: &Step) {
        self.steps += 1;
        self.virtual_seconds += step.virtual_seconds;
        self.wall_seconds += step.wall_seconds;
    }

    fn to_json(self) -> Json {
        Json::object().with("steps", u64::from(self.steps)).with("virtualSeconds", self.virtual_seconds).with("wallSeconds", self.wall_seconds).with("factor", self.factor())
    }
}

/// One stage of the profile construction.
#[derive(Clone, Debug)]
pub struct Stage {
    pub name: String,
    pub virtual_seconds: f64,
    pub wall_seconds: f64,
    pub checkpoint: Checkpoint,
}

/// One dive.
#[derive(Clone, Debug)]
pub struct DiveRun {
    pub depth: Depth,
    pub checkpoints: Vec<Checkpoint>,
    pub steps: Vec<Step>,
    /// Calls of the accelerated routines replaced by memo entries, and the instructions those calls would have retired.
    pub replaced_calls: u64,
    pub replaced_instructions: u64,
    /// The tissue state before the depth was applied was finite (the benchmark's premise).
    pub tissues_finite: bool,
    pub tissue_word: u32,
    pub raw_ndl: u32,
    pub shadow_mismatches: u64,
    /// The last LCD frame (PNG) when `DiveConfig::keep_images` is set.
    pub final_png: Vec<u8>,
    /// Application state at the end: the main firmware's screen and mode bytes.
    pub screen_mode: Option<u32>,
    pub main_mode: Option<u32>,
}

impl DiveRun {
    /// Average, burst-step and quiet-step speed.
    pub fn speeds(&self) -> (Speed, Speed, Speed) {
        let (mut all, mut burst, mut quiet) = (Speed::default(), Speed::default(), Speed::default());
        for step in &self.steps {
            all.add(step);
            if step.burst {
                burst.add(step);
            } else {
                quiet.add(step);
            }
        }
        (all, burst, quiet)
    }

    /// The slowest burst step factor over windows of ~1.5 s of consecutive burst steps (a burst is 1.4 s long).
    pub fn worst_burst_factor(&self) -> f64 {
        let mut worst = f64::INFINITY;
        let mut run = Speed::default();
        for step in &self.steps {
            if step.burst {
                run.add(step);
            } else {
                if run.steps >= 3 {
                    worst = worst.min(run.factor());
                }
                run = Speed::default();
            }
        }
        if run.steps >= 3 {
            worst = worst.min(run.factor());
        }
        worst
    }
}

/// The result of a benchmark run.
#[derive(Clone, Debug)]
pub struct DiveReport {
    pub config: DiveConfig,
    pub stages: Vec<Stage>,
    pub dives: Vec<DiveRun>,
}

impl DiveReport {
    /// Every checkpoint in run order.
    pub fn checkpoints(&self) -> Vec<&Checkpoint> {
        self.stages.iter().map(|s| &s.checkpoint).chain(self.dives.iter().flat_map(|d| d.checkpoints.iter())).collect()
    }

    /// Differences between two runs that must be identical (checkpoint digests, retire counts); empty when identical.
    pub fn differences(&self, other: &DiveReport) -> Vec<String> {
        let (a, b) = (self.checkpoints(), other.checkpoints());
        let mut out = Vec::new();
        if a.len() != b.len() {
            out.push(format!("{} checkpoints against {}", a.len(), b.len()));
        }
        for (x, y) in a.iter().zip(b.iter()) {
            if x != y {
                let mut what = Vec::new();
                if x.virtual_ns != y.virtual_ns {
                    what.push("virtual time");
                }
                if x.fingerprint != y.fingerprint {
                    what.push("fingerprint");
                }
                if x.exact != y.exact {
                    what.push("exactness digest");
                }
                if x.instructions != y.instructions {
                    what.push("retire counts");
                }
                if x.lcd_sha256 != y.lcd_sha256 {
                    what.push("LCD");
                }
                out.push(format!("{}: {}", x.name, what.join(", ")));
            }
        }
        out
    }

    pub fn to_json(&self) -> Json {
        let mode = |m: RoutineAccelMode| match m {
            RoutineAccelMode::Off => "off",
            RoutineAccelMode::On => "on",
            RoutineAccelMode::Shadow => "shadow",
        };
        Json::object()
            .with("routineAccel", mode(self.config.routine_accel))
            .with("idleFastForward", self.config.idle_fast_forward)
            .with("diveSeconds", self.config.dive_seconds)
            .with("sliceSeconds", SLICE_SECONDS)
            .with("stepSeconds", STEP_SECONDS)
            .with("burstExecutedMips", BURST_EXECUTED_MIPS)
            .with(
                "stages",
                Json::from_items(self.stages.iter().map(|s| {
                    Json::object()
                        .with("name", s.name.as_str())
                        .with("virtualSeconds", s.virtual_seconds)
                        .with("wallSeconds", s.wall_seconds)
                        .with("factor", s.virtual_seconds / s.wall_seconds.max(1e-12))
                        .with("checkpoint", s.checkpoint.to_json())
                })),
            )
            .with(
                "dives",
                Json::from_items(self.dives.iter().map(|d| {
                    let (all, burst, quiet) = d.speeds();
                    Json::object()
                        .with("depth", d.depth.name)
                        .with("depthMetres", d.depth.meters)
                        .with("pressureMbar", d.depth.mbar)
                        .with("average", all.to_json())
                        .with("burst", burst.to_json())
                        .with("quiet", quiet.to_json())
                        .with("worstBurstFactor", d.worst_burst_factor())
                        .with("replacedCalls", d.replaced_calls)
                        .with("replacedInstructions", d.replaced_instructions)
                        .with("tissuesFinite", d.tissues_finite)
                        .with("tissueWord", format!("{:#010x}", d.tissue_word))
                        .with("rawNdl", d.raw_ndl)
                        .with("screenMode", d.screen_mode.map(u64::from))
                        .with("mainMode", d.main_mode.map(u64::from))
                        .with("shadowMismatches", d.shadow_mismatches)
                        .with("checkpoints", Json::from_items(d.checkpoints.iter().map(Checkpoint::to_json)))
                })),
            )
    }
}

// ---- the timelines -------------------------------------------------------------------------------------------------------

struct Act {
    at: f64,
    request: String,
}

fn act(at: f64, request: &str) -> Act {
    Act { at, request: request.to_string() }
}

fn inputs_request(mbar: f64) -> String {
    format!(r#"{{"action":"inputs","inputs":{{"pressure1Mbar":{mbar},"pressure2Mbar":{mbar},"oxygen1Mv":71.4,"oxygen2Mv":71.4,"oxygen3Mv":71.4}}}}"#)
}

/// Stage 1: B1 and B2 battery selection (Photolithium, third row): Down, Down, Confirm, Confirm for each bank.
fn battery_wizard() -> Vec<Act> {
    vec![act(6.5, DOWN), act(7.15, DOWN), act(7.8, CONFIRM), act(8.45, CONFIRM), act(9.1, DOWN), act(9.75, DOWN), act(10.4, CONFIRM), act(11.05, CONFIRM)]
}

/// Stage 2: Up opens the menu, Confirm enters Calibration, Down/Confirm select gas Air, three Downs reach Start, Confirm
/// starts the ~9 s test and two more Confirms save it.
fn air_calibration() -> Vec<Act> {
    vec![act(6.5, UP), act(7.2, CONFIRM), act(7.9, DOWN), act(8.6, CONFIRM), act(9.4, DOWN), act(10.1, DOWN), act(10.8, DOWN), act(11.5, CONFIRM), act(12.3, CONFIRM), act(13.1, CONFIRM), act(22.0, CONFIRM)]
}

/// Stage 3: the depth applied at 8 s, "LOW PPO2! On the loop?" answered YES (Up) at 30 s, "Bubble check!" acknowledged at 33 s.
fn nan_dive() -> Vec<Act> {
    vec![act(8.0, &inputs_request(DEPTH_30M.mbar)), act(30.0, UP), act(33.0, DOWN)]
}

/// Stage 5 up to the depth: the aged-calibration notification (Up opens it, Down acknowledges), "LOW PPO2! On the loop?"
/// answered NO (Down), the menu (Confirm), the calibration route of stage 2 again.
fn recalibration() -> Vec<Act> {
    vec![
        act(6.5, UP),
        act(7.5, DOWN),
        act(8.5, DOWN),
        act(9.5, CONFIRM),
        act(11.2, DOWN),
        act(11.9, CONFIRM),
        act(12.7, DOWN),
        act(13.4, DOWN),
        act(14.1, DOWN),
        act(14.8, CONFIRM),
        act(15.6, CONFIRM),
        act(16.4, CONFIRM),
        act(27.0, CONFIRM),
    ]
}

// ---- running -----------------------------------------------------------------------------------------------------------

type Clock<'a> = &'a mut dyn FnMut() -> f64;

/// The benchmark keeps the workload it was measured with (DESIGN.md 16.6): batteries at the 1500 mV that fit the
/// Photolithium type its wizard chooses (a fresh profile's 4100 mV would make the firmware ask for a battery change and
/// stand by), and both decompression fixtures off (the profile is built through the firmware's own routes instead).
fn open(env: &ScenarioEnv<'_>, config: &DiveConfig, profile: Profile) -> Result<Session, String> {
    let session_config = super::recorded_config(SessionConfig {
        idle_fast_forward: config.idle_fast_forward,
        routine_accel: config.routine_accel != RoutineAccelMode::Off,
        routine_accel_shadow: config.routine_accel == RoutineAccelMode::Shadow,
        ..SessionConfig::default()
    });
    Session::new_with(env.options, "", session_config, Some(env.main), env.handset, super::recorded_inputs(profile))
}

pub fn checkpoint(session: &mut Session, name: &str) -> Checkpoint {
    let system = session.system_mut();
    let lcd_sha256 = system.lcd_ppm().map(|ppm| sha256::digest_hex(&ppm)).unwrap_or_default();
    let system = session.system();
    Checkpoint {
        name: name.to_string(),
        virtual_ns: system.time(),
        fingerprint: system.fingerprint(),
        exact: [system.exactness_digest(Which::Main).unwrap_or(0), system.exactness_digest(Which::Handset).unwrap_or(0)],
        instructions: [system.instructions(Which::Main).unwrap_or(0), system.instructions(Which::Handset).unwrap_or(0)],
        lcd_sha256,
    }
}

fn main_word(session: &Session, address: u32) -> u32 {
    session.system().board(Which::Main).and_then(|b| b.peek(address, Width::Word)).unwrap_or(0)
}

/// Retire count and idle-skipped count of the main board.
fn main_counters(session: &Session) -> (u64, u64) {
    session.system().counters(Which::Main).map_or((0, 0), |c| (c.instructions, c.fast_forward.skipped_instructions))
}

/// Runs `session` in slices to `until` virtual seconds, applying the actions of `timeline` between slices. `on_step` is
/// called after every [`STEP_SECONDS`] with the step's measurements.
fn play(session: &mut Session, timeline: &[Act], until: f64, clock: Clock<'_>, mut on_step: impl FnMut(&mut Session, Step), mut on_checkpoint: impl FnMut(&mut Session, f64)) -> Result<f64, String> {
    let until_ns = (until * 1e9).round() as u64;
    let slices_per_step = (STEP_SECONDS / SLICE_SECONDS).round() as u32;
    let mut next = 0usize;
    let mut wall_total = 0.0;
    let mut wall_step = 0.0;
    let mut slices = 0u32;
    let mut step_start = main_counters(session);
    let mut checkpoint_at = CHECKPOINT_SECONDS;
    while session.virtual_ns() < until_ns {
        let now = session.virtual_ns();
        while next < timeline.len() && (timeline[next].at * 1e9).round() as u64 <= now {
            session.action(&timeline[next].request).map_err(|e| format!("action {} at {:.2} s: {e}", timeline[next].request, timeline[next].at))?;
            next += 1;
        }
        let t0 = clock();
        let outcome = session.run_for(SLICE_SECONDS);
        let wall = clock() - t0;
        if let Some(error) = outcome.error {
            return Err(format!("the system stopped at {:.3} s: {error}", outcome.virtual_ns as f64 / 1e9));
        }
        if outcome.standby {
            return Err(format!("the system entered standby at {:.3} s", outcome.virtual_ns as f64 / 1e9));
        }
        wall_total += wall;
        wall_step += wall;
        slices += 1;
        if slices == slices_per_step {
            let counters = main_counters(session);
            let executed = (counters.0 - step_start.0).saturating_sub(counters.1 - step_start.1);
            let burst = executed as f64 / STEP_SECONDS >= BURST_EXECUTED_MIPS * 1e6;
            on_step(session, Step { virtual_seconds: STEP_SECONDS, wall_seconds: wall_step, main_executed: executed, burst });
            step_start = counters;
            wall_step = 0.0;
            slices = 0;
        }
        let seconds = session.virtual_ns() as f64 / 1e9;
        if seconds + 1e-9 >= checkpoint_at {
            on_checkpoint(session, checkpoint_at);
            checkpoint_at += CHECKPOINT_SECONDS;
        }
    }
    Ok(wall_total)
}

/// Runs one profile stage and returns its closed profile.
fn stage(env: &ScenarioEnv<'_>, config: &DiveConfig, name: &str, profile: Profile, timeline: &[Act], seconds: f64, clock: Clock<'_>, stages: &mut Vec<Stage>) -> Result<Profile, String> {
    let mut session = open(env, config, profile)?;
    let wall = play(&mut session, timeline, seconds, clock, |_, _| {}, |_, _| {})?;
    let checkpoint = checkpoint(&mut session, name);
    stages.push(Stage { name: name.to_string(), virtual_seconds: session.virtual_ns() as f64 / 1e9, wall_seconds: wall, checkpoint });
    Ok(session.shutdown())
}

/// Adds `days` to the day-of-month of the BCD date register (the benchmark's clock jump stays inside the month).
fn add_days_bcd(date_register: u32, days: u32) -> Result<u32, String> {
    let day = ((date_register >> 4) & 3) * 10 + (date_register & 15);
    let new_day = day + days;
    if new_day > 28 {
        return Err(format!("the clock jump of {days} days from day {day} would leave the month"));
    }
    Ok((date_register & !0x3F) | ((new_day / 10) << 4) | (new_day % 10))
}

/// Stage 4: the explicit clock fixture on the saved profile.
fn clock_fixture(profile: &Profile, surface_inputs: Option<String>) -> Result<Profile, String> {
    let text = profile.rtc_state.as_deref().ok_or("the NaN-dive stage saved no RTC checkpoint")?;
    let mut state = RtcState::parse(text, "rtc-state.json")?;
    let mut board = state.board(BOARD_MAIN).cloned().ok_or("the saved RTC checkpoint has no main board")?;
    board.checkpoint.date_register = add_days_bcd(board.checkpoint.date_register, CLOCK_JUMP_DAYS)?;
    state.set_board(BOARD_MAIN, board);
    Ok(Profile { rtc_state: Some(state.to_file_text()), inputs: surface_inputs, ..profile.clone() })
}

/// Builds the valid-tissue profile (stages 1-4). The stage reports are appended to `stages`.
pub fn build_profile(env: &ScenarioEnv<'_>, config: &DiveConfig, clock: Clock<'_>, stages: &mut Vec<Stage>) -> Result<Profile, String> {
    let profile = stage(env, config, "battery-wizard", Profile::default(), &battery_wizard(), 12.5, clock, stages)?;
    let profile = stage(env, config, "air-calibration", profile, &air_calibration(), 27.0, clock, stages)?;
    let surface_inputs = profile.inputs.clone();
    let profile = stage(env, config, "nan-dive", profile, &nan_dive(), NAN_DIVE_SECONDS, clock, stages)?;
    clock_fixture(&profile, surface_inputs)
}

/// One dive from the prepared profile.
pub fn dive(env: &ScenarioEnv<'_>, config: &DiveConfig, profile: &Profile, depth: Depth, clock: Clock<'_>) -> Result<DiveRun, String> {
    let mut session = open(env, config, profile.clone())?;
    let mut timeline = recalibration();
    timeline.push(act(DEPTH_AT, &inputs_request(depth.mbar)));
    timeline.push(act(BUBBLE_AT, DOWN));
    let until = BUBBLE_AT + config.dive_seconds;
    let mut checkpoints = Vec::new();
    let mut steps = Vec::new();
    let mut tissue = (0u32, 0u32);
    let label = depth.name.replace(' ', "");
    // The tissue state right before the depth is applied: the benchmark's premise is that the firmware's own start-up path
    // produced finite tissues (a NaN state takes a cheap path in the main board's decompression code).
    let before_depth = DEPTH_AT - 0.5;
    let mut sampled = false;
    {
        let steps_ref = &mut steps;
        let checkpoints_ref = &mut checkpoints;
        let tissue_ref = &mut tissue;
        let sampled_ref = &mut sampled;
        play(
            &mut session,
            &timeline,
            until,
            clock,
            |session, step| {
                steps_ref.push(step);
                if !*sampled_ref && session.virtual_ns() as f64 / 1e9 >= before_depth {
                    *tissue_ref = (main_word(session, TISSUE_WORD), main_word(session, RAW_NDL_WORD));
                    *sampled_ref = true;
                }
            },
            |session, at| {
                let name = format!("dive-{label}-{at:.0}s");
                checkpoints_ref.push(checkpoint(session, &name));
            },
        )?;
    }
    let final_checkpoint = checkpoint(&mut session, &format!("dive-{label}-end"));
    checkpoints.push(final_checkpoint);
    let stats = session.system().routine_accel_stats(Which::Main);
    let (replaced_calls, replaced_instructions, shadow_mismatches) = stats.map_or((0, 0, 0), |s| (s.hits() + s.shadow_checks, s.instructions_replaced(), s.shadow_mismatches));
    let tissues_finite = (tissue.0 >> 23) & 0xFF != 0xFF;
    let application = session.system().main_application();
    let final_png = if config.keep_images { session.lcd_png() } else { Vec::new() };
    Ok(DiveRun {
        depth,
        checkpoints,
        steps,
        replaced_calls,
        replaced_instructions,
        tissues_finite,
        tissue_word: tissue.0,
        raw_ndl: tissue.1,
        shadow_mismatches,
        final_png,
        screen_mode: application.and_then(|a| a.screen_mode),
        main_mode: application.and_then(|a| a.main_mode),
    })
}

/// Runs the whole benchmark: the profile, then one dive per configured depth. `clock` returns wall-clock seconds.
pub fn run(env: &ScenarioEnv<'_>, config: &DiveConfig, clock: Clock<'_>) -> Result<DiveReport, String> {
    if env.main.release.id != crate::firmware::TRITON.id || env.handset.release.id != crate::firmware::TRITON.id {
        return Err(format!("the dive benchmark is written for {} (application RAM addresses, menu routes)", crate::firmware::TRITON.id));
    }
    let mut stages = Vec::new();
    let profile = build_profile(env, config, clock, &mut stages)?;
    let mut dives = Vec::new();
    for depth in &config.depths {
        let run = dive(env, config, &profile, *depth, clock)?;
        if !run.tissues_finite {
            return Err(format!("the {} dive started with NaN tissues ({:#010x}): the profile did not take the firmware's own tissue initialization", depth.name, run.tissue_word));
        }
        dives.push(run);
    }
    Ok(DiveReport { config: config.clone(), stages, dives })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_jump_edits_the_day_digits() {
        assert_eq!(add_days_bcd(0x0020_2101, 5), Ok(0x0020_2106));
        assert_eq!(add_days_bcd(0x0020_2108, 5), Ok(0x0020_2113));
        assert!(add_days_bcd(0x0020_2126, 5).is_err());
    }

    #[test]
    fn timelines_are_ordered() {
        for timeline in [battery_wizard(), air_calibration(), nan_dive(), recalibration()] {
            assert!(timeline.windows(2).all(|w| w[0].at <= w[1].at));
        }
    }

    #[test]
    fn speed_summary_separates_bursts() {
        let run = DiveRun {
            depth: DEPTH_20M,
            checkpoints: Vec::new(),
            steps: vec![
                Step { virtual_seconds: 0.25, wall_seconds: 0.25, main_executed: 25_000_000, burst: true },
                Step { virtual_seconds: 0.25, wall_seconds: 0.25, main_executed: 25_000_000, burst: true },
                Step { virtual_seconds: 0.25, wall_seconds: 0.25, main_executed: 25_000_000, burst: true },
                Step { virtual_seconds: 0.25, wall_seconds: 0.0125, main_executed: 100, burst: false },
            ],
            replaced_calls: 0,
            replaced_instructions: 0,
            tissues_finite: true,
            tissue_word: 0,
            raw_ndl: 0,
            shadow_mismatches: 0,
            final_png: Vec::new(),
            screen_mode: None,
            main_mode: None,
        };
        let (all, burst, quiet) = run.speeds();
        assert_eq!(burst.steps, 3);
        assert_eq!(quiet.steps, 1);
        assert!((burst.factor() - 1.0).abs() < 1e-9);
        assert!((quiet.factor() - 20.0).abs() < 1e-9);
        assert!((all.factor() - 1.0 / 0.7625 * 1.0).abs() < 1e-9);
        assert!((run.worst_burst_factor() - 1.0).abs() < 1e-9);
    }
}
