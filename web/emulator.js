// Emulator screen: renders the state and frames the worker publishes and turns UI controls into actions.
//
// Ported from emulation/viewer.html of the analysis workspace (the basic view with the alert status strip and "Simulated conditions", the
// closed "Advanced" panel with raw inputs, outputs, histories and "Replay pulses"); the HTTP polling is replaced by
// worker messages. The decisions live in DOM-free modules the Node tests drive directly:
//   conditions.js  action queue, basic/raw input logic     sensors.js  simulated-conditions arithmetic
//   replay.js      output histories and replay             keys.js     handset keyboard shortcuts

import { byId, confirmDialog, formatBytes, formatClock, h, hex32, prefs } from './dom.js';
import { ActionQueue, BASIC_IDS, ConditionsController } from './conditions.js';
import { handsetKeyAction } from './keys.js';
import { LcdView } from './lcd.js';
import { ReplayController, STATUS_STRIP, activityText, describeEntry, driveText, historyText } from './replay.js';
import { describeRelease } from './releases.js';
import { readZip } from './zip.js';

const PROFILE_FILES = ['eeprom.bin', 'nor.ngc', 'rtc-state.json', 'inputs.json', 'led-colors.json'];
// Actions that recreate the boards: output histories start over.
const RESET_ACTIONS = new Set(['reset', 'cold', 'wake', 'serial']);

/** The DOM side of `ConditionsController`: the basic controls, the raw sensor form and the status line. */
class ConditionsDom {
  constructor() {
    this.form = byId('sensor-form');
    this.fields = [...this.form.querySelectorAll('input')];
  }

  field(name) {
    return this.form.elements.namedItem(name);
  }

  getBasic(id) {
    return byId(id).value;
  }

  setBasic(id, value) {
    byId(id).value = String(value);
  }

  getRaw(name) {
    const input = this.field(name);
    if (!input) return '';
    return input.type === 'checkbox' ? input.checked : input.value;
  }

  setRaw(name, value) {
    const input = this.field(name);
    if (!input) return;
    if (input.type === 'checkbox') input.checked = !!value;
    else input.value = value;
  }

  rawFields() {
    return this.fields.map((input) => ({ name: input.name, checkbox: input.type === 'checkbox' }));
  }

  rawValid(name) {
    const input = this.field(name);
    return !!input && input.checkValidity();
  }

  renderPreview(settings, values) {
    byId('oxygen-base-value').textContent = `${settings.oxygenBaseMv.toFixed(2)} mV`;
    byId('depth-value').textContent = `${settings.depthM.toFixed(2)} m`;
    byId('temperature-base-value').textContent = `${settings.temperatureBaseC.toFixed(2)} °C`;
    byId('preview-oxygen').textContent = `${[1, 2, 3].map((i) => `Cell ${i}: ${values[`oxygen${i}Mv`].toFixed(2)}`).join(' · ')} mV`;
    byId('preview-pressure').textContent = `${[1, 2].map((i) => `P${i}: ${values[`pressure${i}Mbar`].toFixed(2)}`).join(' · ')} mbar`;
    byId('preview-temperature').textContent = `${[1, 2].map((i) => `T${i}: ${values[`temperature${i}C`].toFixed(2)}`).join(' · ')} °C`;
  }

  setStatus(text) {
    const status = byId('basic-input-status');
    status.textContent = text;
    // A second cue besides the words: a refused edit and an unapplied draft are coloured.
    status.dataset.state = text === 'Not applied' ? 'rejected' : text === 'Raw edits pending' ? 'pending' : 'ok';
  }

  setError(text) {
    const box = byId('basic-input-error');
    box.hidden = !text;
    if (text) box.textContent = text;
  }
}

