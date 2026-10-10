//! Byte-compatible persistence formats of the Renode runner profile (`emulation/run_emulator.py`,
//! `emulation/rtc_persistence.py`), as pure byte/string functions: the library never touches a file system.
//!
//! A *profile* is the set of files the runner keeps in its data directory (`--data-dir`):
//!
//! | file | content | format |
//! | --- | --- | --- |
//! | `eeprom.bin` | the 2 048 EEPROM cells | raw bytes |
//! | `nor.ngc` | the sparse QSPI NOR image | `NGCNOR01`, capacity, page count, `{page, 4096 bytes}` records (`NgcQuadSpi::serialize_backing`) |
//! | `rtc-state.json` | per-board RTC calendar, prescaler, hour format and the 20 backup words | `json.dump(indent=2)` plus a newline, version 1 |
//! | `inputs.json` | the viewer's sensor controls (dual runs) | `json.dumps(indent=2)` plus a newline |
//! | `led-colors.json` | the HUD color labels | `json.dumps(indent=2)` plus a newline |
//!
//! The RTC checkpoint code is a port of `emulation/rtc_persistence.py`: the same strict validation (exact field
//! sets, unsigned 32-bit integers that are not booleans, BCD digits, reserved bits, real calendar dates, the
//! 12-hour range of the pinned model, weekday 1..7, 20 backup words, provenance rules, duplicate JSON keys),
//! the same error texts, the same restore planning (a saved checkpoint wins; otherwise the main board may be
//! seeded from the legacy EEPROM calendar `0x2d` when the validity marker `0xA3` is present and the packed date
//! is a real date) and the same save semantics (boards that were not captured keep their saved state; the
//! provenance of a restored board is kept). One engine extension, read only: the provenance `host-local-time` of a calendar that
//! engine builds of 2026-10-10 started from the host's local time for a new profile (removed the same day, DESIGN.md section 23).
//! Such a checkpoint is restored like any other and keeps its provenance; the engine never assigns it to a new board. The Renode
//! runner's own loader would refuse it.
//!
//! Nothing here runs guest code. `session.rs` decides when to call the functions (the runner calls them at
//! launch, Restart/Cold/Wake/serial and close).

use crate::fixtures::Inputs;
use emu_core::json::{Json, ParseOptions, WriteOptions};
use stm32::rtc::{DateTime, RtcCheckpoint, BACKUP_WORDS};

pub const EEPROM_FILE: &str = "eeprom.bin";
pub const NOR_FILE: &str = "nor.ngc";
pub const RTC_STATE_FILE: &str = "rtc-state.json";
pub const INPUTS_FILE: &str = "inputs.json";
pub const LED_COLORS_FILE: &str = "led-colors.json";

pub const RTC_STATE_VERSION: u32 = 1;
pub const RTC_CLOCK_POLICY: &str = "virtual-time-only";
pub const RTC_PRECISION: &str = "whole-calendar-seconds";
pub const BOARD_MAIN: &str = "ngc-main";
pub const BOARD_HANDSET: &str = "ngc-handset";

/// Legacy EEPROM calendar checkpoint of the main firmware: marker byte and packed date (logical ID `0x23`).
pub const EEPROM_MARKER_OFFSET: usize = 254;
pub const EEPROM_MARKER_VALUE: u8 = 0xA3;
pub const EEPROM_PACKED_DATE_OFFSET: usize = 0x2D;

/// The files of a runner profile as bytes/text. `None` means "no such file" when read and "unchanged" when
/// returned by `Session::take_profile_changes`. Byte-compatible with the Renode runner's data directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// `eeprom.bin` (2 048 bytes).
    pub eeprom: Option<Vec<u8>>,
    /// `nor.ngc` (`NGCQuadSPI` backing format).
    pub nor: Option<Vec<u8>>,
    /// `rtc-state.json` (`emulation/rtc_persistence.py` format).
    pub rtc_state: Option<String>,
    /// `inputs.json`.
    pub inputs: Option<String>,
    /// `led-colors.json`.
    pub led_colors: Option<String>,
}

impl Profile {
    /// True when no item is present.
    pub fn is_empty(&self) -> bool {
        self.eeprom.is_none() && self.nor.is_none() && self.rtc_state.is_none() && self.inputs.is_none() && self.led_colors.is_none()
    }

    /// Overlays the items of `newer` that are present onto `self`.
    pub fn merge(&mut self, newer: Profile) {
        if newer.eeprom.is_some() {
            self.eeprom = newer.eeprom;
        }
        if newer.nor.is_some() {
            self.nor = newer.nor;
        }
        if newer.rtc_state.is_some() {
            self.rtc_state = newer.rtc_state;
        }
        if newer.inputs.is_some() {
            self.inputs = newer.inputs;
        }
        if newer.led_colors.is_some() {
            self.led_colors = newer.led_colors;
        }
    }

    /// `(file name, bytes)` of every present item, in a fixed order.
    pub fn files(&self) -> Vec<(&'static str, Vec<u8>)> {
        let mut out = Vec::new();
        if let Some(bytes) = &self.eeprom {
            out.push((EEPROM_FILE, bytes.clone()));
        }
        if let Some(bytes) = &self.nor {
            out.push((NOR_FILE, bytes.clone()));
        }
        if let Some(text) = &self.rtc_state {
            out.push((RTC_STATE_FILE, text.clone().into_bytes()));
        }
        if let Some(text) = &self.inputs {
            out.push((INPUTS_FILE, text.clone().into_bytes()));
        }
        if let Some(text) = &self.led_colors {
            out.push((LED_COLORS_FILE, text.clone().into_bytes()));
        }
        out
    }
}

// ---- Python float formatting ----------------------------------------------------------------------------

