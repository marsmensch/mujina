//! Apollo III board composition (SG4).
//!
//! Wires the Aura ASIC chain (over `/dev/ttyS4`) to the board power/thermal
//! envelope: GPIO bring-up, rail-power watchdog, fan PI loop, board
//! temperature and PSU telemetry.
//!
//! Bring-up follows the vendor-proven contract
//! (`docs/apollo-iii-boot-contract.md` §5): gpio148 high (heartbeat clock)
//! -> gpio115 reset pulse `0 -> 1` -> discovery sweep. Only those two GPIOs
//! are touched before discovery. The rail-power hold (gpio100), the
//! thermal-trip input (gpio138) and the PSU baseline are driven only after
//! the chain is live: the thread's post-bring-up hooks ([`dvfs::psu_hold`]
//! and the [`BaudSwitch`]) run inside the Aura actor right after discovery,
//! and the board monitor's watchdog gates on the same `chain_ready` latch.
//!
//! The board targets Linux (sysfs GPIO/PWM, i2c-dev, serial). On other
//! platforms the factory reports that clearly; all board logic is exercised
//! by unit tests against injected fakes and temp-dir mock sysfs trees.
//!
//! On non-Linux platforms the board logic below is only reachable from
//! `#[cfg(test)]`, so dead-code analysis cannot see its uses (the Linux
//! factory is compiled out). The `dead_code` lint is therefore relaxed
//! outside Linux; on Linux it runs in full.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::{
    api_client::types::{
        BoardTelemetry, Fan, PowerMeasurement, TemperatureSensor, ThreadTelemetry,
    },
    asic::{
        aura::{chain::PLL_RAMP_END, chain::PLL_RAMP_START, dvfs},
        hash_thread::{HashThreadStatus, ThreadRemovalSignal},
    },
    hw_trait::{self, GpioPin, PinMode, PinValue, i2c::I2c},
    peripheral::{pwm_sysfs::PwmSysfs, sic450::Sic450},
    tracing::prelude::*,
    types::Temperature,
};

use super::{BackplaneConnector, VirtualBoardDescriptor};

// Register this virtual board with the inventory system.
inventory::submit! {
    VirtualBoardDescriptor {
        device_type: "apollo_iii",
        name: "Apollo III",
        create_fn: || Box::pin(create_apollo_board()),
    }
}

/// Default serial device for the Aura ASIC link.
pub const DEFAULT_SERIAL_PORT: &str = "/dev/ttyS4";
/// Default discovery baud rate (8-N-1).
pub const DEFAULT_BAUD_INIT: u32 = 115_200;
/// Default mining baud rate, switched to after discovery.
pub const DEFAULT_BAUD_MINING: u32 = 921_600;
/// Default hashrate target in TH/s (full rate across 21 chips).
pub const DEFAULT_HASHRATE_TARGET_TH: f64 = 12.1;
/// Default expected number of Aura chips on the chain.
pub const DEFAULT_EXPECTED_CHIPS: usize = 21;

/// GPIO heartbeat-clock line (held high at bring-up).
const GPIO_HEARTBEAT: u8 = 148;
/// GPIO ASIC reset line (pulsed `0 -> 1` at bring-up).
const GPIO_RESET: u8 = 115;
/// GPIO ASIC rail power: `active_low = 0`, `value = 1` is ON. The monitor
/// holds it high and dips it ~100 ms every 1-4 s as the power-MCU watchdog
/// kick.
const GPIO_RAIL_POWER: u8 = 100;
/// GPIO thermal-trip input (fault -> emergency stop).
const GPIO_THERMAL_TRIP: u8 = 138;

/// I2C bus for the SIC450 PMBus / board temperature sensor.
const APOLLO_I2C_BUS: u8 = 3;

/// Fan PWM channel (`pwmchip0/pwm0`).
const FAN_PWM_CHIP: u32 = 0;
const FAN_PWM_CHANNEL: u32 = 0;
const FAN_PWM_PERIOD_NS: u64 = 40_000;
/// Safe startup fan duty percent, applied before the PI loop takes over.
const FAN_SAFE_DUTY_PERCENT: u8 = 40;
/// Tachometer sample window for the startup RPM check.
const FAN_STARTUP_SAMPLE_WINDOW: Duration = Duration::from_secs(1);
/// Tachometer sample window used by the monitor loop.
const FAN_RPM_WINDOW: Duration = Duration::from_secs(1);

/// PSU PWM channel (`pwmchip1/pwm0`); duty drives the SIC450 output
/// voltage. The thread's ramp climbs the duty via [`dvfs::psu_voltage_step`].
const PSU_PWM_CHIP: u32 = 1;
const PSU_PWM_CHANNEL: u32 = 0;
const PSU_PWM_PERIOD_NS: u64 = 40_000;

/// Vendor reset-pulse width at bring-up.
const RESET_PULSE_HOLD: Duration = Duration::from_millis(650);

/// Monitor tick cadence.
const MONITOR_TICK_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive thermal-trip readings before the emergency stop.
const THERMAL_TRIP_LIMIT: u32 = 3;

/// Apollo III board configuration, parsed from environment variables.
///
/// The board is enabled by the *presence* of `MUJINA_APOLLO_SERIAL` (an
/// empty value falls back to [`DEFAULT_SERIAL_PORT`]); the other variables
/// tune the link and the hashrate target.
#[derive(Debug, Clone)]
pub struct ApolloBoardConfig {
    /// Serial device path for the Aura ASIC link.
    pub serial_port: String,
    /// Discovery baud rate (8-N-1).
    pub baud_init: u32,
    /// Mining baud rate, switched to after discovery.
    pub baud_mining: u32,
    /// Hashrate target in TH/s; the ramp stops at the PLL N for this
    /// target (12.1 TH/s = PLL 491 = full rate).
    pub hashrate_target_th: f64,
    /// Expected number of Aura chips on the chain.
    pub expected_chips: usize,
}