export class EmulatorView {
  /**
   * @param {import('./worker-client.js').WorkerClient} client
   * @param {{closeSession: () => void, notify: (level: string, text: string) => void}} hooks
   * @param {{timers?: {setTimer: Function, clearTimer: Function}}} [options] timers drive the pulse replay (tests inject a fake clock)
   */
  constructor(client, hooks, { timers } = {}) {
    this.client = client;
    this.hooks = hooks;
    this.root = byId('screen-emulator');
    this.lcd = new LcdView({ container: byId('lcd'), canvas: byId('frame'), placeholder: byId('placeholder'), sizeLabel: byId('frame-size') });
    this.state = null;
    this.host = null;
    this.info = null;
    this.haveFrame = false;
    this.inputsEpoch = null; // `profileEpoch` of the state whose inputs the form shows
    this.canConnected = true;
    this.actionError = '';
    this.connectionError = '';
    this.notices = [];
    this.advancing = false;
    this.outputRows = new Map();
    this.uartChannels = new Map();
    this.lastNote = 0;
    this.noteShowsFrame = false; // the note under the LCD names the last frame (not "No complete LCD frame...")
    this.frameInfo = null; // {version, at} of the newest frame drawn
    this.frameCount = 0;
    this.queue = new ActionQueue({
      perform: (payload) => this.client.request('action', { request: payload }),
      before: (request) => {
        this.actionError = '';
        if (RESET_ACTIONS.has(request.action)) this.replay.clear();
      },
      onResult: (request, state) => {
        this.conditions.handleResult(request, state);
        this.showErrors();
      },
      onError: (request, error) => {
        this.actionError = (error && error.message) || 'The emulator action failed.';
        this.conditions.handleError(request);
        this.showErrors();
      },
    });
    this.conditionsDom = new ConditionsDom();
    this.conditions = new ConditionsController({
      view: this.conditionsDom,
      queue: this.queue,
      onActionError: (text) => {
        this.actionError = text;
        this.showErrors();
      },
    });
    this.replay = new ReplayController({
      setTimer: timers && timers.setTimer,
      clearTimer: timers && timers.clearTimer,
      enabled: () => byId('replay-pulses').checked,
      paused: () => document.hidden || !!this.connectionError,
      draw: (entry) => this.paintIndicator(entry),
    });
    this.wire();
  }

  // ---- wiring ----------------------------------------------------------------------------------

  wire() {
    for (const button of document.querySelectorAll('[data-action]')) {
      button.addEventListener('click', () => this.sendAction(button.dataset.action));
    }
    // Tapping the LCD presses a handset button: upper third Up, middle third Confirm, lower third Down.
    const frame = byId('frame');
    frame.addEventListener('click', (event) => {
      const bounds = frame.getBoundingClientRect();
      if (!bounds.height) return;
      const third = Math.floor((3 * (event.clientY - bounds.top)) / bounds.height);
      this.sendAction(third <= 0 ? 'up' : third === 1 ? 'confirm' : 'down');
    });
    byId('sensor-form').addEventListener('submit', (event) => {
      event.preventDefault();
      this.conditions.submitRaw();
    });
    for (const input of this.conditionsDom.fields) input.addEventListener('input', () => this.conditions.rawEdited(input.name));
    for (const id of BASIC_IDS) byId(id).addEventListener(id === 'water-type' ? 'change' : 'input', () => this.conditions.basicChanged());
    byId('can-form').addEventListener('submit', (event) => {
      event.preventDefault();
      this.sendAction('can', { dropId: Number(byId('can-form').elements.namedItem('dropId').value) });
    });
    byId('serial-form').addEventListener('submit', (event) => {
      event.preventDefault();
      this.sendAction('serial', { serialNumber: Number(byId('serial-form').elements.namedItem('serialNumber').value) });
    });
    byId('can-toggle').addEventListener('click', () => this.sendAction('can', { connected: !this.canConnected }));
    byId('advance-form').addEventListener('submit', (event) => {
      event.preventDefault();
      if (this.advancing) {
        this.client.send('cancel-advance');
        return;
      }
      this.sendAction('advance', { seconds: Number(byId('advance-seconds').value) });
    });
    byId('uart-channel').addEventListener('change', () => this.renderConsole());
    byId('uart-view').addEventListener('change', () => this.renderConsole());
    // The worker sends the UART bytes only while the console is on screen (its panel and Advanced are both open).
    const reportUart = () => this.client.send('ui', { uartOpen: this.uartVisible() });
    byId('uart-panel').addEventListener('toggle', reportUart);
    byId('advanced-panel').addEventListener('toggle', reportUart);
    byId('replay-pulses').addEventListener('change', () => {
      this.replay.clear();
      this.renderOutputs(this.state && this.state.hardwareOutputs);
    });

    const speed = byId('speed');
    speed.value = prefs.get('speed', '1');
    if (speed.selectedIndex < 0) speed.value = '1';
    speed.addEventListener('change', () => {
      prefs.set('speed', speed.value);
      this.applySpeed();
    });
    const background = byId('background-policy');
    background.value = prefs.get('background', 'run');
    if (background.selectedIndex < 0) background.value = 'run';
    background.addEventListener('change', () => {
      prefs.set('background', background.value);
      this.client.send('background', { policy: background.value });
    });
    const integer = byId('integer-scale');
    integer.checked = prefs.get('integer-scale', '1') === '1';
    this.lcd.setIntegerScaling(integer.checked);
    integer.addEventListener('change', () => {
      prefs.set('integer-scale', integer.checked ? '1' : '0');
      this.lcd.setIntegerScaling(integer.checked);
    });

    byId('evidence-save').addEventListener('click', () => this.sendAction('capture'));
    byId('profile-export').addEventListener('click', () => this.exportProfile());
    byId('profile-import').addEventListener('click', () => byId('profile-file').click());
    byId('profile-file').addEventListener('change', () => {
      const files = [...byId('profile-file').files];
      byId('profile-file').value = '';
      if (files.length) this.importProfile(files);
    });
    byId('profile-reset').addEventListener('click', () => this.resetProfile());
    byId('change-firmware').addEventListener('click', () => this.hooks.closeSession());
    byId('firmware-details').addEventListener('toggle', () => {
      if (byId('firmware-details').open && this.state) this.renderSessionInfo(this.state);
    });

    // Keyboard: arrows and Enter drive the handset buttons (a held key is one press, there is no auto-repeat).
    // keys.js decides which key presses belong to something else (fields, links, disclosure summaries, the UART
    // console, dialogs). The viewer ignored every key while a <button> had focus, so arrows did nothing after a
    // mouse click on any control; here a mouse click gives the focus back to the page instead.
    document.addEventListener('keydown', (event) => {
      if (this.root.hidden) return;
      const action = handsetKeyAction(event);
      if (action) {
        event.preventDefault();
        this.sendAction(action);
      }
    });
    document.addEventListener('click', (event) => {
      const control = event.target instanceof Element ? event.target.closest('button, summary') : null;
      if (control && event.detail > 0 && !control.closest('dialog')) setTimeout(() => control.blur(), 0);
    });
    document.addEventListener('visibilitychange', () => {
      // A pulse that happened while the page was hidden is never replayed on return (no stale catch-up).
      this.replay.clear();
      this.client.send('visibility', { hidden: document.hidden });
    });
    window.addEventListener('pagehide', () => {
      this.replay.clear();
      this.client.send('flush');
    });
  }

