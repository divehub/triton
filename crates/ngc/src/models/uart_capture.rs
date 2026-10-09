// Ported from class NGCUartCapture in emulation/models/NGCBoardTelemetry.cs of the analysis workspace
// (Renode 1.17.0 external, see that file for its license).

//! `NGCUartCapture`: passive observation of UART transmit streams.
//!
//! The Renode external subscribes to `IUART.CharReceived` of each connected USART (the TX byte, raised when the
//! guest writes `TDR`) and keeps, per channel, at most the **last 16 KiB** of bytes, the **total** number of bytes
//! and the virtual time of the **last** byte. Nothing here consumes guest receive data or feeds a byte back:
//! `WriteChar`/RX injection is deliberately not used. A bounded tail with its total and a truncation flag keeps a
//! diagnostic view from being mistaken for an exhaustive trace.
//!
//! Wiring: `capture.attach("ngc-main.uart4")` returns the hook to install on the USART
//! (`Usart::set_tx_hook(Some(hook))`); it is the Rust counterpart of `connector Connect uart4 uartCapture`.
//! [`UartCapture::snapshot_json`] is `uartCapture SnapshotJson` (what the runner's `uartConsole` state is built from).

use emu_core::{to_secs_f64, Json, Time};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;
use stm32::usart::TxHook;

/// Per-channel retained bytes (`Capacity` in the C# class).
pub const CAPACITY: usize = 16384;

struct Stream {
    name: String,
    total: u64,
    bytes: VecDeque<u8>,
    last_tx: Option<Time>,
    attached: bool,
}

/// One channel of [`UartCapture::snapshot`] (the fields of the C# `SnapshotJson` objects).
#[derive(Clone, Debug, PartialEq)]
pub struct StreamSnapshot {
    /// Channel name given to [`UartCapture::attach`], e.g. `ngc-main.uart4`.
    pub name: String,
    /// Total bytes transmitted since the capture was attached (`txBytes`).
    pub tx_bytes: u64,
    /// The retained tail as text: tab, LF, CR and printable ASCII verbatim, everything else as `\xNN`.
    pub text: String,
    /// The retained tail as upper-case hex bytes separated by spaces.
    pub hex: String,
    /// More bytes were transmitted than are retained.
    pub truncated: bool,
    /// Virtual seconds of the last transmitted byte (`lastTxVirtualTime`), `None` before the first.
    pub last_tx_virtual_time: Option<f64>,
}

impl StreamSnapshot {
    /// The JSON object the runner expects (`name`, `txBytes`, `text`, `hex`, `truncated`, `lastTxVirtualTime`).
    pub fn to_json(&self) -> Json {
        Json::from_pairs([
            ("name", Json::from(self.name.as_str())),
            ("txBytes", Json::from(self.tx_bytes)),
            ("text", Json::from(self.text.as_str())),
            ("hex", Json::from(self.hex.as_str())),
            ("truncated", Json::from(self.truncated)),
            ("lastTxVirtualTime", self.last_tx_virtual_time.map_or(Json::Null, Json::from)),
        ])
    }
}

/// The capture external. Channels are kept in attachment order.
#[derive(Default)]
pub struct UartCapture {
    streams: Vec<Rc<RefCell<Stream>>>,
}

impl UartCapture {
    pub fn new() -> UartCapture {
        UartCapture::default()
    }

    /// `AttachTo`: starts capturing the channel `name` and returns the transmit hook for its USART.
    /// Attaching a name that is already attached keeps the existing channel (Renode ignores a second connect).
    pub fn attach(&mut self, name: impl Into<String>) -> TxHook {
        let name = name.into();
        let stream = match self.streams.iter().find(|s| s.borrow().name == name) {
            Some(existing) => existing.clone(),
            None => {
                let stream = Rc::new(RefCell::new(Stream { name, total: 0, bytes: VecDeque::new(), last_tx: None, attached: true }));
                self.streams.push(stream.clone());
                stream
            }
        };
        Box::new(move |byte, time| {
            let mut stream = stream.borrow_mut();
            if !stream.attached {
                return;
            }
            // Receive(): count, enqueue, drop the oldest byte beyond the capacity, remember the time.
            stream.total += 1;
            stream.bytes.push_back(byte);
            if stream.bytes.len() > CAPACITY {
                stream.bytes.pop_front();
            }
            stream.last_tx = Some(time);
        })
    }

