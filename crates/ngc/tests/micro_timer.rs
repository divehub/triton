//! Micro vectors for TIM6 (`testdata/renode-micro-vectors.json`, recorded from Renode 1.17.0; see
//! `testdata/README.md`): tiny Thumb programs run on the real Cortex-M4 core and the board run loop with the
//! `stm32::timer` model at `0x40001000` (IRQ 54), mirroring the single-timer Renode platform they were
//! recorded on. The instruction index at which every ISR is entered must equal Renode's: this pins down the
//! ceil-nanosecond `LimitTimer` periods, the `RequestReturn` of the timer register writes and the clock-source
//! lag at timer enable.

use emu_core::{Json, Time};
use ngc::{Board, BoardConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use stm32::timer::{Stm32Timer, IRQ_LINE};

const FLASH_BASE: u32 = 0x0800_0000;
const TIM6_BASE: u32 = 0x4000_1000;
const TIM6_IRQ: u32 = 54;

struct Program {
    image: Vec<u8>,
    labels: HashMap<String, u32>,
    executed: u64,
    run_ns: Time,
    expected: Json,
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/renode-micro-vectors.json")
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

fn load_program(name: &str) -> Option<Program> {
    let text = std::fs::read_to_string(vectors_path()).unwrap_or_else(|e| panic!("{}: {e}", vectors_path().display()));
    let json = Json::parse(&text).ok()?;
    let p = json.get("programs")?.get(name)?;
    Some(Program {
        image: hex_bytes(p.get("imageHex")?.as_str()?),
        labels: p.get("labels")?.as_object()?.iter().map(|(k, v)| (k.clone(), parse_hex(v.as_str().unwrap()))).collect(),
        executed: p.get("executedInstructions")?.as_u64()?,
        run_ns: p.get("runVirtualNs")?.as_u64()?,
        expected: p.get("expected")?.clone(),
    })
}

fn expected_u64(e: &Json, key: &str) -> u64 {
    e.get(key).unwrap_or_else(|| panic!("no expected.{key}")).as_u64().unwrap()
}

/// Runs the program on the micro board and returns the executed-PC trace and the board.
fn run(name: &str) -> Option<(Program, Vec<u32>, Board)> {
    let Some(prog) = load_program(name) else {
        eprintln!("skipping {name}: {} not present", vectors_path().display());
        return None;
    };
    let mut board = Board::new(BoardConfig::new("micro"));
    let tim6 = board.add_mapped(TIM6_BASE, 0x400, Box::new(Stm32Timer::new("timer6", 80_000_000, 0xFFFF))).unwrap();
    board.connect_irq(tim6, IRQ_LINE, TIM6_IRQ).unwrap();
    board.load(FLASH_BASE, &prog.image).unwrap();
    board.cpu.set_vtor(FLASH_BASE);
    board.cpu.set_sp(0x2001_8000);
    board.cpu.set_pc(prog.labels["reset"]);
    board.cpu.trace_pcs(prog.executed as usize + 64);
    board.run_until(prog.run_ns);
    let pcs = board.cpu.trace_take_pcs();
    Some((prog, pcs, board))
}

fn positions(pcs: &[u32], pc: u32) -> Vec<usize> {
    pcs.iter().enumerate().filter(|(_, &p)| p == pc).map(|(i, _)| i).collect()
}

fn check_periodic_entries(name: &str) {
    let Some((prog, pcs, board)) = run(name) else { return };
    let entries = positions(&pcs, prog.labels["tim6_isr"]);
    assert_eq!(entries.len() as u64, expected_u64(&prog.expected, "isrEntries"), "{name}: number of ISR entries");
    assert_eq!(entries[0] as u64, expected_u64(&prog.expected, "firstIsrEntryIndex"), "{name}: first ISR entry");
    let listed: Vec<u64> = prog.expected.get("entryIndices").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    for (k, want) in listed.iter().enumerate() {
        assert_eq!(entries[k] as u64, *want, "{name}: ISR entry {k}");
    }
    // The spacing histogram of the whole run: [[delta, count], ...].
    let mut histogram: Vec<(u64, u64)> = Vec::new();
    for w in entries.windows(2) {
        let delta = (w[1] - w[0]) as u64;
        match histogram.iter_mut().find(|(d, _)| *d == delta) {
            Some((_, count)) => *count += 1,
            None => histogram.push((delta, 1)),
        }
    }
    histogram.sort_unstable();
    let want: Vec<(u64, u64)> = prog
        .expected
        .get("deltaHistogram")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            let pair = pair.as_array().unwrap();
            (pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap())
        })
        .collect();
    assert_eq!(histogram, want, "{name}: ISR spacing histogram");
    assert_eq!(board.cpu.instructions(), prog.executed, "{name}: executed instructions");
}

#[test]
fn tim6_arr1001_psc0_matches_renode() {
    check_periodic_entries("tim6-ceil-arr1001-psc0");
}

#[test]
fn tim6_arr1001_psc2_matches_renode() {
    check_periodic_entries("tim6-ceil-arr1001-psc2");
}

#[test]
fn timer_enable_lag_matches_renode() {
    let Some((prog, pcs, board)) = run("timer-enable-lag") else { return };
    let trials = prog.expected.get("trials").unwrap().as_array().unwrap();
    let isr = positions(&pcs, prog.labels["tim6_isr"]);
    assert_eq!(isr.len(), trials.len(), "one ISR entry per trial");
    for (k, trial) in trials.iter().enumerate() {
        let cen = positions(&pcs, prog.labels[&format!("cen{k}")]);
        assert_eq!(cen[0] as u64, expected_u64(trial, "cenWriteIndex"), "trial {k}: CEN write");
        assert_eq!(isr[k] as u64, expected_u64(trial, "isrEntryIndex"), "trial {k}: ISR entry (delay {})", expected_u64(trial, "delayIterations"));
        assert_eq!((isr[k] - cen[0]) as u64, expected_u64(trial, "isrMinusCen"), "trial {k}: ISR entry relative to the CEN store");
    }
    assert_eq!(board.cpu.instructions(), prog.executed, "executed instructions");
}
