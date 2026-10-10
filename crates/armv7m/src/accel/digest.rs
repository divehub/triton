//! A digest of everything a routine acceleration must leave exactly as interpretation would: the
//! architectural state (registers, flags, IT state, FPSCR and the VFP registers, control, exclusive monitor,
//! retire counts) and the translation state that can change later block partitioning (the predecode cache,
//! its VFP and page tables, the cut-block history). Two cores that executed the same program, one with
//! routine acceleration and one without, must have equal digests at every chunk boundary; so must two cores with
//! the idle fast-forward on and off (it reads a candidate loop without translating it and translates an instruction
//! only when it executes it, as interpretation does; `tests/tb_start.rs`, `ngc-cli bench --verify-idle-ff`).
//!
//! Not included, because they are host-speed state and never influence the guest: the idle fast-forward's
//! tables and its `FL_NOFF` cache marks, the acceleration's own memo tables, and the transient cut context.

use crate::cpu::Cpu;
use crate::op::{Op, FL_NOFF};

struct Mixer(u64);

impl Mixer {
    #[inline]
    fn word(&mut self, v: u64) {
        self.0 = (self.0 ^ v).wrapping_mul(0x0000_0100_0000_01B3);
        self.0 ^= self.0 >> 29;
    }

    fn op(&mut self, op: &Op) {
        self.word(u64::from(op.kind as u8) | u64::from(op.flags & !FL_NOFF) << 8 | u64::from(op.rd) << 16 | u64::from(op.rn) << 24 | u64::from(op.rm) << 32 | u64::from(op.ra) << 40 | u64::from(op.len) << 48 | u64::from(op.x) << 56);
        self.word(u64::from(op.imm) | u64::from(op.raw) << 32);
    }
}

impl Cpu {
    /// Digest of the architectural and translation state (see the module documentation).
    pub fn exactness_digest(&self) -> u64 {
        let mut m = Mixer(0xCBF2_9CE4_8422_2325);
        for r in self.r {
            m.word(u64::from(r));
        }
        m.word(u64::from(self.apsr));
        m.word(u64::from(self.ge));
        m.word(u64::from(self.itstate));
        m.word(u64::from(self.thumb));
        m.word(u64::from(self.ipsr));
        m.word(u64::from(self.control));
        m.word(u64::from(self.sp_other));
        m.word(u64::from(self.use_psp));
        m.word(self.exclusive.map_or(u64::MAX, u64::from));
        for s in self.fp.s {
            m.word(u64::from(s));
        }
        m.word(u64::from(self.fp.fpscr));
        m.word(self.icount);
        m.word(self.tb_icount);
        m.word(self.clock_time);
        m.word(u64::from(self.nvic.primask) | u64::from(self.nvic.faultmask) << 1);
        m.word(self.cache_base as u64 | (self.cache_span as u64) << 32);
        for op in &self.cache {
            m.op(op);
        }
        m.word(self.vfp_table.len() as u64);
        for insn in &self.vfp_table {
            let (a, b) = insn.encoding();
            m.word(u64::from(a) << 16 | u64::from(b));
        }
        m.word(self.page_ops.len() as u64);
        for op in &self.page_ops {
            m.op(op);
        }
        m.word(self.cut_entries.len() as u64);
        for e in &self.cut_entries {
            m.op(&e.inner);
            m.word(e.idx as u64);
            for &c in &e.cuts {
                m.word(u64::from(c));
            }
        }
        m.0
    }
}
