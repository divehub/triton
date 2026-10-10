// Sound for the dive game: every sound but the MAV is synthesized with the Web Audio API.
//
// No external asset (the MAV's four WAV files sit beside this module and are fetched from the same origin), no dependency, no
// `data:` or `blob:` URL, no inline anything: plain Web Audio nodes only, so it runs under the site's Content-Security-Policy
// (`default-src 'none'; script-src 'self'; connect-src 'self'`) without an exception. The module touches no DOM and knows nothing
// about the game: the game tells it what is going on (`setUnderwater`, `setVentRate`, `click`, ...).
//
// The sounds (the user's picks from the reviewed candidates; every one is soft-edged and quiet enough to ignore):
//   ambience   on the boat or at the surface, waves lapping and a soft hull knock; in the water a low rumble, a faint hiss, a far
//              creak now and then and a very soft closed-loop breathing cycle (a closed circuit makes no bubbles)
//   vent       deep, muffled "blub blub" of big bubbles (100 to 450 Hz, their own low-pass); the density follows the vent rate
//   MAV        recorded regulator clips (the only sound that is not synthesized; four short files, see licenses/README.md): the
//              opening of a breath and a seamless loop of its steadiest part, for as long as the valve is held; oxygen a natural
//              exhale, diluent an inhale played lower. Letting go simply fades out over 50 ms: nothing else plays
//   swim       a steady, soft flow of water that brightens and swells with the speed; silence when holding depth
//   click      a handset button: a crisp tick (Confirm is a double click)
//   vibrator   a phone's buzz rebuilt from a reference recording (its measured harmonics, a 150 Hz motor that spins up from 135 Hz,
//              its attack and spin-down; no file), at least 120 ms; never muffled: a buzz on the arm is heard through the bone
//   torch      a small switch click; on adds a faint hum
//   surface    reaching the surface from below: a soft wash, two soft drops, a faint airy lift
//   power      power down (three falling bell notes) and power up (a rising arpeggio)
// There is no splash sound: entering the water only closes the bus low-pass (everything below the surface is muffled).
//
// The graph (built on `unlock()`, which must run inside a user gesture):
//
//   one-shots ----------------------------------------\
//   ambience / vent / swim / MAV -> continuous bus ----+-> bus -> underwater low-pass (2 stages) -> master -> limiter -> out
//   vibrator -------------------------------------------------------------------------------------/
//
// Rules every voice follows:
//   * bounded: noise comes from two shared buffers, a voice is a handful of nodes, and every kind has a concurrency cap
//     (a request over the cap is dropped, never queued);
//   * released: a voice is a `Voice` group; when its last source has ended the whole group is disconnected and forgotten
//     (`stats().liveNodes` returns to its baseline);
//   * soft-edged: no hard starts or stops, every envelope has an attack and a release;
//   * safe: no call throws (a failure is counted in `stats().errors`), and without Web Audio the module is silent.

const LEAD_S = 0.005; // a one-shot starts this far ahead of the audio clock
const SILENT = 0.0001; // the floor of an exponential ramp (-80 dB)
const PUMP_MS = 50; // the scheduler's period
const LOOKAHEAD_S = 0.2; // how far ahead the scheduler plans stochastic events
const MAX_VOICES = 96; // all voices together

const DEFAULT_VOLUME = 0.8;
const MASTER_TRIM = 1; // volume 1 is a unity master: the loudest one-shot peaks near -6 dBFS before the master
const SURFACE_CUTOFF_HZ = 18000; // the bus low-pass in the air (transparent)
const UNDERWATER_CUTOFF_HZ = 2300; // ... and in the water (two stages, 24 dB per octave in all)
const VENT_CUTOFF_HZ = 750; // the vent bubbles' own low-pass, before the bus
// The vibrator, measured on a reference recording of a phone's buzz: a steady 150 Hz motor that starts near 135 Hz and settles within
// about 40 ms, a 15 ms attack (-20 to -3 dB) and a spin-down of about 0.57 dB per ms. One period's spectrum (cosine and sine terms
// per harmonic, the fundamental at 1): 2nd -15.8 dB, 3rd -17.4 dB, 5th -26.4 dB, the rest below -42 dB. These 24 harmonics hold all
// of the recording's energy (it has no noise), so the oscillator below is the recording without its file.
const BUZZ_HZ = 150;
const BUZZ_SPIN_FROM = 0.9; // the spin-up starts at 0.9 times the motor's speed
const BUZZ_SPIN_TAU_S = 0.015;
const BUZZ_ATTACK_S = 0.015;
const BUZZ_RELEASE_S = 0.14; // -80 dB at the recording's 0.57 dB per ms
const BUZZ_REAL = Object.freeze([0, -0.77833, 0.09563, -0.13384, -0.00607, 0.04311, 0.00207, -0.00158, -0.00119, -0.00248, -0.00141,
  0.00122, -0.0004, -0.00123, -0.00106, 0.00405, 0.00131, 0.00169, 0.00269, 0.00134, 0.00069, 0.00034, 0.00002, 0.00023, -0.00079]);
const BUZZ_IMAG = Object.freeze([0, 0.62786, 0.13126, 0.01597, -0.00489, -0.0204, 0.00042, 0.00009, -0.00117, 0.00481, 0.00144,
  -0.00053, 0.0001, -0.00045, -0.00475, 0.00027, 0.00051, -0.00053, -0.00042, 0.0008, 0.00016, -0.00052, -0.00246, -0.00271, -0.00054]);
const LIMITER = Object.freeze({ threshold: -4, knee: 3, ratio: 12, attack: 0.002, release: 0.12 });
// The DynamicsCompressorNode applies an automatic makeup gain that depends on its curve (it is part of the Web Audio specification);
// the post-limiter trim cancels it so that a signal below the threshold passes at unity. Measured in Chromium with these
// parameters: +1.65 dB below the threshold, and a 0 dBFS to +6 dBFS sine comes out at -2.7 to -2.1 dBFS after the trim.
const LIMITER_MAKEUP_DB = 1.65;

// Gain staging, in dB, measured before the master (volume 1, no limiter) with an offline loudness harness (48 kHz, peak and a
// K-weighted level): every one-shot peaks at about -6 to -13 dBFS, the continuous layers sit far below them (the ambience at least
// 18 dB under a click), and the vent bubbles and the MAV stay quiet next to the clicks.
const TRIM_DB = Object.freeze({
  click: -4.4, vibrate: -20, surface: -7.3, // the vibrator: the K-weighted level of the synthesized buzz it replaced
  torchOn: -1.7, torchHum: -21, torchOff: 0,
  powerDown: -3.5, powerUp: -4.5,
  ambienceSurface: -28, ambienceWater: -26, breath: -4,
  vent: -20, swim: -17.3,
});
const SWIM_SPAN_DB = 3.9; // the explicit rise in loudness from the slowest swim to the fastest

const dbToGain = (db) => 10 ** (db / 20);
const clamp = (value, low, high) => Math.min(high, Math.max(low, value));

/** Holds an automated parameter at its present value from `t` on (no jump when the next ramp starts). */
function hold(param, t) {
  if (typeof param.cancelAndHoldAtTime === 'function') {
    param.cancelAndHoldAtTime(t);
  } else {
    const value = param.value;
    param.cancelScheduledValues(t);
    param.setValueAtTime(value, t);
  }
}

