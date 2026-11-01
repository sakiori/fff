/**
 * Delivery of watch batches from the native library to the event loop.
 *
 * The library calls one process-wide callback, from a thread of each
 * subscription. ffi-rs turns a JS function into such a callback through a
 * threadsafe function, which keeps the event loop it belongs to alive for as
 * long as it exists. It therefore lives on a worker thread of its own that the
 * package unrefs: the worker reads each batch, frees it, and posts its events
 * to the main thread, where they reach the subscription's listener. Neither the
 * worker nor a subscription keeps the process alive.
 *
 * The library never waits for the callback, so the worker bounds what the main
 * thread has yet to handle: past `PENDING_LIMIT` events of one subscription it
 * drops that subscription's events, and once the main thread has handled what
 * was posted, it posts one `lost` message in their place.
 */

import { Worker } from "node:worker_threads";
import { type ErrorCode, err, nativeErrorCode, type Result } from "./errors.js";
import type { ServiceWatchEvent } from "./service-options.js";

/** Events of one subscription posted to the main thread and not handled yet, at most. */
export const PENDING_LIMIT = 1024;

/** What the delivery worker loads: the ffi-rs entry and the native library. */
export interface DeliverySetup {
  ffiPath: string;
  library: string;
}

/** What a subscription hears, on the event loop. */
export interface WatchListener {
  /** The native instance the subscription belongs to. */
  handle: bigint;
  ready(): void;
  events(events: ServiceWatchEvent[]): void;
  /**
   * Events were dropped: the subscription's listener fell behind, or delivery restarted,
   * which may also have dropped the readiness of a subscription not ready yet.
   */
  lost(): void;
  /** The subscription ended for good. */
  failed(error: string, code: ErrorCode): void;
}

type Reply = { ok: true } | { ok: false; error: string; nativeCode: number };

type Message =
  | { kind: "events"; watchId: number; events: ServiceWatchEvent[] }
  | { kind: "ready" | "lost"; watchId: number }
  | { kind: "failed"; watchId: number; error: string; nativeCode: number }
  | { kind: "reply"; id: number; reply: Reply };

const listeners = new Map<number, WatchListener>();
// Instances whose callback points at the worker's trampoline, by native address.
const registered = new Map<bigint, Promise<Result<void>>>();
const pending = new Map<number, (reply: Result<void>) => void>();
let nextRequest = 0;
let worker: Worker | null = null;
let deliverySetup: (() => DeliverySetup) | null = null;
let closing = false;

function deliveryWorker(setup: () => DeliverySetup): Worker {
  if (worker) return worker;
  deliverySetup = setup;
  const started = new Worker(DELIVERY_WORKER_SOURCE, {
    eval: true,
    // The host's flags, as --input-type=module, would change how the source is read.
    execArgv: [],
    workerData: { ...setup(), pendingLimit: PENDING_LIMIT },
  });
  started.on("message", (message: Message) => {
    if (message.kind === "reply") {
      const settle = pending.get(message.id);
      pending.delete(message.id);
      refWhileWaiting();
      const reply = message.reply;
      settle?.(
        reply.ok
          ? { ok: true, value: undefined }
          : err(reply.error, nativeErrorCode(reply.nativeCode)),
      );
      return;
    }
    const listener = listeners.get(message.watchId);
    if (message.kind !== "events") {
      waitingReady.delete(message.watchId);
      refWhileWaiting();
    }
    try {
      switch (message.kind) {
        case "events":
          if (listener) listener.events(message.events);
          break;
        case "ready":
          listener?.ready();
          break;
        case "lost":
          listener?.lost();
          break;
        case "failed":
          listener?.failed(message.error, nativeErrorCode(message.nativeCode));
          break;
      }
    } catch {
      // A failing callback must not end delivery to the others.
    } finally {
      if (message.kind === "events") {
        started.postMessage({ ack: message.watchId, count: message.events.length });
      }
    }
  });
  let failure = "";
  started.on("error", (error) => {
    failure = String(error);
  });
  started.on("exit", (code) => {
    if (worker === started) worker = null;
    registered.clear();
    for (const settle of pending.values()) {
      settle(err(`fff watch delivery worker exited (${failure || `code ${code}`})`));
    }
    pending.clear();
    if (!closing) restart();
  });
  worker = started;
  // After the listeners: adding a message listener refs the worker again.
  refWhileWaiting();
  return started;
}

