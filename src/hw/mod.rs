//! Hardware access layer: the kernel-driver clients and everything built on raw MSR, port, PCI,
//! SMBus and embedded-controller access.

use std::any::Any;

pub mod native;
pub mod startup_io_diagnostics;
#[cfg(windows)]
pub mod winring0;
#[cfg(windows)]
pub mod pawnio;
#[cfg(not(windows))]
#[path = "pawnio_stub.rs"]
pub mod pawnio;
#[cfg(target_os = "linux")]
pub mod linux;

pub mod amd_cpu;
pub mod amd_smu;
pub mod asus_audit;
pub mod asus_board_control;
pub mod bclk_controller;
#[cfg(target_os = "linux")]
pub mod board_vr;
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
#[cfg(target_os = "linux")]
pub mod smbus_kernel;
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

    /// One SMBus transfer through the operating system's own adapter driver (Linux `i2c-dev`),
    /// so the kernel's lock serialises it with the kernel's own transfers. Drivers without one
    /// refuse.
    #[cfg_attr(windows, allow(dead_code))]
    fn kernel_smbus(&self, _adapter: u32, _address: u8, _transfer: KernelSmbus) -> anyhow::Result<u16> {
        anyhow::bail!("{} has no kernel SMBus path", self.name())
    }

    /// A firmware table the operating system publishes as a file (`/sys/firmware/...`), read
    /// with this driver's privileges. Drivers without one refuse.
    #[cfg_attr(windows, allow(dead_code))]
    fn read_firmware_file(&self, _path: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("{} reads no firmware files", self.name())
    }

    /// Runs the core-cycle BCLK measurement where the hardware is, when this driver is a proxy
    /// for another process: the method needs its core busy for the whole window, and a thread
    /// blocked on a pipe halts it. `None`: measure here, in this process.
    fn measure_bclk_from_cycles(&self, _cpu: i32, _sample_ms: i32) -> Option<anyhow::Result<Option<f64>>> {
        None
    }

    /// Downcasting hook, so diagnostics can reach driver-specific calls (refcount, version).
    fn as_any(&self) -> &dyn Any;
}

/// One SMBus transfer handed to the kernel's adapter driver.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelSmbus {
    ReadByte { register: u8 },
    ReadWord { register: u8 },
    WriteByte { register: u8, value: u8 },
    WriteWord { register: u8, value: u16 },
}

/// Opens this platform's kernel driver: WinRing0 on Windows; on Linux the kernel's own
/// interfaces, directly when this process may use them, otherwise through the root helper.
pub fn open_driver() -> anyhow::Result<std::sync::Arc<dyn KernelDriver>> {
    #[cfg(windows)]
    {
        Ok(std::sync::Arc::new(winring0::WinRing0Driver::open()?))
    }
    #[cfg(target_os = "linux")]
    {
        linux::open()
    }
}

/// One line for the log about the opened driver.
pub fn driver_status(driver: &dyn KernelDriver) -> String {
    #[cfg(windows)]
    if let Some(w) = driver.as_any().downcast_ref::<winring0::WinRing0Driver>() {
        return format!("{} (driver v{:X})", w.name(), w.get_driver_version());
    }
    driver.name()
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