impl ApolloBoardConfig {
    /// Parse configuration from environment variables.
    ///
    /// Returns `Some(config)` if `MUJINA_APOLLO_SERIAL` is set, `None`
    /// otherwise.
    ///
    /// # Environment Variables
    ///
    /// - `MUJINA_APOLLO_SERIAL`: serial device (presence enables the board)
    /// - `MUJINA_APOLLO_BAUD_INIT`: discovery baud (default 115200)
    /// - `MUJINA_APOLLO_BAUD_MINING`: mining baud (default 921600)
    /// - `MUJINA_APOLLO_MODE`: hashrate target TH/s (default 12.1)
    /// - `MUJINA_APOLLO_EXPECTED_CHIPS`: expected chain size (default 21)
    pub fn from_env() -> Option<Self> {
        let serial_port = std::env::var("MUJINA_APOLLO_SERIAL").ok()?;
        let serial_port = if serial_port.trim().is_empty() {
            DEFAULT_SERIAL_PORT.to_string()
        } else {
            serial_port
        };
        Some(Self {
            serial_port,
            baud_init: env_u32("MUJINA_APOLLO_BAUD_INIT").unwrap_or(DEFAULT_BAUD_INIT),
            baud_mining: env_u32("MUJINA_APOLLO_BAUD_MINING").unwrap_or(DEFAULT_BAUD_MINING),
            hashrate_target_th: env_f64("MUJINA_APOLLO_MODE").unwrap_or(DEFAULT_HASHRATE_TARGET_TH),
            expected_chips: env_usize("MUJINA_APOLLO_EXPECTED_CHIPS")
                .unwrap_or(DEFAULT_EXPECTED_CHIPS),
        })
    }

