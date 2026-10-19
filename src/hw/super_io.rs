//! Motherboard hardware monitor behind the LPC Super I/O at 0x2E / 0x4E.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateMutexExW, MUTEX_MODIFY_STATE, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use super::KernelDriver;
use super::native::{self, OwnedHandle};

// ---------------------------------------------------------------- named Win32 mutex

/// A Win32 mutex, named (shared with other monitoring tools, e.g. `Global\Access_EC`) or
/// unnamed. The .NET `System.Threading.Mutex` this replaces: ownership is per thread and
/// recursive, an abandoned mutex counts as acquired.
pub struct NamedMutex(OwnedHandle);

impl NamedMutex {
    /// `new Mutex(false, name)`: creates or opens the mutex without taking ownership. Opens with
    /// the rights .NET requests (MAXIMUM_ALLOWED | SYNCHRONIZE | MUTEX_MODIFY_STATE), so a mutex
    /// another tool created with a restrictive DACL can still be shared.
    pub fn create(name: Option<&str>) -> Result<NamedMutex> {
        const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
        let wide = name.map(native::wide);
        let ptr = wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
        // SAFETY: plain Win32 call; `ptr` is null or a NUL-terminated UTF-16 string that outlives it.
        let handle = unsafe {
            CreateMutexExW(std::ptr::null(), ptr, 0, MAXIMUM_ALLOWED | SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE)
        };
        if handle.is_null() {
            bail!("{}", native::last_error_message());
        }
        Ok(NamedMutex(OwnedHandle(handle)))
    }

    /// `WaitOne(ms)`, with `AbandonedMutexException` treated as acquired (as every caller does).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: the handle is a live mutex handle owned by self.
        let r = unsafe { WaitForSingleObject(self.0.0, timeout_ms) };
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }

    /// `ReleaseMutex()`; fails when the calling thread does not own the mutex.
    pub fn release(&self) -> Result<()> {
        // SAFETY: the handle is a live mutex handle owned by self.
        if unsafe { ReleaseMutex(self.0.0) } == 0 {
            bail!("Object synchronization method was called from an unsynchronized block of code. {}", native::last_error_message());
        }
        Ok(())
    }
}

/// C# `try { body } finally { fin }`: an exception from the finally block replaces the body's outcome.
pub(crate) fn with_finally<T>(body: Result<T>, fin: Result<()>) -> Result<T> {
    fin?;
    body
}

// ---------------------------------------------------------------- Super I/O

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuperIoKind {
    None,
    /// Nuvoton NCT6683/6686/6687 - EC address space. MSI 600/700-series.
    NuvotonEc,
    /// Nuvoton NCT679xD - banked register access. ASUS, and some ASRock/Gigabyte.
    NuvotonBank,
    /// ITE IT86xx/87xx. Gigabyte, and some ASRock.
    Ite,
}

/// One board voltage rail as the Super I/O reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct BoardRail {
    pub name: String,
    pub index: i32,
    pub volts: f64,
}

const NUVOTON_ENTER: u8 = 0x87;
const NUVOTON_EXIT: u8 = 0xAA;
const CHIP_ID_REGISTER: u8 = 0x20;
const CHIP_REVISION_REGISTER: u8 = 0x21;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const NUVOTON_HWM_LDN: u8 = 0x0B;
const ITE_HWM_LDN: u8 = 0x04;

// EC address space offsets from the monitor base (Nuvoton 668x)
const EC_PAGE: u16 = 0x04;
const EC_INDEX: u16 = 0x05;
const EC_DATA: u16 = 0x06;
const EC_PAGE_FREE: u8 = 0xFF;
// Banked / indexed access offsets (Nuvoton 679x, ITE)
const IDX: u16 = 0x05;
const DAT: u16 = 0x06;
const BANK_SELECT: u8 = 0x4E;

const EC_VOLTAGE_REGISTERS: [u16; 14] =
    [0x120, 0x122, 0x124, 0x126, 0x128, 0x12A, 0x12C, 0x12E, 0x130, 0x13A, 0x13E, 0x136, 0x138, 0x13C];

