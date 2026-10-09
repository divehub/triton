// The dive game's logic without a DOM (DESIGN 21): the oxygen-cell fixture, the depth and gas to sensor inputs, the throttled
// sender of those inputs, the play clock (speed, pause and the MAV override), the dive simulation that follows the emulator's
// virtual time, and the run state and alerts. game.js is the DOM side; the Node tests drive this file directly.
//
// Everything the game shows or sends is a game fixture feeding the emulated sensors: the depth and the loop gas are simulated,
// the cell voltages are a labeled fixture (below), and the firmware's own reading is on the handset. It is a functional model,
// not a claim of physical accuracy.

import { decoWarnings } from './deco.js';
import { faultDetail, faultReports } from './faults.js';
import { describeEntry } from './replay.js';
import { PRESSURE_KEYS, RAW_LIMITS, pressureMbar } from './sensors.js';
import {
  DEFAULT_MAV_FLOW_SL_MIN, MAX_DEPTH_METERS, SURFACE_PRESSURE_BAR, advanceLoop, createLoop, getLoopReadings, setDiluent,
} from './game-gas.js';

// ---- the oxygen cells: a labeled game fixture --------------------------------------------------------------------------

/**
 * Each cell reads 12 mV in air at the surface (the loop's ppO2 of 0.2128 bar at the model's surface reference), plus a random
 * per-cell deviation, uniform within +-1.0 mV (so 11.0 to 13.0 mV). The deviation is drawn once per profile and kept in the page
 * settings for that profile area, so a calibration made through the firmware menu stays valid across sessions like a real cell;
 * a profile reset draws new ones. Cell voltage = ppO2 x sensitivity, linear and without drift.
 */
export const CELL_COUNT = 3;
export const CELL_AIR_MV = 12;
export const CELL_DEVIATION_MV = 1;
export const AIR_O2_FRACTION = 0.21;
export const AIR_SURFACE_PPO2 = AIR_O2_FRACTION * SURFACE_PRESSURE_BAR;
const CELL_STORE_VERSION = 1;

const round = (value, decimals) => Math.round(value * 10 ** decimals) / 10 ** decimals;

/** The page-settings key of a profile area's cell fixture (`localStorage`, through `prefs`, which adds its own prefix). */
export function cellFixtureKey(area) {
  return `game-cells.${area}`;
}

/** Three deviations, uniform in [-1, +1] mV, to 0.001 mV. `random` returns a number in [0, 1). */
export function drawCellDeviations(random = Math.random) {
  return Array.from({ length: CELL_COUNT }, () => round((random() * 2 - 1) * CELL_DEVIATION_MV, 3));
}

function validDeviations(value) {
  return Array.isArray(value) && value.length === CELL_COUNT
    && value.every((item) => typeof item === 'number' && Number.isFinite(item) && Math.abs(item) <= CELL_DEVIATION_MV + 1e-9);
}

/**
 * The cell fixture of a profile area: the stored deviations, or (none stored, or damaged) newly drawn ones that are stored.
 * `store` is `{get(key, fallback), set(key, value)}` (dom.js `prefs`). Returns `{deviationsMv, drawn}`.
 */
export function loadCellFixture(store, area, random = Math.random) {
  const key = cellFixtureKey(area);
  try {
    const stored = JSON.parse(store.get(key, ''));
    if (stored && stored.version === CELL_STORE_VERSION && validDeviations(stored.deviationsMv)) {
      return { deviationsMv: [...stored.deviationsMv], drawn: false };
    }
  } catch (_) { /* nothing stored, or not readable: draw */ }
  const deviationsMv = drawCellDeviations(random);
  store.set(key, JSON.stringify({ version: CELL_STORE_VERSION, deviationsMv }));
  return { deviationsMv, drawn: true };
}

/** A profile reset forgets the area's deviations; the next game session draws new ones. */
export function clearCellFixture(store, area) {
  if (typeof store.remove === 'function') store.remove(cellFixtureKey(area));
  else store.set(cellFixtureKey(area), '');
}

/** mV per bar of ppO2, per cell. */
export function cellSensitivities(deviationsMv) {
  return deviationsMv.map((deviation) => (CELL_AIR_MV + deviation) / AIR_SURFACE_PPO2);
}

