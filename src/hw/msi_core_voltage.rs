//! CPU-core override control used by MSI's Intel 600/700-series boards.

use std::fmt;
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;

use anyhow::{Result, anyhow, bail};

use super::core_page_probe::{self, CorePageProbeTransport, CorePageSnapshot, CorePageVerificationResult};
use super::ec_mailbox::EcMailbox;
use super::native;
use super::startup_io_diagnostics;
use super::super_io::{NamedMutex, with_finally};

// Eight-bit I2C address as required by the EC mailbox (7-bit address 0x63).
const ADDRESS: u8 = 0xC6;
const R_PAGE: u8 = 0x00;
const R_VOUT_COMMAND: u8 = 0x21;
const R_VOUT_SECONDARY: u8 = 0x23;
const R_MODE: u8 = 0xF0;
const MODE_MASK: u16 = 0x0030;
const MODE_OVERRIDE: u16 = 0x0030;

/// C# `MsiCoreVoltage.State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub page: u8,
    pub mode_register: u16,
    pub target_mv: u16,
    pub secondary_target_mv: u16,
}

impl State {
    pub fn new(page: u8, mode_register: u16, target_mv: u16, secondary_target_mv: u16) -> State {
        State { page, mode_register, target_mv, secondary_target_mv }
    }

    pub fn mode(&self) -> i32 {
        ((self.mode_register >> 4) & 3) as i32
    }

    pub fn override_enabled(&self) -> bool {
        self.mode() == 3 && self.target_mv != 0
    }

    pub fn target_volts(&self) -> Option<f64> {
        if self.override_enabled() { Some(self.target_mv as f64 / 1000.0) } else { None }
    }

    fn snapshot(&self) -> CorePageSnapshot {
        CorePageSnapshot::new(self.mode_register, self.target_mv, self.secondary_target_mv)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "page {}, mode {}, target {:.3} V, secondary 0x{:04X}",
            self.page,
            self.mode(),
            self.target_mv as f64 / 1000.0,
            self.secondary_target_mv
        )
    }
}

/// A re-entrant in-process lock (C# `lock (obj)` is a re-entrant Monitor; the pair transaction
/// re-enters the core-PAGE lease from inside its own work callback).
struct ReentrantLock {
    owner: Mutex<(Option<ThreadId>, usize)>,
    cv: Condvar,
}

struct ReentrantGuard<'a>(&'a ReentrantLock);

impl ReentrantLock {
    fn new() -> ReentrantLock {
        ReentrantLock { owner: Mutex::new((None, 0)), cv: Condvar::new() }
    }

    fn lock(&self) -> ReentrantGuard<'_> {
        let me = std::thread::current().id();
        let mut st = native::lock(&self.owner);
        loop {
            match st.0 {
                None => {
                    *st = (Some(me), 1);
                    break;
                }
                Some(t) if t == me => {
                    st.1 += 1;
                    break;
                }
                Some(_) => st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
            }
        }
        ReentrantGuard(self)
    }
}

impl Drop for ReentrantGuard<'_> {
    fn drop(&mut self) {
        let mut st = native::lock(&self.0.owner);
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = None;
            self.0.cv.notify_one();
        }
    }
}

struct Inner {
    last_verified_state: Option<State>,
    verified_baseline: Option<State>,
    target_read_is_cached: bool,
    target_read_failed: bool,
    target_read_status: String,
}

/// .NET `AggregateException(inner…)` message.
fn aggregate(message: &str, failures: &[anyhow::Error]) -> anyhow::Error {
    let mut text = message.to_string();
    for f in failures {
        text.push_str(&format!(" ({f})"));
    }
    anyhow!(text)
}

/// CPU-core override control used by MSI's Intel 600/700-series boards.
///
/// The CPU's OC mailbox carries the requested VID, but on these boards the Renesas regulator also
/// has to be put into override mode and given the same target. MSI Dragon Power performs this
/// transaction first, through the EC-owned I2C bus, then writes OC mailbox domain 0. Writing only
/// the mailbox changes its readback while leaving the physical rail under the board's old target.
pub struct MsiCoreVoltage {
    mailbox: EcMailbox,
    sequence_mutex: Option<NamedMutex>,
    ec_sequence_mutex: Option<NamedMutex>,
    op_lock: ReentrantLock,
    inner: Mutex<Inner>,
}

impl MsiCoreVoltage {
    fn new(mailbox: &EcMailbox) -> MsiCoreVoltage {
        Self::with_mutexes(
            mailbox,
            NamedMutex::create(Some(r"Global\Access_SMBUS.Renesas.HTP.Method")).ok(),
            NamedMutex::create(Some(r"Global\Access_EC")).ok(),
        )
    }

    /// The constructor body with the two bus mutexes supplied (tests substitute isolated ones).
    fn with_mutexes(mailbox: &EcMailbox, sequence: Option<NamedMutex>, ec_sequence: Option<NamedMutex>) -> MsiCoreVoltage {
        MsiCoreVoltage {
            mailbox: mailbox.with_strict_io(),
            sequence_mutex: sequence,
            ec_sequence_mutex: ec_sequence,
            op_lock: ReentrantLock::new(),
            inner: Mutex::new(Inner {
                last_verified_state: None,
                verified_baseline: None,
                target_read_is_cached: false,
                target_read_failed: false,
                target_read_status: "Core page has not been verified.".to_string(),
            }),
        }
    }

