//! The SMBus on Linux: the kernel's own adapter (`i2c_i801` on Intel, `i2c_piix4` on AMD), through
//! `/dev/i2c-N`. The kernel driver owns the controller's registers, so driving them directly as
//! the Windows build does would collide with its transfers; going through the adapter puts every
//! transfer under the kernel's lock instead.

use std::any::Any;
use std::sync::Arc;

use super::linux;
use super::{KernelDriver, KernelSmbus, Smbus};

pub struct SmbusKernel {
    drv: Arc<dyn KernelDriver>,
    adapter: u32,
    name: String,
}

impl SmbusKernel {
    /// The adapter names the kernel gives the PCH and FCH SMBus controllers. On AMD the DIMMs sit
    /// on port 0 of the FCH's mux.
    const INTEL_PREFIX: &'static str = "SMBus I801 adapter";
    const AMD_PREFIX: &'static str = "SMBus PIIX4 adapter port 0";

    pub fn try_create(drv: Arc<dyn KernelDriver>, amd: bool) -> (Option<SmbusKernel>, String) {
        let prefix = if amd { Self::AMD_PREFIX } else { Self::INTEL_PREFIX };
        let Some(adapter) = linux::find_i2c_adapter(prefix) else {
            let driver = if amd { "i2c_piix4" } else { "i2c_i801" };
            return (None, format!("No kernel SMBus adapter named \"{prefix}...\" (is the {driver} module loaded?)."));
        };
        let name = linux::i2c_adapter_name(adapter).unwrap_or_else(|| prefix.to_string());
        let status = format!("kernel adapter i2c-{adapter}: {name}");
        (Some(SmbusKernel { drv, adapter, name }), status)
    }

    fn transfer(&self, address: u8, transfer: KernelSmbus) -> Option<u16> {
        self.drv.kernel_smbus(self.adapter, address, transfer).ok()
    }
}

impl Smbus for SmbusKernel {
    fn description(&self) -> String {
        format!("i2c-{} ({})", self.adapter, self.name)
    }
    fn read_byte(&self, address: u8, command: u8) -> Option<u8> {
        self.transfer(address, KernelSmbus::ReadByte { register: command }).map(|v| v as u8)
    }
    fn write_byte(&self, address: u8, command: u8, value: u8) -> bool {
        self.transfer(address, KernelSmbus::WriteByte { register: command, value }).is_some()
    }
    fn read_word(&self, address: u8, command: u8) -> Option<u16> {
        self.transfer(address, KernelSmbus::ReadWord { register: command })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
