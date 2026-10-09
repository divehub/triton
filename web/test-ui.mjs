#!/usr/bin/env node
// Tests of the browser application's user-interface logic, without the WebAssembly engine and without a browser:
// the DOM-free modules (sensors.js, replay.js, conditions.js, keys.js, releases.js) and the real index.html with the
// real emulator.js / entry.js running on a minimal fake DOM (fake-dom.mjs).
//
//   node web/test-ui.mjs
//
// The cases are ported from the analysis workspace's emulation/test_sensor_controls.js, test_output_replay.js,
// test_output_replay_ui.js and test_scenario_ui.js (which ran the viewer.html scripts there), plus tests of the parts
// that only exist here: releases, per-release profiles, the session options and the worker-side runtime logic on a
// fake engine. The real engine is covered by test-node.mjs.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Engine, EngineError } from './engine.js';
import { ActionQueue, BASIC_IDS, ConditionsController } from './conditions.js';
import { handsetKeyAction, isHandsetArrow } from './keys.js';
import { Cursor, PulseQueue, ReplayController, describeEntry, driveText, historyText } from './replay.js';
import { CUSTOM_RELEASE_ID, DEFAULT_RELEASE_ID, RELEASES, RELEASE_IDS, describeRelease, firmwareArea, mixedPairMessage, pairConflict, profileArea, releaseOf, storageAreas } from './releases.js';
import * as faults from './faults.js';
import { FirmwareFetchError, MAX_FETCH_BYTES, configuredProxyUrl, fetchFirmware, loopbackProxyUrl, normalizeFirmwareUrl, proxyEndpoint } from './firmware-url.js';
import { FIRMWARE_PROXY_URL } from './config.js';
import * as sensors from './sensors.js';
import * as deco from './deco.js';
import * as game from './game-logic.js';
import { MAX_DEPTH_METERS, getLoopReadings } from './game-gas.js';
import { Runtime, nonceFromWords } from './runtime.js';
import { MemoryStorage } from './storage.js';
import { Element, installDom } from './fake-dom.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const html = fs.readFileSync(path.join(here, 'index.html'), 'utf8');
const plain = (value) => JSON.parse(JSON.stringify(value));
const clone = plain;
const settle = async () => { for (let turn = 0; turn < 30; turn++) await Promise.resolve(); };

// =====================================================================================================
// sensors.js  (analysis workspace: test_sensor_controls.js)
// =====================================================================================================

const { defaults, calculate, fromInputs, pressureMbar } = sensors;
const near = (actual, expected, message = '') => assert.ok(Math.abs(actual - expected) <= 1e-9, `${message}: expected ${expected}, received ${actual}`);
const rawDefaults = {
  oxygen1Mv: 10, oxygen2Mv: 10, oxygen3Mv: 10,
  pressure1Mbar: 1013.25, pressure2Mbar: 1013.25,
  temperature1C: 20, temperature2C: 20,
};

test('sensors: default physical readings, and defaults() never shares its arrays', () => {
  assert.deepEqual(plain(calculate(defaults())), rawDefaults);
  assert.deepEqual(plain(fromInputs(rawDefaults)), plain(defaults()));
  const edited = defaults();
  edited.oxygenVariationsMv[0] = 20;
  edited.pressureVariationsMbar[0] = 30;
  edited.temperatureVariationsC[0] = 2;
  assert.deepEqual(plain(defaults().oxygenVariationsMv), [0, 0, 0]);
  assert.deepEqual(plain(defaults().pressureVariationsMbar), [0, 0]);
  assert.deepEqual(plain(defaults().temperatureVariationsC), [0, 0]);
  assert.deepEqual(plain(sensors.WATER_DENSITIES), { fresh: 1000, salt: 1025, en13319: 1020 });
  assert.equal(sensors.GRAVITY, 9.80665);
});

test('sensors: depth converts meters to absolute mbar with each adopted water density', () => {
  near(pressureMbar(1013.25, 10, 'fresh'), 1993.915, 'Fresh water at 10m');
  near(pressureMbar(1013.25, 10, 'salt'), 2018.431625, 'Salt water at 10m');
  near(pressureMbar(1013.25, 10, 'en13319'), 2013.5283, 'EN13319 at 10m');
  near(pressureMbar(800, 0, 'salt'), 800, 'Surface absolute pressure');
  near(pressureMbar(1013.25, 110, 'en13319'), 12016.3113, 'Maximum depth');
});

test('sensors: signed variations alter individual sensors independently and leave unrelated inputs untouched', () => {
  const settings = {
    ...defaults(), oxygenBaseMv: 60, oxygenVariationsMv: [-10, 0, 5],
    surfacePressureMbar: 1000, depthM: 10, waterType: 'fresh', pressureVariationsMbar: [-25, 10],
    temperatureBaseC: 4, temperatureVariationsC: [-8, 3],
  };
  // The page numbers the sensors as the firmware does, the reverse of the engine keys: sensor 1 (the first variation) is the
  // I2C2 device (pressure2Mbar, temperature2C), sensor 2 the I2C1 device (pressure1Mbar, temperature1C).
  assert.deepEqual(plain(calculate(settings)), {
    oxygen1Mv: 50, oxygen2Mv: 60, oxygen3Mv: 65,
    pressure2Mbar: 1955.665, pressure1Mbar: 1990.665,
    temperature2C: -4, temperature1C: 7,
  });
  assert.equal(settings.oxygenVariationsMv[0], -10);
  assert.equal(Object.hasOwn(calculate(settings), 'battery1Mv'), false);
});

test('sensors: the page numbers the two MS5837 sensors as the firmware does, the reverse of the engine keys', () => {
  assert.deepEqual([...sensors.PRESSURE_KEYS], ['pressure2Mbar', 'pressure1Mbar']);
  assert.deepEqual([...sensors.TEMPERATURE_KEYS], ['temperature2C', 'temperature1C']);
  assert.deepEqual([...sensors.SENSOR_BUSES], ['I2C2', 'I2C1']);
  // The engine keys themselves are unchanged (inputs.json and the command-line scripts use them) and all seven stay calculated.
  assert.deepEqual([...sensors.SENSOR_KEYS].sort(), ['oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv', 'pressure1Mbar', 'pressure2Mbar', 'temperature1C', 'temperature2C']);
  // An unequal pair reverses into offsets in page order: sensor 1 is the I2C2 reading.
  const settings = fromInputs({ ...rawDefaults, pressure1Mbar: 1000, pressure2Mbar: 1030, temperature1C: 18, temperature2C: 22 });
  near(settings.pressureVariationsMbar[0] - settings.pressureVariationsMbar[1], 30, 'sensor 1 minus sensor 2 (I2C2 minus I2C1)');
  assert.deepEqual(plain(settings.temperatureVariationsC), [2, -2]);
});

test('sensors: all physical output limits are inclusive even when outside basic slider ranges', () => {
  const settings = {
    ...defaults(), oxygenBaseMv: 100, oxygenVariationsMv: [-100, 150, 0],
    surfacePressureMbar: 100, pressureVariationsMbar: [0, 29900],
    temperatureBaseC: -4, temperatureVariationsC: [-16, 89],
  };
  assert.deepEqual(plain(calculate(settings)), {
    oxygen1Mv: 0, oxygen2Mv: 250, oxygen3Mv: 100,
    pressure2Mbar: 100, pressure1Mbar: 30000, // sensor 1 is the engine's pressure2Mbar / temperature2C
    temperature2C: -20, temperature1C: 85,
  });
});

test('sensors: out-of-range base controls are rejected instead of clamped', () => {
  for (const [key, value] of [
    ['oxygenBaseMv', -0.1], ['oxygenBaseMv', 100.1],
    ['depthM', -0.1], ['depthM', 110.1],
    ['temperatureBaseC', -4.1], ['temperatureBaseC', 40.1],
    ['surfacePressureMbar', 99.9], ['surfacePressureMbar', 30000.1],
  ]) assert.throws(() => calculate({ ...defaults(), [key]: value }), new RegExp(key));
});

test('sensors: out-of-range combined readings reject the whole patch without silently clipping', () => {
  for (const [key, value, message] of [
    ['oxygenVariationsMv', [-10.1, 0, 0], 'oxygen1Mv'],
    ['oxygenVariationsMv', [0, 241, 0], 'oxygen2Mv'],
    ['pressureVariationsMbar', [-913.26, 0], 'pressure2Mbar'], // page sensor 1
    ['pressureVariationsMbar', [0, 28986.76], 'pressure1Mbar'], // page sensor 2
    ['temperatureVariationsC', [-40.1, 0], 'temperature2C'],
    ['temperatureVariationsC', [0, 65.1], 'temperature1C'],
  ]) assert.throws(() => calculate({ ...defaults(), [key]: value }), new RegExp(message));
  assert.throws(() => calculate({ ...defaults(), surfacePressureMbar: 30000, depthM: 1 }), /pressure2Mbar/);
});

test('sensors: a sum that misses a limit only by floating-point rounding is snapped to the limit, not rejected', () => {
  // 62.04 + (250 - 62.04) is not exactly 250 in binary floating point; the engine would refuse 250.00000000000003.
  const base = 62.04;
  const settings = { ...defaults(), oxygenBaseMv: base, oxygenVariationsMv: [250 - base, 0, 0] };
  assert.equal(calculate(settings).oxygen1Mv, 250);
  assert.throws(() => calculate({ ...settings, oxygenVariationsMv: [250 - base + 1e-6, 0, 0] }), /oxygen1Mv/);
});

test('sensors: nonfinite, textual, malformed and unknown choices produce clear errors', () => {
  for (const value of [NaN, Infinity, -Infinity, '', '10', null, true]) {
    assert.throws(() => calculate({ ...defaults(), oxygenBaseMv: value }), /oxygenBaseMv.*finite/);
  }
  assert.throws(() => calculate({ ...defaults(), oxygenVariationsMv: [0, NaN, 0] }), /oxygenVariationsMv\[1\]/);
  assert.throws(() => calculate({ ...defaults(), oxygenVariationsMv: Array(3) }), /oxygenVariationsMv\[0\]/);
  assert.throws(() => calculate({ ...defaults(), pressureVariationsMbar: [0] }), /2 sensor variations/);
  assert.throws(() => calculate({ ...defaults(), temperatureVariationsC: [0, 0, 0] }), /2 sensor variations/);
  for (const value of ['seawater', '__proto__', undefined, null, { toString: () => 'fresh' }]) {
    assert.throws(() => calculate({ ...defaults(), waterType: value }), /waterType/);
  }
  assert.throws(() => calculate(null), /settings/);
});

test('sensors: raw readings reverse into bounded sliders while offsets preserve outlier values', () => {
  const raw = {
    oxygen1Mv: 0, oxygen2Mv: 110, oxygen3Mv: 250,
    pressure1Mbar: 100, pressure2Mbar: 30000, temperature1C: 85, temperature2C: 85,
  };
  const settings = fromInputs(raw);
  assert.equal(settings.oxygenBaseMv, 100);
  assert.deepEqual(plain(settings.oxygenVariationsMv), [-100, 10, 150]);
  assert.equal(settings.depthM, 110);
  assert.equal(settings.temperatureBaseC, 40);
  assert.deepEqual(plain(settings.temperatureVariationsC), [45, 45]);
  const restored = calculate(settings);
  for (const key of Object.keys(raw)) near(restored[key], raw[key], key);
});

test('sensors: raw pressure below surface remains depth zero and is preserved with signed offsets', () => {
  const raw = { ...rawDefaults, pressure1Mbar: 800, pressure2Mbar: 900, temperature1C: -20, temperature2C: -20 };
  const settings = fromInputs(raw);
  assert.equal(settings.depthM, 0);
  assert.equal(settings.temperatureBaseC, -4);
  assert.deepEqual(plain(settings.temperatureVariationsC), [-16, -16]);
  assert.deepEqual(plain(calculate(settings)), raw);
});

test('sensors: reverse conversion keeps selected surface and water type and derives depth from sensor mean', () => {
  const raw = { ...rawDefaults, pressure1Mbar: 1475.665, pressure2Mbar: 1485.665 };
  const settings = fromInputs(raw, { ...defaults(), surfacePressureMbar: 500, waterType: 'fresh', depthM: 60 });
  assert.equal(settings.surfacePressureMbar, 500);
  assert.equal(settings.waterType, 'fresh');
  near(settings.depthM, 10);
  near(settings.pressureVariationsMbar[0], 5); // sensor 1 is pressure2Mbar
  near(settings.pressureVariationsMbar[1], -5);
  assert.deepEqual(plain(calculate(settings)), raw);
});

test('sensors: reverse conversion validates existing physical readings and retained surface/water selections', () => {
  assert.throws(() => fromInputs({ ...rawDefaults, oxygen3Mv: 251 }), /oxygen3Mv/);
  assert.throws(() => fromInputs({ ...rawDefaults, pressure2Mbar: Infinity }), /pressure2Mbar/);
  assert.throws(() => fromInputs({ ...rawDefaults, temperature1C: -21 }), /temperature1C/);
  assert.throws(() => fromInputs({}, defaults()), /oxygen1Mv/);
  assert.throws(() => fromInputs(rawDefaults, { surfacePressureMbar: 99 }), /surfacePressureMbar/);
  assert.throws(() => fromInputs(rawDefaults, { waterType: 'unknown' }), /waterType/);
});

test('sensors: round trips preserve unequal sensor readings throughout the engine-accepted domain', () => {
  let seed = 721;
  const random = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
  for (let index = 0; index < 200; index++) {
    const raw = {
      oxygen1Mv: random() * 250, oxygen2Mv: random() * 250, oxygen3Mv: random() * 250,
      pressure1Mbar: 100 + random() * 29900, pressure2Mbar: 100 + random() * 29900,
      temperature1C: -20 + random() * 105, temperature2C: -20 + random() * 105,
    };
    const restored = calculate(fromInputs(raw));
    for (const key of Object.keys(raw)) near(restored[key], raw[key], key);
  }
  // Readings on the limits themselves survive the round trip (exactly, never one ulp outside).
  for (const value of [0, 250]) {
    const restored = calculate(fromInputs({ ...rawDefaults, oxygen1Mv: value }));
    assert.equal(restored.oxygen1Mv, value);
  }
});

test('sensors: a rejected value is explained with friendly names and the outcome', () => {
  assert.equal(sensors.explainRejection('oxygen2Mv must be between 0 and 250'), 'Oxygen cell 2 must be between 0 and 250. Changes have not been applied.');
  assert.equal(sensors.explainRejection('Depth needs a number.'), 'Depth needs a number. Changes have not been applied.');
  // The sensors are named by the page's (firmware) numbers, which are the reverse of the engine keys.
  assert.equal(sensors.explainRejection('pressure2Mbar must be between 100 and 30000'), 'Pressure sensor 1 must be between 100 and 30000. Changes have not been applied.');
  assert.equal(sensors.explainRejection('pressure1Mbar must be between 100 and 30000'), 'Pressure sensor 2 must be between 100 and 30000. Changes have not been applied.');
  assert.equal(sensors.explainRejection('temperature2C must be between -20 and 85'), 'Temperature sensor 1 must be between -20 and 85. Changes have not been applied.');
  assert.equal(sensors.explainRejection('temperature1C must be between -20 and 85'), 'Temperature sensor 2 must be between -20 and 85. Changes have not been applied.');
});

// =====================================================================================================
// replay.js: Cursor and PulseQueue  (analysis workspace: test_output_replay.js)
// =====================================================================================================

const event = (sequence, active, extra = {}) => ({
  sequence, virtualTime: sequence / 20, kind: sequence === 1 ? 'initial-sample' : 'change',
  active, dutyPercent: active ? 95 : 0, level: null, command: null, ...extra,
});
const history = (events, extra = {}) => ({
  source: 'sampled-pwm-command', eventCount: events.length ? events[events.length - 1].sequence : 0,
  events, ...extra,
});
const output = (events, extra = {}) => ({
  id: 'main-hud-2', active: events.length ? events[events.length - 1].active : false,
  activity: history(events), ...extra,
});
const sequences = (result) => Array.from(result.activations, (entry) => entry.sequence);

test('replay: short On then Off between polls replays once despite current Off', () => {
  const cursor = new Cursor();
  assert.equal(cursor.observe(output([event(1, false)]), 1).attached, true);
  const snapshot = output([event(1, false), event(2, true), event(3, false)]);
  const update = cursor.observe(snapshot, 2);
  assert.deepEqual(sequences(update), [2]);
  assert.deepEqual(plain(update.activations[0]), { sequence: 2, virtualTime: 0.1, dutyPercent: 95, source: 'sampled-pwm-command' });
  assert.equal(update.observedActive, false);
  assert.equal(update.gapCount, 0);
  assert.deepEqual(sequences(cursor.observe(snapshot, 2)), []);
  assert.deepEqual(sequences(cursor.observe(snapshot, 3)), []);
});

test('replay: first attach discards old backlog and initial samples never replay', () => {
  const cursor = new Cursor();
  const events = Array.from({ length: 32 }, (_, index) => event(index + 1, index % 2 === 1));
  const first = cursor.observe(output(events), 20);
  assert.deepEqual(sequences(first), []);
  assert.equal(first.baselineEventCount, 32);
  assert.equal(first.observedActive, true);
  const initial = new Cursor();
  initial.observe(output([]), 0);
  assert.deepEqual(sequences(initial.observe(output([event(1, true)]), 1)), []);
});

test('replay: vibrator PWM pulses replay while PB15 enable stays high', () => {
  const cursor = new Cursor();
  const enabled = history([event(1, null, { level: true, dutyPercent: null })], { source: 'gpio-enable-command' });
  const motor = (events) => ({
    id: 'handset-vibrator', active: false, level: true, activity: enabled,
    pwmActivity: history(events, { source: 'sampled-gated-pwm-command' }),
  });
  cursor.observe(motor([event(1, false)]), 1);
  const update = cursor.observe(motor([event(1, false), event(2, true), event(3, false), event(4, true), event(5, false)]), 2);
  assert.deepEqual(sequences(update), [2, 4]);
  assert.equal(update.source, 'sampled-gated-pwm-command');
  assert.equal(update.observedActive, false);
});

test('replay: GPIO level is a fallback only for GPIO enable-command histories', () => {
  const events = [event(1, null, { level: false }), event(2, null, { level: true }), event(3, null, { level: false })];
  const gpio = new Cursor();
  gpio.observe(output(events.slice(0, 1), { activity: history(events.slice(0, 1), { source: 'gpio-enable-command' }) }), 1);
  assert.deepEqual(sequences(gpio.observe(output(events, { activity: history(events, { source: 'gpio-enable-command' }) }), 2)), [2]);
  const pwm = new Cursor();
  pwm.observe(output(events.slice(0, 1)), 1);
  assert.deepEqual(sequences(pwm.observe(output(events), 2)), []);
});

test('replay: configuration and duty changes on an already active interval do not replay', () => {
  const cursor = new Cursor();
  const events = [event(1, false), event(2, true)];
  cursor.observe(output(events.slice(0, 1)), 1);
  assert.deepEqual(sequences(cursor.observe(output(events), 2)), [2]);
  events.push(event(3, true, { dutyPercent: 50, command: { ccr: 1000 } }));
  events.push(event(4, true, { dutyPercent: 50, command: { ccr: 1000, arr: 1999 } }));
  assert.deepEqual(sequences(cursor.observe(output(events), 3)), []);
  assert.equal(cursor.observedActive, true);
  events.push(event(5, false), event(6, true), event(7, false));
  assert.deepEqual(sequences(cursor.observe(output(events), 4)), [6]);
});

test('replay: live snapshot state does not create duplicate historical activations', () => {
  const cursor = new Cursor();
  const events = [event(1, false), event(2, true)];
  cursor.observe(output(events.slice(0, 1)), 1);
  assert.deepEqual(sequences(cursor.observe(output(events, { active: false }), 2)), [2]);
  events.push(event(3, true, { dutyPercent: 50 }));
  assert.deepEqual(sequences(cursor.observe(output(events, { active: true }), 3)), []);
  assert.equal(cursor.observedActive, true);
});

test('replay: history count regression and virtual-time reversal baseline a reset', () => {
  for (const resetBy of ['count', 'time']) {
    const cursor = new Cursor();
    const old = output([event(1, false), event(2, true), event(3, false)]);
    cursor.observe(old, 5);
    const next = resetBy === 'count' ? output([event(1, false), event(2, true)]) : old;
    const update = cursor.observe(next, resetBy === 'time' ? 4 : 6);
    assert.equal(update.reset, true);
    assert.equal(update.attached, true);
    assert.deepEqual(sequences(update), []);
  }
});

test('replay: equal-count/equal-time changed history and an explicit lifecycle domain reset safely', () => {
  const cursor = new Cursor();
  const old = output([event(1, false), event(2, true), event(3, false)]);
  cursor.observe(old, 5, 'first-instance');
  const replaced = output([event(1, true), event(2, false), event(3, true)]);
  const changed = cursor.observe(replaced, 5, 'first-instance');
  assert.equal(changed.reset, true);
  assert.deepEqual(sequences(changed), []);
  const repeated = cursor.observe(replaced, 5, 'new-instance');
  assert.equal(repeated.reset, true);
  assert.deepEqual(sequences(repeated), []);
});

test('replay: source change, output replacement, and reconnect all discard backlog', () => {
  const events = [event(1, false), event(2, true), event(3, false)];
  const cursor = new Cursor();
  cursor.observe(output(events), 1);
  const source = cursor.observe(output(events, { activity: history(events, { source: 'sampled-gated-pwm-command' }) }), 2);
  assert.equal(source.reset, true);
  assert.deepEqual(sequences(source), []);
  const replaced = cursor.observe(output(events, { id: 'replacement' }), 3);
  assert.equal(replaced.reset, true);
  cursor.reset();
  assert.deepEqual(sequences(cursor.observe(output(events), 4)), []);
  assert.equal(cursor.observe(null, 5).reset, true);
  assert.deepEqual(sequences(cursor.observe(output(events), 6)), []);
});

test('replay: truncated gaps baseline the first retained event and replay only proven rises', () => {
  const cursor = new Cursor();
  cursor.observe(output([event(1, false)]), 1);
  const tail = output([event(5, true), event(6, false), event(7, true), event(8, false)], {
    activity: history([event(5, true), event(6, false), event(7, true), event(8, false)], { truncated: true }),
  });
  const update = cursor.observe(tail, 2);
  assert.deepEqual(sequences(update), [7]);
  assert.equal(update.gapCount, 3);
  assert.equal(update.totalGapCount, 3);
  assert.equal(update.observedActive, false);
  const repeated = cursor.observe(tail, 3);
  assert.equal(repeated.gapCount, 0);
  assert.equal(repeated.totalGapCount, 3);
  assert.deepEqual(sequences(repeated), []);
});

test('replay: missing final events and malformed entries cannot invent a pulse', () => {
  const cursor = new Cursor();
  cursor.observe(output([event(1, false)]), 1);
  const incomplete = output([event(1, false)], { activity: history([event(1, false)], { eventCount: 4 }) });
  const gap = cursor.observe(incomplete, 2);
  assert.equal(gap.gapCount, 3);
  assert.equal(gap.observedActive, null);
  const events = [event(5, true), event(6, false), event(7, true), event(7, true), { sequence: -1 }];
  const update = cursor.observe(output(events, { activity: history(events, { eventCount: 7 }) }), 3);
  assert.deepEqual(sequences(update), [7]);
  assert.equal(update.invalidEventCount, 2);
});

test('replay: bounded pulse queue keeps freshest pulses and reports dropped pending entries', () => {
  const queue = new PulseQueue();
  const activations = Array.from({ length: 15 }, (_, index) => ({ sequence: index + 1, virtualTime: index }));
  assert.equal(queue.enqueue(activations), 12);
  assert.equal(queue.length, 12);
  assert.equal(queue.droppedCount, 3);
  assert.equal(queue.shift().sequence, 4);
  assert.equal(queue.length, 11);
  queue.enqueue([{ sequence: 16 }, { sequence: 17 }]);
  assert.equal(queue.droppedCount, 4);
  assert.equal(queue.shift().sequence, 6);
  queue.clear();
  assert.equal(queue.length, 0);
  assert.equal(queue.droppedCount, 0);
  assert.equal(queue.shift(), undefined);
  assert.throws(() => new PulseQueue(0), /positive integer/);
  assert.throws(() => new PulseQueue(1.5), /positive integer/);
});

// =====================================================================================================
// the DOM-level harness: the real index.html with the real EmulatorView
// =====================================================================================================

function fakeTimers() {
  const timers = new Map();
  let clock = 0;
  let timerId = 0;
  return {
    timers,
    setTimer: (fn, delay) => { const id = ++timerId; timers.set(id, { fn, due: clock + delay }); return id; },
    clearTimer: (id) => timers.delete(id),
    advance(ms) {
      const target = clock + ms;
      for (;;) {
        const next = [...timers].filter(([, value]) => value.due <= target).sort((a, b) => a[1].due - b[1].due || a[0] - b[0])[0];
        if (!next) break;
        timers.delete(next[0]);
        clock = next[1].due;
        next[1].fn();
      }
      clock = target;
    },
  };
}

const fakeSlot = (name) => ({ name, size: 1000, report: { srecSha256: 'sha', checks: [] } });

/** Mounts the real EmulatorView on a fresh fake document; `client.request('action')` calls wait for the test. */
async function mount(search = '') {
  installDom(html, { search });
  const { EmulatorView } = await import('./emulator.js');
  const clock = fakeTimers();
  const sent = [];
  const requests = [];
  const client = {
    send: (type, payload) => sent.push({ type, payload }),
    request: (type, payload) => new Promise((resolve, reject) => requests.push({ type, payload, resolve, reject, done: false })),
  };
  const view = new EmulatorView(client, { closeSession() {}, notify() {} }, { timers: clock });
  view.show({
    options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: describeRelease(DEFAULT_RELEASE_ID),
    slots: { main: fakeSlot('main.srec'), handset: fakeSlot('handset.srec') },
  });
  return { view, clock, sent, requests, document: globalThis.document, window: globalThis.window };
}

// ---- replay in the page (analysis workspace: test_output_replay_ui.js) ----------------------------------------------

const baseState = { running: false, virtualTime: 0, pc: 0x08008410, frameReady: false, hardwareOutputs: [], uartConsole: [], outputHistoryEpoch: 'domain-a' };
const hostBase = { profileEpoch: 1, generation: 1 };

function vibratorOrLed(events, kind = 'led', extra = {}) {
  const activity = {
    source: kind === 'vibrator' ? 'sampled-gated-pwm-command' : 'sampled-pwm-command',
    eventCount: events.at(-1)?.sequence || 0, events, activationCount: 0,
    lastOnVirtualTime: null, lastOffVirtualTime: null, samplingPeriodSeconds: kind === 'vibrator' ? 0.02 : 0.05,
  };
  const result = {
    id: kind === 'vibrator' ? 'handset-vibrator' : 'main-hud-2', kind,
    label: kind === 'vibrator' ? 'Vibrator' : 'HUD channel 2', board: kind === 'vibrator' ? 'handset' : 'main',
    color: kind === 'vibrator' ? 'unknown' : 'white', active: events.at(-1)?.active ?? false,
    dutyPercent: events.at(-1)?.dutyPercent || 0, activity,
  };
  if (kind === 'vibrator') {
    result.pwmActivity = activity;
    // The enable remains high while the motor compare pulses. This history must not independently replay an
    // extra motor pulse.
    result.level = true;
    result.activity = { source: 'gpio-enable-command', eventCount: 1, activationCount: 1, events: [event(1, null, { level: true, dutyPercent: null })] };
  }
  return { ...result, ...extra };
}

async function replayHarness() {
  const m = await mount();
  let last = { ...baseState };
  const show = (outputs, virtualTime = 1, epoch = 'domain-a') => {
    last = { ...baseState, hardwareOutputs: outputs, virtualTime, outputHistoryEpoch: epoch };
    m.view.onState({ state: last, host: { ...hostBase } });
  };
  return {
    ...m,
    show,
    advance: (ms) => m.clock.advance(ms),
    timers: m.clock.timers,
    row: (id = 'main-hud-2') => ({ ...m.view.outputRows.get(id), entry: m.view.replay.entries.get(id) }),
    light: (id) => m.document.getElementById(`simple-${id}-light`),
    stateText: (id) => m.document.getElementById(`simple-${id}-state`).textContent,
    checkbox: (checked) => { const box = m.document.getElementById('replay-pulses'); box.checked = checked; box.dispatch('change'); },
    disconnect: () => m.view.setConnectionError('Test connection lost'),
    reconnect: () => m.view.onState({ state: last, host: { ...hostBase } }),
  };
}

const isOn = (row) => row.indicator.className.split(/\s+/).includes('on');
const pulses = (count) => Array.from({ length: count * 2 + 1 }, (_, i) => event(i + 1, i % 2 === 1));

for (const kind of ['led', 'vibrator']) {
  test(`replay page: ${kind}: brief captured pulse lights the real row while current Drive remains Off`, async () => {
    const h = await replayHarness();
    const id = kind === 'led' ? 'main-hud-2' : 'handset-vibrator';
    h.show([vibratorOrLed(pulses(0), kind)]);
    assert.equal(isOn(h.row(id)), false);
    h.show([vibratorOrLed(pulses(1), kind)], 2);
    const row = h.row(id);
    assert.equal(isOn(row), true);
    assert.match(h.light(id).className, /\bon\b/);
    assert.equal(h.stateText(id), 'Pulse');
    assert.match(row.status.textContent, /^Drive: Off · PWM 0\.0%/);
    assert.match(row.replay.textContent, /Replaying 0\.100 s pulse/);
    assert.match(row.indicator.title, /Captured pulse from 0\.100 virtual s/);
    assert.match(row.historyBody.textContent, /0\.100 s · On · PWM 95\.0%/);
    assert.equal(row.entry.queue.length, 0);
    h.advance(149);
    assert.equal(isOn(row), true);
    h.advance(1);
    assert.equal(isOn(row), false);
    assert.match(h.light(id).className, /\boff\b/);
    assert.equal(h.stateText(id), 'Off');
    assert.match(row.replay.textContent, /Pulse replay gap/);
    h.advance(100);
    assert.equal(h.timers.size, 0);
    assert.equal(row.replay.textContent, 'Pulse replay caught up');
    h.show([vibratorOrLed(pulses(1), kind)], 2);
    assert.equal(h.timers.size, 0, 'Re-rendering the same state must not duplicate playback.');
  });
}

test('replay page: queued pulses have visible Off gaps and drain once despite repeated polls', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(3))], 2);
  const row = h.row();
  assert.equal(row.entry.queue.length, 2);
  assert.equal(row.entry.replayEvent.sequence, 2);
  h.show([vibratorOrLed(pulses(3))], 3);
  assert.equal(row.entry.queue.length, 2);
  h.advance(150);
  assert.equal(isOn(row), false);
  h.advance(99);
  assert.equal(isOn(row), false);
  h.advance(1);
  assert.equal(isOn(row), true);
  assert.equal(row.entry.replayEvent.sequence, 4);
  h.advance(250);
  assert.equal(row.entry.replayEvent.sequence, 6);
  h.advance(250);
  assert.equal(isOn(row), false);
  assert.equal(row.entry.queue.length, 0);
  assert.equal(h.timers.size, 0);
});

test('replay page: an accumulated retained tail is bounded and reports skipped visual pulses', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  // Thirty-one events fit the telemetry history, but fifteen distinct pulses exceed the twelve-pulse visual queue.
  // Keep its newest twelve pulses.
  h.show([vibratorOrLed(pulses(15))], 2);
  const row = h.row();
  assert.equal(row.entry.replayEvent.sequence, 8);
  assert.equal(row.entry.queue.length, 11);
  assert.equal(row.entry.queue.droppedCount, 3);
  assert.match(row.replay.textContent, /3 replay pulses skipped/);
  h.advance(12 * 250);
  assert.equal(h.timers.size, 0);
  assert.equal(row.entry.queue.length, 0);
  assert.equal(isOn(row), false);
});

test('replay page: live On stays steady and cancels queued historical pulses', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(3))], 2);
  h.show([vibratorOrLed([...pulses(3), event(8, true)])], 3);
  const row = h.row();
  assert.equal(isOn(row), true);
  assert.match(row.status.textContent, /^Drive: On/);
  assert.equal(row.replay.textContent, 'Current drive is on');
  assert.equal(row.entry.queue.length, 0);
  assert.equal(h.timers.size, 0);
  h.advance(5000);
  assert.equal(isOn(row), true);
  h.show([vibratorOrLed([...pulses(3), event(8, true), event(9, false)])], 4);
  assert.equal(isOn(row), false);
  assert.equal(h.timers.size, 0);
});

test('replay page: the checkbox cancels playback, suppresses intervening pulses and enables future pulses only', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  h.checkbox(false);
  assert.equal(isOn(h.row()), false);
  assert.equal(h.timers.size, 0);
  h.show([vibratorOrLed(pulses(3))], 3);
  assert.equal(h.timers.size, 0);
  assert.match(h.row().replay.textContent, /Pulse replay disabled/);
  h.checkbox(true);
  assert.equal(h.timers.size, 0);
  h.show([vibratorOrLed(pulses(4))], 4);
  assert.equal(h.row().entry.replayEvent.sequence, 8);
  assert.equal(h.row().entry.queue.length, 0);
});

test('replay page: a history epoch reset cancels old timers and ignores replacement backlog', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(3))], 2);
  h.show([vibratorOrLed(pulses(2))], 3, 'domain-b');
  assert.equal(h.timers.size, 0);
  assert.equal(isOn(h.row()), false);
  h.show([vibratorOrLed(pulses(3))], 4, 'domain-b');
  assert.equal(h.row().entry.replayEvent.sequence, 6);
  assert.equal(h.row().entry.queue.length, 0);
});

test('replay page: a restarted board (new host generation, same epoch text) also starts the histories over', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  assert.equal(h.row().entry.replayEvent.sequence, 2);
  h.view.onState({ state: { ...baseState, hardwareOutputs: [vibratorOrLed(pulses(3))], virtualTime: 3 }, host: { profileEpoch: 1, generation: 2 } });
  assert.equal(h.timers.size, 0, 'the old generation cannot replay into the new one');
  assert.equal(isOn(h.row()), false);
});

