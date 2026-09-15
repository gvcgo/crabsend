import {
  cancelSession,
  describeError,
  dismissTransfer,
  openReceivedFile,
  retryFile,
  revealPath,
} from "../api";
import { button, el, iconButton } from "../dom";
import { formatBytes, formatPercent, formatRelativeTime, isDirectoryMime, percentOf } from "../format";
import { icon } from "../icons";
import { FILE_LABELS, SESSION_LABELS, isPending } from "../status";
import { store } from "../store";
import type { ProgressPayload, Session, Snapshot, TransferFile } from "../types";
import { showToast } from "./toast";

interface Meter {
  bar: HTMLElement;
  fill: HTMLElement;
  label: HTMLElement;
}

function progressText(transferred: number, total: number): string {
  if (total <= 0) {
    return formatBytes(transferred);
  }
  return `${formatPercent(percentOf(transferred, total))} · ${formatBytes(transferred)} / ${formatBytes(total)}`;
}

function applyMeter(meter: Meter, transferred: number, total: number): void {
  const unknownTotal = total <= 0;
  const percent = percentOf(transferred, total);
  // An unknown total has no percentage: the bar slides instead of filling.
  meter.fill.style.width = unknownTotal ? "35%" : `${percent}%`;
  meter.bar.setAttribute("aria-valuenow", String(Math.floor(percent)));
  meter.bar.classList.toggle("is-indeterminate", unknownTotal);
  meter.label.textContent = progressText(transferred, total);
}

function buildMeter(className: string): { root: HTMLElement; meter: Meter } {
  const fill = el("div", { class: "bar-fill" });
  const bar = el("div", {
    class: "bar",
    attrs: { role: "progressbar", "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": "0" },
    children: [fill],
  });
  const label = el("div", { class: "meter-label" });
  return { root: el("div", { class: className, children: [bar, label] }), meter: { bar, fill, label } };
}

function sessionTotals(session: Session): { transferred: number; total: number } {
  let transferred = 0;
  let total = 0;
  for (const file of session.files) {
    transferred += file.transferred;
    total += file.size;
  }
  return { transferred, total };
}

/** A finished receive records its destination only in `history[].savedDir`. */
function savedDirFor(state: Snapshot, session: Session): string | null {
  const exact = state.history.find((entry) => entry.id === session.id);
  if (exact !== undefined && exact.savedDir !== null) {
    return exact.savedDir;
  }
  const match = state.history.find(
    (entry) =>
      entry.direction === "receive" &&
      entry.peerFingerprint === session.peer.fingerprint &&
      entry.at >= session.startedAt &&
      entry.savedDir !== null,
  );
  return match?.savedDir ?? null;
}

