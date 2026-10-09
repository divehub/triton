// Output histories and the "Replay pulses" visibility aid.
//
// Ported from the analysis workspace's emulation/viewer.html (`output-replay` script and the output rendering around it), keeping
// the rules of its README ("Hardware outputs and UART console"):
//
//  * The engine reports, per output, bounded histories of the commanded drive (DESIGN 15.3a): `activity` and, for
//    the handset vibrator, `pwmActivity`. They are commanded drive sampled at the system's quantum boundaries
//    (HUD every 50 virtual ms, motor command every 20 virtual ms, PB15 enable changes exactly), not physical
//    edges or actuator power. A command shorter than the sampling period can be missed.
//  * Current On stays steady. When the current command is Off, newly observed activations that happened between
//    two state updates queue wall-clock flashes (150 ms on, 100 ms gap, at most 12 per output). The first
//    observation (initial load, reconnect, an Unknown drive) is a baseline: old history never replays. A new
//    `outputHistoryEpoch`, missing history, a count regression, a changed overlap or a sequence gap clears the
//    queue.
//  * Replay is a visibility aid. It never changes what the firmware did, the captured timestamps or the Drive
//    labels, and its duration says nothing about firmware or physical timing. Turn it off to see current drive
//    alone.
//
// Everything here is DOM-free (timers are injected) so the Node tests exercise exactly what the page runs.

export const REPLAY_ON_MS = 150;
export const REPLAY_GAP_MS = 100;
export const REPLAY_QUEUE_CAPACITY = 12;

const isSequence = (value) => Number.isSafeInteger(value) && value >= 0;
const isTime = (value) => typeof value === 'number' && Number.isFinite(value);

function eventActive(event, source) {
  if (typeof event.active === 'boolean') return event.active;
  return source === 'gpio-enable-command' && typeof event.level === 'boolean' ? event.level : null;
}

function fingerprint(event) {
  const command = event.command && typeof event.command === 'object'
    ? Object.keys(event.command).sort().map((key) => [key, event.command[key]]) : null;
  return JSON.stringify([event.virtualTime, event.kind, event.active, event.level, event.dutyPercent, command]);
}

/**
 * Consumes one output's retained history, snapshot after snapshot, and reports which activations are new.
 * An explicit lifecycle domain detects resets even if time and history repeat.
 */
export class Cursor {
  constructor() {
    this.reset();
  }

  reset() {
    this.attached = false;
    this.source = null;
    this.outputId = null;
    this.domain = null;
    this.virtualTime = null;
    this.sequence = 0;
    this.observedActive = null;
    this.fingerprints = new Map();
    this.totalGapCount = 0;
  }