    /// PLL N at which the frequency ramp stops for this hashrate target.
    ///
    /// Full rate (12.1 TH/s across 21 chips) maps to PLL N 491
    /// (`Fhash = 5*N/4 = 613.75 MHz`); lower targets stop the ramp early at
    /// the corresponding N, clamped to the ramp's operating range.
    pub fn pll_max(&self) -> u32 {
        let frac = (self.hashrate_target_th / DEFAULT_HASHRATE_TARGET_TH).clamp(0.0, 1.0);
        let n = (frac * f64::from(PLL_RAMP_END)).round() as u32;
        n.clamp(PLL_RAMP_START, PLL_RAMP_END)
    }
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

/// Create the Apollo III board.
///
/// Opens the ASIC serial link at the discovery baud, performs the
/// vendor-proven GPIO bring-up (gpio148 high -> gpio115 reset pulse),
/// configures the fan at a safe duty and verifies the tachometer, then
/// hands the Aura chain thread to the scheduler. Discovery itself is lazy
/// (runs on the first job assignment inside the thread); the post-discovery
/// board hooks (baud switch, PSU baseline hold) fire from inside the thread
/// actor at that point.
#[cfg(target_os = "linux")]
async fn create_apollo_board() -> Result<BackplaneConnector> {
    use anyhow::Context as _;
    use std::path::PathBuf;

    use crate::{
        asic::aura::{AuraConfig, AuraThread, BaudSwitch, ChainConfig},
        hw_trait::{
            Gpio, LinuxI2c,
            fan_tach::{DEFAULT_CHIP_PATH, DEFAULT_LINE_OFFSET, DEFAULT_PULSES_PER_REV, FanTach},
            sysfs_gpio::SysfsGpio,
        },
        peripheral::sic450::DEFAULT_ADDRESS,
        transport::serial::SerialStream,
    };

    use super::BoardInfo;

    let config = ApolloBoardConfig::from_env()
        .ok_or_else(|| anyhow!("Apollo III not configured (MUJINA_APOLLO_SERIAL not set)"))?;

    // Serial link at the discovery baud; the thread switches to the mining
    // baud via its post-discovery hook.
    let stream = SerialStream::new(&config.serial_port, config.baud_init)
        .context("failed to open Apollo III serial port")?;
    let (reader, writer, serial_control) = stream.split();

    // Vendor-proven GPIO bring-up: heartbeat clock high, then reset pulse
    // 0 -> 1. Nothing else touches hardware before discovery.
    let mut gpio = SysfsGpio::new();
    let mut gpio148 = gpio.pin(GPIO_HEARTBEAT).await?;
    let mut gpio115 = gpio.pin(GPIO_RESET).await?;
    bring_up_gpios(&mut gpio148, &mut gpio115, RESET_PULSE_HOLD).await?;

    // Fan: safe startup duty, then verify the tachometer before ASIC work.
    let mut fan = PwmSysfs::new(FAN_PWM_CHIP, FAN_PWM_CHANNEL);
    fan.export().await.context("failed to export fan PWM")?;
    fan.set_period_ns(FAN_PWM_PERIOD_NS).await?;
    fan.set_duty_percent(f32::from(FAN_SAFE_DUTY_PERCENT))
        .await?;
    fan.enable().await?;
    let mut tach = FanTach::new(
        PathBuf::from(DEFAULT_CHIP_PATH),
        DEFAULT_LINE_OFFSET,
        DEFAULT_PULSES_PER_REV,
    );
    tach.open()?;
    let startup_rpm = tach.sample(FAN_STARTUP_SAMPLE_WINDOW).await?;
    if startup_rpm <= 0.0 {
        bail!("fan tachometer read {startup_rpm:.0} RPM at startup; refusing ASIC work");
    }
    info!(
        rpm = format!("{startup_rpm:.0}"),
        "Fan startup RPM check passed"
    );

    // PSU PWM: exported and given its period, but NOT enabled and NOT given
    // a duty here — per the boot contract the PSU is not touched before
    // discovery. The thread's post-bring-up hook ([`dvfs::psu_hold`]) holds
    // the baseline 5.0 V once the chain is live and the ramp climbs it.
    let mut psu = PwmSysfs::new(PSU_PWM_CHIP, PSU_PWM_CHANNEL);
    psu.export().await.context("failed to export PSU PWM")?;
    psu.set_period_ns(PSU_PWM_PERIOD_NS).await?;

    // Board temperature + SIC450 telemetry on i2c-3 @ 0x49.
    let sic450 = Sic450::new(
        Box::new(LinuxI2c::new(APOLLO_I2C_BUS)?) as Box<dyn I2c>,
        DEFAULT_ADDRESS,
    );

    // The post-discovery hook: flip the monitor's chain_ready latch and
    // switch the serial link to the mining baud. Runs exactly once, inside
    // the thread actor, after discovery and before any job is dispatched.
    let chain_ready = Arc::new(AtomicBool::new(false));
    let baud_switch = BaudSwitch::new({
        let control = serial_control.clone();
        let chain_ready = Arc::clone(&chain_ready);
        let mining_baud = config.baud_mining;
        move || {
            chain_ready.store(true, Ordering::Release);
            if let Err(e) = control.set_baud_rate(mining_baud) {
                error!(
                    error = %e,
                    baud = mining_baud,
                    "Failed to switch serial link to mining baud"
                );
            }
        }
    });

    let aura_config = AuraConfig {
        chain: ChainConfig {
            expected_chips: config.expected_chips,
            ..ChainConfig::default()
        },
        expected_hashrate_th: config.hashrate_target_th,
        ramp_max_pll: config.pll_max(),
        psu: Some(psu.clone()),
        baud_switch: Some(baud_switch),
        ..AuraConfig::default()
    };

    let (thread_shutdown_tx, thread_shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);
    let thread = AuraThread::new(
        "Apollo-III-chain".to_string(),
        reader,
        writer,
        aura_config,
        thread_shutdown_rx,
    );
    let thread_status = thread.status_handle();

    // Rail power and thermal-trip pins: direction setup only. The rail
    // VALUE is not touched until the monitor sees chain_ready.
    let mut gpio100 = gpio.pin(GPIO_RAIL_POWER).await?;
    gpio100.set_mode(PinMode::Output).await?;
    let mut gpio138 = gpio.pin(GPIO_THERMAL_TRIP).await?;
    gpio138.set_mode(PinMode::Input).await?;

    let board_name = "apollo-iii".to_string();
    let initial_state = BoardTelemetry {
        name: board_name.clone(),
        model: "FutureBit Apollo III".into(),
        serial: Some(board_name.clone()),
        ..Default::default()
    };
    let (telemetry_tx, telemetry_rx) = watch::channel(initial_state);

    let monitor = ApolloBoardMonitor {
        gpio100: Box::new(gpio100),
        gpio115: Box::new(gpio115),
        gpio138: Box::new(gpio138),
        fan,
        psu,
        fan_rpm: Box::new(tach),
        sic450,
        thread_status,
        thread_shutdown: thread_shutdown_tx,
        board_name,
        chain_ready,
        watchdog: WatchdogTiming::default(),
        next_dip: Instant::now(),
        tick_interval: MONITOR_TICK_INTERVAL,
        rpm_window: FAN_RPM_WINDOW,
        fan_pid: FanPid::default(),
        fan_duty_percent: None,
        thermal_trip_count: 0,
        last_tick: Instant::now(),
    };

    let cancel = CancellationToken::new();
    let monitor_handle = tokio::spawn(monitor.run_monitor(telemetry_tx, cancel.clone()));

    let shutdown = Box::pin(async move {
        cancel.cancel();
        let _ = monitor_handle.await;
    });

    let info = BoardInfo {
        model: "FutureBit Apollo III".to_string(),
        firmware_version: None,
        serial_number: Some(board_name.clone()),
    };

    Ok(BackplaneConnector {
        info,
        threads: vec![Box::new(thread)],
        telemetry_rx,
        shutdown: Some(shutdown),
    })
}

/// The Apollo board only exists on Linux (serial + sysfs backends).
#[cfg(not(target_os = "linux"))]
async fn create_apollo_board() -> Result<BackplaneConnector> {
    anyhow::bail!("Apollo III board requires Linux serial/sysfs backends")
}

/// Vendor-proven GPIO bring-up: heartbeat clock high, then ASIC reset
/// pulse `0 -> 1`. These are the ONLY hardware touches before discovery.
async fn bring_up_gpios(
    gpio148: &mut dyn GpioPin,
    gpio115: &mut dyn GpioPin,
    pulse_hold: Duration,
) -> Result<()> {
    gpio148
        .set_mode(PinMode::Output)
        .await
        .map_err(|e| anyhow!("failed to configure heartbeat GPIO: {e}"))?;
    gpio148
        .write(PinValue::High)
        .await
        .map_err(|e| anyhow!("failed to raise heartbeat GPIO: {e}"))?;
    gpio115
        .set_mode(PinMode::Output)
        .await
        .map_err(|e| anyhow!("failed to configure reset GPIO: {e}"))?;
    gpio115
        .write(PinValue::Low)
        .await
        .map_err(|e| anyhow!("failed to assert ASIC reset: {e}"))?;
    tokio::time::sleep(pulse_hold).await;
    gpio115
        .write(PinValue::High)
        .await
        .map_err(|e| anyhow!("failed to release ASIC reset: {e}"))?;
    Ok(())
}

/// Fan tachometer abstraction so the monitor loop is testable without the
/// Linux GPIO chardev.
#[async_trait]
pub trait FanRpm: Send {
    /// Measure RPM over `window`.
    async fn sample(&mut self, window: Duration) -> hw_trait::Result<f64>;
}

#[cfg(target_os = "linux")]
#[async_trait]
impl FanRpm for FanTach {
    async fn sample(&mut self, window: Duration) -> hw_trait::Result<f64> {
        FanTach::sample(self, window).await
    }
}

/// `I2c` through a boxed trait object, so the monitor can own a
/// `Sic450<Box<dyn I2c>>` over either the Linux backend or a test fake.
#[async_trait]
impl I2c for Box<dyn I2c> {
    async fn write(&mut self, addr: u8, data: &[u8]) -> hw_trait::Result<()> {
        (**self).write(addr, data).await
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> hw_trait::Result<()> {
        (**self).read(addr, buffer).await
    }

    async fn write_read(
        &mut self,
        addr: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> hw_trait::Result<()> {
        (**self).write_read(addr, write, read).await
    }

    async fn set_frequency(&mut self, hz: u32) -> hw_trait::Result<()> {
        (**self).set_frequency(hz).await
    }
}

/// Per-tick watchdog timing for the ASIC rail-power kick.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogTiming {
    /// Interval between dips (vendor range: 1-4 s).
    pub period: Duration,
    /// Duration the rail is dipped low (~100 ms).
    pub dip: Duration,
}

impl Default for WatchdogTiming {
    fn default() -> Self {
        Self {
            period: Duration::from_secs(2),
            dip: Duration::from_millis(100),
        }
    }
}

/// Simple PI fan controller against board temperature.
///
/// Constants are tunable; the locked ground truth is only that the fan is
/// closed-loop controlled against temperature (the exact plant response is
/// not yet characterized — G6).
#[derive(Debug, Clone, Copy)]
pub struct FanPid {
    /// Proportional gain (duty percent per degree of error).
    pub kp: f32,
    /// Integral gain (duty percent per degree-second).
    pub ki: f32,
    /// Temperature setpoint in degrees Celsius.
    pub setpoint_c: f32,
    /// Minimum fan duty percent (stays on even when cold).
    pub min_duty_percent: u8,
    /// Maximum fan duty percent.
    pub max_duty_percent: u8,
    /// Accumulated integral term (duty percent), clamped to `[0, 100]`.
    integral: f32,
}

impl Default for FanPid {
    fn default() -> Self {
        Self {
            kp: 4.0,
            ki: 0.2,
            setpoint_c: 50.0,
            min_duty_percent: 20,
            max_duty_percent: 100,
            integral: 0.0,
        }
    }
}

impl FanPid {
    /// One PI update: `duty = clamp(kp * error + integral, min, max)`.
    ///
    /// The integral only accumulates upward (never winds down below zero),
    /// so the fan never drops below `min_duty_percent` while warm.
    pub fn update(&mut self, temp_c: f32, dt: f32) -> u8 {
        let error = temp_c - self.setpoint_c;
        self.integral = (self.integral + self.ki * error * dt).clamp(0.0, 100.0);
        let duty = (self.kp * error + self.integral).clamp(
            f32::from(self.min_duty_percent),
            f32::from(self.max_duty_percent),
        );
        duty.round() as u8
    }
}

/// Decode an LM75/TMP-style 1-byte temperature read (register 0x00, MSB) as
/// degrees Celsius: two's complement with 0.5 C per LSB.
///
/// Assumption (G6): the Apollo III board temperature register follows the
/// LM75 MSB-byte encoding. Verify the scaling on device.
pub fn lm75_temp_c(raw: u8) -> f32 {
    (raw as i8) as f32 * 0.5
}

/// PSU output voltage for a PWM duty (ns): linear between the vendor
/// anchors `duty 20000 -> 5.0 V` and `duty 36000 -> ~6.1 V`. Duties outside
/// the anchor range clamp to the nearest anchor.
pub fn psu_voltage_from_duty_ns(duty_ns: u64) -> f32 {
    let duty = duty_ns.clamp(dvfs::PSU_DUTY_BASELINE_NS, dvfs::PSU_DUTY_FULL_NS);
    5.0 + (duty - dvfs::PSU_DUTY_BASELINE_NS) as f32 * (6.1 - 5.0)
        / (dvfs::PSU_DUTY_FULL_NS - dvfs::PSU_DUTY_BASELINE_NS) as f32
}

/// Internal state owned by the board monitor task.
///
/// The factory assembles this and moves it into `run_monitor()`. Hardware
/// touches are gated on `chain_ready` (set by the thread's post-discovery
/// baud-switch hook), so nothing here disturbs discovery.
struct ApolloBoardMonitor {
    /// ASIC rail power (held high + watchdog dips once the chain is live).
    gpio100: Box<dyn GpioPin>,
    /// ASIC reset (asserted on shutdown / emergency stop).
    gpio115: Box<dyn GpioPin>,
    /// Thermal-trip input (fault -> emergency stop).
    gpio138: Box<dyn GpioPin>,
    /// Fan PWM channel (`pwmchip0/pwm0`).
    fan: PwmSysfs,
    /// PSU PWM channel (`pwmchip1/pwm0`); duty read back for telemetry.
    psu: PwmSysfs,
    /// Fan tachometer.
    fan_rpm: Box<dyn FanRpm>,
    /// Board temperature + SIC450 telemetry (i2c-3 @ 0x49).
    sic450: Sic450<Box<dyn I2c>>,
    /// Shared Aura thread status (hashrate/activity for telemetry).
    thread_status: Arc<RwLock<HashThreadStatus>>,
    /// Watch channel signalling the Aura thread to shut down.
    thread_shutdown: watch::Sender<ThreadRemovalSignal>,
    /// Board id / telemetry name.
    board_name: String,
    /// Latched by the thread after discovery; gates rail/PSU control.
    chain_ready: Arc<AtomicBool>,
    /// Rail watchdog timing.
    watchdog: WatchdogTiming,
    /// When the next watchdog dip is due.
    next_dip: Instant,
    /// Monitor tick cadence.
    tick_interval: Duration,
    /// Fan tachometer sample window.
    rpm_window: Duration,
    /// Fan PI controller.
    fan_pid: FanPid,
    /// Last commanded fan duty (for telemetry).
    fan_duty_percent: Option<u8>,
    /// Consecutive thermal-trip readings (debounce).
    thermal_trip_count: u32,
    /// Last tick instant (PI dt).
    last_tick: Instant,
}

impl ApolloBoardMonitor {
    async fn run_monitor(
        mut self,
        telemetry_tx: watch::Sender<BoardTelemetry>,
        cancel: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(self.tick_interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.monitor_tick(&telemetry_tx).await {
                        error!(error = %e, "Apollo III board monitor failed; shutting down");
                        self.shutdown(ThreadRemovalSignal::HardwareFault {
                            description: e.to_string(),
                        }).await;
                        return;
                    }
                }
                _ = cancel.cancelled() => {
                    self.shutdown(ThreadRemovalSignal::Shutdown).await;
                    return;
                }
            }
        }
    }

