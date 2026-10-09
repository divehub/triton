// Ported from emulation/models/NGCCANLink.cs of the analysis workspace (Renode 1.17.0 external, see that file for its
// license), which in turn follows Renode's `Antmicro.Renode.Core.CAN` types (MIT License, Copyright (c) Antmicro).

//! `NGCCANLink`: the functional CAN link between the main and handset controllers.
//!
//! The Renode external subscribes to `ICAN.FrameSent` of both `STMCAN` instances and, for every frame,
//! counts it, appends a trace line, optionally discards it (link not started, `Connected` false or the frame's
//! identifier equals `DropId`) and otherwise schedules `OnFrameReceived` on every other endpoint at the sender's
//! time stamp; Renode runs those at the synchronization point at the end of the quantum, ordered by
//! `(stamp, scheduling order)`. No bus timing, arbitration or synthetic replies exist.
//!
//! This port is a plain system-level object (not memory mapped): the system drains each board's CAN out-queue
//! after every quantum (`stm32::can::StmCan::take_tx_frames`), hands the batches to [`CanLink::route`] (or lets
//! [`CanLink::pump`] do the whole cycle through a [`CanPorts`] adapter) and applies the returned
//! [`Delivery`] list with `StmCan::deliver_frame`.
//!
//! Evidence formats reproduced exactly: [`CanLink::summary`] (`ngcCAN Summary`) and the trace
//! ([`CanLink::trace_lines`] / [`CanLink::trace_text`], what `ngcCAN SaveTrace` writes to `can-trace.tsv`).
//!
//! # Quantum-boundary protocol for the system
//!
//! 1. run both boards to the boundary (main first, then handset);
//! 2. `link.pump(&[main, handset], &mut ports)`: drains both out-queues, processes the frames in
//!    `(stamp, main-before-handset, transmit order)` order and delivers them (`StmCan::deliver_frame`);
//! 3. only then apply UI/automation inputs for the new quantum, in particular
//!    [`CanLink::set_connected`] / [`CanLink::set_drop_id`] / [`CanLink::pause`].
//!
//! Renode decides at the transmit call (the firmware's register write), so a frame sent before a control change
//! must be judged with the old setting. The link only sees a frame when it is pumped (up to one quantum later):
//! pumping first and changing the controls afterwards keeps every frame of the finished quantum on the old setting.
//! (Checked against Renode 1.17.0 in a two-machine scenario run, see the work-package report.)

use emu_core::{Time, TICKS_PER_SECOND};
use std::collections::VecDeque;
use std::fmt::Write as _;
use stm32::can::{CanFrame, TxFrame};

/// Renode trims the trace when it exceeds this many lines ...
pub const TRACE_LIMIT: usize = 100_000;
/// ... by removing this many of the oldest lines.
pub const TRACE_TRIM: usize = 1_000;

/// Handle of an attached controller (attachment order, like the Renode dictionary insertion order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(pub usize);

/// A frame the link wants delivered to `to` (call `StmCan::deliver_frame` at the quantum boundary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub to: EndpointId,
    pub frame: CanFrame,
    /// The sender's stamp (kept for diagnostics; Renode delivers at the first sync point at or after it).
    pub stamp: Time,
}

/// What [`CanLink::pump`] needs from the system: access to the controllers behind the endpoints.
pub trait CanPorts {
    /// Drains the frames the endpoint's controller emitted since the previous call (transmit order).
    fn take_tx_frames(&mut self, endpoint: EndpointId) -> Vec<TxFrame>;
    /// Hands a frame to the endpoint's controller (`OnFrameReceived`).
    fn deliver(&mut self, endpoint: EndpointId, frame: &CanFrame);
}

struct Endpoint {
    name: String,
    attached: bool,
}

struct TraceEntry {
    stamp: Time,
    endpoint: usize,
    id: u32,
    data: Vec<u8>,
    discarded: bool,
    extended: bool,
    remote: bool,
}

