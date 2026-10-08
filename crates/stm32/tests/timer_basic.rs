//! `Timers.STM32_Timer`: registers, update/compare timing, flags and interrupt lines on a bare machine
//! (`emu_core::testing::Harness`, no CPU). Expected times follow Renode's ceil-nanosecond `LimitTimer` rules.

use emu_core::testing::{Harness, IrqChange};
use emu_core::{PeriphId, Time, Width, TICKS_PER_MICROSECOND, TICKS_PER_MILLISECOND};
use stm32::timer::{reg, Stm32Timer, IRQ_LINE, PIN_LINE_BASE};

const US: Time = TICKS_PER_MICROSECOND;
const MS: Time = TICKS_PER_MILLISECOND;
const BASE: u32 = 0x4000_1000;
const NVIC: u32 = 54;

fn timer(limit: u32) -> (Harness, PeriphId) {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, Stm32Timer::new("timer", 80_000_000, limit));
    h.connect_irq(id, IRQ_LINE, NVIC);
    h.clear_irq_changes();
    (h, id)
}

fn w(h: &mut Harness, offset: u32, value: u32) {
    h.write32(BASE + offset, value);
}

fn r(h: &mut Harness, offset: u32) -> u32 {
    h.read32(BASE + offset)
}

/// ARR/PSC/DIER/CEN as the HAL tick initialisation does it.
fn start_tick(h: &mut Harness, arr: u32, psc: u32, dier: u32) {
    w(h, reg::ARR, arr);
    w(h, reg::PSC, psc);
    w(h, reg::DIER, dier);
    w(h, reg::CR1, 1);
}

#[test]
fn reset_state_reads_zero_except_arr_and_the_compare_limits() {
    let (mut h, _) = timer(0xFFFF);
    for offset in (0..=0x44).step_by(4) {
        // CCRx reads the compare timer's limit, which starts at `initialLimit`.
        let expected = if offset == reg::ARR || (reg::CCR1..=reg::CCR4).contains(&offset) { 0xFFFF } else { 0 };
        assert_eq!(r(&mut h, offset), expected, "register 0x{offset:02X}");
    }
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
    let (mut h32, _) = timer(0xFFFF_FFFF);
    assert_eq!(r(&mut h32, reg::ARR), 0xFFFF_FFFF);
    assert_eq!(r(&mut h32, reg::CCR3), 0xFFFF_FFFF);
}

#[test]
fn undefined_registers_warn_once_and_read_zero() {
    let (mut h, _) = timer(0xFFFF);
    assert_eq!(r(&mut h, 0x48), 0);
    assert_eq!(r(&mut h, 0x48), 0);
    assert_eq!(r(&mut h, 0x100), 0);
    w(&mut h, 0x50, 0x1234);
    let warnings = h.warnings();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings[0].contains("Unhandled read from offset 0x48"));
    assert!(warnings[2].contains("Unhandled write to offset 0x50, value 0x1234"));
}

#[test]
fn hal_tick_interrupts_at_exact_nanoseconds() {
    let (mut h, id) = timer(0xFFFF);
    start_tick(&mut h, 999, 79, 1);
    assert!(h.take_stop_request(), "CEN/ARR/PSC writes are LimitTimer setters: they ask the CPU to return");
    // One tick of 80 MHz / 80 = 1 MHz: 999 ticks = 999 000 ns.
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(999 * US));
    h.advance_to(999 * US - 1);
    assert_eq!(r(&mut h, reg::SR), 0);
    assert!(h.irq_changes().is_empty());
    h.advance_to(999 * US);
    assert_eq!(h.irq_changes(), [IrqChange { time: 999 * US, irq: NVIC, level: true }]);
    assert_eq!(r(&mut h, reg::SR), 1);
    // Every later period is 999 us again; the line stays high until UIF is cleared.
    h.advance_to(3 * MS);
    assert_eq!(h.irq_changes().len(), 1);
    w(&mut h, reg::SR, !1);
    assert_eq!(h.irq_changes().last(), Some(&IrqChange { time: 3 * MS, irq: NVIC, level: false }));
    assert_eq!(r(&mut h, reg::SR), 0);
    // 3 ms is 3 periods and 3 us: the next event is at 4 * 999 us.
    h.advance_to(4 * 999 * US);
    assert_eq!(h.irq_changes().last(), Some(&IrqChange { time: 4 * 999 * US, irq: NVIC, level: true }));
    assert_eq!(h.irq_changes().len(), 3);
}

