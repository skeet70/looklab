// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser checks: worker round trip, match/permalink, contribute upload with HAR, page screenshots.

import { copyFile, link, mkdir, readFile, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const USAGE = `usage: node site/checks/e2e.mjs [options]

  --browser chromium|firefox   engine to drive (default chromium)
  --flow all|match|contribute|pages|flatness
  --origin URL                 stub server origin (default http://localhost:8099)
  --out DIR                    artifact directory (default /tmp/looklab-e2e)
  --raw FILE --jpeg FILE       the contribution files
  --body ID --firmware V --sim KEY
  --controls "key=step,..."    control steps, comma separated
  --workers N                  worker count for the contribute run
  --count N                    files for the flatness run (default 100)
  --expected FILE              canonical permalink to compare against
  --status TOKEN               status token for the /status screenshot
  --permalink BASE64URL        permalink for the /verify screenshot

needs the built site behind the stub server and Playwright from nix:

  cd site && nix develop -c env CARGO_TARGET_DIR=target trunk build --release
  nix develop -c cargo run -p looklab-web --features stub --example stub_server -- --port 8099
  nix develop -c env \\
    PLAYWRIGHT_MODULE=$(nix eval --raw nixpkgs#playwright-driver)/index.js \\
    PLAYWRIGHT_BROWSERS_PATH=$(nix eval --raw nixpkgs#playwright-driver.browsers) \\
    node site/checks/e2e.mjs --browser chromium --flow all --out /tmp/looklab-e2e`;

const options = {
  browser: 'chromium',
  flow: 'all',
  origin: 'http://localhost:8099',
  out: '/tmp/looklab-e2e',
  raw: 'fixtures/fujifilm-x-s10.raf',
  jpeg: 'fixtures/generated/fujifilm-x-s10.jpg',
  body: 'fujifilm/x-s10',
  firmware: '1.10',
  sim: 'provia',
  controls: '',
  workers: 1,
  count: 100,
  expected: 'site/data/looks/synthetic-match/1.0.0/match.permalink',
  status: '',
  permalink: '',
};

for (let index = 2; index < process.argv.length; index += 1) {
  const argument = process.argv[index];
  if (argument === '--help' || argument === '-h') {
    console.log(USAGE);
    process.exit(0);
  }
  const key = argument.replace(/^--/, '');
  if (!(key in options)) {
    throw new Error(`unknown option ${argument}\n\n${USAGE}`);
  }
  index += 1;
  const value = process.argv[index];
  options[key] = ['workers', 'count'].includes(key) ? Number(value) : value;
}

const playwrightModule = process.env.PLAYWRIGHT_MODULE;
const playwright = playwrightModule
  ? await import(pathToFileURL(playwrightModule).href)
  : await import('playwright-core');
const { chromium, firefox } = playwright.default ?? playwright;
const engine = options.browser === 'firefox' ? firefox : chromium;

const VIEWPORTS = {
  desktop: { width: 1280, height: 960 },
  mobile: { width: 375, height: 812 },
};

await mkdir(options.out, { recursive: true });

function check(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
  console.log(`ok ${message}`);
}

function launchOptions() {
  if (options.browser !== 'firefox') {
    return {};
  }
  return {
    firefoxUserPrefs: {
      'security.webauth.webauthn_enable_softtoken': true,
      'security.webauth.webauthn_enable_usbtoken': false,
      'security.webauth.webauthn_enable_android_fido2': false,
    },
  };
}

async function addVirtualAuthenticator(page) {
  if (options.browser !== 'chromium') {
    return;
  }
  const session = await page.context().newCDPSession(page);
  await session.send('WebAuthn.enable');
  const { authenticatorId } = await session.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      ctap2Version: 'ctap2_1',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  await session.send('WebAuthn.setAutomaticPresenceSimulation', {
    authenticatorId,
    enabled: true,
  });
}

async function open(viewport, harPath) {
  const browser = await engine.launch(launchOptions());
  const context = await browser.newContext({
    viewport,
    acceptDownloads: true,
    ...(harPath
      ? {
          recordHar: {
            path: harPath,
            content: 'embed',
            urlFilter: /\/api\/(nonce|passkey\/register|upload)|\/status\/[A-Za-z0-9_-]+$/,
          },
        }
      : {}),
  });
  const page = await context.newPage();
  await page.bringToFront();
  const bodies = new Map();
  page.on('request', (request) => {
    if (!/\/api\/|\/status\//.test(request.url())) {
      return;
    }
    const data = request.postDataBuffer();
    if (data) {
      bodies.set(`${request.method()} ${request.url()}`, data.toString('base64'));
    }
  });
  const problems = [];
  page.on('pageerror', (error) => problems.push(String(error)));
  page.on('console', (message) => {
    if (message.type() === 'error') {
      problems.push(message.text());
    }
  });
  await addVirtualAuthenticator(page);
  return { browser, context, page, problems, bodies };
}

async function close(session) {
  await session.context.close();
  await session.browser.close();
}

async function match(size) {
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    await page.goto(`${options.origin}/match`);
    await page.waitForSelector('#look', { timeout: 60_000 });
    await page.click('text=Compute settings');
    await page.waitForSelector('pre.json', { state: 'attached', timeout: 600_000 });
    const permalink = (await page.locator('pre.json').first().textContent()).trim();
    await page.screenshot({ path: `${options.out}/match-${size}.png`, fullPage: true });

    await page.click('a[href^="/match/r/"]');
    await page.waitForSelector('text=Reproduced from the pinned versions', { timeout: 600_000 });
    const reproduced = (await page.locator('pre.json').first().textContent()).trim();
    await page.screenshot({ path: `${options.out}/match-permalink-${size}.png`, fullPage: true });

    check(reproduced === permalink, `${options.browser} ${size}: permalink route reproduces the result`);
    check(problems.length === 0, `${options.browser} ${size}: no page errors during match`);
    return permalink;
  } finally {
    await close(session);
  }
}

