//! Edge cases of the predecode cache and the hot loop: execution running off the end of the cached
//! code region (guard slots), a 32-bit instruction whose second halfword lies behind the region, code
//! outside any code region, and `invalidate_code_cache` after the code changed.

use armv7m::{Cpu, CpuBus, CpuConfig, ExitReason};
use emu_core::TICKS_PER_INSTRUCTION as TPI;

const CODE: u32 = 0x0800_0000;
const CODE_LEN: usize = 0x100;
/// Memory right behind the code region: readable through `fetch16` only (not a code region).
const BEYOND: u32 = CODE + CODE_LEN as u32;

struct Edge {
    code: Vec<u8>,
    beyond: Vec<u8>,
    ram: Vec<u8>,
}

impl Edge {
    fn new() -> Edge {
        Edge { code: vec![0; CODE_LEN], beyond: vec![0; 0x100], ram: vec![0; 0x1000] }
    }

    fn put(&mut self, addr: u32, halfwords: &[u16]) {
        for (i, hw) in halfwords.iter().enumerate() {
            let a = addr + 2 * i as u32;
            let (mem, base) = if a >= BEYOND { (&mut self.beyond, BEYOND) } else { (&mut self.code, CODE) };
            let o = (a - base) as usize;
            mem[o..o + 2].copy_from_slice(&hw.to_le_bytes());
        }
    }
}

impl CpuBus for Edge {
    fn read8(&mut self, a: u32, _: u64) -> u8 {
        self.ram.get(a.wrapping_sub(0x2000_0000) as usize).copied().unwrap_or(0)
    }
    fn read16(&mut self, a: u32, _: u64) -> u16 {
        u16::from(self.read8(a, 0)) | u16::from(self.read8(a + 1, 0)) << 8
    }
    fn read32(&mut self, a: u32, _: u64) -> u32 {
        u32::from(self.read16(a, 0)) | u32::from(self.read16(a + 2, 0)) << 16
    }
    fn write8(&mut self, a: u32, v: u8, _: u64) {
        if let Some(b) = self.ram.get_mut(a.wrapping_sub(0x2000_0000) as usize) {
            *b = v;
        }
    }
    fn write16(&mut self, a: u32, v: u16, _: u64) {
        self.write8(a, v as u8, 0);
        self.write8(a + 1, (v >> 8) as u8, 0);
    }
    fn write32(&mut self, a: u32, v: u32, _: u64) {
        self.write16(a, v as u16, 0);
        self.write16(a + 2, (v >> 16) as u16, 0);
    }
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])> {
        (addr.wrapping_sub(CODE) < CODE_LEN as u32).then_some((CODE, &self.code[..]))
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        let (mem, base) = if addr >= BEYOND { (&self.beyond, BEYOND) } else { (&self.code, CODE) };
        let o = addr.wrapping_sub(base) as usize;
        if o + 1 < mem.len() {
            u16::from_le_bytes([mem[o], mem[o + 1]])
        } else {
            0
        }
    }
    fn is_plain_memory(&self, _addr: u32) -> bool {
        true
    }
    fn take_notifications(&mut self) -> u32 {
        0
    }
    fn drain_irq_changes(&mut self, _sink: &mut dyn FnMut(u32, bool)) {}
}

fn cpu_at(pc: u32) -> Cpu {
    let mut cpu = Cpu::new(CpuConfig::default());
    cpu.set_vtor(CODE);
    cpu.set_sp(0x2000_1000);
    cpu.set_pc(pc);
    cpu
}

/// Runs `n` instructions in one chunk.
fn run(cpu: &mut Cpu, bus: &mut Edge, now: &mut u64, n: u64) -> armv7m::RunExit {
    let exit = cpu.run(bus, *now, *now + n * TPI);
    *now = exit.now;
    exit
}

const NOP: u16 = 0xBF00;
const B_SELF: u16 = 0xE7FE;
const MOVW_R2_1234: [u16; 2] = [0xF241, 0x2234];

#[test]
fn a_16_bit_instruction_in_the_last_halfword_falls_through_into_uncached_code() {
    let mut bus = Edge::new();
    bus.put(CODE + CODE_LEN as u32 - 2, &[0x2001]); // movs r0,#1 (last halfword of the region)
    bus.put(BEYOND, &[0x2102, 0x2203, B_SELF]); // movs r1,#2 ; movs r2,#3 ; b .
    let mut cpu = cpu_at(CODE + CODE_LEN as u32 - 2);
    let mut now = 0;
    let exit = run(&mut cpu, &mut bus, &mut now, 3);
    assert_eq!((exit.executed, exit.reason), (3, ExitReason::Deadline));
    assert_eq!((cpu.reg(0), cpu.reg(1), cpu.reg(2)), (1, 2, 3));
    assert_eq!(cpu.pc(), BEYOND + 4, "at the `b .`");
    assert!(cpu.undefined_log().is_empty());
    // The spin loop executes as WFI.
    let exit = run(&mut cpu, &mut bus, &mut now, 10);
    assert_eq!((exit.executed, exit.reason), (1, ExitReason::Sleeping));
}

