//! WebAssembly exports (extern "C") for the browser worker and the Node tools.
//! See `DESIGN.md` (sections 4, 10 and 14).
//!
//! There is no wasm-bindgen: the host calls plain functions, passes byte buffers through linear memory
//! (`ngc_alloc` / `ngc_free`) and reads text and binary results from an internal output buffer
//! (`ngc_output_ptr` plus the length a function returns) or, for named files, from the *parts* list
//! (`ngc_part_*`). Unless stated otherwise a function returns `0` for success, `1` for failure with the message
//! available through `ngc_error`, and `2` when there is no session. The module never reads a wall clock: pacing
//! belongs to the host (`ngc_session_run_for` runs exactly the requested virtual time, rounded up to whole
//! 100 us quanta).
//!
//! # Browser API (the session)
//!
//! ```text
//! ngc_init()                           install the panic hook (idempotent); ngc_panic_ptr/len read its message
//! ngc_engine()                         "ngc-wasm/<version>" into the output buffer, returns its length
//! ngc_firmware_inspect(ptr, len)       verification report of an SREC file (JSON) into the output buffer;
//!                                      `release` is `{"id", "label"}` of the identified firmware release or `null`
//! ngc_set_firmware(role, ptr, len)     role 0 main 5.8, 1 handset 65.3: verify and keep (error text on failure); the
//!                                      image of either supported release (TRITON-5.8-65.3, NEPTUN-5.8-65.3) is accepted
//!                                      per role, a mixed pair is refused by ngc_session_create
//! ngc_firmware_clear(role)
//! ngc_profile_clear(); ngc_profile_set(kind, ptr, len)    stage profile files for the next session
//!                                      kind 0 eeprom.bin, 1 nor.ngc, 2 rtc-state.json, 3 inputs.json, 4 led-colors.json
//! ngc_session_create(cfg_ptr, cfg_len) JSON {mode, bootMode, simultaneousStart, idleFastForward, adcSample, startPaused,
//!                                      i2cIdleHigh, historyNonce}, every key optional:
//!                                      `i2cIdleHigh` (default true) drives the main board's PB6/PB7/PB10/PB11 high before
//!                                      the first instruction (functional I2C idle-line fixture), false leaves them low;
//!                                      `historyNonce` (unsigned integer < 2^64, default 0) is the host's random part of
//!                                      the state's `outputHistoryEpoch`, `"<historyNonce>-<generation>"`.
//!                                      Fails with a clear message when the main and handset images are of different
//!                                      releases, and for `bootMode` "cold" on a release without a cold-boot route (NEPTUN)
//! ngc_session_run_for(seconds)         0 still running, 1 paused/standby/error, 2 no session
//! ngc_session_action(ptr, len)         runner action JSON; the state JSON (or the error text) in the output buffer
//! ngc_session_state()                  state JSON into the output buffer, returns its length
//! ngc_session_frame()                  brings the LCD up to date, returns the frame version (f64);
//!                                      ngc_frame_ptr/len/width/height describe the RGBA bytes (read them at once)
//! ngc_session_profile_changes(), ngc_session_profile_export(), ngc_session_capture(), ngc_session_shutdown()
//!                                      fill the parts list and return the number of parts
//! ngc_capture_name()                   name of the last capture into the output buffer
//! ngc_part_count/name/ptr/len, ngc_parts_clear
//! ```
//!
//! # Benchmark API (kept for `web/bench-node.mjs`)
//!
//! ```text
//! ngc_create(mode, flags, adc_sample)   mode 0 dual / 1 handset; flags bit0 simultaneous start,
//!                                       bit1 cold boot, bit2 idle fast-forward OFF
//! ngc_run_for(seconds)                  1 when the system stopped (standby / error), else 0
//! ngc_time_ns(), ngc_instructions(w)    u64 (BigInt in JS); *_f64 variants for convenience
//! ngc_idle_skipped(w), ngc_slices(w)    fast-forwarded instructions / chunks of board w (f64)
//! ngc_status()                          status JSON into the output buffer, returns its length
//! ngc_fingerprint()                     state digest (hex) into the output buffer, returns its length
//! ngc_navigate(up), ngc_confirm(), ngc_press(mask)   physical button inputs (return 0 / error)
//! ```

