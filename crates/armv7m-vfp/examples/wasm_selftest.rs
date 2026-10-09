//! WebAssembly entry points for the determinism self-test and a throughput
//! micro-benchmark (`scripts/wasm_selftest.mjs` and `scripts/wasm_bench.mjs`
//! run them in Node/V8).
//!
//! Build: `cargo build -p armv7m-vfp --release --target wasm32-unknown-unknown --example wasm_selftest`

use armv7m_vfp::selftest::selftest;
use armv7m_vfp::{decode, execute, FpRegs, VfpDecode, VfpHost};

/// Checksum of `selftest(seed, n)` (seed = `seed_hi << 32 | seed_lo`).
#[no_mangle]
pub extern "C" fn vfp_selftest_checksum(seed_lo: u32, seed_hi: u32, n: u32) -> u64 {
    selftest(((seed_hi as u64) << 32) | seed_lo as u64, n).checksum
}

/// Number of fast-path disagreements of `selftest(seed, n)` (must be 0).
#[no_mangle]
pub extern "C" fn vfp_selftest_mismatches(seed_lo: u32, seed_hi: u32, n: u32) -> u32 {
    selftest(((seed_hi as u64) << 32) | seed_lo as u64, n).mismatches
}

struct BenchHost {
    r: [u32; 16],
    fp: FpRegs,
    nzcv: u32,
    mem: [u32; 4096],
}

impl VfpHost for BenchHost {
    fn reg(&self, n: u32) -> u32 {
        self.r[n as usize & 15]
    }
    fn set_reg(&mut self, n: u32, value: u32) {
        self.r[n as usize & 15] = value;
    }
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.nzcv = nzcv;
    }
    fn fp(&mut self) -> &mut FpRegs {
        &mut self.fp
    }
    fn load32(&mut self, addr: u32) -> u32 {
        self.mem[(addr >> 2) as usize & 4095]
    }
    fn store32(&mut self, addr: u32, value: u32) {
        self.mem[(addr >> 2) as usize & 4095] = value;
    }
    fn literal_base(&self) -> u32 {
        0x0800_1000
    }
}

/// Benchmarked instructions (A32 layout, cond = 1110); index 0 is "operand feed only".
const BENCH: [u32; 17] = [
    0, // feed only
    0xEE30_0A81, // vadd.f32 s0,s1,s2
    0xEE20_0A81, // vmul.f32
    0xEE80_0A81, // vdiv.f32
    0xEEB1_0AE0, // vsqrt.f32 s0,s1
    0xEEA0_0A81, // vfma.f32
    0xEE00_0A81, // vmla.f32
    0xEEF4_0A41, // vcmp.f32 s1,s2
    0xEEB8_0AE0, // vcvt.f32.s32 s0,s1
    0xEEBD_0AE0, // vcvt.s32.f32 s0,s1
    0xEEFE_0AED, // vcvt.s32.f32 s1,s1,#5
    0xED90_0A01, // vldr s0,[r0,#4]
    0xED80_0A02, // vstr s0,[r0,#8]
    0xEEB0_0A60, // vmov.f32 s0,s1
    0xED2D_8B02, // vpush {d8}
    0xECBD_8B02, // vpop {d8}
    0xEE30_0A81 | 1 << 22, // exact path: vadd with FPSCR.RMode = RP is selected below
];

/// Runs `iters` executions of benchmark instruction `kind` (see `BENCH`) on operands
/// drawn from a pool of ordinary normal numbers; returns a checksum so nothing is
/// optimized away. `kind == 16` runs vadd in round-toward-plus-infinity (exact path).
#[no_mangle]
pub extern "C" fn vfp_bench(kind: u32, iters: u32) -> u32 {
    let mut pool = [0u32; 1024];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for p in pool.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let e = 127 - 8 + (x >> 40) as u32 % 17;
        *p = (((x >> 8) as u32) & 0x8000_0000) | (e << 23) | ((x >> 20) as u32 & 0x7F_FFFF);
    }
    let mut host = BenchHost { r: [0; 16], fp: FpRegs::default(), nzcv: 0, mem: [0; 4096] };
    host.r[0] = 0x2000_0000;
    host.r[13] = 0x2000_4000;
    let kind = (kind as usize).min(BENCH.len() - 1);
    let (word, rp) = if kind == 16 { (BENCH[1], true) } else { (BENCH[kind], false) };
    if rp {
        host.fp.fpscr = 1 << 22;
    }
    let insn = if kind == 0 {
        None
    } else {
        match decode((word >> 16) as u16, word as u16) {
            VfpDecode::Insn(i) => Some(i),
            _ => return 0xFFFF_FFFF,
        }
    };
    let mut acc = 0u32;
    for i in 0..iters as usize {
        host.fp.s[1] = pool[i & 1023];
        host.fp.s[2] = pool[(i * 7 + 3) & 1023];
        host.fp.s[3] = pool[(i * 13 + 5) & 1023];
        host.fp.s[0] = pool[(i * 5 + 1) & 1023];
        if kind == 14 {
            host.r[13] = 0x2000_4000;
        } else if kind == 15 {
            host.r[13] = 0x2000_3FF0;
        }
        if let Some(insn) = &insn {
            let _ = execute(insn, &mut host);
        }
        acc ^= host.fp.s[0].wrapping_add(host.fp.fpscr);
    }
    acc
}
