// The worker-side logic of the browser application, independent of any worker global so the Node tests drive the
// very same code: firmware slots, boot, real-time pacing, UI actions, frame and state publishing, profile
// persistence, evidence capture and profile import/export.
//
// A `Runtime` talks to its host through `options.post(message, transferList)` and receives requests through
// `runtime.receive(message)` (see handlers below; every request with an `id` is answered by a `response`).
// Broadcast messages: `state`, `frame`, `download`, `notice`, `crash`, `lifecycle`.
//
// Releases: the engine identifies each firmware file's release (TRITON or NEPTUN); a slot refuses a file whose
// release differs from the other slot's, a session runs one release, and its profile lives in that release's own
// storage area (TRITON keeps the original `profile` area). Every session creation passes a random `historyNonce`
// and the `i2cIdleHigh` fixture option.
//
// Pacing (see README.md): the engine runs in slices of 10 virtual ms. Each timer tick compares the virtual time
// with `epochVirtual + elapsed wall time * speed` and runs slices until it has caught up or its wall-clock
// budget (10 ms) is used, so incoming UI messages are handled between slices. A backlog above 250 virtual ms is
// forgotten (never spiral) and reported. The measured real-time factor is virtual time per wall time over the
// last two seconds; the capacity factor is virtual time per second of engine execution (the speed the engine
// could sustain if unpaced).

import { parseSurfacePressure } from './deco.js';
import { Engine, EngineError, PROFILE_FILES } from './engine.js';
import { DEFAULT_RELEASE_ID, RELEASE_IDS, describeRelease, mixedPairMessage, profileArea, releaseOf } from './releases.js';
import { makeZip } from './zip.js';
import { MemoryStorage } from './storage.js';

export const SLICE_SECONDS = 0.01;
export const BUDGET_MS = 10;
export const MAX_LAG_SECONDS = 0.25;
export const STATE_INTERVAL_MS = 200;
export const FRAME_INTERVAL_MS = 1000 / 60;
export const HIDDEN_FRAME_INTERVAL_MS = 1000;
export const AUTOSAVE_MS = 2000;
export const SAVE_SOON_MS = 500;
export const CHECKPOINT_MS = 15000;
export const STATS_WINDOW_MS = 2000;
export const DROP_MEMORY_MS = 60000;
export const HIDDEN_STATE_INTERVAL_MS = 1000;
export const MAX_FRAMES_IN_FLIGHT = 3;
export const ADVANCE_CHUNK_SECONDS = 0.5;
export const MAX_FIRMWARE_BYTES = 16 * 1024 * 1024;

const ROLES = ['main', 'handset'];
const URGENT = new Set(['speed', 'visibility', 'background', 'ui', 'recycle', 'cancel-advance']);

function describeError(error) {
  if (error instanceof EngineError) return { message: error.message, kind: 'engine', profileProblem: !!error.profileProblem };
  return { message: (error && error.message) || String(error), kind: (error && error.name) || 'Error' };
}

function isTrap(error) {
  return (typeof WebAssembly !== 'undefined' && error instanceof WebAssembly.RuntimeError) ||
    (error instanceof RangeError && /memory|stack/i.test(error.message || ''));
}

/** Two random 32-bit words (the page's entropy for the engine: output-history epochs and the history nonce). */
function defaultRandomWords() {
  const words = new Uint32Array(2);
  if (typeof crypto !== 'undefined' && crypto.getRandomValues) {
    crypto.getRandomValues(words);
  } else {
    words[0] = Math.floor(Math.random() * 0x100000000);
    words[1] = Math.floor(Math.random() * 0x100000000);
  }
  return words;
}

/** A random 53-bit integer: exactly representable in JSON and in the engine's u64 `historyNonce`. */
export function nonceFromWords(words) {
  return (words[0] & 0x1fffff) * 0x100000000 + words[1];
}

function timestampName(date = new Date()) {
  const p = (n, w = 2) => String(n).padStart(w, '0');
  return `${date.getUTCFullYear()}${p(date.getUTCMonth() + 1)}${p(date.getUTCDate())}T${p(date.getUTCHours())}${p(date.getUTCMinutes())}${p(date.getUTCSeconds())}Z`;
}

/** Turns an `inspect` report into the message the entry screen shows for a refused file. */
function refusalMessage(report) {
  if (report.message) return report.message;
  if (report.error) return `Not a valid S-record file: ${report.error}`;
  const failed = (report.checks || []).filter((check) => !check.ok).map((check) => `${check.name}: ${check.detail}`);
  return failed.length ? failed.join('; ') : 'The file is not one of the two supported firmware images.';
}

