// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser check for the first-party client-error channel. Serves the real site/errors.js and
// site/workers/prep-worker.js.
//
//   nix develop -c env PLAYWRIGHT_MODULE=$(nix eval --raw nixpkgs#playwright-driver)/index.mjs \
//     PLAYWRIGHT_BROWSERS_PATH=$(nix eval --raw nixpkgs#playwright-driver.browsers) \
//     node site/checks/client-errors.mjs --browser chromium

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../..', import.meta.url));
const USAGE = `usage: node site/checks/client-errors.mjs [options]

  --browser chromium|firefox   engine to drive (default chromium)
  --out DIR                    artifact directory (default /tmp/looklab-client-errors)`;

const options = {
  browser: 'chromium',
  out: '/tmp/looklab-client-errors',
};

for (let index = 2; index < process.argv.length; index += 1) {
  const argument = process.argv[index];
  if (argument === '--help' || argument === '-h') {
    console.log(USAGE);
    process.exit(0);
  }
  const key = argument.replace(/^--/, '');
  if (!(key in options)) throw new Error(`unknown option ${argument}\n\n${USAGE}`);
  index += 1;
  options[key] = process.argv[index];
}

const playwrightModule = process.env.PLAYWRIGHT_MODULE;
const playwright = playwrightModule
  ? await import(pathToFileURL(playwrightModule).href)
  : await import('playwright-core');
const { chromium, firefox } = playwright.default ?? playwright;
const engine = options.browser === 'firefox' ? firefox : chromium;

const RELEASE = '0123456789abcdef'.repeat(4);
const KINDS = new Set([
  'console_error',
  'exception',
  'unhandled_rejection',
  'resource_error',
]);
const ERROR_TYPES = new Set([
  'Error',
  'TypeError',
  'RangeError',
  'ReferenceError',
  'SyntaxError',
  'URIError',
  'EvalError',
  'RuntimeError',
  'CompileError',
  'LinkError',
  'Unknown',
]);
const PAGES = new Set([
  'home',
  'looks',
  'look',
  'compare',
  'cameras',
  'camera',
  'match',
  'contribute',
  'me',
  'status',
  'community',
  'moderation',
  'about',
  'other',
]);
const ALLOWED_ASSETS = [
  /^pkg\/[A-Za-z0-9_-][A-Za-z0-9_.-]*\.js$/,
  /^pkg\/[A-Za-z0-9_-][A-Za-z0-9_.-]*\.wasm$/,
  /^workers\/(?:prep|match|scene)-worker\.js$/,
  /^errors\.js$/,
  /^generated\/wasm\/looklab_prep_module\.wasm$/,
  /^generated\/wasm\/looklab_scene_module\.wasm$/,
  /^generated\/wasm\/looklab_match\.wasm$/,
];

const errorsSource = await readFile(`${REPO_ROOT}/site/errors.js`, 'utf8');
const workerSource = await readFile(`${REPO_ROOT}/site/workers/prep-worker.js`, 'utf8');
const indexSource = await readFile(`${REPO_ROOT}/site/index.html`, 'utf8');

const EMPTY_WASM = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
const MEMORY_WASM = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x05, 0x03, 0x01, 0x00, 0x01,
  0x07, 0x0a, 0x01, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00,
]);
const TRAP_WASM = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
  0x03, 0x02, 0x01, 0x00,
  0x07, 0x08, 0x01, 0x04, 0x74, 0x72, 0x61, 0x70, 0x00, 0x00,
  0x0a, 0x05, 0x01, 0x03, 0x00, 0x00, 0x0b,
]);

const BOOT_MODULE = `console.error("SECRET_BOOT_CONSOLE");
export default function init() {
  throw new TypeError("SECRET_BOOT_THROW");
}`;

const failures = [];
const evidence = [];
const servers = [];
const contexts = [];

function check(condition, message) {
  if (condition) {
    console.log(`ok ${message}`);
  } else {
    failures.push(message);
    console.log(`FAIL ${message}`);
  }
}

