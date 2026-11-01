import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [pkgDir, version] = process.argv.slice(2);
if (!pkgDir || !version) {
  console.error("usage: set-npm-version.mjs <pkg-dir> <version>");
  process.exit(1);
}

// fff-bun/fff-node pull their native binaries from a platform package at runtime.
// Those are only resolvable once published, so they are injected here instead of
// living in the manifests where they would break `npm ci` and bun installs. Each
// consumer takes the platform packages of its own npm scope: the fork publishes
// @sakiori/fff-node with the @sakiori/fff-bin-* packages it builds, as plain
// dependencies, since a registry may drop optionalDependencies and os/cpu/libc
// from package metadata; the package loads the host's platform package at runtime.
const PLATFORM_DEP_FIELDS = {
  "@ff-labs/fff-bun": "optionalDependencies",
  "@sakiori/fff-node": "dependencies",
};
const WORKSPACE_DEPS = ["@ff-labs/fff-bun", "@sakiori/fff-node"];

const manifestPath = join(pkgDir, "package.json");
const pkg = JSON.parse(readFileSync(manifestPath, "utf8"));

pkg.version = version;

if (pkg.optionalDependencies) {
  for (const dep of Object.keys(pkg.optionalDependencies)) {
    pkg.optionalDependencies[dep] = version;
  }
}

for (const dep of WORKSPACE_DEPS) {
  if (pkg.dependencies?.[dep]) pkg.dependencies[dep] = version;
}

const platformDepField = PLATFORM_DEP_FIELDS[pkg.name];
if (platformDepField) {
  const platformDeps = {};
  const scope = `${pkg.name.split("/")[0]}/`;
  for (const name of platformPackageNames().filter((name) => name.startsWith(scope))) {
    platformDeps[name] = version;
  }
  if (Object.keys(platformDeps).length === 0) {
    console.error(`no platform packages found under packages/, refusing to publish ${pkg.name}`);
    process.exit(1);
  }
  pkg[platformDepField] = { ...pkg[platformDepField], ...platformDeps };
}

writeFileSync(manifestPath, `${JSON.stringify(pkg, null, 2)}\n`);
console.log(`Set ${pkgDir} to ${version}`);

function platformPackageNames() {
  return readdirSync("packages")
    .filter((entry) => entry.startsWith("fff-bin-"))
    .map((entry) => JSON.parse(readFileSync(join("packages", entry, "package.json"), "utf8")).name)
    .sort();
}
