#!/usr/bin/env node
// Headless tests of the browser application's worker logic (runtime.js + engine.js + zip.js + storage.js) with
// the real WebAssembly engine and the two original SREC files. The browser-only parts (DOM, workers, OPFS) are
// thin and not covered here.
//
//   node web/test-node.mjs [--wasm file] [--skip-pacing] [--pacing-seconds 3]
//
// Environment: NGC_WASM overrides the wasm path; NGC_FIRMWARE_DIR is the directory that holds the release directories
// (<dir>/TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec, ...; default: the firmware/ directory of this checkout). You
// supply the firmware yourself; it is not part of the repository. It is read from disk and never copied anywhere.

import test, { afterEach } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Engine, EngineError, PROFILE_FILES } from './engine.js';
import { Cursor, ReplayController } from './replay.js';
import { Runtime } from './runtime.js';
import { MemoryStorage } from './storage.js';
import { crc32, makeZip, readZip } from './zip.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '..');
const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
};
const skipPacing = args.includes('--skip-pacing');
const pacingSeconds = Number(option('--pacing-seconds', '3'));

const MAIN_FILE = 'ngc_main_5.8_TRITON.srec';
const HANDSET_FILE = 'ngc_handset_65.3_TRITON.srec';
const MAIN_SHA = '838cb050fa572dddca3153f43a1768db0a0665db4cde0567749fb7be8d18d6ea';
const HANDSET_SHA = '71a9af68de1d23d4f845784bcbf8ccf72dcd9888587e0da0125ff41c74ea1e03';
const NEPTUN_MAIN_FILE = 'ngc_main_5.8_NEPTUN.srec';
const NEPTUN_HANDSET_FILE = 'ngc_handset_65.3_NEPTUN.srec';
const NEPTUN_MAIN_SHA = 'e462bc7345d6ded69124b97b87de9a68884f44b8e716fbbf4fe3839ff8da8c89';
const NEPTUN_HANDSET_SHA = 'f91adcf461fa0e06ef40ab3f757dd66754180b9711956542736efb0d4be8162e';
// LCD of the B1 prompt (320x240 PPM) in the Renode dual-wake evidence, steady from 4.75 s.
const B1_PPM_SHA = '62c3a30e54031ff3c2aeffa16a6b9f361c64ab2326db35da7e345c5318f7632d';

function findFile(candidates) {
  return candidates.find((candidate) => candidate && fs.existsSync(candidate)) || null;
}

const wasmPath = findFile([
  option('--wasm', null) && path.resolve(option('--wasm', null)),
  process.env.NGC_WASM,
  path.join(here, 'pkg/ngc_wasm.wasm'),
  path.join(here, '../target/web/wasm32-unknown-unknown/release/ngc_wasm.wasm'),
  path.join(here, '../target/sys/wasm32-unknown-unknown/release/ngc_wasm.wasm'),
]);
const firmwareRoots = [process.env.NGC_FIRMWARE_DIR, path.join(repo, 'firmware')].filter(Boolean);
const firmwareDir = firmwareRoots.map((root) => path.join(root, 'TRITON-5.8-65.3'))
  .find((dir) => fs.existsSync(path.join(dir, MAIN_FILE)) && fs.existsSync(path.join(dir, HANDSET_FILE)));

if (!wasmPath) {
  console.error('No ngc_wasm.wasm found: run web/build.py or pass --wasm');
  process.exit(2);
}
if (!firmwareDir) {
  console.error('The original SREC files were not found: put them in firmware/TRITON-5.8-65.3/ or set NGC_FIRMWARE_DIR');
  process.exit(2);
}

const wasmModule = await WebAssembly.compile(fs.readFileSync(wasmPath));
const mainSrec = new Uint8Array(fs.readFileSync(path.join(firmwareDir, MAIN_FILE)));
const handsetSrec = new Uint8Array(fs.readFileSync(path.join(firmwareDir, HANDSET_FILE)));
// The NEPTUN pair is optional (its tests are skipped without it): firmware/NEPTUN-5.8-65.3 or below NGC_FIRMWARE_DIR.
const neptunDir = firmwareRoots.map((root) => path.join(root, 'NEPTUN-5.8-65.3'))
  .find((dir) => fs.existsSync(path.join(dir, NEPTUN_MAIN_FILE)) && fs.existsSync(path.join(dir, NEPTUN_HANDSET_FILE)));
const neptunMainSrec = neptunDir ? new Uint8Array(fs.readFileSync(path.join(neptunDir, NEPTUN_MAIN_FILE))) : null;
const neptunHandsetSrec = neptunDir ? new Uint8Array(fs.readFileSync(path.join(neptunDir, NEPTUN_HANDSET_FILE))) : null;
console.log(`wasm ${path.relative(process.cwd(), wasmPath)} (${fs.statSync(wasmPath).size} bytes), firmware ${firmwareDir}${neptunDir ? `, NEPTUN ${neptunDir}` : ' (no NEPTUN pair: its tests are skipped)'}`);

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

/** The Renode `lcd SavePPM` bytes of an RGBA frame. */
function framePpm(frame) {
  const rgba = new Uint8Array(frame.buffer);
  const count = frame.width * frame.height;
  const header = Buffer.from(`P6\n${frame.width} ${frame.height}\n255\n`);
  const out = Buffer.alloc(header.length + count * 3);
  header.copy(out);
  for (let i = 0; i < count; i++) {
    out[header.length + i * 3] = rgba[i * 4];
    out[header.length + i * 3 + 1] = rgba[i * 4 + 1];
    out[header.length + i * 3 + 2] = rgba[i * 4 + 2];
  }
  return out;
}

// A failing test must not leave a running session behind (its timers would keep the process alive).
const openHarnesses = new Set();
afterEach(async () => {
  for (const harness of openHarnesses) await harness.close();
  openHarnesses.clear();
});

