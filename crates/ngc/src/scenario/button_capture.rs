//! `button-capture`: synthetic PE3/PE5 low pulses go through the real TIM3 input capture, IRQ 29 and the original
//! callback at `0x08005B18` of the handset firmware (handset only, like `emulation/main-boot/button-probe.resc`).
//!
//! The callback accepts a low width of 150..700 timer counts (122.88..573.44 ms at the firmware's 1 220.703125 Hz
//! capture clock): a 204.8 ms pulse (250 counts) sets the pending flag of its input (`0x200005BD` for PE3,
//! `0x200005BC` for PE5), a 81.92 ms pulse (100 counts) is rejected. Compared with
//! `emulation/main-boot/button-capture-result.json`: the differences of CCR1/CCR2 (PE3) and CCR3/CCR4 (PE5) and the
//! pending flags before and after.

use super::*;
use crate::session::SessionConfig;
use crate::system::{Input, Mode};

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/main-boot/button-capture-result.json";
const TIM3: u32 = 0x4000_0400;
const PENDING_PE3: u32 = 0x2000_05BD;
const PENDING_PE5: u32 = 0x2000_05BC;

struct Pulse {
    pin: &'static str,
    duration_us: u32,
    pending_address: u32,
    first_ccr: u32,
    second_ccr: u32,
    pending_before: u32,
    pending_after: u32,
}

/// Starts a pulse, runs until the pin is released and the capture interrupt had time to run, and reads the capture
/// registers (side-effect free) and the pending flag (the maximum seen over the 3 ms after the rising edge, because
/// the UI task consumes the flag soon after).
fn pulse(rig: &mut Rig, pin: &'static str, mask: u32, duration_us: u32) -> Result<Pulse, String> {
    let (pending_address, first, second) = if mask == 1 { (PENDING_PE3, TIM3 + 0x34, TIM3 + 0x38) } else { (PENDING_PE5, TIM3 + 0x3C, TIM3 + 0x40) };
    let pending_before = rig.u8(Which::Handset, pending_address);
    rig.session.system_mut().apply_input(&Input::Pulse { mask, duration_us })?;
    // The pulse starts at the next 100 us stimulus tick; run until the gesture has finished.
    let mut seen_low = false;
    let mut released_at = None;
    let limit = u64::from(duration_us) / 100 + 400;
    for _ in 0..limit {
        rig.session.system_mut().step_quantum();
        let summary = rig.session.system().button_summary();
        let low = summary_field(&summary, if mask == 1 { "PE3" } else { "PE5" }) == Some("False");
        seen_low |= low;
        if seen_low && !low {
            released_at = Some(());
            break;
        }
    }
    if released_at.is_none() {
        return Err(format!("the {pin} pulse did not complete"));
    }
    let mut pending_after = rig.u8(Which::Handset, pending_address);
    for _ in 0..30 {
        rig.session.system_mut().step_quantum();
        pending_after = pending_after.max(rig.u8(Which::Handset, pending_address));
    }
    Ok(Pulse { pin, duration_us, pending_address, first_ccr: rig.u32(Which::Handset, first), second_ccr: rig.u32(Which::Handset, second), pending_before, pending_after })
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("button-capture");
    let config = SessionConfig { mode: Mode::HandsetOnly, ..SessionConfig::default() };
    let mut rig = Rig::new(env, config, Profile::default())?;
    let state = rig.advance(2.0)?;
    rec.step("2.0 s: handset idle, TIM3 capture configured", &state, Json::object());
    let summary = state.get("buttonSummary").and_then(Json::as_str).unwrap_or("").to_string();
    rec.check("the stimulus model is ready (TIM3 capture configured by the firmware)", summary_field(&summary, "ready") == Some("True"), summary.clone());
    rec.check("TIM3 prescaler is 65535 (1220.703125 Hz capture clock)", rig.u32(Which::Handset, TIM3 + 0x28) == 65535, u64::from(rig.u32(Which::Handset, TIM3 + 0x28)));

    let expected = [
        // (pin, mask, width, renode difference, renode pending before/after)
        ("PE3", 1u32, 204_800u32, 250u32, (0u32, 1u32), "accepted"),
        ("PE5", 2, 81_920, 100, (0, 0), "rejected by original width filter"),
        ("PE5", 2, 204_800, 250, (0, 1), "accepted"),
    ];
    let mut results = Vec::new();
    for (index, (pin, mask, width, renode_difference, renode_pending, renode_result)) in expected.into_iter().enumerate() {
        let p = pulse(&mut rig, pin, mask, width)?;
        let difference = p.second_ccr.wrapping_sub(p.first_ccr) & 0xFFFF;
        let accepted = p.pending_after != 0 && p.pending_before == 0;
        results.push(
            Json::object()
                .with("pin", p.pin)
                .with("lowDurationMicroseconds", u64::from(p.duration_us))
                .with("firstCCR", format!("0x{:08x}", p.first_ccr))
                .with("secondCCR", format!("0x{:08x}", p.second_ccr))
                .with("differenceCounts", u64::from(difference))
                .with("pendingAddress", format!("0x{:08x}", p.pending_address))
                .with("pendingBefore", u64::from(p.pending_before))
                .with("pendingAfter", u64::from(p.pending_after))
                .with("result", if accepted { "accepted" } else { "rejected" }),
        );
        rec.compare(
            &format!("pulse {} ({} us): capture difference in counts", index + 1, p.duration_us),
            u64::from(difference),
            u64::from(renode_difference),
            SOURCE,
            true,
            "exact 250 counts for 204.8 ms and 100 counts for 81.92 ms in both models",
        );
        rec.compare(
            &format!("pulse {} ({} {} us): pending flag {:#x} before/after", index + 1, p.pin, p.duration_us, p.pending_address),
            Json::from_items([u64::from(p.pending_before), u64::from(u32::from(accepted))]),
            Json::from_items([u64::from(renode_pending.0), u64::from(renode_pending.1)]),
            SOURCE,
            true,
            renode_result,
        );
        rec.compare(&format!("pulse {} first CCR (absolute, informational)", index + 1), u64::from(p.first_ccr), u64::from(match index { 0 => 0x211u32, 1 => 0x30D, _ => 0x373 }), SOURCE, false, "depends on the free-running TIM3 phase when the pulse starts; only differences are meaningful");
        rec.check(&format!("pulse {} ({} {} us) is {}", index + 1, p.pin, p.duration_us, renode_result), accepted == (renode_pending.1 == 1), accepted);
    }
    rec.extra("tests", Json::Array(results));
    let summary = rig.session.system().button_summary();
    rec.check("three gestures started and released", summary_number(&summary, "pulses") == Some(3) && summary_number(&summary, "releases") == Some(3), summary);
    rec.check("handset without faults", rig.u32(Which::Handset, 0xE000_ED28) == 0 && rig.u32(Which::Handset, 0xE000_ED2C) == 0, rig.faults(Which::Handset));
    let png = rig.png();
    rec.image("after-pulses.png", png);
    rec.limitation("Synthetic pulse generator: physical electrical filters, switch bounce, timing variance and complete device behaviour are not reproduced.");
    rec.limitation("The first/second CCR values depend on the free-running TIM3 phase and are shown for information only.");
    Ok(rec.finish(env))
}
