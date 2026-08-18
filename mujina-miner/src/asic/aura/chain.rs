//! Aura chain bring-up: discovery, version bounds, DVFS initial setup and
//! per-chip/chain register init.
//!
//! Discovery is multi-pass and probabilistic: a broadcast telemetry sweep
//! (`reg = 0x02`) is answered by only a few chips per pass, a different
//! subset each pass. Unique chip IDs are accumulated across passes; each
//! pass drains responses for [`ChainConfig::pass_drain`] and passes are
//! paced [`ChainConfig::pass_pace`] apart. After discovery each chip gets a
//! private nonce window via [`version_bounds`], then the chain-wide DVFS
//! initial setup and the PLL/duty/hashconfig init run (broadcast).
//!
//! # Framing at byte level
//!
//! The Aura [`FrameCodec`](super::protocol::FrameCodec) yields only 16-byte
//! [`Response`] frames and frame-syncs exclusively on the response magic, so
//! 92-byte hit frames (which carry the *command* magic) would be dropped by
//! it. The chain driver therefore frames at the byte level, recognising both
//! magics via [`drain_frames`]; hit-response magic (COMMAND vs RESPONSE) is
//! still to be verified on device (G6).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::{Buf, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::protocol::{
    COMMAND_MAGIC, Command, FRAME_LEN, HitFrame, LONG_FRAME_LEN, PLL_CONFIG_INIT, PREAMBLE_LEN,
    RESPONSE_MAGIC, Register, Response, pll_freq_word,
};
use crate::tracing::prelude::*;

/// Expected number of chips on the Apollo III chain (verified ground truth).
pub const EXPECTED_CHIP_COUNT: usize = 21;
/// Low-level command word used for telemetry/discovery reads.
pub const TELEMETRY_LCMD: u16 = 0x1200;
/// Low-level command word used for ordinary register writes.
pub const REG_LCMD: u16 = 0x1000;
/// Low-level command word for the version-bound write.
pub const VERSION_BOUND_LCMD: u16 = 0x1000;
/// Value written to VERSION_SHIFT (0x11).
pub const VERSION_SHIFT_VALUE: u32 = 13;
/// Nonce-window width per chip (`0x0c30`); 21 chips tile the lower 16 bits.
pub const VERSION_WINDOW: u32 = 0x0c30;
/// PLL N divider at the start of the frequency ramp.
pub const PLL_RAMP_START: u32 = 80;
/// PLL N divider at full rate (`fhash = 5 * 491 / 4 = 613.75 MHz`).
pub const PLL_RAMP_END: u32 = 491;
/// PLL N divider step between ramp stages (~25 MHz per step at 5N/4).
pub const PLL_RAMP_STEP: u32 = 20;
/// Duty-cycle register word while the hash clock is at/below 600 MHz.
pub const DUTY_LE_600_MHZ: u32 = 0x8080_0000;
/// Duty-cycle register word once the hash clock exceeds 600 MHz.
pub const DUTY_GT_600_MHZ: u32 = 0x8088_0000;
/// HASHCONFIG (0x14) value; rewritten after every duty-cycle change.
pub const HASHCONFIG_VALUE: u32 = 0x0200_0200;

/// Chain-level bring-up parameters (locked defaults; tests shrink them).
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Number of chips expected on the chain (stop discovery early once
    /// this many unique chips have ACKed).
    pub expected_chips: usize,
    /// How long to drain responses after each broadcast sweep.
    pub pass_drain: Duration,
    /// Pacing between sweep passes (next pass starts this long after the
    /// previous one).
    pub pass_pace: Duration,
    /// Maximum number of sweep passes before giving up.
    pub max_passes: usize,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            expected_chips: EXPECTED_CHIP_COUNT,
            pass_drain: Duration::from_millis(3500),
            pass_pace: Duration::from_millis(10800),
            max_passes: 24,
        }
    }
}

