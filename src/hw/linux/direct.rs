//! The privileged primitives on Linux, and what they refuse.
//!
//! This runs as root - in the helper, or in a process started with sudo - so the rules about
//! what may be touched are enforced here rather than by convention, and a request is refused the
//! same way whichever side of the pipe it came from:
//!
//! * **MSRs** go through `/dev/cpu/N/msr` (the `msr` module, loaded here when missing). Any MSR
//!   may be read; only the ones Roch CPU tunes may be written: see [`msr_write_is_allowed`].
//! * **I/O ports** go through `in`/`out` after `iopl(3)`. The legacy ports below 0x200 are refused
//!   except the Super I/O and EC index pairs, and 0xCF8-0xCFF always are: 0xCF9 is the reset
//!   control register, one byte there restarts the machine, and the kernel uses CF8/CFC itself
//!   under a lock user space cannot take.
//! * **PCI configuration space** goes through sysfs, never CF8/CFC, for the same reason. A
//!   function powered down to D3cold reads as absent rather than being woken. Writes reach only
//!   the AMD SMN index/data pair on the host bridge.
//! * **SMBus** transfers go through `/dev/i2c-N`, so the kernel adapter's lock serialises them
//!   with the kernel's own (the `spd5118` driver reads the same DIMMs). Only the DDR5 SPD hubs and
//!   PMICs can be addressed, and only a PMIC can be written.
//! * Only **firmware tables** can be read as files.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};

use super::super::native::{self, lock};
use super::super::{KernelDriver, KernelSmbus};

/// The MSRs Roch CPU writes, and nothing else: the microcode-signature reset, the OC mailbox,
/// the P-core turbo table, the package power limits, the ring limit, the E-core turbo table, and
/// the fixed-counter controls the BCLK measurement borrows and puts back.
pub const MSR_WRITE_ALLOWLIST: [u32; 8] = [0x8B, 0x150, 0x1AD, 0x610, 0x620, 0x650, 0x38D, 0x38F];

/// Files that may be read for a client, as prefixes. Firmware tables only.
pub const FIRMWARE_FILE_PREFIXES: [&str; 2] = ["/sys/firmware/acpi/tables/", "/sys/firmware/dmi/tables/"];

/// AMD's SMN index and data registers on the host bridge (00:00.0).
pub const SMN_INDEX: u16 = 0x60;
pub const SMN_DATA: u16 = 0x64;

// linux/i2c-dev.h and linux/i2c.h.
const I2C_SLAVE_FORCE: libc::c_ulong = 0x0706;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_WRITE: u8 = 0;
const I2C_SMBUS_BYTE_DATA: u32 = 2;
const I2C_SMBUS_WORD_DATA: u32 = 3;

/// `union i2c_smbus_data`: a byte, a word, or a 34-byte block.
#[repr(C)]
union I2cSmbusData {
    byte: u8,
    word: u16,
    block: [u8; 34],
}

/// `struct i2c_smbus_ioctl_data`.
#[repr(C)]
struct I2cSmbusIoctlData {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut I2cSmbusData,
}

pub fn msr_write_is_allowed(index: u32) -> bool {
    MSR_WRITE_ALLOWLIST.contains(&index)
}

/// Whether a port may be touched at all. Nothing below 0x200 is needed but the Super I/O index
/// pairs and the EC; down there are the keyboard controller's reset line at 0x64 and fast reset
/// at 0x92, and 0xCF9 is the reset control register.
pub fn port_is_allowed(port: u16, width: u8) -> bool {
    let last = port.saturating_add(width.saturating_sub(1) as u16);
    if port <= 0xCFF && last >= 0xCF8 {
        return false;
    }
    if port >= 0x200 {
        return true;
    }
    width == 1 && matches!(port, 0x2E | 0x2F | 0x4E | 0x4F | 0x62 | 0x66)
}

