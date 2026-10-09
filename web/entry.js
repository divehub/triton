// Entry screen: asks for the two original SREC files of one firmware release (TRITON or NEPTUN), has the worker
// verify them, and boots the session.

import { FIRMWARE_PROXY_URL } from './config.js';
import { parseSurfacePressure } from './deco.js';
import { byId, confirmDialog, formatBytes, formatClock, h, prefs } from './dom.js';
import { FETCH_TIMEOUT_MS, fetchFirmware, normalizeFirmwareUrl, proxyEndpoint } from './firmware-url.js';
import { RELEASES, RELEASE_IDS } from './releases.js';

const ROLE_LABEL = { main: 'main controller 5.8', handset: 'handset 65.3' };
const ROLES = ['main', 'handset'];
const PREFILL_LIMIT = 2048;
// Archived copies of the TRITON main 5.8 / handset 65.3 SREC files (fetched only through the proxy, on Load).
const DEFAULT_URLS = {
  main: 'https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/pvlL3Iilv4o_Tu5lggngZAUt.srec',
  handset: 'https://web.archive.org/web/20261008041427/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec',
};

export class EntryView {
  /**
   * @param {import('./worker-client.js').WorkerClient} client
   * @param {{booted: (result: object, info: object) => void}} hooks
   * @param {object} [deps] injected by the tests: `fetch`, `timers` ({setTimer, clearTimer}), `timeoutMs`, `proxy`
   *   (the result of `proxyEndpoint`) or `proxyUrl` (the configured FIRMWARE_PROXY_URL, default: config.js)
   */
  constructor(client, hooks, deps = {}) {
    this.client = client;
    this.hooks = hooks;
    this.deps = {
      fetch: deps.fetch || ((...args) => globalThis.fetch(...args)),
      timers: deps.timers || { setTimer: (fn, ms) => setTimeout(fn, ms), clearTimer: (id) => clearTimeout(id) },
      timeoutMs: deps.timeoutMs ?? FETCH_TIMEOUT_MS,
    };
    this.proxy = deps.proxy || proxyEndpoint(window.location, window.location.search || '', deps.proxyUrl !== undefined ? deps.proxyUrl : FIRMWARE_PROXY_URL);
    this.slots = { main: null, handset: null };
    this.shownSlots = { main: undefined, handset: undefined }; // what each card was last rebuilt for (renderSlot)
    this.rejected = [];
    this.busy = 0;
    this.booting = false;
    this.init = null;
    this.remembered = null; // the firmware pair kept in this browser, by role (the worker's `rememberedInfo`), or null
    this.urlLoading = null; // {controller, timer, canceled, timedOut, phase} while "Load from URLs" runs
    this.urlResult = { main: null, handset: null }; // the last outcome per field: {kind: 'ok'|'bad'|'busy', text}
    this.root = byId('screen-entry');
    this.slotEls = { main: byId('slot-main'), handset: byId('slot-handset') };
    this.dropzone = byId('dropzone');
    this.input = byId('file-input');
    this.bootButton = byId('boot');
    this.bootHint = byId('boot-hint');
    this.remember = byId('remember');
    this.problem = byId('profile-problem');
    this.note = byId('profile-note');
    this.rememberedBanner = byId('remembered-banner');
    this.urlForm = byId('url-form');
    this.urlEls = { main: byId('url-main'), handset: byId('url-handset') };
    this.urlInfoEls = { main: byId('url-main-info'), handset: byId('url-handset-info') };
    this.urlProgressEls = { main: byId('url-main-progress'), handset: byId('url-handset-progress') };
    this.urlLoad = byId('url-load');
    this.urlCancel = byId('url-cancel');
    this.urlStatus = byId('url-status');
    this.wire();
    this.prefillUrls();
    this.renderProxyNote();
    this.renderUrls();
  }

  /**
   * The fields start with the archived TRITON 5.8 / 65.3 addresses; `?main-url=` and `?handset-url=` replace them.
   * Nothing is downloaded until the Load button is pressed.
   */
  prefillUrls() {
    const query = new URLSearchParams(window.location.search || '');
    for (const role of ROLES) {
      const value = query.get(`${role}-url`);
      this.urlEls[role].value = value ? value.slice(0, PREFILL_LIMIT) : DEFAULT_URLS[role];
    }
  }