/// Parse every complete frame at the head of `buf`, returning the 16-byte
/// responses and 92-byte hit frames found.
///
/// Frame-syncs on both magics: response frames start with
/// [`RESPONSE_MAGIC`], hit frames with [`COMMAND_MAGIC`]. Junk (including
/// any wire preambles) is dropped a byte at a time; an incomplete frame at
/// the tail is left in `buf` for the next chunk. Frames that fail CRC are
/// dropped one byte at a time to resync.
pub fn drain_frames(buf: &mut BytesMut) -> (Vec<Response>, Vec<HitFrame>) {
    const RESPONSE_MAGIC_BYTES: [u8; 4] = RESPONSE_MAGIC.to_be_bytes();
    const COMMAND_MAGIC_BYTES: [u8; 4] = COMMAND_MAGIC.to_le_bytes();

    let mut responses = Vec::new();
    let mut hits = Vec::new();
    loop {
        if buf.len() < 4 {
            break;
        }
        if buf.starts_with(&RESPONSE_MAGIC_BYTES) {
            if buf.len() < FRAME_LEN {
                break;
            }
            let frame = buf.split_to(FRAME_LEN);
            match Response::parse(&frame) {
                Ok(response) => responses.push(response),
                Err(e) => {
                    trace!(error = %e, "dropping corrupt Aura response frame");
                    buf.advance(1);
                }
            }
        } else if buf.starts_with(&COMMAND_MAGIC_BYTES) {
            if buf.len() < LONG_FRAME_LEN {
                break;
            }
            let frame = buf.split_to(LONG_FRAME_LEN);
            match HitFrame::parse(&frame) {
                Ok(hit) => hits.push(hit),
                Err(e) => {
                    trace!(error = %e, "dropping corrupt Aura hit frame");
                    buf.advance(1);
                }
            }
        } else {
            // Junk: drop one byte. The tail keeps the last 3 bytes so a
            // magic straddling the buffer boundary is preserved.
            buf.advance(1);
        }
    }
    (responses, hits)
}

/// Write a register command to the chain (20-byte preamble + 16-byte frame).
pub async fn write_reg<W>(
    writer: &mut W,
    broadcast: bool,
    chip_address: u8,
    register: Register,
    lcmd: u16,
    data: u32,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let command = Command::WriteRegister {
        broadcast,
        chip_address,
        register,
        lcmd,
        data,
    };
    let mut buf = BytesMut::with_capacity(PREAMBLE_LEN + FRAME_LEN);
    command.encode_with_preamble(&mut buf);
    writer
        .write_all(&buf)
        .await
        .context("failed to write Aura register command")?;
    Ok(())
}

/// Issue a register read command (response arrives asynchronously).
pub async fn read_reg<W>(
    writer: &mut W,
    chip_address: u8,
    register: Register,
    lcmd: u16,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let command = Command::ReadRegister {
        broadcast: false,
        chip_address,
        register,
        lcmd,
        data: 0,
    };
    let mut buf = BytesMut::with_capacity(PREAMBLE_LEN + FRAME_LEN);
    command.encode_with_preamble(&mut buf);
    writer
        .write_all(&buf)
        .await
        .context("failed to write Aura register read")?;
    Ok(())
}

/// Poll `chip` for one queued hit (fire-and-forget).
///
/// A queued hit makes the chip emit a 92-byte hit frame; an empty hit FIFO
/// yields no response at all, which is normal.
pub async fn hit_poll<W>(writer: &mut W, chip_address: u8) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let command = Command::HitPoll { chip_address };
    let mut buf = BytesMut::with_capacity(PREAMBLE_LEN + FRAME_LEN);
    command.encode_with_preamble(&mut buf);
    writer
        .write_all(&buf)
        .await
        .context("failed to write Aura hit poll")?;
    Ok(())
}

/// Discover the chain by sweeping for ACKs across multiple passes.
///
/// Returns the unique ACKing chip IDs, sorted ascending. A partial result
/// (fewer than `expected_chips`) is not an error: discovery is probabilistic
/// and the caller decides how to treat a short chain.
pub async fn discover_chips<R, W>(
    reader: &mut R,
    writer: &mut W,
    config: &ChainConfig,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut found: Vec<u8> = Vec::new();
    let mut rx = BytesMut::new();
    let mut buf = [0u8; 512];
    let mut pass = 0usize;

    while found.len() < config.expected_chips && pass < config.max_passes {
        pass += 1;
        debug!(pass, "Aura discovery sweep");
        // Broadcast telemetry sweep; a subset of chips ACK per pass.
        write_reg(writer, true, 0x00, Register::Telemetry, TELEMETRY_LCMD, 0).await?;

        // Drain responses for the pass window.
        let deadline = Instant::now() + config.pass_drain;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, reader.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    // EOF: bus hung up or went quiet; end the pass early.
                    trace!("Aura discovery read EOF");
                    break;
                }
                Ok(Ok(n)) => {
                    rx.extend_from_slice(&buf[..n]);
                    let (responses, _hits) = drain_frames(&mut rx);
                    for response in responses {
                        if !found.contains(&response.chip) {
                            info!(
                                chip = format!("0x{:02x}", response.chip),
                                "Aura chip ACKed discovery"
                            );
                            found.push(response.chip);
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "Aura discovery read error");
                    break;
                }
                Err(_elapsed) => break,
            }
        }

        if found.len() < config.expected_chips && pass < config.max_passes {
            // Pace the next pass (start-to-start spacing is `pass_pace`).
            tokio::time::sleep(config.pass_pace.saturating_sub(config.pass_drain)).await;
        }
    }

    found.sort_unstable();
    Ok(found)
}

