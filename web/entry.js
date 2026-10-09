// Entry screen: asks for the two original SREC files of one firmware release (TRITON or NEPTUN), has the worker
// verify them, and boots the session.
//
// Custom mode (DESIGN 20, the explicit "Use custom firmware builds" choice): instead of the two original files and their
// release verification, one .srec build per board is chosen or dropped onto that board's card (the card decides the role), the
// worker checks it structurally, and the card shows the engine's report. There is no URL loading. Custom files, their
// remembered copies and their profile are kept apart from the original ones (`this.custom` against `this.slots`), so switching
// between the two modes never mixes them.

import { FIRMWARE_PROXY_URL } from './config.js';
import { parseSurfacePressure } from './deco.js';
import { byId, confirmDialog, formatBytes, formatClock, h, prefs, reportFacts } from './dom.js';
import { FETCH_TIMEOUT_MS, fetchFirmware, normalizeFirmwareUrl, proxyEndpoint } from './firmware-url.js';
import { clearCellFixture } from './game-logic.js';
import { CUSTOM_RELEASE_ID, RELEASES, RELEASE_IDS, describeRelease, profileArea } from './releases.js';

const ROLE_LABEL = { main: 'main controller 5.8', handset: 'handset 65.3' };
const CUSTOM_LABEL = { main: 'Main', handset: 'Handset' };
const ROLES = ['main', 'handset'];
const CUSTOM_SUBTITLE = 'Custom firmware builds · WebAssembly functional model';
const PREFILL_LIMIT = 2048;
// Archived copies of the TRITON main 5.8 / handset 65.3 SREC files (fetched only through the proxy, on Load).
const DEFAULT_URLS = {
  main: 'https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/pvlL3Iilv4o_Tu5lggngZAUt.srec',
  handset: 'https://web.archive.org/web/20261008041427/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec',
};

/** One line of a check list: a check mark or a cross, the check's name and the engine's detail. */
const checkItem = (check) => h('li', { class: check.ok ? '' : 'fail' }, `${check.ok ? '✓' : '✗'} ${check.name}: ${check.detail}`);

/** How a profile is named in the notes and dialogs: a release's name, or "custom-build" for the one shared custom profile. */
function profileName(id) {
  if (id === CUSTOM_RELEASE_ID) return 'custom-build';
  return (RELEASES[id] && RELEASES[id].name) || id;
}

export class EntryView {
  /**
   * @param {import('./worker-client.js').WorkerClient} client
   * @param {{booted: (result: object, info: object) => void, starting?: (info: {game: boolean}) => void}} hooks `booted` gets
   *   `info.game` true when the session was started with Start game; `starting` runs when a start begins
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
    // Custom mode: its own slots, refused files, remembered pair; none of it is shared with the original mode above.
    this.customMode = false;
    this.custom = {
      slots: { main: null, handset: null }, // the accepted file per board: the worker's `inspect-custom` result
      rejected: { main: null, handset: null }, // the refused file per board: {name, size, message, report}
      pending: { main: false, handset: false },
      shown: { main: undefined, handset: undefined }, // what each card was last rebuilt for (renderCustomSlot)
      remembered: null, // the custom pair kept in this browser, by role, or null
    };
    this.defaultSubtitle = byId('subtitle').textContent;
    this.customToggle = byId('custom-mode');
    this.customEls = { main: byId('custom-slot-main'), handset: byId('custom-slot-handset') };
    this.customInputs = { main: byId('custom-file-main'), handset: byId('custom-file-handset') };
    this.root = byId('screen-entry');
    this.slotEls = { main: byId('slot-main'), handset: byId('slot-handset') };
    this.dropzone = byId('dropzone');
    this.input = byId('file-input');
    this.bootButton = byId('boot');
    this.gameButton = byId('start-game');
    this.gameHint = byId('game-hint');
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
      if (this.customMode) return; // custom mode has no page-wide drop zone: each card takes its own file
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
      if (this.customMode) {
        // A drop outside the cards does nothing (the browser must not open the file); the card decides the role.
        if (event.dataTransfer && event.dataTransfer.files.length) this.setCustomStatus('Drop each file onto its own card: the Main or the Handset card decides which board the build is for.');
        return;
      }
      if (event.dataTransfer && event.dataTransfer.files.length) this.addFiles([...event.dataTransfer.files]);
    });
    this.wireCustom();
    for (const id of ['start-mode', 'start-boot-mode', 'start-adc', 'start-i2c-idle', 'start-surface', 'start-simultaneous', 'start-paused', 'start-idle-ff']) {
      byId(id).addEventListener('change', () => this.refresh());
    }
    this.bootButton.addEventListener('click', () => this.boot('stored'));
    this.gameButton.addEventListener('click', () => this.boot('stored', { game: true }));
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

  /** The release of the files provided so far (they always agree), or null; in custom mode always the custom build. */
  release() {
    if (this.customMode) return describeRelease(CUSTOM_RELEASE_ID);
    const slot = this.slots.main || this.slots.handset;
    return (slot && slot.release) || null;
  }

