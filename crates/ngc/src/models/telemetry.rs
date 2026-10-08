// Ported from emulation/models/NGCBoardTelemetry.cs.

//! `NGCBoardTelemetry`: read-only diagnostics of the firmware's PWM outputs, as JSON for the viewer.
//!
//! The model does not connect PWM edges and never writes peripheral state, so the timers' lazy evaluation stays
//! intact. It reads timer and GPIO registers through **side-effect-free peeks** ([`Telemetry::outputs_json`]
//! takes the peek function of the board) and, on the handset, watches the vibrator enable `PB15`
//! (`gpioB:15 -> outputTelemetry@0`) to count enable activations and remember the virtual time of the last one.
//!
//! * main board: three HUD channels, TIM4 `CH1..CH3` on `PD12..PD14` (AF2), `main-hud-1..3`;
//! * handset: `handset-backlight` (TIM2 `CH2` on `PB3`, AF1) and `handset-vibrator` (TIM15 `CH1` on `PB14`, AF14,
//!   gated by the `PB15` enable when `PB15` is an output).
//!
//! A channel is *supported* when its pin is in alternate-function mode with the expected function, the channel
//! is an output with PWM mode 1 or 2, `CR1` has no center-aligned/direction bits (`CR1 & 0x70 == 0`) and ARR is
//! not zero. The duty is the commanded one, `CCR / ARR` clamped to 1 (the modelled period is ARR ticks), inverted
//! for PWM mode 2 and for `CCxP`, and is reported as 0 while the counter or the channel is disabled. A capture-mode
//! `CCR` is never read (a stock read would clear its flag). `BDTR.MOE` is unmodelled, which the details text states.
//!
//! The JSON text is byte-compatible with the C# model: member order, no whitespace, numbers in .NET's "R" format
//! ([`dotnet_round_trip`]), virtual seconds in "F6".
//!
//! # Output activity histories (`activity`, `pwmActivity`)
//!
//! Each output item ends with `"activity"` and `"pwmActivity"` (`null` when not applicable), the bounded histories of
//! the analysis workspace's `NGCBoardTelemetry.ActivityJson` ([`OutputActivity`]):
//!
//! * main HUD 1-3: `activity` = change-only samples of the TIM4 channel command (`sampled-pwm-command`, period 0.05 s);
//! * handset vibrator: `activity` = exact `PB15` enable-level changes (`gpio-enable-command`, no sampling), recorded when the
//!   enable line changes; `pwmActivity` = change-only samples of the gated TIM15 CH1 command every 0.02 s
//!   (`sampled-gated-pwm-command`);
//! * handset backlight: both `null`.
//!
//! The samples are taken by the system at its 100 us quantum boundaries ([`Telemetry::record_hud`],
//! [`Telemetry::record_vibrator`] with the commands read by [`read_hud_commands`] / [`read_vibrator_command`] through
//! side-effect-free peeks): the Renode runner samples with managed threads, which would add clock entries (and chunk splits)
//! to the board; sampling on the 50 ms / 20 ms grid at the quantum boundaries leaves the guest untouched. Histories are
//! cleared by the model's reset (machine reset); a recreated board starts with empty ones.
//!
//! Counting rules (`OutputActivity.Record`): an *initial sample* is the first observation after a reset and counts
//! neither as a transition nor as an activation; every later record is a transition, an activation when the observed state
//! (`level` if given, else `active`) turns on from anything but on, a deactivation when it turns off from anything but off;
//! the queue keeps the newest 32 events, `eventCount` and the sequence numbers keep counting.

use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, Width, TICKS_PER_SECOND};
use std::collections::VecDeque;

pub const SIZE: u32 = 0x100;
/// Input line 0 receives `gpioB:15` on the handset.
pub const VIBRATOR_ENABLE_LINE: u32 = 0;

const TIM2: u32 = 0x4000_0000;
const TIM4: u32 = 0x4000_0800;
const TIM15: u32 = 0x4001_4000;
const GPIOB: u32 = 0x4800_0400;
const GPIOD: u32 = 0x4800_0C00;

