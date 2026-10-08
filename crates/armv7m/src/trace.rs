//! Opt-in instruction trace. The run loop is instantiated twice (trace on /
//! off) with a const generic, so tracing costs nothing when disabled.
//! Idle-loop fast-forward is disarmed while a trace sink is installed so that
//! traces contain every executed instruction.

use crate::cpu::Cpu;
use crate::op::Op;

/// One traced instruction: the retire count before it executed, its address and
/// raw encoding (`hw1 << 16 | hw2`, or the single halfword for 16-bit instructions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceEntry {
    pub icount: u64,
    pub pc: u32,
    pub raw: u32,
    pub len: u8,
}

enum Sink {
    Off,
    /// Bounded buffer of full entries; `ring` keeps the most recent `cap` entries.
    Entries { cap: usize, ring: bool, head: usize, buf: Vec<TraceEntry> },
    /// Bounded buffer of program counters only (compact differential traces).
    Pcs { cap: usize, buf: Vec<u32> },
    Callback(Box<dyn FnMut(&TraceEntry)>),
}

pub(crate) struct Trace {
    sink: Sink,
    stash_entries: Vec<TraceEntry>,
    stash_pcs: Vec<u32>,
}

impl Trace {
    pub fn new() -> Self {
        Trace { sink: Sink::Off, stash_entries: Vec::new(), stash_pcs: Vec::new() }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        !matches!(self.sink, Sink::Off)
    }

    /// Disables tracing and moves buffered data to the stash (oldest entry first).
    fn finish(&mut self) {
        match core::mem::replace(&mut self.sink, Sink::Off) {
            Sink::Entries { ring, head, mut buf, .. } => {
                if ring && !buf.is_empty() {
                    let n = buf.len();
                    buf.rotate_left(head % n);
                }
                self.stash_entries = buf;
            }
            Sink::Pcs { buf, .. } => self.stash_pcs = buf,
            _ => {}
        }
    }
}

impl Cpu {
    /// Records the next `capacity` instructions (`ring == false`) or the latest
    /// `capacity` instructions (`ring == true`). Replaces any previous sink.
    pub fn trace_to_buffer(&mut self, capacity: usize, ring: bool) {
        self.trace.finish();
        self.trace.sink = Sink::Entries { cap: capacity.max(1), ring, head: 0, buf: Vec::with_capacity(capacity.min(1 << 20)) };
        self.ff.refresh(true);
    }

    /// Records the address of the next `capacity` instructions (4 bytes each).
    pub fn trace_pcs(&mut self, capacity: usize) {
        self.trace.finish();
        self.trace.sink = Sink::Pcs { cap: capacity, buf: Vec::with_capacity(capacity.min(1 << 24)) };
        self.ff.refresh(true);
    }

    /// Calls `cb` for every executed instruction.
    pub fn trace_to_callback(&mut self, cb: Box<dyn FnMut(&TraceEntry)>) {
        self.trace.finish();
        self.trace.sink = Sink::Callback(cb);
        self.ff.refresh(true);
    }

    /// Stops tracing; buffered data stays available through `trace_take` / `trace_take_pcs`.
    pub fn trace_stop(&mut self) {
        self.trace.finish();
        self.ff.refresh(false);
    }

    /// Takes the buffered instruction trace (oldest first), stopping tracing.
    pub fn trace_take(&mut self) -> Vec<TraceEntry> {
        self.trace_stop();
        core::mem::take(&mut self.trace.stash_entries)
    }

    /// Takes the buffered PC-only trace, stopping tracing.
    pub fn trace_take_pcs(&mut self) -> Vec<u32> {
        self.trace_stop();
        core::mem::take(&mut self.trace.stash_pcs)
    }

    #[inline]
    pub(crate) fn trace_record(&mut self, pc: u32, op: &Op) {
        let icount = self.icount;
        match &mut self.trace.sink {
            Sink::Off => {}
            Sink::Entries { cap, ring, head, buf } => {
                let e = TraceEntry { icount, pc, raw: op.raw, len: op.len };
                if buf.len() < *cap {
                    buf.push(e);
                } else if *ring {
                    buf[*head] = e;
                    *head = (*head + 1) % *cap;
                }
            }
            Sink::Pcs { cap, buf } => {
                if buf.len() < *cap {
                    buf.push(pc);
                }
            }
            Sink::Callback(cb) => {
                let e = TraceEntry { icount, pc, raw: op.raw, len: op.len };
                cb(&e);
            }
        }
    }
}
