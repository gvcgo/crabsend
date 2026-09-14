import { cancelSession, describeError, dismissTransfer, sendFiles } from "../api";
import { button, el, field } from "../dom";
import { formatBytes } from "../format";
import { store } from "../store";
import type { Session } from "../types";
import { openModal } from "./modal";
import { showToast } from "./toast";

/**
 * A send session that turns `pinRequired` gets one prompt: the receiver wants a PIN,
 * so the transfer is resubmitted through `send_files` with the PIN attached. That
 * creates a fresh session and the stale row is dropped.
 */
export function initPinPrompts(): void {
  let shown: { sessionId: string; close: () => void } | null = null;

  function sync(): void {
    const sessions = store.snapshot?.sessions ?? [];

    if (shown !== null) {
      const current = sessions.find((session) => session.id === shown?.sessionId);
      if (current === undefined || current.status !== "pinRequired") {
        shown.close();
        shown = null;
      }
    }

    const waiting = sessions.find(
      (session) =>
        session.direction === "send" &&
        session.status === "pinRequired" &&
        !store.pinPrompted(session.id),
    );
    if (waiting === undefined) {
      return;
    }
    store.markPinPrompted(waiting.id);
    const sessionId = waiting.id;
    shown = showPrompt(waiting, () => {
      if (shown?.sessionId === sessionId) {
        shown = null;
      }
    });
  }

  store.subscribe(sync);
  sync();
}

function showPrompt(session: Session, onClosed: () => void): { sessionId: string; close: () => void } {
  let busy = false;

  const pinInput = el("input", {
    class: "input",
    attrs: { type: "password", inputmode: "numeric", autocomplete: "off", spellcheck: "false" },
  });
  const sendButton = button("Send", { class: "btn btn-primary" });
  const cancelButton = button("Cancel transfer", { class: "btn btn-ghost" });

  const totalSize = session.files.reduce((sum, file) => sum + file.size, 0);
  const body = el("div", {
    class: "modal-body",
    children: [
      field("Receiver PIN", pinInput, {
        hint: `${session.peer.alias} requires a PIN before accepting these files.`,
      }),
      el("p", {
        class: "modal-meta",
        text: `${session.files.length} ${session.files.length === 1 ? "item" : "items"} · ${formatBytes(totalSize)}`,
      }),
    ],
  });

  const handle = openModal({
    title: "PIN required",
    subtitle: `Sending to ${session.peer.alias}`,
    body,
    actions: [cancelButton, sendButton],
    onEscape: () => {
      void cancelTransfer();
    },
  });

  function setBusy(next: boolean): void {
    busy = next;
    sendButton.disabled = busy;
    cancelButton.disabled = busy;
    pinInput.disabled = busy;
  }

  function finish(): void {
    handle.close();
    onClosed();
  }

  async function submitPin(): Promise<void> {
    if (busy) {
      return;
    }
    const pin = pinInput.value.trim();
    if (pin.length === 0) {
      pinInput.focus();
      return;
    }
    const pending = store.pendingSend(session.id);
    if (pending === null) {
      showToast("The files for this transfer are no longer available; start it again.", "error");
      finish();
      return;
    }
    setBusy(true);
    try {
      const newSessionId = await sendFiles(pending.target, pending.files, pin);
      store.rememberSend(newSessionId, pending);
      store.forgetSend(session.id);
      await dismissTransfer(session.id);
      finish();
    } catch (error) {
      showToast(describeError(error), "error");
      setBusy(false);
    }
  }

  async function cancelTransfer(): Promise<void> {
    if (busy) {
      return;
    }
    setBusy(true);
    try {
      await cancelSession(session.id);
    } catch (error) {
      showToast(describeError(error), "error");
    }
    store.forgetSend(session.id);
    finish();
  }

  sendButton.addEventListener("click", () => {
    void submitPin();
  });
  cancelButton.addEventListener("click", () => {
    void cancelTransfer();
  });
  pinInput.addEventListener("keydown", (event: KeyboardEvent) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void submitPin();
    }
  });
  setBusy(false);

  return { sessionId: session.id, close: handle.close };
}
