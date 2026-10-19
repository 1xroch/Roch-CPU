//! CPU VDD2 - the memory controller's supply - set through the regulator on the EC's I2C bus.

use std::sync::Mutex;

use anyhow::Result;

use super::ec_mailbox::EcMailbox;
use super::native;
use super::super_io::{SuperIo, with_finally};

const R_STATUS: u8 = 0x05;
const R_VOLTAGE: u8 = 0x01;

/// How long to wait after a write before the rail reading means anything. The Super I/O
/// samples its voltage channels on its own schedule, and it lags a change on this rail by
/// several hundred milliseconds: measuring at 150 ms read the old value, and the *next*
/// reading then showed the previous step, which looks exactly like a step that did nothing.
const SETTLE_MS: u64 = 700;

struct Vdd2State {
    baseline_code: Option<u8>,
    baseline_volts: f64,
    measured_step_volts: f64,
    calibrated: bool,
    status: String,
}

/// CPU VDD2 - the memory controller's supply - set through the regulator on the EC's I2C bus.
///
/// The path is one byte: register 0x01 of the device at 0x20, captured from the vendor tool
/// changing that rail. Register 0x05 reads as a status byte first, which the vendor tool does and
/// this follows. The value is a step count, measured at [`Vdd2Rail::STEP_VOLTS`] per count against
/// the Super I/O's own reading of the rail.
///
/// **This is a regulator, so the value is clamped in volts and everything is relative.**
/// The raw byte reaches well past 3 V at the top of its range, which would destroy the memory
/// controller, so nothing here computes an absolute code from an assumed zero point. The current
/// code is read, the current rail is measured, and the target is reached by stepping the code and
/// checking the rail after each step - the same discipline as the base clock, and with the same
/// consequence for a step that does not land where it was aimed: put the start-up value back and
/// stop. On top of that the request is clamped to [`Vdd2Rail::MIN_VOLTS`]..[`Vdd2Rail::MAX_VOLTS`]
/// and the code may never move more than [`Vdd2Rail::MAX_STEPS_FROM_BASELINE`] from where it
/// started, so even a wrong scale cannot walk the rail away.
pub struct Vdd2Rail {
    mb: EcMailbox,
    measure: Box<dyn Fn() -> Option<f64> + Send + Sync>,
    state: Mutex<Vdd2State>,
}

fn opt3(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_default()
}

impl Vdd2Rail {
    /// The regulator's I2C address in the 8-bit form the mailbox wants (7-bit 0x10).
    pub const ADDRESS: u8 = 0x20;

    /// Volts per code step, measured: the vendor tool moving this rail by two counts moved the
    /// measured rail by 20 mV. [`Vdd2Rail::calibrate`] re-measures it here rather than trusting it.
    pub const STEP_VOLTS: f64 = 0.010;

    /// Clamp. Stock on the board this was measured on is about 1.37 V. DDR5 memory-controller
    /// supplies live in a narrow band and there is no reason to go outside it from here.
    pub const MIN_VOLTS: f64 = 1.100;
    pub const MAX_VOLTS: f64 = 1.450;

    /// Hard limit on how far the code may move from where it was found at start-up.
    pub const MAX_STEPS_FROM_BASELINE: i32 = 20;

    /// How far the rail may land from where it was aimed before a step counts as failed. The
    /// Super I/O reads this rail through a divide-by-two input, so its quantum is a few mV, and
    /// the regulator's own tolerance adds to that.
    pub const TOLERANCE_VOLTS: f64 = 0.012;

    pub fn new(mailbox: EcMailbox, measure_rail: Box<dyn Fn() -> Option<f64> + Send + Sync>) -> Vdd2Rail {
        Vdd2Rail {
            mb: mailbox,
            measure: measure_rail,
            state: Mutex::new(Vdd2State {
                baseline_code: None,
                baseline_volts: 0.0,
                measured_step_volts: Self::STEP_VOLTS,
                calibrated: false,
                status: "not started".to_string(),
            }),
        }
    }

    pub fn is_supported(sio: Option<&SuperIo>) -> bool {
        EcMailbox::is_supported(sio)
    }

