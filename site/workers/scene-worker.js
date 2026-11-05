// SPDX-License-Identifier: AGPL-3.0-or-later
let instance = null;
let readyPromise = null;

async function load(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`scene module ${url}: HTTP ${response.status}`);
  const { instance: loaded } = await WebAssembly.instantiate(await response.arrayBuffer(), {});
  return loaded;
}

function takeBytes(packed) {
  const pointer = Number(packed >> 32n);
  const length = Number(packed & 0xffffffffn);
  const bytes = new Uint8Array(instance.exports.memory.buffer, pointer, length).slice();
  instance.exports.llw_free(pointer, length);
  return bytes;
}

function call(functionId, bytes) {
  const exports = instance.exports;
  const pointer = exports.llw_alloc(bytes.length);
  new Uint8Array(exports.memory.buffer, pointer, bytes.length).set(bytes);
  let packed;
  try {
    packed = exports.llw_call(functionId, pointer, bytes.length);
  } finally {
    exports.llw_free(pointer, bytes.length);
  }
  if (packed === 0n) {
    throw new Error(new TextDecoder().decode(takeBytes(exports.llw_last_error())));
  }
  return takeBytes(packed);
}

self.onmessage = async event => {
  const message = event.data;
  try {
    if (message.type === 'init') {
      readyPromise = load(message.url);
      instance = await readyPromise;
      self.postMessage({ type: 'ready', memory: instance.exports.memory.buffer.byteLength });
      return;
    }
    if (message.type !== 'call') return;
    if (readyPromise) await readyPromise;
    if (!instance) throw new Error('The scene worker has no usable module instance.');
    const bytes = call(message.fn, new Uint8Array(message.bytes));
    self.postMessage({ type: 'result', id: message.id, bytes, memory: instance.exports.memory.buffer.byteLength }, [bytes.buffer]);
  } catch (error) {
    self.postMessage({ type: 'error', id: message.id, message: String(error && error.message ? error.message : error), memory: instance ? instance.exports.memory.buffer.byteLength : 0 });
  }
};
