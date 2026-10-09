//! Regression test against recorded Renode 1.17.0 behavior: `golden.txt` holds transcripts of the *unmodified*
//! stock `Timers.STM32_Timer`, `Timers.STM32F4_RTC` and `Timers.STM32_IndependentWatchdog` (recorded on
//! 2026-10-08 by a probe peripheral; the generator is not part of this repository, see `testdata/README.md`):
//! scripted and seeded-random sequences of register writes, reads,
//! input-line changes and virtual-time steps, together with every register value Renode returned and the
//! timestamped level changes of the connected output lines. Each transcript is replayed here against the Rust
//! models on a bare `Harness` and must reproduce every read and every edge (nanosecond, line, level) exactly,
//! in the same order.
//!
//! The comparison is on observables only: the lazy timer elides the events whose handlers cannot change
//! anything visible (under `Scheduling::Observable`, or once the NGC arithmetic suppression has engaged), so
//! internal event counts differ by design. Register values and output edges are policy independent, which
//! `every_scheduling_policy_reproduces_the_transcripts` checks for all three policies. Where the recording
//! connected the four channel pins they are declared observed (`with_observed_pins`); the `timer16q`/`timer32q`
//! kinds are the firmware wiring (interrupt outputs only), where long stretches are skipped.
//!
//! A 609-scenario soak (450 timers, 100 RTC, 59 watchdog) and a 201-scenario slave-mode soak, run with the
//! generator while it existed, were identical to Renode when this was written.

use emu_core::testing::Harness;
use emu_core::{Ctx, PeriphId, Peripheral, Time, Width};
use std::any::Any;
use stm32::iwdg::Iwdg;
use stm32::rtc::Rtc;
use stm32::timer::{Scheduling, Stm32Timer, TimerStats};

const GOLDEN: &str = include_str!("golden.txt");

/// The committed corpus.
fn golden_text() -> String {
    GOLDEN.to_string()
}

const TIMER_BASE: u32 = 0x4000_1000;
const RTC_BASE: u32 = 0x4000_2800;
const IWDG_BASE: u32 = 0x4000_3000;
/// The default time-source quantum of the stock machine (a pending `RequestReset` runs at its next end).
const QUANTUM: Time = 100_000;
const RESET_LINE: u32 = 254;

/// Receiver standing in for `DiffTimerProbe`: records `(time, line, level)` in delivery order.
struct Recorder {
    events: Vec<(Time, u32, bool)>,
}

impl Peripheral for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        self.events.push((ctx.now(), line, level));
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Write(u32, u32),
    Read(u32),
    Run(Time),
    Input(u32, bool),
}

#[derive(Clone)]
struct Scenario {
    name: String,
    kind: String,
    end: Time,
    last_run_start: Time,
    ops: Vec<Op>,
    reads: Vec<u32>,
    edges: Vec<(Time, u32, bool)>,
    resets: Vec<Time>,
}

fn parse_golden() -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut current: Option<Scenario> = None;
    let text = golden_text();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "scenario" => {
                let f: Vec<&str> = rest.split(' ').collect();
                current = Some(Scenario {
                    name: f[0].to_string(),
                    kind: f[1].to_string(),
                    end: f[2].parse().unwrap(),
                    last_run_start: f[3].parse().unwrap(),
                    ops: Vec::new(),
                    reads: Vec::new(),
                    edges: Vec::new(),
                    resets: Vec::new(),
                });
            }
            "ops" => {
                let s = current.as_mut().unwrap();
                for item in rest.split(' ').filter(|i| !i.is_empty()) {
                    let p: Vec<&str> = item.split(':').collect();
                    let n = |i: usize| -> u64 { p[i].parse().unwrap() };
                    s.ops.push(match p[0] {
                        "w" => Op::Write(n(1) as u32, n(2) as u32),
                        "r" => Op::Read(n(1) as u32),
                        "t" => Op::Run(n(1)),
                        "i" => Op::Input(n(1) as u32, n(2) != 0),
                        other => panic!("unknown op {other}"),
                    });
                }
            }
            "reads" => current.as_mut().unwrap().reads = rest.split(' ').filter(|v| !v.is_empty()).map(|v| v.parse().unwrap()).collect(),
            "edges" => {
                current.as_mut().unwrap().edges = rest
                    .split(' ')
                    .filter(|v| !v.is_empty())
                    .map(|v| {
                        let p: Vec<&str> = v.split(':').collect();
                        (p[0].parse().unwrap(), p[1].parse().unwrap(), p[2] == "1")
                    })
                    .collect()
            }
            "resets" => current.as_mut().unwrap().resets = rest.split(' ').filter(|v| !v.is_empty()).map(|v| v.parse().unwrap()).collect(),
            "end" => out.push(current.take().unwrap()),
            other => panic!("unknown golden record {other}"),
        }
    }
    out
}

