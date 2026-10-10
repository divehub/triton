// The dive game screen (DESIGN 21): a game on top of the same session the emulator view runs.
//
// The game owns no clock, no LCD and no cells of its own. Its depth, gas and profile advance over the emulator's virtual time
// (the `virtualTime` of every state the worker publishes); Pause / 1x / 2x / 4x / Uncapped set the emulator's pacing; the depth
// and the loop gas become the emulated pressure and oxygen-cell inputs, sent through the `inputs` action (coalesced, serialized,
// a few times a second); the handset is operated through its pins by the same actions the emulator view uses (Up, Down,
// Confirm), and its display, vibrator and HUD LEDs are the firmware's, shown with the existing renderer and replay logic.
// The game never writes guest RAM.
//
// The decisions live in game-logic.js (DOM-free, tested in Node); this file is the DOM side: markup in index.html (#screen-game,
// every id starts with `game-`), rules in game.css (every selector starts with #screen-game).

import { ActionQueue } from './conditions.js';
import { DEFAULT_SURFACE_MBAR, parseSurfacePressure } from './deco.js';
import { byId, confirmDialog, prefs, setText } from './dom.js';
import { DEFAULT_MAV_FLOW_SL_MIN, MAX_DEPTH_METERS } from './game-gas.js';
import {
  CELL_FAULTS, CellFixture, ENTRY_SEAT, GAME_INDICATORS, GameSim, InputsSender, PlayClock, SKY_M, VIEW_WINDOW_M, cellFault, clearCellFixture,
  clockText, durationText, estimateVirtual, gameInputs, gaugeGradient, indicatorView, loadCellFixture, loopPpo2, runState, speedLabel,
  stopAlerts, worldGradient,
} from './game-logic.js';
import { createGameSound } from './game-sound.js';
import { DIVER_X, MAX_FRAME_STEP_S, WaterScene } from './game-water.js';
import { handsetKeyAction, isHandsetArrow } from './keys.js';
import { LcdView } from './lcd.js';
import { DEFAULT_RELEASE_ID, describeRelease, profileArea } from './releases.js';
import { ReplayController } from './replay.js';

const $ = (id) => byId(`game-${id}`);
const SVG_NS = 'http://www.w3.org/2000/svg';
const DEFAULT_TITLE = 'NGC system emulator · WebAssembly';
const PRESS_FEEDBACK_MS = 160;
// The far layer (rays and distant shapes) moves at this fraction of the camera's speed: parallax. The reduced-motion preference has none.
const FAR_PARALLAX = 0.45;
// Where the diver sits on the boat (the stern platform) in the boat's drawing, as a fraction of its width, and the boat's size limits (px).
const BOAT_SEAT_FRACTION = 0.1125;
const BOAT_MAX_WIDTH = 330;
const BOAT_MIN_WIDTH = 200;
const GAUGE_LABELS = [0, 30, 60, 90, 110];

/** The browser's animation frames, or null where there are none (the Node tests inject their own). */
function browserFrames() {
  if (typeof requestAnimationFrame !== 'function') return null;
  return { request: (callback) => requestAnimationFrame(callback), cancel: (handle) => cancelAnimationFrame(handle) };
}
const INDICATOR_NAMES = { vibrator: 'Handset vibrator', red: 'Red HUD LED (HUD 3)', white: 'White HUD LED (HUD 2)' };
// W / S swimming: the speed tiers in m/min (hold, double tap and hold, triple tap and hold), up to the gesture maxima of
// 18 m/min ascending and 30 m/min descending; a press counts as a tap when it was shorter than the tap hold and the next press
// follows its release within the gap.
const SWIM_KEYS = { KeyW: 'up', KeyS: 'down' };
const SWIM_RATES = { up: [6, 12, 18], down: [10, 20, 30] };
const SWIM_TIERS = 3;
const SWIM_TAP_GAP_MS = 300;
const SWIM_TAP_HOLD_MS = 300;
// Reset all (DESIGN 23): the confirmation says what is erased.
const RESET_ALL_TITLE = 'Reset all?';
const RESET_ALL_MESSAGE = "This erases the dive computer's memory: settings, calibration, logbook and clock. Both boards restart as new, and the diver returns to the boat.";
// Quit treats the device as a black box: it asks first only while the engine does not report the device in standby (a hardware
// state, the same for original and custom builds), and never relies on the firmware's own power-off.
const QUIT_TITLE = 'The dive computer is still on';
const QUIT_MESSAGE = 'Turn it off on the device first, then quit. Or force a shutdown: the session closes now, and anything the device has not saved yet may be lost.';
// The controls guide opens by itself on the first game start in a browser; this page setting (localStorage, through the store) remembers it.
const HELP_SEEN_KEY = 'game-help-seen';
// Sound (game-sound.js, synthesized): the header control's two settings are page settings too (on or off, and the volume in percent).
const SOUND_ON_KEY = 'game-sound-on';
const SOUND_VOLUME_KEY = 'game-sound-volume';
const SOUND_DEFAULT_VOLUME = 60;
const SOUND_TAIL_MS = 1500; // a closed session lets its last sound (the power-down tone) ring out before the sound is disposed
const VIBRATE_PULSE_MS = 50; // one firmware pulse; the sound makes at least 120 ms of it
const VIBRATE_HOLD_MS = 250; // a vibrator held on keeps its buzz going: each state (every 200 ms) asks for this much from now
const SWIM_SOUND_STEP = 0.5; // m/min: the swim sound hears the speed in steps this big
const VENT_SOUND_STEP = 0.05; // SL/s: and the vent rate in steps this big (anything under half a step is silence)
const VENT_SOUND_MIN_DEPTH_M = 0.15; // the water scene's own limit: shallower than this its bubbles vanish, and the vent is not heard

function svgNode(tag, attributes, content) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, value);
  if (content !== undefined) node.textContent = content;
  return node;
}

/** Shows or hides a part of an SVG: SVG elements have no `hidden` property, only the attribute (which game.css honors). */
function setSvgHidden(node, hidden) {
  node.hidden = hidden; // an HTML element (and the Node tests' DOM) takes the property
  if (hidden) node.setAttribute('hidden', '');
  else node.removeAttribute('hidden');
}

function element(tag, attributes = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (name === 'class') node.className = value;
    else node.setAttribute(name, String(value));
  }
  node.append(...children);
  return node;
}

/**
 * The Quit confirmation. It has the markup and the style of the page's `confirmDialog` (the Reset all confirmation), but the game
 * can also close it from outside: when the device turns itself off while it is open. `answer` resolves to 'back' (the default and
 * the Escape key), 'force', or the reason given to `close`. Without <dialog> support it asks with `window.confirm`, as
 * `confirmDialog` does (a blocking prompt cannot be closed from outside).
 */
function openQuitDialog() {
  if (typeof HTMLDialogElement === 'undefined' || typeof HTMLDialogElement.prototype.showModal !== 'function') {
    return { answer: Promise.resolve(window.confirm(`${QUIT_TITLE}\n\n${QUIT_MESSAGE}`) ? 'force' : 'back'), close() {} };
  }
  let settle;
  const answer = new Promise((resolve) => { settle = resolve; });
  let open = true;
  const dialog = element('dialog', { class: 'dialog', 'aria-labelledby': 'dialog-title' });
  const close = (reason) => {
    if (!open) return;
    open = false;
    dialog.close();
    dialog.remove();
    settle(reason);
  };
  const back = element('button', { type: 'button', autofocus: '' }, 'Back to the dive'); // the safe default has the focus
  const force = element('button', { type: 'button', class: 'danger' }, 'Force shutdown');
  back.addEventListener('click', () => close('back'));
  force.addEventListener('click', () => close('force'));
  dialog.addEventListener('cancel', (event) => {
    event.preventDefault();
    close('back');
  });
  dialog.append(
    element('h2', { id: 'dialog-title' }, QUIT_TITLE),
    element('p', {}, QUIT_MESSAGE),
    element('div', { class: 'actions-row' }, back, force),
  );
  document.body.append(dialog);
  dialog.showModal();
  return { answer, close };
}

export class GameView {
  /**
   * @param {import('./worker-client.js').WorkerClient} client
   * @param {{quit: () => (void|Promise<void>)}} hooks `quit` closes the session (profile saved) and leaves the game
   * @param {{timers?: {setTimer: Function, clearTimer: Function}, now?: () => number, random?: () => number, store?: object,
   *   frames?: ({request: Function, cancel: Function}|null), motion?: ({matches: boolean}|null), createSound?: () => object}} [options]
   *   the tests inject timers, a clock, a random source, the settings store, the animation frames (`null` is none), the
   *   reduced-motion media query (`null` is none) and the sound (an object with the calls of game-sound.js)
   */
  constructor(client, hooks, { timers, now, random, store, frames, motion, createSound } = {}) {
    this.client = client;
    this.hooks = hooks;
    this.root = byId('screen-game');
    this.store = store || prefs;
    this.createSound = createSound || createGameSound;
    this.sound = null; // from Start game to Quit (see `unlockSound`); the page without Web Audio runs silently
    this.soundSettings = this.loadSoundSettings();
    this.soundOpen = false; // the sound popover is open: like the controls guide, it holds the keyboard
    this.random = random || Math.random;
    this.now = now || (() => performance.now());
    this.setTimer = (timers && timers.setTimer) || ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = (timers && timers.clearTimer) || ((handle) => clearTimeout(handle));
    // The water view (DESIGN 22): a scene drawn on animation frames while the game is on screen.
    this.frames = frames === undefined ? browserFrames() : frames;
    this.frameHandle = null;
    this.lastFrameAt = null;
    this.scene = new WaterScene({ random: this.random });
    this.layout = { width: 0, height: 0, ppm: 0 };
    this.layoutStale = true;
    this.gaugeHeight = 0;
    this.painted = new Map(); // what was last written to the page: unchanged values are not written again
    this.waterContext = null;
    this.resizeObserver = null;
    this.lcd = new LcdView({ container: $('lcd'), canvas: $('frame'), placeholder: $('placeholder'), sizeLabel: $('frame-size') });
    this.lcd.setIntegerScaling(false);
    this.replay = new ReplayController({
      setTimer: this.setTimer,
      clearTimer: this.clearTimer,
      enabled: () => true, // the pulse replay of the emulator view's default; the game has no switch for it
      paused: () => document.hidden || !!this.connectionError,
      draw: (entry) => this.paintIndicator(entry),
    });
    this.active = false;
    this.closing = false;
    this.resetting = false; // Reset all is under way: the old session is being discarded, so nothing is sent to it
    this.modalOpen = false; // a confirmation (Reset all or Quit) is open: the keyboard belongs to it
    this.quitDialog = null; // the open Quit confirmation: `close(reason)` ends it from outside (the device turned off, the game ended)
    this.helpOpen = false; // the controls guide is open: the keyboard is the guide's, not the game's
    this.helpShown = false; // the guide has been shown on its own in this page (when the settings cannot remember it)
    this.cellMenu = null; // the cell (0 to 2) whose fault menu is open: like the controls guide, it holds the keyboard
    this.notices = [];
    this.sender = null;
    this.pressTimers = new Map();
    this.motionPointer = null;
    this.motionOrigin = null;
    this.motionReach = 180;
    this.swimHeld = new Map(); // W / S held: the speed tier (1 to 3) of each
    this.swimTaps = { KeyW: 0, KeyS: 0 };
    this.swimReleased = { KeyW: -Infinity, KeyS: -Infinity };
    this.swimPressed = { KeyW: -Infinity, KeyS: -Infinity };
    this.previousPaint = '';
    this.alertsKey = '';
    // The worker sends the first LCD frame while it creates the session, so it can reach `onFrame` before `show` runs: frames
    // seen so far are kept and forgotten only when the session ends (`hide`), as in the emulator view.
    this.haveFrame = false;
    this.frameCount = 0;
    this.beginSession();
    this.buildCellMenu();
    this.buildWater();
    this.watchMotion(motion);
    this.wire();
  }

