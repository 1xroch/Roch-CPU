//! Intel overclocking mailbox (MSR 0x150).

use std::fmt;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use super::KernelDriver;
use super::native::lock;

/// Voltage/frequency settings of one FIVR domain as reported by the OC mailbox.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VfDomainSettings {
    pub max_ratio: i32,
    /// Static override target in volts (0 = none / adaptive).
    pub target_volts: f64,
    pub override_mode: bool,
    /// Offset in volts, -1.0 .. +1.0 (11-bit signed in 1/1024 V).
    pub offset_volts: f64,
}

impl fmt::Display for VfDomainSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ratio={} mode={} target={:.3}V offset={:+.0}mV",
            self.max_ratio,
            if self.override_mode { "override" } else { "adaptive" },
            self.target_volts,
            self.offset_volts * 1000.0
        )
    }
}

/// Intel overclocking mailbox (MSR 0x150). Used for per-domain voltage offsets
/// and static overrides. Works through the CPU FIVR/SVID path, so it does
/// not depend on the motherboard VRM controller.
pub struct OcMailbox {
    drv: Arc<dyn KernelDriver>,
    cpu: i32,
    /// Serialises whole transactions (and read-modify-write sequences). The C# lock is
    /// re-entrant; here the public methods take it once and call the `*_locked` helpers.
    lock: Mutex<()>,
}

impl OcMailbox {
    pub const MSR_OC_MAILBOX: u32 = 0x150;

    pub const CMD_READ_VF: u8 = 0x10;
    pub const CMD_WRITE_VF: u8 = 0x11;
    pub const CMD_READ_ICCMAX: u8 = 0x16;
    pub const CMD_WRITE_ICCMAX: u8 = 0x17;
    /// Turbo control word; MSI Dragon Power ORs 0x14 into it on every CPU-ratio apply.
    pub const CMD_READ_MISC_TURBO: u8 = 0x18;
    pub const CMD_WRITE_MISC_TURBO: u8 = 0x19;
    /// Core ratio ceiling (low byte). Separate from MSR 0x1AD and the domain-0 VF ratio: on the
    /// Z790 MPOWER it read x54 and held the cores there with both of those at x55.
    pub const CMD_READ_CORE_RATIO_CEILING: u8 = 0x1C;
    pub const CMD_WRITE_CORE_RATIO_CEILING: u8 = 0x1D;

    // Domain indices. Read off the hardware rather than taken from the usual documentation, by
    // comparing every domain against the values the vendor tool displays on a Raptor Lake-S /
    // Z790 board: domain 4 held the BIOS "SA Voltage" (1.190 V) and domain 5 the
    // "CPU E-Core L2 Voltage" (1.050 V), both matching to the millivolt. Domain 3 is the uncore
    // and reads zero here. Published tables commonly list E-core L2 as 3, which is wrong on
    // this part - reading domain 3 gets a rail nobody set.
    /// IA cores (P-cores)
    pub const DOMAIN_CORE: i32 = 0;
    /// Graphics
    pub const DOMAIN_GT: i32 = 1;
    /// Ring / LLC (cache)
    pub const DOMAIN_RING: i32 = 2;
    /// Uncore
    pub const DOMAIN_UNCORE: i32 = 3;
    /// System agent
    pub const DOMAIN_SA: i32 = 4;
    /// E-core L2 (present even when the E-cores are disabled)
    pub const DOMAIN_ECORE: i32 = 5;

    pub fn new(drv: Arc<dyn KernelDriver>, cpu: i32) -> Self {
        Self { drv, cpu, lock: Mutex::new(()) }
    }

    /// Executes one mailbox transaction. Returns the response code (0 = OK) and the data dword.
    pub fn execute(&self, command: u8, param1: u8, param2: u8, data: u32) -> Result<(u8, u32)> {
        let _g = lock(&self.lock);
        self.execute_locked(command, param1, param2, data)
    }