/// What the replay of one scenario produced.
struct Replay {
    reads: Vec<u32>,
    read_origin: Vec<(usize, u32)>,
    edges: Vec<(Time, u32, bool)>,
    /// The time at which the watchdog asked for a machine reset, if it did.
    reset_request: Option<Time>,
    /// Work counters of a timer model.
    stats: Option<TimerStats>,
}

/// Replays with the timer's default scheduling policy.
fn replay(s: &Scenario) -> Replay {
    replay_with(s, None)
}

/// Replays with an explicit scheduling policy for the timer scenarios (`None`: the default).
fn replay_with(s: &Scenario, policy: Option<Scheduling>) -> Replay {
    let mut h = Harness::new();
    let timer = |limit: u32, observed: u8| {
        let t = Stm32Timer::new("timer", 80_000_000, limit).with_observed_pins(observed);
        match policy {
            Some(p) => t.with_scheduling(p),
            None => t,
        }
    };
    let (id, base, lines): (PeriphId, u32, &[u32]) = match s.kind.as_str() {
        "timer16" => (h.add_mapped(TIMER_BASE, 0x400, timer(0xFFFF, 0b1111)), TIMER_BASE, &[0, 2, 3, 5, 8, 9, 10, 11]),
        "timer32" => (h.add_mapped(TIMER_BASE, 0x400, timer(0xFFFF_FFFF, 0b1111)), TIMER_BASE, &[0, 2, 3, 5, 8, 9, 10, 11]),
        // Firmware wiring: the interrupt outputs only, no channel pin observed (long stretches can be skipped).
        "timer16q" => (h.add_mapped(TIMER_BASE, 0x400, timer(0xFFFF, 0)), TIMER_BASE, &[0, 2, 3, 5]),
        "timer32q" => (h.add_mapped(TIMER_BASE, 0x400, timer(0xFFFF_FFFF, 0)), TIMER_BASE, &[0, 2, 3, 5]),
        "rtc32768" => (h.add_mapped(RTC_BASE, 0x400, Rtc::new("rtc", 32768)), RTC_BASE, &[0, 1]),
        "rtc32000" => (h.add_mapped(RTC_BASE, 0x400, Rtc::new("rtc", 32000)), RTC_BASE, &[0, 1]),
        "iwdg" => (h.add_mapped(IWDG_BASE, 0x400, Iwdg::ngc("iwdg")), IWDG_BASE, &[]),
        other => panic!("unknown scenario kind {other}"),
    };
    let recorder = h.add(Recorder { events: Vec::new() });
    for &line in lines {
        h.connect_input(id, line, recorder, line);
    }
    let connect_pushes = lines.len();
    let mut reads = Vec::new();
    let mut read_origin = Vec::new();
    for (index, op) in s.ops.iter().enumerate() {
        match *op {
            Op::Write(offset, value) => h.write32(base + offset, value),
            Op::Read(offset) => {
                reads.push(h.read32(base + offset));
                read_origin.push((index, offset));
            }
            Op::Run(ns) => {
                let target = h.now() + ns;
                h.advance_to(target);
            }
            Op::Input(line, level) => h.set_input(id, line, level),
        }
    }
    assert_eq!(h.now(), s.end, "{}: replay ended at a different time", s.name);
    let edges = h.get::<Recorder>(recorder).events.iter().skip(connect_pushes).copied().collect();
    let reset_request = if s.kind == "iwdg" { h.get_mut::<Iwdg>(id).take_reset_request() } else { None };
    let stats = s.kind.starts_with("timer").then(|| h.get::<Stm32Timer>(id).stats());
    Replay { reads, read_origin, edges, reset_request, stats }
}

