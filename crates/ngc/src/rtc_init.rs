//! The clock of a new profile (DESIGN.md section 23).
//!
//! The engine never reads a clock. A host that wants a new profile to carry the current date and time passes it at session creation
//! ([`SessionConfig::initial_local_time`](crate::session::SessionConfig::initial_local_time): the browser's local date and time, the
//! CLI's `--initial-local-time YYYY-MM-DDTHH:MM:SS`). For every board whose RTC has **no saved checkpoint and no EEPROM date seed**
//! (the legacy seed of [`crate::persistence::eeprom_seed`] wins for the main board), the calendar of the freshly created RTC is
//! replaced by that time, in the 24-hour format with the correct ISO weekday, as if the device had been set at the factory. The
//! prescaler and the backup words stay those of the live RTC, and the board's provenance becomes `host-local-time`
//! ([`Provenance::HostLocalTime`]), which round-trips through `rtc-state.json`.
//!
//! A labeled fixture, not a physical observation, and never a repair: an existing checkpoint is never changed, a second launch of
//! the same session (Restart, Cold, Wake, a serial change) finds the checkpoint saved by the first one and does nothing. Afterwards
//! the calendar advances in virtual time only, as before. The recorded scenarios and the dive benchmark pin it off
//! ([`crate::scenario`], `recorded_config`). The state names it (`rtcInit`).

use crate::persistence::{self, Provenance, RtcBoard};
use emu_core::Json;
use stm32::rtc::{DateTime, RtcCheckpoint};

/// The years a host-supplied local time may have: the RTC calendar holds two BCD year digits.
pub const MIN_YEAR: u32 = 2000;
pub const MAX_YEAR: u32 = 2099;

/// A local date and time of the host, whole seconds, no time zone (the browser's wall clock; the device has none either).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

const FIELDS: [&str; 6] = ["year", "month", "day", "hour", "minute", "second"];

impl LocalTime {
    /// A validated local time: the year 2000 to 2099 and a real calendar date and time of day (no leap second).
    pub fn new(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Result<LocalTime, String> {
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return Err(format!("the year must be {MIN_YEAR} to {MAX_YEAR}"));
        }
        if !(1..=12).contains(&month) {
            return Err("the month must be 1 to 12".to_string());
        }
        if DateTime::new(year as i32, month, day, 0, 0, 0).is_none() {
            return Err(format!("{year:04}-{month:02}-{day:02} is not a real calendar date"));
        }
        if hour > 23 {
            return Err("the hour must be 0 to 23".to_string());
        }
        if minute > 59 {
            return Err("the minute must be 0 to 59".to_string());
        }
        if second > 59 {
            return Err("the second must be 0 to 59".to_string());
        }
        Ok(LocalTime { year, month, day, hour, minute, second })
    }

    /// `YYYY-MM-DDTHH:MM:SS` (the `--initial-local-time` form).
    pub fn parse(text: &str) -> Result<LocalTime, String> {
        let shape = || format!("expected YYYY-MM-DDTHH:MM:SS, got '{text}'");
        let bytes = text.as_bytes();
        let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
        if bytes.len() != 19 || separators.iter().any(|&(at, byte)| bytes[at] != byte) {
            return Err(shape());
        }
        let number = |from: usize, to: usize| -> Result<u32, String> {
            let digits = &text[from..to];
            if digits.bytes().all(|b| b.is_ascii_digit()) {
                digits.parse().map_err(|_| shape())
            } else {
                Err(shape())
            }
        };
        LocalTime::new(number(0, 4)?, number(5, 7)?, number(8, 10)?, number(11, 13)?, number(14, 16)?, number(17, 19)?)
    }

    /// The session-create object `{year, month, day, hour, minute, second}`: exactly those six keys, whole numbers.
    pub fn from_json(json: &Json) -> Result<LocalTime, String> {
        let shape = || "expected an object {year, month, day, hour, minute, second} of whole numbers".to_string();
        let members = json.as_object().ok_or_else(shape)?;
        if members.len() != FIELDS.len() || members.iter().any(|(key, _)| !FIELDS.contains(&key.as_str())) {
            return Err(shape());
        }
        let mut values = [0u32; 6];
        for (slot, name) in values.iter_mut().zip(FIELDS) {
            *slot = match json.get(name) {
                Some(Json::Int(n)) if (0..=i64::from(u32::MAX)).contains(n) => *n as u32,
                Some(Json::UInt(n)) if *n <= u64::from(u32::MAX) => *n as u32,
                _ => return Err(format!("{name} must be a whole number")),
            };
        }
        LocalTime::new(values[0], values[1], values[2], values[3], values[4], values[5])
    }

