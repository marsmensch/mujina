//! Aura ASIC wire protocol implementation.
//!
//! Command/response frames are 16-byte records:
//! `magic(4) | chip(1) | reg(1) | lcmd(2 LE) | data(4) | crc32(4 LE over bytes[0..12])`.
//!
//! Command frames are preceded on the wire by a 20-byte all-zero preamble
//! (written as a separate write on hardware); [`FrameCodec`] emits the
//! preamble when encoding and skips it while frame-syncing on decode.
//! Job frames are 92 bytes and are written in a single write with no
//! inline preamble.

use bytes::{Buf, BufMut, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

use super::crc::aura_crc32;
use super::error::ProtocolError;

/// Magic for command frames (TX), wire bytes `78 56 34 12` (little-endian).
pub const COMMAND_MAGIC: u32 = 0x1234_5678;
/// Magic for response frames (RX).
///
/// Unlike the command magic, the response magic appears on the wire as the
/// big-endian bytes `54 76 c0 da`, so it is written/read with `to_be_bytes`
/// / `from_be_bytes`.
pub const RESPONSE_MAGIC: u32 = 0x5476_C0DA;
/// Register byte used on job frames.
pub const JOB_REGISTER: u8 = 0x84;
/// Base low-level command word for job frames (`slot * 0x400 + 0x2a`).
pub const JOB_LCMD_BASE: u16 = 0x002a;
/// Length of a command/response frame in bytes.
pub const FRAME_LEN: usize = 16;
/// Length of the all-zero preamble preceding command frames.
pub const PREAMBLE_LEN: usize = 20;
/// Length of a job/hit frame in bytes.
pub const LONG_FRAME_LEN: usize = 92;
/// Initial PLL_CONFIG value.
pub const PLL_CONFIG_INIT: u32 = 0x0050_2411;

/// Aura register map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Register {
    ChipId = 0x00,
    WorkData = 0x01,
    Telemetry = 0x02,
    ClkRef = 0x04,
    ClkHash = 0x06,
    VersionBound = 0x10,
    VersionShift = 0x11,
    HitConfig = 0x13,
    HashConfig = 0x14,
    PllConfig = 0x18,
    PllFreq = 0x19,
    VoltageAdc = 0x20,
    SmallNonce = 0x25,
    HitCountPrelim = 0x60,
    HitCount = 0x61,
    DutyCycle = 0x68,
    Dvfs = 0x81,
    Job = 0x84,
}

impl From<Register> for u8 {
    fn from(reg: Register) -> Self {
        reg as u8
    }
}

impl TryFrom<u8> for Register {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Register::ChipId),
            0x01 => Ok(Register::WorkData),
            0x02 => Ok(Register::Telemetry),
            0x04 => Ok(Register::ClkRef),
            0x06 => Ok(Register::ClkHash),
            0x10 => Ok(Register::VersionBound),
            0x11 => Ok(Register::VersionShift),
            0x13 => Ok(Register::HitConfig),
            0x14 => Ok(Register::HashConfig),
            0x18 => Ok(Register::PllConfig),
            0x19 => Ok(Register::PllFreq),
            0x20 => Ok(Register::VoltageAdc),
            0x25 => Ok(Register::SmallNonce),
            0x60 => Ok(Register::HitCountPrelim),
            0x61 => Ok(Register::HitCount),
            0x68 => Ok(Register::DutyCycle),
            0x81 => Ok(Register::Dvfs),
            0x84 => Ok(Register::Job),
            other => Err(ProtocolError::InvalidRegister(other)),
        }
    }
}

/// A register read/write command addressed at a chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Read `register` from the chip using low-level command word `lcmd`.
    ReadRegister {
        /// Broadcast to all chips (chip byte forced to `0x80`).
        broadcast: bool,
        /// Chip address on the serial bus.
        chip_address: u8,
        /// Target register.
        register: Register,
        /// Low-level command word.
        lcmd: u16,
        /// Command data payload.
        data: u32,
    },
    /// Write `data` to `register` using low-level command word `lcmd`.
    WriteRegister {
        /// Broadcast to all chips (chip byte forced to `0x80`).
        broadcast: bool,
        /// Chip address on the serial bus.
        chip_address: u8,
        /// Target register.
        register: Register,
        /// Low-level command word.
        lcmd: u16,
        /// Command data payload.
        data: u32,
    },
}

