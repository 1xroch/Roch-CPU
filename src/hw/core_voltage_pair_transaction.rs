//! Existing regulator-first / Intel-mailbox-second transaction, under one PAGE lease.

use anyhow::{Result, anyhow};

use super::msi_core_voltage::{MsiCoreVoltage, State};

struct Pair<M, R> {
    before: Option<State>,
    mailbox_before: Option<M>,
    restore_mailbox: Option<R>,
    captured: bool,
    regulator_attempted: bool,
    mailbox_attempted: bool,
    rolled_back: bool,
}

fn roll_back<M, R: FnOnce(M) -> Result<()>>(st: &mut Pair<M, R>, regulator: &MsiCoreVoltage, failure: anyhow::Error) -> anyhow::Error {
    st.rolled_back = true;
    let mut failures = vec![failure];
    if st.captured {
        if st.mailbox_attempted {
            if let (Some(restore), Some(before)) = (st.restore_mailbox.take(), st.mailbox_before.take()) {
                if let Err(ex) = restore(before) {
                    failures.push(ex);
                }
            }
        }
        if st.regulator_attempted {
            if let Some(state) = st.before {
                if let Err(ex) = regulator.restore(state) {
                    failures.push(ex);
                }
            }
        }
    }
    if failures.len() > 1 {
        // .NET AggregateException(message, inner…) message format.
        let mut text = "Core voltage transaction or rollback failed.".to_string();
        for f in &failures {
            text.push_str(&format!(" ({f})"));
        }
        return anyhow!(text);
    }
    failures.pop().unwrap()
}

/// Existing regulator-first / Intel-mailbox-second transaction, under one PAGE lease.
/// (C# `CoreVoltagePairTransaction.Execute<TMailboxState>`.)
pub fn execute<M>(
    regulator: &MsiCoreVoltage,
    read_mailbox: impl FnOnce() -> Result<M>,
    write_regulator: impl FnOnce() -> Result<()>,
    write_mailbox: impl FnOnce() -> Result<()>,
    verify_mailbox: impl FnOnce() -> Result<()>,
    restore_mailbox: impl FnOnce(M) -> Result<()>,
) -> Result<()> {
    let mut st = Pair {
        before: None,
        mailbox_before: None,
        restore_mailbox: Some(restore_mailbox),
        captured: false,
        regulator_attempted: false,
        mailbox_attempted: false,
        rolled_back: false,
    };
    let outcome = regulator.with_core_page_for_apply(|state| {
        st.mailbox_before = Some(read_mailbox()?);
        st.before = Some(state);
        st.captured = true;
        let steps = (|| -> Result<()> {
            st.regulator_attempted = true;
            write_regulator()?;
            st.mailbox_attempted = true;
            write_mailbox()?;
            verify_mailbox()
        })();
        match steps {
            Ok(()) => Ok(()),
            Err(failure) => Err(roll_back(&mut st, regulator, failure)),
        }
    });
    match outcome {
        Ok(()) => Ok(()),
        // Includes failures restoring PAGE after the pair was otherwise successful.
        // An unread mailbox baseline never permits target writes or invented rollback.
        Err(failure) if st.rolled_back => Err(failure),
        Err(failure) => Err(roll_back(&mut st, regulator, failure)),
    }
}

#[cfg(test)]
mod tests {
    //! The pair-transaction checks from tests/CorePageProbe.LowLevel.Tests.
    use super::*;
    use crate::hw::ec_mailbox::EcMailbox;
    use crate::hw::ec_mailbox::fakes::{EcFakeDriver, fake_sio};
    use anyhow::bail;
    use std::cell::Cell;
    use std::sync::Arc;

    fn check(ok: bool, message: &str) {
        assert!(ok, "{message}");
    }

    fn with_control(test: impl FnOnce(&Arc<EcFakeDriver>, &MsiCoreVoltage)) {
        let driver = EcFakeDriver::new();
        let sio = fake_sio(&driver);
        let (control, result) = MsiCoreVoltage::startup_for_test(&EcMailbox::new(sio));
        check(result.succeeded && control.is_some(), &result.status);
        driver.st().regulator_writes.clear();
        test(&driver, control.as_ref().unwrap());
    }

