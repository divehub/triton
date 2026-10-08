//! Machine log: bounded entry ring, severity counters and warn-once keys.
//!
//! Logging is a slow-path facility. Peripherals call `Ctx::log*` (or the
//! `emu_*!` macros); messages below the active threshold are not formatted.
//! Severity names follow Renode (`Noisy`, `Debug`, `Info`, `Warning`, `Error`).

use crate::Time;
use std::collections::VecDeque;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LogLevel {
    Noisy = 0,
    Debug = 1,
    Info = 2,
    Warning = 3,
    Error = 4,
}

impl LogLevel {
    pub const ALL: [LogLevel; 5] = [LogLevel::Noisy, LogLevel::Debug, LogLevel::Info, LogLevel::Warning, LogLevel::Error];

    pub fn name(self) -> &'static str {
        match self {
            LogLevel::Noisy => "noisy",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// Virtual time at which the message was logged.
    pub time: Time,
    pub level: LogLevel,
    /// Peripheral instance name, or `"machine"` for messages raised by the framework itself.
    pub source: String,
    pub message: String,
}

impl fmt::Display for LogEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:.6}s] {} {}: {}", crate::to_secs_f64(self.time), self.level, self.source, self.message)
    }
}

/// Bounded log with per-level counters. The ring keeps the newest `capacity` entries.
#[derive(Debug)]
pub struct LogBuffer {
    entries: VecDeque<LogEntry>,
    capacity: usize,
    threshold: LogLevel,
    counts: [u64; 5],
    dropped: u64,
}

