//! FPSCR bit definitions for the Cortex-M4F FPv4-SP unit.

/// Negative condition flag copy (bit 31).
pub const N: u32 = 1 << 31;
/// Zero condition flag copy (bit 30).
pub const Z: u32 = 1 << 30;
/// Carry condition flag copy (bit 29).
pub const C: u32 = 1 << 29;
/// Overflow condition flag copy (bit 28).
pub const V: u32 = 1 << 28;
/// Alternative half-precision format (bit 26).
pub const AHP: u32 = 1 << 26;
/// Default NaN mode (bit 25).
pub const DN: u32 = 1 << 25;
/// Flush-to-zero mode (bit 24).
pub const FZ: u32 = 1 << 24;
/// Rounding mode field position (bits 23:22).
pub const RMODE_SHIFT: u32 = 22;
/// Rounding mode field mask (bits 23:22).
pub const RMODE_MASK: u32 = 3 << RMODE_SHIFT;

/// RMode encodings.
pub const RMODE_RN: u32 = 0;
pub const RMODE_RP: u32 = 1;
pub const RMODE_RM: u32 = 2;
pub const RMODE_RZ: u32 = 3;

/// Cumulative exception flags.
pub const IOC: u32 = 1 << 0;
pub const DZC: u32 = 1 << 1;
pub const OFC: u32 = 1 << 2;
pub const UFC: u32 = 1 << 3;
pub const IXC: u32 = 1 << 4;
pub const IDC: u32 = 1 << 7;
/// All cumulative exception flag bits.
pub const FLAGS_MASK: u32 = IOC | DZC | OFC | UFC | IXC | IDC;

/// Writable FPSCR bits on a Cortex-M4F: N, Z, C, V (31:28), AHP, DN, FZ,
/// RMode (26:22) and the cumulative flags IDC, IXC, UFC, OFC, DZC, IOC.
/// QC (27), Stride/Len (21:16) and the trap enables (15, 12:8) do not exist
/// (RAZ/WI): the M-profile floating-point unit never traps.
pub const WRITE_MASK: u32 = 0xF7C0_009F;

/// Bits of FPSCR that select the arithmetic mode used by the native fast
/// paths: rounding mode and flush-to-zero.
pub(crate) const FAST_MODE_MASK: u32 = RMODE_MASK | FZ;

/// Returns the RMode field (0..=3).
#[inline(always)]
pub const fn rmode(fpscr: u32) -> u32 {
    (fpscr >> RMODE_SHIFT) & 3
}
