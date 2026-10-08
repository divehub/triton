//! `NGCBoardTelemetry`: the `OutputsJson` text against hand-derived C# output, .NET's "R"/"F6" number formats,
//! and the vibrator-enable activity counters.

use emu_core::testing::Harness;
use emu_core::TICKS_PER_MILLISECOND;
use ngc::models::telemetry::{boolean, dotnet_round_trip, number, quote, Telemetry, VIBRATOR_ENABLE_LINE};
use std::collections::HashMap;

/// An empty main HUD history, as `ActivityJson` renders it (`samplingPeriodSeconds` 0.05 in "R" format).
const EMPTY_HUD: &str = "{\"source\":\"sampled-pwm-command\",\"clock\":\"virtual-time\",\"capacity\":32,\"eventCount\":0,\"transitionCount\":0,\
    \"activationCount\":0,\"lastOnVirtualTime\":null,\"lastOffVirtualTime\":null,\"samplingPeriodSeconds\":0.05,\
    \"timestampUncertaintySeconds\":0.05,\"truncated\":false,\"events\":[]}";

fn peek_of(map: HashMap<u32, u32>) -> impl Fn(u32) -> Option<u32> {
    move |address| Some(map.get(&address).copied().unwrap_or(0))
}

#[test]
fn main_board_unconfigured_json() {
    let t = Telemetry::new("outputTelemetry", false);
    let json = t.outputs_json(&peek_of(HashMap::new()));
    let item = |n: u32, pin: &str| {
        format!(
            "{{\"id\":\"main-hud-{n}\",\"board\":\"main\",\"kind\":\"led\",\"label\":\"HUD channel {n}\",\"pin\":\"{pin}\",\
             \"active\":null,\"level\":null,\"dutyPercent\":null,\"details\":\"TIM4 CH{n}; CEN=0; CCR=0; ARR=0; \
             pin/PWM configuration not ready or unsupported\",\"activity\":{EMPTY_HUD},\"pwmActivity\":null}}"
        )
    };
    assert_eq!(json, format!("[{},{},{}]", item(1, "PD12"), item(2, "PD13"), item(3, "PD14")));
}

#[test]
fn handset_unconfigured_json_and_activity_text() {
    let t = Telemetry::new("outputTelemetry", true);
    let json = t.outputs_json(&peek_of(HashMap::new()));
    assert_eq!(
        json,
        "[{\"id\":\"handset-backlight\",\"board\":\"handset\",\"kind\":\"backlight\",\"label\":\"LCD backlight\",\"pin\":\"PB3 / TIM2 CH2\",\
         \"active\":null,\"level\":null,\"dutyPercent\":null,\"details\":\"TIM2 CH2; CEN=0; CCR=0; ARR=0; pin/PWM configuration not ready or unsupported\",\
         \"activity\":null,\"pwmActivity\":null},\
         {\"id\":\"handset-vibrator\",\"board\":\"handset\",\"kind\":\"vibrator\",\"label\":\"Vibrator\",\"pin\":\"PB15 enable + PB14 / TIM15 CH1\",\
         \"active\":null,\"level\":null,\"dutyPercent\":null,\"details\":\"TIM15 CH1; CEN=0; CCR=0; ARR=0; MOE unmodeled; commanded enable, not motor current; \
         pin/PWM configuration not ready or unsupported; enable activations=0; last enable=none\",\
         \"activity\":{\"source\":\"gpio-enable-command\",\"clock\":\"virtual-time\",\"capacity\":32,\"eventCount\":0,\"transitionCount\":0,\"activationCount\":0,\
         \"lastOnVirtualTime\":null,\"lastOffVirtualTime\":null,\"samplingPeriodSeconds\":0,\"timestampUncertaintySeconds\":0,\"truncated\":false,\"events\":[]},\
         \"pwmActivity\":{\"source\":\"sampled-gated-pwm-command\",\"clock\":\"virtual-time\",\"capacity\":32,\"eventCount\":0,\"transitionCount\":0,\"activationCount\":0,\
         \"lastOnVirtualTime\":null,\"lastOffVirtualTime\":null,\"samplingPeriodSeconds\":0.02,\"timestampUncertaintySeconds\":0.02,\"truncated\":false,\"events\":[]}}]"
    );
}

