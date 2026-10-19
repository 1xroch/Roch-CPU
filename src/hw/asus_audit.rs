//! ASUS control diagnostics and explicit hardware-validation commands.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::asus_board_control::{AsusBoardControl, SvidState};
use super::bclk_meter::BclkMeter;
use super::intel_audit::{Report, now_o, other_instance_running};
use super::intel_cpu::IntelCpu;
use super::native;
use super::oc_mailbox::OcMailbox;
use super::smbios::SmbiosInfo;
use super::winring0::WinRing0Driver;
use super::KernelDriver;
use crate::model::hardware_model::HardwareModel;

/// The .NET record's `ToString()`.
fn svid(s: &SvidState) -> String {
    format!("SvidState {{ OffsetIndex = {}, TargetIndex = {}, Mode = {} }}", s.offset_index, s.target_index, s.mode)
}

fn require_single_instance() -> Result<()> {
    if other_instance_running() {
        bail!("Close other Roch CPU instances before hardware validation.");
    }
    Ok(())
}

/// A model whose log is copied into the report.
fn model_logging_to(report: &Arc<Report>) -> HardwareModel {
    let model = HardwareModel::new();
    let r = report.clone();
    model.on_log(move |m| r.log(m));
    model.initialize();
    model
}

fn apply(model: &HardwareModel, id: &str, value: f64) -> Result<()> {
    if !model.apply(id, value) {
        bail!("{}", model.setting(id).and_then(|s| s.last_error).unwrap_or_default());
    }
    Ok(())
}

fn ratio_text(r: (i32, i32)) -> String {
    format!("({}, {})", r.0, r.1)
}

pub fn validate_ring_voltage(path: &Path) -> i32 {
    let report = Arc::new(Report::new(path));
    let log = |t: &str| report.log(t);
    log(&format!("ASUS Cache SVID validation {}", now_o()));
    if let Err(e) = require_single_instance() {
        log(&e.to_string());
        return 1;
    }
    let model = model_logging_to(&report);
    let (Some(asus), Some(cpu)) = (model.asus_control().filter(|a| a.baseline_ring().is_some()), model.cpu()) else {
        return 1;
    };
    let other_ids = [AsusBoardControl::CORE_ID, AsusBoardControl::SA_ID, AsusBoardControl::L2_ID];
    let snapshot: Result<(SvidState, HashMap<u32, SvidState>, (i32, i32))> = (|| {
        let before = asus.read_svid(AsusBoardControl::RING_ID)?;
        let mut others = HashMap::new();
        for id in other_ids {
            others.insert(id, asus.read_svid(id)?);
        }
        Ok((before, others, cpu.read_ring_ratio()?))
    })();
    let (before, others, ratio) = match snapshot {
        Ok(v) => v,
        Err(e) => {
            log(&format!("FAILED: {e}"));
            return 1;
        }
    };
    let mut pass = false;
    let mut touched = false;
    let r: Result<()> = (|| {
        log(&format!(
            "Starting cache {}; ring ratio {}; CPU mailbox {}",
            svid(&before),
            ratio_text(ratio),
            cpu.mailbox().read_domain(OcMailbox::DOMAIN_RING)?
        ));
        if model.settings().iter().any(|s| s.id == "cpu_vdd2" || s.id == "cpu_aux") {
            bail!("Unvalidated ASUS sensor labels are still exposed");
        }
        log("PASS: no incorrectly mapped VDD2/AUX rows");
        const TARGET: f64 = 1.300;
        match asus.read_core_target()? {
            Some(core) if (TARGET..=1.31).contains(&core) => {}
            _ => bail!("This diagnostic requires the existing 1.300 V core target; nothing changed"),
        }
        let wanted = AsusBoardControl::with_voltage_target(AsusBoardControl::RING_ID, before, TARGET)?;
        if wanted == before {
            bail!("Cache is already at the diagnostic target; nothing changed");
        }
        touched = true;
        apply(&model, "ring_v", TARGET)?;
        if asus.read_svid(AsusBoardControl::RING_ID)? != wanted {
            bail!("Cache full-state readback mismatch");
        }
        let mailbox = cpu.mailbox().read_domain(OcMailbox::DOMAIN_RING)?;
        log(&format!("CPU mailbox after Apply: {mailbox}; shared Vcore {:.3} V", asus.read_vcore()?));
        if !mailbox.override_mode || (mailbox.target_volts - TARGET).abs() > 1.0 / 1024.0 {
            bail!("ASUS target read back but CPU cache-domain target was not confirmed");
        }
        if cpu.read_ring_ratio()? != ratio {
            bail!("Ring ratio changed during voltage test");
        }
        for id in other_ids {
            if asus.read_svid(id)? != others[&id] {
                bail!("Unrelated voltage item 0x{id:08X} changed");
            }
        }
        pass = true;
        log("PASS: ASUS complete state and CPU cache-domain target verified; ring ratio and other voltage targets unchanged. No separate ring rail measurement is available.");
        Ok(())
    })();
    if let Err(e) = r {
        log(&format!("FAILED: {e}"));
    }
    if touched {
        let restore: Result<()> = (|| {
            asus.set_svid(before, AsusBoardControl::RING_ID)?;
            if asus.read_svid(AsusBoardControl::RING_ID)? != before {
                bail!("Cache restoration mismatch");
            }
            log(&format!(
                "Restored cache {}; CPU mailbox {}; ring ratio {}",
                svid(&before),
                cpu.mailbox().read_domain(OcMailbox::DOMAIN_RING)?,
                ratio_text(cpu.read_ring_ratio()?)
            ));
            for id in other_ids {
                if asus.read_svid(id)? != others[&id] {
                    bail!("Unrelated item 0x{id:08X} no longer matches baseline");
                }
            }
            Ok(())
        })();
        if let Err(e) = restore {
            pass = false;
            log(&format!("RESTORE FAILED: {e}"));
        }
    }
    if pass { 0 } else { 1 }
}

