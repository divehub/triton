/**
 * Ideal-gas counterlung model of the dive game.
 *
 * The loop stays at 4 ambient liters and is perfectly mixed. All added/vented gas quantities and MAV flow use
 * surface-equivalent liters (SL), referenced to 1.01325 bar. The adjustable 100 SL/min default is an arbitrary game
 * setting, not a measurement of a real MAV. No net oxygen consumption is modeled: CMF is assumed to compensate
 * consumption perfectly.
 *
 * A step follows a linear depth trajectory. MAV gas fills compression first; ADV adds the remaining diluent deficit.
 * Excess gas vents with the current loop composition. Mixing is integrated analytically, including changing
 * pressure, so one long step agrees with equivalent smaller steps.
 *
 * This module is the loop physics only. The oxygen-cell voltages the game sends to the emulated firmware are made in
 * game-logic.js (a labeled game fixture); the model has no cells of its own. No DOM, no engine: the Node tests import it.
 */

export const LOOP_VOLUME_LITERS = 4;
export const SURFACE_PRESSURE_BAR = 1.01325;
export const WATER_DENSITY_KG_M3 = 1020;
export const GRAVITY_M_S2 = 9.80665;
export const DEFAULT_MAV_FLOW_SL_MIN = 100;
export const MAX_DEPTH_METERS = 110;

const GASES = ['o2', 'n2', 'he'];

function finiteNumber(value, name, minimum = 0, maximum = Infinity) {
  if (!Number.isFinite(value) || value < minimum || value > maximum) {
    throw new RangeError(`${name} must be finite and between ${minimum} and ${maximum}`);
  }
  return value;
}

function validateDepth(depth) {
  return finiteNumber(depth, 'Depth', 0, MAX_DEPTH_METERS);
}

function validateMix(mix) {
  if (!mix || typeof mix !== 'object') {
    throw new TypeError('Diluent must contain oxygen and optional helium fractions');
  }
  const o2 = finiteNumber(mix.o2, 'Oxygen fraction', 0, 1);
  const he = finiteNumber(mix.he ?? 0, 'Helium fraction', 0, 1);
  if (o2 + he > 1) {
    throw new RangeError('Oxygen and helium fractions must total at most 1');
  }
  return { o2, n2: Math.max(0, 1 - (o2 + he)), he };
}

export function pressureAtDepth(depth) {
  validateDepth(depth);
  return SURFACE_PRESSURE_BAR + WATER_DENSITY_KG_M3 * GRAVITY_M_S2 * depth / 100000;
}

function inventoryAtPressure(pressure) {
  return LOOP_VOLUME_LITERS * pressure / SURFACE_PRESSURE_BAR;
}

function emptyActivity() {
  return { adv: 0, vent: 0, oxygen: 0, diluent: 0 };
}

function fillLoop(loop, mix, depth) {
  const pressure = pressureAtDepth(depth);
  const inventory = inventoryAtPressure(pressure);
  loop.depth = depth;
  loop.pressure = pressure;
  loop.diluent = mix;
  loop.gas = Object.fromEntries(GASES.map(gas => [gas, inventory * mix[gas]]));
  loop.last = emptyActivity();
  loop.totals = emptyActivity();
  return loop;
}

export function createLoop({ depth = 0, diluent = { o2: 0.21, he: 0 } } = {}) {
  return fillLoop({}, validateMix(diluent), validateDepth(depth));
}

/**
 * Selecting a diluent switches the supply only, as turning to another diluent cylinder would: the gas already in the
 * counterlungs stays, and the new mix enters with the next ADV addition (a descent) or diluent MAV.
 */
export function setDiluent(loop, mix) {
  const validMix = validateMix(mix);
  inspectLoop(loop);
  loop.diluent = validMix;
  return loop;
}