/** The voltage of each cell for a loop ppO2 (bar), within the engine's input range (a real cell saturates as well). */
export function cellMillivolts(ppo2, sensitivities) {
  return sensitivities.map((sensitivity) => Math.min(RAW_LIMITS.oxygen[1], Math.max(RAW_LIMITS.oxygen[0], ppo2 * sensitivity)));
}

// ---- depth and gas to the engine's sensor inputs -----------------------------------------------------------------------

/** Absolute pressure (mbar) at a depth: the session's surface pressure plus EN13319 water (1020 kg/m3, 9.80665 m/s2). */
export function ambientMbar(surfaceMbar, depthM) {
  return pressureMbar(surfaceMbar, depthM, 'en13319');
}

/**
 * The `inputs` the game sends: both pressure inputs get the same absolute pressure (through the page's sensor numbering,
 * sensors.js `PRESSURE_KEYS`) and the three cells their voltage. The temperature inputs are not part of it: they keep the value
 * the emulator has.
 */
export function gameInputs({ surfaceMbar, depthM, ppo2, sensitivities }) {
  const pressure = round(ambientMbar(surfaceMbar, depthM), 2);
  const inputs = {};
  for (const key of PRESSURE_KEYS) inputs[key] = pressure;
  cellMillivolts(ppo2, sensitivities).forEach((millivolts, index) => { inputs[`oxygen${index + 1}Mv`] = round(millivolts, 3); });
  return inputs;
}

/**
 * Sends the game's inputs coalesced and serialized (DESIGN 21.2): the newest wins, at most one send per `minIntervalMs`, and
 * nothing is sent while the values equal the last ones sent. `send(inputs)` hands the payload to the action queue (which also
 * serializes and coalesces what is still waiting there). The game never writes guest RAM.
 */
export const INPUT_MIN_INTERVAL_MS = 250;

export class InputsSender {
  constructor({ send, now, setTimer, clearTimer, minIntervalMs = INPUT_MIN_INTERVAL_MS }) {
    this.send = send;
    this.now = now || (() => Date.now());
    this.setTimer = setTimer || ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = clearTimer || ((handle) => clearTimeout(handle));
    this.minIntervalMs = minIntervalMs;
    this.latest = null;
    this.latestKey = null;
    this.sentKey = null;
    this.lastSendAt = -Infinity;
    this.timer = null;
    this.sends = 0;
  }

  /** The newest wanted inputs. `immediate` skips the interval (a new session, a board creation). */
  offer(inputs, { immediate = false } = {}) {
    this.latest = inputs;
    this.latestKey = JSON.stringify(inputs);
    if (this.latestKey === this.sentKey) {
      this.cancelTimer(); // what was waiting is superseded by what the engine already has
      return;
    }
    const wait = immediate ? 0 : Math.max(0, this.lastSendAt + this.minIntervalMs - this.now());
    if (wait === 0) {
      this.flush();
    } else if (this.timer === null) {
      this.timer = this.setTimer(() => {
        this.timer = null;
        this.flush();
      }, wait);
    }
  }

  flush() {
    this.cancelTimer();
    if (this.latest === null || this.latestKey === this.sentKey) return;
    this.sentKey = this.latestKey;
    this.lastSendAt = this.now();
    this.sends += 1;
    this.send(this.latest);
  }

  /** The engine may no longer have what was sent (a board creation, a failed action): the next offer sends again. */
  invalidate() {
    this.sentKey = null;
  }

  cancelTimer() {
    if (this.timer !== null) {
      this.clearTimer(this.timer);
      this.timer = null;
    }
  }

  cancel() {
    this.cancelTimer();
    this.latest = null;
    this.latestKey = null;
    this.sentKey = null;
  }
}

// ---- the play clock: speed, pause and the MAV override ------------------------------------------------------------------

export const SPEEDS = Object.freeze([0, 1, 2, 4, 'uncapped']);

export function speedLabel(speed) {
  return speed === 'uncapped' ? '∞' : speed === 0 ? 'Pause' : `${speed}×`;
}

