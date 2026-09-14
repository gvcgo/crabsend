import { createHistoryView } from "./views/history";
import { initIncomingRequests } from "./views/incoming";
import { initPinPrompts } from "./views/pin";
import { createSendView } from "./views/send";
import { createSettingsView } from "./views/settings";
import { createShell } from "./views/shell";
import { createTransfersView } from "./views/transfers";

/** Builds the whole UI tree; call once and mount the result under `#app`. */
export function createApp(): HTMLElement {
  const shell = createShell();

  shell.mount(
    "send",
    createSendView({
      goToTransfers: () => shell.activate("transfers"),
      goToSend: () => shell.activate("send"),
    }),
  );
  shell.mount("transfers", createTransfersView());
  shell.mount("history", createHistoryView());
  shell.mount("settings", createSettingsView());

  initIncomingRequests();
  initPinPrompts();

  return shell.el;
}