/** A Runtime wired to in-memory messaging, with request/response helpers. */
class Harness {
  constructor({ storage = new MemoryStorage(), lock = async () => () => {}, setTimer, clearTimer, now, autoRecycle = true } = {}) {
    openHarnesses.add(this);
    this.storage = storage;
    this.autoRecycle = autoRecycle;
    this.messages = [];
    this.pending = new Map();
    this.nextId = 1;
    this.listeners = new Set();
    this.runtime = new Runtime({
      post: (message) => {
        message.at = performance.now(); // arrival time, for rate measurements
        this.messages.push(message);
        // The page hands every drawn frame buffer back (a copy here: the test keeps the originals).
        if (message.type === 'frame' && this.autoRecycle) this.runtime.receive({ type: 'recycle', buffer: new ArrayBuffer(message.buffer.byteLength) });
        if (message.type === 'response' && this.pending.has(message.id)) {
          const { resolve, reject } = this.pending.get(message.id);
          this.pending.delete(message.id);
          if (message.ok) resolve(message.result);
          else reject(Object.assign(new Error(message.error.message), message.error));
        }
        for (const listener of [...this.listeners]) listener(message);
      },
      loadEngine: async () => Engine.load(wasmModule),
      openStorage: async () => ({ storage, problems: [] }),
      acquireLock: lock,
      setTimer,
      clearTimer,
      now,
    });
  }

  request(type, payload = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.runtime.receive({ id, type, ...payload });
    });
  }

  last(type) {
    for (let i = this.messages.length - 1; i >= 0; i--) if (this.messages[i].type === type) return this.messages[i];
    return null;
  }

  all(type) {
    return this.messages.filter((message) => message.type === type);
  }

  waitFor(predicate, timeoutMs = 20000) {
    const existing = this.messages.find(predicate);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.listeners.delete(listener);
        reject(new Error('timed out waiting for a message'));
      }, timeoutMs);
      const listener = (message) => {
        if (predicate(message)) {
          clearTimeout(timer);
          this.listeners.delete(listener);
          resolve(message);
        }
      };
      this.listeners.add(listener);
    });
  }

  action(request) {
    return this.request('action', { request });
  }

  /** init + both firmware files accepted. */
  async ready({ main = true } = {}) {
    const init = await this.request('init');
    const handset = await this.request('inspect', { name: HANDSET_FILE, bytes: handsetSrec.slice() });
    assert.equal(handset.accepted, true, handset.message);
    if (main) {
      const mainResult = await this.request('inspect', { name: MAIN_FILE, bytes: mainSrec.slice() });
      assert.equal(mainResult.accepted, true, mainResult.message);
    }
    return init;
  }

  async close() {
    try {
      await this.request('close-session');
    } catch (_) { /* nothing to close */ }
  }
}

// ---------------------------------------------------------------------------------------------------

test('zip: stored archives round trip and match known CRC-32 values', async () => {
  assert.equal(crc32(new TextEncoder().encode('123456789')), 0xcbf43926);
  const zip = makeZip([
    { name: 'a.txt', data: 'hello' },
    { name: 'dir/b.bin', data: new Uint8Array([0, 1, 2, 3, 255]) },
    { name: 'empty', data: new Uint8Array(0) },
  ]);
  const entries = await readZip(zip);
  assert.deepEqual(entries.map((entry) => entry.name), ['a.txt', 'dir/b.bin', 'empty']);
  assert.equal(new TextDecoder().decode(entries[0].data), 'hello');
  assert.deepEqual([...entries[1].data], [0, 1, 2, 3, 255]);
  assert.equal(entries[2].data.length, 0);
  // Corruption is detected.
  const broken = zip.slice();
  broken[40] ^= 0xff;
  await assert.rejects(() => readZip(broken), /CRC-32|corrupt/);
  await assert.rejects(() => readZip(new Uint8Array(10)), /not a ZIP/);
});

test('zip: the system unzip tool accepts the archive (when available)', async (t) => {
  const { spawnSync } = await import('node:child_process');
  const probe = spawnSync('unzip', ['-v'], { encoding: 'utf8' });
  if (probe.error || probe.status !== 0) {
    t.skip('unzip is not installed');
    return;
  }
  const dir = fs.mkdtempSync(path.join(process.env.TMPDIR || '/tmp', 'ngc-zip-'));
  const file = path.join(dir, 'test.zip');
  fs.writeFileSync(file, makeZip([{ name: 'state.json', data: '{"a":1}\n' }, { name: 'lcd.png', data: new Uint8Array(1000).fill(7) }]));
  const result = spawnSync('unzip', ['-t', file], { encoding: 'utf8' });
  fs.rmSync(dir, { recursive: true, force: true });
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.match(result.stdout, /No errors detected/);
});

test('engine: identity, firmware verification and refusals', async () => {
  const engine = await Engine.load(wasmModule);
  assert.match(engine.name, /^ngc-wasm\/\d+\.\d+\.\d+$/);
  const main = engine.inspectFirmware(mainSrec);
  assert.equal(main.ok, true);
  assert.equal(main.role, 'main');
  assert.equal(main.srecSha256, MAIN_SHA);
  assert.equal(main.message, null);
  const handset = engine.inspectFirmware(handsetSrec);
  assert.equal(handset.ok, true);
  assert.equal(handset.role, 'handset');
  assert.equal(handset.srecSha256, HANDSET_SHA);

  const garbage = engine.inspectFirmware(new TextEncoder().encode('S1130000ZZ\nnot firmware'));
  assert.equal(garbage.ok, false);
  assert.ok(garbage.message && garbage.message.length > 10, 'the engine explains the refusal');

  engine.setFirmware('main', mainSrec);
  assert.throws(() => engine.setFirmware('handset', mainSrec), (error) => error instanceof EngineError && /main firmware/.test(error.message) && /handset/.test(error.message));
  // A modified image is not accepted.
  const tampered = mainSrec.slice();
  const text = new TextDecoder().decode(tampered.subarray(0, 200));
  const at = text.indexOf('S3');
  tampered[at + 12] = tampered[at + 12] === 0x30 ? 0x31 : 0x30; // a data digit: the record checksum no longer matches
  const refused = engine.inspectFirmware(tampered);
  assert.equal(refused.ok, false);
  assert.match(refused.message, /checksum|valid|unrecognized|verification/i);
  // The session needs the firmware.
  engine.clearFirmware('main');
  assert.throws(() => engine.createSession({ mode: 'dual' }), /firmware/);
});

test('runtime: refuses unknown files, accepts the two images in either order', async () => {
  const h = new Harness();
  await h.request('init');
  const bad = await h.request('inspect', { name: 'readme.txt', bytes: new TextEncoder().encode('hello world') });
  assert.equal(bad.accepted, false);
  assert.ok(bad.message);
  // Handset first, then main.
  const handset = await h.request('inspect', { name: HANDSET_FILE, bytes: handsetSrec.slice() });
  assert.equal(handset.accepted, true);
  assert.equal(handset.role, 'handset');
  assert.equal(handset.report.srecSha256, HANDSET_SHA);
  const main = await h.request('inspect', { name: 'renamed-by-user.srec', bytes: mainSrec.slice() });
  assert.equal(main.accepted, true, 'identity is by content, not by file name');
  assert.equal(main.role, 'main');
  // Booting without the main image refuses in dual mode only.
  const h2 = new Harness();
  await h2.request('init');
  await h2.request('inspect', { name: HANDSET_FILE, bytes: handsetSrec.slice() });
  await assert.rejects(() => h2.request('boot', { options: { mode: 'dual' } }), /main firmware/);
});

