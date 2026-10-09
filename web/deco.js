// Decompression state: the page's reading of the engine's read-only `decoHealth` report, and the surface-pressure setting.
//
// The original TRITON main firmware keeps its tissue pressures in RAM and loads them from the EEPROM at start-up. It saves
// them only on power-down, so a profile that was booted once and restarted holds a decompression date but no tissues; the
// next start loads 32 erased words as NaN and the no-decompression limit stays at 99. With the oxygen cells uncalibrated in
// the measured-ppO2 mode its ppO2 is NaN and the limit stays at 99 as well. The engine reports both (`decoHealth`, from
// side-effect-free peeks) and, with two labelled emulator fixtures that are on by default, repairs the first before a start
// (`decoStorageFixture`) and starts every board creation at the surface (`startAtSurface`). Nothing here changes the guest.
//
// No DOM: the Node tests import it.

/** The surface pressure of the basic view, in mbar, and the range the engine accepts for it. */
export const DEFAULT_SURFACE_MBAR = 1013.25;
export const SURFACE_LIMITS = Object.freeze([100, 30000]);

/** The route through the handset's menu to a calibration, as the warning names it. */
export const CALIBRATION_ROUTE = 'Menu → Calibration → Air → Auto → Start → Save';

/**
 * A surface pressure setting (a number or the text of a field or of a stored preference) as a finite number inside
 * the engine's range, else null. A blank text is not zero.
 */
export function parseSurfacePressure(value) {
  if (value === undefined || value === null) return null;
  const text = typeof value === 'string' ? value.trim() : value;
  if (text === '') return null;
  const number = typeof text === 'number' ? text : Number(text);
  return Number.isFinite(number) && number >= SURFACE_LIMITS[0] && number <= SURFACE_LIMITS[1] ? number : null;
}

/**
 * The warnings to show for a state document: `[{id: 'tissues', text}]` (at most one). Only a proven bad state warns
 * (`unknown` never does, which is what NEPTUN, a handset-only run and an engine without the report give). The text names
 * the next step.
 */
export function decoWarnings(state) {
  const health = state && typeof state === 'object' ? state.decoHealth : null;
  if (!health || typeof health !== 'object') return [];
  const warnings = [];
  // An uncalibrated oxygen cell is not shown as a warning (user decision): the firmware's own calibration prompt covers
  // it. The report still carries it (`decoHealth.oxygen`) for the advanced details.
  if (health.tissues === 'invalid') {
    const fixture = state.decoStorageFixture;
    const off = !!fixture && fixture.enabled === false;
    warnings.push({
      id: 'tissues',
      text: off
        ? 'Decompression state invalid: the repair fixture is off. Close the session, tick "Repair the stored decompression state" under Start options and boot again.'
        : 'Decompression state invalid: restart the boards to let the firmware reset it.',
    });
  }
  return warnings;
}

/** A line for the session information: what the read-only report says, with the reason when it is unknown. */
export function healthLine(state) {
  const health = state && state.decoHealth;
  if (!health || typeof health !== 'object') return null;
  const details = health.details && typeof health.details === 'object' ? health.details : {};
  const parts = [`tissues ${health.tissues}`, `oxygen ${health.oxygen}`];
  const reasons = [];
  if (health.tissues === 'unknown' && typeof details.tissues === 'string') reasons.push(details.tissues);
  if (health.oxygen === 'unknown' && typeof details.oxygen === 'string') reasons.push(details.oxygen);
  return `Decompression state (read-only report): ${parts.join(', ')}.${reasons.length ? ` ${reasons.join(' ')}` : ''}`;
}

/** Lines for the session information about the two labelled emulator fixtures (empty for an engine without them). */
export function fixtureLines(state) {
  const lines = [];
  const storage = state && state.decoStorageFixture;
  if (storage && typeof storage === 'object') {
    const status = storage.enabled === false ? 'off' : storage.applied ? 'applied at the last start' : 'on, not needed at the last start';
    lines.push(`Fixture: stored decompression state repair (${status}). ${storage.reason || ''}`.trim());
  }
  const surface = state && state.startAtSurface;
  if (surface && typeof surface === 'object') {
    const status = surface.enabled === false ? 'off' : `on, surface ${surface.surfacePressureMbar} mbar`;
    lines.push(`Fixture: start at the surface (${status}). ${surface.note || ''}`.trim());
  }
  return lines;
}
