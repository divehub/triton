//! Throughput micro-benchmark of `execute` for the common instructions
//! (ignored by default):
//!
//! `cargo test -p armv7m-vfp --release --target-dir target/fpu --test bench -- --ignored --nocapture`
//!
//! Operands come from a pool of ordinary normal numbers (the common case on the
//! firmware); the cost of feeding the operands is measured separately and
//! subtracted. The "exact" rows force the integer-only software path by
//! selecting a non-nearest rounding mode (round toward +infinity).

mod common;

use armv7m_vfp::fpscr::*;
use armv7m_vfp::{decode, execute, FpRegs, VfpDecode, VfpHost, VfpInsn};
use common::*;
use std::hint::black_box;
use std::time::Instant;

struct Host {
    r: [u32; 16],
    fp: FpRegs,
    nzcv: u32,
    mem: Vec<u32>,
}

impl VfpHost for Host {
    #[inline(always)]
    fn reg(&self, n: u32) -> u32 {
        self.r[n as usize]
    }
    #[inline(always)]
    fn set_reg(&mut self, n: u32, value: u32) {
        self.r[n as usize] = value;
    }
    #[inline(always)]
    fn set_apsr_nzcv(&mut self, nzcv: u32) {
        self.nzcv = nzcv;
    }
    #[inline(always)]
    fn fp(&mut self) -> &mut FpRegs {
        &mut self.fp
    }
    #[inline(always)]
    fn load32(&mut self, addr: u32) -> u32 {
        self.mem[(addr >> 2) as usize & 4095]
    }
    #[inline(always)]
    fn store32(&mut self, addr: u32, value: u32) {
        self.mem[(addr >> 2) as usize & 4095] = value;
    }
    #[inline(always)]
    fn literal_base(&self) -> u32 {
        0x0800_1000
    }
}

fn enc(w: u32) -> VfpInsn {
    match decode((w >> 16) as u16, w as u16) {
        VfpDecode::Insn(i) => i,
        o => panic!("{w:08x}: {o:?}"),
    }
}

const POOL: usize = 1024;

fn pool(rng: &mut Rng) -> Vec<u32> {
    (0..POOL)
        .map(|_| {
            // Normal numbers with exponents in [-8, 8] and random sign / fraction.
            let e = 127 - 8 + rng.below(17);
            (rng.next32() & 0x8000_0000) | (e << 23) | (rng.next32() & 0x7F_FFFF)
        })
        .collect()
}

fn time<F: FnMut(usize)>(iters: usize, mut f: F) -> f64 {
    // Warm up, then measure.
    for i in 0..iters / 10 {
        f(i);
    }
    let t = Instant::now();
    for i in 0..iters {
        f(i);
    }
    t.elapsed().as_nanos() as f64 / iters as f64
}

