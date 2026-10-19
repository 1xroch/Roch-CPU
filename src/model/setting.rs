//! One tunable row in the main window.

use std::sync::Arc;

use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingGroup {
    Clocks,
    Voltages,
    Power,
    Pbo,
    Memory,
    Board,
}

/// Reads the current setting; `Ok(None)` means "Auto"/not set. An `Err` is a failed read.
pub type ReadFn = Arc<dyn Fn() -> Result<Option<f64>> + Send + Sync>;
/// What the hardware is doing right now against this limit (draw, current, temperature).
pub type LiveFn = Arc<dyn Fn() -> Option<f64> + Send + Sync>;
pub type WriteFn = Arc<dyn Fn(f64) -> Result<()> + Send + Sync>;
pub type RestoreFn = Arc<dyn Fn() -> Result<()> + Send + Sync>;

#[derive(Clone)]
pub struct Setting {
    pub id: String,
    pub name: String,
    pub group: SettingGroup,
    pub unit: String,
    pub min: f64,
    pub max: f64,
    pub decimals: usize,
    pub read: ReadFn,
    /// `None`: nothing live to show.
    pub live: Option<LiveFn>,
    /// Applies a value. `None` makes the row read-only.
    pub write: Option<WriteFn>,
    /// Called when the user enters 0 (restore default). `None`: write `default_value`.
    pub restore_default: Option<RestoreFn>,
    pub note: Option<String>,
    /// Why `max` is where it is, when that is not obvious; added to the out-of-range message.
    pub limit_note: Option<String>,
    pub available: bool,
    pub require_read_back: bool,

    pub default_value: Option<f64>,
    pub current: Option<f64>,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
}

impl Setting {
    /// A row with the given identity and every optional field empty: read-only, reads Auto.
    pub fn new(id: impl Into<String>, name: impl Into<String>, group: SettingGroup) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            group,
            unit: String::new(),
            min: 0.0,
            max: 0.0,
            decimals: 0,
            read: Arc::new(|| Ok(None)),
            live: None,
            write: None,
            restore_default: None,
            note: None,
            limit_note: None,
            available: true,
            require_read_back: false,
            default_value: None,
            current: None,
            last_error: None,
            last_result: None,
        }
    }

    pub fn read_only(&self) -> bool {
        self.write.is_none()
    }

    pub fn range_text(&self) -> String {
        if self.unit.is_empty() {
            format!("Min:{:.1}, Max:{:.1}", self.min, self.max)
        } else {
            format!("Min:{} {u}, Max:{} {u}", self.format(self.min), self.format(self.max), u = self.unit)
        }
    }

    pub fn format(&self, v: f64) -> String {
        format_fixed(v, self.decimals)
    }

    pub fn current_text(&self) -> String {
        match self.current {
            Some(v) => self.format(v),
            None => "Auto".into(),
        }
    }

    /// Parses user text: "auto" means 0 (restore), commas are accepted as decimal points.
    pub fn try_parse(&self, text: &str) -> Option<f64> {
        let t = text.trim().replace(',', ".");
        if t.eq_ignore_ascii_case("auto") {
            return Some(0.0);
        }
        t.parse::<f64>().ok().filter(|v| v.is_finite())
    }
}

/// Fixed-point formatting with invariant culture, as .NET's "F<n>" format specifier.
pub fn format_fixed(v: f64, decimals: usize) -> String {
    format!("{v:.decimals$}")
}
