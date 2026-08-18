//! Sysfs PWM backend (`/sys/class/pwm`).
//!
//! Exports a PWM channel via `pwmchip{N}/export`, then drives it through
//! `period`, `duty_cycle`, and `enable` in the `pwm{M}` directory. Values
//! are nanoseconds as decimal strings (sysfs convention).
//!
//! Apollo III usage:
//!
//! - PSU output voltage = `pwmchip1/pwm0`: period 40000 ns, duty 20000
//!   (~5.0 V) up to 36000 (~6.1 V). The vendor ramps duty to set voltage;
//!   `VOUT_COMMAND` PMBus writes are not used on this board revision.
//! - Fan = `pwmchip0/pwm0` (`fd8b0010.pwm`, `npwm = 1`).
//!
//! The sysfs root is injectable for tests (a temp-dir mock tree) via
//! [`PwmSysfs::with_root`]; it defaults to `/sys/class/pwm`.

use std::path::PathBuf;
use std::time::Duration;

use crate::hw_trait::{HwError, Result};

/// Default sysfs PWM root.
const DEFAULT_ROOT: &str = "/sys/class/pwm";

/// How long to wait for the per-channel directory to appear after an export
/// write.
const EXPORT_POLL_ATTEMPTS: u32 = 100;
const EXPORT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A blocking write to a sysfs attribute, off the async executor.
async fn blocking_write(path: PathBuf, contents: String) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || std::fs::write(&path, contents))
        .await
        .map_err(|e| std::io::Error::other(format!("blocking sysfs write failed: {e}")))?
}

/// A blocking read of a sysfs attribute, off the async executor.
async fn blocking_read(path: PathBuf) -> std::io::Result<String> {
    tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
        .await
        .map_err(|e| std::io::Error::other(format!("blocking sysfs read failed: {e}")))?
}

/// Sysfs PWM channel.
#[derive(Debug, Clone)]
pub struct PwmSysfs {
    /// Root directory holding the `pwmchip{N}` directories.
    root: PathBuf,
    /// PWM chip index.
    chip: u32,
    /// Channel index within the chip.
    channel: u32,
}

impl PwmSysfs {
    /// Create a channel rooted at `/sys/class/pwm`.
    pub fn new(chip: u32, channel: u32) -> Self {
        Self::with_root(PathBuf::from(DEFAULT_ROOT), chip, channel)
    }

    /// Create a channel rooted at `root` (used by tests with a mock tree).
    pub fn with_root(root: PathBuf, chip: u32, channel: u32) -> Self {
        Self {
            root,
            chip,
            channel,
        }
    }

    /// Absolute path of the chip directory.
    fn chip_dir(&self) -> PathBuf {
        self.root.join(format!("pwmchip{}", self.chip))
    }

    /// Absolute path of the channel directory.
    fn pwm_dir(&self) -> PathBuf {
        self.chip_dir().join(format!("pwm{}", self.channel))
    }

