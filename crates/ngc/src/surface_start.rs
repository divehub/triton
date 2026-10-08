//! The start-at-the-surface fixture (DESIGN.md "Decompression state handling").
//!
//! The sensor inputs persist in `inputs.json`, so a profile that was left under water makes the next board creation start
//! at that depth. The original firmware takes the pressure it reads at start-up as the *surface* pressure (it resets its
//! tissues to equilibrium with it), so a start under water gives a wrong surface and wrong tissue pressures.
//!
//! **An emulator fixture, on by default and switchable** (`SessionConfig::start_at_surface`, `--no-start-at-surface`,
//! `startAtSurface`): at every board creation (session start, Restart, Cold, Wake, serial change) both pressure inputs are
//! set to the configured surface pressure plus each sensor's offset, so the depth is 0. The offset of a sensor is its
//! reading minus the mean of the two readings, which is what the basic view of the page derives as the sensor offset as
//! long as the depth is not negative. A **new session** (a boot or a profile replacement, [`Session::new`]) also resets the
//! three oxygen-cell inputs to their defaults, so the cells and the depth start from the same known state. Restart, Cold and
//! Wake keep the oxygen inputs, because a calibration made with them stays meaningful.
//!
//! The file format of `inputs.json` is unchanged: the choice of the surface pressure is a session setting (and the page's own
//! setting), not an input.
//!
//! [`Session::new`]: crate::session::Session::new

use crate::fixtures::Inputs;
use emu_core::Json;

/// The surface pressure of a standard atmosphere used by the page and the input defaults, in mbar.
pub const DEFAULT_SURFACE_MBAR: f64 = 1013.25;
/// The range of the pressure inputs (and of the surface pressure setting), in mbar.
pub const PRESSURE_RANGE: (f64, f64) = (100.0, 30000.0);

/// The refusal of a surface pressure outside [`PRESSURE_RANGE`] (the message shape of the input ranges).
pub const SURFACE_RANGE_MESSAGE: &str = "surfacePressureMbar must be between 100 and 30000";

/// Checks a surface pressure setting.
pub fn validate_surface(mbar: f64) -> Result<f64, String> {
    if mbar.is_finite() && mbar >= PRESSURE_RANGE.0 && mbar <= PRESSURE_RANGE.1 {
        Ok(mbar)
    } else {
        Err(SURFACE_RANGE_MESSAGE.to_string())
    }
}

/// What the fixture did at the last board creation (`startAtSurface` of the state).
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceStart {
    /// The switch: `SessionConfig::start_at_surface`.
    pub enabled: bool,
    /// The surface pressure setting in mbar.
    pub surface_pressure_mbar: f64,
    /// The inputs were set to the surface at the last board creation (a dual run with the switch on).
    pub applied: bool,
    /// The last board creation was a new session, which also resets the oxygen cells.
    pub oxygen_reset: bool,
    /// The inputs the last board creation changed (names of the `inputs` object).
    pub changed: Vec<&'static str>,
}

impl SurfaceStart {
    /// Before any board exists.
    pub fn idle(enabled: bool, surface_pressure_mbar: f64) -> Self {
        Self { enabled, surface_pressure_mbar, applied: false, oxygen_reset: false, changed: Vec::new() }
    }

    /// `{"enabled", "surfacePressureMbar", "applied", "oxygenReset", "changedInputs", "note"}`.
    pub fn to_json(&self) -> Json {
        let note = if !self.enabled {
            "Switched off (startAtSurface: false, or --no-start-at-surface): the saved sensor inputs are used as they are."
        } else {
            "Emulator fixture: at every board creation both pressure inputs are set to the surface pressure plus each sensor's offset (depth 0); a new session also resets the oxygen cells to their defaults."
        };
        Json::object()
            .with("enabled", self.enabled)
            .with("surfacePressureMbar", self.surface_pressure_mbar)
            .with("applied", self.applied)
            .with("oxygenReset", self.oxygen_reset)
            .with("changedInputs", Json::from_items(self.changed.iter().copied()))
            .with("note", note)
    }
}