/// The CAN link external.
pub struct CanLink {
    endpoints: Vec<Endpoint>,
    started: bool,
    connected: bool,
    drop_id: i32,
    transmitted: u64,
    delivered: u64,
    dropped: u64,
    last_id: u32,
    trace: VecDeque<TraceEntry>,
    /// Reused by `pump` so that an idle quantum allocates nothing.
    scratch: Vec<(EndpointId, TxFrame)>,
}

impl Default for CanLink {
    fn default() -> Self {
        Self::new()
    }
}

impl CanLink {
    /// A started, connected link with `DropId = -1`. (In Renode the link starts with the emulation; every
    /// frame of the reference runs happened after that.)
    pub fn new() -> CanLink {
        CanLink {
            endpoints: Vec::new(),
            started: true,
            connected: true,
            drop_id: -1,
            transmitted: 0,
            delivered: 0,
            dropped: 0,
            last_id: 0,
            trace: VecDeque::new(),
            scratch: Vec::new(),
        }
    }

    /// `AttachTo`: registers a controller under its Renode name (`ngc-main.can1`); attaching a name that is
    /// already attached returns the existing endpoint.
    pub fn attach(&mut self, name: impl Into<String>) -> EndpointId {
        let name = name.into();
        if let Some(index) = self.endpoints.iter().position(|e| e.attached && e.name == name) {
            return EndpointId(index);
        }
        self.endpoints.push(Endpoint { name, attached: true });
        EndpointId(self.endpoints.len() - 1)
    }

    /// `DetachFrom`: the endpoint stops sending and receiving.
    pub fn detach(&mut self, endpoint: EndpointId) {
        if let Some(e) = self.endpoints.get_mut(endpoint.0) {
            e.attached = false;
        }
    }

