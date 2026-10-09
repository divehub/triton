// Simulated conditions: the pure arithmetic behind the basic sensor controls.
//
// Ported from the `sensor-controls` script of the analysis workspace's emulation/viewer.html (module form, same API). The basic view
// edits one oxygen base voltage, a surface pressure with a depth and a water type, and one temperature, each with
// signed per-sensor offsets; this module turns those into the seven raw model inputs and back. These are input
// fixtures for the local sensor models, not a claim about physical sensor accuracy. Firmware filtering and stored
// calibration still determine what the handset displays.
//
// No DOM, no engine: the Node tests import it.

// User-selected scenario fixtures. Hydrostatic gauge pressure is rho*g*h; divide pascals by 100 to obtain mbar,
// then add absolute surface pressure.
export const WATER_DENSITIES = Object.freeze({ fresh: 1000, salt: 1025, en13319: 1020 });
export const GRAVITY = 9.80665;
export const BASE_LIMITS = Object.freeze({
  oxygenBaseMv: Object.freeze([0, 100]),
  surfacePressureMbar: Object.freeze([100, 30000]),
  depthM: Object.freeze([0, 110]),
  temperatureBaseC: Object.freeze([-4, 40]),
});
export const RAW_LIMITS = Object.freeze({
  oxygen: Object.freeze([0, 250]),
  pressure: Object.freeze([100, 30000]),
  temperature: Object.freeze([-20, 85]),
});

/** The seven calculated model inputs; the engine's `inputs` object carries these and the seven other fields. */
export const SENSOR_KEYS = Object.freeze(['oxygen1Mv', 'oxygen2Mv', 'oxygen3Mv', 'pressure1Mbar', 'pressure2Mbar', 'temperature1C', 'temperature2C']);

// Pressure and temperature sensor numbering. The page uses the firmware's own numbering of the two MS5837 sensors, which is
// the reverse of the engine's input keys (the Renode model names, kept unchanged so `inputs.json` and the command-line
// scripts stay compatible): the firmware's sensor 1 is the device on I2C2 (engine keys `pressure2Mbar` and
// `temperature2C`), its sensor 2 the device on I2C1 (`pressure1Mbar` and `temperature1C`). The basic settings keep
// their sensor offsets in page order, [sensor 1, sensor 2], and these tables map them to the engine keys.
export const PRESSURE_KEYS = Object.freeze(['pressure2Mbar', 'pressure1Mbar']);
export const TEMPERATURE_KEYS = Object.freeze(['temperature2C', 'temperature1C']);
/** The bus each page sensor (index 0 is sensor 1) sits on. */
export const SENSOR_BUSES = Object.freeze(['I2C2', 'I2C1']);

// Sums that land a few ulps outside a limit only because base + (raw - base) is not exact in floating point are
// snapped to the limit; anything visibly outside is rejected (the readings are never clipped).
const SNAP = 1e-9;

export function defaults() {
  return {
    oxygenBaseMv: 10, oxygenVariationsMv: [0, 0, 0],
    surfacePressureMbar: 1013.25, depthM: 0, waterType: 'en13319',
    pressureVariationsMbar: [0, 0],
    temperatureBaseC: 20, temperatureVariationsC: [0, 0],
  };
}

function number(value, name, limits) {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new RangeError(`${name} must be a finite number`);
  }
  if (limits) {
    if (value < limits[0] || value > limits[1]) {
      const edge = value < limits[0] ? limits[0] : limits[1];
      if (Math.abs(value - edge) <= SNAP * Math.max(1, Math.abs(edge))) return edge;
      throw new RangeError(`${name} must be between ${limits[0]} and ${limits[1]}`);
    }
  }
  return value;
}

function variations(values, count, name) {
  if (!Array.isArray(values) || values.length !== count) {
    throw new RangeError(`${name} must contain ${count} sensor variations`);
  }
  return Array.from(values, (value, index) => number(value, `${name}[${index}]`));
}

function density(waterType) {
  if (typeof waterType !== 'string' || !Object.prototype.hasOwnProperty.call(WATER_DENSITIES, waterType)) {
    throw new RangeError('waterType must be fresh, salt or en13319');
  }
  return WATER_DENSITIES[waterType];
}

/** Absolute pressure (mbar) at `depthM` meters: surface + density * g * depth / 100. */
export function pressureMbar(surfacePressureMbar, depthM, waterType) {
  const surface = number(surfacePressureMbar, 'surfacePressureMbar', BASE_LIMITS.surfacePressureMbar);
  const depth = number(depthM, 'depthM', BASE_LIMITS.depthM);
  return surface + density(waterType) * GRAVITY * depth / 100;
}

