import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

import type { Device, PairingQr, ProgressPayload, SendFile, Settings, Snapshot } from "./types";

/** `get_snapshot` — full state, called once at startup. */
export function getSnapshot(): Promise<Snapshot> {
  return invoke<Snapshot>("get_snapshot");
}

/** `update_settings` — persists and restarts the server when needed. */
export function updateSettings(settings: Settings): Promise<Snapshot> {
  return invoke<Snapshot>("update_settings", { settings });
}

/** `scan` — re-announce and scan subnets; peers arrive through `state` events. */
export function scan(): Promise<void> {
  return invoke<void>("scan");
}

/** `add_device_by_ip` — manual peer by IP, host name, or `host:port`. */
export function addDeviceByIp(ip: string): Promise<void> {
  return invoke<void>("add_device_by_ip", { ip });
}

/** `get_pairing_qr` — this device's code, for another device to scan. */
export function getPairingQr(): Promise<PairingQr> {
  return invoke<PairingQr>("get_pairing_qr");
}

/** `pair_from_qr` — pairs with the device a scanned code names. */
export function pairFromQr(payload: string): Promise<Device> {
  return invoke<Device>("pair_from_qr", { payload });
}

/** `unpair_device` — forgets a device this one was paired with. */
export function unpairDevice(fingerprint: string): Promise<void> {
  return invoke<void>("unpair_device", { fingerprint });
}

export function clearDevices(): Promise<void> {
  return invoke<void>("clear_devices");
}

/** `inspect_files` — stat files/folders without hashing. */
export function inspectFiles(paths: string[]): Promise<SendFile[]> {
  return invoke<SendFile[]>("inspect_files", { paths });
}

/**
 * `send_files` — `target` is the peer fingerprint.
 * Resolves with the new session id immediately; remote refusals surface as session state.
 */
export function sendFiles(target: string, files: SendFile[], pin: string | null): Promise<string> {
  return invoke<string>("send_files", { target, files, pin });
}

/** `respond_to_upload` — `fileIds: null` accepts every offered file. */
export function respondToUpload(
  sessionId: string,
  accept: boolean,
  fileIds: string[] | null,
): Promise<void> {
  return invoke<void>("respond_to_upload", { sessionId, accept, fileIds });
}

export function cancelSession(sessionId: string): Promise<void> {
  return invoke<void>("cancel_session", { sessionId });
}

export function retryFile(sessionId: string, fileId: string): Promise<void> {
  return invoke<void>("retry_file", { sessionId, fileId });
}

/** Drops a finished session row. */
export function dismissTransfer(sessionId: string): Promise<void> {
  return invoke<void>("dismiss_transfer", { sessionId });
}

export function clearHistory(): Promise<void> {
  return invoke<void>("clear_history");
}

/** `state` — emitted on every structural change. */
export function onState(listener: (snapshot: Snapshot) => void): Promise<UnlistenFn> {
  return listen<Snapshot>("state", (event) => listener(event.payload));
}

/** `progress` — bytes moved, throttled to roughly 20 ms server-side. */
export function onProgress(listener: (payload: ProgressPayload) => void): Promise<UnlistenFn> {
  return listen<ProgressPayload>("progress", (event) => listener(event.payload));
}

/** OS drag & drop for the current webview. */
export function onDragDrop(listener: (event: DragDropEvent) => void): Promise<UnlistenFn> {
  return getCurrentWebview().onDragDropEvent((event) => listener(event.payload));
}

/** Multi-file picker; returns `[]` when cancelled. */
export async function pickFiles(title: string): Promise<string[]> {
  const selected = await open({ multiple: true, title });
  if (selected === null) {
    return [];
  }
  return Array.isArray(selected) ? selected : [selected];
}

/** Directory picker; returns `null` when cancelled. */
export async function pickFolder(title: string, defaultPath: string | null): Promise<string | null> {
  const picked =
    defaultPath === null || defaultPath.length === 0
      ? await open({ directory: true, multiple: false, title })
      : await open({ directory: true, multiple: false, title, defaultPath });
  return Array.isArray(picked) ? (picked[0] ?? null) : picked;
}

export function revealPath(path: string): Promise<void> {
  return revealItemInDir(path);
}

/** `invoke` rejects with plain strings in Tauri v2; normalise anything else. */
export function describeError(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    return error.message;
  }
  if (typeof error === "object" && error !== null && "message" in error) {
    const message: unknown = (error as { message: unknown }).message;
    if (typeof message === "string") {
      return message;
    }
  }
  return "Unexpected error";
}
