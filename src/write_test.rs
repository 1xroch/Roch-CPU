//! `--write-test`: every Intel tuning row, written on the real machine, its effect measured by
//! something other than the register it wrote, and the machine put back exactly as found.
//!
//! The window's "0 restores the start-up value" only knows the values this process found, and a
//! row write can touch more than its own register (an all-core ratio also sets every core's
//! ceiling). So this takes its own raw snapshot first - turbo tables on every thread, each
//! core's mailbox ceiling, the ring and power registers, the voltage domains' VF words and the
//! PMIC set-points - and writes that back at the end, whatever happened in between, then reads
//! it all again to prove it.
//!
//! Each change is small and in the safe direction where there is one: ratios one to four bins
//! down, power limits 10 W down, a 10 mV undervolt, SA +9 mV, DIMM rails one 15 mV ADC step up.
//! The only fixed core voltage is 20 mV above the VID this CPU already asks for under load.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::hw::KernelDriver;
use crate::hw::ddr5_dimm::Ddr5Dimm;
use crate::hw::intel_cpu::IntelCpu;
use crate::hw::native;
use crate::hw::oc_mailbox::OcMailbox;
use crate::hw::super_io::SuperIo;
use crate::model::hardware_model::HardwareModel;

const MSR_PERF_STATUS: u32 = 0x198;
const MSR_TURBO: u32 = 0x1AD;
const MSR_ATOM_TURBO: u32 = 0x650;
const MSR_RING: u32 = 0x620;
const MSR_RING_STATUS: u32 = 0x621;
const MSR_POWER: u32 = 0x610;
const DOMAINS: [i32; 3] = [OcMailbox::DOMAIN_CORE, OcMailbox::DOMAIN_RING, OcMailbox::DOMAIN_SA];

struct Report {
    text: String,
    passed: usize,
    failed: usize,
    noted: usize,
}

impl Report {
    fn line(&mut self, s: impl AsRef<str>) {
        let s = s.as_ref();
        println!("{s}");
        self.text.push_str(s);
        self.text.push('\n');
    }
    fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        if ok {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        self.line(format!("  [{}] {name}: {}", if ok { "PASS" } else { "FAIL" }, detail.as_ref()));
    }
    fn note(&mut self, name: &str, detail: impl AsRef<str>) {
        self.noted += 1;
        self.line(format!("  [NOTE] {name}: {}", detail.as_ref()));
    }
}

/// Everything the tests can change, raw.
#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    p_turbo: u64,
    e_turbo: Vec<(i32, u64)>,
    ring: u64,
    power: u64,
    ceilings: Vec<(i32, Option<i32>)>,
    domains: Vec<(i32, Option<u32>)>,
    dimms: Vec<(Option<f64>, Option<f64>, Option<f64>)>,
    /// Board regulator targets (mV) by row id, for the rails the board holds fixed.
    vr: Vec<(&'static str, u16)>,
}

#[cfg(target_os = "linux")]
fn vr_targets(hw: &HardwareModel) -> Vec<(&'static str, u16)> {
    use crate::hw::board_vr::VrRail;
    let Some(vr) = hw.board_vr() else { return Vec::new() };
    [("vr_vccsa", VrRail::VccSa), ("vr_vnnaon", VrRail::VnnAon)]
        .into_iter()
        .filter_map(|(id, rail)| vr.read(rail).ok().filter(|p| p.fixed()).map(|p| (id, p.target_mv)))
        .collect()
}
#[cfg(not(target_os = "linux"))]
fn vr_targets(_hw: &HardwareModel) -> Vec<(&'static str, u16)> {
    Vec::new()
}

fn msr(d: &dyn KernelDriver, index: u32, cpu: i32) -> Result<u64> {
    d.read_msr(index, cpu).ok_or_else(|| anyhow!("RDMSR 0x{index:X} on CPU {cpu}: {}", d.last_error().unwrap_or_default()))
}

