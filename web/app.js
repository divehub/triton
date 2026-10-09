// NGC system emulator (WebAssembly): page controller.
//
// The page itself is thin: a Web Worker (worker.js -> runtime.js) owns the WebAssembly engine, paces it against
// wall-clock time and persists the profile; this module wires the entry screen (firmware files), the emulator
// screen (LCD, controls, state), the dive game (DESIGN 21) and the worker's broadcast messages together. A session is shown
// by the emulator view (Boot emulator) or by the game (Start game), never by both: the broadcasts go to the one that runs it.

import { byId, saveFile } from './dom.js';
import { EmulatorView } from './emulator.js';
import { EntryView } from './entry.js';
import { GameView } from './game.js';
import { WorkerClient } from './worker-client.js';

const screens = { loading: byId('screen-loading'), entry: byId('screen-entry'), emulator: byId('screen-emulator'), game: byId('screen-game') };
const appMain = byId('app-main'); // holds the loading, entry and emulator screens; the game is a screen of its own outside it

let currentScreen = 'loading';

function showScreen(name) {
  const changed = name !== currentScreen;
  currentScreen = name;
  for (const [key, element] of Object.entries(screens)) element.hidden = key !== name;
  appMain.hidden = name === 'game';
  // A new screen starts at its top: the Boot button the user pressed sits far down a long entry page (a phone), and the
  // emulator would otherwise open scrolled past its display.
  if (changed && typeof window.scrollTo === 'function') window.scrollTo(0, 0);
}

function setBadge(text, kind) {
  const badge = byId('status');
  badge.classList.remove('running', 'warn', 'bad');
  if (kind) badge.classList.add(kind);
  byId('status-text').textContent = text;
}

function fatal(text) {
  showScreen('loading');
  byId('loading-text').textContent = text;
  byId('loading-text').classList.add('bad-text');
  setBadge('Unavailable', 'bad');
}

function requirements() {
  const missing = [];
  if (typeof Worker === 'undefined') missing.push('Web Workers');
  if (typeof WebAssembly === 'undefined') missing.push('WebAssembly');
  if (!window.HTMLCanvasElement) missing.push('canvas');
  return missing;
}

async function main() {
  const missing = requirements();
  if (missing.length) {
    fatal(`This browser lacks ${missing.join(', ')}; the emulator needs a current Chrome, Edge, Safari or Firefox.`);
    return;
  }
  let client;
  try {
    client = new WorkerClient(new URL('./worker.js', import.meta.url));
  } catch (error) {
    fatal(`The emulation worker could not be created: ${error.message}`);
    return;
  }

  let init = null;
  let mode = 'emulator'; // the view of the running session: 'emulator' or 'game'
  let startingGame = false; // a Start game boot is under way (its first frame, state and notices come before it returns)
  const entry = new EntryView(client, {
    starting: ({ game: isGame }) => {
      startingGame = isGame;
      if (isGame) game.notices = [];
    },
    booted: (result, info) => (info.game ? startGame(result, info) : startEmulator(result, info)),
  });
  const emulator = new EmulatorView(client, { closeSession: () => closeSession(), notify: (level, text) => emulator.addNotice(level, text) });
  const game = new GameView(client, { quit: () => closeSession() });
  const gameActive = () => mode === 'game' || startingGame;

  function startEmulator(result, info) {
    mode = 'emulator';
    startingGame = false;
    entry.hide();
    emulator.notices = [];
    // The files of the mode that booted (custom builds or an original release), and the release the worker reports for them.
    emulator.show({ ...info, slots: { ...entry.activeSlots() }, release: result.release || entry.release() });
    emulator.onState({ state: result.state, host: result.hostStatus });
    showScreen('emulator');
  }

  function startGame(result, info) {
    mode = 'game';
    startingGame = false;
    entry.hide();
    game.show({ ...info, slots: { ...entry.activeSlots() }, release: result.release || entry.release() });
    game.onState({ state: result.state, host: result.hostStatus });
    showScreen('game');
  }

  async function closeSession() {
    const view = mode === 'game' ? game : emulator;
    try {
      await client.request('close-session');
    } catch (error) {
      view.addNotice('error', error.message);
      return;
    }
    if (mode === 'game') {
      game.hide();
    } else {
      emulator.hide();
      emulator.state = null;
      emulator.host = null;
    }
    mode = 'emulator';
    byId('rt-badge').hidden = true;
    try {
      const info = await client.request('info');
      init = { ...init, ...info };
    } catch (_) { /* keep the previous information */ }
    showScreen('entry');
    entry.show(init);
    setBadge('Ready', null);
  }

  client.on('state', (message) => (gameActive() ? game.onState(message) : emulator.onState(message)));
  client.on('frame', (message) => (gameActive() ? game.onFrame(message) : emulator.onFrame(message)));
  client.on('notice', (message) => (gameActive() ? game.addNotice(message.level, message.text) : emulator.addNotice(message.level, message.text)));
  client.on('download', (message) => saveFile(message.filename, message.mime, message.bytes));
  client.on('crash', (message) => {
    const detail = message.panic ? `\n${message.panic}` : '';
    const text = `The emulation engine stopped after an internal error (${message.message}).${detail}\nReload the page to start again; the saved profile is unchanged.`;
    if (mode === 'game') game.setConnectionError(text);
    else emulator.setConnectionError(text);
    setBadge('Stopped', 'bad');
  });
  client.on('failure', (message) => {
    if (!screens.game.hidden) game.setConnectionError(message.message);
    else if (!screens.emulator.hidden) emulator.setConnectionError(message.message);
    else fatal(message.message);
  });

  try {
    init = await client.request('init');
  } catch (error) {
    fatal(`The emulation engine could not be loaded: ${error.message}`);
    return;
  }
  byId('footer-engine').textContent = init.engine;
  emulator.applyPreferences(); // speed, background policy and visibility are known before the first session starts
  for (const problem of init.storage.problems || []) emulator.addNotice('warning', `Browser storage: ${problem}. The profile will use a fallback.`);
  if (init.storage.kind === 'memory') emulator.addNotice('warning', 'This browser provides no persistent storage here (private browsing?). Nothing, including the profile, will be kept after the page closes.');
  showScreen('entry');
  entry.show(init);
  setBadge('Ready', null);
}

main();
