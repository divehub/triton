#!/usr/bin/env node
// Node benchmark of the WebAssembly engine: the same shape as `ngc-cli bench` and
// the benchmark of the Renode runner (fresh storage, handset-wake boot to 4.5 virtual s with the
// 50 ms power-gate poll inside the engine, three 1 s steady intervals, then the menu-redraw interval with a
// Down and an Up press). Reads the two original SREC files from firmware/TRITON-5.8-65.3/ and the release
// wasm staged by `python3 web/build.py` (web/pkg/ngc_wasm.wasm), else the one in the cargo target
// directory of the web package (`./cargo build -p ngc-wasm --release --target wasm32-unknown-unknown
// --target-dir target/web`) or of the system package (target/sys); `--wasm` overrides. It uses the benchmark half
// of the module's ABI (`ngc_create`, `ngc_run_for`, ...), which is independent of the browser session API.
//
// Usage: node web/bench-node.mjs [--wasm file] [--main srec] [--handset srec]
//          [--boot-seconds 4.5] [--seconds 1] [--steady-samples 3] [--no-idle-ff] [--no-menu]
//          [--slice 0.05] [--json out.json] [--expect native-bench.json]
//
// --expect compares the state digests with a result of `ngc-cli bench --json` taken with the same options
// (the engine is deterministic: native and WebAssembly runs must be bit-identical).
//
// --slice runs every interval as repeated calls of that many virtual seconds (like a browser worker that
// paces the engine); the default is one call per interval. Timing is measured with performance.now() around
// the engine calls only. The state digests printed are comparable with `ngc-cli bench --json`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '..');
// You supply the firmware yourself (it is not part of the repository): firmware/ of this checkout, or the directory
// named by NGC_FIRMWARE_DIR (which holds the release directories); --main / --handset override both.
const firmwareRoot = process.env.NGC_FIRMWARE_DIR || path.join(repo, 'firmware');

// The staged module (web/pkg, see build.py), else the cargo target directories of the web and system work packages.
const wasmCandidates = [
  path.resolve(here, 'pkg/ngc_wasm.wasm'),
  path.resolve(here, '../target/web/wasm32-unknown-unknown/release/ngc_wasm.wasm'),
  path.resolve(here, '../target/sys/wasm32-unknown-unknown/release/ngc_wasm.wasm'),
];

