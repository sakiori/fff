/**
 * The worker threads that run the package's blocking native calls.
 *
 * Calls that reach the shared service block their thread until the service
 * answers or the call's time limit passes. They run here, on a few worker
 * threads the package owns, rather than on the event loop or on the libuv
 * thread pool, whose threads the file system, DNS, crypto and zlib APIs of the
 * whole process share. Workers start when a call needs one, and hold the
 * process open only while they run a call; one that dies fails its call and is
 * replaced by the next call that needs a worker.
 */

import { MessageChannel, type MessagePort, Worker } from "node:worker_threads";
import { ErrorCode, type Failure, nativeErrorCode } from "./errors.js";
import { NATIVE_WORKER_SOURCE } from "./native-worker.js";

/**
 * The most worker threads the package runs. Each instance has at most one
 * call in flight, so this many instances can reach the service at once; an
 * unresponsive service holds at most this many threads until its calls' time
 * limits pass, and nothing outside the package waits for them.
 */
export const NATIVE_POOL_SIZE = 4;

/** The reply of a native call. */
export type NativeReply<T> = { ok: true; value: T } | Failure;

// A worker's reply, with a failure's `FFF_ERROR_*` kind as `nativeCode`.
type WorkerReply = { ok: true; value: unknown } | { ok: false; error: string; nativeCode: number };

/** What every worker needs to load the native library. */
export interface NativeSetup {
  ffiPath: string;
  library: string;
  createOptions: unknown;
}

interface Job {
  operation: string;
  args: unknown[];
  settle: (reply: NativeReply<unknown>) => void;
}

interface Slot {
  worker: Worker;
  // Calls use a channel of their own: Node posts its own messages to a worker's
  // parent port, such as the modules it requires under `--watch`.
  port: MessagePort;
  job: Job | null;
}

class NativePool {
  private readonly slots: Slot[] = [];
  private readonly waiting: Job[] = [];
  private nextId = 0;

  constructor(private readonly setup: () => NativeSetup) {}

  run<T>(operation: string, args: unknown[]): Promise<NativeReply<T>> {
    return new Promise((resolve) => {
      this.waiting.push({
        operation,
        args,
        settle: resolve as (reply: NativeReply<unknown>) => void,
      });
      this.dispatch();
    });
  }

  /** Stops every worker; calls in flight fail, and later calls start new workers. */
  async close(): Promise<void> {
    for (const job of this.waiting.splice(0)) {
      job.settle({ ok: false, error: "fff native workers were closed", code: ErrorCode.failed });
    }
    await Promise.all(this.slots.map((slot) => slot.worker.terminate()));
  }

  private dispatch(): void {
    while (this.waiting.length > 0) {
      const slot = this.slots.find((candidate) => candidate.job === null) ?? this.start();
      if (!slot) return;
      const job = this.waiting.shift() as Job;
      slot.job = job;
      slot.worker.ref();
      slot.port.ref();
      slot.port.postMessage({ id: this.nextId++, operation: job.operation, args: job.args });
    }
  }

  private start(): Slot | null {
    if (this.slots.length >= NATIVE_POOL_SIZE) return null;
    const { port1: port, port2: workerPort } = new MessageChannel();
    const worker = new Worker(NATIVE_WORKER_SOURCE, {
      eval: true,
      // The host's flags, as --input-type=module, would change how the source is read.
      execArgv: [],
      workerData: { ...this.setup(), port: workerPort },
      transferList: [workerPort],
    });
    worker.unref();
    port.unref();
    const slot: Slot = { worker, port, job: null };
    port.on("message", ({ reply }: { reply: WorkerReply }) => {
      const job = slot.job;
      slot.job = null;
      worker.unref();
      port.unref();
      job?.settle(
        reply.ok
          ? reply
          : { ok: false, error: reply.error, code: nativeErrorCode(reply.nativeCode) },
      );
      this.dispatch();
    });
    let failure = "";
    worker.on("error", (error) => {
      failure = String(error);
    });
    worker.on("exit", (code) => {
      port.close();
      this.slots.splice(this.slots.indexOf(slot), 1);
      const job = slot.job;
      slot.job = null;
      job?.settle({
        ok: false,
        error: `fff native worker exited (${failure || `code ${code}`})`,
        code: ErrorCode.failed,
      });
      this.dispatch();
    });
    this.slots.push(slot);
    return slot;
  }
}

let pool: NativePool | null = null;

/** Run one native operation on a worker thread. */
export function runNative<T>(
  setup: () => NativeSetup,
  operation: string,
  args: unknown[],
): Promise<NativeReply<T>> {
  pool ??= new NativePool(setup);
  return pool.run<T>(operation, args);
}

/** Stop the worker threads; the next native call starts new ones. */
export async function closeNativePool(): Promise<void> {
  const closing = pool;
  pool = null;
  await closing?.close();
}
