import type { ProgressPayload, SendFile, Snapshot } from "./types";

/** The files a send session was started with, kept so a PIN retry can resend them. */
export interface PendingSend {
  target: string;
  files: SendFile[];
}

type Listener = () => void;
type ProgressListener = (payload: ProgressPayload) => void;
type Unsubscribe = () => void;

/**
 * Single source of truth for the UI.
 *
 * Snapshots replace the whole state (`state` events carry a full `Snapshot`);
 * progress events only patch `transferred` in place and are dispatched separately so
 * that high-frequency byte updates never trigger a structural re-render.
 */
class Store {
  private state: Snapshot | null = null;
  private readonly listeners = new Set<Listener>();
  private readonly progressListeners = new Set<ProgressListener>();
  private selectedTarget: string | null = null;
  private readonly pending = new Map<string, PendingSend>();
  private readonly promptedPins = new Set<string>();

  get snapshot(): Snapshot | null {
    return this.state;
  }

  /** Fingerprint of the peer selected as send target. */
  get target(): string | null {
    return this.selectedTarget;
  }

  subscribe(listener: Listener): Unsubscribe {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  subscribeProgress(listener: ProgressListener): Unsubscribe {
    this.progressListeners.add(listener);
    return () => {
      this.progressListeners.delete(listener);
    };
  }

  setSnapshot(next: Snapshot): void {
    this.state = next;

    // A vanished peer can no longer be a send target.
    if (
      this.selectedTarget !== null &&
      !next.devices.some((device) => device.fingerprint === this.selectedTarget)
    ) {
      this.selectedTarget = null;
    }

    // Forget bookkeeping for sessions the backend has dropped.
    const liveSessions = new Set(next.sessions.map((session) => session.id));
    for (const id of [...this.pending.keys()]) {
      if (!liveSessions.has(id)) {
        this.pending.delete(id);
      }
    }
    for (const id of [...this.promptedPins]) {
      if (!liveSessions.has(id)) {
        this.promptedPins.delete(id);
      }
    }

    this.emit();
  }

  applyProgress(payload: ProgressPayload): void {
    const session = this.state?.sessions.find((item) => item.id === payload.sessionId);
    const file = session?.files.find((item) => item.id === payload.fileId);
    if (file !== undefined) {
      file.transferred =
        file.size > 0 ? Math.min(payload.transferred, file.size) : Math.max(0, payload.transferred);
    }
    for (const listener of [...this.progressListeners]) {
      listener(payload);
    }
  }

  selectTarget(fingerprint: string): void {
    if (this.selectedTarget === fingerprint) {
      return;
    }
    this.selectedTarget = fingerprint;
    this.emit();
  }

  rememberSend(sessionId: string, pending: PendingSend): void {
    this.pending.set(sessionId, pending);
  }

  pendingSend(sessionId: string): PendingSend | null {
    return this.pending.get(sessionId) ?? null;
  }

  forgetSend(sessionId: string): void {
    this.pending.delete(sessionId);
  }

  /** A `pinRequired` session prompts exactly once. */
  pinPrompted(sessionId: string): boolean {
    return this.promptedPins.has(sessionId);
  }

  markPinPrompted(sessionId: string): void {
    this.promptedPins.add(sessionId);
  }

  private emit(): void {
    for (const listener of [...this.listeners]) {
      listener();
    }
  }
}

export const store = new Store();
