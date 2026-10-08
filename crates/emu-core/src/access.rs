// Ported from Renode 1.17.0 src/Emulator/Main/Core/Extensions/ReadWriteExtensions.cs and the access
// method selection in src/Emulator/Main/Peripherals/Bus/SystemBus.cs (MIT License, Copyright (c) Antmicro).

//! Bus access widths and Renode's per-peripheral access translation.
//!
//! The CPU issues byte, halfword and word accesses. A Renode peripheral
//! implements some of the `IBytePeripheral` / `IWordPeripheral` /
//! `IDoubleWordPeripheral` interfaces and may carry an
//! `[AllowedTranslations(...)]` attribute. The system bus then behaves as
//! follows (Renode 1.17.0, `SystemBus.FillAccessMethodsWithDefaultMethods` and
//! `ReadWriteExtensions`):
//!
//! * an access of a width the peripheral implements goes straight to it with
//!   the original (possibly unaligned) offset;
//! * otherwise the first applicable *allowed* translation is used, in the
//!   order documented on [`AccessPolicy::resolve`];
//! * otherwise the access is "not translated": the peripheral is not called,
//!   a warning is logged, reads return 0 and writes are dropped.
//!
//! Vocabulary: Renode's "Word" is 16 bits and its "DoubleWord" is 32 bits. This
//! crate uses ARM names ([`Width::Half`], [`Width::Word`]); the constants on
//! [`Translations`] state the Renode spelling they correspond to.

/// Access width of a bus transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Width {
    Byte = 1,
    Half = 2,
    Word = 4,
}

impl Width {
    #[inline]
    pub const fn bytes(self) -> u32 {
        self as u32
    }

    #[inline]
    pub const fn bits(self) -> u32 {
        (self as u32) * 8
    }

    /// Mask selecting the bits a value of this width may occupy.
    #[inline]
    pub const fn mask(self) -> u32 {
        match self {
            Width::Byte => 0xFF,
            Width::Half => 0xFFFF,
            Width::Word => 0xFFFF_FFFF,
        }
    }

    pub const fn from_bytes(bytes: u32) -> Option<Width> {
        match bytes {
            1 => Some(Width::Byte),
            2 => Some(Width::Half),
            4 => Some(Width::Word),
            _ => None,
        }
    }

    /// Renode's name for this width (`Byte`, `Word`, `DoubleWord`), as used in its log messages.
    pub const fn renode_name(self) -> &'static str {
        match self {
            Width::Byte => "Byte",
            Width::Half => "Word",
            Width::Word => "DoubleWord",
        }
    }
}

/// Set of widths (bit mask: byte = 1, half = 2, word = 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Widths(pub u8);

impl Widths {
    pub const NONE: Widths = Widths(0);
    pub const BYTE: Widths = Widths(1);
    pub const HALF: Widths = Widths(2);
    pub const WORD: Widths = Widths(4);
    pub const ALL: Widths = Widths(7);

    #[inline]
    pub const fn contains(self, width: Width) -> bool {
        self.0 & (width as u8) != 0
    }

    pub const fn union(self, other: Widths) -> Widths {
        Widths(self.0 | other.0)
    }
}

impl std::ops::BitOr for Widths {
    type Output = Widths;

    fn bitor(self, rhs: Widths) -> Widths {
        self.union(rhs)
    }
}

/// Renode `AllowedTranslation` flags (see the module documentation for naming).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Translations(pub u8);

impl Translations {
    pub const NONE: Translations = Translations(0);
    /// Renode `ByteToWord`: byte accesses via aligned 16-bit accesses.
    pub const BYTE_TO_HALF: Translations = Translations(1 << 0);
    /// Renode `ByteToDoubleWord`: byte accesses via aligned 32-bit accesses.
    pub const BYTE_TO_WORD: Translations = Translations(1 << 1);
    /// Renode `WordToByte`: halfword accesses via two byte accesses.
    pub const HALF_TO_BYTE: Translations = Translations(1 << 2);
    /// Renode `WordToDoubleWord`: halfword accesses via aligned 32-bit accesses.
    pub const HALF_TO_WORD: Translations = Translations(1 << 3);
    /// Renode `DoubleWordToByte`: word accesses via four byte accesses.
    pub const WORD_TO_BYTE: Translations = Translations(1 << 4);
    /// Renode `DoubleWordToWord`: word accesses via two halfword accesses.
    pub const WORD_TO_HALF: Translations = Translations(1 << 5);