/**
 * Pause / 1x / 2x / 4x / Uncapped and the MAV override, with the prototype's semantics: holding a valve (from any source: a
 * pointer, a focused key, the O and D shortcuts) runs the whole clock at 1x, also over Uncapped, and the chosen speed is kept
 * and restored when the last source lets go; choosing another speed while injecting changes the speed that is restored; Pause
 * releases the valves and stays paused; a valve cannot be held while paused.
 *
 * `effective()` is the speed the emulator is paced at: 0 (the engine paused), a number (virtual seconds per wall second) or
 * 'uncapped' (unpaced). `onChange` runs after every change.
 */
export class PlayClock {
  constructor({ onChange } = {}) {
    this.onChange = onChange || (() => {});
    this.speed = 1;
    this.previousSpeed = 1;
    this.held = { oxygen: new Set(), diluent: new Set() };
  }

  injecting() {
    return this.held.oxygen.size > 0 || this.held.diluent.size > 0;
  }

  effective() {
    return this.speed === 0 ? 0 : this.injecting() ? 1 : this.speed;
  }

  /** The speed that comes back when the valves let go (what the menu shows as selected). */
  returnLabel() {
    return this.speed === 'uncapped' ? 'Uncapped' : speedLabel(this.speed);
  }

  setSpeed(speed) {
    if (!SPEEDS.includes(speed)) throw new RangeError(`Unknown play speed ${speed}`);
    const wasInjecting = this.injecting();
    this.speed = speed;
    if (speed !== 0) this.previousSpeed = speed;
    // A pause releases the valves, so resuming cannot leave a MAV stuck.
    if (speed === 0) this.releaseValves({ silent: true });
    this.onChange({ injectingChanged: wasInjecting !== this.injecting() });
  }

  togglePause() {
    this.setSpeed(this.speed === 0 ? this.previousSpeed : 0);
  }

  /** Back to 1x with the valves released (Reset dive). */
  reset() {
    this.releaseValves({ silent: true });
    this.setSpeed(1);
  }

  /** A source starts or stops holding a valve; returns whether the injection state changed. */
  hold(gas, source, active) {
    if (!this.held[gas]) throw new RangeError(`Unknown valve ${gas}`);
    if (active && this.speed === 0) return false; // a paused clock injects nothing
    const wasInjecting = this.injecting();
    if (active) this.held[gas].add(source);
    else this.held[gas].delete(source);
    const injectingChanged = wasInjecting !== this.injecting();
    this.onChange({ injectingChanged });
    return injectingChanged;
  }

  releaseValves({ silent = false } = {}) {
    const wasInjecting = this.injecting();
    this.held.oxygen.clear();
    this.held.diluent.clear();
    if (!silent) this.onChange({ injectingChanged: wasInjecting });
  }
}

// ---- the dive simulation, on the emulator's virtual time ----------------------------------------------------------------

export const DILUENTS = Object.freeze({
  air: Object.freeze({ name: 'Air', o2: 0.21, he: 0 }),
  tx2135: Object.freeze({ name: 'Trimix 21/35', o2: 0.21, he: 0.35 }),
  tx1845: Object.freeze({ name: 'Trimix 18/45', o2: 0.18, he: 0.45 }),
  tx1050: Object.freeze({ name: 'Trimix 10/50', o2: 0.10, he: 0.50 }),
});

export const MAX_STEP_SECONDS = 0.25;
export const MAX_ASCENT_M_MIN = 18;
export const MAX_DESCENT_M_MIN = 30;
const PROFILE_LIMIT = 2400;

/**
 * Depth, gas and the dive profile, advanced over the emulator's virtual time (never a local clock): `advanceTo(v)` integrates
 * from the virtual time reached so far to `v` in steps of at most 0.25 s, with the motion and valves in force. `elapsed` is the
 * dive time (what the profile chart plots), the sum of the virtual time integrated since the last reset.
 */
export class GameSim {
  constructor({ flow = DEFAULT_MAV_FLOW_SL_MIN, onBoundary } = {}) {
    this.flow = flow;
    this.onBoundary = onBoundary || (() => {});
    this.reset();
  }