test('replay page: a truncated history gap cancels stale playback and recovers only definite retained rises', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(3))], 2);
  // Missing 8/9 could hide any drive transition. Event 10 establishes Off; only the retained Off10 -> On11 -> Off12
  // pulse may be replayed.
  const retained = [event(10, false), event(11, true), event(12, false)];
  h.show([vibratorOrLed(retained)], 3);
  const row = h.row();
  assert.equal(row.entry.replayEvent.sequence, 11);
  assert.equal(row.entry.queue.length, 0);
  assert.match(row.replay.textContent, /2 history events unavailable/);
  h.advance(250);
  assert.equal(h.timers.size, 0);
});

test('replay page: unknown drive and output removal cancel scheduled work', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  h.show([vibratorOrLed(pulses(2), 'led', { active: null })], 3);
  assert.equal(h.timers.size, 0);
  assert.match(h.row().indicator.className, /unknown/);
  h.show([vibratorOrLed(pulses(2))], 4);
  assert.equal(h.timers.size, 0, 'Returning from unknown establishes a baseline.');
  h.show([vibratorOrLed(pulses(3))], 5);
  assert.equal(h.timers.size, 1);
  h.show([], 6);
  assert.equal(h.timers.size, 0);
  assert.equal(h.view.outputRows.size, 0);
  assert.equal(h.view.replay.entries.size, 0);
  h.advance(1000);
  assert.equal(h.document.getElementById('output-list').children.length, 0);
});

test('replay page: connection failure clears replay and reconnect does not replay old backlog', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  h.disconnect();
  assert.equal(h.timers.size, 0);
  assert.equal(h.view.outputRows.size, 0);
  assert.match(h.document.getElementById('outputs-status').textContent, /Disconnected/);
  assert.equal(h.stateText('main-hud-2'), 'Disconnected');
  h.reconnect();
  assert.equal(h.timers.size, 0);
  assert.equal(isOn(h.row()), false);
  h.show([vibratorOrLed(pulses(3))], 3);
  assert.equal(h.row().entry.replayEvent.sequence, 6);
});

test('replay page: visibility and pagehide handlers cancel playback without stale catch-up', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  h.document.hidden = true;
  h.document.dispatch('visibilitychange');
  assert.equal(h.timers.size, 0);
  assert.deepEqual(h.sent.filter((message) => message.type === 'visibility').at(-1).payload, { hidden: true });
  h.show([vibratorOrLed(pulses(3))], 3);
  assert.equal(h.timers.size, 0);
  h.document.hidden = false;
  h.document.dispatch('visibilitychange');
  await settle();
  assert.equal(h.timers.size, 0);
  h.reconnect();
  h.show([vibratorOrLed(pulses(3))], 3);
  assert.equal(h.timers.size, 0, 'the first observation after returning is a baseline');
  h.show([vibratorOrLed(pulses(4))], 4);
  assert.equal(h.row().entry.replayEvent.sequence, 8);
  h.window.dispatch('pagehide');
  assert.equal(h.timers.size, 0);
  assert.equal(isOn(h.row()), false);
});

test('replay page: the status strip shows the red and white LEDs by channel and the vibrator, with Drive and Replay apart', async () => {
  const h = await replayHarness();
  const led = (id, color, active) => ({ id, kind: 'led', label: id, board: 'main', color, active, dutyPercent: active ? 50 : 0 });
  h.show([led('main-hud-1', 'unknown', null), led('main-hud-2', 'white', true), led('main-hud-3', 'red', false), { id: 'handset-vibrator', kind: 'vibrator', label: 'Vibrator', board: 'handset', active: false, level: true, dutyPercent: 0 }]);
  assert.equal(h.stateText('main-hud-2'), 'On');
  assert.match(h.light('main-hud-2').className, /\bon\b.*\bwhite\b/);
  assert.equal(h.stateText('main-hud-3'), 'Off');
  assert.match(h.light('main-hud-3').className, /\boff\b.*\bred\b/);
  assert.equal(h.stateText('handset-vibrator'), 'Off');
  assert.match(h.light('handset-vibrator').className, /\bvibrator\b/);
  assert.equal(h.document.getElementById('simple-main-hud-1-light'), null, 'the unidentified HUD 1 has no status card');
  assert.equal(h.view.outputRows.size, 4, 'all four outputs are listed in Advanced');
  assert.equal(h.row('main-hud-1').color.value, 'unknown');
  assert.equal(h.row('main-hud-2').color.value, 'white');
  assert.match(h.row('main-hud-2').status.textContent, /^Drive: On · PWM 50\.0%/);
  assert.ok(h.document.getElementById('replay-pulses').checked, 'Replay pulses starts enabled');
});

test('replay page: an engine without output histories shows the drive only and never replays', async () => {
  const h = await replayHarness();
  const led = (active) => ({ id: 'main-hud-2', kind: 'led', label: 'HUD 2', board: 'main', color: 'white', active, dutyPercent: 0 });
  h.show([led(false)]);
  h.show([led(false)], 2);
  assert.equal(h.timers.size, 0);
  assert.equal(h.row().replay.hidden, true);
  assert.equal(h.row().activity.hidden, true);
  h.show([led(true)], 3);
  assert.equal(isOn(h.row()), true);
});

test('replay: describeEntry, driveText and historyText present Drive, Replay and the retained commands', () => {
  const entry = { id: 'main-hud-3', kind: 'led', active: false, outputColor: 'red', cursor: Object.assign(new Cursor(), { attached: true }), queue: new PulseQueue(), replayTimer: null, replayOn: false, replayEvent: null };
  const idle = describeEntry(entry, true);
  assert.equal(idle.on, false);
  assert.equal(idle.replayText, 'Pulse replay caught up');
  assert.equal(idle.simple.stateText, 'Off');
  assert.equal(describeEntry(entry, false).replayText, 'Pulse replay disabled');
  entry.replayOn = true;
  entry.replayEvent = { virtualTime: 1.25 };
  const flash = describeEntry(entry, true);
  assert.equal(flash.on, true);
  assert.equal(flash.simple.stateText, 'Pulse');
  assert.match(flash.simple.lightClass, /\bon\b.*\bred\b/);
  assert.equal(driveText({ active: null }), 'Drive: Unknown');
  assert.equal(driveText({ active: true, dutyPercent: 12.34, level: false }), 'Drive: On · PWM 12.3% · Pin low');
  const text = historyText({ activity: { events: [event(2, true), event(3, false)], truncated: true } });
  assert.equal(text, '0.150 s · Off · PWM 0.0%\n0.100 s · On · PWM 95.0%\nEarlier commands omitted; totals retained.');
});

// ---- the UART / TTL console in the page ---------------------------------------------------------------------------
//
// Regression: the page rewrote the text of every <option> of #uart-channel (and re-assigned its value) on every state
// update, 5 times a second. A browser reacts to any change of an open select's children by rebuilding or closing its
// dropdown: the list disappeared and reappeared before a channel could be picked, and the page stopped responding.
// The page now touches the select only when the channel list really changed, and the console text only when it differs.

const uartChannel = (id, extra = {}) => ({
  id, board: id.split('.')[0], peripheral: id.split('.')[1], label: `${id} label`, txBytes: 12, lastTxVirtualTime: 1.5, truncated: false,
  text: `text of ${id}\n`, hex: `HEX OF ${id}`, ...extra,
});
const uartChannels = () => ['main.uart4', 'main.usart1', 'main.usart2', 'main.uart5', 'handset.usart3'].map((id) => uartChannel(id));

/** Counts the writes the page makes to properties of one element (a write that stores the same value counts too). */
function spyWrites(element, names) {
  const writes = Object.fromEntries(names.map((name) => [name, 0]));
  for (const name of names) {
    let own = Object.getOwnPropertyDescriptor(element, name);
    let current = element[name];
    const inherited = own ? null : Object.getOwnPropertyDescriptor(Element.prototype, name);
    Object.defineProperty(element, name, {
      configurable: true,
      get() { return inherited ? inherited.get.call(this) : current; },
      set(value) { writes[name]++; if (inherited) inherited.set.call(this, value); else current = value; },
    });
  }
  return writes;
}

async function uartHarness({ open = true } = {}) {
  const m = await mount();
  const document = m.document;
  document.getElementById('advanced-panel').open = open;
  document.getElementById('uart-panel').open = open;
  const select = document.getElementById('uart-channel');
  const pre = document.getElementById('uart-output');
  const calls = { console: 0, uart: 0 };
  for (const [name, key] of [['renderConsole', 'console'], ['renderUart', 'uart']]) {
    const original = m.view[name].bind(m.view);
    m.view[name] = (...args) => { calls[key]++; return original(...args); };
  }
  return {
    ...m, select, pre, calls,
    status: () => document.getElementById('uart-status').textContent,
    optionIds: () => select.options.map((option) => option.value),
    optionTexts: () => select.options.map((option) => option.textContent),
    /** The option elements and their text nodes: any rebuild or text rewrite replaces them (compare with `sameNodes`). */
    nodes: () => select.options.flatMap((option) => [option, option.children[0]]),
    update: (channels = uartChannels(), state = {}) => {
      m.view.onState({ state: { ...baseState, uartConsole: clone(channels), ...state }, host: { ...hostBase } });
    },
    choose: (id) => { select.value = id; select.dispatch('change'); },
  };
}

/** Object identity of two node lists (`assert.deepEqual` would compare a replaced node with its copy as equal). */
const sameNodes = (actual, expected, message) => assert.ok(actual.length === expected.length && actual.every((node, index) => node === expected[index]), message);

test('uart page: repeated state updates leave an unchanged channel select alone, so an open dropdown is never rebuilt', async () => {
  const h = await uartHarness();
  h.update();
  assert.deepEqual(h.optionIds(), ['main.uart4', 'main.usart1', 'main.usart2', 'main.uart5', 'handset.usart3']);
  assert.equal(h.optionTexts()[1], 'main · main.usart1 label');
  h.choose('main.usart2');
  assert.match(h.status(), /^12 transmitted bytes · Last TX at 1\.500 virtual s$/);
  const before = h.nodes();
  let rebuilt = 0;
  const replaceChildren = h.select.replaceChildren.bind(h.select);
  h.select.replaceChildren = (...children) => { rebuilt++; return replaceChildren(...children); };
  const writes = spyWrites(h.select, ['value', 'disabled']);
  for (let i = 1; i <= 50; i++) {
    // Live bytes arrive and the virtual time moves on; the channel list itself does not change.
    h.update(uartChannels().map((channel) => ({ ...channel, txBytes: 12 + i, lastTxVirtualTime: 1.5 + i / 10 })));
  }
  assert.equal(rebuilt, 0, 'the options are not replaced');
  assert.deepEqual(h.nodes(), before, 'neither the <option> elements nor their text nodes are replaced or rewritten');
  assert.deepEqual(writes, { value: 0, disabled: 0 }, 'nor are value or disabled assigned again');
  assert.equal(h.select.value, 'main.usart2', 'the selection survives');
  assert.match(h.status(), /^62 transmitted bytes · Last TX at 6\.500 virtual s$/, 'the console still follows the selected channel');
  assert.equal(h.pre.textContent, 'text of main.usart2\n');
});

test('uart page: a changed channel list updates the options, keeps the selection by channel and falls back when it is gone', async () => {
  const h = await uartHarness();
  h.update();
  h.choose('handset.usart3');
  const before = h.nodes();
  // One label changes: only that text is rewritten, the other options keep their nodes.
  h.update(uartChannels().map((channel) => (channel.id === 'main.uart5' ? { ...channel, label: 'Main UART5 · renamed' } : channel)));
  assert.equal(h.optionTexts()[3], 'main · Main UART5 · renamed');
  const after = h.nodes();
  for (let index = 0; index < 5; index++) {
    assert.ok(after[2 * index] === before[2 * index], `option ${index} is the same element`);
    assert.ok((after[2 * index + 1] === before[2 * index + 1]) === (index !== 3), `option ${index} ${index === 3 ? 'gets a new text' : 'keeps its text node'}`);
  }
  assert.equal(h.select.value, 'handset.usart3');
  // A channel disappears: the options follow and the selection stays on its channel although its index moved.
  h.update(uartChannels().filter((channel) => channel.id !== 'main.usart1'));
  assert.deepEqual(h.optionIds(), ['main.uart4', 'main.usart2', 'main.uart5', 'handset.usart3']);
  assert.equal(h.select.value, 'handset.usart3');
  assert.match(h.pre.textContent, /text of handset\.usart3/);
  // The selected channel disappears: the first one is selected and shown.
  h.update(uartChannels().filter((channel) => channel.id !== 'handset.usart3'));
  assert.equal(h.select.value, 'main.uart4');
  assert.match(h.pre.textContent, /text of main\.uart4/);
  // No channels, a lost connection and the way back.
  h.update([]);
  assert.deepEqual(h.optionTexts(), ['No channels']);
  assert.equal(h.select.disabled, true);
  assert.equal(h.pre.textContent, 'No live UART output available.');
  h.view.setConnectionError('Test connection lost');
  assert.deepEqual(h.optionTexts(), ['Disconnected']);
  assert.equal(h.select.disabled, true);
  h.update();
  assert.deepEqual(h.optionIds(), ['main.uart4', 'main.usart1', 'main.usart2', 'main.uart5', 'handset.usart3']);
  assert.equal(h.select.disabled, false);
  assert.equal(h.select.value, 'main.uart4');
});

test('uart page: nothing renders in a loop, a state update and a user choice each render the console once', async () => {
  const h = await uartHarness();
  h.update();
  assert.deepEqual(h.calls, { console: 1, uart: 1 });
  h.update();
  assert.deepEqual(h.calls, { console: 2, uart: 2 });
  h.choose('main.usart1');
  assert.deepEqual(h.calls, { console: 3, uart: 2 }, 'a change event renders the console, never the whole list');
  h.select.dispatch('change');
  h.document.getElementById('uart-view').value = 'hex';
  h.document.getElementById('uart-view').dispatch('change');
  assert.deepEqual(h.calls, { console: 5, uart: 2 });
  assert.equal(h.pre.textContent, 'HEX OF main.usart1');
  // Even where assigning a select's value fires a change event (a hostile environment), steady updates assign nothing,
  // so they cannot start a render loop.
  const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, 'value');
  Object.defineProperty(h.select, 'value', {
    configurable: true,
    get() { return descriptor.get.call(this); },
    set(value) { descriptor.set.call(this, value); this.dispatch('change'); },
  });
  const before = { ...h.calls };
  for (let i = 0; i < 10; i++) h.update();
  assert.deepEqual(h.calls, { console: before.console + 10, uart: before.uart + 10 });
});

test('uart page: the console text is written only when it changed, and never exceeds its bound', async () => {
  const { UART_CONSOLE_MAX_CHARS } = await import('./emulator.js');
  assert.ok(Number.isInteger(UART_CONSOLE_MAX_CHARS) && UART_CONSOLE_MAX_CHARS >= 4 * 16384, 'the bound covers the engine\'s 16 KiB tail even when every byte is shown as \\xNN');
  const h = await uartHarness();
  h.update();
  h.choose('main.uart4');
  const written = h.pre.children[0];
  for (let i = 0; i < 20; i++) h.update();
  assert.equal(h.pre.children[0], written, 'an unchanged tail does not replace the text node (which also kept a text selection alive)');
  h.update(uartChannels().map((channel) => (channel.id === 'main.uart4' ? { ...channel, text: 'a newer tail\n' } : channel)));
  assert.equal(h.pre.textContent, 'a newer tail\n');
  // Whatever the engine sends, the page shows at most the newest UART_CONSOLE_MAX_CHARS characters.
  const huge = `${'old\n'.repeat(500_000)}the newest line\n`;
  h.update(uartChannels().map((channel) => (channel.id === 'main.uart4' ? { ...channel, text: huge, txBytes: huge.length, truncated: true } : channel)));
  assert.equal(h.pre.textContent.length, UART_CONSOLE_MAX_CHARS);
  assert.ok(h.pre.textContent.endsWith('the newest line\n'));
  assert.match(h.status(), /Showing retained tail; earlier bytes omitted/);
});

test('uart page: while the console is closed the page neither reads nor writes its text, and opening it asks the worker for the bytes', async () => {
  const h = await uartHarness({ open: false });
  h.update();
  assert.equal(h.pre.textContent, 'No transmitted bytes captured yet.', 'the placeholder of index.html is untouched');
  assert.match(h.status(), /^12 transmitted bytes/, 'the status line is cheap and stays current');
  h.sent.length = 0;
  h.document.getElementById('advanced-panel').open = true;
  h.document.getElementById('uart-panel').open = true;
  h.document.getElementById('uart-panel').dispatch('toggle');
  // (a real `toggle` event does not bubble; the fake one does, so Advanced reports as well.)
  assert.ok(h.sent.length > 0 && h.sent.every((message) => message.type === 'ui' && message.payload.uartOpen === true), 'the worker is told the console is on screen');
  h.update();
  assert.equal(h.pre.textContent, 'text of main.uart4\n');
});

test('replay page: the LED color selects are not written again while nothing changed', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  const color = h.row().color;
  const writes = spyWrites(color, ['value', 'disabled']);
  const attributes = [];
  const setAttribute = color.setAttribute.bind(color);
  color.setAttribute = (name, value) => { attributes.push(name); return setAttribute(name, value); };
  for (let i = 0; i < 20; i++) h.show([vibratorOrLed(pulses(0))], 1 + i);
  assert.deepEqual(writes, { value: 0, disabled: 0 });
  assert.deepEqual(attributes, [], 'neither is its label rewritten');
  assert.equal(color.value, 'white');
  // A new color from the engine still arrives.
  h.show([{ ...vibratorOrLed(pulses(0)), color: 'red' }], 30);
  assert.equal(color.value, 'red');
});

// ---- simulated conditions in the page (analysis workspace: test_scenario_ui.js) --------------------------------------

const initialInputs = {
  battery1Mv: 4100, battery2Mv: 4100,
  oxygen1Mv: 60, oxygen2Mv: 61, oxygen3Mv: 59,
  pressure1Mbar: 1013.25, pressure2Mbar: 1013.25, temperature1C: 20, temperature2C: 20,
  acquisitionDelayUs: 0, noiseAmplitudeRaw: 0, noiseSeed: 1,
  acquisitionEnabled: true, pressureMaximumTiming: false,
};
const initialState = { running: false, virtualTime: 22, pc: 0x08008410, frameReady: false, hardwareOutputs: [], uartConsole: [], inputs: initialInputs, outputHistoryEpoch: 'test-a' };

async function scenarioHarness(inputOverrides = {}) {
  const m = await mount();
  let nextState = { ...clone(initialState), inputs: { ...initialInputs, ...inputOverrides } };
  m.view.onState({ state: clone(nextState), host: { ...hostBase } });
  const document = m.document;
  const actions = () => m.requests.filter((request) => request.type === 'action').map((request) => Object.assign(request, { body: request.payload.request }));
  const harness = {
    ...m,
    posts: actions,
    pending: () => actions().filter((request) => !request.done),
    control: (id) => document.getElementById(id),
    raw: (name) => document.getElementById('sensor-form').elements.namedItem(name),
    sendAction: (action, extra) => m.view.sendAction(action, extra),
    /** Delivers a reply: the worker broadcasts the new state, then answers the request. */
    respond: async (request, inputs = {}, options = {}) => {
      assert.ok(request && !request.done, 'Respond to one outstanding request.');
      request.done = true;
      if (options.fail) {
        request.reject(new Error(options.fail));
      } else {
        nextState = { ...nextState, ...options.state, inputs: { ...nextState.inputs, ...inputs } };
        m.view.onState({ state: clone(nextState), host: { ...hostBase } });
        request.resolve(clone(nextState));
      }
      await settle();
    },
    /** A status broadcast (5 Hz in the worker), possibly computed before an action took effect. */
    broadcast: (state = nextState) => m.view.onState({ state: clone(state), host: { ...hostBase } }),
    edit: async (id, value) => {
      const element = document.getElementById(id);
      assert.ok(element, `Actual page control ${id} exists.`);
      element.value = String(value);
      element.dispatch(element.tagName === 'SELECT' ? 'change' : 'input');
      await settle();
    },
    editRaw: async (name, value) => {
      const element = document.getElementById('sensor-form').elements.namedItem(name);
      element.value = String(value);
      element.dispatch('input');
      await settle();
    },
    submitRaw: async () => { document.getElementById('sensor-form').dispatch('submit'); await settle(); },
    status: () => document.getElementById('basic-input-status').textContent,
  };
  return harness;
}

const calculatedKeys = ['oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv', 'pressure1Mbar', 'pressure2Mbar', 'temperature1C', 'temperature2C'].sort();
const applyResponse = async (h, index) => {
  const request = h.posts()[index];
  await h.respond(request, request.body.inputs || {});
};

test('page: initial attachment derives basic controls without sending an input action', async () => {
  const h = await scenarioHarness();
  assert.equal(h.posts().length, 0);
  assert.equal(Number(h.control('oxygen-base').value), 60);
  assert.equal(Number(h.control('oxygen-offset-2').value), 1);
  assert.equal(Number(h.control('oxygen-offset-3').value), -1);
  assert.equal(Number(h.raw('oxygen2Mv').value), 61);
  assert.equal(h.control('basic-sensor-panel').hidden, false);
  assert.equal(h.status(), 'Inputs applied');
  assert.ok(!h.control('advanced-panel').open, 'Advanced starts collapsed.');
  assert.ok(h.document.querySelectorAll('details.variations').every((element) => !element.open), 'Sensor variations start collapsed.');
  assert.equal(h.document.querySelectorAll('details.variations').length, 3, 'one variations section per sensor type');
});

