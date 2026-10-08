//! Renode 1.17.0 timing micro vectors (`testdata/renode-micro-vectors.json`, recorded on 2026-10-07/08;
//! see `testdata/README.md`): tiny Thumb programs whose executed-PC traces and ISR-entry instruction
//! indices were recorded from Renode. The core is driven by a rig that follows Renode's
//! `CpuThreadBodyInner` for a single CPU (100 us quantum rounds, chunk =
//! `min(InstructionsToNearestLimit, instructions left in the round)`, WFI skips to the nearest limit
//! or the round end) and must reproduce them exactly.

mod common;

use armv7m::{Cpu, CpuBus, CpuConfig, ExitReason, BUS_IRQ_CHANGED, BUS_STOP_REQUESTED};
use emu_core::{Json, Time};
use std::collections::HashMap;

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/renode-micro-vectors.json");
const FLASH_BASE: u32 = 0x0800_0000;
const SRAM_BASE: u32 = 0x2000_0000;
const TIM6_BASE: u32 = 0x4000_1000;
const TIM6_IRQ: u32 = 54;
const NS_PER_INSTRUCTION: Time = 10;
const QUANTUM_NS: Time = 100_000;

/// Minimal TIM6 stand-in for the `level-irq-reentry` vector: UG sets UIF, the update interrupt
/// line follows `UIF & UIE`, SR is write-zero-to-clear.
#[derive(Default)]
struct Tim6 {
    regs: HashMap<u32, u32>,
    sr: u32,
    dier: u32,
    line: bool,
}

struct MicroBus {
    flash: Vec<u8>,
    sram: Vec<u8>,
    tim6: Tim6,
    notifications: u32,
    irq_changes: Vec<(u32, bool)>,
}

impl MicroBus {
    fn new(image: &[u8]) -> Self {
        let mut flash = vec![0u8; 0x10_0000];
        flash[..image.len()].copy_from_slice(image);
        MicroBus { flash, sram: vec![0; 0x1_8000], tim6: Tim6::default(), notifications: 0, irq_changes: Vec::new() }
    }

    fn sram32(&self, addr: u32) -> u32 {
        let o = (addr - SRAM_BASE) as usize;
        u32::from_le_bytes([self.sram[o], self.sram[o + 1], self.sram[o + 2], self.sram[o + 3]])
    }

    fn tim6_update_line(&mut self) {
        let level = self.tim6.sr & 1 != 0 && self.tim6.dier & 1 != 0;
        if level != self.tim6.line {
            self.tim6.line = level;
            self.irq_changes.push((TIM6_IRQ, level));
            self.notifications |= BUS_IRQ_CHANGED | BUS_STOP_REQUESTED;
        }
    }

    fn tim6_read(&self, off: u32) -> u32 {
        match off {
            0x0C => self.tim6.dier,
            0x10 => self.tim6.sr,
            o => *self.tim6.regs.get(&o).unwrap_or(&0),
        }
    }

    fn tim6_write(&mut self, off: u32, v: u32) {
        match off {
            0x0C => {
                self.tim6.dier = v & 1;
                self.tim6_update_line();
            }
            0x10 => {
                self.tim6.sr &= v;
                self.tim6_update_line();
            }
            0x14 => {
                if v & 1 != 0 {
                    self.tim6.sr |= 1;
                }
                self.tim6_update_line();
            }
            o => {
                self.tim6.regs.insert(o, v);
            }
        }
    }

    fn read(&mut self, addr: u32, size: usize) -> u32 {
        if addr.wrapping_sub(TIM6_BASE) < 0x400 {
            let w = self.tim6_read((addr - TIM6_BASE) & !3);
            return match size {
                1 => (w >> ((addr & 3) * 8)) & 0xFF,
                2 => (w >> ((addr & 3) * 8)) & 0xFFFF,
                _ => w,
            };
        }
        let (mem, base): (&[u8], u32) = if addr.wrapping_sub(FLASH_BASE) < 0x10_0000 {
            (&self.flash, FLASH_BASE)
        } else if addr.wrapping_sub(SRAM_BASE) < 0x1_8000 {
            (&self.sram, SRAM_BASE)
        } else {
            return 0;
        };
        let o = (addr - base) as usize;
        (0..size).fold(0u32, |v, i| v | (mem.get(o + i).copied().unwrap_or(0) as u32) << (8 * i))
    }

