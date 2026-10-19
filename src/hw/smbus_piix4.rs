//! AMD FCH SMBus host controller (the PIIX4-compatible block at bus 0, device 20, function 0,
//! vendor 0x1022 device 0x790B on every Zen platform), driven through I/O ports. Its register
//! window sits at I/O 0xB00 on AM4 and AM5 boards; the FCH has no BAR for it, the base lives
//! in the PM MMIO block, and every BIOS seen so far leaves the default. The window is checked
//! before use: a status register that reads 0xFF means nothing is there.
//!
//! The FCH can route this controller to one of several physical SMBus ports through a mux in
//! the PM MMIO block. Changing it needs an MMIO write the WinRing0 driver cannot do, so this
//! type talks to whichever port the BIOS left selected; the DIMMs are on it on the boards
//! tried so far, and if they are not the probe simply finds no SPD hubs and says so.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;

use super::native::lock;
use super::smbus_i801::SmbusGlobalMutex;
use super::{KernelDriver, Smbus};

const BUS: u8 = 0;
const DEVICE: u8 = 0x14;
const FUNCTION: u8 = 0;
const VENDOR_AMD: u16 = 0x1022;
const DEVICE_KERN_CZ: u16 = 0x790B;
const DEFAULT_BASE: u16 = 0x0B00;

const HST_STS: u16 = 0x00;
const HST_CNT: u16 = 0x02;
const HST_CMD: u16 = 0x03;
const HST_ADD: u16 = 0x04;
const HST_D0: u16 = 0x05;
const HST_D1: u16 = 0x06;
const STS_BUSY: u8 = 0x01;
const STS_INTR: u8 = 0x02;
const STS_DEV_ERR: u8 = 0x04;
const STS_BUS_ERR: u8 = 0x08;
const STS_FAILED: u8 = 0x10;
const STS_ERROR_FLAGS: u8 = STS_DEV_ERR | STS_BUS_ERR | STS_FAILED;
const CNT_START: u8 = 0x40;
const PROTO_BYTE_DATA: u8 = 0x08;
const PROTO_WORD_DATA: u8 = 0x0C;

/// AMD FCH SMBus host controller (PIIX4-compatible, B0:D20:F0), driven through I/O ports.
pub struct SmbusPiix4 {
    drv: Arc<dyn KernelDriver>,
    base: u16,
    description: String,
    global_mutex: Option<SmbusGlobalMutex>,
    lock: Mutex<()>,
}

/// `Thread.SpinWait(n)`.
fn spin_wait(iterations: u32) {
    for _ in 0..iterations {
        std::hint::spin_loop();
    }
}

impl SmbusPiix4 {
    pub fn base_address(&self) -> u16 {
        self.base
    }

    fn new(drv: Arc<dyn KernelDriver>, base_addr: u16, revision: u8) -> SmbusPiix4 {
        let description = format!("AMD FCH SMBus (0x{DEVICE_KERN_CZ:04X} rev 0x{revision:02X}) at I/O 0x{base_addr:04X}");
        // The same mutex HWiNFO, LibreHardwareMonitor and ZenStates take around SMBus transactions.
        SmbusPiix4 { drv, base: base_addr, description, global_mutex: SmbusGlobalMutex::create(), lock: Mutex::new(()) }
    }

    /// Returns `None` when the FCH SMBus controller is absent or its I/O window does not answer.
    pub fn try_create(drv: Arc<dyn KernelDriver>) -> (Option<SmbusPiix4>, String) {
        let id = match drv.read_pci_config(BUS, DEVICE, FUNCTION, 0) {
            Some(id) if (id & 0xFFFF) == VENDOR_AMD as u32 => id,
            _ => return (None, "AMD FCH SMBus controller (B0:D20:F0) not visible.".to_string()),
        };
        if (id >> 16) != DEVICE_KERN_CZ as u32 {
            return (None, format!("B0:D20:F0 is AMD device 0x{:04X}, not the FCH SMBus controller.", id >> 16));
        }
        let rev = drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x08).unwrap_or(0);
        // The controller's I/O decode must be on; the BIOS enables it for its own SPD reads.
        let cmd = drv.read_pci_config(BUS, DEVICE, FUNCTION, 0x04).unwrap_or(0);
        if (cmd & 1) == 0 {
            return (None, "AMD FCH SMBus controller has I/O decoding disabled (PCI command bit 0).".to_string());
        }
        // The .NET build let a driver error here propagate; the status carries its message instead.
        let sts = match drv.read_io_port_byte(DEFAULT_BASE + HST_STS) {
            Ok(s) => s,
            Err(e) => return (None, e.to_string()),
        };
        if sts == 0xFF {
            return (
                None,
                format!(
                    "AMD FCH SMBus window at I/O 0x{DEFAULT_BASE:04X} does not answer (status 0xFF); the BIOS relocated it."
                ),
            );
        }
        let bus = SmbusPiix4::new(drv, DEFAULT_BASE, (rev & 0xFF) as u8);
        let status = bus.description.clone();
        (Some(bus), status)
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

    /// One transaction. The controller latches its status bits; they are written back to clear them.
    /// A driver I/O error ends it as a failed transfer (`None`).
    fn transaction(&self, address: u8, command: u8, protocol: u8, read: bool, data: u16) -> Option<u16> {
        let _g = lock(&self.lock);
        if !self.acquire() {
            return None;
        }
        let r = self.transaction_core(address, command, protocol, read, data);
        self.release();
        r.unwrap_or(None)
    }

    fn transaction_core(&self, address: u8, command: u8, protocol: u8, read: bool, data: u16) -> Result<Option<u16>> {
        let mut result: u16 = 0;
        let mut sts = self.inp(HST_STS)?;
        if sts != 0 {
            self.out(HST_STS, sts)?;
            sts = self.inp(HST_STS)?;
            if sts != 0 {
                return Ok(None); // stuck busy or an error that will not clear: leave it alone
            }
        }

        self.out(HST_ADD, (address << 1) | if read { 1 } else { 0 })?;
        self.out(HST_CMD, command)?;
        if !read {
            self.out(HST_D0, (data & 0xFF) as u8)?;
            if protocol == PROTO_WORD_DATA {
                self.out(HST_D1, (data >> 8) as u8)?;
            }
        }
        self.out(HST_CNT, protocol)?;
        self.out(HST_CNT, protocol | CNT_START)?;

        let sw = Instant::now();
        loop {
            sts = self.inp(HST_STS)?;
            if (sts & STS_BUSY) == 0 {
                break;
            }
            spin_wait(100);
            if sw.elapsed().as_millis() >= 64 {
                break;
            }
        }

        let ok = (sts & (STS_BUSY | STS_ERROR_FLAGS)) == 0 && (sts & STS_INTR) != 0;
        if ok && read {
            result = self.inp(HST_D0)? as u16;
            if protocol == PROTO_WORD_DATA {
                result |= (self.inp(HST_D1)? as u16) << 8;
            }
        }
        if sts != 0 {
            self.out(HST_STS, sts)?;
        }
        Ok(if ok { Some(result) } else { None })
    }
}

impl Smbus for SmbusPiix4 {
    fn description(&self) -> String {
        self.description.clone()
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