mod host;
mod session_host;

use host::{FrameRef, Host, HostConfig, Part, StagedProfile};
use ngc::firmware::{self, Firmware, Role};
use ngc::system::{BootMode, BuildOptions, Input, Mode, System, SystemConfig, Which};
use std::cell::RefCell;

/// Engine identification string (`"engine": "ngc-wasm/<version>"` in every state and capture).
pub const ENGINE: &str = concat!("ngc-wasm/", env!("CARGO_PKG_VERSION"));

#[derive(Default)]
struct State {
    main: Option<Firmware>,
    handset: Option<Firmware>,
    /// Benchmark path: the bare system.
    system: Option<System>,
    /// Browser path: the session behind the host seam.
    host: Option<Box<dyn Host>>,
    staged: StagedProfile,
    /// Named files returned by the last profile / capture / shutdown call.
    parts: Vec<Part>,
    capture_name: String,
    frame: FrameRef,
    /// Text or bytes result of the last call that returns a length (JSON, hex, error message).
    output: Vec<u8>,
    error: String,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// Message of the last panic. Kept outside `STATE`: the panic may have happened while `STATE` was borrowed.
    static PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

fn fail(state: &mut State, message: impl Into<String>) -> i32 {
    state.error = message.into();
    1
}

fn text_result(state: &mut State, text: String) -> u32 {
    state.output = text.into_bytes();
    state.output.len() as u32
}

fn which(index: u32) -> Which {
    if index == 0 {
        Which::Main
    } else {
        Which::Handset
    }
}

/// Bytes the host placed in linear memory.
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory (or `len` zero).
unsafe fn input_bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len)
    }
}

// ---------------------------------------------------------------------------------------------------
// memory, identity, diagnostics

/// Allocates `len` bytes of linear memory for the host to fill (release with `ngc_free`).
#[no_mangle]
pub extern "C" fn ngc_alloc(len: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

/// Releases a buffer from `ngc_alloc` (same `len`).
///
/// # Safety
/// `ptr` must come from `ngc_alloc(len)` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ngc_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len.max(1)));
    }
}

