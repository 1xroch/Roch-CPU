//! [`UiModel`] for the real [`HardwareModel`]. This is the only UI file that names model or
//! hardware types beyond `Setting`; everything else talks to the trait.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::model::{AmdInfo, CoreInfo, HeaderInfo, IntelInfo, LiveReadings, LogSink, TurboTable, UiModel};
use crate::hw::pawnio::PawnIo;
use crate::model::hardware_model::HardwareModel;
use crate::model::memory_voltage_link::MemoryVoltageGrid;
use crate::model::setting::Setting;

impl HardwareModel {
    fn amd_core(&self, core: &CoreInfo) -> Option<crate::hw::amd_cpu::AmdCore> {
        let amd = self.amd()?;
        amd.cores().iter().find(|c| c.index as i32 == core.index).cloned()
    }
}

impl UiModel for HardwareModel {
    fn subscribe_log(&self, sink: LogSink) {
        self.on_log(move |m: &str| sink(m));
    }

    fn initialize(&self) {
        HardwareModel::initialize(self);
    }

    fn settings(&self) -> Vec<Setting> {
        HardwareModel::settings(self).clone()
    }

    fn setting(&self, id: &str) -> Option<Setting> {
        HardwareModel::setting(self, id)
    }

    fn apply(&self, id: &str, value: f64) -> bool {
        HardwareModel::apply(self, id, value)
    }

    fn refresh(&self, id: &str) {
        HardwareModel::refresh(self, id);
    }

    fn refresh_all(&self) {
        HardwareModel::refresh_all(self, false);
    }

    fn restore_all_defaults(&self) -> i32 {
        HardwareModel::restore_all_defaults(self)
    }

    fn set_current(&self, id: &str, _value: Option<f64>) {
        // The window only records finished base-clock measurements this way (.NET assigned
        // `bclk.Current = LastBclk`). The "bclk" row's read returns that cached measurement, so
        // re-reading the row stores it without touching hardware.
        if id.eq_ignore_ascii_case("bclk") {
            HardwareModel::refresh(self, id);
        }
    }

    fn read_live(&self) -> LiveReadings {
        let l = HardwareModel::read_live(self);
        LiveReadings {
            package_temp_c: l.package_temp_c,
            core_mhz: l.core_mhz,
            core_ratio: l.core_ratio,
            e_core_ratio: l.e_core_ratio,
            core_vid: l.core_vid,
            ring_ratio: l.ring_ratio,
            bclk_mhz: l.bclk_mhz,
            package_watts: l.package_watts,
            vcore_vrm: l.vcore_vrm,
        }
    }

    fn measure_bclk(&self) -> Option<f64> {
        HardwareModel::measure_bclk(self)
    }

    fn last_bclk(&self) -> Option<f64> {
        HardwareModel::last_bclk(self)
    }

    fn header(&self) -> HeaderInfo {
        let sm = self.smbios();
        HeaderInfo {
            cpu_name: self.cpu_name(),
            core_summary: self.core_summary(),
            microcode_revision: self.microcode_revision(),
            board: if sm.board_product.trim().is_empty() { sm.system_product.clone() } else { sm.board_product.clone() },
            agesa: sm.agesa.clone(),
            bios_version: sm.bios_version.clone(),
        }
    }

    fn driver_status(&self) -> String {
        HardwareModel::driver_status(self)
    }

    fn driver_loaded(&self) -> bool {
        self.driver().is_some()
    }

    fn intel(&self) -> Option<IntelInfo> {
        self.cpu().map(|c| IntelInfo {
            hypervisor_present: c.hypervisor_present(),
            is_lga1700_family: c.is_lga1700_family(),
            e_core_count: c.e_core_count() as i32,
        })
    }

    fn amd(&self) -> Option<AmdInfo> {
        HardwareModel::amd(self).map(|a| {
            let smu = a.smu();
            let m = smu.messages();
            AmdInfo {
                is_supported: a.is_supported(),
                has_curve_optimizer: m.has_curve_optimizer(),
                has_curve_optimizer_readback: m.has_curve_optimizer_readback(),
                co_range: m.co_range,
            }
        })
    }

