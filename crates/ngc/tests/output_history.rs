//! The output activity histories of `NGCBoardTelemetry` (`activity`, `pwmActivity`): the counting rules of
//! `OutputActivity.Record`, the 32-event capacity, the `ActivityJson` text, the change-only command samples and the
//! vibrator enable edges. The expected values are derived by hand from the C# model of the Renode-based analysis workspace (not public):
//! the text of `ActivityJson` is a string concatenation, so every member and its order is spelled out below.

use emu_core::testing::Harness;
use emu_core::TICKS_PER_MILLISECOND;
use ngc::models::telemetry::{
    read_hud_commands, read_vibrator_command, OutputActivity, PwmCommand, Telemetry, ACTIVITY_CAPACITY, HUD_SAMPLE_PERIOD_SECONDS, VIBRATOR_ENABLE_LINE,
    VIBRATOR_SAMPLE_PERIOD_SECONDS,
};
use std::collections::HashMap;

fn command(active: Option<bool>, duty: Option<f64>, ccr: u32) -> PwmCommand {
    PwmCommand { active, duty, supported: true, output_mode: true, cr1: 1, ccr, arr: 1999, ccer: 1, ccmr: 0x60, pin_mode: 2, alternate_function: 2 }
}

#[test]
fn constants_follow_the_csharp_model() {
    assert_eq!(ACTIVITY_CAPACITY, 32);
    assert_eq!(HUD_SAMPLE_PERIOD_SECONDS, 0.05);
    assert_eq!(VIBRATOR_SAMPLE_PERIOD_SECONDS, 0.02);
}

#[test]
fn an_initial_sample_is_neither_a_transition_nor_an_activation() {
    let mut activity = OutputActivity::new();
    activity.record(0.05, Some(true), Some(25.0), None, Some(command(Some(true), Some(25.0), 500)), true);
    assert_eq!((activity.total_events(), activity.transition_count(), activity.activation_count()), (1, 0, 0));
    assert_eq!((activity.last_on(), activity.last_off()), (None, None), "an initial sample sets no on/off time, even when it is on");
    // The first change: the observed state was On (from the initial sample), now Off: a transition and a last-off time.
    activity.record(0.10, Some(false), Some(0.0), None, Some(command(Some(false), Some(0.0), 0)), false);
    assert_eq!((activity.total_events(), activity.transition_count(), activity.activation_count()), (2, 1, 0));
    assert_eq!((activity.last_on(), activity.last_off()), (None, Some(0.10)));
    // Back On: an activation.
    activity.record(0.15, Some(true), Some(25.0), None, Some(command(Some(true), Some(25.0), 500)), false);
    assert_eq!((activity.transition_count(), activity.activation_count(), activity.last_on()), (2, 1, Some(0.15)));
    assert_eq!(activity.events().iter().map(|e| (e.sequence, e.initial)).collect::<Vec<_>>(), [(1, true), (2, false), (3, false)]);
}

#[test]
fn the_observed_state_is_the_level_when_given_else_the_active_flag() {
    let mut activity = OutputActivity::new();
    // `level ?? active`: a level of false wins over an active flag of true.
    activity.record(1.0, Some(true), None, Some(false), None, false);
    assert_eq!((activity.activation_count(), activity.last_on(), activity.last_off()), (0, None, Some(1.0)), "observed Off from the unknown state: a last-off time, no activation");
    // A record without any observation (null level and null active) changes neither the activation count nor on/off times.
    activity.record(2.0, None, None, None, None, false);
    assert_eq!((activity.transition_count(), activity.activation_count(), activity.last_on(), activity.last_off()), (2, 0, None, Some(1.0)));
    // From the unknown state to On counts as an activation (`LastObservedOn != true`).
    activity.record(3.0, None, None, Some(true), None, false);
    assert_eq!((activity.activation_count(), activity.last_on()), (1, Some(3.0)));
    // Repeating On is a transition but not an activation.
    activity.record(4.0, None, None, Some(true), None, false);
    assert_eq!((activity.transition_count(), activity.activation_count(), activity.last_on()), (4, 1, Some(3.0)));
}

