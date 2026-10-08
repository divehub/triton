//! A small PNG encoder (no external crates): `lcd.png` captures of the handset display.
//!
//! The runner converts the Renode PPM to an 8-bit RGB PNG with unfiltered scanlines and `zlib.compress`. This
//! encoder produces the same image (colour type 2, filter type 0 on every row) with its own deflate stream:
//! stored blocks (`level 0`) or LZ77 matches with the fixed Huffman code (`level >= 1`, hash chains over a
//! 32 KiB window). Both are plain RFC 1951 streams inside a zlib wrapper (RFC 1950, Adler-32) and PNG chunks with
//! CRC-32, so any PNG reader decodes them to the identical pixels. [`decode_rgb`] and [`zlib_decompress`] are a
//! minimal reader (stored, fixed and dynamic Huffman blocks, unfiltered/Sub/Up/Average/Paeth rows) used by the
//! tests and the scenario suite to prove that a capture decodes to the pixels that were written.

/// PNG file signature.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// ---- checksums ---------------------------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3, as used by PNG and zlib) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Continues a CRC-32 over more data (`crc32_update(crc32(a), b) == crc32(a ++ b)`).
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &byte in data {
        c = CRC_TABLE[((c ^ u32::from(byte)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Adler-32 of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 bytes can be summed before the 32-bit accumulators need a reduction.
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// ---- deflate -------------------------------------------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), bits: 0, count: 0 }
    }

    /// Writes `n` bits of `value`, least significant bit first (extra bits, block headers).
    fn put(&mut self, value: u32, n: u32) {
        self.bits |= value << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// Writes a Huffman code of `n` bits, most significant bit first.
    fn put_code(&mut self, code: u32, n: u32) {
        let mut reversed = 0;
        for i in 0..n {
            reversed |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.put(reversed, n);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

/// Length symbol, extra-bit count and extra value for a match length (3..=258).
fn length_code(length: usize) -> (u32, u32, u32) {
    const BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    let length = length as u16;
    let mut index = 28;
    while BASE[index] > length {
        index -= 1;
    }
    (257 + index as u32, u32::from(EXTRA[index]), u32::from(length - BASE[index]))
}

/// Distance code, extra-bit count and extra value for a match distance (1..=32768).
fn distance_code(distance: usize) -> (u32, u32, u32) {
    const BASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    let distance = distance as u32;
    let mut index = 29;
    while u32::from(BASE[index]) > distance {
        index -= 1;
    }
    (index as u32, u32::from(EXTRA[index]), distance - u32::from(BASE[index]))
}

/// Emits one literal/length symbol of the fixed Huffman code.
fn put_symbol(writer: &mut BitWriter, symbol: u32) {
    match symbol {
        0..=143 => writer.put_code(0x30 + symbol, 8),
        144..=255 => writer.put_code(0x190 + symbol - 144, 9),
        256..=279 => writer.put_code(symbol - 256, 7),
        _ => writer.put_code(0xC0 + symbol - 280, 8),
    }
}

/// Raw deflate with stored blocks only.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65_535 * 5 + 5);
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
        return out;
    }
    let mut chunks = data.chunks(65_535).peekable();
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// Raw deflate: one fixed-Huffman block with greedy LZ77 matching (hash chains of 3-byte prefixes).
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32_768;
    const MAX_CHAIN: usize = 24;
    const HASH_BITS: u32 = 15;
    let mut writer = BitWriter::new();
    writer.put(1, 1); // BFINAL
    writer.put(1, 2); // BTYPE = fixed Huffman
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut previous = vec![usize::MAX; data.len()];
    let hash = |i: usize| -> usize {
        let v = u32::from(data[i]) | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2]) << 16;
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    let mut i = 0;
    while i < data.len() {
        let mut best_length = 0;
        let mut best_distance = 0;
        if i + 3 <= data.len() {
            let h = hash(i);
            let mut candidate = head[h];
            let mut chain = 0;
            let limit = (data.len() - i).min(258);
            while candidate != usize::MAX && i - candidate <= WINDOW && chain < MAX_CHAIN {
                let mut length = 0;
                while length < limit && data[candidate + length] == data[i + length] {
                    length += 1;
                }
                if length > best_length {
                    best_length = length;
                    best_distance = i - candidate;
                    if length == limit {
                        break;
                    }
                }
                candidate = previous[candidate];
                chain += 1;
            }
        }
        let advance = if best_length >= 3 {
            let (symbol, extra_bits, extra) = length_code(best_length);
            put_symbol(&mut writer, symbol);
            if extra_bits > 0 {
                writer.put(extra, extra_bits);
            }
            let (code, extra_bits, extra) = distance_code(best_distance);
            writer.put_code(code, 5);
            if extra_bits > 0 {
                writer.put(extra, extra_bits);
            }
            best_length
        } else {
            put_symbol(&mut writer, u32::from(data[i]));
            1
        };
        // Insert every consumed position into the hash chains.
        for position in i..i + advance {
            if position + 3 <= data.len() {
                let h = hash(position);
                previous[position] = head[h];
                head[h] = position;
            }
        }
        i += advance;
    }
    put_symbol(&mut writer, 256);
    writer.finish()
}

/// zlib stream (RFC 1950): `level` 0 stores, anything else uses fixed-Huffman LZ77.
pub fn zlib_compress(data: &[u8], level: u8) -> Vec<u8> {
    let mut out = if level == 0 { vec![0x78, 0x01] } else { vec![0x78, 0x5E] };
    out.extend_from_slice(&if level == 0 { deflate_stored(data) } else { deflate_fixed(data) });
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

// ---- PNG -----------------------------------------------------------------------------------------------------

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let crc = crc32_update(crc32(kind), payload);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit RGB PNG (`rgb` is `width * height * 3` bytes, row-major). Every row is stored with filter type 0, as the
/// runner's `ppm_to_png` does.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8], level: u8) -> Result<Vec<u8>, String> {
    let stride = width as usize * 3;
    if rgb.len() != stride * height as usize {
        return Err(format!("expected {} RGB bytes for {width}x{height}, got {}", stride * height as usize, rgb.len()));
    }
    let mut scanlines = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks(stride.max(1)).take(height as usize) {
        scanlines.push(0);
        scanlines.extend_from_slice(row);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&SIGNATURE);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib_compress(&scanlines, level));
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// The PNG of an RGBA buffer (alpha is dropped: the LCD frame is opaque).
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8], level: u8) -> Result<Vec<u8>, String> {
    if rgba.len() != width as usize * height as usize * 4 {
        return Err(format!("expected {} RGBA bytes for {width}x{height}, got {}", width as usize * height as usize * 4, rgba.len()));
    }
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    encode_rgb(width, height, &rgb, level)
}

/// The PNG of a binary PPM (`P6\n<w> <h>\n255\n` and RGB triples): the runner's `ppm_to_png`.
pub fn encode_ppm(ppm: &[u8], level: u8) -> Result<Vec<u8>, String> {
    // Header fields are separated by white space: P6 <w> <h> 255 <single whitespace byte> data.
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while at < ppm.len() && ppm[at].is_ascii_whitespace() {
            at += 1;
        }
        let start = at;
        while at < ppm.len() && !ppm[at].is_ascii_whitespace() {
            at += 1;
        }
        if start == at {
            return Err("Unexpected LCD PPM header".to_string());
        }
        fields.push(std::str::from_utf8(&ppm[start..at]).map_err(|_| "Unexpected LCD PPM header".to_string())?);
    }
    if fields[0] != "P6" || fields[3] != "255" || at >= ppm.len() {
        return Err("Unexpected LCD PPM header".to_string());
    }
    let width: u32 = fields[1].parse().map_err(|_| "Unexpected LCD PPM header".to_string())?;
    let height: u32 = fields[2].parse().map_err(|_| "Unexpected LCD PPM header".to_string())?;
    let data = &ppm[at + 1..];
    if data.len() != width as usize * height as usize * 3 {
        return Err("Incomplete LCD snapshot".to_string());
    }
    encode_rgb(width, height, data, level)
}