  /** Back to the surface with a fresh Air loop; `virtual` (when given) is the virtual time the dive starts at. */
  reset(virtual = this.virtual ?? null) {
    this.elapsed = 0;
    this.virtual = virtual;
    this.depth = 0;
    this.maxDepth = 0;
    this.direction = 0;
    this.rate = 0;
    this.gas = DILUENTS.air;
    this.loop = createLoop();
    this.profile = [[0, 0]];
    this.samplePeriod = 1;
    this.nextSample = 1;
    this.lastActivity = { adv: -Infinity, vent: -Infinity };
  }

  /** The virtual time jumped (a first state, boards recreated): continue from `virtual` without integrating the gap. */
  rebase(virtual) {
    this.virtual = Number.isFinite(virtual) ? virtual : null;
  }

  setGas(key) {
    const gas = DILUENTS[key];
    if (!gas) throw new RangeError(`Unknown diluent ${key}`);
    this.gas = gas;
    setDiluent(this.loop, gas, this.depth);
    this.lastActivity = { adv: -Infinity, vent: -Infinity };
  }

  setMotion(direction, rate) {
    if (direction !== this.direction) this.recordProfile(true);
    this.direction = direction;
    this.rate = rate;
  }

  /** Signed rate in m/min (negative ascends), clamped to 18 up and 30 down, in 0.1 m/min increments. */
  setMotionRate(signedRate) {
    const rate = Math.round(Math.max(-MAX_ASCENT_M_MIN, Math.min(MAX_DESCENT_M_MIN, signedRate)) * 10) / 10;
    this.setMotion(Math.sign(rate), Math.abs(rate));
  }

  stopMotion() {
    this.setMotion(0, 0);
  }

  recordProfile(force = false) {
    if (!force && this.elapsed < this.nextSample) return;
    const last = this.profile.at(-1);
    if (last[0] !== this.elapsed) this.profile.push([this.elapsed, this.depth]);
    this.nextSample = this.elapsed + this.samplePeriod;
    // Retain the entire time span while thinning long, accelerated dives.
    if (this.profile.length > PROFILE_LIMIT) {
      this.profile = this.profile.filter((_, index) => index % 2 === 0);
      this.samplePeriod *= 2;
    }
  }

  step(dt, valves) {
    const newDepth = Math.max(0, Math.min(MAX_DEPTH_METERS, this.depth + this.direction * this.rate * dt / 60));
    advanceLoop(this.loop, { depth: newDepth, dt, oxygen: !!valves.oxygen, diluent: !!valves.diluent, flow: this.flow });
    this.elapsed += dt;
    this.depth = newDepth;
    this.maxDepth = Math.max(this.maxDepth, newDepth);
    if (this.loop.last.adv > 1e-12) this.lastActivity.adv = this.elapsed;
    if (this.loop.last.vent > 1e-12) this.lastActivity.vent = this.elapsed;
    if ((this.direction < 0 && newDepth === 0) || (this.direction > 0 && newDepth === MAX_DEPTH_METERS)) {
      this.stopMotion();
      this.onBoundary();
    }
    this.recordProfile();
  }

  /**
   * Integrates up to virtual time `virtual` with the given valves. A time at or before the one reached integrates nothing (a
   * settle at an input change may have gone slightly past the state that follows it). Returns the seconds integrated.
   */
  advanceTo(virtual, valves = {}) {
    if (!Number.isFinite(virtual)) return 0;
    if (this.virtual === null) {
      this.virtual = virtual;
      return 0;
    }
    const total = virtual - this.virtual;
    if (!(total > 1e-9)) return 0;
    let remaining = total;
    while (remaining > 1e-9) {
      const dt = Math.min(MAX_STEP_SECONDS, remaining);
      this.step(dt, valves);
      remaining -= dt;
    }
    this.virtual = virtual;
    return total;
  }

  /** A detached snapshot for the screen. The ambient pressure and the ppO2 follow the pressure sent to the firmware. */
  readings(surfaceMbar) {
    const loop = getLoopReadings(this.loop);
    const ambientBar = ambientMbar(surfaceMbar, this.depth) / 1000;
    return { ...loop, ambientBar, ppo2: loop.fractions.o2 * ambientBar };
  }
}

/**
 * The virtual time "now", estimated from the last state (its virtual time and when it arrived) for a paced run, so an input
 * change integrates up to the moment it happens rather than to the next state. At most `capSeconds` ahead; no extrapolation when
 * the clock is paused, unpaced or the pace is unknown.
 */
