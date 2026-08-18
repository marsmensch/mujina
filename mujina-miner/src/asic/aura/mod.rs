//! Aura ASIC family chip support (Apollo III integration).
//!
//! This module provides the Aura ASIC protocol core: CRC-32, 16-byte
//! command/response frames with 20-byte zero preamble handling, 92-byte
//! job frames, hit responses and PLL helpers.

pub mod crc;
pub mod error;
pub mod protocol;

#[cfg(test)]
mod reference_tests;
#[cfg(test)]
pub mod test_data;

// Re-export commonly used types.
pub use protocol::{Command, FrameCodec, HitFrame, JobFrame, Register, Response};
