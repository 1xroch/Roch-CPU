//! One DDR5 DIMM: its SPD5118 hub (0x50 + slot) and its on-module PMIC (0x48 + slot).
//! Voltages are set on the PMIC itself, so this works on any board whose SMBus controller
//! (Intel PCH or AMD FCH) reaches the DIMM slots. It does not depend on the motherboard VRM.
//!
//! Register scales differ between PMICs (the VDD rail on overclocking kits uses a
//! 10 mV step instead of the JEDEC 5 mV step). Because a wrong scale on a write could
//! double a DIMM voltage, every rail is calibrated against the PMIC's own ADC at
//! start-up and writes are refused for rails whose scale could not be confirmed.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use super::Smbus;
use super::native::{self, lock};
use super::pmic_voltage_measurement::PmicVoltageMeasurement;

const ADC_RAIL_VOLTS_PER_LSB: f64 = 0.015; // 15 mV / LSB for SWA..SWD
const CALIBRATION_TOLERANCE_V: f64 = 0.060; // ADC accuracy + ripple

/// Mutable state (the C# private setters), behind one mutex so the DIMM is `Send + Sync`.
struct DimmState {
    has_pmic: bool,
    pmic_vendor: String,
    vdd_step_mv: i32,
    vddq_step_mv: i32,
    vpp_verified: bool,
    adc_writable: bool,
    calibration_note: String,
    vdd_measurement: Arc<PmicVoltageMeasurement>,
    vddq_measurement: Arc<PmicVoltageMeasurement>,
    vpp_measurement: Arc<PmicVoltageMeasurement>,
}

/// One DDR5 DIMM: its SPD5118 hub (0x50 + slot) and its on-module PMIC (0x48 + slot).
///
/// Register scales differ between PMICs (the VDD rail on overclocking kits uses a
/// 10 mV step instead of the JEDEC 5 mV step). Because a wrong scale on a write could
/// double a DIMM voltage, every rail is calibrated against the PMIC's own ADC at
/// start-up and writes are refused for rails whose scale could not be confirmed.
pub struct Ddr5Dimm {
    bus: Arc<dyn Smbus>,
    slot: i32,
    slot_name: String,
    state: Mutex<DimmState>,
}

fn decode(raw: u8, base_mv: i32, step_mv: i32) -> f64 {
    (base_mv + step_mv * (raw >> 1) as i32) as f64 / 1000.0
}

fn encode(volts: f64, base_mv: i32, step_mv: i32) -> u8 {
    // Math.Round is round-half-to-even.
    let code = ((volts * 1000.0 - base_mv as f64) / step_mv as f64).round_ties_even() as i32;
    (code.clamp(0, 127) << 1) as u8
}

fn join_measurement_errors(previous: Option<String>, next: &str) -> Option<String> {
    Some(match previous {
        None => next.to_string(),
        Some(p) => format!("{p} {next}"),
    })
}

impl Ddr5Dimm {
    pub const SPD_BASE: u8 = 0x50;
    pub const PMIC_BASE: u8 = 0x48;

    // PMIC5000 (JESD301) registers
    pub const R_SWA_PWR: u8 = 0x0C; // 125 mW / LSB
    pub const R_SWB_PWR: u8 = 0x0D;
    pub const R_SWC_PWR: u8 = 0x0E;
    pub const R_SWD_PWR: u8 = 0x0F;
    pub const R_SWA_VOUT: u8 = 0x21; // VDD  : bits 7:1, 800 mV base, 5 or 10 mV per step (calibrated)
    pub const R_SWB_VOUT: u8 = 0x23; // VDD second phase (0 = follows SWA)
    pub const R_SWC_VOUT: u8 = 0x25; // VDDQ : bits 7:1, 800 mV base, 5 or 10 mV per step (calibrated)
    pub const R_SWD_VOUT: u8 = 0x27; // VPP  : bits 7:1, 1500 mV base, 5 mV per step
    pub const R_ADC_ENABLE: u8 = 0x30; // bit 7 = enable, bits 6:3 = input select
    pub const R_ADC_READ: u8 = 0x31;
    pub const R_VENDOR_LSB: u8 = 0x3B;
    pub const R_VENDOR_MSB: u8 = 0x3C;
    pub const R_REVISION: u8 = 0x3D;

    // ADC input selections (verified on Raptor Lake / Z790 with a JEDEC-compliant PMIC)
    pub const ADC_SWA: i32 = 0;
    pub const ADC_SWB: i32 = 1;
    pub const ADC_SWC: i32 = 2;
    pub const ADC_SWD: i32 = 3;
    pub const ADC_VIN_BULK: i32 = 5;

    pub const VDD_MIN_V: f64 = 0.800;
    pub const VDD_MAX_V: f64 = 1.800;
    pub const VDDQ_MIN_V: f64 = 0.800;
    pub const VDDQ_MAX_V: f64 = 1.500;
    pub const VPP_MIN_V: f64 = 1.500;
    pub const VPP_MAX_V: f64 = 2.135;