    /// Run one monitoring cycle.
    ///
    /// - Rail power: once `chain_ready` is set, hold gpio100 high and dip it
    ///   low for [`WatchdogTiming::dip`] every [`WatchdogTiming::period`]
    ///   (power-MCU watchdog kick).
    /// - Thermal trip: a sustained `High` on gpio138 escalates to an
    ///   emergency stop (returns `Err`).
    /// - Fan: PI loop against board temperature, then write the duty.
    /// - Telemetry: fan RPM, board temp, PSU voltage (from the commanded
    ///   duty readback) and the thread's hashrate/activity.
    async fn monitor_tick(&mut self, tx: &watch::Sender<BoardTelemetry>) -> Result<()> {
        if self.chain_ready.load(Ordering::Acquire) {
            if let Err(e) = self.gpio100.write(PinValue::High).await {
                warn!(error = %e, "Failed to hold ASIC rail power on");
            }
            if self.next_dip.elapsed() >= self.watchdog.period {
                self.gpio100
                    .write(PinValue::Low)
                    .await
                    .map_err(|e| anyhow!("failed to dip ASIC rail power: {e}"))?;
                tokio::time::sleep(self.watchdog.dip).await;
                self.gpio100
                    .write(PinValue::High)
                    .await
                    .map_err(|e| anyhow!("failed to restore ASIC rail power: {e}"))?;
                self.next_dip = Instant::now();
            }
        }

        // Thermal trip input -> emergency stop (debounced).
        let trip = self
            .gpio138
            .read()
            .await
            .map_err(|e| anyhow!("failed to read thermal trip GPIO: {e}"))?
            == PinValue::High;
        if trip {
            self.thermal_trip_count += 1;
            warn!(
                count = self.thermal_trip_count,
                "Apollo III thermal trip asserted"
            );
            if self.thermal_trip_count >= THERMAL_TRIP_LIMIT {
                bail!(
                    "thermal trip sustained for {} readings",
                    self.thermal_trip_count
                );
            }
        } else {
            self.thermal_trip_count = 0;
        }

        // Fan PI loop against board temperature.
        let board_temp = match self.sic450.read_board_temp().await {
            Ok(raw) => Some(lm75_temp_c(raw)),
            Err(e) => {
                warn!(error = ?e, "Board temperature read failed");
                None
            }
        };
        if let Some(temp_c) = board_temp {
            let dt = self.last_tick.elapsed().as_secs_f32().max(0.1);
            self.last_tick = Instant::now();
            let duty = self.fan_pid.update(temp_c, dt);
            if let Err(e) = self.fan.set_duty_percent(f32::from(duty)).await {
                warn!(error = %e, "Failed to write fan duty");
            } else {
                self.fan_duty_percent = Some(duty);
            }
        }

        // Telemetry.
        let fan_rpm = self.fan_rpm.sample(self.rpm_window).await.ok();
        let psu_voltage_v = match self.psu.get_duty_ns().await {
            Ok(duty) => Some(psu_voltage_from_duty_ns(duty)),
            Err(e) => {
                trace!(error = %e, "PSU duty readback failed");
                None
            }
        };
        let status = self.thread_status.read().unwrap().clone();
        let _ = tx.send(BoardTelemetry {
            name: self.board_name.clone(),
            model: "FutureBit Apollo III".into(),
            serial: Some(self.board_name.clone()),
            fans: vec![Fan {
                name: "fan".into(),
                rpm: fan_rpm.map(|r| r.round() as u32),
                percent: self.fan_duty_percent,
                target_percent: self.fan_duty_percent,
            }],
            temperatures: vec![TemperatureSensor {
                name: "board".into(),
                temperature: board_temp.map(Temperature::from_celsius),
            }],
            powers: vec![PowerMeasurement {
                name: "psu".into(),
                voltage_v: psu_voltage_v,
                current_a: None,
                power_w: None,
            }],
            threads: vec![ThreadTelemetry {
                name: "aura-chain".into(),
                hashrate: status.hashrate.0,
                is_active: status.is_active,
            }],
        });

        Ok(())
    }

