// Minimal ZIP support: a STORE-method writer (evidence captures, profile export) and a reader for STORE and
// DEFLATE entries (profile import). No dependencies; usable in browsers, workers and Node.
//
// Limits (reported as errors): no ZIP64 (archives and entries below 4 GiB, fewer than 65 536 entries),
// no encryption, no multi-disk archives.

const TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

/** CRC-32 (IEEE 802.3, the ZIP/PNG polynomial) of `data`, optionally continuing `previous`. */
export function crc32(data, previous = 0) {
  let c = (previous ^ 0xffffffff) >>> 0;
  for (let i = 0; i < data.length; i++) c = TABLE[(c ^ data[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

const encoder = new TextEncoder();

function dosDateTime(date) {
  const year = Math.min(2107, Math.max(1980, date.getFullYear()));
  return {
    time: (date.getHours() << 11) | (date.getMinutes() << 5) | (date.getSeconds() >> 1),
    date: ((year - 1980) << 9) | ((date.getMonth() + 1) << 5) | date.getDate(),
  };
}

function toBytes(data) {
  if (typeof data === 'string') return encoder.encode(data);
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  throw new TypeError('zip entry data must be a string, Uint8Array or ArrayBuffer');
}

/**
 * Builds a ZIP archive with every entry stored (no compression).
 * @param {{name: string, data: string|Uint8Array|ArrayBuffer}[]} entries names use "/" and are UTF-8
 * @param {{date?: Date}} options modification time of all entries (default: now)
 * @returns {Uint8Array}
 */
export function makeZip(entries, { date = new Date() } = {}) {
  if (entries.length > 0xffff) throw new Error('too many zip entries');
  const stamp = dosDateTime(date);
  const prepared = entries.map((entry) => {
    const name = encoder.encode(entry.name);
    const data = toBytes(entry.data);
    if (name.length > 0xffff) throw new Error(`zip entry name too long: ${entry.name}`);
    if (data.length >= 0xffffffff) throw new Error(`zip entry too large: ${entry.name}`);
    return { name, data, crc: crc32(data) };
  });
  let size = 22;
  for (const entry of prepared) size += 30 + entry.name.length + entry.data.length + 46 + entry.name.length;
  if (size >= 0xffffffff) throw new Error('zip archive too large');
  const out = new Uint8Array(size);
  const view = new DataView(out.buffer);
  let at = 0;
  const offsets = [];
  const u16 = (value) => { view.setUint16(at, value, true); at += 2; };
  const u32 = (value) => { view.setUint32(at, value >>> 0, true); at += 4; };
  const bytes = (value) => { out.set(value, at); at += value.length; };
  for (const entry of prepared) {
    offsets.push(at);
    u32(0x04034b50);
    u16(20); // version needed
    u16(0x0800); // flags: UTF-8 names
    u16(0); // method: stored
    u16(stamp.time);
    u16(stamp.date);
    u32(entry.crc);
    u32(entry.data.length);
    u32(entry.data.length);
    u16(entry.name.length);
    u16(0);
    bytes(entry.name);
    bytes(entry.data);
  }
  const directoryStart = at;
  prepared.forEach((entry, index) => {
    u32(0x02014b50);
    u16(0x0314); // made by: Unix, version 2.0
    u16(20);
    u16(0x0800);
    u16(0);
    u16(stamp.time);
    u16(stamp.date);
    u32(entry.crc);
    u32(entry.data.length);
    u32(entry.data.length);
    u16(entry.name.length);
    u16(0); // extra
    u16(0); // comment
    u16(0); // disk
    u16(0); // internal attributes
    u32(0o100644 << 16); // external attributes: -rw-r--r--
    u32(offsets[index]);
    bytes(entry.name);
  });
  const directorySize = at - directoryStart;
  u32(0x06054b50);
  u16(0);
  u16(0);
  u16(prepared.length);
  u16(prepared.length);
  u32(directorySize);
  u32(directoryStart);
  u16(0);
  return out;
}

async function inflateRaw(compressed) {
  if (typeof DecompressionStream === 'undefined') {
    throw new Error('this browser cannot decompress ZIP entries (DecompressionStream is missing); re-create the archive without compression');
  }
  const stream = new Blob([compressed]).stream().pipeThrough(new DecompressionStream('deflate-raw'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/**
 * Reads a ZIP archive (STORE and DEFLATE entries). Directory entries and macOS metadata are skipped; sizes and
 * CRC-32 are verified.
 * @param {Uint8Array} bytes
 * @returns {Promise<{name: string, data: Uint8Array}[]>}
 */
export async function readZip(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const decoder = new TextDecoder('utf-8');
  let end = -1;
  for (let at = bytes.length - 22; at >= Math.max(0, bytes.length - 22 - 0xffff); at--) {
    if (view.getUint32(at, true) === 0x06054b50) {
      end = at;
      break;
    }
  }
  if (end < 0) throw new Error('not a ZIP archive (end of central directory not found)');
  const entryCount = view.getUint16(end + 10, true);
  const directorySize = view.getUint32(end + 12, true);
  const directoryStart = view.getUint32(end + 16, true);
  if (view.getUint16(end + 4, true) !== 0 || view.getUint16(end + 6, true) !== 0) throw new Error('multi-disk ZIP archives are not supported');
  if (entryCount === 0xffff || directorySize === 0xffffffff || directoryStart === 0xffffffff) throw new Error('ZIP64 archives are not supported');
  if (directoryStart + directorySize > bytes.length) throw new Error('corrupt ZIP archive (central directory out of range)');
  const entries = [];
  let at = directoryStart;
  for (let index = 0; index < entryCount; index++) {
    if (at + 46 > bytes.length || view.getUint32(at, true) !== 0x02014b50) throw new Error('corrupt ZIP archive (bad central directory entry)');
    const flags = view.getUint16(at + 8, true);
    const method = view.getUint16(at + 10, true);
    const crc = view.getUint32(at + 16, true);
    const compressedSize = view.getUint32(at + 20, true);
    const size = view.getUint32(at + 24, true);
    const nameLength = view.getUint16(at + 28, true);
    const extraLength = view.getUint16(at + 30, true);
    const commentLength = view.getUint16(at + 32, true);
    const localOffset = view.getUint32(at + 42, true);
    const name = decoder.decode(bytes.subarray(at + 46, at + 46 + nameLength));
    at += 46 + nameLength + extraLength + commentLength;
    if (name.endsWith('/') || name.startsWith('__MACOSX/') || name.split('/').pop() === '.DS_Store') continue;
    if (flags & 1) throw new Error(`encrypted ZIP entry ${name} is not supported`);
    if (compressedSize === 0xffffffff || size === 0xffffffff || localOffset === 0xffffffff) throw new Error('ZIP64 archives are not supported');
    if (localOffset + 30 > bytes.length || view.getUint32(localOffset, true) !== 0x04034b50) throw new Error(`corrupt ZIP archive (bad local header for ${name})`);
    const dataStart = localOffset + 30 + view.getUint16(localOffset + 26, true) + view.getUint16(localOffset + 28, true);
    if (dataStart + compressedSize > bytes.length) throw new Error(`corrupt ZIP archive (${name} is truncated)`);
    const raw = bytes.subarray(dataStart, dataStart + compressedSize);
    let data;
    if (method === 0) data = raw.slice();
    else if (method === 8) data = await inflateRaw(raw);
    else throw new Error(`ZIP entry ${name} uses unsupported compression method ${method}`);
    if (data.length !== size) throw new Error(`corrupt ZIP entry ${name} (size mismatch)`);
    if (crc32(data) !== crc) throw new Error(`corrupt ZIP entry ${name} (CRC-32 mismatch)`);
    entries.push({ name, data });
  }
  return entries;
}
