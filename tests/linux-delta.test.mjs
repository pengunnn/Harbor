import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

const source = readFileSync(
  new URL("../src/lib/updater/linux-delta.ts", import.meta.url),
  "utf8",
);

const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS,
  },
});

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function harness(options = {}) {
  const calls = [];
  const closed = [];

  let rejectActiveDownload = null;
  let pauseNextDownload = options.pauseFirstDownload === true;

  class Resource {
    constructor(rid) {
      this.rid = rid;
    }

    async close() {
      closed.push(this.rid);

      if (this.rid === 42 && rejectActiveDownload) {
        const reject = rejectActiveDownload;
        rejectActiveDownload = null;

        reject({
          kind: "cancelled",
          message: "Test download cancelled",
        });
      }
    }
  }

  class Channel {
    onmessage = () => {};
  }

  async function invoke(command, args) {
    calls.push(command);

    switch (command) {
      case "updater_delta_available":
        return options.available ?? true;

      case "updater_delta_create":
        options.createStarted?.resolve();
        if (options.createGate) await options.createGate.promise;
        return { rid: 42, version: "1.0.2" };

      case "updater_delta_download":
        if (options.downloadError) throw options.downloadError;
        if (pauseNextDownload) {
          pauseNextDownload = false;

          await new Promise((_, reject) => {
            rejectActiveDownload = reject;
            options.downloadStarted?.resolve();
          });
        }
        args.onEvent.onmessage("reconstructing");
        args.onEvent.onmessage("verifying");
        args.onEvent.onmessage("ready");
        return;

      case "updater_delta_install":
        return;

      default:
        throw new Error(`Unexpected command: ${command}`);
    }
  }

  const mocks = {
    "@tauri-apps/api/core": { Resource, Channel, invoke },
    "@/lib/platform": { isLinuxDesktop: () => true },
  };

  const module = { exports: {} };

  new Function("require", "module", "exports", outputText)(
    (name) => {
      assert.ok(Object.hasOwn(mocks, name), `Unexpected import: ${name}`);
      return mocks[name];
    },
    module,
    module.exports,
  );

  const update = {
    rid: 10,
    version: "1.0.2",
    body: "Test update",
    rawJson: {},
    async download() {
      calls.push("full-download");
    },
    async install() {
      calls.push("full-install");
    },
    async close() {
      calls.push("update-close");
    },
  };

  return {
    handle: module.exports.createLinuxUpdateHandle(update),
    calls,
    closed,
  };
}

test("delta uses native installation and blocks a second install", async () => {
  const h = harness();
  const phases = [];

  await h.handle.linuxDelta.download((phase) => phases.push(phase));

  assert.deepEqual(phases, ["reconstructing", "verifying", "ready"]);
  assert.equal(h.calls.includes("full-download"), false);

  await h.handle.install();

  assert.equal(h.handle.linuxDelta.isInstalled(), true);
  assert.equal(h.calls.includes("updater_delta_install"), true);
  assert.equal(h.calls.includes("full-install"), false);

  await assert.rejects(h.handle.install(), { kind: "state" });

  assert.equal(
    h.calls.filter((command) => command === "updater_delta_install").length,
    1,
  );

  await h.handle.close();
});

test("missing helper does not automatically start a full download", async () => {
  const h = harness({ available: false });

  await assert.rejects(h.handle.linuxDelta.download(() => {}), {
    kind: "unavailable",
  });

  assert.equal(h.calls.includes("full-download"), false);

  // Explicitly choose the full-download route.
  await h.handle.download(() => {});
  await h.handle.install();

  assert.equal(h.calls.includes("full-download"), true);
  assert.equal(h.calls.includes("full-install"), true);
  assert.equal(h.calls.includes("updater_delta_install"), false);

  await h.handle.close();
});

test("verification failure cannot proceed to installation", async () => {
  const h = harness({
    downloadError: {
      kind: "verification",
      message: "Test signature mismatch",
    },
  });

  await assert.rejects(h.handle.linuxDelta.download(() => {}), {
    kind: "verification",
  });

  await assert.rejects(h.handle.install(), { kind: "state" });

  assert.equal(h.calls.includes("updater_delta_install"), false);
  assert.equal(h.calls.includes("full-download"), false);
  assert.ok(h.closed.includes(42));

  await h.handle.close();
});

test(
  "cancellation during session creation closes the late session",
  { timeout: 5000 },
  async () => {
    const createStarted = deferred();
    const createGate = deferred();
    const h = harness({ createStarted, createGate });

    const downloading = h.handle.linuxDelta.download(() => {});
    const rejected = assert.rejects(downloading, { kind: "cancelled" });

    await createStarted.promise;

    const cancelling = h.handle.linuxDelta.cancel();
    createGate.resolve();

    await Promise.all([cancelling, rejected]);

    assert.ok(h.closed.includes(42));
    assert.equal(h.calls.includes("updater_delta_download"), false);
    assert.equal(h.calls.includes("full-download"), false);

    await assert.rejects(h.handle.install(), { kind: "state" });
    await h.handle.close();
  },
);

test(
  "active download cancellation allows a fresh successful retry",
  { timeout: 5000 },
  async () => {
    const downloadStarted = deferred();
    const h = harness({
      pauseFirstDownload: true,
      downloadStarted,
    });

    const downloading = h.handle.linuxDelta.download(() => {});
    const rejected = assert.rejects(downloading, { kind: "cancelled" });

    await downloadStarted.promise;
    await h.handle.linuxDelta.cancel();
    await rejected;

    assert.ok(h.closed.includes(42));
    assert.equal(h.calls.includes("full-download"), false);

    await assert.rejects(h.handle.install(), { kind: "state" });

    const phases = [];
    await h.handle.linuxDelta.download((phase) => phases.push(phase));

    assert.deepEqual(phases, ["reconstructing", "verifying", "ready"]);
    assert.equal(
      h.calls.filter((command) => command === "updater_delta_create").length,
      2,
    );

    await h.handle.install();

    assert.equal(h.handle.linuxDelta.isInstalled(), true);
    assert.equal(
      h.calls.filter((command) => command === "updater_delta_install").length,
      1,
    );
    assert.equal(h.calls.includes("full-download"), false);

    await h.handle.close();
  },
);
