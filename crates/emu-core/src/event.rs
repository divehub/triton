//! Event queue ordered by `(time, key)`.
//!
//! An indexed binary min-heap over a slab of event slots: `schedule`, `pop_due`
//! and `cancel` are O(log n) and allocation-free once the queue has reached its
//! working size. The `key` makes events at the same time fire in a defined order:
//!
//! * **clock events** (`schedule_clock`, the limit events of clock entries) use
//!   the entry's creation index as key, so simultaneous limits run in creation
//!   order (Renode `BaseClockSource.Update` walks its entry list in order);
//! * **ordinary events** (`schedule`) use `1 << 63 | insertion counter`, so they
//!   sort after every clock event of the same time and keep scheduling order.
//!
//! Cancelling removes the entry from the heap immediately (no tombstones), and
//! `EventId`s carry a generation so stale ids (already fired or cancelled, slot
//! reused) are rejected.

use crate::peripheral::PeriphId;
use crate::Time;

const NOT_QUEUED: u32 = u32::MAX;
/// `Slot::clock` value of an ordinary event.
const NO_CLOCK: u32 = u32::MAX;
/// Key bit that sorts ordinary events after clock events of the same time.
const ORDINARY: u64 = 1 << 63;

/// Handle to a scheduled event. Cheap to copy; `EventId::NONE` never matches an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId(u64);

impl EventId {
    pub const NONE: EventId = EventId(u64::MAX);

    fn new(slot: u32, generation: u32) -> EventId {
        EventId((u64::from(generation) << 32) | u64::from(slot))
    }

    fn slot(self) -> u32 {
        self.0 as u32
    }

    fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub fn is_none(self) -> bool {
        self == EventId::NONE
    }
}

impl Default for EventId {
    fn default() -> Self {
        EventId::NONE
    }
}

#[derive(Clone, Copy)]
struct HeapEntry {
    time: Time,
    key: u64,
    slot: u32,
}

#[inline(always)]
fn before(a: &HeapEntry, b: &HeapEntry) -> bool {
    a.time < b.time || (a.time == b.time && a.key < b.key)
}

struct Slot {
    periph: PeriphId,
    token: u64,
    /// Registry slot of the clock entry that owns this event, or `NO_CLOCK`.
    clock: u32,
    generation: u32,
    heap_pos: u32,
}

/// A queued event as reported by `EventQueue::pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingEvent {
    pub time: Time,
    pub periph: PeriphId,
    pub token: u64,
    /// True for the limit event of a clock entry, false for an ordinary event.
    pub clock: bool,
}

/// An event removed by `EventQueue::pop_due_ex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoppedEvent {
    pub time: Time,
    pub periph: PeriphId,
    pub token: u64,
    /// Registry slot of the owning clock entry (`None` for an ordinary event).
    pub clock: Option<u32>,
}

pub struct EventQueue {
    heap: Vec<HeapEntry>,
    slots: Vec<Slot>,
    free: Vec<u32>,
    next_seq: u64,
}

