//! MSI board-side CPU-core regulator startup and the regulator-plus-mailbox override writes.

use std::sync::Arc;

use anyhow::{Result, bail};

use super::{HardwareModel, msi_board};
use crate::hw::core_page_probe::CorePageVerificationResult;
use crate::hw::core_voltage_pair_transaction;
use crate::hw::ec_mailbox::EcMailbox;
use crate::hw::msi_core_voltage::MsiCoreVoltage;
use crate::hw::native::lock;
use crate::hw::oc_mailbox::OcMailbox;
use crate::hw::super_io::SuperIoKind;

/// Builds the startup controller from an EC mailbox. Production always uses
/// [`MsiCoreVoltage::try_create_for_startup`]; inert tests substitute an isolated fake.
pub(crate) type CoreVoltageStartupFactory<'a> =
    &'a dyn Fn(EcMailbox) -> (Option<MsiCoreVoltage>, CorePageVerificationResult);

impl HardwareModel {
    pub fn core_voltage_initialization_result(&self) -> Option<CorePageVerificationResult> {
        lock(&self.core_voltage_initialization).clone()
    }

    pub fn core_voltage_target_read_status(&self) -> String {
        match self.core_voltage() {
            Some(cv) => cv.target_read_status(),
            None => self.core_voltage_status(),
        }
    }

    pub fn core_voltage_target_read_failed(&self) -> bool {
        self.core_voltage().is_some_and(|cv| cv.target_read_failed())
    }

    /// One bounded startup attempt. Reuses the existing identified EC/driver; no discovery,
    /// driver lifecycle, Intel-mailbox command, voltage or mode write occurs here.
    /// A failed snapshot or PAGE restoration leaves the control disabled. Later calls return
    /// the same recorded result without accessing hardware; refresh never invokes this method.
    pub fn initialize_core_voltage(&self) -> CorePageVerificationResult {
        self.initialize_core_voltage_core(&MsiCoreVoltage::try_create_for_startup)
    }

    // The private seam permits inert tests to substitute an already-isolated fake controller.
    // Production always uses the fixed factory above; capability gates and once caching are shared.
    pub(crate) fn initialize_core_voltage_core(&self, startup_factory: CoreVoltageStartupFactory<'_>) -> CorePageVerificationResult {
        let mut recorded = lock(&self.core_voltage_initialization);
        if let Some(r) = recorded.as_ref() {
            return r.clone();
        }
        let restriction: Option<String> = if !self.driver().is_some_and(|d| d.is_open()) {
            Some("Hardware access is unavailable.".into())
        } else if self.cpu().is_none() {
            Some("An Intel CPU is required.".into())
        } else if let Some(r) = self.voltage_access_restriction() {
            Some(r)
        } else if !self.mailbox_available() {
            Some("The Intel OC mailbox is unavailable.".into())
        } else if !self.super_io().is_some_and(|s| s.kind() == SuperIoKind::NuvotonEc) {
            Some("A supported MSI embedded controller is unavailable.".into())
        } else if !msi_board(self.smbios()) {
            Some("The MSI board identity is not confirmed.".into())
        } else {
            None
        };

        let result = match restriction {
            Some(restriction) => CorePageVerificationResult {
                succeeded: false,
                original_page: None,
                restored_page: None,
                snapshot: None,
                status: restriction,
                page_selection_attempted: false,
                page_restoration_attempted: false,
                diagnostic_lines: Vec::new(),
            },
            None => {
                let (control, result) = startup_factory(EcMailbox::new(self.super_io().unwrap(), false));
                if let Some(c) = control {
                    let _ = self.core_voltage.set(Arc::new(c));
                }
                result
            }
        };

        *recorded = Some(result.clone());
        *lock(&self.core_voltage_status) = if result.succeeded {
            "Core voltage ready.".into()
        } else {
            format!("Core voltage unavailable: {}", result.status)
        };
        self.emit(&format!(
            "CPU core regulator startup: {}{}",
            result.status,
            if result.succeeded { " Editable target enabled." } else { " Control left disabled." }
        ));
        for line in &result.diagnostic_lines {
            self.emit(&format!("CPU core startup I/O: {line}"));
        }
        result
    }
}

pub(super) fn write_msi_core_override(regulator: &MsiCoreVoltage, mailbox: &OcMailbox, volts: f64) -> Result<()> {
    core_voltage_pair_transaction::execute(
        regulator,
        || mailbox.read_domain(OcMailbox::DOMAIN_CORE),
        || regulator.set_override(volts),
        || mailbox.set_override(OcMailbox::DOMAIN_CORE, volts),
        || {
            let after = mailbox.read_domain(OcMailbox::DOMAIN_CORE)?;
            if !after.override_mode || (after.target_volts - volts).abs() > 0.0011 {
                bail!("Intel mailbox did not retain {volts:.3} V (read back {:.3} V)", after.target_volts);
            }
            Ok(())
        },
        |before| mailbox.write_domain(OcMailbox::DOMAIN_CORE, &before),
    )
}

pub(super) fn clear_msi_core_override(regulator: &MsiCoreVoltage, mailbox: &OcMailbox) -> Result<()> {
    core_voltage_pair_transaction::execute(
        regulator,
        || mailbox.read_domain(OcMailbox::DOMAIN_CORE),
        || regulator.disable_override(),
        || mailbox.set_override(OcMailbox::DOMAIN_CORE, 0.0),
        || {
            if mailbox.read_domain(OcMailbox::DOMAIN_CORE)?.override_mode {
                bail!("Intel mailbox remained in override mode");
            }
            Ok(())
        },
        |before| mailbox.write_domain(OcMailbox::DOMAIN_CORE, &before),
    )
}