    /// Number of attached endpoints (the `endpoints=` field of the summary).
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.iter().filter(|e| e.attached).count()
    }

    pub fn endpoint_name(&self, endpoint: EndpointId) -> Option<&str> {
        self.endpoints.get(endpoint.0).map(|e| e.name.as_str())
    }

    /// `Start()` / `Resume()`.
    pub fn resume(&mut self) {
        self.started = true;
    }

    /// `Pause()`: frames are discarded (and traced as dropped) while paused.
    pub fn pause(&mut self) {
        self.started = false;
    }

    pub fn is_paused(&self) -> bool {
        !self.started
    }

    /// `Connected` (runner control "CAN link connected").
    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// `DropId`: -1 forwards every identifier, otherwise that identifier is dropped.
    pub fn drop_id(&self) -> i32 {
        self.drop_id
    }

    pub fn set_drop_id(&mut self, drop_id: i32) {
        self.drop_id = drop_id;
    }

    pub fn transmitted(&self) -> u64 {
        self.transmitted
    }

    /// Frames scheduled for delivery, counted per receiving endpoint (`scheduled=` in the summary).
    pub fn scheduled(&self) -> u64 {
        self.delivered
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// `ngcCAN Summary`.
    pub fn summary(&self) -> String {
        format!(
            "Real firmware CAN link: endpoints={}; connected={}; transmitted={}; scheduled={}; dropped={}; lastId=0x{:X}; dropId={}",
            self.endpoint_count(),
            cs_bool(self.connected),
            self.transmitted,
            self.delivered,
            self.dropped,
            self.last_id,
            self.drop_id
        )
    }

    /// `Transmit`: one frame leaves `sender` with the sender's stamp. Returns the deliveries to apply (empty
    /// when the frame is discarded). Counters and the trace are updated exactly like the C# handler.
    pub fn transmit(&mut self, sender: EndpointId, frame: &CanFrame, stamp: Time) -> Vec<Delivery> {
        if !self.endpoints.get(sender.0).is_some_and(|e| e.attached) {
            return Vec::new();
        }
        // Renode's STMCAN reports a zero-length frame with a null payload; the link replaces it with an empty
        // array and clones the payload before it crosses time domains. A Rust payload is never null.
        let message = frame.clone();
        self.transmitted += 1;
        self.last_id = message.id;
        let discard = !self.started || !self.connected || self.drop_id == message.id as i32;
        if discard {
            self.dropped += 1;
        }
        self.trace.push_back(TraceEntry {
            stamp,
            endpoint: sender.0,
            id: message.id,
            data: message.data.clone(),
            discarded: discard,
            extended: message.extended,
            remote: message.remote,
        });
        if self.trace.len() > TRACE_LIMIT {
            self.trace.drain(..TRACE_TRIM);
        }
        if discard {
            return Vec::new();
        }
        let mut deliveries = Vec::new();
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            if endpoint.attached && index != sender.0 {
                deliveries.push(Delivery { to: EndpointId(index), frame: message.clone(), stamp });
                self.delivered += 1;
            }
        }
        deliveries
    }

    /// The quantum-boundary step: `batches` holds, per sending endpoint, the frames drained from its
    /// controller in transmit order. The frames are processed in `(stamp, batch order, position)` order -
    /// callers pass the batches in board order (main first), which is the tie-break at equal stamps.
    pub fn route(&mut self, batches: &[(EndpointId, Vec<TxFrame>)]) -> Vec<Delivery> {
        let mut order: Vec<(EndpointId, &TxFrame)> = batches.iter().flat_map(|(endpoint, frames)| frames.iter().map(move |f| (*endpoint, f))).collect();
        order.sort_by_key(|(_, frame)| frame.time); // stable
        let mut deliveries = Vec::new();
        for (endpoint, tx) in order {
            deliveries.extend(self.transmit(endpoint, &tx.frame, tx.time));
        }
        deliveries
    }

    /// One whole quantum-boundary cycle: drain `order`'s controllers through `ports`, process the frames in
    /// `(stamp, order of endpoints in `order`, transmit order)` and deliver each one right after it was
    /// traced. Returns the number of deliveries made. With no frame in flight (almost every quantum) it
    /// performs no allocation.
    pub fn pump(&mut self, order: &[EndpointId], ports: &mut dyn CanPorts) -> usize {
        let mut pending = std::mem::take(&mut self.scratch);
        pending.clear();
        for &endpoint in order {
            for tx in ports.take_tx_frames(endpoint) {
                pending.push((endpoint, tx));
            }
        }
        let mut delivered = 0;
        if !pending.is_empty() {
            pending.sort_by_key(|(_, tx)| tx.time); // stable
            for (endpoint, tx) in pending.drain(..) {
                for delivery in self.transmit(endpoint, &tx.frame, tx.time) {
                    ports.deliver(delivery.to, &delivery.frame);
                    delivered += 1;
                }
            }
        }
        self.scratch = pending;
        delivered
    }

    // ---- trace ----

    pub fn trace_len(&self) -> usize {
        self.trace.len()
    }

    pub fn clear_trace(&mut self) {
        self.trace.clear();
    }

    fn format_entry(&self, entry: &TraceEntry) -> String {
        let mut line = String::with_capacity(96);
        let _ = write!(
            line,
            "{}\t{}\t0x{:03X}\t",
            format_time_interval(entry.stamp),
            self.endpoints[entry.endpoint].name,
            entry.id
        );
        for byte in &entry.data {
            let _ = write!(line, "{byte:02X}");
        }
        let _ = write!(
            line,
            "\t{}\textended={};remote={}",
            if entry.discarded { "dropped" } else { "scheduled" },
            cs_bool(entry.extended),
            cs_bool(entry.remote)
        );
        line
    }

    /// Trace lines (`stamp \t sender \t 0xID \t payload \t outcome \t extended=..;remote=..`).
    pub fn trace_lines(&self) -> Vec<String> {
        self.trace.iter().map(|entry| self.format_entry(entry)).collect()
    }

    /// The content `ngcCAN SaveTrace` writes (`File.WriteAllLines`: every line ends in `\n`).
    pub fn trace_text(&self) -> String {
        let mut text = String::new();
        for entry in &self.trace {
            text.push_str(&self.format_entry(entry));
            text.push('\n');
        }
        text
    }
}

