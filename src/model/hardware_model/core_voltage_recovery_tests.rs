//! Port of `tests/CoreVoltageRecovery.Model.Tests/Program.cs`: inert startup model checks. No
//! production constructor, real driver, mailbox target command or live hardware access; bus
//! mutexes are isolated before acquisition.

use std::sync::{Arc, Mutex};

use crate::hw::KernelDriver;
use crate::hw::core_page_probe::CorePageVerificationResult;
use crate::hw::ec_mailbox::fakes::{EcFakeDriver, fake_sio};
use crate::hw::intel_cpu::IntelCpu;
use crate::hw::msi_core_voltage::MsiCoreVoltage;
use crate::hw::native::lock;
use crate::hw::oc_mailbox::OcMailbox;
use crate::hw::smbios::SmbiosInfo;
use crate::hw::super_io::{NamedMutex, SuperIo, SuperIoKind};
use crate::model::hardware_model::HardwareModel;
use crate::model::setting::{Setting, SettingGroup};

fn check(ok: bool, message: &str) {
    assert!(ok, "{message}");
}

/// What the C# test set by reflection on the uninitialised model. A gate case edits these before
/// the model is assembled (the Rust model's hardware handles are set-once).
struct ModelParts {
    driver: Option<Arc<dyn KernelDriver>>,
    sio: Option<Arc<SuperIo>>,
    cpu: Option<Arc<IntelCpu>>,
    smbios: SmbiosInfo,
    mailbox_available: bool,
    voltage_access_restriction: Option<String>,
}

fn fake_model(parts: ModelParts) -> HardwareModel {
    // Bypass all production constructors: no Initialize, SMBIOS, CPUID topology,
    // WinRing0Driver, application entry point, hardware detection or driver lifecycle.
    let model = HardwareModel::for_test(parts.smbios);
    if let Some(d) = parts.driver {
        model.set_driver_for_test(d);
    }
    if let Some(s) = parts.sio {
        model.set_super_io_for_test(s);
    }
    if let Some(c) = parts.cpu {
        model.set_cpu_for_test(c);
    }
    model.set_mailbox_available_for_test(parts.mailbox_available);
    model.set_voltage_access_restriction_for_test(parts.voltage_access_restriction);
    let mut core_off = Setting::new("core_off", "Core offset", SettingGroup::Voltages);
    core_off.current = Some(-20.0);
    core_off.default_value = Some(-10.0);
    let mut dimm = Setting::new("dimm0_vdd", "DIMM VDD", SettingGroup::Memory);
    dimm.current = Some(1.4);
    dimm.default_value = Some(1.35);
    model.settings().extend([core_off, dimm]);
    model
}

fn with_model(test: impl FnOnce(&Arc<EcFakeDriver>, &HardwareModel)) {
    with_model_configured(|_, _| {}, test);
}

fn with_model_configured(
    configure: impl FnOnce(&Arc<EcFakeDriver>, &mut ModelParts),
    test: impl FnOnce(&Arc<EcFakeDriver>, &HardwareModel),
) {
    let driver = EcFakeDriver::with(|s| s.simulate_mailbox_reads = true);
    let sio = fake_sio(&driver);
    let drv: Arc<dyn KernelDriver> = driver.clone();
    let cpu = Arc::new(IntelCpu::with_driver_only_for_test(drv.clone()));
    let smbios = SmbiosInfo { board_manufacturer: "Micro-Star International".into(), ..SmbiosInfo::default() };
    let mut parts = ModelParts {
        driver: Some(drv),
        sio: Some(sio),
        cpu: Some(cpu),
        smbios,
        mailbox_available: true,
        voltage_access_restriction: None,
    };
    configure(&driver, &mut parts);
    let model = fake_model(parts);
    test(&driver, &model);
    drop(model); // C# `finally { model.CoreVoltage?.Dispose(); }`
    check(driver.regulator_writes().iter().all(|w| w.0 == 0), "Startup/refresh wrote a voltage or mode register");
    check(
        driver.mailbox_commands().iter().all(|c| *c == OcMailbox::CMD_READ_VF),
        "Startup/refresh issued a mailbox target/mode command",
    );
}

