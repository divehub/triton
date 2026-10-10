//! NGC main 5.8 / handset 65.3 system: firmware loading, board assembly,
//! NGC-specific peripheral models, dual-board scheduling and runner fixtures.
//! See `DESIGN.md`.
//!
//! Layers (bottom to top):
//!
//! * [`sha256`], [`srec`], [`firmware`]: identity, verification and flash images of the two SRECs;
//! * [`memory`], [`bus`], [`board`]: the NGC memory map, the [`armv7m::CpuBus`] implementation and
//!   the per-board run loop on top of `emu_core::MachineCore`;
//! * [`models`], [`handset`], [`main_board`]: NGC-specific peripherals and board assembly;
//! * [`system`], [`fixtures`], [`state`], [`persistence`]: the dual system and runner parity.

pub mod board;
pub mod bus;
pub mod firmware;
pub mod memory;
pub mod sha256;
pub mod srec;

pub mod actions;
pub mod deco;
pub mod eeprom_init;
pub mod fixtures;
pub mod handset;
pub mod main_board;
pub mod models;
pub mod persistence;
pub mod png;
pub mod rtc_init;
pub mod scenario;
pub mod session;
pub mod state;
pub mod surface_start;
pub mod system;

pub use board::{Board, BoardConfig, BoardStats, CpuCore, RunReport};
pub use bus::BusView;
pub use firmware::{Firmware, FirmwareError, Role};
