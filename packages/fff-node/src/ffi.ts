/**
 * Node.js FFI bindings for the fff-c native library using ffi-rs
 *
 * Every instance is a session on the shared fff service. Searches, greps and
 * scan state travel through `fff_service_call`, which takes one request of the
 * service's program protocol as JSON and returns the whole reply as one JSON
 * document, so a result of any size is decoded in a single pass.
 *
 * Calls that reach the service block until it answers or their time limit
 * passes, so they run on the package's worker threads (see `native-pool.ts`)
 * and resolve a Promise. Only calls that need no service run on the calling
 * thread. An instance is identified by its native address, a BigInt, which
 * every thread of the process can pass back to the library.
 *
 * ## Memory management
 *
 * Every `fff_*` function returning `*mut FffResult` allocates with Rust's Box.
 * We MUST call `fff_free_result` to properly deallocate (not libc::free).
 *
 * ## FffResult struct reading
 *
 * ffi-rs auto-dereferences struct retType pointers, losing the original pointer.
 * We call the C function with `retType: DataType.External` to get the raw
 * pointer, read the struct with `restorePointer`, then free the raw pointer
 * with `fff_free_result`. `isNullPointer` detects null C pointers.
 */

import { createRequire } from "node:module";
import type * as FfiRs from "ffi-rs";
import type { FieldType, JsExternal } from "ffi-rs";
import { findNativeFiles, type NativeFiles } from "./binary.js";
import { ErrorCode, err, nativeErrorCode, type Result } from "./errors.js";
import { closeNativePool, type NativeReply, type NativeSetup, runNative } from "./native-pool.js";

const LIBRARY_KEY = "fff_c";

function createOptionsStruct() {
  const { DataType } = ffi();
  return {
    version: DataType.U32,
    base_path: DataType.String,
    frecency_db_path: DataType.String,
    history_db_path: DataType.String,
    enable_mmap_cache: DataType.U8,
    enable_content_indexing: DataType.U8,
    watch: DataType.U8,
    ai_mode: DataType.U8,
    log_file_path: DataType.String,
    log_level: DataType.String,
    cache_budget_max_files: DataType.U64,
    cache_budget_max_bytes: DataType.U64,
    cache_budget_max_file_size: DataType.U64,
    enable_fs_root_scanning: DataType.U8,
    enable_home_dir_scanning: DataType.U8,
    follow_symlinks: DataType.U8,
  };
}

// ALWAYS KEEP IN SYNC WITH fff.h
const FFF_CREATE_OPTIONS_VERSION = 2;

/**
 * Struct type definition for FffResult used with restorePointer.
 *
 * Uses U8 for the bool success field (correct alignment with ffi-rs).
 * Uses External for ALL pointer fields to avoid hangs on null char* pointers
 * (ffi-rs hangs when trying to read DataType.String from null char*).
 */
function resultStruct() {
  const { DataType } = ffi();
  return {
    success: DataType.U8,
    error: DataType.External,
    handle: DataType.External,
    int_value: DataType.I64,
  };
}

interface FffResultRaw {
  success: number;
  error: JsExternal;
  handle: JsExternal;
  int_value: number;
}

/**
 * Opaque native handle: the instance's address. Callers must not inspect or
 * modify this value.
 */
export type NativeHandle = bigint;

// The native files of the loaded library, resolved once per process.
let loaded: NativeFiles | null = null;

// ffi-rs, required once the platform's native files are found: it refuses to load on a
// platform it has no binary for, which must be a failed Result rather than a failed import.
let ffiModule: typeof FfiRs | null = null;

function ffi(): typeof FfiRs {
  if (!ffiModule) throw new Error("the native library is not loaded");
  return ffiModule;
}

// Native instances created and not yet freed.
let liveInstances = 0;

/**
 * The number of native instances this process created and has not freed yet;
 * each holds a session on the service. A destroyed `FileFinder` frees its own.
 */
export function nativeInstanceCount(): number {
  return liveInstances;
}

/**
 * Load the native library, or report why it cannot be loaded.
 */
function loadLibrary(): Result<NativeFiles> {
  if (loaded) return { ok: true, value: loaded };
  const found = findNativeFiles();
  if (!found.ok) return found;
  try {
    ffiModule ??= createRequire(import.meta.url)("ffi-rs") as typeof FfiRs;
    ffiModule.open({ library: LIBRARY_KEY, path: found.value.library });
  } catch (error) {
    return err(
      `Cannot load ${found.value.library}: ${String(error)}`,
      ErrorCode.libraryUnavailable,
    );
  }
  loaded = found.value;
  return found;
}