export class Runtime {
  /**
   * @param {object} options
   * @param {(message: object, transfer?: Transferable[]) => void} options.post
   * @param {() => Promise<Engine>} options.loadEngine
   * @param {() => Promise<{storage: object, problems: string[]}>} [options.openStorage]
   * @param {() => Promise<(() => void)|null>} [options.acquireLock] resolves to a release function, or null when
   *   another tab holds the profile
   * @param {() => number} [options.now] monotonic milliseconds (performance.now)
   * @param {() => number} [options.wallClock] epoch milliseconds (Date.now)
   * @param {(fn: Function, ms: number) => any} [options.setTimer]
   * @param {(handle: any) => void} [options.clearTimer]
   * @param {() => Uint32Array} [options.randomWords] two random 32-bit words (crypto.getRandomValues by default)
   */
  constructor(options) {
    this.randomWords = options.randomWords || defaultRandomWords;
    this.post = options.post;
    this.loadEngine = options.loadEngine;
    this.openStorage = options.openStorage || (async () => ({ storage: new MemoryStorage(), problems: [] }));
    this.acquireLock = options.acquireLock || (async () => () => {});
    this.now = options.now || (() => performance.now());
    this.wallClock = options.wallClock || (() => Date.now());
    this.setTimer = options.setTimer || ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = options.clearTimer || ((handle) => clearTimeout(handle));

    this.engine = null;
    this.storage = null;
    this.storageProblems = [];
    this.firmware = { main: null, handset: null };
    this.queue = Promise.resolve();
    this.crashed = null;

    this.session = null; // {config, persist, releaseLock, generation}
    this.generation = 0;
    this.profileEpoch = 0; // bumped whenever the profile (inputs, LED labels, storage) is replaced from outside the firmware
    this.speed = 1; // virtual seconds per wall second; null = unpaced
    this.hidden = false;
    this.backgroundPolicy = 'run'; // 'run' | 'pause'
    this.uartOpen = false;

    this.loopTimer = null;
    this.stateTimer = null;
    this.frameTimer = null;
    this.saveTimer = null;
    this.saveSoon = false;
    this.pace = { epochWall: 0, epochVirtual: 0 };
    this.stats = { samples: [], drops: [], lastDropWall: -Infinity, ticks: 0 };
    this.framesInFlight = 0; // frames posted to the page and not yet handed back (`recycle`)
    this.framePending = false;
    this.lagSeconds = 0;
    this.stateDirty = true;
    this.lastStatePost = -Infinity;
    this.lastFramePost = -Infinity;
    this.lastFrameVersion = -1;
    this.lastFrameGeneration = -1;
    this.spareBuffers = [];
    this.lastCapture = null;
    this.advancing = null;
    this.lastSave = { wall: null, error: null };
    this.lastCheckpointWall = 0;
    this.saving = Promise.resolve();
    this.virtual = 0;
    this.noticesSent = new Set();
  }

  // ---- request intake --------------------------------------------------------------------------

  receive(message) {
    if (!message || typeof message.type !== 'string') return;
    if (URGENT.has(message.type)) {
      this.dispatch(message);
      return;
    }
    this.queue = this.queue.then(() => this.dispatch(message));
  }

  async dispatch(message) {
    const { id, type } = message;
    try {
      if (this.crashed && type !== 'init') throw new Error(`The engine stopped after an internal error (${this.crashed.message}). Reload the page.`);
      const handler = this[`on_${type.replace(/-/g, '_')}`];
      if (typeof handler !== 'function') throw new Error(`unknown request: ${type}`);
      const result = await handler.call(this, message);
      if (id !== undefined) this.post({ type: 'response', id, ok: true, result });
    } catch (error) {
      if (isTrap(error)) this.crash(error);
      if (id !== undefined) this.post({ type: 'response', id, ok: false, error: describeError(error) });
      else this.notice('error', describeError(error).message);
    }
  }

  notice(level, text, key) {
    if (key) {
      if (this.noticesSent.has(key)) return;
      this.noticesSent.add(key);
    }
    this.post({ type: 'notice', level, text });
  }

  crash(error) {
    if (this.crashed) return;
    let panic = '';
    try { panic = this.engine ? this.engine.panicMessage() : ''; } catch (_) { /* the instance is unusable */ }
    this.crashed = { message: (error && error.message) || String(error), panic };
    this.stopTimers();
    this.post({ type: 'crash', message: this.crashed.message, panic });
  }

  // ---- startup and firmware --------------------------------------------------------------------

  async on_init() {
    this.engine = await this.loadEngine();
    const opened = await this.openStorage();
    this.storage = opened.storage;
    this.storageProblems = opened.problems || [];
    return this.on_info();
  }

  /**
   * What the entry screen needs: engine, storage backend, the remembered firmware pair and the stored profile of
   * every known release (`profiles[releaseId]`, null when there is none).
   */
  async on_info() {
    return {
      engine: this.engine.name,
      storage: { kind: this.storage.kind, persistent: this.storage.persistent, problems: this.storageProblems },
      remembered: await this.rememberedInfo(),
      profiles: await this.profilesInfo(),
      releases: RELEASE_IDS.map((id) => describeRelease(id)),
    };
  }

  async rememberedInfo() {
    try {
      const raw = await this.storage.read('firmware', 'index.json');
      if (!raw) return null;
      const index = JSON.parse(new TextDecoder().decode(raw));
      const stored = new Set((await this.storage.list('firmware')).map((file) => file.name));
      const files = {};
      for (const role of ROLES) {
        if (index[role] && stored.has(`${role}.srec`)) {
          // An index written before releases existed has no `release`: those files are the TRITON pair.
          files[role] = { ...index[role], release: describeRelease(index[role].release || DEFAULT_RELEASE_ID) };
        }
      }
      return Object.keys(files).length ? files : null;
    } catch (_) {
      return null;
    }
  }

  /** The saved profile of one release: its files and when it was last written (null when there is none). */
  async profileInfo(releaseId = DEFAULT_RELEASE_ID) {
    try {
      const files = await this.storage.list(profileArea(releaseId));
      const known = files.filter((file) => PROFILE_FILES.includes(file.name));
      if (!known.length) return null;
      return { release: describeRelease(releaseId), files: known, modified: Math.max(...known.map((file) => file.modified || 0)) };
    } catch (_) {
      return null;
    }
  }

  async profilesInfo() {
    const profiles = {};
    for (const id of RELEASE_IDS) profiles[id] = await this.profileInfo(id);
    return profiles;
  }