  // ---- session ---------------------------------------------------------------------------------------------------

  /** Everything that belongs to one session: a new simulation, clock, action queue and input sender. */
  beginSession({ startPaused = false } = {}) {
    if (this.sender) this.sender.cancel();
    this.surfaceMbar = DEFAULT_SURFACE_MBAR;
    this.cells = null; // the session's oxygen cells and their simulated faults (`show`): a new session starts with every cell normal
    this.sim = new GameSim({
      flow: DEFAULT_MAV_FLOW_SL_MIN,
      onBoundary: () => this.clearGesture(),
      onIntegrate: (dt, from, to) => this.advanceCells(dt, from, to),
    });
    this.scene.reset(); // the diver is on the boat
    this.clock = new PlayClock({ onChange: (change) => this.clockChanged(change) });
    if (startPaused) this.clock.speed = 0;
    this.queue = new ActionQueue({
      // While Reset all discards the session, an action has nothing to act on: it is dropped without an error (the new session starts clean).
      perform: (payload) => (this.resetting ? Promise.resolve({}) : this.client.request('action', { request: payload })),
      before: (request) => {
        if (request.action === 'wake') this.replay.clear(); // the boards are recreated: the output histories start over
        if (request.action !== 'inputs') this.actionError = '';
      },
      onResult: (request) => {
        if (request.action === 'inputs' && this.inputsError) {
          this.inputsError = '';
          this.renderAlerts();
        }
      },
      onError: (request, error) => {
        const message = (error && error.message) || 'The emulator action failed.';
        if (request.action === 'inputs') {
          this.inputsError = message;
          this.sender.invalidate(); // the engine may not have it: the next state sends the inputs again
        } else {
          this.actionError = message;
        }
        this.renderAlerts();
      },
    });
    this.sender = new InputsSender({
      send: (inputs) => this.queue.send('inputs', { inputs }, { kind: 'basic' }),
      now: () => this.now(),
      setTimer: this.setTimer,
      clearTimer: this.clearTimer,
    });
    this.state = null;
    this.host = null;
    this.info = null;
    this.release = describeRelease(DEFAULT_RELEASE_ID);
    this.lastVirtual = null; // the virtual time of the newest state, and when it arrived
    this.lastWall = 0;
    this.lastGeneration = null;
    this.pacing = null; // what the emulator was last paced at: 0 (paused), a speed, 'uncapped'; null before the first state
    this.paceToken = 0;
    this.injectionLive = false; // the valves count once the emulator is confirmed to run at 1x
    this.actionError = '';
    this.inputsError = '';
    this.connectionError = '';
    // What the sound was last told (it is told only what changed): on the boat, nothing swimming, venting or held.
    this.soundMemo = { wet: false, below: false, paused: false, swim: 0, vent: 0, mav: null, torch: false, buzzing: false };
  }

  /**
   * The session has started (`info` is what the entry screen booted with: options, release, custom). The first state follows
   * from the caller (`onState`).
   */
  show(info) {
    this.beginSession({ startPaused: !!(info.options && info.options.startPaused) });
    this.active = true;
    this.closing = false;
    this.info = info;
    this.release = info.release || describeRelease(DEFAULT_RELEASE_ID);
    const options = info.options || {};
    // The session's surface pressure (the page's remembered setting); the sensors get it plus the water.
    const surface = parseSurfacePressure(options.surfacePressureMbar);
    this.surfaceMbar = surface === null ? DEFAULT_SURFACE_MBAR : surface;
    // The profile's cells (kept in the page settings); their simulated faults belong to this session only and are never stored.
    this.cells = new CellFixture(loadCellFixture(this.store, profileArea(this.release.id), this.random));
    this.root.hidden = false;
    this.replay.clear();
    this.resetIndicators();
    if (!this.haveFrame) this.lcd.setVisible(false, 'Waiting for LCD output');
    $('diluent-select').value = 'air';
    $('mav-flow').value = String(DEFAULT_MAV_FLOW_SL_MIN);
    const name = this.releaseName();
    setText($('release'), this.release.custom ? 'Custom build' : this.release.label);
    setText($('wrist-name'), name);
    setText($('device-name'), name.toUpperCase());
    document.title = `NGC dive game · ${name} · WebAssembly`;
    this.renderFixture();
    // The worker keeps the console text back unless the console is on screen; the game never shows it.
    this.client.send('ui', { uartOpen: false });
    this.startSoundSession();
    this.render();
    this.startWater();
    this.showHelpFirstTime();
  }

  hide() {
    if (this.quitDialog) this.quitDialog.close('hidden'); // the session is gone: nothing is left to confirm
    this.closePopovers();
    this.endSound();
    this.stopWater();
    this.stopMotion({ settle: false });
    this.clock.releaseValves({ silent: true });
    this.sender.cancel();
    this.queue.items = [];
    this.active = false;
    this.root.hidden = true;
    this.haveFrame = false;
    this.frameCount = 0;
    this.lcd.setVisible(false, 'Waiting for LCD output');
    this.replay.clear();
    this.notices = [];
    for (const handle of this.pressTimers.values()) this.clearTimer(handle);
    this.pressTimers.clear();
    document.title = DEFAULT_TITLE;
    this.state = null;
    this.host = null;
  }

  releaseName() {
    return this.release.custom ? 'Custom build' : this.release.name;
  }

  /**
   * Quit: the session closes (the profile is saved) and the page returns to the start screen. The dive computer is a black box
   * here: when the engine reports it in standby, Quit goes ahead at once; while it is on, the player is asked to turn it off on
   * the device first or to force the shutdown. A device that turns itself off while the question is open ends it (Quit goes on).
   * With nothing that could still power down by itself there is nothing to ask either: no state yet, an engine stopped by an error,
   * or a lost engine (then the page leaves even though the session cannot be closed).
   */
  async quit() {
    if (this.closing || this.resetting || this.modalOpen) return;
    this.stopMotion();
    this.releaseValves();
    this.closeMenu();
    this.closePopovers();
    if (this.deviceIsOn() && !(await this.confirmForceShutdown())) return;
    await this.leave();
  }

  /**
   * The device counts as on unless the engine reports it in standby. An engine stopped by an error (DESIGN 24) does not count: the
   * device can no longer turn itself off, so Quit closes the session (the profile saved as it is) without asking.
   */
  deviceIsOn() {
    return !!this.state && !this.state.standby && !this.state.error && !this.connectionError;
  }

  /** Asks whether to force the shutdown; resolves true to quit (forced, or the device turned itself off), false to stay in the dive. */
  async confirmForceShutdown() {
    this.modalOpen = true;
    const dialog = openQuitDialog();
    this.quitDialog = dialog;
    try {
      const answer = await dialog.answer;
      return answer === 'force' || answer === 'standby';
    } finally {
      this.modalOpen = false;
      this.quitDialog = null;
    }
  }

  async leave() {
    this.closing = true;
    this.stopMotion();
    this.releaseValves();
    this.sfx('powerDown'); // the session is closing: the power-down tone rings out while the screen changes (`endSound`)
    $('quit').disabled = true;
    try {
      await this.hooks.quit();
    } finally {
      this.closing = false;
      $('quit').disabled = false;
    }
  }

  // ---- incoming --------------------------------------------------------------------------------------------------

  onFrame(message) {
    const buffer = this.lcd.draw(message);
    this.haveFrame = true;
    this.frameCount += 1;
    this.client.send('recycle', { buffer }, [buffer]);
    this.updateFrameVisibility();
  }

  /** The canvas shows when the panel is on and a frame is in hand, otherwise the placeholder. */
  updateFrameVisibility() {
    const ready = !!(this.state && this.state.frameReady);
    if (ready && this.haveFrame) this.lcd.setVisible(true);
    else this.lcd.setVisible(false, ready ? 'Waiting for LCD snapshot' : 'Waiting for LCD output');
  }

  addNotice(level, text) {
    this.notices.push({ level, text });
    if (this.notices.length > 5) this.notices.shift();
    this.renderAlerts();
  }

  /** The worker crashed or the page lost it: the game says so and stops pretending (a Quit or a reload is all that is left). */
  setConnectionError(text) {
    this.connectionError = text;
    this.stopMotion({ settle: false });
    this.clock.releaseValves({ silent: true });
    this.sender.cancel();
    this.replay.clear();
    this.resetIndicators();
    this.render();
  }

  /**
   * A state of the session. The dive advances over its virtual time; a first state, a board creation or a virtual time that went
   * backwards only restarts the integration from there. The inputs go out after every state (throttled by the sender).
   */
  onState(message) {
    // Reset all: the states of the discarded session and of the one being created wait for `show` (the new simulation).
    if (!this.active || this.resetting) return;
    const state = message.state;
    const host = message.host || {};
    const wall = this.now();
    const virtual = typeof state.virtualTime === 'number' && Number.isFinite(state.virtualTime) ? state.virtualTime : null;
    const first = this.state === null;
    const rebased = first || this.lastGeneration !== host.generation || this.lastVirtual === null
      || (virtual !== null && virtual < this.lastVirtual - 1e-9);
    this.state = state;
    this.host = host;
    this.connectionError = '';
    // The device turned itself off while the Quit confirmation was open: there is nothing left to force, so Quit goes on.
    if (this.quitDialog && state.standby) this.quitDialog.close('standby');
    if (rebased) {
      this.sim.rebase(virtual);
      this.sender.invalidate();
      if (!first) this.replay.clear();
    } else if (virtual !== null) {
      this.sim.advanceTo(virtual, this.valveFlags());
    }
    this.lastVirtual = virtual;
    this.lastWall = wall;
    this.lastGeneration = host.generation;
    if (this.pacing === null) this.applyPacing();
    this.render();
    this.renderOutputs(state);
    this.offerInputs({ immediate: rebased });
  }

  // ---- the clock: pacing, the valve override, the integration edges --------------------------------------------------

  valveFlags() {
    const live = this.injectionLive && this.clock.speed !== 0;
    return { oxygen: live && this.clock.held.oxygen.size > 0, diluent: live && this.clock.held.diluent.size > 0 };
  }

  /**
   * Integrates up to "now" under the inputs in force, before they change (a motion change, a valve, a pause), so a change takes
   * effect at the moment it happens and not at the next state. "Now" is estimated from the last state while the clock is paced;
   * unpaced, paused or stopped runs wait for the next state.
   */
  settle() {
    if (!this.active || this.lastVirtual === null || !this.state || !this.state.running || (this.host && this.host.suspended)) return;
    const pace = typeof this.pacing === 'number' ? this.pacing : 0;
    const virtual = estimateVirtual({ lastVirtual: this.lastVirtual, lastWallMs: this.lastWall, nowMs: this.now(), pace });
    if (virtual !== null) this.sim.advanceTo(virtual, this.valveFlags());
  }

  /** The play clock changed: bring the emulator's pacing in line (and decide whether the valves count yet). */
  clockChanged({ injectingChanged }) {
    if (!this.clock.injecting() || injectingChanged) this.injectionLive = false;
    this.applyPacing();
    this.render();
  }