  uartVisible() {
    return byId('uart-panel').open && byId('advanced-panel').open;
  }

  /** Pushes the persisted UI preferences to a freshly started session. */
  applyPreferences() {
    this.applySpeed();
    this.client.send('background', { policy: byId('background-policy').value });
    this.client.send('visibility', { hidden: document.hidden });
    this.client.send('ui', { uartOpen: this.uartVisible() });
  }

  applySpeed() {
    const value = byId('speed').value;
    this.client.send('speed', { speed: value === 'max' ? null : Number(value) });
  }

  // ---- session lifecycle -----------------------------------------------------------------------

  /**
   * The session has started. The worker sends the first LCD frame while it creates the session, so it can reach
   * `onFrame` before this method runs (the boot reply is handled after the broadcasts that preceded it): frames
   * seen so far (`haveFrame`) are kept here and forgotten only when the session ends (`hide`). Resetting them here
   * left a first-session page without a picture until the next frame, which a steady screen or a hidden page may
   * not send for a long time.
   */
  show(info) {
    this.root.hidden = false;
    this.info = info;
    this.state = null;
    this.host = null;
    this.inputsEpoch = null;
    this.actionError = '';
    this.connectionError = '';
    this.advancing = false;
    this.replay.clear();
    if (!this.haveFrame) {
      this.lcd.setVisible(false, 'Waiting for LCD output');
      this.noteShowsFrame = false;
      byId('frame-note').textContent = 'No complete LCD frame is available yet.';
    }
    this.renderFirmwareInfo();
    this.applyPreferences();
  }

  hide() {
    this.root.hidden = true;
    this.haveFrame = false;
    this.frameCount = 0;
    this.frameInfo = null;
    this.noteShowsFrame = false;
    this.lcd.setVisible(false, 'Waiting for LCD output');
    this.replay.clear();
    document.title = 'NGC system emulator · WebAssembly';
  }

  /** The release this session runs: the engine's own report when it gives one, otherwise what the page verified. */
  currentRelease(state = this.state) {
    const reported = state && state.firmware && state.firmware.release;
    if (reported && typeof reported.id === 'string') return describeRelease(reported.id, reported.label);
    return (this.host && this.host.release) || (this.info && this.info.release) || null;
  }

  renderFirmwareInfo() {
    const info = this.info;
    const target = byId('firmware-info');
    target.replaceChildren();
    if (!info) return;
    const line = (label, slot) => (slot
      ? h('div', {}, h('strong', {}, label), ` ${slot.name} · ${formatBytes(slot.size)} · `, h('code', {}, `SHA-256 ${slot.report.srecSha256}`))
      : null);
    const options = info.options || {};
    const release = info.release;
    target.append(
      ...[ // append() would print a null as text
        release ? h('div', {}, h('strong', {}, 'Release'), ` ${release.label}`) : null,
        line('Main 5.8', options.mode === 'handset' ? null : info.slots.main),
        line('Handset 65.3', info.slots.handset),
      ].filter(Boolean),
      h('div', { class: 'small muted mt10' },
        `Start options: ${options.mode === 'handset' ? 'handset only' : 'dual (main + handset over CAN)'}, ${options.bootMode === 'cold' ? 'cold boot' : 'handset wake'}, ` +
        `${options.simultaneousStart ? 'simultaneous CPU start' : 'handset released by the inferred PE3 supply enable'}, board-ID ADC sample ${options.adcSample}, ` +
        `I2C idle-high fixture ${options.i2cIdleHigh === false ? 'off' : 'on'}, ` +
        `idle fast-forward ${options.idleFastForward === false ? 'off' : 'on'}${info.profile === 'none' ? ', saved profile not used' : ''}.`),
    );
  }

