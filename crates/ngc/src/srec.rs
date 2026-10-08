//! Motorola S-record (SREC) parser with checksum, count, overlap and contiguity checks.
//!
//! Supported records: S0 (header), S1/S2/S3 (data with 16/24/32-bit addresses), S5/S6 (record
//! count), S7/S8/S9 (termination with 32/24/16-bit start address). S4 is reserved and rejected.
//! Lines may end in LF or CRLF; blank lines are ignored; hex digits may be either case.

use std::fmt;

/// A run of contiguous bytes starting at `start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: u32,
    pub data: Vec<u8>,
}

impl Segment {
    /// One past the last address (as `u64`, so a segment ending at 4 GiB is representable).
    pub fn end(&self) -> u64 {
        u64::from(self.start) + self.data.len() as u64
    }
}

/// A parsed S-record file. Data records are merged into address-sorted contiguous segments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Srec {
    /// Payload of the S0 record (usually the file name).
    pub header: Vec<u8>,
    /// Number of records per type digit (index 4 is always 0).
    pub record_counts: [u32; 10],
    /// Address-sorted segments; adjacent data records are merged, gaps are preserved.
    pub segments: Vec<Segment>,
    /// Start (execution) address of the S7/S8/S9 record.
    pub start_address: Option<u32>,
    /// Type digit (7, 8 or 9) of the termination record.
    pub start_record_type: Option<u8>,
    /// Total payload bytes in S1/S2/S3 records.
    pub data_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SrecErrorKind {
    NotARecord,
    ReservedType(u8),
    BadHex,
    LengthMismatch { declared: usize, actual: usize },
    TooShort,
    Checksum { expected: u8, found: u8 },
    UnexpectedData(u8),
    DuplicateHeader,
    DuplicateTermination,
    DataAfterTermination,
    CountMismatch { declared: u32, actual: u32 },
    AddressOverflow { address: u32, length: usize },
    Overlap { address: u32 },
}

/// Error with the 1-based line number (0 for whole-file errors such as overlaps).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrecError {
    pub line: usize,
    pub kind: SrecErrorKind,
}

impl fmt::Display for SrecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "line {}: ", self.line)?;
        }
        match &self.kind {
            SrecErrorKind::NotARecord => write!(f, "not an S-record"),
            SrecErrorKind::ReservedType(t) => write!(f, "reserved record type S{t}"),
            SrecErrorKind::BadHex => write!(f, "invalid hexadecimal digits"),
            SrecErrorKind::LengthMismatch { declared, actual } => {
                write!(f, "record declares {declared} bytes but carries {actual}")
            }
            SrecErrorKind::TooShort => write!(f, "record too short for its address and checksum"),
            SrecErrorKind::Checksum { expected, found } => {
                write!(f, "checksum mismatch (expected 0x{expected:02X}, found 0x{found:02X})")
            }
            SrecErrorKind::UnexpectedData(t) => write!(f, "S{t} record must not carry data"),
            SrecErrorKind::DuplicateHeader => write!(f, "second S0 header record"),
            SrecErrorKind::DuplicateTermination => write!(f, "second termination record"),
            SrecErrorKind::DataAfterTermination => write!(f, "data record after the termination record"),
            SrecErrorKind::CountMismatch { declared, actual } => {
                write!(f, "record count {declared} does not match the {actual} data records seen")
            }
            SrecErrorKind::AddressOverflow { address, length } => {
                write!(f, "{length} bytes at 0x{address:08X} run past the 32-bit address space")
            }
            SrecErrorKind::Overlap { address } => write!(f, "data records overlap at 0x{address:08X}"),
        }
    }
}

