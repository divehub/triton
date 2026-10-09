//! `Stm32Timer::with_external_pull_ups`: the handset's TIM3 sees its button pins (`PE3` / `PE5`, high from reset through an external
//! pull-up) at their real level when the firmware configures the capture, without a capture edge. Renode's `OnGPIO` drops an input
//! received while the channel is an output and forgets its level, so the plain timer (`Stm32Timer::new`, the instance every Renode
//! transcript is replayed against) believes the pin low and loses the first press edge; the extension is opt-in and changes nothing
//! else (the transcripts of `renode_timer` still pass).

use emu_core::testing::Harness;
use emu_core::{PeriphId, Time, TICKS_PER_MILLISECOND};
use stm32::timer::{reg, Stm32Timer};

const BASE: u32 = 0x4000_0400;
const MS: Time = TICKS_PER_MILLISECOND;
/// `SR` bits of the capture flags CC1IF..CC4IF.
const CC1IF: u32 = 1 << 1;
const CC2IF: u32 = 1 << 2;
const CC3IF: u32 = 1 << 3;
const CC4IF: u32 = 1 << 4;

struct Rig {
    h: Harness,
    id: PeriphId,
}

impl Rig {
    fn new(timer: Stm32Timer) -> Rig {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, timer);
        Rig { h, id }
    }

    fn w(&mut self, offset: u32, value: u32) {
        self.h.write32(BASE + offset, value);
    }

    fn r(&mut self, offset: u32) -> u32 {
        self.h.read32(BASE + offset)
    }

    /// The firmware's TIM3 setup: 80 MHz / 65536, channels 1/2 and 3/4 capturing TI1 / TI3 (channel 2 and 4 on the other polarity), the
    /// capture interrupts enabled. `ccer` is the capture enable register as the firmware wrote it.
    fn configure_capture(&mut self, ccer: u32) {
        self.w(reg::PSC, 65_535);
        self.w(reg::ARR, 0xFFFF);
        self.w(reg::CCMR1, 0x0201);
        self.w(reg::CCMR2, 0x0201);
        self.w(reg::CCER, ccer);
        self.w(reg::DIER, 0x1E);
        self.w(reg::CR1, 1);
    }

    fn flags(&mut self) -> u32 {
        self.r(reg::SR) & 0x1E
    }
}

/// CC1E, CC2E with CC2P (falling), CC3E, CC4E with CC4P.
const CCER_ALL: u32 = 1 | 1 << 4 | 1 << 5 | 1 << 8 | 1 << 12 | 1 << 13;

fn plain() -> Stm32Timer {
    Stm32Timer::new("timer3", 80_000_000, 0xFFFF)
}

fn pull_up() -> Stm32Timer {
    plain().with_external_pull_ups()
}

#[test]
fn a_plain_timer_forgets_a_pin_that_was_high_before_the_capture_and_misses_the_first_edge() {
    let mut rig = Rig::new(plain());
    rig.h.set_input(rig.id, 0, true);
    rig.h.advance_to(MS);
    rig.configure_capture(CCER_ALL);
    rig.h.advance_to(5 * MS);
    assert_eq!(rig.flags(), 0, "nothing was captured at the configuration");
    // The press (falling edge) is lost: the timer thinks the pin was low already. The release is then seen as a rising edge.
    rig.h.set_input(rig.id, 0, false);
    assert_eq!(rig.flags(), 0, "Renode: the falling edge is not an edge for a timer that never saw the pin high");
    rig.h.advance_to(10 * MS);
    rig.h.set_input(rig.id, 0, true);
    assert_eq!(rig.flags() & CC1IF, CC1IF, "the release looks like the first edge");
}

#[test]
fn with_external_pull_ups_the_pin_is_high_at_the_configuration_without_an_edge_and_the_press_is_captured() {
    let mut rig = Rig::new(pull_up());
    rig.h.set_input(rig.id, 0, true);
    rig.h.set_input(rig.id, 2, true);
    rig.h.advance_to(MS);
    rig.configure_capture(CCER_ALL);
    rig.h.advance_to(5 * MS);
    assert_eq!(rig.flags(), 0, "no edge when the capture is configured with the pins already high");
    // Press on line 0: channel 2 (the falling-edge channel of TI1) captures, channel 1 does not.
    rig.h.set_input(rig.id, 0, false);
    assert_eq!(rig.flags(), CC2IF);
    let falling = rig.r(reg::CCR2);
    rig.h.advance_to(210 * MS);
    rig.h.set_input(rig.id, 0, true);
    assert_eq!(rig.flags(), CC1IF, "the release is the rising edge of channel 1 (the read of CCR2 cleared CC2IF)");
    let rising = rig.r(reg::CCR1);
    let width = rising.wrapping_sub(falling) & 0xFFFF;
    assert!((250..=257).contains(&width), "205 ms at 1220 Hz: {width}");
    // The other pair never moved.
    assert_eq!(rig.r(reg::CCR3), 0);
    assert_eq!(rig.r(reg::CCR4), 0);
    assert!(rig.h.warnings().is_empty(), "{:?}", rig.h.warnings());
}

#[test]
fn a_disabled_channel_does_not_hide_the_level_of_its_pin_from_the_other_pair() {
    // The firmware leaves CC3E / CC4E clear: the Renode model then forces its pin record low, which must not turn the next press of the
    // other line into an edge of channel 3.
    let mut rig = Rig::new(pull_up());
    rig.h.set_input(rig.id, 0, true);
    rig.h.set_input(rig.id, 2, true);
    rig.configure_capture(1 | 1 << 4 | 1 << 5);
    rig.h.advance_to(5 * MS);
    rig.h.set_input(rig.id, 0, false);
    assert_eq!(rig.flags() & (CC3IF | CC4IF), 0, "line 2 is still high");
    rig.h.advance_to(10 * MS);
    rig.h.set_input(rig.id, 0, true);
    assert_eq!(rig.flags() & (CC3IF | CC4IF), 0);
}

#[test]
fn the_remembered_level_survives_a_timer_reset_and_a_pin_nobody_drove_behaves_as_before() {
    let mut rig = Rig::new(pull_up());
    rig.h.set_input(rig.id, 0, true);
    rig.configure_capture(CCER_ALL);
    rig.h.advance_to(MS);
    // A machine reset clears the timer; the pin is still held high by the pull-up.
    rig.h.core_mut().reset_all();
    rig.h.advance_to(2 * MS);
    rig.configure_capture(CCER_ALL);
    rig.h.advance_to(5 * MS);
    assert_eq!(rig.flags(), 0, "no edge after the reconfiguration either");
    rig.h.set_input(rig.id, 0, false);
    assert_eq!(rig.flags(), CC2IF);
    // Line 2 was never driven: the first thing the timer sees is a rising edge, as with the plain timer.
    rig.h.set_input(rig.id, 2, true);
    assert_eq!(rig.flags() & CC3IF, CC3IF);
}
