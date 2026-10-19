//! Client for the OpenLibSys WinRing0 1.2 kernel driver (WinRing0x64.sys).
//! The driver is installed as a demand-start service the first time it is
//! needed and removed again when this object is dropped, unless it was
//! already running (another monitoring tool may own it).

use std::any::Any;
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_ERROR_NORMAL, SERVICE_KERNEL_DRIVER, SERVICE_STATUS, StartServiceW,
};

use super::native::{self, OwnedHandle, lock, wide};
use super::{KernelDriver, startup_io_diagnostics as diag};

const DEVICE_NAME: &str = r"\\.\WinRing0_1_2_0";
const SERVICE_NAME: &str = "RochCpuRing0";
const DRIVER_FILE: &str = "WinRing0x64.sys";

const OLS_TYPE: u32 = 40000;
const fn ctl_code(function: u32, access: u32) -> u32 {
    (OLS_TYPE << 16) | (access << 14) | (function << 2)
}
const FILE_ANY_ACCESS: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

const IOCTL_GET_DRIVER_VERSION: u32 = ctl_code(0x800, FILE_ANY_ACCESS);
const IOCTL_GET_REFCOUNT: u32 = ctl_code(0x801, FILE_ANY_ACCESS);
const IOCTL_READ_MSR: u32 = ctl_code(0x821, FILE_ANY_ACCESS);
const IOCTL_WRITE_MSR: u32 = ctl_code(0x822, FILE_ANY_ACCESS);
const IOCTL_READ_IO_PORT_BYTE: u32 = ctl_code(0x833, FILE_READ_ACCESS);
const IOCTL_READ_IO_PORT_WORD: u32 = ctl_code(0x834, FILE_READ_ACCESS);
const IOCTL_READ_IO_PORT_DWORD: u32 = ctl_code(0x835, FILE_READ_ACCESS);
const IOCTL_WRITE_IO_PORT_BYTE: u32 = ctl_code(0x836, FILE_WRITE_ACCESS);
const IOCTL_WRITE_IO_PORT_WORD: u32 = ctl_code(0x837, FILE_WRITE_ACCESS);
const IOCTL_WRITE_IO_PORT_DWORD: u32 = ctl_code(0x838, FILE_WRITE_ACCESS);
const IOCTL_READ_PCI_CONFIG: u32 = ctl_code(0x851, FILE_READ_ACCESS);
const IOCTL_WRITE_PCI_CONFIG: u32 = ctl_code(0x852, FILE_WRITE_ACCESS);

struct State {
    handle: Option<OwnedHandle>,
    last_error: Option<String>,
}

pub struct WinRing0Driver {
    /// Also the I/O lock: one DeviceIoControl at a time, like the .NET client's `_ioLock`.
    state: Mutex<State>,
    installed_by_us: Mutex<bool>,
}

struct Scm(SC_HANDLE);
impl Drop for Scm {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn open_scm() -> Option<Scm> {
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if h.is_null() { None } else { Some(Scm(h)) }
}

fn open_service(scm: &Scm) -> Option<Scm> {
    let name = wide(SERVICE_NAME);
    let h = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if h.is_null() { None } else { Some(Scm(h)) }
}

impl WinRing0Driver {
    fn new() -> Self {
        Self {
            state: Mutex::new(State { handle: None, last_error: None }),
            installed_by_us: Mutex::new(false),
        }
    }

    /// Opens only the already-running device. Never installs, starts, adopts, stops, or deletes a
    /// driver service. Dropping this handle cannot change the service lifecycle.
    pub fn open_existing() -> Result<Self> {
        let d = Self::new();
        if d.try_open_device() {
            return Ok(d);
        }
        bail!(
            "The existing WinRing0 device could not be opened. No driver service action was attempted. {}",
            d.last_error().unwrap_or_default()
        )
    }

    /// Opens the driver, installing it from the application directory if needed.
    pub fn open() -> Result<Self> {
        let d = Self::new();
        if d.try_open_device() {
            // A previous instance that was killed (task manager, crash) leaves our service behind.
            // Adopt it so it is removed again when this instance exits - but only if nothing else
            // is using it, or a CLI probe run alongside the window would unload the driver under it.
            *lock(&d.installed_by_us) = our_service_exists() && native::is_only_instance();
            return Ok(d);
        }

        let path = native::app_dir().join(DRIVER_FILE);
        if !path.exists() {
            bail!(
                "{DRIVER_FILE} was not found next to the application. It is required for MSR / port / PCI access. ({})",
                path.display()
            );
        }

        d.install_and_start(&path.to_string_lossy())?;
        if !d.try_open_device() {
            bail!(
                "The WinRing0 driver was installed but the device {DEVICE_NAME} could not be opened. \
                 Another driver may be blocking it, or Windows (Core Isolation / Memory Integrity, or Defender) refused to load it. \
                 Last error: {}",
                d.last_error().unwrap_or_default()
            );
        }
        Ok(d)
    }