/// The inputs a board creation starts with. `new_session` is true for [`crate::session::Session::new`] and false for
/// Restart, Cold, Wake and the serial change. Returns the inputs to use and the report; with the switch off, or in a
/// handset-only run (which has no sensors), the inputs come back unchanged.
pub fn apply(enabled: bool, dual: bool, surface_pressure_mbar: f64, new_session: bool, inputs: &Inputs) -> (Inputs, SurfaceStart) {
    let mut report = SurfaceStart::idle(enabled, surface_pressure_mbar);
    if !enabled || !dual {
        return (inputs.clone(), report);
    }
    let mut next = inputs.clone();
    let mean = (inputs.pressure_mbar[0] + inputs.pressure_mbar[1]) / 2.0;
    for sensor in 0..2 {
        let offset = inputs.pressure_mbar[sensor] - mean;
        next.pressure_mbar[sensor] = (surface_pressure_mbar + offset).clamp(PRESSURE_RANGE.0, PRESSURE_RANGE.1);
    }
    if new_session {
        next.oxygen_mv = Inputs::defaults().oxygen_mv;
    }
    let pairs = [
        ("oxygen1Mv", inputs.oxygen_mv[0], next.oxygen_mv[0]),
        ("oxygen2Mv", inputs.oxygen_mv[1], next.oxygen_mv[1]),
        ("oxygen3Mv", inputs.oxygen_mv[2], next.oxygen_mv[2]),
        ("pressure1Mbar", inputs.pressure_mbar[0], next.pressure_mbar[0]),
        ("pressure2Mbar", inputs.pressure_mbar[1], next.pressure_mbar[1]),
    ];
    report.changed = pairs.iter().filter(|(_, before, after)| before != after).map(|(name, _, _)| *name).collect();
    report.applied = true;
    report.oxygen_reset = new_session;
    (next, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deep() -> Inputs {
        Inputs { pressure_mbar: [3013.3, 3013.3], oxygen_mv: [60.0, 61.0, 59.0], ..Inputs::defaults() }
    }

    #[test]
    fn both_pressure_inputs_return_to_the_surface_and_keep_their_offsets() {
        let mut inputs = deep();
        inputs.pressure_mbar = [3013.5, 3015.5];
        let (restart, report) = apply(true, true, 1013.25, false, &inputs);
        assert_eq!(restart.pressure_mbar, [1012.25, 1014.25], "surface plus the offset of each sensor (-1 and +1 mbar from the mean)");
        assert_eq!(restart.oxygen_mv, [60.0, 61.0, 59.0], "a restart keeps the oxygen inputs");
        assert!(report.applied && !report.oxygen_reset && report.changed == ["pressure1Mbar", "pressure2Mbar"], "{report:?}");
        // Equal sensors land exactly on the surface.
        let (equal, _) = apply(true, true, 987.5, false, &deep());
        assert_eq!(equal.pressure_mbar, [987.5, 987.5]);
        // A new session also resets the oxygen cells to their defaults.
        let (fresh, report) = apply(true, true, 1013.25, true, &deep());
        assert_eq!((fresh.pressure_mbar, fresh.oxygen_mv), ([1013.25; 2], [10.0; 3]));
        assert!(report.oxygen_reset && report.changed == ["oxygen1Mv", "oxygen2Mv", "oxygen3Mv", "pressure1Mbar", "pressure2Mbar"], "{report:?}");
        // Everything else stays as saved: batteries, temperatures, the acquisition settings.
        let saved = Inputs { battery_mv: [1500.0, 1450.0], temperature_c: [4.0, 5.0], noise_seed: 9, ..deep() };
        let (next, _) = apply(true, true, 1013.25, true, &saved);
        assert_eq!((next.battery_mv, next.temperature_c, next.noise_seed), (saved.battery_mv, saved.temperature_c, 9));
    }

    #[test]
    fn nothing_changes_when_the_inputs_are_at_the_surface_already_or_the_switch_is_off() {
        let at_surface = Inputs::defaults();
        let (same, report) = apply(true, true, 1013.25, true, &at_surface);
        assert_eq!(same, at_surface);
        assert!(report.applied && report.changed.is_empty() && report.oxygen_reset, "{report:?}");
        let (off, report) = apply(false, true, 1013.25, true, &deep());
        assert_eq!(off, deep());
        assert!(!report.enabled && !report.applied && report.changed.is_empty());
        let (handset, report) = apply(true, false, 1013.25, true, &deep());
        assert_eq!(handset, deep());
        assert!(!report.applied);
    }

    #[test]
    fn an_offset_that_would_leave_the_pressure_range_is_clamped_and_the_surface_is_validated() {
        let wide = Inputs { pressure_mbar: [100.0, 300.0], ..Inputs::defaults() };
        let (next, _) = apply(true, true, 100.0, false, &wide);
        assert_eq!(next.pressure_mbar, [100.0, 200.0], "mean 200, offsets -100 and +100 around 100 mbar: the lower one is clamped to 100");
        assert_eq!(validate_surface(1013.25), Ok(1013.25));
        for bad in [99.9, 30000.1, f64::NAN, f64::INFINITY] {
            assert_eq!(validate_surface(bad), Err(SURFACE_RANGE_MESSAGE.to_string()), "{bad}");
        }
        assert_eq!(validate_surface(100.0), Ok(100.0));
        assert_eq!(validate_surface(30000.0), Ok(30000.0));
    }

    #[test]
    fn the_report_names_the_fixture() {
        let (_, report) = apply(true, true, 1013.25, true, &deep());
        let json = report.to_json();
        assert_eq!(json.get("enabled"), Some(&Json::Bool(true)));
        assert_eq!(json.get("surfacePressureMbar").and_then(Json::as_f64), Some(1013.25));
        assert!(json.get("note").and_then(Json::as_str).unwrap().starts_with("Emulator fixture"));
        assert!(SurfaceStart::idle(false, 1013.25).to_json().get("note").and_then(Json::as_str).unwrap().starts_with("Switched off"));
    }
}
