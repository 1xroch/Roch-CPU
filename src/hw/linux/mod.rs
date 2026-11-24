//! Register access on Linux.
//!
//! Windows gets its access from a kernel driver, and the whole process is elevated to reach it.
//! Linux already has the primitives - `/dev/cpu/N/msr`, `iopl`, PCI configuration space through
//! sysfs, `/dev/i2c-N` - but they need root, and a Wayland window should not run as root: the
//! compositor's socket, the settings and the font cache belong to the person at the desk.
//!
//! So the window stays an ordinary user process and the privileged half is a small helper, this
//! same executable started with [`HELPER_ARG`] through `pkexec`. The two talk over pipes, one
//! [`KernelDriver`] call to a frame ([`protocol`]), and the helper refuses on its own what
//! [`direct`] refuses, however it is asked. Roch Viewer reaches its registers the same way.
//!
//! In order of preference the driver is:
//!
//! 1. [`direct::Direct`] in this process, when it already has the access (run as root, e.g.
//!    `sudo roch-cpu --probe`).
//! 2. This executable as the helper, through `pkexec`: the polkit prompt, the same question
//!    Windows asks with UAC at every launch.
//!
//! When neither starts, the window still opens and the Log says why, as on Windows with no driver.

pub mod direct;
pub mod helper;
pub mod protocol;
pub mod proxy;

use std::sync::Arc;

use anyhow::{Result, bail};

use super::KernelDriver;
use super::native;

/// The argument that makes this executable the privileged helper.
pub const HELPER_ARG: &str = "--hardware-helper";

/// Set to stop Roch CPU asking for access at all: the window then opens with every row
/// unavailable. For development and headless runs.
pub const NO_HELPER_VAR: &str = "ROCHCPU_NO_HELPER";

/// Opens the best driver this process can have.
pub fn open() -> Result<Arc<dyn KernelDriver>> {
    let direct_error = match direct::Direct::open() {
        Ok(direct) => return Ok(Arc::new(direct)),
        Err(e) => e,
    };
    // Already root and still refused: the helper would be refused the same way.
    if native::is_root() {
        return Err(direct_error);
    }
    if std::env::var_os(NO_HELPER_VAR).is_some() {
        bail!("Register access is turned off: {NO_HELPER_VAR} is set.");
    }
    Ok(Arc::new(proxy::Proxy::start().map_err(anyhow::Error::msg)?))
}

/// The kernel's I2C adapter whose name starts with `prefix`, as its number. World-readable, so
/// no helper is involved. The lowest number wins: a multiplexed bus adds its segments after the
/// adapter they hang off.
pub fn find_i2c_adapter(prefix: &str) -> Option<u32> {
    let mut found: Vec<u32> = std::fs::read_dir("/sys/bus/i2c/devices")
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let number: u32 = entry.file_name().to_str()?.strip_prefix("i2c-")?.parse().ok()?;
            let name = std::fs::read_to_string(entry.path().join("name")).ok()?;
            name.starts_with(prefix).then_some(number)
        })
        .collect();
    found.sort_unstable();
    found.first().copied()
}

/// The adapter's name, for the Log.
pub fn i2c_adapter_name(adapter: u32) -> Option<String> {
    std::fs::read_to_string(format!("/sys/bus/i2c/devices/i2c-{adapter}/name")).ok().map(|s| s.trim().to_string())
}

/// Why writes to MSRs may be refused here, when the kernel says so: `msr.allow_writes=off` on the
/// command line, or kernel lockdown. `None` when nothing in the way is visible.
pub fn msr_write_restriction() -> Option<String> {
    if let Ok(v) = std::fs::read_to_string("/sys/module/msr/parameters/allow_writes")
        && v.trim().eq_ignore_ascii_case("off")
    {
        return Some("the kernel refuses MSR writes (msr.allow_writes=off)".into());
    }
    if let Ok(v) = std::fs::read_to_string("/sys/kernel/security/lockdown")
        && !v.contains("[none]")
    {
        return Some(format!("kernel lockdown is active ({})", v.trim()));
    }
    None
}