    fn write(&mut self, addr: u32, size: usize, value: u32) {
        if addr.wrapping_sub(TIM6_BASE) < 0x400 {
            self.tim6_write((addr - TIM6_BASE) & !3, value);
            return;
        }
        if addr.wrapping_sub(SRAM_BASE) < 0x1_8000 {
            let o = (addr - SRAM_BASE) as usize;
            for i in 0..size {
                self.sram[o + i] = (value >> (8 * i)) as u8;
            }
        }
    }
}

impl CpuBus for MicroBus {
    fn read8(&mut self, addr: u32, _icount: u64) -> u8 {
        self.read(addr, 1) as u8
    }
    fn read16(&mut self, addr: u32, _icount: u64) -> u16 {
        self.read(addr, 2) as u16
    }
    fn read32(&mut self, addr: u32, _icount: u64) -> u32 {
        self.read(addr, 4)
    }
    fn write8(&mut self, addr: u32, value: u8, _icount: u64) {
        self.write(addr, 1, value as u32)
    }
    fn write16(&mut self, addr: u32, value: u16, _icount: u64) {
        self.write(addr, 2, value as u32)
    }
    fn write32(&mut self, addr: u32, value: u32, _icount: u64) {
        self.write(addr, 4, value)
    }
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])> {
        if addr.wrapping_sub(FLASH_BASE) < 0x10_0000 {
            Some((FLASH_BASE, &self.flash))
        } else {
            None
        }
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.read(addr, 2) as u16
    }
    fn is_plain_memory(&self, addr: u32) -> bool {
        addr.wrapping_sub(FLASH_BASE) < 0x10_0000 || addr.wrapping_sub(SRAM_BASE) < 0x1_8000
    }
    fn take_notifications(&mut self) -> u32 {
        std::mem::take(&mut self.notifications)
    }
    fn drain_irq_changes(&mut self, sink: &mut dyn FnMut(u32, bool)) {
        for (irq, level) in self.irq_changes.drain(..) {
            sink(irq, level);
        }
    }
}

struct Program {
    image: Vec<u8>,
    labels: HashMap<String, u32>,
    /// Instructions Renode executed in the whole run.
    executed: u64,
    expected: Json,
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

fn load_program(name: &str) -> Option<Program> {
    let text = std::fs::read_to_string(VECTORS).unwrap_or_else(|e| panic!("{VECTORS}: {e}"));
    let json = Json::parse(&text).ok()?;
    let p = json.get("programs")?.get(name)?;
    let image = hex_bytes(p.get("imageHex")?.as_str()?);
    let labels = p.get("labels")?.as_object()?.iter().map(|(k, v)| (k.clone(), parse_hex(v.as_str().unwrap()))).collect();
    Some(Program { image, labels, executed: p.get("executedInstructions")?.as_u64()?, expected: p.get("expected")?.clone() })
}

struct Rig {
    cpu: Cpu,
    bus: MicroBus,
    now: Time,
    chunks: u64,
}

impl Rig {
    fn new(prog: &Program, cfg: CpuConfig) -> Rig {
        let mut cpu = Cpu::new(cfg);
        cpu.set_vtor(FLASH_BASE);
        cpu.set_sp(0x2001_8000);
        cpu.set_pc(prog.labels["reset"]);
        Rig { cpu, bus: MicroBus::new(&prog.image), now: 0, chunks: 0 }
    }