    #[test]
    fn shared_hardware_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<crate::hw::super_io::SuperIo>();
        assert_send_sync::<EcMailbox>();
        assert_send_sync::<crate::hw::ec_clock_gen::EcClockGen>();
        assert_send_sync::<crate::hw::bclk_controller::BclkController>();
        assert_send_sync::<crate::hw::vdd2_rail::Vdd2Rail>();
        assert_send_sync::<MsiCoreVoltage>();
    }

    fn must_fail(r: Result<()>) {
        check(r.is_err(), "Failure was incorrectly reported successful");
    }

    #[test]
    fn pair_captures_mailbox_before_any_voltage_write() {
        with_control(|driver, control| {
            let mailbox_writes = Cell::new(0);
            must_fail(execute::<i32>(
                control,
                || bail!("read failure"),
                || control.set_override(1.320),
                || {
                    mailbox_writes.set(mailbox_writes.get() + 1);
                    Ok(())
                },
                || Ok(()),
                |_| {
                    mailbox_writes.set(mailbox_writes.get() + 1);
                    Ok(())
                },
            ));
            let s = driver.st();
            check(
                s.page == 1 && s.target == 1280 && mailbox_writes.get() == 0 && s.regulator_writes.iter().all(|w| w.0 == 0),
                "Unread baseline permitted a target write",
            );
        });
    }

    #[test]
    fn pair_success_preserves_original_page_and_regulator_first_mailbox_second_ordering() {
        with_control(|driver, control| {
            let mailbox_target = Cell::new(1280);
            execute(
                control,
                || Ok(mailbox_target.get()),
                || control.set_override(1.320),
                || {
                    {
                        let s = driver.st();
                        check(s.page == 0 && s.target == 1320, "Mailbox preceded regulator or lost core PAGE lease");
                    }
                    mailbox_target.set(1320);
                    Ok(())
                },
                || {
                    check(mailbox_target.get() == 1320, "Mailbox target incorrect");
                    Ok(())
                },
                |before| {
                    mailbox_target.set(before);
                    Ok(())
                },
            )
            .unwrap();
            let s = driver.st();
            check(s.page == 1 && s.target == 1320 && mailbox_target.get() == 1320, "Pair did not complete exactly");
        });
    }

    #[test]
    fn pair_mailbox_failure_restores_both_captured_targets_while_preserving_page() {
        with_control(|driver, control| {
            let mailbox_target = Cell::new(1280);
            must_fail(execute(
                control,
                || Ok(mailbox_target.get()),
                || control.set_override(1.320),
                || {
                    mailbox_target.set(1320);
                    bail!("rejected mailbox write")
                },
                || Ok(()),
                |before| {
                    check(driver.page() == 0, "Rollback lost the core PAGE lease");
                    mailbox_target.set(before);
                    Ok(())
                },
            ));
            let s = driver.st();
            check(s.page == 1 && s.target == 1280 && mailbox_target.get() == 1280, "Pair rollback left changed targets");
        });
    }

    #[test]
    fn pair_failed_mailbox_readback_rolls_back_both_targets() {
        with_control(|driver, control| {
            let mailbox_target = Cell::new(1280);
            must_fail(execute(
                control,
                || Ok(mailbox_target.get()),
                || control.set_override(1.320),
                || {
                    mailbox_target.set(1320);
                    Ok(())
                },
                || bail!("readback mismatch"),
                |before| {
                    mailbox_target.set(before);
                    Ok(())
                },
            ));
            let s = driver.st();
            check(
                s.page == 1 && s.target == 1280 && mailbox_target.get() == 1280,
                "Readback mismatch did not restore both targets",
            );
        });
    }

    #[test]
    fn pair_regulator_failure_never_writes_intel_mailbox_target() {
        with_control(|driver, control| {
            let mailbox_writes = Cell::new(0);
            driver.st().fail_target_write_once = true;
            must_fail(execute(
                control,
                || Ok(1280),
                || control.set_override(1.320),
                || {
                    mailbox_writes.set(mailbox_writes.get() + 1);
                    Ok(())
                },
                || Ok(()),
                |_| {
                    mailbox_writes.set(mailbox_writes.get() + 1);
                    Ok(())
                },
            ));
            let s = driver.st();
            check(
                s.page == 1 && s.target == 1280 && mailbox_writes.get() == 0,
                "Failed regulator unnecessarily changed mailbox",
            );
        });
    }

    #[test]
    fn pair_page_restoration_failure_is_reported_and_rolls_both_targets_back() {
        with_control(|driver, control| {
            let mailbox_target = Cell::new(1280);
            driver.st().fail_page_restore_once = true;
            must_fail(execute(
                control,
                || Ok(mailbox_target.get()),
                || control.set_override(1.320),
                || {
                    mailbox_target.set(1320);
                    Ok(())
                },
                || Ok(()),
                |before| {
                    mailbox_target.set(before);
                    Ok(())
                },
            ));
            let s = driver.st();
            check(
                s.page == 1 && s.target == 1280 && mailbox_target.get() == 1280,
                "PAGE restoration failure left new target pair",
            );
        });
    }
}