  // ---- incoming ---------------------------------------------------------------------------------

  onFrame(message) {
    const buffer = this.lcd.draw(message);
    this.haveFrame = true;
    this.frameCount += 1;
    this.frameInfo = { version: message.version, at: Date.now() };
    this.client.send('recycle', { buffer }, [buffer]);
    this.updateFrameVisibility();
    // While a frame is shown the note follows the frames at most once a second.
    if (this.noteShowsFrame && this.frameInfo.at - this.lastNote > 1000) this.writeFrameNote();
  }

  writeFrameNote() {
    this.lastNote = this.frameInfo.at;
    this.noteShowsFrame = true;
    byId('frame-note').textContent = `Last LCD frame: ${new Date(this.frameInfo.at).toLocaleTimeString()} · frame ${this.frameInfo.version}`;
  }

  /**
   * The canvas shows when the panel is on and a frame is in hand, otherwise the placeholder. The note under the LCD
   * says "No complete LCD frame" only while that is true: it is rewritten the moment the picture appears, because a
   * steady screen sends no further frame that would refresh it.
   */
  updateFrameVisibility() {
    const ready = !!(this.state && this.state.frameReady);
    if (ready && this.haveFrame) {
      this.lcd.setVisible(true);
      if (!this.noteShowsFrame && this.frameInfo) this.writeFrameNote();
    } else {
      this.lcd.setVisible(false, ready ? 'Waiting for LCD snapshot' : 'Waiting for LCD output');
      if (!ready) {
        this.noteShowsFrame = false;
        byId('frame-note').textContent = 'No complete LCD frame is available yet.';
      }
    }
  }

  onState(message) {
    this.state = message.state;
    this.host = message.host;
    this.connectionError = '';
    this.render();
  }

  addNotice(level, text) {
    this.notices.push({ level, text });
    if (this.notices.length > 5) this.notices.shift();
    this.showErrors();
  }

  setConnectionError(text) {
    this.connectionError = text;
    byId('status').classList.remove('running');
    byId('status').classList.add('bad');
    byId('status-text').textContent = 'Disconnected';
    this.clearHardwareStatus();
    this.showErrors();
  }

  // ---- hardware outputs: status strip, raw rows, replay ------------------------------------------

  makeOutputRow(output) {
    const row = h('div', { class: 'output-row' });
    const indicator = h('span', { class: 'output-light unknown', 'aria-hidden': 'true' });
    const name = h('div', { class: 'output-name' });
    const status = h('div', { class: 'output-state' });
    const detail = h('div', { class: 'output-detail mono' });
    const replay = h('div', { class: 'output-replay' });
    const activity = h('div', { class: 'output-activity' });
    const historyBody = h('pre', {});
    const history = h('details', { class: 'output-history' }, h('summary', {}, 'Recent commands · virtual time'), historyBody);
    row.append(indicator, h('div', {}, name, status, detail, replay, activity, history));
    let color = null;
    if (output.kind === 'led') {
      color = h('select', { onchange: () => { if (!this.connectionError) this.sendAction('led-colors', { colors: { [output.id]: color.value } }); } },
        [['unknown', 'Unknown'], ['red', 'Red'], ['white', 'White']].map(([value, text]) => h('option', { value }, text)));
      row.append(h('label', { class: 'output-color' }, h('span', {}, 'LED color'), color));
    }
    return { row, indicator, name, status, detail, replay, activity, history, historyBody, color, kind: output.kind };
  }

  /** Repaints the indicators of one output: its raw row and, for the three alert outputs, the status strip. */
  paintIndicator(entry) {
    const view = describeEntry(entry, byId('replay-pulses').checked);
    const row = this.outputRows.get(entry.id);
    if (row) {
      row.indicator.className = view.indicatorClass;
      row.indicator.title = view.title;
      row.replay.hidden = view.replayHidden;
      row.replay.textContent = view.replayText;
    }
    if (view.simple) {
      const light = byId(`simple-${entry.id}-light`);
      const state = byId(`simple-${entry.id}-state`);
      if (light && state) {
        light.className = view.simple.lightClass;
        light.title = view.title;
        state.textContent = view.simple.stateText;
      }
    }
  }