fn take(hw: &HardwareModel, cpu: &IntelCpu, dimms: &[Arc<Ddr5Dimm>]) -> Result<Snapshot> {
    let d = cpu.driver().as_ref();
    let p = cpu.first_p_thread();
    let e_threads: Vec<i32> = cpu.logical_cpus().iter().filter(|c| !c.is_p_core).map(|c| c.index).collect();
    Ok(Snapshot {
        p_turbo: msr(d, MSR_TURBO, p)?,
        e_turbo: e_threads.iter().map(|&t| Ok((t, msr(d, MSR_ATOM_TURBO, t)?))).collect::<Result<_>>()?,
        ring: msr(d, MSR_RING, p)?,
        power: msr(d, MSR_POWER, p)?,
        ceilings: cpu
            .logical_cpus()
            .iter()
            .map(|c| (c.index, OcMailbox::new(cpu.driver().clone(), c.index).read_core_ratio_ceiling()))
            .collect(),
        domains: DOMAINS.iter().map(|&dom| (dom, cpu.mailbox().read_domain(dom).ok().map(|s| OcMailbox::encode(&s)))).collect(),
        dimms: dimms.iter().map(|m| (m.read_vdd(), m.read_vddq(), m.read_vpp())).collect(),
        vr: vr_targets(hw),
    })
}

fn put_msr(d: &dyn KernelDriver, index: u32, value: u64, cpu: i32, failures: &mut Vec<String>) {
    if d.read_msr(index, cpu) == Some(value) {
        return;
    }
    if !(d.write_msr(index, value, cpu) && d.read_msr(index, cpu) == Some(value)) {
        failures.push(format!("MSR 0x{index:X} on CPU {cpu} back to 0x{value:016X}"));
    }
}

