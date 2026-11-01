/**
 * Options of the Node package beyond the API it shares with fff-bun.
 */

import type { ErrorCode } from "./errors.js";
import type { InitOptions, WatchEventKind, WatchOptions } from "./fff-api.js";

/**
 * Initialization options of a `FileFinder`, whose index lives in the shared
 * fff service of the package's release.
 *
 * `historyDbPath` is not supported: query history has no service equivalent.
 * `cacheBudgetMaxBytes` and `cacheBudgetMaxFileSize` have none either and are
 * ignored. `logFilePath` and `logLevel` configure a service this call starts.
 */
export interface ServiceInitOptions extends InitOptions {
  /**
   * Service namespace directory. Defaults to `FFF_MCP_SERVICE_DIR`, or the
   * per-user namespace that `fff-mcp` clients of the same release use.
   */
  serviceDir?: string;
  /**
   * How long, in milliseconds, `create` and each call wait for a service that
   * does not answer before they fail. A call may also take the time it asks
   * for itself, a grep's `timeBudgetMs`. A call that fails this way, or finds
   * the connection broken, ends the session, and the next call opens a new one
   * within its own bound.
   * (default: 90 seconds for `create`, which may start the service, and 30
   * seconds per call)
   */
  timeoutMs?: number;
}

/**
 * Per-call ranking option of searches and greps.
 */
export interface FrecencyOptions {
  /**
   * Rank as an index without a frecency database would, although the shared
   * index keeps the frecency other clients rank by. A query without fuzzy text
   * then lists entries in path order, and grep searches files newest first.
   * (default: false)
   */
  excludeFrecency?: boolean;
}

/**
 * The kind of a watch event over the shared service: the shared kinds,
 * `repository` when the git state of a repository inside the instance's
 * directory, or containing it, changed, and `refs` when one of its branches,
 * tags or remote-tracking refs changed, for subscriptions that ask for them,
 * and `failed` when the subscription ended for good after `watch` resolved: the
 * service no longer serves its directory, as after the directory was removed,
 * or refused a reconnect. No event follows a `failed` one.
 */
export type ServiceWatchEventKind = WatchEventKind | "repository" | "refs" | "failed";

/** A filesystem change, or a repository state change, of a subscription. */
export interface ServiceWatchEvent {
  /**
   * Absolute path of the affected file; the instance's directory for `rescan`
   * and `failed`, and the repository's work tree for `repository` and `refs`.
   */
  path: string;
  kind: ServiceWatchEventKind;
  /** Absolute path the file moved from. Only set when `kind == "renamed"`. */
  from?: string;
  /** Why the subscription ended. Only set when `kind == "failed"`. */
  error?: string;
  /** The code of `error`. Only set when `kind == "failed"`. */
  code?: ErrorCode;
}

/** Options of a watch subscription over the shared service. */
export interface ServiceWatchOptions extends WatchOptions {
  /**
   * Also deliver a `repository` event, naming the repository's work tree,
   * when the git state of a repository inside the instance's directory, or
   * containing it, changes: its index, as `git add` writes it, HEAD, or an
   * operation in progress such as a merge. Files under `.git` are never
   * delivered as file events. (default: false)
   */
  repository?: boolean;
  /**
   * Also deliver a `refs` event, naming the repository's work tree, when a
   * branch, tag or remote-tracking ref of a repository inside the instance's
   * directory, or containing it, is created, moved or deleted, as `git branch`,
   * `git tag`, a commit, `git fetch` or `git pack-refs` do. A linked worktree
   * shares its main repository's refs, so a change there names every work tree
   * of the repository the subscription covers. The service watches refs only
   * while a subscription asks for them. (default: false)
   */
  refs?: boolean;
}

/** Receives the batches of a subscription on the event loop. */
export type ServiceWatchBatchCallback = (events: ServiceWatchEvent[]) => void;
