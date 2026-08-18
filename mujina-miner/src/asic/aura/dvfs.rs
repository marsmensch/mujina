//! Aura DVFS: continuous heartbeat and the post-job frequency ramp.
//!
//! The DVFS heartbeat (every ~2.1 s) writes the five payload words to
//! register 0x81 with lcmd `0x1f00`; `0x5074` is the corrected vendor
//! full-rate payload (not `0x5009`). The ramp raises PLL_FREQ N from 80 to
//! 491 in +20 steps (~25 MHz each, ~50 ms apart), rewriting DUTY_CYCLE and
//! HASHCONFIG on every step. The ramp only runs once pool work is live on
//! the chips; the thread gates it accordingly.
//!
//! The board-level PSU voltage climb is wired here: an injected
//! [`PwmSysfs`] channel (pwmchip1/pwm0 on Apollo III) gets the baseline
//! 5.0 V duty from [`psu_hold`] once the chain is live, then one duty step
//! per ramp step from [`psu_voltage_step`].

use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::io::AsyncWrite;

use super::chain::{self, PLL_RAMP_END, PLL_RAMP_START, PLL_RAMP_STEP, duty_word};
use super::protocol::{Register, pll_freq_word};
use crate::peripheral::pwm_sysfs::PwmSysfs;
use crate::tracing::prelude::*;

/// Low-level command word for the DVFS heartbeat.
pub const DVFS_HEARTBEAT_LCMD: u16 = 0x1f00;
/// The heartbeat payload: `0x5074` is the corrected vendor full-rate word.
pub const DVFS_HEARTBEAT_PAYLOAD: [u32; 5] = [0x02, 0x03, 0x01, 0x5074, 0x05];
/// Interval between DVFS heartbeats.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(2100);
/// Interval between ramp steps.
pub const RAMP_STEP_INTERVAL: Duration = Duration::from_millis(50);
/// Baseline PSU PWM duty (ns) held once the chain is live (~5.0 V on
/// Apollo III; vendor anchor).
pub const PSU_DUTY_BASELINE_NS: u64 = 20_000;
/// PSU PWM duty (ns) at full rate (~6.1 V on Apollo III; vendor anchor).
pub const PSU_DUTY_FULL_NS: u64 = 36_000;

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
    ramp_steps_up_to(PLL_RAMP_END)
}

/// PLL N dividers for a ramp that stops at `max_n` (clamped to the
/// [`PLL_RAMP_START`]..=[`PLL_RAMP_END`] range): `80, 100, ...` in
/// [`PLL_RAMP_STEP`] increments, ending exactly at `max_n` even when it is
/// not on the step grid (mirroring how the full ramp ends at 491).
pub fn ramp_steps_up_to(max_n: u32) -> Vec<u32> {
    let max_n = max_n.clamp(PLL_RAMP_START, PLL_RAMP_END);
    let mut steps = Vec::new();
    let mut n = PLL_RAMP_START;
    while n < max_n {
        steps.push(n);
        n = (n + PLL_RAMP_STEP).min(max_n);
    }
    steps.push(max_n);
    steps
}

/// PSU PWM duty (ns) for a PLL N divider: linear between the vendor
/// anchors `20000` (~5.0 V) at the ramp start and `36000` (~6.1 V) at full
/// rate. The anchors are vendor-verified; the linear curve between them is
/// an assumption to confirm on device (G6).
pub fn psu_duty_ns_for_pll_n(pll_n: u32) -> u64 {
    let n = u64::from(pll_n.clamp(PLL_RAMP_START, PLL_RAMP_END));
    let span_n = u64::from(PLL_RAMP_END - PLL_RAMP_START);
    PSU_DUTY_BASELINE_NS
        + (n - u64::from(PLL_RAMP_START)) * (PSU_DUTY_FULL_NS - PSU_DUTY_BASELINE_NS) / span_n
}