#[test]
fn configured_handset_channels_report_duty_and_the_enable_gate() {
    let t = Telemetry::new("outputTelemetry", true);
    let mut regs: HashMap<u32, u32> = HashMap::new();
    // GPIOB: PB3 and PB14 in alternate-function mode, PB15 an output; AFRL pin 3 = AF1, AFRH pin 14 (nibble 6) = AF14.
    regs.insert(0x4800_0400, (2 << 6) | (2 << 28) | (1 << 30));
    regs.insert(0x4800_0420, 1 << 12);
    regs.insert(0x4800_0424, 14 << 24);
    regs.insert(0x4800_0414, 1 << 15); // PB15 high
    // TIM2: CEN, ARR 100, CH2 PWM mode 1, enabled, CCR2 = 25.
    regs.insert(0x4000_0000, 1);
    regs.insert(0x4000_002C, 100);
    regs.insert(0x4000_0018, 6 << 12);
    regs.insert(0x4000_0020, 1 << 4);
    regs.insert(0x4000_0038, 25);
    // TIM15: CEN, ARR 100, CH1 PWM mode 2 (inverted), enabled, CCR1 = 30.
    regs.insert(0x4001_4000, 1);
    regs.insert(0x4001_402C, 100);
    regs.insert(0x4001_4018, 7 << 4);
    regs.insert(0x4001_4020, 1);
    regs.insert(0x4001_4034, 30);
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains("\"id\":\"handset-backlight\""));
    assert!(json.contains("\"active\":true,\"level\":null,\"dutyPercent\":25,\"details\":\"TIM2 CH2; CEN=1; CCR=25; ARR=100; sampled register duty\""), "{json}");
    // PWM mode 2: 1 - 0.3 = 0.7 (as an IEEE double), times 100.
    let duty = dotnet_round_trip((1.0 - 30.0f64 / 100.0) * 100.0);
    assert!(
        json.contains(&format!("\"active\":true,\"level\":true,\"dutyPercent\":{duty},\"details\":\"TIM15 CH1; CEN=1; CCR=30; ARR=100; MOE unmodeled")),
        "{json}"
    );
    // PB15 low: the gate is open-circuit, the channel is not active but the duty is still reported.
    regs.insert(0x4800_0414, 0);
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains(&format!("\"active\":false,\"level\":false,\"dutyPercent\":{duty}")), "{json}");
    // PB15 not an output: no gate information.
    regs.insert(0x4800_0400, (2 << 6) | (2 << 28));
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains(&format!("\"active\":null,\"level\":null,\"dutyPercent\":{duty}")), "{json}");
    // CCER polarity inverts the duty; a disabled counter reports 0.
    regs.insert(0x4000_0020, (1 << 4) | (2 << 4));
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains("\"dutyPercent\":75,\"details\":\"TIM2 CH2"), "{json}");
    regs.insert(0x4000_0000, 0);
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains("\"active\":false,\"level\":null,\"dutyPercent\":0,\"details\":\"TIM2 CH2; CEN=0"), "{json}");
}

#[test]
fn capture_mode_channels_are_not_read_and_center_aligned_timers_are_unsupported() {
    let t = Telemetry::new("outputTelemetry", true);
    let mut regs: HashMap<u32, u32> = HashMap::new();
    regs.insert(0x4800_0400, 2 << 6);
    regs.insert(0x4800_0420, 1 << 12);
    regs.insert(0x4000_0000, 1);
    regs.insert(0x4000_002C, 100);
    regs.insert(0x4000_0018, (6 << 12) | (1 << 8)); // CC2S = input
    regs.insert(0x4000_0038, 25);
    let json = t.outputs_json(&peek_of(regs.clone()));
    assert!(json.contains("TIM2 CH2; CEN=1; CCR=not read (capture); ARR=100; pin/PWM configuration not ready or unsupported"), "{json}");
    regs.insert(0x4000_0018, 6 << 12);
    regs.insert(0x4000_0000, 1 | (1 << 5)); // center aligned
    let json = t.outputs_json(&peek_of(regs));
    assert!(json.contains("CCR=25; ARR=100; pin/PWM configuration not ready or unsupported"), "{json}");
}

#[test]
fn vibrator_enable_activations_and_last_time() {
    let mut h = Harness::new();
    let id = h.add_mapped(0x6100_0300, 0x100, Telemetry::new("outputTelemetry", true));
    h.advance_to(1500 * TICKS_PER_MILLISECOND);
    h.set_input(id, VIBRATOR_ENABLE_LINE, true);
    h.set_input(id, VIBRATOR_ENABLE_LINE, true);
    assert_eq!(h.get::<Telemetry>(id).vibration_activations(), 1, "only rising edges count");
    h.advance_to(2500 * TICKS_PER_MILLISECOND + 123_456);
    h.set_input(id, VIBRATOR_ENABLE_LINE, false);
    h.advance_to(3 * 1_000_000_000 + 999_999_999);
    h.set_input(id, VIBRATOR_ENABLE_LINE, true);
    h.set_input(id, 1, true); // other inputs are ignored
    let t = h.get::<Telemetry>(id);
    assert_eq!(t.vibration_activations(), 2);
    let json = t.outputs_json(&peek_of(HashMap::new()));
    assert!(json.contains("enable activations=2; last enable=4.000000 virtual s"), "F6 rounds 3.999999999 up: {json}");
    h.core_mut().reset_all();
    let json = h.get::<Telemetry>(id).outputs_json(&peek_of(HashMap::new()));
    assert!(json.contains("enable activations=0; last enable=none"));
    // The register window reads zero and ignores writes.
    h.write32(0x6100_0300, 5);
    assert_eq!(h.read32(0x6100_0300), 0);
}

#[test]
fn string_helpers_match_the_csharp_ones() {
    assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    assert_eq!(quote("tab\t"), "\"tab\\u0009\"");
    assert_eq!(boolean(Some(true)), "true");
    assert_eq!(boolean(None), "null");
    assert_eq!(number(None), "null");
    assert_eq!(number(Some(12.5)), "12.5");
}

#[test]
fn dotnet_round_trip_formatting() {
    for (value, text) in [
        (0.0, "0"),
        (100.0, "100"),
        (50.0, "50"),
        (0.5, "0.5"),
        (26.0, "26"),
        (0.1 + 0.2, "0.30000000000000004"),
        (1.0 / 3.0, "0.3333333333333333"),
        (0.0001, "0.0001"),
        (0.00001, "1E-05"),
        (0.000012345, "1.2345E-05"),
        (123456789012345.0, "123456789012345"),
        (1e15, "1E+15"),
        (1.5e16, "1.5E+16"),
        (-2.5, "-2.5"),
        (1e-7, "1E-07"),
        (2.3283064365386963e-8, "2.3283064365386963E-08"),
        (1234567.125, "1234567.125"),
    ] {
        assert_eq!(dotnet_round_trip(value), text, "{value:?}");
    }
    assert_eq!(dotnet_round_trip(f64::NAN), "NaN");
    assert_eq!(dotnet_round_trip(f64::INFINITY), "Infinity");
}
