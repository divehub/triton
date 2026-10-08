//! Clock entries through the machine: Renode `ClockEntry`/`BaseClockSource` semantics, `LimitTimer`,
//! `ManagedThread`, `schedule_action`, sync registers and the clock-lag model, checked against the
//! pure-time expectations of `testdata/renode-micro-vectors.json` and the periods of renode-semantics 3.4.

use emu_core::testing::Harness;
use emu_core::*;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fired {
    who: &'static str,
    token: u64,
    now: Time,
    scheduled: Time,
}

type Log = Rc<RefCell<Vec<Fired>>>;
type Notes = Rc<RefCell<Vec<String>>>;
type Hook = Box<dyn FnMut(&mut Ctx<'_>, &[ClockId], u64)>;

fn log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

fn notes() -> Notes {
    Rc::new(RefCell::new(Vec::new()))
}

/// Owns clock entries created in `attach` order and records every event it receives.
struct Rec {
    name: &'static str,
    entries: Vec<(ClockEntry, u64)>,
    ids: Vec<ClockId>,
    log: Log,
    hook: Option<Hook>,
}

impl Peripheral for Rec {
    fn name(&self) -> &str {
        self.name
    }

    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        for &(entry, token) in &self.entries {
            self.ids.push(ctx.clock_add(entry, token));
        }
    }

    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
        self.log.borrow_mut().push(Fired { who: self.name, token, now: ctx.now(), scheduled });
        if let Some(hook) = self.hook.as_mut() {
            hook(ctx, &self.ids, token);
        }
    }

    impl_peripheral_any!();
}

fn rec(h: &mut Harness, name: &'static str, log: &Log, entries: Vec<(ClockEntry, u64)>) -> PeriphId {
    h.add(Rec { name, entries, ids: Vec::new(), log: log.clone(), hook: None })
}

fn rec_hook(h: &mut Harness, name: &'static str, log: &Log, entries: Vec<(ClockEntry, u64)>, hook: Hook) -> PeriphId {
    h.add(Rec { name, entries, ids: Vec::new(), log: log.clone(), hook: Some(hook) })
}

fn asc(period: u64, hz: u64) -> ClockEntry {
    ClockEntry::new(period, hz, true, Direction::Ascending, WorkMode::Periodic)
}

fn desc(period: u64, hz: u64) -> ClockEntry {
    ClockEntry::new(period, hz, true, Direction::Descending, WorkMode::Periodic)
}

fn times(log: &Log) -> Vec<Time> {
    log.borrow().iter().map(|f| f.now).collect()
}

// ---- reference vectors ---------------------------------------------------------------------------

struct Vector {
    run_ns: u64,
    isr_entries: usize,
    /// A prefix of the ISR entry indices (the export keeps at most 60).
    entry_indices: Vec<u64>,
    delta_histogram: Vec<(u64, u64)>,
    period_ceil_ns: u64,
}

fn vector(name: &str) -> Option<Vector> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/renode-micro-vectors.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let json = Json::parse(&text).expect("vectors.json parses");
    let program = json.get("programs")?.get(name).unwrap_or_else(|| panic!("program {name} in vectors.json"));
    let expected = program.get("expected").expect("expected block");
    let numbers = |value: &Json| -> Vec<u64> { value.as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect() };
    Some(Vector {
        run_ns: program.get("runVirtualNs").and_then(|v| v.as_u64()).expect("runVirtualNs"),
        isr_entries: expected.get("isrEntries").and_then(|v| v.as_u64()).expect("isrEntries") as usize,
        entry_indices: numbers(expected.get("entryIndices").expect("entryIndices")),
        delta_histogram: expected
            .get("deltaHistogram")
            .and_then(|v| v.as_array())
            .expect("deltaHistogram")
            .iter()
            .map(|pair| {
                let pair = numbers(pair);
                (pair[0], pair[1])
            })
            .collect(),
        period_ceil_ns: expected.get("periodCeilNs").and_then(|v| v.as_u64()).expect("periodCeilNs"),
    })
}

/// ISR entry index of a limit at `time` ns: the first 10 ns instruction boundary at or after it.
fn entry_index(time: Time) -> u64 {
    time.div_ceil(TICKS_PER_INSTRUCTION)
}

fn histogram(indices: &[u64]) -> Vec<(u64, u64)> {
    let mut hist: Vec<(u64, u64)> = Vec::new();
    for pair in indices.windows(2) {
        let delta = pair[1] - pair[0];
        match hist.iter_mut().find(|(d, _)| *d == delta) {
            Some((_, n)) => *n += 1,
            None => hist.push((delta, 1)),
        }
    }
    hist.sort();
    hist
}

fn check_vector(name: &str, entry: ClockEntry) {
    let Some(v) = vector(name) else { return };
    assert_eq!(entry.ns_to_limit(), v.period_ceil_ns, "{name}: ceil period");
    // The pure entry arithmetic.
    let mut local = LocalClock::new(entry, 0);
    let mut limits = Vec::new();
    local.run_until(v.run_ns, |t| limits.push(t));
    let indices: Vec<u64> = limits.iter().map(|&t| entry_index(t)).collect();
    assert_eq!(indices.len(), v.isr_entries, "{name}: number of limits in the run");
    assert_eq!(indices[..v.entry_indices.len()], v.entry_indices[..], "{name}: LocalClock ISR entry indices");
    let mut expected_hist = v.delta_histogram.clone();
    expected_hist.sort();
    assert_eq!(histogram(&indices), expected_hist, "{name}: delta histogram");
    // The same entry in the machine registry fires at the same times, each with its own time as ctx.now().
    let mut h = Harness::new();
    let l = log();
    rec(&mut h, "dev", &l, vec![(entry, 1)]);
    h.advance_to(v.run_ns);
    assert_eq!(times(&l), limits, "{name}: registry limit times");
    assert!(l.borrow().iter().all(|f| f.now == f.scheduled));
}