impl EventQueue {
    pub fn new() -> Self {
        Self { heap: Vec::new(), slots: Vec::new(), free: Vec::new(), next_seq: 0 }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            heap: Vec::with_capacity(capacity),
            slots: Vec::with_capacity(capacity),
            free: Vec::with_capacity(capacity),
            next_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Time of the earliest queued event.
    #[inline]
    pub fn peek_time(&self) -> Option<Time> {
        self.heap.first().map(|e| e.time)
    }

    /// Time and kind (`true` = clock event) of the earliest queued event.
    #[inline]
    pub fn peek_head(&self) -> Option<(Time, bool)> {
        self.heap.first().map(|e| (e.time, e.key & ORDINARY == 0))
    }

    /// Schedules an ordinary `token` for `periph` at `time`. Ordinary events at the same time fire in
    /// scheduling order, after every clock event of that time.
    pub fn schedule(&mut self, time: Time, periph: PeriphId, token: u64) -> EventId {
        let key = ORDINARY | self.next_seq;
        self.next_seq = (self.next_seq + 1) & !ORDINARY;
        self.insert(time, key, periph, token, NO_CLOCK)
    }

    /// Schedules the limit event of a clock entry. `order` is the entry's creation index (it must be
    /// below `1 << 63`); `clock` is the registry slot handed back by `pop_due_ex`.
    pub fn schedule_clock(&mut self, time: Time, order: u64, periph: PeriphId, token: u64, clock: u32) -> EventId {
        debug_assert!(order & ORDINARY == 0 && clock != NO_CLOCK);
        self.insert(time, order, periph, token, clock)
    }

    fn insert(&mut self, time: Time, key: u64, periph: PeriphId, token: u64, clock: u32) -> EventId {
        let index = match self.free.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.periph = periph;
                slot.token = token;
                slot.clock = clock;
                index
            }
            None => {
                self.slots.push(Slot { periph, token, clock, generation: 0, heap_pos: NOT_QUEUED });
                (self.slots.len() - 1) as u32
            }
        };
        let pos = self.heap.len();
        self.heap.push(HeapEntry { time, key, slot: index });
        self.slots[index as usize].heap_pos = pos as u32;
        self.sift_up(pos);
        EventId::new(index, self.slots[index as usize].generation)
    }

    /// Cancels a queued event. Returns `false` if the id is stale, already fired or `NONE`.
    pub fn cancel(&mut self, id: EventId) -> bool {
        if !self.is_pending(id) {
            return false;
        }
        let index = id.slot();
        let pos = self.slots[index as usize].heap_pos as usize;
        self.remove_at(pos);
        self.release(index);
        true
    }

    /// True while the event is queued (not yet fired and not cancelled).
    pub fn is_pending(&self, id: EventId) -> bool {
        if id.is_none() {
            return false;
        }
        match self.slots.get(id.slot() as usize) {
            Some(slot) => slot.generation == id.generation() && slot.heap_pos != NOT_QUEUED,
            None => false,
        }
    }

    /// Scheduled time of a queued event.
    pub fn time_of(&self, id: EventId) -> Option<Time> {
        if !self.is_pending(id) {
            return None;
        }
        let pos = self.slots[id.slot() as usize].heap_pos as usize;
        Some(self.heap[pos].time)
    }

    /// Removes and returns the earliest event if its time is `<= now`.
    #[inline]
    pub fn pop_due(&mut self, now: Time) -> Option<(Time, PeriphId, u64)> {
        self.pop_due_ex(now).map(|e| (e.time, e.periph, e.token))
    }

    /// Like `pop_due`, also reporting the owning clock entry.
    #[inline]
    pub fn pop_due_ex(&mut self, now: Time) -> Option<PoppedEvent> {
        let top = *self.heap.first()?;
        if top.time > now {
            return None;
        }
        self.remove_at(0);
        let slot = &self.slots[top.slot as usize];
        let clock = if slot.clock == NO_CLOCK { None } else { Some(slot.clock) };
        let result = PoppedEvent { time: top.time, periph: slot.periph, token: slot.token, clock };
        self.release(top.slot);
        Some(result)
    }

    /// Drops every queued event and invalidates all outstanding ids.
    pub fn clear(&mut self) {
        while let Some(entry) = self.heap.pop() {
            self.release(entry.slot);
        }
    }

    /// Cancels every queued event of `periph`; returns how many were removed.
    pub fn cancel_all_for(&mut self, periph: PeriphId) -> usize {
        let mut removed = 0;
        let mut pos = 0;
        while pos < self.heap.len() {
            let slot = self.heap[pos].slot;
            if self.slots[slot as usize].periph == periph {
                self.remove_at(pos);
                self.release(slot);
                removed += 1;
                // The entry moved into `pos` has not been examined yet.
            } else {
                pos += 1;
            }
        }
        removed
    }

    /// Snapshot of the queue in firing order (allocates; for diagnostics and tests).
    pub fn pending(&self) -> Vec<PendingEvent> {
        let mut entries: Vec<HeapEntry> = self.heap.clone();
        entries.sort_by(|a, b| (a.time, a.key).cmp(&(b.time, b.key)));
        entries
            .iter()
            .map(|e| {
                let slot = &self.slots[e.slot as usize];
                PendingEvent { time: e.time, periph: slot.periph, token: slot.token, clock: slot.clock != NO_CLOCK }
            })
            .collect()
    }

    fn release(&mut self, index: u32) {
        let slot = &mut self.slots[index as usize];
        slot.heap_pos = NOT_QUEUED;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(index);
    }

    /// Removes the heap entry at `pos` (the slot itself is released by the caller).
    fn remove_at(&mut self, pos: usize) {
        let last = self.heap.len() - 1;
        if pos == last {
            self.heap.pop();
            return;
        }
        let moved = self.heap[last];
        self.heap.swap_remove(pos);
        self.slots[moved.slot as usize].heap_pos = pos as u32;
        if pos > 0 && before(&self.heap[pos], &self.heap[(pos - 1) / 2]) {
            self.sift_up(pos);
        } else {
            self.sift_down(pos);
        }
    }

    fn sift_up(&mut self, mut pos: usize) {
        let entry = self.heap[pos];
        while pos > 0 {
            let parent = (pos - 1) / 2;
            let parent_entry = self.heap[parent];
            if !before(&entry, &parent_entry) {
                break;
            }
            self.heap[pos] = parent_entry;
            self.slots[parent_entry.slot as usize].heap_pos = pos as u32;
            pos = parent;
        }
        self.heap[pos] = entry;
        self.slots[entry.slot as usize].heap_pos = pos as u32;
    }

    fn sift_down(&mut self, mut pos: usize) {
        let len = self.heap.len();
        let entry = self.heap[pos];
        loop {
            let left = 2 * pos + 1;
            if left >= len {
                break;
            }
            let right = left + 1;
            let child = if right < len && before(&self.heap[right], &self.heap[left]) { right } else { left };
            let child_entry = self.heap[child];
            if !before(&child_entry, &entry) {
                break;
            }
            self.heap[pos] = child_entry;
            self.slots[child_entry.slot as usize].heap_pos = pos as u32;
            pos = child;
        }
        self.heap[pos] = entry;
        self.slots[entry.slot as usize].heap_pos = pos as u32;
    }

    #[cfg(test)]
    fn check_invariants(&self) {
        for (pos, entry) in self.heap.iter().enumerate() {
            if pos > 0 {
                let parent = &self.heap[(pos - 1) / 2];
                assert!(!before(entry, parent), "heap order violated at {pos}");
            }
            assert_eq!(self.slots[entry.slot as usize].heap_pos as usize, pos, "slot back-pointer");
        }
        let queued = self.slots.iter().filter(|s| s.heap_pos != NOT_QUEUED).count();
        assert_eq!(queued, self.heap.len());
        assert_eq!(self.slots.len(), self.heap.len() + self.free.len());
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn p(n: u32) -> PeriphId {
        PeriphId(n)
    }

    #[test]
    fn orders_by_time_then_insertion() {
        let mut q = EventQueue::new();
        q.schedule(30, p(0), 3);
        q.schedule(10, p(1), 1);
        q.schedule(20, p(2), 2);
        q.schedule(10, p(3), 11);
        q.schedule(10, p(4), 12);
        assert_eq!(q.peek_time(), Some(10));
        assert_eq!(q.pop_due(9), None);
        let order: Vec<u64> = std::iter::from_fn(|| q.pop_due(u64::MAX)).map(|e| e.2).collect();
        assert_eq!(order, [1, 11, 12, 2, 3]);
        assert!(q.is_empty());
        assert_eq!(q.peek_time(), None);
    }

    #[test]
    fn clock_events_sort_before_ordinary_events_and_by_creation_order() {
        let mut q = EventQueue::new();
        q.schedule(10, p(0), 100); // ordinary, scheduled first
        q.schedule_clock(10, 7, p(1), 1, 3);
        q.schedule_clock(10, 2, p(2), 2, 4);
        q.schedule(10, p(0), 101);
        q.schedule_clock(5, 9, p(3), 3, 5);
        assert_eq!(q.peek_head(), Some((5, true)));
        let pending: Vec<(u64, bool)> = q.pending().iter().map(|e| (e.token, e.clock)).collect();
        assert_eq!(pending, [(3, true), (2, true), (1, true), (100, false), (101, false)]);
        let order: Vec<(u64, Option<u32>)> =
            std::iter::from_fn(|| q.pop_due_ex(u64::MAX)).map(|e| (e.token, e.clock)).collect();
        assert_eq!(order, [(3, Some(5)), (2, Some(4)), (1, Some(3)), (100, None), (101, None)]);
        assert_eq!(q.peek_head(), None);
        q.check_invariants();
    }

    #[test]
    fn pop_due_respects_now() {
        let mut q = EventQueue::new();
        q.schedule(100, p(0), 1);
        q.schedule(200, p(0), 2);
        assert_eq!(q.pop_due(99), None);
        assert_eq!(q.pop_due(100), Some((100, p(0), 1)));
        assert_eq!(q.pop_due(150), None);
        assert_eq!(q.pop_due(200), Some((200, p(0), 2)));
        assert_eq!(q.pop_due(1_000), None);
    }

    #[test]
    fn cancel_head_middle_tail_and_stale() {
        let mut q = EventQueue::new();
        let a = q.schedule(10, p(0), 1);
        let b = q.schedule(20, p(0), 2);
        let c = q.schedule(30, p(0), 3);
        let d = q.schedule(40, p(0), 4);
        assert!(q.cancel(b));
        q.check_invariants();
        assert!(!q.cancel(b), "double cancel");
        assert!(q.cancel(d));
        assert!(q.cancel(a));
        q.check_invariants();
        assert_eq!(q.len(), 1);
        assert_eq!(q.time_of(c), Some(30));
        assert_eq!(q.pop_due(u64::MAX), Some((30, p(0), 3)));
        assert!(!q.cancel(c), "already fired");
        assert!(!q.is_pending(c));
        assert!(!q.cancel(EventId::NONE));
        assert!(!q.is_pending(EventId::NONE));
        // Slot reuse must not resurrect an old id.
        let e = q.schedule(50, p(0), 5);
        assert_ne!(e, a);
        assert!(!q.cancel(a));
        assert!(!q.cancel(c));
        assert!(q.is_pending(e));
        assert!(q.cancel(e));
    }

    #[test]
    fn cancel_all_for_peripheral() {
        let mut q = EventQueue::new();
        for i in 0..20u64 {
            q.schedule(i * 5, p((i % 3) as u32), i);
        }
        assert_eq!(q.cancel_all_for(p(1)), 7);
        q.check_invariants();
        let remaining: Vec<(u32, u64)> = q.pending().iter().map(|e| (e.periph.0, e.token)).collect();
        assert_eq!(remaining.len(), 13);
        assert!(remaining.iter().all(|(periph, _)| *periph != 1));
        assert!(remaining.windows(2).all(|w| w[0].1 < w[1].1));
        q.clear();
        assert!(q.is_empty());
        q.check_invariants();
    }

    #[test]
    fn clear_invalidates_ids() {
        let mut q = EventQueue::new();
        let ids: Vec<EventId> = (0..8).map(|i| q.schedule(i, p(0), i)).collect();
        q.clear();
        assert!(q.is_empty());
        for id in ids {
            assert!(!q.is_pending(id));
            assert!(!q.cancel(id));
        }
        q.schedule(1, p(0), 1);
        assert_eq!(q.len(), 1);
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[test]
    fn matches_reference_model_under_random_operations() {
        let mut q = EventQueue::new();
        let mut reference: BTreeMap<(Time, u64), (u32, u64)> = BTreeMap::new();
        let mut ids: Vec<(EventId, (Time, u64))> = Vec::new();
        let mut rng = Lcg(12345);
        let mut seq = 0u64;
        let mut now = 0u64;
        for step in 0..20_000 {
            match rng.next() % 10 {
                0..=4 => {
                    let time = now + rng.next() % 50;
                    let periph = (rng.next() % 4) as u32;
                    let token = rng.next();
                    let id = q.schedule(time, p(periph), token);
                    reference.insert((time, seq), (periph, token));
                    ids.push((id, (time, seq)));
                    seq += 1;
                }
                5..=6 if !ids.is_empty() => {
                    let index = (rng.next() as usize) % ids.len();
                    let (id, key) = ids.swap_remove(index);
                    let expected = reference.remove(&key).is_some();
                    assert_eq!(q.cancel(id), expected, "step {step}");
                }
                _ => {
                    now += rng.next() % 20;
                    loop {
                        let fired = q.pop_due(now);
                        let expected = reference.iter().next().map(|(k, v)| (*k, *v)).filter(|(k, _)| k.0 <= now);
                        match (fired, expected) {
                            (None, None) => break,
                            (Some((time, periph, token)), Some((key, (e_periph, e_token)))) => {
                                assert_eq!((time, periph.0, token), (key.0, e_periph, e_token), "step {step}");
                                reference.remove(&key);
                            }
                            (a, b) => panic!("step {step}: queue {a:?} vs reference {b:?}"),
                        }
                    }
                }
            }
            assert_eq!(q.len(), reference.len());
            assert_eq!(q.peek_time(), reference.keys().next().map(|k| k.0));
            if step % 97 == 0 {
                q.check_invariants();
            }
        }
        q.check_invariants();
    }

    #[test]
    fn steady_state_does_not_grow() {
        let mut q = EventQueue::new();
        let mut rng = Lcg(7);
        for i in 0..64u64 {
            q.schedule(i * 10, p(0), i);
        }
        let capacities = |q: &EventQueue| (q.heap.capacity(), q.slots.capacity(), q.free.capacity());
        let mut now = 640;
        let mut churn = |q: &mut EventQueue, rng: &mut Lcg, rounds: u64| {
            for i in 0..rounds {
                // Periodic pattern: pop the earliest, schedule a replacement, sometimes cancel+reschedule.
                let (time, _, _) = q.pop_due(u64::MAX).unwrap();
                now = now.max(time);
                let id = q.schedule(now + 1 + rng.next() % 640, p(0), i);
                if i % 5 == 0 {
                    assert!(q.cancel(id));
                    q.schedule(now + 1 + rng.next() % 640, p(0), i);
                }
            }
        };
        churn(&mut q, &mut rng, 1_000); // warm-up reaches the working size
        let before = capacities(&q);
        churn(&mut q, &mut rng, 200_000);
        assert_eq!(capacities(&q), before, "queue reallocated in steady state");
        assert_eq!(q.len(), 64);
        q.check_invariants();
    }

    #[test]
    #[ignore = "micro-benchmark; run with --ignored --nocapture"]
    fn bench_schedule_pop() {
        let mut q = EventQueue::new();
        let mut rng = Lcg(99);
        for i in 0..32u64 {
            q.schedule(i * 1000, p(0), i);
        }
        let start = std::time::Instant::now();
        let n = 5_000_000u64;
        let mut now = 0;
        for i in 0..n {
            let (time, _, _) = q.pop_due(u64::MAX).unwrap();
            now = now.max(time);
            q.schedule(now + 1 + rng.next() % 32_000, p(0), i);
        }
        let elapsed = start.elapsed();
        println!("event queue: {:.1} ns per pop+schedule", elapsed.as_nanos() as f64 / n as f64);
    }
}