test('page: titles name the release; fields without a proven equivalent say so and show the engine reason; the fixture is described', async () => {
  const m = await mount();
  const reason = 'no byte-identical counterpart in the NEPTUN main image';
  m.view.onState({
    state: {
      ...baseState, inputs: { ...initialInputs }, mainBatteryReady: null, unavailable: { mainBatteryReady: reason },
      firmware: { release: { id: 'NEPTUN-5.8-65.3', label: 'NEPTUN main 5.8 / handset 65.3' } },
      i2cIdleHigh: true, i2cFixture: 'Main I2C idle inputs PB6/PB7/PB10/PB11 driven high (functional idle-line fixture)',
    },
    host: { ...hostBase, release: describeRelease('TRITON-5.8-65.3') },
  });
  assert.equal(m.document.title, 'NGC system emulator · NEPTUN · WebAssembly', 'the engine\'s own release report wins over what the page verified');
  assert.match(m.document.getElementById('subtitle').textContent, /^NEPTUN main 5\.8 \+ handset 65\.3/);
  assert.equal(m.document.getElementById('battery-ready').textContent, 'Unavailable for this release');
  assert.equal(m.document.getElementById('unavailable-info').hidden, false);
  assert.match(m.document.getElementById('unavailable-info').textContent, /Main batteries ready: not reported for this release \(no byte-identical counterpart/);
  m.document.getElementById('firmware-details').open = true;
  m.view.render();
  assert.match(m.document.getElementById('session-info').textContent, /Firmware release: NEPTUN main 5\.8 \/ handset 65\.3 \(NEPTUN-5\.8-65\.3\)\./);
  assert.match(m.document.getElementById('session-info').textContent, /Fixture: Main I2C idle inputs PB6\/PB7\/PB10\/PB11 driven high/);
  assert.match(m.document.getElementById('profile-status').textContent, /^NEPTUN profile/);
  // A TRITON state afterwards: nothing is unavailable, the battery state is shown.
  m.view.onState({ state: { ...baseState, inputs: { ...initialInputs }, mainBatteryReady: true, unavailable: {}, firmware: { release: { id: 'TRITON-5.8-65.3', label: 'TRITON main 5.8 / handset 65.3' } } }, host: { ...hostBase } });
  assert.equal(m.document.getElementById('battery-ready').textContent, 'Yes');
  assert.equal(m.document.getElementById('unavailable-info').hidden, true);
  assert.equal(m.document.title, 'NGC system emulator · TRITON · WebAssembly');
  m.view.hide();
  assert.equal(m.document.title, 'NGC system emulator · WebAssembly');
});

test('page: the first LCD frame, which the worker sends while it creates the session, survives the session being shown', async () => {
  // The worker posts its frame before it answers the boot request, so the page sees `frame` before `show()`. A
  // steady screen (or a hidden page) sends no further frame for a long time: forgetting this one left the first
  // session of a page load without a picture while "Restart boards" (a new forced frame) showed it.
  installDom(html);
  const { EmulatorView } = await import('./emulator.js');
  const sent = [];
  const client = { send: (type, payload) => sent.push({ type, payload }), request: () => new Promise(() => {}) };
  const view = new EmulatorView(client, { closeSession() {}, notify() {} }, { timers: fakeTimers() });
  const info = { options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: describeRelease(DEFAULT_RELEASE_ID), slots: { main: fakeSlot('main.srec'), handset: fakeSlot('handset.srec') } };
  const frame = (version) => ({ type: 'frame', width: 320, height: 240, version, generation: 1, buffer: new ArrayBuffer(320 * 240 * 4) });
  const canvas = () => globalThis.document.getElementById('frame');
  const placeholder = () => globalThis.document.getElementById('placeholder');
  view.onFrame(frame(1));
  assert.equal(sent.filter((message) => message.type === 'recycle').length, 1, 'the drawn buffer goes back to the worker');
  view.show(info);
  view.onState({ state: { ...baseState, frameReady: true }, host: { ...hostBase } });
  assert.equal(canvas().hidden, false, 'the frame received before show() is displayed once the panel is on');
  assert.equal(placeholder().hidden, true);
  assert.match(globalThis.document.getElementById('frame-note').textContent, /^Last LCD frame: .* · frame 1$/, 'the note no longer says "No complete LCD frame" while a frame is shown, although no newer frame follows');
  // Before the panel is on the placeholder stays; the session ending forgets the frame, and a new session starts over.
  view.onState({ state: { ...baseState, frameReady: false }, host: { ...hostBase } });
  assert.equal(canvas().hidden, true);
  assert.equal(globalThis.document.getElementById('frame-note').textContent, 'No complete LCD frame is available yet.');
  view.hide();
  view.show(info);
  view.onState({ state: { ...baseState, frameReady: true }, host: { ...hostBase } });
  assert.equal(canvas().hidden, true, 'a closed session does not leave its frame behind');
  assert.equal(placeholder().textContent, 'Waiting for LCD snapshot');
  view.onFrame(frame(2));
  assert.equal(canvas().hidden, false);
  assert.equal(placeholder().hidden, true);
  assert.match(globalThis.document.getElementById('frame-note').textContent, /Last LCD frame: .* · frame 2/);
});

test('page: a handset-only session has no sensor controls', async () => {
  const m = await mount();
  m.view.onState({ state: { ...baseState, inputs: undefined }, host: { ...hostBase } });
  assert.equal(m.document.getElementById('basic-sensor-panel').hidden, true);
  assert.equal(m.document.getElementById('sensor-panel').hidden, true);
});

test('page: basic edits immediately send calculated fields while unrelated raw drafts remain unapplied', async () => {
  const h = await scenarioHarness();
  await h.editRaw('battery1Mv', 3100);
  await h.editRaw('noiseAmplitudeRaw', 20);
  assert.equal(h.posts().length, 0, 'Typing raw drafts does not apply them.');
  assert.equal(h.status(), 'Raw edits pending');
  await h.edit('oxygen-base', 70);
  assert.equal(h.posts().length, 1);
  const { body } = h.posts()[0];
  assert.equal(body.action, 'inputs');
  assert.deepEqual(Object.keys(body.inputs).sort(), calculatedKeys);
  assert.deepEqual([body.inputs.oxygen1Mv, body.inputs.oxygen2Mv, body.inputs.oxygen3Mv], [70, 71, 69]);
  assert.equal(Number(h.raw('oxygen2Mv').value), 71);
  assert.equal(h.status(), 'Applying…');
  await applyResponse(h, 0);
  assert.equal(Number(h.raw('battery1Mv').value), 3100);
  assert.equal(Number(h.raw('noiseAmplitudeRaw').value), 20);
  assert.equal(h.status(), 'Inputs applied');
});

test('page: slider updates during another action retain only the latest pending basic values', async () => {
  const h = await scenarioHarness();
  h.sendAction('down');
  await settle();
  await h.edit('oxygen-base', 70);
  await h.edit('oxygen-base', 75);
  await h.edit('oxygen-base', 80);
  assert.deepEqual(h.posts().map((request) => request.body.action), ['down']);
  await applyResponse(h, 0);
  assert.deepEqual(h.posts().map((request) => request.body.action), ['down', 'inputs']);
  assert.deepEqual([1, 2, 3].map((index) => h.posts()[1].body.inputs[`oxygen${index}Mv`]), [80, 81, 79]);
  await applyResponse(h, 1);
  assert.equal(h.pending().length, 0);
  assert.equal(Number(h.control('oxygen-base').value), 80);
});

test('page: explicit raw Apply preserves an immutable ordering barrier between basic edits', async () => {
  const h = await scenarioHarness();
  h.sendAction('down');
  await settle();
  await h.edit('oxygen-base', 65);
  await h.editRaw('oxygen1Mv', 120);
  await h.editRaw('oxygen2Mv', 130);
  await h.editRaw('oxygen3Mv', 140);
  await h.editRaw('battery1Mv', 1800);
  await h.submitRaw();
  await h.edit('oxygen-base', 70);
  assert.equal(h.posts().length, 1);
  await applyResponse(h, 0);
  assert.equal(h.posts()[1].body.inputs.oxygen1Mv, 65);
  await applyResponse(h, 1);
  assert.deepEqual([1, 2, 3].map((index) => h.posts()[2].body.inputs[`oxygen${index}Mv`]), [120, 130, 140]);
  assert.equal(h.posts()[2].body.inputs.battery1Mv, 1800);
  await applyResponse(h, 2);
  assert.equal(Number(h.control('oxygen-base').value), 70, 'Older raw success cannot rebase a later basic edit.');
  assert.deepEqual([1, 2, 3].map((index) => h.posts()[3].body.inputs[`oxygen${index}Mv`]), [70, 71, 69]);
  assert.equal(Object.hasOwn(h.posts()[3].body.inputs, 'battery1Mv'), false);
  await applyResponse(h, 3);
  assert.equal(h.posts().length, 4);
  assert.equal(h.pending().length, 0);
});

test('page: status broadcasts preserve unsubmitted raw drafts and never apply them', async () => {
  const h = await scenarioHarness();
  await h.editRaw('oxygen1Mv', 77);
  await h.editRaw('battery1Mv', 2100);
  h.broadcast({ ...initialState, virtualTime: 23 });
  await settle();
  assert.equal(Number(h.raw('oxygen1Mv').value), 77);
  assert.equal(Number(h.raw('battery1Mv').value), 2100);
  assert.equal(Number(h.control('oxygen-base').value), 60);
  assert.equal(h.posts().length, 0);
  assert.equal(h.status(), 'Raw edits pending');
});

test('page: raw Apply success rebases basic controls while a newer raw draft remains intact', async () => {
  const h = await scenarioHarness();
  await h.editRaw('oxygen1Mv', 80);
  await h.editRaw('oxygen2Mv', 90);
  await h.editRaw('oxygen3Mv', 100);
  await h.editRaw('battery1Mv', 1800);
  await h.submitRaw();
  await h.editRaw('battery1Mv', 1900);
  await applyResponse(h, 0);
  assert.equal(h.posts()[0].body.inputs.battery1Mv, 1800);
  assert.equal(Number(h.raw('battery1Mv').value), 1900, 'A response cannot erase a newer unsubmitted edit.');
  assert.equal(Number(h.control('oxygen-base').value), 90);
  assert.deepEqual([1, 2, 3].map((index) => Number(h.control(`oxygen-offset-${index}`).value)), [-10, 0, 10]);
  assert.equal(h.posts().length, 1);
  assert.equal(h.status(), 'Raw edits pending', 'the newer draft keeps the status honest');
});

test('page: invalid calculated sums and incomplete numeric edits do not send actions', async () => {
  const h = await scenarioHarness();
  await h.edit('oxygen-offset-1', -100);
  assert.equal(h.posts().length, 0, 'Negative physical oxygen rejects the complete patch.');
  assert.equal(h.control('basic-input-error').hidden, false);
  assert.match(h.control('basic-input-error').textContent, /Oxygen cell 1 must be between 0 and 250\. Changes have not been applied\./);
  assert.equal(h.status(), 'Not applied');
  await h.edit('oxygen-offset-1', '');
  assert.equal(h.posts().length, 0, 'Blank is not silently interpreted as zero.');
  assert.match(h.control('basic-input-error').textContent, /Oxygen cell 1 offset needs a number/);
  await h.edit('oxygen-offset-1', 0);
  assert.equal(h.posts().length, 1, 'A corrected draft applies without a separate button.');
  assert.equal(h.control('basic-input-error').hidden, true);
  await applyResponse(h, 0);
  await h.edit('surface-pressure', '');
  assert.equal(h.posts().length, 1);
  await h.edit('temperature-offset-2', 100);
  assert.equal(h.posts().length, 1);
  assert.equal(h.status(), 'Not applied');
});

test('page: failed raw Apply preserves its draft and exposes the error without an automatic retry', async () => {
  const h = await scenarioHarness();
  await h.editRaw('oxygen1Mv', 88);
  await h.submitRaw();
  await h.respond(h.posts()[0], {}, { fail: 'Deliberate input rejection' });
  assert.equal(Number(h.raw('oxygen1Mv').value), 88);
  assert.equal(Number(h.control('oxygen-base').value), 60);
  assert.match(h.control('error').textContent, /Deliberate input rejection/);
  assert.equal(h.status(), 'Not applied');
  assert.equal(h.posts().length, 1);
  assert.equal(h.pending().length, 0);
  await h.submitRaw();
  assert.equal(h.posts().length, 2, 'Only an explicit retry sends the retained raw draft again.');
  await applyResponse(h, 1);
});

test('page: a late status broadcast from before an input action cannot replace the applied fields', async () => {
  const h = await scenarioHarness();
  const stale = clone(initialState);
  await h.edit('oxygen-base', 70);
  assert.equal(h.posts().length, 1);
  await applyResponse(h, 0);
  assert.equal(Number(h.control('oxygen-base').value), 70);
  h.broadcast(stale); // computed before the action, delivered after its reply
  await settle();
  assert.equal(Number(h.raw('oxygen1Mv').value), 70);
  assert.equal(Number(h.control('oxygen-base').value), 70);
  assert.equal(h.posts().length, 1);
});

test('page: a newer raw sensor draft prevents an older raw success from rebasing basic controls', async () => {
  const h = await scenarioHarness();
  await h.editRaw('oxygen1Mv', 80);
  await h.editRaw('oxygen2Mv', 90);
  await h.editRaw('oxygen3Mv', 100);
  await h.submitRaw();
  await h.editRaw('oxygen1Mv', 140);
  await applyResponse(h, 0);
  assert.equal(Number(h.raw('oxygen1Mv').value), 140);
  assert.equal(Number(h.control('oxygen-base').value), 60, 'Keep the existing basic basis while a newer sensor draft is unsubmitted.');
  await h.submitRaw();
  assert.equal(h.posts()[1].body.inputs.oxygen1Mv, 140);
  await applyResponse(h, 1);
  assert.equal(Number(h.control('oxygen-base').value), 100, 'Successful explicit retry imports the newest raw sensor snapshot.');
  assert.deepEqual([1, 2, 3].map((index) => Number(h.control(`oxygen-offset-${index}`).value)), [40, -10, 0]);
});

test('page: an invalid latest basic edit cancels its pending values without erasing a prior raw ordering barrier', async () => {
  const h = await scenarioHarness();
  h.sendAction('down');
  await settle();
  await h.edit('oxygen-base', 65);
  await h.editRaw('oxygen1Mv', 120);
  await h.submitRaw();
  await h.edit('oxygen-base', 70);
  await h.edit('oxygen-offset-1', -100);
  assert.equal(h.posts().length, 1);
  await applyResponse(h, 0);
  assert.equal(h.posts()[1].body.inputs.oxygen1Mv, 65, 'Preserve the earlier valid input request ahead of raw Apply.');
  await applyResponse(h, 1);
  assert.equal(h.posts()[2].body.inputs.oxygen1Mv, 120, 'Keep the explicit raw Apply snapshot.');
  await applyResponse(h, 2);
  assert.equal(h.posts().length, 3, 'The superseded latest basic values never apply.');
  assert.equal(h.pending().length, 0);
  assert.equal(h.control('basic-input-error').hidden, false, 'The latest invalid draft remains visible after earlier requests succeed.');
});

test('page: slider step quantization on attachment preserves exact raw readings through compensating variations', async () => {
  const supplied = {
    oxygen1Mv: 62.02, oxygen2Mv: 62.03, oxygen3Mv: 62.06,
    pressure1Mbar: 1499.97, pressure2Mbar: 1500.11, temperature1C: 20.001, temperature2C: 20.009,
  };
  const h = await scenarioHarness(supplied);
  assert.equal(Number(h.control('oxygen-base').value), 62.04);
  assert.equal(h.posts().length, 0);
  await h.edit('temperature-base', 21);
  const applied = h.posts()[0].body.inputs;
  for (const key of ['oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv', 'pressure1Mbar', 'pressure2Mbar']) {
    assert.ok(Math.abs(applied[key] - supplied[key]) < 1e-9, `${key} must not drift after range step quantization.`);
  }
  await applyResponse(h, 0);
});

test('page: readings on a limit attach without error even though the sliders quantize them', async () => {
  const h = await scenarioHarness({ oxygen1Mv: 250, oxygen2Mv: 250, oxygen3Mv: 249.5, temperature1C: 85, temperature2C: -20, pressure1Mbar: 30000, pressure2Mbar: 100 });
  assert.equal(h.control('basic-input-error').hidden, true);
  await h.edit('oxygen-base', 20);
  assert.equal(h.posts().length, 1);
  await applyResponse(h, 0);
});

test('page: depth, water type and signed sensor offsets send the approved hydrostatic pressures', async () => {
  const h = await scenarioHarness();
  await h.edit('water-type', 'salt');
  await applyResponse(h, 0);
  await h.edit('depth', 110);
  assert.ok(Math.abs(h.posts()[1].body.inputs.pressure1Mbar - 12070.247875) < 1e-9);
  await applyResponse(h, 1);
  await h.edit('pressure-offset-1', -25); // sensor 1 is the firmware's first sensor, the engine's pressure2Mbar (I2C2)
  assert.ok(Math.abs(h.posts()[2].body.inputs.pressure2Mbar - 12045.247875) < 1e-9);
  assert.ok(Math.abs(h.posts()[2].body.inputs.pressure1Mbar - 12070.247875) < 1e-9);
  await applyResponse(h, 2);
  assert.match(h.control('preview-pressure').textContent, /P1: 12045\.25 · P2: 12070\.25 mbar/);
  assert.equal(h.control('depth-value').textContent, '110.00 m');
});

test('page: sensor numbers are the firmware\'s in the basic view, the previews and the raw fields: sensor 1 is the I2C2 device (pressure2Mbar, temperature2C)', async () => {
  // Attaching derives the basic offsets from the engine's inputs: sensor 1 is the I2C2 reading, sensor 2 the I2C1 reading.
  const h = await scenarioHarness({ pressure1Mbar: 1000, pressure2Mbar: 1030, temperature1C: 18, temperature2C: 22 });
  assert.ok(Math.abs(Number(h.control('pressure-offset-1').value) - Number(h.control('pressure-offset-2').value) - 30) < 1e-9);
  assert.equal(Number(h.control('temperature-offset-1').value), 2);
  assert.equal(Number(h.control('temperature-offset-2').value), -2);
  assert.equal(h.control('preview-pressure').textContent, 'P1: 1030.00 · P2: 1000.00 mbar');
  assert.equal(h.control('preview-temperature').textContent, 'T1: 22.00 · T2: 18.00 °C');
  // An edit of the basic sensor 2 offset reaches the I2C1 inputs (pressure1Mbar, temperature1C) and leaves the I2C2 ones alone.
  await h.edit('temperature-offset-2', 5);
  assert.equal(h.posts()[0].body.inputs.temperature1C, 25);
  assert.equal(h.posts()[0].body.inputs.temperature2C, 22);
  assert.equal(h.control('preview-temperature').textContent, 'T1: 22.00 · T2: 25.00 °C');
  await applyResponse(h, 0);
  await h.edit('pressure-offset-1', Number(h.control('pressure-offset-1').value) + 10);
  assert.ok(Math.abs(h.posts()[1].body.inputs.pressure2Mbar - 1040) < 1e-9, 'sensor 1 is pressure2Mbar');
  assert.ok(Math.abs(h.posts()[1].body.inputs.pressure1Mbar - 1000) < 1e-9);
  // Rejections name the page's sensor number, not the engine key.
  await h.edit('pressure-offset-1', 1e6);
  assert.match(h.control('basic-input-error').textContent, /^Pressure sensor 1 must be between 100 and 30000\./);
  // The raw fields keep the engine names and say which sensor each is, in the page's order.
  const labelOf = (name) => h.raw(name).closest('label').textContent;
  assert.equal(labelOf('pressure2Mbar'), 'Pressure sensor 1 (I2C2, absolute mbar)');
  assert.equal(labelOf('pressure1Mbar'), 'Pressure sensor 2 (I2C1, absolute mbar)');
  assert.equal(labelOf('temperature2C'), 'Temperature sensor 1 (I2C2, °C)');
  assert.equal(labelOf('temperature1C'), 'Temperature sensor 2 (I2C1, °C)');
  const names = h.document.getElementById('sensor-form').querySelectorAll('input').map((input) => input.name);
  assert.ok(names.indexOf('pressure2Mbar') < names.indexOf('pressure1Mbar') && names.indexOf('temperature2C') < names.indexOf('temperature1C'), 'sensor 1 is listed first');
  // One short note explains the mapping where the engine names are shown.
  const note = h.control('sensor-numbering-note').textContent;
  assert.match(note, /sensor 1 is the MS5837 on I2C2 \(pressure2Mbar, temperature2C\)/);
  assert.match(note, /sensor 2 the one on I2C1 \(pressure1Mbar, temperature1C\)/);
});

test('page: keyboard disclosure controls do not confirm in the guest, and the arrow keys only ever drive the handset', async () => {
  const h = await scenarioHarness();
  const summaries = h.document.querySelectorAll('#advanced-panel summary, details.variations summary');
  assert.ok(summaries.length >= 4, 'Exercise the actual Advanced and three variation disclosures.');
  for (const summary of summaries) h.document.dispatch('keydown', { target: summary, key: 'Enter' });
  await settle();
  assert.equal(h.posts().length, 0, 'Opening Advanced or variations via keyboard must not confirm in the guest.');
  // The arrow keys belong to the handset wherever the focus is (a disclosure too), never scroll (a held key either),
  // and a held key is one press.
  const lcd = h.control('lcd');
  const onSummary = h.document.dispatch('keydown', { target: summaries[0], key: 'ArrowDown' });
  assert.equal(onSummary.defaultPrevented, true);
  h.document.dispatch('keydown', { target: lcd, key: 'Enter' });
  const held = h.document.dispatch('keydown', { target: lcd, key: 'ArrowUp', repeat: true });
  assert.equal(held.defaultPrevented, true, 'a held arrow does not scroll the page');
  await settle();
  assert.deepEqual(h.posts().map((request) => request.body.action), ['down']);
  await h.respond(h.posts()[0]);
  assert.deepEqual(h.posts().map((request) => request.body.action), ['down', 'confirm']);
});

test('page: calculated hydrostatic values remain valid raw drafts for a later explicit Apply', async () => {
  const h = await scenarioHarness();
  await h.edit('depth', 110);
  await applyResponse(h, 0);
  const calculatedPressure = h.posts()[0].body.inputs.pressure1Mbar;
  assert.ok(Math.abs(calculatedPressure - 12016.3113) < 1e-9);
  assert.ok(h.raw('pressure1Mbar').checkValidity(), 'Fractional calculated pressure must satisfy the real raw field constraints.');
  await h.editRaw('battery1Mv', 2200);
  await h.submitRaw();
  assert.equal(h.posts().length, 2, 'Raw Apply cannot be blocked by decimal step mismatch in a generated sensor value.');
  assert.equal(h.posts()[1].body.inputs.pressure1Mbar, calculatedPressure);
  assert.equal(h.posts()[1].body.inputs.battery1Mv, 2200);
  await applyResponse(h, 1);
});

test('page: a raw field that violates its own range or step is not sent', async () => {
  const h = await scenarioHarness();
  await h.editRaw('battery1Mv', 1500.5); // step 1
  await h.submitRaw();
  assert.equal(h.posts().length, 0);
  assert.match(h.control('error').textContent, /Complete the raw inputs before applying\./);
  await h.editRaw('battery1Mv', 4300); // above max
  await h.submitRaw();
  assert.equal(h.posts().length, 0);
  await h.editRaw('battery1Mv', '');
  await h.submitRaw();
  assert.equal(h.posts().length, 0);
});

test('page: a new profile epoch (boot, import, reset) reloads every field and discards drafts', async () => {
  const h = await scenarioHarness();
  await h.editRaw('battery1Mv', 2222);
  await h.edit('oxygen-base', 70);
  await applyResponse(h, 0);
  h.view.onState({ state: { ...clone(initialState), inputs: { ...initialInputs, oxygen1Mv: 30, oxygen2Mv: 30, oxygen3Mv: 30, battery1Mv: 1700 } }, host: { profileEpoch: 2, generation: 2 } });
  assert.equal(Number(h.raw('battery1Mv').value), 1700);
  assert.equal(Number(h.control('oxygen-base').value), 30);
  assert.equal(h.status(), 'Inputs applied');
  assert.equal(h.posts().length, 1, 'attaching never sends an action');
});

test('page: the serial number field accepts nine digits and the action carries it as a number', async () => {
  const h = await scenarioHarness();
  const field = h.document.getElementById('serial-form').elements.namedItem('serialNumber');
  assert.equal(field.max, '999999999');
  assert.equal(field.min, '0');
  field.value = '123456789';
  h.document.getElementById('serial-form').dispatch('submit');
  await settle();
  // A serial change recreates the boards, so it carries the surface pressure of the basic view like Restart does.
  assert.deepEqual(h.posts().at(-1).body, { action: 'serial', serialNumber: 123456789, surfacePressureMbar: 1013.25 });
});

test('page: every action goes through one queue, in order, and a failure is shown once and cleared by the next action', async () => {
  const h = await scenarioHarness();
  h.sendAction('step');
  h.sendAction('up');
  h.sendAction('led-colors', { colors: { 'main-hud-1': 'red' } });
  await settle();
  assert.deepEqual(h.posts().map((request) => request.body.action), ['step'], 'only one request is in flight');
  await h.respond(h.posts()[0], {}, { fail: 'No can do' });
  assert.match(h.control('error').textContent, /No can do/);
  assert.deepEqual(h.posts().map((request) => request.body.action), ['step', 'up']);
  await h.respond(h.posts()[1]);
  assert.equal(h.control('error').hidden, true, 'the next action clears the earlier error');
  await h.respond(h.posts()[2]);
  assert.equal(h.pending().length, 0);
});

test('page: Restart, Cold boot, Wake and a new serial number start the output histories over', async () => {
  const h = await replayHarness();
  h.show([vibratorOrLed(pulses(0))]);
  h.show([vibratorOrLed(pulses(2))], 2);
  assert.equal(h.timers.size, 1);
  h.view.sendAction('reset');
  await settle();
  assert.equal(h.timers.size, 0, 'the restart clears pending flashes before it is sent');
  assert.equal(h.row().entry.cursor.attached, false);
});

// ---- the page itself ---------------------------------------------------------------------------------

test('structure: every element the scripts look up exists in index.html, and the page honors its CSP', () => {
  const ids = new Set([...html.matchAll(/\bid="([\w-]+)"/g)].map((match) => match[1]));
  for (const file of ['emulator.js', 'entry.js', 'app.js', 'game.js']) {
    const source = fs.readFileSync(path.join(here, file), 'utf8');
    for (const match of source.matchAll(/byId\('([\w-]+)'\)/g)) assert.ok(ids.has(match[1]), `${file} uses #${match[1]}, which index.html lacks`);
  }
  for (const id of BASIC_IDS) assert.ok(ids.has(id), `basic control #${id}`);
  for (const id of ['handset-vibrator', 'main-hud-3', 'main-hud-2']) {
    assert.ok(ids.has(`simple-${id}-light`) && ids.has(`simple-${id}-state`), `status strip entry for ${id}`);
  }
  assert.doesNotMatch(html, /\sstyle=/, 'inline style attributes would be blocked by the style-src policy');
  assert.doesNotMatch(html, /<script(?![^>]*\ssrc=)/, 'no inline scripts');
  assert.doesNotMatch(html, /(?:src|href)="https?:/, 'no external resources');
  assert.match(html, /Content-Security-Policy/);
  assert.match(html, /<details id="advanced-panel" class="advanced-panel">/, 'Advanced starts closed');
  assert.match(html, /<input id="replay-pulses" type="checkbox" checked>/, 'Replay pulses starts enabled');
  assert.doesNotMatch(html, /<details class="variations" open/, 'Sensor variations start closed');
  assert.match(html, /name="serialNumber"[^>]*max="999999999"/);
  assert.match(html, /<input type="checkbox" id="start-i2c-idle" checked>/, 'the I2C idle-high fixture is a start option, on by default');
  assert.doesNotMatch(html, /start-eeprom-init|start-deco-fixture/, 'the EEPROM factory image and the older repair have no start option any more');
  assert.match(html, /<input type="checkbox" id="start-surface" checked>/, 'the start at the surface is a start option, on by default');
  assert.match(html, /<input type="checkbox" id="remember" checked>/, 'Remember these files starts checked');
});

test('structure: the raw sensor fields keep the engine ranges and accept any decimal', () => {
  const field = (name) => new RegExp(`<input name="${name}"([^>]*)>`).exec(html)[1];
  for (const name of ['oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv']) assert.match(field(name), /min="0" max="250" step="any"/);
  for (const name of ['pressure1Mbar', 'pressure2Mbar']) assert.match(field(name), /min="100" max="30000" step="any"/);
  for (const name of ['temperature1C', 'temperature2C']) assert.match(field(name), /min="-20" max="85" step="any"/);
  assert.match(field('battery1Mv'), /min="0" max="4200" step="1"/);
  // A fresh profile starts with 4100 mV batteries (the engine's default; the page's field only shows it until the engine's value arrives).
  for (const name of ['battery1Mv', 'battery2Mv']) assert.match(field(name), /value="4100"/, name);
});

test('keys: the arrow keys always drive the handset; Enter confirms except where it belongs to the control', () => {
  const target = (tagName, extra = {}) => ({ tagName, closest: () => null, ...extra });
  const press = (key, element, extra = {}) => handsetKeyAction({ key, target: element, ...extra });
  const inDialog = target('BUTTON', { closest: (selector) => (/dialog/.test(selector) ? {} : null) });
  const inConsole = target('DIV', { closest: (selector) => (/uart-panel/.test(selector) ? {} : null) });
  assert.equal(press('ArrowUp', target('BODY')), 'up');
  assert.equal(press('ArrowDown', target('DIV')), 'down');
  assert.equal(press('Enter', target('BODY')), 'confirm');
  assert.equal(press('a', target('BODY')), null);
  for (const element of [...['INPUT', 'TEXTAREA', 'SELECT', 'SUMMARY', 'A', 'BUTTON'].map((tag) => target(tag)), target('DIV', { isContentEditable: true }), inConsole]) {
    assert.equal(press('ArrowDown', element), 'down', `ArrowDown on ${element.tagName}`);
    assert.equal(isHandsetArrow({ key: 'ArrowUp', target: element }), true, `no scroll or field change on ${element.tagName}`);
    assert.equal(press('Enter', element), null, `Enter belongs to ${element.tagName}`);
  }
  assert.equal(press('ArrowDown', inDialog), null, 'an open modal dialog keeps its keys');
  assert.equal(isHandsetArrow({ key: 'ArrowDown', target: inDialog }), false);
  for (const modifier of ['altKey', 'ctrlKey', 'metaKey', 'shiftKey']) assert.equal(press('ArrowDown', target('BODY'), { [modifier]: true }), null, modifier);
  assert.equal(isHandsetArrow({ key: 'ArrowDown', target: target('BODY'), metaKey: true }), false, 'browser shortcuts stay');
  assert.equal(press('ArrowDown', target('BODY'), { repeat: true }), null, 'a held key is one press');
  assert.equal(isHandsetArrow({ key: 'ArrowDown', target: target('BODY'), repeat: true }), true, 'and is still no scroll');
  assert.equal(press('ArrowDown', target('BODY'), { defaultPrevented: true }), null);
});

test('conditions: the action queue serializes, coalesces adjacent pending basic updates and reports every outcome', async () => {
  const calls = [];
  const queue = new ActionQueue({
    perform: (payload) => new Promise((resolve, reject) => calls.push({ payload, resolve, reject })),
    onBusy: (busy) => calls.push({ busy }),
  });
  const first = queue.send('down');
  const second = queue.send('inputs', { inputs: { a: 1 } }, { kind: 'basic' });
  const third = queue.send('inputs', { inputs: { a: 2 } }, { kind: 'basic' });
  const raw = queue.send('inputs', { inputs: { a: 3 } }, { kind: 'raw' });
  const fourth = queue.send('inputs', { inputs: { a: 4 } }, { kind: 'basic' });
  assert.equal(await second, false, 'a superseded pending update resolves false');
  assert.equal(queue.pending, 3);
  const performs = () => calls.filter((call) => call.payload);
  assert.equal(performs().length, 1);
  performs()[0].resolve({});
  assert.equal(await first, true);
  await settle();
  assert.equal(performs()[1].payload.inputs.a, 2);
  performs()[1].reject(new Error('refused'));
  assert.equal(await third, false);
  await settle();
  assert.equal(performs()[2].payload.inputs.a, 3, 'the raw snapshot keeps its place');
  performs()[2].resolve({});
  assert.equal(await raw, true);
  await settle();
  performs()[3].resolve({});
  assert.equal(await fourth, true);
  await settle();
  assert.equal(queue.busy, false);
  assert.deepEqual(calls.filter((call) => 'busy' in call).map((call) => call.busy), [true, false]);
  queue.send('x', {}, { kind: 'basic' });
  queue.send('y', {}, { kind: 'basic' });
  queue.send('z', {}, { kind: 'raw' });
  queue.dropQueued('basic');
  assert.equal(queue.pending, 1);
});

test('conditions: a controller on a fake view derives, applies and reports without any DOM', async () => {
  const fields = new Map();
  const raw = new Map();
  const log = [];
  const view = {
    getBasic: (id) => fields.get(id) ?? '',
    setBasic: (id, value) => fields.set(id, String(value)),
    getRaw: (name) => raw.get(name) ?? '',
    setRaw: (name, value) => raw.set(name, value),
    rawFields: () => [...raw.keys()].map((name) => ({ name, checkbox: typeof raw.get(name) === 'boolean' })),
    rawValid: () => true,
    renderPreview: () => log.push('preview'),
    setStatus: (text) => log.push(`status:${text}`),
    setError: (text) => log.push(`error:${text}`),
  };
  const sent = [];
  const queue = new ActionQueue({ perform: async (payload) => { sent.push(payload); return { inputs: payload.inputs }; }, onResult: (request, state) => controller.handleResult(request, state) });
  const controller = new ConditionsController({ view, queue });
  controller.attach({ ...initialInputs });
  assert.equal(fields.get('oxygen-base'), '60');
  assert.equal(fields.get('water-type'), 'en13319');
  fields.set('oxygen-base', '70');
  assert.equal(await controller.basicChanged(), true);
  assert.deepEqual(Object.keys(sent[0].inputs).sort(), calculatedKeys);
  fields.set('depth', 'x');
  assert.equal(await controller.basicChanged(), false);
  assert.equal(sent.length, 1);
  assert.ok(log.some((line) => /^error:Depth needs a number\. Changes have not been applied\.$/.test(line)));
  assert.equal(log.at(-1), 'status:Not applied');
});

// ---- releases, session options and the worker-side logic on a fake engine -------------------------------

test('releases: the table, the report mapping and the per-release storage areas', () => {
  assert.deepEqual(Object.keys(RELEASES), ['TRITON-5.8-65.3', 'NEPTUN-5.8-65.3']);
  assert.equal(profileArea('TRITON-5.8-65.3'), 'profile', 'TRITON keeps the location of the first version');
  assert.equal(profileArea('NEPTUN-5.8-65.3'), 'profile-neptun-5_8-65_3');
  assert.notEqual(profileArea('NEPTUN-5.8-65.3'), profileArea('TRITON-5.8-65.3'));
  assert.deepEqual(storageAreas(), ['firmware', 'profile', 'profile-neptun-5_8-65_3', 'firmware-custom', 'custom']);
  assert.deepEqual(releaseOf({ ok: true, release: { id: 'NEPTUN-5.8-65.3', label: 'NEPTUN 5.8 / 65.3' } }), { id: 'NEPTUN-5.8-65.3', name: 'NEPTUN', label: 'NEPTUN 5.8 / 65.3' });
  assert.deepEqual(releaseOf({ ok: true, release: { id: 'TRITON-5.8-65.3' } }), describeRelease('TRITON-5.8-65.3'));
  assert.equal(releaseOf({ ok: true }).id, 'TRITON-5.8-65.3', 'an engine without the release table only knew TRITON');
  assert.equal(releaseOf({ ok: true, release: null }), null);
  assert.equal(releaseOf({ ok: false, release: { id: 'X' } }), null);
  assert.deepEqual(describeRelease('ACME-1'), { id: 'ACME-1', name: 'ACME', label: 'ACME-1' });
  assert.equal(pairConflict({ main: { release: describeRelease('TRITON-5.8-65.3') }, handset: { release: describeRelease('NEPTUN-5.8-65.3') } }).handset.name, 'NEPTUN');
  assert.equal(pairConflict({ main: { release: describeRelease('TRITON-5.8-65.3') }, handset: null }), null);
  assert.match(mixedPairMessage({ role: 'main', release: describeRelease('NEPTUN-5.8-65.3') }, 'handset', { release: describeRelease('TRITON-5.8-65.3') }), /^this is the NEPTUN main controller 5\.8 image, but the handset 65\.3 file provided is TRITON\./);
});

/** The parts of the Engine interface the Runtime uses, with firmware identified by a tag in the file ("main:TRITON-5.8-65.3"). */
class FakeEngine {
  constructor({ rejectOptions = [] } = {}) {
    this.name = 'fake-engine/0';
    this.unsupportedOptions = new Set();
    this.rejectOptions = rejectOptions;
    this.firmware = {};
    this.customImages = {}; // images given with setCustomFirmware (DESIGN 20.1), apart from the original ones
    this.supportsCustom = true;
    this.created = [];
    this.seeds = [];
    this.active = false;
    this.clock = 0;
  }

  /**
   * The structural report of DESIGN 20.1 for a custom build. A file is "S0custom:<tag>": `bad` fails the initial-SP check, a
   * file that does not start with S0 is not an S-record.
   */
  inspectCustomFirmware(bytes) {
    if (!this.supportsCustom) throw new EngineError('This engine build has no support for custom firmware builds.');
    const body = new TextDecoder().decode(bytes);
    const base = {
      custom: true, role: null, release: { id: 'CUSTOM', label: 'Custom build' }, srecBytes: bytes.length, header: 'custom', recordCounts: { S0: 1 },
      segments: [{ start: 0x08004000, end: 0x08005000 }], entry: null, error: null,
    };
    if (!body.startsWith('S0')) {
      return { ...base, ok: false, srecSha256: 'cd'.repeat(32), binSha256: null, span: null, checks: [{ name: 'srec-syntax', ok: false, detail: 'record 1: not an S-record' }], error: 'record 1: not an S-record', message: 'srec-syntax: record 1: not an S-record' };
    }
    const names = ['srec-syntax', 'address-bounds', 'span', 'vector-table', 'initial-sp', 'reset-vector', 'entry-point'];
    const bad = body.endsWith(':bad');
    return {
      ...base,
      ok: !bad, srecSha256: `sha-${body}`, binSha256: `bin-${body}`, span: { start: 0x08004000, end: 0x08005000 },
      initialSp: bad ? 0x20019000 : 0x20018000, resetPc: 0x08004411, entry: 0x08004411,
      checks: names.map((name) => ({ name, ok: !(bad && name === 'initial-sp'), detail: bad && name === 'initial-sp' ? '0x20019000 is above 0x20018000' : 'ok' })),
      message: bad ? 'initial-sp: 0x20019000 is above 0x20018000' : null,
    };
  }

  setCustomFirmware(role, bytes) { this.customImages[role] = new TextDecoder().decode(bytes); }

  inspectFirmware(bytes) {
    const [role, release] = new TextDecoder().decode(bytes).split(':');
    if (!['main', 'handset'].includes(role) || !release) return { ok: false, role: null, message: 'This is not a firmware image.', checks: [], srecSha256: '' };
    const report = { ok: true, role, srecSha256: `sha-${role}-${release}`, checks: [{ ok: true, name: 'check', detail: 'ok' }], message: null };
    if (release !== 'legacy') report.release = { id: release, label: `${release} label` };
    return report;
  }

  setFirmware(role, bytes) { this.firmware[role] = new TextDecoder().decode(bytes); }
  clearFirmware(role) { delete this.firmware[role]; delete this.customImages[role]; }

  createSession(config, profile) {
    this.unsupportedOptions = new Set();
    const options = { ...config };
    for (const name of this.rejectOptions) {
      if (name in options) {
        delete options[name];
        this.unsupportedOptions.add(name);
      }
    }
    this.created.push({ config: options, requested: { ...config }, profile: { ...profile }, firmware: { ...this.firmware }, custom: { ...this.customImages } });
    this.active = true;
  }

  setClock() {}
  setSeed(lo, hi) { this.seeds.push([lo, hi]); }
  setHostInfo() {}
  running() { return false; }
  time() { return 0; }
  state() { return { running: false, virtualTime: 0, inputs: { ...initialInputs }, hardwareOutputs: [], uartConsole: [], engine: this.name }; }
  stateText() { return JSON.stringify(this.state()); }
  frame() { return { version: 0, width: 0, height: 0, ptr: 0, length: 0 }; }
  copyFrame() {}
  shutdown() { return this.exportProfile(); }
  exportProfile() { return [{ name: 'eeprom.bin', data: new Uint8Array([1, 2, 3]) }, { name: 'inputs.json', data: new TextEncoder().encode('{}') }]; }
  profileChanges() { return []; }
  action(request) { return { ...this.state(), request }; }
  panicMessage() { return ''; }
}

class RuntimeHarness {
  constructor(engine, storage = new MemoryStorage()) {
    this.engine = engine;
    this.storage = storage;
    this.messages = [];
    this.pending = new Map();
    this.nextId = 1;
    this.runtime = new Runtime({
      post: (message) => {
        this.messages.push(message);
        if (message.type === 'response' && this.pending.has(message.id)) {
          const { resolve, reject } = this.pending.get(message.id);
          this.pending.delete(message.id);
          if (message.ok) resolve(message.result);
          else reject(Object.assign(new Error(message.error.message), message.error));
        }
      },
      loadEngine: async () => engine,
      openStorage: async () => ({ storage: this.storage, problems: [] }),
      randomWords: (() => { let counter = 0; return () => new Uint32Array([0xffffffff, ++counter]); })(),
    });
  }

  request(type, payload = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.runtime.receive({ id, type, ...payload });
    });
  }

  inspect(role, release, name = `${role}-${release}.srec`) {
    return this.request('inspect', { name, bytes: new TextEncoder().encode(`${role}:${release}`) });
  }
}

test('runtime (fake engine): both releases are accepted in either order and a mixed pair is rejected with a clear message', async () => {
  const h = new RuntimeHarness(new FakeEngine());
  await h.request('init');
  const handset = await h.inspect('handset', 'NEPTUN-5.8-65.3', 'ngc_handset_65.3_NEPTUN.srec');
  assert.equal(handset.accepted, true);
  assert.equal(handset.release.name, 'NEPTUN');
  const mixed = await h.inspect('main', 'TRITON-5.8-65.3', 'ngc_main_5.8_TRITON.srec');
  assert.equal(mixed.accepted, false);
  assert.equal(mixed.conflict, true);
  assert.match(mixed.message, /this is the TRITON main controller 5\.8 image, but the handset 65\.3 file provided is NEPTUN/);
  assert.match(mixed.message, /same release/);
  assert.equal(mixed.report.srecSha256, 'sha-main-TRITON-5.8-65.3', 'the file is still identified in the refusal');
  // The matching main file is accepted; replacing the handset by the other release is refused while main stays.
  const main = await h.inspect('main', 'NEPTUN-5.8-65.3');
  assert.equal(main.accepted, true);
  assert.equal((await h.inspect('handset', 'TRITON-5.8-65.3')).accepted, false);
  // Removing a file lets the other release in.
  await h.request('clear-firmware', { role: 'main' });
  await h.request('clear-firmware', { role: 'handset' });
  assert.equal((await h.inspect('main', 'TRITON-5.8-65.3')).accepted, true);
  assert.equal((await h.inspect('handset', 'TRITON-5.8-65.3')).accepted, true);
  // An unrecognized file and an engine report without a release (older engine) behave as before.
  const bad = await h.request('inspect', { name: 'x.txt', bytes: new TextEncoder().encode('hello') });
  assert.equal(bad.accepted, false);
  assert.equal(bad.release, null);
  const legacy = await h.inspect('handset', 'legacy');
  assert.equal(legacy.release.id, 'TRITON-5.8-65.3');
});

test('runtime (fake engine): the session options, the history nonce and the release of the boot', async () => {
  const engine = new FakeEngine();
  const h = new RuntimeHarness(engine);
  await h.request('init');
  await h.inspect('main', 'NEPTUN-5.8-65.3');
  await h.inspect('handset', 'NEPTUN-5.8-65.3');
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(booted.release.id, 'NEPTUN-5.8-65.3');
  assert.equal(booted.hostStatus.release.name, 'NEPTUN');
  const first = engine.created[0];
  assert.equal(first.config.i2cIdleHigh, true, 'the I2C idle-high fixture is on by default');
  assert.equal(first.config.historyNonce, nonceFromWords([0xffffffff, 1]));
  assert.ok(Number.isSafeInteger(first.config.historyNonce) && first.config.historyNonce > 0);
  assert.equal(first.config.mode, 'dual');
  assert.deepEqual(first.firmware, { main: 'main:NEPTUN-5.8-65.3', handset: 'handset:NEPTUN-5.8-65.3' });
  await h.request('close-session');
  // The fixture is a start option; every session gets a new nonce.
  await h.request('boot', { options: { mode: 'dual', startPaused: true, i2cIdleHigh: false } });
  const second = engine.created[1];
  assert.equal(second.config.i2cIdleHigh, false);
  assert.notEqual(second.config.historyNonce, first.config.historyNonce);
  await h.request('import-profile', { files: [{ name: 'eeprom.bin', data: new Uint8Array([9]) }] });
  assert.equal(engine.created.length, 3);
  assert.notEqual(engine.created[2].config.historyNonce, second.config.historyNonce, 'a recreated session gets a fresh nonce');
  assert.equal(engine.created[2].config.i2cIdleHigh, false, 'the option survives a profile import');
  assert.throws(() => Runtime.normalizeConfig({ adcSample: 5000 }), /ADC sample/);
  await h.request('close-session');
});

