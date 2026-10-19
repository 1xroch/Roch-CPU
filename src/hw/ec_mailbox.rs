//! The I2C mailbox in MSI's embedded controller.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;

use super::native;
use super::startup_io_diagnostics;
use super::super_io::{SuperIo, SuperIoKind};

// Mailbox registers in EC space.
const R_COMMAND: u16 = 0x463; // see the CMD_ constants
const R_ADDRESS: u16 = 0x465; // I2C address, 8-bit form (0xD2 -> 7-bit 0x69)
const R_REGISTER: u16 = 0x466; // register index inside the target device
const R_WDATA_LO: u16 = 0x470;
const R_WDATA_HI: u16 = 0x471;
const R_RDATA_LO: u16 = 0x4B0;
const R_RDATA_HI: u16 = 0x4B1;
const R_DOORBELL: u16 = 0x460;

const CMD_WRITE_BYTE: u8 = 0x02;
const CMD_WRITE_WORD: u8 = 0x03;
const CMD_READ_BYTE: u8 = 0x82;
const CMD_READ_WORD: u8 = 0x83;

// The doorbell holds 0x80 at rest. The caller sets a go bit and the EC clears it when the
// transaction on the I2C bus has finished. A read needs two phases - 0x88 runs the transfer,
// 0xC0 latches the result into 0x4B0/0x4B1 - while a write needs only the 0xC0 phase, because
// its data is already staged in 0x470/0x471.
const GO_TRANSFER: u8 = 0x88;
const GO_LATCH: u8 = 0xC0;
const DOORBELL_IDLE: u8 = 0x80;
const DOORBELL_TIMEOUT_MS: u128 = 250;

/// The I2C mailbox in MSI's embedded controller.
///
/// Several things worth setting on these boards - the base clock generator, the regulator that
/// feeds VDD2 - sit on an I2C bus the EC owns, not on the PCH SMBus, so no amount of work on the
/// SMBus controller reaches them. The EC exposes a mailbox in its own address space that will run
/// one transaction on that bus and hand back the result, and that mailbox is the only route to
/// those devices from software.
///
/// The register roles and the handshake were captured from the vendor tool, by forwarding its
/// kernel driver's exported entry points through a logging proxy and recording real changes. They
/// are observations of an interface, not guesses, and they are replayed here rather than
/// reconstructed.
///
/// Cheap to create. A clone shares the transaction lock (as sharing one C# instance does); a
/// newly constructed mailbox, or [`EcMailbox::with_strict_io`], has its own.
#[derive(Clone)]
pub struct EcMailbox {
    sio: Arc<SuperIo>,
    strict_io: bool,
    lock: Arc<Mutex<()>>,
}

impl EcMailbox {
    pub fn new(sio: Arc<SuperIo>) -> EcMailbox {
        Self::new_with_strict_io(sio, false)
    }

    /// C# `new EcMailbox(sio, strictIo)`.
    pub fn new_with_strict_io(sio: Arc<SuperIo>, strict_io: bool) -> EcMailbox {
        EcMailbox { sio, strict_io, lock: Arc::new(Mutex::new(())) }
    }

    pub fn with_strict_io(&self) -> EcMailbox {
        Self::new_with_strict_io(self.sio.clone(), true)
    }

    pub fn super_io(&self) -> &Arc<SuperIo> {
        &self.sio
    }

    pub fn is_strict_io(&self) -> bool {
        self.strict_io
    }

    fn read_raw(&self, address: u16) -> Result<u8> {
        let r = if self.strict_io { self.sio.read_raw_strict(address) } else { Ok(self.sio.read_raw(address)) };
        match r {
            Ok(value) => {
                startup_io_diagnostics::observe_ec_read(address, value);
                Ok(value)
            }
            Err(ex) => {
                startup_io_diagnostics::observe_ec_failure(address, false, &ex);
                Err(ex)
            }
        }
    }

    fn write_raw(&self, address: u16, value: u8) -> Result<bool> {
        let r = if self.strict_io { self.sio.write_raw_strict(address, value) } else { Ok(self.sio.write_raw(address, value)) };
        match r {
            Ok(ok) => {
                startup_io_diagnostics::observe_ec_write(address, value, ok);
                Ok(ok)
            }
            Err(ex) => {
                startup_io_diagnostics::observe_ec_failure(address, true, &ex);
                Err(ex)
            }
        }
    }

