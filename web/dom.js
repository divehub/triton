// Small DOM and formatting helpers shared by the page modules.

export const byId = (id) => document.getElementById(id);

/** Creates an element: h('p', {class: 'x', onclick: fn}, 'text', child, ...). Text is never parsed as HTML. */
export function h(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') element.className = value;
    else if (key.startsWith('on') && typeof value === 'function') element.addEventListener(key.slice(2), value);
    else if (key === 'dataset') Object.assign(element.dataset, value);
    else if (value === true) element.setAttribute(key, '');
    else element.setAttribute(key, String(value));
  }
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    element.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return element;
}

/**
 * Sets an element's text only when it differs. Replacing a text with identical text still replaces the text node: it
 * ends a text selection, invalidates layout and, for an <option>, makes the browser rebuild or close an open dropdown.
 * Use it for anything the periodic state update rewrites.
 */
export function setText(element, text) {
  if (element.textContent !== text) element.textContent = text;
}

export function formatBytes(count) {
  if (count < 1024) return `${count} bytes`;
  if (count < 1024 * 1024) return `${(count / 1024).toFixed(1)} KiB`;
  return `${(count / (1024 * 1024)).toFixed(2)} MiB`;
}

export function hex32(value) {
  return typeof value === 'number' && Number.isFinite(value) ? `0x${(value >>> 0).toString(16).padStart(8, '0')}` : '—';
}

/**
 * The facts of a custom build's structural report (`ngc_firmware_inspect_custom`) as short lines: the span, the initial SP, the
 * reset PC and the entry address, for those the report has. The entry screen's card and the session information share it.
 */
export function reportFacts(report) {
  if (!report || typeof report !== 'object') return [];
  const has = (value) => value !== undefined && value !== null;
  const span = report.span && Number.isFinite(report.span.start) && Number.isFinite(report.span.end) ? report.span : null;
  const sp = has(report.initialSp) ? report.initialSp : report.stack;
  const pc = has(report.resetPc) ? report.resetPc : report.resetPC;
  return [
    span ? `Span ${hex32(span.start)}–${hex32(span.end)} (${formatBytes(span.end - span.start)}; the end is exclusive)` : null,
    has(sp) ? `Initial SP ${hex32(sp)}` : null,
    has(pc) ? `Reset PC ${hex32(pc)}` : null,
    has(report.entry) ? `Entry ${hex32(report.entry)}` : null,
  ].filter(Boolean);
}

export function formatClock(milliseconds) {
  return milliseconds ? new Date(milliseconds).toLocaleString() : 'never';
}

/** localStorage with try/catch (it can be unavailable in private modes). */
export const prefs = {
  get(key, fallback) {
    try {
      const value = window.localStorage.getItem(`ngc-wasm.${key}`);
      return value === null ? fallback : value;
    } catch (_) {
      return fallback;
    }
  },
  set(key, value) {
    try {
      window.localStorage.setItem(`ngc-wasm.${key}`, String(value));
    } catch (_) { /* ignore */ }
  },
};

/** Modal confirmation (uses <dialog> when available). Resolves to true when the user confirms. */
export function confirmDialog({ title, message, confirm = 'OK', cancel = 'Cancel', danger = false }) {
  if (typeof HTMLDialogElement === 'undefined' || typeof HTMLDialogElement.prototype.showModal !== 'function') {
    return Promise.resolve(window.confirm(`${title}\n\n${message}`));
  }
  return new Promise((resolve) => {
    const dialog = h('dialog', { class: 'dialog', 'aria-labelledby': 'dialog-title' });
    const finish = (value) => {
      dialog.close();
      dialog.remove();
      resolve(value);
    };
    dialog.append(
      h('h2', { id: 'dialog-title' }, title),
      h('p', {}, message),
      h('div', { class: 'actions-row' },
        h('button', { type: 'button', onclick: () => finish(false) }, cancel),
        h('button', { type: 'button', class: danger ? 'danger' : 'primary', onclick: () => finish(true) }, confirm)),
    );
    dialog.addEventListener('cancel', (event) => {
      event.preventDefault();
      finish(false);
    });
    document.body.append(dialog);
    dialog.showModal();
  });
}

/** Offers a file for download. */
export function saveFile(filename, mime, bytes) {
  const url = URL.createObjectURL(new Blob([bytes], { type: mime }));
  const link = h('a', { href: url, download: filename });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
}