  /**
   * Why `release` may not take the place of `role`'s file: the file held for the other role comes from another
   * release (main and handset images only work together within one release). Returns the message, or null.
   */
  mixedPairProblem(role, release, name, held = this.firmware) {
    const otherRole = role === 'main' ? 'handset' : 'main';
    const other = held[otherRole];
    if (!release || !other || !other.release || other.release.id === release.id) return null;
    return mixedPairMessage({ name, role, release }, otherRole, other);
  }

  /** Verifies one SREC file; an accepted file is kept (in memory) for its role. */
  async on_inspect({ name, bytes }) {
    const data = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const result = { name, size: data.length, accepted: false, role: null, release: null, message: '', report: null };
    if (data.length > MAX_FIRMWARE_BYTES) {
      result.message = `The file is ${data.length} bytes; the firmware images are about 1 to 4 MB. Choose the .srec files.`;
      return result;
    }
    const report = this.engine.inspectFirmware(data);
    result.report = report;
    result.role = report.role || null;
    result.release = releaseOf(report);
    if (report.ok && report.role) {
      const mixed = this.mixedPairProblem(report.role, result.release, name);
      if (mixed) {
        result.message = mixed;
        result.conflict = true;
        return result;
      }
      this.firmware[report.role] = { name, size: data.length, bytes: data, report, release: result.release };
      result.accepted = true;
    } else {
      result.message = refusalMessage(report);
    }
    return result;
  }

  async on_clear_firmware({ role }) {
    if (ROLES.includes(role)) this.firmware[role] = null;
    return {};
  }

  /**
   * Takes the remembered pair (they are verified again). The remembered files replace the files held for the same
   * roles; a remembered file that does not match a held file of the other role (or the other remembered file) is
   * reported and not used.
   */
  async on_use_remembered() {
    const problems = [];
    const index = await this.readFirmwareIndex();
    const candidates = {};
    for (const role of ROLES) {
      const bytes = await this.storage.read('firmware', `${role}.srec`);
      if (!bytes) continue;
      const report = this.engine.inspectFirmware(bytes);
      if (report.ok && report.role === role) {
        const name = (index[role] && index[role].name) || `${role}.srec`;
        candidates[role] = { name, size: bytes.length, bytes, report, release: releaseOf(report), remembered: true };
      } else {
        problems.push(`${role}: ${refusalMessage(report)}`);
      }
    }
    const accepted = [];
    for (const role of Object.keys(candidates)) {
      const candidate = candidates[role];
      const mixed = this.mixedPairProblem(role, candidate.release, candidate.name, { ...this.firmware, ...candidates, [role]: null });
      if (mixed) problems.push(`${role}: ${mixed}`);
      else accepted.push(role);
    }
    for (const role of accepted) this.firmware[role] = candidates[role];
    return {
      accepted: accepted.map((role) => ({ role, name: candidates[role].name, size: candidates[role].size, report: candidates[role].report, release: candidates[role].release })),
      problems,
    };
  }

  async readFirmwareIndex() {
    try {
      const raw = await this.storage.read('firmware', 'index.json');
      return raw ? JSON.parse(new TextDecoder().decode(raw)) : {};
    } catch (_) {
      return {};
    }
  }

  async on_forget() {
    await this.storage.clear('firmware');
    return {};
  }

  /**
   * Stores the verified SRECs in the origin-private file system (one remembered pair: remembering another release
   * replaces it). Firmware is never put anywhere else (no IndexedDB).
   */
  async rememberFirmware() {
    if (this.storage.kind !== 'opfs') {
      throw new EngineError('Remembering firmware files needs the origin-private file system, which this browser does not provide here');
    }
    await this.storage.clear('firmware');
    const index = {};
    for (const role of ROLES) {
      const slot = this.firmware[role];
      if (!slot) continue;
      await this.storage.write('firmware', `${role}.srec`, slot.bytes);
      index[role] = { name: slot.name, size: slot.size, sha256: slot.report.srecSha256, release: (slot.release || describeRelease(DEFAULT_RELEASE_ID)).id, savedAt: this.wallClock() };
    }
    await this.storage.write('firmware', 'index.json', new TextEncoder().encode(JSON.stringify(index)));
  }

  // ---- sessions --------------------------------------------------------------------------------

  static normalizeConfig(options = {}) {
    const dual = (options.mode || 'dual') !== 'handset';
    const adc = options.adcSample === undefined ? 400 : Number(options.adcSample);
    if (!Number.isInteger(adc) || adc < 0 || adc > 4095) throw new EngineError('The board-ID ADC sample must be an integer from 0 to 4095');
    const config = {
      mode: dual ? 'dual' : 'handset',
      bootMode: options.bootMode === 'cold' ? 'cold' : 'handset-wake',
      simultaneousStart: !!options.simultaneousStart,
      idleFastForward: options.idleFastForward !== false,
      adcSample: adc,
      startPaused: !!options.startPaused,
      // Fixture (DESIGN 15.3c): the main board's I2C idle lines PB6/PB7/PB10/PB11 are driven high before guest
      // execution, as the external pull-ups would. A start option; on unless switched off.
      i2cIdleHigh: options.i2cIdleHigh !== false,
      // Fixtures (DESIGN "Decompression state handling"), both on unless switched off: the pre-boot EEPROM consistency
      // repair of a tissue block that was never saved, and the start at the surface (a new session also resets the
      // oxygen cells).
      decoStorageFixture: options.decoStorageFixture !== false,
      startAtSurface: options.startAtSurface !== false,
    };
    // The page's surface-pressure setting; the engine's own default (1013.25 mbar) applies without a valid one.
    const surface = parseSurfacePressure(options.surfacePressureMbar);
    if (surface !== null) config.surfacePressureMbar = surface;
    return config;
  }