/// Events kept per output (`ActivityCapacity`).
pub const ACTIVITY_CAPACITY: usize = 32;
/// Sampling period of the HUD command history in seconds (`HudSamplePeriodSeconds`).
pub const HUD_SAMPLE_PERIOD_SECONDS: f64 = 0.05;
/// Sampling period of the vibrator PWM command history in seconds (`VibratorSamplePeriodSeconds`).
pub const VIBRATOR_SAMPLE_PERIOD_SECONDS: f64 = 0.02;

/// A PWM channel command as read from the timer and GPIO registers (`NGCBoardTelemetry.PwmCommand`).
#[derive(Clone, Debug, PartialEq)]
pub struct PwmCommand {
    /// The channel is driving (`None`: the pin/PWM configuration is unsupported, or no gate information).
    pub active: Option<bool>,
    /// Commanded duty in percent.
    pub duty: Option<f64>,
    pub supported: bool,
    pub output_mode: bool,
    /// `CR1 & 0x71`.
    pub cr1: u32,
    pub ccr: u32,
    pub arr: u32,
    /// The channel's two `CCER` bits (enable, polarity).
    pub ccer: u32,
    /// The channel's `CCMR` byte (mode, selection).
    pub ccmr: u32,
    /// The GPIO mode (`MODER` field) of the output pin.
    pub pin_mode: u32,
    pub alternate_function: u32,
}

impl PwmCommand {
    /// `SameAs`: equality of every field the history compares (exact, like the C# `double` comparison).
    pub fn same_as(&self, other: &PwmCommand) -> bool {
        self == other
    }

    /// The `command` object of an event.
    pub fn json(&self) -> String {
        format!(
            "{{\"timerEnabled\":{},\"channelEnabled\":{},\"polarityInverted\":{},\"outputMode\":{},\"pwmMode\":{},\"ccr\":{},\"arr\":{},\"gpioMode\":{},\"alternateFunction\":{}}}",
            self.cr1 & 1 != 0,
            self.ccer & 1 != 0,
            self.ccer & 2 != 0,
            self.output_mode,
            (self.ccmr >> 4) & 7,
            if self.output_mode { self.ccr.to_string() } else { "null".to_string() },
            self.arr,
            self.pin_mode,
            self.alternate_function
        )
    }
}

/// One history entry (`OutputEvent`).
#[derive(Clone, Debug, PartialEq)]
pub struct OutputEvent {
    pub sequence: u64,
    pub time: f64,
    pub initial: bool,
    pub active: Option<bool>,
    pub level: Option<bool>,
    pub duty: Option<f64>,
    pub command: Option<PwmCommand>,
}

/// The bounded activity history of one output (`OutputActivity`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputActivity {
    events: VecDeque<OutputEvent>,
    total_events: u64,
    transition_count: u64,
    activation_count: u64,
    last_on: Option<f64>,
    last_off: Option<f64>,
    last_observed_on: Option<bool>,
    last_command: Option<PwmCommand>,
}

