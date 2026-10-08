//! The complete example of `docs/framework.md` section 17, compiled and run. Keep both in sync.

use emu_core::clock::{Direction, LimitTimer, LimitTimerConfig};
use emu_core::testing::{Harness, IrqChange};
use emu_core::*;

const US: Time = TICKS_PER_MICROSECOND;

// Register offsets.
const CTRL: u32 = 0x00; // bit 0 EN, bit 1 IRQ_EN
const LOAD: u32 = 0x04; // counts per period (Renode STM32_Timer style: limit = LOAD)
const COUNT: u32 = 0x08; // read-only counter; Renode would call cpu.SyncTime() in its read callback
const STATUS: u32 = 0x0C; // bit 0 UIF (period elapsed), write 1 to clear
const DMA_ADDR: u32 = 0x10; // if non-zero, every period stores the period count there

const CTRL_EN: u32 = 1;
const CTRL_IRQ_EN: u32 = 2;
const IRQ_LINE: u32 = 0; // output line 0 = interrupt request
const TIMER: u64 = 1; // clock-entry token

pub struct CountdownTimer {
    timer: LimitTimer,
    ctrl: u32,
    status: u32,
    dma_addr: u32,
    periods: u32,
}

impl CountdownTimer {
    pub fn new() -> Self {
        let cfg = LimitTimerConfig {
            limit: 0xFFFF_FFFF,
            direction: Direction::Ascending,
            event_enabled: true,
            ..LimitTimerConfig::new(1_000_000)
        };
        Self { timer: LimitTimer::new(cfg, TIMER), ctrl: 0, status: 0, dma_addr: 0, periods: 0 }
    }

    fn register(&self, offset: u32, clock: &dyn ClockRead) -> Option<u32> {
        match offset {
            CTRL => Some(self.ctrl),
            LOAD => Some(self.timer.limit(clock) as u32),
            COUNT => Some(self.timer.value(clock) as u32),
            STATUS => Some(self.status),
            DMA_ADDR => Some(self.dma_addr),
            _ => None,
        }
    }

    fn update_irq(&self, ctx: &mut Ctx<'_>) {
        let level = self.status & 1 != 0 && self.ctrl & CTRL_IRQ_EN != 0;
        ctx.set_output(IRQ_LINE, level); // delivered to the targets only if the level changed
    }
}

impl Peripheral for CountdownTimer {
    fn name(&self) -> &str {
        "countdown"
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.attach(ctx); // clock-entry creation order = registration order
    }

    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.reset(ctx);
        self.ctrl = 0;
        self.status = 0;
        self.update_irq(ctx);
    }

    // A Renode-style 32-bit-only device: byte and halfword accesses are turned into aligned
    // word accesses by the bus (read-modify-write for writes), like [AllowedTranslations].
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD)
    }

    // The CPU reads COUNT after cpu.SyncTime() in the original: advance the clock to the exact
    // instruction time first.
    fn sync_registers(&self) -> Vec<SyncRegister> {
        vec![SyncRegister::read(COUNT)]
    }

    fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
        match self.register(offset, ctx) {
            Some(value) => value,
            None => {
                ctx.warn_once(u64::from(offset), format_args!("read from unimplemented register 0x{offset:X}"));
                0
            }
        }
    }

    fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
        match offset {
            CTRL => {
                self.ctrl = value & (CTRL_EN | CTRL_IRQ_EN);
                // LimitTimer setters request a return from the CPU chunk, like Renode's.
                self.timer.set_enabled(ctx, self.ctrl & CTRL_EN != 0);
                self.update_irq(ctx);
            }
            LOAD => self.timer.set_limit(ctx, u64::from(value.max(1))),
            STATUS => {
                self.status &= !value;
                self.update_irq(ctx);
            }
            DMA_ADDR => self.dma_addr = value,
            _ => ctx.warn_once(u64::from(offset) | 1 << 32, format_args!("write to unimplemented register 0x{offset:X}")),
        }
    }

    fn on_event(&mut self, token: u64, _scheduled: Time, ctx: &mut Ctx<'_>) {
        // `ctx.now()` is the entry's own limit time; the entry re-arms itself (periodic).
        if token == TIMER && self.timer.on_limit_reached() {
            self.periods += 1;
            self.status |= 1;
            if self.dma_addr != 0 {
                ctx.mem_write(self.dma_addr, Width::Word, self.periods);
            }
            self.update_irq(ctx);
        }
    }

    fn peek(&self, offset: u32, _width: Width, view: &View<'_>) -> Option<u32> {
        self.register(offset, view)
    }

    fn summary(&self, view: &View<'_>) -> String {
        format!(
            "countdown timer: enabled={}, limit={}, value={}, periods={}",
            self.timer.enabled(view),
            self.timer.limit(view),
            self.timer.value(view),
            self.periods
        )
    }

    impl_peripheral_any!();
}

