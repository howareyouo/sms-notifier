use tracing::error;

/// Copy text to the clipboard. Returns true on success.
pub fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(text) {
            Ok(_) => true,
            Err(e) => {
                error!("Failed to set clipboard: {:?}", e);
                false
            }
        },
        Err(e) => {
            error!("Failed to open clipboard: {:?}", e);
            false
        }
    }
}

/// Simulate a Ctrl+V paste keystroke so the freshly copied SMS is immediately
/// pasted into whichever window currently has focus.
#[cfg(target_os = "windows")]
pub fn simulate_paste() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_KEYUP, VK_CONTROL,
    };

    unsafe {
        // Press Ctrl, press V, release V, release Ctrl.
        keybd_event(VK_CONTROL as u8, 0, 0, 0);
        keybd_event(b'V' as u8, 0, 0, 0);
        keybd_event(b'V' as u8, 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_CONTROL as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

/// Simulate a Cmd+V paste keystroke so the freshly copied SMS is immediately
/// pasted into whichever window currently has focus.
#[cfg(target_os = "macos")]
pub fn simulate_paste() {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    // macOS virtual key codes: Command = 0x37 (55), V = 0x09 (9)
    const KEY_V: u16 = 9;
    // CGEventFlagCommand = 0x00100000
    let cmd_flags = CGEventFlags::from_bits_truncate(0x0010_0000);

    let source = match CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
        Ok(s) => s,
        Err(_) => {
            error!("Failed to create CGEventSource for paste");
            return;
        }
    };

    // Press V with Command flag set (this is sufficient for Cmd+V;
    // no need to separately post FlagsChanged for Command).
    if let Ok(event) = CGEvent::new_keyboard_event(source.clone(), KEY_V, true) {
        event.set_flags(cmd_flags);
        event.post(CGEventTapLocation::HID);
    }
    if let Ok(event) = CGEvent::new_keyboard_event(source, KEY_V, false) {
        event.set_flags(cmd_flags);
        event.post(CGEventTapLocation::HID);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn simulate_paste() {}

/// Simulate pressing the Enter key so the pasted SMS is submitted
/// immediately (e.g. in a chat input).
#[cfg(target_os = "windows")]
pub fn simulate_enter() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_KEYUP, VK_RETURN,
    };

    unsafe {
        // Press Enter, release Enter.
        keybd_event(VK_RETURN as u8, 0, 0, 0);
        keybd_event(VK_RETURN as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

/// macOS: simulate Return key via CGEvent.
#[cfg(target_os = "macos")]
pub fn simulate_enter() {
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    // macOS virtual key code: Return = 0x24 (36)
    const KEY_RETURN: u16 = 36;

    let source = match CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
        Ok(s) => s,
        Err(_) => {
            error!("Failed to create CGEventSource for Enter");
            return;
        }
    };

    if let Ok(event) = CGEvent::new_keyboard_event(source.clone(), KEY_RETURN, true) {
        event.post(CGEventTapLocation::HID);
    }
    if let Ok(event) = CGEvent::new_keyboard_event(source, KEY_RETURN, false) {
        event.post(CGEventTapLocation::HID);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn simulate_enter() {}

/// Copy `code` to the clipboard and paste it into the focused window.
///
/// When `submit` is true, also press Enter after a short delay - used by the
/// SMS / `?code=` paths where the focus is reliably the verification-code
/// input field. When false, only paste - used by the mail listener, where the
/// focus is unpredictable at mail-arrival time and an automatic Enter could
/// trigger unintended actions (reply, send, run a half-typed command, ...).
pub fn copy_paste_submit(code: &str, submit: bool) {
    if copy_to_clipboard(code) {
        simulate_paste();
        if submit {
            std::thread::sleep(std::time::Duration::from_millis(100));
            simulate_enter();
        }
    }
}