/**
 * What a worker thread loads: the same library file as the main thread, and the
 * ffi-rs module this package resolves.
 */
function nativeSetup(): NativeSetup {
  const library = loaded?.library;
  if (!library) throw new Error("the native library is not loaded");
  return {
    ffiPath: createRequire(import.meta.url).resolve("ffi-rs"),
    library,
    createOptions: createOptionsStruct(),
  };
}

/** Run an operation of the native worker once the library is known to load. */
function runOnWorker<T>(operation: string, args: unknown[]): Promise<NativeReply<T>> {
  const library = loadLibrary();
  if (!library.ok) return Promise.resolve(library);
  return runNative<T>(nativeSetup, operation, args);
}

/**
 * Read a C string (char*) from an ffi-rs External pointer.
 *
 * Uses restorePointer + wrapPointer to dereference the char* and read the
 * null-terminated string. Returns null if the pointer is null.
 */
function readCString(ptr: JsExternal): string | null {
  if (ffi().isNullPointer(ptr)) return null;
  try {
    const [str] = ffi().restorePointer({
      retType: [ffi().DataType.String],
      paramsValue: ffi().wrapPointer([ptr]),
    });
    return str as string;
  } catch {
    return null;
  }
}

/** Free a pointer the library allocated with the given free function. */
function freeWith(funcName: string, ptr: JsExternal): void {
  try {
    ffi().load({
      library: LIBRARY_KEY,
      funcName,
      retType: ffi().DataType.Void,
      paramsType: [ffi().DataType.External],
      paramsValue: [ptr],
    });
  } catch {
    // Ignore cleanup errors
  }
}

/**
 * Call a function returning FffResult with a C string in handle, on the calling
 * thread, for calls that need no service.
 */
function callStringSync(
  funcName: string,
  paramsType: FieldType[],
  paramsValue: unknown[],
): Result<string | null> {
  const library = loadLibrary();
  if (!library.ok) return library;
  const rawPtr = ffi().load({
    library: LIBRARY_KEY,
    funcName,
    retType: ffi().DataType.External,
    paramsType,
    paramsValue,
    freeResultMemory: false,
  }) as JsExternal;
  const [struct] = ffi().restorePointer({
    retType: [resultStruct()],
    paramsValue: ffi().wrapPointer([rawPtr]),
  }) as unknown as [FffResultRaw];
  if (struct.success === 0) {
    const error = readCString(struct.error);
    freeWith("fff_free_result", rawPtr);
    return err(error || "Unknown error", nativeErrorCode(Number(struct.int_value)));
  }
  const handle = struct.handle;
  freeWith("fff_free_result", rawPtr);
  if (ffi().isNullPointer(handle)) return { ok: true, value: null };
  const text = readCString(handle);
  freeWith("fff_free_string", handle);
  return { ok: true, value: text };
}

/**
 * Convert snake_case keys to camelCase recursively
 */
function snakeToCamel(obj: unknown): unknown {
  if (obj === null || obj === undefined) return obj;
  if (typeof obj !== "object") return obj;
  if (Array.isArray(obj)) return obj.map(snakeToCamel);

  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj as Record<string, unknown>)) {
    const camelKey = key.replace(/_([a-z])/g, (_, letter: string) => letter.toUpperCase());
    result[camelKey] = snakeToCamel(value);
  }
  return result;
}

/**
 * Open a session on the shared service on a worker thread. `timeoutMs` bounds
 * opening it and each later call (0 takes the library's defaults).
 */