    pub fn verified_baseline(&self) -> Option<State> {
        native::lock(&self.inner).verified_baseline
    }

    pub fn target_read_is_cached(&self) -> bool {
        native::lock(&self.inner).target_read_is_cached
    }

    pub fn target_read_failed(&self) -> bool {
        native::lock(&self.inner).target_read_failed
    }

    pub fn target_read_status(&self) -> String {
        native::lock(&self.inner).target_read_status.clone()
    }

    fn set_last_verified(&self, state: State) {
        native::lock(&self.inner).last_verified_state = Some(state);
    }

    /// Read-only detection. It deliberately refuses to change the regulator's active page.
    pub fn try_create(mailbox: &EcMailbox) -> (Option<MsiCoreVoltage>, String) {
        let control = MsiCoreVoltage::new(mailbox);
        let mut keep = false;
        let outcome = (|| -> Result<String> {
            if !control.acquire()? {
                return Ok("timed out waiting for the Renesas regulator bus".to_string());
            }
            let body = (|| -> Result<String> {
                let page = control.mailbox.read_byte(ADDRESS, R_PAGE)?;
                let Some(page) = page else {
                    return Ok("Renesas core regulator at EC-I2C address 0xC6 did not answer".to_string());
                };
                if page != 0 {
                    return Ok(format!(
                        "Renesas core regulator answered on page {page}, not core page 0; control left disabled"
                    ));
                }
                let state = control.read_state_core(page)?;
                if control.read_byte(R_PAGE)? != 0 {
                    bail!("Regulator PAGE changed while probing the core state; control left disabled.");
                }
                core_page_probe::validate_snapshot(&state.snapshot())?;
                {
                    let mut inner = native::lock(&control.inner);
                    inner.last_verified_state = Some(state);
                    inner.verified_baseline = Some(state);
                    inner.target_read_status = "Core PAGE 0 target read from the regulator.".to_string();
                }
                keep = true;
                Ok(format!("Renesas core regulator: {state}"))
            })();
            with_finally(body, control.release())
        })();
        match outcome {
            Ok(status) => (if keep { Some(control) } else { None }, status),
            Err(ex) => (None, format!("Renesas core regulator unavailable: {ex}")),
        }
    }

    /// One supported startup verification. Selects/reads core PAGE 0 and restores the original
    /// PAGE, whether initialization found PAGE 0 or PAGE 1. It never changes voltage/mode/mailbox
    /// targets and never initializes a driver. The model prevents repeated initialization attempts.
    pub fn try_create_for_startup(mailbox: &EcMailbox) -> (Option<MsiCoreVoltage>, CorePageVerificationResult) {
        let control = MsiCoreVoltage::new(mailbox);
        Self::complete_startup_verification(control)
    }

    // Common verification after construction, kept private so production cannot supply a fake
    // transport or synchronization. Inert tests replace the fake controller's locks first.
    fn complete_startup_verification(control: MsiCoreVoltage) -> (Option<MsiCoreVoltage>, CorePageVerificationResult) {
        let diagnostics = startup_io_diagnostics::begin();
        let mut result = core_page_probe::verify(
            &ProbeTransport { mailbox: &control.mailbox },
            || control.acquire(),
            || control.release(),
        );
        startup_io_diagnostics::observe_probe_result(&result);
        // Diagnostics cannot change activation, failure or hardware flow.
        let lines = diagnostics.as_ref().map(|d| d.snapshot());
        drop(diagnostics);
        if let Some(lines) = lines {
            result.diagnostic_lines = lines;
        }
        let snapshot = match (result.succeeded, result.snapshot) {
            (true, Some(s)) => s,
            _ => return (None, result),
        };
        {
            let mut inner = native::lock(&control.inner);
            let state = State::new(0, snapshot.mode_register, snapshot.target_mv, snapshot.secondary_target_mv);
            inner.verified_baseline = Some(state);
            inner.last_verified_state = Some(state);
            inner.target_read_is_cached = result.restored_page != Some(0);
            inner.target_read_status = if inner.target_read_is_cached {
                format!(
                    "Core target last verified on PAGE 0; regulator restored to PAGE {}.",
                    result.restored_page.map(|p| p.to_string()).unwrap_or_default()
                )
            } else {
                "Core PAGE 0 target read from the regulator.".to_string()
            };
        }
        (Some(control), result)
    }

    fn acquire(&self) -> Result<bool> {
        let (Some(sequence), Some(ec_sequence)) = (&self.sequence_mutex, &self.ec_sequence_mutex) else {
            return Ok(false);
        };
        let held = sequence.wait(1000);
        if !held {
            return Ok(false);
        }
        let ec_held = ec_sequence.wait(1000);
        if !ec_held {
            sequence.release()?;
        }
        Ok(ec_held)
    }