  /** The release of the firmware a session with `config` needs (both roles must come from the same release). */
  bootRelease(config) {
    const roles = config.mode === 'dual' ? ROLES : ['handset'];
    const releases = roles.map((role) => this.firmware[role] && this.firmware[role].release).filter(Boolean);
    const first = releases[0] || describeRelease(DEFAULT_RELEASE_ID);
    if (releases.some((release) => release.id !== first.id)) {
      throw new EngineError('The main and handset firmware files come from different releases; remove one of them and choose the matching file');
    }
    return first;
  }

  async readProfile(area) {
    const profile = {};
    for (const name of PROFILE_FILES) {
      const data = await this.storage.read(area, name);
      if (data) profile[name] = data;
    }
    return profile;
  }

  /**
   * Creates the engine session and gives it what only the host has: the UTC time and some entropy. Every session
   * (a boot, a profile import, a profile reset) gets a fresh random `historyNonce`, so the output-history epochs of
   * two launches never coincide.
   */
  newEngineSession(config, profile) {
    const nonce = nonceFromWords(this.randomWords());
    this.engine.createSession({ ...config, historyNonce: nonce }, profile);
    this.engine.setClock(this.wallClock() * 1000);
    const words = this.randomWords();
    this.engine.setSeed(words[0], words[1]);
  }

  /** The measured real-time factor and the pacing description go into the state document (and captures). */
  refreshHostInfo() {
    const running = this.session && this.engine.running();
    const factor = running && !this.suspended() ? this.measured().realtimeFactor : null;
    const pacing = this.speed === null
      ? `Browser worker, unpaced (maximum speed): ${SLICE_SECONDS * 1000} virtual-ms slices`
      : `Browser worker real-time pacing at ${this.speed}x: ${SLICE_SECONDS * 1000} virtual-ms slices, catch-up limit ${MAX_LAG_SECONDS} s, backlog above it is dropped`;
    this.engine.setHostInfo(factor, pacing);
  }

  installFirmware(config) {
    for (const role of ROLES) this.engine.clearFirmware(role);
    const needed = config.mode === 'dual' ? ROLES : ['handset'];
    for (const role of needed) {
      const slot = this.firmware[role];
      if (!slot) throw new EngineError(`The ${role} firmware file has not been provided`);
      this.engine.setFirmware(role, slot.bytes);
    }
  }

  async on_boot({ options, remember = false, profile: profileMode = 'stored' }) {
    const config = Runtime.normalizeConfig(options);
    await this.closeSession({ save: true });
    this.installFirmware(config);
    const releaseInfo = this.bootRelease(config);
    const area = profileArea(releaseInfo.id);
    let persist = profileMode !== 'none';
    let unlock = null;
    if (persist) {
      unlock = await this.acquireLock();
      if (!unlock) {
        persist = false;
        this.notice('warning', 'Another tab or window of this application is using the saved profile. This session starts from an empty profile and does not save anything.');
      }
    }
    const profile = persist ? await this.readProfile(area) : {};
    try {
      this.newEngineSession(config, profile);
    } catch (error) {
      if (unlock) unlock();
      const described = describeError(error);
      const problem = new EngineError(described.message);
      problem.profileProblem = persist && Object.keys(profile).length > 0;
      throw problem;
    }
    if (remember) {
      try {
        await this.rememberFirmware();
      } catch (error) {
        this.notice('warning', `The firmware files could not be remembered in this browser: ${describeError(error).message}`);
      }
    }
    this.generation += 1;
    this.profileEpoch += 1;
    this.session = { config, persist, unlock, generation: this.generation, releaseInfo, area };
    this.lastCheckpointWall = this.now();
    this.afterSessionChange({ postFrame: true });
    this.startTimers();
    if (this.engine.running()) this.ensureLoop();
    return { state: this.engine.state(), persist, release: releaseInfo, hostStatus: this.hostStatus() };
  }

  /** Saves and closes the current session (if any). */
  async closeSession({ save = true } = {}) {
    this.stopTimers();
    this.advancing = null;
    if (!this.session) return;
    const session = this.session;
    this.session = null;
    let files = [];
    if (this.engine.active) files = this.engine.shutdown();
    if (save && session.persist && files.length) await this.writeProfile(session.area, files, { replace: true });
    if (session.unlock) session.unlock();
    this.post({ type: 'lifecycle', event: 'closed' });
  }

  async on_close_session() {
    await this.closeSession({ save: true });
    return {};
  }

  afterSessionChange({ postFrame = true } = {}) {
    this.rebase();
    this.stats.samples.length = 0;
    this.stateDirty = true;
    this.lastFrameVersion = -1;
    this.virtual = this.engine.time();
    this.post({ type: 'lifecycle', event: 'session', generation: this.session ? this.session.generation : 0 });
    this.postState(true);
    if (postFrame) this.postFrame(true);
  }

  // ---- actions ---------------------------------------------------------------------------------

  requireSession() {
    if (!this.session) throw new EngineError('No emulator session is running');
  }

