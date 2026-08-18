//! Shared fake-transport helpers for Aura chain/thread tests.
//!
//! The fake transport is a `tokio::io::duplex` pair: the code under test
//! gets one half (split into read/write handles), and [`TestChip`] drives
//! the other half, reading commands/jobs the driver writes and scripting
//! response and hit frames back.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::crc::aura_crc32;
use super::protocol::{
    COMMAND_MAGIC, FRAME_LEN, JOB_LCMD_BASE, LONG_FRAME_LEN, PREAMBLE_LEN, RESPONSE_MAGIC,
};

/// Build a valid 16-byte response frame.
pub fn response_frame(chip: u8, reg: u8, lcmd: u16, data: u32) -> [u8; FRAME_LEN] {
    let mut frame = [0u8; FRAME_LEN];
    frame[0..4].copy_from_slice(&RESPONSE_MAGIC.to_be_bytes());
    frame[4] = chip;
    frame[5] = reg;
    frame[6..8].copy_from_slice(&lcmd.to_le_bytes());
    frame[8..12].copy_from_slice(&data.to_le_bytes());
    let crc = aura_crc32(&frame[0..12]);
    frame[12..16].copy_from_slice(&crc.to_le_bytes());
    frame
}

/// Build a valid 92-byte hit frame from a full 80-byte winning header.
///
/// The nonce must already be in `header[76..80]` (absolute bytes `[84..88]`).
pub fn hit_frame(chip: u8, header: [u8; 80]) -> [u8; LONG_FRAME_LEN] {
    let mut frame = [0u8; LONG_FRAME_LEN];
    frame[0..4].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
    frame[4] = chip;
    frame[5] = 0xc4; // reg | 0x40 (hit response)
    frame[6] = 0x1d; // nbits
    frame[7] = 0x00;
    frame[8..88].copy_from_slice(&header);
    let crc = aura_crc32(&frame[0..88]);
    frame[88..92].copy_from_slice(&crc.to_le_bytes());
    frame
}

/// A decoded wire frame (16-byte command or 92-byte job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireFrame {
    /// A preamble + 16-byte command frame.
    Command {
        chip: u8,
        reg: u8,
        lcmd: u16,
        data: u32,
    },
    /// A 92-byte job frame (no preamble).
    Job {
        chip: u8,
        slot: u8,
        job_id: u32,
        ntime: u32,
        nbits: u32,
        nonce_start: u32,
    },
}

impl WireFrame {
    /// The frame's register byte (`None` for job frames, which carry 0x84).
    pub fn reg(&self) -> Option<u8> {
        match self {
            WireFrame::Command { reg, .. } => Some(*reg),
            WireFrame::Job { .. } => None,
        }
    }
}

/// Walk a raw byte stream, splitting it into commands (36 bytes: 20-byte
/// preamble + 16-byte frame) and job frames (92 bytes, no preamble).
pub fn parse_wire_stream(bytes: &[u8]) -> Vec<WireFrame> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == [0u8; 4] {
            if i + PREAMBLE_LEN + FRAME_LEN > bytes.len() {
                break;
            }
            let f = &bytes[i + PREAMBLE_LEN..i + PREAMBLE_LEN + FRAME_LEN];
            out.push(WireFrame::Command {
                chip: f[4],
                reg: f[5],
                lcmd: u16::from_le_bytes([f[6], f[7]]),
                data: u32::from_le_bytes([f[8], f[9], f[10], f[11]]),
            });
            i += PREAMBLE_LEN + FRAME_LEN;
        } else if bytes[i..i + 4] == COMMAND_MAGIC.to_le_bytes() {
            if i + LONG_FRAME_LEN > bytes.len() {
                break;
            }
            let f = &bytes[i..i + LONG_FRAME_LEN];
            let lcmd = u16::from_le_bytes([f[6], f[7]]);
            out.push(WireFrame::Job {
                chip: f[4],
                slot: ((lcmd - JOB_LCMD_BASE) / 0x400) as u8,
                job_id: u32::from_le_bytes([f[8], f[9], f[10], f[11]]),
                ntime: u32::from_le_bytes([f[76], f[77], f[78], f[79]]),
                nbits: u32::from_le_bytes([f[80], f[81], f[82], f[83]]),
                nonce_start: u32::from_le_bytes([f[84], f[85], f[86], f[87]]),
            });
            i += LONG_FRAME_LEN;
        } else {
            i += 1;
        }
    }
    out
}

/// Test-side handle to the fake chip bus.
pub struct TestChip {
    io: DuplexStream,
    pending: Vec<u8>,
}

impl TestChip {
    /// Wrap the test side of a duplex pair.
    pub fn new(io: DuplexStream) -> Self {
        Self {
            io,
            pending: Vec::new(),
        }
    }

    /// Read exactly `n` bytes written by the driver (panics on timeout).
    pub async fn read_bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut buf = [0u8; 2048];
        let deadline = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(deadline);
        while out.len() < n {
            if !self.pending.is_empty() {
                let take = (n - out.len()).min(self.pending.len());
                out.extend(self.pending.drain(..take));
                continue;
            }
            tokio::select! {
                _ = &mut deadline => panic!(
                    "timed out waiting for {n} bytes (got {}): {:02x?}",
                    out.len(),
                    out
                ),
                res = self.io.read(&mut buf) => {
                    let k = res.expect("read from fake chip");
                    if k == 0 {
                        panic!("fake chip EOF while waiting for {n} bytes");
                    }
                    self.pending.extend_from_slice(&buf[..k]);
                }
            }
        }
        out
    }

    /// Read one 16-byte command (preamble + frame) and decode it.
    pub async fn expect_command(&mut self) -> WireFrame {
        let bytes = self.read_bytes(PREAMBLE_LEN + FRAME_LEN).await;
        match parse_wire_stream(&bytes).into_iter().next() {
            Some(frame) => frame,
            None => panic!("no command frame in {:02x?}", bytes),
        }
    }

    /// Write a raw frame (response or hit) to the bus.
    pub async fn send(&mut self, frame: &[u8]) {
        self.io.write_all(frame).await.expect("write to bus");
    }

    /// True if no bytes arrive within `dur` (bus is silent).
    pub async fn expect_silence(&mut self, dur: Duration) -> bool {
        let mut buf = [0u8; 64];
        match tokio::time::timeout(dur, self.io.read(&mut buf)).await {
            Err(_) => true,
            Ok(Ok(0)) => true,
            Ok(Ok(_)) => false,
            Ok(Err(_)) => true,
        }
    }
}