/// `repr(float)` of CPython (shortest round-trip digits; exponent form below 1e-4 and from 1e16 on, at least two
/// exponent digits), used where the runner writes numbers with `json.dumps`.
pub fn python_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exponent) = sci.split_once('e').expect("scientific notation");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exp10: i32 = exponent.parse().expect("exponent");
    let decpt = exp10 + 1; // value = 0.DIGITS * 10^decpt
    let sign = if x < 0.0 { "-" } else { "" };
    if decpt <= -4 || decpt > 16 {
        let mut text = String::from(sign);
        text.push_str(&digits[..1]);
        if digits.len() > 1 {
            text.push('.');
            text.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        text.push('e');
        text.push(if e < 0 { '-' } else { '+' });
        text.push_str(&format!("{:02}", e.abs()));
        text
    } else if decpt <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-decpt) as usize))
    } else if decpt as usize >= digits.len() {
        format!("{sign}{digits}{}.0", "0".repeat(decpt as usize - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    }
}

// ---- inputs.json ------------------------------------------------------------------------------------------

/// Reads `inputs.json` the way the runner does for a dual run: `INPUT_DEFAULTS.update(json)`, then
/// `apply_inputs` validates every key. Unknown keys, out-of-range values and wrong types are errors.
pub fn parse_inputs_file(text: &str) -> Result<Inputs, String> {
    let json = Json::parse(text).map_err(|e| e.to_string())?;
    let Some(members) = json.as_object() else {
        return Err("inputs.json must contain a JSON object".to_string());
    };
    let mut merged = inputs_to_json_defaults();
    for (key, value) in members {
        merged.insert(key.clone(), value.clone());
    }
    let mut inputs = Inputs::defaults();
    inputs.apply_json(&merged)?;
    Ok(inputs)
}

fn inputs_to_json_defaults() -> Json {
    // The runner merges the file into the default dictionary (keys keep their default order).
    Inputs::defaults().to_json()
}

/// `json.dumps(values, indent=2) + "\n"`: floats in Python's `repr` form, integers and booleans plain.
pub fn inputs_file_text(inputs: &Inputs) -> String {
    let json = inputs.to_json();
    let mut out = String::from("{\n");
    let members = json.as_object().expect("inputs object");
    for (index, (key, value)) in members.iter().enumerate() {
        out.push_str("  \"");
        out.push_str(key);
        out.push_str("\": ");
        match value {
            Json::Float(f) => out.push_str(&python_float_repr(*f)),
            other => other.write_to(&mut out, &WriteOptions::compact()),
        }
        if index + 1 < members.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

// ---- led-colors.json ----------------------------------------------------------------------------------------

/// HUD channel ids of the main board and the color labels the viewer assigns (`LED_IDS`,
/// `LED_DEFAULT_COLORS`).
pub const LED_IDS: [&str; 3] = ["main-hud-1", "main-hud-2", "main-hud-3"];
pub const LED_DEFAULT_COLORS: [(&str, &str); 3] = [("main-hud-1", "unknown"), ("main-hud-2", "white"), ("main-hud-3", "red")];
pub const LED_COLOR_NAMES: [&str; 3] = ["unknown", "red", "white"];

/// The HUD color labels, kept in the runner's dictionary order (defaults first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedColors {
    colors: Vec<(String, String)>,
}

impl Default for LedColors {
    fn default() -> Self {
        Self { colors: LED_DEFAULT_COLORS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }
}

impl LedColors {
    /// Applies `validated_led_colors(json)` to a copy and returns it; `Err` carries the runner's message.
    fn validated(colors: &Json) -> Result<Vec<(String, String)>, String> {
        let error = || "LED colors must map HUD channel IDs to unknown, red or white".to_string();
        let members = colors.as_object().ok_or_else(error)?;
        let mut out = Vec::new();
        for (key, value) in members {
            let color = value.as_str().ok_or_else(error)?;
            if !LED_IDS.contains(&key.as_str()) || !LED_COLOR_NAMES.contains(&color) {
                return Err(error());
            }
            out.push((key.clone(), color.to_string()));
        }
        Ok(out)
    }

    /// `self.led_colors.update(validated_led_colors(colors))`.
    pub fn update(&mut self, colors: &Json) -> Result<(), String> {
        for (key, value) in Self::validated(colors)? {
            match self.colors.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => self.colors.push((key, value)),
            }
        }
        Ok(())
    }

    /// Reads `led-colors.json`.
    pub fn from_file_text(text: &str) -> Result<LedColors, String> {
        let json = Json::parse(text).map_err(|e| e.to_string())?;
        let mut colors = LedColors::default();
        colors.update(&json)?;
        Ok(colors)
    }

    /// The color label of a HUD channel (`unknown` when the channel is not listed).
    pub fn get(&self, id: &str) -> &str {
        self.colors.iter().find(|(k, _)| k == id).map_or("unknown", |(_, v)| v.as_str())
    }

    /// `json.dumps(self.led_colors, indent=2) + "\n"`.
    pub fn file_text(&self) -> String {
        let mut out = String::from("{\n");
        for (index, (key, value)) in self.colors.iter().enumerate() {
            out.push_str(&format!("  \"{key}\": \"{value}\""));
            if index + 1 < self.colors.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("}\n");
        out
    }
}

// ---- rtc-state.json --------------------------------------------------------------------------------------------

/// Where a board's retained calendar came from (`provenance` of the checkpoint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// `{"source": "rtc-registers"}`: captured from the RTC registers.
    RtcRegisters,
    /// `{"source": "fresh-rtc"}`: the new RTC instance's own reset calendar (never written to files by the runner
    /// itself, but accepted when read).
    FreshRtc,
    /// `{"source": "eeprom-packed-date", "packedBackup": n}`: migrated from the main EEPROM calendar checkpoint.
    EepromPackedDate(u32),
    /// `{"source": "host-local-time"}`: started from the host's local time for a new profile by the engine builds of 2026-10-10
    /// (an engine extension of the runner's format, since removed; DESIGN.md section 23). Accepted when read so that those
    /// profiles still load, restored like any other checkpoint and kept on save like every restored provenance; the engine never
    /// assigns it to a new board.
    HostLocalTime,
}

impl Provenance {
    pub fn source(&self) -> &'static str {
        match self {
            Provenance::RtcRegisters => "rtc-registers",
            Provenance::FreshRtc => "fresh-rtc",
            Provenance::EepromPackedDate(_) => "eeprom-packed-date",
            Provenance::HostLocalTime => "host-local-time",
        }
    }

    pub fn to_json(&self) -> Json {
        let mut json = Json::object().with("source", self.source());
        if let Provenance::EepromPackedDate(packed) = self {
            json.insert("packedBackup", u64::from(*packed));
        }
        json
    }

    /// `_provenance(value, label)`.
    fn from_json(value: &Json, label: &str) -> Result<Provenance, String> {
        let Some(members) = value.as_object() else {
            return Err(format!("Invalid RTC provenance for {label}"));
        };
        let source = value.get("source").and_then(Json::as_str);
        if !matches!(source, Some("rtc-registers" | "fresh-rtc" | "eeprom-packed-date" | "host-local-time")) {
            return Err(format!("Invalid RTC provenance for {label}"));
        }
        if members.iter().any(|(k, _)| k != "source" && k != "packedBackup") {
            return Err(format!("Invalid RTC provenance fields for {label}"));
        }
        match source {
            Some("eeprom-packed-date") => {
                let packed = uint32(value.get("packedBackup"), "EEPROM migration backup")?;
                decode_packed_date(packed)?;
                Ok(Provenance::EepromPackedDate(packed))
            }
            Some(other) => {
                if members.iter().any(|(k, _)| k == "packedBackup") {
                    return Err(format!("Invalid RTC migration provenance for {label}"));
                }
                Ok(match other {
                    "fresh-rtc" => Provenance::FreshRtc,
                    "host-local-time" => Provenance::HostLocalTime,
                    _ => Provenance::RtcRegisters,
                })
            }
            None => unreachable!("checked above"),
        }
    }
}

/// One board of the checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcBoard {
    pub checkpoint: RtcCheckpoint,
    pub provenance: Provenance,
}

