// Ported from Renode 1.17.0 src/Emulator/Main/Peripherals/Miscellaneous/CombinedInput.cs
// (MIT License, Copyright (c) Antmicro).

//! `Miscellaneous.CombinedInput`: ORs `numberOfInputs` signals into one (`exti5to9` with 5 inputs and
//! `exti10to15` with 6 inputs in both platforms, each wired to one NVIC line).
//!
//! * input lines `0..numberOfInputs`: `OnGPIO(n, level)` stores the level and re-evaluates the output;
//! * output line 0 (`OutputLine`, the unnamed `-> nvic@23` of the `.repl`): high while any input is high.
//!   Like every Renode `GPIO` it only notifies its targets when the level changes, so the NVIC sees a
//!   rising edge when the first input rises and a falling one when the last input falls.
//!
//! The model is not memory mapped (`@ none`); `read`/`write` exist only to satisfy the `Peripheral` trait.

use emu_core::{impl_peripheral_any, Ctx, Peripheral, View, Width};

/// The single output line.
pub const OUTPUT_LINE: u32 = 0;

/// Warn-once key of the "unsupported port" error.
const KEY_BAD_PORT: u64 = 1 << 40;

pub struct CombinedInput {
    name: String,
    /// `inputStates`.
    inputs: Vec<bool>,
}

impl CombinedInput {
    pub fn new(name: impl Into<String>, number_of_inputs: usize) -> Self {
        Self { name: name.into(), inputs: vec![false; number_of_inputs] }
    }

    pub fn number_of_inputs(&self) -> usize {
        self.inputs.len()
    }

    /// Level last received on input `number`.
    pub fn input(&self, number: usize) -> bool {
        self.inputs.get(number).copied().unwrap_or(false)
    }

    /// Level of the output line (any input high).
    pub fn output(&self) -> bool {
        self.inputs.iter().any(|&level| level)
    }

    /// One-line state (the `summary` text).
    pub fn describe(&self) -> String {
        let bits: String = self.inputs.iter().map(|&level| if level { '1' } else { '0' }).collect();
        format!("{}: inputs={bits} output={}", self.name, u8::from(self.output()))
    }
}

impl Peripheral for CombinedInput {
    fn name(&self) -> &str {
        &self.name
    }

    /// `Reset`: inputs cleared, output line dropped.
    fn reset(&mut self, ctx: &mut Ctx<'_>) {
        self.inputs.iter_mut().for_each(|level| *level = false);
        ctx.set_output(OUTPUT_LINE, false);
    }

    fn read(&mut self, _offset: u32, _width: Width, _ctx: &mut Ctx<'_>) -> u32 {
        0
    }