  async on_action({ request }) {
    this.requireSession();
    const name = request && request.action;
    if (name === 'advance') return this.advance(request);
    if (name === 'capture') {
      await this.on_capture();
      return this.engine.state();
    }
    const wasRunning = this.engine.running();
    const state = this.engine.action(request);
    this.virtual = this.engine.time();
    if (name === 'reset' || name === 'cold' || name === 'wake' || name === 'serial') {
      // A surface pressure the action carried is the session's setting from now on (a later profile import or reset
      // creates its session with it).
      const surface = parseSurfacePressure(request.surfacePressureMbar);
      if (surface !== null) this.session.config = { ...this.session.config, surfacePressureMbar: surface };
      this.session.generation = ++this.generation;
      this.afterSessionChange({ postFrame: true });
      await this.saveProfile({ reason: name });
    } else if (name === 'inputs' || name === 'led-colors') {
      this.scheduleSave();
    }
    if (name === 'resume' || (this.engine.running() && !wasRunning)) this.rebase();
    this.stateDirty = true;
    this.postState(true);
    this.postFrame(true);
    if (this.engine.running()) this.ensureLoop();
    return state;
  }

  /** The viewer's Advance, run in short chunks so the worker stays responsive and the state can be followed. */
  async advance(request) {
    const seconds = Number(request.seconds === undefined ? 1 : request.seconds);
    if (!Number.isFinite(seconds) || !(seconds > 0 && seconds <= 20)) throw new EngineError('Advance must be between zero and 20 virtual seconds');
    if (this.advancing) throw new EngineError('An advance is already running');
    this.stopLoop();
    const job = { total: seconds, done: 0, cancel: false };
    this.advancing = job;
    let state = null;
    try {
      let remaining = seconds;
      while (remaining > 1e-9 && !job.cancel) {
        const chunk = remaining > ADVANCE_CHUNK_SECONDS + 1e-9 ? ADVANCE_CHUNK_SECONDS : remaining;
        state = this.engine.action({ action: 'advance', seconds: chunk });
        remaining -= chunk;
        job.done = seconds - remaining;
        this.virtual = this.engine.time();
        this.stateDirty = true;
        this.postState(true, { advance: { total: job.total, done: job.done } });
        this.postFrame(false);
        // Standby halts both CPUs, so nothing more can run. An earlier error stop does not end an Advance (the
        // runner's Step and Advance still execute after the terminal handler was hit).
        if (state.standby) break;
        await this.yieldToMessages();
      }
    } finally {
      this.advancing = null;
    }
    this.rebase();
    this.postState(true);
    this.postFrame(true);
    this.scheduleSave();
    return state || this.engine.state();
  }

  async on_cancel_advance() {
    if (this.advancing) this.advancing.cancel = true;
    return {};
  }

  yieldToMessages() {
    return new Promise((resolve) => this.setTimer(resolve, 0));
  }

  // ---- capture and profile files ---------------------------------------------------------------

  download(filename, mime, bytes) {
    const copy = bytes.buffer.byteLength === bytes.length ? bytes.buffer : bytes.slice().buffer;
    this.post({ type: 'download', filename, mime, bytes: copy }, [copy]);
  }

  async on_capture() {
    this.requireSession();
    this.engine.setClock(this.wallClock() * 1000);
    this.refreshHostInfo();
    const { name, files } = this.engine.capture();
    const zip = makeZip(files.map((file) => ({ name: file.name, data: file.data })));
    const filename = `ngc-capture-${String(name || timestampName()).replace(/^capture-/, '')}.zip`;
    this.lastCapture = filename;
    this.download(filename, 'application/zip', zip);
    this.stateDirty = true;
    this.postState(true);
    return { name, filename, entries: files.map((file) => file.name), bytes: zip.length };
  }

  async on_export_profile() {
    this.requireSession();
    const files = this.engine.exportProfile();
    if (this.session.persist) await this.writeProfile(this.session.area, files);
    const zip = makeZip(files.map((file) => ({ name: file.name, data: file.data })));
    const filename = `ngc-profile-${timestampName()}.zip`;
    this.download(filename, 'application/zip', zip);
    return { filename, entries: files.map((file) => file.name), bytes: zip.length };
  }

  async on_import_profile({ files }) {
    this.requireSession();
    if (!Array.isArray(files) || files.length === 0) throw new EngineError('The archive contains no profile files');
    const profile = {};
    for (const file of files) {
      const base = String(file.name).split('/').pop();
      if (!PROFILE_FILES.includes(base)) continue;
      profile[base] = file.data instanceof Uint8Array ? file.data : new Uint8Array(file.data);
    }
    if (Object.keys(profile).length === 0) {
      throw new EngineError(`The archive contains none of the profile files (${PROFILE_FILES.join(', ')})`);
    }
    const session = this.session;
    const wasPaused = this.pausedByUser();
    this.stopLoop();
    try {
      this.newEngineSession({ ...session.config, startPaused: wasPaused }, profile);
    } catch (error) {
      if (this.engine.running()) this.ensureLoop();
      throw new EngineError(`The profile was not imported (the running session is unchanged): ${describeError(error).message}`);
    }
    session.generation = ++this.generation;
    this.profileEpoch += 1;
    this.afterSessionChange({ postFrame: true });
    if (this.engine.running()) this.ensureLoop();
    if (session.persist) {
      await this.eraseProfile(session.area);
      await this.writeProfile(session.area, Object.entries(profile).map(([name, data]) => ({ name, data })));
    }
    return { imported: Object.keys(profile), state: this.engine.state() };
  }

  /** Paused by the user (as opposed to stopped by standby or an error). */
  pausedByUser() {
    if (!this.session || this.engine.running()) return false;
    const state = this.engine.state();
    return !(state && (state.standby || state.error));
  }

