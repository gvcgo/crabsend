//! The commands the frontend calls.

use std::sync::Arc;

use tauri::State;

use crate::files;
use crate::pairing::PairingQr;
use crate::settings::Settings;
use crate::state::AppState;
use crate::state::DeviceOut;
use crate::state::SendFile;
use crate::state::Snapshot;

/// The complete state the UI renders.
#[tauri::command]
pub fn get_snapshot(state: State<'_, Arc<AppState>>) -> Snapshot {
    state.snapshot()
}

/// Stores new settings, restarting the transfer server when they affect it.
#[tauri::command]
pub fn update_settings(state: State<'_, Arc<AppState>>, settings: Settings) -> Snapshot {
    state.update_settings(settings)
}

/// Announces this device and scans the local subnets.
#[tauri::command]
pub async fn scan(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.scan().await;
    Ok(())
}

/// Probes one host the user typed in, with or without a port.
#[tauri::command]
pub async fn add_device_by_ip(state: State<'_, Arc<AppState>>, ip: String) -> Result<(), String> {
    state.add_device(&ip).await.map_err(report)
}

/// The code another device scans to pair with this one.
#[tauri::command]
pub async fn get_pairing_qr(state: State<'_, Arc<AppState>>) -> Result<PairingQr, String> {
    state.pairing_qr().await.map_err(report)
}

/// Pairs with the device whose code was scanned.
#[tauri::command]
pub async fn pair_from_qr(
    state: State<'_, Arc<AppState>>,
    payload: String,
) -> Result<DeviceOut, String> {
    state.pair_from_qr(&payload).await.map_err(report)
}

/// Forgets a device this one was paired with.
#[tauri::command]
pub fn unpair_device(state: State<'_, Arc<AppState>>, fingerprint: String) -> Result<(), String> {
    state.unpair(&fingerprint).map_err(report)
}

/// Forgets every discovered peer.
#[tauri::command]
pub fn clear_devices(state: State<'_, Arc<AppState>>) {
    state.clear_devices();
}

/// Describes picked files and folders, without hashing them.
#[tauri::command]
pub async fn inspect_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
) -> Result<Vec<SendFile>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Android answers with `content://` URIs, which are not paths yet.
        let paths = files::ingest(&app, &paths)?;
        files::inspect(&paths)
    })
    .await
    .map_err(|error| format!("inspecting the selection failed: {error}"))?
    .map_err(report)
}

/// Starts sending files to a peer and returns the new session id.
#[tauri::command]
pub fn send_files(
    state: State<'_, Arc<AppState>>,
    target: String,
    files: Vec<SendFile>,
    pin: Option<String>,
) -> Result<String, String> {
    state.start_send(&target, files, pin).map_err(report)
}

/// Accepts or declines the pending incoming transfer.
#[tauri::command]
pub async fn respond_to_upload(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    accept: bool,
    file_ids: Option<Vec<String>>,
) -> Result<(), String> {
    state
        .respond_to_upload(&session_id, accept, file_ids)
        .await
        .map_err(report)
}

/// Cancels a transfer in either direction.
#[tauri::command]
pub async fn cancel_session(
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<(), String> {
    state.cancel_session(&session_id).await;
    Ok(())
}

/// Sends one file of a session again, as a session of its own.
#[tauri::command]
pub fn retry_file(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    file_id: String,
) -> Result<String, String> {
    state.retry_file(&session_id, &file_id).map_err(report)
}

/// Removes a finished session from the list.
#[tauri::command]
pub fn dismiss_transfer(state: State<'_, Arc<AppState>>, session_id: String) -> Result<(), String> {
    state.dismiss_transfer(&session_id).map_err(report)
}

/// Empties the transfer history.
#[tauri::command]
pub fn clear_history(state: State<'_, Arc<AppState>>) {
    state.clear_history();
}

/// Opens a received file with the application the platform has for its type.
///
/// A phone has no file manager to show a file in, so this is the action it
/// offers where a desktop shows the file in a folder.
#[tauri::command]
pub fn open_received_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    crate::platform::open(&app, std::path::Path::new(&path)).map_err(report)
}

/// Renders an error with its whole cause chain, which is what the user needs
/// to see when a transfer cannot start.
fn report(error: anyhow::Error) -> String {
    format!("{error:#}")
}
