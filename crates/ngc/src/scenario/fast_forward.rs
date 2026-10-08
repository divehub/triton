//! `fast-forward`: the CPU's exact idle fast-forward must not change anything the guest or the viewer can observe.
//!
//! The same viewer-action script (boot to the B1 prompt, navigation and confirmation, CAN disconnect and restore, cold boot
//! to the observed standby, Wake) runs on two sessions, one with `idle_fast_forward` on and one with it off. After every
//! step the SHA-256 `fingerprint` of the architectural state (registers, retire counts, both SRAMs of each board, the LCD
//! GRAM, the CAN trace and the UART tails) and the whole state document (except the fast-forward statistics themselves)
//! have to be identical. This is an engine-internal proof (DESIGN.md section 10); Renode has no such option, so there is
//! nothing to compare with except the Renode-equal outputs of the other scenarios, which run with the fast-forward on.

use super::*;
use crate::system::BootMode;

/// State fields that describe the fast-forward itself and therefore differ by design.
const IGNORED: [&str; 2] = ["idleSkip", "idleFastForward"];

struct Point {
    label: String,
    state: Json,
    fingerprint: String,
}

fn differing_keys(a: &Json, b: &Json) -> Vec<String> {
    let mut keys = Vec::new();
    for (key, value) in a.as_object().unwrap_or(&[]) {
        if !IGNORED.contains(&key.as_str()) && b.get(key) != Some(value) {
            keys.push(key.clone());
        }
    }
    for (key, _) in b.as_object().unwrap_or(&[]) {
        if !IGNORED.contains(&key.as_str()) && a.get(key).is_none() {
            keys.push(key.clone());
        }
    }
    keys
}

fn point(rig: &Rig, label: &str, points: &mut Vec<Point>) {
    points.push(Point { label: label.to_string(), state: rig.state(), fingerprint: rig.session.system().fingerprint() });
}

/// The action script; returns the checkpoints and the finished rig.
fn script(env: &ScenarioEnv<'_>, fast_forward: bool) -> Result<(Rig, Vec<Point>), String> {
    let config = SessionConfig { idle_fast_forward: fast_forward, ..SessionConfig::default() };
    let mut rig = Rig::new(env, config, Profile::default())?;
    let mut points = Vec::new();
    rig.advance(2.0)?;
    point(&rig, "2.0 s: main running, handset released at 1.05 s", &mut points);
    rig.advance(3.5)?;
    point(&rig, "5.5 s: B1 prompt", &mut points);
    for action in ["down", "confirm", "confirm"] {
        rig.act(&format!("{{\"action\":\"{action}\"}}"))?;
        rig.advance(0.65)?;
        point(&rig, &format!("after {action}"), &mut points);
    }
    rig.act("{\"action\":\"can\",\"connected\":false}")?;
    rig.advance(0.5)?;
    point(&rig, "CAN disconnected for 0.5 s", &mut points);
    rig.act("{\"action\":\"can\",\"connected\":true}")?;
    rig.advance(0.5)?;
    point(&rig, "CAN restored for 0.5 s", &mut points);
    rig.act("{\"action\":\"cold\"}")?;
    rig.advance(2.0)?;
    point(&rig, "cold boot, observed standby", &mut points);
    rig.act("{\"action\":\"wake\"}")?;
    rig.advance(3.0)?;
    point(&rig, "wake, 3 s", &mut points);
    Ok((rig, points))
}