  wire() {
    byId('choose-files').addEventListener('click', (event) => {
      event.stopPropagation();
      this.input.click();
    });
    // The drop zone is a target for the mouse and for drops; its "choose files…" button is the one keyboard stop.
    this.dropzone.addEventListener('click', () => this.input.click());
    this.input.addEventListener('change', () => {
      const files = [...this.input.files];
      this.input.value = '';
      this.addFiles(files);
    });
    const over = (event) => {
      if (this.root.hidden) return;
      event.preventDefault();
      // A drag over the page shows the drop zone, even when the sources are folded away.
      if (event.type === 'dragenter') byId('firmware-sources').open = true;
      this.dropzone.classList.add('over');
    };
    document.addEventListener('dragenter', over);
    document.addEventListener('dragover', over);
    document.addEventListener('dragleave', (event) => {
      if (event.target === this.dropzone || event.target === document.documentElement) this.dropzone.classList.remove('over');
    });
    document.addEventListener('drop', (event) => {
      if (this.root.hidden) return;
      event.preventDefault();
      this.dropzone.classList.remove('over');
      if (event.dataTransfer && event.dataTransfer.files.length) this.addFiles([...event.dataTransfer.files]);
    });
    for (const id of ['start-mode', 'start-boot-mode', 'start-adc', 'start-i2c-idle', 'start-surface', 'start-simultaneous', 'start-paused', 'start-idle-ff']) {
      byId(id).addEventListener('change', () => this.refresh());
    }
    this.bootButton.addEventListener('click', () => this.boot('stored'));
    byId('use-remembered').addEventListener('click', () => this.useRemembered());
    byId('forget-remembered').addEventListener('click', () => this.forget());
    for (const role of ROLES) {
      this.urlEls[role].addEventListener('input', () => {
        if (!this.urlLoading) this.urlResult[role] = null; // an edit makes the earlier outcome stale
        this.renderUrls();
      });
    }
    this.urlForm.addEventListener('submit', (event) => {
      event.preventDefault();
      this.loadUrls();
    });
    this.urlCancel.addEventListener('click', () => this.cancelUrls());
  }

  /** The release of the files provided so far (they always agree), or null. */
  release() {
    const slot = this.slots.main || this.slots.handset;
    return (slot && slot.release) || null;
  }

  /** Shows the screen. `init` is the worker's reply to `init` (or `info`). */
  show(init) {
    this.root.hidden = false;
    this.init = init;
    byId('engine-label').textContent = init.engine;
    // Firmware is remembered in the origin-private file system only (the profile may fall back to IndexedDB).
    const canRemember = init.storage.kind === 'opfs';
    this.remember.disabled = !canRemember;
    if (!canRemember) this.remember.checked = false;
    byId('remember-note').hidden = canRemember;
    this.renderProfileNote();
    this.renderRemembered(init.remembered);
    this.problem.hidden = true;
    this.booting = false;
    this.refresh();
    // Verified files from an earlier visit are used automatically (they are checked again).
    if (init.remembered && !this.slots.main && !this.slots.handset) this.useRemembered();
    const dev = new URLSearchParams(window.location.search);
    if (dev.has('dev-firmware') && !this.slots.main && !this.slots.handset) this.loadDevFirmware(dev.get('dev-firmware'));
  }

  /**
   * Development aid for automated browser checks: with `serve.py --dev-firmware DIR` and the page opened as
   * `/?dev-firmware` (TRITON) or `/?dev-firmware=neptun`, the two files are fetched from the local server and
   * handled exactly like chosen files.
   */
  async loadDevFirmware(which) {
    const wanted = String(which || '').toLowerCase();
    const release = Object.values(RELEASES).find((candidate) => candidate.name.toLowerCase() === wanted) || RELEASES[RELEASE_IDS[0]];
    // `?dev-firmware=mixed` offers a TRITON handset with a NEPTUN main file, to check that a mixed pair is refused.
    const names = wanted === 'mixed' ? [RELEASES[RELEASE_IDS[0]].files.handset, RELEASES[RELEASE_IDS[1]].files.main] : [release.files.handset, release.files.main];
    const files = [];
    for (const name of names) {
      try {
        const response = await fetch(`/dev-firmware/${name}`, { cache: 'no-store' });
        if (!response.ok) throw new Error('route not enabled (start serve.py with --dev-firmware DIR)');
        files.push(new File([await response.arrayBuffer()], name));
      } catch (error) {
        this.rejected.push({ name, message: `Development firmware route: ${error.message}` });
      }
    }
    await this.addFiles(files);
  }