function parseArgs(argv) {
  const options = {
    wasm: wasmCandidates.find((candidate) => fs.existsSync(candidate)) || wasmCandidates[0],
    main: path.join(firmwareRoot, 'TRITON-5.8-65.3/ngc_main_5.8_TRITON.srec'),
    handset: path.join(firmwareRoot, 'TRITON-5.8-65.3/ngc_handset_65.3_TRITON.srec'),
    bootSeconds: 4.5,
    seconds: 1,
    samples: 3,
    idleFf: true,
    menu: true,
    slice: 0,
    json: null,
    expect: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} needs a value`);
      return argv[++i];
    };
    switch (arg) {
      case '--wasm': options.wasm = path.resolve(value()); break;
      case '--main': options.main = path.resolve(value()); break;
      case '--handset': options.handset = path.resolve(value()); break;
      case '--boot-seconds': options.bootSeconds = Number(value()); break;
      case '--seconds': options.seconds = Number(value()); break;
      case '--steady-samples': options.samples = Number(value()); break;
      case '--slice': options.slice = Number(value()); break;
      case '--json': options.json = path.resolve(value()); break;
      case '--expect': options.expect = path.resolve(value()); break;
      case '--no-idle-ff': options.idleFf = false; break;
      case '--no-menu': options.menu = false; break;
      case '--help':
      case '-h':
        console.log(fs.readFileSync(fileURLToPath(import.meta.url), 'utf8').split('\n').filter((l) => l.startsWith('//')).map((l) => l.slice(3)).join('\n'));
        process.exit(0);
        break;
      default: throw new Error(`unknown option ${arg}`);
    }
  }
  return options;
}

class Engine {
  constructor(instance) {
    this.x = instance.exports;
  }

  bytes() {
    return new Uint8Array(this.x.memory.buffer); // re-read after any call: memory may grow
  }

  text(length) {
    const ptr = this.x.ngc_output_ptr();
    return new TextDecoder().decode(this.bytes().slice(ptr, ptr + length));
  }

  error() {
    return this.text(this.x.ngc_error());
  }

  check(code, what) {
    if (code !== 0) throw new Error(`${what} failed: ${this.error()}`);
  }

  setFirmware(role, data) {
    const ptr = this.x.ngc_alloc(data.length);
    this.bytes().set(data, ptr);
    const code = this.x.ngc_set_firmware(role, ptr, data.length);
    this.x.ngc_free(ptr, data.length);
    this.check(code, role === 0 ? 'main firmware' : 'handset firmware');
  }

  create({ idleFf }) {
    this.check(this.x.ngc_create(0, idleFf ? 0 : 4, 400), 'create');
  }

  run(seconds, slice) {
    let left = seconds;
    const step = slice > 0 ? slice : seconds;
    while (left > 1e-9) {
      const part = Math.min(step, left);
      this.x.ngc_run_for(part);
      left -= part;
    }
  }

  fingerprint() {
    return this.text(this.x.ngc_fingerprint());
  }

  status() {
    return JSON.parse(this.text(this.x.ngc_status()));
  }

  counters() {
    return {
      mainInstructions: this.x.ngc_instructions_f64(0),
      handsetInstructions: this.x.ngc_instructions_f64(1),
      mainSkipped: this.x.ngc_idle_skipped(0),
      handsetSkipped: this.x.ngc_idle_skipped(1),
      mainSlices: this.x.ngc_slices(0),
      handsetSlices: this.x.ngc_slices(1),
      timeNs: Number(this.x.ngc_time_ns()),
    };
  }
}

function measure(engine, label, chunks, press, slice) {
  const before = engine.counters();
  const buttons = [];
  const started = performance.now();
  chunks.forEach((seconds, index) => {
    if (press) {
      const up = index % 2 === 1;
      const code = engine.x.ngc_navigate(up ? 1 : 0);
      buttons.push(code === 0 ? `${up ? 'up' : 'down'} accepted` : `${up ? 'up' : 'down'} failed: ${engine.error()}`);
    }
    engine.run(seconds, slice);
  });
  const wall = (performance.now() - started) / 1000;
  const after = engine.counters();
  const elapsed = (after.timeNs - before.timeNs) / 1e9;
  const result = {
    label,
    requestedVirtualSeconds: chunks.reduce((a, b) => a + b, 0),
    elapsedVirtualSeconds: elapsed,
    wallSeconds: wall,
    virtualSecondsPerWallSecond: elapsed / wall,
    runForCalls: slice > 0 ? chunks.reduce((n, s) => n + Math.ceil(s / slice - 1e-9), 0) : chunks.length,
    buttonEvents: buttons,
    executedInstructions: {
      main: after.mainInstructions - before.mainInstructions,
      handset: after.handsetInstructions - before.handsetInstructions,
    },
    idleSkippedInstructions: {
      main: after.mainSkipped - before.mainSkipped,
      handset: after.handsetSkipped - before.handsetSkipped,
    },
    slices: { main: after.mainSlices - before.mainSlices, handset: after.handsetSlices - before.handsetSlices },
    stateDigest: engine.fingerprint(),
  };
  const total = result.executedInstructions.main + result.executedInstructions.handset;
  const skipped = result.idleSkippedInstructions.main + result.idleSkippedInstructions.handset;
  console.log(
    `${label}: ${elapsed.toFixed(3)} virtual s / ${wall.toFixed(3)} wall s = ${result.virtualSecondsPerWallSecond.toFixed(3)}x; ` +
      `instructions main ${result.executedInstructions.main} handset ${result.executedInstructions.handset}; ` +
      `idle-skipped ${(total > 0 ? (100 * skipped) / total : 0).toFixed(1)}%`,
  );
  buttons.forEach((b) => console.log(`    button ${b}`));
  return result;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const wasm = fs.readFileSync(options.wasm);
  const module = await WebAssembly.compile(wasm);
  const imports = WebAssembly.Module.imports(module);
  if (imports.length > 0) {
    console.log(`note: the module imports ${imports.map((i) => `${i.module}.${i.name}`).join(', ')}`);
  }
  const instance = await WebAssembly.instantiate(module, Object.fromEntries(imports.map((i) => [i.module, {}])));
  const engine = new Engine(instance);
  console.log(`node ${process.version}, wasm ${path.relative(process.cwd(), options.wasm)} (${wasm.length} bytes), idle fast-forward ${options.idleFf ? 'on' : 'off'}`);

  engine.setFirmware(0, fs.readFileSync(options.main));
  engine.setFirmware(1, fs.readFileSync(options.handset));
  const created = performance.now();
  engine.create({ idleFf: options.idleFf });
  const setupSeconds = (performance.now() - created) / 1000;

  const boot = performance.now();
  engine.run(options.bootSeconds, options.slice);
  const bootWall = (performance.now() - boot) / 1000;
  const bootCounters = engine.counters();
  const status = engine.status();
  const bootFactor = bootCounters.timeNs / 1e9 / bootWall;
  const bootDigest = engine.fingerprint();
  // `mainBatteryReady` is true / false, or null when this firmware release has no proven readiness byte (NEPTUN):
  // null is unknown, not "not ready". The session state names the reason in `unavailable.mainBatteryReady`; the bare
  // status of this benchmark's ABI may not carry it.
  const batteryUnknown = status.mainBatteryReady === null || status.mainBatteryReady === undefined;
  const batteryReason = (status.unavailable && status.unavailable.mainBatteryReady) || 'no proven readiness byte for this firmware release';
  const batteryText = batteryUnknown ? `battery ready unknown (${batteryReason})` : `battery ready ${status.mainBatteryReady}`;
  console.log(
    `boot: ${(bootCounters.timeNs / 1e9).toFixed(3)} virtual s / ${bootWall.toFixed(3)} wall s = ${bootFactor.toFixed(3)}x ` +
      `(setup ${setupSeconds.toFixed(3)} s, handset release ${status.handsetReleaseTime ?? 'never'}, ${batteryText})`,
  );
  if (status.handsetReleaseTime == null || status.mainBatteryReady === false) {
    console.log('WARNING: the firmware did not reach the expected paired sensor-ready boot state');
  }

  const measurements = [];
  for (let sample = 0; sample < options.samples; sample++) {
    const label = options.samples === 1 ? 'steady' : `steady_sample_${sample + 1}`;
    measurements.push(measure(engine, label, [options.seconds], false, options.slice));
  }
  if (options.menu) {
    measurements.push(measure(engine, 'menu_redraw_after_steady', [0.5, 0.5], true, options.slice));
  }

  const worst = Math.min(bootFactor, ...measurements.map((m) => m.virtualSecondsPerWallSecond));
  const steady = measurements.filter((m) => m.label.startsWith('steady')).map((m) => m.virtualSecondsPerWallSecond);
  console.log(
    `summary: boot ${bootFactor.toFixed(3)}x, steady ${steady.map((v) => v.toFixed(3)).join('x, ')}x` +
      (options.menu ? `, menu redraw ${measurements[measurements.length - 1].virtualSecondsPerWallSecond.toFixed(3)}x` : '') +
      `; gate (>0.9x sustained): ${Math.min(...steady, ...(options.menu ? [measurements[measurements.length - 1].virtualSecondsPerWallSecond] : [])) > 0.9 ? 'met' : 'NOT met'}`,
  );

  let identical = null;
  if (options.expect) {
    // Cross-engine determinism: the state digests of `ngc-cli bench --json` for the same configuration.
    const native = JSON.parse(fs.readFileSync(options.expect, 'utf8'));
    const run = (native.runs || []).find((r) => r.idleFastForward === options.idleFf) || (native.runs || [])[0];
    if (!run) throw new Error(`${options.expect} has no runs`);
    const checks = [['boot', run.bootStateDigest, bootDigest]];
    measurements.forEach((m, i) => checks.push([m.label, run.measurements?.[i]?.stateDigest, m.stateDigest]));
    identical = checks.every(([, a, b]) => a === b);
    console.log(
      `native vs wasm state digests: ${identical ? 'IDENTICAL' : 'MISMATCH'} (` +
        checks.map(([label, a, b]) => `${label} ${a === b ? 'same' : 'DIFFERENT'}`).join(', ') + ')',
    );
  }

  if (options.json) {
    const result = {
      engine: status.engine,
      runtime: { node: process.version, platform: process.platform, arch: process.arch },
      idleFastForward: options.idleFf,
      sliceVirtualSeconds: options.slice || null,
      setupSeconds,
      bootVirtualSeconds: bootCounters.timeNs / 1e9,
      bootWallSeconds: bootWall,
      bootVirtualSecondsPerWallSecond: bootFactor,
      bootStateDigest: bootDigest,
      handsetReleaseVirtualSeconds: status.handsetReleaseTime ?? null,
      mainBatteryReady: batteryUnknown ? null : status.mainBatteryReady,
      mainBatteryReadyUnknownReason: batteryUnknown ? batteryReason : null,
      measurements,
      worstVirtualSecondsPerWallSecond: worst,
      identicalToNativeDigests: identical,
    };
    fs.writeFileSync(options.json, JSON.stringify(result, null, 2) + '\n');
    console.log(`result written to ${options.json}`);
  }
}

main().catch((error) => {
  console.error(error.stack || String(error));
  process.exit(1);
});