impl Command {
    fn fields(&self) -> (bool, u8, Register, u16, u32) {
        match *self {
            Command::ReadRegister {
                broadcast,
                chip_address,
                register,
                lcmd,
                data,
            } => (broadcast, chip_address, register, lcmd, data),
            Command::WriteRegister {
                broadcast,
                chip_address,
                register,
                lcmd,
                data,
            } => (broadcast, chip_address, register, lcmd, data),
        }
    }

    /// Encode into a 16-byte frame.
    ///
    /// Layout: `magic(4) | chip(1) | reg(1) | lcmd(2 LE) | data(4 LE) |
    /// crc32(4 LE over bytes[0..12])`.
    pub fn encode(&self) -> [u8; FRAME_LEN] {
        let (broadcast, chip_address, register, lcmd, data) = self.fields();
        let mut frame = [0u8; FRAME_LEN];
        frame[0..4].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
        frame[4] = if broadcast { 0x80 } else { chip_address };
        frame[5] = u8::from(register);
        frame[6..8].copy_from_slice(&lcmd.to_le_bytes());
        frame[8..12].copy_from_slice(&data.to_le_bytes());
        let crc = aura_crc32(&frame[0..12]);
        frame[12..16].copy_from_slice(&crc.to_le_bytes());
        frame
    }

    /// Encode the 20-byte zero preamble followed by the 16-byte frame
    /// into `dst`, ready for a single write.
    pub fn encode_with_preamble(&self, dst: &mut BytesMut) {
        dst.put_bytes(0, PREAMBLE_LEN);
        dst.put_slice(&self.encode());
    }
}

/// A parsed 16-byte response frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    /// Chip address that answered.
    pub chip: u8,
    /// Register the response is for.
    pub reg: u8,
    /// Low-level command word echoed by the chip.
    pub lcmd: u16,
    /// Response data payload.
    pub data: u32,
}

impl Response {
    /// Parse and validate a 16-byte response frame.
    ///
    /// Validates the length, response magic and CRC (over bytes
    /// `[0..12]`) before extracting the fields.
    pub fn parse(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() != FRAME_LEN {
            return Err(ProtocolError::InvalidFrameLength {
                expected: FRAME_LEN,
                found: frame.len(),
            });
        }
        let magic = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        if magic != RESPONSE_MAGIC {
            return Err(ProtocolError::BadMagic {
                expected: RESPONSE_MAGIC,
                found: magic,
            });
        }
        let stored = u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]);
        let computed = aura_crc32(&frame[0..12]);
        if stored != computed {
            return Err(ProtocolError::BadCrc {
                expected: computed,
                found: stored,
            });
        }
        Ok(Self {
            chip: frame[4],
            reg: frame[5],
            lcmd: u16::from_le_bytes([frame[6], frame[7]]),
            data: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
        })
    }
}

/// Aura job frame (92 bytes, single write, no inline preamble).
pub struct JobFrame;

impl JobFrame {
    /// Encode a job frame.
    ///
    /// Layout: `magic(4) | chip(1) | 0x84(1) | lcmd(2 LE) | job_id(4 LE) |
    /// prevhash(32) | merkle(32) | ntime(4 LE) | nbits(4 LE) |
    /// nonce_start(4 LE) | crc32(4 LE over bytes[0..88])`, with
    /// `lcmd = slot * 0x400 + 0x2a`.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        chip: u8,
        slot: u8,
        job_id: u32,
        prevhash: &[u8; 32],
        merkle: &[u8; 32],
        ntime: u32,
        nbits: u32,
        nonce_start: u32,
    ) -> [u8; LONG_FRAME_LEN] {
        let mut frame = [0u8; LONG_FRAME_LEN];
        frame[0..4].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
        frame[4] = chip;
        frame[5] = JOB_REGISTER;
        let lcmd = u16::from(slot) * 0x400 + JOB_LCMD_BASE;
        frame[6..8].copy_from_slice(&lcmd.to_le_bytes());
        frame[8..12].copy_from_slice(&job_id.to_le_bytes());
        frame[12..44].copy_from_slice(prevhash);
        frame[44..76].copy_from_slice(merkle);
        frame[76..80].copy_from_slice(&ntime.to_le_bytes());
        frame[80..84].copy_from_slice(&nbits.to_le_bytes());
        frame[84..88].copy_from_slice(&nonce_start.to_le_bytes());
        let crc = aura_crc32(&frame[0..88]);
        frame[88..92].copy_from_slice(&crc.to_le_bytes());
        frame
    }
}