  hide() {
    this.root.hidden = true;
  }

  /** The remembered pair as the worker reported it (or null): the banner follows it, and the sources fold away. */
  renderRemembered(remembered) {
    this.remembered = remembered || null;
    // With firmware in the cache, the drop zone and URL loader fold away (they stay one click away).
    byId('firmware-sources').open = !remembered;
    this.renderRememberedBanner();
  }

  /** True while every remembered file is in its slot (the entry screen takes them automatically, after verifying them again). */
  rememberedInUse() {
    const roles = Object.keys(this.remembered || {});
    return roles.length > 0 && roles.every((role) => this.slots[role] && this.slots[role].remembered);
  }

  /**
   * The banner about the files kept in this browser. While they are in the slots it only says so and offers Forget; "Use the
   * remembered files" shows when a slot was removed or filled by another file.
   */
  renderRememberedBanner() {
    const remembered = this.remembered;
    this.rememberedBanner.hidden = !remembered;
    if (!remembered) return;
    const releases = [...new Set(Object.values(remembered).map((file) => file.release && file.release.name).filter(Boolean))];
    const kind = releases.length ? `${releases.join(', ')} ` : '';
    const inUse = this.rememberedInUse();
    byId('use-remembered').hidden = inUse;
    byId('remembered-text').textContent = inUse
      ? `Using the verified ${kind}firmware files remembered from an earlier visit.`
      : `Verified ${kind}firmware files from an earlier visit are stored in this browser: ${Object.values(remembered).map((file) => `${file.name} (${formatBytes(file.size)})`).join(', ')}.`;
  }

  /** The sources (drop zone, URLs) are needed only while a file is missing: open then, folded once both slots are filled. */
  syncSources() {
    byId('firmware-sources').open = !(this.slots.main && this.slots.handset);
  }

  /** The saved profile of the release in use; before any file is provided, of every release that has one. */
  renderProfileNote() {
    this.note.replaceChildren();
    const profiles = (this.init && this.init.profiles) || {};
    const selected = this.release();
    const ids = selected ? [selected.id] : RELEASE_IDS;
    const shown = ids.filter((id) => profiles[id]);
    this.note.hidden = shown.length === 0;
    for (const id of shown) {
      const profile = profiles[id];
      const name = (RELEASES[id] && RELEASES[id].name) || id;
      this.note.append(h('div', {},
        h('span', {}, `A saved ${name} profile (${profile.files.map((file) => file.name).join(', ')}; updated ${formatClock(profile.modified)}) is restored when you boot ${name} firmware. `),
        h('button', { type: 'button', class: 'link', onclick: () => this.resetProfile(id) }, `Reset the saved ${name} profile…`)));
    }
  }

  async resetProfile(releaseId) {
    const name = (RELEASES[releaseId] && RELEASES[releaseId].name) || releaseId;
    const ok = await confirmDialog({
      title: `Reset the saved ${name} profile?`,
      message: `This erases the emulated EEPROM, log flash, clock checkpoint, sensor inputs and LED color labels of the ${name} profile stored in this browser. Export the profile first if you may need it again.`,
      confirm: 'Erase profile',
      danger: true,
    });
    if (!ok) return false;
    try {
      await this.client.request('reset-profile', { release: releaseId });
      if (this.init && this.init.profiles) this.init.profiles[releaseId] = null;
      this.renderProfileNote();
      this.problem.hidden = true;
      return true;
    } catch (error) {
      this.showProblem(error.message);
      return false;
    }
  }

  async addFiles(files) {
    for (const file of files) await this.inspectBytes(file.name, file.size, () => file.arrayBuffer());
    this.problem.hidden = true;
    this.refresh();
    this.syncSources();
  }

  /**
   * The one verification path for every firmware file, chosen or fetched: the worker inspects the bytes (SHA-256,
   * content, role, release, mixed-pair rule) and an accepted file takes the slot of the role it turned out to be.
   * `source` (an address) is only for display. Resolves to the worker's result, or a refusal-shaped object.
   */
  async inspectBytes(name, size, getBytes, source = null) {
    this.busy += 1;
    this.refresh();
    let outcome;
    try {
      const bytes = await getBytes();
      const result = await this.client.request('inspect', { name, bytes }, [bytes]);
      if (result.accepted) {
        this.slots[result.role] = source ? { ...result, source } : result;
        this.rejected = this.rejected.filter((item) => item.name !== name);
      } else {
        this.rejected.push({ name, size, message: result.message || 'Not a supported firmware image.', report: result.report, source });
      }
      outcome = result;
    } catch (error) {
      const message = `Could not check this file: ${error.message}`;
      this.rejected.push({ name, size, message, source });
      outcome = { accepted: false, message };
    }
    this.busy -= 1;
    return outcome;
  }

