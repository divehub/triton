//! Chunk boundaries of the handset's PWM timer events against Renode 1.17.0 (`vector.json`, recorded on
//! 2026-10-08; the generator is not part of this repository, see `testdata/README.md`).
//!
//! The handset's TIM2 is an `NGCLazyPwmTimer` with `ArithmeticUnconnectedPWM` and `LazyUnconnectedPWM` on (the
//! runner's default). Its stock clock-source events end CPU chunks until a normal overflow engages the
//! suppression, and every register write restores them; a chunk boundary moves the clock-source time that lags
//! the CPU, which the DWT cycle counter (`CTRL.CYCCNTENA` is credited from the chunk start) and a timer enable
//! (`timer-enable-lag`) make visible. The tiny Thumb program rewrites TIM2.CCR2, waits D iterations, restarts
//! DWT_CYCCNT, samples it, enables TIM6 and waits for its interrupt, for 19 values of D. Instruction indices of
//! every step, the CYCCNT samples and the total instruction count must equal Renode's.
//!
//! * the default scheduling (`Scheduling::NgcArithmeticPwm`) must reproduce the arithmetic-mode recording;
//! * `Scheduling::Stock` must reproduce the recording of the same program with both optimizations off;
//! * `Scheduling::Observable`, which elides every event nobody can see, must differ (the test has teeth).
//!
//! Skipped when the vector file is absent.

