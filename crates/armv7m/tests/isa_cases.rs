//! Data-driven instruction semantics tests: `tests/cases/*.txt` are assembled by
//! `tests/tools/gen_cases.py` into `generated_cases.rs` (committed, encodings from
//! arm-none-eabi-as). Each case sets registers/flags/memory, executes the program and
//! checks the resulting state.

mod common;
#[path = "generated/cases.rs"]
mod generated_cases;

use common::*;
use generated_cases::*;

const CODE: u32 = 0x0800_4000;
/// Handler of every system exception: `b .` (the core sleeps there).
const HANDLER: u32 = 0x0800_6000;

fn run_case(c: &Case) -> Vec<String> {
    let mut h = Harness::new();
    h.cpu.set_sp(0x2001_0000);
    // A fault at the end of the chunk is taken before `run` returns (tlib takes pending
    // exceptions at every translation-block boundary, the chunk end included): give every system
    // exception a valid vector so that entering the handler does not fault again.
    h.bus.load_halfwords(HANDLER, &[0xE7FE]);
    for exc in 2..16u32 {
        h.bus.poke32(FLASH_BASE + 4 * exc, HANDLER | 1);
    }
    h.bus.load_halfwords(CODE, c.code);
    h.cpu.set_pc(CODE);
    for (r, v) in c.init_regs {
        h.cpu.set_reg(*r, *v);
    }
    let mut apsr = 0u32;
    for (k, v) in c.init_misc {
        match *k {
            "flags" => apsr |= (v & 0xF) << 28,
            "q" => apsr |= (v & 1) << 27,
            "ge" => apsr |= (v & 0xF) << 16,
            "control" => h.cpu.set_control(*v),
            "cpacr" => h.cpu.ppb_poke32(0xE000_ED88, *v, 0),
            "fpccr" => h.cpu.ppb_poke32(0xE000_EF34, *v, 0),
            "fpdscr" => h.cpu.ppb_poke32(0xE000_EF3C, *v, 0),
            "fpscr" => h.cpu.fp_regs_mut().fpscr = *v,
            _ if k.starts_with('s') => h.cpu.fp_regs_mut().s[k[1..].parse::<usize>().unwrap()] = *v,
            _ => panic!("{}: unknown init key {k}", c.name),
        }
    }
    h.cpu.set_apsr(apsr);
    for (sz, a, v) in c.init_mem {
        match *sz {
            b'w' => h.bus.poke32(*a, *v),
            b'h' => h.bus.poke16(*a, *v as u16),
            _ => h.bus.poke8(*a, *v as u8),
        }
    }
    let exit = h.step(c.steps);
    let mut fails = Vec::new();
    let mut fail = |msg: String| fails.push(format!("{} [{}]: {}", c.name, c.asm, msg));
    if exit.executed != c.steps {
        fail(format!("executed {} of {} instructions ({:?})", exit.executed, c.steps, exit.reason));
    }
    for (r, v) in c.expect_regs {
        let got = h.cpu.reg(*r);
        if got != *v {
            fail(format!("r{} = 0x{:08x}, expected 0x{:08x}", r, got, v));
        }
    }
    for (sz, a, v) in c.expect_mem {
        let (got, want) = match *sz {
            b'w' => (h.bus.peek32(*a), *v),
            b'h' => (h.bus.peek16(*a) as u32, *v & 0xFFFF),
            _ => (h.bus.peek8(*a) as u32, *v & 0xFF),
        };
        if got != want {
            fail(format!("mem[0x{:08x}] = 0x{:x}, expected 0x{:x}", a, got, want));
        }
    }
    let apsr = h.cpu.apsr();
    // `pc` is the program counter after the case; when the case ended in an exception (faulting
    // cases), the exception has been entered and `pc` means the stacked return address.
    let pc = if h.cpu.ipsr() != 0 {
        let sp = if h.r(14) & 4 != 0 { h.cpu.psp() } else { h.cpu.msp() };
        h.bus.peek32(sp + 0x18)
    } else {
        h.cpu.pc()
    };
    for (k, v) in c.expect_misc {
        let (name, got, want) = match *k {
            "flags" => ("flags", apsr >> 28, *v),
            "q" => ("q", (apsr >> 27) & 1, *v),
            "ge" => ("ge", (apsr >> 16) & 0xF, *v),
            "pc" => ("pc", pc, *v),
            "pc_rel" => ("pc", pc, CODE + *v),
            "cfsr" => ("cfsr", h.cpu.ppb_peek32(0xE000_ED28, h.now).unwrap(), *v),
            "hfsr" => ("hfsr", h.cpu.ppb_peek32(0xE000_ED2C, h.now).unwrap(), *v),
            "ipsr" => ("ipsr", h.cpu.ipsr(), *v),
            "xpsr" => ("xpsr", h.cpu.xpsr(), *v),
            "control" => ("control", h.cpu.control(), *v),
            "fpscr" => ("fpscr", h.cpu.fp_regs().fpscr, *v),
            "fpccr" => ("fpccr", h.cpu.ppb_peek32(0xE000_EF34, h.now).unwrap(), *v),
            _ if k.starts_with('s') => ("sN", h.cpu.fp_regs().s[k[1..].parse::<usize>().unwrap()], *v),
            _ => panic!("{}: unknown expect key {k}", c.name),
        };
        if got != want {
            fail(format!("{name} = 0x{got:x}, expected 0x{want:x}"));
        }
    }
    fails
}

#[test]
fn isa_cases() {
    let mut all = Vec::new();
    for c in CASES {
        all.extend(run_case(c));
    }
    if !all.is_empty() {
        for f in &all {
            eprintln!("FAIL {f}");
        }
        panic!("{} of {} cases failed", all.len(), CASES.len());
    }
    eprintln!("{} ISA cases passed", CASES.len());
}
