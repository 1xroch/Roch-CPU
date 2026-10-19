//! What the window needs from the model, as a trait over plain data, so the UI never names a
//! `crate::hw` type and can run against the inert UI-preview fixture as well as the hardware.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::model::memory_voltage_link::MemoryVoltageGrid;
use crate::model::setting::Setting;

/// Receives every log line the model emits (the .NET `HardwareModel.Log` event).
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Live readings shown in the header (`HardwareModel.ReadLive`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveReadings {
    pub package_temp_c: Option<i32>,
    pub core_mhz: Option<f64>,
    pub core_ratio: Option<i32>,
    pub e_core_ratio: Option<i32>,
    pub core_vid: Option<f64>,
    pub ring_ratio: Option<i32>,
    pub bclk_mhz: Option<f64>,
    pub package_watts: Option<f64>,
    /// Real Vcore at the VRM output, when a Super I/O can report it.
    pub vcore_vrm: Option<f64>,
}

/// Hardware-read identity shown above the tuning controls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeaderInfo {
    /// Vendor-neutral CPU brand string (`HardwareModel.CpuName`).
    pub cpu_name: String,
    /// "6P + 8E / 20", "8 / 16" (`HardwareModel.CoreSummary`).
    pub core_summary: String,
    /// Intel microcode revision or AMD patch level; 0 when unknown.
    pub microcode_revision: u32,
    /// SMBIOS board product, or the system product when the board product is blank.
    pub board: String,
    /// SMBIOS AGESA version; empty on Intel.
    pub agesa: String,
    pub bios_version: String,
}

/// The Intel facts the window uses (present only on the Intel path).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntelInfo {
    pub hypervisor_present: bool,
    pub is_lga1700_family: bool,
    pub e_core_count: i32,
}

/// The AMD facts the window uses (present only on the AMD path).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmdInfo {
    /// A known Zen generation (SMU message numbers are not assumed).
    pub is_supported: bool,
    pub has_curve_optimizer: bool,
    pub has_curve_optimizer_readback: bool,
    /// Curve Optimizer counts either side of zero.
    pub co_range: i32,
}

/// One physical AMD core as the SMU addresses it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreInfo {
    pub index: i32,
    pub ccd: i32,
    pub core_in_ccd: i32,
    /// "Core 3"
    pub label: String,
    /// "CCD 0 core 3"
    pub location: String,
}

/// An Intel turbo ratio table: the ratio for each group and how many cores the group covers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurboTable {
    pub ratios: Vec<i32>,
    pub cores: Vec<i32>,
}

/// Everything the main window, the log, the per-core ratio window and the Curve Optimizer
/// window ask of the model. Calls that touch hardware block; the window runs the slow ones
/// (apply, restore, base-clock measurement, initialization) on worker threads.
pub trait UiModel: Send + Sync {
    /// Subscribes to the model's log.
    fn subscribe_log(&self, sink: LogSink);
    /// Opens the driver and probes the hardware. Never fails; called once, before anything else.
    fn initialize(&self);

    // ---- rows
    /// A snapshot of every row, in display order.
    fn settings(&self) -> Vec<Setting>;
    /// One row by id (case-insensitive).
    fn setting(&self, id: &str) -> Option<Setting> {
        self.settings().into_iter().find(|s| s.id.eq_ignore_ascii_case(id))
    }
    /// Applies one value by row id. Zero restores the startup default.
    fn apply(&self, id: &str, value: f64) -> bool;
    /// Re-reads one row.
    fn refresh(&self, id: &str);
    /// Re-reads every row (without capturing new defaults).
    fn refresh_all(&self);
    /// Puts every changed writable row back; returns the number that failed.
    fn restore_all_defaults(&self) -> i32;
    /// Overwrites a row's displayed reading without touching hardware (the window records each
    /// finished base-clock measurement into the "bclk" row this way, as .NET set `Current`).
    fn set_current(&self, id: &str, value: Option<f64>);

    // ---- live
    fn read_live(&self) -> LiveReadings;
    /// Blocking base-clock measurement; call from a worker thread.
    fn measure_bclk(&self) -> Option<f64>;
    fn last_bclk(&self) -> Option<f64>;

    // ---- header and status
    fn header(&self) -> HeaderInfo;
    fn driver_status(&self) -> String;
    /// The kernel driver opened.
    fn driver_loaded(&self) -> bool;
    /// `Some` on the Intel path.
    fn intel(&self) -> Option<IntelInfo>;
    /// `Some` on the AMD path.
    fn amd(&self) -> Option<AmdInfo>;
    fn is_amd(&self) -> bool {
        self.amd().is_some()
    }
    fn oc_locked(&self) -> bool;
    fn mailbox_available(&self) -> bool;
    fn voltage_access_restriction(&self) -> Option<String>;
    fn core_voltage_status(&self) -> String;
    /// Set when a core-voltage apply was measured not to reach the rail.
    fn core_voltage_ignored(&self) -> bool;
    fn core_voltage_target_read_failed(&self) -> bool;
    fn smu_available(&self) -> bool;
    fn pbo_allowed(&self) -> bool;
    fn pawnio_installed(&self) -> bool;

    // ---- DIMM linking
    /// The verified PMIC register grid of every DIMM rail, keyed by row id ("dimma1_vdd").
    /// These are cached results of the existing capability detection, not new reads.
    fn memory_grids(&self) -> HashMap<String, MemoryVoltageGrid>;

    // ---- AMD Curve Optimizer
    fn amd_cores(&self) -> Vec<CoreInfo>;
    fn read_curve_optimizer(&self, core: &CoreInfo) -> Option<i32>;
    fn apply_curve_optimizer(&self, core: &CoreInfo, margin: i32) -> Result<()>;

    // ---- Intel turbo ratio tables
    fn read_p_core_turbo_table(&self) -> Result<TurboTable>;
    fn read_e_core_turbo_table(&self) -> Result<TurboTable>;
    fn write_p_core_turbo_table(&self, ratios: &[i32]) -> Result<()>;
    fn write_e_core_turbo_table(&self, ratios: &[i32]) -> Result<()>;
}