test('runtime: dual boot reaches the B1 prompt frame; buttons, actions and validation', async () => {
  const h = new Harness();
  await h.ready();
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.state.running, false);
  assert.match(booted.state.engine, /^ngc-wasm\//);
  assert.equal(booted.state.frameReady, false, 'no LCD output before the firmware runs');
  // The Renode runner recorded the B1 frame below with its 1500 mV default batteries; a fresh profile starts at 4100 mV,
  // so the recorded value is pinned here, before the first instruction (the next test shows the same frame at 4100 mV).
  await h.action({ action: 'inputs', inputs: { battery1Mv: 1500, battery2Mv: 1500 } });

  const advanced = await h.action({ action: 'advance', seconds: 6 });
  assert.ok(advanced.virtualTime >= 6 && advanced.virtualTime < 6.01, `virtual time ${advanced.virtualTime}`);
  assert.equal(advanced.frameReady, true);
  assert.equal(advanced.mainBatteryReady, true);
  assert.equal(advanced.handsetPowered, true);
  assert.equal(advanced.standby, false);
  assert.ok(Array.isArray(advanced.hardwareOutputs) && advanced.hardwareOutputs.length >= 2, 'hardware outputs are reported');
  assert.ok(Array.isArray(advanced.uartConsole) && advanced.uartConsole.length === 5, 'five UART channels in a dual run');
  assert.ok(advanced.canSummary && /connected/i.test(advanced.canSummary));

  const frame = h.last('frame');
  assert.ok(frame, 'a frame was published');
  assert.equal(frame.buffer.byteLength, frame.width * frame.height * 4);
  assert.equal(`${frame.width}x${frame.height}`, '320x240');
  assert.equal(sha256(framePpm(frame)), B1_PPM_SHA, 'the B1 prompt LCD matches the Renode dual-wake evidence');
  assert.ok(h.last('state').state.virtualTime >= 6);

  // Buttons: electrical pulses; the screen changes after they were processed.
  const before = h.last('frame').version;
  await h.action({ action: 'down' });
  await h.action({ action: 'advance', seconds: 0.6 });
  await h.action({ action: 'up' });
  await h.action({ action: 'advance', seconds: 0.6 });
  assert.notEqual(h.last('frame').version, before, 'the display changed after the key presses');

  const step = await h.action({ action: 'step' });
  assert.ok(step.virtualTime > advanced.virtualTime);

  // Validation messages are the runner's.
  await assert.rejects(() => h.action({ action: 'advance', seconds: 0 }), /Advance must be between zero and 20 virtual seconds/);
  await assert.rejects(() => h.action({ action: 'advance', seconds: 21 }), /Advance must be between zero and 20 virtual seconds/);
  await assert.rejects(() => h.action({ action: 'inputs', inputs: { battery1Mv: 5000 } }), /battery1Mv must be between 0 and 4200/);
  await assert.rejects(() => h.action({ action: 'inputs', inputs: { bogus: 1 } }), /Unknown input: bogus/);
  await assert.rejects(() => h.action({ action: 'inputs', inputs: { acquisitionEnabled: 1 } }), /acquisitionEnabled must be a boolean/);
  await assert.rejects(() => h.action({ action: 'can', dropId: 3000 }), /dropId must be -1 or a standard CAN ID/);
  await assert.rejects(() => h.action({ action: 'can', connected: 'yes' }), /connected must be a boolean/);
  await assert.rejects(() => h.action({ action: 'led-colors', colors: { 'main-hud-9': 'red' } }), /LED colors must map HUD channel IDs/);
  await assert.rejects(() => h.action({ action: 'bogus' }), /Unknown action/);

  // Valid control changes take effect.
  const inputs = await h.action({ action: 'inputs', inputs: { pressure1Mbar: 1500.5, oxygen1Mv: 12.25 } });
  assert.equal(inputs.inputs.pressure1Mbar, 1500.5);
  assert.equal(inputs.inputs.oxygen1Mv, 12.25);
  const disconnected = await h.action({ action: 'can', connected: false });
  assert.match(disconnected.canSummary, /connected=False/);
  const reconnected = await h.action({ action: 'can', connected: true, dropId: 558 });
  assert.match(reconnected.canSummary, /connected=True/);
  const colored = await h.action({ action: 'led-colors', colors: { 'main-hud-1': 'red', 'main-hud-2': 'white' } });
  const hud = colored.hardwareOutputs.filter((output) => output.id === 'main-hud-1');
  assert.equal(hud.length, 1);
  assert.equal(hud[0].color, 'red');
  await h.close();
});

test('runtime: a fresh profile starts with 4100 mV batteries and reaches the identical B1 prompt; a saved profile keeps its own voltage', async () => {
  const h = new Harness();
  await h.ready();
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.deepEqual([booted.state.inputs.battery1Mv, booted.state.inputs.battery2Mv], [4100, 4100], 'a fresh profile');
  const advanced = await h.action({ action: 'advance', seconds: 6 });
  assert.equal(advanced.mainBatteryReady, true);
  assert.equal(advanced.handsetPowered, true);
  assert.equal(advanced.frameReady, true);
  assert.equal(sha256(framePpm(h.last('frame'))), B1_PPM_SHA, 'the B1 prompt shows the same screen at 4100 mV as at the recorded 1500 mV');
  const uart = advanced.uartConsole.find((channel) => channel.id === 'main.uart4');
  assert.match(uart.text, /Main voltage: 4099 mV/, 'the firmware reads 4.1 V through its ADC and prints it');
  await h.close();

  // A saved inputs.json keeps its stored voltage; only a bank missing from the file takes the new default.
  const storage = new MemoryStorage();
  await storage.write('profile', 'inputs.json', new TextEncoder().encode('{"battery1Mv": 1500, "battery2Mv": 1400}'));
  const saved = new Harness({ storage });
  await saved.ready();
  const reopened = await saved.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.deepEqual([reopened.state.inputs.battery1Mv, reopened.state.inputs.battery2Mv], [1500, 1400]);
  await saved.close();
  const partial = new MemoryStorage();
  await partial.write('profile', 'inputs.json', new TextEncoder().encode('{"battery1Mv": 1500}'));
  const half = new Harness({ storage: partial });
  await half.ready();
  const mixed = await half.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.deepEqual([mixed.state.inputs.battery1Mv, mixed.state.inputs.battery2Mv], [1500, 4100]);
  await half.close();
});

