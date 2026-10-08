//! `CpuBus` implementation: the view of the machine the CPU core sees during `Cpu::run`.
//!
//! Hot path: flash and SRAM1 accesses are resolved inline by constant range checks and plain
//! slice indexing (no `dyn`, no allocation, no hashing). Everything else (SRAM2, straddling
//! accesses, MMIO, unmapped addresses) goes through one out-of-line call into the machine, with
//! the exact time of the accessing instruction
//! `chunk_start_time + (icount - chunk_start_icount) * ticks_per_instruction`.
//!
//! The machine does **not** show that time to peripherals: like Renode, they see the lagging
//! clock-source time (chunk start or last sync) unless the register is a declared sync register or
//! the model calls `ctx.sync_time()`, in which case the machine first advances its clock to the
//! exact time passed here.
//!
//! The `icount` the CPU passes is its monotonic executed-instruction counter at the access. The
//! board takes it as given: an MMIO access made by the first instruction of a chunk is at
//! `chunk_start_time` when the core passes the count *before* that instruction, and one
//! instruction later when it passes the count *including* it (a 10 ns difference at 100 MIPS).

use crate::memory::{FLASH_BASE, FLASH_SIZE, LAYOUT, SRAM1_BASE, SRAM1_SIZE, SRAM2_BASE, SRAM2_SIZE};
use armv7m::{CpuBus, BUS_IRQ_CHANGED, BUS_STOP_REQUESTED};
use emu_core::{MachineCore, Time, Width, NOTIFY_IRQ_CHANGED, NOTIFY_STOP_REQUESTED};

const _: () = assert!(BUS_IRQ_CHANGED == NOTIFY_IRQ_CHANGED, "notification bits must match");
const _: () = assert!(BUS_STOP_REQUESTED == NOTIFY_STOP_REQUESTED, "notification bits must match");

pub struct BusView<'a> {
    core: &'a mut MachineCore,
    start_time: Time,
    start_icount: u64,
    ticks_per_instruction: Time,
}

#[inline(always)]
fn rd16(m: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(m[o..o + 2].try_into().unwrap())
}

#[inline(always)]
fn rd32(m: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(m[o..o + 4].try_into().unwrap())
}

