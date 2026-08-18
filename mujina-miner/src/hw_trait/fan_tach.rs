//! Fan RPM counter via the Linux GPIO character device.
//!
//! The fan tachometer signal is connected to a GPIO line; each revolution
//! produces `PPR` edges on the line (Apollo III: `/dev/gpiochip0`, line 14,
//! `PPR = 2`). The backend opens the chip, locates the line by offset with
//! `GPIO_GET_LINEINFO`, requests both-edge events with
//! `GPIO_GET_LINEEVENT` (consumer `mujina-fan`), counts
//! `struct gpioevent_data` records over a measurement window, and converts
//! the count to RPM:
//!
//! ```text
//! RPM = (edge_events / PPR) * (60 / window_seconds)
//! ```
//!
//! The [`rpm_from_events`] conversion is platform-independent and unit
//! tested from scripted event counts; the chardev plumbing is Linux-only
//! and never touched by tests (no hardware I/O).

use std::time::Duration;

/// Convert a count of tach edge events over `window` into RPM.
///
/// `pulses_per_rev` is the number of edges the fan produces per revolution
/// (typically 2). A zero `pulses_per_rev` yields `0.0`.
pub fn rpm_from_events(edge_events: u64, pulses_per_rev: u32, window: Duration) -> f64 {
    if pulses_per_rev == 0 || window.is_zero() {
        return 0.0;
    }
    let events_per_minute = edge_events as f64 * (60.0 / window.as_secs_f64());
    events_per_minute / f64::from(pulses_per_rev)
}

#[cfg(target_os = "linux")]
mod chardev {
    use super::*;
    use crate::hw_trait::{HwError, Result};
    use nix::libc;
    use rustix::fs::{Mode, OFlags, open};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::time::Instant;

    /// Default fan tachometer parameters on Apollo III.
    pub const DEFAULT_CHIP_PATH: &str = "/dev/gpiochip0";
    pub const DEFAULT_LINE_OFFSET: u32 = 14;
    pub const DEFAULT_PULSES_PER_REV: u32 = 2;