#[test]
fn systick_reload_79999_matches_the_renode_vector() {
    // SysTick: RELOAD 79999 at 80 MHz, descending, periodic.
    check_vector("systick-reload79999", desc(79_999, 80_000_000));
}

#[test]
fn tim6_arr_1001_psc_0_matches_the_renode_vector() {
    check_vector("tim6-ceil-arr1001-psc0", asc(1001, 80_000_000));
}

#[test]
fn tim6_arr_1001_psc_2_matches_the_renode_vector() {
    // LimitTimer: effective frequency 80 MHz / 3 by integer division.
    check_vector("tim6-ceil-arr1001-psc2", asc(1001, 80_000_000 / 3));
}

#[test]
fn periods_of_section_3_4() {
    // TIM6 ARR=999 at 1 MHz.
    assert_eq!(asc(999, 1_000_000).ns_to_limit(), 999_000);
    // ManagedThread frequencies (period 1 at f).
    assert_eq!(asc(1, 10_000).ns_to_limit(), 100_000);
    assert_eq!(asc(1, 1_000).ns_to_limit(), 1_000_000);
    assert_eq!(asc(1, 120).ns_to_limit(), 8_333_334);
    // Through the machine: 120 Hz fires every ceil(1e9/120) ns and the dropped fraction is not remembered.
    let mut h = Harness::new();
    let l = log();
    rec(&mut h, "dev", &l, vec![(asc(1, 120), 1)]);
    h.advance_to(50_000_002);
    assert_eq!(times(&l), [8_333_334, 16_666_668, 25_000_002, 33_333_336, 41_666_670]);
    h.advance_to(50_000_004);
    assert_eq!(times(&l).last(), Some(&50_000_004));
}

// ---- machine semantics ------------------------------------------------------------------------------

#[test]
fn simultaneous_limits_update_first_then_run_in_creation_order() {
    let mut h = Harness::new();
    let l = log();
    let seen: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
    let seen_hook = seen.clone();
    // Both entries expire at 10 000 ns; a third at 20 000. The handler of the first reads the second.
    let hook: Hook = Box::new(move |ctx, ids, token| {
        if token == 1 {
            let second = ctx.clock_entry(ids[1]);
            seen_hook.borrow_mut().push((second.value(), second.residuum().0));
        }
    });
    rec_hook(&mut h, "dev", &l, vec![(asc(10, 1_000_000), 1), (asc(5, 500_000), 2), (asc(20, 1_000_000), 3)], hook);
    h.advance_to(10_000);
    let order: Vec<u64> = l.borrow().iter().map(|f| f.token).collect();
    assert_eq!(order, [1, 2], "creation order");
    assert_eq!(*seen.borrow(), [(0, 0)], "the second entry was already updated (reset) when the first handler ran");
    h.advance_to(20_000);
    assert_eq!(l.borrow().iter().map(|f| f.token).collect::<Vec<_>>(), [1, 2, 1, 2, 3]);
}

#[test]
fn creation_order_follows_registration_order_across_peripherals() {
    let mut h = Harness::new();
    let l = log();
    rec(&mut h, "first", &l, vec![(asc(1, 1_000_000), 1)]);
    rec(&mut h, "second", &l, vec![(asc(1, 1_000_000), 1)]);
    rec(&mut h, "third", &l, vec![(asc(1, 1_000_000), 1)]);
    h.advance_to(2_000);
    let who: Vec<&str> = l.borrow().iter().map(|f| f.who).collect();
    assert_eq!(who, ["first", "second", "third", "first", "second", "third"]);
}

#[test]
fn clock_events_run_before_ordinary_events_of_the_same_instant() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(1, 1_000_000), 1)]);
    h.with::<Rec, _>(id, |_r, ctx| {
        ctx.schedule_at(1_000, 99);
    });
    h.advance_to(1_000);
    let tokens: Vec<u64> = l.borrow().iter().map(|f| f.token).collect();
    assert_eq!(tokens, [1, 99]);
}

#[test]
fn value_is_exact_at_any_clock_time_without_events() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(1_000_000, 1_000_000), 1)]);
    let value = |h: &mut Harness| h.with::<Rec, _>(id, |r, ctx| ctx.clock_entry(r.ids[0]).value());
    h.advance_to(2_500);
    assert_eq!(value(&mut h), 2);
    h.advance_to(2_999);
    assert_eq!(value(&mut h), 2);
    h.advance_to(3_000);
    assert_eq!(value(&mut h), 3);
    assert!(l.borrow().is_empty());
}

#[test]
fn frequency_changes_clear_the_residuum_value_writes_keep_it() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(1000, 3_000_000), 1)]);
    h.advance_to(100); // 0.3 tick
    let state = |h: &mut Harness| h.with::<Rec, _>(id, |r, ctx| ctx.clock_entry(r.ids[0]));
    assert_eq!((state(&mut h).value(), state(&mut h).residuum()), (0, (3, 10)));
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_value(7)));
    assert_eq!((state(&mut h).value(), state(&mut h).residuum()), (7, (3, 10)), "Value write keeps the residuum");
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_period(500)));
    assert_eq!(state(&mut h).residuum(), (3, 10), "period change keeps it");
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_frequency(3_000_000)));
    assert_eq!(state(&mut h).residuum(), (0, 1), "frequency change clears it");
    assert_eq!(state(&mut h).value(), 7);
}