  /**
   * Erases the saved profile of the running session's release (restarting the boards from factory-fresh storage),
   * or, without a session, of the release named by `release` (the entry screen).
   */
  async on_reset_profile({ release } = {}) {
    if (this.session) {
      this.stopLoop();
      this.newEngineSession({ ...this.session.config, startPaused: this.pausedByUser() }, {});
      this.session.generation = ++this.generation;
      this.profileEpoch += 1;
      this.afterSessionChange({ postFrame: true });
      if (this.engine.running()) this.ensureLoop();
    }
    if (this.session) {
      if (this.session.persist) await this.eraseProfile(this.session.area);
    } else {
      await this.eraseProfile(profileArea(release || DEFAULT_RELEASE_ID));
    }
    return {};
  }

  // ---- persistence -----------------------------------------------------------------------------

  /** Deletes the stored profile files, in order with the writes (an autosave in flight cannot resurrect them). */
  eraseProfile(area) {
    const job = this.saving.then(async () => {
      try {
        await this.storage.clear(area);
      } catch (error) {
        this.notice('error', `Erasing the saved profile failed: ${describeError(error).message}`);
      }
    });
    this.saving = job;
    return job;
  }

  /** Serialises writes so two saves never interleave. */
  writeProfile(area, files, { replace = false } = {}) {
    const job = this.saving.then(async () => {
      try {
        if (replace) {
          const keep = new Set(files.map((file) => file.name));
          for (const name of PROFILE_FILES) if (!keep.has(name)) await this.storage.remove(area, name);
        }
        for (const file of files) await this.storage.write(area, file.name, file.data);
        this.lastSave = { wall: this.wallClock(), error: null };
      } catch (error) {
        this.lastSave = { wall: this.lastSave.wall, error: describeError(error).message };
        this.notice('error', `Saving the profile failed: ${this.lastSave.error}`, `save:${this.lastSave.error}`);
      }
    });
    this.saving = job;
    return job;
  }

  /** Writes what changed (or, with `full`, the whole profile with a fresh RTC checkpoint). */
  async saveProfile({ full = false, reason = '' } = {}) {
    if (!this.session || !this.session.persist || !this.engine.active) return;
    const files = full ? this.engine.exportProfile() : this.engine.profileChanges();
    if (files.length) await this.writeProfile(this.session.area, files);
    if (full) this.lastCheckpointWall = this.now();
    void reason;
  }

  /** Brings the next autosave forward (inputs, LED labels, an Advance, a stop): changes reach storage within 0.5 s. */
  scheduleSave() {
    if (!this.session || this.crashed || this.saveSoon) return;
    this.saveSoon = true;
    if (this.saveTimer !== null) this.clearTimer(this.saveTimer);
    this.saveTimer = this.setTimer(() => this.autosave(), SAVE_SOON_MS);
  }

  async autosave() {
    this.saveTimer = null;
    this.saveSoon = false;
    if (!this.session) return;
    try {
      const checkpoint = this.now() - this.lastCheckpointWall >= CHECKPOINT_MS && this.engine.running();
      if (checkpoint) await this.saveProfile({ full: true });
      else await this.saveProfile({});
    } catch (error) {
      if (isTrap(error)) this.crash(error);
      else this.notice('error', `Saving the profile failed: ${describeError(error).message}`, 'autosave');
    }
    if (this.session && !this.crashed && this.saveTimer === null) this.saveTimer = this.setTimer(() => this.autosave(), AUTOSAVE_MS);
  }

  async on_flush() {
    if (!this.session) return {};
    await this.saveProfile({ full: true });
    await this.saving;
    return { saved: this.lastSave.wall };
  }

  // ---- immediate requests ----------------------------------------------------------------------

  async on_speed({ speed }) {
    this.speed = speed === null || speed === undefined ? null : Math.max(0.01, Number(speed));
    if (this.speed !== null && !Number.isFinite(this.speed)) this.speed = 1;
    this.rebase();
    this.stats.samples.length = 0;
    if (this.session && this.engine.running()) this.ensureLoop();
    this.stateDirty = true;
    this.postState(true);
    return {};
  }

  async on_visibility({ hidden }) {
    this.hidden = !!hidden;
    if (!this.session) return {};
    if (this.hidden) {
      if (this.backgroundPolicy === 'pause') this.stopLoop();
      this.saveProfile({ full: true }).catch(() => {});
    } else {
      this.rebase();
      this.stats.samples.length = 0;
      this.stateDirty = true;
      this.postState(true);
      this.postFrame(true);
      if (this.engine.running()) this.ensureLoop();
    }
    return {};
  }

  async on_background({ policy }) {
    this.backgroundPolicy = policy === 'pause' ? 'pause' : 'run';
    if (this.session && this.hidden) {
      if (this.backgroundPolicy === 'pause') this.stopLoop();
      else if (this.engine.running()) this.ensureLoop();
    }
    this.stateDirty = true;
    return {};
  }

  async on_ui({ uartOpen }) {
    this.uartOpen = !!uartOpen;
    this.stateDirty = true;
    if (this.session) this.postState(true);
    return {};
  }

  /** The page hands a drawn frame buffer back: it can be reused, and a frame held back by backpressure goes out. */
  async on_recycle({ buffer }) {
    this.framesInFlight = Math.max(0, this.framesInFlight - 1);
    if (buffer instanceof ArrayBuffer && this.spareBuffers.length < MAX_FRAMES_IN_FLIGHT) this.spareBuffers.push(buffer);
    if (this.framePending && this.session && !this.crashed) this.postFrame(this.framePending === 'force');
    return undefined;
  }

