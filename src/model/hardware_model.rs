//! Owns every hardware object and exposes the tunables as a flat list of [`Setting`]s that the
//! UI renders. All board-independent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::setting::{Setting, SettingGroup};
use crate::hw::amd_cpu::{AmdCore, AmdCpu};
use crate::hw::asus_board_control::{AsusBoardControl, SvidState};
use crate::hw::bclk_controller::BclkController;
use crate::hw::bclk_meter::BclkMeter;
use crate::hw::core_page_probe::CorePageVerificationResult;
use crate::hw::ddr5_dimm::Ddr5Dimm;
use crate::hw::ec_clock_gen::EcClockGen;
use crate::hw::ec_mailbox::EcMailbox;
use crate::hw::hypervisor_restrictions;
use crate::hw::intel_cpu::IntelCpu;
use crate::hw::msi_core_voltage::MsiCoreVoltage;
use crate::hw::native::{self, lock};
use crate::hw::oc_mailbox::OcMailbox;
use crate::hw::smbios::SmbiosInfo;
use crate::hw::smbus_i801::SmbusI801;
use crate::hw::smbus_piix4::SmbusPiix4;
use crate::hw::super_io::SuperIo;
use crate::hw::vdd2_rail::Vdd2Rail;
use crate::hw::winring0::WinRing0Driver;
use crate::hw::{KernelDriver, Smbus};

mod core_voltage_recovery;

/// Live readings shown in the header.
#[derive(Clone, Debug, Default)]
pub struct LiveStatus {
    pub package_temp_c: Option<i32>,
    pub core_mhz: Option<f64>,
    pub core_ratio: Option<i32>,
    pub e_core_ratio: Option<i32>,
    pub core_vid: Option<f64>,
    pub ring_ratio: Option<i32>,
    pub bclk_mhz: Option<f64>,
    pub package_watts: Option<f64>,
    /// Real Vcore at the VRM output, when a Super I/O can report it.
    pub vcore_vrm: Option<f64>,
}

pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// State the setting closures share with the model. It never holds the settings themselves, so
/// there is no reference cycle and dropping the model releases every hardware object.
struct Ctx {
    sinks: Mutex<Vec<LogSink>>,
    last_bclk: Mutex<Option<f64>>,
    // What was last written here, for the rows whose firmware has no read-back path.
    last_written: Mutex<HashMap<String, f64>>,
    last_co: Mutex<HashMap<i32, i32>>,
    table_read_at: Mutex<Option<Instant>>,
    bclk: OnceLock<Arc<BclkMeter>>,
    cpu: OnceLock<Arc<IntelCpu>>,
    amd: OnceLock<Arc<AmdCpu>>,
}

impl Ctx {
    fn emit(&self, msg: &str) {
        let sinks: Vec<LogSink> = lock(&self.sinks).clone();
        for s in sinks {
            s(msg);
        }
    }

    fn base_ratio(&self) -> i32 {
        self.cpu
            .get()
            .map(|c| c.base_ratio())
            .or_else(|| self.amd.get().map(|a| a.base_ratio()))
            .unwrap_or(0)
    }

    /// Blocking measurement; call from a background thread.
    ///
    /// Counts real core clocks (CPU_CLK_UNHALTED.CORE) against the ACPI timer. The older
    /// TSC-against-ACPI-timer method is only a fallback because it cannot see a BCLK change made
    /// after boot: the TSC runs from a fixed crystal here, so it keeps reporting the boot-time
    /// value however far BCLK is moved. Comparing against a vendor tool's live readback on a
    /// Z790MPOWER at 101.00 MHz: cycle counting gives 101.02, the TSC method still says 99.84.
    fn measure_bclk(&self) -> Option<f64> {
        let bclk = self.bclk.get().filter(|b| b.is_available())?;
        let base_ratio = self.base_ratio();
        if base_ratio == 0 {
            return None;
        }
        let r: Result<f64> = (|| {
            // Ryzen's TSC ticks at base clock x the P0 multiplier. The cycle-counter method below
            // reads Intel-only MSRs, so it is not tried there.
            if self.amd.get().is_some() {
                return bclk.measure_bclk_mhz(base_ratio, 60);
            }
            let core = self.cpu.get().map(|c| c.first_p_thread()).unwrap_or(0);
            match bclk.measure_bclk_from_cycles(core, 150)? {
                Some(v) => Ok(v),
                None => bclk.measure_bclk_mhz(base_ratio, 60),
            }
        })();
        match r {
            Ok(v) => {
                *lock(&self.last_bclk) = Some(v);
                Some(v)
            }
            Err(e) => {
                self.emit(&format!("BCLK measure failed: {e}"));
                None
            }
        }
    }

    /// Refreshes the SMU power table at most every 300 ms; three rows read from it in a row.
    fn table_fresh(&self) -> bool {
        let Some(amd) = self.amd.get() else { return false };
        let smu = amd.smu();
        if !smu.table_readable() {
            return false;
        }
        let mut at = lock(&self.table_read_at);
        if at.is_some_and(|t| t.elapsed().as_millis() < 300) && smu.table().is_some() {
            return true;
        }
        let ok = smu.refresh_table();
        if ok {
            *at = Some(Instant::now());
        }
        ok || smu.table().is_some()
    }
}

/// Owns every hardware object and exposes the tunables as a flat list of settings.
///
/// Fields are declared in the order the .NET model disposed them: the rows (and the closures
/// that hold hardware references) go first, the kernel driver last.
pub struct HardwareModel {
    settings: Mutex<Vec<Setting>>,
    asus_control: OnceLock<Arc<AsusBoardControl>>,
    core_voltage: OnceLock<Arc<MsiCoreVoltage>>,
    vdd2: OnceLock<Arc<Vdd2Rail>>,
    bclk_control: OnceLock<Arc<BclkController>>,
    super_io: OnceLock<Arc<SuperIo>>,
    dimms: Mutex<Vec<Arc<Ddr5Dimm>>>,
    smbus: OnceLock<Arc<dyn Smbus>>,
    ctx: Arc<Ctx>,
    driver: OnceLock<Arc<dyn KernelDriver>>,

    smbios: SmbiosInfo,
    super_io_status: Mutex<String>,
    core_voltage_status: Mutex<String>,
    driver_status: Mutex<String>,
    smbus_status: Mutex<String>,
    mailbox_available: AtomicBool,
    voltage_access_restriction: Mutex<Option<String>>,
    /// True when the BIOS left the core domain in mailbox Override mode (fixed VID).
    bios_core_override: AtomicBool,
    oc_locked: AtomicBool,
    power_limits_locked: AtomicBool,

    // AMD
    smu_available: AtomicBool,
    smu_status: Mutex<String>,
    /// IsOverclockable flags from the SMU: bit 0 OC allowed, bit 1 power limits, bit 2 PBO.
    oc_capabilities: Mutex<Option<u32>>,
    /// The CPU's fused stock PPT / TDC / EDC, what "0" restores on those rows.
    stock_limits: Mutex<(Option<f64>, Option<f64>, Option<f64>)>,

    energy: Mutex<(f64, Option<Instant>)>,
    /// Set when an apply was measured not to reach the rail.
    core_voltage_ignored: AtomicBool,

    core_voltage_initialization: Mutex<Option<CorePageVerificationResult>>,
}

impl Default for HardwareModel {
    fn default() -> Self {
        Self::new()
    }
}

fn msi_board(smbios: &SmbiosInfo) -> bool {
    contains_ci(&smbios.board_manufacturer, "Micro-Star") || contains_ci(&smbios.system_manufacturer, "Micro-Star")
}

pub(crate) fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn is_amd_vendor() -> bool {
    let (_, ebx, ecx, edx) = native::cpuid(0, 0);
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&ebx.to_le_bytes());
    v.extend_from_slice(&edx.to_le_bytes());
    v.extend_from_slice(&ecx.to_le_bytes());
    let vendor = String::from_utf8_lossy(&v);
    vendor == "AuthenticAMD" || vendor == "HygonGenuine"
}

fn opt0(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "n/a".into())
}

fn optq(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "?".into())
}

impl HardwareModel {
    pub fn new() -> Self {
        Self::with_smbios(SmbiosInfo::read())
    }

    fn with_smbios(smbios: SmbiosInfo) -> Self {
        Self {
            settings: Mutex::new(Vec::new()),
            asus_control: OnceLock::new(),
            core_voltage: OnceLock::new(),
            vdd2: OnceLock::new(),
            bclk_control: OnceLock::new(),
            super_io: OnceLock::new(),
            dimms: Mutex::new(Vec::new()),
            smbus: OnceLock::new(),
            ctx: Arc::new(Ctx {
                sinks: Mutex::new(Vec::new()),
                last_bclk: Mutex::new(None),
                last_written: Mutex::new(HashMap::new()),
                last_co: Mutex::new(HashMap::new()),
                table_read_at: Mutex::new(None),
                bclk: OnceLock::new(),
                cpu: OnceLock::new(),
                amd: OnceLock::new(),
            }),
            driver: OnceLock::new(),
            smbios,
            super_io_status: Mutex::new("not probed".into()),
            core_voltage_status: Mutex::new("not probed".into()),
            driver_status: Mutex::new("not loaded".into()),
            smbus_status: Mutex::new("not probed".into()),
            mailbox_available: AtomicBool::new(false),
            voltage_access_restriction: Mutex::new(None),
            bios_core_override: AtomicBool::new(false),
            oc_locked: AtomicBool::new(false),
            power_limits_locked: AtomicBool::new(false),
            smu_available: AtomicBool::new(false),
            smu_status: Mutex::new("not probed".into()),
            oc_capabilities: Mutex::new(None),
            stock_limits: Mutex::new((None, None, None)),
            energy: Mutex::new((0.0, None)),
            core_voltage_ignored: AtomicBool::new(false),
            core_voltage_initialization: Mutex::new(None),
        }
    }

