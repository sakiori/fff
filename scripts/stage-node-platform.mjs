import { chmodSync, copyFileSync, existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

// Copies the native library and the fff-mcp executable of one cargo build into a
// @sakiori/fff-bin-* package. Both must come from the same build: the library
// starts and joins only a service of its own build identity.
const [buildDir, pkgDir] = process.argv.slice(2);
if (!buildDir || !pkgDir) {
  console.error("usage: stage-node-platform.mjs <cargo-output-dir> <platform-package-dir>");
  process.exit(1);
}

const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const executable = pkg.main.endsWith(".dll") ? "fff-mcp.exe" : "fff-mcp";
for (const file of [pkg.main, executable]) {
  const source = join(buildDir, file);
  if (!existsSync(source)) {
    console.error(`missing build output: ${source}`);
    process.exit(1);
  }
  copyFileSync(source, join(pkgDir, file));
}
chmodSync(join(pkgDir, executable), 0o755);
console.log(`Staged ${pkg.main} and ${executable} into ${pkg.name}`);
