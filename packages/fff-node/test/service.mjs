import { after, before, describe, it } from "node:test";
import { strict as assert } from "node:assert";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { ErrorCode, FileFinder, nativeInstanceCount, unpackedAsarPath } from "../dist/index.js";
import { exitDeliveryWorkerForTest } from "../dist/watch-delivery.js";
import { disposableNamespace, removeStateRoot } from "./support.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const namespace = disposableNamespace();
const finders = [];

function tree(name, files) {
  const dir = join(namespace.root, name);
  for (const [path, body] of Object.entries(files)) {
    mkdirSync(dirname(join(dir, path)), { recursive: true });
    writeFileSync(join(dir, path), body);
  }
  return dir;
}

async function open(basePath, options = {}) {
  const created = await FileFinder.create({ basePath, ...namespace.options, ...options });
  assert.ok(created.ok, `create failed: ${!created.ok ? created.error : ""}`);
  finders.push(created.value);
  const ready = await created.value.waitForIndexReady(30_000);
  assert.ok(ready.ok && ready.value, `index not ready: ${JSON.stringify(ready)}`);
  return created.value;
}

// Collects the events of a subscription; `until` resolves with every event from index `from`
// up to the first there that `wanted` accepts, and fails the test when none arrives in time.
function collector() {
  const events = [];
  let waiter = null;
  const check = () => {
    if (!waiter) return;
    const index = events.findIndex((event, at) => at >= waiter.from && waiter.wanted(event));
    if (index >= 0) {
      const { resolve, from } = waiter;
      waiter = null;
      resolve(events.slice(from, index + 1));
    }
  };
  return {
    events,
    callback: (batch) => {
      events.push(...batch);
      check();
    },
    until(wanted, from = 0) {
      return new Promise((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error(`no matching event; received ${JSON.stringify(events)}`)),
          30_000,
        );
        waiter = {
          wanted,
          from,
          resolve: (seen) => {
            clearTimeout(timer);
            resolve(seen);
          },
        };
        check();
      });
    },
  };
}

// Events come in order, and a batch's file events go out before its refs events, so once the
// second of two new sentinel files is heard, every change before the first was.
async function fence(collected, dir, name) {
  for (const round of [1, 2]) {
    const sentinel = join(dir, `${name}-${round}.txt`);
    const from = collected.events.length;
    writeFileSync(sentinel, "sentinel\n");
    await collected.until((event) => event.path === sentinel, from);
  }
}

const paths = (result) => {
  assert.ok(result.ok, `search failed: ${!result.ok ? result.error : ""}`);
  return result.value.items.map((entry) => (entry.item ?? entry).relativePath);
};

