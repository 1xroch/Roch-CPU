//! PawnIO is a Windows driver. On Linux the AMD SMU power table has no reader yet, so this
//! stands in with the same surface and never loads: the rows that need the table show Auto
//! until a value is written, as on Windows without PawnIO.

use std::path::Path;

use anyhow::{Result, bail};

pub struct PawnIo(());

/// A dotted version, as the Windows uninstall entry stores it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub Vec<u32>);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        write!(f, "{}", parts.join("."))
    }
}

impl PawnIo {
    pub fn installed_version() -> Option<Version> {
        None
    }

    pub fn is_installed() -> bool {
        false
    }

    pub fn last_error(&self) -> Option<String> {
        None
    }

    pub fn load_module(_path: &Path) -> Result<Self> {
        bail!("PawnIO is a Windows driver")
    }

    pub fn execute(&self, _function: &str, _input: &[i64], _output_count: usize) -> Option<Vec<i64>> {
        None
    }
}