  // ---- load from URLs ------------------------------------------------------------------------------------

  /** The fields as parsed addresses: [{role, text, parsed}] for the non-empty ones. */
  urlJobs() {
    return ROLES.map((role) => ({ role, text: this.urlEls[role].value.trim() }))
      .filter((job) => job.text !== '')
      .map((job) => ({ ...job, parsed: normalizeFirmwareUrl(job.text) }));
  }

  /** Without a firmware proxy (none configured at build time) the address fields are hidden and the note says so. */
  get urlsAvailable() {
    return this.proxy.kind !== 'none';
  }

  renderProxyNote() {
    const warning = this.proxy.warning ? ` ${this.proxy.warning}` : '';
    for (const id of ['url-intro', 'url-fields', 'url-actions']) byId(id).hidden = !this.urlsAvailable;
    if (!this.urlsAvailable) {
      byId('url-proxy-note').textContent = `Loading from URLs is not configured yet: no firmware proxy address was set when this site was built.${warning} Choose or drop your two firmware files above instead.`;
      return;
    }
    const where = {
      configured: `The firmware proxy at ${this.proxy.endpoint}`,
      dev: `The local development proxy at ${this.proxy.endpoint}`,
    }[this.proxy.kind] || this.proxy.endpoint;
    byId('url-proxy-note').textContent = `${where} fetches the one file you name, on demand, and returns it only to this page; it keeps nothing and caches nothing. It sees the address you enter. The file is then verified here like any chosen file.${warning}`;
  }

  /** The state of the URL form: per-field validation or outcome, and whether Load can be pressed. */
  renderUrls() {
    const jobs = this.urlJobs();
    for (const role of ROLES) {
      const info = this.urlInfoEls[role];
      const result = this.urlResult[role];
      const job = jobs.find((item) => item.role === role);
      let text = '';
      let kind = '';
      if (result) {
        ({ text, kind } = result);
      } else if (job) {
        text = job.parsed.ok ? `${job.parsed.changed ? 'Will be fetched as' : 'Will be fetched'}: ${job.parsed.url}` : job.parsed.message;
        kind = job.parsed.ok ? '' : 'bad';
      }
      info.textContent = text;
      info.className = `small url-info${kind === 'bad' ? ' bad-text' : kind === 'ok' ? ' ok-text' : ''}`;
      this.urlEls[role].disabled = !!this.urlLoading;
    }
    const valid = jobs.length > 0 && jobs.every((job) => job.parsed.ok);
    this.urlLoad.disabled = !this.urlsAvailable || !valid || !!this.urlLoading || this.booting || this.busy > 0;
    this.urlCancel.hidden = !(this.urlLoading && this.urlLoading.phase === 'fetch');
  }

  setUrlResult(role, kind, text) {
    this.urlResult[role] = { kind, text };
    this.renderUrls();
  }

  setUrlProgress(role, loaded, total) {
    const progress = this.urlProgressEls[role];
    if (loaded === null) {
      progress.hidden = true;
      return;
    }
    progress.hidden = false;
    if (total) {
      progress.setAttribute('max', total);
      progress.setAttribute('value', loaded);
    } else {
      progress.removeAttribute('value'); // total unknown: indeterminate
    }
  }

  cancelUrls() {
    const run = this.urlLoading;
    if (!run || run.phase !== 'fetch') return;
    run.canceled = true;
    run.controller.abort();
  }

