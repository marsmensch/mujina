//! Linux I2C backend via `/dev/i2c-{bus}` and the `I2C_RDWR` ioctl.
//!
//! ## Transaction shape (vendor-proven, SIC450 on Apollo III)
//!
//! Telemetry on this board is read with one **combined** `I2C_RDWR`
//! transaction: write the opcode byte, then read N bytes with a repeated
//! START (no STOP between the two messages), **PEC off**. A plain
//! `I2C_SLAVE` write-then-read (STOP between) does not work on this
//! hardware, and enabling PEC reads a phantom byte (both produced
//! `Errno 110` in the OSS miner — see the Apollo III CERTAINTY ledger,
//! items A7/A7c). Every trait method here goes through `I2C_RDWR`; no
//! `I2C_PEC` ioctl is ever issued.
//!
//! The fd is opened `O_NONBLOCK` with the kernel `I2C_TIMEOUT` set to 1s,
//! matching the vendor's telemetry path, and transfers are retried a few
//! times on transient `ETIMEDOUT`/`EREMOTEIO`/`EAGAIN` (the SIC450 NAKs
//! during its power-up window after the GPIO115 enable pulse).
//!
//! ## Layout of the ioctl argument
//!
//! The request code is `_IOWR('i', 0x07, struct i2c_rdwr_ioctl_data)`. On
//! 64-bit Linux (the target: RK3588 aarch64) the structs are:
//!
//! ```text
//! struct i2c_msg { u16 addr; u16 flags; u16 len; u16 pad; u8 *buf; }  // 16 bytes
//! struct i2c_rdwr_ioctl_data { struct i2c_msg *msgs; u32 nmsgs; }     // 16 bytes
//! ```
//!
//! The encoding is modelled here with `#[repr(C)]` structs so the tests can
//! assert the exact kernel-ABI bytes without touching real hardware.

/// `I2C_M_RD`: the message is a read.
pub const I2C_M_RD: u16 = 0x0001;

/// `I2C_M_PEC`: SMBus PEC attached to the message. This backend never sets
/// it (the vendor's SIC450 access is PEC-off).
pub const I2C_M_PEC: u16 = 0x0008;

/// Linux `_IOC` request encoding for 64-bit userspace:
/// `(dir << 30) | (size << 16) | (type << 8) | nr`.
const fn ioc_readwrite(ty: u8, nr: u8, size: usize) -> u32 {
    (3 << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32
}

/// The `I2C_RDWR` request code: `_IOWR('i', 0x07, struct i2c_rdwr_ioctl_data)`
/// with `sizeof == 16` on 64-bit Linux.
pub const I2C_RDWR_REQUEST: u32 = ioc_readwrite(b'i', 0x07, 16);

/// A single transfer message in owned form, before encoding into the kernel
/// ABI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I2cMessageSpec {
    /// 7-bit slave address.
    pub addr: u16,
    /// Message flags (`I2C_M_RD`, ...).
    pub flags: u16,
    /// Payload. For read messages this is the read buffer (length only);
    /// the kernel fills it in place.
    pub data: Vec<u8>,
}

impl I2cMessageSpec {
    /// A write message.
    pub fn write(addr: u8, data: Vec<u8>) -> Self {
        Self {
            addr: addr.into(),
            flags: 0,
            data,
        }
    }

    /// A read message of `len` bytes.
    pub fn read(addr: u8, len: usize) -> Self {
        Self {
            addr: addr.into(),
            flags: I2C_M_RD,
            data: vec![0; len],
        }
    }
}

/// Kernel ABI layout of `struct i2c_msg` (linux/i2c.h) on 64-bit Linux.
#[repr(C)]
#[derive(Debug)]
pub struct I2cMsg {
    /// 7-bit slave address (kernel adds the R/W bit).
    pub addr: u16,
    /// Message flags.
    pub flags: u16,
    /// Payload length in bytes.
    pub len: u16,
    /// Implicit alignment padding before the pointer.
    _pad: u16,
    /// Pointer to the message payload.
    pub buf: *mut u8,
}

