//! SIC450 PSU telemetry driver, generic over an [`I2c`] implementation.
//!
//! Apollo III power supply: a dual-rail Infineon SIC450 on i2c-3. This
//! driver reads board temperature and per-rail telemetry; it never writes
//! voltage (voltage is set via PWM duty — see `pwm_sysfs`).
//!
//! ## Ground truth (Apollo III CERTAINTY ledger, items A7/A7c)
//!
//! - Telemetry is read with a combined `I2C_RDWR` transaction (write opcode
//!   then read 2 bytes, repeated START), **PEC off**. The `I2c` trait maps
//!   `write_read` to exactly that shape on `LinuxI2c`.
//! - `READ_VOUT` (0x8B) and `READ_IOUT` (0x8C) are confirmed live in the
//!   vendor log; `READ_TEMPERATURE_1` (0x8D) is assumed per PMBus
//!   convention. **TODO (G6 verification):** confirm 0x8D and the
//!   temperature encoding on-device, and confirm the SIC450's own I2C
//!   address (CERTAINTY A7c leaves it unresolved; 0x49 is the address this
//!   board's vendor/OSS paths use and what SG4 targets).
//! - Board temperature is the `LM75`-style read: register 0x00, 1 byte.
//!
//! Any individual register read that NAKs yields `None` for that value
//! instead of failing the whole sample, matching the vendor monitor's
//! behavior.

use crate::hw_trait::{HwError, i2c::I2c};
use crate::tracing::prelude::*;

use super::pmbus::{self, PmbusCommand};

/// Default I2C address used for the SIC450/LM75 on Apollo III (i2c-3).
pub const DEFAULT_ADDRESS: u8 = 0x49;

/// Number of SIC450 rails.
pub const NUM_RAILS: usize = 2;

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    /// I2C bus error
    #[error("I2C: {0}")]
    I2c(E),
}

type Result<T> = std::result::Result<T, Error<HwError>>;

/// Telemetry for one PSU rail. `None` means the register read was not
/// served (NAK) or could not be decoded.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RailTelemetry {
    /// Output voltage in volts (`READ_VOUT`, LINEAR11).
    pub vout_v: Option<f32>,
    /// Output current in amperes (`READ_IOUT`, LINEAR11).
    pub iout_a: Option<f32>,
    /// Temperature in degrees Celsius (`READ_TEMPERATURE_1`, LINEAR11).
    pub temp_c: Option<f32>,
}

/// A full SIC450 telemetry sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sic450Telemetry {
    /// Per-rail telemetry, indexed by PMBus page.
    pub rails: [RailTelemetry; NUM_RAILS],
}

/// SIC450 driver, generic over I2C implementation.
pub struct Sic450<I> {
    i2c: I,
    address: u8,
}

impl<I: I2c> Sic450<I> {
    /// Create a new driver instance at `address` (default
    /// [`DEFAULT_ADDRESS`]).
    pub fn new(i2c: I, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read the board temperature: register 0x00, one byte.
    ///
    /// The raw byte is returned as-is; its scaling is board-specific and
    /// unverified (TODO G6).
    pub async fn read_board_temp(&mut self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[0x00], &mut buf)
            .await
            .map_err(Error::I2c)?;
        Ok(buf[0])
    }

    /// Read per-rail telemetry.
    ///
    /// Each rail is selected via the PMBus `PAGE` register, then
    /// `READ_VOUT`/`READ_IOUT`/`READ_TEMPERATURE_1` are read as LINEAR11
    /// words. A failed register read logs a warning and yields `None` for
    /// that value; a failed page select is also tolerated.
    pub async fn read_telemetry(&mut self) -> Result<Sic450Telemetry> {
        let mut telemetry = Sic450Telemetry::default();
        for (rail, slot) in telemetry.rails.iter_mut().enumerate() {
            if let Err(e) = self
                .i2c
                .write(self.address, &[PmbusCommand::Page.as_u8(), rail as u8])
                .await
            {
                warn!("SIC450 page select for rail {rail} failed: {e:?}");
            }
            slot.vout_v = self.read_linear11(PmbusCommand::ReadVout).await;
            slot.iout_a = self.read_linear11(PmbusCommand::ReadIout).await;
            slot.temp_c = self.read_linear11(PmbusCommand::ReadTemperature1).await;
        }
        Ok(telemetry)
    }

