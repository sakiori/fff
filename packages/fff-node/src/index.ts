/**
 * fff - Fast File Finder
 *
 * High-performance fuzzy file finder for Node.js, powered by Rust.
 * Perfect for LLM agent tools that need to search through codebases.
 *
 * Each `FileFinder` instance is a session on the shared fff service, which
 * owns the indexes of every program and `fff-mcp` agent of the same OS user
 * and release. Create as many as you need and destroy them when done. Methods
 * that reach the service return Promises and never block the event loop.
 *
 * Uses ffi-rs to load the native libfff_c library; the platform package
 * carries it beside the `fff-mcp` executable that starts the service.
 *
 * @example
 * ```typescript
 * import { FileFinder } from "@sakiori/fff-node";
 *
 * // Create a file finder instance
 * const result = await FileFinder.create({ basePath: "/path/to/project" });
 * if (!result.ok) {
 *   console.error(result.error);
 *   process.exit(1);
 * }
 * const finder = result.value;
 *
 * // Wait for initial scan
 * await finder.waitForScan(5000);
 *
 * // Search for files
 * const search = await finder.fileSearch("main.ts");
 * if (search.ok) {
 *   for (const item of search.value.items) {
 *     console.log(item.relativePath);
 *   }
 * }
 *
 * // Cleanup when done
 * await finder.destroy();
 * ```
 *
 * @packageDocumentation
 */

export {
  binaryExists,
  findBinary,
  findNativeFiles,
  findServiceExecutable,
  type NativeFiles,
  unpackedAsarPath,
} from "./binary.js";
export { closeLibrary, nativeInstanceCount } from "./ffi.js";
export type {
  DbHealth,
  DirItem,
  DirSearchOptions,
  DirSearchResult,
  FileItem,
  GrepCursor,
  GrepMatch,
  GrepMode,
  DefinitionMode,
  GrepOptions,
  GrepResult,
  HealthCheck,
  InitOptions,
  Location,
  MixedItem,
  MixedSearchResult,
  MultiGrepOptions,
  ScanProgress,
  Score,
  SearchOptions,
  SearchResult,
  WatchBatchCallback,
  WatchEvent,
  WatchEventKind,
  WatchOptions,
  WatchUnsubscribe,
} from "./fff-api.js";
// Results and their error codes
export { ErrorCode, type Failure, type Result, err, ok } from "./errors.js";
export { FileFinder } from "./finder.js";
export {
  getLibExtension,
  getLibFilename,
  getNpmPackageName,
  getServiceExecutableFilename,
  getTriple,
} from "./platform.js";
export type { FrecencyOptions, ServiceInitOptions } from "./service-options.js";
