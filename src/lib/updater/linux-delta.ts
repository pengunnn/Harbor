import { Channel, Resource, invoke } from "@tauri-apps/api/core";
import type { Update } from "@tauri-apps/plugin-updater";
import { isLinuxDesktop } from "@/lib/platform";

export type LinuxDeltaPhase = "reconstructing" | "verifying" | "ready";

type SessionInfo = {
  rid: number;
  version: string;
};

export class LinuxDeltaError extends Error {
  constructor(
    public readonly kind: string,
    message: string,
  ) {
    super(message);
    this.name = "LinuxDeltaError";
  }
}

function normalizeError(error: unknown): LinuxDeltaError {
  if (error instanceof LinuxDeltaError) return error;

  if (typeof error === "object" && error !== null) {
    const value = error as { kind?: unknown; message?: unknown };
    if (typeof value.kind === "string" && typeof value.message === "string") {
      return new LinuxDeltaError(value.kind, value.message);
    }
  }

  return new LinuxDeltaError(
    "internal",
    error instanceof Error ? error.message : String(error),
  );
}

export class LinuxDeltaSession extends Resource {
  private closed = false;
  private downloadStarted = false;
  private ready = false;
  private installing = false;
  private installed = false;
  private closePromise: Promise<void> | null = null;

  constructor(
    rid: number,
    public readonly version: string,
  ) {
    super(rid);
  }

  private assertOpen(): void {
    if (this.closed) {
      throw new LinuxDeltaError("cancelled", "The delta session was closed.");
    }
  }

  async download(onPhase: (phase: LinuxDeltaPhase) => void): Promise<void> {
    this.assertOpen();

    if (this.downloadStarted) {
      throw new LinuxDeltaError(
        "state",
        "This delta session has already been used.",
      );
    }

    this.downloadStarted = true;

    const channel = new Channel<LinuxDeltaPhase>();
    channel.onmessage = (phase) => {
      if (
        !this.closed &&
        (phase === "reconstructing" || phase === "verifying" || phase === "ready")
      ) {
        onPhase(phase);
      }
    };

    try {
      await invoke("updater_delta_download", {
        sessionRid: this.rid,
        onEvent: channel,
      });

      this.assertOpen();
      this.ready = true;
    } catch (error) {
      this.ready = false;

      // Rust normally removes failed downloads itself. Closing again is
      // best-effort cleanup if the failure occurred before Rust handled it.
      await this.close().catch(() => {});
      throw normalizeError(error);
    }
  }

  async install(): Promise<void> {
    this.assertOpen();

    if (!this.ready || this.installing || this.installed) {
      throw new LinuxDeltaError(
        "state",
        "The delta session is not ready for installation.",
      );
    }

    this.installing = true;

    try {
      await invoke("updater_delta_install", {
        sessionRid: this.rid,
      });

      this.installed = true;
      this.ready = false;
    } catch (error) {
      throw normalizeError(error);
    } finally {
      this.installing = false;
    }
  }

  override close(): Promise<void> {
    if (this.installing) {
      return Promise.reject(
        new LinuxDeltaError(
          "state",
          "Installation has started and cannot be cancelled.",
        ),
      );
    }

    if (this.closePromise) return this.closePromise;

    this.closed = true;
    this.ready = false;

    // Rust already closes the resource after successful installation.
    this.closePromise = this.installed ? Promise.resolve() : super.close();
    return this.closePromise;
  }
}

export async function createLinuxDeltaSession(
  update: Update,
): Promise<LinuxDeltaSession> {
  if (!isLinuxDesktop()) {
    throw new LinuxDeltaError(
      "unsupported",
      "Delta updates are only supported on Linux.",
    );
  }

  let info: SessionInfo;
  try {
    info = await invoke<SessionInfo>("updater_delta_create", {
      updateRid: update.rid,
    });
  } catch (error) {
    throw normalizeError(error);
  }

  const session = new LinuxDeltaSession(info.rid, info.version);

  if (info.version !== update.version) {
    await session.close().catch(() => {});
    throw new LinuxDeltaError(
      "state",
      "The delta session does not match the checked update.",
    );
  }

  return session;
}

