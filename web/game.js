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
import { byId, prefs, setText } from './dom.js';
import { DEFAULT_MAV_FLOW_SL_MIN, MAX_DEPTH_METERS } from './game-gas.js';
import {
  GAME_INDICATORS, GameSim, InputsSender, PlayClock, cellMillivolts, cellSensitivities, clockText, durationText,
  estimateVirtual, gameInputs, indicatorView, loadCellFixture, runState, speedLabel, stopAlerts,
} from './game-logic.js';
import { handsetKeyAction } from './keys.js';
import { LcdView } from './lcd.js';
import { DEFAULT_RELEASE_ID, describeRelease, profileArea } from './releases.js';
import { ReplayController } from './replay.js';

const $ = (id) => byId(`game-${id}`);
const SVG_NS = 'http://www.w3.org/2000/svg';
const DEFAULT_TITLE = 'NGC system emulator · WebAssembly';
const PRESS_FEEDBACK_MS = 160;
const INDICATOR_NAMES = { vibrator: 'Handset vibrator', red: 'Red HUD LED (HUD 3)', white: 'White HUD LED (HUD 2)' };

function svgNode(tag, attributes, content) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, value);
  if (content !== undefined) node.textContent = content;
  return node;
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

export class GameView {
  /**
   * @param {import('./worker-client.js').WorkerClient} client
   * @param {{quit: () => (void|Promise<void>)}} hooks `quit` closes the session (profile saved) and leaves the game
   * @param {{timers?: {setTimer: Function, clearTimer: Function}, now?: () => number, random?: () => number, store?: object}} [options]
   *   the tests inject timers, a clock, a random source and the settings store
   */
  constructor(client, hooks, { timers, now, random, store } = {}) {
    this.client = client;
    this.hooks = hooks;
    this.root = byId('screen-game');
    this.store = store || prefs;
    this.random = random || Math.random;
    this.now = now || (() => performance.now());
    this.setTimer = (timers && timers.setTimer) || ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = (timers && timers.clearTimer) || ((handle) => clearTimeout(handle));
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
    this.notices = [];
    this.sender = null;
    this.pressTimers = new Map();
    this.motionPointer = null;
    this.motionOrigin = null;
    this.motionReach = 180;
    this.motionKeys = new Set();
    this.previousPaint = '';
    this.alertsKey = '';
    // The worker sends the first LCD frame while it creates the session, so it can reach `onFrame` before `show` runs: frames
    // seen so far are kept and forgotten only when the session ends (`hide`), as in the emulator view.
    this.haveFrame = false;
    this.frameCount = 0;
    this.beginSession();
    this.wire();
  }

  // ---- session ---------------------------------------------------------------------------------------------------