  /** The files the next boot would use: the custom ones in custom mode, otherwise the original ones. */
  activeSlots() {
    return this.customMode ? this.custom.slots : this.slots;
  }

  /** Whether the engine build can load custom builds (an older module cannot; an init without the flag is taken as capable). */
  customSupported() {
    return !(this.init && this.init.customSupported === false);
  }

  /** Shows the screen. `init` is the worker's reply to `init` (or `info`). */
  show(init) {
    this.root.hidden = false;
    this.init = init;
    byId('engine-label').textContent = init.engine;
    byId('engine-label-custom').textContent = init.engine;
    // Firmware is remembered in the origin-private file system only (the profile may fall back to IndexedDB).
    const canRemember = init.storage.kind === 'opfs';
    this.remember.disabled = !canRemember;
    if (!canRemember) this.remember.checked = false;
    byId('remember-note').hidden = canRemember;
    this.renderProfileNote();
    this.renderRemembered(init.remembered);
    this.renderCustomRemembered(init.rememberedCustom);
    this.problem.hidden = true;
    this.booting = false;
    if (!this.customSupported() && this.customMode) this.setCustomMode(false);
    this.renderMode();
    this.refresh();
    // Verified files from an earlier visit are used automatically (they are checked again), those of the mode in use.
    if (this.customMode) {
      if (init.rememberedCustom && !this.custom.slots.main && !this.custom.slots.handset) this.useRememberedCustom();
    } else if (init.remembered && !this.slots.main && !this.slots.handset) {
      this.useRemembered();
    }
    const dev = new URLSearchParams(window.location.search);
    if (dev.has('dev-firmware')) {
      const slots = String(dev.get('dev-firmware')).toLowerCase() === 'custom' ? this.custom.slots : this.slots;
      if (!slots.main && !slots.handset) this.loadDevFirmware(dev.get('dev-firmware'));
    }
  }

  /**
   * Development aid for automated browser checks: with `serve.py --dev-firmware DIR` and the page opened as
   * `/?dev-firmware` (TRITON), `/?dev-firmware=neptun` or `/?dev-firmware=custom` (the TRITON files in the custom slots), the
   * two files are fetched from the local server and handled exactly like chosen files.
   */
  async loadDevFirmware(which) {
    const wanted = String(which || '').toLowerCase();
    if (wanted === 'custom') return this.loadDevCustom();
    const release = Object.values(RELEASES).find((candidate) => candidate.name.toLowerCase() === wanted) || RELEASES[RELEASE_IDS[0]];
    // `?dev-firmware=mixed` offers a TRITON handset with a NEPTUN main file, to check that a mixed pair is refused.
    const names = wanted === 'mixed' ? [RELEASES[RELEASE_IDS[0]].files.handset, RELEASES[RELEASE_IDS[1]].files.main] : [release.files.handset, release.files.main];
    const files = [];
    for (const name of names) {
      try {
        files.push(await this.fetchDevFile(name));
      } catch (error) {
        this.rejected.push({ name, message: `Development firmware route: ${error.message}` });
      }
    }
    await this.addFiles(files);
  }

  async fetchDevFile(name) {
    const response = await this.deps.fetch(`/dev-firmware/${name}`, { cache: 'no-store' });
    if (!response.ok) throw new Error('route not enabled (start serve.py with --dev-firmware DIR)');
    return new File([await response.arrayBuffer()], name);
  }