  /**
   * Sets the emulator's pacing to the clock's effective speed: the engine's pause / resume through the action queue, the speed
   * through the worker's immediate `speed` request. A valve counts from the moment the worker confirms the 1x pacing: the state
   * it publishes with that confirmation is the exact virtual time at which the override began.
   */
  applyPacing() {
    const effective = this.clock.effective();
    const previous = this.pacing;
    if (effective === previous) {
      if (this.clock.injecting()) this.injectionLive = true; // already running at 1x: the valve counts at once
      return;
    }
    this.pacing = effective;
    if (effective === 0) {
      this.injectionLive = false;
      void this.queue.send('pause');
      return;
    }
    const state = this.state;
    // In standby the engine refuses `resume` (only Wake system brings the boards back, and resumes them): the pause is lifted, nothing more.
    const resume = !(state && state.standby) && (previous === 0 || (previous === null && !!state && !state.running && !state.error));
    const token = ++this.paceToken;
    const confirmed = this.client.request('speed', { speed: effective === 'uncapped' ? null : effective });
    Promise.resolve(confirmed).then(() => {
      if (token === this.paceToken && this.clock.injecting()) this.injectionLive = true;
    }, () => { /* a failed request leaves the valve off; the failure itself is reported by the worker */ });
    if (resume) void this.queue.send('resume');
  }

  chooseSpeed(speed) {
    this.settle();
    $('speed-menu').hidden = true;
    $('play-speed').setAttribute('aria-expanded', 'false');
    // A pause releases the valves and stops the motion, so resuming cannot leave either stuck.
    if (speed === 0) {
      this.clock.releaseValves({ silent: true });
      this.stopMotion({ settle: false });
    }
    this.clock.setSpeed(speed);
  }

  togglePause() {
    this.chooseSpeed(this.clock.speed === 0 ? this.clock.previousSpeed : 0);
  }

  holdValve(gas, source, active) {
    if (!this.clock.held[gas] || this.clock.held[gas].has(source) === active) return;
    this.settle();
    this.clock.hold(gas, source, active);
    this.render();
  }

  releaseValves() {
    if (!this.clock.injecting()) return;
    this.settle();
    this.clock.releaseValves();
  }

  // ---- the dive: motion --------------------------------------------------------------------------------------------

  changeMotion(signedRate) {
    this.settle();
    this.sim.setMotionRate(signedRate);
    this.renderMotion();
  }

  /** The gesture ends (release, cancel, blur, a depth boundary): the motion stops and the pointer is let go. */
  stopMotion({ settle = true } = {}) {
    if (settle) this.settle();
    this.sim.stopMotion();
    this.clearGesture();
    this.renderMotion();
  }

  /**
   * W (up) and S (down): hold to swim; tap once or twice first and keep the last press held for the faster speeds (a
   * double tap and hold, a triple tap and hold). Both held hold the depth; a drag in the water or a pause takes over.
   */
  swimKey(code, down) {
    const now = performance.now();
    if (down) {
      if (this.clock.speed === 0 || this.motionPointer !== null) return;
      const taps = now - this.swimReleased[code] <= SWIM_TAP_GAP_MS ? Math.min(SWIM_TIERS, this.swimTaps[code] + 1) : 1;
      this.swimTaps[code] = taps;
      this.swimPressed[code] = now;
      this.swimHeld.set(code, taps);
    } else {
      if (!this.swimHeld.delete(code)) return;
      // Only a short press is a tap: after a longer hold the next press starts at the first speed again.
      this.swimReleased[code] = now - this.swimPressed[code] < SWIM_TAP_HOLD_MS ? now : -Infinity;
    }
    const up = this.swimHeld.get('KeyW');
    const descend = this.swimHeld.get('KeyS');
    this.changeMotion(up && descend ? 0 : up ? -SWIM_RATES.up[up - 1] : descend ? SWIM_RATES.down[descend - 1] : 0);
  }

  clearGesture() {
    const pointer = this.motionPointer;
    this.motionPointer = null;
    this.motionOrigin = null;
    this.swimHeld.clear();
    const ocean = $('ocean');
    ocean.classList.remove('dragging');
    if (pointer !== null && typeof ocean.hasPointerCapture === 'function' && ocean.hasPointerCapture(pointer)) ocean.releasePointerCapture(pointer);
  }

  dragMotion(event) {
    const delta = event.clientY - this.motionOrigin.clientY;
    const deadzone = 6;
    const fraction = Math.min(1, Math.max(0, Math.abs(delta) - deadzone) / (this.motionReach - deadzone));
    const rate = fraction * (delta < 0 ? -18 : 30);
    const ocean = $('ocean');
    const rect = ocean.getBoundingClientRect();
    const x = Math.max(0, Math.min(ocean.clientWidth, event.clientX - rect.left));
    const y = Math.max(0, Math.min(ocean.clientHeight, event.clientY - rect.top));
    $('gesture-tether').setAttribute('x2', x);
    $('gesture-tether').setAttribute('y2', y);
    $('gesture-contact').setAttribute('cx', x);
    $('gesture-contact').setAttribute('cy', y);
    this.changeMotion(rate);
  }

  // ---- inputs for the emulated sensors ---------------------------------------------------------------------------

  /** The depth and the loop gas as the emulator's sensor inputs; the sender throttles them and sends the newest. */
  offerInputs({ immediate = false } = {}) {
    if (!this.active || !this.cells || this.connectionError || this.resetting) return;
    const readings = this.sim.readings(this.surfaceMbar);
    this.sender.offer(gameInputs({ surfaceMbar: this.surfaceMbar, depthM: this.sim.depth, ppo2: readings.ppo2, cells: this.cells }), { immediate });
  }

  /** A piece of the dive's virtual time was integrated: a slow cell follows the loop ppO2 over it (game-logic.js `CellFixture.advance`). */
  advanceCells(dt, from, to) {
    if (!this.cells) return;
    this.cells.advance(dt, loopPpo2(this.surfaceMbar, from.depth, from.o2), loopPpo2(this.surfaceMbar, to.depth, to.o2));
  }

  // ---- the handset -----------------------------------------------------------------------------------------------

  /** Presses a handset button ('up', 'down', 'confirm') through the same pin actions as the emulator view. */
  pressHandset(action, source = null) {
    if (this.resetting) return Promise.resolve(false);
    this.closeMenu();
    $('device').focus({ preventScroll: true });
    if (source) this.feedback(source);
    this.sfx('click', action); // every press, whichever way it came: the bezel, a tap on the display or the keyboard
    return this.queue.send(action);
  }

  feedback(button) {
    button.classList.add('pressed');
    if (this.pressTimers.has(button)) this.clearTimer(this.pressTimers.get(button));
    this.pressTimers.set(button, this.setTimer(() => {
      button.classList.remove('pressed');
      this.pressTimers.delete(button);
    }, PRESS_FEEDBACK_MS));
  }

  wakeSystem() {
    // The wake recreates the boards (the start-at-the-surface fixture applies); a standby stopped the engine, so it resumes.
    void this.queue.send('wake', { surfacePressureMbar: this.surfaceMbar });
    if (this.clock.speed !== 0) void this.queue.send('resume');
  }

  /**
   * The error alert's Resume. With the game clock paused it resumes the clock as well (at the speed before the pause), so the
   * engine never runs behind a paused header and play control: the pacing then sends the engine's resume itself.
   */
  resumeEngine() {
    if (this.clock.speed === 0) this.chooseSpeed(this.clock.previousSpeed);
    else void this.queue.send('resume');
  }

  // ---- reset -----------------------------------------------------------------------------------------------------

  /**
   * Reset all (DESIGN 23): after a confirmation, the dive computer is made new. The session is closed without saving, the whole
   * profile area of the release is cleared (EEPROM, log flash, RTC checkpoint, inputs, LED colors, and the game's oxygen-cell
   * deviations) through the same worker request as the start screen's "Reset the saved profile", and a new game session starts on
   * the same firmware with the same start options: the EEPROM factory image is written again, both clocks start at the engine's
   * default calendar, the diver is on the boat with a fresh Air loop at 1x.
   */
  async resetAll() {
    if (!this.active || this.resetting || this.closing || this.modalOpen) return;
    this.stopMotion();
    this.releaseValves();
    this.closeMenu();
    this.closePopovers();
    this.modalOpen = true;
    let confirmed = false;
    try {
      confirmed = await confirmDialog({ title: RESET_ALL_TITLE, message: RESET_ALL_MESSAGE, confirm: 'Reset all', danger: true });
    } finally {
      this.modalOpen = false;
    }
    if (!confirmed || !this.active) return;
    await this.performResetAll();
  }

  async performResetAll() {
    const info = this.info || {};
    const release = this.release;
    const custom = !!(info.custom || release.custom);
    this.resetting = true;
    this.stopMotion({ settle: false });
    this.clock.releaseValves({ silent: true });
    this.sender.cancel();
    for (const item of this.queue.items.splice(0)) item.resolve(false); // nothing queued reaches the discarded session
    this.replay.clear();
    this.closeMenu();
    this.soundResetting(); // the old session ends with a power-down tone; the new one starts with a power-up (`show`)
    this.render();
    let closed = false;
    try {
      // Requests are handled in order by the worker: an action still in flight finishes before the session closes.
      const result = await this.client.request('close-session', { save: false });
      closed = true;
      // A session that kept no profile (another tab held it, or "Boot without the saved profile") has none of its own to erase; it
      // starts again the same way, with nothing saved.
      const persist = !result || result.persist !== false;
      if (persist) {
        await this.client.request('reset-profile', { release: release.id });
        clearCellFixture(this.store, profileArea(release.id)); // the cells belong to the profile: new ones are drawn
      }
      const profile = persist ? 'stored' : 'none';
      const booted = await this.client.request('boot', { options: info.options || {}, remember: false, profile, custom });
      this.resetting = false;
      this.show({ ...info, profile, release: (booted && booted.release) || release });
      this.onState({ state: booted.state, host: booted.hostStatus });
    } catch (error) {
      this.resetting = false;
      this.resetFailed(error, closed);
    }
  }

  /**
   * Reset all did not finish. Before the session closed nothing changed and the game goes on with the error in view; after it closed
   * there is no session to go on with, so the game says so and stops pretending (Quit returns to the start screen).
   */
  resetFailed(error, closed) {
    const message = (error && error.message) || 'The emulator could not be reset.';
    if (!closed) {
      this.actionError = `Reset all failed: ${message}`;
      // The dive goes on, and so does its sound: the ambience comes back, and the memo says what `soundResetting` told the module
      // (nothing swimming, venting or held), so the next render and water frame tell it again whatever plays now.
      Object.assign(this.soundMemo, { swim: 0, vent: 0, mav: null });
      this.sfx('setAmbience', true);
      this.render();
      return;
    }
    this.setConnectionError(
      `Reset all did not finish: ${message}\nThe session is closed and the saved profile may be incomplete. Quit returns to the start screen, where the saved profile can be reset again.`,
    );
  }

  // ---- the controls guide ----------------------------------------------------------------------------------------
  //
  // An icon button at the far right of the header opens a popover under it. It is not a modal: the game goes on behind it. Opening
  // it lets go of what is held (as the Quit confirmation does), and while it is open the keyboard is not the game's: Escape closes
  // it, and the arrow keys, W / S, O / D and Space do nothing in the dive, the handset or the clock. It closes on Escape, on the
  // button again, on a click outside it and when the focus moves out of it (so no control behind it takes a key).