function probeSource(prefix) {
  const cap = Array.from(
    { length: 30 },
    (_, index) => `export function cap${index}() { console.error("cap${index}"); }`,
  ).join('\n');
  return `const prefix = ${JSON.stringify(prefix)};
export function consoleError() { console.error("SECRET_CONSOLE", new Error("SECRET_MESSAGE")); }
export function headerError() { console.error(new Error("pkg/private.js:123:45")); }
export function getterError() {
  let reads = 0;
  const error = new Error("SECRET_GETTER");
  Object.defineProperty(error, "stack", { get() { reads += 1; return "SECRET_STACK"; } });
  console.error(error);
  return reads;
}
export function uncaught() { setTimeout(() => { throw new TypeError("SECRET_THROW"); }, 0); }
export function rejection() { Promise.reject(new RangeError("SECRET_REJECT")); }
export function duplicate() { for (let index = 0; index < 5; index += 1) console.error("dup-secret"); }
export function resourceAllowed() {
  const script = document.createElement("script");
  script.src = prefix + "pkg/missing.js";
  document.head.append(script);
}
export function resourcePrivate() {
  const image = document.createElement("img");
  image.src = "/private/photo.jpg";
  document.head.append(image);
}
export async function wasmTrap() {
  const response = await fetch(prefix + "pkg/looklab-site.wasm");
  const { instance } = await WebAssembly.instantiate(await response.arrayBuffer(), {});
  setTimeout(() => instance.exports.trap(), 0);
}
export async function workerError() {
  const worker = new Worker(prefix + "workers/prep-worker.js", { type: "module" });
  try {
    const ready = new Promise(resolve => {
      worker.addEventListener("message", event => {
        if (event.data && event.data.type === "client-error") {
          window.__looklabWorkerClientError(event.data);
        }
        if (event.data && event.data.type === "ready") resolve();
      });
    });
    worker.postMessage({
      type: "init",
      url: prefix + "generated/wasm/looklab_prep_module.wasm",
      diagnostics: "1",
    });
    await Promise.race([ready, new Promise(resolve => setTimeout(resolve, 5000))]);
    worker.postMessage({ type: "call", id: 1, fn: 1, bytes: new Uint8Array() });
    await new Promise(resolve => setTimeout(resolve, 500));
  } finally {
    worker.terminate();
  }
}
${cap}
`;
}

function probePage({ prefix, release, apiBase, preview }) {
  return `<!doctype html>
<meta charset="utf-8">
<script src="${prefix}errors.js" data-release="${release}" data-api-base="${apiBase}" data-preview="${preview}"></script>
<script type="module">
  import * as probe from "${prefix}pkg/client-errors-probe.js";
  window.probe = probe;
</script>`;
}

function bootPage({ prefix, release, apiBase, preview }) {
  return indexSource
    .replaceAll('__PREFIX__', () => prefix)
    .replaceAll('__RELEASE__', () => release)
    .replaceAll('__API_BASE__', () => apiBase)
    .replaceAll('__PREVIEW__', () => preview);
}

function probeRoutes(prefix, pagePath, overrides = {}) {
  const release = overrides.release ?? RELEASE;
  const apiBase = overrides.apiBase ?? '';
  const preview = overrides.preview ?? '';
  return new Map([
    [pagePath, { type: 'text/html', body: probePage({ prefix, release, apiBase, preview }) }],
    [`${prefix}errors.js`, { type: 'text/javascript', body: errorsSource }],
    [
      `${prefix}pkg/client-errors-probe.js`,
      { type: 'text/javascript', body: probeSource(prefix) },
    ],
    [`${prefix}pkg/looklab-site.wasm`, { type: 'application/wasm', body: TRAP_WASM }],
    [`${prefix}workers/prep-worker.js`, { type: 'text/javascript', body: workerSource }],
    [
      `${prefix}generated/wasm/looklab_prep_module.wasm`,
      { type: 'application/wasm', body: MEMORY_WASM },
    ],
  ]);
}

async function startServer({ routes, api, status = 204 }) {
  const requests = [];
  const server = createServer((request, response) => {
    const url = new URL(request.url, `http://${request.headers.host || '127.0.0.1'}`);
    if (url.pathname === '/api/client-errors') {
      if (request.method === 'OPTIONS') {
        response.writeHead(204, {
          'access-control-allow-origin': '*',
          'access-control-allow-methods': 'POST',
          'access-control-allow-headers': 'content-type',
          'access-control-max-age': '600',
        });
        response.end();
        return;
      }
      let body = '';
      request.setEncoding('utf8');
      request.on('data', (chunk) => {
        body += chunk;
      });
      request.on('end', () => {
        requests.push({
          method: request.method,
          path: url.pathname,
          headers: request.headers,
          body,
        });
        if (status === 'destroy') {
          request.socket.destroy();
          return;
        }
        response.writeHead(api ? status : 404, { 'access-control-allow-origin': '*' });
        response.end();
      });
      return;
    }
    const route =
      routes.get(url.pathname) ||
      (url.pathname.endsWith('.css')
        ? { type: 'text/css', body: '' }
        : url.pathname.endsWith('favicon.ico')
          ? { type: 'image/x-icon', body: new Uint8Array() }
          : null);
    if (!route) {
      response.writeHead(404, { 'content-type': 'text/plain' });
      response.end('not found');
      return;
    }
    response.writeHead(200, { 'content-type': route.type });
    response.end(route.body);
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address();
  const handle = {
    origin: `http://127.0.0.1:${port}`,
    requests,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(resolve);
      }),
  };
  servers.push(handle);
  return handle;
}

