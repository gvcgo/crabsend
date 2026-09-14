/** Small presentation helpers shared by the views. */

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

/** 1536 -> "1.50 KB". Non-positive or non-finite input renders as "0 B". */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${UNITS[unit] ?? "B"}`;
}

/** Completion ratio as 0..100. Unknown (non-positive) totals yield 0. */
export function percentOf(transferred: number, total: number): number {
  if (!(total > 0) || !Number.isFinite(transferred)) {
    return 0;
  }
  return Math.min(100, Math.max(0, (transferred / total) * 100));
}

/** 42.7 -> "42%". Rounds down so a bar never reads 100% before it finishes. */
export function formatPercent(percent: number): string {
  return `${Math.floor(percent)}%`;
}

/** "just now", "4 min ago", "3 h ago", "2 d ago", then an absolute date. */
export function formatRelativeTime(at: number, now: number): string {
  const delta = now - at;
  if (!Number.isFinite(delta) || delta < 45_000) {
    return "just now";
  }
  const seconds = Math.floor(delta / 1000);
  if (seconds < 90) {
    return "1 min ago";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours} h ago`;
  }
  const days = Math.round(hours / 24);
  if (days < 7) {
    return `${days} d ago`;
  }
  return new Date(at).toLocaleDateString();
}

/** Full local date + time, used as the `title` of relative timestamps. */
export function formatDateTime(at: number): string {
  return new Date(at).toLocaleString();
}

/** Fingerprints are long digests; show a stable prefix/suffix pair. */
export function shortFingerprint(fingerprint: string): string {
  if (fingerprint.length <= 16) {
    return fingerprint;
  }
  return `${fingerprint.slice(0, 8)}…${fingerprint.slice(-8)}`;
}

/** `inode/directory` (and friends) mean an entry stands for a folder. */
export function isDirectoryMime(mime: string): boolean {
  return mime.toLowerCase().includes("directory");
}