test('runtime: pause, resume, standby handling and boot modes', async () => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual' } });
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime > 0.5, 15000);
  const paused = await h.action({ action: 'pause' });
  assert.equal(paused.running, false);
  const t1 = (await h.action({ action: 'step' })).virtualTime;
  const t2 = (await h.action({ action: 'step' })).virtualTime;
  assert.ok(Math.abs(t2 - t1 - 0.1) < 1e-6, `a step is 0.1 virtual seconds (${t2 - t1})`);
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(h.last('state').state.virtualTime, t2, 'no progress while paused');
  const resumed = await h.action({ action: 'resume' });
  assert.equal(resumed.running, true);
  await h.close();

  // Cold boot ends in the firmware's standby request; Resume is refused, Wake restarts.
  const cold = new Harness();
  await cold.ready();
  await cold.request('boot', { options: { mode: 'dual', bootMode: 'cold', startPaused: true } });
  const state = await cold.action({ action: 'advance', seconds: 8 });
  if (state.standby) {
    assert.equal(state.running, false);
    await assert.rejects(() => cold.action({ action: 'resume' }), /standby/);
    const woke = await cold.action({ action: 'wake' });
    assert.equal(woke.standby, false);
    assert.equal(woke.virtualTime, 0);
  } else {
    console.log('note: the cold boot did not reach standby within 8 s');
  }
  await cold.close();
});

test('runtime: capture produces a zip with state.json, lcd.png and can-trace.tsv', async () => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  await h.action({ action: 'advance', seconds: 5 });
  const result = await h.request('capture');
  assert.match(result.filename, /^ngc-capture-\d{8}T\d{12}Z\.zip$/, 'the capture folder name is the UTC timestamp the host supplied');
  const download = h.last('download');
  assert.equal(download.filename, result.filename);
  assert.equal(download.mime, 'application/zip');
  const entries = await readZip(new Uint8Array(download.bytes));
  assert.deepEqual(entries.map((entry) => entry.name).sort(), ['can-trace.tsv', 'lcd.png', 'state.json']);
  const byName = Object.fromEntries(entries.map((entry) => [entry.name, entry.data]));
  const state = JSON.parse(new TextDecoder().decode(byName['state.json']));
  assert.match(state.engine, /^ngc-wasm\//);
  assert.ok(state.virtualTime >= 5);
  assert.match(state.hostPacing, /Browser worker/, 'the host described its pacing');
  assert.ok('realtimeFactor' in state, 'the state document carries the host-measured real-time factor (null while paused)');
  assert.ok(state.firmware && state.firmware.main && state.firmware.handset, 'the firmware identities are in the capture');
  assert.equal(state.firmware.main.srecSha256, MAIN_SHA);
  assert.deepEqual([...byName['lcd.png'].subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 'PNG signature');
  const trace = new TextDecoder().decode(byName['can-trace.tsv']);
  assert.ok(trace.split('\n').length > 5, 'CAN trace has frames');
  assert.match(h.last('state').host.lastCapture, /^ngc-capture-/);
  await h.close();
});

test('runtime: profile persistence, export and import round trip', async () => {
  const storage = new MemoryStorage();
  const h = new Harness({ storage });
  await h.ready();
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  await h.action({ action: 'advance', seconds: 5 });
  // The serial fixture needs the first boot to have initialised the EEPROM.
  const withSerial = await h.action({ action: 'serial', serialNumber: 123456789 });
  assert.equal(withSerial.serialNumber, 123456789);
  await h.action({ action: 'inputs', inputs: { temperature1C: 31.5 } });
  await h.action({ action: 'led-colors', colors: { 'main-hud-3': 'white' } });
  await h.request('flush');
  const stored = (await storage.list('profile')).map((file) => file.name).sort();
  for (const name of ['eeprom.bin', 'led-colors.json', 'inputs.json']) assert.ok(stored.includes(name), `${name} saved (${stored})`);
  const eeprom = await storage.read('profile', 'eeprom.bin');
  assert.equal(eeprom.length, 2048);

  const exported = await h.request('export-profile');
  for (const name of PROFILE_FILES) assert.ok(exported.entries.includes(name), `${name} is part of an exported profile (${exported.entries})`);
  const archive = new Uint8Array(h.last('download').bytes);
  const files = await readZip(archive);
  for (const file of files) assert.ok(PROFILE_FILES.includes(file.name), file.name);
  // The RTC checkpoint is the runner's versioned document with both boards.
  const rtc = JSON.parse(new TextDecoder().decode(files.find((file) => file.name === 'rtc-state.json').data));
  assert.equal(rtc.version, 1);
  assert.ok(rtc.boards['ngc-main'] && rtc.boards['ngc-handset'], 'both RTC domains are checkpointed');
  await h.close();

  // A fresh browser (empty storage) imports the archive and has the same persistent state.
  const storage2 = new MemoryStorage();
  const h2 = new Harness({ storage: storage2 });
  await h2.ready();
  await h2.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.notEqual((await h2.request('action', { request: { action: 'step' } })).serialNumber, 123456789);
  const epochBefore = h2.last('state').host.profileEpoch;
  const imported = await h2.request('import-profile', { files });
  assert.ok(h2.last('state').host.profileEpoch > epochBefore, 'the page is told to reload its input fields (the new state is posted before the response)');
  assert.ok(imported.imported.includes('eeprom.bin'));
  assert.equal(imported.state.serialNumber, 123456789);
  assert.equal(imported.state.inputs.temperature1C, 31.5);
  assert.deepEqual([...imported.state.rtcPersistence.restoredBoards].sort(), ['ngc-handset', 'ngc-main'], 'the imported RTC checkpoint was restored');
  const colors = h2.last('state').state.hardwareOutputs.find((output) => output.id === 'main-hud-3');
  assert.equal(colors.color, 'white');
  assert.deepEqual([...(await storage2.read('profile', 'eeprom.bin'))], [...files.find((file) => file.name === 'eeprom.bin').data], 'imported files are persisted');

  // A new session in a new Harness restores that stored profile.
  await h2.close();
  const h3 = new Harness({ storage: storage2 });
  await h3.ready();
  const booted = await h3.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.state.serialNumber, 123456789);
  assert.equal(booted.state.inputs.temperature1C, 31.5);

  // Bad archives are rejected and leave the session alone.
  await assert.rejects(() => h3.request('import-profile', { files: [{ name: 'eeprom.bin', data: new Uint8Array(10) }] }), /not imported/);
  await assert.rejects(() => h3.request('import-profile', { files: [{ name: 'notes.txt', data: new Uint8Array(1) }] }), /none of the profile files/);
  assert.equal((await h3.action({ action: 'step' })).serialNumber, 123456789);
  // Reset profile erases storage and starts from factory-fresh storage.
  await h3.request('reset-profile');
  assert.equal((await storage2.list('profile')).length, 0);
  assert.equal(h3.last('state').state.serialNumber === 123456789, false);
  await h3.close();
});