    /// Board shutdown: signal the thread, reset the ASIC, drop the rail and
    /// stop the fan. Matches the vendor's own SIGTERM shutdown behavior.
    async fn shutdown(&mut self, reason: ThreadRemovalSignal) {
        if let Err(e) = self.thread_shutdown.send(reason) {
            warn!("Failed to signal thread shutdown: {}", e);
        }
        if let Err(e) = self.gpio115.write(PinValue::Low).await {
            warn!(error = %e, "Failed to assert ASIC reset on shutdown");
        }
        if let Err(e) = self.gpio100.write(PinValue::Low).await {
            warn!(error = %e, "Failed to drop ASIC rail power on shutdown");
        }
        if let Err(e) = self.fan.disable().await {
            warn!(error = %e, "Failed to stop fan on shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::PinValue;
    use crate::peripheral::sic450::DEFAULT_ADDRESS;
    use crate::types::HashRate;
    use serial_test::serial;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    // ------------------------------------------------------------------
    // Config parsing
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn config_absent_without_serial_var() {
        // SAFETY: Test runs serially, no concurrent env access.
        unsafe { std::env::remove_var("MUJINA_APOLLO_SERIAL") };
        assert!(ApolloBoardConfig::from_env().is_none());
    }

    #[test]
    #[serial]
    fn config_defaults_when_only_serial_set() {
        // SAFETY: Test runs serially, no concurrent env access.
        unsafe {
            std::env::set_var("MUJINA_APOLLO_SERIAL", "/dev/ttyS4");
            std::env::remove_var("MUJINA_APOLLO_BAUD_INIT");
            std::env::remove_var("MUJINA_APOLLO_BAUD_MINING");
            std::env::remove_var("MUJINA_APOLLO_MODE");
            std::env::remove_var("MUJINA_APOLLO_EXPECTED_CHIPS");
        }
        let config = ApolloBoardConfig::from_env().expect("configured");
        assert_eq!(config.serial_port, DEFAULT_SERIAL_PORT);
        assert_eq!(config.baud_init, DEFAULT_BAUD_INIT);
        assert_eq!(config.baud_mining, DEFAULT_BAUD_MINING);
        assert!((config.hashrate_target_th - DEFAULT_HASHRATE_TARGET_TH).abs() < 1e-9);
        assert_eq!(config.expected_chips, DEFAULT_EXPECTED_CHIPS);
    }

    #[test]
    #[serial]
    fn config_parses_explicit_values() {
        // SAFETY: Test runs serially, no concurrent env access.
        unsafe {
            std::env::set_var("MUJINA_APOLLO_SERIAL", "/dev/ttyUSB9");
            std::env::set_var("MUJINA_APOLLO_BAUD_INIT", "9600");
            std::env::set_var("MUJINA_APOLLO_BAUD_MINING", "1000000");
            std::env::set_var("MUJINA_APOLLO_MODE", "6.0");
            std::env::set_var("MUJINA_APOLLO_EXPECTED_CHIPS", "18");
        }
        let config = ApolloBoardConfig::from_env().expect("configured");
        assert_eq!(config.serial_port, "/dev/ttyUSB9");
        assert_eq!(config.baud_init, 9600);
        assert_eq!(config.baud_mining, 1_000_000);
        assert!((config.hashrate_target_th - 6.0).abs() < 1e-9);
        assert_eq!(config.expected_chips, 18);
    }

    #[test]
    #[serial]
    fn empty_serial_var_falls_back_to_default() {
        // SAFETY: Test runs serially, no concurrent env access.
        unsafe {
            std::env::set_var("MUJINA_APOLLO_SERIAL", "");
            std::env::remove_var("MUJINA_APOLLO_MODE");
        }
        let config = ApolloBoardConfig::from_env().expect("configured");
        assert_eq!(config.serial_port, DEFAULT_SERIAL_PORT);
    }

    #[test]
    fn pll_max_maps_target_to_ramp_stop() {
        let base = ApolloBoardConfig {
            serial_port: DEFAULT_SERIAL_PORT.into(),
            baud_init: DEFAULT_BAUD_INIT,
            baud_mining: DEFAULT_BAUD_MINING,
            hashrate_target_th: DEFAULT_HASHRATE_TARGET_TH,
            expected_chips: DEFAULT_EXPECTED_CHIPS,
        };
        // Full rate: 12.1 TH/s -> PLL 491.
        assert_eq!(base.pll_max(), PLL_RAMP_END);
        // Half rate: 6.05 / 12.1 * 491 = 245.5 -> 246.
        let half = ApolloBoardConfig {
            hashrate_target_th: 6.05,
            ..base.clone()
        };
        assert_eq!(half.pll_max(), 246);
        // Zero and absurd targets clamp to the ramp bounds.
        let zero = ApolloBoardConfig {
            hashrate_target_th: 0.0,
            ..base.clone()
        };
        assert_eq!(zero.pll_max(), PLL_RAMP_START);
        let huge = ApolloBoardConfig {
            hashrate_target_th: 100.0,
            ..base
        };
        assert_eq!(huge.pll_max(), PLL_RAMP_END);
    }

    // ------------------------------------------------------------------
    // Pure helpers
    // ------------------------------------------------------------------

    #[test]
    fn lm75_temp_decode() {
        // 0x5A = 90 LSB * 0.5 = 45 C; 0x00 = 0 C; 0xFF = -1 * 0.5 = -0.5 C.
        assert!((lm75_temp_c(0x5A) - 45.0).abs() < 1e-6);
        assert!((lm75_temp_c(0x00) - 0.0).abs() < 1e-6);
        assert!((lm75_temp_c(0xFF) + 0.5).abs() < 1e-6);
    }

    #[test]
    fn psu_voltage_anchors() {
        assert!((psu_voltage_from_duty_ns(20_000) - 5.0).abs() < 1e-6);
        assert!((psu_voltage_from_duty_ns(36_000) - 6.1).abs() < 1e-6);
        // Midpoint: 5.0 + 0.55 = 5.55 V.
        assert!((psu_voltage_from_duty_ns(28_000) - 5.55).abs() < 1e-6);
        // Clamped outside the anchor range.
        assert!((psu_voltage_from_duty_ns(0) - 5.0).abs() < 1e-6);
        assert!((psu_voltage_from_duty_ns(1_000_000) - 6.1).abs() < 1e-6);
    }

    #[test]
    fn fan_pid_min_duty_when_cold() {
        let mut pid = FanPid::default();
        assert_eq!(pid.update(30.0, 1.0), pid.min_duty_percent);
    }

    #[test]
    fn fan_pid_ramps_up_with_heat_and_clamps() {
        let mut pid = FanPid::default();
        let mut last = 0u8;
        for temp in (50..=90).step_by(5) {
            let duty = pid.update(temp as f32, 1.0);
            assert!(
                duty >= last,
                "duty must not fall while hot: {duty} < {last}"
            );
            last = duty;
        }
        assert_eq!(last, 100);
    }

    #[test]
    fn fan_pid_integral_is_bounded() {
        let mut pid = FanPid::default();
        for _ in 0..1000 {
            let _ = pid.update(80.0, 10.0);
        }
        assert!(pid.integral <= 100.0);
    }

    // ------------------------------------------------------------------
    // Bring-up sequencing
    // ------------------------------------------------------------------

    /// In-memory GPIO pin recording every operation into a shared log.
    #[derive(Clone)]
    struct MockPin {
        name: &'static str,
        log: Arc<StdMutex<Vec<String>>>,
        read_value: Arc<StdMutex<PinValue>>,
    }

    impl MockPin {
        fn new(name: &'static str, log: Arc<StdMutex<Vec<String>>>) -> Self {
            Self {
                name,
                log,
                read_value: Arc::new(StdMutex::new(PinValue::Low)),
            }
        }

        fn record(&self, event: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{}", self.name, event));
        }

        fn set_read(&self, value: PinValue) {
            *self.read_value.lock().unwrap() = value;
        }

        fn events(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl GpioPin for MockPin {
        async fn set_mode(&mut self, mode: PinMode) -> hw_trait::Result<()> {
            let m = match mode {
                PinMode::Input => "in",
                PinMode::Output => "out",
            };
            self.record(m);
            Ok(())
        }

        async fn write(&mut self, value: PinValue) -> hw_trait::Result<()> {
            self.record(match value {
                PinValue::Low => "low",
                PinValue::High => "high",
            });
            Ok(())
        }

        async fn read(&mut self) -> hw_trait::Result<PinValue> {
            let value = *self.read_value.lock().unwrap();
            self.record(if value == PinValue::High {
                "read-high"
            } else {
                "read-low"
            });
            Ok(value)
        }
    }

    #[tokio::test]
    async fn bring_up_orders_heartbeat_then_reset_pulse() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut heartbeat = MockPin::new("148", log.clone());
        let mut reset = MockPin::new("115", log.clone());

        bring_up_gpios(&mut heartbeat, &mut reset, Duration::from_millis(1))
            .await
            .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            ["148:out", "148:high", "115:out", "115:low", "115:high"]
        );
    }

    // ------------------------------------------------------------------
    // Monitor loop
    // ------------------------------------------------------------------

    /// A fan tach fake returning a fixed RPM instantly.
    struct MockFanRpm {
        rpm: f64,
    }

    #[async_trait]
    impl FanRpm for MockFanRpm {
        async fn sample(&mut self, _window: Duration) -> hw_trait::Result<f64> {
            Ok(self.rpm)
        }
    }

    /// An I2C fake serving a fixed board-temperature byte.
    struct FakeI2c {
        temp_byte: u8,
    }

    #[async_trait]
    impl I2c for FakeI2c {
        async fn write(&mut self, _addr: u8, _data: &[u8]) -> hw_trait::Result<()> {
            Ok(())
        }

        async fn read(&mut self, _addr: u8, _buffer: &mut [u8]) -> hw_trait::Result<()> {
            Ok(())
        }

        async fn write_read(
            &mut self,
            _addr: u8,
            _write: &[u8],
            read: &mut [u8],
        ) -> hw_trait::Result<()> {
            read.fill(self.temp_byte);
            Ok(())
        }

        async fn set_frequency(&mut self, _hz: u32) -> hw_trait::Result<()> {
            Ok(())
        }
    }

    /// Create a fresh temp-dir mock sysfs PWM root.
    fn mock_pwm_root(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mujina-sg4-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a mock chip with one pre-exported channel.
    fn mock_pwm_chip(root: &Path, chip: u32, channel: u32, duty: &str) {
        let chip_dir = root.join(format!("pwmchip{chip}"));
        std::fs::create_dir_all(&chip_dir).unwrap();
        std::fs::write(chip_dir.join("export"), "").unwrap();
        let pwm_dir = chip_dir.join(format!("pwm{channel}"));
        std::fs::create_dir_all(&pwm_dir).unwrap();
        std::fs::write(pwm_dir.join("period"), "40000").unwrap();
        std::fs::write(pwm_dir.join("duty_cycle"), duty).unwrap();
        std::fs::write(pwm_dir.join("enable"), "0").unwrap();
    }

    /// Assemble a monitor over fakes + a mock sysfs tree.
    fn test_monitor(
        gpio100: MockPin,
        gpio115: MockPin,
        gpio138: MockPin,
        root: PathBuf,
        chain_ready: Arc<AtomicBool>,
        watchdog: WatchdogTiming,
        temp_byte: u8,
    ) -> ApolloBoardMonitor {
        let (shutdown_tx, _shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);
        ApolloBoardMonitor {
            gpio100: Box::new(gpio100),
            gpio115: Box::new(gpio115),
            gpio138: Box::new(gpio138),
            fan: PwmSysfs::with_root(root.clone(), 0, 0),
            psu: PwmSysfs::with_root(root, 1, 0),
            fan_rpm: Box::new(MockFanRpm { rpm: 2300.0 }),
            sic450: Sic450::new(
                Box::new(FakeI2c { temp_byte }) as Box<dyn I2c>,
                DEFAULT_ADDRESS,
            ),
            thread_status: Arc::new(RwLock::new(HashThreadStatus {
                hashrate: HashRate::from_terahashes(12.1),
                is_active: true,
                ..Default::default()
            })),
            thread_shutdown: shutdown_tx,
            board_name: "apollo-iii".into(),
            chain_ready,
            watchdog,
            next_dip: Instant::now(),
            tick_interval: Duration::from_secs(2),
            rpm_window: Duration::from_millis(1),
            fan_pid: FanPid::default(),
            fan_duty_percent: None,
            thermal_trip_count: 0,
            last_tick: Instant::now(),
        }
    }

    #[tokio::test]
    async fn monitor_does_not_touch_rail_before_chain_ready() {
        let rail = MockPin::new("100", Arc::new(StdMutex::new(Vec::new())));
        let reset = MockPin::new("115", Arc::new(StdMutex::new(Vec::new())));
        let trip = MockPin::new("138", Arc::new(StdMutex::new(Vec::new())));
        let root = mock_pwm_root("rail-gate");
        mock_pwm_chip(&root, 0, 0, "16000");
        mock_pwm_chip(&root, 1, 0, "20000");

        let mut monitor = test_monitor(
            rail.clone(),
            reset,
            trip,
            root,
            Arc::new(AtomicBool::new(false)),
            WatchdogTiming::default(),
            0x5A,
        );
        let (tx, _rx) = watch::channel(BoardTelemetry::default());
        monitor.monitor_tick(&tx).await.unwrap();
        assert!(
            rail.events().is_empty(),
            "no gpio100 touches before the chain is live: {:?}",
            rail.events()
        );
    }

    #[tokio::test]
    async fn monitor_holds_rail_and_dips_after_chain_ready() {
        let rail = MockPin::new("100", Arc::new(StdMutex::new(Vec::new())));
        let reset = MockPin::new("115", Arc::new(StdMutex::new(Vec::new())));
        let trip = MockPin::new("138", Arc::new(StdMutex::new(Vec::new())));
        let root = mock_pwm_root("rail-dip");
        mock_pwm_chip(&root, 0, 0, "16000");
        mock_pwm_chip(&root, 1, 0, "20000");

        let chain_ready = Arc::new(AtomicBool::new(false));
        let mut monitor = test_monitor(
            rail.clone(),
            reset,
            trip,
            root,
            chain_ready.clone(),
            WatchdogTiming {
                period: Duration::from_millis(1),
                dip: Duration::from_millis(1),
            },
            0x5A,
        );
        let (tx, _rx) = watch::channel(BoardTelemetry::default());

        // Before ready: no rail writes (covered by the gate test); after
        // ready with an overdue dip, one full dip cycle.
        chain_ready.store(true, Ordering::Release);
        monitor.next_dip = Instant::now() - Duration::from_secs(10);
        monitor.monitor_tick(&tx).await.unwrap();
        assert_eq!(rail.events(), ["100:high", "100:low", "100:high"]);
    }

    #[tokio::test]
    async fn thermal_trip_escalates_to_emergency_stop() {
        let rail = MockPin::new("100", Arc::new(StdMutex::new(Vec::new())));
        let reset = MockPin::new("115", Arc::new(StdMutex::new(Vec::new())));
        let trip = MockPin::new("138", Arc::new(StdMutex::new(Vec::new())));
        trip.set_read(PinValue::High);
        let root = mock_pwm_root("trip");
        mock_pwm_chip(&root, 0, 0, "16000");
        mock_pwm_chip(&root, 1, 0, "20000");

        let mut monitor = test_monitor(
            rail,
            reset,
            trip,
            root,
            Arc::new(AtomicBool::new(false)),
            WatchdogTiming::default(),
            0x5A,
        );
        let (tx, _rx) = watch::channel(BoardTelemetry::default());

        // Two trips are tolerated (debounce), the third escalates.
        for _ in 0..THERMAL_TRIP_LIMIT - 1 {
            monitor.monitor_tick(&tx).await.unwrap();
        }
        let err = monitor.monitor_tick(&tx).await.unwrap_err();
        assert!(
            err.to_string().contains("thermal trip"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn shutdown_resets_asic_and_drops_rail() {
        let rail = MockPin::new("100", Arc::new(StdMutex::new(Vec::new())));
        let reset = MockPin::new("115", Arc::new(StdMutex::new(Vec::new())));
        let trip = MockPin::new("138", Arc::new(StdMutex::new(Vec::new())));
        let root = mock_pwm_root("shutdown");
        mock_pwm_chip(&root, 0, 0, "16000");
        mock_pwm_chip(&root, 1, 0, "20000");

        let (shutdown_tx, shutdown_rx) = watch::channel(ThreadRemovalSignal::Running);
        let mut monitor = test_monitor(
            rail.clone(),
            reset.clone(),
            trip,
            root,
            Arc::new(AtomicBool::new(false)),
            WatchdogTiming::default(),
            0x5A,
        );
        // Swap in the observed shutdown channel.
        monitor.thread_shutdown = shutdown_tx;

        monitor.shutdown(ThreadRemovalSignal::Shutdown).await;
        assert_eq!(*shutdown_rx.borrow(), ThreadRemovalSignal::Shutdown);
        assert_eq!(reset.events(), ["115:low"]);
        assert_eq!(rail.events(), ["100:low"]);
    }

    #[tokio::test]
    async fn telemetry_publishes_board_state() {
        let rail = MockPin::new("100", Arc::new(StdMutex::new(Vec::new())));
        let reset = MockPin::new("115", Arc::new(StdMutex::new(Vec::new())));
        let trip = MockPin::new("138", Arc::new(StdMutex::new(Vec::new())));
        let root = mock_pwm_root("telemetry");
        mock_pwm_chip(&root, 0, 0, "16000");
        mock_pwm_chip(&root, 1, 0, "20000");

        let mut monitor = test_monitor(
            rail,
            reset,
            trip,
            root,
            Arc::new(AtomicBool::new(false)),
            WatchdogTiming::default(),
            0x5A, // 90 LSB * 0.5 = 45 C
        );
        let (tx, rx) = watch::channel(BoardTelemetry::default());
        monitor.monitor_tick(&tx).await.unwrap();

        let telemetry = rx.borrow().clone();
        assert_eq!(telemetry.model, "FutureBit Apollo III");
        assert_eq!(telemetry.name, "apollo-iii");
        assert_eq!(telemetry.fans[0].rpm, Some(2300));
        assert_eq!(telemetry.temperatures[0].name, "board");
        assert!(
            (telemetry.temperatures[0]
                .temperature
                .expect("board temp")
                .as_degrees_c()
                - 45.0)
                .abs()
                < 1e-6
        );
        assert!(
            (telemetry.powers[0].voltage_v.expect("PSU voltage") - 5.0).abs() < 1e-6,
            "PSU duty readback 20000 should read 5.0 V"
        );
        assert_eq!(
            telemetry.threads[0].hashrate,
            HashRate::from_terahashes(12.1).0
        );
        assert!(telemetry.threads[0].is_active);
        // Fan duty was commanded by the PI loop (45 C < setpoint -> min duty).
        assert_eq!(
            telemetry.fans[0].percent,
            Some(FanPid::default().min_duty_percent)
        );
    }
}