/// Uses the same private implementation called by the public fixed-factory entry.
/// Only the factory's mutex handles differ: fake EC, isolated unnamed mutexes.
fn initialize(model: &HardwareModel) -> CorePageVerificationResult {
    model.initialize_core_voltage_core(&MsiCoreVoltage::startup_for_test)
}

/// The production Intel row builder's mailbox reads go to the fake driver,
/// which rejects every mailbox target/mode command before changing anything.
fn build_core_row(model: &HardwareModel) -> Setting {
    model.settings().clear();
    let rows = model.build_intel_settings_for_test();
    let mut cores = rows.into_iter().filter(|s| s.id == "core_v");
    let core = cores.next().expect("no core_v row");
    assert!(cores.next().is_none(), "more than one core_v row");
    model.settings().push(core.clone()); // Refresh only the affected core target.
    core
}

/// The installed core row, as refreshed by the model (the C# test kept the row reference).
fn core_row(model: &HardwareModel) -> Setting {
    model.setting("core_v").expect("core_v row missing")
}

/// C# `ReferenceEquals` on the recorded result: the Rust model hands out clones of the one
/// recorded value, so compare by value (callers also check no further I/O or logs occurred).
fn same(a: &CorePageVerificationResult, b: &CorePageVerificationResult) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

fn log_sink(model: &HardwareModel) -> Arc<Mutex<Vec<String>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    model.on_log(move |line| lock(&l).push(line.to_string()));
    log
}

fn startup_trace_complete(log: &[String], result: &CorePageVerificationResult) -> bool {
    let io: Vec<String> = log.iter().filter(|l| l.starts_with("CPU core startup I/O: ")).cloned().collect();
    let expected: Vec<String> = result.diagnostic_lines.iter().map(|l| format!("CPU core startup I/O: {l}")).collect();
    log.iter().filter(|l| l.starts_with("CPU core regulator startup:")).count() == 1
        && io == expected
        && log.len() == 1 + result.diagnostic_lines.len()
}

#[test]
fn automatic_page_1_startup_needs_no_prior_core_row_normal_row_builder_enables_genuine_target() {
    with_model(|driver, model| {
        let snapshot = |m: &HardwareModel| -> Vec<(String, Option<f64>, Option<f64>)> {
            m.settings().iter().map(|s| (s.id.clone(), s.current, s.default_value)).collect()
        };
        let previous = snapshot(model);
        let result = initialize(model);
        check(
            result.succeeded
                && model.core_voltage().is_some()
                && model.core_voltage_initialization_result().is_some_and(|r| same(&r, &result)),
            &result.status,
        );
        check(model.core_voltage_status() == "Core voltage ready.", "Successful startup status was not concise");
        check(
            snapshot(model) == previous
                && previous[0].1 == Some(-20.0)
                && previous[0].2 == Some(-10.0)
                && previous[1].1 == Some(1.4)
                && previous[1].2 == Some(1.35),
            "Startup mutated unrelated target/default settings",
        );
        let core = build_core_row(model);
        model.refresh_all(true);
        check(
            core.available && !core.read_only() && core.write.is_some() && core.restore_default.is_some(),
            "Normal startup row stayed disabled or bypassed paired writers",
        );
        let core = core_row(model);
        check(
            core.current == Some(1.280)
                && core.default_value == Some(1.280)
                && model.core_voltage().unwrap().verified_baseline().unwrap().target_volts() == Some(1.280),
            "Target/default was invented or came from fake mailbox's different 0.900 V value",
        );
        check(
            driver.page() == 1 && driver.regulator_writes() == vec![(0u8, 0u16), (0, 1)],
            "Startup/refresh failed PAGE restoration or selected more than once",
        );
    });
}

#[test]
fn automatic_page_0_startup_default_capture_never_write_page() {
    with_model(|driver, model| {
        driver.st().page = 0;
        let result = initialize(model);
        let built = build_core_row(model);
        model.refresh_all(true);
        let core = core_row(model);
        check(
            result.succeeded
                && result.original_page == Some(0)
                && result.restored_page == Some(0)
                && built.available
                && core.current == Some(1.280)
                && core.default_value == Some(1.280)
                && !model.core_voltage().unwrap().target_read_is_cached()
                && driver.regulator_writes().is_empty(),
            &result.status,
        );
    });
}