/** A gain shape: silence, a linear attack to `peak`, `hold` seconds there, then an exponential fall over `release` seconds. */
function pulse(param, t, peak, attack, release, holdFor = 0) {
  param.setValueAtTime(SILENT, t);
  param.linearRampToValueAtTime(peak, t + attack);
  if (holdFor > 0) param.setValueAtTime(peak, t + attack + holdFor);
  param.exponentialRampToValueAtTime(SILENT, t + attack + holdFor + release);
}

/** Moves a parameter smoothly to `target` (time constant `tau` seconds) from `t` on. */
function glide(param, t, target, tau) {
  hold(param, t);
  param.setTargetAtTime(target, t, tau);
}

/**
 * The MAV is a recording of a regulator: four short clips (44.1 kHz mono, peaks at -6 dBFS) that sit beside this module, an opening
 * (0.18 s) and a seamless loop (0.70 s; its last 80 ms are cross-faded into its start) of a breath's steadiest part for each gas.
 * Oxygen plays an exhale at its natural pitch, diluent an inhale at 0.87 times its speed (a little lower). `level` is the staging
 * (dB) that brings each to about -31 dBFS RMS before the master, the level the synthesized MAV had.
 */
const MAV_CLIPS = Object.freeze({
  oxygen: { onset: 'mav-oxygen-onset.wav', loop: 'mav-oxygen-loop.wav', rate: 1, level: -12.6 },
  diluent: { onset: 'mav-diluent-onset.wav', loop: 'mav-diluent-loop.wav', rate: 0.87, level: -3.4 },
});
const MAV_LOOP_LEAD_S = 0.03; // the loop starts this long before the opening ends and fades in over the same time
const MAV_STOP_S = 0.05; // letting go, a pause or a switch of gas: the clips fade out linearly over this long and stop
/** The calls the game makes (and the scheduler's `advance`): each runs inside a try/catch (see the constructor). */
const GUARDED = Object.freeze([
  'setEnabled', 'setVolume', 'setUnderwater', 'setPaused', 'setAmbience', 'setMav', 'setVentRate', 'setSwimRate',
  'surfaceBreak', 'click', 'vibrate', 'torch', 'powerDown', 'powerUp', 'advance', 'silence',
]);
const BLUB_LOW_HZ = 100; // the vent bubbles' pitch range
const BLUB_HIGH_HZ = 450;
const SNAP_OFFSETS = Object.freeze([0.37, 1.12]); // where in the noise buffer the first and the second click's snap is taken from
const CLICK_PITCH = Object.freeze({ up: [1250], down: [980], confirm: [1050, 1500] });

// ---- noise ------------------------------------------------------------------------------------------------------------------

const NOISE_SECONDS = 4;
const NOISE_SEAM_S = 0.06;
const NOISE_RMS = 0.25;

/** Two looping noise buffers (white and pink), made once per context. The loop seam is cross-faded, so it never clicks. */
function makeNoise(ctx, random) {
  const rate = ctx.sampleRate;
  const length = Math.round(NOISE_SECONDS * rate);
  const seam = Math.round(NOISE_SEAM_S * rate);
  const make = (pink) => {
    const raw = new Float32Array(length + seam);
    let b0 = 0;
    let b1 = 0;
    let b2 = 0;
    let b3 = 0;
    let b4 = 0;
    let b5 = 0;
    let b6 = 0;
    for (let i = 0; i < raw.length; i++) {
      const white = random() * 2 - 1;
      if (!pink) {
        raw[i] = white;
        continue;
      }
      // Paul Kellet's economy pink filter.
      b0 = 0.99886 * b0 + white * 0.0555179;
      b1 = 0.99332 * b1 + white * 0.0750759;
      b2 = 0.969 * b2 + white * 0.153852;
      b3 = 0.8665 * b3 + white * 0.3104856;
      b4 = 0.55 * b4 + white * 0.5329522;
      b5 = -0.7616 * b5 - white * 0.016898;
      raw[i] = b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362;
      b6 = white * 0.115926;
    }
    const data = new Float32Array(length);
    for (let i = 0; i < length; i++) data[i] = raw[i];
    for (let i = 0; i < seam; i++) {
      const angle = (i / seam) * Math.PI / 2; // equal-power cross-fade from the tail into the head
      data[i] = raw[i] * Math.sin(angle) + raw[length + i] * Math.cos(angle);
    }
    let sum = 0;
    for (let i = 0; i < length; i++) sum += data[i] * data[i];
    const scale = NOISE_RMS / Math.sqrt(sum / length || 1);
    for (let i = 0; i < length; i++) data[i] = clamp(data[i] * scale, -1, 1);
    const buffer = ctx.createBuffer(1, length, rate);
    buffer.copyToChannel(data, 0);
    return buffer;
  };
  return { white: make(false), pink: make(true) };
}

// ---- voices -----------------------------------------------------------------------------------------------------------------

/**
 * A group of nodes that live and die together. Sources are started with `start`; when the last one has ended (or `kill` is
 * called) every node is disconnected and the group is forgotten.
 */
class Voice {
  constructor(engine, kind, t0) {
    this.engine = engine;
    this.kind = kind;
    this.t0 = t0;
    this.nodes = [];
    this.sources = [];
    this.pending = 0;
    this.done = false;
    engine.voices.add(this);
    engine.counts.set(kind, (engine.counts.get(kind) || 0) + 1);
  }

  track(node) {
    this.nodes.push(node);
    this.engine.live.add(node);
    this.engine.created += 1;
    return node;
  }

  gain(value = 1) {
    const node = this.track(this.engine.ctx.createGain());
    node.gain.value = value;
    return node;
  }

  filter(type, frequency, q = 0.707) {
    const node = this.track(this.engine.ctx.createBiquadFilter());
    node.type = type;
    node.frequency.value = frequency;
    node.Q.value = q;
    return node;
  }

  osc(type, frequency, at = this.t0) {
    const node = this.track(this.engine.ctx.createOscillator());
    node.type = type;
    node.frequency.value = frequency;
    this.sources.push({ node, at });
    return node;
  }

  /** A decoded clip from `at`, once or looping, at `rate` times its natural speed (and pitch). */
  clip(buffer, at, { loop = false, rate = 1 } = {}) {
    const node = this.track(this.engine.ctx.createBufferSource());
    node.buffer = buffer;
    node.loop = loop;
    node.playbackRate.value = rate;
    this.sources.push({ node, at });
    return node;
  }

  /** A looping noise source at a random place in the shared buffer (or at the fixed `offset` seconds, for a repeatable transient). */
  noise(kind = 'white', at = this.t0, offset = null) {
    const node = this.track(this.engine.ctx.createBufferSource());
    node.buffer = this.engine.noise[kind];
    node.loop = true;
    this.sources.push({ node, at, offset: offset === null ? this.engine.random() * node.buffer.duration : offset });
    return node;
  }

  /** Connects the nodes in a row; returns the last. */
  chain(...nodes) {
    for (let i = 0; i + 1 < nodes.length; i++) nodes[i].connect(nodes[i + 1]);
    return nodes[nodes.length - 1];
  }

  /** An LFO: a slow sine of `depth` added to `param`. */
  lfo(hz, depth, param, at = this.t0) {
    const osc = this.osc('sine', hz, at);
    const amount = this.gain(depth);
    osc.connect(amount);
    amount.connect(param);
    return osc;
  }