/// Describes the first difference between two edge lists.
fn first_edge_difference(expected: &[(Time, u32, bool)], actual: &[(Time, u32, bool)]) -> Option<String> {
    let common = expected.len().min(actual.len());
    for i in 0..common {
        if expected[i] != actual[i] {
            return Some(format!(
                "edge #{i}: Renode {:?}, Rust {:?} (neighbors: Renode {:?} / Rust {:?})",
                expected[i],
                actual[i],
                &expected[i.saturating_sub(2)..(i + 3).min(expected.len())],
                &actual[i.saturating_sub(2)..(i + 3).min(actual.len())]
            ));
        }
    }
    if expected.len() != actual.len() {
        let extra = if expected.len() > actual.len() { ("Renode", &expected[common..]) } else { ("Rust", &actual[common..]) };
        return Some(format!(
            "edge counts differ: Renode {}, Rust {}; first extra ({}): {:?}",
            expected.len(),
            actual.len(),
            extra.0,
            &extra.1[..extra.1.len().min(4)]
        ));
    }
    None
}

/// Compares one scenario; returns the problems found (empty when the replay matches Renode).
fn compare(s: &Scenario) -> Vec<String> {
    compare_with(s, None)
}

fn compare_with(s: &Scenario, policy: Option<Scheduling>) -> Vec<String> {
    let r = replay_with(s, policy);
    let mut problems = Vec::new();
    if r.reads.len() != s.reads.len() {
        problems.push(format!("{}: {} reads replayed, {} recorded", s.name, r.reads.len(), s.reads.len()));
    }
    for (i, (&want, &got)) in s.reads.iter().zip(&r.reads).enumerate() {
        if want != got {
            let (op, offset) = r.read_origin[i];
            problems.push(format!("{}: read #{i} (op {op}, offset 0x{offset:02X}): Renode 0x{want:08X}, Rust 0x{got:08X}", s.name));
            break;
        }
    }
    // Machine resets are only checked for the watchdog: Renode carries one out at the end of the quantum in
    // which it was requested (the last run, by construction of the scenarios).
    let renode_edges: Vec<_> = s.edges.iter().filter(|e| e.1 != RESET_LINE).copied().collect();
    if let Some(difference) = first_edge_difference(&renode_edges, &r.edges) {
        problems.push(format!("{}: {difference}", s.name));
    }
    if s.kind == "iwdg" {
        match (s.resets.first(), r.reset_request) {
            (None, None) => {}
            (Some(&at), Some(requested)) => {
                let run_start = s.last_run_start;
                let quanta = if requested > run_start { (requested - run_start).div_ceil(QUANTUM) } else { 1 };
                let carried_out = (run_start + quanta * QUANTUM).min(s.end);
                if carried_out != at {
                    problems.push(format!(
                        "{}: watchdog reset requested at {requested} ns, Renode carried it out at {at} ns, expected {carried_out} ns",
                        s.name
                    ));
                }
            }
            (renode, rust) => problems.push(format!("{}: reset: Renode {renode:?}, Rust request {rust:?}", s.name)),
        }
    }
    problems
}

struct Group {
    scenarios: usize,
    reads: usize,
    edges: usize,
    problems: Vec<String>,
}

fn run_group(prefix: &[&str]) -> Group {
    let scenarios = parse_golden();
    let mut group = Group { scenarios: 0, reads: 0, edges: 0, problems: Vec::new() };
    for s in scenarios.iter().filter(|s| prefix.iter().any(|p| s.kind.starts_with(p))) {
        group.scenarios += 1;
        group.reads += s.reads.len();
        group.edges += s.edges.len();
        group.problems.extend(compare(s));
    }
    group
}

/// True when a partial corpus replaces the committed one (the tests about the corpus' content then stand
/// down). Always false since the generator and the soak corpora are not part of this repository.
fn soak() -> bool {
    false
}

