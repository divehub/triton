//! System control space behavior (Renode NVIC.cs / DWT.cs semantics): NVIC enable / pending /
//! priority registers including lane-wise byte and halfword stores, SCB registers and their
//! write masks, fault status registers, MPU register file, FPU control registers, ICSR fields
//! and Renode's quirks, the DWT identification registers, and unmodeled addresses.

mod common;
#[path = "generated/snippets.rs"]
mod snippets;

use armv7m::ExitReason;
use common::*;
use snippets::*;

const MAIN: u32 = 0x0800_4000;
const HANDLER: u32 = 0x0800_6000;
const TOP: u32 = SRAM1_BASE + SRAM1_SIZE as u32;

const ICTR: u32 = 0xE000_E004;
const ISER0: u32 = 0xE000_E100;
const ISER1: u32 = 0xE000_E104;
const ISER3: u32 = 0xE000_E10C;
const ICER0: u32 = 0xE000_E180;
const ISPR0: u32 = 0xE000_E200;
const ICPR0: u32 = 0xE000_E280;
const IABR0: u32 = 0xE000_E300;
const IPR1: u32 = 0xE000_E404;
const CPUID: u32 = 0xE000_ED00;
const ICSR: u32 = 0xE000_ED04;
const VTOR: u32 = 0xE000_ED08;
const AIRCR: u32 = 0xE000_ED0C;
const SCR: u32 = 0xE000_ED10;
const CCR: u32 = 0xE000_ED14;
const SHPR1: u32 = 0xE000_ED18;
const SHPR2: u32 = 0xE000_ED1C;
const SHPR3: u32 = 0xE000_ED20;
const SHCSR: u32 = 0xE000_ED24;
const CFSR: u32 = 0xE000_ED28;
const HFSR: u32 = 0xE000_ED2C;
const CPACR: u32 = 0xE000_ED88;
const MPU_TYPE: u32 = 0xE000_ED90;
const MPU_CTRL: u32 = 0xE000_ED94;
const MPU_RNR: u32 = 0xE000_ED98;
const MPU_RBAR: u32 = 0xE000_ED9C;
const MPU_RASR: u32 = 0xE000_EDA0;
const MPU_RBAR_A1: u32 = 0xE000_EDA4;
const MPU_RASR_A1: u32 = 0xE000_EDA8;
const STIR: u32 = 0xE000_EF00;
const FPCCR: u32 = 0xE000_EF34;
const FPCAR: u32 = 0xE000_EF38;
const FPDSCR: u32 = 0xE000_EF3C;
const DWT_CTRL: u32 = 0xE000_1000;
const DWT_CYCCNT: u32 = 0xE000_1004;

struct S {
    h: Harness,
}

impl S {
    fn new() -> S {
        let mut h = Harness::new();
        h.bus.load_halfwords(HANDLER, &[0xE7FE]);
        for exc in 2..16u32 {
            h.bus.poke32(FLASH_BASE + 4 * exc, HANDLER | 1);
        }
        S { h }
    }

    fn poke(&mut self, addr: u32, v: u32) {
        self.h.cpu.ppb_poke32(addr, v, self.h.now);
    }

    fn peek(&self, addr: u32) -> u32 {
        self.h.cpu.ppb_peek32(addr, self.h.now).unwrap()
    }
}

#[test]
fn identification_registers() {
    let s = S::new();
    assert_eq!(s.peek(CPUID), 0x410F_C240, "Cortex-M4 r0p0 (Renode parity)");
    assert_eq!(s.peek(ICTR), 7, "ICTR is a fixed value in Renode");
    assert_eq!(s.peek(MPU_TYPE), 8 << 8, "eight regions");
    assert_eq!(s.peek(0xE000_E01C), 0x8000_0000 | 800_000, "SYST_CALIB: TENMS for an 80 MHz clock, NOREF");
    assert_eq!(s.h.cpu.ppb_peek32(0x4000_0000, 0), None, "outside the private peripheral bus");
}

