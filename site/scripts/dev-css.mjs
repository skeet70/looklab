// SPDX-License-Identifier: AGPL-3.0-or-later
// Dev-only stylesheet publisher and reload injector; the Rust watcher excludes styles.
import { randomUUID } from "node:crypto";
import { mkdir, readFile, readdir, rename, stat, writeFile } from "node:fs/promises";
import { watch } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptRoot = fileURLToPath(new URL("../..", import.meta.url));

const CSS_STAMP = "dev-css-stamp.txt";
const BUILD_STAMP = "dev-build-stamp.txt";
const CSS_PREFIX = "looklab-css ";
const MARKER = "data-looklab-dev-generation";

// The dist keeps every stylesheet at its root, so the publisher walks both source levels.
const sources = async (origin) => {
  const files = [join(origin, "site", "styles.css")];
  for (const name of (await readdir(join(origin, "site", "styles"))).sort()) {
    if (name.endsWith(".css")) files.push(join(origin, "site", "styles", name));
  }
  return files;
};

const stampPath = (dist) => join(dist, CSS_STAMP);

const put = async (file, body) => {
  const tmp = `${file}.tmp`;
  await writeFile(tmp, body);
  await rename(tmp, file);
};

const bump = async (dist) => {
  await put(stampPath(dist), `${CSS_PREFIX}${randomUUID()}\n`);
};

const publish = async (dist, origin) => {
  await mkdir(dist, { recursive: true });
  let changed = false;
  for (const src of await sources(origin)) {
    let body;
    try {
      body = await readFile(src);
    } catch {
      continue;
    }
    const out = join(dist, basename(src));
    try {
      if ((await readFile(out)).equals(body)) continue;
    } catch {}
    await put(out, body);
    changed = true;
  }
  let stamped = true;
  try {
    await stat(stampPath(dist));
  } catch {
    stamped = false;
  }
  // A page primes its last seen stamp on load, so every dist carries one.
  if (changed || !stamped) await bump(dist);
  if (changed) console.log(`published stylesheets to ${dist}`);
  return changed;
};

// A site build copies the source stylesheets again, so compare contents, not the stamp.
const reconcile = async (dist, origin) => {
  try {
    await stat(join(dist, "index.html"));
  } catch {
    return;
  }
  await publish(dist, origin);
};

const runWatch = (dist, origin) => {
  let timer = null;
  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      enqueue(() => publish(dist, origin)).catch((err) =>
        console.error(`style publish failed: ${err.message}`)
      );
    }, 80);
  };
  watch(join(origin, "site"), (_event, name) => {
    if (name === "styles.css") schedule();
  }).on("error", (err) => console.error(`style watcher failed: ${err.message}`));
  watch(join(origin, "site", "styles"), (_event, name) => {
    if (name === null || `${name}`.endsWith(".css")) schedule();
  }).on("error", (err) => console.error(`style watcher failed: ${err.message}`));
  setInterval(() => {
    enqueue(() => reconcile(dist, origin)).catch((err) =>
      console.error(`style reconcile failed: ${err.message}`)
    );
  }, 1000).unref();
  enqueue(() => reconcile(dist, origin)).catch((err) =>
    console.error(`style publish failed: ${err.message}`)
  );
  process.on("SIGTERM", () => process.exit(0));
  process.on("SIGINT", () => process.exit(0));
};

let queue = Promise.resolve();
const enqueue = (task) => {
  const run = queue.then(task);
  queue = run.catch(() => {});
  return run;
};

const reloader = (generation) => `<script ${MARKER}>
(() => {
  const generation = ${JSON.stringify(generation)};
  const buildStampUrl = "/${BUILD_STAMP}";
  const cssStampUrl = "/${CSS_STAMP}";
  const cssPrefix = ${JSON.stringify(CSS_PREFIX)};
  const stylesheets = () => document.querySelectorAll('link[rel="stylesheet"][href]');
  const repaint = (version) => {
    for (const link of stylesheets()) {
      const url = new URL(link.href);
      url.searchParams.set("v", version);
      link.href = url.href;
    }
  };
  let cssSeen = null;
  let cssPrimed = false;
  const poll = async () => {
    try {
      const build = await fetch(buildStampUrl, { cache: "no-store" });
      if (build.ok) {
        const served = (await build.text()).trim();
        if (served && served !== generation) {
          location.reload();
          return;
        }
      }
    } catch {}
    try {
      const res = await fetch(cssStampUrl, { cache: "no-store" });
      if (!res.ok) return;
      const text = (await res.text()).trim();
      const stamp = text.startsWith(cssPrefix) ? text : null;
      if (!cssPrimed) {
        cssPrimed = true;
        cssSeen = stamp;
        return;
      }
      if (stamp === null || stamp === cssSeen) return;
      cssSeen = stamp;
      repaint(stamp.slice(cssPrefix.length));
    } catch {}
  };
  poll();
  setInterval(poll, 500);
})();
</script>`;

const runInject = async (file) => {
  const generation = process.env.LOOKLAB_SOURCE_GENERATION;
  if (!generation) {
    throw new Error("LOOKLAB_SOURCE_GENERATION is unset; run as a generation build step");
  }
  const html = await readFile(file, "utf8");
  if (html.includes(MARKER)) return;
  const at = html.lastIndexOf("</body>");
  if (at === -1) throw new Error(`${file} has no </body>`);
  const head = html.indexOf("</head>");
  const withMeta =
    head === -1
      ? html
      : `${html.slice(0, head)}<meta name="${MARKER}" content="${generation}" />\n${html.slice(head)}`;
  const bodyAt = withMeta.lastIndexOf("</body>");
  await writeFile(
    file,
    withMeta.slice(0, bodyAt) + `${reloader(generation)}\n` + withMeta.slice(bodyAt)
  );
};

const [mode, distArg, originArg] = process.argv.slice(2);
const originRoot = resolve(originArg ?? process.env.LOOKLAB_SOURCE_ORIGIN ?? scriptRoot);
switch (mode) {
  case "watch":
    if (!distArg) throw new Error("usage: dev-css.mjs watch <dist> [origin]");
    runWatch(resolve(distArg), originRoot);
    break;
  case "publish":
    if (!distArg) throw new Error("usage: dev-css.mjs publish <dist> [origin]");
    await publish(resolve(distArg), originRoot);
    break;
  case "inject":
    if (!distArg) throw new Error("usage: dev-css.mjs inject <index.html>");
    await runInject(resolve(distArg));
    break;
  default:
    throw new Error("usage: dev-css.mjs watch|publish <dist> [origin] | inject <index.html>");
}
