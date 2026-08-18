//! Thread-level tests: job in -> share out, hit FIFO semantics, ramp gating
//! (fake in-memory transport only).

use std::sync::Arc;
use std::time::Duration;

use bitcoin::block::Version;
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::Hash;
use bitcoin::pow::{CompactTarget, Target};
use tokio::sync::{mpsc, watch};

use super::chain::{ChainConfig, DVFS_INIT_WRITES, duty_word, version_bounds};
use super::protocol::{PLL_CONFIG_INIT, pll_freq_word};
use super::test_util::{TestChip, WireFrame, hit_frame, parse_wire_stream, response_frame};
use super::thread::{AuraConfig, AuraThread};
use crate::asic::hash_thread::{HashTask, HashThread, HashThreadEvent, ThreadRemovalSignal};
use crate::job_source::{
    Extranonce2, GeneralPurposeBits, JobTemplate, MerkleRootKind, VersionTemplate,
};

/// Tiny timings so thread tests run in real time; long intervals keep the
/// post-job traffic deterministic (only hit polls and ramp steps).
fn test_config() -> AuraConfig {
    AuraConfig {
        chain: ChainConfig {
            expected_chips: 2,
            pass_drain: Duration::from_millis(30),
            pass_pace: Duration::from_millis(40),
            max_passes: 3,
        },
        heartbeat_interval: Duration::from_secs(60),
        hit_poll_interval: Duration::from_millis(20),
        telemetry_interval: Duration::from_secs(60),
        ramp_step_interval: Duration::from_millis(5),
        ntime_interval: Duration::from_secs(60),
        expected_hashrate_th: 1.0,
        ramp_max_pll: super::chain::PLL_RAMP_END,
        psu: None,
        baud_switch: None,
    }
}

/// A task whose share target accepts any hash (full 256-bit target), so
/// every hit forwards.
fn test_task() -> (HashTask, mpsc::Receiver<crate::asic::hash_thread::Share>) {
    let template = Arc::new(JobTemplate {
        id: "test-job".into(),
        prev_blockhash: BlockHash::from_byte_array([0x11; 32]),
        version: VersionTemplate::new(
            Version::from_consensus(0x2000_0000),
            GeneralPurposeBits::full(),
        )
        .expect("valid version template"),
        bits: CompactTarget::from_consensus(0x1d00_ffff),
        share_target: Target::from(crate::u256::U256::MAX),
        time: 0x6a7d_79bf,
        merkle_root: MerkleRootKind::Fixed(TxMerkleNode::from_byte_array([0x22; 32])),
    });
    let (share_tx, share_rx) = mpsc::channel(8);
    let task = HashTask {
        template,
        en2_range: None,
        en2: Some(Extranonce2::new(0, 1).expect("valid en2")),
        share_target: Target::from(crate::u256::U256::MAX),
        ntime: 0x6a7d_79bf,
        share_tx,
    };
    (task, share_rx)
}

/// An 80-byte winning header that matches `test_task()` exactly, with the
/// nonce `0xdead_beef` in `header[76..80]`.
fn matching_header() -> [u8; 80] {
    let mut header = [0u8; 80];
    header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    header[4..36].fill(0x11);
    header[36..68].fill(0x22);
    header[68..72].copy_from_slice(&0x6a7d_79bfu32.to_le_bytes());
    header[72..76].copy_from_slice(&0x1d00_ffffu32.to_le_bytes());
    header[76..80].copy_from_slice(&0xdead_beefu32.to_le_bytes());
    header
}

/// Build a thread over a duplex transport.
fn make_thread(config: AuraConfig) -> (AuraThread, TestChip, watch::Sender<ThreadRemovalSignal>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (r, w) = tokio::io::split(a);
    let (removal_tx, removal_rx) = watch::channel(ThreadRemovalSignal::Running);
    let thread = AuraThread::new("test-aura".into(), r, w, config, removal_rx);
    (thread, TestChip::new(b), removal_tx)
}