    fn is_amd(&self) -> bool {
        HardwareModel::is_amd(self)
    }

    fn oc_locked(&self) -> bool {
        HardwareModel::oc_locked(self)
    }

    fn mailbox_available(&self) -> bool {
        HardwareModel::mailbox_available(self)
    }

    fn voltage_access_restriction(&self) -> Option<String> {
        HardwareModel::voltage_access_restriction(self)
    }

    fn core_voltage_status(&self) -> String {
        HardwareModel::core_voltage_status(self)
    }

    fn core_voltage_ignored(&self) -> bool {
        HardwareModel::core_voltage_ignored(self)
    }

    fn core_voltage_target_read_failed(&self) -> bool {
        HardwareModel::core_voltage_target_read_failed(self)
    }

    fn smu_available(&self) -> bool {
        HardwareModel::smu_available(self)
    }

    fn pbo_allowed(&self) -> bool {
        HardwareModel::pbo_allowed(self)
    }

    fn pawnio_installed(&self) -> bool {
        PawnIo::is_installed()
    }

    fn memory_grids(&self) -> HashMap<String, MemoryVoltageGrid> {
        // These are the cached results of existing capability detection, not new reads.
        let mut grids = HashMap::new();
        for dimm in self.dimms() {
            let id = dimm.slot_name().to_lowercase();
            let grid = |base: i32, step: i32| MemoryVoltageGrid { base_millivolts: base, step_millivolts: step };
            grids.insert(format!("{id}_vdd"), grid(800, dimm.vdd_step_mv() as i32));
            grids.insert(format!("{id}_vddq"), grid(800, dimm.vddq_step_mv() as i32));
            grids.insert(format!("{id}_vpp"), grid(1500, if dimm.vpp_verified() { 5 } else { 0 }));
        }
        grids
    }

    fn amd_cores(&self) -> Vec<CoreInfo> {
        HardwareModel::amd(self)
            .map(|a| {
                a.cores()
                    .iter()
                    .map(|c| CoreInfo {
                        index: c.index as i32,
                        ccd: c.ccd as i32,
                        core_in_ccd: c.core_in_ccd as i32,
                        label: c.label().to_string(),
                        location: c.location().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read_curve_optimizer(&self, core: &CoreInfo) -> Option<i32> {
        let c = self.amd_core(core)?;
        HardwareModel::read_curve_optimizer(self, &c)
    }

    fn apply_curve_optimizer(&self, core: &CoreInfo, margin: i32) -> Result<()> {
        let Some(c) = self.amd_core(core) else { bail!("Curve Optimizer needs an AMD CPU.") };
        HardwareModel::apply_curve_optimizer(self, &c, margin)
    }

    fn read_p_core_turbo_table(&self) -> Result<TurboTable> {
        let Some(cpu) = self.cpu() else { bail!("The per-core ratio table needs an Intel CPU.") };
        let (ratios, cores) = cpu.read_p_core_turbo_table()?;
        Ok(TurboTable { ratios: ratios.to_vec(), cores: cores.to_vec() })
    }

    fn read_e_core_turbo_table(&self) -> Result<TurboTable> {
        let Some(cpu) = self.cpu() else { bail!("The per-core ratio table needs an Intel CPU.") };
        let (ratios, cores) = cpu.read_e_core_turbo_table()?;
        Ok(TurboTable { ratios: ratios.to_vec(), cores: cores.to_vec() })
    }

    fn write_p_core_turbo_table(&self, ratios: &[i32]) -> Result<()> {
        let Some(cpu) = self.cpu() else { bail!("The per-core ratio table needs an Intel CPU.") };
        cpu.write_p_core_turbo_table(ratios)
    }

    fn write_e_core_turbo_table(&self, ratios: &[i32]) -> Result<()> {
        let Some(cpu) = self.cpu() else { bail!("The per-core ratio table needs an Intel CPU.") };
        cpu.write_e_core_turbo_table(ratios)
    }
}