// Safety: the pointer refers to storage owned by the containing `Transfer`,
// which is the only owner and is never shared across threads.
unsafe impl Send for I2cMsg {}

/// Kernel ABI layout of `struct i2c_rdwr_ioctl_data` (linux/i2c-dev.h) on
/// 64-bit Linux.
#[repr(C)]
#[derive(Debug)]
pub struct I2cRdwrData {
    /// Pointer to the message array.
    pub msgs: *mut I2cMsg,
    /// Number of messages in the array.
    pub nmsgs: u32,
    /// Alignment padding.
    _pad: u32,
}

// Safety: see `I2cMsg`.
unsafe impl Send for I2cRdwrData {}

/// An encoded `I2C_RDWR` transfer.
///
/// Owns the kernel-ABI header, the message array, and every payload buffer,
/// so the ioctl can be handed stable pointers for the duration of the call.
#[derive(Debug)]
pub struct Transfer {
    data: I2cRdwrData,
    msgs: Vec<I2cMsg>,
    buffers: Vec<Vec<u8>>,
}

impl Transfer {
    /// Encode the given messages into the kernel ABI.
    pub fn new(specs: &[I2cMessageSpec]) -> Self {
        let mut buffers: Vec<Vec<u8>> = specs.iter().map(|s| s.data.clone()).collect();
        let mut msgs: Vec<I2cMsg> = specs
            .iter()
            .map(|s| I2cMsg {
                addr: s.addr,
                flags: s.flags,
                len: s.data.len() as u16,
                _pad: 0,
                buf: std::ptr::null_mut(),
            })
            .collect();
        for (msg, buf) in msgs.iter_mut().zip(buffers.iter_mut()) {
            msg.buf = buf.as_mut_ptr();
        }
        let data = I2cRdwrData {
            msgs: msgs.as_mut_ptr(),
            nmsgs: specs.len() as u32,
            _pad: 0,
        };
        Self {
            data,
            msgs,
            buffers,
        }
    }

    /// Mutable pointer to the ioctl argument struct. Valid while `self` is
    /// alive (the ioctl call is synchronous).
    pub fn as_mut_ptr(&mut self) -> *mut I2cRdwrData {
        &mut self.data
    }

    /// Number of messages.
    pub fn nmsgs(&self) -> u32 {
        self.data.nmsgs
    }

    /// The encoded message array.
    pub fn msgs(&self) -> &[I2cMsg] {
        &self.msgs
    }