fn report(name: &str, group: Group) {
    if group.scenarios == 0 && soak() {
        eprintln!("{name}: no scenarios of this kind in the soak corpus");
        return;
    }
    assert!(group.scenarios > 0, "{name}: no golden scenarios");
    if !group.problems.is_empty() {
        let shown: Vec<_> = group.problems.iter().take(12).cloned().collect();
        panic!(
            "{name}: {} problems in {} scenarios compared with Renode; first problems:\n{}",
            group.problems.len(),
            group.scenarios,
            shown.join("\n")
        );
    }
    eprintln!("{name}: {} scenarios, {} register reads and {} output edges identical to Renode", group.scenarios, group.reads, group.edges);
}

#[test]
fn timers_match_renode() {
    report("timers", run_group(&["timer"]));
}

#[test]
fn rtc_matches_renode() {
    report("rtc", run_group(&["rtc"]));
}

#[test]
fn watchdog_matches_renode() {
    report("watchdog", run_group(&["iwdg"]));
}

/// The comparison itself can fail: a changed read, a shifted edge, a missing edge and a wrong reset time are
/// all reported.
#[test]
fn comparison_detects_differences() {
    if soak() {
        return;
    }
    let scenarios = parse_golden();
    let pick = |name: &str| scenarios.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no scenario {name}")).clone();
    for name in ["hal-tick-0", "pwm-tim2-0", "capture-0", "rtc-0", "rtc-alarm-1"] {
        let original = pick(name);
        assert!(compare(&original).is_empty(), "{name} must match before tampering");
        let mut changed_read = original.clone();
        let index = changed_read.reads.len() / 2;
        changed_read.reads[index] ^= 1;
        assert!(compare(&changed_read).iter().any(|p| p.contains("read #")), "{name}: tampered read not detected");
        if !original.edges.is_empty() {
            let mut shifted = original.clone();
            let index = shifted.edges.len() / 2;
            shifted.edges[index].0 += 1;
            assert!(compare(&shifted).iter().any(|p| p.contains("edge #")), "{name}: shifted edge not detected");
            let mut missing = original.clone();
            missing.edges.pop();
            assert!(compare(&missing).iter().any(|p| p.contains("edge counts differ")), "{name}: missing edge not detected");
        }
    }
    let expiring = scenarios.iter().find(|s| s.kind == "iwdg" && !s.resets.is_empty()).expect("an expiring watchdog scenario").clone();
    assert!(compare(&expiring).is_empty());
    let mut late = expiring.clone();
    late.resets[0] += 1;
    assert!(compare(&late).iter().any(|p| p.contains("carried it out")));
    let mut none = expiring;
    none.resets.clear();
    assert!(compare(&none).iter().any(|p| p.contains("reset:")));
}

/// Sanity of the recording itself: the corpus covers what the replay claims to check.
#[test]
fn golden_corpus_has_substance() {
    if soak() {
        return;
    }
    let scenarios = parse_golden();
    let edges = |prefix: &str| -> usize { scenarios.iter().filter(|s| s.kind.starts_with(prefix)).map(|s| s.edges.len()).sum() };
    let reads = |prefix: &str| -> usize { scenarios.iter().filter(|s| s.kind.starts_with(prefix)).map(|s| s.reads.len()).sum() };
    assert!(edges("timer") > 5_000 && reads("timer") > 1_500, "timer edges {}, reads {}", edges("timer"), reads("timer"));
    assert!(edges("rtc") > 200 && reads("rtc") > 1_500, "rtc edges {}, reads {}", edges("rtc"), reads("rtc"));
    assert!(scenarios.iter().filter(|s| s.kind == "iwdg" && !s.resets.is_empty()).count() >= 4);
    assert!(scenarios.iter().filter(|s| s.kind == "iwdg" && s.resets.is_empty()).count() >= 4);
    // Every kind of output line moves somewhere in the corpus: the interrupt outputs (IRQ 0, update 2, trigger 3,
    // capture/compare 5), the four channel pins (8-11), and the RTC's alarm (0) and wakeup (1) lines.
    let line_edges = |prefix: &str, line: u32| -> usize {
        scenarios.iter().filter(|s| s.kind.starts_with(prefix)).map(|s| s.edges.iter().filter(|e| e.1 == line).count()).sum()
    };
    for line in [0, 2, 3, 5, 8, 9, 10, 11] {
        let wanted = if line == 3 { 2 } else { 20 }; // the trigger interrupt needs slave mode + input edges + TIE
        assert!(line_edges("timer", line) >= wanted, "timer line {line}: {} edges", line_edges("timer", line));
    }
    for line in [0, 1] {
        assert!(line_edges("rtc", line) >= 20, "rtc line {line}: {} edges", line_edges("rtc", line));
    }
}