use emu_core::{Json, Time};
use ngc::{Board, BoardConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use stm32::timer::{Scheduling, Stm32Timer, IRQ_LINE};

const FLASH_BASE: u32 = 0x0800_0000;
const TIM2_BASE: u32 = 0x4000_0000;
const TIM2_IRQ: u32 = 28;
const TIM6_BASE: u32 = 0x4000_1000;
const TIM6_IRQ: u32 = 54;
const SAMPLES: u32 = 0x2000_0010;

/// One trial: probe A (TIM6 enable) and probe B (DWT counter enable), as instruction indices of the executed-PC
/// trace plus the sampled `CYCCNT`.
#[derive(Debug, PartialEq, Eq)]
struct Trial {
    delay: u64,
    wr_a: u64,
    cen: u64,
    isr: u64,
    wr_b: u64,
    dwt: u64,
    sample: u64,
    cyccnt: u64,
}

struct Recording {
    image: Vec<u8>,
    labels: HashMap<String, u32>,
    run_ns: Time,
    executed: u64,
    tim2_cen: u64,
    trials: Vec<Trial>,
}

fn vector_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/micro_pwm/vector.json")
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn recording(run: &str) -> Option<Recording> {
    let text = std::fs::read_to_string(vector_path()).ok()?;
    let json = Json::parse(&text).ok()?;
    let r = json.get("runs")?.get(run)?;
    let u = |j: &Json, key: &str| j.get(key).unwrap_or_else(|| panic!("no {key}")).as_u64().unwrap();
    Some(Recording {
        image: hex_bytes(r.get("imageHex")?.as_str()?),
        labels: r
            .get("labels")?
            .as_object()?
            .iter()
            .map(|(k, v)| (k.clone(), u32::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()))
            .collect(),
        run_ns: u(r, "runVirtualNs"),
        executed: u(r, "executedInstructions"),
        tim2_cen: u(r, "tim2CenIndex"),
        trials: r
            .get("trials")?
            .as_array()?
            .iter()
            .map(|t| Trial {
                delay: u(t, "delayIterations"),
                wr_a: u(t, "wrAIndex"),
                cen: u(t, "cenIndex"),
                isr: u(t, "isrIndex"),
                wr_b: u(t, "wrBIndex"),
                dwt: u(t, "dwtIndex"),
                sample: u(t, "sampleIndex"),
                cyccnt: u(t, "cyccnt"),
            })
            .collect(),
    })
}

/// Runs the recorded program on the micro board with TIM2 under `policy`, returns the trials and the totals.
fn replay(rec: &Recording, policy: Option<Scheduling>) -> (Vec<Trial>, u64, u64) {
    let mut board = Board::new(BoardConfig::new("micro"));
    let tim2 = Stm32Timer::new("timer2", 80_000_000, 0xFFFF_FFFF);
    let tim2 = match policy {
        Some(p) => tim2.with_scheduling(p),
        None => tim2,
    };
    let tim2 = board.add_mapped(TIM2_BASE, 0x400, Box::new(tim2)).unwrap();
    board.connect_irq(tim2, IRQ_LINE, TIM2_IRQ).unwrap();
    // TIM6 is the plain STM32_Timer of the platform.
    let tim6 = board
        .add_mapped(TIM6_BASE, 0x400, Box::new(Stm32Timer::new("timer6", 80_000_000, 0xFFFF).with_scheduling(Scheduling::Stock)))
        .unwrap();
    board.connect_irq(tim6, IRQ_LINE, TIM6_IRQ).unwrap();
    board.load(FLASH_BASE, &rec.image).unwrap();
    board.cpu.set_vtor(FLASH_BASE);
    board.cpu.set_sp(0x2001_8000);
    board.cpu.set_pc(rec.labels["reset"]);
    board.cpu.trace_pcs(rec.executed as usize + 64);
    board.run_until(rec.run_ns);
    let pcs = board.cpu.trace_take_pcs();
    let first = |label: &str, k: usize| -> u64 {
        let pc = rec.labels[&format!("{label}{k}")];
        pcs.iter().position(|&p| p == pc).unwrap_or(usize::MAX) as u64
    };
    let isr_pc = rec.labels["tim6_isr"];
    let isr: Vec<u64> = pcs.iter().enumerate().filter(|(_, &p)| p == isr_pc).map(|(i, _)| i as u64).collect();
    let trials = rec
        .trials
        .iter()
        .enumerate()
        .map(|(k, t)| Trial {
            delay: t.delay,
            wr_a: first("wrA", k),
            cen: first("cen", k),
            isr: isr.get(k).copied().unwrap_or(u64::MAX),
            wr_b: first("wrB", k),
            dwt: first("dwt", k),
            sample: first("sample", k),
            cyccnt: u64::from(board.peek32(SAMPLES + 4 * k as u32).unwrap_or(u32::MAX)),
        })
        .collect();
    (trials, board.cpu.instructions(), pcs.iter().position(|&p| p == rec.labels["tim2_cen"]).unwrap_or(usize::MAX) as u64)
}

/// The trials that differ from Renode, ignoring a CYCCNT sample that is off by at most `tolerated` cycles for the
/// delays listed in `known`.
fn describe(trials: &[Trial], want: &[Trial], known: &[(u64, u64)]) -> String {
    let mut out = String::new();
    for (got, expected) in trials.iter().zip(want) {
        if got == expected {
            continue;
        }
        let tolerated = known
            .iter()
            .find(|(delay, _)| *delay == expected.delay)
            .is_some_and(|(_, cycles)| Trial { cyccnt: got.cyccnt, ..*expected } == *got && got.cyccnt.abs_diff(expected.cyccnt) <= *cycles);
        if !tolerated {
            out.push_str(&format!("\n  D={}: Renode {:?}\n        Rust   {:?}", expected.delay, expected, got));
        }
    }
    out
}

fn check(run: &str, policy: Option<Scheduling>, known: &[(u64, u64)]) {
    let Some(rec) = recording(run) else {
        eprintln!("skipping {run}: {} not present", vector_path().display());
        return;
    };
    let (trials, executed, tim2_cen) = replay(&rec, policy);
    assert_eq!(tim2_cen, rec.tim2_cen, "{run}: TIM2 CEN store");
    let diff = describe(&trials, &rec.trials, known);
    assert!(diff.is_empty(), "{run} ({policy:?}): the trials that differ from Renode:{diff}");
    assert_eq!(executed, rec.executed, "{run}: executed instructions");
}

#[test]
fn arithmetic_ngc_timer_has_renodes_chunk_boundaries() {
    check("arithmetic", None, &[]);
}

/// The plain `STM32_Timer` (both optimizations off in the recording). One sample is 2 cycles (25 ns) off, a
/// known limit of the timer-local model: after a one-shot compare timer expired, Renode's `nearestLimitIn` keeps a
/// phantom limit one compare period later (reproduced), but ANY later update pass of the machine's clock source
/// removes it, including the limit event of another timer. Here TIM6's interrupt arrives 3 instructions before the
/// phantom limit of D=40's second probe and drops it in Renode; a timer cannot see the other timer's pass.
#[test]
fn stock_scheduling_has_renodes_chunk_boundaries() {
    check("stock", Some(Scheduling::Stock), &[(40, 2)]);
}

/// The CYCCNT samples of the two recordings differ from trial 11 on (the arithmetic mode has no events once it
/// engaged); a policy that elides the unobservable events cannot reproduce either.
#[test]
fn the_observable_policy_does_not_reproduce_the_chunk_boundaries() {
    let Some(rec) = recording("arithmetic") else { return };
    let stock = recording("stock").unwrap();
    assert_ne!(rec.trials.iter().map(|t| t.cyccnt).collect::<Vec<_>>(), stock.trials.iter().map(|t| t.cyccnt).collect::<Vec<_>>());
    let (trials, _, _) = replay(&rec, Some(Scheduling::Observable));
    assert!(!describe(&trials, &rec.trials, &[]).is_empty(), "the observable policy unexpectedly matched Renode's arithmetic mode");
}