/// Motherboard hardware monitor behind the LPC Super I/O at 0x2E / 0x4E.
///
/// The CPU's own registers cannot see the voltage the board's regulators actually deliver, so
/// this is what lets Roch CPU check that a core-voltage change reached the rail instead of only
/// changing what the CPU requests. Read-only throughout.
///
/// Three families cover the LGA1700 boards in circulation: the Nuvoton EC-space parts MSI uses,
/// the banked Nuvoton parts ASUS favours, and the ITE parts common on Gigabyte and ASRock.
/// An unrecognised chip is not an error: the rail check is simply skipped.
pub struct SuperIo {
    drv: Arc<dyn KernelDriver>,
    #[allow(dead_code)]
    sio_port: u16,
    base: u16,
    lock: Mutex<()>,
    isa_mutex: Option<NamedMutex>,
    kind: SuperIoKind,
    name: String,
    named_rails: Vec<(&'static str, i32)>,
}

impl SuperIo {
    // MSI EC channel numbers
    pub const RAIL_VCORE: i32 = 2;
    pub const RAIL_VDD2: i32 = 4;
    pub const RAIL_SA: i32 = 5;
    pub const RAIL_AUX: i32 = 6;

    pub fn kind(&self) -> SuperIoKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn base_address(&self) -> u16 {
        self.base
    }

    /// Reuses an address/family already identified in the running tool's log. This performs no
    /// configuration-port detection or SMBIOS access. Caller must supply verified existing facts.
    pub fn reuse_verified_msi_ec_address(
        driver: &Arc<dyn KernelDriver>,
        configuration_port: u16,
        monitor_base: u16,
        chip_name: &str,
    ) -> Result<SuperIo> {
        if !driver.is_open() {
            bail!("An existing open driver handle is required.");
        }
        if !(configuration_port == 0x2E || configuration_port == 0x4E) || monitor_base < 0x100 || monitor_base > 0xFFF {
            bail!("Verified MSI EC addresses are required.");
        }
        if !matches!(chip_name, "Nuvoton NCT6683D" | "Nuvoton NCT6686D" | "Nuvoton NCT6687D") {
            bail!("The verified device must be an MSI Nuvoton EC family.");
        }
        Ok(SuperIo::new(driver.clone(), configuration_port, monitor_base, SuperIoKind::NuvotonEc, chip_name.to_string()))
    }

    /// Channel index carrying Vcore for this family.
    pub fn vcore_index(&self) -> i32 {
        match self.kind {
            SuperIoKind::NuvotonEc => 2,
            _ => 0,
        }
    }