    fn execute_locked(&self, command: u8, param1: u8, param2: u8, data: u32) -> Result<(u8, u32)> {
        if !self.wait_not_busy() {
            bail!("OC mailbox busy.");
        }
        let v: u64 = (1u64 << 63)
            | ((command as u64) << 32)
            | ((param1 as u64) << 40)
            | ((param2 as u64) << 48)
            | data as u64;
        if !self.drv.write_msr(Self::MSR_OC_MAILBOX, v, self.cpu) {
            bail!("OC mailbox write failed (MSR 0x150 not accepted; mailbox disabled or CPU not supported).");
        }
        if !self.wait_not_busy() {
            bail!("OC mailbox did not complete.");
        }
        let Some(r) = self.drv.read_msr(Self::MSR_OC_MAILBOX, self.cpu) else {
            bail!("OC mailbox response could not be read; the command is not verified.");
        };
        if (r >> 63) != 0 {
            bail!("OC mailbox response changed during the transaction; try again with other tuning tools closed.");
        }
        Ok((((r >> 32) & 0xFF) as u8, r as u32))
    }

    fn wait_not_busy(&self) -> bool {
        for _ in 0..1000 {
            let Some(v) = self.drv.read_msr(Self::MSR_OC_MAILBOX, self.cpu) else {
                return false;
            };
            if (v >> 63) == 0 {
                return true;
            }
            crate::hw::native::spin_wait(200); // Thread.SpinWait(200)
        }
        false
    }

    pub fn is_available(&self) -> bool {
        matches!(self.execute(Self::CMD_READ_VF, Self::DOMAIN_CORE as u8, 0, 0), Ok((0, _)))
    }

    pub fn describe_status(status: u8) -> String {
        match status {
            0 => "OK".into(),
            1 => "invalid command".into(),
            2 => "illegal data".into(),
            3 => "reserved".into(),
            4 => "ratio exceeds limit".into(),
            5 => "voltage exceeds limit".into(),
            6 => "OC locked".into(),
            7 => "invalid domain".into(),
            _ => format!("error 0x{status:02X}"),
        }
    }

    pub fn read_domain(&self, domain: i32) -> Result<VfDomainSettings> {
        let _g = lock(&self.lock);
        self.read_domain_locked(domain)
    }

    fn read_domain_locked(&self, domain: i32) -> Result<VfDomainSettings> {
        let (st, d) = self.execute_locked(Self::CMD_READ_VF, domain as u8, 0, 0)?;
        if st != 0 {
            bail!("Mailbox read of domain {domain} failed: {}.", Self::describe_status(st));
        }
        Ok(Self::decode(d))
    }

    pub fn decode(d: u32) -> VfDomainSettings {
        let mut off_raw = ((d >> 21) & 0x7FF) as i32;
        if (off_raw & 0x400) != 0 {
            off_raw -= 0x800;
        }
        VfDomainSettings {
            max_ratio: (d & 0xFF) as i32,
            target_volts: ((d >> 8) & 0xFFF) as f64 / 1024.0,
            override_mode: ((d >> 20) & 1) == 1,
            offset_volts: off_raw as f64 / 1024.0,
        }
    }

    pub fn encode(s: &VfDomainSettings) -> u32 {
        // Math.Round is round-half-to-even; (int) of a double saturates (NaN -> 0) like `as`.
        let target = ((s.target_volts * 1024.0).round_ties_even() as i32).clamp(0, 0xFFF) as u32;
        let off = ((s.offset_volts * 1024.0).round_ties_even() as i32).clamp(-1024, 1023);
        let off_raw = (off & 0x7FF) as u32;
        (s.max_ratio & 0xFF) as u32 | (target << 8) | ((if s.override_mode { 1u32 } else { 0 }) << 20) | (off_raw << 21)
    }

    pub fn write_domain(&self, domain: i32, s: &VfDomainSettings) -> Result<()> {
        let _g = lock(&self.lock);
        self.write_domain_locked(domain, s)
    }