    #[inline]
    pub const fn contains(self, other: Translations) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    pub const fn union(self, other: Translations) -> Translations {
        Translations(self.0 | other.0)
    }
}

impl std::ops::BitOr for Translations {
    type Output = Translations;

    fn bitor(self, rhs: Translations) -> Translations {
        self.union(rhs)
    }
}

/// How a peripheral receives bus accesses; mirrors the Renode interfaces it
/// implements (`native`) and its `[AllowedTranslations]` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessPolicy {
    /// Widths delivered unmodified to `Peripheral::read/write`.
    pub native: Widths,
    /// Translations the bus may use for the other widths.
    pub translations: Translations,
}

impl AccessPolicy {
    /// Every width reaches the peripheral unmodified. Default for new peripherals, test
    /// peripherals and models that handle sub-word access themselves (e.g. `ArrayMemory`).
    pub const EXACT: AccessPolicy = AccessPolicy { native: Widths::ALL, translations: Translations::NONE };

    /// Only `IDoubleWordPeripheral` (no allowed translations): halfword and byte accesses are
    /// "not translated".
    pub const WORD_ONLY: AccessPolicy = AccessPolicy { native: Widths::WORD, translations: Translations::NONE };

    pub const fn new(native: Widths, translations: Translations) -> AccessPolicy {
        AccessPolicy { native, translations }
    }

    pub const fn with_translations(self, translations: Translations) -> AccessPolicy {
        AccessPolicy { native: self.native, translations }
    }

    /// Decides how an access of `width` is served, following Renode's priority:
    /// byte: 32-bit then 16-bit; halfword: 32-bit then byte; word: halfword then byte.
    pub const fn resolve(self, width: Width) -> Resolution {
        if self.native.contains(width) {
            return Resolution::Native;
        }
        match width {
            Width::Byte => {
                if self.translations.contains(Translations::BYTE_TO_WORD) && self.native.contains(Width::Word) {
                    Resolution::Via(Width::Word)
                } else if self.translations.contains(Translations::BYTE_TO_HALF) && self.native.contains(Width::Half) {
                    Resolution::Via(Width::Half)
                } else {
                    Resolution::Unsupported
                }
            }
            Width::Half => {
                if self.translations.contains(Translations::HALF_TO_WORD) && self.native.contains(Width::Word) {
                    Resolution::Via(Width::Word)
                } else if self.translations.contains(Translations::HALF_TO_BYTE) && self.native.contains(Width::Byte) {
                    Resolution::Via(Width::Byte)
                } else {
                    Resolution::Unsupported
                }
            }
            Width::Word => {
                if self.translations.contains(Translations::WORD_TO_HALF) && self.native.contains(Width::Half) {
                    Resolution::Via(Width::Half)
                } else if self.translations.contains(Translations::WORD_TO_BYTE) && self.native.contains(Width::Byte) {
                    Resolution::Via(Width::Byte)
                } else {
                    Resolution::Unsupported
                }
            }
        }
    }
}

impl Default for AccessPolicy {
    fn default() -> Self {
        AccessPolicy::EXACT
    }
}

/// Outcome of [`AccessPolicy::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The peripheral implements this width.
    Native,
    /// Served through accesses of the given native width.
    Via(Width),
    /// Renode "not translated": warn, read 0 / drop the write.
    Unsupported,
}

/// Raw register access used by the translation helpers (implemented by the machine for a
/// peripheral port, and by test fixtures).
pub trait RegisterAccess {
    fn read(&mut self, offset: u32, width: Width) -> u32;
    fn write(&mut self, offset: u32, width: Width, value: u32);
}

/// Serves a read whose width is not native, exactly like Renode's `Read*Using*` helpers.
/// Returns `None` for [`Resolution::Unsupported`]. Native widths are passed through.
pub fn translate_read<A: RegisterAccess>(policy: AccessPolicy, port: &mut A, offset: u32, width: Width) -> Option<u32> {
    match policy.resolve(width) {
        Resolution::Native => Some(port.read(offset, width) & width.mask()),
        Resolution::Unsupported => None,
        Resolution::Via(Width::Word) => {
            // ReadByteUsingDoubleWord / ReadWordUsingDoubleWord
            let aligned = offset & !3;
            let shift = (offset & 3) * 8;
            Some((port.read(aligned, Width::Word) >> shift) & width.mask())
        }
        Resolution::Via(Width::Half) => {
            if width == Width::Byte {
                // ReadByteUsingWord
                let aligned = offset & !1;
                let shift = (offset & 1) * 8;
                Some((port.read(aligned, Width::Half) >> shift) & 0xFF)
            } else {
                // ReadDoubleWordUsingWord: low halfword first
                let low = port.read(offset, Width::Half) & 0xFFFF;
                let high = port.read(offset.wrapping_add(2), Width::Half) & 0xFFFF;
                Some(low | (high << 16))
            }
        }
        Resolution::Via(Width::Byte) => {
            // ReadWordUsingByte / ReadDoubleWordUsingByte: lowest address first
            let mut value = 0u32;
            for i in 0..width.bytes() {
                value |= (port.read(offset.wrapping_add(i), Width::Byte) & 0xFF) << (8 * i);
            }
            Some(value)
        }
    }
}