  /** Starts every source; with `end` they all stop then, without it they run until `stop`. */
  start(end = null) {
    this.pending = this.sources.length;
    if (this.pending === 0) {
      this.release();
      return;
    }
    for (const { node, at, offset } of this.sources) {
      node.onended = () => {
        this.pending -= 1;
        if (this.pending <= 0) this.release();
      };
      if (offset === undefined) node.start(at);
      else node.start(at, offset);
      if (end !== null) node.stop(Math.max(end, at + 0.001));
    }
  }

  stop(t) {
    for (const { node } of this.sources) {
      try {
        node.stop(t);
      } catch { /* not started, or already ended */ }
    }
  }

  release() {
    if (this.done) return;
    this.done = true;
    for (const node of this.nodes) {
      node.onended = null;
      try {
        node.disconnect();
      } catch { /* already disconnected */ }
      this.engine.live.delete(node);
    }
    this.engine.released += this.nodes.length;
    this.nodes = [];
    this.sources = [];
    this.engine.voices.delete(this);
    this.engine.counts.set(this.kind, Math.max(0, (this.engine.counts.get(this.kind) || 1) - 1));
  }

  /** A hard stop (dispose): the sources end now and the group is released at once. */
  kill() {
    this.stop(0);
    this.release();
  }
}

/** Fetches one of the MAV clips from beside this module (the same origin: `connect-src 'self'`). */
function fetchClip(file) {
  return fetch(new URL(file, import.meta.url)).then((response) => {
    if (!response.ok) throw new Error(`${file}: HTTP ${response.status}`);
    return response.arrayBuffer();
  });
}

// ---- the engine -------------------------------------------------------------------------------------------------------------

export class GameSound {
  /**
   * @param {{context?: BaseAudioContext, random?: () => number, masterTrim?: number, limiter?: boolean, autoPump?: boolean,
   *   suspendWhenHidden?: boolean, clipLoader?: (file: string) => Promise<ArrayBuffer>}} [options] `context` is for tests and offline
   *   renders (the module then neither resumes nor closes it, and does not start its own scheduler timer unless `autoPump` is set;
   *   call `advance(until)` and `loadClips()` instead); `random` makes the stochastic parts reproducible; `masterTrim` and
   *   `limiter: false` are for loudness measurements; `clipLoader` replaces the fetch of the MAV clips (tests).
   */
  constructor(options = {}) {
    this.random = options.random || Math.random;
    this.clipLoader = options.clipLoader || fetchClip;
    this.clips = null; // the decoded MAV clips, { oxygen: {onset, loop}, diluent: {onset, loop} }, once loaded
    this.clipsPromise = null;
    this.externalContext = options.context || null;
    this.masterTrim = options.masterTrim === undefined ? MASTER_TRIM : options.masterTrim;
    this.useLimiter = options.limiter !== false;
    this.autoPump = options.autoPump === undefined ? !this.externalContext : !!options.autoPump;
    this.suspendWhenHidden = options.suspendWhenHidden !== false && !this.externalContext;
    this.ctx = null;
    this.graph = null;
    this.noise = null;
    this.supported = true;
    this.disposed = false;
    // What the game last told us (applied as soon as the context is running).
    this.enabled = true;
    this.volume = DEFAULT_VOLUME;
    this.underwater = false;
    this.paused = false;
    this.ambienceOn = false;
    this.mavGas = null;
    this.ventTarget = 0;
    this.swimTarget = 0;
    // Bookkeeping: every voice, every node alive, how many voices of each kind.
    this.voices = new Set();
    this.live = new Set();
    this.counts = new Map();
    this.created = 0;
    this.released = 0;
    this.dropped = 0;
    // Continuous layers.
    this.amb = null; // { key, voice, out, pump }
    this.mav = null; // { gas, voice, level }
    this.swim = null; // { voice, env, filter, gust }
    this.ventSmooth = 0;
    this.swimSmooth = 0;
    this.next = { knock: 0, creak: 0, breath: 0, bloop: 0, gust: 0 };
    this.pumped = null;
    this.timer = null;
    this.buzz = null;
    this.suspendTimer = null;
    this.visibility = null;
    this.errors = 0;
    this.lastError = null;
    // Sound is never worth breaking the game for: a failing call is counted (`stats().errors`, `lastError`) and otherwise ignored.
    for (const name of GUARDED) {
      const method = this[name];
      this[name] = (...args) => {
        try {
          return method.apply(this, args);
        } catch (error) {
          this.errors += 1;
          this.lastError = error;
          return undefined;
        }
      };
    }
    if (this.externalContext) {
      try {
        this.ensureGraph();
      } catch (error) {
        this.errors += 1;
        this.lastError = error;
        this.supported = false;
      }
    }
    if (this.suspendWhenHidden && typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
      this.visibility = () => this.visibilityChanged();
      document.addEventListener('visibilitychange', this.visibility);
    }
  }

  // ---- context and graph -------------------------------------------------------------------------------------------------

  /** Creates the context and the permanent graph (once). Returns null where Web Audio is missing. */
  ensureGraph() {
    if (this.graph || this.disposed || !this.supported) return this.graph;
    let ctx = this.externalContext;
    if (!ctx) {
      const Context = globalThis.AudioContext || globalThis.webkitAudioContext;
      try {
        ctx = Context ? new Context({ latencyHint: 'interactive' }) : null;
      } catch {
        ctx = null;
      }
      if (!ctx) {
        this.supported = false;
        return null;
      }
    }
    this.ctx = ctx;
    this.noise = makeNoise(ctx, this.random);
    const g = {};
    g.bus = ctx.createGain();
    g.continuous = ctx.createGain();
    g.ambience = ctx.createGain();
    g.vent = ctx.createGain();
    g.swim = ctx.createGain();
    g.mav = ctx.createGain();
    for (const name of ['ambience', 'swim', 'mav']) g[name].connect(g.continuous);
    // The vent bubbles have a low-pass of their own: heard through the water, they are deep and soft.
    g.ventLow = ctx.createBiquadFilter();
    g.ventLow.type = 'lowpass';
    g.ventLow.frequency.value = VENT_CUTOFF_HZ;
    g.ventLow.Q.value = -3.0103;
    g.vent.connect(g.ventLow);
    g.ventLow.connect(g.continuous);
    g.continuous.connect(g.bus);
    // Two shared periodic waves (plain Web Audio, built from numbers): a round bubble, and the measured phone motor.
    g.blubWave = ctx.createPeriodicWave(new Float32Array([0, 0, 0]), new Float32Array([0, 1, 0.3]));
    g.buzzWave = ctx.createPeriodicWave(new Float32Array(BUZZ_REAL), new Float32Array(BUZZ_IMAG));
    g.low1 = ctx.createBiquadFilter();
    g.low2 = ctx.createBiquadFilter();
    for (const filter of [g.low1, g.low2]) {
      filter.type = 'lowpass';
      filter.Q.value = -3.0103; // a low-pass's Q is in dB: this is the flat Butterworth shape (no bump before the cutoff)
    }
    g.master = ctx.createGain();
    g.bus.connect(g.low1);
    g.low1.connect(g.low2);
    g.low2.connect(g.master);
    if (this.useLimiter) {
      g.limiter = ctx.createDynamicsCompressor();
      g.limiter.threshold.value = LIMITER.threshold;
      g.limiter.knee.value = LIMITER.knee;
      g.limiter.ratio.value = LIMITER.ratio;
      g.limiter.attack.value = LIMITER.attack;
      g.limiter.release.value = LIMITER.release;
      g.trim = ctx.createGain();
      g.trim.gain.value = dbToGain(-LIMITER_MAKEUP_DB);
      g.master.connect(g.limiter);
      g.limiter.connect(g.trim);
      g.trim.connect(ctx.destination);
    } else {
      g.master.connect(ctx.destination);
    }
    this.graph = g;
    this.applyMaster(true);
    this.applyBus(0, true);
    ctx.onstatechange = () => this.syncAll();
    return g;
  }

