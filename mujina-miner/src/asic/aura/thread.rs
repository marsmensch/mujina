//! Aura HashThread implementation (Apollo III chain driver).
//!
//! An `AuraThread` represents a chain of Aura chips on a shared serial bus
//! as a schedulable worker. Unlike [`BM13xxThread`](super::super::bm13xx::thread::BM13xxThread),
//! the thread takes **byte-level** read/write halves and does its own
//! framing: the shared [`FrameCodec`](super::protocol::FrameCodec) only
//! yields 16-byte responses and frame-syncs on the response magic alone, so
//! 92-byte hit frames (command magic) would never reach the thread. The
//! chain framing ([`chain::drain_frames`]) recognises both frame kinds.
//!
//! Chip bring-up (discovery, version bounds, DVFS initial setup, PLL/duty
//! init) happens lazily on the first job assignment. The DVFS heartbeat
//! then runs continuously; hit polls, telemetry reads and ntime rolling run
//! while a task is live; the frequency ramp runs only once pool work is on
//! the chips. The PSU voltage climb is board-level (later phase) and is not
//! implemented here.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use bitcoin::block::{Header as BlockHeader, Version};
use bitcoin::hash_types::{BlockHash, TxMerkleNode};
use bitcoin::hashes::Hash;
use bitcoin::pow::CompactTarget;
use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use super::chain::{self, ChainConfig};
use super::dvfs::{self, RampState};
use super::protocol::{HitFrame, JobFrame};
use super::telemetry::TelemetryTracker;
use crate::asic::hash_thread::{
    HashTask, HashThread, HashThreadCapabilities, HashThreadEvent, HashThreadStatus, Share,
    ThreadRemovalSignal,
};
use crate::job_source::MerkleRootKind;
use crate::tracing::prelude::*;
use crate::types::{Difficulty, HashRate};

/// Placeholder expected hashrate declared on [`HashThread::configure`].
///
/// Not a measured value: replace with a per-chip rate once the final ramp
/// frequency has been verified on device (G6).
const EXPECTED_HASHRATE_TH: f64 = 1.0;

/// Aura thread parameters (locked defaults; tests shrink timings).
#[derive(Debug, Clone)]
pub struct AuraConfig {
    /// Chain discovery parameters.
    pub chain: ChainConfig,
    /// DVFS heartbeat interval (~2.1 s on device).
    pub heartbeat_interval: Duration,
    /// Hit-poll interval (tunable; not locked ground truth).
    pub hit_poll_interval: Duration,
    /// Telemetry counter poll interval (tunable; not locked ground truth).
    pub telemetry_interval: Duration,
    /// Ramp step interval (~50 ms on device).
    pub ramp_step_interval: Duration,
    /// ntime roll interval (mirrors BM13xx behavior).
    pub ntime_interval: Duration,
}

impl Default for AuraConfig {
    fn default() -> Self {
        Self {
            chain: ChainConfig::default(),
            heartbeat_interval: Duration::from_millis(2100),
            hit_poll_interval: Duration::from_millis(200),
            telemetry_interval: Duration::from_secs(5),
            ramp_step_interval: Duration::from_millis(50),
            ntime_interval: Duration::from_secs(1),
        }
    }
}

/// Command messages sent from the scheduler to the thread actor.
#[derive(Debug)]
enum ThreadCommand {
    /// Declare expected hashrate and ready the thread for work.
    Configure,

    /// Update task (old shares still valid).
    UpdateTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Replace task (old shares invalid).
    ReplaceTask {
        new_task: HashTask,
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Go idle (stop hashing, low power).
    GoIdle {
        response_tx: oneshot::Sender<Result<Option<HashTask>>>,
    },

    /// Shutdown the thread.
    #[expect(unused)]
    Shutdown,
}

/// Aura HashThread implementation.
///
/// Represents a chain of Aura chips as a schedulable worker. The thread
/// manages serial communication with the chips, forwards shares, and reports
/// events. Chip initialization happens lazily when first work is assigned.
pub struct AuraThread {
    /// Human-readable name for logging.
    name: String,