pub fn validate_rail(path: &Path, id: u32, target: f64) -> i32 {
    let report = Arc::new(Report::new(path));
    let log = |t: &str| report.log(t);
    log(&format!("ASUS rail validation {}; item=0x{id:08X}; target={target:.3} V", now_o()));
    if let Err(e) = require_single_instance() {
        log(&e.to_string());
        return 1;
    }
    if id != AsusBoardControl::SA_ID && id != AsusBoardControl::L2_ID {
        return 1;
    }
    let model = model_logging_to(&report);
    let Some(asus) = model.asus_control() else { return 1 };
    let mut before: Option<SvidState> = None;
    let mut pass = false;
    let mut touched = false;
    let r: Result<()> = (|| {
        let b = asus.read_svid(id)?;
        before = Some(b);
        let _ = AsusBoardControl::with_voltage_target(id, b, target)?;
        let sample = |phase: &str| -> Result<f64> {
            let mut values = Vec::new();
            for _ in 0..12 {
                let v = asus.read_rail_voltage(id)?;
                if !v.is_finite() || !(0.5..=1.8).contains(&v) {
                    bail!("Invalid ASUS rail sensor reading");
                }
                values.push(v);
                native::sleep_ms(250);
            }
            log(&format!(
                "{phase} samples: {} V",
                values.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", ")
            ));
            values.sort_by(|a, b| a.total_cmp(b));
            let median = (values[5] + values[6]) / 2.0;
            log(&format!("{phase} median {median:.3} V"));
            Ok(median)
        };
        let baseline = sample("Before")?;
        let row = if id == AsusBoardControl::SA_ID { "sa_v" } else { "ecore_v" };
        if (target - baseline).abs() < 0.04 {
            bail!("Target is too close to baseline for an independent physical-response test; nothing changed");
        }
        touched = true;
        apply(&model, row, target)?;
        let measured = sample("After")?;
        let direction = (measured - baseline).signum() == (target - baseline).signum();
        pass = direction && (measured - baseline).abs() >= 0.02 && (measured - target).abs() <= 0.075;
        if !pass {
            bail!("ASUS target read back but physical rail response was not verified");
        }
        log("PASS: complete state readback and board-sensor response");
        Ok(())
    })();
    if let Err(e) = r {
        pass = false;
        log(&format!("FAILED: {e}"));
    }
    if let (true, Some(b)) = (touched, before) {
        let restore: Result<()> = (|| {
            asus.set_svid(b, id)?;
            log(&format!("Restored {}; sensor {:.3} V", svid(&asus.read_svid(id)?), asus.read_rail_voltage(id)?));
            Ok(())
        })();
        if let Err(e) = restore {
            pass = false;
            log(&format!("RESTORE FAILED: {e}"));
        }
    }
    if pass { 0 } else { 1 }
}

