/**
 * FileFinder - High-level API for the fff file finder
 *
 * This class provides a type-safe, ergonomic API for file finding operations.
 * Each instance is a session on the shared fff service of the package's
 * release, which owns the index; instances of any process searching the same
 * directory share one index with each other and with `fff-mcp` agents.
 *
 * Every method that reaches the service returns a Promise of a Result and runs
 * its native call on one of the package's worker threads, so a slow or
 * unresponsive service blocks neither the event loop nor the libuv thread pool.
 * Methods that need no service stay synchronous.
 */

import {
  ensureLoaded,
  ffiCreate,
  ffiDestroy,
  ffiGetBasePath,
  ffiHealthCheck,
  ffiHealthCheckStatic,
  ffiRestartIndex,
  ffiServiceCall,
  ffiRegisterWatchCallback,
  ffiUnwatch,
  ffiWatchStart,
  isAvailable,
  type NativeHandle,
} from "./ffi.js";

import type {
  DirSearchOptions,
  DirSearchResult,
  GlobOptions,
  GrepCursor,
  GrepOptions,
  GrepResult,
  HealthCheck,
  MixedSearchResult,
  MultiGrepOptions,
  ScanProgress,
  SearchOptions,
  SearchResult,
  WatchUnsubscribe,
} from "./fff-api.js";
import { definitionModeToInt } from "./fff-api.js";

import { ErrorCode, err, type Result } from "./errors.js";
import type {
  FrecencyOptions,
  ServiceInitOptions,
  ServiceWatchBatchCallback,
  ServiceWatchOptions,
} from "./service-options.js";
import { deleteWatchListener, setWatchListener } from "./watch-delivery.js";

/** Interval of the readiness polls of `waitForScan` and `waitForIndexReady`. */
const READY_POLL_MS = 50;

/** The error of a method the shared service has no equivalent for. */
function unavailable<T>(method: string): Result<T> {
  return err(`FileFinder.${method} is unavailable over the shared service`, ErrorCode.unavailable);
}

/** Where a grep continues: a file offset into the order of the index that returned it. */
interface ServiceCursor {
  index: string;
  offset: number;
}

/** The opaque cursor of the shared API, carrying the index it belongs to. */
interface IndexedGrepCursor extends GrepCursor {
  readonly _index: string;
}

/** A grep reply before its cursor is wrapped. */
type GrepReply = Omit<GrepResult, "nextCursor"> & { nextCursor: ServiceCursor | null };

const GREP_MODES = { plain: "PlainText", regex: "Regex", fuzzy: "Fuzzy" } as const;

/** The service's definition modes, indexed by `definitionModeToInt`. */
const DEFINITION_MODES = ["Off", "Classify", "DefinitionOnly", "UsageOnly"] as const;

function serviceCursor(cursor: GrepCursor | null | undefined): Result<ServiceCursor | null> {
  if (!cursor) return { ok: true, value: null };
  const index = (cursor as Partial<IndexedGrepCursor>)._index;
  if (index === undefined) {
    return err(
      "The grep cursor was not returned by a FileFinder grep; search again without it",
      ErrorCode.invalidRequest,
    );
  }
  return { ok: true, value: { index, offset: cursor._offset } };
}

function grepResult(reply: Result<GrepReply>): Result<GrepResult> {
  if (!reply.ok) return reply;
  const next = reply.value.nextCursor;
  const nextCursor: IndexedGrepCursor | null = next
    ? { __brand: "GrepCursor", _offset: next.offset, _index: next.index }
    : null;
  return { ok: true, value: { ...reply.value, nextCursor } };
}

function grepOptions(options: GrepOptions & FrecencyOptions, cursor: ServiceCursor | null) {
  const definitionMode = definitionModeToInt(options.classifyDefinitions);
  if (definitionMode === undefined) {
    return err("Invalid classifyDefinitions mode", ErrorCode.invalidRequest);
  }
  const value = {
    max_file_size: options.maxFileSize || 10 * 1024 * 1024,
    max_matches_per_file: options.maxMatchesPerFile ?? 0,
    smart_case: options.smartCase ?? true,
    cursor,
    page_size: options.pageSize || 50,
    time_budget_ms: options.timeBudgetMs ?? 0,
    enforce_time_budget: options.enforceTimeBudget ?? false,
    before_context: options.beforeContext ?? 0,
    after_context: options.afterContext ?? 0,
    classify_definitions: DEFINITION_MODES[definitionMode],
    exclude_frecency: options.excludeFrecency ?? false,
  };
  return { ok: true as const, value };
}

