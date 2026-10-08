//! File-backed runner profiles for `ngc-cli run --data-dir DIR`.
//!
//! The directory holds the same five files as the Python runner's data directory (`emulation/runtime/dual/`):
//! `eeprom.bin`, `nor.ngc`, `rtc-state.json`, `inputs.json` and `led-colors.json`. The engine itself does no file I/O
//! (`ngc::session::Profile` is bytes and text in memory); this module reads the files that exist before the boot and
//! writes the ones that changed after the run, each replaced atomically (temporary file in the same directory, then
//! rename), like the runner's `rtc-state.json` replacement.

use ngc::persistence::{Profile, EEPROM_FILE, INPUTS_FILE, LED_COLORS_FILE, NOR_FILE, RTC_STATE_FILE};
use std::path::Path;

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match read_optional(path)? {
        Some(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| format!("{} is not valid UTF-8", path.display())),
        None => Ok(None),
    }
}

/// Reads the profile files that exist in `dir` (a missing directory is an empty profile).
pub fn read_profile(dir: &Path) -> Result<Profile, String> {
    Ok(Profile {
        eeprom: read_optional(&dir.join(EEPROM_FILE))?,
        nor: read_optional(&dir.join(NOR_FILE))?,
        rtc_state: read_text(&dir.join(RTC_STATE_FILE))?,
        inputs: read_text(&dir.join(INPUTS_FILE))?,
        led_colors: read_text(&dir.join(LED_COLORS_FILE))?,
    })
}

/// Replaces `path` with `bytes`: written to a temporary sibling first, then renamed over the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("{} has no file name", path.display()))?;
    let temporary = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&temporary, bytes).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace {}: {e}", path.display())
    })
}

/// Writes the items of `profile` that are present and differ from what `dir` holds. Returns the names written.
pub fn write_profile(dir: &Path, profile: &Profile) -> Result<Vec<&'static str>, String> {
    let items: [(&'static str, Option<&[u8]>); 5] = [
        (EEPROM_FILE, profile.eeprom.as_deref()),
        (NOR_FILE, profile.nor.as_deref()),
        (RTC_STATE_FILE, profile.rtc_state.as_deref().map(str::as_bytes)),
        (INPUTS_FILE, profile.inputs.as_deref().map(str::as_bytes)),
        (LED_COLORS_FILE, profile.led_colors.as_deref().map(str::as_bytes)),
    ];
    let mut written = Vec::new();
    for (name, bytes) in items {
        let Some(bytes) = bytes else { continue };
        let path = dir.join(name);
        if read_optional(&path)?.as_deref() == Some(bytes) {
            continue;
        }
        write_atomic(&path, bytes)?;
        written.push(name);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ngc-cli-profile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_missing_directory_is_an_empty_profile() {
        let dir = temp_dir("missing");
        assert!(read_profile(&dir).unwrap().is_empty());
    }

    #[test]
    fn profiles_round_trip_and_only_changed_files_are_rewritten() {
        let dir = temp_dir("roundtrip");
        let profile = Profile {
            eeprom: Some(vec![0xA3; 2048]),
            nor: Some(b"NGC-NOR".to_vec()),
            rtc_state: Some("{\n  \"version\": 1\n}\n".to_string()),
            inputs: None,
            led_colors: Some("{}\n".to_string()),
        };
        let written = write_profile(&dir, &profile).unwrap();
        assert_eq!(written, ["eeprom.bin", "nor.ngc", "rtc-state.json", "led-colors.json"]);
        assert!(!dir.join("inputs.json").exists());
        assert!(!dir.join("rtc-state.json.tmp").exists());
        assert_eq!(read_profile(&dir).unwrap(), profile);
        // Nothing changed: nothing is written. One item changed: only that one.
        assert!(write_profile(&dir, &profile).unwrap().is_empty());
        let mut changed = profile.clone();
        changed.eeprom.as_mut().unwrap()[7] = 0;
        assert_eq!(write_profile(&dir, &changed).unwrap(), ["eeprom.bin"]);
        assert_eq!(read_profile(&dir).unwrap(), changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_in_a_json_item_is_reported() {
        let dir = temp_dir("utf8");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inputs.json"), [0xFF, 0xFE]).unwrap();
        let error = read_profile(&dir).unwrap_err();
        assert!(error.contains("inputs.json") && error.contains("UTF-8"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
