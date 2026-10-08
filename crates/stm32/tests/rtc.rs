//! `Timers.STM32F4_RTC` on a bare machine: calendar, write protection, INIT/RSF sequences used by the firmware,
//! alarms, the wakeup timer at both clock frequencies, backup registers and the checkpoint API.

use emu_core::testing::{Harness, Probe};
use emu_core::{PeriphId, Time, Width, TICKS_PER_MILLISECOND, TICKS_PER_SECOND};
use stm32::rtc::{reg, DateTime, Rtc, RtcCheckpoint, ALARM_IRQ_LINE, BACKUP_WORDS, WAKEUP_IRQ_LINE};

const MS: Time = TICKS_PER_MILLISECOND;
const SEC: Time = TICKS_PER_SECOND;
const BASE: u32 = 0x4000_2800;

fn rtc(frequency: u64) -> (Harness, PeriphId) {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, Rtc::new("rtc", frequency));
    (h, id)
}

fn w(h: &mut Harness, offset: u32, value: u32) {
    h.write32(BASE + offset, value);
}

fn r(h: &mut Harness, offset: u32) -> u32 {
    h.read32(BASE + offset)
}

fn unlock(h: &mut Harness) {
    w(h, reg::WPR, 0xCA);
    w(h, reg::WPR, 0x53);
}

fn lock(h: &mut Harness) {
    w(h, reg::WPR, 0xFF);
}

fn bcd(v: u32) -> u32 {
    (v / 10) << 4 | v % 10
}

fn time_register(hour: u32, minute: u32, second: u32, pm: bool) -> u32 {
    bcd(hour) << 16 | bcd(minute) << 8 | bcd(second) | u32::from(pm) << 22
}

fn date_register(year: u32, month: u32, day: u32, weekday: u32) -> u32 {
    bcd(year - 2000) << 16 | weekday << 13 | bcd(month) << 8 | bcd(day)
}

/// The HAL's `RTC_EnterInitMode`/`ExitInitMode` around a calendar write.
fn set_calendar(h: &mut Harness, date: u32, time: u32) {
    unlock(h);
    w(h, reg::ISR, 0x80);
    w(h, reg::DR, date);
    w(h, reg::TR, time);
    w(h, reg::ISR, 0);
    lock(h);
}

fn calendar(h: &Harness, id: PeriphId) -> DateTime {
    h.get::<Rtc>(id).calendar().0
}

fn dt(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime {
    DateTime::new(year, month, day, hour, minute, second).unwrap()
}

#[test]
fn reset_state_and_the_rsf_quirk() {
    let (mut h, _) = rtc(32768);
    assert_eq!(r(&mut h, reg::TR), 0);
    assert_eq!(r(&mut h, reg::DR), 0x0020_2101, "2020-01-01, Monday");
    assert_eq!(r(&mut h, reg::CR), 0x20, "BYPSHAD always reads 1");
    assert_eq!(r(&mut h, reg::PRER), 0x007F_00FF);
    assert_eq!(r(&mut h, reg::WUTR), 0xFFFF, "WUTR reads the timer limit (ulong.MaxValue at reset), truncated to the 16-bit field");
    assert_eq!(r(&mut h, reg::SSR), 255, "ticker value - 1");
    // ISR: ALRAWF, ALRBWF, WUTWF set, INITS (year != 2000) set; RSF reads 0 the first time and 1 afterwards.
    assert_eq!(r(&mut h, reg::ISR), 0x17);
    assert_eq!(r(&mut h, reg::ISR), 0x37);
    assert_eq!(r(&mut h, reg::ISR), 0x37);
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    // Peeking never sets RSF.
    let (mut h2, _) = rtc(32768);
    assert_eq!(h2.peek(BASE + reg::ISR, Width::Word), Some(0x17));
    assert_eq!(h2.peek(BASE + reg::ISR, Width::Word), Some(0x17));
    assert_eq!(r(&mut h2, reg::ISR), 0x17);
}

#[test]
fn calendar_is_frozen_until_the_init_handshake() {
    let (mut h, id) = rtc(32768);
    h.advance_to(10 * SEC);
    assert_eq!(calendar(&h, id), dt(2020, 1, 1, 0, 0, 0));
    assert_eq!(h.next_event_time(), None, "ticker and fastTicker are disabled until INIT is left");
    unlock(&mut h);
    w(&mut h, reg::ISR, 0x80);
    w(&mut h, reg::ISR, 0);
    assert_eq!(h.next_event_time(), Some(10 * SEC + 3_906_250), "the 256 Hz sub-second ticker is the first event");
}

#[test]
fn time_and_date_writes_need_init_mode_and_the_unlock_sequence() {
    let (mut h, id) = rtc(32768);
    w(&mut h, reg::TR, time_register(12, 34, 56, false));
    assert_eq!(calendar(&h, id), dt(2020, 1, 1, 0, 0, 0));
    assert!(h.warnings().iter().any(|m| m.contains("TimeRegister allowed only in init mode")), "{:?}", h.warnings());
    // Unlocked but not in init mode.
    unlock(&mut h);
    w(&mut h, reg::DR, date_register(2026, 10, 7, 3));
    assert_eq!(calendar(&h, id), dt(2020, 1, 1, 0, 0, 0));
    // INIT with the registers locked is ignored (and warns); the ISR write still stores the bit.
    lock(&mut h);
    w(&mut h, reg::ISR, 0x80);
    assert!(h.warnings().iter().any(|m| m.contains("ISR is allowed only when the register is unlocked")), "{:?}", h.warnings());
    // Wrong key order: 0x53 first, then 0xCA 0xCA (the second 0xCA restarts), then 0x53 completes.
    w(&mut h, reg::WPR, 0x53);
    w(&mut h, reg::WPR, 0xCA);
    w(&mut h, reg::WPR, 0xCA);
    w(&mut h, reg::WPR, 0x53);
    // Renode parity: the rejected INIT write left the stored INIT bit set, so writing it again would not be a
    // change. The bit has to be cleared (a change, now accepted) before the handshake works.
    w(&mut h, reg::ISR, 0);
    // Correct sequence works.
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(12, 34, 56, false));
    assert_eq!(calendar(&h, id), dt(2026, 10, 7, 12, 34, 56));
    assert_eq!(r(&mut h, reg::TR), time_register(12, 34, 56, false));
    assert_eq!(r(&mut h, reg::DR), date_register(2026, 10, 7, 3));
    assert_eq!(h.get::<Rtc>(id).calendar().1, 3, "weekday register");
}

