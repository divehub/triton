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
// --dive runs the committed dive benchmark instead (DESIGN.md 16.3, the same plan as `ngc-cli bench --dive`) through the
// browser session ABI (`ngc_session_*`, 10 virtual-ms slices like the worker): a valid-tissue profile is built from a fresh
// one through firmware routes only (battery wizard, air calibration through the menu, a 150 s NaN dive that saves a
// decompression date, a +5 day main RTC checkpoint fixture, restart and recalibration), then one dive per depth. It reports
// the average speed, the speed of the main board's compute bursts and of the quiet periods, and the state digests of the
// checkpoints. Dive options: --dive-seconds S (60), --dive-depths 20,30, --no-routine-accel (the exact routine acceleration
// is on by default), --verify-routine-accel (run it on and off, require identical checkpoints), --no-idle-ff, --frames
// (also call ngc_session_frame() at 60 Hz of virtual time and ngc_session_state() at 5 Hz, like the page does; their wall
// time is reported separately and never enters the engine factors), --json out.json, and --expect native-dive.json
// (a result of `ngc-cli bench --dive --json` with the same options: every checkpoint digest must be identical).
//
// --slice runs every interval as repeated calls of that many virtual seconds (like a browser worker that
// paces the engine); the default is one call per interval. Timing is measured with performance.now() around
// the engine calls only. The state digests printed are comparable with `ngc-cli bench --json`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Engine as SessionEngine } from './engine.js';

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
    routineAccel: true,
    dive: false,
    diveSeconds: 60,
    diveDepths: [20, 30],
    verifyRoutineAccel: false,
    frames: false,
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
      case '--no-routine-accel': options.routineAccel = false; break;
      case '--verify-routine-accel': options.verifyRoutineAccel = true; break;
      case '--dive': options.dive = true; break;
      case '--frames': options.frames = true; break;
      case '--dive-seconds': options.diveSeconds = Number(value()); break;
      case '--dive-depths': options.diveDepths = value().split(',').map((d) => Number(d.trim())); break;
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

  create({ idleFf, routineAccel = true }) {
    // flags: bit 2 idle fast-forward off, bit 4 routine acceleration off
    this.check(this.x.ngc_create(0, (idleFf ? 0 : 4) | (routineAccel ? 0 : 16), 400), 'create');
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

// ---- the dive benchmark (DESIGN.md 16.3; the same plan as crates/ngc/src/scenario/dive.rs) ----------------------------------

const SLICE = 0.01; // virtual seconds per run_for call: the browser worker's slice
const STEP = 0.25; // virtual seconds per speed step
const BURST_EXECUTED_MIPS = 40; // main-board executed (not idle-skipped) instructions per virtual second, in millions
const CHECKPOINT_EVERY = 10;
const NAN_DIVE_SECONDS = 150;
const CLOCK_JUMP_DAYS = 5;
const DEPTH_AT = 34;
const BUBBLE_AT = 42;
const DEPTHS = { 20: { name: '20 m', mbar: 3013.3 }, 30: { name: '30 m', mbar: 4014.1 } };

const act = (at, request) => ({ at, request });
const DOWN = { action: 'down' };
const UP = { action: 'up' };
const CONFIRM = { action: 'confirm' };
const inputsRequest = (mbar) => ({ action: 'inputs', inputs: { pressure1Mbar: mbar, pressure2Mbar: mbar, oxygen1Mv: 71.4, oxygen2Mv: 71.4, oxygen3Mv: 71.4 } });

const BATTERY_WIZARD = [act(6.5, DOWN), act(7.15, DOWN), act(7.8, CONFIRM), act(8.45, CONFIRM), act(9.1, DOWN), act(9.75, DOWN), act(10.4, CONFIRM), act(11.05, CONFIRM)];
const AIR_CALIBRATION = [act(6.5, UP), act(7.2, CONFIRM), act(7.9, DOWN), act(8.6, CONFIRM), act(9.4, DOWN), act(10.1, DOWN), act(10.8, DOWN), act(11.5, CONFIRM), act(12.3, CONFIRM), act(13.1, CONFIRM), act(22.0, CONFIRM)];
const NAN_DIVE = [act(8.0, inputsRequest(DEPTHS[30].mbar)), act(30.0, UP), act(33.0, DOWN)];
const RECALIBRATION = [
  act(6.5, UP), act(7.5, DOWN), act(8.5, DOWN), act(9.5, CONFIRM), act(11.2, DOWN), act(11.9, CONFIRM), act(12.7, DOWN), act(13.4, DOWN),
  act(14.1, DOWN), act(14.8, CONFIRM), act(15.6, CONFIRM), act(16.4, CONFIRM), act(27.0, CONFIRM),
];

function profileOf(parts) {
  return Object.fromEntries(parts.map((part) => [part.name, part.data]));
}

function checkpointOf(session, name) {
  const text = session.text(session.x.ngc_session_checkpoint());
  const c = JSON.parse(text);
  return { name, virtualNs: c.virtualNs, fingerprint: c.fingerprint, exactMain: c.exactMain, exactHandset: c.exactHandset, instructionsMain: c.instructionsMain, instructionsHandset: c.instructionsHandset, lcdSha256: c.lcdSha256 };
}

function mainCounters(session) {
  const s = session.state();
  return { instr: s.instructions.main, skipped: s.idleSkip.main.skippedInstructions, replaced: s.routineAccel?.main ?? { hits: 0, instructionsReplaced: 0 } };
}

/** Runs the session to `until` virtual seconds in slices, applying `timeline` between slices. */
function play(session, timeline, until, options, hooks = {}) {
  const untilNs = Math.round(until * 1e9);
  const slicesPerStep = Math.round(STEP / SLICE);
  let next = 0;
  let slices = 0;
  let wallTotal = 0;
  let wallStep = 0;
  let frameWall = 0;
  let stateWall = 0;
  let stepStart = mainCounters(session);
  let checkpointAt = CHECKPOINT_EVERY;
  let nextFrame = 0;
  let nextState = 0;
  const nowNs = () => Math.round(session.time() * 1e9);
  while (nowNs() < untilNs) {
    const now = nowNs();
    while (next < timeline.length && Math.round(timeline[next].at * 1e9) <= now) {
      session.action(timeline[next].request);
      next++;
    }
    const t0 = performance.now();
    const running = session.runFor(SLICE);
    const wall = (performance.now() - t0) / 1000;
    if (!running) throw new Error(`the system stopped at ${session.time().toFixed(3)} s (standby or error)`);
    wallTotal += wall;
    wallStep += wall;
    slices++;
    if (options.frames) {
      const t = session.time();
      if (t >= nextFrame) {
        const a = performance.now();
        session.frame();
        frameWall += (performance.now() - a) / 1000;
        nextFrame = t + 1 / 60;
      }
      if (t >= nextState) {
        const a = performance.now();
        session.stateText();
        stateWall += (performance.now() - a) / 1000;
        nextState = t + 0.2;
      }
    }
    if (slices === slicesPerStep) {
      const counters = mainCounters(session);
      const executed = Math.max(0, counters.instr - stepStart.instr - (counters.skipped - stepStart.skipped));
      hooks.step?.({ virtualSeconds: STEP, wallSeconds: wallStep, mainExecuted: executed, burst: executed / STEP >= BURST_EXECUTED_MIPS * 1e6 });
      stepStart = counters;
      wallStep = 0;
      slices = 0;
    }
    if (session.time() + 1e-9 >= checkpointAt) {
      hooks.checkpoint?.(checkpointAt);
      checkpointAt += CHECKPOINT_EVERY;
    }
  }
  return { wallTotal, frameWall, stateWall };
}

function addDaysBcd(dateRegister, days) {
  const day = ((dateRegister >> 4) & 3) * 10 + (dateRegister & 15);
  const newDay = day + days;
  if (newDay > 28) throw new Error(`the clock jump of ${days} days from day ${day} would leave the month`);
  return (dateRegister & ~0x3f) | (Math.floor(newDay / 10) << 4) | (newDay % 10);
}

// The workload the benchmark was measured with (DESIGN.md 16.6), as ngc::scenario::dive pins it: batteries at the 1500 mV
// that fit the Photolithium type the wizard chooses (a fresh profile's 4100 mV makes the firmware ask for a battery change
// and stand by) and both decompression fixtures off (the profile is built through the firmware's own routes instead).
const PINNED_INPUTS = new TextEncoder().encode('{"battery1Mv": 1500, "battery2Mv": 1500}\n');

function openSession(session, options, routineAccel, profile) {
  const pinned = profile['inputs.json'] ? profile : { ...profile, 'inputs.json': PINNED_INPUTS };
  session.createSession({ mode: 'dual', bootMode: 'handset-wake', idleFastForward: options.idleFf, routineAccel, decoStorageFixture: false, startAtSurface: false }, pinned);
}

function runStage(session, options, routineAccel, name, profile, timeline, seconds) {
  openSession(session, options, routineAccel, profile);
  const { wallTotal } = play(session, timeline, seconds, options);
  const checkpoint = checkpointOf(session, name);
  const result = { name, virtualSeconds: session.time(), wallSeconds: wallTotal, checkpoint };
  return { result, profile: profileOf(session.shutdown()) };
}

function speedOf(steps) {
  const sum = { steps: steps.length, virtualSeconds: 0, wallSeconds: 0 };
  for (const s of steps) {
    sum.virtualSeconds += s.virtualSeconds;
    sum.wallSeconds += s.wallSeconds;
  }
  sum.factor = sum.wallSeconds > 0 ? sum.virtualSeconds / sum.wallSeconds : Infinity;
  return sum;
}

function worstBurst(steps) {
  let worst = Infinity;
  let run = [];
  const close = () => {
    if (run.length >= 3) worst = Math.min(worst, speedOf(run).factor);
    run = [];
  };
  for (const s of steps) (s.burst ? run.push(s) : close());
  close();
  return worst;
}

function runDive(session, options, routineAccel, profile, depth) {
  openSession(session, options, routineAccel, profile);
  const timeline = [...RECALIBRATION, act(DEPTH_AT, inputsRequest(depth.mbar)), act(BUBBLE_AT, DOWN)];
  const steps = [];
  const checkpoints = [];
  const label = depth.name.replace(' ', '');
  const { wallTotal, frameWall, stateWall } = play(session, timeline, BUBBLE_AT + options.diveSeconds, options, {
    step: (step) => steps.push(step),
    checkpoint: (at) => checkpoints.push(checkpointOf(session, `dive-${label}-${at.toFixed(0)}s`)),
  });
  checkpoints.push(checkpointOf(session, `dive-${label}-end`));
  const state = session.state();
  const replaced = state.routineAccel?.main ?? { hits: 0, instructionsReplaced: 0, shadowMismatches: 0 };
  session.shutdown();
  const burst = speedOf(steps.filter((s) => s.burst));
  const quiet = speedOf(steps.filter((s) => !s.burst));
  const all = speedOf(steps);
  return {
    depth: depth.name,
    pressureMbar: depth.mbar,
    average: all,
    burst,
    quiet,
    worstBurstFactor: worstBurst(steps),
    replacedCalls: replaced.hits + (replaced.shadowChecks ?? 0),
    replacedInstructions: replaced.instructionsReplaced,
    shadowMismatches: replaced.shadowMismatches ?? 0,
    engineWallSeconds: wallTotal,
    frameWallSeconds: frameWall,
    stateWallSeconds: stateWall,
    screenMode: state.mainApplication?.screenMode ?? null,
    checkpoints,
  };
}

function runWholeDive(engine, options, routineAccel) {
  const session = engine;
  const stages = [];
  let profile = {};
  let surface = null;
  for (const [name, timeline, seconds] of [
    ['battery-wizard', BATTERY_WIZARD, 12.5],
    ['air-calibration', AIR_CALIBRATION, 27],
    ['nan-dive', NAN_DIVE, NAN_DIVE_SECONDS],
  ]) {
    const stage = runStage(session, options, routineAccel, name, profile, timeline, seconds);
    stages.push(stage.result);
    profile = stage.profile;
    if (name === 'air-calibration') surface = profile['inputs.json'];
  }
  // The explicit clock fixture: the saved main RTC calendar moves forward by days; the saved inputs are the surface defaults.
  const rtc = JSON.parse(new TextDecoder().decode(profile['rtc-state.json']));
  rtc.boards['ngc-main'].dateRegister = addDaysBcd(rtc.boards['ngc-main'].dateRegister, CLOCK_JUMP_DAYS);
  profile['rtc-state.json'] = new TextEncoder().encode(`${JSON.stringify(rtc, null, 2)}\n`);
  if (surface) profile['inputs.json'] = surface;
  else delete profile['inputs.json']; // openSession then pins the batteries again
  const dives = options.diveDepths.map((metres) => {
    const depth = DEPTHS[metres];
    if (!depth) throw new Error(`--dive-depths takes 20 and/or 30 (got ${metres})`);
    return runDive(session, options, routineAccel, profile, depth);
  });
  return { routineAccel: routineAccel ? 'on' : 'off', idleFastForward: options.idleFf, diveSeconds: options.diveSeconds, stages, dives };
}

function printDive(run) {
  console.log(`-- dive benchmark: routine acceleration ${run.routineAccel}, idle fast-forward ${run.idleFastForward ? 'on' : 'off'} --`);
  for (const s of run.stages) {
    console.log(`profile stage ${s.name.padEnd(16)} ${s.virtualSeconds.toFixed(1).padStart(6)} virtual s in ${s.wallSeconds.toFixed(2).padStart(6)} s wall = ${(s.virtualSeconds / s.wallSeconds).toFixed(2).padStart(6)}x   fingerprint ${s.checkpoint.fingerprint.slice(0, 16)}`);
  }
  for (const d of run.dives) {
    console.log(
      `dive ${d.depth}: average ${d.average.factor.toFixed(2)}x (${d.average.virtualSeconds.toFixed(1)} s in ${d.average.wallSeconds.toFixed(2)} s); bursts ${d.burst.factor.toFixed(2)}x ` +
        `(${d.burst.steps} steps, worst burst ${d.worstBurstFactor.toFixed(2)}x); quiet ${d.quiet.factor.toFixed(2)}x (${d.quiet.steps} steps); replaced ${d.replacedCalls} calls / ${d.replacedInstructions} instructions` +
        (d.shadowMismatches ? `; SHADOW MISMATCHES ${d.shadowMismatches}` : '') +
        (d.frameWallSeconds + d.stateWallSeconds > 0 ? `; frame ${d.frameWallSeconds.toFixed(2)} s + state ${d.stateWallSeconds.toFixed(2)} s on top` : ''),
    );
    for (const c of d.checkpoints) {
      console.log(`    ${c.name.padEnd(16)} t ${(c.virtualNs / 1e9).toFixed(3).padStart(8)} s  fingerprint ${c.fingerprint.slice(0, 16)}  exact ${c.exactMain}/${c.exactHandset}`);
    }
  }
}

function checkpointsOf(run) {
  return [...run.stages.map((s) => s.checkpoint), ...run.dives.flatMap((d) => d.checkpoints)];
}

function differences(a, b) {
  const x = checkpointsOf(a);
  const y = checkpointsOf(b);
  const out = [];
  if (x.length !== y.length) out.push(`${x.length} checkpoints against ${y.length}`);
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const what = [];
    if (x[i].virtualNs !== y[i].virtualNs) what.push('virtual time');
    if (x[i].fingerprint !== y[i].fingerprint) what.push('fingerprint');
    if (x[i].exactMain !== y[i].exactMain || x[i].exactHandset !== y[i].exactHandset) what.push('exactness digest');
    if (x[i].instructionsMain !== y[i].instructionsMain || x[i].instructionsHandset !== y[i].instructionsHandset) what.push('retire counts');
    if (x[i].lcdSha256 !== y[i].lcdSha256) what.push('LCD');
    if (what.length) out.push(`${x[i].name}: ${what.join(', ')}`);
  }
  return out;
}

