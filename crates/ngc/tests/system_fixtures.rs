//! Runner-parity behavior of the dual `System` (work package SYS-CORE): quantum scheduling, the handset
//! power gate with its one-quantum release delay, the standby request, restarts and determinism against host
//! chunking. These tests use the real firmware images and are skipped when the (gitignored) SREC files are not
//! available.

use emu_core::{from_millis, from_micros, Width, QUANTUM};
use ngc::firmware::{self, Firmware, Role};
use ngc::system::{BootMode, Input, Mode, System, SystemConfig, Which};
use std::path::PathBuf;

fn firmware_dir() -> Option<PathBuf> {
    // `NGC_FIRMWARE_DIR` (the directory that holds the release directories), else the repository's `firmware/`.
    let roots = [std::env::var_os("NGC_FIRMWARE_DIR").map(PathBuf::from), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware"))];
    roots
        .into_iter()
        .flatten()
        .map(|root| root.join("TRITON-5.8-65.3"))
        .find(|d| d.join("ngc_main_5.8_TRITON.srec").is_file() && d.join("ngc_handset_65.3_TRITON.srec").is_file())
}

fn images() -> Option<(Firmware, Firmware)> {
    let dir = firmware_dir()?;
    let main = firmware::load(&std::fs::read(dir.join("ngc_main_5.8_TRITON.srec")).ok()?, Some(Role::Main)).ok()?;
    let handset = firmware::load(&std::fs::read(dir.join("ngc_handset_65.3_TRITON.srec")).ok()?, Some(Role::Handset)).ok()?;
    Some((main, handset))
}

fn dual(config: SystemConfig) -> Option<System> {
    let (main, handset) = images()?;
    Some(System::new(config, Some(&main), &handset).expect("system"))
}

#[test]
fn the_dual_system_assembles_and_starts_at_the_reset_vectors() {
    let Some(system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    assert_eq!(system.pc(Which::Main), Some(0x0802_13B8));
    assert_eq!(system.pc(Which::Handset), Some(0x0800_8410));
    assert!(!system.handset_powered(), "the handset is held until the power gate opens");
    assert_eq!(system.time(), 0);
    let main = system.main.as_ref().unwrap();
    // The wake fixture was written through the bus before the first instruction.
    assert_eq!(main.board.peek(0x4000_7010, Width::Word), Some(0x104));
    // The boot fixture wrote the CAN MCR on both boards.
    assert_eq!(system.link.endpoint_count(), 2);
    assert_eq!(system.uart.names(), ["ngc-main.uart4", "ngc-main.usart1", "ngc-main.usart2", "ngc-main.uart5", "ngc-handset.usart3"]);
}

#[test]
fn main_runs_whole_quanta_and_the_handset_waits_for_the_power_gate() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    system.run_until(from_millis(10));
    assert_eq!(system.time(), from_millis(10));
    assert_eq!(system.instructions(Which::Main), Some(1_000_000), "100 MIPS: 1 000 000 instructions in 10 ms");
    assert_eq!(system.instructions(Which::Handset), Some(0), "held halted");
    // A time that is not a multiple of the quantum rounds up.
    system.run_until(from_millis(10) + 1);
    assert_eq!(system.time(), from_millis(10) + QUANTUM);
}

#[test]
fn the_handset_starts_one_quantum_after_the_poll_that_sees_pe3() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    // Raise main PE3 (GPIOE ODR bit 3) in the last quantum before the 50 ms poll.
    system.run_until(from_millis(50) - QUANTUM);
    system.main.as_mut().unwrap().board.bus_write(0x4800_1014, Width::Word, 8);
    assert!(!system.handset_powered());
    system.run_until(from_millis(50));
    assert!(system.handset_powered());
    assert_eq!(system.handset_release_time(), Some(from_millis(50)), "released by the poll at 50 ms");
    assert_eq!(system.instructions(Which::Handset), Some(0), "no handset instruction in the release poll's quantum");
    system.run_until(from_millis(50) + QUANTUM);
    assert_eq!(system.instructions(Which::Handset), Some(0), "the deferred enable takes effect at the next boundary");
    system.run_until(from_millis(50) + 2 * QUANTUM);
    assert_eq!(system.instructions(Which::Handset), Some(10_000), "first handset instruction at 50.1 ms");
    system.run_until(from_millis(60));
    assert_eq!(system.instructions(Which::Handset), Some(((from_millis(60) - from_millis(50) - QUANTUM) / 10) as u64), "(t - 50.1 ms) * 100 MIPS");
}