#[test]
fn the_init_bit_acts_only_when_it_differs_from_the_previously_written_one() {
    let (mut h, id) = rtc(32768);
    // INIT written while locked: the stored bit flips, the callback warns and does nothing.
    w(&mut h, reg::ISR, 0x80);
    unlock(&mut h);
    // Writing INIT = 1 again is not a change any more: init mode is never entered.
    w(&mut h, reg::ISR, 0x80);
    w(&mut h, reg::TR, time_register(1, 2, 3, false));
    assert_eq!(calendar(&h, id), dt(2020, 1, 1, 0, 0, 0));
    // Leaving (1 -> 0) is a change, but enables the tickers only.
    w(&mut h, reg::ISR, 0);
    assert!(h.next_event_time().is_some());
}

#[test]
fn calendar_ticks_once_per_second_at_32768_hz() {
    let (mut h, id) = rtc(32768);
    set_calendar(&mut h, date_register(2020, 12, 31, 4), time_register(23, 59, 58, false));
    assert_eq!(calendar(&h, id), dt(2020, 12, 31, 23, 59, 58));
    h.advance_to(SEC - 1);
    assert_eq!(calendar(&h, id), dt(2020, 12, 31, 23, 59, 58));
    h.advance_to(SEC);
    assert_eq!(calendar(&h, id), dt(2020, 12, 31, 23, 59, 59));
    h.advance_to(2 * SEC);
    assert_eq!(calendar(&h, id), dt(2021, 1, 1, 0, 0, 0));
    assert_eq!(h.get::<Rtc>(id).calendar().1, 5, "the weekday register advanced with the real day of the week");
    assert_eq!(r(&mut h, reg::DR), date_register(2021, 1, 1, 5));
    h.advance_to(2 * SEC + 59 * SEC);
    assert_eq!(calendar(&h, id), dt(2021, 1, 1, 0, 0, 59));
    h.advance_to(2 * SEC + 60 * SEC);
    assert_eq!(calendar(&h, id), dt(2021, 1, 1, 0, 1, 0));
}

#[test]
fn handset_clock_second_lasts_1_024_s() {
    let (mut h, id) = rtc(32_000);
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(0, 0, 0, false));
    h.advance_to(1_023_999_999);
    assert_eq!(calendar(&h, id).second, 0);
    h.advance_to(1_024_000_000);
    assert_eq!(calendar(&h, id).second, 1);
    h.advance_to(10 * 1_024_000_000);
    assert_eq!(calendar(&h, id).second, 10);
}

