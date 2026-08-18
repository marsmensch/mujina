//! Error types for the Aura ASIC protocol.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid frame length: expected {expected} bytes, got {found}")]
    InvalidFrameLength { expected: usize, found: usize },

    #[error("bad magic: expected 0x{expected:08x}, got 0x{found:08x}")]
    BadMagic { expected: u32, found: u32 },

    #[error("bad CRC: expected 0x{expected:08x}, got 0x{found:08x}")]
    BadCrc { expected: u32, found: u32 },

    #[error("invalid register: 0x{0:02x}")]
    InvalidRegister(u8),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
