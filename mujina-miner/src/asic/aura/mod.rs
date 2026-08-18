//! Aura ASIC family chip support (Apollo III integration).
//!
//! This module provides the Aura ASIC protocol core: CRC-32, 16-byte
//! command/response frames with 20-byte zero preamble handling, 92-byte
//! job frames, hit responses and PLL helpers. On top of the SG1 protocol
//! core, SG2 adds the chain driver: discovery, version bounds, DVFS setup
//! and heartbeat, the frequency ramp, telemetry decoders and the
//! [`thread::AuraThread`] HashThread implementation.

pub mod chain;
pub mod crc;
pub mod dvfs;
pub mod error;
pub mod protocol;
pub mod telemetry;
pub mod thread;

#[cfg(test)]
mod chain_tests;
#[cfg(test)]
mod reference_tests;
#[cfg(test)]
pub mod test_data;
#[cfg(test)]
mod test_util;
#[cfg(test)]
mod thread_tests;

// Re-export commonly used types.
pub use chain::{ChainConfig, bring_up_chain, discover_chips, version_bounds};
pub use dvfs::{RampState, ramp_steps, write_ramp_step};
pub use protocol::{Command, FrameCodec, HitFrame, JobFrame, Register, Response};
pub use telemetry::{ChipTelemetry, TelemetryTracker, clock_mhz, hashrate_ghs, voltage};
pub use thread::{AuraConfig, AuraThread, BaudSwitch};
