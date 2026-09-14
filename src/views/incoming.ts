import { describeError, respondToUpload } from "../api";
import { button, el } from "../dom";
import { formatBytes, isDirectoryMime } from "../format";
import { icon } from "../icons";
import { DEVICE_LABELS } from "../status";
import { store } from "../store";
import type { IncomingRequest } from "../types";
import { openModal } from "./modal";
import { showToast } from "./toast";

/** Shows a blocking accept/decline dialog whenever the backend reports a request. */
export function initIncomingRequests(): void {
  let shown: { sessionId: string; close: () => void } | null = null;

  function sync(): void {
    const request = store.snapshot?.incoming ?? null;
    if (request === null) {
      shown?.close();
      shown = null;
      return;
    }
    if (shown !== null && shown.sessionId === request.sessionId) {
      return;
    }
    shown?.close();
    shown = showRequest(request);
  }

  store.subscribe(sync);
  sync();
}

function showRequest(request: IncomingRequest): { sessionId: string; close: () => void } {
  const checked = new Set(request.files.map((file) => file.id));
  const boxes: HTMLInputElement[] = [];
  let busy = false;

  const acceptButton = button("Accept", { class: "btn btn-primary" });
  const declineButton = button("Decline", { class: "btn btn-ghost" });

  function syncButtons(): void {
    acceptButton.disabled = busy || checked.size === 0;
    declineButton.disabled = busy;
    for (const box of boxes) {
      box.disabled = busy;
    }
  }

  const rows = request.files.map((file) => {
    const box = el("input", { attrs: { type: "checkbox" } });
    box.checked = true;
    boxes.push(box);
    box.addEventListener("change", () => {
      if (box.checked) {
        checked.add(file.id);
      } else {
        checked.delete(file.id);
      }
      syncButtons();
    });
    return el("label", {
      class: "pick-row",
      children: [
        box,
        el("span", {
          class: "pick-icon",
          children: [icon(isDirectoryMime(file.mime) ? "folder" : "file", 16)],
        }),
        el("span", { class: "pick-name", text: file.name, title: file.name }),
        el("span", { class: "pick-size", text: formatBytes(file.size) }),
      ],
    });
  });

  const count = request.files.length;
  const body = el("div", {
    class: "modal-body",
    children: [
      el("p", {
        class: "modal-meta",
        text: `${count} ${count === 1 ? "item" : "items"} · ${formatBytes(request.totalSize)}`,
      }),
      request.pinProtected
        ? el("p", {
            class: "modal-note",
            children: [icon("lock", 14), el("span", { text: "PIN protection is enabled on this device." })],
          })
        : null,
      el("div", { class: "pick-list", children: rows }),
    ],
  });

  const handle = openModal({
    title: "Incoming transfer",
    subtitle: `${request.peer.alias} · ${DEVICE_LABELS[request.peer.deviceType]}`,
    body,
    actions: [declineButton, acceptButton],
  });

  async function respond(accept: boolean): Promise<void> {
    if (busy) {
      return;
    }
    busy = true;
    syncButtons();
    const allIds = request.files.map((file) => file.id);
    const subset = allIds.filter((id) => checked.has(id));
    const fileIds = accept && subset.length !== allIds.length ? subset : null;
    try {
      await respondToUpload(request.sessionId, accept, fileIds);
      handle.close();
    } catch (error) {
      showToast(describeError(error), "error");
      busy = false;
      syncButtons();
    }
  }

  acceptButton.addEventListener("click", () => {
    void respond(true);
  });
  declineButton.addEventListener("click", () => {
    void respond(false);
  });
  syncButtons();

  return { sessionId: request.sessionId, close: handle.close };
}
