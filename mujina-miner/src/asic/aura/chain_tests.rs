//! Chain-driver tests: discovery, version bounds, DVFS setup, heartbeat,
//! ramp writes and init sequence (fake in-memory transport only).

use std::time::Duration;

use super::chain::{
    ChainConfig, DVFS_INIT_WRITES, HASHCONFIG_VALUE, REG_LCMD, discover_chips, duty_word,
    version_bounds,
};
use super::dvfs::{self, DVFS_HEARTBEAT_LCMD, DVFS_HEARTBEAT_PAYLOAD};
use super::protocol::{PLL_CONFIG_INIT, Register, pll_freq_word};
use super::test_util::{TestChip, WireFrame, parse_wire_stream, response_frame};

/// Split a duplex pair into driver read/write halves plus a test chip.
fn transport() -> (
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    TestChip,
) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (r, w) = tokio::io::split(a);
    (r, w, TestChip::new(b))
}

/// Build a duplex pair for tests that must drive the driver and the fake
/// chip concurrently (discovery): the driver half is moved into a spawned
/// task, the test chip drives the other half.
fn concurrent_transport(
    config: ChainConfig,
) -> (tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>, TestChip) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (mut r, mut w) = tokio::io::split(a);
    let chip = TestChip::new(b);
    let driver = tokio::spawn(async move { discover_chips(&mut r, &mut w, &config).await });
    (driver, chip)
}

fn fast_config() -> ChainConfig {
    ChainConfig {
        expected_chips: 21,
        pass_drain: Duration::from_millis(30),
        pass_pace: Duration::from_millis(40),
        max_passes: 10,
    }
}

#[tokio::test]
async fn discovery_accumulates_unique_chips_across_partial_passes() {
    let config = fast_config();
    let (driver, mut chip) = concurrent_transport(config.clone());

    // Each pass ACKs a different subset; no single pass reveals all 21.
    let passes: Vec<Vec<u8>> = vec![
        vec![0, 1, 2],
        vec![3, 4, 5, 6],
        vec![7, 8, 9, 10],
        vec![128, 129, 130, 131],
        vec![132, 133, 134, 135, 136, 137],
    ];
    let expected: Vec<u8> = (0..=10).chain(128..=137).collect();

    for subset in &passes {
        let sweep = chip.expect_command().await;
        assert_eq!(
            sweep.reg(),
            Some(u8::from(Register::Telemetry)),
            "sweep targets reg 0x02"
        );
        let frames: Vec<[u8; 16]> = subset
            .iter()
            .map(|&c| response_frame(c, u8::from(Register::Telemetry), 0x1200, 0))
            .collect();
        for frame in frames {
            chip.send(&frame).await;
        }
    }

    let found = driver
        .await
        .expect("discovery task")
        .expect("discovery completes");
    let mut sorted = found.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, expected, "all 21 unique chip IDs accumulated");
}

#[tokio::test]
async fn discovery_stops_early_when_all_chips_found() {
    let config = fast_config();
    let (driver, mut chip) = concurrent_transport(config);

    // First pass ACKs all 21 chips; discovery must not sweep again.
    let sweep = chip.expect_command().await;
    assert_eq!(sweep.reg(), Some(u8::from(Register::Telemetry)));
    for c in 0..=10u8 {
        chip.send(&response_frame(c, 0x02, 0x1200, 0)).await;
    }
    for c in 128..=137u8 {
        chip.send(&response_frame(c, 0x02, 0x1200, 0)).await;
    }

    let found = driver
        .await
        .expect("discovery task")
        .expect("discovery completes");
    assert_eq!(found.len(), 21);
    // No second sweep: the bus stays silent.
    assert!(chip.expect_silence(Duration::from_millis(100)).await);
}

#[test]
fn version_bounds_partition_windows() {
    // Window 0x0c30: lower = i*0x0c30, upper = lower + 0x0c2f, packed
    // lower | (upper << 16). 21 chips tile 0x0000..=0xffef.
    for i in 0..21usize {
        let lower = i as u32 * 0x0c30;
        let upper = lower + 0x0c2f;
        assert_eq!(version_bounds(i), lower | (upper << 16), "chip index {i}");
    }
    assert_eq!(version_bounds(0), 0x0c2f_0000);
    assert_eq!(version_bounds(20), 0xffef_f3c0);
}

#[tokio::test]
async fn version_bounds_written_per_chip() {
    let (_r, mut w, mut chip) = transport();
    let chips = [0x00u8, 0x01, 0x0a, 0x80, 0x89];

    super::chain::configure_version_bounds(&mut w, &chips)
        .await
        .expect("version bounds complete");

    for (i, &c) in chips.iter().enumerate() {
        let bound = chip.expect_command().await;
        assert_eq!(
            bound,
            WireFrame::Command {
                chip: c,
                reg: 0x10,
                lcmd: 0x1000,
                data: version_bounds(i),
            },
            "version bound for chip 0x{c:02x}"
        );
        let shift = chip.expect_command().await;
        assert_eq!(
            shift,
            WireFrame::Command {
                chip: c,
                reg: 0x11,
                lcmd: 0x1000,
                data: 13,
            },
            "version shift for chip 0x{c:02x}"
        );
    }
}

