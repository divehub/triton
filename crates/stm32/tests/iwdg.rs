//! `Timers.STM32_IndependentWatchdog` on a bare machine.

use emu_core::testing::Harness;
use emu_core::{PeriphId, Time, TICKS_PER_MILLISECOND};
use stm32::iwdg::{key, reg, Iwdg};

const MS: Time = TICKS_PER_MILLISECOND;
const BASE: u32 = 0x4000_3000;

fn watchdog() -> (Harness, PeriphId) {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, Iwdg::ngc("iwdg"));
    (h, id)
}

fn w(h: &mut Harness, offset: u32, value: u32) {
    h.write32(BASE + offset, value);
}

fn r(h: &mut Harness, offset: u32) -> u32 {
    h.read32(BASE + offset)
}

fn unlock(h: &mut Harness) {
    w(h, reg::KEY, key::UNLOCK);
}

#[test]
fn reset_values() {
    let (mut h, id) = watchdog();
    assert_eq!(r(&mut h, reg::KEY), 0, "KEY is write only");
    assert_eq!(r(&mut h, reg::PR), 0);
    assert_eq!(r(&mut h, reg::RLR), 0xFFF);
    assert_eq!(r(&mut h, reg::SR), 0);
    assert_eq!(r(&mut h, reg::WINR), 0xFFF);
    assert!(h.warnings().is_empty());
    assert!(!h.get::<Iwdg>(id).reset_requested());
    assert_eq!(h.next_event_time(), None, "the watchdog does not run until started");
}

#[test]
fn started_and_never_reloaded_expires_after_4095_ticks_of_8_khz() {
    let (mut h, id) = watchdog();
    w(&mut h, reg::KEY, key::START);
    assert!(h.take_stop_request(), "LimitTimer.Enabled requests a return");
    // 32 kHz / 4 = 8 kHz: 4095 ticks = 511.875 ms.
    assert_eq!(h.next_event_time(), Some(511_875_000));
    h.advance_to(511_875_000 - 1);
    assert!(!h.get::<Iwdg>(id).reset_requested());
    h.advance_to(511_875_000);
    assert!(h.get::<Iwdg>(id).reset_requested());
    assert_eq!(h.get_mut::<Iwdg>(id).take_reset_request(), Some(511_875_000));
    assert!(!h.get::<Iwdg>(id).reset_requested());
    assert_eq!(h.get::<Iwdg>(id).reset_request_count(), 1);
    assert!(h.warnings().iter().any(|m| m.contains("Watchdog reset triggered!")), "{:?}", h.warnings());
    // One-shot: it does not fire again.
    h.advance_to(2_000 * MS);
    assert_eq!(h.get::<Iwdg>(id).reset_request_count(), 1);
}

#[test]
fn reload_restarts_the_countdown() {
    let (mut h, id) = watchdog();
    w(&mut h, reg::KEY, key::START);
    h.advance_to(400 * MS);
    w(&mut h, reg::KEY, key::RELOAD);
    assert_eq!(h.next_event_time(), Some(400 * MS + 511_875_000));
    h.advance_to(900 * MS);
    assert!(!h.get::<Iwdg>(id).reset_requested());
    w(&mut h, reg::KEY, key::RELOAD);
    h.advance_to(900 * MS + 511_874_999);
    assert!(!h.get::<Iwdg>(id).reset_requested());
    h.advance_to(900 * MS + 511_875_000);
    assert!(h.get::<Iwdg>(id).reset_requested());
}

#[test]
fn prescaler_and_reload_value_need_the_unlock_key() {
    let (mut h, id) = watchdog();
    w(&mut h, reg::PR, 1);
    assert_eq!(h.warnings().len(), 1);
    assert!(h.warnings()[0].contains("without unlocking"), "{:?}", h.warnings());
    assert_eq!(r(&mut h, reg::PR), 1, "the stored value changes even when the write is rejected");
    w(&mut h, reg::KEY, key::START);
    assert_eq!(h.next_event_time(), Some(511_875_000), "the divider stayed 4");
    // Unlock, then PR = 1 -> divider 8 (4 kHz): the running countdown is not restarted, its rate changes.
    unlock(&mut h);
    w(&mut h, reg::PR, 1);
    assert!(h.with::<Iwdg, _>(id, |wd, ctx| wd.running(&*ctx)));
    // The full period at divider 8 would be 1.02375 s; the remaining ticks were rescaled from now (time 0).
    assert_eq!(h.next_event_time(), Some(1_023_750_000));
    // The unlock lasts until the next KEY write: PR is accepted again, a KEY write locks.
    w(&mut h, reg::PR, 0);
    assert_eq!(h.next_event_time(), Some(511_875_000));
    w(&mut h, reg::KEY, 0);
    w(&mut h, reg::PR, 1);
    assert_eq!(h.next_event_time(), Some(511_875_000), "locked again");
}

