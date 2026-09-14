import { el } from "../dom";

let host: HTMLElement | null = null;

function toastHost(): HTMLElement {
  if (host === null) {
    host = el("div", { class: "toast-host", attrs: { "aria-live": "polite" } });
    document.body.append(host);
  }
  return host;
}

/** Transient message, used for `invoke` rejections and short confirmations. */
export function showToast(message: string, tone: "info" | "error" = "info"): void {
  const container = toastHost();
  const node = el("div", {
    class: `toast toast-${tone}`,
    text: message,
    attrs: { role: tone === "error" ? "alert" : "status" },
  });
  container.append(node);
  while (container.childElementCount > 4) {
    container.firstElementChild?.remove();
  }
  window.setTimeout(
    () => {
      node.remove();
    },
    tone === "error" ? 9000 : 4000,
  );
}