#[test]
fn reconfiguration_reschedules_the_limit_event() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(100, 1_000_000), 1)]); // 100 us
    h.advance_to(50_000);
    assert_eq!(h.next_event_time(), Some(100_000));
    // Shorten the period to 80 ticks: 30 more us.
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_period(80)));
    assert_eq!(h.next_event_time(), Some(80_000));
    // A period below the current value reaches the limit at once; the event is delivered right after the call.
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_period(40)));
    assert_eq!(times(&l), [50_000]);
    assert_eq!(h.next_event_time(), Some(50_000 + 40_000), "restarted from the current time with no overshoot");
    // Disabling cancels the event, enabling restarts counting with the kept value.
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_enabled(false)));
    assert_eq!(h.next_event_time(), None);
    h.advance_to(70_000);
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_exchange(r.ids[0], |e| e.with_enabled(true)));
    assert_eq!(h.next_event_time(), Some(70_000 + 40_000));
}

#[test]
fn one_shot_entries_disable_themselves() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(ClockEntry::new(5, 1_000_000, true, Direction::Ascending, WorkMode::OneShot), 1)]);
    h.advance_to(100_000);
    assert_eq!(times(&l), [5_000]);
    assert_eq!(h.next_event_time(), None);
    let enabled = h.with::<Rec, _>(id, |r, ctx| ctx.clock_entry(r.ids[0]).enabled());
    assert!(!enabled);
}

#[test]
fn zero_time_limits_are_delivered_after_the_method_returns() {
    struct Dev {
        notes: Notes,
        id: ClockId,
    }
    impl Peripheral for Dev {
        fn name(&self) -> &str {
            "dev"
        }
        fn attach(&mut self, ctx: &mut Ctx<'_>) {
            self.id = ctx.clock_add(asc(100, 1_000_000), 1);
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            self.notes.borrow_mut().push("write.begin".into());
            ctx.clock_exchange(self.id, |e| e.with_value(100)); // already at the limit
            let value = ctx.clock_entry(self.id).value();
            self.notes.borrow_mut().push(format!("write.end value={value}"));
        }
        fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
            self.notes.borrow_mut().push(format!("event {token} @{} sched {scheduled}", ctx.now()));
        }
        impl_peripheral_any!();
    }
    let n = notes();
    let mut h = Harness::new();
    h.add_mapped(0x4000_0000, 0x100, Dev { notes: n.clone(), id: ClockId::NONE });
    h.advance_to(30_000);
    h.write32(0x4000_0000, 1);
    assert_eq!(
        *n.borrow(),
        ["write.begin", "write.end value=0", "event 1 @30000 sched 30000"],
        "state reset at once (Renode ran the handler inside the setter), handler right after the write"
    );
    assert_eq!(h.next_event_time(), Some(130_000));
}

#[test]
fn a_handler_that_already_ran_at_this_instant_is_not_run_again() {
    // Renode alreadyRunHandlers: reconfiguring the entry inside its own handler so that it reaches its
    // limit at zero time consumes the limit without running the handler a second time.
    let mut h = Harness::new();
    let l = log();
    let hook: Hook = Box::new(|ctx, ids, token| {
        if token == 1 && ctx.now() == 10_000 {
            ctx.clock_exchange(ids[0], |e| e.with_value(10));
        }
    });
    rec_hook(&mut h, "dev", &l, vec![(asc(10, 1_000_000), 1)], hook);
    h.advance_to(10_000);
    assert_eq!(times(&l), [10_000], "no second run at the same instant");
    assert_eq!(h.next_event_time(), Some(20_000), "the consumed limit restarted the entry");
    h.advance_to(20_000);
    assert_eq!(times(&l), [10_000, 20_000]);
}

#[test]
fn a_zero_period_entry_is_cut_off() {
    struct Counter(u64);
    impl Peripheral for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn attach(&mut self, ctx: &mut Ctx<'_>) {
            ctx.clock_add(asc(0, 1_000_000), 1);
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        fn on_event(&mut self, _t: u64, _s: Time, _c: &mut Ctx<'_>) {
            self.0 += 1;
        }
        impl_peripheral_any!();
    }
    let mut h = Harness::new();
    let id = h.add(Counter(0));
    h.advance_to(10);
    assert!(h.core().log.contains(LogLevel::Error, "more than"), "Renode would hang here; the framework cuts the loop off");
    // The handler ran at attach (zero-time limit) and once for the first batch of the instant; further limits
    // of the same instant are consumed silently (alreadyRunHandlers).
    assert_eq!(h.get::<Counter>(id).0, 2);
    assert_eq!(h.now(), 10, "time still advanced");
}

#[test]
fn removed_entries_never_fire_and_stale_ids_are_rejected() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(1, 1_000_000), 1), (asc(2, 1_000_000), 2)]);
    assert_eq!(h.core().clock_entry_count(), 2);
    let removed = h.with::<Rec, _>(id, |r, ctx| (ctx.clock_remove(r.ids[0]), ctx.clock_remove(r.ids[0])));
    assert_eq!(removed, (true, false));
    assert_eq!(h.core().clock_entry_count(), 1);
    h.advance_to(2_000);
    assert_eq!(l.borrow().iter().map(|f| f.token).collect::<Vec<_>>(), [2]);
}

