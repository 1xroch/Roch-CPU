//! Measures the real base clock by comparing the CPU time-stamp counter (which runs
//! at BCLK x base ratio) against the ACPI power-management timer (a fixed 3.579545 MHz
//! reference that does not move with BCLK). This is how monitoring tools report a
//! BCLK such as 100.01 MHz instead of assuming 100.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

use super::KernelDriver;
use super::native;
use super::smbios::FirmwareTables;

const PM_TIMER_HZ: f64 = 3_579_545.0;

const MSR_IA32_MPERF: u32 = 0xE7;
const MSR_IA32_APERF: u32 = 0xE8;
const MSR_IA32_PERF_STATUS: u32 = 0x198;
const IA32_FIXED_CTR1: u32 = 0x30A;
const IA32_FIXED_CTR_CTRL: u32 = 0x38D;
const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;

/// `rdtsc`. The .NET build ran a 10-byte machine-code stub from VirtualAlloc'd memory for this;
/// Rust has the instruction as an intrinsic.
#[allow(unused_unsafe)]
fn rdtsc() -> u64 {
    // SAFETY: RDTSC is available on every x86-64 processor and has no memory effects.
    unsafe { std::arch::x86_64::_rdtsc() }
}

/// Raises the calling thread to `THREAD_PRIORITY_HIGHEST` (the .NET `ThreadPriority.Highest`)
/// and puts the previous priority back when dropped.
struct HighestPriority {
    thread: HANDLE,
    old: i32,
}

impl HighestPriority {
    fn enter() -> HighestPriority {
        // SAFETY: GetCurrentThread returns a pseudo-handle valid for the calling thread.
        let thread = unsafe { GetCurrentThread() };
        let old = unsafe { GetThreadPriority(thread) };
        unsafe { SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST) };
        HighestPriority { thread, old }
    }
}

impl Drop for HighestPriority {
    fn drop(&mut self) {
        // SAFETY: same thread, same pseudo-handle.
        unsafe { SetThreadPriority(self.thread, self.old) };
    }
}

fn average(values: &[i32]) -> f64 {
    values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64
}

/// Measures the real base clock by comparing the CPU time-stamp counter (which runs
/// at BCLK x base ratio) against the ACPI power-management timer (a fixed 3.579545 MHz
/// reference that does not move with BCLK). This is how monitoring tools report a
/// BCLK such as 100.01 MHz instead of assuming 100.
pub struct BclkMeter {
    drv: Arc<dyn KernelDriver>,
    pm_timer_port: u16,
    timer_32bit: bool,
    status: String,
}

impl BclkMeter {
    /// Default window of [`measure_tsc_hz`](Self::measure_tsc_hz) / [`measure_bclk_mhz`](Self::measure_bclk_mhz).
    pub const TSC_SAMPLE_MS: i32 = 60;
    /// Default window of [`measure_bclk_from_core`](Self::measure_bclk_from_core).
    pub const CORE_SAMPLE_MS: i32 = 200;
    /// Default window of [`measure_bclk_from_cycles`](Self::measure_bclk_from_cycles).
    pub const CYCLES_SAMPLE_MS: i32 = 150;

    pub fn new(drv: Arc<dyn KernelDriver>) -> BclkMeter {
        let (pm_timer_port, timer_32bit, status) = Self::locate_pm_timer();
        BclkMeter { drv, pm_timer_port, timer_32bit, status }
    }