    /// Channel for sending commands to the actor.
    command_tx: mpsc::Sender<ThreadCommand>,

    /// Event receiver (taken by the scheduler).
    event_rx: Option<mpsc::Receiver<HashThreadEvent>>,

    /// Cached capabilities.
    capabilities: HashThreadCapabilities,

    /// Shared status (updated by the actor task).
    status: Arc<RwLock<HashThreadStatus>>,
}

impl AuraThread {
    /// Create a new Aura thread over byte-level read/write halves.
    ///
    /// The thread starts with chips unconfigured; the chain is brought up
    /// lazily on the first job assignment.
    ///
    /// # Arguments
    /// * `name` - Human-readable name for logging.
    /// * `reader` - Byte-level read half (chip responses arrive here).
    /// * `writer` - Byte-level write half (commands/jobs are written here).
    /// * `config` - Chain and timing parameters.
    /// * `removal_rx` - Watch channel for board-triggered removal.
    pub fn new<R, W>(
        name: String,
        reader: R,
        writer: W,
        config: AuraConfig,
        removal_rx: watch::Receiver<ThreadRemovalSignal>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, event_rx) = mpsc::channel(100);

        let status = Arc::new(RwLock::new(HashThreadStatus::default()));
        let status_clone = Arc::clone(&status);

        tokio::spawn(async move {
            aura_thread_actor(
                command_rx,
                event_tx,
                removal_rx,
                status_clone,
                reader,
                writer,
                config,
            )
            .await;
        });

        Self {
            name,
            command_tx,
            event_rx: Some(event_rx),
            capabilities: HashThreadCapabilities::default(),
            status,
        }
    }
}

#[async_trait]
impl HashThread for AuraThread {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &HashThreadCapabilities {
        &self.capabilities
    }

    async fn configure(&mut self) -> Result<()> {
        self.command_tx
            .send(ThreadCommand::Configure)
            .await
            .map_err(|_| anyhow!("command channel closed"))
    }

    async fn update_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::UpdateTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    async fn replace_task(&mut self, new_task: HashTask) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::ReplaceTask {
                new_task,
                response_tx,
            })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    async fn go_idle(&mut self) -> Result<Option<HashTask>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ThreadCommand::GoIdle { response_tx })
            .await
            .map_err(|_| anyhow!("command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("no response from thread"))?
    }

    fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<HashThreadEvent>> {
        self.event_rx.take()
    }

    fn status(&self) -> HashThreadStatus {
        self.status.read().unwrap().clone()
    }
}

/// Encode a job frame for every chip and write it to the chain.
///
/// `slot` and `job_id` advance once per dispatch (not per chip); the chips
/// divide nonce space themselves through their version-bound windows, so
/// `nonce_start` is always 0.
async fn dispatch_job<W>(
    writer: &mut W,
    chips: &[u8],
    slot: &mut u8,
    job_id: &mut u32,
    task: &HashTask,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let template = task.template.as_ref();
    let merkle = match &template.merkle_root {
        MerkleRootKind::Computed(_) => {
            let en2 = task
                .en2
                .as_ref()
                .ok_or_else(|| anyhow!("EN2 required for computed merkle root"))?;
            template
                .compute_merkle_root(en2)
                .context("merkle root computation failed")?
        }
        MerkleRootKind::Fixed(root) => *root,
    };
    let prevhash: [u8; 32] = *template.prev_blockhash.as_byte_array();
    let merkle_bytes: [u8; 32] = *merkle.as_byte_array();
    let nbits = template.bits.to_consensus();

    for &chip in chips {
        let frame = JobFrame::encode(
            chip,
            *slot,
            *job_id,
            &prevhash,
            &merkle_bytes,
            task.ntime,
            nbits,
            0,
        );
        writer
            .write_all(&frame)
            .await
            .context("failed to write Aura job frame")?;
    }
    *slot = slot.wrapping_add(1);
    *job_id = job_id.wrapping_add(1);
    Ok(())
}

