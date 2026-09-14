import { describeError, inspectFiles, onDragDrop, pickFiles, pickFolder, sendFiles } from "../api";
import { button, el, iconButton } from "../dom";
import { formatBytes, isDirectoryMime } from "../format";
import { icon } from "../icons";
import { store } from "../store";
import type { SendFile } from "../types";
import { createDevicesPanel } from "./devices";
import { showToast } from "./toast";

export interface SendViewOptions {
  /** Progress of a started session is shown on the Transfers tab. */
  goToTransfers: () => void;
  /** A drop can happen while another tab is open; bring this view forward. */
  goToSend: () => void;
}

export function createSendView(options: SendViewOptions): HTMLElement {
  const selected: SendFile[] = [];
  let busy = false;
  let inspecting = false;

  const filesCount = el("span", { class: "panel-count" });
  const fileList = el("ul", { class: "file-list" });
  const filesEmpty = el("p", { class: "empty", text: "No files selected." });
  const targetLabel = el("div", { class: "send-target" });

  const sendButton = button("Send", {
    class: "btn btn-primary",
    icon: "send",
    onClick: () => {
      void submit();
    },
  });
  const addFilesButton = button("Add files", {
    class: "btn",
    icon: "plus",
    onClick: () => {
      void pickAndAdd();
    },
  });
  const addFolderButton = button("Add folder", {
    class: "btn",
    icon: "folder",
    onClick: () => {
      void pickAndAddFolder();
    },
  });

  const dropzone = el("div", {
    class: "dropzone",
    children: [
      el("div", {
        class: "dropzone-inner",
        children: [
          el("span", { class: "dropzone-icon", children: [icon("plus", 22)] }),
          el("p", { class: "dropzone-title", text: "Drop files here" }),
          el("p", { class: "dropzone-sub", text: "or pick them from disk" }),
          el("div", { class: "dropzone-actions", children: [addFilesButton, addFolderButton] }),
        ],
      }),
    ],
  });

  const root = el("div", {
    class: "send-view",
    children: [
      createDevicesPanel(),
      el("section", {
        class: "card files-card",
        children: [
          el("div", {
            class: "card-head",
            children: [el("h2", { class: "card-title", text: "Files" }), filesCount],
          }),
          dropzone,
          fileList,
          filesEmpty,
          el("div", { class: "send-footer", children: [targetLabel, sendButton] }),
        ],
      }),
    ],
  });

  function renderSendState(): void {
    sendButton.disabled = busy || inspecting || selected.length === 0 || store.target === null;
    addFilesButton.disabled = inspecting;
    // Android's file dialogs cannot choose a directory, and a picked tree URI
    // is not a folder this application can walk, so the button goes away there.
    addFolderButton.disabled = inspecting;
    addFolderButton.hidden = store.snapshot?.canPickFolder === false;
    sendButton.classList.toggle("is-busy", busy);
  }

  function removeFile(path: string): void {
    const index = selected.findIndex((file) => file.path === path);
    if (index < 0) {
      return;
    }
    selected.splice(index, 1);
    renderFiles();
  }

  function fileRow(file: SendFile): HTMLLIElement {
    return el("li", {
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
                el("span", { class: "file-name", text: file.name, title: file.path }),
                el("span", { class: "file-size", text: formatBytes(file.size) }),
              ],
            }),
            el("div", { class: "file-meta mono", text: file.path }),
          ],
        }),
        iconButton("cancel", `Remove ${file.name}`, {
          class: "btn btn-icon btn-sm",
          onClick: () => {
            removeFile(file.path);
          },
        }),
      ],
    });
  }

  function renderFiles(): void {
    fileList.replaceChildren(...selected.map(fileRow));
    filesEmpty.hidden = selected.length > 0;
    const total = selected.reduce((sum, file) => sum + file.size, 0);
    filesCount.textContent =
      selected.length === 0
        ? ""
        : `${selected.length} ${selected.length === 1 ? "item" : "items"} · ${formatBytes(total)}`;
    renderSendState();
  }

  function renderTarget(): void {
    const device = store.snapshot?.devices.find((item) => item.fingerprint === store.target) ?? null;
    if (device === null) {
      targetLabel.replaceChildren(el("span", { class: "muted", text: "Select a device to send to." }));
    } else {
      targetLabel.replaceChildren(
        el("span", { class: "muted", text: "Sending to" }),
        el("span", {
          class: "target-alias",
          children: [icon(device.deviceType, 16), el("strong", { text: device.alias })],
        }),
      );
    }
    renderSendState();
  }

  async function addPaths(paths: string[]): Promise<void> {
    const wanted = paths.filter(
      (path) => path.length > 0 && !selected.some((file) => file.path === path),
    );
    if (wanted.length === 0) {
      return;
    }
    inspecting = true;
    renderSendState();
    try {
      const inspected = await inspectFiles(wanted);
      for (const file of inspected) {
        if (!selected.some((item) => item.path === file.path)) {
          selected.push(file);
        }
      }
      renderFiles();
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      inspecting = false;
      renderSendState();
    }
  }

  async function pickAndAdd(): Promise<void> {
    try {
      await addPaths(await pickFiles("Add files"));
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function pickAndAddFolder(): Promise<void> {
    try {
      const path = await pickFolder("Add folder", null);
      if (path !== null) {
        await addPaths([path]);
      }
    } catch (error) {
      showToast(describeError(error), "error");
    }
  }

  async function submit(): Promise<void> {
    const target = store.target;
    if (target === null || selected.length === 0 || busy) {
      return;
    }
    const files = [...selected];
    busy = true;
    renderSendState();
    try {
      const sessionId = await sendFiles(target, files, null);
      store.rememberSend(sessionId, { target, files });
      selected.length = 0;
      renderFiles();
      options.goToTransfers();
    } catch (error) {
      showToast(describeError(error), "error");
    } finally {
      busy = false;
      renderSendState();
    }
  }

  onDragDrop((event) => {
    if (event.type === "over" || event.type === "enter") {
      dropzone.classList.add("is-over");
      return;
    }
    if (event.type === "leave") {
      dropzone.classList.remove("is-over");
      return;
    }
    dropzone.classList.remove("is-over");
    if (event.paths.length === 0) {
      return;
    }
    options.goToSend();
    void addPaths(event.paths);
  }).catch((error: unknown) => {
    showToast(describeError(error), "error");
  });

  store.subscribe(renderTarget);
  renderTarget();
  renderFiles();

  return root;
}
