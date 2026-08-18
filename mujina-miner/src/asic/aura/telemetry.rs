//! Aura telemetry decoders and per-chip counter tracking.
//!
//! Hashrate comes from the hit-count counter (0x61): each count represents
//! `2^32` hashes, so `hashrate = delta(0x61) * 2^32 / dt / 1e9` GH/s. The
//! preliminary counter (0x60) leads the hit counter; their ratio is the
//! engine health ratio. Clock is derived from the reference (0x04) and hash
//! (0x06) counters: `Fhash = 2 * delta(0x06) / delta(0x04) * 25` MHz. The
//! per-chip voltage ADC (0x20) converts as `V = raw * 0.0001011035 - 0.276029`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::protocol::Register;
use super::protocol::Response;

/// Registers polled per chip for telemetry, in read order.
pub const TELEMETRY_REGS: [Register; 5] = [
    Register::HitCount,
    Register::HitCountPrelim,
    Register::ClkRef,
    Register::ClkHash,
    Register::VoltageAdc,
];

/// Hashrate in GH/s from a hit-counter delta over `dt`.
///
/// Each counter increment represents `2^32` hashes.
pub fn hashrate_ghs(delta_hit_count: u64, dt: Duration) -> f64 {
    if dt.is_zero() {
        return 0.0;
    }
    delta_hit_count as f64 * (1u64 << 32) as f64 / dt.as_secs_f64() / 1e9
}

/// Hash clock in MHz from the clk_hash / clk_ref counter deltas.
pub fn clock_mhz(delta_clk_hash: u64, delta_clk_ref: u64) -> f64 {
    if delta_clk_ref == 0 {
        return 0.0;
    }
    2.0 * delta_clk_hash as f64 / delta_clk_ref as f64 * 25.0
}

/// Core voltage in volts from the raw ADC word.
pub fn voltage(raw: u32) -> f64 {
    f64::from(raw) * 0.000_101_103_5 - 0.276_029
}

/// Engine health ratio: hits / preliminary hits. `1.0` is a healthy engine.
pub fn engine_health_ratio(delta_hit: u64, delta_prelim: u64) -> f64 {
    if delta_prelim == 0 {
        return 0.0;
    }
    delta_hit as f64 / delta_prelim as f64
}

/// Decoded telemetry for one chip over one poll interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipTelemetry {
    /// Chip address.
    pub chip: u8,
    /// Hashrate in GH/s.
    pub hashrate_ghs: f64,
    /// Hash clock in MHz.
    pub clock_mhz: f64,
    /// Core voltage in volts.
    pub voltage: f64,
    /// Engine health ratio (hit/preliminary).
    pub engine_health: f64,
}

/// Latest raw counters observed for one chip.
#[derive(Debug, Clone, Copy, Default)]
struct ChipCounters {
    hit_count: u32,
    hit_count_prelim: u32,
    clk_ref: u32,
    clk_hash: u32,
    voltage_raw: u32,
    seen: u8,
}

const SEEN_HIT: u8 = 1 << 0;
const SEEN_PRELIM: u8 = 1 << 1;
const SEEN_REF: u8 = 1 << 2;
const SEEN_HASH: u8 = 1 << 3;
const SEEN_VOLTAGE: u8 = 1 << 4;
const SEEN_ALL: u8 = SEEN_HIT | SEEN_PRELIM | SEEN_REF | SEEN_HASH | SEEN_VOLTAGE;

/// Accumulates per-chip telemetry responses and produces deltas on poll.
///
/// Counters wrap at 32 bits; deltas are computed mod 2^32, which is exact
/// for monotonically increasing counters.
#[derive(Debug, Default)]
pub struct TelemetryTracker {
    current: HashMap<u8, ChipCounters>,
    previous: HashMap<u8, (ChipCounters, Instant)>,
}