// ---- decoding (tests, scenario verification) ---------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u32,
    count: u32,
}

impl<'a> BitReader<'a> {
    fn need(&mut self, n: u32) -> Result<(), String> {
        while self.count < n {
            let byte = *self.data.get(self.pos).ok_or("unexpected end of deflate data")?;
            self.pos += 1;
            self.bits |= u32::from(byte) << self.count;
            self.count += 8;
        }
        Ok(())
    }

    fn get(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let value = self.bits & ((1u32 << n) - 1);
        self.bits >>= n;
        self.count -= n;
        Ok(value)
    }
}

/// A canonical Huffman decoder (counts per length and sorted symbols, as in zlib's `puff`).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &length in lengths {
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for i in 1..15 {
            offsets[i + 1] = offsets[i] + counts[i];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize] as usize] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= reader.get(1)? as i32;
            let count = i32::from(self.counts[length]);
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("invalid Huffman code".to_string())
    }
}

/// Inflates a raw deflate stream.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DIST_BASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    let mut reader = BitReader { data, pos: 0, bits: 0, count: 0 };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = reader.get(1)?;
        match reader.get(2)? {
            0 => {
                reader.bits = 0;
                reader.count = 0;
                let header = data.get(reader.pos..reader.pos + 4).ok_or("truncated stored block")?;
                let len = usize::from(u16::from_le_bytes([header[0], header[1]]));
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if nlen != !(len as u16) {
                    return Err("stored block length check failed".to_string());
                }
                reader.pos += 4;
                out.extend_from_slice(data.get(reader.pos..reader.pos + len).ok_or("truncated stored block")?);
                reader.pos += len;
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    let mut lengths = [0u8; 288];
                    for (symbol, length) in lengths.iter_mut().enumerate() {
                        *length = match symbol {
                            0..=143 => 8,
                            144..=255 => 9,
                            256..=279 => 7,
                            _ => 8,
                        };
                    }
                    (Huffman::new(&lengths), Huffman::new(&[5u8; 30]))
                } else {
                    let hlit = reader.get(5)? as usize + 257;
                    let hdist = reader.get(5)? as usize + 1;
                    let hclen = reader.get(4)? as usize + 4;
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut code_lengths = [0u8; 19];
                    for &slot in ORDER.iter().take(hclen) {
                        code_lengths[slot] = reader.get(3)? as u8;
                    }
                    let code_code = Huffman::new(&code_lengths);
                    let mut lengths = vec![0u8; hlit + hdist];
                    let mut index = 0;
                    while index < hlit + hdist {
                        let symbol = code_code.decode(&mut reader)?;
                        match symbol {
                            0..=15 => {
                                lengths[index] = symbol as u8;
                                index += 1;
                            }
                            16 => {
                                let previous = *lengths.get(index.wrapping_sub(1)).ok_or("repeat with no previous length")?;
                                for _ in 0..3 + reader.get(2)? {
                                    *lengths.get_mut(index).ok_or("too many code lengths")? = previous;
                                    index += 1;
                                }
                            }
                            _ => {
                                let repeat = if symbol == 17 { 3 + reader.get(3)? } else { 11 + reader.get(7)? };
                                for _ in 0..repeat {
                                    *lengths.get_mut(index).ok_or("too many code lengths")? = 0;
                                    index += 1;
                                }
                            }
                        }
                    }
                    (Huffman::new(&lengths[..hlit]), Huffman::new(&lengths[hlit..]))
                };
                loop {
                    let symbol = literals.decode(&mut reader)?;
                    match symbol {
                        0..=255 => out.push(symbol as u8),
                        256 => break,
                        257..=285 => {
                            let index = symbol as usize - 257;
                            let length = usize::from(LENGTH_BASE[index]) + reader.get(u32::from(LENGTH_EXTRA[index]))? as usize;
                            let code = distances.decode(&mut reader)? as usize;
                            if code >= 30 {
                                return Err("invalid distance code".to_string());
                            }
                            let distance = usize::from(DIST_BASE[code]) + reader.get(u32::from(DIST_EXTRA[code]))? as usize;
                            if distance > out.len() {
                                return Err("distance reaches before the start".to_string());
                            }
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                        _ => return Err("invalid literal/length symbol".to_string()),
                    }
                }
            }
            _ => return Err("invalid deflate block type".to_string()),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// Decompresses a zlib stream and verifies its header and Adler-32.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 || data[0] & 0x0F != 8 || (u32::from(data[0]) << 8 | u32::from(data[1])) % 31 != 0 || data[1] & 0x20 != 0 {
        return Err("invalid zlib header".to_string());
    }
    let out = inflate(&data[2..data.len() - 4])?;
    let expected = u32::from_be_bytes([data[data.len() - 4], data[data.len() - 3], data[data.len() - 2], data[data.len() - 1]]);
    if adler32(&out) != expected {
        return Err("Adler-32 mismatch".to_string());
    }
    Ok(out)
}