const BASE: u32 = 0x4000_1000;

#[test]
fn countdown_timer_example() {
    let mut h = Harness::new();
    let id = h.add_mapped(BASE, 0x400, CountdownTimer::new());
    h.connect_irq(id, IRQ_LINE, 25);
    h.clear_irq_changes(); // drop the connect-time level push

    // 10 us period (10 counts of 1 MHz), interrupt enabled, DMA target in SRAM.
    h.write32(BASE + LOAD, 10);
    h.write32(BASE + DMA_ADDR, 0x2000_0100);
    h.write32(BASE + CTRL, CTRL_EN | CTRL_IRQ_EN);
    assert_eq!(h.next_event_time(), Some(10 * US));
    assert!(h.take_stop_request(), "LimitTimer setters ask the CPU to return, like Renode's");

    h.advance_to(25 * US);
    assert_eq!(h.get::<CountdownTimer>(id).periods, 2, "limits at exactly 10 us and 20 us");
    assert_eq!(h.read32(0x2000_0100), 2, "the second period wrote its count through the bus");
    assert_eq!(h.irq_changes(), [IrqChange { time: 10 * US, irq: 25, level: true }]);
    assert!(h.irq_level(25));
    assert_eq!(h.next_event_time(), Some(30 * US));

    // COUNT is computed from clock time: 25 us is 5 counts into the third period.
    assert_eq!(h.read32(BASE + COUNT), 5);
    assert_eq!(h.peek(BASE + COUNT, Width::Word), Some(5), "peek has no side effects");

    // The bus turns sub-word accesses into word accesses for this policy.
    assert_eq!(h.read8(BASE + LOAD), 10);
    h.write8(BASE + LOAD + 1, 0x01); // read-modify-write: LOAD = 0x10A
    assert_eq!(h.read32(BASE + LOAD), 0x10A);
    h.write32(BASE + LOAD, 10);

    // Acknowledge the interrupt: the line drops.
    h.write32(BASE + STATUS, 1);
    assert!(!h.irq_level(25));
    assert_eq!(h.irq_changes().last(), Some(&IrqChange { time: 25 * US, irq: 25, level: false }));

    // Chunk lag: the CPU runs a chunk from 25 us to 125 us. The 30 us limit has not been processed, so a
    // read of the undeclared STATUS register at 31 us still sees UIF clear; reading the declared COUNT
    // register first advances the clock to 31 us (the 30 us limit fires) and sees the new period.
    assert_eq!(h.cpu_read32(BASE + STATUS, 31 * US), 0, "clock time lags: the 30 us event has not fired yet");
    assert_eq!(h.cpu_read32(BASE + COUNT, 31 * US), 1, "synced to 31 us: 1 count into the period that began at 30 us");
    assert_eq!(h.cpu_read32(BASE + STATUS, 31 * US), 1, "the limit event ran during the sync");
    h.end_chunk(125 * US);

    // Disabling stops the entry: no event is pending, the value is kept.
    h.write32(BASE + CTRL, 0);
    assert_eq!(h.next_event_time(), None);
    let stopped = h.read32(BASE + COUNT);
    h.advance_to(200 * US);
    assert_eq!(h.read32(BASE + COUNT), stopped);

    // Unimplemented registers warn once and read 0.
    assert_eq!(h.read32(BASE + 0x20), 0);
    assert_eq!(h.read32(BASE + 0x20), 0);
    assert_eq!(h.warnings().len(), 1);
    assert!(h.core().summaries().iter().any(|(n, s)| n == "countdown" && s.contains("periods=")));
}