    /// Rails whose board wiring is known. Only populated for the MSI EC parts, where the mapping
    /// was verified against the vendor tool; elsewhere the channel order is board specific and
    /// naming a rail would be a guess, so only Vcore is used.
    pub fn named_rails(&self) -> &[(&'static str, i32)] {
        &self.named_rails
    }

    fn new(drv: Arc<dyn KernelDriver>, sio_port: u16, base_addr: u16, kind: SuperIoKind, name: String) -> SuperIo {
        let named_rails = if kind == SuperIoKind::NuvotonEc {
            vec![("CPU VDD2 Voltage", Self::RAIL_VDD2), ("CPU AUX Voltage", Self::RAIL_AUX)]
        } else {
            Vec::new()
        };
        // MSI's Nuvoton EC path uses this lock in the vendor tool. The other indexed Super-I/O
        // families use the ISA-bus lock. Matching it prevents two tools from interleaving the
        // page/index/data sequence on the same three ports.
        let mutex_name = if kind == SuperIoKind::NuvotonEc { r"Global\Access_EC" } else { r"Global\Access_ISABUS.HTP.Method" };
        let isa_mutex = NamedMutex::create(Some(mutex_name)).ok();
        SuperIo { drv, sio_port, base: base_addr, lock: Mutex::new(()), isa_mutex, kind, name, named_rails }
    }

    /// Test seam for the C# tests' reflection: replaces the bus mutex handle before any use.
    #[cfg(test)]
    pub(crate) fn replace_isa_mutex_for_test(mut self, mutex: Option<NamedMutex>) -> SuperIo {
        self.isa_mutex = mutex;
        self
    }

    /// Test seam for the C# tests' reflection (`SetProperty(sio, "Kind", …)`): an object of
    /// another Super I/O family, used only to check that capability gates refuse it.
    #[cfg(test)]
    pub(crate) fn replace_kind_for_test(mut self, kind: SuperIoKind) -> SuperIo {
        self.kind = kind;
        self
    }

    // ---------------------------------------------------------------- detection
    /// Detects the Super I/O. Returns the chip (if any) and a status line.
    pub fn try_create(drv: &Arc<dyn KernelDriver>) -> Result<(Option<SuperIo>, String)> {
        // Configuration index/data ports are shared with other monitoring programs.
        let mutex = NamedMutex::create(Some(r"Global\Access_ISABUS.HTP.Method"))?;
        let held = mutex.wait(300);
        if !held {
            return Ok((None, "Super I/O detection is busy in another monitor.".to_string()));
        }
        let detected = Self::detect(drv);
        mutex.release()?;
        Ok(detected)
    }

    fn detect(drv: &Arc<dyn KernelDriver>) -> (Option<SuperIo>, String) {
        let mut problems: Vec<String> = Vec::new();
        for port in [0x2Eu16, 0x4E] {
            let attempt = (|| -> Result<Option<(SuperIo, String)>> {
                let (nuvoton, n_status) = Self::try_nuvoton(drv, port)?;
                if let Some(n) = nuvoton {
                    return Ok(Some((n, n_status)));
                }
                if !n_status.is_empty() {
                    problems.push(n_status);
                }
                let (ite, i_status) = Self::try_ite(drv, port)?;
                if let Some(i) = ite {
                    return Ok(Some((i, i_status)));
                }
                if !i_status.is_empty() {
                    problems.push(i_status);
                }
                Ok(None)
            })();
            match attempt {
                Ok(Some((sio, status))) => return (Some(sio), status),
                Ok(None) => {}
                Err(ex) => problems.push(format!("0x{port:02X}: {ex}")),
            }
        }
        let status = if !problems.is_empty() {
            "No supported Super I/O (".to_string()
                + &problems.join("; ")
                + "). Board rail readings and the Vcore check are unavailable."
        } else {
            "No supported Super I/O on the LPC bus. Board rail readings and the Vcore check are unavailable.".to_string()
        };
        (None, status)
    }

    fn try_nuvoton(drv: &Arc<dyn KernelDriver>, port: u16) -> Result<(Option<SuperIo>, String)> {
        let value = port + 1;
        drv.write_io_port_byte(port, NUVOTON_ENTER)?;
        drv.write_io_port_byte(port, NUVOTON_ENTER)?;
        let read = |reg: u8| -> Result<u8> {
            drv.write_io_port_byte(port, reg)?;
            drv.read_io_port_byte(value)
        };
        let write = |reg: u8, v: u8| -> Result<()> {
            drv.write_io_port_byte(port, reg)?;
            drv.write_io_port_byte(value, v)
        };

        let id = read(CHIP_ID_REGISTER)?;
        let rev = read(CHIP_REVISION_REGISTER)?;
        let chip = ((id as u16) << 8) | rev as u16;
        let (kind, name) = Self::identify_nuvoton(chip);
        if kind == SuperIoKind::None {
            let status = format!("LPC 0x{port:02X} returned unrecognized Nuvoton ID 0x{chip:04X}");
            drv.write_io_port_byte(port, NUVOTON_EXIT)?;
            return Ok((None, status));
        }

        write(DEVICE_SELECT_REGISTER, NUVOTON_HWM_LDN)?;
        let hi = read(BASE_ADDRESS_REGISTER)?;
        let lo = read(BASE_ADDRESS_REGISTER + 1)?;
        let addr = ((hi as u16) << 8) | lo as u16;
        native::sleep_ms(1);
        let hi = read(BASE_ADDRESS_REGISTER)?;
        let lo = read(BASE_ADDRESS_REGISTER + 1)?;
        let verify = ((hi as u16) << 8) | lo as u16;
        drv.write_io_port_byte(port, NUVOTON_EXIT)?;

        if addr != verify || addr < 0x100 || addr == 0xFFFF {
            return Ok((
                None,
                format!("{name} at LPC 0x{port:02X} but its monitor base did not read back stable (0x{addr:04X}/0x{verify:04X})"),
            ));
        }
        let detected = SuperIo::new(drv.clone(), port, addr, kind, name.to_string());
        if kind == SuperIoKind::NuvotonBank
            && !detected.with_lock(|| Ok(detected.read_byte_bank(0x804F)? == 0x5C && detected.read_byte_bank(0x004F)? == 0xA3), false)
        {
            drop(detected);
            return Ok((
                None,
                format!("{name} identified at LPC 0x{port:02X}, but monitor vendor ID could not be verified (I/O access may be locked)"),
            ));
        }
        Ok((Some(detected), format!("{name} at LPC 0x{port:02X}, hardware monitor at 0x{addr:04X}")))
    }

    fn identify_nuvoton(chip: u16) -> (SuperIoKind, &'static str) {
        match chip {
            0xC732 => (SuperIoKind::NuvotonEc, "Nuvoton NCT6683D"),
            0xD440 => (SuperIoKind::NuvotonEc, "Nuvoton NCT6686D"),
            0xD592 => (SuperIoKind::NuvotonEc, "Nuvoton NCT6687D"),
            0xC803 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6791D"),
            0xC911 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6792D"),
            0xD121 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6793D"),
            0xD352 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6795D"),
            0xD423 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6796D"),
            0xD42B => (SuperIoKind::NuvotonBank, "Nuvoton NCT6798D"),
            0xD451 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6797D"),
            0xD802 => (SuperIoKind::NuvotonBank, "Nuvoton NCT6799D"),
            _ => (SuperIoKind::None, ""),
        }
    }

    fn try_ite(drv: &Arc<dyn KernelDriver>, port: u16) -> Result<(Option<SuperIo>, String)> {
        let value = port + 1;
        // ITE unlock: 0x87, 0x01, 0x55, then 0x55 on 0x2E or 0xAA on 0x4E.
        drv.write_io_port_byte(port, 0x87)?;
        drv.write_io_port_byte(port, 0x01)?;
        drv.write_io_port_byte(port, 0x55)?;
        drv.write_io_port_byte(port, if port == 0x2E { 0x55 } else { 0xAA })?;
        let read = |reg: u8| -> Result<u8> {
            drv.write_io_port_byte(port, reg)?;
            drv.read_io_port_byte(value)
        };
        let write = |reg: u8, v: u8| -> Result<()> {
            drv.write_io_port_byte(port, reg)?;
            drv.write_io_port_byte(value, v)
        };
        let exit = || write(0x02, 0x02);

        let id = read(CHIP_ID_REGISTER)?;
        let rev = read(CHIP_REVISION_REGISTER)?;
        let chip = ((id as u16) << 8) | rev as u16;
        if (chip & 0xFF00) != 0x8600 && (chip & 0xFF00) != 0x8700 {
            exit()?;
            return Ok((None, String::new()));
        }

        write(DEVICE_SELECT_REGISTER, ITE_HWM_LDN)?;
        let hi = read(BASE_ADDRESS_REGISTER)?;
        let lo = read(BASE_ADDRESS_REGISTER + 1)?;
        let addr = ((hi as u16) << 8) | lo as u16;
        native::sleep_ms(1);
        let hi = read(BASE_ADDRESS_REGISTER)?;
        let lo = read(BASE_ADDRESS_REGISTER + 1)?;
        let verify = ((hi as u16) << 8) | lo as u16;
        exit()?;

        if addr != verify || addr < 0x100 || addr == 0xFFFF {
            return Ok((None, format!("ITE IT{chip:04X} at LPC 0x{port:02X} but its monitor base did not read back stable")));
        }
        let result = SuperIo::new(drv.clone(), port, addr, SuperIoKind::Ite, format!("ITE IT{chip:04X}"));
        Ok((Some(result), format!("ITE IT{chip:04X} at LPC 0x{port:02X}, hardware monitor at 0x{addr:04X}")))
    }

    // ---------------------------------------------------------------- register access
    fn read_byte_ec(&self, address: u16) -> Result<u8> {
        let page = (address >> 8) as u8;
        let index = (address & 0xFF) as u8;
        let sw = Instant::now();
        while self.drv.read_io_port_byte(self.base + EC_PAGE)? != EC_PAGE_FREE && sw.elapsed().as_millis() < 200 {
            native::sleep_ms(1);
        }
        self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE)?;
        self.drv.write_io_port_byte(self.base + EC_PAGE, page)?;
        self.drv.write_io_port_byte(self.base + EC_INDEX, index)?;
        let result = self.drv.read_io_port_byte(self.base + EC_DATA)?;
        self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE)?;
        Ok(result)
    }