pub fn validate_voltage(path: &Path) -> i32 {
    let report = Arc::new(Report::new(path));
    let log = |t: &str| report.log(t);
    log(&format!("ASUS voltage Apply validation {}", now_o()));
    if let Err(e) = require_single_instance() {
        log(&e.to_string());
        return 1;
    }
    let model = model_logging_to(&report);
    let Some(asus) = model.asus_control().filter(|a| a.baseline_svid().is_some()) else { return 1 };
    let before = match asus.read_svid(AsusBoardControl::CORE_ID) {
        Ok(b) => b,
        Err(e) => {
            log(&format!("FAILED: {e}"));
            return 1;
        }
    };
    let mut pass = false;
    let r: Result<()> = (|| {
        for target in [1.275, 1.300] {
            apply(&model, "core_v", target)?;
            let mut values = Vec::new();
            for _ in 0..8 {
                values.push(asus.read_vcore()?);
                native::sleep_ms(250);
            }
            log(&format!(
                "Target {target:.3} V; ASUS Vcore: {}",
                values.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", ")
            ));
        }
        pass = true;
        Ok(())
    })();
    if let Err(e) = r {
        log(&format!("FAILED: {e}"));
    }
    let restore: Result<()> = (|| {
        asus.set_svid(before, AsusBoardControl::CORE_ID)?;
        log(&format!("Starting ASUS mode/offset/target restored: {}", svid(&asus.read_svid(AsusBoardControl::CORE_ID)?)));
        Ok(())
    })();
    if let Err(e) = restore {
        pass = false;
        log(&format!("RESTORE FAILED: {e}"));
    }
    log(if pass { "PASS: app Apply and complete state restoration" } else { "FAIL" });
    if pass { 0 } else { 1 }
}

pub fn run(path: &Path, test_bclk: bool) -> i32 {
    let report = Report::new(path);
    let log = |t: &str| report.log(t);
    log(&format!("ASUS diagnostic {}; BCLK test={}", now_o(), if test_bclk { "True" } else { "False" }));
    let r: Result<i32> = (|| {
        if test_bclk {
            require_single_instance()?;
        }
        let driver: Arc<dyn KernelDriver> = Arc::new(WinRing0Driver::open()?);
        let cpu = IntelCpu::new(driver.clone())?;
        let meter = BclkMeter::new(driver.clone());
        let (asus, status) = AsusBoardControl::try_create(&SmbiosInfo::read().board_manufacturer);
        log(&status);
        let Some(asus) = asus else { return Ok(1) };
        log(&format!(
            "VRM raw {}; measured Vcore {:.3} V",
            asus.read_raw(AsusBoardControl::VRM_ID)?,
            asus.read_vcore()?
        ));
        let measure = || -> Option<f64> {
            let mut values: Vec<f64> = (0..5)
                .filter_map(|_| meter.measure_bclk_from_cycles(cpu.first_p_thread(), 250).ok().flatten())
                .collect();
            values.sort_by(|a, b| a.total_cmp(b));
            log(&format!(
                "BCLK cycle samples: {}",
                values.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", ")
            ));
            if values.len() >= 3 { Some(values[values.len() / 2]) } else { None }
        };
        let fmt = |v: Option<f64>| v.map(|x| format!("{x:.3}")).unwrap_or_default();
        log(&format!(
            "ASUS target {:.3} MHz; cycle measurement {} MHz; TSC reference {:.3} MHz",
            asus.read_bclk()?,
            fmt(measure()),
            meter.measure_bclk_mhz(cpu.base_ratio(), 60)?
        ));
        if !test_bclk {
            return Ok(0);
        }
        let set = asus.set_bclk(101.0, &measure, &log);
        let mut pass = false;
        if set.is_ok() {
            pass = true;
            log("101 MHz programmed target verified; physical clock followed the requested change (see measured values)");
        }
        let restored = asus.restore(&measure, &log);
        if restored.is_ok() {
            log(&format!("Restored target {:.3} MHz; measured {} MHz", asus.read_bclk()?, fmt(measure())));
        }
        set?;
        restored?;
        Ok(if pass { 0 } else { 1 })
    })();
    match r {
        Ok(code) => code,
        Err(e) => {
            log(&format!("FAILED: {e:?}"));
            1
        }
    }
}
