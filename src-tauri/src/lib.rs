pub mod commands;
pub mod files;
pub mod pairing;
pub mod settings;
pub mod state;

use std::sync::Arc;

use tauri::Manager;
use tauri::RunEvent;

use crate::state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CRABSEND_LOG").unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("crabsend_core=info,crabsend_app=info")
            }),
        )
        .init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init());

    // Reading a pairing code needs a camera, which only exists on mobile; the
    // desktop build shows the code and never scans one.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        // A second launch brings the running window forward instead of
        // failing to bind the transfer port.
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_focus();
        }
    }));

    let app = builder
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::update_settings,
            commands::scan,
            commands::add_device_by_ip,
            commands::clear_devices,
            commands::get_pairing_qr,
            commands::pair_from_qr,
            commands::unpair_device,
            commands::inspect_files,
            commands::send_files,
            commands::respond_to_upload,
            commands::cancel_session,
            commands::retry_file,
            commands::dismiss_transfer,
            commands::clear_history,
        ])
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let state = Arc::new(AppState::new(app.handle().clone(), config_dir)?);
            app.manage(state.clone());
            // Serving starts in the background so the window appears at once.
            tauri::async_runtime::spawn(async move { state.restart().await });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Crabsend window");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event
            && let Some(state) = handle.try_state::<Arc<AppState>>()
        {
            // Release the port and stop announcing before the process ends.
            let state = state.inner().clone();
            tauri::async_runtime::block_on(async move { state.stop().await });
        }
    });
}
