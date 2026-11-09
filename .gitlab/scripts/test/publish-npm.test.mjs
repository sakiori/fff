import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, beforeEach, describe, test } from "node:test";
import { distTag, main } from "../publish-npm.mjs";

const PLATFORMS = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-arm64-musl", "linux-x64-gnu", "linux-x64-musl"];
const PACKAGES = [...PLATFORMS.map((platform) => `@sakiori/fff-bin-${platform}`), "@sakiori/fff-node"];
const VERSION = "1.0.2";
const root = mkdtempSync(join(tmpdir(), "publish-npm-test-"));
const home = process.cwd();
after(() => {
  process.chdir(home);
  rmSync(root, { recursive: true, force: true });
});

describe("dist-tag", () => {
  test("latest only above the current latest", () => {
    assert.equal(distTag("1.0.0", null), "latest");
    assert.equal(distTag("1.0.0", "0.11.1-sakiori.20261105.1"), "latest");
    assert.equal(distTag("1.0.3", "1.0.3-rc.1"), "latest");
    assert.equal(distTag("1.10.0", "1.9.12"), "latest");
    assert.equal(distTag("1.0.2", "1.0.3"), "superseded");
    assert.equal(distTag("1.0.3", "1.0.3"), "superseded");
    assert.equal(distTag("2.0.0", "10.0.0"), "superseded");
  });
});

