//! The few Win32 calls the window makes itself: the modal Yes/No prompts and opening links.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_ICONWARNING, MB_YESNO, MessageBoxW, SW_SHOWNORMAL,
};

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
