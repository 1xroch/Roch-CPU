//! `--ui-preview`: fixed display fixtures for UI QA. No hardware objects, reads or writes are
//! used (the .NET `UiPreviewSettings` and `MainForm.CreateUiPreview`).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::model::{AmdInfo, CoreInfo, HeaderInfo, IntelInfo, LiveReadings, LogSink, UiModel};
use crate::model::memory_voltage_link::{MemoryVoltageGrid, MemoryVoltageLink};
use crate::model::setting::{Setting, SettingGroup};

/// The preview switches that follow `--ui-preview`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewOptions {
    pub dark: bool,
    pub scroll_bottom: bool,
    pub verified_core: bool,
    pub split_dimms: bool,
    pub core_read_failed: bool,
    /// `auto-mixed` or `error`.
    pub voltage_readings: Option<String>,
}

impl PreviewOptions {
    /// Parses the arguments after the executable name. A leading `--ui-preview` is skipped, as
    /// .NET read the switches from `args.Skip(1)`. `Err(2)` is the exit code for an unknown
    /// `--voltage-readings` value.
    pub fn parse(args: &[String]) -> std::result::Result<Self, i32> {
        let rest = match args.first() {
            Some(first) if first.eq_ignore_ascii_case("--ui-preview") => &args[1..],
            _ => args,
        };
        let has = |flag: &str| rest.iter().any(|a| a.eq_ignore_ascii_case(flag));
        const PREFIX: &str = "--voltage-readings=";
        let voltage_readings = rest
            .iter()
            .find(|a| a.len() >= PREFIX.len() && a[..PREFIX.len()].eq_ignore_ascii_case(PREFIX))
            .map(|a| a.split_once('=').map(|(_, v)| v).unwrap_or_default().to_lowercase());
        if !matches!(voltage_readings.as_deref(), None | Some("auto-mixed") | Some("error")) {
            return Err(2);
        }
        Ok(Self {
            dark: has("--dark"),
            scroll_bottom: has("--scroll-bottom"),
            verified_core: has("--verified-core"),
            split_dimms: has("--split-dimms"),
            core_read_failed: voltage_readings.as_deref() == Some("error"),
            voltage_readings,
        })
    }
}

/// The fixture rows (`UiPreviewSettings.Create`).
pub fn preview_settings(verified_core: bool, core_read_failed: bool) -> Vec<Setting> {
    use SettingGroup::*;
    vec![
        row("cpu_ratio", "CPU Ratio (P-Core)", Clocks, Some(54.0), "", 2, false, true),
        row("ring_ratio", "Ring Ratio", Clocks, Some(50.0), "", 1, false, true),
        row("bclk", "Base Clock", Clocks, Some(100.02), "MHz", 2, false, true),
        row(
            "core_v",
            "CPU Core Voltage",
            Voltages,
            if verified_core && !core_read_failed { Some(1.280) } else { None },
            "V",
            3,
            !verified_core,
            verified_core,
        ),
        row("core_off", "CPU Core Voltage Offset", Voltages, Some(0.0), "mV", 0, false, true),
        row("ecore_v", "CPU E-Core L2 Voltage", Voltages, None, "V", 3, false, true),
        row("ecore_off", "CPU E-Core L2 Voltage Offset", Voltages, Some(0.0), "mV", 0, false, true),
        row("ring_v", "Ring Voltage", Voltages, None, "V", 3, false, true),
        row("ring_off", "Ring Voltage Offset", Voltages, Some(0.0), "mV", 0, false, true),
        row("sa_v", "SA Voltage", Voltages, Some(1.189), "V", 3, false, true),
        row("sa_off", "SA Voltage Offset", Voltages, Some(0.0), "mV", 0, false, true),
        row("gt_v", "GT (iGPU) Voltage", Voltages, None, "V", 3, false, true),
        row("gt_off", "GT (iGPU) Voltage Offset", Voltages, Some(0.0), "mV", 0, false, true),
        row("pl1", "Package Power Limit 1 (PL1)", Power, Some(4096.0), "W", 0, false, true),
        row("pl2", "Package Power Limit 2 (PL2)", Power, Some(4096.0), "W", 0, false, true),
        row("cpu_vdd2", "CPU VDD2 Voltage", Board, Some(1.380), "V", 3, false, true),
        row("cpu_aux", "CPU AUX", Board, Some(1.796), "V", 3, true, true),
        row("dimma1_vdd", "DRAM DIMMA1 Voltage", Memory, Some(1.440), "V", 3, false, true),
        row("dimma1_vddq", "DRAM DIMMA1 VDDQ Voltage", Memory, Some(1.410), "V", 3, false, true),
        row("dimma1_vpp", "DRAM DIMMA1 VPP Voltage", Memory, Some(1.800), "V", 3, false, true),
        row("dimmb1_vdd", "DRAM DIMMB1 Voltage", Memory, Some(1.440), "V", 3, false, true),
        row("dimmb1_vddq", "DRAM DIMMB1 VDDQ Voltage", Memory, Some(1.410), "V", 3, false, true),
        row("dimmb1_vpp", "DRAM DIMMB1 VPP Voltage", Memory, Some(1.800), "V", 3, false, true),
    ]
}