/// The DDR5 SPD hubs (0x50-0x57) and PMICs (0x48-0x4F), plus the DIMM thermal sensors
/// (0x18-0x1F) the diagnostics list, may be read. Only a PMIC may be written: its voltage and ADC
/// registers are what the memory rows change, and nothing here writes an SPD hub, whose EEPROM
/// holds the module's identity.
pub fn smbus_is_allowed(address: u8, transfer: KernelSmbus) -> bool {
    match transfer {
        KernelSmbus::ReadByte { .. } | KernelSmbus::ReadWord { .. } => {
            matches!(address, 0x18..=0x1F | 0x48..=0x57)
        }
        KernelSmbus::WriteByte { .. } => matches!(address, 0x48..=0x4F),
        KernelSmbus::WriteWord { .. } => false,
    }
}

/// The board voltage regulators on the PCH's serial I2C bus (Renesas PMBus controllers at 0x60
/// and 0x62), on boards where they have been mapped. Anything may be read. The only writes are
/// the PAGE selector (pages 0-2) and VOUT_COMMAND within 0.5-1.5 V: never the mode register,
/// so a rail the CPU drives over SVID stays the CPU's, and never the regulator's protection
/// limits or NVM.
pub fn vr_is_allowed(board: &str, adapter_name: &str, address: u8, transfer: KernelSmbus) -> bool {
    if !super::super::board_vr::board_is_mapped(board)
        || !adapter_name.starts_with("Synopsys DesignWare I2C adapter")
        || !matches!(address, 0x60 | 0x62)
    {
        return false;
    }
    match transfer {
        KernelSmbus::ReadByte { .. } | KernelSmbus::ReadWord { .. } => true,
        KernelSmbus::WriteByte { register, value } => register == 0x00 && value <= 2,
        KernelSmbus::WriteWord { register, value } => register == 0x21 && (500..=1500).contains(&value),
    }
}

pub fn pci_write_is_allowed(bus: u8, device: u8, function: u8, offset: u16) -> bool {
    (bus, device, function) == (0, 0, 0) && (offset == SMN_INDEX || offset == SMN_DATA)
}

pub fn firmware_file_is_allowed(path: &str) -> bool {
    !path.split('/').any(|part| part == "..") && FIRMWARE_FILE_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

fn modprobe(module: &str) {
    let _ = std::process::Command::new("modprobe")
        .args(["-q", module])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

thread_local! {
    /// `iopl` is per thread on Linux, and only threads created after the call inherit it. The
    /// hardware code runs on worker threads that may predate the driver, so each thread raises
    /// its own the first time it touches a port.
    static IOPL_RAISED: Cell<bool> = const { Cell::new(false) };
}

fn ensure_iopl() -> Result<()> {
    if IOPL_RAISED.with(Cell::get) {
        return Ok(());
    }
    // SAFETY: iopl only changes this thread's I/O privilege; root has CAP_SYS_RAWIO.
    if unsafe { libc::iopl(3) } != 0 {
        bail!("iopl(3) was refused: {}", native::last_error_message());
    }
    IOPL_RAISED.with(|c| c.set(true));
    Ok(())
}

/// # Safety
/// The caller has raised IOPL on this thread and checked the port against the allowlist.
unsafe fn port_in(port: u16, width: u8) -> u32 {
    use std::arch::asm;
    unsafe {
        match width {
            1 => {
                let v: u8;
                asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v as u32
            }
            2 => {
                let v: u16;
                asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v as u32
            }
            _ => {
                let v: u32;
                asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v
            }
        }
    }
}

/// # Safety
/// As [`port_in`].
unsafe fn port_out(port: u16, width: u8, value: u32) {
    use std::arch::asm;
    unsafe {
        match width {
            1 => asm!("out dx, al", in("dx") port, in("al") value as u8, options(nomem, nostack, preserves_flags)),
            2 => asm!("out dx, ax", in("dx") port, in("ax") value as u16, options(nomem, nostack, preserves_flags)),
            _ => asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)),
        }
    }
}