  /**
   * "Load from URLs": validates the non-empty fields, fetches them through the proxy (in parallel, with progress, a
   * Cancel button and a timeout), then feeds the bytes through `inspectBytes` in field order, exactly like chosen
   * files. Nothing is fetched except by this method, and it runs only from the Load button / form submission.
   */
  async loadUrls() {
    if (!this.urlsAvailable || this.urlLoading || this.booting || this.busy) return;
    const jobs = this.urlJobs();
    for (const job of jobs) if (!job.parsed.ok) this.urlResult[job.role] = { kind: 'bad', text: job.parsed.message };
    if (!jobs.length) {
      this.urlStatus.textContent = 'Enter at least one address.';
      return;
    }
    if (jobs.some((job) => !job.parsed.ok)) {
      this.urlStatus.textContent = 'Fix the addresses marked above first.';
      this.renderUrls();
      return;
    }
    if (jobs.length === 2 && jobs[0].parsed.url === jobs[1].parsed.url) {
      this.setUrlResult('handset', 'bad', 'This is the same address as the main controller field; the two files must be different.');
      this.urlStatus.textContent = 'Fix the addresses marked above first.';
      return;
    }

    const run = { controller: new AbortController(), timer: null, canceled: false, timedOut: false, phase: 'fetch' };
    this.urlLoading = run;
    this.urlStatus.textContent = 'Fetching…';
    this.problem.hidden = true;
    for (const job of jobs) this.setUrlResult(job.role, 'busy', `Fetching ${job.parsed.url} …`);
    run.timer = this.deps.timers.setTimer(() => {
      run.timedOut = true;
      run.controller.abort();
    }, this.deps.timeoutMs);
    this.refresh();
    let accepted = 0;
    try {
      const fetched = await Promise.all(jobs.map((job) => this.fetchOne(job, run)));
      this.deps.timers.clearTimer(run.timer);
      run.phase = 'verify';
      this.refresh();
      for (const item of fetched) {
        const { job } = item;
        if (item.error) {
          this.setUrlResult(job.role, 'bad', item.error);
          continue;
        }
        const size = item.bytes.length; // the buffer is transferred to the worker below and is empty afterwards
        this.setUrlResult(job.role, 'busy', `Fetched ${item.source} (${formatBytes(size)}). Verifying…`);
        const buffer = item.bytes.byteOffset === 0 && item.bytes.buffer.byteLength === item.bytes.byteLength
          ? item.bytes.buffer
          : item.bytes.buffer.slice(item.bytes.byteOffset, item.bytes.byteOffset + item.bytes.byteLength);
        const result = await this.inspectBytes(job.parsed.file, size, async () => buffer, item.source);
        if (result.accepted) {
          accepted += 1;
          const where = result.role === job.role ? '' : ` It is the ${ROLE_LABEL[result.role]} image, so it went to that slot (the file's content decides, not the field).`;
          this.setUrlResult(job.role, 'ok', `Fetched ${item.source} (${formatBytes(size)}) and verified as ${result.release ? `${result.release.name} ` : ''}${ROLE_LABEL[result.role]}.${where}`);
        } else {
          const sha = result.report && result.report.srecSha256 ? ` SHA-256 ${result.report.srecSha256}.` : '';
          this.setUrlResult(job.role, 'bad', `Fetched ${item.source} (${formatBytes(size)}) but it was not accepted: ${result.message || 'not a supported firmware image.'}${sha}`);
        }
      }
    } finally {
      this.deps.timers.clearTimer(run.timer);
      this.urlLoading = null; // whatever happened, the form is usable again
      this.urlStatus.textContent = accepted === jobs.length
        ? `Loaded and verified ${accepted === 1 ? 'the file' : 'both files'}.`
        : run.canceled ? 'Canceled.' : `${accepted} of ${jobs.length} loaded; see the messages above.`;
      this.refresh();
      // Synchronized only after a clean load: a failed field keeps its message in view.
      if (accepted === jobs.length) this.syncSources();
    }
  }

  /** Fetches one field through the proxy; resolves to {job, bytes, source} or {job, error: message}. */
  async fetchOne(job, run) {
    this.setUrlProgress(job.role, 0, null);
    try {
      const { bytes, source } = await fetchFirmware({
        endpoint: this.proxy.endpoint,
        label: this.proxy.label,
        url: job.parsed.url,
        fetchImpl: this.deps.fetch,
        signal: run.controller.signal,
        onProgress: (loaded, total) => {
          this.setUrlProgress(job.role, loaded, total);
          this.setUrlResult(job.role, 'busy', `Fetching ${job.parsed.url} … ${formatBytes(loaded)}${total ? ` of ${formatBytes(total)}` : ''}`);
        },
      });
      return { job, bytes, source: source || job.parsed.url };
    } catch (error) {
      if (error.code === 'aborted') return { job, error: run.timedOut ? `Timed out after ${Math.round(this.deps.timeoutMs / 1000)} s without a complete file.` : 'Canceled.' };
      return { job, error: error.message || String(error) };
    } finally {
      this.setUrlProgress(job.role, null);
    }
  }