  /** Status strip entries without a live output show an unknown drive (or the lost connection). */
  resetStatusStrip(ids, text) {
    for (const id of ids) {
      const strip = STATUS_STRIP[id];
      const light = byId(`simple-${id}-light`);
      const state = byId(`simple-${id}-state`);
      if (light) light.className = ['output-light', 'unknown', strip.color, strip.vibrator ? 'vibrator' : ''].filter(Boolean).join(' ');
      if (state) state.textContent = text;
    }
  }

  renderOutputs(outputs) {
    const state = this.state || {};
    // A new `outputHistoryEpoch` (session creation, board recreation) or host generation starts the histories over.
    const epoch = `${state.outputHistoryEpoch === undefined ? 'legacy' : state.outputHistoryEpoch}:${(this.host && this.host.generation) || 0}`;
    const { entries, removed } = this.replay.update(outputs, { virtualTime: state.virtualTime, epoch });
    for (const entry of removed) {
      const row = this.outputRows.get(entry.id);
      if (row) row.row.remove();
      this.outputRows.delete(entry.id);
    }
    const shown = new Set(entries.map((entry) => entry.id));
    this.resetStatusStrip(Object.keys(STATUS_STRIP).filter((id) => !shown.has(id)), this.connectionError ? 'Disconnected' : 'Unknown');
    for (const entry of entries) {
      const output = entry.output;
      let row = this.outputRows.get(entry.id);
      if (!row) {
        row = this.makeOutputRow(output);
        this.outputRows.set(entry.id, row);
        byId('output-list').append(row.row);
      }
      this.paintIndicator(entry);
      row.name.textContent = [output.label || output.id, output.board].filter(Boolean).join(' · ');
      row.status.textContent = driveText(output);
      row.detail.textContent = [output.pin, output.details].filter(Boolean).join(' · ');
      const activity = activityText(output);
      row.activity.hidden = row.history.hidden = !activity;
      if (activity) {
        row.activity.textContent = activity;
        row.historyBody.textContent = historyText(output);
      }
      if (row.color) {
        const color = ['red', 'white'].includes(output.color) ? output.color : 'unknown';
        row.color.setAttribute('aria-label', `Color for ${output.label || output.id}`);
        if (document.activeElement !== row.color) row.color.value = color;
        row.color.disabled = !!this.connectionError;
      }
    }
    byId('outputs-status').textContent = entries.length ? '' : 'No hardware output status is available.';
    byId('outputs-status').hidden = entries.length > 0;
  }

  renderConsole() {
    const channel = this.uartChannels.get(byId('uart-channel').value);
    if (!channel) {
      byId('uart-status').textContent = this.connectionError ? 'Disconnected; UART status is unavailable.' : 'No UART channels are available.';
      byId('uart-output').textContent = 'No live UART output available.';
      return;
    }
    const bytes = typeof channel.txBytes === 'number' && Number.isFinite(channel.txBytes) ? channel.txBytes : 0;
    const time = typeof channel.lastTxVirtualTime === 'number' && Number.isFinite(channel.lastTxVirtualTime) ? `Last TX at ${channel.lastTxVirtualTime.toFixed(3)} virtual s` : 'No TX timestamp';
    byId('uart-status').textContent = [`${bytes} transmitted bytes`, time, channel.truncated ? 'Showing retained tail; earlier bytes omitted' : ''].filter(Boolean).join(' · ');
    if (!this.uartVisible()) return; // the worker only sends the bytes while the console is on screen
    const view = byId('uart-view').value === 'hex' ? 'hex' : 'text';
    const content = typeof channel[view] === 'string' ? channel[view] : '';
    const pre = byId('uart-output');
    const followTail = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
    const shown = content || (bytes ? 'Loading…' : 'No transmitted bytes captured yet.');
    if (pre.textContent !== shown) {
      pre.textContent = shown;
      if (followTail) pre.scrollTop = pre.scrollHeight;
    }
  }

  renderUart(channels) {
    const list = Array.isArray(channels) ? channels.filter((channel) => channel && typeof channel.id === 'string') : [];
    this.uartChannels = new Map(list.map((channel) => [channel.id, channel]));
    const select = byId('uart-channel');
    const selected = select.value;
    const optionIds = [...select.options].map((option) => option.value);
    const nextIds = list.map((channel) => channel.id);
    if (optionIds.length !== nextIds.length || optionIds.some((id, index) => id !== nextIds[index])) {
      select.replaceChildren(...list.map((channel) => h('option', { value: channel.id })));
    }
    for (const [index, channel] of list.entries()) select.options[index].textContent = [channel.board, channel.label || channel.peripheral || channel.id].filter(Boolean).join(' · ');
    if (!list.length) {
      select.replaceChildren(h('option', { value: '' }, this.connectionError ? 'Disconnected' : 'No channels'));
    } else {
      select.value = this.uartChannels.has(selected) ? selected : list[0].id;
    }
    select.disabled = !list.length || !!this.connectionError;
    this.renderConsole();
  }

