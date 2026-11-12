import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

const source = readFileSync(
  new URL("../src/lib/updater/use-update.ts", import.meta.url),
  "utf8",
).replaceAll("import.meta.env", "__TEST_ENV__");

const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS,
  },
});

function harness() {
  const calls = {
    check: 0,
    delta: 0,
    full: 0,
    install: 0,
    restart: 0,
    close: 0,
  };

  const config = {
    deltaError: null,
    restartFails: false,
    downloadGate: null,
    downloadStarted: null,
  };
  let channel = "beta";
  let installed = false;

  class LinuxDeltaError extends Error {
    constructor(kind, message) {
      super(message);
      this.kind = kind;
    }
  }

  const update = {
    version: "1.0.2",
    body: "Test update",
    async close() {
      calls.close++;
    },
    async download(onEvent) {
      calls.full++;
      onEvent({ event: "Started", data: { contentLength: 100 } });
      onEvent({ event: "Progress", data: { chunkLength: 100 } });
    },
    async install() {
      calls.install++;
      installed = true;
    },
    linuxDelta: {
      async download(onPhase) {
        calls.delta++;
        config.downloadStarted?.();
        if (config.downloadGate) await config.downloadGate;
        if (config.deltaError) throw config.deltaError;
        onPhase("reconstructing");
        onPhase("verifying");
        onPhase("ready");
      },
      async cancel() {},
      isInstalled: () => installed,
    },
  };

  const mocks = {
    react: {
      useSyncExternalStore: (_subscribe, snapshot) => snapshot(),
    },
    "@tauri-apps/plugin-updater": {
      async check() {
        calls.check++;
        return update;
      },
    },
    "@/lib/context-review": { isContextReview: () => false },
    "@/lib/platform": { isLinuxDesktop: () => true },
    "@/lib/config/endpoints": { HARBOR_API_BASE: "https://example.test" },
    "@/lib/i18n": { t: (text) => text },
    "@tauri-apps/api/core": {
      async invoke(command) {
        if (command === "updater_package_kind") return "appimage";
        if (
          command === "close_aux_windows" ||
          command === "stop_stremio_sidecar"
        ) {
          return;
        }
        throw new Error(`Unexpected native command: ${command}`);
      },
    },
    "@tauri-apps/plugin-process": {
      async relaunch() {
        calls.restart++;
        if (config.restartFails) throw new Error("Test restart failure");
      },
    },
    "./channel": {
      selectedUpdateChannel: () => channel,
      normalUpdateChannel: () => "beta",
      readChannelPreference: () => channel,
      writeUpdateChannel(value) {
        channel = value;
        return true;
      },
      UPDATE_CHANNEL_KEY: "test-channel",
    },
    "./linux-delta": {
      LinuxDeltaError,
      createLinuxUpdateHandle: (value) => value,
    },
    "./experimental": {},
    "./experimental-access": {
      currentExperimentalAccess: () => false,
    },
    "./beta-return": {},
    "./handoff": {},
  };

  const data = new Map();
  const localStorage = {
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => data.set(key, value),
    removeItem: (key) => data.delete(key),
  };

  const module = { exports: {} };

  new Function(
    "require",
    "module",
    "exports",
    "__TEST_ENV__",
    "window",
    "localStorage",
    "setTimeout",
    outputText,
  )(
    (name) => {
      assert.ok(Object.hasOwn(mocks, name), `Unexpected import: ${name}`);
      return mocks[name];
    },
    module,
    module.exports,
    { DEV: false, VITE_HARBOR_LINUX_UPDATE_CHANNEL: "beta" },
    { __TAURI_INTERNALS__: {} },
    localStorage,
    (callback, delay) => {
      assert.equal(delay, 600);
      callback();
      return 0;
    },
  );

  return {
    updater: module.exports,
    calls,
    config,
    LinuxDeltaError,
    changeChannelExternally(value) {
      channel = value;
    },
  };
}