    /// `DetachFrom`: stops capturing the channel and forgets it (hooks already handed out become inert).
    pub fn detach(&mut self, name: &str) -> bool {
        match self.streams.iter().position(|s| s.borrow().name == name) {
            Some(index) => {
                self.streams.remove(index).borrow_mut().attached = false;
                true
            }
            None => false,
        }
    }

    /// Names of the attached channels in attachment order.
    pub fn names(&self) -> Vec<String> {
        self.streams.iter().map(|s| s.borrow().name.clone()).collect()
    }

    /// Typed snapshot of every channel.
    pub fn snapshot(&self) -> Vec<StreamSnapshot> {
        self.streams
            .iter()
            .map(|stream| {
                let stream = stream.borrow();
                let mut text = String::new();
                let mut hex = String::with_capacity(stream.bytes.len() * 3);
                for (index, &byte) in stream.bytes.iter().enumerate() {
                    if byte == 9 || byte == 10 || byte == 13 || (32..127).contains(&byte) {
                        text.push(byte as char);
                    } else {
                        let _ = write!(text, "\\x{byte:02X}");
                    }
                    if index > 0 {
                        hex.push(' ');
                    }
                    let _ = write!(hex, "{byte:02X}");
                }
                StreamSnapshot {
                    name: stream.name.clone(),
                    tx_bytes: stream.total,
                    text,
                    hex,
                    truncated: stream.total > stream.bytes.len() as u64,
                    last_tx_virtual_time: stream.last_tx.map(to_secs_f64),
                }
            })
            .collect()
    }

    /// JSON array of the channel objects.
    pub fn to_json(&self) -> Json {
        Json::from_items(self.snapshot().iter().map(StreamSnapshot::to_json))
    }