#[test]
fn replace_keeps_the_creation_order() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(asc(1, 1_000_000), 1), (asc(1, 1_000_000), 2)]);
    // Replacing the first entry in place (LimitTimer.Reset) keeps it first.
    h.with::<Rec, _>(id, |r, ctx| ctx.clock_replace(r.ids[0], asc(2, 1_000_000)));
    h.advance_to(2_000);
    assert_eq!(l.borrow().iter().map(|f| f.token).collect::<Vec<_>>(), [2, 1, 2]);
    assert_eq!(times(&l), [1_000, 2_000, 2_000]);
}

#[test]
fn descending_entries_through_the_registry() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![(desc(1000, 1_000_000), 1)]);
    h.advance_to(250_000);
    assert_eq!(h.with::<Rec, _>(id, |r, ctx| ctx.clock_entry(r.ids[0]).value()), 750);
    h.advance_to(1_000_000);
    assert_eq!(times(&l), [1_000_000]);
    assert_eq!(h.with::<Rec, _>(id, |r, ctx| ctx.clock_entry(r.ids[0]).value()), 1000);
}

// ---- schedule_action ---------------------------------------------------------------------------------

#[test]
fn schedule_action_reports_the_scheduling_time_and_removes_itself() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![]);
    h.advance_to(1_000);
    h.with::<Rec, _>(id, |_r, ctx| {
        ctx.schedule_action(500, 7);
    });
    assert!(h.take_stop_request(), "ScheduleAction requests a return");
    assert_eq!(h.core().clock_entry_count(), 1);
    h.advance_to(2_000);
    assert_eq!(*l.borrow(), [Fired { who: "dev", token: 7, now: 1_500, scheduled: 1_000 }]);
    assert_eq!(h.core().clock_entry_count(), 0, "the one-shot entry is gone");
    assert_eq!(h.next_event_time(), None);
}

#[test]
fn schedule_action_with_zero_delay_fires_right_after_the_call() {
    let mut h = Harness::new();
    let l = log();
    let id = rec(&mut h, "dev", &l, vec![]);
    h.advance_to(1_000);
    h.with::<Rec, _>(id, |_r, ctx| {
        ctx.schedule_action(0, 1);
    });
    assert_eq!(*l.borrow(), [Fired { who: "dev", token: 1, now: 1_000, scheduled: 1_000 }]);
    assert_eq!(h.core().clock_entry_count(), 0);
}

#[test]
fn schedule_action_syncs_to_the_exact_cpu_time_first() {
    struct Dev;
    impl Peripheral for Dev {
        fn name(&self) -> &str {
            "dev"
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            ctx.schedule_action(100, 5);
        }
        impl_peripheral_any!();
    }
    let mut h = Harness::new();
    let l = log();
    h.add(Rec { name: "other", entries: vec![(asc(1, 1_000_000), 1)], ids: Vec::new(), log: l.clone(), hook: None });
    h.add_mapped(0x4000_0000, 0x100, Dev);
    // A chunk from 0: the instruction at 1 300 ns writes the register. Clock time is still 0, so the other
    // entry's 1 000 ns limit has not fired - the sync fires it first, then the action is measured from 1 300.
    h.cpu_write32(0x4000_0000, 0, 1_300);
    assert_eq!(times(&l), [1_000], "the sync ran the 1000 ns limit before the action was scheduled");
    assert_eq!(h.now(), 1_300);
    assert_eq!(h.next_event_time(), Some(1_400), "the action fires 100 ns after the exact instruction time");
    assert_eq!(h.pending_events().len(), 2);
}

// ---- sync registers and clock lag --------------------------------------------------------------------

/// Registers: 0 = plain, 4 = declared sync-read, 8 = declared sync-write, 0xC = reads `ctx.now()` after
/// an explicit `ctx.sync_time()`. Every register read returns `ctx.now()`; every write stores it.
struct SyncDev {
    last_write_now: Time,
}

impl Peripheral for SyncDev {
    fn name(&self) -> &str {
        "sync"
    }
    fn sync_registers(&self) -> Vec<SyncRegister> {
        vec![SyncRegister::read(4), SyncRegister::write(8)]
    }
    fn read(&mut self, offset: u32, _w: Width, ctx: &mut Ctx<'_>) -> u32 {
        if offset == 0xC {
            ctx.sync_time();
        }
        ctx.now() as u32
    }
    fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
        self.last_write_now = ctx.now();
    }
    impl_peripheral_any!();
}

#[test]
fn sync_registers_advance_the_clock_to_the_exact_instruction_time() {
    let mut h = Harness::new();
    let id = h.add_mapped(0x4000_0000, 0x100, SyncDev { last_write_now: 0 });
    // (Times stay below 256 because the byte read below masks the returned clock time.)
    // Within a chunk that started at clock time 0: a plain register sees the lagging clock time.
    assert_eq!(h.cpu_read32(0x4000_0000, 70), 0);
    assert_eq!(h.now(), 0);
    // A declared read register syncs first.
    assert_eq!(h.cpu_read32(0x4000_0004, 70), 70);
    assert_eq!(h.now(), 70);
    // Any width that overlaps the register matches (byte at +3 of the 4-byte register).
    assert_eq!(h.cpu_read8(0x4000_0007, 90), 90);
    assert_eq!(h.now(), 90);
    // The declaration is per direction: reading the write-sync register does not sync...
    assert_eq!(h.cpu_read32(0x4000_0008, 120), 90);
    assert_eq!(h.now(), 90);
    // ...writing it does, and the model sees the exact time.
    h.cpu_write32(0x4000_0008, 1, 130);
    assert_eq!(h.get::<SyncDev>(id).last_write_now, 130);
    // A register that is not declared does not sync on write either.
    h.cpu_write32(0x4000_0000, 1, 140);
    assert_eq!(h.get::<SyncDev>(id).last_write_now, 130);
    // Conditional syncs: ctx.sync_time() inside the model.
    assert_eq!(h.cpu_read32(0x4000_000C, 150), 150);
    // Host accesses are not CPU accesses and never sync.
    assert_eq!(h.read32(0x4000_0004), 150);
    assert_eq!(h.core().stats.syncs, 4);
    // sync_time outside a CPU access is a no-op returning now().
    let t = h.with::<SyncDev, _>(id, |_d, ctx| ctx.sync_time());
    assert_eq!(t, 150);
}