#[test]
fn nvic_enable_clear_and_read_back() {
    let mut s = S::new();
    s.poke(ISER0, 0x0000_0021);
    assert_eq!(s.peek(ISER0), 0x21);
    assert_eq!(s.peek(ISER1), 0, "other registers untouched");
    s.poke(ICER0, 0x1);
    assert_eq!(s.peek(ISER0), 0x20);
    assert_eq!(s.peek(ICER0), 0x20, "ICER reads the enable state");
    s.poke(ISER3, 0xFFFF_FFFF);
    assert_eq!(s.peek(ISER3), 0, "interrupt lines that do not exist (96 are wired) stay disabled");
    assert_eq!(s.peek(IABR0), 0, "nothing active");
}

#[test]
fn sub_word_stores_only_act_on_the_addressed_lanes() {
    let mut s = S::new();
    // strb r1, [r0] ; strh r2, [r3] ; b .
    s.h.load(MAIN, &[0x7001, 0x801A, 0xE7FE]);
    s.h.set(0, ISER0 + 1);
    s.h.set(1, 0xFF);
    s.h.set(3, ISER0 + 2);
    s.h.set(2, 0xAAAA);
    s.h.step(2);
    assert_eq!(s.peek(ISER0), 0xAAAA_FF00, "byte 1 and the upper halfword of the write-one-to-set register");
    // The same stores to a priority register keep the other lanes (read-modify-write).
    let mut s = S::new();
    s.poke(IPR1, 0x1020_3040);
    s.h.load(MAIN, &[0x7001, 0xE7FE]); // strb r1, [r0]
    s.h.set(0, IPR1 + 2);
    s.h.set(1, 0x90);
    s.h.step(1);
    assert_eq!(s.peek(IPR1), 0x1090_3040);
}

#[test]
fn pending_registers_and_icpr_while_the_line_is_high() {
    let mut s = S::new();
    s.poke(ISPR0, 0x40);
    assert_eq!(s.peek(ISPR0), 0x40);
    s.poke(ICPR0, 0x40);
    assert_eq!(s.peek(ISPR0), 0);
    // ICPR is ignored while the peripheral keeps the line asserted (Renode `Running`).
    s.h.cpu.set_irq_line(6, true);
    assert_eq!(s.peek(ISPR0), 0x40);
    s.poke(ICPR0, 0x40);
    assert_eq!(s.peek(ISPR0), 0x40, "still pending");
    s.h.cpu.set_irq_line(6, false);
    s.poke(ICPR0, 0x40);
    assert_eq!(s.peek(ISPR0), 0);
    // STIR pends an interrupt like ISPR does.
    s.poke(STIR, 5);
    assert_eq!(s.peek(ISPR0), 0x20);
}

#[test]
fn priority_registers_keep_the_implemented_bits_and_warn_once() {
    let mut s = S::new();
    s.poke(IPR1, 0xFF80_4021);
    assert_eq!(s.peek(IPR1), 0xF080_4020, "four implemented bits per byte (priorityMask 0xF0)");
    let warnings = s.h.cpu.take_warnings();
    assert_eq!(warnings.iter().filter(|w| w.contains("priority")).count(), 2, "one warning per offending interrupt: {warnings:?}");
    s.poke(IPR1, 0xFF80_4021);
    assert!(s.h.cpu.take_warnings().is_empty(), "each interrupt warns once");
    // System handler priorities: SHPR1 holds exceptions 4..7, SHPR2 8..11 (SVCall in byte 3), SHPR3 12..15.
    s.poke(SHPR1, 0xFFFF_FFFF);
    s.poke(SHPR2, 0xFFFF_FFFF);
    s.poke(SHPR3, 0xFFFF_FFFF);
    assert_eq!(s.peek(SHPR1), 0xF0F0_F0F0);
    assert_eq!(s.peek(SHPR2), 0xF000_0000, "only SVCall exists in SHPR2");
    assert_eq!(s.peek(SHPR3), 0xF0F0_0000, "PendSV and SysTick");
}