#[test]
fn simultaneous_start_runs_both_cpus_from_time_zero() {
    let config = SystemConfig { simultaneous_start: true, ..SystemConfig::dual() };
    let Some(mut system) = dual(config) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    assert!(system.handset_powered());
    system.run_until(from_millis(5));
    assert_eq!(system.instructions(Which::Handset), Some(500_000));
    assert_eq!(system.instructions(Which::Main), Some(500_000));
    assert_eq!(system.handset_release_time(), None);
}

#[test]
fn standby_request_halts_both_cpus_at_the_next_poll() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    system.run_until(from_millis(20));
    {
        let board = &mut system.main.as_mut().unwrap().board;
        board.bus_write(0x4000_7000, Width::Word, 3); // PWR CR1: LPMS = standby
        let now = board.now();
        board.cpu.ppb_poke32(0xE000_ED10, 4, now); // SCB.SCR.SLEEPDEEP
    }
    assert!(!system.is_standby());
    system.run_until(from_millis(100));
    assert!(system.is_standby());
    assert_eq!(system.standby_time(), Some(from_millis(50)), "detected by the poll at 50 ms");
    assert!(!system.can_run());
    let before = system.time();
    system.run_until(from_millis(200));
    assert_eq!(system.time(), before, "a system in standby does not advance");
    assert!(system.board(Which::Main).unwrap().cpu.is_halted() && system.board(Which::Handset).unwrap().cpu.is_halted());
}

#[test]
fn how_the_host_chunks_run_calls_does_not_change_the_result() {
    let Some(mut a) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let mut b = dual(SystemConfig::dual()).unwrap();
    a.run_until(from_millis(120));
    for _ in 0..24 {
        b.run_for(from_millis(5));
    }
    for _ in 0..3 {
        b.run_for(from_micros(1)); // each rounds up to one quantum
    }
    a.run_until(b.time());
    assert_eq!(a.time(), b.time());
    assert_eq!(a.fingerprint(), b.fingerprint());
}

#[test]
fn restart_recreates_the_boards_and_keeps_the_eeprom() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    system.run_until(from_millis(10));
    system.main.as_ref().unwrap().eeprom.set_byte(254, 0xA3).unwrap();
    system.main.as_ref().unwrap().eeprom.set_byte(100, 0x42).unwrap();
    system.restart(Some(BootMode::Cold)).unwrap();
    assert_eq!(system.time(), 0);
    assert_eq!(system.instructions(Which::Main), Some(0));
    assert_eq!(system.main.as_ref().unwrap().eeprom.get_byte(100).unwrap(), 0x42, "nonvolatile storage survives a restart");
    assert_eq!(system.boot_mode(), BootMode::Cold);
    // Cold: no wake flags.
    assert_eq!(system.main.as_ref().unwrap().board.peek(0x4000_7010, Width::Word), Some(0));
    system.restart(None).unwrap();
    assert_eq!(system.boot_mode(), BootMode::HandsetWake, "a plain restart uses the configured default");
    assert_eq!(system.main.as_ref().unwrap().board.peek(0x4000_7010, Width::Word), Some(0x104));
    // The serial fixture needs an initialized EEPROM and restarts.
    system.set_serial_number(0x1234_5678).unwrap();
    assert_eq!(system.serial_number(), Some(0x1234_5678));
    assert_eq!(system.time(), 0);
}

#[test]
fn serial_fixture_rejects_an_uninitialized_eeprom() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let error = system.set_serial_number(1).unwrap_err();
    assert!(error.contains("first boot"), "{error}");
}

