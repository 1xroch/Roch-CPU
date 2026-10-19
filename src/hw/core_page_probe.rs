//! A bounded diagnostic of the Renesas core regulator's PAGE 0 registers.

use anyhow::{Result, bail};

/// The diagnostic exposes only PAGE changes and reads of the three core registers.
/// (C# `ICorePageProbeTransport`; a C# exception is an `Err`, a C# null is `Ok(None)`.)
pub trait CorePageProbeTransport {
    fn read_page(&self) -> Result<Option<u8>>;
    fn write_page(&self, page: u8) -> Result<bool>;
    fn read_mode(&self) -> Result<Option<u16>>;
    fn read_target(&self) -> Result<Option<u16>>;
    fn read_secondary_target(&self) -> Result<Option<u16>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorePageSnapshot {
    pub mode_register: u16,
    pub target_mv: u16,
    pub secondary_target_mv: u16,
}

impl CorePageSnapshot {
    pub fn new(mode_register: u16, target_mv: u16, secondary_target_mv: u16) -> CorePageSnapshot {
        CorePageSnapshot { mode_register, target_mv, secondary_target_mv }
    }

    pub fn mode(&self) -> i32 {
        ((self.mode_register >> 4) & 3) as i32
    }

    pub fn programmed_target_volts(&self) -> f64 {
        self.target_mv as f64 / 1000.0
    }

