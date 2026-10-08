#!/usr/bin/env node
// Runs the armv7m-vfp determinism self-test inside V8 (WebAssembly) and checks
// it against the native reference checksum (tests/selftest_reference.rs).
//
//   ./cargo build -p armv7m-vfp --release --target wasm32-unknown-unknown \
//       --example wasm_selftest --target-dir target/fpu
//   node crates/armv7m-vfp/scripts/wasm_selftest.mjs target/fpu/wasm32-unknown-unknown/release/examples/wasm_selftest.wasm
import { readFileSync } from 'node:fs';

const SEED = 0x123456789ABCDEF0n;
const REFERENCE_ROUNDS = 200000;
const CHECKSUM = 0xaa5423a2bbce36cen; // keep in sync with tests/selftest_reference.rs

const path = process.argv[2];
if (!path) {
  console.error('usage: wasm_selftest.mjs <wasm_selftest.wasm> [rounds]');
  process.exit(2);
}
// With a custom round count only the fast-path/exact-core agreement is checked
// (the reference checksum belongs to the default round count).
const ROUNDS = process.argv[3] ? Number(process.argv[3]) : REFERENCE_ROUNDS;
const compareChecksum = ROUNDS === REFERENCE_ROUNDS;
const { instance } = await WebAssembly.instantiate(readFileSync(path), {});
const e = instance.exports;
const lo = Number(SEED & 0xFFFFFFFFn);
const hi = Number(SEED >> 32n);

const t0 = performance.now();
const mismatches = e.vfp_selftest_mismatches(lo, hi, ROUNDS);
const checksum = BigInt.asUintN(64, e.vfp_selftest_checksum(lo, hi, ROUNDS));
const ms = performance.now() - t0;
console.log(`wasm self-test: ${ROUNDS} rounds in ${ms.toFixed(0)} ms, mismatches ${mismatches}, checksum 0x${checksum.toString(16)}`);
if (mismatches !== 0 || (compareChecksum && checksum !== CHECKSUM)) {
  console.error(`FAILED: expected 0 mismatches${compareChecksum ? ` and checksum 0x${CHECKSUM.toString(16)}` : ''}`);
  process.exit(1);
}
console.log(compareChecksum
  ? 'OK: WebAssembly results are bit-identical to the native reference'
  : 'OK: fast paths agree with the exact software core under WebAssembly');
