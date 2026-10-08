//! Recording a call (interpretation plus dependency tracking), building the memo entry, and the shadow
//! check that replays and interprets a call and compares the two results.

use super::memo::{Key, Memo, RegWrite, Src, StoreWrite};
use super::track::{PathState, Pre, Tracker, MAX_FRAME};
use super::*;
use crate::decode;

pub(super) enum CallEnd {
    /// The call returned to its caller.
    Returned,
    /// Stopped before the return (unsafe path, too long, an event); the core is in the exact state interpretation
    /// would have reached. `ends`: the last instruction ended its translation block.
    Bailed { ends: bool },
}

/// The core as it was when the call started.
struct EntryState {
    r: [u32; 16],
    s: [u32; 32],
    fpscr: u32,
    apsr: u32,
    ge: u32,
    control: u32,
    ipsr: u32,
    sp0: u32,
    ret: u32,
    icount: u64,
}

impl EntryState {
    fn value(&self, id: u8) -> u32 {
        if id < 16 {
            self.r[id as usize]
        } else {
            self.s[(id - 16) as usize]
        }
    }
}

impl Cpu {
    /// The decoded instruction at `pc` without any side effect on the predecode cache.
    fn peek_op(&self, pc: u32) -> Option<(Op, Option<vfp::VfpInsn>)> {
        let off = pc.wrapping_sub(self.cache_base);
        if off >= self.cache_span {
            return None;
        }
        let mut op = self.cache[(off >> 1) as usize];
        op = match op.kind {
            Kind::Undecoded => return None,
            Kind::PageEnd => self.page_ops[op.imm as usize],
            Kind::CutHead => self.cut_entries[op.raw as usize].inner,
            _ => op,
        };
        let insn = if matches!(op.kind, Kind::Vfp | Kind::VfpEnd) && op.imm != u32::MAX { self.vfp_table.get(op.imm as usize).copied() } else { None };
        Some((op, insn))
    }