    /// `YYYY-MM-DDTHH:MM:SS`.
    pub fn text(&self) -> String {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// ISO weekday: 1 = Monday ... 7 = Sunday (the RTC `WDU` field).
    pub fn iso_weekday(&self) -> u32 {
        match DateTime::new(self.year as i32, self.month, self.day, self.hour, self.minute, self.second).map_or(0, |date| date.day_of_week()) {
            0 => 7,
            n => n,
        }
    }
}

impl std::fmt::Display for LocalTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

/// The calendar of a board from the host's local time: the live RTC's prescaler and backup words, the 24-hour format, the time and
/// date registers in BCD with the ISO weekday, validated like every other checkpoint. The provenance is `host-local-time`.
pub fn seed(local: &LocalTime, live: &RtcCheckpoint) -> Result<RtcBoard, String> {
    let bcd = persistence::encode_bcd;
    let mut checkpoint = live.clone();
    checkpoint.format_12_hour = false;
    checkpoint.time_register = bcd(local.hour) << 16 | bcd(local.minute) << 8 | bcd(local.second);
    checkpoint.date_register = bcd(local.year - MIN_YEAR) << 16 | local.iso_weekday() << 13 | bcd(local.month) << 8 | bcd(local.day);
    persistence::validate_checkpoint(&checkpoint, "the host local time")?;
    Ok(RtcBoard { checkpoint, provenance: Provenance::HostLocalTime })
}

/// What the host's local time did for this session (`rtcInit` of the state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcInit {
    /// This session started at least one board's calendar from the host's local time.
    pub applied: bool,
    /// Why it was or was not applied.
    pub reason: String,
    /// The local time the host supplied (`None`: the host supplied none), applied or not.
    pub local_time: Option<LocalTime>,
    /// The boards whose calendar was started from it (`ngc-main`, `ngc-handset`).
    pub boards: Vec<String>,
}

impl RtcInit {
    /// The host supplied no local time (the default: the CLI without the flag, the recorded scenarios, most tests).
    pub fn not_supplied() -> RtcInit {
        RtcInit {
            applied: false,
            reason: "Not applied: the host supplied no local time (the page sends the browser's clock; the CLI takes --initial-local-time).".to_string(),
            local_time: None,
            boards: Vec::new(),
        }
    }

    /// The outcome of one board creation: `seeded` are the boards that got the host's time, `kept` the ones that already had a
    /// saved checkpoint or the EEPROM date seed.
    pub fn outcome(local: &LocalTime, seeded: &[&str], kept: &[&str]) -> RtcInit {
        if seeded.is_empty() {
            return RtcInit {
                applied: false,
                reason: "Not applied: every board already had a saved RTC checkpoint or the EEPROM date seed, and an existing calendar is never changed.".to_string(),
                local_time: Some(*local),
                boards: Vec::new(),
            };
        }
        let mut reason = format!(
            "This session started the calendar of {} from the host's local time {local} (a labeled fixture, as if the device had been set at the factory; the engine never reads a clock).",
            seeded.join(" and ")
        );
        if !kept.is_empty() {
            reason.push_str(&format!(" {} kept the saved checkpoint or the EEPROM date seed.", kept.join(" and ")));
        }
        RtcInit { applied: true, reason, local_time: Some(*local), boards: seeded.iter().map(|name| name.to_string()).collect() }
    }