  async useRemembered() {
    this.busy += 1;
    this.refresh();
    try {
      const result = await this.client.request('use-remembered');
      for (const file of result.accepted) this.slots[file.role] = { ...file, remembered: true };
      for (const problem of result.problems) this.rejected.push({ name: 'remembered file', message: problem });
      if (result.accepted.length) this.remember.checked = true;
    } catch (error) {
      this.rejected.push({ name: 'remembered files', message: error.message });
    }
    this.busy -= 1;
    this.refresh();
    this.syncSources();
  }

  async forget() {
    try {
      await this.client.request('forget');
      for (const role of Object.keys(this.slots)) {
        if (this.slots[role] && this.slots[role].remembered) {
          this.slots[role] = null;
          await this.client.request('clear-firmware', { role });
        }
      }
      this.remember.checked = false;
      this.renderRemembered(null);
    } catch (error) {
      this.rejected.push({ name: 'remembered files', message: error.message });
    }
    this.refresh();
  }

  async removeSlot(role) {
    this.slots[role] = null;
    this.syncSources(); // a file is missing again: show where to get it
    try {
      await this.client.request('clear-firmware', { role });
    } catch (_) { /* the slot is already cleared on the page */ }
    this.refresh();
  }

  options() {
    const adc = Number(byId('start-adc').value);
    return {
      mode: byId('start-mode').value,
      bootMode: byId('start-boot-mode').value,
      adcSample: adc,
      i2cIdleHigh: byId('start-i2c-idle').checked,
      // The start-at-the-surface fixture (on by default) and the page's remembered surface pressure.
      startAtSurface: byId('start-surface').checked,
      surfacePressureMbar: parseSurfacePressure(prefs.get('surface-pressure', '')),
      simultaneousStart: byId('start-simultaneous').checked,
      startPaused: byId('start-paused').checked,
      idleFastForward: byId('start-idle-ff').checked,
    };
  }

  ready() {
    const handsetOnly = byId('start-mode').value === 'handset';
    const adc = Number(byId('start-adc').value);
    const adcOk = Number.isInteger(adc) && adc >= 0 && adc <= 4095;
    const filesOk = !!this.slots.handset && (handsetOnly || !!this.slots.main);
    return { ok: filesOk && adcOk && !this.busy && !this.booting && !this.urlLoading, filesOk, adcOk, handsetOnly };
  }

  refresh() {
    for (const role of ['main', 'handset']) this.renderSlot(role);
    this.renderRememberedBanner();
    this.renderRejected();
    this.renderReleaseLine();
    this.renderProfileNote();
    this.renderUrls();
    const state = this.ready();
    this.bootButton.disabled = !state.ok;
    // Choosing a cold boot: the firmware clears the oxygen calibration on that wake cause, so say so.
    byId('start-cold-hint').hidden = byId('start-boot-mode').value !== 'cold';
    const release = this.release();
    const name = release ? `${release.name} ` : '';
    if (this.booting) this.bootHint.textContent = 'Starting…';
    else if (this.urlLoading) this.bootHint.textContent = 'Loading from URLs…';
    else if (this.busy) this.bootHint.textContent = 'Verifying…';
    else if (!state.adcOk) this.bootHint.textContent = 'The board-ID ADC sample must be an integer from 0 to 4095.';
    else if (state.filesOk) this.bootHint.textContent = state.handsetOnly ? `The ${name}handset image is verified; the main image is not needed for a handset-only run.` : `Both ${name}images are verified.`;
    else if (state.handsetOnly) this.bootHint.textContent = 'Provide the handset firmware file to continue.';
    else if (this.slots.main || this.slots.handset) this.bootHint.textContent = `Still needed: the ${name}${this.slots.main ? ROLE_LABEL.handset : ROLE_LABEL.main} firmware file.`;
    else this.bootHint.textContent = 'Provide both firmware files to continue.';
  }

  /** Names the release the verified files belong to and what the missing file must be. */
  renderReleaseLine() {
    const line = byId('release-line');
    const release = this.release();
    line.hidden = !release;
    if (!release) return;
    const complete = !!this.slots.main && !!this.slots.handset;
    line.textContent = complete
      ? `Release: ${release.label}. Both files are from this release.`
      : `Release: ${release.label}. The other file must be from the same release (${release.name}).`;
  }