impl TelemetryTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one telemetry response.
    ///
    /// Returns `true` if the response matched a telemetry register and was
    /// stored, `false` otherwise.
    pub fn record(&mut self, response: Response) -> bool {
        let counters = self.current.entry(response.chip).or_default();
        match response.reg {
            r if r == u8::from(Register::HitCount) => {
                counters.hit_count = response.data;
                counters.seen |= SEEN_HIT;
            }
            r if r == u8::from(Register::HitCountPrelim) => {
                counters.hit_count_prelim = response.data;
                counters.seen |= SEEN_PRELIM;
            }
            r if r == u8::from(Register::ClkRef) => {
                counters.clk_ref = response.data;
                counters.seen |= SEEN_REF;
            }
            r if r == u8::from(Register::ClkHash) => {
                counters.clk_hash = response.data;
                counters.seen |= SEEN_HASH;
            }
            r if r == u8::from(Register::VoltageAdc) => {
                counters.voltage_raw = response.data;
                counters.seen |= SEEN_VOLTAGE;
            }
            _ => return false,
        }
        true
    }

    /// Compute per-chip telemetry from the last two full samples.
    ///
    /// Chips without a full current sample (or without a previous sample)
    /// are skipped; their partial current sample carries into the next poll.
    pub fn poll(&mut self, now: Instant) -> Vec<ChipTelemetry> {
        let mut out = Vec::new();
        let current = std::mem::take(&mut self.current);
        for (chip, counters) in current {
            if counters.seen != SEEN_ALL {
                // Incomplete sample: keep accumulating.
                self.current.insert(chip, counters);
                continue;
            }
            if let Some((prev, prev_instant)) = self.previous.get(&chip) {
                let dt = now.saturating_duration_since(*prev_instant);
                let d_hit = u64::from(counters.hit_count.wrapping_sub(prev.hit_count));
                let d_prelim = u64::from(
                    counters
                        .hit_count_prelim
                        .wrapping_sub(prev.hit_count_prelim),
                );
                let d_ref = u64::from(counters.clk_ref.wrapping_sub(prev.clk_ref));
                let d_hash = u64::from(counters.clk_hash.wrapping_sub(prev.clk_hash));
                out.push(ChipTelemetry {
                    chip,
                    hashrate_ghs: hashrate_ghs(d_hit, dt),
                    clock_mhz: clock_mhz(d_hash, d_ref),
                    voltage: voltage(counters.voltage_raw),
                    engine_health: engine_health_ratio(d_hit, d_prelim),
                });
            }
            self.previous.insert(chip, (counters, now));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashrate_formula_matches_locked_fact() {
        // 1e6 counter deltas in 1s = 1e6 * 2^32 / 1e9 GH/s = 4.294967296e6.
        let gh = hashrate_ghs(1_000_000, Duration::from_secs(1));
        assert!((gh - 4_294_967.296).abs() < 1e-3, "got {gh}");
        // Full counter wrap in 1s = 2^64 / 1e9 GH/s.
        let wrap = hashrate_ghs(1u64 << 32, Duration::from_secs(1));
        assert!((wrap - 18_446_744_073.709_55).abs() < 1e-3, "got {wrap}");
        // Zero interval yields zero.
        assert_eq!(hashrate_ghs(100, Duration::ZERO), 0.0);
    }

    #[test]
    fn clock_formula_matches_locked_fact() {
        // delta(0x06)=250, delta(0x04)=25 -> 2*250/25*25 = 500 MHz.
        assert!((clock_mhz(250, 25) - 500.0).abs() < 1e-9);
        // Zero reference delta is degenerate, not an error.
        assert_eq!(clock_mhz(100, 0), 0.0);
    }

    #[test]
    fn voltage_formula_matches_locked_fact() {
        // raw=15000 -> 15000*0.0001011035 - 0.276029 = 1.2405235 V.
        assert!((voltage(15_000) - 1.240_523_5).abs() < 1e-6);
        assert_eq!(voltage(0), -0.276_029);
    }

    #[test]
    fn engine_health_formula() {
        assert!((engine_health_ratio(4, 5) - 0.8).abs() < 1e-9);
        assert_eq!(engine_health_ratio(1, 0), 0.0);
    }

    #[test]
    fn tracker_computes_deltas_across_polls() {
        let mut tracker = TelemetryTracker::new();
        let t0 = Instant::now();

        // First poll: only a reference sample exists, no previous sample.
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::HitCount),
            lcmd: 0x1200,
            data: 0,
        });
        assert!(tracker.poll(t0).is_empty());

        // Complete first sample; still no deltas (no previous).
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::HitCountPrelim),
            lcmd: 0x1200,
            data: 0,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::ClkRef),
            lcmd: 0x1200,
            data: 0,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::ClkHash),
            lcmd: 0x1200,
            data: 0,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::VoltageAdc),
            lcmd: 0x1200,
            data: 15_000,
        });
        assert!(tracker.poll(t0).is_empty());

        // Second complete sample 1s later: deltas produce telemetry.
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::HitCount),
            lcmd: 0x1200,
            data: 1_000_000,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::HitCountPrelim),
            lcmd: 0x1200,
            data: 1_250_000,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::ClkRef),
            lcmd: 0x1200,
            data: 25,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::ClkHash),
            lcmd: 0x1200,
            data: 250,
        });
        tracker.record(Response {
            chip: 3,
            reg: u8::from(Register::VoltageAdc),
            lcmd: 0x1200,
            data: 15_000,
        });
        let samples = tracker.poll(t0 + Duration::from_secs(1));
        assert_eq!(samples.len(), 1);
        let s = samples[0];
        assert_eq!(s.chip, 3);
        assert!((s.hashrate_ghs - 4_294_967.296).abs() < 1e-3);
        assert!((s.clock_mhz - 500.0).abs() < 1e-9);
        assert!((s.voltage - 1.240_523_5).abs() < 1e-6);
        assert!((s.engine_health - 0.8).abs() < 1e-9);
    }
}