#[test]
fn scb_registers_masks_and_parity_choices() {
    let mut s = S::new();
    s.poke(VTOR, 0x0800_407F);
    assert_eq!(s.peek(VTOR), 0x0800_4000, "bits below 7 are RES0");
    assert_eq!(s.peek(AIRCR), 0xFA05_2000, "VECTKEYSTAT 0xFA05; Renode reports BFHFNMINS (bit 13) set without TrustZone");
    s.poke(AIRCR, 0x1234_0700);
    assert_eq!(s.peek(AIRCR), 0xFA05_2000, "writes without VECTKEY are ignored");
    s.poke(AIRCR, 0x05FA_0500);
    assert_eq!(s.peek(AIRCR), 0xFA05_2500, "PRIGROUP = 5, key reads as 0xFA05");
    s.poke(AIRCR, 0x05FA_0000);
    assert_eq!(s.peek(AIRCR), 0xFA05_2000, "BFHFNMINS cannot be cleared from the non-secure view");
    s.poke(SCR, 0xFFFF_FFFF);
    assert_eq!(s.peek(SCR), 0x16, "SLEEPONEXIT, SLEEPDEEP and SEVONPEND");
    s.poke(CCR, 0xFFFF_FFFF);
    assert_eq!(s.peek(CCR), 0x30B, "Renode parity: DIV_0_TRP writes are filtered out");
    s.h.cpu.set_filter_ccr_div0_write(false);
    s.poke(CCR, 0xFFFF_FFFF);
    assert_eq!(s.peek(CCR), 0x31B);
    s.poke(CPACR, 0xFFFF_FFFF);
    assert_eq!(s.peek(CPACR), 0x00F0_0000, "only CP10/CP11 exist");
}

#[test]
fn the_core_leaves_reset_with_the_z_flag_set_like_tlib() {
    // tlib zeroes its inverted zero-flag cache on reset, so xPSR reads 0x41000000 (T and Z) until the first
    // flag-setting instruction. A Renode checkpoint of the handset right after reset shows exactly that.
    let mut s = S::new();
    assert_eq!(s.h.cpu.xpsr(), 0x4100_0000);
    // `movs r0,#1` clears Z; a later reset sets it again.
    s.h.load(MAIN, &[0x2001, 0xE7FE]);
    s.h.step_once(1);
    assert_eq!(s.h.cpu.xpsr() & 0xF000_0000, 0);
    s.h.cpu.reset();
    assert_eq!(s.h.cpu.xpsr(), 0x4100_0000);
    // `beq` right out of reset is taken, as on Renode.
    let mut s = S::new();
    s.h.load(MAIN, &[0xD001, 0x2001, 0x2002, 0x2303, 0xE7FE]); // beq +2 (over both movs r0) ; movs r0,#1 ; movs r0,#2 ; movs r3,#3 ; b .
    s.h.step_once(2);
    assert_eq!((s.h.r(0), s.h.r(3)), (0, 3), "the branch on the reset-state Z flag is taken");
}

#[test]
fn a_system_reset_request_stops_the_chunk_and_is_reported_once() {
    let mut s = S::new();
    s.h.load(MAIN, &[0xBF00, 0xBF00, 0xE7FE]);
    s.poke(AIRCR, 0x05FA_0004); // SYSRESETREQ
    let e = s.h.step_once(10);
    assert_eq!(e.reason, ExitReason::StopRequested);
    assert!(s.h.cpu.take_reset_request());
    assert!(!s.h.cpu.take_reset_request(), "reported once");
    // Without the request the program runs on.
    let e = s.h.step_once(10);
    assert_ne!(e.reason, ExitReason::StopRequested);
}

#[test]
fn floating_point_control_registers() {
    let mut s = S::new();
    assert_eq!(s.peek(FPCCR), 0xC000_0000, "ASPEN and LSPEN are set out of reset");
    s.poke(FPCCR, 0);
    assert_eq!(s.peek(FPCCR), 0);
    s.poke(FPCCR, 0xFFFF_FFFF);
    assert_eq!(
        s.peek(FPCCR),
        0xD000_07FB,
        "Renode keeps every writable bit as written: ASPEN, LSPEN, CLRONRET and [10:0] without S (bit 2); [25:11] and the TrustZone bits read zero"
    );
    s.poke(FPCCR, 0xC000_0000);
    assert_eq!(s.peek(FPCCR), 0xC000_0000);
    s.poke(FPCAR, 0x2000_0105);
    assert_eq!(s.peek(FPCAR), 0x2000_0100, "word aligned");
    s.poke(FPDSCR, 0xFFFF_FFFF);
    assert_eq!(s.peek(FPDSCR), 0x07C0_0000, "AHP, DN, FZ and RMode");
    // Unprivileged code reads zero and its writes are ignored.
    s.h.cpu.set_control(1);
    assert_eq!(s.peek(FPCCR), 0);
    s.poke(FPCAR, 0x2000_0200);
    s.h.cpu.set_control(0);
    assert_eq!(s.peek(FPCAR), 0x2000_0100);
}

