//! Throughput of the Cortex-M4F interpreter on a flat bus (no peripherals): the FreeRTOS-style
//! idle loop with idle-loop fast-forward off and on, and a mixed integer workload (xorshift,
//! byte/word loads and stores, multiply-accumulate, rotates, IT block, call/return, divide).
//!
//! Chunks follow the engine's quantum: 10 000 instructions (100 us at 10 ns per instruction).
//! `--kernels` adds straight-line kernels (the same instruction repeated; a NOP costs almost as much as
//! an ADD, which shows that the loop overhead and not the handlers bounds the throughput).
//!
//! ```text
//! ./cargo run --release -p armv7m --example bench --target-dir target/cpu [-- <million instructions per case> [--kernels]]
//! ```

use armv7m::{Cpu, CpuBus, CpuConfig, ExitReason};
use std::time::Instant;

const FLASH: u32 = 0x0800_0000;
const FLASH_SIZE: usize = 0x10_0000;
const SRAM: u32 = 0x2000_0000;
const SRAM_SIZE: usize = 0x1_8000;
const CODE: u32 = 0x0800_4000;
const TPI: u64 = emu_core::TICKS_PER_INSTRUCTION;

struct Flat {
    flash: Vec<u8>,
    sram: Vec<u8>,
}

impl Flat {
    fn new() -> Flat {
        Flat { flash: vec![0; FLASH_SIZE], sram: vec![0; SRAM_SIZE] }
    }

    fn load(&mut self, addr: u32, halfwords: &[u16]) {
        for (i, hw) in halfwords.iter().enumerate() {
            let o = (addr - FLASH) as usize + 2 * i;
            self.flash[o..o + 2].copy_from_slice(&hw.to_le_bytes());
        }
    }

    #[inline(always)]
    fn read(&self, addr: u32, n: usize) -> u32 {
        let (mem, o) = if addr.wrapping_sub(SRAM) < SRAM_SIZE as u32 {
            (&self.sram, (addr - SRAM) as usize)
        } else if addr.wrapping_sub(FLASH) < FLASH_SIZE as u32 {
            (&self.flash, (addr - FLASH) as usize)
        } else {
            return 0;
        };
        match n {
            1 => mem[o] as u32,
            2 => u16::from_le_bytes([mem[o], mem[o + 1]]) as u32,
            _ => u32::from_le_bytes([mem[o], mem[o + 1], mem[o + 2], mem[o + 3]]),
        }
    }

    #[inline(always)]
    fn write(&mut self, addr: u32, n: usize, v: u32) {
        if addr.wrapping_sub(SRAM) < SRAM_SIZE as u32 {
            let o = (addr - SRAM) as usize;
            self.sram[o..o + n].copy_from_slice(&v.to_le_bytes()[..n]);
        }
    }
}

impl CpuBus for Flat {
    #[inline]
    fn read8(&mut self, addr: u32, _icount: u64) -> u8 {
        self.read(addr, 1) as u8
    }
    #[inline]
    fn read16(&mut self, addr: u32, _icount: u64) -> u16 {
        self.read(addr, 2) as u16
    }
    #[inline]
    fn read32(&mut self, addr: u32, _icount: u64) -> u32 {
        self.read(addr, 4)
    }
    #[inline]
    fn write8(&mut self, addr: u32, value: u8, _icount: u64) {
        self.write(addr, 1, value as u32)
    }
    #[inline]
    fn write16(&mut self, addr: u32, value: u16, _icount: u64) {
        self.write(addr, 2, value as u32)
    }
    #[inline]
    fn write32(&mut self, addr: u32, value: u32, _icount: u64) {
        self.write(addr, 4, value)
    }
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])> {
        if addr.wrapping_sub(FLASH) < FLASH_SIZE as u32 {
            Some((FLASH, &self.flash))
        } else {
            None
        }
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.read(addr, 2) as u16
    }
    #[inline]
    fn is_plain_memory(&self, addr: u32) -> bool {
        addr.wrapping_sub(SRAM) < SRAM_SIZE as u32 || addr.wrapping_sub(FLASH) < FLASH_SIZE as u32
    }
    #[inline]
    fn take_notifications(&mut self) -> u32 {
        0
    }
    fn drain_irq_changes(&mut self, _sink: &mut dyn FnMut(u32, bool)) {}
}

/// `ldr r4,=0x20000100 ; ldr r7,=0x20000104 ; 1: ldr r5,=0x20000108 ; ldr r3,[r4] ; cbnz r3,2f ; ldr r3,=0x2000010c ;
/// ldr r3,[r3] ; cmp r3,#1 ; bls 1b ; 2: movs r0,#99 ; 3: b 3b` (the shape of the FreeRTOS idle task).
const IDLE_LOOP: &[u16] = &[0x4c05, 0x4f06, 0x4d06, 0x6823, 0xb91b, 0x4b06, 0x681b, 0x2b01, 0xd9f8, 0x2063, 0xe7fe, 0x0000, 0x0100, 0x2000, 0x0104, 0x2000, 0x0108, 0x2000, 0x010c, 0x2000];