    /// True when the instruction at `pc` reads or writes the whole FPSCR (`VMSR`, `VMRS Rt`): the recording clears the
    /// cumulative flags while it runs, so such an instruction must not execute under the recording. Decoded from the code
    /// region without touching the predecode cache.
    fn accesses_whole_fpscr<B: CpuBus>(&self, bus: &B, pc: u32) -> bool {
        let Some((base, bytes)) = bus.code_region(pc) else { return true };
        let off = pc.wrapping_sub(base) as usize;
        if off + 3 >= bytes.len() {
            return true;
        }
        let hw1 = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        if !decode::is_32bit(hw1) {
            return false;
        }
        let hw2 = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]);
        match vfp::decode(hw1, hw2) {
            vfp::VfpDecode::Insn(insn) => {
                let u = insn.usage();
                u.writes_fpscr_all || (u.reads_fpscr_all && !u.writes_apsr_nzcv)
            }
            _ => false,
        }
    }

    /// Interprets the call that starts at the current instruction (a translation-block start) with the slow
    /// path, block by block like the run loop, until it returns to `ret` with SP back at `sp0`. With a tracker
    /// every instruction is accounted; without one the return is recognised by value.
    ///
    /// `held_flags`: the FPSCR cumulative flags the caller took out of the FPSCR for the duration of the recording
    /// (`Some` only for floating-point routines). They are put back before an instruction that accesses the whole FPSCR
    /// would run, and the recording stops there (that path is not memoized).
    pub(super) fn interpret_call<B: CpuBus>(&mut self, bus: &mut B, mut tracker: Option<&mut Tracker>, ret: u32, sp0: u32, max_steps: u64, held_flags: Option<&mut u32>) -> CallEnd {
        let mut ends = true;
        let mut steps = 0u64;
        let mut held = held_flags;
        loop {
            if steps >= max_steps || self.icount >= self.budget_end {
                return CallEnd::Bailed { ends };
            }
            if self.nvic.irq_line || self.exit_pending || self.wfi_exit || self.reset_requested || self.lockup.is_some() || self.halted {
                return CallEnd::Bailed { ends };
            }
            let pc = self.r[15];
            if let Some(flags) = held.as_deref_mut() {
                if self.accesses_whole_fpscr(bus, pc) {
                    self.fp.fpscr |= *flags;
                    *flags = 0;
                    if let Some(tr) = tracker.as_deref_mut() {
                        tr.mark_unsafe("accesses the whole FPSCR");
                    }
                    return CallEnd::Bailed { ends };
                }
            }
            let pre = tracker.as_ref().map(|_| Pre { r: self.r, s: self.fp.s, apsr: self.apsr, itstate: self.itstate });
            ends = self.step_insn::<B, false>(bus);
            steps += 1;
            if ends {
                // A translation block ended: the next one starts here (the run loop does the same).
                self.tb_icount = self.icount;
            }
            if self.insn_faulted {
                return CallEnd::Bailed { ends };
            }
            match (tracker.as_deref_mut(), pre) {
                (Some(tr), Some(pre)) => {
                    match self.peek_op(pc) {
                        Some((op, insn)) => tr.step(&pre, self.r[15], self.r[13], &op, insn.as_ref()),
                        None => tr.mark_unsafe("instruction not in the predecode cache"),
                    }
                    match tr.state {
                        PathState::Returned => return CallEnd::Returned,
                        PathState::Unsafe(_) => return CallEnd::Bailed { ends },
                        PathState::Running => {}
                    }
                }
                _ => {
                    if ends && self.r[15] == ret && self.r[13] == sp0 && self.itstate == 0 {
                        return CallEnd::Returned;
                    }
                }
            }
        }
    }

    /// Restores the loop bound after a nested interpretation (an event that needs the outer loop keeps it at zero).
    fn restore_limit(&mut self, saved: u64) {
        let events = self.nvic.irq_line || self.exit_pending || self.wfi_exit || self.reset_requested || self.lockup.is_some() || self.halted;
        self.limit = if events { 0 } else { saved };
    }

    /// Executes and records the call at the current instruction. The core ends in exactly the state interpretation
    /// leaves; on success the memo entry for `key` is stored (otherwise a tombstone, so the path is not retried).
    pub(super) fn record_call<B: CpuBus>(&mut self, bus: &mut B, st: &mut State, ri: usize, key: Key, ret: u32) -> Enter {
        let spec = st.routines[ri].spec;
        let sp0 = self.r[13];
        let entry = EntryState { r: self.r, s: self.fp.s, fpscr: self.fp.fpscr, apsr: self.apsr, ge: self.ge, control: self.control, ipsr: self.ipsr, sp0, ret, icount: self.icount };
        let words = (MAX_FRAME / 4) as u32;
        let window_base = sp0.wrapping_sub(MAX_FRAME as u32);
        let before: Vec<u32> = (0..words).map(|i| bus.read32(window_base.wrapping_add(4 * i), self.tb_icount)).collect();
        let code = (self.cache_base, self.cache_base.wrapping_add(self.cache_span));
        let mut tracker = Tracker::new(spec.core_key, spec.s_key, spec.fp, code, ret, sp0);

        // The idle fast-forward only observes loops; the recording must not feed it (host-speed state).
        let ff_active = std::mem::replace(&mut self.ff.active, false);
        let saved_limit = self.limit;
        // Cumulative FPSCR flags only accumulate: take them out so that the flags the call raises can be read off, and put
        // them back afterwards (before any instruction that would look at the whole FPSCR, see `interpret_call`).
        let mut held = if spec.fp { self.fp.fpscr & vfp::fpscr::FLAGS_MASK } else { 0 };
        if spec.fp {
            self.fp.fpscr &= !vfp::fpscr::FLAGS_MASK;
        }
        let end = self.interpret_call(bus, Some(&mut tracker), ret, sp0, MAX_RECORD, spec.fp.then_some(&mut held));
        let raised = if spec.fp { self.fp.fpscr & vfp::fpscr::FLAGS_MASK } else { 0 };
        if spec.fp {
            self.fp.fpscr |= held;
        }
        self.ff.active = ff_active;
        self.restore_limit(saved_limit);

        let routine = &mut st.routines[ri];
        let verdict: Result<Memo, &'static str> = match end {
            CallEnd::Returned => self.finalize(bus, &tracker, spec, &entry, &before, window_base, raised),
            CallEnd::Bailed { .. } => Err(tracker.unsafe_reason().unwrap_or("path too long or interrupted")),
        };
        match verdict {
            Ok(memo) => {
                routine.stats.recorded += 1;
                routine.table.insert(key, Some(memo));
            }
            Err(why) => {
                routine.stats.unsafe_paths += 1;
                routine.table.insert(key, None);
                *st.reasons.entry(why).or_insert(0) += 1;
            }
        }
        match end {
            CallEnd::Returned => Enter::Done { ends: true },
            CallEnd::Bailed { ends } => Enter::Done { ends },
        }
    }

    /// Builds the memo entry from a path the tracker proved to depend on the key alone, after checking that the final
    /// machine state agrees with the tracker's description of it.
    #[allow(clippy::too_many_arguments)]
    fn finalize<B: CpuBus>(&mut self, bus: &mut B, tr: &Tracker, spec: &Spec, e: &EntryState, before: &[u32], window_base: u32, raised: u32) -> Result<Memo, &'static str> {
        if self.itstate != 0 || self.ipsr != e.ipsr || self.control != e.control || self.ge != e.ge || self.r[13] != e.sp0 || self.r[15] != e.ret || !self.thumb {
            return Err("final state differs from the entry state");
        }
        if (self.apsr ^ e.apsr) & (1 << 27) != 0 {
            return Err("Q flag changed");
        }
        let mut regs = Vec::new();
        for id in 0..15usize {
            if id == 13 {
                continue;
            }
            let post = self.r[id];
            match tr.core[id].entry_id() {
                Some(k) => {
                    if post != e.value(k) {
                        return Err("tracker inconsistency: a copied register changed");
                    }
                    if k as usize != id {
                        regs.push(RegWrite { id: id as u8, src: Src::Copy(k) });
                    }
                }
                None => {
                    if spec.core_key & (1 << id) != 0 && post == e.r[id] {
                        continue; // a key register the call left alone
                    }
                    regs.push(RegWrite { id: id as u8, src: Src::Const(post) });
                }
            }
        }
        if tr.uses_fp {
            for i in 0..32usize {
                let post = self.fp.s[i];
                match tr.s[i].entry_id() {
                    Some(k) => {
                        if post != e.value(k) {
                            return Err("tracker inconsistency: a copied S register changed");
                        }
                        if k as usize != 16 + i {
                            regs.push(RegWrite { id: 16 + i as u8, src: Src::Copy(k) });
                        }
                    }
                    None => {
                        if spec.s_key & (1 << i) != 0 && post == e.s[i] {
                            continue;
                        }
                        regs.push(RegWrite { id: 16 + i as u8, src: Src::Const(post) });
                    }
                }
            }
        } else if self.fp.s != e.s {
            return Err("S registers changed in an integer routine");
        }
        // Flags the path did not produce must be untouched.
        let (post_flags, entry_flags) = ((self.apsr >> 28) as u8 & 0xF, (e.apsr >> 28) as u8 & 0xF);
        if (post_flags ^ entry_flags) & !tr.flags_const != 0 {
            return Err("tracker inconsistency: a flag changed without being produced");
        }
        // FPSCR.
        let post = self.fp.fpscr;
        let mut fpscr_nzcv = None;
        if tr.uses_fp {
            if (post ^ e.fpscr) & CTRL_MASK != 0 {
                return Err("FPSCR control fields changed");
            }
            if tr.fpscr_nzcv_written {
                fpscr_nzcv = Some(post & 0xF000_0000);
            } else if (post ^ e.fpscr) & 0xF000_0000 != 0 {
                return Err("tracker inconsistency: FPSCR flags changed");
            }
            if post & vfp::fpscr::FLAGS_MASK != (e.fpscr & vfp::fpscr::FLAGS_MASK) | raised {
                return Err("tracker inconsistency: FPSCR cumulative flags");
            }
        } else if post != e.fpscr {
            return Err("FPSCR changed in an integer routine");
        }
        // Stores, in program order, and the audit of the whole frame region.
        let mut stores = Vec::with_capacity(tr.stores.len());
        for w in &tr.stores {
            let src = match w.desc.entry_id() {
                Some(id) => {
                    if w.value != e.value(id) {
                        return Err("tracker inconsistency: a copied word differs from its source");
                    }
                    Src::Copy(id)
                }
                None => Src::Const(w.value),
            };
            stores.push(StoreWrite { off: w.off, src });
        }
        for (i, &was) in before.iter().enumerate() {
            let addr = window_base.wrapping_add(4 * i as u32);
            let off = addr.wrapping_sub(e.sp0) as i32;
            let expected = tr.frame.iter().find(|w| w.off == off).map_or(was, |w| w.value);
            if bus.read32(addr, self.tb_icount) != expected {
                return Err("memory changed outside the tracked stores");
            }
        }
        Memo::new((self.icount - e.icount) as u32, tr.uses_fp, &regs, tr.flags_const, post_flags & tr.flags_const, fpscr_nzcv, raised, stores, tr.min_off).ok_or("more register copies than a memo entry holds")
    }

    // ---- shadow verification ------------------------------------------------------------------------------------------

    fn shadow_snapshot<B: CpuBus>(&self, bus: &mut B, base: u32, len: u32) -> Snap {
        let window = (0..len / 4).map(|i| bus.read32(base.wrapping_add(4 * i), self.tb_icount)).collect();
        Snap {
            r: self.r,
            apsr: self.apsr,
            ge: self.ge,
            itstate: self.itstate,
            control: self.control,
            ipsr: self.ipsr,
            s: self.fp.s,
            fpscr: self.fp.fpscr,
            icount: self.icount,
            tb_icount: self.tb_icount,
            base,
            window,
        }
    }

    fn shadow_restore<B: CpuBus>(&mut self, bus: &mut B, snap: &Snap) {
        for (i, &word) in snap.window.iter().enumerate() {
            bus.write32(snap.base.wrapping_add(4 * i as u32), word, self.tb_icount);
        }
        self.r = snap.r;
        self.apsr = snap.apsr;
        self.ge = snap.ge;
        self.itstate = snap.itstate;
        self.control = snap.control;
        self.ipsr = snap.ipsr;
        self.fp.s = snap.s;
        self.fp.fpscr = snap.fpscr;
        self.icount = snap.icount;
        self.tb_icount = snap.tb_icount;
    }

    /// A memo hit in shadow mode: replays the entry, remembers the result, restores the state, interprets the call and
    /// compares. The interpreted state is kept.
    pub(super) fn shadow_call<B: CpuBus>(&mut self, bus: &mut B, st: &mut State, ri: usize, memo: &Memo, ret: u32) -> Enter {
        const BELOW: u32 = 256;
        const ABOVE: u32 = 64;
        let sp = self.r[13];
        let below = if self.frame_usable(bus, sp, -(BELOW as i32)) { BELOW } else { (-memo.min_off) as u32 };
        let above = if bus.is_plain_memory(sp.wrapping_add(ABOVE - 4)) { ABOVE } else { 0 };
        let base = sp.wrapping_sub(below);
        let len = below + above;
        let before = self.shadow_snapshot(bus, base, len);
        self.replay(bus, memo, ret);
        let predicted = self.shadow_snapshot(bus, base, len);
        self.shadow_restore(bus, &before);

        let ff_active = std::mem::replace(&mut self.ff.active, false);
        let saved_limit = self.limit;
        let end = self.interpret_call(bus, None, ret, sp, u64::from(memo.count) + 16, None);
        self.ff.active = ff_active;
        self.restore_limit(saved_limit);

        let name = st.routines[ri].spec.name;
        match end {
            CallEnd::Returned => {
                let actual = self.shadow_snapshot(bus, base, len);
                st.shadow_checks += 1;
                st.routines[ri].stats.shadow_checks += 1;
                if let Some(diff) = compare(&predicted, &actual) {
                    st.shadow_mismatches += 1;
                    st.first_mismatch.get_or_insert_with(|| format!("{name}: {diff}"));
                }
                Enter::Done { ends: true }
            }
            CallEnd::Bailed { ends } => {
                if self.icount - before.icount >= u64::from(memo.count) + 16 {
                    st.shadow_mismatches += 1;
                    st.first_mismatch.get_or_insert_with(|| format!("{name}: interpretation did not return within the recorded {} instructions", memo.count));
                }
                Enter::Done { ends }
            }
        }
    }
}