#[test]
fn the_queue_keeps_the_newest_32_events_and_the_counters_keep_counting() {
    let mut activity = OutputActivity::new();
    for i in 0..40u32 {
        activity.record(f64::from(i), None, None, Some(i % 2 == 0), None, false);
    }
    assert_eq!(activity.events().len(), 32);
    assert_eq!(activity.events().front().map(|e| e.sequence), Some(9));
    assert_eq!(activity.events().back().map(|e| e.sequence), Some(40));
    assert_eq!((activity.total_events(), activity.transition_count(), activity.activation_count()), (40, 40, 20));
    let json = activity.json("gpio-enable-command", 0.0);
    assert!(json.contains("\"eventCount\":40,") && json.contains("\"truncated\":true"), "{json}");
    assert!(json.contains("\"events\":[{\"sequence\":9,"), "{json}");
    // Exactly 32 entries: truncated stays false.
    let mut full = OutputActivity::new();
    for i in 0..32u32 {
        full.record(f64::from(i), None, None, Some(i % 2 == 0), None, false);
    }
    assert!(full.json("x", 0.0).contains("\"truncated\":false"));
    full.record(32.0, None, None, Some(true), None, false);
    assert!(full.json("x", 0.0).contains("\"truncated\":true"));
}

#[test]
fn activity_json_text_matches_the_csharp_concatenation() {
    let mut hud = OutputActivity::new();
    let initial = PwmCommand { active: Some(false), duty: Some(0.0), supported: true, output_mode: true, cr1: 0, ccr: 0, arr: 1999, ccer: 0, ccmr: 0x60, pin_mode: 2, alternate_function: 2 };
    hud.record_command_sample(0.05, initial.clone());
    let lit = PwmCommand { active: Some(true), duty: Some(25.012506253126567), supported: true, output_mode: true, cr1: 1, ccr: 500, arr: 1999, ccer: 1, ccmr: 0x60, pin_mode: 2, alternate_function: 2 };
    hud.record_command_sample(0.15, lit);
    let expected = concat!(
        "{\"source\":\"sampled-pwm-command\",\"clock\":\"virtual-time\",\"capacity\":32,\"eventCount\":2,\"transitionCount\":1,\"activationCount\":1,",
        "\"lastOnVirtualTime\":0.15,\"lastOffVirtualTime\":null,\"samplingPeriodSeconds\":0.05,\"timestampUncertaintySeconds\":0.05,\"truncated\":false,",
        "\"events\":[",
        "{\"sequence\":1,\"virtualTime\":0.05,\"kind\":\"initial-sample\",\"active\":false,\"dutyPercent\":0,\"level\":null,",
        "\"command\":{\"timerEnabled\":false,\"channelEnabled\":false,\"polarityInverted\":false,\"outputMode\":true,\"pwmMode\":6,\"ccr\":0,\"arr\":1999,\"gpioMode\":2,\"alternateFunction\":2}},",
        "{\"sequence\":2,\"virtualTime\":0.15,\"kind\":\"change\",\"active\":true,\"dutyPercent\":25.012506253126567,\"level\":null,",
        "\"command\":{\"timerEnabled\":true,\"channelEnabled\":true,\"polarityInverted\":false,\"outputMode\":true,\"pwmMode\":6,\"ccr\":500,\"arr\":1999,\"gpioMode\":2,\"alternateFunction\":2}}",
        "]}"
    );
    assert_eq!(hud.json("sampled-pwm-command", 0.05), expected);

    // The vibrator enable history: exact level changes, no command, a zero sampling period.
    let mut enable = OutputActivity::new();
    enable.record(1.371401, None, None, Some(true), None, false);
    enable.record(1.421402, None, None, Some(false), None, false);
    let expected = concat!(
        "{\"source\":\"gpio-enable-command\",\"clock\":\"virtual-time\",\"capacity\":32,\"eventCount\":2,\"transitionCount\":2,\"activationCount\":1,",
        "\"lastOnVirtualTime\":1.371401,\"lastOffVirtualTime\":1.421402,\"samplingPeriodSeconds\":0,\"timestampUncertaintySeconds\":0,\"truncated\":false,",
        "\"events\":[",
        "{\"sequence\":1,\"virtualTime\":1.371401,\"kind\":\"change\",\"active\":null,\"dutyPercent\":null,\"level\":true,\"command\":null},",
        "{\"sequence\":2,\"virtualTime\":1.421402,\"kind\":\"change\",\"active\":null,\"dutyPercent\":null,\"level\":false,\"command\":null}",
        "]}"
    );
    assert_eq!(enable.json("gpio-enable-command", 0.0), expected);
}

