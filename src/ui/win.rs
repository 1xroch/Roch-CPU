//! The few calls the window makes to the desktop itself: the modal Yes/No prompts, the work
//! area and opening links. Win32 on Windows; on Linux the prompt is this same program run as a
//! small dialog window (see [`run_confirm_dialog`]) and links go to `xdg-open`.

#[cfg(windows)]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::ShellExecuteW;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_ICONWARNING, MB_YESNO, MessageBoxW, SW_SHOWNORMAL,
};

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// Warning icon, "No" as the default button (the dangerous-value prompt).
    Warning,
    /// Question icon, "Yes" as the default button.
    Question,
}

/// A modal Yes/No message box owned by the active window, like `MessageBox.Show(this, …)`.
#[cfg(windows)]
pub fn confirm(text: &str, caption: &str, kind: Prompt) -> bool {
    let (text, caption) = (wide(text), wide(caption));
    let flags = match kind {
        Prompt::Warning => MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        Prompt::Question => MB_YESNO | MB_ICONQUESTION,
    };
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
    unsafe { MessageBoxW(GetActiveWindow(), text.as_ptr(), caption.as_ptr(), flags) == IDYES }
}

/// The work area (desktop minus the taskbar) of the monitor containing `point`, in physical
/// pixels as `(left, top, right, bottom)` - WinForms' `Screen.FromPoint(p).WorkingArea`.
#[cfg(windows)]
pub fn work_area_at(x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
    // SAFETY: plain Win32 calls; `info` is a correctly sized MONITORINFO that outlives the call.
    unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let r = info.rcWork;
        Some((r.left, r.top, r.right, r.bottom))
    }
}

/// Opens a URL in the default browser (`Process.Start` with `UseShellExecute`). Errors are ignored.
#[cfg(windows)]
pub fn open_url(url: &str) {
    let (verb, file) = (wide("open"), wide(url));
    // SAFETY: NUL-terminated buffers that outlive the call; null parameters and directory are allowed.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// The argument that runs this executable as the Yes/No prompt (Linux).
#[cfg(not(windows))]
pub const CONFIRM_ARG: &str = "--confirm-dialog";

/// A modal Yes/No prompt. Linux has no system message box, so this program opens one of its own
/// in a child process and waits for the answer, which blocks the caller as `MessageBox` does.
/// Anything but a clear Yes is No.
#[cfg(not(windows))]
pub fn confirm(text: &str, caption: &str, kind: Prompt) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let kind = match kind {
        Prompt::Warning => "warning",
        Prompt::Question => "question",
    };
    std::process::Command::new(exe)
        .args([CONFIRM_ARG, kind, caption, text])
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.code() == Some(0))
}

/// No portable way to ask a Wayland compositor for the work area; the caller uses the monitor.
#[cfg(not(windows))]
pub fn work_area_at(_x: i32, _y: i32) -> Option<(i32, i32, i32, i32)> {
    None
}

/// Opens a URL with the desktop's handler. Errors are ignored.
#[cfg(not(windows))]
pub fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `--confirm-dialog <warning|question> <caption> <text>`: a small window with the text and
/// Yes/No. Exit code 0 is Yes, 1 is No or closed. On a warning, No has the focus, as
/// `MB_DEFBUTTON2` gives it on Windows.
#[cfg(not(windows))]
pub fn run_confirm_dialog(args: &[String]) -> i32 {
    use eframe::egui;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let warning = args.first().is_some_and(|k| k == "warning");
    let caption = args.get(1).cloned().unwrap_or_else(|| super::text::APP_NAME.to_string());
    let text = args.get(2).cloned().unwrap_or_default();
    let yes = Arc::new(AtomicBool::new(false));
    let answer = yes.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&caption)
            .with_app_id("Roch CPU")
            .with_inner_size([440.0, 200.0])
            .with_resizable(false)
            .with_icon(super::app_icon()),
        centered: true,
        persist_window: false,
        ..Default::default()
    };
    let result = eframe::run_ui_native(&caption, options, move |ui, _frame| {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(if warning { "!" } else { "?" }).size(28.0).strong());
                ui.add(egui::Label::new(&text).wrap());
            });
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let no = ui.add_sized([80.0, 26.0], egui::Button::new("No"));
                let yes = ui.add_sized([80.0, 26.0], egui::Button::new("Yes"));
                if ui.memory(|m| m.focused().is_none()) {
                    if warning { no.request_focus() } else { yes.request_focus() }
                }
                if yes.clicked() {
                    answer.store(true, Ordering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if no.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    });
    if result.is_ok() && yes.load(Ordering::SeqCst) { 0 } else { 1 }
}