pub(super) fn run(env: &ScenarioEnv<'_>) -> Result<ScenarioReport, String> {
    let mut rec = Recorder::new("fast-forward");
    let (mut on, on_points) = script(env, true)?;
    let (mut off, off_points) = script(env, false)?;
    rec.check("both runs completed the same number of checkpoints", on_points.len() == off_points.len() && on_points.len() == 9, on_points.len() as u64);

    let mut all_equal = true;
    for (a, b) in on_points.iter().zip(&off_points) {
        let keys = differing_keys(&a.state, &b.state);
        let equal = a.fingerprint == b.fingerprint && keys.is_empty();
        all_equal &= equal;
        rec.check(&format!("{}: fingerprint and state document identical", a.label), equal, Json::object().with("on", a.fingerprint.as_str()).with("off", b.fingerprint.as_str()).with("differingStateKeys", Json::from_items(keys.iter().map(String::as_str))));
    }
    rec.check("every checkpoint identical (the fast-forward is exact)", all_equal, all_equal);

    let counters = |rig: &Rig, which: Which| rig.session.system().counters(which).expect("counters");
    let (on_main, on_handset) = (counters(&on, Which::Main), counters(&on, Which::Handset));
    let (off_main, off_handset) = (counters(&off, Which::Main), counters(&off, Which::Handset));
    rec.check("executed instructions are identical per board", on_main.instructions == off_main.instructions && on_handset.instructions == off_handset.instructions, Json::object().with("main", on_main.instructions).with("handset", on_handset.instructions));
    let skipped_on = on_main.fast_forward.skipped_instructions + on_handset.fast_forward.skipped_instructions;
    let total = on_main.instructions + on_handset.instructions;
    rec.check(
        "with the fast-forward on, a large share of the retired instructions is skipped polling-loop iterations",
        skipped_on > 0,
        Json::object().with("skippedInstructions", skipped_on).with("retiredInstructions", total).with("rejectedLoopEntries", on_main.fast_forward.failed_verifications + on_handset.fast_forward.failed_verifications),
    );
    rec.check(
        "with the fast-forward off nothing is skipped or verified",
        off_main.fast_forward.skipped_instructions + off_handset.fast_forward.skipped_instructions == 0 && off_main.fast_forward.failed_verifications + off_handset.fast_forward.failed_verifications == 0,
        Json::object().with("skipped", off_main.fast_forward.skipped_instructions + off_handset.fast_forward.skipped_instructions),
    );
    rec.check("no CPU faults in either run", on.faults_clear() && off.faults_clear(), Json::object().with("on", on.faults(Which::Handset)).with("off", off.faults(Which::Handset)));

    // The toggle is a runtime switch of a live session (the viewer's `performance` control of the runner has no
    // counterpart in Renode; this engine allows it between steps): switching it mid-run must not change the result.
    let config = SessionConfig { boot_mode: BootMode::HandsetWake, ..SessionConfig::default() };
    let mut toggled = Rig::new(env, config, Profile::default())?;
    toggled.advance(1.5)?;
    toggled.session.set_idle_fast_forward(false);
    toggled.advance(1.5)?;
    toggled.session.set_idle_fast_forward(true);
    toggled.advance(2.5)?;
    let mut reference = Rig::new(env, SessionConfig::default(), Profile::default())?;
    reference.advance(5.5)?;
    rec.check("toggling the fast-forward on a live session during a 5.5 s boot changes nothing", toggled.session.system().fingerprint() == reference.session.system().fingerprint(), toggled.session.system().fingerprint());
    rec.step("final state of the fast-forward-on run", &on.state(), Json::object());
    let (png_on, png_off) = (on.png(), off.png());
    rec.check("the final LCD frame is identical with and without the fast-forward", png_on == png_off, png_on.len() as u64);
    rec.image("final-wake.png", png_on);
    rec.note("The fast-forward (crates/armv7m/src/fastfwd.rs) recognises short backward-branch loops with a pure body whose register snapshot repeats after one iteration and whose loads address plain memory; it then skips whole iterations by advancing the retire count up to the next event or the end of the run budget, which is bit-identical to executing them. failedVerifications counts loop entries that did not reach such a fixed point.");
    rec.limitation("This is an equivalence proof between two modes of the same engine on the original firmware; it says nothing about the accuracy of either mode against the physical device or Renode beyond the equality of the other scenarios (which run with the fast-forward on).");
    Ok(rec.finish(env))
}