    /// The payload buffers (index-aligned with `msgs`).
    pub fn buffers(&self) -> &[Vec<u8>] {
        &self.buffers
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::hw_trait::{HwError, I2c, Result};
    use async_trait::async_trait;
    use nix::libc;
    use rustix::fs::{Mode, OFlags, open};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::time::Duration;

    /// `I2C_TIMEOUT` request code: `_IOW('i', 0x09, u32)`.
    const I2C_TIMEOUT_REQUEST: libc::c_ulong = 0x40046909;

    /// Default transfer retry parameters (vendor/OSS-proven: ~5 x 10ms on
    /// the SIC450 power-up NAK window).
    const DEFAULT_MAX_ATTEMPTS: u32 = 5;
    const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(10);

    /// Linux I2C bus via `/dev/i2c-N`.
    pub struct LinuxI2c {
        /// Device path, for error messages.
        path: PathBuf,
        /// Open device fd.
        fd: OwnedFd,
        /// Transfer attempts before giving up on a retryable error.
        max_attempts: u32,
        /// Delay between retry attempts.
        retry_delay: Duration,
    }

    impl LinuxI2c {
        /// Open `/dev/i2c-{bus}`.
        pub fn new(bus: u8) -> Result<Self> {
            Self::with_path(PathBuf::from(format!("/dev/i2c-{bus}")))
        }

        /// Open an explicit device path (injectable for non-standard nodes).
        pub fn with_path(path: PathBuf) -> Result<Self> {
            let fd =
                open(&path, OFlags::RDWR | OFlags::NONBLOCK, Mode::empty()).map_err(HwError::Io)?;
            // Bound each transfer so a wedged slave cannot stall the caller
            // indefinitely (vendor sets the same 1s kernel timeout).
            set_i2c_timeout(&fd, 1).map_err(HwError::Io)?;
            Ok(Self {
                path,
                fd,
                max_attempts: DEFAULT_MAX_ATTEMPTS,
                retry_delay: DEFAULT_RETRY_DELAY,
            })
        }

        /// Run an encoded transfer on the blocking worker, with retries.
        async fn run(&self, specs: Vec<I2cMessageSpec>) -> Result<Vec<u8>> {
            let fd = self.fd.try_clone().map_err(HwError::Io)?;
            let max_attempts = self.max_attempts;
            let retry_delay = self.retry_delay;
            tokio::task::spawn_blocking(move || {
                transfer_with_retry(&fd, &specs, max_attempts, retry_delay)
            })
            .await
            .map_err(|e| HwError::Other(format!("i2c worker failed: {e}")))?
            .map_err(HwError::Io)
        }
    }

    /// Set the kernel `I2C_TIMEOUT` (seconds) for the fd.
    fn set_i2c_timeout(fd: &OwnedFd, timeout_secs: u32) -> std::io::Result<()> {
        let timeout = timeout_secs;
        let rc =
            unsafe { libc::ioctl(fd.as_raw_fd(), I2C_TIMEOUT_REQUEST, &timeout as *const u32) };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Whether a transfer error is worth retrying (bus contention or a
    /// device still in its power-up NAK window).
    fn is_retryable(err: &std::io::Error) -> bool {
        matches!(
            err.raw_os_error(),
            Some(libc::ETIMEDOUT | libc::EREMOTEIO | libc::EAGAIN)
        )
    }

    /// Issue one `I2C_RDWR`, returning the data of every read message.
    fn transfer_once(fd: &OwnedFd, specs: &[I2cMessageSpec]) -> std::io::Result<Vec<u8>> {
        let mut transfer = Transfer::new(specs);
        let rc = unsafe {
            libc::ioctl(
                fd.as_raw_fd(),
                I2C_RDWR_REQUEST as libc::c_ulong,
                transfer.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut out = Vec::new();
        for (msg, buf) in transfer.msgs().iter().zip(transfer.buffers()) {
            if msg.flags & I2C_M_RD != 0 {
                out.extend_from_slice(buf);
            }
        }
        Ok(out)
    }

    /// Issue `I2C_RDWR` with retries on transient errors.
    fn transfer_with_retry(
        fd: &OwnedFd,
        specs: &[I2cMessageSpec],
        max_attempts: u32,
        retry_delay: Duration,
    ) -> std::io::Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            match transfer_once(fd, specs) {
                Ok(out) => return Ok(out),
                Err(e) if is_retryable(&e) && attempt + 1 < max_attempts => {
                    attempt += 1;
                    std::thread::sleep(retry_delay);
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[async_trait]
    impl I2c for LinuxI2c {
        async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
            let specs = vec![I2cMessageSpec::write(addr, data.to_vec())];
            self.run(specs).await?;
            Ok(())
        }

        async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
            let specs = vec![I2cMessageSpec::read(addr, buffer.len())];
            let out = self.run(specs).await?;
            debug_assert_eq!(out.len(), buffer.len());
            buffer.copy_from_slice(&out);
            Ok(())
        }

        async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
            let specs = vec![
                I2cMessageSpec::write(addr, write.to_vec()),
                I2cMessageSpec::read(addr, read.len()),
            ];
            let out = self.run(specs).await?;
            debug_assert_eq!(out.len(), read.len());
            read.copy_from_slice(&out);
            Ok(())
        }

        async fn set_frequency(&mut self, _hz: u32) -> Result<()> {
            // No-op: the bus clock is configured by the kernel/device tree,
            // and the I2C_RDWR interface has no per-transfer frequency
            // control. Kept for trait conformance.
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::LinuxI2c;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_abi_layout_is_64_bit_linux() {
        assert_eq!(std::mem::size_of::<I2cMsg>(), 16);
        assert_eq!(std::mem::size_of::<I2cRdwrData>(), 16);
        assert_eq!(std::mem::offset_of!(I2cMsg, addr), 0);
        assert_eq!(std::mem::offset_of!(I2cMsg, flags), 2);
        assert_eq!(std::mem::offset_of!(I2cMsg, len), 4);
        assert_eq!(std::mem::offset_of!(I2cMsg, buf), 8);
        assert_eq!(std::mem::offset_of!(I2cRdwrData, msgs), 0);
        assert_eq!(std::mem::offset_of!(I2cRdwrData, nmsgs), 8);
    }

    #[test]
    fn i2c_rdwr_request_code_matches_kernel_encoding() {
        // _IOWR('i', 0x07, struct i2c_rdwr_ioctl_data) with
        // sizeof(struct i2c_rdwr_ioctl_data) == 16 on 64-bit Linux.
        assert_eq!(I2C_RDWR_REQUEST, 0xC0106907);
    }

    /// The SIC450 VOUT read shape from the Apollo III CERTAINTY ledger:
    /// write `0x02 0x50 0x00` (register 0x02 = 5.0 V config-latch write,
    /// per the vendor's `02 50` boot config), then read 2 bytes back.
    #[test]
    fn sic450_vout_exchange_encodes_expected_i2c_msgs() {
        let specs = vec![
            I2cMessageSpec::write(0x49, vec![0x02, 0x50, 0x00]),
            I2cMessageSpec::read(0x49, 2),
        ];
        let transfer = Transfer::new(&specs);

        assert_eq!(transfer.nmsgs(), 2);
        assert_eq!(transfer.buffers().len(), 2);

        // Message 0: write 3 bytes to 0x49.
        let m0 = &transfer.msgs()[0];
        assert_eq!(m0.addr, 0x49);
        assert_eq!(m0.flags, 0);
        assert_eq!(m0.len, 3);
        assert_eq!(transfer.buffers()[0], vec![0x02, 0x50, 0x00]);

        // Message 1: read 2 bytes from 0x49 (repeated START, no STOP).
        let m1 = &transfer.msgs()[1];
        assert_eq!(m1.addr, 0x49);
        assert_eq!(m1.flags, I2C_M_RD);
        assert_eq!(m1.len, 2);

        // PEC is never set on any message.
        assert!(transfer.msgs().iter().all(|m| m.flags & I2C_M_PEC == 0));
    }

    #[test]
    fn board_temp_shape_is_single_byte_read() {
        let specs = vec![
            I2cMessageSpec::write(0x49, vec![0x00]),
            I2cMessageSpec::read(0x49, 1),
        ];
        let transfer = Transfer::new(&specs);
        assert_eq!(transfer.msgs()[0].len, 1);
        assert_eq!(transfer.msgs()[1].flags, I2C_M_RD);
        assert_eq!(transfer.msgs()[1].len, 1);
    }

    #[test]
    fn plain_write_encodes_one_message() {
        let transfer = Transfer::new(&[I2cMessageSpec::write(0x49, vec![0x00, 0x00])]);
        assert_eq!(transfer.nmsgs(), 1);
        assert_eq!(transfer.msgs()[0].addr, 0x49);
        assert_eq!(transfer.msgs()[0].flags, 0);
        assert_eq!(transfer.msgs()[0].len, 2);
    }
}
