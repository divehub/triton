// Ported from Renode 1.17.0 src/Emulator/Main/Core/Structure/Registers/{PeripheralRegister,RegisterField,
// RegisterCollection,PeripheralRegisterExtensions,FieldMode}.cs (MIT License, Copyright (c) Antmicro).

//! The subset of Renode's register framework (`DoubleWordRegister` / `DoubleWordRegisterCollection`)
//! that the models of this crate are built on, with the same order of operations so that
//! firmware-visible quirks carry over (`docs/renode-semantics.md` section 11).
//!
//! A register has an *underlying value* (`PeripheralRegister.UnderlyingValue`), disjoint *fields* and
//! *tags* (bits that are known but not implemented). The rules reproduced here:
//!
//! * **read**: every field with a value provider replaces its bits of the underlying value with the
//!   provider's result (the provider's result becomes the stored value), then the value returned to
//!   the caller is the underlying value with the bits of non-readable fields cleared; `ReadToClear` /
//!   `ReadToSet` fields change the underlying value *after* the value to return was captured;
//! * **write**: `difference = underlying ^ value` is taken first; every field with a write mode updates its
//!   bits of the underlying value (only when the written value differs for `Write`, with the matching
//!   condition for the other modes); then **every** field's write callback runs, whether or not anything
//!   changed, then the change callbacks of the fields whose stored bits were updated, then the
//!   register-level write callback; finally bits that differ and belong to no field are reported
//!   ("Unhandled write ... Tags: ...") when they overlap a tag;
//! * **unknown offsets**: `Unhandled read from offset 0x<o>.` / `Unhandled write to offset 0x<o>, value
//!   0x<v>.`, read 0 / write dropped; offsets are matched exactly (a register at 0x14 is not reached by an
//!   access at 0x15).
//!
//! Not ported (not used by the models in this crate): read callbacks, register-level read/change
//! callbacks, shadow reload, tags with allowed values, non-soft-resettable fields.
//!
//! Usage pattern (the model state and the register file are separate struct members so that callbacks
//! can borrow the model mutably while the engine borrows the register file):
//!
//! ```ignore
//! struct Device { regs: RegisterFile, dev: DeviceState }
//! impl Peripheral for Device {
//!     fn read(&mut self, offset: u32, _w: Width, ctx: &mut Ctx<'_>) -> u32 { self.regs.read(offset, &mut self.dev, ctx) }
//!     fn write(&mut self, offset: u32, _w: Width, v: u32, ctx: &mut Ctx<'_>) { self.regs.write(offset, v, &mut self.dev, ctx) }
//! }
//! impl Model for DeviceState { /* provide / field_written / field_changed / register_written */ }
//! ```

use emu_core::{Ctx, LogLevel};
use std::fmt::Write as _;
use std::ops::BitOr;

/// Read half of a Renode `FieldMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// Not readable (a write-only field): its bits read as 0.
    None,
    /// `FieldMode.Read`.
    Read,
    /// `FieldMode.ReadToClear`: the returned value is the stored one, the stored bits are cleared afterwards.
    ReadToClear,
    /// `FieldMode.ReadToSet`.
    ReadToSet,
}

/// Write half of a Renode `FieldMode` (the write flags are mutually exclusive in Renode too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Not writable (a read-only field).
    None,
    /// `FieldMode.Write`: the bits are replaced when the written value differs from the stored one.
    Write,
    /// `FieldMode.Set`: written 1s set bits.
    Set,
    /// `FieldMode.Toggle`: written 1s toggle bits.
    Toggle,
    /// `FieldMode.WriteOneToClear`: written 1s clear bits (only effective on bits that are set).
    WriteOneToClear,
    /// `FieldMode.WriteZeroToClear`.
    WriteZeroToClear,
    /// `FieldMode.WriteZeroToSet`.
    WriteZeroToSet,
    /// `FieldMode.WriteZeroToToggle`.
    WriteZeroToToggle,
    /// `FieldMode.WriteToClear`: any write clears the field.
    WriteToClear,
    /// `FieldMode.WriteToSet`: any write sets the field.
    WriteToSet,
}

/// A Renode `FieldMode` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub read: ReadMode,
    pub write: WriteMode,
}

impl Mode {
    pub const fn new(read: ReadMode, write: WriteMode) -> Mode {
        Mode { read, write }
    }

    /// `FieldMode.Read`.
    pub const READ: Mode = Mode::new(ReadMode::Read, WriteMode::None);
    /// `FieldMode.Write`.
    pub const WRITE: Mode = Mode::new(ReadMode::None, WriteMode::Write);
    /// `FieldMode.Read | FieldMode.Write` (the default of every `With*Field`).
    pub const READ_WRITE: Mode = Mode::new(ReadMode::Read, WriteMode::Write);
    /// `FieldMode.Read | FieldMode.WriteOneToClear`.
    pub const READ_WRITE_ONE_TO_CLEAR: Mode = Mode::new(ReadMode::Read, WriteMode::WriteOneToClear);
}

/// Which callbacks of a field the model implements (`valueProviderCallback`, `writeCallback`,
/// `changeCallback`). The engine only calls the [`Model`] method for flagged fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hooks(u8);

impl Hooks {
    pub const NONE: Hooks = Hooks(0);
    /// `valueProviderCallback`: [`Model::provide`] supplies the field's value on every read.
    pub const PROVIDER: Hooks = Hooks(1);
    /// `writeCallback`: [`Model::field_written`] runs on every write of the register.
    pub const WRITE: Hooks = Hooks(2);
    /// `changeCallback`: [`Model::field_changed`] runs when a write updated the field's stored bits.
    pub const CHANGE: Hooks = Hooks(4);