#[test]
fn startup_adaptive_mode_is_editable_auto_with_genuine_null_default() {
    for mode in [0u16, 1, 2] {
        with_model(|driver, model| {
            driver.st().mode = mode << 4;
            let result = initialize(model);
            let built = build_core_row(model);
            model.refresh_all(true);
            let core = core_row(model);
            check(
                result.succeeded
                    && built.available
                    && !built.read_only()
                    && core.current.is_none()
                    && core.default_value.is_none()
                    && model.core_voltage().unwrap().verified_baseline().unwrap().target_mv == 1280
                    && !model.core_voltage_target_read_failed()
                    && driver.page() == 1,
                &format!("startup adaptive mode {mode}: Adaptive state was fabricated as override, disabled, or confused with failed read"),
            );
        });
    }
}

#[test]
fn startup_adaptive_zero_stays_real_auto_rather_than_an_unread_target() {
    with_model(|driver, model| {
        {
            let mut s = driver.st();
            s.mode = 0;
            s.target = 0;
        }
        let result = initialize(model);
        let built = build_core_row(model);
        model.refresh_all(true);
        let core = core_row(model);
        check(
            result.succeeded
                && built.available
                && core.current.is_none()
                && core.default_value.is_none()
                && !model.core_voltage_target_read_failed()
                && driver.page() == 1,
            &result.status,
        );
    });
}

#[test]
fn startup_success_is_cached_once_without_further_io_page_changes_baseline_changes_logs() {
    with_model(|driver, model| {
        let log = log_sink(model);
        let first = initialize(model);
        let control = model.core_voltage();
        let startup_log = lock(&log).clone();
        check(startup_trace_complete(&startup_log, &first), "Startup summary/diagnostic trace was duplicated or incomplete");
        let (reads, writes) = (driver.io_reads(), driver.io_writes());
        driver.st().target = 1300;
        for _ in 0..5 {
            check(same(&first, &initialize(model)), "Startup result regenerated");
        }
        let control = control.unwrap();
        check(
            first.succeeded
                && model.core_voltage().is_some_and(|c| Arc::ptr_eq(&c, &control))
                && control.verified_baseline().unwrap().target_mv == 1280
                && driver.io_reads() == reads
                && driver.io_writes() == writes
                && *lock(&log) == startup_log,
            "Repeated initialization re-probed, changed the baseline, or duplicated startup logs",
        );
    });
}

#[test]
fn concurrent_startup_calls_perform_one_bounded_attempt_and_share_the_recorded_result() {
    with_model(|driver, model| {
        let log = log_sink(model);
        let results: Vec<CorePageVerificationResult> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| initialize(model))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let first = &results[0];
        let log = lock(&log).clone();
        check(
            first.succeeded
                && results.iter().all(|r| same(r, first))
                && startup_trace_complete(&log, first)
                && driver.regulator_reads().iter().filter(|r| **r == 0xF0).count() == 1
                && driver.regulator_writes() == vec![(0u8, 0u16), (0, 1)],
            "Concurrent initialization performed multiple probes/logs/page changes",
        );
    });
}

#[test]
fn page_1_polling_preserves_cached_target_without_page_writes_page_0_polling_becomes_fresh() {
    with_model(|driver, model| {
        check(initialize(model).succeeded, "Startup failed");
        build_core_row(model);
        model.refresh_all(true);
        driver.st().regulator_writes.clear();
        for _ in 0..4 {
            model.refresh("core_v");
        }
        check(
            core_row(model).current == Some(1.280)
                && model.core_voltage().unwrap().target_read_is_cached()
                && model.core_voltage_target_read_status().contains("PAGE 1")
                && driver.regulator_writes().is_empty(),
            "Cached polling selected PAGE or read another rail",
        );
        {
            let mut s = driver.st();
            s.page = 0;
            s.target = 1300;
        }
        model.refresh("core_v");
        let core = core_row(model);
        check(
            core.current == Some(1.300)
                && core.default_value == Some(1.280)
                && !model.core_voltage().unwrap().target_read_is_cached()
                && driver.regulator_writes().is_empty(),
            "Fresh target/default or polling PAGE behavior was incorrect",
        );
    });
}