async function contribute(size) {
  const harPath = `${options.out}/session-${size}.har`;
  const session = await open(VIEWPORTS[size], harPath);
  const { page, problems } = session;
  const summary = {};
  try {
    await page.goto(`${options.origin}/contribute`);
    await page.setInputFiles('#files', [options.raw, options.jpeg]);
    await page.waitForSelector('text=pair(s) found', { timeout: 120_000 });
    await page.screenshot({ path: `${options.out}/contribute-1-drop-${size}.png`, fullPage: true });

    await page.fill('#body-id', options.body);
    await page.fill('#firmware', options.firmware);
    await page.fill('#sim', options.sim);
    if (options.controls) {
      await page.fill('#controls', options.controls.split(',').join('\n'));
    }
    await page.selectOption('#workers', String(options.workers));
    await page.screenshot({ path: `${options.out}/contribute-2-confirm-${size}.png`, fullPage: true });

    const started = Date.now();
    await page.click('button:has-text("Process locally")');
    await page.waitForSelector('text=3. Payload inspector', { timeout: 900_000 });
    summary.prep_millis = Date.now() - started;
    await page.screenshot({ path: `${options.out}/contribute-3-inspector-${size}.png`, fullPage: true });

    summary.timing = await page.evaluate(() => {
      const table = [...document.querySelectorAll('table.data-table')].find(
        (candidate) => candidate.querySelector('thead th')?.textContent === 'RAW',
      );
      return table
        ? [...table.querySelectorAll('tbody tr')].map((row) =>
            [...row.children].map((cell) => cell.textContent.trim()),
          )
        : [];
    });

    const [seedDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Export seed file")'),
    ]);
    summary.seeds = `${options.out}/seeds-${size}.json`;
    await seedDownload.saveAs(summary.seeds);
    const [payloadDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Download payload")'),
    ]);
    await payloadDownload.saveAs(`${options.out}/payload-${size}.json`);

    await page.bringToFront();
    await page.click('button:has-text("Sign and upload")');
    await page.waitForSelector('text=Status link', { timeout: 300_000 });
    await page.screenshot({ path: `${options.out}/contribute-4-upload-${size}.png`, fullPage: true });
    const link = await page.locator('a[href^="/status/"]').first().getAttribute('href');
    summary.status = link;
    check(Boolean(link), `${options.browser} ${size}: upload returned a status link`);
    check(problems.length === 0, `${options.browser} ${size}: no page errors during contribute`);

    await page.click('a[href^="/status/"]');
    await page.waitForSelector('text=My contributions', { timeout: 60_000 });
    await page.waitForSelector('table.data-table', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/status-${size}.png`, fullPage: true });
    summary.status_token = link.split('/').pop();
    return summary;
  } catch (error) {
    const state = await page
      .evaluate(() => ({
        notice: document.querySelector('.notice')?.textContent ?? null,
        error: document.querySelector('.error')?.textContent ?? null,
        focused: document.hasFocus(),
      }))
      .catch(() => ({}));
    await page.screenshot({ path: `${options.out}/contribute-failed-${size}.png`, fullPage: true }).catch(() => {});
    console.error(`${options.browser} ${size}: ${JSON.stringify(state)} ${problems.join(' | ')}`);
    throw error;
  } finally {
    await close(session);
    await patchHar(harPath, session.bodies);
  }
}

async function patchHar(path, bodies) {
  const har = JSON.parse(await readFile(path, 'utf8'));
  let patched = 0;
  for (const entry of har.log.entries) {
    const body = bodies.get(`${entry.request.method} ${entry.request.url}`);
    if (!body) {
      continue;
    }
    entry.request.postData = {
      mimeType: entry.request.postData?.mimeType ?? 'application/octet-stream',
      text: body,
      encoding: 'base64',
    };
    patched += 1;
  }
  await writeFile(path, JSON.stringify(har));
  console.log(`ok ${path}: ${patched} request bodies recorded`);
}

async function pages() {
  const seed = '32f39504de81195703a8418531a00420bec6efbd0edb4361ddc995f3e4640ee4';
  for (const [size, viewport] of Object.entries(VIEWPORTS)) {
    const session = await open(viewport);
    const { page } = session;
    try {
      await page.goto(`${options.origin}/passkeys`);
      await page.waitForSelector('h1');
      await page.screenshot({ path: `${options.out}/passkeys-${size}.png`, fullPage: true });

      await page.goto(`${options.origin}/pattern/${seed}`);
      await page.waitForSelector('canvas');
      await page.waitForTimeout(1000);
      await page.screenshot({ path: `${options.out}/pattern-${size}.png` });

      if (options.permalink) {
        await page.goto(`${options.origin}/verify?permalink=${options.permalink}`);
        await page.click('button:has-text("Load pinned versions")');
        await page.waitForSelector('text=Now drop the re-rendered RAW+JPEG pairs', { timeout: 120_000 });
        await page.screenshot({ path: `${options.out}/verify-${size}.png`, fullPage: true });
        await page.setInputFiles('input[type=file]', [options.raw, options.jpeg]);
        for (let attempt = 0; attempt < 6; attempt += 1) {
          await page.waitForTimeout(2000);
          await page.click('button:has-text("Run one iteration")');
          try {
            await page.waitForSelector('h2:text("Result")', { timeout: 20_000 });
            break;
          } catch {
            const failure = await page.evaluate(
              () => document.querySelector('.error')?.textContent ?? '',
            );
            if (!failure.includes('drop the re-rendered files')) {
              throw new Error(`verify: ${failure}`);
            }
          }
        }
        await page.waitForSelector('h2:text("Result")', { timeout: 900_000 });
        await page.screenshot({ path: `${options.out}/verify-result-${size}.png`, fullPage: true });
      } else {
        await page.goto(`${options.origin}/verify`);
        await page.waitForSelector('h1');
        await page.screenshot({ path: `${options.out}/verify-${size}.png`, fullPage: true });
      }

      if (options.status) {
        await page.goto(`${options.origin}/status/${options.status}`);
        await page.screenshot({ path: `${options.out}/status-direct-${size}.png`, fullPage: true });
      }
      console.log(`ok ${options.browser} ${size}: pages captured`);
    } finally {
      await close(session);
    }
  }
}

async function flatness() {
  const directory = `${options.out}/many`;
  await mkdir(directory, { recursive: true });
  const files = [];
  for (let index = 0; index < options.count; index += 1) {
    const stem = `fujifilm-x-s10-${String(index).padStart(3, '0')}`;
    const raw = `${directory}/${stem}.raf`;
    const jpeg = `${directory}/${stem}.jpg`;
    await linkOrCopy(options.raw, raw);
    await linkOrCopy(options.jpeg, jpeg);
    files.push(raw, jpeg);
  }

  const session = await open(VIEWPORTS.desktop);
  const { page, problems } = session;
  try {
    await page.goto(`${options.origin}/contribute`);
    await page.setInputFiles('#files', files);
    await page.waitForSelector('text=pair(s) found', { timeout: 600_000 });
    await page.fill('#body-id', options.body);
    await page.fill('#firmware', options.firmware);
    await page.fill('#sim', options.sim);
    if (options.controls) {
      await page.fill('#controls', options.controls.split(',').join('\n'));
    }
    await page.selectOption('#workers', String(options.workers));
    await page.click('text=Process locally', { timeout: 600_000 });
    await page.waitForSelector('text=3. Payload inspector', { timeout: 3_600_000 });
    await page.screenshot({ path: `${options.out}/flatness-${options.count}.png`, fullPage: true });

    const rows = await page.evaluate(() => {
      const table = [...document.querySelectorAll('table.data-table')].find(
        (candidate) => candidate.querySelector('thead th')?.textContent === 'RAW',
      );
      return table
        ? [...table.querySelectorAll('tbody tr')].map((row) =>
            [...row.children].map((cell) => cell.textContent.trim()),
          )
        : [];
    });
    const memory = rows.map((row) => Number.parseFloat(row[2]));
    const peak = Math.max(...memory);
    check(rows.length === options.count, `${options.count} files processed, ${rows.length} rows reported`);
    check(peak < 768, `peak worker memory ${peak} MiB stays under the 75% budget of 768 MiB`);
    check(problems.length === 0, `${options.browser}: no page errors during the ${options.count}-file run`);
    await writeFile(
      `${options.out}/flatness-${options.count}.json`,
      JSON.stringify({ rows, peak, min: Math.min(...memory) }, null, 2),
    );
    console.log(JSON.stringify({ rows, peak, min: Math.min(...memory) }, null, 2));
    return { rows: rows.length, peak, memory };
  } finally {
    await close(session);
  }
}

async function linkOrCopy(from, to) {
  try {
    await link(from, to);
  } catch {
    await copyFile(from, to);
  }
}

const result = { browser: options.browser, out: options.out };
if (options.flow === 'all' || options.flow === 'match') {
  result.permalink = await match('desktop');
  if (options.expected) {
    const expected = (await readFile(options.expected, 'utf8')).trim();
    result.expected = expected;
    check(
      result.permalink === expected,
      `browser match equals the CLI permalink in ${options.expected}`,
    );
  }
  await match('mobile');
}
if (options.flow === 'all' || options.flow === 'contribute') {
  result.contribute = await contribute('desktop');
  result.contribute_mobile = await contribute('mobile');
}
if (options.flow === 'all' || options.flow === 'pages') {
  if (!options.permalink && result.permalink) {
    options.permalink = Buffer.from(result.permalink, 'utf8').toString('base64url');
  }
  await pages();
}
if (options.flow === 'flatness') {
  result.flatness = await flatness();
}
console.log(JSON.stringify(result, null, 2));