#[test]
fn a_32_bit_instruction_in_the_last_halfword_fetches_its_second_halfword_from_the_bus() {
    let mut bus = Edge::new();
    bus.put(CODE + CODE_LEN as u32 - 2, &[MOVW_R2_1234[0]]);
    bus.put(BEYOND, &[MOVW_R2_1234[1], 0x2101, B_SELF]); // second half of movw ; movs r1,#1 ; b .
    let mut cpu = cpu_at(CODE + CODE_LEN as u32 - 2);
    let mut now = 0;
    let exit = run(&mut cpu, &mut bus, &mut now, 2);
    assert_eq!((exit.executed, exit.reason), (2, ExitReason::Deadline));
    assert_eq!(cpu.reg(2), 0x1234, "movw r2,#0x1234 assembled from both sides of the region end");
    assert_eq!(cpu.reg(1), 1);
    assert_eq!(cpu.pc(), BEYOND + 2 + 2, "at the `b .` behind the movs");
    assert!(cpu.undefined_log().is_empty());
}

#[test]
fn the_straddling_instruction_is_decoded_again_when_the_bus_content_changes() {
    // The slot of an instruction whose second halfword lives outside the region is never cached, so a
    // different second halfword is picked up the next time without invalidating anything.
    let mut bus = Edge::new();
    let last = CODE + CODE_LEN as u32 - 2;
    bus.put(last, &[MOVW_R2_1234[0]]);
    bus.put(BEYOND, &[MOVW_R2_1234[1], NOP]);
    let mut cpu = cpu_at(last);
    let mut now = 0;
    run(&mut cpu, &mut bus, &mut now, 1);
    assert_eq!(cpu.reg(2), 0x1234);
    bus.put(BEYOND, &[0x2299]); // movw r2,#0x1299: second halfword 0x2299
    cpu.set_pc(last);
    run(&mut cpu, &mut bus, &mut now, 1);
    assert_eq!(cpu.reg(2), 0x1299);
}

#[test]
fn a_loop_across_the_end_of_the_cached_region_runs_correctly() {
    // last halfword: movs r0,#1 ; beyond: adds r3,#1 ; b last   (a branch from uncached code back into the cache)
    let mut bus = Edge::new();
    let last = CODE + CODE_LEN as u32 - 2;
    bus.put(last, &[0x2001]);
    // b last: at BEYOND+2, target = BEYOND-2 = pc + 4 + off -> off = -(2 + 2 + 4)... computed below
    let b_pc = BEYOND + 2;
    let off = last as i32 - (b_pc as i32 + 4);
    let b = 0xE000 | (((off / 2) as u32) & 0x7FF) as u16;
    bus.put(BEYOND, &[0x3301, b]);
    let mut cpu = cpu_at(last);
    let mut now = 0;
    let exit = run(&mut cpu, &mut bus, &mut now, 300);
    assert_eq!(exit.executed, 300);
    assert_eq!(cpu.reg(3), 100, "three instructions per round");
    assert_eq!(cpu.reg(0), 1);
}

#[test]
fn code_changes_are_picked_up_after_invalidation() {
    let mut bus = Edge::new();
    bus.put(CODE + 0x20, &[0x2001, 0x2102, B_SELF]); // movs r0,#1 ; movs r1,#2 ; b .
    let mut cpu = cpu_at(CODE + 0x20);
    let mut now = 0;
    run(&mut cpu, &mut bus, &mut now, 2);
    assert_eq!((cpu.reg(0), cpu.reg(1)), (1, 2));
    bus.put(CODE + 0x20, &[0x2005, NOP, B_SELF]); // movs r0,#5 ; nop
    cpu.invalidate_code_cache();
    cpu.set_pc(CODE + 0x20);
    run(&mut cpu, &mut bus, &mut now, 2);
    assert_eq!((cpu.reg(0), cpu.reg(1)), (5, 2));
}

#[test]
fn a_chunk_ending_exactly_at_the_end_of_the_region_resumes_in_uncached_code() {
    let mut bus = Edge::new();
    bus.put(CODE + CODE_LEN as u32 - 4, &[NOP, 0x2001]); // nop ; movs r0,#1 (last two halfwords)
    bus.put(BEYOND, &[0x2102, B_SELF]);
    let mut cpu = cpu_at(CODE + CODE_LEN as u32 - 4);
    let mut now = 0;
    for expected_pc in [CODE + CODE_LEN as u32 - 2, BEYOND, BEYOND + 2] {
        let exit = run(&mut cpu, &mut bus, &mut now, 1);
        assert_eq!(exit.executed, 1);
        assert_eq!(cpu.pc(), expected_pc);
    }
    assert_eq!((cpu.reg(0), cpu.reg(1)), (1, 2));
}
