// SPDX-License-Identifier: AGPL-3.0-or-later
let instance = null;
let readyPromise = null;

async function load(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`match module ${url}: HTTP ${response.status}`);
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
  const inPtr = exports.llw_alloc(length);
  new Uint8Array(exports.memory.buffer, inPtr, length).set(bytes);
  const packed = exports.llw_call(fnId, inPtr, length);
  exports.llw_free(inPtr, length);
  const outPtr = Number(packed >> 32n);
  const outLen = Number(packed & 0xffffffffn);
  const out = new Uint8Array(exports.memory.buffer, outPtr, outLen).slice();
  exports.llw_free(outPtr, outLen);
  return out;
}

self.onmessage = async (event) => {
  const message = event.data;
  try {
    if (message.type === "init") {
      readyPromise = load(message.url);
      instance = await readyPromise;
      self.postMessage({ type: "ready", memory: memoryBytes() });
      return;
    }
    if (message.type !== "call") {
      return;
    }
    if (readyPromise) {
      await readyPromise;
    }
    if (!instance) {
      throw new Error("the match worker has no usable module instance");
    }
    const out = call(message.fn, new Uint8Array(message.bytes));
    self.postMessage(
      { type: "result", id: message.id, bytes: out, memory: memoryBytes() },
      [out.buffer],
    );
  } catch (error) {
    console.error("worker error", error);
    self.postMessage({
      type: "error",
      id: message.id,
      message: String(error && error.message ? error.message : error),
      memory: memoryBytes(),
    });
  }
};