/// The handset-style scenarios (pins not observed) must be answerable without replaying every period under the
/// fastest policy: that is the point of the lazy model, and the transcripts prove the elided periods change
/// nothing visible.
#[test]
fn quiet_timers_skip_periods_and_still_match() {
    if soak() {
        return;
    }
    let scenarios = parse_golden();
    let mut checked = 0;
    for s in scenarios.iter().filter(|s| s.name.starts_with("pwm-long-") || s.name.starts_with("tick-long-")) {
        let r = replay_with(s, Some(Scheduling::Observable));
        let stats = r.stats.expect("timer");
        let periods = s.end / 2_500; // the shortest period among them (TIM15: 2.5 us)
        if s.name.starts_with("pwm-long-") {
            assert!(stats.skipped_cycles > 10_000, "{}: {stats:?}", s.name);
            assert!(stats.instants * 20 < periods, "{}: {} instants for {periods} periods ({stats:?})", s.name, stats.instants);
        }
        eprintln!("{} (observable): {} ns, {stats:?}", s.name, s.end);
        assert!(compare_with(s, Some(Scheduling::Observable)).is_empty(), "{}", s.name);
        checked += 1;
    }
    assert!(checked >= 6, "{checked} long scenarios");
}

/// The default policy (the handset's `NGCLazyPwmTimer` in arithmetic mode) keeps all stock events until a
/// normal overflow finds the configuration eligible, then none until the next write. The random scripts often
/// write configurations that are not eligible (a compare value above ARR, ...), where the stock events
/// stay exactly as in Renode; `tests/timer_ngc.rs` pins the engagement rules down. Here: the suppression does
/// engage in the long scripts, and the replay still equals the transcript.
#[test]
fn ngc_arithmetic_policy_engages_in_the_long_scripts() {
    if soak() {
        return;
    }
    let scenarios = parse_golden();
    let mut engaged_scenarios = 0;
    for s in scenarios.iter().filter(|s| s.name.starts_with("pwm-long-")) {
        let r = replay(s);
        let stats = r.stats.expect("timer");
        eprintln!("{} (ngc): {} ns, {stats:?}", s.name, s.end);
        engaged_scenarios += usize::from(stats.engagements > 0 && stats.skipped_cycles > 10_000);
        assert!(compare(s).is_empty(), "{}", s.name);
    }
    assert!(engaged_scenarios >= 4, "{engaged_scenarios} of the long scripts engaged and skipped periods");
}

/// Register values and output edges do not depend on the policy: every timer transcript is reproduced under
/// all three (the policies only decide which limits are machine events, i.e. the CPU's chunk boundaries).
#[test]
fn every_scheduling_policy_reproduces_the_transcripts() {
    let scenarios = parse_golden();
    let mut problems = Vec::new();
    let mut runs = 0;
    for s in scenarios.iter().filter(|s| s.kind.starts_with("timer")) {
        for policy in [Scheduling::Stock, Scheduling::NgcArithmeticPwm, Scheduling::Observable] {
            problems.extend(compare_with(s, Some(policy)).into_iter().map(|p| format!("[{policy:?}] {p}")));
            runs += 1;
        }
    }
    assert!(runs > 300, "{runs} replays");
    assert!(problems.is_empty(), "{} problems, first: {:#?}", problems.len(), &problems[..problems.len().min(6)]);
}
