import { clearHistory, describeError, revealPath } from "../api";
import { button, el } from "../dom";
import { formatBytes, formatDateTime, formatRelativeTime } from "../format";
import { icon } from "../icons";
import { SESSION_LABELS } from "../status";
import { store } from "../store";
import type { HistoryEntry } from "../types";
import { showToast } from "./toast";

/** Newest first, with "Open folder" for receives that recorded a destination. */
export function createHistoryView(): HTMLElement {
  let signature = "";

  const list = el("div", { class: "history-list" });
  const empty = el("p", { class: "empty", text: "Nothing transferred yet." });
  const clearButton = button("Clear", {
    class: "btn btn-ghost",
    icon: "trash",
    onClick: () => {
      void runClear();
    },
  });

  const root = el("section", {
    class: "card",
    children: [
      el("div", {
        class: "card-head",
        children: [el("h2", { class: "card-title", text: "History" }), clearButton],
      }),
      list,
      empty,
    ],
  });

  async function runClear(): Promise<void> {
    clearButton.disabled = true;
    try {
      await clearHistory();
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      render();
    }
  }

  async function runReveal(path: string): Promise<void> {
    try {
      await revealPath(path);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  function entryRow(entry: HistoryEntry, now: number, canReveal: boolean): HTMLElement {
    const status = SESSION_LABELS[entry.status];
    const row = el("div", {
      class: "history-row",
      children: [
        el("span", {
          class: "dir-icon",
          children: [icon(entry.direction === "send" ? "send" : "receive", 16)],
        }),
        el("div", {
          class: "history-main",
          children: [
            el("div", {
              class: "history-top",
              children: [
                el("span", { class: "history-peer", text: entry.peerAlias }),
                el("span", { class: `chip chip-${status.tone}`, text: status.label }),
              ],
            }),
            el("div", {
              class: "history-meta",
              children: [
                `${entry.fileCount} ${entry.fileCount === 1 ? "file" : "files"} · `,
                formatBytes(entry.totalSize),
                " · ",
                el("span", {
                  text: formatRelativeTime(entry.at, now),
                  title: formatDateTime(entry.at),
                }),
              ],
            }),
          ],
        }),
      ],
    });

    if (entry.direction === "receive" && entry.savedDir !== null && canReveal) {
      const directory = entry.savedDir;
      row.append(
        button("Open folder", {
          class: "btn btn-ghost btn-sm",
          icon: "openFolder",
          onClick: () => {
            void runReveal(directory);
          },
        }),
      );
    }
    return row;
  }

  function render(): void {
    const entries = store.snapshot?.history ?? [];
    const canReveal = store.snapshot?.canRevealFiles === true;
    const next = entries.map((entry) => `${entry.id}:${entry.status}:${entry.at}`).join("|");
    if (next === signature) {
      return;
    }
    signature = next;
    const now = Date.now();
    list.replaceChildren(...entries.map((entry) => entryRow(entry, now, canReveal)));
    empty.hidden = entries.length > 0;
    clearButton.disabled = entries.length === 0;
  }

  store.subscribe(render);
  render();

  return root;
}
