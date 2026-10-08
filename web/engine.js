// Thin wrapper around the ngc-wasm C ABI (crates/ngc-wasm/src/lib.rs).
//
// Pure ES module without DOM or worker globals: the browser worker (runtime.js), the Node tests
// (test-node.mjs) and the benchmark share it. It owns the WebAssembly instance, moves bytes and text across
// linear memory and turns the ABI's status codes into exceptions. It never reads a wall clock.

/** Files of a profile, byte-compatible with the Renode runner's data directory. */
export const PROFILE_FILES = ['eeprom.bin', 'nor.ngc', 'rtc-state.json', 'inputs.json', 'led-colors.json'];
const PROFILE_KIND = { 'eeprom.bin': 0, 'nor.ngc': 1, 'rtc-state.json': 2, 'inputs.json': 3, 'led-colors.json': 4 };
/** Session options an older engine build does not know (see `createSession`). */
const OPTIONAL_OPTIONS = ['historyNonce', 'i2cIdleHigh'];

/** A failure reported by the engine (verification, validation, bad profile); `message` is user-readable. */
export class EngineError extends Error {
  constructor(message, code = 1) {
    super(message);
    this.name = 'EngineError';
    this.code = code;
  }
}

export class Engine {
  /**
   * @param {string|URL|ArrayBuffer|Uint8Array|WebAssembly.Module} source URL (fetched, streamed when the server
   *   sends application/wasm), module bytes or a compiled module.
   */
  static async load(source) {
    let module;
    let instance;
    if (source instanceof WebAssembly.Module) {
      module = source;
    } else if (typeof source === 'string' || source instanceof URL) {
      const url = String(source);
      let response = await fetch(url, { cache: 'no-store' });
      if (!response.ok) throw new Error(`Cannot load the engine (${response.status} ${response.statusText} for ${url}). Build it with web/build.py.`);
      if (WebAssembly.instantiateStreaming && /application\/wasm/i.test(response.headers.get('content-type') || '')) {
        const result = await WebAssembly.instantiateStreaming(response, {});
        return new Engine(result.instance, result.module);
      }
      module = await WebAssembly.compile(await response.arrayBuffer());
    } else {
      module = await WebAssembly.compile(source);
    }
    const imports = WebAssembly.Module.imports(module);
    const stubs = {};
    for (const entry of imports) stubs[entry.module] = {};
    instance = await WebAssembly.instantiate(module, stubs);
    return new Engine(instance, module);
  }

  constructor(instance, module) {
    this.x = instance.exports;
    this.module = module;
    this.decoder = new TextDecoder();
    this.encoder = new TextEncoder();
    this._u8 = null;
    this.unsupportedOptions = new Set();
    this.x.ngc_init();
    this.name = this.text(this.x.ngc_engine());
    this.version = this.x.ngc_version();
  }

  // ---- linear memory ---------------------------------------------------------------------------

  /** A view of the whole linear memory; re-created when the memory grew (old views are detached). */
  u8() {
    const buffer = this.x.memory.buffer;
    if (this._u8 === null || this._u8.buffer !== buffer) this._u8 = new Uint8Array(buffer);
    return this._u8;
  }

  /** Text of the output buffer (`length` bytes). */
  text(length) {
    if (length === 0) return '';
    const ptr = this.x.ngc_output_ptr();
    return this.decoder.decode(this.u8().subarray(ptr, ptr + length));
  }

  /** The message of the last failure. */
  error() {
    return this.text(this.x.ngc_error());
  }

  /** Copies `data` into freshly allocated linear memory for the duration of `fn(ptr, length)`. */
  withBytes(data, fn) {
    const length = data.length;
    const ptr = this.x.ngc_alloc(length);
    try {
      this.u8().set(data, ptr);
      return fn(ptr, length);
    } finally {
      this.x.ngc_free(ptr, length);
    }
  }

  withText(text, fn) {
    return this.withBytes(this.encoder.encode(text), fn);
  }

  fail(what) {
    const message = this.error();
    return new EngineError(what ? `${what}: ${message}` : message);
  }

  /** The recorded panic message after a trap (empty when none). */
  panicMessage() {
    const length = this.x.ngc_panic_len();
    if (length === 0) return '';
    const ptr = this.x.ngc_panic_ptr();
    return this.decoder.decode(this.u8().subarray(ptr, ptr + length));
  }

  // ---- firmware --------------------------------------------------------------------------------

  /** Verification report of an SREC file (never throws for bad files: `ok` is false and `error`/`checks` say why). */
  inspectFirmware(bytes) {
    return this.withBytes(bytes, (ptr, length) => JSON.parse(this.text(this.x.ngc_firmware_inspect(ptr, length))));
  }

  /** Verifies and keeps a firmware image for `role` ('main' | 'handset'); throws the engine's message on failure. */
  setFirmware(role, bytes) {
    const code = this.withBytes(bytes, (ptr, length) => this.x.ngc_set_firmware(role === 'main' ? 0 : 1, ptr, length));
    if (code !== 0) throw this.fail();
  }

  clearFirmware(role) {
    this.x.ngc_firmware_clear(role === 'main' ? 0 : 1);
  }

  // ---- session ---------------------------------------------------------------------------------

