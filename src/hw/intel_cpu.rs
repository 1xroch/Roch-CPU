//! Intel hybrid (Alder Lake / Raptor Lake, LGA1700) CPU access through MSRs.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use anyhow::{Result, bail};

use super::KernelDriver;
use super::native::{self, cpuid};
use super::oc_mailbox::OcMailbox;

/// The current CPU's L2 sharing group: its x2APIC ID with the bits that address CPUs sharing the
/// same L2 cache shifted out (CPUID leaf 4 sub-leaf for the level-2 cache, EAX bits 25:14).
fn l2_cluster_key() -> u32 {
    let (_, _, _, apic) = cpuid(0xB, 0);
    for sub in 0..8 {
        let (eax, _, _, _) = cpuid(4, sub);
        if eax & 0x1F == 0 {
            break;
        }
        if (eax >> 5) & 7 == 2 {
            let sharing = ((eax >> 14) & 0xFFF) + 1;
            let shift = 32 - (sharing - 1).leading_zeros();
            return apic >> shift;
        }
    }
    apic >> 3
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalCpu {
    pub index: i32,
    pub apic_id: u32,
    pub core_id: i32,
    pub is_p_core: bool,
}

impl LogicalCpu {
    pub fn new(index: i32, apic_id: u32, core_id: i32, is_p_core: bool) -> Self {
        Self { index, apic_id, core_id, is_p_core }
    }
}

/// The package power-limit register decoded (the C# value tuple
/// `(pl1, pl2, locked, pl1Enabled, pl2Enabled)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackagePowerLimits {
    pub pl1: f64,
    pub pl2: f64,
    pub locked: bool,
    pub pl1_enabled: bool,
    pub pl2_enabled: bool,
}

/// Intel hybrid (Alder Lake / Raptor Lake, LGA1700) CPU access through MSRs.
/// Nothing here is board specific: everything talks to the CPU itself.
pub struct IntelCpu {
    drv: Arc<dyn KernelDriver>,

    brand_string: String,
    family: i32,
    model: i32,
    stepping: i32,
    is_hybrid: bool,
    hypervisor_present: bool,
    logical_cpus: Vec<LogicalCpu>,
    /// E-core clusters, worked out once on first use (CPUID on each E-core; never changes).
    e_clusters: OnceLock<Vec<Vec<i32>>>,
    p_core_count: i32,
    e_core_count: i32,
    first_p_thread: i32,
    first_e_thread: i32,
    generation: String,
    is_lga1700_family: bool,
    mailbox: OcMailbox,

    base_ratio: i32,
    min_ratio: i32,
    ratio_limits_programmable: bool,
    tdp_programmable: bool,
    tj_max: i32,
    microcode_revision: u32,
}

impl IntelCpu {
    pub const MSR_PLATFORM_INFO: u32 = 0xCE;
    pub const MSR_IA32_BIOS_SIGN_ID: u32 = 0x8B;
    pub const MSR_FLEX_RATIO: u32 = 0x194;
    pub const MSR_IA32_PERF_STATUS: u32 = 0x198;
    pub const MSR_IA32_PERF_CTL: u32 = 0x199;
    pub const MSR_IA32_THERM_STATUS: u32 = 0x19C;
    pub const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
    pub const MSR_TURBO_RATIO_LIMIT: u32 = 0x1AD;
    pub const MSR_TURBO_RATIO_LIMIT_CORES: u32 = 0x1AE;
    pub const MSR_IA32_PACKAGE_THERM_STATUS: u32 = 0x1B1;
    pub const MSR_RAPL_POWER_UNIT: u32 = 0x606;
    pub const MSR_PKG_POWER_LIMIT: u32 = 0x610;
    pub const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
    pub const MSR_UNCORE_RATIO_LIMIT: u32 = 0x620;
    pub const MSR_UNCORE_PERF_STATUS: u32 = 0x621;
    pub const MSR_ATOM_TURBO_RATIO_LIMIT: u32 = 0x650;
    pub const MSR_ATOM_TURBO_RATIO_LIMIT_CORES: u32 = 0x651;

    pub fn new(driver: Arc<dyn KernelDriver>) -> Result<Self> {
        let topology = Self::enumerate_topology()?;
        Self::with_topology(driver, topology)
    }