#[test]
fn a_capture_mode_command_has_a_null_ccr() {
    let capture = PwmCommand { active: None, duty: None, supported: false, output_mode: false, cr1: 1, ccr: 0, arr: 100, ccer: 0, ccmr: 0x61, pin_mode: 2, alternate_function: 14 };
    assert_eq!(
        capture.json(),
        "{\"timerEnabled\":true,\"channelEnabled\":false,\"polarityInverted\":false,\"outputMode\":false,\"pwmMode\":6,\"ccr\":null,\"arr\":100,\"gpioMode\":2,\"alternateFunction\":14}"
    );
}

#[test]
fn command_samples_are_change_only_and_the_first_one_is_initial() {
    let mut activity = OutputActivity::new();
    let a = command(Some(true), Some(25.0), 500);
    activity.record_command_sample(0.05, a.clone());
    activity.record_command_sample(0.10, a.clone());
    activity.record_command_sample(0.15, a.clone());
    assert_eq!(activity.total_events(), 1, "an unchanged command adds nothing");
    let b = command(Some(true), Some(25.0), 501);
    activity.record_command_sample(0.20, b);
    assert_eq!(activity.total_events(), 2, "a different CCR is a change even though active and duty can look alike");
    assert!(activity.events().front().is_some_and(|e| e.initial) && activity.events().back().is_some_and(|e| !e.initial));
    // A reset forgets the last command: the next sample is an initial one again.
    activity.reset();
    assert_eq!((activity.total_events(), activity.events().len()), (0, 0));
    activity.record_command_sample(0.25, a);
    assert!(activity.events().front().is_some_and(|e| e.initial && e.sequence == 1));
}

#[test]
fn hud_samples_cover_the_three_channels_independently() {
    let mut telemetry = Telemetry::new("outputTelemetry", false);
    let base = |ccr: u32| command(Some(ccr > 0), Some(f64::from(ccr)), ccr);
    telemetry.record_hud([base(0), base(10), base(20)], 0.05);
    telemetry.record_hud([base(0), base(10), base(30)], 0.10);
    let counts: Vec<u64> = (1..=3).map(|n| telemetry.hud_activity(n).unwrap().total_events()).collect();
    assert_eq!(counts, [1, 1, 2], "only channel 3 changed");
    assert!(telemetry.hud_activity(0).is_none() && telemetry.hud_activity(4).is_none());
    // The JSON of the item carries the channel's own history, and the other kinds none.
    let json = telemetry.outputs_json(&|_| Some(0));
    assert!(json.matches("\"source\":\"sampled-pwm-command\"").count() == 3 && json.matches("\"pwmActivity\":null").count() == 3, "{json}");
    assert!(json.contains("\"eventCount\":2,"), "{json}");
}

/// Register values of a configured main board: GPIOD PD12..PD14 in AF2, TIM4 CH1..CH3 in PWM mode 1.
fn main_registers(ccr: [u32; 3], cr1: u32) -> HashMap<u32, u32> {
    let mut regs = HashMap::new();
    regs.insert(0x4800_0C00, (2 << 24) | (2 << 26) | (2 << 28));
    regs.insert(0x4800_0C24, 0x222 << 16);
    regs.insert(0x4000_0800, cr1);
    regs.insert(0x4000_082C, 1999);
    regs.insert(0x4000_0818, (6 << 4) | (6 << 12));
    regs.insert(0x4000_081C, 6 << 4);
    regs.insert(0x4000_0820, 0x111);
    for (channel, value) in ccr.iter().enumerate() {
        regs.insert(0x4000_0834 + 4 * channel as u32, *value);
    }
    regs
}

#[test]
fn hud_commands_are_read_from_the_registers_like_readpwm() {
    let regs = main_registers([500, 1000, 0], 1);
    let read = |address: u32| regs.get(&address).copied().unwrap_or(0);
    let commands = read_hud_commands(&read);
    assert_eq!(commands[0].ccr, 500);
    assert_eq!((commands[0].active, commands[2].active), (Some(true), Some(false)), "CCR 0 is a supported but inactive channel");
    assert!(commands.iter().all(|c| c.supported && c.output_mode && c.pin_mode == 2 && c.alternate_function == 2 && c.arr == 1999 && c.cr1 == 1));
    assert_eq!(commands[0].duty, Some(500.0 / 1999.0 * 100.0));
    // The command stored in the history keeps only what `SameAs` compares: a stopped counter is another command.
    let stopped = main_registers([500, 1000, 0], 0);
    let read_stopped = |address: u32| stopped.get(&address).copied().unwrap_or(0);
    let off = read_hud_commands(&read_stopped);
    assert!(!commands[0].same_as(&off[0]) && off[0].active == Some(false) && off[0].duty == Some(0.0));
    // Unconfigured registers give an unsupported command with unknown drive.
    let none = read_hud_commands(&|_| 0);
    assert!(none.iter().all(|c| !c.supported && c.active.is_none() && c.duty.is_none()));
}