/// Decodes an 8-bit RGB (colour type 2) PNG to `(width, height, rgb)`: every filter type is supported, only
/// non-interlaced images with one or more IDAT chunks. Checks the chunk CRCs.
pub fn decode_rgb(png: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if png.len() < 8 || png[..8] != SIGNATURE {
        return Err("not a PNG file".to_string());
    }
    let mut at = 8;
    let (mut width, mut height) = (0u32, 0u32);
    let mut idat = Vec::new();
    let mut seen_end = false;
    while at + 12 <= png.len() {
        let length = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
        let kind = &png[at + 4..at + 8];
        let payload = png.get(at + 8..at + 8 + length).ok_or("truncated chunk")?;
        let crc = png.get(at + 8 + length..at + 12 + length).ok_or("truncated chunk")?;
        if u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32_update(crc32(kind), payload) {
            return Err(format!("CRC mismatch in {}", String::from_utf8_lossy(kind)));
        }
        match kind {
            b"IHDR" => {
                if length != 13 || payload[8] != 8 || payload[9] != 2 || payload[10] != 0 || payload[11] != 0 || payload[12] != 0 {
                    return Err("unsupported PNG format (need 8-bit RGB, non-interlaced)".to_string());
                }
                width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            }
            b"IDAT" => idat.extend_from_slice(payload),
            b"IEND" => {
                seen_end = true;
                break;
            }
            _ => {}
        }
        at += 12 + length;
    }
    if !seen_end || width == 0 || height == 0 {
        return Err("missing IHDR/IEND".to_string());
    }
    let raw = zlib_decompress(&idat)?;
    let stride = width as usize * 3;
    if raw.len() != (stride + 1) * height as usize {
        return Err("wrong amount of image data".to_string());
    }
    let mut rgb = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let filter = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let left = if x >= 3 { i32::from(rgb[y * stride + x - 3]) } else { 0 };
            let up = if y > 0 { i32::from(rgb[(y - 1) * stride + x]) } else { 0 };
            let up_left = if y > 0 && x >= 3 { i32::from(rgb[(y - 1) * stride + x - 3]) } else { 0 };
            let predictor = match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => (left + up) / 2,
                4 => {
                    let p = left + up - up_left;
                    let (pa, pb, pc) = ((p - left).abs(), (p - up).abs(), (p - up_left).abs());
                    if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        up_left
                    }
                }
                _ => return Err("invalid filter type".to_string()),
            };
            rgb[y * stride + x] = (i32::from(line[x]) + predictor) as u8;
        }
    }
    Ok((width, height, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_the_standard_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_update(crc32(b"1234"), b"56789"), 0xCBF4_3926);
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        let long = vec![0xFFu8; 100_000];
        assert_eq!(adler32(&long), {
            // Reference value computed with the naive definition.
            let (mut a, mut b) = (1u64, 0u64);
            for &x in &long {
                a = (a + u64::from(x)) % 65_521;
                b = (b + a) % 65_521;
            }
            ((b << 16) | a) as u32
        });
    }

    fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn deflate_round_trips_stored_and_fixed_blocks() {
        let mut inputs: Vec<Vec<u8>> = vec![Vec::new(), vec![0], b"abcabcabcabcabcabc".to_vec(), vec![7; 1000], pseudo_random(1, 70_000)];
        // Text-like data with long repeats and a window-sized distance.
        let mut text = Vec::new();
        for i in 0..4000u32 {
            text.extend_from_slice(format!("row {i} of the display; ").as_bytes());
        }
        inputs.push(text);
        let mut mixed = pseudo_random(2, 40_000);
        let copy = mixed[..30_000].to_vec();
        mixed.extend_from_slice(&copy);
        inputs.push(mixed);
        for (index, input) in inputs.iter().enumerate() {
            for level in [0, 1, 9] {
                let packed = zlib_compress(input, level);
                assert_eq!(zlib_decompress(&packed).unwrap(), *input, "input {index} level {level}");
            }
        }
        // Runs compress strongly; random data does not shrink (the fixed code costs at most 9/8).
        assert!(zlib_compress(&vec![7; 100_000], 1).len() < 1200);
        let random = pseudo_random(3, 50_000);
        assert!(zlib_compress(&random, 1).len() < random.len() * 9 / 8 + 16);
    }

    #[test]
    fn png_encoding_decodes_to_the_same_pixels() {
        let (width, height) = (37u32, 23u32);
        let rgb = pseudo_random(9, (width * height * 3) as usize);
        for level in [0, 1] {
            let png = encode_rgb(width, height, &rgb, level).unwrap();
            assert_eq!(&png[..8], &SIGNATURE);
            let (w, h, decoded) = decode_rgb(&png).unwrap();
            assert_eq!((w, h), (width, height));
            assert_eq!(decoded, rgb, "level {level}");
        }
        // A frame of the LCD size with flat areas.
        let mut frame = vec![0u8; 320 * 240 * 3];
        for y in 100..140 {
            for x in 20..200 {
                frame[(y * 320 + x) * 3 + 1] = 0xFF;
            }
        }
        let png = encode_rgb(320, 240, &frame, 1).unwrap();
        assert!(png.len() < 4000, "{}", png.len());
        assert_eq!(decode_rgb(&png).unwrap().2, frame);
        // The size check, and RGBA input.
        assert!(encode_rgb(2, 2, &[0; 11], 1).is_err());
        let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| if i % 4 == 3 { 255 } else { i as u8 }).collect();
        let (_, _, back) = decode_rgb(&encode_rgba(4, 4, &rgba, 1).unwrap()).unwrap();
        assert_eq!(back.len(), 48);
        assert_eq!(back[..3], rgba[..3]);
    }

    #[test]
    fn ppm_conversion_matches_the_runner_helper() {
        let mut ppm = b"P6\n3 2\n255\n".to_vec();
        ppm.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
        let png = encode_ppm(&ppm, 1).unwrap();
        assert_eq!(decode_rgb(&png).unwrap(), (3, 2, ppm[11..].to_vec()));
        assert!(encode_ppm(b"P5\n3 2\n255\nxxxxxxxxxxxxxxxxxx", 1).is_err());
        assert!(encode_ppm(&ppm[..ppm.len() - 1], 1).unwrap_err().contains("Incomplete"));
        assert!(encode_ppm(b"garbage", 1).is_err());
    }

    #[test]
    fn chunk_crc_errors_are_detected() {
        let rgb = vec![10u8; 4 * 4 * 3];
        let mut png = encode_rgb(4, 4, &rgb, 1).unwrap();
        let last = png.len() - 20;
        png[last] ^= 0xFF;
        assert!(decode_rgb(&png).is_err());
    }
}
