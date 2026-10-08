//! What the timer model needs from its surroundings.
//!
//! The model (`model.rs`, `logic.rs`, `regs.rs`) is a pure state machine: it never touches a `Ctx`
//! directly. Everything that leaves the model goes through [`Io`], which has three implementations:
//!
//! * [`CtxIo`]: the real machine (output lines, return requests, logging, CPU time sync);
//! * [`SimIo`]: the planner's recording sink. The planner runs the model forward on a clone of its state
//!   and only asks "does anything the rest of the machine can see change?" (an interrupt line level or an
//!   observed pin level), which decides whether a real event has to be scheduled;
//! * [`NullIo`]: replays for side-effect-free reads (`Peripheral::peek`, `summary`).

use emu_core::{Ctx, LogLevel, Time};
use std::fmt;

pub(crate) trait Io {
    /// The machine clock time of the access in progress, or the event time inside a replay.
    fn now(&self) -> Time;
    /// Renode `cpu.SyncTime()`: advances the machine clock to the exact CPU time of the access in progress
    /// (a no-op outside CPU accesses) and returns the new clock time.
    fn sync_time(&mut self) -> Time;
    /// Renode `RequestReturn()` (every `LimitTimer` setter except `Mode`).
    fn request_return(&mut self);
    /// Drives an output line (the model only calls this on level changes).
    fn set_output(&mut self, line: u32, level: bool);
    /// A limit handler just did something the rest of the machine may look at: it re-evaluated the interrupt
    /// outputs (update/compare interrupt enabled) or wrote an observed pin. The planner treats such an
    /// instant as a real event whether or not a level changed, like the stock model, whose events exist
    /// independently of the current line levels: the guest may clear a flag right before the next one.
    fn mark_event(&mut self) {}
    fn log(&mut self, level: LogLevel, args: fmt::Arguments<'_>);
    /// Logs at most once per `key`.
    fn log_once(&mut self, level: LogLevel, key: u64, args: fmt::Arguments<'_>);
}

/// [`Io`] on a running peripheral's `Ctx`.
pub(crate) struct CtxIo<'a, 'b> {
    pub ctx: &'a mut Ctx<'b>,
}

impl Io for CtxIo<'_, '_> {
    #[inline]
    fn now(&self) -> Time {
        self.ctx.now()
    }

    #[inline]
    fn sync_time(&mut self) -> Time {
        self.ctx.sync_time()
    }

    #[inline]
    fn request_return(&mut self) {
        self.ctx.request_return();
    }

    #[inline]
    fn set_output(&mut self, line: u32, level: bool) {
        self.ctx.set_output(line, level);
    }

    fn log(&mut self, level: LogLevel, args: fmt::Arguments<'_>) {
        self.ctx.logf(level, args);
    }

    fn log_once(&mut self, level: LogLevel, key: u64, args: fmt::Arguments<'_>) {
        self.ctx.log_once(level, key, args);
    }
}

/// The planner's sink: remembers whether any externally visible output changed.
pub(crate) struct SimIo {
    pub t: Time,
    pub visible: bool,
}

impl Io for SimIo {
    fn now(&self) -> Time {
        self.t
    }

    fn sync_time(&mut self) -> Time {
        self.t
    }

    fn request_return(&mut self) {}

    fn set_output(&mut self, _line: u32, _level: bool) {
        self.visible = true;
    }

    fn mark_event(&mut self) {
        self.visible = true;
    }

    fn log(&mut self, _level: LogLevel, _args: fmt::Arguments<'_>) {}

    fn log_once(&mut self, _level: LogLevel, _key: u64, _args: fmt::Arguments<'_>) {}
}

/// Replays without any effect on the outside world.
pub(crate) struct NullIo {
    pub t: Time,
}

impl Io for NullIo {
    fn now(&self) -> Time {
        self.t
    }

    fn sync_time(&mut self) -> Time {
        self.t
    }

    fn request_return(&mut self) {}

    fn set_output(&mut self, _line: u32, _level: bool) {}

    fn log(&mut self, _level: LogLevel, _args: fmt::Arguments<'_>) {}

    fn log_once(&mut self, _level: LogLevel, _key: u64, _args: fmt::Arguments<'_>) {}
}
