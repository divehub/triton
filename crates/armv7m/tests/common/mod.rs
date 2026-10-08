//! Shared test harness: a flat-memory `CpuBus` (flash, two SRAM banks, a few
//! logged MMIO windows) and a thin driver around `Cpu::run`.
#![allow(dead_code)]

use armv7m::{Cpu, CpuBus, CpuConfig, ExitReason, RunExit, BUS_IRQ_CHANGED};
use emu_core::Time;

pub const FLASH_BASE: u32 = 0x0800_0000;
pub const FLASH_SIZE: usize = 0x10_0000;
pub const SRAM1_BASE: u32 = 0x2000_0000;
pub const SRAM1_SIZE: usize = 0x1_8000;
pub const SRAM2_BASE: u32 = 0x1000_0000;
pub const SRAM2_SIZE: usize = 0x8000;
/// Window whose reads/writes are logged and which can raise notifications.
pub const MMIO_BASE: u32 = 0x4000_0000;
pub const MMIO_SIZE: u32 = 0x1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmioAccess {
    pub addr: u32,
    pub size: u8,
    pub write: bool,
    pub value: u32,
    pub icount: u64,
}

pub struct TestBus {
    pub flash: Vec<u8>,
    pub sram1: Vec<u8>,
    pub sram2: Vec<u8>,
    /// Backing store of the MMIO window (word registers, little endian bytes).
    pub mmio: Vec<u8>,
    pub mmio_log: Vec<MmioAccess>,
    pub notifications: u32,
    pub irq_changes: Vec<(u32, bool)>,
    /// When set, a write to this MMIO address raises these notification bits.
    pub write_trigger: Option<(u32, u32)>,
    /// IRQ changes to deliver when the trigger fires.
    pub trigger_irqs: Vec<(u32, bool)>,
    /// Test-only approximation of the peripheral space: register contents read back as
    /// written, RCC ready flags mirror the enable bits (lets boot code progress).
    pub periph_model: bool,
    periph: std::collections::HashMap<u32, u8>,
}

impl TestBus {
    pub fn new() -> Self {
        TestBus {
            flash: vec![0; FLASH_SIZE],
            sram1: vec![0; SRAM1_SIZE],
            sram2: vec![0; SRAM2_SIZE],
            mmio: vec![0; MMIO_SIZE as usize],
            mmio_log: Vec::new(),
            notifications: 0,
            irq_changes: Vec::new(),
            write_trigger: None,
            trigger_irqs: Vec::new(),
            periph_model: false,
            periph: std::collections::HashMap::new(),
        }
    }

    fn is_periph(addr: u32) -> bool {
        (0x4000_0000..0x6000_0000).contains(&addr) || addr >= 0xA000_0000 && addr < 0xB000_0000
    }

    fn periph_word(&self, addr: u32) -> u32 {
        let a = addr & !3;
        let mut v = 0u32;
        for i in 0..4 {
            v |= (*self.periph.get(&(a + i)).unwrap_or(&0) as u32) << (8 * i);
        }
        // RCC (STM32L4): CR ready flags follow the enable bits, CFGR.SWS follows SW.
        if (0x4002_1000..0x4002_1004).contains(&a) {
            let en = v;
            v |= ((en >> 0 & 1) << 1) | ((en >> 8 & 1) << 10) | ((en >> 16 & 1) << 17) | ((en >> 24 & 1) << 25) | ((en >> 26 & 1) << 27);
        }
        if a == 0x4002_1008 {
            v = (v & !0xC) | ((v & 3) << 2);
        }
        // BDCR.LSEON -> LSERDY, CSR.LSION -> LSIRDY
        if a == 0x4002_1090 || a == 0x4002_1094 {
            v |= (v & 1) << 1;
        }
        v
    }

    fn mem(&self, addr: u32) -> Option<(&[u8], usize)> {
        if addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE as u32 {
            Some((&self.flash, (addr - FLASH_BASE) as usize))
        } else if addr.wrapping_sub(SRAM1_BASE) < SRAM1_SIZE as u32 {
            Some((&self.sram1, (addr - SRAM1_BASE) as usize))
        } else if addr.wrapping_sub(SRAM2_BASE) < SRAM2_SIZE as u32 {
            Some((&self.sram2, (addr - SRAM2_BASE) as usize))
        } else {
            None
        }
    }

    fn mem_mut(&mut self, addr: u32) -> Option<(&mut Vec<u8>, usize)> {
        if addr.wrapping_sub(SRAM1_BASE) < SRAM1_SIZE as u32 {
            Some((&mut self.sram1, (addr - SRAM1_BASE) as usize))
        } else if addr.wrapping_sub(SRAM2_BASE) < SRAM2_SIZE as u32 {
            Some((&mut self.sram2, (addr - SRAM2_BASE) as usize))
        } else if addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE as u32 {
            Some((&mut self.flash, (addr - FLASH_BASE) as usize))
        } else {
            None
        }
    }