  // output.pwmActivity takes precedence: PB15 can stay high across motor PWM pulses. GPIO enable-level events are
  // only the fallback source. First attach, reset and source changes baseline the retained tail. gapCount counts
  // missing history events, not inferred missing pulses.
  observe(output, virtualTime, resetDomain = null) {
    const activity = output && (output.pwmActivity || output.activity);
    const result = {
      activations: [], source: null, observedActive: null,
      attached: false, reset: false, gapCount: 0, totalGapCount: this.totalGapCount,
      baselineEventCount: 0, invalidEventCount: 0,
    };
    if (!activity || typeof activity.source !== 'string' || !isSequence(activity.eventCount)) {
      result.reset = this.attached;
      this.reset();
      result.totalGapCount = 0;
      return result;
    }
    const source = activity.source;
    const count = activity.eventCount;
    const events = [];
    const seen = new Set();
    for (const event of Array.isArray(activity.events) ? activity.events : []) {
      if (!event || !isSequence(event.sequence) || event.sequence === 0
          || event.sequence > count || !isTime(event.virtualTime) || seen.has(event.sequence)) {
        result.invalidEventCount++;
        continue;
      }
      seen.add(event.sequence);
      events.push(event);
    }
    events.sort((a, b) => a.sequence - b.sequence);
    const overlapChanged = events.some((event) => this.fingerprints.has(event.sequence)
      && this.fingerprints.get(event.sequence) !== fingerprint(event));
    const domainChanged = this.attached && (!Object.is(resetDomain, this.domain)
      || source !== this.source || output.id !== this.outputId
      || count < this.sequence || overlapChanged
      || (isTime(virtualTime) && isTime(this.virtualTime) && virtualTime < this.virtualTime));
    if (!this.attached || domainChanged) {
      result.attached = true;
      result.reset = this.attached;
      this.totalGapCount = 0;
      this.observedActive = events.length ? eventActive(events[events.length - 1], source)
        : source === 'gpio-enable-command' && typeof output.level === 'boolean' ? output.level
          : typeof output.active === 'boolean' ? output.active : null;
      result.baselineEventCount = events.length;
    } else {
      let expected = this.sequence + 1;
      for (const event of events) {
        if (event.sequence < expected) continue;
        if (event.sequence > expected) {
          result.gapCount += event.sequence - expected;
          // The missing tail may contain an Off or On command. Establish a new baseline rather than inventing a
          // rising transition.
          this.observedActive = null;
        }
        const active = eventActive(event, source);
        if (event.kind === 'change' && this.observedActive === false && active === true) {
          result.activations.push({
            sequence: event.sequence,
            virtualTime: event.virtualTime,
            dutyPercent: typeof event.dutyPercent === 'number' && Number.isFinite(event.dutyPercent) ? event.dutyPercent : null,
            source,
          });
        }
        this.observedActive = active;
        expected = event.sequence + 1;
      }
      if (expected <= count) {
        result.gapCount += count - expected + 1;
        this.observedActive = null;
      }
      this.totalGapCount += result.gapCount;
    }
    this.attached = true;
    this.source = source;
    this.outputId = output.id;
    this.domain = resetDomain;
    this.virtualTime = isTime(virtualTime) ? virtualTime : this.virtualTime;
    this.sequence = count;
    this.fingerprints = new Map(events.map((event) => [event.sequence, fingerprint(event)]));
    result.source = source;
    result.observedActive = this.observedActive;
    result.totalGapCount = this.totalGapCount;
    return result;
  }
}

/** Bounded queue of pending flashes: the freshest are kept and the number of dropped ones is reported. */
export class PulseQueue {
  constructor(capacity = REPLAY_QUEUE_CAPACITY) {
    if (!Number.isSafeInteger(capacity) || capacity < 1) throw new RangeError('Pulse queue capacity must be a positive integer.');
    this.capacity = capacity;
    this.clear();
  }

  get length() {
    return this.items.length;
  }

  enqueue(activations) {
    for (const activation of activations) {
      this.items.push(activation);
      if (this.items.length > this.capacity) {
        this.items.shift();
        this.droppedCount++;
      }
    }
    return this.length;
  }

  shift() {
    return this.items.shift();
  }

  clear() {
    this.items = [];
    this.droppedCount = 0;
  }
}

// ---- presentation (pure functions, shared by the page and the tests) -----------------------------------------

/** Outputs shown in the basic status strip: the color is fixed by channel (HUD 3 red, HUD 2 white). */
export const STATUS_STRIP = Object.freeze({
  'handset-vibrator': Object.freeze({ color: 'neutral', vibrator: true }),
  'main-hud-3': Object.freeze({ color: 'red', vibrator: false }),
  'main-hud-2': Object.freeze({ color: 'white', vibrator: false }),
});

/** CSS class of the color label the engine reports for an LED (`neutral` while unassigned). */
export function colorClass(output) {
  return output && ['red', 'white'].includes(output.color) ? output.color : 'neutral';
}

const lightClass = (entry, on, color) => ['output-light', entry.active === null ? 'unknown' : (on ? 'on' : 'off'), color, entry.kind === 'vibrator' ? 'vibrator' : ''].filter(Boolean).join(' ');