    /// Instructions to the nearest clock limit as Renode computes them (`InstructionsToNearestLimit`).
    fn instructions_to_nearest_limit(&mut self, left: u64) -> u64 {
        // The clock source has been advanced to `now` (ReportProgress).
        self.cpu.advance_idle(self.now, self.now);
        match self.cpu.next_internal_deadline() {
            Some(d) => ((d.saturating_sub(self.now)) / NS_PER_INSTRUCTION).max(1).min(left),
            None => left,
        }
    }

    /// `emulation RunFor` for `ns` nanoseconds on a 100 us quantum grid.
    fn run_for(&mut self, ns: Time) {
        let end = self.now + ns;
        while self.now < end {
            let round_end = ((self.now / QUANTUM_NS) + 1) * QUANTUM_NS;
            let round_end = round_end.min(end);
            let mut left = (round_end - self.now) / NS_PER_INSTRUCTION;
            while left > 0 {
                let n = self.instructions_to_nearest_limit(left);
                let exit = self.cpu.run(&mut self.bus, self.now, self.now + n * NS_PER_INSTRUCTION);
                self.chunks += 1;
                assert!(exit.executed <= n, "the core executed more than it was asked to: {exit:?} (asked {n})");
                self.now = exit.now;
                left -= exit.executed;
                match exit.reason {
                    ExitReason::Sleeping | ExitReason::Lockup => {
                        let skip = self.instructions_to_nearest_limit(left);
                        self.cpu.advance_idle(self.now, self.now + skip * NS_PER_INSTRUCTION);
                        self.now += skip * NS_PER_INSTRUCTION;
                        left -= skip;
                    }
                    ExitReason::Halted => panic!("unexpected halt"),
                    ExitReason::Deadline | ExitReason::StopRequested => {}
                }
            }
        }
    }
}

fn cfg() -> CpuConfig {
    assert_eq!(emu_core::TICKS_PER_SECOND, 1_000_000_000, "the vectors need the 1 ns time base");
    CpuConfig { ticks_per_instruction: NS_PER_INSTRUCTION, ..CpuConfig::default() }
}

fn positions(pcs: &[u32], pc: u32) -> Vec<usize> {
    pcs.iter().enumerate().filter(|(_, &p)| p == pc).map(|(i, _)| i).collect()
}

fn expected_u64(e: &Json, key: &str) -> u64 {
    e.get(key).unwrap_or_else(|| panic!("no expected.{key}")).as_u64().unwrap()
}

fn run_traced(name: &str, ns: Time) -> Option<(Rig, Program, Vec<u32>)> {
    let Some(prog) = load_program(name) else {
        panic!("program '{name}' is missing from {VECTORS}");
    };
    let mut rig = Rig::new(&prog, cfg());
    rig.cpu.trace_pcs(prog.executed as usize + 64);
    rig.run_for(ns);
    let pcs = rig.cpu.trace_take_pcs();
    Some((rig, prog, pcs))
}

fn check_first_pcs(pcs: &[u32], expected: &Json) {
    if let Some(list) = expected.get("firstPcs").and_then(|v| v.as_array()) {
        for (i, p) in list.iter().enumerate() {
            let want = parse_hex(p.as_str().unwrap());
            assert_eq!(pcs.get(i).copied(), Some(want), "PC trace differs from Renode at instruction {i}");
        }
    }
}

#[test]
fn systick_reload79999_entries_match_renode() {
    let Some((rig, prog, pcs)) = run_traced("systick-reload79999", 40_000_000) else { return };
    let want: Vec<u64> = prog.expected.get("entryIndices").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let got: Vec<usize> = positions(&pcs, prog.labels["systick_isr"]);
    assert_eq!(got.len() as u64, expected_u64(&prog.expected, "isrEntries"));
    for (k, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert_eq!(*g as u64, *w, "ISR entry {k}");
    }
    assert_eq!(rig.cpu.instructions(), 4_000_000, "100 000 instructions per millisecond, no sleeping");
}

#[test]
fn pendsv_is_taken_at_the_end_of_the_translation_block() {
    let Some((rig, prog, pcs)) = run_traced("pendsv-tb-end", 1_000_000) else { return };
    let isr = positions(&pcs, prog.labels["pendsv_isr"]);
    let trials = prog.expected.get("trials").unwrap().as_array().unwrap();
    for (k, t) in trials.iter().enumerate() {
        let pend = positions(&pcs, prog.labels[&format!("pend{k}")])[0];
        assert_eq!(pend as u64, expected_u64(t, "pendWriteIndex"));
        let entry = *isr.iter().find(|&&e| e > pend).expect("ISR entry");
        assert_eq!((entry - pend) as u64, expected_u64(t, "isrMinusWrite"), "trial {k} (nops {})", expected_u64(t, "nops"));
    }
    // `cpsie i` ends the block too: the pended PendSV is taken right after it.
    let seq = prog.expected.get("primaskSequence").unwrap();
    let cpsie = positions(&pcs, prog.labels["pm_cpsie"])[0];
    assert_eq!(cpsie as u64, expected_u64(seq, "cpsieIndex"));
    let entry = *isr.iter().find(|&&e| e > cpsie).unwrap();
    assert_eq!((entry - cpsie) as u64, expected_u64(seq, "isrMinusCpsie"));
    assert_eq!(rig.cpu.instructions(), prog.executed, "the final `B .` sleeps");
}

#[test]
fn level_held_line_reenters_back_to_back() {
    let Some((rig, prog, pcs)) = run_traced("level-irq-reentry", 1_000_000) else { return };
    let want: Vec<usize> = prog.expected.get("isrEntryIndices").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
    let got = positions(&pcs, prog.labels["tim6_isr"]);
    assert_eq!(got, want);
    assert_eq!(rig.cpu.instructions(), 100_000);
    assert_eq!(rig.bus.sram32(SRAM_BASE), 5, "five handler entries");
}

#[test]
fn wfi_counts_one_instruction_and_wakes_exactly_at_the_tick() {
    let Some((rig, prog, pcs)) = run_traced("wfi-systick", 20_000_000) else { return };
    assert_eq!(rig.cpu.instructions(), prog.executed);
    let isr = positions(&pcs, prog.labels["systick_isr"]);
    assert_eq!(isr.len() as u64, expected_u64(&prog.expected, "isrEntries"));
    assert_eq!(isr[0] as u64, expected_u64(&prog.expected, "firstIsrEntryIndex"));
    let spacing: Vec<usize> = isr.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(spacing.iter().all(|&s| s == 7), "{spacing:?}");
    assert_eq!(positions(&pcs, prog.labels["wfi_at"]).len() as u64, expected_u64(&prog.expected, "wfiExecutions"));
    check_first_pcs(&pcs, &prog.expected);
}

#[test]
fn wfi_wakes_for_a_masked_pending_interrupt_once_per_slice() {
    let Some((rig, prog, pcs)) = run_traced("wfi-primask", 3_000_000) else { return };
    assert_eq!(rig.cpu.instructions(), prog.executed);
    assert_eq!(positions(&pcs, prog.labels["systick_isr"]).len(), 0, "PRIMASK keeps the handler from running");
    assert_eq!(positions(&pcs, prog.labels["wfi_at"]).len() as u64, expected_u64(&prog.expected, "wfiExecutions"));
    check_first_pcs(&pcs, &prog.expected);
}

#[test]
fn cyccnt_reads_are_exact_instruction_times() {
    let Some((rig, prog, pcs)) = run_traced("wfi-systick-cyccnt", 12_000_000) else { return };
    assert_eq!(rig.cpu.instructions(), prog.executed);
    check_first_pcs(&pcs, &prog.expected);
    let want: Vec<u32> = prog.expected.get("cyccntSamples").unwrap().as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let got: Vec<u32> = (0..want.len() as u32).map(|i| rig.bus.sram32(SRAM_BASE + 0x10 + 4 * i)).collect();
    assert_eq!(got, want, "DWT CYCCNT sampled by the SysTick handler");
}