#[test]
fn counter_wraps_at_arr_not_arr_plus_one() {
    let (mut h, _) = timer(0xFFFF);
    start_tick(&mut h, 99, 79, 0);
    // 1 MHz: after 99 us the counter wraps to 0; CNT counts 0..=98.
    h.advance_to(98 * US);
    assert_eq!(r(&mut h, reg::CNT), 98);
    h.advance_to(99 * US);
    assert_eq!(r(&mut h, reg::CNT), 0);
    h.advance_to(99 * US + 500);
    assert_eq!(r(&mut h, reg::CNT), 0);
    h.advance_to(100 * US);
    assert_eq!(r(&mut h, reg::CNT), 1);
}

#[test]
fn ceil_nanosecond_periods_do_not_accumulate_fractions() {
    // ARR = 1001 at 80 MHz: 12 512.5 ns exact, 12 513 ns in Renode, every period (micro vector tim6-ceil-arr1001-psc0).
    let (mut h, id) = timer(0xFFFF);
    start_tick(&mut h, 1001, 0, 1);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(12_513));
    for k in 1..=40u64 {
        h.advance_to(k * 12_513);
        assert_eq!(h.irq_changes().len() as u64, 2 * k - 1, "period {k}");
        assert_eq!(h.irq_changes().last(), Some(&IrqChange { time: k * 12_513, irq: NVIC, level: true }));
        w(&mut h, reg::SR, 0);
    }
    // PSC = 2: 80 MHz / 3 = 26 666 666 Hz by integer division; 1001 ticks = 37 537.5 ns -> 37 538 ns.
    let (mut h, id) = timer(0xFFFF);
    start_tick(&mut h, 1001, 2, 1);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(37_538));
}

#[test]
fn psc_and_arr_buffering() {
    let (mut h, _) = timer(0xFFFF);
    // ARR written without preload applies at once.
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 50);
    w(&mut h, reg::CR1, 1);
    h.advance_to(30 * US);
    w(&mut h, reg::ARR, 100);
    h.advance_to(99 * US);
    assert_eq!(r(&mut h, reg::CNT), 99, "the longer period applies immediately");
    h.advance_to(100 * US);
    assert_eq!(r(&mut h, reg::CNT), 0);
    // With ARPE the new value waits for the update event.
    w(&mut h, reg::CR1, 1 | 0x80);
    w(&mut h, reg::ARR, 10);
    h.advance_to(100 * US + 99 * US);
    assert_eq!(r(&mut h, reg::CNT), 99, "the old period is still running");
    h.advance_to(200 * US);
    assert_eq!(r(&mut h, reg::CNT), 0);
    h.advance_to(209 * US);
    assert_eq!(r(&mut h, reg::CNT), 9, "after the update event the shorter period is in effect");
    h.advance_to(210 * US);
    assert_eq!(r(&mut h, reg::CNT), 0);
    assert_eq!(r(&mut h, reg::ARR), 10);
}

#[test]
fn cen_needs_a_nonzero_reload_value() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::ARR, 0);
    w(&mut h, reg::CR1, 1);
    assert_eq!(r(&mut h, reg::CR1), 1, "CEN reads back the request");
    h.advance_to(MS);
    assert_eq!(r(&mut h, reg::CNT), 0, "but the counter does not run");
    w(&mut h, reg::ARR, 5);
    w(&mut h, reg::PSC, 79);
    h.advance_to(MS + 3 * US);
    assert_eq!(r(&mut h, reg::CNT), 3, "ARR != 0 starts the counter at the ARR write (the PSC write follows at the same instant)");
    h.advance_to(MS + 6 * US);
    assert_eq!(r(&mut h, reg::CNT), 1, "wrapped at 5 ticks");
}

