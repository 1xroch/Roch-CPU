//! The operating-system surface used by the hardware layer, plus small helpers shared by every
//! module. Win32 on Windows; on Linux the same calls are answered from libc, `/proc` and `/sys`.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(windows)]
pub const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
pub const GENERIC_WRITE: u32 = 0x4000_0000;
#[cfg(windows)]
pub const FILE_SHARE_READ: u32 = 1;
#[cfg(windows)]
pub const FILE_SHARE_WRITE: u32 = 2;
#[cfg(windows)]
pub const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

#[cfg(windows)]
pub const ERROR_SERVICE_EXISTS: u32 = 1073;
#[cfg(windows)]
pub const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
#[cfg(windows)]
pub const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// Locks a mutex, recovering the data if a previous holder panicked. Hardware state guarded
/// here stays meaningful after a panic elsewhere (it mirrors registers, not invariants).
pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
/// NUL-terminated UTF-16 for the W APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

#[cfg(windows)]
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

#[cfg(windows)]
/// Message for the calling thread's last Win32 error.
pub fn last_error_message() -> String {
    win32_message(last_error())
}

/// CPUID with a sub-leaf: (eax, ebx, ecx, edx).
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID is available on every x86-64 processor.
    let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Number of logical CPUs to enumerate, numbered from 0.
