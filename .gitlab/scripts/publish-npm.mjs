// Publishes the tarballs pack-npm.sh wrote to dist-npm/ to npmjs.org, the platform packages before
// @sakiori/fff-node, with provenance. npm authenticates by trusted publishing (NPM_ID_TOKEN) and
// falls back to NPM_TOKEN, which is set only to publish a package for the first time (CUSTOM.md).
//
// npm never replaces a version: a version published with the same tarball is skipped, so a
// retried job passes, and one published with another tarball fails the job. With --check, as in
// a rehearsal, it only reads the registry and reports what a publish would do.
//
//   node .gitlab/scripts/publish-npm.mjs [--check]
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const DIST = "dist-npm";
const REGISTRY = "https://registry.npmjs.org/";
const PACKAGES = [
  "@sakiori/fff-bin-darwin-arm64",
  "@sakiori/fff-bin-darwin-x64",
  "@sakiori/fff-bin-linux-arm64-gnu",
  "@sakiori/fff-bin-linux-arm64-musl",
  "@sakiori/fff-bin-linux-x64-gnu",
  "@sakiori/fff-bin-linux-x64-musl",
  "@sakiori/fff-node",
];

class Failure extends Error {}

function fail(message) {
  throw new Failure(message);
}

function env(name, why = "") {
  const value = process.env[name];
  if (!value) fail(`${name} is not set${why ? `: ${why}` : ""}.`);
  return value;
}

async function main(args) {
  const unknown = args.filter((arg) => arg !== "--check");
  if (unknown.length > 0) fail(`Unknown arguments: ${unknown.join(" ")}; usage: publish-npm.mjs [--check]`);
  const check = args.includes("--check");
  const version = env("RELEASE_VERSION", "git intake release version sets it");
  if (!check) {
    const tag = env("CI_COMMIT_TAG", "only a tag pipeline publishes");
    if (tag !== `v${version}`) fail(`The tag ${tag} does not release ${version}.`);
    env("SIGSTORE_ID_TOKEN", "provenance needs the job's ID token for Sigstore");
  }

  const tarball = (name) => `${name.slice(1).replace("/", "-")}-${version}.tgz`;
  const present = readdirSync(DIST).sort();
  const expected = PACKAGES.map(tarball).sort();
  if (JSON.stringify(present) !== JSON.stringify(expected)) {
    fail(`${DIST}/ holds ${present.join(" ")}, but the release is ${expected.join(" ")}.`);
  }
  // The registry takes a tarball by the name and version in its manifest, not by its file name.
  for (const name of PACKAGES) {
    const file = join(DIST, tarball(name));
    const manifest = JSON.parse(execFileSync("tar", ["-xOzf", file, "package/package.json"], { encoding: "utf8" }));
    if (manifest.name !== name || manifest.version !== version) {
      fail(`${file} holds ${manifest.name}@${manifest.version}, not ${name}@${version}.`);
    }
  }

  // The integrity of `name` at the release version, or null when it is not published.
  async function published(name) {
    const url = `${REGISTRY}${name.replace("/", "%2F")}`;
    const response = await fetch(url, { headers: { accept: "application/json" } });
    if (response.status === 404) return null;
    if (!response.ok) fail(`GET ${url} answered ${response.status}: ${(await response.text()).slice(0, 2000)}`);
    const dist = (await response.json()).versions?.[version]?.dist;
    if (!dist) return null;
    if (!dist.integrity) fail(`${name}@${version} is published without an integrity.`);
    return dist.integrity;
  }

  const work = mkdtempSync(join(tmpdir(), "publish-npm-"));
  try {
    // npm expands the token from the environment when it reads the file; without it, the file
    // names only the registry and npm publishes by trusted publishing.
    const npmrc = join(work, "npmrc");
    const token = process.env.NPM_TOKEN ? "//registry.npmjs.org/:_authToken=${NPM_TOKEN}\n" : "";
    writeFileSync(npmrc, `registry=${REGISTRY}\n${token}`, { mode: 0o600 });
    for (const name of PACKAGES) {
      const file = join(DIST, tarball(name));
      const integrity = `sha512-${createHash("sha512").update(readFileSync(file)).digest("base64")}`;
      const existing = await published(name);
      if (existing) {
        if (existing !== integrity) {
          fail(`${name}@${version} is published with another tarball; a version is never replaced.`);
        }
        console.log(`${name}@${version} is already published with this tarball; skipped.`);
        continue;
      }
      if (check) {
        console.log(`${name}@${version} is not published; a release would publish it.`);
        continue;
      }
      // A fork version is a prerelease, which npm publishes only under an explicit dist-tag.
      try {
        execFileSync(
          "npm",
          ["publish", resolve(file), "--access", "public", "--tag", "latest", "--provenance", "--ignore-scripts"],
          { stdio: "inherit", env: { ...process.env, NPM_CONFIG_USERCONFIG: npmrc } },
        );
      } catch {
        // A retry soon after a partial publish can read a stale document and publish again.
        const now = await published(name);
        if (now !== integrity) fail(`Publishing ${name}@${version} failed.`);
        console.log(`${name}@${version} is now published with this tarball; skipped.`);
        continue;
      }
      console.log(`Published ${name}@${version}.`);
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

try {
  await main(process.argv.slice(2));
} catch (error) {
  console.error(error instanceof Failure ? error.message : error);
  process.exitCode = 1;
}