    pub fn new(bus: Arc<dyn Smbus>, slot: i32) -> Ddr5Dimm {
        let slot_name = match slot {
            0 => "DIMMA1".to_string(),
            1 => "DIMMA2".to_string(),
            2 => "DIMMB1".to_string(),
            3 => "DIMMB2".to_string(),
            _ => format!("DIMM{slot}"),
        };
        Ddr5Dimm {
            bus,
            slot,
            slot_name,
            state: Mutex::new(DimmState {
                has_pmic: false,
                pmic_vendor: "?".to_string(),
                vdd_step_mv: 0,
                vddq_step_mv: 0,
                vpp_verified: false,
                adc_writable: false,
                calibration_note: "not calibrated".to_string(),
                vdd_measurement: Arc::new(PmicVoltageMeasurement::not_read("PMIC ADC SWA")),
                vddq_measurement: Arc::new(PmicVoltageMeasurement::not_read("PMIC ADC SWC")),
                vpp_measurement: Arc::new(PmicVoltageMeasurement::not_read("PMIC ADC SWD")),
            }),
        }
    }

    pub fn slot(&self) -> i32 {
        self.slot
    }
    pub fn slot_name(&self) -> &str {
        &self.slot_name
    }
    pub fn spd_address(&self) -> u8 {
        (Self::SPD_BASE as i32 + self.slot) as u8
    }
    pub fn pmic_address(&self) -> u8 {
        (Self::PMIC_BASE as i32 + self.slot) as u8
    }
    pub fn has_pmic(&self) -> bool {
        lock(&self.state).has_pmic
    }
    pub fn pmic_vendor(&self) -> String {
        lock(&self.state).pmic_vendor.clone()
    }

    /// mV per register step of the VDD (SWA) rail; 0 until calibrated.
    pub fn vdd_step_mv(&self) -> i32 {
        lock(&self.state).vdd_step_mv
    }
    pub fn vdd_verified(&self) -> bool {
        self.vdd_step_mv() != 0
    }
    /// mV per register step of the VDDQ (SWC) rail; 0 until calibrated. JEDEC says 5 mV, but the
    /// PMIC on a G.Skill DDR5-6000 kit (vendor 8A12) decodes to 1.120 V at 5 mV while its ADC and
    /// HWiNFO both read 1.440 V: a 10 mV step, like the VDD rail on overclocking kits.
    pub fn vddq_step_mv(&self) -> i32 {
        lock(&self.state).vddq_step_mv
    }
    pub fn vddq_verified(&self) -> bool {
        self.vddq_step_mv() != 0
    }
    pub fn vpp_verified(&self) -> bool {
        lock(&self.state).vpp_verified
    }

    /// The highest VDD / VDDQ the register can hold at the calibrated step: 7 bits above 800 mV, so
    /// 1.435 V at the JEDEC 5 mV step and 2.070 V at 10 mV (capped at the rail's own maximum).
    /// Anything above it used to be clamped to the top code without a word.
    pub fn vdd_ceiling_v(&self) -> f64 {
        let step = self.vdd_step_mv();
        if step != 0 { Self::VDD_MAX_V.min(decode(0xFE, 800, step)) } else { Self::VDD_MAX_V }
    }
    pub fn vddq_ceiling_v(&self) -> f64 {
        let step = self.vddq_step_mv();
        if step != 0 { Self::VDDQ_MAX_V.min(decode(0xFE, 800, step)) } else { Self::VDDQ_MAX_V }
    }