test("full download requires an eligible error and explicit action", async () => {
  const h = harness();
  await h.updater.checkForUpdate(true);

  await h.updater.downloadFullLinuxUpdate();
  assert.equal(h.calls.full, 0);

  h.config.deltaError = new h.LinuxDeltaError(
    "transfer",
    "Test transfer failure",
  );

  await h.updater.downloadUpdate();

  assert.equal(h.updater.useUpdate().status, "error");
  assert.equal(h.updater.useUpdate().linuxFullDownloadAvailable, true);
  assert.equal(h.calls.full, 0);

  await h.updater.downloadFullLinuxUpdate();

  assert.equal(h.calls.full, 1);
  assert.equal(h.updater.useUpdate().status, "downloaded");
  assert.equal(h.updater.useUpdate().linuxFullDownloadAvailable, false);
});

test("verification errors do not enable full-download fallback", async () => {
  const h = harness();
  await h.updater.checkForUpdate(true);

  h.config.deltaError = new h.LinuxDeltaError(
    "verification",
    "Test signature mismatch",
  );

  await h.updater.downloadUpdate();

  assert.equal(h.updater.useUpdate().status, "error");
  assert.equal(h.updater.useUpdate().linuxFullDownloadAvailable, false);

  await h.updater.downloadFullLinuxUpdate();
  await h.updater.installUpdate();

  assert.equal(h.calls.full, 0);
  assert.equal(h.calls.install, 0);
});

test("restart failure preserves installation and retries only restart", async () => {
  const h = harness();
  await h.updater.checkForUpdate(true);
  await h.updater.downloadUpdate();

  h.config.restartFails = true;
  await h.updater.installUpdate();

  assert.equal(h.calls.install, 1);
  assert.equal(h.calls.restart, 1);
  assert.equal(h.updater.useUpdate().linuxRestartRequired, true);
  assert.equal(h.updater.useUpdate().installFailed, false);
  assert.equal(h.updater.updateChannelLocked(), true);

  await h.updater.checkForUpdate(true);
  await h.updater.installUpdate();

  assert.equal(h.calls.check, 1);
  assert.equal(h.calls.install, 1);
  assert.equal(h.updater.setUpdateChannel("stable"), false);

  h.config.restartFails = false;
  await h.updater.restartAfterLinuxUpdate();

  assert.equal(h.calls.restart, 2);
  assert.equal(h.calls.install, 1);
});

for (const outcome of ["success", "failure"]) {
  test(
    `stale delta ${outcome} is discarded after an external channel change`,
    { timeout: 5000 },
    async () => {
      const h = harness();
      await h.updater.checkForUpdate(true);

      let releaseDownload;
      h.config.downloadGate = new Promise((resolve) => {
        releaseDownload = resolve;
      });

      const started = new Promise((resolve) => {
        h.config.downloadStarted = resolve;
      });

      if (outcome === "failure") {
        h.config.deltaError = new h.LinuxDeltaError(
          "transfer",
          "Outdated download failure",
        );
      }

      const downloading = h.updater.downloadUpdate();
      await started;

      assert.equal(h.updater.useUpdate().status, "downloading");
      assert.equal(h.updater.setUpdateChannel("stable"), false);

      h.changeChannelExternally("stable");
      releaseDownload();
      await downloading;

      const state = h.updater.useUpdate();

      assert.equal(state.status, "idle");
      assert.equal(state.channel, "stable");
      assert.equal(state.version, null);
      assert.equal(state.error, null);
      assert.equal(state.linuxDeltaPhase, null);
      assert.equal(state.linuxFullDownloadAvailable, false);
      assert.equal(h.calls.close, 1);

      await h.updater.installUpdate();
      await h.updater.downloadFullLinuxUpdate();

      assert.equal(h.calls.install, 0);
      assert.equal(h.calls.full, 0);
    },
  );
}
