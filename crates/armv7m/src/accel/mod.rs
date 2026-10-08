//! Exact acceleration of hot runtime-library routines (DESIGN.md section 16.2).
//!
//! # What it does
//!
//! The main board's firmware spends most of a dive in a few soft-float library routines (the double
//! divide, `expf` and the float/double conversions) that it calls with a small set of repeating operands.
//! Calls to these routines are **memoized**: the first time a key (the argument registers) is seen the call
//! is executed by the interpreter as usual; the second time it is executed *and recorded* (every
//! instruction is checked by the dependency tracker, `track.rs`); from then on a call with the same key is
//! replaced by applying the recorded effect: results, scratch registers, flags, IT state, FPSCR, stores to
//! the routine's frame, the retire count and the return.
//!
//! # Why it is exact (DESIGN.md 16.1)
//!
//! * **Only recorded paths are replayed**, and only with the key they were recorded with. The tracker
//!   proves, instruction by instruction, that every operand of the recorded path is a function of the key
//!   (or a verbatim copy of a register at entry: the callee-saved registers and the return address that
//!   the routine pushes and pops). So any other caller state executes the same instructions on the same
//!   values; the memo stores constants for the former and "copy of register R at entry" for the latter.
//!   Paths that read caller state in any other way, touch memory other than flash at key-determined
//!   addresses and the routine's own frame, use unsupported instructions, fault or take too long are
//!   **unsafe** and never memoized.
//! * **The interpreter has executed the path**, so the predecode cache, the VFP table, the page table and
//!   the cut-block history are exactly what interpretation leaves (the recording *is* the interpretation:
//!   it steps with `step_insn`, the same slow path IT blocks use). Replays change none of them.
//! * **A call is replaced only when it fits the chunk**: the remaining instruction budget of the chunk is
//!   at least the recorded count, no interrupt line, stop request, WFI, lockup or reset is pending, and the
//!   core is at a translation-block start with `ITSTATE == 0`. Nothing can interrupt the call, and a chunk
//!   boundary or block cut can not fall inside it; it ends at a translation-block boundary (the return).
//!   Calls that do not fit are interpreted normally.
//! * Identification is by code bytes (`routines.rs`), the memo is dropped when the code region changes.
//!
//! The switch is [`RoutineAccelMode`]: `Off` is plain interpretation, `On` the acceleration, `Shadow`
//! replays and interprets every hit and compares (used by the tests and `ngc-cli bench --verify-routine-accel`).
//! [`Cpu::exactness_digest`] fingerprints everything that must be identical between modes.
//!
//! # Hook
//!
//! The run loop calls into this module in two places, both marked `ROUTINE-ACCEL HOOK` in `cpu.rs`: once per
//! chunk (`accel_prepare`, finds the routines in the code region) and at every translation-block start in
//! the hot loop whose address is a routine entry (`accel_enter`).

mod digest;
mod memo;
mod record;
mod routines;
mod sha256;
mod track;

use crate::cpu::*;
use crate::op::*;
use crate::{vfp, CpuBus};
use memo::{hash_key, Key, Memo, MemoTable, Src};
use std::collections::BTreeMap;

pub use routines::{anchor_hash, scan, scan_specs, Match, Spec, SPECS};
pub use sha256::{hex as sha256_hex, sha256};

/// Number of probe slots the hot loop indexes (a power of two).
pub(crate) const PROBE_SIZE: usize = 256;
/// Marks an empty probe slot (a program counter is never odd).
const NO_ENTRY: u32 = u32::MAX;
/// Largest number of instructions a recorded call may take.
const MAX_RECORD: u64 = 2048;
const SEEN_SIZE: usize = 2048;
const CTRL_MASK: u32 = vfp::fpscr::AHP | vfp::fpscr::DN | vfp::fpscr::FZ | vfp::fpscr::RMODE_MASK;

#[inline(always)]
pub(crate) fn probe_index(pc: u32) -> usize {
    ((pc >> 1) ^ (pc >> 9)) as usize & (PROBE_SIZE - 1)
}

/// How routine acceleration runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RoutineAccelMode {
    /// Plain interpretation.
    Off,
    /// Memoized calls replace the interpreter (default).
    #[default]
    On,
    /// Every memo hit is replayed *and* interpreted and the two results are compared; the interpreted state
    /// is kept. Mismatches are counted in [`RoutineAccelStats::shadow_mismatches`].
    Shadow,
}