  /** Whether the guide has been shown on its own before: in this page, or in this browser (the page setting). */
  helpWasShown() {
    if (this.helpShown) return true;
    try {
      return this.store.get(HELP_SEEN_KEY, '') === '1';
    } catch (_) {
      return false; // no readable settings: it shows once in each page instead
    }
  }

  /** The first game start in a browser opens the guide by itself, once; after that it opens only on demand. */
  showHelpFirstTime() {
    if (this.helpWasShown()) return;
    this.helpShown = true;
    try {
      this.store.set(HELP_SEEN_KEY, '1');
    } catch (_) { /* the flag is a convenience: without it the guide shows again in the next page */ }
    this.openHelp();
  }

  openHelp() {
    if (!this.active || this.helpOpen || this.modalOpen || this.resetting) return;
    this.closeSound();
    this.closeCellMenu();
    this.stopMotion();
    this.releaseValves();
    this.closeMenu();
    this.helpOpen = true;
    $('help-panel').hidden = false;
    $('help').setAttribute('aria-expanded', 'true');
  }

  /** `focus` puts the keyboard focus back on the button (Escape, the button itself); a click or a focus elsewhere keeps its own. */
  closeHelp({ focus = false } = {}) {
    if (!this.helpOpen) return;
    this.helpOpen = false;
    $('help-panel').hidden = true;
    $('help').setAttribute('aria-expanded', 'false');
    if (focus) $('help').focus({ preventScroll: true });
  }

  toggleHelp() {
    if (this.helpOpen) this.closeHelp({ focus: true });
    else this.openHelp();
  }

  insideHelp(target) {
    return !!(target && typeof target.closest === 'function' && target.closest('#game-help-control'));
  }

  /**
   * A key while the guide is open: Escape closes it; the arrow keys and Space are inert (no handset press, no pause, no page scroll;
   * on a button, Space is still that button's own key, which is how the "?" button closes it); the rest is not the game's.
   */
  helpKeydown(event) {
    const target = event.target;
    if (event.code === 'Escape') {
      event.preventDefault();
      this.closeHelp({ focus: true });
    } else if (isHandsetArrow(event)) {
      event.preventDefault();
    } else if (event.code === 'Space' && !(target && typeof target.closest === 'function' && target.closest('button'))) {
      event.preventDefault();
    }
  }

  // ---- the sound control -----------------------------------------------------------------------------------------
  //
  // A speaker button just left of the "?" opens a small popover under it, in the look and with the rules of the controls guide: not a
  // modal, opening it lets go of what is held, Escape, the button, a click outside and a focus elsewhere close it, and while it is
  // open the keyboard is not the game's. It holds the two settings of the sound: on or off, and the volume.

  popoverOpen() {
    return this.helpOpen || this.soundOpen || this.cellMenu !== null;
  }

  closePopovers() {
    this.closeHelp();
    this.closeSound();
    this.closeCellMenu();
  }

  insideSound(target) {
    return !!(target && typeof target.closest === 'function' && target.closest('#game-sound-control'));
  }

  openSound() {
    if (!this.active || this.soundOpen || this.modalOpen || this.resetting) return;
    this.closeHelp();
    this.closeCellMenu();
    this.stopMotion();
    this.releaseValves();
    this.closeMenu();
    this.soundOpen = true;
    this.paintSoundControl();
    $('sound-panel').hidden = false;
    $('sound').setAttribute('aria-expanded', 'true');
  }

  closeSound({ focus = false } = {}) {
    if (!this.soundOpen) return;
    this.soundOpen = false;
    $('sound-panel').hidden = true;
    $('sound').setAttribute('aria-expanded', 'false');
    if (focus) $('sound').focus({ preventScroll: true });
  }

  toggleSound() {
    if (this.soundOpen) this.closeSound({ focus: true });
    else this.openSound();
  }

  /**
   * A key while the popover is open: Escape closes it; the arrow keys and Space are inert in the game (no handset press, no pause, no
   * page scroll). The slider keeps its own arrow keys, and on a button or the switch Space stays that control's own key.
   */
  soundKeydown(event) {
    const target = event.target;
    const within = (selector) => !!(target && typeof target.closest === 'function' && target.closest(selector));
    if (event.code === 'Escape') {
      event.preventDefault();
      this.closeSound({ focus: true });
    } else if (isHandsetArrow(event)) {
      if (!within('#game-sound-panel input')) event.preventDefault();
    } else if (event.code === 'Space' && !within('button,input')) {
      event.preventDefault();
    }
  }

  /** The settings as the page remembers them: on by default, at 60 %. A store that fails leaves the defaults. */
  loadSoundSettings() {
    let on = true;
    let volume = SOUND_DEFAULT_VOLUME;
    try {
      on = this.store.get(SOUND_ON_KEY, '1') !== '0';
      const stored = String(this.store.get(SOUND_VOLUME_KEY, '')).trim();
      if (stored !== '' && Number.isFinite(Number(stored))) volume = Math.min(100, Math.max(0, Math.round(Number(stored))));
    } catch (_) { /* no readable settings: the defaults */ }
    return { on, volume };
  }

  saveSoundSetting(key, value) {
    try {
      this.store.set(key, value);
    } catch (_) { /* the setting is a convenience: it applies for this page */ }
  }

  setSoundOn(on) {
    this.soundSettings.on = !!on;
    this.saveSoundSetting(SOUND_ON_KEY, on ? '1' : '0');
    this.sfx('setEnabled', !!on);
    if (on) this.unlockSound(); // turning it on is a gesture: a context the browser suspended may start now
    this.paintSoundControl();
  }

  setSoundVolume(percent) {
    const volume = Number.isFinite(percent) ? Math.min(100, Math.max(0, Math.round(percent))) : SOUND_DEFAULT_VOLUME;
    this.soundSettings.volume = volume;
    this.saveSoundSetting(SOUND_VOLUME_KEY, String(volume));
    this.sfx('setVolume', volume / 100);
    this.paintSoundControl();
  }

  /** The button shows a muted speaker while the sound is off or at 0 %; the panel shows the settings. */
  paintSoundControl() {
    const { on, volume } = this.soundSettings;
    const muted = !on || volume === 0;
    $('sound-on').checked = on;
    if ($('sound-volume').value !== String(volume)) $('sound-volume').value = String(volume);
    $('sound-volume').disabled = !on;
    $('sound-volume').setAttribute('aria-valuetext', `${volume} percent`);
    setText($('sound-volume-value'), `${volume}%`);
    $('sound').classList.toggle('muted', muted);
    $('sound').title = on ? 'Sound' : 'Sound is off';
    $('sound').querySelector('use').setAttribute('href', muted ? '#game-i-sound-off' : '#game-i-sound');
  }

  // ---- the cell fault menu (DESIGN 26) ------------------------------------------------------------------------------
  //
  // Each cell reading is a button that opens a small menu under it: the six states of game-logic.js `CELL_FAULTS` as radio items,
  // the current one checked; choosing one applies it at once (the inputs go out as after a diluent switch) and closes the menu. It
  // has the rules of the controls guide and the sound popover: not a modal, opening it lets go of what is held, Escape, the cell
  // again, a click outside and a focus elsewhere close it, and while it is open no game key reaches the game. The arrow keys move
  // between the items (Home and End to the ends), Enter and Space choose; the focus goes into the menu and back to the cell.

  cellButton(index) {
    return byId(`game-cell-button-${index + 1}`);
  }

  /** The menu's six items, built once from `CELL_FAULTS` (the same labels the logic and the tests use). */
  buildCellMenu() {
    const menu = $('cell-menu');
    menu.replaceChildren(...CELL_FAULTS.map((fault) => {
      const attributes = { type: 'button', role: 'menuitemradio', 'aria-checked': 'false', tabindex: '-1', class: 'cell-menu-item', 'data-state': fault.key };
      const item = element('button', attributes,
        element('span', { class: 'cell-menu-check', 'aria-hidden': 'true' }),
        element('span', { class: 'cell-menu-label' }, fault.label),
        element('span', { class: 'cell-menu-detail' }, fault.detail));
      item.addEventListener('click', () => {
        if (this.cellMenu !== null) this.chooseCellFault(this.cellMenu, fault.key);
      });
      return item;
    }));
  }

  cellMenuItems() {
    return [...$('cell-menu').querySelectorAll('[role="menuitemradio"]')];
  }

  insideCellMenu(target) {
    if (!target || typeof target.closest !== 'function') return false;
    return !!target.closest('#game-cell-popover') || (this.cellMenu !== null && !!target.closest(`#game-cell-button-${this.cellMenu + 1}`));
  }

  /** Opens the menu of cell `index` (0 to 2) with its current state checked and focused. */
  openCellMenu(index) {
    if (!this.active || this.modalOpen || this.resetting || !this.cells) return;
    if (this.cellMenu !== null) this.closeCellMenu();
    this.closeHelp();
    this.closeSound();
    this.stopMotion();
    this.releaseValves();
    this.closeMenu();
    this.cellMenu = index;
    const current = this.cells.fault(index);
    for (const item of this.cellMenuItems()) item.setAttribute('aria-checked', String(item.dataset.state === current));
    setText($('cell-menu-title'), `Cell ${index + 1} · simulated fault`);
    const popover = $('cell-popover');
    popover.setAttribute('data-cell', String(index + 1));
    popover.hidden = false;
    this.cellButton(index).setAttribute('aria-expanded', 'true');
    // The menu opens under the cell, near the bottom of the page: the page scrolls just enough to show all of it.
    if (typeof popover.scrollIntoView === 'function') popover.scrollIntoView({ block: 'nearest' });
    const checked = this.cellMenuItems().find((item) => item.dataset.state === current) || this.cellMenuItems()[0];
    checked.focus({ preventScroll: true });
  }

  /** `focus` puts the keyboard focus back on the cell (a choice, Escape, the cell itself); a click or a focus elsewhere keeps its own. */
  closeCellMenu({ focus = false } = {}) {
    const index = this.cellMenu;
    if (index === null) return;
    this.cellMenu = null;
    $('cell-popover').hidden = true;
    this.cellButton(index).setAttribute('aria-expanded', 'false');
    if (focus) this.cellButton(index).focus({ preventScroll: true });
  }

  toggleCellMenu(index) {
    if (this.cellMenu === index) this.closeCellMenu({ focus: true });
    else this.openCellMenu(index);
  }

  /** The diver chose a state for cell `index`: it applies at once, from the voltage the cell sends now, and the inputs go out. */
  chooseCellFault(index, key) {
    this.closeCellMenu({ focus: true });
    if (!this.active || this.resetting || !this.cells) return;
    this.settle(); // the cell's state up to now (a slow cell's lag) under the state it had
    if (!this.cells.setFault(index, key, this.sim.readings(this.surfaceMbar).ppo2)) return;
    this.offerInputs();
    this.renderFixture();
    this.render();
  }

