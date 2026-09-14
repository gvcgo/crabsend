import type { DeviceType, FileStatus, SessionStatus } from "./types";

/** Visual tone of a status chip. */
export type Tone = "neutral" | "info" | "ok" | "warn" | "danger";

export interface StatusLabel {
  label: string;
  tone: Tone;
}

export const SESSION_LABELS: Record<SessionStatus, StatusLabel> = {
  preparing: { label: "Preparing", tone: "info" },
  waiting: { label: "Waiting for peer", tone: "info" },
  active: { label: "Transferring", tone: "info" },
  pinRequired: { label: "PIN required", tone: "warn" },
  done: { label: "Completed", tone: "ok" },
  failed: { label: "Failed", tone: "danger" },
  cancelled: { label: "Cancelled", tone: "neutral" },
  declined: { label: "Declined", tone: "warn" },
  busy: { label: "Peer busy", tone: "warn" },
};

export const FILE_LABELS: Record<FileStatus, string> = {
  pending: "Waiting",
  hashing: "Hashing",
  active: "Transferring",
  done: "Done",
  failed: "Failed",
  skipped: "Skipped",
};

export const DEVICE_LABELS: Record<DeviceType, string> = {
  mobile: "Mobile",
  desktop: "Desktop",
  web: "Web",
  headless: "Headless",
  server: "Server",
};

export const DEVICE_TYPES: DeviceType[] = ["mobile", "desktop", "web", "headless", "server"];

export function isDeviceType(value: string): value is DeviceType {
  return DEVICE_TYPES.some((type) => type === value);
}

/** A session still being negotiated or transferring: cancellable, shown in the tab badge. */
export function isPending(status: SessionStatus): boolean {
  return status === "preparing" || status === "waiting" || status === "active" || status === "pinRequired";
}

/** A session that can only be dismissed from the list. */
export function isFinished(status: SessionStatus): boolean {
  return !isPending(status);
}