#[test]
fn leap_day_and_weekday_rollover() {
    let (mut h, id) = rtc(32768);
    set_calendar(&mut h, date_register(2024, 2, 28, 3), time_register(23, 59, 59, false));
    h.advance_to(SEC);
    assert_eq!(calendar(&h, id), dt(2024, 2, 29, 0, 0, 0));
    assert_eq!(h.get::<Rtc>(id).calendar().1, 4);
    set_calendar(&mut h, date_register(2024, 2, 29, 4), time_register(23, 59, 59, false));
    let t = h.now();
    h.advance_to(t + SEC);
    assert_eq!(calendar(&h, id), dt(2024, 3, 1, 0, 0, 0), "2024 is a leap year");
    set_calendar(&mut h, date_register(2025, 2, 28, 5), time_register(23, 59, 59, false));
    let t = h.now();
    h.advance_to(t + SEC);
    assert_eq!(calendar(&h, id), dt(2025, 3, 1, 0, 0, 0), "2025 is not");
    // Sunday -> Monday wraps the weekday register to 1.
    set_calendar(&mut h, date_register(2026, 10, 11, 7), time_register(23, 59, 59, false));
    let t = h.now();
    h.advance_to(t + SEC);
    assert_eq!(calendar(&h, id), dt(2026, 10, 12, 0, 0, 0));
    assert_eq!(h.get::<Rtc>(id).calendar().1, 1);
}

#[test]
fn invalid_calendar_writes_are_ignored_with_an_error() {
    let (mut h, id) = rtc(32768);
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(1, 2, 3, false));
    unlock(&mut h);
    w(&mut h, reg::ISR, 0x80);
    w(&mut h, reg::ISR, 0x80 | 0x7); // keep INIT, no change callback
    w(&mut h, reg::ISR, 0);
    w(&mut h, reg::ISR, 0x80);
    // 30 February, hour 29 and a BCD digit above 9 are rejected.
    w(&mut h, reg::DR, date_register(2026, 2, 30, 3));
    w(&mut h, reg::TR, bcd(29) << 16);
    w(&mut h, reg::TR, 0xA);
    assert_eq!(calendar(&h, id), dt(2026, 10, 7, 1, 2, 3));
    assert!(h.warnings().iter().filter(|m| m.contains("Renode throws")).count() >= 2, "{:?}", h.warnings());
    // Weekday 0 is forbidden.
    w(&mut h, reg::DR, date_register(2026, 10, 7, 0));
    assert!(h.warnings().iter().any(|m| m.contains("Writting value 0 to WeekDay")), "{:?}", h.warnings());
    assert_eq!(h.get::<Rtc>(id).calendar().1, 3);
}

#[test]
fn twelve_hour_format_folds_the_hour_with_the_pm_flag() {
    let (mut h, id) = rtc(32768);
    unlock(&mut h);
    w(&mut h, reg::CR, 0x20 | 0x40); // FMT
    assert_eq!(r(&mut h, reg::CR), 0x60);
    w(&mut h, reg::ISR, 0x80);
    w(&mut h, reg::DR, date_register(2026, 10, 7, 3));
    w(&mut h, reg::TR, time_register(3, 20, 0, true)); // 03:20 PM
    w(&mut h, reg::ISR, 0);
    assert_eq!(calendar(&h, id), dt(2026, 10, 7, 15, 20, 0), "stored in 24-hour form");
    assert_eq!(r(&mut h, reg::TR), time_register(3, 20, 0, true));
    // The PM flag is a stored flag, not derived: AM stays below 12 hours, i.e. 11:59:59 AM -> 00:00:00 AM.
    w(&mut h, reg::ISR, 0x80);
    w(&mut h, reg::TR, time_register(11, 59, 59, false));
    w(&mut h, reg::ISR, 0);
    assert_eq!(calendar(&h, id).hour, 11);
    let t = h.now();
    h.advance_to(t + SEC);
    assert_eq!(calendar(&h, id), dt(2026, 10, 7, 0, 0, 0), "this pinned model folds noon back to midnight");
}