#[inline(always)]
fn wr16(m: &mut [u8], o: usize, v: u16) {
    m[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline(always)]
fn wr32(m: &mut [u8], o: usize, v: u32) {
    m[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

impl<'a> BusView<'a> {
    /// `start_time` / `start_icount` are the board time and `Cpu::instructions()` when the chunk
    /// starts. The machine must use the NGC memory layout (checked in debug builds).
    pub fn new(core: &'a mut MachineCore, start_time: Time, start_icount: u64, ticks_per_instruction: Time) -> Self {
        debug_assert!(core.mem.layout == LAYOUT, "BusView requires the NGC memory layout");
        Self { core, start_time, start_icount, ticks_per_instruction }
    }

    /// Exact virtual time of an access made at executed-instruction count `icount`.
    #[inline(always)]
    pub fn time_at(&self, icount: u64) -> Time {
        self.start_time + icount.wrapping_sub(self.start_icount).wrapping_mul(self.ticks_per_instruction)
    }

    pub fn core(&mut self) -> &mut MachineCore {
        self.core
    }

    /// Everything the inline paths did not serve. SRAM2 is plain memory like SRAM1 (a second, smaller
    /// region): it is answered here before the exact instruction time is even computed. Unaligned accesses
    /// inside a plain memory are single host accesses (Renode `MappedMemory`).
    #[inline(never)]
    fn read_slow(&mut self, addr: u32, width: Width, icount: u64) -> u32 {
        let o = addr.wrapping_sub(SRAM2_BASE);
        if o <= SRAM2_SIZE - width.bytes() {
            let memory = &self.core.mem.sram2;
            let o = o as usize;
            return match width {
                Width::Byte => u32::from(memory[o]),
                Width::Half => u32::from(rd16(memory, o)),
                Width::Word => rd32(memory, o),
            };
        }
        let now = self.time_at(icount);
        let value = self.core.cpu_read(addr, width, now);
        if access_trace::enabled() {
            access_trace::record(icount, addr, width, false, value);
        }
        value
    }

    #[inline(never)]
    fn write_slow(&mut self, addr: u32, width: Width, value: u32, icount: u64) {
        let o = addr.wrapping_sub(SRAM2_BASE);
        if o <= SRAM2_SIZE - width.bytes() {
            let memory = &mut self.core.mem.sram2;
            let o = o as usize;
            match width {
                Width::Byte => memory[o] = value as u8,
                Width::Half => wr16(memory, o, value as u16),
                Width::Word => wr32(memory, o, value),
            }
            return;
        }
        let now = self.time_at(icount);
        self.core.cpu_write(addr, width, value, now);
        if access_trace::enabled() {
            access_trace::record(icount, addr, width, true, value);
        }
    }
}

/// Opt-in log of the CPU's accesses outside flash and SRAM (MMIO, unmapped addresses, flash stores): a
/// bounded ring of the latest accesses for diagnosing divergences from the reference ("which register read
/// returned what just before the first different instruction"). Disabled by default; the inline plain-memory
/// paths never look at it and the slow path tests one thread-local flag.
pub mod access_trace {
    use emu_core::Width;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    /// One traced access.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Record {
        /// Tag set by [`set_tag`] when the access happened (the system uses 0 = main, 1 = handset).
        pub tag: u8,
        /// Executed-instruction count the core passed with the access: the count at the start of the
        /// current translation block (tlib/Renode `SyncTime` semantics), not the count before the
        /// accessing instruction.
        pub icount: u64,
        pub address: u32,
        pub width: Width,
        pub write: bool,
        /// Value read or written (zero-extended).
        pub value: u32,
    }

    struct Log {
        capacity: usize,
        records: VecDeque<Record>,
    }

    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static TAG: Cell<u8> = const { Cell::new(0) };
        static LOG: RefCell<Log> = const { RefCell::new(Log { capacity: 0, records: VecDeque::new() }) };
    }

    #[inline(always)]
    pub fn enabled() -> bool {
        ENABLED.with(Cell::get)
    }

    /// Starts keeping the latest `capacity` accesses (replaces a previous log).
    pub fn start(capacity: usize) {
        LOG.with(|log| {
            let mut log = log.borrow_mut();
            log.capacity = capacity.max(1);
            log.records = VecDeque::with_capacity(capacity.clamp(1, 1 << 16));
        });
        ENABLED.with(|flag| flag.set(true));
    }

    /// Stops tracing and returns the kept accesses, oldest first.
    pub fn stop() -> Vec<Record> {
        ENABLED.with(|flag| flag.set(false));
        LOG.with(|log| std::mem::take(&mut log.borrow_mut().records).into_iter().collect())
    }

    /// Labels the following accesses (the system sets it before it runs each board).
    #[inline(always)]
    pub fn set_tag(tag: u8) {
        TAG.with(|cell| cell.set(tag));
    }

    pub(super) fn record(icount: u64, address: u32, width: Width, write: bool, value: u32) {
        let tag = TAG.with(Cell::get);
        LOG.with(|log| {
            let mut log = log.borrow_mut();
            if log.records.len() >= log.capacity {
                log.records.pop_front();
            }
            log.records.push_back(Record { tag, icount, address, width, write, value: value & width.mask() });
        });
    }
}

impl CpuBus for BusView<'_> {
    #[inline(always)]
    fn read8(&mut self, addr: u32, icount: u64) -> u8 {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o < SRAM1_SIZE {
            return self.core.mem.sram1[o as usize];
        }
        let o = addr.wrapping_sub(FLASH_BASE);
        if o < FLASH_SIZE {
            return self.core.mem.flash[o as usize];
        }
        self.read_slow(addr, Width::Byte, icount) as u8
    }

    #[inline(always)]
    fn read16(&mut self, addr: u32, icount: u64) -> u16 {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o <= SRAM1_SIZE - 2 {
            return rd16(&self.core.mem.sram1, o as usize);
        }
        let o = addr.wrapping_sub(FLASH_BASE);
        if o <= FLASH_SIZE - 2 {
            return rd16(&self.core.mem.flash, o as usize);
        }
        self.read_slow(addr, Width::Half, icount) as u16
    }

    #[inline(always)]
    fn read32(&mut self, addr: u32, icount: u64) -> u32 {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o <= SRAM1_SIZE - 4 {
            return rd32(&self.core.mem.sram1, o as usize);
        }
        let o = addr.wrapping_sub(FLASH_BASE);
        if o <= FLASH_SIZE - 4 {
            return rd32(&self.core.mem.flash, o as usize);
        }
        self.read_slow(addr, Width::Word, icount)
    }

    #[inline(always)]
    fn write8(&mut self, addr: u32, value: u8, icount: u64) {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o < SRAM1_SIZE {
            self.core.mem.sram1[o as usize] = value;
            return;
        }
        self.write_slow(addr, Width::Byte, u32::from(value), icount);
    }

    #[inline(always)]
    fn write16(&mut self, addr: u32, value: u16, icount: u64) {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o <= SRAM1_SIZE - 2 {
            wr16(&mut self.core.mem.sram1, o as usize, value);
            return;
        }
        self.write_slow(addr, Width::Half, u32::from(value), icount);
    }

    #[inline(always)]
    fn write32(&mut self, addr: u32, value: u32, icount: u64) {
        let o = addr.wrapping_sub(SRAM1_BASE);
        if o <= SRAM1_SIZE - 4 {
            wr32(&mut self.core.mem.sram1, o as usize, value);
            return;
        }
        self.write_slow(addr, Width::Word, value, icount);
    }

    #[inline]
    fn code_region(&self, addr: u32) -> Option<(u32, &[u8])> {
        if addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE {
            Some((FLASH_BASE, &self.core.mem.flash))
        } else {
            None
        }
    }

    #[inline]
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.core.mem.read(addr, Width::Half).map_or(0, |v| v as u16)
    }

    #[inline]
    fn is_plain_memory(&self, addr: u32) -> bool {
        addr.wrapping_sub(SRAM1_BASE) < SRAM1_SIZE
            || addr.wrapping_sub(FLASH_BASE) < FLASH_SIZE
            || addr.wrapping_sub(SRAM2_BASE) < SRAM2_SIZE
    }

    #[inline(always)]
    fn take_notifications(&mut self) -> u32 {
        self.core.take_notifications()
    }

    fn drain_irq_changes(&mut self, sink: &mut dyn FnMut(u32, bool)) {
        self.core.drain_irq_changes(sink);
    }

    /// Renode `cpu.SyncTime()` for the core-internal registers (SysTick `CVR`, DWT `CYCCNT`), which never
    /// reach the machine's MMIO table: the machine clock advances to the exact time of the instruction
    /// that is about to read them, `slice_start_time + (icount - slice_start_icount) * ticks_per_instruction`,
    /// firing every event that is due on the way - the same path a declared sync register
    /// (`Peripheral::sync_registers`) takes for CPU accesses. The core polls `take_notifications`
    /// right afterwards, so interrupts raised by the fired events are applied before the register is read.
    #[inline]
    fn sync_time(&mut self, icount: u64) {
        let exact = self.time_at(icount);
        if exact > self.core.clock_time() {
            self.core.advance_clock(exact);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::{impl_peripheral_any, AccessPolicy, Ctx, Peripheral, SyncRegister, TICKS_PER_INSTRUCTION};
    use std::cell::RefCell;
    use std::rc::Rc;

    const TPI: Time = TICKS_PER_INSTRUCTION;

    type Log = Rc<RefCell<Vec<String>>>;

    struct Probe {
        log: Log,
        raise: bool,
    }

    impl Peripheral for Probe {
        fn name(&self) -> &str {
            "probe"
        }

        fn access_policy(&self) -> AccessPolicy {
            AccessPolicy::EXACT
        }

        /// Offsets 0x14..0x17 are read after `cpu.SyncTime()` (like a timer CNT register).
        fn sync_registers(&self) -> Vec<SyncRegister> {
            vec![SyncRegister::read(0x14)]
        }

        fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32 {
            self.log.borrow_mut().push(format!("r{}@{:x}@{}", width.bytes(), offset, ctx.now()));
            0x1122_3344 >> ((offset & 3) * 8)
        }

        fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push(format!("w{}@{:x}={:x}@{}", width.bytes(), offset, value, ctx.now()));
            if self.raise {
                ctx.set_output(0, value & 1 != 0);
            }
            if value == 0xDEAD {
                ctx.request_cpu_stop();
            }
        }

        fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
            self.log.borrow_mut().push(format!("e{token}@{scheduled}@{}", ctx.now()));
            if self.raise {
                ctx.set_output(0, token & 1 != 0);
            }
        }

        impl_peripheral_any!();
    }

    #[test]
    fn access_trace_keeps_the_latest_slow_path_accesses_only() {
        let (mut core, _) = machine(false);
        {
            let mut bus = BusView::new(&mut core, 0, 0, TPI);
            bus.write32(0x4000_0010, 1, 5); // before tracing: not recorded
            access_trace::start(3);
            access_trace::set_tag(1);
            bus.write32(SRAM1_BASE, 7, 6); // plain memory: never recorded
            bus.read32(0x1000_0000, 6); // SRAM2: plain, never recorded
            bus.write32(0x4000_0010, 0xAA, 7);
            bus.read16(0x4000_0012, 8);
            bus.write8(0x4000_0011, 0x55, 9);
            bus.read32(0x5000_0000, 10); // unmapped
            let records = access_trace::stop();
            assert_eq!(records.len(), 3, "a ring of the latest three");
            assert_eq!((records[0].icount, records[0].address, records[0].write, records[0].tag), (8, 0x4000_0012, false, 1));
            assert_eq!(records[0].value, 0x1122);
            assert_eq!((records[1].icount, records[1].address, records[1].write, records[1].value), (9, 0x4000_0011, true, 0x55));
            assert_eq!((records[2].address, records[2].value, records[2].width), (0x5000_0000, 0, Width::Word));
            // Stopped: nothing is recorded any more.
            bus.write32(0x4000_0010, 2, 11);
            assert!(access_trace::stop().is_empty());
        }
    }

    #[test]
    fn sync_time_advances_the_machine_clock_to_the_exact_instruction_time() {
        // The core calls `sync_time(icount)` before a SysTick CVR / DWT CYCCNT read: events due up to the
        // start of that instruction fire (at their own times), later ones stay queued, and the clock
        // lands on the exact instruction time.
        let (mut core, log) = machine(true);
        core.advance_clock(1_000_000);
        let id = core.find("probe").unwrap();
        core.with_peripheral::<Probe, _>(id, |_, ctx| {
            ctx.schedule_at(1_000_000 + 5 * TPI, 1);
            ctx.schedule_at(1_000_000 + 30 * TPI, 2);
        });
        core.take_notifications();
        core.drain_irq_changes(&mut |_, _| {});
        let mut bus = BusView::new(&mut core, 1_000_000, 500, TPI);
        bus.sync_time(500); // the first instruction of the chunk: nothing is due before it
        assert_eq!(bus.core().clock_time(), 1_000_000);
        assert!(log.borrow().is_empty());
        bus.sync_time(520); // 20 instructions into the chunk: the 5-instruction event fired, the 30 one waits
        assert_eq!(bus.core().clock_time(), 1_000_000 + 20 * TPI);
        assert_eq!(*log.borrow(), [format!("e1@{0}@{0}", 1_000_000 + 5 * TPI)]);
        assert_eq!(bus.take_notifications() & BUS_IRQ_CHANGED, BUS_IRQ_CHANGED, "the event's line change reaches the core");
        let mut seen = Vec::new();
        bus.drain_irq_changes(&mut |irq, level| seen.push((irq, level)));
        assert_eq!(seen, [(17, true)]);
        bus.sync_time(510); // never moves time backwards
        assert_eq!(bus.core().clock_time(), 1_000_000 + 20 * TPI);
        bus.sync_time(540);
        assert_eq!(bus.core().clock_time(), 1_000_000 + 40 * TPI);
        assert_eq!(log.borrow().len(), 2);
        assert_eq!(log.borrow()[1], format!("e2@{}@{}", 1_000_000 + 30 * TPI, 1_000_000 + 30 * TPI));
    }

    fn machine(raise: bool) -> (MachineCore, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut core = MachineCore::new(LAYOUT);
        let id = core.add_mapped(0x4000_0000, 0x400, Box::new(Probe { log: log.clone(), raise })).unwrap();
        core.connect_irq(id, 0, 17).unwrap();
        core.take_notifications();
        core.drain_irq_changes(&mut |_, _| {});
        (core, log)
    }

    #[test]
    fn plain_memory_round_trips_at_every_width_and_alignment() {
        let (mut core, _) = machine(false);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        for base in [SRAM1_BASE, SRAM2_BASE, FLASH_BASE + 0x4000] {
            bus.write32(base, 0x1122_3344, 0);
            assert_eq!(bus.read32(base, 0), 0x1122_3344);
            assert_eq!(bus.read16(base, 0), 0x3344);
            assert_eq!(bus.read16(base + 2, 0), 0x1122);
            assert_eq!(bus.read8(base + 1, 0), 0x33);
            bus.write16(base + 1, 0xAABB, 0);
            assert_eq!(bus.read32(base, 0), 0x11AA_BB44);
            bus.write8(base + 3, 0x7F, 0);
            assert_eq!(bus.read32(base, 0), 0x7FAA_BB44);
            // Unaligned word access.
            bus.write32(base + 5, 0xCAFE_BABE, 0);
            assert_eq!(bus.read32(base + 5, 0), 0xCAFE_BABE);
            assert_eq!(bus.read8(base + 4, 0), 0);
            assert_eq!(bus.read8(base + 9, 0), 0);
        }
    }

    #[test]
    fn region_edges() {
        let (mut core, _) = machine(false);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        for (base, size) in [(SRAM1_BASE, SRAM1_SIZE), (FLASH_BASE, FLASH_SIZE), (SRAM2_BASE, SRAM2_SIZE)] {
            bus.write32(base + size - 4, 0xDEAD_BEEF, 0);
            assert_eq!(bus.read32(base + size - 4, 0), 0xDEAD_BEEF);
            assert_eq!(bus.read16(base + size - 2, 0), 0xDEAD);
            assert_eq!(bus.read8(base + size - 1, 0), 0xDE);
            // A word starting 3 bytes before the end spills into unmapped space: bytes past the
            // end read as zero, the in-range byte pair is composed correctly.
            assert_eq!(bus.read32(base + size - 3, 0), 0x00DE_ADBE & 0x00FF_FFFF);
            assert_eq!(bus.read16(base + size - 1, 0), 0x00DE);
            // Straddling writes keep the in-range bytes and drop the rest.
            bus.write16(base + size - 1, 0x1234, 0);
            assert_eq!(bus.read8(base + size - 1, 0), 0x34);
            assert_eq!(bus.read8(base + size - 2, 0), 0xAD);
        }
        assert!(bus.core().log.count(emu_core::LogLevel::Warning) > 0);
        // Addresses just outside every plain region and outside MMIO: unmapped.
        assert_eq!(bus.read32(SRAM1_BASE + SRAM1_SIZE, 0), 0);
        assert_eq!(bus.read8(SRAM1_BASE - 1, 0), 0);
    }

    #[test]
    fn flash_is_writable_like_renode_mapped_memory() {
        let (mut core, _) = machine(false);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        let epoch = bus.core().mem.flash_epoch();
        bus.write32(FLASH_BASE + 0x8000, 0xAABB_CCDD, 0);
        assert_eq!(bus.read32(FLASH_BASE + 0x8000, 0), 0xAABB_CCDD);
        assert_eq!(bus.core().mem.flash_epoch(), epoch + 1);
        bus.write8(FLASH_BASE + 0x8001, 1, 0);
        bus.write16(FLASH_BASE + 0x8002, 2, 0);
        assert_eq!(bus.core().mem.flash_epoch(), epoch + 3);
        bus.write32(SRAM1_BASE, 1, 0);
        assert_eq!(bus.core().mem.flash_epoch(), epoch + 3, "SRAM writes do not touch the epoch");
    }

    #[test]
    fn mmio_sees_the_lagging_clock_time_except_for_sync_registers() {
        let (mut core, log) = machine(false);
        core.advance_clock(1_000_000); // the chunk starts at clock time 1 ms
        let mut bus = BusView::new(&mut core, 1_000_000, 500, TPI);
        // Plain registers see the clock time of the chunk start whatever instruction accesses them.
        bus.write32(0x4000_0010, 7, 500);
        bus.write8(0x4000_0011, 8, 501);
        assert_eq!(bus.read16(0x4000_0012, 10_500), 0x1122);
        // A declared sync register (offset 0x14, read) advances the clock to the exact instruction time first.
        assert_eq!(bus.read32(0x4000_0014, 520), 0x1122_3344);
        assert_eq!(bus.read16(0x4000_0012, 10_500), 0x1122, "the clock stays at the synced time afterwards");
        assert_eq!(
            *log.borrow(),
            [
                format!("w4@10=7@{}", 1_000_000),
                format!("w1@11=8@{}", 1_000_000),
                format!("r2@12@{}", 1_000_000),
                format!("r4@14@{}", 1_000_000 + 20 * TPI),
                format!("r2@12@{}", 1_000_000 + 20 * TPI),
            ]
        );
        assert_eq!(bus.time_at(500), 1_000_000);
        assert_eq!(bus.time_at(10_500), 1_000_000 + 10_000 * TPI);
        assert_eq!(bus.core().clock_time(), 1_000_000 + 20 * TPI);
    }

    #[test]
    fn unaligned_mmio_accesses_are_split_like_the_translation_library() {
        // Renode tlib (docs/renode-semantics.md 7.5): unaligned MMIO load = two aligned loads of
        // the same width merged by shift; unaligned MMIO store = single-byte stores, high to low.
        let (mut core, log) = machine(false);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        // Probe returns 0x11223344 >> ((offset & 3) * 8) for every read.
        let word = bus.read32(0x4000_0002, 0);
        let low = 0x1122_3344u32; // aligned read at offset 0: shift 0
        let high = 0x1122_3344u32; // aligned read at offset 4: shift 0
        assert_eq!(word, ((u64::from(low) | (u64::from(high) << 32)) >> 16) as u32);
        let half = bus.read16(0x4000_0003, 0);
        // Aligned halfword reads at offsets 2 and 4: 0x1122_3344 >> 16 = 0x1122 and 0x3344.
        assert_eq!(half, ((0x1122u32 | (0x3344 << 16)) >> 8) as u16 & 0xFFFF);
        bus.write32(0x4000_0001, 0xAABB_CCDD, 1);
        bus.write16(0x4000_0007, 0x1234, 2);
        assert_eq!(
            *log.borrow(),
            [
                "r4@0@0", "r4@4@0", "r2@2@0", "r2@4@0",
                // 0x4000_0001 word store -> bytes 4,3,2,1 (highest address first)
                "w1@4=aa@0", "w1@3=bb@0", "w1@2=cc@0", "w1@1=dd@0",
                "w1@8=12@0", "w1@7=34@0",
            ]
        );
        // Aligned accesses and unaligned accesses to plain memory are untouched.
        log.borrow_mut().clear();
        bus.read32(0x4000_0004, 3);
        bus.write32(SRAM1_BASE + 1, 0x5566_7788, 3);
        assert_eq!(bus.read32(SRAM1_BASE + 1, 3), 0x5566_7788);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn code_region_and_plain_memory_queries() {
        let (mut core, _) = machine(false);
        core.mem.flash[0x4000] = 0xAB;
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        let (base, bytes) = bus.code_region(0x0800_4000).unwrap();
        assert_eq!(base, FLASH_BASE);
        assert_eq!(bytes.len(), FLASH_SIZE as usize);
        assert_eq!(bytes[0x4000], 0xAB);
        assert!(bus.code_region(SRAM1_BASE).is_none());
        assert!(bus.code_region(0x4000_0000).is_none());
        assert!(bus.code_region(FLASH_BASE - 1).is_none());
        assert_eq!(bus.fetch16(0x0800_4000), 0x00AB);
        bus.write16(SRAM1_BASE + 2, 0x1234, 0);
        assert_eq!(bus.fetch16(SRAM1_BASE + 2), 0x1234);
        assert_eq!(bus.fetch16(0x4000_0000), 0, "fetches never touch peripherals");
        assert!(bus.is_plain_memory(SRAM1_BASE) && bus.is_plain_memory(0x0800_0000) && bus.is_plain_memory(SRAM2_BASE + 4));
        assert!(!bus.is_plain_memory(0x4000_0000) && !bus.is_plain_memory(0xE000_ED10) && !bus.is_plain_memory(SRAM1_BASE + SRAM1_SIZE));
    }

    #[test]
    fn notifications_and_irq_changes() {
        let (mut core, _) = machine(true);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        assert_eq!(bus.take_notifications(), 0);
        bus.write32(0x4000_0000, 1, 0);
        assert_eq!(bus.take_notifications(), BUS_IRQ_CHANGED);
        assert_eq!(bus.take_notifications(), 0, "bits are consumed");
        let mut seen = Vec::new();
        bus.drain_irq_changes(&mut |irq, level| seen.push((irq, level)));
        assert_eq!(seen, [(17, true)]);
        seen.clear();
        bus.drain_irq_changes(&mut |irq, level| seen.push((irq, level)));
        assert!(seen.is_empty());
        bus.write32(0x4000_0000, 0, 1);
        bus.write32(0x4000_0000, 1, 2);
        bus.drain_irq_changes(&mut |irq, level| seen.push((irq, level)));
        assert_eq!(seen, [(17, false), (17, true)], "changes keep their order");
        bus.take_notifications();
        bus.write32(0x4000_0000, 0xDEAD, 3);
        assert_eq!(bus.take_notifications() & BUS_STOP_REQUESTED, BUS_STOP_REQUESTED);
    }

    #[test]
    fn unmapped_mmio_reads_zero_and_ignores_writes() {
        let (mut core, _) = machine(false);
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        assert_eq!(bus.read32(0x5000_0000, 0), 0);
        bus.write32(0x5000_0000, 5, 0);
        assert_eq!(bus.read8(0x5000_0001, 0), 0);
        assert_eq!(bus.core().stats.unmapped_reads, 2);
        assert_eq!(bus.core().stats.unmapped_writes, 1);
    }

    /// Minimal register file: measures the framework's dispatch cost, not the model's.
    struct Regs([u32; 8]);

    impl Peripheral for Regs {
        fn name(&self) -> &str {
            "regs"
        }

        fn read(&mut self, offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
            self.0[(offset as usize >> 2) & 7]
        }

        fn write(&mut self, offset: u32, _width: Width, value: u32, _ctx: &mut Ctx<'_>) {
            self.0[(offset as usize >> 2) & 7] = value;
        }

        impl_peripheral_any!();
    }

    #[test]
    #[ignore = "micro-benchmark; run with --ignored --nocapture"]
    fn bench_bus_paths() {
        let mut core = MachineCore::new(LAYOUT);
        core.add_mapped(0x4000_0000, 0x400, Box::new(Regs([0; 8]))).unwrap();
        let mut bus = BusView::new(&mut core, 0, 0, TPI);
        let n = 100_000_000u64;
        let mut acc = 0u32;
        let start = std::time::Instant::now();
        for i in 0..n {
            let a = SRAM1_BASE + ((i as u32 * 4) & 0xFFFC);
            bus.write32(a, i as u32, i);
            acc = acc.wrapping_add(bus.read32(a, i));
            acc = acc.wrapping_add(bus.read32(FLASH_BASE + 0x4000 + ((i as u32 * 4) & 0xFFFC), i));
        }
        let plain = start.elapsed();
        println!("plain (sram write + sram read + flash read): {:.2} ns per triple (acc {acc})", plain.as_nanos() as f64 / n as f64);
        let m = 10_000_000u64;
        let start = std::time::Instant::now();
        for i in 0..m {
            bus.write32(0x4000_0000, i as u32, i);
            acc = acc.wrapping_add(bus.read32(0x4000_0004, i));
        }
        let mmio = start.elapsed();
        println!("mmio (write + read through the table and a dyn call): {:.1} ns per pair (acc {acc})", mmio.as_nanos() as f64 / m as f64);
        let start = std::time::Instant::now();
        for i in 0..m {
            acc = acc.wrapping_add(bus.read32(0x5000_0000 + ((i as u32 & 7) << 2), i));
        }
        let unmapped = start.elapsed();
        println!("unmapped read (warned once per address): {:.1} ns per read (acc {acc})", unmapped.as_nanos() as f64 / m as f64);
        let start = std::time::Instant::now();
        for i in 0..m {
            acc = acc.wrapping_add(bus.read32(0x1000_0000 + ((i as u32 & 7) << 2), i));
        }
        let sram2 = start.elapsed();
        println!("sram2 read (slow path): {:.1} ns per read (acc {acc})", sram2.as_nanos() as f64 / m as f64);
    }
}