    /// Export the channel.
    ///
    /// A missing chip directory is a hard error. Writing to `export` when
    /// the channel is already exported fails with `EBUSY`; that is treated
    /// as success. The channel directory appears asynchronously, so we poll
    /// briefly for it.
    pub async fn export(&mut self) -> Result<()> {
        let chip_dir = self.chip_dir();
        if !chip_dir.exists() {
            return Err(HwError::Other(format!(
                "pwmchip{} not found under {}",
                self.chip,
                self.root.display()
            )));
        }

        let pwm_dir = self.pwm_dir();
        let poll_dir = pwm_dir.clone();
        let channel = self.channel;
        tokio::task::spawn_blocking(move || -> std::result::Result<(), HwError> {
            match std::fs::write(chip_dir.join("export"), format!("{channel}\n")) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(nix::libc::EBUSY) => {
                    // Already exported by someone else.
                }
                Err(e) => return Err(e.into()),
            }
            for _ in 0..EXPORT_POLL_ATTEMPTS {
                if poll_dir.exists() {
                    return Ok(());
                }
                std::thread::sleep(EXPORT_POLL_INTERVAL);
            }
            Err(HwError::Other(format!(
                "pwm channel {channel} did not appear under {} after export",
                chip_dir.display()
            )))
        })
        .await
        .map_err(|e| HwError::Other(format!("export worker failed: {e}")))??;
        Ok(())
    }

    /// Set the PWM period in nanoseconds.
    pub async fn set_period_ns(&self, period_ns: u64) -> Result<()> {
        self.write_attr("period", period_ns.to_string()).await
    }

    /// Set the PWM duty cycle in nanoseconds (must not exceed the period).
    pub async fn set_duty_ns(&self, duty_ns: u64) -> Result<()> {
        self.write_attr("duty_cycle", duty_ns.to_string()).await
    }

    /// Read the currently applied duty cycle in nanoseconds.
    pub async fn get_duty_ns(&self) -> Result<u64> {
        let raw = blocking_read(self.pwm_dir().join("duty_cycle"))
            .await
            .map_err(HwError::Io)?;
        raw.trim()
            .parse()
            .map_err(|_| HwError::Other(format!("unparseable PWM duty: {raw:?}")))
    }

    /// Set the duty cycle as a percentage of the current period.
    ///
    /// Reads the applied period from sysfs first (the kernel may have
    /// rounded it), then writes `period * percent / 100`. The value is
    /// clamped to `[0%, 100%]`.
    pub async fn set_duty_percent(&self, percent: f32) -> Result<()> {
        let raw_period = blocking_read(self.pwm_dir().join("period"))
            .await
            .map_err(HwError::Io)?;
        let period_ns: u64 = raw_period
            .trim()
            .parse()
            .map_err(|_| HwError::Other(format!("unparseable PWM period: {raw_period:?}")))?;
        let clamped = percent.clamp(0.0, 100.0);
        let duty_ns = ((period_ns as f64) * f64::from(clamped) / 100.0).round() as u64;
        self.write_attr("duty_cycle", duty_ns.to_string()).await
    }

    /// Enable the PWM output (writes `1` to `enable`).
    pub async fn enable(&self) -> Result<()> {
        self.write_attr("enable", "1".to_string()).await
    }

    /// Disable the PWM output (writes `0` to `enable`).
    pub async fn disable(&self) -> Result<()> {
        self.write_attr("enable", "0".to_string()).await
    }

    /// Write a channel attribute, erroring if the channel is not exported.
    async fn write_attr(&self, attr: &str, contents: String) -> Result<()> {
        let pwm_dir = self.pwm_dir();
        if !pwm_dir.exists() {
            return Err(HwError::Other(format!(
                "pwm channel {} is not exported (missing {})",
                self.channel,
                pwm_dir.display()
            )));
        }
        blocking_write(pwm_dir.join(attr), contents)
            .await
            .map_err(HwError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a fresh temp-dir mock sysfs root.
    fn mock_root(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mujina-sg3-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a mock chip with one pre-exported channel.
    fn mock_chip(root: &Path, chip: u32, channel: u32) {
        let chip_dir = root.join(format!("pwmchip{chip}"));
        fs::create_dir_all(&chip_dir).unwrap();
        fs::write(chip_dir.join("export"), "").unwrap();
        let pwm_dir = chip_dir.join(format!("pwm{channel}"));
        fs::create_dir_all(&pwm_dir).unwrap();
        fs::write(pwm_dir.join("period"), "").unwrap();
        fs::write(pwm_dir.join("duty_cycle"), "").unwrap();
        fs::write(pwm_dir.join("enable"), "").unwrap();
    }

    #[tokio::test]
    async fn export_writes_channel_and_attrs_land_in_right_files() {
        let root = mock_root("pwm-roundtrip");
        mock_chip(&root, 1, 0);

        let mut pwm = PwmSysfs::with_root(root.clone(), 1, 0);
        pwm.export().await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/export")).unwrap(),
            "0\n"
        );

        pwm.set_period_ns(40_000).await.unwrap();
        pwm.set_duty_ns(20_000).await.unwrap();
        pwm.enable().await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/period")).unwrap(),
            "40000"
        );
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/duty_cycle")).unwrap(),
            "20000"
        );
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/enable")).unwrap(),
            "1"
        );

        // Percent duty is computed from the period currently in sysfs.
        fs::write(root.join("pwmchip1/pwm0/period"), "40000").unwrap();
        pwm.set_duty_percent(50.0).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/duty_cycle")).unwrap(),
            "20000"
        );
        pwm.set_duty_percent(90.0).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/duty_cycle")).unwrap(),
            "36000"
        );

        pwm.disable().await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pwmchip1/pwm0/enable")).unwrap(),
            "0"
        );
    }

    #[tokio::test]
    async fn missing_chip_errors_cleanly() {
        let root = mock_root("pwm-missing");
        // No pwmchip9 directory exists.

        let mut pwm = PwmSysfs::with_root(root, 9, 0);
        let err = pwm.export().await.unwrap_err();
        assert!(
            err.to_string().contains("pwmchip9"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn duty_readback_round_trips() {
        let root = mock_root("pwm-readback");
        mock_chip(&root, 1, 0);

        let mut pwm = PwmSysfs::with_root(root.clone(), 1, 0);
        pwm.export().await.unwrap();
        pwm.set_duty_ns(20_000).await.unwrap();
        assert_eq!(pwm.get_duty_ns().await.unwrap(), 20_000);
        pwm.set_duty_ns(36_000).await.unwrap();
        assert_eq!(pwm.get_duty_ns().await.unwrap(), 36_000);

        // Garbage in the attribute is an error, not a panic.
        fs::write(root.join("pwmchip1/pwm0/duty_cycle"), "not-a-number").unwrap();
        assert!(pwm.get_duty_ns().await.is_err());
    }

    #[tokio::test]
    async fn ops_before_export_error() {
        let root = mock_root("pwm-not-exported");
        let chip_dir = root.join("pwmchip0");
        fs::create_dir_all(&chip_dir).unwrap();
        fs::write(chip_dir.join("export"), "").unwrap();
        // No pwm0 directory: the channel was never exported.

        let pwm = PwmSysfs::with_root(root, 0, 0);
        let err = pwm.set_period_ns(1000).await.unwrap_err();
        assert!(
            err.to_string().contains("not exported"),
            "unexpected error: {err}"
        );
    }
}
