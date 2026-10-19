//! AMD Zen family CPU (Ryzen desktop and mobile) through MSRs, CPUID and the SMU.

use std::sync::Arc;

use anyhow::{Result, bail};

use super::KernelDriver;
use super::amd_smu::{AmdSmu, SmuMessageSet, round_digits};
use super::native;

/// One physical core as the SMU addresses it, plus the Windows threads that run on it.
#[derive(Clone, Debug, PartialEq)]
pub struct AmdCore {
    pub index: i32,
    pub ccd: i32,
    pub core_in_ccd: i32,
    pub threads: Vec<i32>,
}

impl AmdCore {
    pub fn label(&self) -> String {
        format!("Core {}", self.index)
    }
    pub fn location(&self) -> String {
        format!("CCD {} core {}", self.ccd, self.core_in_ccd)
    }
}

/// AMD Zen family CPU (Ryzen desktop and mobile) through MSRs, CPUID and the SMU. Everything
/// talks to the processor itself; the board only matters for whether PBO is enabled in the BIOS.
pub struct AmdCpu {
    drv: Arc<dyn KernelDriver>,

    brand_string: String,
    family: i32,
    model: i32,
    stepping: i32,
    package_type: i32,
    code_name: String,
    generation: String,
    is_supported: bool,
    patch_level: u32,

    logical_count: i32,
    threads_per_core: i32,
    core_count: i32,
    cores: Vec<AmdCore>,
    ccd_count: i32,
    topology_note: String,

    smu: AmdSmu,
    /// P0 multiplier from MSR 0xC0010064: what the TSC ticks at, for the BCLK measurement.
    base_ratio: i32,
}

// SMN
const THM_CUR_TEMP: u32 = 0x00059800;
const SVI3_TEL_PLANE0_F19: u32 = 0x00073014; // VDDCR_CPU telemetry, Zen 4 / Zen 5
const SVI2_TEL_PLANE0_AM4: u32 = 0x0005A050; // Zen 2 / Zen 3 AM4
const SVI2_TEL_PLANE0_ZEN1: u32 = 0x0005A00C;

/// `Encoding.ASCII.GetString(BitConverter.GetBytes(reg))`: little-endian bytes, non-ASCII as '?'.
fn ascii(reg: u32) -> String {
    reg.to_le_bytes().iter().map(|&b| if b < 0x80 { b as char } else { '?' }).collect()
}

/// .NET `Math.Round(double)`: round half to even.
fn round_even(v: f64) -> f64 {
    v.round_ties_even()
}

/// The topology `enumerate_cores` derives: cores, CCD count, note.
struct Topology {
    cores: Vec<AmdCore>,
    ccd_count: i32,
    note: String,
}

impl AmdCpu {
    pub const MSR_PSTATE_0: u32 = 0xC0010064;
    pub const MSR_HW_PSTATE_STATUS: u32 = 0xC0010293;
    pub const MSR_RAPL_POWER_UNIT: u32 = 0xC0010299;
    pub const MSR_PKG_ENERGY_STATUS: u32 = 0xC001029B;
    pub const MSR_PATCH_LEVEL: u32 = 0x8B;

    pub fn new(driver: Arc<dyn KernelDriver>) -> Result<Self> {
        let (_, ebx, ecx, edx) = native::cpuid(0, 0);
        let vendor = ascii(ebx) + &ascii(edx) + &ascii(ecx);
        if vendor != "AuthenticAMD" && vendor != "HygonGenuine" {
            bail!("Not an AMD CPU ({vendor}).");
        }

        let brand_string = Self::read_brand_string();
        let (l1_eax, l1_ebx, _, _) = native::cpuid(1, 0);
        let (l1_eax, l1_ebx) = (l1_eax as i32, l1_ebx as i32);
        let family = ((l1_eax >> 8) & 0xF) + ((l1_eax >> 20) & 0xFF);
        let model = ((l1_eax >> 4) & 0xF) | (((l1_eax >> 16) & 0xF) << 4);
        let stepping = l1_eax & 0xF;
        let mut logical_count = (l1_ebx >> 16) & 0xFF;
        if logical_count == 0 {
            logical_count = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1);
        }
        let (_, l81_ebx, _, _) = native::cpuid(0x8000_0001, 0);
        let package_type = (l81_ebx >> 28) as i32;
        let (_, l81e_ebx, _, _) = native::cpuid(0x8000_001E, 0);
        let threads_per_core = (((l81e_ebx as i32) >> 8) & 0xF) + 1;
        let core_count = 1.max(logical_count / 1.max(threads_per_core));