/// Engine version number (major * 10000 + minor * 100 + patch).
#[no_mangle]
pub extern "C" fn ngc_version() -> u32 {
    let mut parts = env!("CARGO_PKG_VERSION").split('.').map(|part| part.parse::<u32>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major * 10_000 + minor * 100 + patch
}

/// Installs a panic hook that records the message (the build aborts on panic, so the host sees a trap; the
/// message explains it). Idempotent.
#[no_mangle]
pub extern "C" fn ngc_init() {
    std::panic::set_hook(Box::new(|info| {
        let text = info.to_string();
        PANIC.with(|panic| {
            if let Ok(mut panic) = panic.try_borrow_mut() {
                *panic = text;
            }
        });
    }));
}

/// Pointer to the recorded panic message (`ngc_panic_len` bytes of UTF-8).
#[no_mangle]
pub extern "C" fn ngc_panic_ptr() -> *const u8 {
    PANIC.with(|panic| panic.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn ngc_panic_len() -> u32 {
    PANIC.with(|panic| panic.borrow().len() as u32)
}

/// Writes `"ngc-wasm/<version>"` into the output buffer and returns its length.
#[no_mangle]
pub extern "C" fn ngc_engine() -> u32 {
    with_state(|state| text_result(state, ENGINE.to_string()))
}

/// Writes the text of the last error into the output buffer and returns its length.
#[no_mangle]
pub extern "C" fn ngc_error() -> u32 {
    with_state(|state| {
        let text = state.error.clone();
        text_result(state, text)
    })
}

/// Pointer to the output buffer filled by the last call that returned a length.
#[no_mangle]
pub extern "C" fn ngc_output_ptr() -> *const u8 {
    with_state(|state| state.output.as_ptr())
}

/// Length of the output buffer (for calls that return a status code instead of a length, like
/// `ngc_session_action`).
#[no_mangle]
pub extern "C" fn ngc_output_len() -> u32 {
    with_state(|state| state.output.len() as u32)
}

// ---------------------------------------------------------------------------------------------------
// firmware

/// Verifies an SREC file without keeping it: writes the report (identity, SHA-256, record counts, span, vector
/// table, the list of checks and an `error` text for malformed files) as JSON into the output buffer and
/// returns its length. `message` is `null` for an accepted image, otherwise the engine's refusal text.
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_firmware_inspect(ptr: *const u8, len: usize) -> u32 {
    let bytes = input_bytes(ptr, len);
    let (mut json, message) = match firmware::load(bytes, None) {
        Ok(image) => (image.report.to_json(), None),
        Err(error) => (firmware::inspect(bytes).to_json(), Some(error.to_string())),
    };
    json.insert("message", message);
    let text = json.to_string_with(&emu_core::json::WriteOptions::compact());
    with_state(|state| text_result(state, text))
}

/// Verifies and keeps a firmware SREC (`role` 0 = main 5.8, 1 = handset 65.3). The text of a verification
/// failure (unknown image, wrong board, failed fact) is available through `ngc_error`.
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_set_firmware(role: u32, ptr: *const u8, len: usize) -> i32 {
    let bytes = input_bytes(ptr, len);
    let expected = if role == 0 { Role::Main } else { Role::Handset };
    let loaded = firmware::load(bytes, Some(expected));
    with_state(|state| match loaded {
        Ok(image) => {
            if role == 0 {
                state.main = Some(image);
            } else {
                state.handset = Some(image);
            }
            0
        }
        Err(error) => fail(state, error.to_string()),
    })
}

/// Forgets a kept firmware image (`role` as for `ngc_set_firmware`).
#[no_mangle]
pub extern "C" fn ngc_firmware_clear(role: u32) {
    with_state(|state| {
        if role == 0 {
            state.main = None;
        } else {
            state.handset = None;
        }
    });
}

// ---------------------------------------------------------------------------------------------------
// session

/// Forgets every staged profile file.
#[no_mangle]
pub extern "C" fn ngc_profile_clear() {
    with_state(|state| state.staged = StagedProfile::default());
}

/// Stages one profile file for the next `ngc_session_create` (kind 0 `eeprom.bin`, 1 `nor.ngc`,
/// 2 `rtc-state.json`, 3 `inputs.json`, 4 `led-colors.json`).
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_profile_set(kind: u32, ptr: *const u8, len: usize) -> i32 {
    let bytes = input_bytes(ptr, len).to_vec();
    with_state(|state| match state.staged.slot_mut(kind) {
        Some(slot) => {
            *slot = Some(bytes);
            0
        }
        None => fail(state, "unknown profile file kind"),
    })
}

/// Creates the session from the kept firmware, the staged profile (consumed) and a JSON configuration
/// `{mode: "dual"|"handset", bootMode: "handset-wake"|"cold", simultaneousStart, idleFastForward, adcSample,
/// startPaused}`; every key is optional. A new session replaces an existing one without saving it; when
/// creation fails (bad profile, missing firmware) the existing session is left untouched.
///
/// # Safety
/// `cfg_ptr..cfg_ptr+cfg_len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_session_create(cfg_ptr: *const u8, cfg_len: usize) -> i32 {
    let config = match std::str::from_utf8(input_bytes(cfg_ptr, cfg_len)) {
        Ok(text) => HostConfig::from_json(text),
        Err(_) => Err("the session configuration is not UTF-8 text".to_string()),
    };
    with_state(|state| {
        let profile = std::mem::take(&mut state.staged);
        let config = match config {
            Ok(config) => config,
            Err(error) => return fail(state, error),
        };
        let Some(handset) = state.handset.as_ref() else {
            return fail(state, "the handset firmware has not been provided");
        };
        let main = if config.dual {
            match state.main.as_ref() {
                Some(main) => Some(main),
                None => return fail(state, "the dual system needs the main firmware"),
            }
        } else {
            None
        };
        match session_host::SessionHost::create(&config, main, handset, profile) {
            Ok(host) => {
                state.host = Some(host);
                state.frame = FrameRef::EMPTY;
                0
            }
            Err(error) => fail(state, error),
        }
    })
}

/// Drops the session without saving anything (the kept firmware stays).
#[no_mangle]
pub extern "C" fn ngc_session_destroy() {
    with_state(|state| {
        state.host = None;
        state.frame = FrameRef::EMPTY;
    });
}

/// 1 while a session exists.
#[no_mangle]
pub extern "C" fn ngc_session_active() -> u32 {
    with_state(|state| u32::from(state.host.is_some()))
}

/// Runs `seconds` of virtual time. Returns 0 while the session is running, 1 when it is paused or stopped
/// (standby, error), 2 without a session.
#[no_mangle]
pub extern "C" fn ngc_session_run_for(seconds: f64) -> i32 {
    with_state(|state| match state.host.as_mut() {
        Some(host) => {
            host.run_for(seconds);
            i32::from(!host.running())
        }
        None => 2,
    })
}

/// 1 while the session is running (not paused, in standby or stopped by an error).
#[no_mangle]
pub extern "C" fn ngc_session_running() -> u32 {
    with_state(|state| state.host.as_ref().map_or(0, |host| u32::from(host.running())))
}

/// Virtual time of the session in seconds.
#[no_mangle]
pub extern "C" fn ngc_session_time() -> f64 {
    with_state(|state| state.host.as_ref().map_or(0.0, |host| host.time_seconds()))
}

/// Tells the session the current UTC time (microseconds since the Unix epoch; the engine has no clock). Capture
/// folder names are derived from it.
#[no_mangle]
pub extern "C" fn ngc_session_set_clock(utc_micros: f64) {
    with_state(|state| {
        if let Some(host) = state.host.as_mut() {
            if utc_micros.is_finite() {
                host.set_clock(utc_micros as i64);
            }
        }
    });
}

/// Deprecated and without effect: the `outputHistoryEpoch` is `"<historyNonce>-<generation>"` and the nonce comes from the
/// session-create JSON (`historyNonce`). Kept so that older hosts keep working.
#[no_mangle]
pub extern "C" fn ngc_session_set_seed(lo: u32, hi: u32) {
    with_state(|state| {
        if let Some(host) = state.host.as_mut() {
            host.set_seed((u64::from(hi) << 32) | u64::from(lo));
        }
    });
}

/// What only the host can measure, for the state document: the real-time factor (NaN: none yet) and a text
/// describing the pacing (empty: the engine default).
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_session_host_info(realtime_factor: f64, ptr: *const u8, len: usize) {
    let pacing = String::from_utf8_lossy(input_bytes(ptr, len)).into_owned();
    with_state(|state| {
        if let Some(host) = state.host.as_mut() {
            host.set_host_info(realtime_factor.is_finite().then_some(realtime_factor), (!pacing.is_empty()).then_some(pacing));
        }
    });
}

/// One UI action (the runner's `POST /api/action` JSON). On success the state JSON is in the output buffer
/// (return 0); on failure the message is available through `ngc_error` and the output buffer (return 1).
///
/// # Safety
/// `ptr..ptr+len` must be readable linear memory.
#[no_mangle]
pub unsafe extern "C" fn ngc_session_action(ptr: *const u8, len: usize) -> i32 {
    let request = String::from_utf8_lossy(input_bytes(ptr, len)).into_owned();
    with_state(|state| {
        let Some(host) = state.host.as_mut() else { return 2 };
        match host.action(&request) {
            Ok(json) => {
                text_result(state, json);
                0
            }
            Err(error) => {
                text_result(state, error.clone());
                fail(state, error)
            }
        }
    })
}

/// Writes the state JSON into the output buffer and returns its length (0 without a session).
#[no_mangle]
pub extern "C" fn ngc_session_state() -> u32 {
    with_state(|state| {
        let text = state.host.as_ref().map(|host| host.state_json()).unwrap_or_default();
        text_result(state, text)
    })
}

/// Brings the LCD visible buffer up to date and returns the frame version (it changes only when the visible
/// pixels or the geometry change). `ngc_frame_ptr` / `ngc_frame_len` / `ngc_frame_width` / `ngc_frame_height`
/// then describe the RGBA bytes in linear memory; read them before the next call that runs the session.
#[no_mangle]
pub extern "C" fn ngc_session_frame() -> f64 {
    with_state(|state| {
        state.frame = state.host.as_mut().map_or(FrameRef::EMPTY, |host| host.frame());
        state.frame.version as f64
    })
}

#[no_mangle]
pub extern "C" fn ngc_frame_ptr() -> *const u8 {
    with_state(|state| state.frame.ptr)
}

#[no_mangle]
pub extern "C" fn ngc_frame_len() -> u32 {
    with_state(|state| state.frame.len as u32)
}

#[no_mangle]
pub extern "C" fn ngc_frame_width() -> u32 {
    with_state(|state| state.frame.width)
}

#[no_mangle]
pub extern "C" fn ngc_frame_height() -> u32 {
    with_state(|state| state.frame.height)
}

fn set_parts(state: &mut State, parts: Vec<Part>) -> u32 {
    state.parts = parts;
    state.parts.len() as u32
}

/// Storage files that changed since the last call; returns their number (0: nothing to save).
#[no_mangle]
pub extern "C" fn ngc_session_profile_changes() -> u32 {
    with_state(|state| {
        let parts = state.host.as_mut().map(|host| host.take_profile_changes()).unwrap_or_default();
        set_parts(state, parts)
    })
}

/// The complete profile including a fresh RTC checkpoint; returns the number of files.
#[no_mangle]
pub extern "C" fn ngc_session_profile_export() -> u32 {
    with_state(|state| {
        let parts = state.host.as_mut().map(|host| host.export_profile()).unwrap_or_default();
        set_parts(state, parts)
    })
}

/// Evidence capture (`state.json`, `lcd.png`, `can-trace.tsv`): the files go into the parts list (the number of
/// files is returned), the capture's name is available through `ngc_capture_name`.
#[no_mangle]
pub extern "C" fn ngc_session_capture() -> u32 {
    with_state(|state| {
        let Some(host) = state.host.as_mut() else { return set_parts(state, Vec::new()) };
        let (name, parts) = host.capture();
        state.capture_name = name;
        set_parts(state, parts)
    })
}

/// Writes the name of the last capture (a timestamp-like identifier) into the output buffer and returns its length.
#[no_mangle]
pub extern "C" fn ngc_capture_name() -> u32 {
    with_state(|state| {
        let name = state.capture_name.clone();
        text_result(state, name)
    })
}

/// Closes the session with the runner's shutdown semantics (the RTC checkpoint is saved) and returns the
/// number of profile files in the parts list.
#[no_mangle]
pub extern "C" fn ngc_session_shutdown() -> u32 {
    with_state(|state| {
        state.frame = FrameRef::EMPTY;
        let parts = match state.host.take() {
            Some(host) => host.shutdown(),
            None => Vec::new(),
        };
        set_parts(state, parts)
    })
}

/// Number of files in the parts list.
#[no_mangle]
pub extern "C" fn ngc_part_count() -> u32 {
    with_state(|state| state.parts.len() as u32)
}

/// Writes the name of part `index` into the output buffer and returns its length (0 for a bad index).
#[no_mangle]
pub extern "C" fn ngc_part_name(index: u32) -> u32 {
    with_state(|state| {
        let name = state.parts.get(index as usize).map(|part| part.name.clone()).unwrap_or_default();
        text_result(state, name)
    })
}

#[no_mangle]
pub extern "C" fn ngc_part_ptr(index: u32) -> *const u8 {
    with_state(|state| state.parts.get(index as usize).map_or(std::ptr::null(), |part| part.data.as_ptr()))
}

#[no_mangle]
pub extern "C" fn ngc_part_len(index: u32) -> u32 {
    with_state(|state| state.parts.get(index as usize).map_or(0, |part| part.data.len() as u32))
}

/// Releases the parts list.
#[no_mangle]
pub extern "C" fn ngc_parts_clear() {
    with_state(|state| state.parts = Vec::new());
}

// ---------------------------------------------------------------------------------------------------
// benchmark path (the bare system; independent of the session above)

/// Creates the system from the kept firmware. `mode`: 0 dual, 1 handset only. `flags`: bit 0 simultaneous
/// start, bit 1 cold boot, bit 2 disable the idle fast-forward, bit 3 turn the main I2C idle-high fixture OFF (it
/// is on by default). `adc_sample`: handset ADC board-ID sample (400 is the platform default; values above 4095
/// are rejected). Both images must come from the same firmware release.
#[no_mangle]
pub extern "C" fn ngc_create(mode: u32, flags: u32, adc_sample: u32) -> i32 {
    with_state(|state| {
        state.system = None;
        let mode = if mode == 0 { Mode::Dual } else { Mode::HandsetOnly };
        if adc_sample > 4095 {
            return fail(state, "adc_sample must be in 0..=4095");
        }
        let Some(handset) = state.handset.as_ref() else {
            return fail(state, "handset firmware: not provided");
        };
        let main = if mode == Mode::Dual {
            match state.main.as_ref() {
                Some(firmware) => Some(firmware),
                None => return fail(state, "main firmware: not provided"),
            }
        } else {
            None
        };
        let config = SystemConfig {
            mode,
            boot_mode: if flags & 2 != 0 { BootMode::Cold } else { BootMode::HandsetWake },
            simultaneous_start: flags & 1 != 0,
            idle_fast_forward: flags & 4 == 0,
            adc_sample,
            ..SystemConfig::default()
        };
        let options = BuildOptions { main_i2c_idle_high: flags & 8 == 0 };
        match System::new_with(config, main, handset, options) {
            Ok(system) => {
                state.system = Some(system);
                0
            }
            Err(error) => fail(state, error),
        }
    })
}

/// Drops the benchmark system (the firmware copies stay).
#[no_mangle]
pub extern "C" fn ngc_destroy() {
    with_state(|state| state.system = None);
}

/// Switches the exact idle fast-forward of both cores (results are identical either way; only host speed
/// differs).
#[no_mangle]
pub extern "C" fn ngc_set_idle_fast_forward(enabled: u32) -> i32 {
    with_state(|state| match state.system.as_mut() {
        Some(system) => {
            system.set_idle_fast_forward(enabled != 0);
            0
        }
        None => fail(state, "no system"),
    })
}

/// The viewer's Restart (`mode` 0, the configured boot mode), Cold (1) and Wake (2): the boards are recreated,
/// EEPROM and NOR survive in memory.
#[no_mangle]
pub extern "C" fn ngc_restart(mode: u32) -> i32 {
    with_state(|state| {
        let boot = match mode {
            0 => None,
            1 => Some(BootMode::Cold),
            _ => Some(BootMode::HandsetWake),
        };
        match state.system.as_mut() {
            Some(system) => match system.restart(boot) {
                Ok(()) => 0,
                Err(error) => fail(state, error),
            },
            None => fail(state, "no system"),
        }
    })
}

/// Runs `seconds` of virtual time. Returns 0, 1 when the system cannot run (standby or error stop: read
/// `ngc_status`), or 2 when no system exists.
#[no_mangle]
pub extern "C" fn ngc_run_for(seconds: f64) -> i32 {
    with_state(|state| match state.system.as_mut() {
        Some(system) => {
            system.run_for_secs(seconds);
            i32::from(!system.can_run())
        }
        None => 2,
    })
}

/// Virtual time in nanoseconds.
#[no_mangle]
pub extern "C" fn ngc_time_ns() -> u64 {
    with_state(|state| state.system.as_ref().map_or(0, |system| system.time()))
}

/// Executed instructions of a board (0 main, 1 handset).
#[no_mangle]
pub extern "C" fn ngc_instructions(board: u32) -> u64 {
    with_state(|state| state.system.as_ref().and_then(|system| system.instructions(which(board))).unwrap_or(0))
}

/// `ngc_instructions` as a double (exact below 2^53, which is 2.8 million virtual years).
#[no_mangle]
pub extern "C" fn ngc_instructions_f64(board: u32) -> f64 {
    ngc_instructions(board) as f64
}

/// Instructions the exact idle fast-forward skipped on a board (as a double).
#[no_mangle]
pub extern "C" fn ngc_idle_skipped(board: u32) -> f64 {
    with_state(|state| {
        state.system.as_ref().and_then(|system| system.counters(which(board))).map_or(0.0, |c| c.fast_forward.skipped_instructions as f64)
    })
}

/// Chunks (`cpu.run` calls) executed by a board.
#[no_mangle]
pub extern "C" fn ngc_slices(board: u32) -> f64 {
    with_state(|state| state.system.as_ref().and_then(|system| system.counters(which(board))).map_or(0.0, |c| c.slices as f64))
}

/// Writes the status JSON (virtual time, PCs, instruction counts, CAN/ADC/storage summaries, ...) into the
/// output buffer and returns its length (0 without a system).
#[no_mangle]
pub extern "C" fn ngc_status() -> u32 {
    with_state(|state| {
        let text = match state.system.as_ref() {
            Some(system) => {
                let mut json = system.status_json();
                json.insert("engine", ENGINE);
                json.to_string_with(&emu_core::json::WriteOptions::compact())
            }
            None => String::new(),
        };
        text_result(state, text)
    })
}

/// Writes the state digest (hex SHA-256) into the output buffer and returns its length.
#[no_mangle]
pub extern "C" fn ngc_fingerprint() -> u32 {
    with_state(|state| {
        let text = state.system.as_ref().map(System::fingerprint).unwrap_or_default();
        text_result(state, text)
    })
}

fn apply(input: Input) -> i32 {
    with_state(|state| match state.system.as_mut() {
        Some(system) => match system.apply_input(&input) {
            Ok(()) => 0,
            Err(error) => fail(state, error),
        },
        None => fail(state, "no system"),
    })
}

/// Physical navigation button: `up` non-zero for Up, zero for Down (a 204.8 ms low pulse).
#[no_mangle]
pub extern "C" fn ngc_navigate(up: u32) -> i32 {
    apply(Input::Navigate { up: up != 0 })
}

/// Physical confirm (two overlapping pulses staggered by 50 virtual ms).
#[no_mangle]
pub extern "C" fn ngc_confirm() -> i32 {
    apply(Input::Confirm)
}

/// `buttons Press mask`: 1 = PE3, 2 = PE5, 3 = both exactly simultaneously.
#[no_mangle]
pub extern "C" fn ngc_press(mask: u32) -> i32 {
    apply(Input::Press { mask })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_alloc_round_trip() {
        assert!(ngc_version() > 0);
        let ptr = ngc_alloc(64);
        assert!(!ptr.is_null());
        unsafe { ngc_free(ptr, 64) };
        assert_eq!(ngc_run_for(0.1), 2, "no system yet");
        assert_eq!(ngc_status(), 0);
        assert_eq!(ngc_session_run_for(0.1), 2, "no session yet");
        assert_eq!(ngc_session_state(), 0);
        assert_eq!(ngc_part_count(), 0);
    }

    #[test]
    fn config_parsing() {
        let config = HostConfig::from_json(r#"{"mode":"handset","bootMode":"cold","adcSample":12,"startPaused":true}"#).unwrap();
        assert!(!config.dual && config.cold && config.start_paused && config.adc_sample == 12);
        assert!(HostConfig::from_json(r#"{"adcSample":5000}"#).is_err());
        assert!(HostConfig::from_json(r#"{"bogus":1}"#).is_err());
        assert!(HostConfig::from_json("").unwrap().dual);
    }

    #[test]
    fn inspect_reports_garbage_without_failing() {
        let data = b"not an s-record file";
        let length = unsafe { ngc_firmware_inspect(data.as_ptr(), data.len()) };
        assert!(length > 0);
        with_state(|state| {
            let text = String::from_utf8(state.output.clone()).unwrap();
            assert!(text.contains("\"ok\":false"), "{text}");
            assert!(text.contains("\"release\":null"), "an unknown file has no release: {text}");
        });
    }

    #[test]
    fn the_parity_options_of_the_session_configuration() {
        let defaults = HostConfig::from_json("{}").unwrap();
        assert!(defaults.i2c_idle_high, "the I2C idle-high fixture is on by default");
        assert_eq!(defaults.history_nonce, 0);
        let config = HostConfig::from_json(r#"{"i2cIdleHigh":false,"historyNonce":9007199254740991}"#).unwrap();
        assert!(!config.i2c_idle_high && config.history_nonce == 9_007_199_254_740_991);
        let config = HostConfig::from_json(r#"{"historyNonce":18446744073709551615}"#).unwrap();
        assert_eq!(config.history_nonce, u64::MAX);
        for bad in [r#"{"historyNonce":-1}"#, r#"{"historyNonce":"7"}"#, r#"{"historyNonce":1.5}"#, r#"{"i2cIdleHigh":1}"#, r#"{"i2cIdleHigh":"no"}"#] {
            assert!(HostConfig::from_json(bad).is_err(), "{bad}");
        }
    }

    // ---- with the real firmware (skipped when the gitignored SREC files are not available) ----

    fn srec(release: &firmware::Release, role: Role) -> Option<Vec<u8>> {
        // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
        let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(std::path::PathBuf::from), Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
        let name = release.expected(role).file_name;
        roots.into_iter().flatten().find_map(|root| std::fs::read(root.join(release.id).join(name)).ok())
    }

    fn output_text() -> String {
        with_state(|state| String::from_utf8(state.output.clone()).unwrap())
    }

    fn error_text() -> String {
        with_state(|state| state.error.clone())
    }

    fn create(config: &str) -> i32 {
        unsafe { ngc_session_create(config.as_ptr(), config.len()) }
    }

    #[test]
    fn inspect_and_set_firmware_name_the_release_and_creation_refuses_a_mixed_pair() {
        let (Some(triton_main), Some(triton_handset), Some(neptun_main), Some(neptun_handset)) =
            (srec(&firmware::TRITON, Role::Main), srec(&firmware::TRITON, Role::Handset), srec(&firmware::NEPTUN, Role::Main), srec(&firmware::NEPTUN, Role::Handset))
        else {
            eprintln!("skipping: both firmware releases are needed");
            return;
        };
        for (bytes, id, label, role) in [
            (&triton_main, "TRITON-5.8-65.3", "TRITON main 5.8 / handset 65.3", "main"),
            (&neptun_handset, "NEPTUN-5.8-65.3", "NEPTUN main 5.8 / handset 65.3", "handset"),
        ] {
            unsafe { ngc_firmware_inspect(bytes.as_ptr(), bytes.len()) };
            let text = output_text();
            assert!(text.contains(&format!("\"role\":\"{role}\",\"release\":{{\"id\":\"{id}\",\"label\":\"{label}\"}}")), "{text}");
            assert!(text.contains("\"ok\":true") && text.contains("\"message\":null"), "{text}");
        }
        // Either release is accepted per role; the wrong role is still refused with the release named.
        unsafe {
            assert_eq!(ngc_set_firmware(0, neptun_main.as_ptr(), neptun_main.len()), 0);
            assert_eq!(ngc_set_firmware(1, triton_handset.as_ptr(), triton_handset.len()), 0);
        }
        assert_eq!(create(r#"{"mode":"dual"}"#), 1);
        let message = error_text();
        assert!(message.contains("Mixed firmware releases") && message.contains("NEPTUN-5.8-65.3") && message.contains("TRITON-5.8-65.3"), "{message}");
        assert_eq!(ngc_session_active(), 0);
        unsafe {
            assert_eq!(ngc_set_firmware(1, triton_main.as_ptr(), triton_main.len()), 1, "a main image in the handset slot");
        }
        assert!(error_text().contains("this is the main firmware (TRITON-5.8-65.3 main 5.8) but the handset image is required"), "{}", error_text());
        // A matching NEPTUN pair creates a session whose state names the release, the nonce and the fixture.
        unsafe { assert_eq!(ngc_set_firmware(1, neptun_handset.as_ptr(), neptun_handset.len()), 0) };
        assert_eq!(create(r#"{"historyNonce":77,"i2cIdleHigh":false,"startPaused":true}"#), 0, "{}", error_text());
        assert_eq!(ngc_session_active(), 1);
        let length = ngc_session_state();
        assert!(length > 0);
        let state = emu_core::Json::parse(&output_text()).unwrap();
        assert_eq!(state.get("outputHistoryEpoch").and_then(emu_core::Json::as_str), Some("77-1"));
        assert_eq!(state.get("i2cIdleHigh"), Some(&emu_core::Json::Bool(false)));
        assert_eq!(state.get("firmware").and_then(|f| f.get("release")).and_then(|r| r.get("id")).and_then(emu_core::Json::as_str), Some("NEPTUN-5.8-65.3"));
        // A cold boot request is refused for NEPTUN with the reason.
        assert_eq!(create(r#"{"bootMode":"cold"}"#), 1);
        assert!(error_text().contains("cold-boot fixture"), "{}", error_text());
        assert_eq!(ngc_session_active(), 1, "the failed creation left the existing session untouched");
        ngc_session_destroy();
    }
}