/// The parsed `rtc-state.json`: boards in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtcState {
    pub boards: Vec<(String, RtcBoard)>,
}

fn uint32(value: Option<&Json>, label: &str) -> Result<u32, String> {
    let error = || format!("Invalid RTC {label}: expected an unsigned 32-bit integer");
    match value {
        Some(Json::Int(i)) => u32::try_from(*i).map_err(|_| error()),
        Some(Json::UInt(u)) => u32::try_from(*u).map_err(|_| error()),
        _ => Err(error()),
    }
}

fn bcd(value: u32, label: &str) -> Result<u32, String> {
    if (value & 15) > 9 || (value >> 4) > 9 {
        Err(format!("Invalid RTC {label}: invalid BCD digits"))
    } else {
        Ok((value >> 4) * 10 + (value & 15))
    }
}

fn encode_bcd(value: u32) -> u32 {
    (value / 10) << 4 | value % 10
}

/// CPython's `datetime(...)` argument checks and messages.
fn python_datetime(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Result<DateTime, String> {
    if !(1..=9999).contains(&year) {
        return Err(format!("year {year} is out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".to_string());
    }
    if day < 1 || DateTime::new(year as i32, month, day, 0, 0, 0).is_none() {
        return Err("day is out of range for month".to_string());
    }
    if hour > 23 {
        return Err("hour must be in 0..23".to_string());
    }
    if minute > 59 {
        return Err("minute must be in 0..59".to_string());
    }
    if second > 59 {
        return Err("second must be in 0..59".to_string());
    }
    DateTime::new(year as i32, month, day, hour, minute, second).ok_or_else(|| "date value out of range".to_string())
}

/// `_decode_packed_backup`: year offset 5 bits... the layout of the main firmware's packed calendar
/// (`storage-clock-investigation.md`): bits 5:0 year-2000, 9:6 month, 14:10 day, 19:15 hour, 25:20 minute, 31:26 second.
pub fn decode_packed_date(packed: u32) -> Result<DateTime, String> {
    python_datetime(2000 + (packed & 0x3F), (packed >> 6) & 15, (packed >> 10) & 31, (packed >> 15) & 31, (packed >> 20) & 63, (packed >> 26) & 63)
        .map_err(|e| format!("Invalid packed EEPROM date: {e}"))
}

/// `calendar_datetime(board_state)`: validates a register calendar and returns it.
fn calendar_datetime(checkpoint: &RtcCheckpoint) -> Result<DateTime, String> {
    let (tr, dr) = (checkpoint.time_register, checkpoint.date_register);
    if tr & !RtcCheckpoint::TIME_MASK != 0 || dr & !RtcCheckpoint::DATE_MASK != 0 {
        return Err("Invalid RTC calendar: reserved bits are set".to_string());
    }
    let second = bcd(tr & 0x7F, "second")?;
    let minute = bcd((tr >> 8) & 0x7F, "minute")?;
    let mut hour = bcd((tr >> 16) & 0x3F, "hour")?;
    let pm = tr & 0x40_0000 != 0;
    // The pinned model reports 00..11 in AM/PM mode, including midnight/noon.
    if checkpoint.format_12_hour {
        if hour > 11 {
            return Err("Invalid RTC 12-hour calendar for the pinned model".to_string());
        }
        if pm {
            hour += 12;
        }
    } else if pm || hour > 23 {
        return Err("Invalid RTC 24-hour calendar".to_string());
    }
    let day = bcd(dr & 0x3F, "day")?;
    let month = bcd((dr >> 8) & 0x1F, "month")?;
    let year = 2000 + bcd((dr >> 16) & 0xFF, "year")?;
    let weekday = (dr >> 13) & 7;
    if !(1..=7).contains(&weekday) {
        return Err("Invalid RTC weekday: expected 1 through 7".to_string());
    }
    python_datetime(year, month, day, hour, minute, second).map_err(|e| format!("Invalid RTC calendar: {e}"))
}

/// `_validate_board` for an already typed checkpoint (prescaler reserved bits included).
fn validate_checkpoint(checkpoint: &RtcCheckpoint, name: &str) -> Result<(), String> {
    calendar_datetime(checkpoint)?;
    if checkpoint.prescaler_register & !RtcCheckpoint::PRESCALER_MASK != 0 {
        return Err(format!("Invalid RTC prescaler reserved bits for {name}"));
    }
    Ok(())
}

impl RtcBoard {
    fn to_json(&self) -> Json {
        let c = &self.checkpoint;
        Json::object()
            .with("timeRegister", u64::from(c.time_register))
            .with("dateRegister", u64::from(c.date_register))
            .with("prescalerRegister", u64::from(c.prescaler_register))
            .with("format12Hour", c.format_12_hour)
            .with("backupRegisters", Json::from_items(c.backup_registers.iter().map(|w| Json::from(u64::from(*w)))))
            .with("provenance", self.provenance.to_json())
    }

    /// `_validate_board(value, name)`: exact field set, then the calendar, prescaler, backup words and provenance.
    fn from_json(value: &Json, name: &str) -> Result<RtcBoard, String> {
        const FIELDS: [&str; 6] = ["timeRegister", "dateRegister", "prescalerRegister", "format12Hour", "backupRegisters", "provenance"];
        let fields_error = || format!("Invalid RTC checkpoint fields for {name}");
        let members = value.as_object().ok_or_else(fields_error)?;
        if members.len() != FIELDS.len() || FIELDS.iter().any(|f| !members.iter().any(|(k, _)| k == f)) {
            return Err(fields_error());
        }
        let time_register = uint32(value.get("timeRegister"), "time register")?;
        let date_register = uint32(value.get("dateRegister"), "date register")?;
        let format_12_hour = match value.get("format12Hour") {
            Some(Json::Bool(b)) => *b,
            _ => return Err("Invalid RTC format12Hour: expected a boolean".to_string()),
        };
        // Python's calendar_datetime runs before the prescaler is read; keep that order so the first error matches.
        let mut checkpoint = RtcCheckpoint { time_register, date_register, prescaler_register: 0, format_12_hour, backup_registers: [0; BACKUP_WORDS] };
        calendar_datetime(&checkpoint)?;
        let prescaler = uint32(value.get("prescalerRegister"), "prescaler register")?;
        if prescaler & !RtcCheckpoint::PRESCALER_MASK != 0 {
            return Err(format!("Invalid RTC prescaler reserved bits for {name}"));
        }
        checkpoint.prescaler_register = prescaler;
        let backup = value.get("backupRegisters").and_then(Json::as_array);
        let backup = match backup {
            Some(items) if items.len() == BACKUP_WORDS => items,
            _ => return Err(format!("Invalid RTC backup registers for {name}: expected 20 words")),
        };
        for (index, word) in backup.iter().enumerate() {
            checkpoint.backup_registers[index] = uint32(Some(word), &format!("{name} backup register {index}"))?;
        }
        let provenance = Provenance::from_json(value.get("provenance").expect("field present"), name)?;
        Ok(RtcBoard { checkpoint, provenance })
    }
}

impl RtcState {
    /// `_empty_state()`: no board saved.
    pub fn empty() -> RtcState {
        RtcState::default()
    }

    pub fn board(&self, name: &str) -> Option<&RtcBoard> {
        self.boards.iter().find(|(n, _)| n == name).map(|(_, b)| b)
    }

    /// Replaces or appends a board (keeping the order of existing ones).
    pub fn set_board(&mut self, name: &str, board: RtcBoard) {
        match self.boards.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = board,
            None => self.boards.push((name.to_string(), board)),
        }
    }

    /// `load_rtc_state` for a file's text: strict parse and validation. `label` stands for the file path in the
    /// message (`Cannot load RTC checkpoint {label}: {error}`).
    pub fn parse(text: &str, label: &str) -> Result<RtcState, String> {
        Self::parse_inner(text).map_err(|error| format!("Cannot load RTC checkpoint {label}: {error}"))
    }

    fn parse_inner(text: &str) -> Result<RtcState, String> {
        let json = Json::parse_with(text, ParseOptions { reject_duplicate_keys: true }).map_err(|e| {
            if e.message.to_ascii_lowercase().contains("duplicate") {
                // `_unique_object`: `Duplicate RTC checkpoint field: <key>`.
                let key = e.message.split('"').nth(1).unwrap_or("?").to_string();
                format!("Duplicate RTC checkpoint field: {key}")
            } else {
                e.to_string()
            }
        })?;
        Self::from_json(&json)
    }

    /// `_validate_state`.
    pub fn from_json(json: &Json) -> Result<RtcState, String> {
        let members = json.as_object();
        let document_ok = members.is_some_and(|m| {
            m.len() == 4 && ["version", "clockPolicy", "precision", "boards"].iter().all(|f| m.iter().any(|(k, _)| k == f))
        });
        if !document_ok {
            return Err("Invalid RTC checkpoint document".to_string());
        }
        // `type(state["version"]) is not int or != VERSION`: booleans and floats fail.
        match json.get("version") {
            Some(Json::Int(v)) if *v == i64::from(RTC_STATE_VERSION) => {}
            _ => return Err("Unsupported RTC checkpoint version".to_string()),
        }
        if json.get("clockPolicy").and_then(Json::as_str) != Some(RTC_CLOCK_POLICY) || json.get("precision").and_then(Json::as_str) != Some(RTC_PRECISION) {
            return Err("Unsupported RTC checkpoint clock policy or precision".to_string());
        }
        let boards = json.get("boards").and_then(Json::as_object);
        let Some(boards) = boards.filter(|b| b.iter().all(|(name, _)| name == BOARD_MAIN || name == BOARD_HANDSET)) else {
            return Err("Invalid RTC checkpoint board names".to_string());
        };
        let mut state = RtcState::default();
        for (name, value) in boards {
            state.boards.push((name.clone(), RtcBoard::from_json(value, name)?));
        }
        Ok(state)
    }

    pub fn to_json(&self) -> Json {
        let mut boards = Json::object();
        for (name, board) in &self.boards {
            boards.insert(name.clone(), board.to_json());
        }
        Json::object()
            .with("version", u64::from(RTC_STATE_VERSION))
            .with("clockPolicy", RTC_CLOCK_POLICY)
            .with("precision", RTC_PRECISION)
            .with("boards", boards)
    }

    /// The file text: `json.dump(state, output, indent=2)` followed by a newline.
    pub fn to_file_text(&self) -> String {
        let mut text = self.to_json().to_string_with(&WriteOptions::python_indent2());
        text.push('\n');
        text
    }

    /// `save_rtc_state`'s capture step: copies `self` (the saved document, which keeps boards that are not
    /// captured), then for every name captures the live checkpoint with its origin: the explicit provenance of a
    /// restore, else the saved provenance of that board, else `rtc-registers`. The result is validated.
    pub fn capture(&self, live: &[(&str, RtcCheckpoint)], provenance: &[(String, Provenance)]) -> Result<RtcState, String> {
        let mut state = self.clone();
        for (name, checkpoint) in live {
            if *name != BOARD_MAIN && *name != BOARD_HANDSET {
                return Err("RTC board names must uniquely select ngc-main and/or ngc-handset".to_string());
            }
            let origin = provenance
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
                .or_else(|| self.board(name).map(|b| b.provenance.clone()))
                .unwrap_or(Provenance::RtcRegisters);
            validate_checkpoint(checkpoint, name)?;
            state.set_board(name, RtcBoard { checkpoint: checkpoint.clone(), provenance: origin });
        }
        Ok(state)
    }
}

/// `_eeprom_seed`: the main board's calendar from the legacy EEPROM checkpoint. Needs the validity marker
/// `0xA3` at offset 254 and a real packed date at `0x2D..0x31`; `live` is the freshly created RTC (its
/// prescaler and backup words are kept, as the runner captures them before overriding the calendar).
pub fn eeprom_seed(eeprom: &[u8], live: &RtcCheckpoint) -> Option<RtcBoard> {
    if eeprom.get(EEPROM_MARKER_OFFSET).copied() != Some(EEPROM_MARKER_VALUE) {
        return None;
    }
    let bytes = eeprom.get(EEPROM_PACKED_DATE_OFFSET..EEPROM_PACKED_DATE_OFFSET + 4)?;
    let packed = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let date = decode_packed_date(packed).ok()?;
    let mut checkpoint = live.clone();
    checkpoint.format_12_hour = false;
    checkpoint.time_register = encode_bcd(date.hour) << 16 | encode_bcd(date.minute) << 8 | encode_bcd(date.second);
    let weekday = match date.day_of_week() {
        0 => 7,
        n => n,
    };
    checkpoint.date_register = encode_bcd(date.year as u32 - 2000) << 16 | weekday << 13 | encode_bcd(date.month) << 8 | encode_bcd(date.day);
    validate_checkpoint(&checkpoint, BOARD_MAIN).ok()?;
    Some(RtcBoard { checkpoint, provenance: Provenance::EepromPackedDate(packed) })
}

/// `restore_rtc_state`'s planning: for the selected boards, the saved checkpoint, or (main only, when
/// `allow_eeprom_seed`) the EEPROM seed. Returns `(board name, board)` for every board to be restored; boards
/// without a plan stay untouched. `live_main` is the new main RTC's checkpoint (needed by the seed).
pub fn plan_restore(
    saved: &RtcState,
    names: &[&str],
    allow_eeprom_seed: bool,
    eeprom: Option<&[u8]>,
    live_main: Option<&RtcCheckpoint>,
) -> Vec<(String, RtcBoard)> {
    let mut plans = Vec::new();
    for name in names {
        let mut board = saved.board(name).cloned();
        if board.is_none() && *name == BOARD_MAIN && allow_eeprom_seed {
            if let (Some(eeprom), Some(live)) = (eeprom, live_main) {
                board = eeprom_seed(eeprom, live);
            }
        }
        if let Some(board) = board {
            plans.push((name.to_string(), board));
        }
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(seed: u32) -> RtcCheckpoint {
        let mut backup = [0u32; BACKUP_WORDS];
        for (index, word) in backup.iter_mut().enumerate() {
            *word = seed.wrapping_add(index as u32 * 0x10101);
        }
        RtcCheckpoint { time_register: 0x23_5959, date_register: 0x24_8229, prescaler_register: 0x007F_00FF, format_12_hour: false, backup_registers: backup }
    }

    fn state_text() -> String {
        let mut state = RtcState::empty();
        state.set_board(BOARD_MAIN, RtcBoard { checkpoint: checkpoint(0x8100_0000), provenance: Provenance::RtcRegisters });
        let mut handset = checkpoint(0xC100_0000);
        handset.time_register = 0x17_2005;
        handset.date_register = 0x26_7007;
        state.set_board(BOARD_HANDSET, RtcBoard { checkpoint: handset, provenance: Provenance::RtcRegisters });
        state.to_file_text()
    }

    #[test]
    fn python_float_repr_matches_cpython() {
        for (value, text) in [
            (1500.0, "1500.0"),
            (1013.25, "1013.25"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (20.0, "20.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.2345678901234567e22, "1.2345678901234568e+22"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (123456789.125, "123456789.125"),
            (4200.0, "4200.0"),
            (0.30000000000000004, "0.30000000000000004"),
        ] {
            assert_eq!(python_float_repr(value), text, "{value:?}");
        }
    }

    #[test]
    fn inputs_file_round_trips_in_the_runner_layout() {
        let inputs = Inputs::defaults();
        let text = inputs_file_text(&inputs);
        assert_eq!(
            text,
            "{\n  \"battery1Mv\": 4100.0,\n  \"battery2Mv\": 4100.0,\n  \"oxygen1Mv\": 10.0,\n  \"oxygen2Mv\": 10.0,\n  \"oxygen3Mv\": 10.0,\n  \
             \"pressure1Mbar\": 1013.25,\n  \"pressure2Mbar\": 1013.25,\n  \"temperature1C\": 20.0,\n  \"temperature2C\": 20.0,\n  \
             \"acquisitionEnabled\": true,\n  \"acquisitionDelayUs\": 0,\n  \"noiseAmplitudeRaw\": 0,\n  \"noiseSeed\": 1,\n  \"pressureMaximumTiming\": false\n}\n"
        );
        assert_eq!(parse_inputs_file(&text).unwrap(), inputs);
        // The runner merges a partial file into the defaults; unknown keys are rejected by apply_inputs.
        let partial = parse_inputs_file("{\"battery1Mv\": 1450, \"pressureMaximumTiming\": true}").unwrap();
        assert_eq!(partial.battery_mv, [1450.0, 4100.0]);
        assert!(partial.pressure_maximum_timing);
        assert!(parse_inputs_file("{\"bogus\": 1}").unwrap_err().contains("Unknown input: bogus"));
        assert!(parse_inputs_file("{\"battery1Mv\": 5000}").unwrap_err().contains("battery1Mv must be between 0 and 4200"));
        assert!(parse_inputs_file("[1]").is_err());
    }

    #[test]
    fn led_colors_follow_the_runner_dictionary() {
        let mut colors = LedColors::default();
        assert_eq!(colors.get("main-hud-1"), "unknown");
        assert_eq!(colors.get("main-hud-2"), "white");
        assert_eq!(colors.get("main-hud-3"), "red");
        colors.update(&Json::parse("{\"main-hud-1\": \"white\"}").unwrap()).unwrap();
        assert_eq!(colors.file_text(), "{\n  \"main-hud-1\": \"white\",\n  \"main-hud-2\": \"white\",\n  \"main-hud-3\": \"red\"\n}\n");
        for bad in ["{\"main-hud-9\": \"red\"}", "{\"main-hud-1\": \"blue\"}", "[1]", "{\"main-hud-1\": 5}"] {
            let error = colors.update(&Json::parse(bad).unwrap()).unwrap_err();
            assert_eq!(error, "LED colors must map HUD channel IDs to unknown, red or white", "{bad}");
        }
        assert_eq!(LedColors::from_file_text(&colors.file_text()).unwrap(), colors);
        // An empty object is valid (and changes nothing).
        let mut copy = colors.clone();
        copy.update(&Json::parse("{}").unwrap()).unwrap();
        assert_eq!(copy, colors);
    }

    #[test]
    fn rtc_state_text_matches_the_python_layout_and_round_trips() {
        let text = state_text();
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"clockPolicy\": \"virtual-time-only\",\n  \"precision\": \"whole-calendar-seconds\",\n  \"boards\": {\n    \"ngc-main\": {\n      \"timeRegister\": 2316633,\n"));
        assert!(text.contains("      \"backupRegisters\": [\n        2164260864,\n"));
        assert!(text.contains("      \"provenance\": {\n        \"source\": \"rtc-registers\"\n      }\n    },\n    \"ngc-handset\": {"));
        assert!(text.ends_with("    }\n  }\n}\n"));
        let parsed = RtcState::parse(&text, "rtc-state.json").unwrap();
        assert_eq!(parsed.to_file_text(), text);
        assert_eq!(parsed.boards.len(), 2);
        assert_eq!(parsed.board(BOARD_MAIN).unwrap().checkpoint.time_register, 0x23_5959);
    }

    #[test]
    fn a_real_runner_checkpoint_round_trips_byte_for_byte() {
        // Written by `rtc_persistence.save_rtc_state` of the Renode runner of the analysis workspace (clock/storage probe of
        // 2026-10-07); committed as `testdata/renode-runner-rtc-state.json`.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/renode-runner-rtc-state.json");
        let text = std::fs::read_to_string(path).expect("testdata/renode-runner-rtc-state.json");
        let parsed = RtcState::parse(&text, path).unwrap();
        assert_eq!(parsed.to_file_text(), text, "the writer must reproduce the runner's bytes");
        assert_eq!(parsed.board(BOARD_MAIN).unwrap().checkpoint.time_register, 1_193_221);
    }

    #[test]
    fn strict_validation_rejects_what_the_python_code_rejects() {
        let valid = state_text();
        let load = |text: &str| RtcState::parse(text, "rtc-state.json");
        let bad = |from: &str, to: &str| load(&valid.replacen(from, to, 1)).unwrap_err();
        assert!(bad("\"version\": 1", "\"version\": 2").contains("Unsupported RTC checkpoint version"));
        assert!(bad("\"version\": 1", "\"version\": 1.0").contains("Unsupported RTC checkpoint version"));
        assert!(bad("\"version\": 1", "\"version\": true").contains("Unsupported RTC checkpoint version"));
        assert!(bad("virtual-time-only", "wall-clock").contains("Unsupported RTC checkpoint clock policy or precision"));
        assert!(bad("\"ngc-main\"", "\"ngc-other\"").contains("Invalid RTC checkpoint board names"));
        assert!(bad("\"timeRegister\": 2316633", "\"timeRegister\": 2316634").contains("invalid BCD digits"), "0x235959+1 has BCD digit 10");
        assert!(bad("\"timeRegister\": 2316633", "\"timeRegister\": 1.5").contains("Invalid RTC time register: expected an unsigned 32-bit integer"));
        assert!(bad("\"timeRegister\": 2316633", "\"timeRegister\": true").contains("expected an unsigned 32-bit integer"));
        assert!(bad("\"format12Hour\": false", "\"format12Hour\": 1").contains("Invalid RTC format12Hour: expected a boolean"));
        assert!(bad("\"prescalerRegister\": 8323327", "\"prescalerRegister\": 2147483648").contains("Invalid RTC prescaler reserved bits for ngc-main"));
        assert!(bad("\"prescalerRegister\": 8323327", "\"prescalerRegister\": 4294967296").contains("expected an unsigned 32-bit integer"));
        assert!(bad("\"source\": \"rtc-registers\"", "\"source\": \"somewhere\"").contains("Invalid RTC provenance for ngc-main"));
        assert!(bad("\"source\": \"rtc-registers\"", "\"source\": \"fresh-rtc\", \"packedBackup\": 1").contains("Invalid RTC migration provenance for ngc-main"));
        assert!(bad("\"source\": \"rtc-registers\"", "\"source\": \"rtc-registers\", \"x\": 1").contains("Invalid RTC provenance fields for ngc-main"));
        // A missing board field; the date registers of the Python unit tests: weekday zero (0x240229) and
        // February 29 of a non-leap year (0x238229).
        assert!(bad("\"timeRegister\": 2316633,", "").contains("Invalid RTC checkpoint fields for ngc-main"));
        let date = format!("\"dateRegister\": {}", 0x24_8229u32);
        assert!(bad(&date, &format!("\"dateRegister\": {}", 0x24_0229u32)).contains("Invalid RTC weekday: expected 1 through 7"));
        assert!(bad(&date, &format!("\"dateRegister\": {}", 0x23_8229u32)).contains("Invalid RTC calendar: day is out of range for month"));
        assert!(bad(&date, &format!("\"dateRegister\": {}", 0x24_8229u32 | 1 << 24)).contains("reserved bits"));
        // The 12-hour format needs hours 00..11 in this pinned model.
        assert!(bad("\"format12Hour\": false", "\"format12Hour\": true").contains("Invalid RTC 12-hour calendar for the pinned model"));
        // Duplicate keys are refused before any validation.
        let duplicated = valid.replacen("\"version\": 1", "\"version\": 1, \"version\": 1", 1);
        assert!(load(&duplicated).unwrap_err().contains("Duplicate RTC checkpoint field: version"), "{}", load(&duplicated).unwrap_err());
        assert!(load("not json").unwrap_err().starts_with("Cannot load RTC checkpoint rtc-state.json: "));
        assert!(load("[]").unwrap_err().contains("Invalid RTC checkpoint document"));
    }

    #[test]
    fn capture_keeps_unselected_boards_and_the_provenance_of_restored_ones() {
        let saved = RtcState::parse(&state_text(), "x").unwrap();
        let mut main = checkpoint(0x1111);
        main.time_register = 0x01_0203;
        let packed = 24 | (2 << 6) | (29 << 10) | (13 << 15) | (45 << 20) | (59 << 26);
        // Saving only the handset keeps the saved main board bit for bit.
        let handset_only = saved.capture(&[(BOARD_HANDSET, checkpoint(0x2222))], &[]).unwrap();
        assert_eq!(handset_only.board(BOARD_MAIN), saved.board(BOARD_MAIN));
        assert_eq!(handset_only.board(BOARD_HANDSET).unwrap().checkpoint, checkpoint(0x2222));
        // An explicit provenance (a migrated board) is kept; without one the saved provenance stays.
        let migrated = saved.capture(&[(BOARD_MAIN, main.clone())], &[(BOARD_MAIN.to_string(), Provenance::EepromPackedDate(packed))]).unwrap();
        assert_eq!(migrated.board(BOARD_MAIN).unwrap().provenance, Provenance::EepromPackedDate(packed));
        let again = migrated.capture(&[(BOARD_MAIN, main)], &[]).unwrap();
        assert_eq!(again.board(BOARD_MAIN).unwrap().provenance, Provenance::EepromPackedDate(packed));
        // A fresh state gains boards in capture order.
        let fresh = RtcState::empty().capture(&[(BOARD_MAIN, checkpoint(1)), (BOARD_HANDSET, checkpoint(2))], &[]).unwrap();
        assert_eq!(fresh.boards.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), [BOARD_MAIN, BOARD_HANDSET]);
        assert_eq!(fresh.board(BOARD_MAIN).unwrap().provenance, Provenance::RtcRegisters);
        // An invalid live calendar is refused.
        let mut broken = checkpoint(3);
        broken.date_register = 0x238229;
        assert!(RtcState::empty().capture(&[(BOARD_MAIN, broken)], &[]).is_err());
    }

    #[test]
    fn the_host_local_time_provenance_of_older_profiles_is_still_read_and_kept() {
        // The engine builds of 2026-10-10 saved new profiles with this provenance (DESIGN.md 23; the feature is removed). Such a file
        // parses strictly like any other, round-trips byte for byte, and a capture keeps the provenance of the restored board.
        let mut state = RtcState::parse(&state_text(), "x").unwrap();
        for (_, board) in &mut state.boards {
            board.provenance = Provenance::HostLocalTime;
        }
        let text = state.to_file_text();
        assert!(text.contains("      \"provenance\": {\n        \"source\": \"host-local-time\"\n      }"), "{text}");
        let parsed = RtcState::parse(&text, "rtc-state.json").unwrap();
        assert_eq!(parsed.board(BOARD_MAIN).unwrap().provenance, Provenance::HostLocalTime);
        assert_eq!(parsed.board(BOARD_HANDSET).unwrap().provenance, Provenance::HostLocalTime);
        assert_eq!(parsed.to_file_text(), text);
        let saved_again = parsed.capture(&[(BOARD_MAIN, checkpoint(0x3333))], &[]).unwrap();
        assert_eq!(saved_again.board(BOARD_MAIN).unwrap().provenance, Provenance::HostLocalTime);
        // It carries nothing else, like the other plain sources, and an invalid calendar under it is refused as always.
        let extra = text.replacen("\"source\": \"host-local-time\"", "\"source\": \"host-local-time\", \"packedBackup\": 1", 1);
        assert_eq!(RtcState::parse(&extra, "rtc-state.json").unwrap_err(), "Cannot load RTC checkpoint rtc-state.json: Invalid RTC migration provenance for ngc-main");
        let bad_date = RtcState::parse(&text.replacen(&format!("\"dateRegister\": {}", 0x24_8229), &format!("\"dateRegister\": {}", 0x23_8229), 1), "rtc-state.json");
        assert!(bad_date.is_err(), "{bad_date:?}");
    }

    fn eeprom_with_date(packed: u32, marker: u8) -> Vec<u8> {
        let mut eeprom = vec![0xFF; 2048];
        eeprom[EEPROM_MARKER_OFFSET] = marker;
        eeprom[EEPROM_PACKED_DATE_OFFSET..EEPROM_PACKED_DATE_OFFSET + 4].copy_from_slice(&packed.to_le_bytes());
        eeprom
    }

    #[test]
    fn the_legacy_eeprom_seed_needs_the_marker_and_a_real_date() {
        let live = checkpoint(0x5000);
        let packed = 24 | (2 << 6) | (29 << 10) | (13 << 15) | (45 << 20) | (59 << 26);
        let seed = eeprom_seed(&eeprom_with_date(packed, 0xA3), &live).expect("valid seed");
        assert_eq!(seed.provenance, Provenance::EepromPackedDate(packed));
        // 2024-02-29 13:45:59 is a Thursday (ISO weekday 4); prescaler and backup words come from the live RTC.
        assert_eq!(seed.checkpoint.time_register, 0x13_4559);
        assert_eq!(seed.checkpoint.date_register, 0x24_0000 | 4 << 13 | 0x0200 | 0x29);
        assert_eq!(seed.checkpoint.prescaler_register, live.prescaler_register);
        assert_eq!(seed.checkpoint.backup_registers, live.backup_registers);
        assert!(!seed.checkpoint.format_12_hour);
        // Zero, erased and unmarked dates do not seed.
        let valid = 26 | (10 << 6) | (7 << 10) | (17 << 15) | (20 << 20) | (5 << 26);
        for (packed, marker) in [(0, 0xA3), (0xFFFF_FFFF, 0xA3), (valid, 0xFF)] {
            assert!(eeprom_seed(&eeprom_with_date(packed, marker), &live).is_none(), "{packed:#x} {marker:#x}");
        }
        // 2026-10-07 17:20:05 is a Wednesday.
        let wednesday = eeprom_seed(&eeprom_with_date(valid, 0xA3), &live).unwrap();
        assert_eq!(wednesday.checkpoint.date_register, 0x26_0000 | 3 << 13 | 0x1000 | 0x07);
        // Sunday gets ISO weekday 7: 2026-10-04.
        let sunday = 26 | (10 << 6) | (4 << 10);
        assert_eq!(eeprom_seed(&eeprom_with_date(sunday, 0xA3), &live).unwrap().checkpoint.date_register >> 13 & 7, 7);
    }

    #[test]
    fn restore_planning_prefers_the_saved_checkpoint() {
        let saved = RtcState::parse(&state_text(), "x").unwrap();
        let live = checkpoint(0x5000);
        let packed = 26 | (10 << 6) | (7 << 10) | (17 << 15) | (20 << 20) | (5 << 26);
        let eeprom = eeprom_with_date(packed, 0xA3);
        let both = [BOARD_MAIN, BOARD_HANDSET];
        // A saved main calendar takes precedence over the EEPROM backup.
        let plans = plan_restore(&saved, &both, true, Some(&eeprom), Some(&live));
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].1.checkpoint.time_register, 0x23_5959);
        // No saved checkpoint: main migrates, the handset stays untouched.
        let plans = plan_restore(&RtcState::empty(), &both, true, Some(&eeprom), Some(&live));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, BOARD_MAIN);
        assert_eq!(plans[0].1.provenance, Provenance::EepromPackedDate(packed));
        // Handset-only runs and runs that forbid the seed restore nothing from the EEPROM.
        assert!(plan_restore(&RtcState::empty(), &[BOARD_HANDSET], true, Some(&eeprom), Some(&live)).is_empty());
        assert!(plan_restore(&RtcState::empty(), &both, false, Some(&eeprom), Some(&live)).is_empty());
    }

    #[test]
    fn profile_helpers() {
        let mut profile = Profile::default();
        assert!(profile.is_empty());
        profile.merge(Profile { eeprom: Some(vec![1, 2, 3]), ..Profile::default() });
        profile.merge(Profile { inputs: Some("x".to_string()), ..Profile::default() });
        profile.merge(Profile { eeprom: Some(vec![9]), ..Profile::default() });
        assert_eq!(profile.eeprom, Some(vec![9]));
        assert_eq!(profile.files().iter().map(|(n, _)| *n).collect::<Vec<_>>(), [EEPROM_FILE, INPUTS_FILE]);
        assert!(!profile.is_empty());
    }
}
