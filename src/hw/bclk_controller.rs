//! Sets the base clock, through [`EcClockGen`], with every step measured.

use std::sync::Mutex;

use anyhow::Result;

use super::ec_clock_gen::EcClockGen;
use super::native;
use super::super_io::with_finally;

/// How long to let the PLL settle after a write before measuring.
const SETTLE_MS: u64 = 60;

/// Fraction bits in the divider, i.e. the width of the tuning word the part uses.
const FRACTION_BITS: i32 = 28;
const FRACTION_SCALE: f64 = (1u32 << FRACTION_BITS) as f64;
const FRACTION_MASK: u32 = (1u32 << FRACTION_BITS) - 1;

struct BclkState {
    vco_mhz: f64,
    calibrated: bool,
    baseline: Option<Vec<u8>>,
    baseline_mhz: f64,
    status: String,
}

/// Sets the base clock, through [`EcClockGen`], with every step measured.
///
/// **The model.** The clock generator is a fractional-N PLL: a fixed oscillator of about
/// 10002 MHz divided by `D = block[0] + fraction/2^28`, where the fraction is the 28-bit
/// tuning word. So the base clock is `Fvco / D`, and both fields run backwards against
/// frequency. The oscillator is measured on this board by [`BclkController::calibrate`] rather
/// than assumed, and every step is then checked against a real measurement anyway.
///
/// **Why the guards are here.** Writing a clock generator is the one adjustment in this
/// program where a wrong value does not produce a wrong reading, it stops the machine. There is no
/// ceiling on what may be asked for - where a given board gives up is not something this can know -
/// but a change is never applied blind: steps of at most [`BclkController::MAX_STEP_MHZ`]; a
/// measurement against the ACPI timer, which does not move with the base clock, after every single
/// step; and a restore of the block found at start-up the moment one lands somewhere it was not
/// aimed. A request the meter cannot confirm is refused rather than believed.
pub struct BclkController {
    clock_gen: EcClockGen,
    measure: Box<dyn Fn() -> Option<f64> + Send + Sync>,
    state: Mutex<BclkState>,
}

fn opt3(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_default()
}

impl BclkController {
    /// The widest base clock this can express and still check, in MHz. **This is not a safety
    /// ceiling.** There is no view here about what your board tolerates: where that limit falls
    /// depends on the board, the memory, the cache ratio and how far the CPU is already pushed,
    /// and only the person at the machine knows it. Every board tested so far gave up well below
    /// the top of this range.
    ///
    /// What the bounds are is the range in which a result can still be believed: the meter treats
    /// anything outside it as a failed reading rather than a real clock, so a step landing there
    /// is treated as a fault and rolled back. Widening them past what the meter can validate would
    /// not remove a restriction, it would remove the check.
    ///
    /// The protection that remains is the same as it was: small steps, each one measured, and the
    /// start-up value put back the moment one lands somewhere it was not aimed.
    pub const MAX_BCLK_MHZ: f64 = 200.0;
    pub const MIN_BCLK_MHZ: f64 = 50.0;

    /// Above this, applying from the window asks for confirmation first - the same treatment a
    /// voltage outside its usual daily-use range gets. It stops a typo, not a decision.
    pub const CONFIRM_ABOVE_MHZ: f64 = 105.0;

    /// Largest single change, in MHz. The divider latches all at once, so this is not about
    /// avoiding a half-applied value - it is about not asking the PLL for a big jump, and about
    /// bounding how far a wrong model could take the clock before the check after the step
    /// notices and puts everything back.
    pub const MAX_STEP_MHZ: f64 = 0.5;

    /// How far a step may land from where it was aimed before the change is treated as failed.
    /// Repeat measurements of a steady clock spread about 0.03 MHz, so this is a few times that.
    pub const TOLERANCE_MHZ: f64 = 0.12;

    pub fn new(clock_gen: EcClockGen, measure: Box<dyn Fn() -> Option<f64> + Send + Sync>) -> BclkController {
        BclkController {
            clock_gen,
            measure,
            state: Mutex::new(BclkState {
                // The oscillator the divider divides, in MHz. Measured; the seed is only there so
                // the first probe is roughly the right size.
                vco_mhz: 10002.3,
                calibrated: false,
                baseline: None,
                baseline_mhz: 0.0,
                status: "not started".to_string(),
            }),
        }
    }

    /// The oscillator the divider divides, in MHz.
    pub fn vco_mhz(&self) -> f64 {
        native::lock(&self.state).vco_mhz
    }

    pub fn calibrated(&self) -> bool {
        native::lock(&self.state).calibrated
    }

    /// The block as it was found at start-up.
    pub fn baseline(&self) -> Option<Vec<u8>> {
        native::lock(&self.state).baseline.clone()
    }

