//! The board's CPU voltage regulators, driven over PMBus where the board wires them to a bus the
//! operating system can reach.
//!
//! On the Gigabyte Z890 AORUS TACHYON ICE two Renesas digital multiphase controllers sit on the
//! PCH's serial I2C bus (the first DesignWare adapter): 0x60 (IC_DEVICE_ID 0x49D29800) and 0x62
//! (0x49D2AC00). Mapped by reading every page against the board's ITE sensor:
//!
//! | address | page | rail | at the time |
//! | --- | --- | --- | --- |
//! | 0x60 | 0 | VccIA (CPU core) | SVID: the CPU sets it |
//! | 0x60 | 1 | VccGT | off (iGPU disabled) |
//! | 0x62 | 1 | VccSA (memory controller, NGU) | fixed by the board at 1.320 V |
//! | 0x62 | 2 | VnnAON (D2D) | fixed by the board at 0.850 V |
//!
//! A rail the board holds fixed (mode bits 5:4 of register 0xF0 = 3, the same override mode MSI's
//! Renesas parts use) follows VOUT_COMMAND (0x21, mV), so its voltage can be changed by writing
//! a new target - the very register the BIOS programmed it through. A rail in SVID mode belongs
//! to the CPU and is only reported here: switching modes is not something this does. The root
//! helper enforces the same (see `linux::direct::vr_is_allowed`): page and target writes only,
//! 0.5-1.5 V.
//!
//! On this board VccSA being fixed by the regulator is also why the CPU's own SA target (OC
//! mailbox domain 4) changes nothing: the rail does not listen to the CPU's request.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::native::lock;
use super::{KernelDriver, KernelSmbus};

const R_PAGE: u8 = 0x00;
const R_VOUT_COMMAND: u8 = 0x21;
const R_READ_VOUT: u8 = 0x8B;
const R_PMBUS_REVISION: u8 = 0x98;
const R_MODE: u8 = 0xF0;
const MODE_OVERRIDE: u16 = 0x0030;

/// Boards whose regulator map is known.
pub fn board_is_mapped(board: &str) -> bool {
    board.trim().eq_ignore_ascii_case("Z890 AORUS TACHYON ICE")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VrRail {
    Vcore,
    VccSa,
    VnnAon,
}

impl VrRail {
    pub fn address(self) -> u8 {
        match self {
            VrRail::Vcore => 0x60,
            VrRail::VccSa | VrRail::VnnAon => 0x62,
        }
    }
    pub fn page(self) -> u8 {
        match self {
            VrRail::Vcore => 0,
            VrRail::VccSa => 1,
            VrRail::VnnAon => 2,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            VrRail::Vcore => "Vcore (VccIA)",
            VrRail::VccSa => "VCCSA",
            VrRail::VnnAon => "VNNAON",
        }
    }
    /// The range offered for a fixed rail. VnnAON above 1.0 V gives cold-boot trouble on Arrow
    /// Lake (SkatterBencher); VccSA's 1.40 V is well past what NGU 34x needed (1.30 V).
    pub fn range(self) -> (f64, f64) {
        match self {
            VrRail::Vcore => (0.6, 1.5),
            VrRail::VccSa => (0.95, 1.40),
            VrRail::VnnAon => (0.70, 1.05),
        }
    }
}

/// One page as read: what it is told to hold, what it holds, and who decides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VrPage {
    pub mode_register: u16,
    pub target_mv: u16,
    pub output_mv: u16,
}

impl VrPage {
    /// Fixed by the board (PMBus override): VOUT_COMMAND is what the rail holds.
    pub fn fixed(&self) -> bool {
        self.mode_register & MODE_OVERRIDE == MODE_OVERRIDE
    }
}

pub struct BoardVr {
    drv: Arc<dyn KernelDriver>,
    adapter: u32,
    /// PAGE is shared state on the chip: select, use, put back, under one lock.
    lock: Mutex<()>,
}

