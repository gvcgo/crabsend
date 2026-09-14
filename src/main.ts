import "./style.css";

import { describeError, getSnapshot, onProgress, onState } from "./api";
import { createApp } from "./app";
import { store } from "./store";
import { showToast } from "./views/toast";

const host = document.querySelector<HTMLDivElement>("#app");
if (host === null) {
  throw new Error("Missing #app root element");
}
host.replaceChildren(createApp());

async function start(): Promise<void> {
  // Subscribe before the initial read so no state event can slip through the gap.
  await onState((snapshot) => {
    store.setSnapshot(snapshot);
  });
  await onProgress((payload) => {
    store.applyProgress(payload);
  });
  store.setSnapshot(await getSnapshot());
}

start().catch((error: unknown) => {
  showToast(describeError(error), "error");
});