/// Register access through the kernel's own interfaces, in this process.
pub struct Direct {
    msr: Mutex<HashMap<u32, File>>,
    i2c: Mutex<HashMap<u32, File>>,
    /// Port sequences are short; one lock keeps two threads from interleaving single accesses.
    io: Mutex<()>,
    last_error: Mutex<Option<String>>,
}

impl Direct {
    /// Succeeds when this process can use the MSR and port interfaces: it is root, kernel
    /// lockdown is off and the `msr` module is (or can be) loaded.
    pub fn open() -> Result<Direct> {
        if !native::is_root() {
            bail!("Register access needs root.");
        }
        if let Ok(v) = std::fs::read_to_string("/sys/kernel/security/lockdown")
            && !v.contains("[none]")
        {
            bail!(
                "Kernel lockdown is active ({}), which refuses MSR writes and port access to every program. \
                 It is usually tied to Secure Boot.",
                v.trim()
            );
        }
        if !Path::new("/dev/cpu/0/msr").exists() {
            modprobe("msr");
        }
        if !Path::new("/dev/cpu/0/msr").exists() {
            bail!("/dev/cpu/0/msr is missing and the msr module could not be loaded (modprobe msr).");
        }
        // /dev/i2c-N only exists with i2c-dev loaded, which most distributions leave to whoever
        // needs it. It lasts until the next boot and changes nothing on disk.
        if !Path::new("/sys/class/i2c-dev").exists() {
            modprobe("i2c-dev");
        }
        ensure_iopl()?;
        let d = Direct { msr: Mutex::new(HashMap::new()), i2c: Mutex::new(HashMap::new()), io: Mutex::new(()), last_error: Mutex::new(None) };
        // Prove the MSR path works before claiming the driver is open.
        if d.read_msr(0x10, 0).is_none() {
            bail!("Reading MSRs through /dev/cpu/0/msr failed: {}", d.last_error().unwrap_or_default());
        }
        Ok(d)
    }

    fn fail<T>(&self, message: String) -> Option<T> {
        *lock(&self.last_error) = Some(message);
        None
    }