  clearHardwareStatus() {
    this.renderOutputs([]);
    byId('outputs-status').textContent = 'Disconnected; output state is unavailable.';
    this.renderUart([]);
  }

  showErrors() {
    const messages = [this.connectionError, this.actionError, this.state && this.state.error, ...this.notices.map((notice) => notice.text)].filter(Boolean).map(String);
    const box = byId('error');
    box.replaceChildren();
    box.hidden = messages.length === 0;
    if (!messages.length) return;
    const worst = this.notices.some((notice) => notice.level === 'error') || this.connectionError || this.actionError || (this.state && this.state.error);
    box.classList.toggle('warning', !worst);
    box.append(
      h('button', { type: 'button', class: 'link dismiss', onclick: () => { this.notices = []; this.actionError = ''; this.showErrors(); } }, 'Dismiss'),
      messages.join('\n'),
    );
  }

  // ---- status and state ------------------------------------------------------------------------

  renderStatus() {
    const state = this.state;
    const host = this.host || {};
    const badge = byId('status');
    let text;
    if (state.standby) text = 'Standby';
    else if (host.suspended) text = 'Suspended';
    else if (state.running) text = 'Running';
    else text = 'Paused';
    badge.classList.remove('bad', 'warn');
    badge.classList.toggle('running', !!state.running && !host.suspended);
    if (state.error) badge.classList.add('bad');
    byId('status-text').textContent = text;

    const rt = byId('rt-badge');
    rt.hidden = false;
    rt.classList.remove('running', 'warn', 'idle');
    let rtText;
    if (!state.running) {
      rtText = host.suspended ? 'suspended (background tab)' : 'not running';
      rt.classList.add('idle');
    } else if (host.suspended) {
      rtText = 'suspended (background tab)';
      rt.classList.add('idle');
    } else if (typeof host.realtimeFactor !== 'number' || !Number.isFinite(host.realtimeFactor) || host.realtimeFactor === 0) {
      rtText = 'measuring…';
      rt.classList.add('idle');
    } else {
      const factor = host.realtimeFactor;
      const target = host.speed === null ? 'unpaced' : `${host.speed}× target`;
      rtText = `${factor.toFixed(factor >= 10 ? 1 : 2)}× real time`;
      if (host.speed === null) rtText += ' · unpaced';
      else if (!host.keepingUp) {
        rtText += ' · not keeping up';
        rt.classList.add('warn');
      } else {
        rt.classList.add('running');
      }
      byId('rt-factor').title = `Target ${target}`;
    }
    byId('rt-text').textContent = rtText;
    const capacity = typeof host.capacityFactor === 'number' && Number.isFinite(host.capacityFactor) ? `engine capacity ${host.capacityFactor.toFixed(1)}×` : '';
    const lag = host.lagMs > 20 ? `behind by ${host.lagMs.toFixed(0)} ms` : '';
    const dropped = host.droppedSeconds > 0.05 ? `${host.droppedSeconds.toFixed(1)} s of backlog skipped in the last minute` : '';
    byId('rt-factor').textContent = state.running && !host.suspended ? [rtText.replace(' · unpaced', ''), capacity, lag, dropped].filter(Boolean).join(' · ') : (host.suspended ? 'suspended while this tab is in the background' : '—');
  }

  /** The reasons the engine gives for fields it cannot report for this firmware release. */
  renderUnavailable(unavailable) {
    const labels = { mainBatteryReady: 'Main batteries ready' };
    const entries = Object.entries(unavailable).filter(([, reason]) => typeof reason === 'string' && reason);
    const box = byId('unavailable-info');
    box.hidden = entries.length === 0;
    box.textContent = entries.map(([field, reason]) => `${labels[field] || field}: not reported for this release (${reason}).`).join(' ');
  }

  /** Header and window title: the release the session runs. */
  renderTitles(next) {
    const release = this.currentRelease(next);
    const dual = !!next.inputs;
    const product = dual ? 'NGC system emulator' : 'NGC handset emulator';
    byId('title').textContent = product;
    const name = release ? release.name : 'TRITON';
    byId('subtitle').textContent = dual
      ? `${name} main 5.8 + handset 65.3 · live LCD output · WebAssembly functional model`
      : `${name} handset 65.3 · live LCD output · WebAssembly functional model`;
    document.title = `${product} · ${name} · WebAssembly`;
  }