#[allow(clippy::too_many_arguments)]
fn row(
    id: &str,
    name: &str,
    group: SettingGroup,
    current: Option<f64>,
    unit: &str,
    decimals: usize,
    read_only: bool,
    available: bool,
) -> Setting {
    let mut s = Setting::new(id, name, group);
    s.current = current;
    s.unit = unit.into();
    s.decimals = decimals;
    s.available = available;
    let memory = group == SettingGroup::Memory;
    s.min = if memory { if id.ends_with("_vpp") { 1.500 } else { 0.800 } } else { -500.0 };
    s.max = if memory {
        if id.ends_with("_vpp") {
            2.135
        } else if id.ends_with("_vddq") {
            1.500
        } else {
            1.800
        }
    } else {
        5000.0
    };
    s.note = Some("Fixed UI fixture; not a hardware reading or recommended setting.".into());
    s.read = Arc::new(|| bail!("UI preview does not read hardware."));
    if !read_only {
        s.write = Some(Arc::new(|_| bail!("UI preview does not write hardware.")));
    }
    s.restore_default = Some(Arc::new(|| bail!("UI preview does not restore hardware.")));
    s
}

/// The inert model behind `--ui-preview`. The window never calls its hardware methods in
/// preview mode; they answer without touching anything all the same.
pub struct PreviewModel {
    options: PreviewOptions,
    settings: Vec<Setting>,
}

impl PreviewModel {
    pub fn new(mut options: PreviewOptions) -> Self {
        options.core_read_failed |= options.voltage_readings.as_deref() == Some("error");
        let mut settings = preview_settings(options.verified_core, options.core_read_failed);
        // Inert target-state fixtures retained for UI QA; no sensor objects or callbacks are used.
        if options.voltage_readings.as_deref() == Some("auto-mixed") {
            for s in settings.iter_mut().filter(|s| s.id == "core_v" || s.group == SettingGroup::Memory) {
                s.current = None;
            }
        }
        Self { options, settings }
    }
}