/// Packed version-bound word for chip `index`.
///
/// The chip's private nonce window is `[lower, upper]` with
/// `lower = index * 0x0c30`, `upper = lower + 0x0c2f`, packed as
/// `lower | (upper << 16)`. Index is the chip's position in the sorted
/// discovered list, so 21 chips tile `0x0000..=0xffef`.
pub fn version_bounds(index: usize) -> u32 {
    let lower = index as u32 * VERSION_WINDOW;
    let upper = lower + VERSION_WINDOW - 1;
    lower | (upper << 16)
}

/// Write per-chip version bounds and version shift for every discovered chip.
pub async fn configure_version_bounds<W>(writer: &mut W, chips: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for (index, &chip) in chips.iter().enumerate() {
        let packed = version_bounds(index);
        write_reg(
            writer,
            false,
            chip,
            Register::VersionBound,
            VERSION_BOUND_LCMD,
            packed,
        )
        .await?;
        write_reg(
            writer,
            false,
            chip,
            Register::VersionShift,
            VERSION_BOUND_LCMD,
            VERSION_SHIFT_VALUE,
        )
        .await?;
    }
    Ok(())
}

/// The one-time DVFS InitialSetup sequence: 18 writes to register 0x81.
///
/// Exact order and values are locked ground truth. For these writes the
/// register address is the low byte of the low-level command word (all `0x00`
/// here); the frame register byte is always 0x81.
pub const DVFS_INIT_WRITES: [(u16, u32); 18] = [
    (0x1800, 11_245_000),
    (0x1900, 0x0000_000a),
    (0x2500, 0x00f0_ff00),
    (0x1400, 0x0200_0200),
    (0x6800, 0x8080_0000),
    (0x1400, 0x0200_0200),
    (0x1c00, 0x0100_0000),
    (0x1d00, 0x0d00_0000),
    (0x2500, 0x00f0_ff00),
    (0x1400, 0x0200_0200),
    (0x6800, 0x8080_0000),
    (0x1400, 0x0200_0200),
    (0x1900, 0x0000_0005),
    (0x2300, 0x4408_0000),
    (0x6800, 0x8080_0000),
    (0x1900, 0x0000_000a),
    (0x2400, 0x0000_0000),
    (0x2300, 0x4408_0000),
];

/// Send the one-time DVFS InitialSetup sequence (broadcast).
pub async fn dvfs_initial_setup<W>(writer: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for (lcmd, data) in DVFS_INIT_WRITES {
        write_reg(writer, true, 0x00, Register::Dvfs, lcmd, data).await?;
    }
    Ok(())
}

/// Duty-cycle register word for a PLL N divider (0x8080 while the hash clock
/// is at/below 600 MHz, 0x8088 above).
pub fn duty_word(pll_n: u32) -> u32 {
    let mhz = 5.0 * f64::from(pll_n) / 4.0;
    if mhz <= 600.0 {
        DUTY_LE_600_MHZ
    } else {
        DUTY_GT_600_MHZ
    }
}

/// Chain-wide init: PLL_CONFIG -> PLL_FREQ -> DUTY_CYCLE -> HASHCONFIG.
///
/// `pll_n` is the initial PLL divider (the ramp start, 80). HASHCONFIG is
/// rewritten after the duty-cycle change, as required.
pub async fn init_chain<W>(writer: &mut W, pll_n: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_reg(
        writer,
        true,
        0x00,
        Register::PllConfig,
        REG_LCMD,
        PLL_CONFIG_INIT,
    )
    .await?;
    write_reg(
        writer,
        true,
        0x00,
        Register::PllFreq,
        REG_LCMD,
        pll_freq_word(pll_n),
    )
    .await?;
    write_reg(
        writer,
        true,
        0x00,
        Register::DutyCycle,
        REG_LCMD,
        duty_word(pll_n),
    )
    .await?;
    write_reg(
        writer,
        true,
        0x00,
        Register::HashConfig,
        REG_LCMD,
        HASHCONFIG_VALUE,
    )
    .await?;
    Ok(())
}

/// Full chain bring-up: discovery, version bounds, DVFS initial setup, init.
///
/// Runs at the low end of the ramp (`PLL_RAMP_START`); the frequency ramp
/// itself is driven later by the thread once pool work is live.
pub async fn bring_up_chain<R, W>(
    reader: &mut R,
    writer: &mut W,
    config: &ChainConfig,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let chips = discover_chips(reader, writer, config)
        .await
        .context("Aura chip discovery failed")?;
    if chips.is_empty() {
        anyhow::bail!("no Aura chips discovered");
    }
    if chips.len() < config.expected_chips {
        warn!(
            count = chips.len(),
            expected = config.expected_chips,
            "fewer Aura chips discovered than expected"
        );
    }
    debug!(count = chips.len(), "Aura chip discovery complete");

    configure_version_bounds(writer, &chips).await?;
    dvfs_initial_setup(writer).await?;
    init_chain(writer, PLL_RAMP_START).await?;
    Ok(chips)
}