/// Mixed integer workload (assembled with arm-none-eabi-as; see the module documentation).
const MIXED: &[u16] = &[
    0x4d13, 0x4e14, 0x2700, 0xea86, 0x3646, 0xea86, 0x4656, 0xea86, 0x1646, 0xf406, 0x707f, 0x5829, 0x4431, 0x5029, 0x70ee, 0x78ea, 0xfb01, 0x7702, 0xea4f, 0x13f7, 0x18ff, 0x2a80, 0xbf2c, 0x3f01, 0x3703, 0xf000, 0xf804, 0xfbb7,
    0xf4f0, 0x193f, 0xe7e3, 0xb510, 0xf046, 0x0001, 0x08c4, 0xeb40, 0x0004, 0xf040, 0x0001, 0xbd10, 0x1000, 0x2000, 0x79b9, 0x9e37,
];

/// About 1000 halfwords of copies of `insn` followed by a branch back to the start (a straight-line
/// kernel that isolates the per-instruction overhead of the interpreter loop). The sled stays within
/// the +-2 KiB reach of the 16-bit branch.
fn kernel(insn: &[u16]) -> Vec<u16> {
    let mut v = Vec::new();
    for _ in 0..1000 / insn.len() {
        v.extend_from_slice(insn);
    }
    let words = v.len() as i32; // halfwords before the branch
    // B (T2) at byte offset 2*words; target 0: offset = -(2*words + 4)
    let imm11 = ((-(words + 2)) as u32 & 0x7ff) as u16;
    assert!(words < 1020, "sled too long for a 16-bit branch");
    v.push(0xe000 | imm11);
    v
}

fn run(ff: bool, program: &[u16], instructions: u64) -> (f64, Cpu) {
    run_with(ff, program, instructions, |_| {})
}

fn run_with(ff: bool, program: &[u16], instructions: u64, setup: impl FnOnce(&mut Cpu)) -> (f64, Cpu) {
    let mut bus = Flat::new();
    bus.load(CODE, program);
    let mut cpu = Cpu::new(CpuConfig::default());
    cpu.set_vtor(FLASH);
    cpu.set_sp(SRAM + SRAM_SIZE as u32);
    cpu.set_pc(CODE);
    cpu.set_idle_fast_forward(ff);
    setup(&mut cpu);
    let start = Instant::now();
    let mut now = 0u64;
    let mut done = 0u64;
    while done < instructions {
        let n = (instructions - done).min(10_000);
        let exit = cpu.run(&mut bus, now, now + n * TPI);
        now = exit.now;
        done += exit.executed;
        match exit.reason {
            ExitReason::Deadline | ExitReason::StopRequested => {}
            other => panic!("unexpected exit {other:?} after {done} instructions"),
        }
    }
    let secs = start.elapsed().as_secs_f64();
    (done as f64 / secs / 1e6, cpu)
}

fn main() {
    let millions: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(400);
    let n = millions * 1_000_000;
    println!("armv7m interpreter benchmark, {millions} M instructions per case, 10 000-instruction chunks");
    let (mips, _) = run(false, IDLE_LOOP, n);
    println!("idle loop, fast-forward off : {mips:9.1} M instr/s");
    let (mips, cpu) = run(true, IDLE_LOOP, n);
    let stats = cpu.fast_forward_stats();
    println!("idle loop, fast-forward on  : {mips:9.1} M instr/s (virtual; {} loops verified, {:.1} % of the instructions skipped)", stats.loops, 100.0 * stats.skipped_instructions as f64 / n as f64);
    let (mips, cpu) = run(false, MIXED, n);
    println!("mixed integer workload      : {mips:9.1} M instr/s (r7 = {:#010x})", cpu.reg(7));
    let (mips, cpu) = run(true, MIXED, n);
    println!("mixed, fast-forward armed   : {mips:9.1} M instr/s (skipped {}; the workload is not an idle loop)", cpu.fast_forward_stats().skipped_instructions);
    if std::env::args().any(|a| a == "--kernels") {
        println!("straight-line kernels (fast-forward off; per-instruction cost of the loop and of single handlers):");
        let cases: &[(&str, &[u16])] = &[
            ("nop", &[0xbf00]),
            ("movs r1,#1", &[0x2101]),
            ("adds r0,#1 (dependent)", &[0x3001]),
            ("add r0,r1,r2", &[0x1888]),
            ("eors r0,r1 (dependent)", &[0x4048]),
            ("lsls r0,r0,#1", &[0x0040]),
            ("ldr r0,[r1] (sram)", &[0x6808]),
            ("str r0,[r1] (sram)", &[0x6008]),
            ("add.w r0,r1,r2", &[0xeb01, 0x0002]),
            ("ldr.w r0,[r1,#4] (sram)", &[0xf8d1, 0x0004]),
            ("b +2 (taken, skips a nop)", &[0xe000, 0xbf00]),
        ];
        for (name, insn) in cases {
            let prog = kernel(insn);
            // r1 must point at SRAM for the load/store kernels
            let (mips, _) = run_with(false, &prog, n / 2, |cpu| cpu.set_reg(1, SRAM + 0x100));
            println!("  {name:28}: {mips:9.1} M instr/s");
        }
    }
}