/// Counters of one routine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineStats {
    pub name: &'static str,
    pub entry: u32,
    /// Calls replaced by a memo entry.
    pub hits: u64,
    /// Instructions those calls would have retired.
    pub instructions_replaced: u64,
    /// Calls with a key never recorded (interpreted).
    pub misses: u64,
    /// Memo entries created.
    pub recorded: u64,
    /// Paths proven unsafe (never replayed).
    pub unsafe_paths: u64,
    /// Calls with a memo entry that did not fit the remaining chunk budget (interpreted).
    pub budget_skips: u64,
    /// Calls declined for another reason (stack frame not plain RAM, pending events, ...).
    pub declined: u64,
    pub memo_entries: u64,
    pub shadow_checks: u64,
}

/// Statistics of the acceleration of one core.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutineAccelStats {
    pub mode: RoutineAccelMode,
    pub routines: Vec<RoutineStats>,
    pub shadow_checks: u64,
    pub shadow_mismatches: u64,
    /// Description of the first shadow mismatch.
    pub first_mismatch: Option<String>,
    /// Why recorded paths were proven unsafe, with counts.
    pub unsafe_reasons: Vec<(&'static str, u64)>,
}

impl RoutineAccelStats {
    pub fn hits(&self) -> u64 {
        self.routines.iter().map(|r| r.hits).sum()
    }

    pub fn instructions_replaced(&self) -> u64 {
        self.routines.iter().map(|r| r.instructions_replaced).sum()
    }
}

/// Result of the hook's attempt at a routine entry.
pub(crate) enum Enter {
    /// Nothing was done; interpret normally.
    Declined,
    /// Instructions were executed (the state is that after the last of them); `ends` tells whether the
    /// last one ended its translation block.
    Done { ends: bool },
}

struct Routine {
    spec: &'static Spec,
    entry: u32,
    table: MemoTable,
    /// First sightings of keys (direct mapped); a key is recorded when it is seen a second time.
    seen: Vec<u64>,
    stats: RoutineStats,
}

pub(crate) struct State {
    routines: Vec<Routine>,
    shadow_checks: u64,
    shadow_mismatches: u64,
    first_mismatch: Option<String>,
    reasons: BTreeMap<&'static str, u64>,
}

pub(crate) struct Accel {
    pub(crate) mode: RoutineAccelMode,
    /// Entry addresses the hot loop compares the program counter with (hashed by [`probe_index`]).
    pub(crate) probe: [u32; PROBE_SIZE],
    /// The code region changed or was invalidated: rescan before the next use.
    pub(crate) stale: bool,
    region: Option<(u32, usize)>,
    /// The routine table the code region is scanned for ([`SPECS`]; tests install their own).
    specs: &'static [Spec],
    st: Option<Box<State>>,
}

impl Accel {
    pub(crate) fn new() -> Self {
        Accel {
            mode: RoutineAccelMode::On,
            probe: [NO_ENTRY; PROBE_SIZE],
            stale: true,
            region: None,
            specs: SPECS,
            st: Some(Box::new(State { routines: Vec::new(), shadow_checks: 0, shadow_mismatches: 0, first_mismatch: None, reasons: BTreeMap::new() })),
        }
    }
}

impl Cpu {
    /// Selects routine acceleration (see [`RoutineAccelMode`]). Results are identical in every mode.
    pub fn set_routine_accel(&mut self, mode: RoutineAccelMode) {
        self.accel.mode = mode;
        self.accel.stale = true;
        if mode == RoutineAccelMode::Off {
            self.accel.probe = [NO_ENTRY; PROBE_SIZE];
        }
    }

    pub fn routine_accel_mode(&self) -> RoutineAccelMode {
        self.accel.mode
    }

    /// Installs another routine table than [`SPECS`] (tests of the machinery use synthetic routines).
    #[doc(hidden)]
    pub fn set_routine_specs(&mut self, specs: &'static [Spec]) {
        self.accel.specs = specs;
        self.accel.stale = true;
    }

    /// Counters of the routine acceleration.
    pub fn routine_accel_stats(&self) -> RoutineAccelStats {
        let st = self.accel.st.as_ref().expect("acceleration state");
        let routines = st
            .routines
            .iter()
            .map(|r| {
                let mut s = r.stats.clone();
                s.memo_entries = r.table.memos() as u64;
                s
            })
            .collect();
        RoutineAccelStats {
            mode: self.accel.mode,
            routines,
            shadow_checks: st.shadow_checks,
            shadow_mismatches: st.shadow_mismatches,
            first_mismatch: st.first_mismatch.clone(),
            unsafe_reasons: st.reasons.iter().map(|(k, v)| (*k, *v)).collect(),
        }
    }