    /// The code found at start-up.
    pub fn baseline_code(&self) -> Option<u8> {
        native::lock(&self.state).baseline_code
    }

    /// The rail that went with the start-up code.
    pub fn baseline_volts(&self) -> f64 {
        native::lock(&self.state).baseline_volts
    }

    pub fn measured_step_volts(&self) -> f64 {
        native::lock(&self.state).measured_step_volts
    }

    pub fn calibrated(&self) -> bool {
        native::lock(&self.state).calibrated
    }

    pub fn status(&self) -> String {
        native::lock(&self.state).status.clone()
    }

    fn set_status(&self, s: impl Into<String>) {
        native::lock(&self.state).status = s.into();
    }

    // ---------------------------------------------------------------- access
    pub fn read_code(&self) -> Result<Option<u8>> {
        self.mb.read_byte(Self::ADDRESS, R_STATUS)?; // the vendor tool reads this first; harmless, and kept
        self.mb.read_byte(Self::ADDRESS, R_VOLTAGE)
    }

    fn write_code(&self, code: u8) -> Result<bool> {
        self.mb.write_byte(Self::ADDRESS, R_VOLTAGE, code)
    }

    /// Median of several rail readings; the Super I/O value wanders by a millivolt or two.
    /// (C# default `samples` is 5.)
    fn measure_rail(&self, samples: i32) -> Option<f64> {
        let mut v = Vec::new();
        for _ in 0..samples {
            if let Some(r) = (self.measure)() {
                if r > 0.5 && r < 3.0 {
                    v.push(r);
                }
            }
            native::sleep_ms(40);
        }
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(v[v.len() / 2])
    }

    pub fn capture_baseline(&self) -> Result<bool> {
        let Some(code) = self.read_code()? else {
            self.set_status("the VDD2 regulator did not answer");
            return Ok(false);
        };
        let Some(v) = self.measure_rail(3) else {
            self.set_status("the VDD2 rail could not be measured");
            return Ok(false);
        };
        let mut s = native::lock(&self.state);
        s.baseline_code = Some(code);
        s.baseline_volts = v;
        s.status = format!("code 0x{code:02X}, rail {v:.3} V");
        Ok(true)
    }

    pub fn restore(&self) -> Result<bool> {
        let Some(code) = self.baseline_code() else { return Ok(false) };
        let ok = self.write_code(code)?;
        let volts = self.baseline_volts();
        self.set_status(if ok { format!("restored to {volts:.3} V") } else { "restore FAILED".to_string() });
        Ok(ok)
    }

    /// Measures volts per code step by moving one count down and watching the rail. Downwards is
    /// the safe direction, and a single count is the smallest move the regulator can make.
    pub fn calibrate(&self, log: Option<&dyn Fn(&str)>) -> Result<bool> {
        if self.baseline_code().is_none() && !self.capture_baseline()? {
            return Ok(false);
        }
        let start = self.baseline_code().unwrap();
        if start == 0 {
            self.set_status("VDD2 code is already at zero");
            return Ok(false);
        }
        let Some(v0) = self.measure_rail(5) else {
            self.set_status("the VDD2 rail could not be measured");
            return Ok(false);
        };
        if let Some(log) = log {
            log(&format!("  baseline code 0x{start:02X}, rail {v0:.3} V"));
        }

        let body = (|| -> Result<bool> {
            if !self.write_code(start - 1)? {
                self.set_status("probe write failed");
                return Ok(false);
            }
            native::sleep_ms(SETTLE_MS);
            let Some(v1) = self.measure_rail(5) else {
                self.set_status("the probe could not be measured");
                return Ok(false);
            };
            let delta = v0 - v1;
            if let Some(log) = log {
                log(&format!("  one count down -> {v1:.3} V (a step of {:.1} mV)", delta * 1000.0));
            }

            if delta < 0.004 || delta > 0.030 {
                self.set_status(format!(
                    "a one-count step moved the rail {:.1} mV, which is not the expected scale - refusing to set VDD2",
                    delta * 1000.0
                ));
                return Ok(false);
            }

            let mut s = native::lock(&self.state);
            s.measured_step_volts = delta;
            s.calibrated = true;
            s.status = format!(
                "{:.1} mV per step, range {:.3}-{:.3} V",
                delta * 1000.0,
                Self::MIN_VOLTS,
                Self::MAX_VOLTS
            );
            Ok(true)
        })();
        let fin = (|| -> Result<()> {
            self.write_code(start)?;
            native::sleep_ms(SETTLE_MS);
            if let Some(log) = log {
                log(&format!("  restored: {} V", opt3(self.measure_rail(3))));
            }
            Ok(())
        })();
        with_finally(body, fin)
    }