#[test]
fn alarm_a_raises_the_line_for_one_subsecond_tick() {
    let (mut h, id) = rtc(32768);
    let line: Probe = h.probe(id, ALARM_IRQ_LINE);
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(10, 20, 0, false));
    let t0 = h.now();
    // Seconds only: SU = 5, everything else masked.
    unlock(&mut h);
    w(&mut h, reg::ALRMAR, 0x5 | 1 << 15 | 1 << 23 | 1 << 31);
    assert_eq!(r(&mut h, reg::ALRMAR), 0x5 | 1 << 15 | 1 << 23 | 1 << 31);
    w(&mut h, reg::CR, 0x20 | 1 << 8 | 1 << 12); // ALRAE | ALRAIE
    assert_eq!(r(&mut h, reg::CR), 0x20 | 1 << 8 | 1 << 12);
    lock(&mut h);
    h.advance_to(t0 + 10 * SEC);
    // The ticker and the sub-second ticker reach their limits together every second; the sub-second event
    // (created later) sees the freshly reloaded ticker. The next sub-second event 1/256 s later clears the flag.
    let changes = h.probe_changes(line);
    assert_eq!(changes, vec![(t0 + 5 * SEC, true), (t0 + 5 * SEC + 3_906_250, false)]);
    // ISR.ALRAF is never seen set by a read after that; and the alarm is enabled so ALRAWF reads 0.
    assert_eq!(r(&mut h, reg::ISR) & 0x1, 0);
}

#[test]
fn alarm_configuration_needs_a_disabled_alarm_and_unlocked_registers() {
    let (mut h, _) = rtc(32768);
    w(&mut h, reg::ALRMAR, 0x5);
    assert!(h.warnings().iter().any(|m| m.contains("AlarmARegister is allowed only when the register is unlocked")), "{:?}", h.warnings());
    assert_eq!(r(&mut h, reg::ALRMAR), 0);
    unlock(&mut h);
    w(&mut h, reg::CR, 0x20 | 1 << 8); // enable alarm A
    w(&mut h, reg::ALRMAR, 0x7);
    assert!(h.warnings().iter().any(|m| m.contains("is allowed only when it is disabled")), "{:?}", h.warnings());
    assert_eq!(r(&mut h, reg::ALRMAR), 0);
    // Alarm B is independent.
    w(&mut h, reg::ALRMBR, 0x9);
    assert_eq!(r(&mut h, reg::ALRMBR), 0x9);
}

#[test]
fn isr_flags_clear_only_after_a_read_saw_them() {
    // WUTF has no value provider, the alarm flags do: the stale stored bit decides whether a write-zero clears.
    let (mut h, id) = rtc(32768);
    let wakeup: Probe = h.probe(id, WAKEUP_IRQ_LINE);
    unlock(&mut h);
    w(&mut h, reg::WUTR, 16_383); // 16384 ticks
    w(&mut h, reg::CR, 0x20 | 3 | 1 << 10 | 1 << 14); // WUCKSEL = RTC/2, WUTE, WUTIE
    let t0 = h.now();
    h.advance_to(t0 + SEC - 1);
    assert_eq!(r(&mut h, reg::ISR) & 1 << 10, 0);
    h.advance_to(t0 + SEC);
    assert_eq!(h.probe_changes(wakeup), vec![(t0 + SEC, true)]);
    assert_eq!(r(&mut h, reg::ISR) & 1 << 10, 1 << 10);
    // The HAL clears flags with a read-modify-write: ~WUTF written, other bits as read.
    w(&mut h, reg::ISR, !(1 << 10));
    assert_eq!(r(&mut h, reg::ISR) & 1 << 10, 0);
    assert_eq!(h.probe_changes(wakeup), vec![(t0 + SEC, true), (t0 + SEC, false)]);
    h.advance_to(t0 + 2 * SEC);
    assert_eq!(h.probe_changes(wakeup).len(), 3, "the next wakeup period raised it again");
}

#[test]
fn wakeup_flag_needs_wutie_and_follows_the_selected_clock() {
    let (mut h, id) = rtc(32768);
    unlock(&mut h);
    w(&mut h, reg::WUTR, 16_383);
    w(&mut h, reg::CR, 0x20 | 3 | 1 << 10); // no WUTIE
    h.advance_to(5 * SEC);
    assert_eq!(r(&mut h, reg::ISR) & 1 << 10, 0, "EventEnabled is false: the limit does not set WUTF");
    assert_eq!(r(&mut h, reg::CR) & 1 << 14, 0);
    // WUTR reads WUT + 1 (the limit); with the errata property it reads the written value.
    assert_eq!(r(&mut h, reg::WUTR), 16_384);
    h.get_mut::<Rtc>(id).set_wakeup_timer_register_errata(true);
    assert_eq!(r(&mut h, reg::WUTR), 16_383);
    // WUCKSEL = 11x adds 2^16 to the programmed value.
    w(&mut h, reg::CR, 0x20 | 6);
    h.get_mut::<Rtc>(id).set_wakeup_timer_register_errata(false);
    w(&mut h, reg::WUTR, 5);
    assert_eq!(r(&mut h, reg::WUTR), (5 + 0x10000 + 1) & 0xFFFF);
}