  renderSlot(role) {
    const slot = this.slots[role];
    const element = this.slotEls[role];
    element.classList.toggle('ok', !!slot);
    // refresh() runs on every option change: rebuild a card only when what it shows changed, so an opened <details> (and
    // the keyboard focus inside the card) survives.
    const shown = slot || (this.busy ? 'busy' : 'waiting');
    if (this.shownSlots[role] === shown) return;
    this.shownSlots[role] = shown;
    const state = element.querySelector('[data-part="state"]');
    state.replaceChildren();
    if (!slot) {
      state.append(this.busy ? 'Verifying…' : 'Waiting for the file.');
      return;
    }
    // A verified file is one line (check mark, release and board, file name and size, Remove). The hash, the source address
    // and the checks are in a <details> that starts closed; a failed check is never folded away.
    const report = slot.report;
    const failed = report.checks.filter((check) => !check.ok);
    const passed = report.checks.length - failed.length;
    const checkItem = (check) => h('li', { class: check.ok ? '' : 'fail' }, `${check.ok ? '✓' : '✗'} ${check.name}: ${check.detail}`);
    const file = [slot.name, formatBytes(slot.size), slot.remembered ? 'remembered in this browser' : null].filter(Boolean).join(' · ');
    state.append(
      h('div', { class: 'slot-line' },
        h('div', { class: 'slot-title ok-text' },
          h('span', { role: 'img', 'aria-label': 'Verified' }, '✓'),
          ` ${slot.release ? `${slot.release.name} ` : ''}${ROLE_LABEL[role]}`),
        h('div', { class: 'slot-file' }, file),
        h('button', { type: 'button', class: 'slot-remove', 'aria-label': `Remove the ${ROLE_LABEL[role]} file`, onclick: () => this.removeSlot(role) }, 'Remove')),
      ...(failed.length ? [h('ul', { class: 'check-list failed' }, failed.map(checkItem))] : []), // state.append() would print a null
      h('details', { class: 'slot-details' },
        h('summary', {}, `${passed} of ${report.checks.length} checks passed · SHA-256`),
        h('code', { class: 'hash' }, `SHA-256 ${report.srecSha256}`),
        slot.source ? h('div', { class: 'small muted source' }, `Loaded from ${slot.source}`) : null,
        h('ul', { class: 'check-list' }, report.checks.map(checkItem))),
    );
  }

  renderRejected() {
    const list = byId('rejected');
    list.replaceChildren(...this.rejected.map((item, index) =>
      h('li', {},
        h('strong', {}, item.name), item.size !== undefined ? ` (${formatBytes(item.size)})` : '', ' was not accepted. ',
        item.message ? item.message.charAt(0).toUpperCase() + item.message.slice(1) : '',
        item.report && item.report.srecSha256 ? h('div', { class: 'small' }, `SHA-256 ${item.report.srecSha256}`) : null,
        item.source ? h('div', { class: 'small' }, `Loaded from ${item.source}`) : null,
        ' ', h('button', { type: 'button', class: 'link', onclick: () => { this.rejected.splice(index, 1); this.renderRejected(); } }, 'Dismiss'))));
  }

  showProblem(text, actions = []) {
    this.problem.replaceChildren(text, ...(actions.length ? [h('div', { class: 'actions-row' }, actions)] : []));
    this.problem.hidden = false;
  }

  async boot(profile) {
    if (!this.ready().ok && !this.booting) return;
    const options = this.options();
    this.booting = true;
    this.problem.hidden = true;
    this.refresh();
    try {
      const result = await this.client.request('boot', { options, remember: this.remember.checked, profile });
      this.booting = false;
      this.hooks.booted(result, { options, profile });
    } catch (error) {
      this.booting = false;
      this.refresh();
      if (error.profileProblem) {
        const release = this.release();
        const name = release ? `${release.name} ` : '';
        this.showProblem(`The saved ${name}profile could not be used: ${error.message}`, [
          h('button', { type: 'button', class: 'danger', onclick: async () => { if (await this.resetProfile(release ? release.id : RELEASE_IDS[0])) this.boot('stored'); } }, 'Erase the saved profile and boot'),
          h('button', { type: 'button', onclick: () => this.boot('none') }, 'Boot without the saved profile (nothing is saved)'),
        ]);
      } else {
        this.showProblem(error.message);
      }
    }
  }
}
