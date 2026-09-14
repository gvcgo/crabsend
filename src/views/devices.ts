import { addDeviceByIp, clearDevices, describeError, scan } from "../api";
import { button, el } from "../dom";
import { formatRelativeTime } from "../format";
import { icon } from "../icons";
import { store } from "../store";
import type { Device } from "../types";
import { scanPairingCode, showPairingCode } from "./pair";
import { showToast } from "./toast";

/** Peer list with scan, manual-IP entry and target selection. */
export function createDevicesPanel(): HTMLElement {
  // Long enough for the announcement burst and the subnet probes to land.
  const SCAN_SETTLE = 3000;
  const countLabel = el("span", { class: "panel-count" });
  const list = el("div", { class: "device-list" });
  const empty = el("p", {
    class: "empty",
    text: "No devices found. Open Crabsend or LocalSend on the other device, then scan the network.",
  });

  const scanButton = button("Scan", {
    class: "btn btn-ghost",
    icon: "refresh",
    onClick: () => {
      void runScan();
    },
  });
  const clearButton = button("Clear", {
    class: "btn btn-ghost",
    icon: "trash",
    onClick: () => {
      void runClear();
    },
  });
  const pairButton = button("Show QR", {
    class: "btn btn-ghost",
    icon: "qr",
    title: "Show a code another device can scan to pair without discovery",
    onClick: () => {
      void showPairingCode();
    },
  });
  const qrScanButton = button("Scan QR", {
    class: "btn btn-ghost",
    icon: "camera",
    title: "Read a pairing code from another device",
    onClick: () => {
      void scanPairingCode();
    },
  });

  const ipInput = el("input", {
    class: "input",
    attrs: {
      type: "text",
      placeholder: "IP address, host name or host:port",
      "aria-label": "Peer IP address, host name or host:port",
      spellcheck: "false",
      autocomplete: "off",
    },
  });
  const addButton = button("Add", {
    class: "btn",
    onClick: () => {
      void runAdd();
    },
  });
  ipInput.addEventListener("keydown", (event: KeyboardEvent) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void runAdd();
    }
  });

  const root = el("section", {
    class: "card devices-card",
    children: [
      el("div", {
        class: "card-head",
        children: [
          el("h2", { class: "card-title", text: "Devices" }),
          countLabel,
          el("div", {
            class: "card-actions",
            children: [scanButton, clearButton, pairButton, qrScanButton],
          }),
        ],
      }),
      el("div", { class: "manual-ip", children: [ipInput, addButton] }),
      list,
      empty,
    ],
  });

  async function runScan(): Promise<void> {
    scanButton.disabled = true;
    countLabel.textContent = "Scanning…";
    const before = store.snapshot?.devices.length ?? 0;
    try {
      await scan();
      // The command announces and probes the whole subnet in the background, so
      // its result only exists once that has had time to finish. Without saying
      // so the button looks like it did nothing.
      const settled = Promise.withResolvers<void>();
      window.setTimeout(settled.resolve, SCAN_SETTLE);
      await settled.promise;
      const after = store.snapshot?.devices.length ?? 0;
      const found = after - before;
      if (after === 0) {
        showToast(
          "No device answered. Run Crabsend or LocalSend on another device of this network, or scan its pairing code.",
        );
      } else if (found === 0) {
        showToast(`No new device; ${after} ${after === 1 ? "device" : "devices"} in the list.`);
      } else {
        showToast(`${found} new ${found === 1 ? "device" : "devices"} found.`);
      }
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      scanButton.disabled = false;
      render();
    }
  }

  async function runClear(): Promise<void> {
    clearButton.disabled = true;
    try {
      await clearDevices();
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      render();
    }
  }

  async function runAdd(): Promise<void> {
    const value = ipInput.value.trim();
    if (value.length === 0) {
      ipInput.focus();
      return;
    }
    addButton.disabled = true;
    try {
      await addDeviceByIp(value);
      ipInput.value = "";
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      addButton.disabled = false;
    }
  }

  function deviceCard(device: Device, now: number): HTMLButtonElement {
    const selected = store.target === device.fingerprint;
    const cardNode = el("button", {
      class: selected ? "device-card is-selected" : "device-card",
      attrs: { type: "button", "aria-pressed": String(selected) },
      title: `Fingerprint ${device.fingerprint}${device.paired ? " (paired)" : ""}`,
      children: [
        el("span", { class: "device-icon", children: [icon(device.deviceType, 22)] }),
        el("span", {
          class: "device-body",
          children: [
            el("span", { class: "device-alias", text: device.alias }),
            el("span", {
              class: "device-meta",
              text: `${device.host}:${device.port} · seen ${formatRelativeTime(device.lastSeen, now)}`,
            }),
          ],
        }),
        device.paired ? el("span", { class: "badge badge-paired", text: "Paired" }) : null,
        el("span", { class: "badge", text: device.protocol.toUpperCase() }),
      ],
    });
    cardNode.addEventListener("click", () => {
      store.selectTarget(device.fingerprint);
    });
    return cardNode;
  }

  function render(): void {
    const devices = store.snapshot?.devices ?? [];
    const pairing = store.snapshot?.pairing;
    pairButton.hidden = pairing?.canShowQr !== true;
    qrScanButton.hidden = pairing?.canScan !== true;
    const now = Date.now();
    list.replaceChildren(...devices.map((device) => deviceCard(device, now)));
    empty.hidden = devices.length > 0;
    clearButton.disabled = devices.length === 0;
    countLabel.textContent =
      devices.length === 0 ? "" : `${devices.length} ${devices.length === 1 ? "device" : "devices"}`;
  }

  store.subscribe(render);
  render();

  return root;
}
