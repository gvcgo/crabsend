import {
  Format,
  cancel,
  checkPermissions,
  requestPermissions,
  scan,
} from "@tauri-apps/plugin-barcode-scanner";
import { describeError, getPairingQr, pairFromQr } from "../api";
import { button, el } from "../dom";
import { icon } from "../icons";
import { store } from "../store";
import { openModal } from "./modal";
import { createPairedList } from "./paired";
import { showToast } from "./toast";

/**
 * Shows this device's pairing code together with the devices already paired.
 *
 * Scanning the code is what proves the identity: the fingerprint inside it is
 * the one the scanning device pins its connection to, which discovery over
 * multicast cannot offer.
 */
export async function showPairingCode(): Promise<void> {
  let code;
  try {
    code = await getPairingQr();
  } catch (error) {
    showToast(describeError(error), "error");
    return;
  }

  // The code travels as a blob: the CSP allows `blob:` images, and no markup
  // from outside this module is ever injected into the document.
  const url = URL.createObjectURL(new Blob([code.svg], { type: "image/svg+xml" }));
  const image = el("img", {
    class: "qr-image",
    attrs: { src: url, alt: "Pairing code for this device" },
  });
  const paired = createPairedList("No device is paired with this one yet.");

  const body = el("div", {
    class: "modal-body",
    children: [
      el("div", { class: "qr-frame", children: [image] }),
      el("p", {
        class: "modal-note",
        children: [
          icon("lock", 14),
          el("span", {
            text: "Scan this with Crabsend on the other device. The code carries this device's certificate fingerprint, so the connection is pinned to it.",
          }),
        ],
      }),
      el("h3", { class: "paired-heading", text: "Paired devices" }),
      paired.rows,
      paired.empty,
    ],
  });

  const close = (): void => {
    URL.revokeObjectURL(url);
    handle.close();
  };
  const handle = openModal({
    title: "Pair a device",
    subtitle: `${store.snapshot?.device.alias ?? "This device"}`,
    body,
    actions: [button("Close", { class: "btn btn-primary", onClick: close })],
    onEscape: close,
  });
}

/**
 * Reads a pairing code with the camera and pairs with the device it names.
 *
 * The native side of the plugin exists on mobile only, so the interface offers
 * this where `snapshot.pairing.canScan` is set.
 */
export async function scanPairingCode(): Promise<void> {
  // Cancelling cannot make the plugin settle its own promise, so the overlay
  // races the scan and wins on its own.
  const cancellation = Promise.withResolvers<never>();
  let stopScanning: () => void = () => {};

  try {
    let permission = await checkPermissions();
    if (permission !== "granted") {
      permission = await requestPermissions();
    }
    if (permission !== "granted") {
      showToast("Crabsend needs the camera to read a pairing code.", "error");
      return;
    }

    stopScanning = openScanOverlay(() => {
      void cancel();
      cancellation.reject(new Error(CANCELLED));
    });
    // `windowed` keeps the page on top of the camera, which is what the overlay
    // needs; without it the preview would cover the interface and nothing could
    // be tapped — not even a way out.
    const scanned = await Promise.race([
      withTimeout(scan({ formats: [Format.QRCode], windowed: true })),
      cancellation.promise,
    ]);
    const device = await pairFromQr(scanned.content);
    showToast(`Paired with ${device.alias}.`);
  } catch (error) {
    const message = describeError(error);
    // Backing out is the user's own doing and needs no complaint.
    if (message !== CANCELLED) {
      showToast(message, "error");
    }
  } finally {
    stopScanning();
  }
}

/** Marker for the scan the user stopped themselves. */
const CANCELLED = "Scanning cancelled.";

/** How long a scan may run before the interface stops waiting for the camera. */
const SCAN_TIMEOUT = 45_000;

/** Full-screen layer over the camera preview, with a way out of the scan. */
function openScanOverlay(onCancel: () => void): () => void {
  const overlay = el("div", {
    class: "scan-overlay",
    attrs: { role: "dialog", "aria-label": "Scanning a pairing code" },
    children: [
      el("div", { class: "scan-frame" }),
      el("div", {
        class: "scan-foot",
        children: [
          el("p", {
            class: "scan-hint",
            text: "Point the camera at the pairing code on the other device.",
          }),
          button("Cancel", { class: "btn", onClick: onCancel }),
        ],
      }),
    ],
  });
  document.body.classList.add("is-scanning");
  document.body.append(overlay);
  return () => {
    overlay.remove();
    document.body.classList.remove("is-scanning");
  };
}

/**
 * Bounds a scan.
 *
 * The plugin abandons the request on a device that reports no camera, and it
 * waits forever for a barcode model that Google Play services cannot deliver;
 * in both cases nothing resolves and no code is read, so the interface has to
 * give up on its own.
 */
function withTimeout<T>(work: Promise<T>): Promise<T> {
  const expiry = Promise.withResolvers<never>();
  const timer = window.setTimeout(() => {
    void cancel();
    expiry.reject(new Error("No code was read. Check the camera permission and the network."));
  }, SCAN_TIMEOUT);
  return Promise.race([work, expiry.promise]).finally(() => {
    window.clearTimeout(timer);
  });
}
