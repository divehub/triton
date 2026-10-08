//! The [`Host`] over `ngc::session::Session` (DESIGN.md section 14): the single host-facing engine API. The
//! C ABI in `lib.rs` never reaches into boards; everything behavioural (actions, state, profile, capture) is the
//! session's.

use crate::host::{FrameRef, Host, HostConfig, Part, StagedProfile};
use emu_core::to_secs_f64;
use ngc::firmware::Firmware;
use ngc::persistence::{INPUTS_FILE, LED_COLORS_FILE, RTC_STATE_FILE};
use ngc::session::{HostInfo, Profile, Session, SessionConfig};
use ngc::system::{BootMode, Mode};

pub struct SessionHost {
    session: Session,
}

fn text(file: &str, bytes: Option<Vec<u8>>) -> Result<Option<String>, String> {
    match bytes {
        Some(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| format!("{file} is not UTF-8 text")),
        None => Ok(None),
    }
}

fn parts(profile: Profile) -> Vec<Part> {
    profile.files().into_iter().map(|(name, data)| Part::new(name, data)).collect()
}

impl SessionHost {
    pub fn create(
        config: &HostConfig,
        main: Option<&Firmware>,
        handset: &Firmware,
        staged: StagedProfile,
    ) -> Result<Box<dyn Host>, String> {
        let profile = Profile {
            eeprom: staged.eeprom,
            nor: staged.nor,
            rtc_state: text(RTC_STATE_FILE, staged.rtc_state)?,
            inputs: text(INPUTS_FILE, staged.inputs)?,
            led_colors: text(LED_COLORS_FILE, staged.led_colors)?,
        };
        let config = SessionConfig {
            mode: if config.dual { Mode::Dual } else { Mode::HandsetOnly },
            boot_mode: if config.cold { BootMode::Cold } else { BootMode::HandsetWake },
            simultaneous_start: config.simultaneous_start,
            idle_fast_forward: config.idle_fast_forward,
            routine_accel: config.routine_accel,
            routine_accel_shadow: config.routine_accel_shadow,
            adc_sample: config.adc_sample,
            start_paused: config.start_paused,
            i2c_idle_high: config.i2c_idle_high,
            history_nonce: config.history_nonce,
            deco_storage_fixture: config.deco_storage_fixture,
            start_at_surface: config.start_at_surface,
            surface_pressure_mbar: config.surface_pressure_mbar,
        };
        Ok(Box::new(SessionHost { session: Session::new(config, main, handset, profile)? }))
    }
}

impl Host for SessionHost {
    fn run_for(&mut self, seconds: f64) {
        self.session.run_for(seconds);
    }

    fn running(&self) -> bool {
        self.session.running()
    }

    fn time_seconds(&self) -> f64 {
        to_secs_f64(self.session.virtual_ns())
    }

    fn action(&mut self, request_json: &str) -> Result<String, String> {
        self.session.action(request_json)
    }

    fn state_json(&self) -> String {
        self.session.state_json()
    }

    fn checkpoint_json(&mut self) -> String {
        ngc::scenario::dive::checkpoint(&mut self.session, "").to_json().to_string()
    }

    fn frame(&mut self) -> FrameRef {
        let view = self.session.frame();
        FrameRef { width: view.width, height: view.height, version: view.version, ptr: view.rgba.as_ptr(), len: view.rgba.len() }
    }

    fn take_profile_changes(&mut self) -> Vec<Part> {
        self.session.take_profile_changes().map(parts).unwrap_or_default()
    }

    fn export_profile(&mut self) -> Vec<Part> {
        parts(self.session.export_profile())
    }

    fn capture(&mut self) -> (String, Vec<Part>) {
        let capture = self.session.capture();
        let mut files = vec![Part::new("state.json", capture.state_json.into_bytes()), Part::new("lcd.png", capture.lcd_png)];
        // The runner writes the CAN trace only for a dual run (an empty trace is still a file there).
        if self.session.config().mode == Mode::Dual {
            files.push(Part::new("can-trace.tsv", capture.can_trace_tsv.into_bytes()));
        }
        (capture.name, files)
    }

    fn shutdown(self: Box<Self>) -> Vec<Part> {
        parts(self.session.shutdown())
    }

    fn set_clock(&mut self, utc_micros: i64) {
        self.session.set_utc_micros(utc_micros);
    }

    fn set_seed(&mut self, seed: u64) {
        self.session.set_epoch_seed(seed);
    }

    fn set_host_info(&mut self, realtime_factor: Option<f64>, pacing: Option<String>) {
        self.session.set_host_info(HostInfo { pacing, realtime_factor });
    }
}
