// Simulated conditions: how the basic controls and the Advanced raw inputs reach the engine without racing.
//
// Ported from the action queue and input handling of the analysis workspace's emulation/viewer.html (README "Controls"):
//
//  * Basic edits (oxygen base + offsets, surface pressure + depth + water type + offsets, temperature base +
//    offsets) apply immediately: the seven raw model inputs are calculated, copied into the raw fields and sent as
//    one `inputs` action. Invalid sums or incomplete numbers (a blank field is not zero) show "Not applied" and
//    send nothing; readings are never clipped.
//  * Every UI action goes through one queue: actions run in order, one at a time, and adjacent *pending* basic
//    updates coalesce to the newest. An explicit raw "Apply inputs" is an ordered snapshot (it is never merged and
//    keeps its place between basic updates).
//  * Revision counters keep stale replies from overwriting newer edits: a reply rebases the basic controls only
//    when no basic edit and no newer raw sensor edit happened since the request was made, and it refreshes the raw
//    fields only when no raw edit happened since. Status polls and broadcasts never touch the input fields.
//  * Raw fields are drafts until "Apply inputs"; the status says "Applying…", "Inputs applied" or "Raw edits
//    pending", never presenting a draft as a sensor update. Bases and offsets are derived from the current raw
//    inputs when a session starts (nothing is stored separately).
//
// No DOM: the page supplies a `view` adapter and the Node tests supply a fake one.
//
//   view.getBasic(id) -> string            view.setBasic(id, value)      (a slider may quantize the value)
//   view.getRaw(name) -> string|boolean    view.setRaw(name, value)      (unknown names are ignored)
//   view.rawFields() -> [{name, checkbox}] view.rawValid(name) -> boolean  (the field's own min/max/step)
//   view.renderPreview(settings, values)   view.setStatus(text)          view.setError(text|null)

import { PRESSURE_KEYS, SENSOR_KEYS, TEMPERATURE_KEYS, calculate, explainRejection, fromInputs, pressureMbar } from './sensors.js';

export const BASIC_IDS = Object.freeze([
  'oxygen-base', 'oxygen-offset-1', 'oxygen-offset-2', 'oxygen-offset-3',
  'surface-pressure', 'depth', 'water-type', 'pressure-offset-1', 'pressure-offset-2',
  'temperature-base', 'temperature-offset-1', 'temperature-offset-2',
]);

export const STATUS = Object.freeze({
  connecting: 'Connecting',
  applying: 'Applying…',
  applied: 'Inputs applied',
  pending: 'Raw edits pending',
  rejected: 'Not applied',
});

/** Serializes UI actions; adjacent pending basic updates coalesce to the newest. */
/** The engine's answer to a handset press while the previous button pulse still runs (`ButtonsError::Busy`, the runner's message). */
export const BUTTON_PULSE_BUSY = 'A button pulse is already in progress';
const HANDSET_PRESSES = new Set(['up', 'down', 'confirm']);

/**
 * A handset press refused because the previous pulse (204.8 ms, Confirm a little longer) still runs. Like pressing a button that
 * is already down, it simply does nothing: the views drop it without showing an error.
 */
export function isBusyPress(request, error) {
  return !!request && HANDSET_PRESSES.has(request.action) && !!error && error.message === BUTTON_PULSE_BUSY;
}

export class ActionQueue {
  /**
   * @param {object} hooks
   * @param {(payload: object) => Promise<object>} hooks.perform sends one action, resolves with the new state
   * @param {(request: object) => void} [hooks.before] called when a request is about to be sent
   * @param {(request: object, state: object) => void} [hooks.onResult] called with the reply of a request
   * @param {(request: object, error: Error) => void} [hooks.onError] called when a request failed
   * @param {(busy: boolean) => void} [hooks.onBusy] called when the queue starts / stops working
   */
  constructor({ perform, before, onResult, onError, onBusy }) {
    this.perform = perform;
    this.before = before || (() => {});
    this.onResult = onResult || (() => {});
    this.onError = onError || (() => {});
    this.onBusy = onBusy || (() => {});
    this.items = [];
    this.busy = false;
  }

  get pending() {
    return this.items.length;
  }