    fn release(&self) -> Result<()> {
        let ec = self.ec_sequence_mutex.as_ref().map_or(Ok(()), |m| m.release());
        let seq = self.sequence_mutex.as_ref().map_or(Ok(()), |m| m.release());
        with_finally(ec, seq)
    }

    fn read_byte(&self, register: u8) -> Result<u8> {
        self.mailbox.read_byte(ADDRESS, register)?.ok_or_else(|| anyhow!("regulator read 0x{register:02X} failed"))
    }

    fn read_word(&self, register: u8) -> Result<u16> {
        self.mailbox.read_word(ADDRESS, register)?.ok_or_else(|| anyhow!("regulator read 0x{register:02X} failed"))
    }

    fn write_byte(&self, register: u8, value: u8) -> Result<()> {
        if !self.mailbox.write_byte(ADDRESS, register, value)? || self.read_byte(register)? != value {
            bail!("regulator write 0x{register:02X}=0x{value:02X} did not read back");
        }
        Ok(())
    }

    fn write_word(&self, register: u8, value: u16) -> Result<()> {
        if self.read_byte(R_PAGE)? != 0 {
            bail!("Core PAGE 0 is no longer selected; target write refused.");
        }
        if !self.mailbox.write_word(ADDRESS, register, value)? || self.read_word(register)? != value {
            bail!("regulator write 0x{register:02X}=0x{value:04X} did not read back");
        }
        Ok(())
    }

    fn select_core_page(&self) -> Result<()> {
        if self.read_byte(R_PAGE)? != 0 {
            self.write_byte(R_PAGE, 0)?;
        }
        Ok(())
    }

    fn read_state_core(&self, page: u8) -> Result<State> {
        let mode = self.read_word(R_MODE)?;
        let target = self.read_word(R_VOUT_COMMAND)?;
        let secondary = self.read_word(R_VOUT_SECONDARY)?;
        Ok(State::new(page, mode, target, secondary))
    }

    fn on_core_page<T>(&self, work: impl FnOnce(State) -> Result<T>, validate_current: bool) -> Result<T> {
        let _guard = self.op_lock.lock();
        if !self.acquire()? {
            bail!("timed out waiting for the regulator bus");
        }
        let mut original_page: Option<u8> = None;
        let mut failure: Option<anyhow::Error> = None;
        let mut result: Option<T> = None;
        let body = (|| -> Result<T> {
            let page = self.read_byte(R_PAGE)?;
            original_page = Some(page);
            if !(page == 0 || page == 1) {
                bail!("Regulator PAGE {page} is not verified for core control.");
            }
            self.select_core_page()?;
            let state = self.read_state_core(0)?;
            if self.read_byte(R_PAGE)? != 0 {
                bail!("Regulator PAGE changed while reading the core state.");
            }
            if validate_current {
                core_page_probe::validate_snapshot(&state.snapshot())?;
            }
            work(state)
        })();
        match body {
            Ok(r) => result = Some(r),
            Err(ex) => failure = Some(ex),
        }

        let restore = (|| -> Result<()> {
            if let Some(original) = original_page.filter(|p| *p == 0 || *p == 1) {
                // A failing select/read may still have switched PAGE. Restore even then.
                let current = self.read_byte(R_PAGE).ok();
                if current != Some(original) {
                    self.write_byte(R_PAGE, original)?;
                }
                if self.read_byte(R_PAGE)? != original {
                    bail!("Original regulator PAGE restoration did not read back.");
                }
                let mut inner = native::lock(&self.inner);
                if inner.last_verified_state.is_some() {
                    if failure.is_none() {
                        inner.target_read_failed = false;
                    }
                    inner.target_read_is_cached = original != 0;
                    inner.target_read_status = if inner.target_read_is_cached {
                        "Core target last verified on PAGE 0; regulator restored to PAGE 1.".to_string()
                    } else {
                        "Core PAGE 0 target read from the regulator.".to_string()
                    };
                }
            }
            Ok(())
        })();
        if let Err(ex) = restore {
            failure = Some(match failure {
                None => ex,
                Some(f) => aggregate("One or more errors occurred.", &[f, ex]),
            });
        }
        self.release()?;
        if let Some(f) = failure {
            return Err(f);
        }
        Ok(result.unwrap())
    }

    /// Explicit apply/rollback state read; preserves the regulator's original PAGE.
    pub fn read_state(&self) -> Result<State> {
        self.on_core_page(
            |state| {
                self.set_last_verified(state);
                Ok(state)
            },
            true,
        )
    }

    /// Explicit Apply only: holds the shared bus locks and core PAGE across both targets.
    pub fn with_core_page_for_apply(&self, work: impl FnOnce(State) -> Result<()>) -> Result<()> {
        self.on_core_page(work, true)
    }