/**
 * The native calls of one instance, one at a time.
 *
 * The service session answers one call at a time anyway; queuing here keeps an
 * instance to one worker thread, so other instances keep the rest. When a call
 * finds the service lost (it did not answer within its bound, or the
 * connection broke), the calls queued behind it fail with the same error
 * rather than each waiting out that bound again; the next call opens a new
 * session. A call run with `always` runs regardless, as freeing the instance
 * must.
 */
class CallQueue {
  private tail: Promise<unknown> = Promise.resolve();
  private lost = { epoch: 0, error: "" };

  run<T>(
    call: () => Promise<Result<T>>,
    always = false,
  ): Promise<Result<T>> {
    const epoch = this.lost.epoch;
    const result = this.tail.then(async (): Promise<Result<T>> => {
      if (!always && this.lost.epoch !== epoch) return err(this.lost.error, ErrorCode.lostService);
      let outcome: Result<T>;
      try {
        outcome = await call();
      } catch (error) {
        return err(`fff native call failed: ${String(error)}`);
      }
      if (outcome.ok) return outcome;
      if (outcome.code === ErrorCode.lostService) {
        this.lost = { epoch: this.lost.epoch + 1, error: outcome.error };
      }
      return outcome;
    });
    this.tail = result.catch(() => undefined);
    return result;
  }
}

function page(options?: SearchOptions & FrecencyOptions) {
  return {
    current_file: options?.currentFile || null,
    offset: options?.pageIndex ?? 0,
    limit: options?.pageSize || 100,
    exclude_frecency: options?.excludeFrecency ?? false,
  };
}

/**
 * FileFinder - Fast file finder with fuzzy search
 *
 * Each instance is a session on the shared fff service. Create as many as you
 * need and destroy them when done. Calls on one instance may overlap; they
 * reach the service one at a time, in call order.
 *
 * @example
 *
 * ```ts
 * import { FileFinder } from "@sakiori/fff-node";
 *
 * // Create an instance
 * const finder = await FileFinder.create({ basePath: "/path/to/project" });
 * if (!finder.ok) {
 *   console.error(finder.error);
 *   process.exit(1);
 * }
 *
 * // Wait for initial scan
 * await finder.value.waitForScan(5000);
 *
 * // Search for files
 * const search = await finder.value.fileSearch("main.ts");
 * if (search.ok) {
 *   for (const item of search.value.items) {
 *     console.log(item.relativePath);
 *   }
 * }
 *
 * // Cleanup
 * await finder.value.destroy();
 * ```
 */
export class FileFinder {
  private handle: NativeHandle | null;
  private readonly calls = new CallQueue();
  private readonly watching: boolean;
  /** The instance's subscriptions by native id, with how to fail one still pending. */
  private readonly watches = new Map<number, (failure: Result<WatchUnsubscribe>) => void>();
  /** Registrations of the watch callback in flight, which `destroy` lets finish. */
  private readonly registrations = new Set<Promise<unknown>>();

  private constructor(handle: NativeHandle, watching: boolean) {
    this.handle = handle;
    this.watching = watching;
  }