  // ---- pacing ----------------------------------------------------------------------------------

  suspended() {
    return this.hidden && this.backgroundPolicy === 'pause';
  }

  /** Restarts the wall-clock reference: virtual time now corresponds to wall time now. */
  rebase() {
    this.pace.epochWall = this.now();
    this.pace.epochVirtual = this.session && this.engine ? this.engine.time() : 0;
    this.lagSeconds = 0;
  }

  ensureLoop() {
    if (this.loopTimer !== null || !this.session || this.crashed || this.suspended() || this.advancing) return;
    if (!this.engine.running()) return;
    this.rebase();
    this.stats.samples.length = 0;
    this.loopTimer = this.setTimer(() => this.tick(), 0);
  }

  stopLoop() {
    if (this.loopTimer !== null) {
      this.clearTimer(this.loopTimer);
      this.loopTimer = null;
    }
  }

  tick() {
    this.loopTimer = null;
    if (!this.session || this.crashed || this.advancing || this.suspended()) return;
    try {
      this.runSlices();
    } catch (error) {
      if (isTrap(error)) {
        this.crash(error);
      } else {
        this.notice('error', `The emulation loop stopped: ${describeError(error).message}`);
        this.stopLoop();
      }
    }
  }

  runSlices() {
    const engine = this.engine;
    if (!engine.running()) {
      this.onLoopStopped();
      return;
    }
    const t0 = this.now();
    const unpaced = this.speed === null;
    let virtual = engine.time();
    let target = unpaced ? Infinity : this.pace.epochVirtual + ((t0 - this.pace.epochWall) / 1000) * this.speed;
    if (!unpaced && target - virtual > MAX_LAG_SECONDS) {
      // Never spiral: forget the backlog above the catch-up limit and carry on from there.
      this.stats.drops.push({ wall: t0, seconds: target - virtual - MAX_LAG_SECONDS });
      this.stats.lastDropWall = t0;
      this.pace.epochVirtual = virtual + MAX_LAG_SECONDS;
      this.pace.epochWall = t0;
      target = this.pace.epochVirtual;
    }
    let running = true;
    while (virtual < target) {
      running = engine.runFor(SLICE_SECONDS);
      virtual = engine.time();
      if (!running || this.now() - t0 >= BUDGET_MS) break;
    }
    const t1 = this.now();
    this.virtual = virtual;
    this.lagSeconds = unpaced ? 0 : Math.max(0, target - virtual);
    this.recordSample(t1, virtual, t1 - t0);
    this.publish(t1);
    if (!running) {
      this.onLoopStopped();
      return;
    }
    if (unpaced || virtual < target) {
      this.loopTimer = this.setTimer(() => this.tick(), 0);
    } else {
      const waitMs = Math.max(0, ((virtual - target) * 1000) / this.speed);
      this.loopTimer = this.setTimer(() => this.tick(), waitMs);
    }
  }

  onLoopStopped() {
    this.stats.samples.length = 0;
    this.virtual = this.engine.time();
    this.stateDirty = true;
    this.postState(true);
    this.postFrame(true);
    this.scheduleSave();
  }

  recordSample(wall, virtual, busyMs) {
    const samples = this.stats.samples;
    const previous = samples.length ? samples[samples.length - 1] : { busy: 0 };
    samples.push({ wall, virtual, busy: previous.busy + busyMs });
    while (samples.length > 2 && wall - samples[0].wall > STATS_WINDOW_MS) samples.shift();
    this.stats.ticks += 1;
  }

  /** Real-time factor (virtual seconds per wall second) and engine capacity over the last two seconds. */
  measured() {
    const samples = this.stats.samples;
    if (samples.length < 2) return { realtimeFactor: null, capacityFactor: null };
    const first = samples[0];
    const last = samples[samples.length - 1];
    const wall = (last.wall - first.wall) / 1000;
    const busy = (last.busy - first.busy + 1e-9) / 1000;
    const virtual = last.virtual - first.virtual;
    return { realtimeFactor: wall > 0 ? virtual / wall : null, capacityFactor: busy > 0 ? virtual / busy : null };
  }

  /** Virtual seconds of backlog forgotten during the last minute (the catch-up limit was exceeded). */
  recentDroppedSeconds() {
    const now = this.now();
    const drops = this.stats.drops;
    while (drops.length && now - drops[0].wall > DROP_MEMORY_MS) drops.shift();
    return drops.reduce((sum, drop) => sum + drop.seconds, 0);
  }

  hostStatus() {
    const { realtimeFactor, capacityFactor } = this.measured();
    const running = !!(this.session && this.engine.running());
    const recentDrop = this.now() - this.stats.lastDropWall < 3000;
    const lagMs = this.speed === null ? 0 : (this.lagSeconds * 1000) / this.speed;
    return {
      generation: this.session ? this.session.generation : 0,
      profileEpoch: this.profileEpoch,
      release: this.session ? this.session.releaseInfo : null,
      // Session options the engine build did not understand (an engine older than the page); empty otherwise.
      unsupportedOptions: this.engine ? [...this.engine.unsupportedOptions] : [],
      engine: this.engine ? this.engine.name : '',
      speed: this.speed,
      running,
      suspended: this.suspended(),
      hidden: this.hidden,
      backgroundPolicy: this.backgroundPolicy,
      realtimeFactor: running ? realtimeFactor : 0,
      capacityFactor: running ? capacityFactor : null,
      keepingUp: this.speed === null ? true : !(recentDrop || lagMs > 100),
      lagMs,
      droppedSeconds: this.recentDroppedSeconds(),
      advance: this.advancing ? { total: this.advancing.total, done: this.advancing.done } : null,
      storage: {
        kind: this.storage ? this.storage.kind : 'none',
        persistent: this.storage ? this.storage.persistent : false,
        saving: !!(this.session && this.session.persist),
        lastSave: this.lastSave.wall,
        error: this.lastSave.error,
      },
      lastCapture: this.lastCapture,
    };
  }

