//! `can-loss`: the CAN link's fault injection: `can {connected}` disconnects the link, `can {dropId}` discards one standard
//! identifier (`emulation/runtime/system-validation/{selective-loss-state,disconnected-state,validated-final-state}.json`,
//! `validated-can.tsv`, `emulation/system-model.md`).
//!
//! Renode's evidence: with `dropId = 558` (0x22E) the link counted one frame as `dropped` (main's periodic 0x22E), with
//! `connected = False` the next frame (0x010) was dropped, restoring both left `transmitted = scheduled + dropped`. This
//! scenario repeats that on a configured profile and adds the loss of a handset command (the B1 battery commit `0x082`).

use super::*;
use crate::system::Input;

// Expected values are embedded constants, recorded by the Renode runner in the analysis workspace (2026-10-07).
const SOURCE: &str = "recorded by the Renode runner in the analysis workspace, emulation/runtime/system-validation/validated-final-state.json";

/// `(scheduled, dropped)` and the per-identifier counts of the frames stamped in `[from, to)` seconds.
fn window(trace: &str, from: f64, to: f64) -> (u32, u32, Vec<(u32, bool)>) {
    let (mut scheduled, mut dropped, mut frames) = (0, 0, Vec::new());
    for line in trace.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 5 {
            continue;
        }
        let Some(stamp) = trace_stamp_seconds(fields[0]) else { continue };
        if stamp < from || stamp >= to {
            continue;
        }
        let id = u32::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap_or(0);
        let is_dropped = fields[4] == "dropped";
        if is_dropped {
            dropped += 1;
        } else {
            scheduled += 1;
        }
        frames.push((id, is_dropped));
    }
    (scheduled, dropped, frames)
}

fn summary(rig: &Rig) -> String {
    rig.state().get("canSummary").and_then(Json::as_str).unwrap_or("").to_string()
}

fn press(rig: &mut Rig, action: &str) -> Result<Json, String> {
    rig.act(&format!("{{\"action\":\"{action}\"}}"))?;
    rig.advance(0.65)
}

