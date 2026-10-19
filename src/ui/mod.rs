//! The window: egui port of the WinForms UI. (Placeholder until the UI port lands.)

use std::sync::Arc;

use crate::model::hardware_model::HardwareModel;

pub fn run(_model: Arc<HardwareModel>) -> anyhow::Result<()> {
    anyhow::bail!("UI not ported yet")
}

pub fn run_preview(_args: &[String]) -> i32 {
    1
}