    fn try_open_device(&self) -> bool {
        match native::open_device(DEVICE_NAME, native::FILE_ATTRIBUTE_NORMAL) {
            Ok(h) => {
                lock(&self.state).handle = Some(OwnedHandle(h));
                true
            }
            Err(code) => {
                lock(&self.state).last_error = Some(native::win32_message(code));
                false
            }
        }
    }

    fn install_and_start(&self, driver_path: &str) -> Result<()> {
        let scm = open_scm().ok_or_else(|| {
            anyhow!("OpenSCManager failed (run as administrator). {}", native::last_error_message())
        })?;
        let name = wide(SERVICE_NAME);
        let display = wide("Roch CPU ring-0 driver");
        let bin = wide(driver_path);
        let created = unsafe {
            CreateServiceW(
                scm.0,
                name.as_ptr(),
                display.as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_KERNEL_DRIVER,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                bin.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        let svc = if created.is_null() {
            let err = native::last_error();
            if err == native::ERROR_SERVICE_EXISTS || err == native::ERROR_SERVICE_MARKED_FOR_DELETE {
                open_service(&scm)
                    .ok_or_else(|| anyhow!("OpenService failed. {}", native::last_error_message()))?
            } else {
                bail!("CreateService failed for the WinRing0 driver. {}", native::win32_message(err));
            }
        } else {
            *lock(&self.installed_by_us) = true;
            Scm(created)
        };

        if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
            let err = native::last_error();
            if err != native::ERROR_SERVICE_ALREADY_RUNNING {
                let msg = format!(
                    "StartService failed for the WinRing0 driver. \
                     Windows may have blocked the driver (Memory Integrity / vulnerable driver blocklist) or Defender quarantined WinRing0x64.sys. {}",
                    native::win32_message(err)
                );
                let mut ours = lock(&self.installed_by_us);
                if *ours {
                    unsafe { DeleteService(svc.0) };
                    *ours = false;
                }
                bail!(msg);
            }
        }
        Ok(())
    }

    pub fn get_driver_version(&self) -> u32 {
        let mut out = [0u8; 4];
        self.ioctl(IOCTL_GET_DRIVER_VERSION, &[], &mut out);
        u32::from_le_bytes(out)
    }

    /// How many handles the driver has open, ours included. WinRing0's device name is the same for
    /// every program that uses it, so another tool (an older ZenStates-Core build, a monitoring
    /// utility) may have opened the device this service created. Stopping the service under it
    /// leaves the driver in STOP_PENDING with the device unusable for everyone until that tool
    /// exits, which is exactly what happened on the bench; so the refcount is checked before a stop.
    pub fn get_ref_count(&self) -> u32 {
        let mut out = [0u8; 4];
        if self.ioctl(IOCTL_GET_REFCOUNT, &[], &mut out) { u32::from_le_bytes(out) } else { 0 }
    }

    fn read_port(&self, code: u32, port: u16) -> Result<u32> {
        let mut out = [0u8; 4];
        if !self.ioctl(code, &(port as u32).to_le_bytes(), &mut out) {
            bail!("Port read 0x{port:X} failed: {}", self.last_error().unwrap_or_default());
        }
        Ok(u32::from_le_bytes(out))
    }

    fn write_port(&self, code: u32, port: u16, value: u32) -> Result<()> {
        let mut input = [0u8; 8];
        input[..4].copy_from_slice(&(port as u32).to_le_bytes());
        input[4..].copy_from_slice(&value.to_le_bytes());
        if !self.ioctl(code, &input, &mut []) {
            bail!("Port write 0x{port:X} failed: {}", self.last_error().unwrap_or_default());
        }
        Ok(())
    }

    fn pci_address(bus: u8, device: u8, function: u8) -> u32 {
        ((bus as u32) << 8) | (((device & 0x1F) as u32) << 3) | (function & 7) as u32
    }

    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> bool {
        let mut st = lock(&self.state);
        let Some(handle) = st.handle.as_ref().map(|h| h.0 as HANDLE) else {
            st.last_error = Some("driver not open".into());
            diag::observe_driver_not_issued(code, output.len() as u32);
            return false;
        };
        let (ok, returned, err) = native::device_io_control(handle, code, input, output);
        if let Some(e) = err {
            st.last_error = Some(format!("{} (Win32 {e})", native::win32_message(e)));
        }
        diag::observe_ioctl_result(code, output.len() as u32, ok, returned, err);
        ok
    }
}

fn our_service_exists() -> bool {
    open_scm().is_some_and(|scm| open_service(&scm).is_some())
}

impl KernelDriver for WinRing0Driver {
    fn name(&self) -> String {
        "WinRing0 1.2".into()
    }