#[test]
fn ug_resets_counter_keeps_residuum_and_flags_the_update() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 1000);
    w(&mut h, reg::DIER, 1);
    w(&mut h, reg::CR1, 1);
    h.advance_to(10 * US + 400);
    assert_eq!(r(&mut h, reg::CNT), 10);
    w(&mut h, reg::EGR, 1);
    assert_eq!(r(&mut h, reg::CNT), 0);
    assert_eq!(r(&mut h, reg::SR), 1, "URS = 0 and UIE = 1: UG sets UIF");
    assert!(h.irq_level(NVIC));
    // The 0.4 tick of residuum was kept: the next whole tick arrives 600 ns later.
    h.advance_to(10 * US + 400 + 599);
    assert_eq!(r(&mut h, reg::CNT), 0);
    h.advance_to(10 * US + 400 + 600);
    assert_eq!(r(&mut h, reg::CNT), 1);
}

#[test]
fn urs_and_udis_gate_the_update_event() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::DIER, 1);
    w(&mut h, reg::CR1, 1 << 2); // URS
    w(&mut h, reg::EGR, 1);
    assert_eq!(r(&mut h, reg::SR), 0, "URS: UG does not flag the update");
    w(&mut h, reg::CR1, 1 << 1); // UDIS
    w(&mut h, reg::CNT, 77);
    w(&mut h, reg::EGR, 1);
    assert_eq!(r(&mut h, reg::CNT), 77, "UDIS: UG does nothing");
    // Renode parity: the UG callback ignores the written bit, so writing zero to EGR generates an update too.
    w(&mut h, reg::CR1, 0);
    w(&mut h, reg::EGR, 0);
    assert_eq!(r(&mut h, reg::CNT), 0);
    assert_eq!(r(&mut h, reg::SR), 1);
}

#[test]
fn one_pulse_mode_stops_at_the_update() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 10);
    w(&mut h, reg::CR1, 1 | 1 << 3);
    assert_eq!(r(&mut h, reg::CR1), 0x9);
    h.advance_to(10 * US - 1);
    assert_eq!(r(&mut h, reg::CR1), 0x9);
    assert_eq!(r(&mut h, reg::CNT), 9);
    h.advance_to(10 * US);
    assert_eq!(r(&mut h, reg::CR1), 0x8, "CEN is cleared by the update event");
    assert_eq!(r(&mut h, reg::CNT), 0);
    h.advance_to(MS);
    assert_eq!(r(&mut h, reg::CNT), 0, "the counter stopped");
}

#[test]
fn counting_down_reloads_with_arr() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 100);
    w(&mut h, reg::CNT, 10);
    w(&mut h, reg::CR1, 1 | 1 << 4); // CEN | DIR
    assert_eq!(r(&mut h, reg::CR1), 0x11);
    h.advance_to(4 * US);
    assert_eq!(r(&mut h, reg::CNT), 6);
    h.advance_to(10 * US);
    assert_eq!(r(&mut h, reg::CNT), 100, "limit reached when 10 ticks elapsed: reloaded with the period");
    h.advance_to(20 * US);
    assert_eq!(r(&mut h, reg::CNT), 90);
}

#[test]
fn cnt_write_and_byte_access_translation() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::CNT, 0x1234);
    assert_eq!(r(&mut h, reg::CNT), 0x1234);
    // Byte and halfword accesses are read-modify-write word accesses (AllowedTranslations).
    assert_eq!(h.read8(BASE + reg::CNT), 0x34);
    assert_eq!(h.read16(BASE + reg::CNT), 0x1234);
    h.write8(BASE + reg::CNT + 1, 0x56);
    assert_eq!(r(&mut h, reg::CNT), 0x5634);
    h.write16(BASE + reg::ARR, 0x0777);
    assert_eq!(r(&mut h, reg::ARR), 0x0777);
    // Bits above the counter width are reserved: they log the Renode warning and are dropped.
    w(&mut h, reg::CNT, 0x1_0005);
    assert_eq!(r(&mut h, reg::CNT), 5);
    assert!(h.warnings().iter().any(|m| m.contains("Unhandled write to offset 0x24. Unhandled bits: [16]") && m.contains("RESERVED (0x1)")), "{:?}", h.warnings());
}

