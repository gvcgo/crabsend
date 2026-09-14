import { el } from "../dom";
import { shortFingerprint } from "../format";
import { icon } from "../icons";
import { DEVICE_LABELS, isPending } from "../status";
import { store } from "../store";
import type { Snapshot } from "../types";

export type TabId = "send" | "transfers" | "history" | "settings";

const TABS: { id: TabId; label: string }[] = [
  { id: "send", label: "Send" },
  { id: "transfers", label: "Transfers" },
  { id: "history", label: "History" },
  { id: "settings", label: "Settings" },
];

export interface Shell {
  el: HTMLElement;
  mount(tab: TabId, panel: HTMLElement): void;
  activate(tab: TabId): void;
}

/** Header, persistent server banner and the tab strip that hosts the panels. */
export function createShell(): Shell {
  const deviceLine = el("div", { class: "topbar-device" });
  const serverChip = el("div", { class: "server-chip", attrs: { "aria-live": "polite" } });
  const bannerSlot = el("div", { class: "banner-slot", attrs: { "aria-live": "polite" } });
  const badge = el("span", { class: "tab-badge", attrs: { hidden: "" } });

  const tabButtons = new Map<TabId, HTMLButtonElement>();
  const panels = new Map<TabId, HTMLElement>();

  const tabList = el("div", { class: "tabs", attrs: { role: "tablist", "aria-label": "Sections" } });
  for (const tab of TABS) {
    const tabButton = el("button", {
      class: "tab",
      text: tab.label,
      attrs: {
        type: "button",
        role: "tab",
        id: `tab-${tab.id}`,
        "aria-controls": `panel-${tab.id}`,
        "aria-selected": "false",
        tabindex: "-1",
      },
    });
    if (tab.id === "transfers") {
      tabButton.append(badge);
    }
    tabButton.addEventListener("click", () => {
      activate(tab.id);
    });
    tabButtons.set(tab.id, tabButton);
    tabList.append(tabButton);
  }

  const panelHost = el("main", { class: "panels" });
  for (const tab of TABS) {
    const panel = el("section", {
      class: "panel",
      attrs: {
        id: `panel-${tab.id}`,
        role: "tabpanel",
        "aria-labelledby": `tab-${tab.id}`,
        tabindex: "0",
        hidden: "",
      },
    });
    panels.set(tab.id, panel);
    panelHost.append(panel);
  }

  const root = el("div", {
    class: "app",
    children: [
      el("header", {
        class: "topbar",
        children: [
          el("div", {
            class: "brand",
            children: [icon("send", 18), el("span", { class: "brand-name", text: "Crabsend" })],
          }),
          deviceLine,
          serverChip,
        ],
      }),
      bannerSlot,
      tabList,
      panelHost,
    ],
  });

  let active: TabId = "send";

  function activate(tab: TabId): void {
    active = tab;
    for (const [id, panel] of panels) {
      panel.hidden = id !== tab;
    }
    for (const [id, tabButton] of tabButtons) {
      const selected = id === tab;
      tabButton.setAttribute("aria-selected", String(selected));
      tabButton.tabIndex = selected ? 0 : -1;
      tabButton.classList.toggle("is-active", selected);
    }
  }

  tabList.addEventListener("keydown", (event: KeyboardEvent) => {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (step === 0 && event.key !== "Home" && event.key !== "End") {
      return;
    }
    event.preventDefault();
    const currentIndex = TABS.findIndex((tab) => tabButtons.get(tab.id) === document.activeElement);
    const from = currentIndex < 0 ? 0 : currentIndex;
    const nextIndex =
      event.key === "Home" ? 0 : event.key === "End" ? TABS.length - 1 : (from + step + TABS.length) % TABS.length;
    const next = TABS[nextIndex];
    if (next === undefined) {
      return;
    }
    activate(next.id);
    tabButtons.get(next.id)?.focus();
  });

  function renderHeader(state: Snapshot | null): void {
    if (state === null) {
      deviceLine.replaceChildren(el("span", { class: "muted", text: "Loading…" }));
      serverChip.className = "server-chip";
      serverChip.replaceChildren(el("span", { class: "muted", text: "Starting" }));
      bannerSlot.replaceChildren();
      return;
    }

    deviceLine.replaceChildren(
      el("span", { class: "topbar-alias", text: state.device.alias }),
      el("span", { class: "sep", text: "·" }),
      el("span", { class: "muted", text: DEVICE_LABELS[state.device.deviceType] }),
      el("span", {
        class: "fp mono",
        text: shortFingerprint(state.device.fingerprint),
        title: state.device.fingerprint,
      }),
    );

    const server = state.server;
    if (server.error !== null) {
      serverChip.className = "server-chip is-danger";
      serverChip.replaceChildren(icon("alert", 14), el("span", { text: "Server error" }));
      bannerSlot.replaceChildren(
        el("div", {
          class: "banner",
          attrs: { role: "alert" },
          children: [icon("alert", 16), el("span", { text: `Server error: ${server.error}` })],
        }),
      );
    } else if (server.running) {
      serverChip.className = "server-chip is-ok";
      serverChip.replaceChildren(
        icon("server", 14),
        el("span", { text: `${server.protocol.toUpperCase()} · port ${server.port}` }),
      );
      bannerSlot.replaceChildren();
    } else {
      serverChip.className = "server-chip is-warn";
      serverChip.replaceChildren(icon("server", 14), el("span", { text: "Server stopped" }));
      bannerSlot.replaceChildren();
    }

    const activeSessions = state.sessions.filter((session) => isPending(session.status)).length;
    badge.textContent = activeSessions === 0 ? "" : String(activeSessions);
    badge.hidden = activeSessions === 0;
    tabButtons
      .get("transfers")
      ?.setAttribute("aria-label", activeSessions === 0 ? "Transfers" : `Transfers, ${activeSessions} in progress`);
  }

  store.subscribe(() => {
    renderHeader(store.snapshot);
  });
  renderHeader(store.snapshot);
  activate(active);

  return {
    el: root,
    mount(tab: TabId, panel: HTMLElement): void {
      panels.get(tab)?.append(panel);
    },
    activate,
  };
}
