//! Reference tests against pre-verified Aura protocol vectors.

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use super::crc::aura_crc32;
use super::error::ProtocolError;
use super::protocol::{
    COMMAND_MAGIC, Command, FRAME_LEN, FrameCodec, HitFrame, JobFrame, LONG_FRAME_LEN,
    PREAMBLE_LEN, RESPONSE_MAGIC, Register, Response, fhash_mhz, pll_freq_word,
};
use super::test_data::{CAPTURED_TX, JOB_FRAME, RX_RESPONSE_FRAME, TX_VECTORS};

/// CRC matches every pre-verified vector (over bytes `[0..12]` / `[0..88]`).
#[test]
fn crc_matches_vectors() {
    for v in TX_VECTORS {
        let expected = u32::from_le_bytes(v.frame[12..16].try_into().unwrap());
        assert_eq!(aura_crc32(&v.frame[0..12]), expected);
    }
    for frame in CAPTURED_TX {
        let expected = u32::from_le_bytes(frame[12..16].try_into().unwrap());
        assert_eq!(aura_crc32(&frame[0..12]), expected);
    }
    let expected = u32::from_le_bytes(RX_RESPONSE_FRAME[12..16].try_into().unwrap());
    assert_eq!(aura_crc32(&RX_RESPONSE_FRAME[0..12]), expected);
    let expected = u32::from_le_bytes(JOB_FRAME[88..92].try_into().unwrap());
    assert_eq!(aura_crc32(&JOB_FRAME[0..88]), expected);
}

/// Command encoder reproduces every TX vector byte-exactly.
#[test]
fn command_encoder_reproduces_tx_vectors() {
    for v in TX_VECTORS {
        let cmd = Command::ReadRegister {
            broadcast: false,
            chip_address: v.chip,
            register: Register::try_from(v.reg).unwrap(),
            lcmd: v.lcmd,
            data: v.data,
        };
        assert_eq!(cmd.encode(), v.frame, "vector (chip={:#04x})", v.chip);
    }

    for frame in CAPTURED_TX {
        // Variant choice does not affect the encoded bytes; use
        // WriteRegister for the DVFS write and ReadRegister otherwise.
        let cmd = if frame[5] == 0x81 {
            Command::WriteRegister {
                broadcast: false,
                chip_address: frame[4],
                register: Register::try_from(frame[5]).unwrap(),
                lcmd: u16::from_le_bytes([frame[6], frame[7]]),
                data: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
            }
        } else {
            Command::ReadRegister {
                broadcast: false,
                chip_address: frame[4],
                register: Register::try_from(frame[5]).unwrap(),
                lcmd: u16::from_le_bytes([frame[6], frame[7]]),
                data: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
            }
        };
        assert_eq!(cmd.encode(), *frame);
    }
}

/// Wire encoder emits the 20-byte zero preamble followed by the frame.
#[test]
fn wire_encoder_emits_preamble_and_frame() {
    let cmd = Command::ReadRegister {
        broadcast: false,
        chip_address: 0x00,
        register: Register::Telemetry,
        lcmd: 0x1200,
        data: 0,
    };
    let mut buf = BytesMut::new();
    FrameCodec::default()
        .encode(cmd, &mut buf)
        .expect("encode command");
    assert_eq!(buf.len(), PREAMBLE_LEN + FRAME_LEN);
    assert!(buf[..PREAMBLE_LEN].iter().all(|&b| b == 0));
    assert_eq!(&buf[PREAMBLE_LEN..], &cmd.encode()[..]);
}

/// Response parser decodes the RX vector and validates magic and CRC.
#[test]
fn response_parser_decodes_rx_vector() {
    let resp = Response::parse(&RX_RESPONSE_FRAME).expect("parse rx vector");
    assert_eq!(resp.chip, 0x09);
    assert_eq!(resp.reg, 0x02);
    assert_eq!(resp.lcmd, 0x1200);
    assert_eq!(resp.data, 0x0719_eb08);
}

#[test]
fn response_parser_rejects_bad_magic() {
    let mut frame = RX_RESPONSE_FRAME;
    frame[0] ^= 0xff;
    assert!(matches!(
        Response::parse(&frame),
        Err(ProtocolError::BadMagic { .. })
    ));
}

#[test]
fn response_parser_rejects_bad_crc() {
    let mut frame = RX_RESPONSE_FRAME;
    frame[15] ^= 0xff;
    assert!(matches!(
        Response::parse(&frame),
        Err(ProtocolError::BadCrc { .. })
    ));
}

