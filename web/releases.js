// Known firmware releases and the browser-side naming derived from the engine's identification (DESIGN 15.3e).
//
// The engine reports `release: {id, label}` (or null) in the `ngc_firmware_inspect` JSON. This module adds what only
// the page needs: the display name, the file names the entry screen suggests and the storage area of the release's
// profile. TRITON keeps the storage location of the first version of this app ('profile'); every other release gets
// a directory of its own, so two releases never share EEPROM, log flash or clock state.
//
// Custom builds (DESIGN 20): the engine reports `release: {id: 'CUSTOM', label: 'Custom build'}` for them. They have no entry in
// the release table (no known file names, no release verification); this module only names them and gives them the one shared
// custom profile area and their own remembered-firmware area, so they never share storage with TRITON or NEPTUN.
//
// Pure ES module (no DOM, no worker globals): the Node tests import it.

export const DEFAULT_RELEASE_ID = 'TRITON-5.8-65.3';
export const CUSTOM_RELEASE_ID = 'CUSTOM';
/** Storage areas of the remembered firmware pair: the original releases share one, custom builds have their own. */
export const FIRMWARE_AREA = 'firmware';
export const CUSTOM_FIRMWARE_AREA = 'firmware-custom';
/** The one profile area shared by every custom build. */
export const CUSTOM_PROFILE_AREA = 'custom';

export const RELEASES = Object.freeze({
  'TRITON-5.8-65.3': Object.freeze({
    id: 'TRITON-5.8-65.3',
    name: 'TRITON',
    label: 'TRITON main 5.8 / handset 65.3',
    files: Object.freeze({ main: 'ngc_main_5.8_TRITON.srec', handset: 'ngc_handset_65.3_TRITON.srec' }),
  }),
  'NEPTUN-5.8-65.3': Object.freeze({
    id: 'NEPTUN-5.8-65.3',
    name: 'NEPTUN',
    label: 'NEPTUN main 5.8 / handset 65.3',
    files: Object.freeze({ main: 'ngc_main_5.8_NEPTUN.srec', handset: 'ngc_handset_65.3_NEPTUN.srec' }),
  }),
});

/** Ids of the releases this page knows, TRITON first. */
export const RELEASE_IDS = Object.freeze(Object.keys(RELEASES));

/** A storage-safe, stable fragment of a release id (letters, digits and dashes only). */
function slug(id) {
  return String(id).toLowerCase().replace(/[^a-z0-9.]+/g, '-').replace(/^-+|-+$/g, '').replace(/\./g, '_');
}

/** `{id, name, label}` for a release id; unknown ids (a newer engine) still get a usable record. A custom build adds `custom: true`. */
export function describeRelease(id, label) {
  if (id === CUSTOM_RELEASE_ID) return { id, name: 'Custom build', label: label || 'Custom build', custom: true };
  const known = RELEASES[id];
  if (known) return { id, name: known.name, label: label || known.label };
  const name = String(id).split('-')[0] || String(id);
  return { id: String(id), name, label: label || String(id) };
}

/**
 * The release of an `inspect` report. A report without a `release` member comes from an engine that predates the
 * release table: it only ever accepted the TRITON pair. `null` (reported explicitly) means "not a known release".
 */
export function releaseOf(report) {
  if (!report || !report.ok) return null;
  if (report.release === undefined) return describeRelease(DEFAULT_RELEASE_ID);
  if (!report.release || typeof report.release.id !== 'string') return null;
  return describeRelease(report.release.id, typeof report.release.label === 'string' ? report.release.label : undefined);
}

/** Storage area of a release's profile ('profile' for TRITON, as in the first version of the app; 'custom' for custom builds). */
export function profileArea(id) {
  if (id === CUSTOM_RELEASE_ID) return CUSTOM_PROFILE_AREA;
  return id === DEFAULT_RELEASE_ID ? 'profile' : `profile-${slug(id)}`;
}

/** The area of the remembered firmware pair for original releases (`custom` false) or custom builds. */
export function firmwareArea(custom) {
  return custom ? CUSTOM_FIRMWARE_AREA : FIRMWARE_AREA;
}

/** Every storage area the page can use: the remembered firmware and profile areas of the known releases, then the custom ones. */
export function storageAreas() {
  return [FIRMWARE_AREA, ...RELEASE_IDS.map(profileArea), CUSTOM_FIRMWARE_AREA, CUSTOM_PROFILE_AREA];
}

/** `{main, handset}` of the slots that hold a different release each, or null when they agree (or one is empty). */
export function pairConflict(slots) {
  const main = slots && slots.main && slots.main.release;
  const handset = slots && slots.handset && slots.handset.release;
  if (!main || !handset || main.id === handset.id) return null;
  return { main, handset };
}

/**
 * The refusal shown when `incoming` (a role + release) does not match the file already held for the other role.
 * The entry screen prefixes the file name, so the sentence starts with "This is".
 */
export function mixedPairMessage({ role, release }, otherRole, other) {
  const roleText = { main: 'main controller 5.8', handset: 'handset 65.3' };
  return `this is the ${release.name} ${roleText[role]} image, but the ${roleText[otherRole]} file provided is ${other.release.name}. `
    + `Main and handset must come from the same release: remove the ${roleText[otherRole]} file first to switch releases.`;
}
