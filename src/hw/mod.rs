//! Hardware access layer: the kernel-driver clients and everything built on raw MSR, port, PCI,
//! SMBus and embedded-controller access.

use std::any::Any;

pub mod native;
pub mod startup_io_diagnostics;
pub mod winring0;
pub mod pawnio;

pub mod amd_cpu;
pub mod amd_smu;
pub mod asus_audit;
pub mod asus_board_control;
pub mod bclk_controller;
pub mod bclk_meter;
pub mod core_page_probe;
pub mod core_voltage_pair_transaction;
pub mod ddr5_dimm;
pub mod ec_clock_gen;
pub mod ec_mailbox;
pub mod hypervisor_restrictions;
pub mod intel_audit;
pub mod intel_cpu;
pub mod msi_core_voltage;
pub mod oc_mailbox;
pub mod pmic_voltage_measurement;
pub mod smbios;
pub mod smbus_i801;
pub mod smbus_piix4;
pub mod super_io;
pub mod vdd2_rail;

/// Minimal ring-0 access surface needed by an overclocking tool. Everything the
/// application does with hardware goes through this so a different driver
/// (PawnIO, a vendor driver, a simulator for UI work) can be dropped in later.
///
/// Every method takes `&self`: a driver is shared (`Arc<dyn KernelDriver>`) by every hardware
/// object, and implementations serialise their own I/O.
pub trait KernelDriver: Send + Sync {
    fn name(&self) -> String;
    fn is_open(&self) -> bool;
    /// Detail for the last failed driver operation, when available.
    fn last_error(&self) -> Option<String> {
        None
    }

    /// Reads an MSR on the logical CPU `cpu` (-1 = whichever CPU we are on). `None` on failure.
    fn read_msr(&self, index: u32, cpu: i32) -> Option<u64>;
    fn write_msr(&self, index: u32, value: u64, cpu: i32) -> bool;

    fn read_io_port_byte(&self, port: u16) -> anyhow::Result<u8>;
    fn read_io_port_word(&self, port: u16) -> anyhow::Result<u16>;
    fn read_io_port_dword(&self, port: u16) -> anyhow::Result<u32>;
    fn write_io_port_byte(&self, port: u16, value: u8) -> anyhow::Result<()>;
    fn write_io_port_word(&self, port: u16, value: u16) -> anyhow::Result<()>;
    fn write_io_port_dword(&self, port: u16, value: u32) -> anyhow::Result<()>;

    /// PCI configuration space read (legacy 256-byte space). `None` if the device is absent.
    fn read_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32>;
    /// PCI configuration read that only reports whether the driver call itself succeeded, so an
    /// all-ones value comes back as `Some(0xFFFF_FFFF)` (a legal SMN data value on AMD).
    fn read_pci_config_raw(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        self.read_pci_config(bus, device, function, offset)
    }
    fn write_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> bool;

    /// Downcasting hook, so diagnostics can reach driver-specific calls (refcount, version).
    fn as_any(&self) -> &dyn Any;
}

/// SMBus host controller: byte and word transfers to a 7-bit address. Intel's PCH controller
/// and AMD's FCH controller have different registers but the same job, and the DDR5 SPD hub
/// and PMIC code only needs this much.
pub trait Smbus: Send + Sync {
    fn description(&self) -> String;
    fn read_byte(&self, address: u8, command: u8) -> Option<u8>;
    fn write_byte(&self, address: u8, command: u8, value: u8) -> bool;
    fn read_word(&self, address: u8, command: u8) -> Option<u16>;
    /// Downcasting hook (the Intel PCH controller has recovery calls the AMD one does not).
    fn as_any(&self) -> &dyn Any;
}