#[test]
fn syncs_fire_events_in_order_before_the_access() {
    struct Reader {
        notes: Notes,
    }
    impl Peripheral for Reader {
        fn name(&self) -> &str {
            "reader"
        }
        fn sync_registers(&self) -> Vec<SyncRegister> {
            vec![SyncRegister::read(0)]
        }
        fn read(&mut self, _o: u32, _w: Width, ctx: &mut Ctx<'_>) -> u32 {
            self.notes.borrow_mut().push(format!("read @{}", ctx.now()));
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        impl_peripheral_any!();
    }
    struct Ticker {
        notes: Notes,
    }
    impl Peripheral for Ticker {
        fn name(&self) -> &str {
            "ticker"
        }
        fn attach(&mut self, ctx: &mut Ctx<'_>) {
            ctx.clock_add(asc(1, 1_000_000), 1);
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        fn on_event(&mut self, _t: u64, _s: Time, ctx: &mut Ctx<'_>) {
            self.notes.borrow_mut().push(format!("tick @{}", ctx.now()));
        }
        impl_peripheral_any!();
    }
    let n = notes();
    let mut h = Harness::new();
    h.add(Ticker { notes: n.clone() });
    h.add_mapped(0x4000_0000, 0x100, Reader { notes: n.clone() });
    h.cpu_read32(0x4000_0000, 2_500);
    assert_eq!(*n.borrow(), ["tick @1000", "tick @2000", "read @2500"]);
}

#[test]
fn events_for_the_running_peripheral_are_deferred_until_it_returns() {
    struct Dev {
        notes: Notes,
    }
    impl Peripheral for Dev {
        fn name(&self) -> &str {
            "dev"
        }
        fn attach(&mut self, ctx: &mut Ctx<'_>) {
            ctx.clock_add(asc(1, 1_000_000), 1);
        }
        fn read(&mut self, _o: u32, _w: Width, ctx: &mut Ctx<'_>) -> u32 {
            self.notes.borrow_mut().push(format!("read.begin @{}", ctx.now()));
            let now = ctx.sync_time(); // fires this peripheral's own 1000 ns limit: deferred
            self.notes.borrow_mut().push(format!("read.end @{now}"));
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
        fn on_event(&mut self, token: u64, scheduled: Time, ctx: &mut Ctx<'_>) {
            self.notes.borrow_mut().push(format!("event {token} sched {scheduled} @{}", ctx.now()));
        }
        impl_peripheral_any!();
    }
    let n = notes();
    let mut h = Harness::new();
    h.add_mapped(0x4000_0000, 0x100, Dev { notes: n.clone() });
    h.cpu_read32(0x4000_0000, 1_500);
    assert_eq!(
        *n.borrow(),
        ["read.begin @0", "read.end @1500", "event 1 sched 1000 @1500"],
        "the owner's limit waits for its method to return; it still carries its own scheduled time"
    );
}

#[test]
fn timer_enable_lag_the_entry_starts_at_the_clock_time_not_the_instruction_time() {
    // renode-semantics "timer-enable-lag": an enable write at instruction time 110 ns inside a chunk that
    // started at clock time 0 starts the entry at 0, so the first limit is one period after 0.
    struct Timer {
        id: ClockId,
    }
    impl Peripheral for Timer {
        fn name(&self) -> &str {
            "tim"
        }
        fn attach(&mut self, ctx: &mut Ctx<'_>) {
            self.id = ctx.clock_add(asc(1001, 80_000_000).with_enabled(false), 1);
        }
        fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
            0
        }
        fn write(&mut self, _o: u32, _w: Width, _v: u32, ctx: &mut Ctx<'_>) {
            ctx.clock_exchange(self.id, |e| e.with_enabled(true));
        }
        impl_peripheral_any!();
    }
    let mut h = Harness::new();
    h.add_mapped(0x4000_0000, 0x100, Timer { id: ClockId::NONE });
    h.cpu_write32(0x4000_0000, 1, 110);
    assert_eq!(h.next_event_time(), Some(12_513), "period counted from clock time 0, not from 110 ns");
}

// ---- LimitTimer ----------------------------------------------------------------------------------------

struct TimerDev {
    timer: LimitTimer,
    fired: Rc<RefCell<Vec<(Time, bool)>>>,
}

impl TimerDev {
    fn new(cfg: LimitTimerConfig, fired: &Rc<RefCell<Vec<(Time, bool)>>>) -> Self {
        Self { timer: LimitTimer::new(cfg, 1), fired: fired.clone() }
    }
}

impl Peripheral for TimerDev {
    fn name(&self) -> &str {
        "timer"
    }
    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.attach(ctx);
    }
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.timer.reset(ctx);
    }
    fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
        0
    }
    fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
    fn on_event(&mut self, token: u64, _s: Time, ctx: &mut Ctx<'_>) {
        if token == 1 {
            let alarm = self.timer.on_limit_reached();
            self.fired.borrow_mut().push((ctx.now(), alarm));
        }
    }
    impl_peripheral_any!();
}