impl LogBuffer {
    pub const DEFAULT_CAPACITY: usize = 4096;

    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            threshold: LogLevel::Info,
            counts: [0; 5],
            dropped: 0,
        }
    }

    /// Entries below this level are neither stored nor counted.
    pub fn threshold(&self) -> LogLevel {
        self.threshold
    }

    pub fn set_threshold(&mut self, level: LogLevel) {
        self.threshold = level;
    }

    #[inline]
    pub fn enabled(&self, level: LogLevel) -> bool {
        level >= self.threshold
    }

    pub fn push(&mut self, entry: LogEntry) {
        if !self.enabled(entry.level) {
            return;
        }
        self.counts[entry.level as usize] += 1;
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(entry);
    }

    pub fn entries(&self) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of entries recorded at `level` since creation (including entries since evicted).
    pub fn count(&self, level: LogLevel) -> u64 {
        self.counts[level as usize]
    }

    /// Entries evicted because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Removes and returns all stored entries (counters are kept).
    pub fn drain(&mut self) -> Vec<LogEntry> {
        self.entries.drain(..).collect()
    }

    /// True if any stored entry at `level` or above contains `needle`.
    pub fn contains(&self, level: LogLevel, needle: &str) -> bool {
        self.entries.iter().any(|e| e.level >= level && e.message.contains(needle))
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Set of `(source, key)` pairs for warn-once logging. Open addressing with a
/// multiplicative hash; no allocation after growth, no SipHash. The set is bounded
/// (`DEFAULT_LIMIT` entries): firmware that probes a huge unmapped range must not exhaust
/// memory, so once the set is full further new keys are treated as already seen.
#[derive(Debug)]
pub struct WarnSet {
    slots: Vec<u128>,
    len: usize,
    limit: usize,
    suppressed: u64,
}

impl WarnSet {
    /// Maximum number of distinct keys remembered (about 0.5 MiB of table).
    pub const DEFAULT_LIMIT: usize = 16_384;

    pub fn new() -> Self {
        Self::with_limit(Self::DEFAULT_LIMIT)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self { slots: vec![0; 64], len: 0, limit, suppressed: 0 }
    }

    /// True once `limit` keys are stored (new keys are then suppressed).
    pub fn is_full(&self) -> bool {
        self.len >= self.limit
    }

    /// Number of new keys refused because the set was full.
    pub fn suppressed(&self) -> u64 {
        self.suppressed
    }

    #[inline]
    fn encode(source: u32, key: u64) -> u128 {
        // Bit 127 marks an occupied slot so the all-zero key is representable.
        (1u128 << 127) | (u128::from(source) << 64) | u128::from(key)
    }

    #[inline]
    fn hash(code: u128) -> u64 {
        let folded = (code as u64) ^ ((code >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mixed = folded.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed ^ (mixed >> 32)
    }

    /// Inserts the pair; returns `true` if it was not present before. When the set is full a new
    /// key is not stored and `false` is returned (counted in `suppressed`).
    pub fn insert(&mut self, source: u32, key: u64) -> bool {
        let code = Self::encode(source, key);
        let mask = self.slots.len() - 1;
        let mut index = Self::hash(code) as usize & mask;
        loop {
            let slot = self.slots[index];
            if slot == 0 {
                break;
            }
            if slot == code {
                return false;
            }
            index = (index + 1) & mask;
        }
        // Not present.
        if self.len >= self.limit {
            self.suppressed += 1;
            return false;
        }
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
            let mask = self.slots.len() - 1;
            index = Self::hash(code) as usize & mask;
            while self.slots[index] != 0 {
                index = (index + 1) & mask;
            }
        }
        self.slots[index] = code;
        self.len += 1;
        true
    }

    pub fn contains(&self, source: u32, key: u64) -> bool {
        let code = Self::encode(source, key);
        let mask = self.slots.len() - 1;
        let mut index = Self::hash(code) as usize & mask;
        loop {
            let slot = self.slots[index];
            if slot == 0 {
                return false;
            }
            if slot == code {
                return true;
            }
            index = (index + 1) & mask;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = 0;
        }
        self.len = 0;
        self.suppressed = 0;
    }

    fn grow(&mut self) {
        let old = std::mem::replace(&mut self.slots, vec![0; 0]);
        self.slots = vec![0; old.len() * 2];
        self.len = 0;
        let mask = self.slots.len() - 1;
        for code in old {
            if code != 0 {
                let mut index = Self::hash(code) as usize & mask;
                while self.slots[index] != 0 {
                    index = (index + 1) & mask;
                }
                self.slots[index] = code;
                self.len += 1;
            }
        }
    }
}

impl Default for WarnSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: LogLevel, message: &str) -> LogEntry {
        LogEntry { time: 0, level, source: "t".into(), message: message.into() }
    }

    #[test]
    fn threshold_filters_and_counts() {
        let mut log = LogBuffer::new();
        log.push(entry(LogLevel::Debug, "hidden"));
        log.push(entry(LogLevel::Info, "info"));
        log.push(entry(LogLevel::Warning, "warn"));
        assert_eq!(log.len(), 2);
        assert_eq!(log.count(LogLevel::Debug), 0);
        assert_eq!(log.count(LogLevel::Warning), 1);
        log.set_threshold(LogLevel::Noisy);
        assert!(log.enabled(LogLevel::Noisy));
        log.push(entry(LogLevel::Noisy, "now visible"));
        assert_eq!(log.len(), 3);
        assert!(log.contains(LogLevel::Noisy, "visible"));
        assert!(!log.contains(LogLevel::Error, "visible"));
    }

    #[test]
    fn ring_evicts_oldest() {
        let mut log = LogBuffer::with_capacity(3);
        for i in 0..5 {
            log.push(entry(LogLevel::Error, &format!("m{i}")));
        }
        let messages: Vec<String> = log.entries().map(|e| e.message.clone()).collect();
        assert_eq!(messages, ["m2", "m3", "m4"]);
        assert_eq!(log.dropped(), 2);
        assert_eq!(log.count(LogLevel::Error), 5);
        let drained = log.drain();
        assert_eq!(drained.len(), 3);
        assert!(log.is_empty());
        assert_eq!(log.count(LogLevel::Error), 5);
    }

    #[test]
    fn entry_display() {
        let e = LogEntry { time: crate::TICKS_PER_SECOND / 2, level: LogLevel::Warning, source: "uart".into(), message: "bad".into() };
        assert_eq!(e.to_string(), "[0.500000s] warning uart: bad");
    }

    #[test]
    fn warn_set_dedups_and_grows() {
        let mut set = WarnSet::new();
        assert!(set.insert(1, 0));
        assert!(!set.insert(1, 0));
        assert!(set.insert(2, 0));
        assert!(set.insert(1, 1));
        assert!(set.contains(1, 1));
        assert!(!set.contains(3, 3));
        for key in 0..10_000u64 {
            set.insert(7, key * 4096);
        }
        assert_eq!(set.len(), 3 + 10_000);
        for key in 0..10_000u64 {
            assert!(set.contains(7, key * 4096));
            assert!(!set.insert(7, key * 4096));
        }
        assert!(set.contains(1, 0));
        set.clear();
        assert!(set.is_empty());
        assert!(set.insert(1, 0));
    }

    #[test]
    fn warn_set_is_bounded() {
        let mut set = WarnSet::with_limit(100);
        for key in 0..100u64 {
            assert!(set.insert(0, key));
        }
        assert!(set.is_full());
        // New keys are refused (as if already seen) and counted; old keys still report "seen".
        assert!(!set.insert(0, 1000));
        assert!(!set.insert(0, 1001));
        assert!(!set.insert(0, 5));
        assert_eq!(set.suppressed(), 2);
        assert_eq!(set.len(), 100);
        assert!(!set.contains(0, 1000));
        // Memory stays bounded even for a million distinct probes.
        for key in 0..1_000_000u64 {
            set.insert(1, key);
        }
        assert_eq!(set.len(), 100);
        assert!(set.suppressed() > 999_000);
    }
}
