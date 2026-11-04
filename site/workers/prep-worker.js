// SPDX-License-Identifier: AGPL-3.0-or-later
// No imports; one file per worker at a time is the caller's rule.

let instance = null;
let poisoned = false;
let readyPromise = null;

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