#[tokio::test]
async fn dvfs_initial_setup_emits_exact_18_writes() {
    let (_r, mut w, mut chip) = transport();

    super::chain::dvfs_initial_setup(&mut w)
        .await
        .expect("DVFS initial setup complete");

    for (lcmd, data) in DVFS_INIT_WRITES {
        let write = chip.expect_command().await;
        assert_eq!(
            write,
            WireFrame::Command {
                chip: 0x80, // broadcast
                reg: u8::from(Register::Dvfs),
                lcmd,
                data,
            },
            "DVFS write lcmd=0x{lcmd:04x}"
        );
    }
}

#[tokio::test]
async fn dvfs_heartbeat_emits_exact_payload() {
    let (_r, mut w, mut chip) = transport();

    dvfs::heartbeat(&mut w).await.expect("heartbeat complete");

    for data in DVFS_HEARTBEAT_PAYLOAD {
        let write = chip.expect_command().await;
        assert_eq!(
            write,
            WireFrame::Command {
                chip: 0x80, // broadcast
                reg: u8::from(Register::Dvfs),
                lcmd: DVFS_HEARTBEAT_LCMD,
                data,
            },
            "heartbeat word 0x{data:08x}"
        );
    }
}

#[tokio::test]
async fn ramp_step_writes_pll_duty_hashconfig() {
    let (_r, mut w, mut chip) = transport();

    dvfs::write_ramp_step(&mut w, None, 100)
        .await
        .expect("ramp step complete");

    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::PllFreq),
            lcmd: REG_LCMD,
            data: pll_freq_word(100),
        }
    );
    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::DutyCycle),
            lcmd: REG_LCMD,
            data: duty_word(100),
        }
    );
    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::HashConfig),
            lcmd: REG_LCMD,
            data: HASHCONFIG_VALUE,
        }
    );
}

#[tokio::test]
async fn init_chain_writes_config_freq_duty_hashconfig() {
    let (_r, mut w, mut chip) = transport();

    super::chain::init_chain(&mut w, 80)
        .await
        .expect("init complete");

    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::PllConfig),
            lcmd: REG_LCMD,
            data: PLL_CONFIG_INIT,
        }
    );
    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::PllFreq),
            lcmd: REG_LCMD,
            data: pll_freq_word(80),
        }
    );
    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::DutyCycle),
            lcmd: REG_LCMD,
            data: duty_word(80),
        }
    );
    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x80,
            reg: u8::from(Register::HashConfig),
            lcmd: REG_LCMD,
            data: HASHCONFIG_VALUE,
        }
    );
}

/// The hit-poll command carries register byte 0x84 | 0x40 = 0xc4, lcmd 0.
#[tokio::test]
async fn hit_poll_uses_register_byte_0xc4() {
    let (_r, mut w, mut chip) = transport();

    super::chain::hit_poll(&mut w, 0x05)
        .await
        .expect("hit poll complete");

    assert_eq!(
        chip.expect_command().await,
        WireFrame::Command {
            chip: 0x05,
            reg: 0xc4,
            lcmd: 0x0000,
            data: 0,
        }
    );
}

/// drain_frames handles interleaved 16-byte responses and 92-byte hit
/// frames, tolerating preamble junk between them.
#[test]
fn drain_frames_handles_mixed_frame_types() {
    use super::test_util::hit_frame;
    use bytes::BytesMut;

    let mut buf = BytesMut::new();
    // Response, junk preamble, hit, truncated hit.
    buf.extend_from_slice(&response_frame(0x03, 0x02, 0x1200, 0x1234));
    buf.extend_from_slice(&[0u8; 20]);
    let mut header = [0u8; 80];
    header[76..80].copy_from_slice(&0xdead_beefu32.to_le_bytes());
    let hit = hit_frame(0x03, header);
    buf.extend_from_slice(&hit);
    buf.extend_from_slice(&hit[..40]); // truncated

    let (responses, hits) = super::chain::drain_frames(&mut buf);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].chip, 0x03);
    assert_eq!(responses[0].data, 0x1234);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].nonce, 0xdead_beef);
    // Truncated hit stays buffered for the next chunk.
    assert_eq!(buf.len(), 40);
}

/// Wire round-trip through parse_wire_stream decodes commands and jobs.
#[test]
fn parse_wire_stream_decodes_commands_and_jobs() {
    use super::protocol::JobFrame;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0u8; 20]);
    bytes.extend_from_slice(&super::test_util::response_frame(
        0x01, 0x81, 0x1f00, 0x5074,
    ));
    let job = JobFrame::encode(
        0x02,
        3,
        7,
        &[0xaa; 32],
        &[0xbb; 32],
        0x6a7d_79bf,
        0x1d00_ffff,
        0,
    );
    bytes.extend_from_slice(&job);

    let frames = parse_wire_stream(&bytes);
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        WireFrame::Command {
            chip: 0x01,
            reg: 0x81,
            lcmd: 0x1f00,
            data: 0x5074,
        }
    );
    assert_eq!(
        frames[1],
        WireFrame::Job {
            chip: 0x02,
            slot: 3,
            job_id: 7,
            ntime: 0x6a7d_79bf,
            nbits: 0x1d00_ffff,
            nonce_start: 0,
        }
    );
}
