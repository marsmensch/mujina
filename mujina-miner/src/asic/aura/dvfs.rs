//! Aura DVFS: continuous heartbeat and the post-job frequency ramp.
//!
//! The DVFS heartbeat (every ~2.1 s) writes the five payload words to
//! register 0x81 with lcmd `0x1f00`; `0x5074` is the corrected vendor
//! full-rate payload (not `0x5009`). The ramp raises PLL_FREQ N from 80 to
//! 491 in +20 steps (~25 MHz each, ~50 ms apart), rewriting DUTY_CYCLE and
//! HASHCONFIG on every step. The ramp only runs once pool work is live on
//! the chips; the thread gates it accordingly.

use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncWrite;

use super::chain::{self, PLL_RAMP_END, PLL_RAMP_START, PLL_RAMP_STEP, duty_word};
use super::protocol::{Register, pll_freq_word};
use crate::tracing::prelude::*;

/// Low-level command word for the DVFS heartbeat.
pub const DVFS_HEARTBEAT_LCMD: u16 = 0x1f00;
/// The heartbeat payload: `0x5074` is the corrected vendor full-rate word.
pub const DVFS_HEARTBEAT_PAYLOAD: [u32; 5] = [0x02, 0x03, 0x01, 0x5074, 0x05];
/// Interval between DVFS heartbeats.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(2100);
/// Interval between ramp steps.
pub const RAMP_STEP_INTERVAL: Duration = Duration::from_millis(50);

/// Send one DVFS heartbeat: the five payload words to reg 0x81 (broadcast).
pub async fn heartbeat<W>(writer: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for &word in &DVFS_HEARTBEAT_PAYLOAD {
        chain::write_reg(
            writer,
            true,
            0x00,
            Register::Dvfs,
            DVFS_HEARTBEAT_LCMD,
            word,
        )
        .await?;
    }
    Ok(())
}

/// PLL N dividers for the frequency ramp: `80, 100, ..., 480, 491`.
pub fn ramp_steps() -> Vec<u32> {
    let mut steps = Vec::new();
    let mut n = PLL_RAMP_START;
    while n < PLL_RAMP_END {
        steps.push(n);
        n += PLL_RAMP_STEP;
    }
    steps.push(PLL_RAMP_END);
    steps
}

/// Write one ramp step for PLL N: PLL_FREQ, then DUTY_CYCLE + HASHCONFIG.
pub async fn write_ramp_step<W>(writer: &mut W, pll_n: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    chain::write_reg(
        writer,
        true,
        0x00,
        Register::PllFreq,
        chain::REG_LCMD,
        pll_freq_word(pll_n),
    )
    .await?;
    chain::write_reg(
        writer,
        true,
        0x00,
        Register::DutyCycle,
        chain::REG_LCMD,
        duty_word(pll_n),
    )
    .await?;
    chain::write_reg(
        writer,
        true,
        0x00,
        Register::HashConfig,
        chain::REG_LCMD,
        chain::HASHCONFIG_VALUE,
    )
    .await?;
    psu_voltage_step(writer, pll_n).await
}

/// Placeholder hook for the board-level PSU voltage climb.
///
/// Raising the PSU rail as the PLL N climbs is board-level work (PWM control
/// of the regulator) and belongs to a later phase; it is intentionally not
/// implemented here. The hook is a no-op until the board backend lands.
pub async fn psu_voltage_step<W>(_writer: &mut W, _pll_n: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    Ok(())
}

/// Ramp progress state machine.
///
/// The thread ticks [`Self::next_step`] every [`RAMP_STEP_INTERVAL`] while a
/// job is live. Ramping resumes where it left off if the thread goes idle
/// mid-ramp.
#[derive(Debug)]
pub struct RampState {
    steps: Vec<u32>,
    idx: usize,
}

impl RampState {
    /// Start at the low end of the ramp.
    pub fn new() -> Self {
        Self {
            steps: ramp_steps(),
            idx: 0,
        }
    }

    /// Whether the ramp has reached full rate.
    pub fn done(&self) -> bool {
        self.idx >= self.steps.len()
    }

    /// The next PLL N to write, advancing the state machine.
    pub fn next_step(&mut self) -> Option<u32> {
        let n = self.steps.get(self.idx).copied();
        if n.is_some() {
            self.idx += 1;
            if self.idx == self.steps.len() {
                info!(pll_n = PLL_RAMP_END, "Aura ramp reached full rate");
            }
        }
        n
    }
}

impl Default for RampState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_steps_cover_80_to_491() {
        let steps = ramp_steps();
        // 80, 100, ..., 480 (21 values) plus the final 491.
        assert_eq!(steps.len(), 22);
        assert_eq!(steps[0], 80);
        assert_eq!(steps[1], 100);
        assert_eq!(steps[steps.len() - 2], 480);
        assert_eq!(steps[steps.len() - 1], 491);
        for w in steps.windows(2) {
            let delta = w[1] - w[0];
            assert!(delta == PLL_RAMP_STEP || (w[1] == PLL_RAMP_END && delta == 11));
        }
    }

    #[test]
    fn ramp_state_advances_and_completes() {
        let mut state = RampState::new();
        assert!(!state.done());
        assert_eq!(state.next_step(), Some(80));
        assert_eq!(state.next_step(), Some(100));
        let mut n = 0;
        while state.next_step().is_some() {
            n += 1;
        }
        // 20 remaining steps after the first two.
        assert_eq!(n, 20);
        assert!(state.done());
        assert_eq!(state.next_step(), None);
    }

    #[test]
    fn duty_boundary_at_600_mhz() {
        // fhash(480) = 600 MHz exactly -> low duty; fhash(491) = 613.75 MHz -> high duty.
        assert_eq!(duty_word(480), chain::DUTY_LE_600_MHZ);
        assert_eq!(duty_word(491), chain::DUTY_GT_600_MHZ);
        assert_eq!(duty_word(80), chain::DUTY_LE_600_MHZ);
    }
}