#[test]
fn polling_failure_is_unread_startup_default_stays_genuine_and_adaptive_null_distinct() {
    with_model(|driver, model| {
        check(initialize(model).succeeded, "Startup failed");
        build_core_row(model);
        model.refresh_all(true);
        {
            let mut s = driver.st();
            s.regulator_writes.clear();
            s.page = 0;
            s.fail_mode_result_once = true;
        }
        model.refresh_all(true);
        let core = core_row(model);
        check(
            core.current.is_none()
                && core.default_value == Some(1.280)
                && model.core_voltage_target_read_failed()
                && model.core_voltage_target_read_status().starts_with("Core target not read:")
                && driver.regulator_writes().is_empty(),
            "Read failure fabricated Auto/default or selected PAGE",
        );
        {
            let mut s = driver.st();
            s.mode = 0;
            s.target = 0;
        }
        model.refresh("core_v");
        let core = core_row(model);
        check(
            core.current.is_none()
                && !model.core_voltage_target_read_failed()
                && core.default_value == Some(1.280)
                && driver.regulator_writes().is_empty(),
            "Real adaptive null was confused with failed read",
        );
    });
}

#[test]
fn unsupported_polling_page_fails_closed_and_never_retries_startup_selection() {
    with_model(|driver, model| {
        let initial = initialize(model);
        build_core_row(model);
        model.refresh_all(true);
        {
            let mut s = driver.st();
            s.regulator_writes.clear();
            s.page = 2;
        }
        model.refresh("core_v");
        model.refresh("core_v");
        let core = core_row(model);
        check(
            core.current.is_none()
                && core.default_value == Some(1.280)
                && model.core_voltage_target_read_failed()
                && driver.regulator_writes().is_empty()
                && same(&initial, &initialize(model)),
            "Unsupported polling PAGE retried discovery or exposed another rail",
        );
    });
}

#[test]
fn strict_initial_page_read_failure_is_recorded_once_and_never_recovers_silently_during_refresh() {
    with_model(|driver, model| {
        driver.st().fail_page_result_once = true;
        let first = initialize(model);
        check(
            !first.succeeded
                && first.original_page.is_none()
                && model.core_voltage().is_none()
                && driver.page() == 1
                && driver.regulator_writes().is_empty(),
            &first.status,
        );
        let core = build_core_row(model);
        let (reads, writes) = (driver.io_reads(), driver.io_writes());
        check(
            !core.available && core.read_only() && core.write.is_none() && core.restore_default.is_none(),
            "Failed startup row can write an unverified rail",
        );
        model.refresh_all(true);
        model.refresh("core_v");
        let core = core_row(model);
        check(
            same(&first, &initialize(model))
                && driver.io_reads() == reads
                && driver.io_writes() == writes
                && core.current.is_none()
                && core.default_value.is_none()
                && model.core_voltage_status().starts_with("Core voltage unavailable:"),
            "Unavailable polling/failed initialization retried page access",
        );
    });
}

#[test]
fn startup_failure_leaves_an_unavailable_row_and_one_recorded_attempt() {
    for failure in [
        "mode-read",
        "target-read",
        "secondary-read",
        "select-after-change",
        "select-mismatch",
        "restore-after-change",
        "restore-mismatch",
        "snapshot-drift",
    ] {
        with_model(|driver, model| {
            {
                let mut s = driver.st();
                match failure {
                    "mode-read" => s.fail_mode_result_once = true,
                    "target-read" => s.fail_target_result_once = true,
                    "secondary-read" => s.fail_secondary_result_once = true,
                    "select-after-change" => s.fail_page_select_once = true,
                    "select-mismatch" => s.ignore_page_select_once = true,
                    "restore-after-change" => s.fail_page_restore_once = true,
                    "restore-mismatch" => s.ignore_page_restore_once = true,
                    "snapshot-drift" => s.core_page_drift_on_secondary_once = true,
                    _ => unreachable!(),
                }
            }
            let result = initialize(model);
            let core = build_core_row(model);
            check(
                !result.succeeded
                    && model.core_voltage().is_none()
                    && !core.available
                    && core.write.is_none()
                    && core.restore_default.is_none()
                    && driver.page() == if failure == "restore-mismatch" { 0 } else { 1 },
                &format!("startup {failure}: {}", result.status),
            );
            let (reads, writes) = (driver.io_reads(), driver.io_writes());
            check(
                same(&result, &initialize(model)) && driver.io_reads() == reads && driver.io_writes() == writes,
                &format!("startup {failure}: Failure retried startup"),
            );
        });
    }
}