    /// Refresh never selects a regulator PAGE. On a different page it returns the last verified
    /// core target and labels it cached; it never reads another page as Vcore or substitutes VID.
    pub fn read_target_volts(&self) -> Result<Option<f64>> {
        let _guard = self.op_lock.lock();
        let outcome = (|| -> Result<Option<f64>> {
            if !self.acquire()? {
                bail!("timed out waiting for the regulator bus");
            }
            let body = (|| -> Result<Option<f64>> {
                let page = self.read_byte(R_PAGE)?;
                if page == 0 {
                    let state = self.read_state_core(0)?;
                    if self.read_byte(R_PAGE)? != 0 {
                        bail!("Regulator PAGE changed while reading the core target.");
                    }
                    core_page_probe::validate_snapshot(&state.snapshot())?;
                    let mut inner = native::lock(&self.inner);
                    inner.last_verified_state = Some(state);
                    inner.target_read_is_cached = false;
                    inner.target_read_status = "Core PAGE 0 target read from the regulator.".to_string();
                } else if page == 1 && native::lock(&self.inner).last_verified_state.is_some() {
                    let mut inner = native::lock(&self.inner);
                    inner.target_read_is_cached = true;
                    inner.target_read_status = "Core target last verified on PAGE 0; regulator currently on PAGE 1.".to_string();
                } else {
                    bail!("Core target unavailable on regulator PAGE {page}; no page change attempted.");
                }
                let mut inner = native::lock(&self.inner);
                inner.target_read_failed = false;
                Ok(inner.last_verified_state.and_then(|s| s.target_volts()))
            })();
            with_finally(body, self.release())
        })();
        if let Err(ex) = &outcome {
            let mut inner = native::lock(&self.inner);
            inner.target_read_failed = true;
            inner.target_read_status = format!("Core target not read: {ex}");
        }
        outcome
    }

    /// Set the board regulator to the requested millivolt target. This is the precise ordering used
    /// by MSI Dragon Power: disable another active target, program VOUT_COMMAND, then select mode 3.
    pub fn set_override(&self, volts: f64) -> Result<()> {
        // checked((ushort)Math.Round(volts * 1000.0)) - Math.Round rounds half to even.
        let rounded = (volts * 1000.0).round_ties_even();
        if !(rounded > -1.0 && rounded < 65536.0) {
            bail!("Arithmetic operation resulted in an overflow.");
        }
        let target_mv = rounded as u16;
        if target_mv == 0 {
            return self.disable_override();
        }

        self.on_core_page(
            |before| {
                let attempt = (|| -> Result<()> {
                    let mut mode = before.mode_register;

                    // The secondary target (0x23) and primary target (0x21) cannot own override mode
                    // together. MSI clears 0x23 before enabling the primary CPU-core target.
                    if before.secondary_target_mv != 0 && (mode & MODE_MASK) != 0 {
                        mode &= !MODE_MASK;
                        self.write_word(R_MODE, mode)?;
                        self.write_word(R_VOUT_SECONDARY, 0)?;
                    }

                    if self.read_word(R_VOUT_COMMAND)? != target_mv {
                        self.write_word(R_VOUT_COMMAND, target_mv)?;
                    }
                    if (mode & MODE_MASK) != MODE_OVERRIDE {
                        mode = (mode & !MODE_MASK) | MODE_OVERRIDE;
                        self.write_word(R_MODE, mode)?;
                    }

                    let after = self.read_state_core(0)?;
                    if !after.override_enabled() || after.target_mv != target_mv {
                        bail!("regulator rejected {volts:.3} V ({after})");
                    }
                    self.set_last_verified(after);
                    Ok(())
                })();
                if let Err(ex) = attempt {
                    self.try_restore_core(before);
                    return Err(ex);
                }
                Ok(())
            },
            true,
        )
    }

    pub fn disable_override(&self) -> Result<()> {
        self.on_core_page(
            |before| {
                let attempt = (|| -> Result<()> {
                    let mode = before.mode_register & !MODE_MASK;
                    if mode != before.mode_register {
                        self.write_word(R_MODE, mode)?;
                    }
                    if before.target_mv != 0 {
                        self.write_word(R_VOUT_COMMAND, 0)?;
                    }
                    if before.secondary_target_mv != 0 {
                        self.write_word(R_VOUT_SECONDARY, 0)?;
                    }
                    let state = self.read_state_core(0)?;
                    self.set_last_verified(state);
                    Ok(())
                })();
                if let Err(ex) = attempt {
                    self.try_restore_core(before);
                    return Err(ex);
                }
                Ok(())
            },
            true,
        )
    }

    /// Restores a captured regulator state; used when the following CPU-mailbox write fails.
    pub fn restore(&self, state: State) -> Result<()> {
        core_page_probe::validate_snapshot(&state.snapshot())?;
        self.on_core_page(
            |_| {
                self.restore_core(State { page: 0, ..state })?;
                let after = self.read_state_core(0)?;
                self.set_last_verified(after);
                Ok(())
            },
            false,
        )
    }

