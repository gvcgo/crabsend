/**
 * Wire types shared with the Rust backend.
 *
 * These mirror the frozen IPC contract (`local://ipc-contract.md`) exactly:
 * `serde` camelCase field names, millisecond timestamps, `null` for absent optionals.
 */

export type DeviceType = "mobile" | "desktop" | "web" | "headless" | "server";

export type Protocol = "http" | "https";

export type Direction = "send" | "receive";

export type SessionStatus =
  | "preparing"
  | "waiting"
  | "active"
  | "pinRequired"
  | "done"
  | "failed"
  | "cancelled"
  | "declined"
  | "busy";

export type FileStatus = "pending" | "hashing" | "active" | "done" | "failed" | "skipped";

export interface Settings {
  alias: string;
  deviceModel: string | null;
  deviceType: DeviceType;
  /** 1..65535 */
  port: number;
  /** true => HTTPS server + mTLS, false => plain HTTP */
  encryption: boolean;
  downloadDir: string;
  /** Optional PIN required to send *to this* device. */
  pin: string | null;
  autoAccept: boolean;
  createChecksums: boolean;
}

export interface DeviceInfo {
  alias: string;
  fingerprint: string;
  deviceModel: string | null;
  deviceType: DeviceType;
  protocol: Protocol;
  port: number;
}

export interface ServerStatus {
  running: boolean;
  protocol: Protocol;
  port: number;
  /** e.g. "address already in use" */
  error: string | null;
}

export interface Device {
  fingerprint: string;
  alias: string;
  deviceModel: string | null;
  deviceType: DeviceType;
  protocol: Protocol;
  host: string;
  port: number;
  download: boolean;
  /** ms since epoch */
  lastSeen: number;
  /** True when a pairing code proved this device's identity; kept between runs. */
  paired: boolean;
}

/** What pairing can do on the platform the backend runs on. */
export interface PairingSupport {
  /** Reading a code needs a camera, which only the mobile build can open. */
  canScan: boolean;
  /** Every platform can show its own code for another device to scan. */
  canShowQr: boolean;
}

/** A pairing code and the string it encodes. */
export interface PairingQr {
  /** The exact string inside the code. */
  payload: string;
  /** The code as a standalone SVG document. */
  svg: string;
}

export interface TransferFile {
  id: string;
  name: string;
  size: number;
  mime: string;
  transferred: number;
  status: FileStatus;
  error: string | null;
  /** Where a received file ended up; null while it is on its way and for sent files. */
  savedPath: string | null;
}

export interface TransferPeer {
  alias: string;
  fingerprint: string;
  deviceModel: string | null;
  deviceType: DeviceType;
}

export interface Session {
  id: string;
  direction: Direction;
  peer: TransferPeer;
  status: SessionStatus;
  files: TransferFile[];
  error: string | null;
  /** ms since epoch */
  startedAt: number;
}

export interface IncomingFile {
  id: string;
  name: string;
  size: number;
  mime: string;
}

export interface IncomingRequest {
  sessionId: string;
  peer: TransferPeer;
  files: IncomingFile[];
  totalSize: number;
  pinProtected: boolean;
}

export interface HistoryEntry {
  id: string;
  direction: Direction;
  peerAlias: string;
  peerFingerprint: string;
  fileCount: number;
  totalSize: number;
  status: SessionStatus;
  /** ms since epoch */
  at: number;
  /** receive: where the files landed */
  savedDir: string | null;
}

export interface SendFile {
  path: string;
  name: string;
  size: number;
  mime: string;
  /** always null from `inspect_files` (filled during send) */
  sha256: string | null;
  /** RFC 3339 */
  modified: string | null;
  accessed: string | null;
}

export interface Snapshot {
  settings: Settings;
  device: DeviceInfo;
  server: ServerStatus;
  devices: Device[];
  /** newest first */
  sessions: Session[];
  incoming: IncomingRequest | null;
  /** newest first, capped */
  history: HistoryEntry[];
  pairing: PairingSupport;
  /** Whether this platform has a file manager to show a received file in. */
  canRevealFiles: boolean;
  /** Whether a received file can be handed to an application that opens it. */
  canOpenFiles: boolean;
  /** Whether this platform can ask the user for a directory. */
  canPickFolder: boolean;
}

/** Payload of the `progress` event. */
export interface ProgressPayload {
  sessionId: string;
  fileId: string;
  transferred: number;
}
