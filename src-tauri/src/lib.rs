//! Crabsend's entry point.

pub mod commands;
pub mod files;
pub mod pairing;
pub mod platform;
pub mod settings;
pub mod state;

use std::sync::Arc;

use tauri::Manager;
use tauri::RunEvent;

use crate::state::AppState;

/// The panel icon, which is also how the application is asked to leave.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const TRAY_ID: &str = "crabsend";

/// Whether that icon was created. Closing the window hides the application
/// only when there is a tray to reach it again through.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
static TRAY_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            commands::open_received_file,
        ])
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let state = Arc::new(AppState::new(app.handle().clone(), config_dir)?);
            app.manage(state.clone());
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            create_tray(app)?;
            // Serving starts in the background so the window appears at once.
            tauri::async_runtime::spawn(async move { state.restart().await });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window leaves the application in the tray rather than
            // taking the transfer server down with it, which is how a peer can
            // still send this device something. Quit in the tray ends it, and
            // without a tray the window closes as it always did — there would
            // be no way back to a hidden one.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && TRAY_READY.load(std::sync::atomic::Ordering::Relaxed)
            {
                api.prevent_close();
                let _ = window.hide();
                name_tray_entry(window.app_handle(), false);
            }

            #[cfg(any(target_os = "android", target_os = "ios"))]
            let _ = (window, event);
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

/// Puts the application in the desktop's panel.
///
/// The window can be out of the way while the transfer server keeps running,
/// which is what makes this device still reachable by a peer that wants to send
/// it something. A phone has no panel, and its application is the window.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn create_tray(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::Menu;
    use tauri::menu::MenuItem;
    use tauri::tray::TrayIconBuilder;

    let toggle = MenuItem::with_id(app, "toggle", "Hide window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &quit])?;

    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Crabsend")
        // A panel on Linux shows the menu on a click; there is no other gesture
        // for it to offer.
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => toggle_window(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    let tray = tray.build(app)?;
    app.manage(TrayToggle {
        item: toggle,
        icon: tray,
    });
    TRAY_READY.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// What the tray menu needs to keep its one moving part honest: the entry that
/// says what clicking it will do.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct TrayToggle {
    item: tauri::menu::MenuItem<tauri::Wry>,
    /// Held so the icon is not dropped with it.
    #[allow(dead_code)]
    icon: tauri::tray::TrayIcon<tauri::Wry>,
}

/// Shows the window, or puts it back out of the way.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn toggle_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    if visible {
        let _ = window.hide();
    } else {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    name_tray_entry(app, !visible);
}

/// Names what a click on the tray's one moving part will do next. The state is
/// passed in rather than read back: hiding a window has not taken effect by the
/// time the call that asked for it returns.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn name_tray_entry(app: &tauri::AppHandle, visible: bool) {
    if let Some(toggle) = app.try_state::<TrayToggle>() {
        let _ = toggle.item.set_text(if visible {
            "Hide window"
        } else {
            "Show window"
        });
    }
}
