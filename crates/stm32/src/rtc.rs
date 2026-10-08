// Ported from Renode 1.17.0 src/Emulator/Peripherals/Peripherals/Timers/STM32F4_RTC.cs
// (MIT License, Copyright (c) Antmicro).

//! `Timers.STM32F4_RTC`: the real-time clock of both boards (`wakeupTimerFrequency` 32768 on the main board,
//! 32000 on the handset), with Renode's quirks.
//!
//! * The calendar is a `DateTime` that starts at **2020-01-01 00:00:00** (weekday Monday) and advances by one
//!   second each time the `ticker` (a descending `LimitTimer` at `wakeupTimerFrequency / (PREDIV_A + 1)` with
//!   limit `PREDIV_S + 1`) reaches its limit. The ticker (and the sub-second `fastTicker`) are **disabled
//!   until the INIT handshake** (`ISR.INIT` 1 -> 0) runs; with the 32000 Hz handset clock and the default
//!   prescalers one "second" lasts 1.024 s of virtual time. Virtual time only: nothing follows the host clock.
//! * Write protection: `WPR` takes `0xCA` then `0x53` to unlock; any other key locks again. Time/date writes
//!   additionally need INIT mode; alarm writes need the alarm to be disabled. Rejected writes log warnings.
//! * `CR.BYPSHAD` always reads 1 ("Shadow registers are not supported" is logged when it is written 0). `RSF`
//!   reads 0 the first time and 1 afterwards until it is cleared (the Zephyr driver quirk).
//! * `ISR` flags are write-zero-to-clear against the *stale* value the previous read provided (a flag that was
//!   never seen set by a read cannot be cleared by a write), `INIT` acts only when the written bit differs from
//!   the previously written one. Both follow from Renode's register framework and are kept.
//! * The wakeup timer sets `WUTF` only when `CR.WUTIE` is set (`EventEnabled`); `WUTR` reads back `WUT + 1`.
//! * 12-hour format: the model stores the hour in 24-hour form and re-folds it on every tick according to the
//!   PM flag last written, so it only ever shows 00..11 unless firmware writes PM (as the Python checkpoint
//!   code documents).
//! * The 20 backup registers (`0x50..=0x9C`) are scratch words that survive [`Peripheral::reset`].
//! * Deviation: Renode throws on invalid BCD digits or impossible dates written to `TR`/`DR`; here an error is
//!   logged and the write ignored.
//!
//! # Checkpoint
//!
//! [`Rtc::checkpoint`] exports what `emulation/rtc_persistence.py` keeps (TR, DR, PRER, CR.FMT, the 20 backup
//! words, all read without side effects) and [`Rtc::restore_checkpoint`] writes it back through the same
//! protected register sequence the Python code uses: `WPR` unlock, `INIT`, `PRER`, `CR`, `DR`, `TR`, leave
//! `INIT`, lock, backup words. The calendar then restarts from a whole-second boundary at the time of the call.
//!
//! Lines: output 0 is `AlarmIRQ`, output 1 `WakeupIRQ` (neither is connected in the NGC platforms).

use emu_core::{
    impl_peripheral_any, AccessPolicy, ClockRead, Ctx, Direction, LimitTimer, LimitTimerConfig, LogLevel, Peripheral, Time,
    View, Width,
};

use crate::iwdg::{tag, warn_tags, Tag};

pub const SIZE: u32 = 0x400;

pub const ALARM_IRQ_LINE: u32 = 0;
pub const WAKEUP_IRQ_LINE: u32 = 1;

/// Register offsets.
pub mod reg {
    pub const TR: u32 = 0x00;
    pub const DR: u32 = 0x04;
    pub const CR: u32 = 0x08;
    pub const ISR: u32 = 0x0C;
    pub const PRER: u32 = 0x10;
    pub const WUTR: u32 = 0x14;
    pub const CALIBR: u32 = 0x18;
    pub const ALRMAR: u32 = 0x1C;
    pub const ALRMBR: u32 = 0x20;
    pub const WPR: u32 = 0x24;
    pub const SSR: u32 = 0x28;
    pub const SHIFTR: u32 = 0x2C;
    pub const TSTR: u32 = 0x30;
    pub const TSDR: u32 = 0x34;
    pub const TSSSR: u32 = 0x38;
    pub const CALR: u32 = 0x3C;
    pub const TAFCR: u32 = 0x40;
    pub const ALRMASSR: u32 = 0x44;
    pub const ALRMBSSR: u32 = 0x48;
    pub const OR: u32 = 0x4C;
    pub const BKP0R: u32 = 0x50;
    pub const BKP19R: u32 = 0x9C;
}

/// Number of implemented backup registers.
pub const BACKUP_WORDS: usize = 20;

const UNLOCK_KEY1: u32 = 0xCA;
const UNLOCK_KEY2: u32 = 0x53;
const DEFAULT_SYNCHRONOUS_PRESCALER: u32 = 0xFF;
const DEFAULT_ASYNCHRONOUS_PRESCALER: u32 = 0x7F;
const ISR_RESET: u32 = 0x7;

const TICKER: u64 = 1;
const FAST_TICKER: u64 = 2;
const WAKEUP_TIMER: u64 = 3;

/// `ISR` bits that are filled in by value providers on every read.
const ISR_PROVIDED: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x40 | 0x100 | 0x200 | 0x1_0000;
const ISR_RSF: u32 = 1 << 5;
const ISR_INIT: u32 = 1 << 7;
const ISR_WUTF: u32 = 1 << 10;
const ISR_FLAGS_11_15: u32 = 0xF800;
const ISR_IGNORED: u32 = 0xFFFE_0000;