#[test]
fn wakeup_timer_at_the_handset_frequency() {
    let (mut h, id) = rtc(32_000);
    let wakeup: Probe = h.probe(id, WAKEUP_IRQ_LINE);
    unlock(&mut h);
    w(&mut h, reg::WUTR, 15_999); // 16000 ticks of 32 kHz / 2
    w(&mut h, reg::CR, 0x20 | 3 | 1 << 10 | 1 << 14);
    h.advance_to(SEC);
    // 32000 / 2 = 16000 Hz: 16000 ticks = exactly one second.
    assert_eq!(h.probe_changes(wakeup), vec![(SEC, true)]);
}

/// Renode parity (not reproduced): ck_spre with prescalers whose product exceeds the RTC clock makes the stock
/// wakeup timer run at 0 Hz (integer division) and throw DivideByZeroException when it is enabled. The handset's
/// 32 kHz clock with the default 128 * 256 prescalers is such a case. Here the device must stay usable.
#[test]
fn ck_spre_below_1_hz_never_fires_and_does_not_break_the_device() {
    let (mut h, id) = rtc(32_000);
    let wakeup: Probe = h.probe(id, WAKEUP_IRQ_LINE);
    unlock(&mut h);
    w(&mut h, reg::WUTR, 1);
    w(&mut h, reg::CR, 0x20 | 4); // WUCKSEL = ck_spre, timer still disabled: fine in Renode too
    assert!(h.warnings().iter().any(|m| m.contains("0 Hz")), "{:?}", h.warnings());
    w(&mut h, reg::CR, 0x20 | 4 | 1 << 10 | 1 << 14); // enable it: Renode throws here
    h.advance_to(100 * SEC);
    assert!(h.probe_changes(wakeup).is_empty(), "a 0 Hz entry never reaches its limit");
    assert_eq!(r(&mut h, reg::CR), 0x20 | 4 | 1 << 10 | 1 << 14, "the device is still readable");
    assert_eq!(r(&mut h, reg::ISR) & 1 << 10, 0);
    // With a prescaler product that fits (125 * 256 = 32000) the same selection is a plain 1 Hz clock.
    let (mut h, id) = rtc(32_000);
    let wakeup: Probe = h.probe(id, WAKEUP_IRQ_LINE);
    unlock(&mut h);
    w(&mut h, reg::PRER, 124 << 16 | 255);
    w(&mut h, reg::WUTR, 1);
    w(&mut h, reg::CR, 0x20 | 4 | 1 << 10 | 1 << 14);
    h.advance_to(5 * SEC);
    assert_eq!(h.probe_changes(wakeup), vec![(2 * SEC, true)], "WUT + 1 = 2 ticks of 1 Hz");
    assert!(h.warnings().iter().all(|m| !m.contains("0 Hz")), "{:?}", h.warnings());
}

#[test]
fn backup_registers_survive_reset_everything_else_does_not() {
    let (mut h, id) = rtc(32768);
    for i in 0..BACKUP_WORDS as u32 {
        w(&mut h, reg::BKP0R + 4 * i, 0x1111_0000 + i);
    }
    w(&mut h, reg::BKP0R + 4, 0x32F0);
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(8, 9, 10, false));
    h.core_mut().reset_all();
    assert_eq!(calendar(&h, id), dt(2020, 1, 1, 0, 0, 0));
    assert_eq!(r(&mut h, reg::BKP0R + 4), 0x32F0);
    assert_eq!(r(&mut h, reg::BKP19R), 0x1111_0013);
    assert_eq!(r(&mut h, reg::TR), 0);
    assert_eq!(r(&mut h, reg::PRER), 0x007F_00FF);
    assert_eq!(h.next_event_time(), None);
    // Offsets beyond the 20 implemented words are unhandled.
    assert_eq!(r(&mut h, reg::BKP19R + 4), 0);
    assert!(h.warnings().iter().any(|m| m.contains("Unhandled read from offset 0xA0.")), "{:?}", h.warnings());
}