async function newPage(browser, url) {
  const context = await browser.newContext();
  contexts.push(context);
  const page = await context.newPage();
  const problems = [];
  const messages = [];
  page.on('pageerror', (error) => problems.push(String(error.message ?? error)));
  page.on('console', (message) =>
    messages.push({ type: message.type(), text: message.text() }),
  );
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  return { context, page, problems, messages };
}

async function waitFor(predicate, timeout = 5000) {
  const start = Date.now();
  while (!predicate()) {
    if (Date.now() - start > timeout) return false;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  return true;
}

async function settle(milliseconds = 500) {
  await new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function reportsOf(server) {
  return server.requests
    .map((request) => {
      try {
        return JSON.parse(request.body);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

function validateReport(request, expected = {}) {
  const label = `${request.method} ${request.path}`;
  check(request.method === 'POST', `${label} uses POST`);
  check(request.path === '/api/client-errors', `${label} targets /api/client-errors`);
  check(
    /^application\/json\b/i.test(request.headers['content-type'] || ''),
    `${label} sends JSON`,
  );
  check(!request.headers.cookie, `${label} sends no cookie`);
  check(!request.headers.authorization, `${label} sends no authorization`);
  check(!request.headers.referer, `${label} sends no referrer`);
  let report;
  try {
    report = JSON.parse(request.body);
  } catch {
    check(false, `${label} body is JSON`);
    return null;
  }
  check(
    Object.keys(report).sort().join(',') === 'error_type,frames,kind,page,release,version',
    `${label} has only contract fields`,
  );
  check(report.version === 1, `${label} version is 1`);
  check(KINDS.has(report.kind), `${label} kind ${report.kind} is allowed`);
  check(
    ERROR_TYPES.has(report.error_type),
    `${label} error_type ${report.error_type} is allowed`,
  );
  check(PAGES.has(report.page), `${label} page ${report.page} is allowed`);
  check(/^[0-9a-f]{64}$/.test(report.release), `${label} release is 64 lowercase hex`);
  if (expected.release !== undefined) {
    check(report.release === expected.release, `${label} release matches the build`);
  }
  if (expected.page !== undefined) {
    check(report.page === expected.page, `${label} page is ${expected.page}`);
  }
  check(
    Array.isArray(report.frames) && report.frames.length <= 12,
    `${label} has at most 12 frames`,
  );
  if (Array.isArray(report.frames)) {
    for (const frame of report.frames) {
      check(
        Object.keys(frame).sort().join(',') === 'asset,column,line',
        `${label} frame has only contract fields`,
      );
      check(
        ALLOWED_ASSETS.some((pattern) => pattern.test(frame.asset)),
        `${label} frame asset ${frame.asset} is allowed`,
      );
      check(
        Number.isInteger(frame.line) && frame.line >= 0 && frame.line <= 0xffffffff,
        `${label} frame line is u32`,
      );
      check(
        Number.isInteger(frame.column) && frame.column >= 0 && frame.column <= 0xffffffff,
        `${label} frame column is u32`,
      );
    }
  }
  check(
    !/SECRET|ll_alloc|usable module|pkg\/private\.js|private\/photo|localhost|127\.0\.0\.1|https?:\/\//.test(
      request.body,
    ),
    `${label} carries no message, URL or private path`,
  );
  return report;
}

function record(scenario, server) {
  evidence.push({
    scenario,
    requests: server.requests.map((request) => ({
      method: request.method,
      path: request.path,
      headers: request.headers,
      body: request.body,
    })),
  });
}

async function bootScenario(browser, prefix, pagePath) {
  const routes = new Map([
    [pagePath, { type: 'text/html', body: bootPage({ prefix, release: RELEASE, apiBase: '', preview: '' }) }],
    [
      `${prefix}index.html`,
      { type: 'text/html', body: bootPage({ prefix, release: RELEASE, apiBase: '', preview: '' }) },
    ],
    [`${prefix}errors.js`, { type: 'text/javascript', body: errorsSource }],
    [`${prefix}pkg/looklab-site.js`, { type: 'text/javascript', body: BOOT_MODULE }],
    [`${prefix}pkg/looklab-site.wasm`, { type: 'application/wasm', body: EMPTY_WASM }],
  ]);
  const server = await startServer({ routes, api: true });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await waitFor(() => server.requests.length >= 2, 8000);
  await settle(300);
  for (const request of server.requests) {
    validateReport(request, { release: RELEASE, page: 'home' });
  }
  const reports = reportsOf(server);
  check(
    reports.some(
      (report) =>
        report.kind === 'console_error' &&
        report.frames.some((frame) => frame.asset === 'pkg/looklab-site.js'),
    ),
    `${prefix} captures console.error before WASM boot`,
  );
  check(
    reports.some(
      (report) =>
        (report.kind === 'exception' || report.kind === 'unhandled_rejection') &&
        report.error_type === 'TypeError' &&
        report.frames.some((frame) => frame.asset === 'pkg/looklab-site.js'),
    ),
    `${prefix} captures a boot exception`,
  );
  check(
    session.problems.some((problem) => problem.includes('SECRET_BOOT_THROW')) ||
      session.messages.some((message) => message.text.includes('SECRET_BOOT_THROW')),
    `${prefix} keeps the boot exception visible`,
  );
  record(`boot ${prefix}`, server);
}

async function probeSameOrigin(browser, prefix, pagePath, expectedPage) {
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath),
    api: true,
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => window.probe.consoleError());
  await session.page.evaluate(() => window.probe.headerError());
  check(await session.page.evaluate(() => window.probe.getterError()) === 0,
    `${prefix} does not invoke a custom stack getter`);
  await session.page.evaluate(() => window.probe.uncaught());
  await session.page.evaluate(() => window.probe.rejection());
  await session.page.evaluate(() => window.probe.resourceAllowed());
  await session.page.evaluate(() => window.probe.resourcePrivate());
  await session.page.evaluate(() => window.probe.wasmTrap());
  await session.page.evaluate(() => window.probe.workerError());
  await waitFor(() => server.requests.length >= 7, 8000);
  await settle(800);
  for (const request of server.requests) {
    validateReport(request, { release: RELEASE, page: expectedPage });
  }
  const reports = reportsOf(server);
  check(
    reports.some(
      (report) =>
        report.kind === 'console_error' &&
        report.error_type === 'Error' &&
        report.frames.some((frame) => frame.asset === 'pkg/client-errors-probe.js'),
    ),
    `${prefix} reports console.error with an Error callsite`,
  );
  check(
    reports.some(
      (report) =>
        report.kind === 'exception' &&
        report.error_type === 'TypeError' &&
        report.frames.some((frame) => frame.asset === 'pkg/client-errors-probe.js'),
    ),
    `${prefix} reports an uncaught exception callsite`,
  );
  check(
    reports.some(
      (report) =>
        report.kind === 'unhandled_rejection' &&
        report.error_type === 'RangeError' &&
        report.frames.some((frame) => frame.asset === 'pkg/client-errors-probe.js'),
    ),
    `${prefix} reports an unhandled rejection callsite`,
  );
  check(
    reports.some(
      (report) =>
        report.kind === 'resource_error' &&
        report.frames.some((frame) => frame.asset === 'pkg/missing.js'),
    ),
    `${prefix} reports an allowed resource failure`,
  );
  check(
    !reports.some((report) => JSON.stringify(report).includes('private/photo')),
    `${prefix} drops a private resource failure`,
  );
  check(
    reports.some(
      (report) =>
        report.kind === 'exception' &&
        report.error_type === 'RuntimeError' &&
        report.frames.some(
          (frame) =>
            frame.asset === 'pkg/looklab-site.wasm' &&
            frame.line === 0 &&
            frame.column > 0,
        ),
    ),
    `${prefix} encodes wasm function index and byte offset`,
  );
  check(
    reports.some(
      (report) =>
        report.kind === 'console_error' &&
        report.frames.some((frame) => frame.asset === 'workers/prep-worker.js'),
    ),
    `${prefix} reports a worker caught error`,
  );
  check(
    !reports.some((report) => report.frames.some((frame) => frame.asset === 'pkg/private.js')),
    `${prefix} does not parse an error header as a frame`,
  );
  check(
    session.messages.some((message) => message.text.includes('SECRET_CONSOLE')),
    `${prefix} keeps console.error output`,
  );
  check(
    session.problems.some((problem) => problem.includes('SECRET_THROW')),
    `${prefix} keeps uncaught exceptions visible`,
  );
  check(
    session.problems.some((problem) => problem.includes('SECRET_REJECT')) ||
      session.messages.some((message) => message.text.includes('SECRET_REJECT')),
    `${prefix} keeps unhandled rejections visible`,
  );
  record(`probe ${prefix}`, server);
}

async function dedupScenario(browser) {
  const prefix = '/';
  const pagePath = '/dedup.html';
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath),
    api: true,
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => window.probe.duplicate());
  await settle(800);
  check(server.requests.length === 1, 'repeated identical failures are deduplicated');
  for (const request of server.requests) validateReport(request, { release: RELEASE });
  record('dedup', server);
}

async function capScenario(browser) {
  const prefix = '/';
  const pagePath = '/cap.html';
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath),
    api: true,
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => {
    for (let index = 0; index < 30; index += 1) window.probe[`cap${index}`]();
  });
  await waitFor(() => server.requests.length >= 20, 8000);
  await settle(800);
  check(server.requests.length === 20, 'at most 20 reports are sent per page');
  for (const request of server.requests) validateReport(request, { release: RELEASE });
  record('cap', server);
}