/// C# `bool.ToString()`.
fn cs_bool(value: bool) -> &'static str {
    if value {
        "True"
    } else {
        "False"
    }
}

/// Renode `TimeInterval.ToString()`: `hh:mm:ss.nnnnnnnnn` of the elapsed virtual time.
pub fn format_time_interval(time: Time) -> String {
    let nanoseconds = (u128::from(time) * 1_000_000_000 / u128::from(TICKS_PER_SECOND)) as u64;
    let decimals = nanoseconds % 1_000_000_000;
    let seconds = nanoseconds / 1_000_000_000;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    format!("{hours:02}:{minutes:02}:{:02}.{decimals:09}", seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{from_micros, PeriphId};
    use stm32::can::{offset, StmCan, LINE_RX0};

    const SYSBUS_CAN: u32 = 0x4000_6400;

    /// `ns` nanoseconds as a virtual time (rounded up, so converting back yields `ns` for any time base
    /// finer than 1 ns).
    fn ns(nanoseconds: u64) -> Time {
        ((u128::from(nanoseconds) * u128::from(TICKS_PER_SECOND) + 999_999_999) / 1_000_000_000) as Time
    }

    /// First lines of the CAN trace of a Renode 1.17.0 dual handset-wake run recorded on 2026-10-08 (the
    /// whole trace, ~50 frames, replayed byte for byte when the model was ported).
    const REFERENCE_HEAD: &str = "\
00:00:01.008884160\tngc-main.can1\t0x154\tFF\tscheduled\textended=False;remote=False
00:00:01.376401720\tngc-handset.can1\t0x173\t\tscheduled\textended=False;remote=False
00:00:01.476401840\tngc-handset.can1\t0x103\t0102\tscheduled\textended=False;remote=False
00:00:01.486401840\tngc-handset.can1\t0x089\t00410302\tscheduled\textended=False;remote=False
00:00:01.618270020\tngc-main.can1\t0x118\t\tscheduled\textended=False;remote=False
00:00:01.636400000\tngc-handset.can1\t0x119\t\tscheduled\textended=False;remote=False
00:00:02.378435640\tngc-main.can1\t0x070\t01\tscheduled\textended=False;remote=False
00:00:02.388322640\tngc-main.can1\t0x068\t32\tscheduled\textended=False;remote=False
00:00:02.398284950\tngc-main.can1\t0x07B\tFF\tscheduled\textended=False;remote=False
00:00:02.408304670\tngc-main.can1\t0x081\t00\tscheduled\textended=False;remote=False
00:00:02.418284230\tngc-main.can1\t0x172\t\tscheduled\textended=False;remote=False
00:00:02.428277650\tngc-main.can1\t0x5A5\t\tscheduled\textended=False;remote=False
";

    struct ParsedLine {
        stamp_ns: u64,
        sender: String,
        id: u32,
        data: Vec<u8>,
        dropped: bool,
        extended: bool,
        remote: bool,
    }

    fn parse_trace(text: &str) -> Vec<ParsedLine> {
        text.lines()
            .map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                assert_eq!(f.len(), 6, "{line}");
                let (hms, frac) = f[0].split_once('.').unwrap();
                let parts: Vec<u64> = hms.split(':').map(|p| p.parse().unwrap()).collect();
                let stamp_ns = ((parts[0] * 60 + parts[1]) * 60 + parts[2]) * 1_000_000_000 + frac.parse::<u64>().unwrap();
                let hex = f[3];
                let data = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
                let flags = f[5].strip_prefix("extended=").unwrap();
                let (extended, remote) = flags.split_once(";remote=").unwrap();
                ParsedLine {
                    stamp_ns,
                    sender: f[1].to_string(),
                    id: u32::from_str_radix(f[2].strip_prefix("0x").unwrap(), 16).unwrap(),
                    data,
                    dropped: f[4] == "dropped",
                    extended: extended == "True",
                    remote: remote == "True",
                }
            })
            .collect()
    }

    /// Replays `text` through a link with the dual.resc attachment order and returns the link.
    fn replay(text: &str) -> CanLink {
        let mut link = CanLink::new();
        let handset = link.attach("ngc-handset.can1");
        let main = link.attach("ngc-main.can1");
        for line in parse_trace(text) {
            let sender = if line.sender == "ngc-main.can1" { main } else { handset };
            link.set_connected(!line.dropped);
            let frame = CanFrame { id: line.id, data: line.data, extended: line.extended, remote: line.remote, ..CanFrame::default() };
            link.transmit(sender, &frame, ns(line.stamp_ns));
        }
        link
    }

    #[test]
    fn summary_text_of_a_fresh_link_matches_the_runner_evidence() {
        let mut link = CanLink::new();
        link.attach("ngc-handset.can1");
        link.attach("ngc-main.can1");
        assert_eq!(
            link.summary(),
            "Real firmware CAN link: endpoints=2; connected=True; transmitted=0; scheduled=0; dropped=0; lastId=0x0; dropId=-1"
        );
        assert!(!link.is_paused());
        assert_eq!(link.trace_text(), "");
    }

    #[test]
    fn trace_lines_have_the_exact_renode_format() {
        let link = replay(REFERENCE_HEAD);
        assert_eq!(link.trace_text(), REFERENCE_HEAD);
        assert_eq!(link.trace_lines().len(), 12);
        assert_eq!(link.transmitted(), 12);
        assert_eq!(link.scheduled(), 12);
        assert_eq!(
            link.summary(),
            "Real firmware CAN link: endpoints=2; connected=True; transmitted=12; scheduled=12; dropped=0; lastId=0x5A5; dropId=-1"
        );
    }

    #[test]
    fn dropped_and_extended_remote_lines_use_the_same_format() {
        let mut link = CanLink::new();
        let a = link.attach("ngc-handset.can1");
        let _b = link.attach("ngc-main.can1");
        link.set_connected(false);
        link.transmit(a, &CanFrame { id: 0x12345678, data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03], extended: true, remote: true, ..CanFrame::default() }, ns(3_723_000_000_123));
        assert_eq!(
            link.trace_lines(),
            vec!["01:02:03.000000123\tngc-handset.can1\t0x12345678\tDEADBEEF010203\tdropped\textended=True;remote=True".to_string()]
        );
        // Identifier below 0x100 is zero padded to three digits.
        link.set_connected(true);
        link.transmit(a, &CanFrame::standard(0x7, &[]), 5);
        assert!(link.trace_lines()[1].contains("\t0x007\t\tscheduled\t"), "{}", link.trace_lines()[1]);
    }

    #[test]
    fn drop_id_connected_and_pause_discard_frames() {
        let mut link = CanLink::new();
        let a = link.attach("a");
        let b = link.attach("b");
        assert_eq!(link.transmit(a, &CanFrame::standard(0x10, &[1]), 1).len(), 1);
        link.set_drop_id(0x20);
        assert!(link.transmit(a, &CanFrame::standard(0x20, &[2]), 2).is_empty());
        assert_eq!(link.transmit(b, &CanFrame::standard(0x21, &[3]), 3).len(), 1);
        link.set_drop_id(-1);
        link.set_connected(false);
        assert!(link.transmit(a, &CanFrame::standard(0x10, &[4]), 4).is_empty());
        link.set_connected(true);
        link.pause();
        assert!(link.is_paused());
        assert!(link.transmit(a, &CanFrame::standard(0x10, &[5]), 5).is_empty());
        link.resume();
        assert_eq!(link.transmit(a, &CanFrame::standard(0x10, &[6]), 6).len(), 1);
        assert_eq!((link.transmitted(), link.scheduled(), link.dropped()), (6, 3, 3));
        let outcomes: Vec<String> = link.trace_lines().iter().map(|l| l.split('\t').nth(4).unwrap().to_string()).collect();
        assert_eq!(outcomes, ["scheduled", "dropped", "scheduled", "dropped", "dropped", "scheduled"]);
        assert_eq!(link.drop_id(), -1);
        assert!(link.summary().contains("lastId=0x10;"));
        // Every receiver but the sender gets the frame, in attachment order.
        let c = link.attach("c");
        let targets: Vec<EndpointId> = link.transmit(b, &CanFrame::standard(1, &[]), 7).iter().map(|d| d.to).collect();
        assert_eq!(targets, vec![a, c]);
        link.detach(a);
        let targets: Vec<EndpointId> = link.transmit(b, &CanFrame::standard(1, &[]), 8).iter().map(|d| d.to).collect();
        assert_eq!(targets, vec![c]);
        assert!(link.transmit(a, &CanFrame::standard(1, &[]), 9).is_empty(), "a detached endpoint cannot send");
        assert_eq!(link.endpoint_count(), 2);
        // Attaching the same name twice is a no-op.
        assert_eq!(link.attach("b"), b);
        assert_eq!(link.endpoint_count(), 2);
    }

    #[test]
    fn route_orders_by_stamp_then_batch_order() {
        let mut link = CanLink::new();
        let main = link.attach("ngc-main.can1");
        let handset = link.attach("ngc-handset.can1");
        let tx = |time: Time, id: u32| TxFrame { time, frame: CanFrame::standard(id, &[]) };
        let batches = vec![(main, vec![tx(300, 0x30), tx(500, 0x50)]), (handset, vec![tx(100, 0x10), tx(300, 0x31), tx(300, 0x32)])];
        let deliveries = link.route(&batches);
        let ids: Vec<u32> = deliveries.iter().map(|d| d.frame.id).collect();
        assert_eq!(ids, vec![0x10, 0x30, 0x31, 0x32, 0x50], "equal stamps keep batch order, then transmit order");
        assert_eq!(deliveries[0].to, main);
        assert_eq!(deliveries[1].to, handset);
        let senders: Vec<String> = link.trace_lines().iter().map(|l| l.split('\t').nth(1).unwrap().to_string()).collect();
        assert_eq!(senders, ["ngc-handset.can1", "ngc-main.can1", "ngc-handset.can1", "ngc-handset.can1", "ngc-main.can1"]);
    }

    #[test]
    fn the_trace_is_trimmed_like_renode() {
        let mut link = CanLink::new();
        let a = link.attach("a");
        link.attach("b");
        for i in 0..=TRACE_LIMIT as u32 {
            link.transmit(a, &CanFrame::extended(i, &[]), u64::from(i));
        }
        // The 100 001st line pushed the count over the limit: the oldest 1000 are gone.
        assert_eq!(link.trace_len(), TRACE_LIMIT + 1 - TRACE_TRIM);
        let first = &link.trace_lines()[0];
        assert!(first.contains(&format!("\t0x{:03X}\t", TRACE_TRIM)), "{first}");
        link.clear_trace();
        assert_eq!(link.trace_len(), 0);
        assert_eq!(link.transmitted(), TRACE_LIMIT as u64 + 1, "counters are not trimmed");
    }

    #[test]
    fn time_interval_text_matches_renode() {
        assert_eq!(format_time_interval(0), "00:00:00.000000000");
        assert_eq!(format_time_interval(ns(1_050_000_000)), "00:00:01.050000000");
        assert_eq!(format_time_interval(ns(86_399_999_999_999)), "23:59:59.999999999");
        assert_eq!(format_time_interval(ns(360_000_000_000_000)), "100:00:00.000000000");
    }

    // ---- end to end with two bxCAN controllers ----

    struct Rig {
        boards: [Harness; 2],
        cans: [PeriphId; 2],
    }

    impl CanPorts for Rig {
        fn take_tx_frames(&mut self, endpoint: EndpointId) -> Vec<TxFrame> {
            self.boards[endpoint.0].get_mut::<StmCan>(self.cans[endpoint.0]).take_tx_frames()
        }

        fn deliver(&mut self, endpoint: EndpointId, frame: &CanFrame) {
            self.boards[endpoint.0].with::<StmCan, _>(self.cans[endpoint.0], |can, ctx| can.deliver_frame(ctx, frame));
        }
    }

    impl Rig {
        fn new() -> Rig {
            let make = || {
                let mut h = Harness::new();
                let id = h.add_mapped(SYSBUS_CAN, 0x400, StmCan::new("can1"));
                h.connect_irq(id, LINE_RX0, 20);
                h.clear_irq_changes();
                h.get_mut::<StmCan>(id).set_frame_sink_attached(true);
                h.write32(SYSBUS_CAN + offset::MCR, 0x0001_0000);
                // Accept-all 32-bit mask filter in FIFO 0 and the FMP0 interrupt.
                h.write32(SYSBUS_CAN + offset::FMR, 0x2A1C_0E01);
                h.write32(SYSBUS_CAN + offset::FS1R, 1);
                h.write32(SYSBUS_CAN + offset::FA1R, 1);
                h.write32(SYSBUS_CAN + offset::FMR, 0x2A1C_0E00);
                h.write32(SYSBUS_CAN + offset::IER, 1 << 1);
                (h, id)
            };
            let (ha, ia) = make();
            let (hb, ib) = make();
            Rig { boards: [ha, hb], cans: [ia, ib] }
        }

        fn send(&mut self, board: usize, mailbox: u32, std_id: u32, data: &[u8]) {
            let h = &mut self.boards[board];
            let base = SYSBUS_CAN + offset::TI0R + 0x10 * mailbox;
            let mut low = 0u32;
            let mut high = 0u32;
            for (i, byte) in data.iter().enumerate() {
                if i < 4 {
                    low |= u32::from(*byte) << (8 * i);
                } else {
                    high |= u32::from(*byte) << (8 * (i - 4));
                }
            }
            h.write32(base + 4, data.len() as u32);
            h.write32(base + 8, low);
            h.write32(base + 12, high);
            h.write32(base, (std_id << 21) | 1);
        }

        fn rx_head(&mut self, board: usize) -> [u32; 4] {
            let h = &mut self.boards[board];
            [offset::RI0R, offset::RDT0R, offset::RL0R, offset::RH0R].map(|off| h.read32(SYSBUS_CAN + off))
        }
    }

    #[test]
    fn two_controllers_exchange_frames_through_the_link() {
        let mut rig = Rig::new();
        let mut link = CanLink::new();
        let handset = link.attach("ngc-handset.can1");
        let main = link.attach("ngc-main.can1");
        // Handset sends 0x103 [1,2]; main sends 0x154 [0xFF] and a zero-length 0x118; both at known times.
        rig.boards[handset.0].advance_to(from_micros(100));
        rig.send(handset.0, 0, 0x103, &[1, 2]);
        rig.boards[main.0].advance_to(from_micros(40));
        rig.send(main.0, 0, 0x154, &[0xFF]);
        rig.boards[main.0].advance_to(from_micros(60));
        rig.send(main.0, 1, 0x118, &[]);
        assert!(!rig.boards[handset.0].irq_level(20));
        let delivered = link.pump(&[main, handset], &mut rig);
        assert_eq!(delivered, 3);
        // Receivers got the payloads, the zero-length frame has DLC 0, the interrupt line is up.
        assert_eq!(rig.boards[main.0].get::<StmCan>(rig.cans[main.0]).fifo_len(0), 1);
        assert_eq!(rig.boards[handset.0].get::<StmCan>(rig.cans[handset.0]).fifo_len(0), 2);
        assert_eq!(rig.rx_head(main.0), [0x103 << 21, 2, 0x0201, 0]);
        assert_eq!(rig.rx_head(handset.0), [0x154 << 21, 1, 0xFF, 0], "oldest first (stamp 40 us)");
        assert!(rig.boards[handset.0].irq_level(20));
        rig.boards[handset.0].write32(SYSBUS_CAN + offset::RF0R, 1 << 5);
        assert_eq!(rig.rx_head(handset.0), [0x118 << 21, 0, 0, 0]);
        assert_eq!(link.trace_lines(), vec![
            format!("{}\tngc-main.can1\t0x154\tFF\tscheduled\textended=False;remote=False", format_time_interval(from_micros(40))),
            format!("{}\tngc-main.can1\t0x118\t\tscheduled\textended=False;remote=False", format_time_interval(from_micros(60))),
            format!("{}\tngc-handset.can1\t0x103\t0102\tscheduled\textended=False;remote=False", format_time_interval(from_micros(100))),
        ]);
        // The out-queues were drained.
        assert_eq!(link.pump(&[main, handset], &mut rig), 0);
    }

    #[test]
    fn disconnected_link_and_drop_id_keep_frames_away_from_the_receiver() {
        let mut rig = Rig::new();
        let mut link = CanLink::new();
        let handset = link.attach("ngc-handset.can1");
        let main = link.attach("ngc-main.can1");
        link.set_connected(false);
        rig.send(main.0, 0, 0x070, &[1]);
        assert_eq!(link.pump(&[main, handset], &mut rig), 0);
        assert_eq!(rig.boards[handset.0].get::<StmCan>(rig.cans[handset.0]).fifo_len(0), 0);
        // The sender is unaffected: its mailbox completed (the controller knows nothing about the link).
        assert_eq!(rig.boards[main.0].read32(SYSBUS_CAN + offset::TSR) & 1, 1);
        assert_eq!(rig.boards[main.0].read32(SYSBUS_CAN + offset::ESR), 0);
        link.set_connected(true);
        link.set_drop_id(0x068);
        rig.send(main.0, 1, 0x068, &[2]);
        rig.send(main.0, 2, 0x07B, &[3]);
        assert_eq!(link.pump(&[main, handset], &mut rig), 1);
        assert_eq!(rig.rx_head(handset.0)[0] >> 21, 0x07B);
        let lines = link.trace_lines();
        assert!(lines[0].ends_with("\tdropped\textended=False;remote=False"));
        assert!(lines[1].contains("\t0x068\t02\tdropped\t"));
        assert!(lines[2].contains("\t0x07B\t03\tscheduled\t"));
        assert_eq!(
            link.summary(),
            "Real firmware CAN link: endpoints=2; connected=True; transmitted=3; scheduled=1; dropped=2; lastId=0x7B; dropId=104"
        );
    }

    #[test]
    fn extended_remote_and_long_frames_survive_the_round_trip() {
        let mut rig = Rig::new();
        let mut link = CanLink::new();
        let a = link.attach("a");
        let b = link.attach("b");
        {
            // Extended remote frame 0x12345678, then a DLC-15 frame (bytes above 8 are zero).
            let h = &mut rig.boards[a.0];
            h.write32(SYSBUS_CAN + offset::TDT0R, 0);
            h.write32(SYSBUS_CAN + offset::TI0R, ((0x1234_5678u32 << 3) | 0b110) | 1);
            h.write32(SYSBUS_CAN + offset::TDT1R, 15);
            h.write32(SYSBUS_CAN + offset::TDL1R, 0x0403_0201);
            h.write32(SYSBUS_CAN + offset::TDH1R, 0x0807_0605);
            h.write32(SYSBUS_CAN + offset::TI1R, (0x555 << 21) | 1);
        }
        assert_eq!(link.pump(&[a, b], &mut rig), 2);
        assert_eq!(rig.rx_head(b.0), [(0x1234_5678 << 3) | 0b110, 0, 0, 0]);
        rig.boards[b.0].write32(SYSBUS_CAN + offset::RF0R, 1 << 5);
        assert_eq!(rig.rx_head(b.0), [0x555 << 21, 15, 0x0403_0201, 0x0807_0605]);
        assert!(link.trace_lines()[0].contains("\t0x12345678\t\tscheduled\textended=True;remote=True"));
        let payload = format!("0102030405060708{}", "00".repeat(7));
        assert!(link.trace_lines()[1].contains(&format!("\t0x555\t{payload}\tscheduled")), "{}", link.trace_lines()[1]);
    }
}
