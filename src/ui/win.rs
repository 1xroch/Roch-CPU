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