fn time(rig: &Rig) -> f64 {
    rig.state().get("virtualTime").and_then(Json::as_f64).unwrap_or(0.0)
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("can-loss");

    // ---- a configured profile publishes the periodic surface frames ------------------------------------------------------
    let mut rig = Rig::new(env, SessionConfig::default(), Profile::default())?;
    rig.advance(6.5)?;
    for action in ["down", "confirm", "confirm", "down", "confirm", "confirm"] {
        press(&mut rig, action)?;
    }
    rig.act("{\"action\":\"reset\"}")?;
    rig.advance(8.0)?;
    let start = time(&rig);
    rig.advance(2.0)?;
    let (scheduled, dropped, frames) = window(&rig.can_trace(), start, start + 2.0);
    let mut counts: Vec<(u32, u32)> = Vec::new();
    for (id, _) in &frames {
        match counts.iter_mut().find(|(i, _)| i == id) {
            Some(entry) => entry.1 += 1,
            None => counts.push((*id, 1)),
        }
    }
    counts.sort();
    rec.step("configured profile, steady surface mode (10 s)", &rig.state(), Json::object().with("frameCountsLast2s", Json::from_items(counts.iter().map(|(id, n)| Json::from(format!("0x{id:03X} x{n}"))))));
    rec.check("no frame is dropped on a healthy link", dropped == 0 && scheduled > 0, Json::from_items([scheduled, dropped]));
    let main_frames: Vec<u32> = rig.main_frames().iter().map(|(id, _)| *id).collect();
    // Renode dropped main's periodic 0x22E; use it when this run publishes it, else the most frequent main identifier.
    let periodic = if main_frames.contains(&0x22E) {
        0x22E
    } else {
        let mut best = (0u32, 0usize);
        for id in &main_frames {
            let n = main_frames.iter().filter(|i| *i == id).count();
            if n > best.1 || (n == best.1 && *id < best.0) {
                best = (*id, n);
            }
        }
        best.0
    };
    rec.compare("main's periodic frame 0x22E exists in the surface traffic", main_frames.contains(&0x22E), true, SOURCE, false, "the Renode run dropped 0x22E (payload 4B0412004B0412)");

    // ---- selective loss ----------------------------------------------------------------------------------------------------
    let invalid = rig.act("{\"action\":\"can\",\"dropId\":2048}").err();
    rec.compare("an out-of-range dropId is refused", invalid, Some("dropId must be -1 or a standard CAN ID (0..2047)".to_string()), "emulation/run_emulator.py Emulator.action", true, "");
    let invalid = rig.act("{\"action\":\"can\",\"dropId\":true}").err();
    rec.compare("a boolean dropId is refused", invalid, Some("dropId must be -1 or a standard CAN ID (0..2047)".to_string()), "emulation/run_emulator.py Emulator.action", true, "Python: isinstance(True, bool) is excluded");
    let invalid = rig.act("{\"action\":\"can\",\"connected\":\"yes\"}").err();
    rec.compare("a non-boolean connected is refused", invalid, Some("connected must be a boolean".to_string()), "emulation/run_emulator.py Emulator.action", true, "");
    let before = summary(&rig);
    rig.act(&format!("{{\"action\":\"can\",\"dropId\":{periodic}}}"))?;
    let start = time(&rig);
    rig.advance(3.0)?;
    let trace = rig.can_trace();
    let (_, _, frames) = window(&trace, start, start + 3.0);
    let of_id: Vec<bool> = frames.iter().filter(|(id, _)| *id == periodic).map(|(_, d)| *d).collect();
    rec.check(&format!("every frame with the dropped identifier 0x{periodic:03X} is discarded and counted"), !of_id.is_empty() && of_id.iter().all(|d| *d), Json::from_items(of_id.iter().copied()));
    let other_dropped = frames.iter().filter(|(id, d)| *id != periodic && *d).count();
    rec.check("other identifiers are still forwarded during selective loss", other_dropped == 0 && frames.iter().any(|(id, d)| *id != periodic && !*d), other_dropped as u64);
    let after = summary(&rig);
    rec.check("the dropped counter grows by the number of lost frames", summary_number(&after, "dropped").unwrap_or(0) - summary_number(&before, "dropped").unwrap_or(0) == of_id.len() as u64, after.clone());
    rec.expect("the summary shows the identifier being dropped", summary_number(&after, "dropId"), Some(u64::from(periodic)));
    rec.check("invariant: transmitted = scheduled + dropped", summary_number(&after, "transmitted") == Some(summary_number(&after, "scheduled").unwrap_or(0) + summary_number(&after, "dropped").unwrap_or(0)), after.clone());

    // ---- disconnect -------------------------------------------------------------------------------------------------------------
    rig.act("{\"action\":\"can\",\"connected\":false}")?;
    let start = time(&rig);
    rig.advance(2.5)?;
    let (scheduled, dropped, _) = window(&rig.can_trace(), start, start + 2.5);
    let disconnected = summary(&rig);
    rec.expect("the summary reports the link as disconnected", summary_field(&disconnected, "connected"), Some("False"));
    rec.check("with the link disconnected every frame is dropped (heartbeats of both boards)", scheduled == 0 && dropped >= 2, Json::from_items([scheduled, dropped]));
    rec.compare("the Renode disconnect dropped the next heartbeat 0x010 (payload 72 from main)", dropped >= 1, true, SOURCE, false, "validated-can.tsv: 0x010 dropped at 16.003 s while disconnected");

    // ---- restore -----------------------------------------------------------------------------------------------------------------
    rig.act("{\"action\":\"can\",\"connected\":true,\"dropId\":-1}")?;
    let start = time(&rig);
    rig.advance(2.5)?;
    let (scheduled, dropped, _) = window(&rig.can_trace(), start, start + 2.5);
    let restored = summary(&rig);
    rec.check("after restoring the link frames are forwarded again", scheduled > 0 && dropped == 0, Json::from_items([scheduled, dropped]));
    rec.compare("restored link summary: connected, dropId", Json::from_items([summary_field(&restored, "connected").map(str::to_string), summary_field(&restored, "dropId").map(str::to_string)]), Json::from_items(["True", "-1"]), SOURCE, true, "validatedFinalState: connected=True; dropId=-1");
    rec.check("invariant after restore: transmitted = scheduled + dropped", summary_number(&restored, "transmitted") == Some(summary_number(&restored, "scheduled").unwrap_or(0) + summary_number(&restored, "dropped").unwrap_or(0)), restored.clone());

    // ---- partial application: the runner applies `connected` before it validates `dropId` ----------------------------------------
    let error = rig.act("{\"action\":\"can\",\"connected\":false,\"dropId\":5000}").err();
    rec.check("a request with a valid connected and an invalid dropId reports the error", error.as_deref() == Some("dropId must be -1 or a standard CAN ID (0..2047)"), error.unwrap_or_default());
    rec.check("...but `connected` was already applied (runner order)", summary_field(&summary(&rig), "connected") == Some("False"), summary(&rig));
    rig.act("{\"action\":\"can\",\"connected\":true}")?;
    rec.check("faults: none on either board", rig.faults_clear(), Json::object().with("main", rig.faults(Which::Main)).with("handset", rig.faults(Which::Handset)));
    rec.file("can-trace.tsv", rig.can_trace().into_bytes());

    // ---- loss of a handset command: the B1 commit 0x082 never reaches main ---------------------------------------------------------
    let mut lossy = Rig::new(env, SessionConfig::default(), Profile::default())?;
    lossy.advance(6.5)?;
    lossy.act("{\"action\":\"can\",\"dropId\":130}")?;
    for action in ["down", "confirm", "confirm"] {
        press(&mut lossy, action)?;
    }
    let wizard = super::dual_wake::battery_wizard(&lossy, lossy.view());
    let dropped_commit = lossy.can_trace().lines().any(|l| l.contains("ngc-handset.can1\t0x082\t01\tdropped"));
    rec.check("the handset commit 0x082 is dropped by the link", dropped_commit, dropped_commit);
    rec.check("the handset wizard moved on to bank 2 (it believes B1 was committed)", wizard.get("activeBank").and_then(Json::as_u64) == Some(2) && wizard.get("b1Done").and_then(Json::as_u64) == Some(1), wizard.clone());
    rec.check("main never received B1: its battery type stays unset (0xFF)", lossy.u8(Which::Main, 0x2000_2444) == 0xFF, u64::from(lossy.u8(Which::Main, 0x2000_2444)));
    let png = lossy.png();
    rec.image("lost-commit.png", png);
    lossy.session.system_mut().apply_input(&Input::CanDropId(-1))?;
    rec.note("handset and main disagree about B1 after the loss; this is a synthetic fault injection, not a reproduction of a reported device failure");
    rec.limitation("The link is functional: no arbitration, bus load, electrical errors or timing; a scheduled frame does not prove FIFO acceptance or application handling. Disconnect and DropId are explicit test faults.");
    Ok(rec.finish(env))
}