export async function ffiCreate(
  basePath: string,
  frecencyDbPath: string,
  enableMmapCache: boolean,
  enableContentIndexing: boolean,
  watch: boolean,
  aiMode: boolean,
  logFilePath: string,
  logLevel: string,
  cacheBudgetMaxFiles: number,
  enableFsRootScanning: boolean,
  enableHomeDirScanning: boolean,
  followSymlinks: boolean,
  serviceDir: string,
  timeoutMs: number,
): Promise<Result<NativeHandle>> {
  const library = loadLibrary();
  if (!library.ok) return library;

  const optsValue = {
    version: FFF_CREATE_OPTIONS_VERSION,
    base_path: basePath,
    frecency_db_path: frecencyDbPath,
    history_db_path: "",
    enable_mmap_cache: enableMmapCache ? 1 : 0,
    enable_content_indexing: enableContentIndexing ? 1 : 0,
    watch: watch ? 1 : 0,
    ai_mode: aiMode ? 1 : 0,
    log_file_path: logFilePath,
    log_level: logLevel,
    cache_budget_max_files: cacheBudgetMaxFiles,
    cache_budget_max_bytes: 0,
    cache_budget_max_file_size: 0,
    enable_fs_root_scanning: enableFsRootScanning ? 1 : 0,
    enable_home_dir_scanning: enableHomeDirScanning ? 1 : 0,
    follow_symlinks: followSymlinks ? 1 : 0,
  };

  const created = await runOnWorker<bigint>("create", [
    optsValue,
    library.value.executable,
    serviceDir,
    timeoutMs,
  ]);
  if (!created.ok) return created;
  if (created.value === 0n) {
    return err("fff_create_service_instance returned null handle");
  }
  liveInstances++;
  return { ok: true, value: created.value };
}

/**
 * End an instance's session and free it on a worker thread. No other call may
 * be in flight on the instance.
 */
export async function ffiDestroy(handle: NativeHandle): Promise<void> {
  const destroyed = await runOnWorker("destroy", [handle]);
  if (destroyed.ok) liveInstances--;
}

/**
 * Send one request of the service's program protocol on a worker thread and
 * decode the reply.
 */
export async function ffiServiceCall<T>(
  handle: NativeHandle,
  request: unknown,
): Promise<Result<T>> {
  const reply = await runOnWorker<string | null>("call", [handle, JSON.stringify(request)]);
  if (!reply.ok) return reply;
  if (reply.value === null) return err("fff_service_call returned no reply");
  try {
    return { ok: true, value: JSON.parse(reply.value) as T };
  } catch (error) {
    return err(`Cannot decode the service reply: ${String(error)}`);
  }
}

/**
 * Get the base path of the file picker; the instance knows it without the service.
 */
export function ffiGetBasePath(handle: NativeHandle): Result<string | null> {
  const library = loadLibrary();
  if (!library.ok) return library;
  return callStringSync("fff_get_base_path", [ffi().DataType.BigInt], [handle]);
}

/**
 * Restart the index in `newPath`, which for a service instance rescans its own
 * directory, on a worker thread.
 */
export async function ffiRestartIndex(
  handle: NativeHandle,
  newPath: string,
): Promise<Result<void>> {
  const done = await runOnWorker<null>("restart", [handle, newPath]);
  return done.ok ? { ok: true, value: undefined } : done;
}

function healthResult(json: Result<string | null>): Result<unknown> {
  if (!json.ok) return json;
  if (json.value === null || json.value === "") return { ok: true, value: undefined };
  try {
    return { ok: true, value: snakeToCamel(JSON.parse(json.value)) };
  } catch {
    return { ok: true, value: json.value };
  }
}

/**
 * Health check of an instance, which asks its service, on a worker thread.
 */
export async function ffiHealthCheck(
  handle: NativeHandle,
  testPath: string,
): Promise<Result<unknown>> {
  const json = await runOnWorker<string | null>("health", [handle, testPath]);
  return json.ok ? healthResult(json) : json;
}

/**
 * Health check without an instance (version and git only), on the calling
 * thread. A zero address stands in for the null pointer.
 */
export function ffiHealthCheckStatic(testPath: string): Result<unknown> {
  const library = loadLibrary();
  if (!library.ok) return library;
  const { DataType } = ffi();
  return healthResult(
    callStringSync("fff_health_check", [DataType.BigInt, DataType.String], [0n, testPath]),
  );
}

/**
 * Ensure the library is loaded.
 *
 * Loads the native library from the platform-specific npm package
 * or a local dev build. Throws if the binary is not found.
 */
export function ensureLoaded(): void {
  const library = loadLibrary();
  if (!library.ok) throw new Error(library.error);
}

/**
 * Check if the library is available.
 */
export function isAvailable(): boolean {
  return loadLibrary().ok;
}

/**
 * Close the library and stop the worker threads that call it. Call this when
 * completely done with the library; instances must have been destroyed.
 */
export async function closeLibrary(): Promise<void> {
  await closeNativePool();
  if (loaded) {
    ffi().close(LIBRARY_KEY);
    loaded = null;
  }
}