#[test]
#[ignore]
fn bench_execute() {
    let iters: usize = std::env::var("NGC_VFP_BENCH_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000_000);
    let mut rng = Rng::new(42);
    let p = pool(&mut rng);
    let mut host = Host { r: [0; 16], fp: FpRegs::default(), nzcv: 0, mem: p.iter().cycle().take(4096).copied().collect() };
    host.r[0] = 0x2000_0000;
    host.r[1] = 0x2000_0100;
    host.r[13] = 0x2000_4000;

    // Operand feed shared by every row: s1, s2 and s3 take new values each iteration.
    let feed_ns = {
        let h = &mut host;
        time(iters, |i| {
            h.fp.s[1] = black_box(p[i & (POOL - 1)]);
            h.fp.s[2] = black_box(p[(i * 7 + 3) & (POOL - 1)]);
            h.fp.s[3] = black_box(p[(i * 13 + 5) & (POOL - 1)]);
            h.fp.s[0] = black_box(p[(i * 5 + 1) & (POOL - 1)]);
        })
    };
    println!("operand feed overhead: {feed_ns:.2} ns/iteration (subtracted below)");

    // (name, instruction word in A32 layout with cond = 1110, fpscr mode)
    let rows: Vec<(&str, u32, u32)> = vec![
        ("vldr.32 s0,[r0,#4]", 0xED90_0A01, 0),
        ("vstr.32 s0,[r0,#8]", 0xED80_0A02, 0),
        ("vldr.64 d0,[r0,#8]", 0xED90_0B02, 0),
        ("vmov.f32 s0,s1", 0xEEB0_0A60, 0),
        ("vmov.f32 s0,#3.0", 0xEEB0_0A08, 0),
        ("vmov s0,r1", 0xEE00_1A10, 0),
        ("vmov r1,s0", 0xEE10_1A10, 0),
        ("vabs.f32 s0,s1", 0xEEB0_0AE0, 0),
        ("vneg.f32 s0,s1", 0xEEB1_0A60, 0),
        ("vadd.f32 s0,s1,s2", 0xEE30_0A81, 0),
        ("vsub.f32 s0,s1,s2", 0xEE30_0AC1, 0),
        ("vmul.f32 s0,s1,s2", 0xEE20_0A81, 0),
        ("vnmul.f32 s0,s1,s2", 0xEE20_0AC1, 0),
        ("vdiv.f32 s0,s1,s2", 0xEE80_0A81, 0),
        ("vsqrt.f32 s0,s1", 0xEEB1_0AE0, 0),
        ("vmla.f32 s0,s1,s2", 0xEE00_0A81, 0),
        ("vfma.f32 s0,s1,s2", 0xEEA0_0A81, 0),
        ("vcmp.f32 s1,s2", 0xEEF4_0A41, 0),
        ("vcmpe.f32 s1,#0", 0xEEF5_0AC0, 0),
        ("vcvt.f32.s32 s0,s1", 0xEEB8_0AE0, 0),
        ("vcvt.s32.f32 s0,s1", 0xEEBD_0AE0, 0),
        ("vcvt.s32.f32 s1,s1,#5", 0xEEFE_0AED, 0),
        ("vcvt.f32.s32 s1,s1,#5", 0xEEFA_0AED, 0),
        ("vmrs APSR_nzcv", 0xEEF1_FA10, 0),
        ("vpush {d8}", 0xED2D_8B02, 0),
        ("vpop {d8}", 0xECBD_8B02, 0),
        ("exact add (RP)", 0xEE30_0A81, RMODE_RP << RMODE_SHIFT),
        ("exact mul (RP)", 0xEE20_0A81, 1 << RMODE_SHIFT),
        ("exact div (RP)", 0xEE80_0A81, 1 << RMODE_SHIFT),
        ("exact sqrt (RP)", 0xEEB1_0AE0, 1 << RMODE_SHIFT),
        ("exact fma (RP)", 0xEEA0_0A81, 1 << RMODE_SHIFT),
        ("exact cvt.f32.s32 (RP)", 0xEEB8_0AE0, 1 << RMODE_SHIFT),
        ("exact add (FZ)", 0xEE30_0A81, FZ),
    ];
    println!("{:28} {:>10} {:>12}", "instruction", "ns/op", "Mop/s");
    for (name, w, mode) in rows {
        let insn = enc(w);
        host.fp.fpscr = mode;
        let is_push = name.starts_with("vpush");
        let is_pop = name.starts_with("vpop");
        let h = &mut host;
        let ns = time(iters, |i| {
            h.fp.s[1] = black_box(p[i & (POOL - 1)]);
            h.fp.s[2] = black_box(p[(i * 7 + 3) & (POOL - 1)]);
            h.fp.s[3] = black_box(p[(i * 13 + 5) & (POOL - 1)]);
            h.fp.s[0] = black_box(p[(i * 5 + 1) & (POOL - 1)]);
            // Keep SP in range for push/pop pairs.
            if is_push {
                h.r[13] = 0x2000_4000;
            } else if is_pop {
                h.r[13] = 0x2000_3FF0;
            }
            let r = execute(black_box(&insn), h);
            black_box(r);
        }) - feed_ns;
        let ns = ns.max(0.01);
        println!("{name:28} {ns:>10.2} {:>12.1}", 1000.0 / ns);
    }
    black_box(&host);
}
