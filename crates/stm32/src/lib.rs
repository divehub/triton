//! STM32 peripheral models. Behavioral reference: the pinned Renode 1.17.0
//! sources (infrastructure commit 066a7f13c052215632d469c995c89aea37c573b1),
//! including firmware-visible quirks. See `DESIGN.md` section 8.
//! Module files are owned by the peripheral work packages; this list is
//! planner-owned.

pub mod can;
pub mod combined_input;
pub mod crc;
pub mod dma;
pub mod exti;
pub mod gpio;
pub mod i2c;
pub mod iwdg;
pub mod rng;
pub mod rtc;
pub mod timer;
pub mod usart;
