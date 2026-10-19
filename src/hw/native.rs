//! Win32 surface used by the hardware layer, plus small helpers shared by every module.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_SHARE_READ: u32 = 1;
pub const FILE_SHARE_WRITE: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

pub const ERROR_SERVICE_EXISTS: u32 = 1073;
pub const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
pub const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// Locks a mutex, recovering the data if a previous holder panicked. Hardware state guarded
/// here stays meaningful after a panic elsewhere (it mirrors registers, not invariants).
pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// NUL-terminated UTF-16 for the W APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// The system message for a Win32 error code, like .NET's `Win32Exception(code).Message`.
pub fn win32_message(code: u32) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    if n == 0 {
        return format!("Unknown error (0x{code:X})");
    }
    String::from_utf16_lossy(&buf[..n as usize]).trim_end().to_string()
}

/// Message for the calling thread's last Win32 error.
pub fn last_error_message() -> String {
    win32_message(last_error())
}

/// CPUID with a sub-leaf: (eax, ebx, ecx, edx).
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID is available on every x86-64 processor.
    let r = unsafe { std::arch::x86_64::__cpuid_count(leaf, subleaf) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Directory of the running executable (the .NET build's `AppContext.BaseDirectory`).
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Pins the calling thread to one logical CPU for the duration of `action`.
pub fn run_on_cpu<T>(cpu: i32, action: impl FnOnce() -> T) -> anyhow::Result<T> {
    if cpu < 0 {
        return Ok(action());
    }
    if cpu >= 64 {
        anyhow::bail!("Processor-group addressing is required above logical CPU 63.");
    }
    let thread = unsafe { GetCurrentThread() };
    let previous = unsafe { SetThreadAffinityMask(thread, 1usize << cpu) };
    if previous == 0 {
        anyhow::bail!(
            "Could not select logical CPU {cpu}; hardware access was not attempted. {}",
            last_error_message()
        );
    }
    struct Restore(HANDLE, usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { SetThreadAffinityMask(self.0, self.1) };
        }
    }
    let _restore = Restore(thread, previous);
    Ok(action())
}

/// Number of running processes whose executable name matches this one.
pub fn same_name_process_count() -> Option<usize> {
    let me = std::env::current_exe().ok()?;
    let my_name = me.file_name()?.to_string_lossy().to_lowercase();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut count = 0usize;
        let mut ok = Process32FirstW(snap, &mut entry) != 0;
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if name == my_name {
                count += 1;
            }
            ok = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        Some(count)
    }
}

/// True when no other copy of this program is running.
pub fn is_only_instance() -> bool {
    matches!(same_name_process_count(), Some(n) if n <= 1)
}

/// Attaches to the console of the launching process so diagnostic switches can print.
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Reads a firmware table (`GetSystemFirmwareTable`), e.g. provider `'RSMB'` for SMBIOS.
pub fn get_system_firmware_table(provider: u32, table_id: u32) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::SystemInformation::GetSystemFirmwareTable;
    unsafe {
        let size = GetSystemFirmwareTable(provider, table_id, std::ptr::null_mut(), 0);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let got = GetSystemFirmwareTable(provider, table_id, buf.as_mut_ptr().cast(), size);
        if got == 0 || got > size {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }
}

/// Reads a REG_SZ value under HKEY_LOCAL_MACHINE; `None` when the key or value is absent.
pub fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let sk = wide(subkey);
    let vn = wide(value);
    let mut size: u32 = 0;
    unsafe {
        if RegGetValueW(HKEY_LOCAL_MACHINE, sk.as_ptr(), vn.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size2 = (buf.len() * 2) as u32;
        if RegGetValueW(HKEY_LOCAL_MACHINE, sk.as_ptr(), vn.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size2) != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// Opens a device path with CreateFileW. Returns the raw handle or the Win32 error code.
pub fn open_device(path: &str, flags_and_attributes: u32) -> Result<HANDLE, u32> {
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    let p = wide(path);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags_and_attributes,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(last_error())
    } else {
        Ok(h)
    }
}

/// DeviceIoControl over byte buffers. Returns (ok, bytes returned, Win32 error when not ok).
pub fn device_io_control(handle: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> (bool, u32, Option<u32>) {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            if input.is_empty() { std::ptr::null() } else { input.as_ptr().cast() },
            input.len() as u32,
            if output.is_empty() { std::ptr::null_mut() } else { output.as_mut_ptr().cast() },
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    } != 0;
    let err = if ok { None } else { Some(last_error()) };
    (ok, returned, err)
}

/// An owned Win32 handle, closed on drop.
pub struct OwnedHandle(pub HANDLE);
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}
