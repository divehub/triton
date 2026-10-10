// Decompression state: the page's reading of the engine's read-only `decoHealth` report, and the surface-pressure setting.
//
// The original TRITON main firmware keeps its tissue pressures in RAM and loads them from the EEPROM at start-up. It saves
// them only on power-down, so a profile that was booted once and restarted holds a decompression date but no tissues; the
// next start loads 32 erased words as NaN and the no-decompression limit stays at 99. With the oxygen cells uncalibrated in
// the measured-ppO2 mode its ppO2 is NaN and the limit stays at 99 as well. The engine reports both (`decoHealth`, from
// side-effect-free peeks). Two labeled emulator fixtures keep the first problem away from new profiles: the EEPROM factory image
// (no option) fills the records the firmware never initializes, among them the tissue block, when a new EEPROM is created
// (`eepromFactoryInit`), and the start at the surface (`startAtSurface`, on by default) starts every board creation at depth 0.
// An older profile whose stored tissues are blank is not repaired: the report says so and the next step is to reset the profile.
// Nothing here changes the guest.
//
// No DOM: the Node tests import it.

/** The surface pressure of the basic view, in mbar, and the range the engine accepts for it. */
export const DEFAULT_SURFACE_MBAR = 1013.25;
export const SURFACE_LIMITS = Object.freeze([100, 30000]);

/** The route through the handset's menu to a calibration, as the warning names it. */
export const CALIBRATION_ROUTE = 'Menu → Calibration → Air → Auto → Start → Save';

/**
 * The warning for stored tissues that are not finite. A new EEPROM is created with valid tissues, so this is an older profile whose
 * stored tissue block was never written; the engine does not repair an existing EEPROM, and a profile reset creates an initialized
 * one. The route names the page's controls: Advanced, the "Profile and evidence" section, the "Reset profile…" button.
 */
export const INVALID_TISSUES_WARNING =
  "Decompression state invalid: this profile's stored tissues are blank. Reset the profile (Advanced → Profile and evidence → Reset profile) to start with an initialized EEPROM.";

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
    warnings.push({ id: 'tissues', text: INVALID_TISSUES_WARNING });
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

/** Lines for the session information about the two labeled emulator fixtures (empty for an engine without them). */
export function fixtureLines(state) {
  const lines = [];
  // The EEPROM factory image: whether this session created its EEPROM from it (an existing EEPROM is never touched).
  const factory = state && state.eepromFactoryInit;
  if (factory && typeof factory === 'object') {
    const status = factory.applied ? 'this session created the EEPROM from it' : 'not applied';
    lines.push(`Fixture: EEPROM factory image (${status}). ${factory.reason || ''}`.trim());
  }
  const surface = state && state.startAtSurface;
  if (surface && typeof surface === 'object') {
    const status = surface.enabled === false ? 'off' : `on, surface ${surface.surfacePressureMbar} mbar`;
    lines.push(`Fixture: start at the surface (${status}). ${surface.note || ''}`.trim());
  }
  return lines;
}
