import { el } from "../dom";

export interface ModalOptions {
  title: string;
  subtitle?: string;
  body: HTMLElement;
  actions: HTMLElement[];
  /** When provided, Escape invokes it instead of doing nothing. */
  onEscape?: () => void;
}

export interface ModalHandle {
  close: () => void;
}

let sequence = 0;
let openModals = 0;

/**
 * Blocking modal dialog.
 *
 * The app root is marked `inert` while a modal is open, so the surrounding UI is
 * unreachable for both pointer and keyboard until it closes.
 */
export function openModal(options: ModalOptions): ModalHandle {
  sequence += 1;
  const titleId = `modal-title-${sequence}`;

  const head = el("div", {
    class: "modal-head",
    children: [
      el("h2", { class: "modal-title", id: titleId, text: options.title }),
      options.subtitle !== undefined ? el("p", { class: "modal-sub", text: options.subtitle }) : null,
    ],
  });

  const dialog = el("div", {
    class: "modal",
    attrs: { role: "dialog", "aria-modal": "true", "aria-labelledby": titleId, tabindex: "-1" },
    children: [head, options.body, el("div", { class: "modal-foot", children: options.actions })],
  });

  const backdrop = el("div", { class: "modal-backdrop", children: [dialog] });
  const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  document.body.append(backdrop);

  const appRoot = document.getElementById("app");
  openModals += 1;
  appRoot?.setAttribute("inert", "");

  const onKeyDown = (event: KeyboardEvent): void => {
    if (event.key === "Escape" && options.onEscape !== undefined) {
      event.preventDefault();
      options.onEscape();
    }
  };
  document.addEventListener("keydown", onKeyDown, true);

  let closed = false;
  const close = (): void => {
    if (closed) {
      return;
    }
    closed = true;
    document.removeEventListener("keydown", onKeyDown, true);
    backdrop.remove();
    openModals -= 1;
    if (openModals === 0) {
      appRoot?.removeAttribute("inert");
    }
    previousFocus?.focus();
  };

  const focusTarget = dialog.querySelector<HTMLElement>("input, button, select, textarea");
  (focusTarget ?? dialog).focus();

  return { close };
}