  /** Queues an action; resolves to true when it was applied, false when it failed or was superseded. */
  send(action, extra = {}, meta = {}) {
    const payload = JSON.parse(JSON.stringify({ action, ...extra }));
    return new Promise((resolve) => {
      const request = { action, payload, meta: { ...meta }, resolve };
      const previous = this.items[this.items.length - 1];
      if (meta.kind === 'basic' && previous && previous.meta.kind === 'basic') this.items.pop().resolve(false);
      this.items.push(request);
      this.drain();
    });
  }

  /** Discards the newest queued basic updates (a later invalid or incomplete edit supersedes unsent values). */
  dropTrailingBasic() {
    while (this.items.length && this.items[this.items.length - 1].meta.kind === 'basic') this.items.pop().resolve(false);
  }

  /** Discards every queued (not yet sent) request of `kind`. */
  dropQueued(kind) {
    const kept = [];
    for (const request of this.items) {
      if (request.meta.kind === kind) request.resolve(false);
      else kept.push(request);
    }
    this.items = kept;
  }

  async drain() {
    if (this.busy || !this.items.length) return;
    this.busy = true;
    this.onBusy(true);
    try {
      while (this.items.length) {
        const request = this.items.shift();
        let applied = false;
        try {
          this.before(request);
          const state = await this.perform(request.payload);
          this.onResult(request, state);
          applied = true;
        } catch (error) {
          this.onError(request, error);
        }
        request.resolve(applied);
      }
    } finally {
      this.busy = false;
      this.onBusy(false);
    }
  }
}

function readNumber(view, id, label) {
  const text = String(view.getBasic(id)).trim();
  const value = text === '' ? NaN : Number(text);
  if (!Number.isFinite(value)) throw new RangeError(`${label} needs a number.`);
  return value;
}

/** The basic and raw input logic of the "Simulated conditions" section and the Advanced raw form. */
export class ConditionsController {
  /**
   * @param {object} options
   * @param {object} options.view the page adapter (see the top of this file)
   * @param {ActionQueue} options.queue the shared action queue
   * @param {(text: string) => void} [options.onActionError] reports an error shown with the other action errors
   */
  constructor({ view, queue, onActionError }) {
    this.view = view;
    this.queue = queue;
    this.onActionError = onActionError || (() => {});
    this.scenarioRevision = 0;
    this.rawRevision = 0;
    this.rawSensorRevision = 0;
    this.scenarioSettings = null;
  }

  readBasic() {
    const view = this.view;
    const number = (id, label) => readNumber(view, id, label);
    return {
      oxygenBaseMv: number('oxygen-base', 'Oxygen base'),
      oxygenVariationsMv: [1, 2, 3].map((i) => number(`oxygen-offset-${i}`, `Oxygen cell ${i} offset`)),
      surfacePressureMbar: number('surface-pressure', 'Surface pressure'),
      depthM: number('depth', 'Depth'),
      waterType: view.getBasic('water-type'),
      pressureVariationsMbar: [1, 2].map((i) => number(`pressure-offset-${i}`, `Pressure sensor ${i} offset`)),
      temperatureBaseC: number('temperature-base', 'Base temperature'),
      temperatureVariationsC: [1, 2].map((i) => number(`temperature-offset-${i}`, `Temperature sensor ${i} offset`)),
    };
  }

  writeRawSensors(inputs) {
    for (const name of SENSOR_KEYS) this.view.setRaw(name, inputs[name]);
  }

  /** Derives bases and offsets from the raw readings (the bases are quantized by their sliders, offsets compensate). */
  syncBasic(inputs) {
    const view = this.view;
    const settings = fromInputs(inputs, this.scenarioSettings);
    view.setBasic('oxygen-base', settings.oxygenBaseMv);
    view.setBasic('depth', settings.depthM);
    view.setBasic('temperature-base', settings.temperatureBaseC);
    view.setBasic('surface-pressure', settings.surfacePressureMbar);
    view.setBasic('water-type', settings.waterType);
    // Range controls quantize to their step. Recompute offsets from their actual values so attaching or raw Apply
    // preserves every reading.
    settings.oxygenBaseMv = readNumber(view, 'oxygen-base', 'Oxygen base');
    settings.depthM = readNumber(view, 'depth', 'Depth');
    settings.temperatureBaseC = readNumber(view, 'temperature-base', 'Base temperature');
    const pressure = pressureMbar(settings.surfacePressureMbar, settings.depthM, settings.waterType);
    for (const i of [1, 2, 3]) view.setBasic(`oxygen-offset-${i}`, inputs[`oxygen${i}Mv`] - settings.oxygenBaseMv);
    // Page sensor numbers are the firmware's, the reverse of the engine's keys (sensors.js): sensor 1 is pressure2Mbar.
    for (const i of [1, 2]) {
      view.setBasic(`pressure-offset-${i}`, inputs[PRESSURE_KEYS[i - 1]] - pressure);
      view.setBasic(`temperature-offset-${i}`, inputs[TEMPERATURE_KEYS[i - 1]] - settings.temperatureBaseC);
    }
    this.scenarioSettings = this.readBasic();
    view.renderPreview(this.scenarioSettings, calculate(this.scenarioSettings));
    view.setError(null);
  }

