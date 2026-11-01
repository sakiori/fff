/**
 * Platform detection utilities for downloading the correct binary
 */

import { readFileSync } from "node:fs";

let triple: string | undefined;

/**
 * Get the platform triple (e.g., "x86_64-unknown-linux-gnu"), detected once per process.
 */
export function getTriple(): string {
  triple ??= detectTriple();
  return triple;
}

function detectTriple(): string {
  const platform = process.platform;
  const arch = process.arch;

  let osName: string;
  if (platform === "darwin") {
    osName = "apple-darwin";
  } else if (platform === "android") {
    osName = "linux-android";
  } else if (platform === "linux") {
    osName = detectLinuxLibc();
  } else if (platform === "win32") {
    osName = "pc-windows-msvc";
  } else if (platform === "freebsd") {
    osName = "unknown-freebsd";
  } else {
    throw new Error(`Unsupported platform: ${platform}`);
  }

  const archName = normalizeArch(arch);
  return `${archName}-${osName}`;
}

/**
 * Detect whether we're on musl or glibc Linux, as detect-libc does: the process
 * report names the glibc a glibc Node.js runs on, or the musl objects it
 * loaded; otherwise the system's `ldd` script names its C library.
 */
function detectLinuxLibc(): string {
  const report = processReport();
  if (report?.header?.glibcVersionRuntime) return "unknown-linux-gnu";
  if (report?.sharedObjects?.some((object) => /\/(ld-musl-|libc\.musl-)/.test(object))) {
    return "unknown-linux-musl";
  }
  try {
    if (readFileSync("/usr/bin/ldd", "utf-8").includes("musl")) return "unknown-linux-musl";
  } catch {
    // No ldd script: glibc is the common case.
  }
  return "unknown-linux-gnu";
}

interface LibcReport {
  header?: { glibcVersionRuntime?: string };
  sharedObjects?: string[];
}

function processReport(): LibcReport | null {
  // `excludeNetwork` exists from Node 20.13 and 22.0; older versions ignore it.
  const report = process.report as
    | (NodeJS.ProcessReport & { excludeNetwork?: boolean })
    | undefined;
  if (!report) return null;
  // Network details are not needed and slow the report down.
  const excludeNetwork = report.excludeNetwork;
  report.excludeNetwork = true;
  try {
    return report.getReport() as LibcReport;
  } catch {
    return null;
  } finally {
    report.excludeNetwork = excludeNetwork;
  }
}

/**
 * Normalize architecture name to Rust target format
 */
function normalizeArch(arch: string): string {
  switch (arch) {
    case "x64":
    case "amd64":
      return "x86_64";
    case "arm64":
      return "aarch64";
    case "arm":
      return "arm";
    default:
      throw new Error(`Unsupported architecture: ${arch}`);
  }
}

/**
 * Get the library file extension for the current platform
 */
export function getLibExtension(): "dylib" | "so" | "dll" {
  switch (process.platform) {
    case "darwin":
      return "dylib";
    case "win32":
      return "dll";
    default:
      return "so";
  }
}

/**
 * Get the library filename prefix (empty on Windows)
 */
export function getLibPrefix(): string {
  return process.platform === "win32" ? "" : "lib";
}

/**
 * Get the full library filename for the current platform
 */
export function getLibFilename(): string {
  const prefix = getLibPrefix();
  const ext = getLibExtension();
  return `${prefix}fff_c.${ext}`;
}

/**
 * Get the filename of the shared service executable for the current platform
 */
export function getServiceExecutableFilename(): string {
  return process.platform === "win32" ? "fff-mcp.exe" : "fff-mcp";
}

/**
 * Map from Rust target triple to npm platform package name.
 * Each @sakiori/fff-bin-* package carries the libfff_c shared library and the
 * fff-mcp service executable of the same build.
 */
export const TRIPLE_TO_NPM_PACKAGE: Record<string, string> = {
  "aarch64-apple-darwin": "@sakiori/fff-bin-darwin-arm64",
  "x86_64-unknown-linux-gnu": "@sakiori/fff-bin-linux-x64-gnu",
  "x86_64-unknown-linux-musl": "@sakiori/fff-bin-linux-x64-musl",
};

/**
 * Get the npm package name for the current platform's native binaries.
 *
 * @returns Package name like "@sakiori/fff-bin-darwin-arm64"
 * @throws If the current platform is not supported
 */
export function getNpmPackageName(): string {
  const triple = getTriple();
  const packageName = TRIPLE_TO_NPM_PACKAGE[triple];
  if (!packageName) {
    throw new Error(`No npm package available for platform: ${triple}`);
  }
  return packageName;
}