  render() {
    const next = this.state;
    const host = this.host || {};
    this.renderStatus();
    this.renderTitles(next);
    byId('virtual-time').textContent = typeof next.virtualTime === 'number' ? `${next.virtualTime.toFixed(3)} s` : String(next.virtualTime ?? '—');
    byId('pc').textContent = typeof next.pc === 'number' ? hex32(next.pc) : String(next.pc ?? '—');
    byId('main-pc').textContent = typeof next.mainPC === 'number' ? hex32(next.mainPC) : '—';
    // Fields that depend on a firmware-specific address are reported as null with a reason for releases where no
    // equivalent is proven (never a value taken over from another release).
    const unavailable = next.unavailable && typeof next.unavailable === 'object' ? next.unavailable : {};
    byId('battery-ready').textContent = typeof next.mainBatteryReady === 'boolean' ? (next.mainBatteryReady ? 'Yes' : 'Waiting') : (unavailable.mainBatteryReady ? 'Unavailable for this release' : '—');
    this.renderUnavailable(unavailable);
    byId('sensor-panel').hidden = byId('basic-sensor-panel').hidden = !next.inputs;
    // The input fields take the engine's values when a session starts and whenever the profile is replaced (boot,
    // import, reset); otherwise they keep what the user typed (the viewer loaded them once). Bases and offsets of
    // the basic controls are derived from the raw readings at the same moments.
    if (next.inputs && this.inputsEpoch !== host.profileEpoch) {
      this.conditions.attach(next.inputs);
      byId('serial-form').elements.namedItem('serialNumber').value = next.serialNumber ?? 0;
      this.inputsEpoch = host.profileEpoch;
    }
    byId('storage-summary').textContent = [next.storageSummary, next.flashSummary].filter(Boolean).join(' · ');
    byId('can-summary').textContent = next.canSummary || '';
    this.canConnected = !String(next.canSummary).includes('connected=False');
    byId('can-toggle').textContent = this.canConnected ? 'Disconnect CAN' : 'Connect CAN';
    byId('capture-path').textContent = host.lastCapture ? `Saved: ${host.lastCapture} (downloaded)` : '';
    byId('mode').textContent = [String(next.mode ?? '—'), next.bootMode ? `Boot: ${next.bootMode}` : '', next.powerModel || '', next.performanceMode || ''].filter(Boolean).join(' · ');
    byId('lcd-summary').textContent = typeof next.lcdSummary === 'object' ? JSON.stringify(next.lcdSummary, null, 2) : String(next.lcdSummary ?? 'No LCD status available.');
    byId('footer-engine').textContent = next.engine || host.engine || '—';
    this.renderOutputs(next.hardwareOutputs);
    this.renderUart(next.uartConsole);
    const toggle = byId('run-toggle');
    toggle.dataset.action = next.running || host.suspended ? 'pause' : 'resume';
    toggle.textContent = next.running || host.suspended ? 'Pause' : 'Resume';
    this.renderAdvance(host.advance);
    if (byId('firmware-details').open) this.renderSessionInfo(next);
    this.renderProfileStatus();
    this.updateFrameVisibility();
    this.showErrors();
  }

  /** Release, fixtures, clock persistence provenance, executed instructions with the idle fast-forward share, machine resets. */
  renderSessionInfo(state) {
    const lines = [];
    const board = (name) => String(name).replace(/^ngc-/, '');
    const release = this.currentRelease(state);
    if (release) lines.push(`Firmware release: ${release.label} (${release.id}).`);
    const options = (this.info && this.info.options) || {};
    if (state.inputs) {
      // The engine names the fixture in the state (`i2cIdleHigh`); an older build does not, and ignores the option.
      const unsupported = ((this.host && this.host.unsupportedOptions) || []).includes('i2cIdleHigh');
      const high = typeof state.i2cIdleHigh === 'boolean' ? state.i2cIdleHigh : options.i2cIdleHigh !== false;
      if (unsupported && typeof state.i2cIdleHigh !== 'boolean') lines.push('Main I2C idle lines: this engine build has no idle-high fixture option; the lines keep the engine default.');
      else if (high && typeof state.i2cFixture === 'string') lines.push(`Fixture: ${state.i2cFixture}.`);
      else lines.push(`Main I2C idle lines PB6/PB7/PB10/PB11: ${high ? 'driven high before the firmware runs' : 'left at their default (low)'}. This is an idle-line fixture, not electrical I2C modelling.`);
    }
    const rtc = state.rtcPersistence;
    if (rtc) {
      const sources = Object.entries(rtc.sources || {}).map(([name, source]) => `${board(name)}: ${source && source.source}`).join(', ');
      const restored = (rtc.restoredBoards || []).length ? `restored ${rtc.restoredBoards.map(board).join(' + ')}` : 'no saved checkpoint (fresh RTC)';
      lines.push(`Clock (${rtc.policy}, ${rtc.precision}): ${restored}${sources ? ` [${sources}]` : ''}${rtc.mainBkp1WakeOverride ? '; main RTC.BKP1R wake override applied' : ''}.`);
    }
    if (state.instructions) {
      const parts = Object.entries(state.instructions).map(([name, count]) => {
        const skipped = state.idleSkip && state.idleSkip[name] ? state.idleSkip[name].skippedInstructions : 0;
        const share = count > 0 ? ` (${((100 * skipped) / count).toFixed(1)}% skipped by the idle fast-forward)` : '';
        return `${name} ${Number(count).toLocaleString()}${share}`;
      });
      lines.push(`Executed instructions since the last start: ${parts.join('; ')}. Idle fast-forward ${state.idleFastForward ? 'on' : 'off'}.`);
    }
    if (Array.isArray(state.machineResets) && state.machineResets.length) {
      lines.push(`Machine resets: ${state.machineResets.map((event) => `${event.board} (${event.cause}) at ${Number(event.appliedAt).toFixed(3)} s`).join('; ')}.`);
    }
    byId('session-info').replaceChildren(...lines.map((line) => h('div', {}, line)));
  }