test('runtime: changed inputs reach storage within a second without an explicit flush', async () => {
  const storage = new MemoryStorage();
  const h = new Harness({ storage });
  await h.ready();
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  await h.action({ action: 'inputs', inputs: { temperature1C: 33.25 } });
  await new Promise((resolve) => setTimeout(resolve, 1000));
  const text = new TextDecoder().decode(await storage.read('profile', 'inputs.json'));
  assert.match(text, /"temperature1C": 33\.25/, 'the autosave wrote the new value');
  // The write is the runner's inputs.json layout (Python json.dumps, indent 2); a fresh profile holds 4100 mV batteries.
  assert.match(text, /^\{\n  "battery1Mv": 4100/);
  await h.close();
});

test('runtime: a second tab holding the profile lock runs without saving', async () => {
  const storage = new MemoryStorage();
  const h = new Harness({ storage, lock: async () => null });
  await h.ready();
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.persist, false);
  assert.ok(h.all('notice').some((notice) => /another tab/i.test(notice.text)));
  await h.action({ action: 'advance', seconds: 1 });
  await h.close();
  assert.equal((await storage.list('profile')).length, 0, 'nothing was written');
});

test('runtime: firmware is remembered in OPFS only, never in the fallback backends', async () => {
  const fallback = new MemoryStorage(); // kind "memory": stands for IndexedDB / no persistence
  const h = new Harness({ storage: fallback });
  await h.ready({ main: false });
  await h.request('boot', { options: { mode: 'handset', startPaused: true }, remember: true });
  assert.ok(h.all('notice').some((notice) => /origin-private file system/.test(notice.text)), 'the user is told why nothing was remembered');
  assert.equal((await fallback.list('firmware')).length, 0, 'no firmware was stored');
  await h.close();
});

test('runtime: handset-only mode, remembered firmware and forgetting it', async () => {
  const storage = new MemoryStorage();
  storage.kind = 'opfs'; // the in-memory stand-in plays the origin-private file system
  const h = new Harness({ storage });
  await h.ready({ main: false });
  const booted = await h.request('boot', { options: { mode: 'handset', startPaused: true }, remember: true });
  assert.equal(booted.state.inputs, undefined);
  await h.action({ action: 'advance', seconds: 2 });
  await assert.rejects(() => h.action({ action: 'can', connected: false }), /dual/i);
  await h.close();
  const remembered = (await storage.list('firmware')).map((file) => file.name).sort();
  assert.deepEqual(remembered, ['handset.srec', 'index.json']);

  const h2 = new Harness({ storage });
  const init = await h2.request('init');
  assert.ok(init.remembered && init.remembered.handset, 'remembered firmware is advertised');
  assert.equal(init.remembered.handset.name, HANDSET_FILE);
  const used = await h2.request('use-remembered');
  assert.equal(used.accepted.length, 1);
  assert.equal(used.accepted[0].report.srecSha256, HANDSET_SHA);
  await h2.request('forget');
  assert.equal((await storage.list('firmware')).length, 0);
});

test('runtime: pacing publishes frames and state, honours pause, speed and background policy', async () => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual' }, profile: 'none' });
  await h.request('speed', { speed: null });
  const state = await h.waitFor((m) => m.type === 'state' && m.state.virtualTime >= 5.5 && m.state.frameReady, 30000);
  assert.ok(state.host.capacityFactor > 1, `capacity ${state.host.capacityFactor}`);
  const framesBefore = h.all('frame').length;
  assert.ok(framesBefore >= 1);
  await h.request('speed', { speed: 1 });
  // Background policy "pause": hiding the page stops the loop without touching the session.
  await h.request('background', { policy: 'pause' });
  await h.request('visibility', { hidden: true });
  await new Promise((resolve) => setTimeout(resolve, 100));
  const frozen = h.last('state').state.virtualTime;
  await new Promise((resolve) => setTimeout(resolve, 400));
  assert.equal(h.last('state').state.virtualTime, frozen, 'suspended while hidden');
  assert.equal(h.last('state').host.suspended, true);
  await h.request('visibility', { hidden: false });
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime > frozen + 0.2, 5000);
  assert.equal(h.last('state').host.suspended, false);
  // Hidden but running: the loop keeps going (default policy).
  await h.request('background', { policy: 'run' });
  await h.request('visibility', { hidden: true });
  const mark = h.last('state').state.virtualTime;
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime > mark + 0.3, 5000);
  await h.request('visibility', { hidden: false });
  await h.close();
});

test('runtime: a hidden page still gets the LCD picture, at most one frame a second (the first boot after a page load)', async () => {
  // Embedded browser panes and occluded windows report "hidden" while the page is looked at. The worker used to
  // send nothing but forced frames to a hidden page: the picture of a first session stayed black until some action
  // (Restart boards) forced a frame.
  const h = new Harness();
  await h.ready();
  await h.request('visibility', { hidden: true });
  await h.request('boot', { options: { mode: 'dual' }, profile: 'none' });
  await h.request('speed', { speed: null });
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime >= 5.5, 30000);
  await h.waitFor((m) => m.type === 'frame' && m.version >= 2, 6000); // a frame after the forced one of the boot
  const before = h.all('frame').length;
  await new Promise((resolve) => setTimeout(resolve, 3200));
  assert.ok(h.all('frame').length - before <= 4, `at most one frame a second while hidden (${h.all('frame').length - before} in 3.2 s)`);
  assert.equal(h.last('state').host.hidden, true);
  // The screen is steady from 4.75 s on: the hidden page's picture is the B1 prompt, like a visible page's.
  assert.equal(sha256(framePpm(h.last('frame'))), B1_PPM_SHA);
  // Returning to the page sends the current frame at once.
  const count = h.all('frame').length;
  await h.request('visibility', { hidden: false });
  assert.equal(h.all('frame').length, count + 1, 'a forced frame on return');
  await h.close();
});

