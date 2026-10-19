//! Read-only register audit and explicit, separate hardware-validation commands.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, bail};

use super::intel_cpu::IntelCpu;
use super::native::{self, lock};
use super::oc_mailbox::OcMailbox;
use super::super_io::SuperIo;
use super::winring0::WinRing0Driver;
use super::KernelDriver;

/// A report that is rewritten after every line, so a crash mid-test still leaves the evidence.
pub(crate) struct Report {
    path: std::path::PathBuf,
    text: Mutex<String>,
}

impl Report {
    pub(crate) fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), text: Mutex::new(String::new()) }
    }

    pub(crate) fn log(&self, line: &str) {
        let mut t = lock(&self.text);
        t.push_str(line);
        t.push_str("\r\n");
        let _ = std::fs::write(&self.path, t.as_bytes());
    }
}

/// Local time in .NET's round-trip ("O") format.
pub(crate) fn now_o() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.7f%:z").to_string()
}

fn tf(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

fn join(v: &[i32]) -> String {
    v.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",")
}

fn open_driver() -> Result<Arc<dyn KernelDriver>> {
    Ok(Arc::new(WinRing0Driver::open()?))
}

/// True when another copy of this program is running.
pub(crate) fn other_instance_running() -> bool {
    native::same_name_process_count().is_some_and(|n| n > 1)
}

/// Busy-works one thread for 600 ms and returns the highest value `sample` reports after 200 ms.
fn peak_under_load(cpu_index: i32, sample: impl Fn() -> i32) -> Result<i32> {
    native::run_on_cpu(cpu_index, || {
        let timer = Instant::now();
        let mut peak = 0;
        let mut work = 1.001f64;
        while timer.elapsed().as_millis() < 600 {
            for _ in 0..300000 {
                work = (work + 1.00001).sqrt();
            }
            if timer.elapsed().as_millis() > 200 {
                peak = peak.max(sample());
            }
        }
        std::hint::black_box(work);
        peak
    })
}

pub fn validate_e_core(path: &Path, target: i32) -> i32 {
    if !(8..=44).contains(&target) {
        println!("Specified argument was out of the range of valid values. (Parameter 'target')");
        return 1;
    }
    let report = Report::new(path);
    let log = |t: &str| report.log(t);
    if other_instance_running() {
        log("Validation refused: close other Roch CPU instances.");
        return 1;
    }
    let (driver, cpu) = match open_driver().and_then(|d| Ok((d.clone(), IntelCpu::new(d)?))) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{e:?}"));
            return 1;
        }
    };
    let _ = &driver;
    if cpu.e_core_count() == 0 {
        log("No E-cores detected.");
        return 1;
    }
    let before = match cpu.read_e_core_turbo_table() {
        Ok(t) => t.0,
        Err(e) => {
            log(&format!("{e:?}"));
            return 1;
        }
    };
    let baseline = before.iter().copied().filter(|r| *r > 0).min().unwrap_or(0);
    if baseline == 0 || target > baseline + 1 {
        log("Test refused: no usable baseline or target is more than one step above the current E-core ceiling.");
        return 1;
    }
    let mut pass = false;
    log(&format!(
        "E-core validation {}; target x{target}; original {}; hypervisor {}",
        now_o(),
        join(&before),
        tf(cpu.hypervisor_present())
    ));
    let r: Result<()> = (|| {
        let peak = || peak_under_load(cpu.first_e_thread(), || cpu.read_perf_status(cpu.first_e_thread()).map(|p| p.0).unwrap_or(0));
        let lower = 8.max(baseline.min(target) - 2);
        cpu.write_e_core_all_core_ratio(lower)?;
        let lower_peak = peak()?;
        let lower_pass = cpu.read_e_core_all_core_ratio()? == lower && lower_peak == lower;
        log(&format!(
            "E-core lower ceiling x{lower}; active peak x{lower_peak}: {}",
            if lower_pass { "PASS" } else { "FAIL" }
        ));
        cpu.write_e_core_all_core_ratio(target)?;
        let p = peak()?;
        pass = lower_pass && cpu.read_e_core_all_core_ratio()? == target && p == target;
        log(&format!(
            "E-core target read back x{}; active peak x{p}: {}",
            cpu.read_e_core_all_core_ratio()?,
            if pass { "PASS" } else { "Target clock not demonstrated" }
        ));
        Ok(())
    })();
    if let Err(e) = r {
        log(&format!("E-core: FAIL {e}"));
    }
    let restored: Result<bool> = (|| {
        if cpu.read_e_core_turbo_table()?.0 != before {
            cpu.write_e_core_turbo_table(&before)?;
        }
        Ok(cpu.read_e_core_turbo_table()?.0 == before)
    })();
    let restored = match restored {
        Ok(ok) => {
            log(&if ok { format!("Original E-core table verified: {}", join(&before)) } else { "RESTORE FAILED".into() });
            ok
        }
        Err(e) => {
            log(&format!("RESTORE FAILED: {e}"));
            false
        }
    };
    if pass && restored { 0 } else { 2 }
}