/// FrameCodec round-trip: encode a command, then decode the matching
/// response stream and recover the same fields; decoder skips preambles.
#[test]
fn frame_codec_round_trip() {
    let cmd = Command::WriteRegister {
        broadcast: false,
        chip_address: 0x01,
        register: Register::WorkData,
        lcmd: 0x1000,
        data: 0x923f_8610,
    };
    let mut codec = FrameCodec::default();

    // Encode: preamble + frame.
    let mut buf = BytesMut::new();
    codec.encode(cmd, &mut buf).expect("encode command");
    assert_eq!(buf.len(), PREAMBLE_LEN + FRAME_LEN);
    assert!(buf[..PREAMBLE_LEN].iter().all(|&b| b == 0));
    assert_eq!(&buf[PREAMBLE_LEN..], &cmd.encode()[..]);

    // Decode: responses carry a different magic; synthesize the matching
    // response frame (same payload) behind a preamble and verify the
    // decoder skips the preamble and recovers the same fields.
    let mut resp_frame = cmd.encode();
    resp_frame[0..4].copy_from_slice(&RESPONSE_MAGIC.to_be_bytes());
    let crc = aura_crc32(&resp_frame[0..12]);
    resp_frame[12..16].copy_from_slice(&crc.to_le_bytes());

    let mut rx = BytesMut::new();
    rx.extend_from_slice(&[0u8; PREAMBLE_LEN]);
    rx.extend_from_slice(&resp_frame);
    let decoded = codec
        .decode(&mut rx)
        .expect("decode response")
        .expect("frame");
    assert_eq!(decoded.chip, 0x01);
    assert_eq!(decoded.reg, 0x01);
    assert_eq!(decoded.lcmd, 0x1000);
    assert_eq!(decoded.data, 0x923f_8610);
    assert!(rx.is_empty());
}

/// Decoder skips the preamble of a real RX stream.
#[test]
fn frame_codec_decodes_rx_stream() {
    let mut codec = FrameCodec::default();
    let mut rx = BytesMut::new();
    rx.extend_from_slice(&[0u8; PREAMBLE_LEN]);
    rx.extend_from_slice(&RX_RESPONSE_FRAME);
    let decoded = codec
        .decode(&mut rx)
        .expect("decode response")
        .expect("frame");
    assert_eq!(decoded.chip, 0x09);
    assert_eq!(decoded.data, 0x0719_eb08);
    assert!(rx.is_empty());
}

/// JobFrame::encode reproduces the captured 92-byte job frame byte-exactly.
#[test]
fn job_frame_matches_captured_frame() {
    assert_eq!(&JOB_FRAME[6..8], &[0x2a, 0x04], "lcmd 0x042a (slot 1)");

    let chip = JOB_FRAME[4];
    let slot = 1;
    let job_id = u32::from_le_bytes([JOB_FRAME[8], JOB_FRAME[9], JOB_FRAME[10], JOB_FRAME[11]]);
    let prevhash: [u8; 32] = JOB_FRAME[12..44].try_into().unwrap();
    let merkle: [u8; 32] = JOB_FRAME[44..76].try_into().unwrap();
    let ntime = u32::from_le_bytes([JOB_FRAME[76], JOB_FRAME[77], JOB_FRAME[78], JOB_FRAME[79]]);
    let nbits = u32::from_le_bytes([JOB_FRAME[80], JOB_FRAME[81], JOB_FRAME[82], JOB_FRAME[83]]);
    let nonce_start =
        u32::from_le_bytes([JOB_FRAME[84], JOB_FRAME[85], JOB_FRAME[86], JOB_FRAME[87]]);

    let encoded = JobFrame::encode(
        chip,
        slot,
        job_id,
        &prevhash,
        &merkle,
        ntime,
        nbits,
        nonce_start,
    );
    assert_eq!(encoded.len(), LONG_FRAME_LEN);
    assert_eq!(encoded, JOB_FRAME);
}

/// HitFrame::parse on a synthesized 92-byte frame.
#[test]
fn hit_frame_parse_round_trip() {
    let mut frame = [0u8; LONG_FRAME_LEN];
    frame[0..4].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
    frame[4] = 0x03; // chip
    frame[5] = 0xc4; // reg | 0x40 (hit command)
    frame[6] = 0x1d; // nbits
    frame[7] = 0x00; // id_hi_seq
    for (i, b) in frame[8..88].iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce: u32 = 0xdead_beef;
    frame[84..88].copy_from_slice(&nonce.to_le_bytes());
    let crc = aura_crc32(&frame[0..88]);
    frame[88..92].copy_from_slice(&crc.to_le_bytes());

    let hit = HitFrame::parse(&frame).expect("parse hit frame");
    assert_eq!(hit.chip, 0x03);
    assert_eq!(hit.nonce, nonce);
    assert_eq!(hit.header, frame[8..88]);

    // Bad CRC rejected.
    frame[91] ^= 0xff;
    assert!(matches!(
        HitFrame::parse(&frame),
        Err(ProtocolError::BadCrc { .. })
    ));
}

/// PLL helpers: wire word is N << 20, hash frequency is 5N/4 MHz.
#[test]
fn pll_helpers() {
    assert_eq!(pll_freq_word(491), 0x1eb0_0000);
    assert_eq!(pll_freq_word(160), 0x0a00_0000);
    assert_eq!(fhash_mhz(491.0), 613.75);
    assert_eq!(fhash_mhz(160.0), 200.0);
}

/// Register enum round-trips through u8.
#[test]
fn register_conversions() {
    assert_eq!(u8::from(Register::Job), 0x84);
    assert_eq!(Register::try_from(0x84).unwrap(), Register::Job);
    assert_eq!(u8::from(Register::PllFreq), 0x19);
    assert!(matches!(
        Register::try_from(0xff),
        Err(ProtocolError::InvalidRegister(0xff))
    ));
}