#[test]
fn status_flags_are_write_zero_to_clear() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::DIER, 1);
    w(&mut h, reg::EGR, 1);
    assert_eq!(r(&mut h, reg::SR), 1);
    w(&mut h, reg::SR, 0xFFFF_FFFF);
    assert_eq!(r(&mut h, reg::SR), 1, "writing 1 keeps UIF");
    w(&mut h, reg::SR, 0xFFFF_FFFE);
    assert_eq!(r(&mut h, reg::SR), 0);
    // The silent tags of SR only log at Noisy level.
    assert!(h.warnings().is_empty(), "{:?}", h.warnings());
}

#[test]
fn unimplemented_tag_bits_warn_like_renode() {
    let (mut h, _) = timer(0xFFFF);
    w(&mut h, reg::CR1, 0x100); // CKD
    w(&mut h, reg::BDTR, 0x8000); // MOE
    assert_eq!(r(&mut h, reg::BDTR), 0, "MOE reads 0");
    w(&mut h, reg::CCMR1, 0x8); // OC1PE
    let warnings = h.warnings();
    assert!(warnings.iter().any(|m| m.contains("offset 0x0.") && m.contains("Clock Division (CKD) (0x1)")), "{warnings:?}");
    assert!(warnings.iter().any(|m| m.contains("offset 0x44.") && m.contains("Main Output Enable (MOE) (0x1)")), "{warnings:?}");
    assert!(warnings.iter().any(|m| m.contains("offset 0x18.") && m.contains("Output compare 1 preload enable (OC1PE) (0x1)")), "{warnings:?}");
}

#[test]
fn compare_channel_drives_pwm_outputs_and_flags() {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, Stm32Timer::new("timer", 80_000_000, 0xFFFF).with_observed_pins(0b0010));
    h.connect_irq(id, IRQ_LINE, NVIC);
    let pin = h.probe(id, PIN_LINE_BASE + 1);
    h.clear_irq_changes();
    // Channel 2: PWM mode 1, CCR 25 of ARR 100 at 1 MHz.
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 100);
    w(&mut h, reg::CCMR1, 6 << 12);
    w(&mut h, reg::CCER, 1 << 4);
    w(&mut h, reg::CCR2, 25);
    assert_eq!(r(&mut h, reg::CCR2), 25);
    w(&mut h, reg::CR1, 1);
    h.advance_to(350 * US);
    // High at each update (100, 200, 300 us), low at the compare match (25 us later).
    let want: Vec<(Time, bool)> = vec![
        (25 * US, false),
        (100 * US, true),
        (125 * US, false),
        (200 * US, true),
        (225 * US, false),
        (300 * US, true),
        (325 * US, false),
    ];
    let got = h.probe_changes(pin);
    // The first compare match at 25 us finds the pin low already (never set): no edge.
    assert_eq!(got, want[1..].to_vec(), "{got:?}");
    assert!(h.irq_changes().is_empty(), "DIER = 0: no interrupt");
    // Without CCxIE the compare flag is never set.
    assert_eq!(r(&mut h, reg::SR), 1);
}