    /// Only MSI's EC-space Nuvoton parts carry this mailbox.
    pub fn is_supported(sio: Option<&SuperIo>) -> bool {
        matches!(sio, Some(s) if s.kind() == SuperIoKind::NuvotonEc)
    }

    // ---------------------------------------------------------------- handshake
    fn wait_doorbell(&self) -> Result<bool> {
        let sw = Instant::now();
        loop {
            if self.read_raw(R_DOORBELL)? == DOORBELL_IDLE {
                return Ok(true);
            }
            native::sleep_ms(1);
            if sw.elapsed().as_millis() >= DOORBELL_TIMEOUT_MS {
                break;
            }
        }
        Ok(false)
    }

    fn ring(&self, go: u8) -> Result<bool> {
        Ok(self.write_raw(R_DOORBELL, go)? && self.wait_doorbell()?)
    }

    fn stage(&self, command: u8, address: u8, register: u8) -> Result<bool> {
        // The vendor tool reads each of these back after writing it. Keeping that check means a
        // mailbox busy with someone else's transaction is noticed before the doorbell is rung.
        if !self.write_raw(R_COMMAND, command)? || self.read_raw(R_COMMAND)? != command {
            return Ok(false);
        }
        if !self.write_raw(R_ADDRESS, address)? || self.read_raw(R_ADDRESS)? != address {
            return Ok(false);
        }
        if !self.write_raw(R_REGISTER, register)? || self.read_raw(R_REGISTER)? != register {
            return Ok(false);
        }
        Ok(true)
    }

    /// `Some(read)` when the transaction completed (`read` is 0 for writes), `None` when it did not.
    fn transact(&self, command: u8, address: u8, register: u8, write: Option<u16>) -> Result<Option<u16>> {
        let _guard = native::lock(&self.lock);
        if !self.wait_doorbell()? {
            return Ok(None);
        }
        if !self.stage(command, address, register)? {
            return Ok(None);
        }

        if let Some(w) = write {
            let lo = w as u8;
            if !self.write_raw(R_WDATA_LO, lo)? || self.read_raw(R_WDATA_LO)? != lo {
                return Ok(None);
            }
            if command == CMD_WRITE_WORD {
                let hi = (w >> 8) as u8;
                if !self.write_raw(R_WDATA_HI, hi)? || self.read_raw(R_WDATA_HI)? != hi {
                    return Ok(None);
                }
            }
            return Ok(if self.ring(GO_LATCH)? { Some(0) } else { None });
        }

        if !self.ring(GO_TRANSFER)? {
            return Ok(None);
        }
        if !self.ring(GO_LATCH)? {
            return Ok(None);
        }
        let mut read = self.read_raw(R_RDATA_LO)? as u16;
        if command == CMD_READ_WORD {
            read |= (self.read_raw(R_RDATA_HI)? as u16) << 8;
        }
        Ok(Some(read))
    }

    // ---------------------------------------------------------------- transfers
    /// Reads one byte from a device on the EC's I2C bus.
    pub fn read_byte(&self, address: u8, register: u8) -> Result<Option<u8>> {
        let value = self.transact(CMD_READ_BYTE, address, register, None)?.map(|v| v as u8);
        startup_io_diagnostics::observe_register_read(address, register, value.map(u16::from), false);
        Ok(value)
    }

    /// Reads one 16-bit register from a device on the EC's I2C bus.
    pub fn read_word(&self, address: u8, register: u8) -> Result<Option<u16>> {
        let value = self.transact(CMD_READ_WORD, address, register, None)?;
        startup_io_diagnostics::observe_register_read(address, register, value, true);
        Ok(value)
    }

    /// Writes one byte to a device on the EC's I2C bus.
    pub fn write_byte(&self, address: u8, register: u8, value: u8) -> Result<bool> {
        Ok(self.transact(CMD_WRITE_BYTE, address, register, Some(value as u16))?.is_some())
    }