describe("FileFinder over the shared service", { concurrency: 1 }, () => {
  after(async () => {
    for (const finder of finders) await finder.destroy();
    namespace.cleanup();
    removeStateRoot();
  });

  it("shares one index between FileFinders of two processes", async () => {
    const dir = tree("shared", { "a.txt": "a\n", "src/b.rs": "fn b() {}\n" });
    const finder = await open(dir);
    const child = spawn(
      process.execPath,
      [join(__dirname, "hold-finder.mjs"), dir, join(namespace.root, "child.log")],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["pipe", "pipe", "inherit"],
      },
    );
    try {
      const [line] = await once(child.stdout, "data");
      assert.equal(String(line).trim(), "ready");
      const [service, ...others] = namespace.status().services;
      assert.equal(others.length, 0, "one service runs");
      assert.equal(service.index_count, 1, JSON.stringify(service));
      const programs = service.sessions.filter((s) => s.kind === "program");
      assert.equal(programs.length, 2);
      assert.ok(programs.every((s) => s.index === service.indexes[0].id));
      assert.deepEqual(paths(await finder.fileSearch("", { excludeFrecency: true })), [
        "a.txt",
        "src/b.rs",
      ]);
    } finally {
      child.stdin.end();
      await once(child, "exit");
    }
  });

  it("answers calls in a program Node runs in watch mode", async () => {
    // `node --watch` makes every worker post the modules it requires to its
    // parent port, beside the package's own replies.
    const dir = tree("watched", { "a.txt": "a\n" });
    const child = spawn(
      process.execPath,
      ["--watch", join(__dirname, "hold-finder.mjs"), dir, join(namespace.root, "watched.log")],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["pipe", "pipe", "inherit"],
      },
    );
    const lines = createInterface({ input: child.stdout });
    const next = async () => String((await once(lines, "line"))[0]).trim();
    try {
      assert.equal(await next(), "ready");
      child.stdin.end();
      assert.match(await next(), /^Completed running/);
    } finally {
      child.kill();
      await once(child, "exit");
    }
  });

  it("answers calls in a program whose flags make code an ES module", { timeout: 120_000 }, async () => {
    // Workers must not take the host's flags: `--input-type=module` would read
    // their CommonJS source as an ES module.
    const dir = tree("module-input", { "a.txt": "a\n" });
    const helper = pathToFileURL(join(__dirname, "hold-finder.mjs")).href;
    const child = spawn(
      process.execPath,
      [
        "--input-type=module",
        "-e",
        `process.argv.splice(1, 0, "-e"); await import(${JSON.stringify(helper)});`,
        dir,
        join(namespace.root, "module-input.log"),
      ],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["pipe", "pipe", "inherit"],
      },
    );
    const lines = createInterface({ input: child.stdout });
    try {
      assert.equal(await lineOrExit(child, lines), "ready");
    } finally {
      child.stdin.end();
      await once(child, "exit");
    }
  });

  it("delivers watch events in a program Node runs in watch mode", async () => {
    const dir = tree("watched-events", { "a.txt": "a\n" });
    const child = spawn(
      process.execPath,
      [
        "--watch",
        join(__dirname, "watch-one-change.mjs"),
        dir,
        join(dir, "b.txt"),
        join(namespace.root, "watched-events.log"),
      ],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["ignore", "pipe", "inherit"],
      },
    );
    const lines = createInterface({ input: child.stdout });
    const next = async () => String((await once(lines, "line"))[0]).trim();
    try {
      assert.equal(await next(), "watching");
      writeFileSync(join(dir, "b.txt"), "b\n");
      assert.equal(await next(), "heard");
      assert.match(await next(), /^Completed running/);
    } finally {
      child.kill();
      await once(child, "exit");
    }
  });

  it("delivers watch events in a program whose flags make code an ES module", { timeout: 120_000 }, async () => {
    const dir = tree("module-input-events", { "a.txt": "a\n" });
    const helper = pathToFileURL(join(__dirname, "watch-one-change.mjs")).href;
    const child = spawn(
      process.execPath,
      [
        "--input-type=module",
        "-e",
        `process.argv.splice(1, 0, "-e"); await import(${JSON.stringify(helper)});`,
        dir,
        join(dir, "b.txt"),
        join(namespace.root, "module-input-events.log"),
      ],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["ignore", "pipe", "inherit"],
      },
    );
    const lines = createInterface({ input: child.stdout });
    const next = () => lineOrExit(child, lines);
    try {
      assert.equal(await next(), "watching");
      writeFileSync(join(dir, "b.txt"), "b\n");
      assert.equal(await next(), "heard");
    } finally {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill();
        await once(child, "exit");
      }
    }
  });

  it("ranks without frecency when a call excludes it", async () => {
    const files = Object.fromEntries(
      ["a.txt", "b.txt", "c.txt", "zz-dirty.txt"].map((name) => [name, "needle\n"]),
    );
    const dir = tree("ranking", files);
    const git = (...args) => execFileSync("git", ["-C", dir, ...args], { stdio: "ignore" });
    git("init", "-q");
    git("add", ".");
    git("-c", "user.name=fff", "-c", "user.email=fff@example.invalid", "commit", "-qm", "init");
    // The modified file is the oldest, so only its frecency ranks it first.
    writeFileSync(join(dir, "zz-dirty.txt"), "needle\nchanged\n");
    const old = new Date(Date.now() - 120_000);
    utimesSync(join(dir, "zz-dirty.txt"), old, old);
    const finder = await open(dir, { frecencyDbPath: join(namespace.root, "frecency-db") });

    assert.equal(paths(await finder.fileSearch(""))[0], "zz-dirty.txt");
    assert.deepEqual(paths(await finder.fileSearch("", { excludeFrecency: true })), [
      "a.txt",
      "b.txt",
      "c.txt",
      "zz-dirty.txt",
    ]);
    assert.equal(paths(await finder.mixedSearch("", { excludeFrecency: true })).at(-1), "zz-dirty.txt");
    assert.equal(paths(await finder.grep("needle"))[0], "zz-dirty.txt");
    assert.equal(paths(await finder.grep("needle", { excludeFrecency: true })).at(-1), "zz-dirty.txt");
    const multi = await finder.multiGrep({ patterns: ["needle"], excludeFrecency: true });
    assert.equal(paths(multi).at(-1), "zz-dirty.txt");
  });

  it("pages grep results with cursors", async () => {
    const files = Object.fromEntries(
      Array.from({ length: 7 }, (_, n) => [`page${n}.txt`, "PageNeedle\n"]),
    );
    const finder = await open(tree("paging", files));
    const seen = [];
    let cursor = null;
    let pages = 0;
    do {
      const page = await finder.grep("PageNeedle", { pageSize: 2, maxMatchesPerFile: 1, cursor });
      seen.push(...paths(page));
      cursor = page.value.nextCursor;
      pages++;
    } while (cursor !== null && pages < 10);
    assert.ok(pages >= 4, `expected several pages, got ${pages}`);
    assert.deepEqual([...seen].sort(), Object.keys(files).sort());
    const forged = { __brand: "GrepCursor", _offset: 2 };
    const refused = await finder.grep("PageNeedle", { cursor: forged });
    assert.equal(refused.ok, false);
    assert.equal(refused.code, ErrorCode.invalidRequest);
    assert.match(refused.error, /not returned by a FileFinder grep/);
  });

  it("reports a service that went away, then reconnects on the next call", async () => {
    const own = disposableNamespace();
    try {
      const created = await FileFinder.create({
        basePath: tree("gone", { "a.txt": "a\n" }),
        ...own.options,
      });
      assert.ok(created.ok, `create failed: ${!created.ok ? created.error : ""}`);
      const finder = created.value;
      const [service] = own.status().services;
      // The service of this test's own namespace.
      process.kill(service.pid, "SIGKILL");
      const waited = await finder.waitForScan(5_000);
      assert.equal(waited.ok, false, JSON.stringify(waited));
      assert.equal(waited.code, ErrorCode.lostService, waited.error);
      // The next call opens a new session, starting a new service.
      const ready = await finder.waitForIndexReady(30_000);
      assert.ok(ready.ok && ready.value, JSON.stringify(ready));
      assert.deepEqual(paths(await finder.fileSearch("")), ["a.txt"]);
      const [restarted] = own.status().services;
      assert.notEqual(restarted.pid, service.pid);
      await finder.destroy();
    } finally {
      own.cleanup();
    }
  });

  it("answers overlapping calls on one instance and across instances", async () => {
    const files = Object.fromEntries(
      Array.from({ length: 12 }, (_, n) => [`f${n}.txt`, "OverlapNeedle\n"]),
    );
    const first = await open(tree("overlap-a", files));
    const second = await open(tree("overlap-b", { "x.rs": "fn x() {}\n", "y.rs": "fn y() {}\n" }));
    const listed = paths(await first.fileSearch("", { excludeFrecency: true }));
    assert.equal(listed.length, 12);
    const calls = [];
    for (let round = 0; round < 8; round++) {
      calls.push(
        first.fileSearch("", { excludeFrecency: true }).then((r) => ["list", paths(r)]),
        first
          .grep("OverlapNeedle", { maxMatchesPerFile: 1, pageSize: 100 })
          .then((r) => ["grep", paths(r).sort()]),
        first.getScanProgress().then((r) => ["progress", r.ok && r.value.scannedFilesCount]),
        second.glob("**/*.rs").then((r) => ["glob", paths(r).sort()]),
        second.isScanning().then((scanning) => ["scanning", scanning]),
      );
    }
    for (const [kind, value] of await Promise.all(calls)) {
      const expected = {
        list: listed,
        grep: [...listed].sort(),
        progress: 12,
        glob: ["x.rs", "y.rs"],
        scanning: false,
      }[kind];
      assert.deepEqual(value, expected, kind);
    }
  });

  it("finishes calls made before destroy and refuses calls after it", async () => {
    const created = await FileFinder.create({
      basePath: tree("destroying", { "a.txt": "a\n" }),
      ...namespace.options,
    });
    assert.ok(created.ok, `create failed: ${!created.ok ? created.error : ""}`);
    const finder = created.value;
    const before = [finder.getScanProgress(), finder.fileSearch(""), finder.isScanning()];
    const destroyed = finder.destroy();
    assert.equal(finder.isDestroyed, true);
    const after = await finder.fileSearch("");
    const [progress, search, scanning] = await Promise.all(before);
    assert.ok(progress.ok && search.ok, JSON.stringify([progress, search]));
    assert.equal(typeof scanning, "boolean");
    await destroyed;
    assert.equal(after.ok, false);
    assert.match(after.error, /destroyed/);
  });

  it("bounds calls to an unresponsive service without blocking the event loop", async () => {
    const own = disposableNamespace();
    const basePath = tree("stopped", { "a.txt": "a\n" });
    let stopped = null;
    try {
      const created = await FileFinder.create({ basePath, ...own.options, timeoutMs: 1_000 });
      assert.ok(created.ok, `create failed: ${!created.ok ? created.error : ""}`);
      const finder = created.value;
      const ready = await finder.waitForIndexReady(30_000);
      assert.ok(ready.ok && ready.value, JSON.stringify(ready));
      // The service of this test's own namespace, which stays alive but stops answering.
      [{ pid: stopped }] = own.status().services;
      process.kill(stopped, "SIGSTOP");

      let ticks = 0;
      let last = performance.now();
      let longestGap = 0;
      const timer = setInterval(() => {
        const now = performance.now();
        longestGap = Math.max(longestGap, now - last);
        last = now;
        ticks++;
      }, 10);
      let results;
      let other;
      try {
        results = await Promise.all([
          finder.getScanProgress(),
          finder.fileSearch("a"),
          finder.grep("a"),
        ]);
        other = await FileFinder.create({ basePath, ...own.options, timeoutMs: 1_000 });
      } finally {
        clearInterval(timer);
      }
      const [progress, ...queued] = results;
      assert.equal(progress.ok, false);
      assert.equal(progress.code, ErrorCode.lostService);
      assert.match(progress.error, /did not answer within/);
      // Calls queued behind the unanswered one fail with it instead of waiting again.
      for (const result of queued) assert.deepEqual(result, progress);
      assert.equal(other.ok, false);
      assert.equal(other.code, ErrorCode.lostService);
      assert.match(other.error, /no shared service became available within/);
      // Two waits of a second each; a blocked loop would have stalled the timer throughout.
      assert.ok(ticks > 20, `the timer fired ${ticks} times`);
      assert.ok(longestGap < 500, `the event loop stalled for ${longestGap} ms`);

      process.kill(stopped, "SIGCONT");
      stopped = null;
      // The next call opens a new session on the service that answers again; the index the
      // dropped session held was released with it, so the new one scans first.
      const rescanned = await finder.waitForIndexReady(30_000);
      assert.ok(rescanned.ok && rescanned.value, JSON.stringify(rescanned));
      assert.deepEqual(paths(await finder.fileSearch("")), ["a.txt"]);
      await finder.destroy();
    } finally {
      if (stopped !== null) process.kill(stopped, "SIGCONT");
      own.cleanup();
    }
  });

  it("keeps the libuv pool free while more instances than workers wait on a stopped service", async () => {
    const own = disposableNamespace();
    let stopped = null;
    try {
      const finders = [];
      for (let n = 0; n < 6; n++) {
        const basePath = tree(`stalled-${n}`, { "a.txt": "a\n" });
        const created = await FileFinder.create({ basePath, ...own.options, timeoutMs: 2_000 });
        assert.ok(created.ok, `create failed: ${!created.ok ? created.error : ""}`);
        const ready = await created.value.waitForIndexReady(30_000);
        assert.ok(ready.ok && ready.value, JSON.stringify(ready));
        finders.push(created.value);
      }
      const live = nativeInstanceCount();
      // The service of this test's own namespace, which stays alive but stops answering.
      [{ pid: stopped }] = own.status().services;
      process.kill(stopped, "SIGSTOP");

      let last = performance.now();
      let longestGap = 0;
      const timer = setInterval(() => {
        const now = performance.now();
        longestGap = Math.max(longestGap, now - last);
        last = now;
      }, 10);
      let reads;
      let results;
      try {
        const calls = finders.map((finder) => finder.fileSearch("a"));
        // A destroy queued behind a call that loses the service still frees the instance.
        const destroyed = finders[0].destroy();
        reads = [];
        for (let n = 0; n < 5; n++) {
          const started = performance.now();
          await readFile(join(namespace.root, "stalled-0", "a.txt"));
          reads.push(performance.now() - started);
        }
        results = await Promise.all(calls);
        await destroyed;
      } finally {
        clearInterval(timer);
      }
      for (const result of results) {
        assert.equal(result.ok, false);
        assert.equal(result.code, ErrorCode.lostService);
        assert.match(result.error, /did not answer within/);
      }
      assert.equal(nativeInstanceCount(), live - 1);
      // The workers hold the stalled calls; file reads on the libuv pool never wait for them.
      assert.ok(Math.max(...reads) < 250, `file reads took ${reads.map(Math.round)} ms`);
      assert.ok(longestGap < 500, `the event loop stalled for ${longestGap} ms`);

      process.kill(stopped, "SIGCONT");
      stopped = null;
      for (const finder of finders.slice(1)) await finder.destroy();
      assert.equal(nativeInstanceCount(), live - finders.length);
    } finally {
      if (stopped !== null) process.kill(stopped, "SIGCONT");
      own.cleanup();
    }
  });

  it("reports methods without a service equivalent as unavailable", async () => {
    const finder = await open(tree("unavailable", { "a.txt": "a\n" }));
    const results = {
      trackQuery: finder.trackQuery("a", "a.txt"),
      getHistoricalQuery: finder.getHistoricalQuery(0),
      refreshGitStatus: finder.refreshGitStatus(),
    };
    for (const [method, result] of Object.entries(results)) {
      assert.equal(result.ok, false, method);
      assert.equal(result.code, ErrorCode.unavailable, method);
      assert.match(result.error, new RegExp(`${method} is unavailable over the shared service`));
    }
    const other = await finder.reindex(namespace.root);
    assert.equal(other.ok, false);
    assert.equal(other.code, ErrorCode.unavailable);
    assert.match(other.error, /unavailable over the shared service/);
    const history = await FileFinder.create({
      basePath: namespace.root,
      ...namespace.options,
      historyDbPath: join(namespace.root, "history"),
    });
    assert.equal(history.ok, false);
    assert.equal(history.code, ErrorCode.unavailable);
    assert.match(history.error, /historyDbPath is unavailable over the shared service/);
  });

  it("fails to create when the service cannot start", async () => {
    const blocker = join(namespace.root, "blocker");
    writeFileSync(blocker, "");
    const created = await FileFinder.create({
      basePath: namespace.root,
      serviceDir: join(blocker, "svc"),
      logFilePath: namespace.log,
    });
    assert.equal(created.ok, false);
    assert.equal(created.code, ErrorCode.failed, created.error);
    assert.match(created.error, /shared service/);
  });

  it("reports a cursor of an index the session left as stale", async () => {
    const parent = tree("stale", {
      "sub/a.txt": "StaleNeedle\n",
      "sub/b.txt": "StaleNeedle\n",
      "c.txt": "c\n",
    });
    const child = await open(join(parent, "sub"));
    const options = { pageSize: 1, maxMatchesPerFile: 1 };
    const first = await child.grep("StaleNeedle", options);
    assert.ok(first.ok && first.value.nextCursor, JSON.stringify(first));
    // A session for the parent builds a broader index, to which the child's session moves.
    await open(parent);
    const deadline = Date.now() + 30_000;
    let page;
    do {
      page = await child.grep("StaleNeedle", { ...options, cursor: first.value.nextCursor });
      if (page.ok) await new Promise((resolve) => setTimeout(resolve, 50));
    } while (page.ok && Date.now() < deadline);
    assert.equal(page.ok, false, "the session never moved to the broader index");
    assert.equal(page.code, ErrorCode.staleCursor, page.error);
  });

  it("reports a directory the service refuses as an unavailable scope", async () => {
    const home = await FileFinder.create({ basePath: homedir(), ...namespace.options });
    assert.equal(home.ok, false);
    assert.equal(home.code, ErrorCode.scopeUnavailable, home.error);
  });

  it("reports an empty glob the service refuses as an invalid request", async () => {
    const finder = await open(tree("invalid", { "a.txt": "a\n" }));
    const glob = await finder.glob("");
    assert.equal(glob.ok, false);
    assert.equal(glob.code, ErrorCode.invalidRequest, glob.error);
    const timeout = await FileFinder.create({ basePath: namespace.root, timeoutMs: -1 });
    assert.equal(timeout.code, ErrorCode.invalidRequest);
  });

  it("reports a platform without native files, and missing native files", () => {
    const dist = new URL("../dist/index.js", import.meta.url).href;
    const probe = (url, prelude = "") =>
      JSON.parse(
        execFileSync(
          process.execPath,
          [
            "--input-type=module",
            "-e",
            `${prelude}
            const { FileFinder } = await import(${JSON.stringify(url)});
            const created = await FileFinder.create({ basePath: ${JSON.stringify(namespace.root)} });
            console.log(JSON.stringify(created));`,
          ],
          { encoding: "utf8" },
        ),
      );
    const unsupported = probe(dist, 'Object.defineProperty(process, "arch", { value: "mips" });');
    assert.equal(unsupported.code, ErrorCode.unsupportedPlatform, unsupported.error);

    // A copy of the package outside the workspace, with its dependencies but no platform package.
    const copy = join(namespace.root, "package-copy");
    mkdirSync(copy);
    cpSync(join(__dirname, "..", "package.json"), join(copy, "package.json"));
    cpSync(join(__dirname, "..", "dist"), join(copy, "dist"), { recursive: true });
    symlinkSync(join(__dirname, "..", "..", "node_modules"), join(copy, "node_modules"));
    const missing = probe(pathToFileURL(join(copy, "dist", "index.js")).href);
    assert.equal(missing.code, ErrorCode.libraryUnavailable, missing.error);
  });

  it("applies changes the program reports before its next search", async () => {
    const dir = tree("reported", { "keep.txt": "keep\n" });
    // Without a watcher only the report can bring the changes into the index.
    const finder = await open(dir, { disableWatch: true });
    const listed = async () => paths(await finder.fileSearch("", { excludeFrecency: true }));

    writeFileSync(join(dir, "fresh.txt"), "FreshNeedle\n");
    mkdirSync(join(dir, "node_modules"));
    writeFileSync(join(dir, "node_modules", "dep.js"), "FreshNeedle\n");
    const reported = await finder.notifyChanged(["fresh.txt", "node_modules/dep.js"]);
    assert.ok(reported.ok, JSON.stringify(reported));
    assert.deepEqual(await listed(), ["fresh.txt", "keep.txt"]);
    assert.deepEqual(paths(await finder.grep("FreshNeedle")), ["fresh.txt"]);

    // A report spelled in another case updates the entry where the filesystem ignores case.
    if (existsSync(join(dir, "KEEP.TXT"))) {
      assert.ok((await finder.notifyChanged(["KEEP.TXT", "Fresh.Txt"])).ok);
      assert.deepEqual(await listed(), ["fresh.txt", "keep.txt"]);
    }

    rmSync(join(dir, "fresh.txt"));
    const removed = await finder.notifyChanged([join(dir, "fresh.txt")]);
    assert.ok(removed.ok, JSON.stringify(removed));
    assert.deepEqual(await listed(), ["keep.txt"]);

    for (const outside of ["../elsewhere.txt", namespace.root, join(dir, "..", "x.txt")]) {
      const refused = await finder.notifyChanged(["keep.txt", outside]);
      assert.equal(refused.ok, false, outside);
      assert.equal(refused.code, ErrorCode.outsideScope, refused.error);
    }
  });

  it("watches the directory once subscribed and delivers batches on the event loop", async () => {
    const dir = realpathSync(tree("watched", { "a.txt": "a\n", "src/b.rs": "fn b() {}\n" }));
    const finder = await open(dir);
    const all = collector();
    const sources = collector();
    const watched = await finder.watch(all.callback);
    assert.ok(watched.ok, JSON.stringify(watched));
    const scoped = await finder.watch("src/**", sources.callback, { ignore: ["src/gen"] });
    assert.ok(scoped.ok, JSON.stringify(scoped));

    writeFileSync(join(dir, "a.txt"), "changed\n");
    await all.until((event) => event.path === join(dir, "a.txt") && event.kind === "modified");
    mkdirSync(join(dir, "src", "gen"));
    writeFileSync(join(dir, "src", "gen", "out.rs"), "\n");
    writeFileSync(join(dir, "src", "c.rs"), "fn c() {}\n");
    const seen = await sources.until((event) => event.path === join(dir, "src", "c.rs"));
    assert.ok(
      seen.every((event) => !event.path.includes("gen") && !event.path.endsWith("a.txt")),
      JSON.stringify(seen),
    );

    // Once unsubscribed, a callback hears nothing; the other subscription still does.
    watched.value();
    const before = all.events.length;
    writeFileSync(join(dir, "src", "d.rs"), "fn d() {}\n");
    await sources.until((event) => event.path === join(dir, "src", "d.rs"));
    assert.equal(all.events.length, before, JSON.stringify(all.events.slice(before)));
    scoped.value();
  });

  it("reports repository state to subscriptions that ask for it", async () => {
    const dir = realpathSync(tree("repository-state", { "a.txt": "a\n", "src/b.txt": "b\n" }));
    execFileSync("git", ["-C", dir, "init", "-q"]);
    const finder = await open(dir);
    const state = collector();
    const watched = await finder.watch(state.callback, { repository: true });
    assert.ok(watched.ok, JSON.stringify(watched));
    // A directory below the work tree hears the repository that contains it.
    const below = await open(join(dir, "src"));
    const contained = collector();
    const belowWatched = await below.watch(contained.callback, { repository: true });
    assert.ok(belowWatched.ok, JSON.stringify(belowWatched));
    execFileSync("git", ["-C", dir, "add", "a.txt"]);
    const seen = await state.until((event) => event.kind === "repository");
    assert.equal(seen.at(-1).path, dir);
    assert.ok(seen.every((event) => !event.path.includes(".git")), JSON.stringify(seen));
    const heard = await contained.until((event) => event.kind === "repository");
    assert.equal(heard.at(-1).path, dir);
    watched.value();
    belowWatched.value();
  });

  it("reports ref changes to subscriptions that ask for them", async () => {
    const dir = realpathSync(tree("ref-changes", { "a.txt": "a\n" }));
    const git = (...args) =>
      execFileSync("git", ["-C", dir, "-c", "user.name=fff", "-c", "user.email=fff@example.invalid", ...args]);
    git("init", "-q");
    git("add", "a.txt");
    git("commit", "-q", "-m", "init");
    const finder = await open(dir);
    const refs = collector();
    const watched = await finder.watch(refs.callback, { refs: true });
    assert.ok(watched.ok, JSON.stringify(watched));
    const plain = collector();
    const plainWatched = await finder.watch(plain.callback, { repository: true });
    assert.ok(plainWatched.ok, JSON.stringify(plainWatched));

    git("branch", "feature");
    const seen = await refs.until((event) => event.kind === "refs");
    assert.equal(seen.at(-1).path, dir);
    await fence(refs, dir, "after-branch");
    const beforeTag = refs.events.length;
    git("tag", "v1");
    await refs.until((event) => event.kind === "refs", beforeTag);
    await fence(refs, dir, "after-tag");
    const beforeSwitch = refs.events.length;
    git("switch", "-q", "feature");
    await refs.until((event) => event.kind === "refs" && event.path === dir, beforeSwitch);
    // Changes come in order, so the sentinel's event follows any of the refs'.
    writeFileSync(join(dir, "sentinel.txt"), "after the refs\n");
    const heard = await plain.until((event) => event.path === join(dir, "sentinel.txt"));
    assert.ok(heard.every((event) => event.kind !== "refs"), JSON.stringify(heard));
    watched.value();
    plainWatched.value();
  });

  it("hears the HEAD of a worktree no index follows as refs of its own work tree", async () => {
    const root = realpathSync(tree("watch-other-head", { "main/a.txt": "a\n", "other/a.txt": "a\n" }));
    const [main, other] = [join(root, "main"), join(root, "other")];
    const git = (dir, ...args) =>
      execFileSync("git", ["-C", dir, "-c", "user.name=fff", "-c", "user.email=fff@example.invalid", ...args]);
    for (const dir of [main, other]) {
      git(dir, "init", "-q", "-b", "main");
      git(dir, "add", "a.txt");
      git(dir, "commit", "-q", "-m", "init");
    }
    git(main, "branch", "side");
    const [linked, beside] = [join(root, "linked"), join(root, "beside")];
    git(main, "worktree", "add", "-q", "-b", "linked", linked);
    git(main, "worktree", "add", "-q", "--detach", beside);
    // Refs of another repository outside the index come through the same watcher stream as
    // those of the main repository, in order, and name a work tree of their own.
    const nested = join(linked, "nested");
    git(other, "worktree", "add", "-q", "-b", "nested", nested);
    const finder = await open(linked);
    const refs = collector();
    const watched = await finder.watch(refs.callback, { refs: true });
    assert.ok(watched.ok, JSON.stringify(watched));
    // The second of two proves the batch of the first was handed over in full.
    let fences = 0;
    const fenced = async () => {
      for (const _ of [1, 2]) {
        const from = refs.events.length;
        git(other, "branch", `fence-${++fences}`);
        await refs.until((event) => event.kind === "refs" && event.path === nested, from);
      }
    };
    await fenced();

    const from = refs.events.length;
    git(beside, "switch", "-q", "side");
    const seen = await refs.until((event) => event.path === linked, from);
    assert.deepEqual(seen.at(-1), { path: linked, kind: "refs" });
    await fenced();

    // Its index stays its own.
    const before = refs.events.length;
    writeFileSync(join(beside, "b.txt"), "b\n");
    git(beside, "add", "b.txt");
    await fenced();
    const later = refs.events.slice(before).filter((event) => event.path === linked);
    assert.deepEqual(later, []);
    watched.value();
  });

  it("tells a main checkout of a switch in each of its worktrees, as a server relies on", { timeout: 180_000 }, async () => {
    const root = realpathSync(tree("watch-all-heads", { "main/a.txt": "a\n", "main/.gitignore": ".worktrees/\n" }));
    const main = join(root, "main");
    const git = (dir, ...args) =>
      execFileSync("git", ["-C", dir, "-c", "user.name=fff", "-c", "user.email=fff@example.invalid", ...args]);
    git(main, "init", "-q", "-b", "main");
    git(main, "add", ".");
    git(main, "commit", "-q", "-m", "one");
    git(main, "tag", "one");
    git(main, "commit", "-q", "--allow-empty", "-m", "two");
    git(main, "tag", "two");
    // Ignored below the checkout, beside it, and open below it, which is named to its subscribers.
    const layouts = [
      [join(main, ".worktrees/ignored"), main],
      [join(root, "beside"), main],
      [join(main, "open"), join(main, "open")],
    ];
    for (const [worktree] of layouts) git(main, "worktree", "add", "-q", "--detach", worktree);
    const options = { repository: true, refs: true };
    const finder = await open(main);
    const heard = collector();
    const watched = await finder.watch(heard.callback, options);
    assert.ok(watched.ok, JSON.stringify(watched));
    let fences = 0;
    const fenced = () => fence(heard, main, `heads-${++fences}`);
    const switched = async ([worktree, named], tag) => {
      await fenced();
      const from = heard.events.length;
      git(worktree, "switch", "-q", "--detach", tag);
      await heard.until((event) => event.kind === "refs" && event.path === named, from);
    };
    for (const layout of layouts) {
      await switched(layout, "one");
      // An index of the worktree's own, as a view of the worktree holds, changes nothing here.
      const other = await open(layout[0]);
      const held = await other.watch(() => {}, options);
      assert.ok(held.ok, JSON.stringify(held));
      await switched(layout, "two");
      await switched(layout, "one");
      held.value();
      await switched(layout, "two");
    }
    watched.value();
  });

  it("replaces what a callback falling behind could not take with one rescan", async () => {
    const files = Object.fromEntries(Array.from({ length: 1500 }, (_, n) => [`f${n}.rs`, ""]));
    const dir = realpathSync(tree("watch-behind", files));
    const finder = await open(dir);
    const heard = collector();
    let blocked = false;
    let firstBatch = 0;
    const watched = await finder.watch((batch) => {
      if (!blocked) {
        blocked = true;
        firstBatch = batch.length;
        // Holds the event loop until the service indexed every change, so more than the
        // package holds for one callback arrive meanwhile.
        for (let n = 0; n < 1500; n++) writeFileSync(join(dir, `f${n}.rs`), "changed");
        execFileSync(
          process.execPath,
          [join(__dirname, "wait-for-size.mjs"), dir, "f1499.rs", "7", join(namespace.root, "wait.log")],
          { env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir }, stdio: "inherit" },
        );
      }
      heard.callback(batch);
    });
    assert.ok(watched.ok, JSON.stringify(watched));
    writeFileSync(join(dir, "first.rs"), "first");
    const seen = await heard.until((event) => event.kind === "rescan" && event.path === dir);
    // The batch that held the event loop, at most the limit after it, then the rescan.
    assert.ok(seen.length - firstBatch - 1 <= 1024, `${seen.length - firstBatch - 1} events`);
    writeFileSync(join(dir, "after.rs"), "after");
    await heard.until((event) => event.path === join(dir, "after.rs"));
    watched.value();
  });

  it("restarts delivery with a rescan when its worker exits", async () => {
    const dir = realpathSync(tree("watch-restart", { "a.txt": "a\n" }));
    const finder = await open(dir);
    const heard = collector();
    const watched = await finder.watch(heard.callback);
    assert.ok(watched.ok, JSON.stringify(watched));
    exitDeliveryWorkerForTest();
    // The rescan follows once a new worker took over the instance's callback.
    await heard.until((event) => event.kind === "rescan" && event.path === dir);
    writeFileSync(join(dir, "after.txt"), "after");
    await heard.until((event) => event.path === join(dir, "after.txt"));
    watched.value();
  });

  it("ends a subscription whose directory is removed with a failed event", async () => {
    const dir = realpathSync(tree("watch-removed", { "a.txt": "a\n" }));
    const finder = await open(dir);
    const heard = collector();
    const watched = await finder.watch(heard.callback);
    assert.ok(watched.ok, JSON.stringify(watched));
    rmSync(dir, { recursive: true, force: true });
    const [failed] = (await heard.until((event) => event.kind === "failed")).slice(-1);
    assert.equal(failed.path, dir);
    assert.equal(failed.code, ErrorCode.scopeUnavailable, failed.error);
    // The subscription already ended; unsubscribing changes nothing.
    watched.value();
  });

  it("subscribes without holding the native workers searches need", async () => {
    const stoppedNamespace = disposableNamespace();
    let stopped = null;
    try {
      const waiting = [];
      for (let n = 0; n < 6; n++) {
        const created = await FileFinder.create({
          basePath: tree(`watch-waiting-${n}`, { "a.txt": "a\n" }),
          ...stoppedNamespace.options,
          timeoutMs: 10_000,
        });
        assert.ok(created.ok, created.error);
        waiting.push(created.value);
      }
      [{ pid: stopped }] = stoppedNamespace.status().services;
      process.kill(stopped, "SIGSTOP");
      let settled = 0;
      const watches = waiting.map((finder) =>
        finder.watch(() => {}).then((result) => {
          settled++;
          return result;
        }),
      );
      // Six subscriptions wait on a service that does not answer; another instance
      // still opens and searches.
      const finder = await open(tree("watch-not-waiting", { "b.txt": "b\n" }));
      assert.deepEqual(paths(await finder.fileSearch("")), ["b.txt"]);
      assert.equal(settled, 0, "a subscription settled before the search");
      process.kill(stopped, "SIGCONT");
      stopped = null;
      for (const result of await Promise.all(watches)) {
        if (result.ok) result.value();
      }
      for (const each of waiting) await each.destroy();
    } finally {
      if (stopped !== null) process.kill(stopped, "SIGCONT");
      stoppedNamespace.cleanup();
    }
  });

  it("ends subscriptions with destroy, and refuses watch without watching", async () => {
    const dir = realpathSync(tree("watch-destroy", { "a.txt": "a\n" }));
    const created = await FileFinder.create({ basePath: dir, ...namespace.options });
    assert.ok(created.ok, created.error);
    const finder = created.value;
    const heard = collector();
    const watched = await finder.watch(heard.callback);
    assert.ok(watched.ok, JSON.stringify(watched));
    const watchSessions = () =>
      namespace
        .status()
        .services.flatMap((s) => s.sessions)
        .filter((s) => s.kind === "watch" && s.scope === dir);
    assert.equal(watchSessions().length, 1);
    await finder.destroy();
    // The service ends the session once it sees the connection close.
    const deadline = Date.now() + 15_000;
    while (watchSessions().length > 0) {
      assert.ok(Date.now() < deadline, "the subscription's session outlived the instance");
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.equal(heard.events.length, 0, JSON.stringify(heard.events));
    const after = await finder.watch(() => {});
    assert.equal(after.code, ErrorCode.destroyed);

    const snapshot = await open(tree("watch-disabled", { "a.txt": "a\n" }), { disableWatch: true });
    const refused = await snapshot.watch(() => {});
    assert.equal(refused.ok, false);
    assert.equal(refused.code, ErrorCode.unavailable, refused.error);
  });

  it("lets a process with a subscription exit", async () => {
    const dir = tree("watch-exit", { "a.txt": "a\n" });
    const child = spawn(
      process.execPath,
      [join(__dirname, "watch-and-return.mjs"), dir, join(namespace.root, "exit.log")],
      {
        env: { ...process.env, FFF_MCP_SERVICE_DIR: namespace.dir },
        stdio: ["ignore", "pipe", "inherit"],
      },
    );
    let output = "";
    child.stdout.on("data", (chunk) => {
      output += chunk;
    });
    const [code] = await once(child, "exit");
    assert.equal(code, 0);
    assert.equal(output.trim(), "watching");
  });

  it("loads through CommonJS require as the same module", () => {
    const required = createRequire(import.meta.url)("@sakiori/fff-node");
    assert.equal(required.FileFinder, FileFinder);
  });
});