  /**
   * Call this from a user gesture (a click or a key press): it creates the AudioContext, resumes it and starts loading the MAV
   * clips (once). Resolves true once the sound is running, false where it cannot (no Web Audio, sound off). Safe to call again.
   */
  unlock() {
    if (this.disposed) return Promise.resolve(false);
    let graph = null;
    try {
      graph = this.ensureGraph();
    } catch (error) {
      this.errors += 1;
      this.lastError = error;
      this.supported = false;
    }
    if (!graph) return Promise.resolve(false);
    this.loadClips(); // not awaited: until they are in hand the MAV is silent, and a failure only counts in `stats().errors`
    if (this.externalContext) return Promise.resolve(true);
    if (!this.enabled) return Promise.resolve(false);
    let resumed;
    try {
      resumed = this.ctx.state === 'running' ? Promise.resolve() : this.ctx.resume();
    } catch {
      return Promise.resolve(false);
    }
    return Promise.resolve(resumed).then(() => {
      this.syncAll();
      return this.ctx.state === 'running';
    }, () => false);
  }

  /**
   * Fetches and decodes the four MAV clips (once; `unlock` starts it, a test or an offline render calls it itself). Resolves true
   * when they are in hand. A failure (a missing file, a decoder that refuses it) is counted in `stats().errors` and leaves the MAV
   * silent; the next call tries again. A valve held meanwhile starts as soon as the clips arrive.
   */
  loadClips() {
    if (this.clips) return Promise.resolve(true);
    if (this.clipsPromise) return this.clipsPromise;
    if (this.disposed || !this.graph) return Promise.resolve(false);
    const ctx = this.ctx;
    const load = async (file) => ctx.decodeAudioData(await this.clipLoader(file));
    this.clipsPromise = (async () => {
      try {
        const clips = {};
        await Promise.all(Object.entries(MAV_CLIPS).map(async ([gas, spec]) => {
          const [onset, loop] = await Promise.all([load(spec.onset), load(spec.loop)]);
          clips[gas] = { onset, loop };
        }));
        if (this.disposed) return false;
        this.clips = clips;
        this.syncMav();
        return true;
      } catch (error) {
        this.errors += 1;
        this.lastError = error;
        return false;
      } finally {
        this.clipsPromise = null;
      }
    })();
    return this.clipsPromise;
  }

  /** True once sounds can play: the graph exists, the sound is on and the context is running. */
  ready() {
    if (this.disposed || !this.enabled || !this.graph) return false;
    return this.externalContext ? true : this.ctx.state === 'running';
  }

  /** The audio clock time at which a one-shot should start, or null when nothing can play. */
  when() {
    return this.ready() ? this.ctx.currentTime + LEAD_S : null;
  }

  audible() {
    return this.ready() && !this.paused;
  }

  visibilityChanged() {
    if (this.disposed || !this.ctx || this.externalContext) return;
    if (document.hidden) {
      if (this.ctx.state === 'running') this.ctx.suspend().catch(() => {});
    } else if (this.enabled && this.ctx.state === 'suspended') {
      this.ctx.resume().catch(() => {});
    }
  }

  // ---- settings ------------------------------------------------------------------------------------------------------------

  masterGain() {
    return this.enabled ? this.masterTrim * clamp(this.volume, 0, 1) ** 2 : 0;
  }

  applyMaster(immediate = false) {
    if (!this.graph) return;
    const param = this.graph.master.gain;
    if (immediate) {
      param.setValueAtTime(this.masterGain(), this.ctx.currentTime);
    } else {
      glide(param, this.ctx.currentTime, this.masterGain(), 0.015);
    }
  }

  /** The master volume, 0 to 1 (a squared taper, so the slider feels even). */
  setVolume(volume) {
    this.volume = Number.isFinite(volume) ? clamp(volume, 0, 1) : DEFAULT_VOLUME;
    this.applyMaster();
  }

  /** Off silences everything at once and suspends the context (no CPU); on resumes it. */
  setEnabled(on) {
    const enabled = !!on;
    if (enabled === this.enabled) return;
    this.enabled = enabled;
    this.applyMaster();
    if (this.suspendTimer !== null) {
      clearTimeout(this.suspendTimer);
      this.suspendTimer = null;
    }
    if (!enabled) {
      this.endLayers(0.03);
      this.updatePump();
      if (this.ctx && !this.externalContext) {
        this.suspendTimer = setTimeout(() => {
          this.suspendTimer = null;
          if (!this.enabled && this.ctx.state === 'running') this.ctx.suspend().catch(() => {});
        }, 150);
      }
    } else if (this.ctx && !this.externalContext && this.ctx.state === 'suspended') {
      this.ctx.resume().then(() => this.syncAll(), () => {});
    } else {
      this.syncAll();
    }
  }

  /**
   * Under the water everything is muffled (the bus low-pass closes) and the ambience changes from the boat's to the water's.
   * `delay` (seconds) puts the change after the present moment; `immediate` jumps; `ramp` sets the length of the sweep (0.3 s in,
   * 0.2 s out by default).
   */
  setUnderwater(on, { delay = 0, immediate = false, ramp = null } = {}) {
    const underwater = !!on;
    if (underwater === this.underwater) return;
    this.underwater = underwater;
    this.applyBus(delay, immediate, ramp);
    this.syncAmbience(delay);
  }

  /** The bus low-pass moves to its water or air cutoff: `ramp` seconds (0.3 in, 0.2 out by default) starting `delay` from now. */
  applyBus(delay, immediate = false, ramp = null) {
    if (!this.graph) return;
    const ctx = this.ctx;
    const target = this.underwater ? UNDERWATER_CUTOFF_HZ : Math.min(SURFACE_CUTOFF_HZ, ctx.sampleRate * 0.4);
    const t = ctx.currentTime + delay;
    for (const filter of [this.graph.low1, this.graph.low2]) {
      const param = filter.frequency;
      hold(param, ctx.currentTime);
      if (immediate) {
        param.setValueAtTime(target, t);
        continue;
      }
      const from = param.value;
      param.setValueAtTime(from, t);
      param.exponentialRampToValueAtTime(target, t + (ramp === null ? (this.underwater ? 0.3 : 0.2) : ramp));
    }
  }

  /**
   * Pausing silences the continuous sounds at once (the one-shots, the handset's click for one, still play); resuming brings them
   * back. The game's valves are released by its own pause, so a MAV does not restart unless the game holds the valve again.
   */
  setPaused(on) {
    const paused = !!on;
    if (paused === this.paused) return;
    this.paused = paused;
    if (this.graph) glide(this.graph.continuous.gain, this.ctx.currentTime, paused ? 0 : 1, paused ? 0.012 : 0.05);
    this.syncAll();
  }

  // ---- bookkeeping -----------------------------------------------------------------------------------------------------------

  /** A new voice of `kind`, or null when that kind (or the whole engine) is at its cap. */
  voice(kind, cap, t0) {
    if ((this.counts.get(kind) || 0) >= cap || this.voices.size >= MAX_VOICES) {
      this.dropped += 1;
      return null;
    }
    return new Voice(this, kind, t0);
  }

