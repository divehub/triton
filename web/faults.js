// CPU fault report: the page's reading of the per-board `faults: {main, handset}` of the state, each `{cfsr, hfsr, lockup}`
// (DESIGN 20.2).
//
// The engine reports it for every firmware, original or custom: CFSR and HFSR are the Cortex-M fault status registers as the
// guest reads them, and `lockup` is the reason the core is locked up (a fault inside a fault handler), or null when it is not.
// A healthy run has both registers at zero and no lockup, so the basic view shows a line only for a board that has something
// to report, and the Advanced details list both boards. An engine without the report (or a handset-only run, which has no
// main board) simply gives fewer entries.
//
// Accepted shapes: the state keys the report by board (`{main: {...}, handset: {...}}`; an `ngc-` prefix of a board name is
// ignored) or lists it (`[{board, cfsr, hfsr, lockup}]`); registers may be numbers or the text of a number; `lockup` may be a
// reason text, true, false or null.
//
// No DOM: the Node tests import it.

const BOARD_ORDER = ['main', 'handset'];
export const BOARD_NAMES = Object.freeze({ main: 'Main', handset: 'Handset' });

const hex = (value) => `0x${(value >>> 0).toString(16).padStart(8, '0')}`;

/** A register value as `{number, text}`: `number` is null when the value is missing or not a number (`text` then says so). */
function register(value) {
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0) return { number: value >>> 0, text: hex(value) };
  if (typeof value === 'string' && /^\s*(?:0x[0-9a-f]+|\d+)\s*$/i.test(value)) {
    const number = Number(value.trim());
    if (Number.isSafeInteger(number) && number >= 0 && number <= 0xffffffff) return { number, text: hex(number) };
  }
  return { number: null, text: typeof value === 'string' && value.trim() ? value.trim() : 'not reported' };
}

/** The lockup member as `{locked, reason}`: `locked` is null when the report does not say. */
function lockupOf(value) {
  if (value === true) return { locked: true, reason: null };
  if (typeof value === 'string' && value.trim()) return { locked: true, reason: value.trim() };
  if (value === false || value === null || (typeof value === 'string')) return { locked: false, reason: null };
  return { locked: null, reason: null };
}

/**
 * The reports of the state, main before handset:
 * `[{board, cfsr: {number, text}, hfsr: {number, text}, lockup: {locked, reason}, active}]` (empty without a `faults` member).
 * `active` is true for a nonzero register or a lockup.
 */
export function faultReports(state) {
  const faults = state && typeof state === 'object' ? state.faults : null;
  if (!faults || typeof faults !== 'object') return [];
  const entries = Array.isArray(faults)
    ? faults.filter((entry) => entry && typeof entry === 'object').map((entry) => [entry.board, entry])
    : Object.entries(faults).filter(([, entry]) => entry && typeof entry === 'object');
  const reports = [];
  for (const [name, entry] of entries) {
    const board = String(name).replace(/^ngc-/, '');
    if (!BOARD_ORDER.includes(board)) continue;
    const cfsr = register(entry.cfsr);
    const hfsr = register(entry.hfsr);
    const lockup = lockupOf(entry.lockup);
    reports.push({ board, cfsr, hfsr, lockup, active: lockup.locked === true || (cfsr.number !== null && cfsr.number !== 0) || (hfsr.number !== null && hfsr.number !== 0) });
  }
  return reports.sort((a, b) => BOARD_ORDER.indexOf(a.board) - BOARD_ORDER.indexOf(b.board));
}

/** The compact line of the Advanced details: both registers and the lockup (with the engine's reason). */
export function faultDetail(report) {
  const { locked, reason } = report.lockup;
  const lockup = locked === null ? 'lockup not reported' : locked ? `locked up${reason ? `: ${reason}` : ''}` : 'no lockup';
  return `CFSR ${report.cfsr.text} · HFSR ${report.hfsr.text} · ${lockup}`;
}

/** The brief basic-view warnings: one per board with a nonzero register or a lockup (none for a healthy state). */
export function faultWarnings(state) {
  return faultReports(state).filter((report) => report.active).map((report) => {
    const { locked, reason } = report.lockup;
    const what = locked ? `locked up${reason ? ` (${reason})` : ''}` : 'reported a fault';
    return {
      board: report.board,
      text: `${BOARD_NAMES[report.board]} CPU ${what}: CFSR ${report.cfsr.text}, HFSR ${report.hfsr.text}. Details: Advanced → Execution and model details.`,
    };
  });
}