#[tokio::test]
async fn no_commands_before_first_job() {
    let (_thread, mut chip, _removal_tx) = make_thread(test_config());
    // No task assigned: no discovery, no ramp, no heartbeat — bus silent.
    assert!(
        chip.expect_silence(Duration::from_millis(80)).await,
        "no commands may be written before the first job"
    );
}

#[tokio::test]
async fn job_in_share_out_with_empty_fifo_semantics() {
    let (mut thread, mut chip, _removal_tx) = make_thread(test_config());

    let mut event_rx = thread.take_event_receiver().expect("event receiver");
    thread.configure().await.expect("configure");
    let (task, mut share_rx) = test_task();

    // Answer the discovery sweep from a concurrent task while update_task
    // is in flight (bring-up happens inside the update).
    let chip_task = tokio::spawn(async move {
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        tokio::select! {
            _ = &mut deadline => panic!("no discovery sweep received"),
            frame = chip.expect_command() => {
                assert_eq!(frame.reg(), Some(0x02), "first command must be the sweep");
                chip.send(&response_frame(0x00, 0x02, 0x1200, 0)).await;
                chip.send(&response_frame(0x01, 0x02, 0x1200, 0)).await;
                chip
            }
        }
    });

    let old = thread.update_task(task.clone()).await.expect("update task");
    assert!(old.is_none(), "first task: no previous task");
    let mut chip = chip_task.await.expect("sweep responder");

    // configure() emits the expected-hashrate event.
    let event = event_rx.recv().await.expect("event");
    assert!(matches!(event, HashThreadEvent::ExpectedHashRate(_)));

    // Deterministic bring-up bytes: version bounds (4), DVFS setup (18),
    // init (4), jobs (2). Ramp steps and hit polls only ever follow.
    let bring_up_len = 4 * 36 + 18 * 36 + 4 * 36 + 2 * 92;
    let bytes = chip.read_bytes(bring_up_len).await;
    let frames = parse_wire_stream(&bytes);
    assert_eq!(frames.len(), 28, "26 commands + 2 job frames");

    // Version bounds for chips 0 and 1.
    assert_eq!(
        frames[0],
        WireFrame::Command {
            chip: 0x00,
            reg: 0x10,
            lcmd: 0x1000,
            data: version_bounds(0),
        }
    );
    assert_eq!(
        frames[1],
        WireFrame::Command {
            chip: 0x00,
            reg: 0x11,
            lcmd: 0x1000,
            data: 13,
        }
    );
    assert_eq!(
        frames[2],
        WireFrame::Command {
            chip: 0x01,
            reg: 0x10,
            lcmd: 0x1000,
            data: version_bounds(1),
        }
    );
    assert_eq!(
        frames[3],
        WireFrame::Command {
            chip: 0x01,
            reg: 0x11,
            lcmd: 0x1000,
            data: 13,
        }
    );

    // DVFS InitialSetup: exact 18 writes in exact order (broadcast).
    for (i, (lcmd, data)) in DVFS_INIT_WRITES.iter().enumerate() {
        assert_eq!(
            frames[4 + i],
            WireFrame::Command {
                chip: 0x80,
                reg: 0x81,
                lcmd: *lcmd,
                data: *data,
            },
            "DVFS write {i}"
        );
    }

    // Init: PLL_CONFIG, PLL_FREQ(80), DUTY, HASHCONFIG (broadcast).
    assert_eq!(
        frames[22],
        WireFrame::Command {
            chip: 0x80,
            reg: 0x18,
            lcmd: 0x1000,
            data: PLL_CONFIG_INIT,
        }
    );
    assert_eq!(
        frames[23],
        WireFrame::Command {
            chip: 0x80,
            reg: 0x19,
            lcmd: 0x1000,
            data: pll_freq_word(80),
        }
    );
    assert_eq!(
        frames[24],
        WireFrame::Command {
            chip: 0x80,
            reg: 0x68,
            lcmd: 0x1000,
            data: duty_word(80),
        }
    );
    assert_eq!(
        frames[25],
        WireFrame::Command {
            chip: 0x80,
            reg: 0x14,
            lcmd: 0x1000,
            data: 0x0200_0200,
        }
    );

    // Jobs for both chips: slot 0, job_id 0, nonce_start 0.
    assert_eq!(
        frames[26],
        WireFrame::Job {
            chip: 0x00,
            slot: 0,
            job_id: 0,
            ntime: 0x6a7d_79bf,
            nbits: 0x1d00_ffff,
            nonce_start: 0,
        }
    );
    assert_eq!(
        frames[27],
        WireFrame::Job {
            chip: 0x01,
            slot: 0,
            job_id: 0,
            ntime: 0x6a7d_79bf,
            nbits: 0x1d00_ffff,
            nonce_start: 0,
        }
    );

    // The ramp only runs after the job: the tail traffic must contain a
    // PLL_FREQ + DUTY_CYCLE + HASHCONFIG ramp group (hit polls are 0xc4).
    let tail = chip.read_bytes(1000).await;
    let tail_frames = parse_wire_stream(&tail);
    assert!(
        tail_frames
            .iter()
            .any(|f| matches!(f, WireFrame::Command { reg: 0x19, .. })),
        "ramp PLL_FREQ writes appear after the job"
    );
    assert!(
        tail_frames
            .iter()
            .any(|f| matches!(f, WireFrame::Command { reg: 0x68, .. })),
        "ramp DUTY_CYCLE writes appear after the job"
    );
    assert!(
        tail_frames
            .iter()
            .any(|f| matches!(f, WireFrame::Command { reg: 0x14, .. })),
        "ramp HASHCONFIG writes appear after the job"
    );

    // Scripted hit frame (chip 0, matching header, nonce 0xdead_beef):
    // the thread forwards a share with the correct nonce.
    chip.send(&hit_frame(0x00, matching_header())).await;
    let share = tokio::time::timeout(Duration::from_secs(2), share_rx.recv())
        .await
        .expect("share arrives")
        .expect("share channel open");
    assert_eq!(share.nonce, 0xdead_beef);
    assert_eq!(share.ntime, 0x6a7d_79bf);
    assert_eq!(share.version.to_consensus(), 0x2000_0000);
    assert_eq!(share.extranonce2, Some(Extranonce2::new(0, 1).unwrap()));

    // Empty hit FIFO: no response is normal — no share, no error.
    let empty = tokio::time::timeout(Duration::from_millis(150), share_rx.recv()).await;
    assert!(empty.is_err(), "empty hit FIFO must not produce a share");

    // The thread is still alive: a second hit still forwards.
    chip.send(&hit_frame(0x01, matching_header())).await;
    let share2 = tokio::time::timeout(Duration::from_secs(2), share_rx.recv())
        .await
        .expect("second share arrives")
        .expect("share channel open");
    assert_eq!(share2.nonce, 0xdead_beef);
}

#[tokio::test]
async fn stale_hit_header_is_dropped() {
    let (mut thread, mut chip, _removal_tx) = make_thread(test_config());
    thread.configure().await.expect("configure");
    let (task, mut share_rx) = test_task();

    let chip_task = tokio::spawn(async move {
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        tokio::select! {
            _ = &mut deadline => panic!("no discovery sweep received"),
            frame = chip.expect_command() => {
                assert_eq!(frame.reg(), Some(0x02));
                chip.send(&response_frame(0x00, 0x02, 0x1200, 0)).await;
                chip.send(&response_frame(0x01, 0x02, 0x1200, 0)).await;
                chip
            }
        }
    });

    thread.update_task(task.clone()).await.expect("update task");
    let mut chip = chip_task.await.expect("sweep responder");

    // A hit whose ntime does not match the current task is stale.
    let mut header = matching_header();
    header[68..72].copy_from_slice(&0x1111_1111u32.to_le_bytes());
    chip.send(&hit_frame(0x00, header)).await;

    // The stale hit is dropped: no share within a generous window.
    let empty = tokio::time::timeout(Duration::from_millis(250), share_rx.recv()).await;
    assert!(empty.is_err(), "stale hit must not produce a share");
}
