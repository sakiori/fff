/**
 * Failures of `@sakiori/fff-node` and their machine-readable codes.
 *
 * Every failed `Result` carries a `code` beside its `error` text. The text is
 * for people and may change; branch on the code. Codes of failures the shared
 * service or the native library report come from the kind of error the
 * library returns, never from the text.
 */

/** The codes a failed `Result` carries. */
export const ErrorCode = {
  /**
   * The instance lost its service: the service did not answer within the
   * call's bound, could not be reached or started, or the connection broke.
   * Calls queued behind the failed one fail the same way; the instance's next
   * call opens a new session.
   */
  lostService: "lostService",
  /** The grep cursor belongs to an index the session no longer uses; search again without it. */
  staleCursor: "staleCursor",
  /** The method or option has no equivalent over the shared service. */
  unavailable: "unavailable",
  /**
   * The directory cannot be served: it is missing or inaccessible, or it is
   * the home directory or filesystem root without the option that allows
   * indexing it.
   */
  scopeUnavailable: "scopeUnavailable",
  /** The library or the service refused the request as invalid. */
  invalidRequest: "invalidRequest",
  /** The call names a path outside the instance's directory. */
  outsideScope: "outsideScope",
  /** The call met the index while its coverage changed; the same call succeeds when retried. */
  retry: "retry",
  /** The package has no native files for the running platform. */
  unsupportedPlatform: "unsupportedPlatform",
  /** The native files of the running platform are missing or cannot be loaded. */
  libraryUnavailable: "libraryUnavailable",
  /** The `FileFinder` was destroyed. */
  destroyed: "destroyed",
  /** Any other failure. */
  failed: "failed",
} as const;

export type ErrorCode = (typeof ErrorCode)[keyof typeof ErrorCode];

/** A failed result: what went wrong, for people, and its code, for programs. */
export interface Failure {
  ok: false;
  error: string;
  code: ErrorCode;
}

/** The result of every fallible call of the package. */
export type Result<T> = { ok: true; value: T } | Failure;

/** A successful result. */
export function ok<T>(value: T): Result<T> {
  return { ok: true, value };
}

/** A failed result with the given text and code. */
export function err<T = never>(error: string, code: ErrorCode = ErrorCode.failed): Result<T> {
  return { ok: false, error, code };
}

// The `FFF_ERROR_*` constants of the native library, in `int_value` of a failure.
const NATIVE_CODES: readonly ErrorCode[] = [
  ErrorCode.failed,
  ErrorCode.lostService,
  ErrorCode.staleCursor,
  ErrorCode.unavailable,
  ErrorCode.scopeUnavailable,
  ErrorCode.invalidRequest,
  ErrorCode.outsideScope,
  ErrorCode.retry,
];

/** The code of a native failure's `int_value`; values of a newer library are `failed`. */
export function nativeErrorCode(intValue: number): ErrorCode {
  return NATIVE_CODES[intValue] ?? ErrorCode.failed;
}