/// Tests the architectural ring register independently of the board's OC mailbox.
pub fn validate_ring(path: &Path) -> i32 {
    let report = Report::new(path);
    let log = |t: &str| report.log(t);
    if other_instance_running() {
        log("Validation refused: close all other Roch CPU instances before changing hardware.");
        return 1;
    }
    let (driver, cpu) = match open_driver().and_then(|d| Ok((d.clone(), IntelCpu::new(d)?))) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{e:?}"));
            return 1;
        }
    };
    let last_error = || driver.last_error().unwrap_or_default();
    let p = cpu.first_p_thread();
    let Some(before) = driver.read_msr(IntelCpu::MSR_UNCORE_RATIO_LIMIT, p) else {
        log(&format!("Ring snapshot failed: {}", last_error()));
        return 1;
    };
    let peak = || peak_under_load(p, || cpu.read_current_ring_ratio());
    let mut pass = false;
    let r: Result<Option<i32>> = (|| {
        let baseline = peak()?;
        let target = 4.max(((before & 0x7F) as i32).min(baseline) - 2);
        if baseline <= target {
            log("No usable ring baseline; no changes made.");
            return Ok(Some(1));
        }
        let min = (((before >> 8) & 0x7F) as i32).min(target);
        let request = (before & !0x7F7Fu64) | target as u64 | ((min as u64) << 8);
        log(&format!(
            "Ring-only validation {}: original 0x{before:016X}, active peak x{baseline}, test ceiling x{target}",
            now_o()
        ));
        if !driver.write_msr(IntelCpu::MSR_UNCORE_RATIO_LIMIT, request, p) {
            bail!("Ring write failed: {}", last_error());
        }
        let Some(actual) = driver.read_msr(IntelCpu::MSR_UNCORE_RATIO_LIMIT, p) else {
            bail!("Ring readback failed: {}", last_error());
        };
        let pk = peak()?;
        pass = actual == request && pk > 0 && pk <= target;
        log(&format!(
            "Register 0x{actual:016X}; active peak x{pk}: {}",
            if pass { "PASS" } else { "FAIL - the requested ceiling was not demonstrated" }
        ));
        Ok(None)
    })();
    let early = match r {
        Ok(v) => v,
        Err(e) => {
            log(&e.to_string());
            None
        }
    };
    // The .NET version restored in a finally block, which also ran on the early return.
    let restored = driver.write_msr(IntelCpu::MSR_UNCORE_RATIO_LIMIT, before, p)
        && driver.read_msr(IntelCpu::MSR_UNCORE_RATIO_LIMIT, p) == Some(before);
    log(&if restored {
        format!("Original ring register restored: 0x{before:016X}")
    } else {
        format!("RESTORE FAILED: {}", last_error())
    });
    if let Some(code) = early {
        return code;
    }
    if pass && restored { 0 } else { 2 }
}