#[test]
fn mpu_registers_are_stored_and_read_back_but_not_enforced() {
    let mut s = S::new();
    s.poke(MPU_RNR, 3);
    s.poke(MPU_RBAR, 0x2000_0000);
    s.poke(MPU_RASR, 0x0308_0001);
    assert_eq!(s.peek(MPU_RNR), 3);
    assert_eq!(s.peek(MPU_RBAR), 0x2000_0003, "the region number reads back in the low bits");
    assert_eq!(s.peek(MPU_RASR), 0x0308_0001);
    // RBAR with VALID selects the region first; the alias registers address the same region.
    s.poke(MPU_RBAR, 0x4000_0000 | 0x10 | 5);
    assert_eq!(s.peek(MPU_RNR), 5);
    assert_eq!(s.peek(MPU_RBAR_A1), 0x4000_0000 | 5);
    s.poke(MPU_RASR_A1, 0xFFFF_FFFF);
    assert_eq!(s.peek(MPU_RASR), 0xFFFF_FF3F, "Renode stores the attribute halfword whole; bits 7:6 are RES0");
    s.poke(MPU_RNR, 3);
    assert_eq!(s.peek(MPU_RASR), 0x0308_0001, "region 3 is untouched");
    assert!(!s.h.cpu.mpu_enabled());
    s.poke(MPU_CTRL, 0xFF);
    assert_eq!(s.peek(MPU_CTRL), 0xFF, "stored as written");
    assert!(s.h.cpu.mpu_enabled());
    assert!(s.h.cpu.take_warnings().iter().any(|w| w.contains("MPU")), "enabling the MPU is reported");
}

#[test]
fn fault_status_registers_are_write_one_to_clear_and_shcsr_holds_the_enables() {
    let mut s = S::new();
    s.h.load(MAIN, UDF_PROG);
    s.h.step(3);
    assert_eq!(s.peek(CFSR), 0x1_0000, "UFSR.UNDEFINSTR");
    assert_eq!(s.peek(HFSR), 0x4000_0000, "FORCED: UsageFault is disabled");
    s.poke(CFSR, 0x0001_0000);
    s.poke(HFSR, 0x4000_0000);
    assert_eq!((s.peek(CFSR), s.peek(HFSR)), (0, 0));
    s.poke(SHCSR, (1 << 16) | (1 << 17) | (1 << 18));
    assert_eq!(s.peek(SHCSR) >> 16 & 7, 7, "MEMFAULTENA, BUSFAULTENA, USGFAULTENA");
    s.poke(SHCSR, (1 << 15) | (7 << 16));
    assert_ne!(s.peek(SHCSR) & (1 << 15), 0, "SVCALLPENDED");
    s.poke(SHCSR, 7 << 16);
    assert_eq!(s.peek(SHCSR) & (1 << 15), 0);
}

#[test]
fn icsr_fields_and_renode_quirks() {
    let mut s = S::new();
    s.h.load(MAIN, COUNT_LOOP);
    s.h.bus.load_halfwords(0x0800_6100, H_COUNT_A);
    s.h.bus.load_halfwords(0x0800_6200, H_COUNT_B);
    s.h.bus.poke32(FLASH_BASE + 4 * 21, 0x0800_6100 | 1);
    s.h.bus.poke32(FLASH_BASE + 4 * 22, 0x0800_6200 | 1);
    s.poke(ISER0, (1 << 5) | (1 << 6));
    s.poke(IPR1, 0x40 | (0x20 << 8)); // IRQ5 0x40, IRQ6 0x20
    s.h.step(4);
    assert_eq!(s.peek(ICSR) & 0x1FF, 0, "VECTACTIVE is zero in Thread mode");
    s.poke(ISPR0, 1 << 5);
    assert_eq!((s.peek(ICSR) >> 12) & 0x1FF, 21, "VECTPENDING shows the exception that will be taken");
    s.h.step(1);
    assert_eq!(s.peek(ICSR) & 0x1FF, 21, "VECTACTIVE in IRQ5's handler");
    assert_ne!(s.peek(ICSR) & (1 << 11), 0, "RETTOBASE: one system exception or fewer active");
    // A nested external interrupt still reports RETTOBASE (Renode only counts system exceptions).
    s.poke(ISPR0, 1 << 6);
    s.h.step(1);
    assert_eq!(s.peek(ICSR) & 0x1FF, 22);
    assert_ne!(s.peek(ICSR) & (1 << 11), 0, "Renode quirk: RETTOBASE ignores active external interrupts");
    s.h.step(40);
    assert_eq!(s.peek(ICSR) & 0x1FF, 0);
    // PENDSVSET / PENDSVCLR / PENDSTSET / PENDSTCLR / PENDNMISET are write-one actions.
    s.poke(ICSR, (1 << 28) | (1 << 26));
    assert_eq!(s.peek(ICSR) >> 26 & 1, 1);
    assert_eq!(s.peek(ICSR) >> 28 & 1, 1);
    s.poke(ICSR, (1 << 27) | (1 << 25));
    assert_eq!(s.peek(ICSR) & ((1 << 26) | (1 << 28)), 0);
}