    /// `SnapshotJson`: the compact array exactly as the C# external writes it (control characters in `text` use
    /// the `\u00xx` escape of `NGCBoardTelemetry.Quote`; times use the shortest decimal form).
    pub fn snapshot_json(&self) -> String {
        let mut out = String::from("[");
        for (index, s) in self.snapshot().iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            quote(&mut out, &s.name);
            let _ = write!(out, ",\"txBytes\":{},\"text\":", s.tx_bytes);
            quote(&mut out, &s.text);
            out.push_str(",\"hex\":");
            quote(&mut out, &s.hex);
            let _ = write!(out, ",\"truncated\":{},\"lastTxVirtualTime\":", s.truncated);
            match s.last_tx_virtual_time {
                Some(seconds) => out.push_str(&format_double_r(seconds)),
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push(']');
        out
    }
}

/// .NET `double.ToString("R", InvariantCulture)` (`NGCBoardTelemetry.Number`): the shortest round-trip digits,
/// in fixed notation for decimal exponents -4..=14 and in `d.dddE+XX` / `d.dddE-XX` notation otherwise
/// (so a transmit at 1 microsecond reads `1E-06`).
fn format_double_r(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let scientific = format!("{value:e}"); // shortest round-trip mantissa, e.g. "2.03e-5"
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent marker");
    let exponent: i32 = exponent.parse().expect("exponent digits");
    if exponent > -5 && exponent < 15 {
        format!("{value}")
    } else {
        format!("{mantissa}E{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    }
}

/// `NGCBoardTelemetry.Quote`: backslash-escapes `"` and `\`, writes other control characters as `\u00xx`.
fn quote(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
            out.push(c);
        } else if (c as u32) < 32 {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push(c);
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::{from_secs_f64, TICKS_PER_SECOND};

    fn feed(hook: &mut TxHook, bytes: &[u8], time: Time) {
        for &byte in bytes {
            hook(byte, time);
        }
    }

    #[test]
    fn channels_keep_attachment_order_and_report_nothing_before_the_first_byte() {
        let mut capture = UartCapture::new();
        let _a = capture.attach("ngc-main.uart4");
        let _b = capture.attach("ngc-main.usart1");
        let _c = capture.attach("ngc-handset.usart3");
        assert_eq!(capture.names(), ["ngc-main.uart4", "ngc-main.usart1", "ngc-handset.usart3"]);
        assert_eq!(
            capture.snapshot_json(),
            "[{\"name\":\"ngc-main.uart4\",\"txBytes\":0,\"text\":\"\",\"hex\":\"\",\"truncated\":false,\"lastTxVirtualTime\":null},\
             {\"name\":\"ngc-main.usart1\",\"txBytes\":0,\"text\":\"\",\"hex\":\"\",\"truncated\":false,\"lastTxVirtualTime\":null},\
             {\"name\":\"ngc-handset.usart3\",\"txBytes\":0,\"text\":\"\",\"hex\":\"\",\"truncated\":false,\"lastTxVirtualTime\":null}]"
        );
        assert_eq!(UartCapture::new().snapshot_json(), "[]");
    }

    #[test]
    fn text_hex_total_and_last_time_follow_the_transmitted_bytes() {
        let mut capture = UartCapture::new();
        let mut uart4 = capture.attach("ngc-main.uart4");
        let mut usart3 = capture.attach("ngc-handset.usart3");
        feed(&mut uart4, b"Wakeup from: HANDSET\r\n", from_secs_f64(0.5));
        feed(&mut usart3, &[0x00, 0x7F, 0xC3, b'A'], from_secs_f64(1.25));
        feed(&mut uart4, b"ok", from_secs_f64(2.5));
        let snapshot = capture.snapshot();
        assert_eq!(snapshot[0].name, "ngc-main.uart4");
        assert_eq!(snapshot[0].tx_bytes, 24);
        assert_eq!(snapshot[0].text, "Wakeup from: HANDSET\r\nok");
        assert_eq!(snapshot[0].hex, "57 61 6B 65 75 70 20 66 72 6F 6D 3A 20 48 41 4E 44 53 45 54 0D 0A 6F 6B");
        assert!(!snapshot[0].truncated);
        assert!((snapshot[0].last_tx_virtual_time.unwrap() - 2.5).abs() < 1e-9);
        assert_eq!(snapshot[1].text, "\\x00\\x7F\\xC3A", "non-printable bytes are shown as \\xNN");
        assert_eq!(snapshot[1].hex, "00 7F C3 41");
        assert!((snapshot[1].last_tx_virtual_time.unwrap() - 1.25).abs() < 1e-9);
    }

    #[test]
    fn the_tail_is_bounded_at_16_kib_with_total_and_truncation_flag() {
        let mut capture = UartCapture::new();
        let mut hook = capture.attach("ngc-main.uart4");
        let bytes: Vec<u8> = (0..CAPACITY + 10).map(|i| b'a' + (i % 26) as u8).collect();
        feed(&mut hook, &bytes, 7);
        let s = &capture.snapshot()[0];
        assert_eq!(s.tx_bytes, (CAPACITY + 10) as u64);
        assert!(s.truncated);
        assert_eq!(s.text.len(), CAPACITY);
        assert_eq!(s.text.as_bytes(), &bytes[10..], "the newest 16384 bytes survive");
        assert_eq!(s.hex.len(), CAPACITY * 3 - 1);
        // Exactly full is not truncated.
        let mut other = UartCapture::new();
        let mut hook = other.attach("x");
        feed(&mut hook, &bytes[..CAPACITY], 0);
        assert!(!other.snapshot()[0].truncated);
    }

    #[test]
    fn snapshot_json_reproduces_the_renode_wire_format() {
        // Taken from the Renode reference run (`ngc-main.uart4` after the first boot message).
        let mut capture = UartCapture::new();
        let mut hook = capture.attach("ngc-main.uart4");
        feed(&mut hook, b"Wakeup from: HANDSET\r\n", 0);
        let mut json = capture.snapshot_json();
        let expected_without_time = "[{\"name\":\"ngc-main.uart4\",\"txBytes\":22,\"text\":\"Wakeup from: HANDSET\\u000d\\u000a\",\"hex\":\"57 61 6B 65 75 70 20 66 72 6F 6D 3A 20 48 41 4E 44 53 45 54 0D 0A\",\"truncated\":false,\"lastTxVirtualTime\":";
        assert!(json.starts_with(expected_without_time), "{json}");
        // Quote(): `"` and `\` are backslash-escaped, tab becomes \u0009.
        let mut other = UartCapture::new();
        let mut hook = other.attach("a\"b\\c");
        feed(&mut hook, b"\t\"\\", 0);
        json = other.snapshot_json();
        assert!(json.contains("\"name\":\"a\\\"b\\\\c\""), "{json}");
        assert!(json.contains("\"text\":\"\\u0009\\\"\\\\\""), "{json}");
        // The typed JSON value parses back to the same content.
        let parsed = Json::parse(&json).unwrap();
        let stream = parsed.at(0).unwrap();
        assert_eq!(stream.get("name").and_then(Json::as_str), Some("a\"b\\c"));
        assert_eq!(stream.get("text").and_then(Json::as_str), Some("\t\"\\"));
        assert_eq!(other.to_json().at(0).unwrap().get("txBytes").and_then(Json::as_u64), Some(3));
    }

    #[test]
    fn the_reference_time_text_is_shortest_decimal_when_the_base_is_one_nanosecond() {
        if TICKS_PER_SECOND != 1_000_000_000 {
            return; // virtual time is not in nanoseconds: sub-nanosecond rounding changes the last digits
        }
        let mut capture = UartCapture::new();
        let mut hook = capture.attach("ngc-main.uart4");
        feed(&mut hook, b"x", 3_268_660);
        assert!(capture.snapshot_json().ends_with("\"lastTxVirtualTime\":0.00326866}]"), "{}", capture.snapshot_json());
        feed(&mut hook, b"y", 1_003_200_000);
        assert!(capture.snapshot_json().ends_with("\"lastTxVirtualTime\":1.0032}]"), "{}", capture.snapshot_json());
    }

    #[test]
    fn times_are_formatted_like_dotnet_round_trip_doubles() {
        for (value, text) in [
            (0.0, "0"),
            (1.0, "1"),
            (1.0032, "1.0032"),
            (0.00326866, "0.00326866"),
            (0.0001, "0.0001"),
            (0.00001, "1E-05"),
            (0.000001, "1E-06"),
            (2.03e-5, "2.03E-05"),
            (1e-7, "1E-07"),
            (123.456789012, "123.456789012"),
            (1e15, "1E+15"),
            (1.5e17, "1.5E+17"),
            (999999999999999.0, "999999999999999"),
        ] {
            assert_eq!(format_double_r(value), text, "{value:e}");
        }
    }

    #[test]
    fn detach_stops_recording_and_a_second_attach_shares_the_channel() {
        let mut capture = UartCapture::new();
        let mut first = capture.attach("ngc-main.uart4");
        let mut second = capture.attach("ngc-main.uart4");
        assert_eq!(capture.names().len(), 1);
        feed(&mut first, b"a", 1);
        feed(&mut second, b"b", 2);
        assert_eq!(capture.snapshot()[0].text, "ab");
        assert!(capture.detach("ngc-main.uart4"));
        assert!(!capture.detach("ngc-main.uart4"));
        feed(&mut first, b"c", 3);
        assert!(capture.names().is_empty());
        assert_eq!(capture.snapshot_json(), "[]");
    }

    #[test]
    fn a_usart_feeds_the_capture_with_the_register_write_time() {
        use emu_core::testing::Harness;
        use stm32::usart::{offset, Usart};
        let mut capture = UartCapture::new();
        let mut h = Harness::new();
        let id = h.add_mapped(0x4000_4C00, 0x400, Usart::new("uart4", 80_000_000));
        h.get_mut::<Usart>(id).set_tx_hook(Some(capture.attach("ngc-main.uart4")));
        h.write32(0x4000_4C00 + offset::CR1, 0b1001);
        h.advance_to(from_secs_f64(0.001));
        for byte in *b"Hi\n" {
            h.write32(0x4000_4C00 + offset::TDR, u32::from(byte));
        }
        let s = &capture.snapshot()[0];
        assert_eq!(s.tx_bytes, 3);
        assert_eq!(s.text, "Hi\n");
        assert!((s.last_tx_virtual_time.unwrap() - 0.001).abs() < 1e-9);
    }
}