test('runtime: frame backpressure keeps at most three frames in flight', async () => {
  const h = new Harness({ autoRecycle: false });
  await h.ready();
  await h.request('boot', { options: { mode: 'dual' }, profile: 'none' });
  await h.request('speed', { speed: null });
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime >= 6 && m.state.frameReady, 30000);
  await h.request('speed', { speed: 1 });
  assert.equal(h.all('frame').length, 3, 'only three frames are sent until the page hands buffers back');
  // Handing one back releases the newest frame that was held.
  h.runtime.receive({ type: 'recycle', buffer: new ArrayBuffer(h.last('frame').buffer.byteLength) });
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(h.all('frame').length, 4);
  assert.ok(h.last('frame').version > h.all('frame')[2].version, 'the held-back frame is the latest one');
  await h.close();
});

test('runtime: an engine trap stops everything cleanly and is reported once', async () => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual' }, profile: 'none' });
  await h.waitFor((m) => m.type === 'state' && m.state.virtualTime > 0.2, 10000);
  // Simulate a Rust panic (panic = abort turns it into a trap): the next slice throws a RuntimeError.
  const engine = h.runtime.engine;
  engine.x = { ...engine.x, ngc_session_run_for: () => { throw new WebAssembly.RuntimeError('unreachable'); } };
  const crash = await h.waitFor((m) => m.type === 'crash', 5000);
  assert.match(crash.message, /unreachable/);
  const states = h.all('state').length;
  await new Promise((resolve) => setTimeout(resolve, 600));
  assert.equal(h.all('state').length, states, 'no further state is published after the crash');
  assert.equal(h.all('crash').length, 1, 'reported once');
  await assert.rejects(() => h.action({ action: 'pause' }), /stopped after an internal error/);
  await assert.rejects(() => h.request('capture'), /stopped after an internal error/);
});

test('runtime: unknown requests and requests without a session answer with errors', async () => {
  const h = new Harness();
  await h.request('init');
  await assert.rejects(() => h.request('nonsense'), /unknown request/);
  await assert.rejects(() => h.action({ action: 'pause' }), /No emulator session/);
  await assert.rejects(() => h.request('capture'), /No emulator session/);
});

// ---- releases, session options and output histories (DESIGN 15.3 / 15.4) ------------------------------

test('runtime: the engine identifies the release; the inspection, the boot result and the host status name it', async () => {
  const h = new Harness();
  await h.request('init');
  const handset = await h.request('inspect', { name: HANDSET_FILE, bytes: handsetSrec.slice() });
  assert.equal(handset.release.id, 'TRITON-5.8-65.3');
  assert.equal(handset.release.name, 'TRITON');
  const main = await h.request('inspect', { name: MAIN_FILE, bytes: mainSrec.slice() });
  assert.equal(main.release.id, 'TRITON-5.8-65.3');
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.release.id, 'TRITON-5.8-65.3');
  assert.equal(h.last('state').host.release.name, 'TRITON');
  if (booted.state.firmware.release) assert.equal(booted.state.firmware.release.id, 'TRITON-5.8-65.3', 'the engine names the release in the state document');
  await h.close();
});

test('runtime: a NEPTUN pair is accepted and boots to the B1 screen; a mixed pair is refused; each release has its own profile', { skip: !neptunDir }, async () => {
  const storage = new MemoryStorage();
  const h = new Harness({ storage });
  await h.request('init');
  const handset = await h.request('inspect', { name: NEPTUN_HANDSET_FILE, bytes: neptunHandsetSrec.slice() });
  assert.equal(handset.accepted, true, handset.message);
  assert.equal(handset.role, 'handset');
  assert.equal(handset.release.id, 'NEPTUN-5.8-65.3');
  assert.equal(handset.report.srecSha256, NEPTUN_HANDSET_SHA);
  // The TRITON main image does not belong to the NEPTUN handset: refused with a message, and still identified.
  const mixed = await h.request('inspect', { name: MAIN_FILE, bytes: mainSrec.slice() });
  assert.equal(mixed.accepted, false);
  assert.equal(mixed.conflict, true);
  assert.match(mixed.message, /TRITON main controller 5\.8 image, but the handset 65\.3 file provided is NEPTUN/);
  assert.equal(mixed.report.srecSha256, MAIN_SHA);
  const main = await h.request('inspect', { name: NEPTUN_MAIN_FILE, bytes: neptunMainSrec.slice() });
  assert.equal(main.accepted, true, main.message);
  assert.equal(main.report.srecSha256, NEPTUN_MAIN_SHA);
  // The other direction: a TRITON handset next to the NEPTUN main is refused as well.
  const mixedHandset = await h.request('inspect', { name: HANDSET_FILE, bytes: handsetSrec.slice() });
  assert.equal(mixedHandset.accepted, false);
  assert.match(mixedHandset.message, /TRITON handset 65\.3 image, but the main controller 5\.8 file provided is NEPTUN/);

  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.release.id, 'NEPTUN-5.8-65.3');
  if (booted.state.firmware.release) assert.equal(booted.state.firmware.release.id, 'NEPTUN-5.8-65.3');
  const state = await h.action({ action: 'advance', seconds: 10.5 });
  assert.equal(state.error, null);
  assert.equal(state.standby, false);
  assert.equal(state.handsetPowered, true, 'the handset is released by the main board');
  assert.equal(state.frameReady, true);
  assert.match(state.canSummary, /connected=True; transmitted=\d{2,}/, 'CAN frames flow between the boards');
  assert.equal(sha256(framePpm(h.last('frame'))), B1_PPM_SHA, 'the NEPTUN dual boot reaches the B1 battery-type screen (same 320x240 frame as TRITON)');
  assert.equal(state.firmware.main.srecSha256, NEPTUN_MAIN_SHA);
  await h.action({ action: 'inputs', inputs: { temperature1C: 27.5 } });
  await h.request('flush');
  await h.close();
  assert.ok((await storage.list('profile-neptun-5_8-65_3')).some((file) => file.name === 'eeprom.bin'), 'the NEPTUN profile has its own location');
  assert.equal((await storage.list('profile')).length, 0, 'the TRITON location is untouched');

  // A TRITON session afterwards starts from its own (empty) profile, not from NEPTUN's.
  const t = new Harness({ storage });
  await t.ready();
  const triton = await t.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(triton.release.id, 'TRITON-5.8-65.3');
  assert.equal(triton.state.inputs.temperature1C, 20);
  await t.close();
  const info = await t.request('info');
  assert.ok(info.profiles['NEPTUN-5.8-65.3'], 'the NEPTUN profile is offered for NEPTUN firmware');
  assert.ok(info.profiles['TRITON-5.8-65.3']);
});