// Subscriptions whose readiness the main thread awaits.
const waitingReady = new Set<number>();

// A registration or a readiness in flight keeps the process alive, as other calls do while
// they run; a subscription that is ready does not.
function refWhileWaiting(): void {
  if (pending.size > 0 || waitingReady.size > 0) worker?.ref();
  else worker?.unref();
}

// Batches in flight died with the worker: the instances that still watch point at a new
// worker, then every subscription hears a loss.
function restart(): void {
  const setup = deliverySetup;
  if (!setup || listeners.size === 0) return;
  const handles = new Set([...listeners.values()].map((listener) => listener.handle));
  void Promise.all([...handles].map((handle) => registerWatchCallback(setup, handle))).then(
    () => {
      for (const listener of [...listeners.values()]) {
        try {
          listener.lost();
        } catch {
          // As above.
        }
      }
    },
  );
}

/**
 * Point the instance's watch callback at the delivery worker, once per instance.
 */
export function registerWatchCallback(
  setup: () => DeliverySetup,
  handle: bigint,
): Promise<Result<void>> {
  const known = registered.get(handle);
  if (known) return known;
  const target = deliveryWorker(setup);
  const id = nextRequest++;
  const reply = new Promise<Result<void>>((resolve) => {
    pending.set(id, resolve);
    refWhileWaiting();
    target.postMessage({ id, handle });
  }).then((result) => {
    if (!result.ok) registered.delete(handle);
    return result;
  });
  registered.set(handle, reply);
  return reply;
}

/** Forget an instance the library freed; its address may be reused. */
export function forgetWatchCallback(handle: bigint): void {
  registered.delete(handle);
}

/** Listen to a subscription that is not ready yet. */
export function setWatchListener(watchId: number, listener: WatchListener): void {
  listeners.set(watchId, listener);
  waitingReady.add(watchId);
  refWhileWaiting();
}

/** Once this returns, the listener is never called again. */
export function deleteWatchListener(watchId: number): void {
  listeners.delete(watchId);
  waitingReady.delete(watchId);
  refWhileWaiting();
  worker?.postMessage({ forget: watchId });
}

/**
 * Makes the delivery worker exit as a crash would, for tests of the restart. Internal: the
 * package's entry does not export it.
 */
export function exitDeliveryWorkerForTest(): void {
  worker?.postMessage({ exit: true });
}

/** Stop the delivery worker; every instance must have been destroyed. */
export async function closeWatchDelivery(): Promise<void> {
  const stopping = worker;
  worker = null;
  registered.clear();
  closing = true;
  try {
    await stopping?.terminate();
  } finally {
    closing = false;
  }
}

/**
 * Source of the delivery worker, an `eval` worker like the native workers.
 * Kinds follow the library's `kind` byte.
 */
