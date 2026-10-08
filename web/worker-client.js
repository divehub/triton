// Page-side handle of the emulation worker: promise-based requests, event subscriptions, failure reporting.

export class WorkerError extends Error {
  constructor(info) {
    super(info && info.message ? info.message : String(info));
    this.name = 'WorkerError';
    Object.assign(this, info && typeof info === 'object' ? info : {});
  }
}

export class WorkerClient {
  constructor(url) {
    this.nextId = 1;
    this.pending = new Map();
    this.handlers = new Map();
    this.failed = null;
    this.worker = new Worker(url, { type: 'module' });
    this.worker.onmessage = (event) => this.dispatch(event.data);
    this.worker.onerror = (event) => {
      event.preventDefault();
      this.fail(`The emulation worker failed: ${event.message || 'could not be started (is pkg/ngc_wasm.wasm built?)'}`);
    };
    this.worker.onmessageerror = () => this.fail('The emulation worker sent a message the page could not read.');
  }

  /** Subscribes to broadcast messages of one type (`state`, `frame`, `notice`, `download`, `crash`, `lifecycle`, `failure`). */
  on(type, handler) {
    if (!this.handlers.has(type)) this.handlers.set(type, []);
    this.handlers.get(type).push(handler);
  }

  emit(type, message) {
    for (const handler of this.handlers.get(type) || []) handler(message);
  }

  dispatch(message) {
    if (message.type === 'response') {
      const entry = this.pending.get(message.id);
      if (!entry) return;
      this.pending.delete(message.id);
      if (message.ok) entry.resolve(message.result);
      else entry.reject(new WorkerError(message.error));
      return;
    }
    this.emit(message.type, message);
  }

  fail(text) {
    if (this.failed) return;
    this.failed = text;
    for (const entry of this.pending.values()) entry.reject(new WorkerError({ message: text }));
    this.pending.clear();
    this.emit('failure', { type: 'failure', message: text });
  }

  /** Sends a request and resolves with the worker's result (rejects with a WorkerError carrying the engine's message). */
  request(type, payload = {}, transfer = []) {
    if (this.failed) return Promise.reject(new WorkerError({ message: this.failed }));
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, type, ...payload }, transfer);
    });
  }

  /** Fire-and-forget message. */
  send(type, payload = {}, transfer = []) {
    if (!this.failed) this.worker.postMessage({ type, ...payload }, transfer);
  }

  terminate() {
    this.worker.terminate();
  }
}
