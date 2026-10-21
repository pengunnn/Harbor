#[tauri::command]
pub fn updater_package_kind(_app: tauri::AppHandle) -> &'static str {
    #[cfg(target_os = "linux")]
    {
        use tauri::Manager;

        if std::env::var_os("FLATPAK_ID").is_some()
            || std::path::Path::new("/.flatpak-info").is_file()
        {
            return "flatpak";
        }

        let env = _app.env();
        if let Some(appimage) = env.appimage.as_ref() {
            let path = std::path::Path::new(appimage);
            if path.is_absolute() && path.is_file() {
                return "appimage";
            }
        }

        "system-or-unknown"
    }

    #[cfg(not(target_os = "linux"))]
    {
        "other"
    }
}