    pub fn clamp(volts: f64) -> f64 {
        volts.clamp(Self::MIN_VOLTS, Self::MAX_VOLTS)
    }

    /// Walks the rail to `target_volts` one code step at a time, measuring after
    /// every step. Anything that lands off target puts the start-up code back and stops.
    pub fn set_volts(&self, target_volts: f64, log: Option<&dyn Fn(&str)>) -> Result<bool> {
        if !self.calibrated() && !self.calibrate(log)? {
            return Ok(false);
        }
        let Some(baseline) = self.baseline_code() else { return Ok(false) };

        let want = Self::clamp(target_volts);
        if (want - target_volts).abs() > 0.0005 {
            if let Some(log) = log {
                log(&format!(
                    "  {target_volts:.3} V is outside {:.3}-{:.3}; using {want:.3}",
                    Self::MIN_VOLTS,
                    Self::MAX_VOLTS
                ));
            }
        }

        let Some(mut code) = self.read_code()? else {
            self.set_status("the VDD2 regulator did not answer");
            return Ok(false);
        };
        let Some(mut volts) = self.measure_rail(5) else {
            self.set_status("the VDD2 rail could not be measured");
            return Ok(false);
        };

        let step_volts = self.measured_step_volts();
        let mut guard = 0;
        while (want - volts).abs() > step_volts * 0.6 && {
            let g = guard;
            guard += 1;
            g < 2 * Self::MAX_STEPS_FROM_BASELINE
        } {
            let dir: i32 = if want > volts { 1 } else { -1 };
            let next = code as i32 + dir;
            if !(0..=0xFF).contains(&next) {
                self.set_status("the VDD2 code is at the end of its range");
                break;
            }
            if (next - baseline as i32).abs() > Self::MAX_STEPS_FROM_BASELINE {
                self.set_status(format!(
                    "stopped {} steps from where VDD2 started, which is as far as this will go",
                    Self::MAX_STEPS_FROM_BASELINE
                ));
                break;
            }

            let aim = volts + dir as f64 * step_volts;
            if !self.write_code(next as u8)? {
                self.set_status("write failed");
                self.restore()?;
                return Ok(false);
            }
            native::sleep_ms(SETTLE_MS);

            let Some(got) = self.measure_rail(5) else {
                self.set_status("could not measure the rail after the write");
                self.restore()?;
                return Ok(false);
            };
            if let Some(log) = log {
                log(&format!("  code 0x{next:02X}: aimed {aim:.3} -> measured {got:.3} V"));
            }

            // Checked against the measurement, not only the request, so a wrong scale cannot walk
            // the rail past the clamp unnoticed.
            if got > Self::MAX_VOLTS + Self::TOLERANCE_VOLTS || got < Self::MIN_VOLTS - Self::TOLERANCE_VOLTS {
                self.set_status(format!("a step took the rail to {got:.3} V, outside the safe window; restored"));
                self.restore()?;
                return Ok(false);
            }
            if (got - aim).abs() > Self::TOLERANCE_VOLTS {
                self.set_status(format!("a step aimed at {aim:.3} V landed at {got:.3}; restored"));
                self.restore()?;
                return Ok(false);
            }

            code = next as u8;
            volts = got;
        }

        self.set_status(format!("CPU VDD2 {volts:.3} V (code 0x{code:02X})"));
        Ok(true)
    }
}
