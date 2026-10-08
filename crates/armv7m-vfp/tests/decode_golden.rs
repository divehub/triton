//! Golden encodings (hw1, hw2) with their expected disassembly. The expected
//! texts were produced by GNU binutils 2.45 (`arm-none-eabi-objdump -D -b binary
//! -m arm -M force-thumb`) and reformatted to the Ghidra-style layout used by the
//! crate; `scripts/objdump_crosscheck.py` re-verifies all 16.8 M CP10/CP11
//! encodings against objdump when the Arm toolchain is available.

use armv7m_vfp::{decode, disassemble, VfpDecode};

const GOLDEN: &[(u16, u16, &str)] = &[
    (0xEDD3, 0x7A02, "vldr.32 s15,[r3,#0x8]"),
    (0xED90, 0x0A0D, "vldr.32 s0,[r0,#0x34]"),
    (0xED95, 0x1A00, "vldr.32 s2,[r5]"),
    (0xED14, 0x7A01, "vldr.32 s14,[r4,#-0x4]"),
    (0xED1F, 0x7A04, "vldr.32 s14,[pc,#-0x10]"),
    (0xED9F, 0x8B28, "vldr.64 d8,[pc,#0xa0]"),
    (0xED8D, 0x0B08, "vstr.64 d0,[sp,#0x20]"),
    (0xEDCD, 0x7A00, "vstr.32 s15,[sp]"),
    (0xEEF8, 0x7AE7, "vcvt.f32.s32 s15,s15"),
    (0xEEF8, 0x7A67, "vcvt.f32.u32 s15,s15"),
    (0xEEFD, 0x7AE7, "vcvt.s32.f32 s15,s15"),
    (0xEEFC, 0x7AE7, "vcvt.u32.f32 s15,s15"),
    (0xEEBD, 0x0A40, "vcvtr.s32.f32 s0,s0"),
    (0xEEBC, 0x0A40, "vcvtr.u32.f32 s0,s0"),
    (0xEEFE, 0x6AED, "vcvt.s32.f32 s13,s13,#0x5"),
    (0xEEBA, 0x0AC0, "vcvt.f32.s32 s0,s0,#0x20"),
    (0xEEFA, 0x0A40, "vcvt.f32.s16 s1,s1,#0x10"),
    (0xEEB2, 0x0A40, "vcvtb.f32.f16 s0,s0"),
    (0xEEB3, 0x0AC0, "vcvtt.f16.f32 s0,s0"),
    (0xEEB0, 0x7AE7, "vabs.f32 s14,s15"),
    (0xEEB1, 0x0A40, "vneg.f32 s0,s0"),
    (0xEEB1, 0x0AC0, "vsqrt.f32 s0,s0"),
    (0xEEF0, 0x0A40, "vmov.f32 s1,s0"),
    (0xEEF0, 0x0A08, "vmov.f32 s1,0x40400000"),
    (0xEEF7, 0x0A00, "vmov.f32 s1,0x3f800000"),
    (0xEEB5, 0x0AC0, "vcmpe.f32 s0,#0"),
    (0xEEB5, 0x0A40, "vcmp.f32 s0,#0"),
    (0xEEB4, 0x7A67, "vcmp.f32 s14,s15"),
    (0xEEB4, 0x7AE7, "vcmpe.f32 s14,s15"),
    (0xEE87, 0x0A87, "vdiv.f32 s0,s15,s14"),
    (0xEEA7, 0xBA89, "vfma.f32 s22,s15,s18"),
    (0xEEE7, 0x7A66, "vfms.f32 s15,s14,s13"),
    (0xEE66, 0x5AC7, "vnmul.f32 s11,s13,s14"),
    (0xEE00, 0x1A10, "vmov s0,r1"),
    (0xEE16, 0x3A90, "vmov r3,s13"),
    (0xEE00, 0x1B10, "vmov.32 d0[0],r1"),
    (0xEE10, 0x1B10, "vmov.32 r1,d0[0]"),
    (0xEE20, 0x2B10, "vmov.32 d0[1],r2"),
    (0xEC51, 0x0B10, "vmov r0,r1,d0"),
    (0xEC41, 0x0B18, "vmov d8,r0,r1"),
    (0xEC40, 0x0A10, "vmov s0,s1,r0,r0"),
    (0xEC46, 0xAA10, "vmov s0,s1,r10,r6"),
    (0xEEF1, 0xFA10, "vmrs apsr,fpscr"),
    (0xEEF1, 0x3A10, "vmrs r3,fpscr"),
    (0xEEE1, 0x3A10, "vmsr fpscr,r3"),
    (0xED2D, 0x8B02, "vpush {d8}"),
    (0xED2D, 0x8B04, "vpush {d8,d9}"),
    (0xECBD, 0x8B02, "vpop {d8}"),
    (0xED2D, 0x0A04, "vpush {s0,s1,s2,s3}"),
    (0xECBD, 0x0A02, "vpop {s0,s1}"),
    (0xECB4, 0x0A01, "vldmia r4!,{s0}"),
    (0xECA3, 0x7A01, "vstmia r3!,{s14}"),
    (0xEC93, 0x2A02, "vldmia r3,{s4,s5}"),
    (0xED20, 0x8A10, "vstmdb r0!,{s16,s17,s18,s19,s20,s21,s22,s23,s24,s25,s26,s27,s28,s29,s30,s31}"),
    (0xECB0, 0x1B04, "vldmia r0!,{d1,d2}"),
];

const UNDEFINED: &[(u16, u16, &str)] = &[
    (0xEE30, 0x0B00, "vadd.f64 d0,d0,d0"),
    (0xEEB7, 0x0AC0, "vcvt.f64.f32 d0,s0"),
    (0xEEB6, 0x0A40, "vrintr.f32"),
    (0xFE00, 0x0A00, "vseleq.f32"),
    (0xEE80, 0x1B10, "vdup.32 d0,r1"),
    (0xEEF0, 0x1A10, "vmrs r1,fpsid"),
    (0xEEF7, 0x1A10, "vmrs r1,mvfr0"),
    (0xED42, 0x0B00, "vstr.64 d16,[r2]"),
    (0xEC9F, 0x1A01, "vldmia pc!,{s2}"),
    (0xED8F, 0x0A00, "vstr.32 s0,[pc]"),
    (0xEC50, 0x0A30, "vmov r0,r0,s1,s2"),
    (0xEEB0, 0x0A80, "vmov.f32 with bit 7 set"),
    (0xEEB0, 0x0A20, "vmov.f32 with bit 5 set"),
    (0xEEB5, 0x0A61, "vcmp #0 with M set"),
    (0xEC06, 0xAA10, "P=U=W=0 D=0 unallocated"),
];

#[test]
fn golden_encodings_disassemble_as_gnu_binutils() {
    for &(hw1, hw2, text) in GOLDEN {
        match decode(hw1, hw2) {
            VfpDecode::Insn(i) => {
                assert_eq!(i.disassemble(), text, "{hw1:04x} {hw2:04x}");
                assert_eq!(disassemble(hw1, hw2), text);
                assert_eq!(i.encoding(), (hw1, hw2));
            }
            other => panic!("{hw1:04x} {hw2:04x} `{text}` decoded as {other:?}"),
        }
    }
}

#[test]
fn golden_undefined_encodings() {
    for &(hw1, hw2, why) in UNDEFINED {
        assert_eq!(decode(hw1, hw2), VfpDecode::Undefined, "{hw1:04x} {hw2:04x} ({why})");
        assert_eq!(disassemble(hw1, hw2), "undefined");
    }
}