  renderAdvance(progress) {
    const busy = !!progress;
    this.advancing = busy;
    const button = byId('advance-button');
    button.textContent = busy ? 'Cancel' : 'Advance';
    byId('advance-progress').hidden = !busy;
    if (busy) byId('advance-progress').firstElementChild.style.width = `${Math.min(100, (100 * progress.done) / progress.total)}%`;
    for (const control of document.querySelectorAll('#run-toggle, [data-action="step"], [data-action="reset"], [data-action="cold"], [data-action="wake"], #serial-form button, #advance-seconds')) {
      control.disabled = busy;
    }
  }

  renderProfileStatus() {
    const storage = (this.host && this.host.storage) || {};
    const release = this.currentRelease();
    const parts = [];
    if (release) parts.push(`${release.name} profile`);
    if (storage.kind === 'opfs') parts.push('stored in this browser (origin-private file system)');
    else if (storage.kind === 'indexeddb') parts.push('stored in this browser (IndexedDB)');
    else parts.push('this browser offers no persistent storage here: the profile is kept in memory only');
    parts.push(storage.saving ? 'saving automatically' : 'saving is off for this session');
    if (storage.lastSave) parts.push(`last saved ${formatClock(storage.lastSave)}`);
    if (storage.error) parts.push(`last save failed: ${storage.error}`);
    const text = parts.join(' · ');
    byId('profile-status').textContent = `${text.charAt(0).toUpperCase()}${text.slice(1)}.`;
  }

  // ---- actions ---------------------------------------------------------------------------------

  /** Queues a UI action (see conditions.js: one at a time, in order). Resolves to whether it was applied. */
  sendAction(action, extra = {}) {
    return this.queue.send(action, extra);
  }

  async exportProfile() {
    this.actionError = '';
    try {
      await this.client.request('export-profile');
    } catch (error) {
      this.actionError = error.message;
    }
    this.showErrors();
  }

  async importProfile(files) {
    this.actionError = '';
    try {
      const entries = [];
      for (const file of files) {
        const bytes = new Uint8Array(await file.arrayBuffer());
        const isZip = bytes.length > 4 && bytes[0] === 0x50 && bytes[1] === 0x4b;
        if (isZip) {
          for (const entry of await readZip(bytes)) entries.push({ name: entry.name, data: entry.data });
        } else {
          entries.push({ name: file.name, data: bytes });
        }
      }
      const known = entries.filter((entry) => PROFILE_FILES.includes(entry.name.split('/').pop()));
      if (!known.length) throw new Error(`No profile files found (expected ${PROFILE_FILES.join(', ')}).`);
      const release = this.currentRelease();
      const ok = await confirmDialog({
        title: 'Import this profile?',
        message: `The emulator restarts with the imported ${known.map((entry) => entry.name.split('/').pop()).join(', ')} and replaces the stored ${release ? `${release.name} ` : ''}profile.`,
        confirm: 'Import and restart',
        danger: true,
      });
      if (!ok) return;
      const copies = known.map((entry) => ({ name: entry.name.split('/').pop(), data: entry.data.slice().buffer }));
      await this.client.request('import-profile', { files: copies.map((file) => ({ name: file.name, data: new Uint8Array(file.data) })) }, copies.map((file) => file.data));
    } catch (error) {
      this.actionError = error.message || String(error);
    }
    this.showErrors();
  }

  async resetProfile() {
    const release = this.currentRelease();
    const ok = await confirmDialog({
      title: 'Reset the profile?',
      message: `This erases the emulated EEPROM, log flash, clock checkpoint, sensor inputs and LED colour labels of the ${release ? `${release.name} ` : ''}profile (in this browser) and restarts the boards from factory-fresh storage. Export the profile first if you may need it again.`,
      confirm: 'Erase profile',
      danger: true,
    });
    if (!ok) return;
    this.actionError = '';
    try {
      await this.client.request('reset-profile');
    } catch (error) {
      this.actionError = error.message;
    }
    this.showErrors();
  }
}