    /// The C# `internal IntelCpu(IKernelDriver, IReadOnlyList<LogicalCpu>)` constructor, used
    /// with a fixed topology by the tests.
    pub fn with_topology(driver: Arc<dyn KernelDriver>, topology: Vec<LogicalCpu>) -> Result<Self> {
        let drv = driver;
        let brand_string = Self::read_brand_string();
        let (l1_eax, _, l1_ecx, _) = cpuid(1, 0);
        let hypervisor_present = (l1_ecx & (1u32 << 31)) != 0;
        let mut family = ((l1_eax >> 8) & 0xF) as i32;
        let mut model = ((l1_eax >> 4) & 0xF) as i32;
        let ext_family = ((l1_eax >> 20) & 0xFF) as i32;
        let ext_model = ((l1_eax >> 16) & 0xF) as i32;
        if family == 0xF {
            family += ext_family;
        }
        if family == 6 || family == 0xF {
            model |= ext_model << 4;
        }
        let stepping = (l1_eax & 0xF) as i32;

        let (_, _, _, l7_edx) = cpuid(7, 0);
        let is_hybrid = ((l7_edx >> 15) & 1) == 1;

        let (generation, is_lga1700_family) = match model {
            0x97 => ("12th Gen (Alder Lake-S)".to_string(), true),
            0x9A => ("12th Gen (Alder Lake-P)".to_string(), false),
            0xB7 => ("13th/14th Gen (Raptor Lake-S)".to_string(), true),
            0xBF => ("13th/14th Gen (Raptor Lake-S, Alder Lake die)".to_string(), true),
            0xBA => ("13th Gen (Raptor Lake-P)".to_string(), false),
            _ => (format!("Family 6 Model 0x{model:02X}"), false),
        };

        let logical_cpus = topology;
        let p_core_count =
            logical_cpus.iter().filter(|c| c.is_p_core).map(|c| c.core_id).collect::<HashSet<_>>().len() as i32;
        let e_core_count =
            logical_cpus.iter().filter(|c| !c.is_p_core).map(|c| c.core_id).collect::<HashSet<_>>().len() as i32;
        let first_p_thread = logical_cpus.iter().find(|c| c.is_p_core).map(|c| c.index).unwrap_or(0);
        let first_e_thread = logical_cpus.iter().find(|c| !c.is_p_core).map(|c| c.index).unwrap_or(-1);

        // Intel documents the loaded microcode revision in the high dword of
        // IA32_BIOS_SIGN_ID after CPUID leaf 1 has serialised the update signature.
        let microcode_revision = native::run_on_cpu(first_p_thread, || {
            drv.write_msr(Self::MSR_IA32_BIOS_SIGN_ID, 0, first_p_thread);
            let _ = cpuid(1, 0);
            match drv.read_msr(Self::MSR_IA32_BIOS_SIGN_ID, first_p_thread) {
                Some(signature) => (signature >> 32) as u32,
                None => 0,
            }
        })?;

        let mailbox = OcMailbox::new(drv.clone(), first_p_thread);

        let (mut base_ratio, mut min_ratio, mut ratio_limits_programmable, mut tdp_programmable) = (0, 0, false, false);
        if let Some(pi) = drv.read_msr(Self::MSR_PLATFORM_INFO, first_p_thread) {
            base_ratio = ((pi >> 8) & 0xFF) as i32;
            min_ratio = ((pi >> 40) & 0xFF) as i32;
            ratio_limits_programmable = ((pi >> 28) & 1) == 1;
            tdp_programmable = ((pi >> 29) & 1) == 1;
        }
        let mut tj_max = match drv.read_msr(Self::MSR_TEMPERATURE_TARGET, first_p_thread) {
            Some(tt) => ((tt >> 16) & 0xFF) as i32,
            None => 100,
        };
        if tj_max == 0 {
            tj_max = 100;
        }

        Ok(Self {
            drv,
            brand_string,
            family,
            model,
            stepping,
            is_hybrid,
            hypervisor_present,
            logical_cpus,
            e_clusters: OnceLock::new(),
            p_core_count,
            e_core_count,
            first_p_thread,
            first_e_thread,
            generation,
            is_lga1700_family,
            mailbox,
            base_ratio,
            min_ratio,
            ratio_limits_programmable,
            tdp_programmable,
            tj_max,
            microcode_revision,
        })
    }

    /// Test seam for the C# model tests, which build an uninitialised `IntelCpu` and set only
    /// `_drv` and `Mailbox` (CPU 0): no CPUID, topology, thread pinning or MSR access here.
    /// Every other field keeps its zero/empty value.
    #[cfg(test)]
    pub(crate) fn with_driver_only_for_test(driver: Arc<dyn KernelDriver>) -> Self {
        Self {
            mailbox: OcMailbox::new(driver.clone(), 0),
            drv: driver,
            brand_string: String::new(),
            family: 0,
            model: 0,
            stepping: 0,
            is_hybrid: false,
            hypervisor_present: false,
            logical_cpus: Vec::new(),
            e_clusters: OnceLock::new(),
            p_core_count: 0,
            e_core_count: 0,
            first_p_thread: 0,
            first_e_thread: 0,
            generation: String::new(),
            is_lga1700_family: false,
            base_ratio: 0,
            min_ratio: 0,
            ratio_limits_programmable: false,
            tdp_programmable: false,
            tj_max: 0,
            microcode_revision: 0,
        }
    }