/// A parsed 92-byte hit (winning nonce) response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitFrame {
    /// Chip that reported the hit.
    pub chip: u8,
    /// Winning nonce (absolute bytes `[84..88]`, the last 4 bytes of the
    /// header).
    pub nonce: u32,
    /// The 80-byte winning block header (frame bytes `[8..88]`).
    pub header: [u8; 80],
}

impl HitFrame {
    /// Parse and validate a 92-byte hit frame.
    ///
    /// Layout: `magic(4) | chip(1) | reg(1) | nbits(1) | id_hi_seq(1) |
    /// header(80) | crc32(4 LE over bytes[0..88])`. The nonce is the last
    /// 4 bytes of the header, at absolute bytes `[84..88]`. An empty hit
    /// FIFO yields no response at all, so this only sees real hits.
    pub fn parse(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() != LONG_FRAME_LEN {
            return Err(ProtocolError::InvalidFrameLength {
                expected: LONG_FRAME_LEN,
                found: frame.len(),
            });
        }
        let magic = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        if magic != COMMAND_MAGIC {
            return Err(ProtocolError::BadMagic {
                expected: COMMAND_MAGIC,
                found: magic,
            });
        }
        let stored = u32::from_le_bytes([frame[88], frame[89], frame[90], frame[91]]);
        let computed = aura_crc32(&frame[0..88]);
        if stored != computed {
            return Err(ProtocolError::BadCrc {
                expected: computed,
                found: stored,
            });
        }
        let mut header = [0u8; 80];
        header.copy_from_slice(&frame[8..88]);
        let nonce = u32::from_le_bytes([frame[84], frame[85], frame[86], frame[87]]);
        Ok(Self {
            chip: frame[4],
            nonce,
            header,
        })
    }
}

/// Encode a PLL N divider into the PLL_FREQ wire word (`N << 20`).
pub fn pll_freq_word(n: u32) -> u32 {
    n << 20
}

/// Hash frequency in MHz for a given PLL N (`Fhash = 5 * N / 4`).
pub fn fhash_mhz(n: f64) -> f64 {
    5.0 * n / 4.0
}

/// Tokio-util codec for Aura command/response frames.
///
/// Encoding a [`Command`] emits the 20-byte all-zero preamble followed by
/// the 16-byte command frame. Decoding skips preamble bytes and
/// frame-syncs on the response magic, yielding [`Response`] frames.
#[derive(Default)]
pub struct FrameCodec;

impl Encoder<Command> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, command: Command, dst: &mut BytesMut) -> Result<(), Self::Error> {
        command.encode_with_preamble(dst);
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = Response;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < FRAME_LEN {
            return Ok(None);
        }

        // Frame-sync on the response magic, skipping preamble/junk bytes.
        let magic = RESPONSE_MAGIC.to_be_bytes();
        match src.windows(4).position(|w| w == magic.as_slice()) {
            Some(pos) => {
                if src.len() < pos + FRAME_LEN {
                    // Magic found but the full frame is not buffered yet.
                    return Ok(None);
                }
                src.advance(pos);
                let frame = src.split_to(FRAME_LEN);
                match Response::parse(&frame) {
                    Ok(response) => Ok(Some(response)),
                    // Bad magic/CRC: drop a byte and resync.
                    Err(_) => {
                        src.advance(1);
                        Ok(None)
                    }
                }
            }
            None => {
                // No magic yet: keep the last 3 bytes in case the magic
                // straddles the buffer boundary.
                let keep = 3usize.min(src.len());
                src.advance(src.len() - keep);
                Ok(None)
            }
        }
    }
}
