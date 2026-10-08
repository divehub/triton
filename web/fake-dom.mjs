// A minimal DOM for the Node tests (test helper; never served by serve.py). It parses the real index.html and
// provides just enough of the DOM for `emulator.js`, `entry.js`, `lcd.js` and `dom.js` to run unchanged:
// elements with attributes, classes, values (range inputs quantize like browsers), validity, selects, details,
// event dispatch with bubbling, simple selectors and a few window/document globals.

const VOID = new Set(['input', 'img', 'meta', 'link', 'br', 'hr', 'source']);
const BOOLEAN = new Set(['hidden', 'required', 'disabled', 'checked', 'selected', 'open', 'multiple']);

export class Node {}

export class TextNode extends Node {
  constructor(data) {
    super();
    this.data = String(data);
    this.parent = null;
  }

  get textContent() {
    return this.data;
  }

  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this);
    this.parent = null;
  }
}

function matchesToken(element, token) {
  const tag = token.match(/^[a-z][\w-]*/i)?.[0];
  if (tag && element.tagName !== tag.toUpperCase()) return false;
  const id = token.match(/#([\w-]+)/)?.[1];
  if (id && element.id !== id) return false;
  for (const match of token.matchAll(/\.([\w-]+)/g)) {
    if (!element.className.split(/\s+/).includes(match[1])) return false;
  }
  for (const match of token.matchAll(/\[([\w-]+)(?:\s*=\s*["']?([^\]"']+)["']?)?\]/g)) {
    if (!(match[1] in element.attributes)) return false;
    if (match[2] !== undefined && element.getAttribute(match[1]) !== match[2]) return false;
  }
  return true;
}

function matches(element, selector) {
  const tokens = selector.trim().split(/\s+/);
  if (!matchesToken(element, tokens.pop())) return false;
  let ancestor = element.parent;
  while (tokens.length) {
    const token = tokens.pop();
    while (ancestor && !(ancestor instanceof Element && matchesToken(ancestor, token))) ancestor = ancestor.parent;
    if (!ancestor) return false;
    ancestor = ancestor.parent;
  }
  return true;
}

export class Element extends Node {
  constructor(tag = 'div') {
    super();
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.parent = null;
    this.listeners = new Map();
    this.attributes = {};
    this.className = '';
    this.dataset = {};
    this.style = { setProperty() {} };
    this._value = '';
    this._checked = false;
    this.hidden = false;
    this.disabled = false;
    this.open = false;
    this.required = false;
    this.title = '';
    this.scrollHeight = 0;
    this.scrollTop = 0;
    this.clientHeight = 0;
    this.width = 0;
    this.height = 0;
    this.classList = {
      toggle: (name, flag) => {
        const classes = new Set(this.className.split(/\s+/).filter(Boolean));
        const on = flag === undefined ? !classes.has(name) : flag;
        if (on) classes.add(name); else classes.delete(name);
        this.className = [...classes].join(' ');
      },
      remove: (name) => this.classList.toggle(name, false),
      add: (name) => this.classList.toggle(name, true),
      contains: (name) => this.className.split(/\s+/).includes(name),
    };
    this.elements = { namedItem: (name) => this.querySelectorAll('[name]').find((item) => item.name === name) || null };
  }

  get id() { return this.attributes.id || ''; }
  get name() { return this.attributes.name || ''; }
  get type() { return this.attributes.type || (this.tagName === 'INPUT' ? 'text' : ''); }
  get min() { return this.attributes.min; }
  get max() { return this.attributes.max; }
  get step() { return this.attributes.step; }
  get firstElementChild() { return this.children.find((child) => child instanceof Element) || null; }
  get isContentEditable() { return false; }
  get options() { return this.children.filter((child) => child.tagName === 'OPTION'); }

  get textContent() {
    return this.children.map((child) => child.textContent).join('');
  }

  set textContent(text) {
    for (const child of this.children) child.parent = null;
    this.children = [];
    if (String(text) !== '') this.append(new TextNode(text));
  }

  get checked() { return this._checked; }
  set checked(flag) { this._checked = !!flag; }

  get selectedIndex() {
    return this.options.findIndex((option) => option.selected);
  }

  set selectedIndex(index) {
    this.options.forEach((option, i) => { option.selected = i === index; });
  }

  get value() {
    if (this.tagName === 'SELECT') {
      const option = this.options.find((candidate) => candidate.selected) || this.options[0];
      return option ? option.value : '';
    }
    if (this.tagName === 'OPTION') return this.attributes.value ?? this.textContent;
    return this._value;
  }

  set value(value) {
    let text = String(value);
    if (this.tagName === 'SELECT') {
      const index = this.options.findIndex((option) => option.value === text);
      this.selectedIndex = index;
      return;
    }
    if (this.tagName === 'OPTION') {
      this.attributes.value = text;
      return;
    }
    // Browser range elements clamp and align assigned values to their step.
    if (this.type === 'range' && text.trim() && Number.isFinite(Number(text))) {
      const min = Number(this.min ?? 0);
      const max = Number(this.max ?? 100);
      const step = Number(this.step ?? 1);
      const bounded = Math.max(min, Math.min(max, Number(text)));
      text = String(Number((Math.round((bounded - min) / step) * step + min).toFixed(12)));
    }
    this._value = text;
  }

  get valueAsNumber() { return String(this.value).trim() === '' ? NaN : Number(this.value); }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
    if (name === 'class') this.className = String(value);
    else if (name.startsWith('data-')) this.dataset[name.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = String(value);
    else if (name === 'value') {
      if (this.tagName !== 'SELECT') this._value = String(value);
    } else if (BOOLEAN.has(name)) this[name === 'checked' ? '_checked' : name] = true;
    else if (name === 'title') this.title = String(value);
  }

  getAttribute(name) { return this.attributes[name] ?? null; }
  removeAttribute(name) { delete this.attributes[name]; }

  append(...children) {
    for (const item of children) {
      // Like the DOM, anything that is not a node becomes a text node: append(null) prints "null".
      const child = item instanceof Node ? item : new TextNode(String(item));
      if (child.parent) child.remove();
      child.parent = this;
      this.children.push(child);
    }
  }

  after(...children) {
    const index = this.parent.children.indexOf(this);
    for (const child of children) child.parent = this.parent;
    this.parent.children.splice(index + 1, 0, ...children);
  }

  replaceChildren(...children) {
    for (const child of this.children) child.parent = null;
    this.children = [];
    this.append(...children);
  }

  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this);
    this.parent = null;
  }

  addEventListener(name, fn) {
    if (!this.listeners.has(name)) this.listeners.set(name, []);
    this.listeners.get(name).push(fn);
  }

  /** Dispatches an event with bubbling through the parents (and the document's listeners). */
  dispatch(name, extra = {}) {
    const event = {
      type: name, target: this, bubbles: true, defaultPrevented: false,
      preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.bubbles = false; },
      ...extra,
    };
    for (let node = this; node && event.bubbles; node = node.parent) {
      event.currentTarget = node;
      for (const fn of node.listeners ? node.listeners.get(name) || [] : []) fn(event);
    }
    return event;
  }

  click() {
    if (this.tagName === 'INPUT' && this.type === 'checkbox') this.checked = !this.checked;
    return this.dispatch('click', { detail: 0 });
  }

  focus() { this.ownerDocument().activeElement = this; }
  blur() { if (this.ownerDocument().activeElement === this) this.ownerDocument().activeElement = null; }

  ownerDocument() {
    let node = this;
    while (node.parent) node = node.parent;
    return node.isDocument ? node : globalThis.document;
  }

  getBoundingClientRect() { return { width: 640, height: 480, top: 0, left: 0 }; }
  getContext() { return { putImageData() {} }; }

  querySelectorAll(selector) {
    const found = [];
    const visit = (element) => {
      for (const child of element.children) {
        if (child instanceof Element) {
          found.push(child);
          visit(child);
        }
      }
    };
    visit(this);
    const selectors = selector.split(',');
    return found.filter((element) => selectors.some((item) => matches(element, item)));
  }

  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }

  closest(selector) {
    for (let element = this; element instanceof Element; element = element.parent) {
      if (selector.split(',').some((item) => matches(element, item))) return element;
    }
    return null;
  }

  checkValidity() {
    if (this.tagName === 'FORM') return this.querySelectorAll('input').every((input) => input.checkValidity());
    if (this.type === 'checkbox' || !['number', 'range'].includes(this.type)) return true;
    if (String(this.value).trim() === '') return !this.required;
    const value = this.valueAsNumber;
    if (!Number.isFinite(value) || (this.min !== undefined && value < Number(this.min)) || (this.max !== undefined && value > Number(this.max))) return false;
    if (this.step === 'any') return true;
    const step = Number(this.step ?? 1);
    const base = Number(this.min ?? this.attributes.value ?? 0);
    const units = (value - base) / step;
    return Math.abs(units - Math.round(units)) < 1e-7;
  }
}

