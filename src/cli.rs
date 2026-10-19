//! The diagnostic command-line switches. Run with no arguments the program is a window; the
//! switches exist for reporting a board that misbehaves and for porting to hardware this has
//! not seen.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::hw::amd_smu::{AmdSmu, SmuStatus};
use crate::hw::asus_audit;
use crate::hw::asus_board_control::AsusBoardControl;
use crate::hw::bclk_controller::BclkController;
use crate::hw::ddr5_dimm::Ddr5Dimm;
use crate::hw::ec_clock_gen::EcClockGen;
use crate::hw::ec_mailbox::EcMailbox;
use crate::hw::hypervisor_restrictions;
use crate::hw::intel_audit;
use crate::hw::intel_cpu::IntelCpu;
use crate::hw::native::{self, app_dir};
use crate::hw::oc_mailbox::{OcMailbox, VfDomainSettings};
use crate::hw::smbus_i801::SmbusI801;
use crate::hw::super_io::SuperIo;
use crate::model::hardware_model::HardwareModel;

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn arg_or(args: &[String], i: usize, default: PathBuf) -> PathBuf {
    args.get(i).map(PathBuf::from).unwrap_or(default)
}

fn write_file(path: &Path, text: &str) {
    let _ = std::fs::write(path, text);
}

fn append_file(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

fn parse_u32(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// A model with the log routed to stderr, as most diagnostic modes use.
fn model_logging_to_stderr() -> HardwareModel {
    let hw = HardwareModel::new();
    hw.on_log(|m| eprintln!("{m}"));
    hw.initialize();
    hw
}

fn model_quiet() -> HardwareModel {
    let hw = HardwareModel::new();
    hw.initialize();
    hw
}

/// Busy threads used to load the P-cores during the voltage tests.
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start(n: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n.max(1))
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1.0001f64;
                    while !stop.load(Ordering::Relaxed) {
                        x = (x * 1.000001 + 1e-9).sqrt();
                    }
                    std::hint::black_box(x);
                })
            })
            .collect();
        Self { stop, threads }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn p_core_count(cpu: &IntelCpu) -> usize {
    cpu.logical_cpus().iter().filter(|c| c.is_p_core).count().max(1)
}

