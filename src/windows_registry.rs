#[cfg(target_os = "windows")]
use tracing::{error, info};

/// Stable Windows application identity. It must match both the Start Menu
/// shortcut and the AppUserModelID used for toast notifications.
#[cfg(target_os = "windows")]
pub const APP_ID: &str = "howareyouo.SmsNotifier";

/// Register the portable executable for branded Windows toast notifications.
/// No executable is copied or installed; only a per-user Start Menu shortcut
/// is created and kept up to date when the executable is moved.
#[cfg(target_os = "windows")]
pub fn configure() {
    if let Err(error) = set_process_app_id() {
        error!("Failed to set Windows AppUserModelID: {error}");
        return;
    }

    match std::thread::spawn(register_start_menu_shortcut).join() {
        Ok(Ok(())) => info!("Registered Windows toast identity: {APP_ID}"),
        Ok(Err(error)) => error!("Failed to register Windows toast shortcut: {error}"),
        Err(_) => error!("Toast shortcut registration thread panicked"),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn configure() {}

#[cfg(target_os = "windows")]
fn set_process_app_id() -> windows::core::Result<()> {
    use windows::{core::HSTRING, Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID};

    let app_id = HSTRING::from(APP_ID);
    unsafe { SetCurrentProcessExplicitAppUserModelID(&app_id) }
}

#[cfg(target_os = "windows")]
fn register_start_menu_shortcut() -> Result<(), String> {
    use windows::{
        core::{Interface, HSTRING},
        Win32::{
            Storage::EnhancedStorage::PKEY_AppUserModel_ID,
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
                StructuredStorage::PROPVARIANT, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{IShellLinkW, PropertiesSystem::IPropertyStore, ShellLink},
        },
    };

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|error| error.to_string())?;

    let result = (|| -> windows::core::Result<()> {
        let executable = std::env::current_exe().map_err(|_| windows::core::Error::from_win32())?;
        let app_data = std::env::var_os("APPDATA").ok_or_else(windows::core::Error::from_win32)?;
        let shortcut = std::path::PathBuf::from(app_data)
            .join("Microsoft\\Windows\\Start Menu\\Programs")
            .join("SMS Notifier.lnk");
        std::fs::create_dir_all(shortcut.parent().expect("shortcut has a parent"))
            .map_err(|_| windows::core::Error::from_win32())?;

        let executable = HSTRING::from(executable.to_string_lossy().as_ref());
        let shortcut = HSTRING::from(shortcut.to_string_lossy().as_ref());
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?;
        unsafe {
            link.SetPath(&executable)?;
            link.SetIconLocation(&executable, 0)?;

            let property_store: IPropertyStore = link.cast()?;
            let value = PROPVARIANT::from(APP_ID);
            property_store.SetValue(&PKEY_AppUserModel_ID, &value)?;
            property_store.Commit()?;

            let persist: IPersistFile = link.cast()?;
            persist.Save(&shortcut, true)?;
        }
        Ok(())
    })();

    unsafe { CoUninitialize() };
    result.map_err(|error| error.to_string())
}