/// Read a little-endian u32 from a fixed slice offset.
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("4-byte slice"))
}

/// Process a 92-byte hit frame against the current task.
///
/// The hit frame echoes the full 80-byte winning header, so the share is
/// self-validating: it is accepted only when the echoed prevhash, ntime and
/// bits match the current task and the resulting hash meets the task's share
/// target. Hits that fail any of these checks are stale and dropped.
async fn handle_hit(
    hit: HitFrame,
    current_task: &Option<HashTask>,
    status: &Arc<RwLock<HashThreadStatus>>,
) -> Result<()> {
    // TODO(G6): the hit-response magic (COMMAND vs RESPONSE) is assumed to
    // be the command magic per HitFrame::parse; verify on device.
    let task = match current_task {
        Some(task) => task,
        None => {
            trace!(
                chip = format!("0x{:02x}", hit.chip),
                "hit for no active task"
            );
            return Ok(());
        }
    };
    let template = task.template.as_ref();

    let version = Version::from_consensus(le32(&hit.header[0..4]) as i32);
    let prev_blockhash =
        BlockHash::from_byte_array(hit.header[4..36].try_into().expect("32 bytes"));
    let merkle_root =
        TxMerkleNode::from_byte_array(hit.header[36..68].try_into().expect("32 bytes"));
    let ntime = le32(&hit.header[68..72]);
    let bits = CompactTarget::from_consensus(le32(&hit.header[72..76]));

    if prev_blockhash != template.prev_blockhash || bits != template.bits || ntime != task.ntime {
        trace!(
            chip = format!("0x{:02x}", hit.chip),
            nonce = format!("{:#x}", hit.nonce),
            "hit header mismatch with current task (stale share)"
        );
        return Ok(());
    }

    let header = BlockHeader {
        version,
        prev_blockhash,
        merkle_root,
        time: ntime,
        bits,
        nonce: hit.nonce,
    };
    let hash = header.block_hash();

    if !task.share_target.is_met_by(hash) {
        trace!(
            chip = format!("0x{:02x}", hit.chip),
            nonce = format!("{:#x}", hit.nonce),
            hash = %hash,
            hash_diff = %Difficulty::from_hash(&hash),
            target_diff = %Difficulty::from_target(task.share_target),
            "Nonce does not meet share target (filtered)"
        );
        return Ok(());
    }

    let share = Share {
        nonce: hit.nonce,
        hash,
        version,
        ntime,
        extranonce2: task.en2,
        expected_work: task.share_target.to_work(),
    };

    let mut sent = false;
    if task.share_tx.send(share).await.is_err() {
        debug!("Share channel closed (task replaced)");
    } else {
        sent = true;
        debug!(
            chip = format!("0x{:02x}", hit.chip),
            nonce = format!("{:#x}", hit.nonce),
            hash = %hash,
            hash_diff = %Difficulty::from_hash(&hash),
            target_diff = %Difficulty::from_target(task.share_target),
            "Share found and sent"
        );
    }

    if sent {
        let mut s = status.write().unwrap();
        s.chip_shares_found += 1;
        s.pool_shares_submitted += 1;
    }
    Ok(())
}