#[test]
fn the_vibrator_command_carries_the_pb15_gate() {
    let mut regs: HashMap<u32, u32> = HashMap::new();
    regs.insert(0x4800_0400, (2 << 28) | (1 << 30)); // PB14 AF, PB15 output
    regs.insert(0x4800_0424, 14 << 24);
    regs.insert(0x4001_4000, 1);
    regs.insert(0x4001_402C, 100);
    regs.insert(0x4001_4018, 6 << 4);
    regs.insert(0x4001_4020, 1);
    regs.insert(0x4001_4034, 51);
    let with_gate = |level: bool| {
        let mut regs = regs.clone();
        regs.insert(0x4800_0414, u32::from(level) << 15);
        let read = move |address: u32| regs.get(&address).copied().unwrap_or(0);
        read_vibrator_command(&read)
    };
    let (high, low) = (with_gate(true), with_gate(false));
    assert_eq!((high.active, high.duty), (Some(true), Some(51.0)));
    assert_eq!((low.active, low.duty), (Some(false), Some(51.0)), "PB15 low gates the command off, the duty is still the commanded one");
    // PB15 not an output: no gate information.
    regs.insert(0x4800_0400, 2 << 28);
    let read = |address: u32| regs.get(&address).copied().unwrap_or(0);
    let ungated = read_vibrator_command(&read);
    assert_eq!((ungated.active, ungated.duty), (None, Some(51.0)));
}

#[test]
fn vibrator_enable_edges_are_recorded_with_the_clock_time_and_cleared_by_reset() {
    let mut h = Harness::new();
    let id = h.add_mapped(0x6100_0300, 0x100, Telemetry::new("outputTelemetry", true));
    h.advance_to(1500 * TICKS_PER_MILLISECOND);
    h.set_input(id, VIBRATOR_ENABLE_LINE, true);
    h.set_input(id, VIBRATOR_ENABLE_LINE, true);
    h.advance_to(2500 * TICKS_PER_MILLISECOND + 123_456);
    h.set_input(id, VIBRATOR_ENABLE_LINE, false);
    let activity = h.get::<Telemetry>(id).vibrator_activity().clone();
    assert_eq!(activity.events().iter().map(|e| (e.sequence, e.time, e.level)).collect::<Vec<_>>(), [(1, 1.5, Some(true)), (2, 2.500123456, Some(false))]);
    assert_eq!((activity.activation_count(), activity.transition_count(), activity.last_on(), activity.last_off()), (1, 2, Some(1.5), Some(2.500123456)));
    assert!(activity.events().iter().all(|e| !e.initial && e.command.is_none() && e.active.is_none()));
    // A level that does not change is not recorded; neither is another input line.
    h.set_input(id, VIBRATOR_ENABLE_LINE, false);
    h.set_input(id, 1, true);
    assert_eq!(h.get::<Telemetry>(id).vibrator_activity().total_events(), 2);
    // The PWM history has its own queue and is cleared with the rest by the model's reset.
    h.get_mut::<Telemetry>(id).record_vibrator(command(Some(true), Some(51.0), 51), 2.6);
    assert_eq!(h.get::<Telemetry>(id).vibrator_pwm_activity().total_events(), 1);
    h.core_mut().reset_all();
    let t = h.get::<Telemetry>(id);
    assert_eq!((t.vibrator_activity().total_events(), t.vibrator_pwm_activity().total_events(), t.vibration_activations()), (0, 0, 0));
}

#[test]
fn the_history_digest_text_covers_every_history() {
    let mut a = Telemetry::new("outputTelemetry", false);
    let b = Telemetry::new("outputTelemetry", false);
    assert_eq!(a.history_digest_text(), b.history_digest_text());
    a.record_hud([command(Some(true), Some(1.0), 1), command(None, None, 0), command(None, None, 0)], 0.05);
    assert_ne!(a.history_digest_text(), b.history_digest_text());
}
