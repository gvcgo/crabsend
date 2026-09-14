import type { DeviceType } from "./types";

/**
 * Inline SVG icons (24x24, stroke follows `currentColor`).
 * Markup is static, so it is assigned verbatim.
 */
export type IconName =
  | DeviceType
  | "file"
  | "folder"
  | "plus"
  | "refresh"
  | "trash"
  | "alert"
  | "lock"
  | "cancel"
  | "send"
  | "receive"
  | "qr"
  | "camera"
  | "openFolder";

const ICONS: Record<IconName, string> = {
  mobile:
    '<rect x="7" y="2.5" width="10" height="19" rx="2.2"/><line x1="10.5" y1="18.2" x2="13.5" y2="18.2"/>',
  desktop:
    '<rect x="2.5" y="3.5" width="19" height="13" rx="2"/><line x1="8.5" y1="20.5" x2="15.5" y2="20.5"/><line x1="12" y1="16.5" x2="12" y2="20.5"/>',
  web:
    '<circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><line x1="3" y1="12" x2="21" y2="12"/>',
  headless:
    '<rect x="2.5" y="4" width="19" height="16" rx="2"/><path d="m7 9.5 2.5 2.5L7 14.5"/><line x1="12.5" y1="14.5" x2="17" y2="14.5"/>',
  server:
    '<rect x="3" y="3.5" width="18" height="7" rx="2"/><rect x="3" y="13.5" width="18" height="7" rx="2"/><line x1="7" y1="7" x2="7.01" y2="7"/><line x1="7" y1="17" x2="7.01" y2="17"/>',
  file:
    '<path d="M14 3H7.5A2.5 2.5 0 0 0 5 5.5v13A2.5 2.5 0 0 0 7.5 21h9a2.5 2.5 0 0 0 2.5-2.5V8Z"/><path d="M14 3v5h5"/>',
  folder:
    '<path d="M3 7.5A2 2 0 0 1 5 5.5h3.6a2 2 0 0 1 1.7.9l.9 1.4H19a2 2 0 0 1 2 2v6.7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"/>',
  plus: '<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>',
  refresh: '<path d="M20.5 12a8.5 8.5 0 1 1-2.6-6.1"/><path d="M21 4.2v4.6h-4.6"/>',
  trash:
    '<line x1="4" y1="7" x2="20" y2="7"/><path d="M9.5 7V5.6A1.6 1.6 0 0 1 11.1 4h1.8a1.6 1.6 0 0 1 1.6 1.6V7"/><path d="M6.5 7 7.4 19.4A1.6 1.6 0 0 0 9 21h6a1.6 1.6 0 0 0 1.6-1.6L17.5 7"/>',
  alert: '<path d="M12 3.6 21 19.6H3Z"/><line x1="12" y1="9.6" x2="12" y2="14"/><line x1="12" y1="16.8" x2="12.01" y2="16.8"/>',
  lock:
    '<rect x="4.5" y="10.5" width="15" height="10" rx="2"/><path d="M8 10.5V8a4 4 0 0 1 8 0v2.5"/>',
  cancel: '<line x1="6.5" y1="6.5" x2="17.5" y2="17.5"/><line x1="17.5" y1="6.5" x2="6.5" y2="17.5"/>',
  send: '<line x1="12" y1="20" x2="12" y2="4.5"/><path d="m5.5 11 6.5-6.5 6.5 6.5"/>',
  receive: '<line x1="12" y1="4" x2="12" y2="19.5"/><path d="m5.5 13 6.5 6.5 6.5-6.5"/>',
  qr: '<rect x="3.5" y="3.5" width="7" height="7" rx="1.4"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.4"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.4"/><path d="M13.5 13.5h3.2v3.2h-3.2z"/><line x1="20.5" y1="13.5" x2="20.5" y2="17"/><line x1="13.5" y1="20.5" x2="17" y2="20.5"/>',
  camera:
    '<path d="M3.5 8.4h3.1l1.6-2.2h7.6l1.6 2.2h3.1v10.1h-17z"/><circle cx="12" cy="13.2" r="3.3"/>',
  openFolder:
    '<path d="M14 4h6v6"/><path d="m20 4-9.5 9.5"/><path d="M18 14.5V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4.5"/>',
};

const SVG_NS = "http://www.w3.org/2000/svg";

export function icon(name: IconName, size = 18): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.7");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  svg.innerHTML = ICONS[name];
  return svg;
}