export class FakeDocument extends Element {
  constructor() {
    super('#document');
    this.isDocument = true;
    this.hidden = false;
    this.activeElement = null;
    this.title = '';
    this.documentElement = new Element('html');
    this.body = new Element('body');
  }

  createElement(tag) { return new Element(tag); }
  createTextNode(text) { return new TextNode(text); }
  getElementById(id) { return this.querySelector(`#${id}`); }
}

/** Parses markup into a FakeDocument (scripts and styles are dropped, comments ignored). */
export function parseHtml(html) {
  const document = new FakeDocument();
  const stack = [document];
  const markup = html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<script(?:\s[^>]*)?>[\s\S]*?<\/script>/g, '')
    .replace(/<style[^>]*>[\s\S]*?<\/style>/g, '')
    .replace(/<!doctype[^>]*>/i, '');
  let last = 0;
  const tag = /<(\/?)([a-z][\w-]*)((?:[^>"']|"[^"]*"|'[^']*')*)>/gi;
  for (const match of markup.matchAll(tag)) {
    const text = markup.slice(last, match.index);
    if (text.trim()) stack[stack.length - 1].append(new TextNode(text.replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>')));
    last = match.index + match[0].length;
    const [, closing, name, attributes] = match;
    if (closing) {
      while (stack.length > 1) {
        const top = stack.pop();
        if (top.tagName === name.toUpperCase()) break;
      }
      continue;
    }
    const element = new Element(name);
    for (const attribute of attributes.matchAll(/([\w:-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
      element.setAttribute(attribute[1], attribute[2] ?? attribute[3] ?? attribute[4] ?? '');
    }
    stack[stack.length - 1].append(element);
    if (!VOID.has(name.toLowerCase())) stack.push(element);
  }
  for (const select of document.querySelectorAll('select')) {
    if (select.selectedIndex < 0 && select.options.length) select.selectedIndex = 0;
  }
  return document;
}

class FakeStorage {
  constructor() { this.map = new Map(); }
  getItem(key) { return this.map.has(key) ? this.map.get(key) : null; }
  setItem(key, value) { this.map.set(key, String(value)); }
}

/** Installs a fresh document and window as globals; returns them. */
export function installDom(html, { search = '', location = {} } = {}) {
  const document = parseHtml(html);
  const window = new Element('window');
  window.devicePixelRatio = 1;
  // A page served by serve.py unless a test says otherwise (`location` overrides protocol, hostname, port, origin).
  window.location = { search, protocol: 'http:', hostname: '127.0.0.1', port: '8770', origin: 'http://127.0.0.1:8770', ...location };
  window.localStorage = new FakeStorage();
  window.confirm = () => true;
  Object.assign(globalThis, {
    document, window, Node, Element, ImageData: class ImageData { constructor(data, width, height) { Object.assign(this, { data, width, height }); } },
  });
  delete globalThis.HTMLDialogElement;
  return { document, window };
}
