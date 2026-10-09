//! Exact idle-loop fast-forward (DESIGN.md section 6).
//!
//! A short backward branch that closes a loop whose body is pure (ALU, compare,
//! move, loads from plain memory, branches, IT) is a candidate. At the loop head
//! the full register state is snapshotted and the loop is executed normally for
//! one iteration while every load address is checked to be plain memory. When the
//! next arrival at the head reproduces the identical snapshot, the loop is a fixed
//! point: no instruction in it can change any state, and no other agent can change
//! plain memory or raise an event before the end of the current run budget, so
//! whole iterations are skipped by advancing the retire count (and with it
//! virtual time). The result is bit-identical to executing the instructions.

use crate::cpu::{Cpu, FastForwardStats};
use crate::op::*;
use crate::CpuBus;

/// Maximum loop body length (instructions) and distance (bytes) considered.
const MAX_BODY: u32 = 32;
/// Verification iterations tried before giving up on a loop entry.
const MAX_TRIES: u8 = 4;
/// Failed entries after which a loop is never considered again.
const MAX_FAILS: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Snap {
    r: [u32; 15],
    apsr: u32,
    ge: u32,
}

#[derive(Clone, Copy)]
struct Verify {
    head: u32,
    tail: u32,
    snap: Snap,
    start_icount: u64,
    tries: u8,
}

#[derive(Clone, Copy, Default)]
struct Entry {
    tail: u32,
    fails: u8,
    valid: bool,
    /// The loop ending at `tail` passed the static classification (`ff_classify`): it is made of pure
    /// instructions only. The loop head is the target of the branch at `tail`, so the result never changes
    /// until the code cache is flushed (which resets this table).
    classified: bool,
}

pub(crate) struct FastFwd {
    pub user_enabled: bool,
    /// `user_enabled` and tracing is off: the hook in branch handlers is armed.
    pub active: bool,
    pub stats: FastForwardStats,
    pending: Option<(u32, u32)>,
    verify: Option<Verify>,
    table: [Entry; 64],
}

impl FastFwd {
    pub fn new() -> Self {
        FastFwd { user_enabled: true, active: true, stats: FastForwardStats::default(), pending: None, verify: None, table: [Entry::default(); 64] }
    }

    pub fn reset(&mut self) {
        self.pending = None;
        self.verify = None;
        self.table = [Entry::default(); 64];
    }

    pub fn refresh(&mut self, tracing: bool) {
        self.active = self.user_enabled && !tracing;
        if !self.active {
            self.pending = None;
            self.verify = None;
        }
    }

    #[inline]
    pub fn verifying(&self) -> bool {
        self.verify.is_some()
    }

    #[inline]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// Kinds whose execution is a pure function of the registers, flags and plain memory.
fn pure_kind(k: Kind) -> bool {
    use Kind::*;
    matches!(
        k,
        Nop | MovImm
            | MvnImm
            | Movw
            | Movt
            | AndImm
            | BicImm
            | OrrImm
            | OrnImm
            | EorImm
            | TstImm
            | TeqImm
            | AddImm
            | AdcImm
            | SubImm
            | SbcImm
            | RsbImm
            | CmpImm
            | CmnImm
            | MovReg
            | MvnReg
            | AndReg
            | BicReg
            | OrrReg
            | OrnReg
            | EorReg
            | TstReg
            | TeqReg
            | AddReg
            | AdcReg
            | SubReg
            | SbcReg
            | RsbReg
            | CmpReg
            | CmnReg
            | LslImm
            | LsrImm
            | AsrImm
            | RorImm
            | Rrx
            | LslReg
            | LsrReg
            | AsrReg
            | RorReg
            | Mul
            | Mla
            | Mls
            | Umull
            | Smull
            | Umlal
            | Smlal
            | Umaal
            | Sdiv
            | Udiv
            | Sxtb
            | Sxth
            | Uxtb
            | Uxth
            | Sxtab
            | Sxtah
            | Uxtab
            | Uxtah
            | Sxtb16
            | Uxtb16
            | Sxtab16
            | Uxtab16
            | Rev
            | Rev16
            | Revsh
            | Rbit
            | Clz
            | Bfi
            | Bfc
            | Ubfx
            | Sbfx
            | Ssat
            | Usat
            | Ssat16
            | Usat16
            | Qadd
            | Qsub
            | Qdadd
            | Qdsub
            | Pkhbt
            | Pkhtb
            | Parallel
            | Usad8
            | Usada8
            | Sel
            | Smulxy
            | Smlaxy
            | Smulwy
            | Smlawy
            | Smlalxy
            | Smuad
            | Smusd
            | Smlad
            | Smlsd
            | Smlald
            | Smlsld
            | Smmul
            | Smmla
            | Smmls
            | LdrImm
            | LdrbImm
            | LdrhImm
            | LdrsbImm
            | LdrshImm
            | LdrReg
            | LdrbReg
            | LdrhReg
            | LdrsbReg
            | LdrshReg
            | LdrLit
            | LdrbLit
            | LdrhLit
            | LdrsbLit
            | LdrshLit
            | B
            | Bcc
            | Cbz
            | Cbnz
            | It
    )
}

impl Cpu {
    /// Called by branch handlers for a taken short backward branch while armed.
    #[cold]
    #[inline(never)]
    pub(crate) fn ff_backward_branch(&mut self, tail: u32, head: u32) {
        if self.ff.verify.is_some() || self.ff.pending.is_some() || self.itstate != 0 {
            return;
        }
        let idx = ((tail >> 1) & 63) as usize;
        let e = self.ff.table[idx];
        if e.valid && e.tail == tail && e.fails >= MAX_FAILS {
            return;
        }
        self.ff.pending = Some((tail, head));
        self.kick();
    }