/// Briefly lowers the P-core ceiling and checks it under single-thread load; always restores snapshots.
pub fn validate(path: &Path, volts: f64) -> i32 {
    if !volts.is_finite() || !(0.6..=1.35).contains(&volts) {
        println!("Specified argument was out of the range of valid values. (Parameter 'volts')");
        return 1;
    }
    let report = Report::new(path);
    let log = |t: &str| report.log(t);
    if other_instance_running() {
        log("Validation refused: close all other Roch CPU instances before changing hardware.");
        return 1;
    }
    let (driver, cpu) = match open_driver().and_then(|d| Ok((d.clone(), IntelCpu::new(d)?))) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{e:?}"));
            return 1;
        }
    };
    let mut failures = 0;
    log(&format!("Intel validation {}; hypervisor: {}", now_o(), tf(cpu.hypervisor_present())));
    let (sensor, sensor_status) = SuperIo::try_create(driver.clone());
    log(&format!("Vcore sensor: {sensor_status}"));
    let p = cpu.first_p_thread();

    let sample_loaded_voltage = |label: &str| -> Result<()> {
        native::run_on_cpu(p, || {
            let timer = Instant::now();
            let (mut vid_min, mut vid_max, mut rail_min, mut rail_max) = (f64::INFINITY, 0.0f64, f64::INFINITY, 0.0f64);
            let (mut rail_sum, mut rail_count, mut work) = (0.0, 0, 1.001f64);
            let mut next_sample = 1000u128;
            while timer.elapsed().as_millis() < 3000 {
                for _ in 0..300000 {
                    work = (work + 1.00001).sqrt();
                }
                if timer.elapsed().as_millis() < next_sample {
                    continue;
                }
                next_sample = timer.elapsed().as_millis() + 200; // Allow the monitor to refresh between samples.
                let vid = cpu.read_perf_status(p).map(|v| v.1).unwrap_or(0.0);
                if vid > 0.0 {
                    vid_min = vid_min.min(vid);
                    vid_max = vid_max.max(vid);
                }
                if let Some(rail) = sensor.as_ref().and_then(|s| s.read_vcore()).filter(|r| *r > 0.4 && *r < 1.9) {
                    rail_min = rail_min.min(rail);
                    rail_max = rail_max.max(rail);
                    rail_sum += rail;
                    rail_count += 1;
                }
            }
            std::hint::black_box(work);
            log(&format!(
                "{label}: VID {}; measured Vcore {}.",
                if vid_min.is_finite() { format!("{vid_min:.3}–{vid_max:.3} V") } else { "unavailable".into() },
                if rail_count > 0 {
                    format!("{rail_min:.3}–{rail_max:.3} V, mean {:.3} V ({rail_count} samples)", rail_sum / rail_count as f64)
                } else {
                    "unavailable".into()
                }
            ));
        })
    };
    let peak_ratio = || peak_under_load(p, || cpu.read_perf_status(p).map(|v| v.0).unwrap_or(0));

    let r: Result<()> = (|| {
        let (before_p, _) = cpu.read_p_core_turbo_table()?;
        let before_core = cpu.mailbox().read_domain(OcMailbox::DOMAIN_CORE)?;
        let baseline = peak_ratio()?;
        let target = 8.max(before_p.iter().copied().min().unwrap_or(0).min(baseline) - 2);
        log(&format!("P-core baseline: table {}, active peak x{baseline}; test ceiling x{target}", join(&before_p)));
        let pr: Result<()> = (|| {
            cpu.write_p_core_all_core_ratio(target)?;
            let peak = peak_ratio()?;
            let read = cpu.read_p_core_all_core_ratio()?;
            let pass = read == target && peak <= target && peak > 0 && baseline > target;
            log(&format!(
                "P-core: table x{read}, active peak x{peak}: {}",
                if pass { "PASS" } else { "FAIL - actual clock did not demonstrate the requested ceiling" }
            ));
            if !pass {
                failures += 1;
            }
            Ok(())
        })();
        if let Err(e) = pr {
            log(&format!("P-core: FAIL {e}"));
            failures += 1;
        }
        cpu.write_p_core_turbo_table(&before_p)?;
        log(&format!("P-core table restored: {}", join(&cpu.read_p_core_turbo_table()?.0)));

        if cpu.e_core_count() > 0 {
            let (before_e, _) = cpu.read_e_core_turbo_table()?;
            match cpu.write_e_core_turbo_table(&before_e) {
                Ok(()) => log(&format!("E-core existing-value write/readback: PASS on E thread {}", cpu.first_e_thread())),
                Err(e) => {
                    log(&format!("E-core existing-value write: FAIL {e}"));
                    failures += 1;
                }
            }
        }
        let vr: Result<()> = (|| {
            sample_loaded_voltage("Before override")?;
            cpu.mailbox().set_override(OcMailbox::DOMAIN_CORE, volts)?;
            log(&format!("Core voltage target readback: {}", cpu.mailbox().read_domain(OcMailbox::DOMAIN_CORE)?));
            sample_loaded_voltage("With override")?;
            log("VID is a request. Only the separate Vcore sensor measures the rail; readback success alone does not verify it.");
            Ok(())
        })();
        if let Err(e) = vr {
            log(&format!("Core voltage: FAIL {e}"));
            failures += 1;
        }
        cpu.mailbox().write_domain(OcMailbox::DOMAIN_CORE, &before_core)?;
        log(&format!("Core mailbox restored: {}", cpu.mailbox().read_domain(OcMailbox::DOMAIN_CORE)?));
        sample_loaded_voltage("After restore")?;
        Ok(())
    })();
    if let Err(e) = r {
        log(&format!("STOPPED: {e}"));
        failures += 1;
    }
    log(&format!("Result: {failures} failed checks."));
    if failures == 0 { 0 } else { 2 }
}