/// Writes the snapshot back: tables first, so a ceiling going back up is not capped by them.
fn restore(hw: &HardwareModel, cpu: &IntelCpu, dimms: &[Arc<Ddr5Dimm>], s: &Snapshot) -> Vec<String> {
    let d = cpu.driver().as_ref();
    let p = cpu.first_p_thread();
    let mut failures = Vec::new();
    put_msr(d, MSR_TURBO, s.p_turbo, p, &mut failures);
    for &(t, v) in &s.e_turbo {
        put_msr(d, MSR_ATOM_TURBO, v, t, &mut failures);
    }
    put_msr(d, MSR_RING, s.ring, p, &mut failures);
    put_msr(d, MSR_POWER, s.power, p, &mut failures);
    for &(dom, word) in &s.domains {
        let Some(word) = word else { continue };
        let now = cpu.mailbox().read_domain(dom).ok().map(|v| OcMailbox::encode(&v));
        if now != Some(word) {
            if let Err(e) = cpu.mailbox().write_domain(dom, &OcMailbox::decode(word)) {
                failures.push(format!("mailbox domain {dom} back to 0x{word:08X}: {e}"));
            }
        }
    }
    for &(c, ceiling) in &s.ceilings {
        let Some(ceiling) = ceiling.filter(|&v| v > 0) else { continue };
        let mb = OcMailbox::new(cpu.driver().clone(), c);
        if mb.read_core_ratio_ceiling() != Some(ceiling) {
            if let Err(e) = mb.set_core_ratio_ceiling(ceiling) {
                failures.push(format!("CPU {c} ratio ceiling back to x{ceiling}: {e}"));
            }
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(vr) = hw.board_vr() {
        use crate::hw::board_vr::VrRail;
        for &(id, mv) in &s.vr {
            let rail = if id == "vr_vccsa" { VrRail::VccSa } else { VrRail::VnnAon };
            if vr.read(rail).ok().map(|p| p.target_mv) != Some(mv) {
                if let Err(e) = vr.set_target(rail, mv as f64 / 1000.0) {
                    failures.push(format!("{id} back to {mv} mV: {e}"));
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = hw;
    for (m, &(vdd, vddq, vpp)) in dimms.iter().zip(&s.dimms) {
        let rails: [(&str, Option<f64>, Option<f64>, &dyn Fn(f64) -> Result<()>); 3] = [
            ("VDD", vdd, m.read_vdd(), &|v| m.write_vdd(v)),
            ("VDDQ", vddq, m.read_vddq(), &|v| m.write_vddq(v)),
            ("VPP", vpp, m.read_vpp(), &|v| m.write_vpp(v)),
        ];
        for (name, want, now, write) in rails {
            if let (Some(want), Some(now)) = (want, now) {
                if (want - now).abs() > 0.0005 {
                    if let Err(e) = write(want) {
                        failures.push(format!("{} {name} back to {want:.3} V: {e}", m.slot_name()));
                    }
                }
            }
        }
    }
    failures
}

/// What the CPU did while `cpus` were busy: the highest ratio on each of `watch`, the highest
/// ring ratio and the highest VID.
struct Loaded {
    ratio: i32,
    ring: i32,
    vid: f64,
}

fn under_load(cpu: &IntelCpu, busy: &[i32], watch: &[i32]) -> Loaded {
    let d = cpu.driver().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = busy
        .iter()
        .map(|&c| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let _ = native::run_on_cpu(c, || {
                    let mut x = 1.0001f64;
                    while !stop.load(Ordering::Relaxed) {
                        x = (x * 1.000001 + 1e-9).sqrt();
                    }
                    std::hint::black_box(x);
                });
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(400));
    let mut out = Loaded { ratio: 0, ring: 0, vid: 0.0 };
    let until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
        for &c in watch {
            if let Some(v) = d.read_msr(MSR_PERF_STATUS, c) {
                out.ratio = out.ratio.max(((v >> 8) & 0xFF) as i32);
                out.vid = out.vid.max(((v >> 32) & 0xFFFF) as f64 / 8192.0);
            }
        }
        if let Some(v) = d.read_msr(MSR_RING_STATUS, cpu.first_p_thread()) {
            out.ring = out.ring.max((v & 0x7F) as i32);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    out
}

fn row(hw: &HardwareModel, id: &str) -> Option<f64> {
    hw.refresh(id);
    hw.setting(id).and_then(|s| s.current)
}

fn apply(r: &mut Report, hw: &HardwareModel, id: &str, value: f64) -> bool {
    let ok = hw.apply(id, value);
    let s = hw.setting(id);
    let read = s.as_ref().and_then(|s| s.current).map_or("none".to_string(), |v| format!("{v}"));
    let err = s.and_then(|s| s.last_error).map(|e| format!(" ({e})")).unwrap_or_default();
    r.line(format!("    apply {id} = {value}: {} - reads back {read}{err}", if ok { "accepted" } else { "refused" }));
    ok
}

fn powercap_watts(constraint: u32) -> Option<f64> {
    std::fs::read_to_string(format!("/sys/class/powercap/intel-rapl:0/constraint_{constraint}_power_limit_uw"))
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|uw| uw / 1e6)
}

/// Mean of a few samples, so one noisy read does not decide a 10 mV question.
fn mean(n: usize, f: impl Fn() -> Option<f64>) -> Option<f64> {
    let v: Vec<f64> = (0..n).filter_map(|_| f()).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

pub fn run(path: &Path) -> i32 {
    let mut r = Report { text: String::new(), passed: 0, failed: 0, noted: 0 };
    r.line(format!("Roch CPU {} write test {}", crate::APP_VERSION, chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    if crate::hw::intel_audit::other_instance_running() {
        r.line("Refused: close every other Roch CPU window first.");
        return 1;
    }
    let hw = HardwareModel::new();
    let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let log = log.clone();
        hw.on_log(move |m| native::lock(&log).push(m.to_string()));
    }
    hw.initialize();
    for m in native::lock(&log).drain(..) {
        r.line(format!("  log: {m}"));
    }
    let Some(cpu) = hw.cpu() else {
        r.line("Intel CPU access is unavailable; nothing to test.");
        let _ = std::fs::write(path, &r.text);
        return 1;
    };
    if !cpu.is_known_desktop() || hw.oc_locked() {
        r.line("Refused: not a known LGA1700/LGA1851 desktop part, or the BIOS has OC Lock set.");
        let _ = std::fs::write(path, &r.text);
        return 1;
    }
    let dimms = hw.dimms();
    let snapshot = match take(&hw, &cpu, &dimms) {
        Ok(s) => s,
        Err(e) => {
            r.line(format!("Snapshot failed, nothing written: {e}"));
            let _ = std::fs::write(path, &r.text);
            return 1;
        }
    };
    r.line(format!("Snapshot: {snapshot:?}"));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tests(&mut r, &hw, &cpu, &dimms)));
    let stopped = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some("a test panicked".to_string()),
    };
    if let Some(why) = stopped {
        r.line(format!("Test sequence stopped early: {why}"));
        r.failed += 1;
    }

    r.line("");
    r.line("Restoring the snapshot:");
    let failures = restore(&hw, &cpu, &dimms, &snapshot);
    for f in &failures {
        r.line(format!("  could not restore {f}"));
    }
    match take(&hw, &cpu, &dimms) {
        Ok(after) => {
            let same = after == snapshot;
            r.check("everything back as found", same && failures.is_empty(), if same { "every register reads its start value".to_string() } else { format!("now {after:?}") });
        }
        Err(e) => r.check("everything back as found", false, format!("could not read back: {e}")),
    }
    for m in native::lock(&log).drain(..) {
        r.line(format!("  log: {m}"));
    }
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("dmesg").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("msr") || l.contains("Tainted")).collect();
        r.line(format!("Kernel log lines about MSR writes: {}", lines.len()));
        for l in lines.iter().rev().take(5).rev() {
            r.line(format!("  {l}"));
        }
    }
    r.line(format!("Result: {} passed, {} failed, {} notes", r.passed, r.failed, r.noted));
    let _ = std::fs::write(path, &r.text);
    println!("Written to {}", path.display());
    if r.failed == 0 { 0 } else { 2 }
}

fn rail_reading(sio: &Option<Arc<SuperIo>>, index: i32) -> Option<f64> {
    let s = sio.clone()?;
    mean(5, || s.read_voltage(index))
}

fn tests(r: &mut Report, hw: &HardwareModel, cpu: &Arc<IntelCpu>, dimms: &[Arc<Ddr5Dimm>]) -> Result<()> {
    let p_threads = cpu.p_core_threads();
    let e_threads: Vec<i32> = cpu.logical_cpus().iter().filter(|c| !c.is_p_core).map(|c| c.index).collect();
    let clusters = cpu.e_core_clusters();

    // ---- baseline
    r.line("");
    r.line("Baseline under load:");
    let p_base = under_load(cpu, &p_threads, &p_threads);
    let e_base = under_load(cpu, &e_threads, &e_threads);
    r.line(format!("  all P-cores busy: x{} ring x{} VID {:.4} V", p_base.ratio, p_base.ring, p_base.vid));
    r.line(format!("  all E-cores busy: x{}", e_base.ratio));

    // ---- P-core all-core ratio
    r.line("");
    r.line("CPU Ratio (P-Core):");
    let start = row(hw, "cpu_ratio").ok_or_else(|| anyhow!("cpu_ratio unreadable"))? as i32;
    let target = start - 2;
    if apply(r, hw, "cpu_ratio", target as f64) {
        let l = under_load(cpu, &p_threads, &p_threads);
        r.check("P-cores run at the new ratio", l.ratio == target, format!("asked x{target}, all P-cores busy ran at x{}", l.ratio));
        let kernel_max = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq").unwrap_or_default();
        r.line(format!("    (kernel scaling_max_freq on cpu0: {} kHz; the kernel caches HWP limits)", kernel_max.trim()));
    } else {
        r.check("P-core ratio accepted", false, "refused");
    }
    apply(r, hw, "cpu_ratio", start as f64);

    // ---- per-core P ceilings
    r.line("");
    r.line("Per-core P ratios (mailbox 0x1D):");
    let ceilings = cpu.read_p_core_ratio_ceilings();
    if ceilings.iter().all(Option::is_some) && p_threads.len() >= 2 {
        let mut wanted: Vec<i32> = ceilings.iter().map(|c| c.unwrap()).collect();
        let top = *wanted.iter().max().unwrap();
        wanted.iter_mut().for_each(|w| *w = top.min(54));
        wanted[0] = wanted[0] - 4;
        match cpu.write_p_core_ratio_ceilings(&wanted) {
            Ok(()) => {
                let first = under_load(cpu, &[p_threads[0]], &[p_threads[0]]);
                let other = under_load(cpu, &[p_threads[1]], &[p_threads[1]]);
                r.check(
                    "only the chosen core is capped",
                    first.ratio == wanted[0] && other.ratio == wanted[1],
                    format!("core 0 asked x{} ran x{}; core 1 asked x{} ran x{}", wanted[0], first.ratio, wanted[1], other.ratio),
                );
            }
            Err(e) => r.check("per-core ceilings accepted", false, e.to_string()),
        }
    } else {
        r.note("per-core P ratios", "ceilings not readable; skipped");
    }

    // ---- E-core all-core ratio
    if !e_threads.is_empty() {
        r.line("");
        r.line("E-Core Ratio:");
        let start = row(hw, "ecore_ratio").ok_or_else(|| anyhow!("ecore_ratio unreadable"))? as i32;
        let target = start - 2;
        if apply(r, hw, "ecore_ratio", target as f64) {
            let l = under_load(cpu, &e_threads, &e_threads);
            r.check("E-cores run at the new ratio", l.ratio == target, format!("asked x{target}, all E-cores busy ran at x{}", l.ratio));
        } else {
            r.check("E-core ratio accepted", false, "refused");
        }
        apply(r, hw, "ecore_ratio", start as f64);

        // ---- per-cluster E ceilings
        r.line("");
        r.line("Per-cluster E ratios:");
        if clusters.len() >= 2 {
            let current: Vec<i32> = cpu.read_e_cluster_ratio_ceilings().iter().map(|c| c.unwrap_or(start)).collect();
            let mut wanted = current.clone();
            wanted[0] -= 3;
            match cpu.write_e_cluster_ratio_ceilings(&wanted) {
                Ok(()) => {
                    let first = under_load(cpu, &clusters[0], &clusters[0]);
                    let other = under_load(cpu, &clusters[1], &clusters[1]);
                    r.check(
                        "only the chosen cluster is capped",
                        first.ratio == wanted[0] && other.ratio == wanted[1],
                        format!("cluster 0 asked x{} ran x{}; cluster 1 asked x{} ran x{}", wanted[0], first.ratio, wanted[1], other.ratio),
                    );
                }
                Err(e) => r.check("per-cluster ceilings accepted", false, e.to_string()),
            }
            let _ = cpu.write_e_cluster_ratio_ceilings(&current);
        }
    }

    // ---- ring
    r.line("");
    r.line("Ring Ratio:");
    let start = row(hw, "ring_ratio").ok_or_else(|| anyhow!("ring_ratio unreadable"))? as i32;
    let target = start - 2;
    if apply(r, hw, "ring_ratio", target as f64) {
        let l = under_load(cpu, &p_threads, &p_threads);
        r.check("ring runs at the new ratio", l.ring == target, format!("asked x{target}, ring ran at x{} (MSR 0x621)", l.ring));
    } else {
        r.check("ring ratio accepted", false, "refused");
    }
    apply(r, hw, "ring_ratio", start as f64);

    // ---- power limits
    for (id, constraint) in [("pl1", 0u32), ("pl2", 1)] {
        r.line("");
        r.line(format!("{}:", id.to_uppercase()));
        let start = row(hw, id).ok_or_else(|| anyhow!("{id} unreadable"))?;
        let target = (start - 10.0).max(65.0);
        if apply(r, hw, id, target) {
            let kernel = powercap_watts(constraint);
            r.check(
                "the kernel's RAPL driver sees it",
                kernel.is_some_and(|k| (k - target).abs() < 0.5),
                format!("asked {target} W, powercap constraint_{constraint} reads {}", kernel.map_or("n/a".into(), |k| format!("{k} W"))),
            );
        } else {
            r.check(&format!("{id} accepted"), false, "refused");
        }
        apply(r, hw, id, start);
    }

    let sio: Option<Arc<SuperIo>> = hw.super_io();
    let rail = |index: i32| -> Option<f64> {
        let s = sio.clone()?;
        mean(5, || s.read_voltage(index))
    };

    // ---- core offset
    r.line("");
    r.line("CPU Core Voltage Offset:");
    let before = under_load(cpu, &p_threads, &p_threads);
    let core_locked = hw.setting("core_off").is_some_and(|s| s.read_only());
    if core_locked {
        r.note("core voltage rows", hw.setting("core_off").and_then(|s| s.note).unwrap_or_default());
        let refused = !hw.apply("core_off", -10.0);
        let unchanged = cpu.mailbox().read_domain(OcMailbox::DOMAIN_CORE).is_ok_and(|d| d.offset_volts == 0.0);
        r.check("a read-only core row writes nothing", refused && unchanged, format!("refused {refused}, domain unchanged {unchanged}"));
    } else if apply(r, hw, "core_off", -10.0) {
        let after = under_load(cpu, &p_threads, &p_threads);
        let delta = (after.vid - before.vid) * 1000.0;
        r.check(
            "VID under load drops by about the offset",
            (-25.0..=-2.0).contains(&delta),
            format!("VID {:.4} -> {:.4} V ({delta:+.1} mV, asked -10 mV; ratio x{} -> x{})", before.vid, after.vid, before.ratio, after.ratio),
        );
    } else {
        r.check("core offset accepted", false, "refused");
    }
    if !core_locked {
        apply(r, hw, "core_off", 0.0);
    }

    // ---- core override
    r.line("");
    r.line("CPU Core Voltage (fixed):");
    let target = ((before.vid + 0.02) * 200.0).round() / 200.0;
    if core_locked {
        r.note("core voltage", "read-only on this machine; skipped");
    } else if before.vid > 0.5 && target <= 1.35 {
        let vcore_before = rail(0);
        if apply(r, hw, "core_v", target) {
            let after = under_load(cpu, &p_threads, &p_threads);
            let vcore_after = rail(0);
            r.check(
                "VID under load follows the fixed target",
                (after.vid - target).abs() <= 0.02,
                format!("target {target:.3} V, loaded VID {:.4} V (was {:.4})", after.vid, before.vid),
            );
            r.note(
                "board Vcore sensor (idle)",
                format!(
                    "{} -> {} V",
                    vcore_before.map_or("n/a".into(), |v| format!("{v:.3}")),
                    vcore_after.map_or("n/a".into(), |v| format!("{v:.3}"))
                ),
            );
        } else {
            r.check("core voltage accepted", false, "refused");
        }
        apply(r, hw, "core_v", 0.0);
    } else {
        r.note("core voltage", format!("loaded VID {:.4} V leaves no safe fixed target; skipped", before.vid));
    }

    // ---- SA
    r.line("");
    r.line("SA Voltage:");
    if let Some(start) = row(hw, "sa_v") {
        let sa_before = rail(6);
        let target = start + 0.009;
        if apply(r, hw, "sa_v", target) {
            std::thread::sleep(Duration::from_millis(300));
            let sa_after = rail(6);
            r.check("SA target reads back", row(hw, "sa_v").is_some_and(|v| (v - target).abs() < 0.002), format!("asked {target:.3} V"));
            match (sa_before, sa_after) {
                (Some(a), Some(b)) => r.note("board VCCSA sensor", format!("{a:.3} -> {b:.3} V ({:+.0} mV; sensor steps are 12 mV)", (b - a) * 1000.0)),
                _ => r.note("board VCCSA sensor", "not mapped on this board"),
            }
        } else {
            r.check("SA voltage accepted", false, "refused");
        }
        apply(r, hw, "sa_v", start);
        r.line("SA Voltage Offset while SA is fixed (must be refused, not switch SA to adaptive):");
        let refused = !hw.apply("sa_off", -5.0);
        let still_fixed = cpu.mailbox().read_domain(OcMailbox::DOMAIN_SA).is_ok_and(|d| d.override_mode);
        r.check("offset refused and SA still fixed", refused && still_fixed, format!("refused {refused}, override kept {still_fixed}"));
    } else {
        r.note("SA voltage", "no SA row here");
    }

    // ---- board regulators
    #[cfg(target_os = "linux")]
    if let Some(vr) = hw.board_vr() {
        use crate::hw::board_vr::VrRail;
        for (id, rail, sensor) in [("vr_vccsa", VrRail::VccSa, 6), ("vr_vnnaon", VrRail::VnnAon, 5)] {
            r.line("");
            r.line(format!("{} (board regulator):", rail.name()));
            let Some(row_now) = hw.setting(id) else {
                r.note(id, "no row (the rail is not fixed by the board)");
                continue;
            };
            if row_now.read_only() {
                r.note(id, row_now.note.unwrap_or_default());
                continue;
            }
            let Some(start) = row(hw, id) else {
                r.note(id, "target unreadable");
                continue;
            };
            let out_before = mean(5, || vr.read_output(rail));
            let sensor_before = rail_reading(&sio, sensor);
            let target = ((start + 0.010) * 1000.0).round() / 1000.0;
            if apply(r, hw, id, target) {
                std::thread::sleep(Duration::from_millis(200));
                let out_after = mean(5, || vr.read_output(rail));
                let sensor_after = rail_reading(&sio, sensor);
                let moved = out_after.zip(out_before).map(|(a, b)| (a - b) * 1000.0);
                r.check(
                    "the regulator's output follows",
                    moved.is_some_and(|m| (5.0..=15.0).contains(&m)),
                    format!("target {start:.3} -> {target:.3} V, output {out_before:?} -> {out_after:?} V"),
                );
                r.note("board sensor (12 mV steps)", format!("{sensor_before:?} -> {sensor_after:?} V"));
            } else {
                r.check(&format!("{id} accepted"), false, "refused");
            }
            if !apply(r, hw, id, start) {
                bail!("{id} could not be put back to {start:.3} V");
            }
        }
    }

    // ---- DIMMs
    for (i, m) in dimms.iter().enumerate() {
        let slot = m.slot_name().to_lowercase();
        for (rail_name, id, select, step) in [("VDD", "vdd", Ddr5Dimm::ADC_SWA, 0.015), ("VDDQ", "vddq", Ddr5Dimm::ADC_SWC, 0.015), ("VPP", "vpp", Ddr5Dimm::ADC_SWD, 0.015)] {
            let id = format!("{slot}_{id}");
            r.line("");
            r.line(format!("{} {rail_name}:", m.slot_name()));
            let Some(start) = row(hw, &id) else {
                r.note(&id, "row not available");
                continue;
            };
            let adc_before = mean(3, || m.read_adc_volts(select));
            let target = ((start + step) * 1000.0).round() / 1000.0;
            if apply(r, hw, &id, target) {
                let adc_after = mean(3, || m.read_adc_volts(select));
                let moved = match (adc_before, adc_after) {
                    (Some(a), Some(b)) => b - a,
                    _ => f64::NAN,
                };
                r.check(
                    "the PMIC's own ADC sees the rail move",
                    moved > 0.005,
                    format!("set-point {start:.3} -> {target:.3} V, ADC {:?} -> {:?} V", adc_before, adc_after),
                );
            } else {
                r.check(&format!("{id} accepted"), false, "refused");
            }
            if !apply(r, hw, &id, start) {
                bail!("{id} could not be put back to {start:.3} V; stopping before the next DIMM write");
            }
        }
        let _ = i;
    }
    Ok(())
}