/** Active and finished sessions, with per-file progress driven by `progress` events. */
export function createTransfersView(): HTMLElement {
  const sessionMeters = new Map<string, Meter>();
  const fileMeters = new Map<string, Meter>();
  let signature = "";

  const countLabel = el("span", { class: "panel-count" });
  const list = el("div", { class: "session-list" });
  const empty = el("p", { class: "empty", text: "No transfers yet." });

  const root = el("section", {
    class: "card",
    children: [
      el("div", {
        class: "card-head",
        children: [el("h2", { class: "card-title", text: "Transfers" }), countLabel],
      }),
      list,
      empty,
    ],
  });

  async function runCancel(sessionId: string): Promise<void> {
    try {
      await cancelSession(sessionId);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function runDismiss(sessionId: string): Promise<void> {
    try {
      await dismissTransfer(sessionId);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function runRetry(sessionId: string, fileId: string): Promise<void> {
    try {
      await retryFile(sessionId, fileId);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function runReveal(path: string): Promise<void> {
    try {
      await revealPath(path);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function runOpen(path: string): Promise<void> {
    try {
      await openReceivedFile(path);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  function fileRow(state: Snapshot, session: Session, file: TransferFile): HTMLLIElement {
    const built = buildMeter("file-meter");
    fileMeters.set(`${session.id}::${file.id}`, built.meter);
    applyMeter(built.meter, file.transferred, file.size);

    const row = el("li", {
      class: "file-row",
      children: [
        el("span", {
          class: "file-icon",
          children: [icon(isDirectoryMime(file.mime) ? "folder" : "file", 16)],
        }),
        el("div", {
          class: "file-main",
          children: [
            el("div", {
              class: "file-top",
              children: [
                el("span", { class: "file-name", text: file.name, title: file.name }),
                el("span", { class: "file-status", text: FILE_LABELS[file.status] }),
              ],
            }),
            built.root,
            file.error !== null ? el("p", { class: "file-error", text: file.error }) : null,
          ],
        }),
      ],
    });

    if (file.status === "failed") {
      row.append(
        button("Retry", {
          class: "btn btn-ghost btn-sm",
          icon: "refresh",
          onClick: () => {
            void runRetry(session.id, file.id);
          },
        }),
      );
    }

    // A phone has no folder to show a received file in, so it opens the file
    // itself instead.
    if (
      file.status === "done" &&
      session.direction === "receive" &&
      state.canOpenFiles &&
      file.savedPath !== null
    ) {
      const path = file.savedPath;
      row.append(
        button("Open", {
          class: "btn btn-ghost btn-sm",
          icon: "openFolder",
          onClick: () => {
            void runOpen(path);
          },
        }),
      );
    }
    return row;
  }

  function sessionRow(state: Snapshot, session: Session, now: number): HTMLElement {
    const status = SESSION_LABELS[session.status];
    const actions = el("div", { class: "session-actions" });

    if (session.direction === "receive" && session.status === "done" && state.canRevealFiles) {
      const directory = savedDirFor(state, session) ?? state.settings.downloadDir;
      actions.append(
        button("Show in folder", {
          class: "btn btn-ghost btn-sm",
          icon: "openFolder",
          onClick: () => {
            void runReveal(directory);
          },
        }),
      );
    }

    if (isPending(session.status)) {
      actions.append(
        button("Cancel", {
          class: "btn btn-ghost btn-sm",
          icon: "cancel",
          onClick: () => {
            void runCancel(session.id);
          },
        }),
      );
    } else {
      actions.append(
        iconButton("cancel", "Dismiss", {
          class: "btn btn-icon btn-sm",
          onClick: () => {
            void runDismiss(session.id);
          },
        }),
      );
    }

    const totals = sessionTotals(session);
    const overall = buildMeter("meter-session");
    sessionMeters.set(session.id, overall.meter);
    applyMeter(overall.meter, totals.transferred, totals.total);

    return el("article", {
      class: "session",
      children: [
        el("header", {
          class: "session-head",
          children: [
            el("span", {
              class: "dir-icon",
              children: [icon(session.direction === "send" ? "send" : "receive", 16)],
            }),
            el("div", {
              class: "session-title",
              children: [
                el("span", { class: "session-peer", text: session.peer.alias }),
                el("span", {
                  class: `chip chip-${status.tone}`,
                  text: status.label,
                  attrs: { "aria-live": "polite" },
                }),
              ],
            }),
            actions,
          ],
        }),
        el("div", {
          class: "session-meta",
          text: `${session.files.length} ${session.files.length === 1 ? "item" : "items"} · ${formatBytes(
            totals.total,
          )} · started ${formatRelativeTime(session.startedAt, now)}`,
        }),
        overall.root,
        el("ul", { class: "file-list", children: session.files.map((file) => fileRow(state, session, file)) }),
        session.error !== null ? el("p", { class: "session-error", text: session.error }) : null,
      ],
    });
  }

  function sessionSignature(state: Snapshot): string {
    const sessions = state.sessions
      .map((session) =>
        [
          session.id,
          session.status,
          session.error ?? "",
          session.startedAt,
          session.files
            .map(
              (file) =>
                `${file.id}:${file.status}:${file.transferred}:${file.error ?? ""}:${file.savedPath ?? ""}`,
            )
            .join(","),
        ].join(":"),
      )
      .join("|");
    return `${sessions}||${state.settings.downloadDir}||${state.history.length}`;
  }

  function render(): void {
    const state = store.snapshot;
    const sessions = state?.sessions ?? [];
    const next = state === null ? "" : sessionSignature(state);
    if (next === signature) {
      return;
    }
    signature = next;
    sessionMeters.clear();
    fileMeters.clear();
    const now = Date.now();
    list.replaceChildren(...(state === null ? [] : sessions.map((session) => sessionRow(state, session, now))));
    empty.hidden = sessions.length > 0;
    const pending = sessions.filter((session) => isPending(session.status)).length;
    countLabel.textContent = pending === 0 ? "" : `${pending} in progress`;
  }

  function handleProgress(payload: ProgressPayload): void {
    const session = store.snapshot?.sessions.find((item) => item.id === payload.sessionId);
    if (session === undefined) {
      return;
    }
    const file = session.files.find((item) => item.id === payload.fileId);
    const fileMeter = fileMeters.get(`${payload.sessionId}::${payload.fileId}`);
    if (file !== undefined && fileMeter !== undefined) {
      applyMeter(fileMeter, file.transferred, file.size);
    }
    const overall = sessionMeters.get(session.id);
    if (overall !== undefined) {
      const totals = sessionTotals(session);
      applyMeter(overall, totals.transferred, totals.total);
    }
  }

  store.subscribe(render);
  store.subscribeProgress(handleProgress);
  render();

  return root;
}
