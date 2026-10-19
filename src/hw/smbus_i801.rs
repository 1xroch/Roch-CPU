//! Intel PCH SMBus host controller (the i801 family: bus 0, device 31, function 4)
//! driven through I/O ports. Used to reach the DDR5 SPD hubs and PMICs on the DIMMs.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use super::native::{self, OwnedHandle, lock, wide};
use super::{KernelDriver, Smbus};

/// The named mutex HWiNFO, LibreHardwareMonitor and ZenStates take around SMBus transactions
/// (the .NET build's `new Mutex(false, @"Global\Access_SMBUS.HTP.Method")`).
pub(crate) struct SmbusGlobalMutex(OwnedHandle);

impl SmbusGlobalMutex {
    pub(crate) const NAME: &'static str = r"Global\Access_SMBUS.HTP.Method";

    /// `None` when the mutex cannot be created or opened (the .NET build then ran without it).
    pub(crate) fn create() -> Option<SmbusGlobalMutex> {
        let name = wide(Self::NAME);
        // SAFETY: FFI call with a valid NUL-terminated name; a null return is handled.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() { None } else { Some(SmbusGlobalMutex(OwnedHandle(h))) }
    }

    /// `WaitOne(500)`; an abandoned mutex is owned by the caller and counts as acquired.
    pub(crate) fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: the handle is a live mutex handle owned by self.
        let r = unsafe { WaitForSingleObject(self.0.0, timeout_ms) };
        r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
    }

    pub(crate) fn release(&self) {
        // SAFETY: the handle is a live mutex handle owned by self; failure is ignored like the C# catch.
        unsafe { ReleaseMutex(self.0.0) };
    }
}

/// Intel PCH SMBus host controller (the i801 family: bus 0, device 31, function 4)
/// driven through I/O ports. Used to reach the DDR5 SPD hubs and PMICs on the DIMMs.
pub struct SmbusI801 {
    drv: Arc<dyn KernelDriver>,
    base: u16,
    pci_id: u32,
    global_mutex: Option<SmbusGlobalMutex>,
    lock: Mutex<()>,
}

const BUS: u8 = 0;
const DEVICE: u8 = 31;
const FUNCTION: u8 = 4;

// Register offsets from the I/O base
const HST_STS: u16 = 0;
const HST_CNT: u16 = 2;
const HST_CMD: u16 = 3;
const XMIT_SLVA: u16 = 4;
const HST_D0: u16 = 5;
const HST_D1: u16 = 6;
const AUX_CTL: u16 = 0xD;

// HST_STS bits
const STS_BUSY: u8 = 0x01;
const STS_INTR: u8 = 0x02;
const STS_DEV_ERR: u8 = 0x04;
const STS_BUS_ERR: u8 = 0x08;
const STS_FAILED: u8 = 0x10;
const STS_INUSE: u8 = 0x40;
const STS_ERROR_FLAGS: u8 = STS_DEV_ERR | STS_BUS_ERR | STS_FAILED;
const STS_ALL_FLAGS: u8 = STS_BUSY | STS_INTR | STS_ERROR_FLAGS;

// HST_CNT bits
const CNT_START: u8 = 0x40;
const CNT_KILL: u8 = 0x02;
const PROTO_BYTE_DATA: u8 = 2 << 2;
const PROTO_WORD_DATA: u8 = 3 << 2;

const PROTO_BLOCK_DATA: u8 = 5 << 2;
const STS_BYTE_DONE: u8 = 0x80;
const BLOCK_DB: u16 = 0x07;

/// `Thread.SpinWait(n)`.
fn spin_wait(iterations: u32) {
    for _ in 0..iterations {
        std::hint::spin_loop();
    }
}

impl SmbusI801 {
    pub fn base_address(&self) -> u16 {
        self.base
    }

    pub fn pci_id(&self) -> u32 {
        self.pci_id
    }

    fn new(drv: Arc<dyn KernelDriver>, base_addr: u16, pci_id: u32) -> SmbusI801 {
        SmbusI801 { drv, base: base_addr, pci_id, global_mutex: SmbusGlobalMutex::create(), lock: Mutex::new(()) }
    }