describe("publish", () => {
  let dir;
  let log;
  let integrity;

  beforeEach((t) => {
    dir = mkdtempSync(join(root, "run-"));
    process.chdir(dir);
    integrity = {};
    for (const name of PACKAGES) integrity[name] = tarball(dir, name);
    log = join(dir, "npm.log");
    const bin = join(dir, "bin");
    mkdirSync(bin);
    writeFileSync(join(bin, "npm"), `#!/bin/sh\necho "$@" >> "${log}"\n`);
    chmodSync(join(bin, "npm"), 0o755);
    for (const [name, value] of Object.entries({
      PATH: `${bin}:${process.env.PATH}`,
      RELEASE_VERSION: VERSION,
      CI_COMMIT_TAG: `v${VERSION}`,
      SIGSTORE_ID_TOKEN: "token",
      NPM_TOKEN: "",
    })) {
      const previous = process.env[name];
      process.env[name] = value;
      t.after(() => (previous === undefined ? delete process.env[name] : (process.env[name] = previous)));
    }
  });

  // Serves each package's registry document: its published integrity at VERSION and its latest.
  function registry(t, documents) {
    t.mock.method(globalThis, "fetch", async (url) => {
      const name = decodeURIComponent(new URL(url).pathname.slice(1));
      const document = documents[name];
      if (!document) return new Response("{}", { status: 404 });
      const versions = document.published ? { [VERSION]: { dist: { integrity: document.published } } } : {};
      return Response.json({ "dist-tags": { latest: document.latest }, versions });
    });
  }

  function published() {
    let lines = [];
    try {
      lines = readFileSync(log, "utf8").trim().split("\n");
    } catch {}
    return Object.fromEntries(
      lines.map((line) => {
        const args = line.split(" ");
        const file = args[1].split("/").pop();
        return [file, args[args.indexOf("--tag") + 1]];
      }),
    );
  }

  test("publishes to latest above it and to superseded below it", async (t) => {
    registry(t, {
      "@sakiori/fff-node": { latest: "1.0.3" },
      "@sakiori/fff-bin-linux-x64-gnu": { latest: "0.11.1-sakiori.20261105.1" },
      "@sakiori/fff-bin-linux-x64-musl": { latest: "1.0.1" },
    });
    t.mock.method(console, "log", () => {});
    await main([]);
    const tags = published();
    assert.equal(Object.keys(tags).length, PACKAGES.length);
    assert.equal(tags[`sakiori-fff-node-${VERSION}.tgz`], "superseded");
    assert.equal(tags[`sakiori-fff-bin-linux-x64-gnu-${VERSION}.tgz`], "latest");
    assert.equal(tags[`sakiori-fff-bin-linux-x64-musl-${VERSION}.tgz`], "latest");
    assert.equal(tags[`sakiori-fff-bin-darwin-arm64-${VERSION}.tgz`], "latest");
  });

  test("a retry skips what is published with the same tarball", async (t) => {
    registry(t, {
      "@sakiori/fff-node": { latest: VERSION, published: integrity["@sakiori/fff-node"] },
      "@sakiori/fff-bin-darwin-arm64": { latest: VERSION, published: integrity["@sakiori/fff-bin-darwin-arm64"] },
    });
    const lines = [];
    t.mock.method(console, "log", (line) => lines.push(line));
    await main([]);
    assert.equal(Object.keys(published()).length, PACKAGES.length - 2);
    assert.ok(lines.includes(`@sakiori/fff-node@${VERSION} is already published with this tarball; skipped.`));
  });

  test("a version published with another tarball fails", async (t) => {
    registry(t, { "@sakiori/fff-bin-darwin-arm64": { latest: VERSION, published: "sha512-other" } });
    await assert.rejects(main([]), /published with another tarball/);
  });

  test("a check reports the dist-tag and publishes nothing", async (t) => {
    registry(t, { "@sakiori/fff-node": { latest: "1.1.0" } });
    delete process.env.CI_COMMIT_TAG;
    const lines = [];
    t.mock.method(console, "log", (line) => lines.push(line));
    await main(["--check"]);
    assert.deepEqual(published(), {});
    assert.ok(lines.includes(`@sakiori/fff-node@${VERSION} is not published; a release would publish it to the dist-tag superseded.`));
  });

  test("a rehearsal's version takes the dist-tag of its release", async (t) => {
    const rehearsal = `${VERSION}-rehearsal.20261109`;
    process.env.RELEASE_VERSION = rehearsal;
    delete process.env.CI_COMMIT_TAG;
    rmSync(join(dir, "dist-npm"), { recursive: true });
    for (const name of PACKAGES) tarball(dir, name, rehearsal);
    registry(t, { "@sakiori/fff-node": { latest: "1.0.1" }, "@sakiori/fff-bin-darwin-x64": { latest: "1.1.0" } });
    const lines = [];
    t.mock.method(console, "log", (line) => lines.push(line));
    await main(["--check"]);
    assert.deepEqual(published(), {});
    assert.ok(lines.includes(`@sakiori/fff-node@${rehearsal} is not published; a release would publish it to the dist-tag latest.`));
    assert.ok(
      lines.includes(`@sakiori/fff-bin-darwin-x64@${rehearsal} is not published; a release would publish it to the dist-tag superseded.`),
    );
    await assert.rejects(main([]), /is not a release version x\.y\.z\./);
  });

  test("the tag must release RELEASE_VERSION, and RELEASE_VERSION must be a release version", async (t) => {
    registry(t, {});
    process.env.CI_COMMIT_TAG = "v1.0.3";
    await assert.rejects(main([]), /The tag v1\.0\.3 does not release 1\.0\.2\./);
    process.env.RELEASE_VERSION = "0.11.1-sakiori.20261108.1";
    await assert.rejects(main(["--check"]), /is not a release version x\.y\.z or a rehearsal's/);
    process.env.RELEASE_VERSION = "1.0.2+fff-0.11.0";
    await assert.rejects(main(["--check"]), /is not a release version/);
  });
});

// Writes the package's tarball to dist-npm/ and returns its integrity.
function tarball(dir, name, version = VERSION) {
  const stage = join(dir, "stage", name);
  mkdirSync(join(stage, "package"), { recursive: true });
  writeFileSync(join(stage, "package", "package.json"), JSON.stringify({ name, version }));
  mkdirSync(join(dir, "dist-npm"), { recursive: true });
  const file = join(dir, "dist-npm", `${name.slice(1).replace("/", "-")}-${version}.tgz`);
  execFileSync("tar", ["-czf", file, "-C", stage, "package"]);
  return `sha512-${createHash("sha512").update(readFileSync(file)).digest("base64")}`;
}