/** What an output indicator shows right now: Drive (current command) and Replay (a flash of a captured pulse). */
export function describeEntry(entry, replayEnabled) {
  const replayOn = !!(replayEnabled && entry.replayOn && entry.replayEvent);
  const on = entry.active === true || replayOn;
  const driveText = entry.active === null ? 'Unknown' : entry.active ? 'On' : 'Off';
  const title = replayOn
    ? `Captured pulse from ${entry.replayEvent.virtualTime.toFixed(3)} virtual s`
    : `Current drive: ${driveText}`;
  const strip = STATUS_STRIP[entry.id];
  let replayLabel;
  if (!replayEnabled) replayLabel = 'Pulse replay disabled';
  else if (replayOn) replayLabel = `Replaying ${entry.replayEvent.virtualTime.toFixed(3)} s pulse`;
  else if (entry.replayTimer !== null) replayLabel = 'Pulse replay gap';
  else if (entry.active === true) replayLabel = 'Current drive is on';
  else replayLabel = 'Pulse replay caught up';
  return {
    on,
    replayOn,
    title,
    indicatorClass: lightClass(entry, on, entry.outputColor),
    simple: strip ? {
      lightClass: lightClass(entry, on, strip.color),
      stateText: entry.active === null ? 'Unknown' : replayOn ? 'Pulse' : entry.active ? 'On' : 'Off',
    } : null,
    replayHidden: !entry.cursor.attached,
    replayText: [
      replayLabel,
      entry.queue.length ? `${entry.queue.length} queued` : '',
      entry.queue.droppedCount ? `${entry.queue.droppedCount} replay pulses skipped` : '',
      entry.cursor.totalGapCount ? `${entry.cursor.totalGapCount} history events unavailable` : '',
    ].filter(Boolean).join(' · '),
  };
}

const seconds = (value) => (typeof value === 'number' && Number.isFinite(value) ? `${value.toFixed(3)} s` : 'none');

/** "Drive: Off · PWM 0.0% · Pin high": the current command, kept apart from the replay label. */
export function driveText(output) {
  const active = typeof output.active === 'boolean' ? output.active : null;
  const stateLabel = active === null ? 'Unknown' : (active ? 'On' : 'Off');
  const duty = typeof output.dutyPercent === 'number' && Number.isFinite(output.dutyPercent) ? `PWM ${output.dutyPercent.toFixed(1)}%` : '';
  const level = typeof output.level === 'boolean' ? `Pin ${output.level ? 'high' : 'low'}` : '';
  return [`Drive: ${stateLabel}`, duty, level].filter(Boolean).join(' · ');
}

/** Counts and last on/off times of the history that drives replay (null when the output has none). */
export function activityText(output) {
  const activity = output && (output.pwmActivity || output.activity);
  if (!activity || typeof activity.source !== 'string') return null;
  const sampled = activity.source.startsWith('sampled-');
  return [
    `${activity.activationCount} ${sampled ? 'activations' : 'enable commands'}`,
    `last on ${seconds(activity.lastOnVirtualTime)}`,
    `last off ${seconds(activity.lastOffVirtualTime)}`,
    sampled ? `sampled every ${Math.round(activity.samplingPeriodSeconds * 1000)} ms` : '',
    output.pwmActivity && output.activity ? `${output.activity.activationCount} enable commands` : '',
  ].filter(Boolean).join(' · ');
}

/** The retained commands, newest first, in virtual time (empty text when there is no history). */
export function historyText(output) {
  const activity = output && (output.pwmActivity || output.activity);
  if (!activity) return '';
  const events = [...(activity.events || []), ...(output.pwmActivity && output.activity ? output.activity.events || [] : [])];
  return events.sort((a, b) => b.virtualTime - a.virtualTime).map((event) => {
    const level = typeof event.level === 'boolean' ? (event.level ? 'Enable high' : 'Enable low')
      : typeof event.active === 'boolean' ? (event.active ? 'On' : 'Off') : 'Unknown';
    const duty = typeof event.dutyPercent === 'number' ? `PWM ${event.dutyPercent.toFixed(1)}%` : '';
    return [seconds(event.virtualTime), level, duty, event.kind === 'initial-sample' ? '(baseline)' : ''].filter(Boolean).join(' · ');
  }).join('\n') + (activity.truncated ? '\nEarlier commands omitted; totals retained.' : '');
}

// ---- the replay controller ----------------------------------------------------------------------------------