    fn restore_core(&self, state: State) -> Result<()> {
        self.select_core_page()?;
        let disabled = self.read_word(R_MODE)? & !MODE_MASK;
        self.write_word(R_MODE, disabled)?;
        self.write_word(R_VOUT_COMMAND, state.target_mv)?;
        self.write_word(R_VOUT_SECONDARY, state.secondary_target_mv)?;
        self.write_word(R_MODE, state.mode_register)?;
        if state.page != 0 {
            self.write_byte(R_PAGE, state.page)?;
        }
        Ok(())
    }

    fn try_restore_core(&self, state: State) {
        let _ = self.restore_core(state);
    }

    // ---------------------------------------------------------------- test seams
    // The C# tests reach these through reflection (private constructor, mutex fields,
    // CompleteStartupVerification, Acquire/Release).

    /// C# test `Startup`: the production startup completion over a controller whose two global
    /// mutex handles were replaced with unnamed ones before any acquisition.
    #[cfg(test)]
    pub(crate) fn startup_for_test(mailbox: &EcMailbox) -> (Option<MsiCoreVoltage>, CorePageVerificationResult) {
        let control = Self::with_mutexes(mailbox, Some(NamedMutex::create(None).unwrap()), Some(NamedMutex::create(None).unwrap()));
        Self::complete_startup_verification(control)
    }
}

struct ProbeTransport<'a> {
    mailbox: &'a EcMailbox,
}

impl CorePageProbeTransport for ProbeTransport<'_> {
    fn read_page(&self) -> Result<Option<u8>> {
        self.mailbox.read_byte(ADDRESS, R_PAGE)
    }
    fn write_page(&self, page: u8) -> Result<bool> {
        if !(page == 0 || page == 1) {
            bail!("Specified argument was out of the range of valid values. (Parameter 'page')");
        }
        self.mailbox.write_byte(ADDRESS, R_PAGE, page)
    }
    fn read_mode(&self) -> Result<Option<u16>> {
        self.mailbox.read_word(ADDRESS, R_MODE)
    }
    fn read_target(&self) -> Result<Option<u16>> {
        self.mailbox.read_word(ADDRESS, R_VOUT_COMMAND)
    }
    fn read_secondary_target(&self) -> Result<Option<u16>> {
        self.mailbox.read_word(ADDRESS, R_VOUT_SECONDARY)
    }
}

#[cfg(test)]
mod tests {
    //! Port of tests/CorePageProbe.LowLevel.Tests (simulated EC; only a fake KernelDriver is
    //! constructed). The pair-transaction checks live in `core_voltage_pair_transaction`.
    use super::*;
    use crate::hw::ec_mailbox::fakes::{EcFakeDriver, fake_sio};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn check(ok: bool, message: &str) {
        assert!(ok, "{message}");
    }

    fn must_fail<T>(r: Result<T>) {
        check(r.is_err(), "Failure was incorrectly reported successful");
    }

    /// C# test `Transport`.
    struct Transport(EcMailbox);

    impl CorePageProbeTransport for Transport {
        fn read_page(&self) -> Result<Option<u8>> {
            self.0.read_byte(0xC6, 0)
        }
        fn write_page(&self, page: u8) -> Result<bool> {
            self.0.write_byte(0xC6, 0, page)
        }
        fn read_mode(&self) -> Result<Option<u16>> {
            self.0.read_word(0xC6, 0xF0)
        }
        fn read_target(&self) -> Result<Option<u16>> {
            self.0.read_word(0xC6, 0x21)
        }
        fn read_secondary_target(&self) -> Result<Option<u16>> {
            self.0.read_word(0xC6, 0x23)
        }
    }

    fn probe(driver: &Arc<EcFakeDriver>) -> CorePageVerificationResult {
        let sio = fake_sio(driver);
        core_page_probe::verify(&Transport(EcMailbox::new_with_strict_io(sio, true)), || Ok(true), || Ok(()))
    }

    fn startup(driver: &Arc<EcFakeDriver>) -> (Option<MsiCoreVoltage>, CorePageVerificationResult) {
        MsiCoreVoltage::startup_for_test(&EcMailbox::new(fake_sio(driver)))
    }

    fn only_page_writes(driver: &EcFakeDriver) -> bool {
        driver.regulator_writes().iter().all(|w| w.0 == 0)
    }

    #[test]
    fn strict_failed_initial_page_result_cannot_fabricate_zero_or_restore_another_page() {
        let driver = EcFakeDriver::with(|s| s.fail_page_result_once = true);
        let result = probe(&driver);
        check(!result.succeeded && result.original_page.is_none() && driver.page() == 1, &result.status);
        let s = driver.st();
        check(
            s.regulator_writes.is_empty() && s.ec_selected_page == 0xFF,
            "Unknown original PAGE caused a write or EC index page was stranded",
        );
    }

    #[test]
    fn strict_failed_core_snapshot_restores_the_genuinely_saved_page() {
        let driver = EcFakeDriver::with(|s| s.fail_mode_result_once = true);
        let result = probe(&driver);
        check(
            !result.succeeded && result.original_page == Some(1) && result.restored_page == Some(1) && result.snapshot.is_none(),
            &result.status,
        );
        check(driver.regulator_writes() == vec![(0, 0), (0, 1)], "Failure changed a voltage/mode register");
    }

