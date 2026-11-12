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

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn updater_delta_available(app: tauri::AppHandle) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    use tauri::Manager;

    if updater_package_kind(app.clone()) != "appimage" {
        return Ok(false);
    }

    let helper = app
        .path()
        .resolve(
            "linux-updater/zsync2/AppRun",
            tauri::path::BaseDirectory::Resource,
        )
        .map_err(|error| format!("resolve delta helper: {error}"))?;

    match std::fs::metadata(&helper) {
        Ok(metadata) => {
            Ok(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect delta helper: {error}")),
    }
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub async fn updater_delta_create(
    app: tauri::AppHandle,
    webview: tauri::Webview,
    update_rid: tauri::ResourceId,
) -> Result<serde_json::Value, String> {
    let session =
        crate::linux_updater::create_session(&app, &webview, update_rid)?;

    serde_json::to_value(session)
        .map_err(|error| format!("Encode update session: {error}"))
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub async fn updater_delta_download(
    app: tauri::AppHandle,
    webview: tauri::Webview,
    session_rid: tauri::ResourceId,
    on_event: tauri::ipc::Channel<crate::linux_updater::DeltaPhase>,
) -> Result<(), crate::linux_updater::DeltaError> {
    crate::linux_updater::download_session(app, webview, session_rid, on_event).await
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub async fn updater_delta_install(
    webview: tauri::Webview,
    session_rid: tauri::ResourceId,
) -> Result<(), crate::linux_updater::DeltaError> {
    crate::linux_updater::install_session(webview, session_rid).await
}