impl BoardVr {
    /// Finds the regulators: on a mapped board, the DesignWare adapter where 0x60 and 0x62 both
    /// answer as PMBus 1.3 devices.
    #[cfg(target_os = "linux")]
    pub fn try_create(drv: Arc<dyn KernelDriver>, board: &str) -> (Option<BoardVr>, String) {
        if !board_is_mapped(board) {
            return (None, "board regulators not mapped on this board".into());
        }
        let mut adapters: Vec<u32> = std::fs::read_dir("/sys/bus/i2c/devices")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n: u32 = e.file_name().to_str()?.strip_prefix("i2c-")?.parse().ok()?;
                let name = std::fs::read_to_string(e.path().join("name")).ok()?;
                name.starts_with("Synopsys DesignWare I2C adapter").then_some(n)
            })
            .collect();
        adapters.sort_unstable();
        for adapter in adapters {
            let pmbus13 = |address: u8| {
                drv.kernel_smbus(adapter, address, KernelSmbus::ReadByte { register: R_PMBUS_REVISION }).ok() == Some(0x33)
            };
            if pmbus13(0x60) && pmbus13(0x62) {
                let vr = BoardVr { drv, adapter, lock: Mutex::new(()) };
                return (Some(vr), format!("Renesas PMBus regulators at 0x60 and 0x62 on i2c-{adapter}"));
            }
        }
        (None, "the board's PMBus regulators did not answer".into())
    }

    fn rb(&self, address: u8, register: u8) -> Result<u8> {
        Ok(self.drv.kernel_smbus(self.adapter, address, KernelSmbus::ReadByte { register })? as u8)
    }
    fn rw(&self, address: u8, register: u8) -> Result<u16> {
        self.drv.kernel_smbus(self.adapter, address, KernelSmbus::ReadWord { register })
    }

    /// Runs `work` with `page` selected, then selects whatever page was selected before.
    fn on_page<T>(&self, rail: VrRail, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let _g = lock(&self.lock);
        let a = rail.address();
        let before = self.rb(a, R_PAGE)?;
        if before != rail.page() {
            self.drv.kernel_smbus(self.adapter, a, KernelSmbus::WriteByte { register: R_PAGE, value: rail.page() })?;
            if self.rb(a, R_PAGE)? != rail.page() {
                bail!("{} did not select page {}", rail.name(), rail.page());
            }
        }
        let result = work();
        if before != rail.page() && before <= 2 {
            let put_back = self.drv.kernel_smbus(self.adapter, a, KernelSmbus::WriteByte { register: R_PAGE, value: before });
            if let (Ok(_), Err(e)) = (&result, put_back) {
                bail!("{} page selector could not be put back to {before}: {e}", rail.name());
            }
        }
        result
    }

    pub fn read(&self, rail: VrRail) -> Result<VrPage> {
        self.on_page(rail, || {
            Ok(VrPage {
                mode_register: self.rw(rail.address(), R_MODE)?,
                target_mv: self.rw(rail.address(), R_VOUT_COMMAND)?,
                output_mv: self.rw(rail.address(), R_READ_VOUT)?,
            })
        })
    }

    /// The rail's output, in volts.
    pub fn read_output(&self, rail: VrRail) -> Option<f64> {
        self.read(rail).ok().map(|p| p.output_mv as f64 / 1000.0)
    }

    /// The target of a rail the board holds fixed; `None` while the CPU drives it.
    pub fn read_target(&self, rail: VrRail) -> Result<Option<f64>> {
        let p = self.read(rail)?;
        Ok(p.fixed().then(|| p.target_mv as f64 / 1000.0))
    }

    /// Sets a fixed rail's target and checks the output follows. A rail the CPU drives is refused,
    /// and so is a target outside the rail's range. On a failed check the old target goes back.
    pub fn set_target(&self, rail: VrRail, volts: f64) -> Result<()> {
        let (lo, hi) = rail.range();
        if !volts.is_finite() || volts < lo || volts > hi {
            bail!("{} accepts {lo:.3}-{hi:.3} V here.", rail.name());
        }
        let wanted = (volts * 1000.0).round() as u16;
        self.on_page(rail, || {
            let a = rail.address();
            let mode = self.rw(a, R_MODE)?;
            if mode & MODE_OVERRIDE != MODE_OVERRIDE {
                bail!(
                    "{} is on Auto (the CPU sets it over SVID). Give it a manual voltage in the BIOS first; this changes a manual rail's value, not who controls it.",
                    rail.name()
                );
            }
            let before = self.rw(a, R_VOUT_COMMAND)?;
            self.drv.kernel_smbus(self.adapter, a, KernelSmbus::WriteWord { register: R_VOUT_COMMAND, value: wanted })?;
            let check = (|| -> Result<()> {
                let target = self.rw(a, R_VOUT_COMMAND)?;
                if target != wanted {
                    bail!("{} target reads {target} mV after writing {wanted} mV", rail.name());
                }
                // The regulator slews in well under a millisecond; give the load-line a moment.
                std::thread::sleep(Duration::from_millis(60));
                let out = self.rw(a, R_READ_VOUT)?;
                // Load-line droop: SA sat 14 mV under its 1.320 V target at idle.
                if (out as i32 - wanted as i32).abs() > 40 {
                    bail!("{} output reads {out} mV after setting {wanted} mV", rail.name());
                }
                Ok(())
            })();
            if let Err(e) = check {
                let restored = self
                    .drv
                    .kernel_smbus(self.adapter, a, KernelSmbus::WriteWord { register: R_VOUT_COMMAND, value: before })
                    .is_ok()
                    && self.rw(a, R_VOUT_COMMAND).ok() == Some(before);
                return Err(anyhow!(
                    "{e}. {}",
                    if restored { format!("Previous target {before} mV restored.") } else { "Restoring the previous target could not be verified.".into() }
                ));
            }
            Ok(())
        })
    }
}