    fn read_byte_bank(&self, address: u16) -> Result<u8> {
        let bank = (address >> 8) as u8;
        let reg = (address & 0xFF) as u8;
        self.drv.write_io_port_byte(self.base + IDX, BANK_SELECT)?;
        self.drv.write_io_port_byte(self.base + DAT, bank)?;
        self.drv.write_io_port_byte(self.base + IDX, reg)?;
        self.drv.read_io_port_byte(self.base + DAT)
    }

    fn read_byte_ite(&self, address: u16) -> Result<u8> {
        self.drv.write_io_port_byte(self.base + IDX, (address & 0xFF) as u8)?;
        self.drv.read_io_port_byte(self.base + DAT)
    }

    fn read_byte_raw(&self, address: u16) -> Result<u8> {
        match self.kind {
            SuperIoKind::NuvotonEc => self.read_byte_ec(address),
            SuperIoKind::NuvotonBank => self.read_byte_bank(address),
            SuperIoKind::Ite => self.read_byte_ite(address),
            SuperIoKind::None => Ok(0),
        }
    }

    fn with_lock<T>(&self, body: impl FnOnce() -> Result<T>, fallback: T) -> T {
        let _guard = native::lock(&self.lock);
        let held = self.isa_mutex.as_ref().map_or(true, |m| m.wait(300));
        if !held {
            return fallback;
        }
        let result = body();
        if let Some(m) = &self.isa_mutex {
            let _ = m.release();
        }
        result.unwrap_or(fallback)
    }

