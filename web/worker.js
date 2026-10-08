// Web Worker entry point: owns the WebAssembly engine and everything that runs against wall-clock time.
// The page talks to it through the message protocol documented in runtime.js. Module worker: no importScripts.

import { Engine } from './engine.js';
import { Runtime, makeTimers } from './runtime.js';
import { openStorage } from './storage.js';

const WASM_URL = new URL('./pkg/ngc_wasm.wasm', import.meta.url).href;
const LOCK_NAME = 'ngc-wasm-profile';

/**
 * Takes the profile lock for the lifetime of a session so two tabs never write the same profile. Resolves to a
 * release function, or to null when another tab holds the lock (or the Web Locks API is missing: then the
 * profile is not protected, which is better than refusing to save).
 */
function acquireLock() {
  if (typeof navigator === 'undefined' || !navigator.locks) return Promise.resolve(() => {});
  return new Promise((resolve) => {
    navigator.locks
      .request(LOCK_NAME, { ifAvailable: true }, (lock) => {
        if (!lock) {
          resolve(null);
          return undefined;
        }
        return new Promise((release) => resolve(() => release()));
      })
      .catch(() => resolve(() => {}));
  });
}

const timers = makeTimers();
const runtime = new Runtime({
  post: (message, transfer) => self.postMessage(message, transfer || []),
  loadEngine: () => Engine.load(WASM_URL),
  openStorage,
  acquireLock,
  now: () => performance.now(),
  wallClock: () => Date.now(),
  setTimer: timers.setTimer,
  clearTimer: timers.clearTimer,
});

self.onmessage = (event) => runtime.receive(event.data);
self.onmessageerror = () => runtime.notice('error', 'The page sent a message the worker could not read.');