#[test]
fn compare_interrupt_sets_ccxif_at_the_match() {
    let (mut h, id) = timer(0xFFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 100);
    w(&mut h, reg::CCMR1, 6 << 4);
    w(&mut h, reg::CCER, 1);
    w(&mut h, reg::CCR1, 40);
    w(&mut h, reg::DIER, 1 << 1); // CC1IE
    w(&mut h, reg::CR1, 1);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(40 * US));
    h.advance_to(40 * US - 1);
    assert_eq!(r(&mut h, reg::SR), 0);
    h.advance_to(40 * US);
    assert_eq!(r(&mut h, reg::SR), 0x2);
    assert_eq!(h.irq_changes(), [IrqChange { time: 40 * US, irq: NVIC, level: true }]);
    w(&mut h, reg::SR, !2);
    assert!(!h.irq_level(NVIC));
    // The compare timer is one-shot: it disabled itself at the match, yet Renode's `nearestLimitIn` keeps its
    // would-be next limit one compare period later (40 us -> 80 us), the "phantom" limit that still ends a CPU
    // chunk, until some update pass removes it. Then the update event (100 us) re-arms the compare timer, and
    // its match follows at 140 us (with another phantom at 180 us).
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(80 * US));
    h.advance_to(80 * US);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(100 * US));
    h.advance_to(100 * US);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(140 * US));
    h.advance_to(140 * US);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(180 * US));
}

#[test]
fn ccr_zero_disables_the_compare_timer() {
    let (mut h, id) = timer(0xFFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::ARR, 100);
    w(&mut h, reg::CCMR1, 6 << 4);
    w(&mut h, reg::CCER, 1);
    w(&mut h, reg::DIER, 1 << 1);
    w(&mut h, reg::CR1, 1);
    w(&mut h, reg::CCR1, 0);
    // CCR = 0: no compare event, only the counter's own limit (100 us) remains.
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(100 * US));
    w(&mut h, reg::CCR1, 50);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), Some(50 * US));
}

#[test]
fn reset_input_restores_the_reset_state() {
    let (mut h, id) = timer(0xFFFF);
    start_tick(&mut h, 10, 79, 1);
    h.advance_to(25 * US);
    assert!(h.irq_level(NVIC));
    h.set_input(id, stm32::timer::RESET_INPUT, true);
    assert!(!h.irq_level(NVIC));
    assert_eq!(r(&mut h, reg::ARR), 0xFFFF);
    assert_eq!(r(&mut h, reg::CR1), 0);
    assert_eq!(r(&mut h, reg::PSC), 0);
    assert_eq!(r(&mut h, reg::SR), 0);
    h.advance_to(MS);
    assert_eq!(r(&mut h, reg::CNT), 0);
    assert_eq!(h.get::<Stm32Timer>(id).armed_alarm(), None);
}

#[test]
fn cnt_is_a_sync_register_and_other_registers_see_the_lagging_clock() {
    // The CPU runs a chunk from 0: the 999 us limit lies inside it. A CNT read at 1.2 ms syncs the clock to
    // the exact time (the limit fires, UIF is set), an SR read before that still sees the old clock time.
    let (mut h, _) = timer(0xFFFF);
    start_tick(&mut h, 999, 79, 1);
    assert_eq!(h.cpu_read32(BASE + reg::SR, 1200 * US), 0, "the clock still stands at 0");
    assert_eq!(h.cpu_read32(BASE + reg::CNT, 1200 * US), 201, "synced to 1.2 ms: 201 ticks into the second period");
    assert_eq!(h.cpu_read32(BASE + reg::SR, 1200 * US), 1, "the update event ran during the sync");
    h.end_chunk(2 * MS);
}

#[test]
fn width_of_the_counter_follows_initial_limit() {
    let (mut h, _) = timer(0xFFFF_FFFF);
    w(&mut h, reg::PSC, 79);
    w(&mut h, reg::CNT, 0x1_0000);
    assert_eq!(r(&mut h, reg::CNT), 0x1_0000);
    w(&mut h, reg::ARR, 0xFFFF_FFFE);
    assert_eq!(r(&mut h, reg::ARR), 0xFFFF_FFFE);
    w(&mut h, reg::CR1, 1);
    h.advance_to(5 * US);
    assert_eq!(r(&mut h, reg::CNT), 0x1_0005);
    let _ = Width::Word;
}