  /**
   * Create a new file finder instance.
   *
   * Opens a session on the shared fff service, starting the service from the
   * platform package's `fff-mcp` when none of this release runs. When the
   * service cannot be started or reached within `timeoutMs` (90 seconds by
   * default), the result carries its reason.
   *
   * @param options - Initialization options
   * @returns Result containing the new FileFinder instance or an error
   *
   * @example
   * ```typescript
   * const finder = await FileFinder.create({ basePath: "/path/to/project" });
   * ```
   */
  static async create(options: ServiceInitOptions): Promise<Result<FileFinder>> {
    if (options.historyDbPath) {
      return err("historyDbPath is unavailable over the shared service", ErrorCode.unavailable);
    }
    const timeoutMs = options.timeoutMs ?? 0;
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0) {
      return err(
        "timeoutMs must be a non-negative integer number of milliseconds",
        ErrorCode.invalidRequest,
      );
    }
    let result: Result<NativeHandle>;
    try {
      result = await ffiCreate(
        options.basePath,
        options.frecencyDbPath ?? "",
        !(options.disableMmapCache ?? false),
        !(options.disableContentIndexing ?? options.disableMmapCache ?? false),
        !(options.disableWatch ?? false),
        options.aiMode ?? false,
        options.logFilePath ?? "",
        options.logLevel ?? "",
        options.cacheBudgetMaxFiles ?? 0,
        options.enableFsRootScanning ?? false,
        options.enableHomeDirScanning ?? false,
        options.followSymlinks ?? false,
        options.serviceDir ?? "",
        timeoutMs,
      );
    } catch (error) {
      return err(`fff native call failed: ${String(error)}`);
    }

    if (!result.ok) {
      return result;
    }