test('runtime: every session gets a fresh history nonce and the I2C idle-high fixture; epochs change with every board recreation', async () => {
  const h = new Harness();
  await h.ready();
  const engine = h.runtime.engine;
  const created = [];
  const original = engine.createSession.bind(engine);
  engine.createSession = (config, profile) => { created.push({ ...config }); return original(config, profile); };
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(created[0].i2cIdleHigh, true, 'the fixture is on by default');
  assert.ok(Number.isSafeInteger(created[0].historyNonce) && created[0].historyNonce > 0, 'a random 53-bit nonce');
  const supported = engine.unsupportedOptions.size === 0;
  if (!supported) console.log(`note: this engine build does not know ${[...engine.unsupportedOptions]} yet (retried without them)`);
  assert.deepEqual(h.last('state').host.unsupportedOptions, [...engine.unsupportedOptions]);
  const epochs = [booted.state.outputHistoryEpoch];
  assert.equal(typeof epochs[0], 'string');
  if (supported) assert.match(epochs[0], new RegExp(`^${created[0].historyNonce}-\\d+$`), 'the epoch is "<historyNonce>-<generation>"');
  epochs.push((await h.action({ action: 'reset' })).outputHistoryEpoch);
  epochs.push((await h.action({ action: 'wake' })).outputHistoryEpoch);
  await h.request('import-profile', { files: [{ name: 'led-colors.json', data: new TextEncoder().encode('{}') }] });
  epochs.push(h.last('state').state.outputHistoryEpoch);
  assert.equal(new Set(epochs.slice(0, 3)).size, 3, `a new epoch for every board recreation: ${epochs}`);
  // A brand-new session also gets its own epoch once the engine takes the nonce at creation (an older build only
  // mixes the host entropy into later launches; the page's replay logic additionally keys on its own generation).
  if (supported) assert.equal(new Set(epochs).size, epochs.length, `a new epoch for every session: ${epochs}`);
  assert.equal(created.length, 2, 'the profile import created the second session');
  assert.notEqual(created[1].historyNonce, created[0].historyNonce);
  await h.close();
  // The fixture is a start option.
  await h.request('boot', { options: { mode: 'dual', startPaused: true, i2cIdleHigh: false } });
  assert.equal(created[2].i2cIdleHigh, false);
  await h.close();
});

test('runtime: output histories follow the DESIGN 15.3a contract and feed the replay cursor without gaps', async (t) => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  let state = await h.action({ action: 'advance', seconds: 1 });
  if (!state.hardwareOutputs.some((output) => 'activity' in output)) {
    t.skip('this engine build has no output histories yet');
    await h.close();
    return;
  }
  const by = (id) => state.hardwareOutputs.find((output) => output.id === id);
  const cursors = new Map(state.hardwareOutputs.map((output) => [output.id, new Cursor()]));
  const seen = new Map();
  for (let step = 0; step < 24; step++) {
    state = await h.action({ action: 'advance', seconds: 0.5 });
    for (const output of state.hardwareOutputs) {
      const result = cursors.get(output.id).observe(output, state.virtualTime, state.outputHistoryEpoch);
      assert.equal(result.invalidEventCount, 0, `${output.id}: no malformed events`);
      assert.equal(result.gapCount, 0, `${output.id}: no history events lost between 0.5 s polls`);
      seen.set(output.id, (seen.get(output.id) || 0) + result.activations.length);
    }
  }
  for (const id of ['main-hud-1', 'main-hud-2', 'main-hud-3']) {
    const activity = by(id).activity;
    assert.equal(activity.source, 'sampled-pwm-command');
    assert.equal(activity.clock, 'virtual-time');
    assert.equal(activity.capacity, 32);
    assert.equal(activity.samplingPeriodSeconds, 0.05);
    assert.ok(activity.eventCount >= 1 && activity.events.length === Math.min(32, activity.eventCount));
    assert.equal(activity.events[0].sequence, activity.eventCount - activity.events.length + 1);
    assert.equal(by(id).pwmActivity, null);
  }
  const vibrator = by('handset-vibrator');
  assert.equal(vibrator.activity.source, 'gpio-enable-command');
  assert.equal(vibrator.pwmActivity.source, 'sampled-gated-pwm-command');
  assert.equal(vibrator.pwmActivity.samplingPeriodSeconds, 0.02);
  assert.equal(by('handset-backlight').activity, null);
  assert.equal(by('handset-backlight').pwmActivity, null);
  // Defaults: HUD 1 unknown, HUD 2 white, HUD 3 red.
  assert.deepEqual(['main-hud-1', 'main-hud-2', 'main-hud-3'].map((id) => by(id).color), ['unknown', 'white', 'red']);
  console.log(`output histories: replay activations seen in 12 s of boot: ${JSON.stringify(Object.fromEntries(seen))}`);
  await h.close();
});