    /// `{"applied", "reason", "localTime", "boards"}`; `localTime` is `YYYY-MM-DDTHH:MM:SS` or null.
    pub fn to_json(&self) -> Json {
        Json::object()
            .with("applied", self.applied)
            .with("reason", self.reason.as_str())
            .with("localTime", self.local_time.map(|local| local.text()))
            .with("boards", Json::from_items(self.boards.iter().map(String::as_str)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{RtcState, BOARD_HANDSET, BOARD_MAIN};
    use stm32::rtc::BACKUP_WORDS;

    fn live() -> RtcCheckpoint {
        let mut backup = [0u32; BACKUP_WORDS];
        for (index, word) in backup.iter_mut().enumerate() {
            *word = 0x7000_0000 + index as u32;
        }
        // The fresh RTC after reset: 2020-01-01 00:00:00, weekday field 1, the default prescaler.
        RtcCheckpoint { time_register: 0, date_register: 0x20_2101, prescaler_register: 0x007F_00FF, format_12_hour: false, backup_registers: backup }
    }

    #[test]
    fn the_seed_is_a_24_hour_bcd_calendar_with_the_right_weekday() {
        // 2026-10-10 is a Saturday (ISO weekday 6); 14:03:22 is afternoon, so the 24-hour format matters.
        let local = LocalTime::new(2026, 10, 10, 14, 3, 22).unwrap();
        let board = seed(&local, &live()).unwrap();
        assert_eq!(board.provenance, Provenance::HostLocalTime);
        assert!(!board.checkpoint.format_12_hour);
        assert_eq!(board.checkpoint.time_register, 0x14_0322);
        assert_eq!(board.checkpoint.date_register, 0x26_0000 | 6 << 13 | 0x1000 | 0x10);
        // The prescaler and the backup words of the live RTC are kept.
        assert_eq!(board.checkpoint.prescaler_register, live().prescaler_register);
        assert_eq!(board.checkpoint.backup_registers, live().backup_registers);
        // Sunday is 7 (not 0), Monday is 1: 2026-10-04 and 2026-10-05.
        let sunday = seed(&LocalTime::new(2026, 10, 4, 0, 0, 0).unwrap(), &live()).unwrap();
        assert_eq!(sunday.checkpoint.date_register >> 13 & 7, 7);
        let monday = seed(&LocalTime::new(2026, 10, 5, 23, 59, 59).unwrap(), &live()).unwrap();
        assert_eq!((monday.checkpoint.date_register >> 13 & 7, monday.checkpoint.time_register), (1, 0x23_5959));
        // The ends of the range: 2000-01-01 (Saturday) and 2099-12-31 (Thursday), and a leap day.
        let first = seed(&LocalTime::new(2000, 1, 1, 0, 0, 0).unwrap(), &live()).unwrap();
        assert_eq!(first.checkpoint.date_register, 6 << 13 | 0x0100 | 0x01);
        let last = seed(&LocalTime::new(2099, 12, 31, 23, 59, 59).unwrap(), &live()).unwrap();
        assert_eq!(last.checkpoint.date_register, 0x99_0000 | 4 << 13 | 0x1200 | 0x31);
        let leap = seed(&LocalTime::new(2028, 2, 29, 12, 0, 0).unwrap(), &live()).unwrap();
        assert_eq!(leap.checkpoint.date_register, 0x28_0000 | 2 << 13 | 0x0200 | 0x29, "2028-02-29 is a Tuesday");
        // Every seed validates as a checkpoint (the strict rtc-state.json rules).
        for local in [local, LocalTime::new(2026, 10, 10, 0, 0, 0).unwrap(), LocalTime::new(2026, 10, 10, 12, 0, 0).unwrap()] {
            assert!(seed(&local, &live()).unwrap().checkpoint.validate().is_ok());
        }
    }

    #[test]
    fn invalid_dates_and_times_are_refused() {
        for (args, expected) in [
            ((1999, 12, 31, 0, 0, 0), "the year must be 2000 to 2099"),
            ((2100, 1, 1, 0, 0, 0), "the year must be 2000 to 2099"),
            ((2026, 0, 1, 0, 0, 0), "the month must be 1 to 12"),
            ((2026, 13, 1, 0, 0, 0), "the month must be 1 to 12"),
            ((2026, 2, 29, 0, 0, 0), "2026-02-29 is not a real calendar date"),
            ((2026, 4, 31, 0, 0, 0), "2026-04-31 is not a real calendar date"),
            ((2026, 1, 0, 0, 0, 0), "2026-01-00 is not a real calendar date"),
            ((2026, 1, 1, 24, 0, 0), "the hour must be 0 to 23"),
            ((2026, 1, 1, 0, 60, 0), "the minute must be 0 to 59"),
            ((2026, 1, 1, 0, 0, 60), "the second must be 0 to 59"),
        ] {
            let (y, mo, d, h, mi, s) = args;
            assert_eq!(LocalTime::new(y, mo, d, h, mi, s).unwrap_err(), expected, "{args:?}");
        }
        assert!(LocalTime::new(2024, 2, 29, 0, 0, 0).is_ok(), "2024 is a leap year");
        assert!(LocalTime::new(2100, 2, 29, 0, 0, 0).is_err());
    }

    #[test]
    fn the_text_and_json_forms_round_trip_and_refuse_garbage() {
        let local = LocalTime::parse("2026-10-10T14:03:22").unwrap();
        assert_eq!((local.year, local.month, local.day, local.hour, local.minute, local.second), (2026, 10, 10, 14, 3, 22));
        assert_eq!(local.text(), "2026-10-10T14:03:22");
        assert_eq!(LocalTime::parse(&local.text()).unwrap(), local);
        for bad in ["", "2026-10-10", "2026-10-10 14:03:22", "2026-10-10T14:03", "2026-1-1T1:2:3", "2026-10-10T14:03:2x", "+026-10-10T14:03:22", "2026-02-30T00:00:00", "1999-12-31T00:00:00", "2026-10-10T24:00:00"] {
            assert!(LocalTime::parse(bad).is_err(), "{bad:?}");
        }
        let json = Json::parse(r#"{"year":2026,"month":10,"day":10,"hour":14,"minute":3,"second":22}"#).unwrap();
        assert_eq!(LocalTime::from_json(&json).unwrap(), local);
        for bad in [
            r#"{"year":2026,"month":10,"day":10,"hour":14,"minute":3}"#,
            r#"{"year":2026,"month":10,"day":10,"hour":14,"minute":3,"second":22,"ms":5}"#,
            r#"{"year":2026.5,"month":10,"day":10,"hour":14,"minute":3,"second":22}"#,
            r#"{"year":"2026","month":10,"day":10,"hour":14,"minute":3,"second":22}"#,
            r#"{"year":-1,"month":10,"day":10,"hour":14,"minute":3,"second":22}"#,
            r#"{"year":2026,"month":2,"day":30,"hour":14,"minute":3,"second":22}"#,
            r#"[2026,10,10,14,3,22]"#,
            r#""2026-10-10T14:03:22""#,
        ] {
            assert!(LocalTime::from_json(&Json::parse(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_host_local_time_provenance_round_trips_through_rtc_state_json() {
        let local = LocalTime::new(2026, 10, 10, 14, 3, 22).unwrap();
        let mut state = RtcState::empty();
        state.set_board(BOARD_MAIN, seed(&local, &live()).unwrap());
        state.set_board(BOARD_HANDSET, seed(&local, &live()).unwrap());
        let text = state.to_file_text();
        assert!(text.contains("      \"provenance\": {\n        \"source\": \"host-local-time\"\n      }"), "{text}");
        let parsed = RtcState::parse(&text, "rtc-state.json").unwrap();
        assert_eq!(parsed.board(BOARD_MAIN).unwrap().provenance, Provenance::HostLocalTime);
        assert_eq!(parsed.board(BOARD_HANDSET).unwrap().checkpoint.time_register, 0x14_0322);
        assert_eq!(parsed.to_file_text(), text);
        // The provenance carries nothing else: an extra field is refused like for the other plain sources.
        let extra = text.replacen("\"source\": \"host-local-time\"", "\"source\": \"host-local-time\", \"packedBackup\": 1", 1);
        assert!(RtcState::parse(&extra, "rtc-state.json").unwrap_err().contains("Invalid RTC migration provenance for ngc-main"));
        // A capture keeps the provenance of a board that was seeded when no explicit provenance is given.
        let again = parsed.capture(&[(BOARD_MAIN, live())], &[]).unwrap();
        assert_eq!(again.board(BOARD_MAIN).unwrap().provenance, Provenance::HostLocalTime);
    }

    #[test]
    fn an_existing_checkpoint_and_the_eeprom_seed_win_over_the_host_time() {
        // `plan_restore` plans the saved checkpoint, or (main only) the EEPROM seed; the host time only takes the boards it leaves out.
        let local = LocalTime::new(2026, 10, 10, 14, 3, 22).unwrap();
        let mut saved = RtcState::empty();
        let mut checkpoint = live();
        checkpoint.time_register = 0x01_0203;
        saved.set_board(BOARD_MAIN, RtcBoard { checkpoint: checkpoint.clone(), provenance: Provenance::RtcRegisters });
        let names = [BOARD_MAIN, BOARD_HANDSET];
        let plans = persistence::plan_restore(&saved, &names, true, None, None);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].1.checkpoint, checkpoint, "the saved main checkpoint is planned unchanged");
        let missing: Vec<&str> = names.iter().copied().filter(|name| !plans.iter().any(|(n, _)| n == name)).collect();
        assert_eq!(missing, [BOARD_HANDSET], "only the handset is left for the host time");
        let init = RtcInit::outcome(&local, &missing, &[BOARD_MAIN]);
        assert!(init.applied && init.boards == [BOARD_HANDSET] && init.reason.contains("ngc-handset") && init.reason.contains("ngc-main kept"), "{init:?}");
        // Nothing left: not applied, the supplied value is still reported.
        let none = RtcInit::outcome(&local, &[], &names);
        assert!(!none.applied && none.local_time == Some(local) && none.reason.starts_with("Not applied: every board"), "{none:?}");
        assert_eq!(none.to_json().get("localTime").and_then(Json::as_str), Some("2026-10-10T14:03:22"));
        let unsupplied = RtcInit::not_supplied();
        assert!(!unsupplied.applied && unsupplied.to_json().get("localTime").is_some_and(Json::is_null));
        assert_eq!(unsupplied.to_json().len(), 4);
    }
}
