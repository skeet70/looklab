// SPDX-License-Identifier: AGPL-3.0-or-later
let instance = null;
let readyPromise = null;
let diagnostics = null;

const nativeStackGetter = (() => {
  try {
    for (let object = new Error(); object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, 'stack');
      if (descriptor) return typeof descriptor.get === 'function' ? descriptor.get : null;
    }
    return null;
  } catch {
    return null;
  }
})();

function errorTypeOf(error) {
  try {
    for (let object = error; object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, 'name');
      if (descriptor) {
        return typeof descriptor.value === 'string' ? descriptor.value : 'Unknown';
      }
    }
  } catch {
    return 'Unknown';
  }
  return 'Unknown';
}

function stackOf(error) {
  if (!(error instanceof Error)) return '';
  try {
    for (let object = error; object; object = Object.getPrototypeOf(object)) {
      const descriptor = Object.getOwnPropertyDescriptor(object, 'stack');
      if (!descriptor) continue;
      if (typeof descriptor.value === 'string') return descriptor.value;
      if (!nativeStackGetter || descriptor.get !== nativeStackGetter) return '';
      const prepare = Object.getOwnPropertyDescriptor(Error, 'prepareStackTrace');
      if (prepare && (!('value' in prepare) || prepare.value !== undefined)) return '';
      for (const key of ['name', 'message']) {
        for (let owner = error; owner; owner = Object.getPrototypeOf(owner)) {
          const property = Object.getOwnPropertyDescriptor(owner, key);
          if (!property) continue;
          if (!('value' in property) || typeof property.value !== 'string') return '';
          break;
        }
      }
      const stack = nativeStackGetter.call(error);
      return typeof stack === 'string' ? stack : '';
    }
  } catch {}
  return '';
}

function frameLines(stack) {
  if (typeof stack !== 'string') return '';
  return stack
    .split('\n')
    .filter(line => {
      const text = line.trim();
      return (
        text.startsWith('at ') ||
        /^\S*@[^\s@]+:\d+:\d+$/.test(text) ||
        /wasm-function\[\d+\]:0x[0-9a-f]+/i.test(text)
      );
    })
    .join('\n');
}

function describe(error) {
  if (!(error instanceof Error)) return { errorType: 'Unknown', stack: '' };
  return { errorType: errorTypeOf(error), stack: frameLines(stackOf(error)) };
}

function reportClientError(kind, error, location) {
  if (!diagnostics) return;
  try {
    const { errorType, stack } = describe(error);
    self.postMessage({
      type: 'client-error',
      kind,
      errorType,
      stack,
      wasmAsset: diagnostics.wasmAsset,
      asset: location && typeof location.asset === 'string' ? location.asset : '',
      line: location && Number.isInteger(location.line) ? location.line : 0,
      column: location && Number.isInteger(location.column) ? location.column : 0,
    });
  } catch {}
}

self.addEventListener('error', event => {
  reportClientError('exception', event.error, {
    asset: event.filename,
    line: event.lineno,
    column: event.colno,
  });
});

self.addEventListener('unhandledrejection', event => {
  reportClientError('unhandled_rejection', event.reason, null);
});

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
      if (message.diagnostics) {
        diagnostics = {
          wasmAsset: typeof message.url === 'string' ? message.url : '',
        };
      }
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
    reportClientError('exception', error, null);
    self.postMessage({ type: 'error', id: message.id, message: String(error && error.message ? error.message : error), memory: instance ? instance.exports.memory.buffer.byteLength : 0 });
  }
};
