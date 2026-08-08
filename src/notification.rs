use tracing::error;

#[cfg(target_os = "windows")]
use crate::keyboard;

#[cfg(target_os = "windows")]
use crate::windows_registry::APP_ID;

#[cfg(target_os = "windows")]
use tauri_winrt_notification::{Duration, Sound, Toast};

/// Show a native Windows toast notification with explicit title and body.
///
/// Clicking the notification copies the body content to the clipboard.
/// Must be called from the main event-loop thread.
#[cfg(target_os = "windows")]
pub fn notify_sms(title: &str, body: &str) {
    let body_string = body.to_string();
    let result = Toast::new(APP_ID)
        .title(title)
        .text1(body)
        .duration(Duration::Short)
        .sound(Some(Sound::Reminder))
        .on_activated(move |_| {
            for attempt in 0..3 {
                if keyboard::copy_to_clipboard(&body_string) {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(60 * (attempt + 1)));
            }
            error!("Failed to copy SMS body to clipboard after 3 attempts");
            Ok(())
        })
        .show();

    if let Err(e) = result {
        error!("Failed to show notification: {:?}", e);
    }
}

/// Show a native macOS notification with explicit title and body.
///
/// Clicking the notification copies the body content to the clipboard.
/// Spawns a background thread because `send_notification` blocks until the
/// user interacts or the notification auto-dismisses.
#[cfg(target_os = "macos")]
pub fn notify_sms(title: &str, body: &str) {
    use std::sync::Once;

    use crate::keyboard;

    static SET_APP: Once = Once::new();

    let title = title.to_string();
    let body = body.to_string();

    std::thread::spawn(move || {
        // set_application must be called once before sending notifications.
        // CLI apps have no bundle id of their own, so borrow Terminal's.
        SET_APP.call_once(|| {
            let bundle = mac_notification_sys::get_bundle_identifier_or_default("Terminal");
            if let Err(e) = mac_notification_sys::set_application(&bundle) {
                error!("Failed to set notification application: {e:?}");
            }
        });

        let response = mac_notification_sys::send_notification(&title, None, &body, None);

        match response {
            Ok(mac_notification_sys::NotificationResponse::Click) => {
                if !keyboard::copy_to_clipboard(&body) {
                    error!("Failed to copy SMS body to clipboard on notification click");
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("Failed to show notification: {e:?}");
            }
        }
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn notify_sms(_title: &str, _body: &str) {}