    // ------------------------------------------------------------- properties
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
    pub fn is_hybrid(&self) -> bool {
        self.is_hybrid
    }
    pub fn hypervisor_present(&self) -> bool {
        self.hypervisor_present
    }
    pub fn logical_cpus(&self) -> &[LogicalCpu] {
        &self.logical_cpus
    }
    pub fn p_core_count(&self) -> i32 {
        self.p_core_count
    }
    pub fn e_core_count(&self) -> i32 {
        self.e_core_count
    }
    pub fn first_p_thread(&self) -> i32 {
        self.first_p_thread
    }
    pub fn first_e_thread(&self) -> i32 {
        self.first_e_thread
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn is_lga1700_family(&self) -> bool {
        self.is_lga1700_family
    }
    pub fn mailbox(&self) -> &OcMailbox {
        &self.mailbox
    }
    pub fn base_ratio(&self) -> i32 {
        self.base_ratio
    }
    pub fn min_ratio(&self) -> i32 {
        self.min_ratio
    }
    pub fn ratio_limits_programmable(&self) -> bool {
        self.ratio_limits_programmable
    }
    pub fn tdp_programmable(&self) -> bool {
        self.tdp_programmable
    }
    pub fn tj_max(&self) -> i32 {
        self.tj_max
    }
    pub fn microcode_revision(&self) -> u32 {
        self.microcode_revision
    }
    /// The driver this CPU object talks through.
    pub fn driver(&self) -> &Arc<dyn KernelDriver> {
        &self.drv
    }

    fn read_brand_string() -> String {
        let mut bytes = Vec::with_capacity(48);
        for i in 0..3u32 {
            let (a, b, c, d) = cpuid(0x8000_0002 + i, 0);
            for reg in [a, b, c, d] {
                bytes.extend_from_slice(&reg.to_le_bytes());
            }
        }
        // Encoding.ASCII: bytes above 0x7F decode to '?'.
        let s: String = bytes.iter().map(|&b| if b < 0x80 { b as char } else { '?' }).collect();
        s.replace('\0', "").trim().to_string()
    }

    fn enumerate_topology() -> Result<Vec<LogicalCpu>> {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(64);
        let mut list = Vec::with_capacity(n);
        for i in 0..n as i32 {
            let (apic, core_id, pcore) = native::run_on_cpu(i, || {
                let (max_leaf, _, _, _) = cpuid(0, 0);
                let mut pcore = true;
                if max_leaf >= 0x1A {
                    let (l1a_eax, _, _, _) = cpuid(0x1A, 0);
                    let core_type = (l1a_eax >> 24) & 0xFF;
                    pcore = core_type != 0x20; // 0x20 = Atom (E-core), 0x40 = Core (P-core)
                }
                let (lb_eax, _, _, lb_edx) = cpuid(0xB, 0);
                let apic = lb_edx;
                let smt_shift = lb_eax & 0x1F;
                (apic, (apic >> smt_shift) as i32, pcore)
            })?;
            list.push(LogicalCpu::new(i, apic, core_id, pcore));
        }
        Ok(list)
    }

    // ------------------------------------------------------------- helpers
    fn read_or_throw(&self, msr: u32, cpu: i32) -> Result<u64> {
        match self.drv.read_msr(msr, cpu) {
            Some(v) => Ok(v),
            None => bail!("RDMSR 0x{msr:X} failed on CPU {cpu}."),
        }
    }

    fn write_or_throw(&self, msr: u32, v: u64, cpu: i32) -> Result<()> {
        if !self.drv.write_msr(msr, v, cpu) {
            bail!(
                "WRMSR 0x{msr:X} failed on CPU {cpu}: {}. The driver or firmware rejected this register write.",
                self.drv.last_error().unwrap_or_default()
            );
        }
        Ok(())
    }

    fn write_verified(&self, msr: u32, value: u64, cpu: i32) -> Result<()> {
        let before = self.read_or_throw(msr, cpu)?;
        self.write_or_throw(msr, value, cpu)?;
        let check = (|| -> Result<()> {
            let actual = self.read_or_throw(msr, cpu)?;
            if actual != value {
                bail!("MSR 0x{msr:X} ignored the request: requested 0x{value:016X}, read back 0x{actual:016X}.");
            }
            Ok(())
        })();
        if let Err(ex) = check {
            let restored =
                self.drv.write_msr(msr, before, cpu) && self.drv.read_msr(msr, cpu).is_some_and(|r| r == before);
            bail!(
                "{ex}{}",
                if restored { " Previous register value restored." } else { " Restore could not be verified." }
            );
        }
        Ok(())
    }

    fn encode_ratios(ratios: &[i32]) -> Result<u64> {
        if ratios.len() != 8 || ratios.iter().any(|&r| !(8..=120).contains(&r)) {
            bail!("Every turbo group needs an integer ratio between 8 and 120.");
        }
        let mut value = 0u64;
        for (i, &r) in ratios.iter().enumerate() {
            value |= (r as u64) << (8 * i);
        }
        Ok(value)
    }

    /// MSR_FLEX_RATIO bit 20: BIOS locked overclocking (typical on non-Z chipsets).
    pub fn is_oc_locked(&self) -> bool {
        self.drv.read_msr(Self::MSR_FLEX_RATIO, self.first_p_thread).is_some_and(|v| ((v >> 20) & 1) == 1)
    }

    // ------------------------------------------------------------- ratios
    /// Turbo ratio table for P-cores: `ratios[i]` applies when up to `cores[i]` cores are active.
    /// Returns `(ratios, cores)`.
    pub fn read_p_core_turbo_table(&self) -> Result<(Vec<i32>, Vec<i32>)> {
        let r = self.read_or_throw(Self::MSR_TURBO_RATIO_LIMIT, self.first_p_thread)?;
        let mut ratios = vec![0i32; 8];
        let mut cores = vec![0i32; 8];
        let c = self.drv.read_msr(Self::MSR_TURBO_RATIO_LIMIT_CORES, self.first_p_thread);
        for i in 0..8 {
            ratios[i] = ((r >> (8 * i)) & 0xFF) as i32;
            cores[i] = match c {
                Some(c) => ((c >> (8 * i)) & 0xFF) as i32,
                None => i as i32 + 1,
            };
        }
        Ok((ratios, cores))
    }

    /// Returns `(ratios, cores)`; both empty when there are no E-cores.
    pub fn read_e_core_turbo_table(&self) -> Result<(Vec<i32>, Vec<i32>)> {
        if self.e_core_count == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let r = self.read_or_throw(Self::MSR_ATOM_TURBO_RATIO_LIMIT, self.first_e_thread)?;
        let mut ratios = vec![0i32; 8];
        let mut cores = vec![0i32; 8];
        let c = self.drv.read_msr(Self::MSR_ATOM_TURBO_RATIO_LIMIT_CORES, self.first_e_thread);
        for i in 0..8 {
            ratios[i] = ((r >> (8 * i)) & 0xFF) as i32;
            let count = match c {
                Some(c) => ((c >> (8 * i)) & 0xFF) as i32,
                None => 0,
            };
            cores[i] = if count > 0 && count <= self.e_core_count { count } else { 0 }; // 0xFF is not 255 active cores.
        }
        Ok((ratios, cores))
    }

    /// The first logical CPU of each physical P-core, in core order (one per core: Hyper-Threading
    /// siblings share the core's ratio ceiling).
    pub fn p_core_threads(&self) -> Vec<i32> {
        let mut seen = HashSet::new();
        self.logical_cpus.iter().filter(|c| c.is_p_core && seen.insert(c.core_id)).map(|c| c.index).collect()
    }

    /// E-core clusters: the E-core logical CPUs that share an L2 cache (CPUID leaf 4), in order.
    /// Intel E-cores run one clock per cluster of four, and the ratio ceiling set on any of them
    /// applies to the whole cluster (on the i5-14600KF, 0x1D on E-core 6 moved E-cores 6-9 only).
    pub fn e_core_clusters(&self) -> Vec<Vec<i32>> {
        self.e_clusters
            .get_or_init(|| {
                let mut clusters: Vec<(u32, Vec<i32>)> = Vec::new();
                for c in self.logical_cpus.iter().filter(|c| !c.is_p_core) {
                    let key = native::run_on_cpu(c.index, l2_cluster_key).unwrap_or(c.apic_id >> 3);
                    match clusters.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, v)) => v.push(c.index),
                        None => clusters.push((key, vec![c.index])),
                    }
                }
                clusters.into_iter().map(|(_, v)| v).collect()
            })
            .clone()
    }

    /// Each E-core cluster's ratio ceiling (mailbox 0x1C on its first E-core).
    pub fn read_e_cluster_ratio_ceilings(&self) -> Vec<Option<i32>> {
        self.e_core_clusters()
            .iter()
            .map(|cl| OcMailbox::new(self.drv.clone(), cl[0]).read_core_ratio_ceiling())
            .collect()
    }

    /// One ratio per E-core cluster, in `e_core_clusters` order. The E-core turbo table is raised
    /// to the highest value first if it is below it, as for the P-cores.
    pub fn write_e_cluster_ratio_ceilings(&self, ratios: &[i32]) -> Result<()> {
        let clusters = self.e_core_clusters();
        if ratios.len() != clusters.len() {
            bail!("Expected one ratio per E-core cluster ({}), got {}.", clusters.len(), ratios.len());
        }
        if ratios.iter().any(|r| !(8..=120).contains(r)) {
            bail!("Every E-core cluster needs a ratio between 8 and 120.");
        }
        let top = ratios.iter().copied().max().unwrap_or(0);
        let (table, _) = self.read_e_core_turbo_table()?;
        if table.iter().any(|&r| r > 0 && r != top) {
            let raised: Vec<i32> = table.iter().map(|&r| if r == 0 { 0 } else { top }).collect();
            self.write_e_core_turbo_table(&raised)?;
        }
        for (cl, r) in clusters.iter().zip(ratios) {
            OcMailbox::new(self.drv.clone(), cl[0]).set_core_ratio_ceiling(*r)?;
        }
        Ok(())
    }

    /// Each P-core's own ratio ceiling (mailbox 0x1C on that core), in `p_core_threads` order.
    pub fn read_p_core_ratio_ceilings(&self) -> Vec<Option<i32>> {
        self.p_core_threads().into_iter().map(|t| OcMailbox::new(self.drv.clone(), t).read_core_ratio_ceiling()).collect()
    }

    /// Per physical core ratios, the way MSI Dragon Power's "Per Core Ratio" works: each P-core's
    /// own ratio ceiling (mailbox 0x1D run on that core). One value per P-core, in
    /// `p_core_threads` order. The turbo table still bounds the result, so its entries are raised
    /// to the highest per-core value first if they are below it.
    pub fn write_p_core_ratio_ceilings(&self, ratios: &[i32]) -> Result<()> {
        let threads = self.p_core_threads();
        if ratios.len() != threads.len() {
            bail!("Expected one ratio per P-core ({}), got {}.", threads.len(), ratios.len());
        }
        if ratios.iter().any(|r| !(8..=120).contains(r)) {
            bail!("Every P-core needs a ratio between 8 and 120.");
        }
        let top = ratios.iter().copied().max().unwrap_or(0);
        // The table, domain 0 and every core's ceiling go to the highest per-core value first
        // (always: skipping it when the table already matched left a lower domain-0 ratio, set by
        // another tool, capping every core), then each core is set to its own value.
        self.write_p_core_turbo_table(&[top; 8])?;
        for (t, r) in threads.iter().zip(ratios) {
            OcMailbox::new(self.drv.clone(), *t).set_core_ratio_ceiling(*r)?;
        }
        Ok(())
    }

    pub fn write_p_core_turbo_table(&self, ratios: &[i32]) -> Result<()> {
        let v = Self::encode_ratios(ratios)?;
        let before = self.read_or_throw(Self::MSR_TURBO_RATIO_LIMIT, self.first_p_thread)?;
        // Captured before anything is written, so a failure part-way through can put all three
        // places back, not just the table.
        let domain_before = self.mailbox.read_domain(OcMailbox::DOMAIN_CORE).ok();
        let ceilings_before: Vec<(i32, Option<i32>)> = self
            .p_core_threads()
            .into_iter()
            .map(|t| (t, OcMailbox::new(self.drv.clone(), t).read_core_ratio_ceiling()))
            .collect();
        self.write_verified(Self::MSR_TURBO_RATIO_LIMIT, v, self.first_p_thread)?;
        // The OC mailbox carries its own ceiling for the core domain; raise it when the table goes above it.
        let wanted = ratios.iter().copied().max().unwrap_or(0);
        let ceiling = (|| -> Result<()> {
            let mut d = self.mailbox.read_domain(OcMailbox::DOMAIN_CORE)?;
            // MSI Dragon Power sets the domain-0 ratio and the core ratio ceiling (mailbox
            // 0x1C/0x1D) to exactly the requested maximum, down as well as up; a register snapshot
            // of its "CPU Ratio 50" showed 0x1AD, domain 0 and 0x1C all at x50. Do the same so
            // the three never disagree.
            if d.max_ratio > 0 && d.max_ratio != wanted {
                d.max_ratio = wanted;
                self.mailbox.write_domain(OcMailbox::DOMAIN_CORE, &d)?;
            }
            // The ceiling is per core (it answers for the core the command runs on: P-core 0 read
            // x54 while P-cores 1-5 read x55), so set it on every P-core, not just the first.
            for t in self.p_core_threads() {
                OcMailbox::new(self.drv.clone(), t).set_core_ratio_ceiling(wanted)?;
            }
            Ok(())
        })();
        if let Err(ex) = ceiling {
            let mut restored = self.drv.write_msr(Self::MSR_TURBO_RATIO_LIMIT, before, self.first_p_thread)
                && self.drv.read_msr(Self::MSR_TURBO_RATIO_LIMIT, self.first_p_thread).is_some_and(|a| a == before);
            if let Some(d) = &domain_before {
                restored &= self.mailbox.write_domain(OcMailbox::DOMAIN_CORE, d).is_ok();
            }
            for (t, c) in &ceilings_before {
                if let Some(c) = c.filter(|&c| c > 0) {
                    restored &= OcMailbox::new(self.drv.clone(), *t).set_core_ratio_ceiling(c).is_ok();
                }
            }
            bail!(
                "OC mailbox ceiling could not be verified: {ex}{}",
                if restored {
                    " Previous turbo table, core ratio and per-core limits restored."
                } else {
                    " Restoring the previous ratios could not be verified everywhere."
                }
            );
        }
        Ok(())
    }

    pub fn write_e_core_turbo_table(&self, ratios: &[i32]) -> Result<()> {
        if self.e_core_count == 0 {
            return Ok(());
        }
        let (current, _) = self.read_e_core_turbo_table()?;
        if ratios.len() != 8 || !current.iter().any(|&r| r > 0) {
            bail!("No usable eight-entry E-core table.");
        }
        let mut v = 0u64;
        for i in 0..8 {
            // Firmware can expose one populated group and seven zero entries.
            // Keep inactive entries zero instead of inventing additional turbo groups.
            let invalid = if current[i] == 0 { ratios[i] != 0 } else { ratios[i] < 8 || ratios[i] > 120 };
            if invalid {
                bail!("Active E-core groups require ratios 8–120; inactive groups must remain zero.");
            }
            v |= (ratios[i] as u64) << (8 * i);
        }
        // MSR 0x650 is per E-core cluster, not per package: written on the first E-core only, the
        // i5-14600KF's second four-core cluster stayed at x40 while the first went to x41. Write
        // it, and the E-core ratio ceiling below, on every E-core thread; first one first, so a
        // rejected value fails before anything else has changed.
        let mut e_threads: Vec<i32> = self.logical_cpus.iter().filter(|c| !c.is_p_core).map(|c| c.index).collect();
        e_threads.sort_by_key(|&t| t != self.first_e_thread);
        let wanted = ratios.iter().copied().max().unwrap_or(0);
        let ceilings_before: Vec<(i32, Option<i32>)> = e_threads
            .iter()
            .map(|&t| (t, OcMailbox::new(self.drv.clone(), t).read_core_ratio_ceiling()))
            .collect();
        let mut written: Vec<(i32, u64)> = Vec::new();
        let r: Result<()> = (|| {
            for &t in &e_threads {
                let before = self.read_or_throw(Self::MSR_ATOM_TURBO_RATIO_LIMIT, t)?;
                written.push((t, before));
                self.write_verified(Self::MSR_ATOM_TURBO_RATIO_LIMIT, v, t)?;
            }
            // The core ratio ceiling (mailbox 0x1C/0x1D) is per core type: read on an E-core it
            // returns the E-core ceiling (0x2828, x40 on the i5-14600KF). Set it from each E-core
            // so ratios above the fused x40 are not capped there, as for the P-cores.
            for &t in &e_threads {
                OcMailbox::new(self.drv.clone(), t)
                    .set_core_ratio_ceiling(wanted)
                    .map_err(|ex| anyhow::anyhow!("E-core ratio ceiling could not be verified: {ex}"))?;
            }
            Ok(())
        })();
        if let Err(ex) = r {
            let mut restored = true;
            for &(t, before) in &written {
                restored &= self.drv.write_msr(Self::MSR_ATOM_TURBO_RATIO_LIMIT, before, t)
                    && self.drv.read_msr(Self::MSR_ATOM_TURBO_RATIO_LIMIT, t).is_some_and(|a| a == before);
            }
            for (t, c) in &ceilings_before {
                if let Some(c) = c.filter(|&c| c > 0) {
                    restored &= OcMailbox::new(self.drv.clone(), *t).set_core_ratio_ceiling(c).is_ok();
                }
            }
            bail!(
                "{ex}{}",
                if restored { " Previous E-core table restored." } else { " E-core table restore could not be verified." }
            );
        }
        Ok(())
    }

    /// The all-core P ratio: the lowest entry of the turbo table (largest active-core group).
    pub fn read_p_core_all_core_ratio(&self) -> Result<i32> {
        let (ratios, _) = self.read_p_core_turbo_table()?;
        Ok(ratios.into_iter().filter(|&r| r > 0).min().unwrap_or(self.base_ratio))
    }

    pub fn read_e_core_all_core_ratio(&self) -> Result<i32> {
        if self.e_core_count == 0 {
            // E-cores disabled in the BIOS: MSR 0x650 still holds the E-core table and reads
            // from a P-core (x40 on the Z790 MPOWER), which is what MSI Dragon Power shows.
            let r = self.read_or_throw(Self::MSR_ATOM_TURBO_RATIO_LIMIT, self.first_p_thread)?;
            return Ok((0..8).map(|i| ((r >> (8 * i)) & 0xFF) as i32).filter(|&x| x > 0).min().unwrap_or(0));
        }
        let (ratios, _) = self.read_e_core_turbo_table()?;
        Ok(ratios.into_iter().filter(|&r| r > 0).min().unwrap_or(0))
    }

    pub fn write_p_core_all_core_ratio(&self, ratio: i32) -> Result<()> {
        self.write_p_core_turbo_table(&[ratio; 8])
    }

    pub fn write_e_core_all_core_ratio(&self, ratio: i32) -> Result<()> {
        if !(8..=120).contains(&ratio) {
            bail!("Specified argument was out of the range of valid values. (Parameter 'ratio')");
        }
        if self.e_core_count == 0 {
            // Same table through a P-core, populated groups only. Kept for when the E-cores are
            // enabled again; with them disabled it changes no running clock.
            let current = self.read_or_throw(Self::MSR_ATOM_TURBO_RATIO_LIMIT, self.first_p_thread)?;
            if current == 0 {
                bail!("No usable eight-entry E-core table.");
            }
            let mut v = 0u64;
            for i in 0..8 {
                if (current >> (8 * i)) & 0xFF != 0 {
                    v |= (ratio as u64) << (8 * i);
                }
            }
            return self.write_verified(Self::MSR_ATOM_TURBO_RATIO_LIMIT, v, self.first_p_thread);
        }
        let (current, _) = self.read_e_core_turbo_table()?;
        let table: Vec<i32> = current.iter().map(|&r| if r == 0 { 0 } else { ratio }).collect();
        self.write_e_core_turbo_table(&table)
    }

    // ------------------------------------------------------------- ring
    /// Programmed ring limit `(max, min)`. Some boards (including the tested MSI Z790) expose an
    /// additional mailbox ceiling. Only include it when the mailbox reports a nonzero ratio.
    /// This is a configured limit, not a measurement of the running ring clock.
    pub fn read_ring_ratio(&self) -> Result<(i32, i32)> {
        let v = self.read_or_throw(Self::MSR_UNCORE_RATIO_LIMIT, self.first_p_thread)?;
        let mut max = (v & 0x7F) as i32;
        let min = ((v >> 8) & 0x7F) as i32;
        if let Ok(d) = self.mailbox.read_domain(OcMailbox::DOMAIN_RING) {
            let mb = d.max_ratio;
            if mb > 0 {
                max = max.min(mb);
            }
        }
        Ok((max, min))
    }

    pub fn write_ring_ratio(&self, max: i32, min: Option<i32>) -> Result<()> {
        if !(4..=120).contains(&max) || min.is_some_and(|m| !(0..=120).contains(&m)) || min.is_some_and(|m| m > max) {
            bail!("Invalid ring ratio range.");
        }
        let mut v = self.read_or_throw(Self::MSR_UNCORE_RATIO_LIMIT, self.first_p_thread)?;
        let before = v;
        let cur_min = ((v >> 8) & 0x7F) as i32;
        v = (v & !0x7Fu64) | (max & 0x7F) as u64;
        let new_min = min.unwrap_or(cur_min.min(max)); // never leave min above max
        v = (v & !(0x7Fu64 << 8)) | (((new_min & 0x7F) as u64) << 8);
        self.write_verified(Self::MSR_UNCORE_RATIO_LIMIT, v, self.first_p_thread)?;
        let mailbox = (|| -> Result<()> {
            let mut d = self.mailbox.read_domain(OcMailbox::DOMAIN_RING)?;
            // ASUS can report a completely empty domain here;
            // do not turn a verified MSR write into an unrelated failed VF write.
            // Preserve the mailbox path on boards that expose it (tested on MSI).
            if OcMailbox::encode(&d) != 0 {
                d.max_ratio = max;
                self.mailbox.write_domain(OcMailbox::DOMAIN_RING, &d)?;
            }
            Ok(())
        })();
        if let Err(ex) = mailbox {
            let restored = self.drv.write_msr(Self::MSR_UNCORE_RATIO_LIMIT, before, self.first_p_thread)
                && self.drv.read_msr(Self::MSR_UNCORE_RATIO_LIMIT, self.first_p_thread).is_some_and(|a| a == before);
            bail!(
                "Ring ratio mailbox failed ({ex}). {}",
                if restored { "Previous ring register restored." } else { "Ring-register restore could not be verified." }
            );
        }
        Ok(())
    }

    pub fn read_current_ring_ratio(&self) -> i32 {
        match self.drv.read_msr(Self::MSR_UNCORE_PERF_STATUS, self.first_p_thread) {
            Some(v) => (v & 0x7F) as i32,
            None => 0,
        }
    }

    // ------------------------------------------------------------- live status
    /// Current ratio (bits 15:8) and VID in volts (bits 47:32, 1/8192 V) on a logical CPU.
    /// Returns `(ratio, vid)`.
    pub fn read_perf_status(&self, cpu: i32) -> Result<(i32, f64)> {
        let v = self.read_or_throw(Self::MSR_IA32_PERF_STATUS, cpu)?;
        Ok((((v >> 8) & 0xFF) as i32, ((v >> 32) & 0xFFFF) as f64 / 8192.0))
    }

    /// Highest current ratio across P-core threads.
    pub fn read_max_current_p_ratio(&self) -> i32 {
        let mut max = 0;
        for c in self.logical_cpus.iter().filter(|c| c.is_p_core) {
            if let Some(v) = self.drv.read_msr(Self::MSR_IA32_PERF_STATUS, c.index) {
                max = max.max(((v >> 8) & 0xFF) as i32);
            }
        }
        max
    }

    pub fn read_max_current_e_ratio(&self) -> Option<i32> {
        let mut max: Option<i32> = None;
        for c in self.logical_cpus.iter().filter(|c| !c.is_p_core) {
            if let Some(v) = self.drv.read_msr(Self::MSR_IA32_PERF_STATUS, c.index) {
                max = Some(max.unwrap_or(0).max(((v >> 8) & 0xFF) as i32));
            }
        }
        max
    }

    pub fn read_package_temperature(&self) -> Result<i32> {
        let v = self.read_or_throw(Self::MSR_IA32_PACKAGE_THERM_STATUS, self.first_p_thread)?;
        let delta = ((v >> 16) & 0x7F) as i32;
        Ok(self.tj_max - delta)
    }

    pub fn read_core_temperature(&self, cpu: i32) -> Result<i32> {
        let v = self.read_or_throw(Self::MSR_IA32_THERM_STATUS, cpu)?;
        Ok(self.tj_max - ((v >> 16) & 0x7F) as i32)
    }

    // ------------------------------------------------------------- power limits
    fn power_unit(&self) -> Result<f64> {
        let u = self.read_or_throw(Self::MSR_RAPL_POWER_UNIT, self.first_p_thread)?;
        Ok(1.0 / (1i32 << (u & 0xF)) as f64)
    }

    pub fn read_package_power_limits(&self) -> Result<PackagePowerLimits> {
        let unit = self.power_unit()?;
        let v = self.read_or_throw(Self::MSR_PKG_POWER_LIMIT, self.first_p_thread)?;
        let pl1 = (v & 0x7FFF) as f64 * unit;
        let pl2 = ((v >> 32) & 0x7FFF) as f64 * unit;
        Ok(PackagePowerLimits {
            pl1,
            pl2,
            locked: ((v >> 63) & 1) == 1,
            pl1_enabled: ((v >> 15) & 1) == 1,
            pl2_enabled: ((v >> 47) & 1) == 1,
        })
    }

    pub fn write_package_power_limits(&self, pl1: Option<f64>, pl2: Option<f64>) -> Result<()> {
        let unit = self.power_unit()?;
        let mut v = self.read_or_throw(Self::MSR_PKG_POWER_LIMIT, self.first_p_thread)?;
        if ((v >> 63) & 1) == 1 {
            bail!("Package power limits are locked by the BIOS (MSR 0x610 bit 63).");
        }
        if let Some(p1) = pl1 {
            let raw = ((p1 / unit).round_ties_even() as u64) & 0x7FFF;
            v = (v & !0x7FFFu64) | raw | (1u64 << 15);
        }
        if let Some(p2) = pl2 {
            let raw = ((p2 / unit).round_ties_even() as u64) & 0x7FFF;
            v = (v & !(0x7FFFu64 << 32)) | (raw << 32) | (1u64 << 47);
        }
        self.write_verified(Self::MSR_PKG_POWER_LIMIT, v, self.first_p_thread)
    }

    pub fn read_package_energy_joules(&self) -> Result<f64> {
        let u = self.read_or_throw(Self::MSR_RAPL_POWER_UNIT, self.first_p_thread)?;
        let energy_unit = 1.0 / (1i32 << ((u >> 8) & 0x1F)) as f64;
        let e = self.read_or_throw(Self::MSR_PKG_ENERGY_STATUS, self.first_p_thread)?;
        Ok((e & 0xFFFF_FFFF) as f64 * energy_unit)
    }
}