  /**
   * Creates the session from the kept firmware.
   * @param {object} config {mode: 'dual'|'handset', bootMode: 'handset-wake'|'cold', simultaneousStart, idleFastForward,
   *   adcSample, startPaused, historyNonce, i2cIdleHigh}
   * @param {Object<string, Uint8Array|string>} profile files by name (PROFILE_FILES)
   *
   * `historyNonce` and `i2cIdleHigh` are options of newer engine builds (DESIGN 15.3); an older module rejects
   * unknown options, so they are dropped one at a time when the engine says it does not know them. What the engine
   * did not understand is kept in `unsupportedOptions` for the page to report.
   */
  createSession(config = {}, profile = {}) {
    const options = { ...config };
    this.unsupportedOptions = new Set();
    for (;;) {
      this.x.ngc_profile_clear();
      for (const [name, data] of Object.entries(profile)) {
        if (!(name in PROFILE_KIND)) throw new EngineError(`unknown profile file ${name}`);
        const bytes = typeof data === 'string' ? this.encoder.encode(data) : data;
        const code = this.withBytes(bytes, (ptr, length) => this.x.ngc_profile_set(PROFILE_KIND[name], ptr, length));
        if (code !== 0) throw this.fail(name);
      }
      // The staged profile is consumed by every attempt, successful or not, so each retry stages it again.
      const code = this.withText(JSON.stringify(options), (ptr, length) => this.x.ngc_session_create(ptr, length));
      this.x.ngc_profile_clear();
      if (code === 0) return;
      const error = this.fail();
      const unknown = /unknown session option: (\w+)/.exec(error.message);
      if (unknown && OPTIONAL_OPTIONS.includes(unknown[1]) && unknown[1] in options) {
        delete options[unknown[1]];
        this.unsupportedOptions.add(unknown[1]);
        continue;
      }
      throw error;
    }
  }

  get active() {
    return this.x.ngc_session_active() !== 0;
  }

  /** Tells the session the UTC time in microseconds (the engine has no clock); capture names derive from it. */
  setClock(utcMicros) {
    this.x.ngc_session_set_clock(utcMicros);
  }

  /** Mixes 64 bits of host entropy into the ids of later launches (`outputHistoryEpoch`). */
  setSeed(lo, hi) {
    this.x.ngc_session_set_seed(lo >>> 0, hi >>> 0);
  }

  /** The host-measured real-time factor (null: none) and a text describing the pacing, for the state document. */
  setHostInfo(realtimeFactor, pacing = '') {
    this.withText(pacing, (ptr, length) => this.x.ngc_session_host_info(Number.isFinite(realtimeFactor) ? realtimeFactor : NaN, ptr, length));
  }

  destroySession() {
    this.x.ngc_session_destroy();
  }

  /** Runs `seconds` of virtual time; returns whether the session is still running (false: paused / standby / error). */
  runFor(seconds) {
    const code = this.x.ngc_session_run_for(seconds);
    if (code === 2) throw new EngineError('There is no session');
    return code === 0;
  }

  running() {
    return this.x.ngc_session_running() !== 0;
  }

  /** Virtual time in seconds. */
  time() {
    return this.x.ngc_session_time();
  }

  /** One UI action (runner schema `{action, ...}`); returns the parsed state, throws the engine's message on failure. */
  action(request) {
    const code = this.withText(JSON.stringify(request), (ptr, length) => this.x.ngc_session_action(ptr, length));
    if (code === 2) throw new EngineError('There is no session');
    // The result (state JSON, or the message on failure) is left in the output buffer.
    const text = this.text(this.x.ngc_output_len());
    if (code !== 0) throw new EngineError(text || this.error());
    return JSON.parse(text);
  }

  stateText() {
    return this.text(this.x.ngc_session_state());
  }

  state() {
    const text = this.stateText();
    return text ? JSON.parse(text) : null;
  }

  /**
   * Brings the LCD up to date and describes the frame. `ptr` points into linear memory: use `copyFrame` before
   * calling anything else on the engine.
   */
  frame() {
    const version = this.x.ngc_session_frame();
    return {
      version,
      width: this.x.ngc_frame_width(),
      height: this.x.ngc_frame_height(),
      ptr: this.x.ngc_frame_ptr(),
      length: this.x.ngc_frame_len(),
    };
  }

  /** Copies the RGBA bytes described by `frame()` into `target` (a Uint8Array/Uint8ClampedArray of at least `length` bytes). */
  copyFrame(info, target) {
    target.set(this.u8().subarray(info.ptr, info.ptr + info.length));
  }

  // ---- profile, capture, shutdown (parts lists) ------------------------------------------------

  readParts(count) {
    const files = [];
    for (let index = 0; index < count; index++) {
      const name = this.text(this.x.ngc_part_name(index));
      const ptr = this.x.ngc_part_ptr(index);
      const length = this.x.ngc_part_len(index);
      files.push({ name, data: this.u8().slice(ptr, ptr + length) });
    }
    this.x.ngc_parts_clear();
    return files;
  }

  /** Storage files that changed since the last call (empty when nothing is dirty). */
  profileChanges() {
    return this.readParts(this.x.ngc_session_profile_changes());
  }

  /** The complete profile including a fresh RTC checkpoint. */
  exportProfile() {
    return this.readParts(this.x.ngc_session_profile_export());
  }

  /** Evidence capture: `{name, files: [{name, data}]}` (state.json, lcd.png, can-trace.tsv). */
  capture() {
    const count = this.x.ngc_session_capture();
    const name = this.text(this.x.ngc_capture_name());
    return { name, files: this.readParts(count) };
  }

  /** Closes the session with the runner's shutdown semantics; returns the profile files. */
  shutdown() {
    return this.readParts(this.x.ngc_session_shutdown());
  }
}