test('runtime (fake engine): a mixed pair that got past the page is refused at boot', async () => {
  const h = new RuntimeHarness(new FakeEngine());
  await h.request('init');
  await h.inspect('main', 'TRITON-5.8-65.3');
  await h.inspect('handset', 'TRITON-5.8-65.3');
  h.runtime.firmware.handset.release = describeRelease('NEPTUN-5.8-65.3'); // as if the slot had been filled behind our back
  await assert.rejects(() => h.request('boot', { options: { mode: 'dual', startPaused: true } }), /different releases/);
  // A handset-only session only needs the handset image.
  const booted = await h.request('boot', { options: { mode: 'handset', startPaused: true } });
  assert.equal(booted.release.id, 'NEPTUN-5.8-65.3');
  await h.request('close-session');
});

test('runtime (fake engine): each release has its own profile; TRITON keeps the original location', async () => {
  const storage = new MemoryStorage();
  const engine = new FakeEngine();
  const h = new RuntimeHarness(engine, storage);
  await h.request('init');
  for (const release of ['TRITON-5.8-65.3', 'NEPTUN-5.8-65.3']) {
    await h.request('clear-firmware', { role: 'main' });
    await h.request('clear-firmware', { role: 'handset' });
    await h.inspect('main', release);
    await h.inspect('handset', release);
    await h.request('boot', { options: { mode: 'dual', startPaused: true } });
    await h.request('flush');
    await h.request('close-session');
  }
  assert.deepEqual((await storage.list('profile')).map((file) => file.name).sort(), ['eeprom.bin', 'inputs.json']);
  assert.deepEqual((await storage.list('profile-neptun-5_8-65_3')).map((file) => file.name).sort(), ['eeprom.bin', 'inputs.json']);
  // Writing one release's profile never touches the other: put a marker into the NEPTUN profile and boot TRITON.
  await storage.write('profile-neptun-5_8-65_3', 'led-colors.json', new TextEncoder().encode('{"marker":1}'));
  await h.request('clear-firmware', { role: 'main' });
  await h.request('clear-firmware', { role: 'handset' });
  await h.inspect('main', 'TRITON-5.8-65.3');
  await h.inspect('handset', 'TRITON-5.8-65.3');
  await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(engine.created.at(-1).profile['led-colors.json'], undefined, 'the NEPTUN profile is not offered to TRITON firmware');
  const info = await h.request('info');
  assert.ok(info.profiles['TRITON-5.8-65.3'] && info.profiles['NEPTUN-5.8-65.3']);
  assert.ok(info.profiles['NEPTUN-5.8-65.3'].files.some((file) => file.name === 'led-colors.json'));
  // Resetting a profile erases that release only (with or without a running session).
  await h.request('reset-profile');
  assert.equal((await storage.list('profile')).length, 0);
  assert.ok((await storage.list('profile-neptun-5_8-65_3')).length > 0);
  await h.request('close-session');
  await h.request('reset-profile', { release: 'NEPTUN-5.8-65.3' });
  assert.equal((await storage.list('profile-neptun-5_8-65_3')).length, 0);
});

test('runtime (fake engine): the remembered firmware pair carries its release and is verified again', async () => {
  const storage = new MemoryStorage();
  storage.kind = 'opfs';
  const h = new RuntimeHarness(new FakeEngine(), storage);
  await h.request('init');
  await h.inspect('handset', 'NEPTUN-5.8-65.3', 'ngc_handset_65.3_NEPTUN.srec');
  await h.inspect('main', 'NEPTUN-5.8-65.3', 'ngc_main_5.8_NEPTUN.srec');
  await h.request('boot', { options: { mode: 'dual', startPaused: true }, remember: true });
  await h.request('close-session');
  const info = await h.request('info');
  assert.equal(info.remembered.main.release.name, 'NEPTUN');
  assert.equal(info.remembered.handset.name, 'ngc_handset_65.3_NEPTUN.srec');
  // A fresh runtime (a later visit) takes the remembered pair.
  const later = new RuntimeHarness(new FakeEngine(), storage);
  await later.request('init');
  const used = await later.request('use-remembered');
  assert.deepEqual(used.accepted.map((file) => file.role).sort(), ['handset', 'main']);
  assert.equal(used.accepted[0].release.id, 'NEPTUN-5.8-65.3');
  assert.deepEqual(used.problems, []);
  // It replaces held files of the same roles; a held file of another release for a role that is not remembered blocks it.
  await later.request('clear-firmware', { role: 'main' });
  await later.request('clear-firmware', { role: 'handset' });
  await later.inspect('main', 'TRITON-5.8-65.3');
  const replaced = await later.request('use-remembered');
  assert.equal(replaced.accepted.length, 2, 'the remembered pair replaces the held main file');
  assert.equal(later.runtime.firmware.main.release.name, 'NEPTUN');
  // Remembering a TRITON pair afterwards replaces the NEPTUN pair (one remembered pair).
  await later.request('clear-firmware', { role: 'main' });
  await later.request('clear-firmware', { role: 'handset' });
  await later.inspect('main', 'TRITON-5.8-65.3');
  await later.inspect('handset', 'TRITON-5.8-65.3');
  await later.request('boot', { options: { mode: 'dual', startPaused: true }, remember: true });
  await later.request('close-session');
  assert.equal((await later.request('info')).remembered.main.release.name, 'TRITON');
  // An index written before releases existed means TRITON.
  await storage.write('firmware', 'index.json', new TextEncoder().encode(JSON.stringify({ main: { name: 'old.srec', size: 1 }, handset: { name: 'old2.srec', size: 1 } })));
  assert.equal((await later.request('info')).remembered.handset.release.id, 'TRITON-5.8-65.3');
});

test('engine: options an older engine build rejects are dropped one at a time and reported', () => {
  const memory = new ArrayBuffer(1 << 16);
  const bytes = new Uint8Array(memory);
  let top = 4096;
  let output = new Uint8Array(0);
  let error = '';
  const staged = [];
  const attempts = [];
  const known = new Set(['mode', 'bootMode', 'simultaneousStart', 'idleFastForward', 'adcSample', 'startPaused']);
  const encoder = new TextEncoder();
  const exports = {
    memory: { buffer: memory },
    ngc_init() {},
    ngc_engine() { output = encoder.encode('fake/1'); bytes.set(output, 100); return output.length; },
    ngc_version() { return 1; },
    ngc_output_ptr() { return 100; },
    ngc_alloc(length) { const at = top; top += length + 8; return at; },
    ngc_free() {},
    ngc_profile_clear() { staged.length = 0; },
    ngc_profile_set(kind, ptr, length) { staged.push([kind, [...bytes.subarray(ptr, ptr + length)]]); return 0; },
    ngc_session_create(ptr, length) {
      const config = JSON.parse(new TextDecoder().decode(bytes.subarray(ptr, ptr + length)));
      attempts.push({ keys: Object.keys(config).sort(), staged: staged.length });
      staged.length = 0; // consumed by the attempt, successful or not (as in the real engine)
      for (const key of Object.keys(config)) {
        if (!known.has(key)) { error = `unknown session option: ${key}`; return 1; }
      }
      return 0;
    },
    ngc_error() { output = encoder.encode(error); bytes.set(output, 100); return output.length; },
  };
  const engine = new Engine({ exports }, null);
  engine.createSession({ mode: 'dual', historyNonce: 5, i2cIdleHigh: true }, { 'eeprom.bin': new Uint8Array([1, 2]) });
  assert.deepEqual(attempts.map((attempt) => attempt.staged), [1, 1, 1], 'the profile is staged again for every attempt');
  assert.deepEqual(attempts.at(-1).keys, ['mode']);
  assert.deepEqual([...engine.unsupportedOptions].sort(), ['historyNonce', 'i2cIdleHigh']);
  // An option that is not one of the optional ones is a real error, with the engine's message.
  known.delete('adcSample');
  assert.throws(() => engine.createSession({ adcSample: 400 }), (failure) => failure instanceof EngineError && /unknown session option: adcSample/.test(failure.message));
  // A newer engine takes everything.
  known.add('adcSample').add('historyNonce').add('i2cIdleHigh');
  engine.createSession({ mode: 'dual', historyNonce: 5, i2cIdleHigh: true });
  assert.equal(engine.unsupportedOptions.size, 0);
});

// =====================================================================================================
// Load from URLs (firmware-url.js and the entry screen)
// =====================================================================================================

const MAIN_EXAMPLE = 'https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/pvlL3Iilv4o_Tu5lggngZAUt.srec';
const HANDSET_EXAMPLE = 'https://web.archive.org/web/20261008041427/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec';
const MAIN_FETCHED = 'https://web.archive.org/web/20261008041333id_/https://api.multi3s.com/static/pvlL3Iilv4o_Tu5lggngZAUt.srec';
const HANDSET_FETCHED = 'https://web.archive.org/web/20261008041427id_/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec';

test('urls: the page-side allowlist accepts the two forms and normalizes the Wayback form to the raw id_ form', () => {
  assert.deepEqual(normalizeFirmwareUrl(MAIN_EXAMPLE), {
    ok: true, kind: 'archive', name: 'pvlL3Iilv4o_Tu5lggngZAUt', timestamp: '20261008041333',
    file: 'pvlL3Iilv4o_Tu5lggngZAUt.srec', url: MAIN_FETCHED, changed: true,
  });
  assert.equal(normalizeFirmwareUrl(HANDSET_EXAMPLE).url, HANDSET_FETCHED);
  assert.equal(normalizeFirmwareUrl(MAIN_FETCHED).changed, false, 'the raw form is already normalized');
  assert.equal(normalizeFirmwareUrl(MAIN_FETCHED).url, MAIN_FETCHED);
  const direct = normalizeFirmwareUrl('https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec');
  assert.deepEqual([direct.ok, direct.kind, direct.name, direct.changed], [true, 'origin', 'rlVpEk1qk8-0r1E4vMHNAjQG', false]);
  // White space around the address is ignored; case differences in the scheme and hosts are normalized, the rest
  // (path, name, extension, "id_") is case-sensitive upstream and never changed.
  assert.equal(normalizeFirmwareUrl(`  ${MAIN_EXAMPLE}\n`).url, MAIN_FETCHED);
  const shouting = normalizeFirmwareUrl('HTTPS://API.MULTI3S.COM/static/AbC_-1.srec');
  assert.deepEqual([shouting.ok, shouting.url, shouting.changed], [true, 'https://api.multi3s.com/static/AbC_-1.srec', true]);
  assert.equal(normalizeFirmwareUrl('https://WEB.ARCHIVE.ORG/web/20261008041333/HTTPS://API.MULTI3S.COM/static/Name.srec').url, 'https://web.archive.org/web/20261008041333id_/https://api.multi3s.com/static/Name.srec');
  assert.equal(normalizeFirmwareUrl(`https://api.multi3s.com/static/${'a'.repeat(64)}.srec`).ok, true, 'a 64-character name is the longest');
});

test('urls: everything else is refused, each with its reason', () => {
  const refused = {
    '': 'empty',
    '   ': 'empty',
    'api.multi3s.com/static/x.srec': 'not-url',
    'ftp://api.multi3s.com/static/x.srec': 'scheme',
    'http://api.multi3s.com/static/x.srec': 'scheme',
    'http://web.archive.org/web/20261008041333id_/https://api.multi3s.com/static/x.srec': 'scheme',
    'https://user:pw@api.multi3s.com/static/x.srec': 'userinfo',
    'https://api.multi3s.com@evil.example/static/x.srec': 'userinfo',
    'https://api.multi3s.com:8443/static/x.srec': 'port',
    'https://api.multi3s.com:/static/x.srec': 'port',
    'https://evil.example/static/x.srec': 'host',
    'https://api.multi3s.com.evil.example/static/x.srec': 'host',
    'https://evilapi.multi3s.com/static/x.srec': 'host',
    'https://127.0.0.1/static/x.srec': 'host',
    'https://[::1]/static/x.srec': 'host',
    'https://2130706433/static/x.srec': 'host',
    'https://localhost/static/x.srec': 'host',
    'https://api.multi3s.com/static/x.srec?x=1': 'query',
    'https://api.multi3s.com/static/x.srec?': 'query',
    'https://api.multi3s.com/static/x.srec#frag': 'fragment',
    'https://api.multi3s.com/static/%2e%2e/x.srec': 'encoded',
    'https://api.multi3s.com/static/x%2esrec': 'encoded',
    'https://api.multi3s.com/static/x.srec%00': 'encoded',
    'https://api.multi3s.com/static/../x.srec': 'path',
    'https://api.multi3s.com/static/./x.srec': 'path',
    'https://api.multi3s.com//static/x.srec': 'path',
    'https://api.multi3s.com/other/x.srec': 'path',
    'https://api.multi3s.com/static/x.bin': 'path',
    'https://api.multi3s.com/static/x.SREC': 'path',
    'https://api.multi3s.com/STATIC/x.srec': 'path',
    'https://web.archive.org/web/20261008041333ID_/https://api.multi3s.com/static/x.srec': 'archive-form',
    'https://api.multi3s.com/static/x.srec/': 'path',
    'https://api.multi3s.com/static/a/b.srec': 'path',
    'https://api.multi3s.com/static/.srec': 'path',
    'https://api.multi3s.com/static/x.srec.srec': 'path',
    'https://api.multi3s.com/': 'path',
    'https://api.multi3s.com': 'path',
    [`https://api.multi3s.com/static/${'a'.repeat(65)}.srec`]: 'path',
    'https://api.multi3s.com\\static\\x.srec': 'characters',
    'https://api.multi3s.com/static/x y.srec': 'characters',
    'https://api.multi3s.com/static/x.srec junk': 'characters',
    'https://api.multi3s.com/static/é.srec': 'characters',
    'https://web.archive.org/web/2026/https://api.multi3s.com/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333im_/https://api.multi3s.com/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333id_/https://evil.example/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333/http://api.multi3s.com/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333/https:/api.multi3s.com/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/x.srec?a=b': 'query',
    'https://web.archive.org/save/https://api.multi3s.com/static/x.srec': 'archive-form',
    'https://web.archive.org/web/20261008041333/https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/x.srec': 'archive-form',
    [`https://${'a'.repeat(300)}.example/x.srec`]: 'too-long',
  };
  for (const [input, reason] of Object.entries(refused)) {
    const result = normalizeFirmwareUrl(input);
    assert.equal(result.ok, false, `${input.slice(0, 90)} must be refused`);
    assert.equal(result.reason, reason, `${input.slice(0, 90)} is refused as ${reason}`);
    assert.ok(result.message && result.message.length > 10);
  }
  assert.equal(normalizeFirmwareUrl(undefined).reason, 'empty');
});

const PROXY = 'https://firmware-proxy.example/api/firmware';

test('urls: the committed config.js configures no proxy; the proxy is the configured one, a loopback dev proxy or none (never a same-origin /api/firmware)', () => {
  assert.equal(FIRMWARE_PROXY_URL, null, 'the committed default configures nothing; the Pages build generates its own config.js');
  const at = (hostname, search = '', configured = PROXY) => proxyEndpoint({ protocol: 'https:', hostname }, search, configured);
  // The deployed site and any other page use the configured proxy, wherever they are served.
  assert.deepEqual(at('triton.divehub.ai'), { endpoint: PROXY, kind: 'configured', label: PROXY });
  assert.equal(at('divehub.github.io').endpoint, PROXY);
  assert.equal(at('127.0.0.1').endpoint, PROXY, 'a local page uses the configured proxy too');
  // Nothing configured: no proxy at all, on the deployed domain and on loopback, with or without an empty value.
  for (const hostname of ['triton.divehub.ai', 'divehub.github.io', '127.0.0.1', 'localhost', '']) {
    assert.deepEqual(at(hostname, '', null), { endpoint: null, kind: 'none', label: 'not configured' }, `${hostname} without configuration`);
    assert.deepEqual(proxyEndpoint({ hostname }, ''), { endpoint: null, kind: 'none', label: 'not configured' }, `${hostname} with the default argument`);
  }
  assert.deepEqual(proxyEndpoint({ protocol: 'file:', hostname: '' }), { endpoint: null, kind: 'none', label: 'not configured' });
  // The configured value is only accepted in its canonical form.
  assert.equal(configuredProxyUrl(PROXY), PROXY);
  assert.equal(configuredProxyUrl('https://ngc-proxy.vercel.app:8443/api/firmware'), 'https://ngc-proxy.vercel.app:8443/api/firmware');
  for (const bad of [
    '', 'firmware', '/api/firmware', 'http://firmware-proxy.example/api/firmware', 'https://firmware-proxy.example', 'https://firmware-proxy.example/',
    'https://firmware-proxy.example/api/firmware/', 'https://firmware-proxy.example/api/other', 'https://firmware-proxy.example/api/firmware?x=1',
    'https://firmware-proxy.example/api/firmware#x', 'https://user@firmware-proxy.example/api/firmware', 'https://FIRMWARE-PROXY.example/api/firmware',
    'https://firmware-proxy.example:443/api/firmware', 'https://firmware-proxy.example/api/firmware ', 'https://firmware-proxy.example/%61pi/firmware',
    'javascript:alert(1)', 'data:text/plain,x', `https://${'a'.repeat(300)}.example/api/firmware`, 42, {}, true,
  ]) {
    assert.equal(configuredProxyUrl(bad), null, `${String(bad).slice(0, 60)} is not an accepted proxy address`);
  }
  const invalid = at('triton.divehub.ai', '', 'https://firmware-proxy.example/');
  assert.deepEqual([invalid.endpoint, invalid.kind], [null, 'none']);
  assert.match(invalid.warning, /not valid/);
  // The dev proxy parameter: loopback pages only, loopback addresses only, and it wins over the configured proxy.
  const dev = at('127.0.0.1', '?firmware-proxy=http://127.0.0.1:8775/api/firmware');
  assert.deepEqual([dev.endpoint, dev.kind, dev.warning], ['http://127.0.0.1:8775/api/firmware', 'dev', undefined]);
  assert.equal(at('localhost', '?firmware-proxy=http%3A%2F%2Flocalhost%3A9%2Fapi%2Ffirmware', null).endpoint, 'http://localhost:9/api/firmware');
  assert.equal(at('127.0.0.1', '?x=1&firmware-proxy=http://127.0.0.1/api/firmware').endpoint, 'http://127.0.0.1/api/firmware', 'the default port is fine');
  for (const bad of [
    'https://127.0.0.1:8775/api/firmware', 'http://192.168.1.5:8775/api/firmware', 'http://127.0.0.1.evil.example/api/firmware',
    'http://evil.example/api/firmware', 'http://user@127.0.0.1:8775/api/firmware', 'http://127.0.0.1:8775/api/other',
    'http://127.0.0.1:8775/api/firmware?x=1', 'http://127.0.0.1:8775/api/firmware#x', 'http://127.0.0.1:8775/', 'firmware', '',
    'https://triton.divehub.ai/api/firmware',
  ]) {
    const result = at('127.0.0.1', `?firmware-proxy=${encodeURIComponent(bad)}`, null);
    assert.equal(result.kind, 'none', `${bad} is ignored`);
    assert.match(result.warning, /ignored/);
    assert.equal(at('127.0.0.1', `?firmware-proxy=${encodeURIComponent(bad)}`).endpoint, PROXY, `${bad} is ignored in favor of the configured proxy`);
  }
  // On any other page the parameter is ignored altogether, even with a loopback address.
  const hosted = at('triton.divehub.ai', '?firmware-proxy=http://127.0.0.1:8775/api/firmware');
  assert.deepEqual([hosted.kind, hosted.endpoint], ['configured', PROXY]);
  assert.match(hosted.warning, /only used when this page is served from localhost/);
  assert.equal(at('triton.divehub.ai', '?firmware-proxy=http://127.0.0.1:8775/api/firmware', null).kind, 'none');
  assert.equal(loopbackProxyUrl('http://127.0.0.1:8775/api/firmware'), 'http://127.0.0.1:8775/api/firmware');
  assert.equal(loopbackProxyUrl('http://[::1]:8775/api/firmware'), null);
});

// ---- the fetch and its errors ---------------------------------------------------------------------------

const text = (value) => new TextEncoder().encode(value);
const proxyBody = (value, headers = {}) => new Response(value, { status: 200, headers: { 'content-type': 'text/plain; charset=us-ascii', ...headers } });
const proxyJson = (status, body) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json; charset=utf-8' } });
const streamOf = (...chunks) => new ReadableStream({
  start(controller) {
    for (const chunk of chunks) controller.enqueue(chunk);
    controller.close();
  },
});

async function fetchFails(code, respond, options = {}) {
  const calls = [];
  const fetchImpl = async (url, init) => { calls.push({ url, init }); return respond(url, init); };
  const failure = await fetchFirmware({ endpoint: PROXY, url: MAIN_FETCHED, label: 'the test proxy', fetchImpl, ...options }).then(
    () => assert.fail(`expected ${code}`),
    (error) => error,
  );
  assert.ok(failure instanceof FirmwareFetchError, String(failure));
  assert.equal(failure.code, code, failure.message);
  return { failure, calls };
}