  /** A session starts (or the profile was replaced): load every field from the engine's inputs, discard drafts. */
  attach(inputs) {
    this.scenarioRevision++;
    this.rawRevision++;
    this.rawSensorRevision++;
    this.queue.dropQueued('basic');
    for (const [name, value] of Object.entries(inputs || {})) this.view.setRaw(name, value);
    try {
      this.syncBasic(inputs);
    } catch (error) {
      this.view.setError(`The basic controls could not be derived from the current inputs (${error.message}). Use the raw inputs under Advanced.`);
    }
    this.view.setStatus(STATUS.applied);
  }

  /** A basic control changed: calculate, show the raw readings and apply, or refuse and send nothing. */
  basicChanged() {
    this.scenarioRevision++;
    this.rawRevision++;
    this.rawSensorRevision++;
    let inputs;
    try {
      const settings = this.readBasic();
      inputs = calculate(settings);
      this.scenarioSettings = settings;
      this.view.renderPreview(settings, inputs);
      this.writeRawSensors(inputs);
      this.view.setError(null);
      this.view.setStatus(STATUS.applying);
    } catch (error) {
      // A newer incomplete or invalid edit supersedes unsent slider values.
      this.queue.dropTrailingBasic();
      this.view.setError(explainRejection(error.message));
      this.view.setStatus(STATUS.rejected);
      return Promise.resolve(false);
    }
    return this.queue.send('inputs', { inputs }, { kind: 'basic', scenarioRevision: this.scenarioRevision, rawRevision: this.rawRevision });
  }

  /** A raw field was edited: it is a draft until Apply inputs. */
  rawEdited(name) {
    this.rawRevision++;
    if (SENSOR_KEYS.includes(name)) this.rawSensorRevision++;
    this.view.setStatus(STATUS.pending);
  }

  /** "Apply inputs": an ordered snapshot of every raw field. */
  submitRaw() {
    const inputs = {};
    for (const { name, checkbox } of this.view.rawFields()) {
      if (checkbox) {
        inputs[name] = !!this.view.getRaw(name);
        continue;
      }
      const text = String(this.view.getRaw(name)).trim();
      const value = text === '' ? NaN : Number(text);
      if (!Number.isFinite(value) || !this.view.rawValid(name)) {
        this.onActionError('Complete the raw inputs before applying.');
        return Promise.resolve(false);
      }
      inputs[name] = value;
    }
    this.view.setStatus(STATUS.applying);
    return this.queue.send('inputs', { inputs }, {
      kind: 'raw', scenarioRevision: this.scenarioRevision, rawRevision: this.rawRevision, rawSensorRevision: this.rawSensorRevision,
    });
  }

  /** Reply of an `inputs` request (queue `onResult`). */
  handleResult(request, state) {
    const meta = request.meta;
    if (meta.kind !== 'basic' && meta.kind !== 'raw') return;
    const inputs = state && state.inputs;
    if (meta.kind === 'raw' && inputs) {
      if (meta.scenarioRevision === this.scenarioRevision && meta.rawSensorRevision === this.rawSensorRevision) {
        try {
          this.syncBasic(inputs);
        } catch (error) {
          this.view.setError(`The basic controls could not be derived from the applied inputs (${error.message}).`);
        }
      }
      if (meta.rawRevision === this.rawRevision) {
        for (const [name, value] of Object.entries(inputs)) this.view.setRaw(name, value);
      }
    }
    if (meta.scenarioRevision === this.scenarioRevision && meta.rawRevision === this.rawRevision) this.view.setStatus(STATUS.applied);
  }

  /** A request failed (queue `onError`): the draft stays, nothing is retried. */
  handleError(request) {
    if (request.meta.kind === 'basic' || request.meta.kind === 'raw') this.view.setStatus(STATUS.rejected);
  }
}