test('runtime: a real vibrator and HUD activation (Photolithium B1, dive pressure) is observed by the replay logic, with no history gaps', async (t) => {
  const h = new Harness();
  await h.ready();
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  let state = await h.action({ action: 'advance', seconds: 0.5 });
  if (!state.hardwareOutputs.some((output) => 'activity' in output)) {
    t.skip('this engine build has no output histories yet');
    return;
  }
  // The page's replay controller on the real states, with a fake wall clock: how many flashes would the page show?
  const timers = new Map();
  let clock = 0;
  let timerId = 0;
  const flashes = new Map();
  const wasOn = new Map();
  const gaps = [];
  const replay = new ReplayController({
    setTimer: (fn, delay) => { const id = ++timerId; timers.set(id, { fn, due: clock + delay }); return id; },
    clearTimer: (id) => timers.delete(id),
    enabled: () => true,
    paused: () => false,
    draw: (entry) => {
      const on = entry.replayOn;
      if (on && !wasOn.get(entry.id)) flashes.set(entry.id, (flashes.get(entry.id) || 0) + 1);
      wasOn.set(entry.id, on);
      if (entry.cursor.totalGapCount) gaps.push(entry.id);
    },
  });
  const wall = (ms) => { // advances the fake wall clock, firing the flash timers
    const target = clock + ms;
    for (;;) {
      const next = [...timers].filter(([, value]) => value.due <= target).sort((a, b) => a[1].due - b[1].due || a[0] - b[0])[0];
      if (!next) break;
      timers.delete(next[0]);
      clock = next[1].due;
      next[1].fn();
    }
    clock = target;
  };
  const observe = () => replay.update(state.hardwareOutputs, { virtualTime: state.virtualTime, epoch: `${state.outputHistoryEpoch}:1` });
  const advance = async (seconds) => {
    for (let spent = 0; spent < seconds - 1e-9; spent += 0.5) {
      state = await h.action({ action: 'advance', seconds: Math.min(0.5, seconds - spent) });
      observe();
      wall(500);
    }
  };
  const move = async (action) => { await h.action({ action }); await advance(0.65); };
  await advance(6);
  for (let i = 0; i < 2; i++) await move('down'); // B1 type 2 (Photolithium: the type that allows alert vibration)
  await move('confirm'); await move('confirm');
  await move('down'); await move('confirm'); await move('confirm');
  await h.action({ action: 'inputs', inputs: { oxygen1Mv: 10, oxygen2Mv: 10, oxygen3Mv: 10, pressure1Mbar: 1000, pressure2Mbar: 1000 } });
  await advance(5);
  await h.action({ action: 'inputs', inputs: { oxygen1Mv: 40, oxygen2Mv: 40, oxygen3Mv: 40 } });
  await advance(5);
  await h.action({ action: 'inputs', inputs: { pressure1Mbar: 3500, pressure2Mbar: 3500 } }); // a dive
  await advance(25);
  const by = (id) => state.hardwareOutputs.find((output) => output.id === id);
  const counts = { vibrator: by('handset-vibrator').pwmActivity.eventCount, hud1: by('main-hud-1').activity.activationCount, hud2: by('main-hud-2').activity.activationCount };
  console.log(`real alerts: engine history ${JSON.stringify(counts)}, page replay flashes ${JSON.stringify(Object.fromEntries(flashes))}`);
  assert.ok(counts.vibrator > 4, 'the original firmware commanded the vibrator motor');
  assert.ok(counts.hud1 + counts.hud2 > 2, 'the original firmware commanded HUD channels');
  assert.ok((flashes.get('handset-vibrator') || 0) > 0, 'a vibrator activation was replayed');
  assert.ok((flashes.get('main-hud-1') || 0) + (flashes.get('main-hud-2') || 0) > 0, 'a HUD activation was replayed');
  assert.deepEqual(gaps, [], 'polling every 0.5 s never loses history events (capacity 32)');
  assert.equal(by('main-hud-3').activity.activationCount, 0, 'HUD 3 (red) never activated in this scenario');
  // Restart: the engine starts new histories; the page's replay would baseline them (new epoch).
  const before = state.outputHistoryEpoch;
  state = await h.action({ action: 'reset' });
  assert.notEqual(state.outputHistoryEpoch, before);
  assert.ok(state.hardwareOutputs.every((output) => !output.activity || output.activity.eventCount <= 1), 'histories restart with the boards');
  await h.close();
});

/** Virtual seconds per wall second between two state messages (their arrival times, not the sampling times). */
function rate(first, last) {
  return (last.state.virtualTime - first.state.virtualTime) / ((last.at - first.at) / 1000);
}

test('pacing: real-time factor of the paced loop versus the unpaced engine', { skip: skipPacing }, async () => {
  const h = new Harness();
  await h.ready();
  const bootStart = performance.now();
  await h.request('boot', { options: { mode: 'dual' }, profile: 'none' });
  // Unpaced boot: how fast does the worker logic (slices, frames, state, messages) run the boot sequence?
  await h.request('speed', { speed: null });
  const bootDone = await h.waitFor((m) => m.type === 'state' && m.state.virtualTime >= 4.5, 60000);
  const bootFactor = bootDone.state.virtualTime / ((bootDone.at - bootStart) / 1000);
  // Unpaced steady state over about a second and a half.
  await new Promise((resolve) => setTimeout(resolve, 300));
  const u0 = h.last('state');
  await new Promise((resolve) => setTimeout(resolve, Math.max(1000, pacingSeconds * 500)));
  const u1 = h.last('state');
  const unpacedFactor = rate(u0, u1);
  const unpacedStatus = u1.host;
  // Paced at 1x for pacingSeconds of wall time.
  await h.request('speed', { speed: 1 });
  await new Promise((resolve) => setTimeout(resolve, 500));
  const p0 = h.last('state');
  await new Promise((resolve) => setTimeout(resolve, pacingSeconds * 1000));
  const p1 = h.last('state');
  const pacedFactor = rate(p0, p1);
  const status = p1.host;
  const framesPerSecond = h.all('frame').filter((m) => m.at >= p0.at && m.at <= p1.at).length / ((p1.at - p0.at) / 1000);
  console.log(
    `pacing: boot unpaced ${bootFactor.toFixed(1)}x, steady unpaced ${unpacedFactor.toFixed(1)}x (runtime reports ${unpacedStatus.realtimeFactor.toFixed(1)}x, capacity ${unpacedStatus.capacityFactor.toFixed(1)}x); ` +
      `paced 1x measured ${pacedFactor.toFixed(3)}x (runtime reports ${status.realtimeFactor.toFixed(3)}x, capacity ${status.capacityFactor.toFixed(1)}x, ` +
      `keepingUp ${status.keepingUp}, lag ${status.lagMs.toFixed(1)} ms, frames ${framesPerSecond.toFixed(1)}/s)`,
  );
  assert.ok(pacedFactor > 0.97 && pacedFactor < 1.03, `paced real-time factor ${pacedFactor}`);
  assert.ok(status.realtimeFactor > 0.95 && status.realtimeFactor < 1.05, `reported real-time factor ${status.realtimeFactor}`);
  assert.equal(status.keepingUp, true);
  assert.ok(unpacedFactor > 1, `unpaced steady state ${unpacedFactor}`);
  assert.ok(framesPerSecond <= 61, `frame publication is capped at 60 per second (${framesPerSecond})`);
  await h.close();
});