async function previewScenario(browser) {
  const prefix = '/';
  const pagePath = '/preview.html';
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath, { preview: '1' }),
    api: true,
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => {
    window.probe.consoleError();
    window.probe.uncaught();
    window.probe.rejection();
  });
  await settle(800);
  check(server.requests.length === 0, 'preview builds send no reports');
  check(
    session.messages.some((message) => message.text.includes('SECRET_CONSOLE')),
    'preview builds keep console.error output',
  );
  check(
    session.problems.some((problem) => problem.includes('SECRET_THROW')),
    'preview builds keep uncaught exceptions visible',
  );
  record('preview', server);
}

async function invalidReleaseScenario(browser) {
  const prefix = '/';
  const pagePath = '/invalid.html';
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath, { release: '' }),
    api: true,
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => window.probe.consoleError());
  await settle(800);
  check(server.requests.length === 0, 'an invalid build id disables reporting');
  record('invalid release', server);
}

async function splitOriginScenario(browser) {
  const prefix = '/';
  const pagePath = '/split.html';
  const api = await startServer({ routes: new Map(), api: true });
  const pageServer = await startServer({
    routes: probeRoutes(prefix, pagePath, { apiBase: api.origin }),
    api: false,
  });
  const session = await newPage(browser, `${pageServer.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => window.probe.consoleError());
  await waitFor(() => api.requests.length >= 1, 8000);
  await settle(300);
  check(api.requests.length === 1, 'split-origin reports reach LOOKLAB_API_BASE');
  check(pageServer.requests.length === 0, 'split-origin pages post nothing to the static origin');
  for (const request of api.requests) {
    validateReport(request, { release: RELEASE, page: 'other' });
  }
  record('split origin', api);
}

async function transportFailureScenario(browser) {
  const prefix = '/';
  const pagePath = '/transport.html';
  const server = await startServer({
    routes: probeRoutes(prefix, pagePath),
    api: true,
    status: 'destroy',
  });
  const session = await newPage(browser, `${server.origin}${pagePath}`);
  await session.page.waitForFunction(() => Boolean(window.probe), null, { timeout: 5000 });
  await session.page.evaluate(() => window.probe.consoleError());
  await waitFor(() => server.requests.length >= 1, 5000);
  await settle(800);
  check(session.problems.length === 0, 'a rejected transport raises no page error');
  check(
    new Set(server.requests.map((request) => request.body)).size === 1,
    'a rejected transport does not report itself',
  );
  for (const request of server.requests) validateReport(request, { release: RELEASE });
  record('transport failure', server);
}

const browser = await engine.launch({ headless: true, args: ['--no-sandbox'] });
const browserVersion = browser.version();
try {
  await bootScenario(browser, '/', '/');
  await bootScenario(browser, '/looklab/', '/looklab/');
  await probeSameOrigin(browser, '/', '/', 'home');
  await probeSameOrigin(browser, '/looklab/', '/looklab/looks', 'looks');
  await dedupScenario(browser);
  await capScenario(browser);
  await previewScenario(browser);
  await invalidReleaseScenario(browser);
  await splitOriginScenario(browser);
  await transportFailureScenario(browser);
} catch (error) {
  check(false, `check run failed: ${String(error.stack ?? error)}`);
} finally {
  for (const context of contexts) await context.close();
  for (const server of servers) await server.close();
  await browser.close();
  await mkdir(options.out, { recursive: true });
  await writeFile(
    `${options.out}/report.json`,
    JSON.stringify({ browser: browserVersion, failures, evidence }, null, 2),
  );
}

if (failures.length > 0) {
  console.error(`${failures.length} failure(s)`);
  process.exitCode = 1;
} else {
  console.log('all checks passed');
}