/// Internal actor task for `AuraThread`.
///
/// Runs as an independent Tokio task: reads raw bytes from the chip bus,
/// frame-syncs both response and hit frames, services scheduler commands,
/// and drives the heartbeat, hit polls, telemetry polls, ntime rolling and
/// the frequency ramp.
async fn aura_thread_actor<R, W>(
    mut command_rx: mpsc::Receiver<ThreadCommand>,
    event_tx: mpsc::Sender<HashThreadEvent>,
    mut removal_rx: watch::Receiver<ThreadRemovalSignal>,
    status: Arc<RwLock<HashThreadStatus>>,
    mut reader: R,
    mut writer: W,
    config: AuraConfig,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut chips: Vec<u8> = Vec::new();
    let mut chain_initialized = false;
    let mut ramp_started = false;
    let mut current_task: Option<HashTask> = None;
    let mut slot: u8 = 0;
    let mut job_id: u32 = 0;
    let mut telemetry = TelemetryTracker::new();
    let mut ramp = RampState::new();

    let mut heartbeat_tick = tokio::time::interval(config.heartbeat_interval);
    let mut hit_poll_tick = tokio::time::interval(config.hit_poll_interval);
    let mut telemetry_tick = tokio::time::interval(config.telemetry_interval);
    // ntime uses interval_at so the first tick is not immediate: an
    // immediate first tick races a freshly-assigned task and rolls ntime
    // before any hit can echo the original header.
    let mut ntime_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + config.ntime_interval,
        config.ntime_interval,
    );
    let mut ramp_tick = tokio::time::interval(config.ramp_step_interval);
    for ticker in [
        &mut heartbeat_tick,
        &mut hit_poll_tick,
        &mut telemetry_tick,
        &mut ntime_ticker,
        &mut ramp_tick,
    ] {
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    }

    let mut buf = [0u8; 1024];
    let mut rx_buf = BytesMut::new();

    loop {
        tokio::select! {
            // Removal signal (highest priority).
            signal = removal_rx.changed() => {
                match signal {
                    Ok(()) => {
                        let signal = removal_rx.borrow().clone();
                        match signal {
                            ThreadRemovalSignal::Running => {}
                            _reason => {
                                status.write().unwrap().is_active = false;
                                break;
                            }
                        }
                    }
                    // Sender dropped: nobody can signal anymore, shut down.
                    Err(_) => break,
                }
            }

            // Commands from the scheduler.
            Some(command) = command_rx.recv() => {
                match command {
                    ThreadCommand::Configure => {
                        let expected = HashRate::from_terahashes(EXPECTED_HASHRATE_TH);
                        if event_tx.send(HashThreadEvent::ExpectedHashRate(expected)).await.is_err() {
                            debug!("Event channel closed during configure");
                        }
                    }

                    ThreadCommand::UpdateTask { new_task, response_tx } => {
                        if current_task.is_none() {
                            debug!(job = %new_task.template.id, "Updating work from idle");
                        }

                        if !chain_initialized {
                            trace!("Initializing Aura chain on first assignment.");
                            match chain::bring_up_chain(&mut reader, &mut writer, &config.chain).await {
                                Ok(found) => {
                                    chips = found;
                                    chain_initialized = true;
                                }
                                Err(e) => {
                                    error!(error = %e, "Aura chain bring-up failed");
                                    response_tx.send(Err(e)).ok();
                                    continue;
                                }
                            }
                        }

                        let old_task = current_task.replace(new_task.clone());
                        if let Err(e) = dispatch_job(&mut writer, &chips, &mut slot, &mut job_id, &new_task).await {
                            error!(error = %e, "Failed to send Aura job");
                            response_tx.send(Err(e)).ok();
                            continue;
                        }
                        ramp_started = true;
                        status.write().unwrap().is_active = true;
                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::ReplaceTask { new_task, response_tx } => {
                        debug!(job = %new_task.template.id, "Replacing work");

                        if !chain_initialized {
                            trace!("Initializing Aura chain on first assignment.");
                            match chain::bring_up_chain(&mut reader, &mut writer, &config.chain).await {
                                Ok(found) => {
                                    chips = found;
                                    chain_initialized = true;
                                }
                                Err(e) => {
                                    error!(error = %e, "Aura chain bring-up failed");
                                    response_tx.send(Err(e)).ok();
                                    continue;
                                }
                            }
                        }

                        // Old work invalidated: only the new task's shares count.
                        let old_task = current_task.replace(new_task.clone());
                        if let Err(e) = dispatch_job(&mut writer, &chips, &mut slot, &mut job_id, &new_task).await {
                            error!(error = %e, "Failed to send Aura job");
                            response_tx.send(Err(e)).ok();
                            continue;
                        }
                        ramp_started = true;
                        status.write().unwrap().is_active = true;
                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::GoIdle { response_tx } => {
                        debug!("Going idle");
                        let old_task = current_task.take();
                        status.write().unwrap().is_active = false;
                        response_tx.send(Ok(old_task)).ok();
                    }

                    ThreadCommand::Shutdown => {
                        info!("Shutdown command received");
                        break;
                    }
                }
            }

            // Raw bytes from the chip bus.
            read = reader.read(&mut buf) => {
                match read {
                    Ok(0) => {
                        trace!("EOF on Aura data stream");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(n) => {
                        rx_buf.extend_from_slice(&buf[..n]);
                        let (responses, hits) = chain::drain_frames(&mut rx_buf);
                        for response in responses {
                            if !telemetry.record(response) {
                                trace!(
                                    chip = format!("0x{:02x}", response.chip),
                                    reg = format!("0x{:02x}", response.reg),
                                    "Unhandled Aura response"
                                );
                            }
                        }
                        for hit in hits {
                            if let Err(e) = handle_hit(hit, &current_task, &status).await {
                                warn!(error = %e, "Failed to process Aura hit");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Aura data read error");
                        status.write().unwrap().hardware_errors += 1;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }

            // DVFS heartbeat.
            _ = heartbeat_tick.tick(), if chain_initialized => {
                if let Err(e) = dvfs::heartbeat(&mut writer).await {
                    warn!(error = %e, "DVFS heartbeat failed");
                }
            }

            // Hit polling (only while work is live).
            _ = hit_poll_tick.tick(), if chain_initialized && current_task.is_some() => {
                for &chip in &chips {
                    if let Err(e) = chain::hit_poll(&mut writer, chip).await {
                        warn!(error = %e, chip = format!("0x{chip:02x}"), "Hit poll failed");
                    }
                }
            }

            // Telemetry: read counters for every chip, then emit deltas.
            _ = telemetry_tick.tick(), if chain_initialized => {
                for &chip in &chips {
                    for register in super::telemetry::TELEMETRY_REGS {
                        if let Err(e) = chain::read_reg(&mut writer, chip, register, chain::TELEMETRY_LCMD).await {
                            warn!(error = %e, chip = format!("0x{chip:02x}"), "Telemetry read failed");
                            break;
                        }
                    }
                }
                let samples = telemetry.poll(Instant::now());
                let total_ghs: f64 = samples.iter().map(|s| s.hashrate_ghs).sum();
                let snapshot = {
                    let mut s = status.write().unwrap();
                    s.hashrate = HashRate::from_gigahashes(total_ghs);
                    s.clone()
                };
                if event_tx
                    .send(HashThreadEvent::StatusUpdate(snapshot))
                    .await
                    .is_err()
                {
                    debug!("Event channel closed during status update");
                }
                if !samples.is_empty() {
                    let worst = samples
                        .iter()
                        .min_by(|a, b| a.clock_mhz.total_cmp(&b.clock_mhz))
                        .unwrap();
                    debug!(
                        chips = samples.len(),
                        total_ghs = format!("{total_ghs:.1}"),
                        min_clock_mhz = format!("{:.1}", worst.clock_mhz),
                        "Aura telemetry"
                    );
                }
            }

            // ntime rolling.
            _ = ntime_ticker.tick(), if current_task.is_some() => {
                let task = current_task.as_mut().unwrap();
                task.ntime = task.ntime.wrapping_add(1);
                if let Err(e) = dispatch_job(&mut writer, &chips, &mut slot, &mut job_id, task).await {
                    error!(error = %e, "Failed to send ntime-rolled Aura job");
                }
            }

            // Frequency ramp (only after pool work is live).
            _ = ramp_tick.tick(), if chain_initialized && ramp_started && current_task.is_some() && !ramp.done() => {
                if let Some(n) = ramp.next_step()
                    && let Err(e) = dvfs::write_ramp_step(&mut writer, n).await
                {
                    error!(error = %e, pll_n = n, "Ramp step failed");
                }
            }
        }
    }

    debug!("Aura thread actor exiting");
}
