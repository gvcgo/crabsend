import { describeError, pickFolder, updateSettings } from "../api";
import { button, checkField, el, field } from "../dom";
import { DEVICE_LABELS, DEVICE_TYPES, isDeviceType } from "../status";
import { store } from "../store";
import type { Settings, Snapshot } from "../types";
import { createPairedList } from "./paired";
import { showToast } from "./toast";

/** Every `Settings` field, plus the read-only identity and live server status. */
export function createSettingsView(): HTMLElement {
  let dirty = false;
  let busy = false;
  let lastSignature = "";

  const aliasInput = el("input", { class: "input", attrs: { type: "text", autocomplete: "off" } });
  const modelInput = el("input", {
    class: "input",
    attrs: { type: "text", placeholder: "Not set", autocomplete: "off" },
  });
  const typeSelect = el("select", {
    class: "input",
    children: DEVICE_TYPES.map((type) =>
      el("option", { text: DEVICE_LABELS[type], attrs: { value: type } }),
    ),
  });
  const portInput = el("input", {
    class: "input",
    attrs: { type: "number", min: "1", max: "65535", step: "1" },
  });
  const encryptionInput = el("input", { attrs: { type: "checkbox" } });
  const dirInput = el("input", {
    class: "input",
    attrs: { type: "text", autocomplete: "off", spellcheck: "false" },
  });
  const pinInput = el("input", {
    class: "input",
    attrs: { type: "text", placeholder: "No PIN", autocomplete: "off" },
  });
  const autoAcceptInput = el("input", { attrs: { type: "checkbox" } });
  const checksumsInput = el("input", { attrs: { type: "checkbox" } });
  const fingerprintInput = el("input", {
    class: "input mono",
    attrs: { type: "text", readonly: "", "aria-label": "Fingerprint of this device" },
  });

  const serverStatus = el("p", { class: "server-status", attrs: { "aria-live": "polite" } });
  const formError = el("p", { class: "form-error", attrs: { role: "alert" } });
  const dirtyHint = el("span", { class: "dirty-hint", text: "Unsaved changes", attrs: { hidden: "" } });
  const saveButton = button("Save changes", {
    class: "btn btn-primary",
    onClick: () => {
      void save();
    },
  });
  saveButton.type = "submit";

  const browseButton = button("Browse…", {
    class: "btn",
    onClick: () => {
      void browse();
    },
  });

  const form = el("form", {
    class: "settings-form",
    children: [
      el("section", {
        class: "card",
        children: [
          el("div", {
            class: "card-head",
            children: [
              el("h2", { class: "card-title", text: "This device" }),
              dirtyHint,
              el("div", { class: "card-actions", children: [saveButton] }),
            ],
          }),
          el("div", {
            class: "grid-2",
            children: [
              field("Alias", aliasInput, { hint: "Shown to other devices on the network." }),
              field("Device model", modelInput, { hint: "Optional; empty means unknown." }),
              field("Device type", typeSelect),
              field("Fingerprint", fingerprintInput, { hint: "Read-only identity of this device." }),
            ],
          }),
        ],
      }),
      el("section", {
        class: "card",
        children: [
          el("div", {
            class: "card-head",
            children: [el("h2", { class: "card-title", text: "Server" })],
          }),
          serverStatus,
          el("div", {
            class: "grid-2",
            children: [
              field("Port", portInput, { hint: "1 – 65535. Changing it restarts the server." }),
              checkField("Encryption", encryptionInput, {
                hint: "HTTPS with mutual TLS. Changing it restarts the server.",
              }),
            ],
          }),
        ],
      }),
      el("section", {
        class: "card",
        children: [
          el("div", {
            class: "card-head",
            children: [el("h2", { class: "card-title", text: "Transfers" })],
          }),
          el("div", {
            class: "grid-2",
            children: [
              field(
                "Download folder",
                el("div", { class: "input-row", children: [dirInput, browseButton] }),
              ),
              field("PIN", pinInput, {
                hint: "Senders must enter it to reach this device. Empty disables the PIN.",
              }),
            ],
          }),
          checkField("Accept incoming transfers without asking", autoAcceptInput),
          checkField("Create checksums (SHA-256) before sending", checksumsInput),
        ],
      }),
      formError,
    ],
  });

  // Pairing is undone where it is looked for, which is the settings rather
  // than only the code that shows the QR.
  const paired = createPairedList("No device is paired with this one.");
  form.insertBefore(
    el("section", {
      class: "card",
      children: [
        el("div", {
          class: "card-head",
          children: [el("h2", { class: "card-title", text: "Paired devices" })],
        }),
        paired.rows,
        paired.empty,
      ],
    }),
    formError,
  );

  function renderServer(state: Snapshot | null): void {
    if (state === null) {
      serverStatus.className = "server-status";
      serverStatus.textContent = "Loading…";
      return;
    }
    const server = state.server;
    if (server.error !== null) {
      serverStatus.className = "server-status is-danger";
      serverStatus.textContent = `Server error: ${server.error}`;
      return;
    }
    serverStatus.className = server.running ? "server-status is-ok" : "server-status is-warn";
    serverStatus.textContent = server.running
      ? `Running — ${server.protocol.toUpperCase()} on port ${server.port}`
      : "Server stopped";
  }

  function renderSaveState(): void {
    saveButton.disabled = busy || !dirty;
    saveButton.classList.toggle("is-busy", busy);
    dirtyHint.hidden = !dirty;
  }

  function applySettings(settings: Settings): void {
    aliasInput.value = settings.alias;
    modelInput.value = settings.deviceModel ?? "";
    typeSelect.value = settings.deviceType;
    portInput.value = String(settings.port);
    encryptionInput.checked = settings.encryption;
    dirInput.value = settings.downloadDir;
    pinInput.value = settings.pin ?? "";
    autoAcceptInput.checked = settings.autoAccept;
    checksumsInput.checked = settings.createChecksums;
  }

  function readDraft(): Settings {
    const model = modelInput.value.trim();
    const pin = pinInput.value.trim();
    const deviceType = typeSelect.value;
    return {
      alias: aliasInput.value.trim(),
      deviceModel: model === "" ? null : model,
      deviceType: isDeviceType(deviceType) ? deviceType : "desktop",
      port: Number.parseInt(portInput.value, 10),
      encryption: encryptionInput.checked,
      downloadDir: dirInput.value.trim(),
      pin: pin === "" ? null : pin,
      autoAccept: autoAcceptInput.checked,
      createChecksums: checksumsInput.checked,
    };
  }

  async function save(): Promise<void> {
    if (busy) {
      return;
    }
    const draft = readDraft();
    if (draft.alias.length === 0) {
      formError.textContent = "Alias must not be empty.";
      aliasInput.focus();
      return;
    }
    if (draft.downloadDir.length === 0) {
      formError.textContent = "Download folder must not be empty.";
      dirInput.focus();
      return;
    }
    if (!Number.isInteger(draft.port) || draft.port < 1 || draft.port > 65535) {
      formError.textContent = "Port must be a whole number between 1 and 65535.";
      portInput.focus();
      return;
    }
    formError.textContent = "";
    busy = true;
    renderSaveState();
    try {
      const snapshot = await updateSettings(draft);
      dirty = false;
      store.setSnapshot(snapshot);
      showToast("Settings saved");
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      busy = false;
      renderSaveState();
    }
  }

  async function browse(): Promise<void> {
    try {
      const picked = await pickFolder("Choose download folder", dirInput.value.trim());
      if (picked !== null) {
        dirInput.value = picked;
        dirty = true;
        formError.textContent = "";
        renderSaveState();
      }
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  function syncFromStore(): void {
    const state = store.snapshot;
    renderServer(state);
    if (state === null) {
      return;
    }
    fingerprintInput.value = state.device.fingerprint;
    // A phone has no folder picker: its dialogs cannot ask for a directory, so
    // the interface must not offer the buttons that would fail.
    browseButton.hidden = !state.canPickFolder;
    dirInput.readOnly = !state.canPickFolder;
    const signature = JSON.stringify(state.settings);
    if (signature === lastSignature) {
      return;
    }
    lastSignature = signature;
    if (dirty) {
      return;
    }
    applySettings(state.settings);
    renderSaveState();
  }

  for (const control of [
    aliasInput,
    modelInput,
    typeSelect,
    portInput,
    encryptionInput,
    dirInput,
    pinInput,
    autoAcceptInput,
    checksumsInput,
  ]) {
    control.addEventListener("input", () => {
      dirty = true;
      formError.textContent = "";
      renderSaveState();
    });
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void save();
  });

  store.subscribe(syncFromStore);
  syncFromStore();
  renderSaveState();

  return form;
}