    /// Kernel `_IOC` request encoding for 64-bit userspace.
    const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
        ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
    }
    const _IOC_READ: u32 = 2;
    const _IOC_WRITE: u32 = 1;
    const _IOC_READWRITE: u32 = 3;

    /// `struct gpiochip_info` (linux/gpio.h), size 68.
    #[repr(C)]
    #[derive(Debug, Default)]
    struct GpiochipInfo {
        name: [i8; 32],
        label: [i8; 32],
        lines: u32,
    }

    /// `struct gpioline_info` (linux/gpio.h), size 72.
    #[repr(C)]
    #[derive(Debug, Default)]
    struct GpiolineInfo {
        line_offset: u32,
        flags: u32,
        name: [i8; 32],
        consumer: [i8; 32],
    }

    /// `struct gpioevent_request` (linux/gpio.h), size 48.
    #[repr(C)]
    #[derive(Debug)]
    struct GpioeventRequest {
        lineoffset: u32,
        handleflags: u32,
        eventflags: u32,
        consumer: [i8; 32],
        fd: i32,
    }

    /// `struct gpioevent_data` (linux/gpio.h), size 16.
    #[repr(C)]
    #[derive(Debug)]
    struct GpioeventData {
        timestamp: u64,
        id: u32,
    }

    const GPIO_GET_CHIPINFO: libc::c_ulong = ioc(_IOC_READ, 0xB4, 0x01, size_of::<GpiochipInfo>());
    const GPIO_GET_LINEINFO: libc::c_ulong = ioc(_IOC_READ, 0xB4, 0x02, size_of::<GpiolineInfo>());
    const GPIO_GET_LINEEVENT: libc::c_ulong =
        ioc(_IOC_READWRITE, 0xB4, 0x05, size_of::<GpioeventRequest>());

    /// The line must be requested as an input.
    const GPIOHANDLE_REQUEST_INPUT: u32 = 0x0001;
    /// Count both rising and falling edges.
    const GPIOEVENT_REQUEST_RISING_EDGE: u32 = 0x0001;
    const GPIOEVENT_REQUEST_FALLING_EDGE: u32 = 0x0002;

    /// Fan tachometer counter backed by the GPIO chardev.
    ///
    /// Call [`FanTach::open`] once after construction, then [`FanTach::sample`]
    /// to measure RPM over a window.
    #[derive(Debug)]
    pub struct FanTach {
        chip_path: PathBuf,
        line_offset: u32,
        pulses_per_rev: u32,
        event_fd: Option<OwnedFd>,
    }

    impl FanTach {
        /// Create a counter for `line_offset` on `chip_path` with the given
        /// pulses-per-revolution.
        pub fn new(chip_path: PathBuf, line_offset: u32, pulses_per_rev: u32) -> Self {
            Self {
                chip_path,
                line_offset,
                pulses_per_rev,
                event_fd: None,
            }
        }

        /// Open the chip and request edge events on the line.
        pub fn open(&mut self) -> Result<()> {
            if self.event_fd.is_some() {
                return Ok(());
            }

            let chip = open(
                &self.chip_path,
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(HwError::Io)?;

            // Validate the chip exists and the offset is in range.
            let mut chip_info = GpiochipInfo::default();
            if unsafe { libc::ioctl(chip.as_raw_fd(), GPIO_GET_CHIPINFO, &mut chip_info) } < 0 {
                return Err(HwError::Io(std::io::Error::last_os_error()));
            }
            let mut line_info = GpiolineInfo {
                line_offset: self.line_offset,
                ..Default::default()
            };
            if unsafe { libc::ioctl(chip.as_raw_fd(), GPIO_GET_LINEINFO, &mut line_info) } < 0 {
                return Err(HwError::Io(std::io::Error::last_os_error()));
            }

            let mut request = GpioeventRequest {
                lineoffset: self.line_offset,
                handleflags: GPIOHANDLE_REQUEST_INPUT,
                eventflags: GPIOEVENT_REQUEST_RISING_EDGE | GPIOEVENT_REQUEST_FALLING_EDGE,
                consumer: consumer_name(),
                fd: -1,
            };
            if unsafe { libc::ioctl(chip.as_raw_fd(), GPIO_GET_LINEEVENT, &mut request) } < 0 {
                return Err(HwError::Io(std::io::Error::last_os_error()));
            }
            // Safety: the kernel wrote a valid fd (>= 0) on success.
            let event_fd = unsafe { OwnedFd::from_raw_fd(request.fd) };
            self.event_fd = Some(event_fd);
            Ok(())
        }

        /// Measure RPM over `window` by counting tach edge events.
        ///
        /// The measurement is blocking; the async wrapper runs it on the
        /// blocking worker pool.
        pub async fn sample(&mut self, window: Duration) -> Result<f64> {
            let fd = self
                .event_fd
                .as_ref()
                .ok_or_else(|| HwError::Other("fan tach not open; call open() first".into()))?
                .try_clone()
                .map_err(HwError::Io)?;
            let pulses_per_rev = self.pulses_per_rev;
            let events = tokio::task::spawn_blocking(move || count_events(&fd, window))
                .await
                .map_err(|e| HwError::Other(format!("fan tach worker failed: {e}")))?
                .map_err(HwError::Io)?;
            Ok(rpm_from_events(events, pulses_per_rev, window))
        }
    }

    /// NUL-terminated consumer name for the event request.
    fn consumer_name() -> [i8; 32] {
        let mut name = [0i8; 32];
        let bytes = b"mujina-fan";
        for (slot, byte) in name.iter_mut().zip(bytes) {
            *slot = *byte as i8;
        }
        name
    }

    /// Count `gpioevent_data` records on the event fd for `window`,
    /// polling in short slices so the deadline is honored.
    fn count_events(fd: &OwnedFd, window: Duration) -> std::io::Result<u64> {
        let deadline = Instant::now() + window;
        let mut count = 0u64;
        let mut data = std::mem::MaybeUninit::<GpioeventData>::uninit();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let timeout_ms = remaining.as_millis().min(100) as libc::c_int;

            let mut pollfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if rc == 0 {
                continue; // Poll slice elapsed; check the deadline again.
            }

            let n = unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    data.as_mut_ptr() as *mut libc::c_void,
                    size_of::<GpioeventData>(),
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n as usize == size_of::<GpioeventData>() {
                count += 1;
            }
        }
        Ok(count)
    }
}

#[cfg(target_os = "linux")]
pub use chardev::{DEFAULT_CHIP_PATH, DEFAULT_LINE_OFFSET, DEFAULT_PULSES_PER_REV, FanTach};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpm_math_with_ppr_2() {
        // 200 edges over 3 s with 2 pulses/rev: (200/2) * (60/3) = 2000 RPM.
        assert!((rpm_from_events(200, 2, Duration::from_secs(3)) - 2000.0).abs() < 1e-9);

        // 1200 edges/min at PPR 2 -> 600 RPM.
        assert!((rpm_from_events(1200, 2, Duration::from_secs(60)) - 600.0).abs() < 1e-9);

        // One revolution per second at PPR 2 -> 30 RPM.
        assert!((rpm_from_events(2, 2, Duration::from_secs(2)) - 30.0).abs() < 1e-9);

        // No events -> 0 RPM.
        assert_eq!(rpm_from_events(0, 2, Duration::from_secs(1)), 0.0);

        // Degenerate PPR is guarded.
        assert_eq!(rpm_from_events(100, 0, Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn rpm_scales_linearly_with_window() {
        let a = rpm_from_events(60, 2, Duration::from_secs(10));
        let b = rpm_from_events(6, 2, Duration::from_secs(1));
        assert!((a - b).abs() < 1e-9);
    }
}