    /// Read a 16-bit LINEAR11 PMBus value; `None` on a NAK or bus error.
    async fn read_linear11(&mut self, command: PmbusCommand) -> Option<f32> {
        let mut buf = [0u8; 2];
        match self
            .i2c
            .write_read(self.address, &[command.as_u8()], &mut buf)
            .await
        {
            Ok(()) => Some(pmbus::linear11::to_float(u16::from_le_bytes(buf))),
            Err(e) => {
                warn!("SIC450 read {command} failed: {e:?}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw_trait::{I2cError, Result as HwResult};
    use async_trait::async_trait;
    use std::collections::VecDeque;

    /// A scripted I2C fake: each call pops the next expected exchange and
    /// either returns the scripted reply bytes or the scripted error.
    struct ScriptedI2c {
        script: VecDeque<ScriptedExchange>,
    }

    struct ScriptedExchange {
        addr: u8,
        write: Vec<u8>,
        read_len: usize,
        reply: HwResult<Vec<u8>>,
    }

    fn script(
        addr: u8,
        write: Vec<u8>,
        read_len: usize,
        reply: HwResult<Vec<u8>>,
    ) -> ScriptedExchange {
        ScriptedExchange {
            addr,
            write,
            read_len,
            reply,
        }
    }

    impl ScriptedI2c {
        fn new(script: Vec<ScriptedExchange>) -> Self {
            Self {
                script: script.into(),
            }
        }

        fn take(&mut self, kind: &str) -> ScriptedExchange {
            self.script
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected {kind} call"))
        }

        fn apply(&self, exchange: ScriptedExchange, read: &mut [u8]) -> HwResult<()> {
            match exchange.reply {
                Ok(bytes) => {
                    assert_eq!(bytes.len(), read.len(), "scripted reply length mismatch");
                    read.copy_from_slice(&bytes);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }

    #[async_trait]
    impl I2c for ScriptedI2c {
        async fn write(&mut self, addr: u8, data: &[u8]) -> HwResult<()> {
            let exchange = self.take("write");
            assert_eq!(exchange.addr, addr);
            assert_eq!(exchange.write, data);
            assert_eq!(exchange.read_len, 0);
            exchange.reply.map(|_| ())
        }

        async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> HwResult<()> {
            let exchange = self.take("read");
            assert_eq!(exchange.addr, addr);
            assert_eq!(exchange.write.len(), 0);
            assert_eq!(exchange.read_len, buffer.len());
            self.apply(exchange, buffer)
        }

        async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> HwResult<()> {
            let exchange = self.take("write_read");
            assert_eq!(exchange.addr, addr);
            assert_eq!(exchange.write, write);
            assert_eq!(exchange.read_len, read.len());
            self.apply(exchange, read)
        }

        async fn set_frequency(&mut self, _hz: u32) -> HwResult<()> {
            Ok(())
        }
    }

    fn page_select(rail: u8) -> Vec<u8> {
        vec![PmbusCommand::Page.as_u8(), rail]
    }

    #[tokio::test]
    async fn board_temp_issues_write_zero_then_read_one_byte() {
        let i2c = ScriptedI2c::new(vec![script(0x49, vec![0x00], 1, Ok(vec![0x5A]))]);
        let mut sic = Sic450::new(i2c, 0x49);
        assert_eq!(sic.read_board_temp().await.unwrap(), 0x5A);
    }

    #[tokio::test]
    async fn telemetry_decodes_linear11_fixtures() {
        // LINEAR11 fixtures:
        //   VOUT rail 0: raw 0xF80A (exp -1, mant 10)  = 5.0 V
        //   IOUT rail 0: raw 0xF819 (exp -1, mant 25)  = 12.5 A
        //   TEMP rail 0: raw 0x002D (exp 0,  mant 45)  = 45.0 C
        //   IOUT rail 1: raw 0x000A (exp 0,  mant 10)  = 10.0 A
        let i2c = ScriptedI2c::new(vec![
            script(0x49, page_select(0), 0, Ok(vec![])),
            script(0x49, vec![0x8B], 2, Ok(vec![0x0A, 0xF8])),
            script(0x49, vec![0x8C], 2, Ok(vec![0x19, 0xF8])),
            script(0x49, vec![0x8D], 2, Ok(vec![0x2D, 0x00])),
            script(0x49, page_select(1), 0, Ok(vec![])),
            script(0x49, vec![0x8B], 2, Ok(vec![0x0A, 0xF8])),
            script(0x49, vec![0x8C], 2, Ok(vec![0x0A, 0x00])),
            script(0x49, vec![0x8D], 2, Ok(vec![0x2D, 0x00])),
        ]);
        let mut sic = Sic450::new(i2c, 0x49);

        let telemetry = sic.read_telemetry().await.unwrap();
        let rail0 = &telemetry.rails[0];
        assert!(
            (rail0.vout_v.unwrap() - 5.0).abs() < 1e-6,
            "vout: {:?}",
            rail0.vout_v
        );
        assert!(
            (rail0.iout_a.unwrap() - 12.5).abs() < 1e-6,
            "iout: {:?}",
            rail0.iout_a
        );
        assert!(
            (rail0.temp_c.unwrap() - 45.0).abs() < 1e-6,
            "temp: {:?}",
            rail0.temp_c
        );

        let rail1 = &telemetry.rails[1];
        assert!(
            (rail1.vout_v.unwrap() - 5.0).abs() < 1e-6,
            "vout: {:?}",
            rail1.vout_v
        );
        assert!(
            (rail1.iout_a.unwrap() - 10.0).abs() < 1e-6,
            "iout: {:?}",
            rail1.iout_a
        );
        assert!(
            (rail1.temp_c.unwrap() - 45.0).abs() < 1e-6,
            "temp: {:?}",
            rail1.temp_c
        );
    }

    #[tokio::test]
    async fn nak_on_one_register_yields_none_not_failure() {
        // VOUT NAKs (device does not serve it) -> None; IOUT reads fine.
        // Rail 1 is scripted normally to let the loop complete.
        let i2c = ScriptedI2c::new(vec![
            script(0x49, page_select(0), 0, Ok(vec![])),
            script(
                0x49,
                vec![0x8B],
                2,
                Err(HwError::I2c(I2cError::NoAck(0x49))),
            ),
            script(0x49, vec![0x8C], 2, Ok(vec![0x19, 0xF8])),
            script(0x49, vec![0x8D], 2, Ok(vec![0x2D, 0x00])),
            script(0x49, page_select(1), 0, Ok(vec![])),
            script(0x49, vec![0x8B], 2, Ok(vec![0x0A, 0xF8])),
            script(0x49, vec![0x8C], 2, Ok(vec![0x0A, 0x00])),
            script(0x49, vec![0x8D], 2, Ok(vec![0x2D, 0x00])),
        ]);
        let mut sic = Sic450::new(i2c, 0x49);

        let telemetry = sic.read_telemetry().await.unwrap();
        assert!(telemetry.rails[0].vout_v.is_none());
        assert!(telemetry.rails[0].iout_a.is_some());
    }

    #[tokio::test]
    async fn board_temp_nak_is_an_error() {
        let i2c = ScriptedI2c::new(vec![script(
            0x49,
            vec![0x00],
            1,
            Err(HwError::I2c(I2cError::NoAck(0x49))),
        )]);
        let mut sic = Sic450::new(i2c, 0x49);
        assert!(sic.read_board_temp().await.is_err());
    }
}