  // ---- publishing ------------------------------------------------------------------------------

  startTimers() {
    this.stopTimers();
    this.stateTimer = this.setTimer(() => this.stateTick(), STATE_INTERVAL_MS);
    this.saveTimer = this.setTimer(() => this.autosave(), AUTOSAVE_MS);
  }

  stopTimers() {
    this.stopLoop();
    this.saveSoon = false;
    for (const name of ['stateTimer', 'frameTimer', 'saveTimer']) {
      if (this[name] !== null) {
        this.clearTimer(this[name]);
        this[name] = null;
      }
    }
  }

  /** State is published at 5 Hz while the page is visible and once a second while it is hidden. */
  stateInterval() {
    return this.hidden ? HIDDEN_STATE_INTERVAL_MS : STATE_INTERVAL_MS;
  }

  stateTick() {
    this.stateTimer = null;
    if (!this.session || this.crashed) return;
    try {
      const due = this.now() - this.lastStatePost >= this.stateInterval();
      if (this.stateDirty || (this.engine.running() && due)) this.postState(true);
    } catch (error) {
      if (isTrap(error)) this.crash(error);
    }
    if (this.session && !this.crashed) this.stateTimer = this.setTimer(() => this.stateTick(), STATE_INTERVAL_MS);
  }

  /** Called after every slice batch: throttled frame and state publishing. */
  publish(wall) {
    if (wall - this.lastStatePost >= this.stateInterval()) this.postState(false);
    this.postFrame(false, wall);
  }

  postState(force, extra = {}) {
    if (!this.session || this.crashed) return;
    const wall = this.now();
    if (!force && wall - this.lastStatePost < this.stateInterval()) return;
    this.lastStatePost = wall;
    this.stateDirty = false;
    this.refreshHostInfo();
    const state = JSON.parse(this.engine.stateText());
    if (!this.uartOpen && Array.isArray(state.uartConsole)) {
      for (const channel of state.uartConsole) {
        delete channel.text;
        delete channel.hex;
      }
    }
    const host = this.hostStatus();
    if (extra.advance) host.advance = extra.advance;
    this.post({ type: 'state', state, host });
  }

  /**
   * Sends the LCD frame when its visible pixels changed (always with `force`). At most 60 frames per second while
   * the page is visible. A hidden page still gets one frame per second instead of none: the browser's visibility
   * flag is only a hint (embedded panes and occluded windows report "hidden" while the user looks at the page, and
   * the first frame after boot is sent before the page can know the session), and one frame a second costs
   * almost nothing. Nothing is lost by throttling: a newer frame always replaces an older one.
   */
  postFrame(force, wall = this.now()) {
    if (!this.session || this.crashed) return;
    const interval = this.hidden ? HIDDEN_FRAME_INTERVAL_MS : FRAME_INTERVAL_MS;
    if (!force && wall - this.lastFramePost < interval) {
      if (this.frameTimer === null) {
        this.frameTimer = this.setTimer(() => {
          this.frameTimer = null;
          if (this.session && !this.crashed) this.postFrame(false);
        }, Math.max(0, interval - (wall - this.lastFramePost)));
      }
      return;
    }
    const info = this.engine.frame();
    const generation = this.session.generation;
    if (!force && info.version === this.lastFrameVersion && generation === this.lastFrameGeneration) return;
    if (info.length === 0) return;
    if (this.framesInFlight >= MAX_FRAMES_IN_FLIGHT) {
      // Backpressure: the page has not consumed the earlier frames yet; the newest one is sent when it does.
      this.framePending = this.framePending === 'force' || force ? 'force' : true;
      return;
    }
    let buffer = this.spareBuffers.pop();
    if (!buffer || buffer.byteLength !== info.length) buffer = new ArrayBuffer(info.length);
    this.engine.copyFrame(info, new Uint8Array(buffer));
    this.lastFrameVersion = info.version;
    this.lastFrameGeneration = generation;
    this.lastFramePost = wall;
    this.framePending = false;
    this.framesInFlight += 1;
    this.post({ type: 'frame', width: info.width, height: info.height, version: info.version, generation, buffer }, [buffer]);
  }
}

/** Convenience used by the worker: a zero-delay timer that avoids the 4 ms clamp of nested `setTimeout`. */
export function makeTimers() {
  if (typeof MessageChannel === 'undefined') {
    return { setTimer: (fn, ms) => setTimeout(fn, ms), clearTimer: (handle) => clearTimeout(handle) };
  }
  const channel = new MessageChannel();
  const pending = new Map();
  let nextId = 1;
  channel.port1.onmessage = (event) => {
    const entry = pending.get(event.data);
    if (entry) {
      pending.delete(event.data);
      entry();
    }
  };
  return {
    setTimer(fn, ms) {
      if (ms > 0) return { timeout: setTimeout(fn, ms) };
      const id = nextId++;
      pending.set(id, fn);
      channel.port2.postMessage(id);
      return { immediate: id };
    },
    clearTimer(handle) {
      if (!handle) return;
      if (handle.timeout !== undefined) clearTimeout(handle.timeout);
      else pending.delete(handle.immediate);
    },
  };
}