    /// Marks the branch instruction at `tail` so that it no longer calls the hook.
    fn ff_reject(&mut self, tail: u32) {
        let off = tail.wrapping_sub(self.cache_base);
        if off < self.cache_span {
            let idx = (off >> 1) as usize;
            self.cache[idx].flags |= FL_NOFF;
        }
    }

    /// Static classification of the loop `head..=tail`; returns the instruction count.
    fn ff_classify<B: CpuBus>(&mut self, bus: &mut B, head: u32, tail: u32) -> Option<u32> {
        let mut a = head;
        let mut n = 0u32;
        let mut hit_tail = false;
        while a <= tail {
            let op = self.fetch_op(bus, a);
            if !pure_kind(op.kind) {
                return None;
            }
            n += 1;
            if n > MAX_BODY {
                return None;
            }
            if a == tail {
                hit_tail = true;
            }
            a = a.wrapping_add(op.len as u32);
        }
        if hit_tail {
            Some(n)
        } else {
            None
        }
    }

    fn ff_snapshot(&self) -> Snap {
        let mut r = [0u32; 15];
        r.copy_from_slice(&self.r[..15]);
        Snap { r, apsr: self.apsr, ge: self.ge }
    }

    /// Starts verification of a pending loop candidate (outer loop, bus available).
    pub(crate) fn ff_begin<B: CpuBus>(&mut self, bus: &mut B) {
        let (tail, head) = match self.ff.pending.take() {
            Some(p) => p,
            None => return,
        };
        if self.r[15] != head || self.itstate != 0 || !self.ff.active {
            return;
        }
        let idx = ((tail >> 1) & 63) as usize;
        if !(self.ff.table[idx].valid && self.ff.table[idx].tail == tail) {
            self.ff.table[idx] = Entry { tail, fails: 0, valid: true, classified: false };
        }
        // The static classification only reads the (immutable) predecoded loop body: once a loop passed it,
        // later entries skip it.
        if !self.ff.table[idx].classified {
            match self.ff_classify(bus, head, tail) {
                None => {
                    self.ff.table[idx].fails = MAX_FAILS;
                    self.ff_reject(tail);
                    return;
                }
                Some(_n) => self.ff.table[idx].classified = true,
            }
        }
        // Need room for at least a couple of iterations to be worth verifying.
        let lim = self.budget_end;
        if lim.saturating_sub(self.icount) < 4 * MAX_BODY as u64 {
            return;
        }
        self.ff.verify = Some(Verify { head, tail, snap: self.ff_snapshot(), start_icount: self.icount, tries: 0 });
    }

    fn ff_abort(&mut self, penalize: bool) {
        if let Some(v) = self.ff.verify.take() {
            if penalize {
                let idx = ((v.tail >> 1) & 63) as usize;
                let e = &mut self.ff.table[idx];
                if e.valid && e.tail == v.tail {
                    e.fails = e.fails.saturating_add(1);
                    if e.fails >= MAX_FAILS {
                        let tail = v.tail;
                        self.ff_reject(tail);
                    }
                }
                self.ff.stats.failed_verifications += 1;
            }
        }
    }

