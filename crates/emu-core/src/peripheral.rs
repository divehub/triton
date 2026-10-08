//! The `Peripheral` trait and the ids used to wire peripherals together.
//!
//! See `docs/framework.md` for the authoring guide.

use crate::access::{AccessPolicy, Width};
use crate::machine::{Ctx, View};
use crate::Time;
use std::any::Any;
use std::fmt;

/// Index of a peripheral inside a `MachineCore` (assigned by `add_peripheral`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeriphId(pub u32);

impl PeriphId {
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for PeriphId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Destination of a peripheral output line (the `->` in a Renode `.repl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// External interrupt input `n` of the CPU's NVIC (`-> nvic@n`).
    Irq(u32),
    /// Numbered GPIO input of another peripheral (`-> periph@line`), delivered to its `on_input`.
    Input(PeriphId, u32),
}

/// A register of a peripheral that the CPU accesses after Renode's `cpu.SyncTime()`: before such an
/// access the machine advances its clock to the exact instruction time (firing the events on the
/// way). Returned by [`Peripheral::sync_registers`]; `offset` is relative to the region base and the
/// register is 4 bytes wide (an access of any width that overlaps it matches).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncRegister {
    pub offset: u32,
    pub on_read: bool,
    pub on_write: bool,
}

impl SyncRegister {
    /// Sync before CPU reads of the register (the timer `CNT` read callback).
    pub const fn read(offset: u32) -> Self {
        Self { offset, on_read: true, on_write: false }
    }

    /// Sync before CPU writes of the register.
    pub const fn write(offset: u32) -> Self {
        Self { offset, on_read: false, on_write: true }
    }

    pub const fn read_write(offset: u32) -> Self {
        Self { offset, on_read: true, on_write: true }
    }

    /// True if an access of `width` at `offset` overlaps this register and is of the declared direction.
    #[inline]
    pub fn matches(&self, offset: u32, width: Width, write: bool) -> bool {
        (if write { self.on_write } else { self.on_read })
            && offset < self.offset.wrapping_add(4)
            && offset.wrapping_add(width.bytes()) > self.offset
    }
}

/// A memory-mapped model. All methods run with the peripheral temporarily taken out of the
/// machine, so `ctx` can reach everything else (other peripherals through the bus, memory,
/// the event queue and the signal nets) without aliasing `self`.
///
/// Time: `ctx.now()` is the machine's **clock-source time**: inside a CPU chunk it lags the
/// instruction that accesses the peripheral (chunk start or last sync, see `docs/framework.md`
/// section 2.2), inside an event it is the event's own time. Never keep wall-clock or global state.
pub trait Peripheral: 'static {
    /// Instance name used in logs and for lookups (`gpioA`, `timer3`, ...).
    fn name(&self) -> &str;

    /// Called once by `MachineCore::add_peripheral`, in registration order, before the peripheral is
    /// mapped or connected. Create clock entries here (`LimitTimer::attach`, `ManagedThread::attach`,
    /// `ctx.clock_add`): entry creation order is `attach` order and decides the order of handlers that
    /// expire in the same nanosecond.
    fn attach(&mut self, _ctx: &mut Ctx<'_>) {}

    /// Return to the power-on state. Called by `MachineCore::reset_all`; models also call it
    /// internally when their reset input fires. Outputs and events go through `ctx`.
    fn reset(&mut self, _ctx: &mut Ctx<'_>) {}

    /// Register read. `offset` is relative to the region base and is passed unmodified
    /// (possibly unaligned); `width` is the access width. Only the low `width` bits of the
    /// result are used. Which widths arrive here is governed by [`Peripheral::access_policy`].
    fn read(&mut self, offset: u32, width: Width, ctx: &mut Ctx<'_>) -> u32;

    /// Register write; `value` holds the data in its low `width` bits.
    fn write(&mut self, offset: u32, width: Width, value: u32, ctx: &mut Ctx<'_>);

    /// An event of this peripheral fires: an ordinary event (`ctx.schedule_at/in`), the limit of one of
    /// its clock entries (`LimitTimer`, `ManagedThread`, `ctx.clock_add`), or a `ctx.schedule_action`.
    /// `ctx.now()` is the event's own time, except for a delivery that had to wait for this peripheral
    /// to stop running (then it is the later clock time). `scheduled` is the event time (ordinary and
    /// clock events) or the **scheduling time** of an action (what Renode passes to the callback).
    fn on_event(&mut self, _token: u64, _scheduled: Time, _ctx: &mut Ctx<'_>) {}

    /// A connected output line changed level (`GPIO.OnGPIO`). Delivered once per change, plus
    /// once with the current level when the connection is made.
    fn on_input(&mut self, _line: u32, _level: bool, _ctx: &mut Ctx<'_>) {}

    /// How sub-word accesses are served. Defaults to "everything reaches `read`/`write`".
    /// Ports of Renode classes that implement only some `I*WordPeripheral` interfaces should
    /// return the matching policy (see `access.rs`). Queried once, when the peripheral is mapped.
    fn access_policy(&self) -> AccessPolicy {
        AccessPolicy::EXACT
    }

    /// Registers that Renode reads or writes after `cpu.SyncTime()` (the STM32 timer `CNT` read, ...).
    /// For a CPU access that overlaps one of them the machine first advances clock time to the exact
    /// instruction time (firing due events), so `ctx.now()` inside `read`/`write` is exact. Queried once,
    /// when the peripheral is mapped. For syncs that depend on state use `ctx.sync_time()` instead.
    fn sync_registers(&self) -> Vec<SyncRegister> {
        Vec::new()
    }

    /// Side-effect-free register read for debuggers and state snapshots. `None` when the
    /// register has no side-effect-free view (the default). Time-dependent registers (timer
    /// counters) are computed for `view.now()` (the machine's clock time); `view` implements
    /// `ClockRead`, so `timer.value(view)` works.
    fn peek(&self, _offset: u32, _width: Width, _view: &View<'_>) -> Option<u32> {
        None
    }

    /// Side-effect-free state patch for fixtures and snapshot restore; `false` if unsupported.
    fn poke(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) -> bool {
        false
    }

    /// One-line human readable state (counterpart of the Renode `Summary` property).
    fn summary(&self, _view: &View<'_>) -> String {
        String::new()
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Implements `as_any`/`as_any_mut` inside an `impl Peripheral for T` block.
#[macro_export]
macro_rules! impl_peripheral_any {
    () => {
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
    };
}