pub fn run(args: &[String]) -> i32 {
    let a0 = args[0].as_str();
    let dir = app_dir();

    if matches!(a0, "--help" | "-h" | "/?") {
        return help();
    }
    if eq(a0, "--probe") {
        return probe(&arg_or(args, 1, dir.join("probe.txt")));
    }
    if eq(a0, "--intel-audit") {
        return intel_audit::run(&arg_or(args, 1, dir.join("intel-audit.txt")));
    }
    if a0 == "--asus-audit" || a0 == "--asus-bclk-validate" {
        return asus_audit::run(&dir.join("asus-audit.txt"), a0 == "--asus-bclk-validate");
    }
    if a0 == "--asus-voltage-validate" {
        return asus_audit::validate_voltage(&dir.join("asus-voltage-validation.txt"));
    }
    if a0 == "--asus-ring-voltage-validate" {
        return asus_audit::validate_ring_voltage(&dir.join("asus-ring-voltage-validation.txt"));
    }
    if args.len() > 2 && a0 == "--asus-rail-validate" {
        let id = match args[1].as_str() {
            "sa" => AsusBoardControl::SA_ID,
            "l2" => AsusBoardControl::L2_ID,
            _ => 0,
        };
        let Some(volts) = parse_f64(&args[2]) else {
            println!("'{}' is not a number", args[2]);
            return 1;
        };
        return asus_audit::validate_rail(&dir.join(format!("asus-{}-validation.txt", args[1])), id, volts);
    }
    if args.len() > 1 && eq(a0, "--intel-validate") {
        let Some(volts) = parse_f64(&args[1]) else {
            println!("'{}' is not a number", args[1]);
            return 1;
        };
        return intel_audit::validate(&dir.join("intel-validation.txt"), volts);
    }
    if eq(a0, "--ring-validate") {
        return intel_audit::validate_ring(&dir.join("ring-validation.txt"));
    }
    if args.len() > 1 && eq(a0, "--ecore-validate") {
        let Ok(target) = args[1].parse::<i32>() else {
            println!("'{}' is not a number", args[1]);
            return 1;
        };
        return intel_audit::validate_e_core(&dir.join("ecore-validation.txt"), target);
    }
    if eq(a0, "--windows-audit") {
        let (restrictions, status) = hypervisor_restrictions::read_current_boot();
        let mut lines = vec![
            status,
            hypervisor_restrictions::voltage_block_reason(&restrictions)
                .unwrap_or_else(|| "No voltage restriction confirmed by the log.".into()),
        ];
        for r in &restrictions {
            lines.push(format!(
                "{} MSR 0x{:X} write={} driver={}",
                r.time_utc.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                r.register,
                if r.is_write { "True" } else { "False" },
                r.driver
            ));
        }
        write_file(&dir.join("windows-audit.txt"), &(lines.join("\r\n") + "\r\n"));
        return 0;
    }
    if eq(a0, "--pm-dump") {
        return pm_dump(&arg_or(args, 1, dir.join("pmtable.txt")));
    }
    if args.len() > 2 && eq(a0, "--smu") {
        return smu_command(&args[1..]);
    }
    if args.len() > 2 && eq(a0, "--apply") {
        return apply_setting(&args[1], &args[2]);
    }
    if args.len() > 1 && eq(a0, "--smn") {
        let count = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
        return smn_dump(&args[1], count);
    }
    if eq(a0, "--smb-regs") {
        let hw = model_quiet();
        let Some(bus) = hw.smbus() else {
            println!("no SMBus");
            return 1;
        };
        let addrs: [u8; 7] = [0x44, 0x48, 0x4A, 0x50, 0x52, 0x70, 0x72];
        let mut out = Vec::new();
        for a in addrs {
            for r in 0..=255u8 {
                out.push(bus.read_byte(a, r).unwrap_or(0xEE));
            }
        }
        let _ = std::fs::write(args.get(1).map(String::as_str).unwrap_or("smb.bin"), &out);
        let vcore = hw.super_io().and_then(|s| s.read_voltage(SuperIo::RAIL_VCORE));
        println!(
            "dumped {} addresses x 256 regs; Vcore {} V",
            addrs.len(),
            vcore.map(|v| format!("{v:.3}")).unwrap_or_default()
        );
        return 0;
    }

    // NOTE: several EC modes that lived here are gone, and should not come back in that shape.
    //
    // The dump/peek/poke/watch modes polled the Super I/O EC as fast as the LPC path allowed,
    // to catch the order of the vendor tool's writes. That hard-reset this machine twice
    // (Kernel-Power 41): the NCT6687D also runs fan control and power sequencing, and
    // saturating it - especially while another tool holds the same ISA lock - hangs the board.
    // Do not reintroduce high-rate EC polling.
    //
    // --ec-capture, --ec-restore and --bclk-step went with them. Those were built on a reading
    // of the EC that turned out to be wrong: 0x463 taken for a doorbell with the parameter at
    // 0x470. 0x463 is the command, 0x460 is the doorbell, and 0x470 is the mailbox's write-data
    // register - which is why the vendor tool's value appeared there and why writing it alone
    // did nothing. See EcMailbox for what it actually is.

    if eq(a0, "--smbus-reset") {
        let hw = model_quiet();
        let Some(bus) = hw.smbus() else {
            println!("no SMBus");
            return 1;
        };
        // Recovery is Intel-specific: SSRESET lives in the PCH's HSTCFG register, and the
        // AMD (PIIX4) controller has no equivalent.
        let Some(bus9) = bus.as_any().downcast_ref::<SmbusI801>() else {
            println!(
                "SMBus recovery is only implemented for the Intel PCH controller; this system has {}.",
                bus.description()
            );
            return 1;
        };
        println!("soft reset : {}", bus9.reset().unwrap_or_else(|e| e.to_string()));
        println!("host reset : {}", bus9.hard_reset().unwrap_or_else(|e| e.to_string()));
        for attempt in 1..=3 {
            let found = Ddr5Dimm::probe(bus.clone());
            println!(
                "attempt {attempt}: {} DDR5 module(s) {}",
                found.len(),
                found.iter().map(|d| format!("{}@0x{:02X}", d.slot_name(), d.spd_address())).collect::<Vec<_>>().join(", ")
            );
            if !found.is_empty() {
                break;
            }
            let _ = bus9.reset();
            native::sleep_ms(300);
        }
        return 0;
    }

    if eq(a0, "--sio-ldn") {
        // Enumerate the Super I/O's logical devices and their I/O windows. Plain config-space
        // reads with the documented unlock/lock sequence - no polling, nothing written.
        let hw = model_quiet();
        let Some(drv) = hw.driver() else {
            println!("no driver");
            return 1;
        };
        let r: anyhow::Result<()> = (|| {
            for port in [0x2Eu16, 0x4E] {
                let val = port + 1;
                let rd = |reg: u8| -> anyhow::Result<u8> {
                    drv.write_io_port_byte(port, reg)?;
                    drv.read_io_port_byte(val)
                };
                let wr = |reg: u8, v: u8| -> anyhow::Result<()> {
                    drv.write_io_port_byte(port, reg)?;
                    drv.write_io_port_byte(val, v)
                };
                drv.write_io_port_byte(port, 0x87)?;
                drv.write_io_port_byte(port, 0x87)?;
                let (id, rev) = (rd(0x20)?, rd(0x21)?);
                if id == 0xFF || (id == 0 && rev == 0) {
                    drv.write_io_port_byte(port, 0xAA)?;
                    continue;
                }
                println!("LPC 0x{port:02X}: chip 0x{id:02X}{rev:02X}");
                for ldn in 0..=0x1Fu8 {
                    wr(0x07, ldn)?;
                    let active = rd(0x30)?;
                    let b0 = ((rd(0x60)? as u16) << 8) | rd(0x61)? as u16;
                    let b1 = ((rd(0x62)? as u16) << 8) | rd(0x63)? as u16;
                    if active == 0xFF && b0 == 0xFFFF {
                        continue;
                    }
                    if active == 0 && b0 == 0 && b1 == 0 {
                        continue;
                    }
                    println!("   LDN 0x{ldn:02X}  active={active:02X}  base0=0x{b0:04X}  base1=0x{b1:04X}");
                }
                drv.write_io_port_byte(port, 0xAA)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            println!("{e}");
            return 1;
        }
        return 0;
    }

    if eq(a0, "--bclk-test") {
        let hw = model_quiet();
        let (Some(cpu), Some(meter)) = (hw.cpu(), hw.bclk().filter(|b| b.is_available())) else {
            println!("no CPU / BCLK meter");
            return 1;
        };
        println!("Comparing the two ways of measuring BCLK.");
        println!("  base ratio {}", cpu.base_ratio());
        for _ in 0..3 {
            let tsc = meter.measure_bclk_mhz(cpu.base_ratio(), 60).unwrap_or(f64::NAN);
            let core = meter.measure_bclk_from_core(cpu.first_p_thread(), 200).ok().flatten();
            let cycles = meter.measure_bclk_from_cycles(cpu.first_p_thread(), 150).ok().flatten();
            println!(
                "  TSC {:>8}   APERF/MPERF {:>8}   core cycles {:>8} MHz",
                format!("{tsc:.3}"),
                core.map(|v| format!("{v:.3}")).unwrap_or_default(),
                cycles.map(|v| format!("{v:.3}")).unwrap_or_default()
            );
        }
        return 0;
    }

    if eq(a0, "--clkgen") {
        let hw = model_logging_to_stderr();
        println!("{}", hw.super_io_status());
        let Some(sio) = hw.super_io().filter(|s| EcClockGen::is_supported(Some(s))) else {
            println!("no EC mailbox on this Super I/O");
            return 1;
        };
        let generator = EcClockGen::new(EcMailbox::new(sio, false));
        for _ in 0..3 {
            let blk = generator.read_block();
            let mhz = match (hw.cpu(), hw.bclk().filter(|b| b.is_available())) {
                (Some(c), Some(m)) => m.measure_bclk_from_cycles(c.first_p_thread(), 150).ok().flatten(),
                _ => None,
            };
            match blk {
                None => println!("  clock generator did not answer"),
                Some(b) => println!(
                    "  {}   measured {} MHz",
                    EcClockGen::describe(&b),
                    mhz.map(|v| format!("{v:.3}")).unwrap_or_default()
                ),
            }
        }
        return 0;
    }

    if eq(a0, "--bclk-cal") || eq(a0, "--bclk-probe") || eq(a0, "--clkgen-dump") || eq(a0, "--bclk-set") {
        return bclk_tools(args);
    }

    if eq(a0, "--vdd2") {
        let hw = model_logging_to_stderr();
        let Some(rail) = hw.vdd2() else {
            println!("CPU VDD2 is not settable on this board");
            return 1;
        };
        println!("baseline: {}", rail.status());
        if args.len() < 2 {
            println!("usage: --vdd2 <volts>   (omit to just report)");
            return 0;
        }
        let Some(target) = parse_f64(&args[1]) else {
            println!("usage: --vdd2 <volts>   (omit to just report)");
            return 1;
        };
        let ok = rail.set_volts(target, Some(&|m: &str| println!("{m}")));
        println!("{}{}", if ok { "ok: " } else { "FAILED: " }, rail.status());
        return if ok { 0 } else { 1 };
    }

    if eq(a0, "--vcore-test") {
        return vcore_test(&arg_or(args, 1, dir.join("vcore.txt")));
    }

    if eq(a0, "--sio-dump") {
        let hw = model_logging_to_stderr();
        let mut sb = String::new();
        let _ = writeln!(sb, "{}", hw.super_io_status());
        if let Some(sio) = hw.super_io() {
            for (i, v) in sio.read_all() {
                let _ = writeln!(sb, "VIN{:<2} = {v:.3} V", i);
            }
        }
        write_file(&arg_or(args, 1, dir.join("sio.txt")), &crlf(&sb));
        return 0;
    }

    if eq(a0, "--smbus-scan") {
        return smbus_scan(&arg_or(args, 1, dir.join("smbus.txt")));
    }

    if eq(a0, "--vtest") {
        return voltage_test(&arg_or(args, 1, dir.join("vtest.txt")));
    }

    println!("Unknown option {a0}. Run with --help for the list.");
    1
}

fn tf(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

/// .NET's `AppendLine` writes CRLF on Windows; the report files keep that.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn bclk_tools(args: &[String]) -> i32 {
    let a0 = args[0].as_str();
    let hw = model_logging_to_stderr();
    let Some(sio) = hw.super_io().filter(|s| EcClockGen::is_supported(Some(s))) else {
        println!("no EC mailbox on this Super I/O");
        return 1;
    };
    let (Some(cpu), Some(meter)) = (hw.cpu(), hw.bclk().filter(|b| b.is_available())) else {
        println!("no BCLK meter");
        return 1;
    };

    let thread = cpu.first_p_thread();
    let ctl = BclkController::new(
        EcClockGen::new(EcMailbox::new(sio.clone(), false)),
        Box::new(move || meter.measure_bclk_from_cycles(thread, 150).ok().flatten()),
    );
    if !ctl.capture_baseline() {
        println!("{}", ctl.status());
        return 1;
    }
    println!("{}", ctl.status());
    let print = |m: &str| println!("{m}");

    if eq(a0, "--clkgen-dump") {
        // Read-only sweep of the clock generator, to find what else on it moves with the
        // base clock. Word reads overlap by a byte, so the low byte of each is the map.
        let gen_d = EcMailbox::new(sio, false);
        let from = args.get(1).and_then(|s| i32::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0x00);
        let to = args.get(2).and_then(|s| i32::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0xFF);
        println!("clock generator 0x{:02X}, registers 0x{from:02X}-0x{to:02X}", EcClockGen::CLOCK_GEN_ADDRESS);
        let mut b = from & !0xF;
        while b <= to {
            let mut cells = Vec::new();
            for i in 0..16 {
                let r = b + i;
                cells.push(if r < from || r > to {
                    "  ".to_string()
                } else {
                    match gen_d.read_word(EcClockGen::CLOCK_GEN_ADDRESS, r as u8) {
                        Some(wv) => format!("{:02X}", wv as u8),
                        None => "--".into(),
                    }
                });
            }
            println!("  {b:02X}: {}", cells.join(" "));
            b += 16;
        }
        return 0;
    }

    if eq(a0, "--bclk-probe") {
        // Dividers spread across the range, including two that cross an integer boundary,
        // which is where the half-written-divider case shows up if the write order is wrong.
        let mut divs = vec![100.0, 99.5, 99.0, 98.98, 98.5, 98.02];
        if args.len() > 1 {
            let parsed: Option<Vec<f64>> = args[1..].iter().map(|a| parse_f64(a)).collect();
            match parsed {
                Some(p) => divs = p,
                None => {
                    println!("dividers must be numbers");
                    return 1;
                }
            }
        }
        let ok = ctl.probe_dividers(&divs, Some(&print));
        println!("{}{}", if ok { "" } else { "FAILED: " }, ctl.status());
        return if ok { 0 } else { 1 };
    }

    if eq(a0, "--bclk-cal") {
        let ok = ctl.calibrate(Some(&print));
        println!("{}{}", if ok { "calibrated: " } else { "FAILED: " }, ctl.status());
        return if ok { 0 } else { 1 };
    }

    let Some(want) = args.get(1).and_then(|s| parse_f64(s)) else {
        println!("usage: --bclk-set <MHz>");
        return 1;
    };
    // No pre-calibration: set_bclk establishes the scale from its own first read, the same
    // way the Base Clock row does, so this measures what the application actually costs.
    let set = ctl.set_bclk(want, Some(&print));
    println!("{}{}", if set { "ok: " } else { "FAILED: " }, ctl.status());
    if set { 0 } else { 1 }
}

/// Decisive A/B: does a mailbox core-voltage override move the voltage the VRM actually
/// delivers? Raises the request by 60 mV (safe: well inside the normal range), measures the
/// real rail with the Super I/O, and restores the previous setting.
fn vcore_test(report_path: &Path) -> i32 {
    let mut sb = String::new();
    macro_rules! w {
        ($($t:tt)*) => {{ let s = format!($($t)*); println!("{s}"); sb.push_str(&s); sb.push_str("\r\n"); }};
    }
    let hw = model_quiet();
    let Some(cpu) = hw.cpu() else {
        w!("This test drives the Intel OC mailbox; not available on this CPU.");
        write_file(report_path, &sb);
        return 1;
    };
    let Some(sio) = hw.super_io() else {
        w!("No Super I/O: cannot measure the real rail.");
        write_file(report_path, &sb);
        return 1;
    };
    let mb = cpu.mailbox();
    let load = Load::start(p_core_count(&cpu));

    let r: anyhow::Result<()> = (|| {
        let measure = || -> anyhow::Result<(f64, f64)> {
            native::sleep_ms(600);
            let (mut vid_sum, mut vc_sum, mut c) = (0.0, 0.0, 0);
            for _ in 0..8 {
                let (_, vid) = cpu.read_perf_status(cpu.first_p_thread())?;
                vid_sum += vid;
                vc_sum += sio.read_voltage(SuperIo::RAIL_VCORE).unwrap_or(0.0);
                c += 1;
                native::sleep_ms(60);
            }
            Ok((vid_sum / c as f64, vc_sum / c as f64))
        };

        let s0 = mb.read_domain(OcMailbox::DOMAIN_CORE)?;
        let m0 = measure()?;
        w!("Baseline     : mailbox {s0}");
        w!("               VID {:.3} V, real Vcore {:.3} V", m0.0, m0.1);
        w!("");

        // A board can clamp upward while still following downward, and the adaptive-offset path
        // is a different mechanism from a static override, so test all three before concluding.
        let mut results: Vec<(String, f64, f64)> = Vec::new();
        let mut try_one = |name: &str, s: VfDomainSettings, sb: &mut String| -> anyhow::Result<()> {
            mb.write_domain(OcMailbox::DOMAIN_CORE, &s)?;
            let m = measure()?;
            let (d_vid, d_rail) = ((m.0 - m0.0) * 1000.0, (m.1 - m0.1) * 1000.0);
            let line = format!(
                "{name:<22}: VID {:.3} V ({d_vid:+.0} mV), real Vcore {:.3} V ({d_rail:+.0} mV)",
                m.0, m.1
            );
            println!("{line}");
            sb.push_str(&line);
            sb.push_str("\r\n");
            results.push((name.to_string(), d_vid, d_rail));
            mb.write_domain(OcMailbox::DOMAIN_CORE, &s0)?;
            native::sleep_ms(400);
            Ok(())
        };

        let base_target = if s0.override_mode { s0.target_volts } else { m0.0 };
        let with = |override_mode: bool, target: f64, offset: f64| VfDomainSettings {
            max_ratio: s0.max_ratio,
            override_mode,
            target_volts: target,
            offset_volts: offset,
        };
        try_one("override +60 mV", with(true, base_target + 0.060, s0.offset_volts), &mut sb)?;
        try_one("override -60 mV", with(true, base_target - 0.060, s0.offset_volts), &mut sb)?;
        try_one("offset -60 mV", with(s0.override_mode, s0.target_volts, s0.offset_volts - 0.060), &mut sb)?;
        try_one("adaptive, offset -60", with(false, 0.0, -0.060), &mut sb)?;

        mb.write_domain(OcMailbox::DOMAIN_CORE, &s0)?;
        let m2 = measure()?;
        w!("");
        w!("Restored     : mailbox {}", mb.read_domain(OcMailbox::DOMAIN_CORE)?);
        w!("               VID {:.3} V, real Vcore {:.3} V", m2.0, m2.1);
        w!("");
        // What matters is whether the rail tracked the VID, not how big the step happened to be:
        // in adaptive mode the CPU clamps the request to its V/F curve, so a 60 mV ask can
        // legitimately produce a 20 mV VID move - and the rail should move that same 20 mV.
        let followed: Vec<&(String, f64, f64)> = results
            .iter()
            .filter(|r| r.1.abs() >= 10.0 && r.2.signum() == r.1.signum() && r.2.abs() >= r.1.abs() * 0.6)
            .collect();
        if followed.is_empty() {
            w!("VERDICT: no path moved the rail. The board pins Vcore; only a VRM-side write can change it.");
        } else {
            w!(
                "VERDICT: the rail follows the CPU. Paths verified: {}.",
                followed.iter().map(|f| format!("{} (VID {:+.0} mV -> rail {:+.0} mV)", f.0, f.1, f.2)).collect::<Vec<_>>().join(", ")
            );
            w!("The Core Voltage rows control real Vcore on this board.");
        }
        Ok(())
    })();
    if let Err(e) = r {
        w!("ERROR: {e}");
    }
    load.stop();
    write_file(report_path, &sb);
    0
}

/// Read-only scan of the PCH SMBus for PMBus voltage regulators (VRM controllers).
fn smbus_scan(report_path: &Path) -> i32 {
    let sb = Arc::new(std::sync::Mutex::new(String::new()));
    let w = |s: String| {
        let mut b = native::lock(&sb);
        b.push_str(&s);
        b.push_str("\r\n");
    };
    let hw = HardwareModel::new();
    {
        let sb = sb.clone();
        hw.on_log(move |m| {
            let mut b = native::lock(&sb);
            b.push_str("  log: ");
            b.push_str(m);
            b.push_str("\r\n");
        });
    }
    hw.initialize();
    let Some(bus) = hw.smbus() else {
        w("No SMBus.".into());
        write_file(report_path, &native::lock(&sb));
        return 1;
    };
    w(format!("Scanning {}", hw.smbus_status()));
    for addr in 0x08u8..=0x77 {
        let r00 = bus.read_byte(addr, 0x00);
        let r20 = bus.read_byte(addr, 0x20);
        if r00.is_none() && r20.is_none() {
            continue;
        }
        let hex2 = |v: Option<u8>| v.map(|b| format!("{b:02X}")).unwrap_or_else(|| "--".into());
        let hex4 = |v: Option<u16>| v.map(|b| format!("{b:04X}")).unwrap_or_else(|| "--".into());
        let mut line = format!("0x{addr:02X}: r00={} VOUT_MODE(20)={}", hex2(r00), hex2(r20));
        for (reg, name) in [(0x01u8, "OPERATION"), (0x02, "ON_OFF"), (0x98, "PMBUS_REV"), (0x19, "CAPABILITY")] {
            let _ = write!(line, " {name}={}", hex2(bus.read_byte(addr, reg)));
        }
        for (reg, name) in [
            (0x21u8, "VOUT_CMD"),
            (0x22, "VOUT_TRIM"),
            (0x23, "VOUT_CAL_OFS"),
            (0x24, "VOUT_MAX"),
            (0x8B, "READ_VOUT"),
            (0x8C, "READ_IOUT"),
            (0x8D, "READ_TEMP"),
            (0x88, "READ_VIN"),
            (0x96, "READ_POUT"),
        ] {
            let _ = write!(line, " {name}={}", hex4(bus.read_word(addr, reg)));
        }
        w(line);
    }
    write_file(report_path, &native::lock(&sb));
    0
}

/// Diagnostic: does the CPU follow small changes through each write path? Every change is
/// tiny (10 mV, one ratio step down) and the exact previous values are restored afterwards.
fn voltage_test(report_path: &Path) -> i32 {
    let sb = Arc::new(std::sync::Mutex::new(String::new()));
    let w = |s: String| {
        let mut b = native::lock(&sb);
        b.push_str(&s);
        b.push_str("\r\n");
    };
    let hw = HardwareModel::new();
    {
        let sb = sb.clone();
        hw.on_log(move |m| {
            let mut b = native::lock(&sb);
            b.push_str("  log: ");
            b.push_str(m);
            b.push_str("\r\n");
        });
    }
    hw.initialize();
    let Some(cpu) = hw.cpu() else {
        w("This test drives the Intel OC mailbox; not available on this CPU.".into());
        write_file(report_path, &native::lock(&sb));
        return 1;
    };
    let drv = hw.driver().unwrap();
    let thread = cpu.first_p_thread();

    // Busy-load one P-core for a moment and read its VID / ratio and the ring ratio while loaded.
    let sample = || -> anyhow::Result<(f64, i32, i32)> {
        native::run_on_cpu(thread, || {
            let sw = Instant::now();
            let mut x = 1.0001f64;
            while sw.elapsed().as_millis() < 250 {
                x = (x * 1.000001 + 1e-9).sqrt();
            }
            let (mut vsum, mut n, mut r) = (0.0, 0, 0);
            for _ in 0..20 {
                let v = drv.read_msr(IntelCpu::MSR_IA32_PERF_STATUS, -1).unwrap_or(0);
                vsum += ((v >> 32) & 0xFFFF) as f64 / 8192.0;
                n += 1;
                r = ((v >> 8) & 0xFF) as i32;
                for _ in 0..20000 {
                    x = (x + 1e-9).sqrt();
                }
            }
            std::hint::black_box(x);
            let u = drv.read_msr(IntelCpu::MSR_UNCORE_PERF_STATUS, -1).unwrap_or(0);
            (vsum / n as f64, r, (u & 0x7F) as i32)
        })
    };

    let r: anyhow::Result<()> = (|| {
        let mb = cpu.mailbox();
        let s0 = mb.read_domain(0)?;
        let base0 = sample()?;
        w(format!("Baseline: dom0 {s0}; loaded VID {:.3} V at x{}, ring x{}", base0.0, base0.1, base0.2));

        let vf = |max_ratio: i32, override_mode: bool, target_volts: f64, offset_volts: f64| VfDomainSettings {
            max_ratio,
            override_mode,
            target_volts,
            offset_volts,
        };

        // 1. Override target +10 mV (keep mode as the BIOS left it)
        let t = vf(s0.max_ratio, true, (if s0.override_mode { s0.target_volts } else { base0.0 }) + 0.010, s0.offset_volts);
        mb.write_domain(0, &t)?;
        native::sleep_ms(300);
        let rb = mb.read_domain(0)?;
        let a1 = sample()?;
        w(format!(
            "Override +10mV: wrote {t}; readback {rb}; loaded VID {:.3} V (delta {:+.0} mV)",
            a1.0,
            (a1.0 - base0.0) * 1000.0
        ));
        mb.write_domain(0, &s0)?;
        native::sleep_ms(200);

        // 2. Offset -10 mV in the current mode
        let o = vf(s0.max_ratio, s0.override_mode, s0.target_volts, s0.offset_volts - 0.010);
        mb.write_domain(0, &o)?;
        native::sleep_ms(300);
        let rb = mb.read_domain(0)?;
        let a2 = sample()?;
        w(format!(
            "Offset -10mV: wrote {o}; readback {rb}; loaded VID {:.3} V (delta {:+.0} mV)",
            a2.0,
            (a2.0 - base0.0) * 1000.0
        ));
        mb.write_domain(0, &s0)?;
        native::sleep_ms(200);

        // 3. Adaptive offset -10 mV (mode bit cleared)
        let ad = vf(s0.max_ratio, false, 0.0, -0.010);
        mb.write_domain(0, &ad)?;
        native::sleep_ms(300);
        let rb = mb.read_domain(0)?;
        let a3 = sample()?;
        w(format!(
            "Adaptive offset -10mV: wrote {ad}; readback {rb}; loaded VID {:.3} V (delta {:+.0} mV)",
            a3.0,
            (a3.0 - base0.0) * 1000.0
        ));
        mb.write_domain(0, &s0)?;
        native::sleep_ms(200);
        w(format!("Restored dom0: {}; loaded VID {:.3} V", mb.read_domain(0)?, sample()?.0));

        // 4. Ring: MSR 0x620 max ratio -1
        let (rmax, rmin) = cpu.read_ring_ratio()?;
        let d2 = mb.read_domain(2)?;
        w(format!("Ring baseline: 0x620 max {rmax} min {rmin}; mailbox dom2 {d2}; loaded ring x{}", base0.2));
        cpu.write_ring_ratio(rmax - 1, None)?;
        native::sleep_ms(300);
        let (rmax2, rmin2) = cpu.read_ring_ratio()?;
        let r1 = sample()?;
        w(format!("0x620 max={}: readback max {rmax2} min {rmin2}; loaded ring x{}", rmax - 1, r1.2));
        cpu.write_ring_ratio(rmax, Some(rmin))?;
        native::sleep_ms(200);

        // 5. Ring: mailbox domain 2 ratio -1
        let d2n = vf(d2.max_ratio - 1, d2.override_mode, d2.target_volts, d2.offset_volts);
        mb.write_domain(2, &d2n)?;
        native::sleep_ms(300);
        let d2rb = mb.read_domain(2)?;
        let r2 = sample()?;
        let ring_now = cpu.read_ring_ratio()?;
        w(format!(
            "mailbox dom2 ratio={}: readback {d2rb}; 0x620 now ({}, {}); loaded ring x{}",
            d2.max_ratio - 1,
            ring_now.0,
            ring_now.1,
            r2.2
        ));
        mb.write_domain(2, &d2)?;
        cpu.write_ring_ratio(rmax, Some(rmin))?;
        native::sleep_ms(200);
        let ring_now = cpu.read_ring_ratio()?;
        w(format!(
            "Restored ring: 0x620 ({}, {}); dom2 {}; loaded ring x{}",
            ring_now.0,
            ring_now.1,
            mb.read_domain(2)?,
            sample()?.2
        ));

        // 6. Core ratio: 0x1AD vs mailbox dom0 ratio
        let d0 = mb.read_domain(0)?;
        w(format!(
            "Core ratio: 0x1AD all-core {}, mailbox dom0 max ratio {}, loaded x{}",
            cpu.read_p_core_all_core_ratio()?,
            d0.max_ratio,
            base0.1
        ));

        // 7. Does the VRM follow the VID? Package power under an all-core load scales with V^2.
        let power_under_load = |seconds: u64| -> anyhow::Result<f64> {
            let load = Load::start(p_core_count(&cpu));
            native::sleep_ms(700); // settle
            let e0 = cpu.read_package_energy_joules()?;
            let sw = Instant::now();
            native::sleep_ms(seconds * 1000);
            let e1 = cpu.read_package_energy_joules()?;
            load.stop();
            let mut de = e1 - e0;
            if de < 0.0 {
                de += 65536.0;
            }
            Ok(de / sw.elapsed().as_secs_f64())
        };
        let _ = power_under_load(1)?; // warm-up, discarded
        // Interleaved A/B with the load running continuously: removes the drift seen in single phases.
        {
            let load = Load::start(p_core_count(&cpu));
            native::sleep_ms(1500);
            let up = vf(s0.max_ratio, true, (if s0.override_mode { s0.target_volts } else { base0.0 }) + 0.040, s0.offset_volts);
            let phase = |s: &VfDomainSettings| -> anyhow::Result<f64> {
                mb.write_domain(0, s)?;
                native::sleep_ms(300);
                let e0 = cpu.read_package_energy_joules()?;
                let sw = Instant::now();
                native::sleep_ms(1200);
                let mut de = cpu.read_package_energy_joules()? - e0;
                if de < 0.0 {
                    de += 65536.0;
                }
                Ok(de / sw.elapsed().as_secs_f64())
            };
            let (mut base_p, mut up_p) = (Vec::new(), Vec::new());
            let phases: anyhow::Result<()> = (|| {
                for _ in 0..4 {
                    base_p.push(phase(&s0)?);
                    up_p.push(phase(&up)?);
                }
                Ok(())
            })();
            let restore = mb.write_domain(0, &s0);
            load.stop();
            phases?;
            restore?;
            let mut vid_up = 0.0;
            let _ = (|| -> anyhow::Result<()> {
                mb.write_domain(0, &up)?;
                native::sleep_ms(200);
                vid_up = sample()?.0;
                mb.write_domain(0, &s0)
            })();
            let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            let (mb0, mb1) = (avg(&base_p), avg(&up_p));
            let join = |v: &[f64]| v.iter().map(|p| format!("{p:.1}")).collect::<Vec<_>>().join("/");
            w(format!(
                "VRM check (4 interleaved rounds, all-core load): base {} W avg {mb0:.1}; +40 mV {} W avg {mb1:.1}; ratio {:.3}, expected ~{:.3} if the VRM follows VID ({:.3} -> {vid_up:.3})",
                join(&base_p),
                join(&up_p),
                mb1 / mb0,
                (vid_up / base0.0).powi(2),
                base0.0
            ));
        }
        w(format!("Restored dom0: {}", mb.read_domain(0)?));
        Ok(())
    })();
    if let Err(e) = r {
        w(format!("ERROR: {e:?}"));
    }
    write_file(report_path, &native::lock(&sb));
    0
}

/// Raw SMU mailbox transaction for research: --smu rsmu|mp1|hsmp 0xMSG [arg0 arg1 ...].
/// Prints the status and the six argument registers after the call. Anything sent here goes
/// straight to the firmware; know what the message does before using it.
fn smu_command(a: &[String]) -> i32 {
    let hw = model_quiet();
    let Some(amd) = hw.amd().filter(|_| hw.smu_available()) else {
        println!("No AMD SMU.");
        return 1;
    };
    let smu = amd.smu();
    let msgs = smu.messages();
    let mb = match a[0].to_lowercase().as_str() {
        "rsmu" => Some(msgs.rsmu),
        "mp1" => Some(msgs.mp1),
        "hsmp" => Some(msgs.hsmp),
        _ => None,
    };
    let Some(mb) = mb else {
        println!("mailbox must be rsmu, mp1 or hsmp");
        return 1;
    };
    let Some(msg) = parse_u32(&a[1]) else {
        println!("'{}' is not a number", a[1]);
        return 1;
    };
    let mut margs = [0u32; 6];
    for i in 2..a.len().min(8) {
        match parse_u32(&a[i]) {
            Some(v) => margs[i - 2] = v,
            None => {
                println!("'{}' is not a number", a[i]);
                return 1;
            }
        }
    }
    let st = smu.send(&mb, msg, &mut margs);
    let line = format!(
        "{} (msg 0x{:08X}) message 0x{msg:X}: {} (0x{:02X}); args = {}",
        mb.name,
        mb.msg,
        AmdSmu::describe(st),
        st.code(),
        margs.iter().map(|x| format!("0x{x:08X}")).collect::<Vec<_>>().join(" ")
    );
    println!("{line}");
    append_file(&app_dir().join("smu.txt"), &format!("[{}] {line}\r\n", chrono::Local::now().format("%H:%M:%S")));
    if st == SmuStatus::Ok { 0 } else { 2 }
}

/// Reads a run of SMN registers: --smn 0x73000 [count]. Prints hex and the SVI3 / SVI2 VID decodes, to smn.txt as well.
fn smn_dump(start: &str, count: i32) -> i32 {
    let mut sb = String::new();
    let hw = model_quiet();
    let Some(amd) = hw.amd() else {
        println!("No AMD CPU.");
        return 1;
    };
    let Some(a0) = parse_u32(start) else {
        println!("'{start}' is not a number");
        return 1;
    };
    for i in 0..count.max(0) as u32 {
        let a = a0 + i * 4;
        let line = match amd.smu().read_smn(a) {
            Some(v) => {
                let (svi3, svi2) = ((v >> 6) & 0x1FF, v >> 24);
                let hi = (v >> 16) & 0x1FF;
                format!(
                    "0x{a:08X} = 0x{v:08X}  svi3[14:6]={svi3} -> {:.3} V  svi3[24:16]={hi} -> {:.3} V  svi2[31:24]={svi2} -> {:.3} V  low16={}",
                    0.245 + svi3 as f64 * 0.005,
                    0.245 + hi as f64 * 0.005,
                    1.55 - svi2 as f64 * 0.00625,
                    v & 0xFFFF
                )
            }
            None => format!("0x{a:08X} = read failed"),
        };
        println!("{line}");
        sb.push_str(&line);
        sb.push_str("\r\n");
    }
    append_file(&app_dir().join("smn.txt"), &sb);
    0
}

/// Applies one row by id through the same path the window uses: --apply ppt 150. Logs to the console and apply.txt.
fn apply_setting(id: &str, text: &str) -> i32 {
    let sb = Arc::new(std::sync::Mutex::new(String::new()));
    let w = {
        let sb = sb.clone();
        move |m: &str| {
            println!("{m}");
            let mut b = native::lock(&sb);
            b.push_str(m);
            b.push_str("\r\n");
        }
    };
    let hw = HardwareModel::new();
    {
        let w = w.clone();
        hw.on_log(move |m| w(&format!("  log: {m}")));
    }
    hw.initialize();
    let rc = match hw.setting(id) {
        None => {
            let ids: Vec<String> = hw.settings().iter().map(|s| s.id.clone()).collect();
            w(&format!("no row '{id}'. Rows: {}", ids.join(", ")));
            1
        }
        Some(s) => match s.try_parse(text) {
            None => {
                w(&format!("'{text}' is not a number"));
                1
            }
            Some(v) => {
                w(&format!("{}: before = {} {}", s.name, s.current_text(), s.unit));
                let ok = hw.apply(&s.id, v);
                let after = hw.setting(&s.id).unwrap();
                w(&format!("{}: apply {}, now = {} {}", s.name, if ok { "OK" } else { "FAILED" }, after.current_text(), s.unit));
                if ok { 0 } else { 2 }
            }
        },
    };
    append_file(&app_dir().join("apply.txt"), &native::lock(&sb));
    rc
}

/// Dumps the whole SMU power table as index / offset / float, for layout research on a new firmware.
fn pm_dump(report_path: &Path) -> i32 {
    let sb = Arc::new(std::sync::Mutex::new(String::new()));
    let hw = HardwareModel::new();
    {
        let sb = sb.clone();
        hw.on_log(move |m| {
            let mut b = native::lock(&sb);
            b.push_str("  log: ");
            b.push_str(m);
            b.push_str("\r\n");
        });
    }
    hw.initialize();
    let mut out = native::lock(&sb).clone();
    let Some(amd) = hw.amd().filter(|_| hw.smu_available()) else {
        out.push_str("No AMD SMU.\r\n");
        write_file(report_path, &out);
        return 1;
    };
    let smu = amd.smu();
    let _ = write!(
        out,
        "{} - {}; SMU {}; table v0x{:08X} at 0x{:X} size 0x{:X}\r\n",
        amd.brand_string(),
        amd.code_name(),
        smu.version_text(),
        smu.table_version(),
        smu.table_address(),
        smu.table_size()
    );
    let table = if smu.refresh_table() { smu.table() } else { None };
    let Some(t) = table else {
        let _ = write!(out, "table read failed: {}\r\n", smu.last_table_error().unwrap_or_default());
        write_file(report_path, &out);
        return 1;
    };
    out.push_str("index  offset  value\r\n");
    for (i, v) in t.iter().enumerate() {
        let _ = write!(out, "{i:>5}  0x{:04X}  {}\r\n", i * 4, trim_4(*v as f64));
    }
    write_file(report_path, &out);
    println!("wrote {} floats to {}", t.len(), report_path.display());
    0
}

/// .NET "0.####".
fn trim_4(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// .NET "0.##".
fn trim_2(v: Option<f32>) -> String {
    match v {
        None => String::new(),
        Some(v) => {
            let s = format!("{v:.2}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s == "-0" { "0".into() } else { s.to_string() }
        }
    }
}

/// .NET "0.###" right-aligned in 10 columns.
fn trim_3_10(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{:>10}", if s == "-0" { "0" } else { s })
}

/// Lists the switches.
fn help() -> i32 {
    println!(
        "Roch CPU {} - CPU and memory tuning for Intel LGA1700 and AMD Ryzen.
Run with no arguments for the window. Everything below needs administrator rights.

Reporting
  --probe [file]        hardware readings and current settings; no tuning writes.
  --intel-audit [file]  per-thread Intel register and mailbox readings.
  --asus-audit         ASUS control metadata and measured clocks; no tuning writes.
  --windows-audit      read this boot's restricted-MSR events; no hardware access.
  --ring-validate      briefly lower the ring ceiling, check live clock, restore.
  --ecore-validate N   test E-core ratio (up to 44 and one step above current), restore.
  --intel-validate V   temporary lower P-core ceiling and voltage-target check;
                       closes no apps, requires one instance, restores snapshots.
                        Attach this when reporting a board that misbehaves.
  --help                this list.

Checking a control that seems to do nothing
  --asus-voltage-validate  test ASUS Manual SVID at 1.275/1.300 V, then restore.
  --asus-bclk-validate     test nominal 101 MHz in measured steps, then restore.
  --asus-rail-validate sa|l2 volts  test the selected ASUS voltage and sensor response, then restore.
  --asus-ring-voltage-validate    test Cache SVID at 1.300 V, verify CPU target, then restore.
  --vtest [file]        nudge core voltage and ring ratio, confirm the CPU follows, restore.
  --vcore-test [file]   raise the core voltage request, measure the rail the VRM produces,
                        and say plainly whether the board follows the CPU.
  --bclk-test           the three ways of measuring base clock side by side. Only the
                        core-cycle one sees a change; the other two are blind by design.

Base clock (Intel, MSI EC boards)
  --clkgen              read the clock generator's divider and the clock it produces.
  --clkgen-dump [a] [b] its whole register map, read-only.
  --bclk-cal            measure the oscillator, confirm the divider model, restore.
  --bclk-probe [d...]   write a series of dividers, measure each, restore.
  --bclk-set <MHz>      set the base clock, measuring every step.

CPU VDD2 (Intel, MSI EC boards)
  --vdd2                report the regulator's code and the measured rail.
  --vdd2 <volts>        set it, one code step at a time, measuring the rail after each.

Buses and sensors
  --sio-dump            every Super I/O voltage channel.
  --sio-ldn             the Super I/O's logical devices and their I/O windows.
  --smbus-scan          devices answering on the SMBus.
  --smb-regs [file]     256 registers of each known SMBus device, as binary.
  --smbus-reset         recover an SMBus wedged by another tool (Intel PCH only).

AMD
  --apply <row> <value> apply one row through the same path the window uses.
  --smu <rsmu|mp1|hsmp> <0xMSG> [args]   send one raw mailbox message. Research only:
                        it goes straight to the firmware with no checking.
  --pm-dump [file]      the SMU power table (needs PawnIO).
  --smn <0xADDR> [n]    read SMN registers.",
        crate::APP_VERSION
    );
    0
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn opt_fmt(v: Option<f64>, decimals: usize) -> String {
    v.map(|x| format!("{x:.decimals$}")).unwrap_or_default()
}

/// Hardware readings and current settings; no tuning writes.
fn probe(report_path: &Path) -> i32 {
    let sb = Arc::new(std::sync::Mutex::new(String::new()));
    let w = {
        let sb = sb.clone();
        move |s: &str| {
            let mut b = native::lock(&sb);
            b.push_str(s);
            b.push_str("\r\n");
        }
    };
    let hw = HardwareModel::new();
    {
        let w = w.clone();
        hw.on_log(move |m| w(&format!("  log: {m}")));
    }
    w(&format!("Roch CPU probe {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe_body(&hw, &w)));
    match r {
        Ok(Err(e)) => w(&format!("FATAL: {e:?}")),
        Err(_) => w("FATAL: panic while probing"),
        Ok(Ok(())) => {}
    }
    write_file(report_path, &native::lock(&sb));
    0
}

fn probe_body(hw: &HardwareModel, w: &dyn Fn(&str)) -> anyhow::Result<()> {
    hw.initialize();
    w("");
    w(&format!("Driver       : {}", hw.driver_status()));
    let sm = hw.smbios();
    w(&format!("Board        : {} / BIOS {} {}", sm.mainboard_model(), sm.bios_version, sm.bios_date));
    if let Some(cpu) = hw.cpu() {
        w(&format!(
            "CPU          : {} model 0x{:X} stepping {} ({})",
            cpu.brand_string(),
            cpu.model(),
            cpu.stepping(),
            cpu.generation()
        ));
        w(&format!(
            "Topology     : {}P + {}E, {} threads, hybrid flag={}",
            cpu.p_core_count(),
            cpu.e_core_count(),
            cpu.logical_cpus().len(),
            tf(cpu.is_hybrid())
        ));
        w(&format!(
            "PlatformInfo : base ratio {}, min ratio {}, ratio programmable={}, TDP programmable={}, TjMax {}",
            cpu.base_ratio(),
            cpu.min_ratio(),
            tf(cpu.ratio_limits_programmable()),
            tf(cpu.tdp_programmable()),
            cpu.tj_max()
        ));
        w(&format!(
            "OC lock      : {}   mailbox available: {}   PL locked: {}",
            tf(hw.oc_locked()),
            tf(hw.mailbox_available()),
            tf(hw.power_limits_locked())
        ));
        let (pr, pc) = cpu.read_p_core_turbo_table()?;
        w(&format!(
            "P turbo table: {}",
            pr.iter().enumerate().map(|(i, r)| format!("{}c={r}", pc[i])).collect::<Vec<_>>().join(" ")
        ));
        if cpu.e_core_count() > 0 {
            let (er, ec) = cpu.read_e_core_turbo_table()?;
            w(&format!(
                "E turbo table: {}",
                er.iter().enumerate().map(|(i, r)| format!("{}c={r}", ec[i])).collect::<Vec<_>>().join(" ")
            ));
        }
        let ring = cpu.read_ring_ratio()?;
        w(&format!("Ring         : max {} min {} current {}", ring.0, ring.1, cpu.read_current_ring_ratio()));
        let pl = cpu.read_package_power_limits()?;
        w(&format!(
            "Power limits : PL1 {} W (en={}) PL2 {} W (en={}) locked={}",
            pl.pl1,
            tf(pl.pl1_enabled),
            pl.pl2,
            tf(pl.pl2_enabled),
            tf(pl.locked)
        ));
        if hw.mailbox_available() {
            // Domain 5 is the E-core L2 rail and domain 3 the uncore, established by
            // matching every domain against the vendor tool's displayed values; published
            // tables commonly put E-core L2 at 3, which reads a rail nobody set.
            for (id, name) in [(0, "core"), (1, "gt"), (2, "ring"), (3, "uncore"), (4, "sa"), (5, "ecore-l2")] {
                match cpu.mailbox().read_domain(id) {
                    Ok(d) => w(&format!(
                        "Mailbox dom {id} ({name}): {d}  IccMax={} A",
                        cpu.mailbox().read_icc_max(id).map(|v| v.to_string()).unwrap_or_else(|| "n/a".into())
                    )),
                    Err(e) => w(&format!("Mailbox dom {id} ({name}): {e}")),
                }
            }
        }
        w(&format!(
            "BCLK         : {} MHz ({})",
            hw.measure_bclk().map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into()),
            hw.bclk().map(|b| b.status().to_string()).unwrap_or_default()
        ));
        let _ = hw.read_live();
        native::sleep_ms(500);
        let live = hw.read_live();
        w(&format!(
            "Live         : {} C, x{} = {} MHz, VID {} V, ring x{}, {} W",
            opt(live.package_temp_c),
            opt(live.core_ratio),
            opt_fmt(live.core_mhz, 1),
            opt_fmt(live.core_vid, 3),
            opt(live.ring_ratio),
            opt_fmt(live.package_watts, 1)
        ));

        w("Readback alone does not establish that a setting reached the hardware. Use controlled validation for that.");
    }
    if let Some(amd) = hw.amd() {
        let smu = amd.smu();
        w(&format!(
            "CPU          : {} family {:X}h model {:02X}h stepping {} package type {} - {}, {}",
            amd.brand_string(),
            amd.family(),
            amd.model(),
            amd.stepping(),
            amd.package_type(),
            amd.code_name(),
            amd.generation()
        ));
        w(&format!(
            "Topology     : {} cores / {} threads, {} threads per core, {} CCD(s); {}",
            amd.core_count(),
            amd.logical_count(),
            amd.threads_per_core(),
            amd.ccd_count(),
            amd.topology_note()
        ));
        w(&format!(
            "Cores        : {}",
            amd.cores()
                .iter()
                .map(|c| format!(
                    "{}={} thr[{}] mask 0x{:08X}",
                    c.label(),
                    c.location(),
                    c.threads.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","),
                    smu.core_mask(c.ccd, c.core_in_ccd)
                ))
                .collect::<Vec<_>>()
                .join("  ")
        ));
        w(&format!("P0 ratio     : {}   microcode 0x{:08X}", amd.base_ratio(), amd.patch_level()));
        w(&format!(
            "SMU          : {}   OC caps {}",
            hw.smu_status(),
            hw.oc_capabilities().map(|c| format!("0x{c:X}")).unwrap_or_else(|| "n/a".into())
        ));
        if hw.smu_available() {
            let (fp, ft, fe) = smu.read_stock_limits();
            let n0 = |v: Option<f64>| v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "n/a".into());
            w(&format!(
                "Stock limits : PPT {} W, TDC {} A, EDC {} A; sustained {}",
                n0(fp),
                n0(ft),
                n0(fe),
                smu.read_sustained_limits().map(|(p, t)| format!("{p} W / {t} C")).unwrap_or_else(|| "n/a".into())
            ));
            w(&format!(
                "Scalar       : {}   boost limit {} MHz",
                smu.read_scalar().map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into()),
                smu.read_boost_limit_mhz().map(|v| v.to_string()).unwrap_or_else(|| "n/a".into())
            ));
            w(&format!(
                "Curve Opt.   : {}",
                amd.cores()
                    .iter()
                    .map(|c| format!(
                        "{}={}",
                        c.label(),
                        smu.read_curve_optimizer(c.ccd, c.core_in_ccd).map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                    ))
                    .collect::<Vec<_>>()
                    .join("  ")
            ));
            w(&format!(
                "Table        : v0x{:08X} at 0x{:X} size 0x{:X} layout {}",
                smu.table_version(),
                smu.table_address(),
                smu.table_size(),
                smu.layout().map(|l| l.name.to_string()).unwrap_or_else(|| "unknown".into())
            ));
            if let Some(t) = if smu.refresh_table() { smu.table() } else { None } {
                w(&format!(
                    "  limits     : PPT {}/{} W  TDC {}/{} A  EDC {}/{} A  THM {} C  socket {} W",
                    trim_2(smu.ppt_limit()),
                    trim_2(smu.ppt_value()),
                    trim_2(smu.tdc_limit()),
                    trim_2(smu.tdc_value()),
                    trim_2(smu.edc_limit()),
                    trim_2(smu.edc_value()),
                    trim_2(smu.thm_limit()),
                    trim_2(smu.socket_power())
                ));
                let mut line = String::from("  table[0..63]:");
                for (i, v) in t.iter().take(64).enumerate() {
                    if i % 8 == 0 {
                        let _ = write!(line, "\r\n    {i:>3}:");
                    }
                    let _ = write!(line, " {}", trim_3_10(*v));
                }
                w(&line);
            } else {
                w(&format!("  table read failed: {}", smu.last_table_error().unwrap_or_default()));
            }
        }
        w(&format!(
            "Temperature  : {} C   VID {} V   max core {:.0} MHz",
            amd.read_temperature().map(|v| format!("{v:.1}")).unwrap_or_else(|| "n/a".into()),
            amd.read_core_vid().map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into()),
            amd.read_max_core_mhz()
        ));
        w(&format!(
            "BCLK         : {} MHz ({})",
            hw.measure_bclk().map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into()),
            hw.bclk().map(|b| b.status().to_string()).unwrap_or_default()
        ));
        let _ = hw.read_live();
        native::sleep_ms(500);
        let live = hw.read_live();
        w(&format!(
            "Live         : {} C, {} MHz, VID {} V, {} W",
            opt(live.package_temp_c),
            opt_fmt(live.core_mhz, 1),
            opt_fmt(live.core_vid, 3),
            opt_fmt(live.package_watts, 1)
        ));
    }
    w("");
    w(&format!("SMBus        : {}", hw.smbus_status()));
    for d in hw.dimms() {
        let (pv, pq, pp) = d.read_power_mw();
        w(&format!(
            "{}: SPD 0x{:02X} PMIC {}  VDD {} VDDQ {} VPP {}  power VDD {} VDDQ {} VPP {} mW",
            d.slot_name(),
            d.spd_address(),
            if d.has_pmic() { format!("0x{:02X} {}", d.pmic_address(), d.pmic_vendor()) } else { "none".into() },
            opt_fmt(d.read_vdd(), 3),
            opt_fmt(d.read_vddq(), 3),
            opt_fmt(d.read_vpp(), 3),
            opt(pv),
            opt(pq),
            opt(pp)
        ));
        if d.has_pmic() {
            w(&format!("  calibration: {}", d.calibration_note()));
            let mut line = String::from("  PMIC regs 0x00-0x3F:");
            for r in 0..0x40u8 {
                if r % 16 == 0 {
                    let _ = write!(line, "\r\n    {r:02X}:");
                }
                match d.read_register(r) {
                    Some(v) => {
                        let _ = write!(line, " {v:02X}");
                    }
                    None => line.push_str(" --"),
                }
            }
            w(&line);
            let mut adc = String::from("  PMIC ADC sweep (select 0..15 -> raw):");
            for sel in 0..16 {
                let _ = write!(adc, " {sel}={}", d.read_adc(sel).map(|b| b.to_string()).unwrap_or_else(|| "--".into()));
            }
            w(&adc);
        }
        let mut spd = String::from("  SPD5118 regs 0x00-0x0F:");
        for r in 0..0x10u8 {
            match d.read_spd_register(r) {
                Some(v) => {
                    let _ = write!(spd, " {v:02X}");
                }
                None => spd.push_str(" --"),
            }
        }
        w(&spd);
    }
    w("");
    w("Settings:");
    for s in hw.settings().iter() {
        w(&format!(
            "  {:<16} {:<34} {:<34} = {} {} {}",
            s.id,
            s.name,
            if s.available { s.range_text() } else { "N/A".into() },
            if s.available { s.current_text() } else { "N/A".into() },
            s.unit,
            if s.read_only() { "[read-only]" } else { "" }
        ));
    }
    Ok(())
}
