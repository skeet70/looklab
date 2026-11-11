// SPDX-License-Identifier: AGPL-3.0-or-later
// First-party failure reports for the static site, loaded before the WASM module.
// Capture and transport failures stay silent; no diagnostic path reports its own failure.

(() => {
  "use strict";

  const VERSION = 1;
  const REPORT_LIMIT = 20;
  const DEDUP_LIMIT = 64;
  const FRAME_LIMIT = 12;
  const U32_MAX = 0xffffffff;
  const DEFAULT_WASM = "pkg/looklab-site.wasm";
  const KINDS = new Set([
    "console_error",
    "exception",
    "unhandled_rejection",
    "resource_error",
  ]);
  const ERROR_TYPES = new Set([
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "URIError",
    "EvalError",
    "RuntimeError",
    "CompileError",
    "LinkError",
    "Unknown",
  ]);
  const PAGES = new Set([
    "home",
    "looks",
    "look",
    "compare",
    "cameras",
    "camera",
    "match",
    "contribute",
    "me",
    "status",
    "community",
    "moderation",
    "about",
    "other",
  ]);
  const WORKER_KINDS = new Set([
    "console_error",
    "exception",
    "unhandled_rejection",
  ]);
  const ASSET_PATTERNS = [
    /^pkg\/[A-Za-z0-9_-][A-Za-z0-9_.-]*\.js$/,
    /^pkg\/[A-Za-z0-9_-][A-Za-z0-9_.-]*\.wasm$/,
    /^workers\/(?:prep|match|scene)-worker\.js$/,
    /^errors\.js$/,
    /^generated\/wasm\/looklab_prep_module\.wasm$/,
    /^generated\/wasm\/looklab_scene_module\.wasm$/,
    /^generated\/wasm\/looklab_match\.wasm$/,
  ];
  const NATIVE_STACK_GETTER = (() => {
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

  const script = document.currentScript;
  const config = script && script.dataset ? script.dataset : {};
  const release = /^[0-9a-f]{64}$/.test(config.release || "") ? config.release : "";
  const preview = config.preview === "1";
  const apiBase = (config.apiBase || "").trim().replace(/\/+$/, "");
  const enabled = !preview && release !== "";
  const endpoint = apiBase ? `${apiBase}/api/client-errors` : "/api/client-errors";

  let prefix = "/";
  try {
    const source = new URL(script.src, location.href).pathname;
    prefix = source.slice(0, source.lastIndexOf("/") + 1);
  } catch {}

  let sent = 0;
  const seen = new Set();

  function toU32(value) {
    return Number.isInteger(value) && value >= 0 && value <= U32_MAX ? value : null;
  }

  function assetOf(value) {
    if (typeof value !== "string" || value === "") return null;
    if (ASSET_PATTERNS.some((pattern) => pattern.test(value))) return value;
    let pathname;
    try {
      const url = new URL(value, location.href);
      if (url.origin !== location.origin) return null;
      pathname = url.pathname;
    } catch {
      return null;
    }
    if (prefix !== "/" && pathname.startsWith(prefix)) {
      pathname = pathname.slice(prefix.length);
    } else if (pathname.startsWith("/")) {
      pathname = pathname.slice(1);
    }
    return ASSET_PATTERNS.some((pattern) => pattern.test(pathname)) ? pathname : null;
  }

  function locationIn(line) {
    const text = line.trim().replace(/\)$/, "").trim();
    const match = /^(.*):(\d+):(\d+)$/.exec(text);
    if (!match) return null;
    let candidate = match[1];
    const open = candidate.lastIndexOf("(");
    if (open >= 0) {
      candidate = candidate.slice(open + 1);
    } else {
      const at = candidate.lastIndexOf(" at ");
      if (at >= 0) {
        candidate = candidate.slice(at + 4);
      } else if (candidate.startsWith("at ")) {
        candidate = candidate.slice(3);
      }
      const atSign = candidate.lastIndexOf("@");
      if (atSign >= 0) candidate = candidate.slice(atSign + 1);
    }
    if (candidate.startsWith("async ")) candidate = candidate.slice(6);
    candidate = candidate.trim();
    if (candidate === "") return null;
    return {
      url: candidate,
      line: Number(match[2]),
      column: Number(match[3]),
    };
  }

  function wasmAssetIn(line) {
    const marker = line.indexOf(":wasm-function[");
    if (marker < 0) return null;
    let candidate = line.slice(0, marker).trim();
    const open = candidate.lastIndexOf("(");
    if (open >= 0) {
      candidate = candidate.slice(open + 1);
    } else {
      const at = candidate.lastIndexOf(" at ");
      if (at >= 0) {
        candidate = candidate.slice(at + 4);
      } else if (candidate.startsWith("at ")) {
        candidate = candidate.slice(3);
      }
      const atSign = candidate.lastIndexOf("@");
      if (atSign >= 0) candidate = candidate.slice(atSign + 1);
    }
    if (candidate.startsWith("async ")) candidate = candidate.slice(6);
    return assetOf(candidate.trim());
  }

  function frameFromLine(line, defaultWasm) {
    const wasm = /wasm-function\[(\d+)\]:0x([0-9a-f]+)/i.exec(line);
    if (wasm) {
      const functionIndex = toU32(Number(wasm[1]));
      const offset = toU32(Number.parseInt(wasm[2], 16));
      if (functionIndex === null || offset === null) return null;
      const asset = wasmAssetIn(line) || defaultWasm;
      return asset ? { asset, line: functionIndex, column: offset } : null;
    }
    const location = locationIn(line);
    if (!location) return null;
    const asset = assetOf(location.url);
    if (!asset || asset === "errors.js") return null;
    const number = toU32(location.line);
    const column = toU32(location.column);
    if (number === null || column === null) return null;
    return { asset, line: number, column };
  }

  function isFrameLine(line) {
    const text = line.trim();
    return (
      text.startsWith("at ") ||
      /^\S*@[^\s@]+:\d+:\d+$/.test(text) ||
      /wasm-function\[\d+\]:0x[0-9a-f]+/i.test(text)
    );
  }

  function framesFromStack(stack, defaultWasm) {
    const frames = [];
    if (typeof stack !== "string" || stack === "") return frames;
    for (const line of stack.split("\n")) {
      if (frames.length >= FRAME_LIMIT) break;
      if (!isFrameLine(line)) continue;
      const frame = frameFromLine(line, defaultWasm);
      if (frame) frames.push(frame);
    }
    return frames;
  }

  function errorTypeOf(value) {
    if (!(value instanceof Error)) return "Unknown";
    try {
      for (let object = value; object; object = Object.getPrototypeOf(object)) {
        const descriptor = Object.getOwnPropertyDescriptor(object, "name");
        if (descriptor) {
          return typeof descriptor.value === "string" && ERROR_TYPES.has(descriptor.value)
            ? descriptor.value
            : "Unknown";
        }
      }
    } catch {
      return "Unknown";
    }
    return "Unknown";
  }

  function stackOf(value) {
    if (!(value instanceof Error)) return "";
    try {
      for (let object = value; object; object = Object.getPrototypeOf(object)) {
        const descriptor = Object.getOwnPropertyDescriptor(object, "stack");
        if (!descriptor) continue;
        if (typeof descriptor.value === "string") return descriptor.value;
        if (!NATIVE_STACK_GETTER || descriptor.get !== NATIVE_STACK_GETTER) return "";
        const prepare = Object.getOwnPropertyDescriptor(Error, "prepareStackTrace");
        if (prepare && (!("value" in prepare) || prepare.value !== undefined)) return "";
        for (const key of ["name", "message"]) {
          for (let owner = value; owner; owner = Object.getPrototypeOf(owner)) {
            const property = Object.getOwnPropertyDescriptor(owner, key);
            if (!property) continue;
            if (!("value" in property) || typeof property.value !== "string") return "";
            break;
          }
        }
        const stack = NATIVE_STACK_GETTER.call(value);
        return typeof stack === "string" ? stack : "";
      }
    } catch {}
    return "";
  }

  function callsiteStack() {
    try {
      return stackOf(new Error());
    } catch {
      return "";
    }
  }

  function pageOf() {
    let path;
    try {
      path = location.pathname;
    } catch {
      return "other";
    }
    if (prefix !== "/" && path.startsWith(prefix)) {
      path = path.slice(prefix.length - 1);
    }
    if (!path.startsWith("/")) path = `/${path}`;
    path = path.replace(/\/+$/, "") || "/";
    if (path === "/") return "home";
    if (path === "/looks") return "looks";
    if (path.startsWith("/looks/")) return "look";
    if (path === "/compare") return "compare";
    if (path === "/cameras") return "cameras";
    if (path.startsWith("/cameras/")) return "camera";
    if (path === "/match" || path.startsWith("/match/")) return "match";
    if (path === "/contribute" || path.startsWith("/contribute/")) {
      return "contribute";
    }
    if (path === "/me") return "me";
    if (path.startsWith("/status/")) return "status";
    if (path === "/review") return "moderation";
    if (
      path === "/community" ||
      path.startsWith("/community/") ||
      path.startsWith("/proposals/")
    ) {
      return "community";
    }
    if (path === "/about" || path.startsWith("/about/")) return "about";
    return "other";
  }

  function send(kind, errorType, frames) {
    if (!enabled || sent >= REPORT_LIMIT) return;
    if (!KINDS.has(kind)) return;
    const type = ERROR_TYPES.has(errorType) ? errorType : "Unknown";
    const page = pageOf();
    if (!PAGES.has(page)) return;
    const key = `${kind}|${type}|${page}|${frames
      .map((frame) => `${frame.asset}:${frame.line}:${frame.column}`)
      .join(",")}`;
    if (seen.has(key)) return;
    seen.add(key);
    if (seen.size > DEDUP_LIMIT) seen.delete(seen.values().next().value);
    sent += 1;
    const report = {
      version: VERSION,
      kind,
      error_type: type,
      page,
      release,
      frames,
    };
    try {
      fetch(endpoint, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(report),
        credentials: "omit",
        referrerPolicy: "no-referrer",
        cache: "no-store",
        keepalive: true,
      }).catch(() => {});
    } catch {}
  }

  function onError(event) {
    try {
      if (typeof ErrorEvent === "function" && event instanceof ErrorEvent) {
        const error = event.error;
        let frames = framesFromStack(stackOf(error), DEFAULT_WASM);
        if (frames.length === 0) {
          const asset = assetOf(event.filename);
          const line = toU32(event.lineno);
          const column = toU32(event.colno);
          if (asset && line !== null && column !== null) {
            frames = [{ asset, line, column }];
          }
        }
        send("exception", errorTypeOf(error), frames);
        return;
      }
      const target = event.target;
      if (!target || target === window) return;
      const asset = assetOf(target.currentSrc || target.src || target.href || "");
      if (!asset) return;
      send("resource_error", "Unknown", [{ asset, line: 0, column: 0 }]);
    } catch {}
  }

  function onRejection(event) {
    try {
      const reason = event.reason;
      send(
        "unhandled_rejection",
        errorTypeOf(reason),
        framesFromStack(stackOf(reason), DEFAULT_WASM),
      );
    } catch {}
  }

  function receiveWorkerError(message) {
    try {
      if (!message || typeof message !== "object") return;
      if (!WORKER_KINDS.has(message.kind)) return;
      const type =
        typeof message.errorType === "string" && ERROR_TYPES.has(message.errorType)
          ? message.errorType
          : "Unknown";
      const wasmAsset = assetOf(message.wasmAsset);
      let frames = framesFromStack(
        typeof message.stack === "string" ? message.stack : "",
        wasmAsset,
      );
      if (frames.length === 0) {
        const asset = assetOf(message.asset);
        const line = toU32(message.line);
        const column = toU32(message.column);
        if (asset && line !== null && column !== null) {
          frames = [{ asset, line, column }];
        }
      }
      send(message.kind, type, frames);
    } catch {}
  }

  window.addEventListener("error", onError, true);
  window.addEventListener("unhandledrejection", onRejection);
  window.__looklabWorkerClientError = receiveWorkerError;

  const originalConsoleError = console.error;
  if (typeof originalConsoleError === "function") {
    console.error = function (...args) {
      try {
        let error = null;
        for (const argument of args) {
          if (argument instanceof Error) {
            error = argument;
            break;
          }
        }
        let frames = error ? framesFromStack(stackOf(error), DEFAULT_WASM) : [];
        if (frames.length === 0) {
          frames = framesFromStack(callsiteStack(), DEFAULT_WASM);
        }
        send("console_error", error ? errorTypeOf(error) : "Unknown", frames);
      } catch {}
      return originalConsoleError.apply(this, args);
    };
  }
})();