    #[test]
    fn strict_low_level_successful_probe_reads_actual_page_0_and_restores_page_1() {
        let driver = EcFakeDriver::new();
        let result = probe(&driver);
        check(
            result.succeeded && result.snapshot == Some(CorePageSnapshot::new(0x30, 1280, 0)) && driver.page() == 1,
            &result.status,
        );
        check(only_page_writes(&driver), "Diagnostic wrote a voltage/mode target");
    }

    #[test]
    fn strict_ec_command_readback_failure_is_propagated() {
        let driver = EcFakeDriver::with(|s| s.fail_command_result_once = true);
        let result = probe(&driver);
        check(
            !result.succeeded && result.original_page.is_none() && driver.regulator_writes().is_empty(),
            &result.status,
        );
    }

    #[test]
    fn verified_controller_refresh_on_page_1_uses_genuine_cached_core_state_without_page_writes() {
        let driver = EcFakeDriver::new();
        let (control, result) = startup(&driver);
        check(result.succeeded && control.is_some(), &result.status);
        let control = control.unwrap();
        driver.st().regulator_writes.clear();
        check(
            control.read_target_volts().unwrap() == Some(1.280) && control.target_read_is_cached() && driver.page() == 1,
            "Cached core target was replaced with another rail or unknown state",
        );
        check(driver.regulator_writes().is_empty(), "Periodic target refresh selected a PAGE");
    }

    #[test]
    fn explicit_apply_state_read_preserves_original_page() {
        let driver = EcFakeDriver::new();
        let (control, result) = startup(&driver);
        driver.st().regulator_writes.clear();
        let state = control.unwrap().read_state().unwrap();
        check(state.target_mv == 1280 && driver.page() == 1 && only_page_writes(&driver), &result.status);
    }

    fn with_control(test: impl FnOnce(&Arc<EcFakeDriver>, &MsiCoreVoltage)) {
        let driver = EcFakeDriver::new();
        let (control, result) = startup(&driver);
        check(result.succeeded && control.is_some(), &result.status);
        driver.st().regulator_writes.clear();
        test(&driver, control.as_ref().unwrap());
    }

    #[test]
    fn automatic_startup_on_page_1_captures_core_target_and_restores_page_with_only_page_writes() {
        let driver = EcFakeDriver::new();
        let (control, result) = startup(&driver);
        let control = control.expect(&result.status);
        check(
            result.succeeded
                && control.verified_baseline().unwrap().target_volts() == Some(1.280)
                && driver.page() == 1
                && driver.regulator_writes() == vec![(0, 0), (0, 1)],
            &result.status,
        );
    }

    #[test]
    fn automatic_startup_on_page_0_captures_genuine_baseline_without_page_writes() {
        let driver = EcFakeDriver::with(|s| s.page = 0);
        let (control, result) = startup(&driver);
        let control = control.expect(&result.status);
        check(
            result.succeeded
                && control.verified_baseline() == Some(State::new(0, 0x30, 1280, 0))
                && driver.page() == 0
                && driver.regulator_writes().is_empty(),
            &result.status,
        );
    }

    #[test]
    fn strict_startup_page_readback_failure_cannot_create_a_controller() {
        for readback in [2, 3, 5] {
            let driver = EcFakeDriver::with(|s| s.fail_page_result_at = readback);
            let (control, result) = startup(&driver);
            check(
                control.is_none()
                    && !result.succeeded
                    && driver.page() == 1
                    && driver.ec_selected_page() == 0xFF
                    && only_page_writes(&driver),
                &format!("readback {readback}: {}", result.status),
            );
        }
    }

    #[test]
    fn strict_startup_missing_restoration_pre_read_still_restores_and_verifies_saved_page() {
        let driver = EcFakeDriver::with(|s| s.fail_page_result_at = 4);
        let (control, result) = startup(&driver);
        check(
            control.is_some()
                && result.succeeded
                && driver.page() == 1
                && driver.ec_selected_page() == 0xFF
                && only_page_writes(&driver),
            &result.status,
        );
    }

    #[test]
    fn strict_startup_failure_fails_closed_and_writes_no_target_or_mode() {
        for failure in [
            "target-read",
            "secondary-read",
            "select-after-change",
            "select-mismatch",
            "restore-after-change",
            "restore-mismatch",
            "snapshot-drift",
        ] {
            let driver = EcFakeDriver::with(|s| match failure {
                "target-read" => s.fail_target_result_once = true,
                "secondary-read" => s.fail_secondary_result_once = true,
                "select-after-change" => s.fail_page_select_once = true,
                "select-mismatch" => s.ignore_page_select_once = true,
                "restore-after-change" => s.fail_page_restore_once = true,
                "restore-mismatch" => s.ignore_page_restore_once = true,
                "snapshot-drift" => s.core_page_drift_on_secondary_once = true,
                _ => unreachable!(),
            });
            let (control, result) = startup(&driver);
            check(
                control.is_none() && !result.succeeded && driver.ec_selected_page() == 0xFF && only_page_writes(&driver),
                &format!("{failure}: {}", result.status),
            );
            check(
                driver.page() == if failure == "restore-mismatch" { 0 } else { 1 },
                "Failure did not preserve/restore PAGE as transport permits",
            );
        }
    }

