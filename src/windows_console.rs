// Console output helpers for a `windows_subsystem = "windows"` binary.
//
// With the windows subsystem the process has no console of its own. To still
// show --help / usage errors when launched from a terminal, we attach to the
// parent process's console (cmd / PowerShell / Windows Terminal) and write
// directly to its output handle. If there is no parent console (double-click
// from Explorer), we fall back to a Win32 message box.

/// Write `msg` to the parent console (stdout or stderr depending on
/// `is_error`). If no parent console exists, shows a message box instead.
#[cfg(target_os = "windows")]
pub fn print_to_console(msg: &str, is_error: bool) {
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    unsafe {
        // Attach to the parent process's console. Returns 0 if there is none
        // (e.g. launched via Explorer double-click).
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            show_message_box(msg, is_error);
            return;
        }

        let handle = if is_error {
            GetStdHandle(STD_ERROR_HANDLE)
        } else {
            GetStdHandle(STD_OUTPUT_HANDLE)
        };

        // GetStdHandle can return null or INVALID_HANDLE_VALUE (-1 as pointer).
        if handle.is_null() || handle as usize == usize::MAX {
            return;
        }

        // Wrap the OS handle in a File to use the Write trait. We must NOT
        // close this handle (it belongs to the console), so forget the File.
        let f = std::fs::File::from_raw_handle(handle as _);
        let _ = (&f).write_all(msg.as_bytes());
        std::mem::forget(f);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn print_to_console(msg: &str, is_error: bool) {
    if is_error {
        eprint!("{}", msg);
    } else {
        print!("{}", msg);
    }
}

/// Show a Win32 message box. Used as a fallback when there is no parent
/// console, and directly for fatal errors (e.g. port-in-use at startup).
#[cfg(target_os = "windows")]
pub fn show_message_box(msg: &str, is_error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    };

    let title = if is_error {
        "SMS Notifier - Error"
    } else {
        "SMS Notifier"
    };
    let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let flags = MB_OK
        | if is_error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            msg_wide.as_ptr(),
            title_wide.as_ptr(),
            flags,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub fn show_message_box(msg: &str, is_error: bool) {
    if is_error {
        eprint!("{}", msg);
    } else {
        print!("{}", msg);
    }
}