    fn is_open(&self) -> bool {
        lock(&self.state).handle.is_some()
    }

    fn last_error(&self) -> Option<String> {
        lock(&self.state).last_error.clone()
    }

    fn read_msr(&self, index: u32, cpu: i32) -> Option<u64> {
        let r = native::run_on_cpu(cpu, || {
            let mut out = [0u8; 8];
            self.ioctl(IOCTL_READ_MSR, &index.to_le_bytes(), &mut out).then(|| u64::from_le_bytes(out))
        });
        match r {
            Ok(v) => v,
            Err(e) => {
                lock(&self.state).last_error = Some(e.to_string());
                None
            }
        }
    }

    fn write_msr(&self, index: u32, value: u64, cpu: i32) -> bool {
        // #pragma pack(1) { uint Register; ulong Value; }
        let mut input = [0u8; 12];
        input[..4].copy_from_slice(&index.to_le_bytes());
        input[4..].copy_from_slice(&value.to_le_bytes());
        match native::run_on_cpu(cpu, || self.ioctl(IOCTL_WRITE_MSR, &input, &mut [])) {
            Ok(ok) => ok,
            Err(e) => {
                lock(&self.state).last_error = Some(e.to_string());
                false
            }
        }
    }

    fn read_io_port_byte(&self, port: u16) -> Result<u8> {
        Ok(self.read_port(IOCTL_READ_IO_PORT_BYTE, port)? as u8)
    }
    fn read_io_port_word(&self, port: u16) -> Result<u16> {
        Ok(self.read_port(IOCTL_READ_IO_PORT_WORD, port)? as u16)
    }
    fn read_io_port_dword(&self, port: u16) -> Result<u32> {
        self.read_port(IOCTL_READ_IO_PORT_DWORD, port)
    }
    fn write_io_port_byte(&self, port: u16, value: u8) -> Result<()> {
        self.write_port(IOCTL_WRITE_IO_PORT_BYTE, port, value as u32)
    }
    fn write_io_port_word(&self, port: u16, value: u16) -> Result<()> {
        self.write_port(IOCTL_WRITE_IO_PORT_WORD, port, value as u32)
    }
    fn write_io_port_dword(&self, port: u16, value: u32) -> Result<()> {
        self.write_port(IOCTL_WRITE_IO_PORT_DWORD, port, value)
    }

    fn read_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
        let mut input = [0u8; 8];
        input[..4].copy_from_slice(&Self::pci_address(bus, device, function).to_le_bytes());
        input[4..].copy_from_slice(&(offset as u32).to_le_bytes());
        let mut out = [0u8; 4];
        let ok = self.ioctl(IOCTL_READ_PCI_CONFIG, &input, &mut out);
        let v = u32::from_le_bytes(out);
        (ok && v != 0xFFFF_FFFF).then_some(v)
    }

    fn write_pci_config(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) -> bool {
        let mut input = [0u8; 12];
        input[..4].copy_from_slice(&Self::pci_address(bus, device, function).to_le_bytes());
        input[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        input[8..].copy_from_slice(&value.to_le_bytes());
        self.ioctl(IOCTL_WRITE_PCI_CONFIG, &input, &mut [])
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for WinRing0Driver {
    fn drop(&mut self) {
        let others = if self.is_open() { self.get_ref_count().max(1) - 1 } else { 0 };
        lock(&self.state).handle = None;
        let mut ours = lock(&self.installed_by_us);
        if !*ours {
            return;
        }
        *ours = false;
        if !native::is_only_instance() {
            return; // another copy started while we ran; leave the driver loaded
        }
        if others > 0 {
            return; // another program opened our device; stopping now would strand it
        }
        let Some(scm) = open_scm() else { return };
        let Some(svc) = open_service(&scm) else { return };
        unsafe {
            let mut status: SERVICE_STATUS = std::mem::zeroed();
            ControlService(svc.0, SERVICE_CONTROL_STOP, &mut status);
            DeleteService(svc.0);
        }
    }
}