#[test]
fn startup_rejects_invalid_state_without_activating_a_wrong_stale_rail() {
    for invalid in ["unknown-page", "mode-sentinel", "low-target", "high-target", "invalid-secondary", "override-zero"] {
        with_model(|driver, model| {
            {
                let mut s = driver.st();
                match invalid {
                    "unknown-page" => s.page = 2,
                    "mode-sentinel" => s.mode = u16::MAX,
                    "low-target" => s.target = 599,
                    "high-target" => s.target = 1721,
                    "invalid-secondary" => s.secondary = u16::MAX,
                    "override-zero" => s.target = 0,
                    _ => unreachable!(),
                }
            }
            let result = initialize(model);
            let core = build_core_row(model);
            check(
                !result.succeeded
                    && model.core_voltage().is_none()
                    && !core.available
                    && core.write.is_none()
                    && core.default_value.is_none()
                    && driver.page() == if invalid == "unknown-page" { 2 } else { 1 },
                &format!("startup rejects {invalid}: {}", result.status),
            );
            if invalid == "unknown-page" {
                check(driver.regulator_writes().is_empty(), "Unknown original PAGE changed");
            }
        });
    }
}

#[test]
fn public_startup_gate_prevents_all_ec_access_and_records_one_refusal() {
    for gate in [
        "missing-driver",
        "closed-driver",
        "missing-Intel",
        "mailbox-unavailable",
        "voltage-restriction",
        "missing-EC",
        "unsupported-EC",
        "unconfirmed-MSI",
    ] {
        with_model_configured(
            |driver, parts| match gate {
                "missing-driver" => parts.driver = None,
                "closed-driver" => driver.st().driver_open = false,
                "missing-Intel" => parts.cpu = None,
                "mailbox-unavailable" => parts.mailbox_available = false,
                "voltage-restriction" => {
                    parts.voltage_access_restriction = Some("Voltage access is restricted by this boot.".into())
                }
                "missing-EC" => parts.sio = None,
                "unsupported-EC" => {
                    let drv: Arc<dyn KernelDriver> = driver.clone();
                    let wrong = SuperIo::reuse_verified_msi_ec_address(&drv, 0x4E, 0x0A20, "Nuvoton NCT6687D")
                        .unwrap()
                        .replace_isa_mutex_for_test(Some(NamedMutex::create(None).unwrap()))
                        .replace_kind_for_test(SuperIoKind::NuvotonBank);
                    parts.sio = Some(Arc::new(wrong));
                }
                "unconfirmed-MSI" => parts.smbios.board_manufacturer = "ASUS".into(),
                _ => unreachable!(),
            },
            |driver, model| {
                // Public fixed-factory gate returns before constructing a controller.
                let result = model.initialize_core_voltage();
                check(
                    !result.succeeded
                        && model.core_voltage().is_none()
                        && result.original_page.is_none()
                        && result.snapshot.is_none()
                        && !result.page_selection_attempted
                        && driver.io_reads() == 0
                        && driver.io_writes() == 0
                        && driver.mailbox_commands().is_empty(),
                    &format!("public startup {gate}: Capability gate bypassed: {}", result.status),
                );
                check(
                    same(&result, &model.initialize_core_voltage()) && driver.io_reads() == 0 && driver.io_writes() == 0,
                    &format!("public startup {gate}: Refusal silently retried"),
                );
            },
        );
    }
}

#[test]
fn verified_msi_system_identity_is_accepted_when_board_maker_text_is_absent() {
    with_model_configured(
        |_, parts| {
            parts.smbios.board_manufacturer = String::new();
            parts.smbios.system_manufacturer = "Micro-Star International".into();
        },
        |driver, model| {
            let result = initialize(model);
            check(result.succeeded && model.core_voltage().is_some() && driver.page() == 1, &result.status);
        },
    );
}