    fn is_mmio(addr: u32) -> bool {
        addr.wrapping_sub(MMIO_BASE) < MMIO_SIZE
    }

    pub fn read_bytes(&self, addr: u32, n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| self.peek8(addr.wrapping_add(i))).collect()
    }

    pub fn peek8(&self, addr: u32) -> u8 {
        match self.mem(addr) {
            Some((m, o)) => m[o],
            None => 0,
        }
    }

    pub fn peek16(&self, addr: u32) -> u16 {
        u16::from_le_bytes([self.peek8(addr), self.peek8(addr.wrapping_add(1))])
    }

    pub fn peek32(&self, addr: u32) -> u32 {
        u32::from_le_bytes([self.peek8(addr), self.peek8(addr + 1), self.peek8(addr + 2), self.peek8(addr + 3)])
    }

    pub fn poke8(&mut self, addr: u32, v: u8) {
        if let Some((m, o)) = self.mem_mut(addr) {
            m[o] = v;
        }
    }

    pub fn poke16(&mut self, addr: u32, v: u16) {
        for (i, b) in v.to_le_bytes().iter().enumerate() {
            self.poke8(addr.wrapping_add(i as u32), *b);
        }
    }

    pub fn poke32(&mut self, addr: u32, v: u32) {
        for (i, b) in v.to_le_bytes().iter().enumerate() {
            self.poke8(addr.wrapping_add(i as u32), *b);
        }
    }

    /// Writes halfwords (instructions) at `addr`.
    pub fn load_halfwords(&mut self, addr: u32, hws: &[u16]) {
        for (i, hw) in hws.iter().enumerate() {
            self.poke16(addr + 2 * i as u32, *hw);
        }
    }

    fn rd(&mut self, addr: u32, size: usize, icount: u64) -> u32 {
        if self.periph_model && Self::is_periph(addr) && !Self::is_mmio(addr) {
            let w = self.periph_word(addr);
            let shift = (addr & 3) * 8;
            return match size {
                1 => (w >> shift) & 0xFF,
                2 => (w >> shift) & 0xFFFF,
                _ => w,
            };
        }
        if Self::is_mmio(addr) {
            let o = (addr - MMIO_BASE) as usize;
            let mut v = 0u32;
            for i in 0..size {
                v |= (self.mmio[(o + i) % self.mmio.len()] as u32) << (8 * i);
            }
            self.mmio_log.push(MmioAccess { addr, size: size as u8, write: false, value: v, icount });
            return v;
        }
        let mut v = 0u32;
        for i in 0..size as u32 {
            v |= (self.peek8(addr.wrapping_add(i)) as u32) << (8 * i);
        }
        v
    }

    fn wr(&mut self, addr: u32, size: usize, value: u32, icount: u64) {
        if self.periph_model && Self::is_periph(addr) && !Self::is_mmio(addr) {
            for i in 0..size as u32 {
                self.periph.insert(addr.wrapping_add(i), (value >> (8 * i)) as u8);
            }
            return;
        }
        if Self::is_mmio(addr) {
            let o = (addr - MMIO_BASE) as usize;
            for i in 0..size {
                let n = self.mmio.len();
                self.mmio[(o + i) % n] = (value >> (8 * i)) as u8;
            }
            self.mmio_log.push(MmioAccess { addr, size: size as u8, write: true, value, icount });
            if let Some((a, bits)) = self.write_trigger {
                if a == addr {
                    self.notifications |= bits;
                    if bits & BUS_IRQ_CHANGED != 0 {
                        let ch = self.trigger_irqs.clone();
                        self.irq_changes.extend(ch);
                    }
                }
            }
            return;
        }
        // Flash is read-only for the core.
        if addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE as u32 {
            return;
        }
        for i in 0..size as u32 {
            self.poke8(addr.wrapping_add(i), (value >> (8 * i)) as u8);
        }
    }
}

