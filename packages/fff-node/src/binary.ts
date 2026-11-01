/**
 * Native file resolution for fff-node
 *
 * The native library and the fff-mcp service executable must come from one
 * build, so both are resolved from the same location:
 * 1. The FFF_C_LIB environment variable (explicit path to libfff_c.so/.dylib/.dll,
 *    with fff-mcp beside it)
 * 2. Platform-specific npm package (e.g. @sakiori/fff-bin-darwin-arm64)
 * 3. Local dev build (bin/, then target/release or target/debug)
 */

import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { ErrorCode, err, type Result } from "./errors.js";
import {
  getLibFilename,
  getServiceExecutableFilename,
  getTriple,
  TRIPLE_TO_NPM_PACKAGE,
} from "./platform.js";

const PACKAGE_NAME = "@sakiori/fff-node";

/** The native library and the service executable of one build. */
export interface NativeFiles {
  library: string;
  executable: string;
}

/**
 * Get the current file's directory
 */
function getCurrentDir(): string {
  const url = import.meta.url;

  if (url.startsWith("file://")) {
    return dirname(fileURLToPath(url));
  }
  return dirname(url);
}

/**
 * Get the package root directory
 */
function getPackageDir(): string {
  const currentDir = getCurrentDir();
  // In dev: src/ -> package root
  // In dist: dist/ -> package root
  // We look for package.json to find the actual root
  let dir = currentDir;
  for (let i = 0; i < 5; i++) {
    if (existsSync(join(dir, "package.json"))) {
      try {
        const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf-8"));
        if (pkg.name === PACKAGE_NAME) {
          return dir;
        }
      } catch {
        // Not our package.json, keep going up
      }
    }
    dir = dirname(dir);
  }
  // Fallback: assume we're one level deep in src/
  return dirname(currentDir);
}

/**
 * Map a path inside an `.asar` archive to its `.asar.unpacked` counterpart when
 * the unpacked file exists. Native files cannot be loaded or executed from inside
 * an archive, so Electron applications unpack them beside it.
 */
export function unpackedAsarPath(path: string): string {
  const segments = path.split(sep);
  for (let i = 0; i < segments.length; i++) {
    if (!segments[i].endsWith(".asar")) continue;
    const unpacked = [...segments];
    unpacked[i] = `${segments[i]}.unpacked`;
    const candidate = unpacked.join(sep);
    if (existsSync(candidate)) return candidate;
  }
  return path;
}

/**
 * Both native files in `dir`, taken from its `.asar.unpacked` counterpart when
 * that exists, so they always come from one directory. Null when neither is
 * there; an error when only one is, or when they are inside an archive.
 */
function nativeFilesIn(dir: string): Result<NativeFiles> | null {
  const unpacked = unpackedAsarPath(dir);
  const library = join(unpacked, getLibFilename());
  const executable = join(unpacked, getServiceExecutableFilename());
  const [hasLibrary, hasExecutable] = [existsSync(library), existsSync(executable)];
  if (hasLibrary && hasExecutable) {
    if (unpacked === dir && dir.split(sep).some((segment) => segment.endsWith(".asar"))) {
      return err(
        `The native files of ${dir} are inside an asar archive; unpack the platform package's directory`,
        ErrorCode.libraryUnavailable,
      );
    }
    return { ok: true, value: { library, executable } };
  }
  if (hasLibrary || hasExecutable) {
    const missing = hasLibrary ? executable : library;
    return err(
      `${missing} is missing beside the other native file; ship both from one build`,
      ErrorCode.libraryUnavailable,
    );
  }
  return null;
}

/**
 * Resolve the native files from the platform-specific npm package.
 *
 * @sakiori/fff-node depends on every platform package; the host's one (e.g.
 * @sakiori/fff-bin-darwin-arm64) holds both files at its root. An application
 * bundle may ship only its target's one.
 */