  /** Everything that belongs to one session: a new simulation, clock, action queue and input sender. */
  beginSession({ startPaused = false } = {}) {
    if (this.sender) this.sender.cancel();
    this.surfaceMbar = DEFAULT_SURFACE_MBAR;
    this.cells = null;
    this.sim = new GameSim({ flow: DEFAULT_MAV_FLOW_SL_MIN, onBoundary: () => this.clearGesture() });
    this.clock = new PlayClock({ onChange: (change) => this.clockChanged(change) });
    if (startPaused) this.clock.speed = 0;
    this.queue = new ActionQueue({
      perform: (payload) => this.client.request('action', { request: payload }),
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
    this.cells = loadCellFixture(this.store, profileArea(this.release.id), this.random);
    this.cells.sensitivities = cellSensitivities(this.cells.deviationsMv);
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
    this.render();
  }

  hide() {
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

  async quit() {
    if (this.closing) return;
    this.closing = true;
    this.stopMotion();
    this.releaseValves();
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
    if (!this.active) return;
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
    const resume = previous === 0 || (previous === null && !!state && !state.running && !state.standby && !state.error);
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

  clearGesture() {
    const pointer = this.motionPointer;
    this.motionPointer = null;
    this.motionOrigin = null;
    this.motionKeys.clear();
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

  updateKeyboardMotion() {
    const up = this.motionKeys.has('ArrowUp');
    const down = this.motionKeys.has('ArrowDown');
    this.changeMotion(up === down ? 0 : up ? -6 : 10);
  }

  // ---- inputs for the emulated sensors ---------------------------------------------------------------------------

  /** The depth and the loop gas as the emulator's sensor inputs; the sender throttles them and sends the newest. */
  offerInputs({ immediate = false } = {}) {
    if (!this.active || !this.cells || this.connectionError) return;
    const readings = this.sim.readings(this.surfaceMbar);
    this.sender.offer(gameInputs({
      surfaceMbar: this.surfaceMbar, depthM: this.sim.depth, ppo2: readings.ppo2, sensitivities: this.cells.sensitivities,
    }), { immediate });
  }

  // ---- the handset -----------------------------------------------------------------------------------------------

  /** Presses a handset button ('up', 'down', 'confirm') through the same pin actions as the emulator view. */
  pressHandset(action, source = null) {
    this.closeMenu();
    $('device').focus({ preventScroll: true });
    if (source) this.feedback(source);
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

  resumeEngine() {
    void this.queue.send('resume');
  }

  // ---- reset -----------------------------------------------------------------------------------------------------

  /** Back to the surface with a fresh Air loop at 1x. The boards keep running; the firmware ends its dive itself. */
  resetDive() {
    this.stopMotion();
    this.clock.releaseValves({ silent: true });
    this.sim.reset(this.sim.virtual);
    $('diluent-select').value = 'air';
    this.closeMenu();
    this.clock.reset();
    this.offerInputs({ immediate: true });
    this.render();
  }

  // ---- wiring ----------------------------------------------------------------------------------------------------

  wire() {
    $('reset').addEventListener('click', () => this.resetDive());
    $('quit').addEventListener('click', () => this.quit());
    $('alerts').addEventListener('click', (event) => {
      const button = event.target && typeof event.target.closest === 'function' ? event.target.closest('button') : null;
      if (!button) return;
      const kind = button.dataset.alert;
      if (kind === 'wake') this.wakeSystem();
      else if (kind === 'resume') this.resumeEngine();
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
    $('device').addEventListener('keydown', (event) => {
      const action = handsetKeyAction(event);
      // A held key is one press (no auto-repeat), and the page must not scroll under the handset's arrow keys either.
      if (!action) {
        if (event.repeat && !event.altKey && !event.ctrlKey && !event.metaKey && ['ArrowUp', 'ArrowDown'].includes(event.key)) event.preventDefault();
        return;
      }
      event.preventDefault();
      this.pressHandset(action);
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
    ocean.addEventListener('keydown', (event) => {
      if (event.target !== ocean || event.metaKey || event.ctrlKey || event.altKey || !['ArrowUp', 'ArrowDown'].includes(event.code)) return;
      event.preventDefault();
      if (this.clock.speed === 0 || this.motionPointer !== null) return;
      this.motionKeys.add(event.code);
      this.updateKeyboardMotion();
    });
    ocean.addEventListener('blur', () => this.stopMotion());

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

    // The valves: hold a MAV button (pointer, or Space / Enter while it has the focus) or the O / D keys.
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
      // Space / Enter keep a focused valve usable without a pointer.
      button.addEventListener('keydown', (event) => {
        if ((event.code === 'Space' || event.code === 'Enter') && this.clock.speed !== 0) {
          event.preventDefault();
          this.holdValve(gas, `focused-key:${event.code}`, true);
        }
      });
      button.addEventListener('keyup', (event) => {
        if (event.code === 'Space' || event.code === 'Enter') {
          event.preventDefault();
          this.holdValve(gas, `focused-key:${event.code}`, false);
        }
      });
      button.addEventListener('blur', () => {
        this.holdValve(gas, 'focused-key:Space', false);
        this.holdValve(gas, 'focused-key:Enter', false);
      });
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

    // Keyboard: O and D hold the valves, Space pauses and resumes, Escape lets everything go.
    document.addEventListener('keydown', (event) => {
      const target = event.target;
      if (!this.active || event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
      if (target && typeof target.closest === 'function' && target.closest('input,select,textarea,button,summary')) return;
      const gas = event.code === 'KeyO' ? 'oxygen' : event.code === 'KeyD' ? 'diluent' : null;
      if (gas && this.clock.speed !== 0) {
        event.preventDefault();
        this.holdValve(gas, 'shortcut', true);
      }
      if (event.code === 'Space' && !event.repeat) {
        event.preventDefault();
        this.togglePause();
      }
      if (event.code === 'Escape') {
        this.stopMotion();
        this.releaseValves();
        this.closeMenu();
        this.render();
      }
    });
    document.addEventListener('keyup', (event) => {
      if (!this.active) return;
      if (this.motionKeys.delete(event.code)) this.updateKeyboardMotion();
      const gas = event.code === 'KeyO' ? 'oxygen' : event.code === 'KeyD' ? 'diluent' : null;
      if (gas) this.holdValve(gas, 'shortcut', false);
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
      this.client.send('visibility', { hidden: document.hidden });
      this.render();
    });
    window.addEventListener('pagehide', () => {
      if (this.active) this.client.send('flush');
    });
    window.addEventListener('resize', () => {
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
    const run = runState(this.state, this.host, { connectionError: !!this.connectionError });
    const badge = $('run-state');
    setText(badge, run.text);
    if (badge.dataset.tone !== run.tone) badge.dataset.tone = run.tone;
    setText($('virtual-time'), clockText(this.state && typeof this.state.virtualTime === 'number' ? this.state.virtualTime : 0));
    setText($('header-speed'), speedLabel(this.clock.effective()));
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

  /** The cell panel: the voltages sent to the firmware, and the true loop ppO2 (the simulated truth). */
  renderCells(readings) {
    if (!this.cells) return;
    const volts = cellMillivolts(readings.ppo2, this.cells.sensitivities);
    volts.forEach((millivolts, index) => setText($(`cell-${index + 1}`), millivolts.toFixed(2)));
    setText($('loop-ppo2'), readings.ppo2.toFixed(2));
  }

  /** The assumptions panel lists the cell fixture of this profile: the voltage in air at the surface and the sensitivity. */
  renderFixture() {
    const list = $('fixture-list');
    list.replaceChildren();
    if (!this.cells) return;
    this.cells.deviationsMv.forEach((deviation, index) => {
      const sensitivity = this.cells.sensitivities[index];
      list.append(element('div', {}, `Cell ${index + 1}: ${(12 + deviation).toFixed(2)} mV in air at the surface (${deviation >= 0 ? '+' : '−'}${Math.abs(deviation).toFixed(2)} mV), ${sensitivity.toFixed(1)} mV/bar`));
    });
    list.append(element('div', { class: 'fixture-note' }, `Kept in this browser for the ${this.release.custom ? 'custom-build' : this.release.name} profile${this.cells.drawn ? ' (newly drawn)' : ''}.`));
  }

  renderOcean(readings) {
    const sim = this.sim;
    setText($('depth-value'), sim.depth.toFixed(1));
    setText($('ambient-pressure'), `${readings.ambientBar.toFixed(2)} bar ambient`);
    setText($('max-depth-meta'), `Max ${sim.maxDepth.toFixed(1)} m`);
    // Zoom out as the dive gets deeper, preserving room under the diver.
    const range = Math.min(MAX_DEPTH_METERS, Math.max(40, Math.ceil((sim.maxDepth + 8) / 20) * 20));
    const grid = $('depth-grid');
    if (grid.dataset.range !== String(range)) {
      grid.replaceChildren();
      grid.dataset.range = String(range);
      const increment = range <= 60 ? 10 : 20;
      for (let depth = increment; depth < range; depth += increment) {
        const line = element('div', { class: 'depth-grid-line' }, element('span', {}, `${depth} m`));
        line.style.top = `${depth / range * 100}%`;
        grid.append(line);
      }
    }
    const oceanHeight = $('ocean').clientHeight;
    $('diver-track').style.top = `${24 + sim.depth / range * (oceanHeight - 44)}px`;
  }

  renderMotion() {
    const sim = this.sim;
    const paused = this.clock.speed === 0;
    const stopped = !!this.state && !this.state.running && !paused && !(this.host && this.host.suspended);
    $('diver').style.rotate = sim.direction === 1 ? '8deg' : sim.direction === -1 ? '-8deg' : '0deg';
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
    const timeRange = Math.max(600, Math.ceil(sim.elapsed / 600) * 600);
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
    const samples = [...sim.profile, [sim.elapsed, sim.depth]];
    const path = samples.map(([time, depth], index) => `${index === 0 ? 'M' : 'L'}${x(time).toFixed(2)},${y(depth).toFixed(2)}`).join(' ');
    $('profile-line').setAttribute('d', path);
    $('profile-area').setAttribute('d', `${path} L${x(sim.elapsed).toFixed(2)},${top} L${left},${top} Z`);
    $('profile-point').setAttribute('cx', x(sim.elapsed));
    $('profile-point').setAttribute('cy', y(sim.depth));
    setText($('profile-duration'), `${durationText(sim.elapsed)} elapsed`);
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
  }

  resetIndicator(id) {
    const slot = GAME_INDICATORS[id];
    if (!slot) return;
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