    /// The clock that went with the start-up block.
    pub fn baseline_mhz(&self) -> f64 {
        native::lock(&self.state).baseline_mhz
    }

    pub fn status(&self) -> String {
        native::lock(&self.state).status.clone()
    }

    fn set_status(&self, s: impl Into<String>) {
        native::lock(&self.state).status = s.into();
    }

    // ---------------------------------------------------------------- divider <-> block
    pub fn divider_of(block: &[u8]) -> f64 {
        block[0] as f64 + (EcClockGen::get_tuning(block) & FRACTION_MASK) as f64 / FRACTION_SCALE
    }

    /// Rebuilds a block for a divider, leaving the bits the part ignores as they were.
    pub fn block_for(block: &[u8], divider: f64) -> Vec<u8> {
        let divider = divider.clamp(1.0, 255.999);
        let whole = divider.floor() as u8;
        let frac = ((divider - whole as f64) * FRACTION_SCALE).round_ties_even().clamp(0.0, FRACTION_MASK as f64) as u32;
        let word = (EcClockGen::get_tuning(block) & !FRACTION_MASK) | frac;
        let mut copy = EcClockGen::with_tuning(block, word);
        copy[0] = whole;
        copy
    }

    pub fn mhz_for(&self, divider: f64) -> f64 {
        self.vco_mhz() / divider
    }

    pub fn divider_for_mhz(&self, mhz: f64) -> f64 {
        self.vco_mhz() / mhz
    }