async function diveMain(options) {
  const wasm = fs.readFileSync(options.wasm);
  const engine = await SessionEngine.load(new Uint8Array(wasm));
  engine.setFirmware('main', new Uint8Array(fs.readFileSync(options.main)));
  engine.setFirmware('handset', new Uint8Array(fs.readFileSync(options.handset)));
  console.log(`node ${process.version}, wasm ${path.relative(process.cwd(), options.wasm)} (${wasm.length} bytes), ${engine.name}`);
  const modes = options.verifyRoutineAccel ? [true, false] : [options.routineAccel];
  const runs = [];
  for (const accel of modes) {
    const run = runWholeDive(engine, options, accel);
    printDive(run);
    runs.push(run);
  }
  let status = 0;
  let verification = null;
  if (options.verifyRoutineAccel) {
    const diffs = differences(runs[0], runs[1]);
    verification = { identical: diffs.length === 0, differences: diffs };
    console.log(`routine acceleration on/off: ${diffs.length === 0 ? 'IDENTICAL' : 'MISMATCH'} (${checkpointsOf(runs[0]).length} checkpoints compared${diffs.length ? `; ${diffs.join('; ')}` : ''})`);
    runs[0].dives.forEach((on, i) => {
      const off = runs[1].dives[i];
      console.log(`dive ${on.depth}: average ${off.average.factor.toFixed(2)}x -> ${on.average.factor.toFixed(2)}x (${(on.average.factor / off.average.factor).toFixed(2)}x faster), bursts ${off.burst.factor.toFixed(2)}x -> ${on.burst.factor.toFixed(2)}x (${(on.burst.factor / off.burst.factor).toFixed(2)}x faster)`);
    });
    if (diffs.length) status = 1;
  }
  let identicalToNative = null;
  if (options.expect) {
    // Native and WebAssembly runs of the same configuration must produce identical checkpoints.
    const native = JSON.parse(fs.readFileSync(options.expect, 'utf8'));
    const reference = (native.runs || []).find((r) => r.routineAccel === runs[0].routineAccel) || (native.runs || [])[0];
    if (!reference) throw new Error(`${options.expect} has no runs`);
    const asRun = {
      stages: reference.stages.map((s) => ({ checkpoint: s.checkpoint })),
      dives: reference.dives.map((d) => ({ checkpoints: d.checkpoints })),
    };
    const diffs = differences(runs[0], asRun);
    identicalToNative = diffs.length === 0;
    console.log(`native vs wasm checkpoints: ${identicalToNative ? 'IDENTICAL' : 'MISMATCH'} (${checkpointsOf(runs[0]).length} compared${diffs.length ? `; ${diffs.join('; ')}` : ''})`);
    if (!identicalToNative) status = 1;
  }
  if (runs.some((r) => r.dives.some((d) => d.shadowMismatches > 0))) status = 1;
  if (options.json) {
    const result = { engine: engine.name, runtime: { node: process.version, platform: process.platform, arch: process.arch }, benchmark: 'dive (DESIGN.md 16.3)', runs, routineAccelVerification: verification, identicalToNativeCheckpoints: identicalToNative };
    fs.writeFileSync(options.json, `${JSON.stringify(result, null, 2)}\n`);
    console.log(`result written to ${options.json}`);
  }
  process.exit(status);
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.dive) return diveMain(options);
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
  engine.create({ idleFf: options.idleFf, routineAccel: options.routineAccel });
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