    fn write_domain_locked(&self, domain: i32, s: &VfDomainSettings) -> Result<()> {
        let before = Self::encode(&self.read_domain_locked(domain)?);
        let requested = Self::encode(s);
        let attempt = (|| -> Result<()> {
            let (st, _) = self.execute_locked(Self::CMD_WRITE_VF, domain as u8, 0, requested)?;
            if st != 0 {
                bail!("Mailbox write to domain {domain} rejected: {}.", Self::describe_status(st));
            }
            let actual = Self::encode(&self.read_domain_locked(domain)?);
            if actual != requested {
                bail!(
                    "Mailbox domain {domain} did not retain the request (requested 0x{requested:08X}, read back 0x{actual:08X}). BIOS restrictions or a hypervisor may be filtering writes."
                );
            }
            Ok(())
        })();
        if let Err(ex) = attempt {
            let restore = match (|| -> Result<bool> {
                let (restore_status, _) = self.execute_locked(Self::CMD_WRITE_VF, domain as u8, 0, before)?;
                Ok(restore_status == 0 && Self::encode(&self.read_domain_locked(domain)?) == before)
            })() {
                Ok(true) => "Previous mailbox value restored and read back.".to_string(),
                Ok(false) => "Previous mailbox value could not be verified after restore.".to_string(),
                Err(restore_error) => format!("Restore failed: {restore_error}"),
            };
            bail!("{ex} {restore}");
        }
        Ok(())
    }

    /// Sets an adaptive-mode voltage offset, keeping the domain ratio.
    ///
    /// A domain in override mode keeps its override: the .NET build (and the first Rust port)
    /// cleared override mode and the target here, so "SA offset -5 mV" on a BIOS that fixes SA at
    /// 1.189 V silently switched SA to adaptive voltage instead - and the restore to 0 left it
    /// adaptive. On the Z790 MPOWER with overclocked DDR5 that hard-froze the machine three times.
    /// So in override mode a nonzero offset is refused (change the target instead), and 0 only
    /// clears an offset while leaving the override target alone.
    pub fn set_offset(&self, domain: i32, offset_volts: f64) -> Result<()> {
        let _g = lock(&self.lock);
        let mut s = self.read_domain_locked(domain)?;
        if s.override_mode {
            if offset_volts != 0.0 {
                bail!(
                    "Domain {domain} is in override mode ({:.3} V); an offset would switch it to adaptive voltage. Change the voltage target instead, or set the target to 0 (Auto) first.",
                    s.target_volts
                );
            }
            if s.offset_volts == 0.0 {
                return Ok(());
            }
        } else {
            s.target_volts = 0.0;
        }
        s.offset_volts = offset_volts;
        self.write_domain_locked(domain, &s)
    }

    /// Sets a static override voltage (volts). 0 restores adaptive mode.
    pub fn set_override(&self, domain: i32, volts: f64) -> Result<()> {
        let _g = lock(&self.lock);
        let mut s = self.read_domain_locked(domain)?;
        if volts <= 0.0 {
            s.override_mode = false;
            s.target_volts = 0.0;
        } else {
            s.override_mode = true;
            s.target_volts = volts;
        }
        self.write_domain_locked(domain, &s)
    }

    /// Current core ratio ceiling (mailbox 0x1C, low byte); `None` where the command is unsupported.
    pub fn read_core_ratio_ceiling(&self) -> Option<i32> {
        match self.execute(Self::CMD_READ_CORE_RATIO_CEILING, 0, 0, 0) {
            Ok((0, d)) => Some((d & 0xFF) as i32),
            _ => None,
        }
    }