#[test]
fn prescaler_divider_is_capped_at_256() {
    for (pr, divider) in [(0u32, 4u64), (1, 8), (2, 16), (3, 32), (4, 64), (5, 128), (6, 256), (7, 256)] {
        let (mut h, _) = watchdog();
        unlock(&mut h);
        w(&mut h, reg::PR, pr);
        w(&mut h, reg::KEY, key::START);
        // 4095 ticks at 32000 / divider Hz, rounded up to a nanosecond.
        let exact_ns = 4095u64 * divider * 1_000_000_000 / 32_000;
        assert_eq!(h.next_event_time(), Some(exact_ns), "PR = {pr}");
    }
}

#[test]
fn reload_register_applies_at_the_next_reload() {
    let (mut h, id) = watchdog();
    unlock(&mut h);
    w(&mut h, reg::RLR, 100);
    assert_eq!(r(&mut h, reg::RLR), 100);
    w(&mut h, reg::KEY, key::START);
    assert_eq!(h.next_event_time(), Some(511_875_000), "starting uses the limit of the reset");
    w(&mut h, reg::KEY, key::RELOAD);
    // 100 ticks at 8 kHz = 12.5 ms.
    assert_eq!(h.next_event_time(), Some(12 * MS + 500_000));
    h.advance_to(12 * MS + 500_000);
    assert!(h.get::<Iwdg>(id).reset_requested());
}

#[test]
fn window_violation_requests_a_reset_and_a_valid_reload_does_not() {
    let (mut h, id) = watchdog();
    unlock(&mut h);
    w(&mut h, reg::WINR, 2000); // window = 2000 (also reloads)
    w(&mut h, reg::KEY, key::START);
    // The counter counts down from 4095; a reload while it is above 2000 violates the window.
    h.advance_to(10 * MS); // 4095 - 80 = 4015 > 2000
    w(&mut h, reg::KEY, key::RELOAD);
    assert!(h.get::<Iwdg>(id).reset_requested());
    assert!(h.warnings().iter().any(|m| m.contains("outside of window")), "{:?}", h.warnings());
    h.get_mut::<Iwdg>(id).take_reset_request();
    // After ~262 ms the counter is below 2000: the reload is accepted.
    h.advance_to(10 * MS + 270 * MS);
    w(&mut h, reg::KEY, key::RELOAD);
    assert!(!h.get::<Iwdg>(id).reset_requested());
}

#[test]
fn unlock_is_consumed_by_every_key_write_and_reserved_bits_warn() {
    let (mut h, _) = watchdog();
    unlock(&mut h);
    w(&mut h, reg::KEY, 0x1234); // any other key locks again
    w(&mut h, reg::RLR, 5);
    assert!(h.warnings().iter().any(|m| m.contains("reload value without unlocking")));
    w(&mut h, reg::KEY, 0x1_5555);
    assert!(h.warnings().iter().any(|m| m.contains("offset 0x0.") && m.contains("RESERVED (0x1)")), "{:?}", h.warnings());
    // Unhandled offsets carry the register mapper annotation of BasicDoubleWordPeripheral.
    assert_eq!(r(&mut h, 0x14), 0);
    assert!(h.warnings().iter().any(|m| m.contains("Unhandled read from offset 0x14 (Window+0x4).")), "{:?}", h.warnings());
}

#[test]
fn reset_restores_registers_and_stops_the_countdown() {
    let (mut h, id) = watchdog();
    unlock(&mut h);
    w(&mut h, reg::PR, 3);
    w(&mut h, reg::RLR, 77);
    w(&mut h, reg::KEY, key::START);
    h.core_mut().reset_all();
    assert_eq!(r(&mut h, reg::PR), 0);
    assert_eq!(r(&mut h, reg::RLR), 0xFFF);
    assert_eq!(h.next_event_time(), None);
    assert!(!h.with::<Iwdg, _>(id, |wd, ctx| wd.running(&*ctx)));
}

#[test]
fn summary_describes_the_state() {
    let (mut h, _) = watchdog();
    w(&mut h, reg::KEY, key::START);
    let summaries = h.core().summaries();
    let (_, text) = summaries.iter().find(|(n, _)| n == "iwdg").unwrap();
    assert!(text.contains("running=true") && text.contains("divider=4"), "{text}");
}