#[cfg(windows)]
pub fn logical_cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Directory of the running executable (the .NET build's `AppContext.BaseDirectory`).
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `Thread.SpinWait(iterations)`. .NET normalises each iteration to about 35 ns whatever the
/// CPU's `pause` latency, so the busy-poll budgets ported from it (OC mailbox, SMBus) are time
/// budgets; a raw `pause` loop is shorter or longer depending on the core. Spin for that time.
pub fn spin_wait(iterations: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_nanos(iterations as u64 * 35);
    while std::time::Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

/// The shortest wait [`sleep_ms`] makes; see there.
pub const MIN_SLEEP_MS: u64 = 8;

#[cfg(windows)]
/// `Thread.Sleep(ms)`: the plain Win32 `Sleep`, which rounds up to the system timer tick.
///
/// Not `std::thread::sleep`: Rust's uses a high-resolution waitable timer, so `sleep(1)` really
/// is 1 ms, where the .NET build's `Thread.Sleep(1)` waited a whole tick (measured ~7.6 ms on the
/// Z790 MPOWER). Every EC doorbell, EC page and SMBus poll loop here was written and validated
/// against the .NET rate; at the high-resolution rate those loops hammer the embedded controller
/// about seven times harder - exactly the kind of traffic that has hard-reset this board
/// (Kernel-Power 41). The wall-clock timeouts around the loops are unchanged.
///
/// Plain `Sleep` still depends on the system timer: any program that raises the timer resolution
/// (HWiNFO, browsers, games) turns `Sleep(1)` back into ~1 ms. So short waits get a fixed 8 ms
/// floor, about the rate the .NET build polled at, whatever else is running. Longer waits are
/// unchanged; every poll loop keeps its own wall-clock timeout.
pub fn sleep_ms(ms: u64) {
    use windows_sys::Win32::System::Threading::Sleep;
    let ms = if (1..MIN_SLEEP_MS).contains(&ms) { MIN_SLEEP_MS } else { ms };
    // SAFETY: Sleep has no preconditions.
    unsafe { Sleep(ms.min(u32::MAX as u64 - 1) as u32) };
}

#[cfg(windows)]
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

#[cfg(windows)]
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
#[cfg(windows)]
pub fn is_only_instance() -> bool {
    matches!(same_name_process_count(), Some(n) if n <= 1)
}

#[cfg(windows)]
/// Attaches to the console of the launching process so diagnostic switches can print.
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
/// An owned Win32 handle, closed on drop.
pub struct OwnedHandle(pub HANDLE);
#[cfg(windows)]
unsafe impl Send for OwnedHandle {}
#[cfg(windows)]
unsafe impl Sync for OwnedHandle {}
#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The Linux side of everything above.
#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};

    use super::MIN_SLEEP_MS;

    /// The calling thread's last OS error, as text.
    pub fn last_error_message() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// The same floor as the Windows build: every EC, SMBus and mailbox poll loop was validated
    /// at roughly the old .NET rate, and Linux sleeps as precisely as asked. Short waits get the
    /// same fixed 8 ms floor; longer waits are unchanged.
    pub fn sleep_ms(ms: u64) {
        let ms = if (1..MIN_SLEEP_MS).contains(&ms) { MIN_SLEEP_MS } else { ms };
        std::thread::sleep(Duration::from_millis(ms));
    }

    /// The logical CPU the calling thread is running on.
    pub fn current_cpu() -> i32 {
        // SAFETY: sched_getcpu takes nothing and returns -1 on failure.
        let cpu = unsafe { libc::sched_getcpu() };
        cpu.max(0)
    }

    /// Pins the calling thread to one logical CPU for the duration of `action`, then puts the
    /// previous affinity back.
    pub fn run_on_cpu<T>(cpu: i32, action: impl FnOnce() -> T) -> Result<T> {
        if cpu < 0 {
            return Ok(action());
        }
        if cpu as usize >= libc::CPU_SETSIZE as usize {
            bail!("Logical CPU {cpu} is beyond what this build can address.");
        }
        // SAFETY: cpu_set_t is plain data; sched_[gs]etaffinity(0, ...) act on the calling thread.
        unsafe {
            let mut previous: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut previous) != 0 {
                bail!("Could not read this thread's CPU affinity; hardware access was not attempted. {}", super::last_error_message());
            }
            let mut wanted: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu as usize, &mut wanted);
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &wanted) != 0 {
                bail!("Could not select logical CPU {cpu}; hardware access was not attempted. {}", super::last_error_message());
            }
            struct Restore(libc::cpu_set_t);
            impl Drop for Restore {
                fn drop(&mut self) {
                    // SAFETY: as above.
                    unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &self.0) };
                }
            }
            let _restore = Restore(previous);
            // The scheduler moves the thread at its next opportunity; yield so it has one.
            std::thread::yield_now();
            Ok(action())
        }
    }

    /// Number of running processes whose executable is this one.
    pub fn same_name_process_count() -> Option<usize> {
        let me = std::fs::canonicalize("/proc/self/exe").ok()?;
        let count = std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
            .filter(|e| std::fs::read_link(e.path().join("exe")).is_ok_and(|p| p == me))
            .count();
        // Another user's processes (the root helper among them) cannot be resolved; count this one.
        Some(count.max(1))
    }

    /// The highest online CPU number plus one. Not `available_parallelism`, which shrinks to this
    /// thread's affinity mask and cgroup quota; the topology needs every CPU.
    pub fn logical_cpu_count() -> usize {
        let parse = |text: &str| -> Option<usize> {
            text.trim().split(',').filter_map(|r| r.rsplit('-').next()?.trim().parse::<usize>().ok()).max().map(|m| m + 1)
        };
        std::fs::read_to_string("/sys/devices/system/cpu/online")
            .ok()
            .and_then(|t| parse(&t))
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
    }

    /// A terminal process already has its console.
    pub fn attach_parent_console() {}

    /// True when this process is root.
    pub fn is_root() -> bool {
        // SAFETY: geteuid takes nothing and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// The person at the desk, also when this runs as root through sudo or pkexec.
    pub fn desktop_uid() -> u32 {
        for var in ["PKEXEC_UID", "SUDO_UID"] {
            if let Some(uid) = std::env::var(var).ok().and_then(|v| v.parse().ok()) {
                return uid;
            }
        }
        // SAFETY: getuid takes nothing and cannot fail.
        unsafe { libc::getuid() }
    }

    // ------------------------------------------------------------ named mutex

    #[derive(Default)]
    struct Owner {
        /// Alive for as long as the owning thread is: see [`thread_token`].
        thread: Option<Weak<()>>,
        depth: u32,
        file: Option<File>,
    }

    #[derive(Default)]
    struct Entry {
        owner: Mutex<Owner>,
        released: Condvar,
    }

    thread_local! {
        static THREAD_TOKEN: Arc<()> = Arc::new(());
    }

    /// A value that dies with this thread. A thread id would not say that the owner has gone,
    /// which is what makes a mutex abandoned.
    fn thread_token() -> Arc<()> {
        THREAD_TOKEN.with(Arc::clone)
    }

    fn entry(name: &str) -> Arc<Entry> {
        static ENTRIES: OnceLock<Mutex<HashMap<String, Arc<Entry>>>> = OnceLock::new();
        let mut entries = super::lock(ENTRIES.get_or_init(Default::default));
        entries.entry(name.to_string()).or_default().clone()
    }

    /// The directory the cross-process lock files live in: the desk user's runtime directory,
    /// which Roch Viewer and Roch Monitor use for the same names, so the three tools serialise
    /// against one another as they do on Windows.
    fn lock_dir() -> PathBuf {
        let run_user = PathBuf::from(format!("/run/user/{}", desktop_uid()));
        if run_user.is_dir() {
            return run_user;
        }
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
    }

    /// `Global\Access_ISABUS.HTP.Method` as `.rochviewer-Global_Access_ISABUS.HTP.Method.lock`.
    pub fn lock_file_name(name: &str) -> String {
        let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect();
        format!(".rochviewer-{safe}.lock")
    }

    fn lock_file(name: &str) -> Option<File> {
        let path = lock_dir().join(lock_file_name(name));
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).ok()?;
        // Created by root (the CLI under sudo): hand it to the desk user, so the window can
        // still open it later.
        if super::is_root() {
            let uid = desktop_uid();
            // SAFETY: a path we just opened; failure only costs the window the cross-process half.
            if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
                unsafe { libc::chown(c.as_ptr(), uid, u32::MAX) };
            }
        }
        Some(file)
    }

    /// Whether a kernel hwmon driver is using the Super I/O. Its register accesses cannot be
    /// locked against from here, and interleaving with them reads the wrong bank, so the ISA
    /// mutex cannot be had while one is bound, which every caller reads as "leave it alone".
    pub fn superio_owned_by_kernel() -> Option<&'static str> {
        static OWNER: OnceLock<Option<String>> = OnceLock::new();
        OWNER
            .get_or_init(|| {
                std::fs::read_dir("/sys/class/hwmon")
                    .ok()?
                    .flatten()
                    .filter_map(|entry| std::fs::read_to_string(entry.path().join("name")).ok())
                    .map(|name| name.trim().to_string())
                    .find(|name| name.starts_with("nct6") || name.starts_with("it8"))
            })
            .as_deref()
    }

    /// The Windows named mutex the hardware code is written against: owned by a thread,
    /// recursive, acquired when its owner died holding it. Within this process that is a
    /// condition variable; between processes, an advisory `flock` on a file named for the mutex,
    /// which the kernel releases however the holder ended. `None` makes a private one.
    pub struct NamedMutex {
        entry: Arc<Entry>,
        name: Option<String>,
    }

    impl NamedMutex {
        pub fn create(name: Option<&str>) -> Result<NamedMutex> {
            let entry = match name {
                Some(n) => entry(n),
                None => Arc::new(Entry::default()),
            };
            Ok(NamedMutex { entry, name: name.map(str::to_string) })
        }

        /// `WaitOne(ms)`, with an abandoned mutex counted as acquired.
        pub fn wait(&self, timeout_ms: u32) -> bool {
            if self.name.as_deref() == Some(r"Global\Access_ISABUS.HTP.Method") && superio_owned_by_kernel().is_some() {
                return false;
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            let token = thread_token();
            {
                let mut owner = super::lock(&self.entry.owner);
                loop {
                    match owner.thread.as_ref().map(Weak::upgrade) {
                        // Free, or its thread has gone: abandoned, so acquired.
                        None | Some(None) => {
                            owner.depth = 0;
                            owner.file = None;
                            break;
                        }
                        Some(Some(held)) if Arc::ptr_eq(&held, &token) => {
                            owner.depth += 1;
                            return true;
                        }
                        Some(Some(_)) => {}
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    // Short, because a thread dying while holding this signals nothing.
                    let wait = (deadline - now).min(Duration::from_millis(10));
                    owner = self.entry.released.wait_timeout(owner, wait).unwrap_or_else(|e| e.into_inner()).0;
                }
                owner.thread = Some(Arc::downgrade(&token));
                owner.depth = 1;
            }
            // Other processes, outside the lock so a wait on them does not hold up this one.
            let file = self.name.as_deref().and_then(lock_file);
            if let Some(file) = &file {
                loop {
                    // SAFETY: an fd this function owns.
                    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                        break;
                    }
                    if Instant::now() >= deadline {
                        let mut owner = super::lock(&self.entry.owner);
                        owner.thread = None;
                        owner.depth = 0;
                        drop(owner);
                        self.entry.released.notify_one();
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            super::lock(&self.entry.owner).file = file;
            true
        }

        /// `ReleaseMutex()`; fails when the calling thread does not own the mutex.
        pub fn release(&self) -> Result<()> {
            let token = thread_token();
            let mut owner = super::lock(&self.entry.owner);
            let mine = owner.thread.as_ref().and_then(Weak::upgrade).is_some_and(|t| Arc::ptr_eq(&t, &token));
            if !mine || owner.depth == 0 {
                bail!("Object synchronization method was called from an unsynchronized block of code.");
            }
            owner.depth -= 1;
            if owner.depth == 0 {
                owner.thread = None;
                // Closing the file is what releases the flock.
                owner.file = None;
                drop(owner);
                self.entry.released.notify_one();
            }
            Ok(())
        }
    }
}
