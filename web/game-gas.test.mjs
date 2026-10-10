// Mass-balance checks of the dive game's counterlung model (game-gas.js).
//
//     node --test web/game-gas.test.mjs
//
// Ported from the prototype's gas-model.test.mjs. The checks cover constant volume, ADV mixing, proportional ascent
// venting, continuous injection, combined valves, pause, invalid inputs and agreement between one large step and subdivided
// steps. The prototype's three identical mock cell voltages are gone: the cells are a fixture of game-logic.js (tested in
// test-ui.mjs), the model only reports the loop.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  createLoop, setDiluent, advanceLoop, getLoopReadings, pressureAtDepth,
  LOOP_VOLUME_LITERS, SURFACE_PRESSURE_BAR,
} from './game-gas.js';

function near(actual, expected, tolerance = 1e-11) {
  assert.ok(Math.abs(actual - expected) <= tolerance,
    `Expected ${actual} to be within ${tolerance} of ${expected}`);
}

test('surface loop contains four ambient liters of air and reports no cell voltages', () => {
  const readings = getLoopReadings(createLoop());
  near(readings.pressure, SURFACE_PRESSURE_BAR);
  near(readings.volume, 4);
  near(readings.inventory, 4);
  near(readings.fractions.o2, 0.21);
  near(readings.fractions.n2, 0.79);
  near(readings.ppo2, 0.21 * SURFACE_PRESSURE_BAR);
  assert.equal('sensorMillivolts' in readings, false, 'the cells are a fixture of the game, not of the gas model');
});

test('oxygen MAV injection uses exponential mixing with matching venting', () => {
  const loop = createLoop();
  advanceLoop(loop, { dt: 60, oxygen: true, flow: 5 });
  const readings = getLoopReadings(loop);
  near(readings.fractions.o2, 1 - 0.79 * Math.exp(-5 / 4));
  near(readings.injected.oxygen, 5);
  near(readings.vent, 5);
  near(readings.adv, 0);
  near(readings.volume, LOOP_VOLUME_LITERS);
});

test('descent ADV adds diluent to the compression deficit', () => {
  const loop = createLoop();
  advanceLoop(loop, { dt: 30, oxygen: true, flow: 5 });
  const before = getLoopReadings(loop);
  advanceLoop(loop, { depth: 40, dt: 120 });
  const after = getLoopReadings(loop);
  const added = after.inventory - before.inventory;
  near(after.adv, added);
  near(after.vent, 0);
  near(after.fractions.o2,
    (before.inventory * before.fractions.o2 + added * 0.21) / after.inventory);
  assert.ok(after.fractions.o2 < before.fractions.o2);
  near(after.volume, 4);
});

test('ascent vents proportionally and preserves every gas fraction', () => {
  const loop = createLoop({ depth: 60, diluent: { o2: 0.18, he: 0.45 } });
  advanceLoop(loop, { dt: 40, oxygen: true });
  const before = getLoopReadings(loop);
  advanceLoop(loop, { depth: 5, dt: 300 });
  const after = getLoopReadings(loop);
  for (const gas of ['o2', 'n2', 'he']) near(after.fractions[gas], before.fractions[gas]);
  near(after.vent, before.inventory - after.inventory);
  near(after.adv, 0);
  near(after.volume, 4);
});

test('diluent MAV flush moves an enriched loop back toward the selected mix', () => {
  const loop = createLoop({ depth: 20 });
  advanceLoop(loop, { dt: 60, oxygen: true, flow: 5 });
  const before = getLoopReadings(loop);
  advanceLoop(loop, { dt: 60, diluent: true, flow: 5 });
  const after = getLoopReadings(loop);
  near(after.fractions.o2, 0.21 + (before.fractions.o2 - 0.21) * Math.exp(-5 / before.inventory));
  near(after.injected.diluent, 5);
  near(after.vent, 5);
});

test('MAV fills descent compression first and ADV supplies only the remainder', () => {
  const loop = createLoop();
  const newInventory = 4 * pressureAtDepth(10) / SURFACE_PRESSURE_BAR;
  advanceLoop(loop, { depth: 10, dt: 30, oxygen: true, flow: 2 });
  const readings = getLoopReadings(loop);
  near(readings.injected.oxygen, 1);
  near(readings.adv, newInventory - 4 - 1);
  near(readings.vent, 0);
  near(readings.fractions.o2, (4 * 0.21 + 1 + readings.adv * 0.21) / newInventory);
});