function inspectLoop(loop) {
  if (!loop || typeof loop !== 'object' || !loop.gas || !loop.diluent) {
    throw new TypeError('Expected a loop created with createLoop');
  }
  const pressure = pressureAtDepth(loop.depth);
  const inventory = GASES.reduce((total, gas) => total + finiteNumber(loop.gas[gas], `${gas} inventory`), 0);
  const requiredInventory = inventoryAtPressure(pressure);
  if (Math.abs(inventory - requiredInventory) > requiredInventory * 1e-9) {
    throw new RangeError('Loop gas inventory does not match its depth and fixed volume');
  }
  validateMix(loop.diluent);
  return { pressure, inventory };
}

/**
 * Advance by virtual seconds. Depth must remain in 0–110 m; flow is SL/min
 * per held MAV. dt=0 freezes depth, gas, and activity as a complete no-op.
 */
export function advanceLoop(loop, {
  depth = loop.depth,
  dt = 0,
  oxygen = false,
  diluent = false,
  flow = DEFAULT_MAV_FLOW_SL_MIN,
} = {}) {
  const current = inspectLoop(loop);
  validateDepth(depth);
  finiteNumber(dt, 'Virtual time step');
  finiteNumber(flow, 'MAV flow');
  if (typeof oxygen !== 'boolean' || typeof diluent !== 'boolean') {
    throw new TypeError('MAV activation flags must be boolean');
  }
  if (dt === 0) return loop;

  const pressure = pressureAtDepth(depth);
  const nextInventory = inventoryAtPressure(pressure);
  const change = nextInventory - current.inventory;
  const oxygenAdded = oxygen ? flow * dt / 60 : 0;
  const diluentAdded = diluent ? flow * dt / 60 : 0;
  const mavAdded = oxygenAdded + diluentAdded;
  if (!Number.isFinite(mavAdded)) {
    throw new RangeError('MAV gas quantity exceeds the finite model range');
  }
  const advAdded = Math.max(0, change - mavAdded);
  const vented = Math.max(0, mavAdded - change);
  const diluentInflow = diluentAdded + advAdded;
  const totalInflow = oxygenAdded + diluentInflow;

  // For inventory N(t) linear in time, the dilution exponent is
  // I / ΔN × log(N1 / N0), with I the integrated input quantity.
  // log1p and the constant-volume limit avoid cancellation near ΔN = 0.
  const relativeChange = change / current.inventory;
  const integral = Math.abs(relativeChange) < 1e-12
    ? 1 / current.inventory
    : Math.log1p(relativeChange) / change;
  const replacement = -Math.expm1(-totalInflow * integral);
  const nextGas = {};
  for (const gas of GASES) {
    const initialFraction = loop.gas[gas] / current.inventory;
    const incomingFraction = totalInflow === 0 ? initialFraction
      : ((gas === 'o2' ? oxygenAdded : 0) + diluentInflow * loop.diluent[gas]) / totalInflow;
    const finalFraction = initialFraction + (incomingFraction - initialFraction) * replacement;
    nextGas[gas] = nextInventory * finalFraction;
  }

  // Keep arithmetic drift out of the conserved gas total.
  const sum = GASES.reduce((total, gas) => total + nextGas[gas], 0);
  for (const gas of GASES) nextGas[gas] *= nextInventory / sum;
  loop.gas = nextGas;
  loop.depth = depth;
  loop.pressure = pressure;
  loop.last = { adv: advAdded, vent: vented, oxygen: oxygenAdded, diluent: diluentAdded };
  for (const key of Object.keys(loop.last)) loop.totals[key] += loop.last[key];
  return loop;
}

/** Return a detached snapshot; pressure and ppO₂ are in bar (at the model's own surface reference, see game-logic.js). */
export function getLoopReadings(loop) {
  const { pressure, inventory } = inspectLoop(loop);
  const fractions = Object.fromEntries(GASES.map(gas => [gas, loop.gas[gas] / inventory]));
  const ppo2 = fractions.o2 * pressure;
  return {
    depth: loop.depth,
    pressure,
    fractions,
    ppo2,
    volume: inventory * SURFACE_PRESSURE_BAR / pressure,
    inventory,
    adv: loop.last.adv,
    vent: loop.last.vent,
    injected: { oxygen: loop.last.oxygen, diluent: loop.last.diluent },
    totals: { ...loop.totals },
  };
}