    // ---------------------------------------------------------------- isolated mutex checks
    struct Isolated {
        driver: Arc<EcFakeDriver>,
        mailbox: EcMailbox,
        control: MsiCoreVoltage,
        renesas: String,
        ec: String,
    }

    fn unique(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!("{prefix}-{}-{t}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
    }

    /// Invoke only the controller's constructor around a fake mailbox, with both handles
    /// replaced by test-unique names before any acquisition; the running app's bus mutexes are
    /// never held, abandoned, closed or modified by these contention checks.
    fn with_isolated_locks(missing: Option<&str>, test: impl FnOnce(&Isolated)) {
        let driver = EcFakeDriver::new();
        let sio = fake_sio(&driver);
        let mailbox = EcMailbox::new_with_strict_io(sio, true);
        let renesas = unique("RochCpuFakeRenesas");
        let ec = unique("RochCpuFakeEc");
        let seq = if missing == Some("_sequenceMutex") { None } else { Some(NamedMutex::create(Some(&renesas)).unwrap()) };
        let ecm = if missing == Some("_ecSequenceMutex") { None } else { Some(NamedMutex::create(Some(&ec)).unwrap()) };
        let control = MsiCoreVoltage::with_mutexes(&mailbox, seq, ecm);
        test(&Isolated { driver, mailbox, control, renesas, ec });
    }

    fn probe_with_controller_locks(iso: &Isolated) -> CorePageVerificationResult {
        core_page_probe::verify(&Transport(iso.mailbox.clone()), || iso.control.acquire(), || iso.control.release())
    }

    fn can_acquire_from_another_thread(name: &str) -> bool {
        let name = name.to_string();
        let worker = std::thread::spawn(move || {
            let m = NamedMutex::create(Some(&name)).unwrap();
            let acquired = m.wait(250);
            if acquired {
                m.release().unwrap();
            }
            acquired
        });
        worker.join().expect("Isolated mutex probe worker did not finish")
    }

    #[test]
    fn mutex_timeout_is_bounded_accesses_no_registers_and_releases_partial_acquisition() {
        for hold_ec in [false, true] {
            with_isolated_locks(None, |iso| {
                let held = if hold_ec { iso.ec.clone() } else { iso.renesas.clone() };
                let (ready_tx, ready_rx) = mpsc::channel();
                let (stop_tx, stop_rx) = mpsc::channel::<()>();
                let holder = std::thread::spawn(move || {
                    let m = NamedMutex::create(Some(&held)).unwrap();
                    assert!(m.wait(u32::MAX));
                    ready_tx.send(()).unwrap();
                    let _ = stop_rx.recv_timeout(Duration::from_millis(5000));
                    m.release().unwrap();
                });
                check(ready_rx.recv_timeout(Duration::from_millis(2000)).is_ok(), "Holder did not acquire isolated test mutex");
                let timer = Instant::now();
                let result = probe_with_controller_locks(iso);
                let elapsed = timer.elapsed().as_millis();
                {
                    let s = iso.driver.st();
                    check(
                        !result.succeeded && s.io_reads == 0 && s.io_writes == 0 && s.regulator_writes.is_empty(),
                        &result.status,
                    );
                }
                check((900..4000).contains(&elapsed), "Bus timeout escaped its bounded lease budget");
                if hold_ec {
                    check(can_acquire_from_another_thread(&iso.renesas), "EC timeout leaked the already acquired Renesas mutex");
                }
                stop_tx.send(()).unwrap();
                holder.join().expect("Holder did not release test mutex");
                check(
                    can_acquire_from_another_thread(&iso.renesas) && can_acquire_from_another_thread(&iso.ec),
                    "Timeout leaked a shared mutex",
                );
            });
        }
    }

    #[test]
    fn abandoned_mutex_is_owned_probed_safely_and_released() {
        for abandon_ec in [false, true] {
            with_isolated_locks(None, |iso| {
                let name = if abandon_ec { iso.ec.clone() } else { iso.renesas.clone() };
                let worker = std::thread::spawn(move || {
                    let m = NamedMutex::create(Some(&name)).unwrap();
                    m.wait(u32::MAX);
                    // Exit owning the mutex, so the next waiter sees it abandoned.
                });
                worker.join().expect("Abandonment fixture worker did not exit");
                let result = probe_with_controller_locks(iso);
                check(result.succeeded && iso.driver.page() == 1 && only_page_writes(&iso.driver), &result.status);
                check(
                    can_acquire_from_another_thread(&iso.renesas) && can_acquire_from_another_thread(&iso.ec),
                    "Recovered abandoned mutex was not released",
                );
            });
        }
    }

    #[test]
    fn successful_startup_probe_releases_both_mutexes_after_snapshot_and_restore() {
        with_isolated_locks(None, |iso| {
            let result = probe_with_controller_locks(iso);
            check(result.succeeded && iso.driver.page() == 1 && only_page_writes(&iso.driver), &result.status);
            check(
                can_acquire_from_another_thread(&iso.renesas) && can_acquire_from_another_thread(&iso.ec),
                "Successful probe leaked a mutex",
            );
        });
    }

    #[test]
    fn failed_startup_snapshot_restores_page_and_releases_both_mutexes() {
        with_isolated_locks(None, |iso| {
            iso.driver.st().fail_mode_result_once = true;
            let result = probe_with_controller_locks(iso);
            check(!result.succeeded && iso.driver.page() == 1 && only_page_writes(&iso.driver), &result.status);
            check(
                can_acquire_from_another_thread(&iso.renesas) && can_acquire_from_another_thread(&iso.ec),
                "Failed probe leaked a mutex",
            );
        });
    }

    #[test]
    fn missing_handle_fails_closed_before_every_register_access() {
        for missing in ["_sequenceMutex", "_ecSequenceMutex"] {
            with_isolated_locks(Some(missing), |iso| {
                let result = probe_with_controller_locks(iso);
                {
                    let s = iso.driver.st();
                    check(!result.succeeded && s.io_reads == 0 && s.io_writes == 0, &result.status);
                }
                check(
                    can_acquire_from_another_thread(&iso.renesas) && can_acquire_from_another_thread(&iso.ec),
                    "Missing handle probe acquired a partial lease",
                );
            });
        }
    }

    // ---------------------------------------------------------------- manual control
    #[test]
    fn manual_override_preserves_page_1_and_writes_targets_only_on_core_page_0() {
        with_control(|driver, control| {
            control.set_override(1.320).unwrap();
            {
                let s = driver.st();
                check(s.page == 1 && s.target == 1320 && s.mode == 0x30, "Override or PAGE was incorrect");
            }
            check(
                control.target_read_is_cached() && control.target_read_status().contains("restored to PAGE 1"),
                "Post-Apply target status was not updated before the next refresh",
            );
            driver.st().regulator_writes.clear();
            check(
                control.read_target_volts().unwrap() == Some(1.320)
                    && control.target_read_is_cached()
                    && driver.regulator_writes().is_empty(),
                "Applied target cache/refresh was incorrect",
            );
        });
    }

    #[test]
    fn manual_adaptive_zero_preserves_page_1_and_clears_existing_override() {
        with_control(|driver, control| {
            control.disable_override().unwrap();
            {
                let s = driver.st();
                check(
                    s.page == 1 && s.target == 0 && s.mode == 0 && s.secondary == 0,
                    "Adaptive reset altered PAGE or retained target",
                );
            }
            check(
                control.target_read_is_cached() && control.target_read_status().contains("restored to PAGE 1"),
                "Post-reset target status was stale before refresh",
            );
            driver.st().regulator_writes.clear();
            check(
                control.read_target_volts().unwrap().is_none()
                    && control.target_read_is_cached()
                    && driver.regulator_writes().is_empty(),
                "Adaptive cache was fabricated",
            );
        });
    }

    #[test]
    fn manual_captured_state_restore_preserves_original_page() {
        with_control(|driver, control| {
            let before = control.read_state().unwrap();
            control.set_override(1.320).unwrap();
            control.restore(before).unwrap();
            let s = driver.st();
            check(
                s.page == 1 && s.target == 1280 && s.mode == 0x30 && s.secondary == 0,
                "Rollback did not restore the exact core state",
            );
        });
    }

    #[test]
    fn failed_regulator_target_write_rolls_back_and_restores_original_page() {
        with_control(|driver, control| {
            driver.st().fail_target_write_once = true;
            must_fail(control.set_override(1.320));
            let s = driver.st();
            check(s.page == 1 && s.target == 1280 && s.mode == 0x30, "Failed target write left changed state");
        });
    }

    #[test]
    fn strict_refresh_failure_propagates_instead_of_inventing_an_adaptive_target() {
        with_control(|driver, control| {
            {
                let mut s = driver.st();
                s.page = 0;
                s.fail_mode_result_once = true;
            }
            must_fail(control.read_target_volts());
            check(
                driver.regulator_writes().is_empty()
                    && driver.page() == 0
                    && control.target_read_failed()
                    && control.target_read_status().starts_with("Core target not read:"),
                "Refresh failure wrote hardware or fabricated adaptive state",
            );
            {
                let mut s = driver.st();
                s.mode = 0;
                s.target = 0;
            }
            check(
                control.read_target_volts().unwrap().is_none() && !control.target_read_failed(),
                "A genuine verified adaptive-null target was incorrectly marked unread",
            );
        });
    }

    /// Rust-only: the C# `checked((ushort)…)` overflow guard refuses before any register access.
    #[test]
    fn set_override_out_of_range_target_is_refused() {
        with_control(|driver, control| {
            must_fail(control.set_override(f64::NAN));
            must_fail(control.set_override(70.0));
            check(driver.regulator_writes().is_empty(), "Overflowing target reached the regulator");
        });
    }
}