struct Snap {
    r: [u32; 16],
    apsr: u32,
    ge: u32,
    itstate: u8,
    control: u32,
    ipsr: u32,
    s: [u32; 32],
    fpscr: u32,
    icount: u64,
    tb_icount: u64,
    base: u32,
    window: Vec<u32>,
}

fn compare(predicted: &Snap, actual: &Snap) -> Option<String> {
    for i in 0..16 {
        if predicted.r[i] != actual.r[i] {
            return Some(format!("r{i} replayed {:#010x}, interpreted {:#010x}", predicted.r[i], actual.r[i]));
        }
    }
    for i in 0..32 {
        if predicted.s[i] != actual.s[i] {
            return Some(format!("s{i} replayed {:#010x}, interpreted {:#010x}", predicted.s[i], actual.s[i]));
        }
    }
    let fields = [
        ("apsr", predicted.apsr, actual.apsr),
        ("ge", predicted.ge, actual.ge),
        ("itstate", u32::from(predicted.itstate), u32::from(actual.itstate)),
        ("control", predicted.control, actual.control),
        ("ipsr", predicted.ipsr, actual.ipsr),
        ("fpscr", predicted.fpscr, actual.fpscr),
    ];
    for (name, a, b) in fields {
        if a != b {
            return Some(format!("{name} replayed {a:#010x}, interpreted {b:#010x}"));
        }
    }
    if predicted.icount != actual.icount {
        return Some(format!("retire count replayed {}, interpreted {}", predicted.icount, actual.icount));
    }
    if predicted.tb_icount != actual.tb_icount {
        return Some(format!("block start count replayed {}, interpreted {}", predicted.tb_icount, actual.tb_icount));
    }
    for (i, (a, b)) in predicted.window.iter().zip(&actual.window).enumerate() {
        if a != b {
            let addr = predicted.base.wrapping_add(4 * i as u32);
            return Some(format!("memory at {addr:#010x} replayed {a:#010x}, interpreted {b:#010x}"));
        }
    }
    None
}