    /// Writes one 16-bit register to a device on the EC's I2C bus.
    pub fn write_word(&self, address: u8, register: u8, value: u16) -> Result<bool> {
        Ok(self.transact(CMD_WRITE_WORD, address, register, Some(value))?.is_some())
    }
}

/// Shared test fakes: the simulated MSI EC + Renesas regulator (C# `EcFakeDriver`).
#[cfg(test)]
pub(crate) mod fakes {
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    use anyhow::{Result, anyhow, bail};

    use crate::hw::KernelDriver;
    use crate::hw::native;
    use crate::hw::oc_mailbox::{OcMailbox, VfDomainSettings};
    use crate::hw::super_io::{NamedMutex, SuperIo};

    const BASE: u16 = 0x0A20;

    pub(crate) struct EcFakeState {
        ec: HashMap<u16, u8>,
        pub page: u8,
        pub ec_selected_page: u8,
        ec_index: u8,
        pub fail_page_result_once: bool,
        pub fail_mode_result_once: bool,
        pub fail_target_result_once: bool,
        pub fail_secondary_result_once: bool,
        pub fail_command_result_once: bool,
        pub fail_target_write_once: bool,
        pub fail_page_select_once: bool,
        pub fail_page_restore_once: bool,
        pub ignore_page_select_once: bool,
        pub ignore_page_restore_once: bool,
        pub core_page_drift_on_secondary_once: bool,
        pub page_result_reads: i32,
        pub fail_page_result_at: i32,
        pub io_reads: i32,
        pub io_writes: i32,
        pub regulator_reads: Vec<u8>,
        pub driver_open: bool,
        pub regulator_writes: Vec<(u8, u16)>,
        /// Answers the Intel OC mailbox's `CMD_READ_VF` (model tests only).
        pub simulate_mailbox_reads: bool,
        /// Every OC-mailbox command byte written while `simulate_mailbox_reads` is set.
        pub mailbox_commands: Vec<u8>,
        mailbox_response: u64,
        pub mode: u16,
        pub target: u16,
        pub secondary: u16,
    }

    impl EcFakeState {
        fn get(&self, address: u16) -> u8 {
            self.ec.get(&address).copied().unwrap_or(0)
        }
    }

    pub(crate) struct EcFakeDriver {
        state: Mutex<EcFakeState>,
    }

    impl EcFakeDriver {
        pub(crate) fn new() -> Arc<EcFakeDriver> {
            Self::with(|_| {})
        }

        pub(crate) fn with(setup: impl FnOnce(&mut EcFakeState)) -> Arc<EcFakeDriver> {
            let mut ec = HashMap::new();
            ec.insert(0x460u16, 0x80u8);
            let mut s = EcFakeState {
                ec,
                page: 1,
                ec_selected_page: 0xFF,
                ec_index: 0,
                fail_page_result_once: false,
                fail_mode_result_once: false,
                fail_target_result_once: false,
                fail_secondary_result_once: false,
                fail_command_result_once: false,
                fail_target_write_once: false,
                fail_page_select_once: false,
                fail_page_restore_once: false,
                ignore_page_select_once: false,
                ignore_page_restore_once: false,
                core_page_drift_on_secondary_once: false,
                page_result_reads: 0,
                fail_page_result_at: 0,
                io_reads: 0,
                io_writes: 0,
                regulator_reads: Vec::new(),
                driver_open: true,
                regulator_writes: Vec::new(),
                simulate_mailbox_reads: false,
                mailbox_commands: Vec::new(),
                mailbox_response: 0,
                mode: 0x30,
                target: 1280,
                secondary: 0,
            };
            setup(&mut s);
            Arc::new(EcFakeDriver { state: Mutex::new(s) })
        }

        /// The fake's state. Do not hold the guard across a call into the code under test.
        pub(crate) fn st(&self) -> MutexGuard<'_, EcFakeState> {
            native::lock(&self.state)
        }