    /// Subscribes to the log (the .NET `Log` event).
    pub fn on_log(&self, sink: impl Fn(&str) + Send + Sync + 'static) {
        lock(&self.ctx.sinks).push(Arc::new(sink));
    }

    fn emit(&self, msg: &str) {
        self.ctx.emit(msg);
    }

    fn emitter(&self) -> impl Fn(&str) + Send + Sync + Clone + 'static {
        let ctx = self.ctx.clone();
        move |m: &str| ctx.emit(m)
    }

    // ------------------------------------------------------------------ accessors
    pub fn driver(&self) -> Option<Arc<dyn KernelDriver>> {
        self.driver.get().cloned()
    }
    /// Intel LGA1700 path. `None` on AMD.
    pub fn cpu(&self) -> Option<Arc<IntelCpu>> {
        self.ctx.cpu.get().cloned()
    }
    /// AMD Zen path. `None` on Intel.
    pub fn amd(&self) -> Option<Arc<AmdCpu>> {
        self.ctx.amd.get().cloned()
    }
    pub fn is_amd(&self) -> bool {
        self.ctx.amd.get().is_some()
    }
    pub fn smbios(&self) -> &SmbiosInfo {
        &self.smbios
    }
    pub fn bclk(&self) -> Option<Arc<BclkMeter>> {
        self.ctx.bclk.get().cloned()
    }
    pub fn smbus(&self) -> Option<Arc<dyn Smbus>> {
        self.smbus.get().cloned()
    }
    pub fn super_io(&self) -> Option<Arc<SuperIo>> {
        self.super_io.get().cloned()
    }
    pub fn super_io_status(&self) -> String {
        lock(&self.super_io_status).clone()
    }
    /// Base-clock control, when the board's clock generator is reachable.
    pub fn bclk_control(&self) -> Option<Arc<BclkController>> {
        self.bclk_control.get().cloned()
    }
    pub fn asus_control(&self) -> Option<Arc<AsusBoardControl>> {
        self.asus_control.get().cloned()
    }
    /// CPU VDD2 control, when the board's regulator is reachable.
    pub fn vdd2(&self) -> Option<Arc<Vdd2Rail>> {
        self.vdd2.get().cloned()
    }
    /// MSI board-side CPU-core regulator control. The Intel mailbox alone is insufficient on these boards.
    pub fn core_voltage(&self) -> Option<Arc<MsiCoreVoltage>> {
        self.core_voltage.get().cloned()
    }
    pub fn core_voltage_status(&self) -> String {
        lock(&self.core_voltage_status).clone()
    }
    pub fn dimms(&self) -> Vec<Arc<Ddr5Dimm>> {
        lock(&self.dimms).clone()
    }
    /// The rows. Do not hold the guard while calling into the model (apply, refresh, …).
    pub fn settings(&self) -> MutexGuard<'_, Vec<Setting>> {
        lock(&self.settings)
    }
    pub fn setting(&self, id: &str) -> Option<Setting> {
        lock(&self.settings).iter().find(|s| s.id.eq_ignore_ascii_case(id)).cloned()
    }
    pub fn driver_status(&self) -> String {
        lock(&self.driver_status).clone()
    }
    pub fn smbus_status(&self) -> String {
        lock(&self.smbus_status).clone()
    }
    pub fn mailbox_available(&self) -> bool {
        self.mailbox_available.load(Ordering::SeqCst)
    }
    pub fn voltage_access_restriction(&self) -> Option<String> {
        lock(&self.voltage_access_restriction).clone()
    }
    pub fn bios_core_override(&self) -> bool {
        self.bios_core_override.load(Ordering::SeqCst)
    }
    pub fn oc_locked(&self) -> bool {
        self.oc_locked.load(Ordering::SeqCst)
    }
    pub fn power_limits_locked(&self) -> bool {
        self.power_limits_locked.load(Ordering::SeqCst)
    }
    pub fn last_bclk(&self) -> Option<f64> {
        *lock(&self.ctx.last_bclk)
    }
    pub fn smu_available(&self) -> bool {
        self.smu_available.load(Ordering::SeqCst)
    }
    pub fn smu_status(&self) -> String {
        lock(&self.smu_status).clone()
    }
    pub fn oc_capabilities(&self) -> Option<u32> {
        *lock(&self.oc_capabilities)
    }
    pub fn pbo_allowed(&self) -> bool {
        match self.oc_capabilities() {
            None => true,
            Some(c) => (c & 0x4) != 0,
        }
    }
    pub fn stock_limits(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        *lock(&self.stock_limits)
    }
    pub fn core_voltage_ignored(&self) -> bool {
        self.core_voltage_ignored.load(Ordering::SeqCst)
    }

    // ---- header text that does not care which vendor is underneath
    pub fn cpu_name(&self) -> String {
        if let Some(c) = self.cpu() {
            return c.brand_string().to_string();
        }
        if let Some(a) = self.amd() {
            return a.brand_string().to_string();
        }
        "No CPU access".into()
    }
    /// Intel shows the generation under the CPU name; on AMD the brand string already says it all.
    pub fn cpu_generation(&self) -> String {
        self.cpu().map(|c| c.generation().to_string()).unwrap_or_default()
    }
    pub fn core_summary(&self) -> String {
        if let Some(c) = self.cpu() {
            let logical = c.logical_cpus().len();
            return if c.e_core_count() > 0 {
                format!("{}P + {}E / {}", c.p_core_count(), c.e_core_count(), logical)
            } else {
                format!("{} / {}", c.p_core_count(), logical)
            };
        }
        if let Some(a) = self.amd() {
            return format!("{} / {}", a.core_count(), a.logical_count());
        }
        String::new()
    }
    pub fn base_ratio(&self) -> i32 {
        self.ctx.base_ratio()
    }
    pub fn microcode_revision(&self) -> u32 {
        self.cpu()
            .map(|c| c.microcode_revision())
            .or_else(|| self.amd().map(|a| a.patch_level()))
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------ initialization
    /// Opens the driver and probes the hardware. Never fails: failures are logged and the affected
    /// rows become unavailable.
    pub fn initialize(&self) {
        match WinRing0Driver::open() {
            Ok(drv) => {
                let status = format!("{} (driver v{:X})", drv.name(), drv.get_driver_version());
                *lock(&self.driver_status) = status.clone();
                let _ = self.driver.set(Arc::new(drv));
                self.emit(&format!("Driver: {status}"));
            }
            Err(e) => {
                *lock(&self.driver_status) = "unavailable".into();
                self.emit(&format!("Driver error: {e}"));
                self.emit("Running in read-only / offline mode.");
            }
        }

        if let Some(driver) = self.driver() {
            if is_amd_vendor() {
                self.initialize_amd(&driver);
            } else {
                self.initialize_intel(&driver);
            }

            match BclkMeter::new(driver.clone()) {
                Ok(b) => {
                    self.emit(&format!("BCLK: {}", b.status()));
                    let _ = self.ctx.bclk.set(Arc::new(b));
                }
                Err(e) => self.emit(&format!("BCLK meter error: {e}")),
            }

            self.initialize_smbus(&driver);
            self.initialize_super_io(&driver);
        }

        if self.cpu().is_some() {
            let (asus, asus_status) = AsusBoardControl::try_create(&self.smbios.board_manufacturer);
            if let Some(a) = asus {
                let _ = self.asus_control.set(Arc::new(a));
            }
            if contains_ci(&self.smbios.board_manufacturer, "ASUS") {
                self.emit(&format!("ASUS control: {asus_status}"));
            }
        }
        self.build_settings();
        self.refresh_all(true);
    }

    fn initialize_smbus(&self, driver: &Arc<dyn KernelDriver>) {
        let (bus, st): (Option<Arc<dyn Smbus>>, String) = if self.is_amd() {
            let (b, s) = SmbusPiix4::try_create(driver.clone());
            (b.map(|b| Arc::new(b) as Arc<dyn Smbus>), s)
        } else {
            let (b, s) = SmbusI801::try_create(driver.clone());
            (b.map(|b| Arc::new(b) as Arc<dyn Smbus>), s)
        };
        *lock(&self.smbus_status) = st.clone();
        self.emit(&format!("SMBus: {st}"));
        let Some(bus) = bus else { return };
        let _ = self.smbus.set(bus.clone());
        let found: Vec<Arc<Ddr5Dimm>> = Ddr5Dimm::probe(bus).into_iter().map(Arc::new).collect();
        if found.is_empty() {
            self.emit(if self.is_amd() {
                "No DDR5 SPD5118 hubs answer on the FCH SMBus port the BIOS left selected. The DIMMs may sit on another port of the FCH mux, which needs an MMIO write this driver cannot do; the memory rows stay hidden."
            } else {
                "No DDR5 SPD5118 hubs found on the SMBus (DDR4 board, or DIMM SMBus segment not routed through the PCH)."
            });
        }
        for d in &found {
            let pmic = if d.has_pmic() {
                format!("at 0x{:02X} ({})", d.pmic_address(), d.pmic_vendor())
            } else {
                "not reachable".into()
            };
            self.emit(&format!("{}: SPD at 0x{:02X}, PMIC {pmic}", d.slot_name(), d.spd_address()));
            if d.has_pmic() {
                self.emit(&format!("{}: {}", d.slot_name(), d.calibration_note()));
            }
        }
        lock(&self.dimms).extend(found);
    }

    fn initialize_super_io(&self, driver: &Arc<dyn KernelDriver>) {
        let (sio, status) = SuperIo::try_create(driver.clone());
        *lock(&self.super_io_status) = status.clone();
        self.emit(&format!("Super I/O: {status}"));
        let Some(sio) = sio else { return };
        let sio = Arc::new(sio);
        let _ = self.super_io.set(sio.clone());

        let rails = sio.read_rails();
        self.emit(&format!(
            "Board rails: {}",
            if rails.is_empty() {
                "none readable".to_string()
            } else {
                rails.iter().map(|r| format!("{} {:.3} V", r.name, r.volts)).collect::<Vec<_>>().join(", ")
            }
        ));

        // The clock generator sits on the EC's own I2C bus, so it is only reachable
        // where that mailbox exists. The clock controller reads its baseline and
        // does not change the clock until a value is applied.
        if !EcMailbox::is_supported(Some(&sio)) {
            return;
        }
        let mailbox = EcMailbox::new(sio.clone(), false);
        if self.cpu().is_some() && msi_board(&self.smbios) {
            // One bounded core-PAGE read/restore, before constructing the target row.
            // No voltage/mode target is changed, and periodic refresh never selects PAGE.
            self.initialize_core_voltage();
        }
        if let (Some(cpu), Some(meter)) = (self.cpu(), self.bclk().filter(|b| b.is_available())) {
            let measure = Box::new(move || meter.measure_bclk_from_cycles(cpu.first_p_thread(), 150).ok().flatten());
            let control = BclkController::new(EcClockGen::new(mailbox.clone()), measure);
            if control.capture_baseline() {
                self.emit(&format!("Clock generator: {}", control.status()));
                let _ = self.bclk_control.set(Arc::new(control));
            } else {
                self.emit(&format!("Clock generator: {}", control.status()));
            }
        }
        let sio2 = sio.clone();
        let vdd2 = Vdd2Rail::new(mailbox, Box::new(move || sio2.read_voltage(SuperIo::RAIL_VDD2)));
        let ok = vdd2.capture_baseline();
        self.emit(&format!("CPU VDD2: {}", vdd2.status()));
        if ok {
            let _ = self.vdd2.set(Arc::new(vdd2));
        }
    }

    fn initialize_intel(&self, driver: &Arc<dyn KernelDriver>) {
        let r: Result<()> = (|| {
            let cpu = Arc::new(IntelCpu::new(driver.clone())?);
            let _ = self.ctx.cpu.set(cpu.clone());
            if cpu.hypervisor_present() {
                self.emit("Hypervisor detected. Register readback may be virtualized and does not prove a tuning change reached the CPU. VBS/Memory Integrity can restrict overclocking access.");
            }
            self.emit(&format!(
                "CPU: {} - {}, {}P + {}E cores, base ratio {}, TjMax {} C",
                cpu.brand_string(),
                cpu.generation(),
                cpu.p_core_count(),
                cpu.e_core_count(),
                cpu.base_ratio(),
                cpu.tj_max()
            ));
            if !cpu.is_lga1700_family() {
                self.emit("Warning: this CPU is not a known LGA1700 (12th-14th Gen desktop) part. Ratio/voltage controls may not behave as expected.");
            }
            let oc_locked = cpu.is_oc_locked();
            self.oc_locked.store(oc_locked, Ordering::SeqCst);
            if oc_locked {
                self.emit("Warning: BIOS has set OC Lock (MSR 0x194 bit 20). Ratio and voltage changes will be rejected on this board/chipset.");
            }
            let mut mailbox_available = cpu.mailbox().is_available();
            if cpu.hypervisor_present() {
                let (restrictions, status) = hypervisor_restrictions::read_current_boot();
                self.emit(&status);
                let reason = hypervisor_restrictions::voltage_block_reason(&restrictions);
                if reason.is_some() {
                    mailbox_available = false;
                }
                *lock(&self.voltage_access_restriction) = reason;
            }
            self.mailbox_available.store(mailbox_available, Ordering::SeqCst);
            if !mailbox_available {
                self.emit(
                    &self
                        .voltage_access_restriction()
                        .unwrap_or_else(|| "OC mailbox (MSR 0x150) not responding; FIVR voltage rows disabled.".into()),
                );
            } else {
                match cpu.mailbox().read_domain(OcMailbox::DOMAIN_CORE) {
                    Ok(core) => {
                        self.bios_core_override.store(core.override_mode, Ordering::SeqCst);
                        self.emit(&format!("Startup OC mailbox core read succeeded (decoded): {core}"));
                        if core.override_mode {
                            self.emit(&format!(
                                "BIOS has the core voltage in Override mode ({:.3} V). \
                                 The board regulator path will also be probed before CPU Core Voltage is made writable; the measured Vcore rail is checked after each change.",
                                core.target_volts
                            ));
                        }
                    }
                    Err(e) => self.emit(&format!("Startup OC mailbox core read failed: {e}")),
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.emit(&format!("CPU init error: {e}"));
        }
    }

    fn initialize_amd(&self, driver: &Arc<dyn KernelDriver>) {
        let r: Result<()> = (|| {
            let amd = Arc::new(AmdCpu::new(driver.clone())?);
            let _ = self.ctx.amd.set(amd.clone());
            self.emit(&format!(
                "CPU: {} - {} ({}), family {:X}h model {:02X}h stepping {}, {} cores / {} threads, P0 ratio {}, microcode 0x{:08X}",
                amd.brand_string(),
                amd.code_name(),
                amd.generation(),
                amd.family(),
                amd.model(),
                amd.stepping(),
                amd.core_count(),
                amd.logical_count(),
                amd.base_ratio(),
                amd.patch_level()
            ));
            self.emit(&format!(
                "Topology: {}: {}",
                amd.topology_note(),
                amd.cores().iter().map(|c| format!("{}={}", c.label(), c.location())).collect::<Vec<_>>().join(", ")
            ));
            if !amd.is_supported() {
                self.emit("Warning: unknown Zen generation. The SMU message numbers of Zen 4 / Zen 5 are assumed; nothing is written until the SMU answers the test message.");
            }

            let smu = amd.smu();
            let available = smu.probe();
            self.smu_available.store(available, Ordering::SeqCst);
            *lock(&self.smu_status) = smu.status();
            self.emit(&format!("SMU: {}", smu.status()));
            if !available {
                return Ok(());
            }

            let caps = smu.read_oc_capabilities();
            *lock(&self.oc_capabilities) = caps;
            if let Some(caps) = caps {
                self.emit(&format!(
                    "SMU overclocking capabilities 0x{caps:X}: OC {}, power limits {}, PBO {}",
                    if caps & 1 != 0 { "allowed" } else { "locked" },
                    if caps & 2 != 0 { "adjustable" } else { "locked" },
                    if caps & 4 != 0 { "available" } else { "not available (enable Precision Boost Overdrive in the BIOS)" }
                ));
            }
            let stock = smu.read_stock_limits();
            *lock(&self.stock_limits) = stock;
            if stock.0.is_some() || stock.1.is_some() || stock.2.is_some() {
                self.emit(&format!(
                    "Stock limits fused into this CPU: PPT {} W, TDC {} A, EDC {} A (what 0 writes on those rows).",
                    opt0(stock.0),
                    opt0(stock.1),
                    opt0(stock.2)
                ));
            }
            if let Some((power, temp)) = smu.read_sustained_limits() {
                self.emit(&format!("Platform sustained limits: {power} W, {temp} C"));
            }

            let (located, ts) = smu.locate_table();
            if located {
                self.emit(&format!("SMU {ts}"));
                let module = native::app_dir().join("pawnio").join("RyzenSMU.bin");
                let (pawn, ps) = smu.try_attach_pawn_io(&module);
                self.emit(&format!("Power table: {ps}"));
                if !pawn {
                    self.emit("PPT/TDC/EDC rows show Auto (the BIOS value is not readable) until a value is written here.");
                } else if !smu.refresh_table() {
                    self.emit(&format!("Power table read failed: {}", smu.last_table_error().unwrap_or_default()));
                } else if let Some(layout) = smu.layout() {
                    self.emit(&format!(
                        "Limits in force from the table ({}): PPT {} W (drawing {:.1}), TDC {} A (drawing {:.1}), EDC {} A (drawing {:.1}), Tctl max {} C, socket {:.1} W",
                        layout.name,
                        optq(smu.ppt_limit()),
                        smu.ppt_value().unwrap_or(0.0),
                        optq(smu.tdc_limit()),
                        smu.tdc_value().unwrap_or(0.0),
                        optq(smu.edc_limit()),
                        smu.edc_value().unwrap_or(0.0),
                        optq(smu.thm_limit()),
                        smu.socket_power().unwrap_or(0.0)
                    ));
                } else {
                    self.emit("The limit positions in this table version are not known; PPT/TDC/EDC rows show Auto until written.");
                }
            } else {
                self.emit(&format!("SMU power table: {ts}"));
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.emit(&format!("CPU init error: {e}"));
        }
    }

    fn build_settings(&self) {
        let mut rows = Vec::new();
        if self.is_amd() {
            self.build_amd_settings(&mut rows);
        } else {
            self.build_intel_settings(&mut rows);
            self.build_board_rails(&mut rows);
        }
        self.build_dimms(&mut rows);
        *lock(&self.settings) = rows;
    }

    // ------------------------------------------------------------------ Intel rows
    fn build_intel_settings(&self, rows: &mut Vec<Setting>) {
        let cpu = self.cpu();

        // ---------------- clocks ----------------
        {
            let mut s = Setting::new("cpu_ratio", "CPU Ratio (P-Core)", SettingGroup::Clocks);
            s.min = 8.0;
            s.max = 120.0;
            s.decimals = 2;
            s.require_read_back = true;
            if let Some(c) = cpu.clone() {
                let r = c.clone();
                s.read = Arc::new(move || Ok(Some(r.read_p_core_all_core_ratio()? as f64)));
                s.write = Some(Arc::new(move |v| c.write_p_core_all_core_ratio(v.round() as i32)));
            }
            s.note = Some("All-core P-core multiplier (MSR 0x1AD turbo ratio table). Use Per Core Ratio for the per-active-core-count table.".into());
            s.available = cpu.is_some();
            rows.push(s);
        }
        {
            let has_e = cpu.as_ref().is_some_and(|c| c.e_core_count() > 0);
            let mut s = Setting::new("ecore_ratio", "E-Core Ratio", SettingGroup::Clocks);
            s.min = 8.0;
            s.max = 120.0;
            s.decimals = 2;
            s.require_read_back = true;
            if let (Some(c), true) = (cpu.clone(), has_e) {
                let r = c.clone();
                s.read = Arc::new(move || Ok(Some(r.read_e_core_all_core_ratio()? as f64)));
                s.write = Some(Arc::new(move |v| c.write_e_core_all_core_ratio(v.round() as i32)));
            }
            s.note = Some("All-core E-core multiplier (MSR 0x650).".into());
            s.available = has_e;
            rows.push(s);
        }
        {
            let mut s = Setting::new("ring_ratio", "Ring Ratio", SettingGroup::Clocks);
            s.min = 4.0;
            s.max = 120.0;
            s.decimals = 1;
            s.require_read_back = true;
            if let Some(c) = cpu.clone() {
                let r = c.clone();
                s.read = Arc::new(move || Ok(Some(r.read_ring_ratio()?.0 as f64)));
                s.write = Some(Arc::new(move |v| c.write_ring_ratio(v.round() as i32, None)));
            }
            s.note = Some("Maximum ring/uncore multiplier. Verifies MSR 0x620 and updates the additional mailbox ceiling when one is reported. The running ring clock can be lower than this limit.".into());
            s.available = cpu.is_some();
            rows.push(s);
        }
        self.add_bclk_row(rows);

        // ---------------- FIVR voltages via OC mailbox ----------------
        let mb_ok = cpu.is_some() && self.mailbox_available();
        let msi = msi_board(&self.smbios);
        let restriction = self.voltage_access_restriction();
        let core_voltage = self.core_voltage();
        let domain_readable = |d: i32| cpu.as_ref().is_some_and(|c| c.mailbox().read_domain(d).is_ok());

        let mut add_domain = |id_prefix: &str, name: &str, domain: i32, min_v: f64, max_v: f64, available: bool| {
            let row_available = available && (domain != OcMailbox::DOMAIN_CORE || !msi || core_voltage.is_some());
            let mut s = Setting::new(format!("{id_prefix}_v"), format!("{name} Voltage"), SettingGroup::Voltages);
            s.unit = "V".into();
            s.min = min_v;
            s.max = max_v;
            s.decimals = 3;
            if let (Some(c), true) = (cpu.clone(), mb_ok && row_available) {
                let cv = core_voltage.clone();
                let rc = c.clone();
                s.read = Arc::new(move || {
                    if domain == OcMailbox::DOMAIN_CORE {
                        if let Some(cv) = &cv {
                            return cv.read_target_volts();
                        }
                    }
                    let st = rc.mailbox().read_domain(domain)?;
                    // MSI Dragon Power shows the programmed target even when firmware leaves the
                    // mode bit adaptive. In particular, the E-core L2 domain reports 1.050 V in
                    // target bits 19:8 while bit 20 is clear; hiding it as Auto loses a real value.
                    if st.target_volts > 0.0 {
                        return Ok(Some(st.target_volts));
                    }
                    // This editor represents the programmed target, never live VID or a shared rail.
                    // Showing telemetry here makes Apply skip a real requested override.
                    Ok(None)
                });
                let (wc, wcv) = (c.clone(), core_voltage.clone());
                s.write = Some(Arc::new(move |v| match (&wcv, domain == OcMailbox::DOMAIN_CORE) {
                    (Some(reg), true) => write_msi_core_override(reg, wc.mailbox(), v),
                    _ => wc.mailbox().set_override(domain, v),
                }));
                let (dc, dcv) = (c.clone(), core_voltage.clone());
                s.restore_default = Some(Arc::new(move || match (&dcv, domain == OcMailbox::DOMAIN_CORE) {
                    (Some(reg), true) => clear_msi_core_override(reg, dc.mailbox()),
                    _ => dc.mailbox().set_override(domain, 0.0),
                }));
            }
            s.note = Some(restriction.clone().unwrap_or_else(|| {
                if domain == OcMailbox::DOMAIN_CORE && core_voltage.is_some() {
                    "CPU-core override written to both the MSI board regulator and Intel OC mailbox, in the same order as MSI Dragon Power. Both targets are read back; a failure rolls the regulator back. Enter 0 to return to adaptive mode.".into()
                } else if domain == OcMailbox::DOMAIN_RING {
                    "Programmed ring override through Intel OC mailbox domain 2. Auto means no explicit target. Live VID is shown separately and is not measured Vcore.".into()
                } else {
                    format!("Programmed voltage target for domain {domain} through the Intel OC mailbox. Entering a value selects override mode; the board's load-line still affects the measured rail. Enter 0 to return to adaptive mode.")
                }
            }));
            s.available = mb_ok && row_available;
            rows.push(s);

            let mut o = Setting::new(format!("{id_prefix}_off"), format!("{name} Voltage Offset"), SettingGroup::Voltages);
            o.unit = "mV".into();
            o.min = -500.0;
            o.max = 500.0;
            o.decimals = 0;
            if let (Some(c), true) = (cpu.clone(), mb_ok && available) {
                let rc = c.clone();
                o.read = Arc::new(move || Ok(Some((rc.mailbox().read_domain(domain)?.offset_volts * 1000.0).round())));
                o.write = Some(Arc::new(move |v| c.mailbox().set_offset(domain, v / 1000.0)));
            }
            o.note = Some(
                restriction
                    .clone()
                    .unwrap_or_else(|| format!("Adaptive offset for FIVR domain {domain} (mailbox 0x150). Negative values undervolt.")),
            );
            o.available = mb_ok && available;
            rows.push(o);
        };
        add_domain("core", "CPU Core", OcMailbox::DOMAIN_CORE, 0.600, 1.720, mb_ok && domain_readable(OcMailbox::DOMAIN_CORE));

        if let Some(asus) = self.asus_control().filter(|a| a.baseline_svid().is_some()) {
            let index = rows.iter().position(|s| s.id == "core_v").unwrap();
            let mut s = Setting::new("core_v", "CPU Core Voltage target", SettingGroup::Voltages);
            s.unit = "V".into();
            s.min = 0.6;
            s.max = 1.7;
            s.decimals = 3;
            let (a1, a2, a3) = (asus.clone(), asus.clone(), asus.clone());
            s.read = Arc::new(move || a1.read_core_target());
            s.write = Some(Arc::new(move |v| a2.set_core_voltage(v)));
            s.restore_default = Some(Arc::new(move || {
                let baseline = a3.baseline_svid().ok_or_else(|| anyhow!("no ASUS core baseline"))?;
                a3.set_svid(&baseline, AsusBoardControl::CORE_ID)
            }));
            s.require_read_back = true;
            s.note = Some("ASUS Global Core SVID Manual request, not a measured voltage. The applied target is read back from ASUS; measured Vcore is shown below it. The difference can vary with board regulation and load, even with BIOS settings on Auto. No automatic voltage compensation is applied. Enter 0 to restore the starting ASUS voltage state.".into());
            rows[index] = s;

            let index = rows.iter().position(|s| s.id == "core_off").unwrap();
            let mut o = Setting::new("core_off", "CPU Core Voltage Offset", SettingGroup::Voltages);
            o.unit = "mV".into();
            o.min = -500.0;
            o.max = 500.0;
            o.decimals = 0;
            o.require_read_back = true;
            let (a1, a2) = (asus.clone(), asus.clone());
            o.read = Arc::new(move || Ok(Some(a1.read_svid(AsusBoardControl::CORE_ID)?.offset_index as f64 - 999.0)));
            o.write = Some(Arc::new(move |v| {
                let state = a2.read_svid(AsusBoardControl::CORE_ID)?;
                if state.mode != 0 {
                    bail!("ASUS core voltage is in Manual mode; restore CPU Core to Auto before changing its adaptive offset.");
                }
                let idx = (v + 999.0).round();
                if !(0.0..=u32::MAX as f64).contains(&idx) {
                    bail!("Arithmetic operation resulted in an overflow.");
                }
                a2.set_svid(&SvidState { offset_index: idx as u32, ..state }, AsusBoardControl::CORE_ID)
            }));
            o.note = Some("ASUS adaptive SVID offset. Available for changes while CPU Core is in Auto/adaptive mode.".into());
            rows[index] = o;
        }
        // Not gated on the E-core count: the L2 rail for that cluster exists and is settable even
        // when the E-cores are switched off in the BIOS, which is exactly what the vendor tool shows.
        add_domain("ecore", "CPU E-Core L2", OcMailbox::DOMAIN_ECORE, 0.600, 1.520, mb_ok && domain_readable(OcMailbox::DOMAIN_ECORE));
        add_domain("ring", "Ring", OcMailbox::DOMAIN_RING, 0.600, 1.520, mb_ok && domain_readable(OcMailbox::DOMAIN_RING));
        add_domain("sa", "SA", OcMailbox::DOMAIN_SA, 0.600, 1.520, mb_ok && domain_readable(OcMailbox::DOMAIN_SA));
        add_domain("gt", "GT (iGPU)", OcMailbox::DOMAIN_GT, 0.600, 1.520, mb_ok && domain_readable(OcMailbox::DOMAIN_GT));

        if let Some(asus) = self.asus_control() {
            let mut add_asus_rail = |prefix: &str, name: &str, id: u32, baseline: Option<SvidState>| {
                let Some(baseline) = baseline else { return };
                let Some(index) = rows.iter().position(|s| s.id == format!("{prefix}_v")) else { return };
                let mut s = Setting::new(
                    format!("{prefix}_v"),
                    format!("{name} Voltage{}", if id == AsusBoardControl::RING_ID { " target" } else { "" }),
                    SettingGroup::Voltages,
                );
                s.unit = "V".into();
                s.min = 0.7;
                s.max = 1.52;
                s.decimals = 3;
                s.require_read_back = true;
                let (a1, a2, a3) = (asus.clone(), asus.clone(), asus.clone());
                s.read = Arc::new(move || a1.read_voltage_target(id));
                s.write = Some(Arc::new(move |v| a2.set_rail_voltage(id, v)));
                s.restore_default = Some(Arc::new(move || a3.set_svid(&baseline, id)));
                s.note = Some(format!(
                    "ASUS Manual voltage target through the board's dedicated control. Mode, target and offset are read back together; a failed request restores the previous state. Enter 0 to restore the startup state. {}",
                    if id == AsusBoardControl::RING_ID {
                        "Cache SVID request: ring and cores share the Vcore rail. There is no separate measured ring voltage; lowering this request alone may not lower Vcore."
                    } else {
                        "The Log shows the separate ASUS rail reading."
                    }
                ));
                rows[index] = s;
            };
            add_asus_rail("sa", "SA", AsusBoardControl::SA_ID, asus.baseline_sa());
            add_asus_rail("ecore", "CPU E-Core L2", AsusBoardControl::L2_ID, asus.baseline_l2());
            add_asus_rail("ring", "Ring", AsusBoardControl::RING_ID, asus.baseline_ring());
        }

        // ---------------- power limits ----------------
        if let Some(c) = &cpu {
            if let Ok(pl) = c.read_package_power_limits() {
                self.power_limits_locked.store(pl.2, Ordering::SeqCst);
            }
        }
        let locked = self.power_limits_locked();
        for (id, name, which, note) in [
            ("pl1", "Package Power Limit 1 (PL1)", 1, "Long duration package power limit (MSR 0x610)."),
            ("pl2", "Package Power Limit 2 (PL2)", 2, "Short duration package power limit (MSR 0x610)."),
        ] {
            let mut s = Setting::new(id, name, SettingGroup::Power);
            s.unit = "W".into();
            s.min = 10.0;
            s.max = 4095.0;
            s.decimals = 0;
            if let Some(c) = cpu.clone() {
                let r = c.clone();
                s.read = Arc::new(move || {
                    let pl = r.read_package_power_limits()?;
                    Ok(Some(if which == 1 { pl.0 } else { pl.1 }))
                });
                if !locked {
                    s.write = Some(Arc::new(move |v| {
                        if which == 1 { c.write_package_power_limits(Some(v), None) } else { c.write_package_power_limits(None, Some(v)) }
                    }));
                }
            }
            s.note = Some(if locked { "Locked by BIOS (MSR 0x610 bit 63).".into() } else { note.into() });
            s.available = cpu.is_some();
            rows.push(s);
        }
    }

    fn add_bclk_row(&self, rows: &mut Vec<Setting>) {
        let control = self.bclk_control();
        let mut s = Setting::new("bclk", "Base Clock", SettingGroup::Clocks);
        s.min = if control.is_some() { BclkController::MIN_BCLK_MHZ } else { 10.0 };
        s.max = if control.is_some() { BclkController::MAX_BCLK_MHZ } else { 655.25 };
        s.decimals = 2;
        let ctx = self.ctx.clone();
        s.read = Arc::new(move || Ok(*lock(&ctx.last_bclk)));
        s.default_value = control.as_ref().map(|c| c.baseline_mhz());
        if let Some(c) = control.clone() {
            let (ctx, emit) = (self.ctx.clone(), self.emitter());
            let wc = c.clone();
            s.write = Some(Arc::new(move |v| {
                if !wc.set_bclk(v, Some(&emit)) {
                    bail!("{}", wc.status());
                }
                ctx.measure_bclk();
                Ok(())
            }));
            let ctx = self.ctx.clone();
            s.restore_default = Some(Arc::new(move || {
                if !c.restore() {
                    bail!("{}", c.status());
                }
                ctx.measure_bclk();
                Ok(())
            }));
        }
        s.note = Some(if self.is_amd() {
            "Measured base clock: the time-stamp counter, which Ryzen runs at base clock x the P0 multiplier, \
             timed against the ACPI timer, which does not move with base clock. Read-only: no Windows write path \
             to the board's clock generator has been verified on AM5."
                .into()
        } else if self.asus_control().is_some() && control.is_none() {
            "Measured physical BCLK. The ASUS scalar target read back during testing, but the physical clock did not move. Live writes remain unavailable until a working clock-generator interface is verified.".into()
        } else if control.is_none() {
            "Real core clocks counted against the ACPI timer, so it tracks a BCLK change made anywhere - including \
             one made in the board vendor's tool while this is running. Read-only here: the write path goes through \
             the board's own clock generator, which is only reachable where the board's EC exposes it."
                .into()
        } else {
            "Set through the board's clock generator, over the EC's own I2C bus. Every step is measured against the \
             ACPI timer, which does not move with the base clock, and anything that lands off target puts back the \
             value found at start-up. There is no ceiling on what you can ask for - where a board gives up is not \
             something this can know - but base clock scales the memory controller, the ring and the PCIe/DMI \
             reference with it, so a little goes a long way and past a point the machine simply stops. \
             Enter 0 to put back the start-up value."
                .into()
        });
        s.available = self.bclk().is_some_and(|b| b.is_available()) && self.base_ratio() > 0;
        rows.push(s);
    }

    // ------------------------------------------------------------------ AMD rows
    fn build_amd_settings(&self, rows: &mut Vec<Setting>) {
        let amd = self.amd().unwrap();
        let msgs = amd.smu().messages().clone();
        let live = self.smu_available();
        let pbo = live && self.pbo_allowed();
        let pbo_note = if pbo {
            ""
        } else {
            " PBO is reported as unavailable by the SMU: enable Precision Boost Overdrive in the BIOS, or the write will be rejected."
        };

        // ---------------- clocks ----------------
        {
            let mut s = Setting::new("fmax", "FMax", SettingGroup::Clocks);
            s.unit = "MHz".into();
            s.min = 1000.0;
            s.max = 8000.0;
            s.decimals = 0;
            let a = amd.clone();
            s.read = Arc::new(move || Ok(if live { a.smu().read_boost_limit_mhz().map(|v| v as f64) } else { None }));
            if live && msgs.rsmu_set_boost_limit_all != 0 {
                let a = amd.clone();
                s.write = Some(Arc::new(move |v| a.smu().set_boost_limit_mhz(v.round() as i32)));
            }
            s.note = Some("Ceiling for the all-core boost clock (the SMU's boost limit, what the BIOS calls Max CPU Boost Clock Override / FMax). Raising it only helps if the CPU has thermal, current and power headroom left. 0 restores the start-up value.".into());
            s.available = live && msgs.has_boost_limit();
            rows.push(s);
        }
        self.add_bclk_row(rows);

        // ---------------- power / current limits ----------------
        let smu_layout_known = amd.smu().table_readable() && amd.smu().layout().is_some();
        let table_note = if smu_layout_known {
            " Read back from the SMU power table through PawnIO."
        } else {
            " The SMU has no message that reports the limit in force and, without PawnIO, its power table cannot be read, so the row starts as Auto (whatever the BIOS set) and then shows what was written here."
        };
        let stock = self.stock_limits();
        let stock_note = |v: Option<f64>, unit: &str| match v {
            Some(d) => format!(" Entering 0 writes the CPU's stock value ({d:.0} {unit}); the BIOS value itself only comes back with a reboot."),
            None => String::new(),
        };

        type Field = fn(&AmdCpu) -> Option<f32>;
        type Write = fn(&AmdCpu, f64) -> Result<()>;
        let limit_row = |id: &'static str,
                         name: &str,
                         unit: &str,
                         min: f64,
                         max: f64,
                         label: &'static str,
                         limit: Field,
                         value: Field,
                         write: Write,
                         writable: bool,
                         stock: Option<f64>,
                         note: String| {
            let mut s = Setting::new(id, name, SettingGroup::Power);
            s.unit = unit.into();
            s.min = min;
            s.max = max;
            s.decimals = 0;
            let (a, ctx) = (amd.clone(), self.ctx.clone());
            s.read = Arc::new(move || {
                if !live {
                    return Ok(None);
                }
                if ctx.table_fresh() {
                    if let Some(f) = limit(&a).filter(|f| *f > 0.0) {
                        return Ok(Some((f as f64).round()));
                    }
                }
                Ok(lock(&ctx.last_written).get(id).copied())
            });
            // What the CPU is drawing against each limit; only where the table itself can be read.
            if live && smu_layout_known {
                let (a, ctx) = (amd.clone(), self.ctx.clone());
                s.live = Some(Arc::new(move || {
                    if !ctx.table_fresh() {
                        return None;
                    }
                    value(&a).filter(|f| *f >= 0.0).map(|f| f as f64)
                }));
            }
            if live && writable {
                let (a, ctx) = (amd.clone(), self.ctx.clone());
                s.write = Some(Arc::new(move |v| {
                    write(&a, v)?;
                    lock(&ctx.last_written).insert(id.to_string(), v);
                    let followed = a.smu().verify_layout(id, &|| limit(&a), v);
                    *lock(&ctx.table_read_at) = None;
                    match followed {
                        Some(true) => ctx.emit(&format!("{label}: the power table confirms {v:.0}.")),
                        Some(false) => ctx.emit(&format!(
                            "{label}: the SMU accepted {v:.0} but the power table float this build expected for {label} did not follow. The limit is applied; this row now shows what is written here instead of reading it back. Please report your CPU and table version 0x{:08X}.",
                            a.smu().table_version()
                        )),
                        None => {}
                    }
                    Ok(())
                }));
            }
            if live {
                if let Some(v) = stock {
                    let (a, ctx) = (amd.clone(), self.ctx.clone());
                    s.restore_default = Some(Arc::new(move || {
                        write(&a, v)?;
                        lock(&ctx.last_written).insert(id.to_string(), v);
                        Ok(())
                    }));
                }
            }
            s.note = Some(note);
            s.available = live && writable;
            s
        };

        rows.push(limit_row(
            "ppt",
            "PPT (Package Power Tracking)",
            "W",
            5.0,
            2000.0,
            "PPT",
            |a| a.smu().ppt_limit(),
            |a| a.smu().ppt_value(),
            |a, v| a.smu().set_ppt(v),
            msgs.has_power_limits(),
            stock.0,
            format!(
                "Total socket power the boost algorithm may use, in watts (SMU message SetPPTLimit).{table_note}{}{pbo_note}",
                stock_note(stock.0, "W")
            ),
        ));
        rows.push(limit_row(
            "tdc",
            "TDC (Thermal Design Current)",
            "A",
            5.0,
            2000.0,
            "TDC",
            |a| a.smu().tdc_limit(),
            |a| a.smu().tdc_value(),
            |a, v| a.smu().set_tdc(v),
            msgs.has_current_limits(),
            stock.1,
            format!(
                "Sustained current the VRM may deliver on the core rail, in amperes, thermally limited (SetTDCVDDLimit).{table_note}{}{pbo_note}",
                stock_note(stock.1, "A")
            ),
        ));
        rows.push(limit_row(
            "edc",
            "EDC (Electrical Design Current)",
            "A",
            5.0,
            2000.0,
            "EDC",
            |a| a.smu().edc_limit(),
            |a| a.smu().edc_value(),
            |a, v| a.smu().set_edc(v),
            msgs.has_current_limits(),
            stock.2,
            format!(
                "Peak current the VRM may deliver on the core rail, in amperes (SetEDCVDDLimit).{table_note}{}{pbo_note}",
                stock_note(stock.2, "A")
            ),
        ));
        rows.push(limit_row(
            "tctl",
            "Thermal Limit (Tctl max)",
            "C",
            50.0,
            115.0,
            "Tctl max",
            |a| a.smu().thm_limit(),
            |a| a.smu().thm_value(),
            |a, v| a.smu().set_tctl_max(v),
            msgs.rsmu_set_tctl_max != 0,
            None,
            format!(
                "Temperature the boost algorithm holds the CPU to, in degrees C (SetTctlMax). Lower it to trade a little clock for a quieter, cooler CPU.{table_note}"
            ),
        ));

        // ---------------- PBO ----------------
        {
            let mut s = Setting::new("scalar", "PBO Scalar", SettingGroup::Pbo);
            s.unit = "x".into();
            s.min = 1.0;
            s.max = 10.0;
            s.decimals = 0;
            let a = amd.clone();
            s.read = Arc::new(move || Ok(if live { a.smu().read_scalar() } else { None }));
            if live && msgs.has_scalar() {
                let a = amd.clone();
                s.write = Some(Arc::new(move |v| a.smu().set_scalar(v)));
            }
            s.note = Some(format!(
                "Precision Boost Overdrive scalar, 1x to 10x: how far past the silicon's fused voltage/reliability envelope the boost algorithm may sustain. 1 is stock.{}{pbo_note}",
                if msgs.rsmu_get_scalar != 0 {
                    " Read back from the SMU; a read-back of 0 means the SMU is in manual overclock mode."
                } else {
                    ""
                }
            ));
            s.available = live && msgs.has_scalar();
            rows.push(s);
        }
        {
            let range = msgs.co_range();
            let mut s = Setting::new("co_all", "Curve Optimizer (all cores)", SettingGroup::Pbo);
            s.min = -(range as f64);
            s.max = range as f64;
            s.decimals = 0;
            let (a, ctx) = (amd.clone(), self.ctx.clone());
            let readback = msgs.has_curve_optimizer_readback();
            s.read = Arc::new(move || {
                if !live || !readback {
                    return Ok(lock(&ctx.last_written).get("co_all").copied());
                }
                let margins: Vec<Option<i32>> = a.cores().iter().map(|c| read_curve_optimizer_with(&a, &ctx, live, c)).collect();
                if margins.iter().any(|m| m.is_none()) {
                    return Ok(None);
                }
                let first = margins.first().copied().flatten();
                Ok(if margins.iter().all(|m| *m == first) { first.map(|v| v as f64) } else { None })
            });
            if live && msgs.has_curve_optimizer() {
                let (a, ctx) = (amd.clone(), self.ctx.clone());
                s.write = Some(Arc::new(move |v| {
                    let m = v.round() as i32;
                    a.smu().set_curve_optimizer_all(m)?;
                    lock(&ctx.last_written).insert("co_all".into(), m as f64);
                    let mut co = lock(&ctx.last_co);
                    for c in a.cores().iter() {
                        co.insert(c.index, m);
                    }
                    Ok(())
                }));
                let (a, ctx) = (amd.clone(), self.ctx.clone());
                s.restore_default = Some(Arc::new(move || {
                    a.smu().set_curve_optimizer_all(0)?;
                    lock(&ctx.last_written).insert("co_all".into(), 0.0);
                    let mut co = lock(&ctx.last_co);
                    for c in a.cores().iter() {
                        co.insert(c.index, 0);
                    }
                    Ok(())
                }));
            }
            s.note = Some(format!(
                "One Curve Optimizer offset for every core, in counts ({} to +{range}; one count is roughly 3 to 5 mV). Negative undervolts. \
                 Auto means the cores currently differ: open the Curve Optimizer window for the per-core values. Entering 0 sets every core to 0.{pbo_note}",
                -range
            ));
            s.available = live && msgs.has_curve_optimizer();
            rows.push(s);
        }
    }

    /// Per-core Curve Optimizer margin from the SMU, or what was last written here when the firmware cannot report it.
    pub fn read_curve_optimizer(&self, core: &AmdCore) -> Option<i32> {
        let amd = self.amd()?;
        read_curve_optimizer_with(&amd, &self.ctx, self.smu_available(), core)
    }

    pub fn apply_curve_optimizer(&self, core: &AmdCore, margin: i32) -> Result<()> {
        let Some(amd) = self.amd().filter(|_| self.smu_available()) else { bail!("SMU not available.") };
        let smu = amd.smu();
        smu.set_curve_optimizer(core.ccd, core.core_in_ccd, margin)?;
        lock(&self.ctx.last_co).insert(core.index, margin);
        let rb = if smu.messages().has_curve_optimizer_readback() { smu.read_curve_optimizer(core.ccd, core.core_in_ccd) } else { None };
        self.emit(&format!(
            "Curve Optimizer {} ({}): set to {margin}{}.",
            core.label(),
            core.location(),
            rb.map(|r| format!(" (SMU reports {r})")).unwrap_or_default()
        ));
        if let Some(r2) = rb {
            if r2 != margin {
                bail!("the SMU accepted {margin} but reports {r2}.");
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ shared rows
    fn build_board_rails(&self, rows: &mut Vec<Setting>) {
        // These have no CPU-side register at all: the motherboard's regulators produce them, and
        // the only way to set them is that board's own VRM protocol. Shown live, read-only, rather
        // than offered as a field that would silently do nothing.
        let Some(sio) = self.super_io() else { return };
        self.add_vdd2_row(rows, &sio);
        let mut add_board_rail = |id: &str, name: &str, rail: i32, note: &str| {
            if !sio.named_rails().iter().any(|r| r.1 == rail) {
                return;
            }
            match sio.read_voltage(rail) {
                Some(v) if v >= 0.05 => {}
                _ => return,
            }
            let mut s = Setting::new(id, name, SettingGroup::Board);
            s.unit = "V".into();
            s.min = 0.0;
            s.max = 5.0;
            s.decimals = 3;
            let r = sio.clone();
            s.read = Arc::new(move || Ok(r.read_voltage(rail)));
            s.note = Some(note.into());
            s.available = true;
            rows.push(s);
        };
        add_board_rail(
            "cpu_aux",
            "CPU AUX Voltage",
            SuperIo::RAIL_AUX,
            "CPU AUX rail, measured at the board. It is produced by the motherboard VRM, like CPU VDD2, and most likely \
             sits on the same EC I2C bus - but which device and register carry it has not been established, and this \
             does not write a regulator it has not verified. Set it in the BIOS.",
        );
    }

    /// CPU VDD2. Measured at the Super I/O either way; settable as well where the regulator on the
    /// EC's I2C bus answers, which is what the vendor tool drives.
    fn add_vdd2_row(&self, rows: &mut Vec<Setting>, sio: &Arc<SuperIo>) {
        // MSI EC channel numbers are not labels for ASUS banked-Nuvoton inputs.
        if !sio.named_rails().iter().any(|r| r.1 == SuperIo::RAIL_VDD2) {
            return;
        }
        match sio.read_voltage(SuperIo::RAIL_VDD2) {
            Some(v) if v >= 0.05 => {}
            _ => return,
        }
        let rail = self.vdd2();
        let mut s = Setting::new("cpu_vdd2", "CPU VDD2 Voltage", SettingGroup::Board);
        s.unit = "V".into();
        s.min = if rail.is_some() { Vdd2Rail::MIN_VOLTS } else { 0.0 };
        s.max = if rail.is_some() { Vdd2Rail::MAX_VOLTS } else { 5.0 };
        s.decimals = 3;
        let r = sio.clone();
        s.read = Arc::new(move || Ok(r.read_voltage(SuperIo::RAIL_VDD2)));
        s.default_value = rail.as_ref().map(|r| r.baseline_volts());
        if let Some(rail) = rail.clone() {
            let (w, emit) = (rail.clone(), self.emitter());
            s.write = Some(Arc::new(move |t| {
                if !w.set_volts(t, Some(&emit)) {
                    bail!("{}", w.status());
                }
                Ok(())
            }));
            s.restore_default = Some(Arc::new(move || {
                if !rail.restore() {
                    bail!("{}", rail.status());
                }
                Ok(())
            }));
        }
        s.note = Some(if rail.is_none() {
            "CPU VDD2 (the memory controller's supply), measured at the board through the Super I/O.".into()
        } else {
            "CPU VDD2, the memory controller's supply. Set on the board's regulator over the EC's I2C bus, one \
             step at a time, with the rail measured after each step; anything that lands off target puts back \
             the value found at start-up. The reading is the Super I/O's measurement of the rail, so it sits a \
             little below what the regulator was asked for. Enter 0 to put back the start-up value."
                .into()
        });
        s.available = true;
        rows.push(s);
    }

    fn build_dimms(&self, rows: &mut Vec<Setting>) {
        for dimm in self.dimms() {
            let unverified = " Scale could not be verified against the PMIC ADC (secure-mode PMIC), so this row is read-only.";
            let slot = dimm.slot_name().to_string();
            let lower = slot.to_lowercase();

            let mut s = Setting::new(format!("{lower}_vdd"), format!("DRAM {slot} Voltage"), SettingGroup::Memory);
            s.unit = "V".into();
            s.min = Ddr5Dimm::VDD_MIN_V;
            s.max = dimm.vdd_ceiling_v();
            s.decimals = 3;
            let d = dimm.clone();
            s.read = Arc::new(move || Ok(d.read_vdd()));
            if dimm.vdd_verified() {
                let d = dimm.clone();
                s.write = Some(Arc::new(move |v| d.write_vdd(v)));
                s.limit_note = Some(Ddr5Dimm::step_limit_note(dimm.vdd_step_mv()));
            }
            s.note = Some(format!(
                "VDD set-point on the PMIC of {slot} (0x{:02X}, {}), register step {}.{}{}",
                dimm.pmic_address(),
                dimm.pmic_vendor(),
                if dimm.vdd_verified() { format!("{} mV", dimm.vdd_step_mv()) } else { "unknown".into() },
                if dimm.vdd_verified() { "" } else { unverified },
                if dimm.vdd_verified() && dimm.vdd_step_mv() == 5 { format!(" {}", Ddr5Dimm::step_limit_note(5)) } else { String::new() }
            ));
            s.available = dimm.has_pmic();
            rows.push(s);

            let mut s = Setting::new(format!("{lower}_vddq"), format!("DRAM {slot} VDDQ Voltage"), SettingGroup::Memory);
            s.unit = "V".into();
            s.min = Ddr5Dimm::VDDQ_MIN_V;
            s.max = dimm.vddq_ceiling_v();
            s.decimals = 3;
            let d = dimm.clone();
            s.read = Arc::new(move || Ok(d.read_vddq()));
            if dimm.vddq_verified() {
                let d = dimm.clone();
                s.write = Some(Arc::new(move |v| d.write_vddq(v)));
                s.limit_note = Some(Ddr5Dimm::step_limit_note(dimm.vddq_step_mv()));
            }
            s.note = Some(format!(
                "VDDQ set-point (PMIC SWC rail), register step {}.{}{}",
                if dimm.vddq_verified() { format!("{} mV", dimm.vddq_step_mv()) } else { "unknown".into() },
                if dimm.vddq_verified() { "" } else { unverified },
                if dimm.vddq_verified() && dimm.vddq_step_mv() == 5 { format!(" {}", Ddr5Dimm::step_limit_note(5)) } else { String::new() }
            ));
            s.available = dimm.has_pmic();
            rows.push(s);

            let mut s = Setting::new(format!("{lower}_vpp"), format!("DRAM {slot} VPP Voltage"), SettingGroup::Memory);
            s.unit = "V".into();
            s.min = Ddr5Dimm::VPP_MIN_V;
            s.max = Ddr5Dimm::VPP_MAX_V;
            s.decimals = 3;
            let d = dimm.clone();
            s.read = Arc::new(move || Ok(d.read_vpp()));
            if dimm.vpp_verified() {
                let d = dimm.clone();
                s.write = Some(Arc::new(move |v| d.write_vpp(v)));
            }
            s.note = Some(format!(
                "VPP set-point (PMIC SWD rail, 5 mV steps).{}",
                if dimm.vpp_verified() { "" } else { unverified }
            ));
            s.available = dimm.has_pmic();
            rows.push(s);
        }
    }

    // ------------------------------------------------------------------ refresh / apply
    pub fn refresh_all(&self, capture_defaults: bool) {
        let snapshot: Vec<Setting> = lock(&self.settings).clone();
        let core_baseline = self.core_voltage().and_then(|cv| cv.verified_baseline());
        let mut results = Vec::with_capacity(snapshot.len());
        for s in &snapshot {
            let current = if !s.available {
                None
            } else {
                match (s.read)() {
                    Ok(v) => v,
                    Err(e) => {
                        self.emit(&format!("{}: read failed - {e}", s.name));
                        None
                    }
                }
            };
            results.push(current);
        }
        let mut rows = lock(&self.settings);
        for (row, current) in rows.iter_mut().zip(results) {
            row.current = current;
            if capture_defaults {
                row.default_value = match (&core_baseline, row.id.as_str()) {
                    (Some(b), "core_v") => b.target_volts(),
                    _ => row.current,
                };
            }
        }
    }

    /// Re-reads one row.
    pub fn refresh(&self, id: &str) {
        let Some(mut s) = self.setting(id) else { return };
        self.refresh_row(&mut s);
        self.store_row(&s);
    }

    fn refresh_row(&self, s: &mut Setting) {
        if !s.available {
            return;
        }
        match (s.read)() {
            Ok(v) => s.current = v,
            Err(e) => {
                s.current = None;
                self.emit(&format!("{}: read failed - {e}", s.name));
            }
        }
    }

    fn store_row(&self, s: &Setting) {
        let mut rows = lock(&self.settings);
        if let Some(row) = rows.iter_mut().find(|r| r.id == s.id) {
            row.current = s.current;
            row.default_value = s.default_value;
            row.last_error = s.last_error.clone();
            row.last_result = s.last_result.clone();
        }
    }

    /// Average VID over a few samples; idle VID jitters, so one reading is not enough.
    fn sample_vid(&self) -> Option<f64> {
        let cpu = self.cpu()?;
        let mut sum = 0.0;
        for _ in 0..4 {
            sum += cpu.read_perf_status(cpu.first_p_thread()).ok()?.1;
            native::sleep_ms(25);
        }
        Some(sum / 4.0)
    }

    /// Mean and spread of the measured Vcore. The spread matters: the rail moves on its own with
    /// idle load, and without knowing that band a noise blip reads as a successful change.
    fn sample_vcore(&self, samples: usize) -> Option<(f64, f64)> {
        let asus = self.asus_control();
        let sio = self.super_io();
        if sio.is_none() && asus.is_none() {
            return None;
        }
        let mut vals = Vec::with_capacity(samples);
        for _ in 0..samples {
            let v = match &asus {
                Some(a) => Some(a.read_vcore().ok()?),
                None => sio.as_ref().and_then(|s| s.read_vcore()),
            };
            if let Some(v) = v.filter(|v| *v > 0.4 && *v < 2.5) {
                vals.push(v);
            }
            native::sleep_ms(30);
        }
        if vals.is_empty() {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((mean, max - min))
    }

    /// True when the row drives the core voltage, so an apply can be checked against the rail
    /// the VRM actually produces rather than trusting that the CPU's request was honoured.
    fn is_core_voltage_row(s: &Setting) -> bool {
        s.id == "core_v" || s.id == "core_off"
    }

    /// Applies one value by row id. Zero restores startup defaults.
    pub fn apply(&self, id: &str, value: f64) -> bool {
        let Some(mut s) = self.setting(id) else { return false };
        let ok = self.apply_row(&mut s, value);
        self.store_row(&s);
        ok
    }

    fn apply_row(&self, s: &mut Setting, value: f64) -> bool {
        s.last_error = None;
        s.last_result = None;
        let fail = |s: &mut Setting, message: String| {
            self.emit(&format!("{}: {message}", s.name));
            s.last_error = Some(message);
            false
        };
        let Some(write) = s.write.clone().filter(|_| s.available) else {
            return fail(s, "unavailable or read-only.".into());
        };
        if !value.is_finite() {
            return fail(s, "enter a finite number.".into());
        }
        if matches!(s.id.as_str(), "cpu_ratio" | "ecore_ratio" | "ring_ratio") && value != value.trunc() {
            return fail(s, "enter a whole-number multiplier.".into());
        }
        // A detected Super I/O does not necessarily have a valid Vcore channel. Do not claim a
        // rail check unless an actual plausible sample was captured. The MSI regulator path also
        // performs its own target-register readback.
        let rail_before = if Self::is_core_voltage_row(s) { self.sample_vcore(6) } else { None };
        let verify = rail_before.is_some();
        let vid_before = if verify { self.sample_vid() } else { None };
        let setting_before = s.current;
        let unit_suffix = if s.unit.is_empty() { String::new() } else { format!(" {}", s.unit) };

        let r: Result<bool> = (|| {
            if value == 0.0 {
                if let Some(restore) = s.restore_default.clone() {
                    restore()?;
                } else if let Some(d) = s.default_value {
                    write(d)?;
                } else {
                    return Ok(fail(s, "no default captured; nothing changed.".into()));
                }
                self.refresh_row(s);
                if s.require_read_back {
                    if let Some(expected) = s.default_value {
                        if s.current != Some(expected) {
                            let msg = format!(
                                "reset could not be verified (expected {}, read {}).",
                                s.format(expected),
                                s.current_text()
                            );
                            return Ok(fail(s, msg));
                        }
                    }
                }
                s.last_result = Some("restored".into());
                if Self::is_core_voltage_row(s) {
                    self.core_voltage_ignored.store(false, Ordering::SeqCst);
                }
                self.emit(&format!("{}: restored default ({}{unit_suffix}).", s.name, s.current_text()));
                return Ok(true);
            }
            if value < s.min || value > s.max {
                let mut msg = format!("{} is outside {}.", s.format(value), s.range_text());
                if value > s.max {
                    if let Some(n) = s.limit_note.as_ref().filter(|n| !n.is_empty()) {
                        msg += &format!(" {n}");
                    }
                }
                return Ok(fail(s, msg));
            }
            write(value)?;
            self.refresh_row(s);
            if s.require_read_back {
                let retained = s.current.is_some_and(|actual| (actual - value).abs() <= 0.5 * 10f64.powi(-(s.decimals as i32)));
                if !retained {
                    let msg = format!(
                        "request was not retained (requested {}, read back {}).",
                        s.format(value),
                        s.current_text()
                    );
                    return Ok(fail(s, msg));
                }
            }
            self.emit(&format!("{}: set to {}{unit_suffix} (now {}).", s.name, s.format(value), s.current_text()));
            if s.id == "core_v" && self.core_voltage().is_some() {
                self.core_voltage_ignored.store(false, Ordering::SeqCst);
                self.emit(&format!("{}: MSI regulator and Intel mailbox both read back {} V.", s.name, s.current_text()));
            }
            let asus = self.asus_control();
            if let (true, Some(a)) = (s.id == "core_v", &asus) {
                self.core_voltage_ignored.store(false, Ordering::SeqCst);
                self.emit(&format!(
                    "ASUS Manual SVID target {} V retained; ASUS measured Vcore {:.3} V. Measured rail and requested target are different quantities; load-line behavior affects the rail.",
                    s.current_text(),
                    a.read_vcore()?
                ));
            } else if let (true, Some(a)) = (matches!(s.id.as_str(), "sa_v" | "ecore_v"), &asus) {
                let id = if s.id == "sa_v" { AsusBoardControl::SA_ID } else { AsusBoardControl::L2_ID };
                self.emit(&format!(
                    "ASUS {} target {} V retained; board sensor {:.3} V.",
                    s.name,
                    s.current_text(),
                    a.read_rail_voltage(id)?
                ));
            } else if let (true, Some(a)) = (s.id == "ring_v", &asus) {
                self.emit(&format!(
                    "ASUS Cache SVID target {} V retained. Shared Vcore sensor {:.3} V; this is not a separate ring-voltage measurement.",
                    s.current_text(),
                    a.read_vcore()?
                ));
            } else if verify && self.verify_against_rail(s, setting_before, vid_before, rail_before) == Some(false) {
                return Ok(fail(s, "request read back; Vcore response could not be verified (see Log).".into()));
            }
            let intel = self.cpu().is_some();
            s.last_result = Some(
                if intel && (s.require_read_back || s.group == SettingGroup::Voltages) { "read back" } else { "applied" }.into(),
            );
            if intel
                && s.group == SettingGroup::Voltages
                && !verify
                && !(asus.is_some() && matches!(s.id.as_str(), "sa_v" | "ecore_v" | "ring_v"))
            {
                self.emit(&format!("{}: register readback verified; actual rail voltage is unverified on this board.", s.name));
            }
            Ok(true)
        })();
        match r {
            Ok(ok) => ok,
            Err(e) => {
                self.refresh_row(s);
                fail(s, format!("{e:#}"))
            }
        }
    }

    /// The CPU accepting a voltage request does not prove the board acted on it. Compare the VID
    /// the CPU now asks for against the rail the Super I/O measures, and say so when they disagree.
    fn verify_against_rail(
        &self,
        s: &Setting,
        setting_before: Option<f64>,
        vid_before: Option<f64>,
        rail_before: Option<(f64, f64)>,
    ) -> Option<bool> {
        native::sleep_ms(300);
        let vid_after = self.sample_vid();
        let rail_after = self.sample_vcore(6);
        let (v0, v1, r0, r1) = (vid_before?, vid_after?, rail_before?, rail_after?);
        let d_vid = (v1 - v0) * 1000.0;
        let d_rail = (r1.0 - r0.0) * 1000.0;
        // For a fixed core override, compare the rail with the regulator target that was actually
        // changed. VID can move even during a no-op reconciliation because the mailbox and board
        // had previously been left out of sync. Offset rows still use the VID delta.
        let expected_mv = match (s.id == "core_v", setting_before, s.current) {
            (true, Some(old), Some(new)) => (new - old) * 1000.0,
            _ => d_vid,
        };
        if expected_mv.abs() < 5.0 {
            return Some(true); // target did not move: register readback is the verification
        }

        // A real change must be in the same direction as the request, clear of the rail's own
        // noise band, and a decent fraction of what was asked for. Magnitude alone is not enough:
        // idle Vcore wanders by tens of millivolts on its own.
        let noise = (r0.1.max(r1.1) * 1000.0).max(12.0);
        let same_direction = sign(d_rail) == sign(expected_mv);
        let followed = same_direction && d_rail.abs() > noise && d_rail.abs() >= expected_mv.abs() * 0.35;

        if followed {
            self.emit(&format!(
                "{}: verified at the rail - target {expected_mv:+.0} mV, VID {d_vid:+.0} mV, measured Vcore {d_rail:+.0} mV (now {:.3} V).",
                s.name, r1.0
            ));
            self.core_voltage_ignored.store(false, Ordering::SeqCst);
            return Some(true);
        }
        self.core_voltage_ignored.store(true, Ordering::SeqCst);
        self.emit(&format!(
            "{}: rail response unverified. The requested target moved {expected_mv:+.0} mV (VID {d_vid:+.0} mV), and measured Vcore moved {d_rail:+.0} mV \
             (now {:.3} V, noise threshold {noise:.0} mV). Load changes and monitor resolution can obscure the response; these samples do not establish the cause. \
             The programmed target remains changed. Check measured Vcore under a consistent load or restore the starting setting.",
            s.name, r1.0
        ));
        Some(false)
    }

    pub fn restore_all_defaults(&self) -> i32 {
        let candidates: Vec<String> = lock(&self.settings)
            .iter()
            .filter(|s| s.available && !s.read_only() && s.current != s.default_value)
            .map(|s| s.id.clone())
            .collect();
        candidates.iter().filter(|id| !self.apply(id, 0.0)).count() as i32
    }

    // ---------------------------------------------------------------- live
    pub fn read_live(&self) -> LiveStatus {
        let mut st = LiveStatus { bclk_mhz: self.last_bclk(), ..Default::default() };
        st.vcore_vrm = match self.asus_control() {
            Some(a) => a.read_vcore().ok(),
            None => self.super_io().and_then(|s| s.read_vcore()),
        };
        if let Some(cpu) = self.cpu() {
            st.package_temp_c = cpu.read_package_temperature().ok();
            if let Ok((ratio, vid)) = cpu.read_perf_status(cpu.first_p_thread()) {
                st.core_vid = Some(vid);
                if let Ok(max) = cpu.read_max_current_p_ratio() {
                    let r = ratio.max(max);
                    st.core_ratio = Some(r);
                    st.core_mhz = Some(r as f64 * st.bclk_mhz.unwrap_or(100.0));
                }
            }
            st.ring_ratio = cpu.read_current_ring_ratio().ok();
            st.e_core_ratio = cpu.read_max_current_e_ratio().ok().flatten();
            if let Ok(j) = cpu.read_package_energy_joules() {
                st.package_watts = self.power_from_energy(j);
            }
        } else if let Some(amd) = self.amd() {
            st.package_temp_c = amd.read_temperature().map(|t| t.round() as i32);
            let mhz = amd.read_max_core_mhz() * (st.bclk_mhz.unwrap_or(100.0) / 100.0);
            st.core_mhz = Some(mhz);
            st.core_ratio = Some((mhz / 100.0).round() as i32);
            st.core_vid = amd.read_core_vid();
            if let Ok(j) = amd.read_package_energy_joules() {
                st.package_watts = self.power_from_energy(j);
            }
        }
        st
    }

    /// Package power from two RAPL energy counter samples; the first call only primes the counter.
    fn power_from_energy(&self, joules: f64) -> Option<f64> {
        let now = Instant::now();
        let mut e = lock(&self.energy);
        let mut watts = None;
        if let Some(at) = e.1 {
            let dt = (now - at).as_secs_f64();
            let mut de = joules - e.0;
            if de < 0.0 {
                de += 4294967296.0 * (1.0 / 65536.0); // 32-bit wrap at the common 1/65536 J unit
            }
            if dt > 0.2 {
                watts = Some(de / dt);
            }
        }
        *e = (joules, Some(now));
        watts
    }

    /// Blocking measurement; call from a background thread. See [`Ctx::measure_bclk`].
    pub fn measure_bclk(&self) -> Option<f64> {
        self.ctx.measure_bclk()
    }
}

fn sign(v: f64) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

fn read_curve_optimizer_with(amd: &AmdCpu, ctx: &Ctx, smu_available: bool, core: &AmdCore) -> Option<i32> {
    if !smu_available {
        return None;
    }
    let smu = amd.smu();
    let m = if smu.messages().has_curve_optimizer_readback() { smu.read_curve_optimizer(core.ccd, core.core_in_ccd) } else { None };
    if m.is_some() {
        return m;
    }
    lock(&ctx.last_co).get(&core.index).copied()
}

use core_voltage_recovery::{clear_msi_core_override, write_msi_core_override};