#[test]
fn unmodeled_private_peripheral_bus_addresses_read_zero_with_one_warning() {
    let mut s = S::new();
    // ldr r1, [r0] ; ldr r2, [r0] ; b .   (FPB_CTRL at 0xE0002000 has no model in Renode)
    s.h.load(MAIN, &[0x6801, 0x6802, 0xE7FE]);
    s.h.set(0, 0xE000_2000);
    s.h.set(1, 0xFFFF_FFFF);
    s.h.step(2);
    assert_eq!((s.h.r(1), s.h.r(2)), (0, 0));
    let warnings = s.h.cpu.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("0xe0002000"), "{warnings:?}");
    // Writes are dropped, also with one warning per address (the first address already warned).
    s.h.load(MAIN, &[0x6001, 0xE7FE]); // str r1,[r0]
    s.h.set(0, 0xE000_2008);
    s.h.set(1, 5);
    s.h.step(1);
    let warnings = s.h.cpu.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    s.h.cpu.set_pc(MAIN);
    s.h.step(1);
    assert!(s.h.cpu.take_warnings().is_empty(), "once per address");
}

#[test]
fn dwt_registers() {
    let mut s = S::new();
    s.poke(DWT_CTRL, 0xFFFF_FFFF);
    assert_eq!(s.peek(DWT_CTRL), 1, "only CYCCNTENA is implemented");
    s.poke(DWT_CYCCNT, 1234);
    assert_eq!(s.peek(DWT_CYCCNT), 1234);
    s.poke(DWT_CTRL, 0);
    assert_eq!(s.peek(DWT_CTRL), 0);
    for (addr, want) in [(0xE000_1FD0u32, 0x04u32), (0xE000_1FE0, 0x02), (0xE000_1FE4, 0xB0), (0xE000_1FE8, 0x1B), (0xE000_1FF0, 0x0D), (0xE000_1FF4, 0xE0), (0xE000_1FF8, 0x05), (0xE000_1FFC, 0xB1)] {
        assert_eq!(s.peek(addr), want, "{addr:#x}");
    }
    assert_eq!(s.peek(0xE000_1020), 0, "comparators are tags: they read zero");
    let _ = TOP;
}

#[test]
fn unaligned_traps_follow_ccr() {
    let mut s = S::new();
    // ldr r1, [r0] ; b .  with r0 pointing at an odd address
    s.h.load(MAIN, &[0x6801, 0xE7FE]);
    s.h.set(0, SRAM1_BASE + 0x101);
    s.h.bus.poke32(SRAM1_BASE + 0x100, 0x4433_2211);
    s.h.bus.poke32(SRAM1_BASE + 0x104, 0x8877_6655);
    s.h.step(1);
    assert_eq!(s.h.r(1), 0x5544_3322, "unaligned LDR is allowed by default");
    assert_eq!(s.peek(CFSR), 0);
    s.poke(CCR, 1 << 3); // UNALIGN_TRP
    s.h.cpu.set_pc(MAIN);
    s.h.step(1);
    assert_eq!(s.peek(CFSR) & 0x0100_0000, 0x0100_0000, "UFSR.UNALIGNED");
}