        // Guard-free reads, safe to combine in one expression.
        pub(crate) fn page(&self) -> u8 {
            self.st().page
        }
        pub(crate) fn ec_selected_page(&self) -> u8 {
            self.st().ec_selected_page
        }
        pub(crate) fn target(&self) -> u16 {
            self.st().target
        }
        pub(crate) fn regulator_writes(&self) -> Vec<(u8, u16)> {
            self.st().regulator_writes.clone()
        }
        pub(crate) fn regulator_reads(&self) -> Vec<u8> {
            self.st().regulator_reads.clone()
        }
        pub(crate) fn mailbox_commands(&self) -> Vec<u8> {
            self.st().mailbox_commands.clone()
        }
        pub(crate) fn io_reads(&self) -> i32 {
            self.st().io_reads
        }
        pub(crate) fn io_writes(&self) -> i32 {
            self.st().io_writes
        }
    }

    /// C# `FakeSio`: the verified MSI EC address over the fake, with an unnamed bus mutex.
    pub(crate) fn fake_sio(driver: &Arc<EcFakeDriver>) -> Arc<SuperIo> {
        let drv: Arc<dyn KernelDriver> = driver.clone();
        let sio = SuperIo::reuse_verified_msi_ec_address(&drv, 0x4E, 0x0A20, "Nuvoton NCT6687D").unwrap();
        Arc::new(sio.replace_isa_mutex_for_test(Some(NamedMutex::create(None).unwrap())))
    }

    impl KernelDriver for EcFakeDriver {
        fn name(&self) -> String {
            "strict EC simulation".into()
        }
        fn is_open(&self) -> bool {
            self.st().driver_open
        }
        // With `simulate_mailbox_reads`, the C# fake's thrown exceptions become driver failures
        // (`None` / `false`): the model code under test catches them exactly as the C# catches
        // the exception (e.g. the power-limit probe in the Intel row builder). Without it, any
        // MSR access is a test failure, as before.
        fn read_msr(&self, index: u32, _cpu: i32) -> Option<u64> {
            let s = self.st();
            if s.simulate_mailbox_reads {
                if index == OcMailbox::MSR_OC_MAILBOX {
                    return Some(s.mailbox_response);
                }
                return None; // Unexpected MSR read
            }
            drop(s);
            panic!("Unexpected MSR read")
        }
        fn write_msr(&self, index: u32, value: u64, _cpu: i32) -> bool {
            let mut s = self.st();
            if !s.simulate_mailbox_reads || index != OcMailbox::MSR_OC_MAILBOX {
                if s.simulate_mailbox_reads {
                    return false; // Unexpected MSR write
                }
                drop(s);
                panic!("Unexpected MSR write")
            }
            let command = (value >> 32) as u8;
            s.mailbox_commands.push(command);
            if command != OcMailbox::CMD_READ_VF {
                // The model fixture refuses every target/mode mailbox command.
                return false;
            }
            // Deliberately disagree with the board's 1.280 V target. Startup/defaults must
            // come from the verified regulator snapshot rather than this mailbox value.
            s.mailbox_response = OcMailbox::encode(&VfDomainSettings {
                max_ratio: 50,
                target_volts: 0.900,
                override_mode: true,
                offset_volts: -0.010,
            }) as u64;
            true
        }
        fn read_io_port_byte(&self, port: u16) -> Result<u8> {
            let mut s = self.st();
            s.io_reads += 1;
            if port == BASE + 4 {
                return Ok(s.ec_selected_page);
            }
            if port != BASE + 6 {
                bail!("Unexpected port read");
            }
            let address = ((s.ec_selected_page as u16) << 8) | s.ec_index as u16;
            if address == 0x463 && s.fail_command_result_once {
                s.fail_command_result_once = false;
                bail!("Synthetic command result I/O failure");
            }
            if address == 0x4B0 && s.get(0x466) == 0 {
                s.page_result_reads += 1;
                if s.fail_page_result_once || s.page_result_reads == s.fail_page_result_at {
                    s.fail_page_result_once = false;
                    bail!("Synthetic PAGE data I/O failure");
                }
            }
            if address == 0x4B0 && s.get(0x466) == 0xF0 && s.fail_mode_result_once {
                s.fail_mode_result_once = false;
                bail!("Synthetic mode data I/O failure");
            }
            if address == 0x4B0 && s.get(0x466) == 0x21 && s.fail_target_result_once {
                s.fail_target_result_once = false;
                bail!("Synthetic target data I/O failure");
            }
            if address == 0x4B0 && s.get(0x466) == 0x23 && s.fail_secondary_result_once {
                s.fail_secondary_result_once = false;
                bail!("Synthetic secondary data I/O failure");
            }
            Ok(s.get(address))
        }
        fn write_io_port_byte(&self, port: u16, value: u8) -> Result<()> {
            let mut s = self.st();
            s.io_writes += 1;
            if port == BASE + 4 {
                s.ec_selected_page = value;
                return Ok(());
            }
            if port == BASE + 5 {
                s.ec_index = value;
                return Ok(());
            }
            if port != BASE + 6 {
                bail!("Unexpected port write");
            }
            let address = ((s.ec_selected_page as u16) << 8) | s.ec_index as u16;
            s.ec.insert(address, value);
            if address != 0x460 {
                return Ok(());
            }
            let command = s.get(0x463);
            let register = s.get(0x466);
            if s.get(0x465) != 0xC6 {
                bail!("Unexpected regulator address");
            }
            if command >= 0x80 && value == 0x88 {
                s.regulator_reads.push(register);
                let raw: u16 = match register {
                    0 => s.page as u16,
                    0xF0 => s.mode,
                    0x21 => s.target,
                    0x23 => s.secondary,
                    _ => bail!("Unexpected regulator read"),
                };
                if register != 0 && s.page != 0 {
                    bail!("Read another regulator page as core");
                }
                s.ec.insert(0x4B0, raw as u8);
                s.ec.insert(0x4B1, (raw >> 8) as u8);
                if register == 0x23 && s.core_page_drift_on_secondary_once {
                    s.core_page_drift_on_secondary_once = false;
                    s.page = 1;
                }
            }
            if command < 0x80 && value == 0xC0 {
                let mut raw = s.get(0x470) as u16 | ((s.get(0x471) as u16) << 8);
                if command == 2 {
                    raw = s.get(0x470) as u16;
                }
                s.regulator_writes.push((register, raw));
                if register != 0 && s.page != 0 {
                    bail!("Attempted target/mode write on non-core PAGE");
                }
                if register == 0 {
                    if raw == 0 && s.ignore_page_select_once {
                        s.ignore_page_select_once = false;
                    } else if raw == 1 && s.ignore_page_restore_once {
                        s.ignore_page_restore_once = false;
                    } else {
                        s.page = raw as u8;
                    }
                } else if register == 0xF0 {
                    s.mode = raw;
                } else if register == 0x21 {
                    s.target = raw;
                } else if register == 0x23 {
                    s.secondary = raw;
                } else {
                    bail!("Unexpected regulator write");
                }
                s.ec.insert(0x460, 0x80);
                if register == 0x21 && s.fail_target_write_once {
                    s.fail_target_write_once = false;
                    bail!("Synthetic target-write I/O failure after physical change");
                }
                if register == 0 && raw == 0 && s.fail_page_select_once {
                    s.fail_page_select_once = false;
                    bail!("Synthetic PAGE selection failure after physical change");
                }
                if register == 0 && raw == 1 && s.fail_page_restore_once {
                    s.fail_page_restore_once = false;
                    bail!("Synthetic PAGE restoration failure after physical change");
                }
            }
            s.ec.insert(0x460, 0x80);
            Ok(())
        }
        fn read_io_port_word(&self, _port: u16) -> Result<u16> {
            Err(anyhow!("Unexpected word port read"))
        }
        fn read_io_port_dword(&self, _port: u16) -> Result<u32> {
            Err(anyhow!("Unexpected dword port read"))
        }
        fn write_io_port_word(&self, _port: u16, _value: u16) -> Result<()> {
            Err(anyhow!("Unexpected word port write"))
        }
        fn write_io_port_dword(&self, _port: u16, _value: u32) -> Result<()> {
            Err(anyhow!("Unexpected dword port write"))
        }
        fn read_pci_config(&self, _bus: u8, _device: u8, _function: u8, _offset: u16) -> Option<u32> {
            panic!("Unexpected PCI read")
        }
        fn write_pci_config(&self, _bus: u8, _device: u8, _function: u8, _offset: u16, _value: u32) -> bool {
            panic!("Unexpected PCI write")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
}
