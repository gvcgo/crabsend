import { describeError, unpairDevice } from "../api";
import { button, el } from "../dom";
import { store } from "../store";
import type { Device } from "../types";
import { showToast } from "./toast";

/** The paired devices, each with a way to forget it. */
export interface PairedList {
  /** One row per paired device; hidden while there is none. */
  rows: HTMLElement;
  /** The hint shown while there is none. */
  empty: HTMLElement;
}

/**
 * Builds a list of the devices this one is paired with.
 *
 * Pairing is what a QR code proves, and it is meant to be undone as easily as
 * it was made, so every row carries its own way out.
 */
export function createPairedList(emptyText: string): PairedList {
  const rows = el("div", { class: "paired-list" });
  const empty = el("p", { class: "empty", text: emptyText });

  function row(device: Device): HTMLElement {
    return el("div", {
      class: "paired-row",
      children: [
        el("span", {
          class: "device-body",
          children: [
            el("span", { class: "device-alias", text: device.alias }),
            el("span", {
              class: "device-meta",
              text: `${device.host}:${device.port} · ${device.protocol.toUpperCase()}`,
            }),
          ],
        }),
        button("Forget", {
          class: "btn btn-ghost",
          title: `Stop pairing with ${device.alias}`,
          onClick: () => {
            void forget(device);
          },
        }),
      ],
    });
  }

  async function forget(device: Device): Promise<void> {
    try {
      await unpairDevice(device.fingerprint);
      showToast(`Forgot ${device.alias}.`);
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  function render(): void {
    const paired = (store.snapshot?.devices ?? []).filter((device) => device.paired);
    rows.replaceChildren(...paired.map(row));
    rows.hidden = paired.length === 0;
    empty.hidden = paired.length > 0;
  }

  store.subscribe(render);
  render();

  return { rows, empty };
}