const DELIVERY_WORKER_SOURCE = String.raw`
"use strict";
const { parentPort, workerData } = require("node:worker_threads");
const ffi = require(workerData.ffiPath);
const { DataType } = ffi;
const LIBRARY = "fff_c";
const PENDING_LIMIT = workerData.pendingLimit;
ffi.open({ library: LIBRARY, path: workerData.library });

const KINDS = ["created", "modified", "removed", "rescan", "renamed", "repository", "failed", "ready"];
const RESULT = {
  success: DataType.U8,
  error: DataType.External,
  handle: DataType.External,
  int_value: DataType.I64,
};

function load(funcName, retType, paramsType, paramsValue) {
  return ffi.load({ library: LIBRARY, funcName, retType, paramsType, paramsValue });
}

function cString(pointer) {
  if (ffi.isNullPointer(pointer)) return null;
  const [text] = ffi.restorePointer({
    retType: [DataType.String],
    paramsValue: ffi.wrapPointer([pointer]),
  });
  return text;
}

// Reads a batch the library handed over; the caller frees it.
function read(batch) {
  const count = load("fff_watch_events_count", DataType.U32, [DataType.External], [batch]);
  const events = [];
  for (let i = 0; i < count; i++) {
    const at = [DataType.External, DataType.U32];
    const path = cString(load("fff_watch_events_get_path", DataType.External, at, [batch, i])) ?? "";
    const kind = KINDS[load("fff_watch_events_get_kind", DataType.U8, at, [batch, i])] ?? "rescan";
    const event = { path, kind };
    if (kind === "renamed") {
      const from = cString(load("fff_watch_events_get_from_path", DataType.External, at, [batch, i]));
      if (from) event.from = from;
    }
    events.push(event);
  }
  return events;
}

// Per subscription: events posted and not acknowledged, and whether some were dropped.
const flow = new Map();

function deliver(watchId, batch) {
  const events = read(batch);
  if (events.length === 0) return;
  const kind = events[0].kind;
  if (kind === "ready") return parentPort.postMessage({ kind, watchId });
  if (kind === "failed") {
    const error = cString(load("fff_watch_events_get_error", DataType.External, [DataType.External], [batch]));
    const nativeCode = Number(load("fff_watch_events_get_error_code", DataType.I64, [DataType.External], [batch]));
    flow.delete(watchId);
    return parentPort.postMessage({ kind, watchId, error: error || "The watch subscription ended", nativeCode });
  }
  const state = flow.get(watchId) ?? { posted: 0, dropping: false };
  flow.set(watchId, state);
  if (state.dropping) return;
  if (state.posted + events.length > PENDING_LIMIT) {
    state.dropping = true;
    if (state.posted === 0) lost(watchId, state);
    return;
  }
  state.posted += events.length;
  parentPort.postMessage({ kind: "events", watchId, events });
}

function lost(watchId, state) {
  state.dropping = false;
  parentPort.postMessage({ kind: "lost", watchId });
}

const TRAMPOLINE = ffi.funcConstructor({
  paramsType: [DataType.U64, DataType.U64, DataType.U64],
  retType: DataType.Void,
});
const trampoline = ffi.createPointer({
  paramsType: [TRAMPOLINE],
  paramsValue: [
    (rawId, address) => {
      const watchId = Number(rawId);
      try {
        const batch = load("fff_ptr_offset", DataType.External, [DataType.U64, DataType.U64], [address, 0]);
        deliver(watchId, batch);
      } catch {
        parentPort.postMessage({ kind: "lost", watchId });
      } finally {
        try {
          load("fff_free_watch_events", DataType.Void, [DataType.U64], [address]);
        } catch {
          // Nothing more can be done for this batch.
        }
      }
    },
  ],
});
const callback = ffi.unwrapPointer(trampoline)[0];

function register(handle) {
  const pointer = ffi.load({
    library: LIBRARY,
    funcName: "fff_set_watch_callback",
    retType: DataType.External,
    paramsType: [DataType.BigInt, DataType.External, DataType.U64],
    paramsValue: [handle, callback, 0],
    freeResultMemory: false,
  });
  const [result] = ffi.restorePointer({ retType: [RESULT], paramsValue: ffi.wrapPointer([pointer]) });
  const reply =
    result.success === 0
      ? { ok: false, error: cString(result.error) || "Unknown error", nativeCode: Number(result.int_value) }
      : { ok: true };
  load("fff_free_result", DataType.Void, [DataType.External], [pointer]);
  return reply;
}

parentPort.on("message", (message) => {
  if (message.exit) process.exit(1);
  if (message.forget !== undefined) {
    flow.delete(message.forget);
    return;
  }
  if (message.ack !== undefined) {
    const state = flow.get(message.ack);
    if (!state) return;
    state.posted -= message.count;
    if (state.dropping && state.posted === 0) lost(message.ack, state);
    return;
  }
  let reply;
  try {
    reply = register(message.handle);
  } catch (error) {
    reply = { ok: false, error: "fff native call failed: " + String(error), nativeCode: 0 };
  }
  parentPort.postMessage({ kind: "reply", id: message.id, reply });
});
`;