impl OutputActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Record`: appends an event at `time` (virtual seconds) and updates the counters.
    pub fn record(&mut self, time: f64, active: Option<bool>, duty: Option<f64>, level: Option<bool>, command: Option<PwmCommand>, initial: bool) {
        let observed_on = level.or(active);
        if !initial {
            self.transition_count += 1;
            if observed_on == Some(true) && self.last_observed_on != Some(true) {
                self.activation_count += 1;
                self.last_on = Some(time);
            }
            if observed_on == Some(false) && self.last_observed_on != Some(false) {
                self.last_off = Some(time);
            }
        }
        self.last_observed_on = observed_on;
        self.total_events += 1;
        self.events.push_back(OutputEvent { sequence: self.total_events, time, initial, active, level, duty, command });
        while self.events.len() > ACTIVITY_CAPACITY {
            self.events.pop_front();
        }
    }

    /// `Reset`: forgets everything, including the last sampled command (the next sample is an initial one again).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records a change-only command sample (`SampleHudCommands` / `SampleVibratorCommand`): nothing when the command
    /// equals the previous sample, an initial sample for the first one after a reset.
    pub fn record_command_sample(&mut self, time: f64, command: PwmCommand) {
        if self.last_command.as_ref().is_some_and(|last| last.same_as(&command)) {
            return;
        }
        let initial = self.last_command.is_none();
        self.record(time, command.active, command.duty, None, Some(command.clone()), initial);
        self.last_command = Some(command);
    }

    pub fn events(&self) -> &VecDeque<OutputEvent> {
        &self.events
    }

    /// Events recorded since the reset (not limited by the capacity).
    pub fn total_events(&self) -> u64 {
        self.total_events
    }

    pub fn transition_count(&self) -> u64 {
        self.transition_count
    }

    pub fn activation_count(&self) -> u64 {
        self.activation_count
    }

    pub fn last_on(&self) -> Option<f64> {
        self.last_on
    }

    pub fn last_off(&self) -> Option<f64> {
        self.last_off
    }

    /// `ActivityJson(activity, source, uncertainty)`.
    pub fn json(&self, source: &str, uncertainty: f64) -> String {
        let events: Vec<String> = self
            .events
            .iter()
            .map(|entry| {
                format!(
                    "{{\"sequence\":{},\"virtualTime\":{},\"kind\":{},\"active\":{},\"dutyPercent\":{},\"level\":{},\"command\":{}}}",
                    entry.sequence,
                    number(Some(entry.time)),
                    quote(if entry.initial { "initial-sample" } else { "change" }),
                    boolean(entry.active),
                    number(entry.duty),
                    boolean(entry.level),
                    entry.command.as_ref().map_or_else(|| "null".to_string(), PwmCommand::json)
                )
            })
            .collect();
        format!(
            "{{\"source\":{},\"clock\":\"virtual-time\",\"capacity\":{},\"eventCount\":{},\"transitionCount\":{},\"activationCount\":{},\"lastOnVirtualTime\":{},\"lastOffVirtualTime\":{},\"samplingPeriodSeconds\":{},\"timestampUncertaintySeconds\":{},\"truncated\":{},\"events\":[{}]}}",
            quote(source),
            ACTIVITY_CAPACITY,
            self.total_events,
            self.transition_count,
            self.activation_count,
            number(self.last_on),
            number(self.last_off),
            number(Some(uncertainty)),
            number(Some(uncertainty)),
            self.total_events > self.events.len() as u64,
            events.join(",")
        )
    }
}

pub struct Telemetry {
    name: String,
    handset: bool,
    vibration_level: bool,
    /// Exact `PB15` enable-level changes (handset).
    vibrator_activity: OutputActivity,
    /// Change-only samples of the gated TIM15 CH1 command (handset).
    vibrator_pwm_activity: OutputActivity,
    /// Change-only samples of the three TIM4 channel commands (main).
    hud_activity: [OutputActivity; 3],
}

impl Telemetry {
    /// `outputTelemetry: Miscellaneous.NGCBoardTelemetry @ sysbus 0x61000300 { handset: true|false }`.
    pub fn new(name: impl Into<String>, handset: bool) -> Self {
        Self {
            name: name.into(),
            handset,
            vibration_level: false,
            vibrator_activity: OutputActivity::new(),
            vibrator_pwm_activity: OutputActivity::new(),
            hud_activity: [OutputActivity::new(), OutputActivity::new(), OutputActivity::new()],
        }
    }

    pub fn handset(&self) -> bool {
        self.handset
    }

    /// Rising edges of `PB15` seen so far.
    pub fn vibration_activations(&self) -> u64 {
        self.vibrator_activity.activation_count()
    }

    /// Virtual time (seconds) of the last rising edge of the vibrator enable.
    pub fn last_vibration_seconds(&self) -> Option<f64> {
        self.vibrator_activity.last_on()
    }

    /// The exact `PB15` enable history.
    pub fn vibrator_activity(&self) -> &OutputActivity {
        &self.vibrator_activity
    }

    /// The sampled gated-PWM history of the vibrator.
    pub fn vibrator_pwm_activity(&self) -> &OutputActivity {
        &self.vibrator_pwm_activity
    }

    /// The sampled command history of main HUD channel `channel` (1..=3).
    pub fn hud_activity(&self, channel: usize) -> Option<&OutputActivity> {
        self.hud_activity.get(channel.wrapping_sub(1))
    }

    /// `SampleHudCommands` at virtual time `seconds`: one change-only sample per HUD channel.
    pub fn record_hud(&mut self, commands: [PwmCommand; 3], seconds: f64) {
        for (activity, command) in self.hud_activity.iter_mut().zip(commands) {
            activity.record_command_sample(seconds, command);
        }
    }

    /// `SampleVibratorCommand` at virtual time `seconds`.
    pub fn record_vibrator(&mut self, command: PwmCommand, seconds: f64) {
        self.vibrator_pwm_activity.record_command_sample(seconds, command);
    }

    /// Everything the histories hold, as one string: part of the system's state fingerprint.
    pub fn history_digest_text(&self) -> String {
        let mut text = String::new();
        for (source, uncertainty, activity) in [
            ("gpio-enable-command", 0.0, &self.vibrator_activity),
            ("sampled-gated-pwm-command", VIBRATOR_SAMPLE_PERIOD_SECONDS, &self.vibrator_pwm_activity),
        ] {
            text.push_str(&activity.json(source, uncertainty));
        }
        for activity in &self.hud_activity {
            text.push_str(&activity.json("sampled-pwm-command", HUD_SAMPLE_PERIOD_SECONDS));
        }
        text
    }

    /// The `OutputsJson` property. `peek` reads a 32-bit register by address without side effects (a failed
    /// peek reads as 0, like a register the C# bus read could not provide).
    pub fn outputs_json(&self, peek: &dyn Fn(u32) -> Option<u32>) -> String {
        let read = |address: u32| peek(address).unwrap_or(0);
        if !self.handset {
            let items: Vec<String> = (1..=3usize)
                .map(|channel| {
                    let activity = self.hud_activity[channel - 1].json("sampled-pwm-command", HUD_SAMPLE_PERIOD_SECONDS);
                    pwm(
                        &read,
                        &format!("main-hud-{channel}"),
                        "main",
                        "led",
                        &format!("HUD channel {channel}"),
                        &format!("PD{}", 11 + channel),
                        TIM4,
                        channel as u32,
                        GPIOD,
                        11 + channel as u32,
                        2,
                        Some(true),
                        "",
                        &activity,
                        "null",
                    )
                })
                .collect();
            return format!("[{}]", items.join(","));
        }
        let backlight = pwm(&read, "handset-backlight", "handset", "backlight", "LCD backlight", "PB3 / TIM2 CH2", TIM2, 2, GPIOB, 3, 1, Some(true), "", "null", "null");
        let gate = vibrator_gate(&read);
        let details = format!(
            "; enable activations={}; last enable={}",
            self.vibrator_activity.activation_count(),
            match self.vibrator_activity.last_on() {
                Some(seconds) => format!("{seconds:.6} virtual s"),
                None => "none".to_string(),
            }
        );
        let activity = self.vibrator_activity.json("gpio-enable-command", 0.0);
        let pwm_activity = self.vibrator_pwm_activity.json("sampled-gated-pwm-command", VIBRATOR_SAMPLE_PERIOD_SECONDS);
        let vibrator = pwm(
            &read,
            "handset-vibrator",
            "handset",
            "vibrator",
            "Vibrator",
            "PB15 enable + PB14 / TIM15 CH1",
            TIM15,
            1,
            GPIOB,
            14,
            14,
            gate,
            &details,
            &activity,
            &pwm_activity,
        );
        format!("[{backlight},{vibrator}]")
    }

    /// `OutputsJson` read from a board through its side-effect-free peek.
    pub fn outputs_json_for_board<C: crate::board::CpuCore>(&self, board: &crate::board::Board<C>) -> String {
        self.outputs_json(&|address| board.peek32(address))
    }
}

/// One `Pwm(...)` item.
#[allow(clippy::too_many_arguments)]
fn pwm(
    read: &dyn Fn(u32) -> u32,
    id: &str,
    board: &str,
    kind: &str,
    label: &str,
    pin: &str,
    timer: u32,
    channel: u32,
    gpio: u32,
    gpio_pin: u32,
    alternate_function: u32,
    gate: Option<bool>,
    extra: &str,
    activity_json: &str,
    pwm_activity_json: &str,
) -> String {
    let command = read_pwm(read, timer, channel, gpio, gpio_pin, alternate_function, gate);
    let (cr1, arr, ccr, output_mode, supported) = (command.cr1, command.arr, command.ccr, command.output_mode, command.supported);
    let (active, duty) = (command.active, command.duty);
    let timer_name = if timer == TIM4 {
        "4"
    } else if timer == TIM2 {
        "2"
    } else {
        "15"
    };
    let details = format!(
        "TIM{} CH{}; CEN={}; CCR={}; ARR={}{}; {}{}",
        timer_name,
        channel,
        cr1 & 1,
        if output_mode { ccr.to_string() } else { "not read (capture)".to_string() },
        arr,
        if timer == TIM15 { "; MOE unmodeled; commanded enable, not motor current" } else { "" },
        if supported { "sampled register duty" } else { "pin/PWM configuration not ready or unsupported" },
        extra
    );
    format!(
        "{{\"id\":{},\"board\":{},\"kind\":{},\"label\":{},\"pin\":{},\"active\":{},\"level\":{},\"dutyPercent\":{},\"details\":{},\"activity\":{},\"pwmActivity\":{}}}",
        quote(id),
        quote(board),
        quote(kind),
        quote(label),
        quote(pin),
        boolean(active),
        if kind == "vibrator" { boolean(gate) } else { "null".to_string() },
        number(duty),
        quote(&details),
        activity_json,
        pwm_activity_json
    )
}

/// The gate of the vibrator command: `PB15` as an output (`MODER` field 1) gives its level, otherwise no gate information.
fn vibrator_gate(read: &dyn Fn(u32) -> u32) -> Option<bool> {
    let motor_mode = (read(GPIOB) >> 30) & 3;
    let motor_level = read(GPIOB + 0x14) & (1 << 15) != 0;
    if motor_mode == 1 {
        Some(motor_level)
    } else {
        None
    }
}

/// The three TIM4 channel commands of the main HUD (`SampleHudCommands`), from side-effect-free register reads.
pub fn read_hud_commands(read: &dyn Fn(u32) -> u32) -> [PwmCommand; 3] {
    [1u32, 2, 3].map(|channel| read_pwm(read, TIM4, channel, GPIOD, 11 + channel, 2, Some(true)))
}

/// The gated TIM15 CH1 command of the vibrator (`SampleVibratorCommand`).
pub fn read_vibrator_command(read: &dyn Fn(u32) -> u32) -> PwmCommand {
    read_pwm(read, TIM15, 1, GPIOB, 14, 14, vibrator_gate(read))
}

/// `ReadPwm`: the command of one timer channel, derived from the timer and GPIO registers.
fn read_pwm(read: &dyn Fn(u32) -> u32, timer: u32, channel: u32, gpio: u32, gpio_pin: u32, alternate_function: u32, gate: Option<bool>) -> PwmCommand {
    let cr1 = read(timer);
    let arr = read(timer + 0x2C);
    let ccer = read(timer + 0x20);
    let ccmr = read(timer + if channel <= 2 { 0x18 } else { 0x1C });
    let shift = (channel - 1) % 2 * 8;
    let mode = (ccmr >> (shift + 4)) & 7;
    let output_mode = (ccmr >> shift) & 3 == 0;
    // A stock CCR read in input-capture mode clears its IRQ flag: inspect the selection first and never consume a
    // capture register.
    let ccr = if output_mode { read(timer + 0x34 + (channel - 1) * 4) } else { 0 };
    let pin_mode = (read(gpio) >> (gpio_pin * 2)) & 3;
    let af = (read(gpio + if gpio_pin < 8 { 0x20 } else { 0x24 }) >> (gpio_pin % 8 * 4)) & 15;
    let supported = pin_mode == 2 && af == alternate_function && output_mode && (mode == 6 || mode == 7) && (cr1 & 0x70) == 0 && arr != 0;
    let mut active: Option<bool> = None;
    let mut duty: Option<f64> = None;
    if supported {
        // Match the pinned stock timer's Period = ARR. This is an average modeled high level, not an electrical
        // PWM/brightness model.
        let mut ratio = (f64::from(ccr) / f64::from(arr)).min(1.0);
        if mode == 7 {
            ratio = 1.0 - ratio;
        }
        if ccer & (2u32 << ((channel - 1) * 4)) != 0 {
            ratio = 1.0 - ratio;
        }
        // The pinned stock timer drops tagged BDTR.MOE writes and does not gate PWM with it: report the commanded
        // enable and duty; a zero BDTR read cannot establish a physical motor-off state.
        let running = (cr1 & 1) != 0 && (ccer & (1u32 << ((channel - 1) * 4))) != 0;
        duty = Some(if running { ratio * 100.0 } else { 0.0 });
        active = match gate {
            Some(g) => Some(running && ratio > 0.0 && g),
            None => None,
        };
    }
    PwmCommand {
        active,
        duty,
        supported,
        output_mode,
        cr1: cr1 & 0x71,
        ccr,
        arr,
        ccer: (ccer >> ((channel - 1) * 4)) & 3,
        ccmr: (ccmr >> shift) & 0xFF,
        pin_mode,
        alternate_function: af,
    }
}

/// `NGCBoardTelemetry.Boolean`.
pub fn boolean(value: Option<bool>) -> String {
    match value {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => "null".to_string(),
    }
}

/// `NGCBoardTelemetry.Number`: .NET's round-trip format, `null` for no value.
pub fn number(value: Option<f64>) -> String {
    match value {
        Some(v) => dotnet_round_trip(v),
        None => "null".to_string(),
    }
}

/// `NGCBoardTelemetry.Quote`: `"` and `\` escaped, control characters as `\u00xx`.
pub fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
            out.push(c);
        } else if (c as u32) < 32 {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// `double.ToString("R", CultureInfo.InvariantCulture)` of .NET Core 3.0 and later: the shortest string that
/// round-trips, in fixed notation unless the decimal point position is below -3 or above max(digits, 15)
/// (then `d.dddE+xx` with at least two exponent digits).
pub fn dotnet_round_trip(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    // `{:e}` gives the shortest round-trip digits in scientific form: d.ddde[-]x
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent");
    let exponent: i32 = exponent.parse().expect("exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let scale = exponent + 1; // position of the decimal point relative to the first digit
    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    let max_digits = (digits.len() as i32).max(15);
    if scale > max_digits || scale < -3 {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('E');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.abs()));
    } else if scale <= 0 {
        out.push_str("0.");
        for _ in 0..(-scale) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if (digits.len() as i32) <= scale {
        out.push_str(&digits);
        for _ in 0..(scale - digits.len() as i32) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..scale as usize]);
        out.push('.');
        out.push_str(&digits[scale as usize..]);
    }
    out
}