    /// The accelerated routines found in the code region (entry addresses), for reports.
    pub fn accelerated_routines(&self) -> Vec<(&'static str, u32)> {
        self.accel.st.as_ref().map_or_else(Vec::new, |st| st.routines.iter().map(|r| (r.spec.name, r.entry)).collect())
    }

    /// Drops the memo tables and rescans the code region at the next chunk (flash contents changed).
    pub(crate) fn accel_invalidate(&mut self) {
        self.accel.stale = true;
    }

    /// Once per chunk: makes sure the routines of the current code region are known.
    pub(crate) fn accel_prepare<B: CpuBus>(&mut self, bus: &B) {
        if self.accel.mode == RoutineAccelMode::Off {
            return;
        }
        let addr = if self.cache_span != 0 { self.cache_base } else { self.r[15] };
        let Some((base, bytes)) = bus.code_region(addr) else { return };
        let key = (base, bytes.len());
        if !self.accel.stale && self.accel.region == Some(key) {
            return;
        }
        let found = scan_specs(self.accel.specs, base, bytes);
        let accel = &mut self.accel;
        let st = accel.st.as_mut().expect("acceleration state");
        let old = std::mem::take(&mut st.routines);
        accel.probe = [NO_ENTRY; PROBE_SIZE];
        for m in found {
            let slot = probe_index(m.entry);
            if accel.probe[slot] != NO_ENTRY {
                continue; // probe collision: this routine stays interpreted
            }
            accel.probe[slot] = m.entry;
            let spec = &accel.specs[m.spec];
            let stats = old.iter().find(|r| r.entry == m.entry && r.spec.name == spec.name).map(|r| r.stats.clone()).unwrap_or_else(|| RoutineStats { name: spec.name, entry: m.entry, ..RoutineStats::default() });
            st.routines.push(Routine { spec, entry: m.entry, table: MemoTable::new(), seen: vec![0; SEEN_SIZE], stats });
        }
        accel.region = Some(key);
        accel.stale = false;
    }

    /// The hook at a translation-block start whose address is a routine entry (see the module documentation).
    #[cold]
    #[inline(never)]
    pub(crate) fn accel_enter<B: CpuBus>(&mut self, bus: &mut B, pc: u32) -> Enter {
        let Some(mut st) = self.accel.st.take() else { return Enter::Declined };
        let result = self.accel_enter_inner(bus, &mut st, pc);
        self.accel.st = Some(st);
        result
    }

    fn accel_enter_inner<B: CpuBus>(&mut self, bus: &mut B, st: &mut State, pc: u32) -> Enter {
        let mode = self.accel.mode;
        let Some(ri) = st.routines.iter().position(|r| r.entry == pc) else { return Enter::Declined };
        if mode == RoutineAccelMode::Off {
            return Enter::Declined;
        }
        let spec = st.routines[ri].spec;
        // ---- gates: nothing can interrupt the call and the return is an ordinary one ----------------------------
        let lr = self.r[14];
        let sp = self.r[13];
        let ret = lr & !1;
        if self.itstate != 0
            || !self.thumb
            || self.halted
            || self.lockup.is_some()
            || self.nvic.irq_line
            || self.exit_pending
            || self.wfi_exit
            || self.reset_requested
            || self.force_tb_end
            || lr & 1 == 0
            || ret.wrapping_sub(self.cache_base) >= self.cache_span
            || sp & 3 != 0
        {
            st.routines[ri].stats.declined += 1;
            return Enter::Declined;
        }
        if spec.fp {
            let cp = (self.scb.cpacr >> 20) & 3;
            if self.control & CONTROL_FPCA == 0 || !(cp == 3 || (cp == 1 && self.privileged())) || self.scb.fpccr_lspact() {
                st.routines[ri].stats.declined += 1;
                return Enter::Declined;
            }
        }
        // ---- the key ----------------------------------------------------------------------------------------------------
        let mut key: Key = [0; memo::KEY_WORDS];
        for i in 0..4 {
            if spec.core_key & (1 << i) != 0 {
                key[i] = self.r[i];
            }
        }
        for i in 0..2 {
            if spec.s_key & (1 << i) != 0 {
                key[4 + i] = self.fp.s[i];
            }
        }
        if spec.fp {
            key[6] = self.fp.fpscr & CTRL_MASK;
        }
        let hash = hash_key(&key);
        let avail = self.budget_end.saturating_sub(self.icount);
        let routine = &mut st.routines[ri];
        match routine.table.find(&key, hash) {
            Some(entry) => {
                let Some(memo) = entry.memo.as_ref() else { return Enter::Declined };
                if avail < u64::from(memo.count) {
                    routine.stats.budget_skips += 1;
                    return Enter::Declined;
                }
                if !self.frame_usable(bus, sp, memo.min_off) {
                    routine.stats.declined += 1;
                    return Enter::Declined;
                }
                if mode == RoutineAccelMode::Shadow {
                    let memo = memo.clone();
                    return self.shadow_call(bus, st, ri, &memo, ret);
                }
                self.replay(bus, memo, ret);
                routine.stats.hits += 1;
                routine.stats.instructions_replaced += u64::from(memo.count);
                Enter::Done { ends: true }
            }
            None => {
                routine.stats.misses += 1;
                let tag = hash | 1;
                let slot = (hash >> 7) as usize % SEEN_SIZE;
                if routine.seen[slot] != tag {
                    routine.seen[slot] = tag;
                    return Enter::Declined;
                }
                if avail < MAX_RECORD || routine.table.is_full() || !self.frame_usable(bus, sp, -track::MAX_FRAME) {
                    return Enter::Declined;
                }
                self.record_call(bus, st, ri, key, ret)
            }
        }
    }