fn limit_timer(h: &mut Harness, cfg: LimitTimerConfig) -> (PeriphId, Rc<RefCell<Vec<(Time, bool)>>>) {
    let fired = Rc::new(RefCell::new(Vec::new()));
    let id = h.add(TimerDev::new(cfg, &fired));
    (id, fired)
}

#[test]
fn limit_timer_defaults_follow_renode() {
    let cfg = LimitTimerConfig::new(1_000_000);
    assert_eq!(
        (cfg.limit, cfg.direction, cfg.enabled, cfg.mode, cfg.event_enabled, cfg.auto_update, cfg.divider),
        (u64::MAX, Direction::Descending, false, WorkMode::Periodic, false, false, 1)
    );
    let mut h = Harness::new();
    let (id, fired) = limit_timer(&mut h, cfg);
    h.advance_to(1_000_000);
    assert!(fired.borrow().is_empty());
    assert_eq!(h.next_event_time(), None, "disabled timers own no event");
    let t = h.get::<TimerDev>(id);
    assert_eq!(h.core().clock_entry_count(), 1);
    assert!(!t.timer.raw_interrupt() && !t.timer.event_enabled() && !t.timer.interrupt());
}

#[test]
fn limit_timer_counts_fires_and_reports_the_event_flag() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 10, direction: Direction::Ascending, enabled: true, ..LimitTimerConfig::new(1_000_000) };
    let (id, fired) = limit_timer(&mut h, cfg);
    h.advance_to(25_000);
    // Event disabled: raw_interrupt is set but the owner's LimitReached handler is not invoked.
    assert_eq!(*fired.borrow(), [(10_000, false), (20_000, false)]);
    let t = h.get::<TimerDev>(id);
    assert!(t.timer.raw_interrupt() && !t.timer.interrupt());
    h.with::<TimerDev, _>(id, |t, _ctx| {
        t.timer.set_event_enabled(true);
        t.timer.clear_interrupt();
    });
    h.advance_to(35_000);
    assert_eq!(fired.borrow().last(), Some(&(30_000, true)));
    assert!(h.get::<TimerDev>(id).timer.interrupt());
}

#[test]
fn limit_timer_divider_uses_integer_division_and_clears_the_residuum() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 1001, direction: Direction::Ascending, enabled: true, divider: 3, ..LimitTimerConfig::new(80_000_000) };
    let (id, fired) = limit_timer(&mut h, cfg);
    h.advance_to(80_000);
    assert_eq!(fired.borrow().iter().map(|f| f.0).collect::<Vec<_>>(), [37_538, 75_076]);
    // Changing the divider to the same value is a no-op (no return request); another value re-times the entry.
    h.take_stop_request();
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_divider(ctx, 3));
    assert!(!h.take_stop_request());
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_divider(ctx, 1));
    assert!(h.take_stop_request());
    assert_eq!(h.get::<TimerDev>(id).timer.divider(), 1);
    // 4 924 ns after the last limit the value is floor(4924 * 26 666 666 / 1e9) = 131 ticks; the residuum is
    // cleared and the remaining 870 ticks run at 80 MHz: 10 875 ns.
    assert_eq!(h.with::<TimerDev, _>(id, |t, ctx| t.timer.value(ctx)), 131);
    assert_eq!(h.next_event_time(), Some(80_000 + 10_875));
}

#[test]
fn limit_timer_setters_request_a_return_except_mode_and_flags() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 100, direction: Direction::Ascending, enabled: true, ..LimitTimerConfig::new(1_000_000) };
    let (id, _) = limit_timer(&mut h, cfg);
    h.take_stop_request();
    let mut requested = Vec::new();
    macro_rules! check {
        ($name:expr, $body:expr) => {{
            h.with::<TimerDev, _>(id, $body);
            requested.push(($name, h.take_stop_request()));
        }};
    }
    check!("enabled", |t, ctx| t.timer.set_enabled(ctx, true));
    check!("value", |t, ctx| t.timer.set_value(ctx, 5));
    check!("limit", |t, ctx| t.timer.set_limit(ctx, 100));
    check!("frequency", |t, ctx| t.timer.set_frequency(ctx, 2_000_000));
    check!("divider", |t, ctx| t.timer.set_divider(ctx, 2));
    check!("direction", |t, ctx| t.timer.set_direction(ctx, Direction::Ascending));
    check!("reset_value", |t, ctx| t.timer.reset_value(ctx));
    check!("mode", |t, ctx| t.timer.set_mode(ctx, WorkMode::Periodic));
    check!("event_enabled", |t, _ctx| t.timer.set_event_enabled(true));
    check!("auto_update", |t, _ctx| t.timer.set_auto_update(true));
    check!("clear_interrupt", |t, _ctx| t.timer.clear_interrupt());
    assert_eq!(
        requested,
        [
            ("enabled", true),
            ("value", true),
            ("limit", true),
            ("frequency", true),
            ("divider", true),
            ("direction", true),
            ("reset_value", true),
            ("mode", false),
            ("event_enabled", false),
            ("auto_update", false),
            ("clear_interrupt", false),
        ]
    );
}