  /** A key while the menu is open: it is the menu's, never the game's (no handset press, no pause, no swim, no valve, no scroll). */
  cellMenuKeydown(event) {
    const items = this.cellMenuItems();
    const target = event.target;
    const item = target && typeof target.closest === 'function' ? target.closest('[role="menuitemradio"]') : null;
    const at = item ? items.indexOf(item) : -1;
    const move = (index) => {
      event.preventDefault();
      items[(index + items.length) % items.length].focus();
    };
    if (event.code === 'Escape') {
      event.preventDefault();
      this.closeCellMenu({ focus: true });
    } else if (event.key === 'ArrowDown') {
      move(at + 1);
    } else if (event.key === 'ArrowUp') {
      move(at < 0 ? items.length - 1 : at - 1);
    } else if (event.key === 'Home') {
      move(0);
    } else if (event.key === 'End') {
      move(items.length - 1);
    } else if (event.code === 'Enter' || event.code === 'NumpadEnter' || event.code === 'Space') {
      event.preventDefault(); // chosen here, so the focused item does not also take the key as a click
      if (item && !event.repeat) this.chooseCellFault(this.cellMenu, item.dataset.state);
    }
  }

  // ---- sound -------------------------------------------------------------------------------------------------------
  //
  // The game tells game-sound.js what happens (the module synthesizes everything; nothing here reads it back): the entry and the
  // surface (the underwater muffling and the ambience), the swim speed, the vented gas, a held valve, handset presses, the vibrator
  // lighting, the torch, the pause, a session starting (power up), Reset all and Quit (power down). Every call goes through `sfx`,
  // which ignores a sound that is missing or fails: the game runs silently where there is no Web Audio (the Node tests).

  /** Creates the sound the first time it is needed and gives it the settings. */
  ensureSound() {
    if (this.sound) return this.sound;
    try {
      this.sound = this.createSound();
    } catch (_) {
      this.sound = null;
    }
    if (this.sound) {
      this.sfx('setVolume', this.soundSettings.volume / 100);
      this.sfx('setEnabled', this.soundSettings.on);
    }
    return this.sound;
  }

  /** Start game: the sound is created and unlocked inside the click (a browser starts audio only from a user gesture). */
  unlockSound() {
    const sound = this.ensureSound();
    if (!sound || typeof sound.unlock !== 'function') return;
    try {
      Promise.resolve(sound.unlock()).catch(() => {});
    } catch (_) { /* silent */ }
  }

  /**
   * A Start game whose boot failed (or a session started in the emulator view instead): the sound that the click created is released
   * at once, so no audio context is left behind without a game. The next Start game creates a new one.
   */
  discardSound() {
    if (this.active) return;
    const sound = this.sound;
    this.sound = null;
    try {
      if (sound && typeof sound.dispose === 'function') sound.dispose();
    } catch (_) { /* silent */ }
  }

  /** One call to the sound, or nothing. */
  sfx(name, ...args) {
    const sound = this.sound;
    if (!sound || typeof sound[name] !== 'function') return;
    try {
      sound[name](...args);
    } catch (_) { /* a sound never breaks the game */ }
  }

  /** A session starts (also after Reset all): the boat with its ambience, nothing held, the clock's pause, and the power-up. */
  startSoundSession() {
    this.unlockSound();
    const memo = this.soundMemo;
    memo.paused = this.clock.speed === 0 || !!this.connectionError;
    memo.torch = false; // a session starts at the surface without the torch (Reset all included): the torch is told only when it changes
    this.sfx('setMav', null);
    this.sfx('setSwimRate', 0);
    this.sfx('setVentRate', 0);
    this.sfx('setUnderwater', false, { immediate: true });
    this.sfx('setPaused', memo.paused);
    this.sfx('setAmbience', true);
    this.sfx('powerUp');
    this.paintSoundControl();
  }

  /** Reset all: the continuous sounds stop, the old session ends with the power-down tone. */
  soundResetting() {
    this.sfx('setMav', null);
    this.sfx('setSwimRate', 0);
    this.sfx('setVentRate', 0);
    this.sfx('setAmbience', false);
    this.sfx('powerDown');
  }

  /** The session is over (Quit): everything stops, the sound is disposed once the last tone has rung out. */
  endSound() {
    const sound = this.sound;
    this.sound = null;
    if (!sound) return;
    for (const [name, value] of [['setMav', null], ['setSwimRate', 0], ['setVentRate', 0], ['setAmbience', false]]) {
      try {
        if (typeof sound[name] === 'function') sound[name](value);
      } catch (_) { /* silent */ }
    }
    this.setTimer(() => {
      try {
        if (typeof sound.dispose === 'function') sound.dispose();
      } catch (_) { /* silent */ }
    }, SOUND_TAIL_MS);
  }

  /**
   * The dive as the sound hears it, from the simulation: underwater from the moment the diver leaves the surface (a descent is
   * commanded or the depth is above 0 m; on the boat it is the entry), and back at the surface only when the depth is 0 again,
   * which, from below, is the surface break. The swim sound follows the speed (silence when holding depth), and a held valve is a
   * MAV of that gas. Called after every render and every water frame; the sound is told only what changed.
   */
  syncSound() {
    if (!this.sound || !this.active || this.resetting) return;
    const memo = this.soundMemo;
    const sim = this.sim;
    const paused = this.clock.speed === 0 || !!this.connectionError;
    if (paused !== memo.paused) {
      memo.paused = paused;
      this.sfx('setPaused', paused);
    }
    const wet = sim.depth > 0 || sim.direction > 0;
    if (sim.depth > 0) memo.below = true;
    if (wet !== memo.wet) {
      memo.wet = wet;
      if (wet) {
        this.sfx('setUnderwater', true);
      } else if (memo.below) {
        this.sfx('surfaceBreak'); // reaching 0 m from below: a soft splash, and the bus opens
      } else {
        this.sfx('setUnderwater', false); // a descent that never left the surface
      }
      if (!wet) memo.below = false;
    }
    const swim = paused || sim.direction === 0 ? 0 : Math.round(sim.rate / SWIM_SOUND_STEP) * SWIM_SOUND_STEP;
    if (swim !== memo.swim) {
      memo.swim = swim;
      this.sfx('setSwimRate', swim);
    }
    // A valve is a MAV from the press (before the 1x override is confirmed): the sound answers the hand, not the emulator.
    const held = (gas) => !paused && this.clock.held[gas].size > 0;
    const mav = held('oxygen') ? 'oxygen' : held('diluent') ? 'diluent' : null;
    if (mav !== memo.mav) {
      memo.mav = mav;
      this.sfx('setMav', mav);
    }
  }

  /**
   * What the water scene knows after a frame: the recent vent rate (the scene's own smoothing of the gas `takeVented()` gave it, so
   * the bubbles drawn and the bubbles heard come from the same gas) and the torch (on at 40 m, off above 39 m).
   */
  syncSoundWater() {
    if (!this.sound || !this.active || this.resetting) return;
    const memo = this.soundMemo;
    const paused = this.clock.speed === 0;
    // Like the bubbles, the vent is heard only in the water: on the boat and at the surface the loop's gas goes nowhere to be seen.
    const venting = !paused && this.scene.depth > VENT_SOUND_MIN_DEPTH_M;
    const rate = venting ? Math.round(this.scene.ventLevel / VENT_SOUND_STEP) * VENT_SOUND_STEP : 0;
    if (rate !== memo.vent) {
      memo.vent = rate;
      this.sfx('setVentRate', rate);
    }
    const torch = !!this.scene.view.torch.on;
    if (torch !== memo.torch) {
      memo.torch = torch;
      this.sfx('torch', torch);
    }
    this.syncSound();
  }

  /**
   * The vibrator indicator lit: one buzz per lit pulse (the replay's flashes are the pulses), and a vibrator held on is one buzz for
   * as long as it is on: from the first state that shows it on, each state asks for the hold length again, which the sound extends.
   */
  buzzFrom(view) {
    const memo = this.soundMemo;
    const lit = !!view.lit;
    const rising = lit && !memo.buzzing;
    memo.buzzing = lit;
    if (lit && view.state === 'on') this.sfx('vibrate', VIBRATE_HOLD_MS);
    else if (rising) this.sfx('vibrate', VIBRATE_PULSE_MS);
  }

  // ---- wiring ----------------------------------------------------------------------------------------------------