export function estimateVirtual({ lastVirtual, lastWallMs, nowMs, pace, capSeconds = 0.35 }) {
  if (!Number.isFinite(lastVirtual)) return null;
  if (typeof pace !== 'number' || !(pace > 0)) return lastVirtual;
  const ahead = Math.max(0, (nowMs - lastWallMs) / 1000) * pace;
  return lastVirtual + Math.min(capSeconds, ahead);
}

// ---- run state, stops and alerts ---------------------------------------------------------------------------------------

/**
 * The header's run state for a state document and the host status: `{key, text, tone}` with tone 'ok', 'idle', 'warn' or 'bad'.
 * A standby or an error stop is the engine's own (not the game's Pause), and a connection error overrides everything.
 */
export function runState(state, host, { connectionError = false } = {}) {
  if (connectionError) return { key: 'disconnected', text: 'Disconnected', tone: 'bad' };
  if (!state) return { key: 'starting', text: 'Starting', tone: 'idle' };
  const info = host || {};
  if (state.standby) return { key: 'standby', text: 'Standby', tone: 'warn' };
  if (state.error) return { key: 'error', text: 'Stopped by an error', tone: 'bad' };
  if (info.suspended) return { key: 'suspended', text: 'Suspended (background tab)', tone: 'idle' };
  if (!state.running) return { key: 'paused', text: 'Paused', tone: 'idle' };
  if (info.speed !== null && info.speed !== undefined && info.keepingUp === false) return { key: 'behind', text: 'Running, not keeping up', tone: 'warn' };
  return { key: 'running', text: 'Running', tone: 'ok' };
}

/**
 * Messages the game must not hide (no silent freeze): the engine's error text, a standby, a CPU fault, a proven invalid
 * decompression state. `action` names the button the game offers (`wake`, `resume`); each item has a stable `id`.
 */
export function stopAlerts(state) {
  const alerts = [];
  if (!state) return alerts;
  if (state.error) {
    alerts.push({ id: 'error', level: 'error', text: `The emulator stopped: ${String(state.error)}`, action: 'resume' });
  }
  if (state.standby) {
    alerts.push({
      id: 'standby', level: 'warning', action: 'wake',
      text: 'The firmware requested standby, so the emulated unit is powered down and the dive clock has stopped. Wake it to continue.',
    });
  }
  for (const report of faultReports(state).filter((entry) => entry.active)) {
    const name = report.board === 'main' ? 'Main' : 'Handset';
    alerts.push({
      id: `fault-${report.board}`, level: 'error',
      text: `${name} CPU fault: ${faultDetail(report)}. The emulated board may misbehave; Quit and restart the session if the unit stops responding.`,
    });
  }
  if (decoWarnings(state).some((warning) => warning.id === 'tissues')) {
    alerts.push({
      id: 'deco-tissues', level: 'warning',
      text: "Decompression state invalid: this profile's stored tissues are blank, so the handset's no-decompression limit stays at 99. Quit and reset the saved profile on the start screen to begin with an initialized EEPROM.",
    });
  }
  return alerts;
}

/** What one of the three indicators (vibrator, red and white HUD LED) shows for a replay entry: steady drive or a replayed pulse. */
export const GAME_INDICATORS = Object.freeze({ 'handset-vibrator': 'vibrator', 'main-hud-3': 'red', 'main-hud-2': 'white' });

export function indicatorView(entry) {
  const view = describeEntry(entry, true);
  const state = entry.active === null ? 'unknown' : view.replayOn ? 'pulse' : entry.active ? 'on' : 'off';
  return { lit: view.on, state, text: view.simple ? view.simple.stateText : state, title: view.title };
}

// ---- small formatting helpers ------------------------------------------------------------------------------------------

export function clockText(seconds) {
  const whole = Math.max(0, Math.floor(Number.isFinite(seconds) ? seconds : 0));
  return [Math.floor(whole / 3600), Math.floor(whole / 60) % 60, whole % 60].map((value) => String(value).padStart(2, '0')).join(':');
}

export function durationText(seconds) {
  const whole = Math.max(0, Math.floor(Number.isFinite(seconds) ? seconds : 0));
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`;
}