describe("native files inside an Electron archive", () => {
  let root;
  before(() => {
    root = mkdtempSync(join(tmpdir(), "fff-asar-"));
    writeFileSync(join(root, "app.asar"), "");
    mkdirSync(join(root, "app.asar.unpacked", "native"), { recursive: true });
    writeFileSync(join(root, "app.asar.unpacked", "native", "libfff_c.so"), "");
  });
  after(() => rmSync(root, { recursive: true, force: true }));

  it("maps a file unpacked beside the archive", () => {
    assert.equal(
      unpackedAsarPath(join(root, "app.asar", "native", "libfff_c.so")),
      join(root, "app.asar.unpacked", "native", "libfff_c.so"),
    );
  });

  it("keeps a path that has no unpacked counterpart", () => {
    const inside = join(root, "app.asar", "native", "fff-mcp");
    assert.equal(unpackedAsarPath(inside), inside);
    const outside = join(root, "native", "libfff_c.so");
    assert.equal(unpackedAsarPath(outside), outside);
  });
});

// The next line the child prints, or a failure naming how it exited first.
async function lineOrExit(child, lines) {
  const exited = once(child, "exit").then(([code, signal]) => {
    throw new Error(`exited with ${signal ?? code} before printing a line`);
  });
  return String((await Promise.race([once(lines, "line"), exited]))[0]).trim();
}
