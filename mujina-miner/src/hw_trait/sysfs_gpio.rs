//! Linux GPIO backend via the sysfs interface (`/sys/class/gpio`).
//!
//! This backend exports a GPIO line by writing its number to the `export`
//! attribute, then drives it through the `direction` and `value` files of
//! the per-line directory (`/sys/class/gpio/gpio{N}`).
//!
//! Apollo III usage (all lines fit in the `u8` pin-number type of the
//! [`Gpio`] trait):
//!
//! - 148: board-MCU heartbeat clock, output high
//! - 115: ASIC reset, pulse `0 -> 1`
//! - 100: ASIC rail power (`active_low = 0`, so `value = 1` is ON)
//! - 138: thermal trip, input
//!
//! The root directory is injectable for tests (a temp-dir mock sysfs tree)
//! via [`SysfsGpio::with_root`]; it defaults to `/sys/class/gpio`.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use super::{Gpio, GpioPin, HwError, PinMode, PinValue, Result};

/// Default sysfs GPIO root.
const DEFAULT_ROOT: &str = "/sys/class/gpio";

/// How long to wait for the per-line directory to appear after an export
/// write. sysfs creates it asynchronously; this bounds the wait.
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

/// Sysfs GPIO controller.
#[derive(Debug, Clone)]
pub struct SysfsGpio {
    root: PathBuf,
}

impl SysfsGpio {
    /// Create a controller rooted at `/sys/class/gpio`.
    pub fn new() -> Self {
        Self::with_root(PathBuf::from(DEFAULT_ROOT))
    }

    /// Create a controller rooted at `root` (used by tests with a mock tree).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// Absolute path of the `gpio{N}` line directory.
    fn line_dir(&self, number: u8) -> PathBuf {
        self.root.join(format!("gpio{number}"))
    }

    /// Export `number`, returning the line directory once it exists.
    ///
    /// Writing to `export` when the line is already exported fails with
    /// `EBUSY`; that is treated as success. After a successful write the
    /// line directory appears asynchronously, so we poll briefly for it.
    async fn export_line(&self, number: u8) -> Result<PathBuf> {
        let dir = self.line_dir(number);
        let root = self.root.clone();
        let poll_dir = dir.clone();
        tokio::task::spawn_blocking(move || -> std::result::Result<(), HwError> {
            match std::fs::write(root.join("export"), format!("{number}\n")) {
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
                "line gpio{number} did not appear under {} after export",
                root.display()
            )))
        })
        .await
        .map_err(|e| HwError::Other(format!("export worker failed: {e}")))??;
        Ok(dir)
    }
}

impl Default for SysfsGpio {
    fn default() -> Self {
        Self::new()
    }
}

/// A single exported sysfs GPIO line.
#[derive(Debug, Clone)]
pub struct SysfsGpioPin {
    /// Absolute path of the `gpio{N}` directory.
    dir: PathBuf,
}

impl SysfsGpioPin {
    /// Path of a sysfs attribute inside the line directory.
    fn attr(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

#[async_trait]
impl Gpio for SysfsGpio {
    type Pin = SysfsGpioPin;

    async fn pin(&mut self, number: u8) -> Result<Self::Pin> {
        let dir = self.export_line(number).await?;
        Ok(SysfsGpioPin { dir })
    }
}

#[async_trait]
impl GpioPin for SysfsGpioPin {
    async fn set_mode(&mut self, mode: PinMode) -> Result<()> {
        let contents = match mode {
            PinMode::Input => "in",
            PinMode::Output => "out",
        };
        blocking_write(self.attr("direction"), contents.to_string())
            .await
            .map_err(HwError::Io)
    }

    async fn write(&mut self, value: PinValue) -> Result<()> {
        let contents = match value {
            PinValue::Low => "0",
            PinValue::High => "1",
        };
        blocking_write(self.attr("value"), contents.to_string())
            .await
            .map_err(HwError::Io)
    }

    async fn read(&mut self) -> Result<PinValue> {
        let value = blocking_read(self.attr("value"))
            .await
            .map_err(HwError::Io)?;
        Ok(if value.trim() == "1" {
            PinValue::High
        } else {
            PinValue::Low
        })
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

    /// Build a mock tree with a pre-exported `gpio{N}` line.
    fn mock_line(root: &Path, number: u8) {
        let line = root.join(format!("gpio{number}"));
        fs::create_dir_all(&line).unwrap();
        fs::write(line.join("direction"), "").unwrap();
        fs::write(line.join("value"), "").unwrap();
        fs::write(root.join("export"), "").unwrap();
    }

    #[tokio::test]
    async fn export_writes_number_and_round_trips() {
        let root = mock_root("gpio-roundtrip");
        mock_line(&root, 148);

        let mut gpio = SysfsGpio::with_root(root.clone());
        let mut pin = gpio.pin(148).await.unwrap();

        // Export write landed in the export file.
        assert_eq!(fs::read_to_string(root.join("export")).unwrap(), "148\n");

        // Direction round trip.
        pin.set_mode(PinMode::Output).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("gpio148/direction")).unwrap(),
            "out"
        );

        // Value round trip as output.
        pin.write(PinValue::High).await.unwrap();
        assert_eq!(fs::read_to_string(root.join("gpio148/value")).unwrap(), "1");
        pin.write(PinValue::Low).await.unwrap();
        assert_eq!(fs::read_to_string(root.join("gpio148/value")).unwrap(), "0");

        // Read round trip as input.
        pin.set_mode(PinMode::Input).await.unwrap();
        assert_eq!(
            fs::read_to_string(root.join("gpio148/direction")).unwrap(),
            "in"
        );
        fs::write(root.join("gpio148/value"), "1").unwrap();
        assert_eq!(pin.read().await.unwrap(), PinValue::High);
        fs::write(root.join("gpio148/value"), "0").unwrap();
        assert_eq!(pin.read().await.unwrap(), PinValue::Low);
    }

    #[tokio::test]
    async fn missing_line_errors() {
        let root = mock_root("gpio-missing");
        fs::write(root.join("export"), "").unwrap();
        // No gpio200 directory exists, so the export never materializes.

        let mut gpio = SysfsGpio::with_root(root.clone());
        let err = gpio.pin(200).await.unwrap_err();
        assert!(
            err.to_string().contains("gpio200"),
            "unexpected error: {err}"
        );

        // The export attribute still received the line number.
        assert_eq!(fs::read_to_string(root.join("export")).unwrap(), "200\n");
    }
}
