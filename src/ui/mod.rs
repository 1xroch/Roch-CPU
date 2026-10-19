//! The window: egui port of the WinForms UI.
//!
//! [`run`] opens the real window over any [`UiModel`] (the hardware model implements it in
//! `hw_adapter.rs`); [`run_preview`] opens the inert `--ui-preview` fixture.

mod curve_optimizer;
mod log_window;
mod main_window;
pub mod model;
mod moon;
mod per_core;
pub mod preview;
mod text;
mod theme;
mod win;

// `UiModel` for the real `HardwareModel`.
mod hw_adapter;

use std::sync::Arc;

use anyhow::anyhow;
use eframe::egui::{self, IconData};

pub use model::UiModel;
use preview::{PreviewModel, PreviewOptions};
use text::APP_NAME;

fn app_icon() -> Arc<IconData> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/mark.png"))
        .unwrap_or(IconData { rgba: vec![0; 4], width: 1, height: 1 });
    Arc::new(icon)
}

fn native_options(icon: Arc<IconData>) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("Roch CPU")
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size([500.0, 760.0])
            .with_min_inner_size([480.0, 400.0])
            .with_icon(icon),
        centered: true,
        persist_window: false,
        ..Default::default()
    }
}

fn run_window(model: Arc<dyn UiModel>, preview: Option<PreviewOptions>) -> anyhow::Result<()> {
    let icon = app_icon();
    let options = native_options(icon.clone());
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(main_window::MainWindow::new(cc, model, preview, icon)))),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Opens the main window over `model` and blocks until it closes. The window calls
/// `model.initialize()` itself, on a worker thread, once its log is subscribed.
pub fn run(model: Arc<dyn UiModel>) -> anyhow::Result<()> {
    run_window(model, None)
}

/// `--ui-preview [--dark] [--scroll-bottom] [--verified-core] [--split-dimms]
/// [--voltage-readings=auto-mixed|error]`: the window over fixed fixture values. No hardware
/// model, drivers or refresh timers are created and no preference is saved. `args` are the
/// arguments after the executable name (a leading `--ui-preview` is skipped). Returns the
/// process exit code: 0, or 2 for an unknown `--voltage-readings` value.
pub fn run_preview(args: &[String]) -> i32 {
    let options = match PreviewOptions::parse(args) {
        Ok(o) => o,
        Err(code) => return code,
    };
    theme::configure_preview(options.dark);
    let model: Arc<dyn UiModel> = Arc::new(PreviewModel::new(options.clone()));
    match run_window(model, Some(options)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
