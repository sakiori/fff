import { after, before, describe, it } from "node:test";
import { strict as assert } from "node:assert";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
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
      watch: finder.watch(() => {}),
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
