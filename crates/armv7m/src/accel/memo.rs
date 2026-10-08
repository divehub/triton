//! Memo entries and the per-routine memo table.
//!
//! An entry maps a key (argument registers, argument S registers, FPSCR control fields) to the complete
//! effect of one call: instruction count, register/flag/FPSCR results, the stores to the routine's frame and
//! which registers are copies of their value at entry. Entries are created only from a call the interpreter
//! executed and the dependency tracker proved to depend on the key alone (`track.rs`).

/// Key width in words: r0..r3, s0, s1, FPSCR control fields, spare.
pub(super) const KEY_WORDS: usize = 8;

pub(super) type Key = [u32; KEY_WORDS];

/// Where a value written by a memo entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Src {
    /// A function of the key, recorded.
    Const(u32),
    /// The value register `id` had at entry (core 0..=14, S registers 16..=47).
    Copy(u8),
}

/// A register the call leaves holding recorded constants (`Const`) or a copy of another register's entry value (`Copy`),
/// as the recorder describes it; [`Memo::new`] splits them into the replay layout.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegWrite {
    pub id: u8,
    pub src: Src,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct StoreWrite {
    /// Offset from the entry SP (negative).
    pub off: i32,
    pub src: Src,
}

/// At most this many registers may end as copies of other registers' entry values (the recorded effect is refused otherwise).
pub(super) const MAX_COPIES: usize = 8;

#[derive(Clone, Debug)]
pub(super) struct Memo {
    /// Instructions the call retires.
    pub count: u32,
    pub uses_fp: bool,
    /// Core registers r0..r14 the call leaves with recorded values (bit i = r_i) and the values in ascending register order.
    pub core_mask: u16,
    pub core_vals: Vec<u32>,
    /// The same for S registers.
    pub s_mask: u32,
    pub s_vals: Vec<u32>,
    /// Registers that end as copies of the entry value of another register: `(destination id, source id)`, ids as in
    /// [`Src::Copy`]. Usually empty (a callee-saved register is restored to its own entry value and needs no entry).
    pub copies: Vec<(u8, u8)>,
    /// APSR flag bits (N = 8 .. V = 1) the call defines, and their values.
    pub nzcv_mask: u8,
    pub nzcv: u8,
    /// FPSCR N Z C V (bits 31:28) when a `vcmp` ran.
    pub fpscr_nzcv: Option<u32>,
    /// Cumulative exception flags the call raises (OR-ed into the FPSCR).
    pub fpscr_cum: u32,
    pub stores: Vec<StoreWrite>,
    /// Lowest store offset (0 when the call stores nothing).
    pub min_off: i32,
}

pub(super) struct Entry {
    pub key: Key,
    /// `None`: the path for this key was proven unsafe; never try again.
    pub memo: Option<Memo>,
}

/// Open-addressing table of entries, bounded.
pub(super) struct MemoTable {
    entries: Vec<Entry>,
    index: Vec<u32>,
}

pub(super) const MAX_ENTRIES: usize = 8192;

#[inline(always)]
pub(super) fn hash_key(key: &Key) -> u64 {
    // Two words per multiplication (a 64-bit multiply-xor mix): the key is hashed on every call of a routine.
    let pair = |a: u32, b: u32| u64::from(a) | u64::from(b) << 32;
    let mut h = pair(key[0], key[1]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h = (h ^ (h >> 32) ^ pair(key[2], key[3])).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h = (h ^ (h >> 29) ^ pair(key[4], key[5])).wrapping_mul(0x1656_67B1_9E37_79F9);
    h = (h ^ (h >> 31) ^ pair(key[6], key[7])).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 32)
}

impl Memo {
    /// Builds the replay layout from the recorder's list of register effects. `None` when more registers end as copies
    /// than [`MAX_COPIES`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        count: u32,
        uses_fp: bool,
        regs: &[RegWrite],
        nzcv_mask: u8,
        nzcv: u8,
        fpscr_nzcv: Option<u32>,
        fpscr_cum: u32,
        stores: Vec<StoreWrite>,
        min_off: i32,
    ) -> Option<Memo> {
        let (mut core_mask, mut s_mask) = (0u16, 0u32);
        let mut consts: Vec<(u8, u32)> = Vec::new();
        let mut copies = Vec::new();
        for w in regs {
            match w.src {
                Src::Const(v) => consts.push((w.id, v)),
                Src::Copy(src) => copies.push((w.id, src)),
            }
        }
        if copies.len() > MAX_COPIES {
            return None;
        }
        consts.sort_by_key(|&(id, _)| id);
        let (mut core_vals, mut s_vals) = (Vec::new(), Vec::new());
        for (id, v) in consts {
            if id < 16 {
                core_mask |= 1 << id;
                core_vals.push(v);
            } else {
                s_mask |= 1 << (id - 16);
                s_vals.push(v);
            }
        }
        Some(Memo { count, uses_fp, core_mask, core_vals, s_mask, s_vals, copies, nzcv_mask, nzcv, fpscr_nzcv, fpscr_cum, stores, min_off })
    }
}

impl MemoTable {
    pub fn new() -> Self {
        MemoTable { entries: Vec::new(), index: vec![0; 64] }
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= MAX_ENTRIES
    }

    #[inline]
    pub fn find(&self, key: &Key, hash: u64) -> Option<&Entry> {
        let mask = self.index.len() - 1;
        let mut slot = (hash >> 20) as usize & mask;
        loop {
            let i = self.index[slot];
            if i == 0 {
                return None;
            }
            let entry = &self.entries[(i - 1) as usize];
            if entry.key == *key {
                return Some(entry);
            }
            slot = (slot + 1) & mask;
        }
    }

    pub fn insert(&mut self, key: Key, memo: Option<Memo>) {
        if self.is_full() {
            return;
        }
        if (self.entries.len() + 1) * 2 > self.index.len() {
            self.grow();
        }
        let hash = hash_key(&key);
        self.entries.push(Entry { key, memo });
        let id = self.entries.len() as u32;
        let mask = self.index.len() - 1;
        let mut slot = (hash >> 20) as usize & mask;
        while self.index[slot] != 0 {
            slot = (slot + 1) & mask;
        }
        self.index[slot] = id;
    }

    fn grow(&mut self) {
        let new_len = self.index.len() * 2;
        self.index = vec![0; new_len];
        let mask = new_len - 1;
        for (n, entry) in self.entries.iter().enumerate() {
            let mut slot = (hash_key(&entry.key) >> 20) as usize & mask;
            while self.index[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            self.index[slot] = n as u32 + 1;
        }
    }

    pub fn memos(&self) -> usize {
        self.entries.iter().filter(|e| e.memo.is_some()).count()
    }
}