/// Test doubles shared by the Intel CPU and OC mailbox tests (the C# test project's `FakeDriver`).
#[cfg(test)]
pub(crate) mod fakes {
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    use anyhow::{Result, bail};

    use crate::hw::KernelDriver;
    use crate::hw::native::lock;

    pub struct FakeState {
        pub ignore_voltage: bool,
        pub ignore_ratio: bool,
        pub fail_mailbox_response: bool,
        pub registers: HashMap<u32, u64>,
        pub domains: HashMap<i32, u32>,
        /// `(reg, value, cpu)` for every WRMSR.
        pub writes: Vec<(u32, u64, i32)>,
        response: u64,
        response_reads: i32,
        mailbox_written: bool,
    }

    pub struct FakeDriver {
        state: Mutex<FakeState>,
    }

    impl FakeDriver {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeState {
                    ignore_voltage: false,
                    ignore_ratio: false,
                    fail_mailbox_response: false,
                    registers: HashMap::from([(0x650, 0x2B2B_2B2B_2B2B_2B2B)]),
                    domains: HashMap::new(),
                    writes: Vec::new(),
                    response: 0,
                    response_reads: 0,
                    mailbox_written: false,
                }),
            })
        }

        pub fn state(&self) -> MutexGuard<'_, FakeState> {
            lock(&self.state)
        }
    }

    impl KernelDriver for FakeDriver {
        fn name(&self) -> String {
            "simulation".into()
        }
        fn is_open(&self) -> bool {
            true
        }
        fn read_msr(&self, index: u32, _cpu: i32) -> Option<u64> {
            let mut s = self.state();
            let value = if index == 0x150 { s.response } else { s.registers.get(&index).copied().unwrap_or(0) };
            if index == 0x150 && s.mailbox_written && s.fail_mailbox_response {
                s.response_reads += 1;
                if s.response_reads == 2 {
                    return None;
                }
            }
            Some(value)
        }
        fn write_msr(&self, index: u32, value: u64, cpu: i32) -> bool {
            let mut s = self.state();
            s.writes.push((index, value, cpu));
            if index == 0x150 {
                let command = ((value >> 32) & 255) as i32;
                let domain = ((value >> 40) & 255) as i32;
                if command == 0x11 && !s.ignore_voltage {
                    s.domains.insert(domain, value as u32);
                }
                s.response = if command == 0x10 { s.domains.get(&domain).copied().unwrap_or(0) as u64 } else { 0 };
                s.mailbox_written = true;
                s.response_reads = 0;
            } else if !(s.ignore_ratio && index == 0x1AD) {
                s.registers.insert(index, value);
            }
            true
        }
        fn read_io_port_byte(&self, _port: u16) -> Result<u8> {
            bail!("not supported")
        }
        fn read_io_port_word(&self, _port: u16) -> Result<u16> {
            bail!("not supported")
        }
        fn read_io_port_dword(&self, _port: u16) -> Result<u32> {
            bail!("not supported")
        }
        fn write_io_port_byte(&self, _port: u16, _value: u8) -> Result<()> {
            bail!("not supported")
        }
        fn write_io_port_word(&self, _port: u16, _value: u16) -> Result<()> {
            bail!("not supported")
        }
        fn write_io_port_dword(&self, _port: u16, _value: u32) -> Result<()> {
            bail!("not supported")
        }
        fn read_pci_config(&self, _bus: u8, _device: u8, _function: u8, _offset: u16) -> Option<u32> {
            panic!("not supported")
        }
        fn write_pci_config(&self, _bus: u8, _device: u8, _function: u8, _offset: u16, _value: u32) -> bool {
            panic!("not supported")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::FakeDriver;
    use super::*;
    use crate::hw::oc_mailbox::VfDomainSettings;

    fn cpu(d: &Arc<FakeDriver>) -> IntelCpu {
        IntelCpu::with_topology(d.clone(), vec![LogicalCpu::new(0, 0, 0, true), LogicalCpu::new(1, 2, 1, false)])
            .unwrap()
    }

    #[test]
    fn e_core_write_uses_e_thread_and_verifies_it() {
        let d = FakeDriver::new();
        let c = cpu(&d);
        c.write_e_core_all_core_ratio(43).unwrap();
        let last = d.state().writes.iter().rev().find(|w| w.0 == 0x650).copied().unwrap();
        assert!(last.2 == 1, "Used P thread for E-core write");
    }

    #[test]
    fn ignored_p_core_table_fails() {
        let d = FakeDriver::new();
        d.state().ignore_ratio = true;
        let c = cpu(&d);
        assert!(c.write_p_core_all_core_ratio(54).is_err());
    }

    #[test]
    fn asus_single_entry_e_core_table_preserves_unused_entries() {
        let d = FakeDriver::new();
        d.state().registers.insert(0x650, 44);
        cpu(&d).write_e_core_all_core_ratio(42).unwrap();
        assert!(d.state().registers[&0x650] == 42, "Filled unused E-core entries");
        cpu(&d).write_e_core_turbo_table(&[44, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(d.state().registers[&0x650] == 44, "Sparse table restore failed");
        assert!(cpu(&d).write_e_core_turbo_table(&[44; 8]).is_err());
    }

    #[test]
    fn populated_msi_style_e_core_groups_all_update() {
        let d = FakeDriver::new();
        cpu(&d).write_e_core_all_core_ratio(44).unwrap();
        assert!(d.state().registers[&0x650] == 0x2C2C_2C2C_2C2C_2C2C, "Dropped populated E-core groups");
    }

    #[test]
    fn rejected_mailbox_ceiling_restores_p_core_table() {
        let d = FakeDriver::new();
        {
            let mut s = d.state();
            s.ignore_voltage = true;
            s.domains.insert(0, 50);
            s.registers.insert(0x1AD, 0x3232_3232_3232_3232);
        }
        let c = cpu(&d);
        assert!(c.write_p_core_all_core_ratio(55).is_err());
        assert!(d.state().registers[&0x1AD] == 0x3232_3232_3232_3232, "Partial table write not restored");
    }

    #[test]
    fn out_of_range_turbo_table_rejected_before_hardware_writes() {
        let d = FakeDriver::new();
        let c = cpu(&d);
        d.state().writes.clear();
        assert!(c.write_p_core_all_core_ratio(256).is_err());
        assert!(d.state().writes.is_empty(), "Invalid request wrote hardware");
    }

    #[test]
    fn rejected_ring_mailbox_restores_the_ring_register() {
        let d = FakeDriver::new();
        {
            let mut s = d.state();
            s.ignore_voltage = true;
            s.registers.insert(0x620, 0x0832);
            s.domains.insert(OcMailbox::DOMAIN_RING, 50);
        }
        assert!(cpu(&d).write_ring_ratio(49, None).is_err());
        assert!(d.state().registers[&0x620] == 0x0832, "Partial ring change not restored");
    }

    #[test]
    fn empty_ring_mailbox_does_not_reject_the_architectural_ratio_control() {
        let d = FakeDriver::new();
        {
            let mut s = d.state();
            s.ignore_voltage = true;
            s.registers.insert(0x620, 0x0832);
        }
        cpu(&d).write_ring_ratio(43, None).unwrap();
        let s = d.state();
        assert!(s.registers[&0x620] == 0x082B, "Ring register not retained");
        assert!(
            !s.writes.iter().any(|w| w.0 == 0x150 && ((w.1 >> 32) & 255) == 0x11),
            "Wrote an empty mailbox domain"
        );
    }

    #[test]
    fn msi_style_ring_mailbox_preserves_voltage_and_updates_both_ceilings() {
        let d = FakeDriver::new();
        d.state().registers.insert(0x620, 0x0832);
        let settings = VfDomainSettings { max_ratio: 50, target_volts: 1.3, override_mode: true, offset_volts: -0.05 };
        let original = OcMailbox::encode(&settings);
        d.state().domains.insert(OcMailbox::DOMAIN_RING, original);
        cpu(&d).write_ring_ratio(49, None).unwrap();
        let s = d.state();
        assert!(s.registers[&0x620] == 0x0831, "MSR ceiling not updated");
        assert!(
            s.domains[&OcMailbox::DOMAIN_RING] == ((original & !255u32) | 49),
            "Mailbox ceiling or voltage changed incorrectly"
        );
    }

    #[test]
    fn invalid_e_core_count_map_does_not_invent_255_active_cores() {
        let d = FakeDriver::new();
        d.state().registers.insert(0x651, u64::MAX);
        assert!(cpu(&d).read_e_core_turbo_table().unwrap().1.iter().all(|&c| c == 0), "Invalid active core count");
    }

    #[test]
    fn ring_mailbox_with_voltage_but_automatic_ratio_still_receives_its_ceiling() {
        let d = FakeDriver::new();
        d.state().registers.insert(0x620, 0x0832);
        let original = OcMailbox::encode(&VfDomainSettings { target_volts: 1.3, override_mode: true, ..Default::default() });
        d.state().domains.insert(OcMailbox::DOMAIN_RING, original);
        cpu(&d).write_ring_ratio(49, None).unwrap();
        assert!(d.state().domains[&OcMailbox::DOMAIN_RING] == (original | 49), "Populated mailbox was skipped");
    }
}