/// Enable the PSU PWM and hold the baseline voltage (~5.0 V).
///
/// Called once by the thread after chain bring-up (post-discovery, before
/// any job is dispatched) so the rail is at a known voltage before the
/// ramp starts climbing it. A `None` channel (no board-level PSU) is a
/// no-op.
pub async fn psu_hold(psu: Option<&PwmSysfs>) -> Result<()> {
    let Some(psu) = psu else {
        return Ok(());
    };
    // Write the duty while the channel may still be disabled, then enable:
    // the output glitches straight to the baseline duty.
    psu.set_duty_ns(PSU_DUTY_BASELINE_NS)
        .await
        .map_err(|e| anyhow!("failed to write PSU baseline duty: {e}"))?;
    psu.enable()
        .await
        .map_err(|e| anyhow!("failed to enable PSU PWM: {e}"))?;
    Ok(())
}

/// Write one ramp step for PLL N: PLL_FREQ, then DUTY_CYCLE + HASHCONFIG.
pub async fn write_ramp_step<W>(writer: &mut W, psu: Option<&PwmSysfs>, pll_n: u32) -> Result<()>
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
    psu_voltage_step(psu, pll_n).await
}

/// Board-level PSU voltage climb: write the PWM duty for the current PLL N.
///
/// `None` (no injected PSU channel) is a no-op, preserving the hook's
/// original placeholder behavior. The duty curve is
/// [`psu_duty_ns_for_pll_n`]; the PSU channel must be enabled once via
/// [`psu_hold`] before the ramp climbs it.
pub async fn psu_voltage_step(psu: Option<&PwmSysfs>, pll_n: u32) -> Result<()> {
    let Some(psu) = psu else {
        return Ok(());
    };
    psu.set_duty_ns(psu_duty_ns_for_pll_n(pll_n))
        .await
        .map_err(|e| anyhow!("failed to write PSU duty for PLL N {pll_n}: {e}"))
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
        Self::with_max_pll(PLL_RAMP_END)
    }

    /// Start at the low end of a ramp that stops at `max_pll` instead of
    /// full rate (used when the board's hashrate target is below full
    /// rate).
    pub fn with_max_pll(max_pll: u32) -> Self {
        Self {
            steps: ramp_steps_up_to(max_pll),
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

    #[test]
    fn ramp_steps_up_to_stops_at_target() {
        // Full rate: identical to the legacy ramp.
        assert_eq!(ramp_steps_up_to(PLL_RAMP_END), ramp_steps());
        // A target off the step grid ends exactly at the target.
        let steps = ramp_steps_up_to(300);
        assert_eq!(steps[0], 80);
        assert_eq!(steps.last().copied(), Some(300));
        assert!(steps.windows(2).all(|w| {
            let delta = w[1] - w[0];
            delta == PLL_RAMP_STEP || (w[1] == 300 && delta < PLL_RAMP_STEP)
        }));
        // Clamped to the ramp bounds.
        assert_eq!(ramp_steps_up_to(0), vec![80]);
        assert_eq!(ramp_steps_up_to(10_000), ramp_steps());
    }

    #[test]
    fn ramp_state_with_max_pll_completes_early() {
        let mut state = RampState::with_max_pll(300);
        assert_eq!(state.next_step(), Some(80));
        let mut count = 0;
        while state.next_step().is_some() {
            count += 1;
        }
        assert_eq!(count, ramp_steps_up_to(300).len() - 1);
        assert!(state.done());
    }

    #[test]
    fn psu_duty_anchors_and_monotonic() {
        assert_eq!(psu_duty_ns_for_pll_n(PLL_RAMP_START), PSU_DUTY_BASELINE_NS);
        assert_eq!(psu_duty_ns_for_pll_n(PLL_RAMP_END), PSU_DUTY_FULL_NS);
        // Clamped outside the ramp range.
        assert_eq!(psu_duty_ns_for_pll_n(0), PSU_DUTY_BASELINE_NS);
        assert_eq!(psu_duty_ns_for_pll_n(u32::MAX), PSU_DUTY_FULL_NS);
        // Monotonic across the ramp.
        let mut last = 0u64;
        for n in (PLL_RAMP_START..=PLL_RAMP_END).step_by(PLL_RAMP_STEP as usize) {
            let duty = psu_duty_ns_for_pll_n(n);
            assert!(duty >= last, "duty must not fall: {duty} < {last} at N={n}");
            last = duty;
        }
    }
}