  /**
   * The custom variant of the development aid: switches to custom mode and puts the TRITON main and handset files (served by
   * `serve.py --dev-firmware`, the same four names as above) into the custom slots, each for the role of its slot.
   */
  async loadDevCustom() {
    if (!this.customSupported()) return;
    this.setCustomMode(true, { auto: false });
    const release = RELEASES[RELEASE_IDS[0]];
    for (const role of ['handset', 'main']) {
      const name = release.files[role];
      try {
        await this.addCustomFiles(role, [await this.fetchDevFile(name)]);
      } catch (error) {
        this.custom.rejected[role] = { name, message: `Development firmware route: ${error.message}` };
        this.refresh();
      }
    }
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

  /**
   * The saved profile of the release in use; before any file is provided, of every release that has one. In custom mode only
   * the one profile shared by every custom build is offered (it is never mixed with a release's).
   */
  renderProfileNote() {
    this.note.replaceChildren();
    const profiles = (this.init && this.init.profiles) || {};
    const selected = this.release();
    const ids = this.customMode ? [CUSTOM_RELEASE_ID] : selected ? [selected.id] : RELEASE_IDS;
    const shown = ids.filter((id) => profiles[id]);
    this.note.hidden = shown.length === 0;
    for (const id of shown) {
      const profile = profiles[id];
      const name = profileName(id);
      const files = profile.files.map((file) => file.name).join(', ');
      const text = id === CUSTOM_RELEASE_ID
        ? `A saved custom-build profile (${files}; updated ${formatClock(profile.modified)}) is shared by every custom build and is restored when you boot one. Reset it before loading a build that stores its data differently. `
        : `A saved ${name} profile (${files}; updated ${formatClock(profile.modified)}) is restored when you boot ${name} firmware. `;
      this.note.append(h('div', {},
        h('span', {}, text),
        h('button', { type: 'button', class: 'link', onclick: () => this.resetProfile(id) }, `Reset the saved ${name} profile…`)));
    }
  }

  async resetProfile(releaseId) {
    const name = profileName(releaseId);
    const ok = await confirmDialog({
      title: `Reset the saved ${name} profile?`,
      message: `This erases the emulated EEPROM, log flash, clock checkpoint, sensor inputs and LED color labels of the ${name} profile stored in this browser${releaseId === CUSTOM_RELEASE_ID ? ' (shared by every custom build)' : ''}. Export the profile first if you may need it again.`,
      confirm: 'Erase profile',
      danger: true,
    });
    if (!ok) return false;
    try {
      await this.client.request('reset-profile', { release: releaseId });
      // The game's oxygen-cell deviations belong to the profile (a calibration is stored in its EEPROM): new ones are drawn.
      clearCellFixture(prefs, profileArea(releaseId));
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
    if (this.customMode || !this.urlsAvailable || this.urlLoading || this.booting || this.busy) return; // no URL loading for custom builds
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

  // ---- custom builds (DESIGN 20) -------------------------------------------------------------------------

  wireCustom() {
    this.customToggle.addEventListener('change', () => this.setCustomMode(this.customToggle.checked));
    for (const role of ROLES) {
      const card = this.customEls[role];
      const input = this.customInputs[role];
      input.addEventListener('change', () => {
        const files = [...input.files];
        input.value = '';
        this.addCustomFiles(role, files);
      });
      // The card is the drop target and decides the role; the page-wide handlers above only keep the browser from opening a file.
      const over = (event) => {
        event.preventDefault();
        card.classList.add('over');
      };
      card.addEventListener('dragenter', over);
      card.addEventListener('dragover', over);
      card.addEventListener('dragleave', (event) => {
        const into = event.relatedTarget && typeof event.relatedTarget.closest === 'function' ? event.relatedTarget.closest('.custom-slot') : null;
        if (into !== card) card.classList.remove('over');
      });
      card.addEventListener('drop', (event) => {
        event.preventDefault();
        event.stopPropagation();
        card.classList.remove('over');
        const files = event.dataTransfer ? [...event.dataTransfer.files] : [];
        if (files.length) this.addCustomFiles(role, files);
      });
    }
    byId('custom-use-remembered').addEventListener('click', () => this.useRememberedCustom());
    byId('custom-forget-remembered').addEventListener('click', () => this.forgetCustom());
  }

  /**
   * The explicit choice between the original releases and custom builds. Each mode keeps its own files, remembered pair and
   * profile; switching only changes which of them the screen shows and the next boot uses. In custom mode the page offers the
   * remembered custom builds of an earlier visit (`auto` false: not now, the caller fills the slots itself).
   */
  setCustomMode(flag, { auto = true } = {}) {
    const custom = !!flag && this.customSupported();
    this.customMode = custom;
    this.customToggle.checked = custom;
    for (const [id, hidden] of [['original-mode', custom], ['custom-mode-panel', !custom], ['lead-original', custom], ['lead-custom', !custom], ['fact-original', custom], ['fact-custom', !custom]]) {
      byId(id).hidden = hidden;
    }
    this.problem.hidden = true;
    this.renderMode();
    this.refresh();
    if (custom && auto && this.custom.remembered && !this.custom.slots.main && !this.custom.slots.handset) this.useRememberedCustom();
  }

  /** The header line under the title follows the mode (the emulator screen sets its own once a session runs). */
  renderMode() {
    byId('subtitle').textContent = this.customMode ? CUSTOM_SUBTITLE : this.defaultSubtitle;
  }

  setCustomStatus(text) {
    byId('custom-status').textContent = text;
  }

  /** Files for one board's card: one file per board, `.srec` only; the card decides the role. */
  async addCustomFiles(role, files) {
    if (!files.length) return;
    const [file, ...extra] = files;
    this.setCustomStatus(extra.length ? `One file per board: using ${file.name} and ignoring ${extra.length} other file${extra.length > 1 ? 's' : ''}.` : '');
    await this.inspectCustom(role, file);
    this.problem.hidden = true;
    this.refresh();
  }

  /** A new file for a board replaces whatever the card held (the worker drops the earlier one too); a refused file leaves it empty. */
  async inspectCustom(role, file) {
    const custom = this.custom;
    custom.slots[role] = null;
    custom.rejected[role] = null;
    custom.pending[role] = true;
    this.busy += 1;
    this.refresh();
    try {
      if (!/\.srec$/i.test(file.name)) {
        await this.client.request('clear-firmware', { role, custom: true });
        custom.rejected[role] = { name: file.name, size: file.size, message: 'custom builds are loaded from .srec files only; choose the .srec export of the build.' };
      } else {
        const bytes = await file.arrayBuffer();
        const result = await this.client.request('inspect-custom', { role, name: file.name, bytes }, [bytes]);
        if (result.accepted) custom.slots[role] = result;
        else custom.rejected[role] = { name: file.name, size: file.size, message: result.message || 'The structural checks failed.', report: result.report };
      }
    } catch (error) {
      custom.rejected[role] = { name: file.name, size: file.size, message: `Could not check this file: ${error.message}` };
    }
    custom.pending[role] = false;
    this.busy -= 1;
  }

  async removeCustom(role) {
    this.custom.slots[role] = null;
    this.custom.rejected[role] = null;
    this.setCustomStatus('');
    this.refresh();
    try {
      await this.client.request('clear-firmware', { role, custom: true });
    } catch (_) { /* the card is already cleared on the page */ }
    this.refresh();
  }

  /**
   * One board's card: waiting (with the file chooser), verifying, accepted (one line, the report in a closed <details>) or
   * refused (the message and the failed checks in view). Rebuilt only when what it shows changed, so an opened <details> survives.
   */
  renderCustomSlot(role) {
    const custom = this.custom;
    const slot = custom.slots[role];
    const rejected = slot ? null : custom.rejected[role];
    const element = this.customEls[role];
    element.classList.toggle('ok', !!slot);
    element.classList.toggle('bad', !!rejected);
    const shown = slot || rejected || (custom.pending[role] ? 'busy' : 'waiting');
    if (custom.shown[role] === shown) return;
    custom.shown[role] = shown;
    const state = element.querySelector('[data-part="state"]');
    state.replaceChildren();
    const label = CUSTOM_LABEL[role];
    const choose = (text) => h('button', { type: 'button', class: 'link', 'aria-label': `Choose the ${label.toLowerCase()} build file`, onclick: () => this.customInputs[role].click() }, text);
    if (shown === 'busy') {
      state.append('Verifying…');
    } else if (shown === 'waiting') {
      state.append('Waiting for the file. ', choose('Choose file…'));
    } else if (rejected) {
      const failed = ((rejected.report && rejected.report.checks) || []).filter((check) => !check.ok);
      state.append(
        h('div', { class: 'slot-line' },
          h('div', { class: 'slot-title bad-text' }, h('span', { role: 'img', 'aria-label': 'Not accepted' }, '✗'), ` ${rejected.name} was not accepted`),
          rejected.size !== undefined ? h('div', { class: 'slot-file' }, formatBytes(rejected.size)) : null,
          h('button', { type: 'button', class: 'slot-remove', 'aria-label': `Dismiss the message about the ${label.toLowerCase()} build`, onclick: () => this.removeCustom(role) }, 'Dismiss')),
        // The engine's message is the failed checks joined; show it only when no check says it (a wrong file type, an unreadable file).
        ...(failed.length ? [] : [h('p', { class: 'small bad-text' }, rejected.message ? rejected.message.charAt(0).toUpperCase() + rejected.message.slice(1) : 'The structural checks failed.')]),
        ...(failed.length ? [h('ul', { class: 'check-list failed' }, failed.map(checkItem))] : []),
        ...(rejected.report && (rejected.report.srecSha256 || (rejected.report.checks || []).length) ? [this.customDetails(rejected.report)] : []),
        choose('Choose another file…'));
    } else {
      const report = slot.report;
      const failed = (report.checks || []).filter((check) => !check.ok);
      const file = [slot.name, formatBytes(slot.size), slot.remembered ? 'remembered in this browser' : null].filter(Boolean).join(' · ');
      state.append(
        h('div', { class: 'slot-line' },
          h('div', { class: 'slot-title ok-text' }, h('span', { role: 'img', 'aria-label': 'Verified' }, '✓'), ` ${label} (custom build)`),
          h('div', { class: 'slot-file' }, file),
          h('button', { type: 'button', class: 'slot-remove', 'aria-label': `Remove the ${label.toLowerCase()} build`, onclick: () => this.removeCustom(role) }, 'Remove')),
        ...(failed.length ? [h('ul', { class: 'check-list failed' }, failed.map(checkItem))] : []),
        this.customDetails(report));
    }
  }

  /** The engine's structural report: both SHA-256 values, span, initial SP, reset PC, entry and the checks. */
  customDetails(report) {
    const checks = report.checks || [];
    const passed = checks.filter((check) => check.ok).length;
    const facts = reportFacts(report);
    return h('details', { class: 'slot-details' },
      h('summary', {}, `${passed} of ${checks.length} checks passed · SHA-256 · span · SP · reset PC`),
      report.srecSha256 ? h('code', { class: 'hash' }, `SREC SHA-256 ${report.srecSha256}`) : null,
      report.binSha256 ? h('code', { class: 'hash' }, `Binary SHA-256 ${report.binSha256}`) : null,
      facts.length ? h('div', { class: 'slot-facts small' }, facts.map((fact) => h('div', {}, fact))) : null,
      checks.length ? h('ul', { class: 'check-list' }, checks.map(checkItem)) : null);
  }

  /** The custom pair kept in this browser as the worker reported it (or null). */
  renderCustomRemembered(remembered) {
    this.custom.remembered = remembered || null;
    this.renderCustomBanner();
  }

  customRememberedInUse() {
    const roles = Object.keys(this.custom.remembered || {});
    return roles.length > 0 && roles.every((role) => this.custom.slots[role] && this.custom.slots[role].remembered);
  }

  renderCustomBanner() {
    const remembered = this.custom.remembered;
    byId('custom-remembered-banner').hidden = !remembered;
    if (!remembered) return;
    const inUse = this.customRememberedInUse();
    byId('custom-use-remembered').hidden = inUse;
    byId('custom-remembered-text').textContent = inUse
      ? 'Using the custom builds remembered from an earlier visit (they were checked again).'
      : `Custom builds from an earlier visit are stored in this browser: ${Object.entries(remembered).map(([role, file]) => `${CUSTOM_LABEL[role] || role}: ${file.name} (${formatBytes(file.size)})`).join(', ')}.`;
  }

  async useRememberedCustom() {
    this.busy += 1;
    this.refresh();
    try {
      const result = await this.client.request('use-remembered', { custom: true });
      for (const file of result.accepted) {
        this.custom.slots[file.role] = { ...file, remembered: true };
        this.custom.rejected[file.role] = null;
      }
      for (const problem of result.problems) {
        const match = /^(main|handset): ([\s\S]*)$/.exec(problem);
        if (match) this.custom.rejected[match[1]] = { name: `remembered ${match[1]} build`, message: match[2] };
        else this.setCustomStatus(problem);
      }
      if (result.accepted.length) this.remember.checked = true;
    } catch (error) {
      this.setCustomStatus(`The remembered custom builds could not be used: ${error.message}`);
    }
    this.busy -= 1;
    this.refresh();
  }

  async forgetCustom() {
    try {
      await this.client.request('forget', { custom: true });
      for (const role of ROLES) {
        if (this.custom.slots[role] && this.custom.slots[role].remembered) {
          this.custom.slots[role] = null;
          await this.client.request('clear-firmware', { role, custom: true });
        }
      }
      this.remember.checked = false;
      this.renderCustomRemembered(null);
    } catch (error) {
      this.setCustomStatus(`The remembered custom builds could not be forgotten: ${error.message}`);
    }
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
    const slots = this.activeSlots();
    const filesOk = !!slots.handset && (handsetOnly || !!slots.main);
    return { ok: filesOk && adcOk && !this.busy && !this.booting && !this.urlLoading, filesOk, adcOk, handsetOnly };
  }

  refresh() {
    for (const role of ['main', 'handset']) {
      this.renderSlot(role);
      this.renderCustomSlot(role);
    }
    this.renderRememberedBanner();
    this.renderCustomBanner();
    this.renderRejected();
    this.renderReleaseLine();
    this.renderProfileNote();
    this.renderUrls();
    const state = this.ready();
    this.bootButton.disabled = !state.ok;
    // Start game needs the main board (it feeds the sensors the game drives): with "Handset only" it is off, and says why.
    this.gameButton.disabled = !state.ok || state.handsetOnly;
    this.gameHint.hidden = !state.handsetOnly;
    // The mode choice cannot change while something is being checked, fetched or started.
    this.customToggle.disabled = !this.customSupported() || this.booting || this.busy > 0 || !!this.urlLoading;
    byId('custom-unsupported').hidden = this.customSupported();
    // Choosing a cold boot: the original firmware clears the oxygen calibration on that wake cause, so say so; a custom build
    // may have its cold boot refused by the engine.
    const cold = byId('start-boot-mode').value === 'cold';
    byId('start-cold-hint').hidden = !cold || this.customMode;
    byId('start-cold-hint-custom').hidden = !cold || !this.customMode;
    const release = this.release();
    const name = release ? `${release.name} ` : '';
    const slots = this.activeSlots();
    const board = this.customMode ? { main: 'main build', handset: 'handset build' } : { main: `${name}${ROLE_LABEL.main} firmware file`, handset: `${name}${ROLE_LABEL.handset} firmware file` };
    if (this.booting) this.bootHint.textContent = 'Starting…';
    else if (this.urlLoading) this.bootHint.textContent = 'Loading from URLs…';
    else if (this.busy) this.bootHint.textContent = 'Verifying…';
    else if (!state.adcOk) this.bootHint.textContent = 'The board-ID ADC sample must be an integer from 0 to 4095.';
    else if (state.filesOk && this.customMode) this.bootHint.textContent = state.handsetOnly ? 'The handset build passed the structural checks; the main build is not needed for a handset-only run.' : 'Both custom builds passed the structural checks.';
    else if (state.filesOk) this.bootHint.textContent = state.handsetOnly ? `The ${name}handset image is verified; the main image is not needed for a handset-only run.` : `Both ${name}images are verified.`;
    else if (state.handsetOnly) this.bootHint.textContent = this.customMode ? 'Provide the handset build to continue.' : 'Provide the handset firmware file to continue.';
    else if (slots.main || slots.handset) this.bootHint.textContent = `Still needed: the ${slots.main ? board.handset : board.main}.`;
    else this.bootHint.textContent = this.customMode ? 'Provide both custom builds to continue.' : 'Provide both firmware files to continue.';
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

  /** Boots the session: the emulator view, or (`game`) the dive game on the same firmware, profile and start options. */
  async boot(profile, { game = false } = {}) {
    if (!this.ready().ok && !this.booting) return;
    if (game && this.ready().handsetOnly) return;
    const options = this.options();
    this.booting = true;
    this.problem.hidden = true;
    this.refresh();
    const custom = this.customMode;
    if (this.hooks.starting) this.hooks.starting({ game });
    try {
      const result = await this.client.request('boot', { options, remember: this.remember.checked, profile, custom });
      this.booting = false;
      this.hooks.booted(result, { options, profile, custom, game });
    } catch (error) {
      this.booting = false;
      this.refresh();
      if (error.profileProblem) {
        const release = this.release();
        const name = release ? `${release.name} ` : '';
        this.showProblem(`The saved ${name}profile could not be used: ${error.message}`, [
          h('button', { type: 'button', class: 'danger', onclick: async () => { if (await this.resetProfile(release ? release.id : RELEASE_IDS[0])) this.boot('stored', { game }); } }, 'Erase the saved profile and boot'),
          h('button', { type: 'button', onclick: () => this.boot('none', { game }) }, 'Boot without the saved profile (nothing is saved)'),
        ]);
      } else {
        this.showProblem(error.message);
      }
    }
  }
}