#[test]
fn handset_only_system_has_no_main_board_and_no_gate() {
    let Some((_, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let mut system = System::new(SystemConfig::handset_only(), None, &handset).unwrap();
    assert_eq!(system.mode(), Mode::HandsetOnly);
    assert!(system.main.is_none() && system.handset_powered());
    system.run_until(from_millis(2));
    assert_eq!(system.instructions(Which::Handset), Some(200_000));
    assert_eq!(system.uart.names(), ["ngc-handset.usart3"]);
    assert!(system.apply_input(&Input::CanConnected(false)).is_err(), "CAN controls need the dual system");
}

#[test]
fn physical_button_inputs_reach_the_handset_button_model_and_its_summary() {
    let Some((_, handset)) = images() else {
        eprintln!("skipping: firmware not available");
        return;
    };
    let mut system = System::new(SystemConfig::handset_only(), None, &handset).unwrap();
    system.run_until(from_millis(400));
    // The runner's `buttons Summary` text, after the firmware configured the TIM3 capture channels.
    assert_eq!(
        system.button_summary(),
        "ready=True; activeMask=0; pendingMask=0; pulses=0; releases=0; PE3=True; PE5=True; gesture=idle; confirmStaggerUs=50000; \
         delayedStartTicks=0; remainingPE3Ticks=0; remainingPE5Ticks=0"
    );
    // A navigation press is a 204.8 ms low pulse on one input; a confirm starts two staggered pulses.
    system.apply_input(&Input::Navigate { up: false }).unwrap();
    system.run_until(from_millis(450));
    let pressed = system.button_summary();
    assert!(pressed.contains("pulses=1") && pressed.contains("activeMask=") && !pressed.contains("activeMask=0;"), "{pressed}");
    system.run_until(from_millis(700));
    let released = system.button_summary();
    assert!(released.contains("releases=1") && released.contains("activeMask=0;"), "{released}");
    system.apply_input(&Input::Confirm).unwrap();
    system.run_until(from_millis(720));
    assert!(!system.button_summary().contains("gesture=idle"), "a confirm gesture is in progress: {}", system.button_summary());
    // Invalid masks are rejected with the model's own message instead of being ignored.
    assert!(system.apply_input(&Input::Press { mask: 0 }).is_err());
    assert!(system.apply_input(&Input::Pulse { mask: 1, duration_us: 0 }).is_err());
}

#[test]
fn hardware_outputs_list_the_backlight_the_vibrator_and_the_hud_channels() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    system.run_until(from_millis(1500));
    let outputs = system.hardware_outputs();
    let ids: Vec<&str> = outputs.as_array().unwrap().iter().map(|o| o.get("id").and_then(|v| v.as_str()).unwrap_or("?")).collect();
    assert_eq!(ids, ["handset-backlight", "handset-vibrator", "main-hud-1", "main-hud-2", "main-hud-3"]);
    let backlight = &outputs.as_array().unwrap()[0];
    assert_eq!(backlight.get("active").and_then(|v| v.as_bool()), Some(true), "the handset firmware turns the backlight on");
    assert!(backlight.get("dutyPercent").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.0);
    // The status document the viewer polls carries the same data.
    let status = system.status_json();
    assert_eq!(status.get("hardwareOutputs"), Some(&outputs));
    assert!(status.get("buttonSummary").and_then(|v| v.as_str()).is_some_and(|s| s.starts_with("ready=")));
    assert!(status.get("lcdSummary").and_then(|v| v.as_str()).is_some_and(|s| s.contains("320x240")));
}

#[test]
fn queued_inputs_are_applied_at_the_first_boundary_at_or_after_their_time() {
    let Some(mut system) = dual(SystemConfig::dual()) else {
        eprintln!("skipping: firmware not available");
        return;
    };
    system.schedule_input(from_millis(1) + 1, Input::CanDropId(0x154));
    system.schedule_input(from_millis(1) + 1, Input::CanConnected(false));
    system.run_until(from_millis(1));
    assert_eq!(system.link.drop_id(), -1);
    system.run_until(from_millis(1) + QUANTUM);
    assert_eq!(system.link.drop_id(), 0x154);
    assert!(!system.link.connected());
    assert_eq!(system.stats().inputs_applied, 2);
    assert!(system.apply_input(&Input::CanDropId(5000)).is_err());
}