    // ---------------------------------------------------------------- voltages
    /// MSI's channel wiring on the EC parts, verified against HWiNFO and the vendor tool:
    /// VIN0 x12, VIN1 x5, VIN4 (VDD2) and VIN6 (AUX) x2, everything else direct.
    fn ec_divider(index: i32) -> f64 {
        match index {
            0 => 12.0,
            1 => 5.0,
            4 => 2.0,
            6 => 2.0,
            _ => 1.0,
        }
    }

    pub fn channel_count(&self) -> i32 {
        match self.kind {
            SuperIoKind::NuvotonEc => EC_VOLTAGE_REGISTERS.len() as i32,
            SuperIoKind::NuvotonBank => 16,
            SuperIoKind::Ite => 9,
            SuperIoKind::None => 0,
        }
    }

    pub fn read_voltage(&self, index: i32) -> Option<f64> {
        if index < 0 || index >= self.channel_count() {
            return None;
        }
        self.with_lock(
            || {
                Ok(match self.kind {
                    // 16-bit reading at reg / reg+1, 1 mV per LSB before the input divider.
                    SuperIoKind::NuvotonEc => {
                        let reg = EC_VOLTAGE_REGISTERS[index as usize];
                        let high = self.read_byte_raw(reg)? as i32;
                        let low = self.read_byte_raw(reg + 1)? as i32;
                        Some(0.001 * ((16 * high) + (low >> 4)) as f64 * Self::ec_divider(index))
                    }
                    // Banked Nuvoton: 8 mV per LSB, Vcore on VIN0.
                    SuperIoKind::NuvotonBank => Some(0.008 * self.read_byte_raw((0x480 + index) as u16)? as f64),
                    // ITE: 12 mV per LSB, Vcore on VIN0.
                    SuperIoKind::Ite => Some(0.012 * self.read_byte_raw((0x20 + index) as u16)? as f64),
                    SuperIoKind::None => None,
                })
            },
            None,
        )
    }

