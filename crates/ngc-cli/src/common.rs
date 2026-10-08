//! Helpers shared by the `run`, `bench` and `scenario` commands.

use emu_core::Json;
use ngc::firmware::{self, Firmware, Role};
use ngc::system::{BootMode, BuildOptions, Mode, RoutineAccelMode, System, SystemConfig, Which};
use std::path::{Path, PathBuf};

/// The TRITON firmware directory (`firmware/TRITON-5.8-65.3`), when it exists.
#[cfg(test)]
pub fn default_firmware_dir() -> Option<PathBuf> {
    release_firmware_dir(&firmware::TRITON)
}

/// The local directory of a release (both SRECs present): `<release id>` below the directory named by the
/// `NGC_FIRMWARE_DIR` environment variable, else below the repository's `firmware/` directory, which is found from the
/// crate location. The firmware is never part of the repository; supply it yourself.
pub fn release_firmware_dir(release: &firmware::Release) -> Option<PathBuf> {
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    roots
        .into_iter()
        .flatten()
        .map(|root| root.join(release.id))
        .find(|dir| dir.join(release.main.file_name).is_file() && dir.join(release.handset.file_name).is_file())
}

/// `--release <id>`: the release whose local firmware directory supplies the default SREC paths (TRITON when absent).
pub fn parse_release(text: Option<&str>) -> Result<&'static firmware::Release, String> {
    match text {
        None => Ok(&firmware::TRITON),
        Some(id) => firmware::Release::by_id(id).ok_or_else(|| {
            format!("--release must be one of {} (got '{id}')", firmware::RELEASES.iter().map(|r| r.id).collect::<Vec<_>>().join(", "))
        }),
    }
}

/// Default output directory of `scenario` (`target/scenarios`, ignored by git).
pub fn default_scenario_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scenarios")
}

/// Resolves the SREC path of a role: the explicit option, else the repository copy of `release` (TRITON by default).
pub fn firmware_path(explicit: Option<&str>, role: Role) -> Result<PathBuf, String> {
    firmware_path_in(explicit, role, &firmware::TRITON)
}

/// [`firmware_path`] for a chosen release.
pub fn firmware_path_in(explicit: Option<&str>, role: Role, release: &firmware::Release) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(PathBuf::from(path));
    }
    release_firmware_dir(release)
        .map(|dir| dir.join(release.expected(role).file_name))
        .ok_or_else(|| format!("no --{role} SREC given and the firmware directory firmware/{} was not found (put the SREC files there, or set NGC_FIRMWARE_DIR)", release.id))
}

/// Reads, identifies and verifies one SREC.
pub fn load_firmware(path: &Path, role: Role) -> Result<Firmware, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    firmware::load(&bytes, Some(role)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Both firmware images for a mode (`main` only for the dual system) with the default paths of a chosen release
/// (`--release`, TRITON by default). The two images must be of the same release.
pub fn load_images_in(main: Option<&str>, handset: Option<&str>, mode: Mode, release: &firmware::Release) -> Result<(Option<Firmware>, Firmware), String> {
    let handset = load_firmware(&firmware_path_in(handset, Role::Handset, release)?, Role::Handset)?;
    let main = if mode == Mode::Dual { Some(load_firmware(&firmware_path_in(main, Role::Main, release)?, Role::Main)?) } else { None };
    if let Some(main) = &main {
        firmware::common_release(main, &handset)?;
    }
    Ok((main, handset))
}

pub fn parse_mode(text: Option<&str>) -> Result<Mode, String> {
    match text.unwrap_or("dual") {
        "dual" => Ok(Mode::Dual),
        "handset" => Ok(Mode::HandsetOnly),
        other => Err(format!("--mode must be dual or handset (got '{other}')")),
    }
}

pub fn parse_boot_mode(text: Option<&str>) -> Result<BootMode, String> {
    let text = text.unwrap_or("handset-wake");
    BootMode::parse(text).ok_or_else(|| format!("--boot-mode must be handset-wake or cold (got '{text}')"))
}

pub fn parse_which(text: Option<&str>, default: Which) -> Result<Which, String> {
    match text {
        None => Ok(default),
        Some(text) => Which::parse(text).ok_or_else(|| format!("--board must be main or handset (got '{text}')")),
    }
}

pub fn parse_f64(name: &str, text: Option<&str>, default: f64) -> Result<f64, String> {
    match text {
        None => Ok(default),
        Some(text) => text.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| format!("--{name} must be a number (got '{text}')")),
    }
}