    /// Returns `None` if the PCH SMBus controller is absent, hidden by the BIOS, or has no I/O window.
    pub fn try_create(drv: Arc<dyn KernelDriver>) -> (Option<SmbusI801>, String) {
        let id = match drv.read_pci_config(BUS, DEVICE, FUNCTION, 0) {
            Some(id) if (id & 0xFFFF) == 0x8086 => id,
            _ => {
                return (
                    None,
                    "PCH SMBus controller (B0:D31:F4) not visible - hidden by BIOS or unsupported chipset.".to_string(),
                );
            }
        };
        let bar = drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x20).unwrap_or(0);
        let base_addr = (bar & 0xFFFE) as u16;
        if (bar & 1) == 0 || base_addr == 0 || base_addr == 0xFFFE {
            return (None, "PCH SMBus controller has no I/O base (SMB_BASE).".to_string());
        }
        let hstcfg = drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x40).unwrap_or(0);
        if (hstcfg & 1) == 0 {
            return (None, "PCH SMBus host interface is disabled (HST_EN=0).".to_string());
        }
        let status = format!("PCH SMBus 0x{:04X} at I/O 0x{:04X}", id >> 16, base_addr);
        (Some(SmbusI801::new(drv, base_addr, id)), status)
    }

    fn inp(&self, off: u16) -> Result<u8> {
        self.drv.read_io_port_byte(self.base.wrapping_add(off))
    }

    fn out(&self, off: u16, v: u8) -> Result<()> {
        self.drv.write_io_port_byte(self.base.wrapping_add(off), v)
    }

    fn acquire(&self) -> bool {
        match &self.global_mutex {
            Some(m) => m.wait(500),
            None => true,
        }
    }

    fn release(&self) {
        if let Some(m) = &self.global_mutex {
            m.release();
        }
    }

    /// One transaction. A driver I/O error ends it as a failed transfer (`None`).
    fn transaction(&self, address: u8, command: u8, protocol: u8, read: bool, data: u16) -> Option<u16> {
        let _g = lock(&self.lock);
        if !self.acquire() {
            return None;
        }
        let r = self.transaction_core(address, command, protocol, read, data);
        self.release();
        match r {
            Ok(v) => v,
            Err(_) => None,
        }
    }

    fn transaction_core(&self, address: u8, command: u8, protocol: u8, read: bool, data: u16) -> Result<Option<u16>> {
        let mut result: u16 = 0;
        let mut sts = self.inp(HST_STS)?;
        if (sts & STS_BUSY) != 0 {
            self.out(HST_CNT, CNT_KILL)?;
            native::sleep_ms(1);
            self.out(HST_CNT, 0)?;
            sts = self.inp(HST_STS)?;
            if (sts & STS_BUSY) != 0 {
                return Ok(None);
            }
        }
        if (sts & STS_ALL_FLAGS) != 0 {
            self.out(HST_STS, sts & STS_ALL_FLAGS)?;
        }

        self.out(XMIT_SLVA, (address << 1) | if read { 1 } else { 0 })?;
        self.out(HST_CMD, command)?;
        if !read {
            self.out(HST_D0, (data & 0xFF) as u8)?;
            if protocol == PROTO_WORD_DATA {
                self.out(HST_D1, (data >> 8) as u8)?;
            }
        }
        self.out(AUX_CTL, 0)?; // no PEC, no 32-byte buffer
        self.out(HST_CNT, protocol | CNT_START)?;

        let sw = Instant::now();
        loop {
            sts = self.inp(HST_STS)?;
            if (sts & STS_BUSY) == 0 && (sts & (STS_INTR | STS_ERROR_FLAGS)) != 0 {
                break;
            }
            spin_wait(50);
            if sw.elapsed().as_millis() >= 100 {
                break;
            }
        }

        let ok = (sts & STS_ERROR_FLAGS) == 0 && (sts & STS_INTR) != 0;
        if ok && read {
            result = self.inp(HST_D0)? as u16;
            if protocol == PROTO_WORD_DATA {
                result |= (self.inp(HST_D1)? as u16) << 8;
            }
        }
        self.out(HST_STS, sts & STS_ALL_FLAGS)?; // clear status
        self.out(HST_STS, STS_INUSE)?; // release the in-use semaphore
        Ok(if ok { Some(result) } else { None })
    }

    // NOTE: a QuickCommand() scan lived here. Driving a bare address phase across every
    // address hung this board's SMBus - afterwards no device answered at all, including the DDR5
    // SPD hubs, and only a power cycle brought it back. Detect devices with a byte read instead.

    /// SMBus block read. Returns the payload, or `None` when the device does not answer.
    pub fn block_read(&self, address: u8, command: u8) -> Option<Vec<u8>> {
        let _g = lock(&self.lock);
        if !self.acquire() {
            return None;
        }
        let r = self.block_read_core(address, command);
        self.release();
        r.unwrap_or_default()
    }

    fn block_read_core(&self, address: u8, command: u8) -> Result<Option<Vec<u8>>> {
        let mut sts = self.inp(HST_STS)?;
        if (sts & STS_BUSY) != 0 {
            self.out(HST_CNT, CNT_KILL)?;
            native::sleep_ms(1);
            self.out(HST_CNT, 0)?;
            if (self.inp(HST_STS)? & STS_BUSY) != 0 {
                return Ok(None);
            }
        }
        if (sts & STS_ALL_FLAGS) != 0 {
            self.out(HST_STS, sts & STS_ALL_FLAGS)?;
        }

        self.out(XMIT_SLVA, (address << 1) | 1)?;
        self.out(HST_CMD, command)?;
        self.out(AUX_CTL, 0)?; // byte-at-a-time, no 32-byte buffer
        self.out(HST_CNT, PROTO_BLOCK_DATA | CNT_START)?;

        let sw = Instant::now();
        let wait = |flag: u8| -> Result<bool> {
            while sw.elapsed().as_millis() < 100 {
                let s = self.inp(HST_STS)?;
                if (s & STS_ERROR_FLAGS) != 0 {
                    return Ok(false);
                }
                if (s & flag) != 0 {
                    return Ok(true);
                }
                spin_wait(50);
            }
            Ok(false)
        };

        if !wait(STS_BYTE_DONE)? {
            self.abort();
            return Ok(None);
        }
        let count = self.inp(HST_D0)? as i32;
        if !(1..=32).contains(&count) {
            self.abort();
            return Ok(None);
        }
        let mut data = vec![0u8; count as usize];
        for i in 0..count {
            data[i as usize] = self.inp(BLOCK_DB)?;
            self.out(HST_STS, STS_BYTE_DONE)?; // advance to the next byte
            if i < count - 1 && !wait(STS_BYTE_DONE)? {
                self.abort();
                return Ok(None);
            }
        }
        sts = self.inp(HST_STS)?;
        self.out(HST_STS, sts & STS_ALL_FLAGS)?;
        self.out(HST_STS, STS_INUSE)?;
        Ok(Some(data))
    }

    /// Bring the host controller back to idle. A transaction that was killed part way, or an
    /// address phase a device never completed, leaves BUSY or the in-use semaphore set and every
    /// later transfer then fails. This is the recovery path for that.
    pub fn reset(&self) -> Result<String> {
        let _g = lock(&self.lock);
        let before = self.inp(HST_STS)?;
        for _ in 0..3 {
            self.out(HST_CNT, CNT_KILL)?;
            native::sleep_ms(2);
            self.out(HST_CNT, 0)?;
            native::sleep_ms(2);
            self.out(HST_STS, STS_ALL_FLAGS | STS_BYTE_DONE)?; // clear every latched flag
            self.out(HST_STS, STS_INUSE)?; // hand the in-use semaphore back
            native::sleep_ms(2);
            if (self.inp(HST_STS)? & STS_BUSY) == 0 {
                break;
            }
        }
        let after = self.inp(HST_STS)?;
        Ok(format!("HST_STS 0x{before:02X} -> 0x{after:02X}"))
    }

    /// Soft-reset the host controller through HSTCFG (PCI 0:31:4 offset 0x40, bit 3 = SSRESET).
    /// Needed when HOST_BUSY latches high and a KILL will not clear it, which is what a killed
    /// or never-completed address phase leaves behind.
    pub fn hard_reset(&self) -> Result<String> {
        let _g = lock(&self.lock);
        let Some(cfg) = self.drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x40) else {
            return Ok("could not read HSTCFG".to_string());
        };
        let before = cfg;
        self.drv.write_pci_config(BUS, DEVICE, FUNCTION, 0x40, cfg | 0x08); // SSRESET
        native::sleep_ms(10);
        self.drv.write_pci_config(BUS, DEVICE, FUNCTION, 0x40, cfg & !0x08u32); // release, HST_EN preserved
        native::sleep_ms(10);
        self.out(HST_STS, STS_ALL_FLAGS | STS_BYTE_DONE)?;
        self.out(HST_STS, STS_INUSE)?;
        native::sleep_ms(10);
        let after = self.drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x40).unwrap_or(0);
        let sts = self.inp(HST_STS)?;
        Ok(format!("HSTCFG 0x{before:02X} -> 0x{after:02X}, HST_STS now 0x{sts:02X}"))
    }

    fn abort(&self) {
        let _ = (|| -> Result<()> {
            self.out(HST_CNT, CNT_KILL)?;
            native::sleep_ms(1);
            self.out(HST_CNT, 0)?;
            self.out(HST_STS, STS_ALL_FLAGS)?;
            self.out(HST_STS, STS_INUSE)?;
            Ok(())
        })();
    }
}

impl Smbus for SmbusI801 {
    fn description(&self) -> String {
        format!("PCH SMBus 0x{:04X} at I/O 0x{:04X}", self.pci_id >> 16, self.base)
    }

    fn read_byte(&self, address: u8, command: u8) -> Option<u8> {
        self.transaction(address, command, PROTO_BYTE_DATA, true, 0).map(|r| r as u8)
    }

    fn write_byte(&self, address: u8, command: u8, value: u8) -> bool {
        self.transaction(address, command, PROTO_BYTE_DATA, false, value as u16).is_some()
    }

    fn read_word(&self, address: u8, command: u8) -> Option<u16> {
        self.transaction(address, command, PROTO_WORD_DATA, true, 0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