    // ---------------------------------------------------------------- measurement
    /// Median of several measurements, discarding any that fail.
    fn measure(&self, samples: i32) -> Option<f64> {
        let mut vals = Vec::new();
        for _ in 0..samples {
            if let Some(v) = (self.measure)() {
                if v >= Self::MIN_BCLK_MHZ && v <= Self::MAX_BCLK_MHZ {
                    vals.push(v);
                }
            }
        }
        if vals.is_empty() {
            return None;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(vals[vals.len() / 2])
    }

    /// Reads the current block and the clock it produces (C# default `samples` is 3).
    pub fn read(&self, samples: i32) -> Result<Option<(Vec<u8>, f64)>> {
        let Some(blk) = self.clock_gen.read_block()? else { return Ok(None) };
        let Some(mhz) = self.measure(samples) else { return Ok(None) };
        Ok(Some((blk, mhz)))
    }

    /// Records where the machine started, so any change can be undone. Runs at start-up, so it
    /// takes a single measurement: the figure is for reporting and for the row's default, and
    /// every path that depends on the clock being known accurately measures it again.
    pub fn capture_baseline(&self) -> Result<bool> {
        let Some((block, mhz)) = self.read(1)? else {
            self.set_status("clock generator did not answer");
            return Ok(false);
        };
        let mut s = native::lock(&self.state);
        s.status = format!("baseline {} at {mhz:.3} MHz", EcClockGen::describe(&block));
        s.baseline = Some(block);
        s.baseline_mhz = mhz;
        Ok(true)
    }

    /// Puts back the block found at start-up, slow side first.
    pub fn restore(&self) -> Result<bool> {
        let Some(baseline) = self.baseline() else { return Ok(false) };
        let ok = self.write_safely(&baseline)?;
        let mhz = self.baseline_mhz();
        self.set_status(if ok { format!("restored to {mhz:.3} MHz") } else { "restore FAILED".to_string() });
        Ok(ok)
    }

    /// The divider takes effect all at once, because the part latches it when the last byte of the
    /// block arrives - see [`EcClockGen::write_block`], which is why nothing here has to
    /// worry about a half-applied divider driving the clock.
    fn write_safely(&self, target: &[u8]) -> Result<bool> {
        self.clock_gen.write_block(target)
    }

    // ---------------------------------------------------------------- calibration
    /// Establishes the oscillator from a single measurement. The divider is known exactly - it is
    /// just the block - so `Fvco = f * D` needs nothing else, and this costs one read and one
    /// measurement rather than a write-probe-restore round trip.
    ///
    /// It deliberately does not confirm the model by moving the clock. [`BclkController::set_bclk`]
    /// checks every step against a measurement and restores on any mismatch, so a wrong scale is
    /// caught on the first step having moved the clock by at most [`BclkController::MAX_STEP_MHZ`] -
    /// the same protection the probe gave, without making every apply wait for it.
    pub fn establish_scale(&self, block: &[u8], mhz: f64, log: Option<&dyn Fn(&str)>) -> bool {
        let divider = Self::divider_of(block);
        if divider < 1.0 || mhz < Self::MIN_BCLK_MHZ - 5.0 || mhz > Self::MAX_BCLK_MHZ + 5.0 {
            self.set_status(format!("implausible starting point ({mhz:.3} MHz at divider {divider:.3})"));
            return false;
        }
        let vco = mhz * divider;
        {
            let mut s = native::lock(&self.state);
            s.vco_mhz = vco;
            s.calibrated = true;
        }
        if let Some(log) = log {
            log(&format!("  {mhz:.3} MHz at divider {divider:.6} -> oscillator {vco:.1} MHz"));
        }
        self.set_status(format!(
            "oscillator {vco:.1} MHz, range {:.1}-{:.1} MHz",
            Self::MIN_BCLK_MHZ,
            Self::MAX_BCLK_MHZ
        ));
        true
    }

    /// Establishes the oscillator and then confirms it by moving the clock to a second divider,
    /// so the relation is shown to be a division rather than something that merely looked like one
    /// over a short span. This is the diagnostic form; the apply path uses
    /// [`BclkController::establish_scale`] and leans on its per-step check instead. The probe moves
    /// the clock down, which is the safe direction, and the block is put back either way.
    pub fn calibrate(&self, log: Option<&dyn Fn(&str)>) -> Result<bool> {
        if self.baseline().is_none() && !self.capture_baseline()? {
            return Ok(false);
        }
        let start = self.baseline().unwrap();

        let Some(start_mhz) = self.measure(4) else {
            self.set_status("could not measure the base clock");
            return Ok(false);
        };
        if !self.establish_scale(&start, start_mhz, log) {
            return Ok(false);
        }
        let vco = self.vco_mhz();

        // A quarter of a MHz slower: a bigger divider. Comfortably above the noise, and downwards
        // so that a wrong model costs speed rather than stability.
        let probe_div = vco / (start_mhz - 0.25);
        let probe = Self::block_for(&start, probe_div);
        if let Some(log) = log {
            log(&format!("  probing divider {:.6} (aiming for {:.3} MHz)", Self::divider_of(&probe), start_mhz - 0.25));
        }

        let body = (|| -> Result<bool> {
            if !self.write_safely(&probe)? {
                self.set_status("probe write failed");
                return Ok(false);
            }
            native::sleep_ms(150);
            let Some(probed) = self.measure(5) else {
                self.set_status("probe could not be measured");
                return Ok(false);
            };
            if let Some(log) = log {
                log(&format!("  measured {probed:.3} MHz"));
            }

            if probed > Self::MAX_BCLK_MHZ || probed < Self::MIN_BCLK_MHZ {
                self.set_status(format!(
                    "the probe measured {probed:.3} MHz, which the meter cannot confirm - refusing to tune"
                ));
                return Ok(false);
            }
            if (probed - (start_mhz - 0.25)).abs() > Self::TOLERANCE_MHZ {
                self.set_status(format!(
                    "a step aimed at {:.3} MHz landed at {probed:.3} - the divider model does not hold here",
                    start_mhz - 0.25
                ));
                return Ok(false);
            }

            // Both points agree on the oscillator, so average them.
            let averaged = (vco + probed * Self::divider_of(&probe)) / 2.0;
            {
                let mut s = native::lock(&self.state);
                s.vco_mhz = averaged;
                s.calibrated = true;
            }
            self.set_status(format!(
                "oscillator {averaged:.1} MHz, range {:.1}-{:.1} MHz",
                Self::MIN_BCLK_MHZ,
                Self::MAX_BCLK_MHZ
            ));
            Ok(true)
        })();
        let fin = (|| -> Result<()> {
            self.write_safely(&start)?;
            native::sleep_ms(150);
            if let Some(log) = log {
                log(&format!("  restored: {} MHz", opt3(self.measure(3))));
            }
            Ok(())
        })();
        with_finally(body, fin)
    }

    // ---------------------------------------------------------------- diagnostics
    /// Writes a series of dividers and measures each, restoring the baseline afterwards whatever
    /// happens. For characterising a board this has not been measured on. Aborts the run if any
    /// measurement lands outside what the meter can confirm.
    pub fn probe_dividers(&self, dividers: &[f64], log: Option<&dyn Fn(&str)>) -> Result<bool> {
        if self.baseline().is_none() && !self.capture_baseline()? {
            return Ok(false);
        }
        let start = self.baseline().unwrap();
        if let Some(log) = log {
            log(&format!("  baseline divider {:.6} at {:.3} MHz", Self::divider_of(&start), self.baseline_mhz()));
        }
        let body = (|| -> Result<bool> {
            for &d in dividers {
                let attempt = Self::block_for(&start, d);
                if !self.write_safely(&attempt)? {
                    self.set_status("probe write failed");
                    return Ok(false);
                }
                native::sleep_ms(150);
                let got = self.measure(4);
                if let Some(log) = log {
                    log(&format!(
                        "  divider {:>11.6}  predicted {:>8.3}  measured {} MHz",
                        Self::divider_of(&attempt),
                        self.mhz_for(Self::divider_of(&attempt)),
                        opt3(got)
                    ));
                }
                if let Some(m) = got {
                    if m > Self::MAX_BCLK_MHZ || m < Self::MIN_BCLK_MHZ {
                        self.set_status(format!("probe measured {m:.3} MHz, which the meter cannot confirm - stopping"));
                        return Ok(false);
                    }
                }
            }
            self.set_status("probe complete");
            Ok(true)
        })();
        let fin = (|| -> Result<()> {
            self.write_safely(&start)?;
            native::sleep_ms(150);
            if let Some(log) = log {
                log(&format!("  restored: {} MHz", opt3(self.measure(3))));
            }
            Ok(())
        })();
        with_finally(body, fin)
    }

    pub fn clamp(mhz: f64) -> f64 {
        mhz.clamp(Self::MIN_BCLK_MHZ, Self::MAX_BCLK_MHZ)
    }

    // ---------------------------------------------------------------- apply
    /// Walks the clock to `target_mhz` in small steps, measuring each one. Any
    /// step that does not land where it was aimed puts the start-up block back and stops.
    pub fn set_bclk(&self, target_mhz: f64, log: Option<&dyn Fn(&str)>) -> Result<bool> {
        let clamped = Self::clamp(target_mhz);
        if (clamped - target_mhz).abs() > 0.0005 {
            if let Some(log) = log {
                log(&format!(
                    "  {target_mhz:.3} MHz is outside what can be measured ({:.1}-{:.1}); using {clamped:.3}",
                    Self::MIN_BCLK_MHZ,
                    Self::MAX_BCLK_MHZ
                ));
            }
        }

        // One read and one measurement serve both as the starting point and, the first time, as
        // the whole of the calibration.
        let Some((mut block, mut mhz)) = self.read(3)? else {
            self.set_status("clock generator did not answer");
            return Ok(false);
        };
        if !self.calibrated() && !self.establish_scale(&block, mhz, log) {
            return Ok(false);
        }

        // Enough iterations to cover the distance asked for, with slack, rather than a fixed
        // count: without a ceiling the walk can be long, and a loop that ran out partway would
        // stop somewhere in the middle and report success.
        let mut guard = 0;
        let max_steps = ((clamped - mhz).abs() / Self::MAX_STEP_MHZ) as i32 + 8;
        while (clamped - mhz).abs() > Self::TOLERANCE_MHZ / 2.0 && {
            let g = guard;
            guard += 1;
            g < max_steps
        } {
            let next = mhz + (clamped - mhz).clamp(-Self::MAX_STEP_MHZ, Self::MAX_STEP_MHZ);
            let attempt = Self::block_for(&block, self.divider_for_mhz(next));
            let last = (next - clamped).abs() <= Self::TOLERANCE_MHZ / 2.0;

            if !self.write_safely(&attempt)? {
                self.set_status("write failed");
                self.restore()?;
                return Ok(false);
            }
            native::sleep_ms(SETTLE_MS);

            // Intermediate steps only have to show the clock is tracking; the one that lands on
            // the target gets the more careful reading.
            let Some(got) = self.measure(if last { 3 } else { 2 }) else {
                self.set_status("could not measure after the write");
                self.restore()?;
                return Ok(false);
            };
            if let Some(log) = log {
                log(&format!("  aimed {next:.3} -> measured {got:.3} MHz"));
            }

            // Against the measurement, not the request: a reading outside the range the meter can
            // validate is a failed reading, not a clock, so treat it as a fault rather than
            // believing it.
            if got > Self::MAX_BCLK_MHZ + Self::TOLERANCE_MHZ || got < Self::MIN_BCLK_MHZ - Self::TOLERANCE_MHZ {
                self.set_status(format!(
                    "a step measured {got:.3} MHz, which is outside what the meter can confirm; restored"
                ));
                self.restore()?;
                return Ok(false);
            }
            if (got - next).abs() > Self::TOLERANCE_MHZ {
                self.set_status(format!("a step aimed at {next:.3} MHz landed at {got:.3}; restored"));
                self.restore()?;
                return Ok(false);
            }
            block = attempt;
            mhz = got;
        }

        if (clamped - mhz).abs() > Self::TOLERANCE_MHZ {
            self.set_status(format!("stopped at {mhz:.3} MHz, short of {clamped:.3}"));
            return Ok(false);
        }
        self.set_status(format!("base clock {mhz:.3} MHz"));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_and_block_round_trip() {
        let block = [100u8, 0, 0, 0, 0, 0, 0, 0xF0];
        assert_eq!(BclkController::divider_of(&block), 100.0);
        let b = BclkController::block_for(&block, 99.5);
        assert_eq!(b[0], 99);
        assert_eq!(EcClockGen::get_tuning(&b), 0xF000_0000 | (1 << 27));
        assert!((BclkController::divider_of(&b) - 99.5).abs() < 1e-9);
    }
}