pub fn parse_u64(name: &str, text: Option<&str>) -> Result<Option<u64>, String> {
    match text {
        None => Ok(None),
        Some(text) => text.replace('_', "").parse::<u64>().map(Some).map_err(|_| format!("--{name} must be a non-negative integer (got '{text}')")),
    }
}

/// A 32-bit address in hexadecimal, with or without the `0x` prefix.
pub fn parse_address(text: &str) -> Result<u32, String> {
    let digits = text.trim().trim_start_matches("0x").trim_start_matches("0X").replace('_', "");
    u32::from_str_radix(&digits, 16).map_err(|_| format!("'{text}' is not a hexadecimal address"))
}

/// Parses a scripted input list: entries separated by `;`, each `<seconds> <action> [arguments]`, e.g.
/// `5.0 down; 5.6 up; 6.0 confirm; 7.0 set pressure1Mbar=1500 oxygen1Mv=12; 8.0 can-drop 0x154; 9 can-connect false`.
/// Actions: `up`, `down`, `confirm`, `press MASK`, `pulse MASK MICROSECONDS`, `can-connect true|false`,
/// `can-drop ID` (-1 to forward everything), `set key=value ...` (the viewer's sensor controls).
pub fn parse_inputs(text: &str) -> Result<Vec<(emu_core::Time, ngc::system::Input)>, String> {
    use ngc::system::Input;
    let number = |token: &str| -> Result<i64, String> {
        let token = token.trim();
        if let Some(hex) = token.strip_prefix("0x") {
            i64::from_str_radix(hex, 16).map_err(|_| format!("'{token}' is not a number"))
        } else {
            token.parse::<i64>().map_err(|_| format!("'{token}' is not a number"))
        }
    };
    let mut out = Vec::new();
    for entry in text.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let mut words = entry.split_whitespace();
        let time = words.next().ok_or("empty input entry")?;
        let seconds: f64 = time.parse().map_err(|_| format!("'{time}' is not a time in seconds (entry '{entry}')"))?;
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(format!("negative or invalid time in '{entry}'"));
        }
        let action = words.next().ok_or_else(|| format!("missing action in '{entry}'"))?;
        let rest: Vec<&str> = words.collect();
        let input = match (action, rest.as_slice()) {
            ("up", []) => Input::Navigate { up: true },
            ("down", []) => Input::Navigate { up: false },
            ("confirm", []) => Input::Confirm,
            ("press", [mask]) => Input::Press { mask: number(mask)? as u32 },
            ("pulse", [mask, us]) => Input::Pulse { mask: number(mask)? as u32, duration_us: number(us)? as u32 },
            ("can-connect", [value]) => Input::CanConnected(match *value {
                "true" => true,
                "false" => false,
                other => return Err(format!("can-connect takes true or false (got '{other}')")),
            }),
            ("can-drop", [id]) => Input::CanDropId(number(id)? as i32),
            ("set", pairs) if !pairs.is_empty() => {
                let mut object = Json::object();
                for pair in pairs {
                    let (key, value) = pair.split_once('=').ok_or_else(|| format!("'{pair}' is not key=value"))?;
                    let json = match value {
                        "true" => Json::Bool(true),
                        "false" => Json::Bool(false),
                        other => Json::Float(other.parse::<f64>().map_err(|_| format!("'{other}' is not a number"))?),
                    };
                    object.insert(key, json);
                }
                Input::Sensors(object)
            }
            _ => return Err(format!("cannot understand input '{entry}'")),
        };
        out.push((emu_core::from_secs_f64(seconds), input));
    }
    Ok(out)
}