    /// Why a rail stops at 1.435 V. Seen on a G.Skill kit (PMIC 8A12): with the BIOS at 1.41 V the PMIC
    /// runs the 5 mV step; with the BIOS above 1.435 V it runs a 10 mV step and 1.5 V could be set here.
    pub fn step_limit_note(step_mv: i32) -> &'static str {
        if step_mv == 5 {
            "At the JEDEC 5 mV step this PMIC's register tops out at 1.435 V. The BIOS switches it to the 10 mV step when DRAM voltage is set above 1.435 V there; do that once, then higher values can be set here."
        } else {
            ""
        }
    }

    pub fn adc_writable(&self) -> bool {
        lock(&self.state).adc_writable
    }
    pub fn calibration_note(&self) -> String {
        lock(&self.state).calibration_note.clone()
    }
    /// Cached snapshot; no hardware access. The same `Arc` is returned until a new ADC attempt
    /// on that rail replaces it.
    pub fn vdd_measurement(&self) -> Arc<PmicVoltageMeasurement> {
        lock(&self.state).vdd_measurement.clone()
    }
    pub fn vddq_measurement(&self) -> Arc<PmicVoltageMeasurement> {
        lock(&self.state).vddq_measurement.clone()
    }
    pub fn vpp_measurement(&self) -> Arc<PmicVoltageMeasurement> {
        lock(&self.state).vpp_measurement.clone()
    }

    /// Probe all four slots; a DIMM is present when its SPD5118 hub answers with device type 0x51.
    pub fn probe(bus: Arc<dyn Smbus>) -> Vec<Ddr5Dimm> {
        let mut list = Vec::new();
        for s in 0..4 {
            let d = Ddr5Dimm::new(bus.clone(), s);
            if bus.read_byte(d.spd_address(), 0x00) != Some(0x51) {
                continue; // MR0 = 0x51 -> SPD5118 (DDR5)
            }
            let has_pmic = bus.read_byte(d.pmic_address(), Self::R_SWA_VOUT).is_some();
            lock(&d.state).has_pmic = has_pmic;
            if has_pmic {
                if let Some(vl) = bus.read_byte(d.pmic_address(), Self::R_VENDOR_LSB) {
                    if let Some(vh) = bus.read_byte(d.pmic_address(), Self::R_VENDOR_MSB) {
                        lock(&d.state).pmic_vendor = format!("ID {vh:02X}{vl:02X}");
                    }
                }
            }
            if has_pmic {
                d.calibrate();
            }
            list.push(d);
        }
        list
    }

    // ---------------------------------------------------------------- raw access
    pub fn read_register(&self, reg: u8) -> Option<u8> {
        self.bus.read_byte(self.pmic_address(), reg)
    }
    pub fn read_spd_register(&self, reg: u8) -> Option<u8> {
        self.bus.read_byte(self.spd_address(), reg)
    }

    /// Reads the PMIC ADC for one input selection, restoring the previous ADC configuration
    /// afterwards. Only the ADC mux is touched; regulator outputs are not affected.
    pub fn read_adc(&self, select: i32) -> Option<u8> {
        self.read_adc_core(select).0
    }

    /// (sample, error, sampled-at). The SMBus trait reports failures as values, so the .NET
    /// build's exception paths (driver errors thrown through the bus) do not exist here.
    fn read_adc_core(&self, select: i32) -> (Option<u8>, Option<String>, Option<DateTime<Utc>>) {
        if !self.has_pmic() {
            return (None, Some("No PMIC is available.".to_string()), None);
        }
        let pmic = self.pmic_address();
        let Some(saved) = self.bus.read_byte(pmic, Self::R_ADC_ENABLE) else {
            return (None, Some("PMIC ADC configuration could not be read.".to_string()), None);
        };
        let mut sampled_at_utc = None;
        let (value, mut error) = (|| {
            let cfg = (0x80 | ((select & 0xF) << 3)) as u8;
            if !self.bus.write_byte(pmic, Self::R_ADC_ENABLE, cfg) {
                return (None, Some("PMIC ADC input selection was rejected.".to_string()));
            }
            native::sleep_ms(15);
            if self.bus.read_byte(pmic, Self::R_ADC_ENABLE) != Some(cfg) {
                return (None, Some("PMIC ADC input selection did not read back.".to_string())); // not writable (secure mode)
            }
            let Some(v) = self.bus.read_byte(pmic, Self::R_ADC_READ) else {
                return (None, Some("PMIC ADC sample could not be read.".to_string()));
            };
            sampled_at_utc = Some(Utc::now());
            (Some(v), None)
        })();
        // finally: put the ADC configuration back
        if !self.bus.write_byte(pmic, Self::R_ADC_ENABLE, saved) {
            error = join_measurement_errors(error, "PMIC ADC configuration restore was rejected.");
        }
        (value, error, sampled_at_utc)
    }

    pub fn read_adc_volts(&self, select: i32) -> Option<f64> {
        let attempt_at_utc = Utc::now();
        let (raw, error, sampled_at_utc) = self.read_adc_core(select);
        let value = raw.map(|b| b as f64 * ADC_RAIL_VOLTS_PER_LSB);
        self.cache_measurement(select, value, sampled_at_utc, attempt_at_utc, error);
        value // Preserve existing calibration/verification return behavior, including failed restore.
    }

    fn cache_measurement(
        &self,
        select: i32,
        value: Option<f64>,
        sampled_at_utc: Option<DateTime<Utc>>,
        attempt_at_utc: DateTime<Utc>,
        error: Option<String>,
    ) {
        let mut st = lock(&self.state);
        let slot = match select {
            Self::ADC_SWA => &mut st.vdd_measurement,
            Self::ADC_SWC => &mut st.vddq_measurement,
            Self::ADC_SWD => &mut st.vpp_measurement,
            _ => return, // Unknown inputs never become a DIMM rail measurement.
        };
        let previous = slot.clone();
        let next = if error.is_none() && value.is_some() && sampled_at_utc.is_some() {
            PmicVoltageMeasurement::new(value, sampled_at_utc, Some(attempt_at_utc), None, previous.source.clone())
        } else {
            PmicVoltageMeasurement {
                last_attempt_at_utc: Some(attempt_at_utc),
                last_error: Some(error.unwrap_or_else(|| "PMIC ADC sample was not read.".to_string())),
                ..(*previous).clone()
            }
        };
        *slot = Arc::new(next);
    }

    // ---------------------------------------------------------------- calibration
    /// Compares each rail's register decode with the ADC and records which scales are trustworthy.
    pub fn calibrate(&self) {
        let has_pmic = {
            let mut st = lock(&self.state);
            st.vdd_step_mv = 0;
            st.vddq_step_mv = 0;
            st.vpp_verified = false;
            st.adc_writable = false;
            if !st.has_pmic {
                st.calibration_note = "no PMIC".to_string();
            }
            st.has_pmic
        };
        if !has_pmic {
            return;
        }
        let adc_a = self.read_adc_volts(Self::ADC_SWA);
        let adc_c = self.read_adc_volts(Self::ADC_SWC);
        let adc_d = self.read_adc_volts(Self::ADC_SWD);
        let (Some(adc_a), Some(adc_c), Some(adc_d)) = (adc_a, adc_c, adc_d) else {
            lock(&self.state).calibration_note =
                "PMIC ADC not writable (secure mode) - voltage scales unverified, writes disabled".to_string();
            return;
        };
        lock(&self.state).adc_writable = true;
        let regs = self.read_register(Self::R_SWA_VOUT).and_then(|ra| {
            self.read_register(Self::R_SWC_VOUT)
                .and_then(|rc| self.read_register(Self::R_SWD_VOUT).map(|rd| (ra, rc, rd)))
        });
        let Some((ra, rc, rd)) = regs else {
            lock(&self.state).calibration_note = "PMIC register read failed".to_string();
            return;
        };
        let vdd5 = decode(ra, 800, 5);
        let vdd10 = decode(ra, 800, 10);
        let m5 = (vdd5 - adc_a).abs() <= CALIBRATION_TOLERANCE_V;
        let m10 = (vdd10 - adc_a).abs() <= CALIBRATION_TOLERANCE_V;
        let mut vdd_step = 0;
        if m5 && !m10 {
            vdd_step = 5;
        } else if m10 && !m5 {
            vdd_step = 10;
        }
        let q5 = (decode(rc, 800, 5) - adc_c).abs() <= CALIBRATION_TOLERANCE_V;
        let q10 = (decode(rc, 800, 10) - adc_c).abs() <= CALIBRATION_TOLERANCE_V;
        let mut vddq_step = 0;
        if q5 && !q10 {
            vddq_step = 5;
        } else if q10 && !q5 {
            vddq_step = 10;
        }
        let vpp_verified = (decode(rd, 1500, 5) - adc_d).abs() <= CALIBRATION_TOLERANCE_V;
        let mut st = lock(&self.state);
        st.vdd_step_mv = vdd_step;
        st.vddq_step_mv = vddq_step;
        st.vpp_verified = vpp_verified;
        let vdd_text = if vdd_step != 0 { format!("{vdd_step} mV") } else { "unknown".to_string() };
        let vddq_text = if vddq_step != 0 { format!("{vddq_step} mV") } else { "unknown".to_string() };
        st.calibration_note = format!(
            "ADC: VDD {adc_a:.3} V, VDDQ {adc_c:.3} V, VPP {adc_d:.3} V -> VDD step {vdd_text}, VDDQ step {vddq_text}, VPP {}",
            if vpp_verified { "ok" } else { "mismatch" }
        );
    }

    fn read_rail(&self, reg: u8, base_mv: i32, step_mv: i32) -> Option<f64> {
        if !self.has_pmic() || step_mv <= 0 {
            return None;
        }
        self.bus.read_byte(self.pmic_address(), reg).map(|raw| decode(raw, base_mv, step_mv))
    }

    // ---------------------------------------------------------------- reads
    /// VDD set-point. Uses the calibrated step; falls back to the 5 mV JEDEC decode when unverified.
    pub fn read_vdd(&self) -> Option<f64> {
        let step = self.vdd_step_mv();
        self.read_rail(Self::R_SWA_VOUT, 800, if step != 0 { step } else { 5 })
    }
    pub fn read_vddq(&self) -> Option<f64> {
        let step = self.vddq_step_mv();
        self.read_rail(Self::R_SWC_VOUT, 800, if step != 0 { step } else { 5 })
    }
    pub fn read_vpp(&self) -> Option<f64> {
        self.read_rail(Self::R_SWD_VOUT, 1500, 5)
    }

    /// Power draw per rail in mW (SWA+SWB = VDD, SWC = VDDQ, SWD = VPP); `None` when not reported.
    /// Returns (vdd, vddq, vpp).
    pub fn read_power_mw(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        if !self.has_pmic() {
            return (None, None, None);
        }
        let pmic = self.pmic_address();
        let mut vdd = None;
        let mut vddq = None;
        let mut vpp = None;
        if let Some(a) = self.bus.read_byte(pmic, Self::R_SWA_PWR) {
            let mut v = a as f64 * 125.0;
            if let Some(b) = self.bus.read_byte(pmic, Self::R_SWB_PWR) {
                v += b as f64 * 125.0;
            }
            vdd = Some(v);
        }
        if let Some(c) = self.bus.read_byte(pmic, Self::R_SWC_PWR) {
            vddq = Some(c as f64 * 125.0);
        }
        if let Some(d) = self.bus.read_byte(pmic, Self::R_SWD_PWR) {
            vpp = Some(d as f64 * 125.0);
        }
        (vdd, vddq, vpp)
    }

    // ---------------------------------------------------------------- writes
    fn write_rail(&self, reg: u8, value: u8, name: &str, adc_select: i32, expected_volts: f64) -> Result<()> {
        let slot_name = &self.slot_name;
        if !self.has_pmic() {
            bail!("{slot_name}: no PMIC.");
        }
        let pmic = self.pmic_address();
        if !self.bus.write_byte(pmic, reg, value) {
            bail!("{slot_name}: SMBus write to PMIC register 0x{reg:02X} failed.");
        }
        native::sleep_ms(5);
        match self.bus.read_byte(pmic, reg) {
            Some(back) if (back & 0xFE) == (value & 0xFE) => {}
            other => {
                let back = other.unwrap_or(0);
                bail!(
                    "{slot_name}: PMIC rejected the new {name} setting (read back 0x{back:02X}). \
                     The PMIC is probably in Secure Mode (vendor locked); the voltage can then only be changed by the BIOS."
                );
            }
        }
        native::sleep_ms(20);
        let measured = self.read_adc_volts(adc_select);
        if let Some(m) = measured {
            if (m - expected_volts).abs() > CALIBRATION_TOLERANCE_V + 0.03 {
                bail!(
                    "{slot_name}: {name} register accepted but the ADC measures {m:.3} V instead of {expected_volts:.3} V. \
                     Check the value with another monitoring tool before going further."
                );
            }
        }
        Ok(())
    }

    pub fn write_vdd(&self, volts: f64) -> Result<()> {
        let slot_name = &self.slot_name;
        let step = self.vdd_step_mv();
        if step == 0 {
            bail!("{slot_name}: VDD scale not verified against the PMIC ADC; write refused for safety.");
        }
        let ceiling = self.vdd_ceiling_v();
        if volts < Self::VDD_MIN_V || volts > ceiling {
            let msg = format!("VDD must be {:.3}-{ceiling:.3} V. {}", Self::VDD_MIN_V, Self::step_limit_note(step));
            bail!("{}", msg.trim_end());
        }
        let v = encode(volts, 800, step);
        self.write_rail(Self::R_SWA_VOUT, v, "VDD", Self::ADC_SWA, decode(v, 800, step))?;
        // Second VDD phase (SWB) mirrors SWA on dual-rail PMICs; 0 means it follows SWA and must be left alone.
        if let Some(swb) = self.bus.read_byte(self.pmic_address(), Self::R_SWB_VOUT) {
            if swb != 0 {
                self.bus.write_byte(self.pmic_address(), Self::R_SWB_VOUT, v);
            }
        }
        Ok(())
    }

    pub fn write_vddq(&self, volts: f64) -> Result<()> {
        let slot_name = &self.slot_name;
        let step = self.vddq_step_mv();
        if step == 0 {
            bail!("{slot_name}: VDDQ scale not verified against the PMIC ADC; write refused for safety.");
        }
        let ceiling = self.vddq_ceiling_v();
        if volts < Self::VDDQ_MIN_V || volts > ceiling {
            let msg = format!("VDDQ must be {:.3}-{ceiling:.3} V. {}", Self::VDDQ_MIN_V, Self::step_limit_note(step));
            bail!("{}", msg.trim_end());
        }
        let v = encode(volts, 800, step);
        self.write_rail(Self::R_SWC_VOUT, v, "VDDQ", Self::ADC_SWC, decode(v, 800, step))
    }

    pub fn write_vpp(&self, volts: f64) -> Result<()> {
        let slot_name = &self.slot_name;
        if !self.vpp_verified() {
            bail!("{slot_name}: VPP scale not verified against the PMIC ADC; write refused for safety.");
        }
        if volts < Self::VPP_MIN_V || volts > Self::VPP_MAX_V {
            bail!("VPP must be {:.3}-{:.3} V.", Self::VPP_MIN_V, Self::VPP_MAX_V);
        }
        let v = encode(volts, 1500, 5);
        self.write_rail(Self::R_SWD_VOUT, v, "VPP", Self::ADC_SWD, decode(v, 1500, 5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::collections::HashMap;

    // ------------------------------------------------------------------------------------------
    // PmicVoltageMeasurement.Tests: the simulated PMIC at 0x48 with a bus-operation trace. The C#
    // suite compared the trace against a frozen pre-cache copy of this class; the traces below
    // are what that baseline produces for the same operations.

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Failure {
        None,
        FailInitialRead,
        FailSelectionWrite,
        FailSelectionReadBack,
        FailAdcRead,
        FailRestoreWrite,
        // ThrowAdcRead / ThrowRestoreWrite are not ported: the Smbus trait has no way to throw.
    }

    struct FakeBusState {
        registers: HashMap<u8, u8>,
        trace: Vec<String>,
        next_failure: Failure,
    }

    struct FakeBus(Mutex<FakeBusState>);

    impl FakeBus {
        fn new() -> Arc<FakeBus> {
            let registers = HashMap::from([(0x30, 0), (0x21, 120), (0x23, 0), (0x25, 110), (0x27, 120)]);
            Arc::new(FakeBus(Mutex::new(FakeBusState { registers, trace: Vec::new(), next_failure: Failure::None })))
        }
        fn with_failure(failure: Failure) -> Arc<FakeBus> {
            let b = FakeBus::new();
            b.set_failure(failure);
            b
        }
        fn set_failure(&self, failure: Failure) {
            lock(&self.0).next_failure = failure;
        }
        fn trace(&self) -> Vec<String> {
            lock(&self.0).trace.clone()
        }
        fn clear_trace(&self) {
            lock(&self.0).trace.clear();
        }
    }

    fn consume(st: &mut FakeBusState, failure: Failure) -> bool {
        if st.next_failure != failure {
            return false;
        }
        st.next_failure = Failure::None;
        true
    }

    impl Smbus for FakeBus {
        fn description(&self) -> String {
            "Simulated PMIC; no native hardware".to_string()
        }
        fn read_byte(&self, address: u8, command: u8) -> Option<u8> {
            let mut st = lock(&self.0);
            st.trace.push(format!("R {address:02X}:{command:02X}"));
            assert_eq!(address, 0x48, "Unexpected simulated SMBus address");
            let adc_cfg = st.registers[&0x30];
            if command == 0x30 && adc_cfg == 0 && consume(&mut st, Failure::FailInitialRead) {
                return None;
            }
            if command == 0x30 && adc_cfg != 0 && consume(&mut st, Failure::FailSelectionReadBack) {
                return None;
            }
            if command == 0x31 {
                if consume(&mut st, Failure::FailAdcRead) {
                    return None;
                }
                let select = (adc_cfg >> 3) & 0xF;
                let r = |k: u8| st.registers.get(&k).copied().unwrap_or(0) as i32;
                let volts = match select {
                    0 => (800 + 10 * (r(0x21) >> 1)) as f64 / 1000.0,
                    2 => (800 + 10 * (r(0x25) >> 1)) as f64 / 1000.0,
                    3 => (1500 + 5 * (r(0x27) >> 1)) as f64 / 1000.0,
                    _ => 1.2,
                };
                return Some((volts / 0.015).round_ties_even() as u8);
            }
            Some(st.registers.get(&command).copied().unwrap_or(0))
        }
        fn write_byte(&self, address: u8, command: u8, value: u8) -> bool {
            let mut st = lock(&self.0);
            st.trace.push(format!("W {address:02X}:{command:02X}={value:02X}"));
            assert_eq!(address, 0x48, "Unexpected simulated SMBus address");
            if command == 0x30 && value != 0 && consume(&mut st, Failure::FailSelectionWrite) {
                return false;
            }
            if command == 0x30 && value == 0 && consume(&mut st, Failure::FailRestoreWrite) {
                return false;
            }
            st.registers.insert(command, value);
            true
        }
        fn read_word(&self, _address: u8, _command: u8) -> Option<u16> {
            panic!("Unexpected simulated word read");
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn present(dimm: &Ddr5Dimm) {
        lock(&dimm.state).has_pmic = true;
    }

    fn dimm_on(bus: &Arc<FakeBus>) -> Ddr5Dimm {
        let d = Ddr5Dimm::new(bus.clone(), 0);
        present(&d);
        d
    }

    /// The legacy ADC sequence: save config, select, read back, sample, restore.
    fn adc_trace(cfg: u8) -> Vec<String> {
        vec!["R 48:30".into(), format!("W 48:30={cfg:02X}"), "R 48:30".into(), "R 48:31".into(), "W 48:30=00".into()]
    }

    fn calibration_trace() -> Vec<String> {
        let mut t = Vec::new();
        t.extend(adc_trace(0x80));
        t.extend(adc_trace(0x90));
        t.extend(adc_trace(0x98));
        t.extend(["R 48:21".to_string(), "R 48:25".to_string(), "R 48:27".to_string()]);
        t
    }

    #[test]
    fn calibration_trace_identical_and_cache_holds_actual_adc_values_not_pmic_targets() {
        let bus = FakeBus::new();
        let after = dimm_on(&bus);
        after.calibrate();
        assert_eq!(bus.trace(), calibration_trace(), "Calibration added/reordered a bus operation");
        assert!(
            after.vdd_step_mv() == 10
                && after.vddq_step_mv() == 10
                && after.vpp_verified()
                && after.calibration_note()
                    == "ADC: VDD 1.395 V, VDDQ 1.350 V, VPP 1.800 V -> VDD step 10 mV, VDDQ step 10 mV, VPP ok",
            "Calibration behavior changed: {}",
            after.calibration_note()
        );
        assert!(
            (after.vdd_measurement().last_successful_volts.unwrap() - 1.395).abs() < 0.000001 && after.read_vdd() == Some(1.4),
            "Measured VDD was substituted with its programmed target"
        );
        assert!(
            (after.vddq_measurement().last_successful_volts.unwrap() - 1.35).abs() < 0.000001
                && (after.vpp_measurement().last_successful_volts.unwrap() - 1.8).abs() < 0.000001,
            "ADC rail mappings are incorrect"
        );
        let m = after.vdd_measurement();
        assert!(
            m.latest_attempt_succeeded() && m.sampled_at_utc.is_some() && m.source == "PMIC ADC SWA",
            "Actual observation metadata is missing"
        );
    }

    #[test]
    fn existing_voltage_write_verification_trace_unchanged_and_updates_only_the_measured_rail() {
        let bus = FakeBus::new();
        let after = dimm_on(&bus);
        after.calibrate();
        bus.clear_trace();
        let old_q = after.vddq_measurement();
        let old_p = after.vpp_measurement();
        let old_a = after.vdd_measurement();
        after.write_vdd(1.44).unwrap();
        let mut expected = vec!["W 48:21=80".to_string(), "R 48:21".to_string()];
        expected.extend(adc_trace(0x80));
        expected.push("R 48:23".to_string());
        assert_eq!(bus.trace(), expected, "Apply verification added/reordered a bus operation");
        let a = after.vdd_measurement();
        assert!(
            (a.last_successful_volts.unwrap() - 1.44).abs() < 0.000001 && a.sampled_at_utc > old_a.sampled_at_utc,
            "Existing verification observation was not captured"
        );
        assert!(
            Arc::ptr_eq(&old_q, &after.vddq_measurement()) && Arc::ptr_eq(&old_p, &after.vpp_measurement()),
            "VDD observation contaminated another rail"
        );
    }

    #[test]
    fn cache_getters_have_zero_bus_access_and_retain_original_timestamps() {
        let bus = FakeBus::new();
        let dimm = dimm_on(&bus);
        dimm.calibrate();
        bus.clear_trace();
        let observed = dimm.vdd_measurement();
        for _ in 0..100 {
            assert!(
                Arc::ptr_eq(&observed, &dimm.vdd_measurement())
                    && dimm.vddq_measurement().source == "PMIC ADC SWC"
                    && dimm.vpp_measurement().source == "PMIC ADC SWD",
                "Reading cache mutated a record"
            );
        }
        assert!(
            bus.trace().is_empty() && dimm.vdd_measurement().sampled_at_utc == observed.sampled_at_utc,
            "Getter polled hardware or made a sample look fresh"
        );
    }

    fn check_adc_trace(failure: Failure, expected_trace: Vec<String>, expected_value: Option<f64>) {
        let bus = FakeBus::with_failure(failure);
        let after = dimm_on(&bus);
        let value = after.read_adc_volts(0);
        assert_eq!(value, expected_value, "Legacy ADC return semantics changed ({failure:?})");
        assert_eq!(bus.trace(), expected_trace, "ADC transport operation order/count changed ({failure:?})");
        if failure != Failure::None {
            let m = after.vdd_measurement();
            assert!(
                !m.latest_attempt_succeeded() && m.last_error.is_some(),
                "Failure was presented as a successful measured sample ({failure:?})"
            );
        }
    }

    const VDD_ADC: Option<f64> = Some(93.0 * ADC_RAIL_VOLTS_PER_LSB);

    #[test]
    fn legacy_adc_trace_return_preserved_none() {
        check_adc_trace(Failure::None, adc_trace(0x80), VDD_ADC);
    }

    #[test]
    fn legacy_adc_trace_return_preserved_fail_initial_read() {
        check_adc_trace(Failure::FailInitialRead, vec!["R 48:30".into()], None);
    }

    #[test]
    fn legacy_adc_trace_return_preserved_fail_selection_write() {
        check_adc_trace(
            Failure::FailSelectionWrite,
            vec!["R 48:30".into(), "W 48:30=80".into(), "W 48:30=00".into()],
            None,
        );
    }

    #[test]
    fn legacy_adc_trace_return_preserved_fail_selection_read_back() {
        check_adc_trace(
            Failure::FailSelectionReadBack,
            vec!["R 48:30".into(), "W 48:30=80".into(), "R 48:30".into(), "W 48:30=00".into()],
            None,
        );
    }

    #[test]
    fn legacy_adc_trace_return_preserved_fail_adc_read() {
        check_adc_trace(Failure::FailAdcRead, adc_trace(0x80), None);
    }

    #[test]
    fn legacy_adc_trace_return_preserved_fail_restore_write() {
        check_adc_trace(Failure::FailRestoreWrite, adc_trace(0x80), VDD_ADC);
    }

    #[test]
    fn latest_failure_preserves_old_successful_value_time_but_is_marked_not_read() {
        let bus = FakeBus::new();
        let dimm = dimm_on(&bus);
        dimm.calibrate();
        let before = dimm.vddq_measurement();
        let untouched = dimm.vdd_measurement();
        bus.set_failure(Failure::FailAdcRead);
        assert!(dimm.read_adc_volts(2).is_none(), "Failed observation fabricated a value");
        let failed = dimm.vddq_measurement();
        assert!(
            !failed.latest_attempt_succeeded()
                && failed.last_successful_volts == before.last_successful_volts
                && failed.sampled_at_utc == before.sampled_at_utc
                && failed.last_attempt_at_utc > before.last_attempt_at_utc
                && failed.last_error.is_some(),
            "Failed cache lost history or appeared current"
        );
        assert!(Arc::ptr_eq(&untouched, &dimm.vdd_measurement()), "Failure leaked to another rail");
        assert!(
            dimm.read_adc_volts(2).is_some()
                && dimm.vddq_measurement().latest_attempt_succeeded()
                && dimm.vddq_measurement().last_error.is_none(),
            "Later genuine sample did not clear error"
        );
    }

    #[test]
    fn failed_config_restoration_retains_last_good_sample_and_records_failure_without_altering_return() {
        let bus = FakeBus::new();
        let dimm = dimm_on(&bus);
        dimm.calibrate();
        let before = dimm.vpp_measurement();
        bus.set_failure(Failure::FailRestoreWrite);
        assert!(dimm.read_adc_volts(3).is_some(), "Cache changed the established ADC return behavior");
        let m = dimm.vpp_measurement();
        assert!(
            !m.latest_attempt_succeeded()
                && m.last_error.as_deref().unwrap().contains("restore")
                && m.sampled_at_utc == before.sampled_at_utc,
            "Failed restore was shown as a new valid observation"
        );
    }

    #[test]
    fn unknown_adc_inputs_are_excluded_from_all_dimm_rail_caches() {
        let bus = FakeBus::new();
        let dimm = dimm_on(&bus);
        dimm.calibrate();
        let a = dimm.vdd_measurement();
        let c = dimm.vddq_measurement();
        let d = dimm.vpp_measurement();
        dimm.read_adc_volts(1);
        dimm.read_adc_volts(5);
        dimm.read_adc_volts(99);
        assert!(
            Arc::ptr_eq(&a, &dimm.vdd_measurement())
                && Arc::ptr_eq(&c, &dimm.vddq_measurement())
                && Arc::ptr_eq(&d, &dimm.vpp_measurement()),
            "Another ADC input was relabelled as a rail"
        );
    }

    #[test]
    fn missing_pmic_remains_not_read_and_performs_no_bus_operation() {
        let bus = FakeBus::new();
        let dimm = Ddr5Dimm::new(bus.clone(), 0);
        let m = dimm.vdd_measurement();
        assert!(
            !m.latest_attempt_succeeded()
                && m.sampled_at_utc.is_none()
                && dimm.read_adc_volts(0).is_none()
                && bus.trace().is_empty(),
            "Missing PMIC fabricated telemetry or accessed hardware"
        );
    }

    #[test]
    fn set_point_reads_do_not_change_measurement_values_or_timestamps() {
        let bus = FakeBus::new();
        let dimm = dimm_on(&bus);
        dimm.calibrate();
        let measurement = dimm.vdd_measurement();
        bus.clear_trace();
        dimm.read_vdd();
        assert!(
            Arc::ptr_eq(&measurement, &dimm.vdd_measurement()) && bus.trace() == vec!["R 48:21".to_string()],
            "Target read refreshed measured telemetry or added ADC access"
        );
    }

    // ------------------------------------------------------------------------------------------
    // RochPower.Tests: one DDR5 DIMM in slot A2: SPD5118 hub at 0x51 and a PMIC at 0x49 whose
    // VDD/VDDQ step is fixed.

    struct FakePmicBus {
        pmic: Mutex<[u8; 256]>,
        step_mv: i32,
    }

    impl FakePmicBus {
        fn new(step_mv: i32, vdd: f64) -> Arc<FakePmicBus> {
            let mut pmic = [0u8; 256];
            pmic[Ddr5Dimm::R_SWA_VOUT as usize] = Self::code(vdd, 800, step_mv);
            pmic[Ddr5Dimm::R_SWC_VOUT as usize] = Self::code(vdd, 800, step_mv);
            pmic[Ddr5Dimm::R_SWD_VOUT as usize] = Self::code(1.8, 1500, 5);
            Arc::new(FakePmicBus { pmic: Mutex::new(pmic), step_mv })
        }
        fn code(v: f64, base_mv: i32, step: i32) -> u8 {
            ((((v * 1000.0 - base_mv as f64) / step as f64).round_ties_even() as i32) << 1) as u8
        }
        fn volts(raw: u8, base_mv: i32, step: i32) -> f64 {
            (base_mv + step * (raw >> 1) as i32) as f64 / 1000.0
        }
        fn reg(&self, r: u8) -> u8 {
            lock(&self.pmic)[r as usize]
        }
    }

    impl Smbus for FakePmicBus {
        fn description(&self) -> String {
            "fake PMIC bus".to_string()
        }
        fn read_byte(&self, address: u8, command: u8) -> Option<u8> {
            if address == 0x51 {
                return Some(if command == 0 { 0x51 } else { 0 });
            }
            if address != 0x49 {
                return None;
            }
            let pmic = lock(&self.pmic);
            if command == Ddr5Dimm::R_ADC_READ {
                let select = ((pmic[Ddr5Dimm::R_ADC_ENABLE as usize] >> 3) & 0xF) as i32;
                let v = match select {
                    Ddr5Dimm::ADC_SWA => Self::volts(pmic[Ddr5Dimm::R_SWA_VOUT as usize], 800, self.step_mv),
                    Ddr5Dimm::ADC_SWC => Self::volts(pmic[Ddr5Dimm::R_SWC_VOUT as usize], 800, self.step_mv),
                    Ddr5Dimm::ADC_SWD => Self::volts(pmic[Ddr5Dimm::R_SWD_VOUT as usize], 1500, 5),
                    _ => 0.0,
                };
                return Some((v / 0.015).round_ties_even() as u8);
            }
            Some(pmic[command as usize])
        }
        fn write_byte(&self, address: u8, command: u8, value: u8) -> bool {
            if address != 0x49 {
                return false;
            }
            lock(&self.pmic)[command as usize] = value;
            true
        }
        fn read_word(&self, _address: u8, _command: u8) -> Option<u16> {
            None
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn single(bus: &Arc<FakePmicBus>) -> Ddr5Dimm {
        let mut found = Ddr5Dimm::probe(bus.clone());
        assert_eq!(found.len(), 1);
        found.remove(0)
    }

    #[test]
    fn ddr5_vdd_above_the_5_mv_step_ceiling_is_refused_not_clamped() {
        let bus = FakePmicBus::new(5, 1.410);
        let dimm = single(&bus);
        assert!(
            dimm.vdd_step_mv() == 5 && (dimm.vdd_ceiling_v() - 1.435).abs() < 1e-9,
            "5 mV step or its 1.435 V ceiling not detected"
        );
        let before = bus.reg(Ddr5Dimm::R_SWA_VOUT);
        assert!(dimm.write_vdd(1.5).is_err(), "Expected ArgumentOutOfRangeException");
        assert_eq!(bus.reg(Ddr5Dimm::R_SWA_VOUT), before, "Refused VDD request still wrote the PMIC");
        dimm.write_vdd(1.435).unwrap();
        assert!((dimm.read_vdd().unwrap() - 1.435).abs() < 1e-9, "Ceiling value not written");
    }

    #[test]
    fn ddr5_vdd_at_the_10_mv_step_reaches_1_5_v() {
        let bus = FakePmicBus::new(10, 1.450);
        let dimm = single(&bus);
        assert!(
            dimm.vdd_step_mv() == 10 && (dimm.vdd_ceiling_v() - Ddr5Dimm::VDD_MAX_V).abs() < 1e-9,
            "10 mV step ceiling wrong"
        );
        dimm.write_vdd(1.5).unwrap();
        assert!((dimm.read_vdd().unwrap() - 1.5).abs() < 1e-9, "1.5 V not written at the 10 mV step");
    }

    #[test]
    fn refusal_message_carries_the_step_note() {
        let bus = FakePmicBus::new(5, 1.410);
        let dimm = single(&bus);
        let e = dimm.write_vdd(1.5).unwrap_err().to_string();
        assert_eq!(e, format!("VDD must be 0.800-1.435 V. {}", Ddr5Dimm::step_limit_note(5)));
        assert_eq!(dimm.slot_name(), "DIMMA2");
        assert_eq!(dimm.pmic_address(), 0x49);
    }
}