test('urls (fetch): the request carries only the encoded address, without credentials or referrer; a success reports progress and the final source', async () => {
  const calls = [];
  const progress = [];
  const result = await fetchFirmware({
    endpoint: 'http://127.0.0.1:8775/api/firmware', url: MAIN_FETCHED,
    fetchImpl: async (url, init) => {
      calls.push({ url, init });
      return new Response(streamOf(text('S00600004844521B\r\n'), text('S9030000FC\r\n')), { status: 200, headers: { 'content-length': '30', 'x-firmware-source': HANDSET_FETCHED } });
    },
    onProgress: (loaded, total) => progress.push([loaded, total]),
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, `http://127.0.0.1:8775/api/firmware?url=${encodeURIComponent(MAIN_FETCHED)}`);
  assert.deepEqual(plain({ ...calls[0].init, signal: undefined }), { method: 'GET', credentials: 'omit', cache: 'no-store', referrerPolicy: 'no-referrer' });
  assert.equal(new TextDecoder().decode(result.bytes), 'S00600004844521B\r\nS9030000FC\r\n');
  assert.equal(result.source, HANDSET_FETCHED, 'the address the proxy finally fetched is reported');
  assert.deepEqual(progress, [[18, 30], [30, 30]]);
});

test('urls (fetch): every failure is classified with a message that says what to do', async () => {
  // The proxy cannot be reached at all: network, CORS, CSP, proxy not running.
  const network = await fetchFails('network', () => { throw new TypeError('Failed to fetch'); });
  assert.match(network.failure.message, /Could not reach the firmware proxy \(the test proxy\).*dev-proxy\.mjs.*firmware-proxy=/);
  // Something answers, but is not the proxy (the page served without its function, a wrong dev proxy port).
  const missing = await fetchFails('proxy-missing', () => new Response('<html>Not found</html>', { status: 404, headers: { 'content-type': 'text/html' } }));
  assert.match(missing.failure.message, /No firmware proxy answered at the test proxy \(HTTP 404\)/);
  assert.equal((await fetchFails('proxy-error', () => new Response('oops', { status: 500 }))).failure.status, 500);
  // The proxy's own refusals.
  const forbidden = await fetchFails('proxy-forbidden', () => proxyJson(403, { error: 'origin_not_allowed', message: 'This page origin may not use the firmware proxy.' }));
  assert.match(forbidden.failure.message, /does not serve this page \(HTTP 403/);
  const refused = await fetchFails('not-allowed', () => proxyJson(400, { error: 'upstream_not_allowed', message: 'Only https:// addresses are accepted.', reason: 'scheme' }));
  assert.match(refused.failure.message, /The proxy refused this address: Only https/);
  // The source's answers.
  const status = await fetchFails('http', () => proxyJson(502, { error: 'upstream_status', message: 'The upstream answered HTTP 404.', upstreamStatus: 404 }));
  assert.equal(status.failure.message, 'The source answered HTTP 404.');
  assert.equal(status.failure.upstreamStatus, 404);
  assert.match((await fetchFails('too-large', () => proxyJson(502, { error: 'upstream_too_large', message: 'x' }))).failure.message, /larger than 4 MiB/);
  assert.match((await fetchFails('not-srec', () => proxyJson(502, { error: 'not_srec', message: 'x' }))).failure.message, /does not start with S0/);
  assert.match((await fetchFails('timeout', () => proxyJson(504, { error: 'upstream_timeout', message: 'The upstream did not answer within 20 seconds.' }))).failure.message, /did not answer in time\. The upstream did not answer within 20 seconds/);
  assert.equal((await fetchFails('proxy-error', () => proxyJson(502, { error: 'redirect_not_allowed', message: 'The upstream redirected somewhere that is not an allowed address.' }))).failure.message.startsWith('The firmware proxy reported: The upstream redirected'), true);
  // The page checks what arrives, too: size by Content-Length and while reading, then the S0 start.
  const declared = await fetchFails('too-large', () => proxyBody('S0', { 'content-length': String(MAX_FETCH_BYTES + 1) }));
  assert.match(declared.failure.message, new RegExp(`${MAX_FETCH_BYTES + 1} bytes`));
  const streamed = await fetchFails('too-large', () => new Response(streamOf(new Uint8Array(MAX_FETCH_BYTES - 8).fill(0x53), new Uint8Array(16).fill(0x53)), { status: 200 }));
  assert.match(streamed.failure.message, /larger than 4 MiB/);
  await fetchFails('not-srec', () => proxyBody('<html><body>hi</body></html>'));
  await fetchFails('not-srec', () => proxyBody(''));
  await fetchFails('not-srec', () => proxyBody('S'));
  assert.equal((await fetchFails('not-srec', () => proxyBody('s0aa'))).failure.code, 'not-srec', 'the S must be upper case');
  // A body that breaks half-way is a network error; a stopped request is "aborted".
  const broken = await fetchFails('network', () => new Response(new ReadableStream({
    pull(controller) { controller.error(new Error('reset')); },
  }), { status: 200 }));
  assert.match(broken.failure.message, /broke while the file was being received/);
  const controller = new AbortController();
  const aborted = await fetchFails('aborted', (url, init) => new Promise((resolve, reject) => {
    init.signal.addEventListener('abort', () => reject(new DOMException('stopped', 'AbortError')));
    controller.abort();
  }), { signal: controller.signal });
  assert.match(aborted.failure.message, /stopped/);
});

// ---- the entry screen ---------------------------------------------------------------------------------

/** A fake engine whose files are "S0" + "<role>:<release>", so the proxy-side check and the engine's both apply. */
class SrecFakeEngine extends FakeEngine {
  inspectFirmware(bytes) {
    const body = new TextDecoder().decode(bytes);
    if (body === 'S0unknown') return { ok: false, role: null, message: 'This S-record file is not a known firmware image.', checks: [], srecSha256: 'ab'.repeat(32) };
    return super.inspectFirmware(bytes.subarray(2));
  }

  setFirmware(role, bytes) { super.setFirmware(role, bytes.subarray(2)); }
}

const srec = (role, release) => `S0${role}:${release}`;

/**
 * The real EntryView on the real index.html (fake DOM), a worker client that is the real Runtime on a fake engine
 * (so the bytes take exactly the path of a chosen file) and a fake fetch / fake timers.
 */
async function mountEntry({ search = '', location, respond = () => assert.fail('fetch must not be called'), engine = new SrecFakeEngine(), timeoutMs, proxyUrl = PROXY, storageKind = 'memory', booted = () => {} } = {}) {
  installDom(html, { search, location });
  const { EntryView } = await import('./entry.js');
  // `opfs` stands for a browser with the origin-private file system (the only place firmware can be remembered).
  const storage = new MemoryStorage();
  storage.kind = storageKind;
  const runtime = new RuntimeHarness(engine, storage);
  await runtime.request('init');
  const clock = fakeTimers();
  const calls = [];
  // postMessage semantics: transferred buffers arrive in the worker and are detached (empty) on the page afterwards.
  const client = {
    request: (type, payload = {}, transfer = []) => runtime.request(type, transfer.length ? structuredClone(payload, { transfer }) : payload),
  };
  const view = new EntryView(client, { booted }, {
    fetch: async (url, init) => { calls.push({ url: String(url), init }); return respond(String(url), init); },
    timers: clock,
    timeoutMs,
    proxyUrl,
  });
  view.show({ engine: 'fake', storage: { kind: storageKind, problems: [] }, remembered: null, profiles: {} });
  const { document } = globalThis;
  return {
    view, runtime, clock, calls, document, engine,
    el: (id) => document.getElementById(id),
    type: (role, value) => { const input = document.getElementById(`url-${role}`); input.value = value; input.dispatch('input'); },
    info: (role) => document.getElementById(`url-${role}-info`).textContent,
    slotText: (role) => document.getElementById(`slot-${role}`).querySelector('[data-part="state"]').textContent,
    rejectedText: () => document.getElementById('rejected').textContent,
  };
}

/** A proxy that answers by the address asked for: {[normalized upstream URL]: Response factory}. */
const proxyFor = (answers) => (url) => {
  const upstream = new URL(url, 'http://page.invalid').searchParams.get('url');
  const answer = answers[upstream];
  return answer ? answer() : proxyJson(400, { error: 'upstream_not_allowed', message: 'not in the test table' });
};

test('urls (page): a query parameter pre-fills the fields and never fetches; the fields show the exact address that will be requested', async () => {
  const m = await mountEntry({ search: `?main-url=${encodeURIComponent(MAIN_EXAMPLE)}&handset-url=${encodeURIComponent(HANDSET_EXAMPLE)}` });
  assert.equal(m.el('url-main').value, MAIN_EXAMPLE);
  assert.equal(m.el('url-handset').value, HANDSET_EXAMPLE);
  assert.equal(m.info('main'), `Will be fetched as: ${MAIN_FETCHED}`);
  assert.equal(m.info('handset'), `Will be fetched as: ${HANDSET_FETCHED}`);
  assert.equal(m.el('url-load').disabled, false, 'Load is available, but nothing was started');
  await settle();
  assert.equal(m.calls.length, 0, 'a pre-filled address is never fetched automatically');
  assert.equal(m.view.slots.main, null);
  assert.equal(m.view.slots.handset, null);
  assert.equal(m.el('boot').disabled, true);
  // A bad pre-filled address is explained and Load stays disabled.
  const bad = await mountEntry({ search: '?main-url=http%3A%2F%2Fevil.example%2Fx.srec' });
  assert.match(bad.info('main'), /Only https/);
  assert.equal(bad.el('url-load').disabled, true);
  assert.equal(bad.el('url-main').value, 'http://evil.example/x.srec', 'the value is shown as text, never interpreted');
  // Without parameters the fields start with the archived TRITON addresses (nothing is fetched); emptied, nothing to load.
  const empty = await mountEntry();
  assert.equal(empty.el('url-main').value, MAIN_EXAMPLE);
  assert.equal(empty.el('url-handset').value, HANDSET_EXAMPLE);
  await settle();
  assert.equal(empty.calls.length, 0, 'the default addresses are never fetched automatically');
  empty.type('main', '');
  empty.type('handset', '');
  assert.equal(empty.el('url-load').disabled, true);
  assert.match(empty.el('url-proxy-note').textContent, /The firmware proxy at https:\/\/firmware-proxy\.example\/api\/firmware fetches the one file you name/);
});

test('urls (page): the proxy note names the proxy in use for each kind of page', async () => {
  const hosted = await mountEntry({ location: { protocol: 'https:', hostname: 'triton.divehub.ai', origin: 'https://triton.divehub.ai' } });
  assert.match(hosted.el('url-proxy-note').textContent, /^The firmware proxy at https:\/\/firmware-proxy\.example\/api\/firmware fetches the one file you name, on demand/);
  const dev = await mountEntry({ search: '?firmware-proxy=http://127.0.0.1:8775/api/firmware' });
  assert.match(dev.el('url-proxy-note').textContent, /The local development proxy at http:\/\/127\.0\.0\.1:8775\/api\/firmware/);
  const ignored = await mountEntry({ search: '?firmware-proxy=https://evil.example/api/firmware' });
  assert.match(ignored.el('url-proxy-note').textContent, /parameter was ignored/);
});

test('urls (page): without a configured firmware proxy "Load from URLs" is unavailable and says so, on the deployed domain and locally', async () => {
  for (const location of [{ protocol: 'https:', hostname: 'triton.divehub.ai', origin: 'https://triton.divehub.ai' }, undefined]) {
    const m = await mountEntry({ location, proxyUrl: null, search: `?main-url=${encodeURIComponent(MAIN_EXAMPLE)}` });
    assert.equal(m.view.proxy.kind, 'none');
    assert.equal(m.view.proxy.endpoint, null);
    assert.match(m.el('url-proxy-note').textContent, /^Loading from URLs is not configured yet: no firmware proxy address was set when this site was built\./);
    for (const id of ['url-intro', 'url-fields', 'url-actions']) assert.equal(m.el(id).hidden, true, `${id} is hidden`);
    assert.equal(m.el('url-load').disabled, true, 'even a pre-filled valid address cannot be loaded');
    await m.view.loadUrls(); // for example an Enter key in a hidden field
    await settle();
    assert.equal(m.calls.length, 0, 'nothing is fetched, in particular no same-origin /api/firmware request');
    assert.equal(m.el('url-status').textContent, '');
    // Choosing files still works as always.
    assert.equal(m.el('dropzone').hidden, false);
  }
  // With a proxy the same elements are shown.
  const configured = await mountEntry();
  for (const id of ['url-intro', 'url-fields', 'url-actions']) assert.equal(configured.el(id).hidden, false, `${id} is shown`);
  // A loopback dev proxy makes the feature available on a page without configuration.
  const dev = await mountEntry({ proxyUrl: null, search: '?firmware-proxy=http://127.0.0.1:8775/api/firmware' });
  assert.equal(dev.view.proxy.kind, 'dev');
  assert.equal(dev.el('url-fields').hidden, false);
});

test('urls (page): two fetched files take the same verification path as chosen files and fill both slots', async () => {
  const m = await mountEntry({
    respond: proxyFor({
      [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3'), { 'x-firmware-source': MAIN_FETCHED }),
      [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')),
    }),
  });
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', `  ${HANDSET_EXAMPLE}  `);
  await m.view.loadUrls();
  // Requests: through the configured proxy (never a relative address), with the normalized addresses, credentials omitted.
  assert.deepEqual(m.calls.map((call) => call.url).sort(), [
    `${PROXY}?url=${encodeURIComponent(MAIN_FETCHED)}`, `${PROXY}?url=${encodeURIComponent(HANDSET_FETCHED)}`,
  ].sort());
  assert.ok(m.calls.every((call) => call.init.credentials === 'omit' && call.init.method === 'GET'));
  // Slots, release and wording come from the worker's inspection, not from the fetch.
  assert.equal(m.view.slots.main.release.name, 'TRITON');
  assert.equal(m.view.slots.handset.release.id, 'TRITON-5.8-65.3');
  assert.equal(m.view.slots.main.name, 'pvlL3Iilv4o_Tu5lggngZAUt.srec');
  assert.equal(m.view.slots.main.source, MAIN_FETCHED);
  assert.match(m.slotText('main'), /✓ TRITON main controller 5\.8/);
  assert.match(m.slotText('main'), /Loaded from https:\/\/web\.archive\.org\/web\/20261008041333id_\//);
  assert.match(m.slotText('handset'), /SHA-256 sha-handset-TRITON-5\.8-65\.3/);
  assert.match(m.info('main'), /^Fetched https:\/\/web\.archive\.org\/web\/20261008041333id_\/\S+ \(22 bytes\) and verified as TRITON main controller 5\.8\.$/, 'the size is read before the buffer is transferred');
  assert.match(m.info('handset'), / \(25 bytes\) and verified as TRITON handset 65\.3\.$/);
  assert.match(m.el('release-line').textContent, /^Release: TRITON-5\.8-65\.3 label\. Both files are from this release\.$/, 'the label is the worker\'s');
  assert.equal(m.el('url-status').textContent, 'Loaded and verified both files.');
  assert.equal(m.el('boot').disabled, false, 'booting is now possible');
  assert.equal(m.el('url-load').disabled, false);
  assert.equal(m.el('url-cancel').hidden, true);
  assert.ok(m.view.urlProgressEls.main.hidden && m.view.urlProgressEls.handset.hidden, 'progress bars are hidden again');
  assert.equal(m.view.rejected.length, 0);
  // The worker holds the very bytes (the boot would use them): same path as a drop.
  assert.equal(m.runtime.runtime.firmware.main.report.srecSha256, 'sha-main-TRITON-5.8-65.3');
  assert.equal(new TextDecoder().decode(m.runtime.runtime.firmware.handset.bytes), srec('handset', 'TRITON-5.8-65.3'));
});

test('urls (page): the content decides the slot; a single address is enough; the remember option is untouched', async () => {
  const m = await mountEntry({ respond: proxyFor({ [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'NEPTUN-5.8-65.3')) }) });
  assert.equal(m.el('remember').checked, false);
  m.type('main', HANDSET_EXAMPLE); // the handset image in the main field
  m.type('handset', '');
  await m.view.loadUrls();
  assert.equal(m.view.slots.main, null);
  assert.equal(m.view.slots.handset.release.name, 'NEPTUN');
  assert.match(m.info('main'), /It is the handset 65\.3 image, so it went to that slot \(the file's content decides, not the field\)\./);
  assert.equal(m.calls.length, 1, 'the empty field was not fetched');
  assert.equal(m.el('remember').checked, false, 'loading from a URL does not check the remember option');
  assert.match(m.el('boot-hint').textContent, /Still needed: the NEPTUN main controller 5\.8 firmware file\./);
});

test('entry: "Remember these files" is on by default, unchecking it boots without remembering, Forget removes them, and without the origin-private file system it is off and explained', async () => {
  // The markup starts the box checked and no longer says "off by default".
  assert.match(html, /<input type="checkbox" id="remember" checked><span>Remember these files in this browser/);
  assert.doesNotMatch(/<input type="checkbox" id="remember"[^>]*><span>[^<]*/.exec(html)[0], /off by default/);
  const answers = () => proxyFor({
    [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3')),
    [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')),
  });
  const fill = async (m) => { m.type('main', MAIN_EXAMPLE); m.type('handset', HANDSET_EXAMPLE); await m.view.loadUrls(); };
  const remembered = (m) => m.runtime.storage.list('firmware').then((files) => files.map((file) => file.name).sort());

  // With the origin-private file system: checked, enabled, no warning; booting stores the pair.
  const booted = [];
  const m = await mountEntry({ storageKind: 'opfs', respond: answers(), booted: (result) => booted.push(result) });
  assert.equal(m.el('remember').checked, true, 'on by default');
  assert.equal(m.el('remember').disabled, false);
  assert.equal(m.el('remember-note').hidden, true);
  await fill(m);
  assert.equal(m.el('remember').checked, true, 'loading from URLs leaves it as it was');
  await m.view.boot('stored');
  assert.equal(booted.length, 1);
  assert.deepEqual(await remembered(m), ['handset.srec', 'index.json', 'main.srec'], 'the verified pair is remembered without the user having checked anything');
  assert.deepEqual(m.runtime.messages.filter((message) => message.type === 'notice'), [], 'and nothing is warned about');
  // Forget keeps working: it removes the files and unticks the box.
  await m.view.forget();
  assert.deepEqual(await remembered(m), []);
  assert.equal(m.el('remember').checked, false);
  await m.runtime.request('close-session');

  // Unchecked before booting: the session starts and nothing is stored.
  const unchecked = await mountEntry({ storageKind: 'opfs', respond: answers(), booted: () => {} });
  await fill(unchecked);
  unchecked.el('remember').checked = false;
  await unchecked.view.boot('stored');
  assert.deepEqual(await remembered(unchecked), []);
  await unchecked.runtime.request('close-session');

  // Without the origin-private file system (IndexedDB or memory only): off, disabled and explained, and booting works as before.
  const memory = await mountEntry({ storageKind: 'memory', respond: answers(), booted: () => {} });
  assert.equal(memory.el('remember').checked, false);
  assert.equal(memory.el('remember').disabled, true);
  assert.equal(memory.el('remember-note').hidden, false);
  assert.match(memory.el('remember-note').textContent, /origin-private file system/);
  await fill(memory);
  await memory.view.boot('stored');
  assert.deepEqual(await remembered(memory), []);
  assert.deepEqual(memory.runtime.messages.filter((message) => message.type === 'notice'), [], 'no "could not be remembered" warning: the option was never offered');
  await memory.runtime.request('close-session');
});

test('entry: the sources fold once both files are in and open again when one is missing; the remembered banner offers "Use" only while the files are not in the slots', async () => {
  const answers = proxyFor({
    [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3')),
    [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')),
  });
  const m = await mountEntry({ storageKind: 'opfs', respond: answers, booted: () => {} });
  const sources = m.el('firmware-sources');
  assert.equal(sources.open, true, 'no file yet: the sources are open');
  // One file is not enough; the second one folds the sources, which brings the Boot button into reach.
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', '');
  await m.view.loadUrls();
  assert.equal(sources.open, true, 'one file is still missing');
  m.type('main', '');
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(sources.open, false, 'both files are in');
  // Removing one opens them again; a drag over the page shows the drop zone even when they are folded.
  await m.view.removeSlot('handset');
  assert.equal(sources.open, true, 'a file is missing again');
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(sources.open, false);
  const dragged = m.document.dispatch('dragenter', { dataTransfer: { files: [] } });
  assert.equal(dragged.defaultPrevented, true);
  assert.equal(sources.open, true, 'dragging a file over the page shows the drop zone');
  // The drop zone is a mouse target; its button is the one keyboard stop, so it is not a second, nested "button".
  assert.doesNotMatch(html, /id="dropzone"[^>]*(?:tabindex|role=)/);

  // Remembered files: the page takes them automatically and the banner then only says so and offers Forget.
  await m.view.boot('stored');
  await m.runtime.request('close-session');
  const info = await m.runtime.request('info');
  m.view.slots.main = null;
  m.view.slots.handset = null;
  m.view.renderRemembered(info.remembered);
  m.view.refresh();
  assert.equal(m.el('remembered-banner').hidden, false);
  assert.equal(m.el('use-remembered').hidden, false, 'the files are not in the slots yet');
  assert.match(m.el('remembered-text').textContent, /^Verified TRITON firmware files from an earlier visit are stored in this browser: /);
  await m.view.useRemembered();
  assert.equal(m.view.rememberedInUse(), true);
  assert.equal(m.el('use-remembered').hidden, true, 'already in use: nothing to offer');
  assert.equal(m.el('forget-remembered').hidden, false);
  assert.match(m.el('remembered-text').textContent, /^Using the verified TRITON firmware files remembered from an earlier visit\.$/);
  assert.equal(sources.open, false, 'remembered files in use: the sources stay folded');
  await m.view.removeSlot('main');
  assert.equal(m.el('use-remembered').hidden, false, 'a removed file can be taken again');
  assert.equal(sources.open, true);
  await m.view.useRemembered();
  assert.equal(m.el('use-remembered').hidden, true);
  assert.equal(sources.open, false);
  // The Remember option applies when booting, so it sits with the boot-time choices, after the sources and the start options.
  assert.ok(html.indexOf('id="remembered-banner"') < html.indexOf('id="firmware-sources"') && html.indexOf('id="start-options"') < html.indexOf('id="remember"') && html.indexOf('id="remember"') < html.indexOf('id="boot"'));
  await m.runtime.request('close-session');
});

test('urls (page): a mixed release pair is refused exactly like a dropped file, whether it comes from a URL or from a drop', async () => {
  const m = await mountEntry({
    respond: proxyFor({
      [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3')),
      [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'NEPTUN-5.8-65.3')),
    }),
  });
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(m.view.slots.main.release.name, 'TRITON');
  assert.equal(m.view.slots.handset, null, 'the NEPTUN handset did not take the slot');
  assert.match(m.info('handset'), /was not accepted: this is the NEPTUN handset 65\.3 image, but the main controller 5\.8 file provided is TRITON\. Main and handset must come from the same release/);
  assert.match(m.rejectedText(), /rlVpEk1qk8-0r1E4vMHNAjQG\.srec/);
  assert.match(m.rejectedText(), /Loaded from https:\/\/web\.archive\.org\/web\/20261008041427id_\//);
  assert.match(m.rejectedText(), /SHA-256 sha-handset-NEPTUN-5\.8-65\.3/);
  assert.equal(m.el('url-status').textContent, '1 of 2 loaded; see the messages above.');
  assert.equal(m.el('boot').disabled, true);
  // A dropped NEPTUN main file is refused against the fetched TRITON files in the same way.
  await m.view.addFiles([{ name: 'ngc_handset_65.3_NEPTUN.srec', size: 20, arrayBuffer: async () => text(srec('handset', 'NEPTUN-5.8-65.3')).buffer }]);
  assert.equal(m.view.slots.handset, null);
  assert.equal(m.view.rejected.filter((item) => /same release/.test(item.message)).length, 2);
});

test('urls (page): an unknown firmware file is refused with its SHA-256', async () => {
  const m = await mountEntry({ respond: proxyFor({ [MAIN_FETCHED]: () => proxyBody('S0unknown') }) });
  m.type('main', MAIN_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(m.view.slots.main, null);
  assert.match(m.info('main'), /but it was not accepted: This S-record file is not a known firmware image\. SHA-256 (ab){32}\.$/);
  assert.match(m.rejectedText(), new RegExp(`SHA-256 ${'ab'.repeat(32)}`));
});

test('urls (page): fetch failures are shown per field, without touching the slots', async () => {
  const answers = {
    [MAIN_FETCHED]: () => proxyJson(502, { error: 'upstream_status', message: 'The upstream answered HTTP 404.', upstreamStatus: 404 }),
    [HANDSET_FETCHED]: () => proxyBody('<!doctype html>'),
  };
  const m = await mountEntry({ respond: proxyFor(answers) });
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(m.info('main'), 'The source answered HTTP 404.');
  assert.match(m.info('handset'), /not an S-record file \(it does not start with S0\)/);
  assert.equal(m.el('url-status').textContent, '0 of 2 loaded; see the messages above.');
  assert.deepEqual([m.view.slots.main, m.view.slots.handset], [null, null]);
  assert.equal(m.view.rejected.length, 0, 'a failed fetch is not a refused file');
  // Editing a field clears its stale outcome and validates the new text.
  m.type('main', 'https://api.multi3s.com/static/other.srec');
  assert.equal(m.info('main'), 'Will be fetched: https://api.multi3s.com/static/other.srec');
  // The proxy cannot be reached.
  const down = await mountEntry({ respond: () => { throw new TypeError('Failed to fetch'); } });
  down.type('handset', HANDSET_EXAMPLE);
  await down.view.loadUrls();
  assert.match(down.info('handset'), /Could not reach the firmware proxy \(https:\/\/firmware-proxy\.example\/api\/firmware\)/);
  assert.equal(down.el('url-load').disabled, false, 'the user can try again');
});

test('urls (page): a disallowed address never reaches fetch, and Load stays disabled until every filled field is valid', async () => {
  const m = await mountEntry({ respond: proxyFor({ [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')) }) });
  m.type('main', 'https://evil.example/static/x.srec');
  m.type('handset', HANDSET_EXAMPLE);
  assert.match(m.info('main'), /Only api\.multi3s\.com and web\.archive\.org addresses are accepted/);
  assert.equal(m.el('url-load').disabled, true);
  await m.view.loadUrls(); // a forced attempt (for example Enter pressed in a field) is refused too
  assert.equal(m.calls.length, 0);
  assert.equal(m.el('url-status').textContent, 'Fix the addresses marked above first.');
  assert.equal(m.view.slots.handset, null, 'the valid field was not loaded either');
  // The same address twice is refused before any request.
  m.type('main', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(m.calls.length, 0);
  assert.match(m.info('handset'), /same address/);
  // No field at all.
  m.type('main', '');
  m.type('handset', '');
  await m.view.loadUrls();
  assert.equal(m.el('url-status').textContent, 'Enter at least one address.');
  assert.equal(m.calls.length, 0);
});

test('urls (page): the form submits on Enter, shows progress while fetching and Cancel stops the downloads', async () => {
  const gates = [];
  const m = await mountEntry({
    respond: (url, init) => new Promise((resolve, reject) => {
      gates.push({ url, resolve });
      init.signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
    }),
  });
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', HANDSET_EXAMPLE);
  const submitted = m.el('url-form').dispatch('submit'); // Enter in a field
  assert.equal(submitted.defaultPrevented, true, 'the form never navigates');
  await settle();
  assert.equal(gates.length, 2);
  assert.equal(m.el('url-cancel').hidden, false);
  assert.equal(m.el('url-load').disabled, true);
  assert.equal(m.el('url-main').disabled, true, 'the fields are locked while fetching');
  assert.equal(m.el('boot').disabled, true);
  assert.equal(m.el('boot-hint').textContent, 'Loading from URLs…');
  assert.equal(m.el('url-main-progress').hidden, false);
  assert.match(m.info('main'), /^Fetching https:\/\/web\.archive\.org\/web\/20261008041333id_\//);
  m.el('url-cancel').dispatch('click');
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(m.info('main'), 'Canceled.');
  assert.equal(m.info('handset'), 'Canceled.');
  assert.equal(m.el('url-status').textContent, 'Canceled.');
  assert.equal(m.el('url-cancel').hidden, true);
  assert.equal(m.el('url-load').disabled, false);
  assert.equal(m.el('url-main').disabled, false);
  assert.equal(m.el('url-main-progress').hidden, true);
  assert.equal(m.clock.timers.size, 0, 'the timeout timer is gone');
  assert.equal(m.view.urlLoading, null);
});

test('urls (page): a download that does not finish times out and reports it', async () => {
  const m = await mountEntry({
    timeoutMs: 45_000,
    respond: (url, init) => new Promise((resolve, reject) => init.signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')))),
  });
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', '');
  const loading = m.view.loadUrls();
  await settle();
  assert.equal(m.clock.timers.size, 1);
  m.clock.advance(44_999);
  assert.equal(m.view.urlLoading.timedOut, false);
  m.clock.advance(1);
  await loading;
  assert.equal(m.info('main'), 'Timed out after 45 s without a complete file.');
  assert.equal(m.el('url-status').textContent, '0 of 1 loaded; see the messages above.');
  assert.equal(m.el('url-load').disabled, false);
});

test('urls (page): the progress bar follows the download, indeterminate when the size is unknown', async () => {
  const chunk = (n) => text(`S0${'x'.repeat(n - 2)}`);
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const m = await mountEntry({
    respond: () => new Response(new ReadableStream({
      async start(controller) {
        controller.enqueue(chunk(100));
        await gate;
        controller.enqueue(chunk(50));
        controller.close();
      },
    }), { status: 200, headers: { 'content-length': '150' } }),
    engine: new SrecFakeEngine(),
  });
  m.type('main', MAIN_EXAMPLE);
  const loading = m.view.loadUrls();
  await new Promise((resolve) => setTimeout(resolve, 20));
  const bar = m.el('url-main-progress');
  assert.equal(bar.hidden, false);
  assert.deepEqual([bar.getAttribute('value'), bar.getAttribute('max')], ['100', '150']);
  assert.match(m.info('main'), /… 100 bytes of 150 bytes$/);
  release();
  await loading;
  assert.equal(bar.hidden, true);
  // The loaded text is not a firmware image: refused with the engine's message after a complete fetch.
  assert.match(m.info('main'), /but it was not accepted/);
  // Without a Content-Length the bar has no value (indeterminate) and the text shows the bytes so far.
  let finish;
  const done = new Promise((resolve) => { finish = resolve; });
  const unknown = await mountEntry({
    respond: () => new Response(new ReadableStream({
      async start(controller) {
        controller.enqueue(chunk(64));
        await done;
        controller.close();
      },
    }), { status: 200 }),
  });
  unknown.type('handset', HANDSET_EXAMPLE);
  const pending = unknown.view.loadUrls();
  await new Promise((resolve) => setTimeout(resolve, 20));
  const indeterminate = unknown.el('url-handset-progress');
  assert.equal(indeterminate.hidden, false);
  assert.equal(indeterminate.getAttribute('value'), null);
  assert.match(unknown.info('handset'), /… 64 bytes$/);
  finish();
  await pending;
});

test('page: a verified slot and the session information never print "null" (append() turns a null child into text), with or without a source address', async () => {
  const m = await mountEntry();
  await m.view.addFiles([{ name: 'ngc_main_5.8_TRITON.srec', size: 22, arrayBuffer: async () => text(srec('main', 'TRITON-5.8-65.3')).buffer }]);
  assert.match(m.slotText('main'), /✓ TRITON main controller 5\.8/);
  assert.doesNotMatch(m.slotText('main'), /null|undefined|Loaded from/, 'a chosen or dropped file has no source address');
  // The session information of a handset-only session (no main slot) and of a release the page could not name.
  const session = await mount();
  for (const shown of [
    { options: { mode: 'handset', adcSample: 400 }, profile: 'stored', release: describeRelease(DEFAULT_RELEASE_ID), slots: { main: null, handset: fakeSlot('handset.srec') } },
    { options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: null, slots: { main: fakeSlot('main.srec'), handset: fakeSlot('handset.srec') } },
  ]) {
    session.view.show(shown);
    const rendered = session.document.getElementById('firmware-info').textContent;
    assert.doesNotMatch(rendered, /null|undefined/, rendered);
    assert.match(rendered, /Handset 65\.3/);
  }
});

test('entry: a waiting card is short, a verified card is one line with the hash and the checks in a closed <details>, and a failed check is never folded away', async () => {
  const m = await mountEntry();
  const card = (role) => m.document.getElementById(`slot-${role}`);
  // Waiting: no verified line, no details, only the state text (the expected file names stay in the markup).
  assert.equal(card('main').querySelector('.slot-line'), null);
  assert.equal(card('main').querySelector('details'), null);
  assert.equal(m.slotText('main'), 'Waiting for the file.');
  assert.equal(card('main').classList.contains('ok'), false);

  await m.view.addFiles([{ name: 'ngc_main_5.8_TRITON.srec', size: 22, arrayBuffer: async () => text(srec('main', 'TRITON-5.8-65.3')).buffer }]);
  assert.equal(card('main').classList.contains('ok'), true);
  const line = card('main').querySelector('.slot-line');
  assert.ok(line, 'one line: check mark, release and board, file, Remove');
  assert.match(line.querySelector('.slot-title').textContent, /^✓ TRITON main controller 5\.8$/);
  assert.equal(line.querySelector('.slot-title span').getAttribute('aria-label'), 'Verified', 'the check mark has a text for screen readers');
  assert.match(line.querySelector('.slot-file').textContent, /^ngc_main_5\.8_TRITON\.srec · \d+ bytes$/);
  const remove = line.querySelector('button.slot-remove');
  assert.equal(remove.textContent, 'Remove');
  assert.equal(remove.getAttribute('aria-label'), 'Remove the main controller 5.8 file', 'two Remove buttons need distinct names');
  // The hash, the checks and the source address are inside a <details> that starts closed; nothing else shows them.
  const details = card('main').querySelector('details');
  assert.equal(details.open, false, 'closed by default');
  assert.equal(details.querySelector('summary').textContent, '1 of 1 checks passed · SHA-256');
  assert.match(details.textContent, /SHA-256 sha-main-TRITON-5\.8-65\.3/);
  assert.match(details.querySelector('.check-list').textContent, /✓ check: ok/);
  assert.equal(line.textContent.includes('sha-main'), false, 'the line itself carries no hash');
  assert.equal(card('main').querySelector('.check-list.failed'), null, 'no failed check, no extra list');

  // A refresh (every start option change calls it) keeps the card, its opened details and the focus inside it.
  details.open = true;
  m.view.refresh();
  assert.equal(card('main').querySelector('details'), details, 'the card is not rebuilt for an unchanged file');
  assert.equal(details.open, true);

  // A failed check is shown outside the closed details, in the failure color class.
  const slot = m.view.slots.main;
  m.view.slots.main = { ...slot, report: { ...slot.report, checks: [{ ok: true, name: 'a', detail: 'fine' }, { ok: false, name: 'b', detail: 'broken' }] } };
  m.view.refresh();
  const failed = card('main').querySelector('.check-list.failed');
  assert.ok(failed, 'a failed check is listed');
  assert.match(failed.textContent, /^✗ b: broken$/);
  assert.notEqual(failed.parent.tagName, 'DETAILS', 'not folded away');
  assert.equal(card('main').querySelector('details').open, false, 'a new rendering starts with a closed details');
  assert.equal(card('main').querySelector('summary').textContent, '1 of 2 checks passed · SHA-256');

  // Remove returns the card to the short waiting state.
  card('main').querySelector('button.slot-remove').click();
  await settle();
  assert.equal(m.view.slots.main, null);
  assert.equal(card('main').querySelector('.slot-line'), null);
  assert.equal(m.slotText('main'), 'Waiting for the file.');
  assert.equal(card('main').classList.contains('ok'), false);
  // The markup keeps the heading, the state and the expected names in that order, and the verified card is styled one line.
  assert.match(html, /<h3>Main controller 5\.8<\/h3>\s*<div class="state" data-part="state">Waiting for the file\.<\/div>\s*<p class="expected">ngc_main_5\.8_TRITON\.srec or ngc_main_5\.8_NEPTUN\.srec<\/p>/);
  const css = fs.readFileSync(path.join(here, 'style.css'), 'utf8');
  assert.match(css, /\.slot-line \{[^}]*display: grid/);
});

test('structure: the URL form is a real form with labeled fields, and the CSP only gained the loopback dev proxies in connect-src (the built site adds the configured proxy origin)', () => {
  assert.match(html, /<form id="url-form"[^>]*novalidate/);
  assert.match(html, /<button type="submit" id="url-load"/, 'Enter in a field submits the form');
  assert.match(html, /<label>Main controller 5\.8 URL<input id="url-main" type="text"/);
  assert.match(html, /<label>Handset 65\.3 URL<input id="url-handset" type="text"/);
  assert.match(html, /<p id="url-status" class="small" role="status" aria-live="polite">/);
  const csp = (/http-equiv="Content-Security-Policy" content="([^"]+)"/.exec(html) || [])[1];
  assert.ok(csp, 'the page carries its CSP');
  assert.deepEqual(Object.fromEntries(csp.split(';').map((part) => part.trim().split(/\s+(.*)/s).slice(0, 2))), {
    'default-src': "'none'",
    'script-src': "'self' 'wasm-unsafe-eval'",
    'style-src': "'self'",
    'img-src': "'self' data: blob:",
    'connect-src': "'self' http://127.0.0.1:* http://localhost:*",
    'worker-src': "'self'",
    'base-uri': "'none'",
    'form-action': "'none'",
  });
  const served = fs.readFileSync(path.join(here, 'serve.py'), 'utf8');
  const header = [...served.matchAll(/^CSP = \(((?:.|\n)*?)\)\nWASM_NAME/gm)][0][1];
  const joined = [...header.matchAll(/"([^"]*)"/g)].map((match) => match[1]).join('');
  assert.equal(joined, csp, 'serve.py sends the same policy as the page <meta>');
});

// =====================================================================================================
// deco.js and the decompression handling in the page: the read-only warnings, the surface start, the oxygen reset
// =====================================================================================================

const decoState = (health, extra = {}) => ({
  ...initialState, inputs: { ...initialInputs },
  decoHealth: { tissues: 'valid', oxygen: 'calibrated', details: {}, ...health }, ...extra,
});
// Only the invalid-tissue warning exists: the oxygen warning was removed (its paragraph with it).
const shownWarnings = (m) => ['tissues'].filter((id) => !m.document.getElementById(`deco-warning-${id}`).hidden).map((id) => m.document.getElementById(`deco-warning-${id}`).textContent);

test('deco: only a proven bad state warns, and each warning names the next step', () => {
  assert.deepEqual(deco.decoWarnings(null), []);
  assert.deepEqual(deco.decoWarnings({}), [], 'an engine without the report');
  assert.deepEqual(deco.decoWarnings({ decoHealth: { tissues: 'unknown', oxygen: 'unknown' } }), [], 'unknown (NEPTUN, handset only, not yet running) never warns');
  assert.deepEqual(deco.decoWarnings({ decoHealth: { tissues: 'valid', oxygen: 'calibrated' } }), []);
  assert.deepEqual(deco.decoWarnings({ decoHealth: { tissues: 'valid', oxygen: 'uncalibrated' } }), [], 'uncalibrated oxygen is not shown as a warning');
  // The next step is to reset the profile, which creates an initialized EEPROM; the text names the page's controls.
  const reset = "Decompression state invalid: this profile's stored tissues are blank. Reset the profile (Advanced → Profile and evidence → Reset profile) to start with an initialized EEPROM.";
  assert.equal(deco.INVALID_TISSUES_WARNING, reset);
  assert.deepEqual(deco.decoWarnings({ decoHealth: { tissues: 'invalid', oxygen: 'calibrated' } }), [{ id: 'tissues', text: reset }]);
  assert.deepEqual(deco.decoWarnings({ decoHealth: { tissues: 'invalid', oxygen: 'uncalibrated' } }).map((w) => w.id), ['tissues']);
  // Every control the text names exists on the page: the Advanced panel, its "Profile and evidence" section and the reset button.
  assert.match(html, /<summary>Advanced ·/);
  assert.match(html, /<h3 id="profile-heading">Profile and evidence<\/h3>/);
  assert.match(html, /<button type="button" id="profile-reset" class="danger">Reset profile…<\/button>/);
  // The surface pressure setting: a finite number inside the engine's range, else null; a blank text is not zero.
  assert.equal(deco.parseSurfacePressure('900'), 900);
  assert.equal(deco.parseSurfacePressure(1013.25), 1013.25);
  assert.equal(deco.parseSurfacePressure(' 100 '), 100);
  assert.equal(deco.parseSurfacePressure('30000'), 30000);
  for (const bad of ['', '  ', '99.9', '30000.1', 'abc', NaN, Infinity, null, undefined, '0x10']) assert.equal(deco.parseSurfacePressure(bad), null, String(bad));
  // The session information: the report with the reason when unknown, and what the fixtures did.
  assert.match(deco.healthLine({ decoHealth: { tissues: 'unknown', oxygen: 'valid', details: { tissues: 'Unknown for NEPTUN-5.8-65.3: not proven.' } } }), /tissues unknown.*Unknown for NEPTUN-5\.8-65\.3: not proven\./);
  assert.equal(deco.healthLine({}), null);
  assert.deepEqual(deco.fixtureLines({}), []);
  assert.deepEqual(deco.fixtureLines({ startAtSurface: { enabled: true, surfacePressureMbar: 900, note: 'Depth 0.' } }), ['Fixture: start at the surface (on, surface 900 mbar). Depth 0.']);
  // The EEPROM factory image comes first: whether this session created its EEPROM from it, or the profile already had one.
  assert.deepEqual(deco.fixtureLines({ eepromFactoryInit: { applied: true, reason: 'Created.' }, startAtSurface: { enabled: false } }), ['Fixture: EEPROM factory image (this session created the EEPROM from it). Created.', 'Fixture: start at the surface (off).']);
  assert.equal(deco.fixtureLines({ eepromFactoryInit: { applied: false, reason: 'Not applied: the profile already holds an EEPROM.' } })[0], 'Fixture: EEPROM factory image (not applied). Not applied: the profile already holds an EEPROM.');
  // The older repair fixture is gone: a state from an older engine that still carries its member shows no line for it.
  assert.deepEqual(deco.fixtureLines({ decoStorageFixture: { enabled: true, applied: true, reason: 'Repaired.' } }), []);
});

test('page: the decompression warnings show only for a proven bad state, name the next step and go away with it', async () => {
  const m = await mount();
  const show = (health, extra) => m.view.onState({ state: decoState(health, extra), host: { ...hostBase } });
  assert.equal(m.document.getElementById('deco-health').hidden, true, 'nothing is shown before a state arrives');
  show({});
  assert.equal(m.document.getElementById('deco-health').hidden, true, 'a healthy state shows nothing');
  show({ oxygen: 'uncalibrated' });
  assert.equal(m.document.getElementById('deco-health').hidden, true, 'uncalibrated oxygen is not shown as a warning');
  show({ oxygen: 'uncalibrated', tissues: 'invalid' });
  assert.deepEqual(shownWarnings(m), [deco.INVALID_TISSUES_WARNING]);
  show({ oxygen: 'calibrated', tissues: 'invalid' });
  assert.deepEqual(shownWarnings(m), [deco.INVALID_TISSUES_WARNING]);
  assert.match(shownWarnings(m)[0], /^Decompression state invalid: this profile's stored tissues are blank\. Reset the profile \(Advanced → Profile and evidence → Reset profile\) to start with an initialized EEPROM\.$/);
  show({ oxygen: 'unknown', tissues: 'unknown' });
  assert.equal(m.document.getElementById('deco-health').hidden, true, 'unknown never warns (NEPTUN reports it)');
  // An engine build without the report: no member, no warning, nothing breaks.
  m.view.onState({ state: { ...initialState, inputs: { ...initialInputs } }, host: { ...hostBase } });
  assert.equal(m.document.getElementById('deco-health').hidden, true);
  // The region is a live status region with the warnings inside, and Advanced describes the report and the fixtures.
  assert.match(html, /<section id="deco-health" class="deco-health" aria-label="Decompression state" role="status" hidden>/);
  show({ tissues: 'invalid' }, {
    eepromFactoryInit: { applied: true, reason: 'This session created the EEPROM from the factory image.' },
    startAtSurface: { enabled: true, surfacePressureMbar: 1013.25, note: 'Depth 0 at every start.' },
  });
  m.document.getElementById('firmware-details').open = true;
  m.view.render();
  const info = m.document.getElementById('session-info').textContent;
  assert.match(info, /Decompression state \(read-only report\): tissues invalid, oxygen calibrated\./);
  assert.match(info, /Fixture: EEPROM factory image \(this session created the EEPROM from it\)\. This session created the EEPROM from the factory image\./);
  assert.doesNotMatch(info, /stored decompression state repair/);
  assert.match(info, /Fixture: start at the surface \(on, surface 1013\.25 mbar\)\. Depth 0 at every start\./);
});

test('page: the cold boot hint sits at the Cold boot button and appears in the start options when a cold boot is chosen', async () => {
  assert.match(html, /<button type="button" id="cold-boot" data-action="cold" title="[^"]*oxygen calibration[^"]*">Cold boot<\/button>/);
  assert.match(/<p id="cold-hint"[^>]*>([^<]*)<\/p>/.exec(html)[1], /Cold boot: the firmware clears the oxygen-cell calibration.*Calibrate again afterwards: Menu → Calibration → Air → Auto → Start → Save\./);
  const m = await mountEntry();
  assert.equal(m.el('start-cold-hint').hidden, true, 'the default boot is a handset wake: no hint');
  m.el('start-boot-mode').value = 'cold';
  m.el('start-boot-mode').dispatch('change');
  assert.equal(m.el('start-cold-hint').hidden, false);
  assert.match(m.el('start-cold-hint').textContent, /clear the oxygen-cell calibration.*Calibrate again afterwards/);
  m.el('start-boot-mode').value = 'handset-wake';
  m.el('start-boot-mode').dispatch('change');
  assert.equal(m.el('start-cold-hint').hidden, true);
});

test('entry and runtime: the start at the surface is a start option, on by default, with the remembered surface pressure; the factory image has no option', async () => {
  const m = await mountEntry();
  assert.equal(m.el('start-surface').checked, true, 'the start at the surface starts checked');
  assert.deepEqual(plain(m.view.options()), { ...plain(m.view.options()), startAtSurface: true, surfacePressureMbar: null });
  for (const removed of ['eepromFactoryInit', 'decoStorageFixture']) assert.equal(removed in m.view.options(), false, `${removed} is not a start option any more`);
  m.el('start-surface').checked = false;
  globalThis.window.localStorage.setItem('ngc-wasm.surface-pressure', '900');
  const options = m.view.options();
  assert.deepEqual([options.startAtSurface, options.surfacePressureMbar], [false, 900]);
  globalThis.window.localStorage.setItem('ngc-wasm.surface-pressure', '50');
  assert.equal(m.view.options().surfacePressureMbar, null, 'a remembered value outside the engine\'s range is not sent');

  // The worker passes them on: on unless switched off; the surface pressure only when valid. The removed keys are not passed on.
  assert.equal(Runtime.normalizeConfig({}).startAtSurface, true);
  assert.equal('surfacePressureMbar' in Runtime.normalizeConfig({}), false);
  assert.equal('surfacePressureMbar' in Runtime.normalizeConfig({ surfacePressureMbar: 50 }), false);
  assert.deepEqual(Runtime.normalizeConfig({ startAtSurface: false, surfacePressureMbar: 900 }), { ...Runtime.normalizeConfig({}), startAtSurface: false, surfacePressureMbar: 900 });
  const stale = Runtime.normalizeConfig({ eepromFactoryInit: false, decoStorageFixture: false });
  assert.equal('eepromFactoryInit' in stale || 'decoStorageFixture' in stale, false, 'an old option never reaches the engine');

  const engine = new FakeEngine();
  const h = new RuntimeHarness(engine);
  await h.request('init');
  await h.inspect('main', 'TRITON-5.8-65.3');
  await h.inspect('handset', 'TRITON-5.8-65.3');
  await h.request('boot', { options: { mode: 'dual', startPaused: true, surfacePressureMbar: 950 } });
  const first = engine.created[0].config;
  assert.deepEqual([first.startAtSurface, first.surfacePressureMbar], [true, 950]);
  // An action that recreates the boards and carries a surface pressure changes the session's setting; a profile import then uses it.
  await h.request('action', { request: { action: 'reset', surfacePressureMbar: 900 } });
  await h.request('action', { request: { action: 'inputs', inputs: { oxygen1Mv: 11 }, surfacePressureMbar: 123 } });
  await h.request('import-profile', { files: [{ name: 'eeprom.bin', data: new Uint8Array([9]) }] });
  assert.equal(engine.created[1].config.surfacePressureMbar, 900, 'the Restart\'s value, not the boot\'s, and not one carried by another action');
  assert.equal(engine.created[1].config.startAtSurface, true, 'the fixture switch survives a profile import');
  await h.request('close-session');
  // An older engine build that does not know the options: they are dropped one at a time and the page can say so.
  const older = new FakeEngine();
  older.rejectOptions = ['startAtSurface', 'surfacePressureMbar'];
  const o = new RuntimeHarness(older);
  await o.request('init');
  await o.inspect('main', 'TRITON-5.8-65.3');
  await o.inspect('handset', 'TRITON-5.8-65.3');
  await o.request('boot', { options: { mode: 'dual', startPaused: true, surfacePressureMbar: 900 } });
  assert.deepEqual([...older.unsupportedOptions].sort(), ['startAtSurface', 'surfacePressureMbar']);
  await o.request('close-session');
});

test('page: Restart, Cold boot, Wake and a serial change carry the surface pressure of the basic view, which is remembered', async () => {
  const h = await scenarioHarness();
  await h.edit('surface-pressure', 900);
  assert.equal(globalThis.window.localStorage.getItem('ngc-wasm.surface-pressure'), '900', 'remembered in this browser');
  await applyResponse(h, 0);
  for (const [index, action] of ['reset', 'cold', 'wake'].entries()) {
    h.sendAction(action);
    await settle();
    assert.deepEqual(h.posts()[index + 1].body, { action, surfacePressureMbar: 900 }, action);
    await h.respond(h.posts()[index + 1]);
  }
  // A half-typed or out-of-range value is neither sent nor remembered; the last valid one is.
  await h.edit('surface-pressure', '50');
  assert.equal(globalThis.window.localStorage.getItem('ngc-wasm.surface-pressure'), '900');
  assert.equal(h.pending().length, 0, 'an invalid edit sends nothing');
  h.sendAction('reset');
  await settle();
  assert.equal(h.posts().at(-1).body.surfacePressureMbar, 900, 'the last valid setting goes with the Restart');
  await h.respond(h.posts().at(-1));
  // Other actions carry nothing extra.
  h.sendAction('step');
  await settle();
  assert.deepEqual(h.posts().at(-1).body, { action: 'step' });
});

test('page: after a Restart that returned the unit to the surface the depth slider shows 0 m and the oxygen cells keep their values', async () => {
  const deepInputs = { pressure1Mbar: 4600, pressure2Mbar: 4600, oxygen1Mv: 12.5, oxygen2Mv: 12, oxygen3Mv: 12.25 };
  const h = await scenarioHarness(deepInputs);
  assert.ok(Number(h.control('depth').value) > 30, 'the form starts at depth');
  h.sendAction('reset');
  await settle();
  const request = h.posts()[0];
  assert.equal(request.body.surfacePressureMbar, 1013.25);
  // The worker broadcasts the state of the new boards (generation 2), where the engine returned both pressures to the surface
  // and kept the cells, then answers the request.
  const next = { ...initialState, inputs: { ...initialInputs, ...deepInputs, pressure1Mbar: 1013.25, pressure2Mbar: 1013.25 } };
  h.view.onState({ state: clone(next), host: { ...hostBase, generation: 2 } });
  request.done = true;
  request.resolve(clone(next));
  await settle();
  assert.equal(Number(h.control('depth').value), 0, 'the depth slider shows 0 m');
  assert.equal(h.control('depth-value').textContent, '0.00 m');
  assert.equal(Number(h.raw('pressure1Mbar').value), 1013.25);
  assert.equal(Number(h.raw('pressure2Mbar').value), 1013.25);
  near(Number(h.control('oxygen-base').value), 12.25, 'the cells are the ones the user set');
  near(Number(h.raw('oxygen1Mv').value), 12.5, 'raw cell 1');
  assert.equal(h.status(), 'Inputs applied');
  // A board creation that left the sensors as they were does not touch the form: a raw draft stays.
  await h.editRaw('noiseSeed', '77');
  const same = { ...next };
  h.view.onState({ state: clone(same), host: { ...hostBase, generation: 3 } });
  assert.equal(h.raw('noiseSeed').value, '77', 'the draft is kept');
  assert.equal(h.status(), 'Raw edits pending');
});

test('page: a new session starts at 0 m with the engine\'s default cells whatever the previous one left, using the remembered surface pressure', async () => {
  const h = await scenarioHarness({ pressure1Mbar: 4600, pressure2Mbar: 4600 });
  assert.ok(Number(h.control('depth').value) > 30);
  near(Number(h.control('oxygen-base').value), 60, 'the previous session\'s cell');
  // The session is closed and a new one booted: the page shows the state of the new engine session, whose inputs the engine reset.
  h.view.hide();
  globalThis.window.localStorage.setItem('ngc-wasm.surface-pressure', '900');
  h.view.show({
    options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: describeRelease(DEFAULT_RELEASE_ID),
    slots: { main: fakeSlot('main.srec'), handset: fakeSlot('handset.srec') },
  });
  const fresh = { ...initialState, inputs: { ...initialInputs, oxygen1Mv: 10, oxygen2Mv: 10, oxygen3Mv: 10, pressure1Mbar: 900, pressure2Mbar: 900 } };
  h.view.onState({ state: clone(fresh), host: { ...hostBase, profileEpoch: 2, generation: 2 } });
  assert.equal(Number(h.control('depth').value), 0, 'the depth slider shows 0 m after the boot');
  assert.equal(Number(h.control('surface-pressure').value), 900, 'the depth is derived from the remembered surface pressure');
  assert.equal(Number(h.control('pressure-offset-1').value), 0, 'and no phantom sensor offset appears');
  assert.equal(Number(h.control('oxygen-base').value), 10);
  for (const index of [1, 2, 3]) assert.equal(Number(h.control(`oxygen-offset-${index}`).value), 0, `cell ${index} offset`);
  assert.equal(h.posts().length, 0, 'showing the new session sends nothing');
});

test('structure: deco.js is a published page module and the engine option list names the start-at-the-surface options', () => {
  const build = fs.readFileSync(path.join(here, '..', 'deploy', 'build_site.py'), 'utf8');
  assert.match(build, /"deco\.js"/, 'deco.js is on the site allowlist on purpose');
  const engineSource = fs.readFileSync(path.join(here, 'engine.js'), 'utf8');
  assert.match(engineSource, /OPTIONAL_OPTIONS = \[[^\]]*'startAtSurface'[^\]]*'surfacePressureMbar'/);
  assert.doesNotMatch(engineSource, /OPTIONAL_OPTIONS = \[[^\]]*'(eepromFactoryInit|decoStorageFixture)'/, 'the removed options are not retried without');
});

// =====================================================================================================
// custom (native) firmware builds, DESIGN 20: the mode choice, the structural report, isolated profiles, the fault report
// =====================================================================================================

const customSrec = (tag) => `S0custom:${tag}`;
/** A chosen or dropped file: a fresh object per use (the worker takes its buffer). */
const customFile = (name, tag) => {
  const bytes = text(customSrec(tag));
  return { name, size: bytes.length, arrayBuffer: async () => bytes.buffer };
};
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
const until = async (predicate) => {
  for (let turn = 0; turn < 200 && !predicate(); turn++) await flush();
  assert.ok(predicate(), 'the condition was not reached');
};
const names = (files) => files.map((file) => file.name).sort();

test('custom: the release is named by the page and has the one shared profile area and a remembered-firmware area of its own', () => {
  assert.deepEqual(describeRelease('CUSTOM', 'Custom build'), { id: 'CUSTOM', name: 'Custom build', label: 'Custom build', custom: true });
  assert.equal(describeRelease(CUSTOM_RELEASE_ID).custom, true);
  assert.equal(describeRelease('TRITON-5.8-65.3').custom, undefined, 'an original release never carries the custom flag');
  assert.ok(!RELEASE_IDS.includes(CUSTOM_RELEASE_ID), 'custom builds are not in the release table');
  assert.equal(releaseOf({ ok: true, release: { id: 'CUSTOM', label: 'Custom build' } }).custom, true);
  assert.equal(profileArea('CUSTOM'), 'custom');
  const areas = [profileArea('CUSTOM'), profileArea('TRITON-5.8-65.3'), profileArea('NEPTUN-5.8-65.3'), firmwareArea(false), firmwareArea(true)];
  assert.equal(new Set(areas).size, 5, 'no two kinds share a storage area');
  assert.ok(areas.every((area) => storageAreas().includes(area)), 'IndexedDB creates a store for each');
});

test('engine: the custom-firmware exports map the role like the original ones, and an older module is told so', () => {
  const calls = [];
  const memory = new ArrayBuffer(1 << 16);
  const bytes = new Uint8Array(memory);
  let top = 4096;
  const encoder = new TextEncoder();
  const exports = {
    memory: { buffer: memory },
    ngc_init() {},
    ngc_engine() { return 0; },
    ngc_version() { return 1; },
    ngc_output_ptr() { return 100; },
    ngc_alloc(length) { const at = top; top += length + 8; return at; },
    ngc_free() {},
    ngc_firmware_inspect_custom(ptr, length) { calls.push(['inspect', length]); const json = encoder.encode('{"ok":true,"custom":true}'); bytes.set(json, 100); return json.length; },
    ngc_set_custom_firmware(role, ptr, length) { calls.push(['set', role, length]); return role === 1 && length === 1 ? 1 : 0; },
    ngc_error() { const message = encoder.encode('initial-sp: too high'); bytes.set(message, 100); return message.length; },
  };
  const engine = new Engine({ exports }, null);
  assert.equal(engine.supportsCustom, true);
  assert.deepEqual(engine.inspectCustomFirmware(new Uint8Array([1, 2, 3])), { ok: true, custom: true });
  engine.setCustomFirmware('main', new Uint8Array([1, 2]));
  assert.throws(() => engine.setCustomFirmware('handset', new Uint8Array([1])), (error) => error instanceof EngineError && /initial-sp: too high/.test(error.message));
  assert.deepEqual(calls.filter((call) => call[0] === 'set').map((call) => call[1]), [0, 1], 'main is role 0, handset role 1');
  const older = new Engine({ exports: { ...exports, ngc_firmware_inspect_custom: undefined, ngc_set_custom_firmware: undefined } }, null);
  assert.equal(older.supportsCustom, false);
  assert.throws(() => older.inspectCustomFirmware(new Uint8Array(1)), /no support for custom firmware/);
  assert.throws(() => older.setCustomFirmware('main', new Uint8Array(1)), /no support for custom firmware/);
});

test('runtime (fake engine): a custom build is checked for the role of its slot, held apart from the original files and booted with the shared custom profile', async () => {
  const engine = new FakeEngine();
  const storage = new MemoryStorage();
  const h = new RuntimeHarness(engine, storage);
  const info = await h.request('init');
  assert.equal(info.customSupported, true);
  assert.equal(info.rememberedCustom, null);
  assert.equal(info.profiles.CUSTOM, null);
  // The slot decides the role (both native builds share a reset vector): this "handset" file serves the main slot.
  const asMain = await h.request('inspect-custom', { role: 'main', name: 'native_handset.srec', bytes: text(customSrec('a')) });
  assert.equal(asMain.accepted, true);
  assert.equal(asMain.role, 'main');
  assert.equal(asMain.release.custom, true);
  assert.equal(asMain.report.role, null, 'the engine report names no role for a custom build');
  assert.equal((await h.request('inspect-custom', { role: 'handset', name: 'h.srec', bytes: text(customSrec('b')) })).accepted, true);
  // A refused file is explained with the report and leaves the slot empty (the earlier file is replaced, not kept).
  const bad = await h.request('inspect-custom', { role: 'handset', name: 'bad.srec', bytes: text(customSrec('x:bad')) });
  assert.equal(bad.accepted, false);
  assert.match(bad.message, /^initial-sp: /);
  assert.equal(bad.report.checks.find((check) => !check.ok).name, 'initial-sp');
  assert.equal(h.runtime.custom.handset, null);
  assert.equal((await h.request('inspect-custom', { role: 'handset', name: 'h.srec', bytes: text(customSrec('b')) })).accepted, true);
  await assert.rejects(() => h.request('inspect-custom', { role: 'both', name: 'x.srec', bytes: text(customSrec('c')) }), /main or the handset slot/);
  assert.equal(h.runtime.firmware.main, null, 'no original file was touched');

  // Booting custom gives the engine the custom images only, and the release is the custom build.
  const booted = await h.request('boot', { options: { mode: 'dual', startPaused: true }, custom: true });
  assert.equal(booted.release.id, 'CUSTOM');
  assert.equal(booted.release.custom, true);
  assert.equal(booted.hostStatus.release.name, 'Custom build');
  assert.deepEqual(engine.created[0].custom, { main: customSrec('a'), handset: customSrec('b') });
  assert.deepEqual(engine.created[0].firmware, {});
  await h.request('flush');
  await h.request('close-session');
  assert.ok(h.runtime.lastSave.wall, 'the custom session saved its profile');
  assert.deepEqual(names(await storage.list('custom')), ['eeprom.bin', 'inputs.json'], 'the custom profile area');
  assert.equal((await storage.list('profile')).length, 0, 'TRITON\'s area stays empty');
  assert.equal((await storage.list(profileArea('NEPTUN-5.8-65.3'))).length, 0);

  // Isolation both ways: markers in each area are seen only by their own kind of session.
  await storage.write('custom', 'led-colors.json', text('{"marker":"custom"}'));
  await storage.write('profile', 'led-colors.json', text('{"marker":"triton"}'));
  await h.inspect('main', 'TRITON-5.8-65.3');
  await h.inspect('handset', 'TRITON-5.8-65.3');
  assert.equal(h.runtime.custom.main.name, 'native_handset.srec', 'inspecting original files leaves the custom ones');
  assert.equal(h.runtime.firmware.main.release.name, 'TRITON');
  const original = await h.request('boot', { options: { mode: 'dual', startPaused: true } });
  assert.equal(original.release.id, 'TRITON-5.8-65.3');
  assert.equal(original.hostStatus.storage.lastSave, null, 'a new session does not inherit the earlier session\'s save time');
  assert.deepEqual(engine.created.at(-1).custom, {}, 'an original boot gets no custom image');
  assert.ok(engine.created.at(-1).firmware.main);
  assert.equal(new TextDecoder().decode(engine.created.at(-1).profile['led-colors.json']), '{"marker":"triton"}');
  await h.request('close-session');
  await h.request('boot', { options: { mode: 'dual', startPaused: true }, custom: true });
  assert.equal(new TextDecoder().decode(engine.created.at(-1).profile['led-colors.json']), '{"marker":"custom"}');
  assert.deepEqual(engine.created.at(-1).firmware, {}, 'a custom boot gets no original image');

  // Reset profile erases the profile of the session's kind only.
  await h.request('reset-profile');
  assert.equal((await storage.list('custom')).length, 0);
  assert.ok((await storage.list('profile')).length > 0, 'TRITON\'s profile is untouched');
  await h.request('close-session');
  await storage.write('custom', 'eeprom.bin', new Uint8Array([1]));
  assert.ok((await h.request('info')).profiles.CUSTOM, 'the entry screen sees the custom profile');
  await h.request('reset-profile', { release: 'CUSTOM' });
  assert.equal((await storage.list('custom')).length, 0, 'without a session the entry screen resets the custom area');
  assert.ok((await storage.list('profile')).length > 0);

  // Clearing a slot of one kind leaves the other kind's.
  await h.request('clear-firmware', { role: 'main', custom: true });
  assert.equal(h.runtime.custom.main, null);
  assert.ok(h.runtime.firmware.main);
  await assert.rejects(() => h.request('boot', { options: { mode: 'dual', startPaused: true }, custom: true }), /main firmware file has not been provided/);
});

test('runtime (fake engine): remembered custom builds live in their own area and never mix with the remembered original pair', async () => {
  const storage = new MemoryStorage();
  storage.kind = 'opfs';
  const h = new RuntimeHarness(new FakeEngine(), storage);
  await h.request('init');
  await h.request('inspect-custom', { role: 'main', name: 'm.srec', bytes: text(customSrec('m')) });
  await h.request('inspect-custom', { role: 'handset', name: 'h.srec', bytes: text(customSrec('h')) });
  await h.request('boot', { options: { mode: 'dual', startPaused: true }, remember: true, custom: true });
  await h.request('close-session');
  assert.deepEqual(names(await storage.list('firmware-custom')), ['handset.srec', 'index.json', 'main.srec']);
  assert.equal((await storage.list('firmware')).length, 0);
  const info = await h.request('info');
  assert.equal(info.remembered, null, 'the original banner has nothing to offer');
  assert.equal(info.rememberedCustom.main.name, 'm.srec');
  assert.equal(info.rememberedCustom.main.release.custom, true);
  // Remembering an original pair afterwards does not replace the custom pair.
  await h.inspect('main', 'TRITON-5.8-65.3');
  await h.inspect('handset', 'TRITON-5.8-65.3');
  await h.request('boot', { options: { mode: 'dual', startPaused: true }, remember: true });
  await h.request('close-session');
  assert.equal((await storage.list('firmware-custom')).length, 3);
  assert.equal((await storage.list('firmware')).length, 3);
  assert.equal((await h.request('info')).remembered.main.release.name, 'TRITON');

  // A later visit verifies the custom files again, for the slot each was stored for, and keeps them apart from the original slots.
  const later = new RuntimeHarness(new FakeEngine(), storage);
  await later.request('init');
  const used = await later.request('use-remembered', { custom: true });
  assert.deepEqual(used.accepted.map((file) => file.role).sort(), ['handset', 'main']);
  assert.deepEqual(used.problems, []);
  assert.equal(later.runtime.custom.main.remembered, true);
  assert.equal(later.runtime.firmware.main, null);
  // A remembered build that no longer passes the structural checks is reported and not used.
  await storage.write('firmware-custom', 'main.srec', text(customSrec('z:bad')));
  const broken = new RuntimeHarness(new FakeEngine(), storage);
  await broken.request('init');
  const refused = await broken.request('use-remembered', { custom: true });
  assert.deepEqual(refused.accepted.map((file) => file.role), ['handset']);
  assert.match(refused.problems[0], /^main: initial-sp: /);
  assert.equal(broken.runtime.custom.main, null);
  // Forget (custom) leaves the original pair.
  await later.request('forget', { custom: true });
  assert.equal((await storage.list('firmware-custom')).length, 0);
  assert.equal((await storage.list('firmware')).length, 3);
});

test('entry (custom mode): the explicit choice swaps the original sources for one card per board, with no URL loading', async () => {
  const m = await mountEntry(); // the default fetch fails the test: nothing may be fetched
  assert.match(html, /<input type="checkbox" id="custom-mode"><span><strong>Use custom firmware builds<\/strong> \(release verification skipped\)/, 'unchecked by default');
  assert.match(html, /<input id="custom-file-main" type="file" accept="\.srec" hidden>/);
  assert.match(html, /<input id="custom-file-handset" type="file" accept="\.srec" hidden>/);
  assert.equal(m.el('custom-mode').checked, false);
  assert.equal(m.el('original-mode').hidden, false);
  assert.equal(m.el('custom-mode-panel').hidden, true);
  m.el('custom-mode').checked = true;
  m.el('custom-mode').dispatch('change');
  assert.equal(m.view.customMode, true);
  assert.equal(m.el('original-mode').hidden, true, 'the normal TRITON / NEPTUN loading is out of the way');
  assert.equal(m.el('custom-mode-panel').hidden, false);
  assert.equal(m.el('lead-custom').hidden, false);
  assert.equal(m.el('lead-original').hidden, true);
  assert.equal(m.el('fact-custom').hidden, false);
  assert.equal(m.el('fact-original').hidden, true);
  assert.match(m.document.getElementById('subtitle').textContent, /^Custom firmware builds/);
  assert.equal(m.el('boot').disabled, true);
  assert.equal(m.el('boot-hint').textContent, 'Provide both custom builds to continue.');
  for (const role of ['main', 'handset']) assert.equal(m.el(`custom-slot-${role}`).querySelector('[data-part="state"]').textContent, 'Waiting for the file. Choose file…');
  // No URL loading in custom mode, even with the pre-filled fields.
  assert.equal(m.el('url-main').value.startsWith('https://'), true);
  await m.view.loadUrls();
  assert.equal(m.calls.length, 0);
  // A cold boot is explained for custom builds, not with the original firmware's calibration hint.
  m.el('start-boot-mode').value = 'cold';
  m.el('start-boot-mode').dispatch('change');
  assert.equal(m.el('start-cold-hint').hidden, true);
  assert.equal(m.el('start-cold-hint-custom').hidden, false);
  assert.match(m.el('start-cold-hint-custom').textContent, /refuses the boot and says why/);
  // Back to the normal loading.
  m.el('custom-mode').checked = false;
  m.el('custom-mode').dispatch('change');
  assert.equal(m.el('original-mode').hidden, false);
  assert.equal(m.el('custom-mode-panel').hidden, true);
  assert.equal(m.document.getElementById('subtitle').textContent, 'TRITON or NEPTUN main 5.8 + handset 65.3 · WebAssembly functional model');
  assert.equal(m.el('start-cold-hint').hidden, false);
});

test('entry (custom mode): the card decides the role; each card shows the structural report; failures stay in view; only .srec files', async () => {
  const m = await mountEntry();
  m.view.setCustomMode(true);
  const card = (role) => m.el(`custom-slot-${role}`);
  const state = (role) => card(role).querySelector('[data-part="state"]').textContent;

  // The file chooser of a card: the input of that board.
  const input = m.el('custom-file-handset');
  input.files = [customFile('app_handset.srec', 'h')];
  input.dispatch('change');
  await until(() => m.view.custom.slots.handset);
  assert.equal(card('handset').classList.contains('ok'), true);
  assert.match(card('handset').querySelector('.slot-title').textContent, /^✓ Handset \(custom build\)$/);
  assert.match(card('handset').querySelector('.slot-file').textContent, /^app_handset\.srec · \d+ bytes$/);
  assert.equal(card('handset').querySelector('button.slot-remove').getAttribute('aria-label'), 'Remove the handset build');
  const details = card('handset').querySelector('details');
  assert.equal(details.open, false, 'the report starts collapsed');
  assert.equal(details.querySelector('summary').textContent, '7 of 7 checks passed · SHA-256 · span · SP · reset PC');
  assert.match(details.textContent, /SREC SHA-256 sha-S0custom:h/);
  assert.match(details.textContent, /Binary SHA-256 bin-S0custom:h/);
  assert.match(details.textContent, /Span 0x08004000–0x08005000 \(4\.0 KiB; the end is exclusive\)/);
  assert.match(details.textContent, /Initial SP 0x20018000/);
  assert.match(details.textContent, /Reset PC 0x08004411/);
  assert.match(details.textContent, /✓ initial-sp: ok/);
  assert.equal(m.view.custom.slots.handset.role, 'handset', 'the slot decided the role (the engine report names none)');
  assert.equal(m.el('boot-hint').textContent, 'Still needed: the main build.');
  // A refresh keeps the opened details.
  details.open = true;
  m.view.refresh();
  assert.equal(card('handset').querySelector('details'), details);
  assert.equal(details.open, true);

  // A drop onto the main card: the card is the target, and drag feedback follows it.
  const over = card('main').dispatch('dragover');
  assert.equal(over.defaultPrevented, true);
  assert.equal(card('main').classList.contains('over'), true);
  card('main').dispatch('dragleave');
  assert.equal(card('main').classList.contains('over'), false);
  const drop = card('main').dispatch('drop', { dataTransfer: { files: [customFile('native_main.srec', 'm')] } });
  assert.equal(drop.defaultPrevented, true);
  await until(() => m.view.custom.slots.main);
  assert.match(card('main').querySelector('.slot-title').textContent, /^✓ Main \(custom build\)$/);
  assert.equal(m.el('custom-status').textContent, '');
  assert.equal(m.el('boot').disabled, false);
  assert.equal(m.el('boot-hint').textContent, 'Both custom builds passed the structural checks.');

  // More than one file on a card: the first is used and the rest are said to be ignored.
  card('main').dispatch('drop', { dataTransfer: { files: [customFile('first.srec', 'm2'), customFile('second.srec', 'm3')] } });
  await until(() => m.view.custom.slots.main && m.view.custom.slots.main.name === 'first.srec');
  assert.equal(m.el('custom-status').textContent, 'One file per board: using first.srec and ignoring 1 other file.');
  // A drop outside the cards does nothing (and says where to drop).
  const loose = m.document.dispatch('drop', { dataTransfer: { files: [customFile('lost.srec', 'l')] } });
  assert.equal(loose.defaultPrevented, true, 'the browser does not open the file');
  assert.match(m.el('custom-status').textContent, /Drop each file onto its own card/);
  assert.equal(m.view.custom.slots.main.name, 'first.srec');

  // Only .srec files: anything else is refused on the card and empties the slot (here and in the worker).
  await m.view.addCustomFiles('main', [customFile('main.bin', 'm')]);
  assert.equal(m.view.custom.slots.main, null);
  assert.equal(m.runtime.runtime.custom.main, null);
  assert.equal(card('main').classList.contains('bad'), true);
  assert.equal(card('main').classList.contains('ok'), false);
  assert.match(card('main').textContent, /main\.bin was not accepted/);
  assert.match(card('main').textContent, /\.srec files only/);
  assert.equal(m.el('boot').disabled, true);
  // A structural failure keeps the engine's message and the failed check in view, outside the closed details.
  await m.view.addCustomFiles('main', [customFile('native.srec', 'q:bad')]);
  assert.equal(card('main').classList.contains('bad'), true);
  assert.match(card('main').textContent, /native\.srec was not accepted/);
  const failed = card('main').querySelector('.check-list.failed');
  assert.equal(card('main').querySelectorAll('p.bad-text').length, 0, 'the failed check says it; the engine\'s message is not repeated');
  assert.match(failed.textContent, /^✗ initial-sp: 0x20019000 is above 0x20018000$/);
  assert.notEqual(failed.parent.tagName, 'DETAILS', 'a failure is never folded away');
  assert.match(card('main').querySelector('details').textContent, /SREC SHA-256 sha-S0custom:q:bad/);
  assert.match(state('main'), /Choose another file…/);
  // Not an S-record at all.
  await m.view.addCustomFiles('main', [{ name: 'notes.srec', size: 5, arrayBuffer: async () => text('hello').buffer }]);
  assert.match(card('main').querySelector('.check-list.failed').textContent, /^✗ srec-syntax: record 1: not an S-record$/);
  // A good file replaces the refusal; Remove returns the card to waiting.
  await m.view.addCustomFiles('main', [customFile('again.srec', 'm4')]);
  assert.equal(card('main').classList.contains('bad'), false);
  assert.equal(card('main').classList.contains('ok'), true);
  card('main').querySelector('button.slot-remove').click();
  await until(() => !m.view.custom.slots.main);
  assert.equal(state('main'), 'Waiting for the file. Choose file…');
  assert.equal(m.runtime.runtime.custom.main, null);
});

test('entry (custom mode): the page boots custom builds; the original files, the profile notes and the remembered pair stay apart', async () => {
  const answers = proxyFor({
    [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3')),
    [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')),
  });
  const booted = [];
  const m = await mountEntry({ storageKind: 'opfs', respond: answers, booted: (result, info) => booted.push({ result, info }) });
  const note = () => m.el('profile-note').textContent;
  const profile = (name) => ({ files: [{ name }], modified: 1 });
  m.view.show({ engine: 'fake', storage: { kind: 'opfs', problems: [] }, remembered: null, rememberedCustom: null, profiles: { 'TRITON-5.8-65.3': profile('eeprom.bin'), CUSTOM: profile('inputs.json') } });
  // The original pair first.
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(m.el('boot').disabled, false);
  assert.match(note(), /A saved TRITON profile \(eeprom\.bin; updated .*\) is restored when you boot TRITON firmware/);
  assert.doesNotMatch(note(), /custom/);

  // Custom mode: its own cards are empty, so booting is not possible yet; the profile note is the shared custom profile.
  m.view.setCustomMode(true);
  assert.equal(m.el('boot').disabled, true, 'the original files do not stand in for custom builds');
  assert.match(note(), /A saved custom-build profile \(inputs\.json; updated .*\) is shared by every custom build and is restored when you boot one\. Reset it before loading a build/);
  assert.doesNotMatch(note(), /TRITON/);
  assert.match(note(), /Reset the saved custom-build profile…/);
  await m.view.addCustomFiles('handset', [customFile('h.srec', 'h')]);
  await m.view.addCustomFiles('main', [customFile('m.srec', 'm')]);
  assert.equal(m.el('boot').disabled, false);
  assert.equal(m.view.activeSlots(), m.view.custom.slots);
  assert.equal(m.view.release().custom, true);
  assert.equal(m.view.slots.main.release.name, 'TRITON', 'the original slots were not touched');

  // Back to the original mode: its files are still there, the custom ones are not offered.
  m.view.setCustomMode(false);
  assert.equal(m.view.activeSlots(), m.view.slots);
  assert.equal(m.el('boot').disabled, false);
  assert.match(m.el('release-line').textContent, /^Release: TRITON-5\.8-65\.3 label\. Both files are from this release\./, 'the original release line, not a custom one');
  assert.match(note(), /TRITON profile/);
  assert.notEqual(m.runtime.runtime.firmware.main, m.runtime.runtime.custom.main);
  await m.view.boot('stored');
  assert.equal(booted.at(-1).info.custom, false);
  assert.equal(booted.at(-1).result.release.id, 'TRITON-5.8-65.3');
  assert.deepEqual(m.engine.created.at(-1).custom, {});
  assert.deepEqual(names(await m.runtime.storage.list('firmware')), ['handset.srec', 'index.json', 'main.srec']);
  assert.equal((await m.runtime.storage.list('firmware-custom')).length, 0, 'booting the original files remembers nothing as custom');
  await m.runtime.request('close-session');

  // Booting custom builds: the worker is asked for a custom boot, the engine gets the custom images only, and remembering
  // (on by default) stores them in the custom area.
  m.view.show(await m.runtime.request('info'));
  m.view.setCustomMode(true);
  await m.view.boot('stored');
  assert.equal(booted.at(-1).info.custom, true);
  assert.equal(booted.at(-1).result.release.custom, true);
  assert.deepEqual(m.engine.created.at(-1).firmware, {});
  assert.deepEqual(m.engine.created.at(-1).custom, { main: customSrec('m'), handset: customSrec('h') });
  assert.deepEqual(names(await m.runtime.storage.list('firmware-custom')), ['handset.srec', 'index.json', 'main.srec']);
  assert.deepEqual(names(await m.runtime.storage.list('firmware')), ['handset.srec', 'index.json', 'main.srec'], 'the original pair is still the original pair');
  assert.match(new TextDecoder().decode(await m.runtime.storage.read('firmware', 'index.json')), /TRITON-5\.8-65\.3/);
  await m.runtime.request('close-session');

  // A later visit: the custom banner offers the custom pair, the original banner the original one.
  const info = await m.runtime.request('info');
  assert.equal(info.rememberedCustom.handset.name, 'h.srec');
  await m.view.removeCustom('main');
  await m.view.removeCustom('handset');
  m.view.setCustomMode(false);
  m.view.show(info);
  assert.equal(m.el('custom-remembered-banner').hidden, false);
  m.view.setCustomMode(true); // the remembered custom pair is taken (verified again) on the way in
  await until(() => m.view.customRememberedInUse());
  assert.equal(m.view.custom.slots.main.remembered, true);
  assert.equal(m.el('custom-use-remembered').hidden, true);
  assert.match(m.el('custom-remembered-text').textContent, /^Using the custom builds remembered from an earlier visit/);
  assert.equal(m.el('remember').checked, true);
  await m.view.removeCustom('main');
  assert.equal(m.el('custom-use-remembered').hidden, false, 'a removed build can be taken again');
  await m.view.useRememberedCustom();
  assert.equal(m.view.customRememberedInUse(), true);
  // Forget removes the custom pair and unticks the shared box; the original pair stays.
  await m.view.forgetCustom();
  assert.equal((await m.runtime.storage.list('firmware-custom')).length, 0);
  assert.equal((await m.runtime.storage.list('firmware')).length, 3);
  assert.equal(m.view.custom.slots.main, null);
  assert.equal(m.el('remember').checked, false);
  assert.equal(m.el('custom-remembered-banner').hidden, true);
});

test('entry (custom mode): the engine\'s refusals and a module without custom support are shown, not hidden', async () => {
  // A cold boot the engine refuses: its message is on the entry screen, the files stay.
  const engine = new SrecFakeEngine();
  const m = await mountEntry({ engine });
  m.view.setCustomMode(true);
  await m.view.addCustomFiles('handset', [customFile('h.srec', 'h')]);
  await m.view.addCustomFiles('main', [customFile('m.srec', 'm')]);
  engine.createSession = () => { throw new EngineError('Cold boot is not available for custom builds: the standby request cannot be detected'); };
  m.el('start-boot-mode').value = 'cold';
  await m.view.boot('stored');
  assert.equal(m.el('profile-problem').hidden, false);
  assert.match(m.el('profile-problem').textContent, /Cold boot is not available for custom builds/);
  assert.equal(m.view.booting, false);
  assert.ok(m.view.custom.slots.main && m.view.custom.slots.handset, 'the builds stay in their cards');

  // An engine module without the custom exports: the choice is disabled and says why.
  const old = new SrecFakeEngine();
  old.supportsCustom = false;
  const o = await mountEntry({ engine: old });
  o.view.show({ engine: 'old', storage: { kind: 'memory', problems: [] }, remembered: null, rememberedCustom: null, customSupported: false, profiles: {} });
  assert.equal(o.el('custom-mode').disabled, true);
  assert.equal(o.el('custom-unsupported').hidden, false);
  o.view.setCustomMode(true);
  assert.equal(o.view.customMode, false, 'custom mode cannot be switched on');
  assert.equal(o.el('custom-mode').checked, false);
});

test('entry (dev aid): ?dev-firmware=custom puts the TRITON file names into the custom slots, through the structural checks only', async () => {
  const m = await mountEntry({ search: '?dev-firmware=custom', respond: (url) => proxyBody(customSrec(url.includes('handset') ? 'h' : 'm')) });
  await until(() => m.view.custom.slots.main && m.view.custom.slots.handset);
  assert.deepEqual(m.calls.map((call) => call.url), ['/dev-firmware/ngc_handset_65.3_TRITON.srec', '/dev-firmware/ngc_main_5.8_TRITON.srec']);
  assert.equal(m.view.customMode, true);
  assert.equal(m.el('custom-mode').checked, true);
  assert.equal(m.view.custom.slots.main.name, 'ngc_main_5.8_TRITON.srec');
  assert.equal(m.runtime.runtime.custom.handset.name, 'ngc_handset_65.3_TRITON.srec');
  assert.equal(m.view.slots.main, null, 'the original slots stay empty');
  assert.equal(m.el('boot').disabled, false);
  // Without the route (no --dev-firmware) the card says so.
  const none = await mountEntry({ search: '?dev-firmware=custom', respond: () => new Response('Not found', { status: 404 }) });
  await until(() => none.view.custom.rejected.main && none.view.custom.rejected.handset);
  assert.match(none.view.custom.rejected.main.message, /Development firmware route: route not enabled/);
});

test('faults: the report of the state, in the engine\'s shape and the tolerated ones', () => {
  const healthy = { cfsr: 0, hfsr: 0, lockup: null };
  const reports = faults.faultReports({ faults: { handset: { cfsr: 0x20000, hfsr: 0x40000000, lockup: 'fault in a fault handler' }, main: healthy } });
  assert.deepEqual(reports.map((report) => report.board), ['main', 'handset'], 'main first');
  assert.equal(reports[0].active, false);
  assert.equal(faults.faultDetail(reports[0]), 'CFSR 0x00000000 · HFSR 0x00000000 · no lockup');
  assert.equal(reports[1].active, true);
  assert.equal(faults.faultDetail(reports[1]), 'CFSR 0x00020000 · HFSR 0x40000000 · locked up: fault in a fault handler');
  assert.deepEqual(faults.faultWarnings({ faults: { main: healthy, handset: healthy } }), [], 'a healthy state has nothing to say');
  assert.deepEqual(faults.faultWarnings({ faults: { main: { cfsr: 0x100, hfsr: 0, lockup: null } } }).map((warning) => warning.text), [
    'Main CPU reported a fault: CFSR 0x00000100, HFSR 0x00000000. Details: Advanced → Execution and model details.',
  ]);
  assert.match(faults.faultWarnings({ faults: { handset: { cfsr: 0, hfsr: 0, lockup: 'x' } } })[0].text, /^Handset CPU locked up \(x\): CFSR 0x00000000/);
  // Tolerated: a list, board names with the ngc- prefix, text numbers, a boolean lockup; anything else is ignored.
  const listed = faults.faultReports({ faults: [{ board: 'ngc-handset', cfsr: '0x10', hfsr: '2', lockup: true }, { board: 'other', cfsr: 1 }] });
  assert.equal(listed.length, 1);
  assert.equal(faults.faultDetail(listed[0]), 'CFSR 0x00000010 · HFSR 0x00000002 · locked up');
  assert.equal(faults.faultDetail(faults.faultReports({ faults: { main: { cfsr: 0, hfsr: 0 } } })[0]), 'CFSR 0x00000000 · HFSR 0x00000000 · lockup not reported');
  for (const state of [null, {}, { faults: null }, { faults: 'none' }, { faults: {} }]) assert.deepEqual(faults.faultReports(state), []);
});

test('page: CPU faults show briefly in the basic view only when nonzero or locked up, and both boards under Advanced', async () => {
  const m = await mount();
  const doc = m.document;
  const show = (faultsValue) => m.view.onState({ state: { ...baseState, inputs: { ...initialInputs }, ...(faultsValue === undefined ? {} : { faults: faultsValue }) }, host: { ...hostBase } });
  const healthy = { cfsr: 0, hfsr: 0, lockup: null };
  show({ main: healthy, handset: healthy });
  assert.equal(doc.getElementById('fault-health').hidden, true, 'nothing in the basic view for a healthy run');
  assert.equal(doc.getElementById('faults-main').textContent, 'CFSR 0x00000000 · HFSR 0x00000000 · no lockup');
  assert.equal(doc.getElementById('faults-handset').textContent, 'CFSR 0x00000000 · HFSR 0x00000000 · no lockup');
  assert.ok(doc.getElementById('model-details').querySelector('#faults-handset'), 'the details are in Execution and model details');
  show({ main: healthy, handset: { cfsr: 0x20000, hfsr: 0x40000000, lockup: 'fault while handling a fault' } });
  assert.equal(doc.getElementById('fault-health').hidden, false);
  assert.equal(doc.getElementById('fault-warning-handset').hidden, false);
  assert.match(doc.getElementById('fault-warning-handset').textContent, /^Handset CPU locked up \(fault while handling a fault\): CFSR 0x00020000, HFSR 0x40000000\./);
  assert.equal(doc.getElementById('fault-warning-main').hidden, true, 'only the board with something to report');
  assert.equal(doc.getElementById('faults-handset').textContent, 'CFSR 0x00020000 · HFSR 0x40000000 · locked up: fault while handling a fault');
  show({ main: { cfsr: 0x100, hfsr: 0, lockup: null }, handset: healthy });
  assert.equal(doc.getElementById('fault-warning-main').hidden, false);
  assert.match(doc.getElementById('fault-warning-main').textContent, /^Main CPU reported a fault: CFSR 0x00000100, HFSR 0x00000000\./);
  assert.equal(doc.getElementById('fault-warning-handset').hidden, true);
  // A handset-only run has no main board; an engine without the report gives no rows and no strip.
  show({ handset: healthy });
  assert.equal(doc.getElementById('fault-health').hidden, true);
  assert.equal(doc.getElementById('faults-main').textContent, '—');
  show(undefined);
  assert.equal(doc.getElementById('faults-handset').textContent, '—');
  assert.equal(doc.getElementById('fault-health').hidden, true);
});

test('page (custom build): titles say Custom build, original-firmware facts are unknown with the engine\'s reason, and the original firmware\'s hints are hidden', async () => {
  const m = await mount();
  const doc = m.document;
  const reason = 'custom build: original firmware addresses do not apply';
  const report = { srecSha256: 'srec-sha', binSha256: 'bin-sha', checks: [], span: { start: 0x08004000, end: 0x08005000 }, initialSp: 0x20018000, resetPc: 0x08004411 };
  const custom = describeRelease('CUSTOM');
  m.view.show({
    options: { mode: 'dual', adcSample: 400, bootMode: 'handset-wake' }, profile: 'stored', release: custom,
    slots: { main: { ...fakeSlot('native_main.srec'), report }, handset: { ...fakeSlot('native_handset.srec'), report } },
  });
  m.view.onState({
    state: {
      ...baseState, inputs: { ...initialInputs }, mainBatteryReady: null, unavailable: { mainBatteryReady: reason, terminalHandlerDetection: reason },
      decoHealth: { tissues: 'unknown', oxygen: 'unknown', details: { tissues: reason, oxygen: reason } },
      eepromFactoryInit: { applied: false, reason: 'custom build: the original record table is absent' },
      firmware: { release: { id: 'CUSTOM', label: 'Custom build' } },
      rtcPersistence: { policy: 'virtual', precision: 'second', mainBkp1WakeOverride: false, sources: {} },
    },
    host: { ...hostBase, release: custom },
  });
  assert.equal(doc.title, 'NGC system emulator · Custom build · WebAssembly');
  assert.match(doc.getElementById('subtitle').textContent, /^Custom build · main \+ handset · live LCD output/);
  assert.doesNotMatch(doc.getElementById('subtitle').textContent, /TRITON|NEPTUN|5\.8|65\.3/);
  assert.equal(doc.getElementById('battery-ready').textContent, 'Unknown');
  assert.match(doc.getElementById('unavailable-info').textContent, /Main batteries ready: unknown \(custom build: original firmware addresses do not apply\)\./);
  assert.match(doc.getElementById('unavailable-info').textContent, /Terminal-handler detection: unknown \(/);
  assert.doesNotMatch(doc.getElementById('unavailable-info').textContent, /not reported for this release/);
  assert.equal(doc.getElementById('deco-health').hidden, true, 'an unknown decompression state never warns');
  assert.equal(doc.getElementById('battery-note').hidden, true, 'the original firmware\'s battery wizard note does not apply');
  assert.equal(doc.getElementById('cold-hint').hidden, true, 'nor does the oxygen calibration a cold boot clears');
  assert.match(doc.getElementById('cold-boot').title, /^Cold boot of a custom build/);
  assert.doesNotMatch(doc.getElementById('firmware-info').textContent, /5\.8|65\.3|null|undefined/);
  assert.match(doc.getElementById('firmware-info').textContent, /Main build native_main\.srec · .*SREC SHA-256 srec-sha · binary SHA-256 bin-sha · Span 0x08004000–0x08005000 .*Initial SP 0x20018000 · Reset PC 0x08004411/);
  assert.match(doc.getElementById('firmware-info').textContent, /Handset build native_handset\.srec/);
  doc.getElementById('firmware-details').open = true;
  m.view.render();
  const info = doc.getElementById('session-info').textContent;
  assert.match(info, /Firmware: Custom build \(CUSTOM\)\. Release verification was skipped/);
  assert.match(info, /Decompression state \(read-only report\): tissues unknown, oxygen unknown\. custom build: original firmware addresses do not apply/);
  assert.match(info, /Fixture: EEPROM factory image \(not applied\)\. custom build: the original record table is absent/);
  assert.match(info, /Handset wake fixture: PWR\.SR1 = 0x104 and RCC\.CSR = 0 only; the original application's RTC\.BKP1R value is not written\./);
  assert.doesNotMatch(info, /BKP1R wake override applied/);
  assert.match(doc.getElementById('profile-status').textContent, /^Custom build profile \(shared by every custom build/);
  // A TRITON session afterwards: its hints and values are back, and nothing of "custom" remains.
  m.view.show({ options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: describeRelease(DEFAULT_RELEASE_ID), slots: { main: fakeSlot('main.srec'), handset: fakeSlot('handset.srec') } });
  m.view.onState({ state: { ...baseState, inputs: { ...initialInputs }, mainBatteryReady: true, unavailable: {}, firmware: { release: { id: 'TRITON-5.8-65.3', label: 'TRITON main 5.8 / handset 65.3' } } }, host: { ...hostBase, release: describeRelease(DEFAULT_RELEASE_ID) } });
  assert.equal(doc.title, 'NGC system emulator · TRITON · WebAssembly');
  assert.equal(doc.getElementById('battery-ready').textContent, 'Yes');
  assert.equal(doc.getElementById('battery-note').hidden, false);
  assert.equal(doc.getElementById('cold-hint').hidden, false);
  assert.match(doc.getElementById('cold-boot').title, /oxygen calibration/);
  assert.doesNotMatch(doc.getElementById('profile-status').textContent, /custom/i);
  // Ending a session restores the page header.
  m.view.hide();
  assert.equal(doc.getElementById('subtitle').textContent, 'TRITON or NEPTUN main 5.8 + handset 65.3 · WebAssembly functional model');
  assert.equal(doc.title, 'NGC system emulator · WebAssembly');
});

test('page: a new session starts without the earlier session\'s UART channels, console text and output rows', async () => {
  const h = await uartHarness();
  h.update(uartChannels(), { hardwareOutputs: [vibratorOrLed(pulses(1))], faults: { handset: { cfsr: 1, hfsr: 0, lockup: null } } });
  h.choose('handset.usart3');
  assert.equal(h.pre.textContent, 'text of handset.usart3\n');
  assert.equal(h.view.outputRows.size, 1);
  assert.equal(h.document.getElementById('fault-health').hidden, false);
  // Another session (here custom builds) is shown before its first state arrives.
  h.view.show({ options: { mode: 'dual', adcSample: 400 }, profile: 'stored', release: describeRelease('CUSTOM'), slots: { main: fakeSlot('m.srec'), handset: fakeSlot('h.srec') } });
  assert.deepEqual(h.optionIds(), [''], 'the channel list is empty again');
  assert.equal(h.select.disabled, true);
  assert.equal(h.pre.textContent, 'No transmitted bytes captured yet.');
  assert.equal(h.status(), 'Waiting for UART status.');
  assert.equal(h.view.uartChannels.size, 0);
  assert.equal(h.view.outputRows.size, 0);
  assert.equal(h.document.getElementById('output-list').children.length, 0);
  assert.equal(h.document.getElementById('fault-health').hidden, true);
  // The new session's own channels then start from their first state (the earlier selection is not carried over).
  h.update(uartChannels());
  assert.equal(h.select.value, 'main.uart4');
  assert.equal(h.pre.textContent, 'text of main.uart4\n');
});

test('structure: faults.js is a published page module, and custom mode is off by default in the markup', () => {
  const build = fs.readFileSync(path.join(here, '..', 'deploy', 'build_site.py'), 'utf8');
  assert.match(build, /"faults\.js"/, 'faults.js is on the site allowlist on purpose');
  assert.doesNotMatch(html, /id="custom-mode"[^>]*checked/);
  for (const id of ['original-mode', 'custom-mode-panel', 'custom-slot-main', 'custom-slot-handset', 'fault-health', 'faults-main', 'faults-handset', 'cold-boot']) assert.match(html, new RegExp(`id="${id}"`), id);
  assert.match(html, /<div id="custom-mode-panel" hidden>/);
  // The URL form is part of the original mode only: custom builds are not loaded from addresses.
  const original = html.slice(html.indexOf('<div id="original-mode">'), html.indexOf('<div id="custom-mode-panel"'));
  assert.ok(original.includes('id="url-form"'));
  assert.ok(!html.slice(html.indexOf('<div id="custom-mode-panel"'), html.indexOf('<details id="start-options"')).includes('url-'));
});

// =====================================================================================================
// the dive game (DESIGN 21): game-logic.js, game-gas.js and game.js on the fake DOM
// =====================================================================================================

const closeTo = (actual, expected, tolerance, message = '') => assert.ok(Math.abs(actual - expected) <= tolerance, `${message}: expected ${expected} within ${tolerance}, received ${actual}`);

/** A settings store with the interface of dom.js `prefs`. */
function memoryStore() {
  const map = new Map();
  return { map, get: (key, fallback) => (map.has(key) ? map.get(key) : fallback), set: (key, value) => map.set(key, String(value)), remove: (key) => map.delete(key) };
}

test('game (entry): Start game sits beside Boot with the same files and options, and is off with a reason for a handset-only run', async () => {
  const answers = proxyFor({
    [MAIN_FETCHED]: () => proxyBody(srec('main', 'TRITON-5.8-65.3')),
    [HANDSET_FETCHED]: () => proxyBody(srec('handset', 'TRITON-5.8-65.3')),
  });
  const booted = [];
  const started = [];
  const m = await mountEntry({ respond: answers, booted: (result, info) => booted.push({ result, info }) });
  m.view.hooks.starting = (info) => started.push(info);
  const start = m.el('start-game');
  assert.equal(start.parent, m.el('boot').parent, 'in the row of the Boot button');
  assert.equal(start.disabled, true, 'no files yet');
  m.type('main', MAIN_EXAMPLE);
  m.type('handset', HANDSET_EXAMPLE);
  await m.view.loadUrls();
  assert.equal(start.disabled, false);
  assert.equal(m.el('boot').disabled, false);
  assert.equal(m.el('game-hint').hidden, true);

  // "Handset only" has no main board to feed: Start game is off and says why, Boot stays available.
  m.el('start-mode').value = 'handset';
  m.el('start-mode').dispatch('change');
  assert.equal(start.disabled, true);
  assert.equal(m.el('boot').disabled, false);
  assert.equal(m.el('game-hint').hidden, false);
  assert.match(m.el('game-hint').textContent, /needs both boards.*Main 5\.8 \+ handset 65\.3 over CAN/);
  await m.view.boot('stored', { game: true });
  assert.equal(booted.length, 0, 'a game start is refused without the main board');
  m.el('start-mode').value = 'dual';
  m.el('start-mode').dispatch('change');
  assert.equal(start.disabled, false);
  assert.equal(m.el('game-hint').hidden, true);

  // The same start options and verified files as Boot; the page is told it was a game.
  m.el('start-boot-mode').value = 'cold';
  m.el('start-boot-mode').dispatch('change');
  start.click();
  await until(() => booted.length === 1);
  assert.deepEqual(started, [{ game: true }]);
  assert.equal(booted[0].info.game, true);
  assert.equal(booted[0].info.options.bootMode, 'cold');
  assert.equal(booted[0].info.profile, 'stored');
  assert.equal(booted[0].result.release.id, 'TRITON-5.8-65.3');
  assert.equal(m.engine.created.at(-1).config.mode, 'dual');
  await m.runtime.request('close-session');
  await m.view.boot('stored');
  assert.equal(booted[1].info.game, false, 'Boot emulator is the emulator view');
  assert.deepEqual(started.at(-1), { game: false });
  assert.equal(booted[1].result.release.id, booted[0].result.release.id);
  await m.runtime.request('close-session');
});

test('game (cells): each cell reads 12 mV in air at the surface plus its own deviation of up to 1.0 mV, kept per profile area', () => {
  assert.deepEqual(game.drawCellDeviations(() => 0), [-1, -1, -1]);
  assert.deepEqual(game.drawCellDeviations(() => 0.5), [0, 0, 0]);
  assert.ok(game.drawCellDeviations(() => 0.999999).every((value) => value > 0.99 && value <= 1));
  const draws = Array.from({ length: 2000 }, () => game.drawCellDeviations()).flat();
  assert.ok(draws.every((value) => value >= -1 && value <= 1), 'uniform within +-1.0 mV');
  assert.ok(Math.min(...draws) < -0.95 && Math.max(...draws) > 0.95 && new Set(draws).size > 1000, 'and spread over the whole range');

  const store = memoryStore();
  let calls = 0;
  const random = () => (((calls++) * 0.37) % 1);
  const first = game.loadCellFixture(store, 'profile', random);
  assert.equal(first.drawn, true);
  assert.equal(first.deviationsMv.length, 3);
  const used = calls;
  const again = game.loadCellFixture(store, 'profile', () => assert.fail('stored deviations are reused, not drawn'));
  assert.deepEqual(again.deviationsMv, first.deviationsMv, 'the same cells in the next session');
  assert.equal(again.drawn, false);
  assert.equal(calls, used);
  assert.deepEqual(JSON.parse(store.map.get('game-cells.profile')), { version: 1, deviationsMv: first.deviationsMv }, 'kept in the page settings, keyed by profile area');
  const other = game.loadCellFixture(store, 'profile-neptun-5_8-65_3', random);
  assert.equal(other.drawn, true, 'another profile area has cells of its own');
  assert.ok(store.map.has('game-cells.profile-neptun-5_8-65_3'));
  assert.deepEqual(game.loadCellFixture(store, 'profile', () => assert.fail('untouched by the other area')).deviationsMv, first.deviationsMv);

  // A damaged or out-of-range entry is replaced by a new draw; a profile reset forgets the area's cells.
  for (const damaged of ['not json', '{"version":1,"deviationsMv":[5,0,0]}', '{"version":2,"deviationsMv":[0,0,0]}', '{"version":1,"deviationsMv":[0,0]}']) {
    store.map.set('game-cells.profile', damaged);
    assert.equal(game.loadCellFixture(store, 'profile', () => 0.75).drawn, true, damaged);
  }
  game.clearCellFixture(store, 'profile');
  assert.equal(store.map.has('game-cells.profile'), false);
  assert.equal(game.loadCellFixture(store, 'profile', () => 0.25).drawn, true);
  assert.deepEqual(game.loadCellFixture(store, 'profile', () => 0).deviationsMv, [-0.5, -0.5, -0.5], 'the new draw is the one that is kept');

  // Air at the surface reads 12 mV plus the deviation; the voltage is linear in ppO2 and stays inside the engine's input range.
  const sensitivities = game.cellSensitivities([-1, 0, 1]);
  const air = game.cellMillivolts(game.AIR_SURFACE_PPO2, sensitivities);
  [11, 12, 13].forEach((expected, index) => near(air[index], expected, `cell ${index + 1} in air`));
  assert.deepEqual(game.cellMillivolts(0, sensitivities), [0, 0, 0]);
  near(game.cellMillivolts(2 * game.AIR_SURFACE_PPO2, sensitivities)[1], 24, 'twice the ppO2, twice the voltage');
  assert.deepEqual(game.cellMillivolts(100, sensitivities), [250, 250, 250], 'a cell saturates at the engine\'s 250 mV limit instead of being refused');
  near(game.AIR_SURFACE_PPO2, 0.21 * 1.01325, 'air at the surface');
});

test('game (cells): a profile reset, from the start screen or from the emulator, draws new cells', async () => {
  const key = (area) => `ngc-wasm.game-cells.${area}`;
  const entry = await mountEntry();
  for (const [id, area] of [['TRITON-5.8-65.3', 'profile'], ['NEPTUN-5.8-65.3', 'profile-neptun-5_8-65_3'], ['CUSTOM', 'custom']]) {
    globalThis.window.localStorage.setItem(key(area), '{"version":1,"deviationsMv":[0.1,0.2,0.3]}');
    globalThis.window.localStorage.setItem(key('other'), 'kept');
    assert.equal(await entry.view.resetProfile(id), true);
    assert.equal(globalThis.window.localStorage.getItem(key(area)), null, `${id} resets ${area}`);
    assert.equal(globalThis.window.localStorage.getItem(key('other')), 'kept', 'nothing else is touched');
  }
  // The emulator's own Reset profile (Advanced) is the same reset.
  const m = await mount();
  globalThis.window.localStorage.setItem(key('profile'), '{"version":1,"deviationsMv":[0.1,0.2,0.3]}');
  const done = m.view.resetProfile();
  await settle();
  const request = m.requests.find((item) => item.type === 'reset-profile');
  assert.ok(request, 'the reset was requested');
  assert.notEqual(globalThis.window.localStorage.getItem(key('profile')), null, 'nothing is forgotten before the engine did it');
  request.resolve({});
  await done;
  assert.equal(globalThis.window.localStorage.getItem(key('profile')), null);
  // A reset that fails keeps the cells (the profile is still the old one).
  globalThis.window.localStorage.setItem(key('profile'), '{"version":1,"deviationsMv":[0.1,0.2,0.3]}');
  const failed = m.view.resetProfile();
  await settle();
  m.requests.filter((item) => item.type === 'reset-profile').at(-1).reject(new Error('refused'));
  await failed;
  assert.notEqual(globalThis.window.localStorage.getItem(key('profile')), null);
});

test('game (sensors): both pressure inputs get the surface pressure plus EN13319 water, the cells their voltage, and nothing else changes', () => {
  const sensitivities = game.cellSensitivities([0, 0, 0]);
  const at = (depthM, surfaceMbar = 1013.25, ppo2 = game.AIR_SURFACE_PPO2) => game.gameInputs({ surfaceMbar, depthM, ppo2, sensitivities });
  const surface = at(0);
  assert.deepEqual(Object.keys(surface).sort(), [...sensors.PRESSURE_KEYS, 'oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv'].sort(), 'no temperature, no battery, nothing else');
  assert.equal(surface.pressure1Mbar, 1013.25);
  assert.equal(surface.pressure2Mbar, 1013.25);
  assert.deepEqual([surface.oxygen1Mv, surface.oxygen2Mv, surface.oxygen3Mv], [12, 12, 12], 'air at the surface');
  const deep = at(20);
  closeTo(deep.pressure1Mbar, 1013.25 + (1020 * 9.80665 * 20) / 100, 0.006, 'the surface pressure plus water (EN13319: 1020 kg/m3, 9.80665 m/s2), to 0.01 mbar');
  closeTo(deep.pressure1Mbar, 3013.8, 0.1, 'about 3.014 bar at 20 m');
  assert.equal(deep.pressure1Mbar, deep.pressure2Mbar, 'both sensors get the same value');
  assert.deepEqual(Object.keys(deep).filter((name) => name.startsWith('pressure')), [...sensors.PRESSURE_KEYS], 'through the page\'s sensor numbering');
  closeTo(at(10, 950).pressure2Mbar, 950 + (1020 * 9.80665 * 10) / 100, 0.006, 'the session\'s surface pressure, not a fixed one');
  // The same conversion as the basic view's depth control.
  closeTo(at(37.5).pressure1Mbar, sensors.calculate({ ...sensors.defaults(), depthM: 37.5 }).pressure1Mbar, 0.006, 'sensors.js is the one conversion');
  // The ends of the range stay inside what the engine accepts, and a depth outside it is refused, never clipped.
  for (const depth of [0, MAX_DEPTH_METERS]) {
    const inputs = at(depth, 1013.25, 20);
    assert.ok(inputs.pressure1Mbar >= sensors.RAW_LIMITS.pressure[0] && inputs.pressure1Mbar <= sensors.RAW_LIMITS.pressure[1]);
    assert.ok(inputs.oxygen1Mv <= sensors.RAW_LIMITS.oxygen[1], 'a very high ppO2 saturates the cell');
  }
  assert.throws(() => at(MAX_DEPTH_METERS + 1), /depthM must be between 0 and 110/);
  assert.throws(() => at(-1), /depthM/);
});

test('game (sensors): inputs are coalesced, serialized and sent a few times per second at most, newest first', () => {
  const clock = fakeTimers();
  let wall = 0;
  const sent = [];
  const sender = new game.InputsSender({ send: (inputs) => sent.push(inputs), now: () => wall, setTimer: clock.setTimer, clearTimer: clock.clearTimer });
  sender.offer({ n: 1 });
  assert.deepEqual(sent, [{ n: 1 }], 'the first goes at once');
  wall = 50;
  sender.offer({ n: 2 });
  sender.offer({ n: 3 });
  assert.equal(sent.length, 1, 'inside the interval nothing more is sent');
  assert.equal(clock.timers.size, 1, 'one wait, however many offers');
  wall = 250;
  clock.advance(200);
  assert.deepEqual(sent, [{ n: 1 }, { n: 3 }], 'the newest wins; the one in between is never sent');
  sender.offer({ n: 3 });
  assert.equal(sent.length, 2, 'unchanged values are not sent again');
  wall = 300;
  sender.offer({ n: 4 });
  sender.offer({ n: 3 });
  assert.equal(clock.timers.size, 0, 'a newer offer equal to what the engine has supersedes the one that was waiting');
  assert.equal(sent.length, 2);
  sender.invalidate();
  wall = 600;
  sender.offer({ n: 3 });
  assert.equal(sent.length, 3, 'after a board creation or a failure the same values go out again');
  wall = 610;
  sender.offer({ n: 5 }, { immediate: true });
  assert.equal(sent.at(-1).n, 5, 'a new session does not wait');

  // A busy second: many changes, at most four sends per wall second (plus the first).
  const busy = [];
  const fast = new game.InputsSender({ send: (inputs) => busy.push(inputs), now: () => wall, setTimer: clock.setTimer, clearTimer: clock.clearTimer });
  wall = 10_000;
  for (let step = 1; step <= 100; step++) {
    wall += 10;
    clock.advance(10);
    fast.offer({ step });
  }
  assert.ok(busy.length >= 3 && busy.length <= 5, `${busy.length} sends in one second`);
  wall += 250;
  clock.advance(250);
  assert.equal(busy.at(-1).step, 100, 'the newest values always arrive');
  sender.cancel();
  fast.cancel();
});

test('game (clock): Pause / 1x / 2x / 4x / Uncapped, with the prototype\'s valve override: 1x while held, the chosen speed comes back', () => {
  const changes = [];
  const clock = new game.PlayClock({ onChange: (change) => changes.push({ ...change, effective: clock.effective() }) });
  assert.equal(clock.effective(), 1);
  for (const speed of [2, 4, 'uncapped']) {
    clock.setSpeed(speed);
    assert.equal(clock.effective(), speed);
  }
  // Holding a valve forces 1x over Uncapped, from any source; the chosen speed is kept.
  assert.equal(clock.hold('oxygen', 'pointer:1', true), true);
  assert.equal(clock.effective(), 1);
  assert.equal(clock.speed, 'uncapped');
  assert.equal(clock.returnLabel(), 'Uncapped');
  assert.equal(clock.hold('diluent', 'shortcut', true), false, 'a second valve does not change the injection state');
  assert.equal(clock.hold('oxygen', 'pointer:1', false), false, 'one source letting go keeps the override while another holds');
  assert.equal(clock.effective(), 1);
  assert.equal(clock.hold('diluent', 'shortcut', false), true);
  assert.equal(clock.effective(), 'uncapped', 'restored after the last source lets go');
  // Choosing another speed while injecting changes the speed that comes back.
  clock.hold('oxygen', 'focused-key:Space', true);
  clock.setSpeed(4);
  assert.equal(clock.effective(), 1, 'still at 1x while held');
  clock.hold('oxygen', 'focused-key:Space', false);
  assert.equal(clock.effective(), 4);
  // Pause releases the valves and stays paused; a paused clock cannot hold a valve; resuming uses the speed before the pause.
  clock.hold('diluent', 'pointer:2', true);
  clock.setSpeed(0);
  assert.equal(clock.injecting(), false, 'the pause released the valve');
  assert.equal(clock.effective(), 0);
  assert.equal(clock.hold('oxygen', 'shortcut', true), false);
  assert.equal(clock.injecting(), false);
  clock.togglePause();
  assert.equal(clock.effective(), 4, 'Space resumes at the speed before the pause');
  clock.togglePause();
  assert.equal(clock.effective(), 0);
  // Reset dive: 1x, nothing held.
  clock.setSpeed(2);
  clock.hold('oxygen', 'shortcut', true);
  clock.reset();
  assert.equal(clock.speed, 1);
  assert.equal(clock.injecting(), false);
  assert.throws(() => clock.setSpeed(3), /Unknown play speed/);
  assert.throws(() => clock.hold('helium', 'x', true), /Unknown valve/);
  assert.ok(changes.every((change) => typeof change.injectingChanged === 'boolean'));
});

test('game (dive): depth and gas advance over the emulator\'s virtual time, the same whether a state brings one step or many', () => {
  const run = (times, setup = () => {}) => {
    const sim = new game.GameSim();
    sim.rebase(10);
    setup(sim);
    for (const virtual of times) sim.advanceTo(virtual, {});
    return sim;
  };
  const descend = (sim) => sim.setMotionRate(30);
  const coarse = run([70], descend);
  const fine = run(Array.from({ length: 600 }, (_, index) => 10 + (index + 1) / 10), descend);
  near(coarse.depth, 30, 'a minute at 30 m/min');
  near(fine.depth, coarse.depth, 'the same in 0.1 s states');
  near(coarse.elapsed, 60, 'the dive time is the virtual time integrated');
  near(getLoopReadings(fine.loop).fractions.o2, getLoopReadings(coarse.loop).fractions.o2, 'the same gas');
  assert.equal(coarse.advanceTo(70, {}), 0, 'a time that stands still integrates nothing (a paused emulator)');
  assert.equal(coarse.advanceTo(65, {}), 0, 'nor one that went back (a settle went slightly past the next state)');
  near(coarse.depth, 30);
  assert.equal(coarse.advanceTo(NaN, {}), 0);

  // The first state only sets where the integration starts.
  const late = new game.GameSim();
  late.setMotionRate(30);
  late.advanceTo(500, {});
  assert.equal(late.depth, 0);
  assert.equal(late.virtual, 500);
  late.rebase(10); // the boards were recreated: the virtual time started over
  late.advanceTo(40, {});
  near(late.depth, 15, 'half a minute at 30 m/min from the new start');

  // The depth boundaries stop the motion; time keeps counting.
  let hits = 0;
  const deep = new game.GameSim({ onBoundary: () => { hits += 1; } });
  deep.rebase(0);
  deep.setMotionRate(30);
  deep.advanceTo(300, {});
  assert.equal(deep.depth, MAX_DEPTH_METERS);
  assert.equal(deep.maxDepth, MAX_DEPTH_METERS);
  assert.equal(deep.direction, 0);
  assert.equal(hits, 1);
  near(deep.elapsed, 300);
  deep.setMotionRate(-18);
  deep.advanceTo(1000, {});
  assert.equal(deep.depth, 0, 'the surface');
  assert.equal(hits, 2);
  assert.equal(deep.maxDepth, MAX_DEPTH_METERS, 'the maximum stays');

  // A held oxygen valve (100 SL/min, the default, = 5/3 SL/s into the 4 L loop) follows the analytic mixing; the diluent selection refills the loop.
  const injected = new game.GameSim();
  injected.rebase(0);
  injected.advanceTo(10, { oxygen: true });
  near(getLoopReadings(injected.loop).fractions.o2, 1 - 0.79 * Math.exp(-(100 / 60) * 10 / 4), 'ten seconds of oxygen');
  injected.setGas('tx1845');
  near(getLoopReadings(injected.loop).fractions.he, 0.45);
  assert.equal(injected.gas.name, 'Trimix 18/45');
  assert.throws(() => injected.setGas('nitrox'), /Unknown diluent/);
  injected.setMotionRate(30);
  injected.advanceTo(70, {});
  injected.reset(70);
  assert.equal(injected.depth, 0);
  assert.equal(injected.maxDepth, 0);
  assert.equal(injected.gas.name, 'Air');
  assert.equal(injected.virtual, 70, 'a reset keeps the virtual time');
  near(getLoopReadings(injected.loop).fractions.o2, 0.21);
  assert.deepEqual(injected.profile, [[0, 0]]);

  // The profile keeps the whole time span while long dives thin their samples.
  const long = new game.GameSim();
  long.rebase(0);
  long.advanceTo(20_000, {});
  assert.ok(long.profile.length <= 2401, `${long.profile.length} samples`);
  assert.ok(long.profile.at(-1)[0] > 19_000, 'up to the end of the dive');
});

test('game (clock): the virtual time at "now" is estimated only for a paced run, and never far ahead', () => {
  const base = { lastVirtual: 100, lastWallMs: 1000 };
  near(game.estimateVirtual({ ...base, nowMs: 1100, pace: 1 }), 100.1);
  near(game.estimateVirtual({ ...base, nowMs: 1100, pace: 4 }), 100.35, 'capped at 0.35 virtual s');
  assert.equal(game.estimateVirtual({ ...base, nowMs: 1100, pace: 0 }), 100, 'paused');
  assert.equal(game.estimateVirtual({ ...base, nowMs: 1100, pace: null }), 100, 'unpaced: wait for the state');
  assert.equal(game.estimateVirtual({ lastVirtual: null, lastWallMs: 0, nowMs: 5, pace: 1 }), null);
});

test('game (state): the run state and the messages that must not hide an engine stop', () => {
  const running = { running: true, virtualTime: 5 };
  assert.deepEqual(game.runState(running, { speed: 1, keepingUp: true }), { key: 'running', text: 'Running', tone: 'ok' });
  assert.equal(game.runState({ running: false }, {}).text, 'Paused');
  assert.equal(game.runState({ running: false, standby: true }, {}).text, 'Standby');
  assert.equal(game.runState({ running: false, error: 'x' }, {}).text, 'Stopped by an error');
  assert.equal(game.runState(running, { suspended: true }).key, 'suspended');
  assert.equal(game.runState(running, { speed: 1, keepingUp: false }).key, 'behind');
  assert.equal(game.runState(running, { speed: null, keepingUp: true }).key, 'running', 'unpaced runs cannot fall behind');
  assert.equal(game.runState(running, {}, { connectionError: true }).text, 'Disconnected');
  assert.equal(game.runState(null, null).key, 'starting');

  assert.deepEqual(game.stopAlerts(running), []);
  assert.deepEqual(game.stopAlerts(null), []);
  const standby = game.stopAlerts({ standby: true, running: false });
  assert.equal(standby.length, 1);
  assert.equal(standby[0].action, 'wake');
  assert.match(standby[0].text, /requested standby/);
  const error = game.stopAlerts({ error: 'Terminal handler reached at 0x1', running: false });
  assert.equal(error[0].action, 'resume');
  assert.match(error[0].text, /Terminal handler reached/);
  const fault = game.stopAlerts({ running: true, faults: { main: { cfsr: 0x8000, hfsr: 0, lockup: null }, handset: { cfsr: 0, hfsr: 0, lockup: null } } });
  assert.equal(fault.length, 1);
  assert.match(fault[0].text, /^Main CPU fault: CFSR 0x00008000/);
  assert.match(game.stopAlerts({ running: true, decoHealth: { tissues: 'invalid', oxygen: 'ok' } })[0].text, /stored tissues are blank.*reset the saved profile on the start screen/);
  assert.deepEqual(game.stopAlerts({ running: true, decoHealth: { tissues: 'unknown', oxygen: 'unknown' } }), []);
});

/** The real GameView on the real index.html (fake DOM), a fake worker client and fake timers; the cells are fixed (+0.5 mV each). */
async function mountGame({ options = {}, release = describeRelease(DEFAULT_RELEASE_ID), store = memoryStore() } = {}) {
  installDom(html);
  const { GameView } = await import('./game.js');
  const clock = fakeTimers();
  let wall = 0;
  const sent = [];
  const requests = [];
  const client = {
    send: (type, payload) => sent.push({ type, payload }),
    request: (type, payload) => { requests.push({ type, payload }); return Promise.resolve({}); },
  };
  const quits = [];
  const view = new GameView(client, { quit: () => { quits.push(true); } }, { timers: clock, now: () => wall, random: () => 0.75, store });
  view.show({ options: { mode: 'dual', adcSample: 400, ...options }, profile: 'stored', release });
  const document = globalThis.document;
  const host = { generation: 1, profileEpoch: 1, speed: 1, keepingUp: true };
  const baseState = { running: true, virtualTime: 0, frameReady: false, hardwareOutputs: [], uartConsole: [], outputHistoryEpoch: 'domain-a' };
  const feed = (virtualTime, wallMs = wall, extra = {}, hostExtra = {}) => {
    wall = wallMs;
    view.onState({ state: { ...baseState, virtualTime, ...extra }, host: { ...host, ...hostExtra } });
  };
  return {
    view, clock, sent, requests, quits, document, store, feed, host, baseState,
    el: (id) => document.getElementById(`game-${id}`),
    text: (id) => document.getElementById(`game-${id}`).textContent,
    actions: () => requests.filter((item) => item.type === 'action').map((item) => item.payload.request),
    inputs: () => requests.filter((item) => item.type === 'action' && item.payload.request.action === 'inputs').map((item) => item.payload.request.inputs),
    speeds: () => requests.filter((item) => item.type === 'speed').map((item) => item.payload.speed),
    time: () => wall,
  };
}

test('game (view): a session starts with the emulator\'s pacing and the surface inputs, and depth becomes pressure over virtual time', async () => {
  const g = await mountGame();
  assert.deepEqual(g.sent.at(0), { type: 'ui', payload: { uartOpen: false } }, 'the worker is told the console is not on screen');
  assert.equal(g.document.getElementById('screen-game').hidden, false);
  g.feed(0.25, 0);
  await settle();
  assert.deepEqual(g.speeds(), [1], 'the first state sets the pacing');
  assert.equal(g.actions().some((request) => request.action === 'pause' || request.action === 'resume'), false, 'a running engine is not resumed');
  const first = g.inputs();
  assert.equal(first.length, 1);
  assert.equal(first[0].pressure1Mbar, 1013.25);
  assert.equal(first[0].pressure2Mbar, 1013.25);
  assert.deepEqual([first[0].oxygen1Mv, first[0].oxygen2Mv, first[0].oxygen3Mv], [12.5, 12.5, 12.5], 'air at the surface plus the fixed +0.5 mV of the test');
  assert.equal(g.text('virtual-time'), '00:00:00');
  assert.equal(g.text('release'), 'TRITON main 5.8 / handset 65.3');
  assert.equal(g.text('run-state'), 'Running');
  assert.match(g.text('fixture-list'), /Cell 1: 12\.50 mV in air at the surface \(\+0\.50 mV\)/);

  // A descent at 30 m/min: ten virtual seconds are five meters, whatever the wall clock does.
  g.view.changeMotion(30);
  assert.equal(g.text('motion-status'), 'Descending · 30.0 m/min');
  for (let step = 1; step <= 10; step++) g.feed(0.25 + step, step * 300);
  near(g.view.sim.depth, 5, 'depth follows the emulator\'s virtual time');
  await settle();
  const last = g.inputs().at(-1);
  closeTo(last.pressure1Mbar, 1013.25 + (1020 * 9.80665 * g.view.sim.depth) / 100, 0.006, 'the pressure follows the depth');
  assert.equal(last.pressure1Mbar, last.pressure2Mbar);
  assert.ok(g.inputs().length >= 2 && g.inputs().length <= 11, `coalesced by the queue, never more than one per state (${g.inputs().length})`);
  assert.equal(g.text('depth-value'), '5.0');
  assert.equal(g.text('max-depth-meta'), 'Max 5.0 m');
  assert.equal(g.text('virtual-time'), '00:00:10', 'the header clock is the emulator\'s virtual time');
  assert.match(g.text('profile-duration'), /^0:10 elapsed$/);
  closeTo(Number(g.text('cell-1')), last.oxygen1Mv, 0.006, 'the voltages shown are the ones sent');
  assert.ok(last.oxygen1Mv > 12.5 * 1.4, 'more ambient pressure, more voltage');
  assert.equal(g.text('loop-ppo2'), (0.21 * g.view.sim.readings(1013.25).ambientBar).toFixed(2), 'the true loop ppO2 is shown as the simulated truth');

  // Release holds the depth; a paused emulator (the same virtual time again) changes nothing.
  g.view.stopMotion();
  g.feed(11.25, 3500);
  g.feed(11.25, 3800);
  near(g.view.sim.depth, 5);
  assert.equal(g.view.sim.direction, 0);
  g.view.chooseSpeed(0);
  g.feed(11.25, 4100, { running: false });
  assert.equal(g.text('run-state'), 'Paused');
  assert.equal(g.text('motion-status'), 'Paused');
  g.view.hide();
});

test('game (view): pacing follows the speed menu, Uncapped is unpaced, a held valve runs at 1x and the chosen speed comes back', async () => {
  const g = await mountGame();
  g.feed(0, 0);
  await settle();
  const menu = (speed) => g.el('speed-menu').querySelectorAll('[data-speed]').find((button) => button.dataset.speed === String(speed));
  menu(4).click();
  assert.deepEqual(g.speeds(), [1, 4]);
  assert.equal(g.text('header-speed'), '4×');
  assert.equal(g.text('play-speed-label'), '4×');
  menu('uncapped').click();
  assert.equal(g.speeds().at(-1), null, 'Uncapped is the unpaced engine');
  assert.equal(g.text('play-speed-label'), '∞');
  menu(2).click();
  assert.equal(g.speeds().at(-1), 2);

  // A valve held: the whole clock runs at 1x (the engine is paced at 1x), the status says what comes back; the injection counts only
  // once the worker confirmed the pacing, so no earlier virtual time is injected.
  g.view.holdValve('oxygen', 'pointer:1', true);
  assert.equal(g.speeds().at(-1), 1);
  assert.equal(g.text('header-speed'), '1×');
  assert.equal(g.text('mav-status'), 'Injecting at 1× · returns to 2×');
  assert.equal(g.el('mav-oxygen').classList.contains('active'), true);
  assert.deepEqual(g.view.valveFlags(), { oxygen: false, diluent: false }, 'not yet: the 1x pacing is not confirmed');
  g.feed(5, 1000);
  assert.equal(g.view.sim.loop.totals.oxygen, 0, 'virtual time before the confirmation is not injected');
  await settle();
  assert.deepEqual(g.view.valveFlags(), { oxygen: true, diluent: false });
  g.feed(10, 2000);
  near(g.view.sim.loop.totals.oxygen, 500 / 60, 'five virtual seconds at 100 SL/min');
  // Choosing another speed while injecting changes the speed that comes back.
  g.view.chooseSpeed(4);
  assert.equal(g.text('mav-status'), 'Injecting at 1× · returns to 4×');
  assert.equal(g.speeds().at(-1), 1, 'still at 1x');
  g.view.holdValve('oxygen', 'pointer:1', false);
  assert.equal(g.speeds().at(-1), 4);
  assert.equal(g.text('header-speed'), '4×');
  assert.equal(g.text('mav-status'), '100 surface L/min · 1× while held');
  assert.deepEqual(g.view.valveFlags(), { oxygen: false, diluent: false });

  // Over Uncapped as well, from the keyboard shortcut and a focused key, which are separate sources.
  menu('uncapped').click();
  g.view.holdValve('diluent', 'shortcut', true);
  assert.equal(g.speeds().at(-1), 1);
  g.view.holdValve('oxygen', 'focused-key:Space', true);
  g.view.holdValve('diluent', 'shortcut', false);
  assert.equal(g.speeds().at(-1), 1, 'one source still holds');
  g.view.holdValve('oxygen', 'focused-key:Space', false);
  assert.equal(g.speeds().at(-1), null, 'unpaced again');

  // Pause: the engine's pause action, valves released, nothing can be held; Space resumes at the speed before.
  g.view.holdValve('oxygen', 'pointer:2', true);
  menu(0).click();
  await settle();
  assert.equal(g.actions().at(-1).action, 'pause');
  assert.equal(g.view.clock.injecting(), false);
  assert.equal(g.el('mav-oxygen').disabled, true);
  g.view.holdValve('oxygen', 'pointer:3', true);
  assert.equal(g.view.clock.injecting(), false, 'a paused clock holds no valve');
  g.view.togglePause();
  await settle();
  assert.equal(g.speeds().at(-1), null, 'back to the speed before the pause (Uncapped)');
  assert.equal(g.actions().at(-1).action, 'resume');
  g.view.hide();
});

test('game (view): Reset dive returns to the surface with a fresh Air loop at 1x while the boards keep running', async () => {
  const g = await mountGame();
  g.feed(0, 0);
  g.view.chooseSpeed(4);
  g.view.changeMotion(30);
  g.feed(30, 300);
  g.view.sim.setGas('tx1050');
  assert.ok(g.view.sim.depth > 10);
  g.el('diluent-select').value = 'tx1050';
  g.el('reset').click();
  await settle();
  const sim = g.view.sim;
  assert.equal(sim.depth, 0);
  assert.equal(sim.maxDepth, 0);
  assert.equal(sim.direction, 0);
  assert.equal(sim.gas.name, 'Air');
  assert.equal(g.el('diluent-select').value, 'air');
  assert.equal(g.view.clock.speed, 1);
  assert.equal(g.speeds().at(-1), 1);
  assert.equal(sim.virtual, 30, 'the emulator\'s clock is not reset');
  assert.equal(g.text('virtual-time'), '00:00:30');
  assert.equal(g.text('max-depth-meta'), 'Max 0.0 m');
  assert.equal(g.actions().some((request) => ['reset', 'cold', 'wake'].includes(request.action)), false, 'no board is restarted');
  const surface = g.inputs().at(-1);
  assert.equal(surface.pressure1Mbar, 1013.25);
  assert.equal(surface.oxygen1Mv, 12.5);
  g.view.hide();
});

test('game (view): the handset is operated through its pins: bezel keys, the display\'s tap zones, and the keyboard while it has the focus', async () => {
  const g = await mountGame();
  g.feed(0, 0);
  await settle();
  const device = g.el('device');
  const press = (name) => g.actions().filter((request) => request.action === name).length;
  const counts = async () => { await settle(); return [press('up'), press('down'), press('confirm')]; };
  g.el('handset-up').click();
  g.el('handset-down').click();
  assert.deepEqual(await counts(), [1, 1, 0]);
  assert.equal(g.view.sim.direction, 0, 'bezel keys do not move the diver');
  assert.equal(g.document.activeElement, device, 'the handset has the focus after a click, so the keyboard drives it');
  // The display: the upper third is Up, the middle Confirm, the lower third Down (the fake canvas is 480 high).
  const frame = g.el('frame');
  frame.dispatch('click', { clientY: 10 });
  frame.dispatch('click', { clientY: 240 });
  frame.dispatch('click', { clientY: 470 });
  assert.deepEqual(await counts(), [2, 2, 1]);
  // Enter confirms only with the handset focused. The arrow keys press the handset's Up and Down wherever the focus is
  // (one press per key press, no page scroll, a field too) and never move the diver; W and S swim.
  device.dispatch('keydown', { key: 'Enter', code: 'Enter', repeat: false });
  assert.deepEqual(await counts(), [2, 2, 2]);
  const ocean = g.el('ocean');
  g.document.dispatch('keydown', { target: ocean, key: 'ArrowUp', code: 'ArrowUp', repeat: false });
  g.document.dispatch('keydown', { target: device, key: 'ArrowDown', code: 'ArrowDown', repeat: false });
  g.document.dispatch('keydown', { key: 'ArrowDown', code: 'ArrowDown', repeat: false });
  assert.deepEqual(await counts(), [3, 4, 2]);
  const held = g.document.dispatch('keydown', { key: 'ArrowDown', code: 'ArrowDown', repeat: true });
  assert.deepEqual(await counts(), [3, 4, 2], 'a held key is one press');
  assert.equal(held.defaultPrevented, true, 'and does not scroll the page');
  g.document.dispatch('keydown', { key: 'Enter', code: 'Enter', repeat: false });
  assert.deepEqual(await counts(), [3, 4, 2], 'Enter outside the handset is not Confirm');
  const inField = g.document.dispatch('keydown', { target: g.el('mav-flow'), key: 'ArrowUp', code: 'ArrowUp', repeat: false });
  assert.equal(inField.defaultPrevented, true, 'a field does not take the arrow keys');
  assert.deepEqual(await counts(), [4, 4, 2], 'they press the handset there too');
  assert.equal(g.view.sim.direction, 0, 'and the arrow keys never move the diver');

  // W and S: hold to swim at 6 / 10 m/min; a double tap and hold, or a triple tap and hold, goes faster (to 18 / 30).
  const key = (type, code, target) => g.document.dispatch(type, { code, key: code.slice(3).toLowerCase(), repeat: false, ...(target ? { target } : {}) });
  key('keydown', 'KeyS');
  assert.equal(g.view.sim.direction, 1);
  near(g.view.sim.rate, 10);
  key('keyup', 'KeyS');
  assert.equal(g.view.sim.direction, 0, 'release holds the depth');
  key('keydown', 'KeyS');
  near(g.view.sim.rate, 20, 'double tap and hold');
  key('keyup', 'KeyS');
  key('keydown', 'KeyS');
  near(g.view.sim.rate, 30, 'triple tap and hold');
  key('keyup', 'KeyS');
  key('keydown', 'KeyS');
  near(g.view.sim.rate, 30, 'and no faster');
  key('keyup', 'KeyS');
  await new Promise((resolve) => setTimeout(resolve, 350));
  key('keydown', 'KeyW');
  assert.equal(g.view.sim.direction, -1);
  near(g.view.sim.rate, 6, 'a pause between taps starts over');
  key('keydown', 'KeyS');
  assert.equal(g.view.sim.direction, 0, 'W and S together hold the depth');
  key('keyup', 'KeyS');
  assert.equal(g.view.sim.direction, -1);
  key('keyup', 'KeyW');
  assert.equal(g.view.sim.direction, 0);
  key('keydown', 'KeyS', g.el('mav-flow'));
  assert.equal(g.view.sim.direction, 0, 'a field being edited keeps its letters');
  key('keyup', 'KeyS', g.el('mav-flow'));
  await new Promise((resolve) => setTimeout(resolve, 350));
  key('keydown', 'KeyS');
  g.view.chooseSpeed(0);
  assert.equal(g.view.sim.direction, 0, 'a pause stops the swim');
  key('keyup', 'KeyS');
  key('keydown', 'KeyS');
  assert.equal(g.view.sim.direction, 0, 'and a paused water takes no keys');
  g.view.hide();
});

test('game (view): errors and stops stay in view, the standby has a Wake button, and Quit goes through the session close', async () => {
  const g = await mountGame();
  g.feed(0, 0);
  assert.equal(g.el('alerts').hidden, true);
  g.feed(1, 300, { running: false, standby: true });
  assert.equal(g.el('alerts').hidden, false);
  assert.equal(g.text('run-state'), 'Standby');
  assert.match(g.text('alerts'), /requested standby/);
  assert.equal(g.text('motion-status'), 'Emulator stopped');
  const wake = g.el('alerts').querySelectorAll('button').find((button) => button.dataset.alert === 'wake');
  assert.ok(wake, 'a Wake button');
  wake.click();
  await settle();
  const names = g.actions().map((request) => request.action);
  assert.deepEqual(names.slice(-2), ['wake', 'resume'], 'the boards are woken and the engine resumed');
  assert.equal(g.actions().find((request) => request.action === 'wake').surfacePressureMbar, 1013.25);
  // The board creation restarts the integration from the new virtual time and sends the inputs again.
  const before = g.inputs().length;
  g.feed(0.4, 2000, {}, { generation: 2 });
  await settle();
  assert.equal(g.view.sim.virtual, 0.4);
  assert.equal(g.inputs().length, before + 1, 'the inputs go out again after a board creation');
  assert.equal(g.el('alerts').hidden, true, 'the message goes away with the stop');
  // An engine error is shown with its text and a Resume button; action errors are shown and can be dismissed.
  g.feed(2, 2500, { running: false, error: 'Terminal handler reached at 0x08001234' });
  assert.match(g.text('alerts'), /Terminal handler reached at 0x08001234/);
  assert.equal(g.text('run-state'), 'Stopped by an error');
  g.view.actionError = 'The main board has not enabled the handset supply yet';
  g.view.renderAlerts();
  assert.match(g.text('alerts'), /handset supply/);
  g.el('alerts').querySelectorAll('button').find((button) => button.dataset.dismiss === 'action').click();
  assert.doesNotMatch(g.text('alerts'), /handset supply/);
  // A lost engine is not a silent freeze either.
  g.view.setConnectionError('The emulation engine stopped after an internal error (x).');
  assert.equal(g.text('run-state'), 'Disconnected');
  assert.match(g.text('alerts'), /internal error/);
  // Notices of the worker (a failed save, the profile lock) are listed too.
  g.view.addNotice('warning', 'Another tab or window of this application is using the saved profile.');
  assert.match(g.text('alerts'), /saved profile/);
  // Quit hands over to the page's session close.
  g.el('quit').click();
  await settle();
  assert.equal(g.quits.length, 1);
  g.view.hide();
});

test('game (view): the three indicators show the firmware\'s outputs with the pulse replay, and have no tap-to-preview', async () => {
  const g = await mountGame();
  g.feed(0, 0, { hardwareOutputs: [vibratorOrLed(pulses(0)), vibratorOrLed(pulses(0), 'vibrator')] });
  const lit = (slot) => g.el(`signal-${slot}`).classList.contains('active');
  assert.equal(lit('white'), false);
  assert.match(g.el('signal-white').getAttribute('aria-label'), /^White HUD LED \(HUD 2\): Off$/);
  assert.match(g.el('signal-vibrator').getAttribute('aria-label'), /^Handset vibrator: Off$/);
  assert.match(g.el('signal-red').getAttribute('aria-label'), /^Red HUD LED \(HUD 3\): unknown$/, 'an output the engine does not report stays unknown');
  assert.equal(g.el('signal-red').classList.contains('unknown'), true);
  // A short pulse between two states flashes the indicator (the replay of the emulator view) while the drive reads Off.
  g.feed(1, 300, { hardwareOutputs: [vibratorOrLed(pulses(1))] });
  assert.equal(lit('white'), true, 'the captured pulse is replayed');
  assert.match(g.el('signal-white').getAttribute('aria-label'), /Pulse$/);
  g.clock.advance(150);
  assert.equal(lit('white'), false, 'after 150 ms');
  // A steady On stays lit.
  g.feed(2, 600, { hardwareOutputs: [vibratorOrLed([event(1, false), event(2, true)])] });
  assert.equal(lit('white'), true);
  assert.match(g.el('signal-white').getAttribute('aria-label'), /On$/);
  // Click handlers of the old demo are gone: the indicators are not controls.
  assert.equal(g.el('signal-white').tagName, 'DIV');
  assert.equal(g.el('signal-white').getAttribute('role'), 'img');
  assert.equal(g.el('signal-white').listeners.has('click'), false);
  g.view.hide();
});

test('game (view): the firmware frame fills the bezel, and a frame that arrives while the game starts is kept', async () => {
  const g = await mountGame();
  const recycled = [];
  g.view.client.send = (type, payload) => { if (type === 'recycle') recycled.push(payload.buffer.byteLength); };
  g.feed(0, 0, { frameReady: false });
  assert.equal(g.el('frame').hidden, true);
  assert.equal(g.text('placeholder'), 'Waiting for LCD output');
  g.view.onFrame({ width: 320, height: 240, version: 1, buffer: new ArrayBuffer(320 * 240 * 4) });
  assert.deepEqual(recycled, [320 * 240 * 4], 'the buffer goes back to the worker');
  g.feed(0.2, 300, { frameReady: true });
  assert.equal(g.el('frame').hidden, false, 'the firmware frame is shown');
  assert.equal(g.el('placeholder').hidden, true);
  g.feed(0.4, 600, { frameReady: false });
  assert.equal(g.el('frame').hidden, true, 'a panel that is off shows the placeholder again');
  g.view.hide();
  // The first frame of a session arrives before the page knows the session: it is not lost by show().
  g.view.onFrame({ width: 320, height: 240, version: 2, buffer: new ArrayBuffer(320 * 240 * 4) });
  g.view.show({ options: { mode: 'dual' }, release: describeRelease(DEFAULT_RELEASE_ID) });
  g.view.onState({ state: { ...g.baseState, virtualTime: 0, frameReady: true }, host: g.host });
  assert.equal(g.el('frame').hidden, false);
  g.view.hide();
});

test('game (view): the header names the release or the custom build, Start paused starts paused, and the surface pressure is the session\'s', async () => {
  const custom = await mountGame({ release: describeRelease(CUSTOM_RELEASE_ID) });
  assert.equal(custom.text('release'), 'Custom build');
  assert.equal(custom.text('wrist-name'), 'Custom build');
  assert.match(custom.document.title, /Custom build/);
  assert.ok(custom.store.map.has('game-cells.custom'), 'the cells of the shared custom profile');
  custom.view.hide();
  const neptun = await mountGame({ release: describeRelease('NEPTUN-5.8-65.3') });
  assert.equal(neptun.text('release'), 'NEPTUN main 5.8 / handset 65.3');
  assert.ok(neptun.store.map.has('game-cells.profile-neptun-5_8-65_3'));
  neptun.view.hide();
  const paused = await mountGame({ options: { startPaused: true, surfacePressureMbar: 950 } });
  paused.feed(0, 0, { running: false });
  await settle();
  assert.equal(paused.view.clock.speed, 0, 'Start paused is the game\'s Pause');
  assert.equal(paused.text('header-speed'), 'Pause');
  assert.equal(paused.text('run-state'), 'Paused');
  assert.equal(paused.inputs().at(-1).pressure1Mbar, 950, 'the session\'s surface pressure, not 1013.25');
  assert.equal(paused.speeds().length, 0, 'no pacing request: the engine is paused and stays so');
  paused.view.togglePause();
  await settle();
  assert.equal(paused.speeds().at(-1), 1);
  assert.equal(paused.actions().at(-1).action, 'resume');
  paused.view.hide();
  assert.equal(globalThis.document.title, 'NGC system emulator · WebAssembly', 'the window title is the emulator\'s again');
});

// ---- the game's page structure: ids, styles, publishing ------------------------------------------------------------

test('game (structure): every id of the game starts with game-, the scripts find all of theirs, and the emulator\'s wiring cannot reach the game', () => {
  const start = html.indexOf('<div id="screen-game"');
  const end = html.indexOf('<script type="module" src="app.js">');
  assert.ok(start > 0 && end > start);
  const section = html.slice(start, end);
  const ids = [...section.matchAll(/\bid="([\w-]+)"/g)].map((match) => match[1]);
  assert.ok(ids.length > 60);
  for (const id of ids) assert.ok(id === 'screen-game' || id.startsWith('game-'), `${id} would collide with the other screens`);
  const pageIds = [...html.matchAll(/\bid="([\w-]+)"/g)].map((match) => match[1]);
  assert.equal(new Set(pageIds).size, pageIds.length, 'ids are unique across the page');
  const source = fs.readFileSync(path.join(here, 'game.js'), 'utf8');
  const used = new Set([...source.matchAll(/\$\('([\w-]+)'\)/g)].map((match) => `game-${match[1]}`));
  for (const name of [...source.matchAll(/\$\(`([\w-]+)-\$\{[a-z]+\}`\)/g)].map((match) => match[1])) {
    for (const suffix of ['up', 'down', 'oxygen', 'diluent', 'o2', 'n2', 'he', '1', '2', '3', 'vibrator', 'red', 'white']) if (ids.includes(`game-${name}-${suffix}`)) used.add(`game-${name}-${suffix}`);
  }
  for (const id of used) assert.ok(ids.includes(id), `game.js looks up #${id}, which index.html lacks`);
  assert.ok(used.size > 50, 'the lookups were found');
  // emulator.js binds every [data-action] of the page when it starts; the game uses none of them (nor its ids).
  assert.doesNotMatch(section, /data-action=/);
  assert.doesNotMatch(section, /\sstyle=/);
  assert.match(html, /<div id="screen-game" hidden>/, 'the game is hidden until it starts');
  assert.match(html, /<main id="app-main">/);
  assert.ok(html.indexOf('</main>') < start, 'the game is a screen of its own, outside <main>');
  // The entry screen has the button and the reason.
  assert.match(html, /<button type="button" id="start-game" class="game-start" disabled/);
  assert.match(html, /id="game-hint"/);
  // The mock-only texts of the prototype are gone.
  assert.doesNotMatch(section, /UI PROTOTYPE|Emulator disconnected|Mock readings|Mock assumptions|MOCK CLOCK|Handset key preview|no emulator connection/i);
  assert.doesNotMatch(source, /signalPreviews|Handset key preview|MOCK_CELL/);
});

test('game (structure): every rule of game.css is scoped to #screen-game, the page\'s styles never name the game, and the keyframes are the game\'s own', () => {
  const css = fs.readFileSync(path.join(here, 'game.css'), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const scoped = (text) => {
    const found = [];
    let depth = 0;
    let start = 0;
    let preludeStack = [];
    for (let index = 0; index < text.length; index++) {
      if (text[index] === '{') {
        preludeStack.push(text.slice(start, index).trim());
        depth += 1;
        start = index + 1;
      } else if (text[index] === '}') {
        depth -= 1;
        preludeStack.pop();
        start = index + 1;
      } else if (text[index] === ';' && depth === 0) {
        start = index + 1;
      }
      if (text[index] === '{') found.push({ prelude: preludeStack.at(-1), parents: preludeStack.slice(0, -1) });
    }
    return found;
  };
  const blocks = scoped(css);
  assert.ok(blocks.length > 200, `${blocks.length} blocks`);
  const keyframes = [];
  for (const { prelude, parents } of blocks) {
    if (prelude.startsWith('@media')) {
      assert.match(prelude, /^@media\s*\(/, prelude);
      continue;
    }
    if (prelude.startsWith('@keyframes')) {
      keyframes.push(prelude.split(/\s+/)[1]);
      continue;
    }
    if (parents.some((parent) => parent.startsWith('@keyframes'))) continue; // from, to and percentages
    assert.ok(!prelude.startsWith('@'), `an at-rule the game does not expect: ${prelude}`);
    // Commas inside parentheses (:where(:not(svg, svg *))) do not separate selectors.
    const selectors = [];
    let level = 0;
    let current = '';
    for (const char of prelude) {
      if (char === '(') level += 1;
      if (char === ')') level -= 1;
      if (char === ',' && level === 0) {
        selectors.push(current);
        current = '';
      } else {
        current += char;
      }
    }
    selectors.push(current);
    for (const selector of selectors) {
      assert.ok(/^#screen-game(?=[\s.:#\[>]|$)/.test(selector.trim()), `unscoped game selector: ${selector.trim()}`);
    }
  }
  assert.ok(keyframes.length >= 3 && keyframes.every((name) => name.startsWith('game-')), `keyframes: ${keyframes}`);
  assert.doesNotMatch(css, /:root|(^|[\s,{}])(html|body)\b/m, 'no rule about the document itself');
  assert.doesNotMatch(css, /@import|url\(\s*['"]?https?:/, 'no external resource');
  // The isolation rule: the page's own element and class rules do not reach into the game (SVG presentation attributes are kept).
  assert.match(css, /#screen-game :where\(:not\(svg, svg \*\)\) \{ all: revert; \}/);
  assert.ok(css.indexOf('all: revert') < css.indexOf('#screen-game * { box-sizing'), 'the reset comes before the game\'s own rules');
  // The other way round: style.css never mentions the game screen, and loads before game.css.
  const page = fs.readFileSync(path.join(here, 'style.css'), 'utf8');
  assert.doesNotMatch(page, /screen-game|#game-/);
  assert.ok(html.indexOf('href="style.css"') < html.indexOf('href="game.css"'));
  // Custom properties of the game are its own (--g-*); none of the page's names is redefined.
  const defined = [...css.matchAll(/(--[\w-]+)\s*:/g)].map((match) => match[1]);
  assert.ok(defined.every((name) => name.startsWith('--g-') || name.startsWith('--signal-')), `variables: ${defined.filter((name) => !name.startsWith('--g-') && !name.startsWith('--signal-'))}`);
});

test('game (structure): the game modules are published on purpose, the page script is the only entry, and no new dependency or external resource appears', () => {
  const build = fs.readFileSync(path.join(here, '..', 'deploy', 'build_site.py'), 'utf8');
  for (const name of ['game.js', 'game-gas.js', 'game-logic.js', 'game.css']) assert.match(build, new RegExp(`"${name.replace('.', '\\.')}"`), `${name} is on the site allowlist`);
  assert.doesNotMatch(build, /game-gas\.test|test-ui/);
  for (const file of ['game.js', 'game-logic.js', 'game-gas.js']) {
    const source = fs.readFileSync(path.join(here, file), 'utf8');
    for (const match of source.matchAll(/from\s+'([^']+)'/g)) assert.match(match[1], /^\.\//, `${file} imports only page modules (${match[1]})`);
    assert.doesNotMatch(source, /\bfetch\(|XMLHttpRequest|WebSocket|eval\(|new Function|innerHTML/, `${file} makes no request and builds no markup from text`);
  }
  assert.match(fs.readFileSync(path.join(here, 'app.js'), 'utf8'), /import \{ GameView \} from '\.\/game\.js'/);
});
