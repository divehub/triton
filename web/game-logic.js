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

  /** Back to 1x with the valves released (a new game session). */
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
const SURFACE_EPSILON_M = 1e-9;

/**
 * Depth, gas and the dive profile, advanced over the emulator's virtual time (never a local clock): `advanceTo(v)` integrates
 * from the virtual time reached so far to `v` in steps of at most 0.25 s, with the motion and valves in force. `elapsed` is the
 * session time, the sum of the virtual time integrated since the last reset (the ADV and vent indicators stamp it).
 *
 * The dive profile records the dive only, like a dive computer. A dive starts when the diver first leaves the surface (the depth
 * goes above 0 m): the profile starts with the surface point at dive time 0, so the time on the boat or floating at the surface
 * before it is not recorded. Reaching the surface again ends the dive: the final surface point is recorded and nothing more is
 * recorded while the diver is at the surface. The next descent starts a new profile, so `profile`, `diveTime` and `maxDepth` always
 * belong to the current or the last dive (empty until the first descent). Long dives keep thinning their samples.
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
    this.direction = 0;
    this.rate = 0;
    this.gas = DILUENTS.air;
    this.loop = createLoop();
    // The current or last dive (none yet): [dive seconds, depth] samples, the dive time and the deepest point, and whether the diver
    // is in the water below the surface now.
    this.profile = [];
    this.diveTime = 0;
    this.maxDepth = 0;
    this.diving = false;
    this.samplePeriod = 1;
    this.nextSample = 1;
    this.lastActivity = { adv: -Infinity, vent: -Infinity };
    this.pendingVent = 0; // gas the loop vented since the last takeVented(), surface-equivalent liters (the water view's bubbles)
  }

  /**
   * The gas the loop vented since the last call (surface-equivalent liters) and nothing else: the loop model's own `vent` (an
   * ascent expanding the loop, or MAV gas beyond what the loop volume takes), summed over the integration steps. A closed loop
   * (holding depth, descending without a MAV beyond its need) returns 0. Reading it changes no physics.
   */
  takeVented() {
    const vented = this.pendingVent;
    this.pendingVent = 0;
    return vented;
  }

  /** The virtual time jumped (a first state, boards recreated): continue from `virtual` without integrating the gap. */
  rebase(virtual) {
    this.virtual = Number.isFinite(virtual) ? virtual : null;
  }

  setGas(key) {
    const gas = DILUENTS[key];
    if (!gas) throw new RangeError(`Unknown diluent ${key}`);
    this.gas = gas;
    setDiluent(this.loop, gas); // the supply only: the loop keeps its gas until an ADV addition or a diluent MAV brings the new mix
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

  /** The diver leaves the surface: a new profile starts at dive time 0 (the earlier dive is forgotten), with its own deepest point. */
  beginDive() {
    this.diving = true;
    this.diveTime = 0;
    this.maxDepth = 0;
    this.profile = [[0, 0]];
    this.samplePeriod = 1;
    this.nextSample = 1;
  }

  /** The diver is back at the surface: the final point (depth 0) is recorded and the profile stays as the last dive. */
  endDive() {
    this.recordProfile(true);
    this.diving = false;
  }

  /** Records a sample of the dive in progress (none at the surface and none before the first dive); `force` records one now. */
  recordProfile(force = false) {
    if (!this.diving || (!force && this.diveTime < this.nextSample)) return;
    const last = this.profile.at(-1);
    if (last[0] !== this.diveTime) this.profile.push([this.diveTime, this.depth]);
    this.nextSample = this.diveTime + this.samplePeriod;
    // Retain the entire time span while thinning long, accelerated dives.
    if (this.profile.length > PROFILE_LIMIT) {
      this.profile = this.profile.filter((_, index) => index % 2 === 0);
      this.samplePeriod *= 2;
    }
  }

  step(dt, valves) {
    // A depth within a nanometer of the surface is the surface (rounding of the 0.25 s steps must not keep a dive open for one more step).
    const moved = this.depth + this.direction * this.rate * dt / 60;
    const newDepth = moved < SURFACE_EPSILON_M ? 0 : Math.min(MAX_DEPTH_METERS, moved);
    if (!this.diving && newDepth > 0) this.beginDive(); // the dive starts at the beginning of the step that leaves the surface
    advanceLoop(this.loop, { depth: newDepth, dt, oxygen: !!valves.oxygen, diluent: !!valves.diluent, flow: this.flow });
    this.elapsed += dt;
    if (this.diving) this.diveTime += dt;
    this.depth = newDepth;
    this.maxDepth = Math.max(this.maxDepth, newDepth);
    if (this.loop.last.adv > 1e-12) this.lastActivity.adv = this.elapsed;
    if (this.loop.last.vent > 1e-12) this.lastActivity.vent = this.elapsed;
    this.pendingVent += this.loop.last.vent;
    if ((this.direction < 0 && newDepth === 0) || (this.direction > 0 && newDepth === MAX_DEPTH_METERS)) {
      this.stopMotion();
      this.onBoundary();
    }
    this.recordProfile();
    if (this.diving && newDepth === 0) this.endDive(); // back at the surface: this dive is over
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

// ---- the water view (DESIGN 22): a following camera, light by depth, the torch, the entry from the boat -------------------
//
// Pure functions and small classes; game-water.js draws with them and game.js puts them on the page. Nothing here reads the
// clock or the DOM, and nothing feeds back into the dive simulation or the emulator: the view is a picture of the depth.

/** The water panel shows a fixed window of this many meters; it no longer zooms out with the maximum depth. */
export const VIEW_WINDOW_M = 30;
/** The air above the surface that stays in view when the camera is at the top (sky and boat), and the sand below the seabed at the bottom. */
export const SKY_M = 6;
export const SAND_M = 4;
/** Everything the water layers draw, from the top of the sky to the bottom of the sand, in meters. */
export const WORLD_M = SKY_M + MAX_DEPTH_METERS + SAND_M;
/** The camera moves only when the diver leaves the middle third of the panel. */
export const DEAD_ZONE = Object.freeze({ top: 1 / 3, bottom: 2 / 3 });
/** The camera takes a few tenths of a second to follow (critically damped). */
export const CAMERA_SMOOTH_S = 0.35;
/** The drawn diver follows the 5 Hz depth updates over this time, so the motion between two states is smooth. */
export const DEPTH_SMOOTH_S = 0.12;

const clamp = (value, low, high) => Math.min(high, Math.max(low, value));

/** The top of the window, in meters of depth (negative is the air above the surface), at its two ends. */
export function cameraLimits() {
  return { min: -SKY_M, max: MAX_DEPTH_METERS + SAND_M - VIEW_WINDOW_M };
}

/**
 * Where the top of the window wants to be for a diver at `depth`, given where it wanted to be before (`previous`): unchanged while
 * the diver is inside the middle third of the window, otherwise just far enough that the diver is on the edge of that third.
 * Clamped at the top (surface and sky in view) and at the bottom (the seabed and some sand in view).
 */
export function cameraTarget(depth, previous = -SKY_M) {
  const { min, max } = cameraLimits();
  if (!Number.isFinite(depth)) return clamp(Number.isFinite(previous) ? previous : min, min, max);
  const start = Number.isFinite(previous) ? previous : min;
  const lowest = depth - DEAD_ZONE.bottom * VIEW_WINDOW_M; // the window top that puts the diver on the lower edge of the zone
  const highest = depth - DEAD_ZONE.top * VIEW_WINDOW_M; // ... on the upper edge
  return clamp(Math.min(Math.max(start, lowest), highest), min, max);
}

/**
 * One critically damped step toward `target`, exact for any `dt` (the closed form of the spring, not an Euler step), so the
 * result does not depend on the frame rate: one step of 0.3 s equals thirty of 0.01 s. `smoothTime` is roughly the time the
 * follow takes. Returns the new `{position, velocity}`.
 */
export function smoothDamp(position, velocity, target, smoothTime, dt) {
  if (!(dt > 0)) return { position, velocity };
  if (!(smoothTime > 1e-4)) return { position: target, velocity: 0 };
  const omega = 2 / smoothTime;
  const offset = position - target;
  const b = velocity + omega * offset;
  const decay = Math.exp(-omega * dt);
  return { position: target + (offset + b * dt) * decay, velocity: (velocity - omega * b * dt) * decay };
}

/** The margin (a fraction of the window) the diver always keeps from the top and the bottom of the panel. */
const KEEP_IN_VIEW = 0.08;

/**
 * The camera: `target` is where the window wants to be (dead zone and clamps), `top` where it is (the target, followed smoothly).
 * `update(depth, dt, {ease: false})` moves without easing (the reduced-motion preference); `dt` 0 (a pause) holds everything.
 */
export class Camera {
  constructor({ smoothTime = CAMERA_SMOOTH_S } = {}) {
    this.smoothTime = smoothTime;
    this.snap(0);
  }

  /** Put the window where it belongs for `depth` at once, the diver in the middle of it (a new session). */
  snap(depth = 0) {
    this.target = cameraTarget(depth, depth - VIEW_WINDOW_M / 2);
    this.top = this.target;
    this.velocity = 0;
  }

  update(depth, dt, { ease = true } = {}) {
    if (!(dt > 0)) return this.top; // paused: the camera holds
    this.target = cameraTarget(depth, this.target);
    if (!ease) {
      this.top = this.target;
      this.velocity = 0;
    } else {
      const next = smoothDamp(this.top, this.velocity, this.target, this.smoothTime, dt);
      // The diver never leaves the window, however fast the clock runs (Uncapped): the follow catches up at the edges.
      const { min, max } = cameraLimits();
      const kept = clamp(next.position, depth - VIEW_WINDOW_M * (1 - KEEP_IN_VIEW), depth - VIEW_WINDOW_M * KEEP_IN_VIEW);
      this.top = clamp(kept, min, max);
      this.velocity = this.top === next.position ? next.velocity : 0;
    }
    return this.top;
  }
}

/** How much of the surface light is left at the top of the window: 1 at the top, 0 once the window is `falloffM` deep. */
export function lightFade(cameraTop, falloffM) {
  return clamp(1 - Math.max(0, cameraTop + SKY_M) / falloffM, 0, 1);
}

// ---- light by depth ----

/** The water color by depth: bright turquoise at the surface, deep blue around 30 to 40 m, near black by 100 m. [meters, [r, g, b]]. */
export const WATER_STOPS = Object.freeze([
  [0, [43, 176, 181]], [6, [31, 150, 165]], [15, [22, 116, 135]], [25, [15, 84, 112]], [35, [11, 61, 95]], [50, [8, 42, 69]],
  [70, [5, 26, 43]], [90, [3, 13, 23]], [100, [2, 7, 13]], [MAX_DEPTH_METERS, [1, 4, 8]],
].map(([meters, color]) => Object.freeze([meters, Object.freeze(color)])));
/** The air above the water, from the top of the sky to the horizon. */
export const SKY_STOPS = Object.freeze([[-SKY_M, [27, 62, 76]], [-3, [58, 128, 136]], [-0.8, [140, 206, 196]], [0, [176, 226, 212]]]
  .map(([meters, color]) => Object.freeze([meters, Object.freeze(color)])));

function mixStops(stops, meters) {
  if (meters <= stops[0][0]) return [...stops[0][1]];
  for (let index = 1; index < stops.length; index++) {
    const [end, to] = stops[index];
    if (meters <= end) {
      const [start, from] = stops[index - 1];
      const fraction = (meters - start) / (end - start);
      return from.map((value, channel) => value + (to[channel] - value) * fraction);
    }
  }
  return [...stops.at(-1)[1]];
}

/** The color of the water at a depth in meters, [r, g, b] (0 to 255). */
export function waterColor(depthM) {
  return mixStops(WATER_STOPS, Number.isFinite(depthM) ? depthM : 0);
}

export function rgbText(color, alpha = 1) {
  const [red, green, blue] = color.map((value) => Math.round(clamp(value, 0, 255)));
  return alpha >= 1 ? `rgb(${red}, ${green}, ${blue})` : `rgba(${red}, ${green}, ${blue}, ${round(alpha, 3)})`;
}

/** The CSS gradient of the whole world, top of the sky to the bottom of the sand; the water gradient has the depth color at each meter. */
export function worldGradient() {
  const at = (meters) => `${round((meters + SKY_M) / WORLD_M * 100, 3)}%`;
  const parts = [];
  for (const [meters, color] of SKY_STOPS) parts.push(`${rgbText(color)} ${at(meters)}`);
  for (const [meters, color] of WATER_STOPS) parts.push(`${rgbText(color)} ${at(meters)}`);
  parts.push(`${rgbText(WATER_STOPS.at(-1)[1])} 100%`);
  return `linear-gradient(to bottom, ${parts.join(', ')})`;
}

/** The depth gauge's gradient: the same colors over the dive range 0 to 110 m. */
export function gaugeGradient() {
  return `linear-gradient(to bottom, ${WATER_STOPS.map(([meters, color]) => `${rgbText(color)} ${round(meters / MAX_DEPTH_METERS * 100, 3)}%`).join(', ')})`;
}

// ---- the torch: on at 40 m, off again above 39 m ----

export const TORCH_ON_M = 40;
export const TORCH_OFF_M = 39;

/** The torch (and the speedometer's glow) after a depth: on from 40 m down, off again only above 39 m, so it never flickers at the boundary. */
export function torchNext(depthM, on) {
  if (!Number.isFinite(depthM)) return !!on;
  return on ? depthM >= TORCH_OFF_M : depthM >= TORCH_ON_M;
}

export const TORCH_CONE = Object.freeze({ length: 320, halfAngle: 0.3 });

/**
 * How much of the torch's light a point gets: 0 outside the cone, up to 1 at the lens. (`dx`, `dy`) is the point relative to the
 * lens in screen pixels, `angle` the way the torch points (radians, 0 along +x, positive down).
 */
export function coneLight(dx, dy, angle, { length = TORCH_CONE.length, halfAngle = TORCH_CONE.halfAngle } = {}) {
  const cos = Math.cos(angle);
  const sin = Math.sin(angle);
  const along = dx * cos + dy * sin;
  if (!(along > 0) || along >= length) return 0;
  const across = Math.abs(dy * cos - dx * sin);
  const half = along * Math.tan(halfAngle) + 5;
  if (across >= half) return 0;
  const reach = 1 - along / length;
  return reach * reach * (1 - across / half);
}

// ---- the entry from the boat ----

/** The first descent plays a back roll off the boat of about this long (wall seconds): purely visual, the depth follows the simulation. */
export const ENTRY_SECONDS = 1.1;
/** The fraction of the entry at which the diver reaches the water (the splash). */
export const ENTRY_SPLASH_AT = 0.5;
/** The diver has left the boat once the depth exceeds this, or a descent is commanded. */
export const ENTRY_TRIGGER_M = 0.02;
/** Where the diver sits on the boat, relative to the pose in the water (pixels; the sprite turned, and a little smaller). */
export const ENTRY_SEAT = Object.freeze({ dx: 84, dy: -58, spin: -52, scale: 0.8 });
const ENTRY_LEAN_END = 0.1; // the fraction of the entry spent sitting and leaning back
const ENTRY_ROLL_END = 0.85; // ... at which the roll has come all the way round
const ENTRY_DIP_PX = 15; // how far under its resting depth the diver plunges before it comes back up

const smoothstep = (value) => {
  const x = clamp(value, 0, 1);
  return x * x * (3 - 2 * x);
};

/**
 * The state of the diver's entry: 'boat' (sitting on the boat; a new session, and so Reset all, starts here), 'entering' (the roll and
 * the splash) and 'water' (swimming, from then on, also back at the surface). `update` reports the phase, the progress (0 to 1)
 * and `splash` (true on the one update that reaches the water). With the reduced-motion preference the diver goes from the boat
 * straight into the water. A `dt` of 0 (a pause) holds the animation.
 */
export class EntryState {
  constructor() {
    this.reset();
  }

  reset() {
    this.phase = 'boat';
    this.elapsed = 0;
    this.splashed = false;
  }

  get progress() {
    return this.phase === 'boat' ? 0 : this.phase === 'water' ? 1 : clamp(this.elapsed / ENTRY_SECONDS, 0, 1);
  }

  update({ dt = 0, depth = 0, direction = 0, reducedMotion = false } = {}) {
    let splash = false;
    if (this.phase === 'boat' && (depth > ENTRY_TRIGGER_M || direction > 0)) {
      this.phase = reducedMotion ? 'water' : 'entering';
    }
    if (this.phase === 'entering' && reducedMotion) this.phase = 'water';
    if (this.phase === 'entering') {
      this.elapsed += Math.max(0, dt);
      if (!this.splashed && this.elapsed / ENTRY_SECONDS >= ENTRY_SPLASH_AT) {
        this.splashed = true;
        splash = true;
      }
      if (this.elapsed >= ENTRY_SECONDS) this.phase = 'water';
    }
    return { phase: this.phase, progress: this.progress, splash };
  }
}

/**
 * The diver's pose during the entry, relative to the pose in the water: pixels right (`dx`) and down (`dy`), the turn (`spin`,
 * degrees, negative is backward) and the size. Progress 0 sits on the boat, then the diver leans back and rolls off, falls,
 * reaches the water at ENTRY_SPLASH_AT, plunges a little and comes back up to the pose 0, 0, 0, 1 at progress 1.
 */
export function entryPose(progress) {
  const p = clamp(progress, 0, 1);
  const fall = clamp((p - ENTRY_LEAN_END) / (ENTRY_SPLASH_AT - ENTRY_LEAN_END), 0, 1);
  const lean = smoothstep(p / ENTRY_LEAN_END);
  const roll = smoothstep((p - ENTRY_LEAN_END) / (ENTRY_ROLL_END - ENTRY_LEAN_END));
  let dx = ENTRY_SEAT.dx * (1 - smoothstep(fall));
  let dy = ENTRY_SEAT.dy * (1 - fall * fall);
  if (p > ENTRY_SPLASH_AT) {
    const rise = smoothstep((p - ENTRY_SPLASH_AT) / (1 - ENTRY_SPLASH_AT));
    dx = 0;
    dy = ENTRY_DIP_PX * Math.sin(Math.PI * rise);
  }
  const start = ENTRY_SEAT.spin - 10 * lean;
  const spin = p <= ENTRY_LEAN_END ? start : start + (-360 - start) * roll;
  return { dx, dy, spin, scale: ENTRY_SEAT.scale + (1 - ENTRY_SEAT.scale) * smoothstep(fall) };
}

// ---- vent bubbles: a closed loop makes none ----

/** One visible bubble stands for this much vented gas (surface-equivalent liters). */
export const SL_PER_BUBBLE = 0.025;

/**
 * Turns the gas the loop vented into whole bubbles: none for no venting, and the count follows the vented volume (the remainder
 * is carried over, so the count over time is the volume divided by SL_PER_BUBBLE however it is split into frames).
 */
export class BubbleEmitter {
  constructor() {
    this.carry = 0;
  }

  reset() {
    this.carry = 0;
  }

  emit(ventedLiters) {
    if (!(ventedLiters > 0)) return 0;
    this.carry += ventedLiters / SL_PER_BUBBLE;
    const count = Math.floor(this.carry);
    this.carry -= count;
    return count;
  }
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
 * decompression state. `action` names the button the game offers (`wake`, `resume`) and `quit` adds a Quit button (a powered-off
 * device is one click from closing the session); each item has a stable `id`.
 */
export function stopAlerts(state) {
  const alerts = [];
  if (!state) return alerts;
  if (state.error) {
    alerts.push({ id: 'error', level: 'error', text: `The emulator stopped: ${String(state.error)}`, action: 'resume' });
  }
  if (state.standby) {
    alerts.push({
      id: 'standby', level: 'warning', action: 'wake', quit: true,
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
      text: "Decompression state invalid: this profile's stored tissues are blank, so the handset's no-decompression limit stays at 99. Use Reset all to begin with an initialized EEPROM.",
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