    /// One file per CPU, opened read-write once; MSR writes need the write half.
    fn with_msr_file<T>(&self, cpu: u32, f: impl FnOnce(&File) -> std::io::Result<T>) -> std::io::Result<T> {
        let mut files = lock(&self.msr);
        let file = match files.entry(cpu) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(OpenOptions::new().read(true).write(true).open(format!("/dev/cpu/{cpu}/msr"))?)
            }
        };
        f(file)
    }

    fn resolve_cpu(cpu: i32) -> u32 {
        if cpu < 0 { native::current_cpu() as u32 } else { cpu as u32 }
    }

    pub fn read_msr_on(&self, index: u32, cpu: u32) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.with_msr_file(cpu, |f| f.read_exact_at(&mut buf, index as u64))
            .map_err(|e| anyhow!("RDMSR 0x{index:X} on CPU {cpu}: {e}"))?;
        Ok(u64::from_le_bytes(buf))
    }

    pub fn write_msr_on(&self, index: u32, value: u64, cpu: u32) -> Result<()> {
        if !msr_write_is_allowed(index) {
            bail!("MSR 0x{index:X} is not one Roch CPU writes; refused.");
        }
        self.with_msr_file(cpu, |f| f.write_all_at(&value.to_le_bytes(), index as u64))
            .map_err(|e| anyhow!("WRMSR 0x{index:X} on CPU {cpu}: {e}"))
    }

    pub fn port_in(&self, port: u16, width: u8) -> Result<u32> {
        if !matches!(width, 1 | 2 | 4) || !port_is_allowed(port, width) {
            bail!("Port 0x{port:X} is not one Roch CPU uses; refused.");
        }
        ensure_iopl()?;
        let _g = lock(&self.io);
        // SAFETY: IOPL raised on this thread, port checked above.
        Ok(unsafe { port_in(port, width) })
    }

    pub fn port_out(&self, port: u16, width: u8, value: u32) -> Result<()> {
        if !matches!(width, 1 | 2 | 4) || !port_is_allowed(port, width) {
            bail!("Port 0x{port:X} is not one Roch CPU uses; refused.");
        }
        ensure_iopl()?;
        let _g = lock(&self.io);
        // SAFETY: as above.
        unsafe { port_out(port, width, value) };
        Ok(())
    }

    fn pci_path(bus: u8, device: u8, function: u8, leaf: &str) -> String {
        format!("/sys/bus/pci/devices/0000:{bus:02x}:{device:02x}.{function}/{leaf}")
    }

    /// One configuration dword. `Ok(None)` when the function does not exist or is asleep in
    /// D3cold, where a read would wake it.
    pub fn read_pci(&self, bus: u8, device: u8, function: u8, offset: u16) -> Result<Option<u32>> {
        if device > 31 || function > 7 || offset > 0xFFC {
            bail!("PCI address out of range");
        }
        let power = std::fs::read_to_string(Self::pci_path(bus, device, function, "power_state")).unwrap_or_default();
        if power.trim() == "D3cold" {
            return Ok(None);
        }
        let file = match File::open(Self::pci_path(bus, device, function, "config")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => bail!("PCI {bus:02x}:{device:02x}.{function}: {e}"),
        };
        let mut buf = [0u8; 4];
        match file.read_exact_at(&mut buf, (offset & !3) as u64) {
            Ok(()) => Ok(Some(u32::from_le_bytes(buf))),
            // Past the end of the space the kernel exposes (256 bytes for legacy functions).
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(Some(0xFFFF_FFFF)),
            Err(e) => bail!("PCI {bus:02x}:{device:02x}.{function} +0x{offset:X}: {e}"),
        }
    }

    pub fn write_pci(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> Result<()> {
        if !pci_write_is_allowed(bus, device, function, offset) {
            bail!("PCI configuration write to {bus:02x}:{device:02x}.{function} +0x{offset:X} refused.");
        }
        let file = OpenOptions::new().write(true).open(Self::pci_path(bus, device, function, "config"))?;
        file.write_all_at(&value.to_le_bytes(), (offset & !3) as u64)?;
        Ok(())
    }

    pub fn firmware_file(&self, path: &str) -> Result<Vec<u8>> {
        if !firmware_file_is_allowed(path) {
            bail!("{path} is not a firmware table; refused.");
        }
        Ok(std::fs::read(path)?)
    }

    pub fn smbus(&self, adapter: u32, address: u8, transfer: KernelSmbus) -> Result<u16> {
        let adapter_name = std::fs::read_to_string(format!("/sys/bus/i2c/devices/i2c-{adapter}/name")).unwrap_or_default();
        let board = std::fs::read_to_string("/sys/class/dmi/id/board_name").unwrap_or_default();
        let pch_smbus = adapter_name.starts_with("SMBus ");
        if !(pch_smbus && smbus_is_allowed(address, transfer) || vr_is_allowed(board.trim(), adapter_name.trim(), address, transfer)) {
            bail!("SMBus {transfer:?} at 0x{address:02X} on i2c-{adapter} is outside the allowlist; refused.");
        }
        let mut files = lock(&self.i2c);
        let fd = match files.entry(adapter) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{adapter}"))?)
            }
        }
        .as_raw_fd();
        // SAFETY: an fd we own; FORCE because the spd5118 driver may own the hub addresses, and
        // every transfer still goes through the adapter's lock.
        if unsafe { libc::ioctl(fd, I2C_SLAVE_FORCE, address as libc::c_ulong) } != 0 {
            bail!("I2C_SLAVE_FORCE 0x{address:02X}: {}", native::last_error_message());
        }
        let mut data = I2cSmbusData { block: [0; 34] };
        let (read_write, command, size) = match transfer {
            KernelSmbus::ReadByte { register } => (I2C_SMBUS_READ, register, I2C_SMBUS_BYTE_DATA),
            KernelSmbus::ReadWord { register } => (I2C_SMBUS_READ, register, I2C_SMBUS_WORD_DATA),
            KernelSmbus::WriteByte { register, value } => {
                data.byte = value;
                (I2C_SMBUS_WRITE, register, I2C_SMBUS_BYTE_DATA)
            }
            KernelSmbus::WriteWord { register, value } => {
                data.word = value;
                (I2C_SMBUS_WRITE, register, I2C_SMBUS_WORD_DATA)
            }
        };
        let mut args = I2cSmbusIoctlData { read_write, command, size, data: &mut data };
        // SAFETY: args and data outlive the call and have the kernel's layout.
        if unsafe { libc::ioctl(fd, I2C_SMBUS, &mut args) } != 0 {
            bail!("SMBus transfer at 0x{address:02X}: {}", native::last_error_message());
        }
        // SAFETY: the kernel filled the union for the size asked for.
        Ok(unsafe {
            match transfer {
                KernelSmbus::ReadByte { .. } => data.byte as u16,
                KernelSmbus::ReadWord { .. } => data.word,
                KernelSmbus::WriteByte { .. } | KernelSmbus::WriteWord { .. } => 0,
            }
        })
    }
}