    pub fn is_available(&self) -> bool {
        self.pm_timer_port != 0
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    fn locate_pm_timer() -> (u16, bool, String) {
        let fadt = FirmwareTables::get(FirmwareTables::ACPI, FirmwareTables::signature("FACP"));
        parse_fadt(fadt.as_deref())
    }

    fn read_pm_timer(&self) -> Result<u32> {
        let v = self.drv.read_io_port_dword(self.pm_timer_port)?;
        Ok(if self.timer_32bit { v } else { v & 0xFFFFFF })
    }

    /// Returns the measured TSC frequency in Hz over roughly `sample_ms` milliseconds
    /// (the .NET defaults were 60 ms on CPU 0).
    pub fn measure_tsc_hz(&self, sample_ms: i32, cpu: i32) -> Result<f64> {
        if !self.is_available() {
            bail!("{}", self.status);
        }
        let mask: u32 = if self.timer_32bit { 0xFFFFFFFF } else { 0xFFFFFF };
        let target_ticks = (PM_TIMER_HZ * sample_ms as f64 / 1000.0) as u32;
        native::run_on_cpu(cpu, || -> Result<f64> {
            let _priority = HighestPriority::enter();
            // Warm up the driver path so the first sample is not slower than the last one.
            for _ in 0..4 {
                self.read_pm_timer()?;
            }

            // Bracket every port read with rdtsc and use the midpoint, so IOCTL latency cancels out.
            let mut a = rdtsc();
            let p0 = self.read_pm_timer()?;
            let mut b = rdtsc();
            let t0 = a.wrapping_add(b.wrapping_sub(a) / 2);
            let mut p1;
            let mut t1;
            let guard = Instant::now();
            loop {
                a = rdtsc();
                p1 = self.read_pm_timer()?;
                b = rdtsc();
                t1 = a.wrapping_add(b.wrapping_sub(a) / 2);
                if guard.elapsed().as_millis() as i64 > sample_ms as i64 * 5 + 200 {
                    bail!("ACPI PM timer did not advance.");
                }
                if (p1.wrapping_sub(p0) & mask) >= target_ticks {
                    break;
                }
            }
            let seconds = (p1.wrapping_sub(p0) & mask) as f64 / PM_TIMER_HZ;
            Ok(t1.wrapping_sub(t0) as f64 / seconds)
        })?
    }

    /// BCLK in MHz = TSC / base ratio (the .NET default window was 60 ms).
    pub fn measure_bclk_mhz(&self, base_ratio: i32, sample_ms: i32) -> Result<f64> {
        if base_ratio <= 0 {
            bail!("Specified argument was out of the range of valid values. (Parameter 'baseRatio')");
        }
        Ok(self.measure_tsc_hz(sample_ms, 0)? / base_ratio as f64 / 1_000_000.0)
    }

    /// BCLK measured from the core's own clock instead of the TSC (the .NET default window was 200 ms).
    ///
    /// This exists because measuring the TSC is wrong on this platform. Since Skylake the TSC is
    /// driven by a fixed reference and its rate does NOT follow a runtime BCLK change, so the
    /// TSC method reports the boot-time BCLK for ever and silently misses every adjustment.
    /// APERF/MPERF give the real core frequency as a ratio against the TSC, and dividing that by
    /// the multiplier the core is actually running gives the true BCLK.
    pub fn measure_bclk_from_core(&self, cpu_index: i32, sample_ms: i32) -> Result<Option<f64>> {
        if !self.is_available() {
            return Ok(None);
        }
        let tsc_hz = self.measure_tsc_hz(60, cpu_index)?;
        native::run_on_cpu(cpu_index, || -> Option<f64> {
            let _priority = HighestPriority::enter();
            // Hold the core busy so it settles on a steady turbo ratio; an idle core parks at
            // a low multiplier and the division below would then be against the wrong number.
            let mut x: f64 = 1.0001;
            let warm = Instant::now();
            while warm.elapsed().as_millis() < 60 {
                x = (x * 1.000001 + 1e-9).sqrt();
            }
            x = black_box(x);

            let a0 = self.drv.read_msr(MSR_IA32_APERF, -1)?;
            let m0 = self.drv.read_msr(MSR_IA32_MPERF, -1)?;
            let mut ratios: Vec<i32> = Vec::new();
            let sw = Instant::now();
            while (sw.elapsed().as_millis() as i64) < sample_ms as i64 {
                for _ in 0..20000 {
                    x = (x * 1.000001 + 1e-9).sqrt();
                }
                x = black_box(x);
                if let Some(ps) = self.drv.read_msr(MSR_IA32_PERF_STATUS, -1) {
                    ratios.push(((ps >> 8) & 0xFF) as i32);
                }
            }
            let a1 = self.drv.read_msr(MSR_IA32_APERF, -1)?;
            let m1 = self.drv.read_msr(MSR_IA32_MPERF, -1)?;
            black_box(x); // the busy loop must not be optimised away

            let d_a = a1.wrapping_sub(a0) as f64;
            let d_m = m1.wrapping_sub(m0) as f64;
            if d_m <= 0.0 || d_a <= 0.0 || ratios.is_empty() {
                return None;
            }
            let ratio = average(&ratios);
            if ratio < 4.0 {
                return None;
            }
            let core_hz = tsc_hz * (d_a / d_m);
            Some(core_hz / ratio / 1_000_000.0)
        })
    }

    /// BCLK from the core's unhalted cycle counter, timed by the ACPI timer (the .NET default
    /// window was 150 ms).
    ///
    /// The obvious measurements are all blind to a runtime BCLK change on this platform, which is
    /// why they were wrong: the TSC runs from a fixed crystal and does not follow BCLK, and
    /// MPERF increments in proportion to BCLK just as APERF does, so their ratio cancels. The
    /// fixed-function counter CPU_CLK_UNHALTED.CORE counts real core clocks, so dividing it by
    /// elapsed ACPI-timer seconds gives the true core frequency, and that over the running
    /// multiplier is the true BCLK. The counter only advances while the core is unhalted, hence
    /// the busy loop.
    pub fn measure_bclk_from_cycles(&self, cpu_index: i32, sample_ms: i32) -> Result<Option<f64>> {
        if !self.is_available() {
            return Ok(None);
        }
        let mask: u32 = if self.timer_32bit { 0xFFFFFFFF } else { 0xFFFFFF };
        let target_ticks = (PM_TIMER_HZ * sample_ms as f64 / 1000.0) as u32;
        native::run_on_cpu(cpu_index, || -> Result<Option<f64>> {
            let Some(ctrl_old) = self.drv.read_msr(IA32_FIXED_CTR_CTRL, -1) else { return Ok(None) };
            let Some(global_old) = self.drv.read_msr(IA32_PERF_GLOBAL_CTRL, -1) else { return Ok(None) };
            let priority = HighestPriority::enter();
            let result = (|| -> Result<Option<f64>> {
                // fixed counter 1 = CPU_CLK_UNHALTED.CORE; bits 7:4 are its control, OS+USR = 0x3
                self.drv.write_msr(IA32_FIXED_CTR_CTRL, (ctrl_old & !0xF0u64) | 0x30u64, -1);
                self.drv.write_msr(IA32_PERF_GLOBAL_CTRL, global_old | (1u64 << 33), -1);

                let mut x: f64 = 1.0001;
                for _ in 0..500_000 {
                    x = x * 1.0000001 + 1e-9; // reach turbo
                }
                x = black_box(x);

                let Some(c0) = self.drv.read_msr(IA32_FIXED_CTR1, -1) else { return Ok(None) };
                let p0 = self.read_pm_timer()?;
                let mut ratios: Vec<i32> = Vec::new();
                let mut p1;
                loop {
                    for _ in 0..50_000 {
                        x = x * 1.0000001 + 1e-9;
                    }
                    x = black_box(x);
                    if let Some(ps) = self.drv.read_msr(MSR_IA32_PERF_STATUS, -1) {
                        ratios.push(((ps >> 8) & 0xFF) as i32);
                    }
                    p1 = self.read_pm_timer()?;
                    if (p1.wrapping_sub(p0) & mask) >= target_ticks {
                        break;
                    }
                }
                let Some(c1) = self.drv.read_msr(IA32_FIXED_CTR1, -1) else { return Ok(None) };
                black_box(x);

                let seconds = (p1.wrapping_sub(p0) & mask) as f64 / PM_TIMER_HZ;
                let cycles = c1.wrapping_sub(c0) as f64;
                if cycles <= 0.0 || seconds <= 0.0 || ratios.is_empty() {
                    return Ok(None);
                }
                let ratio = average(&ratios);
                if ratio < 4.0 {
                    return Ok(None);
                }
                Ok(Some(cycles / seconds / ratio / 1_000_000.0))
            })();
            // finally: leave the counters as we found them, then the thread priority
            self.drv.write_msr(IA32_PERF_GLOBAL_CTRL, global_old, -1);
            self.drv.write_msr(IA32_FIXED_CTR_CTRL, ctrl_old, -1);
            drop(priority);
            result
        })?
    }

    #[allow(dead_code)]
    fn read_cmos(&self, reg: u8) -> Result<u8> {
        self.drv.write_io_port_byte(0x70, reg)?;
        self.drv.read_io_port_byte(0x71)
    }

    /// Seconds from the RTC, read outside its update window so the value is settled.
    #[allow(dead_code)]
    fn rtc_seconds(&self) -> Result<u8> {
        let guard = Instant::now();
        while (self.read_cmos(0x0A)? & 0x80) != 0 && guard.elapsed().as_millis() < 50 {}
        self.read_cmos(0x00)
    }
}

/// Locates the ACPI PM timer in a FADT (`FACP`) table: (I/O port or 0, 32-bit timer, status).
fn parse_fadt(fadt: Option<&[u8]>) -> (u16, bool, String) {
    let Some(fadt) = fadt.filter(|f| f.len() >= 80) else {
        return (0, false, "ACPI FADT not available".to_string());
    };
    let u32_at = |o: usize| u32::from_le_bytes([fadt[o], fadt[o + 1], fadt[o + 2], fadt[o + 3]]);
    let mut pm_tmr_blk = u32_at(76);
    let flags = if fadt.len() >= 116 { u32_at(112) } else { 0 };
    let ext = ((flags >> 8) & 1) == 1;
    if pm_tmr_blk == 0 && fadt.len() >= 220 {
        // X_PM_TMR_BLK generic address structure at 208: space id, bit width, bit offset, access size, address (8 bytes)
        let space_id = fadt[208];
        let mut a = [0u8; 8];
        a.copy_from_slice(&fadt[212..220]);
        let addr = u64::from_le_bytes(a);
        if space_id == 1 && addr != 0 && addr < 0x10000 {
            pm_tmr_blk = addr as u32;
        }
    }
    if pm_tmr_blk == 0 || pm_tmr_blk > 0xFFFF {
        return (0, ext, "ACPI PM timer not in I/O space".to_string());
    }
    (pm_tmr_blk as u16, ext, format!("ACPI PM timer at I/O 0x{pm_tmr_blk:04X}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fadt_pm_timer_location() {
        assert_eq!(parse_fadt(None), (0, false, "ACPI FADT not available".to_string()));
        assert_eq!(parse_fadt(Some(&[0u8; 79])).2, "ACPI FADT not available");

        let mut f = vec![0u8; 244];
        f[76..80].copy_from_slice(&0x1808u32.to_le_bytes());
        f[112..116].copy_from_slice(&(1u32 << 8).to_le_bytes());
        assert_eq!(parse_fadt(Some(&f)), (0x1808, true, "ACPI PM timer at I/O 0x1808".to_string()));

        // PM_TMR_BLK empty: fall back to X_PM_TMR_BLK when it is in system I/O space.
        f[76..80].copy_from_slice(&0u32.to_le_bytes());
        f[112..116].copy_from_slice(&0u32.to_le_bytes());
        f[208] = 1;
        f[212..220].copy_from_slice(&0x408u64.to_le_bytes());
        assert_eq!(parse_fadt(Some(&f)), (0x408, false, "ACPI PM timer at I/O 0x0408".to_string()));

        f[208] = 0; // system memory: not usable
        assert_eq!(parse_fadt(Some(&f)), (0, false, "ACPI PM timer not in I/O space".to_string()));
    }
}