// ---- calendar -----------------------------------------------------------------------------------------

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `System.DateTime` restricted to what the model uses (second resolution, years 1..=9999).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateTime {
    /// `new DateTime(year, month, day, hour, minute, second)`; `None` where .NET throws.
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Option<DateTime> {
        let valid = (1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60;
        valid.then_some(DateTime { year, month, day, hour, minute, second })
    }

    fn seconds_since_epoch(&self) -> i64 {
        days_from_civil(i64::from(self.year), i64::from(self.month), i64::from(self.day)) * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    fn from_seconds_since_epoch(total: i64) -> DateTime {
        let min = days_from_civil(1, 1, 1) * 86_400;
        let max = days_from_civil(9999, 12, 31) * 86_400 + 86_399;
        let total = total.clamp(min, max);
        let days = total.div_euclid(86_400);
        let rest = total.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        DateTime {
            year: y as i32,
            month: m as u32,
            day: d as u32,
            hour: (rest / 3600) as u32,
            minute: (rest % 3600 / 60) as u32,
            second: (rest % 60) as u32,
        }
    }

    /// `AddSeconds` (saturating at the ends of the range, where .NET throws).
    pub fn add_seconds(&self, seconds: i64) -> DateTime {
        Self::from_seconds_since_epoch(self.seconds_since_epoch() + seconds)
    }

    pub fn add_hours(&self, hours: i64) -> DateTime {
        self.add_seconds(hours * 3600)
    }

    /// `System.DayOfWeek`: 0 = Sunday.
    pub fn day_of_week(&self) -> u32 {
        let days = days_from_civil(i64::from(self.year), i64::from(self.month), i64::from(self.day));
        (days + 4).rem_euclid(7) as u32
    }
}

/// `Rank.Units` / `Rank.Tens` of Renode's `IntegerRankExtensions`.
#[derive(Clone, Copy)]
enum Rank {
    Units,
    Tens,
}

fn read_rank(value: i32, rank: Rank) -> u32 {
    match rank {
        Rank::Units => value.rem_euclid(10) as u32,
        Rank::Tens => (value / 10).rem_euclid(10) as u32,
    }
}

/// `WithUpdatedRank`: replaces one decimal digit; `None` where Renode throws (digit above 9).
fn with_rank(current: i32, digit: u32, rank: Rank) -> Option<i32> {
    if digit > 9 {
        return None;
    }
    let old = read_rank(current, rank) as i32;
    Some(match rank {
        Rank::Units => current + (digit as i32 - old),
        Rank::Tens => current + 10 * (digit as i32 - old),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Select {
    Second,
    Minute,
    Hour,
    Day,
}

/// `TimerConfig`: the calendar proper.
#[derive(Clone, Copy)]
struct MainTimer {
    time: DateTime,
    pm: bool,
    /// 1 = Monday ... 7 = Sunday; may disagree with the date (as the hardware allows).
    weekday: u32,
}

impl MainTimer {
    fn new() -> MainTimer {
        MainTimer { time: DateTime::new(2020, 1, 1, 0, 0, 0).unwrap(), pm: false, weekday: 1 }
    }

    /// `ConfigureAMPM()`.
    fn configure_ampm(&mut self, ampm: bool) {
        if !ampm {
            return;
        }
        if self.pm {
            if self.time.hour < 12 {
                self.time = self.time.add_hours(12);
            }
        } else if self.time.hour >= 12 {
            self.time = self.time.add_hours(-12);
        }
    }

    /// The `TimeState` setter.
    fn set_time(&mut self, time: DateTime, ampm: bool) {
        self.time = time;
        self.configure_ampm(ampm);
    }

    /// The `PM` property getter.
    fn pm_flag(&self, ampm: bool) -> bool {
        self.time.hour > 11 && ampm
    }

    fn get(&self, select: Select, ampm: bool) -> i32 {
        match select {
            Select::Second => self.time.second as i32,
            Select::Minute => self.time.minute as i32,
            Select::Hour => {
                if self.pm_flag(ampm) {
                    self.time.hour as i32 - 12
                } else {
                    self.time.hour as i32
                }
            }
            Select::Day => self.time.day as i32,
        }
    }
}

/// `AlarmConfig`.
#[derive(Clone, Copy, Default)]
struct Alarm {
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    subsecond: i32,
    pm: bool,
    enable: bool,
    flag: bool,
    interrupt_enable: bool,
    subseconds_mask: u32,
    seconds_mask: bool,
    minutes_mask: bool,
    hours_mask: bool,
    days_mask: bool,
}

impl Alarm {
    fn pm_flag(&self, ampm: bool) -> bool {
        self.hour > 11 && ampm
    }

    fn get(&self, select: Select, ampm: bool) -> i32 {
        match select {
            Select::Second => self.second,
            Select::Minute => self.minute,
            Select::Hour => {
                if self.pm_flag(ampm) {
                    self.hour - 12
                } else {
                    self.hour
                }
            }
            Select::Day => self.day,
        }
    }
}

/// What `emulation/rtc_persistence.py` keeps per board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcCheckpoint {
    pub time_register: u32,
    pub date_register: u32,
    pub prescaler_register: u32,
    pub format_12_hour: bool,
    pub backup_registers: [u32; BACKUP_WORDS],
}

impl RtcCheckpoint {
    pub const TIME_MASK: u32 = 0x007F_7F7F;
    pub const DATE_MASK: u32 = 0x00FF_FF3F;
    pub const PRESCALER_MASK: u32 = 0x007F_7FFF;

    /// The validation of `rtc_persistence.py` (`calendar_datetime` and the prescaler mask): BCD digits, reserved
    /// bits, the 12-hour range of the pinned model, weekday 1..=7 and a real calendar date.
    pub fn validate(&self) -> Result<DateTime, String> {
        let bcd = |value: u32, what: &str| -> Result<u32, String> {
            if (value & 15) > 9 || (value >> 4) > 9 {
                Err(format!("Invalid RTC {what}: invalid BCD digits"))
            } else {
                Ok((value >> 4) * 10 + (value & 15))
            }
        };
        if self.time_register & !Self::TIME_MASK != 0 || self.date_register & !Self::DATE_MASK != 0 {
            return Err("Invalid RTC calendar: reserved bits are set".into());
        }
        let second = bcd(self.time_register & 0x7F, "second")?;
        let minute = bcd((self.time_register >> 8) & 0x7F, "minute")?;
        let mut hour = bcd((self.time_register >> 16) & 0x3F, "hour")?;
        let pm = self.time_register & 0x40_0000 != 0;
        if self.format_12_hour {
            // This pinned model reports 00..11 in AM/PM mode, including midnight/noon.
            if hour > 11 {
                return Err("Invalid RTC 12-hour calendar for the pinned model".into());
            }
            if pm {
                hour += 12;
            }
        } else if pm || hour > 23 {
            return Err("Invalid RTC 24-hour calendar".into());
        }
        let day = bcd(self.date_register & 0x3F, "day")?;
        let month = bcd((self.date_register >> 8) & 0x1F, "month")?;
        let year = 2000 + bcd((self.date_register >> 16) & 0xFF, "year")?;
        let weekday = (self.date_register >> 13) & 7;
        if !(1..=7).contains(&weekday) {
            return Err("Invalid RTC weekday: expected 1 through 7".into());
        }
        if self.prescaler_register & !Self::PRESCALER_MASK != 0 {
            return Err("Invalid RTC prescaler reserved bits".into());
        }
        DateTime::new(year as i32, month, day, hour, minute, second).ok_or_else(|| "Invalid RTC calendar: date out of range".to_string())
    }
}

fn bit(value: u32, n: u32) -> bool {
    value & (1 << n) != 0
}

/// A once-only log key for a (message kind, register name) pair.
fn name_key(kind: u64, name: &str) -> u64 {
    name.bytes().fold(0xCBF2_9CE4_8422_2325u64 ^ kind, |hash, b| (hash ^ u64::from(b)).wrapping_mul(0x100_0000_01B3))
}

fn tags_of(table: &[(&'static str, u32, u32)]) -> Vec<Tag> {
    table.iter().map(|&(name, pos, width)| tag(name, pos, width)).collect()
}

/// Alarm register selection.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    A,
    B,
}

impl Which {
    fn index(self) -> usize {
        match self {
            Which::A => 0,
            Which::B => 1,
        }
    }

    fn register_name(self) -> &'static str {
        match self {
            Which::A => "AlarmARegister",
            Which::B => "AlarmBRegister",
        }
    }
}

pub struct Rtc {
    name: String,
    ticker: LimitTimer,
    fast_ticker: LimitTimer,
    wakeup_timer: LimitTimer,
    main: MainTimer,
    alarms: [Alarm; 2],
    // Register storage that is not derived from the models above.
    cr_wucksel: u32,
    isr_under: u32,
    prediv_s: u32,
    prediv_a: u32,
    wakeup_auto_reload: u32,
    backup: [u32; BACKUP_WORDS],
    first_stage_unlocked: bool,
    registers_unlocked: bool,
    init_mode: bool,
    ampm_format: bool,
    /// `WakeupTimerRegisterErrata`: WUTR reads the written value instead of the timer limit.
    wakeup_timer_register_errata: bool,
    /// The RTC clock of the wakeup timer (`wakeupTimerFrequency`).
    clock_frequency: u64,
}

impl Rtc {
    /// `new STM32F4_RTC(machine, wakeupTimerFrequency)`; 32768 is Renode's default.
    pub fn new(name: impl Into<String>, wakeup_timer_frequency: u64) -> Self {
        let ticker = LimitTimerConfig {
            limit: u64::from(DEFAULT_SYNCHRONOUS_PRESCALER + 1),
            direction: Direction::Descending,
            event_enabled: true,
            divider: u64::from(DEFAULT_ASYNCHRONOUS_PRESCALER + 1),
            auto_update: true,
            ..LimitTimerConfig::new(wakeup_timer_frequency)
        };
        let fast_ticker = LimitTimerConfig {
            limit: 1,
            direction: Direction::Ascending,
            event_enabled: true,
            divider: u64::from(DEFAULT_ASYNCHRONOUS_PRESCALER + 1),
            ..LimitTimerConfig::new(wakeup_timer_frequency)
        };
        let wakeup = LimitTimerConfig { direction: Direction::Ascending, auto_update: true, ..LimitTimerConfig::new(wakeup_timer_frequency) };
        Self {
            name: name.into(),
            ticker: LimitTimer::new(ticker, TICKER),
            fast_ticker: LimitTimer::new(fast_ticker, FAST_TICKER),
            wakeup_timer: LimitTimer::new(wakeup, WAKEUP_TIMER),
            main: MainTimer::new(),
            alarms: [Alarm::default(); 2],
            cr_wucksel: 0,
            isr_under: ISR_RESET,
            prediv_s: DEFAULT_SYNCHRONOUS_PRESCALER,
            prediv_a: DEFAULT_ASYNCHRONOUS_PRESCALER,
            wakeup_auto_reload: 0xFFFF,
            backup: [0; BACKUP_WORDS],
            first_stage_unlocked: false,
            registers_unlocked: false,
            init_mode: false,
            ampm_format: false,
            wakeup_timer_register_errata: false,
            clock_frequency: wakeup_timer_frequency,
        }
    }

    /// `WakeupTimerRegisterErrata` property.
    pub fn set_wakeup_timer_register_errata(&mut self, errata: bool) {
        self.wakeup_timer_register_errata = errata;
    }

    /// The calendar, its weekday register (1 = Monday) and the 12-hour flag, without side effects.
    pub fn calendar(&self) -> (DateTime, u32, bool) {
        (self.main.time, self.main.weekday, self.ampm_format)
    }

    /// Whether the calendar ticker is running.
    pub fn ticking(&self, clock: &dyn ClockRead) -> bool {
        self.ticker.enabled(clock)
    }

    // ---- derived register values ---------------------------------------------------------------------

    fn digits(value: i32) -> (u32, u32) {
        (read_rank(value, Rank::Units), read_rank(value, Rank::Tens))
    }

    fn time_register(&self) -> u32 {
        let (m, a) = (&self.main, self.ampm_format);
        let (su, st) = Self::digits(m.get(Select::Second, a));
        let (mu, mt) = Self::digits(m.get(Select::Minute, a));
        let (hu, ht) = Self::digits(m.get(Select::Hour, a));
        (su & 0xF) | (st & 7) << 4 | (mu & 0xF) << 8 | (mt & 7) << 12 | (hu & 0xF) << 16 | (ht & 3) << 20 | u32::from(m.pm_flag(a)) << 22
    }

    fn date_register(&self) -> u32 {
        let t = &self.main.time;
        let (du, dt) = Self::digits(t.day as i32);
        let (mu, mt) = Self::digits(t.month as i32);
        let (yu, yt) = Self::digits(t.year);
        (du & 0xF) | (dt & 3) << 4 | (mu & 0xF) << 8 | (mt & 1) << 12 | (self.main.weekday & 7) << 13 | (yu & 0xF) << 16 | (yt & 0xF) << 20
    }

    fn control_register(&self, clock: &dyn ClockRead) -> u32 {
        let a = &self.alarms;
        self.cr_wucksel
            | 1 << 5 // BYPSHAD always reads 1
            | u32::from(self.ampm_format) << 6
            | u32::from(a[0].enable) << 8
            | u32::from(a[1].enable) << 9
            | u32::from(self.wakeup_timer.enabled(clock)) << 10
            | u32::from(a[0].interrupt_enable) << 12
            | u32::from(a[1].interrupt_enable) << 13
            | u32::from(self.wakeup_timer.event_enabled()) << 14
    }

    fn alarm_register(&self, which: Which) -> u32 {
        let (al, a) = (&self.alarms[which.index()], self.ampm_format);
        let (su, st) = Self::digits(al.get(Select::Second, a));
        let (mu, mt) = Self::digits(al.get(Select::Minute, a));
        let (hu, ht) = Self::digits(al.get(Select::Hour, a));
        let (du, dt) = Self::digits(al.get(Select::Day, a));
        (su & 0xF)
            | (st & 7) << 4
            | u32::from(al.seconds_mask) << 7
            | (mu & 0xF) << 8
            | (mt & 7) << 12
            | u32::from(al.minutes_mask) << 15
            | (hu & 0xF) << 16
            | (ht & 3) << 20
            | u32::from(al.pm_flag(a)) << 22
            | u32::from(al.hours_mask) << 23
            | (du & 0xF) << 24
            | (dt & 3) << 28
            | u32::from(al.days_mask) << 31
    }

    fn alarm_subsecond_register(&self, which: Which) -> u32 {
        let al = &self.alarms[which.index()];
        ((al.subsecond as u32) & 0x7FFF) | (al.subseconds_mask & 0xF) << 24
    }

    /// The bits of `ISR` its value providers produce now.
    fn isr_provided(&self, clock: &dyn ClockRead) -> u32 {
        u32::from(!self.alarms[0].enable)
            | u32::from(!self.alarms[1].enable) << 1
            | u32::from(!self.wakeup_timer.enabled(clock)) << 2
            // bit 3 (SHPF) and bit 16 (RECALPF) read false
            | u32::from(self.main.time.year != 2000) << 4
            | u32::from(self.init_mode) << 6
            | u32::from(self.alarms[0].flag) << 8
            | u32::from(self.alarms[1].flag) << 9
    }

    /// Side-effect-free register value.
    fn register(&self, offset: u32, clock: &dyn ClockRead) -> Option<u32> {
        Some(match offset {
            reg::TR => self.time_register(),
            reg::DR => self.date_register(),
            reg::CR => self.control_register(clock),
            // The value a read would return (INIT is write-only): stored bits plus the providers' results.
            reg::ISR => ((self.isr_under & !ISR_PROVIDED) | self.isr_provided(clock)) & !ISR_INIT,
            reg::PRER => self.prediv_s | self.prediv_a << 16,
            reg::WUTR => {
                if self.wakeup_timer_register_errata {
                    self.wakeup_auto_reload
                } else {
                    (self.wakeup_timer.limit(clock) as u32) & 0xFFFF
                }
            }
            reg::ALRMAR => self.alarm_register(Which::A),
            reg::ALRMBR => self.alarm_register(Which::B),
            reg::SSR => (self.ticker.value(clock) as u32).wrapping_sub(1) & 0xFFFF,
            reg::ALRMASSR => self.alarm_subsecond_register(Which::A),
            reg::ALRMBSSR => self.alarm_subsecond_register(Which::B),
            reg::CALIBR | reg::WPR | reg::SHIFTR | reg::TSTR | reg::TSDR | reg::TSSSR | reg::CALR | reg::TAFCR | reg::OR => 0,
            reg::BKP0R..=reg::BKP19R if offset % 4 == 0 => self.backup[((offset - reg::BKP0R) / 4) as usize],
            _ => return None,
        })
    }

    // ---- guards ---------------------------------------------------------------------------------------

    fn check_init_mode(&self, ctx: &mut Ctx<'_>, register: &'static str) -> bool {
        if self.init_mode {
            return true;
        }
        ctx.warn_once(name_key(0x11, register), format_args!("Writing to {register} allowed only in init mode"));
        false
    }

    fn check_unlocked(&self, ctx: &mut Ctx<'_>, register: &'static str) -> bool {
        if self.registers_unlocked {
            return true;
        }
        ctx.warn_once(name_key(0x12, register), format_args!("Writing to {register} is allowed only when the register is unlocked"));
        false
    }

    // ---- outputs and alarm logic ------------------------------------------------------------------------

    /// `UpdateInterrupts()`.
    fn update_interrupts(&self, ctx: &mut Ctx<'_>) {
        let a = &self.alarms;
        let state = (a[0].flag && a[0].interrupt_enable) || (a[1].flag && a[1].interrupt_enable);
        ctx.set_output(ALARM_IRQ_LINE, state);
        ctx.set_output(WAKEUP_IRQ_LINE, self.isr_under & ISR_WUTF != 0);
    }

    /// `AlarmConfig.UpdateInterruptFlag()`.
    fn update_alarm_flag(&mut self, ctx: &mut Ctx<'_>, which: Which) {
        let al = self.alarms[which.index()];
        let mut state = al.enable;
        if al.subseconds_mask == 0 {
            // Subseconds mask 0: the alarm fires when the second unit is incremented.
            state &= self.ticker.value(&*ctx) == self.ticker.limit(&*ctx);
        } else {
            let mask = (1u64 << al.subseconds_mask.min(63)) - 1;
            let masked_alarm = (al.subsecond as u32 as u64) & mask;
            let masked_current = self.ticker.value(&*ctx) & mask;
            state &= masked_alarm == masked_current;
        }
        let t = &self.main.time;
        if !al.seconds_mask {
            state &= al.second == t.second as i32;
        }
        if !al.minutes_mask {
            state &= al.minute == t.minute as i32;
        }
        if !al.hours_mask {
            state &= al.hour == t.hour as i32;
        }
        if !al.days_mask {
            // Day of week is not supported.
            state &= al.day == t.day as i32;
        }
        self.alarms[which.index()].flag = state;
        self.update_interrupts(ctx);
    }

    /// `AlarmConfig.ConfigureAMPM()`.
    fn configure_alarm_ampm(&mut self, ctx: &mut Ctx<'_>, which: Which) {
        if !self.ampm_format {
            return;
        }
        let al = self.alarms[which.index()];
        if al.pm {
            if al.hour < 12 {
                self.alarms[which.index()].hour += 12;
                self.update_alarm_flag(ctx, which);
            }
        } else if al.hour >= 12 {
            self.alarms[which.index()].hour -= 12;
            self.update_alarm_flag(ctx, which);
        }
    }

    /// `AlarmConfig.Update(select, rank, digit)`.
    fn update_alarm_digit(&mut self, ctx: &mut Ctx<'_>, which: Which, select: Select, rank: Rank, digit: u32) {
        let current = self.alarms[which.index()].get(select, self.ampm_format);
        let Some(new) = with_rank(current, digit, rank) else {
            ctx.warn_once(0xE100, format_args!("Expected a single-digit value, but got: {digit} (Renode throws ArgumentException here; ignored)"));
            return;
        };
        let al = &mut self.alarms[which.index()];
        match select {
            Select::Second => al.second = new,
            Select::Minute => al.minute = new,
            Select::Hour => al.hour = new,
            Select::Day => al.day = new,
        }
        self.update_alarm_flag(ctx, which);
    }

    /// `UpdateAlarm(alarm, register, action)`: the action runs only for a disabled alarm and unlocked registers.
    fn alarm_allowed(&self, ctx: &mut Ctx<'_>, which: Which) -> bool {
        if self.alarms[which.index()].enable {
            ctx.warn_once(
                0x1300 + which.index() as u64,
                format_args!("Configuring Antmicro.Renode.Peripherals.Timers.STM32F4_RTC+AlarmConfig is allowed only when it is disabled"),
            );
            return false;
        }
        self.check_unlocked(ctx, which.register_name())
    }

    /// `UpdateState()`: the ticker's limit, one second passes.
    fn update_state(&mut self) {
        let previous = self.main.time.day_of_week();
        let next = self.main.time.add_seconds(1);
        self.main.set_time(next, self.ampm_format);
        // The weekday register may disagree with the date; it advances only when the real day of week changed.
        if previous != self.main.time.day_of_week() {
            self.main.weekday = (self.main.weekday % 7) + 1;
        }
    }

    /// `UpdateAlarms()`: the sub-second ticker's limit.
    fn update_alarms(&mut self, ctx: &mut Ctx<'_>) {
        self.update_alarm_flag(ctx, Which::A);
        self.update_alarm_flag(ctx, Which::B);
    }

    // ---- TR / DR ----------------------------------------------------------------------------------------

    /// `UpdateTime()`: the register-level write callback of `TR`.
    fn update_time(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        if !self.check_init_mode(ctx, "TimeRegister") || !self.check_unlocked(ctx, "TimeRegister") {
            return;
        }
        let t = self.main.time;
        let new = (|| {
            let second = with_rank(with_rank(t.second as i32, value & 0xF, Rank::Units)?, (value >> 4) & 7, Rank::Tens)?;
            let minute = with_rank(with_rank(t.minute as i32, (value >> 8) & 0xF, Rank::Units)?, (value >> 12) & 7, Rank::Tens)?;
            let hour = with_rank(with_rank(t.hour as i32, (value >> 16) & 0xF, Rank::Units)?, (value >> 20) & 3, Rank::Tens)?;
            if hour < 0 || minute < 0 || second < 0 {
                return None;
            }
            DateTime::new(t.year, t.month, t.day, hour as u32, minute as u32, second as u32)
        })();
        match new {
            Some(time) => self.main.set_time(time, self.ampm_format),
            None => ctx.warn_once(0xE200, format_args!("Invalid time written to the time register: 0x{value:X} (Renode throws here; the write is ignored)")),
        }
    }

    /// `UpdateDate()`: the register-level write callback of `DR`.
    fn update_date(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        if !self.check_init_mode(ctx, "DateRegister") || !self.check_unlocked(ctx, "DateRegister") {
            return;
        }
        let t = self.main.time;
        let new = (|| {
            let day = with_rank(with_rank(t.day as i32, value & 0xF, Rank::Units)?, (value >> 4) & 3, Rank::Tens)?;
            let month = with_rank(with_rank(t.month as i32, (value >> 8) & 0xF, Rank::Units)?, (value >> 12) & 1, Rank::Tens)?;
            let year = with_rank(with_rank(t.year, (value >> 16) & 0xF, Rank::Units)?, (value >> 20) & 0xF, Rank::Tens)?;
            if day < 0 || month < 0 {
                return None;
            }
            DateTime::new(year, month as u32, day as u32, t.hour, t.minute, t.second)
        })();
        match new {
            Some(time) => self.main.set_time(time, self.ampm_format),
            None => ctx.warn_once(0xE300, format_args!("Invalid date written to the date register: 0x{value:X} (Renode throws here; the write is ignored)")),
        }
    }

    // ---- writes ---------------------------------------------------------------------------------------

    fn write_register(&mut self, ctx: &mut Ctx<'_>, offset: u32, value: u32) {
        match offset {
            reg::TR => {
                // PM field callback, then the register-level UpdateTime().
                if self.check_init_mode(ctx, "TimeRegister") && self.check_unlocked(ctx, "TimeRegister") {
                    self.main.pm = bit(value, 22);
                    self.main.configure_ampm(self.ampm_format);
                }
                self.update_time(ctx, value);
                warn_tags(ctx, offset, value, 0x007F_7F7F, &tags_of(&[("RESERVED", 7, 1), ("RESERVED", 15, 1), ("RESERVED", 23, 9)]));
            }
            reg::DR => {
                // WDU field callback, then the register-level UpdateDate().
                if self.check_init_mode(ctx, "DateRegister") && self.check_unlocked(ctx, "DateRegister") {
                    let weekday = (value >> 13) & 7;
                    if weekday == 0 {
                        ctx.logf(LogLevel::Warning, format_args!("Writting value 0 to WeekDay register is forbidden"));
                    } else {
                        self.main.weekday = weekday;
                    }
                }
                self.update_date(ctx, value);
                warn_tags(ctx, offset, value, 0x00FF_FF3F, &tags_of(&[("RESERVED", 6, 2), ("RESERVED", 24, 8)]));
            }
            reg::CR => self.write_control(ctx, value),
            reg::ISR => self.write_isr(ctx, value),
            reg::PRER => {
                self.prediv_s = value & 0x7FFF;
                self.prediv_a = (value >> 16) & 0x7F;
                self.ticker.set_limit(ctx, u64::from(self.prediv_s) + 1);
                let divider = u64::from(self.prediv_a) + 1;
                self.ticker.set_divider(ctx, divider);
                self.fast_ticker.set_divider(ctx, divider);
                warn_tags(ctx, offset, value, 0x007F_7FFF, &tags_of(&[("RESERVED", 15, 1), ("RESERVED", 23, 9)]));
            }
            reg::WUTR => {
                self.wakeup_auto_reload = value & 0xFFFF;
                if self.check_unlocked(ctx, "WakeupTimerRegister") {
                    let mut wut = u64::from(value & 0xFFFF);
                    // WUCKSEL '11x': 2^16 is added to the WUT counter value.
                    if self.cr_wucksel & 0b110 == 0b110 {
                        wut += 0x10000;
                    }
                    // The wakeup flag is set every WUT + 1 cycles of the wakeup timer.
                    self.wakeup_timer.set_limit(ctx, wut + 1);
                }
                warn_tags(ctx, offset, value, 0xFFFF, &tags_of(&[("RESERVED", 16, 16)]));
            }
            reg::CALIBR => warn_tags(ctx, offset, value, 0, &tags_of(&[("DC", 0, 5), ("RESERVED", 5, 2), ("DCS", 7, 1), ("RESERVED", 8, 24)])),
            reg::ALRMAR => self.write_alarm(ctx, Which::A, value),
            reg::ALRMBR => self.write_alarm(ctx, Which::B, value),
            reg::WPR => {
                let key = value & 0xFF;
                if key == UNLOCK_KEY1 && !self.first_stage_unlocked {
                    self.first_stage_unlocked = true;
                } else if key == UNLOCK_KEY2 && self.first_stage_unlocked {
                    self.registers_unlocked = true;
                    self.first_stage_unlocked = false;
                } else {
                    self.first_stage_unlocked = false;
                    self.registers_unlocked = false;
                }
                warn_tags(ctx, offset, value, 0xFF, &tags_of(&[("RESERVED", 8, 24)]));
            }
            reg::SSR => warn_tags(ctx, offset, value, 0xFFFF, &tags_of(&[("RESERVED", 16, 16)])),
            reg::SHIFTR => warn_tags(ctx, offset, value, 0, &tags_of(&[("SUBFS", 0, 15), ("RESERVED", 15, 16), ("ADD1S", 31, 1)])),
            reg::TSTR => warn_tags(
                ctx,
                offset,
                value,
                0,
                &tags_of(&[("Second", 0, 7), ("RESERVED", 7, 1), ("Minute", 8, 7), ("RESERVED", 15, 1), ("Hour", 16, 6), ("PM", 22, 1), ("RESERVED", 23, 9)]),
            ),
            reg::TSDR => warn_tags(ctx, offset, value, 0, &tags_of(&[("Day", 0, 6), ("RESERVED", 6, 2), ("Month", 8, 5), ("WDU", 13, 3), ("RESERVED", 16, 16)])),
            reg::TSSSR => warn_tags(ctx, offset, value, 0, &tags_of(&[("SS", 0, 16), ("RESERVED", 16, 16)])),
            reg::CALR => warn_tags(
                ctx,
                offset,
                value,
                0,
                &tags_of(&[("CALM", 0, 9), ("RESERVED", 9, 4), ("CALW16", 13, 1), ("CALW8", 14, 1), ("CALP", 15, 1), ("RESERVED", 16, 16)]),
            ),
            reg::TAFCR => warn_tags(
                ctx,
                offset,
                value,
                0,
                &tags_of(&[
                    ("TAMP1E", 0, 1),
                    ("TAMP1TRG", 1, 1),
                    ("TAMPIE", 2, 1),
                    ("TAMP2E", 3, 1),
                    ("TAMP2TRG", 4, 1),
                    ("RESERVED", 5, 2),
                    ("TAMPTS", 7, 1),
                    ("TAMPFREQ", 8, 3),
                    ("TAMPFLT", 11, 2),
                    ("TAMPPRCH", 13, 2),
                    ("TAMPPUDIS", 15, 1),
                    ("TAMP1INSEL", 16, 1),
                    ("TSINSEL", 17, 1),
                    ("ALARMOUTTYPE", 18, 1),
                    ("RESERVED", 19, 13),
                ]),
            ),
            reg::ALRMASSR => self.write_alarm_subsecond(ctx, Which::A, offset, value),
            reg::ALRMBSSR => self.write_alarm_subsecond(ctx, Which::B, offset, value),
            reg::OR => warn_tags(ctx, offset, value, 0, &tags_of(&[("RTC_ALARM_TYPE", 0, 1), ("RTC_OUT_RMP", 1, 1), ("RESERVED", 2, 30)])),
            reg::BKP0R..=reg::BKP19R if offset % 4 == 0 => self.backup[((offset - reg::BKP0R) / 4) as usize] = value,
            _ => ctx.warn_once(0x4EAD_8000 | u64::from(offset), format_args!("Unhandled write to offset 0x{offset:X}, value 0x{value:X}.")),
        }
    }

    /// The write callbacks of `CR`, in field order.
    fn write_control(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        let unlocked = |s: &Self, ctx: &mut Ctx<'_>| s.check_unlocked(ctx, "ControlRegister");
        // WUCKSEL (stored regardless of the lock).
        let wucksel = value & 7;
        self.cr_wucksel = wucksel;
        if unlocked(self, ctx) {
            let divider = if wucksel & 0b100 == 0 {
                // 0xx: RTC / 2^(4 - xx)
                1u64 << (4 - wucksel)
            } else {
                // 1xx: ck_spre = RTC / ((PREDIV_S + 1) * (PREDIV_A + 1))
                u64::from(self.prediv_s + 1) * u64::from(self.prediv_a + 1)
            };
            if divider > self.clock_frequency {
                // Renode parity (not reproduced): `Frequency / divider` is an integer division, so the wakeup timer's
                // clock entry gets 0 Hz. The stock clock source throws DivideByZeroException as soon as that entry is
                // enabled (the CR write that sets WUTE) and again on every later access to the clock entry, which
                // leaves the RTC unusable. Here the entry never reaches its limit and the device keeps working.
                ctx.warn_once(
                    0xE400,
                    format_args!(
                        "Wakeup clock selection ck_spre divides the {} Hz RTC clock by {divider}: 0 Hz (Renode throws DivideByZeroException when the wakeup timer is enabled; here it never fires)",
                        self.clock_frequency
                    ),
                );
            }
            self.wakeup_timer.set_divider(ctx, divider);
        }
        // BYPSHAD: always reads 1.
        if !bit(value, 5) {
            ctx.logf(LogLevel::Warning, format_args!("Shadow registers are not supported"));
        }
        // FMT
        if unlocked(self, ctx) {
            self.ampm_format = bit(value, 6);
            self.main.configure_ampm(self.ampm_format);
            self.configure_alarm_ampm(ctx, Which::A);
            self.configure_alarm_ampm(ctx, Which::B);
        }
        // ALRAE, ALRBE
        if unlocked(self, ctx) {
            self.alarms[0].enable = bit(value, 8);
            self.update_alarm_flag(ctx, Which::A);
        }
        if unlocked(self, ctx) {
            self.alarms[1].enable = bit(value, 9);
            self.update_alarm_flag(ctx, Which::B);
        }
        // WUTE
        if unlocked(self, ctx) {
            self.wakeup_timer.set_enabled(ctx, bit(value, 10));
            self.wakeup_timer.set_value(ctx, 0);
        }
        // ALRAIE, ALRBIE
        if unlocked(self, ctx) {
            self.alarms[0].interrupt_enable = bit(value, 12);
            self.update_alarm_flag(ctx, Which::A);
        }
        if unlocked(self, ctx) {
            self.alarms[1].interrupt_enable = bit(value, 13);
            self.update_alarm_flag(ctx, Which::B);
        }
        // WUTIE
        if unlocked(self, ctx) {
            self.wakeup_timer.set_event_enabled(bit(value, 14));
        }
        warn_tags(
            ctx,
            reg::CR,
            value,
            0x7767,
            &tags_of(&[
                ("TSEDGE", 3, 1),
                ("REFCKON", 4, 1),
                ("DCE", 7, 1),
                ("TSE", 11, 1),
                ("TSIE", 15, 1),
                ("ADD1H", 16, 1),
                ("SUB1H", 17, 1),
                ("BKP", 18, 1),
                ("COSEL", 19, 1),
                ("POL", 20, 1),
                ("OSEL", 21, 2),
                ("COE", 23, 1),
                ("RESERVED", 24, 8),
            ]),
        );
    }

    /// `ISR`: write-zero-to-clear flags against the stale stored value, `INIT` by change.
    fn write_isr(&mut self, ctx: &mut Ctx<'_>, value: u32) {
        let base = self.isr_under;
        let difference = base ^ value;
        let mut under = base;
        let mut init_changed = false;
        let mut cleared = [false; 3]; // ALRAF, ALRBF, WUTF
        // RSF
        if difference & under & ISR_RSF != 0 {
            under &= !(!value & ISR_RSF);
        }
        // INIT (write only; a change callback fires when the written bit differs from the stored one)
        if difference & ISR_INIT != 0 {
            under = (under & !ISR_INIT) | (value & ISR_INIT);
            init_changed = true;
        }
        // ALRAF, ALRBF, WUTF, then the other flags (no callbacks)
        for (n, bit_mask) in [(0usize, 1u32 << 8), (1, 1 << 9), (2, ISR_WUTF)] {
            if difference & under & bit_mask != 0 {
                under &= !(!value & bit_mask);
                cleared[n] = true;
            }
        }
        if difference & under & ISR_FLAGS_11_15 != 0 {
            under &= !(!value & ISR_FLAGS_11_15);
        }
        // The "ignored" bits 17..31 simply store.
        if difference & ISR_IGNORED != 0 {
            under = (under & !ISR_IGNORED) | (value & ISR_IGNORED);
        }
        self.isr_under = under;
        // Change callbacks in field order.
        if init_changed && self.check_unlocked(ctx, "ISR") {
            let init = value & ISR_INIT != 0;
            self.ticker.set_enabled(ctx, !init);
            self.fast_ticker.set_enabled(ctx, !init);
            self.init_mode = init;
        }
        if cleared[0] {
            self.alarms[0].flag = false;
            self.update_interrupts(ctx);
        }
        if cleared[1] {
            self.alarms[1].flag = false;
            self.update_interrupts(ctx);
        }
        if cleared[2] {
            self.update_interrupts(ctx);
        }
    }

    /// `ALRMAR`/`ALRMBR`: every field callback runs `UpdateAlarm`.
    fn write_alarm(&mut self, ctx: &mut Ctx<'_>, which: Which, value: u32) {
        let digit = |shift: u32, width: u32| (value >> shift) & ((1 << width) - 1);
        // (select, rank, digit) fields and flag fields, in register order.
        #[derive(Clone, Copy)]
        enum Field {
            Digit(Select, Rank, u32),
            SecondsMask(bool),
            MinutesMask(bool),
            Pm(bool),
            HoursMask(bool),
            DaysMask(bool),
        }
        let fields = [
            Field::Digit(Select::Second, Rank::Units, digit(0, 4)),
            Field::Digit(Select::Second, Rank::Tens, digit(4, 3)),
            Field::SecondsMask(bit(value, 7)),
            Field::Digit(Select::Minute, Rank::Units, digit(8, 4)),
            Field::Digit(Select::Minute, Rank::Tens, digit(12, 3)),
            Field::MinutesMask(bit(value, 15)),
            Field::Digit(Select::Hour, Rank::Units, digit(16, 4)),
            Field::Digit(Select::Hour, Rank::Tens, digit(20, 2)),
            Field::Pm(bit(value, 22)),
            Field::HoursMask(bit(value, 23)),
            Field::Digit(Select::Day, Rank::Units, digit(24, 4)),
            Field::Digit(Select::Day, Rank::Tens, digit(28, 2)),
            Field::DaysMask(bit(value, 31)),
        ];
        for field in fields {
            if !self.alarm_allowed(ctx, which) {
                continue;
            }
            match field {
                Field::Digit(select, rank, d) => self.update_alarm_digit(ctx, which, select, rank, d),
                Field::SecondsMask(v) => {
                    self.alarms[which.index()].seconds_mask = v;
                    self.update_alarm_flag(ctx, which);
                }
                Field::MinutesMask(v) => {
                    self.alarms[which.index()].minutes_mask = v;
                    self.update_alarm_flag(ctx, which);
                }
                Field::Pm(v) => {
                    self.alarms[which.index()].pm = v;
                    self.configure_alarm_ampm(ctx, which);
                }
                Field::HoursMask(v) => {
                    self.alarms[which.index()].hours_mask = v;
                    self.update_alarm_flag(ctx, which);
                }
                Field::DaysMask(v) => {
                    self.alarms[which.index()].days_mask = v;
                    self.update_alarm_flag(ctx, which);
                }
            }
        }
        let offset = if which == Which::A { reg::ALRMAR } else { reg::ALRMBR };
        warn_tags(ctx, offset, value, !(1u32 << 30), &tags_of(&[("WDSEL", 30, 1)]));
    }

    fn write_alarm_subsecond(&mut self, ctx: &mut Ctx<'_>, which: Which, offset: u32, value: u32) {
        if self.alarm_allowed(ctx, which) {
            self.alarms[which.index()].subsecond = (value & 0x7FFF) as i32;
            self.update_alarm_flag(ctx, which);
        }
        if self.alarm_allowed(ctx, which) {
            self.alarms[which.index()].subseconds_mask = (value >> 24) & 0xF;
            self.update_alarm_flag(ctx, which);
        }
        warn_tags(ctx, offset, value, 0x0F00_7FFF, &tags_of(&[("RESERVED", 15, 9), ("RESERVED", 28, 4)]));
    }

    // ---- reads ----------------------------------------------------------------------------------------

    fn read_isr(&mut self, ctx: &mut Ctx<'_>) -> u32 {
        // Value providers replace their bits of the stored value, the non-readable INIT bit is hidden, then
        // RSF's read callback sets the stored flag if the value just read had it clear.
        self.isr_under = (self.isr_under & !ISR_PROVIDED) | self.isr_provided(&*ctx);
        let value = self.isr_under & !ISR_INIT;
        if self.isr_under & ISR_RSF == 0 {
            self.isr_under |= ISR_RSF;
        }
        value
    }

    // ---- checkpoint -----------------------------------------------------------------------------------

    /// Exports the state `rtc_persistence.py` retains, without side effects.
    pub fn checkpoint(&self) -> RtcCheckpoint {
        RtcCheckpoint {
            time_register: self.time_register(),
            date_register: self.date_register(),
            prescaler_register: self.prediv_s | self.prediv_a << 16,
            format_12_hour: self.ampm_format,
            backup_registers: self.backup,
        }
    }

    /// Restores a checkpoint through the protected register sequence of `restore_rtc_state`: unlock, `ISR.INIT`,
    /// `PRER`, `CR` (`BYPSHAD` | `FMT`), `DR`, `TR`, leave `INIT` (the tickers restart from a whole second at
    /// the time of the call), lock again, then the backup words. Call it on a new, not yet running board
    /// (`Board::with_peripheral`). Invalid checkpoints are rejected before the first write.
    pub fn restore_checkpoint(&mut self, ctx: &mut Ctx<'_>, checkpoint: &RtcCheckpoint) -> Result<(), String> {
        checkpoint.validate()?;
        self.write_register(ctx, reg::WPR, 0xCA);
        self.write_register(ctx, reg::WPR, 0x53);
        self.write_register(ctx, reg::ISR, 0x80); // INIT: calendar tickers stopped during programming
        self.write_register(ctx, reg::PRER, checkpoint.prescaler_register);
        self.write_register(ctx, reg::CR, 0x20 | if checkpoint.format_12_hour { 0x40 } else { 0 });
        self.write_register(ctx, reg::DR, checkpoint.date_register);
        self.write_register(ctx, reg::TR, checkpoint.time_register);
        self.write_register(ctx, reg::ISR, 0); // leave INIT
        self.write_register(ctx, reg::WPR, 0xFF);
        for (index, word) in checkpoint.backup_registers.iter().enumerate() {
            self.write_register(ctx, reg::BKP0R + 4 * index as u32, *word);
        }
        Ok(())
    }
}

impl Peripheral for Rtc {
    fn name(&self) -> &str {
        &self.name
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        // Creation order of the clock entries: ticker, fastTicker, wakeupTimer.
        self.ticker.attach(ctx);
        self.fast_ticker.attach(ctx);
        self.wakeup_timer.attach(ctx);
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        // registers.Reset(): everything but the backup registers returns to its reset value.
        self.cr_wucksel = 0;
        self.isr_under = ISR_RESET;
        self.prediv_s = DEFAULT_SYNCHRONOUS_PRESCALER;
        self.prediv_a = DEFAULT_ASYNCHRONOUS_PRESCALER;
        self.wakeup_auto_reload = 0xFFFF;
        ctx.set_output(ALARM_IRQ_LINE, false);
        ctx.set_output(WAKEUP_IRQ_LINE, false);
        // ResetInnerTimers()
        self.main = MainTimer::new();
        self.ticker.reset(ctx);
        self.fast_ticker.reset(ctx);
        self.alarms = [Alarm::default(); 2];
        self.wakeup_timer.reset(ctx);
        // ResetInnerStatus()
        self.first_stage_unlocked = false;
        self.registers_unlocked = false;
        self.init_mode = false;
        self.ampm_format = false;
    }

    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        if offset == reg::ISR {
            return self.read_isr(ctx);
        }
        match self.register(offset, &*ctx) {
            Some(value) => value,
            None => {
                ctx.warn_once(0x4EAD_0000 | u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}."));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        self.write_register(ctx, offset, value);
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        match token {
            TICKER => {
                if self.ticker.on_limit_reached() {
                    self.update_state();
                }
            }
            FAST_TICKER => {
                if self.fast_ticker.on_limit_reached() {
                    self.update_alarms(ctx);
                }
            }
            WAKEUP_TIMER => {
                if self.wakeup_timer.on_limit_reached() {
                    self.isr_under |= ISR_WUTF; // reset by software
                    self.update_interrupts(ctx);
                }
            }
            _ => {}
        }
    }

    fn peek(&self, offset: u32, _width: Width, view: &View<'_>) -> Option<u32> {
        self.register(offset, view)
    }

    fn summary(&self, view: &View<'_>) -> String {
        let t = &self.main.time;
        format!(
            "{}: {:04}-{:02}-{:02} {:02}:{:02}:{:02} weekday={} ampm={} ticking={} init={} unlocked={} alarmA={} alarmB={} wakeup={}",
            self.name,
            t.year,
            t.month,
            t.day,
            t.hour,
            t.minute,
            t.second,
            self.main.weekday,
            self.ampm_format,
            self.ticker.enabled(view),
            self.init_mode,
            self.registers_unlocked,
            self.alarms[0].enable,
            self.alarms[1].enable,
            self.wakeup_timer.enabled(view)
        )
    }

    impl_peripheral_any!();
}