#[test]
fn checkpoint_round_trips_through_the_protected_sequence() {
    let (mut h, id) = rtc(32768);
    let mut backup = [0u32; BACKUP_WORDS];
    for (i, b) in backup.iter_mut().enumerate() {
        *b = 0xA5A5_0000 | i as u32;
    }
    backup[1] = 0x32F0;
    let cp = RtcCheckpoint {
        time_register: time_register(21, 43, 5, false),
        date_register: date_register(2026, 10, 7, 3),
        prescaler_register: 0x007F_00FF,
        format_12_hour: false,
        backup_registers: backup,
    };
    assert_eq!(cp.validate(), Ok(dt(2026, 10, 7, 21, 43, 5)));
    h.advance_to(123 * MS);
    h.with::<Rtc, _>(id, |rtc, ctx| rtc.restore_checkpoint(ctx, &cp)).unwrap();
    assert_eq!(calendar(&h, id), dt(2026, 10, 7, 21, 43, 5));
    assert_eq!(h.get::<Rtc>(id).checkpoint(), cp, "export returns what was restored");
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    // The calendar restarts from a whole second at the time of the call.
    h.advance_to(123 * MS + SEC - 1);
    assert_eq!(calendar(&h, id).second, 5);
    h.advance_to(123 * MS + SEC);
    assert_eq!(calendar(&h, id).second, 6);
    // Locked again afterwards.
    w(&mut h, reg::ISR, 0x80);
    assert!(h.warnings().iter().any(|m| m.contains("ISR is allowed only when the register is unlocked")));
}

#[test]
fn checkpoint_12_hour_format_and_validation_rules() {
    let (mut h, id) = rtc(32768);
    let cp = RtcCheckpoint {
        time_register: time_register(4, 30, 0, true),
        date_register: date_register(2026, 10, 7, 3),
        prescaler_register: 0x007F_00FF,
        format_12_hour: true,
        backup_registers: [0; BACKUP_WORDS],
    };
    assert_eq!(cp.validate(), Ok(dt(2026, 10, 7, 16, 30, 0)));
    h.with::<Rtc, _>(id, |rtc, ctx| rtc.restore_checkpoint(ctx, &cp)).unwrap();
    assert_eq!(h.get::<Rtc>(id).checkpoint(), cp);
    assert_eq!(calendar(&h, id).hour, 16);
    // Rejections mirror rtc_persistence.py and happen before any register is touched.
    let bad = |f: &dyn Fn(&mut RtcCheckpoint)| {
        let mut c = cp.clone();
        f(&mut c);
        c.validate().unwrap_err()
    };
    assert!(bad(&|c| c.time_register |= 0xA).contains("BCD"));
    assert!(bad(&|c| c.time_register |= 0x80).contains("reserved"));
    assert!(bad(&|c| c.time_register = time_register(13, 0, 0, false)).contains("12-hour"));
    assert!(bad(&|c| c.format_12_hour = false).contains("24-hour"));
    assert!(bad(&|c| c.date_register = date_register(2026, 10, 7, 0)).contains("weekday"));
    assert!(bad(&|c| c.date_register = date_register(2026, 2, 30, 3)).contains("calendar"));
    assert!(bad(&|c| c.prescaler_register |= 0x8000).contains("prescaler"));
    let before = h.get::<Rtc>(id).checkpoint();
    let mut broken = cp.clone();
    broken.time_register |= 0xA;
    assert!(h.with::<Rtc, _>(id, |rtc, ctx| rtc.restore_checkpoint(ctx, &broken)).is_err());
    assert_eq!(h.get::<Rtc>(id).checkpoint(), before);
}

#[test]
fn byte_and_halfword_accesses_are_not_supported() {
    let (mut h, _) = rtc(32768);
    assert_eq!(h.read8(BASE + reg::DR), 0);
    assert_eq!(h.read16(BASE + reg::DR), 0);
    h.write8(BASE + reg::BKP0R, 0x11);
    assert_eq!(r(&mut h, reg::BKP0R), 0);
    assert_eq!(h.warnings().len(), 3, "{:?}", h.warnings());
    assert!(h.warnings()[0].contains("Attempted Byte read isn't supported"), "{:?}", h.warnings());
}

#[test]
fn summary_shows_the_calendar() {
    let (mut h, _) = rtc(32768);
    set_calendar(&mut h, date_register(2026, 10, 7, 3), time_register(8, 9, 10, false));
    let summaries = h.core().summaries();
    let (_, text) = summaries.iter().find(|(n, _)| n == "rtc").unwrap();
    assert!(text.contains("2026-10-07 08:09:10") && text.contains("ticking=true"), "{text}");
}
