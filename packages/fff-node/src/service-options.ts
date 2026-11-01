/**
 * Options of the Node package beyond the API it shares with fff-bun.
 */

import type { InitOptions } from "./fff-api.js";

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
