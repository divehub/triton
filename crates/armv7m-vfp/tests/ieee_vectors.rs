//! Hand-checked vectors: directed rounding modes, flush-to-zero, default NaN,
//! NaN propagation order, exception flags, conversion saturation, fixed point
//! and half precision (IEEE and alternative format). The expectations were
//! worked out from the Arm pseudocode by hand; the table in
//! `common/vectors.rs` is also validated against the host hardware on aarch64.

mod common;

use common::vectors::*;

fn check_all(fast: bool) {
    let table = vectors();
    let mut bad = Vec::new();
    for v in &table {
        let (bits, flags) = run_impl(v, fast);
        if (bits, flags) != (v.expect, v.flags) {
            bad.push(format!(
                "`{}` ({:?}): a={:08x} b={:08x} c={:08x} mode={:#010x}: expected {:08x}/{:#04x}, got {:08x}/{:#04x}",
                v.name, v.op, v.a, v.b, v.c, v.mode, v.expect, v.flags, bits, flags
            ));
        }
    }
    assert!(bad.is_empty(), "{} of {} vectors failed ({}):\n{}", bad.len(), table.len(), if fast { "fast" } else { "soft" }, bad.join("\n"));
    println!("{} vectors passed ({})", table.len(), if fast { "fast paths" } else { "exact software core" });
}

#[test]
fn vectors_exact_software_core() {
    check_all(false);
}

#[test]
fn vectors_fast_path_layer() {
    check_all(true);
}

#[test]
fn vector_table_is_substantial() {
    assert!(vectors().len() >= 250, "{} vectors", vectors().len());
}

#[cfg(target_arch = "aarch64")]
#[test]
fn vectors_agree_with_hardware() {
    let mut bad = Vec::new();
    let mut checked = 0;
    for v in vectors() {
        let Some((bits, flags)) = run_hw(&v) else { continue };
        checked += 1;
        if (bits, flags) != (v.expect, v.flags) {
            bad.push(format!(
                "`{}` ({:?}): expected {:08x}/{:#04x}, hardware {:08x}/{:#04x}",
                v.name, v.op, v.expect, v.flags, bits, flags
            ));
        }
    }
    assert!(bad.is_empty(), "hand-checked vectors disagree with the hardware:\n{}", bad.join("\n"));
    println!("{checked} vectors confirmed by the hardware FPU");
}