test('one long step agrees with many short steps on a linear depth trajectory', () => {
  for (const [startDepth, endDepth] of [[0, 80], [80, 0], [40, 40]]) {
    const whole = createLoop({ depth: startDepth, diluent: { o2: 0.18, he: 0.45 } });
    const sliced = createLoop({ depth: startDepth, diluent: { o2: 0.18, he: 0.45 } });
    advanceLoop(whole, { depth: endDepth, dt: 240, oxygen: true, diluent: true, flow: 5 });
    const slices = 1000;
    for (let step = 1; step <= slices; step++) {
      advanceLoop(sliced, {
        depth: startDepth + (endDepth - startDepth) * step / slices,
        dt: 240 / slices, oxygen: true, diluent: true, flow: 5,
      });
    }
    const expected = getLoopReadings(whole);
    const actual = getLoopReadings(sliced);
    for (const gas of ['o2', 'n2', 'he']) near(actual.fractions[gas], expected.fractions[gas]);
    for (const key of ['adv', 'vent', 'oxygen', 'diluent']) near(actual.totals[key], expected.totals[key], 1e-9);
    near(actual.volume, 4);
    near(actual.fractions.o2 + actual.fractions.n2 + actual.fractions.he, 1);
  }
});

test('selecting a diluent switches the supply only: the loop keeps its gas until ADV or the diluent MAV brings the new mix', () => {
  const loop = createLoop({ depth: 30 });
  advanceLoop(loop, { dt: 60, oxygen: true });
  const before = getLoopReadings(loop);
  assert.equal(setDiluent(loop, { o2: 0.18, he: 0.45 }), loop);
  assert.deepEqual(getLoopReadings(loop), before, 'the counterlungs, the activity and the totals are untouched');

  advanceLoop(loop, { dt: 60 });
  assert.deepEqual(getLoopReadings(loop).fractions, before.fractions, 'holding depth adds no gas');

  // A descent: ADV adds the compression deficit, all of it the new diluent, and nothing vents.
  advanceLoop(loop, { depth: 40, dt: 20 });
  const deeper = getLoopReadings(loop);
  const added = deeper.inventory - before.inventory;
  near(deeper.adv, added);
  near(deeper.vent, 0);
  near(deeper.fractions.he, 0.45 * added / deeper.inventory);
  near(deeper.fractions.o2, (before.fractions.o2 * before.inventory + 0.18 * added) / deeper.inventory);

  // A long diluent MAV flushes the loop to the new mix.
  advanceLoop(loop, { dt: 600, diluent: true });
  const flushed = getLoopReadings(loop);
  near(flushed.fractions.o2, 0.18, 1e-9);
  near(flushed.fractions.he, 0.45, 1e-9);
  near(flushed.volume, 4);
});

test('paused virtual time changes neither gas nor depth', () => {
  const loop = createLoop({ depth: 20 });
  const before = structuredClone(loop);
  advanceLoop(loop, { depth: 40, dt: 0, oxygen: true, diluent: true });
  assert.deepEqual(loop, before);
});

test('snapshots do not expose mutable loop fractions or totals', () => {
  const loop = createLoop();
  const readings = getLoopReadings(loop);
  readings.fractions.o2 = 1;
  readings.totals.oxygen = 500;
  near(getLoopReadings(loop).fractions.o2, 0.21);
  near(getLoopReadings(loop).totals.oxygen, 0);
});

test('invalid inputs throw before any loop mutation', () => {
  const loop = createLoop();
  const before = structuredClone(loop);
  for (const options of [
    { depth: -1, dt: 1 }, { depth: 111, dt: 1 }, { depth: NaN, dt: 1 },
    { dt: -1 }, { dt: Infinity }, { dt: 1, flow: -5 }, { dt: 1, flow: NaN },
    { dt: 1, oxygen: 'yes' }, { dt: Number.MAX_VALUE, flow: Number.MAX_VALUE, oxygen: true },
  ]) {
    assert.throws(() => advanceLoop(loop, options));
    assert.deepEqual(loop, before);
  }
  for (const mix of [{ o2: -0.1 }, { o2: 1.1 }, { o2: NaN }, { o2: 0.7, he: 0.4 }]) {
    assert.throws(() => setDiluent(loop, mix));
    assert.deepEqual(loop, before);
  }
});