/**
 * Per-output replay state: a cursor over the history, a bounded flash queue and the flash timers.
 *
 *   draw(entry)   called whenever an entry's indicator must be repainted (also from the flash timers)
 *   enabled()     the "Replay pulses" setting
 *   paused()      true while the page is hidden or disconnected: nothing is queued and nothing flashes
 */
export class ReplayController {
  constructor({ setTimer, clearTimer, draw, enabled, paused } = {}) {
    this.setTimer = setTimer || ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = clearTimer || ((handle) => clearTimeout(handle));
    this.draw = draw || (() => {});
    this.enabled = enabled || (() => true);
    this.paused = paused || (() => false);
    this.entries = new Map();
    this.generation = 0;
  }

  makeEntry(output) {
    return {
      id: output.id, kind: output.kind, output,
      cursor: new Cursor(), queue: new PulseQueue(REPLAY_QUEUE_CAPACITY),
      active: null, outputColor: 'neutral', replayTimer: null, replayOn: false, replayEvent: null,
    };
  }

  cancel(entry, resetCursor = false) {
    if (entry.replayTimer !== null) this.clearTimer(entry.replayTimer);
    entry.replayTimer = null;
    entry.replayOn = false;
    entry.replayEvent = null;
    entry.queue.clear();
    if (resetCursor) entry.cursor.reset();
  }

  playNext(entry) {
    if (entry.replayTimer !== null || !entry.queue.length || entry.active !== false || this.paused() || !this.enabled()) return;
    entry.replayEvent = entry.queue.shift();
    entry.replayOn = true;
    entry.replayTimer = this.setTimer(() => {
      entry.replayTimer = null;
      entry.replayOn = false;
      entry.replayEvent = null;
      entry.replayTimer = this.setTimer(() => {
        entry.replayTimer = null;
        this.playNext(entry);
        this.draw(entry);
      }, REPLAY_GAP_MS);
      this.draw(entry);
    }, REPLAY_ON_MS);
    this.draw(entry);
  }

  /** Forgets every queue and baseline (Replay toggled, page hidden or closed, board reset). */
  clear() {
    this.generation++;
    for (const entry of this.entries.values()) {
      this.cancel(entry, true);
      this.draw(entry);
    }
  }

  /**
   * Takes the outputs of one state. `epoch` is the lifecycle domain (`outputHistoryEpoch`); the controller adds
   * its own generation. Returns `{entries, removed}`: the live entries in output order and the ones that went away
   * (the page removes their rows).
   */
  update(outputs, { virtualTime, epoch }) {
    const list = Array.isArray(outputs) ? outputs.filter((output) => output && typeof output.id === 'string') : [];
    const ids = new Set(list.map((output) => output.id));
    const removed = [];
    for (const [id, entry] of this.entries) {
      if (!ids.has(id)) {
        this.cancel(entry, true);
        this.entries.delete(id);
        removed.push(entry);
      }
    }
    const entries = [];
    for (const output of list) {
      let entry = this.entries.get(output.id);
      if (entry && entry.kind !== output.kind) {
        this.cancel(entry, true);
        this.entries.delete(output.id);
        removed.push(entry);
        entry = null;
      }
      if (!entry) {
        entry = this.makeEntry(output);
        this.entries.set(output.id, entry);
      }
      entry.output = output;
      entry.active = typeof output.active === 'boolean' ? output.active : null;
      entry.outputColor = colorClass(output);
      const observed = entry.cursor.observe(output, virtualTime, `${epoch}:${this.generation}`);
      if (observed.reset || observed.gapCount) this.cancel(entry);
      if (entry.active === null) {
        this.cancel(entry, true);
      } else if (entry.active === true || !this.enabled() || this.paused()) {
        this.cancel(entry);
      } else {
        entry.queue.enqueue(observed.activations);
        this.playNext(entry);
      }
      this.draw(entry);
      entries.push(entry);
    }
    return { entries, removed };
  }

  get pendingTimers() {
    let count = 0;
    for (const entry of this.entries.values()) if (entry.replayTimer !== null) count++;
    return count;
  }
}