    /// The frame `[sp + min_off, sp)` is plain RAM (not flash, not MMIO) so that its stores are plain writes.
    fn frame_usable<B: CpuBus>(&self, bus: &B, sp: u32, min_off: i32) -> bool {
        let low = sp.wrapping_add(min_off as u32);
        let in_code = |a: u32| a.wrapping_sub(self.cache_base) < self.cache_span;
        min_off <= 0 && low <= sp && !in_code(low) && !in_code(sp.wrapping_sub(4)) && bus.is_plain_memory(low) && bus.is_plain_memory(sp.wrapping_sub(4))
    }

    fn reg_by_id(&self, id: u8) -> u32 {
        if id < 16 {
            self.r[id as usize]
        } else {
            self.fp.s[(id - 16) as usize]
        }
    }

    /// Applies a memo entry: the exact effect of the call on the core and its frame.
    fn replay<B: CpuBus>(&mut self, bus: &mut B, memo: &Memo, ret: u32) {
        let sp = self.r[13];
        // Everything that reads entry values (stores of callee-saved registers, register copies) goes before any register
        // is overwritten.
        for store in &memo.stores {
            let value = match store.src {
                Src::Const(v) => v,
                Src::Copy(id) => self.reg_by_id(id),
            };
            bus.write32(sp.wrapping_add(store.off as u32), value, self.tb_icount);
        }
        let mut copied = [0u32; memo::MAX_COPIES];
        for (slot, &(_, src)) in copied.iter_mut().zip(memo.copies.iter()) {
            *slot = self.reg_by_id(src);
        }
        let mut mask = memo.core_mask;
        let mut k = 0;
        while mask != 0 {
            self.r[mask.trailing_zeros() as usize] = memo.core_vals[k];
            k += 1;
            mask &= mask - 1;
        }
        let mut mask = memo.s_mask;
        let mut k = 0;
        while mask != 0 {
            self.fp.s[mask.trailing_zeros() as usize] = memo.s_vals[k];
            k += 1;
            mask &= mask - 1;
        }
        for (&(dst, _), &value) in memo.copies.iter().zip(copied.iter()) {
            if dst < 16 {
                self.r[dst as usize] = value;
            } else {
                self.fp.s[(dst - 16) as usize] = value;
            }
        }
        if memo.nzcv_mask != 0 {
            let mask = u32::from(memo.nzcv_mask) << 28;
            self.apsr = (self.apsr & !mask) | ((u32::from(memo.nzcv) << 28) & mask);
        }
        if memo.uses_fp {
            let mut fpscr = self.fp.fpscr;
            if let Some(nzcv) = memo.fpscr_nzcv {
                fpscr = (fpscr & 0x0FFF_FFFF) | nzcv;
            }
            self.fp.fpscr = fpscr | memo.fpscr_cum;
        }
        self.icount += u64::from(memo.count);
        self.tb_icount = self.icount;
        self.r[15] = ret;
    }
}
