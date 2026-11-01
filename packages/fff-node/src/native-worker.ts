/**
 * Source of the worker threads that make the blocking calls into libfff_c.
 *
 * It runs as an `eval` worker, a CommonJS script, so it needs no file of its
 * own next to the bundle. The main thread passes the paths it resolved, the
 * ffi-rs entry and the native library (an `.asar.unpacked` path inside
 * Electron archives), and the layout of the create options in `workerData`.
 * Calls arrive on the private port in `workerData`, never on the parent port,
 * which Node also uses for messages of its own. Each message is one call; the
 * reply carries plain values only: the instance handle as a BigInt address, and
 * a failure's `FFF_ERROR_*` kind as `nativeCode`.
 */
export const NATIVE_WORKER_SOURCE = String.raw`
"use strict";
const { workerData } = require("node:worker_threads");
const port = workerData.port;
const ffi = require(workerData.ffiPath);
const { DataType } = ffi;
const LIBRARY = "fff_c";
ffi.open({ library: LIBRARY, path: workerData.library });

const RESULT = {
  success: DataType.U8,
  error: DataType.External,
  handle: DataType.External,
  int_value: DataType.I64,
};
const HANDLE_RESULT = { ...RESULT, handle: DataType.BigInt };
const CREATE_OPTIONS = workerData.createOptions;

function call(funcName, paramsType, paramsValue) {
  return ffi.load({
    library: LIBRARY,
    funcName,
    retType: DataType.External,
    paramsType,
    paramsValue,
    freeResultMemory: false,
  });
}

function free(funcName, pointer) {
  ffi.load({
    library: LIBRARY,
    funcName,
    retType: DataType.Void,
    paramsType: [DataType.External],
    paramsValue: [pointer],
  });
}

function cString(pointer) {
  if (ffi.isNullPointer(pointer)) return null;
  const [text] = ffi.restorePointer({
    retType: [DataType.String],
    paramsValue: ffi.wrapPointer([pointer]),
  });
  return text;
}

// Reads an FffResult, frees it and returns a reply; payload reads its handle field.
function envelope(pointer, layout, payload) {
  const [result] = ffi.restorePointer({ retType: [layout], paramsValue: ffi.wrapPointer([pointer]) });
  if (result.success === 0) {
    const error = cString(result.error) || "Unknown error";
    free("fff_free_result", pointer);
    return { ok: false, error, nativeCode: Number(result.int_value) };
  }
  const value = payload(result.handle);
  free("fff_free_result", pointer);
  return { ok: true, value };
}

function stringPayload(handle) {
  if (ffi.isNullPointer(handle)) return null;
  const text = cString(handle);
  free("fff_free_string", handle);
  return text;
}

const operations = {
  create(options, executable, serviceDir, timeoutMs) {
    const pointer = call(
      "fff_create_service_instance",
      [CREATE_OPTIONS, DataType.String, DataType.String, DataType.U64],
      [options, executable, serviceDir, timeoutMs],
    );
    return envelope(pointer, HANDLE_RESULT, (handle) => handle);
  },
  call(handle, request) {
    const pointer = call("fff_service_call", [DataType.BigInt, DataType.String], [handle, request]);
    return envelope(pointer, RESULT, stringPayload);
  },
  restart(handle, path) {
    const pointer = call("fff_restart_index", [DataType.BigInt, DataType.String], [handle, path]);
    return envelope(pointer, RESULT, () => null);
  },
  health(handle, testPath) {
    const pointer = call("fff_health_check", [DataType.BigInt, DataType.String], [handle, testPath]);
    return envelope(pointer, RESULT, stringPayload);
  },
  destroy(handle) {
    ffi.load({
      library: LIBRARY,
      funcName: "fff_destroy",
      retType: DataType.Void,
      paramsType: [DataType.BigInt],
      paramsValue: [handle],
    });
    return { ok: true, value: null };
  },
};

port.on("message", ({ id, operation, args }) => {
  let reply;
  try {
    reply = operations[operation](...args);
  } catch (error) {
    reply = { ok: false, error: "fff native call failed: " + String(error), nativeCode: 0 };
  }
  port.postMessage({ id, reply });
});
`;
