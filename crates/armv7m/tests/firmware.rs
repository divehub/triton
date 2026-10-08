//! Firmware smoke tests: run the original SRECs from their reset state on a flat bus
//! (flash, SRAM1, SRAM2; every other address reads 0) and report how far they get.
//! Skipped gracefully when the firmware files are absent (they are local, never committed).

mod common;
use armv7m::ExitReason;
use common::*;

struct Fw {
    name: &'static str,
    srec: &'static str,
    reset_pc: u32,
}

const MAIN: Fw = Fw { name: "main", srec: "firmware/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec", reset_pc: 0x0802_13b8 };
const HANDSET: Fw = Fw { name: "handset", srec: "firmware/TRITON-5.8-65.3/ngc_handset_65.3_TRITON.srec", reset_pc: 0x0800_8410 };

/// Builds a harness with the firmware loaded into flash (gap bytes inside the span are 0xFF).
fn boot(fw: &Fw) -> Option<Harness> {
    let text = std::fs::read_to_string(repo_path(fw.srec)).ok()?;
    let segs = parse_srec(&text);
    let lo = segs.iter().map(|s| s.0).min()?;
    let hi = segs.iter().map(|s| s.0 + s.1.len() as u32).max()?;
    let mut h = Harness::new();
    for a in lo..hi {
        h.bus.poke8(a, 0xFF);
    }
    for (addr, data) in &segs {
        for (i, b) in data.iter().enumerate() {
            h.bus.flash[(*addr - FLASH_BASE) as usize + i] = *b;
        }
    }
    h.cpu.set_vtor(lo);
    let sp = h.bus.peek32(lo);
    h.cpu.set_sp(sp);
    h.cpu.set_pc(fw.reset_pc);
    Some(h)
}

fn run_slices(h: &mut Harness, total: u64, slice: u64) -> (u64, Option<ExitReason>) {
    let mut done = 0;
    while done < total {
        let exit = h.step(slice.min(total - done));
        done += exit.executed;
        if exit.reason != ExitReason::Deadline {
            return (done, Some(exit.reason));
        }
    }
    (done, None)
}

fn report(h: &mut Harness, fw: &Fw, executed: u64, early: Option<ExitReason>) {
    let now = h.now;
    let cfsr = h.cpu.ppb_peek32(0xE000_ED28, now).unwrap();
    let hfsr = h.cpu.ppb_peek32(0xE000_ED2C, now).unwrap();
    eprintln!("{}: executed {} instructions, early exit {:?}", fw.name, executed, early);
    eprintln!("{}: pc=0x{:08x} sp=0x{:08x} lr=0x{:08x} xpsr=0x{:08x} CFSR=0x{:x} HFSR=0x{:x}", fw.name, h.cpu.pc(), h.r(13), h.r(14), h.cpu.xpsr(), cfsr, hfsr);
    eprintln!("{}: undefined encodings hit: {:x?}", fw.name, h.cpu.undefined_log());
    for w in h.cpu.take_warnings().iter().take(20) {
        eprintln!("{}: warning: {}", fw.name, w);
    }
}

fn smoke(fw: &Fw) {
    let mut h = match boot(fw) {
        Some(h) => h,
        None => {
            eprintln!("skipping {}: firmware not present", fw.name);
            return;
        }
    };
    let (executed, early) = run_slices(&mut h, 3_000_000, 10_000);
    report(&mut h, fw, executed, early);
    assert!(h.cpu.undefined_log().is_empty(), "undefined encodings: {:x?}", h.cpu.undefined_log());
    assert_eq!(h.cpu.ppb_peek32(0xE000_ED28, h.now), Some(0), "CFSR");
}

/// Architectural state compared between the two runs of `fast_forward_identity`.
fn fingerprint(h: &Harness) -> ([u32; 16], u32, u64, u64) {
    let mut regs = [0u32; 16];
    for (i, r) in regs.iter_mut().enumerate() {
        *r = h.r(i);
    }
    (regs, h.cpu.xpsr(), h.cpu.instructions(), h.now)
}

/// The idle-loop fast-forward must be invisible: stepping the real firmware in 100 us quanta with it on
/// and off gives the same registers, instruction count and time after every quantum (compared on the
/// flat bus; the firmware polls peripherals that read as zero here, so this checks that the detector
/// never skips a loop that is not idle and that skipped loops end in the same state).
fn fast_forward_identity(fw: &Fw) {
    let (mut off, mut on) = match (boot(fw), boot(fw)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            eprintln!("skipping {}: firmware not present", fw.name);
            return;
        }
    };
    off.cpu.set_idle_fast_forward(false);
    on.cpu.set_idle_fast_forward(true);
    let total = 6_000_000u64;
    while off.cpu.instructions() < total {
        let quantum_end = (off.now / 100_000 + 1) * 100_000;
        let a = off.run_until(quantum_end);
        let b = on.run_until(quantum_end);
        assert_eq!((a.reason, a.now, a.executed), (b.reason, b.now, b.executed), "{}: run exits diverge at {}", fw.name, off.now);
        assert_eq!(fingerprint(&off), fingerprint(&on), "{}: state diverges at t={} ns", fw.name, off.now);
        if a.reason != ExitReason::Deadline && a.reason != ExitReason::StopRequested {
            break;
        }
    }
    let stats = on.cpu.fast_forward_stats();
    eprintln!("{}: fast-forward on/off identical after {} instructions; {:?}", fw.name, off.cpu.instructions(), stats);
}

#[test]
fn handset_fast_forward_identity() {
    fast_forward_identity(&HANDSET);
}

#[test]
fn main_fast_forward_identity() {
    fast_forward_identity(&MAIN);
}

#[test]
fn handset_smoke() {
    smoke(&HANDSET);
}

#[test]
fn main_smoke() {
    smoke(&MAIN);
}