    /// Effective address of a verifiable load.
    fn ff_load_addr(&self, op: &Op) -> Option<u32> {
        let rn = (op.rn & 15) as usize;
        let rm = (op.rm & 15) as usize;
        Some(match op.kind {
            Kind::LdrImm | Kind::LdrbImm | Kind::LdrhImm | Kind::LdrsbImm | Kind::LdrshImm => {
                let base = self.r[rn];
                if op.flags & FL_IDX != 0 {
                    base.wrapping_add(op.imm)
                } else {
                    base
                }
            }
            Kind::LdrReg | Kind::LdrbReg | Kind::LdrhReg | Kind::LdrsbReg | Kind::LdrshReg => self.r[rn].wrapping_add(self.r[rm] << op.x),
            Kind::LdrLit | Kind::LdrbLit | Kind::LdrhLit | Kind::LdrsbLit | Kind::LdrshLit => op.imm,
            _ => return None,
        })
    }

    /// One verification step: executes one instruction of the candidate loop with
    /// every load address checked, and detects the fixed point at the loop head.
    /// `at_boundary` is whether the core is at a translation-block boundary on entry; the
    /// return value is the same property after the step.
    pub(crate) fn ff_verify_step<B: CpuBus>(&mut self, bus: &mut B, at_boundary: bool) -> bool {
        let (head, tail) = match &self.ff.verify {
            Some(v) => (v.head, v.tail),
            None => return at_boundary,
        };
        let pc = self.r[15];
        let lim = self.budget_end;
        if pc < head || pc > tail || self.icount >= lim || self.itstate != 0 && self.icount + 8 >= lim {
            self.ff_abort(false);
            return at_boundary;
        }
        let op = self.fetch_op(bus, pc);
        if let Some(addr) = self.ff_load_addr(&op) {
            if addr >> 20 == 0xE00 || !bus.is_plain_memory(addr) {
                self.ff_abort(true);
                return at_boundary;
            }
        }
        // (`step_insn`, with the instruction fetched above instead of fetching it a second time.)
        let ends = self.step_fetched(bus, pc, op);
        if self.nvic.irq_line || self.exit_pending || self.wfi_exit || self.insn_faulted || self.reset_requested || self.lockup.is_some() || self.halted {
            self.ff_abort(false);
            return ends;
        }
        let npc = self.r[15];
        if npc == head && self.itstate == 0 {
            let cur = self.ff_snapshot();
            let (same, len) = match &self.ff.verify {
                Some(v) => (cur == v.snap, self.icount - v.start_icount),
                None => return ends,
            };
            if same && len > 0 {
                // Fixed point: skip whole iterations up to the end of the chunk. No event can
                // happen before it (pure loop, plain memory only), so every translation-block
                // boundary inside the skipped iterations would be a no-op.
                let remaining = lim.saturating_sub(self.icount);
                let k = remaining / len;
                if k > 0 {
                    self.icount += k * len;
                    self.ff.stats.skipped_instructions += k * len;
                }
                self.ff.stats.loops += 1;
                self.ff.verify = None;
                let idx = ((tail >> 1) & 63) as usize;
                if self.ff.table[idx].valid && self.ff.table[idx].tail == tail {
                    self.ff.table[idx].fails = 0;
                }
                return ends;
            }
            let icount = self.icount;
            if let Some(v) = self.ff.verify.as_mut() {
                v.tries += 1;
                if v.tries >= MAX_TRIES {
                    self.ff_abort(true);
                    return ends;
                }
                v.snap = cur;
                v.start_icount = icount;
            }
        } else if npc < head || npc > tail {
            self.ff_abort(false);
        }
        ends
    }

    /// Verification steps back to back. Between two steps the outer loop only repeats its own checks, which are all
    /// no-ops while no exception, stop request, WFI exit, reset request or lockup is pending and the chunk budget is
    /// not used up (`boundary()` and `event_pending()` then do nothing); this loop makes the same test and returns to
    /// the outer loop as soon as any of them holds, or when the verification has ended.
    pub(crate) fn ff_verify_run<B: CpuBus>(&mut self, bus: &mut B, mut at_boundary: bool) -> bool {
        loop {
            at_boundary = self.ff_verify_step(bus, at_boundary);
            if self.ff.verify.is_none() {
                return at_boundary;
            }
            if self.nvic.irq_line
                || self.exit_pending
                || self.wfi_exit
                || self.reset_requested
                || self.lockup.is_some()
                || self.icount >= self.budget_end
            {
                return at_boundary;
            }
            if at_boundary {
                // The outer loop starts the next translation block here.
                self.tb_icount = self.icount;
            }
        }
    }
}
