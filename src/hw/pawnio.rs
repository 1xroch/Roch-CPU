//! Client for namazso's PawnIO driver (https://pawnio.eu): a signed kernel driver that runs
//! signed, sandboxed "modules" and exposes each module's functions by name. It is optional
//! here. WinRing0 does everything except read the AMD SMU power table, which needs a physical
//! memory mapping the WinRing0 build cannot do; when PawnIO is installed the bundled RyzenSMU
//! module reads that table in the kernel. Nothing is installed by Roch CPU: the user installs
//! PawnIO themselves, and the module blob is loaded into the already-running service.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Result, bail};

use super::native::{self, OwnedHandle, lock};

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_TYPE: u32 = 41394u32 << 16;
const IOCTL_LOAD_BINARY: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_EXECUTE_FN: u32 = DEVICE_TYPE | (0x841 << 2);
const FN_NAME_LENGTH: usize = 32;

pub struct PawnIo {
    /// Also the execution lock.
    handle: Mutex<OwnedHandle>,
    last_error: Mutex<Option<String>>,
}

/// A dotted version as the uninstall entry stores it ("2.0.1.0").
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub Vec<u32>);

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Option<Vec<u32>> = s.trim().split('.').map(|p| p.parse().ok()).collect();
        let parts = parts?;
        if parts.len() < 2 || parts.len() > 4 {
            return None;
        }
        Some(Self(parts))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s: Vec<String> = self.0.iter().map(|v| v.to_string()).collect();
        f.write_str(&s.join("."))
    }
}

impl PawnIo {
    /// Version of the installed PawnIO, from its uninstall entry; `None` when it is not installed.
    pub fn installed_version() -> Option<Version> {
        native::read_hklm_string(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO", "DisplayVersion")
            .and_then(|s| Version::parse(&s))
    }

    pub fn is_installed() -> bool {
        Self::installed_version().is_some()
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    /// Opens the driver and loads one signed module from a file. Fails with the reason if either step fails.
    pub fn load_module(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("PawnIO module not found. ({})", path.display());
        }
        let h = match native::open_device(DEVICE_PATH, 0) {
            Ok(h) => OwnedHandle(h),
            Err(err) => bail!(
                "PawnIO device not reachable ({}); is the PawnIO service running?",
                native::win32_message(err)
            ),
        };
        let bin = std::fs::read(path)?;
        let (ok, _, err) = native::device_io_control(h.0, IOCTL_LOAD_BINARY, &bin, &mut []);
        if !ok {
            bail!(
                "PawnIO refused the module {} ({}). Only modules signed for PawnIO load; try a newer PawnIO.",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                native::win32_message(err.unwrap_or(0))
            );
        }
        Ok(Self { handle: Mutex::new(h), last_error: Mutex::new(None) })
    }

    /// Calls a module function: 32-byte name followed by the input longs; the output is a run of
    /// longs. Returns `None` on failure (`last_error` says why) so callers can fall back.
    pub fn execute(&self, function: &str, input: &[i64], output_count: usize) -> Option<Vec<i64>> {
        let mut in_buf = vec![0u8; FN_NAME_LENGTH + input.len() * 8];
        let name = function.as_bytes();
        let n = name.len().min(FN_NAME_LENGTH - 1);
        in_buf[..n].copy_from_slice(&name[..n]);
        for (i, v) in input.iter().enumerate() {
            in_buf[FN_NAME_LENGTH + i * 8..FN_NAME_LENGTH + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        let mut out_buf = vec![0u8; output_count * 8];
        let h = lock(&self.handle);
        let (ok, read, err) = native::device_io_control(h.0, IOCTL_EXECUTE_FN, &in_buf, &mut out_buf);
        if !ok {
            *lock(&self.last_error) = Some(format!("{function}: {}", native::win32_message(err.unwrap_or(0))));
            return None;
        }
        let count = output_count.min((read / 8) as usize);
        Some(
            (0..count)
                .map(|i| i64::from_le_bytes(out_buf[i * 8..i * 8 + 8].try_into().unwrap()))
                .collect(),
        )
    }
}