function resolveFromNpmPackage(packageName: string): Result<NativeFiles> | null {
  let packageJsonPath: string;
  try {
    const require = createRequire(join(getPackageDir(), "package.json"));
    packageJsonPath = require.resolve(`${packageName}/package.json`);
  } catch {
    // Not installed, e.g. left out of an application bundle
    return null;
  }
  return nativeFilesIn(dirname(packageJsonPath));
}

/**
 * Resolve the native files from an explicit library path, for platforms without a
 * prebuilt package (for example a libfff_c.so built from source on FreeBSD) or any
 * custom build. The service executable of the same build must sit beside it.
 */
function resolveFromOverride(library: string): Result<NativeFiles> {
  if (!existsSync(library)) {
    return err(
      `FFF_C_LIB points to a file that does not exist: ${library}`,
      ErrorCode.libraryUnavailable,
    );
  }
  const executable = join(dirname(library), getServiceExecutableFilename());
  if (!existsSync(executable)) {
    return err(
      `${executable} is missing beside FFF_C_LIB; ship both from one build`,
      ErrorCode.libraryUnavailable,
    );
  }
  return { ok: true, value: { library, executable } };
}

/**
 * Get the development build (for local development)
 */
function resolveFromDevBuild(): Result<NativeFiles> | null {
  const packageDir = getPackageDir();
  const workspaceRoot = join(packageDir, "..", "..");
  for (const dir of [
    join(packageDir, "bin"),
    join(workspaceRoot, "target", "release"),
    join(workspaceRoot, "target", "debug"),
  ]) {
    const found = nativeFilesIn(dir);
    if (found) return found;
  }
  return null;
}

function isDevWorkspace(): boolean {
  const packageDir = getPackageDir();
  const workspaceRoot = join(packageDir, "..", "..");
  return existsSync(join(workspaceRoot, "Cargo.toml"));
}

// Resolved once per process; the files do not move while it runs.
let resolved: Result<NativeFiles> | undefined;

function resolveNativeFiles(): Result<NativeFiles> {
  const override = process.env["FFF_C_LIB"];
  if (override) return resolveFromOverride(override);
  let triple: string;
  try {
    triple = getTriple();
  } catch (error) {
    return err(String(error instanceof Error ? error.message : error), ErrorCode.unsupportedPlatform);
  }
  const packageName = TRIPLE_TO_NPM_PACKAGE[triple];
  const fromPackage = () => (packageName ? resolveFromNpmPackage(packageName) : null);
  const found = isDevWorkspace()
    ? (resolveFromDevBuild() ?? fromPackage())
    : (fromPackage() ?? resolveFromDevBuild());
  if (found) return found;
  if (!packageName) {
    return err(
      `@sakiori/fff-node supports macOS on arm64 and Linux on x64 with glibc or musl, not ${triple}; set FFF_C_LIB to a source build`,
      ErrorCode.unsupportedPlatform,
    );
  }
  return err(
    `fff native files not found: install ${packageName} beside @sakiori/fff-node, or build libfff_c and fff-mcp from source with \`make prepare-node\``,
    ErrorCode.libraryUnavailable,
  );
}

/**
 * Find the native library and the service executable.
 *
 * Resolution order:
 * - FFF_C_LIB (if set) always wins
 * - Dev workspace: local dev build first, then npm package
 * - Production: npm package first, then dev build
 *
 * @returns Absolute paths of both files from one directory, or why they cannot be used
 */
export function findNativeFiles(): Result<NativeFiles> {
  resolved ??= resolveNativeFiles();
  return resolved;
}

/**
 * Find the native library.
 *
 * @returns Absolute path to the library, or null if not found
 */
export function findBinary(): string | null {
  const files = findNativeFiles();
  return files.ok ? files.value.library : null;
}

/**
 * Find the fff-mcp executable that starts the shared service.
 *
 * @returns Absolute path to the executable, or null if not found
 */
export function findServiceExecutable(): string | null {
  const files = findNativeFiles();
  return files.ok ? files.value.executable : null;
}

/**
 * Check if the native files exist in any known location
 */
export function binaryExists(): boolean {
  return findNativeFiles().ok;
}