        let (code_name, generation, messages) = Self::identify(family, model, package_type);
        let is_supported = messages.is_some();
        let smu = AmdSmu::new(driver.clone(), messages.unwrap_or(SmuMessageSet::ZEN4));

        let patch_level = driver.read_msr(Self::MSR_PATCH_LEVEL, 0).map(|pl| pl as u32).unwrap_or(0);
        let base_ratio = match driver.read_msr(Self::MSR_PSTATE_0, 0) {
            Some(v) => Self::ratio_from_pstate(family, v),
            None => 0,
        };
        let topo = Self::enumerate_cores(
            &smu,
            family,
            model,
            &code_name,
            core_count,
            threads_per_core,
            logical_count,
        );

        Ok(Self {
            drv: driver,
            brand_string,
            family,
            model,
            stepping,
            package_type,
            code_name,
            generation,
            is_supported,
            patch_level,
            logical_count,
            threads_per_core,
            core_count,
            cores: topo.cores,
            ccd_count: topo.ccd_count,
            topology_note: topo.note,
            smu,
            base_ratio,
        })
    }

    pub fn brand_string(&self) -> &str {
        &self.brand_string
    }
    pub fn family(&self) -> i32 {
        self.family
    }
    pub fn model(&self) -> i32 {
        self.model
    }
    pub fn stepping(&self) -> i32 {
        self.stepping
    }
    pub fn package_type(&self) -> i32 {
        self.package_type
    }
    pub fn code_name(&self) -> &str {
        &self.code_name
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn is_supported(&self) -> bool {
        self.is_supported
    }
    pub fn patch_level(&self) -> u32 {
        self.patch_level
    }
    pub fn logical_count(&self) -> i32 {
        self.logical_count
    }
    pub fn threads_per_core(&self) -> i32 {
        self.threads_per_core
    }
    pub fn core_count(&self) -> i32 {
        self.core_count
    }
    pub fn cores(&self) -> &[AmdCore] {
        &self.cores
    }
    pub fn ccd_count(&self) -> i32 {
        self.ccd_count
    }
    pub fn topology_note(&self) -> &str {
        &self.topology_note
    }
    pub fn smu(&self) -> &AmdSmu {
        &self.smu
    }
    /// P0 multiplier from MSR 0xC0010064: what the TSC ticks at, for the BCLK measurement.
    pub fn base_ratio(&self) -> i32 {
        self.base_ratio
    }

    fn read_brand_string() -> String {
        let mut sb = String::new();
        for i in 0..3 {
            let (a, b, c, d) = native::cpuid(0x8000_0002 + i, 0);
            for reg in [a, b, c, d] {
                sb.push_str(&ascii(reg));
            }
        }
        sb.replace('\0', "").trim().to_string()
    }

    /// Family/model to code name and SMU message set. Package type 4 = SP3, 7 = TRX4, 2 = AM4, 1 = FL1 (Dragon Range).
    fn identify(family: i32, model: i32, package_type: i32) -> (String, String, Option<SmuMessageSet>) {
        let r = |code: &str, gen_: &str, msgs: Option<SmuMessageSet>| (code.to_string(), gen_.to_string(), msgs);
        match family {
            0x17 => match model {
                0x01 => {
                    return if package_type == 4 {
                        r("Naples", "Zen (EPYC)", Some(SmuMessageSet::ZEN))
                    } else if package_type == 7 {
                        r("Whitehaven", "Zen (Threadripper)", Some(SmuMessageSet::ZEN))
                    } else {
                        r("Summit Ridge", "Zen (Ryzen 1000)", Some(SmuMessageSet::ZEN))
                    };
                }
                0x08 => {
                    return if package_type == 4 || package_type == 7 {
                        r("Colfax", "Zen+ (Threadripper 2000)", Some(SmuMessageSet::ZEN_PLUS))
                    } else {
                        r("Pinnacle Ridge", "Zen+ (Ryzen 2000)", Some(SmuMessageSet::ZEN_PLUS))
                    };
                }
                0x11 => return r("Raven Ridge", "Zen APU (Ryzen 2000G)", None),
                0x18 => return r("Picasso", "Zen+ APU (Ryzen 3000G)", None),
                0x20 => return r("Dali", "Zen APU", None),
                0x31 => {
                    return if package_type == 7 {
                        r("Castle Peak", "Zen 2 (Threadripper 3000)", Some(SmuMessageSet::ZEN2))
                    } else {
                        r("Rome", "Zen 2 (EPYC)", Some(SmuMessageSet::ZEN2))
                    };
                }
                0x60 => return r("Renoir", "Zen 2 APU (Ryzen 4000)", Some(SmuMessageSet::apu("Zen 2 APU", 0))),
                0x68 => return r("Lucienne", "Zen 2 APU (Ryzen 5000U)", Some(SmuMessageSet::apu("Zen 2 APU", 0))),
                0x71 => return r("Matisse", "Zen 2 (Ryzen 3000)", Some(SmuMessageSet::ZEN2)),
                0x90 | 0x91 => {
                    return r("Van Gogh", "Zen 2 APU (Steam Deck)", Some(SmuMessageSet::apu("Zen 2 APU", 0)));
                }
                0xA0 => {
                    return r("Mendocino", "Zen 2 APU (Ryzen 7020)", Some(SmuMessageSet::apu("Zen 2 APU", 0x2F)));
                }
                _ => {}
            },
            0x19 => match model {
                0x01 => return r("Milan", "Zen 3 (EPYC)", Some(SmuMessageSet::ZEN3)),
                0x08 => return r("Chagall", "Zen 3 (Threadripper 5000)", Some(SmuMessageSet::ZEN3)),
                0x11 => return r("Genoa", "Zen 4 (EPYC)", Some(SmuMessageSet::ZEN4)),
                0x18 => return r("Storm Peak", "Zen 4 (Threadripper 7000)", Some(SmuMessageSet::ZEN4)),
                0x21 => return r("Vermeer", "Zen 3 (Ryzen 5000)", Some(SmuMessageSet::ZEN3)),
                0x44 => {
                    return r(
                        "Rembrandt",
                        "Zen 3+ APU (Ryzen 6000)",
                        Some(SmuMessageSet::apu_phoenix("Zen 3+ APU", 0x2F, false)),
                    );
                }
                0x50 => {
                    return r("Cezanne", "Zen 3 APU (Ryzen 5000G)", Some(SmuMessageSet::apu("Zen 3 APU", 0xC3)));
                }
                0x61 => {
                    return if package_type == 1 {
                        r("Dragon Range", "Zen 4 (Ryzen 7045HX)", Some(SmuMessageSet::ZEN4))
                    } else {
                        r("Raphael", "Zen 4 (Ryzen 7000)", Some(SmuMessageSet::ZEN4))
                    };
                }
                0x74 | 0x75 => {
                    return r(
                        "Phoenix",
                        "Zen 4 APU (Ryzen 7040 / 8000G)",
                        Some(SmuMessageSet::apu_phoenix("Zen 4 APU", 0xE1, false)),
                    );
                }
                0x78 => {
                    return r(
                        "Phoenix 2",
                        "Zen 4 APU (Ryzen 8000)",
                        Some(SmuMessageSet::apu_phoenix("Zen 4 APU", 0xE1, false)),
                    );
                }
                0x7C => {
                    return r(
                        "Hawk Point",
                        "Zen 4 APU (Ryzen 8040)",
                        Some(SmuMessageSet::apu_phoenix("Zen 4 APU", 0xE1, false)),
                    );
                }
                _ => {}
            },
            0x1A => match model {
                0x02 => return r("Turin", "Zen 5 (EPYC)", Some(SmuMessageSet::ZEN5)),
                0x08 => return r("Shimada Peak", "Zen 5 (Threadripper 9000)", Some(SmuMessageSet::SHIMADA_PEAK)),
                0x11 => return r("Turin Dense", "Zen 5c (EPYC)", Some(SmuMessageSet::ZEN5)),
                0x20 | 0x24 => {
                    return r(
                        "Strix Point",
                        "Zen 5 APU (Ryzen AI 300)",
                        Some(SmuMessageSet::apu_phoenix("Zen 5 APU", 0xAF, true)),
                    );
                }
                0x44 => return r("Granite Ridge", "Zen 5 (Ryzen 9000)", Some(SmuMessageSet::ZEN5)),
                0x60 | 0x68 => {
                    return r(
                        "Krackan Point",
                        "Zen 5 APU (Ryzen AI 300)",
                        Some(SmuMessageSet::apu_phoenix("Zen 5 APU", 0xAF, true)),
                    );
                }
                0x70 => {
                    return r(
                        "Strix Halo",
                        "Zen 5 APU (Ryzen AI Max)",
                        Some(SmuMessageSet::apu_phoenix("Zen 5 APU", 0xAF, true)),
                    );
                }
                0xA0 => return r("Bergamo", "Zen 5c (EPYC)", Some(SmuMessageSet::ZEN5)),
                _ => {}
            },
            _ => {}
        }
        (format!("Family {family:X}h model {model:02X}h"), "unknown Zen generation".into(), None)
    }

    // ------------------------------------------------------------------ topology
    /// Which CCDs exist and which cores on each are fused off, read from the SMN fuse registers the
    /// way ZenStates and ryzen_smu do. The SMU addresses a core by its physical position (CCD,
    /// core-in-CCD), so a 6-core CCD with cores 2 and 5 disabled must send 0,1,3,4,6,7.
    /// Falls back to a dense numbering if the fuses cannot be read.
    fn enumerate_cores(
        smu: &AmdSmu,
        family: i32,
        model: i32,
        code_name: &str,
        core_count: i32,
        threads_per_core: i32,
        logical_count: i32,
    ) -> Topology {
        let cores_per_ccd_max = 8;
        let mut fuse1: u32 = 0x5D218;
        let mut fuse2: u32 = 0x5D21C;
        let mut core_disable_base: u32 = 0x30081800 + 0x238;
        if family == 0x19 {
            core_disable_base = 0x30081800 + 0x598;
            if code_name == "Raphael" || code_name == "Dragon Range" {
                core_disable_base = 0x30081800 + 0x4D0;
                fuse1 += 0x1A4;
                fuse2 += 0x1A4;
            }
        } else if family == 0x17 && model != 0x71 && model != 0x31 {
            fuse1 += 0x40;
            fuse2 += 0x40;
        } else if family == 0x1A {
            core_disable_base = 0x304A03DC;
            fuse1 += 0x1A4;
            fuse2 += 0x1A4;
        }

        let mut cores: Vec<AmdCore> = Vec::new();
        let mut ccd_count = 0;
        let mut note = String::new();
        if !smu.messages().is_apu {
            if let Some(present) = smu.read_smn(fuse1) {
                if smu.read_smn(fuse2).is_some() {
                    let ccd_enable = (present >> 22) & 0x3;
                    let mut ccds: Vec<i32> = Vec::new();
                    for i in 0..2 {
                        if ((ccd_enable >> i) & 1) == 1 {
                            ccds.push(i);
                        }
                    }
                    if ccds.is_empty() {
                        ccds.push(0);
                    }
                    for &ccd in &ccds {
                        let disabled = match smu.read_smn(((ccd as u32) << 25).wrapping_add(core_disable_base)) {
                            Some(d) => d & 0xFF,
                            None => 0,
                        };
                        for c in 0..cores_per_ccd_max {
                            if ((disabled >> c) & 1) == 0 {
                                cores.push(AmdCore { index: cores.len() as i32, ccd, core_in_ccd: c, threads: Vec::new() });
                            }
                        }
                    }
                    ccd_count = ccds.len() as i32;
                    if cores.len() as i32 != core_count {
                        note = format!(
                            "fuse map lists {} cores on {} CCD(s) but CPUID reports {core_count}; using a dense numbering",
                            cores.len(),
                            ccds.len()
                        );
                        cores.clear();
                    } else {
                        note = format!("{} CCD(s), core map from fuses", ccds.len());
                    }
                }
            }
        }

        if cores.is_empty() {
            ccd_count = 1.max((core_count as f64 / 8.0).ceil() as i32);
            let per_ccd = (core_count as f64 / ccd_count as f64).ceil() as i32;
            for i in 0..core_count {
                cores.push(AmdCore { index: i, ccd: i / per_ccd, core_in_ccd: i % per_ccd, threads: Vec::new() });
            }
            if note.is_empty() {
                note = "dense core numbering (no fuse map)".into();
            }
        }

        // Windows numbers logical processors in APIC order: core 0's threads first, then core 1's.
        for (i, core) in cores.iter_mut().enumerate() {
            let first = i as i32 * threads_per_core;
            core.threads = (first..first + threads_per_core).filter(|&t| t < logical_count && t < 64).collect();
        }
        Topology { cores, ccd_count, note }
    }

    // ------------------------------------------------------------------ MSR readings
    /// Multiplier encoded in a P-state or HW P-state status register: Zen 5 uses 5 MHz steps, earlier parts FID/DID.
    fn ratio_from_pstate(family: i32, v: u64) -> i32 {
        if family >= 0x1A {
            return round_even(((v & 0xFFF) * 5) as f64 / 100.0) as i32;
        }
        let fid = (v & 0xFF) as i32;
        let did = ((v >> 8) & 0x3F) as i32;
        if did == 0 { 0 } else { round_even(fid as f64 * 200.0 / did as f64 / 100.0) as i32 }
    }

    fn mhz_from_pstate(family: i32, v: u64) -> f64 {
        if family >= 0x1A {
            return (v & 0xFFF) as f64 * 5.0;
        }
        let fid = (v & 0xFF) as i32;
        let did = ((v >> 8) & 0x3F) as i32;
        if did == 0 { 0.0 } else { fid as f64 * 200.0 / did as f64 }
    }

    /// Current multiplier of one core (its first thread), from the hardware P-state status.
    pub fn read_core_mhz(&self, core: &AmdCore) -> f64 {
        let Some(&first) = core.threads.first() else { return 0.0 };
        match self.drv.read_msr(Self::MSR_HW_PSTATE_STATUS, first) {
            Some(v) => Self::mhz_from_pstate(self.family, v),
            None => 0.0,
        }
    }

    /// Highest current core clock across the package, at BCLK = 100.
    pub fn read_max_core_mhz(&self) -> f64 {
        let mut it = self.cores.iter().map(|c| self.read_core_mhz(c));
        match it.next() {
            None => 0.0,
            Some(first) => it.fold(first, f64::max),
        }
    }

    pub fn read_package_energy_joules(&self) -> Result<f64> {
        let Some(u) = self.drv.read_msr(Self::MSR_RAPL_POWER_UNIT, 0) else { bail!("RDMSR 0xC0010299 failed.") };
        let unit = 1.0 / (1i32 << ((u >> 8) & 0x1F) as i32) as f64;
        let Some(e) = self.drv.read_msr(Self::MSR_PKG_ENERGY_STATUS, 0) else { bail!("RDMSR 0xC001029B failed.") };
        Ok((e & 0xFFFF_FFFF) as f64 * unit)
    }

    // ------------------------------------------------------------------ SMN readings
    /// Tctl from the thermal monitor block, in degrees C (with the range-select offset applied).
    pub fn read_temperature(&self) -> Option<f64> {
        let v = self.smu.read_smn(THM_CUR_TEMP)?;
        Some(Self::tctl_from_raw(&self.brand_string, v))
    }

    fn tctl_from_raw(brand: &str, v: u32) -> f64 {
        let mut t = (v >> 21) as f64 * 0.125;
        if (v & 0x80000) != 0 {
            t -= 49.0;
        }
        // Zen / Zen+ X parts report Tctl with a fixed offset above Tdie.
        if brand.contains("2700X") {
            t -= 10.0;
        } else if brand.contains("1600X") || brand.contains("1700X") || brand.contains("1800X") {
            t -= 20.0;
        } else if brand.contains("Threadripper 19") || brand.contains("Threadripper 29") {
            t -= 27.0;
        }
        t
    }

    /// Core rail VID the SMU is requesting, in volts, from the SVI telemetry register.
    pub fn read_core_vid(&self) -> Option<f64> {
        let (addr, svi3) = Self::vid_register(self.family, self.model, &self.code_name);
        let v = self.smu.read_smn(addr)?;
        Self::vid_from_raw(svi3, v)
    }

    /// Which telemetry register holds the core VID, and whether it is SVI3-encoded.
    fn vid_register(family: i32, model: i32, code_name: &str) -> (u32, bool) {
        if family >= 0x19 && !matches!(code_name, "Vermeer" | "Chagall" | "Milan" | "Cezanne") {
            return (SVI3_TEL_PLANE0_F19, true);
        }
        let addr = if family == 0x17 && matches!(model, 0x01 | 0x08 | 0x11 | 0x18 | 0x20) {
            SVI2_TEL_PLANE0_ZEN1
        } else {
            SVI2_TEL_PLANE0_AM4
        };
        (addr, false)
    }

    fn vid_from_raw(svi3: bool, v: u32) -> Option<f64> {
        if svi3 {
            let vid = (v >> 6) & 0x1FF;
            return if vid == 0 { None } else { Some(round_digits(0.245 + vid as f64 * 0.005, 3)) };
        }
        let vid2 = v >> 24;
        if vid2 == 0 { None } else { Some(round_digits(1.55 - vid2 as f64 * 0.00625, 3)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::amd_smu::fake::{FakeSmn, OK};
    use crate::hw::native::lock;

    fn smu_over(drv: &Arc<FakeSmn>, msgs: SmuMessageSet) -> AmdSmu {
        AmdSmu::without_pci_mutex(drv.clone(), msgs)
    }

    #[test]
    fn identify_picks_message_sets() {
        let (c, g, m) = AmdCpu::identify(0x1A, 0x44, 2);
        assert_eq!((c.as_str(), g.as_str(), m), ("Granite Ridge", "Zen 5 (Ryzen 9000)", Some(SmuMessageSet::ZEN5)));
        let (c, _, m) = AmdCpu::identify(0x19, 0x61, 1);
        assert_eq!((c.as_str(), m), ("Dragon Range", Some(SmuMessageSet::ZEN4)));
        let (c, _, m) = AmdCpu::identify(0x19, 0x61, 2);
        assert_eq!((c.as_str(), m), ("Raphael", Some(SmuMessageSet::ZEN4)));
        let (c, _, m) = AmdCpu::identify(0x17, 0x01, 7);
        assert_eq!((c.as_str(), m), ("Whitehaven", Some(SmuMessageSet::ZEN)));
        let (c, _, m) = AmdCpu::identify(0x17, 0x08, 4);
        assert_eq!((c.as_str(), m), ("Colfax", Some(SmuMessageSet::ZEN_PLUS)));
        let (c, _, m) = AmdCpu::identify(0x17, 0x11, 2);
        assert_eq!((c.as_str(), m), ("Raven Ridge", None));
        let (c, _, m) = AmdCpu::identify(0x17, 0xA0, 2);
        assert_eq!((c.as_str(), m), ("Mendocino", Some(SmuMessageSet::apu("Zen 2 APU", 0x2F))));
        let (c, _, m) = AmdCpu::identify(0x19, 0x21, 2);
        assert_eq!((c.as_str(), m), ("Vermeer", Some(SmuMessageSet::ZEN3)));
        let (c, _, m) = AmdCpu::identify(0x1A, 0x08, 7);
        assert_eq!((c.as_str(), m), ("Shimada Peak", Some(SmuMessageSet::SHIMADA_PEAK)));
        let (c, _, m) = AmdCpu::identify(0x1A, 0x70, 0);
        assert_eq!((c.as_str(), m), ("Strix Halo", Some(SmuMessageSet::apu_phoenix("Zen 5 APU", 0xAF, true))));
        let (c, g, m) = AmdCpu::identify(0x1B, 0x05, 0);
        assert_eq!((c.as_str(), g.as_str(), m), ("Family 1Bh model 05h", "unknown Zen generation", None));
    }

    #[test]
    fn core_label_and_location() {
        let c = AmdCore { index: 3, ccd: 1, core_in_ccd: 4, threads: vec![6, 7] };
        assert_eq!(c.label(), "Core 3");
        assert_eq!(c.location(), "CCD 1 core 4");
    }

    #[test]
    fn pstate_decoding() {
        // Zen 5: 5 MHz steps in bits 11:0.
        assert_eq!(AmdCpu::ratio_from_pstate(0x1A, 1140), 57);
        assert_eq!(AmdCpu::mhz_from_pstate(0x1A, 1140), 5700.0);
        // FID/DID: fid 0x90, did 8 -> 144 * 200 / 8 = 3600 MHz.
        let v = 0x90 | (8 << 8);
        assert_eq!(AmdCpu::ratio_from_pstate(0x19, v), 36);
        assert_eq!(AmdCpu::mhz_from_pstate(0x19, v), 3600.0);
        assert_eq!(AmdCpu::ratio_from_pstate(0x17, 0x90), 0);
        // Half rounds to even, as .NET does: fid 5 / did 4 -> 250 MHz -> ratio 2.5 -> 2.
        assert_eq!(AmdCpu::ratio_from_pstate(0x17, 5 | (4 << 8)), 2);
    }

    #[test]
    fn temperature_and_vid_decoding() {
        assert_eq!(AmdCpu::tctl_from_raw("AMD Ryzen 9 9950X", 400 << 21), 50.0);
        assert_eq!(AmdCpu::tctl_from_raw("AMD Ryzen 9 9950X", (400 << 21) | 0x80000), 1.0);
        assert_eq!(AmdCpu::tctl_from_raw("AMD Ryzen 7 2700X", 400 << 21), 40.0);
        assert_eq!(AmdCpu::vid_register(0x1A, 0x44, "Granite Ridge"), (SVI3_TEL_PLANE0_F19, true));
        assert_eq!(AmdCpu::vid_register(0x19, 0x21, "Vermeer"), (SVI2_TEL_PLANE0_AM4, false));
        assert_eq!(AmdCpu::vid_register(0x17, 0x08, "Pinnacle Ridge"), (SVI2_TEL_PLANE0_ZEN1, false));
        assert_eq!(AmdCpu::vid_from_raw(true, 200 << 6), Some(1.245));
        assert_eq!(AmdCpu::vid_from_raw(true, 0), None);
        assert_eq!(AmdCpu::vid_from_raw(false, 64 << 24), Some(1.15));
    }

    #[test]
    fn cores_from_fuse_map() {
        let drv = Arc::new(FakeSmn::new(&[], Box::new(|_, _, _| OK)));
        {
            let mut st = lock(&drv.state);
            // Granite Ridge: fuses at 0x5D218 + 0x1A4; both CCDs present.
            st.regs.insert(0x5D218 + 0x1A4, 0b11 << 22);
            st.regs.insert(0x5D21C + 0x1A4, 0);
            // CCD 0: cores 2 and 5 off; CCD 1: cores 0 and 7 off.
            st.regs.insert(0x304A03DC, 0b0010_0100);
            st.regs.insert((1 << 25) + 0x304A03DC, 0b1000_0001);
        }
        let smu = smu_over(&drv, SmuMessageSet::ZEN5);
        let t = AmdCpu::enumerate_cores(&smu, 0x1A, 0x44, "Granite Ridge", 12, 2, 24);
        assert_eq!(t.note, "2 CCD(s), core map from fuses");
        assert_eq!(t.ccd_count, 2);
        let pos: Vec<(i32, i32)> = t.cores.iter().map(|c| (c.ccd, c.core_in_ccd)).collect();
        assert_eq!(pos, vec![(0, 0), (0, 1), (0, 3), (0, 4), (0, 6), (0, 7), (1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6)]);
        assert_eq!(t.cores[5].threads, vec![10, 11]);
        assert_eq!(t.cores[11].index, 11);
    }

    #[test]
    fn fuse_mismatch_falls_back_to_dense() {
        let drv = Arc::new(FakeSmn::new(&[], Box::new(|_, _, _| OK)));
        {
            let mut st = lock(&drv.state);
            st.regs.insert(0x5D218 + 0x1A4, 0b01 << 22);
            st.regs.insert(0x5D21C + 0x1A4, 0);
        }
        let smu = smu_over(&drv, SmuMessageSet::ZEN4);
        let t = AmdCpu::enumerate_cores(&smu, 0x19, 0x61, "Raphael", 16, 2, 32);
        assert_eq!(
            t.note,
            "fuse map lists 8 cores on 1 CCD(s) but CPUID reports 16; using a dense numbering"
        );
        assert_eq!(t.ccd_count, 2);
        assert_eq!(t.cores.len(), 16);
        assert_eq!((t.cores[9].ccd, t.cores[9].core_in_ccd), (1, 1));
    }

    #[test]
    fn apu_uses_dense_numbering() {
        let drv = Arc::new(FakeSmn::new(&[], Box::new(|_, _, _| OK)));
        let smu = smu_over(&drv, SmuMessageSet::apu_phoenix("Zen 4 APU", 0xE1, false));
        let t = AmdCpu::enumerate_cores(&smu, 0x19, 0x74, "Phoenix", 6, 2, 12);
        assert_eq!(t.note, "dense core numbering (no fuse map)");
        assert_eq!(t.ccd_count, 1);
        assert_eq!(t.cores.len(), 6);
        assert_eq!(t.cores[5].threads, vec![10, 11]);
    }
}