    pub fn override_target_volts(&self) -> Option<f64> {
        if self.mode() == 3 && self.target_mv != 0 { Some(self.programmed_target_volts()) } else { None }
    }
}

#[derive(Clone, Debug)]
pub struct CorePageVerificationResult {
    pub succeeded: bool,
    pub original_page: Option<u8>,
    pub restored_page: Option<u8>,
    pub snapshot: Option<CorePageSnapshot>,
    pub status: String,
    pub page_selection_attempted: bool,
    pub page_restoration_attempted: bool,
    pub diagnostic_lines: Vec<String>,
}

impl CorePageVerificationResult {
    /// The C# positional record constructor; `diagnostic_lines` starts empty.
    pub fn new(
        succeeded: bool,
        original_page: Option<u8>,
        restored_page: Option<u8>,
        snapshot: Option<CorePageSnapshot>,
        status: impl Into<String>,
        page_selection_attempted: bool,
        page_restoration_attempted: bool,
    ) -> CorePageVerificationResult {
        CorePageVerificationResult {
            succeeded,
            original_page,
            restored_page,
            snapshot,
            status: status.into(),
            page_selection_attempted,
            page_restoration_attempted,
            diagnostic_lines: Vec::new(),
        }
    }
}

fn opt_text<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

/// A bounded diagnostic. The caller supplies the existing Renesas bus lock and an already
/// identified regulator transport. No voltage, mode, Intel mailbox, or driver-lifecycle API is exposed.
/// (C# `CorePageProbe.Verify`.)
pub fn verify(
    transport: &dyn CorePageProbeTransport,
    acquire_bus: impl FnOnce() -> Result<bool>,
    release_bus: impl FnOnce() -> Result<()>,
) -> CorePageVerificationResult {
    let mut original: Option<u8> = None;
    let mut restored: Option<u8> = None;
    let mut snapshot: Option<CorePageSnapshot> = None;
    let mut failure: Option<String> = None;
    let mut restoration_failure: Option<String> = None;
    let mut acquired = false;
    let mut page_context = false;
    let mut selected = false;
    let mut restoring = false;

    let body = (|| -> Result<()> {
        acquired = acquire_bus()?;
        if !acquired {
            bail!("Timed out waiting for the Renesas regulator bus; no register access attempted.");
        }
        let Some(page) = transport.read_page()? else {
            bail!("Regulator PAGE read failed; no page change attempted.");
        };
        original = Some(page);
        if !(page == 0 || page == 1) {
            bail!("Regulator PAGE {page} is outside the verified 0/1 diagnostic scope; no page change attempted.");
        }
        page_context = true;
        if page != 0 {
            selected = true; // A failed write may still have changed the physical page.
            if !transport.write_page(0)? {
                bail!("Selecting core PAGE 0 failed.");
            }
        }
        if transport.read_page()? != Some(0) {
            bail!("Core PAGE 0 selection did not read back.");
        }
        let Some(mode) = transport.read_mode()? else { bail!("Core register 0xF0 read failed.") };
        let Some(target) = transport.read_target()? else { bail!("Core register 0x21 read failed.") };
        let Some(secondary) = transport.read_secondary_target()? else { bail!("Core register 0x23 read failed.") };
        let snap = CorePageSnapshot::new(mode, target, secondary);
        snapshot = Some(snap);
        if transport.read_page()? != Some(0) {
            bail!("Regulator PAGE changed while reading core registers.");
        }
        validate_snapshot(&snap)
    })();
    if let Err(ex) = body {
        failure = Some(ex.to_string());
    }

    if acquired {
        let restore = (|| -> Result<()> {
            if page_context {
                // Re-read before restoring. If the read fails, still attempt the saved page.
                let current = transport.read_page().ok().flatten();
                let mut write_ok = true;
                if current != original {
                    restoring = true;
                    write_ok = transport.write_page(original.unwrap())?;
                }
                restored = transport.read_page()?;
                if !write_ok || restored != original {
                    restoration_failure = Some(format!(
                        "Original regulator PAGE {} restoration could not be verified (read {}).",
                        opt_text(original),
                        restored.map(|r| r.to_string()).unwrap_or_else(|| "failed".to_string())
                    ));
                }
            }
            Ok(())
        })();
        if let Err(ex) = restore {
            restoration_failure = Some(format!("Original regulator PAGE restoration failed: {ex}"));
        }
        if let Err(ex) = release_bus() {
            restoration_failure = Some(
                restoration_failure.map(|r| r + " ").unwrap_or_default() + "Regulator bus lock release failed: " + &ex.to_string(),
            );
        }
    }

    let succeeded = failure.is_none() && restoration_failure.is_none() && snapshot.is_some() && restored == original;
    let status = if succeeded {
        let s = snapshot.unwrap();
        format!(
            "Core PAGE 0 verified: mode {}, target {:.3} V, secondary 0x{:04X}; original PAGE {} restored. No voltage or mode changed.",
            s.mode(),
            s.programmed_target_volts(),
            s.secondary_target_mv,
            opt_text(original)
        )
    } else {
        [failure, restoration_failure].into_iter().flatten().collect::<Vec<_>>().join(" ")
    };
    CorePageVerificationResult::new(succeeded, original, restored, snapshot, status, selected, restoring)
}

/// C# `CorePageProbe.ValidateSnapshot`.
pub fn validate_snapshot(snapshot: &CorePageSnapshot) -> Result<()> {
    if snapshot.mode_register == u16::MAX {
        bail!("Core mode register returned 0xFFFF; controller identity/state is not verified.");
    }
    if snapshot.target_mv != 0 && (snapshot.target_mv < 600 || snapshot.target_mv > 1720) {
        bail!(
            "Core target register {} mV is outside the existing 0.600–1.720 V control range; activation refused.",
            snapshot.target_mv
        );
    }
    if snapshot.secondary_target_mv != 0 && (snapshot.secondary_target_mv < 600 || snapshot.secondary_target_mv > 1720) {
        bail!(
            "Core secondary target {} mV is outside the verified range; activation refused.",
            snapshot.secondary_target_mv
        );
    }
    if snapshot.mode() == 3 && snapshot.target_mv == 0 {
        bail!("Core override mode has no programmed target; activation refused.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Port of tests/CorePageProbe.Tests (fake-only; no driver).
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    struct FakeTransport {
        page: Cell<u8>,
        mode: u16,
        target: u16,
        secondary: u16,
        held: Cell<bool>,
        acquire_result: bool,
        fail_first_page_read: bool,
        selection_reports_failure: bool,
        ignore_selection: bool,
        ignore_restore: bool,
        fail_restore: bool,
        throw_selection: bool,
        throw_restore: bool,
        throw_core_read: bool,
        drift_during_snapshot: bool,
        acquires: Cell<i32>,
        releases: Cell<i32>,
        accesses: Cell<i32>,
        core_reads: Cell<i32>,
        fail_core_read: i32,
        writes: RefCell<Vec<u8>>,
        page_reads: Cell<i32>,
        failing_page_reads: RefCell<HashMap<i32, bool>>,
    }

    impl Default for FakeTransport {
        fn default() -> Self {
            FakeTransport {
                page: Cell::new(1),
                mode: 0x30,
                target: 1280,
                secondary: 0,
                held: Cell::new(false),
                acquire_result: true,
                fail_first_page_read: false,
                selection_reports_failure: false,
                ignore_selection: false,
                ignore_restore: false,
                fail_restore: false,
                throw_selection: false,
                throw_restore: false,
                throw_core_read: false,
                drift_during_snapshot: false,
                acquires: Cell::new(0),
                releases: Cell::new(0),
                accesses: Cell::new(0),
                core_reads: Cell::new(0),
                fail_core_read: -1,
                writes: RefCell::new(Vec::new()),
                page_reads: Cell::new(0),
                failing_page_reads: RefCell::new(HashMap::new()),
            }
        }
    }

    impl FakeTransport {
        fn fail_page_read(&self, index: i32, throws: bool) {
            self.failing_page_reads.borrow_mut().insert(index, throws);
        }
        fn access(&self) -> Result<()> {
            if !self.held.get() {
                bail!("Transport access outside Renesas lease");
            }
            self.accesses.set(self.accesses.get() + 1);
            Ok(())
        }
        fn read_core(&self, index: i32, value: u16) -> Result<Option<u16>> {
            self.access()?;
            self.core_reads.set(self.core_reads.get() + 1);
            if self.page.get() != 0 {
                bail!("Reading a non-core page");
            }
            if self.throw_core_read {
                bail!("Synthetic transport read failure");
            }
            if self.fail_core_read == index {
                return Ok(None);
            }
            if index == 2 && self.drift_during_snapshot {
                self.page.set(1);
            }
            Ok(Some(value))
        }
        fn writes(&self) -> Vec<u8> {
            self.writes.borrow().clone()
        }
    }

    impl CorePageProbeTransport for FakeTransport {
        fn read_page(&self) -> Result<Option<u8>> {
            self.access()?;
            self.page_reads.set(self.page_reads.get() + 1);
            if let Some(&throws) = self.failing_page_reads.borrow().get(&self.page_reads.get()) {
                if throws {
                    bail!("Synthetic PAGE read failure");
                }
                return Ok(None);
            }
            if self.fail_first_page_read && self.page_reads.get() == 1 {
                return Ok(None);
            }
            Ok(Some(self.page.get()))
        }
        fn write_page(&self, page: u8) -> Result<bool> {
            self.access()?;
            if !(page == 0 || page == 1) {
                bail!("Unsupported PAGE write");
            }
            self.writes.borrow_mut().push(page);
            if page == 1 && self.fail_restore {
                return Ok(false);
            }
            if !(page == 0 && self.ignore_selection) && !(page == 1 && self.ignore_restore) {
                self.page.set(page);
            }
            if page == 0 && self.throw_selection {
                bail!("Synthetic select failure after physical change");
            }
            if page == 1 && self.throw_restore {
                bail!("Synthetic restore failure after physical change");
            }
            Ok(!(page == 0 && self.selection_reports_failure))
        }
        fn read_mode(&self) -> Result<Option<u16>> {
            self.read_core(0, self.mode)
        }
        fn read_target(&self) -> Result<Option<u16>> {
            self.read_core(1, self.target)
        }
        fn read_secondary_target(&self) -> Result<Option<u16>> {
            self.read_core(2, self.secondary)
        }
    }

    fn run(io: &FakeTransport) -> CorePageVerificationResult {
        verify(
            io,
            || {
                io.acquires.set(io.acquires.get() + 1);
                io.held.set(io.acquire_result);
                Ok(io.acquire_result)
            },
            || {
                io.releases.set(io.releases.get() + 1);
                io.held.set(false);
                Ok(())
            },
        )
    }

    fn check(ok: bool, message: &str) {
        assert!(ok, "{message}");
    }

    #[test]
    fn page_1_core_snapshot_restores_page_1_with_only_page_writes() {
        let io = FakeTransport::default();
        let r = run(&io);
        check(r.succeeded && r.original_page == Some(1) && r.restored_page == Some(1) && io.page.get() == 1, &r.status);
        check(io.writes() == vec![0, 1], "PAGE write scope differs");
        check(r.snapshot == Some(CorePageSnapshot::new(0x30, 1280, 0)), "Wrong-page snapshot");
        check(io.acquires.get() == 1 && io.releases.get() == 1 && !io.held.get(), "Bus lease lost");
    }

    #[test]
    fn page_0_verification_performs_no_page_writes() {
        let io = FakeTransport { page: Cell::new(0), ..Default::default() };
        let r = run(&io);
        check(r.succeeded && io.writes().is_empty(), &r.status);
    }

    #[test]
    fn bus_timeout_performs_no_register_access() {
        let io = FakeTransport { acquire_result: false, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.accesses.get() == 0 && io.releases.get() == 0, "Access after lock timeout");
    }

    #[test]
    fn unknown_page_refuses_all_changes() {
        let io = FakeTransport { page: Cell::new(2), ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.writes().is_empty() && io.accesses.get() == 1 && io.releases.get() == 1, &r.status);
    }

    #[test]
    fn missing_original_page_refuses_all_writes() {
        let io = FakeTransport { fail_first_page_read: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.writes().is_empty() && io.releases.get() == 1, &r.status);
    }

    #[test]
    fn selection_failed_after_changing_the_page_still_restores_original() {
        let io = FakeTransport { selection_reports_failure: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && io.writes() == vec![0, 1], &r.status);
    }

    #[test]
    fn selection_readback_mismatch_prevents_core_reads() {
        let io = FakeTransport { ignore_selection: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && io.core_reads.get() == 0, &r.status);
    }

    #[test]
    fn core_register_failure_restores_original() {
        for failing_register in [0, 1, 2] {
            let io = FakeTransport { fail_core_read: failing_register, ..Default::default() };
            let r = run(&io);
            check(
                !r.succeeded && io.page.get() == 1 && io.writes() == vec![0, 1] && io.releases.get() == 1,
                &format!("core register {failing_register}: {}", r.status),
            );
        }
    }

    #[test]
    fn invalid_core_register_identity_refuses_activation_and_restores() {
        let io = FakeTransport { mode: u16::MAX, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && r.snapshot.is_some(), &r.status);
    }

    #[test]
    fn invalid_target_refuses_activation_and_restores() {
        let io = FakeTransport { target: 65535, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1, &r.status);
    }

    #[test]
    fn adaptive_zero_target_is_real_state_not_measured_vcore() {
        let io = FakeTransport { mode: 0, target: 0, ..Default::default() };
        let r = run(&io);
        let s = r.snapshot.unwrap();
        check(r.succeeded && s.programmed_target_volts() == 0.0 && s.override_target_volts().is_none(), &r.status);
    }

    #[test]
    fn restoration_write_failure_is_reported_and_never_activates() {
        let io = FakeTransport { fail_restore: true, ..Default::default() };
        let r = run(&io);
        check(
            !r.succeeded && r.restored_page == Some(0) && r.status.contains("restoration") && io.releases.get() == 1,
            &r.status,
        );
    }

    #[test]
    fn exception_during_core_read_restores_original_and_releases_bus() {
        let io = FakeTransport { throw_core_read: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && io.releases.get() == 1, &r.status);
    }

    #[test]
    fn page_drift_during_snapshot_is_detected_and_original_restored() {
        let io = FakeTransport { drift_during_snapshot: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && r.status.contains("changed"), &r.status);
    }

    #[test]
    fn mode_preserves_the_actual_programmed_state_and_override_meaning() {
        for mode in [0u16, 1, 2, 3] {
            let io = FakeTransport { mode: mode << 4, target: 1280, ..Default::default() };
            let r = run(&io);
            let s = r.snapshot.unwrap();
            check(r.succeeded && s.mode() == mode as i32 && s.programmed_target_volts() == 1.280, &r.status);
            check(
                s.override_target_volts() == if mode == 3 { Some(1.280) } else { None },
                "Adaptive state was invented as an override target",
            );
            check(io.page.get() == 1 && io.writes() == vec![0, 1], "Mode/target verification altered state");
        }
    }

    #[test]
    fn existing_target_range_boundary_remains_valid() {
        for boundary in [600u16, 1720] {
            let io = FakeTransport { target: boundary, secondary: boundary, ..Default::default() };
            let r = run(&io);
            check(
                r.succeeded
                    && r.snapshot.unwrap().target_mv == boundary
                    && r.snapshot.unwrap().secondary_target_mv == boundary
                    && io.page.get() == 1,
                &r.status,
            );
        }
    }

    #[test]
    fn target_outside_existing_range_fails_closed() {
        for invalid in [599u16, 1721] {
            let io = FakeTransport { target: invalid, ..Default::default() };
            let r = run(&io);
            check(!r.succeeded && io.page.get() == 1 && io.releases.get() == 1, &r.status);
        }
    }

    #[test]
    fn secondary_target_refuses_activation_and_restores() {
        for invalid in [599u16, 1721, u16::MAX] {
            let io = FakeTransport { secondary: invalid, ..Default::default() };
            let r = run(&io);
            check(!r.succeeded && io.page.get() == 1 && io.releases.get() == 1, &r.status);
        }
    }

    #[test]
    fn override_mode_with_zero_target_refuses_activation() {
        let io = FakeTransport { mode: 0x30, target: 0, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && io.releases.get() == 1, &r.status);
    }

    #[test]
    fn selection_readback_failure_prevents_every_core_read_and_restores_page() {
        for throws in [false, true] {
            let io = FakeTransport::default();
            io.fail_page_read(2, throws);
            let r = run(&io);
            check(
                !r.succeeded && io.core_reads.get() == 0 && io.page.get() == 1 && r.restored_page == Some(1) && io.releases.get() == 1,
                &r.status,
            );
        }
    }

    #[test]
    fn post_snapshot_page_failure_cannot_activate_a_readable_looking_target() {
        for throws in [false, true] {
            let io = FakeTransport::default();
            io.fail_page_read(3, throws);
            let r = run(&io);
            check(
                !r.succeeded && r.snapshot.is_some() && io.page.get() == 1 && r.restored_page == Some(1) && io.releases.get() == 1,
                &r.status,
            );
        }
    }

    #[test]
    fn restoration_pre_read_failure_still_attempts_saved_page_and_verifies_it() {
        for throws in [false, true] {
            let io = FakeTransport::default();
            io.fail_page_read(4, throws);
            let r = run(&io);
            check(
                r.succeeded
                    && r.page_restoration_attempted
                    && r.restored_page == Some(1)
                    && io.page.get() == 1
                    && io.writes() == vec![0, 1]
                    && io.releases.get() == 1,
                &r.status,
            );
        }
    }

    #[test]
    fn restoration_final_readback_failure_refuses_activation_and_releases_bus() {
        for throws in [false, true] {
            let io = FakeTransport::default();
            io.fail_page_read(5, throws);
            let r = run(&io);
            check(
                !r.succeeded && r.snapshot.is_some() && io.page.get() == 1 && r.status.contains("restoration") && io.releases.get() == 1,
                &r.status,
            );
        }
    }

    #[test]
    fn restoration_acknowledged_without_physical_restore_refuses_activation() {
        let io = FakeTransport { ignore_restore: true, ..Default::default() };
        let r = run(&io);
        check(
            !r.succeeded && r.restored_page == Some(0) && r.page_restoration_attempted && io.releases.get() == 1,
            &r.status,
        );
    }

    #[test]
    fn selection_exception_after_physical_page_change_still_restores_original_and_releases_bus() {
        let io = FakeTransport { throw_selection: true, ..Default::default() };
        let r = run(&io);
        check(
            !r.succeeded && io.page.get() == 1 && r.restored_page == Some(1) && io.core_reads.get() == 0 && io.releases.get() == 1,
            &r.status,
        );
    }

    #[test]
    fn restoration_exception_after_physical_change_refuses_activation_and_releases_bus() {
        let io = FakeTransport { throw_restore: true, ..Default::default() };
        let r = run(&io);
        check(!r.succeeded && io.page.get() == 1 && r.snapshot.is_some() && io.releases.get() == 1, &r.status);
    }

    #[test]
    fn acquisition_exception_does_not_access_transport_or_release_an_unowned_lock() {
        let io = FakeTransport::default();
        let r = verify(
            &io,
            || {
                io.acquires.set(io.acquires.get() + 1);
                bail!("Synthetic acquire failure")
            },
            || {
                io.releases.set(io.releases.get() + 1);
                Ok(())
            },
        );
        check(!r.succeeded && io.acquires.get() == 1 && io.accesses.get() == 0 && io.releases.get() == 0, &r.status);
    }

    #[test]
    fn release_exception_refuses_activation_even_after_verified_snapshot_and_restore() {
        let io = FakeTransport::default();
        let r = verify(
            &io,
            || {
                io.acquires.set(io.acquires.get() + 1);
                io.held.set(true);
                Ok(true)
            },
            || {
                io.releases.set(io.releases.get() + 1);
                io.held.set(false);
                bail!("Synthetic release failure")
            },
        );
        check(
            !r.succeeded
                && r.snapshot.is_some()
                && r.restored_page == Some(1)
                && io.page.get() == 1
                && io.releases.get() == 1
                && !io.held.get()
                && r.status.contains("lock release"),
            &r.status,
        );
    }

    #[test]
    fn page_0_snapshot_drift_is_rejected_and_original_page_0_restored() {
        let io = FakeTransport { page: Cell::new(0), drift_during_snapshot: true, ..Default::default() };
        let r = run(&io);
        check(
            !r.succeeded
                && r.original_page == Some(0)
                && r.restored_page == Some(0)
                && io.page.get() == 0
                && io.writes() == vec![0]
                && io.releases.get() == 1,
            &r.status,
        );
    }

    #[test]
    fn page_0_core_register_failure_releases_bus_without_page_writes() {
        for failing_register in [0, 1, 2] {
            let io = FakeTransport { page: Cell::new(0), fail_core_read: failing_register, ..Default::default() };
            let r = run(&io);
            check(
                !r.succeeded
                    && r.original_page == Some(0)
                    && r.restored_page == Some(0)
                    && io.writes().is_empty()
                    && io.releases.get() == 1,
                &r.status,
            );
        }
    }
}