    /// Sets the core ratio ceiling to `wanted`, the way MSI Dragon Power applies a CPU ratio:
    /// mailbox 0x1D with the ratio, then 0x14 ORed into the turbo control word (0x18 -> 0x19).
    /// Without this the cores stay at the ceiling the BIOS left, whatever MSR 0x1AD says. Returns
    /// whether anything was written. Firmware that does not implement 0x1C is left alone.
    pub fn set_core_ratio_ceiling(&self, wanted: i32) -> Result<bool> {
        // A ceiling of 0 means none is reported (or the command is a stub): leave it alone, as
        // the domain-0 ratio is when the mailbox reports 0.
        let Some(current) = self.read_core_ratio_ceiling().filter(|&c| c > 0) else { return Ok(false) };
        if current == wanted || !(1..=255).contains(&wanted) {
            return Ok(false);
        }
        let (st, _) = self.execute(Self::CMD_WRITE_CORE_RATIO_CEILING, 0, 0, wanted as u32)?;
        if st != 0 {
            bail!("Core ratio ceiling x{wanted} rejected: {}.", Self::describe_status(st));
        }
        if let Ok((0, misc)) = self.execute(Self::CMD_READ_MISC_TURBO, 0, 0, 0) {
            if misc & 0x14 != 0x14 {
                let (st, _) = self.execute(Self::CMD_WRITE_MISC_TURBO, 0, 0, misc | 0x14)?;
                if st != 0 {
                    bail!("Turbo control update rejected: {}.", Self::describe_status(st));
                }
            }
        }
        // The new ceiling shows up a little after the write is acknowledged (50 -> 55 still read
        // x50 straight away and x55 seconds later), so give the read-back up to ~0.5 s.
        let mut last = None;
        for _ in 0..25 {
            last = self.read_core_ratio_ceiling();
            if last == Some(wanted) {
                return Ok(true);
            }
            crate::hw::native::sleep_ms(20);
        }
        match last {
            Some(r) if r == wanted => Ok(true),
            other => bail!(
                "Core ratio ceiling did not read back x{wanted} (read {}).",
                other.map_or("nothing".to_string(), |r| format!("x{r}"))
            ),
        }
    }

    /// IccMax for a domain in amperes (mailbox 0x16, 1/4 A units).
    pub fn read_icc_max(&self, domain: i32) -> Option<f64> {
        match self.execute(Self::CMD_READ_ICCMAX, domain as u8, 0, 0) {
            Ok((0, d)) => Some((d & 0x3FF) as f64 / 4.0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::intel_cpu::fakes::FakeDriver;

    #[test]
    fn voltage_request_is_quantized_retained_and_verified() {
        let d = FakeDriver::new();
        let mb = OcMailbox::new(d.clone(), 0);
        mb.set_override(0, 1.3).unwrap();
        let actual = mb.read_domain(0).unwrap();
        assert!(actual.override_mode && (actual.target_volts - 1.3).abs() < 1.0 / 1024.0, "Wrong voltage readback");
    }

    #[test]
    fn acknowledged_but_ignored_voltage_fails_and_restores() {
        let d = FakeDriver::new();
        d.state().ignore_voltage = true;
        let mb = OcMailbox::new(d.clone(), 0);
        assert!(mb.set_override(0, 1.3).is_err());
        assert!(d.state().domains.get(&0).copied().unwrap_or(0) == 0, "Baseline changed");
    }

    #[test]
    fn mailbox_read_failure_cannot_masquerade_as_zero_success() {
        let d = FakeDriver::new();
        d.state().fail_mailbox_response = true;
        assert!(OcMailbox::new(d.clone(), 0).read_domain(0).is_err());
    }

    #[test]
    fn negative_offset_preserves_signed_hardware_encoding() {
        let mb = OcMailbox::new(FakeDriver::new(), 0);
        mb.set_offset(0, -0.05).unwrap();
        assert!((mb.read_domain(0).unwrap().offset_volts + 0.05).abs() < 1.0 / 1024.0, "Offset sign/precision");
    }

    #[test]
    fn offset_never_drops_an_override() {
        let d = FakeDriver::new();
        let mb = OcMailbox::new(d.clone(), 4);
        mb.set_override(4, 1.189).unwrap();
        let before = mb.read_domain(4).unwrap();
        assert!(mb.set_offset(4, -0.005).is_err(), "nonzero offset in override mode must be refused");
        assert_eq!(mb.read_domain(4).unwrap(), before, "refused offset changed the domain");
        mb.set_offset(4, 0.0).unwrap();
        let after = mb.read_domain(4).unwrap();
        assert!(after.override_mode && (after.target_volts - 1.189).abs() < 1.0 / 1024.0, "restoring 0 dropped the override");
    }

    #[test]
    fn display_matches_dotnet_format() {
        let s = VfDomainSettings { max_ratio: 50, target_volts: 1.3, override_mode: true, offset_volts: -0.05 };
        assert_eq!(s.to_string(), "ratio=50 mode=override target=1.300V offset=-50mV");
        assert_eq!(VfDomainSettings::default().to_string(), "ratio=0 mode=adaptive target=0.000V offset=+0mV");
    }
}