  wire() {
    $('reset').addEventListener('click', () => this.resetAll());
    $('quit').addEventListener('click', () => this.quit());
    $('help').addEventListener('click', () => this.toggleHelp());
    $('sound').addEventListener('click', () => this.toggleSound());
    $('sound-on').addEventListener('change', (event) => this.setSoundOn(!!event.target.checked));
    $('sound-volume').addEventListener('input', (event) => this.setSoundVolume(Number(event.target.value)));
    // Start game is a click on the start screen's button: the sound is created and unlocked in that gesture (a later `show` repeats
    // it, which needs none once the page has had one).
    const startGame = document.getElementById('start-game');
    if (startGame) startGame.addEventListener('click', () => this.unlockSound());
    document.addEventListener('click', (event) => {
      const open = this.helpOpen ? 'help' : this.soundOpen ? 'sound' : null;
      if (!open || (open === 'help' ? this.insideHelp(event.target) : this.insideSound(event.target))) return;
      this.closePopovers();
      // A click on nothing leaves the focus nowhere: it goes back to the button. A click on a control keeps the control's focus.
      const focused = document.activeElement;
      if (!focused || focused === document.body) $(open).focus({ preventScroll: true });
    });
    document.addEventListener('focusin', (event) => {
      if (this.helpOpen && !this.insideHelp(event.target)) this.closeHelp();
      if (this.soundOpen && !this.insideSound(event.target)) this.closeSound();
      if (this.cellMenu !== null && !this.insideCellMenu(event.target)) this.closeCellMenu();
    });
    // The cells: a click, or Enter with a cell focused, opens its fault menu (Space stays the game's pause). A click outside closes it.
    for (let index = 0; index < 3; index++) {
      const button = this.cellButton(index);
      button.addEventListener('click', () => this.toggleCellMenu(index));
      button.addEventListener('keydown', (event) => {
        if ((event.code !== 'Enter' && event.code !== 'NumpadEnter') || event.repeat || this.helpOpen || this.soundOpen) return;
        event.preventDefault(); // handled here, not also as the button's click
        this.toggleCellMenu(index);
      });
    }
    document.addEventListener('click', (event) => {
      if (this.cellMenu === null || this.insideCellMenu(event.target)) return;
      const index = this.cellMenu;
      this.closeCellMenu();
      const focused = document.activeElement;
      if (!focused || focused === document.body) this.cellButton(index).focus({ preventScroll: true });
    });
    $('alerts').addEventListener('click', (event) => {
      const button = event.target && typeof event.target.closest === 'function' ? event.target.closest('button') : null;
      if (!button) return;
      const kind = button.dataset.alert;
      if (kind === 'wake') this.wakeSystem();
      else if (kind === 'resume') this.resumeEngine();
      else if (kind === 'quit') this.quit();
      else if (kind === 'dismiss') {
        const what = button.dataset.dismiss;
        if (what === 'action') this.actionError = '';
        else if (what === 'inputs') this.inputsError = '';
        else if (what === 'notices') this.notices = [];
        this.renderAlerts();
      }
    });

    // Handset: the bezel keys, the display's tap zones and the keyboard while the handset has the focus.
    for (const [id, action] of [['handset-up', 'up'], ['handset-down', 'down']]) {
      const button = $(id);
      button.addEventListener('click', () => this.pressHandset(action, button));
    }
    const frame = $('frame');
    frame.addEventListener('click', (event) => {
      const bounds = frame.getBoundingClientRect();
      if (!bounds.height) return;
      const third = Math.floor((3 * (event.clientY - bounds.top)) / bounds.height);
      this.pressHandset(third <= 0 ? 'up' : third === 1 ? 'confirm' : 'down');
    });
    // Enter confirms only with the handset focused (elsewhere it activates the focused control); the arrow keys press Up
    // and Down from anywhere in the game (the document listener below).
    $('device').addEventListener('keydown', (event) => {
      if (this.popoverOpen() || handsetKeyAction(event) !== 'confirm') return; // the controls guide and the sound popover have the keyboard
      event.preventDefault();
      this.pressHandset('confirm');
    });

    // The water: press and hold, drag up to ascend or down to descend; release holds depth.
    const ocean = $('ocean');
    ocean.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 || !event.isPrimary || this.motionPointer !== null || this.clock.speed === 0
          || event.target.closest('button,input,select,textarea,a,summary,.play-control')) return;
      event.preventDefault();
      this.stopMotion();
      ocean.focus({ preventScroll: true });
      const rect = ocean.getBoundingClientRect();
      this.motionOrigin = { clientY: event.clientY };
      // Extend the original full-speed displacement by 50% for finer control.
      this.motionReach = 1.5 * Math.max(60, Math.min(120, ocean.clientHeight / 4));
      this.motionPointer = event.pointerId;
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      $('gesture-anchor').setAttribute('cx', x);
      $('gesture-anchor').setAttribute('cy', y);
      $('gesture-tether').setAttribute('x1', x);
      $('gesture-tether').setAttribute('y1', y);
      ocean.setPointerCapture(this.motionPointer);
      ocean.classList.add('dragging');
      this.dragMotion(event);
    });
    ocean.addEventListener('pointermove', (event) => {
      if (event.pointerId === this.motionPointer) this.dragMotion(event);
    });
    ocean.addEventListener('pointerup', (event) => {
      if (event.pointerId === this.motionPointer) this.stopMotion();
    });
    for (const name of ['pointercancel', 'lostpointercapture']) {
      ocean.addEventListener(name, (event) => {
        if (event.pointerId === this.motionPointer) this.stopMotion();
      });
    }
    // The focus leaving the water ends a drag in it. It does not stop a W / S swim: those keys work wherever the focus is (a handset
    // press or a MAV press moves the focus) and end on their release or when the window loses the focus.
    ocean.addEventListener('blur', () => {
      if (this.motionPointer !== null) this.stopMotion();
    });

    // The play control.
    $('play-speed').addEventListener('click', () => {
      const menu = $('speed-menu');
      const open = menu.hidden;
      menu.hidden = !open;
      $('play-speed').setAttribute('aria-expanded', String(open));
    });
    for (const button of $('speed-menu').querySelectorAll('[data-speed]')) {
      button.addEventListener('click', () => this.chooseSpeed(button.dataset.speed === 'uncapped' ? 'uncapped' : Number(button.dataset.speed)));
    }
    document.addEventListener('click', (event) => {
      if (this.active && !(event.target && event.target.closest && event.target.closest('.play-control'))) this.closeMenu();
    });

    // The valves: hold a MAV button (pointer, or Enter while it has the focus) or the O / D keys.
    for (const gas of ['oxygen', 'diluent']) {
      const button = $(`mav-${gas}`);
      button.addEventListener('pointerdown', (event) => {
        if (event.button !== 0 || this.clock.speed === 0) return;
        event.preventDefault();
        button.focus({ preventScroll: true });
        button.setPointerCapture(event.pointerId);
        this.holdValve(gas, `pointer:${event.pointerId}`, true);
      });
      const release = (event) => this.holdValve(gas, `pointer:${event.pointerId}`, false);
      button.addEventListener('pointerup', release);
      button.addEventListener('pointercancel', release);
      button.addEventListener('lostpointercapture', release);
      // Enter keeps a focused valve usable without a pointer (Space stays the game's pause, wherever the focus is; O and D hold
      // the valves from anywhere).
      button.addEventListener('keydown', (event) => {
        if (event.code === 'Enter' && this.clock.speed !== 0 && !this.popoverOpen()) {
          event.preventDefault();
          this.holdValve(gas, 'focused-key:Enter', true);
        }
      });
      button.addEventListener('keyup', (event) => {
        if (event.code === 'Enter') {
          event.preventDefault();
          this.holdValve(gas, 'focused-key:Enter', false);
        }
      });
      button.addEventListener('blur', () => this.holdValve(gas, 'focused-key:Enter', false));
    }

    $('diluent-select').addEventListener('change', (event) => {
      this.settle();
      this.sim.setGas(event.target.value);
      this.offerInputs();
      this.render();
    });
    $('mav-flow').addEventListener('input', (event) => {
      const flow = Number(event.target.value);
      const valid = Number.isFinite(flow) && flow >= 0.1 && flow <= 300;
      if (typeof event.target.setCustomValidity === 'function') event.target.setCustomValidity(valid ? '' : 'Choose a flow from 0.1 to 300 surface L/min.');
      if (valid) {
        this.settle();
        this.sim.flow = flow;
        this.render();
      }
    });
    $('mav-flow').addEventListener('change', (event) => {
      if (typeof event.target.checkValidity === 'function' && !event.target.checkValidity()) {
        event.target.value = this.sim.flow;
        event.target.setCustomValidity('');
      }
    });

    // Keyboard: the arrow keys belong to the handset only (Up and Down wherever the focus is; never a scroll or a field),
    // W and S swim up and down, O and D hold the valves, Space pauses and resumes, Escape lets everything go. The game's keys work
    // whatever has the focus (a MAV held with the mouse, a header button after a dialog closed), except in a field being edited.
    const closest = (target, selector) => !!(target && typeof target.closest === 'function' && target.closest(selector));
    const editing = (target) => closest(target, 'input,select,textarea,[contenteditable]');
    document.addEventListener('keydown', (event) => {
      const target = event.target;
      if (!this.active || this.modalOpen) return; // the Reset all and Quit confirmations have the keyboard
      if (this.resetting) {
        // Reset all is under way: no key reaches the discarded session, and the arrow keys and Space still do not scroll the page.
        if (isHandsetArrow(event) || (event.code === 'Space' && !editing(target))) event.preventDefault();
        return;
      }
      if (this.helpOpen) {
        this.helpKeydown(event); // so does the controls guide
        return;
      }
      if (this.soundOpen) {
        this.soundKeydown(event); // and the sound popover
        return;
      }
      if (this.cellMenu !== null) {
        this.cellMenuKeydown(event); // and a cell's fault menu
        return;
      }
      if (isHandsetArrow(event)) {
        const action = handsetKeyAction(event);
        event.preventDefault(); // a held key is one press, and the page does not scroll
        if (action) this.pressHandset(action);
        return;
      }
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || editing(target)) return;
      if (event.code in SWIM_KEYS) {
        event.preventDefault();
        if (!event.repeat) this.swimKey(event.code, true);
        return;
      }
      const gas = event.code === 'KeyO' ? 'oxygen' : event.code === 'KeyD' ? 'diluent' : null;
      if (gas) {
        event.preventDefault();
        if (this.clock.speed !== 0) this.holdValve(gas, 'shortcut', true);
      } else if (event.code === 'Space') {
        event.preventDefault(); // the game's pause, never also a click of the focused button or a page scroll
        if (!event.repeat) this.togglePause();
      } else if (event.code === 'Escape') {
        this.stopMotion();
        this.releaseValves();
        this.closeMenu();
        this.render();
      }
    });
    document.addEventListener('keyup', (event) => {
      if (!this.active) return;
      if (event.code in SWIM_KEYS) this.swimKey(event.code, false);
      const gas = event.code === 'KeyO' ? 'oxygen' : event.code === 'KeyD' ? 'diluent' : null;
      if (gas) this.holdValve(gas, 'shortcut', false);
      // Space was the pause: a focused button must not take its release as a click (the browsers click a button on Space's keyup).
      if (event.code === 'Space' && !this.modalOpen && !this.popoverOpen() && !editing(event.target)) event.preventDefault();
    });
    window.addEventListener('blur', () => {
      if (!this.active) return;
      this.stopMotion();
      this.releaseValves();
      this.render();
    });
    document.addEventListener('visibilitychange', () => {
      if (!this.active) return;
      // A hidden tab lets go of the water and the valves; the replay never plays a stale catch-up. The pacing rules are the
      // worker's: no burst on return.
      this.replay.clear();
      this.stopMotion();
      this.releaseValves();
      this.lastFrameAt = null; // the first frame back does not jump
      this.client.send('visibility', { hidden: document.hidden });
      this.render();
    });
    window.addEventListener('pagehide', () => {
      if (this.active) this.client.send('flush');
    });
    window.addEventListener('resize', () => {
      this.layoutStale = true;
      if (this.active) this.render();
    });
  }

  closeMenu() {
    $('speed-menu').hidden = true;
    $('play-speed').setAttribute('aria-expanded', 'false');
  }

  // ---- rendering -------------------------------------------------------------------------------------------------

  render() {
    if (!this.active) return;
    const readings = this.sim.readings(this.surfaceMbar);
    this.renderHeader();
    this.renderPlay();
    this.renderLoop(readings);
    this.renderCells(readings);
    this.renderOcean(readings);
    this.renderMotion();
    this.renderProfile();
    this.renderAlerts();
    this.updateFrameVisibility();
  }

  renderHeader() {
    const run = this.resetting ? { text: 'Resetting…', tone: 'warn' } : runState(this.state, this.host, { connectionError: !!this.connectionError });
    const badge = $('run-state');
    setText(badge, run.text);
    if (badge.dataset.tone !== run.tone) badge.dataset.tone = run.tone;
    // The clock chip shows the state: the dot's color always, the word only when the emulator is not simply running (screen
    // readers still hear it).
    const chip = $('session-chip');
    if (chip.dataset.tone !== run.tone) chip.dataset.tone = run.tone;
    badge.classList.toggle('sr-only', run.key === 'running');
    // Reset all discards the session: neither it nor Quit can start another close until it is done.
    $('reset').disabled = this.resetting;
    if (!this.closing) $('quit').disabled = this.resetting;
    setText($('virtual-time'), clockText(this.state && typeof this.state.virtualTime === 'number' ? this.state.virtualTime : 0));
    setText($('header-speed'), speedLabel(this.clock.effective()));
    // Paused, the state word already says so: no second "Pause" beside it.
    $('header-speed').hidden = this.clock.effective() === 0;
  }

  renderPlay() {
    const clock = this.clock;
    const injecting = clock.injecting();
    const flow = this.sim.flow;
    setText($('play-speed-label'), speedLabel(clock.effective()));
    $('play-speed').title = injecting ? `MAV override: 1×; returns to ${clock.returnLabel()} on release` : 'Choose playback speed';
    setText($('mav-status'), clock.speed === 0 ? 'Paused · resume to inject'
      : injecting ? `Injecting at 1× · returns to ${clock.returnLabel()}`
        : `${flow} surface L/min · 1× while held`);
    $('mav-status').classList.toggle('active', injecting);
    $('play-icon').firstElementChild.setAttribute('href', clock.speed === 0 ? '#game-i-pause' : '#game-i-play');
    for (const button of $('speed-menu').querySelectorAll('[data-speed]')) {
      const speed = button.dataset.speed === 'uncapped' ? 'uncapped' : Number(button.dataset.speed);
      button.classList.toggle('selected', speed === clock.speed);
      button.setAttribute('aria-pressed', String(speed === clock.speed));
    }
    for (const gas of ['oxygen', 'diluent']) {
      const button = $(`mav-${gas}`);
      const active = clock.held[gas].size > 0 && clock.speed !== 0;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
      button.disabled = clock.speed === 0;
      button.title = clock.speed === 0 ? 'Resume the clock to inject gas' : `Hold to inject ${gas} at ${flow} surface L/min; playback temporarily runs at 1×`;
    }
  }

  renderLoop(readings) {
    const { o2, n2 } = readings.fractions;
    setText($('loop-o2-value'), (o2 * 100).toFixed(1));
    for (const gas of ['o2', 'n2', 'he']) setText($(`fraction-${gas}`), `${(readings.fractions[gas] * 100).toFixed(1)}%`);
    const fill = `linear-gradient(to top,#c2b16c 0% ${o2 * 100}%,#427888 ${o2 * 100}% ${(o2 + n2) * 100}%,#8a7bab ${(o2 + n2) * 100}% 100%)`;
    if (this.previousPaint !== fill) {
      this.previousPaint = fill;
      $('lung-fill-left').style.background = fill;
      $('lung-fill-right').style.background = fill;
    }
    const sim = this.sim;
    const running = this.clock.speed !== 0;
    const advActive = running && sim.elapsed - sim.lastActivity.adv < 0.5;
    const ventActive = running && sim.elapsed - sim.lastActivity.vent < 0.5;
    const flags = this.valveFlags();
    $('adv-status').classList.toggle('active', advActive);
    setText($('adv-text'), advActive ? 'ADV · adding diluent' : 'ADV · ready');
    $('vent-status').classList.toggle('active', ventActive);
    setText($('vent-text'), ventActive ? 'Vent · releasing' : 'Vent · closed');
    $('flow-in').classList.toggle('active', advActive || flags.oxygen || flags.diluent);
    $('flow-out').classList.toggle('active', ventActive);
  }

  /**
   * The cell panel: the voltages sent to the firmware (with any simulated fault: `data-fault`, the ring, the dimmed reading and the
   * badge, and the cell's accessible name), and the true loop ppO2 (the simulated truth).
   */
  renderCells(readings) {
    if (!this.cells) return;
    this.cells.millivolts(readings.ppo2).forEach((millivolts, index) => {
      const text = millivolts.toFixed(2);
      setText(byId(`game-cell-${index + 1}`), text);
      const fault = cellFault(this.cells.fault(index));
      const button = this.cellButton(index);
      if (fault.key === 'normal') {
        if (button.getAttribute('data-fault') !== null) button.removeAttribute('data-fault');
      } else if (button.getAttribute('data-fault') !== fault.key) {
        button.setAttribute('data-fault', fault.key);
      }
      const badge = byId(`game-cell-badge-${index + 1}`);
      setText(badge, fault.badge);
      badge.hidden = !fault.badge;
      const name = `Cell ${index + 1}: ${text} mV, ${fault.word}. Opens the cell fault menu`;
      if (button.getAttribute('aria-label') !== name) button.setAttribute('aria-label', name);
    });
    setText($('loop-ppo2'), readings.ppo2.toFixed(2));
  }

  /** The assumptions panel lists the cell fixture of this profile: the voltage in air at the surface and the sensitivity. */
  renderFixture() {
    const list = $('fixture-list');
    list.replaceChildren();
    if (!this.cells) return;
    this.cells.deviationsMv.forEach((deviation, index) => {
      const sensitivity = this.cells.sensitivities[index];
      const fault = cellFault(this.cells.fault(index));
      const faulty = fault.key === 'normal' ? '' : ` · simulated fault: ${fault.word} (this session)`;
      list.append(element('div', {}, `Cell ${index + 1}: ${(12 + deviation).toFixed(2)} mV in air at the surface (${deviation >= 0 ? '+' : '−'}${Math.abs(deviation).toFixed(2)} mV), ${sensitivity.toFixed(1)} mV/bar${faulty}`));
    });
    list.append(element('div', { class: 'fixture-note' }, `Kept in this browser for the ${this.release.custom ? 'custom-build' : this.release.name} profile${this.cells.drawn ? ' (newly drawn)' : ''}.`));
  }

  /** The readouts of the water panel (the picture itself is the frame loop's: see `paintWater`). */
  renderOcean(readings) {
    const sim = this.sim;
    setText($('depth-value'), sim.depth.toFixed(1));
    setText($('ambient-pressure'), `${readings.ambientBar.toFixed(2)} bar ambient`);
    setText($('max-depth-meta'), `Max ${sim.maxDepth.toFixed(1)} m`);
  }

  renderMotion() {
    const sim = this.sim;
    const paused = this.clock.speed === 0;
    const stopped = !!this.state && !this.state.running && !paused && !(this.host && this.host.suspended);
    $('ocean').classList.toggle('paused', paused);
    const motionText = sim.direction === 0 ? 'Holding depth' : `${sim.direction > 0 ? 'Descending' : 'Ascending'} · ${sim.rate.toFixed(1)} m/min`;
    setText($('motion-status'), paused ? 'Paused' : stopped ? 'Emulator stopped' : motionText);
    setText($('scene-hint'), paused ? 'Resume the clock to dive'
      : stopped ? 'The emulated unit is stopped · see the message above'
        : sim.direction === 0 ? 'Hold & drag to dive · release to hold' : 'Release to hold depth');
    const signedRate = sim.direction * sim.rate;
    const angle = signedRate / (signedRate < 0 ? 18 : 30) * 90;
    $('speed-needle').setAttribute('transform', `rotate(${angle} 60 65)`);
    setText($('speed-value'), sim.rate.toFixed(1));
    setText($('speed-direction'), sim.direction === 0 ? 'HOLD' : sim.direction > 0 ? '↓ DESCEND' : '↑ ASCEND');
    $('speedometer').classList.toggle('ascending', sim.direction < 0);
    $('speedometer').classList.toggle('descending', sim.direction > 0);
    $('speedometer').setAttribute('aria-valuenow', signedRate.toFixed(1));
    $('speedometer').setAttribute('aria-valuetext', sim.direction === 0 ? 'Holding depth' : `${sim.direction > 0 ? 'Descending' : 'Ascending'} at ${sim.rate.toFixed(1)} meters per minute`);
    this.syncSound(); // every change of the motion, the valves and the clock passes here
  }

  // ---- the water view (DESIGN 22) --------------------------------------------------------------------------------
  //
  // One depth story: a camera follows the diver through a fixed 30 m window; the layers behind (water color by depth, rays and
  // distant shapes with parallax, the world with the boat, the surface, the depth lines and the seabed) are moved by it; the
  // canvas holds what drifts through the water (marine snow, vent bubbles, the splash, the torch). The decisions are in
  // game-logic.js and game-water.js; this is the page side. It runs on animation frames only while the game is on screen, and
  // it only reads the simulation: nothing here changes the depth, the gas or what the emulator receives.

  /** The static parts of the water panel: colors by depth, the depth lines (every 5 m, labeled every 10 m) and the gauge's ticks. */
  buildWater() {
    $('water-bg').style.background = worldGradient();
    $('gauge-fill').style.background = gaugeGradient();
    const grid = $('depth-grid');
    grid.replaceChildren();
    for (let depth = 5; depth <= MAX_DEPTH_METERS; depth += 5) {
      const major = depth % 10 === 0;
      const line = element('div', { class: major ? 'depth-grid-line major' : 'depth-grid-line' });
      if (major) line.append(element('span', {}, `${depth} m`));
      line.style.setProperty('--g-d', String(depth));
      grid.append(line);
    }
    const ticks = $('gauge-ticks');
    ticks.replaceChildren();
    for (const depth of GAUGE_LABELS) {
      const tick = element('div', { class: 'gauge-tick' }, element('span', {}, String(depth)));
      tick.style.setProperty('--g-d', String(depth));
      ticks.append(tick);
    }
  }

  /** The reduced-motion preference: no drift, parallax, bobbing or entry animation, and the camera moves without easing. */
  watchMotion(motion) {
    const query = motion !== undefined ? motion
      : (typeof window !== 'undefined' && typeof window.matchMedia === 'function' ? window.matchMedia('(prefers-reduced-motion: reduce)') : null);
    this.motionQuery = query;
    if (!query) return;
    this.scene.setReducedMotion(!!query.matches);
    const changed = (event) => {
      this.scene.setReducedMotion(!!(event && typeof event.matches === 'boolean' ? event.matches : query.matches));
      if (this.active) this.paintWater();
    };
    if (typeof query.addEventListener === 'function') query.addEventListener('change', changed);
    else if (typeof query.addListener === 'function') query.addListener(changed);
  }

  startWater() {
    this.layoutStale = true;
    this.painted.clear();
    this.lastFrameAt = null;
    if (typeof ResizeObserver === 'function' && !this.resizeObserver) {
      this.resizeObserver = new ResizeObserver(() => { this.layoutStale = true; });
      this.resizeObserver.observe($('ocean'));
    }
    this.advanceWater(0); // the first picture at once
    this.requestFrame();
  }

  stopWater() {
    if (this.frameHandle !== null && this.frames) this.frames.cancel(this.frameHandle);
    this.frameHandle = null;
    this.lastFrameAt = null;
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
  }

  requestFrame() {
    if (!this.frames || this.frameHandle !== null || !this.active) return;
    this.frameHandle = this.frames.request((now) => this.frame(now));
  }

  /** One animation frame: the scene advances by the wall time since the last one (at most MAX_FRAME_STEP_S: no jump after a pause). */
  frame(now) {
    this.frameHandle = null;
    if (!this.active) return;
    const elapsed = this.lastFrameAt === null ? 0 : (now - this.lastFrameAt) / 1000;
    this.lastFrameAt = now;
    this.advanceWater(Math.min(MAX_FRAME_STEP_S, Math.max(0, elapsed)));
    this.requestFrame();
  }

  /** The panel's size and the pixels per meter of the 30 m window; the boat, the canvas and the gauge follow it. */
  measureLayout() {
    this.layoutStale = false;
    const ocean = $('ocean');
    let width = ocean.clientWidth;
    let height = ocean.clientHeight;
    if (!(width > 0) || !(height > 0)) {
      if (this.layout.ppm > 0) return; // not laid out (hidden): keep what was measured
      width = 560; // never measured: a plausible panel
      height = 560;
    }
    if (width === this.layout.width && height === this.layout.height) return;
    const ppm = height / VIEW_WINDOW_M;
    this.layout = { width, height, ppm };
    ocean.style.setProperty('--g-ppm', `${ppm}px`);
    // The boat is drawn so that the stern, where the diver sits, is where the entry starts.
    const seatX = width * DIVER_X + ENTRY_SEAT.dx;
    const boatWidth = Math.max(BOAT_MIN_WIDTH, Math.min(BOAT_MAX_WIDTH, (width - 10 - seatX) / (1 - BOAT_SEAT_FRACTION)));
    ocean.style.setProperty('--g-boat-w', `${boatWidth.toFixed(1)}px`);
    ocean.style.setProperty('--g-boat-x', `${(seatX - BOAT_SEAT_FRACTION * boatWidth).toFixed(1)}px`);
    // The canvas has the panel's size in device pixels (a new size clears it and resets its transform).
    const canvas = $('water-canvas');
    this.canvasScale = (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
    canvas.width = Math.max(1, Math.round(width * this.canvasScale));
    canvas.height = Math.max(1, Math.round(height * this.canvasScale));
    this.gaugeHeight = $('gauge-track').clientHeight || Math.max(1, height - 38);
    this.scene.dirty = true;
  }

  getWaterContext() {
    if (this.waterContext === null) {
      const canvas = $('water-canvas');
      const context = typeof canvas.getContext === 'function' ? canvas.getContext('2d') : null;
      this.waterContext = context && typeof context.clearRect === 'function' ? context : false;
    }
    return this.waterContext || null;
  }

  /**
   * The depth the picture shows: the simulation's, carried forward over the time since the last state while the clock runs at a
   * known pace (the states come five times a second; the drawing runs on every frame). Never read back by the simulation.
   */
  shownDepth() {
    const sim = this.sim;
    if (sim.direction === 0 || sim.virtual === null || this.lastVirtual === null || !this.state || !this.state.running
        || (this.host && this.host.suspended)) return sim.depth;
    const pace = typeof this.pacing === 'number' ? this.pacing : 0;
    const now = estimateVirtual({ lastVirtual: this.lastVirtual, lastWallMs: this.lastWall, nowMs: this.now(), pace });
    const ahead = now === null ? 0 : Math.max(0, now - sim.virtual);
    return Math.max(0, Math.min(MAX_DEPTH_METERS, sim.depth + sim.direction * sim.rate * ahead / 60));
  }

  advanceWater(dt) {
    if (this.layoutStale) this.measureLayout();
    const paused = this.clock.speed === 0;
    const vented = this.sim.takeVented(); // taken on every frame: a pause lets go of it
    const timeScale = typeof this.pacing === 'number' ? this.pacing : this.pacing === 'uncapped' ? 4 : 1;
    this.scene.step({
      dt: paused ? 0 : dt, depth: this.shownDepth(), direction: this.sim.direction, vented: paused ? 0 : vented, layout: this.layout, timeScale,
    });
    this.syncSoundWater(); // the one consumer of the vented gas feeds the bubbles and, through the scene's vent rate, the sound
    this.paintWater();
  }

  /** Writes a style property of a game element once per change. */
  paint(id, property, value) {
    const key = `${id}.${property}`;
    if (this.painted.get(key) === value) return;
    this.painted.set(key, value);
    $(id).style[property] = value;
  }

  /** Toggles a class once per change. */
  flag(node, name, on) {
    if (this.painted.get(`.${name}`) === on) return;
    this.painted.set(`.${name}`, on);
    node.classList.toggle(name, on);
  }

  /** Puts the scene on the page: the layers at the camera, the boat, the diver, the gauge, the classes, and the canvas. */
  paintWater() {
    const { ppm } = this.layout;
    if (!this.active || !(ppm > 0)) return;
    const scene = this.scene;
    const view = scene.view;
    const shift = -(view.cameraTop + SKY_M) * ppm;
    const world = `translate3d(0, ${shift.toFixed(1)}px, 0)`;
    this.paint('water-bg', 'transform', world);
    this.paint('water-fg', 'transform', world);
    this.paint('water-far', 'transform', `translate3d(0, ${(shift * (scene.reducedMotion ? 1 : FAR_PARALLAX)).toFixed(1)}px, 0)`);
    this.paint('far-rays', 'opacity', view.rayFade.toFixed(3));
    this.paint('far-shapes', 'opacity', view.shapeFade.toFixed(3));
    this.paint('boat', 'transform', `translateY(${view.boat.y.toFixed(2)}px) rotate(${view.boat.tilt.toFixed(2)}deg)`);
    const diver = view.diver;
    this.paint('diver-track', 'transform', `translate3d(${diver.x.toFixed(1)}px, ${diver.y.toFixed(1)}px, 0) rotate(${diver.spin.toFixed(1)}deg) scale(${diver.scale.toFixed(3)})`);
    this.paint('diver', 'rotate', `${diver.attitude.toFixed(2)}deg`);
    this.flag($('ocean'), 'on-boat', view.phase === 'boat');
    this.flag($('ocean'), 'entering', view.phase === 'entering');
    this.flag($('ocean'), 'dark', view.torch.on);
    this.flag($('speedometer'), 'glow', view.torch.on); // the dial glows from the torch's depth
    // The gauge: the whole range, the window the camera shows, the maximum depth and the diver.
    const per = this.gaugeHeight / MAX_DEPTH_METERS;
    const top = Math.max(0, Math.min(MAX_DEPTH_METERS, view.cameraTop));
    const bottom = Math.max(0, Math.min(MAX_DEPTH_METERS, view.cameraTop + VIEW_WINDOW_M));
    this.paint('gauge-window', 'transform', `translateY(${(top * per).toFixed(1)}px)`);
    this.paint('gauge-window', 'height', `${Math.max(2, (bottom - top) * per).toFixed(1)}px`);
    this.paint('gauge-diver', 'transform', `translateY(${(view.depth * per).toFixed(1)}px)`);
    this.paint('gauge-max', 'transform', `translateY(${(this.sim.maxDepth * per).toFixed(1)}px)`);
    if (scene.dirty) {
      const context = this.getWaterContext();
      if (context) {
        context.setTransform(this.canvasScale || 1, 0, 0, this.canvasScale || 1, 0, 0);
        scene.draw(context);
      }
      scene.dirty = false;
    }
  }

  renderProfile() {
    const sim = this.sim;
    const width = 700;
    const height = 175;
    const left = 36;
    const right = 18;
    const top = 12;
    const bottom = 29;
    const plotWidth = width - left - right;
    const plotHeight = height - top - bottom;
    // The chart shows the current or the last dive (DESIGN 23 addition): its dive time and its deepest point, nothing before the first descent.
    const timeRange = Math.max(600, Math.ceil(sim.diveTime / 600) * 600);
    const depthRange = Math.min(MAX_DEPTH_METERS, Math.max(40, Math.ceil((sim.maxDepth + 5) / 20) * 20));
    const x = (time) => left + time / timeRange * plotWidth;
    const y = (depth) => top + depth / depthRange * plotHeight;
    const key = `${timeRange}:${depthRange}`;
    const chart = $('profile');
    if (chart.dataset.axes !== key) {
      chart.dataset.axes = key;
      const grid = $('profile-grid');
      const labels = $('profile-labels');
      grid.replaceChildren();
      labels.replaceChildren();
      for (let index = 0; index <= 4; index++) {
        const depth = index * depthRange / 4;
        grid.append(svgNode('line', { x1: left, x2: width - right, y1: y(depth), y2: y(depth) }));
        labels.append(svgNode('text', { x: 0, y: y(depth) + 3 }, `${Math.round(depth)} m`));
      }
      for (let index = 0; index <= 5; index++) {
        const time = index * timeRange / 5;
        labels.append(svgNode('text', { x: x(time), y: height - 5, 'text-anchor': index === 0 ? 'start' : index === 5 ? 'end' : 'middle' }, `${Math.round(time / 60)}′`));
      }
    }
    const empty = sim.profile.length === 0;
    setSvgHidden($('profile-empty'), !empty);
    setSvgHidden($('profile-point'), empty);
    // A dive in progress ends at the diver's current point; a finished dive ends at its surface point (recorded by the dive).
    const samples = sim.diving ? [...sim.profile, [sim.diveTime, sim.depth]] : sim.profile;
    const end = samples.length ? samples[samples.length - 1] : [0, 0];
    const path = samples.map(([time, depth], index) => `${index === 0 ? 'M' : 'L'}${x(time).toFixed(2)},${y(depth).toFixed(2)}`).join(' ');
    $('profile-line').setAttribute('d', path);
    $('profile-area').setAttribute('d', empty ? '' : `${path} L${x(end[0]).toFixed(2)},${top} L${left},${top} Z`);
    $('profile-point').setAttribute('cx', x(end[0]));
    $('profile-point').setAttribute('cy', y(end[1]));
    setText($('profile-duration'), `${durationText(sim.diveTime)} elapsed`);
  }

  /** Engine errors, stops (standby, a CPU fault), lost connections and worker notices stay in view: no silent freeze. */
  renderAlerts() {
    const items = [];
    if (this.connectionError) items.push({ id: 'connection', level: 'error', text: this.connectionError });
    for (const alert of stopAlerts(this.state)) items.push(alert);
    if (this.actionError) items.push({ id: 'action', level: 'error', text: this.actionError, dismiss: 'action' });
    if (this.inputsError) items.push({ id: 'inputs', level: 'error', text: `The sensor inputs were not applied: ${this.inputsError}`, dismiss: 'inputs' });
    this.notices.forEach((notice, index) => items.push({ id: `notice-${index}`, level: notice.level === 'error' ? 'error' : 'warning', text: notice.text, dismiss: 'notices' }));
    const key = JSON.stringify(items);
    if (key === this.alertsKey) return; // the list is rebuilt only when it changed, so a button survives the 5 Hz state updates
    this.alertsKey = key;
    const box = $('alerts');
    box.replaceChildren(...items.map((item) => {
      const row = element('div', { class: `game-alert ${item.level}`, 'data-alert-id': item.id }, element('span', { class: 'game-alert-text' }, item.text));
      const actions = element('span', { class: 'game-alert-actions' });
      if (item.action === 'wake') actions.append(element('button', { type: 'button', 'data-alert': 'wake' }, 'Wake system'));
      if (item.action === 'resume') actions.append(element('button', { type: 'button', 'data-alert': 'resume' }, 'Resume'));
      if (item.quit) actions.append(element('button', { type: 'button', 'data-alert': 'quit', title: 'Close the session (the profile is saved) and return to the start screen' }, 'Quit'));
      if (item.dismiss) actions.append(element('button', { type: 'button', 'data-alert': 'dismiss', 'data-dismiss': item.dismiss }, 'Dismiss'));
      row.append(actions);
      return row;
    }));
    box.hidden = items.length === 0;
  }

  // ---- the firmware's outputs ------------------------------------------------------------------------------------

  renderOutputs(state) {
    const epoch = `${state.outputHistoryEpoch === undefined ? 'legacy' : state.outputHistoryEpoch}:${(this.host && this.host.generation) || 0}`;
    const { entries, removed } = this.replay.update(state.hardwareOutputs, { virtualTime: state.virtualTime, epoch });
    const shown = new Set(entries.map((entry) => entry.id));
    for (const entry of removed) this.resetIndicator(entry.id);
    for (const id of Object.keys(GAME_INDICATORS)) if (!shown.has(id)) this.resetIndicator(id);
  }

  /** Repaints one indicator: steady drive, or a replayed short pulse (reduced motion keeps a static illuminated indicator). */
  paintIndicator(entry) {
    const slot = GAME_INDICATORS[entry.id];
    if (!slot) return;
    const view = indicatorView(entry);
    const node = $(`signal-${slot}`);
    node.classList.toggle('active', view.lit);
    node.classList.toggle('unknown', view.state === 'unknown');
    node.setAttribute('aria-label', `${INDICATOR_NAMES[slot]}: ${view.text}`);
    node.title = `${INDICATOR_NAMES[slot]} · ${view.title}`;
    if (slot === 'vibrator') this.buzzFrom(view); // the sound follows the indicator: a buzz for each lit pulse
  }

  resetIndicator(id) {
    const slot = GAME_INDICATORS[id];
    if (!slot) return;
    if (slot === 'vibrator') this.soundMemo.buzzing = false;
    const node = $(`signal-${slot}`);
    node.classList.remove('active');
    node.classList.add('unknown');
    node.setAttribute('aria-label', `${INDICATOR_NAMES[slot]}: unknown`);
    node.title = INDICATOR_NAMES[slot];
  }

  resetIndicators() {
    for (const id of Object.keys(GAME_INDICATORS)) this.resetIndicator(id);
  }
}