    pub const fn contains(self, other: Hooks) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Hooks {
    type Output = Hooks;

    fn bitor(self, rhs: Hooks) -> Hooks {
        Hooks(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug)]
struct Field {
    pos: u32,
    mask: u32,
    mode: Mode,
    hooks: Hooks,
}

#[derive(Clone, Debug)]
struct Tag {
    name: String,
    pos: u32,
    mask: u32,
    silent: bool,
}

const fn bit_mask(pos: u32, width: u32) -> u32 {
    if width == 0 {
        0
    } else if width >= 32 {
        u32::MAX
    } else {
        ((1u32 << width) - 1) << pos
    }
}

/// Description of one register (`DoubleWordRegister` built with the fluent `With*` API).
#[derive(Clone, Debug)]
pub struct RegisterBuilder {
    offset: u32,
    reset: u32,
    fields: Vec<Field>,
    tags: Vec<Tag>,
    write_hook: bool,
}

impl RegisterBuilder {
    /// A register at `offset` with reset value 0.
    pub fn new(offset: u32) -> Self {
        Self { offset, reset: 0, fields: Vec::new(), tags: Vec::new(), write_hook: false }
    }

    /// Reset value (`new DoubleWordRegister(this, resetValue)`).
    pub fn reset_value(mut self, reset: u32) -> Self {
        self.reset = reset;
        self
    }

    fn check_range(&self, pos: u32, width: u32, what: &str) {
        assert!(pos + width <= 32, "register 0x{:X}: {what} does not fit in the register", self.offset);
        let end = pos + width;
        let others = self.fields.iter().map(|f| (f.pos, f.mask)).chain(self.tags.iter().map(|t| (t.pos, t.mask)));
        for (other_pos, other_mask) in others {
            let other_end = other_pos + other_mask.count_ones();
            assert!(
                end.min(other_end) <= pos.max(other_pos),
                "register 0x{:X}: {what} intersects another field or tag",
                self.offset
            );
        }
    }

    /// `WithValueField` / `WithFlag` / `WithEnumField`: one field of `width` bits at `pos`.
    pub fn field(mut self, pos: u32, width: u32, mode: Mode, hooks: Hooks) -> Self {
        assert!(width >= 1, "register 0x{:X}: field has zero width", self.offset);
        self.check_range(pos, width, "field");
        self.fields.push(Field { pos, mask: bit_mask(pos, width), mode, hooks });
        self
    }

    /// `WithValueFields` / `WithEnumFields` / `WithFlags`: `count` consecutive equal fields (field `i` is at
    /// `pos + i * width`; callbacks identify it by its index).
    pub fn fields(mut self, pos: u32, width: u32, count: u32, mode: Mode, hooks: Hooks) -> Self {
        for i in 0..count {
            self = self.field(pos + i * width, width, mode, hooks);
        }
        self
    }

    /// `WithTag`: bits that exist but are not implemented; writing 1s there logs a warning.
    pub fn tag(mut self, name: &str, pos: u32, width: u32) -> Self {
        self.check_range(pos, width, "tag");
        self.tags.push(Tag { name: name.to_string(), pos, mask: bit_mask(pos, width), silent: false });
        self
    }

    /// `WithTag(..., silent: true)`: the warning is logged at `Noisy` level.
    pub fn silent_tag(mut self, name: &str, pos: u32, width: u32) -> Self {
        self.check_range(pos, width, "tag");
        self.tags.push(Tag { name: name.to_string(), pos, mask: bit_mask(pos, width), silent: true });
        self
    }

    /// `WithTaggedFlag`.
    pub fn tagged_flag(self, name: &str, pos: u32) -> Self {
        self.tag(name, pos, 1)
    }

    /// `WithTaggedFlag(name_i, pos + i)` for `i in 0..count` with names `"{prefix}{i}"`.
    pub fn tagged_flags(mut self, prefix: &str, pos: u32, count: u32) -> Self {
        for i in 0..count {
            let name = format!("{prefix}{i}");
            self = self.tagged_flag(&name, pos + i);
        }
        self
    }

    /// `WithReservedBits`: a tag named `RESERVED` (also just a tag in Renode: writing 1s warns).
    pub fn reserved(self, pos: u32, width: u32) -> Self {
        self.tag("RESERVED", pos, width)
    }

    /// `WithWriteCallback`: [`Model::register_written`] runs after the field callbacks of every write.
    pub fn on_write(mut self) -> Self {
        self.write_hook = true;
        self
    }
}

struct Reg {
    offset: u32,
    reset: u32,
    under: u32,
    fields: Vec<Field>,
    tags: Vec<Tag>,
    defined: u32,
    write_hook: bool,
}

/// Callbacks of a model (the closures of the Renode fluent API). Every method receives the register file
/// so it can read or patch the stored value of any field (`IValueRegisterField.Value`), the context for
/// outputs and logging, and the register/field it was called for (registers are numbered in the order
/// they were given to [`RegisterFile::new`], fields in the order they were declared on the register).
#[allow(unused_variables)]
pub trait Model {
    /// `valueProviderCallback`: returns the value the field exposes on a read; `current` is the stored one.
    fn provide(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, field: usize, current: u32) -> u32 {
        current
    }

    /// `writeCallback(oldFieldValue, writtenFieldValue)`: on every write, even when nothing changed.
    fn field_written(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, field: usize, old: u32, written: u32) {}

    /// `changeCallback(oldFieldValue, newFieldValue)`: only when the write updated the field's stored bits.
    fn field_changed(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, field: usize, old: u32, new: u32) {}

    /// Register-level `WithWriteCallback(oldRegisterValue, writtenValue)`.
    fn register_written(&mut self, regs: &mut RegisterFile, ctx: &mut Ctx<'_>, reg: usize, old: u32, written: u32) {}
}

/// A model without callbacks (plain storage registers).
pub struct NoCallbacks;

impl Model for NoCallbacks {}

/// `DoubleWordRegisterCollection`: registers keyed by exact (4-byte aligned) offsets.
pub struct RegisterFile {
    regs: Vec<Reg>,
    /// `lookup[offset / 4]` = register index + 1, 0 for "no register".
    lookup: Vec<u8>,
    /// `RegisterMapper` of a `BasicDoubleWordPeripheral`: register names by offset (sorted), used to annotate
    /// the unhandled-access messages. Empty for peripherals that are not derived from it.
    offset_names: Vec<(u32, &'static str)>,
}

const KEY_READ: u64 = 1 << 62;
const KEY_WRITE: u64 = 1 << 61;
const KEY_TAGS: u64 = 1 << 60;
const KEY_TAGS_SILENT: u64 = 1 << 59;

impl RegisterFile {
    /// Builds the collection. Register `i` of `registers` has index `i` in the callbacks.
    pub fn new(registers: Vec<RegisterBuilder>) -> Self {
        assert!(registers.len() < 255, "too many registers");
        let mut max_offset = 0;
        for r in &registers {
            assert!(r.offset % 4 == 0, "register offset 0x{:X} is not 4-byte aligned", r.offset);
            max_offset = max_offset.max(r.offset);
        }
        let mut lookup = vec![0u8; (max_offset / 4 + 1) as usize];
        let mut regs = Vec::with_capacity(registers.len());
        for (index, b) in registers.into_iter().enumerate() {
            let slot = &mut lookup[(b.offset / 4) as usize];
            assert!(*slot == 0, "duplicate register offset 0x{:X}", b.offset);
            *slot = index as u8 + 1;
            let defined = b.fields.iter().fold(0, |mask, f| mask | f.mask);
            regs.push(Reg {
                offset: b.offset,
                reset: b.reset,
                under: b.reset,
                fields: b.fields,
                tags: b.tags,
                defined,
                write_hook: b.write_hook,
            });
        }
        Self { regs, lookup, offset_names: Vec::new() }
    }

    /// Enables the `RegisterMapper` annotation of unhandled reads and writes
    /// (`Unhandled read from offset 0x18 (PendingRegister+0x4).`), which Renode adds for peripherals derived
    /// from `BasicDoubleWordPeripheral` (the EXTI). `names` are the members of the peripheral's register enum.
    pub fn with_offset_names(mut self, names: &[(u32, &'static str)]) -> Self {
        self.offset_names = names.to_vec();
        self.offset_names.sort_unstable();
        self
    }

    /// `RegisterMapper.ToString` as the ` (name)` suffix of the unhandled-access messages: the register name,
    /// `name+0x<delta>` past the closest lower register, or `unknown`; empty without a register enum.
    fn offset_label(&self, offset: u32) -> String {
        if self.offset_names.is_empty() {
            return String::new();
        }
        let text = match self.offset_names.iter().rev().find(|(o, _)| *o <= offset) {
            Some((o, name)) if *o == offset => (*name).to_string(),
            Some((o, name)) => format!("{name}+0x{:x}", offset - o),
            None => "unknown".to_string(),
        };
        format!(" ({text})")
    }

    /// Index of the register at exactly `offset`.
    pub fn index_of(&self, offset: u32) -> Option<usize> {
        if offset & 3 != 0 {
            return None;
        }
        match self.lookup.get((offset >> 2) as usize) {
            Some(&slot) if slot != 0 => Some(slot as usize - 1),
            _ => None,
        }
    }

    /// Offset of register `reg`.
    pub fn offset_of(&self, reg: usize) -> u32 {
        self.regs[reg].offset
    }

    /// `RegistersCollection.Reset()`: every register's underlying value returns to its reset value.
    pub fn reset(&mut self) {
        for r in &mut self.regs {
            r.under = r.reset;
        }
    }

    /// The register's underlying value (`PeripheralRegister.Value`): no providers, no callbacks.
    pub fn value(&self, reg: usize) -> u32 {
        self.regs[reg].under
    }

    pub fn set_value(&mut self, reg: usize, value: u32) {
        self.regs[reg].under = value;
    }

    /// `IRegisterField.Value` getter: the stored bits of one field.
    pub fn field(&self, reg: usize, field: usize) -> u32 {
        let r = &self.regs[reg];
        let f = r.fields[field];
        (r.under & f.mask) >> f.pos
    }

    /// `IRegisterField.Value` setter: no callbacks.
    pub fn set_field(&mut self, reg: usize, field: usize, value: u32) {
        let r = &mut self.regs[reg];
        let f = r.fields[field];
        debug_assert!(value & !(f.mask >> f.pos) == 0, "value exceeds the size of the field");
        r.under = (r.under & !f.mask) | ((value << f.pos) & f.mask);
    }

    /// `BaseRegisterCollection.Read`.
    pub fn read<M: Model>(&mut self, offset: u32, model: &mut M, ctx: &mut Ctx<'_>) -> u32 {
        let Some(reg) = self.index_of(offset) else {
            log_unhandled_read_labeled(ctx, offset, &self.offset_label(offset));
            return 0;
        };
        let field_count = self.regs[reg].fields.len();
        // Value providers, in definition order; their results become the stored value.
        for k in 0..field_count {
            let f = self.regs[reg].fields[k];
            if f.hooks.contains(Hooks::PROVIDER) {
                let current = (self.regs[reg].under & f.mask) >> f.pos;
                let provided = model.provide(self, ctx, reg, k, current);
                let r = &mut self.regs[reg];
                r.under = (r.under & !f.mask) | ((provided << f.pos) & f.mask);
            }
        }
        let mut to_read = self.regs[reg].under;
        for k in 0..field_count {
            let f = self.regs[reg].fields[k];
            if f.mode.read == ReadMode::None {
                to_read &= !f.mask;
            }
            let r = &mut self.regs[reg];
            match f.mode.read {
                ReadMode::ReadToClear if r.under & f.mask != 0 => r.under &= !f.mask,
                ReadMode::ReadToSet if !r.under & f.mask != 0 => r.under |= f.mask,
                _ => {}
            }
        }
        to_read
    }

    /// `BaseRegisterCollection.Write`.
    pub fn write<M: Model>(&mut self, offset: u32, value: u32, model: &mut M, ctx: &mut Ctx<'_>) {
        let Some(reg) = self.index_of(offset) else {
            log_unhandled_write_labeled(ctx, offset, &self.offset_label(offset), value);
            return;
        };
        let base = self.regs[reg].under;
        let difference = base ^ value;
        let field_count = self.regs[reg].fields.len();
        let mut changed = 0u64;
        for k in 0..field_count {
            let f = self.regs[reg].fields[k];
            let under = self.regs[reg].under;
            let mask = f.mask;
            let updated = match f.mode.write {
                WriteMode::None => None,
                WriteMode::Write => (difference & mask != 0).then(|| (under & !mask) | (value & mask)),
                WriteMode::Set => {
                    let set = value & !under;
                    (set & mask != 0).then(|| under | (set & mask))
                }
                WriteMode::Toggle => (value & mask != 0).then(|| under ^ (value & mask)),
                WriteMode::WriteOneToClear => ((!difference & value) & mask != 0).then(|| under & !(value & mask)),
                WriteMode::WriteZeroToClear => ((difference & under) & mask != 0).then(|| under & !(!value & mask)),
                WriteMode::WriteZeroToSet => {
                    let set = !value & !under;
                    (set & mask != 0).then(|| under | (set & mask))
                }
                WriteMode::WriteZeroToToggle => (!value & mask != 0).then(|| under ^ (!value & mask)),
                WriteMode::WriteToClear => (under & mask != 0).then(|| under & !mask),
                WriteMode::WriteToSet => (!under & mask != 0).then(|| under | mask),
            };
            if let Some(updated) = updated {
                self.regs[reg].under = updated;
                changed |= 1u64 << k;
            }
        }
        // Write callbacks of every field, changed or not.
        for k in 0..field_count {
            let f = self.regs[reg].fields[k];
            if f.hooks.contains(Hooks::WRITE) {
                model.field_written(self, ctx, reg, k, (base & f.mask) >> f.pos, (value & f.mask) >> f.pos);
            }
        }
        // Change callbacks of the fields whose stored bits were updated.
        for k in 0..field_count {
            let f = self.regs[reg].fields[k];
            if changed & (1u64 << k) != 0 && f.hooks.contains(Hooks::CHANGE) {
                let now = self.regs[reg].under;
                model.field_changed(self, ctx, reg, k, (base & f.mask) >> f.pos, (now & f.mask) >> f.pos);
            }
        }
        if self.regs[reg].write_hook {
            model.register_written(self, ctx, reg, base, value);
        }
        let unhandled = difference & !self.regs[reg].defined;
        if unhandled != 0 {
            let r = &self.regs[reg];
            log_unhandled_tags(ctx, r.offset, &r.tags, unhandled, value);
        }
    }
}

/// `LogUnhandledRead`: Warning `Unhandled read from offset 0x<o>.` (once per offset).
pub fn log_unhandled_read(ctx: &mut Ctx<'_>, offset: u32) {
    log_unhandled_read_labeled(ctx, offset, "");
}

/// `LogUnhandledWrite`: Warning `Unhandled write to offset 0x<o>, value 0x<v>.` (once per offset).
pub fn log_unhandled_write(ctx: &mut Ctx<'_>, offset: u32, value: u32) {
    log_unhandled_write_labeled(ctx, offset, "", value);
}

/// `LogUnhandledRead` with the optional ` (RegisterName+0x..)` annotation of `BasicDoubleWordPeripheral`s.
fn log_unhandled_read_labeled(ctx: &mut Ctx<'_>, offset: u32, label: &str) {
    ctx.warn_once(KEY_READ | u64::from(offset), format_args!("Unhandled read from offset 0x{offset:X}{label}."));
}

/// `LogUnhandledWrite` with the optional annotation.
fn log_unhandled_write_labeled(ctx: &mut Ctx<'_>, offset: u32, label: &str, value: u32) {
    ctx.warn_once(KEY_WRITE | u64::from(offset), format_args!("Unhandled write to offset 0x{offset:X}{label}, value 0x{value:X}."));
}

/// `BitHelper.GetSetBitsPretty`: `"0, 3-5, 7"`, or `"(none)"`.
pub fn set_bits_pretty(mask: u32) -> String {
    if mask == 0 {
        return "(none)".to_string();
    }
    let mut out = String::new();
    let mut bit = 0;
    while bit < 32 {
        if mask & (1 << bit) == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 32 && mask & (1 << (bit + 1)) != 0 {
            bit += 1;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if start == bit {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}-{bit}");
        }
        bit += 1;
    }
    out
}

/// `PeripheralRegister.LogUnhandledWrites`: written bits that belong to no field are reported when they
/// overlap a tag (non-silent tags as a warning, silent ones at Noisy level). Bits outside every tag are
/// silently ignored, as in Renode.
fn log_unhandled_tags(ctx: &mut Ctx<'_>, offset: u32, tags: &[Tag], unhandled: u32, value: u32) {
    for silent in [false, true] {
        let level = if silent { LogLevel::Noisy } else { LogLevel::Warning };
        if !ctx.log_enabled(level) || !tags.iter().any(|t| t.silent == silent && t.mask & unhandled != 0) {
            continue;
        }
        let mut names = String::new();
        for t in tags.iter().filter(|t| t.silent == silent && t.mask & unhandled != 0) {
            if !names.is_empty() {
                names.push_str(", ");
            }
            let _ = write!(names, "{} (0x{:X})", t.name, (value & t.mask) >> t.pos);
        }
        let key = (if silent { KEY_TAGS_SILENT } else { KEY_TAGS }) | (u64::from(unhandled) << 16) | u64::from(offset & 0xFFFF);
        ctx.log_once(
            level,
            key,
            format_args!(
                "Unhandled write to offset 0x{offset:X}. Unhandled bits: [{}] when writing value 0x{value:X}. Tags: {names}.",
                set_bits_pretty(unhandled)
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::Harness;
    use emu_core::{impl_peripheral_any, AccessPolicy, PeriphId, Peripheral, Width};

    const BASE: u32 = 0x4000_0000;

    // Register numbering of the toy device.
    const R_STORE: usize = 0;
    const R_MIXED: usize = 1;
    const R_W1C: usize = 2;
    const R_PROV: usize = 3;
    const R_HOOKED: usize = 4;

    #[derive(Default)]
    struct ToyState {
        provided: u32,
        events: Vec<String>,
        change_veto_field: Option<usize>,
    }

    impl Model for ToyState {
        fn provide(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, field: usize, current: u32) -> u32 {
            self.events.push(format!("provide r{reg} f{field} cur={current:X}"));
            match reg {
                R_PROV => self.provided,
                _ => current,
            }
        }

        fn field_written(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, field: usize, old: u32, written: u32) {
            self.events.push(format!("written r{reg} f{field} old={old:X} new={written:X}"));
        }

        fn field_changed(&mut self, regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, field: usize, old: u32, new: u32) {
            self.events.push(format!("changed r{reg} f{field} old={old:X} new={new:X}"));
            if self.change_veto_field == Some(field) {
                // WithConditionallyWritableValueField: restore the old value from the callback.
                regs.set_field(reg, field, old);
            }
        }

        fn register_written(&mut self, _regs: &mut RegisterFile, _ctx: &mut Ctx<'_>, reg: usize, old: u32, written: u32) {
            self.events.push(format!("register r{reg} old={old:X} new={written:X}"));
        }
    }

    /// A word-only peripheral that forwards everything to the engine.
    struct Dev {
        regs: RegisterFile,
        state: ToyState,
    }

    impl Dev {
        fn new(regs: RegisterFile) -> Self {
            Self { regs, state: ToyState::default() }
        }

        fn toy() -> Self {
            Self::new(RegisterFile::new(vec![
                // STORE: plain 32 bit storage.
                RegisterBuilder::new(0x00).field(0, 32, Mode::READ_WRITE, Hooks::NONE),
                // MIXED: RW byte, RO byte, WO nibble, then tags (bits 20..23 are neither field nor tag).
                RegisterBuilder::new(0x04)
                    .field(0, 8, Mode::READ_WRITE, Hooks::NONE)
                    .field(8, 8, Mode::READ, Hooks::NONE)
                    .field(16, 4, Mode::WRITE, Hooks::NONE)
                    .tagged_flag("FLAG", 24)
                    .tag("PAIR", 25, 2)
                    .reserved(28, 4),
                // W1C: read | write-one-to-clear byte, storage byte, reset value 0x00FF.
                RegisterBuilder::new(0x08)
                    .reset_value(0x00FF)
                    .field(0, 8, Mode::READ_WRITE_ONE_TO_CLEAR, Hooks::NONE)
                    .field(8, 8, Mode::READ_WRITE, Hooks::NONE),
                // PROV: provider + change callback field, change + write callback field.
                RegisterBuilder::new(0x0C)
                    .field(0, 8, Mode::READ_WRITE, Hooks::PROVIDER | Hooks::CHANGE)
                    .field(8, 8, Mode::READ_WRITE, Hooks::CHANGE | Hooks::WRITE),
                // HOOKED: two fields and a register-level write callback.
                RegisterBuilder::new(0x10)
                    .field(0, 16, Mode::READ_WRITE, Hooks::WRITE | Hooks::CHANGE)
                    .field(16, 16, Mode::READ_WRITE, Hooks::WRITE | Hooks::CHANGE)
                    .on_write(),
            ]))
        }

        /// One register at offset 0 with a 4 bit field at bit 0 of the given mode.
        fn single(mode: Mode, hooks: Hooks) -> Self {
            Self::new(RegisterFile::new(vec![RegisterBuilder::new(0).field(0, 4, mode, hooks)]))
        }
    }

    impl Peripheral for Dev {
        fn name(&self) -> &str {
            "dev"
        }

        fn read(&mut self, offset: u32, _width: Width, ctx: &mut Ctx<'_>) -> u32 {
            self.regs.read(offset, &mut self.state, ctx)
        }

        fn write(&mut self, offset: u32, _width: Width, value: u32, ctx: &mut Ctx<'_>) {
            self.regs.write(offset, value, &mut self.state, ctx);
        }

        fn access_policy(&self) -> AccessPolicy {
            AccessPolicy::WORD_ONLY
        }

        impl_peripheral_any!();
    }

    fn harness(dev: Dev) -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add_mapped(BASE, 0x400, dev);
        (h, id)
    }

    fn events(h: &mut Harness, id: PeriphId) -> Vec<String> {
        std::mem::take(&mut h.get_mut::<Dev>(id).state.events)
    }

    #[test]
    fn storage_register_round_trips_and_resets() {
        let (mut h, id) = harness(Dev::toy());
        assert_eq!(h.read32(BASE), 0);
        h.write32(BASE, 0xDEAD_BEEF);
        assert_eq!(h.read32(BASE), 0xDEAD_BEEF);
        h.get_mut::<Dev>(id).regs.reset();
        assert_eq!(h.read32(BASE), 0);
        assert_eq!(h.read32(BASE + 8), 0x00FF, "reset value of the W1C register");
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn engine_accessors() {
        let (mut h, id) = harness(Dev::toy());
        let dev = h.get_mut::<Dev>(id);
        assert_eq!(dev.regs.index_of(0x0C), Some(R_PROV));
        assert_eq!(dev.regs.index_of(0x0D), None, "offsets match exactly");
        assert_eq!(dev.regs.index_of(0x14), None);
        assert_eq!(dev.regs.index_of(0x1000), None);
        assert_eq!(dev.regs.offset_of(R_HOOKED), 0x10);
        dev.regs.set_value(R_STORE, 0x1234_5678);
        assert_eq!(dev.regs.value(R_STORE), 0x1234_5678);
        dev.regs.set_field(R_MIXED, 0, 0x7F);
        assert_eq!(dev.regs.field(R_MIXED, 0), 0x7F);
        assert_eq!(dev.regs.value(R_MIXED), 0x7F);
        dev.regs.set_field(R_MIXED, 1, 0xAB);
        assert_eq!(dev.regs.value(R_MIXED), 0xAB7F, "set_field touches only its own bits, read-only fields included");
    }

    #[test]
    fn read_only_write_only_and_tagged_bits() {
        let (mut h, id) = harness(Dev::toy());
        // RW byte 0xA5, RO byte 0xFF (ignored), WO nibble 3, bits 20-23 = 0xF (no field, no tag),
        // tagged flag, tag pair 0b11, reserved 0xF.
        h.write32(BASE + 4, 0xFFF3_FFA5);
        // RO bits and tags read 0, the write-only nibble reads 0, only the RW byte sticks.
        assert_eq!(h.read32(BASE + 4), 0x0000_00A5);
        // The write-only field keeps its value internally (callbacks read it through `field`).
        assert_eq!(h.get::<Dev>(id).regs.field(R_MIXED, 2), 0x3);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0],
            "Unhandled write to offset 0x4. Unhandled bits: [20-31] when writing value 0xFFF3FFA5. Tags: FLAG (0x1), PAIR (0x3), RESERVED (0xF)."
        );
        // Writing zeroes into the write-only nibble is a change of a *field*: nothing unhandled, no warning.
        h.write32(BASE + 4, 0x0000_00A5);
        assert_eq!(h.get::<Dev>(id).regs.field(R_MIXED, 2), 0);
        assert_eq!(h.warnings().len(), 1);
    }

    #[test]
    fn unhandled_tag_warning_is_deduplicated_per_offset_and_bits() {
        let (mut h, _id) = harness(Dev::toy());
        h.write32(BASE + 4, 0x0100_0000);
        h.write32(BASE + 4, 0x0000_0000);
        h.write32(BASE + 4, 0x0100_0000);
        assert_eq!(h.warnings().len(), 1, "same offset and bits: one message");
        h.write32(BASE + 4, 0x0200_0000);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 2, "different unhandled bits: a new message");
        assert!(warnings[1].contains("Unhandled bits: [25]"));
        assert!(warnings[1].ends_with("Tags: PAIR (0x1)."));
    }

    #[test]
    fn bits_outside_every_field_and_tag_are_silent() {
        let (mut h, _id) = harness(Dev::new(RegisterFile::new(vec![RegisterBuilder::new(0).field(0, 8, Mode::READ_WRITE, Hooks::NONE)])));
        // No tag: writing bits 8..31 is ignored without a log (Renode's TagLogger finds no tag).
        h.write32(BASE, 0xFFFF_FFFF);
        assert_eq!(h.read32(BASE), 0xFF);
        assert!(h.warnings().is_empty());
    }

    #[test]
    fn unknown_offsets_warn_read_zero_and_match_exactly() {
        let (mut h, _id) = harness(Dev::toy());
        assert_eq!(h.read32(BASE + 0x2C), 0);
        assert_eq!(h.read32(BASE + 0x2C), 0);
        h.write32(BASE + 0x2C, 0x1234);
        h.write32(BASE + 0x2C, 0x5678);
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x2C.", "Unhandled write to offset 0x2C, value 0x1234."]);
        // A register exists at 0x0C, but an access at 0x0D (not 4-aligned) does not reach it.
        assert_eq!(h.read(BASE + 0x0D, Width::Word), 0);
        assert!(h.warnings().contains(&"Unhandled read from offset 0xD.".to_string()));
        // Far beyond the last register.
        assert_eq!(h.read32(BASE + 0x3F0), 0);
        assert!(h.warnings().contains(&"Unhandled read from offset 0x3F0.".to_string()));
    }

    #[test]
    fn offset_names_annotate_unhandled_accesses_like_register_mapper() {
        // BasicDoubleWordPeripheral: " (RegisterName+0x<delta, lowercase hex>)" from the closest lower enum member.
        let regs = RegisterFile::new(vec![RegisterBuilder::new(0).field(0, 8, Mode::READ_WRITE, Hooks::NONE)])
            .with_offset_names(&[(0x20, "Gap"), (0x0, "Control")]);
        let (mut h, _id) = harness(Dev::new(regs));
        assert_eq!(h.read32(BASE + 0x10), 0);
        assert_eq!(h.read32(BASE + 0x20), 0);
        assert_eq!(h.read32(BASE + 0x3FC), 0);
        h.write32(BASE + 0x24, 5);
        assert_eq!(
            h.warnings(),
            [
                "Unhandled read from offset 0x10 (Control+0x10).",
                "Unhandled read from offset 0x20 (Gap).",
                "Unhandled read from offset 0x3FC (Gap+0x3dc).",
                "Unhandled write to offset 0x24 (Gap+0x4), value 0x5."
            ]
        );
        // Below the first named register: "unknown".
        let regs = RegisterFile::new(vec![RegisterBuilder::new(0).field(0, 8, Mode::READ_WRITE, Hooks::NONE)])
            .with_offset_names(&[(0x40, "High")]);
        let (mut h, _id) = harness(Dev::new(regs));
        assert_eq!(h.read32(BASE + 0x10), 0);
        assert_eq!(h.warnings(), ["Unhandled read from offset 0x10 (unknown)."]);
    }

    #[test]
    fn write_one_to_clear_clears_only_set_bits() {
        let (mut h, _id) = harness(Dev::toy());
        // W1C byte reset 0xFF, storage byte 0x00.
        assert_eq!(h.read32(BASE + 8), 0x00FF);
        h.write32(BASE + 8, 0x0F | 0x5500);
        assert_eq!(h.read32(BASE + 8), 0x55F0, "bits 0..3 cleared, storage byte replaced");
        h.write32(BASE + 8, 0x5500);
        assert_eq!(h.read32(BASE + 8), 0x55F0, "zeroes do not set or clear bits");
        h.write32(BASE + 8, 0xF0 | 0x5500);
        assert_eq!(h.read32(BASE + 8), 0x5500);
        // Writing 1 to a bit that is clear does not set it.
        h.write32(BASE + 8, 0xFF | 0x5500);
        assert_eq!(h.read32(BASE + 8), 0x5500);
    }

    #[test]
    fn provider_overrides_stored_value_and_change_needs_a_difference_from_the_stored_value() {
        let (mut h, id) = harness(Dev::toy());
        h.get_mut::<Dev>(id).state.provided = 0x42;
        // Reading refreshes the stored bits from the provider.
        assert_eq!(h.read32(BASE + 0x0C), 0x42);
        assert_eq!(h.get::<Dev>(id).regs.field(R_PROV, 0), 0x42);
        assert_eq!(events(&mut h, id), ["provide r3 f0 cur=0"]);
        // Writing the value the provider refreshed: no change callback for field 0, the write callback of
        // field 1 still runs.
        h.write32(BASE + 0x0C, 0x42);
        assert_eq!(events(&mut h, id), ["written r3 f1 old=0 new=0"]);
        // Writing a different value: change callbacks with old = refreshed value.
        h.write32(BASE + 0x0C, 0x1043);
        assert_eq!(
            events(&mut h, id),
            ["written r3 f1 old=0 new=10", "changed r3 f0 old=42 new=43", "changed r3 f1 old=0 new=10"]
        );
        // Without an intervening read the stored value is what was last written: writing it again is not
        // a change (the provider is not consulted on writes).
        h.get_mut::<Dev>(id).state.provided = 0x99;
        h.write32(BASE + 0x0C, 0x1043);
        assert_eq!(events(&mut h, id), ["written r3 f1 old=10 new=10"]);
        assert_eq!(h.read32(BASE + 0x0C), 0x1099);
    }

    #[test]
    fn provider_results_are_masked_to_the_field_width() {
        let (mut h, id) = harness(Dev::toy());
        h.write32(BASE + 0x0C, 0x5A00);
        h.get_mut::<Dev>(id).state.provided = 0x1FF;
        assert_eq!(h.read32(BASE + 0x0C), 0x5AFF, "bit 8 of the provided value must not leak into the next field");
        assert_eq!(h.get::<Dev>(id).regs.field(R_PROV, 1), 0x5A);
    }

    #[test]
    fn write_callbacks_run_for_every_field_even_without_a_change() {
        let (mut h, id) = harness(Dev::toy());
        h.write32(BASE + 0x10, 0);
        assert_eq!(
            events(&mut h, id),
            ["written r4 f0 old=0 new=0", "written r4 f1 old=0 new=0", "register r4 old=0 new=0"],
            "no change callbacks, but write callbacks and the register callback still run"
        );
        h.write32(BASE + 0x10, 0x0002_0001);
        assert_eq!(
            events(&mut h, id),
            [
                "written r4 f0 old=0 new=1",
                "written r4 f1 old=0 new=2",
                "changed r4 f0 old=0 new=1",
                "changed r4 f1 old=0 new=2",
                "register r4 old=0 new=20001"
            ],
            "order: field write callbacks, field change callbacks, register callback"
        );
    }

    #[test]
    fn a_change_callback_can_restore_the_stored_value() {
        let (mut h, id) = harness(Dev::toy());
        h.get_mut::<Dev>(id).state.change_veto_field = Some(0);
        h.write32(BASE + 0x10, 0x0007_1234);
        // Field 0 was vetoed by its own change callback, field 1 kept its value.
        assert_eq!(h.get::<Dev>(id).regs.value(R_HOOKED), 0x0007_0000);
    }

    #[test]
    fn write_modes_follow_renode_field_mode_rules() {
        use WriteMode::*;
        // (mode, stored, written, expected stored, changed callback expected)
        let cases = [
            (None, 0b0101, 0b0011, 0b0101, false),
            (Write, 0b0101, 0b0011, 0b0011, true),
            (Write, 0b0101, 0b0101, 0b0101, false),
            (Set, 0b0101, 0b0011, 0b0111, true),
            (Set, 0b0111, 0b0011, 0b0111, false),
            (Toggle, 0b0101, 0b0011, 0b0110, true),
            (Toggle, 0b0101, 0b0000, 0b0101, false),
            (WriteOneToClear, 0b0101, 0b0011, 0b0100, true),
            (WriteOneToClear, 0b0100, 0b0011, 0b0100, false),
            (WriteOneToClear, 0b0100, 0b0100, 0b0000, true),
            (WriteZeroToClear, 0b0101, 0b0011, 0b0001, true),
            (WriteZeroToClear, 0b0001, 0b1111, 0b0001, false),
            (WriteZeroToSet, 0b0101, 0b0011, 0b1101, true),
            (WriteZeroToSet, 0b1101, 0b0011, 0b1101, false),
            (WriteZeroToToggle, 0b0101, 0b0011, 0b1001, true),
            (WriteZeroToToggle, 0b0101, 0b1111, 0b0101, false),
            (WriteToClear, 0b0101, 0b1111, 0b0000, true),
            (WriteToClear, 0b0000, 0b1111, 0b0000, false),
            (WriteToSet, 0b0101, 0b0000, 0b1111, true),
            (WriteToSet, 0b1111, 0b0000, 0b1111, false),
        ];
        for (mode, stored, written, expected, changed) in cases {
            let (mut h, id) = harness(Dev::single(Mode::new(ReadMode::Read, mode), Hooks::CHANGE));
            h.get_mut::<Dev>(id).regs.set_value(0, stored);
            h.write32(BASE, written);
            assert_eq!(h.get::<Dev>(id).regs.value(0), expected, "{mode:?}: stored {stored:04b}, written {written:04b}");
            assert_eq!(!events(&mut h, id).is_empty(), changed, "{mode:?}: change callback for stored {stored:04b}, written {written:04b}");
        }
    }

    #[test]
    fn read_to_clear_and_read_to_set_return_the_stored_value_then_change_it() {
        let (mut h, id) = harness(Dev::new(RegisterFile::new(vec![RegisterBuilder::new(0)
            .field(0, 2, Mode::new(ReadMode::ReadToClear, WriteMode::Write), Hooks::NONE)
            .field(2, 2, Mode::new(ReadMode::ReadToSet, WriteMode::Write), Hooks::NONE)])));
        h.get_mut::<Dev>(id).regs.set_value(0, 0b0011);
        assert_eq!(h.read32(BASE), 0b0011, "the value read is the stored one");
        assert_eq!(h.get::<Dev>(id).regs.value(0), 0b1100, "ReadToClear cleared its bits, ReadToSet set its bits");
        assert_eq!(h.read32(BASE), 0b1100);
        assert_eq!(h.read32(BASE), 0b1100);
    }

    #[test]
    fn set_bits_pretty_matches_renode_formatting() {
        assert_eq!(set_bits_pretty(0), "(none)");
        assert_eq!(set_bits_pretty(1), "0");
        assert_eq!(set_bits_pretty(0b1011_1001), "0, 3-5, 7");
        assert_eq!(set_bits_pretty(0xFFFF_FFFF), "0-31");
        assert_eq!(set_bits_pretty(0x8000_0001), "0, 31");
        assert_eq!(set_bits_pretty(0xFFFF_0000), "16-31");
    }

    #[test]
    fn silent_tags_log_at_noisy_level_only() {
        let (mut h, _id) = harness(Dev::new(RegisterFile::new(vec![RegisterBuilder::new(0)
            .field(0, 8, Mode::READ_WRITE, Hooks::NONE)
            .silent_tag("QUIET", 8, 8)
            .tag("LOUD", 16, 8)])));
        h.write32(BASE, 0x0001_0100);
        let warnings = h.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("LOUD (0x1)") && !warnings[0].contains("QUIET"));
        h.core_mut().log.set_threshold(LogLevel::Noisy);
        h.write32(BASE, 0x0000_0200);
        let noisy: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == LogLevel::Noisy).map(|e| e.message).collect();
        assert_eq!(noisy.len(), 1);
        assert!(noisy[0].contains("QUIET (0x2)"), "{noisy:?}");
    }

    #[test]
    #[should_panic(expected = "intersects")]
    fn overlapping_fields_are_rejected_at_construction() {
        let _ = RegisterBuilder::new(0).field(0, 8, Mode::READ_WRITE, Hooks::NONE).tag("T", 4, 8);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn fields_beyond_the_register_are_rejected() {
        let _ = RegisterBuilder::new(0).field(28, 8, Mode::READ_WRITE, Hooks::NONE);
    }

    #[test]
    #[should_panic(expected = "duplicate")]
    fn duplicate_offsets_are_rejected() {
        let _ = RegisterFile::new(vec![RegisterBuilder::new(4), RegisterBuilder::new(4)]);
    }
}