    pub fn read_vcore(&self) -> Option<f64> {
        self.read_voltage(self.vcore_index())
    }

    /// Single raw EC-space read, for protocol diagnostics. Rate-limit the caller, not this.
    pub fn read_raw(&self, address: u16) -> u8 {
        self.with_lock(|| self.read_byte_raw(address), 0u8)
    }

    /// Strict MSI EC access for a reviewed PAGE probe: failures never become zero data.
    pub fn read_raw_strict(&self, address: u16) -> Result<u8> {
        self.with_strict_ec_lock(|| {
            self.wait_for_free_ec_page_strict()?;
            let body = (|| -> Result<u8> {
                self.drv.write_io_port_byte(self.base + EC_PAGE, (address >> 8) as u8)?;
                self.drv.write_io_port_byte(self.base + EC_INDEX, address as u8)?;
                self.drv.read_io_port_byte(self.base + EC_DATA)
            })();
            with_finally(body, self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE))
        })
    }

    pub fn write_raw_strict(&self, address: u16, value: u8) -> Result<bool> {
        self.with_strict_ec_lock(|| {
            self.wait_for_free_ec_page_strict()?;
            let body = (|| -> Result<bool> {
                self.drv.write_io_port_byte(self.base + EC_PAGE, (address >> 8) as u8)?;
                self.drv.write_io_port_byte(self.base + EC_INDEX, address as u8)?;
                self.drv.write_io_port_byte(self.base + EC_DATA, value)?;
                Ok(true)
            })();
            with_finally(body, self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE))
        })
    }

    fn wait_for_free_ec_page_strict(&self) -> Result<()> {
        let timer = Instant::now();
        while self.drv.read_io_port_byte(self.base + EC_PAGE)? != EC_PAGE_FREE {
            if timer.elapsed().as_millis() >= 200 {
                bail!("MSI EC page/index interface remained busy; access refused.");
            }
            native::sleep_ms(1);
        }
        Ok(())
    }

    fn with_strict_ec_lock<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let mutex = match (&self.isa_mutex, self.kind) {
            (Some(m), SuperIoKind::NuvotonEc) => m,
            _ => bail!("Strict access requires the verified MSI EC and its bus mutex."),
        };
        let _guard = native::lock(&self.lock);
        let held = mutex.wait(300);
        if !held {
            return Err(anyhow!("MSI EC bus mutex timed out; no port access attempted."));
        }
        let result = work();
        with_finally(result, mutex.release())
    }

    /// Single raw EC-space write. Only the vendor-command path uses this.
    pub fn write_raw(&self, address: u16, value: u8) -> bool {
        self.with_lock(
            || {
                if self.kind != SuperIoKind::NuvotonEc {
                    return Ok(false);
                }
                let page = (address >> 8) as u8;
                let index = (address & 0xFF) as u8;
                let sw = Instant::now();
                while self.drv.read_io_port_byte(self.base + EC_PAGE)? != EC_PAGE_FREE && sw.elapsed().as_millis() < 200 {
                    native::sleep_ms(1);
                }
                self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE)?;
                self.drv.write_io_port_byte(self.base + EC_PAGE, page)?;
                self.drv.write_io_port_byte(self.base + EC_INDEX, index)?;
                self.drv.write_io_port_byte(self.base + EC_DATA, value)?;
                self.drv.write_io_port_byte(self.base + EC_PAGE, EC_PAGE_FREE)?;
                Ok(true)
            },
            false,
        )
    }

    /// The rails whose wiring is known on this board; empty on families where it is not.
    pub fn read_rails(&self) -> Vec<BoardRail> {
        let mut list = Vec::new();
        for &(name, index) in &self.named_rails {
            if let Some(v) = self.read_voltage(index) {
                if v > 0.05 {
                    list.push(BoardRail { name: name.to_string(), index, volts: v });
                }
            }
        }
        list
    }

    /// Every channel, for diagnostics.
    pub fn read_all(&self) -> Vec<(i32, f64)> {
        let mut list = Vec::new();
        for i in 0..self.channel_count() {
            if let Some(v) = self.read_voltage(i) {
                list.push((i, v));
            }
        }
        list
    }
}