#[test]
fn limit_timer_limit_with_auto_update_resets_the_value() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 1000, direction: Direction::Ascending, enabled: true, auto_update: true, ..LimitTimerConfig::new(1_000_000) };
    let (id, _) = limit_timer(&mut h, cfg);
    h.advance_to(300_000);
    let (value, limit) = h.with::<TimerDev, _>(id, |t, ctx| t.timer.value_and_limit(ctx));
    assert_eq!((value, limit), (300, 1000));
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_limit(ctx, 500));
    assert_eq!(h.with::<TimerDev, _>(id, |t, ctx| t.timer.value(ctx)), 0, "AutoUpdate resets the value");
    assert_eq!(h.next_event_time(), Some(300_000 + 500_000));
    // Without AutoUpdate the value is kept, even above the new limit (the timer then fires at once).
    h.with::<TimerDev, _>(id, |t, _ctx| t.timer.set_auto_update(false));
    h.advance_to(400_000);
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_limit(ctx, 50));
    assert_eq!(h.get::<TimerDev>(id).fired.borrow().last().map(|f| f.0), Some(400_000), "value 100 >= limit 50: limit reached now");
}

#[test]
fn limit_timer_value_is_bounded_by_the_initial_limit() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 100, direction: Direction::Ascending, enabled: true, ..LimitTimerConfig::new(1_000_000) };
    let (id, _) = limit_timer(&mut h, cfg);
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_value(ctx, 101));
    assert!(h.core().log.contains(LogLevel::Error, "cannot be larger"));
    assert_eq!(h.with::<TimerDev, _>(id, |t, ctx| t.timer.value(ctx)), 0, "the write was ignored");
    h.with::<TimerDev, _>(id, |t, ctx| t.timer.set_value(ctx, 100));
    assert_eq!(h.get::<TimerDev>(id).fired.borrow().len(), 1, "value == limit reaches it immediately");
}

#[test]
fn limit_timer_increment_and_decrement_wrap_like_renode() {
    let mut h = Harness::new();
    let cfg = LimitTimerConfig { limit: 10, direction: Direction::Ascending, ..LimitTimerConfig::new(1_000_000) };
    let (id, _) = limit_timer(&mut h, cfg);
    h.with::<TimerDev, _>(id, |t, ctx| {
        assert_eq!(t.timer.increment(ctx, 4), 0);
        assert_eq!(t.timer.value(ctx), 4);
        assert_eq!(t.timer.increment(ctx, 27), 3, "31 = 3 * 10 + 1");
        assert_eq!(t.timer.value(ctx), 1);
        assert_eq!(t.timer.decrement(ctx, 1), 0);
        assert_eq!(t.timer.value(ctx), 0);
        assert_eq!(t.timer.decrement(ctx, 1), 1, "wraps below zero once");
        assert_eq!(t.timer.value(ctx), 9);
    });
}

#[test]
fn limit_timer_reset_restores_the_initial_configuration_in_place() {
    let mut h = Harness::new();
    let l = log();
    // Timer first, then another entry: the timer's creation order must survive a reset.
    let cfg = LimitTimerConfig { limit: 10, direction: Direction::Ascending, enabled: true, event_enabled: true, ..LimitTimerConfig::new(1_000_000) };
    let (id, fired) = limit_timer(&mut h, cfg);
    let later = rec(&mut h, "later", &l, vec![(asc(15, 1_000_000), 9)]);
    h.advance_to(5_000);
    h.with::<TimerDev, _>(id, |t, ctx| {
        t.timer.set_limit(ctx, 30);
        t.timer.set_frequency(ctx, 2_000_000);
        t.timer.set_event_enabled(false);
    });
    h.core_mut().reset_all();
    let t = h.get::<TimerDev>(id);
    assert_eq!((t.timer.frequency(), t.timer.divider(), t.timer.event_enabled(), t.timer.raw_interrupt()), (1_000_000, 1, true, false));
    // The timer restarted at 5 000 (limit 10 again): both entries expire at 15 000 and the timer, created first,
    // is still ahead of the other entry in the queue.
    let pending: Vec<(Time, PeriphId)> = h.pending_events().iter().map(|e| (e.0, e.1)).collect();
    assert_eq!(pending, [(15_000, id), (15_000, later)]);
    h.advance_to(15_000);
    assert_eq!(fired.borrow().iter().map(|f| f.0).collect::<Vec<_>>(), [15_000]);
    assert_eq!(times(&l), [15_000]);
    assert_eq!(h.core().clock_entry_count(), 2, "reset replaced the entry, it did not add one");
}

#[test]
#[should_panic(expected = "Limit must be greater than 0")]
fn limit_timer_rejects_a_zero_limit() {
    LimitTimer::new(LimitTimerConfig { limit: 0, ..LimitTimerConfig::new(1) }, 0);
}

// ---- ManagedThread ---------------------------------------------------------------------------------------

const BODY: u64 = 1;
const DELAYED: u64 = 2;

