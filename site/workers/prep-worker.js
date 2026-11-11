// SPDX-License-Identifier: AGPL-3.0-or-later
// No imports; one file per worker at a time is the caller's rule.

let instance = null;
let poisoned = false;
let readyPromise = null;
let diagnostics = null;

const nativeStackGetter = (() => {
  try {
    for (let object = new Error(); object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, "stack");
      if (descriptor) return typeof descriptor.get === "function" ? descriptor.get : null;
    }
    return null;
  } catch {
    return null;
  }
})();

function errorTypeOf(error) {
  try {
    for (let object = error; object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, "name");
      if (descriptor) {
        return typeof descriptor.value === "string" ? descriptor.value : "Unknown";
      }
    }
  } catch {
    return "Unknown";
  }
  return "Unknown";
}

function stackOf(error) {
  if (!(error instanceof Error)) return "";
  try {
    for (let object = error; object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, "stack");
      if (!descriptor) continue;
      if (typeof descriptor.value === "string") return descriptor.value;
      if (!nativeStackGetter || descriptor.get !== nativeStackGetter) return "";
      const prepare = Object.getOwnPropertyDescriptor(Error, "prepareStackTrace");
      if (prepare && (!("value" in prepare) || prepare.value !== undefined)) return "";
      for (const key of ["name", "message"]) {
        for (let owner = error; owner; owner = Object.getPrototypeOf(owner)) {
          const property = Object.getOwnPropertyDescriptor(owner, key);
          if (!property) continue;
          if (!("value" in property) || typeof property.value !== "string") return "";
          break;
        }
      }
      const stack = nativeStackGetter.call(error);
      return typeof stack === "string" ? stack : "";
    }
  } catch {}
  return "";
}

function frameLines(stack) {
  if (typeof stack !== "string") return "";
  return stack
    .split("\n")
    .filter((line) => {
      const text = line.trim();
      return (
        text.startsWith("at ") ||
        /^\S*@[^\s@]+:\d+:\d+$/.test(text) ||
        /wasm-function\[\d+\]:0x[0-9a-f]+/i.test(text)
      );
    })
    .join("\n");
}

function describe(error) {
  if (!(error instanceof Error)) return { errorType: "Unknown", stack: "" };
  return { errorType: errorTypeOf(error), stack: frameLines(stackOf(error)) };
}

function reportClientError(kind, error, location) {
  if (!diagnostics) return;
  try {
    const { errorType, stack } = describe(error);
    self.postMessage({
      type: "client-error",
      kind,
      errorType,
      stack,
      wasmAsset: diagnostics.wasmAsset,
      asset: location && typeof location.asset === "string" ? location.asset : "",
      line: location && Number.isInteger(location.line) ? location.line : 0,
      column: location && Number.isInteger(location.column) ? location.column : 0,
    });
  } catch {}
}

self.addEventListener("error", (event) => {
  reportClientError("exception", event.error, {
    asset: event.filename,
    line: event.lineno,
    column: event.colno,
  });
});

self.addEventListener("unhandledrejection", (event) => {
  reportClientError("unhandled_rejection", event.reason, null);
});

async function load(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`prep module ${url}: HTTP ${response.status}`);
  }
  const bytes = await response.arrayBuffer();
  const { instance: loaded } = await WebAssembly.instantiate(bytes, {});
  return loaded;
}

function memoryBytes() {
  return instance ? instance.exports.memory.buffer.byteLength : 0;
}

function call(fnId, bytes) {
  const exports = instance.exports;
  const length = bytes.length;
  const inPtr = exports.ll_alloc(length);
  new Uint8Array(exports.memory.buffer, inPtr, length).set(bytes);
  const packed = exports.ll_call(fnId, inPtr, length);
  exports.ll_free(inPtr, length);
  const outPtr = Number(packed >> 32n);
  const outLen = Number(packed & 0xffffffffn);
  const out = new Uint8Array(exports.memory.buffer, outPtr, outLen).slice();
  exports.ll_free(outPtr, outLen);
  return out;
}

self.onmessage = async (event) => {
  const message = event.data;
  try {
    if (message.type === "init") {
      if (message.diagnostics) {
        diagnostics = {
          wasmAsset: typeof message.url === "string" ? message.url : "",
        };
      }
      readyPromise = load(message.url);
      instance = await readyPromise;
      poisoned = false;
      self.postMessage({ type: "ready", memory: memoryBytes() });
      return;
    }
    if (message.type !== "call") {
      return;
    }
    if (readyPromise) {
      await readyPromise;
    }
    if (poisoned || !instance) {
      throw new Error("the prep worker has no usable module instance");
    }
    const out = call(message.fn, new Uint8Array(message.bytes));
    self.postMessage(
      { type: "result", id: message.id, bytes: out, memory: memoryBytes() },
      [out.buffer],
    );
  } catch (error) {
    console.error("worker error", error);
    reportClientError("console_error", error, null);
    poisoned = true;
    self.postMessage({
      type: "error",
      id: message.id,
      message: String(error && error.message ? error.message : error),
      trap: error instanceof WebAssembly.RuntimeError,
      memory: memoryBytes(),
    });
  }
};