export function createLinuxUpdateHandle(update: Update) {
  let session: LinuxDeltaSession | null = null;
  let deltaTask: Promise<void> | null = null;
  let fullTask: Promise<void> | null = null;
  let cancelled = false;
  let closed = false;
  let installing = false;
  let installed = false;
  let method: "delta" | "full" | null = null;
  let closePromise: Promise<void> | null = null;

  function assertOpen(): void {
    if (closed || installed) {
      throw new LinuxDeltaError(
        "state",
        "This update handle is no longer available.",
      );
    }
  }

  const linuxDelta = {
    download(onPhase: (phase: LinuxDeltaPhase) => void): Promise<void> {
      assertOpen();

      if (deltaTask || fullTask || installing) {
        throw new LinuxDeltaError("state", "An update operation is already running.");
      }

      cancelled = false;
      method = null;

      const task = (async () => {
        await session?.close().catch(() => {});
        session = null;

        const available = await invoke<boolean>("updater_delta_available");
        if (closed || cancelled) {
          throw new LinuxDeltaError("cancelled", "The delta download was cancelled.");
        }

        if (!available) {
          throw new LinuxDeltaError(
            "unavailable",
            "This AppImage does not contain the delta helper.",
          );
        }

        const created = await createLinuxDeltaSession(update);

        // Cancellation can arrive while Rust is creating the session.
        if (closed || cancelled) {
          await created.close().catch(() => {});
          throw new LinuxDeltaError("cancelled", "The delta download was cancelled.");
        }

        session = created;

        await created.download((phase) => {
          if (!closed && !cancelled) onPhase(phase);
        });

        if (closed || cancelled) {
          await created.close().catch(() => {});
          throw new LinuxDeltaError("cancelled", "The delta download was cancelled.");
        }

        method = "delta";
      })();

      deltaTask = task;

      return task.finally(() => {
        if (deltaTask === task) deltaTask = null;
      });
    },

    async cancel(): Promise<void> {
      if (installing) {
        throw new LinuxDeltaError(
          "state",
          "Installation has started and cannot be cancelled.",
        );
      }

      cancelled = true;

      // A failed native download may already have removed its resource.
      await session?.close().catch(() => {});
      await deltaTask?.catch(() => {});

      session = null;
      if (method === "delta") method = null;
    },

    isInstalled(): boolean {
      return installed;
    },
  };

  return {
    version: update.version,
    body: update.body,
    rawJson: update.rawJson,
    linuxDelta,

    async download(
      onEvent: Parameters<Update["download"]>[0],
    ): Promise<void> {
      assertOpen();

      if (deltaTask || fullTask || installing) {
        throw new LinuxDeltaError("state", "An update operation is already running.");
      }

      // This method remains the full-download route.
      // The UI will call it explicitly after the user chooses that option.
      method = null;
      const task = update.download(onEvent);
      fullTask = task;

      try {
        await task;
        assertOpen();
        method = "full";
      } finally {
        if (fullTask === task) fullTask = null;
      }
    },

    async install(): Promise<void> {
      assertOpen();

      if (installing || deltaTask || fullTask || !method) {
        throw new LinuxDeltaError("state", "The update is not ready to install.");
      }

      installing = true;

      try {
        if (method === "delta") {
          if (!session) {
            throw new LinuxDeltaError("state", "The delta session is unavailable.");
          }
          await session.install();
        } else {
          await update.install();
        }

        installed = true;
      } finally {
        installing = false;
      }
    },

    close(): Promise<void> {
      if (installing) {
        return Promise.reject(
          new LinuxDeltaError(
            "state",
            "Installation has started and cannot be cancelled.",
          ),
        );
      }

      if (closePromise) return closePromise;
      closed = true;

      closePromise = (async () => {
        await linuxDelta.cancel();

        // Tauri's existing full-download API has no cancellation method.
        // Wait before closing it so late-arriving downloaded bytes are released.
        await fullTask?.catch(() => {});
        await update.close();
      })();

      return closePromise;
    },
  };
}
