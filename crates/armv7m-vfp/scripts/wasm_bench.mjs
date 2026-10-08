#!/usr/bin/env node
// Throughput of armv7m-vfp `execute` under WebAssembly (V8), per instruction.
//
//   ./cargo build -p armv7m-vfp --release --target wasm32-unknown-unknown \
//       --example wasm_selftest --target-dir target/fpu
//   node crates/armv7m-vfp/scripts/wasm_bench.mjs target/fpu/wasm32-unknown-unknown/release/examples/wasm_selftest.wasm [iters]
import { readFileSync } from 'node:fs';

const path = process.argv[2];
if (!path) {
  console.error('usage: wasm_bench.mjs <wasm_selftest.wasm> [iterations]');
  process.exit(2);
}
const iters = process.argv[3] ? Number(process.argv[3]) : 20_000_000;
const { instance } = await WebAssembly.instantiate(readFileSync(path), {});
const bench = instance.exports.vfp_bench;

const names = [
  'operand feed only', 'vadd.f32', 'vmul.f32', 'vdiv.f32', 'vsqrt.f32', 'vfma.f32', 'vmla.f32',
  'vcmp.f32', 'vcvt.f32.s32', 'vcvt.s32.f32', 'vcvt.s32.f32 #fbits', 'vldr.32', 'vstr.32',
  'vmov.f32 s,s', 'vpush {d8}', 'vpop {d8}', 'exact vadd (RP)',
];

function time(kind) {
  bench(kind, Math.floor(iters / 10)); // warm up / tier up
  const t0 = performance.now();
  const sum = bench(kind, iters);
  const ns = ((performance.now() - t0) * 1e6) / iters;
  return { ns, sum };
}

const base = time(0).ns;
console.log(`operand feed overhead ${base.toFixed(2)} ns/iteration (subtracted)`);
console.log('instruction'.padEnd(24), 'ns/op'.padStart(8), 'Mop/s'.padStart(8));
for (let k = 1; k < names.length; k++) {
  const r = time(k);
  if (r.sum === 0xFFFFFFFF) {
    console.log(names[k].padEnd(24), 'decode failed');
    continue;
  }
  const ns = Math.max(r.ns - base, 0.01);
  console.log(names[k].padEnd(24), ns.toFixed(2).padStart(8), (1000 / ns).toFixed(1).padStart(8));
}
