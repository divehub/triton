// Browser-local storage for the emulator: the profile (EEPROM, NOR, RTC checkpoint, inputs, LED colors) and the
// optionally remembered firmware files. Nothing is uploaded anywhere.
//
// Backends, all with the same asynchronous interface:
//   OpfsStorage    origin-private file system (preferred; uses synchronous access handles inside workers)
//   IdbStorage     IndexedDB (fallback when OPFS is unavailable, e.g. some private-browsing modes)
//   MemoryStorage  no persistence (last resort, and the Node tests)
//
// Areas: 'firmware' (the remembered SREC pair), one profile area per firmware release ('profile' for TRITON, the
// location of the first version of this app, and 'profile-<release>' for the others; see releases.js), and for custom
// builds (DESIGN 20.4) 'firmware-custom' (their remembered pair) and 'custom' (the one profile shared by every custom
// build), none of which is ever shared with the areas above. Names are plain file names.

import { storageAreas } from './releases.js';

const ROOT = 'ngc-wasm';

export class MemoryStorage {
  constructor() {
    this.kind = 'memory';
    this.persistent = false;
    this.areas = new Map();
  }

  area(name) {
    if (!this.areas.has(name)) this.areas.set(name, new Map());
    return this.areas.get(name);
  }

  async read(area, name) {
    const entry = this.area(area).get(name);
    return entry ? entry.data.slice() : null;
  }

  async write(area, name, data) {
    this.area(area).set(name, { data: data.slice(), modified: Date.now() });
  }

  async remove(area, name) {
    this.area(area).delete(name);
  }

  async list(area) {
    return [...this.area(area)].map(([name, entry]) => ({ name, size: entry.data.length, modified: entry.modified }));
  }

  async clear(area) {
    this.area(area).clear();
  }
}

function notFound(error) {
  return error && (error.name === 'NotFoundError' || error.code === 8);
}

export class OpfsStorage {
  static async open() {
    const root = await navigator.storage.getDirectory();
    const base = await root.getDirectoryHandle(ROOT, { create: true });
    // Probe a write so private-browsing restrictions surface here and not at the first save.
    const profile = await base.getDirectoryHandle('profile', { create: true });
    await base.getDirectoryHandle('firmware', { create: true });
    const probe = await profile.getFileHandle('.probe', { create: true });
    await OpfsStorage.writeFile(probe, new Uint8Array(0));
    await profile.removeEntry('.probe');
    return new OpfsStorage(base);
  }

  static async writeFile(handle, data) {
    if (typeof handle.createSyncAccessHandle === 'function') {
      const access = await handle.createSyncAccessHandle();
      try {
        access.truncate(0);
        let written = 0;
        while (written < data.length) {
          const count = access.write(data.subarray(written), { at: written });
          if (!(count > 0)) throw new Error('short write to the origin-private file system');
          written += count;
        }
        access.flush();
      } finally {
        access.close();
      }
    } else {
      const writable = await handle.createWritable();
      await writable.write(data);
      await writable.close();
    }
  }

  constructor(base) {
    this.kind = 'opfs';
    this.persistent = true;
    this.base = base;
    this.dirs = new Map();
  }

  async dir(area) {
    let dir = this.dirs.get(area);
    if (!dir) {
      dir = await this.base.getDirectoryHandle(area, { create: true });
      this.dirs.set(area, dir);
    }
    return dir;
  }

  async read(area, name) {
    const dir = await this.dir(area);
    let handle;
    try {
      handle = await dir.getFileHandle(name);
    } catch (error) {
      if (notFound(error)) return null;
      throw error;
    }
    const file = await handle.getFile();
    return new Uint8Array(await file.arrayBuffer());
  }

  async write(area, name, data) {
    const dir = await this.dir(area);
    const handle = await dir.getFileHandle(name, { create: true });
    await OpfsStorage.writeFile(handle, data);
  }

  async remove(area, name) {
    const dir = await this.dir(area);
    try {
      await dir.removeEntry(name);
    } catch (error) {
      if (!notFound(error)) throw error;
    }
  }

  async list(area) {
    const dir = await this.dir(area);
    const entries = [];
    for await (const [name, handle] of dir.entries()) {
      if (handle.kind !== 'file' || name.startsWith('.')) continue;
      const file = await handle.getFile();
      entries.push({ name, size: file.size, modified: file.lastModified });
    }
    return entries;
  }

  async clear(area) {
    for (const entry of await this.list(area)) await this.remove(area, entry.name);
  }
}

export class IdbStorage {
  static open() {
    return new Promise((resolve, reject) => {
      // Version 2 adds the profile stores of the other firmware releases; version 3 the stores of custom builds (their profile
      // and their remembered firmware); the stores of the earlier versions are kept.
      const request = indexedDB.open(ROOT, 3);
      request.onupgradeneeded = () => {
        const db = request.result;
        for (const store of new Set(['profile', ...storageAreas()])) {
          if (!db.objectStoreNames.contains(store)) db.createObjectStore(store);
        }
      };
      request.onsuccess = () => resolve(new IdbStorage(request.result));
      request.onerror = () => reject(request.error || new Error('IndexedDB is unavailable'));
      request.onblocked = () => reject(new Error('IndexedDB is blocked by another tab'));
    });
  }

  constructor(db) {
    this.kind = 'indexeddb';
    this.persistent = true;
    this.db = db;
  }

  request(area, mode, operation) {
    return new Promise((resolve, reject) => {
      const transaction = this.db.transaction(area, mode);
      const request = operation(transaction.objectStore(area));
      transaction.oncomplete = () => resolve(request ? request.result : undefined);
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error || new Error('IndexedDB transaction aborted'));
    });
  }

  async read(area, name) {
    const entry = await this.request(area, 'readonly', (store) => store.get(name));
    return entry ? new Uint8Array(entry.data) : null;
  }

  async write(area, name, data) {
    const copy = data.slice().buffer;
    await this.request(area, 'readwrite', (store) => store.put({ data: copy, modified: Date.now() }, name));
  }

  async remove(area, name) {
    await this.request(area, 'readwrite', (store) => store.delete(name));
  }

  async list(area) {
    const keys = await this.request(area, 'readonly', (store) => store.getAllKeys());
    const entries = [];
    for (const name of keys) {
      const entry = await this.request(area, 'readonly', (store) => store.get(name));
      if (entry) entries.push({ name, size: entry.data.byteLength, modified: entry.modified });
    }
    return entries;
  }

  async clear(area) {
    await this.request(area, 'readwrite', (store) => store.clear());
  }
}

/** Opens the best available backend; `problems` lists why better ones were skipped. */
export async function openStorage() {
  const problems = [];
  if (typeof navigator !== 'undefined' && navigator.storage && typeof navigator.storage.getDirectory === 'function') {
    try {
      return { storage: await OpfsStorage.open(), problems };
    } catch (error) {
      problems.push(`origin-private file system: ${error.message || error}`);
    }
  }
  if (typeof indexedDB !== 'undefined') {
    try {
      return { storage: await IdbStorage.open(), problems };
    } catch (error) {
      problems.push(`IndexedDB: ${error.message || error}`);
    }
  }
  return { storage: new MemoryStorage(), problems };
}