struct ThreadDev {
    thread: ManagedThread,
    fired: Rc<RefCell<Vec<(&'static str, Time)>>>,
}

impl Peripheral for ThreadDev {
    fn name(&self) -> &str {
        "thread"
    }
    fn attach(&mut self, ctx: &mut Ctx<'_>) {
        self.thread.attach(ctx);
    }
    fn read(&mut self, _o: u32, _w: Width, _c: &mut Ctx<'_>) -> u32 {
        0
    }
    fn write(&mut self, _o: u32, _w: Width, _v: u32, _c: &mut Ctx<'_>) {}
    fn on_event(&mut self, token: u64, _s: Time, ctx: &mut Ctx<'_>) {
        match token {
            BODY => self.fired.borrow_mut().push(("body", ctx.now())),
            DELAYED => {
                self.thread.start(ctx);
                self.fired.borrow_mut().push(("delayed-start", ctx.now()));
            }
            _ => {}
        }
    }
    impl_peripheral_any!();
}

fn thread(h: &mut Harness, thread: ManagedThread) -> (PeriphId, Rc<RefCell<Vec<(&'static str, Time)>>>) {
    let fired = Rc::new(RefCell::new(Vec::new()));
    let id = h.add(ThreadDev { thread, fired: fired.clone() });
    (id, fired)
}

#[test]
fn managed_thread_is_created_stopped_and_first_fires_one_period_after_start() {
    let mut h = Harness::new();
    let (id, fired) = thread(&mut h, ManagedThread::new(1_000, BODY));
    h.advance_to(3_300_000);
    assert!(fired.borrow().is_empty(), "created disabled");
    h.take_stop_request();
    h.with::<ThreadDev, _>(id, |t, ctx| t.thread.start(ctx));
    assert!(!h.take_stop_request(), "Start() does not request a return");
    h.advance_to(6_000_000);
    let times: Vec<Time> = fired.borrow().iter().map(|f| f.1).collect();
    assert_eq!(times, [4_300_000, 5_300_000], "first firing ceil(1e9/f) after Start");
}

#[test]
fn managed_thread_stop_keeps_the_partial_period_and_restart_resets_it() {
    let mut h = Harness::new();
    let (id, fired) = thread(&mut h, ManagedThread::new(1_000, BODY));
    h.with::<ThreadDev, _>(id, |t, ctx| t.thread.start(ctx));
    h.advance_to(400_000);
    h.with::<ThreadDev, _>(id, |t, ctx| t.thread.stop(ctx));
    assert_eq!(h.next_event_time(), None);
    h.advance_to(5_000_000);
    h.with::<ThreadDev, _>(id, |t, ctx| t.thread.start(ctx));
    h.advance_to(6_000_000);
    assert_eq!(fired.borrow().iter().map(|f| f.1).collect::<Vec<_>>(), [5_600_000], "resumes with 600 us left of the period");
    // 400 us after the 5.6 ms limit: residuum 2/5. Restart resets Value but keeps the residuum, so the next
    // limit is 600 us after the restart.
    h.with::<ThreadDev, _>(id, |t, ctx| {
        t.thread.stop(ctx);
        t.thread.restart(ctx);
    });
    h.advance_to(7_000_000);
    assert_eq!(fired.borrow().last().map(|f| f.1), Some(6_600_000));
}

#[test]
fn managed_thread_frequency_and_period_setters() {
    let mut h = Harness::new();
    let (id, fired) = thread(&mut h, ManagedThread::new(1_000, BODY));
    h.with::<ThreadDev, _>(id, |t, ctx| {
        assert_eq!(t.thread.frequency(ctx), 1_000);
        assert_eq!(t.thread.period(ctx), 1_000_000);
        t.thread.set_frequency(ctx, 10_000);
        assert_eq!(t.thread.period(ctx), 100_000);
        t.thread.start(ctx);
    });
    h.advance_to(250_000);
    assert_eq!(fired.borrow().iter().map(|f| f.1).collect::<Vec<_>>(), [100_000, 200_000]);
    h.with::<ThreadDev, _>(id, |t, ctx| {
        t.thread.set_period(ctx, 70_000);
        assert_eq!(t.thread.period(ctx), 70_000);
        assert_eq!(t.thread.frequency(ctx), TICKS_PER_SECOND);
        assert!(t.thread.enabled(ctx));
    });
    h.advance_to(400_000);
    // set_period = period 70 000 ns at 1e9 Hz: the frequency write cleared the residuum, Value stays 0.
    assert_eq!(fired.borrow().iter().skip(2).map(|f| f.1).collect::<Vec<_>>(), [320_000, 390_000]);
}

#[test]
fn managed_thread_with_period_and_start_delayed() {
    let mut h = Harness::new();
    let (id, fired) = thread(&mut h, ManagedThread::with_period(250_000, BODY));
    h.advance_to(1_000);
    h.with::<ThreadDev, _>(id, |t, ctx| {
        t.thread.start_delayed(ctx, 10_000, DELAYED);
    });
    h.advance_to(1_000_000);
    let all = fired.borrow().clone();
    assert_eq!(all[0], ("delayed-start", 11_000));
    assert_eq!(all[1..].iter().map(|f| f.1).collect::<Vec<_>>(), [261_000, 511_000, 761_000]);
}

#[test]
fn managed_thread_dispose_removes_the_entry() {
    let mut h = Harness::new();
    let (id, fired) = thread(&mut h, ManagedThread::new(1_000_000, BODY));
    h.with::<ThreadDev, _>(id, |t, ctx| {
        t.thread.start(ctx);
        t.thread.dispose(ctx);
    });
    assert_eq!(h.core().clock_entry_count(), 0);
    h.advance_to(10_000);
    assert!(fired.borrow().is_empty());
}

#[test]
#[should_panic(expected = "Frequency must be higher than zero")]
fn managed_thread_rejects_a_zero_frequency() {
    ManagedThread::new(0, 0);
}