/// Serves a write whose width is not native, exactly like Renode's `Write*Using*` helpers
/// (sub-word writes through a wider register are read-modify-write, and the read has the
/// register's normal side effects). Returns `false` for [`Resolution::Unsupported`].
pub fn translate_write<A: RegisterAccess>(policy: AccessPolicy, port: &mut A, offset: u32, width: Width, value: u32) -> bool {
    let value = value & width.mask();
    match policy.resolve(width) {
        Resolution::Native => {
            port.write(offset, width, value);
            true
        }
        Resolution::Unsupported => false,
        Resolution::Via(Width::Word) => {
            // WriteByteUsingDoubleWord / WriteWordUsingDoubleWord
            let aligned = offset & !3;
            let shift = (offset & 3) * 8;
            let old = port.read(aligned, Width::Word) & !(width.mask().wrapping_shl(shift));
            port.write(aligned, Width::Word, old | value.wrapping_shl(shift));
            true
        }
        Resolution::Via(Width::Half) => {
            if width == Width::Byte {
                // WriteByteUsingWord
                let aligned = offset & !1;
                let shift = (offset & 1) * 8;
                let old = port.read(aligned, Width::Half) & 0xFFFF & !(0xFFu32 << shift);
                port.write(aligned, Width::Half, (old | (value << shift)) & 0xFFFF);
            } else {
                // WriteDoubleWordUsingWord: low halfword first
                port.write(offset, Width::Half, value & 0xFFFF);
                port.write(offset.wrapping_add(2), Width::Half, value >> 16);
            }
            true
        }
        Resolution::Via(Width::Byte) => {
            // WriteWordUsingByte / WriteDoubleWordUsingByte: lowest address first
            for i in 0..width.bytes() {
                port.write(offset.wrapping_add(i), Width::Byte, (value >> (8 * i)) & 0xFF);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32-bit register bank that records every access it receives.
    #[derive(Default)]
    struct Bank {
        regs: [u32; 4],
        log: Vec<String>,
    }

    impl RegisterAccess for Bank {
        fn read(&mut self, offset: u32, width: Width) -> u32 {
            self.log.push(format!("r{}@{:x}", width.bytes(), offset));
            let word = self.regs[(offset as usize / 4) % 4];
            (word >> ((offset & 3) * 8)) & width.mask()
        }

        fn write(&mut self, offset: u32, width: Width, value: u32) {
            self.log.push(format!("w{}@{:x}={:x}", width.bytes(), offset, value));
            self.regs[(offset as usize / 4) % 4] = value;
        }
    }

    const WORD_BYTE_HALF: AccessPolicy =
        AccessPolicy::WORD_ONLY.with_translations(Translations(Translations::BYTE_TO_WORD.0 | Translations::HALF_TO_WORD.0));

    #[test]
    fn width_helpers() {
        assert_eq!(Width::Byte.bytes(), 1);
        assert_eq!(Width::Half.bits(), 16);
        assert_eq!(Width::Word.mask(), u32::MAX);
        assert_eq!(Width::from_bytes(2), Some(Width::Half));
        assert_eq!(Width::from_bytes(3), None);
        assert_eq!(Width::Half.renode_name(), "Word");
        assert!(Widths::ALL.contains(Width::Half));
        assert!(!Widths::WORD.contains(Width::Byte));
        assert!((Widths::BYTE | Widths::WORD).contains(Width::Word));
        assert!(!Translations::NONE.contains(Translations::NONE));
    }

    #[test]
    fn resolve_follows_renode_priority() {
        // STM32_Timer / STM32F7_USART: ByteToDoubleWord | WordToDoubleWord.
        let timer = AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD | Translations::HALF_TO_WORD);
        assert_eq!(timer.resolve(Width::Word), Resolution::Native);
        assert_eq!(timer.resolve(Width::Half), Resolution::Via(Width::Word));
        assert_eq!(timer.resolve(Width::Byte), Resolution::Via(Width::Word));
        // STM32_GPIOPort: WordToDoubleWord only; bytes are "not translated".
        let gpio = AccessPolicy::WORD_ONLY.with_translations(Translations::HALF_TO_WORD);
        assert_eq!(gpio.resolve(Width::Half), Resolution::Via(Width::Word));
        assert_eq!(gpio.resolve(Width::Byte), Resolution::Unsupported);
        // STM32F7_I2C: ByteToDoubleWord only; halfwords are "not translated".
        let i2c = AccessPolicy::WORD_ONLY.with_translations(Translations::BYTE_TO_WORD);
        assert_eq!(i2c.resolve(Width::Byte), Resolution::Via(Width::Word));
        assert_eq!(i2c.resolve(Width::Half), Resolution::Unsupported);
        // No attribute: nothing is translated.
        assert_eq!(AccessPolicy::WORD_ONLY.resolve(Width::Byte), Resolution::Unsupported);
        assert_eq!(AccessPolicy::EXACT.resolve(Width::Byte), Resolution::Native);
        // A translation needs the target width to be native.
        let broken = AccessPolicy::new(Widths::BYTE, Translations::BYTE_TO_WORD);
        assert_eq!(broken.resolve(Width::Half), Resolution::Unsupported);
        // Dword-to-smaller (QuadSPI-like: byte + word natively, halfword via bytes if allowed).
        let qspi = AccessPolicy::new(Widths::BYTE | Widths::WORD, Translations::HALF_TO_BYTE);
        assert_eq!(qspi.resolve(Width::Half), Resolution::Via(Width::Byte));
        let halves = AccessPolicy::new(Widths::HALF, Translations::WORD_TO_HALF | Translations::BYTE_TO_HALF);
        assert_eq!(halves.resolve(Width::Word), Resolution::Via(Width::Half));
        assert_eq!(halves.resolve(Width::Byte), Resolution::Via(Width::Half));
        // Preference: byte prefers the 32-bit path over the 16-bit one.
        let both = AccessPolicy::new(Widths::HALF | Widths::WORD, Translations::BYTE_TO_HALF | Translations::BYTE_TO_WORD);
        assert_eq!(both.resolve(Width::Byte), Resolution::Via(Width::Word));
    }

    #[test]
    fn byte_read_through_word() {
        let mut bank = Bank { regs: [0x1122_3344, 0, 0, 0], ..Default::default() };
        for (offset, expected) in [(0, 0x44), (1, 0x33), (2, 0x22), (3, 0x11)] {
            assert_eq!(translate_read(WORD_BYTE_HALF, &mut bank, offset, Width::Byte), Some(expected));
        }
        assert_eq!(bank.log, ["r4@0", "r4@0", "r4@0", "r4@0"]);
    }

    #[test]
    fn half_read_through_word_including_unaligned_tail() {
        let mut bank = Bank { regs: [0x1122_3344, 0, 0, 0], ..Default::default() };
        assert_eq!(translate_read(WORD_BYTE_HALF, &mut bank, 0, Width::Half), Some(0x3344));
        assert_eq!(translate_read(WORD_BYTE_HALF, &mut bank, 2, Width::Half), Some(0x1122));
        assert_eq!(translate_read(WORD_BYTE_HALF, &mut bank, 1, Width::Half), Some(0x2233));
        // Renode quirk: offset 3 returns only the top byte (no second dword is read).
        assert_eq!(translate_read(WORD_BYTE_HALF, &mut bank, 3, Width::Half), Some(0x0011));
        assert_eq!(bank.log.len(), 4);
    }

    #[test]
    fn byte_write_through_word_is_read_modify_write() {
        let mut bank = Bank { regs: [0xAABB_CCDD, 0, 0, 0], ..Default::default() };
        assert!(translate_write(WORD_BYTE_HALF, &mut bank, 2, Width::Byte, 0x5A));
        assert_eq!(bank.regs[0], 0xAA5A_CCDD);
        assert_eq!(bank.log, ["r4@0", "w4@0=aa5accdd"]);
        assert!(translate_write(WORD_BYTE_HALF, &mut bank, 3, Width::Byte, 0x1FF));
        assert_eq!(bank.regs[0], 0xFF5A_CCDD, "value is truncated to the access width");
    }

    #[test]
    fn half_write_through_word_matches_renode_shift_semantics() {
        let mut bank = Bank { regs: [0xAABB_CCDD, 0, 0, 0], ..Default::default() };
        assert!(translate_write(WORD_BYTE_HALF, &mut bank, 0, Width::Half, 0x1234));
        assert_eq!(bank.regs[0], 0xAABB_1234);
        assert!(translate_write(WORD_BYTE_HALF, &mut bank, 2, Width::Half, 0x5678));
        assert_eq!(bank.regs[0], 0x5678_1234);
        // Offset 3: the value's upper byte is shifted out, exactly as the C# uint shift does.
        bank.regs[0] = 0xAABB_CCDD;
        assert!(translate_write(WORD_BYTE_HALF, &mut bank, 3, Width::Half, 0x1234));
        assert_eq!(bank.regs[0], 0x34BB_CCDD);
    }

    #[test]
    fn unsupported_widths_are_reported() {
        let gpio = AccessPolicy::WORD_ONLY.with_translations(Translations::HALF_TO_WORD);
        let mut bank = Bank::default();
        assert_eq!(translate_read(gpio, &mut bank, 0, Width::Byte), None);
        assert!(!translate_write(gpio, &mut bank, 0, Width::Byte, 1));
        assert!(bank.log.is_empty(), "peripheral must not be touched");
        // Native widths pass through with the unaligned offset unchanged.
        let mut bank = Bank::default();
        assert!(translate_write(gpio, &mut bank, 2, Width::Word, 0xCAFE_F00D));
        assert_eq!(bank.log, ["w4@2=cafef00d"]);
    }

    #[test]
    fn narrow_native_widths() {
        // A byte-only device accessed with halfword/word under WORD_TO_BYTE|HALF_TO_BYTE.
        struct Bytes([u8; 8]);
        impl RegisterAccess for Bytes {
            fn read(&mut self, offset: u32, width: Width) -> u32 {
                assert_eq!(width, Width::Byte);
                u32::from(self.0[offset as usize])
            }

            fn write(&mut self, offset: u32, width: Width, value: u32) {
                assert_eq!(width, Width::Byte);
                self.0[offset as usize] = value as u8;
            }
        }
        let policy = AccessPolicy::new(Widths::BYTE, Translations::HALF_TO_BYTE | Translations::WORD_TO_BYTE);
        let mut dev = Bytes([0; 8]);
        assert!(translate_write(policy, &mut dev, 1, Width::Word, 0x0403_0201));
        assert_eq!(dev.0, [0, 1, 2, 3, 4, 0, 0, 0]);
        assert_eq!(translate_read(policy, &mut dev, 1, Width::Word), Some(0x0403_0201));
        assert_eq!(translate_read(policy, &mut dev, 2, Width::Half), Some(0x0302));
        assert!(translate_write(policy, &mut dev, 6, Width::Half, 0xBEEF));
        assert_eq!(&dev.0[6..], [0xEF, 0xBE]);
    }

    #[test]
    fn half_native_translations() {
        struct Halves([u16; 4]);
        impl RegisterAccess for Halves {
            fn read(&mut self, offset: u32, width: Width) -> u32 {
                assert_eq!(width, Width::Half);
                assert_eq!(offset & 1, 0, "aligned");
                u32::from(self.0[offset as usize / 2])
            }

            fn write(&mut self, offset: u32, width: Width, value: u32) {
                assert_eq!(width, Width::Half);
                self.0[offset as usize / 2] = value as u16;
            }
        }
        let policy = AccessPolicy::new(Widths::HALF, Translations::BYTE_TO_HALF | Translations::WORD_TO_HALF);
        let mut dev = Halves([0x2211, 0x4433, 0, 0]);
        assert_eq!(translate_read(policy, &mut dev, 1, Width::Byte), Some(0x22));
        assert_eq!(translate_read(policy, &mut dev, 2, Width::Byte), Some(0x33));
        assert_eq!(translate_read(policy, &mut dev, 0, Width::Word), Some(0x4433_2211));
        assert!(translate_write(policy, &mut dev, 3, Width::Byte, 0xAA));
        assert_eq!(dev.0[1], 0xAA33);
        assert!(translate_write(policy, &mut dev, 4, Width::Word, 0x8877_6655));
        assert_eq!(dev.0[2..], [0x6655, 0x8877]);
    }
}