impl UiModel for PreviewModel {
    fn subscribe_log(&self, _sink: LogSink) {}
    fn initialize(&self) {}
    fn settings(&self) -> Vec<Setting> {
        self.settings.clone()
    }
    fn apply(&self, _id: &str, _value: f64) -> bool {
        false
    }
    fn refresh(&self, _id: &str) {}
    fn refresh_all(&self) {}
    fn restore_all_defaults(&self) -> i32 {
        0
    }
    fn set_current(&self, _id: &str, _value: Option<f64>) {}
    fn read_live(&self) -> LiveReadings {
        LiveReadings::default()
    }
    fn measure_bclk(&self) -> Option<f64> {
        None
    }
    fn last_bclk(&self) -> Option<f64> {
        None
    }
    fn header(&self) -> HeaderInfo {
        HeaderInfo::default()
    }
    fn driver_status(&self) -> String {
        "UI preview".into()
    }
    fn driver_loaded(&self) -> bool {
        false
    }
    fn intel(&self) -> Option<IntelInfo> {
        None
    }
    fn amd(&self) -> Option<AmdInfo> {
        None
    }
    fn oc_locked(&self) -> bool {
        false
    }
    fn mailbox_available(&self) -> bool {
        false
    }
    fn voltage_access_restriction(&self) -> Option<String> {
        None
    }
    fn core_voltage_status(&self) -> String {
        "not probed".into()
    }
    fn core_voltage_ignored(&self) -> bool {
        false
    }
    fn core_voltage_target_read_failed(&self) -> bool {
        self.options.core_read_failed
    }
    fn smu_available(&self) -> bool {
        false
    }
    fn pbo_allowed(&self) -> bool {
        true
    }
    fn pawnio_installed(&self) -> bool {
        true
    }
    fn memory_grids(&self) -> HashMap<String, MemoryVoltageGrid> {
        let error = self.options.voltage_readings.as_deref() == Some("error");
        self.settings
            .iter()
            .filter_map(|s| {
                let rail = MemoryVoltageLink::try_get_rail(s)?;
                let step = if error && s.id == "dimmb1_vddq" {
                    0
                } else if rail == "VPP" {
                    5
                } else {
                    10
                };
                let base = if rail == "VPP" { 1500 } else { 800 };
                Some((s.id.clone(), MemoryVoltageGrid { base_millivolts: base, step_millivolts: step, max_code: 127 }))
            })
            .collect()
    }
    fn amd_cores(&self) -> Vec<CoreInfo> {
        Vec::new()
    }
    fn read_curve_optimizer(&self, _core: &CoreInfo) -> Option<i32> {
        None
    }
    fn apply_curve_optimizer(&self, _core: &CoreInfo, _margin: i32) -> Result<()> {
        bail!("UI preview does not write hardware.")
    }
    fn read_p_core_ratio_ceilings(&self) -> Result<Vec<i32>> {
        bail!("UI preview does not read hardware.")
    }
    fn write_p_core_ratio_ceilings(&self, _ratios: &[i32]) -> Result<()> {
        bail!("UI preview does not write hardware.")
    }
    fn read_e_cluster_ratios(&self) -> Result<Vec<(String, i32)>> {
        bail!("UI preview does not read hardware.")
    }
    fn write_e_cluster_ratios(&self, _ratios: &[i32]) -> Result<()> {
        bail!("UI preview does not write hardware.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_switches_case_insensitively() {
        let o = PreviewOptions::parse(&args(&["--ui-preview", "--DARK", "--scroll-bottom", "--Split-Dimms"])).unwrap();
        assert!(o.dark && o.scroll_bottom && o.split_dimms && !o.verified_core);
        assert_eq!(o.voltage_readings, None);
        assert!(!o.core_read_failed);
    }

    #[test]
    fn voltage_readings_values() {
        let o = PreviewOptions::parse(&args(&["--ui-preview", "--Voltage-Readings=ERROR"])).unwrap();
        assert_eq!(o.voltage_readings.as_deref(), Some("error"));
        assert!(o.core_read_failed);
        let o = PreviewOptions::parse(&args(&["--ui-preview", "--voltage-readings=auto-mixed"])).unwrap();
        assert_eq!(o.voltage_readings.as_deref(), Some("auto-mixed"));
        assert_eq!(PreviewOptions::parse(&args(&["--ui-preview", "--voltage-readings=bogus"])), Err(2));
        assert_eq!(PreviewOptions::parse(&args(&["--ui-preview", "--voltage-readings="])), Err(2));
    }

    #[test]
    fn the_first_argument_is_not_a_switch() {
        // .NET only looked at args.Skip(1); "--dark" in first place without --ui-preview is
        // still accepted here because run_preview may be handed the switches alone.
        let o = PreviewOptions::parse(&args(&["--dark"])).unwrap();
        assert!(o.dark);
    }

    #[test]
    fn fixture_rows_match_the_dotnet_fixture() {
        let rows = preview_settings(false, false);
        assert_eq!(rows.len(), 23);
        let core = rows.iter().find(|s| s.id == "core_v").unwrap();
        assert!(!core.available && core.read_only() && core.current.is_none());
        let aux = rows.iter().find(|s| s.id == "cpu_aux").unwrap();
        assert!(aux.read_only());
        let vpp = rows.iter().find(|s| s.id == "dimma1_vpp").unwrap();
        assert_eq!((vpp.min, vpp.max), (1.5, 2.135));
        let vddq = rows.iter().find(|s| s.id == "dimmb1_vddq").unwrap();
        assert_eq!((vddq.min, vddq.max), (0.8, 1.5));
        let pl1 = rows.iter().find(|s| s.id == "pl1").unwrap();
        assert_eq!((pl1.min, pl1.max), (-500.0, 5000.0));
        assert!((pl1.read)().is_err());
        assert!((pl1.write.as_ref().unwrap())(1.0).is_err());

        let verified = preview_settings(true, false);
        let core = verified.iter().find(|s| s.id == "core_v").unwrap();
        assert!(core.available && !core.read_only());
        assert_eq!(core.current, Some(1.280));
        let failed = preview_settings(true, true);
        assert_eq!(failed.iter().find(|s| s.id == "core_v").unwrap().current, None);
    }

    #[test]
    fn auto_mixed_clears_core_and_memory_targets() {
        let m = PreviewModel::new(PreviewOptions { voltage_readings: Some("auto-mixed".into()), ..Default::default() });
        for s in m.settings() {
            if s.id == "core_v" || s.group == SettingGroup::Memory {
                assert_eq!(s.current, None, "{}", s.id);
            }
        }
        assert_eq!(m.setting("SA_V").unwrap().current, Some(1.189));
    }

    #[test]
    fn grids_follow_the_fixture() {
        let m = PreviewModel::new(PreviewOptions::default());
        let g = m.memory_grids();
        assert_eq!(g.len(), 6);
        assert_eq!(g["dimma1_vpp"], MemoryVoltageGrid { base_millivolts: 1500, step_millivolts: 5, max_code: 127 });
        assert_eq!(g["dimmb1_vddq"], MemoryVoltageGrid { base_millivolts: 800, step_millivolts: 10, max_code: 127 });
        let e = PreviewModel::new(PreviewOptions { voltage_readings: Some("error".into()), ..Default::default() });
        assert_eq!(e.memory_grids()["dimmb1_vddq"].step_millivolts, 0);
        assert!(e.core_voltage_target_read_failed());
    }
}