  rand(low, high) {
    return low + (high - low) * this.random();
  }

  /** The nodes alive and the voices counted; for the leak checks and the tests. */
  stats() {
    return {
      liveNodes: this.live.size,
      liveVoices: this.voices.size,
      created: this.created,
      released: this.released,
      dropped: this.dropped,
      errors: this.errors,
      kinds: Object.fromEntries([...this.counts].filter(([, count]) => count > 0)),
      contextState: this.ctx ? this.ctx.state : 'none',
    };
  }

  /** What the game last told the sound (not what plays: see `stats()` for that). */
  snapshot() {
    return {
      enabled: this.enabled, volume: this.volume, underwater: this.underwater, paused: this.paused, ambience: this.ambienceOn,
      mav: this.mavGas, ventRate: this.ventTarget, swimRate: this.swimTarget,
    };
  }

  // ---- the scheduler -------------------------------------------------------------------------------------------------------

  needsPump() {
    return this.amb !== null || this.ventTarget > 0 || this.ventSmooth > 0 || this.swimTarget > 0 || this.swimSmooth > 0;
  }

  updatePump() {
    if (!this.autoPump) return;
    const need = this.audible() && this.needsPump();
    if (need && this.timer === null) {
      this.pumped = this.ctx.currentTime;
      this.timer = setInterval(() => {
        if (this.audible()) this.advance(this.ctx.currentTime + LOOKAHEAD_S);
      }, PUMP_MS);
    } else if (!need && this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /**
   * Plans the chance events of the continuous sounds (creaks, knocks, breaths, bubbles) up to the audio-clock time `until` and
   * follows the vent and swim rates. An internal timer calls it every 50 ms with 0.2 s of look-ahead; a test or an offline render
   * calls it itself (once, with the render's length, is enough).
   */
  advance(until) {
    if (!this.graph) return;
    const now = this.ctx.currentTime;
    const dt = Math.max(0, until - (this.pumped === null ? now : this.pumped));
    this.pumped = until;
    this.follow(dt);
    this.syncSwim();
    if (this.amb && this.amb.pump) this.amb.pump(until, now);
    this.pumpVent(until, now);
    this.pumpSwim(until, now);
  }

  /** The vent and swim rates glide to their targets (a new sound starts at its target at once). */
  follow(dt) {
    const step = (current, target, tau, floor) => {
      if (target > 0 && current < floor) return target;
      const next = current + (target - current) * (1 - Math.exp(-dt / tau));
      return target === 0 && next < floor ? 0 : next;
    };
    this.ventSmooth = step(this.ventSmooth, this.ventTarget, 0.12, 0.02);
    this.swimSmooth = step(this.swimSmooth, this.swimTarget, 0.18, 0.2);
  }

  /** Re-evaluates every continuous sound against what the game last said. */
  syncAll() {
    this.syncAmbience();
    this.syncMav();
    this.syncSwim();
    this.updatePump();
  }

  /** Fades every continuous layer out (the sound was turned off). */
  endLayers(seconds) {
    this.endAmbience(seconds);
    this.endMav();
    this.endSwim(seconds);
  }

  // ---- continuous: ambience ------------------------------------------------------------------------------------------------

  setAmbience(on) {
    this.ambienceOn = !!on;
    this.syncAmbience();
    this.updatePump();
  }

  syncAmbience(delay = 0) {
    const key = this.audible() && this.ambienceOn ? (this.underwater ? 'water' : 'surface') : null;
    if ((this.amb ? this.amb.key : null) === key) return;
    const fresh = key !== null;
    this.endAmbience(fresh ? 0.45 : 0.12, delay);
    if (fresh) this.amb = this.buildAmbience(key, delay);
    this.updatePump();
  }

  endAmbience(seconds, delay = 0) {
    const amb = this.amb;
    this.amb = null;
    if (!amb) return;
    const t = this.ctx.currentTime + delay;
    glide(amb.out.gain, t, 0, seconds / 4);
    amb.voice.stop(t + seconds + 0.05);
  }

  buildAmbience(key, delay) {
    const t = this.ctx.currentTime + delay;
    const v = this.voice(`ambience.${key}`, 3, t);
    if (!v) return null;
    const out = v.gain(0);
    out.connect(this.graph.ambience);
    out.gain.setValueAtTime(0, t);
    out.gain.setTargetAtTime(dbToGain(key === 'surface' ? TRIM_DB.ambienceSurface : TRIM_DB.ambienceWater), t, 0.18);
    const amb = { key, voice: v, out, pump: null };
    if (key === 'surface') this.buildSurface(v, out, amb, t);
    else this.buildWater(v, out, amb, t);
    v.start(null);
    return amb;
  }

  /** The boat: waves lapping (two slow swells of filtered noise) and now and then a soft knock of the hull. */
  buildSurface(v, out, amb, t) {
    const wave = v.noise('pink');
    const waveLow = v.filter('lowpass', 850, 0.6);
    const waveSwell = v.gain(0.5);
    v.chain(wave, waveLow, waveSwell, out);
    v.lfo(0.11, 0.38, waveSwell.gain);
    v.lfo(0.19, 0.12, waveSwell.gain);
    const lap = v.noise('pink');
    const lapBand = v.filter('bandpass', 1300, 0.8);
    const lapSwell = v.gain(0.18);
    v.chain(lap, lapBand, lapSwell, out);
    v.lfo(0.31, 0.17, lapSwell.gain);
    v.lfo(0.083, 0.05, lapSwell.gain);
    this.next.knock = t + this.rand(2.5, 6);
    amb.pump = (until, now) => {
      while (this.next.knock < until) {
        const at = Math.max(this.next.knock, now + LEAD_S);
        this.knock(at, out, 0.42);
        if (this.random() < 0.3) this.knock(at + this.rand(0.11, 0.17), out, 0.24);
        this.next.knock += this.rand(6, 13);
      }
    };
  }

  /** The hull's knock: a short, dull wooden tock. */
  knock(t, out, strength) {
    const v = this.voice('knock', 3, t);
    if (!v) return;
    const body = v.osc('sine', 150, t);
    body.frequency.setValueAtTime(150, t);
    body.frequency.exponentialRampToValueAtTime(92, t + 0.07);
    const bodyEnv = v.gain(0);
    pulse(bodyEnv.gain, t, 0.9 * strength, 0.004, 0.11);
    v.chain(body, bodyEnv, out);
    const tick = v.noise('white', t);
    const tickBand = v.filter('bandpass', 420, 2);
    const tickEnv = v.gain(0);
    pulse(tickEnv.gain, t, 0.6 * strength, 0.002, 0.02);
    v.chain(tick, tickBand, tickEnv, out);
    v.start(t + 0.2);
  }

  /** Under the water: a low rumble, a faint hiss, now and then a creak and a very soft closed-loop breathing cycle (no bubbles). */
  buildWater(v, out, amb, t) {
    const rumble = v.noise('pink');
    const rumbleHigh = v.filter('highpass', 70, 0.7); // nothing a small speaker could not play anyway
    const rumbleLow = v.filter('lowpass', 300, 0.7);
    const rumbleLevel = v.gain(1);
    v.chain(rumble, rumbleHigh, rumbleLow, rumbleLevel, out);
    v.lfo(0.05, 0.3, rumbleLevel.gain);
    const hiss = v.noise('pink');
    const hissBand = v.filter('bandpass', 700, 0.5);
    const hissLevel = v.gain(0.22);
    v.chain(hiss, hissBand, hissLevel, out);
    v.lfo(0.09, 0.08, hissLevel.gain);
    this.next.creak = t + this.rand(5, 12);
    const breath = v.noise('pink');
    const breathBand = v.filter('bandpass', 650, 0.9);
    const breathLevel = v.gain(SILENT);
    v.chain(breath, breathBand, breathLevel, out);
    this.next.breath = t + 0.4;
    amb.pump = (until, now) => {
      while (this.next.creak < until) {
        this.creak(Math.max(this.next.creak, now + LEAD_S), out);
        this.next.creak += this.rand(9, 20);
      }
      while (this.next.breath < until) {
        this.breathe(Math.max(this.next.breath, now + LEAD_S), breathBand, breathLevel);
        this.next.breath += this.rand(4.7, 5.4);
      }
    };
  }

  /** A faint groan of the hull far away: a gliding sawtooth through a narrow resonance. */
  creak(t, out) {
    const v = this.voice('creak', 1, t);
    if (!v) return;
    const length = this.rand(0.55, 0.9);
    const base = this.rand(150, 230);
    const saw = v.osc('sawtooth', base, t);
    saw.frequency.setValueAtTime(base, t);
    saw.frequency.exponentialRampToValueAtTime(base * this.rand(0.72, 0.85), t + length);
    v.lfo(this.rand(6, 12), base * 0.025, saw.frequency, t);
    const band = v.filter('bandpass', this.rand(420, 620), 5);
    const env = v.gain(0);
    pulse(env.gain, t, 0.5, length * 0.35, length * 0.65);
    v.chain(saw, band, env, out);
    v.start(t + length + 0.1);
  }

  /** One breath of the closed loop (about 5 s): an inhale, a short pause, a longer exhale. Very soft; no bubbles. */
  breathe(t, band, level) {
    const gain = level.gain;
    const freq = band.frequency;
    const scale = dbToGain(TRIM_DB.breath);
    gain.setValueAtTime(SILENT, t);
    gain.setTargetAtTime(0.9 * scale, t, 0.27); // inhale
    gain.setTargetAtTime(SILENT, t + 1.0, 0.08);
    gain.setTargetAtTime(0.7 * scale, t + 1.55, 0.13); // exhale
    gain.setTargetAtTime(SILENT, t + 1.85, 0.55);
    freq.setValueAtTime(520, t);
    freq.linearRampToValueAtTime(900, t + 1.0);
    freq.setValueAtTime(840, t + 1.55);
    freq.linearRampToValueAtTime(430, t + 3.3);
  }

  // ---- continuous: MAV ------------------------------------------------------------------------------------------------------

  /**
   * `'oxygen'` (an exhale at its natural pitch), `'diluent'` (an inhale a little lower) or null (released). Hold the valve and the
   * opening of the recorded breath plays, then its loop for as long as the valve is held. Letting go, a pause or a switch of gas
   * fades the clips out over 50 ms and stops them. Until the clips are loaded (or if they failed) the MAV is silent.
   */
  setMav(gas) {
    this.mavGas = gas === 'oxygen' || gas === 'diluent' ? gas : null;
    this.syncMav();
  }

  syncMav() {
    const gas = this.audible() ? this.mavGas : null;
    const mav = this.mav;
    if (mav && mav.gas === gas) return;
    if (mav) this.endMav();
    const clips = gas && this.clips ? this.clips[gas] : null;
    if (!clips) return;
    const spec = MAV_CLIPS[gas];
    const t = this.ctx.currentTime + LEAD_S;
    const v = this.voice('mav', 3, t); // the one sounding, and the fading ones of the last two presses (a quick press is never dropped)
    if (!v) return;
    const level = v.gain(dbToGain(spec.level));
    level.connect(this.graph.mav);
    // The opening plays once; the loop starts 30 ms before it ends and fades in over those 30 ms (the loop is seamless by itself).
    v.chain(v.clip(clips.onset, t, { rate: spec.rate }), level);
    const loopAt = t + clips.onset.duration / spec.rate - MAV_LOOP_LEAD_S;
    const fade = v.gain(0);
    fade.gain.setValueAtTime(0, loopAt);
    fade.gain.linearRampToValueAtTime(1, loopAt + MAV_LOOP_LEAD_S);
    v.chain(v.clip(clips.loop, loopAt, { loop: true, rate: spec.rate }), fade, level);
    v.start(null);
    this.mav = { gas, voice: v, level };
  }

  /** Letting go: just stop. Both clips fade out linearly over 50 ms and end; nothing is scheduled after that. */
  endMav() {
    const mav = this.mav;
    this.mav = null;
    if (!mav) return;
    const t = this.ctx.currentTime;
    const param = mav.level.gain;
    param.setValueAtTime(param.value, t); // the ramp needs its starting point: the level the clips are playing at
    param.linearRampToValueAtTime(0, t + MAV_STOP_S);
    mav.voice.stop(t + MAV_STOP_S + 0.01);
  }

  // ---- continuous: vent bubbles --------------------------------------------------------------------------------------------

  /**
   * How hard the loop vents, in surface liters per second of wall time (the same vented gas that draws the bubbles). Zero is
   * silence. The number of blubs per second follows it, as does the loudness, so it matches the bubbles drawn.
   */
  setVentRate(slPerSecond) {
    this.ventTarget = Number.isFinite(slPerSecond) && slPerSecond > 0 ? Math.min(slPerSecond, 20) : 0;
    if (this.ventTarget === 0) this.ventSmooth = 0; // the vent closed: the blubs stop now (the ones in flight finish)
    else if (this.audible() && this.ventSmooth === 0) this.ventSmooth = this.ventTarget;
    this.updatePump();
  }

  /** Blubs per second for a vent rate: a few at the faintest leak, a steady "blub blub" at a MAV's full flow (they are big bubbles). */
  ventDensity(rate) {
    return 1.2 * Math.min(1, rate / 0.05) + 11 * (1 - Math.exp(-rate / 0.7));
  }

  ventLevel(rate) {
    return 0.85 + 0.15 * Math.min(1, rate / 1.5);
  }

  pumpVent(until, now) {
    const rate = this.ventSmooth;
    if (!this.audible() || rate <= 0) {
      this.next.bloop = Math.max(this.next.bloop, until);
      return;
    }
    const level = this.ventLevel(rate);
    const density = this.ventDensity(rate);
    if (this.next.bloop < now) this.next.bloop = now + LEAD_S;
    while (this.next.bloop < until) {
      this.bloop(this.next.bloop, level);
      this.next.bloop += -Math.log(1 - this.random()) / density;
    }
  }

  /**
   * One blub: a big bubble, a round tone of 100 to 450 Hz (log-spread) whose pitch climbs about a third as it leaves; the bigger the
   * lower, longer and louder. It goes through the vent's own low-pass and then the bus, so it is heard as a deep "blub" in water.
   */
  bloop(t, level) {
    const v = this.voice('bloop', 16, t);
    if (!v) return;
    const f0 = BLUB_LOW_HZ * (BLUB_HIGH_HZ / BLUB_LOW_HZ) ** this.random();
    const bigness = 1 - (f0 - BLUB_LOW_HZ) / (BLUB_HIGH_HZ - BLUB_LOW_HZ);
    const length = 0.07 + bigness * 0.14;
    const osc = v.osc('sine', f0, t);
    osc.setPeriodicWave(this.graph.blubWave);
    osc.frequency.setValueAtTime(f0, t);
    osc.frequency.exponentialRampToValueAtTime(f0 * this.rand(1.25, 1.5), t + length * 0.55);
    const env = v.gain(0);
    pulse(env.gain, t, level * (0.5 + 0.5 * bigness) * dbToGain(TRIM_DB.vent), 0.008, length);
    v.chain(osc, env, this.graph.vent);
    v.start(t + 0.008 + length + 0.02);
  }

  // ---- continuous: swimming ------------------------------------------------------------------------------------------------

  /** The swim speed in meters per minute (the sign does not matter). Zero, holding depth, is silence. */
  setSwimRate(metersPerMinute) {
    this.swimTarget = Number.isFinite(metersPerMinute) ? Math.min(Math.abs(metersPerMinute), 40) : 0;
    if (this.swimTarget === 0) {
      this.swimSmooth = 0; // holding depth is silence: the flow fades out quickly
      this.endSwim(0.3);
    } else if (this.audible() && this.swimSmooth === 0) {
      this.swimSmooth = this.swimTarget;
    }
    this.syncSwim();
    this.updatePump();
  }

  /** 0 at rest to 1 at the fastest descent (30 m/min). */
  swimFraction() {
    return clamp(this.swimSmooth / 30, 0, 1);
  }

  /** The loudness at the present speed: about 4 dB from the slowest swim to the fastest (the brightness rises with it too). */
  swimLevel() {
    return dbToGain(-SWIM_SPAN_DB + SWIM_SPAN_DB * this.swimFraction() ** 0.7 + TRIM_DB.swim);
  }

  syncSwim() {
    const want = this.audible() && this.swimSmooth > 0;
    if (this.swim && !want) this.endSwim(0.25);
    if (!want || this.swim) return;
    const t = this.ctx.currentTime + LEAD_S;
    const v = this.voice('swim', 3, t);
    if (!v) return;
    // Flow: a steady, soft rush whose brightness and loudness follow the speed.
    const noise = v.noise('pink');
    const filter = v.filter('lowpass', 500, 0.6);
    const gust = v.gain(0.8);
    const env = v.gain(0);
    v.chain(noise, filter, gust, env, this.graph.swim);
    env.gain.setValueAtTime(0, t);
    env.gain.setTargetAtTime(this.swimLevel(), t, 0.08);
    v.start(null);
    this.swim = { voice: v, env, filter, gust };
    this.next.gust = t;
  }

  endSwim(seconds) {
    const swim = this.swim;
    this.swim = null;
    if (!swim) return;
    const t = this.ctx.currentTime;
    glide(swim.env.gain, t, 0, seconds / 4);
    swim.voice.stop(t + seconds + 0.05);
  }

  pumpSwim(until, now) {
    const swim = this.swim;
    if (!swim) return;
    const r = this.swimFraction();
    glide(swim.env.gain, now, this.swimLevel(), 0.1);
    swim.filter.frequency.setTargetAtTime(280 + 900 * r, now, 0.1);
    if (this.next.gust < now) this.next.gust = now;
    while (this.next.gust < until) {
      swim.gust.gain.setTargetAtTime(this.rand(0.55, 1), this.next.gust, 0.09);
      this.next.gust += 0.3;
    }
  }

  // ---- one-shots ------------------------------------------------------------------------------------------------------------

  /**
   * Reaching the surface from below: a soft wash (no sharp edge), a couple of soft drops and a faint brightening of open air while
   * the bus opens slowly.
   */
  surfaceBreak() {
    const t = this.when();
    if (t === null) return;
    const v = this.voice('surface', 2, t);
    if (!v) return;
    const out = v.gain(dbToGain(TRIM_DB.surface) * this.rand(0.9, 1.1));
    out.connect(this.graph.bus);
    const wash = v.noise('pink', t);
    const washLow = v.filter('lowpass', 800, 0.7);
    washLow.frequency.setValueAtTime(800, t);
    washLow.frequency.exponentialRampToValueAtTime(2200, t + 0.4);
    const washEnv = v.gain(0);
    pulse(washEnv.gain, t, 0.7, 0.07, 0.45);
    v.chain(wash, washLow, washEnv, out);
    for (let i = 0; i < 2; i++) {
      const at = t + 0.14 + i * this.rand(0.1, 0.16);
      const f = this.rand(650, 1050);
      const drop = v.osc('sine', f, at);
      drop.frequency.setValueAtTime(f, at);
      drop.frequency.exponentialRampToValueAtTime(f * 1.18, at + 0.03);
      const dropEnv = v.gain(0);
      pulse(dropEnv.gain, at, this.rand(0.14, 0.22), 0.006, 0.05);
      v.chain(drop, dropEnv, out);
    }
    const air = v.noise('pink', t + 0.05);
    const airBand = v.filter('bandpass', 3000, 0.5);
    const airEnv = v.gain(0);
    pulse(airEnv.gain, t + 0.05, 0.05, 0.16, 0.32);
    v.chain(air, airBand, airEnv, out);
    v.start(t + 0.9);
    this.setUnderwater(false, { delay: 0.05, ramp: 0.5 });
  }

  /** A handset button: `'up'`, `'down'` or `'confirm'` (a slightly different double click): a crisp tick, a short ping, a small thud. */
  click(button = 'up') {
    const pitches = CLICK_PITCH[button] || CLICK_PITCH.up;
    const t = this.when();
    if (t === null) return;
    const v = this.voice('click', 6, t);
    if (!v) return;
    const out = v.gain(dbToGain(TRIM_DB.click));
    out.connect(this.graph.bus);
    pitches.forEach((freq, index) => {
      this.tick(v, t + index * 0.062, out, freq, index === 0 ? 1 : 0.8, SNAP_OFFSETS[index]);
    });
    v.start(t + 0.062 * pitches.length + 0.16);
  }

  /** One tick. The snap's noise is a fixed stretch of the buffer, so every press is as loud as the last. */
  tick(v, t, out, freq, level, offset) {
    const ping = v.osc('sine', freq, t);
    ping.frequency.setValueAtTime(freq * 1.12, t);
    ping.frequency.exponentialRampToValueAtTime(freq, t + 0.014);
    const pingEnv = v.gain(0);
    pulse(pingEnv.gain, t, 0.55 * level, 0.0008, 0.04);
    v.chain(ping, pingEnv, out);
    const snap = v.noise('white', t, offset);
    const snapBand = v.filter('bandpass', 2600, 1.3);
    const snapEnv = v.gain(0);
    pulse(snapEnv.gain, t, 0.55 * level, 0.0005, 0.011);
    v.chain(snap, snapBand, snapEnv, out);
    const thud = v.osc('sine', 190, t);
    thud.frequency.setValueAtTime(190, t);
    thud.frequency.exponentialRampToValueAtTime(115, t + 0.03);
    const thudEnv = v.gain(0);
    pulse(thudEnv.gain, t, 0.3 * level, 0.001, 0.034);
    v.chain(thud, thudEnv, out);
  }

  /**
   * One firmware vibrator pulse (often about 50 ms) as a phone's buzz, at least 120 ms long: the measured motor (see `BUZZ_REAL`),
   * spinning up from 135 Hz to 150 Hz, with the recording's 15 ms attack and its spin-down. It goes straight to the master, past
   * the underwater low-pass: the handset buzzes on the diver's arm and is heard through the bone, not through the water. A pulse
   * that follows within the buzz extends it instead of stacking a second one.
   */
  vibrate(milliseconds = 50) {
    const length = clamp((Number.isFinite(milliseconds) ? milliseconds : 50) / 1000, 0.12, 1.5);
    const t = this.when();
    if (t === null) return;
    const release = BUZZ_RELEASE_S;
    const buzz = this.buzz;
    if (buzz && !buzz.voice.done && t <= buzz.sustainEnd && t + length <= buzz.limit) {
      if (t + length > buzz.sustainEnd) {
        buzz.env.gain.cancelScheduledValues(buzz.sustainEnd);
        buzz.sustainEnd = t + length;
        buzz.env.gain.setValueAtTime(buzz.peak, buzz.sustainEnd);
        buzz.env.gain.exponentialRampToValueAtTime(SILENT, buzz.sustainEnd + release);
        buzz.voice.stop(buzz.sustainEnd + release + 0.03);
      }
      return;
    }
    const v = this.voice('vibrate', 2, t);
    if (!v) return;
    const out = v.gain(dbToGain(TRIM_DB.vibrate));
    out.connect(this.graph.master);
    const motor = v.osc('sine', BUZZ_HZ, t);
    motor.setPeriodicWave(this.graph.buzzWave);
    motor.frequency.setValueAtTime(BUZZ_HZ * BUZZ_SPIN_FROM, t);
    motor.frequency.setTargetAtTime(BUZZ_HZ, t, BUZZ_SPIN_TAU_S);
    const env = v.gain(0);
    v.chain(motor, env, out);
    const peak = 1;
    pulse(env.gain, t, peak, BUZZ_ATTACK_S, release, length - BUZZ_ATTACK_S);
    v.start(t + length + release + 0.03);
    this.buzz = { voice: v, env, peak, sustainEnd: t + length, limit: t + 2.5 };
  }

  /** The torch: a small switch click; on also gives a faint hum that swells and settles. */
  torch(on) {
    const t = this.when();
    if (t === null) return;
    const v = this.voice('torch', 2, t);
    if (!v) return;
    const out = v.gain(dbToGain(on ? TRIM_DB.torchOn : TRIM_DB.torchOff));
    out.connect(this.graph.bus);
    const press = (at, freq, level) => {
      const snap = v.noise('white', at);
      const band = v.filter('bandpass', 1900, 1.4);
      const snapEnv = v.gain(0);
      pulse(snapEnv.gain, at, 0.5 * level, 0.0005, 0.012);
      v.chain(snap, band, snapEnv, out);
      const ping = v.osc('sine', freq, at);
      const pingEnv = v.gain(0);
      pulse(pingEnv.gain, at, 0.4 * level, 0.001, 0.03);
      v.chain(ping, pingEnv, out);
    };
    if (on) {
      press(t, 760, 1);
      press(t + 0.026, 1100, 0.6);
      const start = t + 0.05;
      const hum = v.osc('sine', 124, start);
      hum.frequency.setValueAtTime(124, start);
      hum.frequency.exponentialRampToValueAtTime(118, start + 0.8);
      const second = v.osc('triangle', 236, start);
      const third = v.osc('sine', 354, start);
      const secondLevel = v.gain(0.35);
      const thirdLevel = v.gain(0.15);
      const mix = v.gain(1);
      const shape = v.filter('lowpass', 700, 0.7);
      const env = v.gain(0);
      const humOut = v.gain(dbToGain(TRIM_DB.torchHum)); // the hum is far fainter than the click
      hum.connect(mix);
      second.connect(secondLevel);
      secondLevel.connect(mix);
      third.connect(thirdLevel);
      thirdLevel.connect(mix);
      v.chain(mix, shape, env, humOut, this.graph.bus);
      pulse(env.gain, start, 0.5, 0.22, 0.9, 0.35);
      v.start(start + 1.6);
    } else {
      press(t, 520, 0.9);
      const fall = v.osc('sine', 260, t + 0.02);
      fall.frequency.setValueAtTime(260, t + 0.02);
      fall.frequency.exponentialRampToValueAtTime(130, t + 0.16);
      const fallEnv = v.gain(0);
      pulse(fallEnv.gain, t + 0.02, 0.1, 0.01, 0.13);
      v.chain(fall, fallEnv, out);
      v.start(t + 0.3);
    }
  }

  /** A soft bell note: a sine and a faster-fading second partial. */
  bell(v, t, out, freq, level, length) {
    const first = v.osc('sine', freq, t);
    const firstEnv = v.gain(0);
    pulse(firstEnv.gain, t, 0.5 * level, 0.006, length);
    v.chain(first, firstEnv, out);
    const second = v.osc('sine', freq * 2.005, t);
    const secondEnv = v.gain(0);
    pulse(secondEnv.gain, t, 0.14 * level, 0.004, length * 0.45);
    v.chain(second, secondEnv, out);
  }

  /** "Reset all" and Quit: three falling notes that settle. */
  powerDown() {
    const t = this.when();
    if (t === null) return;
    const v = this.voice('power', 2, t);
    if (!v) return;
    const out = v.gain(dbToGain(TRIM_DB.powerDown));
    out.connect(this.graph.bus);
    this.bell(v, t, out, 784, 1, 0.16);
    this.bell(v, t + 0.11, out, 587.3, 0.95, 0.18);
    this.bell(v, t + 0.22, out, 392, 0.9, 0.55);
    v.start(t + 0.22 + 0.55 + 0.05);
  }

  /** A game session starts (also after Reset all): a rising arpeggio that rings out. */
  powerUp() {
    const t = this.when();
    if (t === null) return;
    const v = this.voice('power', 2, t);
    if (!v) return;
    const out = v.gain(dbToGain(TRIM_DB.powerUp));
    out.connect(this.graph.bus);
    [523.25, 659.25, 783.99, 1046.5].forEach((freq, index) => {
      this.bell(v, t + index * 0.085, out, freq, index === 3 ? 1 : 0.8, index === 3 ? 0.85 : 0.22);
    });
    v.start(t + 3 * 0.085 + 0.85 + 0.05);
  }

  // ---- shutdown ------------------------------------------------------------------------------------------------------------

  /** Stops every sound at once (all voices are released); the context stays. */
  silence() {
    this.amb = null;
    this.mav = null;
    this.swim = null;
    this.buzz = null;
    for (const voice of [...this.voices]) voice.kill();
    this.updatePump();
  }

  /** Releases everything: voices, timers, listeners and (when it created it) the context. */
  dispose() {
    if (this.disposed) return;
    this.silence();
    this.disposed = true;
    this.clips = null;
    if (this.timer !== null) clearInterval(this.timer);
    this.timer = null;
    if (this.suspendTimer !== null) clearTimeout(this.suspendTimer);
    this.suspendTimer = null;
    if (this.visibility && typeof document !== 'undefined' && typeof document.removeEventListener === 'function') {
      document.removeEventListener('visibilitychange', this.visibility);
    }
    this.visibility = null;
    if (this.ctx) {
      this.ctx.onstatechange = null;
      if (!this.externalContext) this.ctx.close().catch(() => {});
    }
    this.graph = null;
  }
}

/** The sound for the game; see the header and `GameSound` for the calls. */
export function createGameSound(options) {
  return new GameSound(options);
}