impl KernelDriver for Direct {
    fn name(&self) -> String {
        "Linux kernel interfaces (/dev/cpu/*/msr, iopl, sysfs, i2c-dev)".into()
    }
    fn is_open(&self) -> bool {
        true
    }
    fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }
    fn read_msr(&self, index: u32, cpu: i32) -> Option<u64> {
        match self.read_msr_on(index, Self::resolve_cpu(cpu)) {
            Ok(v) => Some(v),
            Err(e) => self.fail(e.to_string()),
        }
    }
    fn write_msr(&self, index: u32, value: u64, cpu: i32) -> bool {
        match self.write_msr_on(index, value, Self::resolve_cpu(cpu)) {
            Ok(()) => true,
            Err(e) => {
                self.fail::<()>(e.to_string());
                false
            }
        }
    }
    fn read_io_port_byte(&self, port: u16) -> Result<u8> {
        Ok(self.port_in(port, 1)? as u8)
    }
    fn read_io_port_word(&self, port: u16) -> Result<u16> {
        Ok(self.port_in(port, 2)? as u16)
    }
    fn read_io_port_dword(&self, port: u16) -> Result<u32> {
        self.port_in(port, 4)
    }
    fn write_io_port_byte(&self, port: u16, value: u8) -> Result<()> {
        self.port_out(port, 1, value as u32)
    }
    fn write_io_port_word(&self, port: u16, value: u16) -> Result<()> {
        self.port_out(port, 2, value as u32)
    }
    fn write_io_port_dword(&self, port: u16, value: u32) -> Result<()> {
        self.port_out(port, 4, value)
    }
    fn read_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        self.read_pci_config_raw(bus, device, function, offset).filter(|&v| v != 0xFFFF_FFFF)
    }
    fn read_pci_config_raw(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        match self.read_pci(bus, device, function, offset) {
            Ok(v) => v,
            Err(e) => self.fail(e.to_string()),
        }
    }
    fn write_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> bool {
        match self.write_pci(bus, device, function, offset, value) {
            Ok(()) => true,
            Err(e) => {
                self.fail::<()>(e.to_string());
                false
            }
        }
    }
    fn kernel_smbus(&self, adapter: u32, address: u8, transfer: KernelSmbus) -> Result<u16> {
        self.smbus(adapter, address, transfer)
    }
    fn read_firmware_file(&self, path: &str) -> Result<Vec<u8>> {
        self.firmware_file(path)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_ports_and_the_pci_selector_are_never_reachable() {
        for port in [0x64u16, 0x92, 0xCF8, 0xCF9, 0xCFC, 0x70, 0x80] {
            assert!(!port_is_allowed(port, 1), "port 0x{port:X}");
        }
        // A dword at 0xCF6 would reach 0xCF9.
        assert!(!port_is_allowed(0xCF6, 4));
        for port in [0x2Eu16, 0x2F, 0x4E, 0x4F, 0x62, 0x66, 0xA45, 0x1808, 0xEFA0] {
            assert!(port_is_allowed(port, 1), "port 0x{port:X}");
        }
        assert!(port_is_allowed(0x1808, 4));
        assert!(!port_is_allowed(0x2E, 2));
    }

    #[test]
    fn only_tuning_msrs_are_writable() {
        for msr in [0x150u32, 0x1AD, 0x610, 0x620, 0x650] {
            assert!(msr_write_is_allowed(msr));
        }
        for msr in [0xC000_0082u32, 0x1A0, 0x10, 0x3A, 0x1D9, 0x199] {
            assert!(!msr_write_is_allowed(msr), "0x{msr:X}");
        }
    }

    #[test]
    fn spd_hubs_are_read_only_and_pmics_writable() {
        assert!(smbus_is_allowed(0x50, KernelSmbus::ReadByte { register: 0 }));
        assert!(!smbus_is_allowed(0x50, KernelSmbus::WriteByte { register: 0x0B, value: 0 }));
        assert!(smbus_is_allowed(0x48, KernelSmbus::WriteByte { register: 0x21, value: 0x78 }));
        assert!(!smbus_is_allowed(0x30, KernelSmbus::ReadByte { register: 0 }));
    }

    #[test]
    fn board_regulators_take_only_page_and_target_writes() {
        let (b, a) = ("Z890 AORUS TACHYON ICE", "Synopsys DesignWare I2C adapter");
        assert!(vr_is_allowed(b, a, 0x62, KernelSmbus::ReadWord { register: 0xF0 }));
        assert!(vr_is_allowed(b, a, 0x62, KernelSmbus::WriteByte { register: 0x00, value: 2 }));
        assert!(vr_is_allowed(b, a, 0x62, KernelSmbus::WriteWord { register: 0x21, value: 1320 }));
        // The mode register, a page past the mapped ones, out-of-range targets, other boards.
        assert!(!vr_is_allowed(b, a, 0x62, KernelSmbus::WriteWord { register: 0xF0, value: 0x2232 }));
        assert!(!vr_is_allowed(b, a, 0x62, KernelSmbus::WriteByte { register: 0x00, value: 3 }));
        assert!(!vr_is_allowed(b, a, 0x62, KernelSmbus::WriteWord { register: 0x21, value: 1600 }));
        assert!(!vr_is_allowed(b, a, 0x62, KernelSmbus::WriteWord { register: 0x21, value: 400 }));
        assert!(!vr_is_allowed("Z790 MPOWER", a, 0x62, KernelSmbus::ReadByte { register: 0x98 }));
        assert!(!vr_is_allowed(b, "SMBus I801 adapter at 0000:80:1f.4", 0x62, KernelSmbus::ReadByte { register: 0x98 }));
        assert!(!vr_is_allowed(b, a, 0x50, KernelSmbus::ReadByte { register: 0 }));
    }

    #[test]
    fn only_firmware_tables_are_files_and_no_escapes() {
        assert!(firmware_file_is_allowed("/sys/firmware/acpi/tables/FACP"));
        assert!(firmware_file_is_allowed("/sys/firmware/dmi/tables/DMI"));
        assert!(!firmware_file_is_allowed("/etc/shadow"));
        assert!(!firmware_file_is_allowed("/sys/firmware/acpi/tables/../../../etc/shadow"));
    }

    #[test]
    fn pci_writes_reach_only_the_smn_pair() {
        assert!(pci_write_is_allowed(0, 0, 0, 0x60));
        assert!(pci_write_is_allowed(0, 0, 0, 0x64));
        assert!(!pci_write_is_allowed(0, 0, 0, 0x04));
        assert!(!pci_write_is_allowed(0x80, 31, 4, 0x40));
    }
}