/** Basic settings to the seven raw inputs. Throws a RangeError (naming the offending value) for anything invalid. */
export function calculate(settings) {
  if (!settings || typeof settings !== 'object') throw new RangeError('Sensor settings are required');
  const oxygen = number(settings.oxygenBaseMv, 'oxygenBaseMv', BASE_LIMITS.oxygenBaseMv);
  const oxygenOffsets = variations(settings.oxygenVariationsMv, 3, 'oxygenVariationsMv');
  const pressure = pressureMbar(settings.surfacePressureMbar, settings.depthM, settings.waterType);
  const pressureOffsets = variations(settings.pressureVariationsMbar, 2, 'pressureVariationsMbar');
  const temperature = number(settings.temperatureBaseC, 'temperatureBaseC', BASE_LIMITS.temperatureBaseC);
  const temperatureOffsets = variations(settings.temperatureVariationsC, 2, 'temperatureVariationsC');
  const result = {};
  oxygenOffsets.forEach((offset, index) => {
    const key = `oxygen${index + 1}Mv`;
    result[key] = number(oxygen + offset, key, RAW_LIMITS.oxygen);
  });
  pressureOffsets.forEach((offset, index) => {
    const key = PRESSURE_KEYS[index];
    result[key] = number(pressure + offset, key, RAW_LIMITS.pressure);
  });
  temperatureOffsets.forEach((offset, index) => {
    const key = TEMPERATURE_KEYS[index];
    result[key] = number(temperature + offset, key, RAW_LIMITS.temperature);
  });
  return result;
}

function average(values) {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function clamp(value, limits) {
  return Math.max(limits[0], Math.min(limits[1], value));
}

/**
 * The raw readings to basic settings (bases are the sensor means, bounded to the slider ranges; the offsets keep
 * every reading exactly). `previous` supplies the selected surface pressure and water type, which the raw
 * readings cannot tell. Bases and offsets are derived, never stored separately.
 */
export function fromInputs(raw, previous) {
  if (!raw || typeof raw !== 'object') throw new RangeError('Raw sensor inputs are required');
  const result = defaults();
  if (previous && previous.surfacePressureMbar !== undefined) {
    result.surfacePressureMbar = number(previous.surfacePressureMbar, 'surfacePressureMbar', BASE_LIMITS.surfacePressureMbar);
  }
  if (previous && previous.waterType !== undefined) {
    density(previous.waterType);
    result.waterType = previous.waterType;
  }
  const oxygen = [1, 2, 3].map((index) => number(raw[`oxygen${index}Mv`], `oxygen${index}Mv`, RAW_LIMITS.oxygen));
  const pressures = PRESSURE_KEYS.map((key) => number(raw[key], key, RAW_LIMITS.pressure));
  const temperatures = TEMPERATURE_KEYS.map((key) => number(raw[key], key, RAW_LIMITS.temperature));
  result.oxygenBaseMv = clamp(average(oxygen), BASE_LIMITS.oxygenBaseMv);
  result.oxygenVariationsMv = oxygen.map((value) => value - result.oxygenBaseMv);
  result.depthM = clamp((average(pressures) - result.surfacePressureMbar) * 100 /
    (density(result.waterType) * GRAVITY), BASE_LIMITS.depthM);
  const pressure = pressureMbar(result.surfacePressureMbar, result.depthM, result.waterType);
  result.pressureVariationsMbar = pressures.map((value) => value - pressure);
  result.temperatureBaseC = clamp(average(temperatures), BASE_LIMITS.temperatureBaseC);
  result.temperatureVariationsC = temperatures.map((value) => value - result.temperatureBaseC);
  return result;
}

/** Names shown to the user in place of the internal value names in a validation message (the sensors by page numbering). */
export const FRIENDLY_NAMES = Object.freeze({
  oxygenBaseMv: 'Oxygen base', oxygen1Mv: 'Oxygen cell 1', oxygen2Mv: 'Oxygen cell 2', oxygen3Mv: 'Oxygen cell 3',
  surfacePressureMbar: 'Surface pressure', depthM: 'Depth', temperatureBaseC: 'Base temperature',
  pressure2Mbar: 'Pressure sensor 1', pressure1Mbar: 'Pressure sensor 2',
  temperature2C: 'Temperature sensor 1', temperature1C: 'Temperature sensor 2',
});

/** The "Not applied" explanation for a validation error: friendly names, one sentence, and what happened. */
export function explainRejection(message) {
  let text = String(message);
  for (const [name, label] of Object.entries(FRIENDLY_NAMES)) text = text.split(name).join(label);
  return `${text.replace(/\.$/, '')}. Changes have not been applied.`;
}

export const WATER_TYPES = Object.freeze([['fresh', 'Fresh'], ['salt', 'Salt'], ['en13319', 'EN13319']]);