impl CpuBus for TestBus {
    fn read8(&mut self, addr: u32, icount: u64) -> u8 {
        self.rd(addr, 1, icount) as u8
    }
    fn read16(&mut self, addr: u32, icount: u64) -> u16 {
        self.rd(addr, 2, icount) as u16
    }
    fn read32(&mut self, addr: u32, icount: u64) -> u32 {
        self.rd(addr, 4, icount)
    }
    fn write8(&mut self, addr: u32, value: u8, icount: u64) {
        self.wr(addr, 1, value as u32, icount)
    }
    fn write16(&mut self, addr: u32, value: u16, icount: u64) {
        self.wr(addr, 2, value as u32, icount)
    }
    fn write32(&mut self, addr: u32, value: u32, icount: u64) {
        self.wr(addr, 4, value, icount)
    }
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])> {
        if addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE as u32 {
            Some((FLASH_BASE, &self.flash))
        } else {
            None
        }
    }
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.peek16(addr)
    }
    fn is_plain_memory(&self, addr: u32) -> bool {
        self.mem(addr).is_some()
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

/// Cpu + bus + a running virtual clock.
pub struct Harness {
    pub cpu: Cpu,
    pub bus: TestBus,
    pub now: Time,
    pub tpi: Time,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_config(CpuConfig::default())
    }

    pub fn with_config(cfg: CpuConfig) -> Self {
        let mut h = Harness { cpu: Cpu::new(cfg), bus: TestBus::new(), now: 0, tpi: cfg.ticks_per_instruction };
        // A sane vector table base and stack so that tests can use exceptions.
        h.cpu.set_vtor(FLASH_BASE);
        h.cpu.set_sp(SRAM1_BASE + SRAM1_SIZE as u32);
        h
    }

    /// Writes instructions at `addr` and points the PC there (Thumb).
    pub fn load(&mut self, addr: u32, hws: &[u16]) {
        self.bus.load_halfwords(addr, hws);
        self.cpu.set_pc(addr);
    }

    /// Executes exactly `n` instructions like a board loop would: chunks that end early at a
    /// translation-block boundary (`StopRequested`, e.g. after an IRQ edge or a timer register
    /// write) are continued; sleeping, halted and locked-up cores stop the call. The returned
    /// `RunExit` aggregates the call: total `executed`, final `now` and the last `reason`.
    pub fn step(&mut self, n: u64) -> RunExit {
        let mut executed = 0u64;
        let mut spins = 0;
        loop {
            let remaining = n - executed;
            let exit = self.step_once(remaining);
            executed += exit.executed;
            let more = matches!(exit.reason, ExitReason::Deadline | ExitReason::StopRequested) && executed < n;
            if exit.executed == 0 {
                spins += 1;
            }
            if !more || spins > 1000 {
                return RunExit { now: exit.now, executed, reason: exit.reason };
            }
        }
    }

    /// A single `run` call asking for `n` instructions (it can return earlier).
    pub fn step_once(&mut self, n: u64) -> RunExit {
        let until = self.now + n * self.tpi;
        let exit = self.cpu.run(&mut self.bus, self.now, until);
        self.now = exit.now;
        exit
    }

    /// Runs until `until` (absolute) or an early exit.
    pub fn run_until(&mut self, until: Time) -> RunExit {
        let exit = self.cpu.run(&mut self.bus, self.now, until);
        self.now = exit.now;
        exit
    }

    pub fn r(&self, n: usize) -> u32 {
        self.cpu.reg(n)
    }

    pub fn set(&mut self, n: usize, v: u32) {
        self.cpu.set_reg(n, v);
    }

    /// N, Z, C, V as a nibble (N = bit 3).
    pub fn nzcv(&self) -> u32 {
        self.cpu.apsr() >> 28
    }

    pub fn q(&self) -> bool {
        self.cpu.apsr() & (1 << 27) != 0
    }

    pub fn ge(&self) -> u32 {
        (self.cpu.apsr() >> 16) & 0xF
    }
}

pub fn assert_exit(exit: RunExit, reason: ExitReason) {
    assert_eq!(exit.reason, reason, "unexpected exit {:?}", exit);
}

/// Parses an S-record file into `(address, bytes)` segments (S1/S2/S3 data records).
pub fn parse_srec(text: &str) -> Vec<(u32, Vec<u8>)> {
    let mut segs: Vec<(u32, Vec<u8>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.len() < 4 || !line.starts_with('S') {
            continue;
        }
        let kind = line.as_bytes()[1];
        let alen = match kind {
            b'1' => 2,
            b'2' => 3,
            b'3' => 4,
            _ => continue,
        };
        let bytes: Vec<u8> = (2..line.len() - 1).step_by(2).map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap()).collect();
        let count = bytes[0] as usize;
        assert_eq!(bytes.len(), count + 1, "bad S-record length: {line}");
        let mut addr = 0u32;
        for b in &bytes[1..1 + alen] {
            addr = (addr << 8) | *b as u32;
        }
        let data = bytes[1 + alen..bytes.len() - 1].to_vec();
        match segs.last_mut() {
            Some((a, d)) if *a as usize + d.len() == addr as usize => d.extend_from_slice(&data),
            _ => segs.push((addr, data)),
        }
    }
    segs
}

/// Path of a file relative to the repository root. A `firmware/...` path is first looked up below the directory
/// named by `NGC_FIRMWARE_DIR` (the directory that holds the release directories), if that is set and has the file.
pub fn repo_path(rel: &str) -> std::path::PathBuf {
    if let (Some(dir), Some(rest)) = (std::env::var_os("NGC_FIRMWARE_DIR"), rel.strip_prefix("firmware/")) {
        let candidate = std::path::Path::new(&dir).join(rest);
        if candidate.is_file() {
            return candidate;
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}