impl Peripheral for Telemetry {
    fn name(&self) -> &str {
        &self.name
    }

    fn reset(&mut self, _ctx: &mut Ctx<'_>) {
        self.vibration_level = false;
        self.vibrator_activity.reset();
        self.vibrator_pwm_activity.reset();
        for activity in &mut self.hud_activity {
            activity.reset();
        }
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        if line != VIBRATOR_ENABLE_LINE {
            return;
        }
        if level != self.vibration_level {
            // `machine.ClockSource.CurrentValue.Ticks / TicksPerSecond`: the clock-source time as a double.
            let seconds = ctx.now() as f64 / TICKS_PER_SECOND as f64;
            self.vibrator_activity.record(seconds, None, None, Some(level), None, false);
        }
        self.vibration_level = level;
    }

    fn peek(&self, _offset: u32, _width: Width, _view: &emu_core::View<'_>) -> Option<u32> {
        Some(0)
    }

    fn summary(&self, _view: &emu_core::View<'_>) -> String {
        format!(
            "{}: handset={} vibrator enable={} activations={} last={}",
            self.name,
            self.handset,
            self.vibration_level,
            self.vibrator_activity.activation_count(),
            match self.vibrator_activity.last_on() {
                Some(s) => format!("{s:.6}"),
                None => "none".to_string(),
            }
        )
    }

    impl_peripheral_any!();
}