/// The switches of the exact routine acceleration (DESIGN.md 16.2), shared by `run`, `bench` and `scenario`:
/// `--no-routine-accel` turns the acceleration off, `--shadow-routine-accel` selects the verification mode (every memo
/// hit is replayed and interpreted and compared; slow). Default: on.
pub fn routine_accel_mode(parsed: &crate::args::Args) -> Result<RoutineAccelMode, String> {
    match (parsed.flag("no-routine-accel"), parsed.flag("shadow-routine-accel")) {
        (true, true) => Err("--no-routine-accel and --shadow-routine-accel exclude each other".to_string()),
        (true, false) => Ok(RoutineAccelMode::Off),
        (false, true) => Ok(RoutineAccelMode::Shadow),
        (false, false) => Ok(RoutineAccelMode::On),
    }
}

/// A system for the given options (no instruction has run). `i2c_idle_high` is the main board's I2C idle-high fixture
/// (on unless `--no-i2c-idle-high`).
#[allow(clippy::too_many_arguments)]
pub fn build_system(
    mode: Mode,
    boot_mode: BootMode,
    simultaneous_start: bool,
    idle_ff: bool,
    routine_accel: RoutineAccelMode,
    i2c_idle_high: bool,
    main: Option<&Firmware>,
    handset: &Firmware,
) -> Result<System, String> {
    let config = SystemConfig { mode, boot_mode, simultaneous_start, idle_fast_forward: idle_ff, routine_accel, ..SystemConfig::default() };
    System::new_with(config, main, handset, BuildOptions { main_i2c_idle_high: i2c_idle_high })
}

/// Writes a `Json` value (pretty, trailing newline).
pub fn write_json(path: &Path, value: &Json) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, format!("{}\n", value.to_pretty_string())).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Writes little-endian u32 words.
pub fn write_u32le(path: &Path, words: &[u32]) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Host description for result files.
pub fn host_json() -> Json {
    Json::object()
        .with("os", std::env::consts::OS)
        .with("arch", std::env::consts::ARCH)
        .with("profile", if cfg!(debug_assertions) { "debug" } else { "release" })
        .with("version", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ngc::system::Input;

    #[test]
    fn scripted_inputs_parse() {
        let inputs = parse_inputs("5.0 down; 5.6 up; 6 confirm; 7 set pressure1Mbar=1500 pressureMaximumTiming=true; 8 can-drop 0x154; 9 can-connect false; 10 press 3; 11 pulse 1 150000").unwrap();
        assert_eq!(inputs.len(), 8);
        assert_eq!(inputs[0], (5_000_000_000, Input::Navigate { up: false }));
        assert_eq!(inputs[1], (5_600_000_000, Input::Navigate { up: true }));
        assert_eq!(inputs[2].1, Input::Confirm);
        match &inputs[3].1 {
            Input::Sensors(json) => {
                assert_eq!(json.get("pressure1Mbar").and_then(Json::as_f64), Some(1500.0));
                assert_eq!(json.get("pressureMaximumTiming").and_then(Json::as_bool), Some(true));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(inputs[4].1, Input::CanDropId(0x154));
        assert_eq!(inputs[5].1, Input::CanConnected(false));
        assert_eq!(inputs[6].1, Input::Press { mask: 3 });
        assert_eq!(inputs[7].1, Input::Pulse { mask: 1, duration_us: 150_000 });
        for bad in ["x down", "1", "1 jump", "1 set", "1 set a", "-1 up", "1 can-connect maybe", "1 press"] {
            assert!(parse_inputs(bad).is_err(), "{bad}");
        }
        assert!(parse_inputs("").unwrap().is_empty());
    }

    #[test]
    fn addresses_parse_with_or_without_the_prefix() {
        assert_eq!(parse_address("0xE0001004"), Ok(0xE000_1004));
        assert_eq!(parse_address("e0001004"), Ok(0xE000_1004));
        assert_eq!(parse_address(" 0X4000_1424 "), Ok(0x4000_1424));
        assert!(parse_address("0x1_0000_0000").is_err());
        assert!(parse_address("pc").is_err());
        assert!(parse_address("").is_err());
    }
}