    return { ok: true, value: new FileFinder(result.value, !(options.disableWatch ?? false)) };
  }

  /**
   * Destroy the instance and end its service session.
   *
   * Calls made after this report the instance as destroyed; calls made before
   * it finish first. Its watch subscriptions end, and their callbacks are not
   * called once this is called. The service releases the index once no
   * session uses it. An instance that is never destroyed keeps its session
   * until the process exits.
   */
  async destroy(): Promise<void> {
    const handle = this.handle;
    if (handle === null) return;
    this.handle = null;
    for (const [watchId, settle] of this.watches) {
      deleteWatchListener(watchId);
      settle(err("FileFinder instance has been destroyed.", ErrorCode.destroyed));
    }
    this.watches.clear();
    await Promise.allSettled([...this.registrations]);
    await this.calls.run(async () => {
      await ffiDestroy(handle);
      return { ok: true, value: undefined };
    }, true);
  }

  /**
   * Check if this instance has been destroyed.
   */
  get isDestroyed(): boolean {
    return this.handle === null;
  }

  /**
   * Run a native call of this instance, or report it destroyed.
   */
  private call<T>(
    native: (handle: NativeHandle) => Promise<Result<T>>,
  ): Promise<Result<T>> {
    const handle = this.handle;
    if (handle === null) {
      return Promise.resolve(err("FileFinder instance has been destroyed.", ErrorCode.destroyed));
    }
    return this.calls.run(() => native(handle));
  }

  /**
   * Send one request of the service's program protocol.
   */
  private request<T>(request: unknown): Promise<Result<T>> {
    return this.call((handle) => ffiServiceCall<T>(handle, request));
  }

  /**
   * Search for files matching the query.
   *
   * The query supports fuzzy matching and special syntax:
   * - `foo bar` - Match files containing "foo" and "bar"
   * - `src/` - Match files in src directory
   * - `file.ts:42` - Match file.ts with line 42
   * - `file.ts:42:10` - Match file.ts with line 42, column 10
   *
   * @param query - Search query string
   * @param options - Search options
   * @returns Search results with matched files and scores
   *
   * @example
   * ```typescript
   * const result = await finder.fileSearch("main.ts", { pageSize: 10 });
   * if (result.ok) {
   *   console.log(`Found ${result.value.totalMatched} files`);
   *   for (const item of result.value.items) {
   *     console.log(item.relativePath);
   *   }
   * }
   * ```
   */
  fileSearch(
    query: string,
    options?: SearchOptions & FrecencyOptions,
  ): Promise<Result<SearchResult>> {
    return this.request({ SearchFiles: { query, ...page(options) } });
  }

  /**
   * Filters files using glob wildcard expression.
   *
   * The pattern is applied as a single pass SIMD optimized prefiltering
   * without any fuzzy matching involved. Faster and 100% compatible to npm `glob`.
   *
   * @param pattern - Glob pattern (required, non-empty)
   * @param options - Glob search options (pagination, max threads, current file)
   * @returns Search results with files matching the glob
   *
   * @example
   * ```typescript
   * const result = await finder.glob("**\/*.rs", { pageSize: 100 });
   * if (result.ok) {
   *   for (const item of result.value.items) {
   *     console.log(item.relativePath);
   *   }
   * }
   * ```
   */
  glob(pattern: string, options?: GlobOptions & FrecencyOptions): Promise<Result<SearchResult>> {
    return this.request({ Glob: { pattern, ...page(options) } });
  }

  /**
   * Search for directories matching the query.
   *
   * @param query - Search query string
   * @param options - Directory search options
   * @returns Search results with matched directories and scores
   *
   * @example
   * ```typescript
   * const result = await finder.directorySearch("src/comp", { pageSize: 10 });
   * if (result.ok) {
   *   console.log(`Found ${result.value.totalMatched} directories`);
   *   for (const item of result.value.items) {
   *     console.log(item.relativePath);
   *   }
   * }
   * ```
   */
  directorySearch(
    query: string,
    options?: DirSearchOptions & FrecencyOptions,
  ): Promise<Result<DirSearchResult>> {
    return this.request({ SearchDirectories: { query, ...page(options) } });
  }

  /**
   * Search for files and directories matching the query (mixed results).
   *
   * Results are interleaved by total score in descending order, mixing
   * both file and directory items.
   *
   * @param query - Search query string
   * @param options - Search options
   * @returns Mixed search results with files and directories interleaved by score
   *
   * @example
   * ```typescript
   * const result = await finder.mixedSearch("main", { pageSize: 20 });
   * if (result.ok) {
   *   for (const entry of result.value.items) {
   *     if (entry.type === "file") {
   *       console.log(`File: ${entry.item.relativePath}`);
   *     } else {
   *       console.log(`Dir: ${entry.item.relativePath}`);
   *     }
   *   }
   * }
   * ```
   */
  mixedSearch(
    query: string,
    options?: SearchOptions & FrecencyOptions,
  ): Promise<Result<MixedSearchResult>> {
    return this.request({ SearchMixed: { query, ...page(options) } });
  }

  /**
   * Search file contents (live grep).
   *
   * Searches through the contents of indexed files using the specified mode:
   * - `"plain"` (default): SIMD-accelerated literal text matching
   * - `"regex"`: Regular expression matching
   * - `"fuzzy"`: Smith-Waterman fuzzy matching per line
   *
   * Supports pagination for large result sets. The result includes a `nextCursor`
   * that can be passed back to fetch the next page.
   *
   * The query also supports constraint syntax:
   * - `*.ts pattern` - Only search in TypeScript files
   * - `src/ pattern` - Only search in the src directory
   *
   * A grep may take its `timeBudgetMs` beyond the instance's `timeoutMs`
   * before it fails as unanswered.
   *
   * @param query - Search query string
   * @param options - Grep options (mode, pagination, limits)
   * @returns Grep results with matched lines and file metadata
   *
   * @example
   * ```typescript
   * // First page
   * const result = await finder.grep("TODO", { mode: "plain" });
   * if (result.ok) {
   *   for (const match of result.value.items) {
   *     console.log(`${match.relativePath}:${match.lineNumber}: ${match.lineContent}`);
   *   }
   *   // Fetch next page
   *   if (result.value.nextCursor) {
   *     const page2 = await finder.grep("TODO", {
   *       cursor: result.value.nextCursor,
   *     });
   *   }
   * }
   * ```
   */
  async grep(query: string, options?: GrepOptions & FrecencyOptions): Promise<Result<GrepResult>> {
    const cursor = serviceCursor(options?.cursor);
    if (!cursor.ok) return cursor;
    const serviceOptions = grepOptions(options ?? {}, cursor.value);
    if (!serviceOptions.ok) return serviceOptions;
    const request = {
      query,
      mode: GREP_MODES[options?.mode ?? "plain"] ?? GREP_MODES.plain,
      ai_syntax: false,
      options: serviceOptions.value,
    };
    return grepResult(await this.request<GrepReply>({ Grep: request }));
  }

  /**
   * Multi-pattern OR search using Aho-Corasick.
   *
   * Searches for lines matching ANY of the provided patterns using
   * SIMD-accelerated multi-needle matching. Faster than regex alternation
   * for literal text searches.
   *
   * Supports pagination. The result includes a `nextCursor` that can be
   * passed back to fetch the next page.
   *
   * @param options - Multi-grep options including patterns and optional constraints
   * @returns Grep results with matched lines and file metadata
   *
   * @example
   * ```typescript
   * const result = await finder.multiGrep({
   *   patterns: ["VideoFrame", "video_frame", "PreloadedImage"],
   * });
   * if (result.ok) {
   *   for (const match of result.value.items) {
   *     console.log(`${match.relativePath}:${match.lineNumber}: ${match.lineContent}`);
   *   }
   * }
   * ```
   */
  async multiGrep(options: MultiGrepOptions & FrecencyOptions): Promise<Result<GrepResult>> {
    if (!options.patterns || options.patterns.length === 0) {
      return err("patterns array must have at least 1 element", ErrorCode.invalidRequest);
    }

    const cursor = serviceCursor(options.cursor);
    if (!cursor.ok) return cursor;
    const serviceOptions = grepOptions(options, cursor.value);
    if (!serviceOptions.ok) return serviceOptions;
    const request = {
      patterns: options.patterns,
      constraints: options.constraints || null,
      options: serviceOptions.value,
    };
    return grepResult(await this.request<GrepReply>({ MultiGrep: request }));
  }

  /**
   * Trigger a rescan of the shared index, coalesced with one already running.
   *
   * This is useful after major file system changes that the
   * background watcher might have missed.
   */
  async scanFiles(): Promise<Result<void>> {
    const done = await this.request<null>("Rescan");
    return done.ok ? { ok: true, value: undefined } : done;
  }

  /**
   * Apply changes the program just made to files of the instance's directory, so
   * that a search after the returned Promise resolves sees them, without waiting
   * for the service's watcher or a rescan.
   *
   * Each path, absolute or relative to the instance's directory, is added or
   * updated when it exists and the index does not ignore it, and removed, with
   * any indexed files below it, when it does not exist; the content index
   * follows. A symlink is not added unless the index follows symlinks, as its
   * scan would not add it either. Report files rather than new directories: an index without a
   * watcher does not walk a reported directory. Changes the watcher also
   * delivers are applied once more, harmlessly. While the index scans, the
   * changes are applied when the scan commits.
   *
   * @param paths - Files the program wrote, created, renamed or deleted
   * @returns Nothing once the index holds the changes; `outsideScope` when a
   *   path lies outside the instance's directory, which applies none of them
   *
   * @example
   * ```typescript
   * await writeFile(join(root, "src/new.ts"), source);
   * await finder.notifyChanged(["src/new.ts"]);
   * const found = await finder.fileSearch("new.ts"); // includes src/new.ts
   * ```
   */
  async notifyChanged(paths: string[]): Promise<Result<void>> {
    const done = await this.request<null>({ Changed: { paths } });
    return done.ok ? { ok: true, value: undefined } : done;
  }

  /**
   * Check if a scan is currently in progress, or a requested rescan waits for one.
   * An unreachable service reports false; `waitForScan` and `getScanProgress`
   * report its error.
   */
  async isScanning(): Promise<boolean> {
    const scanning = await this.scanning();
    return scanning.ok && scanning.value;
  }

  private scanning(): Promise<Result<boolean>> {
    return this.request<boolean>("IsScanning");
  }

  /**
   * Get the base path of the file picker (the root directory being indexed).
   */
  getBasePath(): Result<string | null> {
    if (this.handle === null) {
      return err("FileFinder instance has been destroyed.", ErrorCode.destroyed);
    }
    return ffiGetBasePath(this.handle);
  }

  /**
   * Get the current scan progress.
   *
   * Once the index is ready, `scannedFilesCount` counts the files of the
   * instance's directory. While the shared index scans, it counts the files
   * scanned so far, which may include files outside that directory when the
   * index covers a broader one.
   */
  getScanProgress(): Promise<Result<ScanProgress>> {
    return this.request<ScanProgress>("Progress");
  }

  /**
   * Wait for the initial file scan to complete, polling `isScanning`.
   *
   * @param timeoutMs - Maximum time to wait in milliseconds (default: 5000)
   * @returns true if scan completed, false if timed out
   *
   * @example
   * ```typescript
   * const completed = await finder.waitForScan(10000);
   * if (!completed.ok || !completed.value) {
   *   console.warn("Scan did not complete in time");
   * }
   * ```
   */
  async waitForScan(timeoutMs: number = 5000): Promise<Result<boolean>> {
    const deadline = Date.now() + timeoutMs;
    while (true) {
      const scanning = await this.scanning();
      if (!scanning.ok) return scanning;
      if (!scanning.value) return { ok: true, value: true };
      if (Date.now() >= deadline) return { ok: true, value: false };
      await new Promise((resolve) => setTimeout(resolve, READY_POLL_MS));
    }
  }

  /**
   * Wait until the index is fully ready: the scan has finished and the warmup
   * (content indexing / bigram) phase has completed. Polls `getScanProgress`.
   *
   * @param timeoutMs - Maximum time to wait in milliseconds (default: 5000)
   * @returns true if the index became ready, false if timed out
   */
  async waitForIndexReady(timeoutMs: number = 5000): Promise<Result<boolean>> {
    const deadline = Date.now() + timeoutMs;
    while (true) {
      const progress = await this.getScanProgress();
      if (!progress.ok) return progress;
      if (!progress.value.isScanning && progress.value.isWarmupComplete) {
        return { ok: true, value: true };
      }
      if (Date.now() >= deadline) {
        return { ok: true, value: false };
      }
      await new Promise((resolve) => setTimeout(resolve, READY_POLL_MS));
    }
  }

  /**
   * Rescan the indexed directory.
   *
   * A session stays on its directory, so another path fails; create a new
   * instance for it.
   *
   * @param newPath - The instance's directory
   */
  reindex(newPath: string): Promise<Result<void>> {
    return this.call((handle) => ffiRestartIndex(handle, newPath));
  }

  /**
   * Unavailable: the shared service keeps git status current itself.
   */
  refreshGitStatus(): Result<number> {
    return unavailable("refreshGitStatus");
  }

  /**
   * Unavailable: query history has no shared service equivalent.
   */
  trackQuery(_query: string, _selectedFilePath: string): Result<boolean> {
    return unavailable("trackQuery");
  }

  /**
   * Unavailable: query history has no shared service equivalent.
   */
  getHistoricalQuery(_offset: number): Result<string | null> {
    return unavailable("getHistoricalQuery");
  }

  /**
   * Subscribe to changes of the files under the instance's directory, as the
   * shared service's watcher sees them.
   *
   * `pattern` may be a glob relative to the instance's directory (`./` works),
   * an exact path, or a directory, relative or absolute inside the instance's
   * directory; without one, the subscription covers the whole directory.
   * Ignored files never trigger an event. The callback receives batches on the
   * event loop, with absolute paths. When changes are lost, because the
   * callback runs slower than they come, the index was rescanned, the session
   * moved to another index or the connection to the service broke, one
   * `rescan` event naming the instance's directory replaces them; a broken
   * connection reconnects by itself, starting the service when none runs. A
   * callback falls behind once more than 1024 of its events wait on the event
   * loop. A `failed` event ends a subscription the service can no longer
   * serve, as after its directory was removed, or that a reconnect was refused.
   * Once ready, neither the subscription nor its delivery keeps the process
   * alive, and subscribing holds none of the package's native workers.
   *
   * @returns Once every later change is delivered, the function that ends the
   *   subscription: once it returns, the callback is not called again.
   *   `unavailable` when the instance was created with `disableWatch`, and the
   *   service's reason when it refuses the subscription.
   *
   * @example
   * ```typescript
   * const watched = await finder.watch("src/**", (events) => {
   *   for (const event of events) console.log(event.kind, event.path);
   * }, { repository: true });
   * if (watched.ok) watched.value(); // unsubscribe
   * ```
   */
  watch(
    callback: ServiceWatchBatchCallback,
    options?: ServiceWatchOptions,
  ): Promise<Result<WatchUnsubscribe>>;
  watch(
    pattern: string,
    callback: ServiceWatchBatchCallback,
    options?: ServiceWatchOptions,
  ): Promise<Result<WatchUnsubscribe>>;
  watch(
    first: string | ServiceWatchBatchCallback,
    second?: ServiceWatchBatchCallback | ServiceWatchOptions,
    third?: ServiceWatchOptions,
  ): Promise<Result<WatchUnsubscribe>> {
    const [pattern, callback, options] =
      typeof first === "string"
        ? [first, second as ServiceWatchBatchCallback, third]
        : ["", first, second as ServiceWatchOptions | undefined];
    if (typeof callback !== "function") {
      return Promise.resolve(
        err("watch needs a callback for the batches", ErrorCode.invalidRequest),
      );
    }
    const handle = this.handle;
    if (handle === null) {
      return Promise.resolve(err("FileFinder instance has been destroyed.", ErrorCode.destroyed));
    }
    if (!this.watching) {
      return Promise.resolve(
        err(
          "FileFinder.watch is unavailable on an instance created with disableWatch",
          ErrorCode.unavailable,
        ),
      );
    }
    return this.subscribe(handle, pattern, callback, options ?? {});
  }

  private async subscribe(
    handle: NativeHandle,
    pattern: string,
    callback: ServiceWatchBatchCallback,
    options: ServiceWatchOptions,
  ): Promise<Result<WatchUnsubscribe>> {
    const registration = ffiRegisterWatchCallback(handle);
    this.registrations.add(registration);
    const registered = await registration;
    this.registrations.delete(registration);
    if (!registered.ok) return registered;
    if (this.handle !== handle) {
      return err("FileFinder instance has been destroyed.", ErrorCode.destroyed);
    }
    // The library returns at once and reports readiness through the callback, so no
    // native worker waits for the index.
    const started = ffiWatchStart(handle, pattern, options.ignore ?? [], options.repository ?? false);
    if (!started.ok) return started;
    const watchId = started.value;
    const base = ffiGetBasePath(handle);
    const directory = base.ok && base.value ? base.value : "";
    return new Promise((resolve) => {
      let ready = false;
      const end = () => {
        this.watches.delete(watchId);
        deleteWatchListener(watchId);
        if (this.handle === handle) ffiUnwatch(handle, watchId);
      };
      const unsubscribe: WatchUnsubscribe = () => {
        if (this.watches.has(watchId)) end();
      };
      this.watches.set(watchId, resolve);
      setWatchListener(watchId, {
        handle,
        ready: () => {
          ready = true;
          resolve({ ok: true, value: unsubscribe });
        },
        events: (events) => {
          if (ready) callback(events);
        },
        lost: () => {
          if (ready) return callback([{ path: directory, kind: "rescan" }]);
          end();
          resolve(err("The watch delivery restarted before the subscription was ready"));
        },
        failed: (error, code) => {
          end();
          if (ready) callback([{ path: directory, kind: "failed", error, code }]);
          else resolve(err(error, code));
        },
      });
    });
  }

  /**
   * Get health check information, including the instance's index and git
   * state as its service reports them.
   *
   * @param testPath - Optional path to test git repository detection
   */
  healthCheck(testPath?: string): Promise<Result<HealthCheck>> {
    return this.call(
      (handle) =>
        ffiHealthCheck(handle, testPath || "") as Promise<Result<HealthCheck>>,
    );
  }

  /**
   * Check if the native library is available.
   */
  static isAvailable(): boolean {
    return isAvailable();
  }

  /**
   * Ensure the native library is loaded.
   *
   * Loads the native library from the platform-specific npm package
   * or a local dev build. Throws if the binary is not found.
   */
  static ensureLoaded(): void {
    ensureLoaded();
  }

  /**
   * Get a health check without an instance: the library version and the git
   * repository of `testPath`, without asking a service.
   *
   * @param testPath - Optional path to test git repository detection
   */
  static healthCheckStatic(testPath?: string): Result<HealthCheck> {
    return ffiHealthCheckStatic(testPath || "") as Result<HealthCheck>;
  }
}