    fn write(&mut self, _offset: u32, _width: Width, _value: u32, _ctx: &mut Ctx<'_>) {}

    fn on_input(&mut self, line: u32, level: bool, ctx: &mut Ctx<'_>) {
        let Some(state) = self.inputs.get_mut(line as usize) else {
            ctx.error_once(
                KEY_BAD_PORT | u64::from(line),
                format_args!(
                    "Received GPIO signal on an unsupported port #{line} (supported ports are 0 - {}). Please check the platform configuration",
                    self.inputs.len() as i64 - 1
                ),
            );
            return;
        };
        *state = level;
        let any = self.inputs.iter().any(|&level| level);
        ctx.set_output(OUTPUT_LINE, any);
    }

    fn summary(&self, _view: &View<'_>) -> String {
        self.describe()
    }

    impl_peripheral_any!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_core::testing::{Harness, IrqChange};
    use emu_core::{LogLevel, PeriphId};

    fn setup(inputs: usize) -> (Harness, PeriphId) {
        let mut h = Harness::new();
        let id = h.add(CombinedInput::new("exti5to9", inputs));
        (h, id)
    }

    fn levels(changes: Vec<(emu_core::Time, bool)>) -> Vec<bool> {
        changes.into_iter().map(|(_, level)| level).collect()
    }

    #[test]
    fn output_is_the_or_of_the_inputs() {
        let (mut h, id) = setup(5);
        let probe = h.probe(id, OUTPUT_LINE);
        h.set_input(id, 2, true);
        assert!(h.get::<CombinedInput>(id).output());
        h.set_input(id, 4, true);
        h.set_input(id, 2, false);
        assert!(h.get::<CombinedInput>(id).output(), "input 4 is still high");
        assert!(h.get::<CombinedInput>(id).input(4) && !h.get::<CombinedInput>(id).input(2));
        h.set_input(id, 4, false);
        assert!(!h.get::<CombinedInput>(id).output());
        // Only the transitions of the OR reach the target, not every input change.
        assert_eq!(levels(h.probe_changes(probe)), [true, false]);
    }

    #[test]
    fn repeated_levels_do_not_notify_again() {
        let (mut h, id) = setup(6);
        let probe = h.probe(id, OUTPUT_LINE);
        h.set_input(id, 0, true);
        h.set_input(id, 0, true);
        h.set_input(id, 1, true);
        h.set_input(id, 0, false);
        h.set_input(id, 1, false);
        h.set_input(id, 1, false);
        assert_eq!(levels(h.probe_changes(probe)), [true, false]);
    }

    #[test]
    fn drives_an_irq_line_like_the_platform_wiring() {
        let (mut h, id) = setup(5);
        h.connect_irq(id, OUTPUT_LINE, 23);
        assert_eq!(h.irq_changes(), [IrqChange { time: 0, irq: 23, level: false }], "connect pushes the low level");
        h.clear_irq_changes();
        h.set_input(id, 3, true);
        assert!(h.irq_level(23));
        h.set_input(id, 3, false);
        assert!(!h.irq_level(23));
        assert_eq!(h.irq_changes().len(), 2);
    }

    #[test]
    fn unsupported_port_is_an_error_and_changes_nothing() {
        let (mut h, id) = setup(5);
        let probe = h.probe(id, OUTPUT_LINE);
        h.set_input(id, 5, true);
        h.set_input(id, 5, true);
        assert!(!h.get::<CombinedInput>(id).output());
        assert!(h.probe_changes(probe).is_empty());
        let errors: Vec<String> = h.drain_log().into_iter().filter(|e| e.level == LogLevel::Error).map(|e| e.message).collect();
        assert_eq!(
            errors,
            ["Received GPIO signal on an unsupported port #5 (supported ports are 0 - 4). Please check the platform configuration"]
        );
    }

    #[test]
    fn reset_clears_the_inputs_and_drops_the_output() {
        let (mut h, id) = setup(6);
        let probe = h.probe(id, OUTPUT_LINE);
        h.set_input(id, 1, true);
        h.set_input(id, 5, true);
        h.core_mut().reset_all();
        assert!(!h.get::<CombinedInput>(id).output());
        assert!(!h.get::<CombinedInput>(id).input(5));
        assert_eq!(levels(h.probe_changes(probe)), [true, false]);
        // A single input after the reset raises the output again.
        h.set_input(id, 0, true);
        assert_eq!(levels(h.probe_changes(probe)), [true, false, true]);
    }

    #[test]
    fn connecting_pushes_the_current_output_level() {
        let (mut h, id) = setup(5);
        h.set_input(id, 0, true);
        let late = h.probe(id, OUTPUT_LINE);
        let events = h.probe_events(late);
        assert_eq!(events.len(), 1);
        assert!(events[0].level, "a target connected while the output is high receives the high level");
    }

    #[test]
    fn summary_shows_inputs_and_output() {
        let (mut h, id) = setup(5);
        h.set_input(id, 1, true);
        h.set_input(id, 4, true);
        assert_eq!(h.get::<CombinedInput>(id).describe(), "exti5to9: inputs=01001 output=1");
        assert!(h.core().summaries().iter().any(|(n, s)| n == "exti5to9" && s == "exti5to9: inputs=01001 output=1"));
        assert_eq!(h.get::<CombinedInput>(id).number_of_inputs(), 5);
    }
}