impl std::error::Error for SrecError {}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn trim(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && matches!(line[start], b' ' | b'\t') {
        start += 1;
    }
    while end > start && matches!(line[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    &line[start..end]
}

/// Parses an S-record file.
pub fn parse(input: &[u8]) -> Result<Srec, SrecError> {
    let mut srec = Srec::default();
    // (address, offset into `blob`, length) of every data record, in file order.
    let mut ranges: Vec<(u32, usize, usize)> = Vec::new();
    let mut blob: Vec<u8> = Vec::with_capacity(input.len() / 2);
    let mut decoded: Vec<u8> = Vec::with_capacity(300);
    let mut data_records = 0u32;
    let mut have_header = false;
    let mut terminated = false;

    for (index, raw) in input.split(|&b| b == b'\n').enumerate() {
        let line_no = index + 1;
        let line = trim(raw);
        if line.is_empty() {
            continue;
        }
        let fail = |kind: SrecErrorKind| SrecError { line: line_no, kind };
        if line.len() < 4 || line[0] != b'S' || !line[1].is_ascii_digit() {
            return Err(fail(SrecErrorKind::NotARecord));
        }
        let kind = line[1] - b'0';
        if kind == 4 {
            return Err(fail(SrecErrorKind::ReservedType(4)));
        }
        let digits = &line[2..];
        if digits.len() % 2 != 0 {
            return Err(fail(SrecErrorKind::BadHex));
        }
        decoded.clear();
        for pair in digits.chunks_exact(2) {
            match (hex_value(pair[0]), hex_value(pair[1])) {
                (Some(hi), Some(lo)) => decoded.push((hi << 4) | lo),
                _ => return Err(fail(SrecErrorKind::BadHex)),
            }
        }
        let declared = usize::from(decoded[0]);
        if decoded.len() != declared + 1 {
            return Err(fail(SrecErrorKind::LengthMismatch { declared, actual: decoded.len() - 1 }));
        }
        let checksum_index = decoded.len() - 1;
        let sum = decoded[..checksum_index].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        let expected = !sum;
        if expected != decoded[checksum_index] {
            return Err(fail(SrecErrorKind::Checksum { expected, found: decoded[checksum_index] }));
        }
        let address_len = match kind {
            0 | 1 | 5 | 9 => 2,
            2 | 6 | 8 => 3,
            _ => 4, // 3, 7
        };
        if declared < address_len + 1 {
            return Err(fail(SrecErrorKind::TooShort));
        }
        let address = decoded[1..1 + address_len].iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
        let data = &decoded[1 + address_len..checksum_index];

        match kind {
            0 => {
                if have_header {
                    return Err(fail(SrecErrorKind::DuplicateHeader));
                }
                have_header = true;
                srec.header = data.to_vec();
            }
            1..=3 => {
                if terminated {
                    return Err(fail(SrecErrorKind::DataAfterTermination));
                }
                if u64::from(address) + data.len() as u64 > 1 << 32 {
                    return Err(fail(SrecErrorKind::AddressOverflow { address, length: data.len() }));
                }
                if !data.is_empty() {
                    ranges.push((address, blob.len(), data.len()));
                    blob.extend_from_slice(data);
                }
                srec.data_bytes += data.len() as u64;
                data_records += 1;
            }
            5 | 6 => {
                if !data.is_empty() {
                    return Err(fail(SrecErrorKind::UnexpectedData(kind)));
                }
                if address != data_records {
                    return Err(fail(SrecErrorKind::CountMismatch { declared: address, actual: data_records }));
                }
            }
            _ => {
                // 7, 8, 9
                if !data.is_empty() {
                    return Err(fail(SrecErrorKind::UnexpectedData(kind)));
                }
                if terminated {
                    return Err(fail(SrecErrorKind::DuplicateTermination));
                }
                terminated = true;
                srec.start_address = Some(address);
                srec.start_record_type = Some(kind);
            }
        }
        srec.record_counts[usize::from(kind)] += 1;
    }

    // Overlap check and segment merge over the address-sorted ranges.
    ranges.sort_by_key(|&(address, _, _)| address);
    for pair in ranges.windows(2) {
        let (a_start, _, a_len) = pair[0];
        let (b_start, _, _) = pair[1];
        if u64::from(a_start) + a_len as u64 > u64::from(b_start) {
            return Err(SrecError { line: 0, kind: SrecErrorKind::Overlap { address: b_start } });
        }
    }
    for &(address, offset, length) in &ranges {
        let bytes = &blob[offset..offset + length];
        match srec.segments.last_mut() {
            Some(last) if last.end() == u64::from(address) => last.data.extend_from_slice(bytes),
            _ => srec.segments.push(Segment { start: address, data: bytes.to_vec() }),
        }
    }
    Ok(srec)
}

impl Srec {
    /// The S0 payload as text (non-printable bytes replaced by `?`).
    pub fn header_text(&self) -> String {
        self.header.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '?' }).collect()
    }

    /// Lowest data address and one past the highest, if there is any data.
    pub fn span(&self) -> Option<(u32, u64)> {
        let first = self.segments.first()?;
        let last = self.segments.last()?;
        Some((first.start, last.end()))
    }

    /// Holes between segments as `(start, end)` (end exclusive).
    pub fn gaps(&self) -> Vec<(u32, u32)> {
        self.segments.windows(2).map(|w| (w[0].end() as u32, w[1].start)).collect()
    }

    /// Reconstructs the contiguous binary from the lowest to the highest data address, filling
    /// holes with `fill`. Returns the base address and the bytes.
    pub fn to_binary(&self, fill: u8) -> Option<(u32, Vec<u8>)> {
        let (base, end) = self.span()?;
        let mut bytes = vec![fill; (end - u64::from(base)) as usize];
        for segment in &self.segments {
            let offset = (segment.start - base) as usize;
            bytes[offset..offset + segment.data.len()].copy_from_slice(&segment.data);
        }
        Some((base, bytes))
    }

    /// Number of S1/S2/S3 records.
    pub fn data_record_count(&self) -> u32 {
        self.record_counts[1] + self.record_counts[2] + self.record_counts[3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a record with a correct checksum. `address_len` is 2, 3 or 4 bytes.
    fn record(kind: u8, address_len: usize, address: u32, data: &[u8]) -> String {
        let mut body = vec![(address_len + data.len() + 1) as u8];
        for i in (0..address_len).rev() {
            body.push((address >> (8 * i)) as u8);
        }
        body.extend_from_slice(data);
        let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        body.push(!sum);
        let hex: String = body.iter().map(|b| format!("{b:02X}")).collect();
        format!("S{kind}{hex}")
    }

    #[test]
    fn checksum_matches_known_record() {
        // First record of the TRITON main image header.
        let text = "S0230000747269746F6E5F6D61696E5F636F6E74726F6C6C65725F6576616C2E73726563B3\n";
        let parsed = parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.header_text(), "triton_main_controller_eval.srec");
        assert_eq!(record(0, 2, 0, b"triton_main_controller_eval.srec"), text.trim());
    }

    #[test]
    fn parses_all_record_types() {
        let text = [
            record(0, 2, 0, b"hdr"),
            record(1, 2, 0x1000, &[1, 2, 3, 4]),
            record(2, 3, 0x2000, &[5, 6]),
            record(3, 4, 0x0800_4000, &[7, 8, 9]),
            record(5, 2, 3, &[]),
            record(7, 4, 0x0800_4001, &[]),
        ]
        .join("\r\n");
        let s = parse(text.as_bytes()).unwrap();
        assert_eq!(s.header, b"hdr");
        assert_eq!(s.segments.len(), 3);
        assert_eq!(s.segments[0], Segment { start: 0x1000, data: vec![1, 2, 3, 4] });
        assert_eq!(s.segments[1].start, 0x2000);
        assert_eq!(s.segments[2], Segment { start: 0x0800_4000, data: vec![7, 8, 9] });
        assert_eq!(s.start_address, Some(0x0800_4001));
        assert_eq!(s.start_record_type, Some(7));
        assert_eq!(s.record_counts[0], 1);
        assert_eq!(s.record_counts[5], 1);
        assert_eq!(s.data_record_count(), 3);
        assert_eq!(s.data_bytes, 9);
        assert_eq!(s.span(), Some((0x1000, 0x0800_4003)));
        assert_eq!(s.gaps(), vec![(0x1004, 0x2000), (0x2002, 0x0800_4000)]);
    }

    #[test]
    fn s6_s8_s9_and_lowercase_hex() {
        let text = [
            record(2, 3, 0x10, &[0xAA]),
            record(6, 3, 1, &[]),
            record(8, 3, 0x123456, &[]),
        ]
        .join("\n")
        .to_lowercase()
        .replace('s', "S");
        let s = parse(text.as_bytes()).unwrap();
        assert_eq!(s.start_address, Some(0x123456));
        assert_eq!(s.start_record_type, Some(8));
        let s9 = parse(record(9, 2, 0xBEEF, &[]).as_bytes()).unwrap();
        assert_eq!(s9.start_address, Some(0xBEEF));
        assert!(s9.segments.is_empty());
        assert_eq!(s9.span(), None);
        assert_eq!(s9.to_binary(0xFF), None);
    }

    #[test]
    fn adjacent_records_merge_and_gaps_are_kept() {
        let text = [
            record(3, 4, 0x100, &[1, 2, 3, 4]),
            record(3, 4, 0x104, &[5, 6]),
            record(3, 4, 0x10C, &[9]),
        ]
        .join("\n");
        let s = parse(text.as_bytes()).unwrap();
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].data, [1, 2, 3, 4, 5, 6]);
        assert_eq!(s.segments[1], Segment { start: 0x10C, data: vec![9] });
        let (base, bin) = s.to_binary(0xFF).unwrap();
        assert_eq!(base, 0x100);
        assert_eq!(bin, [1, 2, 3, 4, 5, 6, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 9]);
    }

    #[test]
    fn out_of_order_records_are_sorted() {
        let text = [record(3, 4, 0x200, &[2]), record(3, 4, 0x100, &[1]), record(3, 4, 0x101, &[3])].join("\n");
        let s = parse(text.as_bytes()).unwrap();
        assert_eq!(s.segments, [Segment { start: 0x100, data: vec![1, 3] }, Segment { start: 0x200, data: vec![2] }]);
    }

    #[test]
    fn rejects_bad_records() {
        let good = record(3, 4, 0x100, &[1, 2, 3]);
        let mut bad_sum = good.clone();
        bad_sum.replace_range(bad_sum.len() - 2.., "00");
        let cases: Vec<(String, SrecErrorKind)> = vec![
            ("hello".into(), SrecErrorKind::NotARecord),
            ("X1".into(), SrecErrorKind::NotARecord),
            ("S".into(), SrecErrorKind::NotARecord),
            (record(4, 2, 0, &[]), SrecErrorKind::ReservedType(4)),
            ("S3G0".into(), SrecErrorKind::BadHex),
            (format!("{good}0"), SrecErrorKind::BadHex),
            (good[..good.len() - 2].to_string(), SrecErrorKind::LengthMismatch { declared: 8, actual: 7 }),
            (bad_sum, SrecErrorKind::Checksum { expected: !(good_sum(&good)), found: 0 }),
            ("S1020000".into(), SrecErrorKind::Checksum { expected: 0xFD, found: 0 }),
            (record(7, 4, 1, &[0]), SrecErrorKind::UnexpectedData(7)),
            (record(5, 2, 0, &[1]), SrecErrorKind::UnexpectedData(5)),
        ];
        for (text, kind) in cases {
            let e = parse(text.as_bytes()).unwrap_err();
            assert_eq!(e.kind, kind, "{text}");
            assert_eq!(e.line, 1);
        }
        // Too short for its address: count 2 for an S3 (needs 4 address bytes + checksum).
        let e = parse(b"S3020000").unwrap_err();
        assert!(matches!(e.kind, SrecErrorKind::Checksum { .. } | SrecErrorKind::TooShort));
        let short = {
            let body = [3u8, 0, 0, 0];
            let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            format!("S3{:02X}{:02X}{:02X}{:02X}", body[0], body[1], body[2], !sum)
        };
        assert_eq!(parse(short.as_bytes()).unwrap_err().kind, SrecErrorKind::TooShort);
    }

    fn good_sum(record: &str) -> u8 {
        // Sum of the bytes covered by the checksum of a record string.
        let bytes: Vec<u8> = (2..record.len() - 2).step_by(2).map(|i| u8::from_str_radix(&record[i..i + 2], 16).unwrap()).collect();
        bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
    }

    #[test]
    fn structural_errors_report_line_numbers() {
        let hdr = record(0, 2, 0, b"a");
        let data = record(3, 4, 0x100, &[1]);
        let term = record(7, 4, 0x100, &[]);

        let e = parse(format!("{hdr}\n{hdr}\n").as_bytes()).unwrap_err();
        assert_eq!((e.line, e.kind), (2, SrecErrorKind::DuplicateHeader));

        let e = parse(format!("{hdr}\n{term}\n{term}\n").as_bytes()).unwrap_err();
        assert_eq!((e.line, e.kind), (3, SrecErrorKind::DuplicateTermination));

        let e = parse(format!("{hdr}\n{term}\n{data}\n").as_bytes()).unwrap_err();
        assert_eq!((e.line, e.kind), (3, SrecErrorKind::DataAfterTermination));

        let e = parse(format!("{data}\n{}\n", record(5, 2, 2, &[])).as_bytes()).unwrap_err();
        assert_eq!((e.line, e.kind), (2, SrecErrorKind::CountMismatch { declared: 2, actual: 1 }));

        let e = parse(record(3, 4, 0xFFFF_FFFF, &[1, 2]).as_bytes()).unwrap_err();
        assert_eq!(e.kind, SrecErrorKind::AddressOverflow { address: 0xFFFF_FFFF, length: 2 });
        // Ending exactly at 4 GiB is fine.
        assert!(parse(record(3, 4, 0xFFFF_FFFE, &[1, 2]).as_bytes()).is_ok());
    }

    #[test]
    fn overlaps_are_detected_regardless_of_order() {
        let a = record(3, 4, 0x100, &[1, 2, 3, 4]);
        let b = record(3, 4, 0x102, &[9, 9]);
        for text in [format!("{a}\n{b}"), format!("{b}\n{a}")] {
            let e = parse(text.as_bytes()).unwrap_err();
            assert_eq!((e.line, e.kind), (0, SrecErrorKind::Overlap { address: 0x102 }), "{text}");
        }
        // Exact duplicates overlap too.
        let e = parse(format!("{a}\n{a}").as_bytes()).unwrap_err();
        assert!(matches!(e.kind, SrecErrorKind::Overlap { address: 0x100 }));
        // Touching but not overlapping is fine.
        assert!(parse(format!("{a}\n{}", record(3, 4, 0x104, &[1])).as_bytes()).is_ok());
    }

    #[test]
    fn blank_lines_and_missing_final_newline() {
        let text = format!("\n  {}  \r\n\r\n{}", record(3, 4, 0x10, &[1]), record(7, 4, 0x10, &[]));
        let s = parse(text.as_bytes()).unwrap();
        assert_eq!(s.segments.len(), 1);
        assert_eq!(s.start_address, Some(0x10));
        assert_eq!(parse(b"").unwrap(), Srec::default());
    }

    #[test]
    fn error_display() {
        let e = parse(b"nonsense").unwrap_err();
        assert_eq!(e.to_string(), "line 1: not an S-record");
        let e = SrecError { line: 0, kind: SrecErrorKind::Overlap { address: 0x1234 } };
        assert_eq!(e.to_string(), "data records overlap at 0x00001234");
    }
}
