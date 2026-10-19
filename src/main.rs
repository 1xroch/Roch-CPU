#![cfg_attr(not(test), windows_subsystem = "windows")]

mod hw;
mod model;
mod ui;

mod cli;

use std::sync::Arc;

use model::hardware_model::HardwareModel;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // This branch returns before diagnostics and the production model. The preview does not
    // create HardwareModel, initialize drivers, run timers or save user preferences.
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("--ui-preview")) {
        return ui::run_preview(&args[1..]);
    }

    // The diagnostic switches print to the console they were launched from; a windows-subsystem
    // executable has none of its own.
    if !args.is_empty() {
        hw::native::attach_parent_console();
        return cli::run(&args);
    }

    let model = Arc::new(HardwareModel::new());
    // A panic on the UI thread must still drop the model, so the driver service is removed.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui::run(model.clone() as Arc<dyn ui::model::UiModel>)));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => show_error("Roch CPU - unexpected error", &format!("{e:?}")),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            show_error("Roch CPU - unexpected error", &msg);
        }
    }
    // Worker threads (start-up probe, BCLK measurement) hold the model too. Give them a moment
    // to finish so the last reference - and with it the driver service cleanup - is dropped here,
    // not lost when the process exits.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while Arc::strong_count(&model) > 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    drop(model);
    0
}

/// A modal error box (the .NET `MessageBox.Show` with the error icon).
fn show_error(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let (t, m) = (hw::native::wide(title), hw::native::wide(text));
    unsafe { MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR) };
}