pub fn run(path: &Path) -> i32 {
    let report = Report::new(path);
    let log = |t: &str| report.log(t);
    let r: Result<()> = (|| {
        let driver = open_driver()?;
        let cpu = IntelCpu::new(driver.clone())?;
        log(&format!("Intel audit {}: {}", now_o(), cpu.brand_string()));
        log(&format!("Hypervisor present: {}", tf((native::cpuid(1, 0).2 >> 31) != 0)));
        log(&format!(
            "P thread {}, E thread {}; {}P + {}E",
            cpu.first_p_thread(),
            cpu.first_e_thread(),
            cpu.p_core_count(),
            cpu.e_core_count()
        ));
        let registers: [u32; 14] = [0x150, 0x194, 0x198, 0x199, 0x1AD, 0x1AE, 0x1A0, 0x620, 0x621, 0x650, 0x651, 0x770, 0x771, 0x774];
        let mut seen_cores = Vec::new();
        let mut threads = Vec::new();
        for c in cpu.logical_cpus().iter().filter(|c| c.is_p_core) {
            if !seen_cores.contains(&c.core_id) {
                seen_cores.push(c.core_id);
                threads.push(c.clone());
            }
        }
        threads.extend(cpu.logical_cpus().iter().filter(|c| !c.is_p_core).cloned());
        for core in &threads {
            log(&format!("CPU {} {} core {}:", core.index, if core.is_p_core { "P" } else { "E" }, core.core_id));
            for reg in registers {
                log(&match driver.read_msr(reg, core.index) {
                    Some(value) => format!("  0x{reg:03X}: 0x{value:016X}"),
                    None => format!("  0x{reg:03X}: read failed ({})", driver.last_error().unwrap_or_default()),
                });
            }
        }
        for thread in [cpu.first_p_thread(), cpu.first_e_thread()].into_iter().filter(|t| *t >= 0) {
            let mailbox = OcMailbox::new(driver.clone(), thread);
            for domain in 0..6u8 {
                match mailbox.execute(OcMailbox::CMD_READ_VF, domain, 0, 0) {
                    Ok((status, data)) => log(&format!(
                        "CPU {thread} mailbox domain {domain}: status 0x{status:02X}, data 0x{data:08X}, {}",
                        OcMailbox::decode(data)
                    )),
                    Err(e) => log(&format!("CPU {thread} mailbox domain {domain}: {e}")),
                }
            }
        }
        Ok(())
    })();
    match r {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("{e:?}"));
            1
        }
    }
}
